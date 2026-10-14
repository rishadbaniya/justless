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

fn run_id(output: &Output) -> String {
    let text = stderr(output);
    let start = text.find("run-").expect("run footer");
    text[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

enum Outcome {
    Ok(&'static str),
    Fail(i32, &'static str),
    /// A nonzero exit Claude Code shows as a non-error.
    Unknown(&'static str),
    /// Raw `content`, `is_error` and `toolUseResult`.
    Raw(String, bool, Value),
}

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

fn call_in(dir: &str, command: &str, outcome: Outcome) -> Call {
    Call {
        cwd: dir.into(),
        ..call(command, outcome)
    }
}

fn failure(code: i32, output: impl Into<String>) -> Outcome {
    let output = output.into();
    Outcome::Raw(
        format!("Exit code {code}\n{output}"),
        true,
        json!(format!("Error: Exit code {code}\n{output}")),
    )
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

    fn rules(&self, args: &[&str]) -> Vec<Value> {
        self.json(args)["rules"].as_array().unwrap().clone()
    }

    fn run(&self, dir: &str, args: &[&str]) -> Output {
        let mut all = vec!["run"];
        all.extend(args);
        self.command(dir, &all).output().unwrap()
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
                Outcome::Ok(output) => (
                    output.to_owned(),
                    false,
                    json!({"stdout": output, "stderr": "", "interrupted": false, "isImage": false}),
                ),
                Outcome::Fail(code, output) => (
                    format!("Exit code {code}\n{output}"),
                    true,
                    json!(format!("Error: Exit code {code}\n{output}")),
                ),
                Outcome::Unknown(output) => (
                    output.to_owned(),
                    false,
                    json!({"stdout": output, "stderr": "", "interrupted": false,
                        "returnCodeInterpretation": "No matches found"}),
                ),
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

    /// The same calls in `count` separate sessions, a day apart.
    fn sessions(&self, prefix: &str, count: usize, calls: impl Fn() -> Vec<Call>) {
        for index in 0..count {
            let session = format!("{prefix}{index}");
            let start = now_ms() - (index as u64 + 1) * DAY;
            self.transcript(&format!("{session}.jsonl"), &session, start, calls());
        }
    }
}

fn rule<'a>(rules: &'a [Value], text: &str) -> &'a Value {
    rules
        .iter()
        .find(|rule| rule["text"] == text)
        .unwrap_or_else(|| panic!("no rule {text:?} in {rules:#?}"))
}

fn held<'a>(report: &'a Value, class: &str) -> Vec<&'a Value> {
    report["held"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|held| held["class"] == class)
        .collect()
}

const GLOB: &str = "(eval):1: no matches found: --include=*.rs";
const STATE: &str = ".justless/learned.json";

fn glob_calls() -> Vec<Call> {
    vec![call(
        "grep -rn needle --include=*.rs .; echo done",
        Outcome::Ok("(eval):1: no matches found: --include=*.rs\ndone"),
    )]
}

const GLOB_RULE: &str = "Quote glob patterns in shell arguments, as in `--include='*.rs'`; zsh stops a command at an unmatched glob.";
const GH_RULE: &str = "`gh` is not installed on this machine.";

fn gh_calls() -> Vec<Call> {
    vec![call(
        "gh pr list",
        Outcome::Fail(127, "zsh:1: command not found: gh"),
    )]
}

#[test]
fn learn_command_not_found_alternative() {
    let lab = Lab::new();
    lab.tool("python3", "exit 0");
    lab.transcript(
        "s1.jsonl",
        "s1",
        now_ms() - 30 * MINUTE,
        vec![
            call(
                "python scripts/gen.py",
                Outcome::Fail(127, "(eval):1: command not found: python"),
            ),
            call("python3 scripts/gen.py", Outcome::Ok("generated")),
        ],
    );
    let rules = lab.rules(&[]);
    let found = rule(
        &rules,
        "Use `python3`, not `python`; `python` is not installed on this machine.",
    );
    assert_eq!(found["class"], "missing_program");
    assert_eq!(
        (found["corrections"].as_u64(), found["sessions"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(found["scope"], "machine");
    assert_eq!(found["check"]["state"], "verified");
    assert_eq!(found["example"], Value::Null);
    let text = lab.text(&[]);
    assert!(
        text.contains("missing program · machine · 1 failure(s), 1 correction(s) in 1 session(s)")
            && text.contains("checked: not on PATH")
            && !text.contains("scripts/gen.py"),
        "{text}"
    );

    // Installing the program makes the rule stale.
    lab.tool("python", "exit 0");
    let report = lab.json(&[]);
    assert!(report["rules"].as_array().unwrap().is_empty(), "{report:#}");
    assert_eq!(held(&report, "missing_program")[0]["reason"], "stale");
    assert_eq!(report["skipped"]["stale"], 1);

    // A chain that exits 0 but could not run the alternative is no correction.
    let lab = Lab::new();
    lab.tool("python3", "exit 0");
    lab.transcript(
        "s1.jsonl",
        "s1",
        now_ms() - 30 * MINUTE,
        vec![
            call(
                "python y.py",
                Outcome::Fail(127, "zsh:1: command not found: python"),
            ),
            call(
                "python3 y.py || echo failed",
                Outcome::Ok("zsh:1: command not found: python3\nfailed"),
            ),
        ],
    );
    let report = lab.json(&[]);
    assert!(report["rules"].as_array().unwrap().is_empty(), "{report:#}");
    let python = held(&report, "missing_program")
        .into_iter()
        .find(|held| held["text"] == "`python` is not installed on this machine.")
        .unwrap();
    assert_eq!(python["reason"], "below_min_sessions");
}

#[test]
fn learn_missing_program_needs_two_sessions() {
    let lab = Lab::new();
    lab.sessions("a", 1, gh_calls);
    let report = lab.json(&[]);
    assert!(report["rules"].as_array().unwrap().is_empty());
    assert_eq!(
        held(&report, "missing_program")[0]["reason"],
        "below_min_sessions"
    );
    assert!(
        lab.text(&[])
            .contains("1 below --min-sessions (--min-sessions 1 lists them)")
    );
    assert_eq!(lab.rules(&["--min-sessions", "1"])[0]["text"], GH_RULE);

    lab.sessions("b", 1, gh_calls);
    // A script that calls gh does not name it in its own command.
    lab.transcript(
        "c.jsonl",
        "c",
        now_ms() - 3 * DAY,
        vec![call(
            "./build.sh",
            Outcome::Fail(127, "bash: line 3: gh: command not found"),
        )],
    );
    let rules = lab.rules(&[]);
    let found = rule(&rules, GH_RULE);
    assert_eq!(
        (found["failures"].as_u64(), found["sessions"].as_u64()),
        (Some(2), Some(2))
    );
    assert_eq!(found["check"]["detail"], "not on PATH");

    // An exit-0 list does not contradict the rule; a plain success does.
    lab.transcript(
        "d.jsonl",
        "d",
        now_ms() - MINUTE,
        vec![call(
            "gh --version; true",
            Outcome::Ok("zsh:1: command not found: gh"),
        )],
    );
    assert_eq!(lab.rules(&[])[0]["text"], GH_RULE);
    lab.transcript(
        "e.jsonl",
        "e",
        now_ms() - MINUTE / 2,
        vec![call("gh --version", Outcome::Ok("gh version 2.0"))],
    );
    let report = lab.json(&[]);
    assert!(report["rules"].as_array().unwrap().is_empty(), "{report:#}");
    assert_eq!(
        held(&report, "missing_program")[0]["reason"],
        "contradicted"
    );
    assert_eq!(report["skipped"]["contradicted"], 1);
}

#[test]
fn learn_zsh_shell_rules() {
    let mut lab = Lab::new();
    lab.sessions("g", 2, glob_calls);
    // Displayed text is not the command's own error.
    lab.transcript(
        "notes.jsonl",
        "notes",
        now_ms() - 3 * DAY,
        vec![call("cat notes.txt", Outcome::Ok(GLOB))],
    );
    let rules = lab.rules(&[]);
    let found = rule(&rules, GLOB_RULE);
    assert_eq!(found["class"], "zsh_glob");
    assert_eq!(found["failures"], 2);
    assert_eq!(
        found["example"],
        json!({"wrong": "--include=*.rs", "right": "--include='*.rs'", "signal": "zsh: no matches found"})
    );
    assert_eq!(found["check"]["detail"], "SHELL is zsh");

    lab.shell = "/bin/bash".into();
    let report = lab.json(&[]);
    assert!(report["rules"].as_array().unwrap().is_empty());
    assert_eq!(held(&report, "zsh_glob")[0]["reason"], "stale");

    let lab = Lab::new();
    lab.sessions("q", 2, || {
        vec![
            call("echo ===== done", Outcome::Fail(1, "zsh:1: ==== not found")),
            // Quoted, the word is not expanded, so the same line is not its error.
            call(
                "echo '=====' && cat log",
                Outcome::Fail(1, "zsh:1: ==== not found"),
            ),
        ]
    });
    let rules = lab.rules(&[]);
    let found = rule(
        &rules,
        "Quote shell words that start with `=`, as in `echo '=== done'`; zsh expands them as commands.",
    );
    assert_eq!(found["failures"], 2);

    // Three agent threads of one session are still one session.
    let lab = Lab::new();
    for agent in ["a", "b", "c"] {
        lab.transcript(
            &format!("s1/subagents/agent-{agent}.jsonl"),
            "s1",
            now_ms() - DAY,
            glob_calls(),
        );
    }
    let report = lab.json(&[]);
    let glob = &held(&report, "zsh_glob")[0];
    assert_eq!(
        (glob["threads"].as_u64(), glob["sessions"].as_u64()),
        (Some(3), Some(1))
    );
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
    let failed = lab.run("", &["--session", "s1", "--", "make", "test"]);
    assert_eq!(failed.status.code(), Some(2));
    let id = run_id(&failed);
    assert!(
        lab.run("server", &["--session", "s1", "--", "make", "test"])
            .status
            .success()
    );
    let record: Value =
        serde_json::from_str(&lab.read(&format!(".justless/runs/{id}/record.json"))).unwrap();
    assert_eq!(
        (&record["argv"], &record["cwd"], &record["session"]),
        (&json!(["make", "test"]), &json!(""), &json!("s1"))
    );
    let runs: Vec<Value> = fs::read_dir(lab.root.join(".justless/runs"))
        .unwrap()
        .map(|entry| {
            serde_json::from_str(
                &fs::read_to_string(entry.unwrap().path().join("record.json")).unwrap(),
            )
            .unwrap()
        })
        .collect();
    assert!(runs.iter().any(|run| run["cwd"] == "server"));

    let report = lab.json(&[]);
    let found = rule(
        report["rules"].as_array().unwrap(),
        "Run `make` from `server/`; the repository root has no Makefile.",
    );
    assert_eq!(found["scope"], "project");
    assert_eq!(
        found["check"],
        json!({"state": "verified", "detail": "server/Makefile"})
    );
    assert_eq!(
        found["evidence"][0],
        json!({"source": "run", "id": id, "stream": "stderr"})
    );
    assert_eq!(
        found["example"],
        json!({"wrong": "make test", "right": "make test (in server/)", "signal": "make: no makefile"})
    );
    assert_eq!(report["sources"]["runs"]["scanned"], 2);
    assert_eq!(report["sources"]["transcripts"]["results"], 0);
    let text = lab.text(&[]);
    assert!(
        text.contains(&format!("   evidence: justless log {id} --stream stderr\n"))
            && text.contains(
                "Preview only. `justless learn --apply` writes these rules to AGENTS.md."
            ),
        "{text}"
    );

    lab.write("Makefile", "test:\n");
    let report = lab.json(&[]);
    assert!(report["rules"].as_array().unwrap().is_empty());
    assert_eq!(held(&report, "wrong_directory")[0]["reason"], "stale");
}

#[test]
fn learn_wrong_directory_normalizes_commands() {
    let lab = Lab::new();
    lab.write("server/Makefile", "test:\n");
    let missing = "make: *** No rule to make target `test'.  Stop.";
    lab.transcript(
        "a.jsonl",
        "a",
        now_ms() - 2 * DAY,
        vec![
            call("make test", Outcome::Fail(2, missing)),
            call("make -C server test", Outcome::Ok("ok")),
        ],
    );
    lab.transcript(
        "b.jsonl",
        "b",
        now_ms() - DAY,
        vec![
            call("make test", Outcome::Fail(2, missing)),
            call("cd server && make test 2>&1 | tail -5", Outcome::Ok("ok")),
        ],
    );
    lab.write(".claude/worktrees/w1/.git", "gitdir: elsewhere\n");
    lab.transcript(
        "c.jsonl",
        "c",
        now_ms() - 2 * MINUTE,
        vec![
            call_in(
                ".claude/worktrees/w1",
                "make test",
                Outcome::Fail(2, missing),
            ),
            call_in(
                ".claude/worktrees/w1/server",
                "make test",
                Outcome::Ok("ok"),
            ),
        ],
    );
    let rules = lab.rules(&[]);
    assert_eq!(rules.len(), 1, "{rules:#?}");
    assert_eq!(
        rules[0]["text"],
        "Run `make` from `server/`; the repository root has no Makefile."
    );
    assert_eq!(
        (
            rules[0]["corrections"].as_u64(),
            rules[0]["sessions"].as_u64()
        ),
        (Some(3), Some(3))
    );
}

#[test]
fn learn_unknown_target() {
    let lab = Lab::new();
    lab.write(
        "package.json",
        r#"{"scripts": {"test:unit": "node --test"}}"#,
    );
    lab.sessions("n", 1, || {
        vec![
            call(
                "npm test",
                Outcome::Fail(1, "npm error Missing script: \"test\""),
            ),
            call("npm run test:unit", Outcome::Ok("ok")),
        ]
    });
    let script = "Run `npm run test:unit`; there is no `test` script.";
    let report = lab.json(&[]);
    assert!(report["rules"].as_array().unwrap().is_empty());
    assert_eq!(held(&report, "unknown_target")[0]["text"], script);
    lab.sessions("m", 1, || {
        vec![
            call(
                "npm test",
                Outcome::Fail(1, "npm error Missing script: \"test\""),
            ),
            call("npm run test:unit", Outcome::Ok("ok")),
        ]
    });
    let rules = lab.rules(&[]);
    assert_eq!(
        rule(&rules, script)["check"],
        json!({"state": "verified", "detail": "package.json scripts"})
    );

    lab.write("Makefile", "check:\n\ttrue\n");
    lab.sessions("k", 2, || {
        vec![
            call(
                "make lint",
                Outcome::Fail(2, "make: *** No rule to make target `lint'.  Stop."),
            ),
            call("make check", Outcome::Ok("ok")),
        ]
    });
    lab.write("crates/core/Cargo.toml", "[package]\nname = \"core-lib\"\n");
    lab.sessions("p", 2, || {
        vec![
            call(
                "cargo test -p core",
                Outcome::Fail(
                    101,
                    "error: package ID specification `core` did not match any packages",
                ),
            ),
            call(
                "cargo test -p core-lib",
                Outcome::Ok("test result: ok. 3 passed"),
            ),
        ]
    });
    let rules = lab.rules(&[]);
    assert_eq!(
        rule(&rules, "Run `make check`; there is no `lint` target.")["check"]["detail"],
        "Makefile"
    );
    assert_eq!(
        rule(
            &rules,
            "Use `cargo test -p core-lib`; there is no package `core`."
        )["check"]["detail"],
        "crates/core/Cargo.toml"
    );
}

#[test]
fn learn_options() {
    let lab = Lab::new();
    lab.sessions("o", 2, || {
        vec![
            call(
                "./tool.py -n 4",
                Outcome::Fail(2, "usage: tool.py [-h] [--last N]\ntool.py: error: unrecognized arguments: -n"),
            ),
            call("./tool.py --last 4", Outcome::Ok("ok")),
            call(
                "./tool.py",
                Outcome::Fail(2, "tool.py: error: the following arguments are required: --x"),
            ),
            call("./tool.py --x 1", Outcome::Ok("ok")),
            call(
                "./cli build --nope",
                Outcome::Fail(2, "error: Found argument '--nope' which wasn't expected, or isn't valid in this context"),
            ),
            call("./cli build", Outcome::Ok("ok")),
            call("git commit --ammend", Outcome::Fail(129, "error: unknown option `ammend'")),
            call("git commit --amend", Outcome::Ok("ok")),
        ]
    });
    let report = lab.json(&[]);
    let rules = report["rules"].as_array().unwrap();
    let option = rule(
        rules,
        "`./tool.py` here does not accept `-n`; use `--last`.",
    );
    assert_eq!(option["scope"], "project");
    assert_eq!(option["check"]["state"], "unverified");
    rule(rules, "Pass `--x` to `./tool.py`.");
    rule(rules, "`./cli build` here does not accept `--nope`.");
    assert_eq!(rules.len(), 3, "{rules:#?}");
    assert_eq!(report["skipped"]["typos"], 2);
    let text = lab.text(&[]);
    assert!(
        text.contains("2 typo(s)") && text.contains("not checked"),
        "{text}"
    );
}

#[test]
fn learn_wrong_test_path() {
    let lab = Lab::new();
    fs::create_dir_all(lab.root.join("api/tests")).unwrap();
    fs::create_dir_all(lab.root.join("src")).unwrap();
    lab.sessions("t", 2, || {
        vec![
            call(
                "pytest tests/test_api.py",
                Outcome::Fail(4, "ERROR: file or directory not found: tests/test_api.py"),
            ),
            call("pytest api/tests/test_api.py", Outcome::Ok("1 passed")),
            // Another file in the same directory is a one-off.
            call(
                "pytest api/tests/test_a.py",
                Outcome::Fail(4, "ERROR: file or directory not found: api/tests/test_a.py"),
            ),
            call("pytest api/tests/test_b.py", Outcome::Ok("1 passed")),
            call(
                "node --test tests/x.test.js",
                Outcome::Fail(1, "Could not find 'tests/x.test.js'"),
            ),
            call("node --test src/x.test.js", Outcome::Ok("# pass 1")),
        ]
    });
    let rules = lab.rules(&[]);
    assert_eq!(rules.len(), 2, "{rules:#?}");
    assert_eq!(
        rule(
            &rules,
            "`pytest` tests are under `api/tests/`, not `tests/`."
        )["check"]["detail"],
        "api/tests/"
    );
    rule(
        &rules,
        "`node --test` tests are under `src/`, not `tests/`.",
    );
    fs::create_dir(lab.root.join("tests")).unwrap();
    assert!(lab.rules(&[]).is_empty());
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
        for args in [
            &["cargo", "test"][..],
            &["cargo", "test", "-p", "core"],
            &["cargo", "test", "x"],
            &["cargo", "test", "x", "--lib"],
            &["pytest"],
            &["pytest", "-k", "api"],
        ] {
            let mut all = vec!["--session", session, "--"];
            all.extend(args);
            lab.run("", &all);
        }
    }
    let rules = lab.rules(&[]);
    assert_eq!(rules.len(), 2, "{rules:#?}");
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
    lab.transcript(
        "a.jsonl",
        "a",
        now_ms() - DAY,
        vec![
            call(
                "npm install",
                Outcome::Fail(1, "npm error code EUNSUPPORTEDPROTOCOL\nnpm error Unsupported URL Type \"workspace:\": workspace:*"),
            ),
            call("pnpm install", Outcome::Ok("done")),
            call(
                "yarn install",
                Outcome::Fail(1, "Usage Error: This project is configured to use pnpm because /x/package.json has a \"packageManager\" field"),
            ),
            call("pnpm install", Outcome::Ok("done")),
        ],
    );
    let rules = lab.rules(&[]);
    let npm = rule(
        &rules,
        "Use `pnpm`, not `npm`; this project has pnpm-lock.yaml.",
    );
    assert_eq!(
        (npm["corrections"].as_u64(), npm["sessions"].as_u64()),
        (Some(1), Some(1))
    );
    rule(
        &rules,
        "Use `pnpm`, not `yarn`; this project has pnpm-lock.yaml.",
    );
    // With npm's lockfile too, the npm rule no longer holds.
    lab.write("package-lock.json", "{}\n");
    let report = lab.json(&[]);
    let rules = report["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 1, "{report:#}");
    assert_eq!(
        rules[0]["text"],
        "Use `pnpm`, not `yarn`; this project has pnpm-lock.yaml."
    );
    let npm = held(&report, "wrong_package_manager");
    assert_eq!((npm.len(), &npm[0]["reason"]), (1, &json!("stale")));
}

#[test]
fn learn_ignores_non_mistakes() {
    let lab = Lab::new();
    let gh = "zsh:1: command not found: gh\n";
    let raw = |content: &str, is_error: bool, result: Value| {
        Outcome::Raw(content.to_owned(), is_error, result)
    };
    lab.sessions("x", 2, || {
        vec![
            call("gh pr list", raw(&format!("Permission to use Bash has been denied. {gh}"), true, json!("Error: denied"))),
            call("gh pr list", raw("The user doesn't want to proceed with this tool use.", true, json!("Error"))),
            call("gh pr list", raw("<tool_use_error>Cancelled</tool_use_error>", true, json!("Error"))),
            call("gh pr list", raw("This command runs in an isolated worktree.", true, json!("Error"))),
            call("gh pr list", raw(gh, false, json!({"stdout": gh, "interrupted": true}))),
            call("gh pr view", raw(gh, false, json!({"stdout": gh, "interrupted": false, "backgroundTaskId": "b1"}))),
            call("gh pr diff", raw(gh, false, json!({"stdout": gh, "interrupted": false, "timedOutAfterMs": 120000}))),
            call("grep foo x", Outcome::Unknown("")),
            call("grep foo x", Outcome::Fail(1, "")),
            call("grep -r foo .", Outcome::Ok("x:foo")),
            call(
                "cargo test",
                Outcome::Fail(101, "running 2 tests\ntest a ... FAILED\ntest result: FAILED. 1 passed; 1 failed"),
            ),
            call("cargo test --release", Outcome::Ok("test result: ok")),
            call(
                "python3 x.py",
                Outcome::Fail(1, "Traceback (most recent call last):\nModuleNotFoundError: No module named 'requests'"),
            ),
            call(
                "justless qury x",
                Outcome::Fail(2, "error: Found argument 'qury' which wasn't expected"),
            ),
            call("justless query x", Outcome::Ok("x")),
        ]
    });
    let report = lab.json(&["--min-sessions", "1"]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    assert_eq!(report["held"], json!([]), "{report:#}");
    let transcripts = &report["sources"]["transcripts"];
    assert_eq!(transcripts["not_run"], 14);
    assert_eq!(transcripts["status_unknown"], 2);
    assert_eq!(transcripts["results"], 16);
}

#[test]
fn learn_transcript_format() {
    let lab = Lab::new();
    lab.sessions("g", 2, glob_calls);
    let summary = |report: Value| {
        let rule = &report["rules"][0];
        (
            report["sources"].clone(),
            rule["id"].clone(),
            rule["failures"].clone(),
            rule["sessions"].clone(),
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
    fs::write(
        &path,
        format!("{blob}\n{user_use}\n{no_cwd}\n{no_cwd_result}\n{original}"),
    )
    .unwrap();
    assert_eq!(summary(lab.json(&[])), baseline);

    // A resumed session replays the same tool_use id: it counts once.
    let mut replay = glob_calls();
    replay[0].id = Some("toolu_g0-jsonl_0".into());
    lab.transcript("g0-resumed.jsonl", "g0", now_ms() - DAY, replay);
    let report = lab.json(&[]);
    assert_eq!(report["sources"]["transcripts"]["results"], 2);
    assert_eq!(report["sources"]["transcripts"]["files"], 3);

    // Workflow subagents are read and share their parent's session.
    lab.transcript(
        "g1/subagents/workflows/wf_1/agent-a.jsonl",
        "g1",
        now_ms() - DAY / 2,
        glob_calls(),
    );
    fs::write(
        lab.transcripts()
            .join("g1/subagents/workflows/wf_1/journal.jsonl"),
        "{\"type\":\"launched\"}\n",
    )
    .unwrap();
    let report = lab.json(&[]);
    let transcripts = &report["sources"]["transcripts"];
    assert_eq!(
        (
            transcripts["files"].as_u64(),
            transcripts["sessions"].as_u64(),
            transcripts["threads"].as_u64()
        ),
        (Some(4), Some(2), Some(3))
    );
    let glob = rule(report["rules"].as_array().unwrap(), GLOB_RULE);
    assert_eq!(
        (glob["sessions"].as_u64(), glob["threads"].as_u64()),
        (Some(2), Some(3))
    );
}

#[test]
fn learn_links_and_dedupes_runs() {
    for form in [
        "[justless {id}] exit 2 · 60 of 60 output bytes · justless log {id}",
        "[justless {id} · exit 2]",
        "[justless {id} · 1.0 KB of 9.0 KB shown · exit 2]",
    ] {
        let lab = Lab::new();
        fake_make(&lab);
        let session = digest("s1");
        let failed = lab.run("", &["--hook", "--session", &session, "--", "make", "test"]);
        let id = run_id(&failed);
        // A hooked success that reached the agent verbatim prints no footer.
        let passed = lab.run(
            "server",
            &["--hook", "--session", &session, "--", "make", "test"],
        );
        assert!(
            passed.status.success() && stderr(&passed).is_empty(),
            "{passed:?}"
        );
        // A run from another client stays.
        lab.run(
            "",
            &["--hook", "--session", "codex-1", "--", "make", "build"],
        );
        let footer = form.replace("{id}", &id);
        lab.transcript(
            "s1.jsonl",
            "s1",
            now_ms() - 10 * MINUTE,
            vec![
                Call {
                    outcome: failure(2, footer),
                    ..call("make test", Outcome::Ok(""))
                },
                call("cd server && make test", Outcome::Ok("built test")),
            ],
        );
        let report = lab.json(&[]);
        let runs = &report["sources"]["runs"];
        assert_eq!(
            (
                runs["scanned"].as_u64(),
                runs["linked"].as_u64(),
                runs["covered_by_transcripts"].as_u64()
            ),
            (Some(3), Some(1), Some(1)),
            "{form}"
        );
        let found = rule(
            report["rules"].as_array().unwrap(),
            "Run `make` from `server/`; the repository root has no Makefile.",
        );
        assert_eq!(
            (found["failures"].as_u64(), found["sessions"].as_u64()),
            (Some(1), Some(1)),
            "{form}"
        );
        assert_eq!(
            found["evidence"][0],
            json!({"source": "run", "id": id, "stream": "stderr"})
        );
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
        let session = format!("c{index}");
        lab.transcript(
            &format!("{session}.jsonl"),
            &session,
            now_ms() - DAY,
            vec![
                Call {
                    outcome: failure(2, "make: *** No rule to make target `test'.  Stop."),
                    ..call(command, Outcome::Ok(""))
                },
                call(&format!("cd server && {command}"), Outcome::Ok("ok")),
            ],
        );
    }
    let report = lab.json(&["--min-sessions", "1"]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    assert_eq!(report["skipped"]["credentials"], secrets.len());
    let text = lab.text(&["--min-sessions", "1"]) + &report.to_string();
    for secret in [
        "abc123",
        "hunter2",
        "example.invalid",
        "ghp_",
        "Zx8Qk",
        "/home/someone",
        "ssh host",
    ] {
        assert!(!text.contains(secret), "{secret}: {text}");
    }
    assert!(
        text.contains("7 dropped for a possible credential"),
        "{text}"
    );
}

#[test]
fn learn_contradiction() {
    let lab = Lab::new();
    lab.write("server/Makefile", "test:\n");
    let episode = || {
        vec![
            call(
                "make test",
                Outcome::Fail(
                    2,
                    "make: *** No targets specified and no makefile found.  Stop.",
                ),
            ),
            call("make -C server test", Outcome::Ok("ok")),
        ]
    };
    lab.transcript("a.jsonl", "a", now_ms() - DAY, episode());
    let text = "Run `make` from `server/`; the repository root has no Makefile.";
    let output = lab.text(&["--apply"]);
    assert!(
        output.contains("Updated CLAUDE.md: 1 learned rule(s) (1 added)."),
        "{output}"
    );
    assert!(lab.read("CLAUDE.md").contains(text));
    lab.transcript(
        "b.jsonl",
        "b",
        now_ms() - MINUTE,
        vec![call("make test", Outcome::Ok("ok"))],
    );
    let report = lab.json(&[]);
    assert_eq!(
        held(&report, "wrong_directory")[0]["reason"],
        "contradicted"
    );
    let output = lab.text(&["--apply", "--target", "claude"]);
    assert!(
        output.contains(
            "Updated CLAUDE.md: 0 learned rule(s) (1 removed: the command later succeeded)."
        ),
        "{output}"
    );
    assert_eq!(lab.read("CLAUDE.md"), "");
}

#[test]
fn learn_apply_targets() {
    let setup = |files: &[(&str, &str)]| {
        let lab = Lab::new();
        lab.sessions("g", 2, glob_calls);
        for (file, content) in files {
            lab.write(file, content);
        }
        lab
    };
    let block = format!(
        "<!-- justless:learned:begin -->\n## Learned command rules\n\n- {GLOB_RULE}\n<!-- justless:learned:end -->\n"
    );

    // (a) CLAUDE.md imports AGENTS.md.
    let lab = setup(&[("AGENTS.md", "# Agents\n"), ("CLAUDE.md", "@AGENTS.md\n")]);
    let report = lab.json(&["--apply"]);
    assert_eq!(report["apply"]["changed"], json!(["AGENTS.md"]));
    assert_eq!(
        report["apply"]["skipped"],
        json!([{"file": "CLAUDE.md", "reason": "imports AGENTS.md"}])
    );
    assert_eq!(lab.read("AGENTS.md"), format!("# Agents\n\n{block}"));
    assert_eq!(lab.read("CLAUDE.md"), "@AGENTS.md\n");

    // (b) A mention in backticks or a fence is not an import. (c) Independent files.
    for claude in ["See `@AGENTS.md`.\n", "```\n@AGENTS.md\n```\n"] {
        let lab = setup(&[("AGENTS.md", "# Agents\n"), ("CLAUDE.md", claude)]);
        let report = lab.json(&["--apply"]);
        assert_eq!(
            report["apply"]["changed"],
            json!(["AGENTS.md", "CLAUDE.md"])
        );
        assert_eq!(lab.read("CLAUDE.md"), format!("{claude}\n{block}"));
    }
    let lab = setup(&[("AGENTS.md", "a\n"), ("GEMINI.md", "g\n")]);
    lab.text(&["--apply"]);
    assert!(lab.read("AGENTS.md").ends_with(&block) && lab.read("GEMINI.md").ends_with(&block));

    // (d) With no file, transcript rules create CLAUDE.md and archive rules AGENTS.md.
    let lab = setup(&[]);
    let output = lab.text(&["--apply"]);
    assert!(output.contains("Updated CLAUDE.md"), "{output}");
    assert_eq!(lab.read("CLAUDE.md"), block);
    let lab = Lab::new();
    fake_make(&lab);
    lab.run("", &["--", "make", "test"]);
    lab.run("server", &["--", "make", "test"]);
    lab.text(&["--apply"]);
    assert!(lab.read("AGENTS.md").contains("Run `make` from `server/`"));
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
        assert_eq!(
            report["apply"]["skipped"],
            json!([{"file": "CLAUDE.md", "reason": "symbolic link"}])
        );
        assert!(
            fs::symlink_metadata(lab.root.join("CLAUDE.md"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(lab.read("AGENTS.md").matches("learned:begin").count(), 1);
        // (g) An explicit symlink target is refused.
        let output = lab.learn(&["--apply", "--target", "claude"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(
            stderr(&output).contains("CLAUDE.md is a symbolic link"),
            "{}",
            stderr(&output)
        );
    }

    // (f) CLAUDE.md is never created beside AGENTS.md.
    let lab = setup(&[("AGENTS.md", "# Agents\n")]);
    let output = lab.learn(&["--apply", "--target", "claude"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output)
            .contains("creating CLAUDE.md would stop Claude Code from reading AGENTS.md"),
        "{}",
        stderr(&output)
    );
    assert!(!lab.root.join("CLAUDE.md").exists() && !lab.root.join(".justless").exists());
    assert_eq!(lab.read("AGENTS.md"), "# Agents\n");
}

#[test]
fn learn_apply_byte_preserving_and_idempotent() {
    let lab = Lab::new();
    lab.sessions("g", 2, glob_calls);
    lab.write("AGENTS.md", "# Project\n\nIntro.\n");
    let init = lab
        .command("", &["init", "--agent", "codex"])
        .output()
        .unwrap();
    assert!(init.status.success(), "{}", stderr(&init));
    let mut original = lab.read("AGENTS.md");
    original.push_str("\n## Notes\n\nKeep this.\n");
    lab.write("AGENTS.md", &original);
    let output = lab.text(&["--apply"]);
    assert!(
        output.contains("Updated AGENTS.md: 1 learned rule(s) (1 added)."),
        "{output}"
    );
    let applied = lab.read("AGENTS.md");
    let block = format!(
        "<!-- justless:learned:begin -->\n## Learned command rules\n\n- {GLOB_RULE}\n<!-- justless:learned:end -->"
    );
    let marker = "<!-- justless:end -->";
    assert_eq!(
        applied,
        original.replacen(marker, &format!("{marker}\n\n{block}"), 1)
    );

    let output = lab.text(&["--apply"]);
    assert!(
        output.contains("AGENTS.md is up to date (1 learned rule(s))."),
        "{output}"
    );
    assert_eq!(lab.read("AGENTS.md"), applied);
    let state = fs::metadata(lab.root.join(".justless/learned.json"))
        .unwrap()
        .modified()
        .unwrap();
    lab.text(&["--apply"]);
    assert_eq!(
        fs::metadata(lab.root.join(".justless/learned.json"))
            .unwrap()
            .modified()
            .unwrap(),
        state
    );
    let init = lab
        .command("", &["init", "--agent", "codex"])
        .output()
        .unwrap();
    assert!(init.status.success());
    assert_eq!(lab.read("AGENTS.md"), applied);

    // CRLF files get a CRLF block, and a file without a final newline is handled.
    for (original, expected) in [
        (
            "# Rules\r\nUse care.\r\n",
            format!(
                "# Rules\r\nUse care.\r\n\r\n{}\r\n",
                block.replace('\n', "\r\n")
            ),
        ),
        ("# Rules", format!("# Rules\n\n{block}\n")),
    ] {
        let lab = Lab::new();
        lab.sessions("g", 2, glob_calls);
        lab.write("AGENTS.md", original);
        lab.text(&["--apply"]);
        assert_eq!(lab.read("AGENTS.md"), expected);
        lab.text(&["--apply"]);
        assert_eq!(lab.read("AGENTS.md"), expected);
    }
}

#[test]
fn learn_apply_stability() {
    let lab = Lab::new();
    lab.write("AGENTS.md", "# Project\n");
    lab.sessions("g", 2, glob_calls);
    lab.sessions("h", 2, gh_calls);
    lab.text(&["--apply"]);
    let first = lab.read("AGENTS.md");
    assert!(
        first.contains(&format!("- {GLOB_RULE}\n- {GH_RULE}\n")),
        "{first}"
    );

    // A new rule that outranks both, and a new most frequent glob example.
    lab.sessions("go", 7, || {
        vec![call(
            "go test ./...",
            Outcome::Fail(127, "zsh:1: command not found: go"),
        )]
    });
    lab.sessions("py", 3, || {
        vec![call(
            "ls *.py",
            Outcome::Fail(1, "zsh:1: no matches found: *.py"),
        )]
    });
    let preview = lab.json(&[]);
    assert_eq!(
        preview["rules"][0]["text"],
        "`go` is not installed on this machine."
    );
    // A written rule keeps its text even when the example changes.
    assert_eq!(
        rule(preview["rules"].as_array().unwrap(), GLOB_RULE)["example"]["right"],
        "'*.py'"
    );

    // Held candidates beyond --limit neither hide a full block nor shrink the counts.
    for program in ["fd", "bat", "jq"] {
        lab.sessions(program, 1, || {
            vec![call(
                &format!("{program} x"),
                failure(127, format!("zsh:1: command not found: {program}")),
            )]
        });
    }
    let report = lab.json(&["--apply", "--limit", "2"]);
    assert_eq!(lab.read("AGENTS.md"), first);
    let held_now = report["held"].as_array().unwrap();
    assert_eq!(held_now.len(), 2, "{report:#}");
    assert_eq!(held_now[0]["reason"], "block_full");
    assert_eq!(
        held_now[0]["text"],
        "`go` is not installed on this machine."
    );
    let text = lab.text(&["--limit", "1"]);
    assert!(
        text.contains("3 below --min-sessions (--min-sessions 1 lists them)"),
        "{text}"
    );

    lab.text(&["--apply"]);
    assert_eq!(
        lab.read("AGENTS.md"),
        first.replace(
            "<!-- justless:learned:end -->",
            "- `go` is not installed on this machine.\n<!-- justless:learned:end -->"
        )
    );
}

#[test]
fn learn_apply_persists_dismisses_expires() {
    let lab = Lab::new();
    let original = "# Project\n";
    lab.write("AGENTS.md", original);
    lab.sessions("g", 2, glob_calls);
    lab.sessions("h", 2, gh_calls);
    lab.text(&["--apply"]);
    let applied = lab.read("AGENTS.md");

    // A reverted file forgets what was written, so the rules come back.
    lab.write("AGENTS.md", original);
    let output = lab.text(&["--apply"]);
    assert!(output.contains("(2 added)"), "{output}");
    assert_eq!(lab.read("AGENTS.md"), applied);

    // Rules whose evidence aged out stay.
    let saved = lab.base.join("saved");
    fs::rename(lab.transcripts(), &saved).unwrap();
    let output = lab.text(&["--apply"]);
    assert!(
        output.contains("AGENTS.md is up to date (2 learned rule(s))."),
        "{output}"
    );
    fs::rename(&saved, lab.transcripts()).unwrap();

    // A machine rule whose check fails stays until the check still fails a day later;
    // then it is removed, and a user line in the block keeps its place.
    lab.tool("gh", "exit 0");
    let output = lab.text(&["--apply"]);
    assert!(
        output.contains("AGENTS.md is up to date (2 learned rule(s)).")
            && output.contains("Kept 1 written rule(s) whose check fails here (gh is on PATH)"),
        "{output}"
    );
    assert_eq!(lab.read("AGENTS.md"), applied);
    lab.backdate();
    let output = lab.text(&["--apply"]);
    assert!(
        output.contains("(1 removed: its check no longer holds)"),
        "{output}"
    );
    let with_note = lab.read("AGENTS.md").replace(
        "<!-- justless:learned:end -->",
        "- Keep this note.\n<!-- justless:learned:end -->",
    );
    lab.write("AGENTS.md", &with_note);
    assert!(lab.text(&["--apply"]).contains("up to date"));
    assert_eq!(lab.read("AGENTS.md"), with_note);

    // Deleting a line dismisses its rule.
    let dismissed = with_note.replace(&format!("- {GLOB_RULE}\n"), "");
    lab.write("AGENTS.md", &dismissed);
    lab.text(&["--apply"]);
    assert_eq!(lab.read("AGENTS.md"), dismissed);
    lab.text(&["--apply"]);
    assert_eq!(lab.read("AGENTS.md"), dismissed);
    let state: Value = serde_json::from_str(&lab.read(".justless/learned.json")).unwrap();
    assert_eq!(state["dismissed"].as_array().unwrap().len(), 1);
    let report = lab.json(&[]);
    assert_eq!(report["skipped"]["dismissed"], 1);

    // Removing the last written rule restores the original bytes.
    let lab = Lab::new();
    lab.write("AGENTS.md", original);
    lab.sessions("g", 2, glob_calls);
    lab.text(&["--apply"]);
    let mut lab = lab;
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
    lab.sessions("h", 3, gh_calls);
    lab.sessions("g", 2, glob_calls);
    lab.text(&["--apply"]);
    let applied = lab.read("AGENTS.md");
    assert!(
        applied.contains(&format!("- {GH_RULE}\n- {GLOB_RULE}\n")),
        "{applied}"
    );

    // A PATH entry or another shell for one run changes nothing in the file.
    lab.tool("gh", "exit 0");
    lab.text(&["--apply"]);
    assert_eq!(lab.read("AGENTS.md"), applied);
    assert!(lab.read(STATE).contains("stale_since_ms"));
    fs::remove_file(lab.bin.join("gh")).unwrap();
    lab.text(&["--apply"]);
    assert_eq!(lab.read("AGENTS.md"), applied);
    assert!(
        !lab.read(STATE).contains("stale_since_ms"),
        "{}",
        lab.read(STATE)
    );
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
    assert_eq!(
        lab.read("AGENTS.md"),
        applied.replace(&format!("- {GH_RULE}\n"), "")
    );
    fs::remove_file(lab.bin.join("gh")).unwrap();
    let output = lab.text(&["--apply"]);
    assert!(output.contains("(1 added)"), "{output}");
    assert_eq!(lab.read("AGENTS.md"), applied);
    lab.shell = "/bin/bash".into();
    lab.text(&["--apply"]);
    lab.backdate();
    lab.text(&["--apply"]);
    assert_eq!(
        lab.read("AGENTS.md"),
        applied.replace(&format!("- {GLOB_RULE}\n"), "")
    );
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
        lab.sessions("g", 2, glob_calls);
        lab.write("AGENTS.md", &broken);
        lab.write("GEMINI.md", "# Gemini\n");
        let output = lab.learn(&["--apply"]);
        assert_eq!(output.status.code(), Some(2), "{broken}");
        assert!(stderr(&output).contains("AGENTS.md"), "{}", stderr(&output));
        assert_eq!(lab.read("AGENTS.md"), broken);
        assert_eq!(lab.read("GEMINI.md"), "# Gemini\n");
        // A preview reports the file and still shows the rules.
        let report = lab.json(&[]);
        let notes = report["notes"].to_string();
        assert!(
            notes.contains("AGENTS.md") && notes.contains("`--apply` stops on it."),
            "{report:#}"
        );
        rule(report["rules"].as_array().unwrap(), GLOB_RULE);
    }
}

#[test]
fn learn_preview_fails_open() {
    let lab = Lab::new();
    lab.sessions("g", 2, glob_calls);
    fs::write(lab.root.join("AGENTS.md"), b"# A\n\xff\xfe\n").unwrap();
    for args in [&[][..], &["--target", "agents"]] {
        let report = lab.json(args);
        assert_eq!(
            report["notes"],
            json!([
                "AGENTS.md is not UTF-8: invalid utf-8 sequence of 1 bytes from index 4; `--apply` stops on it."
            ]),
            "{report:#}"
        );
        rule(report["rules"].as_array().unwrap(), GLOB_RULE);
        let mut apply = vec!["--apply"];
        apply.extend(args);
        let output = lab.learn(&apply);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("AGENTS.md is not UTF-8"));
    }
    assert!(!lab.root.join("CLAUDE.md").exists());

    let lab = Lab::new();
    lab.sessions("g", 2, glob_calls);
    lab.write(STATE, "{not json");
    let report = lab.json(&[]);
    assert!(
        report["notes"][0]
            .as_str()
            .unwrap()
            .starts_with("decode .justless/learned.json"),
        "{report:#}"
    );
    rule(report["rules"].as_array().unwrap(), GLOB_RULE);
    let output = lab.learn(&["--apply"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(lab.read(STATE), "{not json");
}

#[test]
fn learn_counts_corrections_across_nested_checkouts() {
    // The same directory in a worktree is another checkout, not another directory.
    let lab = Lab::new();
    lab.write(".claude/worktrees/w1/.git", "gitdir: elsewhere\n");
    lab.sessions("n", 2, || {
        vec![
            call(
                "make test",
                Outcome::Fail(
                    2,
                    "make: *** No targets specified and no makefile found.  Stop.",
                ),
            ),
            call_in(".claude/worktrees/w1", "make test", Outcome::Ok("ok")),
        ]
    });
    let report = lab.json(&["--min-sessions", "1"]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    assert_eq!(report["skipped"]["nested_checkout"], 2, "{report:#}");
    assert!(
        lab.text(&[]).contains("2 split across nested checkouts"),
        "{}",
        lab.text(&[])
    );
}

#[test]
fn learn_notes_only_with_rules() {
    // Without rules, a CLAUDE.md above the root is not worth a note.
    let lab = Lab::new();
    lab.write("AGENTS.md", "# Agents\n");
    fs::write(lab.base.join("CLAUDE.md"), "# Parent\n").unwrap();
    assert_eq!(lab.json(&[])["notes"], json!([]));

    // A preview names the files it would skip.
    lab.sessions("g", 2, glob_calls);
    lab.write("CLAUDE.md", "@AGENTS.md\n");
    let text = lab.text(&[]);
    assert!(
        text.contains("Skipped CLAUDE.md: it imports AGENTS.md.\n")
            && text.contains(
                "Preview only. `justless learn --apply` writes these rules to AGENTS.md."
            ),
        "{text}"
    );
    lab.text(&["--apply"]);
    assert!(
        lab.text(&[])
            .contains("Preview only. AGENTS.md already up to date."),
        "{}",
        lab.text(&[])
    );
}

#[test]
fn learn_apply_reports_a_full_block() {
    let lab = Lab::new();
    lab.write("AGENTS.md", "# Agents\n");
    for index in 1..=8 {
        lab.sessions(&format!("t{index}-"), 2, || {
            vec![call(
                &format!("tool{index} x"),
                failure(127, format!("zsh:1: command not found: tool{index}")),
            )]
        });
    }
    let output = lab.text(&["--apply", "--target", "agents"]);
    assert!(output.contains("(8 added)"), "{output}");
    let written = lab.read("AGENTS.md");
    lab.sessions("n", 3, || {
        vec![call(
            "newtool x",
            failure(127, "zsh:1: command not found: newtool"),
        )]
    });
    let newtool = "`newtool` is not installed on this machine.";
    for args in [
        &["--target", "agents"][..],
        &["--apply", "--target", "agents"],
    ] {
        let text = lab.text(args);
        assert!(
            text.contains("\nNot written, the block is full:\n- `newtool` is not installed on this machine.\nDelete a rule's line between the markers to dismiss it and make room.\n")
                && !text.contains(&format!(". {newtool}"))
                && !text.contains("beyond --limit")
                && text.contains("8 rule(s)"),
            "{text}"
        );
        let report = lab.json(args);
        let rules = report["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 8, "{report:#}");
        assert!(rules.iter().all(|rule| rule["text"] != newtool));
        assert_eq!(report["held"][0]["text"], newtool);
        assert_eq!(report["held"][0]["reason"], "block_full");
        assert_eq!(report["skipped"]["over_limit"], 0);
    }
    assert!(
        lab.text(&["--apply"])
            .contains("AGENTS.md unchanged: 8 learned rule(s); 1 rule(s) did not fit."),
        "{}",
        lab.text(&["--apply"])
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
    lab
}

fn make_calls() -> Vec<Call> {
    vec![
        call(
            "make test",
            Outcome::Fail(
                2,
                "make: *** No targets specified and no makefile found.  Stop.",
            ),
        ),
        call("make -C server test", Outcome::Ok("ok")),
    ]
}

#[test]
fn learn_keeps_machine_rules_out_of_shared_files() {
    let make_rule = "Run `make` from `server/`; the repository root has no Makefile.";
    let lab = git_lab();
    lab.write("server/Makefile", "test:\n");
    lab.write("AGENTS.md", "# Agents\n");
    lab.sessions("g", 2, glob_calls);
    lab.sessions("m", 2, make_calls);
    let report = lab.json(&[]);
    let rules = report["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 1, "{report:#}");
    assert_eq!(rules[0]["text"], make_rule);
    assert_eq!(held(&report, "zsh_glob")[0]["reason"], "machine_scope");
    let text = lab.text(&[]);
    assert!(
        text.contains(&format!(
            "\nAbout this machine, kept out of files Git shares:\n- {GLOB_RULE}\n"
        )) && text.contains("Rules about this machine stay out of AGENTS.md, which Git shares")
            && text.contains("`--include-machine` writes them"),
        "{text}"
    );
    lab.text(&["--apply"]);
    let agents = lab.read("AGENTS.md");
    assert!(
        agents.contains(make_rule) && !agents.contains(GLOB_RULE),
        "{agents}"
    );

    // --include-machine writes them, and a later plain run keeps them.
    let output = lab.text(&["--apply", "--include-machine"]);
    assert!(output.contains("(1 added)"), "{output}");
    let agents = lab.read("AGENTS.md");
    assert!(agents.contains(GLOB_RULE), "{agents}");
    let output = lab.text(&["--apply"]);
    assert!(
        output.contains("is up to date (2 learned rule(s))"),
        "{output}"
    );
    assert_eq!(lab.read("AGENTS.md"), agents);

    // An ignored file is personal and takes machine rules.
    let lab = git_lab();
    lab.write("server/Makefile", "test:\n");
    lab.write(".gitignore", "CLAUDE.md\n");
    lab.write("AGENTS.md", "# Agents\n");
    lab.write("CLAUDE.md", "# Claude\n");
    lab.sessions("g", 2, glob_calls);
    lab.sessions("m", 2, make_calls);
    let report = lab.json(&["--apply"]);
    assert_eq!(report["rules"].as_array().unwrap().len(), 2, "{report:#}");
    assert_eq!(report["held"], json!([]), "{report:#}");
    assert!(!lab.read("AGENTS.md").contains(GLOB_RULE));
    let claude = lab.read("CLAUDE.md");
    assert!(
        claude.contains(GLOB_RULE) && claude.contains(make_rule),
        "{claude}"
    );
}

#[test]
fn learn_rule_text_injection() {
    let lab = Lab::new();
    let existing = "# P\n\n<!-- justless:learned:begin -->\n## Learned command rules\n\n- Mine.\n<!-- justless:learned:end -->\n";
    lab.write("AGENTS.md", existing);
    lab.write("package.json", r#"{"scripts": {"x-->y": "true"}}"#);
    lab.sessions("i", 2, || {
        vec![
            call(
                "npm test",
                Outcome::Fail(1, "npm error Missing script: \"test\""),
            ),
            call("npm run 'x-->y'", Outcome::Ok("ok")),
            call(
                "make lint",
                Outcome::Fail(2, "make: *** No rule to make target `lint'.  Stop."),
            ),
            call("make '<!--x'", Outcome::Ok("ok")),
        ]
    });
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
    lab.sessions("g", 2, glob_calls);
    lab.sessions("h", 2, gh_calls);
    lab.sessions("o", 1, || {
        vec![call(
            "go version",
            Outcome::Fail(127, "zsh:1: command not found: go"),
        )]
    });
    lab.sessions("p", 1, || {
        vec![call(
            "pip list",
            Outcome::Fail(127, "zsh:1: command not found: pip"),
        )]
    });
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
    assert_eq!(
        keys(&report),
        [
            "days",
            "min_sessions",
            "sources",
            "rules",
            "held",
            "skipped",
            "notes",
            "apply"
        ]
    );
    assert_eq!(
        keys(&report["sources"]["transcripts"]),
        [
            "available",
            "directory",
            "files",
            "sessions",
            "threads",
            "results",
            "not_run",
            "status_unknown"
        ]
    );
    assert_eq!(
        keys(&report["sources"]["runs"]),
        [
            "available",
            "scanned",
            "without_command",
            "unreadable",
            "linked",
            "covered_by_transcripts",
            "capped"
        ]
    );
    let rule = &report["rules"][0];
    assert_eq!(
        keys(rule),
        [
            "id",
            "class",
            "scope",
            "text",
            "failures",
            "corrections",
            "sessions",
            "threads",
            "failure_bytes",
            "first_seen",
            "last_seen",
            "score",
            "check",
            "example",
            "evidence"
        ]
    );
    assert_eq!(rule["id"].as_str().unwrap().len(), 12);
    assert!(rule["score"].as_f64().unwrap() > 0.0);
    assert_eq!(rule["last_seen"], timestamp(now_ms() - DAY)[..10]);
    assert_eq!(rule["evidence"][0]["source"], "transcript");
    assert_eq!(
        keys(&report["held"][0]),
        [
            "id", "class", "scope", "text", "sessions", "threads", "failures", "reason"
        ]
    );
    assert_eq!(
        keys(&report["apply"]),
        [
            "applied", "targets", "changed", "skipped", "added", "removed"
        ]
    );
    assert_eq!(report["held"].as_array().unwrap().len(), 2);
    let limited = lab.json(&["--limit", "1"]);
    assert_eq!(limited["rules"].as_array().unwrap().len(), 1);
    assert_eq!(limited["held"].as_array().unwrap().len(), 1);
    assert_eq!(limited["skipped"]["over_limit"], 1);
    for (flag, value) in [
        ("--limit", "0"),
        ("--limit", "33"),
        ("--days", "0"),
        ("--min-sessions", "0"),
    ] {
        let output = lab.learn(&[flag, value]);
        assert_eq!(output.status.code(), Some(2), "{flag} {value}");
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
    lab.run("", &["--", "make", "test"]);
    lab.run("server", &["--", "make", "test"]);
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
    assert_eq!(report["rules"].as_array().unwrap().len(), 1);

    // An unreadable archive still lets the transcripts report print.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let runs = lab.root.join(".justless/runs");
        fs::set_permissions(&runs, fs::Permissions::from_mode(0o000)).unwrap();
        fs::create_dir_all(lab.transcripts()).unwrap();
        lab.sessions("g", 2, glob_calls);
        let output = lab.learn(&[]);
        fs::set_permissions(&runs, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(stdout(&output).contains(GLOB_RULE), "{}", stdout(&output));
    }
}

#[test]
fn learn_archive_withholds_credentials() {
    let lab = Lab::new();
    lab.tool("tool", "exit 1");
    let records = |output: &Output| -> Value {
        let id = run_id(output);
        serde_json::from_str(&lab.read(&format!(".justless/runs/{id}/record.json"))).unwrap()
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
        let mut all = vec!["--"];
        all.extend(args);
        let record = records(&lab.run("", &all));
        assert_eq!(
            (&record["argv"], &record["argv_withheld"]),
            (&json!([]), &json!(true)),
            "{args:?}"
        );
        assert!(!record.to_string().contains("quux-value"), "{record:#}");
    }
    for args in [
        &["tool", "build"][..],
        &["tool", "install", "left-pad@latest"],
        &["/bin/sh", "-c", "tool build && tool test"],
    ] {
        let mut all = vec!["--"];
        all.extend(args);
        let record = records(&lab.run("", &all));
        assert_eq!(record["argv"], json!(args), "{record:#}");
        assert!(record.get("argv_withheld").is_none(), "{record:#}");
    }

    let report = lab.json(&[]);
    assert_eq!(report["skipped"]["credentials"], 6, "{report:#}");
    assert_eq!(report["sources"]["runs"]["without_command"], 0);
    assert!(
        lab.text(&[])
            .contains("6 dropped for a possible credential"),
        "{}",
        lab.text(&[])
    );
}

/// `cargo test` output when a test fails, captured from a scratch crate (exit 101).
const CARGO_FAILED: &str = "running 1 test\ntest tests::it_works ... FAILED\n\nfailures:\n\n---- tests::it_works stdout ----\n\nthread 'tests::it_works' (3914681) panicked at src/lib.rs:5:9:\nassertion `left == right` failed\n  left: 2\n right: 3\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\nfailures:\n    tests::it_works\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n   Compiling scratch v0.1.0 (/x/scratch)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.11s\n     Running unittests src/lib.rs (target/debug/deps/scratch-87369008df97ec58)\nerror: test failed, to rerun pass `--lib`";

#[test]
fn learn_test_failures_are_not_missing_options() {
    let lab = Lab::new();
    lab.sessions("t", 2, || {
        vec![
            call("cargo test", failure(101, CARGO_FAILED)),
            call("cargo test --lib", Outcome::Ok("test result: ok. 1 passed")),
            call(
                "cargo test",
                failure(101, CARGO_FAILED.replace("`--lib`", "`--test cli`")),
            ),
            call(
                "cargo test --test cli",
                Outcome::Ok("test result: ok. 1 passed"),
            ),
            call(
                "npm test",
                failure(
                    1,
                    "> x@1.0.0 test\n> vitest run\n\n FAIL  api.test.ts\nTests  1 failed | 3 passed\nTry --workspace to run one package",
                ),
            ),
            call("npm test --workspace=api", Outcome::Ok("Tests  3 passed")),
        ]
    });
    let report = lab.json(&["--min-sessions", "1"]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    assert_eq!(report["held"], json!([]), "{report:#}");

    // A failure that names the flag its correction adds is still learned.
    let lab = Lab::new();
    lab.sessions("p", 2, || {
        vec![
            call(
                "git push",
                failure(
                    128,
                    "fatal: The current branch feature has no upstream branch.\nTo push the current branch and set the remote as upstream, use\n\n    git push --set-upstream origin feature\n",
                ),
            ),
            call("git push --set-upstream origin feature", Outcome::Ok("done")),
        ]
    });
    rule(&lab.rules(&[]), "Pass `--set-upstream` to `git push`.");
}

#[test]
fn learn_npm_test_success_contradicts() {
    let lab = Lab::new();
    lab.write("web/package.json", "{}\n");
    let missing = "npm error code ENOENT\nnpm error syscall open\nnpm error path /x/package.json\nnpm error enoent Could not read package.json";
    lab.transcript(
        "a.jsonl",
        "a",
        now_ms() - DAY,
        vec![
            call("npm test", Outcome::Fail(254, missing)),
            call("npm --prefix web test", Outcome::Ok("ok")),
        ],
    );
    let text = "Run `npm` from `web/`; the repository root has no package.json.";
    rule(&lab.rules(&[]), text);
    lab.transcript(
        "b.jsonl",
        "b",
        now_ms() - MINUTE,
        vec![call("npm test", Outcome::Ok("ok"))],
    );
    let report = lab.json(&[]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    assert_eq!(
        held(&report, "wrong_directory")[0]["reason"],
        "contradicted"
    );
}

#[test]
fn learn_self_evident_privacy_and_directory() {
    let lab = Lab::new();
    lab.sessions("s", 2, || {
        vec![
            call(
                "ssh devbox kubectl get pods",
                Outcome::Fail(127, "bash: kubectl: command not found"),
            ),
            call(
                "API_TOKEN=s3cr3t rgx foo",
                Outcome::Fail(127, "zsh:1: command not found: rgx"),
            ),
            call(
                "cd /srv/other/app && ./gradlew build",
                Outcome::Fail(127, "zsh:1: no such file or directory: ./gradlew"),
            ),
            // Opaque within the root: the directory that ran ./mvnw is unknown.
            call(
                "D=sub\ncd $D && ./mvnw test",
                Outcome::Fail(127, "(eval):2: no such file or directory: ./mvnw"),
            ),
        ]
    });
    let report = lab.json(&["--min-sessions", "1"]);
    assert_eq!(report["rules"], json!([]), "{report:#}");
    assert_eq!(report["held"], json!([]), "{report:#}");
    assert_eq!(report["skipped"]["credentials"], 4, "{report:#}");

    // A parsed command keeps its directory.
    let lab = Lab::new();
    lab.sessions("r", 2, || {
        vec![call(
            "cd server && ./gradlew build",
            Outcome::Fail(127, "zsh:1: no such file or directory: ./gradlew"),
        )]
    });
    let found = lab.rules(&[]);
    let found = rule(
        &found,
        "`./gradlew` does not exist here; check the path before running it.",
    );
    assert_eq!(found["check"]["detail"], "./gradlew is missing");
    lab.write("server/gradlew", "#!/bin/sh\n");
    let report = lab.json(&[]);
    assert_eq!(held(&report, "missing_program")[0]["reason"], "stale");
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
    for (index, program) in [
        "npm",
        "cargo",
        "git",
        "make",
        "pytest",
        "node",
        "yarn",
        "timeout 5",
        "env",
        "python3",
        "./x",
    ]
    .iter()
    .enumerate()
    {
        let mut calls = Vec::new();
        for line in lines {
            calls.push(call(program, failure(1, line)));
            calls.push(call("pnpm", Outcome::Ok("ok")));
            calls.push(call(program, failure(2, line)));
            calls.push(call(&format!("{program} x"), Outcome::Ok("ok")));
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
fn learn_apply_inline_import() {
    let lab = Lab::new();
    lab.sessions("g", 2, glob_calls);
    lab.write("AGENTS.md", "# Agents\n");
    let claude = "# Claude\n\nFollow @AGENTS.md for everything.\n";
    lab.write("CLAUDE.md", claude);
    let report = lab.json(&["--apply"]);
    assert_eq!(report["apply"]["changed"], json!(["AGENTS.md"]));
    assert_eq!(
        report["apply"]["skipped"],
        json!([{"file": "CLAUDE.md", "reason": "imports AGENTS.md"}])
    );
    assert_eq!(lab.read("CLAUDE.md"), claude);
}

#[test]
fn learn_counts_only_error_lines_of_lists() {
    let lab = Lab::new();
    let other = "x".repeat(4000);
    lab.sessions("g", 2, || {
        let output = format!("{GLOB}\n{other}");
        vec![call(
            "grep -rn needle --include=*.rs .; cat notes.txt",
            Outcome::Raw(
                output.clone(),
                false,
                json!({"stdout": output, "interrupted": false}),
            ),
        )]
    });
    lab.sessions("h", 2, gh_calls);
    let rules = lab.rules(&[]);
    assert_eq!(
        rule(&rules, GLOB_RULE)["failure_bytes"],
        2 * (GLOB.len() + 1)
    );
    let gh = "Exit code 127\nzsh:1: command not found: gh";
    assert_eq!(rule(&rules, GH_RULE)["failure_bytes"], 2 * gh.len());

    // Transcript evidence points at the thread file and line.
    let text = lab.text(&[]);
    let path = lab.transcripts().join("g0.jsonl");
    assert!(
        text.contains(&format!("   evidence: {}:2\n", path.display())),
        "{text}"
    );
}

/// A run whose Justless was killed keeps the record written before its command
/// started, with no command line or exit status, and is not mistaken for a run
/// recorded before command capture.
#[test]
fn learn_skips_runs_left_unfinished() {
    let lab = Lab::new();
    lab.tool("tool", "exit 1");
    let id = run_id(&lab.run("", &["--", "tool", "build"]));
    let file = format!(".justless/runs/{id}/record.json");
    let mut record: Value = serde_json::from_str(&lab.read(&file)).unwrap();
    record["unfinished"] = json!(true);
    record["status"] = json!(-1);
    record["argv"] = json!([]);
    lab.write(&file, &record.to_string());

    let report = lab.json(&[]);
    assert_eq!(report["sources"]["runs"]["scanned"], 0, "{report:#}");
    assert_eq!(
        report["sources"]["runs"]["without_command"], 0,
        "{report:#}"
    );
    let text = lab.text(&[]);
    assert!(!text.contains("predate command capture"), "{text}");
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

/// Claude Code's Bash post-command hook records a result after the command ran,
/// with the whole command line's status, such as a pipe's 0 after a failing
/// command, so learn leaves those runs out.
#[test]
fn learn_skips_bash_results_recorded_after_the_command_ran() {
    use std::io::Write;
    let lab = Lab::new();
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
    child
        .stdin
        .take()
        .unwrap()
        .write_all(event.to_string().as_bytes())
        .unwrap();
    let answer: Value = serde_json::from_slice(&child.wait_with_output().unwrap().stdout).unwrap();
    assert!(
        answer["hookSpecificOutput"]["updatedToolOutput"].is_object(),
        "{answer}"
    );
    let report = lab.json(&[]);
    assert_eq!(report["sources"]["runs"]["scanned"], 0, "{report:#}");
}
