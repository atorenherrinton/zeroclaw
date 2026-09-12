# Calendar CLI dependency patch

The Google write connector's existing `gog-calendar-patch` dependency is a
separately built [gogcli](https://github.com/openclaw/gogcli) executable. This
directory preserves the local source patch needed to reproduce its calendar
mutation guards; it does not vendor or replace the upstream CLI.

Apply `gogcli-v0.38.1-calendar-guards.patch` to an isolated gogcli checkout at
`324f656a4949c3adbf8e1f18066d3965757ed9db` (v0.38.1). The patch adds:

- `api call --if-match` for exact ETag preconditions.
- `api call --single-attempt` to disable retries and redirects for uncertain
  writes, leaving reconciliation to the caller.
- Single-attempt Calendar inserts to avoid replaying committed events and
  attendee notifications after errors.
- Explicit serialization of `--guests-can-modify=false` on event creation.

From that dependency checkout, with this directory's absolute path substituted:

```sh
git apply --check /absolute/patches/gogcli-v0.38.1-calendar-guards.patch
git apply /absolute/patches/gogcli-v0.38.1-calendar-guards.patch
go test ./internal/cmd -run 'TestAPICallCalendarPatchPreconditionAndSingleAttempt|TestCalendarCreateCmd_InsertSingleAttempt' -count=1
go build -trimpath -o /absolute/staging/gog-calendar-patch ./cmd/gog
```

The named tests use synthetic local HTTP/TLS servers and credentials. They make
no Google API calls and send no invitations. Existing connector tests separately
check the CLI arguments and calendar mutation receipt handling.

No installed executable, Google account, OAuth store or signing setup is changed
by keeping this patch. Before installation, follow the existing connector's
deployment instructions and `~/.zeroclaw/local-signing/README.md`; preserve its
stable signing route and identity, keep a signed rollback copy, and verify the
certificate-pinned designated requirement. Keep Google credentials private.

The upstream MIT license is reproduced in `LICENSE.gogcli`.
