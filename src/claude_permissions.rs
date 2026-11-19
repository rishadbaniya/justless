//! Claude Code permission rules for the Bash command hook.
//!
//! Claude Code checks its permission rules against the command a PreToolUse hook
//! returns, so a routed `justless run --hook -- cargo test` no longer matches a
//! `Bash(cargo test *)` allow rule, nor ask and deny rules written for the original.
//! When Claude Code reports its permission mode, the hook therefore judges the
//! original command against the rules it can read and rewrites only when that
//! keeps the outcome Claude Code would reach on its own.
//!
//! Contract, from <https://code.claude.com/docs/en/permissions> and
//! <https://code.claude.com/docs/en/settings>:
//! - "Rules are evaluated in order: deny, then ask, then allow."
//! - "Every subcommand must be allowed for a compound command to run without a
//!   prompt", while "Deny and ask rules apply when any subcommand matches them".
//! - "`Bash(*)` is equivalent to `Bash`", "A `*` at the end, with a space before
//!   it, also matches the bare command", "The space before a trailing `*` is part of
//!   the rule", and "The `:*` suffix is an equivalent way to write a trailing
//!   wildcard".
//! - Before matching, Claude Code strips the wrappers `timeout`, `time`, `nice`,
//!   `nohup`, `stdbuf`, `command`, `builtin`, `noglob` and bare `xargs`, and "A deny
//!   or ask rule matches past any leading assignment".
//! - Lists such as `permissions.allow` merge across the managed, user, project and
//!   local settings files. Managed files live in `/Library/Application
//!   Support/ClaudeCode/` on macOS and `/etc/claude-code/` on Linux, as
//!   `managed-settings.json` followed by `managed-settings.d/*.json` in alphabetical
//!   order, without hidden files.
//! - "If you start Claude Code in a subdirectory of a git repository, it reads and
//!   writes [`.claude/settings.local.json`] at the repository root", and "In a
//!   worktree, it uses the file at the main checkout's root". It keeps the file with
//!   `.claude/settings.json` instead "outside a git repository, when the repository
//!   root is your home directory, on Windows, or when the repository root or its
//!   `.git` or `.claude` entry isn't owned by your user", and it "still reads a file
//!   an earlier version left there [in the starting directory] alongside the root
//!   file". Claude Code before v2.1.211 read only the starting directory's file.
//! - `permissions.defaultMode` comes from the highest of managed, local, project and
//!   user settings that sets it, and "values `auto` and `bypassPermissions` don't
//!   take effect from project or local settings".
//!
//! The rules are read so that every mistake is safe: allow rules count only in a
//! narrow form that Claude Code certainly honors, while ask and deny rules match
//! loosely, and a settings file that exists but cannot be read or parsed counts as
//! one that might deny. A command that a rule might deny or ask about is left for
//! Claude Code to judge, as is, in most modes, a command no rule allows; the
//! post-run hook still compacts its successful output.
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Internal, for tests: the directory read instead of the system's managed
/// settings directory.
pub const MANAGED_OVERRIDE: &str = "JUSTLESS_TEST_CLAUDE_MANAGED_DIR";

/// Where Claude Code's settings files are.
pub struct Sources {
    /// The managed settings directory.
    managed: PathBuf,
    /// The user settings file, or `None` when neither `CLAUDE_CONFIG_DIR` nor a home
    /// directory locates it.
    user: Option<PathBuf>,
    /// The directory whose `.claude/settings.json` and `.claude/settings.local.json`
    /// Claude Code reads for the session.
    project: PathBuf,
    /// The root of the main checkout of the Git repository that holds `project`,
    /// where Claude Code keeps the local settings file, with whether that place is
    /// certain enough for the file's allow rules to count.
    repository: Option<(PathBuf, bool)>,
    /// Other directories whose `.claude/` files may apply, such as the repository
    /// root that keeps the local file of a session started in a subdirectory. Only
    /// their ask and deny rules count.
    nearby: Vec<PathBuf>,
}

impl Sources {
    /// The files Claude Code reads for a session in `project`, with `root`, the
    /// workspace root, as a nearby directory when it differs.
    pub fn locate(project: &Path, root: &Path) -> Self {
        let variable = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
        let managed = variable(MANAGED_OVERRIDE).map_or_else(system_managed, PathBuf::from);
        let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let user = variable("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| variable(home).map(|home| PathBuf::from(home).join(".claude")))
            .map(|dir| dir.join("settings.json"));
        Self {
            managed,
            user,
            project: project.to_path_buf(),
            repository: repository(project).filter(|(dir, _)| !same(dir, project)),
            nearby: (!same(project, root))
                .then(|| root.to_path_buf())
                .into_iter()
                .collect(),
        }
    }

    /// The sources for a Claude Code hook: the project is `$CLAUDE_PROJECT_DIR`, the
    /// directory the session started in, or else the workspace root.
    pub fn for_hook(root: &Path) -> Self {
        let project = std::env::var_os("CLAUDE_PROJECT_DIR")
            .filter(|value| !value.is_empty())
            .map_or_else(|| root.to_path_buf(), PathBuf::from);
        Self::locate(&project, root)
    }
}

/// The root of the Git repository that holds `dir`, resolved through a linked
/// worktree to its main checkout, as Claude Code places the local settings file.
/// The flag is false where Claude Code may keep the file elsewhere: beside a `.git`
/// file that is not a linked worktree's, such as a submodule's, and in the cases
/// [`owned`] checks.
fn repository(dir: &Path) -> Option<(PathBuf, bool)> {
    let mut top = None;
    for ancestor in dir.ancestors() {
        match fs::symlink_metadata(ancestor.join(".git")) {
            Ok(metadata) => {
                top = Some((ancestor, metadata.is_dir()));
                break;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(_) => return None,
        }
    }
    let (top, checkout) = top?;
    // A linked worktree's `.git` file names its Git directory, whose `commondir`
    // leads to the main checkout's `.git` directory.
    let main = if checkout {
        Some(top.to_path_buf())
    } else {
        fs::read_to_string(top.join(".git"))
            .ok()
            .and_then(|text| Some(top.join(text.trim().strip_prefix("gitdir:")?.trim())))
            .and_then(|git| {
                let common = fs::read_to_string(git.join("commondir")).ok()?;
                git.join(common.trim()).canonicalize().ok()
            })
            .filter(|common| common.file_name() == Some(".git".as_ref()))
            .and_then(|common| common.parent().map(Path::to_path_buf))
    };
    Some(match main {
        Some(main) => {
            let certain = owned(&main);
            (main, certain)
        }
        None => (top.to_path_buf(), false),
    })
}

/// Whether Claude Code keeps the local settings file at the repository root
/// `root`: not on Windows, not in the home directory, and only when the effective
/// user owns the root and its `.git` and `.claude` entries.
#[cfg(unix)]
fn owned(root: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return false;
    };
    // SAFETY: geteuid has no preconditions and cannot fail.
    let user = unsafe { libc::geteuid() };
    let mine = |metadata: io::Result<fs::Metadata>| metadata.is_ok_and(|m| m.uid() == user);
    !same(root, Path::new(&home))
        && mine(fs::metadata(root))
        && mine(fs::symlink_metadata(root.join(".git")))
        && mine(fs::symlink_metadata(root.join(".claude")))
}

#[cfg(not(unix))]
fn owned(_: &Path) -> bool {
    false
}

/// Whether two paths name the same file.
fn same(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

fn system_managed() -> PathBuf {
    PathBuf::from(if cfg!(target_os = "macos") {
        "/Library/Application Support/ClaudeCode"
    } else if cfg!(windows) {
        r"C:\Program Files\ClaudeCode"
    } else {
        "/etc/claude-code"
    })
}

/// The Bash permission rules found in the settings files.
#[derive(Default)]
pub struct Rules {
    /// Allow rules from the files Claude Code certainly reads for the session.
    allow: Vec<String>,
    ask: Vec<String>,
    deny: Vec<String>,
    /// Files that exist but could not be read or used, so they might deny anything.
    broken: Vec<Broken>,
    /// The `permissions.defaultMode` that applies, with the scope of its file.
    default_mode: Option<(Scope, String)>,
    /// A file sets `blockReadsOutsideWorkingDirectories` or
    /// `blockWritesOutsideWorkingDirectories`.
    blocks_paths: bool,
    /// Every allow rule with the file it is in, for `doctor`.
    found: Vec<(PathBuf, String)>,
}

/// A settings file, or the managed drop-in directory, that exists but cannot be
/// read or used.
#[derive(Debug)]
pub struct Broken {
    pub path: PathBuf,
    /// What is wrong, such as `expected value at line 1 column 1`.
    pub problem: String,
}

/// One settings file as read.
enum Loaded {
    Missing,
    Broken(String),
    Settings(Value),
}

fn read(path: &Path) -> Loaded {
    match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(value) if value.is_object() => Loaded::Settings(value),
            Ok(_) => Loaded::Broken("not a JSON object".into()),
            Err(error) => Loaded::Broken(format!("not valid JSON: {error}")),
        },
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Loaded::Missing
        }
        Err(error) => Loaded::Broken(error.to_string()),
    }
}

/// The managed files in the order Claude Code merges them, or the problem when
/// the drop-in directory exists but cannot be listed.
fn managed_files(dir: &Path) -> Result<Vec<PathBuf>, Broken> {
    let mut files = vec![dir.join("managed-settings.json")];
    let drop_ins = dir.join("managed-settings.d");
    let unlisted = |problem: String| Broken {
        path: drop_ins.clone(),
        problem,
    };
    match fs::read_dir(&drop_ins) {
        Ok(entries) => {
            let mut names = BTreeSet::new();
            for entry in entries {
                let name = entry
                    .map_err(|error| unlisted(error.to_string()))?
                    .file_name();
                let name = name
                    .into_string()
                    .map_err(|_| unlisted("a file name is not UTF-8".into()))?;
                if !name.starts_with('.') && name.ends_with(".json") {
                    names.insert(name);
                }
            }
            files.extend(names.into_iter().map(|name| drop_ins.join(name)));
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) => {}
        Err(error) => return Err(unlisted(error.to_string())),
    }
    Ok(files)
}

/// A rule list under `permissions`, or the problem when the file holds something
/// Claude Code would reject. Allow lists skip entries that are not strings, as
/// Claude Code skips malformed entries; for ask and deny such an entry might deny.
fn list(settings: &Value, name: &str) -> Result<Vec<String>, String> {
    let permissions = match settings.get("permissions") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Object(permissions)) => permissions,
        Some(_) => return Err("`permissions` is not an object".into()),
    };
    match permissions.get(name) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let rules: Vec<String> = items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect();
            if name == "allow" || rules.len() == items.len() {
                Ok(rules)
            } else {
                Err(format!(
                    "`permissions.{name}` holds a rule that is not a string"
                ))
            }
        }
        Some(_) => Err(format!("`permissions.{name}` is not a list")),
    }
}

/// Whether managed settings make Claude Code ignore every allow rule outside them.
/// Any value but `false` counts.
fn managed_only(settings: &Value) -> bool {
    [
        settings.get("allowManagedPermissionRulesOnly"),
        settings["permissions"].get("allowManagedPermissionRulesOnly"),
    ]
    .into_iter()
    .flatten()
    .any(|value| !matches!(value, Value::Null | Value::Bool(false)))
}

/// Where a settings file sits, from the lowest precedence to the highest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Scope {
    /// A file Claude Code may read for the session: only its ask and deny rules
    /// count.
    Nearby,
    User,
    Project,
    /// The local file in the directory the session started in.
    Local,
    /// The local file at the repository root, whose values win over the other's.
    RepositoryLocal,
    Managed,
}

/// Read every settings file the sources name.
pub fn load(sources: &Sources) -> Rules {
    let mut rules = Rules::default();
    let mut files: Vec<(PathBuf, Scope)> = Vec::new();
    match managed_files(&sources.managed) {
        Ok(managed) => files.extend(managed.into_iter().map(|file| (file, Scope::Managed))),
        Err(broken) => rules.broken.push(broken),
    }
    match &sources.user {
        Some(user) => files.push((user.clone(), Scope::User)),
        None => rules.broken.push(Broken {
            path: PathBuf::from("~/.claude/settings.json"),
            problem: "neither CLAUDE_CONFIG_DIR nor HOME is set".into(),
        }),
    }
    let claude = sources.project.join(".claude");
    files.push((claude.join("settings.json"), Scope::Project));
    files.push((claude.join("settings.local.json"), Scope::Local));
    if let Some((dir, certain)) = &sources.repository {
        let scope = if *certain {
            Scope::RepositoryLocal
        } else {
            Scope::Nearby
        };
        files.push((dir.join(".claude").join("settings.local.json"), scope));
    }
    for dir in &sources.nearby {
        for name in ["settings.json", "settings.local.json"] {
            files.push((dir.join(".claude").join(name), Scope::Nearby));
        }
    }
    let mut managed_allow = Vec::new();
    let mut only_managed = false;
    // A file listed twice, such as the repository's local file as a nearby one,
    // counts once, in the scope listed first.
    let mut seen = Vec::new();
    for (file, scope) in files {
        let key = file.canonicalize().unwrap_or_else(|_| file.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let settings = match read(&file) {
            Loaded::Missing => continue,
            Loaded::Broken(problem) => {
                rules.broken.push(Broken {
                    path: file,
                    problem,
                });
                continue;
            }
            Loaded::Settings(settings) => settings,
        };
        let (allow, ask, deny) = match (
            list(&settings, "allow"),
            list(&settings, "ask"),
            list(&settings, "deny"),
        ) {
            (Ok(allow), Ok(ask), Ok(deny)) => (allow, ask, deny),
            (Err(problem), _, _) | (_, Err(problem), _) | (_, _, Err(problem)) => {
                rules.broken.push(Broken {
                    path: file,
                    problem,
                });
                continue;
            }
        };
        if scope == Scope::Managed {
            only_managed |= managed_only(&settings);
            managed_allow.extend(allow.iter().cloned());
        }
        if let Some(mode) = settings["permissions"]["defaultMode"].as_str()
            && scope != Scope::Nearby
            && (matches!(scope, Scope::User | Scope::Managed)
                || !matches!(mode, "auto" | "bypassPermissions"))
            && rules
                .default_mode
                .as_ref()
                .is_none_or(|(rank, _)| scope > *rank)
        {
            rules.default_mode = Some((scope, mode.to_owned()));
        }
        rules.blocks_paths |= [
            "blockReadsOutsideWorkingDirectories",
            "blockWritesOutsideWorkingDirectories",
        ]
        .iter()
        .any(|key| {
            settings["permissions"]
                .get(key)
                .is_some_and(|value| !matches!(value, Value::Null | Value::Bool(false)))
        });
        if scope != Scope::Nearby {
            rules.allow.extend(allow.iter().cloned());
        }
        rules
            .found
            .extend(allow.into_iter().map(|rule| (file.clone(), rule)));
        rules.ask.extend(ask);
        rules.deny.extend(deny);
    }
    if only_managed {
        rules.allow = managed_allow;
    }
    rules
}

/// How Claude Code's own rules would treat a command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A deny rule might match, or a settings file might hold one.
    Deny,
    /// An ask rule might match.
    Ask,
    /// Allow rules certainly cover every command of the line.
    Allow,
    Unknown,
}

impl Rules {
    /// Judge a command line by its top-level commands, as split at `&&`, `||` and `;`.
    pub fn verdict(&self, segments: &[&str]) -> Verdict {
        if !self.broken.is_empty() {
            return Verdict::Deny;
        }
        let candidates: Vec<String> = segments.iter().flat_map(|s| candidates(s)).collect();
        let any = |rules: &[String]| {
            rules
                .iter()
                .any(|rule| candidates.iter().any(|text| loosely(rule, text)))
        };
        // Claude Code also checks the paths that file commands in Bash name against
        // Read and Edit rules and the working directories, which this check does
        // not model, so such a command is never taken as allowed while any apply.
        // Those checks do not see the paths inside `justless run -- …`, so while a
        // file rule might deny or ask, a line that runs such a command is judged
        // as that rule would be, and stays as written in every mode.
        let file_rule = |rules: &[String]| rules.iter().any(|rule| names_file_tool(rule));
        let reads = segments.iter().any(|segment| reads_files(segment));
        let guarded = self.blocks_paths || file_rule(&self.deny) || file_rule(&self.ask);
        if any(&self.deny) || reads && file_rule(&self.deny) {
            Verdict::Deny
        } else if any(&self.ask) || reads && file_rule(&self.ask) {
            Verdict::Ask
        } else if !segments.is_empty()
            && segments.iter().all(|segment| {
                approvable(segment, guarded) && self.allow.iter().any(|rule| allows(rule, segment))
            })
        {
            Verdict::Allow
        } else {
            Verdict::Unknown
        }
    }
}

/// Programs that run another command given in their arguments, or change where
/// the commands after them run. A command that starts with one is never taken as
/// allowed, and an ask or deny rule also matches the command inside it.
const WRAPPERS: &[&str] = &[
    "timeout",
    "time",
    "nice",
    "nohup",
    "stdbuf",
    "command",
    "builtin",
    "noglob",
    "nocorrect",
    "xargs",
    "env",
    "exec",
    "sudo",
    "doas",
    "watch",
    "setsid",
    "ionice",
    "flock",
    "chrt",
    "taskset",
];
const DIRECTORY: &[&str] = &["cd", "pushd", "popd", "chdir", "source", ".", "eval"];
/// Programs that read or print files they name, which Claude Code may check
/// against Read and Edit rules. Its docs name `cat`, `head`, `tail`, `sed` and
/// `tee`; the rest are read the same way to be safe.
const FILE_READERS: &[&str] = &[
    "cat", "head", "tail", "sed", "tee", "less", "more", "nl", "grep", "egrep", "fgrep", "rg",
    "ugrep", "ag", "ack", "find", "fd", "ls", "tree", "wc", "diff", "cut", "sort", "uniq", "awk",
    "jq", "xxd", "od", "strings", "file", "stat", "du", "bat", "git",
];

/// Whether a rule may name a file tool, such as `Read(./.env)` or `Edit`.
fn names_file_tool(rule: &str) -> bool {
    let tool = rule.trim().split('(').next().unwrap_or("").trim();
    ["Read", "Edit", "Write", "NotebookEdit"]
        .iter()
        .any(|name| tool.eq_ignore_ascii_case(name) || tool.contains('*') && glob(tool, name))
}

/// The texts an ask or deny rule is matched against: the command as written, and
/// its words without quotes, a final `2>&1`, leading assignments or wrappers.
fn candidates(segment: &str) -> Vec<String> {
    let (words, starts) = starts(segment);
    let mut texts = vec![segment.trim().to_owned()];
    texts.extend(
        starts
            .into_iter()
            .filter(|&start| start < words.len())
            .map(|start| words[start..].join(" ")),
    );
    texts
}

/// Whether any word that may start the command, after leading assignments and
/// wrappers, names a program that reads the files it names, such as `rg`, `cat`
/// or `git`.
fn reads_files(segment: &str) -> bool {
    let (words, starts) = starts(segment);
    starts
        .into_iter()
        .filter_map(|start| words.get(start))
        .any(|word| FILE_READERS.contains(&basename(word)))
}

/// A command's words without quotes or a final `2>&1`, and each index at which
/// the command may start: the first word, the word after each leading
/// assignment, and each of the few words after a wrapper, which may take options
/// and values.
fn starts(segment: &str) -> (Vec<String>, BTreeSet<usize>) {
    let mut words = shlex::split(segment)
        .unwrap_or_else(|| segment.split_whitespace().map(str::to_owned).collect());
    if words.len() > 1 && words.last().is_some_and(|word| word == "2>&1") {
        words.pop();
    }
    let mut starts = BTreeSet::from([0]);
    let mut pending = vec![0];
    while let Some(at) = pending.pop() {
        let Some(word) = words.get(at) else { continue };
        let next: Vec<usize> = if assignment(word) {
            vec![at + 1]
        } else if WRAPPERS.contains(&basename(word)) {
            (at + 1..words.len().min(at + 6)).collect()
        } else {
            Vec::new()
        };
        for start in next {
            if starts.insert(start) {
                pending.push(start);
            }
        }
    }
    (words, starts)
}

fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        let name = name.strip_suffix('+').unwrap_or(name);
        name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Whether an ask or deny rule might match a command text. Any tool name that may
/// be `Bash` counts, `*` stands for any text, and a `:*` prefix needs no word
/// boundary.
fn loosely(rule: &str, text: &str) -> bool {
    let rule = rule.trim();
    let (tool, spec) = match rule.find('(') {
        Some(open) if rule.ends_with(')') => (&rule[..open], Some(&rule[open + 1..rule.len() - 1])),
        _ => (rule, None),
    };
    let tool = tool.trim();
    if !(tool.eq_ignore_ascii_case("Bash") || tool.contains('*') && glob(tool, "Bash")) {
        return false;
    }
    let spec = match spec.map(str::trim) {
        None | Some("" | "*") => return true,
        Some(spec) => spec,
    };
    if let Some(prefix) = spec.strip_suffix(":*") {
        return glob(&format!("{prefix}*"), text);
    }
    glob(spec, text) || glob(spec, &format!("{text} "))
}

/// Whether `text` matches `pattern`, where `*` stands for any text.
fn glob(pattern: &str, text: &str) -> bool {
    let (pattern, text) = (pattern.as_bytes(), text.as_bytes());
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if pattern.get(p) == Some(&b'*') {
            star = Some((p, t));
            p += 1;
        } else if pattern.get(p) == Some(&text[t]) {
            p += 1;
            t += 1;
        } else if let Some((at, from)) = star {
            p = at + 1;
            t = from + 1;
            star = Some((at, from + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&byte| byte == b'*')
}

/// Whether an allow rule may approve this command at all: a literal command, other
/// than a final `2>&1`, that starts with its program, not with an assignment, a
/// wrapper, `find` or a change of directory, nor, while `guarded`, with a program
/// that reads files it names.
fn approvable(segment: &str, guarded: bool) -> bool {
    let words = segment.strip_suffix(" 2>&1").unwrap_or(segment);
    crate::agents::literal_command(words)
        .and_then(|words| words.into_iter().next())
        .is_some_and(|program| {
            let name = basename(&program);
            !(assignment(&program)
                || WRAPPERS.contains(&name)
                || DIRECTORY.contains(&name)
                || name == "find"
                || guarded && FILE_READERS.contains(&name))
        })
}

/// Whether an allow rule certainly covers a command, in the forms Claude Code
/// documents: `Bash` or `Bash(*)`, an exact command, or a command prefix followed
/// by `:*` or ` *` and matched at a word boundary. Any other wildcard never allows.
fn allows(rule: &str, segment: &str) -> bool {
    if rule == "Bash" {
        return true;
    }
    let Some(spec) = rule
        .strip_prefix("Bash(")
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return false;
    };
    if spec == "*" {
        return true;
    }
    let (prefix, wildcard) = match spec.strip_suffix(":*").or_else(|| spec.strip_suffix(" *")) {
        Some(prefix) => (prefix, true),
        None => (spec, false),
    };
    if prefix.is_empty() || prefix.contains('*') || prefix != prefix.trim() {
        return false;
    }
    segment == prefix
        || wildcard
            && segment
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with(' '))
}

/// What the command hook does with a command it can route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Leave the command for Claude Code to judge as written.
    Leave,
    /// Rewrite it without a permission decision.
    Rewrite,
    /// Rewrite it and allow the rewrite, as the rules allow the original.
    Approve,
}

/// The permission mode a hook input reports: `None` when it reports none, and an
/// empty mode, which no rule below knows, for a value that is not a string.
pub fn mode(input: &Value) -> Option<&str> {
    match input.get("permission_mode") {
        None | Some(Value::Null) => None,
        Some(mode) => Some(mode.as_str().unwrap_or("")),
    }
}

/// The hook's answer in a permission mode. A rule that might deny or ask leaves
/// the command in every mode. An allowed command is approved where Claude Code
/// would run it without asking, except in `auto`, where Claude Code drops broad
/// allow rules and lets its classifier review the call instead. A command no rule
/// allows is rewritten only in `bypassPermissions` and `auto`, where Claude Code
/// would not ask about the rewrite.
pub fn answer(mode: &str, verdict: Verdict) -> Answer {
    match (verdict, mode) {
        (Verdict::Deny | Verdict::Ask, _) => Answer::Leave,
        (Verdict::Allow, "default" | "acceptEdits" | "dontAsk" | "bypassPermissions") => {
            Answer::Approve
        }
        (Verdict::Allow | Verdict::Unknown, "auto") | (Verdict::Unknown, "bypassPermissions") => {
            Answer::Rewrite
        }
        _ => Answer::Leave,
    }
}

/// The answer for a hook call whose input reports `mode`.
pub fn decide(root: &Path, mode: &str, segments: &[&str]) -> Answer {
    // Deny and ask leave the command in every mode, and an unknown mode leaves
    // whatever the verdict, so only a known mode reads the settings.
    if !matches!(
        mode,
        "default" | "acceptEdits" | "dontAsk" | "bypassPermissions" | "auto"
    ) {
        return Answer::Leave;
    }
    load(&Sources::for_hook(root)).answer(mode, segments)
}

impl Rules {
    /// The hook's answer for a command line it can route, split into its
    /// top-level commands, in a session that reports `mode`.
    pub fn answer(&self, mode: &str, segments: &[&str]) -> Answer {
        answer(mode, self.verdict(segments))
    }

    /// The permission mode sessions start in unless told otherwise.
    pub fn default_mode(&self) -> &str {
        self.default_mode
            .as_ref()
            .map_or("default", |(_, mode)| mode.as_str())
    }

    /// Settings files that exist but cannot be used, each of which keeps the
    /// hook from rewriting any command.
    pub fn broken(&self) -> &[Broken] {
        &self.broken
    }

    /// Allow rules that let the agent run any command as `justless run -- COMMAND`
    /// without asking, with the file each one is in.
    pub fn broad_justless_rules(&self) -> Vec<(&Path, &str)> {
        let probes = [
            "justless run -- sh -c anything",
            "justless run --hook --session 0123456789abcdef01234567 -- sh -c anything",
        ];
        self.found
            .iter()
            .filter(|(_, rule)| {
                rule.trim_start().starts_with("Bash(justless")
                    && probes.iter().any(|probe| loosely(rule, probe))
            })
            .map(|(file, rule)| (file.as_path(), rule.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules(allow: &[&str], ask: &[&str], deny: &[&str]) -> Rules {
        let own = |list: &[&str]| list.iter().map(|rule| (*rule).to_owned()).collect();
        Rules {
            allow: own(allow),
            ask: own(ask),
            deny: own(deny),
            ..Rules::default()
        }
    }

    #[test]
    fn globs_match_any_text_for_a_star() {
        assert!(glob("git * main", "git push origin main"));
        assert!(!glob("git * main", "git log"));
        assert!(glob("*", ""));
        assert!(glob("a*b*c", "aXbYc"));
        assert!(!glob("a*b*c", "aXbY"));
        assert!(glob("é*", "éè"));
        assert!(!glob("ls", "lsof"));
    }

    #[test]
    fn allow_rules_count_only_in_their_documented_narrow_forms() {
        let cases = [
            ("Bash", "cargo test", true),
            ("Bash(*)", "cargo test", true),
            ("Bash(cargo test)", "cargo test", true),
            ("Bash(cargo test)", "cargo test --lib", false),
            ("Bash(cargo test)", "cargo test 2>&1", false),
            ("Bash(cargo test:*)", "cargo test", true),
            ("Bash(cargo test:*)", "cargo test --lib", true),
            ("Bash(cargo test:*)", "cargo testing", false),
            ("Bash(cargo test *)", "cargo test --lib", true),
            ("Bash(cargo test *)", "cargo test", true),
            ("Bash(cargo test*)", "cargo test", false),
            ("Bash(cargo * --lib)", "cargo test --lib", false),
            ("Bash(* --lib)", "cargo test --lib", false),
            ("Bash(cargo test :*)", "cargo test --lib", false),
            ("Bash(:*)", "cargo test", false),
            ("bash(cargo test)", "cargo test", false),
            (" Bash(cargo test)", "cargo test", false),
            ("Read(cargo test)", "cargo test", false),
            ("*", "cargo test", false),
        ];
        for (rule, segment, expected) in cases {
            assert_eq!(allows(rule, segment), expected, "{rule} {segment}");
        }
    }

    #[test]
    fn ask_and_deny_rules_match_loosely_through_wrappers_and_assignments() {
        let deny = |rule: &str, segment: &str| {
            rules(&["Bash"], &[], &[rule]).verdict(&[segment]) == Verdict::Deny
        };
        assert!(deny("Bash(git push:*)", "git push origin main"));
        assert!(deny("Bash(git push *)", "git push"));
        assert!(deny("Bash(rm:*)", "rmdir x"));
        assert!(deny("Bash(cargo test:*)", "RUST_BACKTRACE=1 cargo test"));
        assert!(deny("Bash(cargo test:*)", "timeout 30 cargo test"));
        assert!(deny("Bash(cargo test:*)", "timeout -s KILL 30 cargo test"));
        assert!(deny("Bash(cargo test:*)", "nice -n 5 cargo test"));
        assert!(deny(
            "Bash(cargo test:*)",
            "/usr/bin/env FOO=1 cargo test 2>&1"
        ));
        assert!(deny("Bash(cargo test --lib)", "cargo  test   '--lib'"));
        assert!(deny("Bash(* --lib)", "cargo test --lib"));
        assert!(deny("Bash", "cargo test"));
        assert!(deny("Bash(*)", "cargo test"));
        assert!(deny("*", "cargo test"));
        assert!(deny("B*", "cargo test"));
        assert!(deny(" Bash(cargo test) ", "cargo test"));
        assert!(!deny("Bash(git push:*)", "git status"));
        assert!(!deny("Read(./.env)", "cargo test"));
        assert!(!deny("Bash(npm test)", "npm test --watch"));
    }

    #[test]
    fn a_verdict_needs_every_command_allowed_and_any_one_denied() {
        let set = rules(
            &["Bash(git status:*)", "Bash(cargo test *)"],
            &["Bash(git log:*)"],
            &["Bash(git push:*)"],
        );
        assert_eq!(set.verdict(&["git status"]), Verdict::Allow);
        assert_eq!(
            set.verdict(&["git status", "cargo test --lib 2>&1"]),
            Verdict::Allow
        );
        assert_eq!(set.verdict(&["git status", "echo done"]), Verdict::Unknown);
        assert_eq!(set.verdict(&["git status", "git log -1"]), Verdict::Ask);
        assert_eq!(set.verdict(&["git log -1", "git push"]), Verdict::Deny);
        assert_eq!(set.verdict(&["git diff"]), Verdict::Unknown);
        assert_eq!(set.verdict(&[]), Verdict::Unknown);
        // Even a blanket rule does not approve what Claude Code may still ask about.
        let blanket = rules(&["Bash"], &[], &[]);
        for segment in [
            "NO_COLOR=1 cargo test",
            "timeout 30 cargo test",
            "cd web",
            "find . -name x",
            "rg $HOME",
            "cargo test | tail",
        ] {
            assert_eq!(blanket.verdict(&[segment]), Verdict::Unknown, "{segment}");
        }
        assert_eq!(blanket.verdict(&["cargo test"]), Verdict::Allow);
        let broken = Rules {
            broken: vec![Broken {
                path: PathBuf::from("settings.json"),
                problem: "not a JSON object".into(),
            }],
            ..rules(&["Bash"], &[], &[])
        };
        assert_eq!(broken.verdict(&["cargo test"]), Verdict::Deny);
    }

    #[test]
    fn file_rules_leave_file_commands_and_approve_only_others() {
        // Claude Code checks the paths file commands name against Read and Edit
        // rules, which do not see them inside `justless run -- …`, so a line that
        // runs such a command takes the verdict of the file rule, through
        // assignments and wrappers and in any command of the list.
        for rule in ["Read(./.env)", "Edit", "R*(./secrets/**)", " Write(x) "] {
            let denied = rules(&["Bash"], &[], &[rule]);
            let asked = rules(&["Bash"], &[rule], &[]);
            for segments in [
                &["cat .env"][..],
                &["rg key"],
                &["'git' show HEAD:.env"],
                &["/bin/ls -l"],
                &["LC_ALL=C git diff .env"],
                &["timeout 5 rg key .env"],
                &["cargo test", "rg key .env"],
            ] {
                assert_eq!(
                    denied.verdict(segments),
                    Verdict::Deny,
                    "{rule} {segments:?}"
                );
                assert_eq!(asked.verdict(segments), Verdict::Ask, "{rule} {segments:?}");
            }
            assert_eq!(denied.verdict(&["cargo test"]), Verdict::Allow, "{rule}");
            assert_eq!(asked.verdict(&["cargo test"]), Verdict::Allow, "{rule}");
        }
        assert_eq!(
            rules(&["Bash"], &[], &["WebFetch"]).verdict(&["cat .env"]),
            Verdict::Allow
        );
        let blocked = Rules {
            blocks_paths: true,
            ..rules(&["Bash"], &[], &[])
        };
        assert_eq!(blocked.verdict(&["rg key /etc"]), Verdict::Unknown);
        assert_eq!(blocked.verdict(&["cargo test"]), Verdict::Allow);
    }

    #[test]
    fn modes_decide_what_each_verdict_does() {
        use Answer::{Approve, Leave, Rewrite};
        let modes = [
            "default",
            "acceptEdits",
            "dontAsk",
            "bypassPermissions",
            "auto",
            "plan",
            "",
        ];
        let table = [
            (Verdict::Deny, [Leave; 7]),
            (Verdict::Ask, [Leave; 7]),
            (
                Verdict::Allow,
                [Approve, Approve, Approve, Approve, Rewrite, Leave, Leave],
            ),
            (
                Verdict::Unknown,
                [Leave, Leave, Leave, Rewrite, Rewrite, Leave, Leave],
            ),
        ];
        for (verdict, answers) in table {
            for (mode, expected) in modes.iter().zip(answers) {
                assert_eq!(answer(mode, verdict), expected, "{verdict:?} {mode}");
            }
        }
        assert_eq!(mode(&json!({})), None);
        assert_eq!(mode(&json!({"permission_mode":null})), None);
        assert_eq!(mode(&json!({"permission_mode":"auto"})), Some("auto"));
        assert_eq!(mode(&json!({"permission_mode":1})), Some(""));
    }

    fn sources(dir: &Path) -> Sources {
        Sources {
            managed: dir.join("managed"),
            user: Some(dir.join("user/settings.json")),
            project: dir.join("project"),
            repository: None,
            nearby: vec![dir.join("root")],
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn settings_files_merge_and_unreadable_ones_might_deny() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path();
        let sources = sources(dir);
        // No files at all: nothing allows, nothing denies.
        assert_eq!(load(&sources).verdict(&["cargo test"]), Verdict::Unknown);
        write(
            &dir.join("user/settings.json"),
            r#"{"permissions":{"allow":["Bash(cargo test:*)", 3]}}"#,
        );
        write(
            &dir.join("project/.claude/settings.local.json"),
            r#"{"permissions":{"allow":["Bash(git status:*)"]}}"#,
        );
        let loaded = load(&sources);
        assert_eq!(
            loaded.verdict(&["cargo test", "git status"]),
            Verdict::Allow
        );
        // A nearby directory adds deny and ask rules, never allow rules.
        write(
            &dir.join("root/.claude/settings.local.json"),
            r#"{"permissions":{"allow":["Bash(npm test)"],"ask":["Bash(git status --short)"]}}"#,
        );
        let loaded = load(&sources);
        assert_eq!(loaded.verdict(&["npm test"]), Verdict::Unknown);
        assert_eq!(loaded.verdict(&["git status --short"]), Verdict::Ask);
        // Managed drop-ins merge in order, without hidden or other files.
        write(
            &dir.join("managed/managed-settings.d/10-a.json"),
            r#"{"permissions":{"deny":["Bash(cargo test --release)"]}}"#,
        );
        write(&dir.join("managed/managed-settings.d/.hidden.json"), "{");
        write(&dir.join("managed/managed-settings.d/notes.txt"), "{");
        let loaded = load(&sources);
        assert_eq!(loaded.verdict(&["cargo test --release"]), Verdict::Deny);
        assert_eq!(loaded.verdict(&["cargo test"]), Verdict::Allow);
        // Managed-only rules drop every other allow rule.
        write(
            &dir.join("managed/managed-settings.json"),
            r#"{"allowManagedPermissionRulesOnly":true,"permissions":{"allow":["Bash(git status)"]}}"#,
        );
        let loaded = load(&sources);
        assert_eq!(loaded.verdict(&["cargo test"]), Verdict::Unknown);
        assert_eq!(loaded.verdict(&["git status"]), Verdict::Allow);
        // Any file that exists but cannot be used might deny.
        for (file, text) in [
            ("project/.claude/settings.json", "{"),
            ("project/.claude/settings.json", ""),
            ("project/.claude/settings.json", "[]"),
            ("project/.claude/settings.json", r#"{"permissions":[]}"#),
            (
                "project/.claude/settings.json",
                r#"{"permissions":{"deny":"Bash"}}"#,
            ),
            (
                "project/.claude/settings.json",
                r#"{"permissions":{"ask":[1]}}"#,
            ),
            ("root/.claude/settings.json", "{"),
            ("managed/managed-settings.d/20-b.json", "{"),
        ] {
            let path = dir.join(file);
            write(&path, text);
            let loaded = load(&sources);
            assert_eq!(
                loaded.verdict(&["git status"]),
                Verdict::Deny,
                "{file}: {text}"
            );
            // Each one is named, with its problem, for `doctor`.
            assert!(
                matches!(loaded.broken(), [broken] if broken.path == path && !broken.problem.is_empty()),
                "{file}: {text}: {:?}",
                loaded.broken()
            );
            fs::remove_file(&path).unwrap();
        }
        write(&dir.join("project/.claude/settings.json"), "{");
        assert_eq!(
            load(&sources).broken()[0].problem,
            "not valid JSON: EOF while parsing an object at line 1 column 1"
        );
        fs::remove_file(dir.join("project/.claude/settings.json")).unwrap();
        assert_eq!(load(&sources).verdict(&["git status"]), Verdict::Allow);
        // Path limits in any file keep file commands from being approved.
        write(
            &dir.join("root/.claude/settings.json"),
            r#"{"permissions":{"blockReadsOutsideWorkingDirectories":true}}"#,
        );
        assert_eq!(load(&sources).verdict(&["git status"]), Verdict::Unknown);
        fs::remove_file(dir.join("root/.claude/settings.json")).unwrap();
        let unlocated = Sources {
            user: None,
            ..sources
        };
        assert_eq!(load(&unlocated).verdict(&["git status"]), Verdict::Deny);
    }

    #[test]
    fn the_repository_local_file_and_the_default_mode_follow_claude_code() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path();
        let mut sources = sources(dir);
        let repository = dir.join("repo/.claude/settings.local.json");
        write(
            &repository,
            r#"{"permissions":{"allow":["Bash(cargo test:*)"],"defaultMode":"acceptEdits"}}"#,
        );
        // Where Claude Code may keep the file elsewhere, only its ask and deny
        // rules count, and it sets no mode.
        sources.repository = Some((dir.join("repo"), false));
        let loaded = load(&sources);
        assert_eq!(loaded.verdict(&["cargo test"]), Verdict::Unknown);
        assert_eq!(loaded.default_mode(), "default");
        sources.repository = Some((dir.join("repo"), true));
        let loaded = load(&sources);
        assert_eq!(loaded.verdict(&["cargo test"]), Verdict::Allow);
        assert_eq!(loaded.default_mode(), "acceptEdits");
        // The highest file that sets a mode wins, and only user and managed
        // settings set `auto` or `bypassPermissions`.
        write(
            &dir.join("user/settings.json"),
            r#"{"permissions":{"defaultMode":"auto"}}"#,
        );
        write(
            &dir.join("project/.claude/settings.local.json"),
            r#"{"permissions":{"defaultMode":"bypassPermissions"}}"#,
        );
        assert_eq!(load(&sources).default_mode(), "acceptEdits");
        fs::remove_file(&repository).unwrap();
        assert_eq!(load(&sources).default_mode(), "auto");
        write(
            &dir.join("managed/managed-settings.json"),
            r#"{"permissions":{"defaultMode":"dontAsk"}}"#,
        );
        assert_eq!(load(&sources).default_mode(), "dontAsk");
    }

    #[test]
    fn only_broad_justless_allow_rules_are_reported() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path();
        write(
            &dir.join("project/.claude/settings.local.json"),
            r#"{"permissions":{"allow":["Bash(justless run:*)","Bash(justless query:*)","Bash(justless run --hook -- cargo test)","Bash(cargo test:*)"]}}"#,
        );
        write(
            &dir.join("user/settings.json"),
            r#"{"permissions":{"allow":["Bash(justless *)","Bash(justless run --hook *)"]}}"#,
        );
        let loaded = load(&sources(dir));
        let found = loaded.broad_justless_rules();
        let rules: Vec<&str> = found.iter().map(|(_, rule)| *rule).collect();
        assert_eq!(
            rules,
            [
                "Bash(justless *)",
                "Bash(justless run --hook *)",
                "Bash(justless run:*)"
            ]
        );
        assert!(found[0].0.ends_with("user/settings.json"));
        assert!(found[2].0.ends_with("settings.local.json"));
    }
}
