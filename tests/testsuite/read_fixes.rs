//! Reads, views and checks whose answer must match what the agent was shown and
//! what its language accepts.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, git, has, lacks, piped, text};

/// A project in a temporary directory, with the home, Claude Code, Codex and state
/// directories in another one, so no command reads or writes the machine's own.
struct Project {
    dir: TempDir,
    home: TempDir,
}

impl Project {
    fn new() -> Self {
        Self {
            dir: TempDir::new().unwrap(),
            home: TempDir::new().unwrap(),
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn write(&self, file: &str, contents: &str) -> PathBuf {
        let path = self.root().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    fn command(&self, cwd: &Path) -> Command {
        let home = self.home.path();
        let mut command = Command::new(BIN);
        command
            .current_dir(cwd)
            .env("HOME", home)
            .env("CLAUDE_CONFIG_DIR", home.join("claude"))
            .env("CODEX_HOME", home.join("codex"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("JUSTLESS_TEST_CLAUDE_MANAGED_DIR", home.join("managed"))
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("JUSTLESS_SESSION");
        command
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd).args(args).output().unwrap()
    }

    fn ok(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.run(cwd, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            text(&output.stderr)
        );
        text(&output.stdout)
    }

    /// `justless edit` with `input` on stdin.
    fn edit(&self, args: &[&str], input: &str) -> Output {
        let mut command = self.command(&self.root());
        command.arg("edit").args(args);
        piped(&mut command, input.as_bytes())
    }

    fn revision(&self, file: &str) -> String {
        let outline: Value =
            serde_json::from_str(&self.ok(&self.root(), &["read", file, "--outline", "--json"]))
                .unwrap();
        outline["revision"].as_str().unwrap().to_owned()
    }

    /// The answer of `justless HANDLER MODE` run in `cwd` to `input`.
    fn hook(&self, cwd: &Path, handler: &str, mode: &str, input: &Value) -> String {
        let mut command = self.command(cwd);
        command.args([handler, mode]);
        let output = piped(&mut command, input.to_string().as_bytes());
        assert!(output.status.success(), "{}", text(&output.stderr));
        text(&output.stdout)
    }

    fn json_hook(&self, cwd: &Path, handler: &str, mode: &str, input: &Value) -> Value {
        serde_json::from_str(&self.hook(cwd, handler, mode, input)).unwrap()
    }
}

/// `n` copies of `item`, each with `{i}` replaced by its number.
fn repeat(n: usize, item: &str) -> String {
    (0..n)
        .map(|i| item.replace("{i}", &i.to_string()))
        .collect()
}

/// The result Claude Code's Read returns for a whole read of `text` at `path`.
fn whole_read(path: &Path, text: &str) -> Value {
    let lines: Vec<&str> = text.split('\n').collect();
    json!({"session_id":"s1","tool_name":"Read","hook_event_name":"PostToolUse",
        "tool_input":{"file_path":path},"tool_response":{"type":"text","file":{
        "filePath":path,"content":text,"numLines":lines.len(),"startLine":1,
        "totalLines":lines.len()}}})
}

fn outline(output: &Value) -> Option<&str> {
    output["hookSpecificOutput"]["updatedToolOutput"]["file"]["content"].as_str()
}

/// Checks that the outline lists each of `code`, lines of `text`, verbatim with
/// its line number after the rows.
#[track_caller]
fn lists(content: &str, text: &str, code: &[&str]) {
    let (_, listed) = content
        .split_once("Code outside these rows:\n")
        .unwrap_or_else(|| panic!("no code listed in:\n{content}"));
    // The hook trims the content's final newline.
    let listed = format!("{listed}\n");
    for line in code {
        let number = text
            .lines()
            .position(|other| other == *line)
            .unwrap_or_else(|| panic!("{line:?} is not a line of the file"))
            + 1;
        has(&listed, &format!("L{number}: {line}\n"));
    }
}

/// A Bash script of 36 functions with a few commands between and after them.
fn deploy_script() -> String {
    format!(
        "#!/bin/bash\nset -euo pipefail\n\n{}# Top-level commands that run:\n{}deploy_step_0\n",
        repeat(
            36,
            "deploy_step_{i}() {\n  echo \"step {i}\"\n  kubectl apply -f manifests/step{i}.yaml\n  kubectl rollout status deployment/app-{i}\n}\n\n",
        ),
        repeat(
            5,
            "rm -rf /var/cache/app/shard{i} && aws s3 sync s3://bucket/shard{i} /srv/shard{i} --delete\n",
        ),
    )
}

const DEPLOY_CODE: [&str; 3] = [
    "set -euo pipefail",
    "rm -rf /var/cache/app/shard3 && aws s3 sync s3://bucket/shard3 /srv/shard3 --delete",
    "deploy_step_0",
];

#[test]
fn outlines_list_top_level_code_in_every_language_that_runs_it() {
    let project = Project::new();
    let root = project.root();
    git(&root, &["init", "-q"]);
    let ts_helpers = repeat(
        30,
        "function helper{i}(n: number): number {\n  const doubled = n * 2\n  return doubled + {i}\n}\n",
    );
    let cases: Vec<(&str, String, Vec<&str>)> = vec![
        ("bin/deploy", deploy_script(), DEPLOY_CODE.to_vec()),
        (
            "bin/tool",
            format!(
                "#!/usr/bin/env python3\nimport shutil\n\n{}shutil.rmtree('/srv/cache')\ntask_0([])\n",
                repeat(
                    30,
                    "def task_{i}(queue):\n    queue.append('an item the worker handles {i}')\n    queue.append('another item')\n    return queue\n\n",
                )
            ),
            vec!["shutil.rmtree('/srv/cache')", "task_0([])"],
        ),
        (
            "tools/build.ps1",
            format!(
                "{}<#\n  Cleanup that runs with the script.\n#>\nRemove-Item -Recurse -Force C:\\cache\\shard0\nInvoke-Step0 -Value 1\n",
                repeat(
                    30,
                    "function Invoke-Step{i} {\n    param([int]$Value)\n    Write-Output \"step {i}\"\n    return $Value * {i}\n}\n\n",
                )
            ),
            vec![
                "Remove-Item -Recurse -Force C:\\cache\\shard0",
                "Invoke-Step0 -Value 1",
            ],
        ),
        (
            "scripts/run.jl",
            format!(
                "{}#=\n  Cleanup that runs with the script.\n=#\nrm(\"/data/out\"; recursive=true)\nstep0(1)\n",
                repeat(
                    30,
                    "\"\"\"\n    step{i}(x)\n\nRun step {i}.\n\"\"\"\nfunction step{i}(x)\n    y = x + {i}\n    println(\"step {i}\")\n    return y * 2\nend\n\n",
                )
            ),
            vec!["rm(\"/data/out\"; recursive=true)", "step0(1)"],
        ),
        (
            "scripts/release.exs",
            format!(
                "defmodule Release do\n{}end\n\nFile.rm_rf!(\"/srv/cache\")\nRelease.step0(1)\n",
                repeat(
                    30,
                    "  def step{i}(x) do\n    IO.puts(\"step {i} of the release\")\n    Logger.info(\"released step {i}\")\n    x + {i}\n  end\n\n",
                )
            ),
            vec!["File.rm_rf!(\"/srv/cache\")", "Release.step0(1)"],
        ),
        (
            "ci/pipeline.groovy",
            format!(
                "{}new File('/srv/cache').deleteDir()\nstep0(1)\n",
                repeat(
                    30,
                    "def step{i}(x) {\n    println \"step {i} of the pipeline, which runs in order\"\n    def y = x * {i}\n    return y\n}\n\n",
                )
            ),
            vec!["new File('/srv/cache').deleteDir()", "step0(1)"],
        ),
        (
            "build.gradle",
            format!(
                "plugins {{\n    id 'java'\n}}\n\n{}tasks.register('wipe') {{\n    doLast {{ delete 'build' }}\n}}\n",
                repeat(
                    30,
                    "def helper{i}(x) {\n    println \"helper {i} of the build, which runs in order\"\n    def y = x * {i}\n    return y\n}\n\n",
                )
            ),
            // A registered task is a row.
            vec!["plugins {", "    id 'java'"],
        ),
        (
            "src/Setup.vue",
            format!(
                "<template>\n  <ul><li v-for=\"i in items\">{{{{ i }}}}</li></ul>\n</template>\n<script setup lang=\"ts\">\nimport {{ ref, onMounted }} from 'vue'\nconst items = ref<number[]>([])\nonMounted(async () => {{\n  await fetch('/api/items', {{ method: 'DELETE' }})\n  items.value = []\n}})\n{ts_helpers}</script>\n"
            ),
            vec![
                "<template>",
                "  <ul><li v-for=\"i in items\">{{ i }}</li></ul>",
                "onMounted(async () => {",
                "  await fetch('/api/items', { method: 'DELETE' })",
                "</script>",
            ],
        ),
        (
            "src/Counter.svelte",
            format!(
                "<script lang=\"ts\">\n  let count = 0\n  localStorage.removeItem('cache')\n{ts_helpers}</script>\n<button on:click={{() => count++}}>{{count}}</button>\n"
            ),
            vec![
                "  localStorage.removeItem('cache')",
                "<button on:click={() => count++}>{count}</button>",
            ],
        ),
        (
            "src/Page.astro",
            format!(
                "---\nimport Card from './Card.astro'\nawait fetch('/api/purge', {{ method: 'POST' }})\n{ts_helpers}---\n<h1>Purged</h1>\n"
            ),
            vec![
                "await fetch('/api/purge', { method: 'POST' })",
                "<h1>Purged</h1>",
            ],
        ),
    ];
    for (file, source, code) in &cases {
        let path = project.write(file, source);
        let output = project.json_hook(
            &root,
            "hook-claude",
            "after-read",
            &whole_read(&path, source),
        );
        let content = outline(&output).unwrap_or_else(|| panic!("{file}: no outline: {output}"));
        lists(content, source, code);
    }
    // A component whose markup is more than 20 lines keeps its full read.
    let markup = format!(
        "<template>\n{}</template>\n<script setup lang=\"ts\">\n{ts_helpers}</script>\n",
        repeat(24, "  <p class=\"row\">Row {i}</p>\n")
    );
    let path = project.write("src/Table.vue", &markup);
    assert_eq!(
        project.json_hook(
            &root,
            "hook-claude",
            "after-read",
            &whole_read(&path, &markup)
        ),
        json!({})
    );
    // Block comments and the docstrings of Julia definitions are not code.
    for file in ["tools/build.ps1", "scripts/run.jl"] {
        let path = root.join(file);
        let source = fs::read_to_string(&path).unwrap();
        let output = project.json_hook(
            &root,
            "hook-claude",
            "after-read",
            &whole_read(&path, &source),
        );
        let content = outline(&output).unwrap();
        lacks(content, "Cleanup that runs");
        lacks(content, "Run step");
    }
}

/// The shell entry points treat a script named by its `#!` line as they treat one
/// named `.sh`: none hides its commands.
#[test]
fn shell_reads_of_an_extensionless_script_keep_its_commands() {
    let project = Project::new();
    let root = project.root();
    git(&root, &["init", "-q"]);
    let script = deploy_script();
    for file in ["bin/deploy", "bin/deploy.sh"] {
        project.write(file, &script);
        // Codex reroutes a `cat` only when the outline would shorten it.
        let event = json!({"session_id":"s1","hook_event_name":"PreToolUse","tool_name":"Bash",
            "cwd":root,"permission_mode":"default","tool_input":{"command":format!("cat {file}")}});
        assert_eq!(
            project.json_hook(&root, "hook-codex", "before-command", &event),
            json!({}),
            "{file}"
        );
        // A workspace read prints what it cannot outline whole.
        assert_eq!(
            project.ok(&root, &["read", "--shell-range", "+1", "--", file]),
            script,
            "{file}"
        );
        // Claude Code's result passes through.
        let command = format!("sed -n '1,300p' {file}");
        let event = json!({"session_id":"s1","hook_event_name":"PostToolUse","tool_name":"Bash",
            "cwd":root,"tool_input":{"command":command},"tool_response":{"stdout":script.trim_end_matches('\n'),
            "stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false}});
        assert_eq!(
            project.json_hook(&root, "hook-claude", "after-bash", &event),
            json!({}),
            "{file}"
        );
    }
}

/// Twelve-line functions named `prefix0` to `prefix5`, as a Rust library.
fn library(prefix: &str) -> String {
    repeat(
        6,
        &format!(
            "pub fn {prefix}{{i}}(x: u32) -> u32 {{\n{}    v0\n}}\n\n",
            (0..12)
                .map(|k| format!("    let v{k} = x + {{i}} + {k} * 1000;\n"))
                .collect::<String>()
        ),
    )
}

#[test]
fn compaction_forgets_the_session_in_nested_projects() {
    let project = Project::new();
    let outer = project.root();
    let inner = outer.join("svc");
    project.write("src/lib.rs", &library("outer"));
    project.write("svc/src/lib.rs", &library("inner"));
    git(&outer, &["init", "-q"]);
    git(&inner, &["init", "-q"]);
    // The command the Claude Code hook routes for the agent's session.
    let routed = |cwd: &Path, symbol: &str| -> Vec<String> {
        let event = json!({"session_id":"s1","hook_event_name":"PreToolUse","tool_name":"Bash",
            "cwd":cwd,"permission_mode":"bypassPermissions",
            "tool_input":{"command":format!("justless context --symbol {symbol}")}});
        let answer = project.json_hook(cwd, "hook-claude", "before-command", &event);
        let command = answer["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap_or_else(|| panic!("{answer}"));
        has(command, " --session ");
        let words = shlex::split(command).unwrap();
        assert_eq!(words[0], "justless", "{command}");
        words[1..].to_vec()
    };
    let view = |cwd: &Path, args: &[String]| {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        project.ok(cwd, &args)
    };
    let inner_view = routed(&inner, "inner3");
    let outer_view = routed(&outer, "outer3");
    let other: Vec<String> = ["context", "--symbol", "inner3", "--session", "other"]
        .map(String::from)
        .to_vec();
    for (cwd, args) in [
        (&inner, &inner_view),
        (&outer, &outer_view),
        (&inner, &other),
    ] {
        lacks(&view(cwd, args), "since you read it");
        has(&view(cwd, args), "unchanged since you read it");
    }
    assert!(inner.join(".justless/seen").is_dir());

    // The hook runs in the outer project, and the session's views print whole in
    // both. Another session's views in the nested project are its own.
    let compact =
        json!({"session_id":"s1","hook_event_name":"PreCompact","trigger":"auto","cwd":outer});
    project.hook(&outer, "hook-claude", "before-compact", &compact);
    lacks(&view(&inner, &inner_view), "since you read it");
    lacks(&view(&outer, &outer_view), "since you read it");
    has(&view(&inner, &other), "unchanged since you read it");

    // The same after compaction, from a project without state of its own.
    has(&view(&inner, &inner_view), "unchanged since you read it");
    let empty = outer.join("empty");
    fs::create_dir(&empty).unwrap();
    git(&empty, &["init", "-q"]);
    let start =
        json!({"session_id":"s1","hook_event_name":"SessionStart","source":"compact","cwd":empty});
    project.hook(&empty, "hook-claude", "after-compact", &start);
    assert!(!empty.join(".justless").exists());
    lacks(&view(&inner, &inner_view), "since you read it");
}

/// Edits of a crate that enables unstable features with `#![feature(…)]`, whose
/// syntax syn does not parse.
const NIGHTLY: [(&str, &str); 7] = [
    (
        "super_let",
        "fn a() -> u32 {\n    let r = { super let x = 1; &x };\n    *r\n}",
    ),
    (
        "gen_blocks",
        "fn a() -> impl Iterator<Item = u32> {\n    gen { yield 1; yield 2; }\n}",
    ),
    ("yeet_expr", "fn a() -> Option<u32> {\n    do yeet;\n}"),
    ("pin_ergonomics", "fn a(x: &pin mut u32) -> u32 {\n    1\n}"),
    (
        "unsafe_binders",
        "fn a(_x: unsafe<'a> &'a u32) -> u32 {\n    1\n}",
    ),
    (
        "ergonomic_clones",
        "fn a() {\n    let s = String::new();\n    let f = use || s.len();\n    let _ = f;\n}",
    ),
    (
        "guard_patterns",
        "fn a(x: Option<u32>) -> u32 {\n    match x {\n        (Some(v) if v > 0) | None => 1,\n        _ => 0,\n    }\n}",
    ),
];

#[test]
fn feature_gated_crates_get_syn_errors_reported_not_rejected() {
    let project = Project::new();
    let root = project.root();
    git(&root, &["init", "-q"]);
    project.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    let stable = "fn a() -> u32 {\n    1\n}\n";
    for (feature, body) in NIGHTLY {
        // The same edit of a crate on stable Rust is rejected.
        project.write("src/lib.rs", stable);
        let rejected = project.edit(
            &[
                "src/lib.rs",
                "a",
                "--revision",
                &project.revision("src/lib.rs"),
            ],
            body,
        );
        assert_eq!(
            rejected.status.code(),
            Some(2),
            "{feature}: {}",
            text(&rejected.stderr)
        );
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), stable);

        let before = format!("#![feature({feature})]\n{stable}");
        project.write("src/lib.rs", &before);
        let written = project.edit(
            &[
                "src/lib.rs",
                "a",
                "--revision",
                &project.revision("src/lib.rs"),
            ],
            body,
        );
        assert!(
            written.status.success(),
            "{feature}: {}",
            text(&written.stderr)
        );
        has(
            &text(&written.stderr),
            "warning: src/lib.rs may no longer parse",
        );
        has(&fs::read_to_string(root.join("src/lib.rs")).unwrap(), body);
    }

    // A module of a crate whose root enables a feature.
    project.write("src/lib.rs", "#![feature(super_let)]\nmod m;\n");
    project.write("src/m.rs", stable);
    let (_, body) = NIGHTLY[0];
    let written = project.edit(
        &["src/m.rs", "a", "--revision", &project.revision("src/m.rs")],
        body,
    );
    assert!(written.status.success(), "{}", text(&written.stderr));
    has(
        &text(&written.stderr),
        "warning: src/m.rs may no longer parse",
    );

    // Claude Code's Edit gets the note for a lagging parser.
    let path = project.write("src/m.rs", stable);
    let after = format!("{body}\n");
    fs::write(&path, &after).unwrap();
    let event = json!({"session_id":"s1","hook_event_name":"PostToolUse","tool_name":"Edit","cwd":root,
        "tool_input":{"file_path":path},"tool_response":{"originalFile":stable}});
    let note = project.json_hook(&root, "hook-claude", "after-edit", &event);
    has(
        note["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_default(),
        "The parser may not know newer syntax",
    );

    // Brackets that no longer pair up are wrong in any Rust.
    project.write("src/m.rs", stable);
    let rejected = project.edit(
        &["src/m.rs", "a", "--revision", &project.revision("src/m.rs")],
        "fn a() -> u32 {\n    1 +",
    );
    assert_eq!(
        rejected.status.code(),
        Some(2),
        "{}",
        text(&rejected.stderr)
    );
    has(&text(&rejected.stderr), "unclosed `{`");
}

/// Claude Code saves a large Bash result to a file and shows the agent its first
/// 2,000 characters. A read the hook passes through was not shown whole.
#[test]
fn a_saved_shell_read_that_passes_through_is_not_recorded_as_shown() {
    let project = Project::new();
    let root = project.root();
    git(&root, &["init", "-q"]);
    // Thirty lines of top-level code keep the whole read, so the hook passes it on.
    let source = format!(
        "{}{}",
        repeat(
            60,
            &format!(
                "def task_{{i}}(queue):\n{}    return queue\n\n",
                "    queue.append('a task item that the worker processes in order')\n".repeat(10)
            ),
        ),
        repeat(30, "register(task_{i})\n")
    );
    project.write("tasks.py", &source);
    let session = TempDir::new().unwrap();
    let saved = session.path().join("tool-results/out.txt");
    fs::create_dir_all(saved.parent().unwrap()).unwrap();
    fs::write(&saved, &source).unwrap();
    let preview: String = source.chars().take(2000).collect();
    let event = json!({"session_id":"s1","hook_event_name":"PostToolUse","tool_name":"Bash","cwd":root,
        "tool_input":{"command":"sed -n '1,900p' tasks.py"},
        "tool_response":{"stdout":preview,"stderr":"","interrupted":false,"isImage":false,
        "noOutputExpected":false,"persistedOutputPath":saved,"persistedOutputSize":source.len()}});
    assert_eq!(
        project.json_hook(&root, "hook-claude", "after-bash", &event),
        json!({})
    );
    let resume = project.ok(&root, &["resume"]);
    lacks(&resume, "Read tasks.py");
    has(&resume, "Outline tasks.py:1-810");
}
