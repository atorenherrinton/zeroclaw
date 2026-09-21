//! Fresh, read-only access to the daemon's canonical emergency-stop authority.
//! This scope covers tentative scheduling, not the rest of the phone service.

use crate::{
    appointments,
    common::{self, SafeResult, check},
};
use serde::Deserialize;
use std::{path::Path, time::Duration};
use zeroclaw_config::{estop::read_current_state, schema::EstopConfig};

#[derive(Default, Deserialize)]
struct Policy {
    #[serde(default)]
    security: Security,
}

#[derive(Default, Deserialize)]
struct Security {
    #[serde(default)]
    estop: EstopConfig,
}

pub(crate) fn check_current(root: &Path) -> SafeResult<()> {
    let directory = common::native_dir(root)?;
    let policy: Policy = toml::from_str(&common::private_read(&directory.join("config.toml"))?)
        .map_err(|_| "appointment_stop_policy_invalid")?;
    if !policy.security.estop.enabled {
        return Ok(());
    }
    let state = read_current_state(&policy.security.estop, &directory);
    // The independent phone tool delegates its sole mutation to this canonical
    // MCP writer. A freeze of either operation must prevent that delegation.
    check(
        !state.blocks_execution(Some(appointments::TOOL_NAME))
            && !state.blocks_execution(Some("google_write__calendar_mutate")),
        "appointment_emergency_stop",
    )
}

pub(crate) async fn interrupted(root: &Path) -> &'static str {
    loop {
        if let Err(error) = check_current(root) {
            return error;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
