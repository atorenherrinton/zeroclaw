# TypeSafe System One for ZeroClaw

A standalone Rust MCP helper for macOS and Linux exposing one `typesafe_system_one` tool. It batches
semantic judgments over supplied application state through
`POST https://api.typesafe.ai/v1/systemone`, using `jev-latest`. ZeroClaw exposes
the tool as `typesafe__typesafe_system_one` when the server is named `typesafe`.

This optional vendor integration uses the existing MCP dispatcher, tool policy,
receipts, and agent bundles. It requires no daemon rebuild or reasoning-provider
change. The helper has no action executor or approval interface. Its output is
untrusted advisory evidence; even a probability of 1 never grants consent to
send messages, purchase, delete, change permissions, or perform external actions.
Those operations must use ordinary ZeroClaw tools and their existing policies.

## Install and configure

Build and verify from the repository root:

```sh
cargo test --locked --manifest-path tools/zeroclaw-typesafe/Cargo.toml
cargo clippy --locked --manifest-path tools/zeroclaw-typesafe/Cargo.toml --all-targets -- -D warnings
cargo build --locked --release --manifest-path tools/zeroclaw-typesafe/Cargo.toml
```

The default operator configuration directory is
`~/.zeroclaw/extensions/typesafe`. It must be owned by the current user, mode
0700, and must not be a symlink. Create an owner-only `settings.json` (0600):

```json
{"enabled": true, "allowed_domains": ["api.typesafe.ai"]}
```

A missing settings file, disabled policy, missing exact domain entry, or absent
protected API key blocks inference. This allowlist belongs to this helper;
ZeroClaw's generic `http_request` allowlist does not govern external MCP servers.
No wildcard or suffix entry authorizes this endpoint. The helper cannot accept
an alternative URL, proxy, header, or model in tool arguments. Redirects and
ambient HTTP proxies are disabled. Ordinary TLS certificate verification stays on.

On macOS, read the installation's `~/.zeroclaw/local-signing/README.md`, add a
`typesafe` component mapping to the canonical signing launcher with stable
identifier `com.zeroclaw.local.typesafe`, and use the existing **ZeroClaw Local
Signing** certificate. Sign the staged candidate with
`zeroclaw-signed-launch typesafe --sign-only /absolute/path/to/candidate` before
atomic installation. Verify its certificate-pinned designated requirement;
plain signature verification alone is insufficient. Preserve existing mappings,
private key material, daemon bytes, and a rollback backup.

Install the executable as
`~/.zeroclaw/extensions/typesafe/zeroclaw-typesafe`, then enter the key at a local
terminal using hidden input:

```sh
~/.zeroclaw/bin/zeroclaw-signed-launch typesafe set-key
~/.zeroclaw/bin/zeroclaw-signed-launch typesafe status
```

The key is atomically stored in the owner's `api-key` file with mode 0600. Never
put it in tool arguments, chat, shell command text, checked-in files, or MCP
configuration. Symlink, hard-linked, nonregular, wrong-owner, or broadly readable
secret files are rejected. Policy and credentials are reread for every call, so
key rotation and disabling access require no helper restart. `status` performs
no network request and prints only readiness information, never the key. A CLI
`--config-dir PATH` override is available for isolated operator deployments.

Register the existing signed-launch route in ZeroClaw configuration (replace the
example command with the actual absolute launcher path):

```toml
[[mcp.servers]]
name = "typesafe"
transport = "stdio"
command = "/absolute/path/to/zeroclaw-signed-launch"
args = ["typesafe", "mcp"]
tool_timeout_secs = 30

[mcp_bundles.typesafe]
servers = ["typesafe"]
exclude = []
```

Append `"typesafe"` to the desired agent's existing `mcp_bundles`. Preserve its
other bundles and risk profile. Reload through the installation's supported
configuration reload path at an idle point and check fresh-session discovery.
No global approval or autonomy settings need changing.

## Tool input and output

```json
{
  "state": {"message": "The deployment is blocked by a build error."},
  "questions": {
    "route": {
      "type": "choice",
      "instructions": "Which queue best matches `message`?",
      "criteria": {"engineering": "Build or software problems", "other": null}
    },
    "urgency": {
      "type": "score",
      "instructions": "How time-sensitive is `message`?",
      "criteria": ["Routine", "Blocking current work", "Immediate emergency"]
    },
    "is_question": {
      "type": "noul",
      "instructions": "Does `message` ask a question?"
    }
  }
}
```

State and instructions accept strings, objects, or arrays. Questions in a batch
are independent; put complete meaning in each question's instructions. Choice
returns a supplied option, probabilities, and confidence. Score returns a
fractional zero-indexed level, legend, probabilities, and confidence. Noul
returns a yes-probability with no separate confidence. The response envelope
marks `authorizes_external_actions` as false. Code should choose uncertainty
thresholds for the task; this helper does not invent approval thresholds.

Requests allow up to 32 questions, 64 choices or score levels, and 128 KiB of
argument JSON. Responses are limited to 256 KiB.

The helper validates all question and answer types and identifiers, offered
choices, score levels, probability ranges and distributions. Unexpected response
content cannot add tool calls or authorization fields. Calls and responses are
size-bounded and HTTP requests have a 20-second deadline. Errors are sanitized
and do not echo remote bodies, credentials, or supplied state. Requests are not
automatically replayed after transport failures. Rate-limit/overload errors
should be retried later with backoff by the caller.

Only the explicitly supplied state and questions go to TypeSafe. The helper
never fetches conversations, reads application documents, or collects local
state itself. It does not persist request/answer content; ZeroClaw may retain
tool inputs and outputs in its ordinary conversation/audit history. Send only
the information needed for the requested judgments.

## Validation and rollback

Tests use synthetic fixtures and loopback HTTP servers; they require no vendor
credentials. They cover request/answer contracts, fixed network policy, errors,
protected storage, and MCP framing. A real authenticated API smoke test is a
separate check requiring an operator-provided TypeSafe key.

Disable immediately by changing helper `settings.json` to `"enabled": false`.
To uninstall, remove only the `typesafe` bundle grant/server and reload while
idle. Restore the prior signed helper and policy from the installation backup
when rolling back an update. Preserve any subsequently supplied key. Remove a
new signing mapping only after its MCP route has been removed; do not replace
the entire signing launcher with a stale copy.

API contract: [TypeSafe HTTP API](https://docs.typesafe.ai/api.md),
[primitives](https://docs.typesafe.ai/primitives.md), and
[confidence](https://docs.typesafe.ai/confidence.md).
