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

## Voice engines

Two engines can bridge call audio. Both produce the same transcript and outcome,
use the same instructions and the same fixed `end_call`/`decline_recording` tools,
and sit behind the same signed Twilio ingress. The default is unchanged.

| Engine | Pipeline | Cost |
| --- | --- | --- |
| `realtime` (default) | One OpenAI Realtime session: speech in, model, speech out | Per audio minute |
| `cascade` | Speech-to-text, then a chat model, then text-to-speech, each an HTTP endpoint | Whatever each endpoint costs; $0 for local Whisper + local model + local Kokoro |

The cascade is opt-in and reversible in `phone.toml`. Settings are read per
admission, so a call already in progress keeps its engine.

```toml
[voice]
engine = "cascade"          # "realtime" restores the integrated session

[voice.cascade]
stt_url   = "http://127.0.0.1:8000/v1/audio/transcriptions"  # Whisper server
stt_model = "gpt-transcribe"                                 # default; set for your server
llm_url   = "https://api.openai.com/v1/chat/completions"
llm_model = "your-chat-model"                                # required, no default
tts_url   = "http://127.0.0.1:8880/v1/audio/speech"          # Kokoro-FastAPI
tts_model = "kokoro"                                         # default
tts_voice = "af_heart"                                       # default
```

Each stage speaks the common OpenAI-compatible shape, so any server that does
works: the recognizer takes a multipart `file` and `model` and returns
`{"text": ...}`; the chat endpoint is chat completions with function calling; the
synthesizer receives `response_format: "pcm"` and must return raw 24 kHz signed
16-bit little-endian mono audio, which is resampled to 8 kHz mu-law for Twilio.
Kokoro is synthesis-only, so turn-taking is done here: an energy endpointer
(300 ms preroll, 500 ms trailing silence, like the Realtime VAD settings) cuts
caller utterances, and speaking over the assistant clears Twilio's playback
buffer, aborts the in-flight reply, and tells the model only what was actually
heard, using the same mark-acknowledged accounting as Realtime.

Endpoint rules, enforced when the configuration loads: `https`, or plain `http`
to this machine only; no credentials, query, or fragment in the URL. The
account's OpenAI key is attached to `https://api.openai.com` and never to any
other host. Redirects and environment proxies are disabled, responses are
size-capped, and one retry is made for transient failures of these idempotent
requests. Errors carry no provider text.

Preserved from Realtime and covered by tests: AI disclosure through the shared
instructions, beep/silence voicemail behavior, barge-in, the two-phase `end_call`
close with a tool-free audible confirmation turn and eight-second silence
fallback, immediate `decline_recording` and the deterministic objection-phrase
check, byte/mark/item ceilings, the hard duration cap, transcript-drain grace on
hangup, and untouched signature verification, TwiML, and owner-task injection.

Differences to know about:

- Caller speech over a goodbye that has not finished playing now keeps the call
  open (Realtime could end it mid-sentence). The model ends it on its next turn.
- Replies are not streamed token by token; the model answers, then synthesis
  starts sentence by sentence, so first audio arrives later than with Realtime.
  Token streaming is a possible follow-up.
- Deterministic tests use fake stages and a fake Twilio; only a real call proves
  caller timing, endpointer sensitivity on your line, and spoken quality.

Check a configured cascade without placing a call (it makes one request to each
stage): `zeroclaw-phone probe ROOT` prints `{"cascadeReady":true}`.

Rollback: set `engine = "realtime"` (or delete the `[voice]` table). The
`[voice.cascade]` section may stay for the next attempt. No data migration exists.

## Inbound recording notice

The canonical `phone.toml` setting selects the admission flow for each new call:

```toml
recording_consent = "notice"
```

Notice mode first identifies the AI assistant, explains that the conversation
will be recorded and transcribed and privately sent to the person called, and
instructs anyone who disagrees to hang up. The entire disclosure plays before a
speech-only prompt asks “Do you agree?” Callers are asked to say “I agree” and
wait for the invitation to leave their message. There is no keypad step. Nonempty
caller speech after the notice admits the call; silence, a recognized objection,
or a recognized recording/transcription question ends it without starting audio
recording. Twilio transcribes this first utterance without saving its audio;
the text is preserved at the start of the voicemail transcript. Audio recording
starts before the realtime message session connects, so that first utterance is
not part of the audio attachment. The assistant then invites the full message.
If a caller started their message early, the assistant is instructed to ask them
to repeat it for the recording; the original transcription remains available.

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

Regression fixtures check agreement admission, recording before the message
session, and the early-message recovery instruction. A telephone call is still
needed to verify caller timing and spoken model behavior end to end.
