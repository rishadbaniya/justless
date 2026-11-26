//! Docker, Compose, Buildx, and Podman BuildKit & classic build step compactor.
//!
//! Compacts verbose container image build logs (BuildKit `--progress=plain` /
//! pipe output and classic `Step M/K : ...` / Podman `STEP M/K: ...` output)
//! into a high-signal, zero-semantic-loss summary with exact `[stdout:A-B]` /
//! `[stderr:A-B]` archive line pointers.
//!
//! Contracts:
//! - Fail closed (`None`) on any input that does not clearly match BuildKit or
//!   classic container build output, on binary input, or on machine-format build
//!   flags such as `--progress=rawjson` or `-q`/`--quiet`.
//! - Never replace unless the formatted report is strictly smaller than the raw
//!   combined `stdout.len() + stderr.len()` AND fits within `budget`.
//! - On failing builds, deduplicate BuildKit's repeated `------` / ` > [stage]`
//!   error echo, prioritize the failing step's error anchors and actionable tail
//!   first within `budget`, and collapse prior steps to a single summary line.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

/// Maximum single line length accepted (matches `logview::MAX_LINE_BYTES`).
const MAX_LINE_BYTES: usize = 256 * 1024;
/// Maximum displayed bytes for a single step header or error line.
const MAX_STEP_HEADER_BYTES: usize = 160;
const MAX_ERROR_LINE_BYTES: usize = 360;
const MAX_WARNING_LINE_BYTES: usize = 220;
const MAX_WARNINGS_SHOWN: usize = 8;
const MAX_FAILING_TAIL_LINES: usize = 12;
const MAX_ANCHOR_AFTER_LINES: usize = 8;

/// BuildKit summary banner: `[+] Building 14.3s (16/16) FINISHED ...`.
static BK_BANNER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[\+\]\s+Building\s+([0-9hms.]+)\s+\((\d+/\d+)\)")
        .expect("buildkit building banner")
});

/// BuildKit Dockerfile location line before `--------------------`: `Dockerfile:14`.
static DOCKERFILE_LOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^\s:]+(?:/[^\s:]+)*):(\d+)\s*$").expect("dockerfile location")
});

/// BuildKit `N warnings found` header.
static BK_WARNINGS_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+\s+warnings?\s+found\b").expect("buildkit warnings header"));

/// Compose completion line: `✔ Service api Built` or `Service api Built`.
static COMPOSE_SERVICE_BUILT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[✔✓]\s+)?(?:Service|Image)\s+([A-Za-z0-9._-]+)\s+Built\b")
        .expect("compose service built")
});

/// Classic Docker / Podman step header:
/// `Step 1/8 : FROM node:20-alpine` or `[1/2] STEP 1/5: FROM rust:1.83`.
static CLASSIC_STEP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\[\d+/\d+\]\s+)?(?i:step)\s+(\d+)/(\d+)\s*:\s*(.+?)\s*$")
        .expect("classic docker step")
});

/// Classic Docker failure trailer: `The command '...' returned a non-zero code: 2`.
static CLASSIC_NONZERO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^The command .+ returned a non-zero code:\s*(\d+)").expect("classic nonzero")
});

/// Podman / Buildah failure line: `Error: building at STEP "...": ...`.
static PODMAN_STEP_ERROR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^Error:\s+building at STEP\s+"#).expect("podman step error"));

/// Hex layer ID line in classic Docker / Podman: `---> 9c8a7b6e5d4f` or `--> 9c8a7b6e5d4f`.
static CLASSIC_LAYER_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^---?>\s+[0-9a-f]{12,64}\s*$").expect("classic layer id"));

/// Bare sha256 or hex image ID line (e.g. Podman final image ID).
static BARE_IMAGE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:sha256:)?[0-9a-f]{12,64}\s*$").expect("bare image id"));

/// Strong error/diagnostic anchor inside a failing `RUN` step.
static STEP_ERROR_ANCHOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?:",
        r"^\s*(?:error(?:\[[A-Za-z0-9_-]+\])?:|fatal(?: error)?:|ERROR:|FATAL[: ]|panic:|npm ERR!|ERR_PNPM_|E:\s+|CMake Error|make(?:\[\d+\])?:\s+\*\*\*)",
        r"|^\s*thread '[^']*' (?:\(\d+\) )?panicked at ",
        r"|^\s*Traceback \(most recent call last\):",
        r"|^\s*(?:[A-Za-z_][\w.]*\.)?(?:[A-Z]\w*)?(?:Error|Exception):",
        r"|:\d+(?::\d+)?:\s*(?:fatal )?error\b",
        r"|\(\d+,\s*\d+\):\s*error\s+[A-Za-z]+\d+:",
        r"|\b(?:Cannot find module|Module not found|ModuleNotFoundError|ImportError|SyntaxError|TypeError|ReferenceError|undefined reference to|command not found|No such file or directory|Permission denied|unable to locate package|unable to select packages|Failed building wheel|Could not find a version|could not compile|aborting due to|Build failed with|returned a non-zero code|exit code:\s*[1-9]\d*|exit status\s+[1-9]\d*)\b",
        r")",
    ))
    .expect("step error anchor")
});

/// Diagnostic continuation line (rustc gutter, caret underline, stack frame, note/help).
static DIAGNOSTIC_CONT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?:",
        r#"^\s*(?:-->|:::|\||\d+\s+\||=\s*(?:note|help):|note:|help:|Caused by:|at\s+|File\s+"|For more information|\^+~*|~+\^*)"#,
        r")",
    ))
    .expect("diagnostic continuation")
});

/// Workspace-relevant path indicator (prioritized when trimming tight failing-step budgets).
static WORKSPACE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s(])(?:\.?/?(?:src|app|lib|cmd|pkg|internal|tests?|packages|services|apps|crates|client|server|web|api|Dockerfile|Cargo\.toml|package\.json|pyproject\.toml|go\.mod)[/:\w.-]*)")
        .expect("workspace path")
});

/// Known Dockerfile instructions for strict classic-build validation.
const DOCKERFILE_INSTRUCTIONS: &[&str] = &[
    "FROM",
    "RUN",
    "CMD",
    "LABEL",
    "MAINTAINER",
    "EXPOSE",
    "ENV",
    "ADD",
    "COPY",
    "ENTRYPOINT",
    "VOLUME",
    "USER",
    "WORKDIR",
    "ARG",
    "ONBUILD",
    "STOPSIGNAL",
    "HEALTHCHECK",
    "SHELL",
];

/// Whether `words` invoke a supported container image build command in a
/// display-oriented mode (excluding `-q`/`--quiet`, `--progress=rawjson`,
/// `--progress=quiet`, `--help`, and `buildx bake --print`).
#[allow(dead_code)]
pub fn is_build_command(words: &[&str]) -> bool {
    matches!(classify_command(words), CommandMatch::Build)
}

/// Command-aware and content-aware detection for `docker build`, `docker buildx
/// build`, `docker compose build`, `podman build`, and classic Docker builds.
///
/// Fails closed (`false`) if `command` requests machine-formatted/quiet output
/// or if `stdout`/`stderr` do not clearly contain BuildKit or classic Docker
/// build output.
#[allow(dead_code)]
pub fn matches(command: &[String], stdout: &str, stderr: &str) -> bool {
    let refs: Vec<&str> = command.iter().map(String::as_str).collect();
    matches_words(&refs, stdout, stderr)
}

/// Slice-of-`&str` variant of [`matches`] for callers such as `capture.rs` and
/// `postroute.rs`.
#[allow(dead_code)]
pub fn matches_words(words: &[&str], stdout: &str, stderr: &str) -> bool {
    let cmd_match = classify_command(words);
    if cmd_match == CommandMatch::Disallowed {
        return false;
    }
    let cmd_is_build = cmd_match == CommandMatch::Build;
    detect_build(stdout, stderr, cmd_is_build).is_some()
}

/// Compact already-redacted BuildKit or classic Docker/Podman build output into
/// a step-oriented summary with exact `[stdout:A-B]` / `[stderr:A-B]` line
/// pointers.
///
/// Returns `None` if:
/// - `budget == 0` or input contains NUL bytes / oversized lines,
/// - the streams do not clearly match BuildKit or classic Docker build output,
/// - the rendered report cannot fit within `budget`, or
/// - the rendered report is not strictly smaller than `stdout.len() + stderr.len()`.
#[allow(dead_code)]
pub fn format(stdout: &str, stderr: &str, exit_code: Option<i32>, budget: usize) -> Option<String> {
    format_inner(stdout, stderr, exit_code, budget, false, None)
}

/// Command-aware compaction entrypoint: rejects machine/quiet build flags, uses
/// command context to tune detection and header labeling (e.g. `podman build`),
/// and fails closed on non-build output.
#[allow(dead_code)]
pub fn format_for_command(
    words: &[&str],
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    budget: usize,
) -> Option<String> {
    let cmd_match = classify_command(words);
    if cmd_match == CommandMatch::Disallowed {
        return None;
    }
    let tool_hint = command_tool_label(words);
    format_inner(
        stdout,
        stderr,
        exit_code,
        budget,
        cmd_match == CommandMatch::Build,
        tool_hint,
    )
}

fn format_inner(
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    budget: usize,
    cmd_is_build: bool,
    tool_hint: Option<&'static str>,
) -> Option<String> {
    if budget == 0 {
        return None;
    }
    let raw_len = stdout.len().saturating_add(stderr.len());
    if raw_len == 0 {
        return None;
    }
    let mut parsed = detect_build(stdout, stderr, cmd_is_build)?;
    if let Some(hint) = tool_hint {
        parsed.tool_label = hint;
    }
    let rendered = render_build(&parsed, exit_code, budget)?;
    if rendered.len() <= budget && rendered.len() < raw_len {
        Some(rendered)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandMatch {
    /// Recognized display-oriented container build command.
    Build,
    /// Recognized container build command with quiet/machine-output/help flags.
    Disallowed,
    /// Not directly a container build command (may still wrap one, e.g. `make`).
    Other,
}

fn classify_command(words: &[&str]) -> CommandMatch {
    if words.is_empty() {
        return CommandMatch::Other;
    }
    let owned = crate::wrappers::effective(words);
    let refs: Vec<&str> = match &owned {
        Some(w) => w.iter().map(String::as_str).collect(),
        None => words.to_vec(),
    };
    let unwrapped = crate::filters::unwrap_tool(&refs);
    let Some((&raw_prog, mut args)) = unwrapped.split_first() else {
        return CommandMatch::Other;
    };
    let Some(program) = Path::new(raw_prog).file_name().and_then(|p| p.to_str()) else {
        return CommandMatch::Other;
    };

    match program {
        "docker" | "podman" | "nerdctl" => {
            args = skip_container_global_opts(args);
            match args {
                ["build", tail @ ..] | ["image", "build", tail @ ..] => {
                    if has_disallowed_build_flag(tail, false) {
                        CommandMatch::Disallowed
                    } else {
                        CommandMatch::Build
                    }
                }
                ["buildx", rest @ ..] => {
                    let rest = skip_buildx_global_opts(rest);
                    match rest {
                        ["build", tail @ ..] => {
                            if has_disallowed_build_flag(tail, false) {
                                CommandMatch::Disallowed
                            } else {
                                CommandMatch::Build
                            }
                        }
                        ["bake", tail @ ..] => {
                            if has_disallowed_build_flag(tail, true) {
                                CommandMatch::Disallowed
                            } else {
                                CommandMatch::Build
                            }
                        }
                        _ => CommandMatch::Other,
                    }
                }
                ["compose", rest @ ..] => classify_compose_subcommand(rest),
                _ => CommandMatch::Other,
            }
        }
        "docker-compose" | "podman-compose" => classify_compose_subcommand(args),
        "buildah" => {
            args = skip_container_global_opts(args);
            match args {
                ["bud" | "build" | "build-using-dockerfile", tail @ ..] => {
                    if has_disallowed_build_flag(tail, false) {
                        CommandMatch::Disallowed
                    } else {
                        CommandMatch::Build
                    }
                }
                _ => CommandMatch::Other,
            }
        }
        _ => CommandMatch::Other,
    }
}

fn command_tool_label(words: &[&str]) -> Option<&'static str> {
    let owned = crate::wrappers::effective(words);
    let refs: Vec<&str> = match &owned {
        Some(w) => w.iter().map(String::as_str).collect(),
        None => words.to_vec(),
    };
    let unwrapped = crate::filters::unwrap_tool(&refs);
    let (&raw_prog, args) = unwrapped.split_first()?;
    let program = Path::new(raw_prog).file_name()?.to_str()?;
    match program {
        "podman" => {
            let args = skip_container_global_opts(args);
            if matches!(args, ["compose", ..]) {
                Some("podman compose build")
            } else {
                Some("podman build")
            }
        }
        "podman-compose" => Some("podman compose build"),
        "buildah" => Some("buildah build"),
        "nerdctl" => {
            let args = skip_container_global_opts(args);
            if matches!(args, ["compose", ..]) {
                Some("nerdctl compose build")
            } else {
                Some("nerdctl build")
            }
        }
        "docker-compose" => Some("docker compose build"),
        "docker" => {
            let args = skip_container_global_opts(args);
            match args {
                ["compose", ..] => Some("docker compose build"),
                ["buildx", ..] => Some("docker buildx build"),
                _ => Some("docker build"),
            }
        }
        _ => None,
    }
}

fn skip_container_global_opts<'a>(mut args: &'a [&'a str]) -> &'a [&'a str] {
    const VALUED: &[&str] = &[
        "-H",
        "--host",
        "-c",
        "--context",
        "--config",
        "-l",
        "--log-level",
        "--tlscacert",
        "--tlscert",
        "--tlskey",
        "--connection",
        "--url",
        "-n",
        "--namespace",
        "--address",
        "--data-root",
        "--root",
        "--runroot",
        "--storage-driver",
    ];
    const FLAGS: &[&str] = &[
        "-D",
        "--debug",
        "--tls",
        "--tlsverify",
        "--remote",
        "--rootless-cni",
    ];
    while let Some((&arg, rest)) = args.split_first() {
        let (key, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(k, v)| (k, Some(v)));
        if VALUED.contains(&key) {
            if inline.is_some() {
                args = rest;
            } else if rest.first().is_some_and(|v| !v.starts_with('-')) {
                args = &rest[1..];
            } else {
                break;
            }
        } else if FLAGS.contains(&arg) {
            args = rest;
        } else {
            break;
        }
    }
    args
}

fn skip_buildx_global_opts<'a>(mut args: &'a [&'a str]) -> &'a [&'a str] {
    while let Some((&arg, rest)) = args.split_first() {
        if arg == "--builder" && rest.first().is_some_and(|v| !v.starts_with('-')) {
            args = &rest[1..];
        } else if arg.starts_with("--builder=") || arg == "--debug" || arg == "-D" {
            args = rest;
        } else {
            break;
        }
    }
    args
}

fn classify_compose_subcommand(mut args: &[&str]) -> CommandMatch {
    const VALUED: &[&str] = &[
        "-f",
        "--file",
        "-p",
        "--project-name",
        "--profile",
        "--env-file",
        "--project-directory",
        "--ansi",
        "--progress",
        "--parallel",
    ];
    const FLAGS: &[&str] = &["--compatibility", "--dry-run"];
    while let Some((&arg, rest)) = args.split_first() {
        let (key, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(k, v)| (k, Some(v)));
        if key == "--progress" {
            let val = inline.or_else(|| rest.first().copied());
            if matches!(val, Some("quiet" | "rawjson")) {
                return CommandMatch::Disallowed;
            }
        }
        if VALUED.contains(&key) {
            if inline.is_some() {
                args = rest;
            } else if rest.first().is_some_and(|v| !v.starts_with('-')) {
                args = &rest[1..];
            } else {
                break;
            }
        } else if FLAGS.contains(&arg) {
            args = rest;
        } else {
            break;
        }
    }
    match args {
        ["build", tail @ ..] => {
            if has_disallowed_build_flag(tail, false) {
                CommandMatch::Disallowed
            } else {
                CommandMatch::Build
            }
        }
        ["up", tail @ ..] if tail.contains(&"--build") => {
            if has_disallowed_build_flag(tail, false) {
                CommandMatch::Disallowed
            } else {
                CommandMatch::Build
            }
        }
        _ => CommandMatch::Other,
    }
}

fn has_disallowed_build_flag(args: &[&str], is_bake: bool) -> bool {
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        let (flag, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(f, v)| (f, Some(v)));
        match flag {
            "-q" | "--quiet" | "-h" | "--help" => return true,
            "--print" if is_bake => return true,
            "--progress" => {
                let val = inline.or_else(|| iter.next());
                if matches!(val, Some("rawjson" | "quiet")) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum StreamKind {
    Stdout,
    Stderr,
}

impl StreamKind {
    fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LineRange {
    stream: StreamKind,
    first: usize,
    last: usize,
}

impl LineRange {
    fn single(stream: StreamKind, line: usize) -> Self {
        Self {
            stream,
            first: line,
            last: line,
        }
    }

    fn fmt_bracketed(self) -> String {
        if self.first == self.last {
            format!("[{}:{}]", self.stream.name(), self.first)
        } else {
            format!("[{}:{}-{}]", self.stream.name(), self.first, self.last)
        }
    }
}

/// Multi-stream range accumulator that renders `[stderr:A-B]` or `[stdout:A-B, stderr:C-D]`.
#[derive(Clone, Debug, Default)]
struct MultiRange {
    by_stream: BTreeMap<StreamKind, (usize, usize)>,
}

impl MultiRange {
    fn record(&mut self, stream: StreamKind, line: usize) {
        self.by_stream
            .entry(stream)
            .and_modify(|(first, last)| {
                if line < *first {
                    *first = line;
                }
                if line > *last {
                    *last = line;
                }
            })
            .or_insert((line, line));
    }

    fn merge(&mut self, other: &Self) {
        for (&stream, &(first, last)) in &other.by_stream {
            self.record(stream, first);
            self.record(stream, last);
        }
    }

    fn is_empty(&self) -> bool {
        self.by_stream.is_empty()
    }

    fn fmt_bracketed(&self) -> String {
        let mut out = String::from("[");
        for (i, (stream, (first, last))) in self.by_stream.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            if first == last {
                let _ = write!(out, "{}:{first}", stream.name());
            } else {
                let _ = write!(out, "{}:{first}-{last}", stream.name());
            }
        }
        out.push(']');
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StepKind {
    Internal,
    Export,
    Run,
    StageStep,
}

#[derive(Clone, Debug, PartialEq)]
enum StepStatus {
    Running,
    Cached,
    Done(Option<f64>),
    Error {
        message: String,
        stream: StreamKind,
        line_no: usize,
    },
    Canceled,
}

#[derive(Clone, Debug)]
struct StepOutputLine<'a> {
    stream: StreamKind,
    line_no: usize,
    text: Cow<'a, str>,
}

#[derive(Clone, Debug)]
struct DockerfileSnippet {
    location: String,
    highlighted: Option<String>,
    stream: StreamKind,
    line_no: usize,
}

#[derive(Clone, Debug)]
struct Step<'a> {
    id: u32,
    classic_label: Option<String>,
    header: String,
    service: Option<String>,
    kind: StepKind,
    status: StepStatus,
    range: MultiRange,
    output: Vec<StepOutputLine<'a>>,
    dockerfile: Option<DockerfileSnippet>,
    order: usize,
}

impl Step<'_> {
    fn display_id(&self) -> String {
        if let Some(label) = &self.classic_label {
            label.clone()
        } else if self.id > 0 {
            format!("#{}", self.id)
        } else {
            String::new()
        }
    }

    fn is_failed(&self) -> bool {
        matches!(self.status, StepStatus::Error { .. })
    }

    fn is_cached(&self) -> bool {
        matches!(self.status, StepStatus::Cached)
    }

    fn duration_secs(&self) -> Option<f64> {
        match self.status {
            StepStatus::Done(d) => d,
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct ExportedImage {
    service: Option<String>,
    names: Vec<String>,
    digest: Option<String>,
    range: MultiRange,
}

#[derive(Clone, Debug)]
struct BuildWarning {
    text: String,
    stream: StreamKind,
    line_no: usize,
}

#[derive(Clone, Debug)]
struct FatalLine {
    text: String,
    stream: StreamKind,
    line_no: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BuildFormat {
    BuildKit,
    Classic,
}

struct ParsedBuild<'a> {
    format: BuildFormat,
    tool_label: &'static str,
    steps: Vec<Step<'a>>,
    images: Vec<ExportedImage>,
    warnings: Vec<BuildWarning>,
    services: Vec<String>,
    wall_duration_secs: Option<f64>,
    fatal_lines: Vec<FatalLine>,
    unmatched_dockerfile: Option<DockerfileSnippet>,
}

impl ParsedBuild<'_> {
    fn has_failure(&self, exit_code: Option<i32>) -> bool {
        exit_code.is_some_and(|c| c != 0)
            || self.steps.iter().any(Step::is_failed)
            || !self.fatal_lines.is_empty()
    }
}

/// Strip ANSI escape codes (guarded by a zero-alloc fast path when no ESC byte is present)
/// and collapse bare `\r` progress overwrites to their final non-empty frame without
/// trimming leading spaces from the payload.
fn clean_line(raw: &str) -> Cow<'_, str> {
    if !raw.contains('\x1b') {
        let trimmed = raw.trim_end_matches('\n');
        let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
        if !trimmed.contains('\r') {
            return Cow::Borrowed(trimmed);
        }
        let last = trimmed
            .rsplit('\r')
            .find(|f| !f.trim().is_empty())
            .unwrap_or_else(|| trimmed.rsplit('\r').next().unwrap_or(trimmed));
        return Cow::Borrowed(last);
    }
    let no_ansi = crate::capture::ANSI.replace_all(raw, "");
    let trimmed = no_ansi.trim_end_matches('\n');
    let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
    if !trimmed.contains('\r') {
        return Cow::Owned(trimmed.to_owned());
    }
    let last = trimmed
        .rsplit('\r')
        .find(|f| !f.trim().is_empty())
        .unwrap_or_else(|| trimmed.rsplit('\r').next().unwrap_or(trimmed));
    Cow::Owned(last.to_owned())
}

fn split_stream_lines<'a>(
    stream: StreamKind,
    text: &'a str,
) -> Option<Vec<(StreamKind, usize, Cow<'a, str>)>> {
    if text.contains('\0') {
        return None;
    }
    let mut lines = Vec::new();
    for (idx, raw) in text.split_inclusive('\n').enumerate() {
        if raw.len() > MAX_LINE_BYTES {
            return None;
        }
        lines.push((stream, idx + 1, clean_line(raw)));
    }
    Some(lines)
}

fn detect_build<'a>(
    stdout: &'a str,
    stderr: &'a str,
    cmd_is_build: bool,
) -> Option<ParsedBuild<'a>> {
    let stdout_lines = split_stream_lines(StreamKind::Stdout, stdout)?;
    let stderr_lines = split_stream_lines(StreamKind::Stderr, stderr)?;

    if stdout_lines.is_empty() && stderr_lines.is_empty() {
        return None;
    }

    // Try BuildKit first (preferred because BuildKit can occasionally quote classic text inside RUN).
    if let Some(parsed) = parse_buildkit(&stdout_lines, &stderr_lines, cmd_is_build) {
        return Some(parsed);
    }

    // Fall back to classic Docker / Podman build.
    parse_classic(&stdout_lines, &stderr_lines, cmd_is_build)
}

// ============================================================================
// BuildKit Parser
// ============================================================================

struct RepeatBlock<'a> {
    header: String,
    lines: Vec<StepOutputLine<'a>>,
}

fn parse_buildkit<'a>(
    stdout_lines: &[(StreamKind, usize, Cow<'a, str>)],
    stderr_lines: &[(StreamKind, usize, Cow<'a, str>)],
    cmd_is_build: bool,
) -> Option<ParsedBuild<'a>> {
    // Determine which stream(s) carry BuildKit `#N` step lines.
    let stdout_bk = count_buildkit_signals(stdout_lines);
    let stderr_bk = count_buildkit_signals(stderr_lines);

    let total_step_lines = stdout_bk.step_lines + stderr_bk.step_lines;
    let total_strong = stdout_bk.strong_markers + stderr_bk.strong_markers;
    let total_lifecycle = stdout_bk.lifecycle_markers + stderr_bk.lifecycle_markers;

    if cmd_is_build {
        if total_step_lines < 2 || total_strong < 1 {
            return None;
        }
    } else if total_step_lines < 3 || total_strong < 2 || total_lifecycle < 1 {
        return None;
    }

    // Order streams: primary BuildKit stream first (usually stderr), then the other stream.
    let ordered_streams: [&[(StreamKind, usize, Cow<'a, str>)]; 2] =
        if stderr_bk.step_lines >= stdout_bk.step_lines {
            [stderr_lines, stdout_lines]
        } else {
            [stdout_lines, stderr_lines]
        };

    let mut steps: Vec<Step> = Vec::new();
    let mut step_index: BTreeMap<u32, usize> = BTreeMap::new();
    let mut images_by_step: BTreeMap<u32, ExportedImage> = BTreeMap::new();
    let mut standalone_images: Vec<ExportedImage> = Vec::new();
    let mut warnings: Vec<BuildWarning> = Vec::new();
    let mut warning_seen: HashSet<String> = HashSet::new();
    let mut fatal_lines: Vec<FatalLine> = Vec::new();
    let mut repeat_blocks: Vec<RepeatBlock> = Vec::new();
    let mut dockerfile_snippets: Vec<DockerfileSnippet> = Vec::new();
    let mut confirmed_services: Vec<String> = Vec::new();
    let mut confirmed_service_set: HashSet<String> = HashSet::new();
    let mut wall_duration_secs: Option<f64> = None;
    let mut is_compose = false;
    let mut next_order = 0usize;

    let mut primary_non_empty = 0usize;
    let mut primary_unrecognized = 0usize;
    let mut secondary_unrecognized = 0usize;

    for (stream_rank, lines) in ordered_streams.iter().enumerate() {
        let is_primary = stream_rank == 0;
        let mut i = 0usize;
        let mut in_warnings_block = false;

        while i < lines.len() {
            let (stream, line_no, ref line) = lines[i];
            let trimmed = line.trim();
            if trimmed.is_empty() {
                in_warnings_block = false;
                i += 1;
                continue;
            }
            if is_primary {
                primary_non_empty += 1;
            }

            // 1. BuildKit repeated error echo block: `------` followed by ` > ...:`
            if is_dashed_rule(trimmed, 6)
                && let Some(next_idx) = next_non_empty(lines, i + 1)
                && let Some(block_header) = lines[next_idx].2.trim().strip_prefix("> ")
            {
                let header = block_header.trim_end_matches(':').trim().to_owned();
                let mut block_lines = Vec::new();
                let mut j = next_idx + 1;
                while j < lines.len() {
                    let (b_stream, b_no, ref b_line) = lines[j];
                    let b_trim = b_line.trim();
                    if is_dashed_rule(b_trim, 6) {
                        j += 1;
                        break;
                    }
                    if !b_trim.is_empty() {
                        block_lines.push(StepOutputLine {
                            stream: b_stream,
                            line_no: b_no,
                            text: strip_repeat_echo_prefix(b_line),
                        });
                    }
                    j += 1;
                }
                repeat_blocks.push(RepeatBlock {
                    header,
                    lines: block_lines,
                });
                i = j;
                continue;
            }

            // 2. Dockerfile snippet block: `Dockerfile:14` followed by `--------------------`
            if DOCKERFILE_LOC.is_match(trimmed)
                && let Some(next_idx) = next_non_empty(lines, i + 1)
                && is_dashed_rule(lines[next_idx].2.trim(), 10)
            {
                let loc = trimmed.to_owned();
                let mut highlighted: Option<(String, StreamKind, usize)> = None;
                let mut j = next_idx + 1;
                while j < lines.len() {
                    let (d_stream, d_no, ref d_line) = lines[j];
                    let d_trim = d_line.trim();
                    if is_dashed_rule(d_trim, 10) {
                        j += 1;
                        break;
                    }
                    if let Some((_, rhs)) = d_trim.split_once("| >>>") {
                        highlighted = Some((rhs.trim().to_owned(), d_stream, d_no));
                    }
                    j += 1;
                }
                let (hl_text, hl_stream, hl_line) = match highlighted {
                    Some((text, s, n)) => (Some(text), s, n),
                    None => (None, stream, line_no),
                };
                dockerfile_snippets.push(DockerfileSnippet {
                    location: loc,
                    highlighted: hl_text,
                    stream: hl_stream,
                    line_no: hl_line,
                });
                i = j;
                continue;
            }

            // 3. Buildx `N warnings found` trailing block
            if BK_WARNINGS_HEADER.is_match(trimmed) {
                in_warnings_block = true;
                i += 1;
                continue;
            }
            if in_warnings_block && let Some(item) = trimmed.strip_prefix("- ") {
                record_warning(
                    &mut warnings,
                    &mut warning_seen,
                    item.trim(),
                    stream,
                    line_no,
                );
                i += 1;
                continue;
            }
            in_warnings_block = false;

            // 4. `[+] Building 14.3s (16/16) FINISHED` banner
            if let Some(caps) = BK_BANNER.captures(trimmed) {
                if let Some(dur) = parse_duration_secs(&caps[1]) {
                    wall_duration_secs = Some(dur);
                }
                i += 1;
                continue;
            }

            // 5. Compose service built summary: `✔ Service api Built`
            if let Some(caps) = COMPOSE_SERVICE_BUILT.captures(trimmed) {
                is_compose = true;
                let svc = caps[1].to_owned();
                if confirmed_service_set.insert(svc.clone()) {
                    confirmed_services.push(svc);
                }
                i += 1;
                continue;
            }

            // 6. BuildKit `#N ...` step line (zero-alloc byte parser).
            if let Some((id, raw_after_id)) = parse_bk_step_line(line) {
                let rest = raw_after_id.trim_end();

                if id == 0 {
                    // `#0 building with ...` preamble; capture WARN/ERROR if any.
                    if let Some(warn) = strip_warning_prefix(rest) {
                        record_warning(&mut warnings, &mut warning_seen, warn, stream, line_no);
                    } else if let Some(err) = rest.strip_prefix("ERROR:") {
                        fatal_lines.push(FatalLine {
                            text: format!("ERROR: {}", err.trim()),
                            stream,
                            line_no,
                        });
                    }
                    i += 1;
                    continue;
                }

                let idx = if let Some(&existing) = step_index.get(&id) {
                    existing
                } else {
                    let idx = steps.len();
                    step_index.insert(id, idx);
                    steps.push(Step {
                        id,
                        classic_label: None,
                        header: String::new(),
                        service: None,
                        kind: StepKind::StageStep,
                        status: StepStatus::Running,
                        range: MultiRange::default(),
                        output: Vec::new(),
                        dockerfile: None,
                        order: next_order,
                    });
                    next_order += 1;
                    idx
                };

                steps[idx].range.record(stream, line_no);

                // Classify step line payload without allocating when skipping CACHED/DONE/progress lines.
                if rest == "CACHED" {
                    steps[idx].status = StepStatus::Cached;
                } else if rest == "CANCELED" {
                    steps[idx].status = StepStatus::Canceled;
                } else if rest == "DONE" || rest.starts_with("DONE ") {
                    let dur_str = rest.strip_prefix("DONE").unwrap_or("").trim();
                    let dur = if dur_str.is_empty() {
                        None
                    } else {
                        parse_duration_secs(dur_str)
                    };
                    if !matches!(steps[idx].status, StepStatus::Error { .. }) {
                        steps[idx].status = StepStatus::Done(dur);
                    }
                } else if let Some(err_msg) = rest.strip_prefix("ERROR:") {
                    steps[idx].status = StepStatus::Error {
                        message: err_msg.trim().to_owned(),
                        stream,
                        line_no,
                    };
                } else if let Some(warn_msg) = strip_warning_prefix(rest) {
                    record_warning(&mut warnings, &mut warning_seen, warn_msg, stream, line_no);
                } else if let Some(out_cow) = extract_bk_output_cow(line) {
                    // Explicit `WARN:` inside step output is also surfaced if uppercase.
                    let out_trim = out_cow.trim();
                    if let Some(warn_msg) = strip_explicit_uppercase_warn(out_trim) {
                        record_warning(&mut warnings, &mut warning_seen, warn_msg, stream, line_no);
                    }
                    steps[idx].output.push(StepOutputLine {
                        stream,
                        line_no,
                        text: out_cow,
                    });
                } else if steps[idx].header.is_empty() && !is_bk_metadata_subline(rest) {
                    let header = rest.trim().to_owned();
                    let (kind, confirmed_svc) = classify_bk_header(&header);
                    if let Some(svc) = confirmed_svc {
                        is_compose = true;
                        if confirmed_service_set.insert(svc.clone()) {
                            confirmed_services.push(svc.clone());
                        }
                        steps[idx].service = Some(svc);
                    }
                    steps[idx].kind = kind;
                    steps[idx].header = header;
                } else {
                    // Metadata/export subline: capture image digest & tag names!
                    observe_bk_export_line(
                        id,
                        steps[idx].service.as_deref(),
                        rest,
                        stream,
                        line_no,
                        &mut images_by_step,
                    );
                }

                i += 1;
                continue;
            }

            // 7. Build-level warnings / deprecations (e.g. `WARN[0000] ...`, `DEPRECATED: ...`)
            if let Some(warn_msg) = strip_build_level_warning(trimmed) {
                record_warning(&mut warnings, &mut warning_seen, warn_msg, stream, line_no);
                i += 1;
                continue;
            }

            // 8. Build-level fatal error lines (`ERROR: failed to solve: ...`, `executor failed...`)
            //    plus immediately following indented detail lines.
            if is_bk_fatal_line(trimmed) {
                fatal_lines.push(FatalLine {
                    text: trimmed.to_owned(),
                    stream,
                    line_no,
                });
                i += 1;
                let mut detail_count = 0usize;
                while i < lines.len() && detail_count < 4 {
                    let (next_stream, next_line_no, ref next_raw) = lines[i];
                    let next_trimmed = next_raw.trim();
                    if next_stream != stream
                        || next_trimmed.is_empty()
                        || !next_raw.starts_with([' ', '\t'])
                        || next_trimmed.starts_with('#')
                        || next_trimmed.starts_with("------")
                    {
                        break;
                    }
                    fatal_lines.push(FatalLine {
                        text: next_trimmed.to_owned(),
                        stream: next_stream,
                        line_no: next_line_no,
                    });
                    i += 1;
                    detail_count += 1;
                }
                continue;
            }

            // 9. Standalone exported image / compose progress lines (`[+] Building ...`, bare sha256)
            if BARE_IMAGE_ID.is_match(trimmed) {
                let mut range = MultiRange::default();
                range.record(stream, line_no);
                standalone_images.push(ExportedImage {
                    service: None,
                    names: Vec::new(),
                    digest: Some(trimmed.to_owned()),
                    range,
                });
                i += 1;
                continue;
            }
            if trimmed.starts_with("=>") || trimmed.starts_with("Building ") {
                i += 1;
                continue;
            }

            // Unrecognized line.
            if is_primary {
                primary_unrecognized += 1;
            } else {
                secondary_unrecognized += 1;
            }
            i += 1;
        }
    }

    // Fail closed if too much unrecognized content exists (avoids dropping non-build output).
    if steps.is_empty() {
        return None;
    }
    if primary_non_empty > 0 && primary_unrecognized * 4 > primary_non_empty {
        return None;
    }
    if secondary_unrecognized > 5 {
        return None;
    }

    // Assign confirmed compose services to any step whose bracket starts with that service.
    if !confirmed_services.is_empty() {
        for step in &mut steps {
            if step.service.is_none()
                && let Some(first_tok) = first_bracket_token(&step.header)
                && confirmed_service_set.contains(first_tok)
            {
                step.service = Some(first_tok.to_owned());
            }
        }
    }

    // Reconcile `------` / ` > [stage] ...:` repeat blocks with parsed steps:
    // - If a matching step already has those lines, deduplicate (drop echo).
    // - If a matching step lacks output lines (or has non-duplicate lines), attach them.
    // - If no step matched (partial log), synthesize a failing step so errors are never lost.
    for block in repeat_blocks {
        let matched_idx = find_step_by_header(&steps, &block.header).or_else(|| {
            // Fallback for truncated logs where the `#N <header>` line was cut off before
            // the step's output lines: match a failing step with an empty header whose
            // output overlaps this `------` block.
            let block_texts: HashSet<&str> = block
                .lines
                .iter()
                .map(|l| l.text.trim())
                .filter(|t| !t.is_empty())
                .collect();
            steps.iter().position(|s| {
                s.is_failed()
                    && s.header.is_empty()
                    && (block_texts.is_empty()
                        || s.output.iter().any(|l| block_texts.contains(l.text.trim())))
            })
        });
        if let Some(idx) = matched_idx {
            let step = &mut steps[idx];
            if step.header.is_empty() && !block.header.is_empty() {
                let (kind, svc) = classify_bk_header(&block.header);
                step.kind = kind;
                if step.service.is_none() {
                    step.service = svc;
                }
                step.header = block.header;
            }
            if !step.is_failed() && !fatal_lines.is_empty() {
                // Ensure the echoed failing step is marked failed even if `#N ERROR:` was clipped.
                if let Some(first) = block.lines.first() {
                    step.status = StepStatus::Error {
                        message: fatal_lines
                            .first()
                            .map_or_else(|| "step failed".to_owned(), |f| f.text.clone()),
                        stream: first.stream,
                        line_no: first.line_no,
                    };
                }
            }
            if step.output.is_empty() {
                for line in block.lines {
                    step.range.record(line.stream, line.line_no);
                    step.output.push(line);
                }
            } else {
                // Only add lines genuinely absent from the step's already-recorded output.
                let existing: HashSet<&str> = step.output.iter().map(|l| l.text.trim()).collect();
                let missing: Vec<StepOutputLine> = block
                    .lines
                    .into_iter()
                    .filter(|l| !l.text.trim().is_empty() && !existing.contains(l.text.trim()))
                    .collect();
                if !missing.is_empty() {
                    for line in missing {
                        step.range.record(line.stream, line.line_no);
                        step.output.push(line);
                    }
                }
            }
        } else if !block.header.is_empty() {
            let mut range = MultiRange::default();
            for line in &block.lines {
                range.record(line.stream, line.line_no);
            }
            let (err_stream, err_line) = block
                .lines
                .last()
                .map_or((StreamKind::Stderr, 1), |l| (l.stream, l.line_no));
            steps.push(Step {
                id: 0,
                classic_label: None,
                header: block.header,
                service: None,
                kind: StepKind::Run,
                status: StepStatus::Error {
                    message: fatal_lines
                        .first()
                        .map_or_else(|| "step failed".to_owned(), |f| f.text.clone()),
                    stream: err_stream,
                    line_no: err_line,
                },
                range,
                output: block.lines,
                dockerfile: None,
                order: next_order,
            });
            next_order += 1;
        }
    }

    // Attach Dockerfile snippet(s) to failing step(s).
    let mut unmatched_dockerfile = None;
    let failed_indices: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, s)| s.is_failed())
        .map(|(i, _)| i)
        .collect();
    for (k, snippet) in dockerfile_snippets.into_iter().enumerate() {
        if let Some(&step_idx) = failed_indices.get(k) {
            steps[step_idx].dockerfile = Some(snippet);
        } else if unmatched_dockerfile.is_none() {
            unmatched_dockerfile = Some(snippet);
        }
    }

    // Collect exported images (attaching service names from their export step if resolved).
    let mut images: Vec<ExportedImage> = Vec::new();
    for (step_id, mut img) in images_by_step {
        if img.service.is_none()
            && let Some(&idx) = step_index.get(&step_id)
        {
            img.service = steps[idx].service.clone();
        }
        if !img.names.is_empty() || img.digest.is_some() {
            images.push(img);
        }
    }
    if images.is_empty() {
        images.extend(standalone_images);
    }

    let tool_label = if is_compose || confirmed_services.len() > 1 {
        "docker compose build"
    } else {
        "docker build"
    };

    Some(ParsedBuild {
        format: BuildFormat::BuildKit,
        tool_label,
        steps,
        images,
        warnings,
        services: confirmed_services,
        wall_duration_secs,
        fatal_lines,
        unmatched_dockerfile,
    })
}

struct BkSignalCounts {
    step_lines: usize,
    strong_markers: usize,
    lifecycle_markers: usize,
}

fn count_buildkit_signals(lines: &[(StreamKind, usize, Cow<'_, str>)]) -> BkSignalCounts {
    let mut counts = BkSignalCounts {
        step_lines: 0,
        strong_markers: 0,
        lifecycle_markers: 0,
    };
    for (_, _, line) in lines {
        let Some((_, raw_after_id)) = parse_bk_step_line(line) else {
            continue;
        };
        counts.step_lines += 1;
        let rest = raw_after_id.trim_end();
        if rest == "CACHED"
            || rest == "CANCELED"
            || rest == "DONE"
            || rest.starts_with("DONE ")
            || rest.starts_with("ERROR:")
        {
            counts.strong_markers += 1;
            counts.lifecycle_markers += 1;
        } else if is_bk_strong_header(rest)
            || rest.starts_with("transferring dockerfile:")
            || rest.starts_with("transferring context:")
            || rest.starts_with("writing image sha256:")
            || rest.starts_with("naming to ")
            || rest.starts_with("exporting layers")
            || rest.starts_with("building with \"")
        {
            counts.strong_markers += 1;
            if rest.contains("internal]") || rest.contains("exporting to ") {
                counts.lifecycle_markers += 1;
            }
        }
    }
    counts
}

fn is_bk_strong_header(rest: &str) -> bool {
    if rest.starts_with("exporting to ")
        || rest.starts_with("exporting cache")
        || rest.starts_with("resolving provenance")
        || rest.starts_with("[internal] ")
        || rest.starts_with("[auth] ")
    {
        return true;
    }
    if let Some(after_open) = rest.strip_prefix('[')
        && let Some((inside, after_close)) = after_open.split_once(']')
    {
        let after = after_close.trim_start();
        if inside == "internal" || inside.ends_with(" internal") {
            return true;
        }
        if after.starts_with("exporting to ") || after.starts_with("resolving provenance") {
            return true;
        }
        let has_step_ratio = inside.split_whitespace().last().is_some_and(is_step_ratio);
        if has_step_ratio && starts_with_dockerfile_instruction(after) {
            return true;
        }
    }
    false
}

fn is_step_ratio(tok: &str) -> bool {
    let Some((a, b)) = tok.split_once('/') else {
        return false;
    };
    !a.is_empty()
        && !b.is_empty()
        && a.bytes().all(|c| c.is_ascii_digit())
        && b.bytes().all(|c| c.is_ascii_digit())
}

fn starts_with_dockerfile_instruction(text: &str) -> bool {
    let first = text.split_whitespace().next().unwrap_or("");
    DOCKERFILE_INSTRUCTIONS
        .iter()
        .any(|kw| first.eq_ignore_ascii_case(kw))
}

/// Zero-alloc parser for BuildKit `#<id>[ <rest>]` lines.
fn parse_bk_step_line(line: &str) -> Option<(u32, &str)> {
    let s = line.trim_start();
    let after_hash = s.strip_prefix('#')?;
    let digits = after_hash.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let id: u32 = after_hash[..digits].parse().ok()?;
    let tail = &after_hash[digits..];
    if tail.is_empty() {
        return Some((id, ""));
    }
    let after_sep = tail.strip_prefix(' ').or_else(|| tail.strip_prefix('\t'))?;
    Some((id, after_sep))
}

/// Zero-alloc parser for BuildKit elapsed-time step output `<secs>.<frac>[ <text>]`.
fn parse_bk_output_payload(raw_after_id: &str) -> Option<&str> {
    let bytes = raw_after_id.as_bytes();
    let d1 = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if d1 == 0 || bytes.get(d1) != Some(&b'.') {
        return None;
    }
    let d2 = bytes[d1 + 1..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if d2 == 0 {
        return None;
    }
    let tail = &raw_after_id[d1 + 1 + d2..];
    if tail.is_empty() {
        Some("")
    } else {
        tail.strip_prefix(' ')
    }
}

/// Extract a BuildKit step output payload as a zero-alloc `Cow::Borrowed` slice
/// whenever `line` itself is borrowed from the input stream.
fn extract_bk_output_cow<'a>(line: &Cow<'a, str>) -> Option<Cow<'a, str>> {
    match line {
        Cow::Borrowed(s) => {
            let (_, after_id) = parse_bk_step_line(s)?;
            let payload = parse_bk_output_payload(after_id)?;
            Some(Cow::Borrowed(payload))
        }
        Cow::Owned(s) => {
            let (_, after_id) = parse_bk_step_line(s)?;
            let payload = parse_bk_output_payload(after_id)?;
            Some(Cow::Owned(payload.to_owned()))
        }
    }
}

fn is_bk_metadata_subline(rest: &str) -> bool {
    [
        "transferring ",
        "resolve ",
        "sha256:",
        "extracting sha256:",
        "exporting layers",
        "exporting manifest",
        "exporting config",
        "exporting attestation",
        "writing image ",
        "naming to ",
        "unpacking to ",
        "sending tarball",
        "pushing layers",
        "pushing manifest",
        "importing cache",
        "preparing build cache",
        "merging ",
    ]
    .iter()
    .any(|p| rest.starts_with(p))
}

fn classify_bk_header(header: &str) -> (StepKind, Option<String>) {
    if header.starts_with("exporting to ")
        || header.starts_with("exporting cache")
        || header.starts_with("exporting layers")
    {
        return (StepKind::Export, None);
    }
    if header.starts_with("resolving provenance")
        || header.starts_with("load build definition")
        || header.starts_with("load metadata")
        || header.starts_with("load .dockerignore")
        || header.starts_with("load build context")
    {
        return (StepKind::Internal, None);
    }

    if let Some(after_open) = header.strip_prefix('[')
        && let Some((inside, after_close)) = after_open.split_once(']')
    {
        let after = after_close.trim_start();
        let tokens: Vec<&str> = inside.split_whitespace().collect();

        // `[internal]` or `[api internal]`
        if tokens.last() == Some(&"internal") {
            let svc = tokens
                .iter()
                .take(tokens.len().saturating_sub(1))
                .find(|t| !is_platform_token(t))
                .map(|s| (*s).to_owned());
            return (StepKind::Internal, svc);
        }
        // `[auth]` or `[api auth]`
        if tokens.last() == Some(&"auth") {
            let svc = tokens
                .iter()
                .take(tokens.len().saturating_sub(1))
                .find(|t| !is_platform_token(t))
                .map(|s| (*s).to_owned());
            return (StepKind::Internal, svc);
        }
        // `[api] exporting to image` / `[api] resolving provenance`
        if after.starts_with("exporting to ") || after.starts_with("exporting cache") {
            let svc =
                (tokens.len() == 1 && !is_platform_token(tokens[0])).then(|| tokens[0].to_owned());
            return (StepKind::Export, svc);
        }
        if after.starts_with("resolving provenance") {
            let svc =
                (tokens.len() == 1 && !is_platform_token(tokens[0])).then(|| tokens[0].to_owned());
            return (StepKind::Internal, svc);
        }

        // `[api builder 2/6] RUN ...` -> 2+ non-platform tokens before `M/K` confirms Compose service!
        let confirmed_svc = if tokens.last().is_some_and(|t| is_step_ratio(t)) {
            let non_platform: Vec<&str> = tokens[..tokens.len() - 1]
                .iter()
                .copied()
                .filter(|t| !is_platform_token(t))
                .collect();
            if non_platform.len() >= 2 {
                Some(non_platform[0].to_owned())
            } else {
                None
            }
        } else {
            None
        };

        let kind = if after.starts_with("RUN ") || after == "RUN" {
            StepKind::Run
        } else {
            StepKind::StageStep
        };
        return (kind, confirmed_svc);
    }

    if header.starts_with("RUN ") {
        (StepKind::Run, None)
    } else {
        (StepKind::StageStep, None)
    }
}

fn is_platform_token(tok: &str) -> bool {
    tok.starts_with("linux/")
        || tok.starts_with("windows/")
        || tok.starts_with("darwin/")
        || tok.starts_with("freebsd/")
}

fn first_bracket_token(header: &str) -> Option<&str> {
    let inside = header.strip_prefix('[')?.split_once(']')?.0;
    let tok = inside.split_whitespace().next()?;
    (!is_platform_token(tok) && !is_step_ratio(tok) && tok != "internal" && tok != "auth")
        .then_some(tok)
}

fn observe_bk_export_line(
    step_id: u32,
    service: Option<&str>,
    rest: &str,
    stream: StreamKind,
    line_no: usize,
    images_by_step: &mut BTreeMap<u32, ExportedImage>,
) {
    if let Some(after) = rest.strip_prefix("writing image ") {
        let digest = after.split_whitespace().next().unwrap_or("").trim();
        if !digest.is_empty() {
            let entry = images_by_step
                .entry(step_id)
                .or_insert_with(|| ExportedImage {
                    service: service.map(str::to_owned),
                    names: Vec::new(),
                    digest: None,
                    range: MultiRange::default(),
                });
            entry.digest = Some(digest.to_owned());
            entry.range.record(stream, line_no);
        }
    } else if let Some(after) = rest
        .strip_prefix("naming to ")
        .or_else(|| rest.strip_prefix("unpacking to "))
    {
        let name = strip_done_timing_suffix(after);
        if !name.is_empty() {
            let entry = images_by_step
                .entry(step_id)
                .or_insert_with(|| ExportedImage {
                    service: service.map(str::to_owned),
                    names: Vec::new(),
                    digest: None,
                    range: MultiRange::default(),
                });
            if !entry.names.iter().any(|n| n == name) {
                entry.names.push(name.to_owned());
            }
            entry.range.record(stream, line_no);
        }
    }
}

fn strip_done_timing_suffix(text: &str) -> &str {
    let mut s = text.trim();
    if let Some(before) = s.strip_suffix(" done") {
        s = before.trim_end();
        // Strip trailing timing like `0.0s` or `1.2s`.
        if let Some((head, last)) = s.rsplit_once(' ')
            && parse_duration_secs(last).is_some()
        {
            s = head.trim_end();
        }
    }
    s
}

fn strip_repeat_echo_prefix<'a>(line: &Cow<'a, str>) -> Cow<'a, str> {
    // Strip optional `#N ` and optional `<float> ` prefix inside `------` echo block.
    match line {
        Cow::Borrowed(s) => {
            let after_hash = parse_bk_step_line(s).map_or(*s, |(_, rest)| rest);
            Cow::Borrowed(parse_bk_output_payload(after_hash).unwrap_or(after_hash))
        }
        Cow::Owned(s) => {
            let after_hash = parse_bk_step_line(s).map_or(s.as_str(), |(_, rest)| rest);
            Cow::Owned(
                parse_bk_output_payload(after_hash)
                    .unwrap_or(after_hash)
                    .to_owned(),
            )
        }
    }
}

fn find_step_by_header(steps: &[Step<'_>], header: &str) -> Option<usize> {
    let needle = header.trim();
    if needle.is_empty() {
        return None;
    }
    // Prefer failed step exact match first.
    if let Some(pos) = steps
        .iter()
        .position(|s| s.is_failed() && s.header.trim() == needle)
    {
        return Some(pos);
    }
    if let Some(pos) = steps.iter().position(|s| s.header.trim() == needle) {
        return Some(pos);
    }
    // Fallback: match when BuildKit truncated long command in header or echo.
    steps.iter().position(|s| {
        let h = s.header.trim();
        !h.is_empty() && (h.starts_with(needle) || needle.starts_with(h))
    })
}

fn is_dashed_rule(trimmed: &str, min_len: usize) -> bool {
    trimmed.len() >= min_len && trimmed.bytes().all(|b| b == b'-')
}

fn next_non_empty(lines: &[(StreamKind, usize, Cow<'_, str>)], mut idx: usize) -> Option<usize> {
    while idx < lines.len() {
        if !lines[idx].2.trim().is_empty() {
            return Some(idx);
        }
        idx += 1;
    }
    None
}

fn strip_warning_prefix(rest: &str) -> Option<&str> {
    if rest.starts_with("WARN:") || rest.starts_with("WARNING:") || rest.starts_with("DEPRECATED:")
    {
        Some(rest.trim())
    } else {
        None
    }
}

fn strip_explicit_uppercase_warn(trimmed: &str) -> Option<&str> {
    if trimmed.starts_with("WARN:")
        || trimmed.starts_with("WARNING:")
        || trimmed.starts_with("DEPRECATED:")
    {
        Some(trimmed)
    } else {
        None
    }
}

fn strip_build_level_warning(trimmed: &str) -> Option<&str> {
    if trimmed.starts_with("WARN:")
        || trimmed.starts_with("WARNING:")
        || trimmed.starts_with("WARN[")
        || trimmed.starts_with("DEPRECATED:")
        || trimmed.starts_with("[Warning]")
    {
        Some(trimmed)
    } else {
        None
    }
}

fn is_bk_fatal_line(trimmed: &str) -> bool {
    let lower_prefix = trimmed
        .get(..22)
        .is_some_and(|p| p.eq_ignore_ascii_case("error: failed to solve"));
    lower_prefix
        || trimmed.starts_with("failed to solve:")
        || trimmed.starts_with("executor failed running ")
        || (trimmed.starts_with("target ") && trimmed.contains("failed to solve:"))
        || (trimmed.starts_with("Service ") && trimmed.contains("failed to build"))
}

fn record_warning(
    warnings: &mut Vec<BuildWarning>,
    seen: &mut HashSet<String>,
    text: &str,
    stream: StreamKind,
    line_no: usize,
) {
    let normalized = text
        .strip_prefix("WARN:")
        .or_else(|| text.strip_prefix("WARNING:"))
        .unwrap_or(text)
        .trim();
    if normalized.is_empty() || seen.contains(normalized) {
        return;
    }
    seen.insert(normalized.to_owned());
    warnings.push(BuildWarning {
        text: text.trim().to_owned(),
        stream,
        line_no,
    });
}

// ============================================================================
// Classic Docker / Podman Build Parser
// ============================================================================

fn parse_classic<'a>(
    stdout_lines: &[(StreamKind, usize, Cow<'a, str>)],
    stderr_lines: &[(StreamKind, usize, Cow<'a, str>)],
    cmd_is_build: bool,
) -> Option<ParsedBuild<'a>> {
    let stdout_sig = count_classic_signals(stdout_lines);
    let stderr_sig = count_classic_signals(stderr_lines);
    let total_steps = stdout_sig.step_headers + stderr_sig.step_headers;
    let total_markers = stdout_sig.markers + stderr_sig.markers;

    if cmd_is_build {
        if total_steps < 1 || (total_steps < 2 && total_markers < 1) {
            return None;
        }
    } else if total_steps < 2 && !(total_steps == 1 && total_markers >= 2) {
        return None;
    }

    let ordered_streams: [&[(StreamKind, usize, Cow<'a, str>)]; 2] =
        if stdout_sig.step_headers >= stderr_sig.step_headers {
            [stdout_lines, stderr_lines]
        } else {
            [stderr_lines, stdout_lines]
        };

    let mut steps: Vec<Step> = Vec::new();
    let mut warnings: Vec<BuildWarning> = Vec::new();
    let mut warning_seen: HashSet<String> = HashSet::new();
    let mut fatal_lines: Vec<FatalLine> = Vec::new();
    let mut image = ExportedImage {
        service: None,
        names: Vec::new(),
        digest: None,
        range: MultiRange::default(),
    };
    let mut saw_podman = false;
    let mut saw_any_step = false;
    let mut secondary_unrecognized = 0usize;
    let mut next_order = 0usize;

    for (stream_rank, lines) in ordered_streams.iter().enumerate() {
        let is_primary = stream_rank == 0;
        for &(stream, line_no, ref line) in *lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some(caps) = CLASSIC_STEP.captures(trimmed) {
                let instruction = caps[3].trim();
                if starts_with_dockerfile_instruction(instruction) {
                    saw_any_step = true;
                    if trimmed.starts_with("STEP ") || trimmed.starts_with('[') {
                        saw_podman = true;
                    }
                    // Finalize previous step as Done if still Running.
                    if let Some(prev) = steps.last_mut()
                        && matches!(prev.status, StepStatus::Running)
                    {
                        prev.status = StepStatus::Done(None);
                    }
                    let num: u32 = caps[1].parse().unwrap_or(0);
                    let total = &caps[2];
                    let label = format!("Step {num}/{total}");
                    let kind = if instruction
                        .split_whitespace()
                        .next()
                        .is_some_and(|kw| kw.eq_ignore_ascii_case("RUN"))
                    {
                        StepKind::Run
                    } else {
                        StepKind::StageStep
                    };
                    let mut range = MultiRange::default();
                    range.record(stream, line_no);
                    steps.push(Step {
                        id: num,
                        classic_label: Some(label),
                        header: instruction.to_owned(),
                        service: None,
                        kind,
                        status: StepStatus::Running,
                        range,
                        output: Vec::new(),
                        dockerfile: None,
                        order: next_order,
                    });
                    next_order += 1;
                    continue;
                }
            }

            if trimmed.starts_with("Sending build context to Docker daemon") {
                continue;
            }

            if let Some(id) = trimmed.strip_prefix("Successfully built ") {
                image.digest = Some(id.trim().to_owned());
                image.range.record(stream, line_no);
                if let Some(prev) = steps.last_mut()
                    && matches!(prev.status, StepStatus::Running)
                {
                    prev.status = StepStatus::Done(None);
                }
                continue;
            }
            if let Some(tag) = trimmed.strip_prefix("Successfully tagged ") {
                let tag = tag.trim();
                if !tag.is_empty() && !image.names.iter().any(|n| n == tag) {
                    image.names.push(tag.to_owned());
                }
                image.range.record(stream, line_no);
                continue;
            }
            if let Some(tag) = trimmed.strip_prefix("COMMIT ") {
                saw_podman = true;
                let tag = tag.trim();
                if !tag.is_empty() && !image.names.iter().any(|n| n == tag) {
                    image.names.push(tag.to_owned());
                }
                image.range.record(stream, line_no);
                if let Some(prev) = steps.last_mut()
                    && matches!(prev.status, StepStatus::Running)
                {
                    prev.status = StepStatus::Done(None);
                }
                continue;
            }

            if CLASSIC_NONZERO.is_match(trimmed) || PODMAN_STEP_ERROR.is_match(trimmed) {
                if PODMAN_STEP_ERROR.is_match(trimmed) {
                    saw_podman = true;
                }
                if let Some(last) = steps.last_mut() {
                    last.range.record(stream, line_no);
                    last.status = StepStatus::Error {
                        message: trimmed.to_owned(),
                        stream,
                        line_no,
                    };
                } else {
                    fatal_lines.push(FatalLine {
                        text: trimmed.to_owned(),
                        stream,
                        line_no,
                    });
                }
                continue;
            }

            if let Some(warn) = strip_build_level_warning(trimmed) {
                record_warning(&mut warnings, &mut warning_seen, warn, stream, line_no);
                continue;
            }

            if is_primary && saw_any_step {
                if let Some(cur) = steps.last_mut() {
                    cur.range.record(stream, line_no);
                    if trimmed == "---> Using cache" || trimmed.starts_with("--> Using cache") {
                        if trimmed.starts_with("--> ") {
                            saw_podman = true;
                        }
                        cur.status = StepStatus::Cached;
                        continue;
                    }
                    if trimmed.starts_with("---> Running in ")
                        || trimmed.starts_with("Removing intermediate container ")
                        || CLASSIC_LAYER_ID.is_match(trimmed)
                    {
                        if trimmed.starts_with("--> ") {
                            saw_podman = true;
                        }
                        continue;
                    }
                    // Final bare 64-hex Podman image ID after COMMIT / Successfully tagged.
                    if BARE_IMAGE_ID.is_match(trimmed) && !image.names.is_empty() {
                        if image.digest.is_none() {
                            image.digest = Some(trimmed.to_owned());
                            image.range.record(stream, line_no);
                        }
                        continue;
                    }
                    // Classic builder failure line without `The command ... returned a non-zero code`
                    // (e.g. `COPY failed: file not found in build context`).
                    if trimmed.starts_with("COPY failed:")
                        || trimmed.starts_with("ADD failed:")
                        || trimmed.starts_with("pull access denied")
                        || trimmed.starts_with("manifest for ")
                    {
                        cur.status = StepStatus::Error {
                            message: trimmed.to_owned(),
                            stream,
                            line_no,
                        };
                        continue;
                    }
                    cur.output.push(StepOutputLine {
                        stream,
                        line_no,
                        text: line.clone(),
                    });
                    continue;
                }
            } else if !is_primary {
                // Secondary stream (often stderr during classic `docker build`, carrying
                // `The command ...` or compiler errors from the failing step).
                if let Some(cur) = steps.last_mut()
                    && (cur.kind == StepKind::Run || cur.is_failed())
                {
                    cur.range.record(stream, line_no);
                    cur.output.push(StepOutputLine {
                        stream,
                        line_no,
                        text: line.clone(),
                    });
                    continue;
                }
                secondary_unrecognized += 1;
            }
        }
    }

    if steps.is_empty() || secondary_unrecognized > 5 {
        return None;
    }

    // Mark the last step Done if we saw a clean image completion or no failure.
    if let Some(last) = steps.last_mut()
        && matches!(last.status, StepStatus::Running)
    {
        last.status = StepStatus::Done(None);
    }

    let mut images = Vec::new();
    if !image.names.is_empty() || image.digest.is_some() {
        images.push(image);
    }

    Some(ParsedBuild {
        format: BuildFormat::Classic,
        tool_label: if saw_podman {
            "podman build"
        } else {
            "docker build"
        },
        steps,
        images,
        warnings,
        services: Vec::new(),
        wall_duration_secs: None,
        fatal_lines,
        unmatched_dockerfile: None,
    })
}

struct ClassicSignalCounts {
    step_headers: usize,
    markers: usize,
}

fn count_classic_signals(lines: &[(StreamKind, usize, Cow<'_, str>)]) -> ClassicSignalCounts {
    let mut counts = ClassicSignalCounts {
        step_headers: 0,
        markers: 0,
    };
    for (_, _, line) in lines {
        let trimmed = line.trim();
        if let Some(caps) = CLASSIC_STEP.captures(trimmed)
            && starts_with_dockerfile_instruction(caps[3].trim())
        {
            counts.step_headers += 1;
        } else if trimmed == "---> Using cache"
            || trimmed.starts_with("--> Using cache")
            || trimmed.starts_with("---> Running in ")
            || trimmed.starts_with("Removing intermediate container ")
            || trimmed.starts_with("Successfully built ")
            || trimmed.starts_with("Successfully tagged ")
            || trimmed.starts_with("Sending build context to Docker daemon")
            || CLASSIC_LAYER_ID.is_match(trimmed)
            || CLASSIC_NONZERO.is_match(trimmed)
            || PODMAN_STEP_ERROR.is_match(trimmed)
        {
            counts.markers += 1;
        }
    }
    counts
}

// ============================================================================
// Renderer
// ============================================================================

fn render_build(parsed: &ParsedBuild<'_>, exit_code: Option<i32>, budget: usize) -> Option<String> {
    if parsed.has_failure(exit_code) {
        render_failed(parsed, exit_code, budget)
    } else {
        render_clean(parsed, budget)
    }
}

fn render_clean(parsed: &ParsedBuild<'_>, budget: usize) -> Option<String> {
    let counts = StepCounts::from_steps(&parsed.steps);
    let dur = total_duration_label(parsed);
    let mut header = build_header_prefix(parsed);
    let _ = write!(header, ": {}", counts.format_clean());
    if let Some(d) = dur {
        let _ = write!(header, " · {d}");
    }
    header.push('\n');

    if header.len() > budget {
        return None;
    }
    let mut out = header;

    // 1. Exported images / digests (high priority).
    for img in &parsed.images {
        let line = format_image_line(img);
        if out.len() + line.len() <= budget {
            out.push_str(&line);
        }
    }

    // 2. Warnings (high priority, never displaced by routine steps).
    append_warnings(&mut out, &parsed.warnings, budget);

    // 3. Collapsed internal/cached/trivial steps summary line (reserve its bytes so it fits).
    let collapsed_line = build_clean_collapsed_line(&parsed.steps);
    let collapsed_reserve = collapsed_line.as_ref().map_or(0, String::len);

    // 4. Non-trivial RUN / output-producing step summaries.
    let notable: Vec<&Step<'_>> = parsed
        .steps
        .iter()
        .filter(|s| is_notable_clean_step(s))
        .collect();

    if !notable.is_empty() {
        let steps_budget = budget.saturating_sub(collapsed_reserve);
        let heading = "Steps:\n";
        if out.len() + heading.len() <= steps_budget {
            let mut section = String::from(heading);
            let mut shown = 0usize;
            for (idx, step) in notable.iter().enumerate() {
                let remaining = notable.len() - idx - 1;
                let overflow_reserve = if remaining > 0 { 36 } else { 0 };
                let line = format_clean_step_line(step);
                if out.len() + section.len() + line.len() + overflow_reserve <= steps_budget {
                    section.push_str(&line);
                    shown += 1;
                } else {
                    let hidden = notable.len() - shown;
                    let note = format!("  … +{hidden} more step(s)\n");
                    if out.len() + section.len() + note.len() <= steps_budget {
                        section.push_str(&note);
                    }
                    break;
                }
            }
            if shown > 0 {
                out.push_str(&section);
            }
        }
    }

    if let Some(collapsed) = collapsed_line
        && out.len() + collapsed.len() <= budget
    {
        out.push_str(&collapsed);
    }

    Some(out)
}

fn render_failed(
    parsed: &ParsedBuild<'_>,
    exit_code: Option<i32>,
    budget: usize,
) -> Option<String> {
    let counts = StepCounts::from_steps(&parsed.steps);
    let dur = total_duration_label(parsed);
    let mut header = build_header_prefix(parsed);
    match exit_code.filter(|&c| c != 0) {
        Some(code) => {
            let _ = write!(header, " FAILED (exit {code}): {}", counts.format_failed());
        }
        None => {
            let _ = write!(header, " FAILED: {}", counts.format_failed());
        }
    }
    if let Some(d) = dur {
        let _ = write!(header, " · {d}");
    }
    header.push('\n');

    if header.len() > budget {
        return None;
    }
    let mut out = header;

    // Identify failing steps (or fall back to the last non-internal step if exit != 0 without explicit ERROR).
    let mut failing: Vec<&Step<'_>> = parsed.steps.iter().filter(|s| s.is_failed()).collect();
    if failing.is_empty()
        && let Some(last) = parsed
            .steps
            .iter()
            .rev()
            .find(|s| s.kind != StepKind::Internal && !s.is_cached())
            .or_else(|| parsed.steps.last())
    {
        failing.push(last);
    }

    // Prepare prior-steps 1-line summary (reserve only if generous budget remains).
    let failed_orders: HashSet<usize> = failing.iter().map(|s| s.order).collect();
    let prior_line = build_prior_steps_line(&parsed.steps, &failed_orders);
    let prior_reserve = prior_line
        .as_ref()
        .filter(|l| out.len() + l.len() + 260 <= budget)
        .map_or(0, String::len);

    // 1. Failing step(s) FIRST — budget-protected above everything else.
    let fail_total_budget = budget.saturating_sub(prior_reserve);
    let fail_count = failing.len().max(1);
    for (idx, step) in failing.iter().enumerate() {
        let remaining_steps = fail_count - idx;
        let remaining_budget = fail_total_budget.saturating_sub(out.len());
        if remaining_budget == 0 {
            break;
        }
        let step_budget = if remaining_steps <= 1 {
            remaining_budget
        } else {
            remaining_budget / remaining_steps
        };
        let block = format_failing_step(step, step_budget);
        if out.len() + block.len() <= budget {
            out.push_str(&block);
        }
    }

    // 2. Unmatched Dockerfile snippet (if not already attached to a failing step).
    if let Some(snippet) = &parsed.unmatched_dockerfile {
        let line = format_dockerfile_snippet(snippet);
        if out.len() + line.len() <= budget {
            out.push_str(&line);
        }
    }

    // 3. Non-redundant build-level fatal error lines.
    for fatal in &parsed.fatal_lines {
        if is_redundant_fatal(&fatal.text, &failing) {
            continue;
        }
        let line = format!(
            "ERROR: {} {}\n",
            truncate_utf8(
                fatal
                    .text
                    .strip_prefix("ERROR:")
                    .unwrap_or(&fatal.text)
                    .trim(),
                MAX_ERROR_LINE_BYTES
            ),
            LineRange::single(fatal.stream, fatal.line_no).fmt_bracketed()
        );
        if out.len() + line.len() <= budget {
            out.push_str(&line);
        }
    }

    // 4. Warnings (if budget permits).
    append_warnings(
        &mut out,
        &parsed.warnings,
        budget.saturating_sub(prior_reserve),
    );

    // 5. Prior completed/cached steps 1-line summary.
    if let Some(prior) = prior_line
        && out.len() + prior.len() <= budget
    {
        out.push_str(&prior);
    }

    Some(out)
}

fn build_header_prefix(parsed: &ParsedBuild<'_>) -> String {
    let mut prefix = parsed.tool_label.to_owned();
    if !parsed.services.is_empty() {
        let n = parsed.services.len();
        let noun = if n == 1 { "service" } else { "services" };
        let shown: Vec<&str> = parsed.services.iter().take(5).map(String::as_str).collect();
        let _ = write!(prefix, " ({n} {noun}: {}", shown.join(", "));
        if n > 5 {
            let _ = write!(prefix, ", +{} more", n - 5);
        }
        prefix.push(')');
    }
    prefix
}

struct StepCounts {
    total: usize,
    cached: usize,
    ran: usize,
    internal: usize,
    failed: usize,
    canceled: usize,
}

impl StepCounts {
    fn from_steps(steps: &[Step<'_>]) -> Self {
        let mut c = Self {
            total: 0,
            cached: 0,
            ran: 0,
            internal: 0,
            failed: 0,
            canceled: 0,
        };
        for s in steps {
            c.total += 1;
            if s.is_failed() {
                c.failed += 1;
            } else if matches!(s.status, StepStatus::Canceled) {
                c.canceled += 1;
            } else if s.kind == StepKind::Internal {
                c.internal += 1;
            } else if s.is_cached() {
                c.cached += 1;
            } else {
                c.ran += 1;
            }
        }
        c
    }

    fn format_clean(&self) -> String {
        let noun = if self.total == 1 { "step" } else { "steps" };
        let mut parts = Vec::new();
        if self.cached > 0 {
            parts.push(format!("{} cached", self.cached));
        }
        if self.ran > 0 {
            parts.push(format!("{} ran", self.ran));
        }
        if self.internal > 0 {
            parts.push(format!("{} internal", self.internal));
        }
        if parts.is_empty() {
            format!("{} {noun}", self.total)
        } else {
            format!("{} {noun} ({})", self.total, parts.join(", "))
        }
    }

    fn format_failed(&self) -> String {
        let noun = if self.total == 1 { "step" } else { "steps" };
        let mut parts = Vec::new();
        if self.cached > 0 {
            parts.push(format!("{} cached", self.cached));
        }
        if self.ran > 0 {
            parts.push(format!("{} ran", self.ran));
        }
        if self.internal > 0 {
            parts.push(format!("{} internal", self.internal));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.canceled > 0 {
            parts.push(format!("{} canceled", self.canceled));
        }
        if parts.is_empty() {
            format!("{} {noun}", self.total)
        } else {
            format!("{} {noun} ({})", self.total, parts.join(", "))
        }
    }
}

fn total_duration_label(parsed: &ParsedBuild<'_>) -> Option<String> {
    if let Some(wall) = parsed.wall_duration_secs {
        return Some(format_duration_secs(wall));
    }
    if parsed.format != BuildFormat::BuildKit {
        return None;
    }
    let mut sum = 0.0f64;
    let mut any = false;
    for s in &parsed.steps {
        if let Some(d) = s.duration_secs() {
            sum += d;
            any = true;
        }
    }
    (any && sum > 0.0).then(|| format_duration_secs(sum))
}

fn is_notable_clean_step(step: &Step<'_>) -> bool {
    if step.kind == StepKind::Internal || step.kind == StepKind::Export || step.is_cached() {
        return false;
    }
    step.kind == StepKind::Run || !step.output.is_empty()
}

fn format_clean_step_line(step: &Step<'_>) -> String {
    let mut line = format!(
        "  {} {}",
        step.display_id(),
        truncate_utf8(&step.header, MAX_STEP_HEADER_BYTES)
    );
    if let Some(d) = step.duration_secs() {
        let _ = write!(line, " · {}", format_duration_secs(d));
    }
    if !step.output.is_empty() {
        let n = step.output.len();
        let noun = if n == 1 { "line" } else { "lines" };
        let _ = write!(line, " ({n} {noun})");
    }
    if !step.range.is_empty() {
        let _ = write!(line, " {}", step.range.fmt_bracketed());
    }
    line.push('\n');
    line
}

fn format_image_line(img: &ExportedImage) -> String {
    let mut line = String::from("Image");
    if let Some(svc) = &img.service {
        let _ = write!(line, " [{svc}]");
    }
    line.push_str(": ");
    match (img.names.is_empty(), &img.digest) {
        (false, Some(digest)) => {
            let _ = write!(line, "{} ({digest})", img.names.join(", "));
        }
        (false, None) => {
            line.push_str(&img.names.join(", "));
        }
        (true, Some(digest)) => {
            line.push_str(digest);
        }
        (true, None) => {}
    }
    if !img.range.is_empty() {
        let _ = write!(line, " {}", img.range.fmt_bracketed());
    }
    line.push('\n');
    line
}

fn append_warnings(out: &mut String, warnings: &[BuildWarning], budget: usize) {
    if warnings.is_empty() {
        return;
    }
    let heading = "Warnings:\n";
    if out.len() + heading.len() > budget {
        return;
    }
    let mut section = String::from(heading);
    let mut shown = 0usize;
    for (idx, w) in warnings.iter().take(MAX_WARNINGS_SHOWN).enumerate() {
        let remaining = warnings.len() - idx - 1;
        let overflow_reserve = if remaining > 0 { 36 } else { 0 };
        let text = truncate_utf8(&w.text, MAX_WARNING_LINE_BYTES);
        let ptr = LineRange::single(w.stream, w.line_no).fmt_bracketed();
        let line = format!("  {text} {ptr}\n");
        if out.len() + section.len() + line.len() + overflow_reserve <= budget {
            section.push_str(&line);
            shown += 1;
        } else {
            break;
        }
    }
    if shown == 0 {
        return;
    }
    if shown < warnings.len() {
        let hidden = warnings.len() - shown;
        let note = format!("  … +{hidden} more warning(s)\n");
        if out.len() + section.len() + note.len() <= budget {
            section.push_str(&note);
        }
    }
    out.push_str(&section);
}

fn build_clean_collapsed_line(steps: &[Step<'_>]) -> Option<String> {
    let mut internal = 0usize;
    let mut cached = 0usize;
    let mut other = 0usize;
    let mut range = MultiRange::default();
    for s in steps {
        if is_notable_clean_step(s) {
            continue;
        }
        range.merge(&s.range);
        if s.kind == StepKind::Internal {
            internal += 1;
        } else if s.is_cached() {
            cached += 1;
        } else {
            other += 1;
        }
    }
    if internal + cached + other == 0 {
        return None;
    }
    let mut parts = Vec::new();
    if internal > 0 {
        parts.push(format!("{internal} internal"));
    }
    if cached > 0 {
        parts.push(format!("{cached} cached"));
    }
    if other > 0 {
        parts.push(format!("{other} metadata/export"));
    }
    let mut line = format!("Collapsed: {} steps", parts.join(", "));
    if !range.is_empty() {
        let _ = write!(line, " {}", range.fmt_bracketed());
    }
    line.push('\n');
    Some(line)
}

fn build_prior_steps_line(steps: &[Step<'_>], failed_orders: &HashSet<usize>) -> Option<String> {
    let mut cached = 0usize;
    let mut ran = 0usize;
    let mut internal = 0usize;
    let mut canceled = 0usize;
    let mut range = MultiRange::default();
    for s in steps {
        if failed_orders.contains(&s.order) {
            continue;
        }
        range.merge(&s.range);
        if matches!(s.status, StepStatus::Canceled) {
            canceled += 1;
        } else if s.kind == StepKind::Internal {
            internal += 1;
        } else if s.is_cached() {
            cached += 1;
        } else {
            ran += 1;
        }
    }
    let total = cached + ran + internal + canceled;
    if total == 0 {
        return None;
    }
    let mut parts = Vec::new();
    if cached > 0 {
        parts.push(format!("{cached} cached"));
    }
    if ran > 0 {
        parts.push(format!("{ran} ran"));
    }
    if internal > 0 {
        parts.push(format!("{internal} internal"));
    }
    if canceled > 0 {
        parts.push(format!("{canceled} canceled"));
    }
    let mut line = format!("Prior steps: {total} ({})", parts.join(", "));
    if !range.is_empty() {
        let _ = write!(line, " {}", range.fmt_bracketed());
    }
    line.push('\n');
    Some(line)
}

fn format_dockerfile_snippet(snippet: &DockerfileSnippet) -> String {
    let ptr = LineRange::single(snippet.stream, snippet.line_no).fmt_bracketed();
    match &snippet.highlighted {
        Some(hl) => format!(
            "  {}: >>> {} {ptr}\n",
            snippet.location,
            truncate_utf8(hl, MAX_STEP_HEADER_BYTES)
        ),
        None => format!("  {} {ptr}\n", snippet.location),
    }
}

fn is_redundant_fatal(fatal: &str, failing: &[&Step<'_>]) -> bool {
    let norm = fatal
        .strip_prefix("ERROR:")
        .unwrap_or(fatal)
        .trim()
        .strip_prefix("failed to solve:")
        .unwrap_or(fatal.strip_prefix("ERROR:").unwrap_or(fatal).trim())
        .trim();
    if norm.is_empty() {
        return true;
    }
    failing.iter().any(|s| match &s.status {
        StepStatus::Error { message, .. } => {
            let m = message.trim();
            m == norm || m.contains(norm) || norm.contains(m)
        }
        _ => false,
    })
}

fn format_failing_step(step: &Step<'_>, budget: usize) -> String {
    let mut out = String::new();

    // Header: `Failed: #9 [builder 4/6] RUN cargo build --release [stderr:45-218]`
    let ptr = if step.range.is_empty() {
        String::new()
    } else {
        format!(" {}", step.range.fmt_bracketed())
    };
    let id = step.display_id();
    let head_prefix = if id.is_empty() {
        "Failed: ".to_owned()
    } else {
        format!("Failed: {id} ")
    };
    let head_max = budget
        .saturating_sub(head_prefix.len() + ptr.len() + 1)
        .min(MAX_STEP_HEADER_BYTES);
    let head = format!(
        "{head_prefix}{}{ptr}\n",
        truncate_utf8(&step.header, head_max)
    );
    if head.len() > budget {
        return String::new();
    }
    out.push_str(&head);

    // Optional Dockerfile location line.
    if let Some(snippet) = &step.dockerfile {
        let line = format_dockerfile_snippet(snippet);
        if out.len() + line.len() <= budget {
            out.push_str(&line);
        }
    }

    // Step ERROR line.
    if let StepStatus::Error {
        message,
        stream,
        line_no,
    } = &step.status
    {
        let err_ptr = LineRange::single(*stream, *line_no).fmt_bracketed();
        let err_prefix = "  ERROR: ";
        let err_avail = budget
            .saturating_sub(out.len() + err_prefix.len() + err_ptr.len() + 2)
            .min(MAX_ERROR_LINE_BYTES);
        if err_avail >= 16 {
            let err_line = format!(
                "{err_prefix}{} {err_ptr}\n",
                truncate_utf8(message, err_avail)
            );
            if out.len() + err_line.len() <= budget {
                out.push_str(&err_line);
            }
        }
    }

    if step.output.is_empty() {
        return out;
    }

    let remaining = budget.saturating_sub(out.len());
    if remaining < 32 {
        return out;
    }

    let excerpt = select_failing_output(&step.output, remaining);
    out.push_str(&excerpt);
    out
}

/// Select the most actionable lines from a failing step's output within `budget`:
/// 1. Error/diagnostic anchor windows (with workspace-relevant anchors highest priority).
/// 2. Actionable final tail lines.
///
/// Emits contiguous spans with exact `[stream:A-B]` pointers and `… N lines omitted [stream:X-Y]`.
fn select_failing_output(lines: &[StepOutputLine<'_>], budget: usize) -> String {
    let n = lines.len();
    if n == 0 {
        return String::new();
    }

    // Identify error anchor indices and their workspace relevance.
    let mut anchors: Vec<(usize, bool)> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if STEP_ERROR_ANCHOR.is_match(&line.text) {
            let near_workspace = WORKSPACE_PATH.is_match(&line.text)
                || lines
                    .get(idx + 1)
                    .is_some_and(|next| WORKSPACE_PATH.is_match(&next.text))
                || (idx > 0 && WORKSPACE_PATH.is_match(&lines[idx - 1].text));
            anchors.push((idx, near_workspace));
        }
    }

    // Expand each anchor into a diagnostic window `[start..=end]`.
    let mut windows: Vec<(usize, usize, u8)> = Vec::new(); // (start, end, priority: 3=ws-err, 2=err, 1=tail)
    for &(anchor_idx, near_ws) in &anchors {
        let start = if anchor_idx > 0 && is_anchor_lead_in(&lines[anchor_idx - 1].text) {
            anchor_idx - 1
        } else {
            anchor_idx
        };
        let mut end = anchor_idx;
        let max_end = (anchor_idx + MAX_ANCHOR_AFTER_LINES).min(n - 1);
        let mut j = anchor_idx + 1;
        while j <= max_end {
            let text = &lines[j].text;
            let trimmed = text.trim();
            if trimmed.is_empty() {
                // Keep a blank line only if another diagnostic continuation follows immediately.
                if j < max_end && DIAGNOSTIC_CONT.is_match(&lines[j + 1].text) {
                    end = j;
                    j += 1;
                    continue;
                }
                break;
            }
            if !is_build_progress_noise(trimmed)
                && (DIAGNOSTIC_CONT.is_match(text)
                    || text.starts_with([' ', '\t'])
                    || STEP_ERROR_ANCHOR.is_match(text))
            {
                end = j;
                j += 1;
            } else {
                break;
            }
        }
        let prio = if near_ws { 3 } else { 2 };
        windows.push((start, end, prio));
    }

    // Include actionable tail lines:
    // - When no explicit error anchors were found, keep up to `MAX_FAILING_TAIL_LINES`.
    // - When explicit error anchors exist, keep only trailing non-empty lines after the
    //   last anchor window (up to 4 lines) so pre-error build chatter is not pulled in.
    let last_anchor_end = windows.iter().map(|&(_, end, _)| end).max();
    let tail_limit = if anchors.is_empty() {
        MAX_FAILING_TAIL_LINES
    } else {
        4
    };
    let mut tail_indices = Vec::new();
    for idx in (0..n).rev() {
        if let Some(after) = last_anchor_end
            && idx <= after
        {
            break;
        }
        if !lines[idx].text.trim().is_empty() {
            tail_indices.push(idx);
            if tail_indices.len() >= tail_limit {
                break;
            }
        }
    }
    if let (Some(&first_tail), Some(&last_tail)) = (tail_indices.last(), tail_indices.first()) {
        windows.push((first_tail, last_tail, 1));
    }

    // Try rendering full merged selection; if it exceeds budget, progressively
    // narrow (drop tail count -> drop non-workspace anchors -> shrink context -> last anchors only).
    let strategies: &[(u8, usize, usize)] = &[
        (1, MAX_ANCHOR_AFTER_LINES, MAX_FAILING_TAIL_LINES),
        (1, 5, 6),
        (2, 4, 4),
        (2, 2, 2),
        (3, 3, 2),
        (3, 1, 1),
        (1, 0, 3),
        (1, 0, 1),
    ];

    for &(min_prio, max_after, max_tail) in strategies {
        let mask = build_selection_mask(
            n,
            &windows,
            &anchors,
            &tail_indices,
            min_prio,
            max_after,
            max_tail,
        );
        if let Some(rendered) = render_selected_lines(lines, &mask, budget) {
            return rendered;
        }
    }

    // Final ultra-tight fallback: single highest-priority error or final non-empty line, truncated.
    let best_idx = anchors
        .iter()
        .find(|(_, ws)| *ws)
        .or_else(|| anchors.first())
        .map(|(i, _)| *i)
        .or_else(|| tail_indices.first().copied())
        .unwrap_or(n - 1);
    let item = &lines[best_idx];
    let ptr = LineRange::single(item.stream, item.line_no).fmt_bracketed();
    let avail = budget.saturating_sub(ptr.len() + 5);
    if avail < 8 {
        return String::new();
    }
    format!("  {} {ptr}\n", truncate_utf8(item.text.trim(), avail))
}

fn is_build_progress_noise(trimmed: &str) -> bool {
    [
        "Compiling ",
        "Checking ",
        "Downloading ",
        "Downloaded ",
        "Updating ",
        "Locking ",
        "Fresh ",
        "Collecting ",
        "Get:",
    ]
    .iter()
    .any(|p| trimmed.starts_with(p))
}

fn is_anchor_lead_in(text: &str) -> bool {
    let trimmed = text.trim();
    !is_build_progress_noise(trimmed)
        && (trimmed.starts_with("> ") || trimmed.starts_with("Traceback") || trimmed.ends_with(':'))
}

fn build_selection_mask(
    n: usize,
    windows: &[(usize, usize, u8)],
    anchors: &[(usize, bool)],
    tail_indices: &[usize],
    min_prio: u8,
    max_after: usize,
    max_tail: usize,
) -> Vec<bool> {
    let mut keep = vec![false; n];
    for &(start, end, prio) in windows {
        if prio < min_prio || prio == 1 {
            continue;
        }
        let capped_end = end.min(start + 1 + max_after);
        for flag in keep.iter_mut().take(capped_end + 1).skip(start) {
            *flag = true;
        }
    }
    // If min_prio filtered out all anchors, keep the anchor lines themselves.
    if !keep.iter().any(|&k| k) {
        for &(idx, _) in anchors {
            keep[idx] = true;
        }
    }
    if min_prio <= 1 || !keep.iter().any(|&k| k) {
        for &idx in tail_indices.iter().take(max_tail) {
            keep[idx] = true;
        }
    }
    // Bridge tiny 1-2 line gaps inside selected spans so we don't print an omission line for 1 line.
    let mut i = 0usize;
    while i < n {
        if keep[i] {
            let mut j = i + 1;
            while j < n && !keep[j] && j - i <= 2 {
                j += 1;
            }
            if j < n && keep[j] && j > i + 1 {
                for flag in keep.iter_mut().take(j).skip(i + 1) {
                    *flag = true;
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    keep
}

fn render_selected_lines(
    lines: &[StepOutputLine<'_>],
    mask: &[bool],
    budget: usize,
) -> Option<String> {
    let n = lines.len();
    let mut out = String::new();
    let mut i = 0usize;

    while i < n {
        if !mask[i] {
            let start = i;
            let mut omit_range = MultiRange::default();
            while i < n && !mask[i] {
                omit_range.record(lines[i].stream, lines[i].line_no);
                i += 1;
            }
            let count = i - start;
            let noun = if count == 1 { "line" } else { "lines" };
            let note = format!(
                "  … {count} {noun} omitted {}\n",
                omit_range.fmt_bracketed()
            );
            if out.len() + note.len() > budget {
                return None;
            }
            out.push_str(&note);
        } else {
            let start = i;
            let mut chunk_range = MultiRange::default();
            while i < n && mask[i] {
                chunk_range.record(lines[i].stream, lines[i].line_no);
                i += 1;
            }
            // Emit chunk pointer header if we omitted anything or if there are multiple chunks,
            // or attach pointer cleanly on a single-line chunk.
            let any_omitted = mask.iter().any(|&k| !k);
            if i - start == 1 {
                let item = &lines[start];
                let ptr = LineRange::single(item.stream, item.line_no).fmt_bracketed();
                let body = truncate_utf8(&item.text, MAX_ERROR_LINE_BYTES);
                let line = format!("  {body} {ptr}\n");
                if out.len() + line.len() > budget {
                    return None;
                }
                out.push_str(&line);
            } else {
                if any_omitted {
                    let hdr = format!("  {}\n", chunk_range.fmt_bracketed());
                    if out.len() + hdr.len() > budget {
                        return None;
                    }
                    out.push_str(&hdr);
                }
                for item in &lines[start..i] {
                    let body = truncate_utf8(&item.text, MAX_ERROR_LINE_BYTES);
                    let line = if any_omitted {
                        format!("    {body}\n")
                    } else {
                        format!("  {body}\n")
                    };
                    if out.len() + line.len() > budget {
                        return None;
                    }
                    out.push_str(&line);
                }
                if !any_omitted {
                    // When all lines of the step are shown, attach exact output span pointer if distinct.
                    let ptr_line = format!("  {}\n", chunk_range.fmt_bracketed());
                    if out.len() + ptr_line.len() <= budget {
                        out.push_str(&ptr_line);
                    }
                }
            }
        }
    }

    Some(out)
}

// ============================================================================
// Formatting & Parsing Helpers
// ============================================================================

fn parse_duration_secs(raw: &str) -> Option<f64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(ms) = s.strip_suffix("ms") {
        return ms.parse::<f64>().ok().map(|v| v / 1000.0);
    }
    let mut total = 0.0f64;
    let mut rest = s;
    let mut parsed_any = false;
    if let Some((h, tail)) = rest.split_once('h') {
        total += h.parse::<f64>().ok()? * 3600.0;
        rest = tail;
        parsed_any = true;
    }
    if let Some((m, tail)) = rest.split_once('m') {
        // Avoid confusing `ms` (already handled above).
        total += m.parse::<f64>().ok()? * 60.0;
        rest = tail;
        parsed_any = true;
    }
    if let Some(sec) = rest.strip_suffix('s') {
        total += sec.parse::<f64>().ok()?;
        rest = "";
        parsed_any = true;
    }
    (parsed_any && rest.is_empty() && total.is_finite() && total >= 0.0).then_some(total)
}

fn format_duration_secs(secs: f64) -> String {
    if secs >= 60.0 {
        let mins = (secs / 60.0).floor() as u64;
        let rem = secs - (mins as f64) * 60.0;
        format!("{mins}m{rem:.1}s")
    } else {
        format!("{secs:.1}s")
    }
}

fn truncate_utf8(text: &str, limit: usize) -> Cow<'_, str> {
    if text.len() <= limit {
        return Cow::Borrowed(text);
    }
    let ellipsis = "…";
    if limit <= ellipsis.len() {
        let cut = text.floor_char_boundary(limit);
        return Cow::Borrowed(&text[..cut]);
    }
    let cut = text.floor_char_boundary(limit - ellipsis.len());
    let mut out = String::with_capacity(cut + ellipsis.len());
    out.push_str(&text[..cut]);
    out.push_str(ellipsis);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: verify every `[stdout:A]` / `[stderr:A-B]` pointer in `report`
    /// points to valid 1-based line numbers within `stdout`/`stderr`.
    fn assert_all_pointers_valid(report: &str, stdout: &str, stderr: &str) {
        let ptr_re = Regex::new(r"\[(stdout|stderr):(\d+)(?:-(\d+))?\]").unwrap();
        let stdout_count = stdout.split_inclusive('\n').count();
        let stderr_count = stderr.split_inclusive('\n').count();
        let mut found = 0usize;
        for caps in ptr_re.captures_iter(report) {
            found += 1;
            let stream = &caps[1];
            let first: usize = caps[2].parse().unwrap();
            let last: usize = caps.get(3).map_or(first, |m| m.as_str().parse().unwrap());
            let max_lines = if stream == "stdout" {
                stdout_count
            } else {
                stderr_count
            };
            assert!(
                first >= 1 && first <= last && last <= max_lines,
                "invalid pointer [{stream}:{first}-{last}] (max_lines={max_lines}) in:\n{report}"
            );
        }
        assert!(
            found > 0,
            "expected at least one line pointer in:\n{report}"
        );
    }

    fn stream_line(text: &str, line_no: usize) -> &str {
        text.split_inclusive('\n')
            .nth(line_no - 1)
            .expect("line exists")
            .trim_end_matches(['\r', '\n'])
    }

    #[test]
    fn large_multistage_buildkit_clean_build_achieves_over_90_percent_reduction() {
        let mut stderr = String::new();
        let mut ln = 0usize;
        let mut push = |s: &str| {
            stderr.push_str(s);
            stderr.push('\n');
            ln += 1;
            ln
        };

        push("#0 building with \"default\" instance using docker driver");
        push("");
        push("#1 [internal] load build definition from Dockerfile");
        push("#1 transferring dockerfile: 1.42kB done");
        let warn_line =
            push("#1 WARN: FromAsCasing: 'as' and 'FROM' keywords' casing do not match (line 1)");
        push("#1 DONE 0.0s");
        push("");
        push("#2 [internal] load metadata for docker.io/library/rust:1.83-slim");
        push("#2 DONE 0.6s");
        push("");
        push("#3 [internal] load metadata for docker.io/library/node:20-alpine");
        push("#3 DONE 0.5s");
        push("");
        push("#4 [internal] load .dockerignore");
        push("#4 transferring context: 184B done");
        push("#4 DONE 0.0s");
        push("");
        push("#5 [internal] load build context");
        push("#5 transferring context: 312.45kB 0.1s done");
        push("#5 DONE 0.1s");
        push("");
        push(
            "#6 [web 1/4] FROM docker.io/library/node:20-alpine@sha256:1111111111111111111111111111111111111111111111111111111111111111",
        );
        push(
            "#6 resolve docker.io/library/node:20-alpine@sha256:1111111111111111111111111111111111111111111111111111111111111111 done",
        );
        push("#6 DONE 0.0s");
        push("");
        push("#7 [web 2/4] WORKDIR /web");
        push("#7 CACHED");
        push("");
        push("#8 [web 3/4] COPY web/package*.json ./");
        push("#8 CACHED");
        push("");
        let npm_start = push("#9 [web 4/4] RUN npm ci && npm run build");
        for i in 0..140 {
            push(&format!(
                "#9 {:.3} npm http fetch GET 200 https://registry.npmjs.org/pkg-{i:03}/-/pkg-{i:03}-1.0.0.tgz {}ms (cache miss)",
                0.2 + i as f64 * 0.03,
                15 + (i % 40)
            ));
        }
        push("#9 4.510 added 412 packages, and audited 413 packages in 4s");
        push("#9 5.820 ✓ built in 1.24s");
        let npm_end = push("#9 DONE 6.1s");
        push("");
        push(
            "#10 [builder 1/4] FROM docker.io/library/rust:1.83-slim@sha256:2222222222222222222222222222222222222222222222222222222222222222",
        );
        push("#10 CACHED");
        push("");
        push("#11 [builder 2/4] WORKDIR /app");
        push("#11 CACHED");
        push("");
        push("#12 [builder 3/4] COPY Cargo.toml Cargo.lock src ./");
        push("#12 CACHED");
        push("");
        let cargo_start = push("#13 [builder 4/4] RUN cargo build --release");
        for i in 0..180 {
            push(&format!(
                "#13 {:.3}    Compiling crate-dep-{i:03} v0.{}.{i} (/usr/local/cargo/registry/src/index.crates.io/crate-dep-{i:03})",
                0.3 + i as f64 * 0.08,
                i % 10
            ));
        }
        push("#13 15.100     Finished `release` profile [optimized] target(s) in 14.82s");
        let cargo_end = push("#13 DONE 15.3s");
        push("");
        push("#14 [runtime 1/3] FROM docker.io/library/debian:bookworm-slim");
        push("#14 CACHED");
        push("");
        push(
            "#15 [runtime 2/3] COPY --from=builder /app/target/release/server /usr/local/bin/server",
        );
        push("#15 DONE 0.2s");
        push("");
        push("#16 [runtime 3/3] COPY --from=web /web/dist /var/www/html");
        push("#16 DONE 0.1s");
        push("");
        push("#17 exporting to image");
        push("#17 exporting layers 1.2s done");
        let digest_line = push(
            "#17 writing image sha256:8f9a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4 done",
        );
        let naming_line = push("#17 naming to docker.io/library/myapp:latest done");
        push("#17 DONE 1.3s");

        let cmd = vec![
            "docker".to_owned(),
            "build".to_owned(),
            "-t".to_owned(),
            "myapp:latest".to_owned(),
            ".".to_owned(),
        ];
        assert!(matches(&cmd, "", &stderr));

        let report = format("", &stderr, Some(0), 4096).expect("should compact clean build");
        assert!(report.len() <= 4096);
        assert!(
            report.len() * 10 < stderr.len(),
            "expected >90% reduction: raw={} compacted={}\n{report}",
            stderr.len(),
            report.len()
        );
        assert_all_pointers_valid(&report, "", &stderr);

        // Verify key semantics preserved.
        assert!(report.contains("docker build: 17 steps (6 cached, 6 ran, 5 internal)"));
        assert!(report.contains(
            "Image: docker.io/library/myapp:latest (sha256:8f9a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4)"
        ));
        assert!(report.contains(&format!("[stderr:{digest_line}-{naming_line}]")));
        assert!(report.contains(&format!(
            "WARN: FromAsCasing: 'as' and 'FROM' keywords' casing do not match (line 1) [stderr:{warn_line}]"
        )));
        assert!(report.contains(&format!(
            "#9 [web 4/4] RUN npm ci && npm run build · 6.1s (142 lines) [stderr:{npm_start}-{npm_end}]"
        )));
        assert!(report.contains(&format!(
            "#13 [builder 4/4] RUN cargo build --release · 15.3s (181 lines) [stderr:{cargo_start}-{cargo_end}]"
        )));
        assert!(report.contains("Collapsed: 5 internal, 6 cached, 4 metadata/export steps"));
    }

    #[test]
    fn failing_buildkit_build_deduplicates_echo_and_preserves_exact_errors_and_pointers() {
        let mut stderr = String::new();
        let mut ln = 0usize;
        let mut push = |s: &str| {
            stderr.push_str(s);
            stderr.push('\n');
            ln += 1;
            ln
        };

        push("#1 [internal] load build definition from Dockerfile");
        push("#1 transferring dockerfile: 640B done");
        push("#1 DONE 0.0s");
        push("#2 [internal] load metadata for docker.io/library/rust:1.83-slim");
        push("#2 DONE 0.4s");
        push("#3 [builder 1/4] FROM docker.io/library/rust:1.83-slim");
        push("#3 CACHED");
        push("#4 [builder 2/4] WORKDIR /app");
        push("#4 CACHED");
        push("#5 [builder 3/4] COPY . .");
        push("#5 DONE 0.1s");

        let fail_start = push("#6 [builder 4/4] RUN cargo build --release");
        for i in 0..110 {
            push(&format!(
                "#6 {:.3}    Compiling dep-{i:03} v1.0.0",
                0.1 + i as f64 * 0.05
            ));
        }
        let err1_line = push("#6 5.901 error[E0308]: mismatched types");
        push("#6 5.901   --> src/handler.rs:42:18");
        push("#6 5.901    |");
        push("#6 5.901 42 |     let port: u16 = \"8080\";");
        push("#6 5.901    |               ---   ^^^^^^ expected `u16`, found `&str`");
        for i in 110..145 {
            push(&format!(
                "#6 {:.3}    Compiling dep-{i:03} v1.0.0",
                6.0 + (i - 110) as f64 * 0.04
            ));
        }
        let err2_line =
            push("#6 7.800 error[E0599]: no method named `finish` found for struct `Order`");
        push("#6 7.800   --> src/order.rs:19:15");
        push("#6 7.800    |");
        push("#6 7.800 19 |         order.finish();");
        push("#6 7.800    |               ^^^^^^ method not found in `Order`");
        push("#6 7.805 error: could not compile `myapp` (bin \"myapp\") due to 2 previous errors");
        let err_status_line = push(
            "#6 ERROR: process \"/bin/sh -c cargo build --release\" did not complete successfully: exit code: 101",
        );

        // BuildKit's duplicated `------` error echo block!
        push("------");
        push(" > [builder 4/4] RUN cargo build --release:");
        push("5.901 error[E0308]: mismatched types");
        push("5.901   --> src/handler.rs:42:18");
        push("5.901    |");
        push("5.901 42 |     let port: u16 = \"8080\";");
        push("5.901    |               ---   ^^^^^^ expected `u16`, found `&str`");
        push("7.800 error[E0599]: no method named `finish` found for struct `Order`");
        push("7.800   --> src/order.rs:19:15");
        push("7.800    |");
        push("7.800 19 |         order.finish();");
        push("7.800    |               ^^^^^^ method not found in `Order`");
        push("7.805 error: could not compile `myapp` (bin \"myapp\") due to 2 previous errors");
        push("------");
        push("Dockerfile:14");
        push("--------------------");
        push("  12 |     WORKDIR /app");
        push("  13 |     COPY . .");
        let df_highlight_line = push("  14 | >>> RUN cargo build --release");
        push("--------------------");
        push(
            "ERROR: failed to solve: process \"/bin/sh -c cargo build --release\" did not complete successfully: exit code: 101",
        );

        let report = format("", &stderr, Some(1), 4096).expect("should compact failed build");
        assert!(report.len() <= 4096 && report.len() < stderr.len());
        assert_all_pointers_valid(&report, "", &stderr);

        // Deduplicated: each compiler error appears ONCE, not repeated from `------`.
        assert_eq!(report.matches("error[E0308]: mismatched types").count(), 1);
        assert_eq!(
            report
                .matches("error[E0599]: no method named `finish`")
                .count(),
            1
        );

        // Exact pointers verified against actual lines!
        assert!(stream_line(&stderr, err1_line).contains("error[E0308]"));
        assert!(stream_line(&stderr, err2_line).contains("error[E0599]"));
        assert!(report.contains(&format!("Failed: #6 [builder 4/4] RUN cargo build --release [stderr:{fail_start}-{err_status_line}]")));
        assert!(report.contains(&format!(
            "Dockerfile:14: >>> RUN cargo build --release [stderr:{df_highlight_line}]"
        )));
        assert!(report.contains(&format!("[stderr:{err_status_line}]")));
        assert!(report.contains("Prior steps: 5 (2 cached, 1 ran, 2 internal)"));
        assert!(report.contains("omitted"));
        assert!(!report.contains("Compiling dep-110"));
        assert!(!report.contains("Compiling dep-144"));

        // Also works when exit_code is None (e.g. masked by a pipe in postroute.rs)!
        let masked_report =
            format("", &stderr, None, 4096).expect("detects failure even with None exit");
        assert!(masked_report.contains("FAILED:"));
        assert_eq!(
            masked_report
                .matches("error[E0308]: mismatched types")
                .count(),
            1
        );

        // Strict tight budget: never exceeds budget and still preserves failing step header & error!
        let tight = format("", &stderr, Some(1), 480).expect("fits tight budget");
        assert!(
            tight.len() <= 480,
            "exceeded budget: {} > 480\n{tight}",
            tight.len()
        );
        assert!(tight.contains("Failed: #6"));
        assert!(tight.contains("error[E0308]") || tight.contains("ERROR:"));
    }

    #[test]
    fn docker_compose_multiservice_buildkit_clean_and_failing() {
        let mut stderr = String::new();
        let mut ln = 0usize;
        let mut push = |s: &str| {
            stderr.push_str(s);
            stderr.push('\n');
            ln += 1;
            ln
        };

        push("#1 [api internal] load build definition from Dockerfile");
        push("#1 transferring dockerfile: 512B done");
        push("#1 DONE 0.0s");
        push("#2 [worker internal] load build definition from Dockerfile.worker");
        push("#2 transferring dockerfile: 410B done");
        push("#2 DONE 0.0s");
        push("#3 [api internal] load metadata for docker.io/library/golang:1.23-alpine");
        push("#3 DONE 0.5s");
        push("#4 [worker internal] load metadata for docker.io/library/python:3.12-slim");
        push("#4 DONE 0.6s");
        push("#5 [api builder 1/3] FROM docker.io/library/golang:1.23-alpine");
        push("#5 CACHED");
        push("#6 [worker 1/3] FROM docker.io/library/python:3.12-slim");
        push("#6 CACHED");
        push("#7 [api builder 2/3] COPY . .");
        push("#7 DONE 0.1s");
        push("#8 [worker 2/3] COPY requirements.txt .");
        push("#8 CACHED");
        push("#9 [api builder 3/3] RUN go build -o /app/api ./cmd/api");
        push("#10 [worker 3/3] RUN pip install -r requirements.txt");
        for i in 0..45 {
            push(&format!(
                "#9 {:.3} go: downloading pkg-{i:02} v1.2.3",
                0.2 + i as f64 * 0.05
            ));
            push(&format!(
                "#10 {:.3} Collecting wheel-{i:02}==2.0.{i}",
                0.2 + i as f64 * 0.06
            ));
        }
        push("#9 DONE 3.4s");
        push("#10 DONE 4.8s");
        push("#11 [api] exporting to image");
        push("#11 exporting layers 0.6s done");
        push(
            "#11 writing image sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa done",
        );
        push("#11 naming to docker.io/library/proj-api:latest done");
        push("#11 DONE 0.7s");
        push("#12 [worker] exporting to image");
        push("#12 exporting layers 0.5s done");
        push(
            "#12 writing image sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb done",
        );
        push("#12 naming to docker.io/library/proj-worker:latest done");
        push("#12 DONE 0.6s");
        push(" ✔ Service api     Built");
        push(" ✔ Service worker  Built");

        let cmd = vec![
            "docker".to_owned(),
            "compose".to_owned(),
            "build".to_owned(),
        ];
        assert!(matches(&cmd, "", &stderr));

        let report = format("", &stderr, Some(0), 4096).expect("compacts compose build");
        assert!(report.len() <= 4096 && report.len() < stderr.len());
        assert_all_pointers_valid(&report, "", &stderr);
        assert!(report.contains("docker compose build (2 services: api, worker)"));
        assert!(report.contains("Image [api]: docker.io/library/proj-api:latest"));
        assert!(report.contains("Image [worker]: docker.io/library/proj-worker:latest"));
        assert!(report.contains("#9 [api builder 3/3] RUN go build"));
        assert!(report.contains("#10 [worker 3/3] RUN pip install"));

        // Now test Compose build failure on `worker` while `api` was canceled.
        let mut fail_stderr = stderr.lines().take(20).collect::<Vec<_>>().join("\n");
        fail_stderr.push('\n');
        for i in 0..40 {
            let _ = writeln!(
                fail_stderr,
                "#10 {:.3} Collecting dep-{i:02}==1.0.0",
                0.1 + i as f64 * 0.04
            );
        }
        fail_stderr.push_str("#10 2.410 ERROR: Could not find a version that satisfies the requirement nonexistent-pkg==9.9.9\n");
        fail_stderr.push_str(
            "#10 2.411 ERROR: No matching distribution found for nonexistent-pkg==9.9.9\n",
        );
        fail_stderr.push_str("#10 ERROR: process \"/bin/sh -c pip install -r requirements.txt\" did not complete successfully: exit code: 1\n");
        fail_stderr.push_str("#9 CANCELED\n");
        fail_stderr.push_str("------\n");
        fail_stderr.push_str(" > [worker 3/3] RUN pip install -r requirements.txt:\n");
        fail_stderr.push_str("2.410 ERROR: Could not find a version that satisfies the requirement nonexistent-pkg==9.9.9\n");
        fail_stderr
            .push_str("2.411 ERROR: No matching distribution found for nonexistent-pkg==9.9.9\n");
        fail_stderr.push_str("------\n");
        fail_stderr.push_str("failed to solve: process \"/bin/sh -c pip install -r requirements.txt\" did not complete successfully: exit code: 1\n");

        let fail_report = format("", &fail_stderr, Some(1), 4096).expect("compacts failed compose");
        assert!(fail_report.len() <= 4096 && fail_report.len() < fail_stderr.len());
        assert_all_pointers_valid(&fail_report, "", &fail_stderr);
        assert!(
            fail_report.contains("docker compose build (2 services: api, worker) FAILED (exit 1)")
        );
        assert!(fail_report.contains("Failed: #10 [worker 3/3] RUN pip install"));
        assert_eq!(
            fail_report
                .matches(
                    "Could not find a version that satisfies the requirement nonexistent-pkg==9.9.9"
                )
                .count(),
            1
        );
    }

    #[test]
    fn classic_docker_and_podman_builds_clean_and_failing() {
        let mut stdout = String::new();
        stdout.push_str("Sending build context to Docker daemon  128.0kB\n");
        stdout.push_str("Step 1/8 : FROM node:20-alpine AS builder\n");
        stdout.push_str(" ---> 9c8a7b6e5d4f\n");
        stdout.push_str("Step 2/8 : WORKDIR /app\n");
        stdout.push_str(" ---> Using cache\n");
        stdout.push_str(" ---> 1a2b3c4d5e6f\n");
        stdout.push_str("Step 3/8 : COPY package*.json ./\n");
        stdout.push_str(" ---> Using cache\n");
        stdout.push_str(" ---> 2b3c4d5e6f7a\n");
        stdout.push_str("Step 4/8 : RUN npm ci\n");
        stdout.push_str(" ---> Running in 8f9e0d1c2b3a\n");
        for i in 0..80 {
            let _ = writeln!(stdout, "added package-{i:03}@1.0.0 from registry");
        }
        stdout.push_str("Removing intermediate container 8f9e0d1c2b3a\n");
        stdout.push_str(" ---> 3c4d5e6f7a8b\n");
        stdout.push_str("Step 5/8 : COPY . .\n");
        stdout.push_str(" ---> 4d5e6f7a8b9c\n");
        stdout.push_str("Step 6/8 : RUN npm run build\n");
        stdout.push_str(" ---> Running in 7e8f9a0b1c2d\n");
        for i in 0..40 {
            let _ = writeln!(stdout, "bundling chunk-{i:02}.js 12.4 kB");
        }
        stdout.push_str("Removing intermediate container 7e8f9a0b1c2d\n");
        stdout.push_str(" ---> 5e6f7a8b9c0d\n");
        stdout.push_str("Step 7/8 : FROM nginx:1.27-alpine\n");
        stdout.push_str(" ---> Using cache\n");
        stdout.push_str(" ---> 6f7a8b9c0d1e\n");
        stdout.push_str("Step 8/8 : COPY --from=builder /app/dist /usr/share/nginx/html\n");
        stdout.push_str(" ---> 7a8b9c0d1e2f\n");
        stdout.push_str("Successfully built 7a8b9c0d1e2f\n");
        stdout.push_str("Successfully tagged myapp:latest\n");

        let clean = format(&stdout, "", Some(0), 4096).expect("compacts classic clean build");
        assert!(clean.len() <= 4096 && clean.len() < stdout.len());
        assert_all_pointers_valid(&clean, &stdout, "");
        assert!(clean.contains("docker build: 8 steps (3 cached, 5 ran)"));
        assert!(clean.contains("Image: myapp:latest (7a8b9c0d1e2f)"));
        assert!(clean.contains("Step 4/8 RUN npm ci (80 lines)"));
        assert!(clean.contains("Step 6/8 RUN npm run build (40 lines)"));

        // Failing classic build.
        let mut fail_out = String::new();
        fail_out.push_str("Sending build context to Docker daemon  64.0kB\n");
        fail_out.push_str("Step 1/5 : FROM node:20-alpine\n");
        fail_out.push_str(" ---> 9c8a7b6e5d4f\n");
        fail_out.push_str("Step 2/5 : WORKDIR /app\n");
        fail_out.push_str(" ---> Using cache\n");
        fail_out.push_str(" ---> 1a2b3c4d5e6f\n");
        fail_out.push_str("Step 3/5 : COPY . .\n");
        fail_out.push_str(" ---> 2b3c4d5e6f7a\n");
        fail_out.push_str("Step 4/5 : RUN npm run build\n");
        fail_out.push_str(" ---> Running in 8f9e0d1c2b3a\n");
        for i in 0..60 {
            let _ = writeln!(fail_out, "checking module-{i:02}.ts ... ok");
        }
        fail_out.push_str(
            "src/server.ts(42,10): error TS2339: Property 'foo' does not exist on type 'Bar'.\n",
        );
        fail_out.push_str("The command '/bin/sh -c npm run build' returned a non-zero code: 2\n");

        let failed = format(&fail_out, "", Some(2), 4096).expect("compacts classic failed build");
        assert!(failed.len() <= 4096 && failed.len() < fail_out.len());
        assert_all_pointers_valid(&failed, &fail_out, "");
        assert!(failed.contains("docker build FAILED (exit 2)"));
        assert!(failed.contains("Failed: Step 4/5 RUN npm run build"));
        assert!(failed.contains("error TS2339: Property 'foo' does not exist on type 'Bar'"));
        assert!(failed.contains("returned a non-zero code: 2"));

        // Podman build format.
        let mut podman = String::new();
        podman.push_str("[1/2] STEP 1/4: FROM docker.io/library/rust:1.83-slim AS builder\n");
        podman.push_str("[1/2] STEP 2/4: WORKDIR /app\n");
        podman.push_str("--> Using cache 9a8b7c6d5e4f\n");
        podman.push_str("--> 9a8b7c6d5e4f\n");
        podman.push_str("[1/2] STEP 3/4: COPY . .\n");
        podman.push_str("--> 1b2c3d4e5f6a\n");
        podman.push_str("[1/2] STEP 4/4: RUN cargo build --release\n");
        for i in 0..50 {
            let _ = writeln!(podman, "   Compiling dep-{i:02} v0.1.0");
        }
        podman.push_str("--> 4d5e6f7a8b9c\n");
        podman.push_str("COMMIT localhost/myapp:v1\n");
        podman.push_str("--> a1b2c3d4e5f6\n");
        podman.push_str("Successfully tagged localhost/myapp:v1\n");
        podman.push_str("a1b2c3d4e5f67890abcdef1234567890abcdef1234567890abcdef1234567890\n");

        let pod_report = format(&podman, "", Some(0), 4096).expect("compacts podman build");
        assert!(pod_report.len() < podman.len());
        assert_all_pointers_valid(&pod_report, &podman, "");
        assert!(pod_report.starts_with("podman build:"));
        assert!(pod_report.contains("Image: localhost/myapp:v1"));
    }

    #[test]
    fn merged_stdout_stream_uses_stdout_pointers_and_strips_ansi_without_shifting_lines() {
        let mut stdout = String::new();
        stdout.push_str("\x1b[36m#1 [internal] load build definition from Dockerfile\x1b[0m\n");
        stdout.push_str("#1 transferring dockerfile: 210B done\n");
        stdout.push_str("#1 DONE 0.0s\n");
        stdout.push_str("#2 [1/2] FROM alpine:3.20\n");
        stdout.push_str("#2 CACHED\n");
        stdout.push_str("#3 [2/2] RUN apk add curl\n");
        for i in 0..40 {
            let _ = writeln!(
                stdout,
                "#3 {:.3} \x1b[32m({}/40) Installing pkg-{i:02} (1.0-r0)\x1b[0m\r#3 {:.3} ({}/40) Installed pkg-{i:02}",
                0.1 + i as f64 * 0.02,
                i + 1,
                0.1 + i as f64 * 0.02,
                i + 1,
            );
        }
        stdout.push_str("#3 DONE 1.4s\n");
        stdout.push_str("#4 exporting to image\n");
        stdout.push_str("#4 writing image sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc done\n");
        stdout.push_str("#4 naming to docker.io/library/tiny:latest done\n");
        stdout.push_str("#4 DONE 0.2s\n");

        let report = format(&stdout, "", Some(0), 4096).expect("compacts stdout BuildKit");
        assert_all_pointers_valid(&report, &stdout, "");
        assert!(report.contains("[stdout:"));
        assert!(!report.contains("[stderr:"));
        assert!(report.contains("#3 [2/2] RUN apk add curl · 1.4s (40 lines) [stdout:6-47]"));
    }

    #[test]
    fn command_matching_and_fail_closed_invariants() {
        // Command classification
        assert!(is_build_command(&["docker", "build", "."]));
        assert!(is_build_command(&[
            "docker",
            "--context",
            "prod",
            "build",
            "-t",
            "x",
            "."
        ]));
        assert!(is_build_command(&[
            "docker",
            "buildx",
            "build",
            "--platform",
            "linux/amd64",
            "."
        ]));
        assert!(is_build_command(&[
            "docker",
            "compose",
            "-f",
            "compose.yml",
            "build"
        ]));
        assert!(is_build_command(&["docker", "compose", "up", "--build"]));
        assert!(is_build_command(&["docker-compose", "build", "api"]));
        assert!(is_build_command(&["podman", "build", "-t", "app", "."]));
        assert!(is_build_command(&["buildah", "bud", "."]));
        assert!(is_build_command(&[
            "env",
            "DOCKER_BUILDKIT=1",
            "docker",
            "build",
            "."
        ]));

        // Disallowed flags / non-build commands
        assert!(!is_build_command(&["docker", "build", "-q", "."]));
        assert!(!is_build_command(&["docker", "build", "--quiet", "."]));
        assert!(!is_build_command(&[
            "docker",
            "build",
            "--progress=rawjson",
            "."
        ]));
        assert!(!is_build_command(&[
            "docker",
            "build",
            "--progress",
            "rawjson",
            "."
        ]));
        assert!(!is_build_command(&["docker", "buildx", "bake", "--print"]));
        assert!(!is_build_command(&["docker", "ps"]));
        assert!(!is_build_command(&["cargo", "build"]));

        // Non-BuildKit inputs fail closed.
        let non_build_samples = [
            "running 12 tests\ntest a ... ok\ntest b ... ok\n",
            "#1 Heading one\n#2 Heading two\n#3 Heading three\nsome markdown text\n",
            "Step 1/3 : initializing database\nStep 2/3 : seeding rows\nStep 3/3 : ready\n",
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock.\n",
            "{\"status\":\"ok\",\"steps\":5}\n",
            "#1 [internal] load build definition\n\0binary\0payload\n",
        ];
        for sample in non_build_samples {
            assert!(!matches(&[], sample, ""), "false positive on:\n{sample}");
            assert!(
                format(sample, "", Some(0), 4096).is_none(),
                "should fail closed on:\n{sample}"
            );
        }

        // Command with --progress=rawjson rejects even if output somehow looks like BuildKit.
        let valid_bk = "#1 [internal] load build definition from Dockerfile\n#1 DONE 0.0s\n#2 [1/1] FROM alpine\n#2 CACHED\n#3 exporting to image\n#3 DONE 0.1s\n";
        assert!(!matches(
            &[
                "docker".to_owned(),
                "build".to_owned(),
                "--progress=rawjson".to_owned(),
                ".".to_owned(),
            ],
            "",
            valid_bk
        ));

        // Tiny BuildKit output where formatted report is not strictly smaller returns None.
        let tiny = "#1 [1/1] FROM alpine\n#1 CACHED\n";
        assert!(format("", tiny, Some(0), 4096).is_none());

        // Zero budget returns None.
        assert!(format("", valid_bk, Some(0), 0).is_none());
    }

    #[test]
    fn buildx_warnings_dedup_and_truncated_header_recovery() {
        let mut stderr = String::new();
        stderr.push_str("#1 [internal] load build definition from Dockerfile\n");
        stderr.push_str(
            "#1 WARN: FromAsCasing: 'as' and 'FROM' keywords' casing do not match (line 1)\n",
        );
        stderr.push_str("#1 DONE 0.0s\n");
        stderr.push_str("#2 [1/3] FROM python:3.12-slim\n");
        stderr.push_str("#2 CACHED\n");
        // Step #3 header intentionally omitted (simulating truncated head); output + ERROR + repeat block exist.
        for i in 0..35 {
            let _ = writeln!(
                stderr,
                "#3 {:.3} Collecting pkg-{i:02}==1.0.0",
                0.1 + i as f64 * 0.04
            );
        }
        stderr.push_str("#3 2.100 ERROR: Failed building wheel for native-ext\n");
        stderr.push_str("#3 ERROR: process \"/bin/sh -c pip install .\" did not complete successfully: exit code: 1\n");
        stderr.push_str("------\n");
        stderr.push_str(" > [2/3] RUN pip install .:\n");
        stderr.push_str("2.100 ERROR: Failed building wheel for native-ext\n");
        stderr.push_str("------\n");
        stderr.push_str("2 warnings found (use docker --debug to expand):\n");
        stderr
            .push_str(" - FromAsCasing: 'as' and 'FROM' keywords' casing do not match (line 1)\n");
        stderr.push_str(" - LegacyKeyValueFormat: \"ENV key=value\" should be used instead of legacy \"ENV key value\" format (line 4)\n");
        stderr.push_str("ERROR: failed to solve: process \"/bin/sh -c pip install .\" did not complete successfully: exit code: 1\n");

        let report =
            format("", &stderr, Some(1), 4096).expect("recovers header and deduplicates warnings");
        assert!(report.len() <= 4096 && report.len() < stderr.len());
        assert_all_pointers_valid(&report, "", &stderr);
        assert!(report.contains("Failed: #3 [2/3] RUN pip install ."));
        assert_eq!(report.matches("FromAsCasing:").count(), 1);
        assert_eq!(report.matches("LegacyKeyValueFormat:").count(), 1);
        assert_eq!(
            report
                .matches("Failed building wheel for native-ext")
                .count(),
            1
        );
    }

    #[test]
    fn parallel_dual_step_failure_preserves_both_failures() {
        let mut stderr = String::new();
        stderr.push_str("#1 [api internal] load build definition from Dockerfile.api\n");
        stderr.push_str("#1 DONE 0.0s\n");
        stderr.push_str("#2 [web internal] load build definition from Dockerfile.web\n");
        stderr.push_str("#2 DONE 0.0s\n");
        stderr.push_str("#3 [api 1/2] RUN cargo build\n");
        stderr.push_str("#4 [web 1/2] RUN npm run build\n");
        for i in 0..40 {
            let _ = writeln!(
                stderr,
                "#3 {:.3}    Compiling crate-{i:02} v0.1.0",
                0.1 + i as f64 * 0.05
            );
            let _ = writeln!(
                stderr,
                "#4 {:.3} transforming module-{i:02}.ts",
                0.1 + i as f64 * 0.05
            );
        }
        stderr.push_str("#3 2.500 error[E0425]: cannot find value `missing_id` in this scope\n");
        stderr.push_str("#3 2.500   --> src/main.rs:12:5\n");
        stderr.push_str("#3 ERROR: process \"/bin/sh -c cargo build\" did not complete successfully: exit code: 101\n");
        stderr
            .push_str("#4 2.600 src/App.tsx(8,3): error TS2304: Cannot find name 'BrokenProp'.\n");
        stderr.push_str("#4 ERROR: process \"/bin/sh -c npm run build\" did not complete successfully: exit code: 2\n");
        stderr.push_str("------\n");
        stderr.push_str(" > [api 1/2] RUN cargo build:\n");
        stderr.push_str("2.500 error[E0425]: cannot find value `missing_id` in this scope\n");
        stderr.push_str("2.500   --> src/main.rs:12:5\n");
        stderr.push_str("------\n");
        stderr.push_str(" > [web 1/2] RUN npm run build:\n");
        stderr.push_str("2.600 src/App.tsx(8,3): error TS2304: Cannot find name 'BrokenProp'.\n");
        stderr.push_str("------\n");

        let report = format("", &stderr, Some(1), 4096).expect("compacts dual failure");
        assert!(report.len() <= 4096 && report.len() < stderr.len());
        assert_all_pointers_valid(&report, "", &stderr);
        assert!(report.contains("Failed: #3 [api 1/2] RUN cargo build"));
        assert!(report.contains("Failed: #4 [web 1/2] RUN npm run build"));
        assert_eq!(report.matches("error[E0425]").count(), 1);
        assert_eq!(report.matches("error TS2304").count(), 1);
    }

    #[test]
    fn format_for_command_applies_podman_label_and_rejects_quiet() {
        let mut stderr = String::new();
        stderr.push_str("#1 [internal] load build definition from Containerfile\n");
        stderr.push_str("#1 DONE 0.0s\n");
        stderr.push_str("#2 [1/2] FROM alpine:3.20\n");
        stderr.push_str("#2 CACHED\n");
        stderr.push_str("#3 [2/2] RUN apk add git\n");
        for i in 0..30 {
            let _ = writeln!(
                stderr,
                "#3 {:.3} Installing dep-{i:02}",
                0.1 + i as f64 * 0.02
            );
        }
        stderr.push_str("#3 DONE 1.2s\n");
        stderr.push_str("#4 exporting to image\n");
        stderr.push_str("#4 naming to localhost/tool:latest done\n");
        stderr.push_str("#4 DONE 0.1s\n");

        let report = format_for_command(&["podman", "build", "."], "", &stderr, Some(0), 4096)
            .expect("formats podman buildkit");
        assert!(report.starts_with("podman build:"));
        assert!(report.contains("Image: localhost/tool:latest"));
        assert_all_pointers_valid(&report, "", &stderr);

        assert!(
            format_for_command(&["podman", "build", "-q", "."], "", &stderr, Some(0), 4096)
                .is_none()
        );
    }

    #[test]
    fn ansi_fast_path_and_plain_parity_with_cached_and_failing_steps() {
        // Zero-alloc Cow::Borrowed fast path on plain lines (even with bare `\r` progress).
        assert!(matches!(
            clean_line("#2 CACHED\n"),
            Cow::Borrowed("#2 CACHED")
        ));
        assert!(matches!(
            clean_line("#3 0.200 step 1\r#3 0.210 step 2\n"),
            Cow::Borrowed("#3 0.210 step 2")
        ));
        assert!(matches!(
            clean_line("\x1b[36m#2 CACHED\x1b[0m\n"),
            Cow::Owned(ref s) if s == "#2 CACHED"
        ));

        let build_log = |colored: bool| -> String {
            let (cyan, red, reset) = if colored {
                ("\x1b[36m", "\x1b[31m", "\x1b[0m")
            } else {
                ("", "", "")
            };
            let mut s = String::new();
            let _ = writeln!(
                s,
                "{cyan}#1 [internal] load build definition from Dockerfile{reset}"
            );
            s.push_str("#1 DONE 0.0s\n");
            s.push_str("#2 [builder 1/4] FROM rust:1.91-slim\n");
            s.push_str("#2 CACHED\n");
            s.push_str("#3 [builder 2/4] COPY Cargo.toml Cargo.lock ./\n");
            s.push_str("#3 CACHED\n");
            let _ = writeln!(s, "{cyan}#4 [builder 3/4] RUN cargo build --release{reset}");
            for i in 0..35 {
                let _ = writeln!(
                    s,
                    "#4 {:.3} Downloading crate-{i:02}\r#4 {:.3}    Compiling crate-{i:02} v0.1.0",
                    0.1 + i as f64 * 0.03,
                    0.11 + i as f64 * 0.03,
                );
            }
            let _ = writeln!(s, "#4 2.400 {red}error[E0308]{reset}: mismatched types");
            s.push_str("#4 2.400   --> src/billing.rs:88:17\n");
            s.push_str("#4 2.400    |\n");
            s.push_str("#4 2.400 88 |     let total: u64 = \"bad\";\n");
            s.push_str("#4 2.400    |                ---   ^^^^^ expected `u64`, found `&str`\n");
            let _ = writeln!(
                s,
                "#4 {red}ERROR: process \"/bin/sh -c cargo build --release\" did not complete successfully: exit code: 101{reset}"
            );
            s.push_str("------\n");
            s.push_str(" > [builder 3/4] RUN cargo build --release:\n");
            let _ = writeln!(s, "2.400 {red}error[E0308]{reset}: mismatched types");
            s.push_str("2.400   --> src/billing.rs:88:17\n");
            s.push_str("------\n");
            s.push_str("ERROR: failed to solve: process \"/bin/sh -c cargo build --release\" did not complete successfully: exit code: 101\n");
            s
        };

        let plain = build_log(false);
        let colored = build_log(true);
        let plain_out = format("", &plain, Some(1), 4096).expect("plain buildkit compacts");
        let color_out = format("", &colored, Some(1), 4096).expect("colored buildkit compacts");
        assert_eq!(
            plain_out, color_out,
            "ANSI fast path must match plain output identically"
        );
        assert_all_pointers_valid(&plain_out, "", &plain);
        assert!(
            plain_out.contains("error[E0308]: mismatched types"),
            "{plain_out}"
        );
        assert!(
            plain_out.contains("--> src/billing.rs:88:17"),
            "{plain_out}"
        );
        assert_eq!(plain_out.matches("error[E0308]").count(), 1, "{plain_out}");
    }
}
