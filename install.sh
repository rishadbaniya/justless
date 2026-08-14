#!/bin/sh
set -eu

repo=rishadbaniya/justless
tmp=

usage() {
    echo "Usage: sh install.sh [--from-source] [codex|claude|agy|opencode|cursor|gemini|copilot ... | --cli-only]"
    echo "By default, configure project hooks for supported agents found on PATH."
    echo "Installs the prebuilt release binary when one matches this system, otherwise builds with Cargo."
    echo "--from-source always builds with Cargo. JUSTLESS_VERSION=X.Y.Z selects a release instead of the latest."
}

cleanup() {
    if [ -n "$tmp" ]; then rm -rf "$tmp"; fi
}

# Print the release target for this machine, or fail when no prebuilt binary exists for it.
release_target() {
    os=$(uname -s 2>/dev/null || true)
    arch=$(uname -m 2>/dev/null || true)
    case "$os" in
        Linux) os=unknown-linux-gnu ;;
        Darwin)
            # A shell translated by Rosetta reports x86_64 on Apple silicon.
            if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
                arch=arm64
            fi
            os=apple-darwin
            ;;
        *) return 1 ;;
    esac
    case "$arch" in
        x86_64|amd64) arch=x86_64 ;;
        aarch64|arm64) arch=aarch64 ;;
        *) return 1 ;;
    esac
    echo "$arch-$os"
}

download() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --retry 2 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        return 1
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        openssl dgst -sha256 "$1" | awk '{ print $NF }'
    fi
}

# Install the release binary for this machine. Returns 1 when none is usable,
# so the caller can build from source. Exits when a checksum does not match.
install_prebuilt() {
    if ! target=$(release_target); then
        echo "No prebuilt Justless binary for $(uname -sm)." >&2
        return 1
    fi
    if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
        echo "Downloading a prebuilt binary requires curl or wget." >&2
        return 1
    fi
    if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1 &&
        ! command -v openssl >/dev/null 2>&1; then
        echo "Verifying a prebuilt binary requires sha256sum, shasum or openssl." >&2
        return 1
    fi
    if [ -n "${JUSTLESS_RELEASE_BASE_URL-}" ]; then
        base=${JUSTLESS_RELEASE_BASE_URL%/}
    elif [ -n "$version" ]; then
        base="https://github.com/$repo/releases/download/v$version"
    else
        base="https://github.com/$repo/releases/latest/download"
    fi
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/justless.XXXXXX") || return 1
    trap cleanup EXIT
    trap 'exit 1' HUP INT TERM

    if ! download "$base/SHA256SUMS" "$tmp/SHA256SUMS"; then
        echo "Could not download $base/SHA256SUMS." >&2
        return 1
    fi
    suffix="-$target.tar.gz"
    want=
    if [ -n "$version" ]; then want="justless-$version$suffix"; fi
    line=$(awk -v suffix="$suffix" -v want="$want" '
        { name = $2; sub(/^\*/, "", name) }
        want != "" && name != want { next }
        substr(name, 1, 9) == "justless-" && length(name) > 9 + length(suffix) &&
            substr(name, length(name) - length(suffix) + 1) == suffix { print $1, name; exit }
    ' "$tmp/SHA256SUMS")
    expected=${line%% *}
    asset=${line#* }
    case "$expected" in
        *[!0-9a-f]*|'') expected= ;;
    esac
    case "$asset" in
        *[!A-Za-z0-9._+-]*|'') asset= ;;
    esac
    if [ "${#expected}" -ne 64 ] || [ -z "$asset" ]; then
        echo "The release has no $target binary." >&2
        return 1
    fi
    release=${asset#justless-}
    release=${release%"$suffix"}

    echo "Downloading Justless $release for $target."
    if ! download "$base/$asset" "$tmp/$asset"; then
        echo "Could not download $base/$asset." >&2
        return 1
    fi
    actual=$(sha256 "$tmp/$asset" || true)
    if [ "$actual" != "$expected" ]; then
        echo "Checksum mismatch for $asset: expected $expected, got ${actual:-nothing}." >&2
        echo "Nothing was installed." >&2
        exit 1
    fi

    mkdir "$tmp/unpack"
    if ! tar -xzf "$tmp/$asset" -C "$tmp/unpack"; then
        echo "Could not unpack $asset." >&2
        return 1
    fi
    unpacked="$tmp/unpack/justless-$release-$target/justless"
    if [ ! -f "$unpacked" ] || [ -L "$unpacked" ]; then
        echo "$asset does not contain justless-$release-$target/justless." >&2
        return 1
    fi
    # The archive matched its published checksum. Check that it runs here, for
    # example that the system C library is new enough.
    if ! "$unpacked" --version >/dev/null 2>&1; then
        echo "The prebuilt binary does not run on this system." >&2
        return 1
    fi

    staged="$prefix/bin/.justless.$$"
    if mkdir -p "$prefix/bin" && cp "$unpacked" "$staged" && chmod 755 "$staged" && mv -f "$staged" "$binary"; then
        echo "Installed Justless $release to $binary."
    else
        rm -f "$staged"
        echo "Could not install $binary." >&2
        exit 1
    fi
}

install_from_source() {
    if ! command -v cargo >/dev/null 2>&1; then
        echo "Install Rust 1.91 or newer (https://rustup.rs) and a C compiler, then rerun this installer." >&2
        exit 1
    fi
    if [ -n "$source_dir" ]; then
        cargo install --locked --root "$prefix" --path "$source_dir"
    elif [ -n "$version" ]; then
        cargo install --locked --root "$prefix" --git "https://github.com/$repo.git" --tag "v$version" justless
    else
        cargo install --locked --root "$prefix" --git "https://github.com/$repo.git" justless
    fi
}

main() {
    from_source=
    for arg do
        shift
        case "$arg" in
            --from-source) from_source=1 ;;
            *) set -- "$@" "$arg" ;;
        esac
    done
    case ${1-} in
        -h|--help) usage; return ;;
    esac
    if [ "$#" -eq 0 ]; then
        for agent in codex claude agy gemini copilot; do
            if command -v "$agent" >/dev/null 2>&1; then set -- "$@" "$agent"; fi
        done
        # The Cursor CLI installs as the generic name `agent`, so detect the editor's
        # `cursor` command or the CLI's older `cursor-agent` name instead.
        if command -v cursor >/dev/null 2>&1 || command -v cursor-agent >/dev/null 2>&1; then
            set -- "$@" cursor
        fi
        if command -v opencode >/dev/null 2>&1; then
            case "$(opencode --version 2>/dev/null)" in 1.*) set -- "$@" opencode ;; esac
        fi
    elif [ "$#" -eq 1 ] && [ "$1" = "--cli-only" ]; then
        shift
    fi
    agents=
    for agent do
        case "$agent" in
            codex|claude|agy|opencode|cursor|gemini|copilot) agents="$agents $agent" ;;
            *) usage >&2; return 2 ;;
        esac
    done

    prefix=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-"$HOME/.cargo"}}
    binary="$prefix/bin/justless"
    version=${JUSTLESS_VERSION-}
    version=${version#v}
    source_dir=
    case "$0" in
        */install.sh|install.sh)
            source_dir=$(CDPATH= cd -P "$(dirname "$0")" && pwd)
            ;;
    esac
    if [ -z "$source_dir" ] || [ ! -f "$source_dir/src/agents.rs" ] || [ ! -f "$source_dir/Cargo.toml" ]; then
        source_dir=
    fi
    # A checkout builds its own source, which may differ from any release.
    if [ -n "$source_dir" ] || [ -n "$from_source" ]; then
        install_from_source
    elif ! install_prebuilt; then
        echo "Building from source with Cargo instead." >&2
        install_from_source
    fi
    if [ "$(command -v justless || true)" != "$binary" ]; then
        printf 'For future agent sessions, put %s first on your PATH.\n' "$prefix/bin"
    fi
    # Hooks belong to a project. In the home directory they would land in the user's
    # global agent settings, so ask for a project folder instead.
    if [ -n "$agents" ] && { [ "$PWD" = "$HOME" ] || [ "$PWD" = "/" ]; }; then
        echo "Justless CLI installed. Run this again inside a project folder to turn it on there,"
        echo "or run justless init --hooks --agent NAME from the project."
        return 0
    fi
    if [ -n "$agents" ]; then
        set -- init --hooks
        for agent in $agents; do set -- "$@" --agent "$agent"; done
        "$binary" "$@"
        printf 'Project hooks configured for%s. Start a new agent session in this project.\n' "$agents"
    else
        echo "Justless CLI installed. To add project hooks later, run justless init --hooks --agent NAME."
    fi
}

main "$@"
