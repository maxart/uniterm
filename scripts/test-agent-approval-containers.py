#!/usr/bin/env python3
"""Capture and accept real agent dialogs without paid model calls.

Requires Linux, Bubblewrap, Python 3.11+, and the selected installed CLI.
Codex, OpenCode and Grok run offline without credentials.
Claude optionally uses authenticated startup in a disposable container.
The resulting ANSI files can be replayed by providers::approval::tests.
"""

import argparse
import base64
import contextlib
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import tomllib
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


@contextlib.contextmanager
def terminal(argv):
    pid, fd = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        os.execv(argv[0], argv)
    try:
        yield fd
    finally:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        os.close(fd)


def capture_until(fd, predicate, timeout=12):
    output = bytearray()
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if select.select([fd], [], [], 0.05)[0]:
            try:
                data = os.read(fd, 65536)
            except OSError:
                if predicate(output):
                    return bytes(output)
                break
            if not data:
                break
            output.extend(data)
            if b"\x1b[6n" in data:
                os.write(fd, b"\x1b[1;1R")
            if b"\x1b[c" in data:
                os.write(fd, b"\x1b[?1;2c")
            for query, color in [(b"10", b"ffff/ffff/ffff"), (b"11", b"0000/0000/0000")]:
                if b"\x1b]" + query + b";?" in data:
                    os.write(fd, b"\x1b]" + query + b";rgb:" + color + b"\x1b\\")
        if predicate(output):
            return bytes(output)
    Path("/results/failed-capture.ansi").write_bytes(output)
    raise AssertionError("The expected dialog or acceptance did not arrive; see failed-capture.ansi")


def codex():
    home = Path("/home/test/.codex")
    home.mkdir()
    (home / "config.toml").write_text(
        'model_provider="fixture"\nmodel="fixture"\n'
        '[model_providers.fixture]\nname="Local fixture"\n'
        'base_url="http://127.0.0.1:4212/v1"\nwire_api="responses"\n'
    )
    subprocess.run(["git", "init", "-q", "/work"], check=True)
    with terminal(["/opt/agent", "--no-daemon"]) as fd:
        screen = capture_until(fd, lambda data: b"Trust and continue" in data and b"esc" in data)
        Path("/results/codex-live.ansi").write_bytes(screen)
        # Startup performs terminal capability negotiation before dispatching
        # keys. Finish that handshake before testing the trust action.
        time.sleep(0.1)
        os.write(fd, b"\r")

        def trusted(_):
            config = tomllib.loads((home / "config.toml").read_text())
            return config.get("projects", {}).get("/work", {}).get("trust_level") == "trusted"

        capture_until(fd, trusted)


def claude():
    home = Path("/home/test")
    (home / ".claude").mkdir()
    credentials = json.loads(Path("/auth/claude").read_text())
    credential_path = home / ".claude/.credentials.json"
    credential_path.write_text(json.dumps({"claudeAiOauth": credentials["claudeAiOauth"]}))
    credential_path.chmod(0o600)
    config_path = home / ".claude.json"
    config_path.write_text(json.dumps({
        "hasCompletedOnboarding": True, "theme": "dark", "autoUpdates": False,
    }))
    with terminal(["/opt/agent"]) as fd:
        screen = capture_until(fd, lambda data: b"trust" in data and b"confirm" in data)
        Path("/results/claude-trust.ansi").write_bytes(screen)
        # This is startup preparation, not the Uniterm recognition benchmark.
        # Let the native CLI attach its input handlers before driving its UI.
        time.sleep(1)
        os.write(fd, b"\x1b[B")

        def selected(data):
            text = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", data).replace(b" ", b"")
            return "❯Yes,Itrustthisfolder".encode() in text

        capture_until(fd, selected)
        time.sleep(0.2)
        os.write(fd, b"\r")
        capture_until(fd, lambda _: json.loads(config_path.read_text()).get(
            "projects", {}).get("/work", {}).get("hasTrustDialogAccepted", False))


def opencode():
    # This password exists only inside the isolated container.
    os.environ["OPENCODE_SERVER_PASSWORD"] = "uniterm-isolated-test"
    authorization = base64.b64encode(b"opencode:uniterm-isolated-test").decode()

    def request(path, body=None):
        query = urllib.request.Request(
            "http://127.0.0.1:4211" + path,
            data=json.dumps(body).encode() if body is not None else None,
            headers={"Authorization": "Basic " + authorization, "Content-Type": "application/json"},
        )
        with urllib.request.urlopen(query, timeout=3) as response:
            return json.load(response)

    with open("/results/opencode-server.log", "w") as log:
        server = subprocess.Popen(["/opt/agent", "serve", "--port", "4211"], stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 10
            while True:
                try:
                    request("/openapi.json")
                    break
                except (OSError, ValueError):
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(0.1)
            session = request("/api/session", {
                "location": {"directory": "/work"},
                "permissions": [{"action": "*", "resource": "*", "effect": "ask"}],
            })["data"]["id"]
            permission = "/api/session/" + session + "/permission"
            request(permission, {
                "action": "bash", "resources": ["echo uniterm-test"],
                "metadata": {"command": "echo uniterm-test"},
            })
            with terminal(["/opt/agent", "--server", "http://127.0.0.1:4211", "--session", session]) as fd:
                screen = capture_until(fd, lambda data: b"Allow once" in data and b"confirm" in data)
                Path("/results/opencode-live.ansi").write_bytes(screen)
                assert request(permission)["data"], "No permission was pending"
                os.write(fd, b"\r")
                capture_until(fd, lambda _: not request(permission)["data"])
        finally:
            server.terminate()
            server.wait(timeout=5)


def grok():
    accepted = threading.Event()

    class Model(BaseHTTPRequestHandler):
        calls = 0

        def log_message(self, *_):
            pass

        def handle(self):
            # The test terminates the CLI once its tool result proves acceptance.
            with contextlib.suppress(BrokenPipeError, ConnectionResetError):
                super().handle()

        def do_GET(self):
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps({"object": "list", "data": [
                {"id": "uniterm-test", "object": "model", "owned_by": "local"},
            ]}).encode())

        def do_POST(self):
            data = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
            Model.calls += 1
            if any(message.get("role") == "tool" for message in data.get("messages", [])):
                accepted.set()
                self.send_error(429)
                return
            if Model.calls > 4:
                self.send_error(429)
                return
            # A deterministic harmless command triggers the real permission UI.
            tool = {
                "index": 0, "id": "call_uniterm_test", "type": "function",
                "function": {"name": "run_terminal_command", "arguments": json.dumps({
                    "command": "python3 -c 'print(123)'",
                    "description": "Uniterm isolated approval check",
                })},
            }
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for delta, finish in [({"role": "assistant", "tool_calls": [tool]}, None), ({}, "tool_calls")]:
                event = {"id": "chatcmpl-test", "object": "chat.completion.chunk", "created": 0,
                         "model": "uniterm-test", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
                self.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")

    server = ThreadingHTTPServer(("127.0.0.1", 4212), Model)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    os.environ.update(XAI_API_KEY="uniterm-dummy", GROK_MODELS_BASE_URL="http://127.0.0.1:4212/v1",
                      GROK_CLI_CHAT_PROXY_BASE_URL="http://127.0.0.1:4212/v1")
    home = Path("/home/test/.grok")
    home.mkdir()
    (home / "hooks").mkdir()
    (home / "hooks/session-start.json").write_text(json.dumps({"hooks": {
        "SessionStart": [{"hooks": [{"type": "command", "command": "touch /tmp/trust-hook-ran"}]}],
    }}))
    (home / "config.toml").write_text(
        '[folder_trust]\nenabled=true\n[cli]\nauto_update=false\n[models]\ndefault="uniterm-test"\n'
        '[features]\nsupport_permission=true\ntelemetry=false\nfeedback=false\ncodebase_indexing=false\n'
    )
    try:
        with terminal(["/opt/agent", "--permission-mode", "default"]) as fd:
            screen = capture_until(fd, lambda data: b"No, quit" in data and b"Yes, proceed" in data)
            Path("/results/grok-trust-live.ansi").write_bytes(screen)
            assert not Path("/tmp/trust-hook-ran").exists()
            os.write(fd, b"y")
            capture_until(fd, lambda data: Path("/tmp/trust-hook-ran").exists())
        with terminal(["/opt/agent", "--permission-mode", "default", "Run the local approval fixture."]) as fd:
            screen = capture_until(fd, lambda data: b"Yes, proceed" in data and b"Ctrl+c:cancel" in re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", data))
            Path("/results/grok-live.ansi").write_bytes(screen)
            assert not accepted.is_set(), "Command ran before approval"
            os.write(fd, b"\r")
            capture_until(fd, lambda _: accepted.is_set())
    finally:
        server.shutdown()
        server.server_close()
        worker.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("provider", choices=["codex", "claude", "opencode", "grok"])
    parser.add_argument("--output", type=Path)
    parser.add_argument("--authenticated-startup", action="store_true",
                        help="Allow Claude startup networking and a temporary credential copy; no model prompts")
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.inside:
        os.environ.clear()
        os.environ.update(HOME="/home/test", PATH="/usr/bin:/bin", TERM="xterm-256color",
                          XDG_CONFIG_HOME="/home/test/.config", XDG_STATE_HOME="/home/test/.state",
                          XDG_DATA_HOME="/home/test/.data")
        Path("/home/test").mkdir(parents=True)
        os.chdir("/work")
        {"codex": codex, "claude": claude, "opencode": opencode, "grok": grok}[args.provider]()
        print(f"{args.provider}: real dialog appeared and acceptance was verified; zero paid model calls")
        return
    executable = shutil.which(args.provider)
    if not executable:
        parser.error(f"{args.provider} is not installed")
    if args.provider == "claude" and not args.authenticated_startup:
        parser.error("Claude needs --authenticated-startup for its startup connectivity check")
    output = (args.output or Path(tempfile.mkdtemp(prefix="uniterm-approval-"))).resolve()
    output.mkdir(parents=True, exist_ok=True)
    access = []
    if args.provider == "claude":
        credentials = Path.home() / ".claude/.credentials.json"
        if not credentials.is_file():
            parser.error("Claude credentials are unavailable")
        resolver = str(Path("/etc/resolv.conf").resolve())
        access = ["--share-net", "--ro-bind", str(credentials), "/auth/claude",
                  "--ro-bind", resolver, resolver]
    command = ["bwrap", "--unshare-all", *access, "--die-with-parent", "--clearenv",
               "--ro-bind", "/usr", "/usr", "--symlink", "usr/bin", "/bin",
               "--symlink", "usr/lib", "/lib", "--symlink", "usr/lib", "/lib64",
               "--ro-bind", "/etc", "/etc", "--proc", "/proc", "--dev", "/dev",
               "--tmpfs", "/tmp", "--dir", "/work",
               "--ro-bind", str(Path(executable).resolve()), "/opt/agent",
               "--ro-bind", str(Path(__file__).resolve()), "/fixture.py",
               "--bind", str(output), "/results", "--", "/usr/bin/python3",
               "/fixture.py", args.provider, "--inside"]
    subprocess.run(command, check=True, timeout=40)
    capture = "claude-trust.ansi" if args.provider == "claude" else args.provider + "-live.ansi"
    print(f"Capture: {output / capture}")
    if args.provider == "grok":
        print(f"Startup capture: {output / 'grok-trust-live.ansi'}")


if __name__ == "__main__":
    main()
