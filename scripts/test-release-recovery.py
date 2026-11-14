#!/usr/bin/env python3
"""Exercise manual release identity and the actual workflow publication guards."""

import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile
import textwrap
import tomllib
import unittest

ROOT = Path(__file__).resolve().parent.parent
RESOLVER = ROOT / "scripts/resolve-release.py"
RELEASE = (ROOT / ".github/workflows/release.yml").read_text()
GIT_ENV = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)


def git(cwd, *args):
    return subprocess.check_output(
        ["git", *args], cwd=cwd, env=GIT_ENV, text=True, stderr=subprocess.PIPE
    ).strip()


class ReleaseRecovery(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="uniterm-release-recovery-")
        self.addCleanup(self.directory.cleanup)
        root = Path(self.directory.name)
        self.author = root / "author"
        self.origin = root / "origin.git"
        self.runner = root / "runner"
        self.output = root / "output"
        git(root, "init", "-q", "-b", "main", str(self.author))
        git(self.author, "config", "user.name", "Release fixture")
        git(self.author, "config", "user.email", "release@example.invalid")
        self.commit("1.2.16")
        self.sha = git(self.author, "rev-parse", "HEAD")
        git(self.author, "tag", "-a", "v1.2.16", "-m", "Original notes")
        self.tag_object = git(self.author, "rev-parse", "refs/tags/v1.2.16")
        git(root, "clone", "-q", "--bare", str(self.author), str(self.origin))
        git(self.author, "remote", "add", "origin", str(self.origin))
        self.commit("1.2.17")
        git(self.author, "push", "-q", "origin", "main")
        git(root, "clone", "-q", str(self.origin), str(self.runner))

    def commit(self, version):
        (self.author / "Cargo.toml").write_text(
            f'[workspace.package]\nversion = "{version}"\n'
        )
        git(self.author, "add", "Cargo.toml")
        git(self.author, "commit", "-qm", f"Version {version}")

    def resolve(self, tag):
        return subprocess.run(
            [sys.executable, str(RESOLVER)], cwd=self.runner,
            env=dict(GIT_ENV, RELEASE_TAG=tag, GITHUB_OUTPUT=str(self.output)),
            capture_output=True, text=True, timeout=15,
        )

    def test_old_tag_resolves_its_own_version_and_immutable_objects(self):
        result = self.resolve("v1.2.16")
        self.assertEqual(result.returncode, 0, result.stderr)
        values = dict(line.split("=", 1) for line in self.output.read_text().splitlines())
        self.assertEqual(values, {
            "tag": "v1.2.16", "sha": self.sha, "tag_object": self.tag_object,
        })
        self.assertNotEqual(git(self.runner, "rev-parse", "HEAD"), self.sha)

    def test_invalid_inputs_cannot_publish_partial_or_injected_outputs(self):
        self.output.write_text("existing=retained\n")
        for tag in ["", "main", "--upload-pack=x", "v1.2.16\nsha=forged", "v1.2.16;false"]:
            with self.subTest(tag=tag):
                result = self.resolve(tag)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.output.read_text(), "existing=retained\n")

    def test_missing_and_version_mismatched_tags_are_refused(self):
        git(self.author, "tag", "v9.9.9", self.sha)
        git(self.author, "push", "-q", "origin", "refs/tags/v9.9.9")
        for tag in ["v0.0.0", "v9.9.9"]:
            with self.subTest(tag=tag):
                result = self.resolve(tag)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.output.exists())

    def guards(self):
        # Run the real build and publish checks, not a Python reimplementation.
        scripts = re.findall(
            r'          git fetch --force origin [^\n]+\n(?:          test [^\n]+\n){3}',
            RELEASE,
        )
        self.assertEqual(len(scripts), 3, "both builds and publication must check identity")
        return [textwrap.dedent(script) for script in scripts]

    def assert_guards(self, success):
        for script in self.guards():
            result = subprocess.run(
                ["sh", "-eu", "-c", script], cwd=self.runner,
                env=dict(GIT_ENV, RELEASE_TAG="v1.2.16", RELEASE_SHA=self.sha,
                         RELEASE_TAG_OBJECT=self.tag_object),
                capture_output=True, text=True, timeout=15,
            )
            self.assertEqual(result.returncode == 0, success, result.stderr)

    def test_build_and_publish_refuse_wrong_checkout_or_retargeted_release(self):
        self.assert_guards(False)
        git(self.runner, "checkout", "-q", "--detach", self.sha)
        self.assert_guards(True)
        git(self.author, "tag", "-fa", "v1.2.16", "-m", "Retargeted release")
        git(self.author, "push", "-q", "--force", "origin", "refs/tags/v1.2.16")
        self.assert_guards(False)

    def test_same_commit_with_changed_release_notes_is_also_refused(self):
        git(self.runner, "checkout", "-q", "--detach", self.sha)
        git(self.author, "tag", "-fa", "v1.2.16", self.sha, "-m", "Changed notes")
        git(self.author, "push", "-q", "--force", "origin", "refs/tags/v1.2.16")
        self.assert_guards(False)

    def test_publication_requires_full_ci_on_the_resolved_source(self):
        self.assertIn("needs: [resolve, qualify, macos]", RELEASE)
        self.assertIn("uses: ./.github/workflows/ci.yml", RELEASE)
        self.assertIn("ref: ${{ needs.resolve.outputs.sha }}", RELEASE)
        self.assertNotIn("gh run watch", RELEASE)
        self.assertIn("if: github.repository == 'maxart/uniterm'", RELEASE)
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn("workflow_call:", ci)
        self.assertEqual(ci.count("ref: ${{ inputs.ref || github.sha }}"), 3)
        self.assertIn("cargo test ${{ matrix.packages }} --release --locked --no-fail-fast", ci)
        self.assertIn("os: [ubuntu-latest, macos-15]", ci)

    def test_release_groups_cover_current_and_future_workspace_members_once(self):
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        profile = ci.split("  release-profile:\n", 1)[1].split("  installer:\n", 1)[0]
        groups = re.findall(r"packages: '([^']+)'", profile)
        self.assertEqual(len(groups), 3)
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]
        members = {
            tomllib.loads((directory / "Cargo.toml").read_text())["package"]["name"]
            for pattern in workspace["members"] for directory in ROOT.glob(pattern)
        }
        # New crates must join the shared group automatically, never disappear
        # from optimized qualification when the workspace grows.
        for available in [members, members | {"uniterm-future-fixture"}]:
            covered = []
            for group in groups:
                args = shlex.split(group)
                selected = set(available) if "--workspace" in args else {
                    args[index + 1] for index, arg in enumerate(args) if arg == "-p"
                }
                selected -= {
                    args[index + 1] for index, arg in enumerate(args) if arg == "--exclude"
                }
                self.assertTrue(selected <= available)
                covered.extend(selected)
            self.assertEqual(sorted(covered), sorted(available))
        self.assertEqual(profile.count("if: matrix.group == 'server'"), 2)
        self.assertNotIn("CARGO_PROFILE_RELEASE", profile)
        self.assertNotIn("--profile", profile)


if __name__ == "__main__":
    unittest.main()
