use super::*;
#[cfg(unix)]
use std::time::{Duration, Instant};
use std::{fs, path::Path};
use tempfile::TempDir;

fn config(path: &Path) -> EstopConfig {
    EstopConfig {
        enabled: true,
        state_file: path.display().to_string(),
        require_otp_to_resume: false,
    }
}

#[test]
fn missing_nested_state_does_not_create_state_directory_or_lock() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("missing/nested/state.json");
    assert_eq!(
        read_current_state(&config(&path), dir.path()),
        EstopState::default()
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn reads_are_fresh_normalized_and_do_not_rewrite_source() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.json");
    let cfg = config(&path);
    let bytes = br#"{"frozen_tools":[" SHELL ","shell","delegate"],"blocked_domains":[" Example.invalid ","example.invalid"],"updated_at":"fixture"}"#;
    fs::write(&path, bytes).unwrap();
    let state = read_current_state(&cfg, dir.path());
    assert_eq!(state.frozen_tools, ["delegate", "shell"]);
    assert_eq!(state.blocked_domains, ["example.invalid"]);
    assert_eq!(state.updated_at.as_deref(), Some("fixture"));
    assert_eq!(fs::read(&path).unwrap(), bytes);
    let next = dir.path().join("replacement.json");
    fs::write(&next, r#"{"kill_all":true}"#).unwrap();
    fs::rename(next, &path).unwrap();
    assert!(read_current_state(&cfg, dir.path()).kill_all);
    fs::write(&path, "{}").unwrap();
    assert_eq!(read_current_state(&cfg, dir.path()), EstopState::default());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn corrupt_unknown_oversized_and_invalid_state_fail_closed_without_writes() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.json");
    for bytes in [
        b"not json".to_vec(),
        br#"{"unknown":true}"#.to_vec(),
        vec![b' '; MAX_STATE_BYTES as usize + 1],
        br#"{"frozen_tools":["bad name"]}"#.to_vec(),
        br#"{"blocked_domains":["https://example.invalid/path"]}"#.to_vec(),
    ] {
        fs::write(&path, &bytes).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let state = read_current_state(&config(&path), dir.path());
        assert!(state.kill_all);
        assert!(state.blocks_execution(None));
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[test]
fn conservative_execution_predicate_preserves_global_and_named_boundaries() {
    assert!(!EstopState::default().blocks_execution(None));
    assert!(!EstopState::default().blocks_execution(Some("shell")));
    for state in [
        EstopState {
            kill_all: true,
            ..Default::default()
        },
        EstopState {
            network_kill: true,
            ..Default::default()
        },
        EstopState {
            blocked_domains: vec!["example.invalid".into()],
            ..Default::default()
        },
    ] {
        for tool in [
            None,
            Some("shell"),
            Some("tentatively_reschedule_appointment"),
        ] {
            assert!(state.blocks_execution(tool));
        }
    }
    let named = EstopState {
        frozen_tools: vec!["calendar_mutate".into()],
        ..Default::default()
    };
    assert!(named.blocks_execution(Some(" CALENDAR_MUTATE ")));
    assert!(!named.blocks_execution(Some("tentatively_reschedule_appointment")));
    assert!(!named.blocks_execution(Some("calendar_mutate_extra")));
    assert!(!named.blocks_execution(None));
}

#[test]
fn reader_does_not_invent_enabled_policy_or_require_resume_credentials() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.json");
    fs::write(&path, r#"{"kill_all":true}"#).unwrap();
    let mut cfg = config(&path);
    cfg.enabled = false;
    cfg.require_otp_to_resume = true;
    // Like the previous runtime reader, this reports the file even when disabled.
    // Admission callers explicitly decide whether to invoke it.
    assert!(read_current_state(&cfg, dir.path()).kill_all);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn path_resolution_preserves_relative_absolute_and_tilde_contract() {
    let dir = TempDir::new().unwrap();
    let absolute = dir.path().join("absolute.json");
    assert_eq!(
        resolve_state_file_path(dir.path(), "relative.json"),
        dir.path().join("relative.json")
    );
    assert_eq!(
        resolve_state_file_path(dir.path(), absolute.to_str().unwrap()),
        absolute
    );
    // Pure expansion only; never open or modify the user's home path.
    assert_eq!(
        resolve_state_file_path(dir.path(), "~/state.json"),
        PathBuf::from(shellexpand::tilde("~/state.json").as_ref())
    );
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn nonregular_links_and_unsafe_modes_are_rejected_without_blocking() {
    use std::{
        ffi::CString,
        os::unix::{
            ffi::OsStrExt,
            fs::{PermissionsExt, symlink},
        },
    };
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state.json");
    let target = dir.path().join("target.json");
    fs::write(&target, "{}").unwrap();
    symlink(&target, &path).unwrap();
    assert!(read_current_state(&config(&path), dir.path()).kill_all);
    fs::remove_file(&path).unwrap();
    fs::hard_link(&target, &path).unwrap();
    assert!(read_current_state(&config(&path), dir.path()).kill_all);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(read_current_state(&config(&path), dir.path()).kill_all);
    fs::remove_dir(&path).unwrap();
    let fifo = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: NUL-terminated private fixture path; no live files are involved.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let start = Instant::now();
    assert!(read_current_state(&config(&path), dir.path()).kill_all);
    assert!(start.elapsed() < Duration::from_secs(1));
    fs::remove_file(&path).unwrap();
    fs::write(&path, "{}").unwrap();
    // Existing reader permits owner-controlled read visibility, but no foreign writes.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!read_current_state(&config(&path), dir.path()).is_engaged());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(read_current_state(&config(&path), dir.path()).kill_all);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"{}");
}

#[cfg(unix)]
#[test]
fn parent_link_and_writable_parent_fail_closed_without_creating_state() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = TempDir::new().unwrap();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::create_dir(real.join("nested")).unwrap();
    let link = dir.path().join("link");
    symlink(&real, &link).unwrap();
    let linked_state = link.join("nested/state.json");
    assert!(read_current_state(&config(&linked_state), dir.path()).kill_all);
    let state = real.join("state.json");
    fs::set_permissions(&real, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(read_current_state(&config(&state), dir.path()).kill_all);
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!state.exists());
    assert_eq!(fs::read_dir(real.join("nested")).unwrap().count(), 0);
}
