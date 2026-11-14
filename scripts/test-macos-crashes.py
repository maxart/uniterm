#!/usr/bin/env python3
"""Verify crash collection scope and bounds with synthetic reports only."""

import contextlib
import importlib.util
import io
import os
from pathlib import Path
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "macos_crashes", Path(__file__).with_name("collect-macos-crashes.py")
)
collector = importlib.util.module_from_spec(spec)
spec.loader.exec_module(collector)


class CrashCollectionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.reports = self.root / "reports"
        self.reports.mkdir()
        self.output = self.root / "collected"

    def report(self, name, size=1, modified=200):
        path = self.reports / name
        with path.open("wb") as output:
            output.truncate(size)
        os.utime(path, (modified, modified))
        return path

    def collect(self):
        with contextlib.redirect_stdout(io.StringIO()):
            collector.collect(self.reports, self.output, 100, collector.report_names(self.root))
        return list(self.output.iterdir()) if self.output.exists() else []

    def test_identity_time_type_symlink_and_per_report_bound(self):
        self.report("ut-current.ips", 5 * 1024 * 1024)
        self.report("sh-current.crash")
        self.report("zsh-current.ips")
        self.report("ut-old.ips", modified=99)
        self.report("unrelated-current.ips")
        self.report("ut-not-a-report.txt")
        (self.reports / "ut-link.ips").symlink_to(self.reports / "ut-current.ips")
        files = self.collect()
        self.assertEqual({p.name for p in files}, {"ut-current.ips", "sh-current.crash", "zsh-current.ips"})
        self.assertEqual((self.output / "ut-current.ips").stat().st_size, 4 * 1024 * 1024)

    def test_report_count_is_bounded(self):
        for index in range(20):
            self.report(f"ut-{index}.ips")
        self.assertEqual(len(self.collect()), 16)

    def test_cargo_library_and_integration_binary_names(self):
        crate = self.root / "crates/example-lib"
        (crate / "tests").mkdir(parents=True)
        (crate / "Cargo.toml").touch()
        (crate / "tests/selection.rs").touch()
        self.report("example_lib-hash.ips")
        self.report("selection-hash.ips")
        self.assertEqual(
            {path.name for path in self.collect()},
            {"example_lib-hash.ips", "selection-hash.ips"},
        )

    def test_total_bytes_are_bounded(self):
        for index in range(20):
            self.report(f"ut-{index}.ips", 2 * 1024 * 1024)
        self.assertEqual(sum(p.stat().st_size for p in self.collect()), 16 * 1024 * 1024)

    def test_daemon_log_collection_retains_bounded_tail_only(self):
        report = self.report("uniterm-daemon-123.log", 5 * 1024 * 1024)
        with report.open("ab") as stream:
            stream.write(b"last failure")
        self.report("uniterm-daemon-old.log", modified=99)
        self.report("unrelated.log")
        (self.reports / "uniterm-daemon-link.log").symlink_to(report)
        with contextlib.redirect_stdout(io.StringIO()):
            collector.collect(self.reports, self.output, 100,
                              {"uniterm-daemon"}, {".log"}, tail=True)
        files = list(self.output.iterdir())
        self.assertEqual([path.name for path in files], [report.name])
        data = files[0].read_bytes()
        self.assertEqual(len(data), 4 * 1024 * 1024)
        self.assertTrue(data.endswith(b"last failure"))


if __name__ == "__main__":
    unittest.main()
