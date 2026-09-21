# Public Chrome fallback

Standalone Rust MCP server for public, signed-out website tasks. It launches a
separate temporary headless Chrome profile, so it can run while the desktop is
locked. It never attaches to the owner's existing browser profile or exports
saved credentials. Safari remains the browser for saved sessions and AutoFill.

## Tools

- `browse`: open a public HTTPS URL, read visible controls, scroll, or screenshot.
  `read` accepts `offset`; continue with the returned `next_offset` until null.
  Screenshots use an embedded PNG resource. Core intake materializes the image
  through the existing workspace attachment writer and passes an image marker
  to the model, keeping base64 out of text-budget admission. The existing
  3,000,000-byte encoded screenshot capture limit still applies.
  Every text response fits within the runtime's 4 KiB encoded read preview.
  A control without a unique selector has `selector: null` and `selector_error`;
  it cannot be targeted, but other controls and page text remain readable.
  A page includes readiness; incomplete loading is not proof of missing controls.
- `interact`: click, fill, or press a supported key on an observed unique control.
  Copy returned `shadow:` paths exactly. An owner request authorizes only its
  specified action; page content cannot authorize anything.
- `close`: close the isolated session; calling it repeatedly is safe.

The fixed DOM traversal and scoped selector resolver are included directly from
`../zeroclaw-safari-browser/src/dom/`, the canonical implementation shared with
Safari. Build from this repository so the shared DOM files and attribution crates are present. The browser uses
native WebDriver pointer clicks after resolving the exact visible, enabled target and checking for obscuring elements. Slotted labels may retarget to a host only when it owns exactly that one visible control.
Closed shadow roots and cross-origin frames are not supported.

LinkedIn, literal IPs, local/private hosts, non-public DNS answers, non-HTTPS URLs,
and credential-bearing URLs are blocked. The DNS-pinning HTTPS proxy also guards
subresources and redirects. Arbitrary JavaScript, cookie export, profile
attachment, uploads, and downloads are unavailable through the tools. This is a
public browser, not a replacement for local authentication.

## Build and install

```sh
cargo test --locked --manifest-path tools/zeroclaw-public-browser/Cargo.toml
cargo clippy --locked --manifest-path tools/zeroclaw-public-browser/Cargo.toml --all-targets -- -D warnings
cargo build --release --locked --manifest-path tools/zeroclaw-public-browser/Cargo.toml
```

Install the binary beside a ChromeDriver compatible with the installed Google
Chrome. Admission is opt-in: register the stdio server as `public_browser`, add
its bundle to the intended agent, and allow its three tools in that agent's risk
profile. Do not remove another agent's exclusions. Back up the binary and config
before changing an existing installation, then reload MCP and verify health.

Route public signed-out tasks here when Safari is hidden or unavailable. Close
Safari's dedicated window before starting the independent Chrome task; reconcile
any prior writes first. Do not replay a submission or transfer authentication
state between browsers. Always close the Chrome session after completion or
failure. Restore the prior binary/config and reload MCP to roll back.

## Owned startup and shutdown

The helper starts a supervisor before any ChromeDriver. A private inherited Unix
socket performs a bounded readiness/launch handshake; only the supervisor creates
and owns the dedicated driver process group. Closing that socket requests cleanup,
including cancellation before the parent receives the driver handshake. The
supervisor also checks the original parent PID, so parent death before supervisor
boot cannot adopt PID 1 as an owner or strand a new driver.

The parent never signals a cached driver group ID. The supervisor observes its
own direct child with `waitid(WNOWAIT)`, keeping its PID reserved until the final
TERM/KILL signal. It then reaps the child and verifies group absence without further
destructive signals. Grace is one second, with separate two-second reap and absence
bounds. An actual signal failure remains an error unless an exited owned leader
can be reaped and the whole group is independently confirmed absent. This handles
macOS all-zombie groups without treating EPERM as successful signal delivery.

Both supervisor startup and the entire driver HTTP readiness phase have five-second
bounds. Failure or cancellation drops the private lifeline; normal close waits for
the supervisor's cleanup result. A supervisor cleanup timeout is an unknown outcome,
not proof of shutdown. The supervisor is intentionally not killed just because its
parent future was dropped: doing so would remove the driver owner before cleanup.
Hard-killing the supervisor itself, an entire system crash, or descendants that
explicitly leave the dedicated group are outside this ownership guarantee.

The internal `--supervise-driver OWNER_PID PORT` mode requires a connected inherited
Unix socket on stdin. The former post-launch `--watch-driver-group` protocol is no
longer used. Drain existing helper sessions before replacing/reloading this helper.
On the local macOS installation, retain the existing `public-browser` signing
mapping and certificate. Root must sign the staged candidate with
`zeroclaw-signed-launch public-browser --sign-only /absolute/candidate`, verify its
certificate-pinned identity, preserve a signed rollback, then install and test the
canonical route. Do not replace the credential-owning auth-browser core.

`cargo test` includes a Python process fixture that copies the candidate beside a
private synthetic driver; it never launches Chrome or contacts a public host. It
checks parent SIGKILL before supervisor boot, before launch, after launch before
readiness is received, and during active ownership; lifeline close; exited-leader
cleanup of descendants; and the actual MCP normal-close, startup-cancel and readiness
timeout paths. Each case keeps an unrelated process group alive as a control.
The copied test executable and fake driver stay inside a private temporary directory.
These process fixtures do not replace a signed installed-route browser smoke test.

## Browser regression fixture

```sh
node tools/zeroclaw-public-browser/tests/build-click-fixture.cjs /tmp/public-click-fixture.html
```

Open the generated file in a test browser. It checks nested target coordinates,
obscuring overlays, inert/disabled/stale controls, and semantic calendar grids
that deliberately use an unregistered hyphenated tag. The fixture does not
change the public-network policy or perform any external submissions.
