# Google notification bridge

Standalone Go service forwarding authenticated Gmail and Calendar notifications
to the existing private personal-operations event inbox. The bridge retains watch
receipts in its private state file and proxies other paths to the existing
loopback phone service, including WebSocket upgrades.

Gmail notifications require the configured callback secret, subscription and
mailbox. Calendar notifications require the configured token and a known channel
and resource. Bodies and concurrent deliveries are bounded; rejected callbacks
never reach the private inbox. Inbox failures return a retryable response.

## Build and test

From this directory:

```sh
go test -race ./...
go vet ./...
go build -o zeroclaw-google-push .
```

Tests use local fake inbox and phone servers, covering callback authentication,
subscription and mailbox binding, payload limits, retry behavior, and proxy
preservation including WebSocket traffic. They do not register Google watches.

## Configuration and operations

The existing owner-private JSON configuration is the source of truth for
`Root`, `Account`, `Topic`, `Subscription`, `PublicURL`, `Listen`, `Upstream`,
`Secret`, `CalendarToken`, `Gog`, `OpsURL`, `OpsKeyFile`, and `EnableRenewal`.
`Root` contains `state.json`; `OpsKeyFile` points to the private inbox key.
`Gog` is the trusted installed Google CLI. Keep the configuration and key private
with mode 0600, and bind the listener to loopback behind the existing HTTPS route.
No account configuration, tokens or watch state are checked into this repository.

```text
zeroclaw-google-push serve /absolute/private/config.json
zeroclaw-google-push configure-push /absolute/private/config.json
zeroclaw-google-push renew /absolute/private/config.json
```

`configure-push` updates the configured Pub/Sub subscription. `renew` registers
Gmail and Calendar watches and retires old Calendar channels. `serve` renews
watches only when `EnableRenewal` is enabled. These operations require the
existing authorized Google CLI account; running tests does not perform them.

## Existing installation and rollback

Preserve the canonical `zeroclaw-signed-launch google-push` LaunchAgent route,
the stable `com.zeroclaw.local.google-push` identifier, and the existing ZeroClaw
Local Signing certificate. Before changing an installed executable, read
`~/.zeroclaw/local-signing/README.md`, back it up, and sign the staged candidate
with `zeroclaw-signed-launch google-push --sign-only /absolute/candidate`.
Verify the certificate-pinned designated requirement before atomic replacement,
then verify the bridge and proxied phone service health. Rollback restores the
signed backup while preserving newer private configuration and watch state.
