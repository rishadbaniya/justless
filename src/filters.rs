//! Command selection and line filters shared by explicit runs and agent hooks.
use crate::wrappers::effective;
use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use regex::Regex;
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Containers,
    ComposeContainers,
    Pods,
    Deployments,
    StatefulSets,
    DaemonSets,
    ReplicaSets,
    Jobs,
    Nodes,
    JsTests,
    Logs,
    Lint,
    Search,
    Listing,
}

impl Profile {
    pub fn label(self) -> &'static str {
        match self {
            Self::Containers => "docker ps",
            Self::ComposeContainers => "docker compose ps",
            Self::Pods => "kubectl get pods",
            Self::Deployments => "kubectl get deployments",
            Self::StatefulSets => "kubectl get statefulsets",
            Self::DaemonSets => "kubectl get daemonsets",
            Self::ReplicaSets => "kubectl get replicasets",
            Self::Jobs => "kubectl get jobs",
            Self::Nodes => "kubectl get nodes",
            Self::JsTests => "JavaScript tests",
            Self::Logs => "container logs",
            Self::Lint => "lint",
            Self::Search => "search",
            Self::Listing => "listing",
        }
    }

    pub fn inventory(self) -> bool {
        !matches!(
            self,
            Self::JsTests | Self::Logs | Self::Lint | Self::Search | Self::Listing
        )
    }
}

// Accept display-oriented options explicitly for resource commands. A new flag or
// custom output format should reach the original command until we understand it.
fn display_options(mut args: &[&str], flags: &[&str], valued: &[&str], names: bool) -> bool {
    while let Some((&arg, rest)) = args.split_first() {
        args = rest;
        let (key, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(k, v)| (k, Some(v)));
        if valued.contains(&key) {
            if inline.is_none() {
                let Some((&v, rest)) = args.split_first() else {
                    return false;
                };
                if v.starts_with('-') {
                    return false;
                }
                args = rest;
            }
        } else if !(flags.contains(&arg) || names && !arg.starts_with('-')) {
            return false;
        }
    }
    true
}

/// Skip package-runner prefixes such as `npx`, `uv run` and `bundle exec` so the
/// tool they launch selects the profile.
pub fn unwrap_tool<'a, 'b>(words: &'a [&'b str]) -> &'a [&'b str] {
    runners(words).0
}

/// The tool behind any package-runner prefixes, and whether a prefix may download
/// the tool or install packages before it runs.
fn runners<'a, 'b>(mut words: &'a [&'b str]) -> (&'a [&'b str], bool) {
    let mut fetches = false;
    loop {
        let (rest, downloads) = match words {
            ["npx" | "bunx" | "uvx" | "pnpx", rest @ ..] => (rest, true),
            ["pnpm" | "yarn" | "bun", "dlx", rest @ ..] => (rest, true),
            // `uv run` and `hatch run` sync the environment first, and the others
            // can be configured to.
            ["uv" | "pdm" | "hatch" | "rye", "run", rest @ ..] => (rest, true),
            // These run a tool the project already installed.
            ["pnpm" | "yarn" | "bun", "exec", rest @ ..]
            | ["poetry" | "pipenv", "run", rest @ ..]
            | ["bundle", "exec", rest @ ..] => (rest, false),
            ["python" | "python3", "-m", tool, ..] if !matches!(*tool, "pytest" | "unittest") => {
                (&words[2..], false)
            }
            _ => return (words, fetches),
        };
        let rest = match rest {
            ["-y" | "--yes" | "--quiet" | "-q", tail @ ..] => tail,
            _ => rest,
        };
        fetches |= downloads;
        // An option such as `uv run --with PKG` stays with its runner.
        if rest.is_empty() || rest[0].starts_with('-') {
            return (words, fetches);
        }
        words = rest;
    }
}

/// A short name for the tool in a lint report, such as `cargo clippy` or `ruff check`.
pub fn tool_label(args: &[OsString]) -> String {
    let words: Vec<_> = args.iter().map(|a| a.to_str().unwrap_or("")).collect();
    tool_label_words(&words)
}

pub fn tool_label_words(words: &[&str]) -> String {
    let owned = effective(words);
    let refs: Vec<&str> = match &owned {
        Some(words) => words.iter().map(String::as_str).collect(),
        None => words.to_vec(),
    };
    let words = unwrap_tool(&refs);
    let Some((program, rest)) = words.split_first() else {
        return "other".into();
    };
    let program = Path::new(program)
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or(program);
    // A search or listing argument is a pattern or path, never a subcommand.
    if matches!(
        program,
        "rg" | "grep"
            | "egrep"
            | "fgrep"
            | "ag"
            | "ack"
            | "ack-grep"
            | "find"
            | "ls"
            | "tree"
            | "fd"
            | "fdfind"
    ) {
        return program.to_owned();
    }
    match rest.first() {
        Some(sub)
            if sub.starts_with(|c: char| c.is_ascii_lowercase())
                && sub.chars().all(|c| c.is_ascii_lowercase() || c == '-') =>
        {
            format!("{program} {sub}")
        }
        _ => program.to_owned(),
    }
}

fn lint_display(args: &[&str]) -> bool {
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        let (flag, value) = arg
            .split_once('=')
            .map_or((*arg, None), |(f, v)| (f, Some(v)));
        match flag {
            // ShellCheck's gcc format is one of the recognized shapes.
            "-f" | "--format" if value.or_else(|| args.peek().copied().copied()) == Some("gcc") => {
                if value.is_none() {
                    args.next();
                }
            }
            "-f" | "--format" | "--output-format" | "--formatter" | "--reporter" | "--out"
            | "--output" | "-o" | "--json" | "--watch" | "-w" | "--serve" | "--statistics"
            | "--show-source" | "--pretty" | "--help" | "--version" => return false,
            _ => {}
        }
    }
    true
}

fn lint_tool(program: &str, args: &[&str]) -> bool {
    match (program, args) {
        ("cargo", ["clippy" | "check" | "build", ..]) => true,
        ("cargo", ["doc", rest @ ..]) => !rest.contains(&"--open"),
        ("ruff", ["check", ..])
        | ("biome", ["check" | "lint" | "ci", ..])
        | ("ty", ["check", ..])
        | ("golangci-lint", ["run", ..])
        | ("go", ["vet" | "build", ..])
        | ("dotnet", ["build" | "restore", ..])
        | ("swift", ["build", ..])
        | (
            "flake8" | "pylint" | "mypy" | "pyright" | "basedpyright" | "eslint" | "oxlint" | "tsc"
            | "vue-tsc" | "shellcheck" | "rubocop" | "staticcheck" | "swiftlint" | "hadolint",
            _,
        ) => true,
        // Bare `make` keeps no profile so adaptive budgets still treat `make` without
        // a target label (such as `make -j8 lint`) as a generic runner.
        ("make", []) => false,
        ("make" | "gmake", _) => make_build_args(args),
        ("ninja", _) => ninja_build_args(args),
        ("cmake", ["--build", rest @ ..]) => cmake_build_args(rest),
        ("gradle" | "gradlew" | "./gradlew", _) => gradle_build_args(args),
        ("mvn" | "mvnw" | "./mvnw", _) => mvn_build_args(args),
        _ => false,
    }
}

/// Whether a build target name runs tests or another non-compile action rather
/// than compiling code.
fn is_non_build_target(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "test"
            | "tests"
            | "check"
            | "checks"
            | "ctest"
            | "bench"
            | "benchmark"
            | "benchmarks"
            | "install"
            | "uninstall"
            | "deploy"
            | "publish"
            | "serve"
            | "server"
            | "run"
            | "dev"
            | "start"
            | "stop"
            | "restart"
            | "watch"
            | "fmt"
            | "format"
            | "help"
            | "version"
            | "info"
            | "list"
            | "doc"
            | "docs"
            | "man"
            | "distclean"
            | "mrproper"
            | "clobber"
            | "migrate"
            | "seed"
            | "shell"
            | "repl"
            | "console"
            | "experimental"
            | "nightly"
            | "continuous"
    ) {
        return true;
    }
    lower.split(['-', '_', '/', ':', '.']).any(|part| {
        matches!(
            part,
            "test"
                | "tests"
                | "unittest"
                | "unittests"
                | "ctest"
                | "e2e"
                | "spec"
                | "specs"
                | "bench"
                | "benchmark"
                | "benchmarks"
        )
    })
}

fn make_build_args(args: &[&str]) -> bool {
    let number = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let mut targets = Vec::new();
    let mut iter = args.iter().copied().peekable();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            targets.extend(iter.filter(|a| !a.contains('=')));
            break;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        if matches!(
            flag,
            "-n" | "--just-print"
                | "--dry-run"
                | "--recon"
                | "-q"
                | "--question"
                | "-p"
                | "--print-data-base"
                | "-v"
                | "--version"
                | "-h"
                | "--help"
        ) {
            return false;
        }
        if matches!(
            flag,
            "-j" | "--jobs" | "-l" | "--load-average" | "--max-load"
        ) {
            if inline.is_none() && iter.peek().is_some_and(|next| number(next)) {
                iter.next();
            }
            continue;
        }
        if matches!(
            flag,
            "-C" | "--directory"
                | "-f"
                | "--file"
                | "--makefile"
                | "-I"
                | "--include-dir"
                | "-o"
                | "--old-file"
                | "--assume-old"
                | "-W"
                | "--what-if"
                | "--new-file"
                | "--assume-new"
                | "--eval"
                | "-O"
                | "--output-sync"
        ) {
            if inline.is_none() {
                iter.next();
            }
            continue;
        }
        if arg.starts_with('-') || arg.contains('=') {
            continue;
        }
        targets.push(arg);
    }
    if targets.is_empty() {
        return true;
    }
    let has_non_clean = targets.iter().any(|t| !t.eq_ignore_ascii_case("clean"));
    has_non_clean && targets.iter().all(|t| !is_non_build_target(t))
}

fn ninja_build_args(args: &[&str]) -> bool {
    let mut targets = Vec::new();
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            targets.extend(iter);
            break;
        }
        if matches!(arg, "-n" | "-h" | "--help" | "--version") || arg.starts_with("-t") {
            return false;
        }
        if matches!(arg, "-C" | "-f" | "-j" | "-k" | "-l" | "-d" | "-w") {
            iter.next();
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        targets.push(arg);
    }
    if targets.is_empty() {
        return true;
    }
    let has_non_clean = targets.iter().any(|t| !t.eq_ignore_ascii_case("clean"));
    has_non_clean && targets.iter().all(|t| !is_non_build_target(t))
}

fn cmake_build_args(rest: &[&str]) -> bool {
    let Some((&dir, tail)) = rest.split_first() else {
        return false;
    };
    if dir.starts_with('-') {
        return false;
    }
    let mut targets = Vec::new();
    let mut iter = tail.iter().copied().peekable();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            let native: Vec<_> = iter.collect();
            return native
                .iter()
                .filter(|a| !a.starts_with('-') && !a.contains('='))
                .all(|t| !t.eq_ignore_ascii_case("clean") && !is_non_build_target(t));
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        if matches!(flag, "--target" | "-t") {
            if let Some(value) = inline {
                targets.push(value);
            } else {
                while let Some(next) = iter.next_if(|a| !a.starts_with('-')) {
                    targets.push(next);
                }
            }
            continue;
        }
        if matches!(flag, "--config" | "-j" | "--parallel") && inline.is_none() {
            iter.next();
        }
    }
    if targets.is_empty() {
        return true;
    }
    let has_non_clean = targets.iter().any(|t| !t.eq_ignore_ascii_case("clean"));
    has_non_clean && targets.iter().all(|t| !is_non_build_target(t))
}

fn gradle_build_args(args: &[&str]) -> bool {
    const VALUED: &[&str] = &[
        "-x",
        "--exclude-task",
        "--console",
        "--warning-mode",
        "--max-workers",
        "-b",
        "--build-file",
        "-c",
        "--settings-file",
        "-p",
        "--project-dir",
        "-I",
        "--init-script",
        "-g",
        "--gradle-user-home",
        "-P",
        "--project-prop",
        "-D",
        "--system-prop",
        "--include-build",
        "--priority",
        "--configuration-cache-problems",
        "--dependency-verification",
        "--write-verification-metadata",
    ];
    let mut has_build = false;
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        if matches!(
            flag,
            "--tests"
                | "--continuous"
                | "-t"
                | "--help"
                | "-h"
                | "--version"
                | "-v"
                | "--dry-run"
                | "-m"
        ) {
            return false;
        }
        if VALUED.contains(&flag) {
            if inline.is_none() {
                iter.next();
            }
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        let task = arg.rsplit(':').next().unwrap_or(arg);
        if task == "clean" {
            continue;
        }
        let is_build = matches!(
            task,
            "build"
                | "assemble"
                | "classes"
                | "testClasses"
                | "jar"
                | "war"
                | "ear"
                | "bootJar"
                | "bootWar"
                | "shadowJar"
        ) || task.starts_with("compile")
            || task.starts_with("assemble");
        if !is_build {
            return false;
        }
        has_build = true;
    }
    has_build
}

fn mvn_build_args(args: &[&str]) -> bool {
    const VALUED: &[&str] = &[
        "-T",
        "--threads",
        "-pl",
        "--projects",
        "-rf",
        "--resume-from",
        "-f",
        "--file",
        "-s",
        "--settings",
        "-gs",
        "--global-settings",
        "-P",
        "--activate-profiles",
        "-l",
        "--log-file",
        "-t",
        "--toolchains",
        "-gt",
        "--global-toolchains",
        "-D",
        "--define",
    ];
    let mut has_build = false;
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        if matches!(flag, "--help" | "-h" | "--version" | "-v") {
            return false;
        }
        if let Some(prop) = arg.strip_prefix("-D") {
            let key = prop.split_once('=').map_or(prop, |(k, _)| k);
            if matches!(key, "test" | "it.test") {
                return false;
            }
        }
        if VALUED.contains(&flag) {
            if inline.is_none() {
                if matches!(flag, "-D" | "--define")
                    && iter.next().is_some_and(|v| {
                        matches!(v.split_once('=').map_or(v, |(k, _)| k), "test" | "it.test")
                    })
                {
                    return false;
                } else if !matches!(flag, "-D" | "--define") {
                    iter.next();
                }
            }
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        if arg == "clean" {
            continue;
        }
        if !matches!(
            arg,
            "validate"
                | "compile"
                | "test-compile"
                | "package"
                | "generate-sources"
                | "process-sources"
                | "process-resources"
                | "process-classes"
                | "generate-test-sources"
                | "process-test-sources"
                | "process-test-resources"
                | "prepare-package"
        ) {
            return false;
        }
        has_build = true;
    }
    has_build
}

pub fn profile(words: &[&str]) -> Option<Profile> {
    let words = unwrap_tool(words);
    if let Some(profile) = crate::search::select(words) {
        return Some(profile);
    }
    let (&program, mut args) = words.split_first()?;
    if lint_tool(program, args) {
        return lint_display(args).then_some(Profile::Lint);
    }
    if program == "kubectl" {
        // kubectl permits a namespace or context before or after the verb.
        while let Some((&arg, rest)) = args.split_first() {
            if ["--namespace", "-n", "--context"].contains(&arg)
                && rest.first().is_some_and(|v| !v.starts_with('-'))
            {
                args = &rest[1..];
            } else if arg.starts_with("--namespace=") || arg.starts_with("--context=") {
                args = rest;
            } else {
                break;
            }
        }
    }
    match (program, args) {
        (
            "docker" | "podman",
            ["ps", tail @ ..] | ["container", "ls" | "list" | "ps", tail @ ..],
        ) if display_options(
            tail,
            &["-a", "--all", "--no-trunc", "--latest", "-l"],
            &["-f", "--filter", "-n", "--last"],
            false,
        ) =>
        {
            Some(Profile::Containers)
        }
        ("docker", ["compose", "ps", tail @ ..])
            if display_options(
                tail,
                &["-a", "--all", "--no-trunc", "--orphans"],
                &["--status", "--filter"],
                true,
            ) =>
        {
            Some(Profile::ComposeContainers)
        }
        ("kubectl", ["get", resource, tail @ ..])
            if display_options(
                tail,
                &["-A", "--all-namespaces"],
                &[
                    "-n",
                    "--namespace",
                    "--context",
                    "-l",
                    "--selector",
                    "--field-selector",
                    "--sort-by",
                ],
                true,
            ) =>
        {
            match *resource {
                "po" | "pod" | "pods" => Some(Profile::Pods),
                "deploy" | "deployment" | "deployments" | "deployments.apps" => {
                    Some(Profile::Deployments)
                }
                "sts" | "statefulset" | "statefulsets" | "statefulsets.apps" => {
                    Some(Profile::StatefulSets)
                }
                "ds" | "daemonset" | "daemonsets" | "daemonsets.apps" => Some(Profile::DaemonSets),
                "rs" | "replicaset" | "replicasets" | "replicasets.apps" => {
                    Some(Profile::ReplicaSets)
                }
                "job" | "jobs" | "jobs.batch" => Some(Profile::Jobs),
                "no" | "node" | "nodes" => Some(Profile::Nodes),
                _ => None,
            }
        }
        (
            "docker" | "podman",
            ["logs", tail @ ..] | ["container", "logs", tail @ ..] | ["compose", "logs", tail @ ..],
        ) if display_options(
            tail,
            &[
                "-t",
                "--timestamps",
                "--details",
                "--no-color",
                "--no-log-prefix",
            ],
            &["--since", "--until", "--tail", "-n"],
            true,
        ) =>
        {
            Some(Profile::Logs)
        }
        ("kubectl" | "oc", ["logs", tail @ ..])
            if display_options(
                tail,
                &[
                    "-p",
                    "--previous",
                    "--timestamps",
                    "--all-containers",
                    "--prefix",
                ],
                &[
                    "--since",
                    "--since-time",
                    "--tail",
                    "-c",
                    "--container",
                    "-n",
                    "--namespace",
                    "--context",
                    "-l",
                    "--selector",
                ],
                true,
            ) =>
        {
            Some(Profile::Logs)
        }
        ("pnpm" | "npm" | "yarn" | "bun", ["test", ..] | ["run", "test", ..])
        | ("jest", _)
        | ("vitest", ["run", ..])
        | ("node", ["--test", ..])
            if js_display(args) =>
        {
            Some(Profile::JsTests)
        }
        ("vitest", rest)
            if !matches!(
                rest.first().copied(),
                Some("watch" | "dev" | "bench" | "init" | "list")
            ) && rest
                .iter()
                .take_while(|a| **a != "--")
                .any(|a| *a == "--run")
                && js_display(args) =>
        {
            Some(Profile::JsTests)
        }
        _ => None,
    }
}

fn js_display(args: &[&str]) -> bool {
    !args.iter().any(|a| {
        let flag = a.split('=').next().unwrap_or(a);
        matches!(
            flag,
            "--watch"
                | "--watchAll"
                | "--watch-all"
                | "-w"
                | "--ui"
                | "--interactive"
                | "--inspect"
                | "--inspect-brk"
                | "--debug"
                | "--json"
                | "--reporter"
                | "--reporters"
                | "--test-reporter"
                | "--test-reporter-destination"
                | "--outputFile"
                | "--output-file"
                | "--listTests"
                | "--showConfig"
                | "--updateSnapshot"
                | "-u"
                | "--coverage"
                | "--help"
                | "--version"
        )
    })
}

/// Whether a command only reads and prints JSON for the agent to look at: `gh`
/// list and view commands with `--json`, `gh api` GET requests, `kubectl get -o
/// json`, `docker ps`, `images` and `inspect` in JSON, `cargo metadata` and `npm ls
/// --json`. Its output is shown as JSON tables or compact JSON (see `jsonview`)
/// when that is smaller. Every option must be one these tools document for such a
/// read, so a request that writes, waits, filters the JSON or names a file
/// elsewhere keeps its own form.
pub fn json_output(words: &[&str]) -> bool {
    let words = unwrap_tool(words);
    let Some((&program, args)) = words.split_first() else {
        return false;
    };
    match (program, args) {
        ("gh", ["api", rest @ ..]) => gh_api(rest),
        ("gh", ["search", kind, rest @ ..]) if !kind.starts_with('-') => gh_json(rest),
        (
            "gh",
            [
                "pr" | "issue" | "repo" | "run" | "release" | "workflow" | "label" | "cache"
                | "secret" | "variable" | "codespace",
                "list" | "ls" | "view" | "status" | "checks",
                rest @ ..,
            ],
        ) => gh_json(rest),
        ("kubectl", _) => kubectl_json(args),
        ("docker", _) => docker_json(args),
        ("cargo", ["metadata", rest @ ..]) => {
            display_options(
                rest,
                &[
                    "--no-deps",
                    "--offline",
                    "--frozen",
                    "--locked",
                    "--all-features",
                    "--no-default-features",
                    "-q",
                    "--quiet",
                ],
                &[
                    "--format-version",
                    "--features",
                    "-F",
                    "--filter-platform",
                    "--manifest-path",
                ],
                false,
            ) && option_values(rest, &["--format-version"]).all(|version| version == "1")
                && option_values(rest, &["--manifest-path"]).all(local_argument)
        }
        ("npm", ["ls" | "list" | "la" | "ll", rest @ ..]) => {
            rest.contains(&"--json")
                && display_options(
                    rest,
                    &[
                        "--json",
                        "--all",
                        "-a",
                        "--long",
                        "-l",
                        "--global",
                        "-g",
                        "--link",
                        "--package-lock-only",
                        "--workspaces",
                        "--include-workspace-root",
                    ],
                    &["--depth", "--omit", "--include", "--workspace", "-w"],
                    true,
                )
        }
        _ => false,
    }
}

/// The arguments that are neither an option nor the value of one in `valued`.
fn operands<'a>(args: &[&'a str], valued: &[&str]) -> Vec<&'a str> {
    let mut operands = Vec::new();
    let mut words = args.iter();
    while let Some(&arg) = words.next() {
        if valued.contains(&arg) {
            words.next();
        } else if !arg.starts_with('-') {
            operands.push(arg);
        }
    }
    operands
}

/// The values given to any of `names`, as `NAME VALUE` or `NAME=VALUE`.
fn option_values<'a>(args: &'a [&'a str], names: &'a [&str]) -> impl Iterator<Item = &'a str> {
    args.iter()
        .enumerate()
        .filter_map(move |(index, arg)| match arg.split_once('=') {
            Some((name, value)) if names.contains(&name) => Some(value),
            None if names.contains(arg) => args.get(index + 1).copied(),
            _ => None,
        })
}

/// `gh api` options for a GET request to a GitHub endpoint path whose response is
/// printed as it came: no fields or input, which make it a POST, no other method,
/// no full URL, and no `--jq` or `--template`, which print something else.
fn gh_api(args: &[&str]) -> bool {
    const VALUED: &[&str] = &[
        "-H",
        "--header",
        "-X",
        "--method",
        "--hostname",
        "--cache",
        "-p",
        "--preview",
    ];
    // A full URL could name another host.
    matches!(operands(args, VALUED).as_slice(),
        [endpoint] if *endpoint != "graphql" && !endpoint.contains("://"))
        && display_options(
            args,
            &["--paginate", "--slurp", "-i", "--include"],
            VALUED,
            true,
        )
        && option_values(args, &["-X", "--method"]).all(|method| method.eq_ignore_ascii_case("GET"))
        && option_values(args, &["-H", "--header"]).all(|header| {
            header.split_once(':').is_some_and(|(name, _)| {
                ["accept", "x-github-api-version"]
                    .contains(&name.trim().to_ascii_lowercase().as_str())
            })
        })
}

/// Options of a `gh` list or view command that prints the `--json` fields as they
/// are, without opening a browser or waiting.
fn gh_json(args: &[&str]) -> bool {
    let flags = || {
        args.iter()
            .map(|arg| arg.split_once('=').map_or(*arg, |(flag, _)| flag))
    };
    flags().any(|flag| flag == "--json")
        && !flags().any(|flag| {
            matches!(
                flag,
                "--web" | "-w" | "--watch" | "--jq" | "-q" | "--template" | "-t"
            )
        })
}

/// `kubectl get` with JSON output and only display options, after any namespace
/// or context given before the verb.
fn kubectl_json(mut args: &[&str]) -> bool {
    while let Some((&arg, rest)) = args.split_first() {
        if ["--namespace", "-n", "--context"].contains(&arg)
            && rest.first().is_some_and(|v| !v.starts_with('-'))
        {
            args = &rest[1..];
        } else if arg.starts_with("--namespace=") || arg.starts_with("--context=") {
            args = rest;
        } else {
            break;
        }
    }
    let ["get", rest @ ..] = args else {
        return false;
    };
    let mut json = false;
    let mut others = Vec::with_capacity(rest.len());
    let mut words = rest.iter();
    while let Some(&arg) = words.next() {
        let value = match arg {
            "-o" | "--output" => words.next().copied(),
            _ => arg
                .strip_prefix("--output=")
                .or_else(|| arg.strip_prefix("-o="))
                .or_else(|| arg.strip_prefix("-o")),
        };
        match value {
            Some("json") => json = true,
            Some(_) => return false,
            None => others.push(arg),
        }
    }
    const VALUED: &[&str] = &[
        "-n",
        "--namespace",
        "--context",
        "-l",
        "--selector",
        "--field-selector",
        "--sort-by",
    ];
    json && !operands(&others, VALUED).is_empty()
        && display_options(
            &others,
            &["-A", "--all-namespaces", "--ignore-not-found"],
            VALUED,
            true,
        )
}

/// `docker ps` and `docker images` with `--format json`, and `docker inspect`,
/// which prints JSON unless given another format.
fn docker_json(args: &[&str]) -> bool {
    let json = |args: &[&str], names: &[&str], default: bool| {
        let mut formats = option_values(args, names).peekable();
        match formats.peek() {
            None => default,
            Some(_) => formats.all(|format| matches!(format, "json" | "{{json .}}")),
        }
    };
    match args {
        ["ps", tail @ ..] | ["container", "ls" | "list" | "ps", tail @ ..] => {
            json(tail, &["--format"], false)
                && display_options(
                    tail,
                    &[
                        "-a",
                        "--all",
                        "--no-trunc",
                        "--latest",
                        "-l",
                        "-s",
                        "--size",
                    ],
                    &["-f", "--filter", "-n", "--last", "--format"],
                    false,
                )
        }
        ["images", tail @ ..] | ["image", "ls" | "list", tail @ ..] => {
            json(tail, &["--format"], false)
                && display_options(
                    tail,
                    &["-a", "--all", "--digests", "--no-trunc"],
                    &["-f", "--filter", "--format"],
                    true,
                )
        }
        ["inspect", tail @ ..]
        | [
            "container" | "image" | "volume" | "network",
            "inspect",
            tail @ ..,
        ] => {
            const VALUED: &[&str] = &["-f", "--format", "--type"];
            json(tail, &["-f", "--format"], true)
                && !operands(tail, VALUED).is_empty()
                && display_options(tail, &["-s", "--size"], VALUED, true)
        }
        _ => false,
    }
}

/// Skip Git global options that only disable pagers/locks or control pathspec
/// matching without changing directory, repository or configuration.
pub(crate) fn git_display_subcommand<'a, 'b>(mut args: &'a [&'b str]) -> &'a [&'b str] {
    while let Some((first, tail)) = args.split_first() {
        if matches!(
            *first,
            "--no-pager"
                | "-P"
                | "--no-optional-locks"
                | "--no-replace-objects"
                | "--literal-pathspecs"
                | "--glob-pathspecs"
                | "--noglob-pathspecs"
                | "--icase-pathspecs"
        ) {
            args = tail;
        } else {
            break;
        }
    }
    args
}

pub fn legacy_hook(words: &[&str]) -> bool {
    match unwrap_tool(words) {
        ["cargo", "build" | "check" | "clippy" | "test" | "tree", ..]
        | ["cargo", "nextest", "run", ..]
        | ["make" | "gmake" | "ninja", ..]
        | ["cmake", "--build", ..]
        | ["gradle" | "gradlew" | "./gradlew", ..]
        | ["mvn" | "mvnw" | "./mvnw", ..]
        | ["dotnet", "build" | "test" | "restore", ..]
        | ["swift", "build" | "test", ..]
        | ["go", "test" | "build", ..]
        | ["python" | "python3", "-m", "pytest", ..]
        | ["pytest" | "rg", ..] => true,
        ["cargo", "doc", rest @ ..] => !rest.contains(&"--open"),
        ["git", rest @ ..] => match git_display_subcommand(rest) {
            ["diff" | "show" | "status" | "log", ..] => true,
            ["branch", branch_args @ ..] => git_branch_display(branch_args),
            ["tag", tag_args @ ..] => git_tag_display(tag_args),
            ["stash", "list", stash_args @ ..] => git_stash_list_display(stash_args),
            ["reflog", reflog_args @ ..] => git_reflog_display(reflog_args),
            _ => false,
        },
        _ => false,
    }
}

/// Whether `git tag` arguments only list tags in display format. Creating,
/// annotating, signing, verifying, deleting and machine-formatted output
/// (`--format`) return `false`.
fn git_tag_display(args: &[&str]) -> bool {
    let mut list = false;
    let mut iter = args.iter().copied().peekable();
    while let Some(arg) = iter.next() {
        if let Some(short) = arg
            .strip_prefix('-')
            .filter(|s| !s.is_empty() && !s.starts_with('-'))
        {
            let mut chars = short.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    'l' => list = true,
                    'i' => {}
                    'n' => {
                        let rest: String = chars.collect();
                        if !rest.is_empty() {
                            if !rest.bytes().all(|b| b.is_ascii_digit()) {
                                return false;
                            }
                        } else if iter.peek().is_some_and(|next| {
                            !next.is_empty() && next.bytes().all(|b| b.is_ascii_digit())
                        }) {
                            iter.next();
                        }
                        break;
                    }
                    _ => return false,
                }
            }
            continue;
        }
        if arg.starts_with("--") {
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg, None), |(f, v)| (f, Some(v)));
            match flag {
                "--list" if inline.is_none() => list = true,
                "--ignore-case" | "--no-color" | "--no-column" if inline.is_none() => {}
                "--color" | "--column" => {
                    if inline.is_some_and(str::is_empty) {
                        return false;
                    }
                }
                "--merged" | "--no-merged" => {
                    if inline.is_some_and(str::is_empty) {
                        return false;
                    }
                    if inline.is_none() && iter.peek().is_some_and(|next| !next.starts_with('-')) {
                        iter.next();
                    }
                }
                "--contains" | "--no-contains" | "--points-at" => {
                    let value = match inline {
                        Some(v) => Some(v),
                        None => iter.next(),
                    };
                    if value.is_none_or(|v| v.is_empty() || v.starts_with('-')) {
                        return false;
                    }
                }
                "--sort" => {
                    let value = match inline {
                        Some(v) => Some(v),
                        None => iter.next(),
                    };
                    if value.is_none_or(|v| v.is_empty() || v.starts_with("--")) {
                        return false;
                    }
                }
                _ => return false,
            }
            continue;
        }
        // Positional operands create a tag unless `-l`/`--list` puts `git tag` in
        // pattern-listing mode.
        if !list {
            return false;
        }
    }
    true
}

/// Whether `git stash list` arguments keep display output without external diff
/// helpers, file outputs or machine-oriented flags.
fn git_stash_list_display(args: &[&str]) -> bool {
    !args.iter().any(|arg| {
        let flag = arg.split_once('=').map_or(*arg, |(k, _)| k);
        matches!(
            flag,
            "--output"
                | "--ext-diff"
                | "--textconv"
                | "--porcelain"
                | "-z"
                | "--null"
                | "--raw"
                | "--name-only"
                | "--name-status"
                | "--numstat"
        )
    })
}

/// Whether `git reflog` arguments only list reflog entries in display format.
/// Mutating subcommands (`expire`, `delete`) and `exists` return `false`.
fn git_reflog_display(args: &[&str]) -> bool {
    let mut subcommand_checked = false;
    for arg in args.iter().copied().take_while(|a| *a != "--") {
        let flag = arg.split_once('=').map_or(arg, |(k, _)| k);
        if matches!(
            flag,
            "--output"
                | "--ext-diff"
                | "--textconv"
                | "--porcelain"
                | "-z"
                | "--null"
                | "--raw"
                | "--format"
                | "--pretty"
        ) {
            return false;
        }
        if !subcommand_checked && !arg.starts_with('-') {
            subcommand_checked = true;
            if matches!(arg, "expire" | "delete" | "exists") {
                return false;
            }
        }
    }
    true
}

/// Whether `git branch` arguments only list or inspect branches in display format.
/// Creating, deleting, renaming, copying, tracking, editing descriptions and
/// machine-formatted output (`--format`) return `false`.
fn git_branch_display(args: &[&str]) -> bool {
    let mut list = false;
    let mut iter = args.iter().copied().peekable();
    while let Some(arg) = iter.next() {
        if let Some(short) = arg
            .strip_prefix('-')
            .filter(|s| !s.is_empty() && !s.starts_with('-'))
        {
            if !short.chars().all(|c| matches!(c, 'a' | 'r' | 'v' | 'i')) {
                return false;
            }
            continue;
        }
        if arg.starts_with("--") {
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg, None), |(f, v)| (f, Some(v)));
            match flag {
                "--list" if inline.is_none() => list = true,
                "--all" | "--remotes" | "--verbose" | "--ignore-case" | "--show-current"
                | "--no-abbrev" | "--no-color" | "--no-column"
                    if inline.is_none() => {}
                "--color" | "--column" | "--abbrev" => {
                    if inline.is_some_and(str::is_empty) {
                        return false;
                    }
                }
                "--merged" | "--no-merged" => {
                    if inline.is_some_and(str::is_empty) {
                        return false;
                    }
                    if inline.is_none() && iter.peek().is_some_and(|next| !next.starts_with('-')) {
                        iter.next();
                    }
                }
                "--contains" | "--no-contains" | "--points-at" => {
                    let value = match inline {
                        Some(v) => Some(v),
                        None => iter.next(),
                    };
                    if value.is_none_or(|v| v.is_empty() || v.starts_with('-')) {
                        return false;
                    }
                }
                "--sort" => {
                    let value = match inline {
                        Some(v) => Some(v),
                        None => iter.next(),
                    };
                    if value.is_none_or(|v| v.is_empty() || v.starts_with("--")) {
                        return false;
                    }
                }
                _ => return false,
            }
            continue;
        }
        // Positional operands create a branch unless `--list` puts `git branch` in
        // pattern-listing mode.
        if !list {
            return false;
        }
    }
    true
}

/// Whether `words` runs a formatter in a strictly read-only check/diff mode with
/// local paths and display/selection options only (no writes, plugins or config
/// overrides). Bare formatter invocations stay unapproved.
fn format_check_only(words: &[&str]) -> bool {
    cargo_fmt_check(words)
        || ruff_format_check(words)
        || prettier_check(words)
        || deno_fmt_check(words)
        || biome_format_check(words)
        || gofmt_check(words)
        || shfmt_check(words)
        || black_check(words)
        || rustfmt_check(words)
}

/// Whether `words` is a read-only `biome format` invocation with local target
/// paths (or `--changed`/`--staged`) and no `--write`, `--fix`, `--unsafe` or
/// `--config-path`. Bare `biome format` stays unapproved.
fn biome_format_check(words: &[&str]) -> bool {
    let ["biome", "format", args @ ..] = words else {
        return false;
    };
    if args.is_empty() || !args.iter().all(|arg| local_argument(arg)) {
        return false;
    }
    let mut target = false;
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(&arg) = iter.next() {
        if positional_only {
            target = true;
            continue;
        }
        if arg == "--" {
            positional_only = true;
            continue;
        }
        if !arg.starts_with('-') {
            target = true;
            continue;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        match flag {
            "--changed" | "--staged" if inline.is_none() => target = true,
            "--verbose"
            | "--no-errors-on-unmatched"
            | "--error-on-warnings"
            | "--skip-parse-errors"
            | "--vcs-enabled"
            | "--vcs-use-ignore-file"
            | "--files-ignore-unknown"
                if inline.is_none() => {}
            "--colors" | "--indent-style" | "--indent-width" | "--line-ending" | "--line-width"
            | "--quote-style" | "--jsx-quote-style" | "--semicolons" | "--trailing-commas"
            | "--max-diagnostics" | "--since" => {
                let value = match inline {
                    Some(v) => Some(v),
                    None => iter.next().copied(),
                };
                if value.is_none_or(|v| v.is_empty() || v.starts_with('-')) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    target
}

/// Whether `words` is a read-only `gofmt -l` or `gofmt -d` invocation with
/// local paths and no `-w` write flag.
fn gofmt_check(words: &[&str]) -> bool {
    let ["gofmt", args @ ..] = words else {
        return false;
    };
    if !args.iter().all(|arg| local_argument(arg)) {
        return false;
    }
    let mut check = false;
    let mut positional_only = false;
    for &arg in args {
        if positional_only {
            continue;
        }
        if arg == "--" {
            positional_only = true;
            continue;
        }
        if !arg.starts_with('-') {
            continue;
        }
        match arg {
            "-l" | "-d" => check = true,
            "-s" | "-e" => {}
            _ => return false,
        }
    }
    check
}

/// Whether `words` is a read-only `shfmt -d` or `shfmt -l` invocation with
/// local paths and no `-w`/`--write` flag.
fn shfmt_check(words: &[&str]) -> bool {
    let ["shfmt", args @ ..] = words else {
        return false;
    };
    if !args.iter().all(|arg| local_argument(arg)) {
        return false;
    }
    let mut check = false;
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(&arg) = iter.next() {
        if positional_only {
            continue;
        }
        if arg == "--" {
            positional_only = true;
            continue;
        }
        if !arg.starts_with('-') {
            continue;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        match flag {
            "-d" | "--diff" | "-l" | "--list" if inline.is_none() => check = true,
            "-s" | "--simplify" | "-p" | "--posix" | "-f" | "--find" | "-bn"
            | "--binary-next-line" | "-ci" | "--case-indent" | "-sr" | "--space-redirects"
            | "-kp" | "--keep-padding" | "-fn" | "--func-next-line" | "-mn" | "--minify"
            | "--apply-ignore"
                if inline.is_none() => {}
            "-i" | "--indent" | "-ln" | "--language-dialect" => {
                let value = match inline {
                    Some(v) => Some(v),
                    None => iter.next().copied(),
                };
                if value.is_none_or(|v| v.is_empty() || v.starts_with('-')) {
                    return false;
                }
            }
            _ => {
                if inline.is_none()
                    && let Some(rest) = arg.strip_prefix("-i")
                    && !rest.is_empty()
                    && rest.bytes().all(|b| b.is_ascii_digit())
                {
                    continue;
                }
                if inline.is_none()
                    && let Some(cluster) = arg
                        .strip_prefix('-')
                        .filter(|s| !s.is_empty() && !s.starts_with('-'))
                    && cluster
                        .chars()
                        .all(|c| matches!(c, 'd' | 'l' | 's' | 'p' | 'f'))
                {
                    check |= cluster.contains(['d', 'l']);
                    continue;
                }
                return false;
            }
        }
    }
    check
}

/// Whether `words` is a read-only `black --check` or `black --diff` invocation
/// with local paths and no `--config` override.
fn black_check(words: &[&str]) -> bool {
    let ["black", args @ ..] = words else {
        return false;
    };
    if !args.iter().all(|arg| local_argument(arg)) {
        return false;
    }
    let mut check = false;
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(&arg) = iter.next() {
        if positional_only {
            continue;
        }
        if arg == "--" {
            positional_only = true;
            continue;
        }
        if !arg.starts_with('-') {
            continue;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        match flag {
            "--check" | "--diff" if inline.is_none() => check = true,
            "-q"
            | "--quiet"
            | "-v"
            | "--verbose"
            | "--color"
            | "--no-color"
            | "--fast"
            | "--safe"
            | "--preview"
            | "--unstable"
            | "-S"
            | "--skip-string-normalization"
            | "-C"
            | "--skip-magic-trailing-comma"
            | "-x"
            | "--skip-source-first-line"
            | "--pyi"
            | "--ipynb"
                if inline.is_none() => {}
            "-l" | "--line-length" | "-t" | "--target-version" | "--include" | "--exclude"
            | "--extend-exclude" | "--force-exclude" | "--required-version" => {
                let value = match inline {
                    Some(v) => Some(v),
                    None => iter.next().copied(),
                };
                if value.is_none_or(|v| v.is_empty() || v.starts_with('-')) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    check
}

/// Whether `words` is a read-only `rustfmt --check` invocation with local paths
/// and no `--config-path`, `--config`, or `--emit` override.
fn rustfmt_check(words: &[&str]) -> bool {
    let ["rustfmt", args @ ..] = words else {
        return false;
    };
    if !args.iter().all(|arg| local_argument(arg)) {
        return false;
    }
    let mut check = false;
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(&arg) = iter.next() {
        if positional_only {
            continue;
        }
        if arg == "--" {
            positional_only = true;
            continue;
        }
        if !arg.starts_with('-') {
            continue;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        match flag {
            "--check" if inline.is_none() => check = true,
            "-l" | "--files-with-diff" | "-v" | "--verbose" | "-q" | "--quiet"
            | "--skip-children"
                if inline.is_none() => {}
            "--edition" | "--color" | "--style-edition" => {
                let value = match inline {
                    Some(v) => Some(v),
                    None => iter.next().copied(),
                };
                if value.is_none_or(|v| v.is_empty() || v.starts_with('-')) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    check
}

/// Whether `words` is a read-only `ruff format --check` or `ruff format --diff`
/// invocation with local paths and no `--config` override.
fn ruff_format_check(words: &[&str]) -> bool {
    let ["ruff", "format", args @ ..] = words else {
        return false;
    };
    if !args.iter().all(|arg| local_argument(arg)) {
        return false;
    }
    let mut check = false;
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(&arg) = iter.next() {
        if positional_only {
            continue;
        }
        if arg == "--" {
            positional_only = true;
            continue;
        }
        if !arg.starts_with('-') {
            continue;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        match flag {
            "--check" | "--diff" if inline.is_none() => check = true,
            "-q"
            | "--quiet"
            | "-v"
            | "--verbose"
            | "-s"
            | "--silent"
            | "--no-cache"
            | "--respect-gitignore"
            | "--no-respect-gitignore"
            | "--force-exclude"
            | "--no-force-exclude"
            | "--isolated"
            | "--preview"
            | "--no-preview"
                if inline.is_none() => {}
            "--color" | "--line-length" | "--target-version" | "--exclude" | "--extend-exclude"
            | "--range" => {
                let value = match inline {
                    Some(v) => Some(v),
                    None => iter.next().copied(),
                };
                if value.is_none_or(|v| v.is_empty() || v.starts_with('-')) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    check
}

/// Whether `words` is a read-only `prettier --check` or `--list-different`
/// invocation with local paths and no writes, plugins or config overrides.
fn prettier_check(words: &[&str]) -> bool {
    let ["prettier", args @ ..] = words else {
        return false;
    };
    if !args.iter().all(|arg| local_argument(arg)) {
        return false;
    }
    let mut check = false;
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(&arg) = iter.next() {
        if positional_only {
            continue;
        }
        if arg == "--" {
            positional_only = true;
            continue;
        }
        if !arg.starts_with('-') {
            continue;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        match flag {
            "--check" | "-c" | "--list-different" | "-l" if inline.is_none() => check = true,
            "--no-color"
            | "--no-error-on-unmatched-pattern"
            | "--ignore-unknown"
            | "-u"
            | "--cache"
            | "--no-config"
            | "--no-editorconfig"
            | "--with-node-modules"
                if inline.is_none() => {}
            "--log-level" => {
                let value = match inline {
                    Some(v) => Some(v),
                    None => iter.next().copied(),
                };
                if value.is_none_or(|v| v.is_empty() || v.starts_with('-')) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    check
}

/// Whether `words` is a read-only `deno fmt --check` invocation with local paths
/// and no `--config` override.
fn deno_fmt_check(words: &[&str]) -> bool {
    let ["deno", "fmt", args @ ..] = words else {
        return false;
    };
    if !args.iter().all(|arg| local_argument(arg)) {
        return false;
    }
    let mut check = false;
    let mut positional_only = false;
    let mut iter = args.iter();
    while let Some(&arg) = iter.next() {
        if positional_only {
            continue;
        }
        if arg == "--" {
            positional_only = true;
            continue;
        }
        if !arg.starts_with('-') {
            continue;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        match flag {
            "--check" if inline.is_none() => check = true,
            "-q" | "--quiet" | "--use-tabs" | "--single-quote" | "--no-semicolons"
            | "--no-config"
                if inline.is_none() => {}
            "--ignore" | "--line-width" | "--indent-width" | "--prose-wrap" => {
                let value = match inline {
                    Some(v) => Some(v),
                    None => iter.next().copied(),
                };
                if value.is_none_or(|v| v.is_empty() || v.starts_with('-')) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    check
}

/// Whether `words` is a read-only `cargo fmt --check` invocation with local paths
/// and display/selection options only. Bare `cargo fmt` writes files and stays
/// unapproved.
fn cargo_fmt_check(words: &[&str]) -> bool {
    let ["cargo", "fmt", args @ ..] = words else {
        return false;
    };
    if !args.iter().all(|arg| local_argument(arg)) {
        return false;
    }
    let (before, after) = match args.iter().position(|arg| *arg == "--") {
        Some(at) => (&args[..at], &args[at + 1..]),
        None => (args, &[][..]),
    };
    let mut check = false;
    let mut iter = before.iter();
    while let Some(&arg) = iter.next() {
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        match flag {
            "--check" if inline.is_none() => check = true,
            "--all" | "-v" | "--verbose" | "-q" | "--quiet" | "--offline" | "--frozen"
            | "--locked"
                if inline.is_none() => {}
            "-p" | "--package" | "--manifest-path" | "--color" => {
                let value = match inline {
                    Some(v) => Some(v),
                    None => iter.next().copied(),
                };
                if value.is_none_or(|v| v.is_empty() || v.starts_with('-')) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    let mut iter = after.iter();
    while let Some(&arg) = iter.next() {
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        match flag {
            "--check" if inline.is_none() => check = true,
            "-l" | "--files-with-diff" | "-v" | "--verbose" | "-q" | "--quiet"
            | "--skip-children"
                if inline.is_none() => {}
            "--edition" | "--color" => {
                let value = match inline {
                    Some(v) => Some(v),
                    None => iter.next().copied(),
                };
                if value.is_none_or(|v| v.is_empty() || v.starts_with('-')) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    check
}

/// Whether a routed command may also run without the client asking, for a client
/// that approves every rewritten command, such as Codex. A package runner that may
/// download the tool or install packages, such as `npx` or `uv run`, fails at any
/// position, while `bundle exec`, `poetry run` and the other runners of installed
/// tools leave the decision to the tool. Build tools are limited to conventional
/// build and test targets and a few display options. No argument may name a path
/// outside the working tree, so options such as `make -C /elsewhere` or
/// `pytest --basetemp=/tmp/x` fail, and `-f`, `-I` and `--config` fail for the tools
/// that load code from them. Options that run another program or code given on
/// the command line, or load a plugin or module they name, also fail, such as
/// `go vet -vettool`, `pylint --init-hook` or `node --test --import`.
pub fn approvable(words: &[&str]) -> bool {
    let (tool, fetches) = runners(words);
    if fetches {
        return false;
    }
    let Some((&program, args)) = tool.split_first() else {
        return false;
    };
    // Searches and listings take only the options their spec lists, which only
    // read, so `rg --pre`, `--hostname-bin` and `-z`, which run other programs,
    // fail like any option the spec does not know.
    if let Some(read_only) = crate::search::read_only(tool) {
        return read_only;
    }
    // Git display commands and JSON reads only read, and their options are
    // checked when they are routed.
    if json_output(tool) {
        return true;
    }
    if program == "git" {
        return legacy_hook(tool);
    }
    // A command routed for its preview alone installs, builds or reaches a
    // service, so it runs only when the client asks — except read-only
    // format-check modes (`cargo fmt --check`, `ruff format --check`/`--diff`,
    // `prettier --check`/`--list-different` and `deno fmt --check`).
    if crate::known::listed(tool) {
        return format_check_only(tool);
    }
    if !args.iter().all(|arg| local_argument(arg)) || runs_code(program, args) {
        return false;
    }
    const TARGETS: &[&str] = &["all", "build", "test", "check", "clean"];
    let target = |name: &str| TARGETS.contains(&name);
    match program {
        "make" | "gmake" => build_args(
            args,
            &[
                "-k",
                "--keep-going",
                "-s",
                "--silent",
                "--quiet",
                "--no-print-directory",
                "-n",
                "--dry-run",
                "--just-print",
            ],
            &["-j", "--jobs", "-l", "--load-average"],
            &[],
            target,
        ),
        "ninja" => build_args(
            args,
            &["-v", "--verbose", "-n"],
            &["-j", "-k", "-l"],
            &[],
            target,
        ),
        "cmake" => match args {
            ["--build", directory, rest @ ..] if !directory.starts_with('-') => {
                build_args(
                    rest,
                    &["-v", "--verbose", "--clean-first"],
                    &["-j", "--parallel"],
                    &["--config", "--target", "-t"],
                    |_| false,
                ) && rest
                    .iter()
                    .enumerate()
                    .all(|(i, arg)| match arg.split_once('=') {
                        Some(("--target", name)) => target(name),
                        _ if matches!(*arg, "--target" | "-t") => {
                            rest.get(i + 1).is_some_and(|name| target(name))
                        }
                        _ => true,
                    })
            }
            _ => false,
        },
        "gradle" | "gradlew" | "./gradlew" => build_args(
            args,
            &[
                "-q",
                "--quiet",
                "-i",
                "--info",
                "-s",
                "--stacktrace",
                "-S",
                "--full-stacktrace",
                "--offline",
                "--continue",
                "--parallel",
                "--no-parallel",
                "--daemon",
                "--no-daemon",
                "--build-cache",
                "--no-build-cache",
                "--rerun-tasks",
                "-m",
                "--dry-run",
            ],
            &[],
            &[
                "--console",
                "--warning-mode",
                "-x",
                "--exclude-task",
                "--tests",
                "--max-workers",
            ],
            // A task may be qualified by its project, such as `:app:test`.
            |task| {
                !task.starts_with('-')
                    && matches!(
                        task.rsplit(':').next(),
                        Some("build" | "test" | "check" | "assemble" | "clean")
                    )
            },
        ),
        "mvn" | "mvnw" | "./mvnw" => build_args(
            args,
            &[
                "-q",
                "--quiet",
                "-o",
                "--offline",
                "-B",
                "--batch-mode",
                "-e",
                "--errors",
                "-X",
                "--debug",
                "-ntp",
                "--no-transfer-progress",
                "-am",
                "--also-make",
                "-amd",
                "--also-make-dependents",
                "-ff",
                "--fail-fast",
                "-fae",
                "--fail-at-end",
                "-N",
                "--non-recursive",
            ],
            &[],
            &[
                "-T",
                "--threads",
                "-pl",
                "--projects",
                "-rf",
                "--resume-from",
            ],
            |phase| {
                let property = phase
                    .strip_prefix("-D")
                    .map(|property| property.split_once('=').map_or(property, |(key, _)| key));
                matches!(
                    phase,
                    "clean"
                        | "validate"
                        | "compile"
                        | "test-compile"
                        | "test"
                        | "package"
                        | "verify"
                ) || matches!(
                    property,
                    Some("test" | "it.test" | "skipTests" | "skipITs" | "maven.test.skip")
                )
            },
        ),
        "cargo" => {
            !args.iter().any(|arg| {
                let flag = arg.split_once('=').map_or(*arg, |(flag, _)| flag);
                arg.starts_with("-Z")
                    || flag == "--config"
                    || (args.first() == Some(&"doc") && *arg == "--open")
                    || (args.first() == Some(&"nextest")
                        && matches!(
                            flag,
                            "--config-file"
                                | "--user-config-file"
                                | "--tool-config-file"
                                | "--archive-file"
                                | "--cargo-metadata"
                                | "--workspace-remap"
                                | "--target-dir-remap"
                                | "--remap-path-prefix"
                                | "--target-runner"
                        ))
            }) && (args.first() != Some(&"nextest") || args.get(1) == Some(&"run"))
                && clippy_lint_levels(args)
        }
        // `-vettool` and `-toolexec` run the program they name, `-gccgoflags` and
        // an `-extld` or `-extldflags` in `-ldflags` choose the compiler's or
        // linker's helper programs.
        "go" => !args.iter().enumerate().any(|(index, arg)| {
            let (flag, value) = arg
                .split_once('=')
                .map_or((*arg, None), |(flag, value)| (flag, Some(value)));
            match flag.trim_start_matches('-') {
                "exec" | "toolexec" | "overlay" | "vettool" | "gccgoflags" => true,
                "ldflags" => value
                    .or_else(|| args.get(index + 1).copied())
                    .is_some_and(|value| value.contains("extld")),
                _ => false,
            }
        }),
        // MSBuild targets and properties, such as `-t:Publish` with a publish
        // profile, and package sources can deploy or fetch from elsewhere.
        "dotnet" => args.split_first().is_some_and(|(_, rest)| {
            build_args(
                rest,
                &["--no-restore", "--no-build", "--nologo", "--no-incremental"],
                &[],
                &[
                    "-c",
                    "--configuration",
                    "-f",
                    "--framework",
                    "-v",
                    "--verbosity",
                    "--filter",
                ],
                |project| !project.starts_with('-'),
            )
        }),
        // `--install-types` installs packages without asking.
        "mypy" | "dmypy" => !args
            .iter()
            .any(|arg| arg.split('=').next() == Some("--install-types")),
        // Biome check/lint/ci and oxlint auto-approve only in read-only mode.
        "biome" => !args.iter().any(|arg| {
            matches!(
                arg.split('=').next(),
                Some("--write" | "--unsafe" | "--fix" | "--config-path")
            )
        }),
        "oxlint" => !args.iter().any(|arg| {
            arg.split('=')
                .next()
                .is_some_and(|f| f.starts_with("--fix"))
        }),
        _ => true,
    }
}

/// Whether a tool's arguments run another program or code given on the command
/// line, or load a plugin or module they name, as these options document:
///
/// - pytest `-p NAME` other than `-p no:NAME`, `--pdbcls` and an `addopts`
///   override that could add them;
/// - pylint `--init-hook` and `--load-plugins`, mypy `--python-executable`,
///   pyright `--pythonpath` and RuboCop `-r`/`--require`;
/// - npm, pnpm, Yarn and Bun `--script-shell`, `--node-options`, `--userconfig`
///   and `--globalconfig`, which choose the shell or Node options scripts run with;
/// - `node --test` with `-e`/`--eval`, `-p`/`--print`, `-r`/`--require`,
///   `--import` or `--loader`, which also take inline `data:` modules, or with
///   `--env-file`, `--experimental-config-file` or `--run`, which can set them
///   or run a script;
/// - Swift `-X…` options passed to the compilers and linker, `--toolchain`,
///   `--toolset`, `--destination` and `--swift-sdks-path`, which choose them,
///   and `--disable-sandbox`, which runs package plugins unsandboxed;
/// - ESLint `--init` and `--inspect-config`, which run npm or npx, and
///   `--plugin`, `--rulesdir`, `--parser` and `--resolve-plugins-relative-to`;
/// - the module and setup options of the JavaScript test runners that `jest`,
///   `vitest`, `bun test` and a package manager's `test` script may start (see
///   `JS_LOADERS`), and an inline JSON `--config`, which can name them.
fn runs_code(program: &str, args: &[&str]) -> bool {
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    let named = |names: &[&str]| {
        args.iter()
            .take_while(|arg| **arg != "--")
            .any(|arg| names.iter().any(|name| names_option(arg, name)))
    };
    if matches!(program, "jest" | "vitest" | "npm" | "pnpm" | "yarn" | "bun")
        && js_runner_loads_code(args)
    {
        return true;
    }
    match program {
        "eslint" => named(&[
            "--init",
            "--inspect-config",
            "--plugin",
            "--rulesdir",
            "--parser",
            "--resolve-plugins-relative-to",
        ]),
        "pytest" | "py.test" => pytest_loads_code(args),
        "python" | "python3" => {
            matches!(args, ["-m", "pytest", ..]) && pytest_loads_code(&args[2..])
        }
        "pylint" => named(&["--init-hook", "--load-plugins"]),
        "mypy" | "dmypy" => named(&["--python-executable"]),
        "pyright" | "basedpyright" => named(&["--pythonpath"]),
        "ty" => named(&["--python", "--venv"]),
        "rubocop" => named(&["-r", "--require"]),
        "npm" | "pnpm" | "yarn" | "bun" => named(&[
            "--script-shell",
            "--node-options",
            "--userconfig",
            "--globalconfig",
        ]),
        "node" => named(&[
            "-e",
            "--eval",
            "-p",
            "--print",
            "-r",
            "--require",
            "--import",
            "--loader",
            "--experimental-loader",
            "--env-file",
            "--env-file-if-exists",
            "--experimental-config-file",
            "--run",
        ]),
        "swift" => {
            args.iter().any(|arg| arg.starts_with("-X"))
                || named(&[
                    "--toolchain",
                    "--toolset",
                    "--destination",
                    "--swift-sdks-path",
                    "--disable-sandbox",
                ])
        }
        _ => false,
    }
}

/// Options of Jest, Vitest and Bun's test runner that load a module they name or
/// start a browser or server, in lowercase without `-` or `_`, so Jest's
/// `--setupFiles` and `--setup-files` both match. A test environment such as
/// `--env=jsdom` stays allowed: tests routinely need one, and it names a
/// package the project installed.
const JS_LOADERS: &[&str] = &[
    "testrunner",
    "runner",
    "globalsetup",
    "globalteardown",
    "setupfiles",
    "setupfilesafterenv",
    "transform",
    "resolver",
    "testsequencer",
    "snapshotresolver",
    "testresultsprocessor",
    "preset",
    "watchplugins",
    "preload",
    "browser",
    "api",
    "open",
];

/// Whether JavaScript test runner arguments, including those a package manager
/// passes to its `test` script after `--`, load code as `JS_LOADERS` lists, or
/// give `--config` as inline JSON, which can name such modules.
fn js_runner_loads_code(args: &[&str]) -> bool {
    args.iter().any(|arg| {
        let Some(option) = arg.strip_prefix("--") else {
            return false;
        };
        let (key, value) = option
            .split_once('=')
            .map_or((option, None), |(key, value)| (key, Some(value)));
        // Vitest reads dotted names such as `--browser.enabled`.
        let key = key.split('.').next().unwrap_or(key);
        let key: String = key
            .chars()
            .filter(|c| !matches!(c, '-' | '_'))
            .flat_map(char::to_lowercase)
            .collect();
        JS_LOADERS.contains(&key.as_str())
            || key == "config" && value.is_some_and(|value| value.trim_start().starts_with('{'))
    }) || args
        .windows(2)
        .any(|pair| matches!(pair[0], "--config" | "-c") && pair[1].trim_start().starts_with('{'))
}

/// Whether `arg` gives the option `name`. A long option matches with or without
/// `=VALUE`, with `_` for `-`, and by any abbreviation, which npm, argparse and
/// other parsers accept. A one-letter option matches in any cluster of short
/// options, alone or with its value attached, as in `-r./x`.
fn names_option(arg: &str, name: &str) -> bool {
    if let Some(long) = name.strip_prefix("--") {
        let Some(key) = arg.strip_prefix("--") else {
            return false;
        };
        let key = key.split_once('=').map_or(key, |(key, _)| key);
        let key = key.replace('_', "-");
        !key.is_empty() && long.starts_with(&key)
    } else {
        let letter = name.trim_start_matches('-');
        arg.strip_prefix('-')
            .is_some_and(|cluster| !cluster.starts_with('-') && cluster.contains(letter))
    }
}

/// Whether pytest arguments load a plugin by name with `-p`, other than `-p
/// no:NAME`, which turns one off, choose a debugger class, or override `addopts`,
/// which could add either.
fn pytest_loads_code(args: &[&str]) -> bool {
    // One-letter options that take a value, alone or attached.
    const VALUED: &str = "kmcopnrW";
    let mut args = args.iter().copied();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (key, inline) = long
                .split_once('=')
                .map_or((long, None), |(key, value)| (key, Some(value)));
            if !key.is_empty() && "pdbcls".starts_with(key) {
                return true;
            }
            if !key.is_empty() && "override-ini".starts_with(key) {
                let value = inline.or_else(|| args.next()).unwrap_or("");
                if value.trim_start().starts_with("addopts") {
                    return true;
                }
            }
            continue;
        }
        let Some(cluster) = arg.strip_prefix('-').filter(|cluster| !cluster.is_empty()) else {
            continue;
        };
        for (index, letter) in cluster.char_indices() {
            if !VALUED.contains(letter) {
                continue;
            }
            let rest = &cluster[index + letter.len_utf8()..];
            let value = if rest.is_empty() {
                args.next().unwrap_or("")
            } else {
                rest
            };
            match letter {
                'p' if !value.starts_with("no:") => return true,
                'o' if value.trim_start().starts_with("addopts") => return true,
                _ => {}
            }
            break;
        }
    }
    false
}

/// Whether the arguments `cargo clippy` passes to the compiler after `--`, if
/// any, only set lint levels, such as `-D warnings`. Others, such as `-C linker`
/// or `--extern`, choose programs or libraries the compiler runs.
fn clippy_lint_levels(args: &[&str]) -> bool {
    if args.first() != Some(&"clippy") {
        return true;
    }
    let Some(at) = args.iter().position(|arg| *arg == "--") else {
        return true;
    };
    const LEVELS: &[&str] = &[
        "-W",
        "-A",
        "-D",
        "-F",
        "--warn",
        "--allow",
        "--deny",
        "--forbid",
        "--force-warn",
        "--cap-lints",
    ];
    let mut rest = args[at + 1..].iter();
    while let Some(arg) = rest.next() {
        if LEVELS.contains(arg) {
            if rest.next().is_none_or(|lint| lint.starts_with('-')) {
                return false;
            }
        } else if !LEVELS.iter().any(|level| {
            arg.strip_prefix(level).is_some_and(|value| {
                let value = if level.starts_with("--") {
                    value.strip_prefix('=')
                } else {
                    Some(value)
                };
                value.is_some_and(|lint| !lint.is_empty() && !lint.starts_with('-'))
            })
        }) {
            return false;
        }
    }
    true
}

/// Whether build tool arguments are only the given flags, job counts, options with a
/// value and targets that `target` accepts. A count is taken only when it is a
/// number, so `make -j deploy` still checks `deploy`. A value that looks like an
/// option fails, so `-x --dry-run` cannot hide a flag.
fn build_args(
    mut args: &[&str],
    flags: &[&str],
    counts: &[&str],
    valued: &[&str],
    target: impl Fn(&str) -> bool,
) -> bool {
    let number = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    while let Some((&arg, rest)) = args.split_first() {
        args = rest;
        let (key, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(key, value)| (key, Some(value)));
        if flags.contains(&arg) {
            continue;
        }
        if counts.contains(&key) {
            match inline {
                Some(value) if !number(value) => return false,
                Some(_) => {}
                None if args.first().is_some_and(|value| number(value)) => args = &args[1..],
                None => {}
            }
            continue;
        }
        if counts
            .iter()
            .any(|count| !count.starts_with("--") && arg.strip_prefix(count).is_some_and(number))
        {
            continue;
        }
        if valued.contains(&key) {
            if inline.is_none() {
                match args.split_first() {
                    Some((value, rest)) if !value.starts_with('-') => args = rest,
                    _ => return false,
                }
            }
            continue;
        }
        if arg.starts_with('-') && !arg.starts_with("-D") || !target(arg) {
            return false;
        }
    }
    true
}

/// Whether an argument, and every value inside it, stays inside the working tree:
/// not absolute, not under `~`, and with no `..` component. A value follows any `=`
/// or `:`, a leading `@` or a blank, so nested settings such as
/// `--override-ini=log_file=/abs`, `--cov-report=html:/abs` and `@/abs/args.rsp`
/// are checked too.
fn local_argument(arg: &str) -> bool {
    let mut values = vec![arg];
    values.extend(
        arg.match_indices(['=', ':', '@', ' ', '\t'])
            .map(|(index, separator)| &arg[index + separator.len()..]),
    );
    // An attached short option value, such as `-o/tmp/out` or `-I/opt/include`.
    let mut chars = arg.chars();
    if chars.next() == Some('-') && chars.next().is_some_and(|c| c != '-') {
        values.push(chars.as_str());
    }
    values.into_iter().all(|value| {
        let path = Path::new(value);
        !value.starts_with('~')
            && !path.has_root()
            && !path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub command: Vec<String>,
    pub omit: String,
}

impl Rule {
    pub fn validate(&self) -> Result<Regex> {
        ensure!(
            !self.command.is_empty() && self.command.iter().all(|s| !s.is_empty()),
            "filters.command needs a nonempty argv prefix"
        );
        let regex = Regex::new(&self.omit).context("invalid filters.omit regex")?;
        ensure!(
            !regex.is_match(""),
            "filters.omit must not match empty text"
        );
        Ok(regex)
    }
}

pub struct Selection {
    pub profile: Option<Profile>,
    pub omit: Option<Regex>,
    /// Whether `omit` holds a tool's built-in progress lines, not a rule from the
    /// settings. Such a preview replaces the ordinary one only when it shows
    /// every other line.
    pub built_in: bool,
}

impl Selection {
    pub fn for_run(args: &[OsString], rules: &[Rule]) -> Result<Self> {
        let mut words: Vec<_> = args.iter().map(|a| a.to_str().unwrap_or("")).collect();
        if let Some(program) = words.first_mut() {
            *program = Path::new(*program)
                .file_name()
                .and_then(|p| p.to_str())
                .unwrap_or("");
        }
        let rule = rules
            .iter()
            .filter(|r| {
                words.starts_with(&r.command.iter().map(String::as_str).collect::<Vec<_>>())
            })
            .max_by_key(|r| r.command.len());
        // A rule from the settings replaces the tool's built-in progress lines.
        let (omit, built_in) = match rule {
            Some(rule) => (Some(rule.validate()?), false),
            None => {
                let omit = crate::known::progress(&words)
                    .map(Regex::new)
                    .transpose()
                    .context("invalid built-in progress rule")?;
                let built_in = omit.is_some();
                (omit, built_in)
            }
        };
        Ok(Self {
            profile: profile(&words),
            omit,
            built_in,
        })
    }
}

// Keep unrecognized evidence. For very large streams retain the first diagnostic and
// its following context independently of the head/tail preview.
pub struct LineFilter<'a> {
    config: &'a Selection,
    limit: usize,
    prefix: String,
    evidence: String,
    tail: std::collections::VecDeque<String>,
    tail_bytes: usize,
    retained: usize,
    omitted: usize,
    duplicate: usize,
    last: String,
    evidence_until: usize,
    clipped: bool,
    invalid: bool,
}

impl<'a> LineFilter<'a> {
    pub fn new(config: &'a Selection, limit: usize) -> Self {
        Self {
            config,
            limit,
            prefix: String::new(),
            evidence: String::new(),
            tail: Default::default(),
            tail_bytes: 0,
            retained: 0,
            omitted: 0,
            duplicate: 0,
            last: String::new(),
            evidence_until: 0,
            clipped: false,
            invalid: false,
        }
    }

    pub fn observe(&mut self, text: &str, n: usize) {
        if self.invalid
            || (self.config.omit.is_none()
                && !matches!(self.config.profile, Some(Profile::JsTests | Profile::Logs)))
        {
            return;
        }
        if text.len() >= 256 * 1024 || text.contains(['\0', '\u{fffd}']) {
            self.invalid = true;
            return;
        }
        let line = text.trim();
        let js = self.config.profile == Some(Profile::JsTests);
        let error = diagnostic(line);
        let context = n <= self.evidence_until;
        if error {
            self.evidence_until = n.saturating_add(4);
        }
        let log_key = (self.config.profile == Some(Profile::Logs)).then(|| log_dedup_key(line));
        if !error && !context {
            let success = js
                && (line.starts_with("PASS ")
                    || line.starts_with("(pass) ")
                    || line.starts_with("✓ ")
                    || line.starts_with("✔ ")
                    || line.starts_with("√ ")
                    || line.starts_with("ok ")
                        && line[3..].starts_with(|c: char| c.is_ascii_digit()));
            if success || self.config.omit.as_ref().is_some_and(|r| r.is_match(line)) {
                self.omitted += 1;
                self.last.clear();
                return;
            }
            if let Some(key) = log_key
                && !key.is_empty()
                && key == self.last
            {
                self.duplicate += 1;
                return;
            }
        }
        if let Some(key) = log_key {
            self.last.clear();
            self.last.push_str(key);
        }
        self.retained += 1;
        if !self.clipped && self.prefix.len() + text.len() <= self.limit {
            self.prefix.push_str(text);
        } else {
            self.clipped = true;
        }
        if error || context {
            let detail = format!("L{n} {}\n", line);
            append(&mut self.evidence, &detail, self.limit / 2);
        }
        let mut end = String::new();
        append(&mut end, text, self.limit / 4);
        self.tail_bytes += end.len();
        self.tail.push_back(end);
        while self.tail_bytes > self.limit / 4 || self.tail.len() > 32 {
            self.tail_bytes -= self.tail.pop_front().unwrap().len();
        }
    }

    /// The filtered preview, and whether it shows every line the filter kept.
    pub fn finish(self) -> Option<(Vec<u8>, bool)> {
        if self.invalid || self.omitted + self.duplicate == 0 {
            return None;
        }
        let mut report = format!(
            "[{} retained lines, {} filtered, {} repeats]\n",
            self.retained, self.omitted, self.duplicate
        );
        let whole = !self.clipped && report.len() + self.prefix.len() <= self.limit;
        if whole {
            report.push_str(&self.prefix);
        } else {
            if !self.evidence.is_empty() {
                report.push_str(&self.evidence);
            }
            append(&mut report, &self.prefix, self.limit * 2 / 3);
            report.push_str("\n[… full stream in log …]\n");
            for line in self.tail {
                append(&mut report, &line, self.limit);
            }
        }
        Some((report.into_bytes(), whole))
    }
}

/// Key used to detect consecutive repeated lines in `Profile::Logs` (`docker logs`
/// / `kubectl logs`), ignoring a leading timestamp so identical periodic lines
/// (such as health checks) fold even when each line carries a fresh timestamp.
pub(crate) fn log_dedup_key(line: &str) -> &str {
    static LEADING_TIMESTAMP: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(
            r"^(?:\[?(?:\d{4}[-/]\d{2}[-/]\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?|(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+\d{1,2}\s+\d{2}:\d{2}:\d{2}|\d{2}:\d{2}:\d{2}(?:[.,]\d+)?)\]?\s+)+",
        )
        .unwrap()
    });
    if let Some(found) = LEADING_TIMESTAMP.find(line) {
        let rest = &line[found.end()..];
        if !rest.is_empty() {
            return rest;
        }
    }
    line
}

pub(crate) fn diagnostic(line: &str) -> bool {
    let text = line.to_ascii_lowercase();
    [
        "fail",
        "error",
        "panic",
        "warn",
        "exception",
        "expected:",
        "received:",
        "not ok",
        "assert",
        "fatal",
        "✗",
        "×",
        "❯",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// Append whole lines that fit. A single line that does not fit is cut and ends
/// with `…`, so a fragment never reads as a complete line.
fn append(out: &mut String, text: &str, budget: usize) {
    let available = budget.saturating_sub(out.len());
    if text.len() <= available {
        out.push_str(text);
        return;
    }
    let fit = &text[..text.floor_char_boundary(available)];
    if let Some(end) = fit.rfind('\n') {
        out.push_str(&fit[..=end]);
    } else if available >= 20 {
        out.push_str(&fit[..fit.floor_char_boundary(available - "…\n".len())]);
        out.push_str("…\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_reads_each_options_own_syntax() {
        for (command, approved) in [
            // A ripgrep option outside its spec, alone, attached or in a cluster.
            ("rg -n foo", true),
            ("rg -c --stats --no-pre foo", true),
            ("rg --hostname-bin=x foo", false),
            ("rg -nz foo", false),
            ("rg --pre-glob=*.gz foo", false),
            // pytest's `-r` takes a value, so `-rp` reports passes and loads nothing.
            ("pytest -rp", true),
            ("pytest -vp evil", false),
            ("pytest -p no:cacheprovider", true),
            ("pytest --override-ini=addopts=-pevil", false),
            ("pytest -o log_cli=true", true),
            ("pytest --pdbcls=x:Y", false),
            ("python3 -m pytest -pevil", false),
            // npm reads abbreviations and `_` for `-` in its option names.
            ("npm test --script-sh=x", false),
            ("npm test --node_options=x", false),
            ("npm test --silent", true),
            // Arguments for the script after `--` are not npm's own.
            ("npm test -- --script-shell", true),
            ("node --test --require=x", false),
            ("node --test -rx", false),
            ("node --test --test-name-pattern=pre", true),
            ("go test -ldflags=-X=main.v=1 ./...", true),
            ("go test -ldflags -extldflags=-B. ./...", false),
            (
                "cargo clippy -- -D warnings -Wclippy::pedantic --cap-lints=warn",
                true,
            ),
            ("cargo clippy -- -D -C", false),
            ("cargo clippy -- -Clinker=x", false),
            ("cargo test -- --nocapture", true),
            // JavaScript test runners, also behind a package manager's script.
            ("jest --setupFiles=./x.js", false),
            ("jest --setup-files ./x.js", false),
            ("jest --config {\"globalSetup\":\"./x.js\"}", false),
            ("jest --env=jsdom", true),
            ("npm test -- --globalSetup ./x.js", false),
            ("vitest run --browser.enabled", false),
            ("bun test --preload ./x.ts", false),
            ("eslint --init", false),
            ("eslint --plugin x src", false),
            ("eslint src", true),
            // Read-only `cargo fmt --check` is approved; mutating `cargo fmt` is not.
            ("cargo fmt", false),
            ("cargo fmt --all", false),
            ("cargo fmt --check", true),
            ("cargo fmt --all -- --check", true),
            ("cargo fmt -p app -- --check --edition 2021", true),
            (
                "cargo fmt --manifest-path crates/a/Cargo.toml -- --check",
                true,
            ),
            (
                "cargo fmt --manifest-path ../outside/Cargo.toml -- --check",
                false,
            ),
            ("cargo fmt --check --config net.offline=false", false),
            ("cargo fmt -- --check --config-path rustfmt.toml", false),
            ("cargo fmt -- --check --emit=stdout", false),
            // Read-only `cargo tree` is approved with local paths and no `--config`/`-Z`.
            (
                "cargo tree -p app --no-default-features --edges normal",
                true,
            ),
            ("cargo tree --config net.offline=false", false),
            ("cargo tree -Z unstable-options", false),
            ("cargo tree --manifest-path ../outside/Cargo.toml", false),
            // Read-only `git branch` display is approved; mutating or formatted forms are not.
            ("git branch", true),
            ("git branch -a", true),
            ("git branch -avv", true),
            ("git branch -r --sort=-committerdate", true),
            ("git branch --merged main", true),
            ("git branch --no-merged", true),
            ("git branch --contains HEAD", true),
            ("git branch --list feat/*", true),
            ("git branch --show-current", true),
            ("git branch new-branch", false),
            ("git branch -a new-branch", false),
            ("git branch -d old", false),
            ("git branch -D old", false),
            ("git branch -m old new", false),
            ("git branch -c old new", false),
            ("git branch -f main HEAD", false),
            ("git branch -u origin/main", false),
            ("git branch --set-upstream-to=origin/main", false),
            ("git branch --format=%(refname:short)", false),
            ("git branch --list -d old", false),
            ("git branch --contains", false),
            // Safe non-interactive Git global flags (`--no-pager`, `-P`, `--no-optional-locks`)
            // are approved for display subcommands; `-c`, `-C`, `--paginate` stay unapproved.
            ("git --no-pager diff", true),
            ("git -P log -n 20", true),
            ("git --no-pager --no-optional-locks status -sb", true),
            ("git --no-pager branch -a", true),
            ("git --no-pager grep -n foo", true),
            ("git --no-pager ls-files", true),
            ("git --no-pager branch new-branch", false),
            ("git --no-pager push", false),
            ("git -c diff.external=evil diff", false),
            ("git -C /tmp diff", false),
            ("git --paginate diff", false),
            // Read-only `cargo doc` (without `--open`) is approved and uses the lint profile.
            ("cargo doc --no-deps", true),
            ("cargo doc --workspace --no-deps --locked", true),
            ("cargo doc --open", false),
            ("cargo doc --no-deps --open", false),
            ("cargo doc --config net.offline=false", false),
            ("cargo doc -Z unstable-options", false),
            // `cargo nextest run` is approved with local paths and without config/archive overrides.
            ("cargo nextest run", true),
            ("cargo nextest run --workspace --locked -E test(foo)", true),
            ("cargo nextest archive --archive-file out.tar.zst", false),
            ("cargo nextest run --config-file nextest.toml", false),
            ("cargo nextest run --user-config-file=user.toml", false),
            ("cargo nextest run --archive-file tests.tar.zst", false),
            ("cargo nextest run --cargo-metadata meta.json", false),
            ("cargo nextest run --target-runner sh", false),
            ("cargo nextest run --config net.offline=false", false),
            ("cargo nextest run -Z unstable-options", false),
            // Read-only `git tag` display and `git stash list` are approved; mutating or formatted forms are not.
            ("git tag", true),
            ("git tag -l", true),
            ("git tag -l v1.*", true),
            ("git tag --list v1.*", true),
            ("git tag -n", true),
            ("git tag -n3", true),
            ("git tag -n 3", true),
            ("git tag --sort=-v:refname", true),
            ("git tag --merged main", true),
            ("git tag --contains HEAD", true),
            ("git --no-pager tag -l", true),
            ("git tag v1.0.0", false),
            ("git tag -a v1.0.0 -m release", false),
            ("git tag -s v1.0.0", false),
            ("git tag -d v1.0.0", false),
            ("git tag -f v1.0.0", false),
            ("git tag -v v1.0.0", false),
            ("git tag -n v1.0.0", false),
            ("git tag --format=%(refname:short)", false),
            ("git stash list", true),
            ("git stash list -n 10 --stat", true),
            ("git --no-pager stash list", true),
            ("git stash", false),
            ("git stash push", false),
            ("git stash pop", false),
            ("git stash drop", false),
            ("git stash list --output=/tmp/x", false),
            ("git stash list --ext-diff", false),
            // Read-only `git reflog` is approved; `expire`, `delete`, `exists` and machine formats are not.
            ("git reflog", true),
            ("git reflog -n 20", true),
            ("git reflog show main", true),
            ("git --no-pager reflog --oneline", true),
            ("git reflog expire --all", false),
            ("git reflog delete HEAD@{0}", false),
            ("git reflog exists HEAD", false),
            ("git reflog --format=%H", false),
            // Read-only format-check modes are approved; bare formatters and config/plugin overrides are not.
            ("ruff format --check", true),
            ("ruff format --diff src", true),
            ("ruff format --check --line-length 100 src", true),
            ("ruff format", false),
            ("ruff format --check --config ruff.toml", false),
            ("ruff format --check ../other", false),
            ("prettier --check .", true),
            ("prettier --list-different src", true),
            ("prettier -c --ignore-unknown src", true),
            ("prettier src", false),
            ("prettier --check --write src", false),
            ("prettier --check --plugin ./p.js src", false),
            ("prettier --check --config .prettierrc src", false),
            ("deno fmt --check", true),
            ("deno fmt --check --single-quote src", true),
            ("deno fmt", false),
            ("deno fmt --check --config deno.json", false),
            ("biome format .", true),
            ("biome format src/app.ts", true),
            ("biome format --changed", true),
            ("pnpm exec biome format .", true),
            ("biome format", false),
            ("biome format --write .", false),
            ("biome format --unsafe .", false),
            ("biome format --config-path biome.json .", false),
            ("biome format ../other", false),
            ("gofmt -l .", true),
            ("gofmt -d src", true),
            ("gofmt -s -l .", true),
            ("gofmt", false),
            ("gofmt -w .", false),
            ("gofmt -l -w .", false),
            ("gofmt -l ../other", false),
            ("gofmt -l -cpuprofile prof .", false),
            ("shfmt -d .", true),
            ("shfmt -l scripts/", true),
            ("shfmt -sd -i 2 -ci scripts/", true),
            ("shfmt", false),
            ("shfmt -w scripts/", false),
            ("shfmt -d -w scripts/", false),
            ("shfmt -d ../other", false),
            ("black --check .", true),
            ("black --diff src", true),
            ("black --check --line-length 100 src", true),
            ("python3 -m black --check .", true),
            ("black", false),
            ("black src", false),
            ("black --check --config pyproject.toml .", false),
            ("black --check ../other", false),
            ("rustfmt --check src/main.rs", true),
            ("rustfmt --check --edition 2024 src/main.rs", true),
            ("rustfmt", false),
            ("rustfmt src/main.rs", false),
            (
                "rustfmt --check --config-path rustfmt.toml src/main.rs",
                false,
            ),
            ("rustfmt --check --emit=stdout src/main.rs", false),
            ("rustfmt --check ../other.rs", false),
            // `vitest --run` matches `vitest run` for approval and safety checks.
            ("vitest --run", true),
            ("vitest --run src/app.test.ts", true),
            ("vitest --run --browser.enabled", false),
            // `podman ps` and `podman logs` mirror `docker`.
            ("podman ps", true),
            ("podman ps -a", true),
            ("podman logs --tail 50 web", true),
            // Newly routed `known::ROUTED` commands stay unapproved.
            ("python3 -m unittest", false),
            ("python -m unittest discover", false),
            ("bun run typecheck", false),
            ("bazel build //...", false),
            ("bazel test //...", false),
            ("zig build", false),
            ("zig test src/main.zig", false),
            ("podman build .", false),
            ("podman images", false),
            ("podman image ls", false),
        ] {
            let words: Vec<&str> = command.split(' ').collect();
            assert_eq!(approvable(&words), approved, "{command}");
        }
        // Multi-ecosystem build/compile invocations route to `Profile::Lint`, while
        // test commands, non-build targets, bare `make`, and machine formats stay out.
        for command in [
            "make build",
            "make all",
            "make -j8",
            "make -j 4 all",
            "make clean build",
            "gmake",
            "gmake -j4 all",
            "ninja",
            "ninja -C build",
            "cmake --build build",
            "cmake --build build -j 8 --target app",
            "gradle build",
            "gradlew assemble",
            "./gradlew clean build",
            "./gradlew build -x test",
            "./gradlew :app:compileKotlin",
            "mvn compile",
            "mvnw clean compile",
            "./mvnw -B -ntp -T 4 clean package -DskipTests",
            "dotnet build",
            "dotnet build -c Release",
            "dotnet restore",
            "swift build",
            "go build ./...",
        ] {
            let words: Vec<&str> = command.split(' ').collect();
            assert_eq!(profile(&words), Some(Profile::Lint), "{command}");
            assert!(legacy_hook(&words), "{command}");
        }
        for command in [
            "make",
            "make test",
            "make -j test",
            "make -j8 test",
            "make check",
            "make clean",
            "make install",
            "make deploy",
            "make serve",
            "make -n build",
            "gmake test",
            "ninja test",
            "ninja -t compdb",
            "cmake --build build --target test",
            "cmake --build build -t install",
            "cmake -S . -B build",
            "gradle",
            "gradle test",
            "./gradlew :app:test --tests Foo",
            "./gradlew check",
            "./gradlew clean",
            "./gradlew bootRun",
            "mvn",
            "mvn test",
            "mvn clean test",
            "mvn clean verify",
            "mvn verify -Dtest=FooTest",
            "mvn deploy",
            "mvn clean",
            "dotnet test",
            "swift test",
            "go test ./...",
            "go build --json ./...",
        ] {
            let words: Vec<&str> = command.split(' ').collect();
            assert_eq!(profile(&words), None, "{command}");
        }
        // `vitest --run` routes to `Profile::JsTests`, respecting `js_display`.
        for command in [
            "vitest --run",
            "vitest --run src/app.test.ts",
            "vitest src/app.test.ts --run",
        ] {
            let words: Vec<&str> = command.split(' ').collect();
            assert_eq!(profile(&words), Some(Profile::JsTests), "{command}");
        }
        for command in [
            "vitest",
            "vitest --run --watch",
            "vitest --run --json",
            "vitest bench --run",
        ] {
            let words: Vec<&str> = command.split(' ').collect();
            assert_eq!(profile(&words), None, "{command}");
        }
        // `podman ps` / `podman container ls|list|ps` and `podman logs` mirror `docker`.
        for (command, expected) in [
            ("podman ps", Some(Profile::Containers)),
            ("podman ps -a", Some(Profile::Containers)),
            ("podman container ls", Some(Profile::Containers)),
            ("podman container list -a", Some(Profile::Containers)),
            ("podman container ps", Some(Profile::Containers)),
            ("podman logs web", Some(Profile::Logs)),
            ("podman logs --tail 50 -t web", Some(Profile::Logs)),
            ("podman container logs web", Some(Profile::Logs)),
            ("podman compose logs web", Some(Profile::Logs)),
            ("podman logs -f web", None),
            ("podman ps --format json", None),
        ] {
            let words: Vec<&str> = command.split(' ').collect();
            assert_eq!(profile(&words), expected, "{command}");
        }
        // `cargo doc` without `--open` selects the lint profile; `--open` selects none.
        assert_eq!(profile(&["cargo", "doc", "--no-deps"]), Some(Profile::Lint));
        assert_eq!(profile(&["cargo", "doc", "--open"]), None);
        // `legacy_hook` and `approvable` agree on `git branch`, `git tag` and `git stash list`.
        assert!(legacy_hook(&["git", "branch", "-a"]));
        assert!(legacy_hook(&["git", "branch", "--list", "feat/*"]));
        assert!(!legacy_hook(&["git", "branch", "new-branch"]));
        assert!(!legacy_hook(&["git", "branch", "-D", "old"]));
        assert!(!legacy_hook(&["git", "branch", "--format=%(refname)"]));
        assert!(legacy_hook(&["git", "tag", "-l", "v*"]));
        assert!(!legacy_hook(&["git", "tag", "v1.0.0"]));
        assert!(!legacy_hook(&["git", "tag", "-d", "v1.0.0"]));
        assert!(legacy_hook(&["git", "stash", "list"]));
        assert!(!legacy_hook(&["git", "stash", "pop"]));
        assert!(legacy_hook(&["cargo", "nextest", "run", "--workspace"]));
        assert!(!legacy_hook(&["cargo", "nextest", "archive"]));
        assert!(legacy_hook(&["git", "reflog", "-n", "10"]));
        assert!(!legacy_hook(&["git", "reflog", "expire", "--all"]));
        // Biome, oxlint and ty check route as linters and auto-approve only when read-only.
        assert_eq!(profile(&["biome", "check", "."]), Some(Profile::Lint));
        assert_eq!(profile(&["oxlint", "src"]), Some(Profile::Lint));
        assert_eq!(profile(&["ty", "check", "."]), Some(Profile::Lint));
        assert!(approvable(&["biome", "check", "."]));
        assert!(!approvable(&["biome", "check", "--write", "."]));
        assert!(approvable(&["oxlint", "src"]));
        assert!(!approvable(&["oxlint", "--fix", "src"]));
        assert!(approvable(&["ty", "check", "."]));
        assert!(!approvable(&[
            "ty",
            "check",
            "--python",
            "/usr/bin/python3"
        ]));
    }

    #[test]
    fn logs_profile_folds_timestamped_repeats_and_js_filters_bun_passes() {
        let log_args: Vec<OsString> = ["docker", "logs", "web"]
            .into_iter()
            .map(OsString::from)
            .collect();
        let selection = Selection::for_run(&log_args, &[]).unwrap();
        assert_eq!(selection.profile, Some(Profile::Logs));
        let mut filter = LineFilter::new(&selection, 4096);
        let lines = [
            "2026-11-25T14:01:01.100Z INFO healthcheck ok\n",
            "2026-11-25T14:01:02.200Z INFO healthcheck ok\n",
            "2026-11-25T14:01:03.300Z INFO healthcheck ok\n",
            "[2026-11-25 14:01:04] INFO healthcheck ok\n",
            "2026/11/25 14:01:05 worker idle\n",
            "2026/11/25 14:01:06 worker idle\n",
            "Nov 25 14:01:07 worker idle\n",
            "2026-11-25T14:01:08.000Z ERROR database connection refused\n",
            "2026-11-25T14:01:09.000Z ERROR database connection refused\n",
        ];
        for (i, line) in lines.iter().enumerate() {
            filter.observe(line, i + 1);
        }
        let (bytes, whole) = filter.finish().unwrap();
        assert!(whole);
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.contains("[4 retained lines, 0 filtered, 5 repeats]"),
            "{text}"
        );
        // Both ERROR lines stay verbatim even though their messages match.
        assert_eq!(
            text.matches("ERROR database connection refused").count(),
            2,
            "{text}"
        );
        // Only the first timestamped healthcheck and worker-idle lines are kept.
        assert_eq!(text.matches("INFO healthcheck ok").count(), 1, "{text}");
        assert_eq!(text.matches("worker idle").count(), 1, "{text}");

        let bun_args: Vec<OsString> = ["bun", "test"].into_iter().map(OsString::from).collect();
        let selection = Selection::for_run(&bun_args, &[]).unwrap();
        assert_eq!(selection.profile, Some(Profile::JsTests));
        let mut filter = LineFilter::new(&selection, 4096);
        let bun_lines = [
            "(pass) auth > verifies token [0.42ms]\n",
            "(pass) auth > refreshes session [0.18ms]\n",
            "(fail) billing > charges card [1.05ms]\n",
            " 2 pass\n",
            " 1 fail\n",
        ];
        for (i, line) in bun_lines.iter().enumerate() {
            filter.observe(line, i + 1);
        }
        let (bytes, _) = filter.finish().unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("2 filtered"), "{text}");
        assert!(text.contains("(fail) billing > charges card"), "{text}");
        assert!(!text.contains("verifies token"), "{text}");
    }
}
