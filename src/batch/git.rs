//! Recheck only display-oriented Git builtins. The diff runs with a private
//! index, refs and config. Working-tree filters, hooks, pagers and submodule
//! helpers configured in the original repository never run.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use tempfile::TempDir;

use super::{bounded, parse::local};

fn revision(arg: &str) -> bool {
    !arg.is_empty()
        && !arg.starts_with('-')
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/~^:".contains(&b))
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn pathspec(arg: &str) -> bool {
    local(arg) && !Path::new(arg).is_absolute() && !arg.starts_with(':')
}

fn diff_option(arg: &str) -> bool {
    matches!(
        arg,
        "--stat"
            | "--shortstat"
            | "--numstat"
            | "--name-only"
            | "--name-status"
            | "-p"
            | "--patch"
            | "--no-color"
            | "--no-ext-diff"
            | "--no-textconv"
            | "--no-patch"
            | "--oneline"
    ) || arg.strip_prefix("-U").is_some_and(digits)
}

/// A `git log` option that limits or formats the commits without running
/// anything else: a format that checks signatures runs GnuPG, so it is refused.
fn log_option(arg: &str) -> bool {
    diff_option(arg)
        || matches!(
            arg,
            "--no-merges" | "--merges" | "--first-parent" | "--reverse" | "--no-decorate"
        )
        || arg.strip_prefix('-').is_some_and(digits)
        || arg.strip_prefix("-n").is_some_and(digits)
        || arg.strip_prefix("--max-count=").is_some_and(digits)
        || (arg.starts_with("--format=") || arg.starts_with("--pretty="))
            && !arg
                .split('%')
                .skip(1)
                .any(|s| s.trim_start_matches(['+', '-', ' ']).starts_with('G'))
}

/// `git status` in its short or porcelain form. The long form and `--branch`
/// name the branch, which the isolated repository does not have.
fn status(rest: &[String]) -> bool {
    let mut short = false;
    let mut paths = false;
    for arg in rest {
        if paths || !arg.starts_with('-') {
            if !pathspec(arg) {
                return false;
            }
        } else if arg == "--" {
            paths = true;
        } else if matches!(
            arg.as_str(),
            "-s" | "--short" | "--porcelain" | "--porcelain=v1"
        ) {
            short = true;
        } else if !matches!(
            arg.as_str(),
            "-uno"
                | "-unormal"
                | "-uall"
                | "-u"
                | "--untracked-files=no"
                | "--untracked-files=normal"
                | "--untracked-files=all"
        ) {
            return false;
        }
    }
    short
}

pub(super) fn supported(args: &[String]) -> bool {
    let Some((op, rest)) = args.split_first() else {
        return false;
    };
    let option: fn(&str) -> bool = match op.as_str() {
        "show" | "diff" => |arg| diff_option(arg) || matches!(arg, "--cached" | "--staged"),
        "log" => log_option,
        "status" => return status(rest),
        _ => return false,
    };
    let mut paths = false;
    for arg in rest {
        if paths {
            if !pathspec(arg) {
                return false;
            }
        } else if arg == "--" {
            paths = true;
        } else if arg.starts_with('-') {
            if !option(arg) {
                return false;
            }
        } else if !revision(arg) {
            return false;
        }
    }
    true
}

fn git(cwd: &Path) -> Command {
    let mut cmd = Command::new("/usr/bin/git");
    cmd.current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["--no-pager", "--no-replace-objects", "--literal-pathspecs"]);
    if let Some(tz) = std::env::var_os("TZ") {
        cmd.env("TZ", tz);
    }
    cmd
}

fn query(cwd: &Path, args: &[&str], deadline: Instant) -> Option<String> {
    let (code, out) = bounded(git(cwd).args(args), 8192, deadline)?;
    (code == 0).then(|| out.strip_suffix('\n').unwrap_or(&out).to_owned())
}

fn oid(cwd: &Path, rev: &str, deadline: Instant) -> Option<String> {
    let id = query(
        cwd,
        &["rev-parse", "--verify", "--end-of-options", rev],
        deadline,
    )?;
    ([40, 64].contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

pub(super) struct Repository {
    dir: TempDir,
    root: PathBuf,
}

impl Repository {
    pub fn open(ws: &crate::workspace::Workspace, cwd: &Path, deadline: Instant) -> Option<Self> {
        let root = PathBuf::from(query(cwd, &["rev-parse", "--show-toplevel"], deadline)?)
            .canonicalize()
            .ok()?;
        if !root.starts_with(&ws.root) {
            return None;
        }
        let common = cwd
            .join(query(cwd, &["rev-parse", "--git-common-dir"], deadline)?)
            .canonicalize()
            .ok()?;
        let objects = common.join("objects").canonicalize().ok()?;
        if objects.to_str()?.chars().any(char::is_control) {
            return None;
        }
        let index = cwd.join(query(cwd, &["rev-parse", "--git-path", "index"], deadline)?);
        let dir = tempfile::tempdir().ok()?;
        let temp = dir.path();
        fs::create_dir_all(temp.join("objects/info")).ok()?;
        fs::create_dir_all(temp.join("refs/heads")).ok()?;
        fs::write(
            temp.join("objects/info/alternates"),
            format!("{}\n", objects.to_str()?),
        )
        .ok()?;
        let format = query(cwd, &["rev-parse", "--show-object-format"], deadline)?;
        let config = match format.as_str() {
            "sha1" => "[core]\nrepositoryformatversion=0\nbare=false\n",
            "sha256" => {
                "[core]\nrepositoryformatversion=1\nbare=false\n[extensions]\nobjectformat=sha256\n"
            }
            _ => return None,
        };
        fs::write(temp.join("config"), config).ok()?;
        fs::write(
            temp.join("HEAD"),
            oid(cwd, "HEAD", deadline).unwrap_or_else(|| "ref: refs/heads/unborn".into()) + "\n",
        )
        .ok()?;
        match fs::symlink_metadata(&index) {
            Ok(m) if m.is_file() && m.len() <= 32 * 1024 * 1024 => {
                fs::copy(index, temp.join("index")).ok()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => return None,
        }
        // `git status` lists untracked files past the repository's own excludes.
        let exclude = common.join("info/exclude");
        if fs::symlink_metadata(&exclude).is_ok_and(|m| m.is_file() && m.len() <= 1024 * 1024) {
            fs::create_dir_all(temp.join("info")).ok()?;
            fs::copy(exclude, temp.join("info/exclude")).ok()?;
        }
        Some(Self { dir, root })
    }

    pub fn output(&self, cwd: &Path, args: &[String], deadline: Instant) -> Option<String> {
        let mut cmd = git(cwd);
        cmd.env("GIT_DIR", self.dir.path())
            .env("GIT_WORK_TREE", &self.root)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.attributesFile=/dev/null",
            ])
            .arg(&args[0]);
        if args[0] == "status" {
            // The default excludes file sits under the home directory, and
            // submodules are never entered.
            for key in ["HOME", "XDG_CONFIG_HOME"] {
                if let Some(value) = std::env::var_os(key) {
                    cmd.env(key, value);
                }
            }
            cmd.arg("--ignore-submodules=all");
            let (options, paths): (Vec<_>, Vec<_>) = args[1..]
                .iter()
                .filter(|arg| *arg != "--")
                .partition(|arg| arg.starts_with('-'));
            cmd.args(options).arg("--").args(paths);
            let (code, text) = bounded(&mut cmd, super::MAX_BYTES, deadline)?;
            return (code == 0).then_some(text);
        }
        cmd.args([
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
        ]);
        let mut paths = false;
        for arg in &args[1..] {
            if paths || arg.starts_with('-') {
                paths |= arg == "--";
                cmd.arg(arg);
            } else if let Some((left, right)) = arg.split_once("..") {
                // The isolated directory has no named refs. Resolve endpoints
                // with rev-parse before asking diff to compare them.
                let (dots, right) = if let Some(right) = right.strip_prefix('.') {
                    ("...", right)
                } else {
                    ("..", right)
                };
                let resolve = |s: &str| oid(cwd, if s.is_empty() { "HEAD" } else { s }, deadline);
                cmd.arg(format!("{}{dots}{}", resolve(left)?, resolve(right)?));
            } else if let Some(id) = oid(cwd, arg, deadline) {
                cmd.arg(id);
            } else if matches!(args[0].as_str(), "diff" | "log")
                && local(arg)
                && cwd.join(arg).is_file()
            {
                cmd.arg("--").arg(arg);
                paths = true;
            } else {
                return None;
            }
        }
        let (code, text) = bounded(&mut cmd, super::MAX_BYTES, deadline)?;
        (code == 0).then_some(text)
    }
}
