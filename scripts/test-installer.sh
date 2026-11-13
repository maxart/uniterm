#!/bin/sh
set -eu

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
root=$(mktemp -d "${TMPDIR:-/tmp}/uniterm-installer-test.XXXXXX")

cleanup() {
    rm -rf -- "$root"
}
trap cleanup EXIT HUP INT TERM

case "$(uname -s):$(uname -m)" in
    Linux:x86_64 | Linux:amd64) platform=linux-x86_64 ;;
    *)
        echo "installer integration test requires Linux x86-64" >&2
        exit 77
        ;;
esac

release="$root/releases/latest/download"
tools="$root/tools"
sudo_marker="$root/sudo-called"
mkdir -p "$release" "$tools"
printf '#!/bin/sh\necho uniterm-test\n' > "$release/uniterm-$platform"
printf '#!/bin/sh\necho ut-test\n' > "$release/ut-$platform"
chmod 0755 "$release/uniterm-$platform" "$release/ut-$platform"
(
    cd "$release"
    sha256sum "uniterm-$platform" "ut-$platform" > SHA256SUMS
)

# Never discover or overwrite the developer's real ut through inherited PATH.
for tool in sh uname ldd grep dirname mktemp rm mkdir curl awk basename sha256sum cat; do
    ln -s "$(command -v "$tool")" "$tools/$tool"
done
cat > "$tools/install" <<'EOF'
#!/bin/sh
set -eu
case "$4" in
    "$UNITERM_TEST_ROOT"/*) exec "$UNITERM_TEST_INSTALL" "$@" ;;
esac
echo "refusing installation outside the test directory" >&2
exit 99
EOF
cat > "$tools/sudo" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$UNITERM_TEST_SUDO_MARKER"
# Simulate elevation only inside this test's disposable directory.
case "$1:$2:$3" in
    mkdir:-p:"$UNITERM_TEST_ROOT"/*)
        /bin/chmod u+w "$3"
        exec "$@"
        ;;
    install:-m:0755)
        case "$5" in
            "$UNITERM_TEST_ROOT"/*) exec "$@" ;;
        esac
        ;;
esac
echo "refusing unexpected sudo operation" >&2
exit 99
EOF
cat > "$tools/id" <<'EOF'
#!/bin/sh
echo 1000
EOF
chmod 0755 "$tools/sudo" "$tools/id" "$tools/install"

run_installer() {
    env TERMUX_VERSION="${test_termux:-}" PREFIX="${test_prefix:-}" \
        CARGO_HOME="${test_cargo_home:-}" HOME="$test_home" PATH="$test_path" \
        UNITERM_DOWNLOAD_ROOT="file://$root/releases" \
        UNITERM_INSTALL_DIR="$destination" \
        UNITERM_INSTALLER_TESTING=1 \
        UNITERM_TEST_ROOT="$root" \
        UNITERM_TEST_INSTALL="$(command -v install)" \
        UNITERM_TEST_SUDO_MARKER="$sudo_marker" \
        sh "$repo/install.sh" "$@" > "$root/output" 2>&1
}

assert_installed() {
    [ "$("$1/uniterm")" = uniterm-test ]
    [ "$("$1/ut")" = ut-test ]
}

new_case() {
    test_home="$root/$1/home"
    test_path="$tools"
    destination=
    test_termux=
    test_prefix=
    test_cargo_home=
    mkdir -p "$test_home"
    rm -f "$sudo_marker"
}

# Fresh installs stay user-owned even when sudo exists.
new_case fresh
run_installer
assert_installed "$test_home/.local/bin"
[ ! -e "$sudo_marker" ]
grep -F "Add $test_home/.local/bin to PATH" "$root/output"

# A directory override wins, including paths with spaces and --system.
new_case override
destination="$test_home/custom bin"
run_installer
assert_installed "$destination"
[ ! -e "$sudo_marker" ]
run_installer --system
assert_installed "$destination"
[ ! -e "$sudo_marker" ]

# Reuse either installed command; ut takes priority if their locations differ.
new_case existing
mkdir -p "$test_home/cargo bin" "$test_home/old"
cp "$release/ut-$platform" "$test_home/cargo bin/ut"
cp "$release/uniterm-$platform" "$test_home/old/uniterm"
test_path="$test_home/old:$test_home/cargo bin:$tools"
run_installer
assert_installed "$test_home/cargo bin"
[ ! -e "$sudo_marker" ]
grep -F "$test_home/old/uniterm takes precedence" "$root/output"
[ ! -e "$test_home/.local/bin" ]
destination="$test_home/explicit"
run_installer
assert_installed "$destination"
grep -F "$test_home/cargo bin/ut takes precedence" "$root/output"

new_case long-name
mkdir -p "$test_home/bin"
cp "$release/uniterm-$platform" "$test_home/bin/uniterm"
test_path="$test_home/bin:$tools"
run_installer
assert_installed "$test_home/bin"

# User installations are recognized even before PATH is configured.
new_case off-path
mkdir -p "$test_home/.cargo/bin"
cp "$release/ut-$platform" "$test_home/.cargo/bin/ut"
run_installer
assert_installed "$test_home/.cargo/bin"

new_case cargo-home
test_cargo_home="$test_home/custom cargo"
mkdir -p "$test_cargo_home/bin"
cp "$release/uniterm-$platform" "$test_cargo_home/bin/uniterm"
run_installer
assert_installed "$test_cargo_home/bin"

# Relative PATH entries and symlinked directories resolve to the same location.
new_case relative
mkdir -p "$test_home/bin"
cp "$release/ut-$platform" "$test_home/bin/ut"
ln -s "$test_home/bin" "$test_home/link"
test_path="./link:$tools"
(cd "$test_home" && run_installer)
assert_installed "$test_home/bin"
if grep -Eq 'takes precedence|Add .* to PATH' "$root/output"; then
    echo "installer failed to recognize a relative, symlinked PATH directory" >&2
    exit 1
fi

# Replacing a symlink must not overwrite a package manager's target binary.
new_case symlink
mkdir -p "$test_home/bin"
printf '#!/bin/sh\necho old-package\n' > "$test_home/package"
chmod 0755 "$test_home/package"
ln -s "$test_home/package" "$test_home/bin/ut"
test_path="$test_home/bin:$tools"
run_installer
assert_installed "$test_home/bin"
[ "$("$test_home/package")" = old-package ]

new_case termux
test_termux=testing
test_prefix="$test_home/prefix"
run_installer
assert_installed "$test_home/prefix/bin"
[ ! -e "$sudo_marker" ]

# The system default can be inspected without writing to /usr/local/bin.
sed '$d' "$repo/install.sh" > "$root/functions.sh"
[ "$(HOME="$test_home" PATH="$tools" UNITERM_INSTALL_DIR='' sh -c \
    '. "$1"; SYSTEM_INSTALL=1; select_install_dir' sh "$root/functions.sh")" = /usr/local/bin ]

# An existing protected install requires explicit elevation, never a silent
# fallback to a second copy. Root can write mode-0555 directories, so skip
# just this permission simulation when the test harness itself runs as root.
if [ "$(id -u)" -ne 0 ]; then
    new_case protected
    mkdir -p "$test_home/protected"
    cp "$release/ut-$platform" "$test_home/protected/ut"
    test_path="$test_home/protected:$tools"
    chmod 0555 "$test_home/protected"
    if run_installer; then
        echo "installer accepted a protected destination without --system" >&2
        exit 1
    fi
    [ ! -e "$sudo_marker" ]
    grep -F 'Rerun with --system' "$root/output"
    [ ! -e "$test_home/.local/bin" ]
    destination="$test_home/protected"
    run_installer --system
    assert_installed "$destination"
    [ -s "$sudo_marker" ]
fi

# Unknown options and help do not download or install anything.
new_case options
run_installer --help
grep -F 'Usage:' "$root/output"
if run_installer --bogus; then
    echo "installer accepted an unknown option" >&2
    exit 1
fi
[ ! -e "$test_home/.local/bin" ]

# Checksum failure must leave both existing binaries untouched, without sudo.
new_case checksum
destination="$test_home/bin"
run_installer
printf '0  uniterm-%s\n0  ut-%s\n' "$platform" "$platform" > "$release/SHA256SUMS"
if run_installer; then
    echo "installer accepted an invalid checksum" >&2
    exit 1
fi
assert_installed "$destination"
[ ! -e "$sudo_marker" ]
if [ "$(id -u)" -ne 0 ]; then
    chmod 0555 "$destination"
    if run_installer --system; then
        echo "system installer accepted an invalid checksum" >&2
        exit 1
    fi
    [ ! -e "$sudo_marker" ]
    assert_installed "$destination"
    chmod 0755 "$destination"
fi
echo "installer integration tests passed"
