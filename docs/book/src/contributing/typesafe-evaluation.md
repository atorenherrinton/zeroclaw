# Offline TypeSafe evaluation

`tools/zeroclaw-typesafe-eval` collects bounded Telegram timing records from an
explicitly selected existing runtime trace and reports paired advisory decisions
when independently supplied. It runs offline, has no agent, MCP, HTTP, scheduler,
action executor, or dependency on the separate TypeSafe helper PR. Running it
cannot send traffic to Jev or change Telegram delivery.

This is an **observational collection and analysis tool**, not a live causal A/B
experiment. It does not generate a counterfactual response, hide Jev from the
currently deployed agent, or establish response-quality improvement. Its report
always declares `response_quality_evidence: "insufficient"`. No experiment starts
automatically when this code is merged.

## Architecture and canonical sources

The existing `crates/zeroclaw-eval` replay harness runs
scripted model outputs through the real agent loop; its grades test machinery,
not model quality. Reusing it to manufacture a second answer would not establish
what the live provider would have answered without Jev. The standalone offline
crate avoids building or extending the transitional runtime and has no paid-call
path. Its dependencies already exist in the repository.

[Canonical logs](../architecture/logging.md) own event identity, attribution and
measured durations. The collector reads the native JSONL format, with Telegram
channel attribution and exact known message/action pairs from
`crates/zeroclaw-channels/src/orchestrator/mod.rs` and
`crates/zeroclaw-runtime/src/agent/turn/post_exec.rs`. It projects an allowlist of
numeric/boolean fields. It does not read tool arguments, interpret judgment
rationales, or infer consent. Jev outputs remain untrusted advisory evidence;
only the existing execution and approval paths can authorize actions.

The currently unbound Observer bridge is unsuitable for collection: its channel
projection loses turn identity and its duration projection defaults missing
values to zero. This collector preserves nulls from native events instead.

The experiment salt creates the experiment identity. HMAC-SHA256 with a new
private 32-byte salt, versioned prefix and separate `pair`, `event`,
`event-content`, and `experiment` domains creates pseudonymous artifacts. No raw
identifier is hashed without that private salt. The manifest is the resulting
`records.json`: a materialized view of the selected trace, never live policy.
There is no second config source or authorization state.

## Run the automation

Build and validate from the repository root. These commands never start ZeroClaw
or make network calls:

```sh
cargo test --offline --locked --manifest-path tools/zeroclaw-typesafe-eval/Cargo.toml
cargo clippy --offline --locked --manifest-path tools/zeroclaw-typesafe-eval/Cargo.toml --all-targets -- -D warnings
cargo build --offline --locked --manifest-path tools/zeroclaw-typesafe-eval/Cargo.toml
```

Use a new experiment directory outside the repository. The trace path below is
an owner-approved, complete, existing snapshot, not a request to enable broader
logging. Input files must already be regular, owner-only files (mode 0600), with
no hard links or symlink path components. On macOS use canonical paths, including
`/private/tmp` instead of the `/tmp` symlink. Do not change live trace permissions
or copy unrelated history merely to satisfy this tool.

```sh
tools/zeroclaw-typesafe-eval/target/debug/zeroclaw-typesafe-eval init /private/tmp/jev-study
tools/zeroclaw-typesafe-eval/target/debug/zeroclaw-typesafe-eval collect /private/tmp/jev-study /absolute/private/approved-trace.jsonl
tools/zeroclaw-typesafe-eval/target/debug/zeroclaw-typesafe-eval report /private/tmp/jev-study
```

`init` creates a new mode-0700 directory and mode-0600 `salt`. `collect` writes
`records.json`; `report` writes `report.json`. Every output uses exclusive
creation, so retries refuse to overwrite prior evidence. Retain the same salt
and original snapshot for reproducibility. To rerun a report after adding labels,
explicitly archive the previous report outside the experiment directory first.
Do not combine separate experiment salts or repeatedly include overlapping
windows as independent samples. The tool does not merge files or tail a log.

All commands emit a small JSON success/failure envelope and exit nonzero on
failure. They never echo source lines, paths, parser diagnostics or secrets.
Check the documented permissions, file existence, schema, matching and bounds
when a command fails. A write failure can leave an incomplete newly created file;
archive/remove that failed artifact explicitly before retrying. No previous file
is replaced. Prefer a completed snapshot: reads are bounded but cannot make a
concurrently changing input coherent.

## Matching, assignment and denominators

The sample unit is one observed Telegram **turn**, keyed only by the native
`trace_id` (or the same attribute). Conflicting native/attribute IDs fail closed.
Missing IDs are counted as uncorrelated; no timestamp/chat proximity join is
attempted. Exact event-ID/content duplicates are counted and ignored; conflicting
reuse fails collection. Repeated distinct generation/delivery milestones, missing
event IDs, or contradictory duration order mark the turn ambiguous and suppress
its timing samples. Tool completions remain separate call-level observations.
The collector does not identify cross-agent causal chains or independent users.

Assignment version 1 uses the high bit of the first HMAC byte to assign stable
50/50 **analysis partitions**. Input order and duplicate records cannot change
assignment. These partitions are not actual baseline/treatment exposure, and
are deliberately not reported as causal treatment arms. All observed turns are
included; there is no outcome-dependent sampling. An offline pair uses both
baseline and treatment decisions for the same pair ID, rather than two unmatched
sets of live turns.

The observed-turn denominator is only turns represented by recognized correlated
events in the selected snapshot. It is not all Telegram traffic: persistence may
be disabled, dropped, rotated or truncated; ignored events and uncorrelated rows
are counted. There is no mechanism to recover unlogged traffic or quantify its
count. Early exits may have an inbound event and no response. Missing Jev events
do not prove the agent did not call Jev. The report retains ambiguous turns and
missing outcomes in denominators instead of excluding them silently.

Every timing distribution reports its own `n`, `missing`, median, p90 and p95.
Median averages the two middle values; percentiles use nearest rank. Empty
samples stay null. Percentiles on small samples are descriptive, not a decision
threshold. Jev timing denominators count recognized tool completions; other
timings count observed turns. Error, timeout, cancellation and unknown delivery
counts are separate. A missing outcome is never counted as a success or timeout.

## Timing meaning

| Report metric | Existing evidence and limitation |
| --- | --- |
| `processing_to_generated` | Monotonic `started_at` duration at `channel_response_generated`; includes preparation, agent loop and final rendering. This timer begins after some inbound processing. |
| `processing_to_submission_completion` | Same timer at final submission completion, including unconfirmed submissions. |
| `processing_to_confirmed_ack` | Same duration, only when submission succeeds and delivery summary confirms every positive-numbered chunk. It is not proof of human receipt/read. |
| `generated_to_confirmed_ack` | Difference of those same-timer durations. Includes finalization, persistence and delivery/retries, not just an HTTP send call. |
| `jev_tool_call` | `tool_call_result` duration for the exact MCP name `typesafe__typesafe_system_one`; includes MCP/helper overhead, not isolated SDK/API latency. Concurrent calls are not summed into critical-path latency. |
| `platform_to_receipt` | Missing: Telegram `ChannelMessage.timestamp` currently uses local `SystemTime::now`, not Telegram's platform date. |
| `receipt_to_processing` | Missing: existing correlated trace does not establish the receipt boundary. |
| `platform_to_confirmed_ack` | Missing: cannot claim end-to-end without the platform boundary. |

Jev failures use the tool's outcome, never free-text error heuristics. Jev timeout
count stays null because a structured timeout distinction is not available here.
Channel timeouts are counted from the specific existing timeout event. Telegram
retry, draft, cancellation and receipt behavior remains untouched. Source event
names and fields are a versioned adapter contract; future emitter changes require
updating fixtures and adapter together, not guessing replacements.

## Optional paired decision records and independent labels

The collector does not fabricate a baseline from the absence of a tool call or
parse a model's rationale as a label. An owner must supply paired observations
from an already authorized source: the same state/question and precommitted
mapping of decision codes, with baseline obtained without the judgment and
treatment obtained with the existing judgment. No extra calls are made by this
tool. If such evidence does not exist, omit these files and keep quality claims
open. Retrospectively asking the same influenced agent to invent a baseline is
not valid paired evidence.

Before viewing outcomes, record a protocol version, decision-code meanings,
eligibility, exclusions, collection window and stopping rule in a private study
note. Codes are integers 0 through 63. A changed mapping requires a new protocol
and study; the tool checks version equality but cannot enforce precommit timing
or truthfulness. It does not accept response text in these files.

`pairs.json` has this strict structure, replacing the placeholders with the
experiment and pair IDs from `records.json`:

```json
{
  "schema_version": 1,
  "experiment_id": "<64 lowercase hex characters>",
  "protocol_version": 1,
  "pairs": [{
    "pair_id": "<64 lowercase hex characters>",
    "baseline": {"status": "ok", "code": 0},
    "treatment": {"status": "ok", "code": 1}
  }]
}
```

Other statuses are `missing`, `error`, and `timeout`, each requiring null `code`.
Duplicate, unmatched, ambiguous or inconsistent pairs fail validation. Reported
changes use complete pairs only, with supplied/incomplete/unsupplied denominators
and separate arm error/timeout counts.

Rubric version 1 is **independent gold decision correctness**: an owner-designated
human receives the necessary redacted case context and the precommitted decision
code mapping, without either arm's predictions or Jev rationale, and supplies the
one correct code. Ambiguous cases remain unlabeled. This workflow happens outside
the tool; no blind response pack is generated and blinding cannot be verified.
Do not ask Jev or the responding agent to grade itself. `labels.json` contains:

```json
{
  "schema_version": 1,
  "experiment_id": "<64 lowercase hex characters>",
  "protocol_version": 1,
  "rubric_version": 1,
  "labels": [{"pair_id": "<64 lowercase hex characters>", "gold_code": 1}]
}
```

```sh
tools/zeroclaw-typesafe-eval/target/debug/zeroclaw-typesafe-eval report /private/tmp/jev-study /absolute/private/pairs.json /absolute/private/labels.json
```

Useful decision change means baseline incorrect and treatment gold-correct;
harmful means the reverse. Without labels these values are null. Agreement,
disagreement, faster requests and decision correctness are not improved response
quality. Label coverage and labeled-change denominators are explicit. Useful and
harmful change rates are reported both among labeled changes and among all labeled
complete pairs; each stays null when its denominator is zero. Selective labeling
biases results; precommit coverage and keep missing labels visible.
There are no significance claims: repeated turns within conversations are
correlated, and this minimized dataset intentionally has no conversation IDs for
cluster-adjusted inference.

## Privacy, rollout and rollback

Collection is manual opt-in, bounded to 64 MiB, 100,000 lines, 256 KiB per line,
10,000 turns and 128 completed Jev calls per turn. Oversized or malformed input
fails; there is no silent truncation. Durations outside one day fail validation.
Outputs contain only salted IDs, bounded codes, counts, flags and durations.
Raw messages, timestamps, sender/chat IDs, prompts, outputs, rationales, model
names, API keys and credentials are not persisted by this tool. JSON parsing
necessarily reads selected source lines transiently in memory. Unknown fields in
pair and label files reject rather than becoming an extra payload channel.

The salt enables linkage within the study and is private, not an API credential.
Protect source snapshots, labels and pseudonymous records as sensitive. Use a
precommitted short collection window and delete the experiment directory and its
salt after review; there is no daemon retention job or hidden copy. Owner-only
file permissions, no symlinks and no hard links are enforced on Unix. The tool
assumes the same OS owner is trusted; it does not defend against a malicious owner
concurrently replacing parent directories. It is not a signed installed helper;
any later local installation must follow the local signing contract.

Rollout starts with the synthetic CLI tests, then an owner-approved snapshot and
manual inspection of denominator/timing coverage. Confirm the deployed event
schema before describing collection as live. To measure actual response quality,
a separate reviewed change must establish a genuine unchanged-state baseline and
advisory treatment, blinded response labeling, suitable sample size/stopping rule,
conversation-aware inference, and missing platform/receipt boundaries. True shadow
mode would keep the judgment outside the user-visible agent context; the current
external MCP route does not guarantee this. No such experiment or new telemetry
is enabled here.

Rollback is to stop invoking the tool and remove its private experiment directory
when retention permits; revert its source commit if needed. There is no daemon
restart, service change, permission change, or delivery rollback. Root workspace
tests do not exercise this standalone crate: the explicit locked test and Clippy
commands above are required validation for changes here.
