"""Bounded, offline, original text-card Shorts drafts for ZeroClaw.

Only status, validate and render are supported. Jobs and their output stay in the
fixed workspace. There are no URLs to fetch, shell commands, uploads, or accounts.
"""
from __future__ import annotations

import contextlib
import datetime as dt
import hashlib
import importlib.metadata
import io
import json
import math
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
from urllib.parse import urlsplit

INSTALL_HOME = Path.home()  # Fixed by the trusted launcher environment.
INSTALL = INSTALL_HOME / ".zeroclaw/extensions/youtube-studio"
WORKSPACE = INSTALL_HOME / ".zeroclaw/agents/youtube_creator/workspace"
JOBS_ROOT = WORKSPACE / "jobs"
FFMPEG = Path("/opt/homebrew/bin/ffmpeg")
FFPROBE = Path("/opt/homebrew/bin/ffprobe")
MODEL = "hexgrad/Kokoro-82M"
VOICE = "af_heart"
SAMPLE_RATE = 24000
MAX_MANIFEST_BYTES = 32768
MAX_RPC_BYTES = 65536

class StudioError(ValueError):
    pass

def strict_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise StudioError(f"duplicate JSON key: {key}")
        result[key] = value
    return result

def strict_json(raw):
    def invalid_constant(value):
        raise StudioError(f"non-finite JSON value: {value}")
    try:
        return json.loads(raw, object_pairs_hook=strict_object, parse_constant=invalid_constant)
    except (UnicodeError, json.JSONDecodeError) as exc:
        raise StudioError(f"invalid JSON: {exc}") from exc

def check_keys(value, required, optional=()):
    if not isinstance(value, dict):
        raise StudioError("expected a JSON object")
    if set(value) - set(required) - set(optional):
        raise StudioError(f"unexpected fields: {sorted(set(value) - set(required) - set(optional))}")
    if set(required) - set(value):
        raise StudioError(f"missing fields: {sorted(set(required) - set(value))}")

def clean_text(value, name, maximum):
    if not isinstance(value, str) or not value.strip() or len(value) > maximum:
        raise StudioError(f"{name} must be nonempty text of at most {maximum} characters")
    if any(ord(c) < 32 or ord(c) == 127 for c in value):
        raise StudioError(f"{name} must be plain text without control characters")
    return value

def validate_manifest(manifest):
    check_keys(manifest, {"schema_version", "channel_name", "title", "scenes"}, {"source_urls"})
    if type(manifest["schema_version"]) is not int or manifest["schema_version"] != 1:
        raise StudioError("schema_version must be 1")
    clean_text(manifest["channel_name"], "channel_name", 50)
    clean_text(manifest["title"], "title", 120)
    scenes = manifest["scenes"]
    if not isinstance(scenes, list) or not 3 <= len(scenes) <= 10:
        raise StudioError("scenes must contain 3 to 10 objects")
    for scene in scenes:
        check_keys(scene, {"heading", "narration", "on_screen"})
        clean_text(scene["heading"], "heading", 64)
        clean_text(scene["narration"], "narration", 750)
        clean_text(scene["on_screen"], "on_screen", 200)
    word_count = sum(len(re.findall(r"\b[\w’'-]+\b", s["narration"])) for s in scenes)
    if not 70 <= word_count <= 175:
        raise StudioError(f"total narration must be 70–175 words (found {word_count}); measured render must be 30–65 seconds")
    urls = manifest.get("source_urls", [])
    if not isinstance(urls, list) or len(urls) > 12:
        raise StudioError("source_urls must be a list of at most 12 informational HTTPS URLs")
    for url in urls:
        clean_text(url, "source URL", 2000)
        try:
            parts = urlsplit(url)
            valid = parts.scheme == "https" and parts.hostname and not parts.username and not parts.password
        except ValueError:
            valid = False
        if not valid or any(c.isspace() for c in url):
            raise StudioError("source_urls must contain informational HTTPS URLs without credentials")
    return word_count

def no_symlink_components(path):
    current = Path(path.anchor)
    for component in path.parts[1:]:
        current = current / component
        info = current.lstat()
        if stat.S_ISLNK(info.st_mode):
            raise StudioError(f"symlinks are not allowed in job paths: {current}")

def job_path(raw):
    if not isinstance(raw, str) or not raw or "\x00" in raw:
        raise StudioError("job_dir must be an absolute directory path")
    candidate = Path(raw)
    if not candidate.is_absolute() or ".." in candidate.parts:
        raise StudioError("job_dir must be absolute without '..' components")
    try:
        relative = candidate.relative_to(JOBS_ROOT)
    except ValueError as exc:
        raise StudioError(f"job_dir must be a descendant of {JOBS_ROOT}") from exc
    if not relative.parts:
        raise StudioError("choose a job directory below the jobs root")
    try:
        no_symlink_components(candidate)
    except OSError as exc:
        raise StudioError(f"job directory is unavailable: {exc.strerror}") from exc
    if not candidate.is_dir():
        raise StudioError("job_dir must exist and be a directory")
    if candidate.resolve() != candidate:
        raise StudioError("job path must already be canonical")
    return candidate

def read_regular(path, limit):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, "rb") as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                raise StudioError(f"{path.name} must be a regular file with no hard links")
            if info.st_size > limit:
                raise StudioError(f"{path.name} is larger than the allowed {limit} bytes")
            raw = stream.read(limit + 1)
            if len(raw) > limit:
                raise StudioError(f"{path.name} is too large")
            return raw
    except OSError as exc:
        raise StudioError(f"cannot read {path.name}: {exc.strerror}") from exc

def load_job(raw):
    directory = job_path(raw)
    payload = read_regular(directory / "manifest.json", MAX_MANIFEST_BYTES)
    manifest = strict_json(payload)
    word_count = validate_manifest(manifest)
    canonical = json.dumps(manifest, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()
    digest = hashlib.sha256(canonical).hexdigest()
    return directory, manifest, digest, word_count

def sha256_file(path):
    digest = hashlib.sha256()
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
            raise StudioError(f"{path.name} must be a regular file without hard links")
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()

def prior_receipt(directory, manifest_hash):
    video = directory / "final.mp4"
    receipt_path = directory / "receipt.json"
    video_exists = os.path.lexists(video)
    receipt_exists = os.path.lexists(receipt_path)
    if not video_exists and not receipt_exists:
        return None
    if not video_exists or not receipt_exists:
        raise StudioError("incomplete existing output; use a fresh job directory")
    receipt = strict_json(read_regular(receipt_path, 65536))
    if not isinstance(receipt, dict) or receipt.get("status") != "local_draft_complete":
        raise StudioError("unrecognized existing receipt; use a fresh job directory")
    if receipt.get("manifest_sha256") != manifest_hash:
        raise StudioError("completed output belongs to a different manifest; use a fresh job directory")
    if receipt.get("video_sha256") != sha256_file(video):
        raise StudioError("completed video differs from its receipt; use a fresh job directory")
    return receipt

def dependencies():
    versions = {}
    for package in ("kokoro", "soundfile", "pillow", "torch", "transformers", "misaki"):
        try:
            versions[package] = importlib.metadata.version(package)
        except importlib.metadata.PackageNotFoundError:
            versions[package] = None
    return versions

def status():
    versions = dependencies()
    return {
        "status": "local_drafts_only", "workspace": str(WORKSPACE),
        "jobs_root": str(JOBS_ROOT), "model": MODEL, "voice": VOICE,
        "offline": True, "upload_supported": False, "network_features": False,
        "python": sys.version.split()[0], "dependencies": versions,
        "ffmpeg_available": FFMPEG.is_file(), "ffprobe_available": FFPROBE.is_file(),
        "render_dependencies_present": all(versions.values()) and FFMPEG.is_file() and FFPROBE.is_file(),
        "note": "Model cache readiness is established by a successful offline render.",
    }

def validate(raw):
    directory, manifest, digest, words = load_job(raw)
    previous = prior_receipt(directory, digest)
    return {"status": "validated", "job_dir": str(directory), "title": manifest["title"],
            "scene_count": len(manifest["scenes"]), "narration_words": words,
            "manifest_sha256": digest, "already_rendered": previous is not None,
            "duration_requirement_seconds": [30, 65],
            "note": "Input valid. Duration is checked after offline speech synthesis."}

def run_fixed(executable, args, timeout=600):
    if executable not in (FFMPEG, FFPROBE):
        raise StudioError("unrecognized executable")
    proc = subprocess.run([str(executable), *args], stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                          timeout=timeout, check=False)
    if proc.returncode:
        raise StudioError(f"{executable.name} failed: {proc.stderr[-3500:]}")
    return proc.stdout

def font(size, bold=False):
    from PIL import ImageFont
    path = Path("/System/Library/Fonts/Supplemental") / ("Arial Bold.ttf" if bold else "Arial.ttf")
    if not path.is_file():
        raise StudioError(f"required system font is unavailable: {path}")
    return ImageFont.truetype(str(path), size)

def wrap_text(draw, text, face, width):
    words = text.split()
    lines = []
    current = ""
    for word in words:
        if draw.textlength(word, font=face) > width:
            raise StudioError("a word is too long for the visual layout; shorten it")
        candidate = f"{current} {word}".strip()
        if current and draw.textlength(candidate, font=face) > width:
            lines.append(current)
            current = word
        else:
            current = candidate
    if current:
        lines.append(current)
    return lines

def fit_text(draw, text, width, max_lines, initial, minimum, bold=False):
    for size in range(initial, minimum - 1, -2):
        face = font(size, bold)
        try:
            lines = wrap_text(draw, text, face, width)
        except StudioError:
            continue
        if len(lines) <= max_lines:
            return face, lines, size
    raise StudioError("text does not fit the visual layout; shorten heading/on_screen text")

def draw_card(manifest, scene_index, caption):
    from PIL import Image, ImageDraw
    image = Image.new("RGB", (1080, 1920), "#0b1020")
    draw = ImageDraw.Draw(image)
    # Original geometric background. Main content remains above Shorts UI overlays.
    for y in range(1920):
        t = y / 1920
        draw.line((0, y, 1080, y), fill=(int(11 + 4*t), int(16 + 8*t), int(32 + 14*t)))
    for radius in (210, 320, 435):
        draw.ellipse((850-radius, 285-radius, 850+radius, 285+radius), outline="#20314b", width=2)
    draw.rounded_rectangle((64, 108, 112, 156), radius=13, fill="#74efce")
    draw.line((76, 141, 88, 121, 100, 141), fill="#0b1020", width=4)
    brand_face, brand_lines, _ = fit_text(draw, manifest["channel_name"].upper(), 860, 1, 29, 17, True)
    draw.text((132, 112), brand_lines[0], font=brand_face, fill="#dce6f5")
    scene = manifest["scenes"][scene_index]
    count = len(manifest["scenes"])
    label = f"THE TAKE  /  {scene_index + 1:02d}"
    draw.text((76, 355), label, font=font(30, True), fill="#74efce")
    face, lines, size = fit_text(draw, scene["heading"], 855, 4, 98, 62, True)
    y = 425
    for line in lines:
        draw.text((72, y), line, font=face, fill="#f8fbff")
        y += size + 16
    panel_top = max(910, y + 55)
    face, lines, size = fit_text(draw, scene["on_screen"], 780, 5, 45, 34)
    panel_height = max(235, len(lines)*(size+16)+82)
    if panel_top + panel_height > 1320:
        raise StudioError("scene copy is too dense for the safe visual area")
    draw.rounded_rectangle((72, panel_top, 960, panel_top+panel_height), radius=28, fill="#172339", outline="#2b3c55", width=2)
    draw.rounded_rectangle((72, panel_top, 80, panel_top+panel_height), radius=4, fill="#74efce")
    y = panel_top+36
    for line in lines:
        draw.text((114, y), line, font=face, fill="#dce6f5")
        y += size+16
    # Burned-in speech captions: comfortably above platform controls and bottom title.
    face, lines, size = fit_text(draw, caption, 810, 3, 49, 37, True)
    caption_top = 1410
    box_height = max(100, len(lines)*(size+13)+42)
    draw.rounded_rectangle((73, caption_top, 959, caption_top+box_height), radius=24, fill="#eff8f5")
    y = caption_top+20
    for line in lines:
        w = draw.textlength(line, font=face)
        draw.text((516-w/2, y), line, font=face, fill="#102b29")
        y += size+13
    for index in range(count):
        left = 76 + index * (860/count)
        draw.rounded_rectangle((int(left), 1758, int(left+860/count-12), 1764), radius=3,
                               fill="#74efce" if index <= scene_index else "#2c3a50")
    draw.text((76, 1796), "ORIGINAL ANALYSIS  •  AI NARRATION", font=font(23), fill="#8699b5")
    return image

def caption_parts(text):
    words = text.split()
    return [" ".join(words[i:i+6]) for i in range(0, len(words), 6)]

def render(raw):
    directory, manifest, digest, words = load_job(raw)
    previous = prior_receipt(directory, digest)
    if previous:
        return previous
    try:
        lock_fd = os.open(directory / ".render.lock", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    except FileExistsError as exc:
        raise StudioError("this job is already rendering or has a stale lock; inspect it before retrying") from exc
    stage = None
    try:
        os.write(lock_fd, str(os.getpid()).encode())
        os.close(lock_fd)
        # A second invocation may have completed between load_job and acquiring the lock.
        previous = prior_receipt(directory, digest)
        if previous:
            return previous
        stage = Path(tempfile.mkdtemp(prefix=".render-", dir=directory))
        os.environ.update({"HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1",
                           "HF_HUB_DISABLE_TELEMETRY": "1", "TOKENIZERS_PARALLELISM": "false",
                           "HF_HOME": str(HOME / ".cache/huggingface"),
                           "PHONEMIZER_ESPEAK_LIBRARY": "/opt/homebrew/lib/libespeak-ng.dylib"})
        # All package logs must stay off the MCP stdout protocol channel.
        with contextlib.redirect_stdout(sys.stderr):
            import numpy as np
            import soundfile as sf
            import torch
            from kokoro import KPipeline
            torch.set_num_threads(4)
            pipeline = KPipeline(lang_code="a", repo_id=MODEL, device="cpu")
            audio_parts = []
            cards = []
            scene_timings = []
            elapsed = 0.0
            for index, scene in enumerate(manifest["scenes"]):
                print(f"Synthesizing scene {index + 1}/{len(manifest['scenes'])}", file=sys.stderr, flush=True)
                start = elapsed
                audio_parts.append(np.zeros(int(.18*SAMPLE_RATE), dtype=np.float32))
                cards.append((index, caption_parts(scene["narration"])[0], .18))
                elapsed += .18
                generated = False
                for result in pipeline(scene["narration"], voice=VOICE, speed=1.03, split_pattern=r"\n+"):
                    spoken, _, audio = result
                    if audio is None:
                        raise StudioError("local speech model returned no audio")
                    if hasattr(audio, "detach"):
                        audio = audio.detach().cpu().numpy()
                    samples = np.asarray(audio, dtype=np.float32).reshape(-1)
                    if not len(samples) or not np.isfinite(samples).all():
                        raise StudioError("local speech model returned invalid audio")
                    duration = len(samples)/SAMPLE_RATE
                    audio_parts.append(samples)
                    parts = caption_parts(spoken)
                    if not parts:
                        raise StudioError("local speech model returned no caption text")
                    weights = [max(1, len(p)) for p in parts]
                    for part, weight in zip(parts, weights):
                        cards.append((index, part, duration*weight/sum(weights)))
                    elapsed += duration
                    generated = True
                if not generated:
                    raise StudioError("local speech model produced no segments")
                audio_parts.append(np.zeros(int(.32*SAMPLE_RATE), dtype=np.float32))
                cards.append((index, cards[-1][1], .32))
                elapsed += .32
                scene_timings.append({"scene": index+1, "start_seconds": round(start, 3), "end_seconds": round(elapsed, 3)})
            if not 30 <= elapsed <= 65:
                raise StudioError(f"generated duration is {elapsed:.1f}s; adjust narration for 30–65s and retry")
            audio = np.concatenate(audio_parts)
            peak = float(np.max(np.abs(audio)))
            if peak < .001:
                raise StudioError("speech output is effectively silent")
            audio *= .88/peak
            sf.write(str(stage / "narration.wav"), audio, SAMPLE_RATE, subtype="PCM_16")
        frame_entries = []
        for frame_index, (scene_index, caption, duration) in enumerate(cards):
            frame_name = f"card-{frame_index:04d}.png"
            draw_card(manifest, scene_index, caption).save(stage / frame_name)
            frame_entries.extend([f"file '{frame_name}'", f"duration {duration:.9f}"])
        frame_entries.append(f"file 'card-{len(cards)-1:04d}.png'")
        (stage / "frames.txt").write_text("\n".join(frame_entries)+"\n")
        output = stage / "final.mp4"
        run_fixed(FFMPEG, ["-hide_banner", "-loglevel", "error", "-nostdin", "-n",
                          "-f", "concat", "-safe", "1", "-i", str(stage/"frames.txt"),
                          "-i", str(stage/"narration.wav"), "-vf", "fps=30,format=yuv420p",
                          "-c:v", "libx264", "-preset", "medium", "-crf", "21", "-threads", "4",
                          "-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "1",
                          "-t", f"{elapsed:.6f}", "-movflags", "+faststart", str(output)])
        probe = strict_json(run_fixed(FFPROBE, ["-v", "error", "-show_streams", "-show_format", "-of", "json", str(output)], 30))
        duration = float(probe["format"]["duration"])
        videos = [s for s in probe["streams"] if s.get("codec_type") == "video"]
        audios = [s for s in probe["streams"] if s.get("codec_type") == "audio"]
        if not (30 <= duration <= 65.1 and len(videos) == 1 and len(audios) == 1
                and videos[0].get("width") == 1080 and videos[0].get("height") == 1920
                and videos[0].get("codec_name") == "h264" and audios[0].get("codec_name") == "aac"):
            raise StudioError("rendered video failed duration, dimensions, or codec verification")
        # Refuse concurrent edits and directory replacement before finalizing.
        if load_job(raw)[2] != digest:
            raise StudioError("manifest changed during rendering; use a fresh job directory")
        receipt = {
            "schema_version": 1, "status": "local_draft_complete", "published": False,
            "job_dir": str(directory), "video_path": str(directory/"final.mp4"),
            "title": manifest["title"], "channel_name": manifest["channel_name"],
            "created_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
            "manifest_sha256": digest, "video_sha256": sha256_file(output),
            "renderer_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            "duration_seconds": round(duration, 3), "width": 1080, "height": 1920,
            "video_codec": "h264", "audio_codec": "aac", "narration_words": words,
            "scene_timings": scene_timings, "model": MODEL, "voice": VOICE,
            "speech_speed": 1.03, "offline": True, "dependencies": dependencies(),
            "ffmpeg": run_fixed(FFMPEG, ["-version"], 30).splitlines()[0],
            "source_urls": manifest.get("source_urls", []),
            "review_notes": ["Local draft only; no upload or publication occurred.",
                             "Review spoken claims, attribution, pronunciation, and captions before publishing.",
                             "Original text-card visuals with AI narration; no third-party video clips.",
                             "Caption timing within speech segments is approximate."],
        }
        (stage / "receipt.json").write_text(json.dumps(receipt, indent=2, ensure_ascii=False)+"\n")
        # Atomic, no-clobber publication of each regular file; a partial pair fails closed.
        os.link(output, directory / "final.mp4", follow_symlinks=False)
        output.unlink()
        os.link(stage / "receipt.json", directory / "receipt.json", follow_symlinks=False)
        (stage / "receipt.json").unlink()
        return receipt
    finally:
        if stage is not None:
            shutil.rmtree(stage)
        (directory / ".render.lock").unlink(missing_ok=True)

TOOLS = [
    {"name": "status", "description": "Read the local offline video-draft renderer status. No network or uploads.",
     "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False}},
    {"name": "validate", "description": "Validate a manifest.json in an existing absolute job directory below the fixed jobs root.",
     "inputSchema": {"type": "object", "properties": {"job_dir": {"type": "string"}}, "required": ["job_dir"], "additionalProperties": False}},
    {"name": "render", "description": "Generate one original narrated vertical video as a local draft, entirely offline. May take minutes. Never uploads. Requires a valid job directory below the fixed jobs root.",
     "inputSchema": {"type": "object", "properties": {"job_dir": {"type": "string"}}, "required": ["job_dir"], "additionalProperties": False}},
]

def call_tool(name, arguments):
    if name == "status":
        check_keys(arguments, set())
        return status()
    if name in ("validate", "render"):
        check_keys(arguments, {"job_dir"})
        if not isinstance(arguments["job_dir"], str):
            raise StudioError("job_dir must be a string")
        return (validate if name == "validate" else render)(arguments["job_dir"])
    raise StudioError("unknown tool; only status, validate and render are supported")

def rpc_response(request):
    if not isinstance(request, dict) or request.get("jsonrpc") != "2.0" or not isinstance(request.get("method"), str):
        return {"jsonrpc": "2.0", "id": None, "error": {"code": -32600, "message": "Invalid Request"}}
    if "id" not in request:
        return None  # Notifications never trigger any work.
    request_id = request["id"]
    if type(request_id) not in (str, int) and request_id is not None:
        return {"jsonrpc": "2.0", "id": None, "error": {"code": -32600, "message": "Invalid request ID"}}
    response = {"jsonrpc": "2.0", "id": request_id}
    method = request["method"]
    params = request.get("params", {})
    if method == "initialize":
        supported = ("2024-11-05", "2025-03-26", "2025-06-18")
        requested = params.get("protocolVersion") if isinstance(params, dict) else None
        response["result"] = {"protocolVersion": requested if requested in supported else "2024-11-05",
                              "capabilities": {"tools": {"listChanged": False}},
                              "serverInfo": {"name": "zeroclaw-youtube-studio", "version": "0.1.0"}}
    elif method == "ping":
        response["result"] = {}
    elif method == "tools/list":
        response["result"] = {"tools": TOOLS}
    elif method == "tools/call":
        try:
            check_keys(params, {"name"}, {"arguments", "_meta"})
            if not isinstance(params["name"], str):
                raise StudioError("tool name must be text")
            with contextlib.redirect_stdout(sys.stderr):
                result = call_tool(params["name"], params.get("arguments", {}))
            response["result"] = {"content": [{"type": "text", "text": json.dumps(result, ensure_ascii=False)}], "isError": False}
        except Exception as exc:
            response["result"] = {"content": [{"type": "text", "text": str(exc)}], "isError": True}
    else:
        response["error"] = {"code": -32601, "message": "Method not found"}
    return response

def mcp():
    while True:
        line = sys.stdin.buffer.readline(MAX_RPC_BYTES+1)
        if not line:
            return 0
        try:
            if len(line) > MAX_RPC_BYTES:
                # Terminate on oversized input rather than treating its remainder as requests.
                print(json.dumps({"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": "Request exceeds 64 KiB"}}), flush=True)
                return 2
            response = rpc_response(strict_json(line))
        except Exception as exc:
            response = {"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": str(exc)}}
        if response is not None:
            print(json.dumps(response, ensure_ascii=False), flush=True)

def main():
    args = sys.argv[1:]
    if not args or args == ["mcp"]:
        return mcp()
    try:
        if args == ["status"]:
            result = status()
        elif len(args) == 2 and args[0] in ("validate", "render"):
            result = call_tool(args[0], {"job_dir": args[1]})
        else:
            raise StudioError("usage: renderer.py [mcp | status | validate JOB_DIR | render JOB_DIR]")
        print(json.dumps(result, indent=2, ensure_ascii=False))
        return 0
    except Exception as exc:
        print(f"YouTube studio: {exc}", file=sys.stderr)
        return 1

if __name__ == "__main__":
    raise SystemExit(main())
