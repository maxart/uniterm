#!/usr/bin/env python3
"""Bounded real-agent inbox evaluation in disposable Linux namespaces.

This is an opt-in, networked test: --live authorizes model requests and temporary
credential copies. It never mounts host Workspaces, agent histories or sockets.
Run paired conditions separately; results are observations, not a speedup claim.
"""

import argparse
import base64
import contextlib
import fcntl
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time


PROVIDERS = ("codex", "claude", "grok", "opencode")
PROMPT = """This is a small collaboration test in a disposable project. Another worker
has the same assignment. Read TASKS.md, choose and complete exactly one available
assignment, coordinate ownership with your peer using the tools available here,
and verify your result. Do not duplicate work already claimed or completed.
Keep changes confined to your chosen task file. Do not spawn agents, install
packages, browse the web, or read authentication files. Keep planning brief,
use at most 8 tool calls, and finish with a short evidence-based handoff.
If you cannot access coordination tools, state that rather than impersonating
a human. Never use --human or alter UNITERM identity variables to gain access.
"""
TASKS = """# Two independent work assignments

Worker names do not preassign work. Choose exactly one available assignment.
Coordinate ownership before editing. Each task should have one owner.

1. slug.py: implement slug(text), lowercasing and replacing runs of whitespace
   with a single hyphen while stripping leading/trailing whitespace.
   Test: python3 -m unittest test_tasks.SlugTests
2. unique.py: implement unique(items), returning the first occurrence of each
   integer in order, without modifying the input.
   Test: python3 -m unittest test_tasks.UniqueTests

The other worker can finish the other task. Do not modify tests or this file.
Record your ownership and completion through the available coordination tools.
"""
TESTS = """import unittest
from slug import slug
from unique import unique
class SlugTests(unittest.TestCase):
    def test_slug(self):
        for value, expected in [('  Hello   WORLD  ', 'hello-world'), ('a\\tb\\nc', 'a-b-c'), ('', ''), (' x ', 'x')]:
            self.assertEqual(slug(value), expected)
class UniqueTests(unittest.TestCase):
    def test_unique(self):
        for value, expected in [([3,1,3,2,1], [3,1,2]), ([], []), ([0,-1,0], [0,-1])]:
            original = value[:]
            self.assertEqual(unique(value), expected)
            self.assertEqual(value, original)
"""


def run(argv, **kwargs):
    return subprocess.run(argv, check=True, text=True, capture_output=True,
                          timeout=kwargs.pop("timeout", 15), **kwargs)


def metrics(provider, events):
    """Normalize reported usage without counting streaming updates twice.

    Codex includes cache reads in input; the other adapters report them apart.
    Unknown monetary usage stays null, including OpenCode's subscription zeros.
    """
    result = dict(input_tokens=0, cached_input_tokens=0, output_tokens=0,
                  tool_calls=0, reported_cost_usd=None, usage_reported=False)
    for event in events:
        kind = event.get("type")
        item = event.get("item", {})
        if provider == "codex":
            result["tool_calls"] += kind == "item.completed" and item.get("type") == "command_execution"
            if kind == "turn.completed":
                usage = event.get("usage", {})
                cached = usage.get("cached_input_tokens", 0)
                result["input_tokens"] += usage.get("input_tokens", 0) - cached
                result["cached_input_tokens"] += cached
                result["output_tokens"] += usage.get("output_tokens", 0)
                result["usage_reported"] = bool(usage)
        elif provider in ("claude", "grok"):
            if provider == "claude" and kind == "assistant":
                result["tool_calls"] += sum(part.get("type") == "tool_use"
                    for part in event.get("message", {}).get("content", []))
            if provider == "grok":
                result["tool_calls"] += kind == "tool_call"
            if kind == ("result" if provider == "claude" else "end"):
                usage = event.get("usage", {})
                result.update(input_tokens=usage.get("input_tokens", 0) + usage.get("cache_creation_input_tokens", 0),
                    cached_input_tokens=usage.get("cache_read_input_tokens", 0),
                    output_tokens=usage.get("output_tokens", 0),
                    reported_cost_usd=event.get("total_cost_usd"), usage_reported=bool(usage))
        elif provider == "opencode":
            result["tool_calls"] += kind == "tool_use"
            if kind == "step_finish":
                usage = event.get("part", {}).get("tokens", {})
                cache = usage.get("cache", {})
                result["input_tokens"] += usage.get("input", 0) + cache.get("write", 0)
                result["cached_input_tokens"] += cache.get("read", 0)
                result["output_tokens"] += usage.get("output", 0)
                result["usage_reported"] = bool(usage)
    return result


def report_metrics(directory, providers):
    result = {}
    for provider in providers:
        events = []
        path = directory / (provider + ".jsonl")
        for line in (path.read_text().splitlines() if path.exists() else []):
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
        result[provider] = metrics(provider, events)
    return result


def control(method, params=None):
    path = Path(os.environ["UNITERM_SOCKET"]).with_suffix(".control.sock")
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(10)
        sock.connect(str(path))
        request = {"version": 1, "id": 1, "workspace": path.name.removesuffix(".control.sock"),
                   "method": method}
        if params is not None:
            request["params"] = params
        sock.sendall(json.dumps(request).encode() + b"\n")
        result = json.loads(sock.makefile("rb").readline())
    response = result.get("data", result)
    if response.get("error"):
        raise RuntimeError(response["error"])
    return response.get("result", response)


def inbox(operation, epoch=None):
    return control("inbox", {"project": None, "identity": None, "epoch": epoch,
                             "operation": operation})


def prepare_auth(providers):
    """Copy only credentials into tmpfs homes, never into results or the project."""
    home = Path.home()
    for provider, source, destination in [
        ("codex", "/auth/codex", ".codex/auth.json"),
        ("claude", "/auth/claude", ".claude/.credentials.json"),
        ("grok", "/auth/grok", ".grok/auth.json"),
    ]:
        if provider in providers or (provider == "codex" and "opencode" in providers):
            target = home / destination
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
            target.chmod(0o600)
    (home / ".claude.json").write_text(json.dumps({"hasCompletedOnboarding": True,
        "theme": "dark", "autoUpdates": False}))
    (home / ".grok").mkdir(exist_ok=True)
    (home / ".grok/config.toml").write_text('[cli]\nauto_update=false\n'
        '[features]\ntelemetry=false\nfeedback=false\ncodebase_indexing=false\n')
    if "opencode" in providers:
        auth = json.loads(Path("/auth/codex").read_text())["tokens"]
        payload = auth["access_token"].split(".")[1]
        claims = json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))
        if claims["exp"] <= time.time() + 180:
            raise RuntimeError("OpenCode requires a fresh Codex access token; will not rotate its refresh token")
        credential = [{"id": "cred_uniterm_inbox", "integrationID": "openai", "label": "inbox-test",
            "active": True, "value": {"type": "oauth", "methodID": "chatgpt-browser",
            "access": auth["access_token"], "refresh": "", "expires": claims["exp"] * 1000,
            "metadata": {"accountID": auth["account_id"]}}}]
        run(["/opt/opencode", "auth", "import", "--standalone"], input=json.dumps(credential))


def save_auth(providers):
    for provider, relative in [("codex", ".codex/auth.json"),
                               ("claude", ".claude/.credentials.json"), ("grok", ".grok/auth.json")]:
        path = Path.home() / relative
        if provider in providers and path.exists():
            # Keep rotated tokens outside report files, and reuse them next run.
            shutil.copyfile(path, "/auth/" + provider)


def worker(provider, condition, argv):
    """Test-only adapter: native headless CLI, normal Uniterm task/PTY ownership."""
    # Uniterm probes installed versions during discovery. Those invocations
    # must never become prompts or overwrite a live trial's completion marker.
    if argv in (["--version"], ["-v"], ["-V"], ["--help"], ["-h"]):
        os.execv("/opt/" + provider, ["/opt/" + provider, *argv])
    prompt = argv[-1] if argv else PROMPT
    prompt = prompt.removeprefix("--prompt=")
    if condition == "baseline":
        prompt = prompt.split(" Uniterm Project inbox:", 1)[0]
        # The baseline has the same task but no inbox discovery hook/context.
        prompt += "\nFor this baseline, use files if coordination is needed; do not use ut inbox."
        settings = Path.home() / ".claude/settings.json"
        if provider == "claude" and settings.exists():
            config = json.loads(settings.read_text())
            for groups in config.get("hooks", {}).values():
                for group in groups:
                    group["hooks"] = [hook for hook in group.get("hooks", [])
                                      if "inbox-context" not in hook.get("command", "")]
            settings.write_text(json.dumps(config))
    commands = {
        "codex": ["/opt/codex", "exec", "--json", "--ephemeral", "--ignore-user-config",
                  "--dangerously-bypass-approvals-and-sandbox", "--skip-git-repo-check",
                  "-c", 'model_reasoning_effort="low"', prompt],
        "claude": ["/opt/claude", "--print", "--output-format", "stream-json", "--verbose",
                   "--max-turns", "8", "--max-budget-usd", "0.50", "--effort", "low",
                   "--allowedTools", "Bash,Read,Edit,Write", "--strict-mcp-config", prompt],
        "grok": ["/opt/grok", "--single", prompt, "--output-format", "streaming-json",
                 "--max-turns", "8", "--no-subagents", "--reasoning-effort", "low",
                 "--permission-mode", "bypassPermissions"],
        "opencode": ["/opt/opencode", "run", "--standalone", "--format", "json", "--auto",
                     "--model", "openai/gpt-6-astra", prompt],
    }
    start = time.monotonic()
    with open(f"/results/{provider}.jsonl", "w") as out, open(f"/results/{provider}.stderr", "w") as err:
        child = subprocess.Popen(commands[provider], stdout=out, stderr=err)
        code = child.wait()
    Path(f"/results/{provider}.done.json").write_text(json.dumps({"exit_code": code,
        "elapsed_seconds": time.monotonic() - start}))


def inside(args):
    os.environ.clear()
    os.environ.update(HOME="/home/test", PATH="/test-bin:/usr/bin:/bin", TERM="xterm-256color",
        XDG_CONFIG_HOME="/home/test/.config", XDG_DATA_HOME="/home/test/.local/share",
        XDG_STATE_HOME="/state", XDG_RUNTIME_DIR="/runtime", SHELL="/bin/bash",
        DISABLE_AUTOUPDATER="1", CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1")
    for path in ["/home/test", "/state", "/runtime", "/test-bin", "/work"]:
        Path(path).mkdir(parents=True, exist_ok=True)
    # Fail before creating model sessions if the container lacks DNS.
    socket.getaddrinfo("chatgpt.com", 443)
    os.symlink("/opt/uniterm", "/test-bin/ut")
    prepare_auth(args.providers)
    os.chdir("/work")
    task_text = TASKS if args.scenario == "shared" else "# Independent task\nImplement slug.py as specified below. You are the only worker; unique.py is out of scope.\n" + TASKS.split("1. ", 1)[1].split("2. ", 1)[0]
    Path("TASKS.md").write_text(task_text)
    Path("test_tasks.py").write_text(TESTS)
    Path("slug.py").write_text("def slug(text):\n    return text\n")
    Path("unique.py").write_text("def unique(items):\n    return list(items)\n")
    run(["git", "init", "-q"])
    for provider in args.providers:
        wrapper = Path("/test-bin") / provider
        wrapper.write_text(f"#!/bin/sh\nexec /usr/bin/python3 /runner.py --worker {provider} --condition {args.condition} -- \"$@\"\n")
        wrapper.chmod(0o755)
    # No host environment or default Workspace can be selected.
    run(["ut", "new", "-d", "inbox-evaluation"])
    sockets = list(Path("/runtime").rglob("inbox-evaluation.sock"))
    assert len(sockets) == 1, sockets
    os.environ["UNITERM_SOCKET"] = str(sockets[0])
    run(["ut", "agent", "auto-approve", "on"])
    if args.condition == "inbox" and args.scenario == "shared":
        for name in ["slug", "unique"]:
            inbox({"op": "post", "kind": "request", "body": f"Implement and test {name}.py as specified in TASKS.md. Claim before editing; post a handoff with test evidence.",
                   "reply_to": None, "to": None, "task": None, "key": name, "ttl_ms": 600_000})
    versions = {provider: run(["/opt/" + provider, "--version"]).stdout.strip()
                for provider in args.providers}
    start = time.monotonic()
    try:
        for provider in args.providers:
            prompt = PROMPT if args.scenario == "shared" else "Read TASKS.md, implement slug.py, and run its supplied test. You are the only worker and have no peer dependencies. Keep changes to slug.py, use at most four tool calls, do not spawn agents, and finish briefly with test evidence."
            control("task_start", {"launch": {"agent": provider, "prompt": prompt}})
        deadline = start + args.timeout
        while time.monotonic() < deadline:
            if all(Path(f"/results/{p}.done.json").exists() for p in args.providers):
                break
            time.sleep(0.5)  # Bounded external test supervisor, never product polling.
        timed_out = [p for p in args.providers if not Path(f"/results/{p}.done.json").exists()]
        board = inbox({"op": "list", "after": 0, "thread": None, "limit": 20})
        Path("/results/board.json").write_text(json.dumps(board, indent=2))
        Path("/results/agents.json").write_text(run(["ut", "agent", "list", "--json"]).stdout)
        checks = {}
        for name in (["SlugTests", "UniqueTests"] if args.scenario == "shared" else ["SlugTests"]):
            result = subprocess.run(["python3", "-m", "unittest", f"test_tasks.{name}"],
                capture_output=True, text=True, timeout=10)
            checks[name] = {"passed": result.returncode == 0, "output": result.stderr}
        Path("/results/summary.json").write_text(json.dumps({"providers": args.providers,
            "condition": args.condition, "scenario": args.scenario, "native_versions": versions,
            "elapsed_seconds": time.monotonic() - start,
            "timed_out": timed_out, "checks": checks,
            "metrics": report_metrics(Path("/results"), args.providers)}, indent=2))
        for name in ["slug.py", "unique.py"]:
            shutil.copyfile(name, "/results/" + name)
    finally:
        save_auth(args.providers)
        with contextlib.suppress(Exception):
            run(["ut", "workspace", "stop", "inbox-evaluation"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--providers", nargs="+", choices=PROVIDERS, default=["codex", "claude"])
    parser.add_argument("--condition", choices=["inbox", "baseline"], default="inbox")
    parser.add_argument("--scenario", choices=["shared", "independent"], default="shared")
    parser.add_argument("--binary", type=Path, default=Path("target/debug/ut"))
    parser.add_argument("--output", type=Path)
    parser.add_argument("--credential-cache", type=Path,
                        help="Private reusable temporary credential directory; kept separate from reports")
    parser.add_argument("--timeout", type=int, default=120)
    parser.add_argument("--live", action="store_true")
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--worker", choices=PROVIDERS, help=argparse.SUPPRESS)
    args, rest = parser.parse_known_args()
    if args.worker:
        worker(args.worker, args.condition, rest[1:] if rest[:1] == ["--"] else rest)
        return
    if args.inside:
        inside(args)
        return
    if rest or not args.live or not args.output or not args.credential_cache:
        parser.error("explicit --live, --output and --credential-cache are required; no extra arguments")
    if not 10 <= args.timeout <= 180 or not 1 <= len(set(args.providers)) == len(args.providers) <= 2:
        parser.error("use one or two distinct providers and a 10-180 second deadline")
    output = args.output.resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    cache = args.credential_cache.resolve()
    cache.mkdir(mode=0o700, parents=True, exist_ok=True)
    if cache.stat().st_mode & 0o077:
        parser.error("credential cache must be owner-only")
    lock = (cache / ".lock").open("w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        parser.error("another live evaluation owns this credential cache")
    mounts = []
    for provider in args.providers:
        executable = shutil.which(provider)
        if not executable:
            parser.error(f"missing installed {provider}")
        mounts += ["--ro-bind", str(Path(executable).resolve()), "/opt/" + provider]
        if provider == "codex":
            helper = Path(executable).resolve().with_name("codex-code-mode-host")
            if helper.exists():
                mounts += ["--ro-bind", str(helper), "/opt/codex-code-mode-host"]
    auth = {"codex": ".codex/auth.json", "claude": ".claude/.credentials.json", "grok": ".grok/auth.json"}
    needed = set(args.providers) | ({"codex"} if "opencode" in args.providers else set())
    for provider in needed:
        if provider in auth:
            original = Path.home() / auth[provider]
            if not original.is_file():
                parser.error(f"missing {provider} credentials")
            source = cache / provider
            if not source.exists():
                value = json.loads(original.read_text())
                if provider == "claude":
                    value = {"claudeAiOauth": value["claudeAiOauth"]}
                source.write_text(json.dumps(value))
                source.chmod(0o600)
            mounts += ["--bind", str(source), "/auth/" + provider]
    command = ["bwrap", "--unshare-all", "--share-net", "--die-with-parent", "--clearenv",
        "--ro-bind", "/usr", "/usr", "--symlink", "usr/bin", "/bin", "--symlink", "usr/lib", "/lib",
        "--symlink", "usr/lib", "/lib64", "--ro-bind", "/etc", "/etc",
        "--ro-bind", str(Path("/etc/resolv.conf").resolve()), str(Path("/etc/resolv.conf").resolve()),
        "--proc", "/proc",
        "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/home", "--tmpfs", "/work",
        "--ro-bind", str(args.binary.resolve()), "/opt/uniterm", *mounts,
        "--ro-bind", str(Path(__file__).resolve()), "/runner.py", "--bind", str(output), "/results",
        "--", "/usr/bin/python3", "/runner.py", "--inside", "--condition", args.condition,
        "--scenario", args.scenario, "--timeout", str(args.timeout), "--providers", *args.providers]
    process = subprocess.Popen(command, start_new_session=True)
    try:
        code = process.wait(timeout=args.timeout + 60)
        if code:
            raise SystemExit(code)
    finally:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
        process.wait()
    print(f"Evaluation evidence: {output / 'summary.json'}")


if __name__ == "__main__":
    main()
