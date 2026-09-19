#![cfg(target_os = "macos")]
use super::*;
use crate::{private_dir, private_write};
use std::os::unix::fs::symlink;

fn boxed(kind: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend(kind);
    out.extend(body);
    out
}
fn mp4() -> Vec<u8> {
    include_bytes!("../../tests/fixtures/black.mp4").to_vec()
}
struct Fixture {
    _temp: tempfile::TempDir,
    ops: Ops,
    home: PathBuf,
    source: PathBuf,
}
impl Fixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().canonicalize()?;
        let root = home.join("config");
        private_dir(&root)?;
        let ops = Ops::open(&root)?;
        private_dir(&home.join("Downloads"))?;
        let share = root.join("agents/main/workspace/share");
        private_dir(&share)?;
        private_write(
            &root.join("extensions/personal-ops/import.json"),
            br#"{"enabled":true,"max_bytes":49000000,"retention_hours":24}"#,
        )?;
        private_write(
            &root.join("extensions/personal-ops/sharing.json"),
            &serde_json::to_vec(&json!({"allowed_roots":[share]}))?,
        )?;
        let source = home.join("Downloads/fixture.mp4");
        private_write(&source, &mp4())?;
        Ok(Self {
            _temp: temp,
            ops,
            home,
            source,
        })
    }
    fn args(&self) -> Value {
        json!({"action":"inspect","request_id":uuid::Uuid::new_v4().to_string(),"source_path":self.source,"owner_requested":true})
    }
    fn run(&self, args: Value) -> Result<Value> {
        import_from(&self.ops, &args, &self.home, || Ok(()))?;
        let mut confirm = args;
        confirm["action"] = json!("confirm");
        import_from(&self.ops, &confirm, &self.home, || Ok(()))
    }

    fn count(&self) -> Result<i64> {
        Ok(self
            .ops
            .db
            .query_row("SELECT count(*) FROM share_imports", [], |r| r.get(0))?)
    }
}
#[test]
fn success_receipt_prepare_and_private_snapshot() -> Result<()> {
    let f = Fixture::new()?;
    let result = f.run(f.args())?;
    let path = Path::new(result["approved_path"].as_str().unwrap());
    assert_eq!(std::fs::read(path)?, mp4());
    assert_eq!(std::fs::metadata(path)?.mode() & 0o777, 0o400);
    assert_eq!(result["receipt"]["sha256"], digest(&mp4()));
    let prepared = f
        .ops
        .prepare_files(&json!({"paths":[path],"recipients":["user@example.com"]}))?;
    assert!(prepared.is_object());
    assert!(f.source.exists());
    assert_eq!(f.count()?, 1);
    Ok(())
}
#[test]
fn authorization_and_unknown_arguments_reject() -> Result<()> {
    let f = Fixture::new()?;
    let valid = f.args();
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("owner_requested");
    let mut denied = valid.clone();
    denied["owner_requested"] = json!(false);
    let mut wrong_type = valid.clone();
    wrong_type["owner_requested"] = json!("true");
    let mut extra = valid;
    extra["destination"] = json!("/tmp");
    for args in [missing, denied, wrong_type, extra] {
        assert!(f.run(args).is_err());
    }
    assert_eq!(f.count()?, 0);
    Ok(())
}
#[test]
fn config_is_closed_and_read_live() -> Result<()> {
    let f = Fixture::new()?;
    let p = f.ops.root.join("extensions/personal-ops/import.json");
    for contents in [
        br#"{"enabled":false,"max_bytes":49000000,"retention_hours":24}"#.as_slice(),
        br#"{"enabled":true,"max_bytes":49000001,"retention_hours":24}"#,
        br#"{"enabled":true,"max_bytes":10,"retention_hours":24}"#,
        br#"{"enabled":true,"max_bytes":49000000,"retention_hours":73}"#,
        b"{}",
    ] {
        std::fs::write(&p, contents)?;
        assert!(f.run(f.args()).is_err());
    }
    std::fs::remove_file(p)?;
    assert!(f.run(f.args()).is_err());
    Ok(())
}
#[test]
fn paths_direct_child_only() -> Result<()> {
    let f = Fixture::new()?;
    for path in [
        "relative.mp4".to_string(),
        format!("{}/Downloads/../Downloads/fixture.mp4", f.home.display()),
        format!("{}/Downloads/./fixture.mp4", f.home.display()),
        format!("{}/Downloads//fixture.mp4", f.home.display()),
        format!("{}/Downloads/nested/fixture.mp4", f.home.display()),
        f.home.join("elsewhere.mp4").display().to_string(),
        f.home.join("Downloads").display().to_string(),
    ] {
        let mut args = f.args();
        args["source_path"] = json!(path);
        assert!(f.run(args).is_err());
    }
    Ok(())
}
#[test]
fn symlinks_hardlinks_directories_special_files_reject() -> Result<()> {
    let f = Fixture::new()?;
    let outside = f.home.join("outside.mp4");
    std::fs::rename(&f.source, &outside)?;
    symlink(&outside, &f.source)?;
    assert!(f.run(f.args()).is_err());
    std::fs::remove_file(&f.source)?;
    std::fs::hard_link(&outside, &f.source)?;
    assert!(f.run(f.args()).is_err());
    std::fs::remove_file(&f.source)?;
    std::fs::create_dir(&f.source)?;
    assert!(f.run(f.args()).is_err());
    std::fs::remove_dir(&f.source)?;
    let name = std::ffi::CString::new(f.source.to_str().unwrap())?;
    // SAFETY: the C string remains live for the syscall.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(f.run(f.args()).is_err());
    Ok(())
}
#[test]
fn source_and_destination_symlink_ancestors_reject() -> Result<()> {
    let f = Fixture::new()?;
    let downloads = f.home.join("Downloads");
    let renamed = f.home.join("renamed");
    std::fs::rename(&downloads, &renamed)?;
    symlink(&renamed, &downloads)?;
    assert!(f.run(f.args()).is_err());
    std::fs::remove_file(&downloads)?;
    std::fs::rename(renamed, downloads)?;
    let share = f.ops.root.join("agents/main/workspace/share");
    let other = f.home.join("other");
    std::fs::rename(&share, &other)?;
    symlink(other, share)?;
    assert!(f.run(f.args()).is_err());
    Ok(())
}
#[test]
fn unsafe_permissions_and_unapproved_destination_reject() -> Result<()> {
    let f = Fixture::new()?;
    std::fs::set_permissions(&f.source, Permissions::from_mode(0o666))?;
    assert!(f.run(f.args()).is_err());
    std::fs::set_permissions(&f.source, Permissions::from_mode(0o600))?;
    let share = f.ops.root.join("agents/main/workspace/share");
    std::fs::set_permissions(&share, Permissions::from_mode(0o755))?;
    assert!(f.run(f.args()).is_err());
    std::fs::set_permissions(share, Permissions::from_mode(0o700))?;
    std::fs::write(
        f.ops.root.join("extensions/personal-ops/sharing.json"),
        b"{\"allowed_roots\":[]}",
    )?;
    assert!(f.run(f.args()).is_err());
    Ok(())
}
#[test]
fn mutation_and_replacement_during_read_reject() -> Result<()> {
    for replace in [false, true] {
        let f = Fixture::new()?;
        let result = import_from(&f.ops, &f.args(), &f.home, || {
            if replace {
                std::fs::remove_file(&f.source)?;
            }
            std::fs::write(&f.source, mp4())?;
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(f.count()?, 0);
    }
    Ok(())
}
#[test]
fn collisions_never_overwrite_or_delete() -> Result<()> {
    let f = Fixture::new()?;
    let (base, _dir) = destination(&f.ops, true)?;
    drop(_dir);
    let id = uuid::Uuid::new_v4();
    let target = base.join(filename(&id.to_string())?);
    private_write(&target, b"unrelated")?;
    let mut args = f.args();
    args["request_id"] = json!(id.to_string());
    assert!(f.run(args).is_err());
    assert_eq!(std::fs::read(target)?, b"unrelated");
    assert_eq!(f.count()?, 1);
    Ok(())
}
#[test]
fn tampering_missing_receipt_and_expiry_fail_closed() -> Result<()> {
    let f = Fixture::new()?;
    let result = f.run(f.args())?;
    let path = Path::new(result["approved_path"].as_str().unwrap());
    std::fs::set_permissions(path, Permissions::from_mode(0o600))?;
    std::fs::write(path, b"tamper")?;
    assert!(verified_bytes(&f.ops, path).is_err());
    f.ops.db.execute("DELETE FROM share_imports", [])?;
    assert!(verified_bytes(&f.ops, path).is_err());
    Ok(())
}
#[test]
fn cleanup_is_bounded_receipt_scoped_and_works_disabled() -> Result<()> {
    let f = Fixture::new()?;
    let result = f.run(f.args())?;
    let path = Path::new(result["approved_path"].as_str().unwrap());
    let other = path.parent().unwrap().join("unrelated.mp4");
    private_write(&other, b"keep")?;
    let mut receipt: Receipt = serde_json::from_value(result["receipt"].clone())?;
    receipt.expires_ms = 0;
    f.ops.db.execute(
        "UPDATE share_imports SET receipt=?1",
        [serde_json::to_string(&receipt)?],
    )?;
    assert!(verified_bytes(&f.ops, path).is_err());
    std::fs::remove_file(f.ops.root.join("extensions/personal-ops/import.json"))?;
    assert_eq!(cleanup(&f.ops, &json!({}))?["removed"], 1);
    assert!(!path.exists());
    assert!(other.exists());
    assert_eq!(f.count()?, 0);
    Ok(())
}
#[test]
fn malformed_and_wrong_formats_reject() -> Result<()> {
    assert!(recognize_mp4(&mp4()).is_ok());
    for bytes in [
        Vec::new(),
        b"not an mp4".to_vec(),
        boxed(b"ftyp", b"isom0000"),
        mp4()[..100].to_vec(),
    ] {
        assert!(recognize_mp4(&bytes).is_err());
    }
    let f = Fixture::new()?;
    std::fs::write(&f.source, b"not an mp4")?;
    assert!(f.run(f.args()).is_err());
    Ok(())
}
#[tokio::test]
async fn public_dispatch_and_schema_enforce_assertion() -> Result<()> {
    let f = Fixture::new()?;
    let mut args = f.args();
    args["owner_requested"] = json!(false);
    assert!(crate::call(&f.ops, "files_import", &args).await.is_err());
    let schema = crate::schema();
    let tool = schema
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "files_import")
        .unwrap();
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    Ok(())
}

#[test]
fn wrong_owner_metadata_is_rejected() -> Result<()> {
    let f = Fixture::new()?;
    assert!(
        regular(
            &std::fs::metadata(&f.source)?,
            false,
            MAX_BYTES,
            uid().wrapping_add(1)
        )
        .is_err()
    );
    Ok(())
}
#[test]
fn policy_symlink_and_hardlink_are_rejected() -> Result<()> {
    let f = Fixture::new()?;
    let policy = f.ops.root.join("extensions/personal-ops/import.json");
    let target = f.home.join("policy.json");
    std::fs::rename(&policy, &target)?;
    symlink(&target, &policy)?;
    assert!(f.run(f.args()).is_err());
    std::fs::remove_file(&policy)?;
    std::fs::hard_link(target, policy)?;
    assert!(f.run(f.args()).is_err());
    Ok(())
}
#[test]
fn receipt_failure_removes_snapshot_and_never_returns_success() -> Result<()> {
    let f = Fixture::new()?;
    f.ops.db.execute_batch("CREATE TRIGGER reject_import BEFORE INSERT ON share_imports BEGIN SELECT RAISE(ABORT,'fixture receipt failure'); END;")?;
    assert!(f.run(f.args()).is_err());
    assert_eq!(f.count()?, 0);
    assert_eq!(
        std::fs::read_dir(f.ops.root.join("agents/main/workspace/share/imports"))?.count(),
        0
    );
    Ok(())
}
#[test]
fn retention_capacity_and_lock_are_enforced() -> Result<()> {
    let f = Fixture::new()?;
    let result = f.run(f.args())?;
    let (base, guard) = destination(&f.ops, false)?;
    assert!(f.run(f.args()).is_err());
    drop(guard);
    let original: Receipt = serde_json::from_value(result["receipt"].clone())?;
    for _ in 1..CAPACITY {
        let mut receipt: Receipt = serde_json::from_value(result["receipt"].clone())?;
        receipt.id = uuid::Uuid::new_v4().to_string();
        f.ops.db.execute(
            "INSERT INTO share_imports VALUES(?1,?2)",
            params![receipt.id, serde_json::to_string(&receipt)?],
        )?;
    }
    assert!(f.run(f.args()).is_err());
    assert_eq!(f.count()?, CAPACITY);
    let (_, dir) = destination(&f.ops, false)?;
    assert_eq!(
        cleanup_locked(&f.ops, &dir, original.expires_ms)?,
        CAPACITY as usize
    );
    assert_eq!(std::fs::read_dir(base)?.count(), 0);
    Ok(())
}
#[test]
fn substituted_cleanup_symlink_never_follows_target() -> Result<()> {
    let f = Fixture::new()?;
    let result = f.run(f.args())?;
    let path = Path::new(result["approved_path"].as_str().unwrap());
    std::fs::remove_file(path)?;
    symlink(&f.source, path)?;
    let receipt: Receipt = serde_json::from_value(result["receipt"].clone())?;
    let (_, dir) = destination(&f.ops, false)?;
    assert!(cleanup_locked(&f.ops, &dir, receipt.expires_ms).is_err());
    assert_eq!(std::fs::read(&f.source)?, mp4());
    Ok(())
}
#[test]
fn source_parent_replaced_mid_import_rejects() -> Result<()> {
    let f = Fixture::new()?;
    let result = import_from(&f.ops, &f.args(), &f.home, || {
        std::fs::rename(f.home.join("Downloads"), f.home.join("old"))?;
        private_dir(&f.home.join("Downloads"))?;
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(f.count()?, 0);
    Ok(())
}

#[test]
fn crash_reservation_cannot_delete_colliding_unrelated_content() -> Result<()> {
    let f = Fixture::new()?;
    let result = f.run(f.args())?;
    let mut receipt: Receipt = serde_json::from_value(result["receipt"].clone())?;
    receipt.snapshot_identity = None;
    f.ops.db.execute(
        "UPDATE share_imports SET receipt=?1",
        [serde_json::to_string(&receipt)?],
    )?;
    let path = Path::new(result["approved_path"].as_str().unwrap());
    let (_, dir) = destination(&f.ops, false)?;
    assert!(cleanup_locked(&f.ops, &dir, receipt.expires_ms).is_err());
    assert!(path.exists());
    assert_eq!(f.count()?, 1);
    Ok(())
}

#[test]
fn generated_mp4_and_structural_rejections() -> Result<()> {
    let original = mp4();
    assert_eq!(
        digest(&original),
        "5046ca6b29719950202ebe3d4cf9e15a9f3fd4642d2f999203e1b70bfe802c69"
    );
    recognize_mp4(&original)?;
    // Every prefix truncates required framing or omits the nonempty mdat.
    for end in 0..original.len() {
        assert!(recognize_mp4(&original[..end]).is_err(), "prefix {end}");
    }
    for (needle, replacement) in [
        (b"stts", b"xxxx"),
        (b"stsc", b"xxxx"),
        (b"stsz", b"xxxx"),
        (b"stco", b"xxxx"),
        (b"vide", b"soun"),
        (b"avc1", b"jpeg"),
    ] {
        let mut bytes = original.clone();
        // Last avc1 occurrence is the sample entry, not the compatible brand.
        let pos = if needle == b"avc1" {
            bytes.windows(4).rposition(|w| w == needle).unwrap()
        } else {
            bytes.windows(4).position(|w| w == needle).unwrap()
        };
        bytes[pos..pos + 4].copy_from_slice(replacement);
        assert!(recognize_mp4(&bytes).is_err(), "{needle:?}");
    }
    for size in [0u32, 1, 7, u32::MAX] {
        let mut bytes = original.clone();
        bytes[..4].copy_from_slice(&size.to_be_bytes());
        assert!(recognize_mp4(&bytes).is_err());
    }
    for extra in [boxed(b"moof", &[]), boxed(b"moov", &[]), vec![0]] {
        let mut bytes = original.clone();
        bytes.extend(extra);
        assert!(recognize_mp4(&bytes).is_err());
    }
    assert!(codec_envelope(&[1, 66, 0, 30, 255, 225, 0], true).is_err());
    assert!(codec_envelope(&[1; 7], false).is_err());
    Ok(())
}

#[test]
fn receipt_update_failure_and_partial_crash_fail_closed() -> Result<()> {
    let f = Fixture::new()?;
    f.ops.db.execute_batch("CREATE TRIGGER reject_update BEFORE UPDATE ON share_imports BEGIN SELECT RAISE(ABORT,'fixture update failure'); END;")?;
    assert!(f.run(f.args()).is_err());
    assert_eq!(f.count()?, 1);
    assert_eq!(
        std::fs::read_dir(f.ops.root.join("agents/main/workspace/share/imports"))?.count(),
        0
    );
    f.ops.db.execute_batch("DROP TRIGGER reject_update")?;
    let result = f.run(f.args())?;
    let path = Path::new(result["approved_path"].as_str().unwrap());
    std::fs::set_permissions(path, Permissions::from_mode(0o600))?;
    std::fs::write(path, &mp4()[..100])?;
    assert!(
        f.ops
            .prepare_files(&json!({"paths":[path],"recipients":["user@example.com"]}))
            .is_err()
    );
    let receipt: Receipt = serde_json::from_value(result["receipt"].clone())?;
    let (_, dir) = destination(&f.ops, false)?;
    assert_eq!(cleanup_locked(&f.ops, &dir, receipt.expires_ms)?, 2);
    assert!(!path.exists());
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn extended_acl_grants_reject_and_deny_only_is_supported() -> Result<()> {
    let f = Fixture::new()?;
    for path in [&f.source, &f.home.join("Downloads")] {
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(path)
            .status()?;
        ensure!(status.success(), "fixture ACL setup failed");
        assert!(f.run(f.args()).is_err());
        ensure!(
            std::process::Command::new("/bin/chmod")
                .arg("-N")
                .arg(path)
                .status()?
                .success(),
            "fixture ACL removal failed"
        );
    }
    ensure!(
        std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone deny delete"])
            .arg(&f.source)
            .status()?
            .success(),
        "fixture ACL setup failed"
    );
    f.run(f.args())?;
    Ok(())
}

#[tokio::test]
async fn complete_import_schemas_and_dispatch_reject_unknown_arguments() -> Result<()> {
    let tools = crate::schema();
    for expected in super::schema() {
        let matches: Vec<_> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["name"] == expected["name"])
            .collect();
        assert_eq!(matches, vec![&expected]);
    }
    let import_schema = &super::schema()[0]["inputSchema"];
    assert_eq!(
        import_schema,
        &json!({"type":"object","additionalProperties":false,"required":["action","request_id","source_path","owner_requested"],"properties":{"action":{"type":"string","enum":["inspect","confirm"]},"request_id":{"type":"string","format":"uuid"},"source_path":{"type":"string","maxLength":4096},"owner_requested":{"type":"boolean","const":true}}})
    );
    assert_eq!(
        super::schema()[1]["inputSchema"],
        json!({"type":"object","additionalProperties":false,"properties":{}})
    );
    let f = Fixture::new()?;
    for args in [
        json!({}),
        json!({"source_path":f.source,"owner_requested":true,"destination":"/tmp"}),
        json!({"source_path":[f.source],"owner_requested":true}),
        json!({"source_path":f.source,"owner_requested":null}),
    ] {
        assert!(crate::call(&f.ops, "files_import", &args).await.is_err());
    }
    for args in [json!({"all":true}), json!(null), json!([])] {
        assert!(
            crate::call(&f.ops, "files_import_cleanup", &args)
                .await
                .is_err()
        );
    }
    assert_eq!(f.count()?, 0);
    Ok(())
}

#[test]
fn reviewed_state_and_replay_are_bound_to_exact_source() -> Result<()> {
    let f = Fixture::new()?;
    let mut args = f.args();
    let inspected = import_from(&f.ops, &args, &f.home, || Ok(()))?;
    assert!(inspected.get("approved_path").is_none());
    assert_eq!(inspected["receipt"]["basename"], "fixture.mp4");
    assert_eq!(inspected["size_bytes"], mp4().len());
    assert_eq!(import_from(&f.ops, &args, &f.home, || Ok(()))?, inspected);
    args["action"] = json!("confirm");
    let result = import_from(&f.ops, &args, &f.home, || Ok(()))?;
    assert_eq!(f.count()?, 1);
    // Lost success response is recoverable even if source subsequently disappears.
    std::fs::remove_file(&f.source)?;
    assert_eq!(import_from(&f.ops, &args, &f.home, || Ok(()))?, result);
    args["source_path"] = json!(f.home.join("Downloads/other.mp4"));
    assert!(import_from(&f.ops, &args, &f.home, || Ok(())).is_err());
    Ok(())
}
#[test]
fn stale_review_same_bytes_replacement_and_expiration_reject() -> Result<()> {
    for replace in [false, true] {
        let f = Fixture::new()?;
        let mut args = f.args();
        let result = import_from(&f.ops, &args, &f.home, || Ok(()))?;
        args["action"] = json!("confirm");
        if replace {
            std::fs::remove_file(&f.source)?;
            private_write(&f.source, &mp4())?;
        } else {
            let mut receipt: Receipt = serde_json::from_value(result["receipt"].clone())?;
            receipt.review_expires_ms = 0;
            save_receipt(&f.ops, &receipt)?;
        }
        assert!(import_from(&f.ops, &args, &f.home, || Ok(())).is_err());
        assert_eq!(
            std::fs::read_dir(f.ops.root.join("agents/main/workspace/share/imports"))?.count(),
            0
        );
    }
    Ok(())
}
#[test]
fn pending_never_replays_and_prepare_requires_complete() -> Result<()> {
    let f = Fixture::new()?;
    let mut args = f.args();
    let result = f.run(args.clone())?;
    let mut receipt: Receipt = serde_json::from_value(result["receipt"].clone())?;
    receipt.state = State::Pending;
    save_receipt(&f.ops, &receipt)?;
    args["action"] = json!("confirm");
    assert!(
        import_from(&f.ops, &args, &f.home, || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("uncertain")
    );
    assert!(verified_bytes(&f.ops, Path::new(result["approved_path"].as_str().unwrap())).is_err());
    assert_eq!(f.count()?, 1);
    Ok(())
}
#[test]
fn revocation_between_review_and_confirm_and_during_copy_rejects() -> Result<()> {
    let f = Fixture::new()?;
    let mut args = f.args();
    import_from(&f.ops, &args, &f.home, || Ok(()))?;
    args["action"] = json!("confirm");
    let policy = f.ops.root.join("extensions/personal-ops/import.json");
    assert!(
        import_from(&f.ops, &args, &f.home, || {
            std::fs::remove_file(&policy)?;
            Ok(())
        })
        .is_err()
    );
    assert!(import_from(&f.ops, &args, &f.home, || Ok(())).is_err());
    assert_eq!(std::fs::read(&f.source)?, mp4());
    Ok(())
}
#[test]
fn prepared_import_respects_live_policy_revocation() -> Result<()> {
    for revoke_import in [true, false] {
        let f = Fixture::new()?;
        let result = f.run(f.args())?;
        let policy = if revoke_import {
            "import.json"
        } else {
            "sharing.json"
        };
        std::fs::remove_file(f.ops.root.join("extensions/personal-ops").join(policy))?;
        assert!(
            f.ops
                .prepare_files(
                    &json!({"paths":[result["approved_path"]],"recipients":["user@example.com"]})
                )
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn late_mutation_revocation_and_destination_replacement_remain_uncertain() -> Result<()> {
    for change in 0..4 {
        let f = Fixture::new()?;
        let mut args = f.args();
        import_from(&f.ops, &args, &f.home, || Ok(()))?;
        args["action"] = json!("confirm");
        let mut calls = 0;
        assert!(
            import_from(&f.ops, &args, &f.home, || {
                calls += 1;
                if calls == 2 {
                    match change {
                        0 => std::fs::write(&f.source, mp4())?,
                        1 => std::fs::remove_file(
                            f.ops.root.join("extensions/personal-ops/import.json"),
                        )?,
                        2 => {
                            let dest = f.ops.root.join("agents/main/workspace/share/imports");
                            std::fs::rename(&dest, f.home.join("old-imports"))?;
                            private_dir(&dest)?;
                        }
                        _ => std::fs::remove_file(
                            f.ops.root.join("extensions/personal-ops/sharing.json"),
                        )?,
                    }
                }
                Ok(())
            })
            .is_err()
        );
        let raw: String = f
            .ops
            .db
            .query_row("SELECT receipt FROM share_imports", [], |r| r.get(0))?;
        assert!(serde_json::from_str::<Receipt>(&raw)?.state == State::Pending);
        assert_eq!(std::fs::read(&f.source)?, mp4());
    }
    Ok(())
}
#[tokio::test]
async fn prepare_and_cleanup_through_registered_dispatch() -> Result<()> {
    let f = Fixture::new()?;
    assert!(
        crate::call(
            &f.ops,
            "files_prepare",
            &json!({"paths":[f.source],"recipients":["user@example.com"]})
        )
        .await
        .is_err()
    );
    let result = f.run(f.args())?;
    let plan = crate::call(
        &f.ops,
        "files_prepare",
        &json!({"paths":[result["approved_path"]],"recipients":["user@example.com"]}),
    )
    .await?;
    assert!(plan.is_object());
    assert_eq!(
        crate::call(&f.ops, "files_import_cleanup", &json!({})).await?["removed"],
        0
    );
    assert_eq!(
        f.ops
            .db
            .query_row("SELECT count(*) FROM deliveries", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    Ok(())
}
#[test]
fn confirm_requires_inspection_and_safe_request_id() -> Result<()> {
    let f = Fixture::new()?;
    let mut args = f.args();
    args["action"] = json!("confirm");
    assert!(import_from(&f.ops, &args, &f.home, || Ok(())).is_err());
    for id in [
        "../escape",
        "00000000-0000-0000-0000-000000000000",
        "not-a-uuid",
    ] {
        args["action"] = json!("inspect");
        args["request_id"] = json!(id);
        assert!(import_from(&f.ops, &args, &f.home, || Ok(())).is_err());
    }
    assert_eq!(f.count()?, 0);
    Ok(())
}

#[test]
fn unsupported_names_and_unsafe_import_directory_reject() -> Result<()> {
    let f = Fixture::new()?;
    for name in ["UPPER.MP4", "wrong.mov", ".hidden.mp4", "control\n.mp4"] {
        let path = f.home.join("Downloads").join(name);
        private_write(&path, &mp4())?;
        let mut args = f.args();
        args["source_path"] = json!(path);
        assert!(f.run(args).is_err());
    }
    let (dest, guard) = destination(&f.ops, true)?;
    drop(guard);
    std::fs::set_permissions(&dest, Permissions::from_mode(0o777))?;
    assert!(f.run(f.args()).is_err());
    std::fs::set_permissions(&dest, Permissions::from_mode(0o700))?;
    std::fs::remove_dir(&dest)?;
    symlink(f.home.join("Downloads"), &dest)?;
    assert!(f.run(f.args()).is_err());
    Ok(())
}

#[test]
fn durable_completed_and_uncertain_receipts_survive_reopen() -> Result<()> {
    let f = Fixture::new()?;
    let mut args = f.args();
    let completed = f.run(args.clone())?;
    args["action"] = json!("confirm");
    let reopened = Ops::open(&f.ops.root)?;
    assert_eq!(
        import_from(&reopened, &args, &f.home, || Ok(()))?,
        completed
    );
    let (_, guard) = destination(&f.ops, false)?;
    assert!(import_from(&reopened, &args, &f.home, || Ok(())).is_err());
    drop(guard);
    let mut uncertain = f.args();
    import_from(&reopened, &uncertain, &f.home, || Ok(()))?;
    uncertain["action"] = json!("confirm");
    reopened.db.execute_batch("CREATE TRIGGER reject_completion BEFORE UPDATE ON share_imports WHEN json_extract(NEW.receipt,'$.state')='complete' BEGIN SELECT RAISE(ABORT,'fixture completion failure'); END;")?;
    assert!(import_from(&reopened, &uncertain, &f.home, || Ok(())).is_err());
    reopened
        .db
        .execute_batch("DROP TRIGGER reject_completion")?;
    drop(reopened);
    let reopened = Ops::open(&f.ops.root)?;
    assert!(
        import_from(&reopened, &uncertain, &f.home, || Ok(()))
            .unwrap_err()
            .to_string()
            .contains("uncertain")
    );
    let path = f
        .ops
        .root
        .join("agents/main/workspace/share/imports")
        .join(filename(uncertain["request_id"].as_str().unwrap())?);
    assert_eq!(std::fs::read(&path)?, mp4());
    assert!(verified_bytes(&reopened, &path).is_err());
    assert_eq!(std::fs::read(&f.source)?, mp4());
    Ok(())
}
