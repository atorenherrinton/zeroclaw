# Google CLI dependency patches

The canonical source patches and build recipe now live in
[`tools/gogcli-local-patches`](../../gogcli-local-patches/README.md).
That kit reconstructs both the shared `gog` client and the guarded
`gog-calendar-patch` companion from the pinned upstream commit.

It preserves the earlier Calendar insert, explicit guest-permission, ETag and
single-attempt guards, and adds the native Keychain refresh, background-prompt
and error-reporting fixes. Use the complete kit for future rebuilds; applying
only the old Calendar patch would lose those credential-access repairs.

Upstream license notices moved with the patches. No executable or credential is
installed by these source files. Follow the kit's signing and rollback steps
before replacing a local client.
