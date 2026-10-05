//! `justless exec`: scripts over large data, with archived output, a time limit and
//! intent-ranked previews.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// A workspace root and its canonical path, which is what the binary reports.
fn workspace() -> (TempDir, PathBuf) {
    let directory = TempDir::new().unwrap();
    fs::write(directory.path().join(".justless.toml"), "").unwrap();
    let root = directory.path().canonicalize().unwrap();
    (directory, root)
}

fn justless(cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_justless"));
    command
        .current_dir(cwd)
        .env_remove("JUSTLESS_SESSION")
        .env_remove("JUSTLESS_FILE")
        .env_remove("NO_COLOR");
    command
}

fn cli(cwd: &Path, args: &[&str]) -> Output {
    justless(cwd).args(args).output().unwrap()
}

fn exec(cwd: &Path, args: &[&str]) -> Output {
    justless(cwd).arg("exec").args(args).output().unwrap()
}

fn exec_with_stdin(cwd: &Path, args: &[&str], input: &[u8]) -> Output {
    let mut child = justless(cwd)
        .arg("exec")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A script that never reads its stdin may exit before all of it is written.
    let _ = child.stdin.take().unwrap().write_all(input);
    child.wait_with_output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The run ID in the footer, as in `[justless run-… · exit 1]`.
fn run_id(output: &Output) -> String {
    let text = stderr(output);
    let start = text.find("[justless run-").expect("run footer") + "[justless ".len();
    text[start..].split([' ', ']']).next().unwrap().to_owned()
}

/// IDs of recorded runs.
fn runs(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root.join(".justless/runs")) else {
        return Vec::new();
    };
    entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("record.json").is_file())
        .map(|path| path.file_name().unwrap().to_str().unwrap().to_owned())
        .collect()
}

/// The one run recorded since `before`, which a verbatim success does not name.
fn new_run(root: &Path, before: &[String]) -> String {
    let added: Vec<_> = runs(root)
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect();
    assert_eq!(added.len(), 1, "{added:?}");
    added[0].clone()
}

fn run_directories(root: &Path) -> usize {
    fs::read_dir(root.join(".justless/runs")).map_or(0, |entries| entries.count())
}

fn have(program: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
}

/// Whether a process exists, polled for up to two seconds until it is gone. A zombie
/// waiting for its reaper counts as gone.
#[cfg(unix)]
fn alive(pid: &str) -> bool {
    let check = format!(
        "kill -0 {pid} 2>/dev/null || exit 1; s=$(ps -o stat= -p {pid} 2>/dev/null) && case \"$s\" in *Z*) exit 1;; esac; exit 0"
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let exists = Command::new("sh")
            .args(["-c", &check])
            .status()
            .unwrap()
            .success();
        if !exists || Instant::now() > deadline {
            return exists;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

const NUMBERED: &str = "i=1; while [ $i -le 3000 ]; do echo \"line $i ok\"; i=$((i + 1)); done";

#[test]
fn inline_sh_prints_only_script_output_and_archives_the_run() {
    let (_directory, root) = workspace();
    let output = exec(&root, &["--lang", "sh", "echo hi; echo oops >&2; exit 3"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_eq!(stdout(&output), "hi\n");
    let id = run_id(&output);
    assert_eq!(stderr(&output), format!("oops\n[justless {id} · exit 3]\n"));

    let replay = cli(&root, &["log", &id, "--stream", "stdout"]);
    assert_eq!(replay.stdout, b"hi\n");
    let replay = cli(&root, &["log", &id, "--stream", "stderr"]);
    assert_eq!(replay.stderr, b"oops\n");
    let history = stdout(&cli(&root, &["history"]));
    assert!(
        history.contains(&format!("{id} · exec sh · exit 3")),
        "{history}"
    );
    let stats: serde_json::Value =
        serde_json::from_slice(&cli(&root, &["stats", "--json"]).stdout).unwrap();
    let families: Vec<_> = stats["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["family"].as_str().unwrap())
        .collect();
    assert_eq!(families, ["exec sh"]);
}

#[test]
fn verbatim_success_adds_no_footer() {
    let (_directory, root) = workspace();
    let output = exec(&root, &["--lang", "sh", "echo ok"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "ok\n");
    assert_eq!(stderr(&output), "");
    let history = stdout(&cli(&root, &["history"]));
    assert!(
        history.contains(&format!("{} · exec sh · exit 0", new_run(&root, &[]))),
        "{history}"
    );
}

#[test]
fn clipped_output_gets_the_short_footer_and_keeps_the_full_log() {
    let (_directory, root) = workspace();
    let output = exec(&root, &["--lang", "sh", "--max-bytes", "1024", NUMBERED]);
    assert!(output.status.success());
    assert!(output.stdout.len() <= 1024, "{}", output.stdout.len());
    let footer = stderr(&output);
    let id = run_id(&output);
    assert!(
        footer.starts_with(&format!("[justless {id} · ")),
        "{footer}"
    );
    assert!(
        footer.ends_with(" shown]\n") && footer.contains(" of "),
        "{footer}"
    );
    let replay = cli(&root, &["log", &id, "--stream", "stdout"]);
    assert_eq!(
        String::from_utf8(replay.stdout).unwrap().lines().count(),
        3000
    );
}

#[test]
fn file_paths_reach_argv_and_env_in_order() {
    let (_directory, root) = workspace();
    fs::write(root.join("data.txt"), "a\n").unwrap();
    fs::write(root.join("other.txt"), "b\n").unwrap();
    let output = exec(
        &root,
        &[
            "--lang",
            "sh",
            "--file",
            "data.txt",
            "--file",
            "other.txt",
            "printf '%s\\n' \"$@\"; echo \"F=$JUSTLESS_FILE\"; echo \"NO_COLOR=$NO_COLOR\"",
            "--",
            "extra",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let data = root.join("data.txt");
    assert_eq!(
        stdout(&output),
        format!(
            "{}\n{}\nextra\nF={}\nNO_COLOR=1\n",
            data.display(),
            root.join("other.txt").display(),
            data.display()
        )
    );

    let output = justless(&root)
        .args(["exec", "--lang", "sh", "echo \"F=${JUSTLESS_FILE-unset}\""])
        .env("JUSTLESS_FILE", "stale")
        .env("NO_COLOR", "0")
        .output()
        .unwrap();
    assert_eq!(stdout(&output), "F=unset\n");
}

#[test]
fn script_file_takes_arguments_after_double_dash_and_runs_in_the_current_directory() {
    let (_directory, root) = workspace();
    fs::create_dir_all(root.join("tools/sub")).unwrap();
    fs::write(root.join("tools/tool.sh"), "printf '%s\\n' \"$@\"; pwd\n").unwrap();
    fs::write(root.join("tools/sub/data.txt"), "x\n").unwrap();
    let sub = root.join("tools/sub");
    let output = exec(
        &sub,
        &[
            "--script",
            "../tool.sh",
            "--file",
            "data.txt",
            "--",
            "--top",
            "20",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "{}\n--top\n20\n{}\n",
            sub.join("data.txt").display(),
            sub.display()
        )
    );
    let history = stdout(&cli(&root, &["history"]));
    assert!(history.contains("exec sh"), "{history}");
}

#[test]
fn missing_file_and_missing_code_fail_before_running() {
    let (_directory, root) = workspace();
    let output = exec(&root, &["--lang", "sh", "--file", "nope.json", "touch ran"]);
    assert_eq!(output.status.code(), Some(2));
    let message = stderr(&output);
    assert!(
        message.starts_with("justless: --file nope.json: "),
        "{message}"
    );
    assert!(!message.contains("[justless run-"), "{message}");
    assert_eq!(run_directories(&root), 0);
    assert!(!root.join("ran").exists());

    let output = exec(&root, &["--lang", "sh"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("as - to read it from stdin"),
        "{}",
        stderr(&output)
    );
    // Piped data is never executed as code without an explicit `-`.
    let output = exec_with_stdin(&root, &["--lang", "sh"], b"touch ran\n");
    assert_eq!(output.status.code(), Some(2));
    assert!(!root.join("ran").exists());

    let output = exec(&root, &["--lang", "sh", "--script", "missing.sh"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).starts_with("justless: --script missing.sh: "),
        "{}",
        stderr(&output)
    );
    assert_eq!(run_directories(&root), 0);
}

#[test]
fn code_naming_a_file_points_to_script() {
    let (_directory, root) = workspace();
    fs::write(root.join("x.py"), "print(1)\n").unwrap();
    let output = exec(&root, &["--lang", "python3", "x.py"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        "justless: x.py is a file; run it with --script x.py\n"
    );
    assert_eq!(run_directories(&root), 0);
}

#[test]
fn stdin_code_with_dash_and_limits() {
    let (_directory, root) = workspace();
    let output = exec_with_stdin(&root, &["--lang", "sh", "-"], b"echo from-stdin\n");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "from-stdin\n");

    let output = exec_with_stdin(&root, &["--lang", "sh", "-"], b"");
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("the script on stdin is empty"));

    let large = vec![b'#'; 1024 * 1024 + 1];
    let output = exec_with_stdin(&root, &["--lang", "sh", "-"], &large);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("exceeds 1 MiB; hand data to it with --file"));
    assert_eq!(runs(&root).len(), 1);
}

#[test]
fn language_comes_from_lang_then_shebang_then_extension() {
    let (_directory, root) = workspace();
    let output = exec(&root, &["#!/bin/sh\necho ok"]);
    assert_eq!(stdout(&output), "ok\n", "{}", stderr(&output));
    assert!(stdout(&cli(&root, &["history"])).contains("exec sh"));

    let output = exec(&root, &["#!/usr/bin/env Rscript\nprint(1)"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        stderr(&output),
        "justless: the #! line names Rscript; pass --lang\n"
    );
    let output = exec(&root, &["echo hi"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("name the language with --lang"),
        "{}",
        stderr(&output)
    );
    let output = exec(
        &root,
        &["--lang", "sh", "#!/usr/bin/env python3\necho sh-won"],
    );
    assert_eq!(stdout(&output), "sh-won\n");

    if have("python3") {
        let output = exec(
            &root,
            &["#!/usr/bin/env -S python3 -u\nimport sys\nprint(sys.version_info[0])"],
        );
        assert_eq!(stdout(&output), "3\n", "{}", stderr(&output));
        fs::create_dir(root.join("tools")).unwrap();
        fs::write(root.join("tools/x.py"), "print('from file')\n").unwrap();
        let before = runs(&root);
        let output = exec(&root, &["--script", "tools/x.py"]);
        assert_eq!(stdout(&output), "from file\n", "{}", stderr(&output));
        let history = stdout(&cli(&root, &["history"]));
        assert!(
            history.contains(&format!("{} · exec python3", new_run(&root, &before))),
            "{history}"
        );
    }
}

#[test]
fn piped_stdin_reaches_script_when_code_is_an_argument() {
    let (_directory, root) = workspace();
    let output = exec_with_stdin(&root, &["--lang", "sh", "wc -l"], b"a\nb\n");
    assert_eq!(stdout(&output).trim(), "2");
    let output = exec_with_stdin(&root, &["--lang", "sh", "-"], b"cat; echo done\n");
    assert_eq!(stdout(&output), "done\n");
}

#[test]
fn python_runs_from_run_directory_without_cwd_shadowing() {
    if !have("python3") {
        return;
    }
    let (_directory, root) = workspace();
    fs::write(root.join("json.py"), "raise SystemExit('shadowed')\n").unwrap();
    let code = "import json; print(json.dumps([1]))";
    let output = exec(&root, &["--lang", "python3", code]);
    assert_eq!(stdout(&output), "[1]\n", "{}", stderr(&output));
    let script = root
        .join(".justless/runs")
        .join(new_run(&root, &[]))
        .join("script.py");
    assert_eq!(fs::read(&script).unwrap(), code.as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn python_traceback_leads_to_the_saved_script_line() {
    if !have("python3") {
        return;
    }
    let (_directory, root) = workspace();
    let output = exec(
        &root,
        &[
            "--lang",
            "python3",
            "import json\nprint(json.load(open('missing.json')))",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let id = run_id(&output);
    let context = stdout(&cli(&root, &["context", "--run", &id]));
    assert!(
        context.contains(&format!(".justless/runs/{id}/script.py:2")),
        "{context}"
    );
    assert!(
        context.contains("2 | print(json.load(open('missing.json')))"),
        "{context}"
    );
    assert!(!context.contains("Source changed"), "{context}");
}

#[test]
fn node_require_works_under_type_module_project() {
    if !have("node") {
        return;
    }
    let (_directory, root) = workspace();
    fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
    let output = exec(
        &root,
        &[
            "--lang",
            "node",
            "const fs = require('fs'); console.log(typeof fs.readFileSync)",
        ],
    );
    assert_eq!(stdout(&output), "function\n", "{}", stderr(&output));
    let version = Command::new("node").arg("--version").output().unwrap();
    let version = String::from_utf8_lossy(&version.stdout);
    let mut parts = version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    if (major, minor) >= (22, 7) {
        let output = exec(
            &root,
            &[
                "--lang",
                "js",
                "import fs from 'fs'; console.log(typeof fs.readFileSync)",
            ],
        );
        assert_eq!(stdout(&output), "function\n", "{}", stderr(&output));
    }
}

#[test]
fn ruby_and_perl_receive_file_argument() {
    let (_directory, root) = workspace();
    fs::write(root.join("data.txt"), "x\n").unwrap();
    let expected = format!("{}\n", root.join("data.txt").display());
    for (lang, code) in [("ruby", "puts ARGV[0]"), ("perl", "print \"$ARGV[0]\\n\"")] {
        if !have(lang) {
            continue;
        }
        let output = exec(&root, &["--lang", lang, "--file", "data.txt", code]);
        assert_eq!(stdout(&output), expected, "{lang}: {}", stderr(&output));
    }
}

/// Deno, Bun and a `python`-only system, through stub interpreters that print argv.
#[cfg(unix)]
#[test]
fn interpreter_table_builds_each_command_line() {
    use std::os::unix::fs::PermissionsExt;
    let (_directory, root) = workspace();
    fs::write(root.join("data.txt"), "x\n").unwrap();
    let stubs = TempDir::new().unwrap();
    for name in ["deno", "bun", "python"] {
        let stub = stubs.path().join(name);
        fs::write(&stub, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let data = root.join("data.txt").display().to_string();
    let deno_line = "#!/usr/bin/env -S deno run --allow-read=./data\ncode";
    for (lang, code, prefix, extension) in [
        ("deno", "code", &["run", "--allow-all", "--quiet"][..], "ts"),
        ("bun", "code", &["run"], "ts"),
        ("python3", "code", &[], "py"),
        // A narrower grant on the `#!` line replaces the default `--allow-all`.
        ("deno", deno_line, &["run", "--allow-read=./data"], "ts"),
    ] {
        let output = justless(&root)
            .args([
                "exec", "--lang", lang, "--file", "data.txt", code, "--", "arg",
            ])
            .env("PATH", stubs.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{lang}: {}", stderr(&output));
        let text = stdout(&output);
        let argv: Vec<_> = text.lines().collect();
        // The saved script is named relative to the working directory.
        let script = format!(
            ".justless/runs/{}/script.{extension}",
            argv[prefix.len()].rsplit('/').nth(1).unwrap()
        );
        let mut expected: Vec<&str> = prefix.to_vec();
        expected.extend([script.as_str(), data.as_str(), "arg"]);
        assert_eq!(argv, expected, "{lang}");
        assert_eq!(fs::read(root.join(&script)).unwrap(), code.as_bytes());
    }
}

/// Options on the `#!` line reach the interpreter, so `-e` and `-u` keep their effect
/// on the exit status.
#[cfg(unix)]
#[test]
fn shebang_options_keep_the_exit_status() {
    use std::os::unix::fs::PermissionsExt;
    let (_directory, root) = workspace();
    fs::create_dir(root.join("tools")).unwrap();
    let check = root.join("tools/check.sh");
    fs::write(&check, "#!/bin/sh -e\nfalse\necho continued past failure\n").unwrap();
    fs::set_permissions(&check, fs::Permissions::from_mode(0o755)).unwrap();
    let direct = Command::new(&check).current_dir(&root).output().unwrap();
    let output = exec(&root, &["--script", "tools/check.sh"]);
    assert_eq!(output.status.code(), direct.status.code());
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");

    let inline = exec(&root, &["#!/bin/sh -e\nfalse\necho continued"]);
    assert_eq!(inline.status.code(), Some(1), "{}", stderr(&inline));
    assert_eq!(stdout(&inline), "");

    if have("bash") {
        let output = exec(
            &root,
            &["#!/usr/bin/env -S bash -eu\necho \"$UNSET_VAR_X\"\necho continued"],
        );
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert_eq!(stdout(&output), "");
        assert!(
            stderr(&output).contains("UNSET_VAR_X: unbound variable"),
            "{}",
            stderr(&output)
        );
    }
    // Variables set by `env` on the `#!` line reach the script.
    let output = exec(
        &root,
        &["#!/usr/bin/env GREETING=hi sh\necho \"$GREETING\""],
    );
    assert_eq!(stdout(&output), "hi\n", "{}", stderr(&output));
    // A `#!` line for another language than --lang contributes no options.
    let output = exec(
        &root,
        &["--lang", "sh", "#!/usr/bin/env -S python3 -u\necho sh-ran"],
    );
    assert_eq!(stdout(&output), "sh-ran\n", "{}", stderr(&output));
}

#[test]
fn missing_interpreter_is_a_clear_error() {
    let (_directory, root) = workspace();
    let empty = TempDir::new().unwrap();
    for (lang, message) in [
        ("ruby", "justless: ruby is not on PATH\n"),
        ("deno", "justless: deno is not on PATH\n"),
        (
            "python3",
            "justless: python3 is not on PATH (tried python3, python)\n",
        ),
    ] {
        let output = justless(&root)
            .args(["exec", "--lang", lang, "1"])
            .env("PATH", empty.path())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{lang}");
        assert_eq!(stderr(&output), message);
    }
    assert_eq!(run_directories(&root), 0);
}

#[test]
fn max_bytes_and_timeout_are_validated() {
    let (_directory, root) = workspace();
    let output = exec(&root, &["--lang", "sh", "--max-bytes", "100", "touch ran"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("at least 512"),
        "{}",
        stderr(&output)
    );
    let output = exec(&root, &["--lang", "sh", "--timeout", "abc", "touch ran"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("--timeout expects a whole number of seconds"),
        "{}",
        stderr(&output)
    );
    assert!(!root.join("ran").exists());
}

#[test]
fn identical_runs_always_show_their_output() {
    let (_directory, root) = workspace();
    let script = "i=1; while [ $i -le 40 ]; do echo \"row $i\"; i=$((i + 1)); done";
    let first = exec(&root, &["--lang", "sh", script]);
    let second = exec(&root, &["--lang", "sh", script]);
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(stdout(&second).lines().count(), 40);
    assert!(!stdout(&second).contains("Output unchanged since"));
    assert_eq!(stderr(&second), "");
}

#[test]
fn a_later_success_supersedes_failed_attempts_in_pack() {
    let (_directory, root) = workspace();
    fs::write(root.join("data.txt"), "x\n").unwrap();
    let failed = exec(
        &root,
        &["--lang", "sh", "--file", "data.txt", "echo boom; exit 1"],
    );
    let id = run_id(&failed);
    let pack = stdout(&cli(&root, &["pack", "why does it fail"]));
    assert!(pack.contains(&id), "{pack}");
    exec(&root, &["--lang", "sh", "--file", "data.txt", "echo ok"]);
    let pack = stdout(&cli(&root, &["pack", "why does it fail"]));
    assert!(!pack.contains(&id), "{pack}");
}

#[cfg(unix)]
#[test]
fn timeout_stops_the_group_keeps_partial_output_and_exits_124() {
    let (_directory, root) = workspace();
    let started = Instant::now();
    let output = exec(
        &root,
        &[
            "--lang",
            "sh",
            "--timeout",
            "1",
            "echo started; sleep 30 & echo $! > bg.pid; wait",
        ],
    );
    assert!(started.elapsed() < Duration::from_secs(6));
    assert_eq!(output.status.code(), Some(124));
    assert_eq!(stdout(&output), "started\n");
    let message = stderr(&output);
    assert!(message.contains(" · exit 124]\n"), "{message}");
    assert!(
        message.ends_with("[justless] stopped at --timeout 1s\n"),
        "{message}"
    );
    let pid = fs::read_to_string(root.join("bg.pid")).unwrap();
    assert!(!alive(pid.trim()), "background sleep survived");
    let replay = cli(&root, &["log", &run_id(&output), "--stream", "stdout"]);
    assert_eq!(replay.stdout, b"started\n");
}

#[cfg(unix)]
#[test]
fn timeout_interrupt_shows_where_python_stopped() {
    if !have("python3") {
        return;
    }
    let (_directory, root) = workspace();
    let output = exec(
        &root,
        &[
            "--lang",
            "python3",
            "--timeout",
            "1",
            "import time\nprint('scanning')\nwhile True: time.sleep(0.1)",
        ],
    );
    assert_eq!(output.status.code(), Some(124));
    assert_eq!(stdout(&output), "scanning\n");
    let message = stderr(&output);
    assert!(message.contains("script.py\", line 3"), "{message}");
    assert!(message.contains("KeyboardInterrupt"), "{message}");
}

#[cfg(unix)]
#[test]
fn killing_justless_stops_the_script() {
    let (_directory, root) = workspace();
    let mut child = justless(&root)
        .args([
            "exec",
            "--lang",
            "sh",
            "--timeout",
            "0",
            "echo $$ > sh.pid; sleep 30 & echo $! > bg.pid; wait",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(root.join("bg.pid")).is_ok_and(|pid| pid.ends_with('\n')) {
        assert!(Instant::now() < deadline, "the script did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    for file in ["sh.pid", "bg.pid"] {
        let pid = fs::read_to_string(root.join(file)).unwrap();
        assert!(!alive(pid.trim()), "{file} survived");
    }
    assert_eq!(runs(&root).len(), 0);
    // The unrecorded run directory left behind does not disturb later commands.
    for args in [&["history"][..], &["stats"], &["resume"]] {
        let output = cli(&root, args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    }
    let output = exec(&root, &["--lang", "sh", "echo after"]);
    assert_eq!(stdout(&output), "after\n", "{}", stderr(&output));
}

#[cfg(unix)]
#[test]
fn background_children_do_not_outlive_a_normal_exit() {
    let (_directory, root) = workspace();
    let started = Instant::now();
    let output = exec(
        &root,
        &["--lang", "sh", "sleep 30 & echo $! > bg.pid; echo done"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    // The sleep gets two seconds to finish, then is stopped, and the notice names the
    // run although every byte was shown.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1900) && elapsed < Duration::from_secs(10),
        "{elapsed:?}"
    );
    assert_eq!(stdout(&output), "done\n");
    let id = new_run(&root, &[]);
    assert_eq!(
        stderr(&output),
        format!(
            "[justless {id}] stopped processes the script left running; output they had not written is missing\n"
        )
    );
    let pid = fs::read_to_string(root.join("bg.pid")).unwrap();
    assert!(!alive(pid.trim()), "background sleep survived");
}

/// Output that a process substitution writes after the shell exits is archived whole.
#[cfg(unix)]
#[test]
fn readers_the_script_leaves_running_finish_writing() {
    if !have("bash") || !have("seq") {
        return;
    }
    let (_directory, root) = workspace();
    let output = exec(
        &root,
        &["--lang", "bash", "seq 1 100000 > >(sort -rn | head -3)"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "100000\n99999\n99998\n");
    assert_eq!(stderr(&output), "");

    let before = runs(&root);
    let output = exec(&root, &["--lang", "bash", "seq 1 100000 > >(sort -rn)"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stderr(&output).contains("stopped"), "{}", stderr(&output));
    let id = new_run(&root, &before);
    let replay = cli(&root, &["log", &id, "--stream", "stdout"]);
    assert_eq!(stdout(&replay).lines().count(), 100000);

    // A reader that starts late still finishes within the two seconds.
    let before = runs(&root);
    let output = exec(
        &root,
        &[
            "--lang",
            "bash",
            "exec > >(sleep 0.5; while read -r l; do echo \"p $l\"; done); seq 1 2000",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stderr(&output).contains("stopped"), "{}", stderr(&output));
    let id = new_run(&root, &before);
    let replay = stdout(&cli(&root, &["log", &id, "--stream", "stdout"]));
    assert_eq!(replay.lines().count(), 2000);
    assert_eq!(replay.lines().last(), Some("p 2000"));
}

/// A script that ignores SIGINT is killed when the grace period ends.
#[cfg(unix)]
#[test]
fn timeout_kills_a_script_that_ignores_the_interrupt() {
    let (_directory, root) = workspace();
    let started = Instant::now();
    let output = exec(
        &root,
        &[
            "--lang",
            "sh",
            "--timeout",
            "1",
            "trap '' INT; echo waiting; sleep 30 & echo $! > bg.pid; wait",
        ],
    );
    let elapsed = started.elapsed();
    assert_eq!(output.status.code(), Some(124), "{}", stderr(&output));
    assert!(
        elapsed >= Duration::from_millis(2900) && elapsed < Duration::from_secs(15),
        "{elapsed:?}"
    );
    assert_eq!(stdout(&output), "waiting\n");
    assert!(
        stderr(&output).ends_with("[justless] stopped at --timeout 1s\n"),
        "{}",
        stderr(&output)
    );
    let pid = fs::read_to_string(root.join("bg.pid")).unwrap();
    assert!(!alive(pid.trim()), "background sleep survived");
}

/// Signals a script sends its own group do not disable the time limit.
#[cfg(unix)]
#[test]
fn timeout_holds_when_the_script_signals_its_group() {
    let (_directory, root) = workspace();
    let started = Instant::now();
    let output = exec(
        &root,
        &[
            "--lang",
            "sh",
            "--timeout",
            "1",
            "trap '' USR1; kill -USR1 0; echo signalled; sleep 30; echo finished",
        ],
    );
    assert_eq!(output.status.code(), Some(124), "{}", stderr(&output));
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(stdout(&output), "signalled\n");
}

/// An interpreter that moves to another process group is still stopped.
#[cfg(unix)]
#[test]
fn timeout_stops_an_interpreter_that_leaves_its_group() {
    if !have("python3") {
        return;
    }
    let (_directory, root) = workspace();
    let started = Instant::now();
    let output = exec(
        &root,
        &[
            "--lang",
            "python3",
            "--timeout",
            "1",
            "import os, signal, time\nsignal.signal(signal.SIGINT, signal.SIG_IGN)\nos.setpgid(0, os.getpgid(os.getppid()))\nprint('left', flush=True)\ntime.sleep(30)\nprint('finished')",
        ],
    );
    let elapsed = started.elapsed();
    assert_eq!(output.status.code(), Some(124), "{}", stderr(&output));
    assert!(elapsed < Duration::from_secs(15), "{elapsed:?}");
    assert_eq!(stdout(&output), "left\n");
}

/// Shells and Ruby print the saved script's path as given, relative to the directory
/// the script runs in.
#[test]
fn saved_script_path_is_relative_to_the_working_directory() {
    let (_directory, root) = workspace();
    fs::create_dir(root.join("src")).unwrap();
    let before = runs(&root);
    let output = exec(&root.join("src"), &["--lang", "sh", "echo \"$0\""]);
    let id = new_run(&root, &before);
    assert_eq!(
        stdout(&output),
        format!("../.justless/runs/{id}/script.sh\n"),
        "{}",
        stderr(&output)
    );
    if have("ruby") {
        let output = exec(&root.join("src"), &["--lang", "ruby", "raise 'boom'"]);
        let id = run_id(&output);
        assert!(
            stderr(&output).starts_with(&format!("../.justless/runs/{id}/script.rb:1:")),
            "{}",
            stderr(&output)
        );
        let context = stdout(&cli(&root, &["context", "--run", &id]));
        assert!(
            context.contains(&format!(".justless/runs/{id}/script.rb:1")),
            "{context}"
        );
    }
}

#[cfg(unix)]
#[test]
fn signal_death_maps_to_128_plus_n() {
    let (_directory, root) = workspace();
    let output = exec(&root, &["--lang", "sh", "kill -TERM $$"]);
    assert_eq!(output.status.code(), Some(143));
    assert!(
        stderr(&output).ends_with(" · exit 143]\n"),
        "{}",
        stderr(&output)
    );
}

/// 3000 lines of filler with matching lines at 1500 and 2900.
const TIMEOUTS: &str = "i=1; while [ $i -le 3000 ]; do if [ $i -eq 1500 ] || [ $i -eq 2900 ]; then echo \"payments timeout at $i\"; else echo \"line $i ok\"; fi; i=$((i + 1)); done";

fn markers(text: &str) -> Vec<usize> {
    text.lines()
        .filter_map(|line| line.strip_prefix("stdout:"))
        .map(|rest| rest.split(':').next().unwrap().parse().unwrap())
        .collect()
}

#[test]
fn intent_ranks_long_stdout_within_budget() {
    let (_directory, root) = workspace();
    let args = [
        "--lang",
        "sh",
        "--max-bytes",
        "1024",
        "--intent",
        "payments timeouts",
        TIMEOUTS,
    ];
    let output = exec(&root, &args);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.len() <= 1024, "{}", text.len());
    assert!(
        text.starts_with(
            "[focus] \"payments timeouts\": 2 of 3000 lines match every term, 0 match some\n"
        ),
        "{text}"
    );
    for expected in [
        "stdout:1500: payments timeout at 1500\n",
        "stdout:2900: payments timeout at 2900\n",
        "stdout:1: line 1 ok\n",
        "stdout:3000: line 3000 ok\n",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    let lines = markers(&text);
    assert!(lines.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
    assert!(
        text.ends_with("[focus] every matching line is shown\n"),
        "{text}"
    );
    let footer = stderr(&output);
    assert!(footer.contains(" shown]"), "{footer}");
    // The marker expands to the original line.
    let id = run_id(&output);
    let line = cli(
        &root,
        &[
            "log", &id, "--stream", "stdout", "--from", "1500", "--to", "1500",
        ],
    );
    assert_eq!(line.stdout, b"payments timeout at 1500\n");

    let again = exec(&root, &args);
    assert_eq!(again.stdout, output.stdout);
    let plain = exec(&root, &["--lang", "sh", "--max-bytes", "1024", TIMEOUTS]);
    assert!(!stdout(&plain).contains("[focus]"));
}

#[test]
fn intent_names_a_search_for_lines_left_out() {
    let (_directory, root) = workspace();
    let script = "i=1; while [ $i -le 400 ]; do echo \"$i: payments timeout request_id=$i\"; echo \"$i: orders timeout\"; i=$((i + 1)); done";
    let output = exec(
        &root,
        &["--lang", "sh", "--intent", "payments timeouts", script],
    );
    let text = stdout(&output);
    assert!(text.len() <= 4096, "{}", text.len());
    assert!(
        text.starts_with(
            "[focus] \"payments timeouts\": 400 of 800 lines match every term, 400 match some\n"
        ),
        "{text}"
    );
    let id = run_id(&output);
    let trailer = text.lines().last().unwrap();
    assert!(trailer.starts_with("[focus] "), "{trailer}");
    assert!(
        trailer.ends_with(&format!("more match some · justless search {id} payments")),
        "{trailer}"
    );
    // Lines that contain every term rank above those that contain one.
    for line in text.lines().filter(|line| line.starts_with("stdout:")) {
        let number: usize = markers(line)[0];
        assert!(number == 800 || line.contains("payments"), "{line}");
    }
    let search = cli(&root, &["search", &id, "payments", "--limit", "1"]);
    assert_eq!(
        stdout(&search),
        "stdout:1: 1: payments timeout request_id=1\n"
    );
}

#[test]
fn intent_fits_one_line_at_minimum_budget() {
    let (_directory, root) = workspace();
    let script = "x=$(printf '%5000s' '' | tr ' ' x); i=1; while [ $i -le 20 ]; do echo \"$x payments timeout $x\"; i=$((i + 1)); done";
    let intent = format!(
        "payments timeout {}",
        (0..20).map(|n| format!("extra{n} ")).collect::<String>()
    );
    let output = exec(
        &root,
        &[
            "--lang",
            "sh",
            "--max-bytes",
            "512",
            "--intent",
            &intent,
            script,
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.len() <= 512, "{}", text.len());
    let shown: Vec<_> = text
        .lines()
        .filter(|line| line.starts_with("stdout:"))
        .collect();
    assert!(!shown.is_empty(), "{text}");
    assert!(shown.iter().all(|line| line.contains('…')), "{text}");
    assert!(shown[0].contains("payments"), "{text}");
}

#[test]
fn intent_respects_utf8_and_long_lines() {
    let (_directory, root) = workspace();
    let script = "pad=$(printf '%2500s' '' | sed 's/ /é/g'); i=1; while [ $i -le 30 ]; do echo \"$pad café ✓ payments timeout $i $pad\"; echo \"filler $i\"; i=$((i + 1)); done";
    let output = exec(
        &root,
        &[
            "--lang",
            "sh",
            "--max-bytes",
            "1024",
            "--intent",
            "\"café ✓ payments\" timeout",
            script,
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text = String::from_utf8(output.stdout).expect("UTF-8 preview");
    assert!(text.len() <= 1024, "{}", text.len());
    assert!(text.contains("café ✓ payments timeout"), "{text}");
    for line in text.lines().filter(|line| line.starts_with("stdout:")) {
        let excerpt = line.split_once(": ").unwrap().1;
        assert!(excerpt.len() <= 240, "{}", excerpt.len());
    }
}

#[test]
fn intent_without_a_match_falls_back_to_marked_head_and_tail() {
    let (_directory, root) = workspace();
    let output = exec(
        &root,
        &[
            "--lang",
            "sh",
            "--max-bytes",
            "1024",
            "--intent",
            "zebra",
            NUMBERED,
        ],
    );
    let text = stdout(&output);
    assert!(text.len() <= 1024, "{}", text.len());
    assert!(
        text.starts_with("[focus] no line matches \"zebra\"; first and last of 3000 lines:\n"),
        "{text}"
    );
    assert!(text.contains("stdout:1: line 1 ok\n"), "{text}");
    assert!(text.ends_with("stdout:3000: line 3000 ok\n"), "{text}");
}

#[test]
fn intent_is_ignored_when_output_fits() {
    let (_directory, root) = workspace();
    let output = exec(
        &root,
        &["--lang", "sh", "--intent", "anything", "echo short"],
    );
    assert_eq!(stdout(&output), "short\n");
    assert_eq!(stderr(&output), "");
}

#[test]
fn intent_of_only_common_words_is_rejected_before_running() {
    let (_directory, root) = workspace();
    let output = exec(
        &root,
        &["--lang", "sh", "--intent", "what is the", "touch ran"],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("common words"),
        "{}",
        stderr(&output)
    );
    assert!(!root.join("ran").exists());
    assert_eq!(run_directories(&root), 0);
}

#[test]
fn intent_folds_word_endings_and_matches_quoted_phrases() {
    let (_directory, root) = workspace();
    let script = "i=1; while [ $i -le 300 ]; do echo \"filler line $i\"; i=$((i + 1)); done; echo 'parsed config'; echo 'parsing config'; echo 'parse config'; echo 'connection reset by peer'; echo 'Connection Reset'; echo 'connection was reset'; echo end";
    let output = exec(
        &root,
        &[
            "--lang",
            "sh",
            "--max-bytes",
            "1024",
            "--intent",
            "parse",
            script,
        ],
    );
    let text = stdout(&output);
    assert!(
        text.starts_with("[focus] \"parse\": 3 of 307 lines match\n"),
        "{text}"
    );
    for line in [301, 302, 303] {
        assert!(text.contains(&format!("stdout:{line}: ")), "{text}");
    }
    let output = exec(
        &root,
        &[
            "--lang",
            "sh",
            "--max-bytes",
            "1024",
            "--intent",
            "\"connection reset\"",
            script,
        ],
    );
    let text = stdout(&output);
    assert!(
        text.starts_with("[focus] \"\"connection reset\"\": 2 of 307 lines match\n"),
        "{text}"
    );
    assert!(
        text.contains("stdout:304: connection reset by peer\n"),
        "{text}"
    );
    assert!(text.contains("stdout:305: Connection Reset\n"), "{text}");
    assert!(!text.contains("stdout:306:"), "{text}");
}

#[test]
fn hooks_leave_exec_unwrapped() {
    let (_directory, root) = workspace();
    fs::create_dir(root.join("src")).unwrap();
    let hook = |handler: &str, input: serde_json::Value| -> serde_json::Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
            .args([handler, "before-command"])
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let bash = |command: &str| serde_json::json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":root,"session_id":"s1","tool_input":{"command":command}});
    let agy = |command: &str| serde_json::json!({"workspacePaths":[root],"toolCall":{"name":"run_command","args":{"CommandLine":command,"Cwd":root}}});

    // Positive controls: the same inputs do rewrite a supported command.
    for handler in ["hook-claude", "hook-codex"] {
        let output = hook(handler, bash("git status"));
        assert!(
            output["hookSpecificOutput"]["updatedInput"]["command"]
                .as_str()
                .unwrap()
                .ends_with("-- git status"),
            "{handler}: {output}"
        );
    }
    assert!(
        hook("hook-agy", agy("git status"))
            .get("overwrite")
            .is_some()
    );

    for command in [
        "justless exec --lang python3 - <<'PY'\nprint(1)\nPY",
        "justless exec --lang sh --file a.log 'grep -c ERROR \"$1\"'",
        "cd src && justless exec --lang sh 'ls'",
    ] {
        for handler in ["hook-claude", "hook-codex"] {
            assert_eq!(
                hook(handler, bash(command)),
                serde_json::json!({}),
                "{handler}: {command}"
            );
        }
        assert!(
            hook("hook-agy", agy(command)).get("overwrite").is_none(),
            "{command}"
        );
    }
    let mixed = "cargo test && justless exec --lang sh 'echo hi'";
    let claude = hook("hook-claude", bash(mixed));
    let rewritten = claude["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .unwrap();
    assert!(rewritten.starts_with("justless run --hook "), "{rewritten}");
    assert!(
        rewritten.ends_with("-- cargo test && justless exec --lang sh 'echo hi'"),
        "{rewritten}"
    );
    assert!(
        claude["hookSpecificOutput"]
            .get("permissionDecision")
            .is_none()
    );
    assert_eq!(hook("hook-codex", bash(mixed)), serde_json::json!({}));
    assert!(hook("hook-agy", agy(mixed)).get("overwrite").is_none());
}
