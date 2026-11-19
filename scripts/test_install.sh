#!/bin/sh
# Exercise install.sh with a stub Cargo, without network or real builds.
# Usage: sh scripts/test_install.sh [SHELL]   (SHELL runs install.sh; default sh)
set -eu

shell=${1:-sh}
if ! shell_path=$(command -v "$shell"); then
    echo "No $shell on PATH." >&2
    exit 1
fi
repo=$(CDPATH= cd -P "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/justless-install-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
# install.sh compares $PWD with $HOME, so use the physical path, as cd -P would.
work=$(CDPATH= cd -P "$work" && pwd)
git_install="install --locked --root ROOT --git https://github.com/rishadbaniya/justless.git"
building="Building Justless from source with Cargo. The first build takes a few minutes."
failures=0

# A stub cargo records its arguments and, instead of building, installs a fake
# justless under --root that records its own arguments.
mkdir "$work/stub" "$work/empty"
cat > "$work/stub/cargo" <<'EOF'
#!/bin/sh
echo "$*" >> "$CARGO_LOG"
echo "${CARGO_NET_GIT_FETCH_WITH_CLI-unset}" >> "$CARGO_LOG.cli"
root=
while [ "$#" -gt 0 ]; do
    if [ "$1" = --root ]; then root=$2; fi
    shift
done
mkdir -p "$root/bin"
printf '#!/bin/sh\necho "$*" >> "$FAKE_LOG"\n' > "$root/bin/justless"
chmod 755 "$root/bin/justless"
EOF
chmod 755 "$work/stub/cargo"

# run CASE VERSION ARGS...: pipe install.sh into the shell, as curl | sh does.
# run_path and run_home override PATH and HOME for one case.
run() {
    case_dir="$work/$1"
    mkdir -p "$case_dir/project"
    version=$2
    shift 2
    status=0
    (
        cd "$case_dir/project"
        env PATH="${run_path:-$work/stub:$PATH}" HOME="${run_home:-$HOME}" CARGO_INSTALL_ROOT="$case_dir/root" \
            CARGO_LOG="$case_dir/cargo.log" FAKE_LOG="$case_dir/fake.log" JUSTLESS_VERSION="$version" \
            "$shell_path" -s -- "$@" < "$repo/install.sh"
    ) > "$case_dir/out" 2>&1 || status=$?
}

check() {
    if ! eval "$2"; then
        echo "FAIL $1: $2" >&2
        sed 's/^/    /' "$work/$1/out" >&2
        failures=$((failures + 1))
    fi
}

# cargo_ran CASE ARGS: Cargo ran exactly once, with ARGS, where ROOT is the case's install root.
cargo_ran() {
    expected=$(printf '%s\n' "$2" | sed "s|ROOT|$work/$1/root|")
    [ "$(cat "$work/$1/cargo.log")" = "$expected" ]
}

# Outside a checkout, Cargo builds the main branch from GitHub.
run git "" --cli-only
check git '[ "$status" -eq 0 ]'
check git 'cargo_ran git "$git_install justless"'
# A git fetch goes through the git command, so url.insteadOf rewrites to SSH work.
check git '[ "$(cat "$work/git/cargo.log.cli")" = true ]'
check git 'grep -qxF "$building" "$work/git/out"'
check git '[ -x "$work/git/root/bin/justless" ] && [ ! -e "$work/git/fake.log" ]'
check git 'grep -qF "Justless CLI installed." "$work/git/out"'

run pinned v1.2.3 --cli-only
check pinned '[ "$status" -eq 0 ] && cargo_ran pinned "$git_install --tag v1.2.3 justless"'

# --from-source used to skip a prebuilt download; every install builds now, so it is accepted.
run from-source "" --from-source --cli-only
check from-source '[ "$status" -eq 0 ] && cargo_ran from-source "$git_install justless"'

run clients "" claude agy
check clients '[ "$status" -eq 0 ]'
check clients 'grep -qx "init --hooks --agent claude --agent agy" "$work/clients/fake.log"'
check clients 'grep -qF "Project hooks configured for claude agy." "$work/clients/out"'

# From the home directory, install the CLI but configure no hooks.
run_home="$work/home/project"
run home "" claude
unset run_home
check home '[ "$status" -eq 0 ] && [ -x "$work/home/root/bin/justless" ] && [ ! -e "$work/home/fake.log" ]'
check home 'grep -qF "Run this again inside a project folder" "$work/home/out"'

# Without Cargo, name what to install and stop before building anything.
run_path="$work/empty"
run no-cargo "" claude
unset run_path
check no-cargo '[ "$status" -eq 1 ]'
check no-cargo '[ "$(cat "$work/no-cargo/out")" = "Install Rust 1.91 or newer (https://rustup.rs) and a C compiler, then rerun this installer." ]'
check no-cargo '[ ! -e "$work/no-cargo/root" ] && [ ! -e "$work/no-cargo/cargo.log" ] && [ ! -e "$work/no-cargo/fake.log" ]'

# Running the checkout's installer builds that checkout.
mkdir -p "$work/checkout/project"
status=0
(
    cd "$work/checkout/project"
    env PATH="$work/stub:$PATH" CARGO_INSTALL_ROOT="$work/checkout/root" CARGO_LOG="$work/checkout/cargo.log" \
        FAKE_LOG="$work/checkout/fake.log" JUSTLESS_VERSION=1.2.3 "$shell_path" "$repo/install.sh" --cli-only
) > "$work/checkout/out" 2>&1 || status=$?
check checkout '[ "$status" -eq 0 ] && cargo_ran checkout "install --locked --root ROOT --path $repo"'
check checkout 'grep -qxF "$building" "$work/checkout/out"'

run usage "" --unknown
check usage '[ "$status" -eq 2 ] && grep -q "Usage:" "$work/usage/out" && [ ! -e "$work/usage/cargo.log" ]'

if [ "$failures" -ne 0 ]; then
    echo "$failures install.sh check(s) failed with $shell." >&2
    exit 1
fi
echo "install.sh checks passed with $shell."
