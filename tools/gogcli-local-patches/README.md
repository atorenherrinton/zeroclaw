# Local Google CLI patches

This kit reconstructs the two Google clients used by the local ZeroClaw
connectors from pinned gogcli and keyring sources. `manifest.json` is the source
of truth for the base commit, module checksum, Go toolchain and patch checksums.
No installed executables, credentials or host-specific signing material belong
in this directory.

Upstream license notices are retained in `LICENSE.gogcli` and `LICENSE.keyring`.

## Behavior

- Both clients keep routine short-lived OAuth access-token refreshes in memory
  when using native macOS Keychain. File-backed stores retain persistent caching.
  Refresh-token rotations, granted-scope changes and account changes still save.
- Background execution (`--no-input`, non-terminal stdin, or MCP) disables native
  Keychain dialogs and fails closed if access is blocked. Interactive Terminal
  commands retain native approval prompts.
- Keychain access errors remain distinguishable from absent items. The CLI
  explains that native approval or unlocking is needed, instead of recommending
  OAuth reconnection solely because access was denied.
- Both clients preserve the local Calendar insert single-attempt behavior and
  explicit `guestsCanModify=false` serialization. Only `gog-calendar-patch` adds
  the existing raw API `--if-match` and `--single-attempt` transport guards.

Legacy macOS Keychain data updates rebuild the secure storage group and remove
partition ACLs. With two locally signed clients, writing the shared token for
every access-token refresh can remove the other client's approval. See Apple's
[`ItemImpl::updateSSGroup` implementation](https://github.com/apple-oss-distributions/Security/blob/main/OSX/libsecurity_keychain/lib/Item.cpp).
Avoiding routine writes addresses that loop; it does not grant Keychain access.
Durable credential changes, rebuilt binaries and a locked Keychain can still
require deliberate owner interaction.

## Reconstruct and validate

Prerequisites: Python 3.9+, Git, the Go version pinned in `manifest.json`, and
the macOS C toolchain. Normal Go module and source downloads require network
access. The upstream Go sums and the additional keyring checksum are verified.

From the ZeroClaw repository root, choose a new staging directory outside the
repository:

```sh
python3 tools/gogcli-local-patches/build.py /absolute/new/staging-directory
```

This reconstructs both variants, runs the affected Go package suites and the
pure Keychain error-mapping regression, then builds candidates under `bin/`.
It does not run the dependency's credential-mutating native integration tests.
`--source-only` reconstructs inputs without compiling or running tests.
`--repository /absolute/local/gogcli-checkout` fetches the same pinned commit
from an existing Git repository; cached Go dependencies are still needed offline.
An existing output directory is refused, as are mismatched patch/module checksums.

Patch order:

1. Materialize the pinned keyring module's top-level Go files, module files,
   README and license, then apply `0003-keyring-errors.patch` inside that module.
2. Apply `0001-shared-client.patch` to gogcli for both clients.
3. Apply `0002-calendar-companion.patch` only to the companion.

The result reproduces source inputs and behavior. It does not promise identical
signed bytes across SDKs, architectures or signing timestamps.

## Sign and install locally

Before changing local helpers, read `~/.zeroclaw/local-signing/README.md` and
preserve the existing **ZeroClaw Local Signing** certificate and mappings:

| Component | Stable identifier |
| --- | --- |
| `gog` | `com.zeroclaw.local.gog` |
| `gog-calendar-patch` | `com.zeroclaw.local.gog-calendar-patch` |

Add `--sign` to the build command to invoke the canonical launcher's sign-only
mode for each staged candidate. Alternatively:

```sh
~/.zeroclaw/bin/zeroclaw-signed-launch gog --sign-only /absolute/staging/bin/gog
~/.zeroclaw/bin/zeroclaw-signed-launch gog-calendar-patch --sign-only /absolute/staging/bin/gog-calendar-patch
```

Keep signed rollback copies of both installed clients. Verify each candidate's
certificate-pinned designated requirement using the existing local signing
policy, then atomically replace the regular executable targets. Preserve the
Homebrew `gog` symlink and all existing launchd/MCP routes through the canonical
signing wrapper. Reload affected idle MCP processes and verify service health.
A plain `codesign --verify` alone does not prove the certificate-pinned identity.
Never generate a replacement certificate or install ad-hoc-only builds.

Use a deliberate interactive read-only request with each final signed client
for owner approval, entering the password only in the native macOS dialog:

```sh
~/.zeroclaw/bin/zeroclaw-signed-launch gog --account ACCOUNT --readonly calendar calendars --max=1
~/.zeroclaw/bin/zeroclaw-signed-launch gog-calendar-patch --account ACCOUNT --readonly calendar calendars --max=1
```

Repeat with `--no-input` after approval. Do not export tokens, broaden partition
ACLs, switch credential backends or reconnect an account merely to silence an
access-denied error. Signing preserves identity; it does not grant permissions.

## Validation and rollback

Regression coverage checks repeated access refreshes without credential writes,
durable refresh/scope updates, file-backend caching, actual native error codes,
and the existing Calendar transport safeguards. Native macOS builds intentionally
emit a deprecation warning for `SecKeychainSetUserInteractionAllowed`, which is
the legacy backend's process-scoped UI control; it does not change ACLs.

For source rollback, revert the kit commit. For a local binary rollback, restore
the prior signed clients through their existing components, reload idle MCP
processes, and verify signatures and health. Keep current credentials, journals,
configuration and runtime receipts. Restoring the old binaries also restores
the prior prompt-loop behavior. No Calendar mutation is needed to validate this
repair.
