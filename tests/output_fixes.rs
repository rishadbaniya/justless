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
