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
def terminal(argv, rows=40):
    pid, fd = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, 120, 0, 0))
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


    if Path("/opt/bin/uniterm").exists():
        codex_command_in_uniterm()


def codex_command_in_uniterm():
    """Run identical one-time requests in one invocation through the loopback provider."""
    first_result = threading.Event()
    second_result = threading.Event()

    class Model(BaseHTTPRequestHandler):
        turns = 0

        def log_message(self, *_):
            pass

        def do_POST(self):
            size = int(self.headers["Content-Length"])
            request = json.loads(self.rfile.read(size))
            results = {item["call_id"] for item in request.get("input", [])
                       if item.get("type") == "function_call_output"
                       and "Process exited with code 0" in item["output"]
                       and item["output"].rstrip().endswith("/work")}
            if "call_pwd_1" in results:
                first_result.set()
            if "call_pwd_2" in results:
                second_result.set()
            if request.get("tools") and not second_result.is_set():
                Model.turns += 1
                if Model.turns > 2 or (Model.turns == 2 and "call_pwd_1" not in results):
                    self.send_error(400, "Missing first tool result or unexpected extra model turn")
                    return
                if Model.turns == 2:
                    # Wait for the actual return to the composer between the identical requests.
                    deadline = time.monotonic() + 2
                    while time.monotonic() < deadline:
                        screen = json.loads(workspace("pane", "read", "1", "--lines", "80", "--json"))["text"]
                        if "✔ You approved codex to run pwd this time" in screen and "› Ask Codex" in screen:
                            break
                        time.sleep(.02)
                    else:
                        self.send_error(400, "Missing native receipt and composer after the first request")
                        return
                item = {
                    "type": "function_call", "id": f"fc_pwd_{Model.turns}", "call_id": f"call_pwd_{Model.turns}",
                    "name": "exec_command",
                    "arguments": json.dumps({"cmd": "pwd", "sandbox_permissions": "require_escalated",
                                             "justification": "May I print the test working directory?"}),
                }
            else:
                # Codex can generate a short conversation title in parallel.
                item = {"type": "message", "id": "msg_fixture", "role": "assistant",
                        "content": [{"type": "output_text", "text": "Check folder"}]}
            response_id = "resp_" + item["id"]
            events = [
                {"type": "response.created", "response": {"id": response_id, "status": "in_progress"}},
                {"type": "response.output_item.done", "output_index": 0, "item": item},
                {"type": "response.completed", "response": {"id": response_id, "status": "completed", "output": [item]}},
            ]
            body = "".join(f"data: {json.dumps(event)}\n\n" for event in events).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    home = Path("/home/test")
    state = home / ".state"
    runtime = Path("/tmp/uniterm-fixture")
    runtime.mkdir(mode=0o700)
    os.environ.update(SHELL="/bin/bash", PATH="/opt/bin:/usr/bin:/bin", XDG_RUNTIME_DIR=str(runtime))
    settings = home / ".config/uniterm/uniterm.conf"
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text("auto-install-connectors = false\nauto-update-connectors = false\n")
    config = home / ".codex/config.toml"
    config.write_text('approval_policy="on-request"\napprovals_reviewer="user"\n' + config.read_text())

    def ut(*args):
        result = subprocess.run(["/opt/bin/uniterm", *args], capture_output=True, text=True, timeout=8, check=True)
        return result.stdout

    model = ThreadingHTTPServer(("127.0.0.1", 4212), Model)
    thread = threading.Thread(target=model.serve_forever, daemon=True)
    thread.start()
    try:
        ut("workspace", "new", "-d", "codex-test")
        def workspace(*args):
            return ut(*args, "-w", "codex-test")
        workspace("agent", "auto-approve", "off")
        workspace("pane", "prompt", "1", "codex --no-daemon 'Report the test working directory.'")
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            screen = json.loads(workspace("pane", "read", "1", "--lines", "80", "--json"))["text"]
            if "Yes, proceed (y)" in screen and "esc to cancel" in screen:
                break
            time.sleep(.05)
        else:
            raise AssertionError(f"Native Codex command picker did not appear: {screen}")
        assert not first_result.wait(.4), "Command ran with Workspace Auto-approve OFF"
        workspace("agent", "auto-approve", "on")
        assert second_result.wait(8), "Codex did not return both identical pwd results with Auto-approve ON"
        ut("workspace", "stop", "codex-test")
        records = [json.loads(line)["event"] for path in state.rglob("*.log")
                   for line in path.read_text().splitlines()]
        deliveries = [record["PermissionApprovalDelivery"] for record in records if "PermissionApprovalDelivery" in record]
        assert len(deliveries) == 2 and all(record["accepted"] for record in deliveries), deliveries
        assert deliveries[0]["invocation"] == deliveries[1]["invocation"], deliveries
        assert not any((home / ".codex/rules").glob("*.rules")), "A persistent tool grant was saved"
        print("Uniterm delivered both one-time approvals and the fixture received both identical pwd results")
    finally:
        model.shutdown()
        model.server_close()
        thread.join()


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


def claude_consent():
    # Local fixture data makes an existing auto-mode user eligible for this offer.
    # The CLI presents it at query end. Cancel an unanswered loopback request
    # so the original draft is visible and no model or credential is involved.
    home = Path("/home/test")
    key = "uniterm-offline-fixture-key"
    os.environ.update(ANTHROPIC_API_KEY=key, ANTHROPIC_BASE_URL="http://127.0.0.1:9",
                      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1",
                      LC_ALL="C.UTF-8", COLORTERM="truecolor")
    for rows in (40, 80):
        (home / ".claude.json").write_text(json.dumps({
            "numStartups": 12, "hasSeenAutoModeEntryWarning": True, "hasSeenAutoDefaultNotice": True,
            "autoModeEnvSetup": {"denials": 6}, "hasCompletedOnboarding": True, "theme": "dark",
            "autoUpdates": False, "customApiKeyResponses": {"approved": [key[-20:]]},
            "projects": {"/work": {"hasTrustDialogAccepted": True}},
        }))
        with terminal(["/opt/agent", "--permission-mode", "auto"], rows) as fd:
            data = capture_until(fd, lambda output: b"shift+tab to cycle" in output)
            os.write(fd, b"fixture")
            time.sleep(0.25)
            os.write(fd, b"\r")
            data += capture_until(fd, lambda output: b"ECONNREFUSED" in output)
            os.write(fd, b"\x03")
            data += capture_until(fd, lambda output: b"Esc to cancel" in output)
            # The native consent callback ignores actions in its first 500 ms.
            ready = time.monotonic() + 0.6
            data += capture_until(fd, lambda _: time.monotonic() >= ready)
            assert b"Teach auto mode about your environment?" in data
            Path(f"/results/claude-consent-{rows}.ansi").write_bytes(data)
            os.write(fd, b"\r")
            transition = capture_until(fd, lambda output: b"Space to toggle" in output)
            assert b"Also scan shell history" in transition
            if rows == 40:
                Path("/results/claude-consent-to-scope.ansi").write_bytes(transition)


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
    parser.add_argument("provider", choices=["codex", "claude", "claude-consent", "opencode", "grok"])
    parser.add_argument("--uniterm", type=Path, help="Also check Codex command approval with this Uniterm binary")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--executable", type=Path, help="Use this native provider executable instead of a PATH wrapper")
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
        {"codex": codex, "claude": claude, "claude-consent": claude_consent, "opencode": opencode, "grok": grok}[args.provider]()
        print(f"{args.provider}: real dialog appeared and acceptance was verified; zero paid model calls")
        return
    executable = args.executable or shutil.which("claude" if args.provider == "claude-consent" else args.provider)
    if not executable:
        parser.error(f"{args.provider} is not installed")
    if args.provider == "claude" and not args.authenticated_startup:
        parser.error("Claude needs --authenticated-startup for its startup connectivity check")
    output = (args.output or Path(tempfile.mkdtemp(prefix="uniterm-approval-"))).resolve()
    output.mkdir(parents=True, exist_ok=True)
    access = []
    if args.uniterm is not None:
        if args.provider != "codex" or not os.access(args.uniterm, os.X_OK):
            parser.error("--uniterm needs an executable Uniterm binary and the codex fixture")
        access.extend(["--ro-bind", str(args.uniterm.resolve()), "/opt/bin/uniterm",
                       "--ro-bind", str(Path(executable).resolve()), "/opt/bin/codex"])
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
    if args.provider == "claude-consent":
        for name in ("claude-consent-40.ansi", "claude-consent-80.ansi", "claude-consent-to-scope.ansi"):
            print(f"Capture: {output / name}")
        return
    capture = "claude-trust.ansi" if args.provider == "claude" else args.provider + "-live.ansi"
    print(f"Capture: {output / capture}")
    if args.provider == "grok":
        print(f"Startup capture: {output / 'grok-trust-live.ansi'}")


if __name__ == "__main__":
    main()
