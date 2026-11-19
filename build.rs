//! Embed the Git revision a binary is built from, so `doctor` can tell when the
//! `justless` that hooks run lags behind a checkout. A build outside Git, or
//! without Git installed, embeds an empty revision.
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    output.status.success().then(|| text.trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let dir = std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default();
    let dir = Path::new(&dir);
    let revision = git(dir, &["rev-parse", "HEAD"])
        .filter(|id| matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_default();
    if !revision.is_empty() {
        // Run again when HEAD moves: a checkout, a commit or a packed ref. The
        // reflog also changes when a commit writes a branch that was packed. A
        // path that does not exist would rerun this script on every build.
        let mut names = ["HEAD", "logs/HEAD", "packed-refs"]
            .map(String::from)
            .to_vec();
        names.extend(git(dir, &["symbolic-ref", "-q", "HEAD"]));
        for name in names {
            if let Some(path) = git(dir, &["rev-parse", "--git-path", &name]) {
                let path = dir.join(path);
                if path.is_file() {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
        }
    }
    println!("cargo:rustc-env=JUSTLESS_REVISION={revision}");
}
