# ZeroClaw phone extension

This standalone Rust service provides two deliberately separate phone paths:

- authenticated inbound voicemail screening with keypad consent or notice-based audio recording;
- owner-triggered outbound AI calls exposed through a narrow MCP stdio server.

Outbound calls require an exact E.164 destination, a disclosed `on_behalf_of`
name, and a bounded purpose. The remote session has no owner memory, files,
browsing, contacts, or general tools. It discloses that it is an AI assistant and
that the call is transcribed, asks whether it may continue, does not record audio,
and can invoke only a fixed `end_call` function for its own call. Signed Twilio
webhooks bind each call to a private durable request, and exact duplicates are
coalesced for ten minutes so an outcome-unknown create request is not replayed.
Synchronous answering detection delays Realtime until the answered leg is
classified: humans connect immediately, detected voicemail connects after the
greeting so the authorized message can be left, and unresolved or
announcement-like results are screened out. Completed outbound transcripts are
summarized separately and privately delivered to the configured owner.
Interactive answers use a server-enforced two-phase close: the first close
request produces an audible recap and final check, then the bridge requires a
remote reply (or eight seconds of post-playback silence) before honoring a later
close request. That authorization remains active across separate speech and tool
responses, while all queued audio still has to finish playback. Detected voicemail
keeps the one-step close after all message playback marks are acknowledged.

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

## Inbound recording notice

The canonical `phone.toml` setting selects the admission flow for each new call:

```toml
recording_consent = "notice"
```

Notice mode first identifies the AI assistant, explains that the conversation
will be recorded and transcribed and privately sent to the person called, and
instructs anyone who disagrees to hang up. The entire disclosure plays before a
speech-only prompt says “Please go ahead.” There is no keypad step. Nonempty
caller speech after the notice admits the call; silence, a recognized objection,
or a recognized recording/transcription question ends it without starting audio
recording. Twilio transcribes this first utterance without saving its audio;
the text is preserved at the start of the voicemail transcript. Audio recording
begins when the realtime conversation connects, so that first utterance is not
part of the audio attachment.

Recognized later objections immediately end the call and suppress its recording
and transcript delivery. Deterministic phrase checks and the realtime assistant's
dedicated stop tool handle objections; speech recognition and interpretation are
not perfect. Recording delivery waits for completed call finalization. The notice
is a configured product behavior, not a determination that notice alone suffices
in every jurisdiction.

The default is `"explicit"`, preserving existing keypad consent unless the
operator selects notice mode. Settings are read for each admission, while calls
already in progress keep their admitted flow and nonce. Set the value back to
`"explicit"` to restore keypad admission for new calls. No schema migration is
required. Preserve the existing local signing identity and retain a signed binary
and private configuration backup when deploying or rolling back the helper.

## Private voicemail channel

Inbound voicemail summaries and consented recordings can use a dedicated bot
and owner-only private Telegram channel. Outbound call summaries continue using
the existing owner chat. The canonical optional route is in `phone.toml`:

```toml
[voicemail]
telegram_alias = "voicemail"
bot_username = "voicemail_bot"
channel_id = "-1001234567890"
```

The alias resolves its encrypted bot token from native ZeroClaw configuration.
The existing paired owner remains the authority; channel IDs do not enter the
agent peer allowlist. Before delivery the service verifies the exact bot, exact
channel ID, absence of public usernames, owner creator status, bot posting
permission, and exactly two members (owner and bot). Additional members or a
public channel stop delivery. It rechecks live routing before the send and keeps
uncertain receipts from being retried. Without this section, existing private
chat delivery is unchanged.

Changing routes does not resend old outbox entries or silently redirect claimed
work. Migrate historical messages as a separately authorized, receipt-tracked
copy from the archive; preserve the original messages and delivery records.
