#!/usr/bin/env python3
"""Exercise the compatibility diagnostic against disposable copied-state fixtures."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


def digest(root):
    return {str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in root.rglob("*") if p.is_file()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    binary = str(args.binary.resolve())
    with tempfile.TemporaryDirectory(prefix="uniterm-history-check-") as directory:
        root = Path(directory)
        state = root / "state"
        history = state / "uniterm"
        history.mkdir(parents=True)
        env = dict(os.environ, HOME=str(root / "home"), XDG_STATE_HOME=str(state),
                   XDG_RUNTIME_DIR=str(root / "runtime"), XDG_CONFIG_HOME=str(root / "config"))

        def check(name, success, override=None):
            before = digest(root)
            result = subprocess.run([binary, name], env=override or env, capture_output=True,
                                    text=True, timeout=10)
            assert (result.returncode == 0) == success, (name, result.stdout, result.stderr)
            assert digest(root) == before, "Diagnostic changed copied Workspace state"
            return result.stdout

        def fixture(name, version, event=None, sequence=1):
            envelope = {"version": version, "sequence": sequence, "timestamp_ms": 0,
                        "workspace": name, "event": event or {"TaskCreated": {
                            "id": 1, "title": "copied task", "status": "Todo"}}}
            (history / (name + ".log")).write_text(json.dumps(envelope) + "\n")

        for version in [7, 8, 9, 10]:
            name = f"schema-{version}"
            fixture(name, version)
            assert "1 Tasks" in check(name, True)
        fixture("future", 999)
        check("future", False)
        fixture("unknown-event", 10, {"FutureEvent": {}})
        check("unknown-event", False)
        (history / "corrupt.log").write_text('{"truncated":')
        check("corrupt", False)
        check("absent", False)
        check("../escape", False)
        fixture("snapshot", 10)
        (history / "snapshot.snap").write_bytes(b"unreadable snapshot")
        check("snapshot", False)
        without_state = dict(env)
        without_state.pop("XDG_STATE_HOME")
        check("schema-9", False, without_state)
        check("schema-9", False, dict(env, XDG_STATE_HOME=""))
        check("schema-9", False, dict(env, XDG_STATE_HOME="relative"))
    print("history_check: 13 compatibility and refusal cases passed; source bytes unchanged")


if __name__ == "__main__":
    main()
