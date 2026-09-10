# Authenticated browser MCP adapter

Credential-free stdio adapter for the [authenticated Chrome helper](../zeroclaw-auth-browser/README.md).
It verifies the installed core against its approved SHA-256 fingerprint, launches
that core through `~/.zeroclaw/bin/zeroclaw-signed-launch auth-browser`, and wraps
legacy login fill-status booleans in MCP text content. Existing MCP content,
protocol errors, and non-login responses retain their original structure.

The adapter accepts no command arguments. It resolves the installation from
`HOME`, bounds protocol lines and pending login requests, and stops the child on
EOF, protocol failure, or termination. It does not access Keychain credentials.

```sh
cargo fmt --manifest-path tools/zeroclaw-auth-browser-mcp/Cargo.toml -- --check
cargo test --locked --manifest-path tools/zeroclaw-auth-browser-mcp/Cargo.toml
cargo clippy --locked --manifest-path tools/zeroclaw-auth-browser-mcp/Cargo.toml --all-targets -- -D warnings
cargo build --release --locked --manifest-path tools/zeroclaw-auth-browser-mcp/Cargo.toml
```

Follow the core helper's installation and rollback instructions. Preserve the
existing `auth-browser-mcp` signing identity when updating this adapter, and keep
the credential-owning core byte-for-byte unchanged. The separate adapter exists
because macOS Keychain's partition check can bind locally signed credential
access to a binary's code hash even when its designated requirement is stable.
