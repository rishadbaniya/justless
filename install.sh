#!/bin/sh
set -eu

usage() {
    echo "Usage: sh install.sh [codex|claude|agy|opencode ... | --cli-only]"
    echo "By default, configure project hooks for supported agents found on PATH."
}

main() {
    case ${1-} in
        -h|--help) usage; return ;;
    esac
    if [ "$#" -eq 0 ]; then
        for agent in codex claude agy; do
            if command -v "$agent" >/dev/null 2>&1; then set -- "$@" "$agent"; fi
        done
        if command -v opencode >/dev/null 2>&1; then
            case "$(opencode --version 2>/dev/null)" in 1.*) set -- "$@" opencode ;; esac
        fi
    elif [ "$#" -eq 1 ] && [ "$1" = "--cli-only" ]; then
        shift
    fi
    agents=
    for agent do
        case "$agent" in
            codex|claude|agy|opencode) agents="$agents $agent" ;;
            *) usage >&2; return 2 ;;
        esac
    done
    if ! command -v cargo >/dev/null 2>&1; then
        echo "Install Rust 1.91 or newer (https://rustup.rs) and a C compiler, then rerun this installer." >&2
        return 1
    fi

    prefix=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-"$HOME/.cargo"}}
    source_dir=
    case "$0" in
        */install.sh|install.sh)
            source_dir=$(CDPATH= cd -P "$(dirname "$0")" && pwd)
            ;;
    esac
    if [ -n "$source_dir" ] && [ -f "$source_dir/src/agents.rs" ] && [ -f "$source_dir/Cargo.toml" ]; then
        cargo install --locked --root "$prefix" --path "$source_dir"
    else
        cargo install --locked --root "$prefix" --git https://github.com/rishadbaniya/justless.git justless
    fi
    binary="$prefix/bin/justless"
    if [ "$(command -v justless || true)" != "$binary" ]; then
        printf 'For future agent sessions, put %s first on your PATH.\n' "$prefix/bin"
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
