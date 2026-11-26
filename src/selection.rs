//! Planning for `justless test --affected`: from the current Git changes to the
//! tests each runner should run, why each was chosen, and when the plan falls
//! back to a wider scope because the index cannot vouch for a subset.
//!
//! The plan is deterministic and runs nothing. `testrun` executes it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::rc::Rc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::context::{Change, Since, changes, relationship};
use crate::doctests::{Docs, Group, doc_like, markdown_examples, may_hold_examples, spans_lines};
use crate::impact::{
    DartGraph, Dependent, JsGraph, RubyGraph, Walk, config_reference, dart_convention_tests,
    deno_convention_tests, is_shared_dart_helper, is_shared_js_config_or_setup,
    is_shared_ruby_helper, rspec_subject_tests, walk,
};
use crate::index::{Definition, Fingerprint, Index};
use crate::map::{is_test_file, name};
use crate::polyglot::Grammar;
use crate::runners::{
    Cargo, Kind, Package, Runner, Target, cargo_user_args, enabled_features, is_bun_test_file,
    is_dart_test_file, is_dart_test_path, is_deno_test_file, is_dot_test_or_spec_file,
    is_node_test_file, under,
};
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
    "pubspec.yaml",
    "deno.json",
    "deno.jsonc",
    "import_map.json",
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
    "deno.lock",
    "pubspec.lock",
    "Gemfile.lock",
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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
    /// The targets a build-only step compiles, as `reports --example weekly`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub built: Vec<String>,
    /// A doctest step's name filters, each of which must match a doctest.
    #[serde(skip)]
    pub filters: Vec<String>,
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
    /// `HEAD 3f2c1ab`, `main (merge-base 3f2c1ab)` with `--base`, or why
    /// there is no base.
    pub base: String,
    /// What a clean tree means and how to test a branch instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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

/// Text of indexed files, and the documentation of Rust files, read once.
struct Texts<'a> {
    workspace: &'a Workspace,
    cache: BTreeMap<String, Option<String>>,
    docs: BTreeMap<String, Option<Rc<Docs>>>,
    /// Byte offsets where each line of a file starts.
    starts: BTreeMap<String, Option<Vec<usize>>>,
    /// Whether a file may hold doc lines that do not look like docs.
    spans: BTreeMap<String, bool>,
    /// Whether a file may hold an example rustdoc runs.
    examples: BTreeMap<String, bool>,
}

impl<'a> Texts<'a> {
    fn new(workspace: &'a Workspace) -> Self {
        Self {
            workspace,
            cache: BTreeMap::new(),
            docs: BTreeMap::new(),
            starts: BTreeMap::new(),
            spans: BTreeMap::new(),
            examples: BTreeMap::new(),
        }
    }

    /// [`may_hold_examples`] of a file, read once.
    fn may_hold_examples(&mut self, file: &str) -> bool {
        if let Some(&examples) = self.examples.get(file) {
            return examples;
        }
        let examples = self.get(file).is_some_and(may_hold_examples);
        self.examples.insert(file.to_owned(), examples);
        examples
    }

    /// [`spans_lines`] of a file, read once.
    fn spans_lines(&mut self, file: &str) -> bool {
        if let Some(&spans) = self.spans.get(file) {
            return spans;
        }
        let spans = self.get(file).is_some_and(spans_lines);
        self.spans.insert(file.to_owned(), spans);
        spans
    }

    fn docs(&mut self, file: &str) -> Option<Rc<Docs>> {
        if !self.docs.contains_key(file) {
            let docs = self.get(file).map(|text| Rc::new(Docs::read(text)));
            self.docs.insert(file.to_owned(), docs);
        }
        self.docs[file].clone()
    }

    /// Line `line` of `file`, as `str::lines` numbers it. Where each line
    /// starts is found once per file, since the name nets ask for thousands.
    fn line(&mut self, file: &str, line: usize) -> Option<String> {
        let at = line.checked_sub(1)?;
        if !self.starts.contains_key(file) {
            let starts = self.get(file).map(|text| {
                std::iter::once(0)
                    .chain(
                        text.match_indices('\n')
                            .map(|(at, _)| at + 1)
                            .filter(|&start| start < text.len()),
                    )
                    .collect()
            });
            self.starts.insert(file.to_owned(), starts);
        }
        let start = *self.starts[file].as_ref()?.get(at)?;
        let text = self.cache[file].as_deref()?;
        let rest = &text[start..];
        let end = rest.find('\n');
        let mut line = &rest[..end.unwrap_or(rest.len())];
        if end.is_some() {
            line = line.strip_suffix('\r').unwrap_or(line);
        }
        Some(line.to_owned())
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

/// Names a line gives `word` with `as`, such as `cost` in
/// `pub use inner::price as cost;`. Casts to primitive types and `_` are not
/// names.
fn aliases(line: &str, word: &str) -> Vec<String> {
    const PRIMITIVE: &[&str] = &[
        "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
        "f32", "f64", "bool", "char", "str", "_",
    ];
    let mut found = Vec::new();
    for (at, _) in line.match_indices(word) {
        let end = at + word.len();
        // `<T as Trait>` names a trait, not another name for `T`.
        if line[..at].chars().next_back().is_some_and(identifier)
            || line[end..].chars().next().is_some_and(identifier)
            || line[..at].trim_end().ends_with('<')
        {
            continue;
        }
        let rest = line[end..].trim_start();
        let Some(rest) = rest.strip_prefix("as") else {
            continue;
        };
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let rest = rest.trim_start();
        let rest = rest.strip_prefix("r#").unwrap_or(rest);
        let name: String = rest.chars().take_while(|&c| identifier(c)).collect();
        if !name.is_empty() && !PRIMITIVE.contains(&name.as_str()) && name != word {
            found.push(name);
        }
    }
    found
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

/// The file node of a Vue, Svelte or Astro component, which is what other
/// code imports.
fn component(index: &Index, file: &str) -> Option<usize> {
    if !matches!(
        index.grammar(file),
        Some(Grammar::Vue | Grammar::Svelte | Grammar::Astro)
    ) {
        return None;
    }
    index
        .at(&Location {
            file: file.to_owned(),
            start: 1,
            end: 1,
        })
        .into_iter()
        .find(|&found| index.definitions[found].kind == "file")
}

/// The components a walk reached inside, such as a function in a component's
/// script that calls the change: each file node with whether only inferred
/// links reached it and what did. A component is imported whole, so its
/// importers depend on the change too.
fn components_reached(index: &Index, reached: &Walk) -> BTreeMap<usize, (bool, String)> {
    let mut found = BTreeMap::<usize, (bool, String)>::new();
    for reach in reached.levels.iter().flatten() {
        let (file, what) = match reach.dependent {
            Dependent::Symbol(id) => {
                let definition = &index.definitions[id];
                (definition.location.file.as_str(), name(definition))
            }
            Dependent::Scope(file) => (file, "top-level code"),
        };
        let Some(node) = component(index, file) else {
            continue;
        };
        if reach.dependent == Dependent::Symbol(node) {
            // The walk goes on from the file node itself.
            continue;
        }
        let entry = found
            .entry(node)
            .or_insert_with(|| (true, format!("{what} in {file} depends on the change")));
        if !reach.possible && entry.0 {
            *entry = (false, format!("{what} in {file} depends on the change"));
        }
    }
    found
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

/// A file's language as `map` names it from the file name, except that a `.m`
/// file is Objective-C only when the index parsed it as that, from the markers
/// in its text: MATLAB and Octave use `.m` too. A `.m` file the index does not
/// hold, such as a deleted one, has no language and counts as data.
fn language(index: &Index, file: &str) -> Option<&'static str> {
    match crate::map::language(file)? {
        "Objective-C" if index.grammar(file) != Some(Grammar::ObjectiveC) => None,
        other => Some(other),
    }
}

/// A language, with JavaScript, its typed dialects and the Vue, Svelte and
/// Astro components that import it as one.
fn language_family(index: &Index, file: &str) -> Option<&'static str> {
    if javascript(file) {
        return Some("JavaScript");
    }
    language(index, file)
}

/// Whether a file is JavaScript, TypeScript or a Vue, Svelte or Astro
/// component: code that Jest and Vitest tests import.
fn javascript(file: &str) -> bool {
    Grammar::for_file(file).is_some_and(|grammar| grammar.family() == Grammar::JavaScript)
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

/// How a changed file reads in the plan: its status, followed by what Git
/// showed of the file as a whole, or only that when nothing else changed,
/// such as `mode 100755 -> 100644` or `symlink deleted`.
fn file_status(status: &str, list: &[&Change]) -> String {
    let whole: Vec<&str> = list
        .iter()
        .filter_map(|change| match change {
            Change::FileLevel(what) | Change::Origin(what) => Some(what.as_str()),
            _ => None,
        })
        .collect();
    let lines = list
        .iter()
        .any(|change| !matches!(change, Change::FileLevel(_) | Change::Origin(_)));
    match (whole.is_empty(), lines) {
        (true, _) => status.to_owned(),
        (false, true) => format!("{status}, {}", whole.join(", ")),
        (false, false) => whole.join(", "),
    }
}

/// Whether a line holds only a comment, which names things without using them.
fn comment_only(line: &str) -> bool {
    let line = line.trim_start();
    (line.starts_with("//") && !doc_line(line))
        || (line.starts_with('#') && !line.starts_with("#[") && !line.starts_with("#!["))
        || line.starts_with("--")
        || line.starts_with("/*")
        || line.starts_with("* ")
        || line == "*"
}

/// Whether a file is code in a language some runner tests: Rust, Python,
/// JavaScript, TypeScript and their components, Dart, Ruby, or Go. Anything
/// else changes tests only as data.
fn runner_language(file: &str) -> bool {
    file.ends_with(".rs")
        || file.ends_with(".py")
        || file.ends_with(".go")
        || file.ends_with(".rb")
        || file.ends_with(".dart")
        || javascript(file)
}

/// Whether a Rust line is documentation, whose examples run as doctests.
fn doc_line(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("///")
        || line.starts_with("//!")
        || line.starts_with("#[doc")
        || line.starts_with("#![doc")
}

/// Whether a line uses `word` the way code uses a definition of `kind`:
/// functions and methods by a call, anything else by name.
fn used_as(line: &str, word: &str, kind: &str) -> bool {
    if !matches!(kind, "function" | "method") {
        return has_word(line, word);
    }
    line.match_indices(word).any(|(at, _)| {
        let after = &line[at + word.len()..];
        let before = &line[..at];
        // `fn main(` defines a function of the same name; it does not call one.
        let defines = before
            .trim_end()
            .rsplit(|c: char| !identifier(c))
            .next()
            .is_some_and(|keyword| matches!(keyword, "fn" | "def" | "func" | "function"));
        !before.chars().next_back().is_some_and(identifier)
            && !defines
            && (after.trim_start().starts_with('(') || after.starts_with("::<"))
    })
}

/// The type behind a trait method its callers reach implicitly, such as
/// `Money` for `Money::fmt`, which `format!` and `to_string` call. For a method
/// the walk `reached`, `new` is left out: callers name it, so its links are in
/// the index, and its type's users are no closer to the change.
fn implicit_type(index: &Index, id: usize, reached: bool) -> Option<usize> {
    let definition = &index.definitions[id];
    if definition.kind != "method"
        || !UBIQUITOUS.contains(&short(definition))
        || (reached && matches!(short(definition), "new" | "constructor" | "__init__"))
    {
        return None;
    }
    let (parent, _) = definition.qualified.rsplit_once("::")?;
    index
        .definitions
        .iter()
        .position(|other| other.qualified == parent && other.kind != "method")
}

fn implicitly(definitions: &[Definition], method: usize, type_id: usize) -> String {
    format!(
        "{} is called implicitly; traced through type {}",
        short(&definitions[method]),
        name(&definitions[type_id])
    )
}

/// Quoted strings on a line, single or double.
fn literals(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    for quote in ['"', '\''] {
        let mut parts = line.split(quote);
        parts.next();
        while let Some(inside) = parts.next() {
            if !inside.is_empty() {
                found.push(inside.to_owned());
            }
            parts.next();
        }
    }
    found
}

/// A path written relative to the file that names it, as a workspace path.
fn resolve(from: &str, literal: &str) -> Option<String> {
    if literal.is_empty() || literal.starts_with('/') || literal.contains("://") {
        return None;
    }
    let mut parts: Vec<&str> = match from.rsplit_once('/') {
        Some((directory, _)) => directory.split('/').collect(),
        None => Vec::new(),
    };
    for part in literal.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Files Rust documentation includes, such as a README behind
/// `#![doc = include_str!("../README.md")]`, with the including file and item.
type DocIncludes = BTreeMap<String, Vec<(String, Option<usize>)>>;

fn doc_includes(index: &Index, texts: &mut Texts) -> DocIncludes {
    let mut found = DocIncludes::new();
    for (path, line) in find(index, texts, "include_str!", false).unwrap_or_default() {
        if !path.ends_with(".rs") {
            continue;
        }
        let Some(text) = texts
            .get(&path)
            .and_then(|text| text.lines().nth(line - 1))
            .map(str::to_owned)
        else {
            continue;
        };
        let trimmed = text.trim_start();
        if !(trimmed.starts_with("#![doc") || trimmed.starts_with("#[doc")) {
            continue;
        }
        let item = if trimmed.starts_with("#![") {
            None
        } else {
            documented_item(index, &path, line)
        };
        for literal in literals(&text) {
            if let Some(included) = resolve(&path, &literal) {
                found
                    .entry(included)
                    .or_default()
                    .push((path.clone(), item));
            }
        }
    }
    found
}

/// The item whose `///` docs hold a line, or `None` for a file's own docs.
fn documented_item(index: &Index, file: &str, line: usize) -> Option<usize> {
    enclosing(index, file, line)
        .filter(|&id| !matches!(index.definitions[id].kind.as_str(), "file" | "module"))
}

/// Rust documentation examples a change reaches.
#[derive(Default)]
struct DocHits {
    /// The file and the item whose own docs hold the examples, or `None`
    /// for every example in the file, with why.
    examples: BTreeMap<(String, Option<usize>), String>,
    /// Files with documentation Justless cannot read: their crate's doctests
    /// run unfiltered, with why.
    crates: BTreeMap<String, String>,
}

impl DocHits {
    fn example(&mut self, file: &str, item: Option<usize>, why: &str) {
        self.examples
            .entry((file.to_owned(), item))
            .or_insert_with(|| why.to_owned());
    }

    fn crate_of(&mut self, file: &str, why: &str) {
        self.crates
            .entry(file.to_owned())
            .or_insert_with(|| why.to_owned());
    }
}

/// What one item's documentation runs: examples rustdoc finds in it, or the
/// whole crate's doctests when its text cannot be read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DocRun {
    Examples,
    Crate,
}

/// Whether documentation holds an example rustdoc runs, `ignore`d ones
/// included with `ignored`. Included files count; one that cannot be read
/// runs the crate's doctests.
fn doc_run(texts: &mut Texts, file: &str, group: &Group, ignored: bool) -> Option<DocRun> {
    if group.unknown {
        return Some(DocRun::Crate);
    }
    if group.runs(ignored) {
        return Some(DocRun::Examples);
    }
    for include in &group.includes {
        let Some(text) = resolve(file, include).and_then(|path| texts.get(&path)) else {
            return Some(DocRun::Crate);
        };
        if markdown_examples(text)
            .iter()
            .any(|example| ignored || !example.ignored)
        {
            return Some(DocRun::Examples);
        }
    }
    None
}

/// The item whose own outer docs `group` is, or `None` when rustdoc names
/// the examples otherwise, as for a file's docs or an enum variant's.
fn documented_by(index: &Index, file: &str, group: &Group) -> Option<usize> {
    if group.inner {
        return None;
    }
    let first = group.first();
    let id = documented_item(index, file, first)?;
    let definition = &index.definitions[id];
    (definition.location.start <= first && first < definition.declaration).then_some(id)
}

/// The test filter that selects an item's doctests. rustdoc names a method
/// after its type with any generics, as in `Shop<T>::total (line 4)`, so a
/// method matches on `::total (line ` instead of its indexed name.
fn doc_filter(definition: &Definition) -> String {
    if definition.kind == "method" {
        format!("::{} (line ", short(definition))
    } else {
        name(definition).to_owned()
    }
}

/// Records the Rust documentation examples on `path:line` when that line is
/// documentation, or lies in a file documentation includes. With `fenced`, the
/// line must sit in a code block rustdoc runs. Returns whether it was
/// documentation.
#[allow(clippy::too_many_arguments)]
fn doc_example(
    index: &Index,
    texts: &mut Texts,
    includes: &DocIncludes,
    path: &str,
    line: usize,
    fenced: bool,
    ignored: bool,
    why: &str,
    hits: &mut DocHits,
) -> bool {
    if let Some(includers) = includes.get(path) {
        let inside = texts.get(path).is_some_and(|text| {
            markdown_examples(text).iter().any(|example| {
                (ignored || !example.ignored) && example.start <= line && line <= example.end
            })
        });
        if !fenced || inside {
            for (file, item) in includers {
                hits.example(file, *item, why);
            }
        }
        return true;
    }
    if !path.ends_with(".rs") {
        return false;
    }
    let Some(text) = texts.line(path, line) else {
        return false;
    };
    // Most lines that name a symbol are code; only parse for one that may be docs.
    if !doc_like(&text) && !texts.spans_lines(path) {
        return false;
    }
    let Some(docs) = texts.docs(path) else {
        return false;
    };
    if docs.opaque(line, &text) {
        hits.crate_of(path, why);
        return true;
    }
    let Some(group) = docs.at(line) else {
        return false;
    };
    if group.unknown {
        hits.crate_of(path, why);
    } else if !fenced || group.in_example(line, ignored) {
        hits.example(path, documented_by(index, path, group), why);
    }
    true
}

/// Records the examples that changed documentation in a Rust file runs: every
/// example in the file when a changed line is documentation holding one, and
/// the crate's when the documentation cannot be read.
fn changed_docs(
    texts: &mut Texts,
    file: &str,
    text: &[String],
    changed: &BTreeSet<usize>,
    ignored: bool,
    hits: &mut DocHits,
) {
    let doc_line_changed = changed.iter().any(|&line| {
        line.checked_sub(1)
            .and_then(|at| text.get(at))
            .is_some_and(|line| doc_like(line))
    });
    if !doc_line_changed && !texts.spans_lines(file) {
        return;
    }
    let Some(docs) = texts.docs(file) else {
        return;
    };
    let mut groups = BTreeSet::new();
    for &line in changed {
        let Some(text) = line.checked_sub(1).and_then(|at| text.get(at)) else {
            continue;
        };
        if docs.opaque(line, text) {
            hits.crate_of(
                file,
                &format!("changed documentation at {file}:{line} that Justless cannot read"),
            );
        } else if let Some(at) = docs.groups.iter().position(|g| g.covers(line))
            && groups.insert(at)
        {
            match doc_run(texts, file, &docs.groups[at], ignored) {
                Some(DocRun::Crate) => hits.crate_of(
                    file,
                    &format!("changed documentation at {file}:{line} that Justless cannot read"),
                ),
                Some(DocRun::Examples) => hits.example(
                    file,
                    None,
                    &format!("changed documentation with an example at {file}:{line}"),
                ),
                None => {}
            }
        }
    }
}

/// Which runners a dependency or configuration file belongs to.
fn concerns(kind: Kind, name: &str, file: &str) -> bool {
    let cargo = kind.is_cargo();
    let javascript = kind.is_js();
    let dart = kind == Kind::DartTest;
    let deno = kind == Kind::DenoTest;
    let ruby = kind == Kind::Rspec;
    match name {
        "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml" => cargo,
        "nextest.toml" => kind == Kind::Nextest,
        "uv.lock" | "poetry.lock" | "Pipfile.lock" | "pyproject.toml" | "setup.py"
        | "setup.cfg" | "pytest.ini" | "tox.ini" | "conftest.py" => kind == Kind::Pytest,
        "package-lock.json"
        | "pnpm-lock.yaml"
        | "yarn.lock"
        | "bun.lock"
        | "bun.lockb"
        | "package.json"
        | ".swcrc"
        | "pnpm-workspace.yaml"
        | "turbo.json"
        | "nx.json"
        | "lerna.json" => javascript,
        "bunfig.toml" => kind == Kind::Bun,
        "deno.json" | "deno.jsonc" | "deno.lock" | "import_map.json" | "import-map.json" => deno,
        "pubspec.yaml"
        | "pubspec.lock"
        | "build.yaml"
        | "analysis_options.yaml"
        | "dart_test.yaml"
        | "flutter_test_config.dart"
        | "l10n.yaml" => dart,
        "Gemfile" | "Gemfile.lock" | ".rspec" | ".ruby-version" | "spec_helper.rb"
        | "rails_helper.rb" | "test_helper.rb" => ruby,
        "go.sum" | "go.mod" | "go.work" => kind == Kind::Go,
        _ if name.starts_with("requirements") && name.ends_with(".txt") => kind == Kind::Pytest,
        _ if file.starts_with(".cargo/config") || file.contains("/.cargo/config") => cargo,
        _ if file == ".config/nextest.toml" || file.ends_with("/.config/nextest.toml") => {
            kind == Kind::Nextest
        }
        _ if is_shared_dart_helper(file) => dart,
        _ if name.ends_with(".gemspec")
            || file.starts_with("spec/support/")
            || file.contains("/spec/support/")
            || file.starts_with("test/support/")
            || file.contains("/test/support/") =>
        {
            ruby
        }
        _ if name.starts_with("jest.config.") || name.starts_with("jest.setup.") => {
            kind == Kind::Jest
        }
        _ if name.starts_with("vitest.config.")
            || name.starts_with("vitest.setup.")
            || name.starts_with("vite.config.") =>
        {
            kind == Kind::Vitest
        }
        _ if name.starts_with("playwright.config.") => kind == Kind::Playwright,
        _ if (name.starts_with("tsconfig") && name.ends_with(".json"))
            || name.starts_with("babel.config.")
            || name.starts_with(".babelrc")
            || is_shared_js_config_or_setup(file) =>
        {
            javascript
        }
        _ => true,
    }
}

/// How a changed file can change what every test sees, before any symbol is looked at.
fn file_trigger(
    workspace: &Workspace,
    index: &Index,
    runner: &Runner,
    file: &str,
) -> Option<Reason> {
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
        "rust-toolchain"
        | "rust-toolchain.toml"
        | ".ruby-version"
        | "pubspec.yaml"
        | "build.yaml"
        | "analysis_options.yaml"
        | "l10n.yaml"
        | "deno.json"
        | "deno.jsonc"
        | "import_map.json"
        | "import-map.json" => true,
        "pyproject.toml"
        | "setup.py"
        | "setup.cfg"
        | ".swcrc"
        | "go.mod"
        | "go.work"
        | "package.json"
        | "pnpm-workspace.yaml"
        | "turbo.json"
        | "nx.json"
        | "lerna.json"
        | "Gemfile" => at_root,
        _ => {
            file.starts_with(".cargo/config")
                || file.contains("/.cargo/config")
                || (name.starts_with("build.") && name.ends_with(".yaml"))
                || name.ends_with(".gemspec")
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
    let nextest_config = runner.kind == Kind::Nextest
        && (name == "nextest.toml"
            || file == ".config/nextest.toml"
            || file.ends_with("/.config/nextest.toml"));
    let js_test_config = runner.kind.is_js() && is_shared_js_config_or_setup(file);
    let dart_test_config = runner.kind == Kind::DartTest && is_shared_dart_helper(file);
    let ruby_test_config = runner.kind == Kind::Rspec && is_shared_ruby_helper(file);
    let test_config = matches!(name, "pytest.ini" | "tox.ini")
        || root_conftest
        || nextest_config
        || js_test_config
        || dart_test_config
        || ruby_test_config
        || name == ".env"
        || name.starts_with(".env.");
    if test_config {
        return Some(Reason {
            code: "test-config",
            message: format!("{file} changed: runner configuration"),
        });
    }
    if runner.kind.is_js()
        && let Some(config) = config_reference(workspace, index, &runner.root, file)
    {
        return Some(Reason {
            code: "test-config",
            message: format!("{file} changed: referenced by runner configuration {config}"),
        });
    }
    if name == "Cargo.toml" && runner.kind.is_cargo() {
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
    /// `definition_id -> index in seeds` for O(1) deduplication and lookup.
    seed_ids: HashMap<usize, usize>,
    /// Changed files and how they changed.
    files: Vec<(String, String)>,
    /// Plan notes for the seeds.
    seed_notes: Vec<Note>,
    /// Changed Rust files whose target must compile even with no symbol seed.
    compile_files: BTreeSet<String>,
    /// Files whose changes could not be pinned to any test or symbol.
    unlinked: BTreeMap<String, Reason>,
    /// Files changed as a whole, with no line range to trace, such as a mode
    /// change or a binary body. Every runner falls back for them.
    file_level: BTreeMap<String, Reason>,
    /// Files with no test impact, and why.
    ignored: Vec<Note>,
    /// Tests or test files selected directly by a mention, by runner-independent key.
    mentioned_tests: BTreeMap<usize, Hit>,
    mentioned_files: BTreeMap<String, Hit>,
    /// Changed ranges that overlap a `macro_rules!` item, as (file, line, macro).
    macros: Vec<(String, usize, String)>,
    /// Files deleted outright.
    deleted: BTreeSet<String>,
    /// Rust documentation examples a change reaches.
    docs: DocHits,
    /// Whether the base command runs `ignore`d tests, so `ignore`d examples count.
    run_ignored: bool,
    /// Changed code files that nothing names by path.
    unmentioned_code: BTreeSet<String>,
    /// Lazily constructed JS/TS barrel & dynamic-import graph (when JS/TS files changed).
    js_graph: Option<JsGraph>,
    /// Lazily constructed Dart/Flutter import/export/part graph (when `.dart` files changed).
    dart_graph: Option<DartGraph>,
    /// Lazily constructed Ruby require & dynamic-loader graph (when `.rb` files changed).
    ruby_graph: Option<RubyGraph>,
}

impl Analysis {
    /// Adds a starting definition once. Returns whether it was new.
    fn add_seed(&mut self, id: usize, origin: String) -> bool {
        if self.seed_ids.contains_key(&id) {
            return false;
        }
        let pos = self.seeds.len();
        self.seed_ids.insert(id, pos);
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

/// Builds the plan for every runner from the current Git changes, against
/// `HEAD` or, with `base`, the merge base of that ref and `HEAD`.
pub(crate) fn plan(
    workspace: &Workspace,
    index: &Index,
    runners: Vec<Runner>,
    settings: &TestSettings,
    no_fallback: bool,
    base: Option<&str>,
) -> Result<Plan> {
    let inside = git_text(workspace, &["rev-parse", "--is-inside-work-tree"]);
    if inside.as_deref() != Some("true") {
        anyhow::ensure!(base.is_none(), "--base needs a Git repository");
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
            note: None,
            changes: Vec::new(),
            seeds: Vec::new(),
            ignored: Vec::new(),
            runners,
        });
    }
    let since = Since::resolve(workspace, false, base)?;
    let head = git_text(workspace, &["rev-parse", "--short", "HEAD"]);
    let base = match &since {
        Since::Base { name, short, .. } => format!("{name} (merge-base {short})"),
        _ => head
            .as_ref()
            .map_or_else(|| "no commits yet".to_owned(), |rev| format!("HEAD {rev}")),
    };
    let changes = changes(workspace, index, &since)?;
    // A clean tree on a branch is no reason to test nothing silently.
    let note = (changes.is_empty() && head.is_some() && matches!(since, Since::Head)).then(|| {
        format!(
            "No changes against HEAD. {}",
            crate::context::base_hint(workspace, "test")
        )
    });
    // `cargo test -- --ignored` runs examples marked `ignore` too.
    let run_ignored = runners.iter().any(|runner| {
        runner.kind == Kind::Cargo
            && runner
                .base
                .iter()
                .any(|word| word == "--ignored" || word == "--include-ignored")
    });
    let mut analysis = analyze(
        workspace,
        index,
        &changes,
        settings,
        run_ignored,
        since.rev(),
    )?;
    // Code in a language no runner here runs, such as SQL or shell, reaches
    // tests only through what names it or links to it; otherwise it is data.
    // A link to a test counts when Justless has a runner for the test's
    // language, run here or not. Tests no runner knows, such as Elixir's ExUnit
    // tests, vouch for nothing, so the code falls back.
    for file in std::mem::take(&mut analysis.unmentioned_code) {
        let runnable = runners
            .iter()
            .any(|runner| in_scope_language(runner, &file));
        let run = |test: &str| {
            runner_language(test) || runners.iter().any(|runner| in_scope(runner, test))
        };
        let linked = analysis.seeds.iter().any(|seed| {
            index.definitions[seed.id].location.file == file
                && (seed
                    .tests
                    .keys()
                    .any(|&test| run(&index.definitions[test].location.file))
                    || seed.files.keys().any(|test| run(test)))
        });
        if runnable || linked {
            continue;
        }
        if inert(&file) {
            analysis.ignored.push(Note {
                subject: file,
                detail: "no test impact (documentation or repository metadata)".into(),
            });
        } else {
            analysis.unlinked.entry(file.clone()).or_insert(Reason {
                code: "data-unlinked",
                message: format!(
                    "{file} is not mentioned by indexed source, and no runner here runs its language"
                ),
            });
        }
    }
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
        note,
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
    run_ignored: bool,
    rev: &str,
) -> Result<Analysis> {
    let definitions = &index.definitions;
    let mut texts = Texts::new(workspace);
    let (changed_ids, section) = crate::impact::changed_in(workspace, index, changes);
    let mut analysis = Analysis {
        seeds: Vec::new(),
        seed_ids: HashMap::new(),
        files: Vec::new(),
        seed_notes: Vec::new(),
        compile_files: BTreeSet::new(),
        unlinked: BTreeMap::new(),
        file_level: BTreeMap::new(),
        ignored: Vec::new(),
        mentioned_tests: BTreeMap::new(),
        mentioned_files: BTreeMap::new(),
        macros: Vec::new(),
        deleted: BTreeSet::new(),
        docs: DocHits::default(),
        run_ignored,
        unmentioned_code: BTreeSet::new(),
        js_graph: None,
        dart_graph: None,
        ruby_graph: None,
    };
    let doc_includes = doc_includes(index, &mut texts);
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
    let mut changed_by_qualified = HashMap::<&str, usize>::with_capacity(changed_ids.len());
    for &id in &changed_ids {
        changed_by_qualified
            .entry(definitions[id].qualified.as_str())
            .or_insert(id);
    }
    for value in &section.values {
        let file = value["file"].as_str().unwrap_or_default().to_owned();
        if let Some(changed) = value["changed"].as_str()
            && let Some(qualified) = value["qualified"].as_str()
            && let Some(&id) = changed_by_qualified.get(qualified)
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
        |file: &str| git_text(workspace, &["cat-file", "-e", &format!("{rev}:./{file}")]).is_some();
    // Names a documentation example may use to exercise a change, each with
    // the Rust files whose crates' doctests run when the name is too common to
    // search: names removed or written outside any definition here, and below
    // everything the change reaches.
    let mut doc_words = BTreeMap::<String, BTreeSet<String>>::new();
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
        analysis
            .files
            .push(((*file).to_owned(), file_status(status, list)));
        // A change no line range shows cannot be traced to what depends on
        // it, even when tests name the file: every runner falls back.
        let opaque: Vec<&str> = list
            .iter()
            .filter_map(|change| match change {
                Change::FileLevel(what) => Some(what.as_str()),
                _ => None,
            })
            .collect();
        let exempt = inert(file)
            || ignore_globs
                .as_ref()
                .is_some_and(|globs| globs.matched_path_or_any_parents(file, false).is_ignore());
        if !opaque.is_empty() && !exempt {
            analysis.file_level.insert(
                (*file).to_owned(),
                Reason {
                    code: "no-line-diff",
                    message: format!("{file} changed without a line diff ({})", opaque.join(", ")),
                },
            );
        }
        if !exists {
            analysis.deleted.insert((*file).to_owned());
        }
        if language(index, file).is_none() {
            continue;
        }
        // Definitions present at the base and gone now, renamed ones included.
        if tracked {
            let before = git_text_raw(workspace, &["show", &format!("{rev}:./{file}")])
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
                    if file.ends_with(".rs") {
                        doc_words
                            .entry(removed_name.to_owned())
                            .or_default()
                            .insert((*file).to_owned());
                    }
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
                    // An import removes no definition, only the names it bound.
                    if deletion.symbols.iter().all(|(_, kind)| kind == "import") {
                        words.extend(tokens(&removed));
                    }
                }
                analysis.compile_files.insert((*file).to_owned());
            }
        }
        if file.ends_with(".rs") {
            for word in &words {
                doc_words
                    .entry(word.clone())
                    .or_default()
                    .insert((*file).to_owned());
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
            // Changed documentation runs the examples in it, wherever the item
            // it documents leads. Lines removed count by their neighbors.
            let mut changed = BTreeSet::new();
            for change in list {
                match change {
                    Change::Ranges(ranges) => {
                        for &(start, end) in ranges {
                            changed.extend(start..=end);
                        }
                    }
                    Change::Deleted(deletions) => {
                        for deletion in deletions {
                            changed.insert(deletion.after.max(1));
                            changed.insert(deletion.after + 1);
                        }
                    }
                    _ => {}
                }
            }
            changed_docs(
                &mut texts,
                file,
                &lines,
                &changed,
                run_ignored,
                &mut analysis.docs,
            );
        }
    }
    analysis.files.sort();

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
        let mut mentions = Vec::<(String, String, usize)>::new();
        for candidate in &candidates {
            let Some(lines) = find(index, &mut texts, candidate, false) else {
                // Named too often to follow. Code is traced through its symbols;
                // data with this many mentions cannot be pinned to a few tests.
                if language(index, file).is_none() {
                    analysis.unlinked.entry(file.clone()).or_insert(Reason {
                        code: "data-unlinked",
                        message: format!(
                            "{file} is named too often in indexed source to pin to tests"
                        ),
                    });
                }
                continue;
            };
            mentions.extend(
                lines
                    .into_iter()
                    .map(|(path, line)| (candidate.clone(), path, line)),
            );
        }
        // Paths relative to the file that names them, such as
        // `include_str!("../README.md")`, whatever the base name.
        if base_name.len() >= 3 {
            for (path, line) in find(index, &mut texts, base_name, false).unwrap_or_default() {
                let Some(text) = texts.get(&path).and_then(|text| text.lines().nth(line - 1))
                else {
                    continue;
                };
                for literal in literals(text) {
                    if literal.ends_with(base_name)
                        && resolve(&path, &literal).as_deref() == Some(file.as_str())
                    {
                        mentions.push((literal, path.clone(), line));
                    }
                }
            }
        }
        let mut mentioned = false;
        for (candidate, path, line) in mentions {
            if &path == file || under(&path, file) {
                continue;
            }
            // Ancestor directories count only from code, not from prose.
            if language(index, &path).is_none() {
                continue;
            }
            let hit = || Hit {
                hops: 0,
                possible: true,
                why: format!("mentions {candidate} at {path}:{line}"),
            };
            // Documentation that includes the file: its examples run.
            if doc_example(
                index,
                &mut texts,
                &doc_includes,
                &path,
                line,
                false,
                run_ignored,
                &format!("its documentation includes {file} at {path}:{line}"),
                &mut analysis.docs,
            ) {
                mentioned = true;
                continue;
            }
            if let Some(test) = enclosing_test(index, &path, line) {
                analysis.mentioned_tests.entry(test).or_insert_with(hit);
                mentioned = true;
                continue;
            }
            let around = enclosing(index, &path, line);
            if let Some(id) = around
                && !matches!(definitions[id].kind.as_str(), "file" | "module")
            {
                let added = add_seed(
                    &mut analysis,
                    id,
                    format!("mentions changed {file} as {candidate} at {path}:{line}"),
                );
                // A mention is data flow, not a code change: no per-symbol rules.
                if added && let Some(&pos) = analysis.seed_ids.get(&id) {
                    analysis.seeds[pos].rules = false;
                }
                mentioned = true;
            } else if is_test_file(&path) || is_deno_test_file(&path) {
                analysis
                    .mentioned_files
                    .entry(path.clone())
                    .or_insert_with(hit);
                mentioned = true;
            } else if runner_language(file) {
                // Code is traced through its own symbols; a path such as
                // `#[path = "..."]` or `include!` adds nothing to follow.
                mentioned = true;
            } else if !texts
                .get(&path)
                .and_then(|text| text.lines().nth(line - 1))
                .is_none_or(comment_only)
            {
                // Code at the top level of a non-test file uses the data. Its
                // importers depend on it; Rust has no such link, so its target
                // builds and the plan cannot vouch for a subset.
                mentioned = true;
                match around {
                    Some(module) if !path.ends_with(".rs") => {
                        if add_seed(
                            &mut analysis,
                            module,
                            format!("mentions changed {file} as {candidate} at {path}:{line}"),
                        ) && let Some(seed) = analysis.seeds.last_mut()
                        {
                            seed.rules = false;
                        }
                    }
                    _ => {
                        if path.ends_with(".rs") {
                            analysis.compile_files.insert(path.clone());
                        }
                        analysis.unlinked.entry(file.clone()).or_insert(Reason {
                            code: "data-unlinked",
                            message: format!(
                                "{file} is used at the top level of {path}:{line}, which no test definition links to"
                            ),
                        });
                    }
                }
            }
        }
        if mentioned {
            continue;
        }
        let code = language(index, file).is_some();
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
        } else {
            // Code nothing names: data too, unless a runner runs its language.
            analysis.unmentioned_code.insert(file.clone());
        }
    }

    // Reach from each seed on its own, so every changed symbol is accounted for.
    let mut name_cache = BTreeMap::<String, Option<Vec<(String, usize)>>>::new();
    for position in 0..analysis.seeds.len() {
        let id = analysis.seeds[position].id;
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
        if definitions[id].test {
            seed.tests.insert(
                id,
                Hit {
                    hops: 0,
                    possible: false,
                    why: "changed test".into(),
                },
            );
        }
        // A Vue, Svelte or Astro component is imported whole, so a change to a
        // function in its script reaches whatever imports the component too,
        // and so does a change that a function in a component depends on.
        let mut starts = vec![id];
        starts.extend(component(index, &definitions[id].location.file).filter(|&file| file != id));
        let mut started_nodes: HashSet<usize> = starts.iter().copied().collect();
        let reached = walk(index, &starts, DEPTH);
        if reached.full || !reached.fanned.is_empty() || reached.beyond > 0 {
            seed.capped = Some(name(&definitions[id]).to_owned());
        }
        collect(index, &reached, &BTreeMap::new(), false, &mut seed);
        let mut inside = components_reached(index, &reached);
        for _ in 0..8 {
            inside.retain(|node, _| !started_nodes.contains(node));
            if inside.is_empty() {
                break;
            }
            started_nodes.extend(inside.keys().copied());
            let mut next = BTreeMap::new();
            for possible in [false, true] {
                let notes: BTreeMap<usize, String> = inside
                    .iter()
                    .filter(|(_, (inferred, _))| *inferred == possible)
                    .map(|(&node, (_, note))| (node, note.clone()))
                    .collect();
                if notes.is_empty() {
                    continue;
                }
                let from: Vec<usize> = notes.keys().copied().collect();
                let reached = walk(index, &from, DEPTH);
                if reached.full || !reached.fanned.is_empty() || reached.beyond > 0 {
                    seed.capped = Some(name(&definitions[from[0]]).to_owned());
                }
                collect(index, &reached, &notes, possible, &mut seed);
                for (node, (inferred, note)) in components_reached(index, &reached) {
                    next.entry(node).or_insert((inferred || possible, note));
                }
            }
            inside = next;
        }
        inside.retain(|node, _| !started_nodes.contains(node));
        if !inside.is_empty() {
            seed.capped = Some(name(&definitions[id]).to_owned());
        }
        // Walks that start past the symbol: the types of trait methods called
        // implicitly, and code that names the symbol where the index has no link.
        let mut further = BTreeMap::<usize, String>::new();
        if let Some(type_id) = implicit_type(index, id, false) {
            further.insert(type_id, implicitly(definitions, id, type_id));
        }
        // The name net: tests whose body names the symbol, such as inside a
        // macro the index does not expand, and other code that does the same.
        let definition = &definitions[id];
        let word = short(definition);
        let family = language_family(index, &definition.location.file);
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
                        // Documentation examples that name the symbol, wherever they sit.
                        if family == Some("Rust")
                            && doc_example(
                                index,
                                &mut texts,
                                &doc_includes,
                                &path,
                                line,
                                true,
                                run_ignored,
                                &format!("its example names {word} at {path}:{line}"),
                                &mut analysis.docs,
                            )
                        {
                            continue;
                        }
                        // Names only count within one language.
                        if language_family(index, &path) != family {
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
                            continue;
                        }
                        let around = enclosing(index, &path, line);
                        let scope = around.is_none_or(|enclosing| {
                            matches!(definitions[enclosing].kind.as_str(), "file" | "module")
                        });
                        if family == Some("JavaScript")
                            && (is_test_file(&path) || is_deno_test_file(&path))
                            && scope
                        {
                            // JavaScript tests are calls, not definitions: the file is the unit.
                            seed.files.entry(path.clone()).or_insert(Hit {
                                hops: 1,
                                possible: true,
                                why: format!("names {word} at {path}:{line}"),
                            });
                        } else if let Some(user) = around
                            && !scope
                            && user != id
                            && !definitions[user].test
                            && definitions[user].kind != "import"
                            && !seed.dependents.contains_key(&user)
                            && texts.get(&path).is_some_and(|text| {
                                text.lines().nth(line - 1).is_some_and(|text| {
                                    !comment_only(text)
                                        && (family == Some("Ruby")
                                            || used_as(text, word, &definition.kind))
                                })
                            })
                        {
                            // Code that uses the symbol without an indexed link,
                            // such as inside `write!`: a possible dependent.
                            seed.dependents.insert(user, 1);
                            further
                                .entry(user)
                                .or_insert_with(|| format!("names {word} at {path}:{line}"));
                        }
                    }
                }
                None => seed.capped = Some(word.to_owned()),
            }
        }
        // A trait method reached on the way, such as `fmt` calling the change, is
        // traced through its type too, since its callers are implicit.
        for &dependent in seed.dependents.keys() {
            if let Some(type_id) = implicit_type(index, dependent, true) {
                further
                    .entry(type_id)
                    .or_insert_with(|| implicitly(definitions, dependent, type_id));
            }
        }
        let mut started = started_nodes;
        for _ in 0..8 {
            further.retain(|start, _| started.insert(*start));
            if further.is_empty() {
                break;
            }
            let starts: Vec<usize> = further.keys().copied().collect();
            let reached = walk(index, &starts, DEPTH);
            if reached.full || !reached.fanned.is_empty() || reached.beyond > 0 {
                seed.capped = Some(name(&definitions[starts[0]]).to_owned());
            }
            let known: HashSet<usize> = seed.dependents.keys().copied().collect();
            collect(index, &reached, &further, true, &mut seed);
            further.clear();
            for &dependent in seed.dependents.keys() {
                if !known.contains(&dependent)
                    && let Some(type_id) = implicit_type(index, dependent, true)
                    && !started.contains(&type_id)
                {
                    further
                        .entry(type_id)
                        .or_insert_with(|| implicitly(definitions, dependent, type_id));
                }
            }
        }
        further.retain(|start, _| !started.contains(start));
        if !further.is_empty() {
            seed.capped = Some(word.to_owned());
        }
        let target = &mut analysis.seeds[position];
        target.tests = seed.tests;
        target.files = seed.files;
        target.dependents = seed.dependents;
        target.capped = seed.capped;
    }

    // JS/TS barrel re-exports & static dynamic imports / require() / vi.mock()
    // augmentation: propagate from changed & reached JS/TS files to whole test files.
    if analysis.files.iter().any(|(file, _)| javascript(file)) {
        let js_graph = JsGraph::build(workspace, index);
        for seed in &mut analysis.seeds {
            let mut js_starts = Vec::new();
            let mut js_seen = HashSet::<&str>::new();
            let seed_file = &definitions[seed.id].location.file;
            if javascript(seed_file) && js_seen.insert(seed_file.as_str()) {
                js_starts.push(seed_file.clone());
            }
            for &dep in seed.dependents.keys() {
                let dep_file = &definitions[dep].location.file;
                if javascript(dep_file) && js_seen.insert(dep_file.as_str()) {
                    js_starts.push(dep_file.clone());
                }
            }
            if js_starts.is_empty() {
                continue;
            }
            let reach = js_graph.reach_from(index, &js_starts);
            for (test_file, (_line, why, _widened)) in reach.test_files {
                seed.files.entry(test_file).or_insert(Hit {
                    hops: 1,
                    possible: false,
                    why,
                });
            }
        }
        // Also propagate directly from changed JS/TS files that had no symbol seed
        // (such as barrel files holding only `export * from '...'` statements).
        let changed_js: Vec<String> = analysis
            .files
            .iter()
            .map(|(f, _)| f.clone())
            .filter(|f| javascript(f))
            .collect();
        let reach = js_graph.reach_from(index, &changed_js);
        for (test_file, (_line, why, _widened)) in reach.test_files {
            analysis.mentioned_files.entry(test_file).or_insert(Hit {
                hops: 1,
                possible: false,
                why,
            });
        }
        analysis.js_graph = Some(js_graph);
    }

    // Dart / Flutter `import`, `export`, and `part` augmentation.
    if analysis
        .files
        .iter()
        .any(|(file, _)| file.ends_with(".dart"))
    {
        let dart_graph = DartGraph::build(workspace, index);
        for seed in &mut analysis.seeds {
            let seed_def = &definitions[seed.id];
            if !seed_def.location.file.ends_with(".dart") || seed_def.test {
                continue;
            }
            let mut dart_starts = vec![seed_def.location.file.clone()];
            let mut dart_seen = HashSet::from([seed_def.location.file.as_str()]);
            for &dep in seed.dependents.keys() {
                let dep_file = &definitions[dep].location.file;
                if dep_file.ends_with(".dart") && dart_seen.insert(dep_file.as_str()) {
                    dart_starts.push(dep_file.clone());
                }
            }
            let reach = dart_graph.reach_from(&dart_starts);
            for (test_file, (_line, why)) in reach.test_files {
                seed.files.entry(test_file).or_insert(Hit {
                    hops: 1,
                    possible: false,
                    why,
                });
            }
        }
        let changed_dart: Vec<String> = analysis
            .files
            .iter()
            .map(|(f, _)| f.clone())
            .filter(|f| f.ends_with(".dart"))
            .collect();
        let reach = dart_graph.reach_from(&changed_dart);
        for (test_file, (_line, why)) in reach.test_files {
            analysis.mentioned_files.entry(test_file).or_insert(Hit {
                hops: 1,
                possible: false,
                why,
            });
        }
        analysis.dart_graph = Some(dart_graph);
    }

    // Ruby RSpec subject matching & static `require_relative`/`require` augmentation.
    if analysis.files.iter().any(|(file, _)| file.ends_with(".rb")) {
        let ruby_graph = RubyGraph::build(workspace, index);
        for seed in &mut analysis.seeds {
            let seed_def = &definitions[seed.id];
            if !seed_def.location.file.ends_with(".rb") || seed_def.test {
                continue;
            }
            let mut rb_starts = vec![seed_def.location.file.clone()];
            let mut rb_seen = HashSet::from([seed_def.location.file.as_str()]);
            let mut rb_ids = vec![seed.id];
            for &dep in seed.dependents.keys() {
                let dep_file = &definitions[dep].location.file;
                if dep_file.ends_with(".rb") {
                    rb_ids.push(dep);
                    if rb_seen.insert(dep_file.as_str()) {
                        rb_starts.push(dep_file.clone());
                    }
                }
            }
            let reach = ruby_graph.reach_from(&rb_starts);
            for (spec_file, (_line, why)) in reach.test_files {
                seed.files.entry(spec_file).or_insert(Hit {
                    hops: 1,
                    possible: false,
                    why,
                });
            }
            for (test_id, hops, inferred) in rspec_subject_tests(index, &rb_ids) {
                seed.tests.entry(test_id).or_insert(Hit {
                    hops,
                    possible: inferred,
                    why: format!("RSpec.describe subject matches {}", name(seed_def)),
                });
            }
        }
        analysis.ruby_graph = Some(ruby_graph);
    }

    // An example exercises a change only through a name on the way to it, such
    // as the public function it calls, so the examples that name anything the
    // change reaches run too. A method too common to search by name, such as
    // `new`, is found by its type, and a name too common to search at all runs
    // its crate's doctests.
    for seed in &analysis.seeds {
        for id in std::iter::once(seed.id).chain(seed.dependents.keys().copied()) {
            let definition = &definitions[id];
            let file = &definition.location.file;
            if definition.test
                || !file.ends_with(".rs")
                || matches!(definition.kind.as_str(), "file" | "module" | "import")
            {
                continue;
            }
            let mut word = short(definition);
            if (word.len() < 3 || UBIQUITOUS.contains(&word))
                && definition.kind == "method"
                && let Some((parent, _)) = definition.qualified.rsplit_once("::")
            {
                word = parent.rsplit("::").next().unwrap_or(parent);
            }
            doc_words
                .entry(word.to_owned())
                .or_default()
                .insert(file.clone());
        }
    }
    let mut searched: BTreeSet<String> = doc_words.keys().cloned().collect();
    while let Some((word, files)) = doc_words.pop_first() {
        let lines = name_cache
            .entry(word.clone())
            .or_insert_with(|| find(index, &mut texts, &word, true))
            .clone();
        let Some(lines) = lines else {
            for file in &files {
                analysis.docs.crate_of(
                    file,
                    &format!("{word} is named too often to find the examples that use it"),
                );
            }
            continue;
        };
        for (path, line) in lines {
            if !path.ends_with(".rs") {
                // An included file, such as a README.
            } else if let Some(text) = texts.line(&path, line) {
                // Examples call a re-export by its own name, as `cost` for
                // `pub use inner::price as cost;`.
                for alias in aliases(&text, &word) {
                    if searched.insert(alias.clone()) {
                        doc_words.insert(alias, files.clone());
                    }
                }
                // Parsing a file for its examples costs more than the search.
                if !texts.may_hold_examples(&path) {
                    continue;
                }
            }
            doc_example(
                index,
                &mut texts,
                &doc_includes,
                &path,
                line,
                true,
                run_ignored,
                &format!("its example names {word} at {path}:{line}"),
                &mut analysis.docs,
            );
        }
    }

    // A seed that did not exist at the base and nothing reaches cannot change a test.
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
            .or_insert_with(|| git_text_raw(workspace, &["show", &format!("{rev}:./{file}")]));
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

/// Records what one walk reached into `seed`. `notes` says how each start was
/// reached, for walks that start past the changed symbol itself. With
/// `through_type`, everything the walk reaches is possible.
fn collect(
    index: &Index,
    reached: &Walk,
    notes: &BTreeMap<usize, String>,
    through_type: bool,
    seed: &mut Seed,
) {
    let definitions = &index.definitions;
    let reached_count: usize = reached.levels.iter().map(Vec::len).sum();
    let mut by_dependent = HashMap::with_capacity(reached_count);
    for level in &reached.levels {
        for reach in level {
            by_dependent.insert(reach.dependent, reach);
        }
    }
    let chain = |from: Dependent| -> String {
        let mut links = Vec::new();
        let mut current = from;
        let mut root = None;
        while let Some(reach) = by_dependent.get(&current) {
            links.push(format!(
                "{}{} at {} → {}",
                if reach.edge.inferred { "possible " } else { "" },
                relationship(reach.edge.kind),
                reach.edge.location,
                name(&definitions[reach.via])
            ));
            root = Some(reach.via);
            if links.len() == CHAIN {
                break;
            }
            current = Dependent::Symbol(reach.via);
        }
        let mut why = links.join(" · ");
        if let Some(note) = root.and_then(|root| notes.get(&root))
            && !note.is_empty()
        {
            why = format!("{note} · {why}");
        }
        why
    };
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
                        && (is_test_file(&definitions[id].location.file)
                            || is_deno_test_file(&definitions[id].location.file)) =>
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
                    if is_test_file(file) || is_deno_test_file(file) {
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
            built: Vec::new(),
            filters: Vec::new(),
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
        Kind::Cargo | Kind::Nextest => {
            file.ends_with(".rs")
                && runner.cargo.as_ref().is_some_and(|cargo| {
                    cargo
                        .owner(file)
                        .is_some_and(|package| cargo.scope.contains(&package.name))
                })
        }
        Kind::Pytest => file.ends_with(".py"),
        Kind::Jest
        | Kind::Vitest
        | Kind::Bun
        | Kind::NodeTest
        | Kind::Playwright
        | Kind::DenoTest => javascript(file),
        Kind::DartTest => file.ends_with(".dart"),
        Kind::Rspec => file.ends_with(".rb"),
        Kind::Go => file.ends_with(".go"),
        Kind::Command => true,
    }
}

/// Whether a change in `file` can change what the runner's base command
/// builds or runs: for Cargo, code in a package it tests or one they depend on.
fn affects(runner: &Runner, file: &str) -> bool {
    match (runner.kind, &runner.cargo) {
        (kind, Some(cargo)) if kind.is_cargo() => {
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
    // A change with no line range can reach tests anywhere, whatever their root.
    let mut reasons: Vec<Reason> = analysis.file_level.values().cloned().collect();
    let mut package_scope = BTreeMap::<String, Reason>::new();
    let mut triggered_files = BTreeSet::<&str>::new();
    // File-level triggers for files under this runner.
    for (file, _) in &analysis.files {
        let deleted = analysis.deleted.contains(file);
        let mine = runner.contains(file) || !file.contains('/');
        if !mine {
            continue;
        }
        if let Some(reason) = file_trigger(workspace, index, runner, file) {
            triggered_files.insert(file.as_str());
            reasons.push(reason);
            continue;
        }
        let name = file.rsplit('/').next().unwrap_or(file);
        if runner.kind.is_cargo()
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
        if language(index, file).is_some()
            && runner.contains(file)
            && in_scope_language(runner, file)
        {
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
            if !deleted && !index.files.contains_key(file.as_str()) {
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
            if deleted {
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
                } else if !runner.kind.is_cargo() {
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
                    whole_targets.insert((package.name.clone(), target.id()), reason);
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
    // Only tests the runner's plan keeps count: a test file pytest would not
    // collect satisfies nothing, so its change falls back instead.
    let runnable = |file: &str| in_scope(runner, file) && collects(runner, file);
    let user_args = cargo_user_args(&runner.base);
    // The same rules the runner's plan keeps a test by: a Cargo target the base
    // command runs (a test with no target runs with its package), a Go test,
    // example or fuzz target.
    let kept = |id: usize| {
        let definition = &definitions[id];
        runnable(&definition.location.file)
            && match (runner.kind, &runner.cargo) {
                (kind, Some(cargo)) if kind.is_cargo() => target_of(cargo, definition)
                    .is_none_or(|(package, target)| runs(package, target, &user_args).is_ok()),
                (Kind::Go, _) => go_test_like(definition),
                _ => true,
            }
    };
    // For `deno test`, trace static ES module `import` chains from each JS/TS
    // seed (and changed files) to Deno test files so namespace (`import * as`)
    // and duplicate-name calls still link cleanly without affecting other JS
    // runners' per-block semantics.
    let mut deno_seed_files = HashMap::<usize, BTreeMap<String, Hit>>::new();
    let mut deno_extra_files = BTreeMap::<String, Hit>::new();
    if runner.kind == Kind::DenoTest
        && let Some(js_graph) = &analysis.js_graph
    {
        for (idx, seed) in analysis.seeds.iter().enumerate() {
            let mut starts = Vec::new();
            let mut seen = HashSet::<&str>::new();
            let seed_file = &definitions[seed.id].location.file;
            if javascript(seed_file) && seen.insert(seed_file.as_str()) {
                starts.push(seed_file.clone());
            }
            for &dep in seed.dependents.keys() {
                let dep_file = &definitions[dep].location.file;
                if javascript(dep_file) && seen.insert(dep_file.as_str()) {
                    starts.push(dep_file.clone());
                }
            }
            if starts.is_empty() {
                continue;
            }
            let reached = js_graph.reach_deno_tests(index, &starts);
            if !reached.is_empty() {
                let map = deno_seed_files.entry(idx).or_default();
                for (test_file, (_line, why)) in reached {
                    map.entry(test_file.clone()).or_insert_with(|| Hit {
                        hops: 1,
                        possible: false,
                        why: why.clone(),
                    });
                    deno_extra_files.entry(test_file).or_insert(Hit {
                        hops: 1,
                        possible: false,
                        why,
                    });
                }
            }
        }
        let changed_js: Vec<String> = analysis
            .files
            .iter()
            .map(|(f, _)| f.clone())
            .filter(|f| javascript(f) && runner.contains(f) && !analysis.deleted.contains(f))
            .collect();
        if !changed_js.is_empty() {
            for (test_file, (_line, why)) in js_graph.reach_deno_tests(index, &changed_js) {
                deno_extra_files.entry(test_file).or_insert(Hit {
                    hops: 1,
                    possible: false,
                    why,
                });
            }
        }
    }

    for (seed_idx, seed) in analysis.seeds.iter().enumerate() {
        let definition = &definitions[seed.id];
        if !seed.rules
            || definition.test
            || triggered_files.contains(definition.location.file.as_str())
        {
            continue;
        }
        let tests: Vec<&Hit> = seed
            .tests
            .iter()
            .filter(|(id, _)| kept(**id))
            .map(|(_, hit)| hit)
            .chain(
                seed.files
                    .iter()
                    .filter(|(file, _)| runnable(file))
                    .map(|(_, hit)| hit),
            )
            .chain(
                deno_seed_files
                    .get(&seed_idx)
                    .into_iter()
                    .flat_map(|map| map.iter())
                    .filter(|(file, _)| runnable(file))
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
        // Code in a binary that an integration test runs whole is exercised by
        // it. Example code no test calls only has to compile, which the plan
        // builds; benchmarks `cargo test` does not build at all.
        if let Some(cargo) = &runner.cargo
            && let Some((_, target)) = target_of(cargo, definition)
            && ((target.kind == "bin" && covered_bins.contains(&target.name))
                || target.kind == "example"
                || (target.kind == "bench" && !target.test))
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
        } else if seed.new
            && ((seed.unused && seed.dependents.is_empty())
                || (!seed.dependents.is_empty()
                    && seed.dependents.keys().all(|&dependent| {
                        runner
                            .cargo
                            .as_ref()
                            .and_then(|cargo| target_of(cargo, &definitions[dependent]))
                            .is_some_and(|(_, target)| target.kind == "example")
                    })))
        {
            // Nothing can call a definition that did not exist and has no
            // users, or only examples, which the plan builds.
        } else {
            reasons.push(Reason {
                code: "no-tests",
                message: format!("no test reaches {label} through indexed links"),
            });
        }
    }

    // Conservative JS/TS barrel re-export and computed dynamic import guards:
    // widen to the full suite whenever a touched barrel has alias/unresolved
    // importers or a computed `import(expr)`/`require(expr)` may load a changed file.
    if runner.kind.is_js()
        && let Some(js_graph) = &analysis.js_graph
    {
        let mut js_seen = HashSet::<&str>::new();
        let mut js_touched = Vec::<String>::new();
        for (f, _) in &analysis.files {
            if javascript(f)
                && runner.contains(f)
                && !analysis.deleted.contains(f)
                && js_seen.insert(f.as_str())
            {
                js_touched.push(f.clone());
            }
        }
        for seed in &analysis.seeds {
            for &dep in seed.dependents.keys() {
                let dep_file = &definitions[dep].location.file;
                if javascript(dep_file)
                    && runner.contains(dep_file)
                    && js_seen.insert(dep_file.as_str())
                {
                    js_touched.push(dep_file.clone());
                }
            }
        }
        if !js_touched.is_empty() {
            let reach = js_graph.reach_from(index, &js_touched);
            for (barrel, importer, line, spec) in reach.alias_barrels {
                if runner.contains(&barrel) {
                    reasons.push(Reason {
                        code: "barrel-reexport",
                        message: format!(
                            "barrel {barrel} is imported via path alias '{spec}' at {importer}:{line}"
                        ),
                    });
                }
            }
            for (barrel, line, spec) in reach.unresolved_barrels {
                if runner.contains(&barrel) {
                    reasons.push(Reason {
                        code: "barrel-reexport",
                        message: format!(
                            "barrel {barrel}:{line} has unresolved re-export '{spec}'"
                        ),
                    });
                }
            }
            for (importer, line, kind, target) in reach.computed_hits {
                if runner.contains(&importer) && runner.contains(&target) {
                    reasons.push(Reason {
                        code: "dynamic-import",
                        message: format!("computed {kind} at {importer}:{line} may load {target}"),
                    });
                }
            }
            for (file, _) in &analysis.files {
                if runner.contains(file)
                    && js_graph.barrels.contains(file)
                    && !reach.test_files.keys().any(|test| runnable(test))
                {
                    reasons.push(Reason {
                        code: "barrel-reexport",
                        message: format!(
                            "{file} is a barrel re-export with no statically traceable test importers"
                        ),
                    });
                }
            }
        }
    }

    // Conservative Ruby dynamic-loader guard for RSpec: if non-spec `.rb` code
    // changed and a dynamic loader (`Zeitwerk`, `autoload`, `Dir.glob` + `require`,
    // or non-literal `require`) exists under the runner root, widen to full suite.
    if runner.kind == Kind::Rspec
        && let Some(ruby_graph) = &analysis.ruby_graph
        && analysis.files.iter().any(|(file, _)| {
            file.ends_with(".rb") && runner.contains(file) && !file.ends_with("_spec.rb")
        })
    {
        for (loader, line, kind) in &ruby_graph.dynamic_loaders {
            if runner.contains(loader) {
                reasons.push(Reason {
                    code: "dynamic-import",
                    message: format!("{kind} at {loader}:{line} loads Ruby files dynamically"),
                });
                break;
            }
        }
    }

    // Conservative Dart reflection guard: if non-test `.dart` code changed and
    // any file under the runner imports `dart:mirrors`, widen to the full suite.
    if runner.kind == Kind::DartTest
        && let Some(dart_graph) = &analysis.dart_graph
        && analysis.files.iter().any(|(file, _)| {
            file.ends_with(".dart") && runner.contains(file) && !is_dart_test_path(file)
        })
    {
        for (site, line) in &dart_graph.reflection_sites {
            if runner.contains(site) {
                reasons.push(Reason {
                    code: "dynamic-import",
                    message: format!("dart:mirrors at {site}:{line} uses runtime reflection"),
                });
                break;
            }
        }
    }

    // Conservative discovery guards: never select a test file that custom runner
    // configuration (`dart_test.yaml` or `deno.json`/`deno.jsonc`) would exclude
    // or miss custom-matched test paths.
    if runner.kind == Kind::DartTest
        && let Some(cfg) = dart_custom_test_config(workspace, runner)
    {
        reasons.push(Reason {
            code: "test-config",
            message: format!("{cfg} configures custom Dart test discovery or tag filters"),
        });
    }
    if runner.kind == Kind::DenoTest
        && let Some(cfg) = deno_custom_test_config(workspace, index, runner)
    {
        reasons.push(Reason {
            code: "test-config",
            message: format!("{cfg} configures custom Deno test include/exclude rules"),
        });
    }

    let mut plan = match runner.kind {
        Kind::Cargo | Kind::Nextest => cargo_plan(
            workspace,
            index,
            runner,
            analysis,
            &package_scope,
            &whole_targets,
        ),
        Kind::Pytest => file_plan(workspace, index, runner, analysis, true, &deno_extra_files),
        Kind::Jest
        | Kind::Vitest
        | Kind::Bun
        | Kind::NodeTest
        | Kind::Playwright
        | Kind::DenoTest
        | Kind::DartTest
        | Kind::Rspec => file_plan(workspace, index, runner, analysis, false, &deno_extra_files),
        Kind::Go => go_plan(workspace, index, runner, analysis),
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
        Kind::Cargo | Kind::Nextest => file.ends_with(".rs"),
        Kind::Pytest => file.ends_with(".py"),
        Kind::Jest
        | Kind::Vitest
        | Kind::Bun
        | Kind::NodeTest
        | Kind::Playwright
        | Kind::DenoTest => javascript(file),
        Kind::DartTest => file.ends_with(".dart"),
        Kind::Rspec => file.ends_with(".rb"),
        Kind::Go => file.ends_with(".go"),
        Kind::Command => false,
    }
}

/// Checks if `<runner.root>/dart_test.yaml` defines custom test paths, filename
/// globs, or tag inclusion/exclusion rules that change what `dart test` runs.
fn dart_custom_test_config(workspace: &Workspace, runner: &Runner) -> Option<String> {
    let rel = if runner.root.is_empty() {
        "dart_test.yaml".to_owned()
    } else {
        format!("{}/dart_test.yaml", runner.root)
    };
    let text = fs::read_to_string(workspace.root.join(&rel)).ok()?;
    let custom = text.lines().any(|raw| {
        if raw.starts_with([' ', '\t', '#']) {
            return false;
        }
        let line = raw.split('#').next().unwrap_or(raw).trim();
        line.starts_with("paths:")
            || line.starts_with("filename:")
            || line.starts_with("include_tags:")
            || line.starts_with("exclude_tags:")
    });
    custom.then_some(rel)
}

/// Checks if `<runner.root>/deno.json` or `deno.jsonc` configures custom test
/// `include`/`exclude` rules (or a top-level `exclude` matching indexed Deno
/// test files) that change what `deno test` discovers.
fn deno_custom_test_config(
    workspace: &Workspace,
    index: &Index,
    runner: &Runner,
) -> Option<String> {
    for name in ["deno.json", "deno.jsonc"] {
        let rel = if runner.root.is_empty() {
            name.to_owned()
        } else {
            format!("{}/{name}", runner.root)
        };
        let Ok(text) = fs::read_to_string(workspace.root.join(&rel)) else {
            continue;
        };
        let cleaned = strip_jsonc(&text);
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&cleaned) else {
            continue;
        };
        if json
            .get("test")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|test| test.contains_key("include") || test.contains_key("exclude"))
        {
            return Some(rel);
        }
        if let Some(excludes) = json.get("exclude").and_then(serde_json::Value::as_array) {
            let matches_test = excludes.iter().filter_map(|v| v.as_str()).any(|entry| {
                let clean = entry.trim_start_matches("./").trim_end_matches('/');
                if clean.is_empty() {
                    return false;
                }
                if clean.contains('*') && clean.contains("test") {
                    return true;
                }
                let prefix = format!("{clean}/");
                index.files.keys().any(|file| {
                    runner.contains(file) && is_deno_test_file(file) && {
                        let local = runner.relative(file);
                        local == clean || local.starts_with(&prefix)
                    }
                })
            });
            if matches_test {
                return Some(rel);
            }
        }
    }
    None
}

/// Strips JSONC `//` / `/* */` comments and trailing commas outside string
/// literals so `serde_json` can parse `deno.jsonc`.
fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut no_comments = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            let ch = text[i..].chars().next().unwrap();
            no_comments.push(ch);
            if b == b'\\' && i + 1 < bytes.len() {
                let next = text[i + 1..].chars().next().unwrap();
                no_comments.push(next);
                i += 1 + next.len_utf8();
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += ch.len_utf8();
            continue;
        }
        if b == b'"' {
            in_string = true;
            no_comments.push('"');
            i += 1;
        } else if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else {
            let ch = text[i..].chars().next().unwrap();
            no_comments.push(ch);
            i += ch.len_utf8();
        }
    }
    let nb = no_comments.as_bytes();
    let mut out = String::with_capacity(no_comments.len());
    let mut j = 0;
    in_string = false;
    while j < nb.len() {
        let b = nb[j];
        if in_string {
            let ch = no_comments[j..].chars().next().unwrap();
            out.push(ch);
            if b == b'\\' && j + 1 < nb.len() {
                let next = no_comments[j + 1..].chars().next().unwrap();
                out.push(next);
                j += 1 + next.len_utf8();
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            j += ch.len_utf8();
            continue;
        }
        if b == b'"' {
            in_string = true;
            out.push('"');
            j += 1;
        } else if b == b',' {
            let mut k = j + 1;
            while k < nb.len() && nb[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < nb.len() && matches!(nb[k], b'}' | b']') {
                j += 1;
                continue;
            }
            out.push(',');
            j += 1;
        } else {
            let ch = no_comments[j..].chars().next().unwrap();
            out.push(ch);
            j += ch.len_utf8();
        }
    }
    out
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
    features_on(package, target, args)
}

/// Whether plain `cargo test` compiles a target: one it runs, or any example.
fn builds(package: &Package, target: &Target, args: &[String]) -> bool {
    if target.kind == "example" {
        features_on(package, target, args).is_ok()
    } else {
        runs(package, target, args).is_ok()
    }
}

/// Whether the target's required features are on under the base command.
fn features_on(package: &Package, target: &Target, args: &[String]) -> Result<(), String> {
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
    let user_args = cargo_user_args(&runner.base);
    let mut plan = full_plan(runner, Vec::new());
    plan.mode = Mode::Subset;
    plan.steps.clear();
    let mut texts = Texts::new(workspace);

    // Indexed tests in runnable targets of the packages in scope, plus per-file
    // test definition counts so opaque-target checks stay O(1).
    let mut indexed = 0;
    let mut test_targets = BTreeSet::new();
    let mut test_counts = HashMap::<&str, usize>::new();
    for definition in definitions.iter().filter(|d| d.test) {
        *test_counts
            .entry(definition.location.file.as_str())
            .or_default() += 1;
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
            && target.build_flag().is_some()
            && builds(package, target, &user_args)
        {
            build.insert((package.name.clone(), target.id()));
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
        let key = (package.name.clone(), target.id());
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
                                unfiltered.insert((package.name.clone(), target.id()));
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
            let key = (package.name.clone(), target.id());
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
                    let visible = test_counts.get(target.src.as_str()).copied().unwrap_or(0);
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
    for ((package_name, target_id), reason) in whole_targets {
        let Some(package) = cargo.package(package_name) else {
            continue;
        };
        let Some(target) = package.target_by_id(target_id) else {
            continue;
        };
        if packages.contains(package_name) {
            continue;
        }
        match runs(package, target, &user_args) {
            Ok(()) => {
                if unfiltered.insert((package_name.clone(), target_id.clone())) {
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
                    .find(|t| &t.id() == target && t.build_flag().is_some())
                && let Some(flag) = target.build_flag()
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
            .and_then(|p| p.target_by_id(target))
            .map(|t| {
                if runner.kind == Kind::Nextest {
                    t.nextest_id(package)
                } else {
                    t.name.replace('-', "_")
                }
            })
            .unwrap_or_else(|| {
                let fallback = target.rsplit(':').next().unwrap_or(target);
                if runner.kind == Kind::Nextest {
                    format!("{package}::{fallback}")
                } else {
                    fallback.replace('-', "_")
                }
            })
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
            built: Vec::new(),
            filters: Vec::new(),
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
            built: Vec::new(),
            filters: Vec::new(),
        });
    }
    if !unfiltered.is_empty() {
        plan.steps.push(Step {
            kind: StepKind::Unfiltered,
            argv: cargo_argv(&runner.base, &target_flags(&unfiltered), &[]),
            names: BTreeMap::new(),
            widen: BTreeMap::new(),
            built: Vec::new(),
            filters: Vec::new(),
        });
    }
    // Doctests: the examples in reached library items' docs, and examples a
    // change reaches elsewhere, such as changed docs or an included README.
    // Documentation that cannot be read runs its crate's doctests unfiltered.
    let mut doc_packages = BTreeSet::new();
    // Test name filter → (shown id, confidence, why).
    let mut doc_paths = BTreeMap::<String, (String, &'static str, String)>::new();
    let mut doc_crates = BTreeMap::<String, String>::new();
    let runs_doctests = |package: &Package, target: &Target| {
        runner.kind == Kind::Cargo
            && target.is_lib()
            && target.doctest
            && cargo.scope.contains(&package.name)
            && !packages.contains(&package.name)
    };
    let lib_of = |file: &str| {
        definitions
            .iter()
            .filter(|d| d.location.file == file)
            .find_map(|d| target_of(cargo, d))
            .or_else(|| {
                let package = cargo.owner(file)?;
                Some((package, package.targets.iter().find(|t| t.is_lib())?))
            })
    };
    let mut reached_ids: BTreeSet<usize> = BTreeSet::new();
    for seed in &analysis.seeds {
        reached_ids.insert(seed.id);
        reached_ids.extend(seed.dependents.keys().copied());
    }
    for id in reached_ids {
        let definition = &definitions[id];
        let file = &definition.location.file;
        if definition.test || !in_scope(runner, file) {
            continue;
        }
        let Some((package, target)) = target_of(cargo, definition) else {
            continue;
        };
        if !runs_doctests(package, target) {
            continue;
        }
        // An item's docs start its span, so an undocumented item needs no parse.
        let documented = texts.get(file).is_some_and(|text| {
            text.lines()
                .skip(definition.location.start.saturating_sub(1))
                .take(
                    definition
                        .declaration
                        .saturating_sub(definition.location.start),
                )
                .any(doc_like)
        });
        let Some(docs) = texts.docs(file).filter(|_| documented) else {
            continue;
        };
        let mut run = None;
        for group in docs.outer(definition.location.start, definition.declaration) {
            match doc_run(&mut texts, file, group, analysis.run_ignored) {
                Some(DocRun::Crate) => run = Some(DocRun::Crate),
                Some(DocRun::Examples) if run.is_none() => run = Some(DocRun::Examples),
                _ => {}
            }
        }
        match run {
            Some(DocRun::Crate) => {
                doc_crates.entry(package.name.clone()).or_insert_with(|| {
                    format!(
                        "documentation Justless cannot read on reached item {}",
                        name(definition)
                    )
                });
            }
            Some(DocRun::Examples) => {
                doc_packages.insert(package.name.clone());
                doc_paths.insert(
                    doc_filter(definition),
                    (
                        name(definition).to_owned(),
                        "exact",
                        "documentation example on a reached item".to_owned(),
                    ),
                );
            }
            None => {}
        }
    }
    // Examples elsewhere that a change reaches.
    for ((file, item), why) in &analysis.docs.examples {
        if !in_scope(runner, file) {
            continue;
        }
        let owner = match item {
            Some(id) => target_of(cargo, &definitions[*id]),
            None => lib_of(file),
        };
        let Some((package, target)) = owner else {
            continue;
        };
        if !runs_doctests(package, target) {
            continue;
        }
        // Doctests are named `reports/src/lib.rs - stats::peak (line 3)`, or
        // without an item for a file's own docs.
        let (filter, id) = match item {
            Some(id) => (
                doc_filter(&definitions[*id]),
                name(&definitions[*id]).to_owned(),
            ),
            None => {
                let within = file
                    .strip_prefix(&package.dir)
                    .map_or(file.as_str(), |rest| rest.trim_start_matches('/'));
                (format!("{within} - "), within.to_owned())
            }
        };
        doc_packages.insert(package.name.clone());
        doc_paths
            .entry(filter)
            .or_insert((id, "mention", why.clone()));
    }
    for (file, why) in &analysis.docs.crates {
        if !in_scope(runner, file) {
            continue;
        }
        if let Some((package, target)) = lib_of(file)
            && runs_doctests(package, target)
        {
            doc_crates
                .entry(package.name.clone())
                .or_insert_with(|| why.clone());
        }
    }
    // A crate whose doctests all run needs no filtered step of its own.
    doc_packages.retain(|package| !doc_crates.contains_key(package));
    if !doc_packages.is_empty() {
        let mut flags = Vec::new();
        for package in &doc_packages {
            flags.push("-p".into());
            flags.push(package.clone());
        }
        flags.push("--doc".into());
        let filters: Vec<String> = doc_paths.keys().cloned().collect();
        for (id, confidence, why) in doc_paths.values() {
            selected.push(Selected {
                id: format!("{id} (doctest)"),
                target: "--doc".into(),
                confidence,
                why: why.clone(),
            });
        }
        // Run a filter that matches no doctest as the crates' whole doctests.
        let widen = BTreeMap::from([("--doc".to_owned(), flags.clone())]);
        plan.steps.push(Step {
            kind: StepKind::Doctests,
            argv: cargo_argv(&runner.base, &flags, &filters),
            names: BTreeMap::new(),
            widen,
            built: Vec::new(),
            filters,
        });
    }
    if !doc_crates.is_empty() {
        let mut flags = Vec::new();
        for (package, why) in &doc_crates {
            flags.push("-p".into());
            flags.push(package.clone());
            selected.push(Selected {
                id: format!("{package} (all doctests)"),
                target: "--doc".into(),
                confidence: "possible",
                why: why.clone(),
            });
        }
        flags.push("--doc".into());
        plan.steps.push(Step {
            kind: StepKind::Doctests,
            argv: cargo_argv(&runner.base, &flags, &[]),
            names: BTreeMap::new(),
            widen: BTreeMap::new(),
            built: Vec::new(),
            filters: Vec::new(),
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
        let built = coverage
            .iter()
            .filter_map(|(package, target)| {
                let package = cargo.package(package)?;
                let target = package
                    .targets
                    .iter()
                    .find(|t| &t.id() == target && t.build_flag().is_some())?;
                Some(target.describe(&package.name))
            })
            .collect();
        plan.steps.push(Step {
            kind: StepKind::Build,
            argv,
            names: BTreeMap::new(),
            widen: BTreeMap::new(),
            built,
            filters: Vec::new(),
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
        let key = (package.name.clone(), target.id());
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
                .map(|t| (p.name.clone(), t.id()))
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

/// A JavaScript or TypeScript test file. Components hold no tests of their own.
fn collectable_js(file: &str) -> bool {
    matches!(
        crate::map::language(file),
        Some("JavaScript" | "TypeScript" | "TSX")
    ) && is_test_file(file)
}

/// Whether the runner's full suite runs tests in `file`: pytest's
/// `python_files` and `testpaths`, Jest/Vitest/Bun/Node/Playwright/Deno test
/// files, Dart/Flutter `test/*_test.dart` files, RSpec spec files, Go test files.
fn collects(runner: &Runner, file: &str) -> bool {
    runner.contains(file)
        && match runner.kind {
            Kind::Pytest => runner.pytest_collects(file),
            Kind::Jest => collectable_js(file),
            Kind::Vitest | Kind::Playwright => is_dot_test_or_spec_file(file),
            Kind::Bun => is_bun_test_file(file),
            Kind::NodeTest => is_node_test_file(file),
            Kind::DenoTest => is_deno_test_file(file),
            Kind::DartTest => is_dart_test_file(&runner.root, file),
            Kind::Rspec => file.ends_with("_spec.rb"),
            Kind::Go => file.ends_with("_test.go"),
            Kind::Cargo | Kind::Nextest | Kind::Command => true,
        }
}

/// pytest (node IDs) and file-based runners (Jest/Vitest/Bun/Node/Playwright/Deno/Dart/RSpec):
/// one step naming what to run.
fn file_plan(
    workspace: &Workspace,
    index: &Index,
    runner: &Runner,
    analysis: &Analysis,
    python: bool,
    extra_files: &BTreeMap<String, Hit>,
) -> RunnerPlan {
    let definitions = &index.definitions;
    let mut plan = full_plan(runner, Vec::new());
    plan.mode = Mode::Subset;
    plan.steps.clear();
    let collectable = |file: &str| collects(runner, file);
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
    for (file, hit) in files
        .iter()
        .map(|(file, hit)| (file.as_str(), *hit))
        .chain(extra_files.iter().map(|(file, hit)| (file.as_str(), hit)))
    {
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
    if runner.kind == Kind::DartTest {
        for (file, _) in &analysis.files {
            if !file.ends_with(".dart") || !runner.contains(file) {
                continue;
            }
            for candidate in dart_convention_tests(index, &runner.root, file) {
                if !collectable(&candidate) {
                    continue;
                }
                let relative = runner.relative(&candidate).to_owned();
                whole_files.insert(relative.clone());
                units.entry(relative.clone()).or_insert(Selected {
                    id: relative.clone(),
                    target: relative,
                    confidence: "mention",
                    why: format!("matches changed {file} by Dart test convention"),
                });
            }
        }
    }
    if runner.kind == Kind::DenoTest {
        for (file, _) in &analysis.files {
            if !javascript(file) || !runner.contains(file) {
                continue;
            }
            for candidate in deno_convention_tests(index, &runner.root, file) {
                if !collectable(&candidate) {
                    continue;
                }
                let relative = runner.relative(&candidate).to_owned();
                whole_files.insert(relative.clone());
                units.entry(relative.clone()).or_insert(Selected {
                    id: relative.clone(),
                    target: relative,
                    confidence: "mention",
                    why: format!("matches changed {file} by Deno test convention"),
                });
            }
        }
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
        if runner.kind == Kind::DenoTest
            && let Some(sep) = argv.iter().position(|word| word == "--")
        {
            // Insert selected test files before `--` so trailing script args
            // remain forwarded to `Deno.args`.
            argv.splice(sep..sep, ids.iter().cloned());
        } else {
            argv.extend(ids.iter().cloned());
        }
        plan.steps.push(Step {
            kind: StepKind::Filtered,
            argv,
            names: BTreeMap::new(),
            widen: BTreeMap::new(),
            built: Vec::new(),
            filters: Vec::new(),
        });
    } else {
        plan.mode = Mode::Nothing;
    }
    plan.indexed = indexed;
    plan.selected = count;
    plan.tests = units.into_values().collect();
    plan
}

/// What `go test` runs: tests, examples and fuzz seeds in `_test.go` files. An
/// example or fuzz target has no `*testing.T` and runs with its whole package.
fn go_test_like(definition: &Definition) -> bool {
    definition.location.file.ends_with("_test.go")
        && ["Test", "Example", "Fuzz"]
            .iter()
            .any(|prefix| short(definition).starts_with(prefix))
}

/// Go's command for some packages: the base command with `./...` replaced,
/// and `-v` so the tests that ran are named and can be checked.
pub(crate) fn go_argv(base: &[String], packages: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = base
        .iter()
        .filter(|word| *word != "./...")
        .cloned()
        .collect();
    let named = argv
        .iter()
        .any(|word| word == "-json" || word == "-v" || word.starts_with("-v="));
    if !named && let Some(at) = argv.iter().position(|word| word == "test") {
        argv.insert(at + 1, "-v".into());
    }
    argv.extend(packages.iter().cloned());
    argv
}

/// Go: `go test -v PACKAGES -run '^(TestA|TestB)$'` for top-level tests, and
/// whole packages for tests `-run` cannot name, such as suite methods.
fn go_plan(
    workspace: &Workspace,
    index: &Index,
    runner: &Runner,
    analysis: &Analysis,
) -> RunnerPlan {
    let definitions = &index.definitions;
    let mut plan = full_plan(runner, Vec::new());
    plan.mode = Mode::Subset;
    plan.steps.clear();
    let mut texts = Texts::new(workspace);
    let (tests, files) = gather(index, runner, analysis);
    // Package → names for `-run`, and packages that run whole.
    let mut named = BTreeMap::<String, BTreeSet<String>>::new();
    let mut whole = BTreeSet::<String>::new();
    let mut selected = Vec::new();
    let test_like = |definition: &Definition| {
        go_test_like(definition) && runner.contains(&definition.location.file)
    };
    // `-run` matches only top-level `func TestX(t *testing.T)`; a suite method
    // such as `func (s *CartSuite) TestCount()` runs as a subtest of another.
    let top_level = |texts: &mut Texts, definition: &Definition| {
        definition.kind == "function"
            && !name(definition).contains("::")
            && texts
                .get(&definition.location.file)
                .and_then(|text| text.lines().nth(definition.declaration.saturating_sub(1)))
                .is_some_and(|line| line.contains("*testing.T"))
    };
    let package_of = |file: &str| -> String {
        match runner.relative(file).rsplit_once('/') {
            Some((directory, _)) => format!("./{directory}"),
            None => ".".into(),
        }
    };
    let mut choose = |definition: &Definition, hit: &Hit, texts: &mut Texts| {
        let package = package_of(&definition.location.file);
        if top_level(texts, definition) {
            named
                .entry(package.clone())
                .or_default()
                .insert(name(definition).to_owned());
        } else {
            whole.insert(package.clone());
        }
        selected.push(Selected {
            id: short(definition).to_owned(),
            target: package,
            confidence: confidence(hit),
            why: hit.why.clone(),
        });
    };
    for (&id, hit) in &tests {
        if test_like(&definitions[id]) {
            choose(&definitions[id], hit, &mut texts);
        }
    }
    for (file, hit) in &files {
        if file.ends_with("_test.go") && runner.contains(file) {
            for definition in definitions
                .iter()
                .filter(|d| &d.location.file == file && test_like(d))
            {
                choose(definition, hit, &mut texts);
            }
        }
    }
    named.retain(|package, _| !whole.contains(package));
    plan.indexed = definitions.iter().filter(|d| test_like(d)).count();
    plan.selected = definitions
        .iter()
        .filter(|d| {
            test_like(d) && {
                let package = package_of(&d.location.file);
                whole.contains(&package)
                    || named
                        .get(&package)
                        .is_some_and(|names| names.contains(name(d)))
            }
        })
        .count();
    if !named.is_empty() {
        let packages: Vec<String> = named.keys().cloned().collect();
        let mut argv = go_argv(&runner.base, &packages);
        argv.push("-run".into());
        let list: BTreeSet<&String> = named.values().flatten().collect();
        argv.push(format!(
            "^({})$",
            list.into_iter().cloned().collect::<Vec<_>>().join("|")
        ));
        plan.steps.push(Step {
            kind: StepKind::Filtered,
            argv,
            widen: packages
                .iter()
                .map(|package| (package.clone(), vec![package.clone()]))
                .collect(),
            names: named
                .into_iter()
                .map(|(package, names)| (package, names.into_iter().collect()))
                .collect(),
            built: Vec::new(),
            filters: Vec::new(),
        });
    }
    if !whole.is_empty() {
        let packages: Vec<String> = whole.into_iter().collect();
        plan.steps.push(Step {
            kind: StepKind::Unfiltered,
            argv: go_argv(&runner.base, &packages),
            names: BTreeMap::new(),
            widen: BTreeMap::new(),
            built: Vec::new(),
            filters: Vec::new(),
        });
    }
    if plan.steps.is_empty() {
        plan.mode = Mode::Nothing;
    }
    plan.tests = selected;
    plan
}

/// Every runner's full suite, for plain `justless test`.
pub(crate) fn full_suite(runners: &[Runner]) -> Plan {
    Plan {
        version: 1,
        base: "full suite".into(),
        note: None,
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
    fn code_uses_functions_by_calling_them() {
        assert!(used_as(
            "        write!(f, \"{}\", grouped(self.0))",
            "grouped",
            "function"
        ));
        assert!(used_as(
            "    let x = parse::<u8>(text);",
            "parse",
            "function"
        ));
        assert!(!used_as("    let grouped = 3;", "grouped", "function"));
        assert!(!used_as("fn main() {", "main", "function"));
        assert!(!used_as("    regrouped(1)", "grouped", "function"));
        assert!(used_as("    let rate: Rate = x;", "Rate", "struct"));
        assert!(comment_only("    // grouped(1)"));
        assert!(!comment_only("    /// grouped(1)"));
        assert!(!comment_only("#[test]"));
        assert!(doc_line("//! assert_eq!(peak(&[1]), 1);"));
    }

    #[test]
    fn relative_paths_resolve_against_the_file_that_names_them() {
        assert_eq!(
            literals("#![doc = include_str!(\"../README.md\")]"),
            ["../README.md"]
        );
        assert_eq!(literals("open('data/rates.json')"), ["data/rates.json"]);
        assert_eq!(
            resolve("reports/src/lib.rs", "../README.md").as_deref(),
            Some("reports/README.md")
        );
        assert_eq!(
            resolve("src/lib.rs", "./data/x.json").as_deref(),
            Some("src/data/x.json")
        );
        assert_eq!(resolve("lib.rs", "../../x"), None);
        assert_eq!(resolve("src/lib.rs", "/etc/passwd"), None);
    }

    #[test]
    fn doc_examples_are_the_code_blocks() {
        let rust = Docs::read(
            "//! Helpers.\n//!\n//! ```\n//! assert_eq!(peak(&[1]), 1);\n//! ```\n/// Uses peak.\nfn a() {}\n",
        );
        assert!(rust.at(4).is_some_and(|docs| docs.in_example(4, false)));
        assert!(rust.at(6).is_some_and(|docs| !docs.in_example(6, false)));
        let markdown = markdown_examples(
            "# x\n\n```\nassert_eq!(mean(&[2]), 2);\n```\nmean is the average.\n",
        );
        assert!(markdown.iter().any(|e| e.start <= 4 && 4 <= e.end));
        assert!(!markdown.iter().any(|e| e.start <= 6 && 6 <= e.end));
        // Methods match whatever generics rustdoc prints with their type.
        let method = |kind: &str| Definition {
            qualified: "probe::Shop::total".into(),
            kind: kind.into(),
            location: Location {
                file: "src/lib.rs".into(),
                start: 1,
                end: 3,
            },
            declaration: 2,
            test: false,
        };
        assert_eq!(doc_filter(&method("method")), "::total (line ");
        assert_eq!(doc_filter(&method("function")), "Shop::total");
    }

    #[test]
    fn go_subsets_name_the_tests_they_ran() {
        let base: Vec<String> = ["go", "test", "-count=1", "./..."]
            .map(String::from)
            .to_vec();
        assert_eq!(
            go_argv(&base, &["./cart".into()]).join(" "),
            "go test -v -count=1 ./cart"
        );
        let json: Vec<String> = ["go", "test", "-json"].map(String::from).to_vec();
        assert_eq!(
            go_argv(&json, &["./cart".into()]).join(" "),
            "go test -json ./cart"
        );
    }

    #[test]
    fn python_modules_have_dotted_paths() {
        assert_eq!(dotted("billing/rates.py").as_deref(), Some("billing.rates"));
        assert_eq!(
            dotted("src/billing/__init__.py").as_deref(),
            Some("billing")
        );
    }

    #[test]
    fn a_rename_is_an_alias() {
        assert_eq!(aliases("pub use inner::price as cost;", "price"), ["cost"]);
        assert_eq!(
            aliases("    price as cost, other as r#type,", "other"),
            ["type"]
        );
        assert_eq!(aliases("extern crate price as cost;", "price"), ["cost"]);
        for (line, word) in [
            ("let x = price as u8;", "price"),
            ("<Shop as Display>::fmt(&shop, f)", "Shop"),
            ("use inner::prices as cost;", "price"),
            ("use inner::price as _;", "price"),
            ("use inner::price;", "price"),
        ] {
            assert!(aliases(line, word).is_empty(), "{line}");
        }
    }

    fn git_repo(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::TempDir::new().unwrap();
        for (rel, content) in files {
            let path = dir.path().join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
        let run_git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.invalid",
                ])
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q"]);
        run_git(&["add", "-A"]);
        run_git(&["commit", "-qm", "init"]);
        let workspace = Workspace::discover(Some(dir.path())).unwrap();
        (dir, workspace)
    }

    fn plan_for(workspace: &Workspace, command: Option<&[&str]>, no_fallback: bool) -> Plan {
        let index = Index::open(workspace).unwrap();
        let owned: Option<Vec<String>> =
            command.map(|words| words.iter().map(|w| (*w).to_owned()).collect());
        let runners = crate::runners::detect(workspace, &index, owned.as_deref()).unwrap();
        plan(
            workspace,
            &index,
            runners,
            &TestSettings::default(),
            no_fallback,
            None,
        )
        .unwrap()
    }

    #[test]
    fn cargo_nextest_selects_subset_skips_doctests_and_widens_on_nextest_config() {
        let (dir, workspace) = git_repo(&[
            (
                "Cargo.toml",
                "[package]\nname = \"demo-pkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            (
                "src/lib.rs",
                concat!(
                    "/// Adds tax.\n",
                    "///\n",
                    "/// ```\n",
                    "/// assert_eq!(demo_pkg::vat(100), 20);\n",
                    "/// ```\n",
                    "pub fn vat(cents: u64) -> u64 { cents / 5 }\n",
                    "pub fn fee(cents: u64) -> u64 { cents + 1 }\n",
                    "#[cfg(test)]\nmod tests {\n",
                    "    #[test] fn vat_works() { assert_eq!(super::vat(100), 20); }\n",
                    "    #[test] fn fee_works() { assert_eq!(super::fee(10), 11); }\n",
                    "    #[test] fn extra_one() { assert_eq!(1, 1); }\n",
                    "    #[test] fn extra_two() { assert_eq!(2, 2); }\n",
                    "}\n",
                ),
            ),
            (".config/nextest.toml", "[profile.default]\nretries = 0\n"),
        ]);

        // Edit vat(): nextest selects ONLY vat_works with nextest binary ID `demo-pkg`,
        // and NEVER emits a `--doc` step (since nextest does not run doctests).
        let lib = dir.path().join("src/lib.rs");
        let text = fs::read_to_string(&lib).unwrap();
        fs::write(&lib, text.replace("cents / 5", "cents * 20 / 100")).unwrap();

        let p = plan_for(
            &workspace,
            Some(&["cargo", "nextest", "run", "--offline"]),
            false,
        );
        let r = &p.runners[0];
        assert_eq!(r.runner, Kind::Nextest);
        assert_eq!(r.mode, Mode::Subset);
        assert_eq!(r.steps.len(), 1);
        assert_eq!(r.steps[0].kind, StepKind::Filtered);
        assert!(
            !r.steps.iter().any(|s| s.kind == StepKind::Doctests),
            "nextest must never select doctests"
        );
        assert_eq!(
            r.steps[0].argv.join(" "),
            "cargo nextest run --offline -p demo-pkg --lib -- tests::vat_works --exact"
        );
        assert_eq!(
            r.steps[0].names.get("demo-pkg").map(Vec::as_slice),
            Some(&["tests::vat_works".to_owned()][..])
        );

        // Editing `.config/nextest.toml` forces full suite for Nextest.
        fs::write(&lib, text).unwrap();
        fs::write(
            dir.path().join(".config/nextest.toml"),
            "[profile.default]\nretries = 1\n",
        )
        .unwrap();
        let cfg = plan_for(
            &workspace,
            Some(&["cargo", "nextest", "run", "--offline"]),
            false,
        );
        assert_eq!(cfg.runners[0].mode, Mode::Full);
        assert_eq!(cfg.runners[0].reasons[0].code, "test-config");
    }

    #[test]
    fn bun_node_playwright_subsets_and_js_conservative_fallbacks_hold() {
        let (dir, workspace) = git_repo(&[
            ("package.json", r#"{"scripts":{"test":"bun test"}}"#),
            ("bunfig.toml", "[test]\npreload = [\"./src/preload.ts\"]\n"),
            ("src/preload.ts", "export const loaded = true;\n"),
            (
                "src/tax.ts",
                "export function vat(n: number) { return n / 5; }\n",
            ),
            (
                "src/index.ts",
                "export { vat as computeVat } from './tax';\n",
            ),
            ("src/other.ts", "export function noop() { return 1; }\n"),
            ("src/a.ts", "export function alpha() { return 1; }\n"),
            (
                "src/tax.test.ts",
                "import { computeVat } from './index';\ntest('vat', () => { computeVat(10); });\n",
            ),
            (
                "src/other.test.ts",
                "import { noop } from './other';\ntest('noop', () => { noop(); });\n",
            ),
            (
                "src/a.test.ts",
                "import { alpha } from './a';\ntest('a', () => { alpha(); });\n",
            ),
            (
                "test/a.test.js",
                "import { alpha } from '../src/a.ts';\ntest('a', () => alpha());\n",
            ),
            ("test/b.test.js", "test('b', () => {});\n"),
            ("test/c.test.js", "test('c', () => {});\n"),
            (
                "src/e2e.spec.ts",
                "import { alpha } from './a';\ntest('e2e', () => alpha());\n",
            ),
            ("src/e2e2.spec.ts", "test('e2e2', () => {});\n"),
            ("src/e2e3.spec.ts", "test('e2e3', () => {});\n"),
        ]);

        // 1. Barrel re-export: editing `src/tax.ts` selects `src/tax.test.ts` via `src/index.ts` under Bun!
        let tax = dir.path().join("src/tax.ts");
        let orig_tax = fs::read_to_string(&tax).unwrap();
        fs::write(&tax, orig_tax.replace("n / 5", "n * 0.2")).unwrap();
        let p = plan_for(&workspace, Some(&["bun", "test"]), false);
        assert_eq!(p.runners[0].runner, Kind::Bun);
        assert_eq!(p.runners[0].mode, Mode::Subset);
        assert_eq!(
            p.runners[0].steps[0].argv,
            ["bun", "test", "src/tax.test.ts"]
        );
        fs::write(&tax, orig_tax).unwrap();

        // 2. Node --test and Playwright positive subsets!
        let a_file = dir.path().join("src/a.ts");
        let orig_a = fs::read_to_string(&a_file).unwrap();
        fs::write(&a_file, orig_a.replace("return 1", "return 2")).unwrap();

        let node_p = plan_for(&workspace, Some(&["node", "--test"]), false);
        assert_eq!(node_p.runners[0].runner, Kind::NodeTest);
        assert_eq!(node_p.runners[0].mode, Mode::Subset);
        assert!(
            node_p.runners[0].steps[0]
                .argv
                .contains(&"test/a.test.js".to_owned()),
            "{:?}",
            node_p.runners[0].steps[0].argv
        );
        assert!(
            !node_p.runners[0].steps[0]
                .argv
                .contains(&"src/e2e.spec.ts".to_owned()),
            "node --test must not collect .spec.ts files"
        );

        let pw_p = plan_for(&workspace, Some(&["playwright", "test"]), false);
        assert_eq!(pw_p.runners[0].runner, Kind::Playwright);
        assert_eq!(pw_p.runners[0].mode, Mode::Subset);
        assert!(
            pw_p.runners[0].steps[0]
                .argv
                .contains(&"src/e2e.spec.ts".to_owned()),
            "{:?}",
            pw_p.runners[0].steps[0].argv
        );
        fs::write(&a_file, orig_a).unwrap();

        // 3. Config-referenced preload file triggers `test-config` full-suite fallback!
        let preload = dir.path().join("src/preload.ts");
        let orig_preload = fs::read_to_string(&preload).unwrap();
        fs::write(&preload, "export const loaded = false;\n").unwrap();
        let preload_p = plan_for(&workspace, Some(&["bun", "test"]), false);
        assert_eq!(preload_p.runners[0].mode, Mode::Full);
        assert_eq!(preload_p.runners[0].reasons[0].code, "test-config");
        fs::write(&preload, orig_preload).unwrap();

        // 4. Computed dynamic import triggers `dynamic-import` full-suite fallback!
        fs::write(
            dir.path().join("src/loader.ts"),
            "export const load = (x: string) => import(`./${x}`);\n",
        )
        .unwrap();
        fs::write(&a_file, "export function alpha() { return 3; }\n").unwrap();
        let dyn_p = plan_for(&workspace, Some(&["bun", "test"]), false);
        assert_eq!(dyn_p.runners[0].mode, Mode::Full);
        assert!(
            dyn_p.runners[0]
                .reasons
                .iter()
                .any(|r| r.code == "dynamic-import"),
            "{:?}",
            dyn_p.runners[0]
                .reasons
                .iter()
                .map(|r| (r.code, &r.message))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn rspec_selects_subset_and_falls_back_conservatively_on_helpers_and_dynamic_loaders() {
        let (dir, workspace) = git_repo(&[
            ("Gemfile", "gem 'rspec'\n"),
            (".rspec", "--format documentation\n"),
            ("spec/spec_helper.rb", "RSpec.configure { |_c| }\n"),
            (
                "lib/order.rb",
                "class Order\n  def total(x)\n    x + 1\n  end\nend\n",
            ),
            (
                "lib/other.rb",
                "class Other\n  def run\n    1\n  end\nend\n",
            ),
            (
                "spec/order_spec.rb",
                "require_relative '../lib/order'\nRSpec.describe Order do\n  it 'adds' do\n    expect(Order.new.total(1)).to eq(2)\n  end\nend\n",
            ),
            (
                "spec/other_spec.rb",
                "require_relative '../lib/other'\nRSpec.describe Other do\n  it 'runs' do\n    expect(Other.new.run).to eq(1)\n  end\nend\n",
            ),
            (
                "spec/third_spec.rb",
                "RSpec.describe 'Third' do\n  it 'ok' do\n    expect(1).to eq(1)\n  end\nend\n",
            ),
        ]);

        // Positive subset via require_relative + auto-detected `bundle exec rspec`.
        let order = dir.path().join("lib/order.rb");
        let orig = fs::read_to_string(&order).unwrap();
        fs::write(&order, orig.replace("x + 1", "x + 2")).unwrap();
        let p = plan_for(&workspace, None, false);
        assert_eq!(p.runners[0].runner, Kind::Rspec);
        assert_eq!(p.runners[0].mode, Mode::Subset);
        assert_eq!(
            p.runners[0].steps[0].argv,
            ["bundle", "exec", "rspec", "spec/order_spec.rb"]
        );

        // Editing spec_helper.rb triggers `test-config` full-suite fallback.
        fs::write(&order, &orig).unwrap();
        let helper = dir.path().join("spec/spec_helper.rb");
        let orig_helper = fs::read_to_string(&helper).unwrap();
        fs::write(&helper, "RSpec.configure { |c| c.order = :random }\n").unwrap();
        let helper_p = plan_for(&workspace, None, false);
        assert_eq!(helper_p.runners[0].mode, Mode::Full);
        assert_eq!(helper_p.runners[0].reasons[0].code, "test-config");

        // Dynamic `Dir.glob` require forces full-suite fallback on non-spec `.rb` edits.
        fs::write(
            &helper,
            "Dir.glob('spec/support/**/*.rb').each { |f| require f }\n",
        )
        .unwrap();
        // Commit helper so only `lib/order.rb` is dirty.
        assert!(
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "-qam",
                    "loader"
                ])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        fs::write(&order, orig.replace("x + 1", "x + 3")).unwrap();
        let dyn_p = plan_for(&workspace, None, false);
        assert_eq!(dyn_p.runners[0].mode, Mode::Full);
        assert!(
            dyn_p.runners[0]
                .reasons
                .iter()
                .any(|r| r.code == "dynamic-import"),
            "{:?}",
            dyn_p.runners[0]
                .reasons
                .iter()
                .map(|r| r.code)
                .collect::<Vec<_>>()
        );
        let _ = orig_helper;
    }

    #[test]
    fn dart_and_flutter_select_convention_and_graph_affected_tests() {
        // 1. Pure Dart package with barrel re-export (`lib/demo.dart`), `package:` import,
        //    and convention `lib/tax.dart` -> `test/tax_test.dart`.
        let (dir, workspace) = git_repo(&[
            ("pubspec.yaml", "name: demo_dart\nversion: 1.0.0\n"),
            ("lib/demo.dart", "export 'src/calc.dart';\n"),
            (
                "lib/src/calc.dart",
                "int vat(int cents) {\n  return cents ~/ 5;\n}\n",
            ),
            ("lib/other.dart", "int noop() {\n  return 1;\n}\n"),
            (
                "test/calc_test.dart",
                "import 'package:demo_dart/demo.dart';\nvoid main() {\n  test('vat', () {\n    vat(100);\n  });\n}\n",
            ),
            (
                "test/summary_test.dart",
                "import 'package:demo_dart/src/calc.dart';\nvoid main() {\n  test('summary', () {\n    vat(200);\n  });\n}\n",
            ),
            (
                "test/other_test.dart",
                "import 'package:demo_dart/other.dart';\nvoid main() {\n  test('other', () {\n    noop();\n  });\n}\n",
            ),
            (
                "test/third_test.dart",
                "void main() {\n  test('third', () {});\n}\n",
            ),
            (
                "test/fourth_test.dart",
                "void main() {\n  test('fourth', () {});\n}\n",
            ),
        ]);

        let calc = dir.path().join("lib/src/calc.dart");
        let orig = fs::read_to_string(&calc).unwrap();
        fs::write(&calc, orig.replace("cents ~/ 5", "cents * 20 ~/ 100")).unwrap();

        let dart_plan = plan_for(&workspace, None, false);
        let r = &dart_plan.runners[0];
        assert_eq!(r.runner, Kind::DartTest);
        assert_eq!(r.mode, Mode::Subset);
        assert_eq!(
            r.steps[0].argv,
            [
                "dart",
                "test",
                "test/calc_test.dart",
                "test/summary_test.dart"
            ]
        );

        // 2. Flutter package (`sdk: flutter`) auto-detects `flutter test` and selects
        //    the affected widget test file.
        let (fdir, fws) = git_repo(&[
            (
                "pubspec.yaml",
                "name: demo_flutter\ndependencies:\n  flutter:\n    sdk: flutter\n",
            ),
            (
                "lib/counter.dart",
                "int increment(int n) {\n  return n + 1;\n}\n",
            ),
            (
                "test/counter_test.dart",
                "import 'package:demo_flutter/counter.dart';\nvoid main() {\n  testWidgets('increments', () {\n    increment(1);\n  });\n}\n",
            ),
            (
                "test/a_test.dart",
                "void main() {\n  test('a', () {});\n}\n",
            ),
            (
                "test/b_test.dart",
                "void main() {\n  test('b', () {});\n}\n",
            ),
        ]);
        let counter = fdir.path().join("lib/counter.dart");
        let orig_counter = fs::read_to_string(&counter).unwrap();
        fs::write(&counter, orig_counter.replace("n + 1", "n + 2")).unwrap();

        let flutter_plan = plan_for(&fws, None, false);
        let fr = &flutter_plan.runners[0];
        assert_eq!(fr.runner, Kind::DartTest);
        assert_eq!(fr.mode, Mode::Subset);
        assert_eq!(
            fr.steps[0].argv,
            ["flutter", "test", "test/counter_test.dart"]
        );
    }

    #[test]
    fn deno_selects_convention_and_graph_affected_tests_and_preserves_script_args() {
        let (dir, workspace) = git_repo(&[
            (
                "deno.json",
                "{\n  \"tasks\": { \"test\": \"deno test\" }\n}\n",
            ),
            (
                "src/calc.ts",
                "export function compute(cents: number): number {\n  return cents * 0.2;\n}\n",
            ),
            (
                "src/other.ts",
                "export function compute(x: number): number {\n  return x + 1;\n}\nexport function otherFn(x: number): number {\n  return x * 2;\n}\n",
            ),
            (
                "src/calc_test.ts",
                "import * as calc from './calc.ts';\nDeno.test('vat', () => {\n  calc.compute(100);\n});\n",
            ),
            (
                "src/report.test.ts",
                "import { compute } from './calc.ts';\nDeno.test('report', () => {\n  compute(50);\n});\n",
            ),
            (
                "src/other_test.ts",
                "import { otherFn } from './other.ts';\nDeno.test('other', () => {\n  otherFn(1);\n});\n",
            ),
            ("src/third_test.ts", "Deno.test('third', () => {});\n"),
            ("src/fourth_test.ts", "Deno.test('fourth', () => {});\n"),
        ]);

        let calc = dir.path().join("src/calc.ts");
        let orig = fs::read_to_string(&calc).unwrap();
        fs::write(&calc, orig.replace("cents * 0.2", "cents / 5")).unwrap();

        // Auto-detected `deno test` selects both namespace-imported `src/calc_test.ts`
        // and graph-connected `src/report.test.ts`, skipping unrelated tests.
        let auto_plan = plan_for(&workspace, None, false);
        let r = &auto_plan.runners[0];
        assert_eq!(r.runner, Kind::DenoTest);
        assert_eq!(
            r.mode,
            Mode::Subset,
            "reasons: {:?}, tests: {:?}",
            r.reasons
                .iter()
                .map(|reason| (reason.code, &reason.message))
                .collect::<Vec<_>>(),
            r.tests.iter().map(|t| &t.id).collect::<Vec<_>>()
        );
        assert_eq!(
            r.steps[0].argv,
            ["deno", "test", "src/calc_test.ts", "src/report.test.ts"]
        );

        // Trailing `-- <script args>` stay after the selected test file arguments.
        let with_args = plan_for(
            &workspace,
            Some(&["deno", "test", "--allow-env", "--", "--port=8080"]),
            false,
        );
        assert_eq!(with_args.runners[0].mode, Mode::Subset);
        assert_eq!(
            with_args.runners[0].steps[0].argv,
            [
                "deno",
                "test",
                "--allow-env",
                "src/calc_test.ts",
                "src/report.test.ts",
                "--",
                "--port=8080"
            ]
        );
    }

    #[test]
    fn dart_and_deno_config_and_harness_changes_fall_back_to_full_suite() {
        // Dart/Flutter config & harness fallbacks.
        let (ddir, dws) = git_repo(&[
            ("pubspec.yaml", "name: cfg_dart\nversion: 1.0.0\n"),
            ("pubspec.lock", "packages: {}\n"),
            ("build.yaml", "targets:\n  $default: {}\n"),
            ("analysis_options.yaml", "linter:\n  rules: []\n"),
            (
                "test/flutter_test_config.dart",
                "Future<void> testExecutable(Future<void> Function() main) => main();\n",
            ),
            ("lib/calc.dart", "int calc(int x) => x + 1;\n"),
            (
                "test/calc_test.dart",
                "import 'package:cfg_dart/calc.dart';\nvoid main() { test('c', () => calc(1)); }\n",
            ),
            ("test/b_test.dart", "void main() { test('b', () {}); }\n"),
            ("test/c_test.dart", "void main() { test('c', () {}); }\n"),
        ]);
        for (rel, updated, expected_code) in [
            (
                "pubspec.yaml",
                "name: cfg_dart\nversion: 1.0.1\n",
                "workspace-config",
            ),
            ("pubspec.lock", "packages:\n  demo: {}\n", "lockfile"),
            (
                "build.yaml",
                "targets:\n  $default:\n    builders: {}\n",
                "workspace-config",
            ),
            (
                "analysis_options.yaml",
                "linter:\n  rules:\n    - avoid_print\n",
                "workspace-config",
            ),
            (
                "test/flutter_test_config.dart",
                "Future<void> testExecutable(Future<void> Function() main) async { await main(); }\n",
                "test-config",
            ),
        ] {
            let p = ddir.path().join(rel);
            let orig = fs::read_to_string(&p).unwrap();
            fs::write(&p, updated).unwrap();
            let planned = plan_for(&dws, None, false);
            assert_eq!(planned.runners[0].mode, Mode::Full, "{rel} must fall back");
            assert!(
                planned.runners[0]
                    .reasons
                    .iter()
                    .any(|reason| reason.code == expected_code && reason.message.contains(rel)),
                "{rel}: {:?}",
                planned.runners[0]
                    .reasons
                    .iter()
                    .map(|r| (r.code, &r.message))
                    .collect::<Vec<_>>()
            );
            fs::write(&p, orig).unwrap();
        }

        // Deno config & harness fallbacks.
        let (ndir, nws) = git_repo(&[
            (
                "deno.json",
                "{\n  \"compilerOptions\": { \"strict\": true }\n}\n",
            ),
            ("deno.jsonc", "// config\n{}\n"),
            ("deno.lock", "{\n  \"version\": \"4\"\n}\n"),
            ("import_map.json", "{\n  \"imports\": {}\n}\n"),
            ("test_setup.ts", "export const ready = true;\n"),
            (
                "src/calc.ts",
                "export function calc(x: number) { return x + 1; }\n",
            ),
            (
                "src/calc_test.ts",
                "import { calc } from './calc.ts';\nDeno.test('c', () => { calc(1); });\n",
            ),
            ("src/b_test.ts", "Deno.test('b', () => {});\n"),
            ("src/c_test.ts", "Deno.test('c', () => {});\n"),
        ]);
        for (rel, updated, expected_code) in [
            (
                "deno.json",
                "{\n  \"compilerOptions\": { \"strict\": false }\n}\n",
                "workspace-config",
            ),
            ("deno.jsonc", "// updated\n{}\n", "workspace-config"),
            ("deno.lock", "{\n  \"version\": \"5\"\n}\n", "lockfile"),
            (
                "import_map.json",
                "{\n  \"imports\": { \"@app/\": \"./src/\" }\n}\n",
                "workspace-config",
            ),
            (
                "test_setup.ts",
                "export const ready = false;\n",
                "test-config",
            ),
        ] {
            let p = ndir.path().join(rel);
            let orig = fs::read_to_string(&p).unwrap();
            fs::write(&p, updated).unwrap();
            let planned = plan_for(&nws, Some(&["deno", "test"]), false);
            assert_eq!(planned.runners[0].mode, Mode::Full, "{rel} must fall back");
            assert!(
                planned.runners[0]
                    .reasons
                    .iter()
                    .any(|reason| reason.code == expected_code && reason.message.contains(rel)),
                "{rel}: {:?}",
                planned.runners[0]
                    .reasons
                    .iter()
                    .map(|r| (r.code, &r.message))
                    .collect::<Vec<_>>()
            );
            fs::write(&p, orig).unwrap();
        }
    }

    #[test]
    fn dart_and_deno_explicit_target_and_filter_args_preserve_base_command() {
        let (dir, workspace) = git_repo(&[
            ("pubspec.yaml", "name: keep_args\nversion: 1.0.0\n"),
            ("deno.json", "{}\n"),
            ("lib/calc.dart", "int calc(int x) => x + 1;\n"),
            (
                "test/calc_test.dart",
                "import 'package:keep_args/calc.dart';\nvoid main() { test('c', () => calc(1)); }\n",
            ),
            (
                "test/other_test.dart",
                "void main() { test('o', () {}); }\n",
            ),
            (
                "src/calc.ts",
                "export function calc(x: number) { return x + 1; }\n",
            ),
            (
                "src/calc_test.ts",
                "import { calc } from './calc.ts';\nDeno.test('c', () => { calc(1); });\n",
            ),
            ("src/other_test.ts", "Deno.test('o', () => {});\n"),
        ]);
        fs::write(
            dir.path().join("lib/calc.dart"),
            "int calc(int x) => x + 2;\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("src/calc.ts"),
            "export function calc(x: number) { return x + 2; }\n",
        )
        .unwrap();

        for (cmd, expected_flag) in [
            (
                &["dart", "test", "test/other_test.dart"][..],
                "test/other_test.dart",
            ),
            (&["flutter", "test", "--name", "widget"][..], "--name"),
            (&["flutter", "test", "-t", "smoke"][..], "-t"),
            (
                &["deno", "test", "src/other_test.ts"][..],
                "src/other_test.ts",
            ),
            (&["deno", "test", "--filter", "vat"][..], "--filter"),
            (
                &["deno", "test", "--ignore=src/slow_test.ts"][..],
                "--ignore=src/slow_test.ts",
            ),
        ] {
            let planned = plan_for(&workspace, Some(cmd), false);
            let r = &planned.runners[0];
            assert_eq!(r.mode, Mode::Command, "{cmd:?} must keep command as given");
            assert_eq!(r.reasons[0].code, "command-selects");
            assert!(
                r.reasons[0].message.contains(expected_flag),
                "{cmd:?}: {}",
                r.reasons[0].message
            );
            assert_eq!(r.steps[0].argv, cmd);
        }

        // Verify Dart & Deno tallies parse passes, failures, zero-ran, and load errors.
        let deno_fail = crate::runners::tally(
            Kind::DenoTest,
            "ERRORS\nvat fails => ./src/calc_test.ts:2:6\n\nFAILED | 2 passed | 1 failed | 1 ignored (15ms)\n",
            "",
        );
        assert!(deno_fail.known);
        assert_eq!((deno_fail.ran, deno_fail.ignored), (3, 1));
        assert_eq!(deno_fail.failed, ["vat fails"]);
        assert!(!deno_fail.no_tests_matched);
        let deno_zero = crate::runners::tally(Kind::DenoTest, "", "error: No test modules found\n");
        assert!(deno_zero.no_tests_matched);

        let dart_fail = crate::runners::tally(
            Kind::DartTest,
            "00:01 +2 ~1 -1: test/calc_test.dart: computes tax [E]\n00:01 +2 ~1 -1: Some tests failed.\n",
            "",
        );
        assert!(dart_fail.known);
        assert_eq!((dart_fail.ran, dart_fail.ignored), (3, 1));
        assert_eq!(dart_fail.failed, ["test/calc_test.dart: computes tax"]);
        let dart_load_err = crate::runners::tally(
            Kind::DartTest,
            "00:00 +0 -1: loading test/broken_test.dart [E]\n00:00 +0 -1: Some tests failed.\n",
            "",
        );
        assert_eq!(dart_load_err.errors, 1);
        assert!(!dart_load_err.no_tests_matched);
        let dart_cr = crate::runners::tally(
            Kind::DartTest,
            "00:00 +1: test/calc_test.dart\r00:01 +2: All tests passed!\r",
            "",
        );
        assert!(dart_cr.known);
        assert_eq!(dart_cr.ran, 2);
        assert!(dart_cr.failed.is_empty());
    }

    #[test]
    fn dart_part_mirrors_support_and_custom_discovery_configs_behave_safely() {
        let git = |cwd: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.invalid",
                ])
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        // 1. Dart `part` / `part of` compilation units link edits in part files to tests;
        //    benign `dart_test.yaml` keeps subsetting; custom `paths:`/`filename:` falls back;
        //    `dart:mirrors` and `test/support/*.dart` fall back conservatively.
        let (dir, workspace) = git_repo(&[
            ("pubspec.yaml", "name: part_demo\nversion: 1.0.0\n"),
            ("dart_test.yaml", "concurrency: 4\n"),
            ("lib/calc.dart", "library calc;\npart 'calc_part.dart';\n"),
            (
                "lib/calc_part.dart",
                "part of 'calc.dart';\nint vat(int c) => c ~/ 5;\n",
            ),
            ("test/support/helpers.dart", "int helper() => 1;\n"),
            (
                "test/calc_test.dart",
                "import 'package:part_demo/calc.dart';\nvoid main() { test('v', () => vat(10)); }\n",
            ),
            ("test/b_test.dart", "void main() { test('b', () {}); }\n"),
            ("test/c_test.dart", "void main() { test('c', () {}); }\n"),
        ]);
        let part_path = dir.path().join("lib/calc_part.dart");
        let orig_part = fs::read_to_string(&part_path).unwrap();
        fs::write(&part_path, orig_part.replace("c ~/ 5", "c ~/ 4")).unwrap();

        let planned = plan_for(&workspace, None, false);
        assert_eq!(planned.runners[0].mode, Mode::Subset);
        assert_eq!(
            planned.runners[0].steps[0].argv,
            ["dart", "test", "test/calc_test.dart"]
        );

        // Custom `paths:` in `dart_test.yaml` forces full suite even for a normal lib edit.
        let cfg_path = dir.path().join("dart_test.yaml");
        fs::write(&cfg_path, "paths:\n  - test/unit\n").unwrap();
        git(dir.path(), &["add", "dart_test.yaml"]);
        git(dir.path(), &["commit", "-m", "custom dart paths"]);
        let custom_dart = plan_for(&workspace, None, false);
        assert_eq!(custom_dart.runners[0].mode, Mode::Full);
        assert!(
            custom_dart.runners[0]
                .reasons
                .iter()
                .any(|r| r.code == "test-config" && r.message.contains("dart_test.yaml"))
        );

        // Revert custom paths, test `dart:mirrors` guard and `test/support/*.dart` fallback.
        fs::write(&cfg_path, "concurrency: 4\n").unwrap();
        fs::write(
            dir.path().join("lib/reflect.dart"),
            "import 'dart:mirrors';\nvoid probe() {}\n",
        )
        .unwrap();
        git(dir.path(), &["add", "dart_test.yaml", "lib/reflect.dart"]);
        git(dir.path(), &["commit", "-m", "add mirrors"]);
        let mirrors_plan = plan_for(&workspace, None, false);
        assert_eq!(mirrors_plan.runners[0].mode, Mode::Full);
        assert!(
            mirrors_plan.runners[0]
                .reasons
                .iter()
                .any(|r| r.code == "dynamic-import" && r.message.contains("dart:mirrors"))
        );

        fs::write(&part_path, &orig_part).unwrap();
        fs::remove_file(dir.path().join("lib/reflect.dart")).unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-m", "remove mirrors"]);
        fs::write(
            dir.path().join("test/support/helpers.dart"),
            "int helper() => 2;\n",
        )
        .unwrap();
        let support_plan = plan_for(&workspace, None, false);
        assert_eq!(support_plan.runners[0].mode, Mode::Full);
        assert!(
            support_plan.runners[0]
                .reasons
                .iter()
                .any(|r| r.code == "test-config" && r.message.contains("test/support/helpers.dart"))
        );

        // 2. Deno multi-line `preload` config reference & custom `test.exclude` guard.
        let (ndir, nws) = git_repo(&[
            (
                "deno.jsonc",
                "{\n  // preload helper\n  \"preload\": [\n    \"./helpers/boot.ts\",\n  ],\n}\n",
            ),
            ("helpers/boot.ts", "export const booted = true;\n"),
            (
                "src/calc.ts",
                "export function calc(x: number) { return x + 1; }\n",
            ),
            (
                "src/calc_test.ts",
                "import { calc } from './calc.ts';\nDeno.test('c', () => { calc(1); });\n",
            ),
            ("src/b_test.ts", "Deno.test('b', () => {});\n"),
            ("src/c_test.ts", "Deno.test('c', () => {});\n"),
        ]);
        let boot = ndir.path().join("helpers/boot.ts");
        let orig_boot = fs::read_to_string(&boot).unwrap();
        fs::write(&boot, "export const booted = false;\n").unwrap();
        let preload_plan = plan_for(&nws, Some(&["deno", "test"]), false);
        assert_eq!(preload_plan.runners[0].mode, Mode::Full);
        assert!(
            preload_plan.runners[0]
                .reasons
                .iter()
                .any(|r| r.code == "test-config" && r.message.contains("deno.jsonc"))
        );
        fs::write(&boot, orig_boot).unwrap();

        fs::write(
            ndir.path().join("deno.jsonc"),
            "{\n  \"test\": {\n    \"exclude\": [\"src/b_test.ts\"],\n  },\n}\n",
        )
        .unwrap();
        git(ndir.path(), &["add", "deno.jsonc"]);
        git(ndir.path(), &["commit", "-m", "deno test exclude"]);
        fs::write(
            ndir.path().join("src/calc.ts"),
            "export function calc(x: number) { return x + 2; }\n",
        )
        .unwrap();
        let deno_excl = plan_for(&nws, Some(&["deno", "test"]), false);
        assert_eq!(deno_excl.runners[0].mode, Mode::Full);
        assert!(
            deno_excl.runners[0]
                .reasons
                .iter()
                .any(|r| r.code == "test-config" && r.message.contains("deno.jsonc"))
        );

        // 3. Regression guard: `test/support/*.rb` must still trigger fallback for Ruby RSpec.
        let (rdir, rws) = git_repo(&[
            (".rspec", "--format documentation\n"),
            ("test/support/helper.rb", "module Helper; end\n"),
            ("lib/calc.rb", "class Calc; def add(a); a + 1; end; end\n"),
            (
                "spec/calc_spec.rb",
                "require_relative '../lib/calc'\nRSpec.describe Calc do\n  it 'adds' do\n    expect(Calc.new.add(1)).to eq(2)\n  end\nend\n",
            ),
            ("spec/b_spec.rb", "RSpec.describe 'B' do; it('b') {}; end\n"),
            ("spec/c_spec.rb", "RSpec.describe 'C' do; it('c') {}; end\n"),
        ]);
        fs::write(
            rdir.path().join("test/support/helper.rb"),
            "module Helper; def x; 1; end; end\n",
        )
        .unwrap();
        let rb_plan = plan_for(&rws, None, false);
        assert_eq!(rb_plan.runners[0].mode, Mode::Full);
        assert!(
            rb_plan.runners[0]
                .reasons
                .iter()
                .any(|r| r.code == "test-config" && r.message.contains("test/support/helper.rb"))
        );
    }
}
