//! What a plain `git log` or `git status` shows in place of the pipes agents write
//! after it, such as `git log --oneline | head -20` or `git status -sb | head -2`:
//! a long log's newest commits and a count of the rest, and a long status's branch,
//! a count per state and its paths grouped by directory. Every count and group
//! points at its exact lines, which replay byte for byte, small output stays as Git
//! printed it, and machine formats keep the ordinary preview. All output here comes
//! from real Git in a temporary repository.
#![cfg(unix)]
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

use crate::common::{BIN, git, has, lacks, run_id, text};

struct Repo {
    _temp: TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("repo");
        let home = base.join("home");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&home).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join(".justless.toml"), "").unwrap();
        fs::write(
            root.join(".git/info/exclude"),
            ".justless/\n.justless.toml\n",
        )
        .unwrap();
        Repo {
            _temp: temp,
            root,
            home,
        }
    }

    /// A repository with `count` commits, each with a subject and a body line.
    fn with_commits(count: usize) -> Self {
        let repo = Self::new();
        let mut stream = String::new();
        for i in 1..=count {
            let message = format!("change {i}\n\nbody line for change {i}\n");
            stream.push_str(&format!(
                "commit refs/heads/main\nmark :{i}\ncommitter t <t@example.com> {} +0000\ndata {}\n{message}",
                1_700_000_000 + i * 60,
                message.len()
            ));
            if i > 1 {
                stream.push_str(&format!("from :{}\n", i - 1));
            }
            stream.push('\n');
        }
        let mut child = Command::new("git")
            .args(["fast-import", "--quiet"])
            .current_dir(&repo.root)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stream.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        repo
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("JUSTLESS_DISABLE")
            .stdin(Stdio::null());
        command
    }

    /// `justless run --fresh -- git ARGS`.
    fn run(&self, args: &[&str]) -> Output {
        let mut all = vec!["run", "--fresh", "--", "git"];
        all.extend_from_slice(args);
        self.command(&all).output().unwrap()
    }

    /// What Git itself prints for `args`, with the same environment.
    fn raw(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        text(&output.stdout)
    }

    /// Lines `from` to `to` of the run's stdout, as `justless log` replays them.
    fn replay(&self, output: &Output, (from, to): (usize, usize)) -> String {
        let id = run_id(output);
        let (from, to) = (from.to_string(), to.to_string());
        let args = [
            "log", &id, "--stream", "stdout", "--from", &from, "--to", &to,
        ];
        text(&self.command(&args).output().unwrap().stdout)
    }

    fn file(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

/// The `[stdout:A-B]` range at the end of `line`.
fn range(line: &str) -> (usize, usize) {
    let tag = line.rsplit_once("[stdout:").expect("a range").1;
    let tag = tag.strip_suffix(']').expect("a closed range");
    match tag.split_once('-') {
        Some((a, b)) => (a.parse().unwrap(), b.parse().unwrap()),
        None => (tag.parse().unwrap(), tag.parse().unwrap()),
    }
}

#[test]
fn a_long_log_shows_its_newest_commits_and_the_rest_replays_exactly() {
    let repo = Repo::with_commits(300);
    let raw = repo.raw(&["log", "--oneline"]);
    let output = repo.run(&["log", "--oneline"]);
    assert_eq!(output.status.code(), Some(0));
    let shown = text(&output.stdout);
    let (count, commits) = shown
        .trim_end()
        .rsplit_once('\n')
        .map(|(a, b)| (b, a))
        .unwrap();
    assert!(commits.lines().count() > 20, "{shown}");
    assert!(raw.starts_with(&format!("{commits}\n")), "{shown}");
    let (from, to) = range(count);
    assert_eq!(from, commits.lines().count() + 1);
    assert_eq!(to, 300);
    assert!(
        count.starts_with(&format!("+{} older commits ", 300 - from + 1)),
        "{count}"
    );
    assert_eq!(
        format!("{commits}\n{}", repo.replay(&output, (from, to))),
        raw
    );
    // The newest commit leads; the oldest is only in the archive.
    has(&shown, "change 300\n");
    lacks(&shown, "change 1\n");
}

#[test]
fn a_reversed_log_ends_with_its_newest_commits() {
    let repo = Repo::with_commits(300);
    let raw = repo.raw(&["log", "--oneline", "--reverse"]);
    let output = repo.run(&["log", "--oneline", "--reverse"]);
    assert_eq!(output.status.code(), Some(0));
    let shown = text(&output.stdout);
    let (count, commits) = shown.split_once('\n').unwrap();
    let (from, to) = range(count);
    assert_eq!(from, 1);
    assert!(
        count.starts_with(&format!("+{to} older commits ")),
        "{count}"
    );
    assert_eq!(
        format!("{}{commits}", repo.replay(&output, (from, to))),
        raw
    );
    assert!(commits.ends_with("change 300\n"), "{shown}");
}

#[test]
fn a_full_log_is_cut_between_commits() {
    let repo = Repo::with_commits(200);
    let raw = repo.raw(&["log"]);
    let output = repo.run(&["log"]);
    let shown = text(&output.stdout);
    assert!(shown.starts_with("commit "), "{shown}");
    let (commits, count) = shown.trim_end().rsplit_once('\n').unwrap();
    let (from, to) = range(count);
    assert_eq!(to, raw.lines().count());
    // The older commits start at a commit header, and every shown one is whole.
    let rest = repo.replay(&output, (from, to));
    assert!(rest.starts_with("commit "), "{rest}");
    assert_eq!(format!("{commits}\n{rest}"), raw);
    let older = rest.lines().filter(|l| l.starts_with("commit ")).count();
    assert!(
        count.starts_with(&format!("+{older} older commits ")),
        "{count}"
    );
}

#[test]
fn a_short_log_and_a_graph_keep_their_previews() {
    let repo = Repo::with_commits(300);
    let output = repo.run(&["log", "--oneline", "-n", "5"]);
    assert_eq!(
        text(&output.stdout),
        repo.raw(&["log", "--oneline", "-n", "5"])
    );
    // `--graph` draws lines between commits, which a count of commits would miss.
    let output = repo.run(&["log", "--oneline", "--graph"]);
    lacks(&text(&output.stdout), "older commits");
    has(
        &text(&output.stdout),
        "[output clipped. Full log retained.]",
    );
}

#[test]
fn a_failing_log_keeps_its_exit_status() {
    let repo = Repo::with_commits(3);
    let output = repo.run(&["log", "no-such-revision"]);
    assert_eq!(output.status.code(), Some(128));
    has(&text(&output.stderr), "no-such-revision");
}

#[test]
fn an_explicit_budget_bounds_the_log() {
    let repo = Repo::with_commits(300);
    let mut command = repo.command(&[
        "run",
        "--fresh",
        "--max-bytes",
        "600",
        "--",
        "git",
        "log",
        "--oneline",
    ]);
    let output = command.output().unwrap();
    let shown = text(&output.stdout);
    assert!(shown.len() <= 600, "{}", shown.len());
    has(&shown, "older commits [stdout:");
}

/// A repository with a conflict, staged and unstaged changes, and many untracked
/// files in a few directories.
fn busy_repo() -> Repo {
    let repo = Repo::new();
    for path in [
        "src/main.rs",
        "src/lib.rs",
        "src/net/http.rs",
        "docs/guide.md",
        "conflict.txt",
    ] {
        repo.file(path, &format!("base {path}\n"));
    }
    git(&repo.root, &["add", "-A"]);
    git(&repo.root, &["commit", "-q", "-m", "base"]);
    git(&repo.root, &["checkout", "-q", "-b", "side"]);
    repo.file("conflict.txt", "side\n");
    git(&repo.root, &["commit", "-q", "-am", "side"]);
    git(&repo.root, &["checkout", "-q", "main"]);
    repo.file("conflict.txt", "main\n");
    git(&repo.root, &["commit", "-q", "-am", "main"]);
    let merge = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["merge", "-q", "side"])
        .current_dir(&repo.root)
        .output()
        .unwrap();
    assert!(!merge.status.success(), "the merge conflicts");
    repo.file("src/main.rs", "staged\n");
    repo.file("src/net/tls.rs", "new\n");
    git(&repo.root, &["add", "src/main.rs", "src/net/tls.rs"]);
    repo.file("src/lib.rs", "dirty\n");
    fs::remove_file(repo.root.join("docs/guide.md")).unwrap();
    for i in 0..150 {
        repo.file(&format!("logs/run/run-{i:03}.log"), "x\n");
    }
    for i in 0..60 {
        repo.file(&format!("web/generated/part{i:02}.tsx"), "x\n");
    }
    repo
}

/// Each group line's range replays lines that name its paths.
fn groups_point_at_their_lines(repo: &Repo, output: &Output, shown: &str) {
    let mut groups = 0;
    for line in shown.lines().filter(|line| line.contains("[stdout:")) {
        groups += 1;
        let (dir, rest) = line.split_once(' ').unwrap();
        let first = rest
            .split_once(": ")
            .and_then(|(_, names)| names.split([',', ' ']).next())
            .unwrap();
        let replayed = repo.replay(output, range(line));
        let dir = dir.trim_start_matches("./");
        assert!(
            replayed.contains(first) && replayed.contains(dir),
            "{line} replays {replayed}"
        );
    }
    assert!(groups >= 4, "{shown}");
}

#[test]
fn a_long_status_leads_with_the_branch_and_a_count_per_state() {
    let repo = busy_repo();
    let raw = repo.raw(&["-c", "status.showUntrackedFiles=all", "status"]);
    assert!(raw.len() > 4096, "{}", raw.len());
    let output = repo.run(&["-c", "status.showUntrackedFiles=all", "status"]);
    assert_eq!(output.status.code(), Some(0));
    let shown = text(&output.stdout);
    assert!(
        shown.starts_with(
            "On branch main\nYou have unmerged paths.\n1 conflicted · 2 staged · 2 modified · 210 untracked\n"
        ),
        "{shown}"
    );
    lacks(&shown, "(use ");
    has(&shown, "logs/run/ 150: run-000.log");
    has(&shown, "web/generated/ 60: part00.tsx");
    groups_point_at_their_lines(&repo, &output, &shown);
}

#[test]
fn a_long_short_status_groups_the_same_states() {
    let repo = busy_repo();
    let output = repo.run(&["-c", "status.showUntrackedFiles=all", "status", "-sb"]);
    assert_eq!(output.status.code(), Some(0));
    let shown = text(&output.stdout);
    assert!(
        shown.starts_with("## main\n1 conflicted · 2 staged · 2 modified · 210 untracked\n"),
        "{shown}"
    );
    groups_point_at_their_lines(&repo, &output, &shown);
}

#[test]
fn porcelain_and_small_statuses_stay_as_git_printed_them() {
    let repo = busy_repo();
    let output = repo.run(&[
        "-c",
        "status.showUntrackedFiles=all",
        "status",
        "--porcelain",
    ]);
    let shown = text(&output.stdout);
    lacks(&shown, " untracked\n");
    assert!(shown.starts_with("UU conflict.txt\n"), "{shown}");
    let output = repo.run(&["status", "-sb"]);
    assert_eq!(text(&output.stdout), repo.raw(&["status", "-sb"]));
}
