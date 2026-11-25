//! End-to-end tests for tiered lossless diff/patch compaction, `gh pr diff` /
//! `glab mr diff`, merge commits (`diff --cc`), `--stat -p`, expanded lockfile
//! dependency diffs (6 new formats with supply-chain `!` flagging), and
//! generated/snapshot file folding.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

use crate::common::{BIN, executable, git, has, lacks, run_id, text, words};

struct Repo {
    _temp: TempDir,
    root: PathBuf,
    bin: PathBuf,
    home: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("repo");
        let bin = base.join("bin");
        let home = base.join("home");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&home).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join(".justless.toml"), "").unwrap();
        fs::write(
            root.join(".git/info/exclude"),
            ".justless/\n.justless.toml\n",
        )
        .unwrap();
        Self {
            _temp: temp,
            root,
            bin,
            home,
        }
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn commit(&self, msg: &str) {
        git(&self.root, &["add", "-A"]);
        git(&self.root, &["commit", "-qm", msg]);
    }

    fn stub(&self, name: &str, body: &str) {
        executable(&self.bin.join(name), &format!("#!/bin/sh\n{body}"));
    }

    fn run(&self, args: &[&str]) -> Output {
        let path_env = format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap());
        Command::new(BIN)
            .args(args)
            .current_dir(&self.root)
            .env("PATH", path_env)
            .env("HOME", &self.home)
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("JUSTLESS_DISABLE")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn replay_all(&self, id: &str) -> String {
        let out = self.run(&["log", id, "--stream", "stdout"]);
        assert!(out.status.success(), "{out:?}");
        text(&out.stdout)
    }

    fn replay_range(&self, id: &str, from: usize, to: usize) -> String {
        let (from, to) = (from.to_string(), to.to_string());
        let out = self.run(&[
            "log", id, "--stream", "stdout", "--from", &from, "--to", &to,
        ]);
        assert!(out.status.success(), "{out:?}");
        text(&out.stdout)
    }

    /// Verify every `[stdout:A-B]` pointer in `shown` replays the exact lines of `raw`.
    fn verify_pointers(&self, id: &str, shown: &str, raw: &str) {
        let raw_lines: Vec<&str> = raw.split_inclusive('\n').collect();
        let mut count = 0;
        for tag in shown.split("[stdout:").skip(1) {
            let range = tag.split(']').next().unwrap();
            let (from, to) = range.split_once('-').unwrap_or((range, range));
            let (from, to): (usize, usize) = (from.parse().unwrap(), to.parse().unwrap());
            let replayed = self.replay_range(id, from, to);
            assert_eq!(
                replayed,
                raw_lines[from - 1..to].concat(),
                "pointer [stdout:{from}-{to}] mismatch"
            );
            count += 1;
        }
        assert!(
            count > 0,
            "expected at least one [stdout:A-B] pointer in:\n{shown}"
        );
    }
}

fn make_source_file(lines: usize, prefix: &str) -> String {
    (1..=lines)
        .map(|i| format!("pub fn {prefix}_{i:03}(x: i64) -> i64 {{ x + {i} }}\n"))
        .collect()
}

#[test]
fn medium_diff_uses_compact_tier_preserving_every_changed_line_and_metadata() {
    let repo = Repo::new();
    // Build 6 source files so the raw git diff exceeds the 4096-byte budget (~6 KB),
    // while the compact diff (collapsed headers + 1-line context) fits well under 4096!
    for f in 0..6 {
        repo.write(
            &format!("src/module_{f}.rs"),
            &make_source_file(40, &format!("item_{f}")),
        );
    }
    repo.write("scripts/deploy.sh", "#!/bin/sh\necho hi\n");
    repo.write("src/old_name.rs", "pub fn stable() -> u32 { 7 }\n");
    repo.commit("base");

    for f in 0..6 {
        let old = make_source_file(40, &format!("item_{f}"));
        let new = old
            .replace(
                &format!("item_{f}_010(x: i64) -> i64 {{ x + 10 }}"),
                &format!("item_{f}_010(x: i64) -> i64 {{ x * 2 + 10 }}"),
            )
            .replace(
                &format!("item_{f}_030(x: i64) -> i64 {{ x + 30 }}"),
                &format!("item_{f}_030(x: i64) -> i64 {{ x * 3 + 30 }}"),
            );
        repo.write(&format!("src/module_{f}.rs"), &new);
    }
    // Pure rename and mode change to verify zero metadata loss.
    git(&repo.root, &["mv", "src/old_name.rs", "src/new_name.rs"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = repo.root.join("scripts/deploy.sh");
        let mut perms = fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&p, perms).unwrap();
    }
    git(&repo.root, &["add", "-A"]);

    let raw = git(&repo.root, &["diff", "--cached"]);
    assert!(
        raw.len() > 4096,
        "expected raw diff > 4096 bytes, got {}",
        raw.len()
    );

    let output = repo.run(&words("run --fresh -- git diff --cached"));
    assert_eq!(output.status.code(), Some(0));
    let shown = text(&output.stdout);
    let id = run_id(&output);

    assert!(
        shown.len() <= 4096,
        "compact diff exceeded 4096: {}",
        shown.len()
    );
    assert!(
        shown.len() * 10 < raw.len() * 8,
        "expected meaningful byte savings: {} vs {}",
        shown.len(),
        raw.len()
    );

    // Compact headers replace 4-line git headers, preserving mode/rename metadata verbatim.
    has(&shown, "=== src/module_0.rs (+2 -2) [stdout:");
    has(&shown, "=== scripts/deploy.sh [stdout:");
    has(&shown, "old mode 100644\nnew mode 100755\n");
    has(
        &shown,
        "rename src/old_name.rs -> src/new_name.rs (100%) [stdout:",
    );
    lacks(&shown, "\nindex ");
    lacks(&shown, "--- a/");
    lacks(&shown, "+++ b/");

    // 100% of every `+` and `-` code line from raw appears verbatim in compact diff!
    for f in 0..6 {
        has(
            &shown,
            &format!("-pub fn item_{f}_010(x: i64) -> i64 {{ x + 10 }}\n"),
        );
        has(
            &shown,
            &format!("+pub fn item_{f}_010(x: i64) -> i64 {{ x * 2 + 10 }}\n"),
        );
        has(
            &shown,
            &format!("-pub fn item_{f}_030(x: i64) -> i64 {{ x + 30 }}\n"),
        );
        has(
            &shown,
            &format!("+pub fn item_{f}_030(x: i64) -> i64 {{ x * 3 + 30 }}\n"),
        );
    }
    // 3-line context trimmed to 1 line: line 007/008 are dropped, line 009 stays.
    lacks(&shown, "item_0_007");
    lacks(&shown, "item_0_008");
    has(&shown, " pub fn item_0_009");

    assert_eq!(repo.replay_all(&id), raw);
    repo.verify_pointers(&id, &shown, &raw);

    // Exit status preserved (`git diff --exit-code` returns 1 when changes exist).
    let exit_out = repo.run(&words("run --fresh -- git diff --cached --exit-code"));
    assert_eq!(exit_out.status.code(), Some(1));
}

#[test]
fn large_multi_hunk_diff_falls_back_to_file_grouped_hunk_index_with_stats_and_exact_replay() {
    let repo = Repo::new();
    for f in 0..4 {
        repo.write(
            &format!("packages/core/src/service_{f}.rs"),
            &make_source_file(120, &format!("svc_{f}")),
        );
    }
    repo.commit("base");

    for f in 0..4 {
        let mut content = make_source_file(120, &format!("svc_{f}"));
        for target in [10, 45, 80, 110] {
            let needle = format!("pub fn svc_{f}_{target:03}(x: i64) -> i64 {{ x + {target} }}\n");
            let mut replacement = String::new();
            for k in 0..12 {
                replacement.push_str(&format!(
                    "pub fn svc_{f}_{target:03}_v{k}(x: i64) -> i64 {{ x + {target} + {k} }}\n"
                ));
            }
            content = content.replace(&needle, &replacement);
        }
        repo.write(&format!("packages/core/src/service_{f}.rs"), &content);
    }

    let raw = git(&repo.root, &["diff"]);
    assert!(raw.len() > 15_000, "raw len = {}", raw.len());

    let output = repo.run(&words("run --fresh -- git diff"));
    assert_eq!(output.status.code(), Some(0));
    let shown = text(&output.stdout);
    let id = run_id(&output);

    assert!(shown.starts_with("16 diff section(s)\n"), "{shown}");
    assert!(
        shown.len() < raw.len() / 5,
        "expected >80% savings: {} vs {}",
        shown.len(),
        raw.len()
    );
    for f in 0..4 {
        has(
            &shown,
            &format!("packages/core/src/service_{f}.rs (+48 -4, 4 hunks) [stdout:"),
        );
    }
    has(&shown, "  @@ ");
    has(&shown, " (+12 -1) [stdout:");

    assert_eq!(repo.replay_all(&id), raw);
    repo.verify_pointers(&id, &shown, &raw);
}

#[test]
fn generated_and_snapshot_files_fold_so_real_code_stays_in_compact_tier() {
    let repo = Repo::new();
    repo.write("src/parser.rs", &make_source_file(20, "parse"));
    repo.write("tests/snapshots/ui.snap", "header\n");
    repo.write("web/dist/bundle.min.js", "var a=1;\n");
    repo.write("proto/api.pb.go", "package api\n");
    repo.write("src/schema.generated.ts", "// gen\n");
    repo.write("proto/api_pb2.py", "# gen\n");
    repo.write("lib/user.g.dart", "// gen\n");
    repo.write("tests/small.snap", "a\nb\nc\n");
    repo.commit("base");

    let new_parser = make_source_file(20, "parse").replace(
        "parse_010(x: i64) -> i64 { x + 10 }",
        "parse_010(x: i64) -> i64 { x + 999 }",
    );
    repo.write("src/parser.rs", &new_parser);
    let big_body: String = (0..60)
        .map(|i| format!("generated row {i:03} payload\n"))
        .collect();
    for path in [
        "tests/snapshots/ui.snap",
        "web/dist/bundle.min.js",
        "proto/api.pb.go",
        "src/schema.generated.ts",
        "proto/api_pb2.py",
        "lib/user.g.dart",
    ] {
        repo.write(path, &big_body);
    }
    // Small snapshot (<= 25 lines) must NOT fold.
    repo.write("tests/small.snap", "a\nb_changed\nc\n");

    let raw = git(&repo.root, &["diff"]);
    assert!(raw.len() > 10_000, "raw len = {}", raw.len());

    let output = repo.run(&words("run --fresh -- git diff"));
    assert_eq!(output.status.code(), Some(0));
    let shown = text(&output.stdout);
    let id = run_id(&output);

    assert!(
        shown.len() < 1200,
        "expected huge reduction, got {} bytes:\n{shown}",
        shown.len()
    );
    for path in [
        "tests/snapshots/ui.snap",
        "web/dist/bundle.min.js",
        "proto/api.pb.go",
        "src/schema.generated.ts",
        "proto/api_pb2.py",
        "lib/user.g.dart",
    ] {
        has(&shown, &format!("=== {path} (generated, +60 -1) [stdout:"));
    }
    // Real code AND small snapshot stay in compact tier with 100% of `+`/`-` lines!
    has(&shown, "=== src/parser.rs (+1 -1) [stdout:");
    has(&shown, "-pub fn parse_010(x: i64) -> i64 { x + 10 }\n");
    has(&shown, "+pub fn parse_010(x: i64) -> i64 { x + 999 }\n");
    has(&shown, "=== tests/small.snap (+1 -1) [stdout:");
    has(&shown, "-b\n+b_changed\n");

    assert_eq!(repo.replay_all(&id), raw);
    repo.verify_pointers(&id, &shown, &raw);
}

#[test]
fn stat_with_patch_and_merge_commit_combined_diffs_are_summarized_losslessly() {
    let repo = Repo::new();
    repo.write(
        "src/lib.rs",
        "line1\nline2\nline3\nbase\nline4\nline5\nline6\n",
    );
    repo.commit("base");

    // Branch A modifies line 4.
    git(&repo.root, &["checkout", "-qb", "side"]);
    repo.write(
        "src/lib.rs",
        "line1\nline2\nline3\nfrom_side\nline4\nline5\nline6\n",
    );
    repo.commit("side change");

    // Main modifies line 4 differently, then merges with conflict resolution -> `diff --cc`!
    git(&repo.root, &["checkout", "-q", "main"]);
    repo.write(
        "src/lib.rs",
        "line1\nline2\nline3\nfrom_main\nline4\nline5\nline6\n",
    );
    repo.commit("main change");
    let merge = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "merge",
            "-q",
            "side",
        ])
        .current_dir(&repo.root)
        .output()
        .unwrap();
    assert!(!merge.status.success(), "expected merge conflict");
    repo.write(
        "src/lib.rs",
        "line1\nline2\nline3\nfrom_main\nfrom_side\nmerged_both\nline4\nline5\nline6\n",
    );
    git(&repo.root, &["add", "src/lib.rs"]);
    git(&repo.root, &["commit", "-qm", "merge side into main"]);

    // 1. `git show HEAD` produces `diff --cc src/lib.rs` with `@@@`.
    let raw_merge = git(&repo.root, &["show", "HEAD"]);
    assert!(raw_merge.contains("diff --cc src/lib.rs"), "{raw_merge}");
    let out_merge = repo.run(&words("run --fresh -- git show HEAD"));
    assert_eq!(out_merge.status.code(), Some(0));
    let shown_merge = text(&out_merge.stdout);
    let id_merge = run_id(&out_merge);
    has(&shown_merge, "1 commit(s), 1 diff section(s)\n");
    has(&shown_merge, "merge side into main");
    has(&shown_merge, "=== src/lib.rs (");
    has(&shown_merge, "@@@ ");
    has(&shown_merge, "++merged_both\n");
    assert_eq!(repo.replay_all(&id_merge), raw_merge);
    repo.verify_pointers(&id_merge, &shown_merge, &raw_merge);

    // 2. `git show --stat -p HEAD~1` and `git diff --stat -p HEAD~2..HEAD~1` are parsed cleanly.
    for cmd in [
        "run --fresh -- git show --stat -p HEAD~1",
        "run --fresh -- git log --stat -p -n 2",
        "run --fresh -- git diff --stat -p HEAD~2..HEAD~1",
    ] {
        let out = repo.run(&words(cmd));
        assert_eq!(out.status.code(), Some(0), "{cmd}");
        let shown = text(&out.stdout);
        has(&shown, "diff section(s)\n");
        has(&shown, "=== src/lib.rs (");
        has(&shown, "+from_main\n");
    }
}

#[test]
fn six_new_lockfile_formats_flag_substitutions_in_git_diff_and_git_log_p() {
    let repo = Repo::new();
    let base_files: &[(&str, &str)] = &[
        (
            "poetry.lock",
            "[[package]]\nname = \"httpx\"\nversion = \"0.27.0\"\nfiles = [{file = \"httpx.whl\", hash = \"sha256:111\"}]\n\n\
             [[package]]\nname = \"anyio\"\nversion = \"4.4.0\"\nfiles = [{file = \"anyio.whl\", hash = \"sha256:good\"}]\n\
             [metadata]\nlock-version = \"2.0\"\n",
        ),
        (
            "Gemfile.lock",
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    rack (3.0.10)\n    rails (7.1.0)\n      rack (>= 3.0)\n\n\
             CHECKSUMS\n  rack (3.0.10) sha256=goodrack\n\nBUNDLED WITH\n   2.5.10\n",
        ),
        (
            "composer.lock",
            r#"{"content-hash":"1","packages":[{"name":"symfony/console","version":"v6.4.8","dist":{"url":"https://dist/c1.zip","shasum":"s1"}},{"name":"symfony/string","version":"v6.4.8","dist":{"url":"https://dist/s1.zip","shasum":"goodsha"}}]}"#,
        ),
        (
            "flake.lock",
            r#"{"nodes":{"nixpkgs":{"locked":{"type":"github","owner":"NixOS","repo":"nixpkgs","rev":"111111111111aa","narHash":"sha256-old="}},"flake-utils":{"locked":{"type":"github","owner":"numtide","repo":"flake-utils","rev":"222222222222bb","narHash":"sha256-good="}},"root":{"inputs":{"nixpkgs":"nixpkgs"}}},"root":"root","version":7}"#,
        ),
        (
            "Package.resolved",
            r#"{"pins":[{"identity":"swift-log","location":"https://github.com/apple/swift-log.git","state":{"revision":"goodrev111","version":"1.5.4"}},{"identity":"swift-nio","location":"https://github.com/apple/swift-nio.git","state":{"revision":"nio111","version":"2.69.0"}}],"version":2}"#,
        ),
        (
            "mix.lock",
            "%{\n  \"jason\": {:hex, :jason, \"1.4.1\", \"goodhash\", [:mix], [], \"hexpm\"},\n  \"plug\": {:hex, :plug, \"1.16.0\", \"p1\", [:mix], [], \"hexpm\"},\n}\n",
        ),
    ];
    for (path, content) in base_files {
        repo.write(path, content);
    }
    repo.commit("base lockfiles");

    let updated_files: &[(&str, &str)] = &[
        (
            "poetry.lock",
            "[[package]]\nname = \"httpx\"\nversion = \"0.28.0\"\nfiles = [{file = \"httpx.whl\", hash = \"sha256:222\"}]\n\n\
             [[package]]\nname = \"anyio\"\nversion = \"4.4.0\"\nfiles = [{file = \"anyio.whl\", hash = \"sha256:evil\"}]\n\
             [metadata]\nlock-version = \"2.0\"\n",
        ),
        (
            "Gemfile.lock",
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    rack (3.0.10)\n    rails (7.2.0)\n      rack (>= 3.0)\n\n\
             CHECKSUMS\n  rack (3.0.10) sha256=evilrack\n\nBUNDLED WITH\n   2.5.10\n",
        ),
        (
            "composer.lock",
            r#"{"content-hash":"2","packages":[{"name":"symfony/console","version":"v6.4.12","dist":{"url":"https://dist/c2.zip","shasum":"s2"}},{"name":"symfony/string","version":"v6.4.8","dist":{"url":"https://dist/s1.zip","shasum":"evilsha"}}]}"#,
        ),
        (
            "flake.lock",
            r#"{"nodes":{"nixpkgs":{"locked":{"type":"github","owner":"NixOS","repo":"nixpkgs","rev":"333333333333cc","narHash":"sha256-new="}},"flake-utils":{"locked":{"type":"github","owner":"numtide","repo":"flake-utils","rev":"222222222222bb","narHash":"sha256-evil="}},"root":{"inputs":{"nixpkgs":"nixpkgs"}}},"root":"root","version":7}"#,
        ),
        (
            "Package.resolved",
            r#"{"pins":[{"identity":"swift-log","location":"https://github.com/apple/swift-log.git","state":{"revision":"evilrev999","version":"1.5.4"}},{"identity":"swift-nio","location":"https://github.com/apple/swift-nio.git","state":{"revision":"nio222","version":"2.70.0"}}],"version":2}"#,
        ),
        (
            "mix.lock",
            "%{\n  \"jason\": {:hex, :jason, \"1.4.1\", \"evilhash\", [:mix], [], \"hexpm\"},\n  \"plug\": {:hex, :plug, \"1.16.1\", \"p2\", [:mix], [], \"hexpm\"},\n}\n",
        ),
    ];
    for (path, content) in updated_files {
        repo.write(path, content);
    }
    repo.commit("bump lockfiles");

    // Verify BOTH `git diff HEAD~1` and `git log -p -n 1` detect all 6 lockfiles and all 6 supply-chain flags!
    for cmd in [
        "run --fresh -- git diff HEAD~1",
        "run --fresh -- git log -p -n 1",
    ] {
        let out = repo.run(&words(cmd));
        assert_eq!(out.status.code(), Some(0), "{cmd}");
        let shown = text(&out.stdout);
        for needle in [
            "! poetry.lock: checksum changed on existing version anyio 4.4.0",
            "! Gemfile.lock: checksum changed on existing version rack 3.0.10",
            "! composer.lock: checksum changed on existing version symfony/string v6.4.8",
            "! flake.lock: checksum changed on existing version flake-utils 222222222222",
            "! Package.resolved: checksum changed on existing version swift-log 1.5.4",
            "! mix.lock: checksum changed on existing version jason 1.4.1",
            "httpx 0.27.0 -> 0.28.0",
            "rails 7.1.0 -> 7.2.0",
            "symfony/console v6.4.8 -> v6.4.12",
            "nixpkgs 111111111111 -> 333333333333",
            "swift-nio 2.69.0 -> 2.70.0",
            "plug 1.16.0 -> 1.16.1",
        ] {
            has(&shown, needle);
        }
    }
}

#[test]
fn gh_pr_diff_and_glab_mr_diff_compact_remote_patches_and_flag_lockfile_substitutions() {
    let repo = Repo::new();
    // Remote patch with blob hashes NOT present in local `.git`, containing:
    // - a real code diff
    // - a Cargo.lock update + tampered checksum (parsed via hunk fallback!)
    // - a large generated snapshot (folded to 1 line!)
    let mut snap = String::new();
    for i in 0..45 {
        snap.push_str(&format!("+snapshot line {i:02} with extra content\n"));
    }
    let remote_patch = format!(
        concat!(
            "diff --git a/src/auth.rs b/src/auth.rs\n",
            "index aaaaaaa..bbbbbbb 100644\n",
            "--- a/src/auth.rs\n",
            "+++ b/src/auth.rs\n",
            "@@ -10,7 +10,7 @@ pub fn check_token(tok: &str) -> bool {{\n",
            " ctx1\n",
            " ctx2\n",
            " ctx3\n",
            "-    tok.len() > 4\n",
            "+    tok.len() >= 32\n",
            " ctx4\n",
            " ctx5\n",
            " ctx6\n",
            "diff --git a/Cargo.lock b/Cargo.lock\n",
            "index ccccccc..ddddddd 100644\n",
            "--- a/Cargo.lock\n",
            "+++ b/Cargo.lock\n",
            "@@ -10,7 +10,7 @@\n",
            " [[package]]\n",
            " name = \"serde\"\n",
            "-version = \"1.0.200\"\n",
            "+version = \"1.0.210\"\n",
            " source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
            "-checksum = \"oldserde\"\n",
            "+checksum = \"newserde\"\n",
            "@@ -40,6 +40,6 @@\n",
            " [[package]]\n",
            " name = \"ring\"\n",
            " version = \"0.17.8\"\n",
            " source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
            "-checksum = \"goodring\"\n",
            "+checksum = \"tamperedring\"\n",
            "diff --git a/tests/auth.snap b/tests/auth.snap\n",
            "index eeeeeee..fffffff 100644\n",
            "--- a/tests/auth.snap\n",
            "+++ b/tests/auth.snap\n",
            "@@ -1,1 +1,46 @@\n",
            " title\n",
            "{}",
        ),
        snap
    );
    let patch_file = repo.root.join("../remote.patch");
    fs::write(&patch_file, &remote_patch).unwrap();
    let script = format!("cat '{}'\n", patch_file.display());
    repo.stub("gh", &script);
    repo.stub("glab", &script);

    for cmd in [
        "run --fresh -- gh pr diff 42",
        "run --fresh -- glab mr diff 17",
    ] {
        let out = repo.run(&words(cmd));
        assert_eq!(out.status.code(), Some(0), "{cmd}: {out:?}");
        let shown = text(&out.stdout);
        let id = run_id(&out);
        assert!(
            shown.len() < remote_patch.len() / 2,
            "{cmd}: expected >50% savings ({} vs {}):\n{shown}",
            shown.len(),
            remote_patch.len()
        );
        has(
            &shown,
            "! Cargo.lock: checksum changed on existing version ring 0.17.8: goodring -> tamperedring\n",
        );
        has(&shown, "updated: serde 1.0.200 -> 1.0.210\n");
        has(&shown, "=== src/auth.rs (+1 -1) [stdout:");
        has(&shown, "-    tok.len() > 4\n+    tok.len() >= 32\n");
        has(&shown, "=== tests/auth.snap (generated, +45 -0) [stdout:");
        assert_eq!(repo.replay_all(&id), remote_patch);
        repo.verify_pointers(&id, &shown, &remote_patch);
    }
}
