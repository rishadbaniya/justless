//! `justless exec`: scripts over large data, with archived output, a time limit and
//! intent-ranked previews.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
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

fn sh(cwd: &Path, script: &str) -> Output {
    exec(cwd, &["--lang", "sh", script])
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

#[track_caller]
fn contains(text: &str, needle: &str) {
    assert!(text.contains(needle), "{needle:?} not in:\n{text}");
}

#[track_caller]
fn starts(text: &str, prefix: &str) {
    assert!(
        text.starts_with(prefix),
        "{prefix:?} does not start:\n{text}"
    );
}

#[track_caller]
fn ends(text: &str, suffix: &str) {
    assert!(text.ends_with(suffix), "{suffix:?} does not end:\n{text}");
}

fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

fn history(root: &Path) -> String {
    stdout(&cli(root, &["history"]))
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

fn record(root: &Path, id: &str) -> Value {
    let path = root.join(".justless/runs").join(id).join("record.json");
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
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

/// Whether the process whose ID the script wrote to `file` is gone.
#[cfg(unix)]
fn stopped(root: &Path, file: &str) -> bool {
    !alive(fs::read_to_string(root.join(file)).unwrap().trim())
}

const NUMBERED: &str = "i=1; while [ $i -le 3000 ]; do echo \"line $i ok\"; i=$((i + 1)); done";

#[test]
fn inline_sh_prints_only_script_output_and_archives_the_run() {
    let (_directory, root) = workspace();
    let output = sh(&root, "echo hi; echo oops >&2; exit 3");
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_eq!(stdout(&output), "hi\n");
    // The agent footer, not the footer printed for a person.
    let id = run_id(&output);
    assert_eq!(stderr(&output), format!("oops\n[justless {id} · exit 3]\n"));

    let replay = cli(&root, &["log", &id, "--stream", "stdout"]);
    assert_eq!(replay.stdout, b"hi\n");
    let replay = cli(&root, &["log", &id, "--stream", "stderr"]);
    assert_eq!(replay.stderr, b"oops\n");
    contains(&history(&root), &format!("{id} · exec sh · exit 3"));
    let stats: Value = serde_json::from_slice(&cli(&root, &["stats", "--json"]).stdout).unwrap();
    let families: Vec<_> = stats["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["family"].as_str().unwrap())
        .collect();
    assert_eq!(families, ["exec sh"]);
    // Hooks did not route the run, so `doctor` and the status line do not count it.
    let record = record(&root, &id);
    assert_eq!(record["hook"], false, "{record}");
    assert_eq!(record["family"], "exec sh", "{record}");
}

#[test]
fn output_that_fits_is_shown_verbatim_without_a_footer() {
    let (_directory, root) = workspace();
    let output = sh(&root, "echo ok");
    assert!(output.status.success());
    assert_eq!(stdout(&output), "ok\n");
    assert_eq!(stderr(&output), "");
    let id = new_run(&root, &[]);
    contains(&history(&root), &format!("{id} · exec sh · exit 0"));
    // An intent is ignored when the output fits, and identical runs always show it.
    let rows = "i=1; while [ $i -le 40 ]; do echo \"row $i\"; i=$((i + 1)); done";
    let forty: String = (1..=40).map(|i| format!("row {i}\n")).collect();
    let short = ["--lang", "sh", "--intent", "anything", "echo short"];
    for (args, expected) in [
        (&short[..], "short\n"),
        (&["--lang", "sh", rows], &forty),
        (&["--lang", "sh", rows], &forty),
    ] {
        let output = exec(&root, args);
        assert_eq!(stdout(&output), expected, "{args:?}");
        assert_eq!(stderr(&output), "", "{args:?}");
    }
}

#[test]
fn clipped_output_gets_the_short_footer_and_keeps_the_full_log() {
    let (_directory, root) = workspace();
    let output = exec(&root, &["--lang", "sh", "--max-bytes", "1024", NUMBERED]);
    assert!(output.status.success());
    assert!(output.stdout.len() <= 1024, "{}", output.stdout.len());
    let footer = stderr(&output);
    let id = run_id(&output);
    starts(&footer, &format!("[justless {id} · "));
    ends(&footer, " shown]\n");
    contains(&footer, " of ");
    let replay = cli(&root, &["log", &id, "--stream", "stdout"]);
    let full = String::from_utf8(replay.stdout).unwrap();
    assert_eq!(full.lines().count(), 3000);

    // Whole lines from both ends, and a line naming the rest, whose command replays
    // exactly what is missing.
    let text = stdout(&output);
    let (head, rest) = text.split_once("[justless] stdout lines ").unwrap();
    let (marker, tail) = rest.split_once('\n').unwrap();
    starts(head, "line 1 ok\n");
    ends(head, " ok\n");
    ends(tail, "line 3000 ok\n");
    let command = marker.split_once(" · ").unwrap().1;
    let words: Vec<&str> = command.split(' ').collect();
    assert_eq!(&words[..5], ["justless", "log", &id, "--stream", "stdout"]);
    let missing = cli(&root, &words[1..]);
    assert_eq!(format!("{head}{}{tail}", stdout(&missing)), full);
    assert!(text.len() > 900, "{}", text.len());
}

#[test]
fn file_paths_reach_argv_and_env_in_order() {
    let (_directory, root) = workspace();
    fs::write(root.join("data.txt"), "a\n").unwrap();
    fs::write(root.join("other.txt"), "b\n").unwrap();
    let code = "printf '%s\\n' \"$@\"; echo \"F=$JUSTLESS_FILE\"; echo \"NO_COLOR=$NO_COLOR\"";
    let mut args = words("--lang sh --file data.txt --file other.txt");
    args.extend([code, "--", "extra"]);
    let output = exec(&root, &args);
    assert!(output.status.success(), "{}", stderr(&output));
    let (data, other) = (root.join("data.txt"), root.join("other.txt"));
    let (data, other) = (data.display(), other.display());
    let expected = format!("{data}\n{other}\nextra\nF={data}\nNO_COLOR=1\n");
    assert_eq!(stdout(&output), expected);

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
    let args = words("--script ../tool.sh --file data.txt -- --top 20");
    let output = exec(&sub, &args);
    assert!(output.status.success(), "{}", stderr(&output));
    let data = sub.join("data.txt");
    let expected = format!("{}\n--top\n20\n{}\n", data.display(), sub.display());
    assert_eq!(stdout(&output), expected);
    contains(&history(&root), "exec sh");
}

/// Bad arguments and code that cannot run exit 2 before anything runs or is archived.
/// An expected message that ends in a newline is the whole of stderr, one that starts
/// with `justless: ` is its start, and any other is part of it.
#[test]
fn usage_errors_exit_2_before_running() {
    let (_directory, root) = workspace();
    fs::write(root.join("x.py"), "print(1)\n").unwrap();
    let large = vec![b'#'; 1024 * 1024 + 1];
    type Case<'a> = (&'a [&'a str], Option<&'a [u8]>, &'a str);
    let cases: [Case; 12] = [
        (
            &["--lang", "sh", "--file", "nope.json", "touch ran"],
            None,
            "justless: --file nope.json: ",
        ),
        (&["--lang", "sh"], None, "as - to read it from stdin"),
        // Piped data is never executed as code without an explicit `-`.
        (&["--lang", "sh"], Some(b"touch ran\n"), ""),
        (
            &["--lang", "sh", "--script", "missing.sh"],
            None,
            "justless: --script missing.sh: ",
        ),
        (
            &["--lang", "python3", "x.py"],
            None,
            "justless: x.py is a file; run it with --script x.py\n",
        ),
        (
            &["--lang", "sh", "-"],
            Some(b""),
            "the script on stdin is empty",
        ),
        (
            &["--lang", "sh", "-"],
            Some(&large),
            "exceeds 1 MiB; hand data to it with --file",
        ),
        (
            &["#!/usr/bin/env Rscript\nprint(1)"],
            None,
            "justless: the #! line names Rscript; pass --lang\n",
        ),
        (&["echo hi"], None, "name the language with --lang"),
        (
            &["--lang", "sh", "--max-bytes", "100", "touch ran"],
            None,
            "at least 512",
        ),
        (
            &["--lang", "sh", "--timeout", "abc", "touch ran"],
            None,
            "--timeout expects a whole number of seconds",
        ),
        (
            &["--lang", "sh", "--intent", "what is the", "touch ran"],
            None,
            "common words",
        ),
    ];
    for (args, input, expected) in cases {
        eprintln!("case: {args:?}");
        let output = match input {
            Some(input) => exec_with_stdin(&root, args, input),
            None => exec(&root, args),
        };
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let message = stderr(&output);
        if expected.ends_with('\n') {
            assert_eq!(message, expected, "{args:?}");
        } else if expected.starts_with("justless: ") {
            starts(&message, expected);
        } else {
            contains(&message, expected);
        }
        assert!(!message.contains("[justless run-"), "{message}");
        assert!(!root.join("ran").exists(), "{args:?}");
    }
    assert_eq!(run_directories(&root), 0);
}

#[test]
fn stdin_holds_the_code_after_dash_and_reaches_a_script_given_as_an_argument() {
    let (_directory, root) = workspace();
    let output = exec_with_stdin(&root, &["--lang", "sh", "-"], b"echo from-stdin\n");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "from-stdin\n");
    let output = exec_with_stdin(&root, &["--lang", "sh", "wc -l"], b"a\nb\n");
    assert_eq!(stdout(&output).trim(), "2");
    let output = exec_with_stdin(&root, &["--lang", "sh", "-"], b"cat; echo done\n");
    assert_eq!(stdout(&output), "done\n");
}

#[test]
fn language_comes_from_lang_then_shebang_then_extension() {
    let (_directory, root) = workspace();
    let output = exec(&root, &["#!/bin/sh\necho ok"]);
    assert_eq!(stdout(&output), "ok\n", "{}", stderr(&output));
    contains(&history(&root), "exec sh");
    let output = sh(&root, "#!/usr/bin/env python3\necho sh-won");
    assert_eq!(stdout(&output), "sh-won\n");
    if have("python3") {
        let code = "#!/usr/bin/env -S python3 -u\nimport sys\nprint(sys.version_info[0])";
        let output = exec(&root, &[code]);
        assert_eq!(stdout(&output), "3\n", "{}", stderr(&output));
        fs::create_dir(root.join("tools")).unwrap();
        fs::write(root.join("tools/x.py"), "print('from file')\n").unwrap();
        let before = runs(&root);
        let output = exec(&root, &["--script", "tools/x.py"]);
        assert_eq!(stdout(&output), "from file\n", "{}", stderr(&output));
        let id = new_run(&root, &before);
        contains(&history(&root), &format!("{id} · exec python3"));
    }
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
    let id = new_run(&root, &[]);
    let script = root.join(".justless/runs").join(id).join("script.py");
    assert_eq!(fs::read(&script).unwrap(), code.as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn python_traceback_leads_to_the_saved_script_line() {
    if !have("python3") {
        return;
    }
    let (_directory, root) = workspace();
    let code = "import json\nprint(json.load(open('missing.json')))";
    let output = exec(&root, &["--lang", "python3", code]);
    assert_eq!(output.status.code(), Some(1));
    let id = run_id(&output);
    let context = stdout(&cli(&root, &["context", "--run", &id]));
    contains(&context, &format!(".justless/runs/{id}/script.py:2"));
    contains(&context, "2 | print(json.load(open('missing.json')))");
    assert!(!context.contains("Source changed"), "{context}");
}

#[test]
fn node_require_works_under_type_module_project() {
    if !have("node") {
        return;
    }
    let (_directory, root) = workspace();
    fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
    let code = "const fs = require('fs'); console.log(typeof fs.readFileSync)";
    let output = exec(&root, &["--lang", "node", code]);
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
        let code = "import fs from 'fs'; console.log(typeof fs.readFileSync)";
        let output = exec(&root, &["--lang", "js", code]);
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
        let id = argv[prefix.len()].rsplit('/').nth(1).unwrap();
        let script = format!(".justless/runs/{id}/script.{extension}");
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
    let inline = exec(&root, &["#!/bin/sh -e\nfalse\necho continued"]);
    for (case, output) in [("--script", &output), ("inline", &inline)] {
        eprintln!("case: {case}");
        assert_eq!(output.status.code(), Some(1), "{}", stderr(output));
        assert_eq!(stdout(output), "");
    }
    if have("bash") {
        let code = "#!/usr/bin/env -S bash -eu\necho \"$UNSET_VAR_X\"\necho continued";
        let output = exec(&root, &[code]);
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert_eq!(stdout(&output), "");
        contains(&stderr(&output), "UNSET_VAR_X: unbound variable");
    }
    // Variables set by `env` on the `#!` line reach the script.
    let output = exec(
        &root,
        &["#!/usr/bin/env GREETING=hi sh\necho \"$GREETING\""],
    );
    assert_eq!(stdout(&output), "hi\n", "{}", stderr(&output));
    // A `#!` line for another language than --lang contributes no options.
    let output = sh(&root, "#!/usr/bin/env -S python3 -u\necho sh-ran");
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
fn a_later_success_supersedes_failed_attempts_in_pack() {
    let (_directory, root) = workspace();
    fs::write(root.join("data.txt"), "x\n").unwrap();
    let run = |code| exec(&root, &["--lang", "sh", "--file", "data.txt", code]);
    let pack = || stdout(&cli(&root, &["pack", "why does it fail"]));
    let id = run_id(&run("echo boom; exit 1"));
    contains(&pack(), &id);
    run("echo ok");
    assert!(!pack().contains(&id), "{}", pack());
}

#[cfg(unix)]
#[test]
fn timeout_stops_the_group_keeps_partial_output_and_exits_124() {
    let (_directory, root) = workspace();
    let started = Instant::now();
    let code = "echo started; sleep 30 & echo $! > bg.pid; wait";
    let output = exec(&root, &["--lang", "sh", "--timeout", "1", code]);
    assert!(started.elapsed() < Duration::from_secs(6));
    assert_eq!(output.status.code(), Some(124));
    assert_eq!(stdout(&output), "started\n");
    let message = stderr(&output);
    contains(&message, " · exit 124]\n");
    ends(&message, "[justless] stopped at --timeout 1s\n");
    assert!(stopped(&root, "bg.pid"), "background sleep survived");
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
    let code = "import time\nprint('scanning')\nwhile True: time.sleep(0.1)";
    let output = exec(&root, &["--lang", "python3", "--timeout", "1", code]);
    assert_eq!(output.status.code(), Some(124));
    assert_eq!(stdout(&output), "scanning\n");
    let message = stderr(&output);
    contains(&message, "script.py\", line 3");
    contains(&message, "KeyboardInterrupt");
}

#[cfg(unix)]
#[test]
fn killing_justless_stops_the_script() {
    let (_directory, root) = workspace();
    let code = "echo $$ > sh.pid; sleep 30 & echo $! > bg.pid; wait";
    let mut child = justless(&root)
        .args(["exec", "--lang", "sh", "--timeout", "0", code])
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
        assert!(stopped(&root, file), "{file} survived");
    }
    // Like a `run` whose Justless was killed, the run stays listed as unfinished,
    // and it does not disturb later commands.
    let left = runs(&root);
    assert_eq!(left.len(), 1);
    let record = record(&root, &left[0]);
    assert_eq!(record["unfinished"], true, "{record}");
    for args in [&["history"][..], &["stats"], &["resume"]] {
        let output = cli(&root, args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    }
    contains(&history(&root), "exec sh · unfinished");
    let output = sh(&root, "echo after");
    assert_eq!(stdout(&output), "after\n", "{}", stderr(&output));
}

#[cfg(unix)]
#[test]
fn background_children_do_not_outlive_a_normal_exit() {
    let (_directory, root) = workspace();
    let started = Instant::now();
    let output = sh(&root, "sleep 30 & echo $! > bg.pid; echo done");
    assert!(output.status.success(), "{}", stderr(&output));
    // The sleep gets two seconds to finish, then is stopped, and the notice names the
    // run although every byte was shown.
    let elapsed = started.elapsed();
    let expected = Duration::from_millis(1900)..Duration::from_secs(10);
    assert!(expected.contains(&elapsed), "{elapsed:?}");
    assert_eq!(stdout(&output), "done\n");
    let id = new_run(&root, &[]);
    let notice =
        "stopped processes the script left running; output they had not written is missing";
    assert_eq!(stderr(&output), format!("[justless {id}] {notice}\n"));
    assert!(stopped(&root, "bg.pid"), "background sleep survived");
}

/// Output that a process substitution writes after the shell exits is archived whole.
#[cfg(unix)]
#[test]
fn readers_the_script_leaves_running_finish_writing() {
    if !have("bash") || !have("seq") {
        return;
    }
    let (_directory, root) = workspace();
    let bash = |code| exec(&root, &["--lang", "bash", code]);
    let output = bash("seq 1 100000 > >(sort -rn | head -3)");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "100000\n99999\n99998\n");
    assert_eq!(stderr(&output), "");

    // A reader that starts late still finishes within the two seconds.
    let late = "exec > >(sleep 0.2; while read -r l; do echo \"p $l\"; done); seq 1 2000";
    for (code, count, last) in [
        ("seq 1 100000 > >(sort -rn)", 100000, "1"),
        (late, 2000, "p 2000"),
    ] {
        eprintln!("case: {code}");
        let before = runs(&root);
        let output = bash(code);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(!stderr(&output).contains("stopped"), "{}", stderr(&output));
        let id = new_run(&root, &before);
        let replay = stdout(&cli(&root, &["log", &id, "--stream", "stdout"]));
        assert_eq!(replay.lines().count(), count, "{code}");
        assert_eq!(replay.lines().last(), Some(last), "{code}");
    }
}

/// A script that ignores SIGINT is killed when the grace period ends.
#[cfg(unix)]
#[test]
fn timeout_kills_a_script_that_ignores_the_interrupt() {
    let (_directory, root) = workspace();
    let started = Instant::now();
    let code = "trap '' INT; echo waiting; sleep 30 & echo $! > bg.pid; wait";
    let output = exec(&root, &["--lang", "sh", "--timeout", "1", code]);
    let elapsed = started.elapsed();
    assert_eq!(output.status.code(), Some(124), "{}", stderr(&output));
    let expected = Duration::from_millis(2900)..Duration::from_secs(15);
    assert!(expected.contains(&elapsed), "{elapsed:?}");
    assert_eq!(stdout(&output), "waiting\n");
    ends(&stderr(&output), "[justless] stopped at --timeout 1s\n");
    assert!(stopped(&root, "bg.pid"), "background sleep survived");
}

/// Signals a script sends its own group do not disable the time limit.
#[cfg(unix)]
#[test]
fn timeout_holds_when_the_script_signals_its_group() {
    let (_directory, root) = workspace();
    let started = Instant::now();
    let code = "trap '' USR1; kill -USR1 0; echo signalled; sleep 30; echo finished";
    let output = exec(&root, &["--lang", "sh", "--timeout", "1", code]);
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
    let code = "import os, signal, time\nsignal.signal(signal.SIGINT, signal.SIG_IGN)\nos.setpgid(0, os.getpgid(os.getppid()))\nprint('left', flush=True)\ntime.sleep(30)\nprint('finished')";
    let output = exec(&root, &["--lang", "python3", "--timeout", "1", code]);
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
    let output = sh(&root.join("src"), "echo \"$0\"");
    let id = new_run(&root, &[]);
    let expected = format!("../.justless/runs/{id}/script.sh\n");
    assert_eq!(stdout(&output), expected, "{}", stderr(&output));
    if have("ruby") {
        let output = exec(&root.join("src"), &["--lang", "ruby", "raise 'boom'"]);
        let id = run_id(&output);
        starts(
            &stderr(&output),
            &format!("../.justless/runs/{id}/script.rb:1:"),
        );
        let context = stdout(&cli(&root, &["context", "--run", &id]));
        contains(&context, &format!(".justless/runs/{id}/script.rb:1"));
    }
}

/// Each name before `:N` is resolved once per stream, so a repeated file location keeps
/// every diagnostic and timestamps never become one.
#[test]
fn repeated_locations_resolve_once_and_timestamps_stay_data() {
    let (_directory, root) = workspace();
    fs::write(root.join("price.py"), "def price():\n    return 1\n").unwrap();
    let script = "i=0; while [ $i -lt 2000 ]; do echo \"2025-03-10T00:$((i % 60)):00 INFO row $i\"; if [ $((i % 700)) -eq 0 ]; then echo \"price.py:2: bad row $i\"; fi; i=$((i + 1)); done; exit 1";
    let output = sh(&root, script);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let id = run_id(&output);
    let context = stdout(&cli(&root, &["context", "--run", &id, "--budget", "12000"]));
    for line in [2, 703, 1404] {
        contains(&context, &format!("price.py:2 · log stdout:{line}"));
    }
    starts(&context, &format!("Run {id} · 3 source locations\n"));
}

#[cfg(unix)]
#[test]
fn signal_death_maps_to_128_plus_n() {
    let (_directory, root) = workspace();
    let output = sh(&root, "kill -TERM $$");
    assert_eq!(output.status.code(), Some(143));
    ends(&stderr(&output), " · exit 143]\n");
}

/// 3000 lines of filler with matching lines at 1500 and 2900.
const TIMEOUTS: &str = "i=1; while [ $i -le 3000 ]; do if [ $i -eq 1500 ] || [ $i -eq 2900 ]; then echo \"payments timeout at $i\"; else echo \"line $i ok\"; fi; i=$((i + 1)); done";

fn markers(text: &str) -> Vec<usize> {
    text.lines()
        .filter_map(|line| line.strip_prefix("stdout:"))
        .map(|rest| rest.split(':').next().unwrap().parse().unwrap())
        .collect()
}

/// `exec --lang sh FLAGS --intent INTENT SCRIPT`.
fn focus(root: &Path, flags: &str, intent: &str, script: &str) -> Output {
    let mut args = vec!["--lang", "sh"];
    args.extend(words(flags));
    args.extend(["--intent", intent, script]);
    exec(root, &args)
}

#[test]
fn intent_ranks_long_stdout_within_budget() {
    let (_directory, root) = workspace();
    let output = focus(&root, "--max-bytes 1024", "payments timeouts", TIMEOUTS);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.len() <= 1024, "{}", text.len());
    starts(
        &text,
        "[focus] \"payments timeouts\": 2 of 3000 lines match every term, 0 match some\n",
    );
    for expected in [
        "stdout:1500: payments timeout at 1500\n",
        "stdout:2900: payments timeout at 2900\n",
        "stdout:1: line 1 ok\n",
        "stdout:3000: line 3000 ok\n",
    ] {
        contains(&text, expected);
    }
    let lines = markers(&text);
    assert!(lines.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
    ends(&text, "[focus] every match is shown\n");
    contains(&stderr(&output), " shown]");
    // The marker expands to the original line.
    let id = run_id(&output);
    let range = format!("log {id} --stream stdout --from 1500 --to 1500");
    let line = cli(&root, &words(&range));
    assert_eq!(line.stdout, b"payments timeout at 1500\n");

    let again = focus(&root, "--max-bytes 1024", "payments timeouts", TIMEOUTS);
    assert_eq!(again.stdout, output.stdout);
    let plain = exec(&root, &["--lang", "sh", "--max-bytes", "1024", TIMEOUTS]);
    assert!(!stdout(&plain).contains("[focus]"));
}

#[test]
fn intent_names_lines_left_out_for_log() {
    let (_directory, root) = workspace();
    let script = "i=1; while [ $i -le 400 ]; do echo \"$i: payments timeout request_id=$i\"; echo \"$i: orders timeout\"; i=$((i + 1)); done";
    let output = focus(&root, "", "payments timeouts", script);
    let text = stdout(&output);
    assert!(text.len() <= 4096, "{}", text.len());
    starts(
        &text,
        "[focus] \"payments timeouts\": 400 of 800 lines match every term, 400 match some\n",
    );
    let shown = markers(&text);
    // Lines that contain every term rank above those that contain one.
    for line in text.lines().filter(|line| line.starts_with("stdout:")) {
        let number: usize = markers(line)[0];
        assert!(number == 800 || line.contains("payments"), "{line}");
    }
    // The trailer lists matching lines that were left out and how to read the first.
    let id = run_id(&output);
    let trailer = text.lines().last().unwrap();
    let (counts, command) = trailer
        .split_once(" · ")
        .unwrap_or_else(|| panic!("{trailer}"));
    contains(counts, " more match every term (");
    let words: Vec<&str> = command.split(' ').collect();
    assert_eq!(&words[..5], ["justless", "log", &id, "--stream", "stdout"]);
    let from: usize = words[6].parse().unwrap();
    assert!(!shown.contains(&from), "{from} is already shown: {text}");
    let replay = cli(&root, &words[1..]);
    contains(stdout(&replay).lines().next().unwrap(), "payments timeout");
}

#[test]
fn intent_fits_one_line_at_minimum_budget() {
    let (_directory, root) = workspace();
    let script = "x=$(printf '%5000s' '' | tr ' ' x); i=1; while [ $i -le 20 ]; do echo \"$x payments timeout $x\"; i=$((i + 1)); done";
    let extra: String = (0..20).map(|n| format!("extra{n} ")).collect();
    let output = focus(
        &root,
        "--max-bytes 512",
        &format!("payments timeout {extra}"),
        script,
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
    contains(shown[0], "payments");
}

#[test]
fn intent_respects_utf8_and_long_lines() {
    let (_directory, root) = workspace();
    let script = "pad=$(printf '%2500s' '' | sed 's/ /é/g'); i=1; while [ $i -le 30 ]; do echo \"$pad café ✓ payments timeout $i $pad\"; echo \"filler $i\"; i=$((i + 1)); done";
    let output = focus(
        &root,
        "--max-bytes 1024",
        "\"café ✓ payments\" timeout",
        script,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text = String::from_utf8(output.stdout).expect("UTF-8 preview");
    assert!(text.len() <= 1024, "{}", text.len());
    contains(&text, "café ✓ payments timeout");
    for line in text.lines().filter(|line| line.starts_with("stdout:")) {
        let excerpt = line.split_once(": ").unwrap().1;
        assert!(excerpt.len() <= 240, "{}", excerpt.len());
    }
}

/// A match past the part of a line that was ranked is never denied, and the preview
/// names the search that reads whole lines.
#[test]
fn intent_points_to_search_when_a_line_is_too_long_to_rank() {
    if !have("yes") || !have("head") {
        return;
    }
    let (_directory, root) = workspace();
    let code = "yes ok | head -c 4300000 | tr '\\n' ' '; echo quota_exceeded";
    let output = focus(&root, "", "quota_exceeded", code);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    starts(
        &text,
        "[focus] no line matches \"quota_exceeded\" in its first 4 MiB;",
    );
    let id = run_id(&output);
    ends(
        &text,
        &format!("[focus] justless search {id} TEXT reads whole lines\n"),
    );
    let search = stdout(&cli(&root, &["search", &id, "quota_exceeded"]));
    starts(&search, "stdout:1: ");
}

/// One long line, such as `print(json.dumps(rows))`, fills the preview with the parts
/// that hold every term.
#[test]
fn intent_shows_several_parts_of_one_long_line() {
    let (_directory, root) = workspace();
    let script = "i=1; while [ $i -le 300 ]; do if [ $((i % 10)) -eq 3 ]; then s=PNUTUSDT; else s=BTCUSDT; fi; if [ $((i % 2)) -eq 1 ]; then d=SELL; else d=BUY; fi; printf '{\"id\": %d, \"symbol\": \"%s\", \"side\": \"%s\"}, ' $i $s $d; i=$((i + 1)); done; echo";
    let output = focus(&root, "", "PNUTUSDT SELL", script);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let parts: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("stdout:1: "))
        .collect();
    assert!(parts.len() >= 10, "{text}");
    for part in &parts {
        contains(part, "\"symbol\": \"PNUTUSDT\", \"side\": \"SELL\"");
    }
    assert!(text.len() > 3000, "{}", text.len());
    contains(text.lines().last().unwrap(), "print one record per line");
}

#[test]
fn intent_without_a_match_falls_back_to_marked_head_and_tail() {
    let (_directory, root) = workspace();
    let text = stdout(&focus(&root, "--max-bytes 1024", "zebra", NUMBERED));
    assert!(text.len() <= 1024, "{}", text.len());
    starts(
        &text,
        "[focus] no line matches \"zebra\"; first and last of 3000 lines:\n",
    );
    contains(&text, "stdout:1: line 1 ok\n");
    ends(&text, "stdout:3000: line 3000 ok\n");
}

#[test]
fn intent_folds_word_endings_and_matches_quoted_phrases() {
    let (_directory, root) = workspace();
    let script = "i=1; while [ $i -le 300 ]; do echo \"filler line $i\"; i=$((i + 1)); done; echo 'parsed config'; echo 'parsing config'; echo 'parse config'; echo 'connection reset by peer'; echo 'Connection Reset'; echo 'connection was reset'; echo end";
    let text = stdout(&focus(&root, "--max-bytes 1024", "parse", script));
    starts(&text, "[focus] \"parse\": 3 of 307 lines match\n");
    for line in [301, 302, 303] {
        contains(&text, &format!("stdout:{line}: "));
    }
    let text = stdout(&focus(
        &root,
        "--max-bytes 1024",
        "\"connection reset\"",
        script,
    ));
    starts(
        &text,
        "[focus] \"connection reset\": 2 of 307 lines match\n",
    );
    contains(&text, "stdout:304: connection reset by peer\n");
    contains(&text, "stdout:305: Connection Reset\n");
    assert!(!text.contains("stdout:306:"), "{text}");
}

#[test]
fn hooks_leave_exec_unwrapped() {
    let (_directory, root) = workspace();
    fs::create_dir(root.join("src")).unwrap();
    let hook = |handler: &str, input: Value| -> Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
            .args([handler, "before-command"])
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.to_string().as_bytes()).unwrap();
        drop(stdin);
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let bash = |command: &str| json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":root,"session_id":"s1","tool_input":{"command":command}});
    let agy = |command: &str| json!({"workspacePaths":[root],"toolCall":{"name":"run_command","args":{"CommandLine":command,"Cwd":root}}});
    let rewrite = |output: &Value| {
        let command = &output["hookSpecificOutput"]["updatedInput"]["command"];
        command.as_str().unwrap_or_default().to_owned()
    };
    let unchanged = |handler: &str, command: &str| hook(handler, bash(command)) == json!({});
    let overwrites = |command: &str| hook("hook-agy", agy(command)).get("overwrite").is_some();

    // Positive controls: the same inputs do rewrite a supported command.
    for handler in ["hook-claude", "hook-codex"] {
        let output = hook(handler, bash("git status"));
        ends(&rewrite(&output), "-- git status");
    }
    assert!(overwrites("git status"));

    for command in [
        "justless exec --lang python3 - <<'PY'\nprint(1)\nPY",
        "justless exec --lang sh --file a.log 'grep -c ERROR \"$1\"'",
        "cd src && justless exec --lang sh 'ls'",
    ] {
        for handler in ["hook-claude", "hook-codex"] {
            assert!(unchanged(handler, command), "{handler}: {command}");
        }
        assert!(!overwrites(command), "{command}");
    }
    let mixed = "cargo test && justless exec --lang sh 'echo hi'";
    let claude = hook("hook-claude", bash(mixed));
    let rewritten = rewrite(&claude);
    starts(&rewritten, "justless run --hook ");
    ends(&rewritten, &format!("-- {mixed}"));
    let decision = claude["hookSpecificOutput"].get("permissionDecision");
    assert!(decision.is_none(), "{claude}");
    assert!(unchanged("hook-codex", mixed));
    assert!(!overwrites(mixed));
}

/// Script output goes through the same pipes as `run`, so `max_run_bytes` caps what
/// each stream stores while the script still runs to completion.
#[test]
fn exec_output_is_capped_by_max_run_bytes() {
    let (_directory, root) = workspace();
    fs::write(root.join(".justless.toml"), "max_run_bytes = 1000\n").unwrap();
    let line = "abcdefghijklmnopqrstuvwxyz0123456789abcdefghij";
    let code = format!(
        "i=0; while [ $i -lt 100 ]; do echo '{line}'; i=$((i + 1)); done; echo done >&2; exit 4"
    );
    let output = sh(&root, &code);
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let id = run_id(&output);
    let directory = root.join(".justless/runs").join(&id);
    assert_eq!(fs::metadata(directory.join("stdout")).unwrap().len(), 1000);
    assert_eq!(fs::read(directory.join("stderr")).unwrap(), b"done\n");
    let message = stderr(&output);
    contains(&message, "[stdout archive truncated at 1000 B of 4.6 KB]");
}
