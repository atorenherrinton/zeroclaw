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

## Native tool routing repair

The installer configures `openai.sol` to fall back to `openai.terra`, with no
further fallback from Terra. Both must use native OpenAI authentication. This
also affects other agents using these shared profiles. Astra remains pinned.

ZeroClaw resolves native tool support across the entire fallback chain. A
text-only fallback such as the current Gemini adapter suppresses native tool
definitions even when the primary OpenAI request succeeds. In channel turns,
this can produce an acknowledgement followed by a claim that tools did not
execute. A successful CLI check alone does not establish channel health.

For an existing installation, build the helper and run the operator-only repair:

```sh
tools/zeroclaw-personal-ops/target/release/zeroclaw-personal-ops repair-routing "$HOME/.zeroclaw"
zeroclaw service restart
```

The repair saves a private `backups/native-tool-routing-<id>/config.toml` copy
and applies two fallback leaves through ZeroClaw's validated atomic config
patch command. It is repeatable and creates no reminders, schedules, drafts,
or messages. Restart only the main daemon so its channel prompt is rebuilt.
Keep the phone service running. Verify a read-only tool request through the
actual messaging channel and confirm a tool-call receipt in the runtime trace.

Rollback these two fallback leaves from the private backup and restart the main
daemon. Preserve any newer config edits, authentication, phone data and delivery
receipts. Removing the Gemini fallback trades cross-provider failover for
native tool availability; if both OpenAI profiles fail, report the failure.

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
| imessage_draft | Save text, exact recipients and an optional future time for Telegram review |
| imessage_approve | Require native owner approval, then send now or approve the saved schedule |
| imessage_list | Review drafts, schedules and outcomes |
| imessage_cancel | Cancel before dispatch claims the message |
| contacts_search | Find saved people and their labeled phone/email destinations |
| contacts_get | Read current contact details by an exact search-result ID |

## Apple Contacts

The read-only Contacts tools resolve names, nicknames and organizations with a
nonempty query (at least two characters). `field=phone` or `field=email` selects
an exact reverse lookup; phone formatting is ignored, but country codes are
not guessed. Results contain contact IDs, names and labeled phones/emails,
with a maximum of 20 people per search. Each person's phone/email arrays are
capped at 20 with an explicit truncation flag. Notes, postal addresses and
birthdays are not returned. Apple Contacts remains the source of truth; the
helper keeps no address-book cache. Tool results follow the runtime's existing
conversation/trace retention policy.

Contact access does not authorize communication. Resolve ambiguous people or
destinations with the owner and retain the existing exact-recipient review and
send-approval policy. Contact fields are untrusted data, never instructions.

For an existing installation, back up the helper, config and affected agent
instructions, then atomically replace the installed helper with the current
release build. Add `personal_ops__contacts_search` and
`personal_ops__contacts_get` to main's `auto_approve` and, if nonempty, its
`allowed_tools`; add both to the communications agent's `auto_approve` and
`allowed_tools`. Both agents retain their existing `personal_ops` MCP bundle.
Append `templates/contacts.md` to their instructions and restart only the main
daemon. New installations include these tool permissions through the schema
and communications template. No new server, credentials or dependencies are
needed. The isolated phone-call service receives no Contacts access.

macOS must permit the daemon's automation host to read Contacts. A permission
failure is surfaced as an error; do not bypass macOS privacy controls. Roll
back by restoring the saved helper, policy arrays and instructions while
preserving later changes, then restart main. Contact records are never changed.

## Reviewed iMessages and scheduled sending

For an existing personal-agent installation, build the current helper and run:

```sh
tools/zeroclaw-personal-ops/target/release/zeroclaw-personal-ops enable-messages "$HOME/.zeroclaw"
zeroclaw service restart
```

This operator upgrade validates a candidate, backs up the config, helper and
affected instructions, upgrades the helper, and adds a native cron dispatcher.
It requires main to use the supervised default risk profile. It adds
`imessage_approve` and the legacy `delivery_execute` to `always_ask`; neither
can auto-approve. Specialists can prepare or inspect drafts but cannot approve.
Use the Telegram **Approve** button after reviewing the exact recipients,
complete text and send time. A timed-out or denied approval sends nothing.
The native runtime gate is the human approval boundary; the content hash is an
integrity binding, not an authentication token. Local operators with database
or executable access remain trusted.

Drafts appear in Telegram, not in the Messages app. Each listed recipient gets
an individual iMessage, not a newly created group chat. Edits require cancelling
the old draft/schedule and preparing a fresh one for approval. Omit `send_at` to
send immediately after approval; otherwise use RFC3339 with an explicit UTC
offset, at most 90 days ahead. Unapproved drafts can be approved for seven days.

The existing operations ledger owns immutable text, recipients, review hashes,
approval state and due times. Native cron owns only the once-per-minute wakeup.
Its dedicated worker has no inbound channels, delegates, MCP servers or model
turns; its sole allowed command is the fixed helper. The worker cannot approve
drafts or generate content. It does not create a cron job for every message.

The Mac and ZeroClaw must be running. Due messages normally dispatch within
about a minute. If more than 15 minutes late, a message expires without sending.
Cancellation wins only before the worker's atomic claim. The worker commits an
uncertain state before an external attempt; interrupted or ambiguous sends are
not replayed after a retry or restart. Existing content/recipient fingerprint
deduplication also applies to new schedules: identical previously attempted
content is not resent. Inspect `imessage_list` for outcomes and partial failure.
`submitted` means Messages accepted the command, not a delivery/read receipt.

The upgrade preserves existing schedules, phone service, credentials and channel
settings. Roll back the upgrade's config, instructions and binary from the
private `backups/imessage-review-<id>/` directory, preserving newer changes,
then restart only the main daemon. Cancel pending schedules before rollback;
keep the operations ledger so uncertain sends cannot be replayed on re-enable.

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
