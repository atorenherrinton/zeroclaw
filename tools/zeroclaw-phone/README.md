# ZeroClaw phone extension

This standalone Rust service provides two deliberately separate phone paths:

- authenticated inbound voicemail screening with optional keypad-consented audio recording;
- owner-triggered outbound AI calls exposed through a narrow MCP stdio server.

Outbound calls require an exact E.164 destination, a disclosed `on_behalf_of`
name, and a bounded purpose. The remote session has no owner memory, files,
browsing, contacts, or general tools. It discloses that it is an AI assistant and
that the call is transcribed, asks whether it may continue, does not record audio,
and can invoke only a fixed `end_call` function for its own call. Signed Twilio
webhooks bind each call to a private durable request, and exact duplicates are
coalesced for ten minutes so an outcome-unknown create request is not replayed.

The MCP server advertises only `place_call` and `call_status`. Its tool contract
forbids calls derived from third-party content, emergencies, unsolicited
marketing, campaigns, harassment, and unrequested retries.

## Build and test

```sh
cargo test --manifest-path tools/zeroclaw-phone/Cargo.toml --all-targets
cargo clippy --manifest-path tools/zeroclaw-phone/Cargo.toml --all-targets -- -D warnings
```

The service reads an owner-private `phone.toml`, the existing ZeroClaw encrypted
configuration, and `screening.md` from its extension root. Credentials and live
configuration are intentionally not part of this repository.
