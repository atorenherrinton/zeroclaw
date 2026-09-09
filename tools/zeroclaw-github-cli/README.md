# GitHub CLI MCP helper

This standalone macOS helper runs the existing authenticated Homebrew GitHub CLI
over MCP stdio. It brings the previously local helper under source control and
repairs archive/output-budget failures without changing the daemon's admission
rules or classifying this mixed read/write tool as read-only.

The executable is `/opt/homebrew/bin/gh`. Authentication remains in the existing
`$HOME/.config/gh` configuration. The working directory defaults to
`$HOME/Documents/Github`; an explicit existing path must resolve beneath that
root. No token is copied into this helper's configuration or output.

## Output and execution contract

`run` accepts the existing `args` array and optional `path`. Successful execution
returns JSON with `command_executed`, `success`, `exit_code`, stdout/stderr,
original byte counts, and explicit truncation/binary flags. Nonzero exit status
sets MCP `isError` while retaining the same execution evidence.

Each stream's presentation is limited to 512 JSON-encoded bytes, with a marked
UTF-8 excerpt. Binary output is omitted explicitly, without lossy conversion or
a claim that a file was saved. Omitted output includes a warning against replaying
writes. Large text can be inspected through narrower `--jq`/`--limit` requests,
or a clone followed by paged file reads. Exact small output stays intact.

Capture retains at most 8 MiB per stream and continues draining excess output
while counting it, allowing the command to finish normally. A 120-second process
deadline remains. Timeout/stream errors report uncertain execution and require
reconciliation; the helper never automatically retries a command.

Repository tarball/zipball API endpoints are rejected before command execution.
They are binary downloads, not model-visible text. Other binary output is omitted
after execution with the exit status retained. No automatic attachment store or
second copy of omitted output is created. Metadata and extreme batches remain
subject to the daemon's exact admission limits.

## Clone transport

Qualified repository and SSH clone arguments become explicit github.com HTTPS
URLs. Process-scoped Git URL rewrites also cover one-part repository shorthand
resolved by `gh`. The same two fixed rewrites are stored only in the new clone,
so later fetch/push uses HTTPS. Existing global Git/SSH preferences and host-key
verification are unchanged. Caller-provided arbitrary clone flags remain blocked.

## Build and verification

```sh
cargo test --locked --manifest-path tools/zeroclaw-github-cli/Cargo.toml
cargo clippy --locked --manifest-path tools/zeroclaw-github-cli/Cargo.toml --all-targets -- -D warnings
cargo build --locked --release --manifest-path tools/zeroclaw-github-cli/Cargo.toml
```

Tests cover pre-execution archive rejection, encoded/UTF-8 bounds, binary
omission, failed-command status, execution exactly once, bounded stream capture,
HTTPS rewrites, working-directory boundaries, and the existing command policy.

For local macOS updates, first read the operator's
`~/.zeroclaw/local-signing/README.md`. Register `github-cli` in the canonical
signing wrapper with the existing component identifier and signing certificate;
do not replace a stable identity with an ad-hoc signature. Sign a staged candidate
with `zeroclaw-signed-launch github-cli --sign-only /absolute/candidate`, verify
its certificate-pinned requirement, retain a rollback backup, and install it
atomically. The MCP command should use the canonical signing wrapper with
`args = ["github-cli"]`. Signing does not itself grant macOS permissions.
