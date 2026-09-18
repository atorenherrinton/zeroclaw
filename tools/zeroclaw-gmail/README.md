# Gmail draft connector

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
cleanup. There are no send, schedule, mailbox mutation or message deletion tools;
the HTTP allowlist independently blocks those endpoints.

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
the existing `gogcli` macOS Keychain record and OAuth client configuration. It
requests and verifies exactly `gmail.compose` and `gmail.readonly`, and checks
the returned Gmail profile against the pinned account. Broader or missing scope
evidence fails closed. Compose itself permits sending at Google's OAuth layer;
the helper's separate HTTP allowlist refuses it.

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
verify the seven discovered tools. Set an appropriate request timeout for the
bounded provider reads and draft-page size. Keep existing Calendar and Google
read registration intact. Do not claim authenticated readiness from `schema`
or tool discovery alone. Disable only this server to roll back; retain its ledger.

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
