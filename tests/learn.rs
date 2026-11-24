//! `justless learn` against synthetic Claude Code transcripts and real archived runs.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const MINUTE: u64 = 60_000;
const DAY: u64 = 86_400_000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// RFC 3339 UTC, as Claude Code writes transcript timestamps.
fn timestamp(ms: u64) -> String {
    let z = (ms / DAY) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let rest = ms % DAY;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

fn digest(session: &str) -> String {
    format!("{:x}", Sha256::digest(session.as_bytes()))[..24].to_owned()
}

fn slug(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[track_caller]
fn contains(text: &str, needle: &str) {
    assert!(text.contains(needle), "{needle:?} not in:\n{text}");
}

fn words(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

fn run_id(output: &Output) -> String {
    let text = stderr(output);
    let start = text.find("run-").expect("run footer");
    text[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

#[derive(Clone)]
enum Outcome {
    Ok(String),
    Fail(i32, String),
    /// A nonzero exit Claude Code shows as a non-error.
    Unknown(String),
    /// Raw `content`, `is_error` and `toolUseResult`.
    Raw(String, bool, Value),
}

#[derive(Clone)]
struct Call {
    command: String,
    cwd: String,
    outcome: Outcome,
    id: Option<String>,
}

fn call(command: &str, outcome: Outcome) -> Call {
    Call {
        command: command.into(),
        cwd: String::new(),
        outcome,
        id: None,
    }
}

fn fail(code: i32, output: impl Into<String>) -> Outcome {
    Outcome::Fail(code, output.into())
}

/// Calls written as `COMMAND => OUTPUT` for one that succeeded, or
/// `COMMAND => CODE: OUTPUT` for one that exited with CODE.
fn calls(script: &[&str]) -> Vec<Call> {
    let parse = |line: &&str| {
        let (command, rest) = line.split_once(" => ").expect("COMMAND => OUTPUT");
        let outcome = match rest.split_once(": ") {
            Some((code, output)) if code.parse::<i32>().is_ok() => {
                fail(code.parse().unwrap(), output)
            }
            _ => Outcome::Ok(rest.into()),
        };
        call(command, outcome)
    };
    script.iter().map(parse).collect()
}

struct Lab {
    _temp: TempDir,
    base: PathBuf,
    root: PathBuf,
    config: PathBuf,
    bin: PathBuf,
    shell: String,
}

impl Lab {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("work");
        fs::create_dir_all(root.join(".git")).unwrap();
        let config = base.join("claude");
        fs::create_dir_all(config.join("projects").join(slug(&root))).unwrap();
        let bin = base.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(base.join("home")).unwrap();
        Lab {
            _temp: temp,
            base,
            root,
            config,
            bin,
            shell: "/bin/zsh".into(),
        }
    }

    fn transcripts(&self) -> PathBuf {
        self.config.join("projects").join(slug(&self.root))
    }

    fn command(&self, dir: &str, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_justless"));
        command
            .args(args)
            .current_dir(self.root.join(dir))
            .env("CLAUDE_CONFIG_DIR", &self.config)
            .env("PATH", &self.bin)
            .env("SHELL", &self.shell)
            .env("HOME", self.base.join("home"))
            // Codex rollouts and Antigravity CLI transcripts are read from the lab's home directory.
            .env_remove("CODEX_HOME")
            .env_remove("JUSTLESS_TEST_AGY_HOME")
            .env_remove("JUSTLESS_SESSION")
            .env_remove("CLAUDE_PROJECT_DIR");
        command
    }

    fn learn(&self, args: &[&str]) -> Output {
        let mut all = vec!["learn"];
        all.extend(args);
        self.command("", &all).output().unwrap()
    }

    fn text(&self, args: &[&str]) -> String {
        let output = self.learn(args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        stdout(&output)
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend(args);
        serde_json::from_str(&self.text(&all)).unwrap()
    }

    fn rules(&self, args: &[&str]) -> Value {
        self.json(args)["rules"].clone()
    }

    /// `justless run` with these space-separated arguments.
    fn run(&self, dir: &str, args: &str) -> Output {
        self.run_argv(dir, &words(args))
    }

    fn run_argv(&self, dir: &str, args: &[&str]) -> Output {
        let mut all = vec!["run"];
        all.extend(args);
        self.command(dir, &all).output().unwrap()
    }

    fn record(&self, id: &str) -> Value {
        serde_json::from_str(&self.read(&format!(".justless/runs/{id}/record.json"))).unwrap()
    }

    fn tool(&self, name: &str, body: &str) {
        let path = self.bin.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn write(&self, file: &str, content: &str) {
        let path = self.root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn read(&self, file: &str) -> String {
        fs::read_to_string(self.root.join(file)).unwrap()
    }

    /// Move every failing check's first failure two days back, as if a day had passed.
    fn backdate(&self) {
        let mut state: Value = serde_json::from_str(&self.read(STATE)).unwrap();
        for rule in state["rules"].as_object_mut().unwrap().values_mut() {
            if rule.get("stale_since_ms").is_some() {
                rule["stale_since_ms"] = json!(now_ms() - 2 * DAY);
            }
        }
        self.write(STATE, &state.to_string());
    }

    /// Write a transcript, one call per minute starting `start_ms`.
    fn transcript(&self, file: &str, session: &str, start_ms: u64, calls: Vec<Call>) {
        let mut lines = Vec::new();
        for (index, call) in calls.into_iter().enumerate() {
            let at = timestamp(start_ms + index as u64 * MINUTE);
            let id = call
                .id
                .unwrap_or_else(|| format!("toolu_{}_{index}", slug(Path::new(file))));
            let cwd = if call.cwd.is_empty() {
                self.root.clone()
            } else {
                self.root.join(&call.cwd)
            };
            lines.push(
                json!({"type": "assistant", "cwd": cwd, "timestamp": at, "sessionId": session,
                "message": {"content": [{"type": "tool_use", "id": id, "name": "Bash",
                    "input": {"command": call.command, "description": "Run"}}]}}),
            );
            let (content, is_error, result) = match call.outcome {
                Outcome::Ok(output) => {
                    let result = json!({"stdout": output, "stderr": "", "interrupted": false, "isImage": false});
                    (output, false, result)
                }
                Outcome::Fail(code, output) => (
                    format!("Exit code {code}\n{output}"),
                    true,
                    json!(format!("Error: Exit code {code}\n{output}")),
                ),
                Outcome::Unknown(output) => {
                    let result = json!({"stdout": output, "stderr": "", "interrupted": false,
                        "returnCodeInterpretation": "No matches found"});
                    (output, false, result)
                }
                Outcome::Raw(content, is_error, result) => (content, is_error, result),
            };
            lines.push(
                json!({"type": "user", "cwd": cwd, "timestamp": at, "sessionId": session,
                "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id,
                    "content": content, "is_error": is_error}]},
                "toolUseResult": result}),
            );
        }
        let path = self.transcripts().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: Vec<String> = lines.iter().map(Value::to_string).collect();
        fs::write(path, body.join("\n") + "\n").unwrap();
    }

    /// Session `name` in `name.jsonl`, started `age` ms ago, running `script` (see `calls`).
    fn session(&self, name: &str, age: u64, script: &[&str]) {
        let file = format!("{name}.jsonl");
        self.transcript(&file, name, now_ms() - age, calls(script));
    }

    /// The same calls in `count` separate sessions, a day apart.
    fn sessions_of(&self, prefix: &str, count: usize, calls: &[Call]) {
        for index in 0..count {
            let session = format!("{prefix}{index}");
            let start = now_ms() - (index as u64 + 1) * DAY;
            self.transcript(&format!("{session}.jsonl"), &session, start, calls.to_vec());
        }
    }

    fn sessions(&self, prefix: &str, count: usize, script: &[&str]) {
        self.sessions_of(prefix, count, &calls(script));
    }
}

fn rule<'a>(rules: &'a Value, text: &str) -> &'a Value {
    rules
        .as_array()
        .unwrap()
        .iter()
        .find(|rule| rule["text"] == text)
        .unwrap_or_else(|| panic!("no rule {text:?} in {rules:#}"))
}

fn held<'a>(report: &'a Value, class: &str) -> Vec<&'a Value> {
    report["held"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|held| held["class"] == class)
        .collect()
}

/// Why the first held candidate of `class` was held.
fn reason<'a>(report: &'a Value, class: &str) -> &'a Value {
    &held(report, class)[0]["reason"]
}

fn len(array: &Value) -> usize {
    array.as_array().unwrap().len()
}

/// The values of these space-separated count fields.
fn counts(value: &Value, keys: &str) -> Vec<u64> {
    let count = |key: &&str| {
        value[key]
            .as_u64()
            .unwrap_or_else(|| panic!("{key}: {value}"))
    };
    words(keys).iter().map(count).collect()
}

const GLOB: &str = "(eval):1: no matches found: --include=*.rs";
const STATE: &str = ".justless/learned.json";
const GLOB_CALLS: [&str; 1] = [
    "grep -rn needle --include=*.rs .; echo done => (eval):1: no matches found: --include=*.rs\ndone",
];
const GLOB_RULE: &str = "Quote glob patterns in shell arguments, as in `--include='*.rs'`; zsh stops a command at an unmatched glob.";
const GH_RULE: &str = "`gh` is not installed on this machine.";
const GH_CALLS: [&str; 1] = ["gh pr list => 127: zsh:1: command not found: gh"];
const PYTHON_RULE: &str = "Use `python3`, not `python`; `python` is not installed on this machine.";
const MAKE_RULE: &str = "Run `make` from `server/`; the repository root has no Makefile.";
const NO_MAKEFILE: &str =
    "make test => 2: make: *** No targets specified and no makefile found.  Stop.";
const MAKE_CALLS: [&str; 2] = [NO_MAKEFILE, "make -C server test => ok"];

#[test]
fn learn_command_not_found_alternative() {
    let lab = Lab::new();
    lab.tool("python3", "exit 0");
    let script = [
        "python scripts/gen.py => 127: (eval):1: command not found: python",
        "python3 scripts/gen.py => generated",
    ];
    lab.session("s1", 30 * MINUTE, &script);
    let rules = lab.rules(&[]);
    let found = rule(&rules, PYTHON_RULE);
    assert_eq!(found["class"], "missing_program");
    assert_eq!(counts(found, "corrections sessions"), [1, 1]);
    assert_eq!(found["scope"], "machine");
    assert_eq!(found["check"]["state"], "verified");
    assert_eq!(found["example"], Value::Null);
    let text = lab.text(&[]);
    contains(
        &text,
        "missing program · machine · 1 failure(s), 1 correction(s) in 1 session(s)",
    );
    contains(&text, "checked: not on PATH");
    assert!(!text.contains("scripts/gen.py"), "{text}");

    // Installing the program makes the rule stale.
    lab.tool("python", "exit 0");
    let report = lab.json(&[]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    assert_eq!(reason(&report, "missing_program"), "stale");
    assert_eq!(report["skipped"]["stale"], 1);

    // A chain that exits 0 but could not run the alternative is no correction.
    let lab = Lab::new();
    lab.tool("python3", "exit 0");
    let script = [
        "python y.py => 127: zsh:1: command not found: python",
        "python3 y.py || echo failed => zsh:1: command not found: python3\nfailed",
    ];
    lab.session("s1", 30 * MINUTE, &script);
    let report = lab.json(&[]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    let python = held(&report, "missing_program")
        .into_iter()
        .find(|held| held["text"] == "`python` is not installed on this machine.")
        .unwrap();
    assert_eq!(python["reason"], "below_min_sessions");
}

#[test]
fn learn_missing_program_needs_two_sessions() {
    let lab = Lab::new();
    lab.sessions("a", 1, &GH_CALLS);
    let report = lab.json(&[]);
    assert_eq!(report["rules"], json!([]));
    assert_eq!(reason(&report, "missing_program"), "below_min_sessions");
    let text = lab.text(&[]);
    contains(
        &text,
        "1 below --min-sessions (--min-sessions 1 lists them)",
    );
    assert_eq!(lab.rules(&["--min-sessions", "1"])[0]["text"], GH_RULE);

    lab.sessions("b", 1, &GH_CALLS);
    // A script that calls gh does not name it in its own command.
    let script = ["./build.sh => 127: bash: line 3: gh: command not found"];
    lab.session("c", 3 * DAY, &script);
    let rules = lab.rules(&[]);
    let found = rule(&rules, GH_RULE);
    assert_eq!(counts(found, "failures sessions"), [2, 2]);
    assert_eq!(found["check"]["detail"], "not on PATH");

    // An exit-0 list does not contradict the rule; a plain success does.
    let script = ["gh --version; true => zsh:1: command not found: gh"];
    lab.session("d", MINUTE, &script);
    assert_eq!(lab.rules(&[])[0]["text"], GH_RULE);
    lab.session("e", MINUTE / 2, &["gh --version => gh version 2.0"]);
    let report = lab.json(&[]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    assert_eq!(reason(&report, "missing_program"), "contradicted");
    assert_eq!(report["skipped"]["contradicted"], 1);
}

#[test]
fn learn_zsh_shell_rules() {
    let mut lab = Lab::new();
    lab.sessions("g", 2, &GLOB_CALLS);
    // Displayed text is not the command's own error.
    lab.session("notes", 3 * DAY, &[&format!("cat notes.txt => {GLOB}")]);
    let rules = lab.rules(&[]);
    let found = rule(&rules, GLOB_RULE);
    assert_eq!(found["class"], "zsh_glob");
    assert_eq!(found["failures"], 2);
    let example = json!({"wrong": "--include=*.rs", "right": "--include='*.rs'", "signal": "zsh: no matches found"});
    assert_eq!(found["example"], example);
    assert_eq!(found["check"]["detail"], "SHELL is zsh");

    lab.shell = "/bin/bash".into();
    let report = lab.json(&[]);
    assert_eq!(report["rules"], json!([]));
    assert_eq!(reason(&report, "zsh_glob"), "stale");

    let lab = Lab::new();
    let script = [
        "echo ===== done => 1: zsh:1: ==== not found",
        // Quoted, the word is not expanded, so the same line is not its error.
        "echo '=====' && cat log => 1: zsh:1: ==== not found",
    ];
    lab.sessions("q", 2, &script);
    let text = "Quote shell words that start with `=`, as in `echo '=== done'`; zsh expands them as commands.";
    assert_eq!(rule(&lab.rules(&[]), text)["failures"], 2);

    // Three agent threads of one session are still one session.
    let lab = Lab::new();
    for agent in ["a", "b", "c"] {
        let file = format!("s1/subagents/agent-{agent}.jsonl");
        lab.transcript(&file, "s1", now_ms() - DAY, calls(&GLOB_CALLS));
    }
    let report = lab.json(&[]);
    let glob = &held(&report, "zsh_glob")[0];
    assert_eq!(counts(glob, "threads sessions"), [3, 1]);
    assert_eq!(glob["reason"], "below_min_sessions");
}

fn fake_make(lab: &Lab) {
    lab.tool(
        "make",
        "if [ -f Makefile ]; then echo \"built $1\"; exit 0; fi\n\
         echo 'make: *** No targets specified and no makefile found.  Stop.' >&2\nexit 2",
    );
    lab.write("server/Makefile", "test:\n\ttrue\n");
}

#[test]
fn learn_wrong_directory_from_run_archive() {
    let lab = Lab::new();
    fake_make(&lab);
    let failed = lab.run("", "--session s1 -- make test");
    assert_eq!(failed.status.code(), Some(2));
    let id = run_id(&failed);
    let passed = lab.run("server", "--session s1 -- make test");
    assert!(passed.status.success());
    let record = lab.record(&id);
    assert_eq!(record["argv"], json!(["make", "test"]));
    assert_eq!(record["cwd"], "");
    assert_eq!(record["session"], "s1");
    let ids = fs::read_dir(lab.root.join(".justless/runs")).unwrap();
    let cwds: Vec<Value> = ids
        .map(|entry| lab.record(entry.unwrap().file_name().to_str().unwrap())["cwd"].clone())
        .collect();
    assert!(cwds.contains(&json!("server")), "{cwds:?}");

    let report = lab.json(&[]);
    let found = rule(&report["rules"], MAKE_RULE);
    assert_eq!(found["scope"], "project");
    let check = json!({"state": "verified", "detail": "server/Makefile"});
    assert_eq!(found["check"], check);
    let evidence = json!({"source": "run", "id": id, "stream": "stderr"});
    assert_eq!(found["evidence"][0], evidence);
    let example = json!({"wrong": "make test", "right": "make test (in server/)", "signal": "make: no makefile"});
    assert_eq!(found["example"], example);
    assert_eq!(report["sources"]["runs"]["scanned"], 2);
    assert_eq!(report["sources"]["transcripts"]["results"], 0);
    let text = lab.text(&[]);
    contains(
        &text,
        &format!("   evidence: justless log {id} --stream stderr\n"),
    );
    contains(
        &text,
        "Preview only. `justless learn --apply` writes these rules to AGENTS.md.",
    );

    lab.write("Makefile", "test:\n");
    let report = lab.json(&[]);
    assert_eq!(report["rules"], json!([]));
    assert_eq!(reason(&report, "wrong_directory"), "stale");
}

#[test]
fn learn_wrong_directory_normalizes_commands() {
    let lab = Lab::new();
    lab.write("server/Makefile", "test:\n");
    let missing = "make test => 2: make: *** No rule to make target `test'.  Stop.";
    lab.session("a", 2 * DAY, &[missing, "make -C server test => ok"]);
    let piped = "cd server && make test 2>&1 | tail -5 => ok";
    lab.session("b", DAY, &[missing, piped]);
    lab.write(".claude/worktrees/w1/.git", "gitdir: elsewhere\n");
    let mut worktree = calls(&[missing, "make test => ok"]);
    worktree[0].cwd = ".claude/worktrees/w1".into();
    worktree[1].cwd = ".claude/worktrees/w1/server".into();
    lab.transcript("c.jsonl", "c", now_ms() - 2 * MINUTE, worktree);
    let rules = lab.rules(&[]);
    assert_eq!(len(&rules), 1, "{rules:#}");
    assert_eq!(rules[0]["text"], MAKE_RULE);
    assert_eq!(counts(&rules[0], "corrections sessions"), [3, 3]);
}

#[test]
fn learn_unknown_target() {
    let lab = Lab::new();
    lab.write(
        "package.json",
        r#"{"scripts": {"test:unit": "node --test"}}"#,
    );
    let npm = [
        "npm test => 1: npm error Missing script: \"test\"",
        "npm run test:unit => ok",
    ];
    lab.sessions("n", 1, &npm);
    let script = "Run `npm run test:unit`; there is no `test` script.";
    let report = lab.json(&[]);
    assert_eq!(report["rules"], json!([]));
    assert_eq!(held(&report, "unknown_target")[0]["text"], script);
    lab.sessions("m", 1, &npm);
    let check = json!({"state": "verified", "detail": "package.json scripts"});
    assert_eq!(rule(&lab.rules(&[]), script)["check"], check);

    lab.write("Makefile", "check:\n\ttrue\n");
    let make = [
        "make lint => 2: make: *** No rule to make target `lint'.  Stop.",
        "make check => ok",
    ];
    lab.sessions("k", 2, &make);
    lab.write("crates/core/Cargo.toml", "[package]\nname = \"core-lib\"\n");
    let cargo = [
        "cargo test -p core => 101: error: package ID specification `core` did not match any packages",
        "cargo test -p core-lib => test result: ok. 3 passed",
    ];
    lab.sessions("p", 2, &cargo);
    let rules = lab.rules(&[]);
    let make = rule(&rules, "Run `make check`; there is no `lint` target.");
    assert_eq!(make["check"]["detail"], "Makefile");
    let cargo = rule(
        &rules,
        "Use `cargo test -p core-lib`; there is no package `core`.",
    );
    assert_eq!(cargo["check"]["detail"], "crates/core/Cargo.toml");
}

#[test]
fn learn_options() {
    let lab = Lab::new();
    let script = [
        "./tool.py -n 4 => 2: usage: tool.py [-h] [--last N]\ntool.py: error: unrecognized arguments: -n",
        "./tool.py --last 4 => ok",
        "./tool.py => 2: tool.py: error: the following arguments are required: --x",
        "./tool.py --x 1 => ok",
        "./cli build --nope => 2: error: Found argument '--nope' which wasn't expected, or isn't valid in this context",
        "./cli build => ok",
        "git commit --ammend => 129: error: unknown option `ammend'",
        "git commit --amend => ok",
    ];
    lab.sessions("o", 2, &script);
    let report = lab.json(&[]);
    let rules = &report["rules"];
    let option = rule(
        rules,
        "`./tool.py` here does not accept `-n`; use `--last`.",
    );
    assert_eq!(option["scope"], "project");
    assert_eq!(option["check"]["state"], "unverified");
    rule(rules, "Pass `--x` to `./tool.py`.");
    rule(rules, "`./cli build` here does not accept `--nope`.");
    assert_eq!(len(rules), 3, "{rules:#}");
    assert_eq!(report["skipped"]["typos"], 2);
    let text = lab.text(&[]);
    contains(&text, "2 typo(s)");
    contains(&text, "not checked");
}

#[test]
fn learn_wrong_test_path() {
    let lab = Lab::new();
    fs::create_dir_all(lab.root.join("api/tests")).unwrap();
    fs::create_dir_all(lab.root.join("src")).unwrap();
    let script = [
        "pytest tests/test_api.py => 4: ERROR: file or directory not found: tests/test_api.py",
        "pytest api/tests/test_api.py => 1 passed",
        // Another file in the same directory is a one-off.
        "pytest api/tests/test_a.py => 4: ERROR: file or directory not found: api/tests/test_a.py",
        "pytest api/tests/test_b.py => 1 passed",
        "node --test tests/x.test.js => 1: Could not find 'tests/x.test.js'",
        "node --test src/x.test.js => # pass 1",
    ];
    lab.sessions("t", 2, &script);
    let rules = lab.rules(&[]);
    assert_eq!(len(&rules), 2, "{rules:#}");
    let pytest = rule(
        &rules,
        "`pytest` tests are under `api/tests/`, not `tests/`.",
    );
    assert_eq!(pytest["check"]["detail"], "api/tests/");
    rule(
        &rules,
        "`node --test` tests are under `src/`, not `tests/`.",
    );
    fs::create_dir(lab.root.join("tests")).unwrap();
    assert_eq!(lab.rules(&[]), json!([]));
}

#[test]
fn learn_empty_test_run() {
    let lab = Lab::new();
    lab.tool(
        "cargo",
        "if [ \"$2\" = -p ]; then echo 'running 3 tests'; echo 'test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out'; exit 0; fi\n\
         if [ \"$2\" = x ]; then echo 'running 0 tests'; echo 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out'; exit 0; fi\n\
         echo 'running 0 tests'; echo 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out'",
    );
    lab.tool(
        "pytest",
        "if [ \"$1\" = -k ]; then echo '1 failed'; exit 1; fi\necho 'no tests ran'; exit 5",
    );
    for session in ["s1", "s2"] {
        let commands =
            "cargo test,cargo test -p core,cargo test x,cargo test x --lib,pytest,pytest -k api";
        for command in commands.split(',') {
            lab.run("", &format!("--session {session} -- {command}"));
        }
    }
    let rules = lab.rules(&[]);
    assert_eq!(len(&rules), 2, "{rules:#}");
    let cargo = rule(
        &rules,
        "Run tests with `cargo test -p core`; plain `cargo test` here runs no tests.",
    );
    assert_eq!(cargo["evidence"][0]["stream"], "stdout");
    rule(
        &rules,
        "Run tests with `pytest -k api`; plain `pytest` here runs no tests.",
    );
}

#[test]
fn learn_wrong_package_manager() {
    let lab = Lab::new();
    lab.write("pnpm-lock.yaml", "lockfileVersion: 9\n");
    let script = [
        "npm install => 1: npm error code EUNSUPPORTEDPROTOCOL\nnpm error Unsupported URL Type \"workspace:\": workspace:*",
        "pnpm install => done",
        "yarn install => 1: Usage Error: This project is configured to use pnpm because /x/package.json has a \"packageManager\" field",
        "pnpm install => done",
    ];
    lab.session("a", DAY, &script);
    let rules = lab.rules(&[]);
    let npm = rule(
        &rules,
        "Use `pnpm`, not `npm`; this project has pnpm-lock.yaml.",
    );
    assert_eq!(counts(npm, "corrections sessions"), [1, 1]);
    let yarn = "Use `pnpm`, not `yarn`; this project has pnpm-lock.yaml.";
    rule(&rules, yarn);
    // With npm's lockfile too, the npm rule no longer holds.
    lab.write("package-lock.json", "{}\n");
    let report = lab.json(&[]);
    assert_eq!(len(&report["rules"]), 1, "{report:#}");
    assert_eq!(report["rules"][0]["text"], yarn);
    let npm = held(&report, "wrong_package_manager");
    assert_eq!((npm.len(), &npm[0]["reason"]), (1, &json!("stale")));
}

#[test]
fn learn_ignores_non_mistakes() {
    let lab = Lab::new();
    let gh = "zsh:1: command not found: gh\n";
    let raw = |command: &str, content: &str, is_error: bool, result: Value| {
        call(command, Outcome::Raw(content.into(), is_error, result))
    };
    let denied = format!("Permission to use Bash has been denied. {gh}");
    let refused = "The user doesn't want to proceed with this tool use.";
    let cancelled = "<tool_use_error>Cancelled</tool_use_error>";
    let isolated = "This command runs in an isolated worktree.";
    let error = json!("Error");
    let interrupted = json!({"stdout": gh, "interrupted": true});
    let background = json!({"stdout": gh, "interrupted": false, "backgroundTaskId": "b1"});
    let timed_out = json!({"stdout": gh, "interrupted": false, "timedOutAfterMs": 120000});
    let mut ignored = vec![
        raw("gh pr list", &denied, true, json!("Error: denied")),
        raw("gh pr list", refused, true, error.clone()),
        raw("gh pr list", cancelled, true, error.clone()),
        raw("gh pr list", isolated, true, error),
        raw("gh pr list", gh, false, interrupted),
        raw("gh pr view", gh, false, background),
        raw("gh pr diff", gh, false, timed_out),
        call("grep foo x", Outcome::Unknown(String::new())),
    ];
    ignored.extend(calls(&[
        "grep foo x => 1: ",
        "grep -r foo . => x:foo",
        "cargo test => 101: running 2 tests\ntest a ... FAILED\ntest result: FAILED. 1 passed; 1 failed",
        "cargo test --release => test result: ok",
        "python3 x.py => 1: Traceback (most recent call last):\nModuleNotFoundError: No module named 'requests'",
        "justless qury x => 2: error: Found argument 'qury' which wasn't expected",
        "justless query x => x",
    ]));
    lab.sessions_of("x", 2, &ignored);
    let report = lab.json(&["--min-sessions", "1"]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    assert_eq!(report["held"], json!([]), "{report:#}");
    let transcripts = &report["sources"]["transcripts"];
    let expected = [14, 2, 16];
    assert_eq!(
        counts(transcripts, "not_run status_unknown results"),
        expected
    );
}

#[test]
fn learn_transcript_format() {
    let lab = Lab::new();
    lab.sessions("g", 2, &GLOB_CALLS);
    let summary = |report: Value| {
        let rule = &report["rules"][0];
        let fields = [&rule["id"], &rule["failures"], &rule["sessions"]];
        (
            report["sources"].clone(),
            fields.map(Value::clone),
            report["held"].clone(),
        )
    };
    let baseline = summary(lab.json(&[]));
    // A blob replay without cwd or timestamp, a tool_use in a user entry and one
    // without cwd change nothing.
    let path = lab.transcripts().join("g0.jsonl");
    let original = fs::read_to_string(&path).unwrap();
    let blob = json!({"type": "api-request-blob", "messages": [{"content": [
        {"type": "tool_use", "id": "blob1", "name": "Bash", "input": {"command": "gh x"}},
        {"type": "tool_result", "tool_use_id": "blob1", "content": "zsh:1: command not found: gh"}]}]});
    let user_use = json!({"type": "user", "cwd": lab.root, "timestamp": timestamp(now_ms() - DAY), "sessionId": "g0",
        "message": {"content": [{"type": "tool_use", "id": "u1", "name": "Bash", "input": {"command": "gh x"}},
            {"type": "tool_result", "tool_use_id": "u1", "content": "zsh:1: command not found: gh"}]}});
    let no_cwd = json!({"type": "assistant", "timestamp": timestamp(now_ms() - DAY), "sessionId": "g0",
        "message": {"content": [{"type": "tool_use", "id": "n1", "name": "Bash", "input": {"command": "gh x"}}]}});
    let no_cwd_result = json!({"type": "user", "cwd": lab.root, "timestamp": timestamp(now_ms() - DAY), "sessionId": "g0",
        "message": {"content": [{"type": "tool_result", "tool_use_id": "n1", "content": "zsh:1: command not found: gh"}]}});
    let lines = format!("{blob}\n{user_use}\n{no_cwd}\n{no_cwd_result}\n{original}");
    fs::write(&path, lines).unwrap();
    assert_eq!(summary(lab.json(&[])), baseline);

    // A resumed session replays the same tool_use id: it counts once.
    let mut replay = calls(&GLOB_CALLS);
    replay[0].id = Some("toolu_g0-jsonl_0".into());
    lab.transcript("g0-resumed.jsonl", "g0", now_ms() - DAY, replay);
    let report = lab.json(&[]);
    assert_eq!(report["sources"]["transcripts"]["results"], 2);
    assert_eq!(report["sources"]["transcripts"]["files"], 3);

    // Workflow subagents are read and share their parent's session.
    let workflow = "g1/subagents/workflows/wf_1";
    let agent = format!("{workflow}/agent-a.jsonl");
    lab.transcript(&agent, "g1", now_ms() - DAY / 2, calls(&GLOB_CALLS));
    let journal = lab.transcripts().join(workflow).join("journal.jsonl");
    fs::write(journal, "{\"type\":\"launched\"}\n").unwrap();
    let report = lab.json(&[]);
    let transcripts = &report["sources"]["transcripts"];
    assert_eq!(counts(transcripts, "files sessions threads"), [4, 2, 3]);
    let glob = rule(&report["rules"], GLOB_RULE);
    assert_eq!(counts(glob, "sessions threads"), [2, 3]);
}

#[test]
fn learn_links_and_dedupes_runs() {
    let lab = Lab::new();
    fake_make(&lab);
    let hooked = format!("--hook --session {} -- make test", digest("s1"));
    let id = run_id(&lab.run("", &hooked));
    // A hooked success that reached the agent verbatim prints no footer.
    let passed = lab.run("server", &hooked);
    let quiet = passed.status.success() && stderr(&passed).is_empty();
    assert!(quiet, "{passed:?}");
    // A run from another client stays.
    lab.run("", "--hook --session codex-1 -- make build");
    for form in [
        "[justless {id}] exit 2 · 60 of 60 output bytes · justless log {id}",
        "[justless {id} · exit 2]",
        "[justless {id} · 1.0 KB of 9.0 KB shown · exit 2]",
    ] {
        eprintln!("case: {form}");
        let failed = format!("make test => 2: {}", form.replace("{id}", &id));
        lab.session(
            "s1",
            10 * MINUTE,
            &[&failed, "cd server && make test => built test"],
        );
        let report = lab.json(&[]);
        let runs = &report["sources"]["runs"];
        let expected = [3, 1, 1];
        let keys = "scanned linked covered_by_transcripts";
        assert_eq!(counts(runs, keys), expected, "{form}");
        let found = rule(&report["rules"], MAKE_RULE);
        assert_eq!(counts(found, "failures sessions"), [1, 1], "{form}");
        let evidence = json!({"source": "run", "id": id, "stream": "stderr"});
        assert_eq!(found["evidence"][0], evidence);
    }
}

#[test]
fn learn_drops_credentials() {
    let lab = Lab::new();
    lab.write("server/Makefile", "test:\n");
    let token = format!("ghp_{}", "a1B2".repeat(9));
    let secrets = [
        "API_TOKEN=abc123 make test".to_owned(),
        "make test --password hunter2".to_owned(),
        "make test URL=https://u:p@example.invalid/x".to_owned(),
        format!("make test {token}"),
        "make test Zx8Qk2Lp9Vr4Tn7Wm3Yb6Hc1Df5Gs0Ja".to_owned(),
        "make test /home/someone/x".to_owned(),
        "ssh host make test".to_owned(),
    ];
    for (index, command) in secrets.iter().enumerate() {
        let failed = format!("{command} => 2: make: *** No rule to make target `test'.  Stop.");
        let fixed = format!("cd server && {command} => ok");
        lab.session(&format!("c{index}"), DAY, &[&failed, &fixed]);
    }
    let report = lab.json(&["--min-sessions", "1"]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    assert_eq!(report["skipped"]["credentials"], secrets.len());
    let text = lab.text(&["--min-sessions", "1"]) + &report.to_string();
    let leaked = "abc123,hunter2,example.invalid,ghp_,Zx8Qk,/home/someone,ssh host";
    for secret in leaked.split(',') {
        assert!(!text.contains(secret), "{secret}: {text}");
    }
    contains(&text, "7 dropped for a possible credential");
}

/// A later success of the failed command, from the root, contradicts a
/// wrong-directory rule and removes it from the file it was written to.
#[test]
fn learn_contradiction() {
    let npm_missing = "npm error code ENOENT\nnpm error syscall open\nnpm error path /x/package.json\nnpm error enoent Could not read package.json";
    let npm_failed = format!("npm test => 254: {npm_missing}");
    for (manifest, content, failed, fixed, text) in [
        (
            "server/Makefile",
            "test:\n",
            NO_MAKEFILE,
            "make -C server test => ok",
            MAKE_RULE,
        ),
        (
            "web/package.json",
            "{}\n",
            &npm_failed,
            "npm --prefix web test => ok",
            "Run `npm` from `web/`; the repository root has no package.json.",
        ),
    ] {
        eprintln!("case: {manifest}");
        let lab = Lab::new();
        lab.write(manifest, content);
        lab.session("a", DAY, &[failed, fixed]);
        let output = lab.text(&["--apply"]);
        contains(&output, "Updated CLAUDE.md: 1 learned rule(s) (1 added).");
        contains(&lab.read("CLAUDE.md"), text);
        let command = failed.split_once(" => ").unwrap().0;
        lab.session("b", MINUTE, &[&format!("{command} => ok")]);
        let report = lab.json(&[]);
        assert_eq!(report["rules"], json!([]), "{report:#}");
        assert_eq!(reason(&report, "wrong_directory"), "contradicted");
        let output = lab.text(&["--apply", "--target", "claude"]);
        contains(
            &output,
            "Updated CLAUDE.md: 0 learned rule(s) (1 removed: the command later succeeded).",
        );
        assert_eq!(lab.read("CLAUDE.md"), "");
    }
}

#[test]
fn learn_apply_targets() {
    let setup = |files: &[(&str, &str)]| {
        let lab = Lab::new();
        lab.sessions("g", 2, &GLOB_CALLS);
        for (file, content) in files {
            lab.write(file, content);
        }
        lab
    };
    let block = format!(
        "<!-- justless:learned:begin -->\n## Learned command rules\n\n- {GLOB_RULE}\n<!-- justless:learned:end -->\n"
    );

    // (a) CLAUDE.md imports AGENTS.md, on a line of its own or inline.
    for claude in [
        "@AGENTS.md\n",
        "# Claude\n\nFollow @AGENTS.md for everything.\n",
    ] {
        eprintln!("case: {claude:?}");
        let lab = setup(&[("AGENTS.md", "# Agents\n"), ("CLAUDE.md", claude)]);
        let report = lab.json(&["--apply"]);
        assert_eq!(report["apply"]["changed"], json!(["AGENTS.md"]));
        let skipped = json!([{"file": "CLAUDE.md", "reason": "imports AGENTS.md"}]);
        assert_eq!(report["apply"]["skipped"], skipped);
        assert_eq!(lab.read("AGENTS.md"), format!("# Agents\n\n{block}"));
        assert_eq!(lab.read("CLAUDE.md"), claude);
    }

    // (b) A mention in backticks or a fence is not an import. (c) Independent files.
    for claude in ["See `@AGENTS.md`.\n", "```\n@AGENTS.md\n```\n"] {
        let lab = setup(&[("AGENTS.md", "# Agents\n"), ("CLAUDE.md", claude)]);
        let report = lab.json(&["--apply"]);
        let changed = json!(["AGENTS.md", "CLAUDE.md"]);
        assert_eq!(report["apply"]["changed"], changed);
        assert_eq!(lab.read("CLAUDE.md"), format!("{claude}\n{block}"));
    }
    let lab = setup(&[("AGENTS.md", "a\n"), ("GEMINI.md", "g\n")]);
    lab.text(&["--apply"]);
    assert!(lab.read("AGENTS.md").ends_with(&block) && lab.read("GEMINI.md").ends_with(&block));

    // (d) With no file, transcript rules create CLAUDE.md and archive rules AGENTS.md.
    let lab = setup(&[]);
    contains(&lab.text(&["--apply"]), "Updated CLAUDE.md");
    assert_eq!(lab.read("CLAUDE.md"), block);
    let lab = Lab::new();
    fake_make(&lab);
    lab.run("", "-- make test");
    lab.run("server", "-- make test");
    lab.text(&["--apply"]);
    contains(&lab.read("AGENTS.md"), "Run `make` from `server/`");
    assert!(!lab.root.join("CLAUDE.md").exists());

    // (d2) A CLAUDE.md above the root hides AGENTS.md from Claude Code.
    let lab = setup(&[("AGENTS.md", "# Agents\n")]);
    fs::write(lab.base.join("CLAUDE.md"), "# Parent\n").unwrap();
    let report = lab.json(&[]);
    let note = format!(
        "Claude Code reads {} here, not AGENTS.md; add a CLAUDE.md with the line @AGENTS.md so it reads these rules.",
        lab.base.join("CLAUDE.md").display()
    );
    assert_eq!(report["notes"], json!([note]));

    // (e) A symlink is skipped and stays a link; its target gets the block once.
    #[cfg(unix)]
    {
        let lab = setup(&[("AGENTS.md", "# Agents\n")]);
        std::os::unix::fs::symlink("AGENTS.md", lab.root.join("CLAUDE.md")).unwrap();
        let report = lab.json(&["--apply"]);
        assert_eq!(report["apply"]["changed"], json!(["AGENTS.md"]));
        let skipped = json!([{"file": "CLAUDE.md", "reason": "symbolic link"}]);
        assert_eq!(report["apply"]["skipped"], skipped);
        let link = fs::symlink_metadata(lab.root.join("CLAUDE.md")).unwrap();
        assert!(link.file_type().is_symlink());
        assert_eq!(lab.read("AGENTS.md").matches("learned:begin").count(), 1);
        // (g) An explicit symlink target is refused.
        let output = lab.learn(&["--apply", "--target", "claude"]);
        assert_eq!(output.status.code(), Some(2));
        contains(&stderr(&output), "CLAUDE.md is a symbolic link");
    }

    // (f) CLAUDE.md is never created beside AGENTS.md.
    let lab = setup(&[("AGENTS.md", "# Agents\n")]);
    let output = lab.learn(&["--apply", "--target", "claude"]);
    assert_eq!(output.status.code(), Some(2));
    contains(
        &stderr(&output),
        "creating CLAUDE.md would stop Claude Code from reading AGENTS.md",
    );
    assert!(!lab.root.join("CLAUDE.md").exists() && !lab.root.join(".justless").exists());
    assert_eq!(lab.read("AGENTS.md"), "# Agents\n");
}

#[test]
fn learn_apply_byte_preserving_and_idempotent() {
    let lab = Lab::new();
    lab.sessions("g", 2, &GLOB_CALLS);
    lab.write("AGENTS.md", "# Project\n\nIntro.\n");
    let init = || {
        lab.command("", &["init", "--agent", "codex"])
            .output()
            .unwrap()
    };
    let output = init();
    assert!(output.status.success(), "{}", stderr(&output));
    let mut original = lab.read("AGENTS.md");
    original.push_str("\n## Notes\n\nKeep this.\n");
    lab.write("AGENTS.md", &original);
    let output = lab.text(&["--apply"]);
    contains(&output, "Updated AGENTS.md: 1 learned rule(s) (1 added).");
    let applied = lab.read("AGENTS.md");
    let block = format!(
        "<!-- justless:learned:begin -->\n## Learned command rules\n\n- {GLOB_RULE}\n<!-- justless:learned:end -->"
    );
    let marker = "<!-- justless:end -->";
    let expected = original.replacen(marker, &format!("{marker}\n\n{block}"), 1);
    assert_eq!(applied, expected);

    let output = lab.text(&["--apply"]);
    contains(&output, "AGENTS.md is up to date (1 learned rule(s)).");
    assert_eq!(lab.read("AGENTS.md"), applied);
    let modified = || {
        let state = fs::metadata(lab.root.join(STATE)).unwrap();
        state.modified().unwrap()
    };
    let state = modified();
    lab.text(&["--apply"]);
    assert_eq!(modified(), state);
    assert!(init().status.success());
    assert_eq!(lab.read("AGENTS.md"), applied);

    // CRLF files get a CRLF block, and a file without a final newline is handled.
    let crlf = format!(
        "# Rules\r\nUse care.\r\n\r\n{}\r\n",
        block.replace('\n', "\r\n")
    );
    for (original, expected) in [
        ("# Rules\r\nUse care.\r\n", crlf),
        ("# Rules", format!("# Rules\n\n{block}\n")),
    ] {
        let lab = Lab::new();
        lab.sessions("g", 2, &GLOB_CALLS);
        lab.write("AGENTS.md", original);
        lab.text(&["--apply"]);
        assert_eq!(lab.read("AGENTS.md"), expected);
        lab.text(&["--apply"]);
        assert_eq!(lab.read("AGENTS.md"), expected);
    }
}

/// A call to a program that is not installed, in sessions named after it.
fn missing(lab: &Lab, program: &str, sessions: usize) {
    let failed = format!("{program} x => 127: zsh:1: command not found: {program}");
    lab.sessions(program, sessions, &[&failed]);
}

#[test]
fn learn_apply_stability() {
    let lab = Lab::new();
    lab.write("AGENTS.md", "# Project\n");
    lab.sessions("g", 2, &GLOB_CALLS);
    lab.sessions("h", 2, &GH_CALLS);
    lab.text(&["--apply"]);
    let first = lab.read("AGENTS.md");
    contains(&first, &format!("- {GLOB_RULE}\n- {GH_RULE}\n"));

    // A new rule that outranks both, and a new most frequent glob example.
    missing(&lab, "go", 7);
    lab.sessions("py", 3, &["ls *.py => 1: zsh:1: no matches found: *.py"]);
    let preview = lab.json(&[]);
    let go = "`go` is not installed on this machine.";
    assert_eq!(preview["rules"][0]["text"], go);
    // A written rule keeps its text even when the example changes.
    let glob = rule(&preview["rules"], GLOB_RULE);
    assert_eq!(glob["example"]["right"], "'*.py'");

    // Held candidates beyond --limit neither hide a full block nor shrink the counts.
    for program in ["fd", "bat", "jq"] {
        missing(&lab, program, 1);
    }
    let report = lab.json(&["--apply", "--limit", "2"]);
    assert_eq!(lab.read("AGENTS.md"), first);
    let held_now = &report["held"];
    assert_eq!(len(held_now), 2, "{report:#}");
    assert_eq!(held_now[0]["reason"], "block_full");
    assert_eq!(held_now[0]["text"], go);
    let text = lab.text(&["--limit", "1"]);
    contains(
        &text,
        "3 below --min-sessions (--min-sessions 1 lists them)",
    );

    lab.text(&["--apply"]);
    let end = "<!-- justless:learned:end -->";
    let expected = first.replace(end, &format!("- {go}\n{end}"));
    assert_eq!(lab.read("AGENTS.md"), expected);
}

#[test]
fn learn_apply_persists_dismisses_expires() {
    let lab = Lab::new();
    let original = "# Project\n";
    lab.write("AGENTS.md", original);
    lab.sessions("g", 2, &GLOB_CALLS);
    lab.sessions("h", 2, &GH_CALLS);
    lab.text(&["--apply"]);
    let applied = lab.read("AGENTS.md");

    // A reverted file forgets what was written, so the rules come back.
    lab.write("AGENTS.md", original);
    contains(&lab.text(&["--apply"]), "(2 added)");
    assert_eq!(lab.read("AGENTS.md"), applied);

    // Rules whose evidence aged out stay.
    let saved = lab.base.join("saved");
    fs::rename(lab.transcripts(), &saved).unwrap();
    let output = lab.text(&["--apply"]);
    contains(&output, "AGENTS.md is up to date (2 learned rule(s)).");
    fs::rename(&saved, lab.transcripts()).unwrap();

    // A machine rule whose check fails stays until the check still fails a day later;
    // then it is removed, and a user line in the block keeps its place.
    lab.tool("gh", "exit 0");
    let output = lab.text(&["--apply"]);
    contains(&output, "AGENTS.md is up to date (2 learned rule(s)).");
    contains(
        &output,
        "Kept 1 written rule(s) whose check fails here (gh is on PATH)",
    );
    assert_eq!(lab.read("AGENTS.md"), applied);
    lab.backdate();
    let output = lab.text(&["--apply"]);
    contains(&output, "(1 removed: its check no longer holds)");
    let end = "<!-- justless:learned:end -->";
    let with_note = lab
        .read("AGENTS.md")
        .replace(end, &format!("- Keep this note.\n{end}"));
    lab.write("AGENTS.md", &with_note);
    contains(&lab.text(&["--apply"]), "up to date");
    assert_eq!(lab.read("AGENTS.md"), with_note);

    // Deleting a line dismisses its rule.
    let dismissed = with_note.replace(&format!("- {GLOB_RULE}\n"), "");
    lab.write("AGENTS.md", &dismissed);
    lab.text(&["--apply"]);
    assert_eq!(lab.read("AGENTS.md"), dismissed);
    lab.text(&["--apply"]);
    assert_eq!(lab.read("AGENTS.md"), dismissed);
    let state: Value = serde_json::from_str(&lab.read(STATE)).unwrap();
    assert_eq!(len(&state["dismissed"]), 1);
    assert_eq!(lab.json(&[])["skipped"]["dismissed"], 1);

    // Removing the last written rule restores the original bytes.
    let mut lab = Lab::new();
    lab.write("AGENTS.md", original);
    lab.sessions("g", 2, &GLOB_CALLS);
    lab.text(&["--apply"]);
    lab.shell = "/bin/bash".into();
    lab.text(&["--apply"]);
    assert_ne!(lab.read("AGENTS.md"), original);
    lab.backdate();
    lab.text(&["--apply"]);
    assert_eq!(lab.read("AGENTS.md"), original);
}

#[test]
fn learn_apply_survives_environment_flips() {
    let mut lab = Lab::new();
    lab.write("AGENTS.md", "# Project\n");
    lab.sessions("h", 3, &GH_CALLS);
    lab.sessions("g", 2, &GLOB_CALLS);
    lab.text(&["--apply"]);
    let applied = lab.read("AGENTS.md");
    contains(&applied, &format!("- {GH_RULE}\n- {GLOB_RULE}\n"));
    let stale = |lab: &Lab| lab.read(STATE).contains("stale_since_ms");

    // A PATH entry or another shell for one run changes nothing in the file.
    lab.tool("gh", "exit 0");
    lab.text(&["--apply"]);
    assert_eq!(lab.read("AGENTS.md"), applied);
    assert!(stale(&lab));
    fs::remove_file(lab.bin.join("gh")).unwrap();
    lab.text(&["--apply"]);
    assert_eq!(lab.read("AGENTS.md"), applied);
    assert!(!stale(&lab), "{}", lab.read(STATE));
    lab.shell = "/bin/bash".into();
    lab.text(&["--apply"]);
    lab.shell = "/bin/zsh".into();
    lab.text(&["--apply"]);
    assert_eq!(lab.read("AGENTS.md"), applied);

    // A rule removed after a day returns to its own line when its check holds again.
    lab.tool("gh", "exit 0");
    lab.text(&["--apply"]);
    lab.backdate();
    lab.text(&["--apply"]);
    let without_gh = applied.replace(&format!("- {GH_RULE}\n"), "");
    assert_eq!(lab.read("AGENTS.md"), without_gh);
    fs::remove_file(lab.bin.join("gh")).unwrap();
    contains(&lab.text(&["--apply"]), "(1 added)");
    assert_eq!(lab.read("AGENTS.md"), applied);
    lab.shell = "/bin/bash".into();
    lab.text(&["--apply"]);
    lab.backdate();
    lab.text(&["--apply"]);
    let without_glob = applied.replace(&format!("- {GLOB_RULE}\n"), "");
    assert_eq!(lab.read("AGENTS.md"), without_glob);
    lab.shell = "/bin/zsh".into();
    lab.text(&["--apply"]);
    assert_eq!(lab.read("AGENTS.md"), applied);
    assert!(
        !lab.read(STATE).contains("\"removed\""),
        "{}",
        lab.read(STATE)
    );
}

#[test]
fn learn_apply_rejects_bad_markers() {
    let begin = "<!-- justless:learned:begin -->";
    let end = "<!-- justless:learned:end -->";
    for broken in [
        format!("# A\n{begin}\n- x\n"),
        format!("# A\n{begin}\n{end}\n{begin}\n{end}\n"),
        format!("<!-- justless:begin -->\n{begin}\n{end}\n<!-- justless:end -->\n"),
    ] {
        let lab = Lab::new();
        lab.sessions("g", 2, &GLOB_CALLS);
        lab.write("AGENTS.md", &broken);
        lab.write("GEMINI.md", "# Gemini\n");
        let output = lab.learn(&["--apply"]);
        assert_eq!(output.status.code(), Some(2), "{broken}");
        contains(&stderr(&output), "AGENTS.md");
        assert_eq!(lab.read("AGENTS.md"), broken);
        assert_eq!(lab.read("GEMINI.md"), "# Gemini\n");
        // A preview reports the file and still shows the rules.
        let report = lab.json(&[]);
        let notes = report["notes"].to_string();
        contains(&notes, "AGENTS.md");
        contains(&notes, "`--apply` stops on it.");
        rule(&report["rules"], GLOB_RULE);
    }
}

#[test]
fn learn_preview_fails_open() {
    let lab = Lab::new();
    lab.sessions("g", 2, &GLOB_CALLS);
    fs::write(lab.root.join("AGENTS.md"), b"# A\n\xff\xfe\n").unwrap();
    for args in [&[][..], &["--target", "agents"]] {
        let report = lab.json(args);
        let note = "AGENTS.md is not UTF-8: invalid utf-8 sequence of 1 bytes from index 4; `--apply` stops on it.";
        assert_eq!(report["notes"], json!([note]), "{report:#}");
        rule(&report["rules"], GLOB_RULE);
        let mut apply = vec!["--apply"];
        apply.extend(args);
        let output = lab.learn(&apply);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        contains(&stderr(&output), "AGENTS.md is not UTF-8");
    }
    assert!(!lab.root.join("CLAUDE.md").exists());

    let lab = Lab::new();
    lab.sessions("g", 2, &GLOB_CALLS);
    lab.write(STATE, "{not json");
    let report = lab.json(&[]);
    let note = report["notes"][0].as_str().unwrap();
    assert!(
        note.starts_with("decode .justless/learned.json"),
        "{report:#}"
    );
    rule(&report["rules"], GLOB_RULE);
    let output = lab.learn(&["--apply"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(lab.read(STATE), "{not json");
}

#[test]
fn learn_counts_corrections_across_nested_checkouts() {
    // The same directory in a worktree is another checkout, not another directory.
    let lab = Lab::new();
    lab.write(".claude/worktrees/w1/.git", "gitdir: elsewhere\n");
    let mut nested = calls(&[NO_MAKEFILE, "make test => ok"]);
    nested[1].cwd = ".claude/worktrees/w1".into();
    lab.sessions_of("n", 2, &nested);
    let report = lab.json(&["--min-sessions", "1"]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    assert_eq!(report["skipped"]["nested_checkout"], 2, "{report:#}");
    contains(&lab.text(&[]), "2 split across nested checkouts");
}

#[test]
fn learn_notes_only_with_rules() {
    // Without rules, a CLAUDE.md above the root is not worth a note.
    let lab = Lab::new();
    lab.write("AGENTS.md", "# Agents\n");
    fs::write(lab.base.join("CLAUDE.md"), "# Parent\n").unwrap();
    assert_eq!(lab.json(&[])["notes"], json!([]));

    // A preview names the files it would skip.
    lab.sessions("g", 2, &GLOB_CALLS);
    lab.write("CLAUDE.md", "@AGENTS.md\n");
    let text = lab.text(&[]);
    contains(&text, "Skipped CLAUDE.md: it imports AGENTS.md.\n");
    contains(
        &text,
        "Preview only. `justless learn --apply` writes these rules to AGENTS.md.",
    );
    lab.text(&["--apply"]);
    let text = lab.text(&[]);
    contains(&text, "Preview only. AGENTS.md already up to date.");
}

#[test]
fn learn_apply_reports_a_full_block() {
    let lab = Lab::new();
    lab.write("AGENTS.md", "# Agents\n");
    for index in 1..=8 {
        missing(&lab, &format!("tool{index}"), 2);
    }
    contains(&lab.text(&["--apply", "--target", "agents"]), "(8 added)");
    let written = lab.read("AGENTS.md");
    missing(&lab, "newtool", 3);
    let newtool = "`newtool` is not installed on this machine.";
    for args in [
        &["--target", "agents"][..],
        &["--apply", "--target", "agents"],
    ] {
        let text = lab.text(args);
        contains(
            &text,
            "\nNot written, the block is full:\n- `newtool` is not installed on this machine.\nDelete a rule's line between the markers to dismiss it and make room.\n",
        );
        assert!(!text.contains(&format!(". {newtool}")), "{text}");
        assert!(!text.contains("beyond --limit"), "{text}");
        contains(&text, "8 rule(s)");
        let report = lab.json(args);
        let rules = report["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 8, "{report:#}");
        assert!(rules.iter().all(|rule| rule["text"] != newtool));
        assert_eq!(report["held"][0]["text"], newtool);
        assert_eq!(report["held"][0]["reason"], "block_full");
        assert_eq!(report["skipped"]["over_limit"], 0);
    }
    let text = lab.text(&["--apply"]);
    contains(
        &text,
        "AGENTS.md unchanged: 8 learned rule(s); 1 rule(s) did not fit.",
    );
    assert_eq!(lab.read("AGENTS.md"), written);
}

/// A real Git repository in the lab, with `git` on the lab's PATH.
fn git_lab() -> Lab {
    let lab = Lab::new();
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    lab.tool("git", &format!("exec '{}' \"$@\"", git.display()));
    fs::remove_dir_all(lab.root.join(".git")).unwrap();
    let init = Command::new(&git)
        .args(["init", "-q"])
        .current_dir(&lab.root)
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");
    lab.write("server/Makefile", "test:\n");
    lab.sessions("g", 2, &GLOB_CALLS);
    lab.sessions("m", 2, &MAKE_CALLS);
    lab
}

#[test]
fn learn_keeps_machine_rules_out_of_shared_files() {
    let lab = git_lab();
    lab.write("AGENTS.md", "# Agents\n");
    let report = lab.json(&[]);
    let rules = &report["rules"];
    assert_eq!(len(rules), 1, "{report:#}");
    assert_eq!(rules[0]["text"], MAKE_RULE);
    assert_eq!(reason(&report, "zsh_glob"), "machine_scope");
    let text = lab.text(&[]);
    let machine = format!("\nAbout this machine, kept out of files Git shares:\n- {GLOB_RULE}\n");
    contains(&text, &machine);
    contains(
        &text,
        "Rules about this machine stay out of AGENTS.md, which Git shares",
    );
    contains(&text, "`--include-machine` writes them");
    lab.text(&["--apply"]);
    let agents = lab.read("AGENTS.md");
    assert!(
        agents.contains(MAKE_RULE) && !agents.contains(GLOB_RULE),
        "{agents}"
    );

    // --include-machine writes them, and a later plain run keeps them.
    contains(&lab.text(&["--apply", "--include-machine"]), "(1 added)");
    let agents = lab.read("AGENTS.md");
    contains(&agents, GLOB_RULE);
    contains(&lab.text(&["--apply"]), "is up to date (2 learned rule(s))");
    assert_eq!(lab.read("AGENTS.md"), agents);

    // An ignored file is personal and takes machine rules.
    let lab = git_lab();
    lab.write(".gitignore", "CLAUDE.md\n");
    lab.write("AGENTS.md", "# Agents\n");
    lab.write("CLAUDE.md", "# Claude\n");
    let report = lab.json(&["--apply"]);
    assert_eq!(len(&report["rules"]), 2, "{report:#}");
    assert_eq!(report["held"], json!([]), "{report:#}");
    assert!(!lab.read("AGENTS.md").contains(GLOB_RULE));
    let claude = lab.read("CLAUDE.md");
    assert!(
        claude.contains(GLOB_RULE) && claude.contains(MAKE_RULE),
        "{claude}"
    );
}

#[test]
fn learn_rule_text_injection() {
    let lab = Lab::new();
    let existing = "# P\n\n<!-- justless:learned:begin -->\n## Learned command rules\n\n- Mine.\n<!-- justless:learned:end -->\n";
    lab.write("AGENTS.md", existing);
    lab.write("package.json", r#"{"scripts": {"x-->y": "true"}}"#);
    let script = [
        "npm test => 1: npm error Missing script: \"test\"",
        "npm run 'x-->y' => ok",
        "make lint => 2: make: *** No rule to make target `lint'.  Stop.",
        "make '<!--x' => ok",
    ];
    lab.sessions("i", 2, &script);
    let report = lab.json(&["--apply"]);
    assert_eq!(report["rules"], json!([]));
    let unsafe_text: Vec<&Value> = held(&report, "unknown_target");
    assert_eq!(unsafe_text.len(), 2, "{report:#}");
    assert!(
        unsafe_text
            .iter()
            .all(|held| held["reason"] == "unsafe_text" && held["text"].is_null())
    );
    assert_eq!(lab.read("AGENTS.md"), existing);
    assert!(!report.to_string().contains("x-->y"));
}

#[test]
fn learn_json_shape_and_determinism() {
    let lab = Lab::new();
    lab.sessions("g", 2, &GLOB_CALLS);
    lab.sessions("h", 2, &GH_CALLS);
    missing(&lab, "go", 1);
    missing(&lab, "pip", 1);
    let first = lab.text(&["--json"]);
    assert_eq!(lab.text(&["--json"]), first);
    let report: Value = serde_json::from_str(&first).unwrap();
    let keys = |value: &Value| {
        value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    for (value, expected) in [
        (
            &report,
            "days min_sessions sources rules held skipped notes apply",
        ),
        (
            &report["sources"]["transcripts"],
            "available directory files sessions threads results not_run status_unknown",
        ),
        (
            &report["sources"]["runs"],
            "available scanned without_command unreadable linked covered_by_transcripts capped",
        ),
        (
            &report["rules"][0],
            "id class scope text failures corrections sessions threads failure_bytes first_seen last_seen score check example evidence",
        ),
        (
            &report["held"][0],
            "id class scope text sessions threads failures reason",
        ),
        (
            &report["apply"],
            "applied targets changed skipped added removed",
        ),
    ] {
        assert_eq!(keys(value), words(expected), "{value}");
    }
    let rule = &report["rules"][0];
    assert_eq!(rule["id"].as_str().unwrap().len(), 12);
    assert!(rule["score"].as_f64().unwrap() > 0.0);
    assert_eq!(rule["last_seen"], timestamp(now_ms() - DAY)[..10]);
    assert_eq!(rule["evidence"][0]["source"], "transcript");
    assert_eq!(len(&report["held"]), 2);
    let limited = lab.json(&["--limit", "1"]);
    assert_eq!(len(&limited["rules"]), 1);
    assert_eq!(len(&limited["held"]), 1);
    assert_eq!(limited["skipped"]["over_limit"], 1);
    for flags in ["--limit 0", "--limit 33", "--days 0", "--min-sessions 0"] {
        let output = lab.learn(&words(flags));
        assert_eq!(output.status.code(), Some(2), "{flags}");
    }
}

#[test]
fn learn_without_sources() {
    let lab = Lab::new();
    fs::remove_dir_all(lab.transcripts()).unwrap();
    let text = lab.text(&[]);
    assert!(
        text.starts_with("No repeated command mistakes in the last 30 day(s): 0 Claude Code session(s), 0 shell results, 0 archived run(s).")
            && text.contains("Claude Code transcripts: none under"),
        "{text}"
    );
    assert!(!lab.root.join(".justless").exists());

    // Without HOME or CLAUDE_CONFIG_DIR the archive is still read.
    fake_make(&lab);
    lab.run("", "-- make test");
    lab.run("server", "-- make test");
    let output = lab
        .command("", &["learn", "--json"])
        .env_remove("HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["sources"]["transcripts"]["available"], false);
    assert_eq!(report["sources"]["transcripts"]["directory"], Value::Null);
    assert_eq!(len(&report["rules"]), 1);

    // An unreadable archive still lets the transcripts report print.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let runs = lab.root.join(".justless/runs");
        fs::set_permissions(&runs, fs::Permissions::from_mode(0o000)).unwrap();
        fs::create_dir_all(lab.transcripts()).unwrap();
        lab.sessions("g", 2, &GLOB_CALLS);
        let output = lab.learn(&[]);
        fs::set_permissions(&runs, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        contains(&stdout(&output), GLOB_RULE);
    }
}

#[test]
fn learn_archive_withholds_credentials() {
    let lab = Lab::new();
    lab.tool("tool", "exit 1");
    let record = |args: &[&str]| {
        let mut all = vec!["--"];
        all.extend(args);
        lab.record(&run_id(&lab.run_argv("", &all)))
    };
    for args in [
        &[
            "tool",
            "-c",
            "http.extraHeader=Authorization: Bearer quux-value",
            "--version",
        ][..],
        &["tool", "publish", "--token", "quux-value"],
        &["tool", "clone", "https://user:quux-value@example.invalid/x"],
        // A shell script is screened word by word, like a typed command.
        &["/bin/sh", "-c", "true && API_TOKEN=quux-value tool"],
        &["/bin/sh", "-c", "tool publish --token quux-value"],
        &["/bin/sh", "-c", "true && ssh host make quux-value"],
    ] {
        let record = record(args);
        assert_eq!(record["argv"], json!([]), "{args:?}");
        assert_eq!(record["argv_withheld"], true, "{args:?}");
        assert!(!record.to_string().contains("quux-value"), "{record:#}");
    }
    for args in [
        &["tool", "build"][..],
        &["tool", "install", "left-pad@latest"],
        &["/bin/sh", "-c", "tool build && tool test"],
    ] {
        let record = record(args);
        assert_eq!(record["argv"], json!(args), "{record:#}");
        assert!(record.get("argv_withheld").is_none(), "{record:#}");
    }

    let report = lab.json(&[]);
    assert_eq!(report["skipped"]["credentials"], 6, "{report:#}");
    assert_eq!(report["sources"]["runs"]["without_command"], 0);
    contains(&lab.text(&[]), "6 dropped for a possible credential");
}

/// `cargo test` output when a test fails, captured from a scratch crate (exit 101).
const CARGO_FAILED: &str = "running 1 test\ntest tests::it_works ... FAILED\n\nfailures:\n\n---- tests::it_works stdout ----\n\nthread 'tests::it_works' (3914681) panicked at src/lib.rs:5:9:\nassertion `left == right` failed\n  left: 2\n right: 3\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\nfailures:\n    tests::it_works\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n   Compiling scratch v0.1.0 (/x/scratch)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.11s\n     Running unittests src/lib.rs (target/debug/deps/scratch-87369008df97ec58)\nerror: test failed, to rerun pass `--lib`";

#[test]
fn learn_test_failures_are_not_missing_options() {
    let lab = Lab::new();
    let lib = format!("cargo test => 101: {CARGO_FAILED}");
    let cli = lib.replace("`--lib`", "`--test cli`");
    let script: [&str; 6] = [
        &lib,
        "cargo test --lib => test result: ok. 1 passed",
        &cli,
        "cargo test --test cli => test result: ok. 1 passed",
        "npm test => 1: > x@1.0.0 test\n> vitest run\n\n FAIL  api.test.ts\nTests  1 failed | 3 passed\nTry --workspace to run one package",
        "npm test --workspace=api => Tests  3 passed",
    ];
    lab.sessions("t", 2, &script);
    let report = lab.json(&["--min-sessions", "1"]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    assert_eq!(report["held"], json!([]), "{report:#}");

    // A failure that names the flag its correction adds is still learned.
    let lab = Lab::new();
    let script = [
        "git push => 128: fatal: The current branch feature has no upstream branch.\nTo push the current branch and set the remote as upstream, use\n\n    git push --set-upstream origin feature\n",
        "git push --set-upstream origin feature => done",
    ];
    lab.sessions("p", 2, &script);
    rule(&lab.rules(&[]), "Pass `--set-upstream` to `git push`.");
}

#[test]
fn learn_self_evident_privacy_and_directory() {
    let lab = Lab::new();
    let script = [
        "ssh devbox kubectl get pods => 127: bash: kubectl: command not found",
        "API_TOKEN=s3cr3t rgx foo => 127: zsh:1: command not found: rgx",
        "cd /srv/other/app && ./gradlew build => 127: zsh:1: no such file or directory: ./gradlew",
        // Opaque within the root: the directory that ran ./mvnw is unknown.
        "D=sub\ncd $D && ./mvnw test => 127: (eval):2: no such file or directory: ./mvnw",
    ];
    lab.sessions("s", 2, &script);
    let report = lab.json(&["--min-sessions", "1"]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    assert_eq!(report["held"], json!([]), "{report:#}");
    assert_eq!(report["skipped"]["credentials"], 4, "{report:#}");

    // A parsed command keeps its directory.
    let lab = Lab::new();
    let script =
        ["cd server && ./gradlew build => 127: zsh:1: no such file or directory: ./gradlew"];
    lab.sessions("r", 2, &script);
    let rules = lab.rules(&[]);
    let text = "`./gradlew` does not exist here; check the path before running it.";
    assert_eq!(
        rule(&rules, text)["check"]["detail"],
        "./gradlew is missing"
    );
    lab.write("server/gradlew", "#!/bin/sh\n");
    assert_eq!(reason(&lab.json(&[]), "missing_program"), "stale");
}

#[test]
fn learn_never_crashes_on_bare_programs() {
    let lines = [
        "make: *** No targets specified and no makefile found.  Stop.",
        "make: *** No rule to make target `lint'.  Stop.",
        "error: could not find `Cargo.toml` in `/x` or any parent directory",
        "npm error code ENOENT\nnpm error path /x/package.json",
        "error: No `pyproject.toml` found in current directory or any parent directory",
        "error: package ID specification `core` did not match any packages",
        "error: no test target named `api`.",
        "error: no such command: `nextest`",
        "git: 'stauts' is not a git command. See 'git --help'.",
        "npm error Missing script: \"test\"",
        "error: unexpected argument '--nope' found",
        "error: unknown option `ammend'",
        "head: invalid option -- 'z'",
        "tool.py: error: the following arguments are required: --x",
        "ERROR: file or directory not found: tests/test_api.py",
        "Could not find 'tests/x.test.js'",
        "npm error Unsupported URL Type \"workspace:\": workspace:*",
        "Usage Error: This project is configured to use pnpm because /x/package.json has a \"packageManager\" field",
        "zsh:1: command not found: x",
        "zsh:1: no such file or directory: ./x",
        "/usr/bin/python3: No module named pytest",
    ];
    let lab = Lab::new();
    let programs = "npm,cargo,git,make,pytest,node,yarn,timeout 5,env,python3,./x";
    for (index, program) in programs.split(',').enumerate() {
        let mut calls = Vec::new();
        for line in lines {
            calls.push(call(program, fail(1, line)));
            calls.push(call("pnpm", Outcome::Ok("ok".into())));
            calls.push(call(program, fail(2, line)));
            calls.push(call(&format!("{program} x"), Outcome::Ok("ok".into())));
        }
        let session = format!("b{index}");
        lab.transcript(&format!("{session}.jsonl"), &session, now_ms() - DAY, calls);
    }
    for args in [&[][..], &["--min-sessions", "1"]] {
        let output = lab.learn(args);
        assert!(output.status.success(), "{}", stderr(&output));
    }
}

#[test]
fn learn_counts_only_error_lines_of_lists() {
    let lab = Lab::new();
    let output = format!("{GLOB}\n{}", "x".repeat(4000));
    let result = json!({"stdout": output, "interrupted": false});
    let list = "grep -rn needle --include=*.rs .; cat notes.txt";
    let outcome = Outcome::Raw(output.clone(), false, result);
    lab.sessions_of("g", 2, &[call(list, outcome)]);
    lab.sessions("h", 2, &GH_CALLS);
    let rules = lab.rules(&[]);
    let glob = rule(&rules, GLOB_RULE);
    assert_eq!(glob["failure_bytes"], 2 * (GLOB.len() + 1));
    let gh = "Exit code 127\nzsh:1: command not found: gh";
    assert_eq!(rule(&rules, GH_RULE)["failure_bytes"], 2 * gh.len());

    // Transcript evidence points at the thread file and line.
    let path = lab.transcripts().join("g0.jsonl");
    let evidence = format!("   evidence: {}:2\n", path.display());
    contains(&lab.text(&[]), &evidence);
}

/// Runs that do not hold a command line the agent typed are left out: a run whose
/// Justless was killed keeps the record written before its command started, with no
/// command line or exit status, and is not mistaken for a run recorded before command
/// capture; and Claude Code's Bash post-command hook records a result after the
/// command ran, with the whole command line's status, such as a pipe's 0 after a
/// failing command.
#[test]
fn learn_skips_runs_left_unfinished_or_recorded_after_the_command_ran() {
    use std::io::Write;
    let lab = Lab::new();
    lab.tool("tool", "exit 1");
    let id = run_id(&lab.run("", "-- tool build"));
    let mut record = lab.record(&id);
    record["unfinished"] = json!(true);
    record["status"] = json!(-1);
    record["argv"] = json!([]);
    lab.write(
        &format!(".justless/runs/{id}/record.json"),
        &record.to_string(),
    );
    let report = lab.json(&[]);
    let runs = &report["sources"]["runs"];
    assert_eq!(counts(runs, "scanned without_command"), [0, 0]);
    assert!(!lab.text(&[]).contains("predate command capture"));

    let output: String = (0..400)
        .map(|i| format!("src/a{}.rs:{i}:let needle = {i};\n", i % 7))
        .collect();
    let event = json!({"session_id":"s1","hook_event_name":"PostToolUse","tool_name":"Bash",
        "cwd":lab.root,"tool_input":{"command":"grep -rn needle src | head -400"},
        "tool_response":{"stdout":output,"stderr":"","interrupted":false,"isImage":false}});
    let mut child = lab
        .command("", &["hook-claude", "after-bash"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(event.to_string().as_bytes()).unwrap();
    drop(stdin);
    let answer: Value = serde_json::from_slice(&child.wait_with_output().unwrap().stdout).unwrap();
    let updated = &answer["hookSpecificOutput"]["updatedToolOutput"];
    assert!(updated.is_object(), "{answer}");
    assert_eq!(lab.json(&[])["sources"]["runs"]["scanned"], 0);
}

/// `justless exec` records a script the agent wrote under a key such as
/// `exec sh -- ARGS`, which is not a command line, so learn leaves its runs out.
#[cfg(unix)]
#[test]
fn learn_skips_exec_runs() {
    let lab = Lab::new();
    std::os::unix::fs::symlink("/bin/sh", lab.bin.join("sh")).unwrap();
    let script = "echo 'sh: nope: command not found' >&2; exit 127";
    let output = lab
        .command("", &["exec", "--lang", "sh", script])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(127), "{}", stderr(&output));
    let report = lab.json(&[]);
    assert_eq!(report["sources"]["runs"]["scanned"], 0, "{report:#}");
}

#[test]
fn learn_reads_codex_rollouts() {
    let lab = Lab::new();
    lab.tool("python3", "exit 0");
    // One Codex thread: a direct call fails, and a code-mode script's call corrects it.
    let failed = "Chunk ID: a\nWall time: 0.1 seconds\nProcess exited with code 127\nOutput:\nzsh:1: command not found: python\n";
    let records = [
        json!({"type": "session_meta", "payload": {"id": "t1", "session_id": "r1", "cwd": lab.root}}),
        json!({"type": "response_item", "payload": {"type": "function_call", "name": "exec_command",
            "call_id": "call_1", "arguments": json!({"cmd": "python scripts/gen.py"}).to_string()}}),
        json!({"type": "response_item", "payload": {"type": "function_call_output",
            "call_id": "call_1", "output": failed}}),
        json!({"type": "event_msg", "payload": {"type": "item_completed", "item": {
            "type": "CommandExecution", "id": "exec-1", "exit_code": 0,
            "command": ["/bin/zsh", "-lc", "python3 scripts/gen.py"],
            "cwd": format!("file://{}", lab.root.display()), "aggregated_output": "generated\n"}}}),
    ];
    let at = timestamp(now_ms() - 30 * MINUTE);
    let mut body = String::new();
    for mut record in records {
        record["timestamp"] = json!(at);
        body.push_str(&format!("{record}\n"));
    }
    let dir = lab.base.join("home/.codex/sessions/2026/11/19");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("rollout-2026-11-19T10-00-00-t1.jsonl"), body).unwrap();
    let report = lab.json(&[]);
    assert_eq!(rule(&report["rules"], PYTHON_RULE)["corrections"], 1);
    assert_eq!(report["sources"]["codex_rollouts"]["results"], 2);
    // Codex reads AGENTS.md, so a rule from its rollouts does not create CLAUDE.md.
    assert_eq!(report["apply"]["targets"], json!(["AGENTS.md"]));
    let text = lab.text(&[]);
    contains(&text, "Also read 1 Codex session(s) (2 shell results).");
}

#[test]
fn learn_reads_antigravity_cli_transcripts() {
    let lab = Lab::new();
    lab.tool("python3", "exit 0");
    let at = timestamp(now_ms() - 30 * MINUTE);
    let agy_home = lab.base.join("home/.gemini/antigravity-cli");
    let logs = agy_home.join("brain/conv-1/.system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(
        agy_home.join("history.jsonl"),
        format!(
            "{}\n",
            json!({"conversationId": "conv-1", "workspace": lab.root})
        ),
    )
    .unwrap();
    // One Antigravity CLI session: a direct call fails, and a backgrounded call
    // finishes in a SYSTEM_MESSAGE and corrects it.
    let failed = concat!(
        "Created At: 2026-11-25T00:05:32+05:45\n",
        "Completed At: 2026-11-25T00:05:33+05:45\n",
        "Command exited with code 127.\n\n",
        "Command output:\n",
        "bash: python: command not found\n",
    );
    let bg_started = concat!(
        "Created At: 2026-11-25T00:06:00+05:45\n",
        "Tool is running as a background task with task id: conv-1/task-2\n",
        "Task logs are available at: file:///tmp/task-2.log\n",
    );
    let bg_done = concat!(
        "<SYSTEM_MESSAGE>\n",
        "[Message] timestamp=2026-11-25T00:06:05Z sender=conv-1/task-2 content=Task id \"conv-1/task-2\" finished with result:\n",
        "Command exited with code 0.\n\n",
        "Command output:\n",
        "generated\n",
        "Log: file:///tmp/task-2.log\n",
        "</SYSTEM_MESSAGE>\n",
    );
    let records = [
        json!({
            "type": "PLANNER_RESPONSE",
            "created_at": at,
            "tool_calls": [{"name": "run_command", "args": {"CommandLine": "python scripts/gen.py"}}],
        }),
        json!({"type": "GENERIC", "created_at": at, "content": failed}),
        json!({
            "type": "PLANNER_RESPONSE",
            "created_at": at,
            "tool_calls": [{"name": "run_command", "args": {"CommandLine": "python3 scripts/gen.py"}}],
        }),
        json!({"type": "GENERIC", "created_at": at, "content": bg_started}),
        json!({"type": "SYSTEM_MESSAGE", "created_at": at, "content": bg_done}),
    ];
    let mut body = String::new();
    for record in records {
        body.push_str(&format!("{record}\n"));
    }
    fs::write(logs.join("transcript_full.jsonl"), body).unwrap();
    let report = lab.json(&[]);
    assert_eq!(rule(&report["rules"], PYTHON_RULE)["corrections"], 1);
    assert_eq!(report["sources"]["agy_sessions"]["results"], 2);
    // Antigravity CLI reads AGENTS.md, so a rule from its transcripts does not create CLAUDE.md.
    assert_eq!(report["apply"]["targets"], json!(["AGENTS.md"]));
    let text = lab.text(&[]);
    contains(
        &text,
        "Also read 1 Antigravity CLI session(s) (2 shell results).",
    );
}
