# Restricted Reddit browser adapter

Standalone Rust WebDriver adapter for a dedicated Reddit Chrome profile. It
exposes bounded reads of inbox, notification and existing conversation pages.
The adapter neither launches Chrome nor installs services or copies a profile.

## Deployment contract

The fixed route is ZeroClaw's native browser tool → private loopback adapter
`127.0.0.1:9516` → dedicated ChromeDriver `127.0.0.1:9515` → independently managed
Chrome debugger `127.0.0.1:18801`. Client capabilities are replaced with the fixed
Chrome attach request; callers cannot select profiles, proxies, launch arguments,
downloads, extensions, insecure TLS or vendor/CDP capabilities.

The adapter reads a private 64-character hexadecimal token from an owner-only
regular file:

```text
zeroclaw-reddit-browser --token-file /absolute/private/token-file
```

The configured WebDriver URL must include that token and a trailing slash.
Keep that URL, its configuration and logs private. Host and browser-origin
validation prevent browser-origin access; direct CDP and ChromeDriver access by
unrestricted local processes remains outside this adapter's boundary.

## Allowed operations

The exact HTTPS host allowlist is `reddit.com`, `www.reddit.com`,
`old.reddit.com`, `mod.reddit.com`, and `modmail.reddit.com`, without credentials
or nonstandard ports. Allowed routes are limited to inboxes and existing
conversations:

- Regular hosts: `/message/inbox`, `/message/messages`, `/message/unread`, and
  alphanumeric message IDs below `/message/messages/`.
- Modern Reddit hosts: `/notifications` and bounded existing
  `/room/!ROOM_ID%3Areddit.com` identifiers (literal colon also accepted).
- Moderator hosts: `/mail/all`, `/mail/inbox`, `/mail/unread`, and alphanumeric
  IDs below `/mail/perma/`.

Queries, fragments, login/logout, compose/post, settings, APIs and unknown
routes are denied. `about:blank` is allowed only for current-URL bootstrap,
never for reading content. Allowed commands are navigation to those routes,
current URL, title, page source, CSS/XPath element lookup, visible element text,
a small attribute allowlist, selected/enabled/displayed checks and bounded
timeouts. URL policy is checked before and after page-data reads.

JavaScript, snapshots, screenshots, cookies, form entry, clicks, submission,
keyboard/mouse actions, windows/frames, alerts, history, downloads, CDP and
unknown commands are denied. Snapshots can mutate page attributes and screenshot
callers can select filesystem output paths, so both endpoints remain closed.

Closing a client releases its logical lease without forwarding session/window
deletion to ChromeDriver. The adapter preserves Chrome, serializes commands,
expires idle leases after five minutes, caps requests at 32 KiB and upstream
responses at 16 MiB, and limits body reads and upstream requests. Errors omit
page contents, token-bearing URLs and driver diagnostic bodies.

Read-only means no agent-issued mutating WebDriver command. A website can still
mark an opened conversation read or perform background requests. Redirects are
detected after navigation and before data is returned; this is not a browser
network firewall. The dedicated monitor policy is in `MONITOR_POLICY.md`.

## Verification and existing installations

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

Tests use a fake driver and synthetic loopback HTTP server. They cover native
Fantoccini route shapes, capability replacement, rejected writes, URL and
token/origin/host checks, before/after page policy, lease expiry, retained Chrome
ownership and stale-session recovery. Tests do not attach to Chrome or Reddit.

Use the ChromeDriver version matching the independently managed Chrome build.
Do not point this adapter at a default or unrelated profile, attach concurrent
controllers, or bypass sign-in and permission prompts. Source consolidation does
not activate the service or migrate a profile.

Before an executable update, read `~/.zeroclaw/local-signing/README.md`, preserve
the component's existing identity and service route, and retain a signed rollback
backup. If this helper has no canonical signing mapping, add a distinct stable
component mapping using the existing certificate before installing it. Verify
the certificate-pinned identity and service health; an ad-hoc signature is
insufficient. Keep the token, profile and service configuration private and
preserve them when rolling back the executable.
