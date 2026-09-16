//! Nonmutating validation primitives shared by the canonical reader and writer.
//! Lock acquisition, file creation/replacement and OTP authorization are owned by
//! runtime; no function in this module changes filesystem contents.

use super::{EstopState, MAX_STATE_BYTES};
use crate::domain_matcher::DomainMatcher;
use anyhow::{Context, Result};
use std::{
    fs,
    io::Read,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// Read a resolved path with the same fail-closed behavior as read_current_state.
pub fn read_state_or_fail_closed(path: &Path) -> EstopState {
    read_state_file(path).unwrap_or_else(|_| EstopState::fail_closed())
}

pub fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Includes junctions and other reparse points, not just symbolic links.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// The containing directory must not permit another user to replace state or
/// the persistent lock inode. Missing descendants are allowed for read-only
/// probes; mutation creates them privately after validating the existing parent.
pub fn check_parent(path: &Path) -> Result<()> {
    reject_symlink_ancestors(path)?;
    let mut candidate = parent_dir(path);
    loop {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) => {
                anyhow::ensure!(
                    !is_link(&metadata) && metadata.is_dir(),
                    "Unsafe estop state directory"
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    // SAFETY: geteuid is a parameter-free process query.
                    let owner = unsafe { libc::geteuid() };
                    anyhow::ensure!(
                        metadata.uid() == owner && metadata.mode() & 0o022 == 0,
                        "Estop state directory must be owned by the current user and not writable by others"
                    );
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                candidate = candidate
                    .parent()
                    .context("No trusted estop state directory")?;
            }
            Err(error) => return Err(error).context("Cannot inspect estop state directory"),
        }
    }
}

fn reject_symlink_ancestors(path: &Path) -> Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    for ancestor in parent_dir(&absolute).ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if is_link(&metadata) => {
                #[cfg(target_os = "macos")]
                {
                    use std::os::unix::fs::MetadataExt;
                    // These fixed root-owned macOS aliases are controlled by
                    // the OS, not application state. In particular /var is
                    // present in the platform's default temporary-directory
                    // path. All application-controlled links are rejected.
                    if metadata.uid() == 0
                        && ["/var", "/tmp", "/etc"]
                            .iter()
                            .any(|system_path| ancestor == Path::new(system_path))
                    {
                        continue;
                    }
                }
                anyhow::bail!("Symlink ancestor in estop state path");
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Cannot inspect estop state ancestor"),
        }
    }
    Ok(())
}

pub fn check_regular(metadata: &fs::Metadata, private: bool) -> Result<()> {
    anyhow::ensure!(
        !is_link(metadata) && metadata.is_file(),
        "Estop path must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid is a parameter-free process query.
        let owner = unsafe { libc::geteuid() };
        anyhow::ensure!(
            metadata.uid() == owner && metadata.nlink() == 1,
            "Estop file must be owned by the current user with one link"
        );
        let forbidden = if private { 0o077 } else { 0o022 };
        anyhow::ensure!(
            metadata.mode() & forbidden == 0,
            "Unsafe estop file permissions"
        );
    }
    #[cfg(not(unix))]
    let _ = private;
    Ok(())
}

pub fn nofollow_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    options
}

pub fn read_state_file(path: &Path) -> Result<EstopState> {
    check_parent(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => check_regular(&metadata, false)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EstopState::default());
        }
        Err(error) => return Err(error).context("Cannot inspect estop state"),
    }
    let mut file = nofollow_options()
        .read(true)
        .open(path)
        .context("Cannot open estop state")?;
    let metadata = file.metadata()?;
    check_regular(&metadata, false)?;
    anyhow::ensure!(
        metadata.len() <= MAX_STATE_BYTES,
        "Estop state exceeds size limit"
    );
    let mut raw = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut raw)?;
    anyhow::ensure!(
        raw.len() as u64 <= MAX_STATE_BYTES,
        "Estop state exceeds size limit"
    );
    let mut state: EstopState = serde_json::from_slice(&raw).context("Malformed estop state")?;
    for domain in &mut state.blocked_domains {
        *domain = domain.trim().to_ascii_lowercase();
        DomainMatcher::validate_pattern(domain)?;
    }
    state.frozen_tools = state
        .frozen_tools
        .iter()
        .map(|tool| normalize_tool_name(tool))
        .collect::<Result<Vec<_>>>()?;
    state.normalize();
    Ok(state)
}

pub fn normalize_tool_name(raw: &str) -> Result<String> {
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() {
        anyhow::bail!("Tool name must not be empty");
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        anyhow::bail!("Tool name '{raw}' contains invalid characters");
    }
    Ok(value)
}

pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)
        .unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH)
        .to_rfc3339()
}
