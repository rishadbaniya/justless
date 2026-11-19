//! `justless test --compare REF`: after the working-tree run, the tests that
//! failed rerun by exact name at the merge base of REF and HEAD, in a temporary
//! Git worktree, and each failure is grouped by what the base says about the
//! same test. Only failing tests run at the base, so a test the change fixed
//! never shows up. Nothing here changes the working-tree run's exit status.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::LazyLock;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, ensure};
use regex::Regex;
use serde_json::{Value, json};

use super::{RunnerResult, plural, quote};
use crate::capture;
use crate::context::Since;
use crate::runners::{self, Failure, Kind, Tally};
use crate::selection::{self, RunnerPlan};
use crate::workspace::{Workspace, write_json};

/// Test names listed per group.
const NAMES: usize = 8;
/// Changed failures shown with both messages.
const CHANGED: usize = 3;
/// Bytes of each message shown for a changed failure.
const MESSAGE_BYTES: usize = 120;
/// Bytes of each message kept in `compare.json`.
const RECORD_BYTES: usize = 2048;
/// Marks the temporary directories this module creates.
const PREFIX: &str = "justless-compare-";

/// The commit failures are compared with.
pub(super) struct Base {
    /// `main (merge-base 3f2c1ab)`.
    pub label: String,
    name: String,
    commit: String,
}

/// Resolves `--compare REF` before anything runs, so a bad ref fails fast.
pub(super) fn resolve(workspace: &Workspace, reference: &str) -> Result<Base> {
    let since = Since::resolve(workspace, false, Some(reference)).map_err(|error| {
        anyhow!(
            "{}",
            format!("{error:#}").replacen("--base", "--compare", 1)
        )
    })?;
    match since {
        Since::Base {
            name,
            commit,
            short,
        } => Ok(Base {
            label: format!("{name} (merge-base {short})"),
            name,
            commit,
        }),
        _ => Err(anyhow!("--compare needs a ref")),
    }
}

/// A temporary worktree at the base commit. Dropping it removes the worktree
/// and its directory, on errors and panics too, and then releases the lock
/// that keeps comparisons in one project from overlapping.
pub(super) struct Checkout {
    /// The repository's top level, from which `git worktree` runs.
    toplevel: PathBuf,
    /// The workspace root within the repository, such as `crates/app/`.
    prefix: String,
    /// The directory made for this comparison: the checkout and an owner file.
    parent: PathBuf,
    path: PathBuf,
    /// Cargo's target directory for base builds, kept between comparisons.
    target: PathBuf,
    _lock: File,
}

impl Drop for Checkout {
    fn drop(&mut self) {
        remove(&self.toplevel, &self.path, &self.parent);
    }
}

fn remove(toplevel: &Path, path: &Path, parent: &Path) {
    let removed = Command::new("git")
        .arg("-C")
        .arg(toplevel)
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .output()
        .is_ok_and(|output| output.status.success());
    let _ = fs::remove_dir_all(parent);
    if !removed {
        let _ = Command::new("git")
            .arg("-C")
            .arg(toplevel)
            .args(["worktree", "prune"])
            .output();
    }
}

fn git_line(workspace: &Workspace, args: &[&str]) -> Result<String> {
    let output = workspace.git(args)?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        first_line(&output.stderr)
    );
    Ok(first_line(&output.stdout))
}

fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Checks out the base commit under the system temporary directory. Base
/// builds share one Cargo target directory under the project state, never the
/// working tree's: Cargo names a workspace's artifacts by their path relative
/// to the workspace, so a base build there would pass for the working tree's
/// own and the next plain `cargo test` would run the base code. The checkout
/// is newer than any base build that finished before the lock was taken, so
/// Cargo rebuilds the project's crates and reuses their dependencies.
fn checkout(workspace: &Workspace, base: &Base) -> Result<Checkout> {
    workspace.prepare_state()?;
    let state = workspace.state.join("compare");
    fs::create_dir_all(&state).with_context(|| format!("create {}", state.display()))?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state.join("lock"))
        .context("open the comparison lock")?;
    if lock.try_lock().is_err() {
        eprintln!("justless: waiting for another --compare in this project");
        lock.lock().context("lock the comparison")?;
    }
    let toplevel = PathBuf::from(git_line(workspace, &["rev-parse", "--show-toplevel"])?);
    let prefix = git_line(workspace, &["rev-parse", "--show-prefix"])?;
    remove_stale(workspace, &toplevel, &state);
    let parent = tempfile::Builder::new()
        .prefix(PREFIX)
        .tempdir()
        .context("create a temporary directory")?
        .keep();
    let name = toplevel
        .file_name()
        .map_or_else(|| "checkout".into(), |name| name.to_owned());
    let checkout = Checkout {
        path: parent.join(name),
        target: state.join("cargo-target"),
        toplevel,
        prefix,
        parent,
        _lock: lock,
    };
    fs::write(
        checkout.parent.join("owner"),
        state.to_string_lossy().as_bytes(),
    )?;
    let path = checkout.path.to_string_lossy().into_owned();
    // The project's checkout hooks have no business in a throwaway checkout.
    let hooks = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let output = workspace.git(&[
        "-c",
        &format!("core.hooksPath={hooks}"),
        "worktree",
        "add",
        "--detach",
        "--quiet",
        &path,
        &base.commit,
    ])?;
    ensure!(
        output.status.success(),
        "git worktree add: {}",
        first_line(&output.stderr)
    );
    Ok(checkout)
}

/// Removes worktrees an interrupted comparison of this project left behind.
/// The lock is held, so no other comparison of the project is using them.
fn remove_stale(workspace: &Workspace, toplevel: &Path, state: &Path) {
    let Ok(output) = workspace.git(&["worktree", "list", "--porcelain"]) else {
        return;
    };
    let owner = state.to_string_lossy();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(path) = line.strip_prefix("worktree ") else {
            continue;
        };
        let path = Path::new(path);
        let Some(parent) = path.parent() else {
            continue;
        };
        let ours = parent
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(PREFIX))
            && fs::read_to_string(parent.join("owner")).is_ok_and(|text| text == owner);
        if ours {
            remove(toplevel, path, parent);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    /// Passes at the base.
    New,
    /// No result at the base, as for a test added since.
    Absent,
    /// Fails at the base with the same message.
    Already,
    /// Fails at the base with another message.
    Changed,
    /// Fails at the base, with no message on either side to compare.
    Unclear,
    /// Not run at the base.
    Skipped,
}

impl Group {
    fn code(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Absent => "no-result-at-base",
            Self::Already => "already-failing",
            Self::Changed => "changed",
            Self::Unclear => "failing-at-both-no-message",
            Self::Skipped => "not-compared",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::New => "New failures, passing at the base",
            Self::Absent => "New failures, no result at the base (added or renamed since?)",
            Self::Already => "Already failing at the base, same message",
            Self::Changed => "Changed failures, failing at the base with another message",
            Self::Unclear => "Failing at the base too, no message to compare",
            Self::Skipped => "Not compared, doctests (their names carry line numbers)",
        }
    }
}

struct Compared<'a> {
    here: &'a Failure,
    group: Group,
    /// Failed at the base too.
    failed_at_base: bool,
    /// The normalized messages, here and at the base.
    now: String,
    then: String,
}

/// Paths that differ between the two checkouts only by where they are.
struct Roots {
    here: Vec<(String, &'static str)>,
    base: Vec<(String, &'static str)>,
}

impl Roots {
    /// `target` is Cargo's target directory for the working-tree run, which
    /// corresponds to the base build's own.
    fn new(workspace: &Workspace, checkout: &Checkout, target: Option<&Path>) -> Self {
        let forms = |path: &Path, token: &'static str| {
            let mut forms = vec![(path.to_string_lossy().into_owned(), token)];
            if let Ok(canonical) = path.canonicalize() {
                forms.push((canonical.to_string_lossy().into_owned(), token));
            }
            forms
        };
        let mut here = forms(&checkout.toplevel, "<repo>");
        let mut root = workspace.root.to_string_lossy().into_owned();
        if let Some(stripped) = root.strip_suffix(checkout.prefix.trim_end_matches('/'))
            && !checkout.prefix.is_empty()
        {
            root = stripped.trim_end_matches('/').to_owned();
        }
        here.extend(forms(Path::new(&root), "<repo>"));
        if let Some(target) = target {
            here.extend(forms(target, "<target>"));
        }
        let mut base = forms(&checkout.target, "<target>");
        base.extend(forms(&checkout.path, "<repo>"));
        // Longest first, so a path inside another is replaced whole.
        for list in [&mut here, &mut base] {
            list.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
            list.dedup();
        }
        Self { here, base }
    }
}

/// Cargo's target directory for the working-tree command run in `dir`: its
/// `--target-dir`, else the one `cargo metadata` reports, which follows
/// `CARGO_TARGET_DIR`, Cargo's configuration and otherwise the Cargo workspace
/// root's `target/`. Paths there, such as `CARGO_BIN_EXE_<name>` and
/// `CARGO_TARGET_TMPDIR`, name the same files as the base build's.
fn cargo_target(command: &[String], dir: &Path) -> Option<PathBuf> {
    let head: Vec<&str> = command
        .iter()
        .map(String::as_str)
        .take_while(|word| *word != "--")
        .collect();
    let value = |flag: &str| -> Option<&str> {
        let joined = format!("{flag}=");
        head.iter().enumerate().rev().find_map(|(at, word)| {
            word.strip_prefix(joined.as_str())
                .or_else(|| (*word == flag).then(|| head.get(at + 1).copied()).flatten())
        })
    };
    if let Some(target) = value("--target-dir") {
        return Some(dir.join(target));
    }
    let mut metadata = Command::new("cargo");
    metadata
        .args([
            "metadata",
            "--no-deps",
            "--offline",
            "--format-version",
            "1",
        ])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(manifest) = value("--manifest-path") {
        metadata.args(["--manifest-path", manifest]);
    }
    let reported = metadata
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
        .and_then(|value| value["target_directory"].as_str().map(PathBuf::from));
    reported.or_else(|| {
        std::env::var_os("CARGO_TARGET_DIR")
            .filter(|target| !target.is_empty())
            .map(|target| dir.join(target))
    })
}

/// A failure message as compared between the two runs: checkout paths, the
/// panicking thread, the panic's source location, `note:` lines and addresses
/// are left out, since they differ without the failure changing. Lines the test
/// printed before it panicked are left out too.
fn normalize(message: &str, roots: &[(String, &'static str)]) -> String {
    static ADDRESS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"0x[0-9a-fA-F]{6,}").expect("valid pattern"));
    let mut text = message.to_owned();
    for (path, token) in roots {
        if !path.is_empty() {
            text = text.replace(path.as_str(), token);
        }
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|line| panic_line(line).is_some())
        .unwrap_or(0);
    let mut kept = Vec::new();
    for line in &lines[start..] {
        let line = line.trim_end();
        if line.starts_with("stack backtrace:") {
            break;
        }
        if line.trim().is_empty() || line.starts_with("note: ") {
            continue;
        }
        match panic_line(line) {
            Some(Some(message)) => kept.push(message),
            Some(None) => {}
            None => kept.push(line.to_owned()),
        }
    }
    ADDRESS.replace_all(&kept.join("\n"), "0x…").into_owned()
}

/// A panic's first line: `thread 'NAME' panicked at FILE:LINE:COL:`, with the
/// message on the lines that follow, or before Rust 1.73
/// `thread 'NAME' panicked at 'MESSAGE', FILE:LINE:COL`. Newer toolchains add
/// the thread's ID after its name.
fn panic_line(line: &str) -> Option<Option<String>> {
    static PANIC: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^thread '.*?'(?: \(\d+\))? panicked at (.*)$").expect("valid pattern")
    });
    let at = PANIC.captures(line)?.get(1)?.as_str();
    Some(
        at.strip_prefix('\'')
            .and_then(|message| message.rsplit_once("', "))
            .map(|(message, _)| message.to_owned()),
    )
}

fn one_line(message: &str, limit: usize) -> String {
    let text = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return "(no message)".into();
    }
    if text.len() > limit {
        format!("{}…", &text[..text.floor_char_boundary(limit)])
    } else {
        text
    }
}

fn is_doctest(failure: &Failure) -> bool {
    failure.target.starts_with("doc:")
        || (failure.name.contains(" - ") && failure.name.ends_with(')'))
}

/// The same test in the base run: by target where both runs know it.
fn same_target(here: &str, base: &str) -> bool {
    here == base || here == "*" || base == "*"
}

/// The working-tree command as it runs in the checkout. A relative program
/// path, such as `.venv/bin/pytest`, stays the working tree's, which has the
/// environment; an absolute path into the repository, such as a
/// `--manifest-path`, moves into the checkout.
fn localize(base: &[String], here: &Path, roots: &Roots, checkout: &Checkout) -> Vec<String> {
    let into = checkout.path.to_string_lossy();
    base.iter()
        .enumerate()
        .map(|(at, word)| {
            if at == 0 {
                return if word.contains('/') && Path::new(word).is_relative() {
                    here.join(word).to_string_lossy().into_owned()
                } else {
                    word.clone()
                };
            }
            let (flag, value) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with('-') => (format!("{flag}="), value),
                _ => (String::new(), word.as_str()),
            };
            for (root, token) in &roots.here {
                if *token == "<repo>"
                    && let Some(rest) = value.strip_prefix(root.as_str())
                    && (rest.is_empty() || rest.starts_with('/'))
                {
                    return format!("{flag}{into}{rest}");
                }
            }
            word.clone()
        })
        .collect()
}

/// The base command for a runner: the working-tree command with the failing
/// names appended, made to run them all and to name each result.
fn base_argv(kind: Kind, command: &[String], names: &[String]) -> Vec<String> {
    match kind {
        Kind::Cargo => {
            // Quiet output names no passing test, fail-fast stops at the first
            // failing test binary, and a target directory given on the command
            // line would beat the base's own.
            let mut base = Vec::new();
            let mut separated = false;
            let mut words = command.iter().peekable();
            while let Some(word) = words.next() {
                separated |= word == "--";
                if word == "-q" || word == "--quiet" || word == "--format=terse" {
                    continue;
                }
                if word == "--format" && words.peek().is_some_and(|next| *next == "terse") {
                    words.next();
                    continue;
                }
                if !separated && word == "--target-dir" {
                    words.next();
                    continue;
                }
                if !separated && word.starts_with("--target-dir=") {
                    continue;
                }
                base.push(word.clone());
            }
            let head = base.iter().take_while(|word| *word != "--");
            let flags: Vec<String> = if head.clone().any(|word| word == "--no-fail-fast") {
                Vec::new()
            } else {
                vec!["--no-fail-fast".into()]
            };
            let mut tests = vec!["--exact".to_owned()];
            tests.extend(names.iter().cloned());
            selection::cargo_argv(&base, &flags, &tests)
        }
        _ => {
            // Every outcome on the summary, and no stop after the first failure.
            let mut argv = command.to_vec();
            argv.extend(["-rA".to_owned(), "--maxfail=0".to_owned()]);
            argv.extend(names.iter().cloned());
            argv
        }
    }
}

/// The base command as echoed: the names collapse to a count.
fn echo(argv: &[String], names: &[String]) -> String {
    let set: BTreeSet<&String> = names.iter().collect();
    let mut words: Vec<String> = argv
        .iter()
        .filter(|word| !set.contains(word))
        .map(|word| quote(word))
        .collect();
    words.push(format!("<{}>", plural(names.len(), "test name")));
    words.join(" ")
}

struct BaseRun {
    argv: Vec<String>,
    status: i32,
    tally: Tally,
    stderr: String,
    elapsed_ms: u64,
}

fn spawn(argv: &[String], dir: &Path, envs: &[(String, String)]) -> Result<Output> {
    Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .envs(envs.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("start {}", argv[0]))
}

/// Runs the failing names at the base. pytest refuses a whole run over one
/// node ID it cannot find, so those are dropped and the rest run again once.
fn run_base(
    (kind, command): (Kind, &[String]),
    base: &Base,
    checkout: &Checkout,
    dir: &Path,
    names: &[String],
    absent: &mut BTreeSet<String>,
) -> Result<BaseRun> {
    let mut envs = Vec::new();
    if kind == Kind::Cargo {
        envs.push((
            "CARGO_TARGET_DIR".to_owned(),
            checkout.target.to_string_lossy().into_owned(),
        ));
    } else {
        // An editable install would otherwise import the working tree's code.
        let mut paths = vec![dir.to_path_buf()];
        if dir.join("src").is_dir() {
            paths.push(dir.join("src"));
        }
        if let Some(existing) = std::env::var_os("PYTHONPATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        if let Ok(joined) = std::env::join_paths(paths) {
            envs.push(("PYTHONPATH".into(), joined.to_string_lossy().into_owned()));
        }
    }
    let started = Instant::now();
    let mut names: Vec<String> = names.to_vec();
    let mut attempt = 0;
    loop {
        let argv = base_argv(kind, command, &names);
        println!(
            "Run at {}, temporary worktree: {}",
            base.label,
            echo(&argv, &names)
        );
        std::io::stdout().flush()?;
        let output = spawn(&argv, dir, &envs)?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let status = capture::exit_code(output.status);
        let tally = runners::tally(kind, &stdout, &stderr);
        let missing: Vec<String> = if kind == Kind::Pytest && status == 4 {
            let mut missing = tally.not_found.clone();
            missing.extend(
                format!("{stdout}\n{stderr}")
                    .lines()
                    .filter_map(|line| line.strip_prefix("ERROR: file or directory not found: "))
                    .map(|id| id.trim().to_owned()),
            );
            missing.retain(|id| names.contains(id));
            missing
        } else {
            Vec::new()
        };
        if missing.is_empty() || attempt > 0 {
            return Ok(BaseRun {
                argv,
                status,
                tally,
                stderr,
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }
        names.retain(|name| !missing.contains(name));
        absent.extend(missing);
        if names.is_empty() {
            return Ok(BaseRun {
                argv,
                status: 0,
                tally: Tally {
                    known: true,
                    ..Tally::default()
                },
                stderr: String::new(),
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }
        attempt += 1;
    }
}

/// Compares one runner's failures with the base and returns the lines that
/// follow its verdict. Errors here are reported, never raised: the exit status
/// stays the working-tree run's.
pub(super) fn runner(
    workspace: &Workspace,
    base: &Base,
    checkout: &mut Option<Result<Checkout, String>>,
    plan: &RunnerPlan,
    result: &RunnerResult,
) -> String {
    let head = format!("Compare with {}", base.label);
    if !matches!(plan.runner, Kind::Cargo | Kind::Pytest) {
        return format!(
            "{head}: not supported for {}; nothing ran at the base.",
            plan.runner.label()
        );
    }
    if result.unavailable.is_some() {
        return format!("{head}: the runner did not start; nothing ran at the base.");
    }
    let mut failing: Vec<&Failure> = Vec::new();
    for failure in &result.failures {
        if result.failed.contains(&failure.name)
            && !failing
                .iter()
                .any(|known| known.target == failure.target && known.name == failure.name)
        {
            failing.push(failure);
        }
    }
    if failing.is_empty() {
        return if result.status == 0 {
            format!("{head}: no test failed, so nothing ran at the base.")
        } else {
            format!(
                "{head}: exit {} without a failing test name, such as a build error; nothing ran at the base.",
                result.status
            )
        };
    }
    let (doctests, failing): (Vec<&Failure>, Vec<&Failure>) =
        failing.into_iter().partition(|failure| is_doctest(failure));
    let mut compared: Vec<Compared> = doctests
        .into_iter()
        .map(|here| Compared {
            here,
            group: Group::Skipped,
            failed_at_base: false,
            now: String::new(),
            then: String::new(),
        })
        .collect();
    if failing.is_empty() {
        return render(&head, None, &compared);
    }
    if checkout.is_none() {
        *checkout = Some(self::checkout(workspace, base).map_err(|error| format!("{error:#}")));
    }
    let checkout = match checkout {
        Some(Ok(checkout)) => checkout,
        Some(Err(error)) => {
            return format!(
                "{head}: could not check out the base: {error}; failures are not grouped."
            );
        }
        None => unreachable!("set above"),
    };
    let relative = Path::new(&plan.cwd);
    let dir = checkout.path.join(&checkout.prefix).join(relative);
    if relative.is_absolute() || !dir.is_dir() {
        return format!(
            "{head}: {} does not exist at the base; failures are not grouped.",
            if plan.cwd.is_empty() {
                "the workspace root"
            } else {
                &plan.cwd
            }
        );
    }
    let here_dir = selection::runner_dir(workspace, &plan.cwd);
    let target = (plan.runner == Kind::Cargo)
        .then(|| cargo_target(&plan.base, &here_dir))
        .flatten();
    let roots = Roots::new(workspace, checkout, target.as_deref());
    // pytest stops the whole run over a file that is missing at the base.
    let mut absent: BTreeSet<String> = BTreeSet::new();
    let mut names: Vec<String> = Vec::new();
    for failure in &failing {
        let file = failure.name.split("::").next().unwrap_or_default();
        if plan.runner == Kind::Pytest && here_dir.join(file).is_file() && !dir.join(file).exists()
        {
            absent.insert(failure.name.clone());
        } else if !names.contains(&failure.name) {
            names.push(failure.name.clone());
        }
    }
    let run = if names.is_empty() {
        None
    } else {
        let command = localize(&plan.base, &here_dir, &roots, checkout);
        match run_base(
            (plan.runner, &command),
            base,
            checkout,
            &dir,
            &names,
            &mut absent,
        ) {
            Ok(run) => Some(run),
            Err(error) => {
                return format!("{head}: {error:#}; failures are not grouped.");
            }
        }
    };
    if let Some(run) = &run
        && !run.tally.known
    {
        let reason = run
            .stderr
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("error"))
            .map(|line| format!(" ({})", one_line(line, MESSAGE_BYTES)))
            .unwrap_or_default();
        return format!(
            "{head}: the base run reported no test result, exit {}{reason}; failures are not grouped.",
            run.status
        );
    }
    let empty = Tally::default();
    let tally = run.as_ref().map_or(&empty, |run| &run.tally);
    for here in failing {
        let at_base = tally
            .failures
            .iter()
            .find(|base| base.name == here.name && same_target(&here.target, &base.target));
        let now = normalize(&here.message, &roots.here);
        let then = at_base.map_or_else(String::new, |base| normalize(&base.message, &roots.base));
        let group = match at_base {
            Some(_) if now.is_empty() && then.is_empty() => Group::Unclear,
            Some(_) if now == then => Group::Already,
            Some(_) => Group::Changed,
            None if absent.contains(&here.name) => Group::Absent,
            None if tally.names.iter().any(|(target, names)| {
                same_target(&here.target, target) && names.contains(&here.name)
            }) =>
            {
                Group::New
            }
            None => Group::Absent,
        };
        compared.push(Compared {
            here,
            group,
            failed_at_base: at_base.is_some(),
            now,
            then,
        });
    }
    let text = render(&head, run.as_ref(), &compared);
    record(workspace, base, plan, result, run.as_ref(), &compared);
    text
}

fn render(head: &str, run: Option<&BaseRun>, compared: &[Compared]) -> String {
    let mut lines = Vec::new();
    let ran = compared
        .iter()
        .filter(|entry| entry.group != Group::Skipped)
        .count();
    lines.push(match run {
        Some(run) => format!(
            "{head}: reran {} failing here by exact name at the base, in a temporary worktree; exit {} ({}).",
            plural(ran, "test"),
            run.status,
            super::seconds(run.elapsed_ms)
        ),
        None if ran > 0 => format!("{head}: none of the failing tests exist at the base."),
        None => format!("{head}: nothing ran at the base."),
    });
    for group in [
        Group::New,
        Group::Absent,
        Group::Changed,
        Group::Unclear,
        Group::Already,
        Group::Skipped,
    ] {
        let mut members: Vec<&Compared> = compared
            .iter()
            .filter(|entry| entry.group == group)
            .collect();
        if members.is_empty() {
            continue;
        }
        members.sort_by(|a, b| a.here.name.cmp(&b.here.name));
        let names: Vec<&str> = members
            .iter()
            .map(|entry| entry.here.name.as_str())
            .collect();
        let shown = names.len().min(NAMES);
        let mut line = format!(
            "  {} ({}): {}",
            group.label(),
            names.len(),
            names[..shown].join(", ")
        );
        if names.len() > shown {
            line.push_str(&format!(", … {} more", names.len() - shown));
        }
        lines.push(line);
        if group == Group::Changed {
            for entry in members.iter().take(CHANGED) {
                lines.push(format!(
                    "    {} · here: {}",
                    entry.here.name,
                    one_line(&entry.now, MESSAGE_BYTES)
                ));
                lines.push(format!(
                    "    {} · base: {}",
                    entry.here.name,
                    one_line(&entry.then, MESSAGE_BYTES)
                ));
            }
        }
    }
    lines.join("\n")
}

fn clip(text: &str) -> &str {
    &text[..text.floor_char_boundary(text.len().min(RECORD_BYTES))]
}

/// Writes `compare.json` beside each run the runner recorded.
fn record(
    workspace: &Workspace,
    base: &Base,
    plan: &RunnerPlan,
    result: &RunnerResult,
    run: Option<&BaseRun>,
    compared: &[Compared],
) {
    let tests: Vec<Value> = compared
        .iter()
        .map(|entry| {
            json!({
                "name": entry.here.name,
                "target": entry.here.target,
                "group": entry.group.code(),
                "here": clip(&entry.now),
                "base": entry.failed_at_base.then(|| clip(&entry.then)),
            })
        })
        .collect();
    let value = json!({
        "version": 1,
        "ref": base.name,
        "merge_base": base.commit,
        "cwd": plan.cwd,
        "argv": run.map(|run| &run.argv),
        "status": run.map(|run| run.status),
        "elapsed_ms": run.map(|run| run.elapsed_ms),
        "tests": tests,
    });
    for id in &result.runs {
        if let Ok((directory, _)) = capture::load(workspace, id)
            && let Err(error) = write_json(&directory.join("compare.json"), &value)
        {
            eprintln!("justless: could not record the comparison for {id}: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(path: &str) -> Vec<(String, &'static str)> {
        vec![(path.to_owned(), "<repo>")]
    }

    #[test]
    fn panics_compare_without_location_thread_or_checkout_path() {
        let here = "printed 1\nthread 'cart::adds' panicked at src/cart.rs:12:9:\nassertion `left == right` failed\n  left: 4\n right: 5\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace";
        let base = "thread 'cart::adds' (19060277) panicked at src/cart.rs:10:5:\nassertion `left == right` failed\n  left: 4\n right: 5";
        assert_eq!(
            normalize(here, &roots("/home/me/shop")),
            normalize(base, &roots("/tmp/justless-compare-x/shop"))
        );
        assert_eq!(
            normalize(here, &[]),
            "assertion `left == right` failed\n  left: 4\n right: 5"
        );
        let other = base.replace("left: 4", "left: 3");
        assert_ne!(normalize(here, &[]), normalize(&other, &[]));
    }

    #[test]
    fn older_panics_and_paths_are_normalized() {
        let here = "thread 'reads' panicked at 'missing /home/me/shop/data.txt at 0x7ffd5e8a1c', src/lib.rs:3:5";
        let base = "thread 'reads' panicked at 'missing /tmp/justless-compare-x/shop/data.txt at 0x7ffd00001c', src/lib.rs:9:5";
        let now = normalize(here, &roots("/home/me/shop"));
        assert_eq!(now, "missing <repo>/data.txt at 0x…");
        assert_eq!(now, normalize(base, &roots("/tmp/justless-compare-x/shop")));
    }

    fn words(text: &str) -> Vec<String> {
        text.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn a_target_directory_on_the_command_line_wins() {
        let dir = Path::new("/work/shop");
        assert_eq!(
            cargo_target(&words("cargo test --target-dir out -- --target-dir x"), dir),
            Some(dir.join("out"))
        );
        assert_eq!(
            cargo_target(&words("cargo test --target-dir=/tmp/t --offline"), dir),
            Some(PathBuf::from("/tmp/t"))
        );
    }

    #[test]
    fn base_commands_name_every_result_and_do_not_stop_early() {
        let names = vec!["tests::a".to_owned()];
        let cargo = words("cargo test -q --target-dir /tmp/t --offline -- --nocapture");
        assert_eq!(
            base_argv(Kind::Cargo, &cargo, &names).join(" "),
            "cargo test --offline --no-fail-fast -- --nocapture --exact tests::a"
        );
        assert_eq!(
            base_argv(Kind::Pytest, &words("pytest -x"), &names).join(" "),
            "pytest -x -rA --maxfail=0 tests::a"
        );
    }
}
