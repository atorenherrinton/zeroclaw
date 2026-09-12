# ZeroClaw YouTube Studio

A bounded local draft renderer for original AI/tech Shorts. It has no network,
upload, account, third-party clip, shell, or caller-selected executable features.

The permanent entry is the Rust `zeroclaw-youtube-studio` binary. It invokes only
`~/.zeroclaw/extensions/youtube-studio/venv/bin/python -I` and the fixed installed
`renderer.py`, with a minimal environment and model downloads disabled. The
parent setup task owns signing, installation, dependencies, and service routing.
Do not install an unsigned or ad-hoc replacement: preserve the user's stable
ZeroClaw Local Signing certificate and canonical signing launcher.

The Rust launcher captures the installing account's home at build time and sets
that exact home in the renderer's cleared environment. The renderer derives its
fixed paths from that value. Build under the account that owns the installation;
runtime arguments cannot choose another interpreter, renderer or workspace.

## Interface

```
zeroclaw-youtube-studio status
zeroclaw-youtube-studio validate /absolute/job/directory
zeroclaw-youtube-studio render /absolute/job/directory
zeroclaw-youtube-studio mcp
```

No arguments also starts MCP. MCP uses newline-delimited JSON-RPC and exposes
only `status {}`, `validate {job_dir}`, and `render {job_dir}`. Initialize,
tools/list, tools/call, ping, and ignored notifications are supported. Render can
take several minutes; configure the MCP tool timeout for 600 seconds (the
installed ZeroClaw maximum). The enclosing agent run may use a 900-second timeout.

Job directories must already exist as canonical, absolute descendants of:

```
~/.zeroclaw/agents/youtube_creator/workspace/jobs/
```

Symlinks in any path component, symlinked/hardlinked manifests, FIFOs, duplicate
JSON keys, nonfinite JSON numbers, unknown fields, and oversized manifests are
rejected. `source_urls` are informational HTTPS references only and never fetched.

## Manifest

Each job contains `manifest.json` with exactly this structure:

```json
{
  "schema_version": 1,
  "channel_name": "Example Tech Channel",
  "title": "The specific question this video answers",
  "scenes": [
    {
      "heading": "The opening claim",
      "narration": "Spoken original commentary for this scene.",
      "on_screen": "Concise evidence or a useful takeaway."
    }
  ],
  "source_urls": ["https://example.com/primary-source"]
}
```

The illustrative one-scene fragment above is incomplete. Actual jobs require
3–10 scenes and 70–175 total narration words. Text limits: channel 50 characters,
title 120, heading 64, scene narration 750, and on-screen copy 200. Newlines and
other control characters are not allowed. Keep headings and visual copy brief.
The spoken duration must measure 30–65 seconds after synthesis; adjust the script
if validation of actual duration fails. URL references are optional (maximum 12).

## Rendering

The offline `hexgrad/Kokoro-82M` model uses American English voice `af_heart` on
CPU. Model and voice assets plus `en_core_web_sm` must already be cached by setup.
Kokoro, soundfile, Pillow, Torch, Transformers and Misaki must exist in the pinned
venv. The renderer uses macOS Arial fonts and `/opt/homebrew/bin/ffmpeg` and
`ffprobe`; eSpeak-NG's library is `/opt/homebrew/lib/libespeak-ng.dylib`.

All visuals are original 1080×1920 text cards. Captions follow generated speech
segments with approximate timing within each segment. Output is 30 fps H.264
video with AAC narration. `final.mp4` and `receipt.json` are created without
clobbering prior outputs. The receipt records hashes, duration, scene timing,
dependency versions, source references, and explicit unpublished draft status.

Re-running the same completed manifest verifies the MP4 hash and returns its
receipt. A changed manifest, modified video, incomplete output pair, or stale
lock fails closed. Use a fresh job directory for changes to completed work.
Failed renders remove their temporary stage. A crash may leave a stage and lock;
inspect the process before manually cleaning those.

## Verification

```
python3 -m unittest discover -s tests -v
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

Tests cover path boundaries, symlinks, hard links, FIFOs, strict manifests,
idempotence, output tampering, MCP allowlists, notification behavior, and a live
stdio protocol round trip. A real offline render and visual/audio review are
still required to verify installed model assets and production output quality.

## Existing installation and rollback

The Python dependency snapshot is `requirements.lock.txt`; local model weights,
the virtual environment, jobs, videos, receipts and installation metadata are
excluded from source control. Preserve those during any update.

Before changing the installed helper, read `~/.zeroclaw/local-signing/README.md`
and retain a rollback copy of the launcher and renderer. Sign a staged launcher
through `zeroclaw-signed-launch youtube-studio --sign-only /absolute/candidate`
using the existing certificate and `com.zeroclaw.local.youtube-studio` identifier.
Verify the certificate-pinned requirement before atomic installation. Preserve
the MCP route `zeroclaw-signed-launch youtube-studio mcp`, then verify status and
tool discovery. Rollback restores the signed launcher and matching renderer
without deleting completed jobs or receipts.
