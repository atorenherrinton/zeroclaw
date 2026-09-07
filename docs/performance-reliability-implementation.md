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

## Verified work and completion boundary (2026-09-07)

This branch is a reliability milestone, **not the completed backlog or a deployment
candidate**. The previously separate checkpoint has now been integrated into the
isolated completion branch. It includes durable channel admission, legal lifecycle
transitions, safe queued recovery, conversation ordering, cancellation checkpoints,
phase deadlines, provider stream cooldowns, scheduler quarantine, and bounded
observer/log payloads. The matrix identifies what each implemented boundary proves
and what remains incomplete. No partial milestone is merged to `master`.

The original numbering (1–23) is used below; the handoff groups the same work into
15 sections plus iMessage. Status describes the entire numbered requirement, so a
working, tested subset is still `prerequisite-only`. Evidence logs from this pass
are under the audit directory's `completion/` subdirectory. Earlier logs are
retained as historical evidence, including failing intermediate runs.

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
| 1. Non-destructive Telegram finalization | prerequisite-only | C`telegram.rs`, C`telegram/delivery.rs`, A`delivery.rs` | C`telegram/delivery/tests.rs`: `oversized_5314_edits_first_and_sends_remainder_in_order`, `chunk_two_failure_keeps_first_and_does_not_retry_5xx`, malformed success/rate-limit, rejected-edit, reopen, and cancellation cases; 10 local fake-server fault tests passed in `milestone-affected-tests-final.log` | Text finalization keeps the draft; unscoped sends, media and voice lack the complete durable contract. Existing redirect/force-voice branches still cancel their draft before replacement confirmation and must be migrated. Fault tests now include the actual request timeout, invalid/mismatched IDs, storage refusal, and strict rejection of fabricated acknowledgements from not-modified errors. Requires new binary/restart; no live testing. |
| 2. Durable turn journal | prerequisite-only | A`turn.rs`, A`inbound.rs`, C`orchestrator/turn_journal.rs`, C`orchestrator/mod.rs`, R`control_plane/task_registry.rs`, R`control_plane/task_store_sqlite.rs`, R`control_plane/authority.rs` | `ingress_ack_follows_persistence_and_duplicates_never_enqueue`, `queued_restart_retains_fifo_and_rejects_unsafe_transport_reconstruction`, `supervised_dispatch_recovers_safe_queue_once_and_quarantines_active_work`; migration, future-version, lifecycle/audit tests | Control-plane schema 9 atomically persists input and lifecycle events; terminal/ack rules enforced. Listener sender commits before acknowledgement, duplicates never enqueue. Telegram queued text is revalidated against current policy (groups requiring mention/reply evidence fail closed because that transport evidence is not retained); active phases quarantine. Gateway direct execution is not integrated. Control messages are conservatively uncertain. PID liveness protects other live owners; PID reuse can delay recovery. Requires binary restart and forward-only schema migration. |
| 3. Ordered conversation execution | prerequisite-only | C`orchestrator/mod.rs`, C`paced_channel.rs`, A`inbound.rs`, `crates/zeroclaw-channels/tests/pacing_integration.rs` | 15 pacing unit and 3 integration tests, including `cancelling_active_send_drops_adapter_work_and_unblocks_recipient`; `reservations_are_fifo_before_spawn_and_cancel_the_entire_queued_chain`, `completion_notification_is_safe_before_and_during_wait_registration`, `cancelled_worker_waiting_for_capacity_reaches_durable_terminal_state`, actual dispatcher restart test | Reservations precede spawn; successors wait before worker acquisition; queue bounded at 8× workers plus 100-slot transport ingress. Hard aborts checkpoint known-not-started versus uncertain work. Accepted identities are no longer merged by debounce. Paced outbound queues preserve per-send journal, route, deadline and acknowledgements; overflow is typed not_started, cancelled/expired queued work does not send later. The pacing depth bounds outstanding work, including the active operation. Cross-resource/global background priority remains incomplete. |
| 4. Idempotent/reconcilable delivery | prerequisite-only | A`delivery.rs`, A`turn.rs`, I`src/session_delivery.rs`, C`telegram/delivery.rs`, R`control_plane/task_store_sqlite.rs` | 7 infrastructure claim/history regressions; 10 Telegram fake-server faults; legal lifecycle/ack audit tests | FULL chunk claims and positive acknowledgements protect exact response chunks. Delivered requires confirmation; partial/uncertain remain distinct. Unscoped sends/media/voice still lack universal receipt binding. Lost Telegram acknowledgements require operator evidence; no retry/reset/reconciliation invention. |
| 5. Stalled-turn watchdog | prerequisite-only | R`control_plane/reaper.rs`, C`orchestrator/mod.rs`, C`orchestrator/turn_journal.rs`, A`deadline.rs` | `hard_worker_abort_checkpoints_without_external_replay`, `nested_and_retry_work_cannot_extend_parent`, worker-capacity cancellation regression | Channel parent deadline cancels the token and records uncertainty. Panic/abort guard makes bounded local checkpoint attempts; restart recovers if persistence/runtime shutdown prevents them. Generic absent-heartbeat rows still lack an age-based watchdog. Detached external child cleanup is not proven. |
| 6. Typed connector uncertainty | prerequisite-only | A`delivery.rs`, A`deadline.rs`, R`agent/tool_execution.rs`, R`tools/delegate.rs`, R`cron/scheduler.rs` | `structured_delivery_and_deadline_errors_never_enter_string_recovery`; Telegram typed fault tests | Tool dispatch and agentic delegates preserve DeliveryFailure and DeadlineExceeded through anyhow instead of string retry recovery. Generic ToolResult and scheduler execution remain string/bool based; unknown connector write errors still need typed receipts and reconciliation metadata throughout. No uncertain-write replay API exists. |
| 7. Cursor/byte-bounded history | prerequisite-only | I`src/session_backend.rs`, I`src/session_sqlite.rs`, T`sessions.rs` | I`tests/bounded_history_delivery.rs`: cursor append isolation, Unicode and storage errors; I`src/session_sqlite.rs`: actual query-plan and locked-database tests; T`sessions.rs` history tests; 7 infrastructure integration regressions, 232 infrastructure units and 1,724 tool tests passed | SQLite tail/cursor and content caps exist, with blocking isolation in tool. Each indexed SQLite step receives only the remaining page byte budget; metadata and JSON envelope are outside content cap. Other backends explicitly unsupported. Does not replace runtime full-history loading. |
| 8. Layered tool-output budgets | prerequisite-only | T`output_budget.rs`, R`agent/turn/mod.rs`, R`agent/tool_execution.rs` | `unicode_round_budget_does_not_persist_unretained_private_data`, `outcome_and_request_id_survive_large_json_result`; passed in checkpoint tools suite (1,724 tests) | Latest inherited code explicitly disables resource persistence, contrary to early handoff report. Only excerpts and limited small top-level evidence survive; no complete receipt/scope/error preservation. Existing artifact writer needs scoped retention/access/reference ownership before reuse; no second store should be introduced. |
| 9. Intent-relevant schemas | prerequisite-only | T`schema_selection.rs`, R`agent/turn/tool_specs.rs`, R`agent/turn/mod.rs` | `explanation_omits_unrelated_connectors_and_retains_discovery`; passed in checkpoint tools suite (1,724 tests) | Only names containing `__` filtered on first iteration with discovery available. Later iterations expose all schemas; built-in writes remain present, follow-up selection is not task-aware. Bundle and write-intent acceptance cases incomplete. Presentation filtering is not authorization. |
| 10. Safe compaction | deferred | R`agent/history.rs`, R`agent/history_trim.rs`, R`agent/turn/history_window.rs` (existing) | Existing pairing tests in `history_trim.rs`; no new semantic-preservation implementation | Existing turn-boundary trimming is lossy. No structured unresolved questions, promises, authorizations, receipts and uncertain outcomes snapshot. Do not add semantic compaction until this state has canonical owners and retention tests. No claim of completion. |
| 11. Bounded turns/deadlines | prerequisite-only | A`deadline.rs`, C`orchestrator/mod.rs`, R`agent/turn/mod.rs`, R`agent/tool_execution.rs`, R`tools/delegate.rs` | `nested_and_retry_work_cannot_extend_parent`, `phase_and_cancellation_survive_nested_deadlines`, typed dispatcher regression | One monotonic parent deadline wraps turn/provider/tool/delegate/delivery phases; a real fake-provider delegate loop verifies child checkpoints cannot overwrite its parent channel journal while receipts remain shared; expired children are never polled. Timeout carries phase and started evidence. Async futures drop by deadline; external spawned processes and MCP cancellation are not yet proven. Abort cleanup may continue up to 5 seconds for local durable persistence only. Larger background budgets/durable handoff remain incomplete. |
| 12. Provider retry/fallback | prerequisite-only | `crates/zeroclaw-providers/src/reliable.rs` | 1,512 provider tests passed in `current-tests.log`, including 167 reliable tests; streamed 429 shared-cooldown and HTTP-date Retry-After regressions | Full numeric/HTTP-date Retry-After, monotonic instance-shared cooldown and single-provider gate apply to ordinary and streaming calls. Concurrent streaming observations cannot shorten cooldown. The web-search failover fixture now uses the allowed Error::msg constructor, without changing its assertions. Account-global pooling and fallback safety after uncertain external tools remain incomplete. No provider configuration changes. |
| 13. Conflict-aware concurrency/backpressure | prerequisite-only | R`agent/tool_execution.rs`, C`orchestrator/mod.rs`, R`cron/scheduler.rs` | Parallel classification and reservation tests passed in checkpoint runtime/channels suites | Known stateless read batches use buffered(4), max 128 calls. Unknown/shell/UI writes serialize within a batch. Cross-turn exact resource locks and global interactive worker reservation absent; separate pools are not proof against resource starvation. |
| 14. Connection reuse/metadata caching | deferred | C`telegram.rs` (`http_client`), `crates/zeroclaw-providers/src/reliable.rs`, R`agent/turn/tool_specs.rs` | Existing client reuse inspected; schema byte telemetry added, no setup benchmark | No new pooling/cache introduced. Need connection-setup measurements, registry generation invalidation and account/catalog expiry before further optimization. |
| 15. Scheduler occurrence durability | prerequisite-only | R`cron/store.rs`, R`cron/scheduler.rs`, `tests/component/cron_delivery_cli.rs` | `clear_stale_locks_quarantines_interrupted_work`, `occurrence_survives_job_deletion_and_blocks_same_occurrence_replay`, scheduler retry tests; passed in checkpoint runtime suite (4,172 passed, 3 ignored) | Stable (job_id,next_run) claim exists, but bool success conflates execution and notification, and occurrence recovery/missed-run policies are incomplete. Manual paths and claim enablement need audit. Interrupted jobs are disabled/uncertain; operators must reconcile before future enablement. |
| 16. Graceful restart/readiness | prerequisite-only | C`orchestrator/mod.rs`, C`orchestrator/turn_journal.rs`, R`control_plane/boot.rs`, R`control_plane/authority.rs`, R`control_plane/reaper.rs` | Actual dispatcher reopen test; queued sender-policy refusal; hard abort and cancelled-admission tests; live-other-boot ownership regression | Queued supported text resumes once after policy revalidation; unsafe/unsupported reconstruction explicitly fails; active/received work quarantines. Listener cancellation follows dispatcher exit. Full gateway/listener/scheduler/delegate coordinated shutdown and connector readiness remain incomplete. No live restart performed. |
| 17. Atomic state/config writes | prerequisite-only | `crates/zeroclaw-config/src/schema.rs`, R`control_plane/task_store_sqlite.rs` | 1,502 config tests passed in `current-tests.log`; v7-to-v9 migration/reopen, future-schema rejection, transaction rollback and legal checkpoint tests | Existing config primitive uses temp/sync/rename/backup. Control-plane migration and lifecycle writes are transactional; claims/checkpoints use FULL SQLite durability. Post-rename directory-sync failures and other state writers require complete inventory/migration before universal atomic-write claims. |
| 18. End-to-end traceability | prerequisite-only | C`orchestrator/turn_journal.rs`, C`orchestrator/mod.rs`, R`agent/turn/mod.rs`, R`agent/turn/tool_specs.rs`, C`telegram/delivery.rs` | Lifecycle tests assert ordered durable audit events; dispatcher reuses journal trace ID; chunk receipts and schema telemetry inspected | Supervised inbound ID is the runtime trace ID. Canonical task_turn_events records state/time/response bytes without bodies. Gateway/delegate/scheduler IDs and complete phase-latency/terminal metrics are not yet unified. No full content in new ingress metrics. |
| 19. Reliability objectives | deferred | C`orchestrator/mod.rs`, R`observability/prometheus.rs`, R`observability/runtime_trace.rs`, `crates/zeroclaw-log/src/broadcast.rs` (existing) | No new objective aggregation tests or deployed dashboard | Missing unanswered/duplicate/uncertain age/recovery/first-visible p50-p95 series and separate schedule execution/delivery rates. Instrument bounded metadata before claiming objectives. |
| 20. Connector/worker readiness | deferred | R`control_plane/reaper.rs`, R`daemon/mod.rs`, R`daemon/registry.rs`, R`health/mod.rs` (existing) | No end-to-end readiness test in inherited diff | No aggregate oldest queued/uncertain, scheduler lag, stopped workers, dropped events or connector readiness surface. Process liveness is insufficient. |
| 21. Bounded/rotating logs | prerequisite-only | R`agent/tool_execution.rs`, `src/main.rs`, `crates/zeroclaw-config/src/schema.rs`, `crates/zeroclaw-log/src/config.rs`, `crates/zeroclaw-log/src/event.rs`, `crates/zeroclaw-log/src/writer.rs` | 1,502 config and 123 log tests passed in `current-tests.log`; Unicode/secret-boundary observer test; event JSON-escaping budget tests; `direct_events_are_bounded_before_broadcast_and_persistence` | New defaults use existing append-oriented rotating 16 MiB storage. Event attributes cap at 16 KiB with correlation/outcome preservation, direct log messages at 4 KiB, observer bodies at 4 KiB before redaction. Central writer bounds before observer/broadcast/disk copies. Normal CLI exit drains/syncs accepted logs with a 5-second limit; full-queue/dead-worker timeout tests cover the wait. Raw tracing formatter allocation, explicit rolling configs, and abandoned-temp cleanup remain to address. |
| 22. Replay/fault tests | prerequisite-only | C`telegram/delivery/tests.rs`, C`orchestrator/turn_journal.rs`, C`orchestrator/mod.rs`, I`tests/bounded_history_delivery.rs`, P`src/imessage_history/tests.rs` | 10 Telegram faults; 7 infra regressions; 11 synthetic Messages cases; actual dispatcher safe-queue recovery, rejected policy, duplicate ingress, cancellation after durable admission, hard abort and wake-up race tests | Faults use local fake services and synthetic SQLite. Whole-process termination with real subprocess/MCP children and gateway ingress still need integration coverage. No real Messages history or outbound messages used. |
| 23. SQLite maintenance | prerequisite-only | I`src/session_sqlite.rs`, I`src/session_delivery.rs`, T`sessions.rs`, P`src/imessage_history.rs` | 232 infrastructure unit tests, 7 cursor/claim regressions, 55 personal-ops tests passed; runtime full-history behavior remains unchanged | Bounded diagnostic reads and blocking adapters present. Most legacy history APIs still mask errors, runtime loads full history, low-activity WAL maintenance and raw-payload retention absent. No replacement proposed; measure first. |
| Adjacent: read-only iMessage history | completed | P`Cargo.toml`, P`src/lib.rs`, P`src/imessage_history.rs`, P`src/imessage_history/tests.rs` | `cargo test --locked --manifest-path tools/zeroclaw-personal-ops/Cargo.toml`: 55 passed, including 11 synthetic history cases, 0 failed; strict Clippy and release build passed (September 7 commands: 1.07 s / 0.16 s / 0.17 s) | Exact GUID/ID, RFC3339 ≤31-day window, cursor, output byte caps, attachments metadata only. Attributed-body-only text explicitly unavailable. Outgoing sender attribution is explicit (self, no recipient mislabeling); corrupt text and oversized identity metadata fail explicitly. SQLite permission errors other than AUTH/PERM may still be classified storage_unavailable; no failure becomes no_results. No real Messages DB read; installation and policy registration remain separate owner actions. |

## Validation ledger

Final September 7 evidence is in `completion/reviewed-validation.jsonl`,
`completion/frozen-validation.jsonl` and their named logs. Only the commands
below describe final source. The earlier `frozen-workspace-tests.log` exposed a
compiler trait-depth overflow while isolating the delegate journal; type-erasing
the child future repaired it without increasing compiler limits. The subsequent
full reviewed test/check/Clippy run is authoritative. No source changed afterward.

All directly affected unit suites passed: API 199, channels 1,570 (2 ignored),
config 1,502, infrastructure 232, logging 125, providers 1,512 (4 ignored), runtime
4,178 (3 ignored), and tools 1,724: **11,042 passed, 0 failed, 9 ignored**.
The full workspace result below also includes other crates, integration suites
and doctests. Counts exclude five child-process test summaries already represented
by their parent tests. Focused cases include 10 Telegram fault tests, 7 session
history/receipt integration tests, 167 reliable-provider tests, 15 pacing unit
and 3 pacing integration tests, lifecycle/recovery tests, and the real
fake-provider delegate loop with parent-journal isolation and retained receipts.

Dev/test/check/Clippy commands used the canonical repository's existing
`target` through `CARGO_TARGET_DIR`, `CARGO_INCREMENTAL=0`,
`CARGO_PROFILE_DEV_DEBUG=0`, and `CARGO_PROFILE_TEST_DEBUG=0`. Release builds used
the same target directory and normal release settings. Personal-ops also used
that shared target in this pass. Release artifact hashes are recorded separately;
no artifact was installed.

| Exact command | Final result | Seconds | Log |
| --- | --- | ---: | --- |
| `cargo fmt --all -- --check` | Passed | 4.04 | `reviewed-fmt.log` |
| `cargo check --locked --workspace --all-targets` | Passed | 33.95 | `reviewed-check.log` |
| `cargo test --locked --workspace --no-fail-fast -- --test-threads=4` | 15,402 passed; 2 failed; 24 ignored (295.54 s). Both failures are unchanged source-policy tests listed below. | 295.54 | `reviewed-workspace-tests.log` |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | Passed | 1.27 | `reviewed-workspace-clippy.log` |
| `cargo test --locked -p zeroclaw-channels --features channel-telegram --lib paced_channel -- --test-threads=4` | 15 passed; 0 failed | 20.18 | `frozen-paced-unit.log` |
| `cargo test --locked -p zeroclaw-channels --features channel-telegram --test pacing_integration -- --test-threads=4` | 3 passed; 0 failed | 12.31 | `frozen-paced-integration.log` |
| `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` | Blocked: generated web/dist/index.html absent; no frontend installation performed. | 4.89 | `frozen-all-features-clippy.log` |
| `cargo clippy --locked -p zeroclaw-api -p zeroclaw-infra -p zeroclaw-runtime -p zeroclaw-channels -p zeroclaw-providers -p zeroclaw-log -p zeroclaw-config --all-targets --all-features -- -D warnings` | Passed | 73.88 | `frozen-affected-all-features-clippy.log` |
| `cargo build --locked --release --workspace` | Passed | 308.32 | `frozen-release.log` |
| `cargo test --locked --manifest-path tools/zeroclaw-personal-ops/Cargo.toml` | 55 passed; 0 failed (11 synthetic Messages cases) | 1.07 | `frozen-personal-test.log` |
| `cargo clippy --locked --manifest-path tools/zeroclaw-personal-ops/Cargo.toml --all-targets -- -D warnings` | Passed | 0.16 | `frozen-personal-clippy.log` |
| `cargo build --release --locked --manifest-path tools/zeroclaw-personal-ops/Cargo.toml` | Passed | 0.17 | `frozen-personal-release.log` |
| `cargo fmt --manifest-path tools/zeroclaw-personal-ops/Cargo.toml -- --check` | Passed | 0.12 | `reviewed-personal-fmt.log` |

Remaining workspace failures are deterministic source scans:
`cli_fluent_coverage::user_facing_strings_route_through_fluent` (five existing
machine-code literals in two root helper binaries), and
`component::reply_target_field_regression::source_does_not_use_legacy_reply_to_field`
(existing cron intent `reply_to` fields). The test files and all reported source
files are byte-identical to the handoff HEAD `ff26f3db9`; evidence is in
`completion/baseline-source-policy-evidence.json`. These failures were not waived,
changed to passes, or used to weaken the scans. The all-features frontend build
prerequisite remains unresolved. Full completion and master integration are blocked
by the acceptance gaps in the matrix, independently of these baseline checks.

### Historical September 6 milestone evidence

The following commands describe the earlier milestone, not the final completion
branch. Their failures and repairs remain useful evidence and are not relabeled
as passing commands.

Logs and preservation evidence are outside the repository in the handoff audit
directory. No prior-run log is counted as current verification. Commands used
`CARGO_TARGET_DIR` pointing to the existing canonical repository build cache;
personal-ops uses its separate existing target directory. Times include Cargo
lock waiting where applicable.

| Command | Historical result | Evidence |
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

Owner-authorized cleanup removed inactive generated Rust build caches only (incremental objects and, during the September 7 pass, dependency/build/fingerprint caches after confirming no active compiler used them).
Available disk initially increased from approximately 147 MiB to 49 GiB. Later
validation regenerated caches; additional inactive caches were removed after
checking compiler command lines and open files. Source, Git state, release
binaries, logs and private preservation snapshots were retained. The older Codex directory containing the shared Git common directory is essential repository metadata and was retained.

## Continuation / integration gate

The current isolated worktree is on `fix/reliability-completion-20260906`; its
parent is the verified delivery milestone. The broader checkpoint has already
been integrated. Do not replay it over the current tree or lose the subsequent
repairs. Only task-owned hunks belong in the completion commits. Partial
milestones remain off `master`, and the canonical dirty source tree is preserved.

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
export CARGO_TARGET_DIR="$PWD/target"
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
install -m 755 target/release/zeroclaw-personal-ops "$HOME/.zeroclaw/bin/zeroclaw-personal-ops.next"
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

Continue from `fix/reliability-completion-20260906` in the isolated `milestone`
worktree, not from the older preserved checkpoint. Read this 1–23 matrix, the
original requirements, completion report and `completion/` validation logs.
Preserve the canonical dirty tree and all eight unrelated Reminders paths.

The listener/control-plane lifecycle and migration are now wired and tested;
do not redo their initial implementation. Finish the remaining boundaries:

1. Route authenticated gateway ingress through canonical durable admission;
   extend current-policy recovery and typed acknowledgements to other adapters,
   control replies, media and voice; remove pre-confirmation draft cancellation from redirect/force-voice branches. No unauthenticated fallback ingress.
2. Carry typed external-effect receipts through generic tools, delegates and
   schedules, preserving earlier successful receipts when a later call fails.
3. Add scoped/expiring overflow ownership to the existing hardened resource
   writer; enforce complete aggregate payload/metadata budgets and cleanup.
4. Complete intent/write schema bundles and active-task follow-up preservation;
   add structured preservation before any semantic compaction.
5. Propagate cancellation/deadlines through MCP, spawned subprocesses and cleanup;
   reserve interactive capacity and add exact mutable-resource serialization.
6. Complete occurrence execution/delivery separation, per-job missed-run policy,
   manual-run claims and deterministic scheduler reconciliation.
7. Coordinate admission stop/drain/readiness across gateway, listeners, connectors,
   scheduler and delegates; add full process fault tests using synthetic services.
8. Instrument missing objectives/readiness, audit raw tracing formatter allocation
   and abandoned temporary logs, migrate remaining atomic writers, and add measured
   low-activity SQLite checkpoint/retention behavior and bounded runtime history.

Do not add a competing turn database, lossy compactor, or uncertain-write retry
path. Reproduce or precisely isolate recorded baseline validation failures. Run
format/check/test/strict-Clippy/release commands on final source. Commit only
verified task work; integrate local master only after the full completion gate is
met. Never push, install, restart, send real messages or read real Messages data.
