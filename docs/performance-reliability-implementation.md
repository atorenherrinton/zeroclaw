# Performance and reliability implementation

## Scope and ownership

Local engineering work against the September 2026 performance/reliability backlog.
No external publication, outbound test communication, credential migration, or
changes to privacy/approval controls. Existing unrelated helper edits are excluded.

Canonical owners:

- Session database: chat history and new platform chunk acknowledgements. Chunk
  receipts create a new fact; they are not another operations outbox. Personal
  operations retain their existing write-ahead ledger unchanged.
- Control plane: durable task lifecycle/recovery. Do not add another turn database.
- Cron database: occurrence claims and execution results.
- Tool dispatcher: budgets and trusted execution evidence; source connectors own
  their own output caps.
- Messages database: read-only history; it never supplies authorization.

## Stages

1. Fix destructive Telegram finalization and stop ambiguous full-response replay;
   validate ordered chunks, partial failure, malformed acknowledgements, and crash
   after submission against a local fake Bot API.
2. Bound session history at the SQLite query and diagnostic tool boundaries;
   preserve explicit errors and keep SQLite work off async workers.
3. Audit/extend control-plane admission, conversation ordering, tool/context
   budgets, deadlines, provider cooldowns, scheduler durability, and observability.
4. Implement narrow read-only Messages history in the maintained Rust connector.
5. Run formatting, targeted and broad practical test/check/build suites. Review
   task-only diff and commit verified milestones on their local branch. Integrate
   onto local master only after full completion. No push.

## Delivery contract

Text finalization edits the existing Telegram draft into the first final chunk;
remaining chunks are submitted in order. Every send requires `ok=true` and a
positive platform message ID. HTTP 200 alone is insufficient. Formatting fallback
is permitted only after a definitive invalid-request rejection. Timeouts, missing
acknowledgements, cancellation, and 5xx responses never cause blind replay.

A scoped SQLite session backend commits a FULL-synchronous possibly-applied chunk
claim before network I/O, then stores the typed acknowledgement. Keys bind the
channel instance, exact destination/thread, inbound ID, immutable response content,
and chunk ordinal. A repeated exact response skips confirmed chunks, but an
existing unconfirmed claim requires reconciliation. Ledger failures fail closed.
The first chunk retains the continuation indicator when later chunks fail.

Telegram Bot API has no arbitrary bot-history read API or client idempotency key.
A lost send acknowledgement therefore cannot be automatically reconciled. Operator
inspection (or future independently authenticated platform evidence) is required;
content hashes and a regeneration are not proof of non-delivery. No retry/reset
API is exposed that could turn uncertainty into permission to send again.

Receipt limitations to close before claiming universal delivery durability:
non-SQLite/unpersisted sessions and unscoped direct sends lack durable inbound
identity; media and queued voice need their own receipt-capable adapter contract.
They must not be described as all-chunk-confirmed text delivery.

## Milestone and preserved checkpoint (2026-09-06)

This is **not a completed backlog or a deployment candidate**. The local milestone
contains Telegram text delivery/chunk receipts, bounded diagnostic history,
provider cooldown repairs, read-only Messages history and directly required test
repairs. The durable-turn, queue/deadline, scheduler, output-budget, schema-selection
and logging-default implementations remain in the **separate, uncommitted
checkpoint**. Rows referring to those files audit that preserved checkpoint; they
are not features shipped by the milestone commits. No partial milestone is merged
to `master`. The inherited
checkpoint compiles for channels/runtime/tools, including Telegram. Full acceptance
and restart safety remain incomplete. Status below refers to the whole numbered
backlog item, not merely the existence of code. Test names without a current result
are evidence to run, not a claim of success. The original numbering (1–23) is used;
the handoff groups the same work into 15 sections plus iMessage.

Ownership inspection found that **all** changes to
`tools/zeroclaw-personal-ops/src/install.rs` and
`tools/zeroclaw-personal-ops/templates/calendar.md` belong to unrelated Reminders
list deletion, in addition to the six Reminders-manager paths. All eight paths
are excluded from this task. `Cargo.lock` adds only `async-trait` to
`zeroclaw-infra`; it contains no mixed Reminders hunk.

For concise source references below, A = `crates/zeroclaw-api/src/`,
C = `crates/zeroclaw-channels/src/`, I = `crates/zeroclaw-infra/`,
R = `crates/zeroclaw-runtime/src/`, T = `crates/zeroclaw-tools/src/`,
and P = `tools/zeroclaw-personal-ops/`. These are path prefixes, not new owners.

| Item | Status | Exact source files | Tests / evidence | Limitation and deployment implication |
| --- | --- | --- | --- | --- |
| 1. Non-destructive Telegram finalization | prerequisite-only | C`telegram.rs`, C`telegram/delivery.rs`, A`delivery.rs` | C`telegram/delivery/tests.rs`: `oversized_5314_edits_first_and_sends_remainder_in_order`, `chunk_two_failure_keeps_first_and_does_not_retry_5xx`, malformed success/rate-limit, rejected-edit, reopen, and cancellation cases; 10 local fake-server fault tests passed in `milestone-affected-tests-final.log` | Text adapter keeps the draft; unscoped sends, media and voice lack the complete durable contract. Fault tests now include the actual request timeout, invalid/mismatched IDs, storage refusal, and strict rejection of fabricated acknowledgements from not-modified errors. Requires new binary/restart; no live testing. |
| 2. Durable turn journal | prerequisite-only | A`turn.rs`, C`orchestrator/turn_journal.rs`, C`orchestrator/mod.rs`, R`control_plane/task_registry.rs`, R`control_plane/task_store_sqlite.rs` | `duplicate_admission_and_restart_keep_input_without_replay`, `crash_in_each_active_phase_is_uncertain_and_terminal_cannot_be_overwritten`; both passed in the isolated checkpoint channels suite | Uses canonical control plane and v8 inputs table. Legal transitions are not enforced; admission is optional without global control plane, gateway/stop/event paths bypass it, queued recovery fails instead of resuming, response references and metadata are incomplete. Do not advertise universal durable admission. |
| 3. Ordered conversation execution | prerequisite-only | C`orchestrator/mod.rs` (`reserve_worker`, `dispatch_worker`, `InFlightTaskCompletion`) | `reservations_are_fifo_before_spawn_and_cancel_the_entire_queued_chain`, `completion_notification_is_safe_before_and_during_wait_registration`; both passed in the isolated checkpoint channels suite | Reservations precede spawn, successors wait before worker acquisition, queue is bounded at 8× workers. Full dispatch/restart/predecessor-failure proof and cancellation while waiting for a worker remain open. Debounce behavior is removed in the inherited diff and needs compatibility review. |
| 4. Idempotent/reconcilable delivery | prerequisite-only | A`delivery.rs`, I`src/session_delivery.rs`, I`src/session_sqlite.rs`, C`telegram/delivery.rs` | I`tests/bounded_history_delivery.rs` claim/reopen and concurrent-claim tests; Telegram reopen/cancellation tests; 7 infrastructure integration regressions and 232 infrastructure unit tests passed | FULL-synchronous claims protect exact response chunks. Registry currently permits completed-but-unconfirmed submissions. Milestone receipt finalization rejects missing acknowledgements, changed identity, and terminal downgrades; full turn-level delivery propagation remains deferred. Lost Telegram acknowledgements require operator evidence; no automatic history reconciliation exists. |
| 5. Stalled-turn watchdog | prerequisite-only | R`control_plane/reaper.rs`, C`orchestrator/mod.rs`, A`deadline.rs` | Existing `sweep_times_out_own_stale_task_but_not_fresh`; `nested_and_retry_work_cannot_extend_parent`; passed in isolated checkpoint API suite | Channel records have no heartbeat; reaper does not expire rows with absent heartbeat. Deadline drop is not a visible watchdog outcome, child cleanup or durable delegation. Needs phase timestamps and worker cancellation integration before deployment claims. |
| 6. Typed connector uncertainty | prerequisite-only | A`delivery.rs`, A`deadline.rs`, R`agent/tool_execution.rs`, R`tools/delegate.rs`, R`cron/scheduler.rs` | Telegram typed failures inspected; generic ToolResult and cron classification remain string/bool based | Effect enum exists at channel boundary only. Exceptions are flattened in tool/cron paths; no end-to-end structured reconciliation contract. Never enable a retry-uncertain-write route. |
| 7. Cursor/byte-bounded history | prerequisite-only | I`src/session_backend.rs`, I`src/session_sqlite.rs`, T`sessions.rs` | I`tests/bounded_history_delivery.rs`: cursor append isolation, Unicode and storage errors; I`src/session_sqlite.rs`: actual query-plan and locked-database tests; T`sessions.rs` history tests; 7 infrastructure integration regressions, 232 infrastructure units and 1,721 tool tests passed | SQLite tail/cursor and content caps exist, with blocking isolation in tool. Each indexed SQLite step receives only the remaining page byte budget; metadata and JSON envelope are outside content cap. Other backends explicitly unsupported. Does not replace runtime full-history loading. |
| 8. Layered tool-output budgets | prerequisite-only | T`output_budget.rs`, R`agent/turn/mod.rs`, R`agent/tool_execution.rs` | `unicode_round_budget_does_not_persist_unretained_private_data`, `outcome_and_request_id_survive_large_json_result`; passed in checkpoint tools suite (1,724 tests) | Latest inherited code explicitly disables resource persistence, contrary to early handoff report. Only excerpts and limited small top-level evidence survive; no complete receipt/scope/error preservation. Existing artifact writer needs scoped retention/access/reference ownership before reuse; no second store should be introduced. |
| 9. Intent-relevant schemas | prerequisite-only | T`schema_selection.rs`, R`agent/turn/tool_specs.rs`, R`agent/turn/mod.rs` | `explanation_omits_unrelated_connectors_and_retains_discovery`; passed in checkpoint tools suite (1,724 tests) | Only names containing `__` filtered on first iteration with discovery available. Later iterations expose all schemas; built-in writes remain present, follow-up selection is not task-aware. Bundle and write-intent acceptance cases incomplete. Presentation filtering is not authorization. |
| 10. Safe compaction | deferred | R`agent/history.rs`, R`agent/history_trim.rs`, R`agent/turn/history_window.rs` (existing) | Existing pairing tests in `history_trim.rs`; no new semantic-preservation implementation | Existing turn-boundary trimming is lossy. No structured unresolved questions, promises, authorizations, receipts and uncertain outcomes snapshot. Do not add semantic compaction until this state has canonical owners and retention tests. No claim of completion. |
| 11. Bounded turns/deadlines | prerequisite-only | A`deadline.rs`, C`orchestrator/mod.rs`, R`agent/turn/mod.rs`, R`tools/delegate.rs` | `nested_and_retry_work_cannot_extend_parent`; passed in isolated checkpoint API suite | Parent task-local deadline wraps channel/tool-loop/delegates; timeout phase absent, blocking subprocess/MCP cleanup and detached child lifetime unproven. No distinct verified background budget or automatic durable handoff. |
| 12. Provider retry/fallback | prerequisite-only | `crates/zeroclaw-providers/src/reliable.rs` | Prior log: 162 reliable tests; expanded 165-test reliable suite passed within 1,510 provider tests, 0 failed (8.87 s); `compute_backoff_does_not_shorten_retry_after` | Full Retry-After, monotonic cooldown and single-provider gate present. Cooldown belongs to each ReliableModelProvider instance, not global account state. Dedicated concurrent weaker-observation and single-provider tests caught and repaired missing cooldown creation in four non-streaming paths. Streaming-error cooldown recording, HTTP-date hints, and provider-fallback safety remain open. No credential/provider reconfiguration performed. |
| 13. Conflict-aware concurrency/backpressure | prerequisite-only | R`agent/tool_execution.rs`, C`orchestrator/mod.rs`, R`cron/scheduler.rs` | Parallel classification and reservation tests passed in checkpoint runtime/channels suites | Known stateless read batches use buffered(4), max 128 calls. Unknown/shell/UI writes serialize within a batch. Cross-turn exact resource locks and global interactive worker reservation absent; separate pools are not proof against resource starvation. |
| 14. Connection reuse/metadata caching | deferred | C`telegram.rs` (`http_client`), `crates/zeroclaw-providers/src/reliable.rs`, R`agent/turn/tool_specs.rs` | Existing client reuse inspected; schema byte telemetry added, no setup benchmark | No new pooling/cache introduced. Need connection-setup measurements, registry generation invalidation and account/catalog expiry before further optimization. |
| 15. Scheduler occurrence durability | prerequisite-only | R`cron/store.rs`, R`cron/scheduler.rs`, `tests/component/cron_delivery_cli.rs` | `clear_stale_locks_quarantines_interrupted_work`, `occurrence_survives_job_deletion_and_blocks_same_occurrence_replay`, scheduler retry tests; passed in checkpoint runtime suite (4,172 passed, 3 ignored) | Stable (job_id,next_run) claim exists, but bool success conflates execution and notification, and occurrence recovery/missed-run policies are incomplete. Manual paths and claim enablement need audit. Interrupted jobs are disabled/uncertain; operators must reconcile before future enablement. |
| 16. Graceful restart/readiness | deferred | C`orchestrator/mod.rs`, R`control_plane/reaper.rs`, R`daemon/mod.rs`, R`daemon/registry.rs` (existing) | Existing worker drain on receiver close and boot sweep inspected; no full process fault proof | No coordinated admission stop/checkpoint/readiness across gateway, listeners, connectors, scheduler and delegates. Safe queued replay not implemented. Do not restart a live daemon to test this work. |
| 17. Atomic state/config writes | prerequisite-only | `crates/zeroclaw-config/src/schema.rs` (`write_config_atomically_with_sync`), R`control_plane/task_store_sqlite.rs` | Existing config tests `config_save_atomic_cleanup` and sync/backup tests; new v8 migration needs dedicated tests | Existing config primitive uses temp/sync/rename/backup. Post-rename directory sync warning is not durable success. Other writers and new schema migrations not comprehensively audited or verified. Preserve private backups before eventual deployment. |
| 18. End-to-end traceability | prerequisite-only | C`orchestrator/turn_journal.rs`, C`orchestrator/mod.rs`, R`agent/turn/mod.rs`, R`agent/turn/tool_specs.rs`, C`telegram/delivery.rs` | Trace ID, chunk receipts, generation/submission events and schema/output byte telemetry inspected | Different runtime turn IDs are not proven unified across all phases. Queue/prompt/timeout phase metrics and terminal-event completeness missing. No payloads should enter metrics. |
| 19. Reliability objectives | deferred | C`orchestrator/mod.rs`, R`observability/prometheus.rs`, R`observability/runtime_trace.rs`, `crates/zeroclaw-log/src/broadcast.rs` (existing) | No new objective aggregation tests or deployed dashboard | Missing unanswered/duplicate/uncertain age/recovery/first-visible p50-p95 series and separate schedule execution/delivery rates. Instrument bounded metadata before claiming objectives. |
| 20. Connector/worker readiness | deferred | R`control_plane/reaper.rs`, R`daemon/mod.rs`, R`daemon/registry.rs`, R`health/mod.rs` (existing) | No end-to-end readiness test in inherited diff | No aggregate oldest queued/uncertain, scheduler lag, stopped workers, dropped events or connector readiness surface. Process liveness is insufficient. |
| 21. Bounded/rotating logs | prerequisite-only | R`agent/tool_execution.rs`, `crates/zeroclaw-config/src/schema.rs`, `crates/zeroclaw-log/src/config.rs` | Existing observability tests inspected; inherited rotating-default change is preserved outside milestone and needs its own config/log crate verification | Defaults switch to rotating 16 MiB with existing retention. Explicit rolling configs remain rolling. Some error log paths remain unbounded; scrubber sees full input before cap, temporary-trace cleanup not implemented. Requires restart/default adoption review. |
| 22. Replay/fault tests | prerequisite-only | C`telegram/delivery/tests.rs`, C`orchestrator/turn_journal.rs`, I`tests/bounded_history_delivery.rs`, P`src/imessage_history/tests.rs` | Local fake Bot API and synthetic SQLite only; 10 Telegram fault tests, 7 infrastructure regressions, 232 infrastructure unit tests and 55 personal-ops tests (11 history) passed | Missing whole-process model/tool termination, connector outage and restart with safely queued replay proof. Tests must assert visible/durable outcomes, not only helper transitions. |
| 23. SQLite maintenance | prerequisite-only | I`src/session_sqlite.rs`, I`src/session_delivery.rs`, T`sessions.rs`, P`src/imessage_history.rs` | 232 infrastructure unit tests, 7 cursor/claim regressions, 55 personal-ops tests passed; runtime full-history behavior remains unchanged | Bounded diagnostic reads and blocking adapters present. Most legacy history APIs still mask errors, runtime loads full history, low-activity WAL maintenance and raw-payload retention absent. No replacement proposed; measure first. |
| Adjacent: read-only iMessage history | completed | P`Cargo.toml`, P`src/lib.rs`, P`src/imessage_history.rs`, P`src/imessage_history/tests.rs` | `cargo test --locked --manifest-path tools/zeroclaw-personal-ops/Cargo.toml`: 55 passed, including 11 synthetic history cases, 0 failed; strict Clippy and release build passed (final commands: 4.07 s / 2.65 s / 9.98 s) | Exact GUID/ID, RFC3339 ≤31-day window, cursor, output byte caps, attachments metadata only. Attributed-body-only text explicitly unavailable. Outgoing sender attribution is explicit (self, no recipient mislabeling); corrupt text and oversized identity metadata fail explicitly. SQLite permission errors other than AUTH/PERM may still be classified storage_unavailable; no failure becomes no_results. No real Messages DB read; installation and policy registration remain separate owner actions. |

## Validation ledger

Logs and preservation evidence are outside the repository in the handoff audit
directory. No prior-run log is counted as current verification. Commands used
`CARGO_TARGET_DIR` pointing to the existing canonical repository build cache;
personal-ops uses its separate existing target directory. Times include Cargo
lock waiting where applicable.

| Command | Current result | Evidence |
| --- | --- | --- |
| `cargo check -p zeroclaw-api -p zeroclaw-infra --all-targets` (initial canonical) | Failed: ENOSPC in dependency compilation | `check-infra-initial.log` |
| `cargo check -p zeroclaw-channels -p zeroclaw-runtime -p zeroclaw-tools --all-targets --features zeroclaw-channels/channel-telegram` (preserved checkpoint) | Passed, 1m 07s | `check-checkpoint.log` |
| `cargo fmt --all -- --check` | Passed on final source; empty formatting diagnostics | `fmt-verified.log`, `personal-fmt-verified.log` |
| `cargo check --locked --workspace --all-targets` | Passed after adding the missing `reply_to` test-fixture field; final 214.91 s including Cargo lock waiting | `workspace-check-final.log` |
| `cargo test --locked --workspace --no-fail-fast` | 15,361 passed, 10 failed, 24 ignored; 510.43 s; six failed targets before boundary repairs | `workspace-tests.log` |
| `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` | Blocked at unchanged embedded-web build: missing generated `web/dist/index.html`; 44.40 s | `workspace-clippy.log` |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | Failed: three unchanged forbidden `anyhow!` uses in `web_search_tool.rs` test code; 176.88 s | `practical-clippy.log` |
| `cargo clippy --locked -p zeroclaw-api -p zeroclaw-infra -p zeroclaw-channels -p zeroclaw-providers -p zeroclaw-runtime --all-targets --features zeroclaw-channels/channel-telegram -- -D warnings` | Passed, 80.96 s | `affected-clippy-final.log` |
| `cargo clippy --locked -p zeroclaw-tools --lib -- -D warnings` | Passed, 44.87 s | `tools-production-clippy.log` |
| `cargo test --locked -p zeroclaw-api -p zeroclaw-infra -p zeroclaw-channels -p zeroclaw-providers -p zeroclaw-runtime -p zeroclaw-tools --no-fail-fast --features zeroclaw-channels/channel-telegram -- --test-threads=4` | 9,436 passed, 0 failed, 9 ignored; 405.48 s including compilation; 165 provider reliability tests and 10 Telegram delivery fault tests included | `milestone-affected-tests-final.log` |
| `cargo test --locked -p zeroclaw --test integration` | 153 passed, 0 failed; 138.29 s including compilation, 3.03 s tests | `milestone-integration-tests.log` |
| `cargo build --locked --release --workspace` | Passed on final code, 332.44 s; earlier full build also passed in 467.86 s | `workspace-release-final.log`, `workspace-release.log` |
| `cargo test --locked --manifest-path tools/zeroclaw-personal-ops/Cargo.toml` | 55 passed, 0 failed, including 11 synthetic history tests; 4.07 s | `personal-final-test-2.log` |
| `cargo clippy --locked --manifest-path tools/zeroclaw-personal-ops/Cargo.toml --all-targets -- -D warnings` | Passed, 2.65 s | `personal-final-clippy-2.log` |
| `cargo build --locked --release --manifest-path tools/zeroclaw-personal-ops/Cargo.toml` | Passed, 9.98 s | `personal-final-release-2.log` |

The full-workspace failure inventory is explicit:

- Unchanged architecture Fluent scan: bare CLI strings in
  `src/bin/verify_native_modmail_browser.rs` and
  `src/bin/zeroclaw-memory-promote.rs`.
- Unchanged component legacy-field scan: existing `reply_to` fields in
  `src/cron/mod.rs` fail the repository source scan.
- Four Telegram integration tests expected destructive delete/resend or
  fabricated success from a not-modified error. Replaced with positive-ack,
  retained-draft expectations; all 153 integration tests subsequently passed.
- Channel tool-iteration fixture used obsolete user-role result messages and
  repeated identical requests. Updated to current tool-role messages, varying
  fixture arguments and explicit fixture approval; channel suite passed.
- Runtime fixture expected model-authored reserved `codex_cli` recovery to run.
  Updated to assert rejection and no subprocess; runtime suite passed.
- New single-provider test uncovered missing cooldown creation, then itself used
  a plain error string outside the existing rate-limit classifier contract.
  Repaired all four non-streaming paths and used the standard 429 error envelope
  in the fixture. Final focused rerun is authoritative.
- Existing Grok subprocess fixture timed out waiting two seconds for a PID under
  parallel suite load; isolated original binary passed. Final bounded-concurrency
  provider run is authoritative; the full failed command remains recorded.

The initial preserved checkpoint suites passed API 198, channels 1,555 plus four
integration tests, infrastructure 230 plus seven integration tests, runtime 4,172
plus integration suites, and tools 1,724. Its provider suite passed 1,506 and failed
the unchanged Grok timing test (four ignored). These are **checkpoint evidence**,
not claims that its unfinished architecture is accepted or committed.

Owner-authorized cleanup removed generated incremental compiler caches only.
Available disk initially increased from approximately 147 MiB to 49 GiB. Later
validation regenerated caches; additional inactive caches were removed after
checking compiler command lines and open files. Source, Git state, release
binaries, logs and private preservation snapshots were retained.

## Continuation / integration gate

The bounded milestone is verified and committed on its isolated local branch,
`fix/reliability-delivery-milestone-20260906`. Port its remaining repairs into the
preserved checkpoint. Verified provider and
iMessage replacements are already copied to that companion checkpoint; the other
changes still need deliberate integration. Do not broaden
the journal, scheduler or compaction implementation while that checkpoint is failing.
Only task-owned hunks entered milestone commits; partial milestones stay off `master`.
Original dirty source files remain preserved separately from the isolated worktree.

Before any eventual installation: all acceptance gaps must be closed or explicitly
accepted by the owner, `master` must contain the verified commits, and build/test
logs must identify exactly the artifact being installed. No push, install, service
restart, credentials changes, real Telegram tests or real Messages reads are part
of this audit.


## Rollback boundary

The new session table is additive; existing chat history is retained. An older
binary does not understand chunk claims. Preserve the updated database and all
uncertain receipts across any binary rollback; never restore an old database or
reenable a possibly-applied send merely because old code cannot see its claim.
The operator must keep affected work quarantined until independent reconciliation.
No rollback or deployment was performed.

## Eventual deployment procedure (not executed)

Do not deploy this partial backlog under the current full-completion condition.
After the remaining work is complete, validated and integrated into local `master`,
use a clean detached checkout of that verified commit, not the mixed canonical
working tree. Example commands (operator-run only):

```sh
cd "$HOME/Documents/GitHub/zeroclaw"
git worktree add --detach /tmp/zeroclaw-verified-deploy master
cd /tmp/zeroclaw-verified-deploy
cargo build --locked --release -p zeroclaw
cargo build --locked --release --manifest-path tools/zeroclaw-personal-ops/Cargo.toml
```

Confirm that the service still uses `$HOME/.cargo/bin/zeroclaw` and the connector
uses `$HOME/.zeroclaw/bin/zeroclaw-personal-ops`; use the configured paths if they
have changed. Wait until interactive turns, scheduled writes and calls are idle.
The full graceful-drain requirement is not implemented by this milestone. Retain
private binary/config backups and a consistent SQLite backup including current
receipts. Do not replace state with an older snapshot to clear uncertainty.

For those existing binary paths, atomic replacement and main-service restart are:

```sh
backup_dir="$HOME/.zeroclaw/backups/reliability-$(date +%Y%m%d-%H%M%S)"
mkdir -m 700 -p "$backup_dir"
cp -p "$HOME/.cargo/bin/zeroclaw" "$backup_dir/zeroclaw"
cp -p "$HOME/.zeroclaw/bin/zeroclaw-personal-ops" "$backup_dir/zeroclaw-personal-ops"
cp -p "$HOME/.zeroclaw/config.toml" "$backup_dir/config.toml"
install -m 755 target/release/zeroclaw "$HOME/.cargo/bin/zeroclaw.next"
mv "$HOME/.cargo/bin/zeroclaw.next" "$HOME/.cargo/bin/zeroclaw"
install -m 755 tools/zeroclaw-personal-ops/target/release/zeroclaw-personal-ops "$HOME/.zeroclaw/bin/zeroclaw-personal-ops.next"
mv "$HOME/.zeroclaw/bin/zeroclaw-personal-ops.next" "$HOME/.zeroclaw/bin/zeroclaw-personal-ops"
"$HOME/.cargo/bin/zeroclaw" service restart
"$HOME/.cargo/bin/zeroclaw" service status
```

Keep existing policy entries. Separately admit only
`personal_ops__imessage_history_resolve` and `personal_ops__imessage_history` to
intended agents' existing allowed-tool policy if history access is desired. Do
not run the fresh personal-operations installer over existing state. No privacy
permission grant or actual history read is required for build validation. No
Telegram message, daemon restart, installation, phone-service change, credential
change, or remote push was performed during this task.

## Continuation prompt

Continue the reliability backlog from the preserved checkpoint and the verified
milestone branch. Read this 1–23 matrix, the original requirements, the external
validation logs and ownership manifest. Preserve all eight unrelated Reminders
paths, including personal-ops install.rs and templates/calendar.md. First replay
the milestone fixes into the checkpoint without overwriting its additional work,
then rerun focused checks. Implement legal canonical control-plane turn transitions,
durable pre-admission input, safe queued recovery, typed uncertainty through every
boundary, FIFO/cancellation/backpressure, structured preservation before compaction,
parent deadline/child cleanup, retained authorized output resources, intent/write
schema gating, scheduler occurrence/delivery separation and missed-run policies,
drain/readiness, bounded telemetry/log cleanup and SQLite maintenance. Do not use
lossy compaction or automatically replay uncertain writes. Reproduce and resolve
or precisely isolate recorded baseline validation failures. Run the full validation
matrix and release builds on the final tree. Commit task-only changes, integrate
onto local master only when full completion is met, and never push/install/restart
or access real Messages/Telegram during validation.
