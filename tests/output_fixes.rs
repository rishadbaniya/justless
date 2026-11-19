//! Previews of odd and adversarial output: a search heading with no lines, lockfile
//! sources, JSON tables, the floor around failures and patches, and file reads. The
//! exact output stays one `justless log` away, and the command's exit status and
//! output always reach the caller.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use tempfile::TempDir;

/// A workspace with its own home, client and state directories, and a `bin`
/// directory searched first for stub programs.
struct Sandbox {
    dir: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        for sub in ["root", "bin", "home", "claude", "codex", "state"] {
            fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        fs::write(dir.path().join("root/.justless.toml"), "").unwrap();
        Self { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("root").canonicalize().unwrap()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// A shell script named `name` on the front of `PATH`.
    #[cfg(unix)]
    fn stub(&self, name: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = self.path("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{script}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![self.path("bin")];
        dirs.extend(std::env::split_paths(&path));
        command
            .current_dir(self.root())
            .env("PATH", std::env::join_paths(dirs).unwrap())
            .env("HOME", self.path("home"))
            .env("CLAUDE_CONFIG_DIR", self.path("claude"))
            .env("CODEX_HOME", self.path("codex"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env_remove("CLAUDE_PROJECT_DIR");
        command
    }

    fn justless(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_justless"))
            .args(args)
            .output()
            .unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(unix)]
#[test]
fn a_search_heading_without_lines_keeps_the_output_and_status() {
    let sandbox = Sandbox::new();
    let root = sandbox.root();
    fs::create_dir_all(root.join("adv")).unwrap();
    fs::write(root.join("adv/a"), "needle\n").unwrap();
    fs::write(root.join("adv/a-1"), "needle\nafter\n").unwrap();
    let printed = "adv/a\n\nadv/a-1\n1:needle\n2-after\n";
    sandbox.stub(
        "rg",
        &format!("printf '{}'\nexit 3\n", printed.replace('\n', "\\n")),
    );
    let output = sandbox.justless(&["run", "--", "rg", "-n", "-C1", "--heading", "needle", "adv"]);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert_eq!(text(&output.stdout), printed, "{output:?}");
}

/// A Cargo.lock of version 1.0.0 packages, each `(name, source, checksum)`.
fn lockfile(packages: &[(String, &str, String)]) -> String {
    let mut text = String::from("version = 4\n");
    for (name, source, checksum) in packages {
        text.push_str(&format!(
            "\n[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\nsource = \"{source}\"\n"
        ));
        if !checksum.is_empty() {
            text.push_str(&format!("checksum = \"{checksum}\"\n"));
        }
    }
    text
}

#[cfg(unix)]
#[test]
fn lockfile_flags_name_every_source_within_a_quarter_of_the_budget() {
    let sandbox = Sandbox::new();
    let stub = sandbox.path("vcs");
    fs::create_dir_all(stub.join("blobs")).unwrap();
    // The VCS stub serves two lockfile blobs, records the environment each blob read
    // runs with, and prints a patch between them for anything else.
    sandbox.stub(
        "git",
        &format!(
            "case \"$1\" in\n  cat-file) echo \"lazy=$GIT_NO_LAZY_FETCH\" >> {stub}/calls; cat {stub}/blobs/$3 ;;\n  *) cat {stub}/out ;;\nesac\n",
            stub = stub.display()
        ),
    );
    fs::write(
        stub.join("out"),
        "diff --git a/Cargo.lock b/Cargo.lock\nindex 1111111..2222222 100644\n\
         --- a/Cargo.lock\n+++ b/Cargo.lock\n@@ -1,2 +1,2 @@\n-old\n+new\n",
    )
    .unwrap();
    let registry = "registry+https://github.com/rust-lang/crates.io-index";
    let (old_fork, new_fork) = (
        "git+https://example.invalid/foo?branch=main",
        "git+https://example.invalid/evil?branch=main",
    );
    let package =
        |name: &str, source, checksum: &str| (name.to_owned(), source, checksum.to_owned());
    // One of two sources of foo 1.0.0 swapped for another repository.
    let dupe = (
        lockfile(&[package("foo", old_fork, ""), package("foo", registry, "aa")]),
        lockfile(&[package("foo", new_fork, ""), package("foo", registry, "aa")]),
    );
    // Every package rebuilt with a new checksum, as by a mirror.
    let rebuilt = |seed: usize| {
        let packages: Vec<_> = (0..120)
            .map(|i| {
                package(
                    &format!("pkg{i:03}"),
                    registry,
                    &format!("{:064x}", i + seed),
                )
            })
            .collect();
        lockfile(&packages)
    };
    let flag = format!(
        "! Cargo.lock: source changed on existing version foo 1.0.0: {old_fork} -> {new_fork}\n"
    );
    let more = "! +115 more source or checksum change(s)\n";
    for ((old, new), expected) in [(dupe, flag.as_str()), ((rebuilt(0), rebuilt(1000)), more)] {
        fs::write(stub.join("blobs/1111111"), old).unwrap();
        fs::write(stub.join("blobs/2222222"), new).unwrap();
        // The second run is unchanged, and its notice keeps the flags.
        for _ in 0..2 {
            let output = sandbox.justless(&["run", "--max-bytes", "4096", "--", "git", "diff"]);
            let shown = text(&output.stdout);
            assert!(output.status.success(), "{output:?}");
            assert!(shown.len() <= 4096, "{} bytes: {shown}", shown.len());
            assert!(shown.contains(expected), "{shown}");
        }
    }
    // No blob read may fetch from a remote.
    let calls = fs::read_to_string(stub.join("calls")).unwrap();
    assert!(
        !calls.is_empty() && calls.lines().all(|line| line == "lazy=1"),
        "{calls}"
    );
}

#[cfg(unix)]
#[test]
fn json_that_fits_takes_a_view_only_when_it_saves_more_than_a_footer() {
    let sandbox = Sandbox::new();
    let root = sandbox.root();
    sandbox.stub("emit", "cat \"$1\"\n");
    let members = |indent: usize| {
        let pad = " ".repeat(indent);
        let lines: Vec<String> = (0..16)
            .map(|i| format!("{pad}\"key{i:02}\": \"{}\"", "v".repeat(30)))
            .collect();
        format!("{{\n{}\n}}\n", lines.join(",\n"))
    };
    // The compact view saves 40 bytes of the first and 104 of the second.
    for (indent, view) in [(4, false), (8, true)] {
        let json = members(indent);
        fs::write(root.join("data.json"), &json).unwrap();
        let output = sandbox.justless(&[
            "run",
            "--hook",
            "--max-bytes",
            "4096",
            "--",
            "emit",
            "data.json",
        ]);
        assert!(output.status.success(), "{output:?}");
        let shown = text(&output.stdout);
        if view {
            assert!(
                shown.starts_with("[JSON without its whitespace; ")
                    && shown.len() + 64 < json.len(),
                "{shown}"
            );
        } else {
            assert_eq!(shown, json);
            assert_eq!(text(&output.stderr), "");
        }
    }
}
