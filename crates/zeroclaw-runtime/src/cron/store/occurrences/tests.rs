use super::*;
use tempfile::TempDir;

fn read(
    config: &Config,
    job_id: &str,
    query: &OccurrenceQuery,
) -> Result<OccurrencePage, OccurrenceReadError> {
    read_path(&cron_db_path(config), job_id, query)
}

fn fixture() -> (TempDir, Config, Connection) {
    let tmp = TempDir::new().unwrap();
    let config = Config {
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };
    super::super::with_initialized_connection(&config, |_| Ok(())).unwrap();
    let conn = Connection::open(cron_db_path(&config)).unwrap();
    (tmp, config, conn)
}

fn insert(conn: &Connection, id: &str, execution: &str, delivery: &str, output: &str) {
    conn.execute("INSERT INTO cron_occurrences VALUES ('deleted-job', ?1, ?2, ?3, ?4, '2026-01-01T00:00:00Z')",
        params![id, execution, delivery, output]).unwrap();
}

#[tokio::test]
async fn missing_storage_is_explicit_and_read_does_not_create_it() {
    let tmp = TempDir::new().unwrap();
    let config = Config {
        data_dir: tmp.path().join("absent"),
        ..Config::default()
    };
    let page = read_occurrences(&config, "job".into(), OccurrenceQuery::default())
        .await
        .unwrap();
    assert!(!page.storage_present);
    assert!(page.occurrences.is_empty());
    assert!(!config.data_dir.exists());
    assert_eq!(
        read(
            &config,
            "job",
            &OccurrenceQuery {
                limit: Some(0),
                ..Default::default()
            }
        )
        .unwrap_err(),
        OccurrenceReadError::InvalidQuery
    );
}

#[test]
fn cursor_reads_deleted_job_receipts_without_duplicates_or_mutation() {
    let (_tmp, config, conn) = fixture();
    for id in ["manual:z", "2026-01-01", "manual:a", "\u{10ffff}z"] {
        insert(&conn, id, "confirmed", "not_requested", "private output");
    }
    let before_bytes = std::fs::read(cron_db_path(&config)).unwrap();
    let mut query = OccurrenceQuery {
        limit: Some(2),
        ..Default::default()
    };
    let mut ids = Vec::new();
    loop {
        let page = read(&config, "deleted-job", &query).unwrap();
        assert!(page.storage_present);
        for receipt in page.occurrences {
            assert!(receipt.output.is_none());
            assert_eq!(receipt.output_bytes, 14);
            assert!(!receipt.retry_allowed);
            assert_eq!(receipt.delivery_outcome, None);
            ids.push(receipt.occurrence_id);
        }
        query.before = page.next_before;
        if query.before.is_none() {
            break;
        }
    }
    assert_eq!(ids, ["\u{10ffff}z", "manual:z", "manual:a", "2026-01-01"]);
    assert_eq!(before_bytes, std::fs::read(cron_db_path(&config)).unwrap());
    assert!(
        read(&config, "other-job", &query)
            .unwrap()
            .occurrences
            .is_empty()
    );
    let exact = read(
        &config,
        "deleted-job",
        &OccurrenceQuery {
            occurrence_id: Some("manual:a".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(exact.occurrences.len(), 1);
    assert!(exact.next_before.is_none());
}

#[test]
fn encoded_page_and_unicode_output_bounds_preserve_pagination() {
    let (_tmp, config, conn) = fixture();
    for i in 0..30 {
        insert(
            &conn,
            &format!("manual:{i:03}"),
            "confirmed",
            "confirmed",
            &"😀\n\"".repeat(5000),
        );
    }
    let mut query = OccurrenceQuery {
        limit: Some(100),
        include_output: true,
        ..Default::default()
    };
    let mut ids = std::collections::HashSet::new();
    loop {
        let page = read(&config, "deleted-job", &query).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= PAGE_BYTES);
        assert!(!page.occurrences.is_empty());
        for receipt in page.occurrences {
            assert!(receipt.output_truncated);
            assert_eq!(receipt.output_bytes, 30000);
            assert!(receipt.output.unwrap().len() <= OUTPUT_BYTES);
            assert!(ids.insert(receipt.occurrence_id));
        }
        query.before = page.next_before;
        if query.before.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 30);
}

#[test]
fn uncertainty_is_typed_and_unknown_states_never_confirm() {
    let (_tmp, config, conn) = fixture();
    for (id, execution, delivery) in [
        ("a", "claimed", "submitting"),
        ("b", "running", "possibly_applied"),
        ("c", "possibly_applied", "reconciliation_required"),
        ("d", "future_state", "future_state"),
        ("e", "skipped", "not_started"),
        ("f", "confirmed", "partially_applied"),
        ("g", "confirmed", "confirmed_failed"),
    ] {
        insert(&conn, id, execution, delivery, "");
    }
    let page = read(&config, "deleted-job", &OccurrenceQuery::default()).unwrap();
    for receipt in &page.occurrences {
        assert!(!receipt.retry_allowed);
        assert_ne!(receipt.delivery_outcome, Some(EffectOutcome::Confirmed));
    }
    let unknown = page
        .occurrences
        .iter()
        .find(|r| r.occurrence_id == "d")
        .unwrap();
    assert_eq!(
        unknown.execution_outcome,
        EffectOutcome::ReconciliationRequired
    );
    assert_eq!(
        unknown.delivery_outcome,
        Some(EffectOutcome::ReconciliationRequired)
    );
    let claimed = page
        .occurrences
        .iter()
        .find(|r| r.occurrence_id == "a")
        .unwrap();
    assert_eq!(claimed.execution_outcome, EffectOutcome::PossiblyApplied);
    assert_eq!(
        claimed.delivery_outcome,
        Some(EffectOutcome::ReconciliationRequired)
    );
}

#[test]
fn malformed_or_locked_storage_is_not_empty_history() {
    let (_tmp, config, conn) = fixture();
    insert(&conn, "one", "confirmed", "confirmed", "body");
    conn.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE")
        .unwrap();
    assert_eq!(
        read(&config, "deleted-job", &OccurrenceQuery::default()).unwrap_err(),
        OccurrenceReadError::StorageUnavailable
    );
    conn.execute_batch("ROLLBACK").unwrap();
    conn.execute("UPDATE cron_occurrences SET updated_at='bad timestamp'", [])
        .unwrap();
    assert_eq!(
        read(&config, "deleted-job", &OccurrenceQuery::default()).unwrap_err(),
        OccurrenceReadError::StorageUnavailable
    );
    conn.execute_batch("DROP TABLE cron_occurrences").unwrap();
    assert_eq!(
        read(&config, "deleted-job", &OccurrenceQuery::default()).unwrap_err(),
        OccurrenceReadError::StorageUnavailable
    );
}

#[test]
fn malformed_output_is_only_read_on_explicit_request_and_metadata_is_bounded() {
    let (_tmp, config, conn) = fixture();
    insert(&conn, "one", "confirmed", "confirmed", "");
    conn.execute("UPDATE cron_occurrences SET output=x'fffe'", [])
        .unwrap();
    assert!(
        read(&config, "deleted-job", &OccurrenceQuery::default())
            .unwrap()
            .occurrences[0]
            .output
            .is_none()
    );
    assert_eq!(
        read(
            &config,
            "deleted-job",
            &OccurrenceQuery {
                include_output: true,
                ..Default::default()
            }
        )
        .unwrap_err(),
        OccurrenceReadError::StorageUnavailable
    );
    conn.execute(
        "UPDATE cron_occurrences SET execution_state=?1",
        ["x".repeat(100000)],
    )
    .unwrap();
    assert_eq!(
        read(&config, "deleted-job", &OccurrenceQuery::default()).unwrap_err(),
        OccurrenceReadError::StorageUnavailable
    );
}

#[test]
fn query_uses_primary_key_for_tail_cursor_and_exact_read() {
    let (_tmp, _config, conn) = fixture();
    for query in [
        OccurrenceQuery::default(),
        OccurrenceQuery {
            before: Some("z".into()),
            ..Default::default()
        },
        OccurrenceQuery {
            occurrence_id: Some("a".into()),
            ..Default::default()
        },
    ] {
        let sql = format!("EXPLAIN QUERY PLAN {}", query_sql(&query));
        let mut stmt = conn.prepare(&sql).unwrap();
        let plans: Vec<String> = stmt
            .query_map(params!["deleted-job", "z", 21, false, OUTPUT_BYTES], |r| {
                r.get(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plans.iter().any(
                |p| p.contains("SEARCH cron_occurrences USING INDEX") && p.contains("job_id=?")
            ),
            "{plans:?}"
        );
        assert!(
            !plans
                .iter()
                .any(|p| p.contains("TEMP B-TREE") || p.contains("SCAN cron_occurrences")),
            "{plans:?}"
        );
        if query.before.is_some() {
            assert!(
                plans.iter().any(|p| p.contains("scheduled_at<?")),
                "{plans:?}"
            );
        }
    }
}
