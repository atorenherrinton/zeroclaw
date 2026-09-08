# Public Chrome fallback

Standalone Rust MCP server for public, signed-out website tasks. It launches a
separate temporary headless Chrome profile, so it can run while the desktop is
locked. It never attaches to the owner's existing browser profile or exports
saved credentials. Safari remains the browser for saved sessions and AutoFill.

## Tools

- `browse`: open a public HTTPS URL, read visible controls, scroll, or screenshot.
  `read` accepts `offset`; continue with the returned `next_offset` until null.
  Every text response fits within the runtime's 4 KiB encoded read preview.
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

## Browser regression fixture

```sh
node tools/zeroclaw-public-browser/tests/build-click-fixture.cjs /tmp/public-click-fixture.html
```

Open the generated file in a test browser. It checks nested target coordinates,
obscuring overlays, inert/disabled/stale controls, and semantic calendar grids
that deliberately use an unregistered hyphenated tag. The fixture does not
change the public-network policy or perform any external submissions.
