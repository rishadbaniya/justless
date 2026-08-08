#!/bin/sh
# Exercise install.sh against local release directories, without network or Cargo builds.
# Usage: sh scripts/test_install.sh [SHELL]   (SHELL runs install.sh; default sh)
set -eu

shell=${1:-sh}
repo=$(CDPATH= cd -P "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/justless-install-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
targets="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin"
failures=0

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}

# A stub cargo records its arguments instead of building.
mkdir "$work/stub"
printf '#!/bin/sh\necho "$*" >> "$CARGO_LOG"\n' > "$work/stub/cargo"
chmod 755 "$work/stub/cargo"

# make_release DIR VERSION EXIT: publish a fake binary for every Unix target.
# The binary prints its version and exits with EXIT, and logs other invocations.
make_release() {
    mkdir -p "$1" "$work/pkg"
    for target in $targets; do
        name="justless-$2-$target"
        mkdir -p "$work/pkg/$name"
        printf '#!/bin/sh\nif [ "$1" = --version ]; then echo "justless %s"; exit %s; fi\necho "$*" >> "$FAKE_LOG"\n' \
            "$2" "$3" > "$work/pkg/$name/justless"
        chmod 755 "$work/pkg/$name/justless"
        tar -C "$work/pkg" -czf "$1/$name.tar.gz" "$name"
    done
    (cd "$1" && sha256 -- *.tar.gz > SHA256SUMS)
}

# run CASE BASE_URL VERSION ARGS...: pipe install.sh into the shell, as curl | sh does.
run() {
    case_dir="$work/$1"
    mkdir -p "$case_dir/project"
    base=$2
    version=$3
    shift 3
    status=0
    (
        cd "$case_dir/project"
        env PATH="$work/stub:$PATH" CARGO_INSTALL_ROOT="$case_dir/root" CARGO_LOG="$case_dir/cargo.log" \
            FAKE_LOG="$case_dir/fake.log" JUSTLESS_RELEASE_BASE_URL="$base" JUSTLESS_VERSION="$version" \
            "$shell" -s -- "$@" < "$repo/install.sh"
    ) > "$case_dir/out" 2>&1 || status=$?
}

check() {
    if ! eval "$2"; then
        echo "FAIL $1: $2" >&2
        sed 's/^/    /' "$work/$1/out" >&2
        failures=$((failures + 1))
    fi
}

make_release "$work/good" 9.9.9 0

run prebuilt "file://$work/good" "" --cli-only
check prebuilt '[ "$status" -eq 0 ]'
check prebuilt '[ "$("$work/prebuilt/root/bin/justless" --version)" = "justless 9.9.9" ]'
check prebuilt '[ ! -e "$work/prebuilt/cargo.log" ]'
check prebuilt 'grep -q "Installed Justless 9.9.9" "$work/prebuilt/out"'

run pinned "file://$work/good/" v9.9.9 --cli-only
check pinned '[ "$status" -eq 0 ] && [ -x "$work/pinned/root/bin/justless" ]'

run clients "file://$work/good" "" claude agy
check clients '[ "$status" -eq 0 ]'
check clients 'grep -qx "init --hooks --agent claude --agent agy" "$work/clients/fake.log"'

# A tampered archive must abort without installing or building anything.
cp -R "$work/good" "$work/tampered"
for archive in "$work"/tampered/*.tar.gz; do printf 'x' >> "$archive"; done
run tampered "file://$work/tampered" "" --cli-only
check tampered '[ "$status" -ne 0 ]'
check tampered '[ ! -e "$work/tampered/root/bin/justless" ] && [ ! -e "$work/tampered/cargo.log" ]'
check tampered 'grep -q "Checksum mismatch" "$work/tampered/out"'

# Missing releases, missing targets and unusable binaries fall back to Cargo.
run missing "file://$work/nothing-here" "" --cli-only
check missing '[ "$status" -eq 0 ]'
check missing 'grep -qx "install --locked --root $work/missing/root --git https://github.com/rishadbaniya/justless.git justless" "$work/missing/cargo.log"'

mkdir "$work/windows-only"
echo "$(printf '%064d' 0)  justless-9.9.9-x86_64-pc-windows-msvc.zip" > "$work/windows-only/SHA256SUMS"
run other-target "file://$work/windows-only" "" --cli-only
check other-target '[ "$status" -eq 0 ] && grep -q -- "--git" "$work/other-target/cargo.log"'

run other-version "file://$work/good" 1.2.3 --cli-only
check other-version 'grep -q -- "--tag v1.2.3 justless" "$work/other-version/cargo.log"'

make_release "$work/broken" 9.9.9 1
run unusable "file://$work/broken" "" --cli-only
check unusable '[ "$status" -eq 0 ] && grep -q -- "--git" "$work/unusable/cargo.log"'
check unusable '[ ! -e "$work/unusable/root/bin/justless" ]'

run from-source "file://$work/good" "" --from-source --cli-only
check from-source '[ "$status" -eq 0 ] && grep -q -- "--git" "$work/from-source/cargo.log"'
check from-source '[ ! -e "$work/from-source/root/bin/justless" ]'

# Running the checkout's installer builds that checkout.
mkdir -p "$work/checkout/project"
status=0
(
    cd "$work/checkout/project"
    env PATH="$work/stub:$PATH" CARGO_INSTALL_ROOT="$work/checkout/root" CARGO_LOG="$work/checkout/cargo.log" \
        JUSTLESS_RELEASE_BASE_URL="file://$work/good" "$shell" "$repo/install.sh" --cli-only
) > "$work/checkout/out" 2>&1 || status=$?
check checkout '[ "$status" -eq 0 ] && grep -q -- "--path $repo" "$work/checkout/cargo.log"'

run usage "file://$work/good" "" --unknown
check usage '[ "$status" -eq 2 ] && grep -q "Usage:" "$work/usage/out"'

if [ "$failures" -ne 0 ]; then
    echo "$failures install.sh check(s) failed with $shell." >&2
    exit 1
fi
echo "install.sh checks passed with $shell."
