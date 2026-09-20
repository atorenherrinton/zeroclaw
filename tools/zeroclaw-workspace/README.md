# Guarded native Google Docs connector

Standalone Rust stdio MCP helper. `workspace_read` discovers app-visible Docs,
reads metadata and all tabs, and compares exact plain-text hashes.
`workspace_apply` accepts only an existing owner-granted operation ID. It creates
one new native Google Doc, inserts the approved UTF-8 text into its blank body,
and reads it back. It cannot overwrite an existing document, share, email,
delete, transfer ownership, upload files, run Apps Script, or operate on Sheets.

## Owner setup

Use the canonical signed launcher from an owner-operated Terminal:

```sh
zeroclaw-signed-launch workspace doctor --interactive
```

The missing-scope failure previously ended with `owner incremental consent for
drive.file required; no broad-scope fallback`. Interactive doctor now offers a
separate Workspace authorization: type `AUTHORIZE`, complete native macOS owner
authentication, open the displayed Google URL, select the configured account and
allow per-file Drive access. The callback expires after 180 seconds; cancellation
or any mismatch saves no credential. No OAuth or native prompt can be initiated
through MCP. The doctor checks scope and account, not document accessibility.

Only `https://www.googleapis.com/auth/drive.file` is requested and accepted.
The installed-app flow uses random state, PKCE S256, a random IPv4 loopback port,
a bounded callback, the existing client, and an exact account check using Drive
about. There is no broad-scope fallback. Google documents that installed apps do
not support incremental authorization; this flow therefore stores a separate
Workspace offline credential. It does not replace or revoke existing Gmail or
Calendar tokens, alter their renewal behavior, or change Keychain ACLs.

Google may require Drive and Docs API enablement for the established desktop
client. A client incompatible with loopback authorization must be corrected by
the owner; the helper does not choose a different client or redirect type.
The exact returned scope must match, including on renewal. Unexpected grants,
missing offline credentials and invalid grants fail closed. Refresh rotations
are atomically persisted only after account verification.

Authorize one document after reviewing the source file:

```sh
zeroclaw-signed-launch workspace authorize OPERATION_ID 'Document title' /absolute/path/to/content.md
```

The issuer displays JSON-escaped exact content, title, account and intent SHA256.
Type that complete digest, then complete a fresh native macOS owner-authentication
prompt bound to that intent. A pipe, cancellation, unavailable native auth, or
non-macOS host cannot issue a grant. OAuth consent does not grant a document
operation. File bytes become **plain text**, including literal Markdown syntax;
this feature does not render Markdown formatting. Google's terminal newline is
additional to the exact inserted bytes. No local-file-reading MCP tool exists.

Then call `workspace_apply` with exactly:

```json
{"operation_id":"OPERATION_ID"}
```

Repeat this same operation ID to reconcile an uncertain result. Never authorize
another ID as an automatic retry. Once verified, replay performs a fresh readback
and returns the same document URL only if its content still matches.
An inaccessible preexisting document does not trigger broader access: authorize
a new document instead. This implementation deliberately has no existing-ID write
argument and no Picker or sharing workaround.

## Canonical state and failure behavior

Google owns document state. Existing `google_write` configuration owns the pinned
account, resolved on every connection. Existing client metadata/Keychain owns the
OAuth client. New isolated credentials and immutable document intents live in
`ZEROCLAW_CONFIG_DIR/workspace-native-v1` (default `~/.zeroclaw`). The new grant
record creates the authority fact; MCP cannot modify its account, client, title,
content or digest. Owner authentication uses a fresh LocalAuthentication context
with biometric reuse disabled, plus exact digest confirmation in Terminal.

The directory is private mode 0700; regular files are 0600. Symlinks, unexpected
owners and hard-linked files fail closed. An OS file lock serializes journal and
credential operations. Updates write a new file, fsync it, atomically rename it,
then fsync the directory. Every grant/phase record has an HMAC authenticated with
a separate Keychain key created only after native owner authentication. MCP
Keychain reads disable native UI. No credential/key material is printed.

Host policy must deny model shell/file access to the journal, credential store,
Keychain administration and owner issuer. Native authentication and sealed records
prevent a model boolean or fabricated journal from minting authority; they do not
isolate a compromised macOS account or prevent rollback of a previously valid
journal by an actor with filesystem control. Preserve the current journal across
updates and never restore old receipts. This is a required integration boundary.

The journal records dispatch intent **before** each side effect. Creation uses
Drive's metadata-only native Docs creation endpoint, with a private app property
bound to the exact intent/account/client. No upload endpoint exists. If a create
response is lost, reconciliation searches that property and requires exactly one
owned, untrashed native Doc with the exact title and marker. Zero results,
multiple results, pagination or mismatches stop; no second create is dispatched.
Google indexing delay may require another read-only reconciliation attempt.

Population requires one complete plain tab, a blank body and an exact required
revision. The helper sends a single Docs `insertText` batch. After any uncertain
population it reads the same document, accepting only exact intended text. It
never retries that insert, even if the body still appears blank. This conservative
choice can strand a blank document after a pre-dispatch crash or an API rejection;
owner review is required and no automatic deletion or reset exists.

All HTTP origins, paths, methods and bodies are internally constructed. IDs,
input/output sizes and revisions are validated. Redirects, proxies and automatic
retries are disabled. Provider errors expose bounded categories/statuses, not
response bodies. OAuth refresh after an uncertain write cannot reset its journal.
Provider document content remains untrusted data, never authority.

## Reads and verification

`schema` generates both strict schemas from the dispatch types; unknown fields
and actions reject before credential access. Examples for `workspace_read`:

```json
{"action":"drive_list","name_contains":"Example","page_token":null}
{"action":"drive_metadata","file_id":"example_document_id"}
{"action":"docs_read","document_id":"example_document_id"}
{"action":"docs_verify","document_id":"example_document_id","tab_id":"t.0","expected_text_sha256":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","expected_revision_id":null}
```

Reads request all tabs and inline suggestions with a 4 MiB provider-response cap.
Exact-tab verification rejects unsupported rich structures, suggestions and
oversized text; it does not prove other tabs or formatting. Mutation readback
additionally requires a single tab without children, legacy bodies or suggested
structures, so omitted tabs cannot silently count as whole-document proof.
Rich documents remain readable as bounded raw JSON. Discovery returns 20 results
per page. `drive.file` only sees files authorized for that OAuth client; knowing
an ID or opening it in a browser does not confer API access.

## Validation and coordinator integration

```sh
./scripts/ci/workspace_native_gate.sh
./scripts/ci/gmail_drafts_gate.sh
cargo build --locked --release --manifest-path tools/zeroclaw-workspace/Cargo.toml
```

These standalone crates are outside root workspace tests. The native gate checks
formatting, strict Clippy and hermetic tests. CI workflow wiring is deferred;
this change does not edit `.github/workflows`. The coordinator must require this
gate in review, plus normal required CI, before merge. No credential permission
bypass is needed to push functional files without a workflow change.

Before signing/installing, read the current local-signing README and retain a
signed rollback executable. Use the existing `workspace` component, certificate,
identifier `com.zeroclaw.local.workspace` and designated requirement:

```sh
zeroclaw-signed-launch workspace --sign-only /absolute/path/to/candidate
```

Verify the certificate-pinned requirement before atomic installation. Do not
replace approved Gmail/Calendar executables or rebuild the frozen auth core.
Signing preserves identity but does not grant native Keychain or Google consent.
After owner setup, noninteractive doctor must work with the same installed bytes.

Coordinator-only MCP configuration sample (substitute the absolute launcher path):

```toml
[[mcp.servers]]
name = "workspace"
transport = "stdio"
command = "/absolute/path/to/zeroclaw-signed-launch"
args = ["workspace", "mcp"]
tool_timeout_secs = 300
max_response_bytes = 16777216
```

Preserve existing account configuration and other MCP routes. Restrict this server
to the intended owner agent; deny access to its private state through shell/file
and background/delegation paths. Reload using the documented host configuration
flow, inspect live registration for exactly `workspace_read` and `workspace_apply`,
and verify service health. Native prompts and real Google effects require owner
operation and are not established by hermetic tests.

Rollback disables the new MCP registration or restores its signed backup through
the canonical wrapper and reloads the affected connector. Preserve credentials,
Keychain keys, current journal and remote documents; never restore old journal
snapshots or replay an uncertain create. A read-only backup cannot consume new
grant records but must leave them intact.

References: [native OAuth](https://developers.google.com/identity/protocols/oauth2/native-app),
[Drive file creation](https://developers.google.com/workspace/drive/api/reference/rest/v3/files/create),
[Docs batch updates](https://developers.google.com/workspace/docs/api/reference/rest/v1/documents/batchUpdate).
