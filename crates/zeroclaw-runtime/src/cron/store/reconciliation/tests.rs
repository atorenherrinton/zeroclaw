use super::*;
use tempfile::TempDir;

const OCCURRENCE: &str = "manual:key:original";
const SOURCE: &str = "local_operator:loopback_admin";

fn fixture() -> (TempDir, Config, String, i64, ReconciliationRequest) {
    let tmp = TempDir::new().unwrap();
    let config = Config {
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };
    let job = add_job(&config, "main", "0 * * * *", "echo test").unwrap();
    let finished = Utc::now();
    let started = finished - chrono::Duration::seconds(1);
    record_run(
        &config,
        &job.id,
        started,
        finished,
        "error",
        Some("failed before connector"),
        1000,
    )
    .unwrap();
    with_initialized_connection(&config, |conn| {
        conn.execute("UPDATE cron_jobs SET enabled=0,last_status='uncertain',last_run=?2,last_output='failed before connector' WHERE id=?1", params![job.id,finished.to_rfc3339()])?;
        conn.execute("INSERT INTO cron_occurrences VALUES (?1,?2,'possibly_applied','not_requested','failed before connector',?3)", params![job.id,OCCURRENCE,finished.to_rfc3339()])?;
        Ok(())
    }).unwrap();
    let run_id = list_runs(&config, &job.id, 1).unwrap()[0].id;
    let status = reconciliation_status(&config, &job.id, run_id, OCCURRENCE).unwrap();
    assert!(status.eligible);
    let request = ReconciliationRequest {
        occurrence_id: OCCURRENCE.into(),
        expected_state: status.expected_state.unwrap(),
        disposition: ReconciliationDisposition::NoExternalEffect,
        evidence: "Owner inspected exact run; zero connector calls and zero external effects"
            .into(),
    };
    (tmp, config, job.id, run_id, request)
}

#[test]
fn exact_no_effect_receipt_releases_only_gate_without_enabling_or_rewriting_history() {
    let (_tmp, config, id, run, request) = fixture();
    let before = snapshot(
        &Connection::open(cron_db_path(&config)).unwrap(),
        &id,
        run,
        OCCURRENCE,
    )
    .unwrap();
    assert!(
        update_job(
            &config,
            &id,
            CronJobPatch {
                enabled: Some(true),
                ..Default::default()
            }
        )
        .is_err()
    );
    let receipt = reconcile_no_external_effect(&config, &id, run, &request, SOURCE).unwrap();
    assert!(!receipt.retry_allowed);
    let after = snapshot(
        &Connection::open(cron_db_path(&config)).unwrap(),
        &id,
        run,
        OCCURRENCE,
    )
    .unwrap();
    assert_eq!(before.run, after.run);
    assert_eq!(before.occurrence, after.occurrence);
    let mut expected = serde_json::to_value(before.job).unwrap();
    expected["last_status"] = "reconciled_no_external_effect".into();
    assert_eq!(expected, serde_json::to_value(&after.job).unwrap());
    assert!(!after.job.enabled);
    assert_eq!(list_runs(&config, &id, 10).unwrap().len(), 1);
    let enabled = update_job(
        &config,
        &id,
        CronJobPatch {
            enabled: Some(true),
            missed_run_policy: Some(Some(CronMissedRunPolicy::Skip)),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(enabled.next_run > Utc::now());
    assert!(claim_manual_run_with_key(&config, &enabled, Some("fresh-verification")).is_ok());
}

#[test]
fn duplicate_identical_is_deterministic_but_conflicting_repeat_is_rejected() {
    let (_tmp, config, id, run, mut request) = fixture();
    let first = reconcile_no_external_effect(&config, &id, run, &request, SOURCE).unwrap();
    let bytes = std::fs::read(cron_db_path(&config)).unwrap();
    assert_eq!(
        first,
        reconcile_no_external_effect(&config, &id, run, &request, SOURCE).unwrap()
    );
    assert_eq!(bytes, std::fs::read(cron_db_path(&config)).unwrap());
    assert!(
        reconcile_no_external_effect(&config, &id, run, &request, "forged-other-source").is_err()
    );
    request.evidence.push('!');
    assert!(reconcile_no_external_effect(&config, &id, run, &request, SOURCE).is_err());
}

#[test]
fn mismatched_job_run_occurrence_and_ambiguous_identifiers_reject() {
    let (_tmp, config, id, run, request) = fixture();
    let other = add_job(&config, "main", "0 * * * *", "echo other").unwrap();
    for (job, number) in [
        (&other.id, run),
        (&id, run + 1),
        (&id, 0),
        (&"latest".to_string(), run),
    ] {
        assert!(reconcile_no_external_effect(&config, job, number, &request, SOURCE).is_err());
    }
    let mut wrong = request.clone();
    wrong.occurrence_id = "latest".into();
    assert!(reconcile_no_external_effect(&config, &id, run, &wrong, SOURCE).is_err());
    assert!(reconciliation_status(&config, &id, run, "latest").is_err());
}

#[test]
fn stale_job_and_receipt_expectations_reject() {
    for change in [
        "UPDATE cron_jobs SET name='changed'",
        "UPDATE cron_jobs SET enabled=1",
        "UPDATE cron_jobs SET last_status='error'",
        "UPDATE cron_jobs SET locked_at='2026-01-01T00:00:00Z'",
        "UPDATE cron_occurrences SET updated_at='changed'",
        "UPDATE cron_occurrences SET delivery_state='submitted'",
        "UPDATE cron_runs SET output='different'",
    ] {
        let (_tmp, config, id, run, request) = fixture();
        with_initialized_connection(&config, |conn| {
            conn.execute_batch(change)?;
            Ok(())
        })
        .unwrap();
        assert!(
            reconcile_no_external_effect(&config, &id, run, &request, SOURCE).is_err(),
            "{change}"
        );
    }
}

#[test]
fn newer_run_and_multiple_uncertainties_reject_even_with_fresh_token() {
    let (_tmp, config, id, run, mut request) = fixture();
    record_run(
        &config,
        &id,
        Utc::now(),
        Utc::now(),
        "error",
        Some("failed before connector"),
        1,
    )
    .unwrap();
    request.expected_state = reconciliation_status(&config, &id, run, OCCURRENCE)
        .unwrap()
        .expected_state
        .unwrap();
    assert!(reconcile_no_external_effect(&config, &id, run, &request, SOURCE).is_err());
    let (_tmp, config, id, run, mut request) = fixture();
    with_initialized_connection(&config,|conn|{conn.execute("INSERT INTO cron_occurrences VALUES (?1,'another','possibly_applied','not_requested',NULL,'now')",[&id])?;Ok(())}).unwrap();
    request.expected_state = reconciliation_status(&config, &id, run, OCCURRENCE)
        .unwrap()
        .expected_state
        .unwrap();
    assert!(reconcile_no_external_effect(&config, &id, run, &request, SOURCE).is_err());
}

#[test]
fn receipt_failure_and_gate_failure_are_atomic() {
    for trigger in [
        "CREATE TRIGGER reject_receipt BEFORE INSERT ON cron_reconciliations BEGIN SELECT RAISE(ABORT,'fault'); END",
        "CREATE TRIGGER reject_gate BEFORE UPDATE ON cron_jobs BEGIN SELECT RAISE(ABORT,'fault'); END",
    ] {
        let (_tmp, config, id, run, request) = fixture();
        with_initialized_connection(&config, |conn| {
            conn.execute_batch(trigger)?;
            Ok(())
        })
        .unwrap();
        assert!(reconcile_no_external_effect(&config, &id, run, &request, SOURCE).is_err());
        let status = reconciliation_status(&config, &id, run, OCCURRENCE).unwrap();
        assert!(status.receipt.is_none());
        assert!(status.eligible);
        assert!(!status.enabled);
    }
}

#[test]
fn status_is_read_only_durable_and_never_authorizes_replay() {
    let (_tmp, config, id, run, request) = fixture();
    let expected = reconcile_no_external_effect(&config, &id, run, &request, SOURCE).unwrap();
    let bytes = std::fs::read(cron_db_path(&config)).unwrap();
    let status = reconciliation_status(&config, &id, run, OCCURRENCE).unwrap();
    assert_eq!(status.receipt, Some(expected));
    assert!(!status.eligible);
    assert!(!status.retry_allowed);
    assert!(!status.enabled);
    assert_eq!(bytes, std::fs::read(cron_db_path(&config)).unwrap());
}

#[test]
fn reconciliation_never_replays_original_manual_key() {
    let (_tmp, config, id, run, mut request) = fixture();
    // Reuse the maintained key derivation by taking a temporary fresh claim.
    let key = "original-request";
    let original = format!(
        "manual:key:{:x}",
        Sha256::digest(serde_json::to_vec(&(&id, "main", key)).unwrap())
    );
    with_initialized_connection(&config, |conn| {
        conn.execute(
            "UPDATE cron_occurrences SET scheduled_at=?1 WHERE job_id=?2",
            params![original, id],
        )?;
        Ok(())
    })
    .unwrap();
    request.occurrence_id = original.clone();
    request.expected_state = reconciliation_status(&config, &id, run, &original)
        .unwrap()
        .expected_state
        .unwrap();
    reconcile_no_external_effect(&config, &id, run, &request, SOURCE).unwrap();
    let job = get_job(&config, &id).unwrap();
    let error = claim_manual_run_with_key(&config, &job, Some(key)).unwrap_err();
    assert!(error.downcast_ref::<DuplicateManualReceipt>().is_some());
    assert_eq!(list_runs(&config, &id, 10).unwrap().len(), 1);
}

#[test]
fn changed_reconciled_occurrence_is_quarantined_again() {
    let (_tmp, config, id, run, request) = fixture();
    reconcile_no_external_effect(&config, &id, run, &request, SOURCE).unwrap();
    with_initialized_connection(&config, |conn| {
        conn.execute(
            "UPDATE cron_occurrences SET updated_at='changed' WHERE job_id=?1",
            [&id],
        )?;
        Ok(())
    })
    .unwrap();
    let job = get_job(&config, &id).unwrap();
    assert!(
        claim_manual_run_with_key(&config, &job, Some("new-request"))
            .unwrap_err()
            .downcast_ref::<ManualAdmissionError>()
            .is_some()
    );
}

#[test]
fn receipt_status_survives_ordinary_run_history_retention() {
    let (_tmp, config, id, run, request) = fixture();
    let expected = reconcile_no_external_effect(&config, &id, run, &request, SOURCE).unwrap();
    with_initialized_connection(&config, |conn| {
        conn.execute(
            "DELETE FROM cron_runs WHERE job_id=?1 AND id=?2",
            params![id, run],
        )?;
        Ok(())
    })
    .unwrap();
    let status = reconciliation_status(&config, &id, run, OCCURRENCE).unwrap();
    assert_eq!(status.receipt, Some(expected));
    assert!(!status.retry_allowed);
}

#[test]
fn absent_storage_is_not_created_by_status_or_reconciliation() {
    let tmp = TempDir::new().unwrap();
    let config = Config {
        data_dir: tmp.path().join("absent"),
        ..Config::default()
    };
    let request = ReconciliationRequest {
        occurrence_id: OCCURRENCE.into(),
        expected_state: "0".repeat(64),
        disposition: ReconciliationDisposition::NoExternalEffect,
        evidence: "operator evidence".into(),
    };
    assert!(reconciliation_status(&config, "exact-job", 1, OCCURRENCE).is_err());
    assert!(reconcile_no_external_effect(&config, "exact-job", 1, &request, SOURCE).is_err());
    assert!(!config.data_dir.exists());
}
