# ZeroClaw service CLI

A signed macOS CLI and stdio MCP server for using specific Vercel and Resend API tokens without returning them to the model. It calls fixed official HTTPS APIs directly; it does not wrap arbitrary shell commands or grant general Keychain access.

## Local setup

Run in your own Terminal, or double-click the installed `Setup Service Tokens.command`:

```sh
~/.zeroclaw/bin/zeroclaw-signed-launch service-cli setup
```

Enter each token at the hidden prompt. Enter alone skips a slot and preserves any existing token. Tokens must not be pasted into chat, command arguments, configuration files, or MCP calls. Individual setup is available with `setup vercel`, `setup resend`, or `setup resend-send`. No token is displayed on success.

| Slot | Purpose | Credential |
| --- | --- | --- |
| `vercel` | Project/deployment/env metadata and explicitly requested env updates | Vercel API token scoped to the intended account/team, with the shortest practical expiry |
| `resend` | Domain verification status and API-key metadata | Resend Full access diagnostics key; sending-only keys cannot read domains |
| `resend-send` | Website sending credential copied directly to Vercel | Separate Resend Sending access key restricted to the website's domain |

Manage tokens locally at https://vercel.com/account/tokens and https://resend.com/api-keys. Skip credentials you do not need. This helper cannot recover existing browser passwords or tokens. Native Safari AutoFill remains the browser login path.

Tokens are stored as generic-password items in the default macOS Keychain, service `com.zeroclaw.local.service-cli`, accounts matching the three slots. Normal creator-app Keychain access controls apply. Keep the signed helper identity unchanged across builds. macOS may require an initial local authorization. Calls fail without prompting if the Keychain is locked or access is denied; they do not unlock the Mac.

## Commands

```sh
~/.zeroclaw/bin/zeroclaw-signed-launch service-cli status
~/.zeroclaw/bin/zeroclaw-signed-launch service-cli call read '{"action":"vercel_projects"}'
~/.zeroclaw/bin/zeroclaw-signed-launch service-cli call read '{"action":"vercel_env","project":"prj_EXACT_ID","team_id":"team_EXACT_ID"}'
~/.zeroclaw/bin/zeroclaw-signed-launch service-cli call read '{"action":"resend_domains"}'
~/.zeroclaw/bin/zeroclaw-signed-launch service-cli call read '{"action":"resend_domain","id":"EXACT_DOMAIN_ID"}'
```

MCP tools are `service_cli__status`, `service_cli__read`, and `service_cli__copy_resend_key`. `read` supports `vercel_projects`, `vercel_deployments`, `vercel_env`, `resend_domains`, `resend_domain`, and `resend_api_keys`. Use exact IDs from metadata. Vercel pagination uses numeric `cursor` from `next_cursor`; Resend uses `after` from `next_after`.

After an explicit owner request for the exact project and environment, `copy_resend_key` accepts:

```json
{"project":"prj_EXACT_ID","team_id":"team_EXACT_ID","target":"production","owner_requested":true}
```

This sends the `resend-send` token directly to Vercel as sensitive `RESEND_API_KEY`. Target is production or preview; team_id is optional. It does not deploy or send mail. `owner_requested` is an agent authorization assertion, not an independent human approval mechanism. Tool installation itself does not authorize a production change. An `uncertain` response requires reconciliation before a retry. No writes are automatically retried.

## Boundaries

The MCP/CLI has no secret export, arbitrary command, custom URL, proxy, redirect, environment-value read, or Keychain enumeration operation. Reads return allowlisted metadata only. Raw error bodies and submitted secret values are omitted. API tokens travel only in an Authorization header or the specific Vercel secret-update body over verified HTTPS. Input/output sizes and request time are bounded.

This keeps credentials out of normal model context; it is not a sandbox against arbitrary programs running as the same macOS user, an administrator, or a modified helper. The process and networking libraries necessarily hold token bytes in memory. Helper-owned credential buffers are zeroized; total process-memory erasure is not guaranteed. Do not add debug HTTP logging or credential-bearing subprocess arguments.

To rotate a token, run local setup for that slot. To revoke it, revoke it at its provider and remove only the matching service/account item in Keychain Access. Do not grant every application access to the item.

## Build and install

```sh
cargo fmt --manifest-path tools/zeroclaw-service-cli/Cargo.toml --check
cargo test --manifest-path tools/zeroclaw-service-cli/Cargo.toml
cargo clippy --manifest-path tools/zeroclaw-service-cli/Cargo.toml --all-targets -- -D warnings
cargo build --release --manifest-path tools/zeroclaw-service-cli/Cargo.toml
```

Read `~/.zeroclaw/local-signing/README.md` before updates. The canonical launcher component is `service-cli`, identifier `com.zeroclaw.local.service-cli`, pinned to the existing ZeroClaw Local Signing certificate. Sign the staged candidate through `zeroclaw-signed-launch service-cli --sign-only /absolute/candidate`, verify the certificate-pinned designated requirement, and atomically install to `~/.zeroclaw/extensions/service-cli/zeroclaw-service-cli`. Preserve the wrapper MCP route and a rollback backup.

`self-test-keychain` creates, reads, and removes a uniquely named synthetic item in a separate self-test service. It never reads real tokens or contacts providers. Unit tests use synthetic tokens and loopback fixtures only.
