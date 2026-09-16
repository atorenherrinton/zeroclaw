//! Canonical read-only emergency-stop state and admission predicate.
//!
//! The persisted file is the only source of stop state. Readers never create a
//! directory, lock, or state file, and never repair corrupt state. Callers resolve
//! current configuration and own the enabled flag; mutation and OTP stay runtime-owned.

use crate::schema::EstopConfig;
use persistence_support::{now_rfc3339, read_state_or_fail_closed};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Nonmutating filesystem validation shared with the runtime writer. This module
/// exposes the same checks so publication and readers cannot drift apart.
pub mod persistence_support;

/// Shared bound for serialized state on both read and publication.
pub const MAX_STATE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EstopState {
    #[serde(default)]
    pub kill_all: bool,
    #[serde(default)]
    pub network_kill: bool,
    #[serde(default)]
    pub blocked_domains: Vec<String>,
    #[serde(default)]
    pub frozen_tools: Vec<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl EstopState {
    pub fn fail_closed() -> Self {
        Self {
            kill_all: true,
            network_kill: false,
            blocked_domains: Vec::new(),
            frozen_tools: Vec::new(),
            updated_at: Some(now_rfc3339()),
        }
    }

    pub fn is_engaged(&self) -> bool {
        self.kill_all
            || self.network_kill
            || !self.blocked_domains.is_empty()
            || !self.frozen_tools.is_empty()
    }

    /// Whether this state denies the named execution. Without trusted per-tool
    /// destination metadata, network/domain latches conservatively deny every
    /// execution. A tool freeze applies only to its explicit canonical name.
    /// Callers remain responsible for honoring the current config enabled flag.
    pub fn blocks_execution(&self, tool: Option<&str>) -> bool {
        self.kill_all
            || self.network_kill
            || !self.blocked_domains.is_empty()
            || tool.is_some_and(|name| {
                self.frozen_tools
                    .iter()
                    .any(|frozen| frozen.eq_ignore_ascii_case(name.trim()))
            })
    }

    /// Canonical ordering shared by readers and the serialized writer.
    pub fn normalize(&mut self) {
        self.blocked_domains = dedup_sort(&self.blocked_domains);
        self.frozen_tools = dedup_sort(&self.frozen_tools);
    }
}

/// Read the current persisted emergency-stop state without acquiring a write
/// lock, creating a directory, or modifying a corrupt file. Missing state is
/// inactive. Unsafe, unreadable, oversized, or malformed state fails closed.
/// The caller decides whether emergency-stop enforcement is enabled in config.
///
/// No diagnostic is logged here: runtime callers may poll this function, and
/// repeated unreadable-state observations must not flood the trace. Operators
/// can inspect the same fail-closed state through the runtime manager status API.
pub fn read_current_state(config: &EstopConfig, config_dir: &Path) -> EstopState {
    read_state_or_fail_closed(&resolve_state_file_path(config_dir, &config.state_file))
}

pub fn resolve_state_file_path(config_dir: &Path, state_file: &str) -> PathBuf {
    let expanded = shellexpand::tilde(state_file).into_owned();
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        path
    } else {
        config_dir.join(path)
    }
}

fn dedup_sort(values: &[String]) -> Vec<String> {
    let mut deduped = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    deduped.sort_unstable();
    deduped.dedup();
    deduped
}

#[cfg(test)]
mod tests;
