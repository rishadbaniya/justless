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
    assert_eq!(output.status.code(), Some(0));
    let shown = text(&output.stdout);
    let (commits, count) = shown.trim_end().rsplit_once('\n').unwrap();
    let shown_lines: Vec<&str> = commits.lines().collect();
    assert!(
        shown_lines.len() >= 40,
        "compact log should fit >=40 commits in budget, got {}:\n{shown}",
        shown_lines.len()
    );
    // Every shown commit uses `<short-hash> YYYY-MM-DD Author: subject [stdout:A-B]`
    // and its `[stdout:A-B]` range replays the full commit header and body verbatim.
    for (idx, line) in shown_lines.iter().enumerate() {
        let expected_num = 200 - idx;
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        assert_eq!(parts.len(), 3, "unexpected compact commit line: {line}");
        assert!(
            parts[0].len() == 7 && parts[0].bytes().all(|b| b.is_ascii_hexdigit()),
            "expected 7-char short hash: {line}"
        );
        assert!(
            parts[1].starts_with("2023-11-"),
            "expected ISO YYYY-MM-DD date: {line}"
        );
        assert!(
            parts[2].starts_with(&format!("t: change {expected_num} [stdout:")),
            "unexpected compact commit line: {line}"
        );
        let replayed = repo.replay(&output, range(line));
        assert!(replayed.starts_with("commit "), "{line} -> {replayed}");
        assert!(
            replayed.contains(&format!("    body line for change {expected_num}\n")),
            "{line} -> {replayed}"
        );
    }
    let (from, to) = range(count);
    assert_eq!(to, raw.lines().count());
    // The older commits start at a commit header and replay byte for byte.
    let rest = repo.replay(&output, (from, to));
    assert!(rest.starts_with("commit "), "{rest}");
    let older = rest.lines().filter(|l| l.starts_with("commit ")).count();
    assert_eq!(shown_lines.len() + older, 200);
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

#[test]
fn a_decorated_log_preserves_refs_and_merge_notes_with_exact_replay() {
    let repo = Repo::with_commits(10);
    git(&repo.root, &["checkout", "-q", "-b", "topic"]);
    repo.file("topic.txt", "topic change\n");
    git(&repo.root, &["add", "topic.txt"]);
    git(
        &repo.root,
        &[
            "commit",
            "-q",
            "-m",
            "topic feature\n\nDetailed topic explanation",
        ],
    );
    git(&repo.root, &["checkout", "-q", "main"]);
    repo.file("main.txt", "main change\n");
    git(&repo.root, &["add", "main.txt"]);
    git(&repo.root, &["commit", "-q", "-m", "main progress"]);
    let merge = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args([
            "merge",
            "--no-ff",
            "-q",
            "-m",
            "Merge topic into main",
            "topic",
        ])
        .current_dir(&repo.root)
        .output()
        .unwrap();
    assert!(merge.status.success());
    git(&repo.root, &["tag", "v1.0"]);

    let raw = repo.raw(&["log", "--decorate"]);
    let output = repo
        .command(&[
            "run",
            "--fresh",
            "--max-bytes",
            "1024",
            "--",
            "git",
            "log",
            "--decorate",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let shown = text(&output.stdout);
    assert!(shown.len() < raw.len(), "{shown}");
    let first = shown.lines().next().unwrap();
    assert!(first.contains("HEAD -> main"), "{first}");
    assert!(first.contains("tag: v1.0"), "{first}");
    assert!(first.contains("(Merge: "), "{first}");
    let replayed = repo.replay(&output, range(first));
    assert!(replayed.starts_with("commit "), "{replayed}");
    assert!(replayed.contains("Merge: "), "{replayed}");
}

#[test]
fn large_branch_tag_and_stash_outputs_compact_and_replay_accurately() {
    let repo = Repo::with_commits(3);
    // Create a local bare remote with 40 branches and tracking branches (ahead + gone).
    let remote = repo.root.parent().unwrap().join("origin.git");
    git(
        repo.root.parent().unwrap(),
        &["init", "--bare", "-q", "origin.git"],
    );
    git(
        &repo.root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repo.root, &["push", "-q", "-u", "origin", "main"]);
    // Create a branch tracked to origin, then delete it on origin so it becomes `[gone]`.
    git(&repo.root, &["checkout", "-q", "-b", "stale-gone"]);
    git(&repo.root, &["push", "-q", "-u", "origin", "stale-gone"]);
    git(&repo.root, &["checkout", "-q", "main"]);
    git(
        &repo.root,
        &["push", "-q", "origin", "--delete", "stale-gone"],
    );
    // Advance main locally so it is `[ahead 1]`.
    repo.file("ahead.txt", "ahead\n");
    git(&repo.root, &["add", "ahead.txt"]);
    git(&repo.root, &["commit", "-q", "-m", "local ahead commit"]);
    // Add 35 local + 35 remote branches directly via update-ref for speed.
    let head = repo.raw(&["rev-parse", "HEAD"]);
    let head = head.trim();
    for i in 0..35 {
        git(
            &repo.root,
            &["update-ref", &format!("refs/heads/feat/local-{i:02}"), head],
        );
        git(
            &repo.root,
            &[
                "update-ref",
                &format!("refs/remotes/origin/feat/remote-{i:02}"),
                head,
            ],
        );
    }

    let branch_out = repo
        .command(&[
            "run",
            "--fresh",
            "--max-bytes",
            "1024",
            "--",
            "git",
            "branch",
            "-avv",
        ])
        .output()
        .unwrap();
    assert_eq!(branch_out.status.code(), Some(0));
    let branch_shown = text(&branch_out.stdout);
    assert!(branch_shown.len() <= 1024, "{branch_shown}");
    let pos_main = branch_shown.find("* main ").expect(&branch_shown);
    let pos_gone = branch_shown.find("stale-gone ").expect(&branch_shown);
    let pos_remote = branch_shown.find("remotes/origin/ (").expect(&branch_shown);
    assert!(
        pos_main < pos_gone && pos_gone < pos_remote,
        "{branch_shown}"
    );
    let remote_line = branch_shown
        .lines()
        .find(|l| l.starts_with("remotes/origin/ ("))
        .unwrap();
    let replayed = repo.replay(&branch_out, range(remote_line));
    assert!(
        replayed.contains("remotes/origin/feat/remote-00"),
        "{replayed}"
    );

    // Tags
    for i in 0..120 {
        git(
            &repo.root,
            &["update-ref", &format!("refs/tags/v0.{i:03}.0"), head],
        );
    }
    let tag_out = repo
        .command(&["run", "--fresh", "--max-bytes", "600", "--", "git", "tag"])
        .output()
        .unwrap();
    assert_eq!(tag_out.status.code(), Some(0));
    let tag_shown = text(&tag_out.stdout);
    assert!(tag_shown.len() <= 600, "{tag_shown}");
    has(&tag_shown, "git tag: 120 tags\n");
    has(&tag_shown, "v0.000.0\n");
    has(&tag_shown, "v0.119.0\n");
    let fold_line = tag_shown
        .lines()
        .find(|l| l.contains("more tags [stdout:"))
        .unwrap();
    let tag_replayed = repo.replay(&tag_out, range(fold_line));
    assert!(tag_replayed.contains("v0.010.0"), "{tag_replayed}");

    // Stashes
    for i in 0..15 {
        repo.file("ahead.txt", &format!("stash change {i}\n"));
        git(
            &repo.root,
            &[
                "stash",
                "push",
                "-q",
                "-m",
                &format!("work in progress step {i}"),
            ],
        );
    }
    let stash_out = repo
        .command(&[
            "run",
            "--fresh",
            "--max-bytes",
            "600",
            "--",
            "git",
            "stash",
            "list",
        ])
        .output()
        .unwrap();
    assert_eq!(stash_out.status.code(), Some(0));
    let stash_shown = text(&stash_out.stdout);
    assert!(stash_shown.len() <= 600, "{stash_shown}");
    assert!(stash_shown.starts_with("stash@{0}: "), "{stash_shown}");
    let stash_fold = stash_shown.lines().last().unwrap();
    assert!(
        stash_fold.contains("older stashes [stdout:"),
        "{stash_fold}"
    );
    let stash_replayed = repo.replay(&stash_out, range(stash_fold));
    assert!(stash_replayed.contains("stash@{14}: "), "{stash_replayed}");
}
