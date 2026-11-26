//! Compact records point back to inclusive line ranges in the original stdout.
use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default budget for the middle "Compact Diff" tier when `Summary::render` does not
/// receive an explicit budget from `capture.rs`.
pub(crate) const COMPACT_DIFF_BUDGET: usize = 4096;
/// Hard cap on buffered compact diff lines across non-lockfile files so huge multi-MB
/// diffs stay strictly bounded in memory.
const COMPACT_BUFFER_CAP: usize = 32 * 1024;
/// Per-file buffer cap for lockfile hunks before dropping them (lockfiles are replaced
/// by `lockdiff::summarize` anyway, so an oversized lockfile must not push code files
/// out of the compact diff tier).
const LOCKFILE_BUFFER_CAP: usize = 8 * 1024;
/// Buffer cap for a single contiguous `-`/`+` block while verifying whether a large
/// hunk is pure whitespace/reindent noise.
const WS_BLOCK_BUFFER_CAP: usize = 512 * 1024;

/// Structured details for Terraform, OpenTofu and Pulumi plan summaries.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlanDetails {
    #[serde(default, skip_serializing_if = "is_false")]
    pub pulumi: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub refreshed: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub refreshed_first: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub refreshed_last: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unchanged: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unchanged_first: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unchanged_last: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}

/// Text in a summary comes from captured output. Records written before redaction can
/// hold raw secrets, so code that prints a stored summary must pass it through
/// `redact::Redactor`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Summary {
    pub kind: String,
    pub items: Vec<Item>,
    pub outcomes: Vec<String>,
    /// `cargo test` totals per test binary, when they account for every line of
    /// both streams. The report then stands in for both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo: Option<crate::cargo_tests::Totals>,
    /// Structured JS/TS or Playwright test report, when recognized output
    /// accounts for both streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub js: Option<crate::js_tests::Report>,
    /// Structured Terraform, OpenTofu or Pulumi plan details (refreshes,
    /// unchanged counts, output changes and diagnostics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanDetails>,
    /// Total raw bytes observed for a diff/patch run (in-memory only).
    #[serde(skip)]
    pub(crate) diff_raw_bytes: usize,
    /// Whether non-lockfile compact diff lines exceeded `COMPACT_BUFFER_CAP`.
    #[serde(skip)]
    pub(crate) compact_overflowed: bool,
    /// Structured commit/file entries for tiered diff rendering (in-memory only).
    #[serde(skip)]
    pub(crate) diff_entries: Vec<DiffEntry>,
}

#[derive(Clone, Debug)]
pub(crate) enum DiffEntry {
    Commit { item_index: usize },
    File(DiffFile),
}

#[derive(Clone, Debug)]
pub(crate) struct DiffFile {
    path: String,
    first: usize,
    last: usize,
    added: usize,
    deleted: usize,
    meta_lines: Vec<String>,
    /// Compact hunks (1-line trimmed context), or `None` if dropped due to cap.
    hunks: Option<Vec<CompactHunk>>,
    items_start: usize,
    items_end: usize,
    generated_folded: bool,
    /// `(old_path, new_path)` for a 100% pure file rename with no content or mode edits.
    pure_rename: Option<(String, String)>,
    /// First stdout line number of each hunk confirmed to be whitespace/indent only.
    whitespace_hunk_firsts: Vec<usize>,
}

#[derive(Clone, Debug)]
struct CompactHunk {
    header: String,
    lines: Vec<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub name: String,
    pub detail: String,
    pub digest: String,
    pub first: usize,
    pub last: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub stderr: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub added: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub deleted: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub file_first: usize,
}

pub fn family(args: &[OsString]) -> String {
    let all: Vec<_> = args.iter().map(|a| a.to_str().unwrap_or("")).collect();
    let owned = crate::wrappers::effective(&all);
    let refs: Vec<&str> = match &owned {
        Some(words) => words.iter().map(String::as_str).collect(),
        None => all.clone(),
    };
    let tool = crate::filters::unwrap_tool(&refs);
    let mut words: Vec<_> = tool.iter().take(4).copied().collect();
    if let Some(command) = words.first_mut() {
        *command = std::path::Path::new(*command)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
    }
    if words.first() == Some(&"cargo")
        && words
            .get(1)
            .is_some_and(|w| w.starts_with('+') && w.len() > 1)
    {
        words.remove(1);
    }
    words.truncate(3);
    // `git log` prints patches only when asked; other logs stay in `git`.
    let patch = || {
        tool.iter().skip(2).take_while(|w| **w != "--").any(|w| {
            matches!(*w, "-p" | "-u" | "--patch")
                || w.starts_with("-U")
                || w.starts_with("--unified=")
        })
    };
    let playwright_display = || {
        let mut iter = tool.iter().skip(2);
        while let Some(&arg) = iter.next() {
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg, None), |(f, v)| (f, Some(v)));
            match flag {
                "--list" | "--ui" | "--debug" | "--headed" | "--help" | "--version" => {
                    return false;
                }
                "--reporter" => {
                    let val = inline.or_else(|| iter.next().copied()).unwrap_or("");
                    if !matches!(val, "list" | "line" | "dot") {
                        return false;
                    }
                }
                _ => {}
            }
        }
        true
    };
    let rspec_display = || {
        let mut iter = tool.iter().skip(1);
        while let Some(&arg) = iter.next() {
            if arg == "--" {
                break;
            }
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg, None), |(f, v)| (f, Some(v)));
            match flag {
                "-h" | "--help" | "-v" | "--version" | "--dry-run" | "--init" => {
                    return false;
                }
                "-f" | "--format" => {
                    let val = inline.or_else(|| iter.next().copied()).unwrap_or("");
                    if !matches!(
                        val,
                        "p" | "progress" | "d" | "doc" | "documentation" | "f" | "failures"
                    ) {
                        return false;
                    }
                }
                _ if arg.starts_with("-f") && arg.len() > 2 => {
                    let val = &arg[2..];
                    if !matches!(
                        val,
                        "p" | "progress" | "d" | "doc" | "documentation" | "f" | "failures"
                    ) {
                        return false;
                    }
                }
                _ => {}
            }
        }
        true
    };
    let phpunit_display = |skip: usize| {
        for &arg in tool.iter().skip(skip) {
            if arg == "--" {
                break;
            }
            let flag = arg.split_once('=').map_or(arg, |(f, _)| f);
            if matches!(
                flag,
                "-h" | "--help"
                    | "--version"
                    | "--list-tests"
                    | "--list-suites"
                    | "--list-groups"
                    | "--teamcity"
                    | "--testdox-html"
                    | "--testdox-xml"
                    | "--log-junit"
                    | "--log-teamcity"
                    | "--log-events-text"
                    | "--log-events-verbose-text"
            ) {
                return false;
            }
        }
        true
    };
    let terraform_plan_display = |skip: usize, stop_at_dashdash: bool| {
        !tool
            .iter()
            .skip(skip)
            .take_while(|&&a| !stop_at_dashdash || a != "--")
            .any(|&arg| {
                let flag = arg.split_once('=').map_or(arg, |(f, _)| f);
                matches!(
                    flag,
                    "-json"
                        | "--json"
                        | "-h"
                        | "-help"
                        | "--help"
                        | "-version"
                        | "--version"
                        | "show"
                )
            })
    };
    let pulumi_display = || {
        !tool.iter().skip(2).take_while(|&&a| a != "--").any(|&arg| {
            let flag = arg.split_once('=').map_or(arg, |(f, _)| f);
            matches!(flag, "-j" | "--json" | "-h" | "--help" | "--version")
        })
    };
    let pulumi_dry_run = || {
        tool.iter()
            .skip(2)
            .take_while(|&&arg| arg != "--")
            .any(|&arg| arg == "--dry-run" || arg == "--dry-run=true")
    };
    match words.as_slice() {
        ["git", "diff", ..] | ["gh", "pr", "diff", ..] | ["glab", "mr", "diff", ..] => "git diff",
        ["git", "show", ..] => "git show",
        ["git", "log", ..] if patch() => "git log",
        ["cargo", "test", ..] => "cargo test",
        ["cargo", "nextest", "run", ..] => "cargo nextest",
        ["go", "test", ..] => "go test",
        ["npm", "test", ..] => "npm test",
        ["playwright", "test", ..] if playwright_display() => "playwright test",
        ["pytest", ..] | ["python" | "python3", "-m", "pytest"] => "pytest",
        ["python" | "python3" | "py", "-m", "unittest"] => "unittest",
        ["dotnet", "test", ..] => "dotnet test",
        ["mvn" | "mvnd" | "mvnw", ..]
            if tool
                .iter()
                .any(|&a| matches!(a, "test" | "verify" | "surefire:test")) =>
        {
            "mvn test"
        }
        ["gradle" | "gradlew", ..]
            if tool
                .iter()
                .any(|&a| a == "test" || a == "check" || a.ends_with(":test")) =>
        {
            "gradle test"
        }
        ["swift", "test", ..] => "swift test",
        ["ctest", ..] => "ctest",
        ["mix", "test", ..] => "mix test",
        ["deno", "test", ..] => "deno test",
        ["rspec", ..] if rspec_display() => "rspec",
        ["phpunit" | "paratest" | "pest", ..] if phpunit_display(1) => "phpunit",
        ["php", "artisan", "test", ..] if phpunit_display(3) => "phpunit",
        ["terraform" | "tofu", "plan", ..] if terraform_plan_display(2, true) => "terraform plan",
        ["terragrunt", "plan", ..] if terraform_plan_display(2, false) => "terraform plan",
        ["terragrunt", "run-all", "plan"] if terraform_plan_display(3, false) => "terraform plan",
        ["pulumi", "preview", ..] if pulumi_display() => "pulumi preview",
        ["pulumi", "up", ..] if pulumi_dry_run() && pulumi_display() => "pulumi preview",
        [cmd @ ("cargo" | "git" | "go" | "npm" | "rg"), ..] => cmd,
        _ => "other",
    }
    .into()
}

const LIMIT: usize = 2048;

pub struct Parser {
    inventory: Option<crate::inventory::Inventory>,
    cargo: Option<crate::cargo_tests::Scan>,
    js_tests: Option<crate::js_tests::Scan>,
    rspec: Option<RspecScan>,
    phpunit: Option<PhpunitScan>,
    plan: Option<PlanScan>,
    summary: Summary,
    hash: Sha256,
    file: String,
    pending: Option<Item>,
    failures: BTreeMap<String, Item>,
    current_test: String,
    /// Message lines still expected after `panicked at path:line:col:`.
    panic_lines: usize,
    /// `go test` package and per-test diagnostic state.
    go_tests: bool,
    go_ok: usize,
    go_no_tests: usize,
    go_ok_first: usize,
    go_ok_last: usize,
    go_ok_samples: Vec<String>,
    go_active: Option<(String, Vec<(usize, String)>)>,
    go_after_fail: Option<(String, usize)>,
    /// `git show` and `git log -p` output: a commit header before each patch.
    commits: bool,
    place: Place,
    /// Whether that output reached a file's patch, which a plain log never does.
    patched: bool,
    discarded: bool,
    /// The output can stop in the middle of an item, as `| head` leaves it.
    ends_early: bool,
    /// The old and new lines the current hunk's header announced and that have not
    /// appeared yet.
    hunk: Option<(usize, usize)>,
    /// Whether every hunk so far held the lines its header announced, as a patch
    /// whose lines no filter removed does.
    whole_hunks: bool,
    /// Parent count for combined merge diffs (`0` for standard `@@`, `>= 2` for `@@@`).
    combined_parents: usize,
    /// Raw bytes observed across a diff/patch stream.
    diff_raw_bytes: usize,
    /// Buffered commit and file entries for tiered diff rendering.
    diff_entries: Vec<DiffEntry>,
    /// Currently open file section while scanning a diff/patch.
    current_file: Option<ActiveDiffFile>,
    /// Total buffered compact diff bytes across non-lockfile files.
    compact_bytes: usize,
    /// Whether non-lockfile compact diff lines exceeded `COMPACT_BUFFER_CAP`.
    compact_overflowed: bool,
}

struct ActiveDiffFile {
    path: String,
    first: usize,
    last: usize,
    added: usize,
    deleted: usize,
    meta_lines: Vec<String>,
    saw_mode_header: bool,
    saw_sim_100: bool,
    rename_from: Option<String>,
    rename_to: Option<String>,
    saw_non_rename_meta: bool,
    saw_hunk: bool,
    hunks: Option<Vec<CompactHunk>>,
    current_hunk: Option<ActiveCompactHunk>,
    buffered_bytes: usize,
    items_start: usize,
    is_generated: bool,
    is_svg: bool,
    is_lockfile: bool,
    generated_folded: bool,
    svg_overflowed: bool,
    indent_significant: bool,
    whitespace_hunk_firsts: Vec<usize>,
}

struct ActiveCompactHunk {
    header: String,
    first: usize,
    last: usize,
    combined_parents: usize,
    raw_lines: Vec<String>,
    raw_bytes: usize,
    raw_overflowed: bool,
    cut_off: bool,
    ws_eligible: bool,
    ws_block_bytes: usize,
    ws_block_rem: Vec<String>,
    ws_block_add: Vec<String>,
    ws_added: usize,
    ws_deleted: usize,
}

/// Where a line of `git show` or `git log -p` output falls.
#[derive(Clone, Copy, PartialEq)]
enum Place {
    /// Before the first commit, or after the blank line that ends one.
    Between,
    /// `Author:`, `Date:` and the other fields after `commit SHA`.
    Fields,
    /// The indented message, after the blank line that ends the fields.
    Message,
    /// A `--stat` block between the commit message and its patch.
    Stat,
    /// Blank line after a `--stat` block, before `diff --git` / `diff --cc` or the next commit.
    AfterStat,
    /// A file's extended header lines, from `diff --git` / `diff --cc` to its first hunk.
    File,
    Hunk,
}

impl Parser {
    pub fn new(family: &str, profile: Option<crate::filters::Profile>) -> Self {
        let is_js_tests = profile == Some(crate::filters::Profile::JsTests)
            || (profile.is_none() && family == "playwright test");
        let commits = profile.is_none() && matches!(family, "git show" | "git log");
        let kind = match family {
            _ if is_js_tests => "tests",
            _ if profile.is_some() => "",
            "git diff" => "diff",
            _ if commits => "diff",
            "terraform plan" | "pulumi preview" => "plan",
            "pytest" | "cargo test" | "cargo nextest" | "go test" | "npm test" | "rspec"
            | "phpunit" => "tests",
            _ => "",
        };
        Self {
            inventory: profile
                .filter(|p| p.inventory())
                .map(crate::inventory::Inventory::new),
            cargo: match family {
                "cargo test" if profile.is_none() => Some(crate::cargo_tests::Scan::default()),
                "cargo nextest" if profile.is_none() => Some(crate::cargo_tests::Scan::nextest()),
                _ => None,
            },
            js_tests: is_js_tests.then(crate::js_tests::Scan::default),
            rspec: (family == "rspec" && profile.is_none()).then(RspecScan::default),
            phpunit: (family == "phpunit" && profile.is_none()).then(PhpunitScan::default),
            plan: if profile.is_none() {
                match family {
                    "terraform plan" => Some(PlanScan::terraform()),
                    "pulumi preview" => Some(PlanScan::pulumi()),
                    _ => None,
                }
            } else {
                None
            },
            summary: Summary {
                kind: kind.into(),
                ..Default::default()
            },
            hash: Sha256::new(),
            file: String::new(),
            pending: None,
            failures: BTreeMap::new(),
            current_test: String::new(),
            panic_lines: 0,
            go_tests: family == "go test" && profile.is_none(),
            go_ok: 0,
            go_no_tests: 0,
            go_ok_first: 0,
            go_ok_last: 0,
            go_ok_samples: Vec::new(),
            go_active: None,
            go_after_fail: None,
            commits,
            place: Place::Between,
            patched: false,
            discarded: false,
            ends_early: false,
            hunk: None,
            whole_hunks: true,
            combined_parents: 0,
            diff_raw_bytes: 0,
            diff_entries: Vec::new(),
            current_file: None,
            compact_bytes: 0,
            compact_overflowed: false,
        }
    }

    /// Read output that can stop in the middle of an item: a last hunk that holds
    /// fewer lines than its header announced is marked as cut off.
    pub fn ends_early(&mut self) {
        self.ends_early = true;
    }

    /// Count `line` against the current hunk's announced lines. A header or any
    /// line outside a hunk ends it, and a hunk header starts the next one.
    fn count_hunk(&mut self, line: &str) {
        if self.combined_parents >= 2 {
            if let Some(parents) = combined_hunk_parents(line) {
                self.combined_parents = parents;
                self.hunk = None;
                return;
            }
            if hunk_counts(line).is_some() {
                self.combined_parents = 0;
                self.hunk = hunk_counts(line);
                return;
            }
            if !line.starts_with([' ', '+', '-', '\\']) {
                self.combined_parents = 0;
            }
            return;
        }
        let remaining = match (self.hunk, line.as_bytes().first()) {
            (Some((old, new)), Some(b' ')) if old > 0 && new > 0 => Some((old - 1, new - 1)),
            (Some((old, new)), Some(b'-')) if old > 0 => Some((old - 1, new)),
            (Some((old, new)), Some(b'+')) if new > 0 => Some((old, new - 1)),
            (Some(counts), Some(b'\\')) => Some(counts),
            // A hunk line past the announced count, or a short hunk, means a filter
            // changed the patch.
            (Some(counts), first) => {
                self.whole_hunks &= counts == (0, 0) && !matches!(first, Some(b' ' | b'-' | b'+'));
                None
            }
            (None, _) => None,
        };
        if remaining.is_some() {
            self.hunk = remaining;
        } else if let Some(parents) = combined_hunk_parents(line) {
            self.combined_parents = parents;
            self.hunk = None;
        } else {
            self.hunk = hunk_counts(line);
        }
    }

    pub fn observe(&mut self, text: &str, number: usize) {
        if (self.summary.kind.is_empty() && self.inventory.is_none()) || self.discarded {
            return;
        }
        if text.len() >= 256 * 1024
            || text.contains('\0')
            || self.summary.items.len() + self.failures.len() >= LIMIT
            || self.summary.outcomes.len() >= 128
        {
            self.discarded = true;
            return;
        }
        if let Some(inventory) = &mut self.inventory {
            self.discarded = !inventory.observe(text, number);
            return;
        }
        if self.summary.kind == "diff" {
            self.diff_raw_bytes += text.len();
        }
        let line = text.trim_end_matches('\n');
        if self.commits {
            self.discarded = !self.commit(line, number);
            return;
        }
        if self.summary.kind == "diff"
            && self.file.is_empty()
            && !line.is_empty()
            && diff_header_path(line).is_none()
        {
            if is_stat_line(line) || is_stat_summary(line) || is_stat_mode_line(line) {
                return;
            }
            self.discarded = true;
            return;
        }
        if self.summary.kind == "diff" {
            self.diff(line, number);
        } else if let Some(plan) = &mut self.plan {
            plan.observe(line, number);
        } else if let Some(js) = &mut self.js_tests {
            js.observe(line, number, crate::capture::Stream::Stdout);
        } else if let Some(rspec) = &mut self.rspec {
            rspec.observe(line, number);
        } else if let Some(phpunit) = &mut self.phpunit {
            phpunit.observe(line, number);
        } else {
            if let Some(cargo) = &mut self.cargo {
                cargo.observe(line, number, crate::capture::Stream::Stdout);
            }
            self.test(line, number);
        }
    }

    /// Read a line of stderr, where `cargo test`, JS runners and Terraform/Pulumi
    /// plans print test output or diagnostics.
    pub fn observe_stderr(&mut self, text: &str, number: usize) {
        if let Some(cargo) = &mut self.cargo {
            cargo.observe(text, number, crate::capture::Stream::Stderr);
        }
        if let Some(js) = &mut self.js_tests
            && !self.discarded
        {
            if text.len() >= 256 * 1024 || text.contains('\0') {
                self.discarded = true;
                return;
            }
            js.observe(
                text.trim_end_matches('\n'),
                number,
                crate::capture::Stream::Stderr,
            );
        }
        if let Some(plan) = &mut self.plan
            && !self.discarded
        {
            if text.len() >= 256 * 1024 || text.contains('\0') {
                self.discarded = true;
                return;
            }
            plan.observe_stderr(text.trim_end_matches('\n'), number);
        }
    }

    fn flush(&mut self) {
        if let Some(mut item) = self.pending.take() {
            item.digest = format!("{:x}", self.hash.finalize_reset());
            self.summary.items.push(item);
        }
    }

    fn flush_compact_hunk(
        file: &mut ActiveDiffFile,
        compact_bytes: &mut usize,
        compact_overflowed: &mut bool,
    ) {
        let Some(mut hunk) = file.current_hunk.take() else {
            return;
        };
        if file.generated_folded {
            return;
        }
        let ws_only = !hunk.cut_off
            && hunk.combined_parents == 0
            && (hunk.ws_added > 0 || hunk.ws_deleted > 0)
            && hunk.ws_eligible
            && is_ws_only_block(
                &hunk.ws_block_rem,
                &hunk.ws_block_add,
                file.indent_significant,
            );
        if ws_only {
            file.whitespace_hunk_firsts.push(hunk.first);
            let Some(hunks) = file.hunks.as_mut() else {
                return;
            };
            let folded_header = format!(
                "{} (whitespace/indent only, -{} +{} lines) [stdout:{}-{}]",
                hunk.header, hunk.ws_deleted, hunk.ws_added, hunk.first, hunk.last
            );
            let added_bytes = folded_header.len() + 1;
            if file.is_lockfile {
                if file.buffered_bytes + added_bytes > LOCKFILE_BUFFER_CAP {
                    file.hunks = None;
                    return;
                }
                file.buffered_bytes += added_bytes;
            } else if file.is_svg {
                if *compact_bytes + added_bytes > COMPACT_BUFFER_CAP {
                    file.svg_overflowed = true;
                    file.hunks = None;
                    return;
                }
                *compact_bytes += added_bytes;
                file.buffered_bytes += added_bytes;
            } else {
                if *compact_bytes + added_bytes > COMPACT_BUFFER_CAP {
                    *compact_overflowed = true;
                    file.hunks = None;
                    return;
                }
                *compact_bytes += added_bytes;
                file.buffered_bytes += added_bytes;
            }
            hunks.push(CompactHunk {
                header: folded_header,
                lines: Vec::new(),
            });
            return;
        }
        if hunk.raw_overflowed {
            file.hunks = None;
            if file.is_svg {
                file.svg_overflowed = true;
            } else if !file.is_lockfile {
                *compact_overflowed = true;
            }
            return;
        }
        let Some(hunks) = file.hunks.as_mut() else {
            return;
        };
        hunk.ws_block_rem.clear();
        hunk.ws_block_add.clear();
        let trimmed = trim_hunk_context(&hunk.raw_lines, hunk.combined_parents);
        let added_bytes: usize =
            hunk.header.len() + 1 + trimmed.iter().map(|l| l.len() + 1).sum::<usize>();
        if file.is_lockfile {
            if file.buffered_bytes + added_bytes > LOCKFILE_BUFFER_CAP {
                file.hunks = None;
                return;
            }
            file.buffered_bytes += added_bytes;
        } else if file.is_svg {
            if *compact_bytes + added_bytes > COMPACT_BUFFER_CAP {
                file.svg_overflowed = true;
                file.hunks = None;
                return;
            }
            *compact_bytes += added_bytes;
            file.buffered_bytes += added_bytes;
        } else {
            if *compact_bytes + added_bytes > COMPACT_BUFFER_CAP {
                *compact_overflowed = true;
                file.hunks = None;
                return;
            }
            *compact_bytes += added_bytes;
            file.buffered_bytes += added_bytes;
        }
        hunks.push(CompactHunk {
            header: hunk.header,
            lines: trimmed,
        });
    }

    fn finish_diff_file(&mut self) {
        self.flush();
        let Some(mut file) = self.current_file.take() else {
            return;
        };
        Self::flush_compact_hunk(
            &mut file,
            &mut self.compact_bytes,
            &mut self.compact_overflowed,
        );
        if file.is_svg && !file.generated_folded && file.svg_overflowed {
            self.compact_overflowed = true;
        }
        let pure_rename = if file.saw_sim_100
            && !file.saw_hunk
            && file.added == 0
            && file.deleted == 0
            && !file.saw_non_rename_meta
            && let (Some(old), Some(new)) = (file.rename_from, file.rename_to)
        {
            Some((old, new))
        } else {
            None
        };
        let items_end = self.summary.items.len();
        self.diff_entries.push(DiffEntry::File(DiffFile {
            path: file.path,
            first: file.first,
            last: file.last,
            added: file.added,
            deleted: file.deleted,
            meta_lines: file.meta_lines,
            hunks: if file.generated_folded {
                None
            } else {
                file.hunks
            },
            items_start: file.items_start,
            items_end,
            generated_folded: file.generated_folded,
            pure_rename,
            whitespace_hunk_firsts: file.whitespace_hunk_firsts,
        }));
    }

    /// Read one line of `git show` or `git log -p` output. Each commit header, from
    /// `commit SHA` through its message, is an item before the commit's file and
    /// hunk items. Supports optional `--stat` blocks before the patch and merge
    /// commits (`diff --cc` / `diff --combined`). A line out of place, such as a
    /// `--raw` tree diff or a custom `--format`, returns false.
    fn commit(&mut self, line: &str, n: usize) -> bool {
        const FIELDS: [&str; 6] = [
            "Merge: ",
            "Author: ",
            "AuthorDate: ",
            "Commit: ",
            "CommitDate: ",
            "Date: ",
        ];
        const HEADERS: [&str; 16] = [
            "index ",
            "mode ",
            "--- ",
            "+++ ",
            "old mode ",
            "new mode ",
            "deleted file mode ",
            "new file mode ",
            "similarity index ",
            "dissimilarity index ",
            "rename from ",
            "rename to ",
            "copy from ",
            "copy to ",
            "Binary files ",
            "GIT binary patch",
        ];
        let place = self.place;
        if let Some(id) = commit_id(line)
            && place != Place::Fields
        {
            self.finish_diff_file();
            self.file.clear();
            let item_index = self.summary.items.len();
            self.diff_entries.push(DiffEntry::Commit { item_index });
            let name = format!("commit {}", short(id, 12));
            self.pending = Some(Item::new(&name, NO_MESSAGE, n));
            self.place = Place::Fields;
        } else if line.is_empty() {
            // A blank line ends the fields, a `--stat` block, or the patch before
            // the next commit. It belongs to no item.
            self.place = match place {
                Place::Fields | Place::Message => Place::Message,
                Place::Stat | Place::AfterStat => Place::AfterStat,
                _ => Place::Between,
            };
            return true;
        } else if diff_header_path(line).is_some() && place != Place::Between {
            self.patched = true;
            self.place = Place::File;
        } else if is_hunk_header(line) && matches!(place, Place::File | Place::Hunk) {
            self.place = Place::Hunk;
        } else if place == Place::Message && line == "---" {
            self.flush();
            self.place = Place::Stat;
            return true;
        } else if matches!(place, Place::Message | Place::Stat)
            && (is_stat_line(line) || is_stat_summary(line) || is_stat_mode_line(line))
        {
            if place == Place::Message {
                // Keep the commit header's `[stdout:first-last]` scoped to its
                // message, excluding the `--stat` block.
                self.flush();
            }
            self.place = Place::Stat;
            return true;
        } else {
            let fits = match place {
                Place::Between | Place::AfterStat | Place::Stat => false,
                Place::Fields => FIELDS.iter().any(|field| line.starts_with(field)),
                Place::Message => line.starts_with("    "),
                Place::File => HEADERS.iter().any(|header| line.starts_with(header)),
                Place::Hunk => line.starts_with([' ', '+', '-', '\\']),
            };
            if !fits {
                return false;
            }
            if place == Place::Message
                && let Some(item) = &mut self.pending
                && item.detail == NO_MESSAGE
                && !line.trim().is_empty()
            {
                item.detail = short(line.trim(), 240);
            }
        }
        self.diff(line, n);
        true
    }

    fn diff(&mut self, line: &str, n: usize) {
        self.count_hunk(line);
        if let Some(path) = diff_header_path(line) {
            self.finish_diff_file();
            self.file = path.to_owned();
            let items_start = self.summary.items.len();
            let is_generated = crate::lockdiff::is_generated_path(&self.file);
            let is_svg = !is_generated && crate::lockdiff::is_svg_path(&self.file);
            let is_lockfile = crate::lockdiff::is_lockfile_path(&self.file);
            let indent_significant = is_indent_significant_path(&self.file);
            let mut item = Item::new(&self.file, "file metadata", n);
            item.file_first = n;
            self.pending = Some(item);
            self.current_file = Some(ActiveDiffFile {
                path: self.file.clone(),
                first: n,
                last: n,
                added: 0,
                deleted: 0,
                meta_lines: Vec::new(),
                saw_mode_header: false,
                saw_sim_100: false,
                rename_from: None,
                rename_to: None,
                saw_non_rename_meta: false,
                saw_hunk: false,
                hunks: if !is_lockfile && self.compact_overflowed {
                    None
                } else {
                    Some(Vec::new())
                },
                current_hunk: None,
                buffered_bytes: 0,
                items_start,
                is_generated,
                is_svg,
                is_lockfile,
                generated_folded: false,
                svg_overflowed: false,
                indent_significant,
                whitespace_hunk_firsts: Vec::new(),
            });
        } else if is_hunk_header(line) && !self.file.is_empty() {
            if let Some(file) = self.current_file.as_mut() {
                Self::flush_compact_hunk(
                    file,
                    &mut self.compact_bytes,
                    &mut self.compact_overflowed,
                );
                file.last = n;
                file.saw_hunk = true;
                if !file.generated_folded {
                    let combined = combined_hunk_parents(line).unwrap_or(0);
                    file.current_hunk = Some(ActiveCompactHunk {
                        header: line.to_owned(),
                        first: n,
                        last: n,
                        combined_parents: combined,
                        raw_lines: Vec::new(),
                        raw_bytes: 0,
                        raw_overflowed: file.hunks.is_none(),
                        cut_off: false,
                        ws_eligible: combined == 0,
                        ws_block_bytes: 0,
                        ws_block_rem: Vec::new(),
                        ws_block_add: Vec::new(),
                        ws_added: 0,
                        ws_deleted: 0,
                    });
                }
            }
            // Ordinary index/---/+++ headers contain blob hashes for the entire file.
            // Avoid invalidating independent hunks when just one of them changes.
            if self
                .pending
                .as_ref()
                .is_some_and(|i| i.detail == "file metadata")
            {
                self.pending = None;
                self.hash = Sha256::new();
            }
            self.flush();
            let mut item = Item::new(&self.file, line, n);
            if let Some(file) = self.current_file.as_ref() {
                item.file_first = file.first;
            }
            self.pending = Some(item);
        } else {
            if (line.starts_with("old mode ")
                || line.starts_with("rename from ")
                || line.starts_with("new file mode ")
                || line.starts_with("deleted file mode ")
                || line.starts_with("copy from "))
                && let Some(item) = &mut self.pending
            {
                item.detail = short(line, 360);
            }
            if let Some(file) = self.current_file.as_mut() {
                file.last = n;
                let in_hunk = self
                    .pending
                    .as_ref()
                    .is_some_and(|i| is_hunk_header(&i.detail));
                if in_hunk {
                    let (add, del) = hunk_line_delta(line, self.combined_parents);
                    if add {
                        file.added += 1;
                        if let Some(item) = &mut self.pending {
                            item.added += 1;
                        }
                    } else if del {
                        file.deleted += 1;
                        if let Some(item) = &mut self.pending {
                            item.deleted += 1;
                        }
                    }
                }
                let should_fold_generated = !file.generated_folded
                    && ((file.is_generated
                        && file.last.saturating_sub(file.first) + 1
                            > crate::lockdiff::GENERATED_FOLD_LINES)
                        || (file.is_svg
                            && file.added + file.deleted
                                > crate::lockdiff::SVG_FOLD_CHANGED_LINES));
                if should_fold_generated {
                    file.generated_folded = true;
                    file.current_hunk = None;
                    if !file.is_lockfile {
                        self.compact_bytes = self.compact_bytes.saturating_sub(file.buffered_bytes);
                    }
                    file.buffered_bytes = 0;
                    file.hunks = None;
                }
                if in_hunk {
                    if !file.generated_folded
                        && let Some(active_hunk) = file.current_hunk.as_mut()
                    {
                        active_hunk.last = n;
                        if active_hunk.ws_eligible {
                            if let Some(rest) = line.strip_prefix('-') {
                                active_hunk.ws_deleted += 1;
                                if active_hunk.ws_block_bytes + rest.len() <= WS_BLOCK_BUFFER_CAP {
                                    active_hunk.ws_block_bytes += rest.len() + 1;
                                    active_hunk.ws_block_rem.push(rest.to_owned());
                                } else {
                                    active_hunk.ws_eligible = false;
                                    active_hunk.ws_block_rem.clear();
                                    active_hunk.ws_block_add.clear();
                                }
                            } else if let Some(rest) = line.strip_prefix('+') {
                                active_hunk.ws_added += 1;
                                if active_hunk.ws_block_bytes + rest.len() <= WS_BLOCK_BUFFER_CAP {
                                    active_hunk.ws_block_bytes += rest.len() + 1;
                                    active_hunk.ws_block_add.push(rest.to_owned());
                                } else {
                                    active_hunk.ws_eligible = false;
                                    active_hunk.ws_block_rem.clear();
                                    active_hunk.ws_block_add.clear();
                                }
                            } else if line.starts_with(' ') {
                                if !active_hunk.ws_block_rem.is_empty()
                                    || !active_hunk.ws_block_add.is_empty()
                                {
                                    if !is_ws_only_block(
                                        &active_hunk.ws_block_rem,
                                        &active_hunk.ws_block_add,
                                        file.indent_significant,
                                    ) {
                                        active_hunk.ws_eligible = false;
                                    }
                                    active_hunk.ws_block_rem.clear();
                                    active_hunk.ws_block_add.clear();
                                    active_hunk.ws_block_bytes = 0;
                                }
                            } else {
                                // `\` ("\ No newline at end of file") or unexpected line is semantic.
                                active_hunk.ws_eligible = false;
                                active_hunk.ws_block_rem.clear();
                                active_hunk.ws_block_add.clear();
                            }
                        }
                        if !active_hunk.raw_overflowed {
                            let cap = if file.is_lockfile {
                                LOCKFILE_BUFFER_CAP
                            } else {
                                COMPACT_BUFFER_CAP
                            };
                            if active_hunk.raw_bytes + line.len() < cap * 2 {
                                active_hunk.raw_bytes += line.len() + 1;
                                active_hunk.raw_lines.push(line.to_owned());
                            } else {
                                active_hunk.raw_overflowed = true;
                                active_hunk.raw_lines.clear();
                            }
                        }
                    }
                } else {
                    if line == "similarity index 100%" {
                        file.saw_sim_100 = true;
                    } else if let Some(old) = line.strip_prefix("rename from ") {
                        file.rename_from = Some(old.to_owned());
                    } else if let Some(new) = line.strip_prefix("rename to ") {
                        file.rename_to = Some(new.to_owned());
                    } else if line.starts_with("old mode ")
                        || line.starts_with("new mode ")
                        || line.starts_with("deleted file mode ")
                        || line.starts_with("new file mode ")
                        || line.starts_with("mode ")
                        || line.starts_with("Binary files ")
                        || line.starts_with("GIT binary patch")
                        || line.starts_with("copy from ")
                        || line.starts_with("copy to ")
                    {
                        file.saw_non_rename_meta = true;
                    }
                    if is_preserved_meta_header(line, file.saw_mode_header) {
                        if line.starts_with("new file mode ")
                            || line.starts_with("deleted file mode ")
                        {
                            file.saw_mode_header = true;
                        }
                        if file.meta_lines.len() < 16 {
                            file.meta_lines.push(line.to_owned());
                        }
                    }
                }
            }
        }
        if let Some(item) = &mut self.pending {
            // A hunk is compared by its lines, not by where it starts, so hunks
            // keep their identity when an earlier change shifts them.
            if !is_hunk_header(line) {
                self.hash.update(line.as_bytes());
                self.hash.update(b"\n");
            }
            item.last = n;
        }
    }

    fn test(&mut self, raw: &str, n: usize) {
        let indented = raw.starts_with([' ', '\t']);
        let line = raw.trim();
        if self.go_tests {
            if line.starts_with("ok  \t") || line.starts_with("ok\t") {
                self.go_active = None;
                self.go_after_fail = None;
                if self.go_ok_first == 0 {
                    self.go_ok_first = n;
                }
                self.go_ok_last = n;
                if line.contains("[no tests to run]") {
                    self.go_no_tests += 1;
                } else {
                    self.go_ok += 1;
                }
                if self.go_ok_samples.len() < 4 {
                    self.go_ok_samples.push(short(line, 220));
                }
                return;
            }
            if line.starts_with("?\t") && line.contains("[no test files]") {
                self.go_active = None;
                self.go_after_fail = None;
                if self.go_ok_first == 0 {
                    self.go_ok_first = n;
                }
                self.go_ok_last = n;
                self.go_no_tests += 1;
                if self.go_ok_samples.len() < 4 {
                    self.go_ok_samples.push(short(line, 220));
                }
                return;
            }
            if line.starts_with("FAIL\t") {
                self.go_active = None;
                self.go_after_fail = None;
                self.summary.outcomes.push(short(line, 220));
                return;
            }
            if let Some(rest) = line
                .strip_prefix("=== RUN")
                .or_else(|| line.strip_prefix("=== CONT"))
                .map(str::trim_start)
                && !rest.is_empty()
            {
                self.go_after_fail = None;
                let name = rest.split_whitespace().next().unwrap_or(rest);
                self.go_active = Some((short(name, 240), Vec::new()));
                return;
            }
            if line.starts_with("=== PAUSE")
                || line.starts_with("--- PASS:")
                || line.starts_with("--- SKIP:")
                || line == "PASS"
                || line == "FAIL"
            {
                self.go_active = None;
                self.go_after_fail = None;
                return;
            }
            if indented && !line.is_empty() && !line.starts_with("--- FAIL:") {
                if let Some((name, left)) = self.go_after_fail.as_mut() {
                    if let Some(item) = self.failures.get_mut(name.as_str()) {
                        let snippet = short(line, 200);
                        item.digest = crate::capture::source_digest(
                            format!("{}{}", item.digest, snippet).as_bytes(),
                        );
                        item.detail = if item.detail.is_empty() {
                            snippet
                        } else {
                            short(&format!("{} {}", item.detail, snippet), 440)
                        };
                        item.last = n;
                    }
                    *left -= 1;
                    if *left == 0 {
                        self.go_after_fail = None;
                    }
                    return;
                }
                if let Some((_, lines)) = self.go_active.as_mut() {
                    if lines.len() < 3 {
                        lines.push((n, short(line, 200)));
                    }
                    return;
                }
            } else if !line.starts_with("--- FAIL:") {
                self.go_after_fail = None;
            }
        }
        let failed = if let Some(name) = line
            .strip_prefix("test ")
            .and_then(|s| s.strip_suffix(" ... FAILED"))
        {
            Some((name, ""))
        } else if let Some(rest) = line
            .strip_prefix("FAIL [")
            .or_else(|| line.strip_prefix("TIMEOUT ["))
            && let Some((_, tail)) = rest.split_once("] ")
        {
            let name = tail.trim_start();
            // Nextest retry lines prefix the test name with `(1/2) `.
            let name = name
                .strip_prefix('(')
                .and_then(|s| s.split_once(") "))
                .filter(|(tries, _)| {
                    tries.split_once('/').is_some_and(|(a, b)| {
                        !a.is_empty()
                            && !b.is_empty()
                            && a.bytes().all(|c| c.is_ascii_digit())
                            && b.bytes().all(|c| c.is_ascii_digit())
                    })
                })
                .map_or(name, |(_, rest)| rest.trim_start());
            (!name.is_empty()).then_some((name, ""))
        } else if self.cargo.is_none()
            && !self.go_tests
            && let Some(entry) = line
                .strip_prefix("FAILED ")
                .or_else(|| line.strip_prefix("ERROR "))
        {
            Some(entry.split_once(" - ").unwrap_or((entry, "")))
        } else if self.cargo.is_none()
            && !self.go_tests
            && let Some(nodeid) = pytest_verbose_failure(line)
        {
            Some((nodeid, ""))
        } else {
            line.strip_prefix("--- FAIL: ")
                .map(|entry| (entry.split_whitespace().next().unwrap_or(entry), ""))
        };
        if let Some((name, detail)) = failed {
            let item = self
                .failures
                .entry(short(name, 240))
                .or_insert_with(|| Item::new(name, "", n));
            if !detail.is_empty() {
                item.digest = crate::capture::source_digest(detail.as_bytes());
                item.detail = short(detail, 320);
                item.last = n;
            }
        }
        if self.go_tests
            && let Some(entry) = line.strip_prefix("--- FAIL: ")
        {
            let name = short(entry.split_whitespace().next().unwrap_or(entry), 240);
            if let Some((active, lines)) = self.go_active.take()
                && active == name
                && !lines.is_empty()
                && let Some(item) = self.failures.get_mut(&name)
            {
                item.first = lines[0].0;
                item.last = n;
                let joined = lines
                    .iter()
                    .map(|(_, text)| text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                item.digest = crate::capture::source_digest(joined.as_bytes());
                item.detail = short(&joined, 440);
            } else if self
                .failures
                .get(&name)
                .is_some_and(|item| item.detail.is_empty())
            {
                self.go_after_fail = Some((name, 3));
            }
        }
        if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|s| s.strip_suffix(" stdout ----"))
            .or_else(|| {
                line.strip_prefix("--- STDOUT:")
                    .or_else(|| line.strip_prefix("--- STDERR:"))
                    .and_then(|s| s.strip_suffix(" ---"))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
        {
            self.current_test = short(name, 240);
        }
        // Since Rust 1.73 the panic message follows the location on its own lines,
        // and it is what tells failures at the same location apart.
        let mut message = false;
        if self.panic_lines > 0 {
            if line.is_empty()
                || line.starts_with("note:")
                || line.starts_with("stack backtrace:")
                || line.starts_with("---- ")
                || line.starts_with("--- ")
            {
                self.panic_lines = 0;
            } else if let Some(item) = self.failures.get_mut(&self.current_test) {
                item.digest =
                    crate::capture::source_digest(format!("{}{}", item.digest, line).as_bytes());
                item.detail = short(&format!("{} {}", item.detail, short(line, 200)), 440);
                item.last = n;
                self.panic_lines -= 1;
                message = true;
            }
        }
        if let Some((_, location)) = line.split_once(" panicked at ")
            && let Some(item) = self.failures.get_mut(&self.current_test)
        {
            item.first = n;
            item.last = n;
            item.digest = crate::capture::source_digest(location.as_bytes());
            item.detail = short(location, 200);
            self.panic_lines = if location.ends_with(':') { 3 } else { 0 };
        }
        if !message
            && (line.starts_with("assertion ")
                || line.starts_with("left:")
                || line.starts_with("right:"))
            && let Some(item) = self.failures.get_mut(&self.current_test)
            && n.saturating_sub(item.last) <= 2
        {
            item.digest =
                crate::capture::source_digest(format!("{}{}", item.digest, line).as_bytes());
            item.detail = short(&format!("{} {}", item.detail, line), 440);
            item.last = n;
        }
        if line.starts_with("test result:")
            || line.starts_with("Tests:")
            || line.starts_with("Test Suites:")
            || line.starts_with("ok  \t")
            || (line.starts_with("Summary [")
                && (line.contains(" tests run:") || line.contains(" test run:")))
            || ((line.contains(" passed")
                || line.contains(" failed")
                || line.contains(" error")
                || line.contains(" skipped")
                || line.contains(" xfailed"))
                && line.contains(" in ")
                && line
                    .trim_start_matches(['=', ' '])
                    .starts_with(|c: char| c.is_ascii_digit()))
        {
            self.summary
                .outcomes
                .push(short(line.trim_matches(['=', ' ']), 220));
        }
    }

    pub fn finish(mut self) -> Summary {
        // Commit headers with no patch, as a plain log or `git show -s` prints,
        // keep the ordinary preview.
        if self.discarded || (self.commits && !self.patched) {
            return Summary::default();
        }
        if let Some(inventory) = self.inventory {
            return inventory.finish();
        }
        if let Some(js) = self.js_tests {
            let Some(report) = js.finish() else {
                return Summary::default();
            };
            self.summary.kind = "tests".into();
            self.summary.items = report.summary_items();
            self.summary.outcomes = report.totals.clone();
            self.summary.js = Some(report);
            return self.summary;
        }
        if let Some(rspec) = self.rspec {
            let (items, outcomes) = rspec.finish();
            self.summary.items = items;
            self.summary.outcomes = outcomes;
            return self.summary;
        }
        if let Some(phpunit) = self.phpunit {
            let (items, outcomes) = phpunit.finish();
            self.summary.items = items;
            self.summary.outcomes = outcomes;
            return self.summary;
        }
        if let Some(plan) = self.plan {
            let (items, outcomes, details) = plan.finish();
            self.summary.items = items;
            self.summary.outcomes = outcomes;
            self.summary.plan = details;
            return self.summary;
        }
        // A hunk that a filter such as `grep` thinned out tells nothing about where
        // the output stopped.
        if self.ends_early
            && self.whole_hunks
            && self.hunk.is_some_and(|counts| counts != (0, 0))
            && let Some(item) = &mut self.pending
        {
            item.detail.push_str(" (cut off)");
            if let Some(hunk) = self
                .current_file
                .as_mut()
                .and_then(|file| file.current_hunk.as_mut())
            {
                hunk.header.push_str(" (cut off)");
                hunk.cut_off = true;
            }
        }
        self.finish_diff_file();
        self.summary.diff_raw_bytes = self.diff_raw_bytes;
        self.summary.compact_overflowed = self.compact_overflowed;
        self.summary.diff_entries = self.diff_entries;
        self.summary.items.extend(self.failures.into_values());
        let go_passed = self.go_ok + self.go_no_tests;
        if go_passed > 4 {
            let mut note = format!("go test: {} passed package(s)", self.go_ok);
            if self.go_no_tests > 0 {
                note.push_str(&format!(", {} without tests", self.go_no_tests));
            }
            note.push_str(&format!(
                " [stdout:{}-{}]",
                self.go_ok_first, self.go_ok_last
            ));
            self.summary.outcomes.push(note);
        } else {
            self.summary.outcomes.extend(self.go_ok_samples);
        }
        self.summary.cargo = self.cargo.and_then(crate::cargo_tests::Scan::finish);
        self.summary
    }
}

// ---------------------------------------------------------------------------
// RSpec and PHPUnit / ParaTest / Pest structured test parsers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum RspecSection {
    #[default]
    None,
    Failures,
    FailedExamples,
    LoadError,
}

#[derive(Default)]
struct RspecScan {
    section: RspecSection,
    failures: Vec<Item>,
    by_desc: BTreeMap<String, VecDeque<usize>>,
    current: Option<usize>,
    capture_after_error: usize,
    detail_lines: usize,
    outcomes: Vec<String>,
}

impl RspecScan {
    fn observe(&mut self, raw: &str, n: usize) {
        let cleaned = strip_ansi_cow(raw);
        let line = cleaned.as_ref();
        let trimmed = line.trim();

        if trimmed == "Failures:" {
            self.section = RspecSection::Failures;
            self.current = None;
            return;
        }
        if trimmed == "Failed examples:" {
            self.section = RspecSection::FailedExamples;
            self.current = None;
            return;
        }
        if trimmed == "Pending:"
            || trimmed.starts_with("Pending: (Failures listed here")
            || trimmed.starts_with("Finished in ")
        {
            self.section = RspecSection::None;
            self.current = None;
            return;
        }
        if trimmed.starts_with("An error occurred while loading ") {
            let name = trimmed.trim_end_matches('.');
            let idx = self.failures.len();
            self.failures.push(Item::new(name, "", n));
            self.section = RspecSection::LoadError;
            self.current = Some(idx);
            self.detail_lines = 0;
            return;
        }
        if is_rspec_totals(trimmed) {
            self.section = RspecSection::None;
            self.current = None;
            self.outcomes.push(short(trimmed, 220));
            return;
        }

        match self.section {
            RspecSection::None => {}
            RspecSection::Failures => {
                if let Some(desc) = parse_numbered_heading(trimmed) {
                    let idx = self.failures.len();
                    self.failures.push(Item::new(desc, "", n));
                    self.by_desc
                        .entry(desc.to_owned())
                        .or_default()
                        .push_back(idx);
                    self.current = Some(idx);
                    self.capture_after_error = 0;
                    return;
                }
                if let Some(idx) = self.current {
                    let item = &mut self.failures[idx];
                    if trimmed.is_empty() {
                        return;
                    }
                    item.last = n;
                    if trimmed.starts_with("Failure/Error:") {
                        if item.detail.is_empty() {
                            item.detail = short(trimmed, 360);
                        } else {
                            item.detail = short(&format!("{} | {trimmed}", item.detail), 360);
                        }
                        item.digest = crate::capture::source_digest(item.detail.as_bytes());
                        self.capture_after_error = 3;
                    } else if trimmed.starts_with("# ") || trimmed.starts_with("Diff:") {
                        self.capture_after_error = 0;
                    } else if self.capture_after_error > 0
                        && !trimmed.starts_with("(compared using")
                        && !trimmed.starts_with("Shared Example Group:")
                    {
                        let snippet = short(trimmed, 180);
                        item.detail = if item.detail.is_empty() {
                            snippet
                        } else {
                            short(&format!("{} {snippet}", item.detail), 360)
                        };
                        item.digest = crate::capture::source_digest(item.detail.as_bytes());
                        self.capture_after_error -= 1;
                    }
                }
            }
            RspecSection::FailedExamples => {
                if let Some(rest) = trimmed.strip_prefix("rspec ")
                    && let Some((loc, desc)) = rest.split_once(" # ")
                {
                    let loc = loc.trim();
                    let desc = desc.trim();
                    if let Some(queue) = self.by_desc.get_mut(desc)
                        && let Some(idx) = queue.pop_front()
                    {
                        self.failures[idx].name = short(&format!("{loc} {desc}"), 320);
                    } else {
                        self.failures
                            .push(Item::new(&format!("{loc} {desc}"), "", n));
                    }
                }
            }
            RspecSection::LoadError => {
                if let Some(idx) = self.current {
                    let item = &mut self.failures[idx];
                    if trimmed.is_empty() {
                        return;
                    }
                    item.last = n;
                    if trimmed.starts_with("# ") {
                        self.detail_lines = 3;
                    } else if self.detail_lines < 3 {
                        let snippet = short(trimmed, 180);
                        item.detail = if item.detail.is_empty() {
                            snippet
                        } else {
                            short(&format!("{} {snippet}", item.detail), 360)
                        };
                        item.digest = crate::capture::source_digest(item.detail.as_bytes());
                        self.detail_lines += 1;
                    }
                }
            }
        }
    }

    fn finish(self) -> (Vec<Item>, Vec<String>) {
        (self.failures, self.outcomes)
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum PhpunitSection {
    #[default]
    None,
    Failure,
    PestFail,
}

#[derive(Default)]
struct PhpunitScan {
    section: PhpunitSection,
    current: Option<Item>,
    detail_lines: usize,
    saw_blank_after_detail: bool,
    failures: Vec<Item>,
    outcomes: Vec<String>,
    pending_banner: Option<String>,
}

impl PhpunitScan {
    fn flush(&mut self) {
        if let Some(item) = self.current.take() {
            self.failures.push(item);
        }
        self.detail_lines = 0;
        self.saw_blank_after_detail = false;
    }

    fn observe(&mut self, raw: &str, n: usize) {
        let cleaned = strip_ansi_cow(raw);
        let line = cleaned.as_ref();
        let trimmed = line.trim();

        if is_phpunit_failure_section(trimmed) {
            self.flush();
            self.section = PhpunitSection::Failure;
            return;
        }
        if is_phpunit_other_section(trimmed) {
            self.flush();
            self.section = PhpunitSection::None;
            return;
        }
        if trimmed == "--" || trimmed.starts_with("───") {
            self.flush();
            self.section = PhpunitSection::None;
            return;
        }
        if trimmed == "FAILURES!" || trimmed == "ERRORS!" {
            self.flush();
            self.section = PhpunitSection::None;
            self.pending_banner = Some(trimmed.to_owned());
            return;
        }
        if trimmed.starts_with("OK (") || trimmed == "No tests executed!" {
            self.flush();
            self.section = PhpunitSection::None;
            self.outcomes.push(short(trimmed, 220));
            return;
        }
        if trimmed.starts_with("Tests:") {
            self.flush();
            self.section = PhpunitSection::None;
            let norm = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
            let line = match self.pending_banner.take() {
                Some(banner) => format!("{banner} {norm}"),
                None => norm,
            };
            self.outcomes.push(short(&line, 220));
            return;
        }
        if trimmed.starts_with("Duration:") {
            self.flush();
            self.section = PhpunitSection::None;
            return;
        }
        if let Some(rest) = trimmed.strip_prefix("FAILED  ").or_else(|| {
            trimmed
                .strip_prefix("FAILED ")
                .filter(|r| r.contains(" > "))
        }) {
            let name = rest.trim();
            if !name.is_empty() {
                self.flush();
                self.current = Some(Item::new(name, "", n));
                self.section = PhpunitSection::PestFail;
                return;
            }
        }

        match self.section {
            PhpunitSection::None => {}
            PhpunitSection::Failure => {
                if !line.starts_with([' ', '\t'])
                    && let Some(name) = parse_numbered_heading(trimmed)
                {
                    self.flush();
                    self.current = Some(Item::new(name, "", n));
                    return;
                }
                if let Some(item) = self.current.as_mut() {
                    if trimmed.is_empty() {
                        if self.detail_lines > 0 {
                            self.saw_blank_after_detail = true;
                        }
                        return;
                    }
                    item.last = n;
                    if !self.saw_blank_after_detail && self.detail_lines < 3 {
                        let snippet = short(trimmed, 180);
                        item.detail = if item.detail.is_empty() {
                            snippet
                        } else {
                            short(&format!("{} {snippet}", item.detail), 360)
                        };
                        item.digest = crate::capture::source_digest(item.detail.as_bytes());
                        self.detail_lines += 1;
                    }
                }
            }
            PhpunitSection::PestFail => {
                if let Some(item) = self.current.as_mut() {
                    if trimmed.is_empty() {
                        if self.detail_lines > 0 {
                            self.saw_blank_after_detail = true;
                        }
                        return;
                    }
                    item.last = n;
                    if !self.saw_blank_after_detail
                        && self.detail_lines < 3
                        && !trimmed.starts_with("at ")
                        && !trimmed.contains('▕')
                    {
                        let snippet = short(trimmed, 180);
                        item.detail = if item.detail.is_empty() {
                            snippet
                        } else {
                            short(&format!("{} {snippet}", item.detail), 360)
                        };
                        item.digest = crate::capture::source_digest(item.detail.as_bytes());
                        self.detail_lines += 1;
                    }
                }
            }
        }
    }

    fn finish(mut self) -> (Vec<Item>, Vec<String>) {
        self.flush();
        if let Some(banner) = self.pending_banner.take() {
            self.outcomes.push(banner);
        }
        (self.failures, self.outcomes)
    }
}

fn strip_ansi_cow(line: &str) -> Cow<'_, str> {
    if line.contains('\x1b') {
        crate::capture::ANSI.replace_all(line, "")
    } else {
        Cow::Borrowed(line)
    }
}

/// Parse a top-level numbered failure heading such as `1) Foo#bar` (requiring
/// pure digits before `)` so RSpec aggregate sub-items like `1.1)` stay attached
/// to their parent failure).
fn parse_numbered_heading(trimmed: &str) -> Option<&str> {
    let (num, rest) = trimmed.split_once(") ")?;
    if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
        let desc = rest.trim();
        (!desc.is_empty()).then_some(desc)
    } else {
        None
    }
}

fn is_rspec_totals(trimmed: &str) -> bool {
    static RSPEC_TOTALS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\d+ examples?, \d+ failures?(?:, \d+ pending)?(?:, \d+ errors? occurred outside of examples)?$")
            .unwrap()
    });
    RSPEC_TOTALS.is_match(trimmed)
}

fn is_phpunit_failure_section(trimmed: &str) -> bool {
    static PHPUNIT_FAIL_SEC: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^There (?:was 1|were \d+) (?:error|errors|failure|failures):$").unwrap()
    });
    PHPUNIT_FAIL_SEC.is_match(trimmed)
}

fn is_phpunit_other_section(trimmed: &str) -> bool {
    trimmed.starts_with("There was ") || trimmed.starts_with("There were ")
}

/// Parse a Terraform/OpenTofu resource plan header such as
/// `aws_instance.web[0] will be created` or `module.net.aws_subnet.a must be replaced`.
fn terraform_action(rest: &str) -> Option<(&'static str, &str, &str)> {
    const ACTIONS: &[(&str, &str)] = &[
        (" must be replaced", "-/+"),
        (" is tainted, so it must be replaced", "-/+"),
        (" will be created", "+"),
        (" will be updated in-place", "~"),
        (" will be destroyed", "-"),
        (" will be read during apply", "<="),
        (" has moved to ", "->"),
        (" will be forgotten", "x"),
        (" will no longer be managed", "x"),
    ];
    for &(phrase, glyph) in ACTIONS {
        if let Some(pos) = rest.find(phrase) {
            let address = rest[..pos].trim();
            let action = rest[pos..].trim();
            if !address.is_empty() && !address.contains(' ') {
                return Some((glyph, address, action));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Terraform / OpenTofu and Pulumi structured plan parser
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlanEngine {
    Terraform,
    Pulumi,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PulumiSection {
    Header,
    Tree,
    Diff,
    Outputs,
    Diagnostics,
    Resources,
}

struct ActiveTfResource {
    glyph: &'static str,
    address: String,
    action: String,
    first: usize,
    last: usize,
    depth: usize,
    block_stack: Vec<(usize, String)>,
    changed_attrs: Vec<String>,
    changed_extra: usize,
    forces_attrs: Vec<String>,
    forces_extra: usize,
    is_update: bool,
    is_replace: bool,
}

struct ActiveTfDiag {
    first: usize,
    last: usize,
    headline: String,
    location: Vec<String>,
    detail: Option<String>,
}

#[derive(Default)]
struct TfDiagState {
    pending_box_start: Option<usize>,
    active: Option<ActiveTfDiag>,
}

impl TfDiagState {
    fn flush(&mut self, stream: &str, out: &mut Vec<String>) {
        let Some(diag) = self.active.take() else {
            return;
        };
        if out.len() >= LIMIT {
            return;
        }
        let range = format_line_range(diag.first, diag.last);
        let line = if !diag.location.is_empty() {
            format!(
                "{} ({}) [{stream}:{range}]",
                diag.headline,
                diag.location.join(" ")
            )
        } else if let Some(detail) = diag.detail {
            format!("{} — {detail} [{stream}:{range}]", diag.headline)
        } else {
            format!("{} [{stream}:{range}]", diag.headline)
        };
        out.push(line);
    }

    fn starts_diag(trimmed: &str) -> bool {
        if trimmed == "╷" || trimmed.starts_with("Warning:") || trimmed.starts_with("Error:") {
            return true;
        }
        if let Some(inner) = trimmed.strip_prefix('│').map(str::trim) {
            return inner.starts_with("Warning:") || inner.starts_with("Error:");
        }
        false
    }

    fn observe(&mut self, raw: &str, n: usize, stream: &str, out: &mut Vec<String>) -> bool {
        let trimmed = raw.trim();
        if trimmed == "╷" {
            self.flush(stream, out);
            self.pending_box_start = Some(n);
            return true;
        }
        if trimmed == "╵" {
            if let Some(diag) = self.active.as_mut() {
                diag.last = n;
            }
            self.flush(stream, out);
            self.pending_box_start = None;
            return true;
        }
        let (is_bar, inner) = match trimmed.strip_prefix('│') {
            Some(rest) => (true, rest.trim()),
            None => (false, trimmed),
        };
        if inner.starts_with("Warning:") || inner.starts_with("Error:") {
            self.flush(stream, out);
            if is_bar {
                let first = self.pending_box_start.take().unwrap_or(n);
                self.active = Some(ActiveTfDiag {
                    first,
                    last: n,
                    headline: short(inner, 240),
                    location: Vec::new(),
                    detail: None,
                });
            } else {
                self.pending_box_start = None;
                if out.len() < LIMIT {
                    out.push(format!("{} [{stream}:{n}]", short(inner, 240)));
                }
            }
            return true;
        }
        if is_bar && let Some(diag) = self.active.as_mut() {
            diag.last = n;
            if inner.starts_with("with ") || inner.starts_with("on ") {
                if diag.location.len() < 2 {
                    diag.location.push(short(inner.trim_end_matches(':'), 120));
                }
            } else if diag.detail.is_none()
                && !inner.is_empty()
                && !inner.chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                diag.detail = Some(short(inner, 120));
            }
            return true;
        }
        self.pending_box_start = None;
        self.flush(stream, out);
        false
    }
}

struct ActivePulumiDiff {
    glyph: &'static str,
    res_type: String,
    res_name: Option<String>,
    action: String,
    first: usize,
    last: usize,
    depth: usize,
    changed_fields: Vec<String>,
    changed_extra: usize,
    track_fields: bool,
}

struct PlanScan {
    engine: PlanEngine,
    items: Vec<Item>,
    outcomes: Vec<String>,
    details: PlanDetails,
    hash: Sha256,
    tf_active: Option<ActiveTfResource>,
    tf_in_outputs: bool,
    tf_stdout_diag: TfDiagState,
    tf_stderr_diag: TfDiagState,
    pulumi_section: PulumiSection,
    pulumi_active: Option<ActivePulumiDiff>,
    pulumi_in_same_block: bool,
    pulumi_resources: Vec<String>,
    pulumi_diag_subject: Option<(String, usize)>,
    pulumi_stderr_diag_subject: Option<(String, usize)>,
    pulumi_in_stderr_diag: bool,
}

impl PlanScan {
    fn terraform() -> Self {
        Self::new(PlanEngine::Terraform)
    }

    fn pulumi() -> Self {
        let mut scan = Self::new(PlanEngine::Pulumi);
        scan.details.pulumi = true;
        scan
    }

    fn new(engine: PlanEngine) -> Self {
        Self {
            engine,
            items: Vec::new(),
            outcomes: Vec::new(),
            details: PlanDetails::default(),
            hash: Sha256::new(),
            tf_active: None,
            tf_in_outputs: false,
            tf_stdout_diag: TfDiagState::default(),
            tf_stderr_diag: TfDiagState::default(),
            pulumi_section: PulumiSection::Header,
            pulumi_active: None,
            pulumi_in_same_block: false,
            pulumi_resources: Vec::new(),
            pulumi_diag_subject: None,
            pulumi_stderr_diag_subject: None,
            pulumi_in_stderr_diag: false,
        }
    }

    fn observe(&mut self, raw: &str, n: usize) {
        let clean = strip_ansi_cow(raw);
        let line = clean.strip_suffix('\r').unwrap_or(&clean);
        match self.engine {
            PlanEngine::Terraform => self.observe_terraform(line, n),
            PlanEngine::Pulumi => self.observe_pulumi(line, n),
        }
    }

    fn observe_stderr(&mut self, raw: &str, n: usize) {
        let clean = strip_ansi_cow(raw);
        let line = clean.strip_suffix('\r').unwrap_or(&clean);
        match self.engine {
            PlanEngine::Terraform => {
                self.tf_stderr_diag
                    .observe(line, n, "stderr", &mut self.details.diagnostics);
            }
            PlanEngine::Pulumi => {
                let trimmed = line.trim();
                if trimmed == "Diagnostics:" {
                    self.pulumi_in_stderr_diag = true;
                    self.pulumi_stderr_diag_subject = None;
                } else if self.pulumi_in_stderr_diag {
                    if trimmed.is_empty() {
                        self.pulumi_stderr_diag_subject = None;
                    } else {
                        observe_pulumi_diag_line(
                            trimmed,
                            n,
                            "stderr",
                            &mut self.pulumi_stderr_diag_subject,
                            &mut self.details.diagnostics,
                        );
                    }
                } else if starts_with_diag_level(trimmed) && self.details.diagnostics.len() < LIMIT
                {
                    self.details
                        .diagnostics
                        .push(format!("{} [stderr:{n}]", short(trimmed, 240)));
                }
            }
        }
    }

    fn flush_tf(&mut self) {
        let Some(res) = self.tf_active.take() else {
            return;
        };
        let detail = if !res.forces_attrs.is_empty() {
            format!(
                "{} (replace: {})",
                res.action,
                format_attr_list(&res.forces_attrs, res.forces_extra)
            )
        } else if res.is_update && !res.changed_attrs.is_empty() {
            format!(
                "{} ({})",
                res.action,
                format_attr_list(&res.changed_attrs, res.changed_extra)
            )
        } else {
            res.action
        };
        let mut item = Item::new(
            &format!("{} {}", res.glyph, res.address),
            &detail,
            res.first,
        );
        item.last = res.last;
        item.digest = format!("{:x}", self.hash.finalize_reset());
        if self.items.len() < LIMIT {
            self.items.push(item);
        }
    }

    fn observe_terraform(&mut self, raw: &str, n: usize) {
        let trimmed = raw.trim();
        if TfDiagState::starts_diag(trimmed) {
            self.flush_tf();
            self.tf_in_outputs = false;
        }
        if self
            .tf_stdout_diag
            .observe(raw, n, "stdout", &mut self.details.diagnostics)
        {
            return;
        }
        if self.tf_active.is_none() && !self.tf_in_outputs && is_tf_refresh_line(trimmed) {
            self.details.refreshed += 1;
            if self.details.refreshed_first == 0 {
                self.details.refreshed_first = n;
            }
            self.details.refreshed_last = n;
            return;
        }
        if let Some(rest) = trimmed.strip_prefix("# ")
            && let Some((glyph, address, action)) = terraform_action(rest)
        {
            self.flush_tf();
            self.tf_in_outputs = false;
            self.hash = Sha256::new();
            self.tf_active = Some(ActiveTfResource {
                glyph,
                address: address.to_owned(),
                action: action.to_owned(),
                first: n,
                last: n,
                depth: 0,
                block_stack: Vec::new(),
                changed_attrs: Vec::new(),
                changed_extra: 0,
                forces_attrs: Vec::new(),
                forces_extra: 0,
                is_update: glyph == "~",
                is_replace: glyph == "-/+",
            });
            return;
        }
        if trimmed.starts_with("Plan: ") || trimmed.starts_with("No changes.") {
            self.flush_tf();
            self.tf_in_outputs = false;
            if self.outcomes.len() < 128 {
                self.outcomes.push(short(trimmed, 220));
            }
            return;
        }
        if trimmed == "Changes to Outputs:" {
            self.flush_tf();
            self.tf_in_outputs = true;
            return;
        }
        if trimmed.starts_with("Note:")
            || trimmed.starts_with("Saved the plan to:")
            || trimmed.starts_with("────")
        {
            self.flush_tf();
            self.tf_in_outputs = false;
            return;
        }
        if self.tf_in_outputs {
            if trimmed.is_empty() {
                return;
            }
            if raw.starts_with([' ', '\t']) {
                if self.details.outputs.len() < LIMIT {
                    self.details
                        .outputs
                        .push(format!("{} [stdout:{n}]", short(trimmed, 240)));
                }
            } else {
                self.tf_in_outputs = false;
            }
            return;
        }
        if let Some(res) = &mut self.tf_active {
            if trimmed.is_empty() {
                return;
            }
            self.hash.update(trimmed.as_bytes());
            self.hash.update(b"\n");
            res.last = n;
            if res.depth == 0 {
                if res.is_replace && trimmed.starts_with("+/- ") {
                    res.glyph = "+/-";
                }
                scan_tf_delimiters(trimmed, &mut res.depth, &mut res.block_stack, None);
                return;
            }
            let (has_change, attr_name) = parse_tf_attr_line(trimmed);
            if res.is_update
                && res.depth == 1
                && has_change
                && let Some(key) = attr_name
            {
                push_unique_capped(&mut res.changed_attrs, &mut res.changed_extra, key, 5);
            }
            if trimmed.contains("# forces replacement") {
                let forced = format_forced_attr(&res.block_stack, attr_name);
                if let Some(key) = forced {
                    push_unique_capped(&mut res.forces_attrs, &mut res.forces_extra, &key, 8);
                }
            }
            scan_tf_delimiters(trimmed, &mut res.depth, &mut res.block_stack, attr_name);
        }
    }

    fn flush_pulumi_active(&mut self) {
        let Some(active) = self.pulumi_active.take() else {
            return;
        };
        let name = match active.res_name {
            Some(rn) => format!("{} {} {rn}", active.glyph, active.res_type),
            None => format!("{} {}", active.glyph, active.res_type),
        };
        let detail = if active.changed_fields.is_empty() {
            active.action
        } else {
            format!(
                "{} ({})",
                active.action,
                format_attr_list(&active.changed_fields, active.changed_extra)
            )
        };
        let mut item = Item::new(&name, &detail, active.first);
        item.last = active.last;
        item.digest = format!("{:x}", self.hash.finalize_reset());
        if self.items.len() < LIMIT {
            self.items.push(item);
        }
    }

    fn flush_pulumi_resources(&mut self) {
        if self.pulumi_resources.is_empty() {
            return;
        }
        let parts = std::mem::take(&mut self.pulumi_resources);
        let mut actions = Vec::new();
        let mut totals = Vec::new();
        for part in parts {
            if part.starts_with(['+', '~', '-']) {
                actions.push(part);
            } else {
                totals.push(part);
            }
        }
        let line = match (actions.is_empty(), totals.is_empty()) {
            (false, false) => format!("Resources: {}; {}", actions.join(", "), totals.join(", ")),
            (false, true) => format!("Resources: {}", actions.join(", ")),
            (true, false) => format!("Resources: {}", totals.join(", ")),
            (true, true) => return,
        };
        if self.outcomes.len() < 128 {
            self.outcomes.push(short(&line, 220));
        }
    }

    fn observe_pulumi(&mut self, raw: &str, n: usize) {
        let trimmed = raw.trim();
        if trimmed.starts_with("Previewing update ") {
            self.flush_pulumi_active();
            self.flush_pulumi_resources();
            self.pulumi_in_same_block = false;
            self.details.header = Some(short(trimmed.trim_end_matches(':'), 220));
            self.pulumi_section = PulumiSection::Header;
            return;
        }
        if trimmed == "Outputs:" || trimmed == "--outputs:--" {
            self.flush_pulumi_active();
            self.flush_pulumi_resources();
            self.pulumi_in_same_block = false;
            self.pulumi_section = PulumiSection::Outputs;
            return;
        }
        if trimmed == "Diagnostics:" {
            self.flush_pulumi_active();
            self.flush_pulumi_resources();
            self.pulumi_in_same_block = false;
            self.pulumi_diag_subject = None;
            self.pulumi_section = PulumiSection::Diagnostics;
            return;
        }
        if trimmed == "Resources:" {
            self.flush_pulumi_active();
            self.flush_pulumi_resources();
            self.pulumi_in_same_block = false;
            self.pulumi_section = PulumiSection::Resources;
            return;
        }
        if trimmed.starts_with("Permalink:") || trimmed.starts_with("View in Browser") {
            return;
        }
        if trimmed.starts_with("Duration:") {
            self.flush_pulumi_active();
            self.flush_pulumi_resources();
            self.pulumi_in_same_block = false;
            self.pulumi_section = PulumiSection::Header;
            return;
        }
        if trimmed.starts_with("Type") && trimmed.contains("Name") && trimmed.contains("Plan") {
            self.flush_pulumi_active();
            self.flush_pulumi_resources();
            self.pulumi_in_same_block = false;
            self.pulumi_section = PulumiSection::Tree;
            return;
        }

        match self.pulumi_section {
            PulumiSection::Resources => {
                if trimmed.is_empty() {
                    self.flush_pulumi_resources();
                    self.pulumi_section = PulumiSection::Header;
                } else {
                    self.pulumi_resources.push(trimmed.to_owned());
                }
            }
            PulumiSection::Outputs => {
                if trimmed.is_empty() {
                    self.pulumi_section = PulumiSection::Header;
                } else if self.details.outputs.len() < LIMIT {
                    self.details
                        .outputs
                        .push(format!("{} [stdout:{n}]", short(trimmed, 240)));
                }
            }
            PulumiSection::Diagnostics => {
                if trimmed.is_empty() {
                    self.pulumi_diag_subject = None;
                } else {
                    observe_pulumi_diag_line(
                        trimmed,
                        n,
                        "stdout",
                        &mut self.pulumi_diag_subject,
                        &mut self.details.diagnostics,
                    );
                }
            }
            PulumiSection::Tree => {
                if trimmed.is_empty() {
                    self.pulumi_section = PulumiSection::Header;
                    return;
                }
                match parse_pulumi_tree_row(raw) {
                    Some(PulumiTreeRow::Unchanged) => {
                        self.details.unchanged += 1;
                        if self.details.unchanged_first == 0 {
                            self.details.unchanged_first = n;
                        }
                        self.details.unchanged_last = n;
                    }
                    Some(PulumiTreeRow::Changed {
                        glyph,
                        res_type,
                        res_name,
                        action,
                        info,
                    }) => {
                        let detail = if info.is_empty() {
                            action.to_owned()
                        } else {
                            format!("{action} {info}")
                        };
                        let mut item =
                            Item::new(&format!("{glyph} {res_type} {res_name}"), &detail, n);
                        item.last = n;
                        let mut h = Sha256::new();
                        h.update(glyph.as_bytes());
                        h.update(b"\0");
                        h.update(res_type.as_bytes());
                        h.update(b"\0");
                        h.update(res_name.as_bytes());
                        h.update(b"\0");
                        h.update(detail.as_bytes());
                        item.digest = format!("{:x}", h.finalize());
                        if self.items.len() < LIMIT {
                            self.items.push(item);
                        }
                    }
                    None => {}
                }
            }
            PulumiSection::Header | PulumiSection::Diff => {
                if let Some(header) = parse_pulumi_diff_header(raw) {
                    self.flush_pulumi_active();
                    self.pulumi_section = PulumiSection::Diff;
                    match header {
                        PulumiDiffHeader::Same => {
                            self.pulumi_in_same_block = true;
                            self.details.unchanged += 1;
                            if self.details.unchanged_first == 0 {
                                self.details.unchanged_first = n;
                            }
                            self.details.unchanged_last = n;
                        }
                        PulumiDiffHeader::Changed {
                            glyph,
                            res_type,
                            action,
                        } => {
                            self.pulumi_in_same_block = false;
                            self.hash = Sha256::new();
                            let track_fields = matches!(glyph, "~" | "+-" | "++" | "--")
                                || matches!(action, "update" | "replace");
                            self.pulumi_active = Some(ActivePulumiDiff {
                                glyph,
                                res_type: res_type.to_owned(),
                                res_name: None,
                                action: action.to_owned(),
                                first: n,
                                last: n,
                                depth: 0,
                                changed_fields: Vec::new(),
                                changed_extra: 0,
                                track_fields,
                            });
                        }
                    }
                    return;
                }
                if self.pulumi_in_same_block {
                    if trimmed.is_empty() {
                        self.pulumi_in_same_block = false;
                    } else {
                        self.details.unchanged_last = n;
                    }
                    return;
                }
                if let Some(active) = &mut self.pulumi_active {
                    if trimmed.is_empty() {
                        return;
                    }
                    self.hash.update(trimmed.as_bytes());
                    self.hash.update(b"\n");
                    active.last = n;
                    if let Some(urn) = trimmed
                        .strip_prefix("[urn=")
                        .and_then(|s| s.strip_suffix(']'))
                    {
                        if let Some(name) = urn.rsplit("::").next()
                            && !name.is_empty()
                        {
                            active.res_name = Some(name.to_owned());
                        }
                        return;
                    }
                    if trimmed.starts_with("[id=") && trimmed.ends_with(']') {
                        return;
                    }
                    if active.track_fields
                        && active.depth == 0
                        && let Some(field) = parse_pulumi_diff_field(trimmed)
                    {
                        push_unique_capped(
                            &mut active.changed_fields,
                            &mut active.changed_extra,
                            field,
                            5,
                        );
                    }
                    update_bracket_depth(trimmed, &mut active.depth);
                    return;
                }
                if starts_with_diag_level(trimmed) && self.details.diagnostics.len() < LIMIT {
                    self.details
                        .diagnostics
                        .push(format!("{} [stdout:{n}]", short(trimmed, 240)));
                }
            }
        }
    }

    fn finish(mut self) -> (Vec<Item>, Vec<String>, Option<PlanDetails>) {
        self.flush_tf();
        self.tf_stdout_diag
            .flush("stdout", &mut self.details.diagnostics);
        self.tf_stderr_diag
            .flush("stderr", &mut self.details.diagnostics);
        self.flush_pulumi_active();
        self.flush_pulumi_resources();
        let details = (!self.details.is_empty()).then_some(self.details);
        (self.items, self.outcomes, details)
    }
}

impl PlanDetails {
    fn is_empty(&self) -> bool {
        self.header.is_none()
            && self.refreshed == 0
            && self.unchanged == 0
            && self.outputs.is_empty()
            && self.diagnostics.is_empty()
    }

    fn render_extras(&self, report: &mut String) {
        if !self.outputs.is_empty() {
            report.push_str(if self.pulumi {
                "Outputs:\n"
            } else {
                "Changes to Outputs:\n"
            });
            for line in &self.outputs {
                report.push_str("  ");
                report.push_str(line);
                report.push('\n');
            }
        }
        if !self.diagnostics.is_empty() {
            if self.pulumi {
                report.push_str("Diagnostics:\n");
                for line in &self.diagnostics {
                    report.push_str("  ");
                    report.push_str(line);
                    report.push('\n');
                }
            } else {
                for line in &self.diagnostics {
                    report.push_str(line);
                    report.push('\n');
                }
            }
        }
    }
}

fn format_line_range(first: usize, last: usize) -> String {
    if first == last || last == 0 {
        first.to_string()
    } else {
        format!("{first}-{last}")
    }
}

fn is_tf_refresh_line(trimmed: &str) -> bool {
    trimmed.contains(": Refreshing state...")
        || trimmed.contains(": Reading...")
        || trimmed.contains(": Still reading...")
        || trimmed.contains(": Read complete after ")
}

fn push_unique_capped(list: &mut Vec<String>, extra: &mut usize, key: &str, max: usize) {
    if list.iter().any(|existing| existing == key) {
        return;
    }
    if list.len() < max {
        list.push(key.to_owned());
    } else {
        *extra += 1;
    }
}

fn format_attr_list(list: &[String], extra: usize) -> String {
    if extra == 0 {
        list.join(", ")
    } else {
        format!("{}, +{extra} more", list.join(", "))
    }
}

fn is_hcl_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

fn parse_tf_attr_line(trimmed: &str) -> (bool, Option<&str>) {
    let (has_change, rest) = if let Some(r) = trimmed
        .strip_prefix("-/+ ")
        .or_else(|| trimmed.strip_prefix("+/- "))
        .or_else(|| trimmed.strip_prefix("<= "))
        .or_else(|| trimmed.strip_prefix("~ "))
        .or_else(|| trimmed.strip_prefix("+ "))
        .or_else(|| trimmed.strip_prefix("- "))
    {
        (true, r.trim_start())
    } else {
        (false, trimmed)
    };
    if rest.starts_with('#') {
        return (has_change, None);
    }
    if let Some((left, _)) = rest.split_once('=') {
        let key = left.trim();
        let unquoted = key
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(key);
        if !unquoted.is_empty()
            && !unquoted.contains(' ')
            && !matches!(unquoted, "resource" | "data" | "output" | "module")
        {
            return (has_change, Some(unquoted));
        }
    }
    let before_comment = rest.split(" # ").next().unwrap_or(rest).trim_end();
    if let Some(before_brace) = before_comment.strip_suffix('{') {
        let key = before_brace.trim();
        if is_hcl_ident(key) && !matches!(key, "resource" | "data" | "output" | "module") {
            return (has_change, Some(key));
        }
    }
    (has_change, None)
}

fn format_forced_attr(stack: &[(usize, String)], attr: Option<&str>) -> Option<String> {
    if stack.is_empty() {
        return attr.map(str::to_owned);
    }
    let prefix: Vec<&str> = stack.iter().map(|(_, name)| name.as_str()).collect();
    match attr {
        Some(key) if stack.last().map(|(_, n)| n.as_str()) != Some(key) => {
            Some(format!("{}.{key}", prefix.join(".")))
        }
        _ => Some(prefix.join(".")),
    }
}

fn scan_tf_delimiters(
    trimmed: &str,
    depth: &mut usize,
    stack: &mut Vec<(usize, String)>,
    attr_name: Option<&str>,
) {
    let mut in_quote = false;
    let mut escaped = false;
    let mut pushed_for_line = false;
    for ch in trimmed.chars() {
        if in_quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_quote = false;
            }
            continue;
        }
        match ch {
            '"' => in_quote = true,
            '#' => break,
            '{' | '[' | '(' => {
                if !pushed_for_line
                    && *depth >= 1
                    && let Some(name) = attr_name
                {
                    stack.push((*depth, name.to_owned()));
                    pushed_for_line = true;
                }
                *depth += 1;
            }
            '}' | ']' | ')' => {
                *depth = depth.saturating_sub(1);
                while stack.last().is_some_and(|(d, _)| *d >= *depth) {
                    stack.pop();
                }
            }
            _ => {}
        }
    }
}

fn update_bracket_depth(trimmed: &str, depth: &mut usize) {
    let mut in_quote = false;
    let mut escaped = false;
    for ch in trimmed.chars() {
        if in_quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_quote = false;
            }
            continue;
        }
        match ch {
            '"' => in_quote = true,
            '{' | '[' => *depth += 1,
            '}' | ']' => *depth = depth.saturating_sub(1),
            _ => {}
        }
    }
}

fn starts_with_diag_level(trimmed: &str) -> bool {
    let lower = trimmed.get(..8).unwrap_or(trimmed).to_ascii_lowercase();
    lower.starts_with("error:") || lower.starts_with("warning:")
}

fn observe_pulumi_diag_line(
    trimmed: &str,
    n: usize,
    stream: &str,
    subject: &mut Option<(String, usize)>,
    diagnostics: &mut Vec<String>,
) {
    if trimmed.ends_with(':') && !starts_with_diag_level(trimmed) && trimmed != "Diagnostics:" {
        *subject = Some((short(trimmed.trim_end_matches(':'), 120), n));
        return;
    }
    if diagnostics.len() >= LIMIT {
        return;
    }
    if starts_with_diag_level(trimmed) {
        let line = if let Some((subj, first)) = subject.as_ref() {
            format!("{subj}: {} [{stream}:{first}-{n}]", short(trimmed, 240))
        } else {
            format!("{} [{stream}:{n}]", short(trimmed, 240))
        };
        diagnostics.push(line);
    } else {
        diagnostics.push(format!("  {} [{stream}:{n}]", short(trimmed, 240)));
    }
}

fn is_pulumi_action(word: &str) -> bool {
    matches!(
        word,
        "create"
            | "update"
            | "delete"
            | "replace"
            | "create-replacement"
            | "delete-original"
            | "delete-replaced"
            | "read"
            | "import"
            | "refresh"
            | "discard"
            | "same"
    )
}

fn pulumi_glyph_for_action(action: &str) -> &'static str {
    match action {
        "create" | "import" => "+",
        "update" | "refresh" => "~",
        "delete" | "discard" => "-",
        "replace" => "+-",
        "create-replacement" => "++",
        "delete-original" | "delete-replaced" => "--",
        "read" => "<=",
        _ => "~",
    }
}

enum PulumiTreeRow<'a> {
    Unchanged,
    Changed {
        glyph: &'static str,
        res_type: &'a str,
        res_name: &'a str,
        action: &'a str,
        info: &'a str,
    },
}

fn parse_pulumi_tree_row(raw: &str) -> Option<PulumiTreeRow<'_>> {
    let leading_spaces = raw.bytes().take_while(|&b| b == b' ').count();
    let trimmed = raw.trim_start();
    let (glyph, after_glyph) = if leading_spaces <= 2 {
        let mut found = None;
        for g in ["+-", "++", "--", "+", "~", "-"] {
            if let Some(rest) = trimmed.strip_prefix(g)
                && rest.starts_with([' ', '\t', '├', '└', '│'])
            {
                found = Some((g, rest));
                break;
            }
        }
        match found {
            Some((g, r)) => (Some(g), r),
            None => (None, trimmed),
        }
    } else {
        (None, trimmed)
    };
    let rest = after_glyph.trim_start_matches([' ', '\t', '├', '└', '─', '│', '|', '`']);
    // Also strip ASCII tree connectors like `|-` or `+-` when indented past column 2.
    let rest = if glyph.is_none() {
        rest.strip_prefix("|-")
            .or_else(|| rest.strip_prefix("+-"))
            .or_else(|| rest.strip_prefix("`-"))
            .map(str::trim_start)
            .unwrap_or(rest)
    } else {
        rest
    };
    let mut parts = rest.split_whitespace();
    let res_type = parts.next()?;
    if !res_type.contains(':') {
        return None;
    }
    let res_name = parts.next()?;
    let after_name = rest
        .strip_prefix(res_type)?
        .trim_start()
        .strip_prefix(res_name)?
        .trim();
    let (plan_action, info) = if after_name.is_empty() {
        (None, "")
    } else {
        let first = after_name.split_whitespace().next().unwrap_or("");
        if is_pulumi_action(first) {
            (
                Some(first),
                after_name.strip_prefix(first).unwrap_or("").trim(),
            )
        } else {
            (None, after_name)
        }
    };
    if glyph.is_none() && matches!(plan_action, None | Some("same")) {
        return Some(PulumiTreeRow::Unchanged);
    }
    let action = plan_action.unwrap_or(match glyph {
        Some("+") => "create",
        Some("~") => "update",
        Some("-") => "delete",
        Some("+-") => "replace",
        Some("++") => "create-replacement",
        Some("--") => "delete-original",
        _ => "change",
    });
    let glyph = glyph.unwrap_or_else(|| pulumi_glyph_for_action(action));
    Some(PulumiTreeRow::Changed {
        glyph,
        res_type,
        res_name,
        action,
        info,
    })
}

enum PulumiDiffHeader<'a> {
    Same,
    Changed {
        glyph: &'static str,
        res_type: &'a str,
        action: &'a str,
    },
}

fn parse_pulumi_diff_header(raw: &str) -> Option<PulumiDiffHeader<'_>> {
    let trimmed = raw.trim();
    let (left, action_paren) = trimmed.rsplit_once(": (")?;
    let action = action_paren.strip_suffix(')')?;
    if !is_pulumi_action(action) {
        return None;
    }
    let mut glyph = None;
    let mut rest = left;
    for g in ["+-", "++", "--", "+", "~", "-"] {
        if let Some(after) = left.strip_prefix(g) {
            glyph = Some(g);
            rest = after.trim_start();
            break;
        }
    }
    if rest.is_empty() || !rest.contains(':') || rest.contains(' ') {
        return None;
    }
    if action == "same" && glyph.is_none() {
        return Some(PulumiDiffHeader::Same);
    }
    let glyph = glyph.unwrap_or_else(|| pulumi_glyph_for_action(action));
    Some(PulumiDiffHeader::Changed {
        glyph,
        res_type: rest,
        action,
    })
}

fn parse_pulumi_diff_field(trimmed: &str) -> Option<&str> {
    let rest = trimmed
        .strip_prefix("+-")
        .or_else(|| trimmed.strip_prefix("~ "))
        .or_else(|| trimmed.strip_prefix("+ "))
        .or_else(|| trimmed.strip_prefix("- "))?
        .trim_start();
    let (key, _) = rest.split_once(':')?;
    let key = key.trim().trim_matches('"');
    (!key.is_empty() && !key.contains(' ')).then_some(key)
}

// ---------------------------------------------------------------------------
// Semantic whitespace / reindent diff helpers
// ---------------------------------------------------------------------------

/// Files where leading indentation is semantically significant: only trailing
/// whitespace differences may be folded, NEVER leading indentation changes.
fn is_indent_significant_path(path: &str) -> bool {
    let Some(name) = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
    else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower == "makefile"
        || lower == "gnumakefile"
        || lower.starts_with("makefile.")
        || [
            ".py", ".pyi", ".yml", ".yaml", ".mk", ".haml", ".pug", ".nim", ".hs", ".lhs",
        ]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

fn is_ws_only_block(rem: &[String], add: &[String], indent_significant: bool) -> bool {
    if rem.is_empty() && add.is_empty() {
        return true;
    }
    if indent_significant {
        return rem.len() == add.len()
            && rem.iter().zip(add.iter()).all(|(r, a)| {
                r.trim_end_matches([' ', '\t', '\r']) == a.trim_end_matches([' ', '\t', '\r'])
            });
    }
    let mut r_iter = rem
        .iter()
        .map(String::as_str)
        .filter(|s| !s.trim_matches([' ', '\t', '\r']).is_empty());
    let mut a_iter = add
        .iter()
        .map(String::as_str)
        .filter(|s| !s.trim_matches([' ', '\t', '\r']).is_empty());
    loop {
        match (r_iter.next(), a_iter.next()) {
            (Some(r), Some(a)) => {
                if !ws_only_line_eq(r, a) {
                    return false;
                }
            }
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn ws_only_line_eq(r: &str, a: &str) -> bool {
    let rt = r.trim_matches([' ', '\t', '\r']);
    let at = a.trim_matches([' ', '\t', '\r']);
    if rt == at {
        return true;
    }
    normalize_ws_outside_quotes(rt) == normalize_ws_outside_quotes(at)
}

/// Collapse runs of horizontal whitespace (`' '`, `'\t'`) outside quoted literals
/// into a single space while preserving exact characters inside `"..."`, `'...'`
/// and `` `...` `` so string-literal whitespace edits are never folded.
fn normalize_ws_outside_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut pending_space = false;
    let mut prev: Option<char> = None;
    for (idx, c) in s.char_indices() {
        if let Some(q) = quote {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' && q != '`' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
        } else if matches!(c, ' ' | '\t' | '\r') {
            pending_space = true;
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
            if matches!(c, '"' | '`') {
                quote = Some(c);
            } else if c == '\'' {
                let rest = &s[idx + 1..];
                if let Some(next_sq) = rest.find('\'') {
                    let inside = &rest[..next_sq];
                    if !(matches!(prev, Some('&' | '<')) && inside.contains(['>', '&', '('])) {
                        quote = Some('\'');
                    }
                }
            }
        }
        prev = Some(c);
    }
    out
}

/// Verbose pytest failure line (`tests/test_a.py::test_x FAILED [ 50%]`) when
/// `--no-summary` / `-rN` omits the short test summary block.
fn pytest_verbose_failure(line: &str) -> Option<&str> {
    let rest = line.strip_suffix(" FAILED").or_else(|| {
        let (before, pct) = line.rsplit_once(" [")?;
        pct.ends_with("%]")
            .then(|| before.strip_suffix(" FAILED"))?
    })?;
    let nodeid = rest.trim();
    (nodeid.contains("::") && !nodeid.contains(' ')).then_some(nodeid)
}

/// The detail of a commit header whose message is empty.
const NO_MESSAGE: &str = "(no message)";

/// The old and new line counts a `@@ -a,b +c,d @@` hunk header announces. An
/// omitted count is 1.
fn hunk_counts(line: &str) -> Option<(usize, usize)> {
    let ranges = line.strip_prefix("@@ -")?.split_once(" @@")?.0;
    let (old, new) = ranges.split_once(" +")?;
    let count = |range: &str| match range.split_once(',') {
        Some((_, count)) => count.parse().ok(),
        None => range.parse::<usize>().ok().map(|_| 1),
    };
    Some((count(old)?, count(new)?))
}

/// Parent count announced by a combined diff hunk header (`@@@ ... @@@` -> 2,
/// `@@@@ ... @@@@` -> 3, etc.).
fn combined_hunk_parents(line: &str) -> Option<usize> {
    let at = line.bytes().take_while(|&b| b == b'@').count();
    if at < 3 {
        return None;
    }
    let rest = line.get(at..)?;
    if !rest.starts_with(" -") {
        return None;
    }
    let closing = format!(" {}", "@".repeat(at));
    rest.contains(&closing).then_some(at - 1)
}

/// Whether `line` is a standard (`@@ ... @@`) or combined (`@@@ ... @@@`) hunk header.
fn is_hunk_header(line: &str) -> bool {
    line.starts_with("@@ ") || combined_hunk_parents(line).is_some()
}

/// The path named by a `diff --git a/... b/...`, `diff --cc ...` or
/// `diff --combined ...` header line.
fn diff_header_path(line: &str) -> Option<&str> {
    if let Some(names) = line.strip_prefix("diff --git ") {
        Some(names.rsplit_once(" b/").map_or(names, |(_, new)| new))
    } else if let Some(path) = line
        .strip_prefix("diff --cc ")
        .or_else(|| line.strip_prefix("diff --combined "))
    {
        let path = path.trim();
        (!path.is_empty()).then_some(path)
    } else {
        None
    }
}

/// A per-file line of Git's `--stat` block, such as ` src/lib.rs | 12 ++++----`
/// or ` assets/logo.png | Bin 0 -> 1234 bytes`.
fn is_stat_line(line: &str) -> bool {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let Some(rest) = line.strip_prefix(' ') else {
        return false;
    };
    let Some((path, stats)) = rest.rsplit_once(" | ") else {
        return false;
    };
    if path.trim().is_empty() {
        return false;
    }
    let stats = stats.trim();
    if stats == "Unmerged" || (stats.starts_with("Bin ") && stats.ends_with(" bytes")) {
        return true;
    }
    let mut parts = stats.split_whitespace();
    let Some(count) = parts.next() else {
        return false;
    };
    if count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match parts.next() {
        None => true,
        Some(graph) => parts.next().is_none() && graph.bytes().all(|b| matches!(b, b'+' | b'-')),
    }
}

/// The trailing summary line of Git's `--stat` block, such as
/// ` 2 files changed, 8 insertions(+), 3 deletions(-)`.
fn is_stat_summary(line: &str) -> bool {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let Some(rest) = line.strip_prefix(' ') else {
        return false;
    };
    let trimmed = rest.trim();
    let Some((num, tail)) = trimmed.split_once(' ') else {
        return false;
    };
    !num.is_empty()
        && num.bytes().all(|b| b.is_ascii_digit())
        && (tail.starts_with("file changed") || tail.starts_with("files changed"))
}

/// Mode/rename summary lines emitted by `git --stat --summary`.
fn is_stat_mode_line(line: &str) -> bool {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let Some(rest) = line.strip_prefix(' ') else {
        return false;
    };
    [
        "create mode ",
        "delete mode ",
        "mode change ",
        "rename ",
        "copy ",
        "rewrite ",
    ]
    .iter()
    .any(|prefix| rest.starts_with(prefix))
}

/// Whether a file-header line carries semantic metadata (mode/rename/copy/binary)
/// that must be preserved verbatim in the compact diff tier.
fn is_preserved_meta_header(line: &str, saw_mode_header: bool) -> bool {
    if [
        "old mode ",
        "new mode ",
        "deleted file mode ",
        "new file mode ",
        "mode ",
        "similarity index ",
        "dissimilarity index ",
        "rename from ",
        "rename to ",
        "copy from ",
        "copy to ",
        "Binary files ",
        "GIT binary patch",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
    {
        return true;
    }
    !saw_mode_header && (line == "--- /dev/null" || line == "+++ /dev/null")
}

/// Whether a hunk body line is an addition or deletion (handling both standard
/// 2-way hunks and `N`-parent combined merge hunks).
fn hunk_line_delta(line: &str, combined_parents: usize) -> (bool, bool) {
    if combined_parents >= 2 {
        let prefix = line.as_bytes().get(..combined_parents).unwrap_or(&[]);
        if prefix.contains(&b'+') {
            (true, false)
        } else if prefix.contains(&b'-') {
            (false, true)
        } else {
            (false, false)
        }
    } else {
        match line.as_bytes().first() {
            Some(b'+') => (true, false),
            Some(b'-') => (false, true),
            _ => (false, false),
        }
    }
}

/// Keep 100% of `+`/`-` change lines and trim surrounding context from 3 lines down
/// to 1 line immediately adjacent to any change.
fn trim_hunk_context(lines: &[String], combined_parents: usize) -> Vec<String> {
    if lines.is_empty() {
        return Vec::new();
    }
    let changed: Vec<bool> = lines
        .iter()
        .map(|line| {
            let (add, del) = hunk_line_delta(line, combined_parents);
            add || del
        })
        .collect();
    if !changed.iter().any(|&c| c) {
        return lines.to_vec();
    }
    let len = lines.len();
    let mut keep = vec![false; len];
    for i in 0..len {
        if changed[i]
            || (i > 0 && changed[i - 1])
            || (i + 1 < len && changed[i + 1])
            || (lines[i].starts_with('\\') && i > 0 && (changed[i - 1] || keep[i - 1]))
        {
            keep[i] = true;
        }
    }
    let mut out = Vec::with_capacity(len);
    let mut emitted_any = false;
    let mut skipped_since_last = false;
    for (i, line) in lines.iter().enumerate() {
        if keep[i] {
            if emitted_any && skipped_since_last {
                out.push(" ...".to_owned());
            }
            out.push(line.clone());
            emitted_any = true;
            skipped_since_last = false;
        } else if emitted_any {
            skipped_since_last = true;
        }
    }
    out
}

/// The commit id on a `commit SHA` or `commit SHA (decorations)` line.
fn commit_id(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("commit ")?;
    let (id, decorations) = rest.split_once(' ').unwrap_or((rest, ""));
    (id.len() >= 7
        && id.bytes().all(|b| b.is_ascii_hexdigit())
        && (decorations.is_empty() || decorations.starts_with('(')))
    .then_some(id)
}

impl Item {
    /// Whether this is a commit header from `git show` or `git log -p`.
    pub(crate) fn is_commit(&self) -> bool {
        self.name
            .strip_prefix("commit ")
            .is_some_and(|id| id.len() >= 7 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    }

    pub(crate) fn new(name: &str, detail: &str, n: usize) -> Self {
        Self::new_in_stream(name, detail, n, crate::capture::Stream::Stdout)
    }

    pub(crate) fn new_in_stream(
        name: &str,
        detail: &str,
        n: usize,
        stream: crate::capture::Stream,
    ) -> Self {
        Self {
            name: short(name, 320),
            detail: short(detail, 360),
            digest: String::new(),
            first: n,
            last: n,
            stderr: matches!(stream, crate::capture::Stream::Stderr),
            added: 0,
            deleted: 0,
            file_first: 0,
        }
    }

    pub(crate) fn stream_name(&self) -> &'static str {
        if self.stderr { "stderr" } else { "stdout" }
    }
    /// The identity used to compare runs. A patch hunk drops its line ranges,
    /// which shift whenever an earlier hunk grows or shrinks.
    fn key(&self) -> (&str, &str, &str) {
        let detail = if is_hunk_header(&self.detail) {
            let at = self.detail.bytes().take_while(|&b| b == b'@').count();
            let marker = format!(" {}", "@".repeat(at));
            let after_open = &self.detail[at + 1..];
            after_open
                .split_once(&marker)
                .map_or(after_open, |(_, context)| context)
        } else {
            &self.detail
        };
        (&self.name, detail, &self.digest)
    }
    fn counts(items: &[Item]) -> BTreeMap<(&str, &str, &str), usize> {
        let mut counts = BTreeMap::new();
        for item in items {
            *counts.entry(item.key()).or_default() += 1;
        }
        counts
    }

    pub(crate) fn display(&self) -> String {
        format!(
            "{} {} [{}:{}-{}]",
            self.name,
            self.detail,
            self.stream_name(),
            self.first,
            self.last
        )
    }
}

pub fn short(s: &str, len: usize) -> String {
    s[..s.floor_char_boundary(s.len().min(len))].to_owned()
}

impl Summary {
    /// A test outcome line that reports failures, such as `test result: FAILED.` or
    /// `1 failed, 2 passed in 0.10s`. Output cut by `| tail` can keep this line and
    /// lose the names of the failing tests.
    fn failing_outcome(&self) -> Option<&str> {
        static FAILED: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"\bFAILED\b|\bFAIL\b|\b(?:FAILURES|ERRORS)!|\b(?:Failures|Errors):\s*[1-9][0-9]*\b|(?:^|[^0-9])[1-9][0-9]* (?:failed|failing|failures?|errors?|timed out)\b",
            )
            .expect("failing outcome pattern")
        });
        (self.kind == "tests")
            .then(|| self.outcomes.iter().find(|line| FAILED.is_match(line)))
            .flatten()
            .map(String::as_str)
    }

    /// Whether this and `prior` are patches of different commits, as `git show HEAD`
    /// prints before and after a commit. Compared, every item of both would show.
    pub fn other_commits(&self, prior: &Summary) -> bool {
        let ids = |summary: &Summary| -> std::collections::BTreeSet<String> {
            summary
                .items
                .iter()
                .filter(|item| item.is_commit())
                .map(|item| item.name.clone())
                .collect()
        };
        let (now, then) = (ids(self), ids(prior));
        !now.is_empty() && !then.is_empty() && now.is_disjoint(&then)
    }

    /// The first line of a patch summary, such as `1 commit(s), 3 diff section(s)`.
    pub(crate) fn patch_heading(&self) -> String {
        let commits = self.items.iter().filter(|item| item.is_commit()).count();
        let sections = format!("{} diff section(s)", self.items.len() - commits);
        if commits == 0 {
            sections
        } else {
            format!("{commits} commit(s), {sections}")
        }
    }

    /// A one-line outcome for handoffs, such as a failing test count or top lint rule.
    pub fn headline(&self) -> Option<String> {
        match self.kind.as_str() {
            "tests" if !self.items.is_empty() => Some(format!(
                "{} failing test(s), first {}",
                self.items.len(),
                self.items[0].name
            )),
            "tests" => self
                .cargo
                .as_ref()
                .filter(|totals| !totals.nextest_failures.is_empty())
                .map(|totals| {
                    let first = totals.nextest_failures[0]
                        .lines()
                        .next()
                        .unwrap_or(&totals.nextest_failures[0]);
                    let first = first.rsplit_once(" [").map_or(first, |(head, _)| head);
                    format!(
                        "{} failing test(s), first {first}",
                        totals.nextest_failures.len()
                    )
                })
                .or_else(|| self.failing_outcome().map(str::to_owned)),
            "lint" => {
                let total: usize = self
                    .items
                    .iter()
                    .filter_map(|i| i.digest.parse::<usize>().ok())
                    .sum();
                let top = self.items.first()?;
                Some(format!(
                    "{total} finding(s) in {} rule(s), first {} ×{}",
                    self.items.len(),
                    top.name,
                    top.digest
                ))
            }
            "diff" if !self.items.is_empty() => Some(self.patch_heading()),
            "plan" if !self.items.is_empty() || !self.outcomes.is_empty() => self
                .outcomes
                .last()
                .cloned()
                .or_else(|| Some(format!("{} resource change(s)", self.items.len()))),
            "inventory" | "search" | "listing" => self.outcomes.first().cloned(),
            _ => None,
        }
    }

    pub fn render(&self, previous: Option<(&str, &Summary)>, status: i32) -> Option<String> {
        if self.kind.is_empty()
            || (self.items.is_empty()
                && self.outcomes.is_empty()
                && self.cargo.is_none()
                && self.js.is_none()
                && self.plan.is_none()
                && !(self.kind == "diff" && previous.is_some_and(|(_, s)| !s.items.is_empty())))
        {
            return None;
        }
        let mut report = String::new();
        if self.kind == "inventory" {
            if status != 0 {
                return None;
            }
            report.push_str(self.outcomes.first()?);
            report.push('\n');
        }
        if self.kind == "plan" && status != 0 && self.items.is_empty() {
            return None;
        }
        if self.kind == "tests" {
            if let Some(js) = self.js.as_ref() {
                let has_diff = previous.is_some_and(|(_, old)| {
                    old.kind == self.kind && (!self.items.is_empty() || !old.items.is_empty())
                });
                if !has_diff {
                    if status != 0 && self.items.is_empty() {
                        return None;
                    }
                    return Some(js.render());
                }
            }
            if let Some(totals) = self
                .cargo
                .as_ref()
                .filter(|totals| totals.nextest_summary.is_some())
            {
                let has_stdout_diff = previous.is_some_and(|(_, old)| {
                    old.kind == self.kind && (!self.items.is_empty() || !old.items.is_empty())
                });
                if !has_stdout_diff {
                    if status != 0 && totals.nextest_failures.is_empty() && self.items.is_empty() {
                        return None;
                    }
                    return Some(totals.render());
                }
            }
            if status != 0 && self.items.is_empty() {
                return None;
            }
            // With no failing test, cargo's totals line comes first.
            if !self.items.is_empty() || (self.cargo.is_none() && self.js.is_none()) {
                if self.items.is_empty() && self.failing_outcome().is_some() {
                    report.push_str("failing test names not in the captured output\n");
                } else {
                    report.push_str(&format!("{} reported failing test(s)\n", self.items.len()));
                }
            }
        }
        let previous = previous.filter(|(_, s)| s.kind == self.kind);
        let mut remaining = Vec::new();
        if let Some((id, old)) = previous {
            // Compare as multisets so identical hunks are matched one for one.
            let known = Item::counts(&old.items);
            let current = Item::counts(&self.items);
            report.push_str(&format!("Compared with {id}:\n"));
            let mut unmatched = known.clone();
            for item in &self.items {
                match unmatched.get_mut(&item.key()) {
                    Some(left) if *left > 0 => {
                        *left -= 1;
                        remaining.push(item);
                    }
                    _ => report.push_str(&format!("+ {}\n", item.display())),
                }
            }
            let mut unmatched = current.clone();
            for item in &old.items {
                match unmatched.get_mut(&item.key()) {
                    Some(left) if *left > 0 => *left -= 1,
                    _ => report.push_str(&format!(
                        "- {} {} [prior {}:{}-{}]\n",
                        item.name,
                        item.detail,
                        item.stream_name(),
                        item.first,
                        item.last
                    )),
                }
            }
            if known == current {
                report.push_str(if self.kind == "inventory" {
                    "Attention list unchanged.\n"
                } else {
                    "Indexed items unchanged; raw output may differ.\n"
                });
            }
            if self.kind == "plan"
                && let Some(plan) = &self.plan
            {
                plan.render_extras(&mut report);
            }
        } else if self.kind == "tests" {
            let mut groups: BTreeMap<(&str, &str), Vec<&Item>> = BTreeMap::new();
            for item in &self.items {
                groups
                    .entry((&item.detail, &item.digest))
                    .or_default()
                    .push(item);
            }
            for ((detail, _), items) in groups {
                if detail.is_empty() {
                    for item in items {
                        report.push_str(&format!("{}\n", item.display()));
                    }
                } else {
                    report.push_str(&format!(
                        "{} test(s): {}\n",
                        items.len(),
                        items[0].display()
                    ));
                    for item in &items[1..] {
                        report.push_str(&format!(
                            "  {} [{}:{}]\n",
                            item.name,
                            item.stream_name(),
                            item.first
                        ));
                    }
                }
            }
        } else if self.kind == "diff" {
            return Some(self.render_diff_with(COMPACT_DIFF_BUDGET, "", |_, _, _, _| None));
        } else if self.kind == "plan" {
            if let Some(plan) = &self.plan {
                if let Some(header) = &plan.header {
                    report.push_str(header);
                    report.push('\n');
                }
                if plan.refreshed > 0 {
                    report.push_str(&format!(
                        "Refreshed/read {} resource(s) [stdout:{}]\n",
                        plan.refreshed,
                        format_line_range(plan.refreshed_first, plan.refreshed_last)
                    ));
                }
            }
            if !self.items.is_empty() {
                report.push_str(&format!("{} resource change(s)\n", self.items.len()));
            }
            for item in &self.items {
                report.push_str(&format!("{}\n", item.display()));
            }
            if let Some(plan) = &self.plan {
                if plan.unchanged > 0 {
                    report.push_str(&format!(
                        "{} unchanged [stdout:{}]\n",
                        plan.unchanged,
                        format_line_range(plan.unchanged_first, plan.unchanged_last)
                    ));
                }
                plan.render_extras(&mut report);
            }
        } else {
            for item in &self.items {
                report.push_str(&format!("{}\n", item.display()));
            }
        }
        if let Some(js) = self.js.as_ref().filter(|_| self.kind == "tests") {
            report.push_str(&js.render_after_diff(&remaining));
            return Some(report);
        }
        if let Some(totals) = self.cargo.as_ref().filter(|_| self.kind == "tests") {
            if totals.nextest_summary.is_some() {
                report.push_str(&totals.render_nextest_summary());
            } else {
                report.push_str(&totals.render());
            }
            return Some(report);
        }
        for line in self
            .outcomes
            .iter()
            .skip(usize::from(self.kind == "inventory"))
        {
            report.push_str(line);
            report.push('\n');
        }
        Some(report)
    }

    /// Render a diff/patch summary in two tiers:
    /// 1. **Compact diff** (`<= budget` and strictly smaller than raw): replaces
    ///    redundant 4-line git headers with `=== path (+A -D) [stdout:first-last]`,
    ///    preserves mode/rename/binary lines verbatim, folds large generated files,
    ///    and keeps 100% of `+`/`-` lines with 1 line of context.
    /// 2. **File-grouped hunk index**: falls back when even the compact diff exceeds
    ///    `budget`, grouping multi-hunk files with `(+A -D, N hunks)` stats and
    ///    folding large generated files to 1 line.
    pub(crate) fn render_diff_with<F>(&self, budget: usize, flags: &str, replace: F) -> String
    where
        F: Fn(&str, usize, usize, Option<usize>) -> Option<String>,
    {
        let heading = format!("{}\n", self.patch_heading());
        if !self.diff_entries.is_empty()
            && !self.compact_overflowed
            && let Some(compact) = self.try_render_compact(&heading, flags, budget, &replace)
        {
            return compact;
        }
        self.render_hunk_index(&heading, flags, &replace)
    }

    fn try_render_compact<F>(
        &self,
        heading: &str,
        flags: &str,
        budget: usize,
        replace: &F,
    ) -> Option<String>
    where
        F: Fn(&str, usize, usize, Option<usize>) -> Option<String>,
    {
        let mut out = String::new();
        out.push_str(heading);
        out.push_str(flags);
        for entry in &self.diff_entries {
            match entry {
                DiffEntry::Commit { item_index } => {
                    if let Some(item) = self.items.get(*item_index) {
                        out.push_str(&item.display());
                        out.push('\n');
                    }
                }
                DiffEntry::File(file) => {
                    let default = file.render_compact();
                    let chosen = match replace(
                        &file.path,
                        file.first,
                        file.last,
                        default.as_ref().map(String::len),
                    ) {
                        Some(replaced) => replaced,
                        None => default?,
                    };
                    out.push_str(&chosen);
                }
            }
            if out.len() > budget {
                return None;
            }
        }
        for line in &self.outcomes {
            out.push_str(line);
            out.push('\n');
        }
        let smaller = self.diff_raw_bytes == 0 || out.len() < self.diff_raw_bytes;
        (out.len() <= budget && smaller).then_some(out)
    }

    fn render_hunk_index<F>(&self, heading: &str, flags: &str, replace: &F) -> String
    where
        F: Fn(&str, usize, usize, Option<usize>) -> Option<String>,
    {
        let mut out = String::new();
        out.push_str(heading);
        out.push_str(flags);
        if self.diff_entries.is_empty() {
            let mut i = 0;
            while i < self.items.len() {
                let item = &self.items[i];
                if item.is_commit() {
                    out.push_str(&item.display());
                    out.push('\n');
                    i += 1;
                    continue;
                }
                let mut end = i + 1;
                while end < self.items.len()
                    && !self.items[end].is_commit()
                    && self.items[end].name == item.name
                {
                    end += 1;
                }
                let group = &self.items[i..end];
                let first = group[0].first;
                let last = group.last().map_or(first, |it| it.last);
                let default_len: usize = group.iter().map(|it| it.display().len() + 1).sum();
                if let Some(replaced) = replace(&item.name, first, last, Some(default_len)) {
                    out.push_str(&replaced);
                } else {
                    for it in group {
                        out.push_str(&it.display());
                        out.push('\n');
                    }
                }
                i = end;
            }
        } else {
            for entry in &self.diff_entries {
                match entry {
                    DiffEntry::Commit { item_index } => {
                        if let Some(item) = self.items.get(*item_index) {
                            out.push_str(&item.display());
                            out.push('\n');
                        }
                    }
                    DiffEntry::File(file) => {
                        let default = file.render_hunk_group(&self.items);
                        if let Some(replaced) =
                            replace(&file.path, file.first, file.last, Some(default.len()))
                        {
                            out.push_str(&replaced);
                        } else {
                            out.push_str(&default);
                        }
                    }
                }
            }
        }
        for line in &self.outcomes {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

impl DiffFile {
    fn render_folded(&self) -> String {
        format!(
            "=== {} (generated, +{} -{}) [stdout:{}-{}]\n",
            self.path, self.added, self.deleted, self.first, self.last
        )
    }

    fn render_pure_rename(&self) -> Option<String> {
        let (old, new) = self.pure_rename.as_ref()?;
        Some(format!(
            "rename {old} -> {new} (100%) [stdout:{}-{}]\n",
            self.first, self.last
        ))
    }

    fn render_compact(&self) -> Option<String> {
        if self.generated_folded {
            return Some(self.render_folded());
        }
        if let Some(renamed) = self.render_pure_rename() {
            return Some(renamed);
        }
        let hunks = self.hunks.as_ref()?;
        let mut out = String::new();
        if self.added > 0 || self.deleted > 0 {
            out.push_str(&format!(
                "=== {} (+{} -{}) [stdout:{}-{}]\n",
                self.path, self.added, self.deleted, self.first, self.last
            ));
        } else {
            out.push_str(&format!(
                "=== {} [stdout:{}-{}]\n",
                self.path, self.first, self.last
            ));
        }
        for meta in &self.meta_lines {
            out.push_str(meta);
            out.push('\n');
        }
        for hunk in hunks {
            out.push_str(&hunk.header);
            out.push('\n');
            for line in &hunk.lines {
                out.push_str(line);
                out.push('\n');
            }
        }
        Some(out)
    }

    fn render_hunk_group(&self, items: &[Item]) -> String {
        if self.generated_folded {
            return self.render_folded();
        }
        if let Some(renamed) = self.render_pure_rename() {
            return renamed;
        }
        let slice = items.get(self.items_start..self.items_end).unwrap_or(&[]);
        if slice.is_empty() {
            return String::new();
        }
        if slice.len() == 1 {
            let item = &slice[0];
            if is_hunk_header(&item.detail) && self.whitespace_hunk_firsts.contains(&item.first) {
                return format!(
                    "{} {} (whitespace/indent only, -{} +{} lines) [{}:{}-{}]\n",
                    item.name,
                    item.detail,
                    item.deleted,
                    item.added,
                    item.stream_name(),
                    item.first,
                    item.last
                );
            }
            return format!("{}\n", item.display());
        }
        let hunk_count = slice.iter().filter(|i| is_hunk_header(&i.detail)).count();
        let mut out = String::new();
        if hunk_count > 0 {
            let noun = if hunk_count == 1 { "hunk" } else { "hunks" };
            out.push_str(&format!(
                "{} (+{} -{}, {hunk_count} {noun}) [stdout:{}-{}]\n",
                self.path, self.added, self.deleted, self.first, self.last
            ));
        } else {
            out.push_str(&format!(
                "{} [stdout:{}-{}]\n",
                self.path, self.first, self.last
            ));
        }
        for item in slice {
            if is_hunk_header(&item.detail) {
                if self.whitespace_hunk_firsts.contains(&item.first) {
                    out.push_str(&format!(
                        "  {} (whitespace/indent only, -{} +{} lines) [{}:{}-{}]\n",
                        item.detail,
                        item.deleted,
                        item.added,
                        item.stream_name(),
                        item.first,
                        item.last
                    ));
                } else {
                    out.push_str(&format!(
                        "  {} (+{} -{}) [{}:{}-{}]\n",
                        item.detail,
                        item.added,
                        item.deleted,
                        item.stream_name(),
                        item.first,
                        item.last
                    ));
                }
            } else {
                out.push_str(&format!(
                    "  {} [{}:{}-{}]\n",
                    item.detail,
                    item.stream_name(),
                    item.first,
                    item.last
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `git show 0940ff5 4e5ef6a` output from this repository.
    const SHOW: &str = include_str!("../fixtures/patches/git_show.txt");

    fn parse(family: &str, text: &str) -> Summary {
        let mut parser = Parser::new(family, None);
        for (index, line) in text.split_inclusive('\n').enumerate() {
            parser.observe(line, index + 1);
        }
        parser.finish()
    }

    #[test]
    fn commit_patches_list_each_header_then_its_hunks() {
        let args = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        for (line, expected) in [
            ("git show HEAD~3", "git show"),
            ("git log -p -2", "git log"),
            ("git log -U1", "git log"),
            ("git log --oneline -5", "git"),
            ("git log -- -p", "git"),
            ("gh pr diff 42", "git diff"),
            ("glab mr diff 17", "git diff"),
        ] {
            assert_eq!(family(&args(line)), expected, "{line}");
        }
        let summary = parse("git show", SHOW);
        // `SHOW` (~1.6 KB) fits in Tier 1 (Compact Diff): preserves 100% of `+`/`-`
        // lines, mode/binary headers, and trims context to 1 line.
        let compact = summary.render(None, 0).unwrap();
        assert!(
            compact.len() < SHOW.len(),
            "{} vs {}",
            compact.len(),
            SHOW.len()
        );
        assert!(
            compact.starts_with(
                "2 commit(s), 4 diff section(s)\n\
                 commit 0940ff5cd49d build(deps): depend on libc on Unix for signal forwarding [stdout:1-7]\n\
                 === Cargo.lock (+1 -0) [stdout:9-20]\n"
            ),
            "{compact}"
        );
        assert!(compact.contains("+ \"libc\",\n"), "{compact}");
        assert!(compact.contains("+libc = \"0.2\"\n"), "{compact}");
        assert!(compact.contains("-# Justless\n"), "{compact}");
        assert!(
            compact.contains(
                "=== assets/logo.png [stdout:62-65]\n\
                 new file mode 100644\n\
                 Binary files /dev/null and b/assets/logo.png differ\n"
            ),
            "{compact}"
        );
        // Context trimmed from 3 lines down to 1 adjacent line: `"anyhow"` and `"clap"` are cut, `"ignore"` and `"proc-macro2"` remain.
        assert!(!compact.contains("\"anyhow\""), "{compact}");
        assert!(
            compact.contains("  \"ignore\",\n+ \"libc\",\n  \"proc-macro2\","),
            "{compact}"
        );
        // Tier 2 (Hunk Index) when compact diff does not fit the budget:
        let index = summary.render_diff_with(10, "", |_, _, _, _| None);
        assert_eq!(
            index,
            "2 commit(s), 4 diff section(s)\n\
             commit 0940ff5cd49d build(deps): depend on libc on Unix for signal forwarding [stdout:1-7]\n\
             Cargo.lock @@ -274,6 +274,7 @@ dependencies = [ [stdout:13-20]\n\
             Cargo.toml @@ -41,3 +41,6 @@ tree-sitter-zig = \"1.1\" [stdout:25-31]\n\
             commit 4e5ef6a7fe67 docs(readme): add the logo and tagline [stdout:33-37]\n\
             README.md @@ -1,4 +1,17 @@ [stdout:43-61]\n\
             assets/logo.png new file mode 100644 [stdout:62-65]\n"
        );
        assert_eq!(
            summary.headline().as_deref(),
            Some("2 commit(s), 4 diff section(s)")
        );
        // A rerun with a changed message reports that header only.
        let reworded = parse("git log", &SHOW.replace("add the logo", "add a logo"));
        let delta = reworded.render(Some(("run-1", &summary)), 0).unwrap();
        assert_eq!(
            delta,
            "Compared with run-1:\n\
             + commit 4e5ef6a7fe67 docs(readme): add a logo and tagline [stdout:33-37]\n\
             - commit 4e5ef6a7fe67 docs(readme): add the logo and tagline [prior stdout:33-37]\n"
        );
        // A rerun that shows only other commits is not compared with them.
        let second = &SHOW[SHOW.find("commit 4e5").unwrap()..];
        let other = second.replacen("4e5ef6a7fe67", "fedcba987654", 1);
        assert!(!parse("git show", second).other_commits(&summary));
        assert!(parse("git show", &other).other_commits(&summary));
        assert!(!summary.other_commits(&Summary::default()));
    }

    #[test]
    fn commit_output_without_a_plain_patch_keeps_the_preview() {
        let header = SHOW.split("diff --git").next().unwrap();
        for text in [
            // `--raw`, `--oneline`, a blank inside a hunk, `git show -s`, and `git show REV:path`.
            SHOW.replacen(
                "\ndiff --git",
                "\n:100644 100644 1111111 2222222 M\tCargo.lock\n\ndiff --git",
                1,
            ),
            SHOW.replacen(
                "commit 0940ff5cd49d5b8fc541e004d5dd8e69e793adad",
                "0940ff5 build(deps)",
                1,
            ),
            SHOW.replacen("  \"clap\",\n", "\n", 1),
            header.to_owned(),
            "[package]\nname = \"justless\"\n".to_owned(),
            // `--stat` alone without a patch still keeps the verbatim preview.
            format!("{header} Cargo.lock | 1 +\n 1 file changed, 1 insertion(+)\n"),
        ] {
            assert!(parse("git show", &text).items.is_empty(), "{text}");
        }
        // `git diff` output with non-stat prose before `diff --git` is discarded.
        assert!(parse("git diff", SHOW).items.is_empty());
        // `git diff --stat` alone (no patch) produces no items so raw stat stays verbatim.
        assert!(
            parse(
                "git diff",
                " src/lib.rs | 4 ++--\n 1 file changed, 2 insertions(+), 2 deletions(-)\n"
            )
            .items
            .is_empty()
        );
    }

    #[test]
    fn stat_blocks_merge_commits_and_tiered_rendering_preserve_all_changes() {
        // 1. `--stat` alongside `-p` on both `git show` and `git diff` is parsed cleanly.
        let with_stat = SHOW.replacen(
            "\ndiff --git",
            "\n Cargo.lock | 1 +\n Cargo.toml | 3 +++\n 2 files changed, 4 insertions(+)\n\ndiff --git",
            1,
        );
        let parsed_stat = parse("git show", &with_stat);
        assert_eq!(parsed_stat.items.len(), 6);
        assert!(
            parsed_stat
                .render(None, 0)
                .unwrap()
                .contains("+libc = \"0.2\""),
            "{:?}",
            parsed_stat.render(None, 0)
        );
        let diff_with_stat = concat!(
            " src/lib.rs | 2 +-\n",
            " 1 file changed, 1 insertion(+), 1 deletion(-)\n",
            "\n",
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            "index 1111111..2222222 100644\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -1,5 +1,5 @@ fn f() {\n",
            " a\n",
            " b\n",
            "-c\n",
            "+d\n",
            " e\n",
        );
        let parsed_diff_stat = parse("git diff", diff_with_stat);
        let rendered_diff_stat = parsed_diff_stat.render(None, 0).unwrap();
        assert!(
            rendered_diff_stat.contains("=== src/lib.rs (+1 -1) [stdout:4-13]"),
            "{rendered_diff_stat}"
        );
        assert!(
            rendered_diff_stat.contains("-c\n+d\n"),
            "{rendered_diff_stat}"
        );

        // 2. Merge commit (`diff --cc` with `@@@` combined hunk and `mode` header).
        let merge = "\
commit 1234567890abcdef1234567890abcdef12345678
Merge: 1111111 2222222
Author: Dev <dev@example.com>
Date:   Mon Jan 1 00:00:00 2026 +0000

    Merge branch 'feature'

diff --cc src/lib.rs
index 1111111,2222222..3333333
mode 100644,100644..100755
--- a/src/lib.rs
+++ b/src/lib.rs
@@@ -1,5 -1,5 +1,7 @@@ fn main() {
  line1
  line2
  line3
 +from_b
+ from_a
++merged
 -old_b
  line4
  line5
  line6
";
        let merged = parse("git show", merge);
        assert_eq!(merged.items.len(), 2);
        assert_eq!((merged.items[1].added, merged.items[1].deleted), (3, 1));
        let merged_rendered = merged.render(None, 0).unwrap();
        assert!(
            merged_rendered
                .contains("=== src/lib.rs (+3 -1) [stdout:8-23]\nmode 100644,100644..100755\n"),
            "{merged_rendered}"
        );
        assert!(
            merged_rendered.contains(" +from_b\n+ from_a\n++merged\n -old_b\n"),
            "{merged_rendered}"
        );
        assert!(!merged_rendered.contains("  line1\n"), "{merged_rendered}");
        assert!(merged_rendered.contains("  line3\n"), "{merged_rendered}");

        // 3. Large multi-hunk diff exceeds compact tier and falls back to file-grouped hunk index with (+A -D).
        let mut big = String::new();
        big.push_str("diff --git a/src/foo.rs b/src/foo.rs\nindex 1111111..2222222 100644\n--- a/src/foo.rs\n+++ b/src/foo.rs\n");
        for h in 0..3 {
            let line = 10 + h * 100;
            big.push_str(&format!("@@ -{line},4 +{line},60 @@ fn f{h}() {{\n ctx\n"));
            big.push_str(&format!("-old_{h}\n"));
            for k in 0..57 {
                big.push_str(&format!(
                    "+    let value_{h}_{k} = compute_something_long({h}, {k});\n"
                ));
            }
            big.push_str(" end\n");
        }
        let big_summary = parse("git diff", &big);
        let big_rendered = big_summary.render(None, 0).unwrap();
        assert!(
            big_rendered.starts_with("3 diff section(s)\n"),
            "{big_rendered}"
        );
        assert!(
            big_rendered.contains("src/foo.rs (+171 -3, 3 hunks) [stdout:1-"),
            "{big_rendered}"
        );
        assert!(
            big_rendered.contains("  @@ -10,4 +10,60 @@ fn f0() { (+57 -1) [stdout:"),
            "{big_rendered}"
        );
        assert!(
            big_rendered.contains("  @@ -210,4 +210,60 @@ fn f2() { (+57 -1) [stdout:"),
            "{big_rendered}"
        );

        // 4. Huge generated snapshot section folds to 1 line so real code stays in Tier 1 Compact Diff!
        let mut snap = String::from(
            "diff --git a/tests/out.snap b/tests/out.snap\nindex 111..222 100644\n--- a/tests/out.snap\n+++ b/tests/out.snap\n@@ -1,1 +1,200 @@\n-old\n",
        );
        for i in 0..200 {
            snap.push_str(&format!(
                "+generated snapshot output row {i:03} with payload\n"
            ));
        }
        snap.push_str(
            "diff --git a/src/real.rs b/src/real.rs\nindex 333..444 100644\n--- a/src/real.rs\n+++ b/src/real.rs\n@@ -1,5 +1,5 @@ fn real() {\n c1\n c2\n-let x = 1;\n+let x = 2;\n c3\n",
        );
        let snap_summary = parse("git diff", &snap);
        let snap_rendered = snap_summary.render(None, 0).unwrap();
        assert!(
            snap_rendered.len() < 400,
            "got {} bytes:\n{snap_rendered}",
            snap_rendered.len()
        );
        assert!(
            snap_rendered.contains("=== tests/out.snap (generated, +200 -1) [stdout:1-206]\n"),
            "{snap_rendered}"
        );
        assert!(
            snap_rendered.contains("=== src/real.rs (+1 -1) [stdout:207-216]\n"),
            "{snap_rendered}"
        );
        assert!(
            snap_rendered.contains("-let x = 1;\n+let x = 2;\n"),
            "{snap_rendered}"
        );
    }

    #[test]
    fn a_last_hunk_cut_short_is_marked_only_when_the_output_can_end_early() {
        let patch = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@ fn a\n x\n-y\n+z\n w\n@@ -9 +9,2 @@\n-q\n+r\n+s\n";
        let lines: Vec<&str> = patch.split_inclusive('\n').collect();
        let thinned: String = lines
            .iter()
            .filter(|line| !line.starts_with(' '))
            .copied()
            .collect();
        // The text, whether it can end early, and the last item's detail.
        for (text, ends_early, last) in [
            (patch.to_owned(), true, "@@ -9 +9,2 @@"),
            (lines[..11].concat(), true, "@@ -9 +9,2 @@ (cut off)"),
            (lines[..11].concat(), false, "@@ -9 +9,2 @@"),
            (lines[..7].concat(), true, "@@ -1,3 +1,3 @@ fn a (cut off)"),
            (
                thinned[..thinned.len() - 2].to_owned(),
                true,
                "@@ -9 +9,2 @@",
            ),
        ] {
            let mut parser = Parser::new("git diff", None);
            if ends_early {
                parser.ends_early();
            }
            for (index, line) in text.split_inclusive('\n').enumerate() {
                parser.observe(line, index + 1);
            }
            let summary = parser.finish();
            assert_eq!(summary.items.last().unwrap().detail, last, "{text}");
        }
    }

    #[test]
    fn cargo_nextest_failures_panics_and_summaries_are_parsed() {
        let args = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            family(&args("cargo nextest run --workspace")),
            "cargo nextest"
        );
        assert_eq!(family(&args("cargo nextest list")), "cargo");

        let failing = "\
    Starting 3 tests across 1 binary
        PASS [   0.004s] demo::unit test_ok
        FAIL [   0.012s] (1/2) demo::unit test_bad
--- STDERR:              demo::unit test_bad ---
thread 'test_bad' panicked at src/lib.rs:42:5:
assertion `left == right` failed
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
     TIMEOUT [  60.001s] demo::unit test_hang
------------
     Summary [  60.020s] 3 tests run: 1 passed, 1 failed, 1 timed out, 0 skipped
        FAIL [   0.012s] demo::unit test_bad
     TIMEOUT [  60.001s] demo::unit test_hang
";
        let summary = parse("cargo nextest", failing);
        let rendered = summary.render(None, 100).unwrap();
        assert!(
            rendered.contains("2 reported failing test(s)"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "FAIL demo::unit test_bad [stdout:5-8]\n  thread 'test_bad' panicked at src/lib.rs:42:5:\n  assertion `left == right` failed\n  left: 1\n  right: 2"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("TIMEOUT demo::unit test_hang [stdout:10]"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "Summary [  60.020s] 3 tests run: 1 passed, 1 failed, 1 timed out, 0 skipped"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("stdout: 3 nextest pass/status line(s)"),
            "{rendered}"
        );

        // Unmerged nextest output written to stderr compacts with `[stderr:...]` pointers.
        let mut parser = Parser::new("cargo nextest", None);
        for (index, line) in failing.split_inclusive('\n').enumerate() {
            parser.observe_stderr(line, index + 1);
        }
        let stderr_summary = parser.finish();
        let stderr_rendered = stderr_summary.render(None, 100).unwrap();
        assert!(
            stderr_rendered.contains("FAIL demo::unit test_bad [stderr:5-8]"),
            "{stderr_rendered}"
        );
        assert!(
            stderr_rendered.contains("TIMEOUT demo::unit test_hang [stderr:10]"),
            "{stderr_rendered}"
        );
        assert!(
            stderr_rendered.contains("stderr: 3 nextest pass/status line(s)"),
            "{stderr_rendered}"
        );
        assert_eq!(
            stderr_summary.headline().as_deref(),
            Some("2 failing test(s), first FAIL demo::unit test_bad")
        );

        let passing = "\
    Starting 3 tests across 1 binary
        PASS [   0.004s] demo::unit test_ok
        PASS [   0.005s] demo::unit test_bad
        PASS [   0.006s] demo::unit test_hang
------------
     Summary [   0.015s] 3 tests run: 3 passed, 0 skipped
";
        let fixed = parse("cargo nextest", passing);
        let delta = fixed.render(Some(("run-1", &summary)), 0).unwrap();
        assert!(delta.contains("Compared with run-1:"), "{delta}");
        assert!(delta.contains("- demo::unit test_bad "), "{delta}");
        assert!(delta.contains("- demo::unit test_hang "), "{delta}");
        assert!(
            delta.contains("Summary [   0.015s] 3 tests run: 3 passed, 0 skipped"),
            "{delta}"
        );
    }

    #[test]
    fn go_test_extracts_failure_diagnostics_and_compacts_many_passing_packages() {
        // Default non-verbose `go test ./...` prints diagnostic lines after `--- FAIL:`.
        let non_verbose = "\
--- FAIL: TestAdd (0.00s)
    math_test.go:18: got 3, want 4
FAIL
FAIL\texample.com/math\t0.004s
ok  \texample.com/util\t0.002s
";
        let summary = parse("go test", non_verbose);
        let rendered = summary.render(None, 1).unwrap();
        assert!(
            rendered.contains("1 test(s): TestAdd math_test.go:18: got 3, want 4 [stdout:1-2]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("FAIL\texample.com/math\t0.004s"),
            "{rendered}"
        );
        assert!(
            rendered.contains("ok  \texample.com/util\t0.002s"),
            "{rendered}"
        );

        // Verbose `go test -v ./...` prints diagnostic lines between `=== RUN` and `--- FAIL:` (including subtests).
        let verbose = "\
=== RUN   TestParent
=== RUN   TestParent/sub
    parent_test.go:42: unexpected empty response
    --- FAIL: TestParent/sub (0.00s)
--- FAIL: TestParent (0.00s)
FAIL
FAIL\texample.com/api\t0.010s
";
        let verbose_summary = parse("go test", verbose);
        let verbose_rendered = verbose_summary.render(None, 1).unwrap();
        assert!(
            verbose_rendered.contains(
                "TestParent/sub parent_test.go:42: unexpected empty response [stdout:3-4]"
            ),
            "{verbose_rendered}"
        );

        // 150 passing/no-test packages never hit the 128-outcome cap and collapse to one range line.
        let mut many = String::new();
        for i in 1..=145 {
            many.push_str(&format!("ok  \texample.com/pkg{i}\t0.00{i}s\n"));
        }
        for i in 1..=5 {
            many.push_str(&format!("?\texample.com/empty{i}\t[no test files]\n"));
        }
        let many_summary = parse("go test", &many);
        let many_rendered = many_summary.render(None, 0).unwrap();
        assert!(
            many_rendered
                .contains("go test: 145 passed package(s), 5 without tests [stdout:1-150]"),
            "{many_rendered}"
        );
        assert!(many_rendered.len() < 150, "{many_rendered}");
    }

    #[test]
    fn pytest_verbose_failures_and_error_or_skipped_outcomes_are_captured() {
        let verbose_no_summary = "\
tests/test_a.py::test_ok PASSED [ 50%]
tests/test_a.py::test_bad FAILED [100%]
========================= 1 failed, 1 passed in 0.12s =========================
";
        let summary = parse("pytest", verbose_no_summary);
        let rendered = summary.render(None, 1).unwrap();
        assert!(
            rendered.contains("1 reported failing test(s)"),
            "{rendered}"
        );
        assert!(rendered.contains("tests/test_a.py::test_bad"), "{rendered}");

        // When both verbose FAILED and short-summary FAILED appear, they merge into one entry.
        let both = "\
tests/test_a.py::test_bad FAILED [100%]
FAILED tests/test_a.py::test_bad - AssertionError: assert 1 == 2
========================= 1 failed in 0.05s =========================
";
        let merged = parse("pytest", both);
        assert_eq!(merged.items.len(), 1);
        assert_eq!(merged.items[0].detail, "AssertionError: assert 1 == 2");

        // Error-only and skipped-only summary lines are captured.
        let skipped = "========================= 5 skipped in 0.08s =========================\n";
        let skipped_summary = parse("pytest", skipped);
        assert_eq!(skipped_summary.outcomes, ["5 skipped in 0.08s"]);
    }

    #[test]
    fn terraform_plan_summarizes_resource_changes_and_reruns() {
        let args = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            family(&args("terraform plan -out=tfplan")),
            "terraform plan"
        );
        assert_eq!(family(&args("tofu plan")), "terraform plan");

        let plan1 = "\
Terraform will perform the following actions:

  # aws_instance.web[0] will be created
  + resource \"aws_instance\" \"web\" {
      + ami           = \"ami-111\"
      + instance_type = \"t3.micro\"
    }

  # aws_security_group.api will be updated in-place
  ~ resource \"aws_security_group\" \"api\" {
      ~ description = \"old\" -> \"new\"
    }

  # aws_launch_template.worker must be replaced
-/+ resource \"aws_launch_template\" \"worker\" {
      ~ image_id = \"ami-1\" -> \"ami-2\" # forces replacement
    }

Plan: 2 to add, 1 to change, 1 to destroy.
";
        let summary = parse("terraform plan", plan1);
        let rendered = summary.render(None, 0).unwrap();
        assert!(rendered.contains("3 resource change(s)"), "{rendered}");
        assert!(
            rendered.contains("+ aws_instance.web[0] will be created [stdout:3-7]"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "~ aws_security_group.api will be updated in-place (description) [stdout:9-12]"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "-/+ aws_launch_template.worker must be replaced (replace: image_id) [stdout:14-17]"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("Plan: 2 to add, 1 to change, 1 to destroy."),
            "{rendered}"
        );
        assert_eq!(
            summary.headline().as_deref(),
            Some("Plan: 2 to add, 1 to change, 1 to destroy.")
        );

        // Rerun reports only the resource whose attribute changed.
        let plan2 = plan1.replace("\"t3.micro\"", "\"t3.small\"");
        let summary2 = parse("terraform plan", &plan2);
        let delta = summary2.render(Some(("run-1", &summary)), 0).unwrap();
        assert!(delta.contains("Compared with run-1:"), "{delta}");
        assert!(delta.contains("+ + aws_instance.web[0]"), "{delta}");
        assert!(!delta.contains("aws_security_group.api"), "{delta}");

        // A failing plan with no resource items returns None so error preview stays verbatim.
        let errored = parse(
            "terraform plan",
            "Error: Unsupported argument on main.tf line 12\n",
        );
        assert!(errored.render(None, 1).is_none());
    }

    #[test]
    fn rspec_parses_failures_groups_identical_errors_passes_and_diffs_reruns() {
        let args = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        assert_eq!(family(&args("rspec spec/")), "rspec");
        assert_eq!(family(&args("bundle exec rspec spec/")), "rspec");
        assert_eq!(family(&args("bundle exec rspec -f doc")), "rspec");
        assert_eq!(family(&args("rspec --format json")), "other");
        assert_eq!(family(&args("rspec -fj")), "other");

        // Passing documentation-format run compacts dramatically to clean pass summary.
        let mut passing = String::new();
        for i in 1..=80 {
            passing.push_str(&format!(
                "OrderCalculator\n  handles billing scenario case #{i:02} with tax and discount\n"
            ));
        }
        passing.push_str(
            "\nFinished in 1.42 seconds (files took 0.65 seconds to load)\n80 examples, 0 failures\n",
        );
        let pass_summary = parse("rspec", &passing);
        let pass_rendered = pass_summary.render(None, 0).unwrap();
        assert!(
            pass_rendered.contains("0 reported failing test(s)\n80 examples, 0 failures\n"),
            "{pass_rendered}"
        );
        assert!(pass_rendered.len() * 40 < passing.len(), "{pass_rendered}");

        // Failing run with two identical failures (grouped!) plus one distinct failure.
        let run1 = "\
FF.F.....

Failures:

  1) Api#get returns 200
     Failure/Error: expect(status).to eq(200)

       expected: 200
            got: 500

       (compared using ==)
     # ./spec/api_spec.rb:14:in `block (3 levels) in <top (required)>'

  2) Api#post returns 200
     Failure/Error: expect(status).to eq(200)

       expected: 200
            got: 500

       (compared using ==)
     # ./spec/api_spec.rb:28:in `block (3 levels) in <top (required)>'

  3) UserMailer#welcome sends email
     Failure/Error: raise RuntimeError, \"SMTP refused\"

     RuntimeError:
       SMTP refused
     # ./app/mailers/user_mailer.rb:24:in `welcome'

Finished in 0.45 seconds (files took 0.31 seconds to load)
9 examples, 3 failures

Failed examples:

rspec ./spec/api_spec.rb:12 # Api#get returns 200
rspec ./spec/api_spec.rb:26 # Api#post returns 200
rspec ./spec/mailers/user_mailer_spec.rb:10 # UserMailer#welcome sends email
";
        let s1 = parse("rspec", run1);
        assert_eq!(s1.kind, "tests");
        assert_eq!(s1.items.len(), 3);
        let r1 = s1.render(None, 1).unwrap();
        assert!(r1.contains("3 reported failing test(s)"), "{r1}");
        assert!(
            r1.contains("2 test(s): ./spec/api_spec.rb:12 Api#get returns 200 Failure/Error: expect(status).to eq(200) expected: 200 got: 500 [stdout:5-12]"),
            "{r1}"
        );
        assert!(
            r1.contains("  ./spec/api_spec.rb:26 Api#post returns 200 [stdout:14]"),
            "{r1}"
        );
        assert!(
            r1.contains("1 test(s): ./spec/mailers/user_mailer_spec.rb:10 UserMailer#welcome sends email Failure/Error: raise RuntimeError, \"SMTP refused\" RuntimeError: SMTP refused [stdout:23-28]"),
            "{r1}"
        );
        assert!(r1.contains("9 examples, 3 failures"), "{r1}");
        assert!(r1.len() < run1.len(), "{r1}");

        // Rerun after fixing the two Api specs shows exact `Compared with run-1:` diff.
        let run2 = "\
..F......

Failures:

  1) UserMailer#welcome sends email
     Failure/Error: raise RuntimeError, \"SMTP refused\"

     RuntimeError:
       SMTP refused
     # ./app/mailers/user_mailer.rb:24:in `welcome'

Finished in 0.41 seconds (files took 0.30 seconds to load)
9 examples, 1 failure

Failed examples:

rspec ./spec/mailers/user_mailer_spec.rb:10 # UserMailer#welcome sends email
";
        let s2 = parse("rspec", run2);
        let delta = s2.render(Some(("run-1", &s1)), 1).unwrap();
        assert!(delta.contains("Compared with run-1:"), "{delta}");
        assert!(
            delta.contains("- ./spec/api_spec.rb:12 Api#get returns 200"),
            "{delta}"
        );
        assert!(
            delta.contains("- ./spec/api_spec.rb:26 Api#post returns 200"),
            "{delta}"
        );
        assert!(
            !delta.contains("+ ./spec/mailers/user_mailer_spec.rb:10"),
            "{delta}"
        );
        assert!(delta.contains("9 examples, 1 failure"), "{delta}");

        // Crash with no parsed examples returns None so error stays verbatim.
        let crashed = parse("rspec", "LoadError: cannot load such file -- missing\n");
        assert!(crashed.render(None, 1).is_none());
    }

    #[test]
    fn phpunit_paratest_and_pest_parse_pass_fail_and_rerun_diffs() {
        let args = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        assert_eq!(family(&args("vendor/bin/phpunit")), "phpunit");
        assert_eq!(
            family(&args("./vendor/bin/paratest --processes=4")),
            "phpunit"
        );
        assert_eq!(family(&args("./vendor/bin/pest")), "phpunit");
        assert_eq!(family(&args("php artisan test")), "phpunit");
        assert_eq!(family(&args("phpunit --log-junit out.xml")), "other");
        assert_eq!(family(&args("phpunit --teamcity")), "other");

        // 1. Passing PHPUnit run with lots of progress dots.
        let mut ok_php = String::from("PHPUnit 11.4.3 by Sebastian Bergmann and contributors.\n\n");
        for i in 1..=20 {
            ok_php.push_str(&format!(
                "............................................................... {:>4} / 1260 ({:>3}%)\n",
                i * 63,
                i * 5
            ));
        }
        ok_php
            .push_str("\nTime: 00:02.110, Memory: 32.00 MB\n\nOK (1260 tests, 3410 assertions)\n");
        let ok_summary = parse("phpunit", &ok_php);
        let ok_rendered = ok_summary.render(None, 0).unwrap();
        assert!(
            ok_rendered.contains("0 reported failing test(s)\nOK (1260 tests, 3410 assertions)\n"),
            "{ok_rendered}"
        );
        assert!(ok_rendered.len() * 15 < ok_php.len(), "{ok_rendered}");

        // 2. Failing PHPUnit run with both error and failure sections + rerun diff.
        let php_fail = "\
PHPUnit 11.4.3 by Sebastian Bergmann and contributors.

Runtime:       PHP 8.3.12

..F..E...                                                           9 / 9 (100%)

Time: 00:00.142, Memory: 18.00 MB

There was 1 error:

1) Tests\\Unit\\OrderTest::testAppliesTax
RuntimeException: Tax rate missing

/app/src/Order.php:42
/app/tests/Unit/OrderTest.php:27

--

There was 1 failure:

1) Tests\\Unit\\CartTest::testComputesTotal
Failed asserting that 100 matches expected 110.

/app/tests/Unit/CartTest.php:19

ERRORS!
Tests: 9, Assertions: 14, Errors: 1, Failures: 1.
";
        let s1 = parse("phpunit", php_fail);
        assert_eq!(s1.items.len(), 2);
        let r1 = s1.render(None, 2).unwrap();
        assert!(r1.contains("2 reported failing test(s)"), "{r1}");
        assert!(
            r1.contains("Tests\\Unit\\OrderTest::testAppliesTax RuntimeException: Tax rate missing [stdout:11-15]"),
            "{r1}"
        );
        assert!(
            r1.contains("Tests\\Unit\\CartTest::testComputesTotal Failed asserting that 100 matches expected 110. [stdout:21-24]"),
            "{r1}"
        );
        assert!(
            r1.contains("ERRORS! Tests: 9, Assertions: 14, Errors: 1, Failures: 1."),
            "{r1}"
        );

        // Rerun after fixing OrderTest shows removal of OrderTest in `Compared with run-1:`.
        let php_fail2 = "\
PHPUnit 11.4.3 by Sebastian Bergmann and contributors.

There was 1 failure:

1) Tests\\Unit\\CartTest::testComputesTotal
Failed asserting that 100 matches expected 110.

/app/tests/Unit/CartTest.php:19

FAILURES!
Tests: 9, Assertions: 14, Failures: 1.
";
        let s2 = parse("phpunit", php_fail2);
        let delta = s2.render(Some(("run-1", &s1)), 1).unwrap();
        assert!(delta.contains("Compared with run-1:"), "{delta}");
        assert!(
            delta.contains("- Tests\\Unit\\OrderTest::testAppliesTax"),
            "{delta}"
        );
        assert!(
            !delta.contains("+ Tests\\Unit\\CartTest::testComputesTotal"),
            "{delta}"
        );
        assert!(
            delta.contains("FAILURES! Tests: 9, Assertions: 14, Failures: 1."),
            "{delta}"
        );

        // 3. Pest output with ANSI colors and snippet lines.
        let pest_raw = "\
   \x1b[32mPASS\x1b[0m  Tests\\Unit\\OrderTest
  ✓ calculates subtotal                                        0.01s

   \x1b[31mFAIL\x1b[0m  Tests\\Unit\\CartTest
  ⨯ computes total                                             0.03s
  ────────────────────────────────────────────────────────────────────
   \x1b[31mFAILED\x1b[0m  Tests\\Unit\\CartTest > computes total
  Failed asserting that 100 is identical to 110.

  at tests/Unit/CartTest.php:19
     18▕     expect($cart->total())->toBe(110);
  ➜  19▕     expect($cart->grandTotal())->toBe(110);

  Tests:    1 failed, 1 passed (4 assertions)
  Duration: 0.18s
";
        let pest_summary = parse("phpunit", pest_raw);
        let pest_rendered = pest_summary.render(None, 1).unwrap();
        assert!(
            pest_rendered.contains("1 reported failing test(s)"),
            "{pest_rendered}"
        );
        assert!(
            pest_rendered.contains("Tests\\Unit\\CartTest > computes total Failed asserting that 100 is identical to 110. [stdout:7-12]"),
            "{pest_rendered}"
        );
        assert!(
            pest_rendered.contains("Tests: 1 failed, 1 passed (4 assertions)"),
            "{pest_rendered}"
        );
    }

    #[test]
    fn whitespace_reindent_hunks_fold_safely_and_never_fold_semantic_edits() {
        // 1. Large pure-reindent hunk in Rust folds to 1 line while real logic hunk stays verbatim.
        let mut patch = String::from(concat!(
            "diff --git a/src/service.rs b/src/service.rs\n",
            "index 1111111..2222222 100644\n",
            "--- a/src/service.rs\n",
            "+++ b/src/service.rs\n",
            "@@ -10,42 +10,43 @@ fn reindented() {\n",
            "     // header\n",
        ));
        for i in 0..40 {
            patch.push_str(&format!("-  let v_{i} = compute(a,   b, {i});\n"));
        }
        for i in 0..40 {
            patch.push_str(&format!("+    let v_{i} = compute(a, b, {i});\n"));
        }
        patch.push_str("+\n"); // pure blank-line insertion inside non-indent-significant file
        patch.push_str("     // footer\n");
        patch.push_str(concat!(
            "@@ -100,5 +101,5 @@ fn real_logic() {\n",
            "     let a = 1;\n",
            "-    let b = a + 2;\n",
            "+    let b = a * 2;\n",
            "     b\n",
            " }\n",
        ));
        let summary = parse("git diff", &patch);
        let rendered = summary.render(None, 0).unwrap();
        assert!(
            rendered.contains(
                "@@ -10,42 +10,43 @@ fn reindented() { (whitespace/indent only, -40 +41 lines) [stdout:5-88]\n"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("-    let b = a + 2;\n+    let b = a * 2;\n"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("let v_0"),
            "folded reindent lines should not appear inline:\n{rendered}"
        );
        assert!(
            rendered.len() * 6 < patch.len(),
            "expected >83% reduction: {} vs {}\n{rendered}",
            rendered.len(),
            patch.len()
        );

        // 2. Zero-semantic-loss guards: MUST NOT fold non-whitespace change, string-literal
        //    whitespace change, moved statement across context, or EOF newline change.
        let semantic_cases = [
            (
                "string literal whitespace",
                concat!(
                    "diff --git a/src/msg.rs b/src/msg.rs\n",
                    "--- a/src/msg.rs\n+++ b/src/msg.rs\n",
                    "@@ -1,3 +1,3 @@\n",
                    " fn msg() {\n",
                    "-    let s = \"hello  world\";\n",
                    "+    let s = \"hello world\";\n",
                    " }\n",
                ),
                "-    let s = \"hello  world\";\n+    let s = \"hello world\";\n",
            ),
            (
                "moved line across context",
                concat!(
                    "diff --git a/src/flow.rs b/src/flow.rs\n",
                    "--- a/src/flow.rs\n+++ b/src/flow.rs\n",
                    "@@ -1,4 +1,4 @@\n",
                    "-    return early();\n",
                    "     side_effect();\n",
                    "+    return early();\n",
                    "     done();\n",
                ),
                "-    return early();\n",
            ),
            (
                "missing eof newline",
                concat!(
                    "diff --git a/src/eof.rs b/src/eof.rs\n",
                    "--- a/src/eof.rs\n+++ b/src/eof.rs\n",
                    "@@ -1,2 +1,2 @@\n",
                    " line1\n",
                    "-line2\n",
                    "\\ No newline at end of file\n",
                    "+line2\n",
                ),
                "\\ No newline at end of file\n",
            ),
        ];
        for (label, diff, expected) in semantic_cases {
            let s = parse("git diff", diff);
            let r = s.render(None, 0).unwrap();
            assert!(
                !r.contains("(whitespace/indent only"),
                "{label} MUST NOT fold:\n{r}"
            );
            assert!(r.contains(expected), "{label}:\n{r}");
        }

        // 3. Indentation-significant files (.py, .yml, .yaml, .mk, Makefile, .haml, .pug, .nim, .hs):
        //    NEVER fold leading-indent changes, ONLY fold trailing-whitespace-only changes.
        for (path, old_line, new_line) in [
            ("app/main.py", "  return x", "    return x"),
            ("deploy/ci.yml", "  run: test", "    run: test"),
            ("k8s/spec.yaml", "  replicas: 2", "    replicas: 2"),
            ("build/rules.mk", "\tcc main.c", "    cc main.c"),
            ("Makefile", "\t@echo hi", "  @echo hi"),
            ("views/index.haml", "  %p hi", "    %p hi"),
            ("views/home.pug", "  p hi", "    p hi"),
            ("src/calc.nim", "  echo x", "    echo x"),
            ("src/Lib.hs", "  where x = 1", "    where x = 1"),
        ] {
            let diff = format!(
                "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,3 +1,3 @@\n ctx\n-{old_line}\n+{new_line}\n end\n"
            );
            let s = parse("git diff", &diff);
            let r = s.render(None, 0).unwrap();
            assert!(
                !r.contains("(whitespace/indent only"),
                "leading indent in {path} MUST NOT fold:\n{r}"
            );
            assert!(
                r.contains(&format!("-{old_line}\n+{new_line}\n")),
                "{path}:\n{r}"
            );
        }

        // Trailing-whitespace-only in Python/YAML IS safely folded!
        let py_trailing = concat!(
            "diff --git a/app/main.py b/app/main.py\n",
            "--- a/app/main.py\n+++ b/app/main.py\n",
            "@@ -1,5 +1,5 @@\n",
            " def run():\n",
            "-    x = 1   \n",
            "-    return x\t \n",
            "+    x = 1\n",
            "+    return x\n",
            " # end\n",
        );
        let py_sum = parse("git diff", py_trailing);
        let py_ren = py_sum.render(None, 0).unwrap();
        assert!(
            py_ren
                .contains("@@ -1,5 +1,5 @@ (whitespace/indent only, -2 +2 lines) [stdout:4-10]\n"),
            "{py_ren}"
        );
    }

    #[test]
    fn pure_renames_fold_to_one_line_while_edited_or_chmod_renames_stay_lossless() {
        let diff = concat!(
            "diff --git a/src/legacy/auth.rs b/src/core/auth.rs\n",
            "similarity index 100%\n",
            "rename from src/legacy/auth.rs\n",
            "rename to src/core/auth.rs\n",
            "diff --git a/src/legacy/util.rs b/src/core/util.rs\n",
            "similarity index 88%\n",
            "rename from src/legacy/util.rs\n",
            "rename to src/core/util.rs\n",
            "index 1111111..2222222 100644\n",
            "--- a/src/legacy/util.rs\n",
            "+++ b/src/core/util.rs\n",
            "@@ -1,3 +1,3 @@\n",
            " pub fn id() -> u32 {\n",
            "-    1\n",
            "+    2\n",
            " }\n",
            "diff --git a/bin/old.sh b/bin/new.sh\n",
            "old mode 100644\n",
            "new mode 100755\n",
            "similarity index 100%\n",
            "rename from bin/old.sh\n",
            "rename to bin/new.sh\n",
        );
        let summary = parse("git diff", diff);
        let compact = summary.render(None, 0).unwrap();
        assert!(
            compact.contains("rename src/legacy/auth.rs -> src/core/auth.rs (100%) [stdout:1-4]\n"),
            "{compact}"
        );
        assert!(
            compact.contains("=== src/core/util.rs (+1 -1) [stdout:5-16]\n")
                && compact.contains("rename from src/legacy/util.rs\nrename to src/core/util.rs\n")
                && compact.contains("-    1\n+    2\n"),
            "rename with content edits must preserve hunks:\n{compact}"
        );
        assert!(
            compact.contains("old mode 100644\nnew mode 100755\n")
                && compact.contains("rename from bin/old.sh\nrename to bin/new.sh\n"),
            "rename with mode change must preserve mode metadata:\n{compact}"
        );

        // Tier 2 hunk index ALSO uses the 1-line pure rename format!
        let tier2 = summary.render_diff_with(10, "", |_, _, _, _| None);
        assert!(
            tier2.contains("rename src/legacy/auth.rs -> src/core/auth.rs (100%) [stdout:1-4]\n"),
            "{tier2}"
        );
    }

    #[test]
    fn expanded_generated_paths_and_large_svgs_fold_while_small_svgs_stay_inline() {
        let mut diff = String::new();
        // Large exported SVG (>100 changed lines) folds to 1 line.
        diff.push_str(concat!(
            "diff --git a/docs/architecture.svg b/docs/architecture.svg\n",
            "index 111..222 100644\n--- a/docs/architecture.svg\n+++ b/docs/architecture.svg\n",
            "@@ -1,2 +1,122 @@\n",
            "-<rect id=\"old\"/>\n",
        ));
        for i in 0..120 {
            diff.push_str(&format!(
                "+<path id=\"p{i:03}\" d=\"M{i} 10 L{i} 90 C10 20 30 40 50 60\"/>\n"
            ));
        }
        // Small hand-edited SVG (<=100 changed lines) stays inline!
        diff.push_str(concat!(
            "diff --git a/assets/icon.svg b/assets/icon.svg\n",
            "index 333..444 100644\n--- a/assets/icon.svg\n+++ b/assets/icon.svg\n",
            "@@ -1,3 +1,3 @@\n",
            " <svg>\n",
            "-  <circle r=\"4\"/>\n",
            "+  <circle r=\"6\"/>\n",
            " </svg>\n",
        ));
        // `.terraform.lock.hcl` (>25 lines) folds to 1 line.
        diff.push_str(concat!(
            "diff --git a/.terraform.lock.hcl b/.terraform.lock.hcl\n",
            "index 555..666 100644\n--- a/.terraform.lock.hcl\n+++ b/.terraform.lock.hcl\n",
            "@@ -1,1 +1,31 @@\n",
            " hashes = [\n",
        ));
        for i in 0..30 {
            diff.push_str(&format!("+  \"h1:providerhash{i:02}=\",\n"));
        }
        // Real code file stays verbatim in Tier 1!
        diff.push_str(concat!(
            "diff --git a/src/main.rs b/src/main.rs\n",
            "index 777..888 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n",
            "@@ -1,3 +1,3 @@\n",
            " fn main() {\n",
            "-    old();\n",
            "+    new();\n",
            " }\n",
        ));
        let summary = parse("git diff", &diff);
        let rendered = summary.render(None, 0).unwrap();
        assert!(
            rendered.contains("=== docs/architecture.svg (generated, +120 -1) [stdout:1-126]\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("=== .terraform.lock.hcl (generated, +30 -0) [stdout:136-171]\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("=== assets/icon.svg (+1 -1) [stdout:127-135]\n")
                && rendered.contains("-  <circle r=\"4\"/>\n+  <circle r=\"6\"/>\n"),
            "small SVG must not fold:\n{rendered}"
        );
        assert!(
            rendered.contains("=== src/main.rs (+1 -1) [stdout:172-180]\n")
                && rendered.contains("-    old();\n+    new();\n"),
            "{rendered}"
        );
        assert!(
            rendered.len() * 10 < diff.len(),
            "expected >90% reduction: {} vs {}\n{rendered}",
            rendered.len(),
            diff.len()
        );
    }

    #[test]
    fn rich_terraform_and_opentofu_plan_preserves_forces_replacement_attrs_outputs_and_diagnostics()
    {
        let args = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        assert_eq!(family(&args("terraform plan")), "terraform plan");
        assert_eq!(
            family(&args("tofu plan -var-file=prod.tfvars")),
            "terraform plan"
        );
        assert_eq!(family(&args("terraform plan -json")), "other");

        let mut raw = String::new();
        for i in 0..40 {
            raw.push_str(&format!(
                "aws_subnet.priv[{i}]: Refreshing state... [id=subnet-{i:04}]\n"
            ));
        }
        raw.push_str("data.aws_ami.ubuntu: Reading...\n");
        raw.push_str("data.aws_ami.ubuntu: Read complete after 1s [id=ami-0999]\n\n");
        raw.push_str(concat!(
            "Terraform will perform the following actions:\n\n",
            "  # aws_instance.web must be replaced\n",
            "-/+ resource \"aws_instance\" \"web\" {\n",
            "      ~ ami           = \"ami-old\" -> \"ami-new\" # forces replacement\n",
            "      ~ subnet_id     = \"subnet-1\" -> \"subnet-2\" # forces replacement\n",
            "      ~ root_block_device {\n",
            "          ~ volume_type = \"gp2\" -> \"gp3\" # forces replacement\n",
            "        }\n",
            "      ~ instance_type = \"t3.micro\" -> \"t3.small\"\n",
            "    }\n\n",
            "  # aws_launch_template.edge must be replaced\n",
            "+/- resource \"aws_launch_template\" \"edge\" {\n",
            "      ~ name = \"edge-v1\" -> \"edge-v2\" # forces replacement\n",
            "    }\n\n",
            "  # aws_security_group.api will be updated in-place\n",
            "  ~ resource \"aws_security_group\" \"api\" {\n",
            "        id          = \"sg-12345\"\n",
            "      ~ description = \"old\" -> \"new\"\n",
            "      ~ tags        = {\n",
            "          ~ \"Env\" = \"staging\" -> \"prod\"\n",
            "          + \"Team\" = \"core\"\n",
            "        }\n",
            "      + ingress {\n",
            "          + from_port = 443\n",
            "          + to_port   = 443\n",
            "        }\n",
            "      - egress {\n",
            "          - from_port = 80\n",
            "        }\n",
            "      ~ revoke_rules_on_delete = false -> true\n",
            "      ~ name_prefix            = \"a-\" -> \"b-\"\n",
            "    }\n\n",
            "  # aws_s3_bucket.old will be destroyed\n",
            "  - resource \"aws_s3_bucket\" \"old\" {\n",
            "      - bucket = \"legacy-bucket\" -> null\n",
            "    }\n\n",
            "  # data.aws_caller_identity.current will be read during apply\n",
            " <= data \"aws_caller_identity\" \"current\" {\n",
            "      + account_id = (known after apply)\n",
            "    }\n\n",
            "Plan: 2 to add, 1 to change, 3 to destroy.\n\n",
            "Changes to Outputs:\n",
            "  + endpoint = \"https://api.example.com\"\n",
            "  ~ url      = \"http://old\" -> \"https://new\"\n",
            "  - old_arn  = \"arn:aws:s3:::legacy-bucket\" -> null\n\n",
            "Warning: Value for undeclared variable \"legacy_flag\"\n",
        ));

        let stderr = concat!(
            "╷\n",
            "│ Warning: Argument is deprecated\n",
            "│\n",
            "│   with aws_s3_bucket.old,\n",
            "│   on main.tf line 42, in resource \"aws_s3_bucket\" \"old\":\n",
            "│   42:   acl = \"private\"\n",
            "╵\n",
        );

        let mut parser = Parser::new("terraform plan", None);
        for (i, line) in raw.split_inclusive('\n').enumerate() {
            parser.observe(line, i + 1);
        }
        for (i, line) in stderr.split_inclusive('\n').enumerate() {
            parser.observe_stderr(line, i + 1);
        }
        let summary = parser.finish();
        let rendered = summary.render(None, 0).unwrap();

        assert!(
            rendered.contains("Refreshed/read 42 resource(s) [stdout:1-42]\n"),
            "{rendered}"
        );
        assert!(rendered.contains("5 resource change(s)\n"), "{rendered}");
        assert!(
            rendered.contains(
                "-/+ aws_instance.web must be replaced (replace: ami, subnet_id, root_block_device.volume_type) [stdout:"
            ),
            "{rendered}"
        );
        assert!(
            rendered
                .contains("+/- aws_launch_template.edge must be replaced (replace: name) [stdout:"),
            "{rendered}"
        );
        // Up to 5 top-level changed attributes on `~` update (nested `"Env"`/`"Team"` excluded!), +1 more for the 6th.
        assert!(
            rendered.contains(
                "~ aws_security_group.api will be updated in-place (description, tags, ingress, egress, revoke_rules_on_delete, +1 more) [stdout:"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("- aws_s3_bucket.old will be destroyed [stdout:"),
            "{rendered}"
        );
        assert!(
            rendered
                .contains("<= data.aws_caller_identity.current will be read during apply [stdout:"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Changes to Outputs:\n")
                && rendered.contains("  + endpoint = \"https://api.example.com\" [stdout:")
                && rendered.contains("  ~ url      = \"http://old\" -> \"https://new\" [stdout:")
                && rendered
                    .contains("  - old_arn  = \"arn:aws:s3:::legacy-bucket\" -> null [stdout:"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Warning: Value for undeclared variable \"legacy_flag\" [stdout:"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "Warning: Argument is deprecated (with aws_s3_bucket.old, on main.tf line 42, in resource \"aws_s3_bucket\" \"old\") [stderr:1-7]"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("Plan: 2 to add, 1 to change, 3 to destroy.\n"),
            "{rendered}"
        );
        assert!(
            rendered.len() * 3 < raw.len(),
            "expected strong plan compaction: {} vs {}\n{rendered}",
            rendered.len(),
            raw.len()
        );
    }

    #[test]
    fn terragrunt_plan_and_run_all_plan_use_rich_terraform_plan_summary() {
        let args = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        for routed in [
            "terragrunt plan",
            "terragrunt plan -out=tfplan",
            "terragrunt run-all plan",
            "terragrunt run-all plan --terragrunt-non-interactive",
            "/usr/local/bin/terragrunt run-all plan --terragrunt-working-dir env/prod",
        ] {
            assert_eq!(family(&args(routed)), "terraform plan", "{routed}");
        }
        for skipped in [
            "terragrunt plan -json",
            "terragrunt plan --json",
            "terragrunt run-all plan -json",
            "terragrunt run-all plan --json",
            "terragrunt run-all plan -- -json",
            "terragrunt plan -h",
            "terragrunt plan --help",
            "terragrunt plan -version",
            "terragrunt plan --version",
            "terragrunt plan show",
            "terragrunt run-all plan show",
            "terragrunt show tfplan",
            "terragrunt apply",
            "terragrunt run-all apply",
            "terragrunt destroy",
        ] {
            assert_eq!(family(&args(skipped)), "other", "{skipped}");
        }

        let fam = family(&args(
            "terragrunt run-all plan --terragrunt-non-interactive",
        ));
        let mut raw = String::new();
        for i in 0..24 {
            raw.push_str(&format!(
                "module.vpc.aws_subnet.private[{i}]: Refreshing state... [id=subnet-{i:04}]\n"
            ));
        }
        raw.push_str(concat!(
            "  # aws_instance.api must be replaced\n",
            "-/+ resource \"aws_instance\" \"api\" {\n",
            "      ~ ami           = \"ami-01\" -> \"ami-02\" # forces replacement\n",
            "      ~ instance_type = \"t3.small\" -> \"t3.medium\"\n",
            "    }\n\n",
            "  # aws_s3_bucket.logs will be updated in-place\n",
            "  ~ resource \"aws_s3_bucket\" \"logs\" {\n",
            "      ~ tags = { \"Env\" = \"stage\" -> \"prod\" }\n",
            "    }\n\n",
            "Plan: 1 to add, 1 to change, 1 to destroy.\n",
            "Changes to Outputs:\n",
            "  ~ api_url = \"https://stage.example.com\" -> \"https://prod.example.com\"\n",
            "Plan: 0 to add, 0 to change, 0 to destroy.\n",
        ));
        let stderr = concat!(
            "╷\n",
            "│ Warning: Deprecated attribute\n",
            "│   on main.tf line 18, in resource \"aws_s3_bucket\" \"logs\":\n",
            "╵\n",
        );

        let mut parser = Parser::new(&fam, None);
        for (i, line) in raw.split_inclusive('\n').enumerate() {
            parser.observe(line, i + 1);
        }
        for (i, line) in stderr.split_inclusive('\n').enumerate() {
            parser.observe_stderr(line, i + 1);
        }
        let summary = parser.finish();
        let rendered = summary.render(None, 0).unwrap();
        assert!(
            rendered.contains("Refreshed/read 24 resource(s) [stdout:1-24]\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("-/+ aws_instance.api must be replaced (replace: ami) [stdout:"),
            "{rendered}"
        );
        assert!(
            rendered.contains("~ aws_s3_bucket.logs will be updated in-place (tags) [stdout:"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Changes to Outputs:\n") && rendered.contains("api_url"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Warning: Deprecated attribute (on main.tf line 18"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Plan: 1 to add, 1 to change, 1 to destroy.\n"),
            "{rendered}"
        );
    }

    #[test]
    fn pulumi_preview_tree_diff_outputs_diagnostics_and_reruns() {
        let args = |line: &str| line.split(' ').map(OsString::from).collect::<Vec<_>>();
        assert_eq!(family(&args("pulumi preview")), "pulumi preview");
        assert_eq!(
            family(&args("pulumi preview --stack prod --diff")),
            "pulumi preview"
        );
        assert_eq!(family(&args("pulumi up --dry-run")), "pulumi preview");
        assert_eq!(
            family(&args("pulumi up --stack dev --dry-run=true")),
            "pulumi preview"
        );
        assert_eq!(family(&args("pulumi up")), "other");
        assert_eq!(family(&args("pulumi preview --json")), "other");
        assert_eq!(family(&args("pulumi preview -j")), "other");
        assert_eq!(family(&args("pulumi up --dry-run --json")), "other");

        // 1. Standard tree view with 80 unchanged resources + creates/updates/replaces/deletes.
        let mut tree = String::from(concat!(
            "Previewing update (prod)\n\n",
            "View in Browser (Ctrl+O): https://app.pulumi.com/org/app/prod/previews/12345\n\n",
            "     Type                         Name               Plan                Info\n",
            "     pulumi:pulumi:Stack          my-app-prod\n",
            " +   ├─ aws:ec2:Vpc               main-vpc           create\n",
            " ~   ├─ aws:s3:Bucket             assets-bucket      update              [diff: ~tags,+versioning]\n",
            " +-  ├─ aws:ec2:Instance          web-server         replace             [diff: ~ami]\n",
            " ++  ├─ aws:ec2:Instance          api-server         create-replacement\n",
            " --  ├─ aws:ec2:Instance          api-server-old     delete-original\n",
            " -   ├─ aws:rds:Instance          legacy-db          delete\n",
        ));
        for i in 0..80 {
            let connector = if i == 79 { "└─" } else { "├─" };
            tree.push_str(&format!(
                "     {connector} aws:iam:RolePolicy         worker-policy-{i:02}\n"
            ));
        }
        tree.push_str(concat!(
            "\nOutputs:\n",
            "  + vpcId       : \"vpc-0123456\"\n",
            "  ~ bucketArn   : \"arn:aws:s3:::old\" => \"arn:aws:s3:::new\"\n",
            "  - legacyDbUrl : \"postgres://old\"\n\n",
            "Diagnostics:\n",
            "  aws:ec2:Instance (web-server):\n",
            "    warning: ami-123 is deprecated\n\n",
            "Resources:\n",
            "    + 1 to create\n",
            "    ~ 1 to update\n",
            "    - 1 to delete\n",
            "    +-1 to replace\n",
            "    4 changes. 81 unchanged\n",
        ));

        let s1 = parse("pulumi preview", &tree);
        assert_eq!(s1.kind, "plan");
        assert_eq!(s1.items.len(), 6);
        let r1 = s1.render(None, 0).unwrap();
        assert!(r1.contains("Previewing update (prod)\n"), "{r1}");
        assert!(r1.contains("6 resource change(s)\n"), "{r1}");
        assert!(
            r1.contains("+ aws:ec2:Vpc main-vpc create [stdout:7-7]\n"),
            "{r1}"
        );
        assert!(
            r1.contains(
                "~ aws:s3:Bucket assets-bucket update [diff: ~tags,+versioning] [stdout:8-8]\n"
            ),
            "{r1}"
        );
        assert!(
            r1.contains("+- aws:ec2:Instance web-server replace [diff: ~ami] [stdout:9-9]\n"),
            "{r1}"
        );
        assert!(
            r1.contains("++ aws:ec2:Instance api-server create-replacement [stdout:10-10]\n"),
            "{r1}"
        );
        assert!(
            r1.contains("-- aws:ec2:Instance api-server-old delete-original [stdout:11-11]\n"),
            "{r1}"
        );
        assert!(
            r1.contains("- aws:rds:Instance legacy-db delete [stdout:12-12]\n"),
            "{r1}"
        );
        assert!(r1.contains("81 unchanged [stdout:6-92]\n"), "{r1}");
        assert!(!r1.contains("worker-policy-40"), "{r1}");
        assert!(
            r1.contains("Outputs:\n")
                && r1.contains("  + vpcId       : \"vpc-0123456\" [stdout:")
                && r1.contains(
                    "  ~ bucketArn   : \"arn:aws:s3:::old\" => \"arn:aws:s3:::new\" [stdout:"
                )
                && r1.contains("  - legacyDbUrl : \"postgres://old\" [stdout:"),
            "{r1}"
        );
        assert!(
            r1.contains(
                "Diagnostics:\n  aws:ec2:Instance (web-server): warning: ami-123 is deprecated [stdout:"
            ),
            "{r1}"
        );
        assert!(
            r1.contains(
                "Resources: + 1 to create, ~ 1 to update, - 1 to delete, +-1 to replace; 4 changes. 81 unchanged\n"
            ),
            "{r1}"
        );
        assert!(
            r1.len() * 5 < tree.len(),
            "expected >80% reduction: {} vs {}\n{r1}",
            r1.len(),
            tree.len()
        );

        // Rerun comparison: web-server replacement fixed & new cloudfront added.
        let tree2 = tree
            .replace(
                " +-  ├─ aws:ec2:Instance          web-server         replace             [diff: ~ami]\n",
                " +   ├─ aws:cloudfront:Distribution cdn              create\n",
            );
        let s2 = parse("pulumi preview", &tree2);
        let delta = s2.render(Some(("run-1", &s1)), 0).unwrap();
        assert!(delta.contains("Compared with run-1:\n"), "{delta}");
        assert!(
            delta.contains("+ + aws:cloudfront:Distribution cdn create [stdout:"),
            "{delta}"
        );
        assert!(
            delta.contains("- +- aws:ec2:Instance web-server replace [diff: ~ami] [prior stdout:"),
            "{delta}"
        );
        assert!(!delta.contains("main-vpc"), "{delta}");

        // 2. `pulumi preview --diff` detailed view extracts changed field names & collapses `(same)`.
        let diff_out = concat!(
            "Previewing update (staging):\n",
            "  pulumi:pulumi:Stack: (same)\n",
            "    [urn=urn:pulumi:staging::my-app::pulumi:pulumi:Stack::my-app-staging]\n",
            "    + aws:ec2/vpc:Vpc: (create)\n",
            "        [urn=urn:pulumi:staging::my-app::aws:ec2/vpc:Vpc::main-vpc]\n",
            "        cidrBlock: \"10.0.0.0/16\"\n",
            "    ~ aws:s3/bucket:Bucket: (update)\n",
            "        [id=assets-bucket-123]\n",
            "        [urn=urn:pulumi:staging::my-app::aws:s3/bucket:Bucket::assets-bucket]\n",
            "      ~ tags: {\n",
            "          + env: \"staging\"\n",
            "        }\n",
            "      + versioning: {\n",
            "            enabled: true\n",
            "        }\n",
            "    +-aws:ec2/instance:Instance: (replace)\n",
            "        [id=i-0123456]\n",
            "        [urn=urn:pulumi:staging::my-app::aws:ec2/instance:Instance::web-server]\n",
            "      ~ ami: \"ami-old\" => \"ami-new\"\n",
            "      ~ instanceType: \"t3.micro\" => \"t3.small\"\n",
            "    aws:iam/role:Role: (same)\n",
            "        [id=app-role-111]\n",
            "        [urn=urn:pulumi:staging::my-app::aws:iam/role:Role::app-role]\n",
            "    --outputs:--\n",
            "      + vpcId: output<string>\n",
            "Resources:\n",
            "    + 1 to create\n",
            "    ~ 1 to update\n",
            "    +-1 to replace\n",
            "    3 changes. 2 unchanged\n",
        );
        let mut p = Parser::new("pulumi preview", None);
        for (i, line) in diff_out.split_inclusive('\n').enumerate() {
            p.observe(line, i + 1);
        }
        p.observe_stderr("warning: stack config key 'oldKey' is unused\n", 1);
        let ds = p.finish();
        let dr = ds.render(None, 0).unwrap();
        assert!(dr.contains("Previewing update (staging)\n"), "{dr}");
        assert!(
            dr.contains("+ aws:ec2/vpc:Vpc main-vpc create [stdout:4-6]\n"),
            "{dr}"
        );
        assert!(
            dr.contains(
                "~ aws:s3/bucket:Bucket assets-bucket update (tags, versioning) [stdout:7-15]\n"
            ),
            "{dr}"
        );
        assert!(
            dr.contains(
                "+- aws:ec2/instance:Instance web-server replace (ami, instanceType) [stdout:16-20]\n"
            ),
            "{dr}"
        );
        assert!(dr.contains("2 unchanged [stdout:2-23]\n"), "{dr}");
        assert!(
            dr.contains("Outputs:\n  + vpcId: output<string> [stdout:25]\n"),
            "{dr}"
        );
        assert!(
            dr.contains("warning: stack config key 'oldKey' is unused [stderr:1]\n"),
            "{dr}"
        );

        // Failing preview with no resource changes returns None so error stays verbatim.
        let crashed = parse("pulumi preview", "error: no stack selected\n");
        assert!(crashed.render(None, 255).is_none());
    }
}
