//! Regressions in the command hooks, their permission checks and their setup.
//! Every command points the home, client config and state directories at a
//! temporary directory, so no test reads or writes the machine's own settings.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Project {
    _temp: TempDir,
    base: PathBuf,
    root: PathBuf,
}

/// A throwaway Cargo project with a `.env` file, and an empty home directory.
fn project() -> Project {
    let temp = TempDir::new().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let root = base.join("project");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(base.join("home")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join(".env"), "SECRET=1\n").unwrap();
    Project {
        _temp: temp,
        base,
        root,
    }
}

impl Project {
    fn home(&self) -> PathBuf {
        self.base.join("home")
    }

    fn claude_config(&self) -> PathBuf {
        self.home().join(".claude")
    }

    fn command(&self, args: &[&str]) -> Command {
        let home = self.home();
        let mut command = Command::new(env!("CARGO_BIN_EXE_justless"));
        command
            .args(args)
            .current_dir(&self.root)
            .env("HOME", &home)
            .env("CLAUDE_CONFIG_DIR", self.claude_config())
            .env("CODEX_HOME", home.join(".codex"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env(
                "JUSTLESS_TEST_CLAUDE_MANAGED_DIR",
                self.base.join("managed"),
            )
            .env("CARGO_TARGET_DIR", self.base.join("target"))
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("JUSTLESS_DISABLE");
        command
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn success(&self, args: &[&str]) -> String {
        let output = self.cli(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// The JSON a hook prints for `input`.
    fn hook(&self, args: &[&str], input: &Value) -> Value {
        let mut child = self
            .command(args)
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
        assert!(output.status.success(), "{args:?}");
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("{args:?}: {}", String::from_utf8_lossy(&output.stdout)))
    }

    fn claude(&self, mode: &str, command: &str) -> Value {
        let input = json!({"hook_event_name":"PreToolUse","tool_name":"Bash","session_id":"s1",
            "cwd":self.root,"permission_mode":mode,"tool_input":{"command":command}});
        self.hook(&["hook-claude", "before-command"], &input)
    }

    /// The command each client's hook would run instead, or `None` when the hook
    /// leaves it as written, with the hook's whole answer.
    fn client(&self, handler: &str, command: &str) -> (Option<String>, Value) {
        let root = &self.root;
        let input = match handler {
            "hook-cursor" => json!({"tool_name":"Shell","cwd":root,
                "tool_input":{"command":command,"working_directory":root}}),
            "hook-gemini" => json!({"hook_event_name":"BeforeTool",
                "tool_name":"run_shell_command","cwd":root,"tool_input":{"command":command}}),
            "hook-copilot" => json!({"cwd":root,"toolName":"bash","toolArgs":{"command":command}}),
            "hook-agy" => json!({"cwd":root,"workspacePaths":[root],
                "toolCall":{"name":"run_command","args":{"CommandLine":command}}}),
            _ => json!({"hook_event_name":"PreToolUse","tool_name":"Bash","session_id":"s1",
                "cwd":root,"tool_input":{"command":command}}),
        };
        let output = self.hook(&[handler, "before-command"], &input);
        let hook = &output["hookSpecificOutput"];
        let rewritten = [
            &hook["updatedInput"]["command"],
            &hook["tool_input"]["command"],
            &output["updated_input"]["command"],
            &output["modifiedArgs"]["command"],
            &output["overwrite"]["CommandLine"],
        ]
        .into_iter()
        .find_map(|command| command.as_str().map(str::to_owned));
        (rewritten, output)
    }

    fn write(&self, file: &str, text: &str) {
        let path = self.root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn read(&self, file: &str) -> String {
        fs::read_to_string(self.root.join(file)).unwrap()
    }
}

fn routed(answer: &Value) -> Option<&str> {
    answer["hookSpecificOutput"]["updatedInput"]["command"].as_str()
}

/// Claude Code checks the paths a file command names against `Read` and `Edit`
/// rules, which do not match `justless run -- …`, so in every mode the hook leaves
/// a command that reads files while such a rule might deny or ask.
#[test]
fn claude_file_rules_leave_file_readers_in_every_mode() {
    let project = project();
    let commands = [
        "rg SECRET .env",
        "git diff .env",
        "RUST_LOG=1 git diff .env",
        "cargo build && rg SECRET .env",
    ];
    for kind in ["deny", "ask"] {
        project.write(
            ".claude/settings.json",
            &json!({"permissions":{kind:["Read(./.env)"]}}).to_string(),
        );
        for mode in [
            "bypassPermissions",
            "auto",
            "default",
            "acceptEdits",
            "dontAsk",
        ] {
            for command in commands {
                assert_eq!(
                    project.claude(mode, command),
                    json!({}),
                    "{kind} {mode}: {command}"
                );
            }
        }
        // Commands that read no file named on the line are still routed.
        let answer = project.claude("bypassPermissions", "cargo build");
        assert_eq!(
            routed(&answer),
            Some("justless run --hook --session e8bc163c82eee18733288c7d -- cargo build"),
            "{kind}: {answer}"
        );
    }
    // Without a file rule, bypassPermissions rewrites a search as before.
    fs::remove_file(project.root.join(".claude/settings.json")).unwrap();
    let answer = project.claude("bypassPermissions", "rg SECRET .env");
    assert!(
        routed(&answer).is_some_and(|command| command.contains("run --hook")),
        "{answer}"
    );
}

/// Codex and Cursor run a rewritten command without asking, so an option that
/// makes a search or a build run another program keeps the command as written.
#[test]
fn approving_hooks_leave_options_that_run_other_programs() {
    let project = project();
    let left = [
        "rg --hostname-bin=./evil.sh foo",
        "rg --hostname-bin ./evil.sh foo",
        "rg --search-zip foo",
        "rg -nz foo",
        "rg -z foo",
        "rg --pre ./x foo",
        "rg --pre=./x foo",
        "rg --pre-glob '*.gz' foo",
        "rg --no-such-option foo",
        "go vet -vettool=./x ./...",
        "go test -vettool ./x ./...",
        "go test -ldflags=-extld=./x ./...",
        "go build -gccgoflags=-B./x ./...",
        "pylint --init-hook=x src",
        "pylint --load-plugins=x src",
        "mypy --python-executable=./py src",
        "pyright --pythonpath ./py",
        "rubocop -r ./x.rb",
        "rubocop --require=./x.rb",
        "pytest -p evil",
        "pytest -pevil",
        "pytest --pdbcls=x:Debugger",
        "python -m pytest -p evil",
        "npm test --script-shell=./x",
        "npm test --node-options=--require=./x.js",
        "node --test --import=data:text/javascript,1",
        "node --test --require ./x.js",
        "node --test -e 1",
        "swift build -Xswiftc -load-plugin-executable",
        "swift test --toolchain ./tc",
        "cargo clippy -- -C linker=./x",
        "cargo clippy -- --extern x=./x.so",
        "jest --setupFiles=./x.js",
        "npm test -- --globalSetup ./x.js",
        "eslint --init",
    ];
    for handler in ["hook-codex", "hook-cursor"] {
        for command in left {
            let (rewritten, answer) = project.client(handler, command);
            assert_eq!(rewritten, None, "{handler}: {command}: {answer}");
        }
        for command in [
            "rg -n foo",
            "rg --heading -n -C2 foo",
            "rg -uu --glob '*.rs' foo src",
            "go test ./...",
            "go test -run TestX -ldflags=-X=main.v=1 ./...",
            "pytest -p no:cacheprovider -q",
            "pylint src",
            "npm test",
            "node --test",
            "swift build",
            "cargo clippy -- -D warnings",
        ] {
            let (rewritten, answer) = project.client(handler, command);
            assert!(
                rewritten.is_some_and(|rewritten| rewritten.ends_with(&format!("-- {command}"))),
                "{handler}: {command}: {answer}"
            );
        }
    }
    // Claude Code and Gemini CLI check the rewritten command themselves, so they
    // still route it.
    let answer = project.claude("bypassPermissions", "rg --hostname-bin=./evil.sh foo");
    assert!(routed(&answer).is_some(), "{answer}");
    let (rewritten, answer) = project.client("hook-gemini", "rg --search-zip foo");
    assert!(rewritten.is_some(), "{answer}");
}

/// `justless run` refuses to start while `.justless.toml` cannot be loaded, so
/// every hook leaves every command as written instead of routing it into a run
/// that fails before the command starts.
#[test]
fn a_settings_file_that_fails_to_load_leaves_every_command() {
    let project = project();
    let clients = [
        "hook-codex",
        "hook-cursor",
        "hook-gemini",
        "hook-copilot",
        "hook-agy",
    ];
    let lines: String = (0..400)
        .map(|line| format!("src/main.rs:{line}:needle {line}\n"))
        .collect();
    let after_bash = || {
        let input = json!({"session_id":"s1","hook_event_name":"PostToolUse","tool_name":"Bash",
            "cwd":project.root,"tool_input":{"command":"rg -n needle src"},
            "tool_response":{"stdout":lines,"stderr":"","interrupted":false,"isImage":false}});
        project.hook(&["hook-claude", "after-bash"], &input)
    };
    // With valid settings, each hook routes the command, and the post-command
    // hook compacts large output.
    project.write(".justless.toml", "global_hooks = false\n");
    for handler in clients {
        let (rewritten, answer) = project.client(handler, "cargo build");
        assert!(rewritten.is_some(), "{handler}: {answer}");
    }
    let answer = after_bash();
    assert!(
        answer["hookSpecificOutput"]["updatedToolOutput"]["stdout"].is_string(),
        "{answer}"
    );
    for text in [
        "global_hooks = false\nfuture_setting = 1\n",
        "global_hooks = false\ncontext_budget = 64\n",
        "exclude_commands = [\"\"]\n",
    ] {
        project.write(".justless.toml", text);
        let output = project.cli(&["run", "--", "cargo", "--version"]);
        assert_eq!(output.status.code(), Some(2), "{text}");
        for handler in clients {
            let (rewritten, answer) = project.client(handler, "cargo build");
            assert_eq!(rewritten, None, "{handler}: {text}: {answer}");
            let noop = if handler == "hook-agy" {
                json!({"decision":"allow"})
            } else {
                json!({})
            };
            assert_eq!(answer, noop, "{handler}: {text}");
        }
        for (mode, command) in [
            ("bypassPermissions", "cargo build"),
            ("bypassPermissions", "cat src/main.rs"),
            ("bypassPermissions", "justless context main"),
        ] {
            assert_eq!(
                project.claude(mode, command),
                json!({}),
                "{text}: {command}"
            );
        }
        // A user-level hook leaves the project alone, as if it opted out.
        for handler in ["hook-claude", "hook-codex"] {
            let input = json!({"hook_event_name":"PreToolUse","tool_name":"Bash",
                "session_id":"s1","cwd":project.root,"permission_mode":"bypassPermissions",
                "tool_input":{"command":"cargo build"}});
            let answer = project.hook(&[handler, "before-command", "--global"], &input);
            assert_eq!(answer, json!({}), "{handler}: {text}");
        }
        let agy_input = json!({"cwd":project.root,"workspacePaths":[project.root],
            "toolCall":{"name":"run_command","args":{"CommandLine":"cargo build"}}});
        let answer = project.hook(&["hook-agy", "before-command", "--global"], &agy_input);
        assert_eq!(answer, json!({"decision":"allow"}), "hook-agy: {text}");
        // The post-command hook leaves large output as it was.
        assert_eq!(after_bash(), json!({}), "{text}");
        let rewrite = project.success(&["rewrite", "--agent", "codex", "cargo build"]);
        assert!(
            rewrite.contains("unchanged") && rewrite.contains(".justless.toml"),
            "{rewrite}"
        );
    }
    let doctor = String::from_utf8_lossy(&project.cli(&["doctor"]).stdout).into_owned();
    assert!(
        doctor.contains(".justless.toml: ") && doctor.contains("leave every command as written"),
        "{doctor}"
    );
}

/// Removing Codex keeps a shared `AGENTS.md` section for Claude Code only when
/// Claude Code was set up, not merely because `CLAUDE.md` imports the file.
#[test]
fn removing_codex_ignores_an_import_claude_code_was_never_set_up_for() {
    let project = project();
    project.write("CLAUDE.md", "# Project\n\n@AGENTS.md\n");
    project.write("AGENTS.md", "# Agents\n\nBe nice.\n");
    project.success(&["init", "--agent", "codex", "--hooks"]);
    assert!(project.read("AGENTS.md").contains("justless:begin"));
    let output = project.success(&["init", "--remove", "--agent", "codex"]);
    assert!(!output.contains("claude also reads"), "{output}");
    assert_eq!(project.read("AGENTS.md"), "# Agents\n\nBe nice.\n");
    assert_eq!(project.read("CLAUDE.md"), "# Project\n\n@AGENTS.md\n");

    // Once Claude Code is set up too, the section stays for it.
    project.success(&["init", "--agent", "codex", "--agent", "claude", "--hooks"]);
    let output = project.success(&["init", "--remove", "--agent", "codex"]);
    assert!(
        output.contains("Kept the Justless section in AGENTS.md, which claude also reads."),
        "{output}"
    );
    assert!(project.read("AGENTS.md").contains("justless:begin"));
}

/// `exclude_commands` entries match a command by its shell words, whatever its
/// spacing or quoting, while an expression still sees the text as written.
#[test]
fn exclusions_match_shell_words_and_expressions_the_raw_text() {
    let project = project();
    project.write(
        ".justless.toml",
        "exclude_commands = [\"cargo test\", \"rg foo\", \"^rg  -n\"]\n",
    );
    for command in [
        "cargo test",
        "cargo  test",
        "'cargo' test",
        "cargo\ttest --lib",
        "\"cargo\" \"test\"",
        "RUST_LOG=1  cargo   test",
        "rg  foo src",
        "rg  -n bar",
    ] {
        assert_eq!(
            project.claude("bypassPermissions", command),
            json!({}),
            "{command}"
        );
        let (rewritten, _) = project.client("hook-codex", command);
        assert_eq!(rewritten, None, "{command}");
    }
    // An entry is not a prefix of a longer word, and the expression needs its
    // two spaces.
    for command in ["rg foobar src", "rg -n bar"] {
        let answer = project.claude("bypassPermissions", command);
        assert!(routed(&answer).is_some(), "{command}: {answer}");
    }
    // In a list, Claude Code's hook routes the other commands and Codex's leaves
    // the whole list.
    let answer = project.claude("bypassPermissions", "cargo build;cargo  test");
    assert_eq!(
        routed(&answer),
        Some("justless run --hook --session e8bc163c82eee18733288c7d -- cargo build;cargo  test"),
        "{answer}"
    );
    let (rewritten, _) = project.client("hook-codex", "cargo build;cargo  test");
    assert_eq!(rewritten, None);
}

/// Antigravity's hook answers a rewrite with `decision: "allow"`, so it takes
/// only the commands Codex's hook would also approve, as `rewrite` says.
#[test]
fn the_antigravity_hook_approves_only_what_codex_would() {
    let project = project();
    for command in [
        "rg --hostname-bin=./evil.sh foo",
        "go vet -vettool=./x ./...",
    ] {
        let (rewritten, answer) = project.client("hook-agy", command);
        assert_eq!(rewritten, None, "{command}: {answer}");
        assert_eq!(answer, json!({"decision":"allow"}), "{command}");
        let report = project.success(&["rewrite", "--agent", "agy", command]);
        assert!(
            report.starts_with(
                "Antigravity CLI hook: unchanged\nWhy: Antigravity CLI runs a rewritten command without asking, so its hook leaves one with an option"
            ),
            "{report}"
        );
    }
    let (rewritten, answer) = project.client("hook-agy", "cargo test");
    assert!(rewritten.is_some(), "{answer}");
    assert_eq!(answer["decision"], "allow");
    let report = project.success(&["rewrite", "--agent", "agy", "cargo test"]);
    assert!(
        report.contains(
            "Permission: the hook allows the rewrite, and Antigravity CLI runs it without asking."
        ),
        "{report}"
    );
}

/// Removing the global hooks after the user edited the file deletes the handlers
/// and the arrays and objects setup created, never the user's own empty ones.
#[test]
fn removing_global_hooks_keeps_the_users_own_empty_hook_arrays() {
    let project = project();
    let settings = project.claude_config().join("settings.json");
    fs::create_dir_all(project.claude_config()).unwrap();
    for before in [
        json!({"model":"opus","hooks":{
            "PreToolUse":[{"matcher":"^Bash$","hooks":[{"type":"command","command":"my-guard.sh"}]}],
            "SessionStart":[]
        },"permissions":{"allow":["Bash(ls)"]}}),
        json!({"model":"opus","hooks":{}}),
    ] {
        fs::write(&settings, serde_json::to_string_pretty(&before).unwrap()).unwrap();
        project.success(&["init", "--global", "--agent", "claude"]);
        let installed: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert!(installed["hooks"]["PostToolUse"].is_array(), "{installed}");
        // The user, or Claude Code, changes a value afterwards.
        let mut edited = installed.clone();
        edited["model"] = json!("sonnet");
        fs::write(&settings, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
        project.success(&["init", "--remove", "--global", "--agent", "claude"]);
        let left: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        let mut expected = before.clone();
        expected["model"] = json!("sonnet");
        assert_eq!(left, expected);
    }
    // Setup run again after an edit that dropped one of its hooks records the file
    // as it was from the first record, so the user's empty array still comes back.
    let before = json!({"model":"opus","hooks":{"SessionStart":[]}});
    fs::write(&settings, serde_json::to_string_pretty(&before).unwrap()).unwrap();
    project.success(&["init", "--global", "--agent", "claude"]);
    let mut edited: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    edited["model"] = json!("sonnet");
    edited["hooks"]
        .as_object_mut()
        .unwrap()
        .remove("PostToolUse")
        .unwrap();
    fs::write(&settings, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
    let output = project.success(&["init", "--global", "--agent", "claude"]);
    assert!(output.starts_with("Updated "), "{output}");
    project.success(&["init", "--remove", "--global", "--agent", "claude"]);
    let left: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert_eq!(left, json!({"model":"sonnet","hooks":{"SessionStart":[]}}));
    // A file setup created holds nothing of the user's, so it goes.
    fs::remove_file(&settings).unwrap();
    project.success(&["init", "--global", "--agent", "claude"]);
    let mut edited: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    edited["model"] = json!("sonnet");
    fs::write(&settings, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
    project.success(&["init", "--remove", "--global", "--agent", "claude"]);
    let left: Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert_eq!(left, json!({"model":"sonnet"}));
}

const ALL_CLIENTS: &[&str] = &[
    "hook-claude",
    "hook-codex",
    "hook-agy",
    "hook-cursor",
    "hook-gemini",
    "hook-copilot",
];

fn assert_wrapped(client: &str, command: &str, rewritten: Option<&str>, output: &Value) {
    let actual = rewritten.unwrap_or_else(|| panic!("{client}: {command} -> {output}"));
    let expected_suffix = format!(" -- {command}");
    let stripped = actual.strip_suffix(" && test 1").unwrap_or(actual);
    assert!(
        stripped.starts_with("justless run --hook ") && stripped.ends_with(&expected_suffix),
        "{client}: {command} -> {actual:?} ({output})"
    );
    if !matches!(client, "hook-claude" | "hook-codex") {
        assert_eq!(
            actual,
            format!("justless run --hook -- {command}"),
            "{client}: {command} -> {output}"
        );
    }
    if client == "hook-agy" {
        assert_eq!(output["decision"], "allow", "{command}: {output}");
    }
}

#[test]
fn posix_single_quoted_literals_route_across_all_clients() {
    let project = project();
    for command in [
        r"rg '\bfoo\b' src",
        r"rg 'fn\s+main' src",
        "rg 'foo$' src",
        r"git grep '\bTODO\b'",
        "go test ./... -run '^TestFoo$'",
    ] {
        for &client in ALL_CLIENTS {
            let (rewritten, output) = project.client(client, command);
            assert_wrapped(client, command, rewritten.as_deref(), &output);
        }
    }
    // `sed -n '10,$p'` and attached `-nN` / `--lines=N` read forms route cleanly on
    // non-Claude clients for large structured files (Claude leaves shell reads to
    // its post-run `after-bash` hook).
    project.write("src/big.rs", &synthetic_rust_file(12, 21));
    for (command, expected_range) in [
        ("sed -n '10,$p' src/big.rs", "--shell-range +10"),
        ("head -n200 src/big.rs", "--shell-range 1-200"),
        ("head --lines=200 src/big.rs", "--shell-range 1-200"),
        ("tail -n+10 src/big.rs", "--shell-range +10"),
    ] {
        for &client in ALL_CLIENTS {
            let (rewritten, output) = project.client(client, command);
            if client == "hook-claude" {
                assert_eq!(rewritten, None, "claude should leave read: {command}");
            } else {
                let actual = rewritten
                    .as_deref()
                    .unwrap_or_else(|| panic!("{client}: {command} -> {output}"));
                assert!(
                    actual.contains(" read ")
                        && actual.contains(expected_range)
                        && actual.ends_with(" -- src/big.rs"),
                    "{client}: {command} -> {actual:?} ({output})"
                );
            }
        }
    }
    // Escaped quote inside single quotes (`\'`), double-quoted `$` / backticks, and
    // unquoted backslashes stay rejected across all 6 clients.
    for rejected in [
        r"rg 'foo\'bar' src",
        r"rg 'foo\' src",
        r#"rg "$FOO" src"#,
        r#"rg "`id`" src"#,
        r"rg \bfoo\b src",
    ] {
        for &client in ALL_CLIENTS {
            let (rewritten, output) = project.client(client, rejected);
            assert_eq!(
                rewritten, None,
                "{client} must reject {rejected:?}: {output}"
            );
        }
    }
}

#[test]
fn safe_wrapper_unwrapping_and_strict_approval() {
    let project = project();
    // Safe auto-approvable wrappers route across all 6 clients (Agy Tier 1 `allow`).
    for command in [
        "timeout 60 cargo test",
        "timeout -s TERM -k 5 --preserve-status 30s cargo test",
        "nice -n 5 cargo test",
        "stdbuf -oL cargo test",
        "stdbuf --output=L cargo test",
        "env RUST_BACKTRACE=1 cargo test",
        "env LC_ALL=C TERM=dumb cargo test",
        "timeout 60 nice -n 5 stdbuf -oL env RUST_BACKTRACE=1 cargo test",
    ] {
        for &client in ALL_CLIENTS {
            let (rewritten, output) = project.client(client, command);
            assert_wrapped(client, command, rewritten.as_deref(), &output);
        }
    }
    // Non-auto-approvable safe `env` wrappers: Codex & Cursor reject; Claude, Gemini &
    // Copilot route; and Antigravity CLI routes in Tier 2 (`decision: "ask"`, never `"allow"`).
    for command in [
        "env -i cargo test",
        "env -u HOME cargo test",
        "env FOO=bar cargo test",
        "env TERM=dumb make test",
    ] {
        for client in ["hook-codex", "hook-cursor"] {
            let (rewritten, output) = project.client(client, command);
            assert_eq!(rewritten, None, "{client} must reject {command}: {output}");
        }
        for client in ["hook-claude", "hook-gemini", "hook-copilot"] {
            let (rewritten, output) = project.client(client, command);
            assert_wrapped(client, command, rewritten.as_deref(), &output);
        }
        let (rewritten, output) = project.client("hook-agy", command);
        assert_eq!(
            rewritten.as_deref(),
            Some(format!("justless run --hook -- {command}").as_str()),
            "agy Tier 2: {command} -> {output}"
        );
        assert_eq!(
            output["decision"], "ask",
            "agy must ask (not auto-allow) for {command}: {output}"
        );
    }
    // Unsafe inner commands inside wrappers must never bypass safety checks.
    for unsafe_wrapped in [
        "timeout 60 rg --hostname-bin=./evil.sh foo",
        "env -i rg --hostname-bin=./evil.sh foo",
        "nice -n 5 go vet -vettool=./evil ./...",
    ] {
        for client in ["hook-codex", "hook-cursor", "hook-agy"] {
            let (rewritten, output) = project.client(client, unsafe_wrapped);
            assert_eq!(
                rewritten, None,
                "{client} must reject unsafe wrapped command {unsafe_wrapped}: {output}"
            );
        }
    }
    // Forbidden wrapper flags, steering env vars, and shell/time wrappers stay raw everywhere.
    for forbidden in [
        "env -S 'cargo test'",
        "env --split-string='cargo test'",
        "env -C /tmp cargo test",
        "env --chdir=/tmp cargo test",
        "env --argv0=x cargo test",
        "env PATH=/tmp cargo test",
        "env -u PATH cargo test",
        "env LD_PRELOAD=/tmp/x.so cargo test",
        "env JUSTLESS_DISABLE=1 cargo test",
        "timeout --unknown 60 cargo test",
        "timeout foo cargo test",
        "nice --unknown cargo test",
        "stdbuf -xL cargo test",
        "bash -c 'cargo test'",
        "sh -c 'cargo test'",
        "time cargo test",
    ] {
        for &client in ALL_CLIENTS {
            let (rewritten, output) = project.client(client, forbidden);
            assert_eq!(
                rewritten, None,
                "{client} must reject {forbidden:?}: {output}"
            );
        }
    }
    // `exclude_commands` still matches the unwrapped command inside safe wrappers.
    project.write(".justless.toml", "exclude_commands = [\"cargo test\"]\n");
    for excluded in ["timeout 60 cargo test", "env RUST_BACKTRACE=1 cargo test"] {
        for &client in ALL_CLIENTS {
            let (rewritten, output) = project.client(client, excluded);
            assert_eq!(
                rewritten, None,
                "{client} must honor exclude_commands for {excluded}: {output}"
            );
        }
    }
}

#[test]
fn codex_and_agy_attach_seen_session_to_context_and_pack() {
    let project = project();
    let root = &project.root;
    let codex = |command: &str| {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "session_id": "codex-sess-1",
            "cwd": root,
            "tool_input": {"command": command}
        });
        let output = project.hook(&["hook-codex", "before-command"], &input);
        let rewritten = output["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .map(str::to_owned);
        (rewritten, output)
    };
    let agy = |command: &str| {
        let input = json!({
            "cwd": root,
            "workspacePaths": [root],
            "conversationId": "agy-conv-1",
            "toolCall": {"name": "run_command", "args": {"CommandLine": command}}
        });
        let output = project.hook(&["hook-agy", "before-command"], &input);
        let rewritten = output["overwrite"]["CommandLine"]
            .as_str()
            .map(str::to_owned);
        (rewritten, output)
    };
    for command in [
        "justless context --symbol main",
        "justless pack \"fix bug\"",
    ] {
        let (c_rewritten, c_out) = codex(command);
        let c_rewritten = c_rewritten.unwrap_or_else(|| panic!("codex {command}: {c_out}"));
        assert!(
            c_rewritten.starts_with(&format!("{command} --session ")),
            "codex: {c_rewritten}"
        );
        let c_token = c_rewritten.rsplit_once(" --session ").unwrap().1;
        assert_eq!(c_token.len(), 24);
        assert!(c_token.chars().all(|c| c.is_ascii_hexdigit()));

        let (a_rewritten, a_out) = agy(command);
        let a_rewritten = a_rewritten.unwrap_or_else(|| panic!("agy {command}: {a_out}"));
        assert_eq!(a_out["decision"], "allow", "agy: {a_out}");
        assert!(
            a_rewritten.starts_with(&format!("{command} --session ")),
            "agy: {a_rewritten}"
        );
        let a_token = a_rewritten.rsplit_once(" --session ").unwrap().1;
        assert_eq!(a_token.len(), 24);
        assert!(a_token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(c_token, a_token);
    }
    // Explicit `--session` or `--` leaves the command untouched.
    for unchanged in [
        "justless context --session existing --symbol main",
        "justless pack -- fix",
    ] {
        assert_eq!(codex(unchanged).0, None, "codex: {unchanged}");
        assert_eq!(agy(unchanged).0, None, "agy: {unchanged}");
    }
    // `exclude_commands` leaves the command untouched.
    project.write(
        ".justless.toml",
        "exclude_commands = [\"justless context\"]\n",
    );
    assert_eq!(codex("justless context --symbol main").0, None);
    assert_eq!(agy("justless context --symbol main").0, None);
    // Invalid `.justless.toml` leaves the command untouched.
    project.write(".justless.toml", "not valid toml = [\n");
    assert_eq!(codex("justless pack \"fix bug\"").0, None);
    assert_eq!(agy("justless pack \"fix bug\"").0, None);
}

fn synthetic_rust_file(functions: usize, body_lines: usize) -> String {
    let mut out = String::new();
    for i in 0..functions {
        out.push_str(&format!(
            "pub fn func_{i}(n: usize) -> usize {{\n    let mut v = n + {i};\n"
        ));
        for j in 0..body_lines {
            out.push_str(&format!(
                "    v += {j}; // filler filler filler filler filler\n"
            ));
        }
        out.push_str("    v * 2\n}\n");
    }
    out
}

#[test]
fn agy_before_read_ranged_outside_repeat_and_retry_bypass() {
    let project = project();
    let root = &project.root;
    let sub = root.join("sub");
    fs::create_dir_all(&sub).unwrap();

    // Structured Rust file with spaces in name (30 functions * 25 lines = 750 lines, >35 KB).
    let spaced_body = synthetic_rust_file(30, 21);
    project.write("src/my file.rs", &spaced_body);

    // 1000-line structured file (40 functions * 25 lines) for omitted EndLine clamping.
    let thousand_body = synthetic_rust_file(40, 21);
    project.write("src/thousand.rs", &thousand_body);

    // Single 700-line function (one row covers >50% of window -> must allow).
    let mut one_big = String::from("pub fn giant() -> usize {\n    let mut total = 0;\n");
    for i in 0..700 {
        one_big.push_str(&format!(
            "    total += {i}; // padding padding padding padding\n"
        ));
    }
    one_big.push_str("    total\n}\n");
    project.write("src/one_big.rs", &one_big);

    // Large file containing a redacted secret -> must allow (never outline secrets).
    let mut secret_big = synthetic_rust_file(30, 21);
    secret_big.push_str("pub const KEY: &str = \"ghp_A7bK9mQ2xR4vL8nP1wZ6cT3yF5hJ0dS8eU2g\";\n");
    project.write("src/secret_big.rs", &secret_big);

    // Medium file (300 lines, >12 KB > 2400B context_budget, <=600 read_outline_lines) for repeat guard.
    let medium_body = synthetic_rust_file(12, 21);
    project.write("src/medium.rs", &medium_body);

    let before_read = |cwd: &std::path::Path, args: Value, conv: &str, step: u64| {
        let input = json!({
            "cwd": cwd,
            "workspacePaths": [root],
            "conversationId": conv,
            "stepIdx": step,
            "toolCall": {"name": "read_file", "args": args}
        });
        project.hook(&["hook-agy", "before-read"], &input)
    };

    // 1. Outside-workspace unbounded read BEFORE any `.justless/index.json` exists.
    let outside_dir = project.base.join("outside");
    fs::create_dir_all(outside_dir.join("tool-results")).unwrap();
    let outside_file = outside_dir.join("big.rs");
    fs::write(&outside_file, &spaced_body).unwrap();
    let out_ans = before_read(root, json!({"Path": outside_file}), "conv-outside", 1);
    assert_eq!(out_ans["decision"], "deny", "{out_ans}");
    let out_reason = out_ans["reason"].as_str().unwrap();
    assert!(
        out_reason.contains("Justless outline of outside file"),
        "{out_reason}"
    );
    assert!(
        !root.join(".justless/index.json").exists(),
        "outside before-read must not build .justless/index.json"
    );
    // Immediate 1-shot retry of the exact same outside read is allowed verbatim.
    let out_retry = before_read(root, json!({"Path": outside_file}), "conv-outside", 2);
    assert_eq!(out_retry["decision"], "allow", "{out_retry}");
    // Ranged outside read passes through.
    let out_ranged = before_read(
        root,
        json!({"Path": outside_file, "StartLine": 1, "EndLine": 700}),
        "conv-outside",
        3,
    );
    assert_eq!(out_ranged["decision"], "allow", "{out_ranged}");

    // Blocked outside targets: dotfile, $HOME/.* , tool-results, and symlink.
    let dotfile = outside_dir.join(".hidden.rs");
    fs::write(&dotfile, &spaced_body).unwrap();
    assert_eq!(
        before_read(root, json!({"Path": dotfile}), "conv-outside", 4)["decision"],
        "allow"
    );
    let home_hidden = project.home().join(".config/big.rs");
    fs::create_dir_all(home_hidden.parent().unwrap()).unwrap();
    fs::write(&home_hidden, &spaced_body).unwrap();
    assert_eq!(
        before_read(root, json!({"Path": home_hidden}), "conv-outside", 5)["decision"],
        "allow"
    );
    let tool_res = outside_dir.join("tool-results/big.rs");
    fs::write(&tool_res, &spaced_body).unwrap();
    assert_eq!(
        before_read(root, json!({"Path": tool_res}), "conv-outside", 6)["decision"],
        "allow"
    );
    #[cfg(unix)]
    {
        let link = outside_dir.join("link.rs");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();
        assert_eq!(
            before_read(root, json!({"Path": link}), "conv-outside", 7)["decision"],
            "allow"
        );
    }

    // 2. Large ranged workspace read from a subdirectory (`cwd = sub/`) with spaces in path.
    let spaced_path = root.join("src/my file.rs");
    let ranged_ans = before_read(
        &sub,
        json!({"Path": spaced_path, "StartLine": 1, "EndLine": 700}),
        "conv-ws",
        1,
    );
    assert_eq!(ranged_ans["decision"], "deny", "{ranged_ans}");
    let reason = ranged_ans["reason"].as_str().unwrap();
    assert!(
        reason.contains("Justless outline of src/my file.rs lines 1-700"),
        "{reason}"
    );
    assert!(
        reason.contains("--root"),
        "subdirectory cwd must include --root: {reason}"
    );
    assert!(
        reason.contains("'src/my file.rs'"),
        "spaces in path must be shell-quoted: {reason}"
    );

    // 3. 1-shot retry bypass right after outline deny -> allows verbatim and records step 2.
    let retry_ans = before_read(
        &sub,
        json!({"Path": spaced_path, "StartLine": 1, "EndLine": 700}),
        "conv-ws",
        2,
    );
    assert_eq!(retry_ans["decision"], "allow", "{retry_ans}");

    // 4. Repeat-read guard within 15 steps -> denies with 1-line unchanged notice.
    let repeat_ans = before_read(
        &sub,
        json!({"Path": spaced_path, "StartLine": 1, "EndLine": 700}),
        "conv-ws",
        6,
    );
    assert_eq!(repeat_ans["decision"], "deny", "{repeat_ans}");
    let repeat_reason = repeat_ans["reason"].as_str().unwrap();
    assert!(
        repeat_reason.contains("unchanged since step 2"),
        "{repeat_reason}"
    );
    assert_eq!(repeat_reason.lines().count(), 1, "{repeat_reason}");

    // Immediate retry after repeat-read deny -> allows verbatim!
    let repeat_retry = before_read(
        &sub,
        json!({"Path": spaced_path, "StartLine": 1, "EndLine": 700}),
        "conv-ws",
        7,
    );
    assert_eq!(repeat_retry["decision"], "allow", "{repeat_retry}");

    // Same repeat-read flow for an allowed <=600-line large read, plus invalidation on edit.
    let medium_path = root.join("src/medium.rs");
    assert_eq!(
        before_read(
            root,
            json!({"Path": medium_path, "StartLine": 1, "EndLine": 300}),
            "conv-med",
            1,
        )["decision"],
        "allow"
    );
    let med_repeat = before_read(
        root,
        json!({"Path": medium_path, "StartLine": 1, "EndLine": 300}),
        "conv-med",
        5,
    );
    assert_eq!(med_repeat["decision"], "deny", "{med_repeat}");
    assert!(
        med_repeat["reason"]
            .as_str()
            .unwrap()
            .contains("unchanged since step 1")
    );
    // Editing the file changes its digest, so reading it again is allowed immediately.
    project.write("src/medium.rs", &format!("{medium_body}// touched\n"));
    assert_eq!(
        before_read(
            root,
            json!({"Path": medium_path, "StartLine": 1, "EndLine": 300}),
            "conv-med",
            6,
        )["decision"],
        "allow"
    );
    // Past the 15-step window, the repeat guard no longer fires (allowed again).
    assert_eq!(
        before_read(
            root,
            json!({"Path": medium_path, "StartLine": 1, "EndLine": 300}),
            "conv-med",
            25,
        )["decision"],
        "allow"
    );

    // 5. Omitted EndLine clamps to `min(total, first + 799)`.
    let thousand_path = root.join("src/thousand.rs");
    let clamped = before_read(
        root,
        json!({"Path": thousand_path, "StartLine": 100}),
        "conv-clamp",
        1,
    );
    assert_eq!(clamped["decision"], "deny", "{clamped}");
    assert!(
        clamped["reason"]
            .as_str()
            .unwrap()
            .contains("lines 100-899"),
        "{clamped}"
    );

    // 6. Pass-through contracts: <=150 lines, <=600 default, ByteOffset, 1-row >50%, secret.
    assert_eq!(
        before_read(
            root,
            json!({"Path": spaced_path, "StartLine": 1, "EndLine": 500}),
            "conv-pass",
            1,
        )["decision"],
        "allow"
    );
    assert_eq!(
        before_read(
            root,
            json!({"Path": spaced_path, "StartLine": 1, "EndLine": 700, "ByteOffset": 64}),
            "conv-pass",
            2,
        )["decision"],
        "allow"
    );
    assert_eq!(
        before_read(
            root,
            json!({"Path": root.join("src/one_big.rs"), "StartLine": 1, "EndLine": 700}),
            "conv-pass",
            3,
        )["decision"],
        "allow"
    );
    assert_eq!(
        before_read(
            root,
            json!({"Path": root.join("src/secret_big.rs"), "StartLine": 1, "EndLine": 700}),
            "conv-pass",
            4,
        )["decision"],
        "allow"
    );
    // Even with a low `read_outline_lines`, <=150 lines passes through.
    project.write(".justless.toml", "read_outline_lines = 50\n");
    assert_eq!(
        before_read(
            root,
            json!({"Path": spaced_path, "StartLine": 1, "EndLine": 150}),
            "conv-pass",
            5,
        )["decision"],
        "allow"
    );
}

#[test]
fn agy_edit_parse_error_bridge_and_lifecycle() {
    let project = project();
    let root = &project.root;

    // Install Agy hooks, verify doctor passes read-only with all 7 hooks, then remove cleanly.
    project.success(&["init", "--hooks", "--agent", "agy"]);
    let hooks_path = root.join(".agents/hooks.json");
    let installed: Value = serde_json::from_slice(&fs::read(&hooks_path).unwrap()).unwrap();
    let post_inv = installed["justless"]["PostInvocation"].as_array().unwrap();
    assert_eq!(post_inv.len(), 1);
    assert!(
        post_inv[0]["command"]
            .as_str()
            .unwrap()
            .contains("hook-agy post-invocation"),
        "PostInvocation must be a flat array entry: {installed}"
    );
    let state_before: Vec<_> = fs::read_dir(root.join(".justless"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let doctor_out = project.success(&["doctor"]);
    assert!(
        doctor_out.contains("has all 7 "),
        "doctor should report all 7 Agy hooks: {doctor_out}"
    );
    let state_after: Vec<_> = fs::read_dir(root.join(".justless"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(state_after, state_before, "doctor must stay read-only");
    assert!(
        !root.join(".justless/index.json").exists(),
        "doctor must not build .justless/index.json"
    );

    let main_path = root.join("src/main.rs");
    // 1. `before-edit` always allows, even under `JUSTLESS_DISABLE=1`.
    let before_input = json!({
        "cwd": root,
        "workspacePaths": [root],
        "conversationId": "conv-edit",
        "toolCall": {"id": "tc-disabled", "name": "edit_file", "args": {"Path": main_path}}
    });
    let mut disabled = project.command(&["hook-agy", "before-edit"]);
    disabled
        .env("JUSTLESS_DISABLE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = disabled.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(before_input.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let disabled_ans: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(disabled_ans, json!({"decision": "allow"}));

    // 2. Broken edit -> `before-edit` snapshots, `after-edit` detects & deletes snapshot,
    //    `post-invocation` injects the note once and clears it.
    let call_edit = json!({
        "cwd": root,
        "workspacePaths": [root],
        "conversationId": "conv-edit",
        "toolCall": {"id": "tc-1", "name": "edit_file", "args": {"Path": main_path}}
    });
    assert_eq!(
        project.hook(&["hook-agy", "before-edit"], &call_edit),
        json!({"decision": "allow"})
    );
    project.write("src/main.rs", "fn main() { let x = ; }\n");
    assert_eq!(
        project.hook(&["hook-agy", "after-edit"], &call_edit),
        json!({})
    );
    let edits_dir = root.join(".justless/agy-edits");
    assert_eq!(
        fs::read_dir(&edits_dir).unwrap().count(),
        0,
        "after-edit must delete the snapshot"
    );
    let post_input = json!({
        "cwd": root,
        "workspacePaths": [root],
        "conversationId": "conv-edit"
    });
    let injected = project.hook(&["hook-agy", "post-invocation"], &post_input);
    let msg = injected["injectSteps"][0]["systemMessage"]["systemMessage"]
        .as_str()
        .unwrap_or_else(|| panic!("expected systemMessage: {injected}"));
    assert!(
        msg.contains("Justless parse check: after this edit_file")
            && msg.contains("src/main.rs")
            && msg.contains("The file parsed before it."),
        "{msg}"
    );
    // Second `post-invocation` hits fast NotFound -> `{}`.
    assert_eq!(
        project.hook(&["hook-agy", "post-invocation"], &post_input),
        json!({})
    );

    // 3. Valid edit via `replace_file_content` (`TargetFile`) queues nothing.
    project.write("src/main.rs", "fn main() {}\n");
    let call_valid = json!({
        "cwd": root,
        "workspacePaths": [root],
        "conversationId": "conv-edit",
        "toolCall": {"id": "tc-2", "name": "replace_file_content", "args": {"TargetFile": main_path}}
    });
    assert_eq!(
        project.hook(&["hook-agy", "before-edit"], &call_valid),
        json!({"decision": "allow"})
    );
    project.write("src/main.rs", "fn main() { let _x = 1; }\n");
    assert_eq!(
        project.hook(&["hook-agy", "after-edit"], &call_valid),
        json!({})
    );
    assert_eq!(
        project.hook(&["hook-agy", "post-invocation"], &post_input),
        json!({})
    );

    // 4. Errored `after-edit` deletes snapshot and queues nothing.
    let call_err_before = json!({
        "cwd": root,
        "workspacePaths": [root],
        "conversationId": "conv-edit",
        "toolCall": {"id": "tc-3", "name": "edit_file", "args": {"Path": main_path}}
    });
    assert_eq!(
        project.hook(&["hook-agy", "before-edit"], &call_err_before),
        json!({"decision": "allow"})
    );
    project.write("src/main.rs", "fn main() { broken( }\n");
    let call_err_after = json!({
        "cwd": root,
        "workspacePaths": [root],
        "conversationId": "conv-edit",
        "error": "Edit failed",
        "toolCall": {"id": "tc-3", "name": "edit_file", "args": {"Path": main_path}}
    });
    assert_eq!(
        project.hook(&["hook-agy", "after-edit"], &call_err_after),
        json!({})
    );
    assert_eq!(fs::read_dir(&edits_dir).unwrap().count(), 0);
    assert_eq!(
        project.hook(&["hook-agy", "post-invocation"], &post_input),
        json!({})
    );

    // 5. New file with parse error + secret redaction via `write_file`.
    let new_file = root.join("src/new_broken.rs");
    let call_new = json!({
        "cwd": root,
        "workspacePaths": [root],
        "conversationId": "conv-edit",
        "toolCall": {"id": "tc-4", "name": "write_file", "args": {"Path": new_file}}
    });
    assert_eq!(
        project.hook(&["hook-agy", "before-edit"], &call_new),
        json!({"decision": "allow"})
    );
    project.write(
        "src/new_broken.rs",
        "fn leak() { let k = \"ghp_A7bK9mQ2xR4vL8nP1wZ6cT3yF5hJ0dS8eU2g\"; let = ; }\n",
    );
    assert_eq!(
        project.hook(&["hook-agy", "after-edit"], &call_new),
        json!({})
    );
    let new_injected = project.hook(&["hook-agy", "post-invocation"], &post_input);
    let new_msg = new_injected["injectSteps"][0]["systemMessage"]["systemMessage"]
        .as_str()
        .unwrap_or_else(|| panic!("expected systemMessage: {new_injected}"));
    assert!(new_msg.contains("The file is new."), "{new_msg}");
    assert!(
        !new_msg.contains("ghp_A7bK9mQ2xR4vL8nP1wZ6cT3yF5hJ0dS8eU2g"),
        "secret in parse-error line must be redacted: {new_msg}"
    );

    // 6. `init --remove --agent agy` removes all 7 hooks and the file cleanly.
    project.success(&["init", "--remove", "--agent", "agy"]);
    assert!(
        !hooks_path.exists(),
        "init --remove must remove freshly created hooks.json"
    );
}

#[cfg(unix)]
#[test]
fn merged_pipe_and_timeout_preserve_streams_footer_and_exit_status() {
    use std::os::unix::fs::PermissionsExt;

    let project = project();
    let bin_dir = project.base.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let justless_dir = PathBuf::from(env!("CARGO_BIN_EXE_justless"))
        .parent()
        .unwrap()
        .to_path_buf();

    let eslint = bin_dir.join("eslint");
    fs::write(
        &eslint,
        "#!/bin/sh\ni=1\nwhile [ \"$i\" -le 120 ]; do\n  echo \"out-$i padding padding padding padding padding padding\"\n  echo \"err-$i padding padding padding padding padding padding\" >&2\n  i=$((i + 1))\ndone\n",
    )
    .unwrap();
    fs::set_permissions(&eslint, fs::Permissions::from_mode(0o755)).unwrap();

    let pytest = bin_dir.join("pytest");
    fs::write(&pytest, "#!/bin/sh\nsleep 5\n").unwrap();
    fs::set_permissions(&pytest, fs::Permissions::from_mode(0o755)).unwrap();

    let path_env = format!(
        "{}:{}:{}",
        justless_dir.display(),
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run_sh = |script: &str| {
        Command::new("sh")
            .args(["-c", script])
            .current_dir(&project.root)
            .env("PATH", &path_env)
            .env("HOME", project.home())
            .env("XDG_STATE_HOME", project.home().join(".local/state"))
            .env_remove("JUSTLESS_DISABLE")
            .output()
            .unwrap()
    };

    // `2>&1 | head -n 4` and `2>&1 | tail -n 4`: stderr+stdout merge into the filter,
    // while Justless's `[justless run-...]` footer arrives unclipped on stderr.
    for (line, marker_out, marker_err) in [
        ("eslint src 2>&1 | head -n 4", "out-1 ", "err-1 "),
        ("eslint src 2>&1 | tail -n 4", "out-120 ", "err-120 "),
    ] {
        let (rewritten, out) = project.client("hook-codex", line);
        let rewritten = rewritten.unwrap_or_else(|| panic!("{line}: {out}"));
        assert!(
            rewritten.contains("--window --merge --"),
            "{line} -> {rewritten}"
        );
        let result = run_sh(&rewritten);
        assert!(result.status.success(), "{rewritten}");
        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert_eq!(stdout.lines().count(), 4, "{rewritten}: {stdout}");
        assert!(
            stdout.contains(marker_out) && stdout.contains(marker_err),
            "merged stream must contain both stdout and stderr: {stdout}"
        );
        assert!(
            stderr.contains("[justless run-"),
            "footer must print unclipped on stderr: {stderr}"
        );
    }

    // `timeout` exit status (124) is preserved verbatim when wrapped.
    if Command::new("timeout").arg("--version").output().is_ok() {
        let (rewritten, out) = project.client("hook-codex", "timeout 1 pytest");
        let rewritten = rewritten.unwrap_or_else(|| panic!("timeout 1 pytest: {out}"));
        assert!(
            rewritten.starts_with("justless run --hook ")
                && rewritten.ends_with(" -- timeout 1 pytest"),
            "{rewritten}"
        );
        let result = run_sh(&rewritten);
        assert_eq!(
            result.status.code(),
            Some(124),
            "timeout exit 124 must be preserved: stderr={}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[cfg(unix)]
#[test]
fn mixed_chains_with_vars_route_safely_execute_and_respect_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let project = project();
    let bin_dir = project.base.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let justless_dir = PathBuf::from(env!("CARGO_BIN_EXE_justless"))
        .parent()
        .unwrap()
        .to_path_buf();

    // Fake `cargo` that echoes its subcommand and TMPDIR (asserting TMPDIR exists
    // on disk during `cargo test`), so we can execute the rewritten chain in `sh`.
    let cargo_stub = bin_dir.join("cargo");
    fs::write(
        &cargo_stub,
        "#!/bin/sh\necho \"cargo:$1:TMPDIR=$TMPDIR\"\nif [ \"$1\" = test ]; then\n  [ -d \"$TMPDIR\" ] || exit 42\nfi\n",
    )
    .unwrap();
    fs::set_permissions(&cargo_stub, fs::Permissions::from_mode(0o755)).unwrap();

    let path_env = format!(
        "{}:{}:{}",
        justless_dir.display(),
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // 1. Exact mixed verification chain with `tmp=...`, `mkdir -p "$tmp"`,
    // `trap 'rm -rf "$tmp"' EXIT`, and `TMPDIR="$tmp" cargo test --locked`.
    let scratch = project.base.join("chain-tmp-dir");
    let chain = format!(
        "cargo fmt --all && cargo clippy --locked --all-targets -- -D warnings && tmp={} && mkdir -p \"$tmp\" && trap 'rm -rf \"$tmp\"' EXIT && TMPDIR=\"$tmp\" cargo test --locked",
        scratch.display()
    );
    let expected = format!(
        "justless run --hook -- cargo fmt --all && justless run --hook -- cargo clippy --locked --all-targets -- -D warnings && tmp={} && mkdir -p \"$tmp\" && trap 'rm -rf \"$tmp\"' EXIT && TMPDIR=\"$tmp\" justless run --hook -- cargo test --locked",
        scratch.display()
    );

    // Antigravity CLI Tier 2 rewrites all 3 cargo commands with `decision: "ask"`
    // (never auto-allowing) while preserving tmp/mkdir/trap/TMPDIR verbatim.
    let (agy_rewritten, agy_out) = project.client("hook-agy", &chain);
    assert_eq!(
        agy_rewritten.as_deref(),
        Some(expected.as_str()),
        "{agy_out}"
    );
    assert_eq!(
        agy_out["decision"], "ask",
        "mixed var chain must use Tier 2 ask, never allow: {agy_out}"
    );

    // Gemini CLI and Claude Code (without a restrictive mode) route the same chain.
    let (gemini_rewritten, gemini_out) = project.client("hook-gemini", &chain);
    assert_eq!(
        gemini_rewritten.as_deref(),
        Some(expected.as_str()),
        "{gemini_out}"
    );
    let (claude_rewritten, claude_out) = project.client("hook-claude", &chain);
    assert!(
        claude_rewritten.as_deref().is_some_and(|s| s
            .contains("TMPDIR=\"$tmp\" justless run --hook --session ")
            && s.ends_with(" -- cargo test --locked")),
        "claude: {claude_out}"
    );

    // Approving / non-mixed clients (`codex`, `cursor`, `copilot`) stay 100% unchanged!
    for strict_client in ["hook-codex", "hook-cursor", "hook-copilot"] {
        let (rewritten, out) = project.client(strict_client, &chain);
        assert_eq!(
            rewritten, None,
            "{strict_client} must stay unchanged on mixed var chain: {out}"
        );
    }

    // Executing the rewritten chain in `/bin/sh` runs all 3 cargo commands, passes
    // the dynamic `$tmp` into `TMPDIR`, and the `EXIT` trap cleans up `$tmp`.
    let exec = Command::new("sh")
        .args(["-c", agy_rewritten.as_deref().unwrap()])
        .current_dir(&project.root)
        .env("PATH", &path_env)
        .env("HOME", project.home())
        .env("XDG_STATE_HOME", project.home().join(".local/state"))
        .env_remove("JUSTLESS_DISABLE")
        .output()
        .unwrap();
    assert!(
        exec.status.success(),
        "rewritten chain failed: stdout={} stderr={}",
        String::from_utf8_lossy(&exec.stdout),
        String::from_utf8_lossy(&exec.stderr)
    );
    let out_text = String::from_utf8_lossy(&exec.stdout);
    assert!(out_text.contains("cargo:fmt:"), "{out_text}");
    assert!(out_text.contains("cargo:clippy:"), "{out_text}");
    assert!(
        out_text.contains(&format!("cargo:test:TMPDIR={}", scratch.display())),
        "{out_text}"
    );
    assert!(
        !scratch.exists(),
        "EXIT trap must have removed scratch dir {}",
        scratch.display()
    );

    // 2. Other real-world mixed chains with `$var` and redirections route for mixed
    // clients only (`agy` Tier 2 `ask`, `gemini`, `claude`), never `codex`/`cursor`/`copilot`.
    for (cmd, expected_agy) in [
        (
            r#"cargo build && ./target/debug/app "$ARG""#,
            r#"justless run --hook -- cargo build && ./target/debug/app "$ARG""#,
        ),
        (
            r#"git status && echo "branch=$BRANCH""#,
            r#"justless run --hook -- git status && echo "branch=$BRANCH""#,
        ),
        (
            "cargo test && git diff > /tmp/out.patch",
            "justless run --hook -- cargo test && git diff > /tmp/out.patch",
        ),
    ] {
        let (rewritten, out) = project.client("hook-agy", cmd);
        assert_eq!(rewritten.as_deref(), Some(expected_agy), "{cmd}: {out}");
        assert_eq!(out["decision"], "ask", "{cmd}: {out}");
        assert_eq!(
            project.client("hook-gemini", cmd).0.as_deref(),
            Some(expected_agy),
            "gemini: {cmd}"
        );
        assert!(
            project.client("hook-claude", cmd).0.is_some(),
            "claude: {cmd}"
        );
        for strict_client in ["hook-codex", "hook-cursor", "hook-copilot"] {
            assert_eq!(
                project.client(strict_client, cmd).0,
                None,
                "{strict_client} must not route {cmd}"
            );
        }
    }

    // 3. Safety rejections: dynamic `cd`, steering env assignments, command
    // substitution in env assignments, and `eval` stay 100% unwrapped across ALL clients.
    for unsafe_cmd in [
        r#"cd "$dir" && cargo test"#,
        r#"PATH="$x" cargo test"#,
        r#"PATH="$x" && cargo test"#,
        r#"export PATH="$x" && cargo test"#,
        r#"export LD_PRELOAD="$x" && cargo test"#,
        r#"export "$VAR" && cargo test"#,
        "FOO=$(id) cargo test",
        r#"FOO="$(id)" cargo test"#,
        "FOO=`id` cargo test",
        r#"FOO="${PATH:=/evil}" cargo test"#,
        r#"cargo test && eval "$x""#,
        "cargo test && source ./env.sh",
        "cargo test && $(touch x)",
        "cargo test && { cd /tmp; }",
        "cargo test &",
        "(cd src && cargo test)",
    ] {
        for &client in ALL_CLIENTS {
            let (rewritten, out) = project.client(client, unsafe_cmd);
            assert_eq!(
                rewritten, None,
                "{client} must reject unsafe command {unsafe_cmd:?}: {out}"
            );
        }
    }

    // 4. Claude Code `deny` and `ask` rules on any segment (including `$var` segments,
    // piped stages, and nested subshells/substitutions) STILL block rewriting!
    let settings = project.root.join(".claude/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(
        &settings,
        serde_json::to_string_pretty(&json!({
            "permissions": {
                "allow": ["Bash(cargo test *)"],
                "ask": ["Bash(git push *)"],
                "deny": ["Bash(rm *)"]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    for mode in ["default", "bypassPermissions", "auto"] {
        // Denied segment with `$tmp` must NEVER be rewritten.
        for blocked in [
            r#"cargo test && rm -rf "$tmp""#,
            r#"tmp=/var/tmp/x && cargo test && rm -rf "$tmp""#,
            r#"cargo test && echo hi | rm -rf "$tmp""#,
            r#"cargo test && echo "$(rm -rf "$tmp")""#,
            r#"cargo test && (rm -rf "$tmp")"#,
            r#"cargo test && git push origin "$BRANCH""#,
        ] {
            let ans = project.claude(mode, blocked);
            assert_eq!(
                routed(&ans),
                None,
                "claude {mode} must block {blocked:?} under deny/ask rules: {ans}"
            );
        }
    }
    // Safe mixed `$var` chain with NO denied/asked segment routes in `bypassPermissions`
    // and `auto` (without `permissionDecision: "allow"`), and stays left in `default`.
    let safe_var_chain =
        r#"tmp=/var/tmp/x && mkdir -p "$tmp" && TMPDIR="$tmp" cargo test --locked"#;
    for mode in ["bypassPermissions", "auto"] {
        let ans = project.claude(mode, safe_var_chain);
        let cmd = routed(&ans).unwrap_or_else(|| panic!("claude {mode}: {ans}"));
        assert!(
            cmd.contains(r#"TMPDIR="$tmp" justless run --hook --session "#),
            "claude {mode}: {cmd}"
        );
        assert!(
            ans["hookSpecificOutput"]
                .get("permissionDecision")
                .is_none(),
            "claude {mode} must not auto-allow dynamic var chain: {ans}"
        );
    }
    assert_eq!(
        routed(&project.claude("default", safe_var_chain)),
        None,
        "claude default mode must leave uncovered mixed var chain to prompt user"
    );

    // 5. Antigravity Tier 1 (`decision: "allow"`) still works unchanged for plain
    // approvable commands while dynamic env assignments never get Tier 1 `allow`.
    let (plain_rewritten, plain_out) = project.client("hook-agy", "cargo test --locked");
    assert_eq!(
        plain_rewritten.as_deref(),
        Some("justless run --hook -- cargo test --locked")
    );
    assert_eq!(plain_out["decision"], "allow");
    let (dyn_rewritten, dyn_out) =
        project.client("hook-agy", r#"TMPDIR="$tmp" cargo test --locked"#);
    assert_eq!(
        dyn_rewritten.as_deref(),
        Some(r#"TMPDIR="$tmp" justless run --hook -- cargo test --locked"#)
    );
    assert_eq!(dyn_out["decision"], "ask");
}
