"""Real helper startup, using only a private fake driver and local sockets."""
import contextlib
import json
import os
from pathlib import Path
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest

DRIVER = r'''#!/usr/bin/env python3
import http.server,json,os,signal,subprocess,sys,time
from pathlib import Path
root=Path(os.environ["ZC_PUBLIC_FIXTURE_ROOT"])
mode=os.environ.get("ZC_PUBLIC_FIXTURE_MODE","resistant")
signal.signal(signal.SIGTERM,signal.SIG_IGN)
signal.alarm(30)
child=subprocess.Popen(["/bin/sh","-c","trap '' TERM; exec /bin/sleep 30"],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
(root/"driver.pending").write_text(json.dumps({"leader":os.getpid(),"descendant":child.pid,"supervisor":os.getppid()}))
os.replace(root/"driver.pending",root/"driver.json")
if mode=="leader_exit": sys.exit(0)
if mode in ("http","stall_status"):
    port=int(next(a.split("=",1)[1] for a in sys.argv if a.startswith("--port=")))
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self,*args): pass
        def do_GET(self):
            if mode=="stall_status" and self.path=="/status":
                (root/"status-entered").write_text("ready")
                time.sleep(25)
            self.reply({"ready":True} if self.path=="/status" else "https://example.com/")
        def do_DELETE(self): self.reply(None)
        def do_POST(self):
            body=json.loads(self.rfile.read(int(self.headers.get("Content-Length",0))) or b"{}")
            if self.path=="/session": result={"sessionId":"fixture"}
            elif self.path.endswith("/execute/sync"):
                if "return {url:location.href" in body.get("script",""):
                    result={"url":"https://example.com/","title":"fixture","text":"synthetic","controls":[],"totalControls":0}
                else: result=json.dumps({"ready":True,"reasons":[],"fingerprint":"synthetic"})
            else: result=None
            self.reply(result)
        def reply(self,value):
            data=json.dumps({"value":value}).encode()
            self.send_response(200); self.send_header("Content-Type","application/json"); self.send_header("Content-Length",str(len(data))); self.end_headers(); self.wfile.write(data)
    http.server.HTTPServer(("127.0.0.1",port),Handler).serve_forever()
else:
    while True: time.sleep(1)
'''


def recv_exact(sock, count):
    data = b""
    while len(data) < count:
        piece = sock.recv(count - len(data))
        if not piece:
            raise RuntimeError("supervisor socket closed early")
        data += piece
    return data


def owner_main(helper, stopped):
    parent, child = socket.socketpair()
    expected_parent = os.getpid()
    pid = os.fork()
    if pid == 0:
        parent.close()
        os.dup2(child.fileno(), 0)
        child.close()
        null = os.open(os.devnull, os.O_RDWR)
        os.dup2(null, 1)
        os.dup2(null, 2)
        os.close(null)
        if stopped:
            os.kill(os.getpid(), signal.SIGSTOP)
        os.execv(helper, [helper, "--supervise-driver", str(expected_parent), "43123"])
    child.close()
    if stopped:
        _, status = os.waitpid(pid, os.WUNTRACED)
        assert os.WIFSTOPPED(status)
    parent.settimeout(5)
    print(json.dumps({"supervisor": pid}), flush=True)
    try:
        for command in sys.stdin:
            command = command.strip()
            if command == "hello":
                assert recv_exact(parent, 1) == b"W"
            elif command == "launch":
                parent.sendall(b"L")
            elif command == "handshake":
                group = struct.unpack("!i", recv_exact(parent, 4))[0]
                assert group > 1
            elif command == "close":
                parent.close()
                _, status = os.waitpid(pid, 0)
                assert os.waitstatus_to_exitcode(status) == 0
                print("ok", flush=True)
                return
            else:
                raise RuntimeError("invalid fixture command")
            print("ok", flush=True)
    finally:
        parent.close()


def present(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def group_present(group):
    assert group > 1 and group != os.getpgrp()
    try:
        os.killpg(group, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def wait_until(check, message, timeout=7):
    end = time.monotonic() + timeout
    while not check():
        if time.monotonic() >= end:
            raise AssertionError(message)
        time.sleep(0.02)


def read_line(process):
    if not select.select([process.stdout], [], [], 8)[0]:
        raise AssertionError("fixture response timed out")
    line = process.stdout.readline()
    if not line:
        raise AssertionError("fixture exited without response")
    return line


class Owner:
    def __init__(self, helper, env, stopped=False):
        self.process = subprocess.Popen([sys.executable, __file__, "--owner", str(helper), str(int(stopped))], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
        self.supervisor = json.loads(read_line(self.process))["supervisor"]
        self.stopped = stopped

    def command(self, name):
        self.process.stdin.write(name + "\n")
        self.process.stdin.flush()
        assert read_line(self.process).strip() == "ok"

    def kill(self):
        self.process.kill()
        self.process.wait(timeout=3)

    def finish(self):
        if self.stopped:
            # The PID is reserved by this stopped fixture process.
            with contextlib.suppress(ProcessLookupError):
                os.kill(self.supervisor, signal.SIGCONT)
            self.stopped = False
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=7)
            except subprocess.TimeoutExpired:
                self.kill()
        for stream in [self.process.stdout, self.process.stderr]:
            stream.close()
        if self.process.stdin and not self.process.stdin.closed:
            self.process.stdin.close()


class LifecycleTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="zc-public-startup-")
        self.root = Path(self.directory.name)
        self.helper = self.root / "zeroclaw-public-browser"
        shutil.copy2(HELPER, self.helper)
        self.helper.chmod(0o700)
        driver = self.root / "chromedriver"
        driver.write_text(DRIVER)
        driver.chmod(0o700)
        self.env = dict(os.environ, ZC_PUBLIC_FIXTURE_ROOT=str(self.root))
        self.owners = []
        self.rpc = []
        self.unrelated = subprocess.Popen(["/bin/sleep", "30"], start_new_session=True)

    def tearDown(self):
        for process in self.rpc:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=7)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
            for stream in [process.stdin, process.stdout, process.stderr]:
                if stream and not stream.closed:
                    stream.close()
        for owner in self.owners:
            owner.finish()
        self.assertIsNone(self.unrelated.poll(), "cleanup touched an unrelated owned test group")
        self.unrelated.kill()
        self.unrelated.wait(timeout=3)
        self.directory.cleanup()

    def owner(self, stopped=False):
        owner = Owner(self.helper, self.env, stopped)
        self.owners.append(owner)
        return owner

    def launched(self, owner, handshake=True):
        owner.command("hello")
        owner.command("launch")
        if handshake:
            owner.command("handshake")
        wait_until(lambda: (self.root / "driver.json").exists(), "synthetic driver did not start")
        return json.loads((self.root / "driver.json").read_text())

    def gone(self, ids, supervisor):
        wait_until(lambda: not group_present(ids["leader"]), "owned driver group survived")
        wait_until(lambda: not present(ids["descendant"]), "owned descendant survived")
        wait_until(lambda: not present(supervisor), "supervisor survived owner termination")

    def test_owner_hard_kill_before_supervisor_boot(self):
        owner = self.owner(stopped=True)
        owner.kill()
        os.kill(owner.supervisor, signal.SIGCONT)
        owner.stopped = False
        wait_until(lambda: not present(owner.supervisor), "pre-boot supervisor did not exit")
        self.assertFalse((self.root / "driver.json").exists())

    def test_owner_hard_kill_before_launch(self):
        owner = self.owner()
        owner.command("hello")
        owner.kill()
        wait_until(lambda: not present(owner.supervisor), "pre-launch supervisor did not exit")
        self.assertFalse((self.root / "driver.json").exists())

    def test_owner_hard_kill_after_launch_before_readiness_received(self):
        owner = self.owner()
        ids = self.launched(owner, handshake=False)
        owner.kill()
        self.gone(ids, owner.supervisor)

    def test_owner_hard_kill_during_active_ownership(self):
        owner = self.owner()
        ids = self.launched(owner)
        owner.kill()
        self.gone(ids, owner.supervisor)

    def test_lifeline_drop_cleans_driver_and_descendant(self):
        owner = self.owner()
        ids = self.launched(owner)
        owner.command("close")
        owner.process.wait(timeout=3)
        self.gone(ids, owner.supervisor)

    def test_exited_leader_still_cleans_descendants(self):
        self.env["ZC_PUBLIC_FIXTURE_MODE"] = "leader_exit"
        owner = self.owner()
        ids = self.launched(owner)
        owner.command("close")
        owner.process.wait(timeout=3)
        self.gone(ids, owner.supervisor)

    def rpc_start(self, mode):
        env = dict(self.env, ZC_PUBLIC_FIXTURE_MODE=mode)
        process = subprocess.Popen([str(self.helper)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
        self.rpc.append(process)
        return process

    def rpc_send(self, process, ident, name, args):
        process.stdin.write(json.dumps({"jsonrpc":"2.0","id":ident,"method":"tools/call","params":{"name":name,"arguments":args}}) + "\n")
        process.stdin.flush()

    def test_actual_mcp_normal_close_preserves_group_teardown(self):
        process = self.rpc_start("http")
        self.rpc_send(process, 1, "browse", {"action":"open","url":"https://example.com/"})
        result = json.loads(read_line(process))
        self.assertFalse(result["result"].get("isError", False))
        ids = json.loads((self.root / "driver.json").read_text())
        self.rpc_send(process, 2, "close", {})
        result = json.loads(read_line(process))
        self.assertFalse(result["result"].get("isError", False))
        self.gone(ids, ids["supervisor"])
        self.rpc_send(process, 3, "close", {})
        self.assertFalse(json.loads(read_line(process))["result"].get("isError", False))
        process.stdin.close()
        self.assertEqual(process.wait(timeout=3), 0)

    def test_actual_mcp_startup_cancellation_closes_lifeline(self):
        process = self.rpc_start("stall_status")
        self.rpc_send(process, 1, "browse", {"action":"open","url":"https://example.com/"})
        wait_until(lambda: (self.root / "status-entered").exists(), "startup never reached fake status")
        ids = json.loads((self.root / "driver.json").read_text())
        process.terminate()
        self.assertEqual(process.wait(timeout=5), 0)
        self.gone(ids, ids["supervisor"])

    def test_actual_mcp_readiness_timeout_cleans_owned_group(self):
        process = self.rpc_start("stall_status")
        started = time.monotonic()
        self.rpc_send(process, 1, "browse", {"action":"open","url":"https://example.com/"})
        result = json.loads(read_line(process))
        self.assertTrue(result["result"].get("isError", False))
        self.assertLess(time.monotonic() - started, 8)
        ids = json.loads((self.root / "driver.json").read_text())
        self.gone(ids, ids["supervisor"])

    def test_internal_supervisor_rejects_non_socket_before_driver(self):
        process = subprocess.run([str(self.helper), "--supervise-driver", str(os.getpid()), "43123"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=3)
        self.assertNotEqual(process.returncode, 0)
        self.assertFalse((self.root / "driver.json").exists())


if __name__ == "__main__":
    if sys.argv[1] == "--owner":
        owner_main(sys.argv[2], bool(int(sys.argv[3])))
    else:
        HELPER = Path(sys.argv.pop(1)).resolve()
        os.umask(0o077)
        unittest.main(verbosity=2)
