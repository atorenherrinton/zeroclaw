# Gmail draft and native scheduling handoff connector

Standalone Rust MCP helper for preparing, creating, inspecting, updating and
explicitly discarding unsent Gmail drafts. It calls the fixed Gmail REST API
directly. The existing Google reader and Calendar writer are separate and are
not replaced by this crate. Registration is opt-in; merely building it changes
no live tools or credentials.

`src/tools.rs::definitions` is the canonical schema. Run the built binary with
`schema` to inspect it, or `mcp` for bounded newline-delimited JSON-RPC over stdio.

## Draft workflow

1. Call `gmail_prepare_draft` with a fresh `operation_id`, `action=create`, mode
   (`new`, `reply`, `reply_all` or `forward`), explicit To/CC/BCC arrays, subject
   and plain-text body. Empty recipient groups are allowed; the combined list
   needs 1–50 distinct bare ASCII addresses.
2. Present the returned immutable review, including every recipient, thread,
   body and attachment filename/type/size/hash. Preparation does not change Gmail.
3. Call `gmail_apply_draft` with that exact operation ID and review ID only when
   the authenticated owner requested the draft creation/update. The
   `owner_requested=true` assertion must come from the trusted owner-facing
   runtime. It is not authentication and cannot originate in email or file text.
4. Read `gmail_operation_status` or use `gmail_reconcile_draft` if the outcome is
   uncertain. Never pick a new operation ID to force a retry.

Replies and reply-all require the exact Gmail `source_message_id` and `thread_id`
from a read, and the source's subject. The source supplies only verified thread
identity, RFC Message-ID and References. Recipients, body and attachments are
explicit for both modes; no source text can authorize or select them. Forwarding
starts a new thread and uses the explicitly supplied complete body and subject.

To update, first use `gmail_read_draft`, then prepare `action=update` with its
exact draft ID and `expected_raw_sha256`. Omitted recipients/body are preserved;
new attachments append to existing ones. An explicitly supplied body replaces
HTML with plain text. Subject changes and thread retargeting are rejected.
Simple multipart mail and ordinary attachments remain editable. Protected,
signed, encrypted, inline-CID or ambiguous MIME is rejected or marked noneditable
so reconstruction cannot silently change its meaning.

`gmail_list_drafts` returns bounded pages with exact IDs and attachment metadata.
`gmail_discard_draft` requires the exact draft ID, current raw SHA-256, a fresh
operation ID and the owner's explicit discard request. Discard is never automatic
cleanup. Native scheduling uses the bounded UI handoff below. There are no API
send/schedule, mailbox mutation or message deletion tools; the HTTP allowlist
independently blocks those endpoints.

## Native Gmail scheduling: bounded UI handoff

The public [Gmail REST method inventory](https://developers.google.com/workspace/gmail/api/reference/rest)
and [v1 discovery document](https://gmail.googleapis.com/$discovery/rest?version=v1)
were checked on 2026-09-18. They expose no schedule-send endpoint or writable
scheduled-time field. `internalDate` is message creation time, not a delivery
timer. The [label guide](https://developers.google.com/workspace/gmail/api/guides/labels)
does not define a writable scheduling mechanism. Invented headers, scheduled
labels, internal Gmail endpoints and local timers are not supported here.

Google documents [Schedule send and Cancel send in Gmail's UI](https://support.google.com/mail/answer/9214606).
This connector prepares a **one-time structured UI handoff**, not an automated
scheduler. It never opens a browser, clicks, schedules, cancels, sends, or claims
provider-verified completion. There is no local outbox or background worker.

1. Read the exact draft. Call `gmail_prepare_native_schedule` with a fresh
   `operation_id`, `draft_id`, `expected_raw_sha256`, `scheduled_at` (absolute
   RFC3339 with offset), and `timezone` (IANA name). For example, synthetic input
   may use `2026-09-19T09:00:00-07:00` and `America/Los_Angeles`.
2. Present the complete immutable review to the authenticated owner: account,
   draft/message identity, To/CC/BCC, Reply-To, subject, entire body, threading,
   exact time and timezone. Drafting permission does not authorize scheduling.
3. Only after a separate owner request approving that exact review, call
   `gmail_begin_native_schedule_handoff` with its operation/review IDs,
   `owner_requested=true` and `authorization_source=authenticated_owner`.
   These assertions are a trusted-caller contract, **not authentication**. The
   connector cannot authenticate conversation origin. Runtime approval and the
   owner-facing coordinator remain responsible for establishing it. Fetched
   messages, pages, attachments and tool results never supply permission.
4. The helper checks the current account, exact raw bytes, provider message and
   thread identity, then durably claims uncertainty before releasing instructions.
   The coordinator may use only approved CUA in the existing dedicated Safari
   window, obeying all browser policy and macOS approvals. Identify the account
   and draft unambiguously; verify every reviewed field and the Gmail UI timezone
   immediately before the final Schedule send action. Abort on ambiguity, drift,
   expiry or denial. Pause concurrent edits. No API/UI transaction can lock out
   another Gmail client, so this remains an operator-enforced handoff boundary.
5. After the native action, inspect that exact message in Gmail Scheduled and
   its displayed absolute date/time. Preserve authoritative UI evidence in the
   coordinator's normal receipt surface. A click or missing draft is insufficient.
   The helper has no trusted UI evidence ingestion path and keeps its own Gmail
   schedule status `unknown`, even if the coordinator independently verifies it.

Schedule input must be a whole minute, 5 minutes to 365 days ahead; reviews
expire after 15 minutes and an issued scheduling handoff expires after 2 minutes.
These are connector safety bounds, not claims about Google's maximum horizon.
Missing offsets/timezones, timezone-offset mismatch, unknown `-00:00` offsets,
nonexistent local times and DST folds (even with an offset) are rejected. Gmail's
UI cannot express a fold selection reliably. Only one ordinary plain-text MIME
part with supported headers is accepted. HTML, attachments, multipart, protected,
inline, malformed or ambiguous MIME fail closed; draft editing keeps its existing
broader MIME support. This restriction can reject ordinary Gmail-authored HTML
drafts and must not be bypassed by silently changing an approved draft.

`gmail_operation_status` reads the local receipt without OAuth. Its ledger
`state` describes the workflow, not Gmail's schedule or delivery state. Repeating
a begin call returns the receipt only, with no second actionable handoff.
`gmail_reconcile_native_schedule` reads the original exact draft and records
`matching_draft_present`, `draft_changed`, `draft_absent`, or `read_failed`.
**Every observation preserves uncertainty and the draft claim.** Matching MIME
cannot prove scheduling, absence cannot distinguish scheduling from sending or
deletion, and a restored draft cannot prove which cancellation caused it.

`gmail_cancel_native_schedule_handoff` needs a separate explicit owner
cancellation request bound to the original operation/review. Stop any in-flight
scheduling UI work before cancellation. It issues at most one Cancel send handoff
with a deterministic cancellation operation ID; retries return only that receipt.
Use that returned ID for status/reconciliation. It does not delete the draft or
claim cancellation. The coordinator must identify the exact scheduled message,
use Cancel send, and verify Gmail restores the exact draft. Missing or elapsed
schedules remain unknown. The original claim is deliberately retained: no
trusted provider evidence adapter exists to clear it safely, and there is no
force-unlock tool. Do not delete ledger rows or use fresh IDs to bypass it.

### Registration and coordinator guidance

`src/tools.rs::definitions` remains the only schema. Existing opt-in stdio MCP
discovery registers the four additional tools without core-runtime changes.
Preparation and reconciliation are marked non-read-only because they persist
local state; begin/cancel also carry destructive hints because their handoffs
lead to external effects. These hints are not approval grants. Keep begin/cancel
under the owner's existing approval policy, and never auto-authorize them from
retrieved content. No live registration or runtime prompt is changed by this PR.
The standalone external MCP helper follows its existing English protocol schema;
no runtime CLI strings or generated locale catalogs are introduced.

Coordinator prompt guidance: distinguish drafting, preparing, issuing a handoff,
natively scheduling, and verifying scheduling. Report exactly the boundary
actually reached. Never announce “scheduled” from a preparation, local receipt,
caller assertion, API draft disappearance, or UI click alone. Do not substitute
another scheduler or an API send. Scheduling, cancellation and delivery status
are separate facts. A future automated completion adapter needs trusted fresh UI
evidence tied to the exact account, message, content and absolute time before it
can change the ledger's unknown state; it is not implemented here.

## Files and receipts

Attachments must be regular files under the existing operator-controlled
`extensions/personal-ops/sharing.json` `allowed_roots`. Every path component is
opened without following symlinks; hard links, devices, directories and FIFOs are
denied. Limits are 10 files, 8 MiB per file and 10 MiB combined. Filenames must be
safe basenames. Known extensions must match the MIME type, and PDF/PNG/JPEG
signatures are checked. Content-addressed bytes are captured before review;
changing the original file cannot change an already prepared draft. Paths do
not enter MIME or reviews.

`extensions/gmail-drafts/drafts.sqlite3` under the canonical configuration root
owns preparations, byte snapshots, operation claims and reconciliation receipts.
The directory is private and the database is mode 0600. Preserve the database
through upgrades and rollback; deleting it removes the at-most-once history.

Each operation is claimed before its one mutation attempt. Competing helper
operations cannot claim the same unresolved draft. An interrupted or lost write
stays uncertain across process restarts. An exact matching provider read can
confirm an apply; an exact 404 can confirm draft absence after a discard without
claiming what caused it. Read errors or mismatching content cannot authorize a
retry. A create whose response was lost may need the exact candidate draft ID
from list/read for reconciliation.

Gmail does not provide an atomic compare-and-swap for draft updates. The helper
checks both message identity and raw content before writing, but it cannot lock
the Gmail UI or another client. Pause concurrent editing of the same draft.

Mutation and reconciliation also share an OS execution lock, so a concurrent
reader cannot release a claim while its write is still running. Process death
releases that lock without clearing durable uncertainty. Every prepared update
gets its own RFC Message-ID while preserving thread, In-Reply-To and References.
Reconciliation requires that operation marker; unchanged pre-write content is
not proof of completion after a timeout.

## Account and macOS setup

The canonical account remains `GOG_ACCOUNT` on the existing `google_write` MCP
server in `config.toml`; it is never supplied by a tool argument. The helper reads
the existing `gogcli` macOS Keychain token record at `token:default:{account}`.
The OAuth `credentials.json` file supplies client metadata; the default client
secret comes from the same Keychain service at `client/default/client-secret`.
Legacy nonempty inline `client_secret` values remain supported. Malformed
metadata and unavailable or invalid secrets fail closed. Both Keychain reads
restore noninteractive mode before returning.

The token record's existing `scopes` array is the source of truth for renewal.
The helper requests exactly `gmail.compose` plus `gmail.readonly` when both are
already granted; otherwise it requests exactly `gmail.modify` only when that
literal scope is already granted. Missing, malformed or unsupported stored scope
evidence fails before renewal. Google requires refresh requests to use a
[subset of the original granted scopes](https://developers.google.com/identity/openid-connect/reference);
a permission's narrower capabilities do not make its scope string part of an
existing grant.

The returned scope set must exactly match the selected subset before the helper
uses the access token or checks the Gmail profile against the pinned account.
Broader, substituted or missing scope evidence fails closed; a denial never
triggers a retry with other scopes. Renewal failures expose only fixed
`invalid_scope`, `invalid_grant`, `invalid_client` or `unknown` categories, never
the provider's response body or description.

Neither supported subset is draft-only at [Google's OAuth layer](https://developers.google.com/workspace/gmail/api/auth/scopes).
Both authorize [draft updates](https://developers.google.com/workspace/gmail/api/reference/rest/v1/users.drafts/update).
Compose permits sending; modify also permits mailbox mutations. Accepting an already granted
modify scope therefore relies on the helper's independent HTTP allowlist to
refuse sending and all non-draft writes. It never requests the full-mail scope
or the existing grant's unrelated Calendar, settings, Pub/Sub or identity scopes.

Refresh access tokens stay in memory. This helper does not change credentials,
Keychain ACLs, scopes, backend storage or the existing Google executables. A new
signed executable may need native owner approval even with the same signing
certificate. A Keychain denial is not evidence of missing credentials.

`doctor` performs only OAuth renewal and a read-only profile check, with Keychain
dialogs disabled. After the final binary is signed, the owner can deliberately
run `doctor --interactive` from a terminal to review macOS's native access prompt.
Piped/background invocations cannot opt into that path. Approval is performed
by the owner in macOS; the command never accepts the prompt itself.

Before any local installation, follow the host's existing
`~/.zeroclaw/local-signing/README.md`. Add a distinct stable `gmail-drafts`
component/identifier to the canonical signing launcher, use the existing
certificate, sign the staged candidate with `--sign-only`, and verify the
certificate-pinned designated requirement. Keep signed rollback artifacts and
preserve every existing signing route. Do not install an ad-hoc-only build.

Once native access is verified, an operator can register a `gmail_drafts` stdio
MCP server through `zeroclaw-signed-launch gmail-drafts mcp`, then reload MCP and
verify tools against the canonical discovered schema. Set an appropriate request timeout for the
bounded provider reads and draft-page size. Keep existing Calendar and Google
read registration intact. Do not claim authenticated readiness from `schema`
or tool discovery alone. Disable only this server to roll back; retain its ledger. Once native handoffs
exist, do not point an older helper at that ledger: older draft reconciliation
could mistake a schedule preparation for a draft apply. Keep this server disabled
until a compatible reader is restored. Rolling back code never cancels Gmail's
scheduled messages; inspect those separately through the approved UI.

## Validation

This crate has its own manifest and lockfile outside the root Cargo workspace:

```sh
cargo fmt --manifest-path tools/zeroclaw-gmail/Cargo.toml -- --check
cargo clippy --manifest-path tools/zeroclaw-gmail/Cargo.toml --locked --all-targets -- -D warnings
cargo test --manifest-path tools/zeroclaw-gmail/Cargo.toml --locked
cargo build --manifest-path tools/zeroclaw-gmail/Cargo.toml --locked --release
```

The tests use synthetic mail, fake provider responses, private temporary
directories and real SQLite. They cover workflow effects, concurrency, restart
receipts, MIME preservation and the shipped stdio process. They never read the
operator's credentials or create, update, delete or send real mail. Live account
readiness and registered runtime behavior require separate deployment checks.
