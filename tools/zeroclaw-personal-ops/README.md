# Personal specialists and fixed workflows

This optional macOS helper adds a coordinator roster and general messaging/file
sharing to an existing ZeroClaw installation. It reuses native delegation,
Google draft/Calendar connectors, Reminders, GitHub and the native scheduler.
It does not replace the runtime or phone service.

## Roles

| Agent | Work | Model profile |
| --- | --- | --- |
| main | Owner conversation, routing, authorized delivery | Existing profile |
| communications | Email/text drafts and file-sharing plans | openai.terra |
| calendar_tasks | Google Calendar and Apple Reminders | openai.terra |
| task_scheduler | Native task schedules and status | openai.terra |
| coding | GitHub work and reusable Rust helper development | openai.astra, gpt-6-astra |

The coordinator uses explicit one-hop bounded delegation. Each specialist has
an explicit tool allowlist and no delegation or shared-memory tools. Bounded
delegation intersects the caller's existing tool registry with the specialist
allowlist; it is not a separate OS sandbox. Existing tool wrappers retain their
own execution boundaries. Main keeps the existing model and tools. Coding
uses the existing native OpenAI authentication route with no model fallback.

Simple named operations can run directly without paying for another agent turn.
Language understanding and writing still use a model; the helper performs file
selection validation, immutable preparation and delivery bookkeeping in Rust.

## Install

Requires the current ZeroClaw named-agent/delegate schema, existing `main`,
`default` risk/runtime profiles, `openai.sol`/`openai.terra` native model
profiles, and MCP bundles named `google_read`, `google_write`, `reminders`,
`github_cli`, and `phone_calls`. It uses the existing `imsg` installation at
`/opt/homebrew/bin/imsg` and its existing macOS permissions. No privacy prompt
is bypassed. The installer does not install dependencies or grant OS permissions.

```sh
cargo test --manifest-path tools/zeroclaw-personal-ops/Cargo.toml
cargo clippy --manifest-path tools/zeroclaw-personal-ops/Cargo.toml --all-targets -- -D warnings
cargo build --release --manifest-path tools/zeroclaw-personal-ops/Cargo.toml
tools/zeroclaw-personal-ops/target/release/zeroclaw-personal-ops install "$HOME/.zeroclaw" "$HOME/Documents/Github"
```

The operator-only installer backs up configuration and main's instructions,
validates a private candidate with the installed native loader, creates four
managed workspaces, installs the binary, and atomically replaces configuration.
It refuses existing specialist aliases, workspaces or helper installations.
It preserves all existing cron, channel, scheduler, recovery, transcription and
phone MCP declarations. Restart only the main daemon after installation when
there is no active call. The phone process, binaries, configuration and source
database must remain untouched.

`plan CONFIG_DIR GITHUB_ROOT` emits installation edits. `candidate` emits a full
candidate configuration, which may include secrets. Treat both as private
operator artifacts; never commit their output. The live configuration remains
the source of truth. Templates apply only at initial installation.

## Named operations

Run `tools` to inspect the JSON tool schemas, or `mcp CONFIG_DIR` for stdio MCP.

| Operation | Effect |
| --- | --- |
| text_prepare | Save an unsent text draft with exact recipients |
| files_prepare | Prepare general file sharing from exact absolute paths |
| voicemail_list | Read a bounded window of completed inbound screening calls |
| voicemail_prepare | Prepare transcripts or consented archived audio from that source |
| delivery_execute | Attempt up to four new items in an authorized prepared plan |
| delivery_status | Read per-item prepared, submitted or uncertain state |

Messages and files use iMessage only, without SMS fallback. An email-shaped
recipient is an iMessage handle, not email delivery. Gmail remains draft-only.
The communications specialist can prepare but cannot call delivery_execute.
Main can execute only when the owner's request explicitly covers the exact
recipients and content. The `owner_requested_send` flag is the calling agent's
assertion of that request, not independent proof of authorization or a human
approval token. The runtime/owner trust boundary and prompt-injection policy
remain necessary. No standing forwarding rules are created.

File-sharing roots are operator-owned in `extensions/personal-ops/sharing.json`.
The initial roots are the GitHub directory supplied at install and main's
`workspace/share/`. Hidden files, noncanonical paths/symlinks, unsupported file
types and files above 49 MB are rejected. Source files are copied into a private
staging directory and hashes are checked before sending. The phone archive has
a separate read-only adapter with explicit recording-consent checks. Caller
claims remain untrusted and are never executed. No recording outbox or call
record is modified.

Prepared plans expire after one hour. The separate private SQLite operations
ledger is the canonical source for plans and delivery attempts. It claims each
recipient/content/file fingerprint durably before invoking a fixed executable
with an argument array. No shell strings, arbitrary commands, URLs, recipients
from caller text, or user-selected executables are evaluated.

A failed process, crash or timeout leaves the attempt `uncertain`. Identical
content is not automatically replayed, even through another plan or after a
restart. This intentionally may suppress a legitimate later identical message;
there is no agent-exposed force-resend or ledger-reset operation. Inspect the
destination manually when resolving an uncertain send. `submitted` means the
Messages command accepted the item, not a recipient delivery/read receipt.
For batches, inspect status and continue the same plan for remaining prepared
items, without retrying uncertain items. Partial success is reported per item.

Google event editing/invitations and email sending are not added by this package.
Calendar creation and Reminders mutations reuse the existing narrow connectors.
The scheduler uses the existing native cron interface and preserves task state.
The coding specialist builds/test helpers in repository source and returns an
installation proposal; development does not authorize live deployment.

## Validation and rollback

Tests exercise real temporary SQLite databases and fake delivery functions;
they never send messages or place calls. They cover input rejection, file-root
and symlink boundaries, immutable snapshots, attachment tampering, expiry,
missing owner-request assertions, duplicate claims across connections/restarts,
partial failure, phone read-only access and recording consent.

Live validation should include all four native aliases, delegation to each
specialist, an Astra/GitHub read, MCP tools/list, and existing phone health and
unsigned-request rejection. A live message, attachment or phone call requires
an explicit owner test request; health checks do not prove delivery.

Rollback: restore `config.toml` and `main-AGENTS.md` from the installation's
private `backups/personal-specialists-<timestamp>/` snapshot to their original
locations, then restart only the main daemon. Do not restore old authentication,
phone databases or delivery receipts. Keep `extensions/personal-ops/` intact so
uncertain attempts cannot be replayed by reinstalling. The new agent directories
and helper may remain dormant after the old config is restored. Preserve any
subsequent configuration changes when rolling back a later installation.
