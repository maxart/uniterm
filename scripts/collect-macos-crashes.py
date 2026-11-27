#!/usr/bin/env python3
"""Retain bounded, job-local native crash evidence after macOS CI failures."""

import argparse
import os
from pathlib import Path
import platform
import time


def report_names(root):
    """Include shipped binaries plus Cargo integration and library test names."""
    names = {"uniterm", "ut", "bash", "sh", "zsh"}
    names.update(path.stem for path in (root / "crates").glob("*/tests/*.rs"))
    names.update(
        path.parent.name.replace("-", "_")
        for path in (root / "crates").glob("*/Cargo.toml")
    )
    return names


def collect(directory, output, since, names, suffixes=None, tail=False):
    """Copy recent matching reports without scanning other users or old jobs."""
    suffixes = suffixes or {".ips", ".crash"}
    deadline = time.monotonic() + 10
    candidates = []
    if not directory.is_dir():
        return
    for index, path in enumerate(directory.iterdir()):
        if index >= 10000 or time.monotonic() >= deadline:
            break
        if path.suffix not in suffixes or path.is_symlink():
            continue
        if not any(path.name.startswith(name + "-") for name in names):
            continue
        try:
            modified = path.stat().st_mtime
            if path.is_file() and modified >= since:
                candidates.append((modified, path))
        except OSError:
            continue
    remaining = 16 * 1024 * 1024
    for _, path in sorted(candidates, reverse=True)[:16]:
        if remaining <= 0 or time.monotonic() >= deadline:
            break
        try:
            with path.open("rb") as report:
                limit = min(remaining, 4 * 1024 * 1024)
                if tail:
                    report.seek(max(0, os.fstat(report.fileno()).st_size - limit))
                data = report.read(limit)
            output.mkdir(parents=True, exist_ok=True)
            (output / path.name).write_bytes(data)
            remaining -= len(data)
            print(f"Collected {path.name}: {len(data)} bytes (bounded)")
        except OSError as error:
            print(f"Could not collect {path.name}: {error}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mark", action="store_true")
    args = parser.parse_args()
    if platform.system() != "Darwin" or os.environ.get("GITHUB_ACTIONS") != "true":
        print("Crash collection is restricted to macOS GitHub Actions jobs")
        return
    temporary = Path(os.environ["RUNNER_TEMP"])
    marker = temporary / "uniterm-native-started"
    if args.mark:
        marker.write_text(str(time.time()))
        (temporary / "uniterm-daemon-logs").mkdir(mode=0o700, exist_ok=True)
        return
    # A missing marker must never expand collection to unrelated older reports.
    if not marker.is_file():
        print("No diagnostic window was recorded")
        return
    root = Path(__file__).resolve().parent.parent
    collect(
        Path.home() / "Library/Logs/DiagnosticReports",
        temporary / "uniterm-native-crashes",
        float(marker.read_text()),
        report_names(root),
    )
    collect(
        temporary / "uniterm-daemon-logs",
        temporary / "uniterm-native-crashes",
        float(marker.read_text()),
        {"uniterm-daemon"}, {".log"}, tail=True,
    )


if __name__ == "__main__":
    main()
