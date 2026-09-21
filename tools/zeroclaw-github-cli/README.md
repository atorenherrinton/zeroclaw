# GitHub CLI MCP helper

This standalone macOS helper runs the existing authenticated Homebrew GitHub CLI
over MCP stdio. It brings the previously local helper under source control and
repairs archive/output-budget failures without changing the daemon's admission
rules or classifying this mixed read/write tool as read-only.

## No code changes: file issues, then stop

ZeroClaw does not change code. Through this helper it can file GitHub issues in
the respective repository and inspect GitHub read-only; it cannot open, merge or
edit pull requests, clone or fork repositories, run workflows, publish releases,
touch secrets, or write through the API. The policy is an allowlist checked
before `gh` runs; anything not named below is refused with a `Not executed:`
notice and no process is started.

| Command | Permitted subcommands |
| --- | --- |
| `issue` | `create`, `comment`, `list`, `view`, `status` |
| `pr` | `list`, `view`, `status`, `diff`, `checks` |
| `repo` | `view`, `list` |
| `run`, `workflow`, `release` | `list`, `view` |
| `label` | `list` |
| `search` | `code`, `commits`, `issues`, `prs`, `repos` |
| `status` | (read-only) |
| `api` | `GET` only; field, `--input` and method-override forms that write are refused |

Issue filing has two extra rules. Assigning an issue to Copilot is refused,
because that starts a coding agent that opens a pull request. `--body-file` is
refused, so issue text is always explicit and a local file cannot be posted to a
possibly public repository. To request a change, file an issue:

```text
issue create --repo OWNER/NAME --title "..." --body "..."
```

Authentication, aliases, extensions, custom hosts, and `--force` remain blocked.
This narrows the helper only. See the operator note below on what other tools an
agent may still hold; the shell and file tools are governed by risk profiles.

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
writes. Large text can be inspected through narrower `--jq`/`--limit` requests or paged
contents API reads. Exact small output stays intact.

Capture retains at most 8 MiB per stream and continues draining excess output
while counting it, allowing the command to finish normally. A 120-second process
deadline remains. Timeout/stream errors report uncertain execution and require
reconciliation; the helper never automatically retries a command.

Repository tarball/zipball API endpoints are rejected before command execution.
They are binary downloads, not model-visible text; read files through the
contents API instead. Other binary output is omitted after execution with the
exit status retained. No automatic attachment store or
second copy of omitted output is created. Metadata and extreme batches remain
subject to the daemon's exact admission limits.

## Build and verification

```sh
cargo test --locked --manifest-path tools/zeroclaw-github-cli/Cargo.toml
cargo clippy --locked --manifest-path tools/zeroclaw-github-cli/Cargo.toml --all-targets -- -D warnings
cargo build --locked --release --manifest-path tools/zeroclaw-github-cli/Cargo.toml
```

Tests cover the issue-only allowlist (permitted and refused tables, Copilot and
`--body-file` refusals, GET-only `api`), refusal before any process or working
directory is touched, pre-execution archive rejection, encoded/UTF-8 bounds,
binary omission, failed-command status, execution exactly once, bounded stream
capture, and working-directory boundaries.

## Operator note

Narrowing this helper does not by itself remove every way an agent could edit
code. A profile that still allows `shell` with `allowed_commands = ["*"]`,
`file_write`/`file_edit`, `git_operations`, or a coding-agent tool such as
`codex_cli` can. Remove those from the risk profile of any agent that must not
change code, and keep repository writes out of its `allowed_roots`.

The helper's version is 0.2.0: `repo clone` and the clone transport were removed
because their only purpose was putting code on disk. Roll back by reinstalling the
previous signed binary from the retained backup.

For local macOS updates, first read the operator's
`~/.zeroclaw/local-signing/README.md`. Register `github-cli` in the canonical
signing wrapper with the existing component identifier and signing certificate;
do not replace a stable identity with an ad-hoc signature. Sign a staged candidate
with `zeroclaw-signed-launch github-cli --sign-only /absolute/candidate`, verify
its certificate-pinned requirement, retain a rollback backup, and install it
atomically. The MCP command should use the canonical signing wrapper with
`args = ["github-cli"]`. Signing does not itself grant macOS permissions.
