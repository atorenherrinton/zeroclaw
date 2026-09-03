# Native recall-weighted memory promotion

This opt-in SQLite feature promotes a small, previously stored owner note after
real native recall evidence. It does not run a model, summarize transcripts,
search files, rewrite source notes, or import historical recall counters.

## Ownership and provenance

`data/memory/brain.db` remains the single owner of source memories, source-version
attestations, recall evidence and atomic promotion receipts. Existing memory and
session/history rows are preserved. Evidence is keyed by agent UUID and the exact
row version (id, content, key, category, namespace, timestamps and privacy scopes).
A changed source requires a fresh authoring attestation and fresh evidence.

The common native turn engine scopes ephemeral provenance around the complete
turn. It is present only for an explicitly enabled agent's local Interactive
turn, or an exact operator-admitted single-owner channel identity. Channel
identity comes from the actual channel kind plus the channel handle's alias,
for example `telegram.luma`; the provisional ingress trust flag is not used.
Cron, daemon, subturn, direct embedded/API/WS turns and missing context are
excluded. Nested turns replace rather than inherit the parent's scope.

The explicit `memory_store` tool may attest a Daily note only if it is the whole
normalized current owner statement (optionally removing `Remember:` or
`Remember that`), not a paraphrase or a selected substring which could omit a
negation. This is authoring provenance, **not** a claim of factual verification.
The note must be in the same agent's default namespace, without session/tenant
scope, and pass contamination checks. Conversation/autosave/history, caller,
external-tool, consolidation, import and prior-promotion namespaces/keys do not
qualify. Imported old notes remain intact but unattested; no blanket trust or
fabricated historical counters are introduced.

Only complete short notes of at most 160 UTF-8 bytes qualify. This conservative
bound is at most 160 byte-pair text tokens without assuming a particular model's
tokenizer. Longer notes are retained unchanged and not silently shortened.

## Evidence and score

Raw backend over-fetches, list/get/export calls, time-only tool browsing,
and filtered/truncated notes do not record qualifying evidence. An actually
returned unscored result can count an exposure but contributes zero relevance,
never a synthetic perfect score. Injection records only entries fully rendered after its
skip set, relevance checks and budgets. `memory_recall` records its successful
results only when the full result fits the native collector's configured bound.
Repeated injection/tool exposures in one turn count once. Distinctness uses
the actual original owner input, not model-generated query variants within a
single turn. Whitespace/case variants normalize identically; blank, wildcard,
punctuation-only and contaminated inputs are excluded.

The admitted Telegram provenance is typed owner text only. Native forwarded
message, document and voice/audio decorators are excluded: an authenticated
sender can forward another person's statement or upload another speaker's audio.
Replies and image-bearing inputs are also excluded. This affects automatic
promotion evidence only; ordinary voice/document chat, history and explicit
memory writes continue unchanged.

Queries and turn ids are stored only as domain-separated SHA-256 hashes with a
per-install random salt and agent scope. Neither raw input nor memory contents
are copied into the evidence tables or a trace log. The existing opt-in general
memory audit is independent and may log queries; this feature does not enable it.

Policy version 1 keeps the original weights: frequency .24, retrieval relevance
.30, distinct-query diversity .15, recency .15, spaced recall days .10, conceptual
.06. Frequency is `ln(1+recalls)/ln(11)`, clamped to one; diversity is unique
queries / 5, clamped to one. Recency uses a 14-day half-life from the last real
recall. A source whose latest qualifying recall is more than 30 days old does
not qualify; future evidence timestamps are excluded. Like the original engine,
lifetime counts, distinct queries, and recall days remain scoped to the exact
source version rather than a rolling 30-day evidence window. A new real recall
can therefore reactivate a previously exposed, unchanged source. The original
consolidation component uses observed recall-day spacing and span.
No conceptual tags, synthetic daily/grounding signals, or model-phase boosts
are invented: unavailable components contribute zero. At least 3 recalls and
3 distinct owner queries are required, independently of weighted score >= .75.
Three perfect same-day recalls alone need not meet the score threshold.

## Deterministic daily job

Configuration is default-off:

```toml
[memory.promotion]
enabled = true
agent_aliases = ["main"]
owner_channels = ["telegram.luma"]
```

Admit a channel only after confirming single-owner pairing, no wildcard/group
ingress, and the intended main-agent route. Revoking that property requires
removing it here. This feature does not widen channel permissions.

Build both the daemon and helper from the same source. The helper invocation is:

```text
zeroclaw-memory-promote --config-dir /absolute/native/install --agent main
```

Append `--dry-run` for metadata-only candidate selection. It opens the native
database and can initialize additive evidence schema, but does not promote
memory. It never reads an OpenClaw tree, starts an agent or accesses an API.
Configuration and database files must be regular owner-only Unix files. Output
is counts and policy version only; failures are a generic fixed error label.

The scheduler owns daily `0 3 * * *` with timezone `America/Los_Angeles` (DST
aware), delivery none, deterministic shell command, no retry prompt or sentinel.
Use a dedicated non-channel, non-delegated scheduler identity with only this
executable allowed; the pinned `--agent main` still selects main's memory owner,
not the scheduler identity. Do not broaden main's general shell policy.
Scheduling/activation is separate from this implementation. The helper's
30-second process deadline also bounds startup and SQLite lock waits.

Each pass admits at most 10 highest-scored source versions. New Core copies,
FTS changes and unique receipts commit in one `BEGIN IMMEDIATE` transaction.
Concurrent runs serialize; a crash rolls back all uncommitted work. A repeated
run cannot promote the same source version again. Existing destination-key
conflicts fail closed rather than overwrite. Source rows and history are never
deleted, modified or truncated. Promotions use strict threat scanning plus the
configured write/redaction/namespace/category quotas. No embedder is called;
the existing optional reindex maintenance can fill vectors later.

## Verification

Synthetic real-SQLite tests cover score/age/half-life, query hashing and
normalization, source-version reset, namespace and agent isolation, unknown
schema versions, preservation, max ten, dry-run and idempotency. A real native
factory + memory_store + memory_recall test exercises authoring, BM25 recall,
collector-budget exclusion and actual promotion without external services.
The injection test proves that skipped, low-score and truncated results are
not submitted as evidence. Live memory is not used by these tests.
