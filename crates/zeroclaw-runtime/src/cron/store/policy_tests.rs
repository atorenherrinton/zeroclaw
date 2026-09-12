use super::*;
use tempfile::TempDir;

fn fixture() -> (TempDir, Config, CronJob) {
    let tmp = TempDir::new().unwrap();
    let config = Config {
        data_dir: tmp.path().join("data"),
        config_path: tmp.path().join("config.toml"),
        ..Config::default()
    };
    let job = add_job(&config, "synthetic-owner", "* * * * *", "echo synthetic").unwrap();
    (tmp, config, job)
}

fn patch(value: serde_json::Value) -> CronJobPatch {
    serde_json::from_value(value).unwrap()
}

#[test]
fn imperative_policy_patch_roundtrips_and_null_restores_inheritance() {
    let (_tmp, config, original) = fixture();
    for (value, expected) in [
        (serde_json::json!("skip"), Some(CronMissedRunPolicy::Skip)),
        (
            serde_json::json!("reconcile"),
            Some(CronMissedRunPolicy::Reconcile),
        ),
        (
            serde_json::json!("catch_up_once"),
            Some(CronMissedRunPolicy::CatchUpOnce),
        ),
        (serde_json::Value::Null, None),
    ] {
        let requested = patch(serde_json::json!({"missed_run_policy":value}));
        let serialized = serde_json::to_value(&requested).unwrap();
        assert_eq!(serialized["missed_run_policy"], value);
        let updated =
            update_job_for_agent(&config, &original.id, "synthetic-owner", requested).unwrap();
        assert_eq!(updated.missed_run_policy, expected);
        assert_eq!(updated.next_run, original.next_run);
        let unchanged = update_job(
            &config,
            &original.id,
            patch(serde_json::json!({"name":"renamed"})),
        )
        .unwrap();
        assert_eq!(unchanged.missed_run_policy, expected);
        assert_eq!(list_jobs(&config).unwrap()[0].missed_run_policy, expected);
        assert_eq!(
            list_jobs_by_agent(&config, "synthetic-owner").unwrap()[0].missed_run_policy,
            expected
        );
    }
    assert!(
        serde_json::to_value(CronJobPatch::default())
            .unwrap()
            .get("missed_run_policy")
            .is_none()
    );
    for invalid in [
        serde_json::json!("retry"),
        serde_json::json!(42),
        serde_json::json!({}),
        serde_json::json!(false),
    ] {
        assert!(
            serde_json::from_value::<CronJobPatch>(
                serde_json::json!({"missed_run_policy":invalid})
            )
            .is_err()
        );
    }
}

#[test]
fn imperative_policy_migration_defaults_and_malformed_storage_fail_closed() {
    let (_tmp, config, job) = fixture();
    let conn = Connection::open(cron_db_path(&config)).unwrap();
    conn.execute_batch("ALTER TABLE cron_jobs DROP COLUMN missed_run_policy")
        .unwrap();
    assert_eq!(get_job(&config, &job.id).unwrap().missed_run_policy, None);
    for raw in ["'future-policy'", "42", "X'00ff'"] {
        conn.execute_batch(&format!("UPDATE cron_jobs SET missed_run_policy={raw}"))
            .unwrap();
        assert!(get_job(&config, &job.id).is_err());
        assert!(list_jobs(&config).is_err());
        assert!(
            update_job(
                &config,
                &job.id,
                patch(serde_json::json!({"missed_run_policy":null}))
            )
            .is_err()
        );
        assert!(
            due_jobs(&config, Utc::now() + chrono::Duration::hours(1))
                .unwrap()
                .is_empty()
        );
        assert!(
            all_overdue_jobs(&config, Utc::now() + chrono::Duration::hours(1))
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn declarative_policy_ignores_db_shadow_and_rejects_even_null_patches() {
    let (_tmp, mut config, job) = fixture();
    config.cron.insert(
        job.id.clone(),
        zeroclaw_config::schema::CronJobDecl {
            missed_run_policy: Some(CronMissedRunPolicy::Reconcile),
            ..Default::default()
        },
    );
    let conn = Connection::open(cron_db_path(&config)).unwrap();
    conn.execute_batch("UPDATE cron_jobs SET source='declarative', missed_run_policy=X'00ff'")
        .unwrap();
    assert_eq!(
        get_job(&config, &job.id).unwrap().missed_run_policy,
        Some(CronMissedRunPolicy::Reconcile)
    );
    let updated = update_job(
        &config,
        &job.id,
        patch(serde_json::json!({"name":"synthetic"})),
    )
    .unwrap();
    assert_eq!(
        updated.missed_run_policy,
        Some(CronMissedRunPolicy::Reconcile)
    );
    for value in [serde_json::json!("skip"), serde_json::Value::Null] {
        assert!(
            update_job(
                &config,
                &job.id,
                patch(serde_json::json!({"name":"must-roll-back","missed_run_policy":value}))
            )
            .is_err()
        );
        assert_eq!(
            get_job(&config, &job.id).unwrap().name.as_deref(),
            Some("synthetic")
        );
    }
    let shadow: Vec<u8> = conn
        .query_row("SELECT missed_run_policy FROM cron_jobs", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(shadow, vec![0, 255]);
    config.cron.get_mut(&job.id).unwrap().missed_run_policy = Some(CronMissedRunPolicy::Skip);
    assert_eq!(
        get_job(&config, &job.id).unwrap().missed_run_policy,
        Some(CronMissedRunPolicy::Skip)
    );
}

#[tokio::test]
async fn imperative_policy_edits_preserve_owner_claim_and_quarantine_evidence() {
    let (_tmp, config, job) = fixture();
    assert!(claim_job(&config, &job.id, Utc::now()).unwrap());
    let conn = Connection::open(cron_db_path(&config)).unwrap();
    conn.execute_batch("UPDATE cron_jobs SET enabled=0,last_status='uncertain'")
        .unwrap();
    let evidence = read_occurrences(&config, job.id.clone(), OccurrenceQuery::default())
        .await
        .unwrap();
    let before = serde_json::to_value(evidence).unwrap();
    assert!(
        update_job_for_agent(
            &config,
            &job.id,
            "other-owner",
            patch(serde_json::json!({"missed_run_policy":"catch_up_once"}))
        )
        .is_err()
    );
    for value in [serde_json::json!("catch_up_once"), serde_json::Value::Null] {
        let error = update_job_for_agent(
            &config,
            &job.id,
            "synthetic-owner",
            patch(serde_json::json!({"enabled":true,"missed_run_policy":value})),
        )
        .unwrap_err();
        assert!(error.to_string().contains("reconciliation"));
        let updated = update_job_for_agent(
            &config,
            &job.id,
            "synthetic-owner",
            patch(serde_json::json!({"missed_run_policy":value})),
        )
        .unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.last_status.as_deref(), Some("uncertain"));
        assert_eq!(updated.next_run, job.next_run);
        assert!(!claim_job(&config, &job.id, Utc::now()).unwrap());
        assert!(
            due_jobs(&config, Utc::now() + chrono::Duration::hours(1))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            serde_json::to_value(
                read_occurrences(&config, job.id.clone(), OccurrenceQuery::default())
                    .await
                    .unwrap()
            )
            .unwrap(),
            before
        );
        let locked: bool = conn
            .query_row("SELECT locked_at IS NOT NULL FROM cron_jobs", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(locked);
    }
}

#[test]
fn imperative_policy_failed_update_is_atomic() {
    let (_tmp, config, job) = fixture();
    let conn = Connection::open(cron_db_path(&config)).unwrap();
    conn.execute_batch("CREATE TRIGGER refuse_policy BEFORE UPDATE OF missed_run_policy ON cron_jobs BEGIN SELECT RAISE(ABORT, 'synthetic storage refusal'); END;").unwrap();
    assert!(
        update_job(
            &config,
            &job.id,
            patch(serde_json::json!({"name":"must-roll-back", "missed_run_policy":"skip"}))
        )
        .is_err()
    );
    let after = get_job(&config, &job.id).unwrap();
    assert_eq!(after.name, job.name);
    assert_eq!(after.missed_run_policy, None);
    assert_eq!(after.next_run, job.next_run);
}

#[test]
fn imperative_policy_stale_startup_disposition_cannot_overwrite_new_override() {
    let (_tmp, config, job) = fixture();
    let now = Utc::now();
    let conn = Connection::open(cron_db_path(&config)).unwrap();
    conn.execute(
        "UPDATE cron_jobs SET next_run=?1, source=NULL",
        [(now - chrono::Duration::hours(1)).to_rfc3339()],
    )
    .unwrap();
    // Legacy NULL source rows have the same imperative owner as the row decoder.
    let stale = get_job(&config, &job.id).unwrap();
    update_job(
        &config,
        &job.id,
        patch(serde_json::json!({"missed_run_policy":"skip"})),
    )
    .unwrap();
    assert!(checkpoint_missed_run(&config, &stale, now, true).is_err());
    let current = get_job(&config, &job.id).unwrap();
    assert_eq!(current.last_status, None);
    assert_eq!(current.next_run, stale.next_run);
    let count: u64 = conn
        .query_row("SELECT count(*) FROM cron_occurrences", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    checkpoint_missed_run(&config, &current, now, false).unwrap();
    assert_eq!(
        get_job(&config, &job.id).unwrap().last_status.as_deref(),
        Some("skipped")
    );
}
