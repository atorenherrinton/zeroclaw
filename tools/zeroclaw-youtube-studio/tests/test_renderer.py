import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

MODULE_PATH = Path(__file__).resolve().parents[1] / "renderer.py"
spec = importlib.util.spec_from_file_location("studio_renderer", MODULE_PATH)
studio = importlib.util.module_from_spec(spec)
spec.loader.exec_module(studio)

def sample():
    narration = "A fluent answer can sound correct even when its evidence is weak. Check the original source and the publication date before sharing a factual claim."
    return {"schema_version": 1, "channel_name": "Example Tech Channel",
            "title": "Three checks before you trust an AI answer",
            "scenes": [{"heading": f"Check {i}", "on_screen": "Source. Date. Evidence.",
                        "narration": narration} for i in range(3)],
            "source_urls": ["https://example.com/reference"]}

class ManifestTests(unittest.TestCase):
    def test_normal_manifest(self):
        self.assertGreaterEqual(studio.validate_manifest(sample()), 70)

    def test_rejects_executable_and_clip_fields(self):
        for field in ("command", "audio_url", "voice", "ffmpeg_path", "clip_path"):
            document = sample()
            document[field] = "/bin/sh"
            with self.subTest(field=field), self.assertRaises(studio.StudioError):
                studio.validate_manifest(document)

    def test_rejects_nested_unknown_keys_and_nonobjects(self):
        for scene in ([], "clip", {**sample()["scenes"][0], "image_url": "https://example.com"}):
            document = sample()
            document["scenes"][0] = scene
            with self.subTest(scene=scene), self.assertRaises(studio.StudioError):
                studio.validate_manifest(document)

    def test_rejects_duplicate_and_nonfinite_json(self):
        for text in ('{"title":"first","title":"second"}', '{"n": NaN}', '{"n": Infinity}'):
            with self.subTest(text=text), self.assertRaises(studio.StudioError):
                studio.strict_json(text)

    def test_rejects_control_characters_and_non_https_sources(self):
        document = sample()
        document["scenes"][0]["narration"] += "\x00"
        with self.assertRaises(studio.StudioError):
            studio.validate_manifest(document)
        for url in ("file:///etc/passwd", "http://example.com", "https://user:pass@example.com", "https://ex ample.com"):
            document = sample()
            document["source_urls"] = [url]
            with self.subTest(url=url), self.assertRaises(studio.StudioError):
                studio.validate_manifest(document)

    def test_rejects_unbounded_narration(self):
        for text in ("Too short.", "word " * 100):
            document = sample()
            for scene in document["scenes"]:
                scene["narration"] = text
            with self.assertRaises(studio.StudioError):
                studio.validate_manifest(document)

class JobBoundaryTests(unittest.TestCase):
    def setUp(self):
        # /tmp and /var are symlinked on macOS; use the physical system temp root.
        base = "/private/tmp" if Path("/private/tmp").is_dir() else None
        self.temp = tempfile.TemporaryDirectory(dir=base)
        self.root = Path(self.temp.name) / "jobs"
        self.root.mkdir()
        self.job = self.root / "sample"
        self.job.mkdir()
        self.manifest = self.job / "manifest.json"
        self.manifest.write_text(json.dumps(sample()))
        self.patch = mock.patch.object(studio, "JOBS_ROOT", self.root)
        self.patch.start()

    def tearDown(self):
        self.patch.stop()
        self.temp.cleanup()

    def test_valid_job_and_canonical_manifest_hash(self):
        initial = studio.load_job(str(self.job))[2]
        self.manifest.write_text(json.dumps(sample(), indent=4))
        self.assertEqual(initial, studio.load_job(str(self.job))[2])

    def test_fresh_render_pins_model_cache_before_import_and_cleans_failed_stage(self):
        original_import = __import__
        import_reached = []

        def stop_before_model(name, *args, **kwargs):
            if name == "numpy":
                import_reached.append(name)
                self.assertEqual(os.environ["HF_HOME"], str(studio.INSTALL_HOME / ".cache/huggingface"))
                self.assertEqual(os.environ["HF_HUB_OFFLINE"], "1")
                self.assertEqual(os.environ["TRANSFORMERS_OFFLINE"], "1")
                raise ImportError("synthetic unavailable model dependency")
            return original_import(name, *args, **kwargs)

        with mock.patch.dict(os.environ, {"HF_HOME": "/untrusted/cache"}), \
                mock.patch("builtins.__import__", side_effect=stop_before_model):
            with self.assertRaisesRegex(ImportError, "synthetic unavailable model dependency"):
                studio.render(str(self.job))
        self.assertEqual(import_reached, ["numpy"])
        self.assertFalse((self.job / ".render.lock").exists())
        self.assertEqual(list(self.job.glob(".render-*")), [])
        self.assertFalse((self.job / "final.mp4").exists())

    def test_rejects_relative_root_parent_and_outside_paths(self):
        for path in ("sample", str(self.root), str(self.root / ".." / "jobs" / "sample"), str(self.root.parent)):
            with self.subTest(path=path), self.assertRaises(studio.StudioError):
                studio.job_path(path)

    def test_rejects_symlink_job_even_when_target_inside_root(self):
        alias = self.root / "alias"
        alias.symlink_to(self.job, target_is_directory=True)
        with self.assertRaises(studio.StudioError):
            studio.load_job(str(alias))

    def test_rejects_symlink_parent_and_manifest(self):
        outside = self.root.parent / "outside"
        outside.mkdir()
        linked = self.root / "linked-parent"
        linked.symlink_to(outside, target_is_directory=True)
        with self.assertRaises(studio.StudioError):
            studio.job_path(str(linked))
        target = outside / "manifest.json"
        self.manifest.rename(target)
        self.manifest.symlink_to(target)
        with self.assertRaises(studio.StudioError):
            studio.load_job(str(self.job))

    def test_rejects_hardlinked_and_oversized_manifest(self):
        os.link(self.manifest, self.root.parent / "copy.json")
        with self.assertRaises(studio.StudioError):
            studio.load_job(str(self.job))
        (self.root.parent / "copy.json").unlink()
        self.manifest.write_bytes(b" " * (studio.MAX_MANIFEST_BYTES+1))
        with self.assertRaises(studio.StudioError):
            studio.load_job(str(self.job))

    def test_rejects_fifo_manifest_without_blocking(self):
        self.manifest.unlink()
        os.mkfifo(self.manifest)
        with self.assertRaises(studio.StudioError):
            studio.load_job(str(self.job))

    def test_completed_output_idempotent_but_changed_manifest_is_rejected(self):
        digest = studio.load_job(str(self.job))[2]
        video = self.job / "final.mp4"
        video.write_bytes(b"test video payload")
        receipt = {"status": "local_draft_complete", "manifest_sha256": digest,
                   "video_sha256": hashlib.sha256(video.read_bytes()).hexdigest()}
        (self.job / "receipt.json").write_text(json.dumps(receipt))
        self.assertEqual(studio.render(str(self.job)), receipt)
        document = sample()
        document["title"] = "Different title"
        self.manifest.write_text(json.dumps(document))
        with self.assertRaisesRegex(studio.StudioError, "different manifest"):
            studio.render(str(self.job))
        self.assertEqual(video.read_bytes(), b"test video payload")

    def test_dangling_output_symlink_and_incomplete_pair_fail_closed(self):
        video = self.job / "final.mp4"
        video.symlink_to(self.job / "nonexistent")
        with self.assertRaisesRegex(studio.StudioError, "incomplete existing output"):
            studio.prior_receipt(self.job, "anything")
        self.assertFalse((self.job / "nonexistent").exists())

    def test_tampered_completed_video_is_rejected(self):
        digest = studio.load_job(str(self.job))[2]
        (self.job / "final.mp4").write_bytes(b"changed")
        (self.job / "receipt.json").write_text(json.dumps({"status": "local_draft_complete",
                    "manifest_sha256": digest, "video_sha256": "bad"}))
        with self.assertRaisesRegex(studio.StudioError, "differs from its receipt"):
            studio.render(str(self.job))

class ProtocolTests(unittest.TestCase):
    def test_only_three_tools_and_no_additional_properties(self):
        self.assertEqual([t["name"] for t in studio.TOOLS], ["status", "validate", "render"])
        for tool in studio.TOOLS:
            self.assertFalse(tool["inputSchema"]["additionalProperties"])
        for name, args in (("status", {"exec": "sh"}), ("render", {"job_dir": "/tmp", "exec": "sh"}), ("upload", {})):
            with self.subTest(name=name), self.assertRaises(studio.StudioError):
                studio.call_tool(name, args)

    def test_notifications_cannot_render(self):
        with mock.patch.object(studio, "render") as render:
            self.assertIsNone(studio.rpc_response({"jsonrpc": "2.0", "method": "tools/call", "params": {"name": "render", "arguments": {"job_dir": "/x"}}}))
            render.assert_not_called()

    def test_unknown_tool_returns_mcp_error(self):
        response = studio.rpc_response({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "execute", "arguments": {}}})
        self.assertTrue(response["result"]["isError"])

    def test_stdio_jsonlines_and_notification_suppression(self):
        messages = [{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}},
                    {"jsonrpc": "2.0", "method": "notifications/initialized"},
                    {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
                    {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "status", "arguments": {}}}]
        result = subprocess.run([sys.executable, "-I", str(MODULE_PATH), "mcp"],
                                input="\n".join(map(json.dumps, messages))+"\n", capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        responses = [json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual([response["id"] for response in responses], [1, 2, 3])
        self.assertEqual(len(responses[1]["result"]["tools"]), 3)
        self.assertFalse(responses[2]["result"]["isError"])

if __name__ == "__main__":
    unittest.main()
