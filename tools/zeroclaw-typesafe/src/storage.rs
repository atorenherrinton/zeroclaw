//! Operator-owned policy and credentials, resolved from disk on each call.
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    enabled: bool,
    allowed_domains: Vec<String>,
}

fn private_root(root: &Path) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(root).map_err(|_| "TypeSafe settings directory is missing")?;
    // SAFETY: geteuid takes no arguments and has no failure mode.
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("TypeSafe settings directory must be owner-only (0700), owned by this user, and not a symlink".into());
    }
    Ok(())
}

fn private_file(root: &Path, name: &str, limit: u64) -> Result<Vec<u8>, String> {
    private_root(root)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(name))
        .map_err(|_| "TypeSafe private file is missing or unreadable")?;
    check_file(&file, limit)?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read TypeSafe private file")?;
    if bytes.len() as u64 > limit {
        return Err("TypeSafe private file exceeds size limit".into());
    }
    Ok(bytes)
}

fn check_file(file: &File, limit: u64) -> Result<(), String> {
    let metadata = file
        .metadata()
        .map_err(|_| "Cannot inspect TypeSafe private file")?;
    // SAFETY: geteuid takes no arguments and has no failure mode.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() > limit
    {
        return Err("TypeSafe private files must be owner-only (0600), owned by this user, regular files without hard links".into());
    }
    Ok(())
}

fn settings(root: &Path) -> Result<Settings, String> {
    serde_json::from_slice(&private_file(root, "settings.json", 4096)?)
        .map_err(|_| "Invalid TypeSafe settings".into())
}

fn validate_key(key: &str) -> Result<(), String> {
    if !(8..=4096).contains(&key.len()) || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(
            "Invalid TypeSafe API key; expected a nonempty printable token without whitespace"
                .into(),
        );
    }
    Ok(())
}

fn read_key(root: &Path) -> Result<String, String> {
    let bytes = private_file(root, "api-key", 4096)
        .map_err(|_| "TypeSafe API key is missing or unsafe; run the operator set-key command")?;
    let key = String::from_utf8(bytes).map_err(|_| "Invalid TypeSafe API key file")?;
    let key = key.trim();
    validate_key(key)?;
    Ok(key.to_owned())
}

pub fn authorized_key(root: &Path) -> Result<String, String> {
    let config = settings(root)?;
    if !config.enabled {
        return Err("TypeSafe is disabled by operator policy".into());
    }
    if !config
        .allowed_domains
        .iter()
        .any(|d| d == "api.typesafe.ai")
    {
        return Err(
            "TypeSafe outbound access requires the exact api.typesafe.ai allowlist entry".into(),
        );
    }
    read_key(root)
}

pub fn status(root: &Path) -> Value {
    match settings(root) {
        Ok(config) => {
            let allowed = config
                .allowed_domains
                .iter()
                .any(|d| d == "api.typesafe.ai");
            let key_configured = read_key(root).is_ok();
            json!({"enabled":config.enabled,"outbound_allowed":allowed,"key_configured":key_configured,
                "ready":config.enabled && allowed && key_configured,"model":"jev-latest",
                "endpoint":"https://api.typesafe.ai/v1/systemone","authorizes_external_actions":false})
        }
        Err(error) => json!({"ready":false,"error":error,"authorizes_external_actions":false}),
    }
}

/// Only reachable from the local operator CLI, never through MCP.
pub fn set_key(root: &Path, key: &str) -> Result<(), String> {
    private_root(root)?;
    validate_key(key)?;
    let mut staged =
        tempfile::NamedTempFile::new_in(root).map_err(|_| "Cannot stage TypeSafe key")?;
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| "Cannot protect TypeSafe key")?;
    staged
        .write_all(key.as_bytes())
        .map_err(|_| "Cannot write TypeSafe key")?;
    staged
        .as_file()
        .sync_all()
        .map_err(|_| "Cannot sync TypeSafe key")?;
    staged
        .persist(root.join("api-key"))
        .map_err(|_| "Cannot install TypeSafe key")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn setup() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            root.path().join("settings.json"),
            br#"{"enabled":true,"allowed_domains":["api.typesafe.ai"]}"#,
        )
        .unwrap();
        fs::set_permissions(
            root.path().join("settings.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        root
    }

    #[test]
    fn missing_secret_fails_closed_then_private_key_works_and_is_not_in_status() {
        let root = setup();
        assert!(authorized_key(root.path()).is_err());
        set_key(root.path(), "test-secret-1234").unwrap();
        assert_eq!(authorized_key(root.path()).unwrap(), "test-secret-1234");
        assert_eq!(status(root.path())["ready"], true);
        assert!(!status(root.path()).to_string().contains("test-secret"));
    }

    #[test]
    fn live_policy_changes_are_enforced_without_restart() {
        let root = setup();
        set_key(root.path(), "test-secret-1234").unwrap();
        for config in [
            r#"{"enabled":false,"allowed_domains":["api.typesafe.ai"]}"#,
            r#"{"enabled":true,"allowed_domains":["*"]}"#,
            r#"{"enabled":true,"allowed_domains":["api.typesafe.ai.evil.test"]}"#,
        ] {
            fs::write(root.path().join("settings.json"), config).unwrap();
            assert!(authorized_key(root.path()).is_err());
        }
    }

    #[test]
    fn unsafe_secret_files_and_directories_are_rejected() {
        let root = setup();
        set_key(root.path(), "test-secret-1234").unwrap();
        let path = root.path().join("api-key");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(authorized_key(root.path()).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&path, root.path().join("alias")).unwrap();
        assert!(authorized_key(root.path()).is_err());
        fs::remove_file(root.path().join("alias")).unwrap();
        fs::rename(&path, root.path().join("actual")).unwrap();
        symlink(root.path().join("actual"), &path).unwrap();
        assert!(authorized_key(root.path()).is_err());
        fs::remove_file(path).unwrap();
        set_key(root.path(), "test-secret-1234").unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(authorized_key(root.path()).is_err());
    }

    #[test]
    fn secret_validation_and_atomic_replacement() {
        let root = setup();
        for key in ["", "short", "has whitespace", "header\r\ninjection"] {
            assert!(set_key(root.path(), key).is_err());
        }
        set_key(root.path(), "test-secret-first").unwrap();
        set_key(root.path(), "test-secret-second").unwrap();
        assert_eq!(authorized_key(root.path()).unwrap(), "test-secret-second");
        assert_eq!(
            fs::metadata(root.path().join("api-key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
