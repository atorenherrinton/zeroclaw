use crate::security::domain_matcher::DomainMatcher;
use crate::security::otp::OtpValidator;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroclaw_config::schema::EstopConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EstopLevel {
    KillAll,
    NetworkKill,
    DomainBlock(Vec<String>),
    ToolFreeze(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeSelector {
    KillAll,
    Network,
    Domains(Vec<String>),
    Tools(Vec<String>),
}

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

    fn normalize(&mut self) {
        self.blocked_domains = dedup_sort(&self.blocked_domains);
        self.frozen_tools = dedup_sort(&self.frozen_tools);
    }
}

/// Bounded policy reads never create files or repair malformed state. The
/// persisted file is the single source of truth, including for existing managers.
const MAX_STATE_BYTES: u64 = 64 * 1024;
const LOCK_WAIT: Duration = Duration::from_secs(5);
const LOCK_RETRY: Duration = Duration::from_millis(10);

/// Read the current persisted emergency-stop state without acquiring a write
/// lock, creating a directory, or modifying a corrupt file. Missing state is
/// inactive. Unsafe, unreadable, oversized, or malformed state fails closed.
/// The caller decides whether emergency-stop enforcement is enabled in config.
///
/// No diagnostic is logged here: runtime callers may poll this function, and
/// repeated unreadable-state observations must not flood the trace. Operators
/// can inspect the same fail-closed state through `EstopManager::status`.
pub fn read_current_state(config: &EstopConfig, config_dir: &Path) -> EstopState {
    read_state_or_fail_closed(&resolve_state_file_path(config_dir, &config.state_file))
}

fn read_state_or_fail_closed(path: &Path) -> EstopState {
    read_state_file(path).unwrap_or_else(|_| EstopState::fail_closed())
}

#[derive(Debug, Clone)]
pub struct EstopManager {
    config: EstopConfig,
    state_path: PathBuf,
}

impl EstopManager {
    pub fn load(config: &EstopConfig, config_dir: &Path) -> Result<Self> {
        Ok(Self {
            config: config.clone(),
            state_path: resolve_state_file_path(config_dir, &config.state_file),
        })
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub fn status(&self) -> EstopState {
        read_state_or_fail_closed(&self.state_path)
    }

    pub fn engage(&mut self, level: EstopLevel) -> Result<()> {
        self.mutate(|state| {
            match level {
                EstopLevel::KillAll => state.kill_all = true,
                EstopLevel::NetworkKill => state.network_kill = true,
                EstopLevel::DomainBlock(domains) => {
                    for domain in domains {
                        let normalized = domain.trim().to_ascii_lowercase();
                        DomainMatcher::validate_pattern(&normalized)?;
                        state.blocked_domains.push(normalized);
                    }
                }
                EstopLevel::ToolFreeze(tools) => {
                    for tool in tools {
                        state.frozen_tools.push(normalize_tool_name(&tool)?);
                    }
                }
            }
            Ok(())
        })
    }

    pub fn resume(
        &mut self,
        selector: ResumeSelector,
        otp_code: Option<&str>,
        otp_validator: Option<&OtpValidator>,
    ) -> Result<()> {
        // Authorization precedes even lock/directory creation. An unauthorized
        // resume cannot alter persistence or race another writer's state.
        self.ensure_resume_is_authorized(otp_code, otp_validator)?;
        self.mutate(|state| {
            match selector {
                ResumeSelector::KillAll => state.kill_all = false,
                ResumeSelector::Network => state.network_kill = false,
                ResumeSelector::Domains(domains) => {
                    let normalized = domains
                        .iter()
                        .map(|domain| domain.trim().to_ascii_lowercase())
                        .collect::<Vec<_>>();
                    state
                        .blocked_domains
                        .retain(|existing| !normalized.contains(existing));
                }
                ResumeSelector::Tools(tools) => {
                    let normalized = tools
                        .iter()
                        .map(|tool| normalize_tool_name(tool))
                        .collect::<Result<Vec<_>>>()?;
                    state
                        .frozen_tools
                        .retain(|existing| !normalized.contains(existing));
                }
            }
            Ok(())
        })
    }

    fn ensure_resume_is_authorized(
        &self,
        otp_code: Option<&str>,
        otp_validator: Option<&OtpValidator>,
    ) -> Result<()> {
        if !self.config.require_otp_to_resume {
            return Ok(());
        }
        let code = otp_code
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("OTP code is required to resume estop state")?;
        let validator = otp_validator
            .context("OTP validator is required to resume estop state with OTP enabled")?;
        if !validator.validate(code)? {
            anyhow::bail!("Invalid OTP code; estop resume denied");
        }
        Ok(())
    }

    fn mutate(&self, mutation: impl FnOnce(&mut EstopState) -> Result<()>) -> Result<()> {
        let lock = acquire_state_lock(&self.state_path)?;
        // Reload only after locking. A manager constructed before another
        // process engaged a layer must preserve that layer during this update.
        // Corrupt state is not silently overwritten, including by resume.
        let mut state = read_state_file(&self.state_path)
            .context("Cannot update unsafe or corrupt estop state; preserve it for inspection")?;
        mutation(&mut state)?;
        state.updated_at = Some(now_rfc3339());
        state.normalize();
        let result = persist_state(&self.state_path, &state);
        drop(lock);
        result
    }
}

fn parent_dir(path: &Path) -> &Path {
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
fn check_parent(path: &Path) -> Result<()> {
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

fn check_regular(metadata: &fs::Metadata, private: bool) -> Result<()> {
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

fn nofollow_options() -> fs::OpenOptions {
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

fn read_state_file(path: &Path) -> Result<EstopState> {
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

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

fn acquire_state_lock(path: &Path) -> Result<fs::File> {
    check_parent(path)?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent_dir(path))?;
    check_parent(path)?;
    let path = lock_path(path);
    let start = Instant::now();
    loop {
        let mut options = nofollow_options();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // The Windows sharing contract serializes open handles across
            // processes; the file remains present after the handle closes.
            options.share_mode(0);
        }
        let file = match options.open(&path) {
            Ok(file) => file,
            #[cfg(windows)]
            Err(error) if error.raw_os_error() == Some(32) => {
                anyhow::ensure!(
                    start.elapsed() < LOCK_WAIT,
                    "Timed out acquiring estop state lock"
                );
                std::thread::sleep(LOCK_RETRY);
                continue;
            }
            Err(error) => return Err(error).context("Cannot open estop state lock"),
        };
        let metadata = file.metadata()?;
        check_regular(&metadata, true)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::MetadataExt;
            // SAFETY: file owns this valid descriptor throughout the call.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::Interrupted
                {
                    anyhow::ensure!(
                        start.elapsed() < LOCK_WAIT,
                        "Timed out acquiring estop state lock"
                    );
                    drop(file);
                    std::thread::sleep(LOCK_RETRY);
                    continue;
                }
                return Err(error).context("Cannot lock estop state");
            }
            let current = fs::symlink_metadata(&path)?;
            check_regular(&current, true)?;
            anyhow::ensure!(
                current.dev() == metadata.dev() && current.ino() == metadata.ino(),
                "Estop state lock was replaced during acquisition"
            );
        }
        #[cfg(not(any(unix, windows)))]
        anyhow::bail!("Estop state locking is unsupported on this platform");
        return Ok(file);
    }
}

fn persist_state(path: &Path, state: &EstopState) -> Result<()> {
    persist_state_with(path, state, |file, body| {
        file.write_all(body)?;
        file.sync_all()
    })
}

fn persist_state_with(
    path: &Path,
    state: &EstopState,
    write_and_sync: impl FnOnce(&mut fs::File, &[u8]) -> std::io::Result<()>,
) -> Result<()> {
    let body = serde_json::to_vec_pretty(state).context("Failed to serialize estop state")?;
    anyhow::ensure!(
        body.len() as u64 <= MAX_STATE_BYTES,
        "Estop state exceeds size limit"
    );
    // NamedTempFile owns cleanup on every pre-publication failure, uses
    // exclusive creation, and creates Unix files with 0600 before any write.
    let mut temporary = tempfile::NamedTempFile::new_in(parent_dir(path))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    restrict_windows_temp(temporary.path())?;
    write_and_sync(temporary.as_file_mut(), &body)?;
    // Recheck the destination immediately before publication. The trusted
    // directory and persistent writer lock exclude other-user/cooperating races.
    match fs::symlink_metadata(path) {
        Ok(metadata) => check_regular(&metadata, false)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Cannot inspect estop publication path"),
    }
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("Failed to atomically publish estop state")?;
    #[cfg(unix)]
    fs::File::open(parent_dir(path))?
        .sync_all()
        .context("Estop state published but directory sync failed")?;
    Ok(())
}

#[cfg(windows)]
fn restrict_windows_temp(path: &Path) -> Result<()> {
    // Match the existing Windows secret-file policy without weakening ACLs or
    // relying on Unix permission bits. New temp files have no explicit grants;
    // remove inherited grants and give only the OS-reported owner full access
    // before writing state. Failure aborts publication and removes the temp.
    let identity = std::process::Command::new("whoami").output()?;
    anyhow::ensure!(
        identity.status.success(),
        "Cannot identify estop state owner"
    );
    let username = std::str::from_utf8(&identity.stdout)?.trim();
    anyhow::ensure!(
        !username.is_empty() && !username.contains(['\r', '\n']),
        "Invalid estop state owner"
    );
    let result = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{username}:F"))
        .output()?;
    anyhow::ensure!(result.status.success(), "Cannot restrict estop state ACL");
    Ok(())
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

fn normalize_tool_name(raw: &str) -> Result<String> {
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

fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)
        .unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH)
        .to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::SecretStore;
    use crate::security::otp::OtpValidator;
    use tempfile::tempdir;
    use zeroclaw_config::schema::OtpConfig;

    fn estop_config(path: &Path) -> EstopConfig {
        EstopConfig {
            enabled: true,
            state_file: path.display().to_string(),
            require_otp_to_resume: false,
        }
    }

    #[test]
    fn estop_levels_compose_and_resume() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let cfg = estop_config(&state_path);
        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();

        manager
            .engage(EstopLevel::DomainBlock(vec!["*.chase.com".into()]))
            .unwrap();
        manager
            .engage(EstopLevel::ToolFreeze(vec!["shell".into()]))
            .unwrap();
        manager.engage(EstopLevel::NetworkKill).unwrap();
        assert!(manager.status().network_kill);
        assert_eq!(manager.status().blocked_domains, vec!["*.chase.com"]);
        assert_eq!(manager.status().frozen_tools, vec!["shell"]);

        manager
            .resume(
                ResumeSelector::Domains(vec!["*.chase.com".into()]),
                None,
                None,
            )
            .unwrap();
        assert!(manager.status().blocked_domains.is_empty());
        assert!(manager.status().network_kill);

        manager
            .resume(ResumeSelector::Tools(vec!["shell".into()]), None, None)
            .unwrap();
        assert!(manager.status().frozen_tools.is_empty());
    }

    #[test]
    fn estop_state_survives_reload() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let cfg = estop_config(&state_path);

        {
            let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
            manager.engage(EstopLevel::KillAll).unwrap();
            manager
                .engage(EstopLevel::DomainBlock(vec!["*.paypal.com".into()]))
                .unwrap();
        }

        let reloaded = EstopManager::load(&cfg, dir.path()).unwrap();
        let state = reloaded.status();
        assert!(state.kill_all);
        assert_eq!(state.blocked_domains, vec!["*.paypal.com"]);
    }

    #[test]
    fn corrupted_state_defaults_to_fail_closed_kill_all() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        fs::write(&state_path, "{not-valid-json").unwrap();
        let cfg = estop_config(&state_path);
        let manager = EstopManager::load(&cfg, dir.path()).unwrap();
        assert!(manager.status().kill_all);
    }

    #[test]
    fn resume_requires_valid_otp_when_enabled() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let mut cfg = estop_config(&state_path);
        cfg.require_otp_to_resume = true;

        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        manager.engage(EstopLevel::KillAll).unwrap();

        let err = manager
            .resume(ResumeSelector::KillAll, None, None)
            .expect_err("resume should require OTP");
        assert!(err.to_string().contains("OTP code is required"));
    }

    #[test]
    fn resume_accepts_valid_otp_code() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let mut cfg = estop_config(&state_path);
        cfg.require_otp_to_resume = true;

        let otp_cfg = OtpConfig {
            enabled: true,
            ..OtpConfig::default()
        };
        let store = SecretStore::new(dir.path(), true);
        let (validator, _) = OtpValidator::from_config(&otp_cfg, dir.path(), &store).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let code = validator.code_for_timestamp(now);

        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        manager.engage(EstopLevel::KillAll).unwrap();
        manager
            .resume(ResumeSelector::KillAll, Some(&code), Some(&validator))
            .unwrap();
        assert!(!manager.status().kill_all);
    }

    #[test]
    fn fresh_reads_observe_engage_and_resume_without_reloading_manager() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let cfg = estop_config(&state_path);
        let reader = EstopManager::load(&cfg, dir.path()).unwrap();
        let mut writer = reader.clone();
        assert!(!reader.status().is_engaged());
        assert!(!read_current_state(&cfg, dir.path()).is_engaged());
        writer.engage(EstopLevel::KillAll).unwrap();
        assert!(reader.status().kill_all);
        assert!(read_current_state(&cfg, dir.path()).kill_all);
        writer.resume(ResumeSelector::KillAll, None, None).unwrap();
        assert!(!reader.status().is_engaged());
        assert!(!read_current_state(&cfg, dir.path()).is_engaged());
    }

    #[test]
    fn missing_state_reads_and_unauthorized_resume_do_not_create_files() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("missing/nested/estop-state.json");
        let mut cfg = estop_config(&state_path);
        cfg.require_otp_to_resume = true;
        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        assert!(!manager.status().is_engaged());
        assert!(!read_current_state(&cfg, dir.path()).is_engaged());
        assert!(manager.resume(ResumeSelector::KillAll, None, None).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn stale_managers_preserve_other_operators_stop_layers() {
        let dir = tempdir().unwrap();
        let cfg = estop_config(&dir.path().join("estop-state.json"));
        let mut first = EstopManager::load(&cfg, dir.path()).unwrap();
        let mut stale = EstopManager::load(&cfg, dir.path()).unwrap();
        first.engage(EstopLevel::KillAll).unwrap();
        stale.engage(EstopLevel::NetworkKill).unwrap();
        first
            .engage(EstopLevel::ToolFreeze(vec!["shell".into()]))
            .unwrap();
        stale.resume(ResumeSelector::KillAll, None, None).unwrap();
        let state = first.status();
        assert!(!state.kill_all);
        assert!(state.network_kill);
        assert_eq!(state.frozen_tools, ["shell"]);
    }

    #[test]
    fn corrupt_state_is_fail_closed_and_never_rewritten() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let corrupt = b"{not-valid-json";
        fs::write(&state_path, corrupt).unwrap();
        let before = fs::metadata(&state_path).unwrap().modified().unwrap();
        let cfg = estop_config(&state_path);
        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        assert!(read_current_state(&cfg, dir.path()).kill_all);
        assert!(manager.status().kill_all);
        assert!(!lock_path(&state_path).exists());
        assert!(manager.engage(EstopLevel::NetworkKill).is_err());
        assert!(manager.resume(ResumeSelector::KillAll, None, None).is_err());
        assert_eq!(fs::read(&state_path).unwrap(), corrupt);
        assert_eq!(
            fs::metadata(&state_path).unwrap().modified().unwrap(),
            before
        );
    }

    #[test]
    fn oversized_unknown_and_invalid_state_fail_closed() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let cfg = estop_config(&state_path);
        for content in [
            vec![b' '; MAX_STATE_BYTES as usize + 1],
            br#"{"kill_al":true}"#.to_vec(),
            br#"{"frozen_tools":["bad tool"]}"#.to_vec(),
            br#"{"blocked_domains":["https://example.invalid/path"]}"#.to_vec(),
        ] {
            fs::write(&state_path, &content).unwrap();
            assert!(read_current_state(&cfg, dir.path()).kill_all);
            assert_eq!(fs::read(&state_path).unwrap(), content);
        }
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_file_types_links_and_permissions_fail_closed() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let cfg = estop_config(&state_path);
        let target = dir.path().join("target.json");
        fs::write(&target, "{}").unwrap();
        symlink(&target, &state_path).unwrap();
        assert!(read_current_state(&cfg, dir.path()).kill_all);
        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        assert!(manager.engage(EstopLevel::KillAll).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"{}");
        fs::remove_file(&state_path).unwrap();
        symlink(dir.path().join("absent.json"), &state_path).unwrap();
        assert!(read_current_state(&cfg, dir.path()).kill_all);
        fs::remove_file(&state_path).unwrap();
        fs::create_dir(&state_path).unwrap();
        assert!(read_current_state(&cfg, dir.path()).kill_all);
        fs::remove_dir(&state_path).unwrap();
        let fifo_path = CString::new(state_path.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid NUL-terminated fixture path, no variadic arguments.
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        let start = Instant::now();
        assert!(read_current_state(&cfg, dir.path()).kill_all);
        assert!(start.elapsed() < Duration::from_secs(1));
        fs::remove_file(&state_path).unwrap();
        fs::hard_link(&target, &state_path).unwrap();
        assert!(read_current_state(&cfg, dir.path()).kill_all);
        fs::remove_file(&state_path).unwrap();
        fs::write(&state_path, "{}").unwrap();
        fs::set_permissions(&state_path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(read_current_state(&cfg, dir.path()).kill_all);
        fs::set_permissions(&state_path, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(read_current_state(&cfg, dir.path()).kill_all);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_directory_and_lock_are_rejected_without_modifying_targets() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let linked = dir.path().join("linked");
        symlink(&real, &linked).unwrap();
        let cfg = estop_config(&linked.join("estop-state.json"));
        assert!(read_current_state(&cfg, dir.path()).kill_all);
        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        assert!(manager.engage(EstopLevel::KillAll).is_err());
        assert_eq!(fs::read_dir(&real).unwrap().count(), 0);

        // A safe immediate parent must not hide an unsafe earlier component.
        fs::create_dir(real.join("nested")).unwrap();
        let nested_cfg = estop_config(&linked.join("nested/estop-state.json"));
        assert!(read_current_state(&nested_cfg, dir.path()).kill_all);
        let mut nested = EstopManager::load(&nested_cfg, dir.path()).unwrap();
        assert!(nested.engage(EstopLevel::KillAll).is_err());
        assert_eq!(fs::read_dir(real.join("nested")).unwrap().count(), 0);

        let state_path = real.join("estop-state.json");
        let target = real.join("sentinel");
        fs::write(&target, "untouched").unwrap();
        symlink(&target, lock_path(&state_path)).unwrap();
        let cfg = estop_config(&state_path);
        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        assert!(manager.engage(EstopLevel::KillAll).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        assert!(!state_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn temporary_state_is_private_before_write_and_failure_cleans_up() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let cfg = estop_config(&state_path);
        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        manager.engage(EstopLevel::KillAll).unwrap();
        let before = fs::read(&state_path).unwrap();
        let files_before = fs::read_dir(dir.path()).unwrap().count();
        let error = persist_state_with(&state_path, &EstopState::default(), |file, body| {
            assert_eq!(file.metadata()?.permissions().mode() & 0o777, 0o600);
            assert_eq!(file.metadata()?.len(), 0);
            file.write_all(&body[..4])?;
            Err(std::io::Error::other("injected partial write failure"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("injected partial write failure"));
        assert_eq!(fs::read(&state_path).unwrap(), before);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), files_before);
        for path in [&state_path, &lock_path(&state_path)] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn read_does_not_wait_for_writer_lock_and_failed_publish_cleans_up() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let cfg = estop_config(&state_path);
        let mut manager = EstopManager::load(&cfg, dir.path()).unwrap();
        manager.engage(EstopLevel::KillAll).unwrap();
        let _lock = acquire_state_lock(&state_path).unwrap();
        let start = Instant::now();
        assert!(read_current_state(&cfg, dir.path()).kill_all);
        assert!(start.elapsed() < Duration::from_secs(1));
        fs::remove_file(&state_path).unwrap();
        fs::create_dir(&state_path).unwrap();
        let files_before = fs::read_dir(dir.path()).unwrap().count();
        assert!(persist_state(&state_path, &EstopState::default()).is_err());
        assert!(state_path.is_dir());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), files_before);
    }

    // A test-only subprocess entry point. Each manager is loaded before the
    // shared gate opens, so an implementation with cached state loses layers.
    #[test]
    fn estop_process_writer() {
        let Some(directory) = std::env::var_os("ZEROCLAW_ESTOP_TEST_PROCESS_DIR") else {
            return;
        };
        let dir = PathBuf::from(directory);
        let tool = std::env::var("ZEROCLAW_ESTOP_TEST_PROCESS_TOOL").unwrap();
        let cfg = estop_config(&dir.join("estop-state.json"));
        let mut manager = EstopManager::load(&cfg, &dir).unwrap();
        fs::write(dir.join(format!("ready-{tool}")), "ready").unwrap();
        let start = Instant::now();
        while !dir.join("gate").exists() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "parent gate timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        manager.engage(EstopLevel::ToolFreeze(vec![tool])).unwrap();
    }

    #[test]
    fn concurrent_processes_serialize_and_preserve_every_stop_layer() {
        let dir = tempdir().unwrap();
        let state_path = dir.path().join("estop-state.json");
        let cfg = estop_config(&state_path);
        let parent_lock = acquire_state_lock(&state_path).unwrap();
        let tools = (0..6)
            .map(|i| format!("fixture_tool_{i}"))
            .collect::<Vec<_>>();
        let mut children = tools
            .iter()
            .map(|tool| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "security::estop::tests::estop_process_writer",
                        "--nocapture",
                    ])
                    .env("ZEROCLAW_ESTOP_TEST_PROCESS_DIR", dir.path())
                    .env("ZEROCLAW_ESTOP_TEST_PROCESS_TOOL", tool)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let start = Instant::now();
        while tools
            .iter()
            .any(|tool| !dir.path().join(format!("ready-{tool}")).exists())
        {
            if start.elapsed() > Duration::from_secs(10) {
                for child in &mut children {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                panic!("child managers did not become ready");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        fs::write(dir.path().join("gate"), "go").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !state_path.exists(),
            "child wrote while another process held the lock"
        );
        drop(parent_lock);
        for child in children {
            let result = child.wait_with_output().unwrap();
            assert!(
                result.status.success(),
                "child failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        assert_eq!(read_current_state(&cfg, dir.path()).frozen_tools, tools);
    }
}
