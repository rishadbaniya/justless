#!/bin/sh
set -eu

repo=rishadbaniya/justless

usage() {
    echo "Usage: sh install.sh [codex|claude|agy|opencode|cursor|gemini|copilot|windsurf|cline|roo ... | --cli-only]"
    echo "By default, configure project hooks for supported agents found on PATH."
    echo "Builds Justless from source with Cargo. JUSTLESS_VERSION=X.Y.Z builds tag vX.Y.Z instead of main."
}

install_from_source() {
    if ! command -v cargo >/dev/null 2>&1; then
        echo "Install Rust 1.91 or newer (https://rustup.rs) and a C compiler, then rerun this installer." >&2
        exit 1
    fi
    echo "Building Justless from source with Cargo. The first build takes a few minutes."
    # Cargo's built-in Git follows url.insteadOf rewrites (https to ssh, for example)
    # but cannot use the SSH agent or keys the git command can, so fetch with git.
    if [ -z "$source_dir" ] && [ -z "${CARGO_NET_GIT_FETCH_WITH_CLI-}" ] && command -v git >/dev/null 2>&1; then
        CARGO_NET_GIT_FETCH_WITH_CLI=true
        export CARGO_NET_GIT_FETCH_WITH_CLI
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
    # Every install builds from source, so --from-source, which once skipped a
    # prebuilt download, is accepted and ignored.
    for arg do
        shift
        case "$arg" in
            --from-source) ;;
            *) set -- "$@" "$arg" ;;
        esac
    done
    case ${1-} in
        -h|--help) usage; return ;;
    esac
    if [ "$#" -eq 0 ]; then
        for agent in codex claude agy gemini copilot windsurf; do
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
            codex|claude|agy|opencode|cursor|gemini|copilot|windsurf|cline|roo) agents="$agents $agent" ;;
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
    # A checkout builds its own source; otherwise Cargo fetches it from GitHub.
    install_from_source
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
