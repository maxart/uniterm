#!/bin/sh
# Reclaim unused hosted-runner SDKs before the full Rust test/link workload.
# Never run against a developer machine or remove Rust/Android build inputs.
set -eu

if [ "${GITHUB_ACTIONS:-}" != true ] || [ "${RUNNER_OS:-}" != Linux ]; then
    echo "prepare-ci-disk: requires a disposable GitHub Actions Linux runner" >&2
    exit 2
fi

df -h .
sudo rm -rf -- /usr/share/dotnet /usr/share/swift /usr/local/.ghcup \
    /opt/ghc /opt/hostedtoolcache/CodeQL /usr/local/share/boost
df -h .
available_kib=$(df -Pk . | awk 'NR == 2 { print $4 }')
case "$available_kib" in
    ''|*[!0-9]*) echo "prepare-ci-disk: could not read free disk space" >&2; exit 1 ;;
esac
if [ "$available_kib" -lt 26214400 ]; then
    echo "prepare-ci-disk: at least 25 GiB free is required for full test linking" >&2
    exit 1
fi
