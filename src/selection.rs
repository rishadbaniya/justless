//! Planning for `justless test --affected`: from the current Git changes to the
//! tests each runner should run, why each was chosen, and when the plan falls
//! back to a wider scope because the index cannot vouch for a subset.
//!
//! The plan is deterministic and runs nothing. `testrun` executes it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::context::{Change, changes, relationship};
use crate::impact::{Dependent, Walk, walk};
use crate::index::{Definition, Fingerprint, Index};
use crate::map::{is_test_file, language, name};
use crate::runners::{Cargo, Kind, Package, Runner, Target, enabled_features, under};
use crate::workspace::{Location, Workspace};

/// Hops followed from each changed symbol. Deep enough to be complete in
/// practice; a walk that stops at a cap falls back instead of truncating.
const DEPTH: usize = 64;
/// A subset at or above this share of the indexed tests runs the full suite.
const TOO_BROAD_PERCENT: usize = 60;
/// Selected names past which name filters collapse to whole targets.
const MAX_NAMES: usize = 300;
/// Argument bytes past which name filters collapse to whole targets.
const MAX_ARGV_BYTES: usize = 32 * 1024;
/// Mention lines read per name before the search counts as capped.
const MAX_MENTIONS: usize = 4000;
/// Links shown in one test's evidence chain.
const CHAIN: usize = 12;

/// Trait methods called implicitly or everywhere. A change to one is traced
/// through its type, since its own name says little about who calls it.
const UBIQUITOUS: &[&str] = &[
    "new",
    "fmt",
    "from",
    "into",
    "default",
    "clone",
    "eq",
    "ne",
    "cmp",
    "partial_cmp",
    "hash",
    "drop",
    "deref",
    "deref_mut",
    "next",
    "as_ref",
    "borrow",
    "try_from",
    "from_str",
    "to_string",
    "add",
    "sub",
    "mul",
    "div",
    "neg",
    "index",
    "serialize",
    "deserialize",
    "__init__",
    "__str__",
    "__repr__",
    "__eq__",
    "toString",
    "constructor",
];

/// File names too common to stand for one file when mentioned alone.
const GENERIC_NAMES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "pyproject.toml",
    "__init__.py",
    "conftest.py",
    "index.ts",
    "index.js",
    "index.tsx",
    "README.md",
    "settings.py",
    "config.json",
    "config.yaml",
    "config.toml",
];

const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "uv.lock",
    "poetry.lock",
    "Pipfile.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "go.sum",
];

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TestSettings {
    /// Gitignore-syntax globs whose changes run every runner's full suite.
    pub full_suite_on: Vec<String>,
    /// Gitignore-syntax globs for changed files with no test impact.
    pub ignore: Vec<String>,
}

#[derive(Clone, Serialize)]
pub(crate) struct Reason {
    pub code: &'static str,
    pub message: String,
}

#[derive(Clone, Serialize)]
pub(crate) struct Note {
    pub subject: String,
    pub detail: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Mode {
    /// Tests reachable from the changes, labeled as a subset.
    Subset,
    /// A subset kept although a fallback reason applied (`--no-fallback`).
    LowConfidence,
    /// The runner's full suite.
    Full,
    /// Changed code compiles; no test reaches it.
    BuildOnly,
    /// No change reaches this runner.
    Nothing,
    /// The command already selects its tests and runs unchanged.
    Command,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StepKind {
    /// Whole packages.
    Package,
    /// Named tests in chosen targets.
    Filtered,
    /// Whole targets that cannot be filtered by name.
    Unfiltered,
    /// Documentation examples of reached items.
    Doctests,
    /// Compile only.
    Build,
    /// The base command unchanged.
    Full,
}

#[derive(Serialize)]
pub(crate) struct Step {
    pub kind: StepKind,
    pub argv: Vec<String>,
    /// Selected test names per target key, checked against what ran.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub names: BTreeMap<String, Vec<String>>,
    /// How to run each target key in full when its names did not all run.
    #[serde(skip)]
    pub widen: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Serialize)]
pub(crate) struct Selected {
    pub id: String,
    pub target: String,
    pub confidence: &'static str,
    pub why: String,
}

#[derive(Serialize)]
pub(crate) struct RunnerPlan {
    pub runner: Kind,
    pub root: String,
    /// Workspace-relative directory the steps run in.
    pub cwd: String,
    pub base: Vec<String>,
    pub mode: Mode,
    pub reasons: Vec<Reason>,
    pub unit: &'static str,
    pub indexed: usize,
    pub selected: usize,
    /// Test targets with a selected test, and test targets in scope (Cargo).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub targets: Option<(usize, usize)>,
    pub steps: Vec<Step>,
    pub tests: Vec<Selected>,
    pub skipped: Vec<Note>,
}

#[derive(Serialize)]
pub(crate) struct Plan {
    pub version: u32,
    /// `HEAD 3f2c1ab`, or why there is no base.
    pub base: String,
    pub changes: Vec<Note>,
    pub seeds: Vec<Note>,
    pub ignored: Vec<Note>,
    pub runners: Vec<RunnerPlan>,
}

impl Plan {
    pub fn changed_files(&self) -> usize {
        self.changes.len()
    }
}

/// How one starting symbol reaches tests.
struct Hit {
    hops: usize,
    possible: bool,
    why: String,
}

struct Seed {
    /// The changed or otherwise affected definition.
    id: usize,
    /// Where the change is, for evidence.
    origin: String,
    /// Test definitions reached.
    tests: BTreeMap<usize, Hit>,
    /// Test-convention files reached at file scope, such as by an import.
    files: BTreeMap<String, Hit>,
    /// Non-test definitions reached, with their distance.
    dependents: BTreeMap<usize, usize>,
    /// Whether the walk stopped at a cap.
    capped: Option<String>,
    /// The definition did not exist at the base revision.
    new: bool,
    /// A new definition whose name appears nowhere outside itself.
    unused: bool,
    /// Whether the seed itself is a code change the per-symbol rules apply to.
    rules: bool,
}

/// Text of indexed files, read once.
struct Texts<'a> {
    workspace: &'a Workspace,
    cache: BTreeMap<String, Option<String>>,
}

impl<'a> Texts<'a> {
    fn new(workspace: &'a Workspace) -> Self {
        Self {
            workspace,
            cache: BTreeMap::new(),
        }
    }

    fn get(&mut self, file: &str) -> Option<&str> {
        if !self.cache.contains_key(file) {
            let text = self
                .workspace
                .source_path(file)
                .ok()
                .and_then(|path| fs::read_to_string(path).ok());
            self.cache.insert(file.to_owned(), text);
        }
        self.cache[file].as_deref()
    }
}

fn identifier(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn has_word(line: &str, word: &str) -> bool {
    line.match_indices(word).any(|(at, _)| {
        !line[..at].chars().next_back().is_some_and(identifier)
            && !line[at + word.len()..]
                .chars()
                .next()
                .is_some_and(identifier)
    })
}

/// Lines of indexed files that contain `needle`, as a whole identifier when
/// `word`. Returns `None` past the cap.
fn find(
    index: &Index,
    texts: &mut Texts,
    needle: &str,
    word: bool,
) -> Option<Vec<(String, usize)>> {
    let fingerprint = Fingerprint::new(needle);
    let mut found = Vec::new();
    let files: Vec<String> = index
        .fingerprints
        .iter()
        .filter(|(_, candidate)| candidate.includes(fingerprint))
        .map(|(file, _)| file.clone())
        .collect();
    for file in files {
        let Some(text) = texts.get(&file) else {
            continue;
        };
        if !text.contains(needle) {
            continue;
        }
        for (offset, line) in text.lines().enumerate() {
            if if word {
                has_word(line, needle)
            } else {
                line.contains(needle)
            } {
                found.push((file.clone(), offset + 1));
                if found.len() > MAX_MENTIONS {
                    return None;
                }
            }
        }
    }
    Some(found)
}

/// The innermost definition at a line, preferring anything over a file or module node.
fn enclosing(index: &Index, file: &str, line: usize) -> Option<usize> {
    let ids = index.at(&Location {
        file: file.to_owned(),
        start: line,
        end: line,
    });
    ids.iter()
        .copied()
        .find(|&id| !matches!(index.definitions[id].kind.as_str(), "file" | "module"))
        .or_else(|| ids.first().copied())
}

/// The enclosing test definition at a line, if any.
fn enclosing_test(index: &Index, file: &str, line: usize) -> Option<usize> {
    index
        .at(&Location {
            file: file.to_owned(),
            start: line,
            end: line,
        })
        .into_iter()
        .find(|&id| index.definitions[id].test)
}

/// A language, with JavaScript and its typed dialects as one.
fn language_family(file: &str) -> Option<&'static str> {
    match language(file)? {
        "TypeScript" | "TSX" => Some("JavaScript"),
        other => Some(other),
    }
}

fn short(definition: &Definition) -> &str {
    let name = name(definition);
    name.rsplit("::")
        .next()
        .unwrap_or(name)
        .rsplit('.')
        .next()
        .unwrap_or(name)
}

fn git_text(workspace: &Workspace, args: &[&str]) -> Option<String> {
    let output = workspace.git(args).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Definition names and kinds in a file's text, by the same parser `read` uses.
fn symbols(file: &str, text: String) -> Option<BTreeSet<(String, String)>> {
    let mut snapshot = crate::source::Snapshot::from_text(file, text);
    snapshot.index(file).ok()?;
    Some(
        snapshot
            .symbols()
            .into_iter()
            .filter(|(_, kind, _, _)| {
                !matches!(kind.as_str(), "import" | "file" | "module" | "section")
            })
            .map(|(name, kind, _, _)| {
                let last = name.rsplit("::").next().unwrap_or(&name).to_owned();
                (last.rsplit('.').next().unwrap_or(&last).to_owned(), kind)
            })
            .collect(),
    )
}

/// Words on changed lines that could name something, for changes outside any definition.
fn tokens(lines: &[&str]) -> BTreeSet<String> {
    const SKIP: &[&str] = &[
        "use", "mod", "pub", "crate", "self", "super", "as", "impl", "fn", "let", "mut", "const",
        "static", "struct", "enum", "trait", "type", "where", "for", "in", "if", "else", "match",
        "return", "true", "false", "None", "Some", "Ok", "Err", "import", "from", "def", "class",
        "export", "default", "require", "package", "func", "var", "std", "core", "alloc", "async",
        "await", "and", "not", "the", "Self", "extern",
    ];
    let mut found = BTreeSet::new();
    for line in lines {
        let line = line.trim();
        if line.starts_with("//")
            || line.starts_with("/*")
            || line.starts_with('*')
            || line.starts_with("#[")
            || line.starts_with("#![")
            || (line.starts_with('#') && !line.starts_with("#include"))
            || line.starts_with("--")
        {
            continue;
        }
        for word in line.split(|c: char| !identifier(c)) {
            if word.len() >= 3
                && !word.starts_with(|c: char| c.is_ascii_digit())
                && !SKIP.contains(&word)
            {
                found.insert(word.to_owned());
            }
        }
    }
    found
}

/// Files that change no test's behavior unless something mentions them.
fn inert(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    let upper = name.to_ascii_uppercase();
    let documentation = [".md", ".rst", ".txt", ".adoc"]
        .iter()
        .any(|extension| name.ends_with(extension))
        && (!file.contains('/') || file.starts_with("docs/") || file.contains("/docs/"));
    documentation
        || [
            "README",
            "CHANGELOG",
            "LICENSE",
            "CONTRIBUTING",
            "AUTHORS",
            "NOTICE",
        ]
        .iter()
        .any(|prefix| upper.starts_with(prefix))
        || matches!(
            name,
            ".gitignore"
                | ".editorconfig"
                | ".justlessignore"
                | ".justless.toml"
                | ".gitattributes"
                | ".mailmap"
                | "AGENTS.md"
                | "CLAUDE.md"
                | "GEMINI.md"
        )
        || file.starts_with(".github/")
        || file.starts_with(".justless/")
}

/// Which runners a dependency or configuration file belongs to.
fn concerns(kind: Kind, name: &str, file: &str) -> bool {
    let javascript = matches!(kind, Kind::Jest | Kind::Vitest);
    match name {
        "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml" => kind == Kind::Cargo,
        "uv.lock" | "poetry.lock" | "Pipfile.lock" | "pyproject.toml" | "setup.py"
        | "setup.cfg" | "pytest.ini" | "tox.ini" | "conftest.py" => kind == Kind::Pytest,
        "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" | "bun.lock" | "bun.lockb"
        | "package.json" | ".swcrc" => javascript,
        "go.sum" | "go.mod" | "go.work" => kind == Kind::Go,
        _ if name.starts_with("requirements") && name.ends_with(".txt") => kind == Kind::Pytest,
        _ if file.starts_with(".cargo/config") || file.contains("/.cargo/config") => {
            kind == Kind::Cargo
        }
        _ if (name.starts_with("tsconfig") && name.ends_with(".json"))
            || name.starts_with("babel.config.")
            || name.starts_with(".babelrc")
            || name.starts_with("jest.config.")
            || name.starts_with("vitest.config.")
            || name.starts_with("vite.config.") =>
        {
            javascript
        }
        _ => true,
    }
}

/// How a changed file can change what every test sees, before any symbol is looked at.
fn file_trigger(workspace: &Workspace, runner: &Runner, file: &str) -> Option<Reason> {
    let name = file.rsplit('/').next().unwrap_or(file);
    let directory = file.rsplit_once('/').map_or("", |(directory, _)| directory);
    if !concerns(runner.kind, name, file) {
        return None;
    }
    if LOCKFILES.contains(&name) || (name.starts_with("requirements") && name.ends_with(".txt")) {
        return Some(Reason {
            code: "lockfile",
            message: format!("{file} changed: dependency versions can affect every test"),
        });
    }
    let at_root = directory == runner.root;
    let workspace_config = match name {
        "rust-toolchain" | "rust-toolchain.toml" => true,
        "pyproject.toml" | "setup.py" | "setup.cfg" | ".swcrc" | "go.mod" | "go.work"
        | "package.json" => at_root,
        _ => {
            file.starts_with(".cargo/config")
                || file.contains("/.cargo/config")
                || (name.starts_with("tsconfig") && name.ends_with(".json"))
                || name.starts_with("babel.config.")
                || name.starts_with(".babelrc")
        }
    };
    if workspace_config {
        return Some(Reason {
            code: "workspace-config",
            message: format!("{file} changed: build configuration can affect every test"),
        });
    }
    let root_conftest = name == "conftest.py" && at_root;
    let test_config = matches!(name, "pytest.ini" | "tox.ini")
        || root_conftest
        || name.starts_with("jest.config.")
        || name.starts_with("vitest.config.")
        || name.starts_with("vite.config.")
        || name == ".env"
        || name.starts_with(".env.");
    if test_config {
        return Some(Reason {
            code: "test-config",
            message: format!("{file} changed: runner configuration"),
        });
    }
    if name == "Cargo.toml" && runner.kind == Kind::Cargo {
        let root_manifest = runner
            .cargo
            .as_ref()
            .is_some_and(|cargo| cargo.root_manifest == file);
        let text = fs::read_to_string(workspace.root.join(file)).unwrap_or_default();
        let single = runner
            .cargo
            .as_ref()
            .is_some_and(|cargo| cargo.packages.len() == 1);
        if root_manifest && (text.contains("[workspace") || text.contains("[profile") || single) {
            return Some(Reason {
                code: "workspace-config",
                message: format!("{file} changed: build configuration can affect every test"),
            });
        }
    }
    None
}

/// The selection pipeline's shared, runner-independent facts.
struct Analysis {
    seeds: Vec<Seed>,
    /// Changed files and how they changed.
    files: Vec<(String, String)>,
    /// Plan notes for the seeds.
    seed_notes: Vec<Note>,
    /// Changed Rust files whose target must compile even with no symbol seed.
    compile_files: BTreeSet<String>,
    /// Files whose changes could not be pinned to any test or symbol.
    unlinked: BTreeMap<String, Reason>,
    /// Files with no test impact, and why.
    ignored: Vec<Note>,
    /// Tests or test files selected directly by a mention, by runner-independent key.
    mentioned_tests: BTreeMap<usize, Hit>,
    mentioned_files: BTreeMap<String, Hit>,
    /// Changed ranges that overlap a `macro_rules!` item, as (file, line, macro).
    macros: Vec<(String, usize, String)>,
    /// Files deleted outright.
    deleted: BTreeSet<String>,
}

impl Analysis {
    /// Adds a starting definition once. Returns whether it was new.
    fn add_seed(&mut self, id: usize, origin: String) -> bool {
        if self.seeds.iter().any(|seed| seed.id == id) {
            return false;
        }
        self.seeds.push(Seed {
            id,
            origin,
            tests: BTreeMap::new(),
            files: BTreeMap::new(),
            dependents: BTreeMap::new(),
            capped: None,
            new: false,
            unused: false,
            rules: true,
        });
        true
    }
}

/// Builds the plan for every runner from the current Git changes.
pub(crate) fn plan(
    workspace: &Workspace,
    index: &Index,
    runners: Vec<Runner>,
    settings: &TestSettings,
    no_fallback: bool,
) -> Result<Plan> {
    let inside = git_text(workspace, &["rev-parse", "--is-inside-work-tree"]);
    if inside.as_deref() != Some("true") {
        let runners = runners
            .into_iter()
            .map(|runner| {
                full_plan(
                    &runner,
                    vec![Reason {
                        code: "no-git",
                        message: "not a Git checkout".into(),
                    }],
                )
            })
            .collect();
        return Ok(Plan {
            version: 1,
            base: "no Git checkout".into(),
            changes: Vec::new(),
            seeds: Vec::new(),
            ignored: Vec::new(),
            runners,
        });
    }
    let base = git_text(workspace, &["rev-parse", "--short", "HEAD"])
        .map_or_else(|| "no commits yet".to_owned(), |rev| format!("HEAD {rev}"));
    let changes = changes(workspace, index, false)?;
    let analysis = analyze(workspace, index, &changes, settings)?;
    let changes_notes: Vec<Note> = analysis
        .files
        .iter()
        .map(|(file, status)| Note {
            subject: file.clone(),
            detail: status.clone(),
        })
        .collect();
    let globs = |patterns: &[String]| -> Option<ignore::gitignore::Gitignore> {
        if patterns.is_empty() {
            return None;
        }
        let mut builder = ignore::gitignore::GitignoreBuilder::new(&workspace.root);
        for pattern in patterns {
            let _ = builder.add_line(None, pattern);
        }
        builder.build().ok()
    };
    let full_on = globs(&settings.full_suite_on);
    let mut plans = Vec::new();
    for runner in &runners {
        let mut runner_plan = plan_runner(workspace, index, runner, &analysis, no_fallback);
        if let Some(globs) = &full_on {
            for (file, _) in &analysis.files {
                if runner_plan.mode != Mode::Command
                    && runner_plan.mode != Mode::Full
                    && globs.matched_path_or_any_parents(file, false).is_ignore()
                {
                    let reason = Reason {
                        code: "configured",
                        message: format!("{file} matches test.full_suite_on"),
                    };
                    runner_plan = escalate(runner, runner_plan, vec![reason], no_fallback);
                }
            }
        }
        plans.push(runner_plan);
    }
    Ok(Plan {
        version: 1,
        base,
        changes: changes_notes,
        seeds: analysis.seed_notes,
        ignored: analysis.ignored,
        runners: plans,
    })
}

fn analyze(
    workspace: &Workspace,
    index: &Index,
    changes: &[(String, Change)],
    settings: &TestSettings,
) -> Result<Analysis> {
    let definitions = &index.definitions;
    let mut texts = Texts::new(workspace);
    let (changed_ids, section) = crate::impact::changed_in(workspace, index, changes);
    let mut analysis = Analysis {
        seeds: Vec::new(),
        files: Vec::new(),
        seed_notes: Vec::new(),
        compile_files: BTreeSet::new(),
        unlinked: BTreeMap::new(),
        ignored: Vec::new(),
        mentioned_tests: BTreeMap::new(),
        mentioned_files: BTreeMap::new(),
        macros: Vec::new(),
        deleted: BTreeSet::new(),
    };
    let ignore_globs = if settings.ignore.is_empty() {
        None
    } else {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(&workspace.root);
        for pattern in &settings.ignore {
            let _ = builder.add_line(None, pattern);
        }
        builder.build().ok()
    };
    // Where each seed came from, as `affected --changes` reports it.
    let mut origins = BTreeMap::<usize, String>::new();
    let mut unknown = Vec::<(String, usize, usize)>::new();
    for value in &section.values {
        let file = value["file"].as_str().unwrap_or_default().to_owned();
        if let Some(changed) = value["changed"].as_str()
            && let Some(qualified) = value["qualified"].as_str()
            && let Some(&id) = changed_ids
                .iter()
                .find(|&&id| definitions[id].qualified == qualified)
        {
            let origin = if value["references_deleted"].is_string() {
                format!(
                    "references removed {} at {changed}",
                    value["references_deleted"].as_str().unwrap_or_default()
                )
            } else {
                format!("changed {changed}")
            };
            origins.entry(id).or_insert(origin);
        }
        match value["note"].as_str() {
            Some("no enclosing indexed symbol") => unknown.push((
                file,
                value["start"].as_u64().unwrap_or(0) as usize,
                value["end"].as_u64().unwrap_or(0) as usize,
            )),
            Some("impact unknown") => {
                let removed = &value["removed"];
                unknown.push((
                    file,
                    removed[0].as_u64().unwrap_or(0) as usize,
                    removed[1].as_u64().unwrap_or(0) as usize,
                ));
            }
            _ => {}
        }
    }
    let add_seed = Analysis::add_seed;
    for &id in &changed_ids {
        let origin = origins
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("changed {}", definitions[id].location));
        let derived = origin.starts_with("references removed");
        if add_seed(&mut analysis, id, origin)
            && derived
            && let Some(seed) = analysis.seeds.last_mut()
        {
            // Code that names a removed item: its tests run, and its target is
            // built, but a name match alone is no reason to fall back.
            seed.rules = false;
        }
    }

    // File-level facts: status, removed definitions, unenclosed changes.
    let mut by_file = BTreeMap::<&str, Vec<&Change>>::new();
    for (file, change) in changes {
        by_file.entry(file.as_str()).or_default().push(change);
    }
    let head_has =
        |file: &str| git_text(workspace, &["cat-file", "-e", &format!("HEAD:./{file}")]).is_some();
    for (file, list) in &by_file {
        let exists = fs::symlink_metadata(workspace.root.join(file)).is_ok();
        let tracked = head_has(file);
        let status = if !exists {
            "deleted"
        } else if !tracked {
            "added"
        } else if list
            .iter()
            .any(|change| matches!(change, Change::Unindexed))
        {
            "modified, not indexed"
        } else {
            "modified"
        };
        analysis.files.push(((*file).to_owned(), status.to_owned()));
        if !exists {
            analysis.deleted.insert((*file).to_owned());
        }
        if language(file).is_none() {
            continue;
        }
        // Definitions present at HEAD and gone now, renamed ones included.
        if tracked {
            let before = git_text_raw(workspace, &["show", &format!("HEAD:./{file}")])
                .and_then(|text| symbols(file, text));
            let now = if exists {
                texts
                    .get(file)
                    .map(str::to_owned)
                    .and_then(|text| symbols(file, text))
            } else {
                Some(BTreeSet::new())
            };
            if let (Some(before), Some(now)) = (before, now) {
                let current: BTreeSet<&str> = now.iter().map(|(name, _)| name.as_str()).collect();
                let removed: BTreeSet<&str> = before
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .filter(|name| !current.contains(name))
                    .collect();
                for removed_name in removed {
                    analysis.seed_notes.push(Note {
                        subject: removed_name.to_owned(),
                        detail: format!("removed from {file}"),
                    });
                    if removed_name.len() < 3 || UBIQUITOUS.contains(&removed_name) {
                        continue;
                    }
                    match find(index, &mut texts, removed_name, true) {
                        Some(lines) => {
                            for (path, line) in lines {
                                if let Some(id) = enclosing(index, &path, line)
                                    && add_seed(
                                        &mut analysis,
                                        id,
                                        format!(
                                            "references removed {removed_name} at {path}:{line}"
                                        ),
                                    )
                                    && let Some(seed) = analysis.seeds.last_mut()
                                {
                                    seed.rules = false;
                                }
                            }
                        }
                        None => {
                            analysis.unlinked.insert(
                                (*file).to_owned(),
                                Reason {
                                    code: "walk-capped",
                                    message: format!(
                                        "{removed_name}, removed from {file}, is named too often to follow"
                                    ),
                                },
                            );
                        }
                    }
                }
            }
        }
        // Changes outside any definition: what the changed lines name.
        let lines: Vec<String> = texts
            .get(file)
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default();
        let mut words = BTreeSet::new();
        for (unknown_file, start, end) in &unknown {
            if unknown_file != file {
                continue;
            }
            analysis.compile_files.insert((*file).to_owned());
            let changed: Vec<&str> = lines
                .iter()
                .skip(start.saturating_sub(1))
                .take(end.saturating_sub(*start) + 1)
                .map(String::as_str)
                .collect();
            words.extend(tokens(&changed));
        }
        for change in list {
            if let Change::Deleted(deletions) = change {
                for deletion in deletions {
                    let removed: Vec<&str> = deletion.lines.iter().map(String::as_str).collect();
                    if deletion.symbols.is_empty() {
                        words.extend(tokens(&removed));
                    }
                }
                analysis.compile_files.insert((*file).to_owned());
            }
        }
        for word in words {
            for (offset, line) in lines.iter().enumerate() {
                if has_word(line, &word)
                    && let Some(id) = enclosing(index, file, offset + 1)
                {
                    add_seed(
                        &mut analysis,
                        id,
                        format!(
                            "uses {word}, which a change outside any definition of {file} names"
                        ),
                    );
                }
            }
        }
        // Rust macros: a change inside `macro_rules!` reaches every expansion.
        if file.ends_with(".rs") {
            let ranges: Vec<(usize, usize)> = list
                .iter()
                .flat_map(|change| match change {
                    Change::Ranges(ranges) => ranges.clone(),
                    Change::Deleted(deletions) => deletions
                        .iter()
                        .map(|deletion| (deletion.after.max(1), deletion.after.max(1)))
                        .collect(),
                    _ => Vec::new(),
                })
                .collect();
            for (macro_name, start, end) in macro_spans(&lines) {
                if ranges.iter().any(|&(a, b)| a <= end && start <= b) {
                    analysis
                        .macros
                        .push(((*file).to_owned(), start, macro_name));
                }
            }
        }
    }
    analysis.files.sort();

    // A trait method changed through an implicit call is traced through its type.
    let mut extra = Vec::new();
    for seed in &analysis.seeds {
        let definition = &definitions[seed.id];
        if definition.kind != "method" || !UBIQUITOUS.contains(&short(definition)) {
            continue;
        }
        let Some((parent, _)) = definition.qualified.rsplit_once("::") else {
            continue;
        };
        if let Some(id) = definitions
            .iter()
            .position(|other| other.qualified == parent && other.kind != "method")
        {
            extra.push((seed.id, id));
        }
    }

    // Path mentions: a changed file named in indexed text selects what names it.
    let changed_files: Vec<String> = analysis.files.iter().map(|(f, _)| f.clone()).collect();
    for file in &changed_files {
        let mut candidates = vec![file.clone()];
        let parts: Vec<&str> = file.split('/').collect();
        for skip in 1..parts.len().saturating_sub(1) {
            candidates.push(parts[skip..].join("/"));
        }
        let base_name = parts.last().copied().unwrap_or_default();
        if base_name.contains('.') && base_name.len() >= 8 && !GENERIC_NAMES.contains(&base_name) {
            candidates.push(base_name.to_owned());
        }
        for depth in 2..parts.len() {
            candidates.push(parts[..depth].join("/"));
        }
        candidates.sort();
        candidates.dedup();
        let mut mentioned = false;
        for candidate in &candidates {
            let Some(lines) = find(index, &mut texts, candidate, false) else {
                // Named too often to follow. Code is traced through its symbols;
                // data with this many mentions cannot be pinned to a few tests.
                if language(file).is_none() {
                    analysis.unlinked.entry(file.clone()).or_insert(Reason {
                        code: "data-unlinked",
                        message: format!(
                            "{file} is named too often in indexed source to pin to tests"
                        ),
                    });
                }
                continue;
            };
            for (path, line) in lines {
                if &path == file || under(&path, file) {
                    continue;
                }
                // Ancestor directories count only from code, not from prose.
                if language(&path).is_none() {
                    continue;
                }
                mentioned = true;
                let hit = || Hit {
                    hops: 0,
                    possible: true,
                    why: format!("mentions {candidate} at {path}:{line}"),
                };
                if let Some(test) = enclosing_test(index, &path, line) {
                    analysis.mentioned_tests.entry(test).or_insert_with(hit);
                } else if let Some(id) = enclosing(index, &path, line)
                    && !matches!(definitions[id].kind.as_str(), "file" | "module")
                {
                    let added = add_seed(
                        &mut analysis,
                        id,
                        format!("mentions changed {file} as {candidate} at {path}:{line}"),
                    );
                    // A mention is data flow, not a code change: no per-symbol rules.
                    if added
                        && let Some(seed) = analysis.seeds.iter_mut().find(|seed| seed.id == id)
                    {
                        seed.rules = false;
                    }
                } else if is_test_file(&path) {
                    analysis
                        .mentioned_files
                        .entry(path.clone())
                        .or_insert_with(hit);
                }
            }
        }
        if mentioned {
            continue;
        }
        let code = language(file).is_some();
        let ignored = ignore_globs
            .as_ref()
            .is_some_and(|globs| globs.matched_path_or_any_parents(file, false).is_ignore());
        if ignored {
            analysis.ignored.push(Note {
                subject: file.clone(),
                detail: "no test impact (test.ignore)".into(),
            });
        } else if !code && inert(file) {
            analysis.ignored.push(Note {
                subject: file.clone(),
                detail: "no test impact (documentation or repository metadata)".into(),
            });
        } else if !code {
            // Data nothing names, such as a binary fixture: it cannot be traced.
            analysis.unlinked.entry(file.clone()).or_insert(Reason {
                code: "data-unlinked",
                message: format!("{file} is not mentioned by indexed source"),
            });
        }
    }

    // Reach from each seed on its own, so every changed symbol is accounted for.
    let mut name_cache = BTreeMap::<String, Option<Vec<(String, usize)>>>::new();
    for position in 0..analysis.seeds.len() {
        let id = analysis.seeds[position].id;
        let mut starts = vec![(id, false, String::new())];
        for &(method, type_id) in &extra {
            if method == id {
                starts.push((
                    type_id,
                    true,
                    format!(
                        "{} is called implicitly; traced through type {}",
                        short(&definitions[id]),
                        name(&definitions[type_id])
                    ),
                ));
            }
        }
        let mut seed = Seed {
            id,
            origin: String::new(),
            tests: BTreeMap::new(),
            files: BTreeMap::new(),
            dependents: BTreeMap::new(),
            capped: None,
            new: false,
            unused: false,
            rules: true,
        };
        for (start, through_type, note) in &starts {
            let reached = walk(index, &[*start], DEPTH);
            if reached.full || !reached.fanned.is_empty() || reached.beyond > 0 {
                seed.capped = Some(name(&definitions[*start]).to_owned());
            }
            collect(index, &reached, *start, *through_type, note, &mut seed);
        }
        // The name net: tests whose body names the symbol, such as inside a
        // macro the index does not expand.
        let definition = &definitions[id];
        let word = short(definition);
        let family = language_family(&definition.location.file);
        if word.len() >= 3
            && !UBIQUITOUS.contains(&word)
            && !definition.test
            && !matches!(definition.kind.as_str(), "file" | "module")
        {
            let lines = name_cache
                .entry(word.to_owned())
                .or_insert_with(|| find(index, &mut texts, word, true))
                .clone();
            match lines {
                Some(lines) => {
                    for (path, line) in lines {
                        // Names only count within one language.
                        if language_family(&path) != family {
                            continue;
                        }
                        if let Some(test) = enclosing_test(index, &path, line)
                            && test != id
                        {
                            seed.tests.entry(test).or_insert(Hit {
                                hops: 1,
                                possible: true,
                                why: format!("names {word} at {path}:{line}"),
                            });
                        } else if family == Some("JavaScript")
                            && is_test_file(&path)
                            && enclosing(index, &path, line).is_none_or(|enclosing| {
                                matches!(definitions[enclosing].kind.as_str(), "file" | "module")
                            })
                        {
                            // JavaScript tests are calls, not definitions: the file is the unit.
                            seed.files.entry(path.clone()).or_insert(Hit {
                                hops: 1,
                                possible: true,
                                why: format!("names {word} at {path}:{line}"),
                            });
                        }
                    }
                }
                None => seed.capped = Some(word.to_owned()),
            }
        }
        let target = &mut analysis.seeds[position];
        target.tests = seed.tests;
        target.files = seed.files;
        target.dependents = seed.dependents;
        target.capped = seed.capped;
    }

    // A seed that did not exist at HEAD and nothing reaches cannot change a test.
    let mut committed = BTreeMap::<String, Option<String>>::new();
    for seed in &mut analysis.seeds {
        let definition = &definitions[seed.id];
        if matches!(definition.kind.as_str(), "file" | "module") {
            continue;
        }
        let file = &definition.location.file;
        let word = short(definition);
        let before = committed
            .entry(file.clone())
            .or_insert_with(|| git_text_raw(workspace, &["show", &format!("HEAD:./{file}")]));
        seed.new = before
            .as_deref()
            .is_none_or(|text| !text.lines().any(|line| has_word(line, word)));
        // A new method that overrides or shadows another is already called by
        // name, though no link reaches it: it counts as used.
        if seed.new {
            let span = &definition.location;
            let named = name_cache
                .entry(word.to_owned())
                .or_insert_with(|| find(index, &mut texts, word, true))
                .clone();
            seed.unused = named.is_some_and(|lines| {
                lines
                    .iter()
                    .all(|(path, line)| path == file && span.start <= *line && *line <= span.end)
            });
        }
    }
    for seed in &analysis.seeds {
        let definition = &definitions[seed.id];
        analysis.seed_notes.push(Note {
            subject: crate::map::label(definition),
            detail: if seed.new {
                format!("{} (new)", seed.origin)
            } else {
                seed.origin.clone()
            },
        });
    }
    analysis
        .seed_notes
        .sort_by(|a, b| (&a.subject, &a.detail).cmp(&(&b.subject, &b.detail)));
    analysis
        .seed_notes
        .dedup_by(|a, b| a.subject == b.subject && a.detail == b.detail);
    Ok(analysis)
}

fn git_text_raw(workspace: &Workspace, args: &[&str]) -> Option<String> {
    let output = workspace.git(args).ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// `macro_rules!` items in a Rust file, as (name, first line, last line).
fn macro_spans(lines: &[String]) -> Vec<(String, usize, usize)> {
    let mut spans = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at].trim_start();
        let Some(rest) = line.strip_prefix("macro_rules!") else {
            at += 1;
            continue;
        };
        let macro_name: String = rest
            .trim_start()
            .chars()
            .take_while(|&c| identifier(c))
            .collect();
        let mut depth = 0i32;
        let mut opened = false;
        let mut end = at;
        'scan: for (offset, text) in lines.iter().enumerate().skip(at) {
            for c in text.chars() {
                match c {
                    '{' | '(' | '[' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' | ')' | ']' => depth -= 1,
                    _ => {}
                }
                if opened && depth <= 0 {
                    end = offset;
                    break 'scan;
                }
            }
            end = offset;
        }
        spans.push((macro_name, at + 1, end + 1));
        at = end + 1;
    }
    spans
}

/// Records what one walk reached into `seed`.
fn collect(
    index: &Index,
    reached: &Walk,
    start: usize,
    through_type: bool,
    note: &str,
    seed: &mut Seed,
) {
    let definitions = &index.definitions;
    let mut by_dependent = BTreeMap::new();
    for (offset, level) in reached.levels.iter().enumerate() {
        for reach in level {
            by_dependent.insert(reach.dependent, (offset + 1, reach));
        }
    }
    let chain = |from: Dependent| -> String {
        let mut links = Vec::new();
        let mut current = from;
        while let Some((_, reach)) = by_dependent.get(&current) {
            links.push(format!(
                "{}{} at {} → {}",
                if reach.edge.inferred { "possible " } else { "" },
                relationship(reach.edge.kind),
                reach.edge.location,
                name(&definitions[reach.via])
            ));
            if links.len() == CHAIN || reach.via == start {
                break;
            }
            current = Dependent::Symbol(reach.via);
        }
        let mut why = links.join(" · ");
        if !note.is_empty() {
            why = format!("{note} · {why}");
        }
        why
    };
    if definitions[start].test && !through_type {
        seed.tests.entry(start).or_insert(Hit {
            hops: 0,
            possible: false,
            why: "changed test".into(),
        });
    }
    for (offset, level) in reached.levels.iter().enumerate() {
        let hops = offset + 1;
        for reach in level {
            let possible = reach.possible || through_type;
            match reach.dependent {
                Dependent::Symbol(id) if definitions[id].test => {
                    let hit = Hit {
                        hops,
                        possible,
                        why: chain(reach.dependent),
                    };
                    let entry = seed.tests.entry(id).or_insert(Hit {
                        hops,
                        possible,
                        why: String::new(),
                    });
                    if entry.why.is_empty()
                        || (hit.possible, hit.hops) < (entry.possible, entry.hops)
                    {
                        *entry = hit;
                    }
                }
                // A test file's own node: code at its top level, such as
                // JavaScript `test(...)` calls, reaches the change.
                Dependent::Symbol(id)
                    if matches!(definitions[id].kind.as_str(), "file" | "module")
                        && is_test_file(&definitions[id].location.file) =>
                {
                    let file = definitions[id].location.file.clone();
                    let hit = Hit {
                        hops,
                        possible,
                        why: chain(reach.dependent),
                    };
                    let entry = seed.files.entry(file).or_insert(Hit {
                        hops,
                        possible,
                        why: String::new(),
                    });
                    if entry.why.is_empty()
                        || (hit.possible, hit.hops) < (entry.possible, entry.hops)
                    {
                        *entry = hit;
                    }
                }
                Dependent::Symbol(id) => {
                    let distance = seed.dependents.entry(id).or_insert(hops);
                    *distance = (*distance).min(hops);
                }
                Dependent::Scope(file) => {
                    if is_test_file(file) {
                        seed.files.entry(file.to_owned()).or_insert(Hit {
                            hops,
                            possible,
                            why: chain(reach.dependent),
                        });
                    }
                }
            }
        }
    }
}

/// The plan that runs a runner's base command unchanged.
fn full_plan(runner: &Runner, reasons: Vec<Reason>) -> RunnerPlan {
    let cwd = step_cwd(runner);
    RunnerPlan {
        runner: runner.kind,
        root: runner.root.clone(),
        cwd,
        base: runner.base.clone(),
        mode: Mode::Full,
        reasons,
        unit: runner.kind.unit(),
        indexed: 0,
        selected: 0,
        targets: None,
        steps: vec![Step {
            kind: StepKind::Full,
            argv: runner.base.clone(),
            names: BTreeMap::new(),
            widen: BTreeMap::new(),
        }],
        tests: Vec::new(),
        skipped: Vec::new(),
    }
}

/// Where a runner's steps run: the user's directory for a command they gave,
/// otherwise the runner root.
fn step_cwd(runner: &Runner) -> String {
    runner.cwd.clone().unwrap_or_else(|| runner.root.clone())
}

/// Applies fallback reasons: the full suite, or with `--no-fallback` the same
/// subset labeled low-confidence.
fn escalate(
    runner: &Runner,
    mut plan: RunnerPlan,
    reasons: Vec<Reason>,
    no_fallback: bool,
) -> RunnerPlan {
    if reasons.is_empty() {
        return plan;
    }
    if no_fallback {
        plan.reasons.extend(reasons);
        if plan.mode == Mode::Subset && !plan.steps.is_empty() {
            plan.mode = Mode::LowConfidence;
        }
        return plan;
    }
    let mut full = full_plan(runner, reasons);
    full.indexed = plan.indexed;
    full.targets = plan.targets.map(|(_, total)| (total, total));
    full.tests = plan.tests;
    full.skipped = plan.skipped;
    full.cwd = plan.cwd;
    full
}

/// Which seeds a runner is responsible for.
fn in_scope(runner: &Runner, file: &str) -> bool {
    if !runner.contains(file) {
        return false;
    }
    match runner.kind {
        Kind::Cargo => {
            file.ends_with(".rs")
                && runner.cargo.as_ref().is_some_and(|cargo| {
                    cargo
                        .owner(file)
                        .is_some_and(|package| cargo.scope.contains(&package.name))
                })
        }
        Kind::Pytest => file.ends_with(".py"),
        Kind::Jest | Kind::Vitest => {
            matches!(language(file), Some("JavaScript" | "TypeScript" | "TSX"))
        }
        Kind::Go => file.ends_with(".go"),
        Kind::Command => true,
    }
}

/// Whether a change in `file` can change what the runner's base command
/// builds or runs: for Cargo, code in a package it tests or one they depend on.
fn affects(runner: &Runner, file: &str) -> bool {
    match (runner.kind, &runner.cargo) {
        (Kind::Cargo, Some(cargo)) => {
            file.ends_with(".rs")
                && cargo.owner(file).is_some_and(|package| {
                    cargo
                        .scope
                        .iter()
                        .any(|name| cargo.closure(name).contains(&package.name))
                })
        }
        _ => in_scope(runner, file),
    }
}

fn plan_runner(
    workspace: &Workspace,
    index: &Index,
    runner: &Runner,
    analysis: &Analysis,
    no_fallback: bool,
) -> RunnerPlan {
    if let Some(flag) = &runner.selects {
        let mut plan = full_plan(
            runner,
            vec![Reason {
                code: "command-selects",
                message: format!("the command already selects tests ({flag})"),
            }],
        );
        plan.mode = Mode::Command;
        return plan;
    }
    if let Some(label) = &runner.unknown {
        return full_plan(
            runner,
            vec![Reason {
                code: "unknown-runner",
                message: format!("{label} cannot be narrowed"),
            }],
        );
    }
    if let Some(error) = &runner.error {
        return full_plan(
            runner,
            vec![Reason {
                code: "runner-facts",
                message: error.clone(),
            }],
        );
    }
    if analysis.files.is_empty() {
        let mut plan = full_plan(runner, Vec::new());
        plan.mode = Mode::Nothing;
        plan.steps.clear();
        return plan;
    }
    let mut reasons = Vec::new();
    let mut package_scope = BTreeMap::<String, Reason>::new();
    // File-level triggers for files under this runner.
    for (file, status) in &analysis.files {
        let mine = runner.contains(file) || !file.contains('/');
        if !mine {
            continue;
        }
        if let Some(reason) = file_trigger(workspace, runner, file) {
            reasons.push(reason);
            continue;
        }
        let name = file.rsplit('/').next().unwrap_or(file);
        if runner.kind == Kind::Cargo
            && let Some(cargo) = &runner.cargo
            && let Some(package) = cargo.owner(file)
            && cargo.scope.contains(&package.name)
        {
            if name == "Cargo.toml" && file == &package.manifest {
                package_scope.entry(package.name.clone()).or_insert(Reason {
                    code: "package-config",
                    message: format!(
                        "{file} changed: {} and its dependents run in full",
                        package.name
                    ),
                });
                continue;
            }
            if package
                .targets
                .iter()
                .any(|target| target.kind == "custom-build" && &target.src == file)
            {
                package_scope.entry(package.name.clone()).or_insert(Reason {
                    code: "package-config",
                    message: format!(
                        "{file} changed: build scripts can change {} and its dependents",
                        package.name
                    ),
                });
                continue;
            }
            if package
                .targets
                .iter()
                .any(|target| target.kind == "proc-macro")
                && file.ends_with(".rs")
            {
                for dependent in cargo.dependents(&BTreeSet::from([package.name.clone()])) {
                    package_scope.entry(dependent).or_insert(Reason {
                        code: "proc-macro",
                        message: format!("{} is a proc-macro crate", package.name),
                    });
                }
                continue;
            }
        }
        if runner.kind == Kind::Pytest && name == "conftest.py" && runner.contains(file) {
            // Nested conftest: every test file under its directory.
            continue;
        }
        if language(file).is_some() && runner.contains(file) && in_scope_language(runner, file) {
            if let Some(error) = index.errors.get(file.as_str()) {
                reasons.push(Reason {
                    code: "parse-error",
                    message: format!(
                        "{file} has a parse error: {}",
                        crate::summary::short(error, 120)
                    ),
                });
                continue;
            }
            if status != "deleted" && !index.files.contains_key(file.as_str()) {
                let why = index
                    .refresh
                    .skipped
                    .get(file.as_str())
                    .cloned()
                    .unwrap_or_else(|| "skipped by the index".into());
                reasons.push(Reason {
                    code: "unindexed",
                    message: format!("{file} is not indexed ({why})"),
                });
                continue;
            }
            if status == "deleted" {
                // A deleted Rust file can leave a stale `mod` behind that only a
                // build of its package finds; elsewhere importers cannot be traced.
                if let Some(cargo) = &runner.cargo
                    && let Some(package) = cargo.owner(file)
                    && cargo.scope.contains(&package.name)
                {
                    for dependent in cargo.dependents(&BTreeSet::from([package.name.clone()])) {
                        package_scope.entry(dependent).or_insert(Reason {
                            code: "deleted-file",
                            message: format!(
                                "{file} was deleted: {} and its dependents run in full",
                                package.name
                            ),
                        });
                    }
                } else if runner.kind != Kind::Cargo {
                    reasons.push(Reason {
                        code: "deleted-module",
                        message: format!("{file} was deleted; its importers cannot be traced"),
                    });
                }
                continue;
            }
        }
        if let Some(reason) = analysis.unlinked.get(file)
            && runner.contains(file)
        {
            reasons.push(reason.clone());
        }
    }
    let mut whole_targets = BTreeMap::<(String, String), Reason>::new();
    for (file, line, macro_name) in &analysis.macros {
        if let Some(cargo) = &runner.cargo
            && let Some(package) = cargo.owner(file)
            && cargo.scope.contains(&package.name)
        {
            let reason = Reason {
                code: "macro",
                message: format!(
                    "{file}:{line} is inside macro_rules! {macro_name}; expansions are not indexed"
                ),
            };
            // A macro in an integration test file reaches only that test target.
            let tests_dir = if package.dir.is_empty() {
                "tests".to_owned()
            } else {
                format!("{}/tests", package.dir)
            };
            if under(file, &tests_dir) {
                let local = package.targets.iter().find(|target| {
                    target.kind == "test"
                        && (target.src == *file
                            || target
                                .src
                                .strip_suffix("/main.rs")
                                .is_some_and(|dir| under(file, dir)))
                });
                if let Some(target) = local {
                    whole_targets.insert((package.name.clone(), target.name.clone()), reason);
                    continue;
                }
            }
            for dependent in cargo.dependents(&BTreeSet::from([package.name.clone()])) {
                package_scope.entry(dependent).or_insert(Reason {
                    code: "macro",
                    message: format!(
                        "{file}:{line} is inside macro_rules! {macro_name}; expansions are not indexed"
                    ),
                });
            }
        }
    }

    // Binaries that integration tests run whole, as `CARGO_BIN_EXE_<bin>`.
    let mut covered_bins = BTreeSet::new();
    if let Some(cargo) = &runner.cargo {
        let mut texts = Texts::new(workspace);
        for package in cargo
            .packages
            .iter()
            .filter(|p| cargo.scope.contains(&p.name))
        {
            for target in package
                .targets
                .iter()
                .filter(|t| t.kind == "test" && t.test)
            {
                if let Some(text) = texts.get(&target.src) {
                    covered_bins.extend(black_box_bins(text));
                }
            }
        }
    }

    // Per-seed rules: every changed symbol must reach a test through an exact link.
    let definitions = &index.definitions;
    let runnable = |id: usize| in_scope(runner, &definitions[id].location.file);
    for seed in &analysis.seeds {
        let definition = &definitions[seed.id];
        if !seed.rules || definition.test {
            continue;
        }
        let tests: Vec<&Hit> = seed
            .tests
            .iter()
            .filter(|(id, _)| runnable(**id))
            .map(|(_, hit)| hit)
            .chain(
                seed.files
                    .iter()
                    .filter(|(file, _)| in_scope(runner, file))
                    .map(|(_, hit)| hit),
            )
            .collect();
        // A change concerns this runner when its base command builds or runs the
        // code, or when, in the runner's language, it reaches the runner's tests
        // from elsewhere, such as a shared module above a runner started in a
        // subdirectory.
        let file = &definition.location.file;
        let concerned =
            affects(runner, file) || (in_scope_language(runner, file) && !tests.is_empty());
        if !concerned {
            continue;
        }
        // Code in a binary that an integration test runs whole is exercised by it.
        if let Some(cargo) = &runner.cargo
            && let Some((_, target)) = target_of(cargo, definition)
            && target.kind == "bin"
            && covered_bins.contains(&target.name)
        {
            continue;
        }
        if let Some(capped) = &seed.capped {
            reasons.push(Reason {
                code: "walk-capped",
                message: format!(
                    "the impact walk from {capped} stopped at its cap; the subset could be incomplete"
                ),
            });
            continue;
        }
        let label = format!("{} ({})", name(definition), definition.location);
        if tests.iter().any(|hit| !hit.possible) {
            continue;
        }
        if !tests.is_empty() {
            reasons.push(Reason {
                code: "inferred-only",
                message: format!(
                    "{label} reaches tests only through inferred links or name matches"
                ),
            });
        } else if seed.new && seed.unused && seed.dependents.is_empty() {
            // Nothing can call a definition that did not exist and has no users.
        } else {
            reasons.push(Reason {
                code: "no-tests",
                message: format!("no test reaches {label} through indexed links"),
            });
        }
    }

    let mut plan = match runner.kind {
        Kind::Cargo => cargo_plan(
            workspace,
            index,
            runner,
            analysis,
            &package_scope,
            &whole_targets,
        ),
        Kind::Pytest => file_plan(workspace, index, runner, analysis, true),
        Kind::Jest | Kind::Vitest => file_plan(workspace, index, runner, analysis, false),
        Kind::Go => go_plan(index, runner, analysis),
        Kind::Command => full_plan(runner, Vec::new()),
    };
    // Package scope is a subset reason, not a fallback; say why it widened.
    let mut package_reasons: Vec<Reason> = package_scope.into_values().collect();
    package_reasons.sort_by(|a, b| a.message.cmp(&b.message));
    package_reasons.dedup_by(|a, b| a.message == b.message);
    plan.reasons.extend(package_reasons);
    if plan.indexed > 0 && plan.mode == Mode::Subset {
        let share = plan.selected * 100 / plan.indexed;
        if share >= TOO_BROAD_PERCENT {
            reasons.push(Reason {
                code: "too-broad",
                message: format!(
                    "the subset reaches {share}% of indexed {}s",
                    runner.kind.unit()
                ),
            });
        }
    }
    reasons.sort_by(|a, b| (a.code, &a.message).cmp(&(b.code, &b.message)));
    reasons.dedup_by(|a, b| a.code == b.code && a.message == b.message);
    escalate(runner, plan, reasons, no_fallback)
}

fn in_scope_language(runner: &Runner, file: &str) -> bool {
    match runner.kind {
        Kind::Cargo => file.ends_with(".rs"),
        Kind::Pytest => file.ends_with(".py"),
        Kind::Jest | Kind::Vitest => {
            matches!(language(file), Some("JavaScript" | "TypeScript" | "TSX"))
        }
        Kind::Go => file.ends_with(".go"),
        Kind::Command => false,
    }
}

/// Everything selected for one runner, with the strongest evidence per unit.
fn gather<'a>(
    index: &Index,
    runner: &Runner,
    analysis: &'a Analysis,
) -> (BTreeMap<usize, &'a Hit>, BTreeMap<String, &'a Hit>) {
    let definitions = &index.definitions;
    let mut tests = BTreeMap::<usize, &Hit>::new();
    let mut files = BTreeMap::<String, &Hit>::new();
    let better = |new: &Hit, old: &Hit| (new.possible, new.hops) < (old.possible, old.hops);
    for seed in &analysis.seeds {
        for (&id, hit) in &seed.tests {
            if in_scope(runner, &definitions[id].location.file) {
                let entry = tests.entry(id).or_insert(hit);
                if better(hit, entry) {
                    *entry = hit;
                }
            }
        }
        for (file, hit) in &seed.files {
            if in_scope(runner, file) {
                let entry = files.entry(file.clone()).or_insert(hit);
                if better(hit, entry) {
                    *entry = hit;
                }
            }
        }
    }
    for (&id, hit) in &analysis.mentioned_tests {
        if in_scope(runner, &definitions[id].location.file) {
            tests.entry(id).or_insert(hit);
        }
    }
    for (file, hit) in &analysis.mentioned_files {
        if in_scope(runner, file) {
            files.entry(file.clone()).or_insert(hit);
        }
    }
    (tests, files)
}

fn confidence(hit: &Hit) -> &'static str {
    if hit.why.starts_with("mentions ") {
        "mention"
    } else if hit.why == "changed test" {
        "changed-test"
    } else if hit.possible {
        "possible"
    } else {
        "exact"
    }
}

/// Appends Justless's own flags before any `--` in the base command and test
/// arguments after it. The base command's own options, such as fail-fast
/// behavior, are kept as they are.
pub(crate) fn cargo_argv(base: &[String], flags: &[String], test_args: &[String]) -> Vec<String> {
    let split = base.iter().position(|word| word == "--");
    let (head, tail) = match split {
        Some(at) => (&base[..at], &base[at + 1..]),
        None => (base, &base[base.len()..]),
    };
    let mut argv = head.to_vec();
    argv.extend(flags.iter().cloned());
    if !tail.is_empty() || !test_args.is_empty() {
        argv.push("--".into());
        argv.extend(tail.iter().cloned());
        argv.extend(test_args.iter().cloned());
    }
    argv
}

fn argv_bytes(argv: &[String]) -> usize {
    argv.iter().map(|word| word.len() + 1).sum()
}

/// The target of a Rust definition, by the first segment of its qualified name.
fn target_of<'c>(cargo: &'c Cargo, definition: &Definition) -> Option<(&'c Package, &'c Target)> {
    let prefix = definition.qualified.split("::").next()?;
    cargo.target(prefix)
}

/// Whether a target runs under the base command: tests enabled and required
/// features on.
fn runs(package: &Package, target: &Target, args: &[String]) -> Result<(), String> {
    if !target.test {
        return Err(format!(
            "skipped like cargo test: {} sets test = false",
            target.src
        ));
    }
    if !target.required.is_empty()
        && let Some(enabled) = enabled_features(package, args)
    {
        let missing: Vec<&str> = target
            .required
            .iter()
            .filter(|feature| !enabled.contains(*feature))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "skipped like cargo test: {} requires feature {}",
                target.src,
                missing.join(", ")
            ));
        }
    }
    Ok(())
}

/// Test attributes, counted to find tests the index cannot see.
fn test_attributes(text: &str) -> usize {
    text.lines()
        .filter(|line| {
            let line = line.trim_start();
            line.starts_with("#[")
                && (line.starts_with("#[test]")
                    || line.contains("::test]")
                    || line.contains("::test(")
                    || line.starts_with("#[rstest")
                    || line.starts_with("#[test_case"))
        })
        .count()
}

fn cargo_plan(
    workspace: &Workspace,
    index: &Index,
    runner: &Runner,
    analysis: &Analysis,
    package_scope: &BTreeMap<String, Reason>,
    whole_targets: &BTreeMap<(String, String), Reason>,
) -> RunnerPlan {
    let cargo = runner.cargo.as_ref().expect("cargo facts");
    let definitions = &index.definitions;
    let user = &runner.base[2.min(runner.base.len())..];
    let user_args: Vec<String> = user
        .iter()
        .take_while(|word| *word != "--")
        .cloned()
        .collect();
    let mut plan = full_plan(runner, Vec::new());
    plan.mode = Mode::Subset;
    plan.steps.clear();
    let mut texts = Texts::new(workspace);

    // Indexed tests in runnable targets of the packages in scope.
    let mut indexed = 0;
    let mut test_targets = BTreeSet::new();
    for definition in definitions.iter().filter(|d| d.test) {
        if let Some((package, target)) = target_of(cargo, definition)
            && cargo.scope.contains(&package.name)
            && runs(package, target, &user_args).is_ok()
        {
            indexed += 1;
            test_targets.insert((
                package.name.clone(),
                target.name.clone(),
                target.kind.clone(),
            ));
        }
    }
    let (tests, files) = gather(index, runner, analysis);

    // Packages whose library or binary code changed, directly or through a
    // reached definition. Test, example and bench code reaches nothing else.
    let mut touched = BTreeSet::new();
    let mut build = BTreeSet::<(String, String)>::new();
    let add_build = |package: &Package, target: &Target, build: &mut BTreeSet<(String, String)>| {
        if cargo.scope.contains(&package.name)
            && target.flag().is_some()
            && runs(package, target, &user_args).is_ok()
        {
            build.insert((package.name.clone(), target.name.clone()));
        }
    };
    let shared = |target: &Target| !matches!(target.kind.as_str(), "test" | "example" | "bench");
    for seed in &analysis.seeds {
        let definition = &definitions[seed.id];
        if !affects(runner, &definition.location.file) {
            continue;
        }
        if let Some((package, target)) = target_of(cargo, definition) {
            if shared(target) && !definition.test {
                touched.insert(package.name.clone());
            }
            add_build(package, target, &mut build);
        }
        for (&dependent, &distance) in &seed.dependents {
            let dependent = &definitions[dependent];
            if !affects(runner, &dependent.location.file) {
                continue;
            }
            if let Some((package, target)) = target_of(cargo, dependent) {
                if shared(target) {
                    touched.insert(package.name.clone());
                }
                if distance == 1 {
                    add_build(package, target, &mut build);
                }
            }
        }
    }
    for file in &analysis.compile_files {
        if !affects(runner, file) {
            continue;
        }
        let mut found = false;
        for definition in definitions.iter().filter(|d| &d.location.file == file) {
            if let Some((package, target)) = target_of(cargo, definition) {
                if shared(target) {
                    touched.insert(package.name.clone());
                }
                add_build(package, target, &mut build);
                found = true;
            }
        }
        if !found && let Some(package) = cargo.owner(file) {
            for target in &package.targets {
                if &target.src == file || (target.is_lib() && !found) {
                    if shared(target) {
                        touched.insert(package.name.clone());
                    }
                    add_build(package, target, &mut build);
                    found = true;
                }
            }
        }
    }
    let touched_or_dependents = cargo.dependents(&touched);

    // Whole packages first; nothing else runs in them.
    let packages: BTreeSet<String> = cargo
        .dependents(&package_scope.keys().cloned().collect())
        .into_iter()
        .filter(|name| cargo.scope.contains(name))
        .collect();

    let mut selected = Vec::<Selected>::new();
    let mut unfiltered = BTreeSet::<(String, String)>::new();
    let mut named = BTreeMap::<(String, String), Vec<String>>::new();
    let mut plain = true;
    for (&id, hit) in &tests {
        let definition = &definitions[id];
        let Some((package, target)) = target_of(cargo, definition) else {
            if let Some(package) = cargo.owner(&definition.location.file) {
                plan.reasons.push(Reason {
                    code: "unmapped-test",
                    message: format!(
                        "{} at {} has no Cargo target; {} runs in full",
                        name(definition),
                        definition.location,
                        package.name
                    ),
                });
                let mut scope = BTreeSet::from([package.name.clone()]);
                scope.retain(|name| cargo.scope.contains(name));
                for name in scope {
                    unfiltered.extend(package_targets(cargo, &name, &user_args));
                }
            }
            continue;
        };
        if !cargo.scope.contains(&package.name) {
            continue;
        }
        if let Err(why) = runs(package, target, &user_args) {
            plan.skipped.push(Note {
                subject: target.describe(&package.name),
                detail: why,
            });
            continue;
        }
        let test_name = name(definition).to_owned();
        selected.push(Selected {
            id: test_name.clone(),
            target: target.describe(&package.name),
            confidence: confidence(hit),
            why: hit.why.clone(),
        });
        if packages.contains(&package.name) {
            continue;
        }
        let key = (package.name.clone(), target.name.clone());
        if !target.harness {
            unfiltered.insert(key);
            continue;
        }
        let attributes = texts
            .get(&definition.location.file)
            .map(|text| {
                text.lines()
                    .skip(definition.location.start.saturating_sub(1))
                    .take(
                        definition
                            .declaration
                            .saturating_sub(definition.location.start)
                            + 1,
                    )
                    .any(|line| line.contains("rstest") || line.contains("test_case"))
            })
            .unwrap_or(false);
        plain &= !attributes;
        named.entry(key).or_default().push(test_name);
    }
    // Test files reached at file scope run in full: their tests use the change
    // in ways the index did not link to a test definition.
    for (file, hit) in &files {
        for package in &cargo.packages {
            if !cargo.scope.contains(&package.name) {
                continue;
            }
            for target in &package.targets {
                if &target.src == file && target.flag().is_some() {
                    match runs(package, target, &user_args) {
                        Ok(()) => {
                            selected.push(Selected {
                                id: format!("{file} (whole target)"),
                                target: target.describe(&package.name),
                                confidence: confidence(hit),
                                why: hit.why.clone(),
                            });
                            if !packages.contains(&package.name) {
                                unfiltered.insert((package.name.clone(), target.name.clone()));
                            }
                        }
                        Err(why) => plan.skipped.push(Note {
                            subject: target.describe(&package.name),
                            detail: why,
                        }),
                    }
                }
            }
        }
    }
    // Black-box and opaque targets in packages the change reaches.
    for package in &cargo.packages {
        if !cargo.scope.contains(&package.name) || packages.contains(&package.name) {
            continue;
        }
        for target in package
            .targets
            .iter()
            .filter(|target| target.kind == "test")
        {
            let Some(text) = texts.get(&target.src).map(str::to_owned) else {
                continue;
            };
            let key = (package.name.clone(), target.name.clone());
            let mut why = None;
            for bin in black_box_bins(&text) {
                let owner = cargo
                    .packages
                    .iter()
                    .find(|candidate| {
                        candidate
                            .targets
                            .iter()
                            .any(|t| t.kind == "bin" && t.name == bin)
                    })
                    .map_or(package.name.clone(), |owner| owner.name.clone());
                if cargo
                    .closure(&owner)
                    .iter()
                    .any(|name| touched.contains(name))
                {
                    why = Some(format!(
                        "{} runs the {bin} binary, which the change reaches",
                        target.src
                    ));
                    break;
                }
            }
            if why.is_none() && touched_or_dependents.contains(&package.name) {
                if !target.harness {
                    why = Some(format!("{} uses a custom test harness", target.src));
                } else {
                    let visible = definitions
                        .iter()
                        .filter(|d| d.test && d.location.file == target.src)
                        .count();
                    if test_attributes(&text) > visible {
                        why = Some(format!(
                            "{} has tests the index cannot see, such as ones a macro generates",
                            target.src
                        ));
                    }
                }
            }
            let Some(why) = why else { continue };
            match runs(package, target, &user_args) {
                Ok(()) => {
                    if unfiltered.insert(key) {
                        selected.push(Selected {
                            id: format!("{} (whole target)", target.src),
                            target: target.describe(&package.name),
                            confidence: "scope",
                            why,
                        });
                    }
                }
                Err(skip) => plan.skipped.push(Note {
                    subject: target.describe(&package.name),
                    detail: skip,
                }),
            }
        }
    }
    for ((package_name, target_name), reason) in whole_targets {
        let Some(package) = cargo.package(package_name) else {
            continue;
        };
        let Some(target) = package.targets.iter().find(|t| &t.name == target_name) else {
            continue;
        };
        if packages.contains(package_name) {
            continue;
        }
        match runs(package, target, &user_args) {
            Ok(()) => {
                if unfiltered.insert((package_name.clone(), target_name.clone())) {
                    selected.push(Selected {
                        id: format!("{} (whole target)", target.src),
                        target: target.describe(package_name),
                        confidence: "scope",
                        why: reason.message.clone(),
                    });
                }
                plan.reasons.push(reason.clone());
            }
            Err(skip) => plan.skipped.push(Note {
                subject: target.describe(package_name),
                detail: skip,
            }),
        }
    }
    // Too many names collapse to whole targets.
    let name_count: usize = named.values().map(Vec::len).sum();
    let name_bytes: usize = named.values().flatten().map(|name| name.len() + 1).sum();
    if name_count > MAX_NAMES || name_bytes > MAX_ARGV_BYTES {
        unfiltered.extend(named.keys().cloned());
        named.clear();
    }
    for key in &unfiltered {
        named.remove(key);
    }

    let target_flags = |keys: &BTreeSet<(String, String)>| -> Vec<String> {
        let mut flags = Vec::new();
        let packages: BTreeSet<&str> = keys.iter().map(|(p, _)| p.as_str()).collect();
        for package in &packages {
            flags.push("-p".into());
            flags.push((*package).to_owned());
        }
        let mut seen = BTreeSet::new();
        for (package, target) in keys {
            if let Some(package) = cargo.package(package)
                && let Some(target) = package
                    .targets
                    .iter()
                    .find(|t| &t.name == target && t.flag().is_some())
                && let Some(flag) = target.flag()
                && seen.insert(flag.clone())
            {
                flags.extend(flag);
            }
        }
        flags
    };
    let target_key = |package: &str, target: &str| -> String {
        cargo
            .package(package)
            .and_then(|p| p.targets.iter().find(|t| t.name == target))
            .map(|t| t.name.replace('-', "_"))
            .unwrap_or_else(|| target.replace('-', "_"))
    };

    if !packages.is_empty() {
        let mut flags = Vec::new();
        for package in &packages {
            flags.push("-p".into());
            flags.push(package.clone());
        }
        plan.steps.push(Step {
            kind: StepKind::Package,
            argv: cargo_argv(&runner.base, &flags, &[]),
            names: BTreeMap::new(),
            widen: BTreeMap::new(),
        });
    }
    // Compile coverage rides along with the filtered step.
    let mut filtered: BTreeSet<(String, String)> = named.keys().cloned().collect();
    let coverage: BTreeSet<(String, String)> = build
        .iter()
        .filter(|(package, _)| !packages.contains(package))
        .filter(|key| !unfiltered.contains(*key))
        .cloned()
        .collect();
    if !filtered.is_empty() {
        filtered.extend(coverage.iter().cloned());
        let mut names: Vec<String> = named.values().flatten().cloned().collect();
        names.sort();
        names.dedup();
        let mut test_args = names.clone();
        if plain {
            test_args.push("--exact".into());
        }
        let mut step_names = BTreeMap::<String, Vec<String>>::new();
        let mut widen = BTreeMap::new();
        for ((package, target), list) in &named {
            let key = target_key(package, target);
            step_names
                .entry(key.clone())
                .or_default()
                .extend(list.iter().cloned());
            widen.insert(
                key,
                target_flags(&BTreeSet::from([(package.clone(), target.clone())])),
            );
        }
        plan.steps.push(Step {
            kind: StepKind::Filtered,
            argv: cargo_argv(&runner.base, &target_flags(&filtered), &test_args),
            names: step_names,
            widen,
        });
    }
    if !unfiltered.is_empty() {
        plan.steps.push(Step {
            kind: StepKind::Unfiltered,
            argv: cargo_argv(&runner.base, &target_flags(&unfiltered), &[]),
            names: BTreeMap::new(),
            widen: BTreeMap::new(),
        });
    }
    // Doctests of reached library items that carry an example.
    let mut doc_packages = BTreeSet::new();
    let mut doc_paths = BTreeSet::new();
    let mut reached_ids: BTreeSet<usize> = BTreeSet::new();
    for seed in &analysis.seeds {
        reached_ids.insert(seed.id);
        reached_ids.extend(seed.dependents.keys().copied());
    }
    for id in reached_ids {
        let definition = &definitions[id];
        if definition.test || !in_scope(runner, &definition.location.file) {
            continue;
        }
        let Some((package, target)) = target_of(cargo, definition) else {
            continue;
        };
        if !target.is_lib() || !target.doctest || packages.contains(&package.name) {
            continue;
        }
        let documented = texts
            .get(&definition.location.file)
            .map(|text| {
                text.lines()
                    .skip(definition.location.start.saturating_sub(1))
                    .take(
                        definition
                            .declaration
                            .saturating_sub(definition.location.start),
                    )
                    .any(|line| {
                        let line = line.trim_start();
                        (line.starts_with("///")
                            || line.starts_with("//!")
                            || line.starts_with("#[doc"))
                            && line.contains("```")
                    })
            })
            .unwrap_or(false);
        if documented {
            doc_packages.insert(package.name.clone());
            doc_paths.insert(name(definition).to_owned());
        }
    }
    if !doc_packages.is_empty() {
        let mut flags = Vec::new();
        for package in &doc_packages {
            flags.push("-p".into());
            flags.push(package.clone());
        }
        flags.push("--doc".into());
        let paths: Vec<String> = doc_paths.iter().cloned().collect();
        for path in &paths {
            selected.push(Selected {
                id: format!("{path} (doctest)"),
                target: "--doc".into(),
                confidence: "exact",
                why: "documentation example on a reached item".into(),
            });
        }
        plan.steps.push(Step {
            kind: StepKind::Doctests,
            argv: cargo_argv(&runner.base, &flags, &paths),
            names: BTreeMap::new(),
            widen: BTreeMap::new(),
        });
    }
    // Changed targets with no test step still build, as the full suite would.
    if filtered.is_empty() && !coverage.is_empty() {
        let only = plan.steps.is_empty();
        let mut argv = cargo_argv(&runner.base, &target_flags(&coverage), &[]);
        let at = argv
            .iter()
            .position(|word| word == "--")
            .unwrap_or(argv.len());
        argv.insert(at, "--no-run".into());
        plan.steps.push(Step {
            kind: StepKind::Build,
            argv,
            names: BTreeMap::new(),
            widen: BTreeMap::new(),
        });
        if only {
            plan.mode = Mode::BuildOnly;
        }
    }
    if plan.steps.is_empty() {
        plan.mode = Mode::Nothing;
    }
    // Counts: whole packages and whole targets count every indexed test in them.
    let mut count = 0;
    let mut chosen_targets = BTreeSet::new();
    for definition in definitions.iter().filter(|d| d.test) {
        let Some((package, target)) = target_of(cargo, definition) else {
            continue;
        };
        if !cargo.scope.contains(&package.name) || runs(package, target, &user_args).is_err() {
            continue;
        }
        let key = (package.name.clone(), target.name.clone());
        let whole = packages.contains(&package.name) || unfiltered.contains(&key);
        let by_name = named
            .get(&key)
            .is_some_and(|names| names.iter().any(|n| n == name(definition)));
        if whole || by_name {
            count += 1;
            chosen_targets.insert(key);
        }
    }
    selected.sort_by(|a, b| (&a.target, &a.id).cmp(&(&b.target, &b.id)));
    selected.dedup_by(|a, b| a.target == b.target && a.id == b.id);
    plan.skipped
        .sort_by(|a, b| (&a.subject, &a.detail).cmp(&(&b.subject, &b.detail)));
    plan.skipped
        .dedup_by(|a, b| a.subject == b.subject && a.detail == b.detail);
    plan.indexed = indexed;
    plan.selected = count;
    plan.targets = Some((chosen_targets.len(), test_targets.len()));
    plan.tests = selected;
    plan
}

fn package_targets(cargo: &Cargo, package: &str, args: &[String]) -> Vec<(String, String)> {
    cargo
        .package(package)
        .map(|p| {
            p.targets
                .iter()
                .filter(|t| t.flag().is_some() && runs(p, t, args).is_ok())
                .map(|t| (p.name.clone(), t.name.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Binaries an integration test runs: `CARGO_BIN_EXE_<name>` or `cargo_bin("<name>")`.
fn black_box_bins(text: &str) -> BTreeSet<String> {
    let mut bins = BTreeSet::new();
    for (marker, close) in [
        ("CARGO_BIN_EXE_", None),
        ("cargo_bin(\"", Some('"')),
        ("cargo_bin!(\"", Some('"')),
    ] {
        for (at, _) in text.match_indices(marker) {
            let rest = &text[at + marker.len()..];
            let bin: String = match close {
                Some(close) => rest.chars().take_while(|&c| c != close).collect(),
                None => rest
                    .chars()
                    .take_while(|&c| identifier(c) || c == '-')
                    .collect(),
            };
            if !bin.is_empty() {
                bins.insert(bin);
            }
        }
    }
    bins
}

/// Python module path for a file under the runner root, such as `billing.rates`.
fn dotted(relative: &str) -> Option<String> {
    let stem = relative.strip_suffix(".py")?;
    let stem = stem.strip_suffix("/__init__").unwrap_or(stem);
    let stem = stem.strip_prefix("src/").unwrap_or(stem);
    (!stem.is_empty()).then(|| stem.replace('/', "."))
}

/// pytest collects functions named test* at module level or in Test* classes.
fn pytest_node(definition: &Definition) -> Option<String> {
    let parts: Vec<&str> = name(definition).split("::").collect();
    let collected = match parts.as_slice() {
        [function] => function.starts_with("test"),
        [class, function] => class.starts_with("Test") && function.starts_with("test"),
        _ => false,
    };
    collected.then(|| parts.join("::"))
}

fn collectable_python(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
}

fn collectable_js(file: &str) -> bool {
    matches!(language(file), Some("JavaScript" | "TypeScript" | "TSX")) && is_test_file(file)
}

/// pytest (node IDs) and Jest/Vitest (test files): one step naming what to run.
fn file_plan(
    workspace: &Workspace,
    index: &Index,
    runner: &Runner,
    analysis: &Analysis,
    python: bool,
) -> RunnerPlan {
    let definitions = &index.definitions;
    let mut plan = full_plan(runner, Vec::new());
    plan.mode = Mode::Subset;
    plan.steps.clear();
    let collectable = |file: &str| {
        runner.contains(file)
            && if python {
                collectable_python(file)
            } else {
                collectable_js(file)
            }
    };
    let (tests, files) = gather(index, runner, analysis);
    let mut units = BTreeMap::<String, Selected>::new();
    let mut whole_files = BTreeSet::new();
    for (&id, hit) in &tests {
        let definition = &definitions[id];
        let file = &definition.location.file;
        if !collectable(file) {
            continue;
        }
        let relative = runner.relative(file).to_owned();
        let unit = if python {
            match pytest_node(definition) {
                Some(node) => format!("{relative}::{node}"),
                None => {
                    whole_files.insert(relative.clone());
                    relative.clone()
                }
            }
        } else {
            whole_files.insert(relative.clone());
            relative.clone()
        };
        units.entry(unit.clone()).or_insert(Selected {
            id: unit,
            target: relative,
            confidence: confidence(hit),
            why: hit.why.clone(),
        });
    }
    for (file, hit) in &files {
        if !collectable(file) {
            continue;
        }
        let relative = runner.relative(file).to_owned();
        whole_files.insert(relative.clone());
        units.entry(relative.clone()).or_insert(Selected {
            id: relative.clone(),
            target: relative,
            confidence: confidence(hit),
            why: hit.why.clone(),
        });
    }
    if python {
        // The import net: test files that name a changed module's dotted path.
        let mut texts = Texts::new(workspace);
        let modules: Vec<(String, String)> = analysis
            .files
            .iter()
            .filter(|(file, _)| file.ends_with(".py") && runner.contains(file))
            .filter_map(|(file, _)| Some((file.clone(), dotted(runner.relative(file))?)))
            .collect();
        let candidates: Vec<String> = index
            .files
            .keys()
            .filter(|file| collectable(file))
            .cloned()
            .collect();
        for (file, module) in &modules {
            let (package, leaf) = module.rsplit_once('.').unwrap_or(("", module));
            for candidate in &candidates {
                if candidate == file {
                    continue;
                }
                let Some(text) = texts.get(candidate) else {
                    continue;
                };
                let from_import = !package.is_empty()
                    && text.lines().any(|line| {
                        let line = line.trim();
                        line.starts_with(&format!("from {package} import")) && has_word(line, leaf)
                    });
                if text.contains(module.as_str()) || from_import {
                    let relative = runner.relative(candidate).to_owned();
                    whole_files.insert(relative.clone());
                    units.entry(relative.clone()).or_insert(Selected {
                        id: relative.clone(),
                        target: relative,
                        confidence: "mention",
                        why: format!("imports {module}, which changed in {file}"),
                    });
                }
            }
        }
        // A nested conftest.py applies to the test files under its directory.
        for (file, _) in &analysis.files {
            if file.rsplit('/').next() == Some("conftest.py") && runner.contains(file) {
                let directory = file.rsplit_once('/').map_or("", |(d, _)| d);
                for candidate in &candidates {
                    if under(candidate, directory) {
                        let relative = runner.relative(candidate).to_owned();
                        whole_files.insert(relative.clone());
                        units.entry(relative.clone()).or_insert(Selected {
                            id: relative.clone(),
                            target: relative,
                            confidence: "scope",
                            why: format!("{file} provides its fixtures"),
                        });
                    }
                }
            }
        }
    }
    // A whole file covers its node IDs.
    units.retain(|unit, _| {
        !python
            || !unit.contains("::")
            || !whole_files.contains(unit.split("::").next().unwrap_or(unit))
    });
    let indexed = if python {
        definitions
            .iter()
            .filter(|d| d.test && collectable(&d.location.file) && pytest_node(d).is_some())
            .count()
    } else {
        index.files.keys().filter(|file| collectable(file)).count()
    };
    let mut count = 0;
    if python {
        for definition in definitions
            .iter()
            .filter(|d| d.test && collectable(&d.location.file))
        {
            let Some(node) = pytest_node(definition) else {
                continue;
            };
            let relative = runner.relative(&definition.location.file);
            if whole_files.contains(relative) || units.contains_key(&format!("{relative}::{node}"))
            {
                count += 1;
            }
        }
    } else {
        count = whole_files.len();
    }
    let mut ids: Vec<String> = units.keys().cloned().collect();
    if ids.len() > MAX_NAMES || argv_bytes(&ids) > MAX_ARGV_BYTES {
        ids = ids
            .iter()
            .map(|id| id.split("::").next().unwrap_or(id).to_owned())
            .collect();
        ids.dedup();
    }
    if !ids.is_empty() {
        let mut argv = runner.base.clone();
        argv.extend(ids.iter().cloned());
        plan.steps.push(Step {
            kind: StepKind::Filtered,
            argv,
            names: BTreeMap::new(),
            widen: BTreeMap::new(),
        });
    } else {
        plan.mode = Mode::Nothing;
    }
    plan.indexed = indexed;
    plan.selected = count;
    plan.tests = units.into_values().collect();
    plan
}

/// Go: `go test PACKAGES -run '^(TestA|TestB)$'`.
fn go_plan(index: &Index, runner: &Runner, analysis: &Analysis) -> RunnerPlan {
    let definitions = &index.definitions;
    let mut plan = full_plan(runner, Vec::new());
    plan.mode = Mode::Subset;
    plan.steps.clear();
    let (tests, files) = gather(index, runner, analysis);
    let mut packages = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut selected = Vec::new();
    let go_test = |definition: &Definition| {
        definition.location.file.ends_with("_test.go")
            && !name(definition).contains("::")
            && name(definition).starts_with("Test")
    };
    let package_of = |file: &str| -> String {
        match runner.relative(file).rsplit_once('/') {
            Some((directory, _)) => format!("./{directory}"),
            None => ".".into(),
        }
    };
    for (&id, hit) in &tests {
        let definition = &definitions[id];
        if !go_test(definition) {
            continue;
        }
        packages.insert(package_of(&definition.location.file));
        names.insert(name(definition).to_owned());
        selected.push(Selected {
            id: name(definition).to_owned(),
            target: package_of(&definition.location.file),
            confidence: confidence(hit),
            why: hit.why.clone(),
        });
    }
    for (file, hit) in &files {
        if file.ends_with("_test.go") && runner.contains(file) {
            for definition in definitions
                .iter()
                .filter(|d| &d.location.file == file && go_test(d))
            {
                packages.insert(package_of(file));
                names.insert(name(definition).to_owned());
                selected.push(Selected {
                    id: name(definition).to_owned(),
                    target: package_of(file),
                    confidence: confidence(hit),
                    why: hit.why.clone(),
                });
            }
        }
    }
    plan.indexed = definitions
        .iter()
        .filter(|d| go_test(d) && runner.contains(&d.location.file))
        .count();
    plan.selected = selected.len();
    if names.is_empty() {
        plan.mode = Mode::Nothing;
    } else {
        let mut argv: Vec<String> = runner
            .base
            .iter()
            .filter(|word| *word != "./...")
            .cloned()
            .collect();
        argv.extend(packages.iter().cloned());
        argv.push("-run".into());
        let list: Vec<String> = names.into_iter().collect();
        argv.push(format!("^({})$", list.join("|")));
        plan.steps.push(Step {
            kind: StepKind::Filtered,
            argv,
            names: BTreeMap::new(),
            widen: BTreeMap::new(),
        });
    }
    plan.tests = selected;
    plan
}

/// Every runner's full suite, for plain `justless test`.
pub(crate) fn full_suite(runners: &[Runner]) -> Plan {
    Plan {
        version: 1,
        base: "full suite".into(),
        changes: Vec::new(),
        seeds: Vec::new(),
        ignored: Vec::new(),
        runners: runners
            .iter()
            .map(|runner| {
                let mut plan = full_plan(runner, Vec::new());
                if let Some(flag) = &runner.selects {
                    plan.mode = Mode::Command;
                    plan.reasons.push(Reason {
                        code: "command-selects",
                        message: format!("the command already selects tests ({flag})"),
                    });
                }
                plan
            })
            .collect(),
    }
}

/// A runner's directory on disk.
pub(crate) fn runner_dir(workspace: &Workspace, relative: &str) -> std::path::PathBuf {
    if relative.is_empty() {
        workspace.root.clone()
    } else {
        workspace.root.join(Path::new(relative))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_argv_keeps_user_test_arguments_after_its_own_flags() {
        let base: Vec<String> = ["cargo", "test", "--offline", "--", "--nocapture"]
            .map(String::from)
            .to_vec();
        let argv = cargo_argv(
            &base,
            &["-p".into(), "api".into(), "--lib".into()],
            &["tests::a".into(), "--exact".into()],
        );
        assert_eq!(
            argv.join(" "),
            "cargo test --offline -p api --lib -- --nocapture tests::a --exact"
        );
    }

    #[test]
    fn macro_spans_cover_their_bodies() {
        let lines: Vec<String> = [
            "fn a() {}",
            "macro_rules! check {",
            "    ($x:expr) => {",
            "        assert!($x)",
            "    };",
            "}",
            "fn b() {}",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(macro_spans(&lines), vec![("check".to_owned(), 2, 6)]);
    }

    #[test]
    fn black_box_targets_name_their_binaries() {
        let bins = black_box_bins("Command::new(env!(\"CARGO_BIN_EXE_api\")); cargo_bin(\"tool\")");
        assert_eq!(bins, BTreeSet::from(["api".to_owned(), "tool".to_owned()]));
    }

    #[test]
    fn inert_files_are_documentation_and_metadata() {
        assert!(inert("docs/pricing.md"));
        assert!(inert("README.md"));
        assert!(inert(".github/workflows/ci.yml"));
        assert!(!inert("fixtures/orders.json"));
        assert!(!inert("src/notes.txt"));
    }

    #[test]
    fn tokens_skip_comments_and_keywords() {
        let found = tokens(&[
            "use crate::tax::vat;",
            "// a comment",
            "#[allow(dead_code)]",
        ]);
        assert_eq!(found, BTreeSet::from(["tax".to_owned(), "vat".to_owned()]));
    }

    #[test]
    fn python_modules_have_dotted_paths() {
        assert_eq!(dotted("billing/rates.py").as_deref(), Some("billing.rates"));
        assert_eq!(
            dotted("src/billing/__init__.py").as_deref(),
            Some("billing")
        );
    }
}
