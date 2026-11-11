#!/usr/bin/env python3
"""Pin an existing release tag before any build, including manual recovery runs."""

import os
import re
import subprocess
import sys
import tomllib


def git(*args):
    """Read authoritative Git data; never interpret the requested tag as shell code."""
    return subprocess.check_output(["git", *args], text=True).strip()


def resolve(tag):
    """Resolve the tag's own Cargo version, not the workflow branch's version."""
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag):
        raise ValueError("release tag must have the form vX.Y.Z")
    ref = f"refs/tags/{tag}"
    git("fetch", "--force", "--no-tags", "origin", f"{ref}:{ref}")
    sha = git("rev-parse", "--verify", f"{ref}^{{commit}}")
    tag_object = git("rev-parse", "--verify", ref)
    manifest = tomllib.loads(git("show", f"{sha}:Cargo.toml"))
    version = manifest["workspace"]["package"]["version"]
    if tag != f"v{version}":
        raise ValueError(f"tag {tag} does not match its Cargo version {version}")
    return {"tag": tag, "sha": sha, "tag_object": tag_object}


def main():
    """Publish outputs only after all identity and version checks have succeeded."""
    try:
        source = resolve(os.environ.get("RELEASE_TAG", ""))
    except (ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f"release source refused: {error}", file=sys.stderr)
        return 1
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        for key, value in source.items():
            output.write(f"{key}={value}\n")
    print(f"Release {source['tag']} pinned to {source['sha']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
