#!/usr/bin/env python3
"""Exercise the hosted-runner cleanup guard without removing any real files."""
import os
from pathlib import Path
import subprocess
import tempfile

script = Path(__file__).with_name("prepare-ci-disk.sh").resolve()
with tempfile.TemporaryDirectory(prefix="uniterm-ci-disk-test-") as directory:
    root = Path(directory)
    log = root / "sudo.log"
    (root / "sudo").write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$CI_TEST_LOG"\n')
    (root / "df").write_text(
        '#!/bin/sh\nprintf "Filesystem 1024-blocks Used Available Capacity Mounted\\n"\n'
        'printf "test 99999999 0 %s 0%% /\\n" "$CI_TEST_FREE"\n'
    )
    for name in ("sudo", "df"):
        (root / name).chmod(0o755)
    environment = dict(os.environ, PATH=f"{root}:/usr/bin:/bin", CI_TEST_LOG=str(log))
    for actions, platform, free, expected in [
        ("false", "Linux", "30000000", 2),
        ("true", "macOS", "30000000", 2),
        ("true", "Linux", "30000000", 0),
        ("true", "Linux", "1000", 1),
        ("true", "Linux", "invalid", 1),
    ]:
        log.unlink(missing_ok=True)
        result = subprocess.run(
            ["sh", str(script)],
            env=dict(environment, GITHUB_ACTIONS=actions, RUNNER_OS=platform, CI_TEST_FREE=free),
            capture_output=True, text=True, timeout=5,
        )
        assert result.returncode == expected, (actions, platform, free, result)
        if expected == 2:
            assert not log.exists(), "cleanup ran outside a hosted Linux job"
        else:
            assert log.read_text().split() == [
                "rm", "-rf", "--", "/usr/share/dotnet", "/usr/share/swift",
                "/usr/local/.ghcup", "/opt/ghc", "/opt/hostedtoolcache/CodeQL",
                "/usr/local/share/boost",
            ], "cleanup must preserve Rust, Android, source, and cache inputs"
print("CI disk guard: five isolated refusal/space/allow-list cases passed")
