# Background work lifecycle

ZeroClaw has several ways to continue work after the inbound request that started it. Cron jobs, SOP runs, delegated tasks, and runtime-spawned subagents share some execution machinery, but they do not share one lifecycle or one durable store. Goal mode defines a related target contract that is not yet wired end to end.

Use this page when a change adds scheduled or autonomous work, introduces a wait or approval state, changes cancellation or restart behavior, or connects child work to an owning task. The first design question is not "how does it run in the background?" but "which subsystem owns its lifecycle?"

## Ownership map

| Work type | Current owner or status surface | Durable records |
| --- | --- | --- |
| Cron job | Cron scheduler and store | `data/cron/jobs.db` |
| SOP run | `SopEngine` and `SopRunStore` | Process memory by default; `data/sop/runs.db` when durable SQLite initialization succeeds |
| Background delegation | Delegate result API, with control-plane supervision overrides when available | `<workspace>/delegate_results/<task-id>.json`; a best-effort task row in `data/control_plane.db` under a booted daemon |
| Runtime-spawned subagent | Spawn site, with control-plane supervision when available | A best-effort task row in `data/control_plane.db` under a booted daemon |

Durable metadata is not the same as durable execution. A result file or task row can preserve what was known and let recovery mark work lost, timed out, or terminal without preserving the process-local future that was doing the work.

## Cron jobs

Cron combines declarative membership with a SQLite execution store. Runtime-created jobs and reconciled config jobs both carry an owning `agent_alias`; execution resolves that agent's security policy instead of running under an ambient daemon identity.

The scheduler polls for due, enabled, unclaimed rows. Each scheduled occurrence has a stable `(job_id, scheduled_at)` identity in the existing cron database. A durable claim precedes execution, and execution state is separate from notification evidence. Completion records bounded output, then reschedules a recurring job, deletes a successful auto-delete one-shot, or disables another one-shot. Occurrence receipts survive deletion of the job row.

Startup recovery quarantines interrupted jobs and notification submissions with missing acknowledgements. It preserves confirmed execution and delivery evidence; it does not replay uncertain effects. Recovery also covers unfinished notifications belonging to deleted one-shots. A storage failure during recovery or missed-run policy handling stops scheduler startup before polling can execute work.

Declarative jobs can set `cron.<alias>.missed_run_policy`:

| Value | Startup behavior |
| --- | --- |
| `catch_up_once` | Consider the oldest pending occurrence once, then use the ordinary completion schedule. Does not enumerate every missed interval. |
| `skip` | Persist a skipped occurrence, advance recurring jobs to a future time, and disable overdue one-shots. |
| `reconcile` | Persist an occurrence with execution and delivery both `not_started`, disable the job with an `uncertain` scheduling status, and require operator review. This does not assert that an external effect occurred. |

Omission inherits `scheduler.catch_up_on_startup`: `true` selects `catch_up_once`, `false` selects `skip`. Imperative jobs can override this default through `cron_update`, HTTP `PATCH /api/cron/{id}`, RPC `cron/patch`, or `zeroclaw cron update <id> --agent <owner> --missed-run-policy <value>`. Their existing cron row owns the optional override. JSON omission preserves it; explicit `null` restores inheritance (`inherit` in the CLI). Declarative policy is read from the owning config declaration, never copied into the scheduler database or borrowed by an imperative row with a colliding alias. Patches to a declarative job’s policy are rejected, including null. New imperative jobs inherit the default until patched. A policy change applies when startup next reads the job; it does not change an already admitted execution. Unknown stored policy values fail closed. It applies when the scheduler starts, including a daemon reload that recreates the scheduler. No running configuration is changed by adding this field to the schema.

Skip and reconciliation checkpoint the schedule change and occurrence evidence together, rejecting a stale imperative policy or changed owner. Job patches read and update under one SQLite writer transaction so they cannot overwrite a concurrent schedule update. They preserve prior execution timestamps and refuse to overwrite an existing claim or receipt. Config resync preserves quarantines and completed one-shot disablement. Changing policy or setting enabled does not authorize replay of an uncertain occurrence; no reconciliation-reset API is provided. An operator must inspect the occurrence evidence and arrange any genuinely new work separately.

Manual tool, gateway, and RPC triggers atomically claim a new occurrence and the existing per-job lock before executing. Admission re-reads the current job inside that transaction, rejects changed approval snapshots, and excludes competing manual or scheduled execution. These occurrences do not consume the job's scheduled timestamp. The cron database owns the invocation and its lock binding; no separate ledger is introduced.

Callers may supply a stable `request_id` to `cron_run` or RPC `cron/trigger`, or an `Idempotency-Key` header to the HTTP run endpoint. IDs must be 1–128 printable ASCII characters without spaces. Only a SHA-256 digest of the job, owning agent, and request ID is persisted as the invocation identity; reassigned ownership cannot retrieve a prior owner's keyed receipt. Repeating a key for the same job returns the original bounded receipt and typed execution/delivery outcomes without executing, notifying, or adding run history again. An unreadable keyed receipt reports reconciliation-required; a storage error is never evidence that the original invocation did not run. A caller that omits the key requests a distinct invocation; it must not blindly repeat a request after losing its response. Job ownership and existing approval checks still apply. A deleted job's receipt survives and is available through the read-only operator APIs described below.

Execution is checkpointed before notification. Receipt completion, run history, quarantine, and owner-checked lock release commit atomically. Notification absence is recorded as `not_requested`, distinct from an attempted notification that remained `not_started`; keyed replay preserves that distinction. Repeated completion cannot unlock a replacement invocation or duplicate history. A missing acknowledgement, cancelled submission, or unknown execution failure preserves evidence and requires reconciliation. Legacy `success`/`degraded` fields remain for compatibility, while `effect_outcome`, `execution_outcome`, and `delivery_outcome` carry the separate facts. Even a best-effort notification failure quarantines subsequent execution; notification repair must not rerun the task. These changes add the `lock_owner` column through the existing additive schema migration and require a new binary/restart; they do not modify a running database during validation.

Operators can inspect the existing occurrence ledger with HTTP `GET /api/cron/{id}/occurrences` or RPC `cron/occurrences`, including deleted-job receipts. Both use their existing authenticated administrative boundary. Output is omitted unless explicitly requested; indexed identity cursors and byte limits bound reads. Missing storage is distinct from unavailable or malformed storage, and unknown outcomes require reconciliation. The read-only connection performs no schema initialization and runs off async workers. It never releases locks, resets quarantines, or authorizes replay. See [Cron occurrence receipts](../gateway/api.md#cron-occurrence-receipts) for limits, privacy, and cursor semantics. Platform notification IDs and evidence-driven reconciliation remain separate work.

The scheduler checks its cancellation token between polling iterations, so shutdown still waits for the current due-job batch to finish. Cancelling it does not promise that an already-dispatched external effect can be rolled back.

Declarative agent jobs may set `cron.<alias>.timeout_secs` to an integer from 1 through 86400. Each agent attempt resolves this optional deadline from the current config declaration; the cron database does not own a copy. Omission preserves the existing behavior without a scheduler agent deadline. Imperative jobs, including rows whose IDs collide with a config alias, do not inherit it. Shell jobs retain their existing independent timeout.

The deadline drops the agent-run future and follows normal failure handling, including isolated-session memory cleanup, result persistence, and claim release. It is a per-attempt limit: `reliability.scheduler_retries` still controls retries, and setting it to `0` avoids repeated attempts for workflows with ambiguous external writes. Async cancellation cannot preempt code that never yields, roll back a completed request, or guarantee termination of an already-launched child process or detached task. Such tools need their own process/request bounds and idempotency or uncertain-write handling. Config edits apply through the existing scheduler reload boundary; deadlines are not copied into persistent jobs or a new live cache.

Declarative agent jobs can also set `completion_check` to a read-only shell command
that independently checks the work. It runs after every agent attempt, including
an agent error or timeout, using the owning agent's existing shell security policy
and a 30-second deadline. A nonzero exit, policy denial, spawn failure, or timeout
makes the attempt fail and follows normal failed-run cleanup and retry handling.
A successful check preserves the agent result, including a quiet `NO_REPLY`, and
never turns an agent error into success. The command is resolved from current
config; imperative jobs and shell jobs do not inherit it. Use
`reliability.scheduler_retries = 0` when repeating the preceding work could cause
ambiguous external writes. A check is evidence of the conditions it validates,
not proof of all side effects or the model's interpretation.

## SOP runs

SOP definitions live under the configured `sops` directory. `SopEngine` owns run progression, approval waits, checkpoints, terminal transitions, and the in-process status surface. `SopRunStore` is the concurrency source of truth when it admits and claims a run.

Run persistence is opt-in. With the default `sop.persist_runs = false`, the engine uses an in-memory store. When persistence is enabled, the default SQLite backend writes `runs.db` under `<data_dir>/sop` unless `run_state_dir` overrides it. Successful store initialization lets active snapshots, terminal records, events, revisions, and concurrency claims support restart restoration. If store initialization fails, the daemon logs a warning and falls back to the in-memory store.

SOP audit records in the Memory backend are a separate observability surface. They do not replace the run store and must not be used as the authority for whether a run is active, paused, approved, or terminal.

Approval and checkpoint states are durable control states only when the run store is durable. Timeout policy remains fail-closed by default: a timed-out approval escalates and keeps waiting unless config explicitly selects cancellation or the legacy auto-approve behavior.

## Delegation and subagents

Subagents inherit their parent's effective security boundary. Policy and memory overrides may narrow the parent envelope but cannot widen it, and child action accounting uses the parent's tracker so spawning children cannot bypass the parent's action budget.

The `spawn_subagent` path is synchronous: the parent waits for the child run to finish, and this path has no local timeout or background cancellation handle.

The delegate tool can run synchronously or start a background task and return a UUID. Background results are written atomically under the workspace passed to the tool and can be checked, listed, awaited in a batch, or cancelled. A live cancellation registry maps task IDs to process-local tokens; cancellation updates the persisted result and signals the running task when that live token is still available.

Under a booted daemon, delegate and subagent producers also write task rows to the durable control plane. These writes are best-effort and independent from delegate result-file writes. Delegate result reads remain file-first; only a file still marked `running` is overlaid as `lost` or `timed_out` from control-plane state, so the two records can diverge.

Current delegate and subagent rows populate agent, status, owner PID and boot ID, depth, and timestamps. They leave heartbeat, parent task, route, and principal absent. Startup recovery marks prior-boot running rows `lost`; `timed_out` applies only to producers that emit stale heartbeats, which these producers do not currently do. The task row makes an interrupted child visible but does not recreate its execution.

## Goal-mode target contract

[ADR-008](./decisions/ADR-008-goal-mode-control-plane-and-usage-accounting.md) accepts the task control plane as the future authority for goal lifecycle, ownership, route, principal, parent relation, and recovery eligibility. The repository contains goal storage and control-plane APIs, but production goal admission and execution are not yet wired end to end.

A background path may participate in goal mode only after it preserves the owning goal relationship and reports terminal state and model usage back to it. Until then, that path is ordinary background work rather than goal-mode execution.

## Change checklist

For background-work changes, answer these before reviewer sign-off:

- Which subsystem owns the lifecycle and which store is authoritative?
- Is the work process-local, durably supervised, or actually restart-resumable?
- Which token or control-plane action cancels it, and what can remain in flight?
- Which parent task, agent, route, principal, recursion depth, and usage fields does this path actually populate?
- Are waiting, approval, checkpoint, lost, timed-out, and terminal states distinguishable?
- Can startup recovery duplicate a side effect or silently strand a claim?
- Does result delivery remain idempotent if completion is observed after restart?

## Source pointers

- Cron scheduler and persistence: `crates/zeroclaw-runtime/src/cron/scheduler.rs`, `crates/zeroclaw-runtime/src/cron/store.rs`
- SOP engine and run stores: `crates/zeroclaw-runtime/src/sop/engine.rs`, `crates/zeroclaw-runtime/src/sop/store/`
- Delegation and subagent behavior: [Delegation & SubAgents](../agents/delegation.md), `crates/zeroclaw-runtime/src/tools/delegate.rs`, `crates/zeroclaw-runtime/src/tools/spawn_subagent.rs`, `crates/zeroclaw-runtime/src/subagent/mod.rs`
- Durable task control plane and recovery: `crates/zeroclaw-runtime/src/control_plane/`
- Goal-mode decision: [ADR-008](./decisions/ADR-008-goal-mode-control-plane-and-usage-accounting.md)
- SOP operator guide: [How SOPs run](../sop/how-it-works.md)
