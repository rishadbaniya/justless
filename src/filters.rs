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
    Pods,
    Deployments,
    StatefulSets,
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
            Self::Pods => "kubectl get pods",
            Self::Deployments => "kubectl get deployments",
            Self::StatefulSets => "kubectl get statefulsets",
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
            ["python" | "python3", "-m", tool, ..] if *tool != "pytest" => (&words[2..], false),
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
    matches!(
        (program, args),
        ("cargo", ["clippy" | "check" | "build", ..])
            | ("ruff", ["check", ..])
            | ("golangci-lint", ["run", ..])
            | ("go", ["vet", ..])
            | (
                "flake8"
                    | "pylint"
                    | "mypy"
                    | "pyright"
                    | "basedpyright"
                    | "eslint"
                    | "tsc"
                    | "vue-tsc"
                    | "shellcheck"
                    | "rubocop"
                    | "staticcheck"
                    | "swiftlint"
                    | "hadolint",
                _,
            )
    )
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
        ("docker", ["ps", tail @ ..] | ["container", "ls" | "list" | "ps", tail @ ..])
            if display_options(
                tail,
                &["-a", "--all", "--no-trunc", "--latest", "-l"],
                &["-f", "--filter", "-n", "--last"],
                false,
            ) =>
        {
            Some(Profile::Containers)
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
                "no" | "node" | "nodes" => Some(Profile::Nodes),
                _ => None,
            }
        }
        ("docker", ["logs", tail @ ..] | ["container", "logs", tail @ ..])
            if display_options(
                tail,
                &["-t", "--timestamps", "--details"],
                &["--since", "--until", "--tail", "-n"],
                true,
            ) =>
        {
            Some(Profile::Logs)
        }
        ("kubectl", ["logs", tail @ ..])
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

/// `gh api` options for a GET request whose response is printed as it came: no
/// fields or input, which make it a POST, no other method, and no `--jq` or
/// `--template`, which print something else.
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
    matches!(operands(args, VALUED).as_slice(), [endpoint] if *endpoint != "graphql")
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

pub fn legacy_hook(words: &[&str]) -> bool {
    matches!(
        unwrap_tool(words),
        ["cargo", "build" | "check" | "clippy" | "test", ..]
            | ["make" | "gmake" | "ninja", ..]
            | ["cmake", "--build", ..]
            | ["gradle" | "gradlew" | "./gradlew", ..]
            | ["mvn" | "mvnw" | "./mvnw", ..]
            | ["dotnet", "build" | "test" | "restore", ..]
            | ["swift", "build" | "test", ..]
            | ["git", "diff" | "show" | "status" | "log", ..]
            | ["go", "test" | "build", ..]
            | ["python" | "python3", "-m", "pytest", ..]
            | ["pytest" | "rg", ..]
    )
}

/// Whether a routed command may also run without the client asking, for a client
/// that approves every rewritten command, such as Codex. A package runner that may
/// download the tool or install packages, such as `npx` or `uv run`, fails at any
/// position, while `bundle exec`, `poetry run` and the other runners of installed
/// tools leave the decision to the tool. Build tools are limited to conventional
/// build and test targets and a few display options. No argument may name a path
/// outside the working tree, so options such as `make -C /elsewhere` or
/// `pytest --basetemp=/tmp/x` fail, and `-f`, `-I` and `--config` fail for the tools
/// that load code from them.
pub fn approvable(words: &[&str]) -> bool {
    let (tool, fetches) = runners(words);
    if fetches {
        return false;
    }
    let Some((&program, args)) = tool.split_first() else {
        return false;
    };
    // Searches, Git display commands and JSON reads only read, and their options
    // are checked when they are routed.
    if crate::search::select(tool).is_some() || program == "git" || json_output(tool) {
        return true;
    }
    if !args.iter().all(|arg| local_argument(arg)) {
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
        "cargo" => !args.iter().any(|arg| {
            arg.starts_with("-Z")
                || arg.split_once('=').map_or(*arg, |(flag, _)| flag) == "--config"
        }),
        "go" => !args.iter().any(|arg| {
            let flag = arg.split_once('=').map_or(*arg, |(flag, _)| flag);
            matches!(
                flag.trim_start_matches('-'),
                "exec" | "toolexec" | "overlay"
            )
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
        _ => true,
    }
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
        Ok(Self {
            profile: profile(&words),
            omit: rule.map(Rule::validate).transpose()?,
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
        if !error && !context {
            let success = js
                && (line.starts_with("PASS ")
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
            if self.config.profile == Some(Profile::Logs) && !line.is_empty() && text == self.last {
                self.duplicate += 1;
                return;
            }
        }
        if self.config.profile == Some(Profile::Logs) {
            self.last = text.to_owned();
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

    pub fn finish(self) -> Option<Vec<u8>> {
        if self.invalid || self.omitted + self.duplicate == 0 {
            return None;
        }
        let mut report = format!(
            "[{} retained lines, {} filtered, {} repeats]\n",
            self.retained, self.omitted, self.duplicate
        );
        if self.clipped || report.len() + self.prefix.len() > self.limit {
            if !self.evidence.is_empty() {
                report.push_str(&self.evidence);
            }
            append(&mut report, &self.prefix, self.limit * 2 / 3);
            report.push_str("\n[… full stream in log …]\n");
            for line in self.tail {
                append(&mut report, &line, self.limit);
            }
        } else {
            report.push_str(&self.prefix);
        }
        Some(report.into_bytes())
    }
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
