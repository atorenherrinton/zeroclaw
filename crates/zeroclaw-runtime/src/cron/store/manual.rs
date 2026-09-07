//! Manual invocations share the cron occurrence ledger and per-job lock.
//! The invocation creates a new identity; it never consumes a scheduled timestamp.
use super::*;
use zeroclaw_api::delivery::EffectOutcome;

#[derive(Debug, Clone)]
pub(crate) struct ManualClaim {
    job_id: String,
    occurrence_id: String,
}

impl ManualClaim {
    pub(crate) fn id(&self) -> &str {
        &self.occurrence_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManualAdmissionError {
    Quarantined,
    InFlight,
    Changed,
    InvalidRequestId,
}

impl std::fmt::Display for ManualAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "manual cron admission refused: {self:?}")
    }
}
impl std::error::Error for ManualAdmissionError {}

#[derive(Debug)]
pub(crate) struct DuplicateManualReceipt {
    pub(crate) occurrence_id: String,
    pub(crate) execution: EffectOutcome,
    pub(crate) delivery: Option<EffectOutcome>,
    pub(crate) effect: EffectOutcome,
    pub(crate) output: String,
}
impl std::fmt::Display for DuplicateManualReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "manual occurrence already admitted: {}",
            self.occurrence_id
        )
    }
}
impl std::error::Error for DuplicateManualReceipt {}

#[cfg(test)]
fn claim_manual_run(config: &Config, expected: &CronJob) -> Result<ManualClaim> {
    claim_manual_run_with_key(config, expected, None)
}

pub(crate) fn claim_manual_run_with_key(
    config: &Config,
    expected: &CronJob,
    request_id: Option<&str>,
) -> Result<ManualClaim> {
    let occurrence_id = match request_id {
        Some(key) => {
            if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(ManualAdmissionError::InvalidRequestId.into());
            }
            use sha2::{Digest, Sha256};
            // The owner is part of the invocation identity, not a cached policy.
            // Reassigning a job must not expose its prior owner's saved receipt.
            let identity = serde_json::to_vec(&(&expected.id, &expected.agent_alias, key))?;
            format!("manual:key:{:x}", Sha256::digest(identity))
        }
        None => format!("manual:{}", Uuid::new_v4()),
    };
    with_initialized_connection(config, |conn| {
        conn.execute_batch("PRAGMA synchronous=FULL")?;
        let tx = conn.unchecked_transaction()?;
        let mut current = read_job_row(&tx, &expected.id)?;
        resolve_declarative_shell_output_format(config, &mut current);
        if current.agent_alias != expected.agent_alias {
            return Err(ManualAdmissionError::Changed.into());
        }
        if request_id.is_some() {
            use rusqlite::OptionalExtension;
            let existing: Option<(String,String,String)> = tx.query_row(
                "SELECT execution_state,delivery_state,CASE WHEN length(CAST(output AS BLOB))>?3 THEN NULL ELSE COALESCE(output,'') END FROM cron_occurrences WHERE job_id=?1 AND scheduled_at=?2",
                params![expected.id,occurrence_id,MAX_CRON_OUTPUT_BYTES as i64], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if let Some((execution, delivery, output)) = existing {
                let execution = if execution == "confirmed" {
                    EffectOutcome::Confirmed
                } else {
                    EffectOutcome::PossiblyApplied
                };
                let delivery = if delivery == "not_requested" {
                    None
                } else {
                    Some(
                        serde_json::from_value(serde_json::Value::String(delivery))
                            .unwrap_or(EffectOutcome::ReconciliationRequired),
                    )
                };
                return Err(DuplicateManualReceipt {
                    occurrence_id,
                    execution,
                    delivery,
                    effect: manual_effect(execution == EffectOutcome::Confirmed, delivery),
                    output,
                }
                .into());
            }
        }

        let locked: bool = tx.query_row(
            "SELECT locked_at IS NOT NULL OR lock_owner IS NOT NULL FROM cron_jobs WHERE id=?1",
            [&expected.id],
            |r| r.get(0),
        )?;
        if locked {
            return Err(ManualAdmissionError::InFlight.into());
        }
        let uncertain: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM cron_occurrences WHERE job_id=?1 AND
             (execution_state='possibly_applied' OR delivery_state IN
              ('submitting','submitted','uncertain','possibly_applied','partially_applied','reconciliation_required')))",
            [&expected.id], |r| r.get(0))?;
        if current.last_status.as_deref() == Some("uncertain") || uncertain {
            return Err(ManualAdmissionError::Quarantined.into());
        }
        // Reject stale policy/command/ownership snapshots instead of applying an
        // approval obtained for one command to a newer database definition.
        if serde_json::to_value(&current)? != serde_json::to_value(expected)? {
            return Err(ManualAdmissionError::Changed.into());
        }
        let claim = ManualClaim {
            job_id: expected.id.clone(),
            occurrence_id,
        };
        let now = Utc::now().to_rfc3339();
        let changed = tx.execute(
            "UPDATE cron_jobs SET locked_at=?2, lock_owner=?3 WHERE id=?1 AND locked_at IS NULL AND lock_owner IS NULL AND COALESCE(last_status,'')!='uncertain'",
            params![claim.job_id,now,claim.occurrence_id])?;
        if changed != 1 {
            return Err(ManualAdmissionError::InFlight.into());
        }
        tx.execute("INSERT INTO cron_occurrences(job_id,scheduled_at,execution_state,updated_at) VALUES(?1,?2,'claimed',?3)",
            params![claim.job_id,claim.occurrence_id,now])?;
        checkpoint_occurrence_inner(
            &tx,
            &claim.job_id,
            &claim.occurrence_id,
            "running",
            "not_started",
            None,
        )?;
        tx.commit()?;
        Ok(claim)
    })
}

pub(crate) fn checkpoint_manual_execution(
    config: &Config,
    claim: &ManualClaim,
    success: bool,
    output: &str,
) -> Result<()> {
    with_initialized_connection(config, |conn| {
        conn.execute_batch("PRAGMA synchronous=FULL")?;
        checkpoint_occurrence_inner(
            conn,
            &claim.job_id,
            &claim.occurrence_id,
            if success {
                "confirmed"
            } else {
                "possibly_applied"
            },
            "submitting",
            Some(output),
        )
    })
}

fn manual_effect(execution_success: bool, delivery: Option<EffectOutcome>) -> EffectOutcome {
    if !execution_success {
        EffectOutcome::ReconciliationRequired
    } else {
        match delivery {
            None | Some(EffectOutcome::Confirmed) => EffectOutcome::Confirmed,
            Some(
                EffectOutcome::NotStarted
                | EffectOutcome::ConfirmedFailed
                | EffectOutcome::PartiallyApplied,
            ) => EffectOutcome::PartiallyApplied,
            Some(EffectOutcome::PossiblyApplied | EffectOutcome::ReconciliationRequired) => {
                EffectOutcome::ReconciliationRequired
            }
        }
    }
}

/// Commit receipt, history and lock release together. A missing/replaced job row
/// cannot erase its occurrence evidence or have another invocation's lock cleared.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_manual_run(
    config: &Config,
    claim: &ManualClaim,
    execution_success: bool,
    delivery: Option<EffectOutcome>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: &str,
) -> Result<EffectOutcome> {
    let delivery_state = match delivery {
        None => "not_requested",
        Some(outcome) => crate::cron::scheduler::occurrence_delivery_state(Some(outcome)),
    };
    let effect = manual_effect(execution_success, delivery);
    let requires_review = effect != EffectOutcome::Confirmed;
    with_initialized_connection(config, |conn| {
        conn.execute_batch("PRAGMA synchronous=FULL")?;
        let tx = conn.unchecked_transaction()?;
        checkpoint_occurrence_inner(
            &tx,
            &claim.job_id,
            &claim.occurrence_id,
            if execution_success {
                "confirmed"
            } else {
                "possibly_applied"
            },
            delivery_state,
            None,
        )?;
        let owns_lock: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM cron_jobs WHERE id=?1 AND lock_owner=?2)",
            params![claim.job_id, claim.occurrence_id],
            |r| r.get(0),
        )?;
        if owns_lock {
            let output = truncate_cron_output(output);
            insert_run_and_prune(
                &tx,
                config,
                &claim.job_id,
                started_at,
                finished_at,
                status,
                Some(&output),
                (finished_at - started_at).num_milliseconds(),
            )?;
            tx.execute("UPDATE cron_jobs SET locked_at=NULL,lock_owner=NULL,last_run=?3,last_status=?4,last_output=?5,enabled=CASE WHEN ?6 THEN 0 ELSE enabled END WHERE id=?1 AND lock_owner=?2",
                params![claim.job_id,claim.occurrence_id,finished_at.to_rfc3339(),if requires_review { "uncertain" } else { status },output,requires_review])?;
        }
        tx.commit()?;
        Ok(effect)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture(tmp: &TempDir) -> (Config, CronJob) {
        let config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        let job = super::super::add_job(&config, "synthetic-agent", "* * * * *", "echo synthetic")
            .unwrap();
        (config, job)
    }

    fn state(config: &Config, claim: &ManualClaim) -> (String, String, Option<String>, String) {
        with_initialized_connection(config, |conn| {
            conn.query_row("SELECT execution_state,delivery_state,output,updated_at FROM cron_occurrences WHERE job_id=?1 AND scheduled_at=?2",
                params![claim.job_id,claim.occurrence_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(Into::into)
        }).unwrap()
    }

    #[test]
    fn keyed_manual_replay_returns_durable_evidence_without_reexecution() {
        let tmp = TempDir::new().unwrap();
        let (config, job) = fixture(&tmp);
        let claim = claim_manual_run_with_key(&config, &job, Some("private-fixture-key")).unwrap();
        assert!(!claim.id().contains("private-fixture-key"));
        let pending =
            claim_manual_run_with_key(&config, &job, Some("private-fixture-key")).unwrap_err();
        let pending = pending.downcast_ref::<DuplicateManualReceipt>().unwrap();
        assert_eq!(pending.effect, EffectOutcome::ReconciliationRequired);
        assert_eq!(pending.occurrence_id, claim.id());
        checkpoint_manual_execution(&config, &claim, true, "original receipt").unwrap();
        finish_manual_run(
            &config,
            &claim,
            true,
            None,
            Utc::now(),
            Utc::now(),
            "ok",
            "original receipt",
        )
        .unwrap();
        let before = state(&config, &claim);
        let confirmed =
            claim_manual_run_with_key(&config, &job, Some("private-fixture-key")).unwrap_err();
        let confirmed = confirmed.downcast_ref::<DuplicateManualReceipt>().unwrap();
        assert_eq!(confirmed.effect, EffectOutcome::Confirmed);
        assert_eq!(confirmed.output, "original receipt");
        assert_eq!(state(&config, &claim), before);
        assert_eq!(list_runs(&config, &job.id, 10).unwrap().len(), 1);
        let mut other_owner = job.clone();
        other_owner.agent_alias = "another-agent".into();
        assert_eq!(
            claim_manual_run_with_key(&config, &other_owner, Some("private-fixture-key"))
                .unwrap_err()
                .downcast_ref::<ManualAdmissionError>(),
            Some(&ManualAdmissionError::Changed)
        );
        with_initialized_connection(&config, |conn| {
            conn.execute(
                "UPDATE cron_jobs SET agent_alias='another-agent' WHERE id=?1",
                [&job.id],
            )?;
            Ok(())
        })
        .unwrap();
        let reassigned = get_job(&config, &job.id).unwrap();
        let new_owner =
            claim_manual_run_with_key(&config, &reassigned, Some("private-fixture-key")).unwrap();
        assert_ne!(new_owner.id(), claim.id());
        assert_eq!(
            state(&config, &new_owner).2,
            None,
            "a reassigned owner cannot retrieve the prior invocation output"
        );
    }

    #[tokio::test]
    async fn unreadable_keyed_receipt_is_uncertain_instead_of_non_delivery_evidence() {
        let tmp = TempDir::new().unwrap();
        let (config, job) = fixture(&tmp);
        let claim = claim_manual_run_with_key(&config, &job, Some("receipt-key")).unwrap();
        checkpoint_manual_execution(&config, &claim, true, "execution receipt").unwrap();
        with_initialized_connection(&config, |conn| {
            conn.execute(
                "UPDATE cron_occurrences SET output=?3 WHERE job_id=?1 AND scheduled_at=?2",
                params![job.id, claim.id(), "x".repeat(MAX_CRON_OUTPUT_BYTES + 1)],
            )?;
            Ok(())
        })
        .unwrap();
        let result = crate::cron::scheduler::run_manual_job_with_request_id(
            &config,
            &job,
            crate::cron::scheduler::CronDeliveryContext::RpcManual,
            &None,
            Some("receipt-key"),
        )
        .await;
        assert!(!result.success);
        assert_eq!(result.effect_outcome, EffectOutcome::ReconciliationRequired);
        assert_eq!(result.execution_outcome, EffectOutcome::PossiblyApplied);
        assert!(result.output.len() < 1024);
        assert!(list_runs(&config, &job.id, 10).unwrap().is_empty());
        assert_eq!(state(&config, &claim).0, "confirmed");
        assert!(!claim_job(&config, &job.id, Utc::now()).unwrap());
    }

    #[test]
    fn invalid_manual_request_ids_are_rejected_before_any_claim() {
        let tmp = TempDir::new().unwrap();
        let (config, job) = fixture(&tmp);
        for key in [
            String::new(),
            "space key".into(),
            "🦀".into(),
            "x".repeat(129),
        ] {
            assert_eq!(
                claim_manual_run_with_key(&config, &job, Some(&key))
                    .unwrap_err()
                    .downcast_ref::<ManualAdmissionError>(),
                Some(&ManualAdmissionError::InvalidRequestId)
            );
        }
        claim_manual_run_with_key(&config, &job, Some("valid-key")).unwrap();
    }

    #[test]
    fn manual_claim_is_atomic_and_excludes_scheduled_and_manual_competitors() {
        let tmp = TempDir::new().unwrap();
        let (config, job) = fixture(&tmp);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let barrier = barrier.clone();
                    let config = &config;
                    let job = &job;
                    scope.spawn(move || {
                        barrier.wait();
                        claim_manual_run(config, job)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        let claim = results.into_iter().find_map(Result::ok).unwrap();
        assert!(!claim_job(&config, &job.id, Utc::now()).unwrap());
        release_job(&config, &job.id).unwrap();
        assert!(
            claim_manual_run(&config, &job).is_err(),
            "legacy release cannot unlock a manual claim"
        );
        assert_eq!(state(&config, &claim).0, "running");
        checkpoint_manual_execution(&config, &claim, true, "synthetic receipt").unwrap();
        assert_eq!(
            finish_manual_run(
                &config,
                &claim,
                true,
                None,
                Utc::now(),
                Utc::now(),
                "ok",
                "synthetic receipt"
            )
            .unwrap(),
            EffectOutcome::Confirmed
        );
        let updated = get_job(&config, &job.id).unwrap();
        assert_eq!(
            updated.next_run, job.next_run,
            "manual execution must not consume the scheduled occurrence"
        );
        assert!(claim_job(&config, &job.id, Utc::now()).unwrap());
        assert!(claim_manual_run(&config, &updated).is_err());
        release_job(&config, &job.id).unwrap();
        let next = claim_manual_run(&config, &updated).unwrap();
        assert_ne!(claim.id(), next.id());
    }

    #[test]
    fn manual_claim_failure_rolls_back_lock_and_rejects_changed_approval_snapshot() {
        let tmp = TempDir::new().unwrap();
        let (config, job) = fixture(&tmp);
        with_initialized_connection(&config,|conn| { conn.execute_batch("CREATE TRIGGER fail_manual BEFORE INSERT ON cron_occurrences BEGIN SELECT RAISE(FAIL,'synthetic fault'); END;")?;Ok(()) }).unwrap();
        assert!(claim_manual_run(&config, &job).is_err());
        with_initialized_connection(&config, |conn| {
            assert!(conn.query_row(
                "SELECT locked_at IS NULL AND lock_owner IS NULL FROM cron_jobs WHERE id=?1",
                [&job.id],
                |r| r.get::<_, bool>(0)
            )?);
            conn.execute_batch("DROP TRIGGER fail_manual")?;
            conn.execute(
                "UPDATE cron_jobs SET command='echo changed' WHERE id=?1",
                [&job.id],
            )?;
            Ok(())
        })
        .unwrap();
        let error = claim_manual_run(&config, &job).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ManualAdmissionError>(),
            Some(&ManualAdmissionError::Changed)
        );
        claim_manual_run(&config, &get_job(&config, &job.id).unwrap()).unwrap();
    }

    #[test]
    fn manual_notification_and_history_commit_atomically_and_completion_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let (config, job) = fixture(&tmp);
        let claim = claim_manual_run(&config, &job).unwrap();
        checkpoint_manual_execution(&config, &claim, true, "execution receipt").unwrap();
        with_initialized_connection(&config,|conn| {conn.execute_batch("CREATE TRIGGER fail_history BEFORE INSERT ON cron_runs BEGIN SELECT RAISE(FAIL,'synthetic fault'); END;")?;Ok(())}).unwrap();
        let finish = || {
            finish_manual_run(
                &config,
                &claim,
                true,
                Some(EffectOutcome::Confirmed),
                Utc::now(),
                Utc::now(),
                "ok",
                "visible receipt",
            )
        };
        assert!(finish().is_err());
        assert_eq!(state(&config, &claim).1, "submitting");
        assert!(claim_manual_run(&config, &job).is_err());
        with_initialized_connection(&config, |conn| {
            conn.execute_batch("DROP TRIGGER fail_history")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(finish().unwrap(), EffectOutcome::Confirmed);
        let receipt = state(&config, &claim);
        assert_eq!(finish().unwrap(), EffectOutcome::Confirmed);
        assert_eq!(state(&config, &claim), receipt);
        assert_eq!(list_runs(&config, &job.id, 10).unwrap().len(), 1);
    }

    #[test]
    fn keyed_replay_preserves_notification_absence_and_all_typed_outcomes() {
        for delivery in [
            None,
            Some(EffectOutcome::NotStarted),
            Some(EffectOutcome::Confirmed),
            Some(EffectOutcome::ConfirmedFailed),
            Some(EffectOutcome::PartiallyApplied),
            Some(EffectOutcome::PossiblyApplied),
            Some(EffectOutcome::ReconciliationRequired),
        ] {
            let tmp = TempDir::new().unwrap();
            let (config, job) = fixture(&tmp);
            let claim = claim_manual_run_with_key(&config, &job, Some("receipt-key")).unwrap();
            checkpoint_manual_execution(&config, &claim, true, "execution receipt").unwrap();
            let effect = finish_manual_run(
                &config,
                &claim,
                true,
                delivery,
                Utc::now(),
                Utc::now(),
                "ok",
                "execution receipt",
            )
            .unwrap();
            let replay = claim_manual_run_with_key(&config, &job, Some("receipt-key")).unwrap_err();
            let replay = replay.downcast_ref::<DuplicateManualReceipt>().unwrap();
            assert_eq!(replay.delivery, delivery);
            assert_eq!(replay.effect, effect);
            assert_eq!(replay.execution, EffectOutcome::Confirmed);
            assert_eq!(list_runs(&config, &job.id, 10).unwrap().len(), 1);
        }
    }

    #[test]
    fn manual_uncertainty_is_quarantined_and_reopen_never_replays() {
        for (success, delivery, effect) in [
            (false, None, EffectOutcome::ReconciliationRequired),
            (
                true,
                Some(EffectOutcome::PossiblyApplied),
                EffectOutcome::ReconciliationRequired,
            ),
            (
                true,
                Some(EffectOutcome::ConfirmedFailed),
                EffectOutcome::PartiallyApplied,
            ),
        ] {
            let tmp = TempDir::new().unwrap();
            let (config, job) = fixture(&tmp);
            let claim = claim_manual_run(&config, &job).unwrap();
            checkpoint_manual_execution(&config, &claim, success, "keep receipt").unwrap();
            assert_eq!(
                finish_manual_run(
                    &config,
                    &claim,
                    success,
                    delivery,
                    Utc::now(),
                    Utc::now(),
                    "error",
                    "keep receipt"
                )
                .unwrap(),
                effect
            );
            let updated = get_job(&config, &job.id).unwrap();
            assert!(!updated.enabled);
            assert_eq!(updated.last_status.as_deref(), Some("uncertain"));
            let before = state(&config, &claim);
            assert_eq!(clear_stale_locks(&config).unwrap(), 0);
            assert_eq!(state(&config, &claim), before);
            assert_eq!(
                claim_manual_run(&config, &updated)
                    .unwrap_err()
                    .downcast_ref::<ManualAdmissionError>(),
                Some(&ManualAdmissionError::Quarantined)
            );
            assert!(!claim_job(&config, &job.id, Utc::now()).unwrap());
        }
    }

    #[test]
    fn manual_receipts_survive_job_deletion_without_releasing_replacement_lock() {
        let tmp = TempDir::new().unwrap();
        let (config, job) = fixture(&tmp);
        let claim = claim_manual_run(&config, &job).unwrap();
        checkpoint_manual_execution(&config, &claim, true, "execution receipt").unwrap();
        remove_job(&config, &job.id).unwrap();
        // Confirm the original invocation after deletion, then prove that an
        // idempotent late completion cannot unlock a newer manual invocation.
        finish_manual_run(
            &config,
            &claim,
            true,
            Some(EffectOutcome::Confirmed),
            Utc::now(),
            Utc::now(),
            "ok",
            "receipt",
        )
        .unwrap();

        let replacement =
            super::super::add_job(&config, "synthetic-agent", "* * * * *", "echo replacement")
                .unwrap();
        with_initialized_connection(&config, |conn| {
            conn.execute(
                "UPDATE cron_jobs SET id=?2 WHERE id=?1",
                params![replacement.id, job.id],
            )?;
            Ok(())
        })
        .unwrap();
        let replacement_claim =
            claim_manual_run(&config, &get_job(&config, &job.id).unwrap()).unwrap();

        assert_eq!(
            finish_manual_run(
                &config,
                &claim,
                true,
                Some(EffectOutcome::Confirmed),
                Utc::now(),
                Utc::now(),
                "ok",
                "receipt"
            )
            .unwrap(),
            EffectOutcome::Confirmed
        );
        assert_eq!(state(&config, &claim).1, "confirmed");
        assert_eq!(state(&config, &replacement_claim).0, "running");
        assert!(!claim_job(&config, &job.id, Utc::now()).unwrap());

        assert_eq!(clear_stale_locks(&config).unwrap(), 1);
        assert_eq!(state(&config, &claim).1, "confirmed");
    }

    #[test]
    fn manual_running_and_submitting_are_recovered_without_erasing_execution_evidence() {
        for submitted in [false, true] {
            let tmp = TempDir::new().unwrap();
            let (config, job) = fixture(&tmp);
            let claim = claim_manual_run(&config, &job).unwrap();
            if submitted {
                checkpoint_manual_execution(&config, &claim, true, "accepted execution").unwrap();
            }
            assert_eq!(clear_stale_locks(&config).unwrap(), 1);
            let receipt = state(&config, &claim);
            assert_eq!(
                receipt.0,
                if submitted {
                    "confirmed"
                } else {
                    "possibly_applied"
                }
            );
            assert_eq!(
                receipt.1,
                if submitted {
                    "reconciliation_required"
                } else {
                    "not_started"
                }
            );
            assert_eq!(clear_stale_locks(&config).unwrap(), 0);
            assert_eq!(state(&config, &claim), receipt);
            assert!(checkpoint_manual_execution(&config, &claim, true, "new execution").is_err());
            assert!(claim_manual_run(&config, &get_job(&config, &job.id).unwrap()).is_err());
        }
    }

    #[test]
    fn manual_lock_owner_migration_preserves_existing_jobs_and_occurrences() {
        let tmp = TempDir::new().unwrap();
        let (config, job) = fixture(&tmp);
        with_initialized_connection(&config, |conn| {
            conn.execute_batch("ALTER TABLE cron_jobs DROP COLUMN lock_owner")?;
            Ok(())
        })
        .unwrap();
        let claim = claim_manual_run(&config, &job).unwrap();
        assert_eq!(get_job(&config, &job.id).unwrap().next_run, job.next_run);
        assert_eq!(state(&config, &claim).0, "running");
    }
}
