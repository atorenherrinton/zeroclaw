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

The owner MCP server advertises `place_call`, `call_status`, and
`appointment_status`. Its outbound-call contract forbids calls derived from
third-party content, emergencies, unsolicited marketing, campaigns, harassment,
and unrequested retries. `appointment_status` only inspects existing inbound
rescheduling receipts and reconciles uncertain calendar results.

## Build and test

```sh
MACOSX_DEPLOYMENT_TARGET=15.0 cargo test --manifest-path tools/zeroclaw-phone/Cargo.toml --locked --all-targets
MACOSX_DEPLOYMENT_TARGET=15.0 cargo clippy --manifest-path tools/zeroclaw-phone/Cargo.toml --locked --all-targets -- -D warnings
MACOSX_DEPLOYMENT_TARGET=15.0 cargo build --manifest-path tools/zeroclaw-phone/Cargo.toml --locked --release --bin zeroclaw-phone
```

The Maps bridge requires macOS 15 or later. Set this deployment target for the
whole build so Rust and native dependencies agree; do not change global system
settings. The build uses the selected Xcode command-line tools and their matching
Clang Darwin runtime for Objective-C availability checks.

The service reads an owner-private `phone.toml`, the existing ZeroClaw encrypted
configuration, and `screening.md` from its extension root. Credentials and live
configuration are intentionally not part of this repository.

`route-check ROOT` recognizes either a direct tunnel to the configured phone port
or the existing Google push bridge described by the private sibling
`google-push/config.json`. Bridge mode requires canonical `Root`, `PublicURL`,
`Listen`, and `Upstream` keys, matching public URL, a literal loopback listener,
and the exact phone upstream. Duplicate topology keys, case-folded aliases,
non-ASCII key names, unsafe files and arbitrary proxy targets fail the check.
Direct mode does not require a bridge config. Both modes still check public
`/voice/health` and reject an unsigned webhook with HTTP 403. The additive
`routeKind` result is `direct` or `google_push_bridge`; this diagnoses configured
topology and endpoint behavior, not the signature or identity of a listening
process. It does not change routes or webhook authentication.

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

- Inbound calls that offer verified appointment holds always use Realtime,
  whatever `engine` says, because only that session has the appointment tools.
  Other calls use the configured engine.
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

## Tentative appointment rescheduling

The owner may opt inbound voicemail calls into a narrow local scheduling tool:

```toml
tentative_rescheduling = true
```

The default is false. The canonical private `phone.toml` is read at admission
and again during the proposal. Outbound sessions do not gain this capability.
The remote voice model receives no calendar listing, event details, credentials,
contacts, general browser, or general calendar mutation tool.

For an existing business appointment, the assistant asks only for the missing
proposed time, clarifies timezone if unclear, and reads the time back once for
confirmation. It does not ask for caller/business name, branch address, callback
number, caller-ID confirmation, or the original appointment time. Local code
queries native Apple MapKit with this signed call's incoming caller ID alone,
requires exactly one listing with that exact normalized public phone number,
and derives the business name and branch address from the listing. Incoming
caller ID is the default callback; a volunteered callback never replaces it for
verification. The Apple Maps place link is retained. A public listing match does
not authenticate the human speaker or guarantee that caller ID was not spoofed.
Missing or ambiguous evidence produces a short message, without further identity
questions. Historical receipts remain readable; live model arguments cannot
supply business identity fields.

The calendar adapter identifies one matching timed appointment in the primary
calendar using the verified listing's name and branch. An original time already
volunteered by the caller narrows the match; otherwise the complete bounded
appointment window must contain exactly one supported match. It rechecks the
current provider record, and checks all selected visible
calendars plus the primary calendar for conflicts. It uses the existing Google
read account and canonical signed Google writer, requiring the accounts to
match. The writer must support `calendar_mutate` with `status: "tentative"`.
A successful proposal creates one real tentative hold, sends no invitations,
and preserves the original event. The assistant reports a tentative arrangement
pending owner review and says the owner will call back to reschedule if that
new date does not work. The private owner summary includes the factual receipt
and the conditional callback task; this workflow does not place an automatic
outbound call.

The current scope is one proposal per call, a future proposed time within 90
days, a current original appointment no more than one day in the past or 90
days ahead, and an original duration of at most eight hours. Missing, ambiguous,
recurring, unavailable or mismatched evidence falls back to taking a message.
A read failure, partial page, free/busy error or uncertain write never becomes
an availability or booking claim. A per-call private receipt is written before
the provider operation. An interrupted or uncertain write is retained for
reconciliation and never automatically retried under a new key.

Tentative scheduling reads the native `[security.estop]` policy and canonical
stop file at admission, every 100 ms while work is pending, and immediately
before the writer starts. When enabled, kill-all, network-kill, any domain block,
or a freeze of `tentatively_reschedule_appointment` or
`google_write__calendar_mutate` prevents this workflow. Unsafe or invalid policy
and stop files fail closed; an explicitly disabled policy retains the existing
behavior. Stop handling drops owned pending work and retains uncertain write
receipts. It cannot retract a request already accepted by Google. This guard
covers tentative scheduling; ordinary voicemail and owner receipt inspection
keep their existing behavior.

The owner-only `appointment_status` MCP tool accepts `{}` for the latest 20
receipts, `{"call_sid":"CA…"}` for one exact call, or
`{"call_sid":"CA…","reconcile":true}` for read-only reconciliation of an
ended call with an unresolved write. Reconciliation checks the existing provider
operation and never creates a new hold or callback. Receipts remain visible if
the caller later declines recording; this view excludes transcripts, caller
numbers, private event text, account details and raw provider receipts. The
remote voice session cannot invoke this tool.

MapKit runs in the existing phone binary's `--maps-lookup-phone` subprocess mode
on its main thread, with an owned stdin lifeline, parent check, native deadline
and bounded output. It makes a public phone-number search without requesting
device location. The legacy `--maps-lookup` name/address diagnostic remains available. Calendar command arguments are constructed locally without a shell;
stdout, runtime and process groups are bounded. Accepted remote calendar writes
cannot be undone by dropping the voice connection, so the receipt distinguishes
verified holds from uncertain outcomes.
