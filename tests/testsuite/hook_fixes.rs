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
