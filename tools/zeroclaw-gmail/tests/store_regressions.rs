//! Independent durable-ledger regression tests using synthetic data and real SQLite.
//! Install as tools/zeroclaw-gmail/tests/store_regressions.rs and run:
//! cargo test --manifest-path tools/zeroclaw-gmail/Cargo.toml --test store_regressions

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use zeroclaw_gmail::{model::hash, store::Store};

fn fixture() -> Result<(TempDir, PathBuf, Store)> {
    let dir = tempfile::tempdir()?;
    // macOS tempdir may contain /var -> /private/var. Store intentionally rejects
    // symlink ancestors, so use the physical path rather than weakening its rule.
    let root = dir.path().canonicalize()?;
    let store = Store::open(&root)?;
    Ok((dir, root, store))
}

fn intent(draft: &str) -> Value {
    json!({"kind":"update","draft_id":draft,"review_id":"synthetic-review"})
}

fn review(operation: &str) -> Value {
    json!({
        "operation_id":operation,
        "kind":"update",
        "draft_id":"draft_fixture",
        "content":{"to":["recipient@example.test"],"subject":"Fixture","body":"Synthetic body"}
    })
}

fn ledger(root: &std::path::Path) -> PathBuf {
    root.join("extensions/gmail-drafts/drafts.sqlite3")
}

#[test]
fn independent_connections_claim_same_operation_at_most_once() -> Result<()> {
    let (_dir, root, store) = fixture()?;
    let count = 8;
    let connections = (0..count)
        .map(|_| Store::open(&root))
        .collect::<Result<Vec<_>>>()?;
    let start = Arc::new(Barrier::new(count));
    let threads = connections
        .into_iter()
        .map(|connection| {
            let start = Arc::clone(&start);
            thread::spawn(move || {
                start.wait();
                connection
                    .claim(
                        "one_operation",
                        &intent("draft_fixture"),
                        Some("draft_fixture"),
                    )
                    .map_err(|e| e.to_string())
            })
        })
        .collect::<Vec<_>>();
    let results = threads
        .into_iter()
        .map(|t| t.join().expect("claim thread panicked"))
        .collect::<Vec<_>>();
    // A busy SQLite snapshot is a safe losing outcome: it must never authorize
    // a provider write. Exactly one caller may receive the affirmative claim.
    assert_eq!(
        results.iter().filter(|r| matches!(r, Ok(true))).count(),
        1,
        "{results:?}"
    );
    let record = store
        .find("one_operation")?
        .context("winning claim missing")?;
    assert_eq!(record["state"], "uncertain");
    assert_eq!(record["intent"], intent("draft_fixture"));
    assert_eq!(record["automatic_retry_allowed"], false);
    for _ in 0..count {
        assert!(!Store::open(&root)?.claim(
            "one_operation",
            &intent("draft_fixture"),
            Some("draft_fixture")
        )?);
    }
    Ok(())
}

#[test]
fn independent_operations_cannot_claim_the_same_draft() -> Result<()> {
    let (_dir, root, store) = fixture()?;
    let count = 8;
    let connections = (0..count)
        .map(|_| Store::open(&root))
        .collect::<Result<Vec<_>>>()?;
    let start = Arc::new(Barrier::new(count));
    let threads = connections
        .into_iter()
        .enumerate()
        .map(|(index, connection)| {
            let start = Arc::clone(&start);
            thread::spawn(move || {
                let operation = format!("contender_{index}");
                start.wait();
                let result = connection
                    .claim(&operation, &intent("draft_fixture"), Some("draft_fixture"))
                    .map_err(|e| e.to_string());
                (operation, result)
            })
        })
        .collect::<Vec<_>>();
    let results = threads
        .into_iter()
        .map(|t| t.join().expect("claim thread panicked"))
        .collect::<Vec<_>>();
    let winners = results
        .iter()
        .filter(|(_, r)| matches!(r, Ok(true)))
        .collect::<Vec<_>>();
    assert_eq!(winners.len(), 1, "{results:?}");
    let winner = &winners[0].0;
    for (operation, result) in &results {
        if operation == winner {
            assert_eq!(
                store.find(operation)?.context("winner missing")?["state"],
                "uncertain"
            );
        } else {
            assert!(
                result.is_err(),
                "another operation silently acquired a locked draft"
            );
            assert!(
                store.find(operation)?.is_none(),
                "losing transaction left a partial operation"
            );
        }
    }
    assert!(
        store
            .claim(
                "later_contender",
                &intent("draft_fixture"),
                Some("draft_fixture")
            )
            .is_err()
    );
    store.finish(winner, "applied", &json!({"draft_id":"draft_fixture"}))?;
    assert!(store.claim(
        "later_contender",
        &intent("draft_fixture"),
        Some("draft_fixture")
    )?);
    Ok(())
}

#[test]
fn uncertain_claim_and_lock_survive_reopen_without_retry() -> Result<()> {
    let (_dir, root, store) = fixture()?;
    assert!(store.claim(
        "crash_boundary",
        &intent("draft_fixture"),
        Some("draft_fixture")
    )?);
    drop(store);
    let reopened = Store::open(&root)?;
    let record = reopened
        .find("crash_boundary")?
        .context("claim lost across reopen")?;
    assert_eq!(record["state"], "uncertain");
    assert_eq!(record["receipt"], json!({}));
    assert_eq!(record["automatic_retry_allowed"], false);
    assert!(!reopened.claim(
        "crash_boundary",
        &intent("draft_fixture"),
        Some("draft_fixture")
    )?);
    assert!(
        reopened
            .claim(
                "replacement_attempt",
                &intent("draft_fixture"),
                Some("draft_fixture")
            )
            .is_err()
    );
    reopened.finish(
        "crash_boundary",
        "uncertain",
        &json!({"reason":"provider outcome unknown"}),
    )?;
    drop(reopened);
    let reopened = Store::open(&root)?;
    assert_eq!(
        reopened.find("crash_boundary")?.context("claim missing")?["receipt"]["reason"],
        "provider outcome unknown"
    );
    assert!(
        reopened
            .claim(
                "replacement_attempt",
                &intent("draft_fixture"),
                Some("draft_fixture")
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn operation_id_cannot_be_rebound_to_another_intent() -> Result<()> {
    let (_dir, _root, store) = fixture()?;
    assert!(store.claim("bound_operation", &intent("draft_one"), Some("draft_one"))?);
    assert!(
        store
            .claim("bound_operation", &intent("draft_two"), Some("draft_two"))
            .is_err()
    );
    assert_eq!(
        store
            .find("bound_operation")?
            .context("operation missing")?["intent"],
        intent("draft_one")
    );
    // The rejected rebound must not leave a lock on its proposed new target.
    assert!(store.claim(
        "independent_operation",
        &intent("draft_two"),
        Some("draft_two")
    )?);
    assert!(
        store
            .claim("blocked_operation", &intent("draft_one"), Some("draft_one"))
            .is_err()
    );
    Ok(())
}

#[test]
fn saved_preparation_is_immutable_and_bound_to_exact_request() -> Result<()> {
    let (_dir, root, store) = fixture()?;
    let raw = b"To: recipient@example.test\r\nSubject: Fixture\r\n\r\nSynthetic body\r\n";
    let saved = store.save(
        "prepare_one",
        "request_sha256_one",
        review("prepare_one"),
        raw,
    )?;
    let review_id = saved["review_id"]
        .as_str()
        .context("review ID missing")?
        .to_owned();
    assert_eq!(saved["raw_sha256"], hash(raw));
    assert_eq!(saved["raw_bytes"], raw.len());
    assert_eq!(
        store.prepared("prepare_one", "request_sha256_one")?,
        Some(saved.clone())
    );
    assert_eq!(
        store.review("prepare_one", &review_id)?,
        (saved.clone(), raw.to_vec())
    );
    assert!(store.prepared("prepare_one", "request_sha256_two").is_err());
    assert!(
        store
            .save(
                "prepare_one",
                "request_sha256_two",
                review("prepare_one"),
                raw
            )
            .is_err()
    );
    assert!(store.review("another_operation", &review_id).is_err());
    assert!(store.review("prepare_one", &"0".repeat(64)).is_err());
    let mut changed = review("prepare_one");
    changed["content"]["body"] = json!("Changed body");
    assert_eq!(
        store.save("prepare_one", "request_sha256_one", changed, b"changed raw")?,
        saved
    );
    drop(store);
    assert_eq!(
        Store::open(&root)?.review("prepare_one", &review_id)?,
        (saved, raw.to_vec())
    );
    Ok(())
}

#[test]
fn review_rejects_raw_bytes_corrupted_on_disk() -> Result<()> {
    let (_dir, root, store) = fixture()?;
    let saved = store.save(
        "corrupt_raw",
        "request_hash",
        review("corrupt_raw"),
        b"original raw",
    )?;
    let review_id = saved["review_id"].as_str().context("review ID missing")?;
    Connection::open(ledger(&root))?.execute(
        "UPDATE preparations SET raw=?1 WHERE operation_id=?2",
        params![b"substituted raw".as_slice(), "corrupt_raw"],
    )?;
    assert!(store.review("corrupt_raw", review_id).is_err());
    Ok(())
}

#[test]
fn review_rejects_modified_review_even_when_raw_hash_is_consistent() -> Result<()> {
    let (_dir, root, store) = fixture()?;
    let raw = b"original raw";
    let mut saved = store.save(
        "corrupt_review",
        "request_hash",
        review("corrupt_review"),
        raw,
    )?;
    let review_id = saved["review_id"]
        .as_str()
        .context("review ID missing")?
        .to_owned();
    saved["content"]["to"] = json!(["substituted@example.test"]);
    assert_eq!(saved["raw_sha256"], hash(raw));
    Connection::open(ledger(&root))?.execute(
        "UPDATE preparations SET review=?1 WHERE operation_id=?2",
        params![serde_json::to_string(&saved)?, "corrupt_review"],
    )?;
    assert!(store.review("corrupt_review", &review_id).is_err());
    Ok(())
}

#[test]
fn only_terminal_resolution_of_lock_owner_unlocks_its_draft() -> Result<()> {
    let (_dir, _root, store) = fixture()?;
    assert!(store.claim("first", &intent("draft_one"), Some("draft_one"))?);
    assert!(store.claim("second", &intent("draft_two"), Some("draft_two"))?);
    assert!(store.finish("unknown", "applied", &json!({})).is_err());
    assert!(store.finish("first", "invalid_state", &json!({})).is_err());
    store.finish("first", "uncertain", &json!({"needs_reconciliation":true}))?;
    assert!(
        store
            .claim("first_successor", &intent("draft_one"), Some("draft_one"))
            .is_err()
    );
    assert!(
        store
            .claim("second_successor", &intent("draft_two"), Some("draft_two"))
            .is_err()
    );
    let resolved = store.finish("first", "applied", &json!({"confirmed":true}))?;
    assert!(store.claim("first_successor", &intent("draft_one"), Some("draft_one"))?);
    assert!(
        store
            .claim("second_successor", &intent("draft_two"), Some("draft_two"))
            .is_err()
    );
    // A repeated/stale finish must preserve the terminal receipt and cannot
    // release the successor operation's lock on the same draft.
    assert_eq!(
        store.finish("first", "rejected", &json!({"different":true}))?,
        resolved
    );
    assert!(
        store
            .claim("third_attempt", &intent("draft_one"), Some("draft_one"))
            .is_err()
    );
    assert!(!store.claim("first", &intent("draft_one"), Some("draft_one"))?);
    Ok(())
}

#[test]
fn all_supported_terminal_states_release_lock_without_replaying_operation() -> Result<()> {
    for state in ["applied", "absent", "rejected"] {
        let (_dir, _root, store) = fixture()?;
        assert!(store.claim("original", &intent("draft_fixture"), Some("draft_fixture"))?);
        let result = store.finish("original", state, &json!({"resolution":state}))?;
        assert_eq!(result["state"], state);
        assert_eq!(result["automatic_retry_allowed"], false);
        assert!(!store.claim("original", &intent("draft_fixture"), Some("draft_fixture"))?);
        assert!(store.claim("successor", &intent("draft_fixture"), Some("draft_fixture"))?);
    }
    Ok(())
}

#[test]
fn same_store_execution_guard_denies_reentry_and_drop_releases_it() -> Result<()> {
    let (_dir, _root, store) = fixture()?;
    let guard = store.execution_guard()?;
    assert!(store.execution_guard().is_err());
    drop(guard);
    let next_guard = store.execution_guard()?;
    assert!(store.execution_guard().is_err());
    drop(next_guard);
    assert!(store.execution_guard().is_ok());
    Ok(())
}

#[test]
fn separate_connections_share_execution_lock_until_guard_drops() -> Result<()> {
    let (_dir, root, first) = fixture()?;
    let second = Store::open(&root)?;
    let first_guard = first.execution_guard()?;
    assert!(second.execution_guard().is_err());
    // A failed acquisition must not poison the second Store's local guard flag.
    drop(first_guard);
    let second_guard = second.execution_guard()?;
    assert!(first.execution_guard().is_err());
    drop(second_guard);
    assert!(first.execution_guard().is_ok());
    Ok(())
}

const CHILD_ROOT: &str = "ZEROCLAW_GMAIL_EXECUTION_GUARD_TEST_ROOT";
const CHILD_MARKER: &str = "execution-guard-child-ready";

/// Invoked only by the exact-test subprocess below. No credentials, production
/// paths, services, or remote APIs are available to this helper.
#[test]
#[ignore = "subprocess fixture; requires an explicit private test root"]
fn execution_guard_child_helper() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(CHILD_ROOT).context("test root required")?);
    let store = Store::open(&root)?;
    let _guard = store.execution_guard()?;
    assert!(store.claim(
        "child_inflight_operation",
        &intent("child_draft"),
        Some("child_draft")
    )?);
    std::fs::write(root.join(CHILD_MARKER), b"guard held; claim committed")?;
    // Bounded even if the parent test aborts before its cleanup guard is created.
    thread::sleep(Duration::from_secs(30));
    anyhow::bail!("parent did not terminate its child fixture in time")
}

struct OwnedTestChild(Child);

impl Drop for OwnedTestChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
fn child_death_releases_execution_lock_but_preserves_uncertain_sql_claim() -> Result<()> {
    let (_dir, root, store) = fixture()?;
    let mut child = OwnedTestChild(
        Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "execution_guard_child_helper",
                "--ignored",
                "--test-threads=1",
            ])
            .env(CHILD_ROOT, &root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join(CHILD_MARKER).exists() {
        assert!(
            child.0.try_wait()?.is_none(),
            "child exited before holding the lock"
        );
        assert!(
            Instant::now() < deadline,
            "child did not acquire the execution lock"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        store.execution_guard().is_err(),
        "cross-process guard was not exclusive"
    );
    let before = store
        .find("child_inflight_operation")?
        .context("child claim missing")?;
    assert_eq!(before["state"], "uncertain");
    child.0.kill()?;
    assert!(
        !child.0.wait()?.success(),
        "child fixture should have been terminated"
    );
    let _guard = store.execution_guard()?;
    let reopened = Store::open(&root)?;
    assert_eq!(reopened.find("child_inflight_operation")?, Some(before));
    assert!(!reopened.claim(
        "child_inflight_operation",
        &intent("child_draft"),
        Some("child_draft")
    )?);
    assert!(
        reopened
            .claim(
                "replacement_attempt",
                &intent("child_draft"),
                Some("child_draft")
            )
            .is_err()
    );
    Ok(())
}
