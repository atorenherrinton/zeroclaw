use crate::cron::store::{
    DuplicateManualReceipt, ManualAdmissionError, RunCompletionAction, checkpoint_manual_execution,
    claim_manual_run_with_key, finish_manual_run, persist_run_completion_state, persist_run_result,
    reconcile_missed_run,
};
use crate::cron::{
    CronJob, DeliveryConfig, JobType, Schedule, SessionTarget, all_overdue_jobs, claim_job,
    clear_stale_locks, due_jobs, next_run_for_schedule, release_job, skip_missed_run,
    sync_declarative_jobs,
};
use crate::security::SecurityPolicy;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use std::process::Stdio;
use std::sync::Arc;
use tokio::time::{self, Duration};
use tokio_util::sync::CancellationToken;
use zeroclaw_api::delivery::{DeliveryFailure, DeliverySummary, EffectOutcome};
use zeroclaw_api::runtime_traits::RuntimeAdapter;
use zeroclaw_config::schema::Config;
use zeroclaw_config::schema::{
    CronJobDecl, CronMissedRunPolicy, CronScheduleDecl, CronShellOutputFormat,
};
use zeroclaw_log::Instrument;

const MIN_POLL_SECONDS: u64 = 5;
const SHELL_JOB_TIMEOUT_SECS: u64 = 120;
const COMPLETION_CHECK_TIMEOUT_SECS: u64 = 30;
const SCHEDULER_COMPONENT: &str = "scheduler";
const CRON_AGENT_DEFAULT_EXCLUDED_TOOLS: &[&str] = &[
    "cron_add",
    "cron_update",
    "cron_remove",
    "cron_run",
    "schedule",
];

/// Type alias for the optional broadcast sender used to push cron results
/// to connected dashboard/SSE clients.
pub type EventBroadcast = Option<tokio::sync::broadcast::Sender<serde_json::Value>>;

#[must_use]
pub fn is_no_reply_sentinel(output: &str) -> bool {
    let trimmed = output.trim();
    if trimmed.eq_ignore_ascii_case("NO_REPLY") {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    // Legacy form (`NO_REPLY: ...`) is documented as "treated as INFO".
    if lower.starts_with("no_reply:") {
        return true;
    }
    // Kinded form (`NO_REPLY[KIND]: ...`): only the informational kind is a
    // "nothing to report" sentinel. REFUSE / FAIL (and any other/unknown kind)
    // carry operator-visible meaning and must be delivered, not suppressed.
    if let Some(rest) = lower.strip_prefix("no_reply[") {
        if let Some((kind, _)) = rest.split_once(']') {
            return kind.trim() == "info";
        }
        // Malformed `NO_REPLY[...` with no closing bracket: not a clean
        // sentinel — deliver it rather than guess.
        return false;
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceDecision {
    /// Send the output to the configured channel.
    Deliver,
    /// Suppress delivery: the output is a quiet `NO_REPLY` sentinel.
    SuppressNoReply,
}

impl AnnounceDecision {
    /// True when the announcement should actually be sent to the channel.
    #[must_use]
    pub fn should_deliver(self) -> bool {
        matches!(self, AnnounceDecision::Deliver)
    }
}

/// Decide whether an announce-mode output should be delivered or suppressed.
/// Suppresses only the *quiet* `NO_REPLY` forms (see [`is_no_reply_sentinel`]);
/// failure/refusal kinds and all real content are delivered.
#[must_use]
pub fn announce_delivery_decision(output: &str) -> AnnounceDecision {
    if is_no_reply_sentinel(output) {
        AnnounceDecision::SuppressNoReply
    } else {
        AnnounceDecision::Deliver
    }
}

#[derive(Clone, Copy)]
pub enum CronDeliveryContext {
    Scheduled,
    ToolManual,
    GatewayManual,
    RpcManual,
}

impl CronDeliveryContext {
    fn failure_message(self, best_effort: bool) -> &'static str {
        match (self, best_effort) {
            (Self::Scheduled, true) => "Cron delivery failed (best_effort)",
            (Self::Scheduled, false) => "Cron delivery failed",
            (Self::ToolManual, true) => "cron_run delivery failed (best_effort)",
            (Self::ToolManual, false) => "cron_run delivery failed",
            (Self::GatewayManual, true) => "manual cron trigger delivery failed (best_effort)",
            (Self::GatewayManual, false) => "manual cron trigger delivery failed",
            (Self::RpcManual, true) => "RPC cron trigger delivery failed (best_effort)",
            (Self::RpcManual, false) => "RPC cron trigger delivery failed",
        }
    }
}

pub struct ManualCronRunResult {
    pub duplicate: bool,
    pub occurrence_id: Option<String>,
    pub effect_outcome: EffectOutcome,
    pub execution_outcome: EffectOutcome,
    pub delivery_outcome: Option<EffectOutcome>,
    pub job_id: String,
    pub success: bool,
    pub status: String,
    pub output: String,
    pub duration_ms: i64,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

pub struct CronDeliveryOutcome {
    /// Notification evidence is independent of the legacy combined job status.
    /// None means delivery was deliberately suppressed or not configured.
    pub delivery_outcome: Option<EffectOutcome>,
    pub success: bool,
    pub status: String,
    pub output: String,
}

pub async fn deliver_and_classify_run_result(
    config: &Config,
    job: &CronJob,
    success: bool,
    output: String,
    context: CronDeliveryContext,
) -> CronDeliveryOutcome {
    deliver_and_classify_with_handler(config, job, success, output, context, DELIVERY_FN.get())
        .await
}

async fn deliver_and_classify_with_handler(
    config: &Config,
    job: &CronJob,
    mut success: bool,
    mut output: String,
    context: CronDeliveryContext,
    handler: Option<&DeliveryFn>,
) -> CronDeliveryOutcome {
    let mut status = if success { "ok" } else { "error" }.to_string();

    let delivery = deliver_if_configured(config, job, &output, handler).await;
    let delivery_outcome = match &delivery {
        Ok(outcome) => *outcome,
        Err(error) => Some(error.downcast_ref::<DeliveryFailure>().map_or(
            EffectOutcome::PossiblyApplied,
            |failure| {
                if failure.confirmed_chunks > 0 && failure.outcome == EffectOutcome::ConfirmedFailed
                {
                    EffectOutcome::PartiallyApplied
                } else {
                    failure.outcome
                }
            },
        )),
    };
    if let Err(e) = delivery {
        // Cron add-time accepts dangling delivery refs (the job's channel
        // may not be provisioned yet); the loudly-logged warn here is
        // the scheduler-side half of that contract. Manual trigger paths
        // share this classifier so status history cannot drift again.
        let channel = job.delivery.channel.as_deref().unwrap_or("");
        let target = job.delivery.to.as_deref().unwrap_or("");
        let delivery_error = e.to_string();

        if job.delivery.best_effort {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "job_id": job.id,
                        "agent_alias": job.agent_alias,
                        "channel": channel,
                        "target": target,
                        "error": delivery_error
                    })),
                context.failure_message(true)
            );
            if success {
                status = "degraded".to_string();
            }
        } else {
            success = false;
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "job_id": job.id,
                        "agent_alias": job.agent_alias,
                        "channel": channel,
                        "target": target,
                        "error": delivery_error
                    })),
                context.failure_message(false)
            );
            status = "error".to_string();
        }

        if output.trim().is_empty() {
            output = format!("delivery failed: {delivery_error}");
        } else {
            output.push_str("\n\ndelivery failed: ");
            output.push_str(&delivery_error);
        }
    }

    CronDeliveryOutcome {
        delivery_outcome,
        success,
        status,
        output,
    }
}

pub async fn run_manual_job(
    config: &Config,
    job: &CronJob,
    context: CronDeliveryContext,
    event_tx: &EventBroadcast,
) -> ManualCronRunResult {
    run_manual_job_with_request_id(config, job, context, event_tx, None).await
}

pub async fn run_manual_job_with_request_id(
    config: &Config,
    job: &CronJob,
    context: CronDeliveryContext,
    event_tx: &EventBroadcast,
    request_id: Option<&str>,
) -> ManualCronRunResult {
    run_manual_job_inner(
        config,
        job,
        context,
        event_tx,
        None,
        false,
        DELIVERY_FN.get(),
        request_id,
    )
    .await
}

pub(crate) async fn run_manual_job_with_runtime(
    config: &Config,
    job: &CronJob,
    context: CronDeliveryContext,
    event_tx: &EventBroadcast,
    runtime: &dyn RuntimeAdapter,
    approved: bool,
    request_id: Option<&str>,
) -> ManualCronRunResult {
    run_manual_job_inner(
        config,
        job,
        context,
        event_tx,
        Some(runtime),
        approved,
        DELIVERY_FN.get(),
        request_id,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_manual_job_inner(
    config: &Config,
    job: &CronJob,
    context: CronDeliveryContext,
    event_tx: &EventBroadcast,
    runtime: Option<&dyn RuntimeAdapter>,
    approved: bool,
    handler: Option<&DeliveryFn>,
    request_id: Option<&str>,
) -> ManualCronRunResult {
    use crate::i18n::get_required_cli_string;
    let started_at = Utc::now();
    let mut result = ManualCronRunResult {
        duplicate: false,
        occurrence_id: None,
        effect_outcome: EffectOutcome::NotStarted,
        execution_outcome: EffectOutcome::NotStarted,
        delivery_outcome: None,
        job_id: job.id.clone(),
        success: false,
        status: "error".into(),
        output: String::new(),
        duration_ms: 0,
        started_at,
        finished_at: started_at,
    };
    let admission_config = config.clone();
    let admission_job = job.clone();
    let keyed_request = request_id.is_some();
    let request_id = request_id.map(str::to_owned);
    let admission = tokio::task::spawn_blocking(move || {
        claim_manual_run_with_key(&admission_config, &admission_job, request_id.as_deref())
    })
    .await;
    let claim = match admission {
        Ok(Ok(claim)) => claim,
        failure => {
            if let Ok(Err(error)) = &failure
                && let Some(existing) = error.downcast_ref::<DuplicateManualReceipt>()
            {
                result.duplicate = true;
                result.occurrence_id = Some(existing.occurrence_id.clone());
                result.effect_outcome = existing.effect;
                result.execution_outcome = existing.execution;
                result.delivery_outcome = existing.delivery;
                result.success = existing.effect == EffectOutcome::Confirmed;
                result.status = if result.success { "ok" } else { "uncertain" }.into();
                result.output = existing.output.clone();
                result.finished_at = Utc::now();
                return result;
            }
            let reason = match &failure {
                Ok(Err(error)) => error.downcast_ref::<ManualAdmissionError>().copied(),
                _ => None,
            };
            let key = match reason {
                Some(ManualAdmissionError::Quarantined) => {
                    result.status = "uncertain".into();
                    result.effect_outcome = EffectOutcome::ReconciliationRequired;
                    "cron-manual-quarantined"
                }
                Some(ManualAdmissionError::InFlight) => "cron-manual-in-flight",
                Some(ManualAdmissionError::Changed) => "cron-manual-changed",
                Some(ManualAdmissionError::InvalidRequestId) => "cron-manual-invalid-request-id",
                None if keyed_request => {
                    // Failure to retrieve a prior keyed receipt is not evidence
                    // that the original invocation never executed.
                    result.status = "uncertain".into();
                    result.effect_outcome = EffectOutcome::ReconciliationRequired;
                    result.execution_outcome = EffectOutcome::PossiblyApplied;
                    "cron-manual-receipt-unavailable"
                }
                None => "cron-manual-storage-unavailable",
            };
            result.output = get_required_cli_string(key);
            result.finished_at = Utc::now();
            return result;
        }
    };
    result.occurrence_id = Some(claim.id().to_owned());
    let (success, output) = execute_job_now_with_runtime(config, job, runtime, approved).await;
    result.execution_outcome = if success {
        EffectOutcome::Confirmed
    } else {
        EffectOutcome::PossiblyApplied
    };
    result.finished_at = Utc::now();
    result.duration_ms = (result.finished_at - started_at).num_milliseconds();
    result.output = output;

    // A dropped future leaves its durable running/submitting claim locked. It
    // cannot be admitted again; startup recovery quarantines the uncertain work.
    let checkpoint_config = config.clone();
    let checkpoint_claim = claim.clone();
    let checkpoint_output = result.output[..result
        .output
        .floor_char_boundary(super::store::MAX_CRON_OUTPUT_BYTES)]
        .to_owned();
    if !matches!(
        tokio::task::spawn_blocking(move || checkpoint_manual_execution(
            &checkpoint_config,
            &checkpoint_claim,
            success,
            &checkpoint_output
        ))
        .await,
        Ok(Ok(()))
    ) {
        result.effect_outcome = EffectOutcome::ReconciliationRequired;
        result.status = "uncertain".into();
        result.output.push('\n');
        result
            .output
            .push_str(&get_required_cli_string("cron-manual-checkpoint-failed"));
        return result;
    }

    let outcome =
        deliver_and_classify_with_handler(config, job, success, result.output, context, handler)
            .await;
    result.delivery_outcome = outcome.delivery_outcome;
    result.success = outcome.success;
    result.status = outcome.status;
    result.output = outcome.output;
    let completion_config = config.clone();
    let completion_claim = claim;
    let completion_output = result.output[..result
        .output
        .floor_char_boundary(super::store::MAX_CRON_OUTPUT_BYTES)]
        .to_owned();
    let completion_status = result.status.clone();
    let delivery = result.delivery_outcome;
    let finished_at = result.finished_at;
    match tokio::task::spawn_blocking(move || {
        finish_manual_run(
            &completion_config,
            &completion_claim,
            success,
            delivery,
            started_at,
            finished_at,
            &completion_status,
            &completion_output,
        )
    })
    .await
    {
        Ok(Ok(effect)) => result.effect_outcome = effect,
        _ => {
            result.success = false;
            result.effect_outcome = EffectOutcome::ReconciliationRequired;
            result.status = "uncertain".into();
            result.output.push('\n');
            result
                .output
                .push_str(&get_required_cli_string("cron-manual-checkpoint-failed"));
        }
    }
    if let Some(tx) = event_tx {
        let _ = tx.send(serde_json::json!({
            "type": "cron_result", "job_id": job.id, "success": result.success,
            "output": result.output, "manual": true, "timestamp": finished_at.to_rfc3339(),
            "occurrence_id": result.occurrence_id, "effect_outcome": result.effect_outcome,
            "execution_outcome": result.execution_outcome, "delivery_outcome": result.delivery_outcome,
        }));
    }
    result
}

pub async fn run(
    config: Config,
    event_tx: EventBroadcast,
    cancel: CancellationToken,
) -> Result<()> {
    let poll_secs = config.reliability.scheduler_poll_secs.max(MIN_POLL_SECONDS);
    let mut interval = time::interval(Duration::from_secs(poll_secs));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    crate::health::mark_component_ok(SCHEDULER_COMPONENT);

    // ── Declarative job sync: reconcile config-defined jobs with the DB.
    let mut jobs_with_builtin = config.cron.clone();
    if let Some(ref schedule_cron) = config.backup.schedule_cron {
        let backup_job = CronJobDecl {
            name: Some("Scheduled backup".to_string()),
            job_type: "shell".to_string(),
            schedule: CronScheduleDecl::Cron {
                expr: schedule_cron.clone(),
                tz: config.backup.schedule_timezone.clone(),
            },
            command: Some("backup create".to_string()),
            prompt: None,
            enabled: true,
            model: None,
            allowed_tools: None,
            uses_memory: true,
            timeout_secs: None,
            missed_run_policy: None,
            completion_check: None,
            session_target: None,
            delivery: None,
            shell_output_format: CronShellOutputFormat::default(),
        };
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"schedule": schedule_cron})),
            "Synthesizing builtin backup cron job from config.backup.schedule_cron"
        );
        jobs_with_builtin.insert("__builtin_backup".to_string(), backup_job);
    }

    match sync_declarative_jobs(&config, &jobs_with_builtin) {
        Ok(()) => {
            if !jobs_with_builtin.is_empty() {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"count": jobs_with_builtin.len()})),
                    "Synced declarative cron jobs from config"
                );
            }
        }
        Err(e) => ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
            "Failed to sync declarative cron jobs"
        ),
    }

    // Checkpoint interrupted work before admitting any catch-up work. Failure
    // must stop startup; continuing could replay an uncheckpointed occurrence.
    let startup_config = config.clone();
    let jobs = tokio::task::spawn_blocking(move || {
        clear_stale_locks(&startup_config).context("scheduler startup recovery failed")?;
        prepare_startup_jobs(&startup_config, Utc::now())
    })
    .await??;
    let jobs = claim_due_jobs(&config, jobs);
    process_due_jobs(&config, jobs, SCHEDULER_COMPONENT, &event_tx).await;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Keep scheduler liveness fresh even when there are no due jobs.
                crate::health::mark_component_ok(SCHEDULER_COMPONENT);

                let jobs = match due_jobs(&config, Utc::now()) {
                    Ok(jobs) => jobs,
                    Err(e) => {
                        crate::health::mark_component_error(SCHEDULER_COMPONENT, e.to_string());
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "Scheduler query failed"
                        );
                        continue;
                    }
                };

                let jobs = claim_due_jobs(&config, jobs);
                process_due_jobs(&config, jobs, SCHEDULER_COMPONENT, &event_tx).await;
            }
            _ = cancel.cancelled() => {
                crate::health::mark_component_ok(SCHEDULER_COMPONENT);
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "Cron scheduler shutting down via cancellation token"
                );
                return Ok(());
            }
        }
    }
}

fn resolve_owning_agent<'a>(config: &'a Config, job: &CronJob) -> Option<&'a str> {
    if !job.agent_alias.is_empty()
        && let Some((alias, _)) = config
            .agents
            .iter()
            .find(|(alias, _)| alias.as_str() == job.agent_alias)
    {
        return Some(alias.as_str());
    }
    config.agent_for_cron_job(&job.id)
}

/// Resolve canonical config only for its owning declarative row. An imperative
/// job with a colliding id must not inherit a declaration's policy.
fn missed_run_policy(config: &Config, job: &CronJob) -> CronMissedRunPolicy {
    if job.source == "declarative"
        && let Some(policy) = config
            .cron
            .get(&job.id)
            .and_then(|decl| decl.missed_run_policy)
    {
        return policy;
    }
    if job.source != "declarative"
        && let Some(policy) = job.missed_run_policy
    {
        return policy;
    }
    if config.scheduler.catch_up_on_startup {
        CronMissedRunPolicy::CatchUpOnce
    } else {
        CronMissedRunPolicy::Skip
    }
}

/// Apply durable dispositions before executing any startup work. Fetch all
/// overdue jobs, regardless of the normal polling batch size. Propagate errors
/// so a failed skip/quarantine cannot fall through to the ordinary polling loop.
fn prepare_startup_jobs(config: &Config, now: DateTime<Utc>) -> Result<Vec<CronJob>> {
    let mut ready = Vec::new();
    let mut skipped = 0;
    let mut quarantined = 0;
    for job in all_overdue_jobs(config, now)? {
        match missed_run_policy(config, &job) {
            CronMissedRunPolicy::CatchUpOnce => ready.push(job),
            CronMissedRunPolicy::Skip => {
                skip_missed_run(config, &job, now)
                    .with_context(|| format!("startup skip failed for cron job {}", job.id))?;
                skipped += 1;
            }
            CronMissedRunPolicy::Reconcile => {
                reconcile_missed_run(config, &job, now).with_context(|| {
                    format!("startup quarantine failed for cron job {}", job.id)
                })?;
                quarantined += 1;
            }
        }
    }
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_attrs(::serde_json::json!({"catch_up_count": ready.len(), "skipped": skipped, "quarantined": quarantined})),
        "Scheduler startup policies checkpointed"
    );
    Ok(ready)
}

pub async fn execute_job_now(config: &Config, job: &CronJob) -> (bool, String) {
    execute_job_now_with_runtime(config, job, None, false).await
}

async fn execute_job_now_with_runtime(
    config: &Config,
    job: &CronJob,
    runtime: Option<&dyn RuntimeAdapter>,
    approved: bool,
) -> (bool, String) {
    // Reject orphaned declarative jobs: a declarative row whose canonical
    // config declaration has been removed must not execute through any
    // path (automatic polling or manual trigger).
    if job.source == "declarative" && !super::store::is_valid_declarative_owner(config, &job.id) {
        return (
            false,
            format!(
                "cron job {id:?} is an orphaned declarative entry \
                 (source = \"declarative\" but absent from live config); \
                 cannot execute",
                id = job.id
            ),
        );
    }
    use zeroclaw_log::Instrument;
    let Some(agent_alias) = resolve_owning_agent(config, job) else {
        return (
            false,
            format!(
                "cron job {id:?} has no owning agent; add the alias to an [agents.<x>].cron_jobs list",
                id = job.id
            ),
        );
    };
    let agent_alias = agent_alias.to_string();
    let security = match SecurityPolicy::for_agent(config, &agent_alias) {
        Ok(s) => s,
        Err(e) => return (false, format!("agent {agent_alias} risk profile: {e}")),
    };
    let span = zeroclaw_log::attribution_span!(job);
    Box::pin(execute_job_with_retry(
        config,
        &security,
        &agent_alias,
        job,
        runtime,
        approved,
    ))
    .instrument(span)
    .await
}

fn cron_agent_run_policy(base: &SecurityPolicy, job: &CronJob) -> SecurityPolicy {
    let mut policy = base.clone();
    if !matches!(job.job_type, JobType::Agent) || job.allowed_tools.is_some() {
        return policy;
    }

    let excluded = policy.excluded_tools.get_or_insert_with(Vec::new);
    for tool in CRON_AGENT_DEFAULT_EXCLUDED_TOOLS {
        if !excluded.iter().any(|existing| existing == tool) {
            excluded.push((*tool).to_string());
        }
    }
    policy
}

fn cron_agent_session_path(target: &SessionTarget, run_session_id: &str) -> std::path::PathBuf {
    match target {
        SessionTarget::Main => std::path::PathBuf::from("main"),
        SessionTarget::Isolated => std::path::PathBuf::from(format!("cron-{run_session_id}")),
    }
}

fn cron_agent_timeout(config: &Config, job: &CronJob) -> Result<Option<Duration>> {
    // A same-ID imperative row must not borrow a declarative job's policy.
    if job.source != "declarative" || job.job_type != JobType::Agent {
        return Ok(None);
    }
    config
        .cron
        .get(&job.id)
        .map(CronJobDecl::validated_timeout_secs)
        .transpose()
        .map(|seconds| seconds.flatten().map(Duration::from_secs))
}

async fn await_cron_agent_run<F>(config: &Config, job: &CronJob, run: F) -> Result<String>
where
    F: std::future::Future<Output = Result<String>>,
{
    match cron_agent_timeout(config, job)? {
        Some(limit) => time::timeout(limit, run).await.map_err(|_| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "error_key": "cron.agent_attempt_timeout",
                        "job_id": job.id,
                        "timeout_secs": limit.as_secs(),
                    })),
                "Cron agent attempt timed out"
            );
            anyhow::Error::msg(crate::i18n::get_required_cli_string_with_args(
                "cli-cron-agent-attempt-timeout",
                &[("seconds", &limit.as_secs().to_string())],
            ))
        })?,
        None => run.await,
    }
}

async fn execute_job_with_retry(
    config: &Config,
    security: &SecurityPolicy,
    agent_alias: &str,
    job: &CronJob,
    runtime: Option<&dyn RuntimeAdapter>,
    approved: bool,
) -> (bool, String) {
    let owned_runtime = if matches!(job.job_type, JobType::Shell) && runtime.is_none() {
        match crate::platform::create_runtime(&config.runtime) {
            Ok(runtime) => Some(runtime),
            Err(error) => return (false, format!("shell setup error: {error}")),
        }
    } else {
        None
    };
    let runtime = runtime.or(owned_runtime.as_deref());

    let mut last_output = String::new();
    // A failed agent/shell attempt may already have changed external state.
    // Only an explicit, enforced allowlist of known stateless reads permits
    // retry. None means unrestricted, not read-only. Never inspect output prose
    // or trust a connector's self-description to authorize replay.
    let read_only = matches!(job.job_type, JobType::Agent)
        && job.allowed_tools.as_ref().is_some_and(|tools| {
            tools
                .iter()
                .all(|name| crate::agent::tool_execution::is_stateless_read_tool(name))
        });
    let retries = if read_only {
        config.reliability.scheduler_retries
    } else {
        0
    };
    let mut backoff_ms = config.reliability.provider_backoff_ms.max(200);

    for attempt in 0..=retries {
        let (success, output) = match job.job_type {
            JobType::Shell => {
                let Some(runtime) = runtime else {
                    return (
                        false,
                        "shell setup error: runtime missing for shell cron job".to_string(),
                    );
                };
                run_job_command_with_runtime(config, runtime, security, job, approved).await
            }
            JobType::Agent => Box::pin(run_agent_job(config, security, agent_alias, job)).await,
        };
        last_output = output;

        if success {
            return (true, last_output);
        }

        if last_output.starts_with("blocked by security policy:") {
            // Deterministic policy violations are not retryable.
            return (false, last_output);
        }

        if attempt < retries {
            let jitter_ms = u64::from(Utc::now().timestamp_subsec_millis() % 250);
            time::sleep(Duration::from_millis(backoff_ms + jitter_ms)).await;
            backoff_ms = (backoff_ms.saturating_mul(2)).min(30_000);
        }
    }

    (false, last_output)
}

fn claim_due_jobs(config: &Config, jobs: Vec<CronJob>) -> Vec<CronJob> {
    jobs.into_iter()
        .filter(|job| match claim_job(config, &job.id, Utc::now()) {
            Ok(true) => true,
            Ok(false) => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"job_id": job.id})),
                    "Cron job already in flight; skipping duplicate launch"
                );
                false
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(
                            ::serde_json::json!({"job_id": job.id, "error": format!("{}", e)})
                        ),
                    "Cron job: failed to claim in-flight lock; skipping launch"
                );
                false
            }
        })
        .collect()
}

async fn process_due_jobs(
    config: &Config,
    jobs: Vec<CronJob>,
    component: &str,
    event_tx: &EventBroadcast,
) {
    // Refresh scheduler health on every successful poll cycle, including idle cycles.
    crate::health::mark_component_ok(component);

    let max_concurrent = config.scheduler.max_concurrent.max(1);
    let mut in_flight = stream::iter(jobs.into_iter().filter_map(|job| {
        let Some(agent_alias) = resolve_owning_agent(config, &job) else {
            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"job_id": job.id})), "Cron job has no owning agent; add the alias to an [agents.<x>].cron_jobs list");
            let _ = release_job(config, &job.id);
            return None;
        };
        let agent_alias = agent_alias.to_owned();
        let security = match SecurityPolicy::for_agent(config, &agent_alias) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"job_id": job.id, "agent": agent_alias, "error": format!("{}", e)})), "Cron job: failed to build SecurityPolicy for owning agent");
                let _ = release_job(config, &job.id);
                return None;
            }
        };
        let config = config.clone();
        let component = component.to_owned();
        Some(async move {
            Box::pin(execute_and_persist_job(
                &config,
                security.as_ref(),
                &agent_alias,
                &job,
                &component,
            ))
            .await
        })
    }))
    .buffer_unordered(max_concurrent);

    while let Some((job_id, success, output)) = in_flight.next().await {
        if !success {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"job_id": job_id, "output": output})),
                "Scheduler job '' failed: "
            );
        }
        // Broadcast cron result to dashboard/SSE clients.
        if let Some(tx) = event_tx {
            let _ = tx.send(serde_json::json!({
                "type": "cron_result",
                "job_id": job_id,
                "success": success,
                "output": output,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }));
        }
    }
}

async fn execute_and_persist_job(
    config: &Config,
    security: &SecurityPolicy,
    agent_alias: &str,
    job: &CronJob,
    component: &str,
) -> (String, bool, String) {
    crate::health::mark_component_ok(component);
    warn_if_high_frequency_agent_job(job);

    let started_at = Utc::now();
    if let Err(error) =
        super::store::checkpoint_occurrence(config, job, "running", "not_started", None)
    {
        return (
            job.id.clone(),
            false,
            format!("occurrence checkpoint failed before execution: {error}"),
        );
    }
    let span = zeroclaw_log::attribution_span!(job);
    let (success, output) = Box::pin(execute_job_with_retry(
        config,
        security,
        agent_alias,
        job,
        None,
        false,
    ))
    .instrument(span)
    .await;
    let finished_at = Utc::now();
    let execution = if success {
        "confirmed"
    } else {
        "possibly_applied"
    };
    // Save execution evidence before attempting a notification. A crash after
    // submission must never restart the job just to regenerate that notification.
    if let Err(error) =
        super::store::checkpoint_occurrence(config, job, execution, "submitting", Some(&output))
    {
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_attrs(serde_json::json!({"error":error.to_string()})),
            "Refusing notification without occurrence checkpoint"
        );
        return (job.id.clone(), false, output);
    }
    let outcome = Box::pin(persist_job_result(
        config,
        job,
        success,
        &output,
        started_at,
        finished_at,
    ))
    .await;

    // Never infer delivery from the combined execution/best-effort success bit.
    // Adapters without positive receipts remain possibly applied even on Ok(()).
    if let Err(error) = super::store::checkpoint_occurrence(
        config,
        job,
        execution,
        occurrence_delivery_state(outcome.delivery_outcome),
        None,
    ) {
        ::zeroclaw_log::record!(
            ERROR,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                .with_attrs(serde_json::json!({"error":error.to_string()})),
            "Occurrence delivery checkpoint failed"
        );
        return (job.id.clone(), false, output);
    }

    // Release the in-flight lock claimed during selection (`claim_due_jobs`) now
    // that the run (and its reschedule/disable/delete in `persist_job_result`) is
    // done. A deleted one-shot row simply releases nothing. If this fails the lock
    // is recovered by `clear_stale_locks` at the next startup
    if let Err(e) = release_job(config, &job.id) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"job_id": job.id, "error": format!("{}", e)})),
            "Cron job: failed to release in-flight lock after run"
        );
    }

    (job.id.clone(), outcome.success, output)
}

async fn run_agent_job(
    config: &Config,
    security: &SecurityPolicy,
    agent_alias: &str,
    job: &CronJob,
) -> (bool, String) {
    if !security.can_act() {
        return (
            false,
            "blocked by security policy: autonomy is read-only".to_string(),
        );
    }

    if security.is_rate_limited() {
        return (
            false,
            "blocked by security policy: rate limit exceeded".to_string(),
        );
    }

    if !security.record_action() {
        return (
            false,
            "blocked by security policy: action budget exhausted".to_string(),
        );
    }
    let name = job.name.clone().unwrap_or_else(|| "cron-job".to_string());
    let prompt = job.prompt.clone().unwrap_or_default();

    let prefixed_prompt = format!("[cron:{} {name}] {prompt}", job.id);
    let model_override = job.model.clone();

    let mut cron_config = config.clone();
    cron_config.memory.auto_save = false;

    // Assign a unique run ID for tracing. Isolated jobs also use it in the
    // session path so failed-run memory purge stays scoped per execution.
    // Main-target jobs reuse the stable `main` session path documented in
    // `session_target`.
    let run_session_id = uuid::Uuid::new_v4().to_string();
    let session_path = cron_agent_session_path(&job.session_target, &run_session_id);

    let subagent_span = zeroclaw_log::info_span!(
        "subagent",
        category = "cron",
        agent_alias = %agent_alias,
        cron_job_id = %job.id,
        run_id = %run_session_id,
        spawn_site = "cron",
    );

    let run_security = cron_agent_run_policy(security, job);
    let run_overrides = crate::agent::loop_::AgentRunOverrides {
        security: Some(Arc::new(run_security)),
        memory: None,
        is_subagent: false,
        // `uses_memory = false` fully opts the job out of the engine's
        // memory-context injection (stateless digest jobs)...
        suppress_memory_inject: !job.uses_memory,
        // ...and makes the run memory-free end to end: the loop binds a
        // `NoneMemory` backend and drops the persistent memory tools, so a
        // `uses_memory = false` job can neither recall/store through a real
        // backend nor reach one via advertised memory tools
        memory_free: !job.uses_memory,
        // Cron runs are short-lived and one-shot — no cross-turn reuse
        // contract, so the per-call `connect_all` path inside
        // `agent::run` is the correct choice. The daemon heartbeat
        // worker is the only `mcp_registry` supplier.
        mcp_registry: None,
    };
    let run_result = match job.session_target {
        SessionTarget::Main | SessionTarget::Isolated => {
            await_cron_agent_run(
                config,
                job,
                Box::pin(
                    crate::agent::run(
                        cron_config,
                        agent_alias,
                        Some(prefixed_prompt),
                        None,
                        model_override,
                        config
                            .model_provider_for_agent(agent_alias)
                            .and_then(|e| e.temperature),
                        vec![],
                        false,
                        Some(session_path.clone()),
                        job.allowed_tools.clone(),
                        zeroclaw_api::ingress::TurnOrigin::Cron,
                        run_overrides,
                    )
                    .instrument(subagent_span),
                ) as futures_util::future::BoxFuture<'_, Result<String>>,
            )
            .await
        }
    };

    let run_result = check_cron_agent_completion(config, security, job, run_result).await;
    finish_cron_agent_run(config, agent_alias, job, &session_path, run_result).await
}

async fn check_cron_agent_completion(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    run_result: Result<String>,
) -> Result<String> {
    // Declarative config is the only authority; an imperative same-ID job must
    // never acquire a command from an unrelated declaration.
    if job.source != "declarative" || job.job_type != JobType::Agent {
        return run_result;
    }
    let Some(declaration) = config.cron.get(&job.id) else {
        return run_result;
    };
    let command = match declaration.validated_completion_check() {
        Ok(Some(command)) => command,
        Ok(None) => return run_result,
        Err(error) => return failed_cron_completion_check(run_result, &error.to_string()),
    };
    let runtime = match crate::platform::create_runtime(&config.runtime) {
        Ok(runtime) => runtime,
        Err(error) => return failed_cron_completion_check(run_result, &error.to_string()),
    };
    let mut check_job = job.clone();
    check_job.command = command.to_string();
    check_job.shell_output_format = CronShellOutputFormat::Wrapped;
    let (success, output) = run_job_command_with_runtime_and_timeout(
        config,
        runtime.as_ref(),
        security,
        &check_job,
        false,
        Duration::from_secs(COMPLETION_CHECK_TIMEOUT_SECS),
    )
    .await;
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
            ::serde_json::json!({
                "job_id": job.id,
                "error_key": "cron.completion_check",
                "success": success,
            })
        ),
        "Cron agent completion check finished"
    );
    if success {
        run_result
    } else {
        failed_cron_completion_check(run_result, &output)
    }
}

fn failed_cron_completion_check(run_result: Result<String>, detail: &str) -> Result<String> {
    let check_error = crate::i18n::get_required_cli_string_with_args(
        "cli-cron-completion-check-failed",
        &[("detail", detail)],
    );
    Err(anyhow::Error::msg(match run_result {
        Ok(_) => check_error,
        Err(error) => format!("{error}\n{check_error}"),
    }))
}

async fn finish_cron_agent_run(
    config: &Config,
    agent_alias: &str,
    job: &CronJob,
    session_path: &std::path::Path,
    run_result: Result<String>,
) -> (bool, String) {
    match run_result {
        Ok(response) => (
            true,
            if response.trim().is_empty() {
                "agent job executed".to_string()
            } else {
                response
            },
        ),
        Err(e) => {
            if matches!(job.session_target, SessionTarget::Isolated) {
                let mem_session_key = zeroclaw_api::session_keys::sanitize_session_key(&format!(
                    "cli:{}",
                    session_path.display()
                ));
                if let Ok(mem) = zeroclaw_memory::create_memory_for_agent(
                    config,
                    agent_alias,
                    config
                        .model_provider_for_agent(agent_alias)
                        .and_then(|e| e.api_key.as_deref()),
                )
                .await
                {
                    let _ = mem.purge_session(&mem_session_key).await;
                }
            }
            (false, format!("agent job failed: {e}"))
        }
    }
}

async fn persist_job_result(
    config: &Config,
    job: &CronJob,
    success: bool,
    output: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
) -> CronDeliveryOutcome {
    let duration_ms = (finished_at - started_at).num_milliseconds();
    let outcome = deliver_and_classify_run_result(
        config,
        job,
        success,
        output.to_string(),
        CronDeliveryContext::Scheduled,
    )
    .await;

    let action = if is_one_shot_auto_delete(job) && outcome.success {
        RunCompletionAction::Delete
    } else if matches!(job.schedule, Schedule::At { .. }) {
        RunCompletionAction::Disable
    } else {
        RunCompletionAction::Reschedule
    };

    let job_state_at = Utc::now();
    if let Err(e) = persist_run_result(
        config,
        job,
        started_at,
        finished_at,
        job_state_at,
        &outcome.status,
        Some(&outcome.output),
        duration_ms,
        action,
    ) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"e": e.to_string()})),
            "Failed to persist scheduler run result: "
        );

        if action == RunCompletionAction::Delete {
            // Best-effort fallback for the legacy behavior: a successful
            // auto-delete one-shot should not be picked up again if the
            // combined history+state transaction fails while inserting or
            // pruning the run row.
            if let Err(disable_err) = persist_run_completion_state(
                config,
                job,
                job_state_at,
                &outcome.status,
                Some(&outcome.output),
                RunCompletionAction::Disable,
            ) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"disable_err": disable_err.to_string()})),
                    "Failed to disable one-shot cron job after history persistence failure: "
                );
            }
        } else {
            // For recurring jobs and non-delete one-shots, keep the scheduler
            // moving even if run-history persistence fails.
            if let Err(state_err) = persist_run_completion_state(
                config,
                job,
                job_state_at,
                &outcome.status,
                Some(&outcome.output),
                action,
            ) {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"state_err": state_err.to_string()})),
                    "Failed to update cron job state after history persistence failure: "
                );
            }
        }
    }

    outcome
}

pub(crate) fn occurrence_delivery_state(outcome: Option<EffectOutcome>) -> &'static str {
    match outcome {
        None | Some(EffectOutcome::NotStarted) => "not_started",
        Some(EffectOutcome::Confirmed) => "confirmed",
        Some(EffectOutcome::ConfirmedFailed) => "confirmed_failed",
        Some(EffectOutcome::PartiallyApplied) => "partially_applied",
        Some(EffectOutcome::PossiblyApplied) => "possibly_applied",
        Some(EffectOutcome::ReconciliationRequired) => "reconciliation_required",
    }
}

fn is_one_shot_auto_delete(job: &CronJob) -> bool {
    job.delete_after_run && matches!(job.schedule, Schedule::At { .. })
}

fn is_high_frequency_agent_job(job: &CronJob) -> bool {
    if !matches!(job.job_type, JobType::Agent) {
        return false;
    }
    match &job.schedule {
        Schedule::Every { every_ms } => *every_ms < 5 * 60 * 1000,
        Schedule::Cron { .. } => {
            let now = Utc::now();
            next_run_for_schedule(&job.schedule, now)
                .and_then(|a| next_run_for_schedule(&job.schedule, a).map(|b| (a, b)))
                .map(|(a, b)| (b - a).num_minutes() < 5)
                .unwrap_or(false)
        }
        Schedule::At { .. } => false,
    }
}

fn warn_if_high_frequency_agent_job(job: &CronJob) {
    if is_high_frequency_agent_job(job) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            &format!(
                "Cron agent job '{}' is scheduled more frequently than every 5 minutes",
                job.id
            )
        );
    }
}

async fn deliver_if_configured(
    config: &Config,
    job: &CronJob,
    output: &str,
    handler: Option<&DeliveryFn>,
) -> Result<Option<EffectOutcome>> {
    let delivery: &DeliveryConfig = &job.delivery;
    if !delivery.mode.eq_ignore_ascii_case("announce") {
        return Ok(None);
    }

    if !announce_delivery_decision(output).should_deliver() {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Success)
                .with_attrs(::serde_json::json!({"job_id": job.id})),
            "Cron job returned NO_REPLY sentinel — skipping delivery"
        );
        return Ok(None);
    }

    let channel = delivery.channel.as_deref().ok_or_else(|| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"field": "channel"})),
            "cron delivery announce refused: required field missing"
        );
        notification_not_started().context("delivery.channel is required for announce mode")
    })?;
    let target = delivery.to.as_deref().ok_or_else(|| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({"field": "to"})),
            "cron delivery announce refused: required field missing"
        );
        notification_not_started().context("delivery.to is required for announce mode")
    })?;

    let route =
        delivery
            .reply_to
            .as_ref()
            .map(|reply_to| zeroclaw_api::conversation::ConversationRoute {
                channel: channel.into(),
                recipient: target.into(),
                sender: String::new(),
                thread: delivery.thread_id.clone(),
                reply_to: reply_to.clone(),
            });
    let delivery = zeroclaw_api::conversation::ACTIVE_CONVERSATION.scope(
        route,
        deliver_announcement_with_handler(
            handler,
            config,
            channel,
            target,
            delivery.thread_id.as_deref(),
            output,
        ),
    );
    zeroclaw_api::delivery::SUMMARY
        .scope(std::sync::Mutex::new(None), async {
            delivery.await?;
            Ok(Some(notification_evidence(
                zeroclaw_api::delivery::take_summary(),
            )))
        })
        .await
}

fn notification_evidence(summary: Option<DeliverySummary>) -> EffectOutcome {
    match summary {
        Some(summary) if summary.is_fully_confirmed() => EffectOutcome::Confirmed,
        Some(summary) if summary.outcome != EffectOutcome::Confirmed => summary.outcome,
        Some(summary)
            if summary.confirmed_chunks > 0 && summary.confirmed_chunks < summary.total_chunks =>
        {
            EffectOutcome::PartiallyApplied
        }
        _ => EffectOutcome::PossiblyApplied,
    }
}

/// Delivery function type — takes owned values so the returned future is 'static.
/// The fourth `Option<String>` is the optional thread/conversation id propagated
/// to channels whose outbound `thread_id` is distinct from the recipient (webhook).
pub type DeliveryFn = Box<
    dyn Fn(
            Config,
            String,
            String,
            Option<String>,
            String,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
        + Send
        + Sync,
>;

/// Global delivery function, injected by the binary crate at startup.
static DELIVERY_FN: std::sync::OnceLock<DeliveryFn> = std::sync::OnceLock::new();

/// Register the channel delivery function. Called once at startup by the binary.
pub fn register_delivery_fn(f: DeliveryFn) {
    let _ = DELIVERY_FN.set(f);
}

pub async fn deliver_announcement(
    config: &Config,
    channel: &str,
    target: &str,
    thread_id: Option<&str>,
    output: &str,
) -> Result<()> {
    deliver_announcement_with_handler(
        DELIVERY_FN.get(),
        config,
        channel,
        target,
        thread_id,
        output,
    )
    .await
}

async fn deliver_announcement_with_handler(
    handler: Option<&DeliveryFn>,
    config: &Config,
    channel: &str,
    target: &str,
    thread_id: Option<&str>,
    output: &str,
) -> Result<()> {
    if let Some(f) = handler {
        f(
            config.clone(),
            channel.to_string(),
            target.to_string(),
            thread_id.map(str::to_string),
            output.to_string(),
        )
        .await
    } else {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"channel": channel, "target": target})),
            "Cron delivery skipped: no delivery handler registered \
             (register_delivery_fn was not called by the binary)"
        );
        Err(notification_not_started())
    }
}

fn notification_not_started() -> anyhow::Error {
    DeliveryFailure {
        outcome: EffectOutcome::NotStarted,
        chunk_index: 0,
        total_chunks: 1,
        confirmed_chunks: 0,
    }
    .into()
}

async fn run_job_command_with_runtime(
    config: &Config,
    runtime: &dyn RuntimeAdapter,
    security: &SecurityPolicy,
    job: &CronJob,
    approved: bool,
) -> (bool, String) {
    run_job_command_with_runtime_and_timeout(
        config,
        runtime,
        security,
        job,
        approved,
        Duration::from_secs(SHELL_JOB_TIMEOUT_SECS),
    )
    .await
}

async fn run_job_command_with_runtime_and_timeout(
    config: &Config,
    runtime: &dyn RuntimeAdapter,
    security: &SecurityPolicy,
    job: &CronJob,
    approved: bool,
    timeout: Duration,
) -> (bool, String) {
    if !security.can_act() {
        return (
            false,
            "blocked by security policy: autonomy is read-only".to_string(),
        );
    }

    if security.is_rate_limited() {
        return (
            false,
            "blocked by security policy: rate limit exceeded".to_string(),
        );
    }

    // Unified command validation: allowlist + risk + path checks in one call.
    // Jobs created via the validated helpers were already checked at creation
    // time, but we re-validate at execution time to catch policy changes and
    // manually-edited job stores.
    if let Err(error) =
        crate::cron::validate_shell_command_with_security(runtime, security, &job.command, approved)
    {
        return (false, error.to_string());
    }

    if let Some(path) = security.forbidden_path_argument(&job.command) {
        return (
            false,
            format!("blocked by security policy: forbidden path argument: {path}"),
        );
    }

    if !security.record_action() {
        return (
            false,
            "blocked by security policy: action budget exhausted".to_string(),
        );
    }

    // `job.shell_output_format` is already the canonical value by the time
    // it reaches here: due_jobs()/all_overdue_jobs() resolve declarative jobs
    // from config and leave imperative jobs on their stored field (see
    // resolve_declarative_shell_output_format in store.rs). Re-deriving it
    // here from `config.cron.get(&job.id)` without checking `job.source`
    // would let an unrelated same-ID declarative config entry silently
    // override an imperative job's stored format.
    let output_format = &job.shell_output_format;

    let mut command = match runtime.build_shell_command(&job.command, &config.data_dir) {
        Ok(command) => command,
        Err(error) => return (false, format!("shell setup error: {error}")),
    };

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return (false, format!("spawn error: {error}")),
    };

    match time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = match output_format {
                // Raw mode on success returns bare stdout, by design — the
                // point is to hand back exactly what a direct shell run
                // would print on stdout, with no wrapper. stderr on a
                // successful exit is intentionally dropped, not lost by
                // accident; a failing exit still gets the full wrapped
                // status/stdout/stderr envelope below for diagnosis.
                CronShellOutputFormat::Raw if output.status.success() => stdout.trim().to_string(),
                _ => format!(
                    "status={}\nstdout:\n{}\nstderr:\n{}",
                    output.status,
                    stdout.trim(),
                    stderr.trim()
                ),
            };
            (output.status.success(), combined)
        }
        Ok(Err(e)) => (false, format!("spawn error: {e}")),
        Err(_) => (
            false,
            format!("job timed out after {}s", timeout.as_secs_f64()),
        ),
    }
}

#[cfg(test)]
async fn run_job_command(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
) -> (bool, String) {
    let runtime = match crate::platform::create_runtime(&config.runtime) {
        Ok(runtime) => runtime,
        Err(error) => return (false, format!("shell setup error: {error}")),
    };
    run_job_command_with_runtime(config, runtime.as_ref(), security, job, false).await
}

#[cfg(all(test, not(target_os = "windows")))]
async fn run_job_command_with_timeout(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    timeout: Duration,
) -> (bool, String) {
    let runtime = match crate::platform::create_runtime(&config.runtime) {
        Ok(runtime) => runtime,
        Err(error) => return (false, format!("shell setup error: {error}")),
    };
    run_job_command_with_runtime_and_timeout(
        config,
        runtime.as_ref(),
        security,
        job,
        false,
        timeout,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::{self, DeliveryConfig};
    use crate::security::SecurityPolicy;
    use chrono::{Duration as ChronoDuration, Utc};
    use tempfile::TempDir;
    use zeroclaw_config::schema::{Config, RuntimeKind};

    const TEST_AGENT: &str = "test-agent";

    fn build_configured_shell_command(
        config: &Config,
        command: &str,
        workspace_dir: &std::path::Path,
    ) -> anyhow::Result<tokio::process::Command> {
        let runtime = crate::platform::create_runtime(&config.runtime)?;
        runtime.build_shell_command(command, workspace_dir)
    }

    #[test]
    fn is_no_reply_sentinel_matches_bare_form_case_insensitively() {
        assert!(is_no_reply_sentinel("NO_REPLY"));
        assert!(is_no_reply_sentinel("no_reply"));
        assert!(is_no_reply_sentinel("No_Reply"));
        // Trim tolerance.
        assert!(is_no_reply_sentinel("  NO_REPLY  "));
        assert!(is_no_reply_sentinel("\nNO_REPLY\n"));
    }

    #[test]
    fn is_no_reply_sentinel_matches_quiet_info_and_legacy_prefixes() {
        // Legacy form is documented as "treated as INFO".
        assert!(is_no_reply_sentinel("NO_REPLY: nothing to report"));
        assert!(is_no_reply_sentinel("  NO_REPLY: trimmed  "));
        // Explicit informational kind.
        assert!(is_no_reply_sentinel("NO_REPLY[INFO]: all healthy"));
        assert!(is_no_reply_sentinel("no_reply[info]: all healthy"));
        // Bracket whitespace tolerance.
        assert!(is_no_reply_sentinel("NO_REPLY[ info ]: spaced"));
    }

    #[test]
    fn is_no_reply_sentinel_does_not_suppress_failure_or_refusal_kinds() {
        // REFUSE / FAIL carry operator-visible meaning. In the cron/heartbeat
        // announce context there is no reaction side-channel, so suppressing
        // them would silently drop a failure/refusal the operator must see
        // review feedback).
        assert!(!is_no_reply_sentinel(
            "NO_REPLY[FAIL]: database check timed out"
        ));
        assert!(!is_no_reply_sentinel("no_reply[fail]: timed out"));
        assert!(!is_no_reply_sentinel(
            "NO_REPLY[REFUSE]: policy prevented the check"
        ));
        assert!(!is_no_reply_sentinel("no_reply[refuse]: blocked"));
        // Unknown/future kinds are conservatively delivered, not suppressed.
        assert!(!is_no_reply_sentinel("NO_REPLY[WARN]: disk at 90%"));
        // Malformed kinded form with no closing bracket is delivered.
        assert!(!is_no_reply_sentinel("NO_REPLY[INFO without close"));
    }

    #[test]
    fn is_no_reply_sentinel_rejects_real_content() {
        assert!(!is_no_reply_sentinel(""));
        assert!(!is_no_reply_sentinel("   "));
        assert!(!is_no_reply_sentinel("All systems nominal"));
        // Sentinel-looking but not a sentinel: word embedded in real prose.
        assert!(!is_no_reply_sentinel(
            "The job returned NO_REPLY which means nothing happened"
        ));
        assert!(!is_no_reply_sentinel("NO_REPLYING is the status"));
    }

    async fn test_config(tmp: &TempDir) -> Config {
        let mut config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.risk_profiles.insert(
            TEST_AGENT.to_string(),
            zeroclaw_config::schema::RiskProfileConfig::default(),
        );
        config.runtime_profiles.insert(
            TEST_AGENT.to_string(),
            zeroclaw_config::schema::RuntimeProfileConfig::default(),
        );
        config.providers.models.openrouter.insert(
            TEST_AGENT.to_string(),
            zeroclaw_config::schema::OpenRouterModelProviderConfig::default(),
        );
        config.agents.insert(
            TEST_AGENT.to_string(),
            zeroclaw_config::schema::AliasedAgentConfig {
                model_provider: format!("openrouter.{TEST_AGENT}").into(),
                risk_profile: TEST_AGENT.into(),
                runtime_profile: TEST_AGENT.into(),
                ..Default::default()
            },
        );
        tokio::fs::create_dir_all(&config.data_dir).await.unwrap();
        config
    }

    fn test_security(config: &Config) -> SecurityPolicy {
        SecurityPolicy::for_agent(config, TEST_AGENT).expect("test-agent has resolvable profiles")
    }

    fn test_job(command: &str) -> CronJob {
        CronJob {
            id: "test-job".into(),
            expression: "* * * * *".into(),
            schedule: crate::cron::Schedule::Cron {
                expr: "* * * * *".into(),
                tz: None,
            },
            command: command.into(),
            prompt: None,
            name: None,
            job_type: JobType::Shell,
            session_target: SessionTarget::Isolated,
            model: None,
            agent_alias: TEST_AGENT.into(),
            enabled: true,
            delivery: DeliveryConfig::default(),
            delete_after_run: false,
            allowed_tools: None,
            uses_memory: true,
            source: "imperative".into(),
            shell_output_format: CronShellOutputFormat::default(),
            missed_run_policy: None,
            created_at: Utc::now(),
            next_run: Utc::now(),
            last_run: None,
            last_status: None,
            last_output: None,
        }
    }

    #[test]
    fn cron_agent_run_policy_uses_scheduler_workspace() {
        let workspace = std::path::PathBuf::from("/tmp/zeroclaw-cron-agent-workspace");
        let security = SecurityPolicy {
            workspace_dir: workspace.clone(),
            ..SecurityPolicy::default()
        };
        let mut job = test_job("");
        job.job_type = JobType::Agent;

        let policy = cron_agent_run_policy(&security, &job);

        assert_eq!(policy.workspace_dir, workspace);
    }

    struct PowerShellProbeRuntime {
        build_calls: std::sync::atomic::AtomicUsize,
    }

    impl PowerShellProbeRuntime {
        fn new() -> Self {
            Self {
                build_calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl RuntimeAdapter for PowerShellProbeRuntime {
        fn name(&self) -> &str {
            "powershell-probe"
        }

        fn has_filesystem_access(&self) -> bool {
            true
        }

        fn storage_path(&self) -> std::path::PathBuf {
            std::env::temp_dir()
        }

        fn supports_long_running(&self) -> bool {
            true
        }

        fn shell_dialect(&self) -> zeroclaw_api::runtime_traits::ShellDialect {
            zeroclaw_api::runtime_traits::ShellDialect::PowerShell
        }

        fn build_shell_command(
            &self,
            _command: &str,
            workspace_dir: &std::path::Path,
        ) -> anyhow::Result<tokio::process::Command> {
            self.build_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

            #[cfg(target_os = "windows")]
            let mut command = {
                let mut command = tokio::process::Command::new("cmd");
                command.args(["/C", "echo", "same-runtime"]);
                command
            };

            #[cfg(not(target_os = "windows"))]
            let mut command = {
                let mut command = tokio::process::Command::new("printf");
                command.arg("same-runtime");
                command
            };

            command.current_dir(workspace_dir);
            Ok(command)
        }
    }

    #[tokio::test]
    async fn cron_shell_validation_and_execution_share_runtime_adapter() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let security = SecurityPolicy {
            autonomy: zeroclaw_config::policy::AutonomyLevel::Full,
            workspace_dir: config.data_dir.clone(),
            allowed_commands: vec!["*".into()],
            block_high_risk_commands: true,
            ..SecurityPolicy::default()
        };
        let runtime = PowerShellProbeRuntime::new();

        let safe_job = test_job("Write-Output \"quoted safe value\" | Select-Object -First 1");
        let (success, output) =
            run_job_command_with_runtime(&config, &runtime, &security, &safe_job, false).await;
        assert!(success, "{output}");
        assert!(output.contains("same-runtime"), "{output}");
        assert_eq!(
            runtime
                .build_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        let dangerous_job = test_job("ac blocked.txt value");
        let (success, output) =
            run_job_command_with_runtime(&config, &runtime, &security, &dangerous_job, true).await;
        assert!(!success);
        assert!(output.contains("high-risk"), "{output}");
        assert_eq!(
            runtime
                .build_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "policy rejection must happen before the runtime builds a command"
        );
    }

    fn unique_component(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    fn agent_job_with_schedule(schedule: crate::cron::Schedule) -> CronJob {
        CronJob {
            job_type: JobType::Agent,
            schedule,
            ..test_job("echo test")
        }
    }

    fn declarative_agent_deadline(config: &mut Config, seconds: Option<u64>) -> CronJob {
        let mut job = agent_job_with_schedule(Schedule::Every { every_ms: 60000 });
        job.source = "declarative".into();
        config.cron.insert(
            job.id.clone(),
            CronJobDecl {
                job_type: "agent".into(),
                prompt: Some("Synthetic scheduled task".into()),
                timeout_secs: seconds,
                ..Default::default()
            },
        );
        job
    }

    #[tokio::test]
    async fn cron_agent_deadline_uses_current_config_not_job_snapshot() {
        let mut config = Config::default();
        let job = declarative_agent_deadline(&mut config, Some(300));
        assert_eq!(
            cron_agent_timeout(&config, &job).unwrap(),
            Some(Duration::from_secs(300))
        );
        config.cron.get_mut(&job.id).unwrap().timeout_secs = Some(900);
        assert_eq!(
            cron_agent_timeout(&config, &job).unwrap(),
            Some(Duration::from_secs(900))
        );
        config.cron.get_mut(&job.id).unwrap().timeout_secs = Some(600);
        assert_eq!(
            cron_agent_timeout(&config, &job).unwrap(),
            Some(Duration::from_secs(600))
        );
        config.cron.get_mut(&job.id).unwrap().timeout_secs = Some(0);
        assert!(
            await_cron_agent_run(&config, &job, async { Ok("must not run".into()) })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn completion_check_overrides_false_success_and_preserves_agent_failure() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        let job = declarative_agent_deadline(&mut config, None);
        let security = SecurityPolicy {
            allowed_commands: vec!["true".into(), "false".into()],
            require_approval_for_medium_risk: false,
            ..test_security(&config)
        };
        config.cron.get_mut(&job.id).unwrap().completion_check = Some("false".into());
        let result =
            check_cron_agent_completion(&config, &security, &job, Ok("NO_REPLY".into())).await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("completion check failed")
        );
        let result = check_cron_agent_completion(
            &config,
            &security,
            &job,
            Err(anyhow::anyhow!("provider failed")),
        )
        .await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("provider failed"));
        assert!(
            error.contains("completion check failed"),
            "check must run after agent failure"
        );
        config.cron.get_mut(&job.id).unwrap().completion_check = Some("true".into());
        assert_eq!(
            check_cron_agent_completion(&config, &security, &job, Ok("NO_REPLY".into()))
                .await
                .unwrap(),
            "NO_REPLY"
        );
        assert_eq!(
            check_cron_agent_completion(
                &config,
                &security,
                &job,
                Err(anyhow::anyhow!("provider failed"))
            )
            .await
            .unwrap_err()
            .to_string(),
            "provider failed"
        );
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn completion_check_honors_current_policy_and_ignores_imperative_collision() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        let mut job = declarative_agent_deadline(&mut config, None);
        config.cron.get_mut(&job.id).unwrap().completion_check = Some("true".into());
        let security = SecurityPolicy {
            allowed_commands: vec![],
            ..test_security(&config)
        };
        assert!(
            check_cron_agent_completion(&config, &security, &job, Ok("done".into()))
                .await
                .is_err()
        );
        job.source = "imperative".into();
        assert_eq!(
            check_cron_agent_completion(&config, &security, &job, Ok("done".into()))
                .await
                .unwrap(),
            "done"
        );
    }

    #[tokio::test]
    async fn cron_agent_deadline_none_and_imperative_collision_remain_unbounded() {
        let mut config = Config::default();
        let mut job = declarative_agent_deadline(&mut config, None);
        assert_eq!(cron_agent_timeout(&config, &job).unwrap(), None);
        assert!(
            time::timeout(
                Duration::from_millis(10),
                await_cron_agent_run(&config, &job, std::future::pending())
            )
            .await
            .is_err()
        );
        config.cron.get_mut(&job.id).unwrap().timeout_secs = Some(1);
        job.source = "imperative".into();
        assert_eq!(cron_agent_timeout(&config, &job).unwrap(), None);
        assert_eq!(
            await_cron_agent_run(&config, &job, async { Ok("imperative completed".into()) })
                .await
                .unwrap(),
            "imperative completed"
        );
        job.source = "declarative".into();
        job.job_type = JobType::Shell;
        assert_eq!(cron_agent_timeout(&config, &job).unwrap(), None);
    }

    #[tokio::test]
    async fn cron_agent_deadline_success_keeps_existing_completion_result() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        let job = declarative_agent_deadline(&mut config, Some(1));
        let result = await_cron_agent_run(&config, &job, async {
            Ok("synthetic completed result".into())
        })
        .await;
        let (success, output) = finish_cron_agent_run(
            &config,
            TEST_AGENT,
            &job,
            std::path::Path::new("cron-fixture"),
            result,
        )
        .await;
        assert!(success);
        assert_eq!(output, "synthetic completed result");
    }

    #[tokio::test]
    async fn cron_agent_deadline_drops_pending_run_and_uses_failure_memory_cleanup() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        let job = declarative_agent_deadline(&mut config, Some(1));
        let session_path = std::path::Path::new("cron-timeout-fixture");
        let key = zeroclaw_api::session_keys::sanitize_session_key("cli:cron-timeout-fixture");
        let mem = zeroclaw_memory::create_memory_for_agent(&config, TEST_AGENT, None)
            .await
            .unwrap();
        mem.store(
            "timeout-fixture",
            "Synthetic failed-run memory",
            zeroclaw_memory::MemoryCategory::Conversation,
            Some(&key),
        )
        .await
        .unwrap();
        mem.store(
            "unrelated-fixture",
            "Synthetic other-run memory",
            zeroclaw_memory::MemoryCategory::Conversation,
            Some("other-session"),
        )
        .await
        .unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(dropped.clone());
        let result = await_cron_agent_run(&config, &job, async move {
            let _guard = guard;
            std::future::pending::<Result<String>>().await
        })
        .await;
        assert!(result.is_err());
        assert!(
            dropped.load(Ordering::SeqCst),
            "deadline must drop the actual agent future"
        );
        let (success, output) =
            finish_cron_agent_run(&config, TEST_AGENT, &job, session_path, result).await;
        assert!(
            !success,
            "a deadline must not become a successful empty completion"
        );
        assert!(output.starts_with("agent job failed:"));
        assert!(output.contains("timed out"));
        assert!(mem.get("timeout-fixture").await.unwrap().is_none());
        assert!(mem.get("unrelated-fixture").await.unwrap().is_some());
    }

    #[test]
    fn high_frequency_daily_cron_is_not_flagged() {
        // `0 6 * * *` fires once per day — must never warn regardless of when the check runs
        let job = agent_job_with_schedule(crate::cron::Schedule::Cron {
            expr: "0 6 * * *".into(),
            tz: Some("America/Chicago".into()),
        });
        assert!(!is_high_frequency_agent_job(&job));
    }

    #[test]
    fn high_frequency_every_4min_cron_is_flagged() {
        let job = agent_job_with_schedule(crate::cron::Schedule::Cron {
            expr: "*/4 * * * *".into(),
            tz: None,
        });
        assert!(is_high_frequency_agent_job(&job));
    }

    #[test]
    fn high_frequency_every_5min_cron_is_not_flagged() {
        // Exactly 5 minutes is acceptable (threshold is strictly less than 5)
        let job = agent_job_with_schedule(crate::cron::Schedule::Cron {
            expr: "*/5 * * * *".into(),
            tz: None,
        });
        assert!(!is_high_frequency_agent_job(&job));
    }

    #[test]
    fn high_frequency_every_interval_below_threshold_is_flagged() {
        let job = agent_job_with_schedule(crate::cron::Schedule::Every {
            every_ms: 4 * 60 * 1000, // 4 minutes
        });
        assert!(is_high_frequency_agent_job(&job));
    }

    #[test]
    fn high_frequency_every_interval_at_threshold_is_not_flagged() {
        let job = agent_job_with_schedule(crate::cron::Schedule::Every {
            every_ms: 5 * 60 * 1000, // exactly 5 minutes
        });
        assert!(!is_high_frequency_agent_job(&job));
    }

    #[test]
    fn high_frequency_shell_job_is_never_flagged() {
        // Shell jobs are exempt regardless of frequency
        let job = CronJob {
            job_type: JobType::Shell,
            schedule: crate::cron::Schedule::Every {
                every_ms: 60 * 1000, // 1 minute
            },
            ..test_job("echo test")
        };
        assert!(!is_high_frequency_agent_job(&job));
    }

    #[test]
    fn cron_agent_session_path_main_is_stable() {
        assert_eq!(
            cron_agent_session_path(&SessionTarget::Main, "ignored"),
            std::path::PathBuf::from("main")
        );
        assert_eq!(
            cron_agent_session_path(&SessionTarget::Isolated, "abc").to_string_lossy(),
            "cron-abc"
        );
    }

    #[test]
    fn cron_agent_run_policy_excludes_scheduler_mutation_tools_by_default() {
        let security = SecurityPolicy::default();
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.allowed_tools = None;

        let policy = cron_agent_run_policy(&security, &job);

        for tool in [
            "cron_add",
            "cron_update",
            "cron_remove",
            "cron_run",
            "schedule",
        ] {
            assert!(
                !policy.is_tool_allowed(tool),
                "{tool} must be excluded from default cron agent runs"
            );
        }
        assert!(
            policy.is_tool_allowed("http_request"),
            "non-scheduler tools remain available when the base policy is unrestricted"
        );
    }

    #[test]
    fn cron_agent_run_policy_respects_explicit_allowed_tools() {
        let security = SecurityPolicy::default();
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.allowed_tools = Some(vec!["cron_add".into()]);

        let policy = cron_agent_run_policy(&security, &job);

        assert!(
            policy.is_tool_allowed("cron_add"),
            "explicit cron job allowed_tools should remain the override for intentional scheduler automation"
        );
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn run_job_command_success() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("echo scheduler-ok");
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(success);
        assert!(output.contains("scheduler-ok"));
        assert!(output.contains("status=exit status: 0"));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn run_job_command_raw_output_success() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        // The store layer resolves shell_output_format before handing the job
        // to the scheduler (see resolve_declarative_shell_output_format), so
        // the job's own field is already canonical by the time it gets here.
        let mut job = test_job("echo raw-format-ok");
        job.shell_output_format = CronShellOutputFormat::Raw;
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(success);
        // Raw output should be just the command's trimmed stdout, no wrapper.
        assert_eq!(output, "raw-format-ok");
        assert!(!output.contains("status="));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn run_job_command_raw_output_success_drops_stderr() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        // A zero-exit command that still writes to stderr (e.g. a tool's
        // progress/warning chatter) must not leak into raw-mode output.
        let mut job = test_job("echo raw-stdout-ok; echo raw-stderr-noise >&2");
        job.shell_output_format = CronShellOutputFormat::Raw;
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(success);
        // Dropping stderr on a successful exit is intentional design, not
        // an oversight — see the comment at the call site.
        assert_eq!(output, "raw-stdout-ok");
        assert!(!output.contains("raw-stderr-noise"));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn run_job_command_raw_output_failure_still_uses_wrapped() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let mut job = test_job("ls definitely_missing_file_raw_test");
        job.shell_output_format = CronShellOutputFormat::Raw;
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        // On failure, raw mode should still include the wrapped format
        // so operators can diagnose the failure.
        assert!(output.contains("status=exit status:"));
        assert!(output.contains("definitely_missing_file_raw_test"));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn run_job_command_imperative_job_ignores_same_id_declarative_config_entry() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        // An unrelated declarative config entry happens to share the
        // imperative job's ID and asks for raw output. Execution must go by
        // the job's own (already-resolved) field, not re-derive from config
        // by ID match, or the imperative job's stored format gets silently
        // overridden.
        config.cron.insert(
            "test-job".into(),
            zeroclaw_config::schema::CronJobDecl {
                command: Some("echo collision-ok".into()),
                shell_output_format: CronShellOutputFormat::Raw,
                ..Default::default()
            },
        );
        let mut job = test_job("echo collision-ok");
        job.source = "imperative".into();
        job.shell_output_format = CronShellOutputFormat::Wrapped;
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(success);
        assert!(
            output.contains("status="),
            "imperative job's own Wrapped format must win over a same-ID declarative config entry: {output}"
        );
    }

    #[tokio::test]
    async fn manual_invocation_blocks_competitors_and_cancellation_preserves_receipts() {
        for cancel in [false, true] {
            let tmp = TempDir::new().unwrap();
            let mut config = test_config(&tmp).await;
            config
                .risk_profiles
                .get_mut(TEST_AGENT)
                .unwrap()
                .allowed_commands = vec!["echo".into()];
            let job = cron::store::add_shell_job(
                &config,
                TEST_AGENT,
                None,
                Schedule::Every { every_ms: 60000 },
                "echo synthetic",
                Some(DeliveryConfig {
                    mode: "announce".into(),
                    channel: Some("telegram.synthetic".into()),
                    to: Some("synthetic-peer".into()),
                    ..Default::default()
                }),
            )
            .unwrap();
            let runtime = Arc::new(PowerShellProbeRuntime::new());
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let handler: DeliveryFn = {
                let entered = entered.clone();
                let release = release.clone();
                Box::new(move |_, _, _, _, _| {
                    let entered = entered.clone();
                    let release = release.clone();
                    Box::pin(async move {
                        entered.notify_one();
                        release.notified().await;
                        zeroclaw_api::delivery::record_summary(DeliverySummary {
                            outcome: EffectOutcome::Confirmed,
                            confirmed_chunks: 1,
                            total_chunks: 1,
                        });
                        Ok(())
                    })
                })
            };
            let first = {
                let config = config.clone();
                let job = job.clone();
                let runtime = runtime.clone();
                ::zeroclaw_spawn::spawn!(async move {
                    run_manual_job_inner(
                        &config,
                        &job,
                        CronDeliveryContext::RpcManual,
                        &None,
                        Some(runtime.as_ref()),
                        true,
                        Some(&handler),
                        Some("boundary-fixture"),
                    )
                    .await
                })
            };
            tokio::time::timeout(Duration::from_secs(5), entered.notified())
                .await
                .unwrap();
            assert!(!claim_job(&config, &job.id, Utc::now()).unwrap());
            let second = run_manual_job_with_runtime(
                &config,
                &job,
                CronDeliveryContext::ToolManual,
                &None,
                runtime.as_ref(),
                true,
                None,
            )
            .await;
            assert_eq!(second.effect_outcome, EffectOutcome::NotStarted);
            assert_eq!(second.execution_outcome, EffectOutcome::NotStarted);
            assert!(second.occurrence_id.is_none());
            assert_eq!(
                runtime
                    .build_calls
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            if cancel {
                first.abort();
                assert!(matches!(first.await, Err(error) if error.is_cancelled()));
                assert!(cron::list_runs(&config, &job.id, 10).unwrap().is_empty());
                assert_eq!(clear_stale_locks(&config).unwrap(), 1);
                let blocked = run_manual_job_with_request_id(
                    &config,
                    &job,
                    CronDeliveryContext::GatewayManual,
                    &None,
                    Some("boundary-fixture"),
                )
                .await;
                assert_eq!(
                    blocked.effect_outcome,
                    EffectOutcome::ReconciliationRequired
                );
                assert!(blocked.duplicate);
                assert_eq!(blocked.execution_outcome, EffectOutcome::Confirmed);
                assert_eq!(
                    runtime
                        .build_calls
                        .load(std::sync::atomic::Ordering::SeqCst),
                    1
                );
            } else {
                release.notify_one();
                let result = first.await.unwrap();
                assert!(result.success, "{}", result.output);
                assert_eq!(result.effect_outcome, EffectOutcome::Confirmed);
                let duplicate = run_manual_job_with_request_id(
                    &config,
                    &job,
                    CronDeliveryContext::RpcManual,
                    &None,
                    Some("boundary-fixture"),
                )
                .await;
                assert!(duplicate.duplicate && duplicate.success);
                assert_eq!(duplicate.occurrence_id, result.occurrence_id);
                assert_eq!(
                    runtime
                        .build_calls
                        .load(std::sync::atomic::Ordering::SeqCst),
                    1
                );

                assert_eq!(result.execution_outcome, EffectOutcome::Confirmed);
                assert_eq!(result.delivery_outcome, Some(EffectOutcome::Confirmed));
                assert!(result.occurrence_id.unwrap().starts_with("manual:"));
                assert_eq!(cron::list_runs(&config, &job.id, 10).unwrap().len(), 1);
                assert_eq!(
                    cron::get_job(&config, &job.id).unwrap().next_run,
                    job.next_run
                );
            }
        }
    }

    #[tokio::test]
    async fn manual_trigger_rejects_durable_quarantine_even_with_stale_snapshot() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let now = Utc::now();
        let job = cron::store::add_shell_job(
            &config,
            TEST_AGENT,
            None,
            Schedule::At {
                at: now + ChronoDuration::hours(1),
            },
            "echo must-not-run",
            None,
        )
        .unwrap();
        reconcile_missed_run(&config, &job, now + ChronoDuration::hours(2)).unwrap();
        assert_eq!(
            job.last_status, None,
            "the caller holds a stale pre-quarantine snapshot"
        );
        let (tx, mut rx) = tokio::sync::broadcast::channel(4);
        for context in [
            CronDeliveryContext::ToolManual,
            CronDeliveryContext::GatewayManual,
            CronDeliveryContext::RpcManual,
        ] {
            let result = run_manual_job(&config, &job, context, &Some(tx.clone())).await;
            assert!(!result.success);
            assert_eq!(result.status, "uncertain");
            assert!(result.output.contains("execution not started"));
        }
        assert!(
            rx.try_recv().is_err(),
            "no synthetic execution or notification event"
        );
        assert!(cron::list_runs(&config, &job.id, 10).unwrap().is_empty());
        let current = cron::get_job(&config, &job.id).unwrap();
        assert_eq!(current.last_status.as_deref(), Some("uncertain"));
        assert_eq!(current.last_run, None);
        assert!(!current.enabled);
        cron::remove_job(&config, &job.id).unwrap();
        let missing = run_manual_job(&config, &job, CronDeliveryContext::RpcManual, &None).await;
        assert!(!missing.success);
        assert!(missing.output.contains("could not read durable state"));
    }

    #[tokio::test]
    async fn run_manual_job_persists_history_and_broadcasts() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .allowed_commands = vec!["echo".into()];
        let job = cron::add_shell_job_with_approval(
            &config,
            TEST_AGENT,
            Some("manual-run".into()),
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo rpc-manual-ok",
            None,
            true,
        )
        .expect("test job should be persisted");
        let (tx, mut rx) = tokio::sync::broadcast::channel(8);
        let event_tx = Some(tx);

        let result = run_manual_job(&config, &job, CronDeliveryContext::RpcManual, &event_tx).await;

        assert!(result.success);
        assert_eq!(result.status, "ok");
        assert!(result.output.contains("rpc-manual-ok"));

        let updated = cron::get_job(&config, &job.id).expect("job state should update");
        assert_eq!(updated.last_status.as_deref(), Some("ok"));
        assert!(
            updated
                .last_output
                .as_deref()
                .is_some_and(|output| output.contains("rpc-manual-ok"))
        );

        let runs = cron::list_runs(&config, &job.id, 10).expect("run history should list");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "ok");
        assert!(
            runs[0]
                .output
                .as_deref()
                .unwrap_or("")
                .contains("rpc-manual-ok")
        );

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("manual trigger should broadcast")
            .expect("broadcast channel should stay open");
        assert_eq!(event["type"], "cron_result");
        assert_eq!(event["job_id"], job.id);
        assert_eq!(event["success"], true);
        assert_eq!(event["manual"], true);
        assert!(
            event["output"]
                .as_str()
                .unwrap_or("")
                .contains("rpc-manual-ok")
        );
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn run_job_command_failure() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("ls definitely_missing_file_for_scheduler_test");
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("definitely_missing_file_for_scheduler_test"));
        assert!(output.contains("status=exit status:"));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn run_job_command_times_out() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .allowed_commands = vec!["sleep".into()];
        let job = test_job("sleep 1");
        let security = test_security(&config);

        let (success, output) =
            run_job_command_with_timeout(&config, &security, &job, Duration::from_millis(50)).await;
        assert!(!success);
        assert!(output.contains("job timed out after"));
    }

    #[tokio::test]
    async fn run_job_command_blocks_disallowed_command() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .allowed_commands = vec!["echo".into()];
        let job = test_job("curl https://evil.example");
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.to_lowercase().contains("not allowed"));
    }

    #[tokio::test]
    async fn run_job_command_blocks_forbidden_path_argument() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .allowed_commands = vec!["cat".into()];
        let outside_path = absolute_path_outside_workspace();
        let job = test_job(&format!("cat {outside_path}"));
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("forbidden path argument"));
        assert!(output.contains(outside_path));
    }

    #[tokio::test]
    async fn run_job_command_blocks_windows_relative_path_for_powershell() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .allowed_commands = vec!["cat".into()];
        let job = test_job("cat ..\\secret.txt");
        let security = test_security(&config);
        let runtime = crate::platform::NativeRuntime::with_shell("pwsh".into());

        let (success, output) =
            run_job_command_with_runtime(&config, &runtime, &security, &job, false).await;

        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("forbidden path argument"));
        assert!(output.contains("..\\secret.txt"));
    }

    #[tokio::test]
    async fn run_job_command_blocks_powershell_stop_parsing_native_mutation() {
        // Cron shares the same dialect-aware validator as the shell tool. On a
        // PowerShell runtime, `git --% push` would strip `--%` and hand `push`
        // to native Git while policy only sees `--%`; the bounded grammar must
        // reject it so scheduled jobs cannot launder mutations through it.
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .allowed_commands = vec!["git".into()];
        let job = test_job("git --% push origin main");
        let security = test_security(&config);
        let runtime = crate::platform::NativeRuntime::with_shell("pwsh".into());

        let (success, output) =
            run_job_command_with_runtime(&config, &runtime, &security, &job, false).await;

        assert!(!success);
        assert!(
            output.contains("blocked by security policy"),
            "output: {output}"
        );
    }

    #[tokio::test]
    async fn run_job_command_blocks_powershell_mixed_quoted_provider_path() {
        // A scheduled job must not launder an `Env:` provider read past policy
        // by splitting the provider prefix with a quote: `cat E'nv:'PATH` binds
        // as `Env:PATH` on PowerShell. The bounded grammar rejects the mixed
        // quoted/unquoted token through the same validator cron uses.
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .allowed_commands = vec!["cat".into()];
        let job = test_job("cat E'nv:'PATH");
        let security = test_security(&config);
        let runtime = crate::platform::NativeRuntime::with_shell("pwsh".into());

        let (success, output) =
            run_job_command_with_runtime(&config, &runtime, &security, &job, false).await;

        assert!(!success);
        assert!(
            output.contains("blocked by security policy"),
            "output: {output}"
        );
    }

    #[tokio::test]
    async fn run_job_command_blocks_forbidden_option_assignment_path_argument() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .allowed_commands = vec!["grep".into()];
        let outside_path = absolute_path_outside_workspace();
        let job = test_job(&format!("grep --file={outside_path} root ./src"));
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("forbidden path argument"));
        assert!(output.contains(outside_path));
    }

    #[tokio::test]
    async fn run_job_command_blocks_forbidden_short_option_attached_path_argument() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .allowed_commands = vec!["grep".into()];
        let outside_path = absolute_path_outside_workspace();
        let job = test_job(&format!("grep -f{outside_path} root ./src"));
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("forbidden path argument"));
        assert!(output.contains(outside_path));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn run_job_command_blocks_tilde_user_path_argument() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .allowed_commands = vec!["cat".into()];
        let job = test_job("cat ~root/.ssh/id_rsa");
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("forbidden path argument"));
        assert!(output.contains("~root/.ssh/id_rsa"));
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn run_job_command_blocks_input_redirection_path_bypass() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .allowed_commands = vec!["cat".into()];
        let job = test_job("cat </etc/passwd");
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.to_lowercase().contains("not allowed"));
    }

    #[tokio::test]
    async fn run_job_command_blocks_readonly_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .level = crate::security::AutonomyLevel::ReadOnly;
        let job = test_job("echo should-not-run");
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("read-only"));
    }

    #[tokio::test]
    async fn run_job_command_blocks_rate_limited() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .runtime_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .max_actions_per_hour = 0;
        let job = test_job("echo should-not-run");
        let security = test_security(&config);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("rate limit exceeded"));
    }

    #[cfg(target_os = "windows")]
    fn absolute_path_outside_workspace() -> &'static str {
        r"C:\Windows\win.ini"
    }

    #[cfg(not(target_os = "windows"))]
    fn absolute_path_outside_workspace() -> &'static str {
        "/etc/passwd"
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn execute_job_with_retry_does_not_replay_possibly_applied_shell() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.reliability.scheduler_retries = 1;
        config.reliability.provider_backoff_ms = 1;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .allowed_commands = vec!["sh".into()];
        let security = test_security(&config);

        tokio::fs::write(
            config.data_dir.join("retry-once.sh"),
            "#!/bin/sh\nif [ -f retry-ok.flag ]; then\n  echo recovered\n  exit 0\nfi\ntouch retry-ok.flag\nexit 1\n",
        )
        .await
        .unwrap();
        let job = test_job("sh ./retry-once.sh");

        let (success, output) = Box::pin(execute_job_with_retry(
            &config,
            &security,
            "test-agent",
            &job,
            None,
            false,
        ))
        .await;
        assert!(!success);
        assert!(!output.contains("recovered"));
        assert!(config.data_dir.join("retry-ok.flag").exists());
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn execute_job_with_retry_exhausts_attempts() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.reliability.scheduler_retries = 1;
        config.reliability.provider_backoff_ms = 1;
        let security = test_security(&config);

        let job = test_job("ls always_missing_for_retry_test");

        let (success, output) = Box::pin(execute_job_with_retry(
            &config,
            &security,
            "test-agent",
            &job,
            None,
            false,
        ))
        .await;
        assert!(!success);
        assert!(output.contains("always_missing_for_retry_test"));
    }

    #[tokio::test]
    async fn run_agent_job_returns_error_without_provider_key() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.prompt = Some("Say hello".into());
        let security = test_security(&config);

        let (success, output) =
            Box::pin(run_agent_job(&config, &security, "test-agent", &job)).await;
        assert!(!success);
        assert!(output.contains("agent job failed:"));
    }

    #[tokio::test]
    async fn agent_cron_run_keeps_workspace_through_shell_on_retry_and_concurrency() {
        use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
        use tokio::net::TcpListener;
        use zeroclaw_config::schema::{ModelProviderConfig, OllamaModelProviderConfig};

        let requests = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let fail_first_request = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let requests_for_handler = Arc::clone(&requests);
        let fail_first_for_handler = Arc::clone(&fail_first_request);
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<serde_json::Value>| {
                requests_for_handler.lock().unwrap().push(body.clone());
                let has_tool_result = body["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message.get("role").and_then(serde_json::Value::as_str) == Some("tool")
                    })
                });
                let fail_this_request =
                    fail_first_for_handler.swap(false, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if fail_this_request {
                        return (StatusCode::OK, "not-json").into_response();
                    }
                    if has_tool_result {
                        return Json(serde_json::json!({
                            "choices": [{"message": {"content": "done"}}]
                        }))
                        .into_response();
                    }
                    let shell_command = if cfg!(windows) {
                        "type .cron-workspace-marker"
                    } else {
                        "cat .cron-workspace-marker"
                    };
                    Json(serde_json::json!({
                        "choices": [{
                            "message": {
                                "content": null,
                                "tool_calls": [{
                                    "id": "call-shell",
                                    "type": "function",
                                    "function": {
                                        "name": "shell",
                                        "arguments": serde_json::json!({
                                            "command": shell_command
                                        })
                                        .to_string()
                                    }
                                }]
                            }
                        }]
                    }))
                    .into_response()
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = zeroclaw_spawn::spawn!(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.memory.backend = "none".to_string();
        config.memory.auto_save = false;
        // Enable the capability globally, then prove this cron job's explicit
        // allowlist still removes it on every retry and concurrent run.
        config.codex_cli.enabled = true;
        config.reliability.scheduler_retries = 1;
        config.reliability.provider_backoff_ms = 1;
        config.providers.models.ollama.insert(
            "default".to_string(),
            OllamaModelProviderConfig {
                base: ModelProviderConfig {
                    model: Some("cron-workspace-test-model".to_string()),
                    timeout_secs: Some(5),
                    uri: Some(format!("http://{address}")),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        config.agents.get_mut(TEST_AGENT).unwrap().model_provider = "ollama.default".into();
        config.risk_profiles.get_mut(TEST_AGENT).unwrap().level =
            crate::security::AutonomyLevel::Full;

        let mut security = test_security(&config);
        let scheduler_workspace = tmp.path().join("scheduler-owned-workspace");
        std::fs::create_dir_all(&scheduler_workspace).unwrap();
        let workspace_marker = "CRON_SCHEDULER_WORKSPACE_MARKER";
        std::fs::write(
            scheduler_workspace.join(".cron-workspace-marker"),
            workspace_marker,
        )
        .unwrap();
        security.workspace_dir = scheduler_workspace.clone();
        assert_ne!(
            security.workspace_dir,
            config.agent_workspace_dir(TEST_AGENT)
        );
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.prompt = Some("Read the scheduler workspace marker".into());
        job.allowed_tools = Some(vec!["shell".into()]);
        job.uses_memory = false;

        let (success, output) = Box::pin(execute_job_with_retry(
            &config, &security, TEST_AGENT, &job, None, false,
        ))
        .await;
        assert!(success, "retrying cron agent run failed: {output}");
        assert_eq!(output, "done");

        let sequential = Box::pin(run_agent_job(&config, &security, TEST_AGENT, &job)).await;
        assert!(
            sequential.0,
            "repeated cron agent run failed: {:?}",
            sequential.1
        );

        let (concurrent_a, concurrent_b, concurrent_c) = tokio::join!(
            run_agent_job(&config, &security, TEST_AGENT, &job),
            run_agent_job(&config, &security, TEST_AGENT, &job),
            run_agent_job(&config, &security, TEST_AGENT, &job),
        );
        for result in [concurrent_a, concurrent_b, concurrent_c] {
            assert!(result.0, "concurrent cron agent run failed: {:?}", result.1);
        }

        let requests = requests.lock().unwrap();
        assert!(
            requests.iter().all(|request| {
                request["tools"].as_array().is_none_or(|tools| {
                    tools
                        .iter()
                        .all(|tool| tool["function"]["name"].as_str() != Some("codex_cli"))
                })
            }),
            "cron retries must not manufacture a codex_cli grant outside job.allowed_tools"
        );
        let tool_results: Vec<&str> = requests
            .iter()
            .filter_map(|request| {
                request["messages"].as_array()?.iter().find_map(|message| {
                    (message.get("role").and_then(serde_json::Value::as_str) == Some("tool"))
                        .then(|| message.get("content")?.as_str())
                        .flatten()
                })
            })
            .collect();
        assert_eq!(
            tool_results.len(),
            5,
            "each successful run must execute shell once"
        );
        for tool_result in tool_results {
            assert!(
                tool_result.contains(workspace_marker),
                "shell output must contain the scheduler workspace marker, got {tool_result:?}"
            );
        }

        server.abort();
    }

    #[tokio::test]
    async fn run_agent_job_blocks_readonly_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .level = crate::security::AutonomyLevel::ReadOnly;
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.prompt = Some("Say hello".into());
        let security = test_security(&config);

        let (success, output) =
            Box::pin(run_agent_job(&config, &security, "test-agent", &job)).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("read-only"));
    }

    #[tokio::test]
    async fn run_agent_job_blocks_rate_limited() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config
            .runtime_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .max_actions_per_hour = 0;
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.prompt = Some("Say hello".into());
        let security = test_security(&config);

        let (success, output) =
            Box::pin(run_agent_job(&config, &security, "test-agent", &job)).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("rate limit exceeded"));
    }

    #[tokio::test]
    async fn process_due_jobs_marks_component_ok_even_when_idle() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let component = unique_component("scheduler-idle");

        crate::health::mark_component_error(&component, "pre-existing error");
        process_due_jobs(&config, Vec::new(), &component, &None).await;

        let snapshot = crate::health::snapshot_json();
        let entry = &snapshot["components"][component.as_str()];
        assert_eq!(entry["status"], "ok");
        assert!(entry["last_ok"].as_str().is_some());
        assert!(entry["last_error"].is_null());
    }

    #[tokio::test]
    async fn process_due_jobs_failure_does_not_mark_component_unhealthy() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("ls definitely_missing_file_for_scheduler_component_health_test");
        let component = unique_component("scheduler-fail");

        crate::health::mark_component_ok(&component);
        process_due_jobs(&config, vec![job], &component, &None).await;

        let snapshot = crate::health::snapshot_json();
        let entry = &snapshot["components"][component.as_str()];
        assert_eq!(entry["status"], "ok");
    }

    #[tokio::test]
    async fn persist_job_result_records_run_and_reschedules_shell_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success.success);

        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        let updated = cron::get_job(&config, &job.id).unwrap();
        assert_eq!(updated.last_status.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn persist_job_result_uses_one_write_connection_for_recurring_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        crate::cron::store::reset_write_connection_count_for_tests(&config);
        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;

        assert!(success.success);
        assert_eq!(
            crate::cron::store::write_connection_count_for_tests(&config),
            1
        );
    }

    #[tokio::test]
    async fn persist_job_result_prunes_run_history_and_updates_last_fields() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.scheduler.max_run_history = 2;
        let job = cron::add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
        let base = Utc::now();

        for idx in 0..3 {
            let started = base + ChronoDuration::seconds(idx);
            let finished = started + ChronoDuration::milliseconds(10);
            let output = format!("run-{idx}");

            let success = persist_job_result(&config, &job, true, &output, started, finished).await;
            assert!(success.success);
        }

        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].output.as_deref(), Some("run-2"));
        assert_eq!(runs[1].output.as_deref(), Some("run-1"));

        let updated = cron::get_job(&config, &job.id).unwrap();
        assert_eq!(updated.last_status.as_deref(), Some("ok"));
        assert_eq!(updated.last_output.as_deref(), Some("run-2"));
        assert!(updated.last_run.is_some());
    }

    #[tokio::test]
    async fn persist_job_result_rolls_back_run_history_when_job_state_update_fails() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
        let original_next_run = job.next_run;
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let conn =
            rusqlite::Connection::open(config.data_dir.join("cron").join("jobs.db")).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_cron_job_update
             BEFORE UPDATE ON cron_jobs
             BEGIN
                 SELECT RAISE(ABORT, 'blocked update');
             END;",
        )
        .unwrap();
        drop(conn);

        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;

        assert!(success.success);
        assert!(cron::list_runs(&config, &job.id, 10).unwrap().is_empty());

        let stored = cron::get_job(&config, &job.id).unwrap();
        assert_eq!(stored.next_run, original_next_run);
        assert!(stored.last_run.is_none());
        assert!(stored.last_status.is_none());
        assert!(stored.last_output.is_none());
    }

    #[tokio::test]
    async fn persist_job_result_success_deletes_one_shot() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_agent_job(
            &config,
            TEST_AGENT,
            Some("one-shot".into()),
            crate::cron::Schedule::At { at },
            "Hello",
            SessionTarget::Isolated,
            None,
            None,
            true,
            None,
            true,
        )
        .unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success.success);
        let lookup = cron::get_job(&config, &job.id);
        assert!(lookup.is_err());
    }

    #[tokio::test]
    async fn persist_job_result_failure_disables_one_shot() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_agent_job(
            &config,
            TEST_AGENT,
            Some("one-shot".into()),
            crate::cron::Schedule::At { at },
            "Hello",
            SessionTarget::Isolated,
            None,
            None,
            true,
            None,
            true,
        )
        .unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, false, "boom", started, finished).await;
        assert!(!success.success);
        let updated = cron::get_job(&config, &job.id).unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.last_status.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn persist_job_result_uses_one_write_connection_for_failed_one_shot_disable() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_agent_job(
            &config,
            "test-agent",
            Some("one-shot".into()),
            crate::cron::Schedule::At { at },
            "Hello",
            SessionTarget::Isolated,
            None,
            None,
            true,
            None,
            true,
        )
        .unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        crate::cron::store::reset_write_connection_count_for_tests(&config);
        let success = persist_job_result(&config, &job, false, "boom", started, finished).await;

        assert!(!success.success);
        assert_eq!(
            crate::cron::store::write_connection_count_for_tests(&config),
            1
        );
    }

    #[tokio::test]
    async fn persist_job_result_falls_back_to_state_update_when_history_prune_fails() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.scheduler.max_run_history = 1;
        let job = cron::add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
        let original_next_run = job.next_run;
        let seed_started = Utc::now() - ChronoDuration::minutes(20);
        let seed_finished = seed_started + ChronoDuration::milliseconds(10);
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let conn =
            rusqlite::Connection::open(config.data_dir.join("cron").join("jobs.db")).unwrap();
        conn.execute(
            "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                job.id,
                seed_started.to_rfc3339(),
                seed_finished.to_rfc3339(),
                "seed",
                "seed",
                10,
            ],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_cron_run_prune
             BEFORE DELETE ON cron_runs
             BEGIN
                 SELECT RAISE(ABORT, 'blocked prune');
             END;",
        )
        .unwrap();
        drop(conn);

        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success.success);

        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "seed");

        let updated = cron::get_job(&config, &job.id).unwrap();
        assert_eq!(updated.last_status.as_deref(), Some("ok"));
        assert_eq!(updated.last_output.as_deref(), Some("ok"));
        assert!(updated.last_run.is_some());
        assert!(updated.next_run >= original_next_run);
    }

    #[tokio::test]
    async fn persist_job_result_falls_back_to_disable_when_auto_delete_history_insert_fails() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job =
            cron::add_once_at(&config, "test-agent", at, "echo one-shot-shell", None).unwrap();
        assert!(job.delete_after_run);
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let conn =
            rusqlite::Connection::open(config.data_dir.join("cron").join("jobs.db")).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_cron_run_insert
             BEFORE INSERT ON cron_runs
             BEGIN
                 SELECT RAISE(ABORT, 'blocked insert');
             END;",
        )
        .unwrap();
        drop(conn);

        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success.success);

        let updated = cron::get_job(&config, &job.id).unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.last_status.as_deref(), Some("ok"));
        assert_eq!(updated.last_output.as_deref(), Some("ok"));
        assert!(cron::list_runs(&config, &job.id, 10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn persist_job_result_success_deletes_one_shot_shell_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job =
            cron::add_once_at(&config, "test-agent", at, "echo one-shot-shell", None).unwrap();
        assert!(job.delete_after_run);
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success.success);
        let lookup = cron::get_job(&config, &job.id);
        assert!(lookup.is_err());
    }

    #[tokio::test]
    async fn persist_job_result_failure_disables_one_shot_shell_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job =
            cron::add_once_at(&config, "test-agent", at, "echo one-shot-shell", None).unwrap();
        assert!(job.delete_after_run);
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, false, "boom", started, finished).await;
        assert!(!success.success);
        let updated = cron::get_job(&config, &job.id).unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.last_status.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn persist_job_result_unacknowledged_delivery_preserves_execution_status() {
        register_recording_delivery_fn();
        // Unit success preserves legacy execution status, but proves no ack.
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_agent_job(
            &config,
            TEST_AGENT,
            Some("announce-job".into()),
            crate::cron::Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "deliver this",
            SessionTarget::Isolated,
            None,
            Some(DeliveryConfig {
                mode: "announce".into(),
                channel: Some("telegram".into()),
                to: Some("123456".into()),
                thread_id: None,
                reply_to: None,
                best_effort: false,
            }),
            false,
            None,
            true,
        )
        .unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success.success);

        let updated = cron::get_job(&config, &job.id).unwrap();
        assert!(updated.enabled);
        assert_eq!(updated.last_status.as_deref(), Some("ok"));

        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "ok");
    }

    #[tokio::test]
    async fn persist_job_result_delivery_failure_best_effort_marks_degraded() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        register_recording_delivery_fn();
        let mut job = cron::add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
        job.delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("fail-delivery".into()),
            to: Some("123456".into()),
            thread_id: None,
            reply_to: None,
            best_effort: true,
        };
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success.success);

        let updated = cron::get_job(&config, &job.id).unwrap();
        assert!(updated.enabled);
        assert_eq!(updated.last_status.as_deref(), Some("degraded"));
        assert!(
            updated
                .last_output
                .as_deref()
                .unwrap_or_default()
                .contains("delivery failed:")
        );

        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "degraded");
    }

    #[tokio::test]
    async fn delivery_failure_classification_preserves_empty_output_evidence() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        register_recording_delivery_fn();
        let mut job = cron::add_job(&config, "test-agent", "*/5 * * * *", "echo ok").unwrap();
        job.delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("fail-delivery".into()),
            to: Some("123456".into()),
            thread_id: None,
            reply_to: None,
            best_effort: true,
        };

        let outcome = deliver_and_classify_run_result(
            &config,
            &job,
            true,
            String::new(),
            CronDeliveryContext::Scheduled,
        )
        .await;

        assert!(outcome.success);
        assert_eq!(outcome.status, "degraded");
        assert!(outcome.output.starts_with("delivery failed:"));
    }

    #[tokio::test]
    async fn persist_job_result_at_schedule_without_delete_after_run_is_disabled() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_agent_job(
            &config,
            TEST_AGENT,
            Some("at-no-autodelete".into()),
            crate::cron::Schedule::At { at },
            "Hello",
            SessionTarget::Isolated,
            None,
            None,
            false,
            None,
            true,
        )
        .unwrap();
        assert!(!job.delete_after_run);

        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);
        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success.success);

        // After reschedule_after_run, At schedule jobs should be disabled
        // to prevent re-execution with a past next_run timestamp.
        let updated = cron::get_job(&config, &job.id).unwrap();
        assert!(
            !updated.enabled,
            "At schedule job should be disabled after execution via reschedule"
        );
        assert_eq!(updated.last_status.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn deliver_if_configured_handles_none_mode() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("echo ok");

        // Default delivery mode is not "announce", so should be a no-op.
        assert!(
            deliver_if_configured(&config, &job, "x", DELIVERY_FN.get())
                .await
                .is_ok()
        );
    }

    static DELIVERED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Channel name the recorder counts. Used only by the suppression test.
    const COUNT_CHANNEL: &str = "count-delivery";

    fn register_recording_delivery_fn() {
        // Idempotent: register_delivery_fn is a no-op once the OnceLock is set,
        // so repeated calls across tests are safe and the first writer wins. The
        // handler honours the `fail-delivery` failure contract used by the
        // delivery-classification tests so it composes regardless of order.
        register_delivery_fn(Box::new(|_config, channel, _target, _thread, _output| {
            Box::pin(async move {
                if channel == "fail-delivery" {
                    anyhow::bail!("synthetic delivery failure");
                }
                if channel == COUNT_CHANNEL {
                    DELIVERED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                Ok(())
            })
        }));
    }

    fn announce_job() -> CronJob {
        let mut job = test_job("echo ok");
        job.delivery = DeliveryConfig {
            mode: "announce".to_string(),
            channel: Some(COUNT_CHANNEL.to_string()),
            to: Some("chat-id".to_string()),
            thread_id: None,
            reply_to: None,
            best_effort: true,
        };
        job
    }

    #[tokio::test]
    async fn deliver_if_configured_suppresses_no_reply_but_delivers_real_and_failure() {
        register_recording_delivery_fn();
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = announce_job();
        use std::sync::atomic::Ordering::SeqCst;

        // Quiet sentinel forms must NOT trigger delivery.
        for quiet in [
            "NO_REPLY",
            "NO_REPLY: nothing to report",
            "NO_REPLY[INFO]: healthy",
        ] {
            let before = DELIVERED.load(SeqCst);
            deliver_if_configured(&config, &job, quiet, DELIVERY_FN.get())
                .await
                .unwrap();
            assert_eq!(
                DELIVERED.load(SeqCst),
                before,
                "quiet sentinel {quiet:?} must be suppressed (no delivery)"
            );
        }

        // Real content must be delivered.
        let before = DELIVERED.load(SeqCst);
        deliver_if_configured(&config, &job, "All systems nominal", DELIVERY_FN.get())
            .await
            .unwrap();
        assert_eq!(
            DELIVERED.load(SeqCst),
            before + 1,
            "real content must be delivered"
        );

        // Failure / refusal kinds must be delivered (operator-visible).
        for visible in [
            "NO_REPLY[FAIL]: database check timed out",
            "NO_REPLY[REFUSE]: policy prevented the check",
        ] {
            let before = DELIVERED.load(SeqCst);
            deliver_if_configured(&config, &job, visible, DELIVERY_FN.get())
                .await
                .unwrap();
            assert_eq!(
                DELIVERED.load(SeqCst),
                before + 1,
                "failure/refusal kind {visible:?} must be delivered, not suppressed"
            );
        }
    }

    #[test]
    fn heartbeat_announce_decision_matches_worker_behavior() {
        // NO_REPLY heartbeat: suppressed.
        assert!(!announce_delivery_decision("NO_REPLY").should_deliver());
        assert!(!announce_delivery_decision("NO_REPLY[INFO]: all good").should_deliver());
        // Non-sentinel heartbeat output: delivered.
        assert!(announce_delivery_decision("disk usage 42%").should_deliver());
        // Empty-output fallback string the worker builds: must deliver.
        assert!(
            announce_delivery_decision("💓 heartbeat task completed: db health").should_deliver(),
            "the empty-output heartbeat fallback must never be mistaken for a sentinel"
        );
        // Failure/refusal kinds: delivered (operator-visible).
        assert!(announce_delivery_decision("NO_REPLY[FAIL]: db timed out").should_deliver());
        assert!(announce_delivery_decision("NO_REPLY[REFUSE]: blocked by policy").should_deliver());
    }

    fn evidence_handler(outcome: EffectOutcome, fail: bool) -> DeliveryFn {
        Box::new(move |_, _, _, _, _| {
            Box::pin(async move {
                if fail {
                    return Err(DeliveryFailure {
                        outcome,
                        chunk_index: 1,
                        total_chunks: 2,
                        confirmed_chunks: 1,
                    }
                    .into());
                }
                zeroclaw_api::delivery::record_summary(DeliverySummary {
                    outcome,
                    confirmed_chunks: 1,
                    total_chunks: 1,
                });
                Ok(())
            })
        })
    }

    #[tokio::test]
    async fn notification_evidence_is_independent_of_execution_and_best_effort() {
        let config = Config::default();
        let job = announce_job();
        let confirmed = evidence_handler(EffectOutcome::Confirmed, false);
        let uncertain = evidence_handler(EffectOutcome::ReconciliationRequired, true);
        // Concurrent calls have separate receipt scopes. A failed execution can
        // have a confirmed notification; best-effort never confirms a lost ack.
        let (failed_job, successful_job) = tokio::join!(
            deliver_and_classify_with_handler(
                &config,
                &job,
                false,
                "execution failed".into(),
                CronDeliveryContext::Scheduled,
                Some(&confirmed)
            ),
            deliver_and_classify_with_handler(
                &config,
                &job,
                true,
                "execution done".into(),
                CronDeliveryContext::Scheduled,
                Some(&uncertain)
            ),
        );
        assert!(!failed_job.success);
        assert_eq!(failed_job.delivery_outcome, Some(EffectOutcome::Confirmed));
        assert!(
            successful_job.success,
            "legacy best-effort status is preserved"
        );
        assert_eq!(successful_job.status, "degraded");
        assert_eq!(
            successful_job.delivery_outcome,
            Some(EffectOutcome::ReconciliationRequired)
        );
        assert_eq!(
            occurrence_delivery_state(failed_job.delivery_outcome),
            "confirmed"
        );
        assert_eq!(
            occurrence_delivery_state(successful_job.delivery_outcome),
            "reconciliation_required"
        );
    }

    #[tokio::test]
    async fn notification_without_ack_is_uncertain_and_suppression_is_not_delivery() {
        let config = Config::default();
        let job = announce_job();
        let unit_handler: DeliveryFn = Box::new(|_, _, _, _, _| Box::pin(async { Ok(()) }));
        let unacknowledged = deliver_and_classify_with_handler(
            &config,
            &job,
            true,
            "done".into(),
            CronDeliveryContext::Scheduled,
            Some(&unit_handler),
        )
        .await;
        assert_eq!(
            unacknowledged.delivery_outcome,
            Some(EffectOutcome::PossiblyApplied)
        );
        let suppressed = deliver_and_classify_with_handler(
            &config,
            &job,
            true,
            "NO_REPLY".into(),
            CronDeliveryContext::Scheduled,
            None,
        )
        .await;
        assert_eq!(suppressed.delivery_outcome, None);
        assert!(suppressed.success);
        let missing = deliver_and_classify_with_handler(
            &config,
            &job,
            true,
            "done".into(),
            CronDeliveryContext::Scheduled,
            None,
        )
        .await;
        assert_eq!(missing.delivery_outcome, Some(EffectOutcome::NotStarted));
        assert_eq!(missing.status, "degraded");
    }

    #[test]
    fn empty_or_inconsistent_summary_cannot_confirm_notification() {
        for (confirmed_chunks, total_chunks) in [(0, 0), (0, 1), (2, 1)] {
            assert_eq!(
                notification_evidence(Some(DeliverySummary {
                    outcome: EffectOutcome::Confirmed,
                    confirmed_chunks,
                    total_chunks,
                })),
                EffectOutcome::PossiblyApplied
            );
        }
        assert_eq!(
            notification_evidence(Some(DeliverySummary {
                outcome: EffectOutcome::Confirmed,
                confirmed_chunks: 1,
                total_chunks: 2,
            })),
            EffectOutcome::PartiallyApplied
        );
    }

    #[tokio::test]
    async fn deliver_announcement_without_handler_is_typed_not_started() {
        let error = deliver_announcement_with_handler(
            None,
            &Config::default(),
            "telegram",
            "synthetic-chat",
            None,
            "payload",
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<DeliveryFailure>().unwrap().outcome,
            EffectOutcome::NotStarted
        );
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn build_cron_shell_command_uses_configured_runtime() {
        let config = Config::default();
        let workspace = std::env::temp_dir();
        let cmd = build_configured_shell_command(&config, "echo cron-test", &workspace).unwrap();
        let debug = format!("{cmd:?}");
        assert!(debug.contains("echo cron-test"));
        assert!(debug.contains("\"sh\""), "should use sh: {debug}");
        // Must NOT use login shell (-l) — login shells load full profile
        // and are slow/unpredictable for cron jobs.
        assert!(
            !debug.contains("\"-lc\""),
            "must not use login shell: {debug}"
        );
    }

    #[tokio::test]
    #[cfg(not(target_os = "windows"))]
    async fn build_cron_shell_command_executes_successfully() {
        let config = Config::default();
        let workspace = std::env::temp_dir();
        let mut cmd = build_configured_shell_command(&config, "echo cron-ok", &workspace).unwrap();
        let output = cmd.output().await.unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("cron-ok"));
    }

    #[tokio::test]
    #[cfg(all(unix, not(target_os = "android")))]
    async fn build_cron_shell_command_executes_with_custom_native_shell() {
        let tmp = TempDir::new().unwrap();
        let shim = tmp.path().join("cron-shell-shim");
        // Avoid writing an executable after the test process is multithreaded:
        // a concurrently forked child can inherit the write descriptor and
        // make the subsequent exec fail with ETXTBSY.
        let shell = which::which("sh").unwrap();
        std::os::unix::fs::symlink(shell, &shim).unwrap();

        let mut config = Config::default();
        config.runtime.shell = Some(shim.to_string_lossy().into_owned());
        let mut cmd = build_configured_shell_command(
            &config,
            "printf 'CUSTOM_SHELL:%s\\n' \"$0\"",
            tmp.path(),
        )
        .unwrap();
        let output = cmd.output().await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(output.status.success());
        assert_eq!(stdout.trim(), format!("CUSTOM_SHELL:{}", shim.display()));
    }

    #[test]
    fn build_cron_shell_command_preserves_docker_runtime_boundary() {
        let mut config = Config::default();
        config.runtime.kind = RuntimeKind::Docker;
        config.runtime.docker.image = "alpine:3.20".into();
        config.runtime.docker.network = "none".into();
        config.runtime.docker.mount_workspace = false;

        let cmd =
            build_configured_shell_command(&config, "echo cron-docker", &std::env::temp_dir())
                .unwrap();
        let debug = format!("{cmd:?}");

        assert!(debug.contains("\"docker\""), "{debug}");
        assert!(debug.contains("\"run\""), "{debug}");
        assert!(debug.contains("\"--network\""), "{debug}");
        assert!(debug.contains("\"none\""), "{debug}");
        assert!(debug.contains("\"alpine:3.20\""), "{debug}");
        assert!(
            debug.contains("\"sh\" \"-c\" \"echo cron-docker\""),
            "{debug}"
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn build_cron_shell_command_uses_configured_powershell() {
        let mut config = Config::default();
        config.runtime.shell = Some("powershell".into());
        let workspace = std::env::temp_dir();
        let cmd =
            build_configured_shell_command(&config, "Write-Output cron-ok", &workspace).unwrap();
        let debug = format!("{cmd:?}");
        assert!(debug.contains("powershell"));
        assert!(debug.contains("-Command"));
        assert!(!debug.contains("cmd.exe"));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn cron_powershell_policy_accepts_read_only_and_rejects_expressions() {
        let mut config = Config::default();
        config.runtime.shell = Some("powershell".into());
        // PowerShell-only command names are deliberately absent from the
        // cross-dialect default allowlist (see
        // `docs/book/src/security/sandboxing.md`): an operator opts into the
        // cmdlets they need. Grant both documented spellings so the assertions
        // below exercise the PowerShell grammar rather than the allowlist.
        let security = SecurityPolicy {
            allowed_commands: vec!["Write-Output".into(), "echo".into()],
            ..SecurityPolicy::default()
        };
        let runtime = crate::platform::create_runtime(&config.runtime).unwrap();

        crate::cron::validate_shell_command_with_security(
            runtime.as_ref(),
            &security,
            "Write-Output $PSHOME",
            false,
        )
        .expect("documented read-only PowerShell command should pass");
        assert!(
            crate::cron::validate_shell_command_with_security(
                runtime.as_ref(),
                &security,
                "echo ([System.IO.File]::Delete('important.txt'))",
                false,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn catch_up_queries_all_overdue_jobs_ignoring_max_tasks() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.scheduler.max_tasks = 1; // limit normal polling to 1

        // Create 3 jobs with "every minute" schedule
        for i in 0..3 {
            let _ = cron::add_job(
                &config,
                "test-agent",
                "* * * * *",
                &format!("echo catchup-{i}"),
            )
            .unwrap();
        }

        // Verify normal due_jobs is limited to max_tasks=1
        let far_future = Utc::now() + ChronoDuration::days(1);
        let due = cron::due_jobs(&config, far_future).unwrap();
        assert_eq!(due.len(), 1, "due_jobs must respect max_tasks");

        // all_overdue_jobs ignores the limit
        let overdue = cron::all_overdue_jobs(&config, far_future).unwrap();
        assert_eq!(overdue.len(), 3, "all_overdue_jobs must return all");
    }

    #[test]
    fn startup_policy_uses_live_declarative_owner_and_global_default() {
        let mut config = Config::default();
        let mut job = test_job("echo synthetic");
        for catch_up in [false, true] {
            config.scheduler.catch_up_on_startup = catch_up;
            let expected = if catch_up {
                CronMissedRunPolicy::CatchUpOnce
            } else {
                CronMissedRunPolicy::Skip
            };
            job.source = "declarative".into();
            config.cron.insert(job.id.clone(), CronJobDecl::default());
            assert_eq!(missed_run_policy(&config, &job), expected);
            for policy in [
                CronMissedRunPolicy::CatchUpOnce,
                CronMissedRunPolicy::Skip,
                CronMissedRunPolicy::Reconcile,
            ] {
                config.cron.get_mut(&job.id).unwrap().missed_run_policy = Some(policy);
                assert_eq!(missed_run_policy(&config, &job), policy);
            }
            job.source = "imperative".into();
            assert_eq!(
                missed_run_policy(&config, &job),
                expected,
                "an alias collision cannot borrow config"
            );
        }
    }

    #[tokio::test]
    async fn imperative_policy_startup_partitions_and_never_resets_review() {
        for catch_up in [false, true] {
            let tmp = TempDir::new().unwrap();
            let mut config = test_config(&tmp).await;
            config.scheduler.catch_up_on_startup = catch_up;
            let now = Utc::now();
            let mut jobs = Vec::new();
            for policy in ["catch_up_once", "skip", "reconcile"] {
                let job = cron::add_job(
                    &config,
                    "test-agent",
                    "* * * * *",
                    "echo synthetic-never-executed",
                )
                .unwrap();
                let patch = serde_json::from_value(serde_json::json!({"missed_run_policy":policy}))
                    .unwrap();
                cron::update_job(&config, &job.id, patch).unwrap();
                jobs.push(job);
            }
            let conn = rusqlite::Connection::open(config.data_dir.join("cron/jobs.db")).unwrap();
            conn.execute(
                "UPDATE cron_jobs SET next_run=?1",
                [(now - ChronoDuration::hours(1)).to_rfc3339()],
            )
            .unwrap();
            let ready = prepare_startup_jobs(&config, now).unwrap();
            assert_eq!(
                ready.iter().map(|job| &job.id).collect::<Vec<_>>(),
                vec![&jobs[0].id]
            );
            let skipped = cron::get_job(&config, &jobs[1].id).unwrap();
            assert_eq!(skipped.last_status.as_deref(), Some("skipped"));
            assert!(skipped.next_run > now);
            let reviewed = cron::get_job(&config, &jobs[2].id).unwrap();
            assert_eq!(reviewed.last_status.as_deref(), Some("uncertain"));
            assert!(!reviewed.enabled);
            cron::update_job(
                &config,
                &reviewed.id,
                serde_json::from_value(
                    serde_json::json!({"enabled":true,"missed_run_policy":"catch_up_once"}),
                )
                .unwrap(),
            )
            .unwrap();
            clear_stale_locks(&config).unwrap();
            let again = prepare_startup_jobs(&config, now).unwrap();
            assert_eq!(
                again.iter().map(|job| &job.id).collect::<Vec<_>>(),
                vec![&jobs[0].id]
            );
            assert!(!cron::get_job(&config, &reviewed.id).unwrap().enabled);
            for job in &jobs {
                assert!(cron::list_runs(&config, &job.id, 10).unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn startup_policies_partition_jobs_and_preserve_quarantine_after_resync() {
        for catch_up in [false, true] {
            let tmp = TempDir::new().unwrap();
            let mut config = test_config(&tmp).await;
            config.scheduler.catch_up_on_startup = catch_up;
            config.scheduler.max_tasks = 1;
            let now = Utc::now();
            for (id, policy) in [
                ("catch", CronMissedRunPolicy::CatchUpOnce),
                ("skip", CronMissedRunPolicy::Skip),
                ("review", CronMissedRunPolicy::Reconcile),
            ] {
                config
                    .agents
                    .get_mut("test-agent")
                    .unwrap()
                    .cron_jobs
                    .push(id.into());
                config.cron.insert(
                    id.into(),
                    CronJobDecl {
                        command: Some("echo synthetic-never-executed".into()),
                        schedule: CronScheduleDecl::At {
                            at: (now - ChronoDuration::hours(1)).to_rfc3339(),
                        },
                        missed_run_policy: Some(policy),
                        ..Default::default()
                    },
                );
            }
            sync_declarative_jobs(&config, &config.cron).unwrap();
            let ready = prepare_startup_jobs(&config, now).unwrap();
            assert_eq!(
                ready.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(),
                vec!["catch"]
            );
            for id in ["catch", "skip", "review"] {
                assert!(cron::list_runs(&config, id, 10).unwrap().is_empty());
            }
            assert!(!cron::get_job(&config, "skip").unwrap().enabled);
            assert_eq!(
                cron::get_job(&config, "review")
                    .unwrap()
                    .last_status
                    .as_deref(),
                Some("uncertain")
            );
            sync_declarative_jobs(&config, &config.cron).unwrap();
            clear_stale_locks(&config).unwrap();
            let after = prepare_startup_jobs(&config, now).unwrap();
            assert_eq!(
                after.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(),
                vec!["catch"]
            );
            assert!(!claim_job(&config, "review", now).unwrap());
            assert!(!claim_job(&config, "skip", now).unwrap());
        }
    }

    #[tokio::test]
    async fn startup_policy_storage_failure_does_not_return_work_for_execution() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.scheduler.catch_up_on_startup = false;
        let now = Utc::now();
        let job = cron::store::add_shell_job(
            &config,
            "test-agent",
            None,
            Schedule::At {
                at: now + ChronoDuration::hours(1),
            },
            "echo synthetic",
            None,
        )
        .unwrap();
        assert!(claim_job(&config, &job.id, now).unwrap());
        release_job(&config, &job.id).unwrap();
        assert!(
            prepare_startup_jobs(&config, now + ChronoDuration::hours(2)).is_err(),
            "an existing receipt must abort startup skip"
        );
        assert!(cron::get_job(&config, &job.id).unwrap().enabled);
        assert!(cron::list_runs(&config, &job.id, 10).unwrap().is_empty());
    }

    // scan_and_redact_output tests moved to zeroclaw-channels orchestrator

    // ── Broadcast / EventBroadcast tests ─────────────────────────────

    #[tokio::test]
    async fn broadcast_sends_cron_result_on_success() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        let job = cron::add_job(&config, "test-agent", "* * * * *", "echo broadcast-ok").unwrap();
        let job_id = job.id.clone();
        // Bind the synthetic test job to test-agent so process_due_jobs's
        // owning-agent lookup succeeds (jobs without an owner are skipped).
        config
            .agents
            .get_mut("test-agent")
            .unwrap()
            .cron_jobs
            .push(job.id.clone());
        let component = unique_component("broadcast-ok");

        let (tx, mut rx) = tokio::sync::broadcast::channel::<serde_json::Value>(16);
        let event_tx: EventBroadcast = Some(tx);

        assert!(claim_job(&config, &job.id, Utc::now()).unwrap());
        process_due_jobs(&config, vec![job], &component, &event_tx).await;

        let event = rx.try_recv().expect("should receive a broadcast event");
        assert_eq!(event["type"], "cron_result");
        assert_eq!(event["job_id"], job_id);
        assert_eq!(event["success"], true);
        assert!(event["output"].as_str().unwrap().contains("broadcast-ok"));
        assert!(event["timestamp"].as_str().is_some());
    }

    #[tokio::test]
    async fn broadcast_sends_cron_result_on_failure() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        let job = test_job("ls definitely_missing_file_for_broadcast_fail_test");
        config
            .agents
            .get_mut("test-agent")
            .unwrap()
            .cron_jobs
            .push(job.id.clone());
        let component = unique_component("broadcast-fail");

        let (tx, mut rx) = tokio::sync::broadcast::channel::<serde_json::Value>(16);
        let event_tx: EventBroadcast = Some(tx);

        process_due_jobs(&config, vec![job], &component, &event_tx).await;

        let event = rx.try_recv().expect("should receive a broadcast event");
        assert_eq!(event["type"], "cron_result");
        assert_eq!(event["job_id"], "test-job");
        assert_eq!(event["success"], false);
        assert!(event["timestamp"].as_str().is_some());
    }

    #[tokio::test]
    async fn claim_due_jobs_skips_in_flight_job() {
        // once a due job is claimed for execution, a
        // subsequent selection pass must not pick it up again until the prior
        // run releases it — otherwise a job that runs longer than the poll
        // interval is launched repeatedly.
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_job(&config, TEST_AGENT, "*/5 * * * *", "echo ok").unwrap();

        let claimed = claim_due_jobs(&config, vec![job.clone()]);
        assert_eq!(claimed.len(), 1, "first selection claims the job");

        let claimed_again = claim_due_jobs(&config, vec![job.clone()]);
        assert!(
            claimed_again.is_empty(),
            "an in-flight job must be skipped by the next selection pass"
        );

        cron::release_job(&config, &job.id).unwrap();
        let after_release = claim_due_jobs(&config, vec![job]);
        assert_eq!(
            after_release.len(),
            1,
            "after release the job is selectable again"
        );
    }

    #[tokio::test]
    async fn process_due_jobs_releases_lock_for_skipped_orphan_job() {
        // A job claimed for execution but then skipped by process_due_jobs (here
        // an orphan with no owning agent) must have its in-flight lock released,
        // so it is retried on the next poll instead of being wedged out of
        // due_jobs until restart
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        // Insert a real, claimable DB row under a configured agent, then drive
        // process_due_jobs with an in-memory view whose agent_alias is cleared.
        // With an empty alias and an id bound to no [agents.<x>].cron_jobs list,
        // resolve_owning_agent returns None, so the job is skipped as an orphan.
        let job = cron::add_job(&config, TEST_AGENT, "* * * * *", "echo orphan").unwrap();
        assert!(cron::claim_job(&config, &job.id, Utc::now()).unwrap());
        let orphan = CronJob {
            agent_alias: String::new(),
            ..job.clone()
        };

        process_due_jobs(&config, vec![orphan], &unique_component("orphan"), &None).await;

        assert!(
            cron::claim_job(&config, &job.id, Utc::now()).unwrap(),
            "a skipped orphan job's in-flight lock must be released, not leaked"
        );
    }

    #[tokio::test]
    async fn broadcast_none_skips_without_error() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("echo no-broadcast");
        let component = unique_component("broadcast-none");

        // event_tx = None — should complete without panic.
        process_due_jobs(&config, vec![job], &component, &None).await;
    }

    #[tokio::test]
    async fn broadcast_handles_no_subscribers() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("echo no-subscribers");
        let component = unique_component("broadcast-no-sub");

        let (tx, _) = tokio::sync::broadcast::channel::<serde_json::Value>(16);
        // Drop the only receiver immediately — `let _ = tx.send(...)` in
        // process_due_jobs must not panic when there are no subscribers.
        let event_tx: EventBroadcast = Some(tx);

        process_due_jobs(&config, vec![job], &component, &event_tx).await;
        // If we got here without panic, the test passes.
    }
}
