//! The closed table of command mistakes `learn` recognizes, how a failure pairs with
//! its correction, and the checks that confirm a rule against the current tree.
//!
//! Every signature matches a line captured from real tool output or quoted from the
//! tool's source, and tests/learn.rs holds a fixture with each exact line. Provenance:
//! [here] marks a line captured on a development machine, [source] one quoted from the
//! tool's source or exit-code enum. Recalled strings that could not be confirmed are
//! left out on purpose: a wrong signature produces confident wrong rules.
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use super::events::{self, Cmd, Event, Pointer, Runner, Status, basename};
use super::privacy;

/// A correction must follow within this many events of the thread and this long.
const WINDOW_EVENTS: usize = 6;
const WINDOW_MS: u64 = 15 * 60 * 1000;
const MANAGERS: [&str; 3] = ["npm", "pnpm", "yarn"];
// GNU make reads GNUmakefile, makefile, then Makefile. Makefile comes before makefile
// here so a case-insensitive file system reports the usual spelling.
const MAKEFILES: [&str; 3] = ["GNUmakefile", "Makefile", "makefile"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    MissingProgram,
    ZshGlob,
    ZshEquals,
    WrongDirectory,
    UnknownTarget,
    UnknownOption,
    MissingOption,
    WrongTestPath,
    EmptyTests,
    WrongPackageManager,
}

impl Class {
    const ALL: [Self; 10] = [
        Self::MissingProgram,
        Self::ZshGlob,
        Self::ZshEquals,
        Self::WrongDirectory,
        Self::UnknownTarget,
        Self::UnknownOption,
        Self::MissingOption,
        Self::WrongTestPath,
        Self::EmptyTests,
        Self::WrongPackageManager,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::MissingProgram => "missing_program",
            Self::ZshGlob => "zsh_glob",
            Self::ZshEquals => "zsh_equals",
            Self::WrongDirectory => "wrong_directory",
            Self::UnknownTarget => "unknown_target",
            Self::UnknownOption => "unknown_option",
            Self::MissingOption => "missing_option",
            Self::WrongTestPath => "wrong_test_path",
            Self::EmptyTests => "empty_tests",
            Self::WrongPackageManager => "wrong_package_manager",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::MissingProgram => "missing program",
            Self::ZshGlob => "zsh glob",
            Self::ZshEquals => "zsh equals",
            Self::WrongDirectory => "wrong directory",
            Self::UnknownTarget => "unknown target",
            Self::UnknownOption => "unknown option",
            Self::MissingOption => "missing option",
            Self::WrongTestPath => "wrong test path",
            Self::EmptyTests => "empty test run",
            Self::WrongPackageManager => "wrong package manager",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.name() == name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Project,
    Machine,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Machine => "machine",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    Verified(String),
    Unverified,
    Stale(String),
}

/// Programs, zsh and the environment as `learn` sees them, without running anything.
pub struct Env {
    /// `None` where executables cannot be checked, as on Windows.
    pub path: Option<Vec<PathBuf>>,
    pub shell: Option<String>,
}

impl Env {
    pub fn current() -> Self {
        let path = (!cfg!(windows)).then(|| {
            std::env::var_os("PATH")
                .map(|path| std::env::split_paths(&path).collect())
                .unwrap_or_default()
        });
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|shell| !shell.is_empty());
        Self { path, shell }
    }

    fn executable(&self, program: &str) -> Option<bool> {
        let path = self.path.as_ref()?;
        Some(!program.contains('/') && path.iter().any(|dir| is_executable(&dir.join(program))))
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

// SELF-EVIDENT classes: an error line in any accepted result is evidence by itself,
// when its operand appears in the command that produced it.

const ZSH: &str = r"(?:\(eval\):\d+: |zsh:(?:\d+: )? ?)";
const PROGRAM: &str = r"[A-Za-z0-9._+-][A-Za-z0-9._+/-]{0,63}";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Missing {
    Program,
    Relative,
    Module,
}

impl Missing {
    fn name(self) -> &'static str {
        match self {
            Self::Program => "program",
            Self::Relative => "relative",
            Self::Module => "module",
        }
    }
}

static MISSING: LazyLock<Vec<(Regex, Missing)>> = LazyLock::new(|| {
    [
        // zsh [here]: `zsh:1: command not found: x`, `(eval):1: command not found: x`
        (
            format!(r"(?m)^{ZSH}command not found: ({PROGRAM})\s*$"),
            Missing::Program,
        ),
        // bash and sh [here]: `/bin/sh: x: command not found`, `bash: line 3: x: command not found`
        (
            format!(r"(?m)^(?:[\w./-]+: )?(?:line \d+: )?({PROGRAM}): command not found\s*$"),
            Missing::Program,
        ),
        // dash [here]: `/bin/dash: 1: x: not found`
        (
            format!(r"(?m)^(?:/bin/)?(?:sh|dash): \d+: ({PROGRAM}): not found\s*$"),
            Missing::Program,
        ),
        // zsh relative path [here]: `zsh:1: no such file or directory: ./x`
        (
            format!(r"(?m)^{ZSH}no such file or directory: (\./{PROGRAM})\s*$"),
            Missing::Relative,
        ),
        // justless [source]: main.rs prints `justless: {error:#}` and capture.rs adds
        // the context `run PROGRAM`.
        (
            format!(r"(?m)^justless: run ({PROGRAM}): No such file or directory"),
            Missing::Program,
        ),
        // python [here]: `/usr/bin/python3: No module named x`
        (
            r"(?m)^\S*python[\d.]*: No module named (\S+)$".to_owned(),
            Missing::Module,
        ),
    ]
    .into_iter()
    .map(|(pattern, kind)| (Regex::new(&pattern).expect("missing program pattern"), kind))
    .collect()
});
// zsh [here]: `zsh:1: no matches found: --include=*.zz`, also inside exit-0 lists.
static GLOB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?m)^{ZSH}no matches found: (\S+)\s*$")).expect("glob pattern")
});
// zsh [here]: `echo ===== done` prints `zsh:1: ==== not found`; zsh drops one `=`.
static EQUALS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(r"(?m)^{ZSH}(=\S*) not found\s*$")).expect("equals pattern")
});
static EXAMPLE_FLAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(--[a-z][a-z-]{0,20}=)\*\.([A-Za-z0-9]{1,8})$").expect("example pattern")
});
static EXAMPLE_EXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\*\.([A-Za-z0-9]{1,8})$").expect("example pattern"));

enum SelfHit {
    Missing(Missing, String),
    Glob(String),
    Equals,
}

fn self_evident(event: &Event) -> Vec<SelfHit> {
    let mut hits = Vec::new();
    let text = event.transcript_text();
    if !event.transcript || event.status == Status::Unknown {
        return hits;
    }
    let maybe_missing = text.contains("not found")
        || text.contains("no such file or directory")
        || text.contains("No such file or directory")
        || text.contains("No module named");
    let words: Vec<&str> = event.raw.split_whitespace().collect();
    if maybe_missing {
        for (pattern, kind) in MISSING.iter() {
            for captures in pattern.captures_iter(text) {
                let operand = captures[1].to_owned();
                // A path names a file in the tree, not an installed program.
                let kind = &if *kind == Missing::Program && operand.contains('/') {
                    Missing::Relative
                } else {
                    *kind
                };
                let gated = match kind {
                    Missing::Module => words
                        .windows(2)
                        .any(|pair| pair[0] == "-m" && pair[1] == operand),
                    _ => words.contains(&operand.as_str()) && basename(&operand) != "justless",
                };
                if gated
                    && !hits.iter().any(
                        |hit| matches!(hit, SelfHit::Missing(k, o) if k == kind && *o == operand),
                    )
                {
                    hits.push(SelfHit::Missing(*kind, operand));
                }
            }
        }
    }
    if text.contains("no matches found")
        && let Some(operand) = GLOB
            .captures_iter(text)
            .map(|captures| captures[1].to_owned())
            .find(|operand| event.raw.contains(operand.as_str()))
    {
        hits.push(SelfHit::Glob(operand));
    }
    if text.contains(" not found") {
        let unquoted = unquoted_words(&event.raw);
        if EQUALS
            .captures_iter(text)
            .any(|captures| unquoted.contains(&format!("={}", &captures[1])))
        {
            hits.push(SelfHit::Equals);
        }
    }
    hits
}

/// Whitespace-separated words of shell text that contain no quotes.
fn unquoted_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut quote = None;
    for c in text.chars() {
        match quote {
            Some(active) if c == active => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                quoted = true;
            }
            None if c.is_whitespace() => {
                if !quoted && !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                word.clear();
                quoted = false;
                continue;
            }
            None => {}
        }
        word.push(c);
    }
    if !quoted && !word.is_empty() {
        words.push(word);
    }
    words
}

// CORRECTED classes: a failing command with one of these signatures, then a later
// command in the same thread that differs in exactly the way the class describes.

#[derive(Clone, Debug, PartialEq, Eq)]
enum Hit {
    NoMakefile,
    NoRule(String),
    NoCargoToml,
    NoPackageJson,
    NoPyproject,
    CargoPackage(String),
    CargoTarget(&'static str, String),
    NoSuchCommand(String),
    MissingScript(String),
    UnknownOption(String),
    RequiredOption(String),
    TestPath(String),
    WorkspaceProtocol,
    Corepack(String),
}

impl Hit {
    fn manifest(&self) -> Option<&'static str> {
        match self {
            Self::NoMakefile | Self::NoRule(_) => Some("Makefile"),
            Self::NoCargoToml => Some("Cargo.toml"),
            Self::NoPackageJson => Some("package.json"),
            Self::NoPyproject => Some("pyproject.toml"),
            _ => None,
        }
    }
}

struct Signatures {
    no_makefile: Regex,
    no_rule: Regex,
    no_cargo_toml: Regex,
    npm_enoent: Regex,
    no_pyproject: Regex,
    cargo_package: Regex,
    cargo_target: Regex,
    cargo_command: Regex,
    git_command: Regex,
    missing_script: Regex,
    options: Vec<Regex>,
    git_option: Regex,
    getopt: Regex,
    required: Regex,
    pytest_path: Regex,
    node_path: Regex,
    workspace: Regex,
    corepack: Regex,
}

static SIGNATURES: LazyLock<Signatures> = LazyLock::new(|| {
    let re = |pattern: &str| Regex::new(pattern).expect("signature pattern");
    Signatures {
        // make [here]: `make: *** No targets specified and no makefile found.  Stop.`
        no_makefile: re(r"(?m)No targets specified and no makefile found"),
        // make [here]: macOS make 3.81 opens with a backtick, GNU make 4 with a quote.
        no_rule: re(r"(?m)No rule to make target [`'‘]([^'’`]+)['’]"),
        // cargo [here]
        no_cargo_toml: re(r"(?m)could not find `Cargo\.toml` in"),
        // npm [here], with `package.json` in the same text
        npm_enoent: re(r"(?m)npm (?:error|ERR!) code ENOENT"),
        // uv [here]
        no_pyproject: re(
            r"(?m)No `pyproject\.toml` found in current directory or any parent directory",
        ),
        // cargo [here]
        cargo_package: re(r"(?m)package ID specification `([^`]+)` did not match any packages"),
        cargo_target: re(r"(?m)no (bin|test|example|bench) target named `([^`]+)`"),
        cargo_command: re(r"(?m)no such command: `([^`]+)`"),
        // git [here]
        git_command: re(r"(?m)git: '([^']+)' is not a git command"),
        // npm [here; also in the local npm install's source]
        missing_script: re(r#"(?m)Missing script: "?([^"\s]+)"?"#),
        options: vec![
            // clap 4 [here]
            re(r"(?m)unexpected argument '([^']+)' found"),
            // clap 3 [here, from this repository's own binary]
            re(r"(?m)Found argument '([^']+)' which wasn't expected"),
            // git [here]
            re(r"(?m)unrecognized argument: (\S+)"),
            // BSD and GNU tools [here]
            re(r"(?m)unrecognized option [`']([^']+)'"),
            // argparse and pytest [here]
            re(r"(?m)unrecognized arguments: (\S+)"),
        ],
        // git [here]: the name comes without dashes.
        git_option: re(r"(?m)unknown (option|switch) [`']([^']+)'"),
        // getopt [here]
        getopt: re(r"(?m)(?:invalid option -- '?(.)'?|illegal option -- (.))"),
        // argparse [here]
        required: re(r"(?m)the following arguments are required: (--?[\w-]+)"),
        // pytest [source: resolve_collection_argument and Session.perform_collect in
        // src/_pytest/main.py; exit 4 is ExitCode.USAGE_ERROR]
        pytest_path: re(r"(?m)^ERROR: (?:file or directory not found|not found): (\S+)"),
        // node --test [here, node 24]
        node_path: re(r"(?m)^Could not find '([^']+)'"),
        // npm [here; also in the local npm install's source]
        workspace: re(r#"(?m)Unsupported URL Type "workspace:""#),
        // corepack [source: the message template in the local corepack install]
        corepack: re(r"(?m)This project is configured to use (npm|pnpm|yarn) because"),
    }
});

fn hits(text: &str) -> Vec<Hit> {
    let s = &*SIGNATURES;
    let mut hits = Vec::new();
    let mut add = |hit: Hit| {
        if !hits.contains(&hit) {
            hits.push(hit);
        }
    };
    if s.no_makefile.is_match(text) {
        add(Hit::NoMakefile);
    }
    for captures in s.no_rule.captures_iter(text) {
        add(Hit::NoRule(captures[1].to_owned()));
    }
    if s.no_cargo_toml.is_match(text) {
        add(Hit::NoCargoToml);
    }
    if s.npm_enoent.is_match(text) && text.contains("package.json") {
        add(Hit::NoPackageJson);
    }
    if s.no_pyproject.is_match(text) {
        add(Hit::NoPyproject);
    }
    for captures in s.cargo_package.captures_iter(text) {
        add(Hit::CargoPackage(captures[1].to_owned()));
    }
    for captures in s.cargo_target.captures_iter(text) {
        let kind = match &captures[1] {
            "bin" => "bin",
            "test" => "test",
            "example" => "example",
            _ => "bench",
        };
        add(Hit::CargoTarget(kind, captures[2].to_owned()));
    }
    for captures in s
        .cargo_command
        .captures_iter(text)
        .chain(s.git_command.captures_iter(text))
    {
        add(Hit::NoSuchCommand(captures[1].to_owned()));
    }
    for captures in s.missing_script.captures_iter(text) {
        add(Hit::MissingScript(captures[1].to_owned()));
    }
    for pattern in &s.options {
        for captures in pattern.captures_iter(text) {
            add(Hit::UnknownOption(option_name(&captures[1])));
        }
    }
    for captures in s.git_option.captures_iter(text) {
        let dashes = if &captures[1] == "switch" { "-" } else { "--" };
        add(Hit::UnknownOption(format!("{dashes}{}", &captures[2])));
    }
    for captures in s.getopt.captures_iter(text) {
        let c = captures
            .get(1)
            .or(captures.get(2))
            .map_or("", |m| m.as_str());
        // `-- '-'` cannot name the flag.
        if c != "-" && !c.is_empty() {
            add(Hit::UnknownOption(format!("-{c}")));
        }
    }
    for captures in s.required.captures_iter(text) {
        add(Hit::RequiredOption(captures[1].to_owned()));
    }
    for captures in s
        .pytest_path
        .captures_iter(text)
        .chain(s.node_path.captures_iter(text))
    {
        add(Hit::TestPath(captures[1].to_owned()));
    }
    if s.workspace.is_match(text) {
        add(Hit::WorkspaceProtocol);
    }
    for captures in s.corepack.captures_iter(text) {
        add(Hit::Corepack(captures[1].to_owned()));
    }
    hits
}

/// `--flag=value` names `--flag`.
fn option_name(word: &str) -> String {
    match word.split_once('=') {
        Some((flag, _)) if flag.starts_with('-') => flag.to_owned(),
        _ => word.to_owned(),
    }
}

/// Signature hits in an event's output, each with the stream or result that held it.
fn event_hits(event: &Event) -> Vec<(Hit, Pointer)> {
    let mut found: Vec<(Hit, Pointer)> = Vec::new();
    for text in &event.texts {
        let pointer = match &text.run {
            Some(id) => Pointer::Run {
                id: id.clone(),
                stream: text.stream,
            },
            None => event.pointer.clone(),
        };
        for hit in hits(&text.body) {
            if !found.iter().any(|(seen, _)| *seen == hit) {
                found.push((hit, pointer.clone()));
            }
        }
    }
    found
}

/// What a later success of the mistaken form looks like.
#[derive(Clone, PartialEq, Eq)]
enum Wrong {
    Command { dir: String, argv: Vec<String> },
    Program(Vec<String>),
    Tests { dir: String, argv: Vec<String> },
}

pub struct Candidate {
    pub class: Class,
    pub id: String,
    /// Key slots, then the latest context slots and the most frequent display slots.
    pub slots: BTreeMap<String, String>,
    votes: BTreeMap<&'static str, BTreeMap<String, u64>>,
    pub failures: u64,
    pub corrections: u64,
    /// Each session with the time of its last failure.
    pub sessions: BTreeMap<String, u64>,
    pub threads: BTreeSet<String>,
    pub failure_bytes: u64,
    pub first_ms: u64,
    pub last_ms: u64,
    evidence: BTreeSet<(u64, Pointer)>,
    /// Claude Code transcripts supplied evidence.
    pub transcript: bool,
    pub contradicted: bool,
    counted: HashSet<(usize, usize)>,
    corrected: HashSet<(usize, usize)>,
    context_ms: u64,
    wrong: Vec<Wrong>,
}

impl Candidate {
    fn new(class: Class, key: &[(&'static str, String)]) -> Self {
        let mut digest = Sha256::new();
        digest.update(class.name());
        for (_, value) in key {
            digest.update([0]);
            digest.update(value);
        }
        Self {
            class,
            id: format!("{:x}", digest.finalize())[..12].to_owned(),
            slots: key
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
            votes: BTreeMap::new(),
            failures: 0,
            corrections: 0,
            sessions: BTreeMap::new(),
            threads: BTreeSet::new(),
            failure_bytes: 0,
            first_ms: u64::MAX,
            last_ms: 0,
            evidence: BTreeSet::new(),
            transcript: false,
            contradicted: false,
            counted: HashSet::new(),
            corrected: HashSet::new(),
            context_ms: 0,
            wrong: Vec::new(),
        }
    }

    fn fail(&mut self, at: (usize, usize), event: &Event, pointer: Pointer) {
        if !self.counted.insert(at) {
            return;
        }
        self.failures += 1;
        let last = self.sessions.entry(event.session.clone()).or_default();
        *last = (*last).max(event.time_ms);
        self.threads.insert(event.thread.clone());
        self.failure_bytes += event.failure_bytes;
        self.first_ms = self.first_ms.min(event.time_ms);
        self.last_ms = self.last_ms.max(event.time_ms);
        self.transcript |= event.transcript;
        self.evidence.insert((event.time_ms, pointer));
    }

    fn correct(&mut self, at: (usize, usize)) {
        if self.corrected.insert(at) {
            self.corrections += 1;
        }
    }

    fn context(&mut self, time_ms: u64, slots: &[(&'static str, String)]) {
        if time_ms >= self.context_ms {
            self.context_ms = time_ms;
            for (name, value) in slots {
                self.slots.insert((*name).to_owned(), value.clone());
            }
        }
    }

    fn vote(&mut self, slot: &'static str, value: &str) {
        *self
            .votes
            .entry(slot)
            .or_default()
            .entry(value.to_owned())
            .or_default() += 1;
    }

    fn expect(&mut self, wrong: Wrong) {
        if !self.wrong.contains(&wrong) {
            self.wrong.push(wrong);
        }
    }

    /// Up to three of the most recent evidence pointers.
    pub fn evidence(&self) -> Vec<&Pointer> {
        self.evidence.iter().rev().take(3).map(|(_, p)| p).collect()
    }

    /// Fill display slots with their most frequent value, the first on a tie.
    fn finish(&mut self) {
        for (slot, counts) in &self.votes {
            if let Some((value, _)) = counts
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            {
                self.slots.insert((*slot).to_owned(), value.clone());
            }
        }
    }
}

pub struct Analysis {
    pub candidates: Vec<Candidate>,
    pub typos: u64,
    pub credentials: u64,
    pub nested_checkout: u64,
}

struct Outcome {
    class: Class,
    key: Vec<(&'static str, String)>,
    context: Vec<(&'static str, String)>,
    wrong: Wrong,
    typo: bool,
    nested: bool,
}

struct Thread<'e> {
    index: usize,
    events: Vec<&'e Event>,
    hits: Vec<OnceCell<Vec<(Hit, Pointer)>>>,
}

impl Thread<'_> {
    fn hits(&self, at: usize) -> &[(Hit, Pointer)] {
        self.hits[at].get_or_init(|| event_hits(self.events[at]))
    }

    fn has(&self, at: usize, kind: &Hit) -> bool {
        self.hits(at)
            .iter()
            .any(|(hit, _)| std::mem::discriminant(hit) == std::mem::discriminant(kind))
    }
}

struct Builder<'a> {
    root: &'a Path,
    candidates: BTreeMap<String, Candidate>,
    skipped: HashSet<(usize, usize)>,
    typos: u64,
    credentials: u64,
    nested_checkout: u64,
}

pub fn analyze(root: &Path, events: &[Event]) -> Analysis {
    let mut grouped: BTreeMap<&str, Vec<&Event>> = BTreeMap::new();
    for event in events {
        grouped.entry(&event.thread).or_default().push(event);
    }
    let mut builder = Builder {
        root,
        candidates: BTreeMap::new(),
        skipped: HashSet::new(),
        typos: 0,
        credentials: 0,
        nested_checkout: 0,
    };
    for (index, events) in grouped.into_values().enumerate() {
        let thread = Thread {
            index,
            hits: events.iter().map(|_| OnceCell::new()).collect(),
            events,
        };
        for at in 0..thread.events.len() {
            if thread.events[at].status == Status::Unknown {
                continue;
            }
            builder.self_evident(&thread, at);
            builder.corrected(&thread, at);
        }
    }
    let mut candidates: Vec<Candidate> = builder.candidates.into_values().collect();
    for candidate in &mut candidates {
        candidate.finish();
        candidate.contradicted = events
            .iter()
            .filter(|event| event.time_ms > candidate.last_ms)
            .any(|event| contradicts(candidate, event));
    }
    Analysis {
        candidates,
        typos: builder.typos,
        credentials: builder.credentials,
        nested_checkout: builder.nested_checkout,
    }
}

impl Builder<'_> {
    fn entry(&mut self, class: Class, key: &[(&'static str, String)]) -> &mut Candidate {
        let candidate = Candidate::new(class, key);
        self.candidates
            .entry(candidate.id.clone())
            .or_insert(candidate)
    }

    fn self_evident(&mut self, thread: &Thread, at: usize) {
        let event = thread.events[at];
        let place = (thread.index, at);
        for hit in self_evident(event) {
            match hit {
                SelfHit::Missing(kind, operand) => {
                    let failed = failed_piece(event, kind, &operand);
                    let python = failed
                        .filter(|_| kind == Missing::Module)
                        .map(|cmd| basename(&cmd.argv[0]).to_owned());
                    let dir = failed.map_or(event.dir.clone(), |cmd| cmd.dir.clone());
                    let alternative = (kind != Missing::Relative)
                        .then(|| {
                            failed.and_then(|cmd| alternative(thread, at, kind, &operand, cmd))
                        })
                        .flatten();
                    let lead = match (kind, failed) {
                        (Missing::Module, Some(cmd)) => cmd.argv[..3].to_vec(),
                        _ => vec![operand.clone()],
                    };
                    let candidate = self.entry(
                        Class::MissingProgram,
                        &[("kind", kind.name().to_owned()), ("program", operand)],
                    );
                    candidate.fail(place, event, event.pointer.clone());
                    candidate.expect(Wrong::Program(lead));
                    if kind == Missing::Relative {
                        candidate.context(event.time_ms, &[("dir", dir)]);
                    }
                    if let Some(python) = python {
                        candidate.vote("python", &python);
                    }
                    if let Some((g, q, q_dir)) = alternative {
                        candidate.correct((thread.index, g));
                        candidate.vote("alternative", &q);
                        if q.split(' ').next().is_some_and(|w| w.contains('/')) {
                            candidate.context(event.time_ms, &[("alternative_dir", q_dir)]);
                        }
                    }
                }
                SelfHit::Glob(operand) => {
                    let candidate = self.entry(Class::ZshGlob, &[]);
                    candidate.fail(place, event, event.pointer.clone());
                    candidate.vote("example", &operand);
                }
                SelfHit::Equals => {
                    self.entry(Class::ZshEquals, &[])
                        .fail(place, event, event.pointer.clone());
                }
            }
        }
    }

    fn corrected(&mut self, thread: &Thread, at: usize) {
        let event = thread.events[at];
        let Some(f) = event.simple() else {
            return;
        };
        let Status::Code(code) = event.status else {
            return;
        };
        if basename(f.program()) == "justless" {
            return;
        }
        if let Some(runner) = empty_run(event, f) {
            let found = find(thread, at, true, |g_event, g| {
                empty_tests(runner, f, g_event, g)
            });
            if let Some((g, outcome)) = found {
                let pointer = test_pointer(event);
                self.record(thread, at, g, outcome, pointer, None);
            }
            return;
        }
        if code == 0 && !f.masked || query_exit(f, code) {
            return;
        }
        let hits = thread.hits(at).to_vec();
        for (hit, pointer) in &hits {
            let found = find(thread, at, false, |g_event, g| {
                if g.masked && thread_has(thread, g_event, hit) {
                    return None;
                }
                pair(hit, event, f, code, g)
            });
            if let Some((g, outcome)) = found {
                self.record(thread, at, g, outcome, pointer.clone(), Some(hit));
                return;
            }
        }
        if hits.is_empty() && code != 0 {
            // Any failure text that names the flag the correction adds.
            let found = find(thread, at, false, |_, g| missing_option(f, g, None, event));
            if let Some((g, outcome)) = found {
                self.record(thread, at, g, outcome, event.pointer.clone(), None);
            }
        }
    }

    fn record(
        &mut self,
        thread: &Thread,
        at: usize,
        g: usize,
        outcome: Outcome,
        pointer: Pointer,
        hit: Option<&Hit>,
    ) {
        let event = thread.events[at];
        let correction = thread.events[g];
        let first = self.skipped.insert((thread.index, g));
        if outcome.nested {
            self.nested_checkout += u64::from(first);
            return;
        }
        if privacy::unsafe_command(&event.raw, &event.env, self.root)
            || privacy::unsafe_command(&correction.raw, &correction.env, self.root)
        {
            self.credentials += u64::from(first);
            return;
        }
        if outcome.typo {
            self.typos += u64::from(first);
            return;
        }
        let f = event.simple().expect("simple failure");
        let candidate = self.entry(outcome.class, &outcome.key);
        candidate.fail((thread.index, at), event, pointer);
        candidate.correct((thread.index, g));
        candidate.context(event.time_ms, &outcome.context);
        candidate.expect(outcome.wrong);
        // Identical failures just before this one belong to the same episode.
        for before in (0..at).rev() {
            let earlier = thread.events[before];
            if earlier.status == Status::Unknown {
                continue;
            }
            let same = earlier
                .simple()
                .is_some_and(|cmd| cmd.argv == f.argv && cmd.same_place(f))
                && earlier.status != Status::Code(0)
                && hit.is_none_or(|hit| thread.has(before, hit));
            if !same {
                break;
            }
            let pointer = hit
                .and_then(|hit| {
                    thread
                        .hits(before)
                        .iter()
                        .find(|(seen, _)| {
                            std::mem::discriminant(seen) == std::mem::discriminant(hit)
                        })
                        .map(|(_, pointer)| pointer.clone())
                })
                .unwrap_or_else(|| earlier.pointer.clone());
            candidate.fail((thread.index, before), earlier, pointer);
        }
    }
}

fn thread_has(thread: &Thread, event: &Event, kind: &Hit) -> bool {
    let at = thread
        .events
        .iter()
        .position(|e| std::ptr::eq(*e, event))
        .expect("event in thread");
    thread.has(at, kind)
}

/// The first later event in the thread, within the window, that pairs with event `at`.
fn find<'e>(
    thread: &Thread<'e>,
    at: usize,
    any_status: bool,
    mut pair: impl FnMut(&'e Event, &'e Cmd) -> Option<Outcome>,
) -> Option<(usize, Outcome)> {
    let start = thread.events[at].time_ms;
    let mut seen = 0;
    for later in at + 1..thread.events.len() {
        let event = thread.events[later];
        if event.time_ms.saturating_sub(start) > WINDOW_MS {
            break;
        }
        if event.status == Status::Unknown {
            continue;
        }
        seen += 1;
        if seen > WINDOW_EVENTS {
            break;
        }
        if !any_status && event.status != Status::Code(0) {
            continue;
        }
        let Some(g) = event.simple() else {
            continue;
        };
        if let Some(outcome) = pair(event, g) {
            return Some((later, outcome));
        }
    }
    None
}

fn pair(hit: &Hit, event: &Event, f: &Cmd, code: i32, g: &Cmd) -> Option<Outcome> {
    match hit {
        Hit::NoMakefile | Hit::NoCargoToml | Hit::NoPackageJson | Hit::NoPyproject => {
            wrong_directory(hit, f, g)
        }
        Hit::NoRule(_) => wrong_directory(hit, f, g).or_else(|| unknown_target(hit, f, g)),
        Hit::CargoPackage(_)
        | Hit::CargoTarget(..)
        | Hit::NoSuchCommand(_)
        | Hit::MissingScript(_) => unknown_target(hit, f, g),
        Hit::UnknownOption(flag) => unknown_option(flag, f, g),
        Hit::RequiredOption(flag) => missing_option(f, g, Some(flag), event),
        Hit::TestPath(path) => wrong_test_path(path, f, code, g),
        Hit::WorkspaceProtocol | Hit::Corepack(_) => wrong_package_manager(hit, f, g),
    }
}

fn wrong_directory(hit: &Hit, f: &Cmd, g: &Cmd) -> Option<Outcome> {
    if g.argv != f.argv || f.same_place(g) {
        return None;
    }
    let nested = f.checkout != g.checkout;
    if !nested && f.dir == g.dir {
        return None;
    }
    Some(Outcome {
        class: Class::WrongDirectory,
        key: vec![
            ("program", f.program().to_owned()),
            ("from", f.dir.clone()),
            ("to", g.dir.clone()),
        ],
        context: vec![
            ("manifest", hit.manifest()?.to_owned()),
            ("command", stem(&f.argv)),
        ],
        wrong: Wrong::Command {
            dir: f.dir.clone(),
            argv: f.argv.clone(),
        },
        typo: false,
        nested,
    })
}

fn unknown_target(hit: &Hit, f: &Cmd, g: &Cmd) -> Option<Outcome> {
    if !f.same_place(g) {
        return None;
    }
    let (before, after) = (canonical(&f.argv), canonical(&g.argv));
    let (index, wrong, right) = replaced(&before, &after)?;
    let program = f.program();
    let flag_of = |names: &[&str]| -> Option<(String, String, String)> {
        // `-p X`, `--package X` or `--package=X`.
        if index > 0 && names.contains(&before[index - 1].as_str()) {
            return Some((wrong.clone(), right.clone(), before[index - 1].clone()));
        }
        let (flag, x) = wrong.split_once('=')?;
        let (other, y) = right.split_once('=')?;
        (names.contains(&flag) && flag == other)
            .then(|| (x.to_owned(), y.to_owned(), flag.to_owned()))
    };
    let (kind, stem_text, x, y, mut context) = match hit {
        Hit::NoRule(name) if matches!(program, "make" | "gmake") && wrong == *name => (
            "make",
            program.to_owned(),
            wrong.clone(),
            right.clone(),
            vec![],
        ),
        Hit::CargoPackage(name) if program == "cargo" => {
            let (x, y, flag) = flag_of(&["-p", "--package"])?;
            (x == *name).then_some(())?;
            ("package", stem(&f.argv), x, y, vec![("flag", flag)])
        }
        Hit::CargoTarget(target, name) if program == "cargo" => {
            let long = format!("--{target}");
            let (x, y, flag) = flag_of(&[long.as_str()])?;
            (x == *name).then_some(())?;
            (
                "target",
                stem(&f.argv),
                x,
                y,
                vec![("flag", flag), ("target_kind", (*target).to_owned())],
            )
        }
        Hit::NoSuchCommand(name)
            if matches!(program, "cargo" | "git") && index == 1 && wrong == *name =>
        {
            (
                "subcommand",
                program.to_owned(),
                wrong.clone(),
                right.clone(),
                vec![],
            )
        }
        Hit::MissingScript(name)
            if program == "npm" && before[1] == "run" && index == 2 && wrong == *name =>
        {
            (
                "script",
                "npm run".to_owned(),
                wrong.clone(),
                right.clone(),
                vec![],
            )
        }
        _ => return None,
    };
    if y.is_empty() || y.starts_with('-') {
        return None;
    }
    context.push(("dir", g.dir.clone()));
    Some(Outcome {
        class: Class::UnknownTarget,
        typo: typo(&x, &y),
        key: vec![
            ("kind", kind.to_owned()),
            ("stem", stem_text),
            ("name", x),
            ("replacement", y),
        ],
        context,
        wrong: Wrong::Command {
            dir: f.dir.clone(),
            argv: before,
        },
        nested: false,
    })
}

fn unknown_option(flag: &str, f: &Cmd, g: &Cmd) -> Option<Outcome> {
    if !flag.starts_with('-') || !f.same_place(g) || stem(&f.argv) != stem(&g.argv) {
        return None;
    }
    let index = f
        .argv
        .iter()
        .position(|word| word == flag || word.starts_with(&format!("{flag}=")))?;
    let replacement = if f.argv.len() == g.argv.len() {
        let (at, _, right) = replaced(&f.argv, &g.argv)?;
        (at == index && right.starts_with('-')).then(|| option_name(&right))?
    } else {
        let without = |width: usize| {
            let mut words = f.argv.clone();
            words.drain(index..(index + width).min(words.len()));
            words == g.argv
        };
        let takes_value = !f.argv[index].contains('=')
            && f.argv.get(index + 1).is_some_and(|w| !w.starts_with('-'));
        (without(1) || takes_value && without(2)).then(String::new)?
    };
    Some(Outcome {
        class: Class::UnknownOption,
        typo: !replacement.is_empty()
            && typo(
                flag.trim_start_matches('-'),
                replacement.trim_start_matches('-'),
            ),
        key: vec![
            ("stem", stem(&f.argv)),
            ("flag", flag.to_owned()),
            ("replacement", replacement),
        ],
        context: vec![],
        wrong: Wrong::Command {
            dir: f.dir.clone(),
            argv: f.argv.clone(),
        },
        nested: false,
    })
}

fn missing_option(f: &Cmd, g: &Cmd, required: Option<&String>, event: &Event) -> Option<Outcome> {
    if !f.same_place(g) || stem(&f.argv) != stem(&g.argv) || g.argv.len() <= f.argv.len() {
        return None;
    }
    let start = f
        .argv
        .iter()
        .zip(&g.argv)
        .position(|(a, b)| a != b)
        .unwrap_or(f.argv.len());
    let width = g.argv.len() - f.argv.len();
    if g.argv[start + width..] != f.argv[start..] {
        return None;
    }
    let added = &g.argv[start..start + width];
    let flag = option_name(&added[0]);
    if !flag.starts_with('-') || added[1..].iter().any(|word| word.starts_with('-')) {
        return None;
    }
    let named = match required {
        Some(required) => flag == *required,
        None => {
            flag.len() >= 3
                && event
                    .texts
                    .iter()
                    .any(|text| contains_word(&text.body, &flag))
        }
    };
    named.then(|| Outcome {
        class: Class::MissingOption,
        typo: false,
        key: vec![("stem", stem(&f.argv)), ("flag", flag)],
        context: vec![],
        wrong: Wrong::Command {
            dir: f.dir.clone(),
            argv: f.argv.clone(),
        },
        nested: false,
    })
}

fn wrong_test_path(path: &str, f: &Cmd, code: i32, g: &Cmd) -> Option<Outcome> {
    let runner = events::runner(&f.argv)?;
    let valid = match runner {
        Runner::Pytest => code == 4,
        Runner::NodeTest => code != 0,
        Runner::Cargo => false,
    };
    if !valid || !f.same_place(g) || stem(&f.argv) != stem(&g.argv) {
        return None;
    }
    let target = node_id(path);
    let index = f.argv.iter().position(|word| {
        let word = node_id(word);
        path_like(word) && (word == target || target.ends_with(&format!("/{word}")))
    })?;
    let (at, wrong, right) = replaced(&f.argv, &g.argv)?;
    let (wrong, right) = (node_id(&wrong), node_id(&right));
    if at != index || !path_like(right) {
        return None;
    }
    let (from, to) = (parent(wrong), parent(right));
    if from.is_empty() || to.is_empty() || from == to {
        return None;
    }
    Some(Outcome {
        class: Class::WrongTestPath,
        typo: typo(from, to),
        key: vec![
            ("stem", stem(&f.argv)),
            ("from", from.to_owned()),
            ("to", to.to_owned()),
        ],
        context: vec![("dir", f.dir.clone())],
        wrong: Wrong::Command {
            dir: f.dir.clone(),
            argv: f.argv.clone(),
        },
        nested: false,
    })
}

fn wrong_package_manager(hit: &Hit, f: &Cmd, g: &Cmd) -> Option<Outcome> {
    let (from, to) = (basename(f.program()), basename(g.program()));
    let valid = match hit {
        Hit::WorkspaceProtocol => from == "npm",
        Hit::Corepack(configured) => MANAGERS.contains(&from) && to == configured,
        _ => false,
    };
    if !valid
        || !MANAGERS.contains(&to)
        || from == to
        || !f.same_place(g)
        || manager_args(&f.argv[1..]) != manager_args(&g.argv[1..])
    {
        return None;
    }
    Some(Outcome {
        class: Class::WrongPackageManager,
        typo: false,
        key: vec![("from", from.to_owned()), ("to", to.to_owned())],
        context: vec![("dir", g.dir.clone())],
        wrong: Wrong::Command {
            dir: f.dir.clone(),
            argv: f.argv.clone(),
        },
        nested: false,
    })
}

/// Which runner ran no tests: cargo printed only `running 0 tests` lines with nothing
/// filtered out and exited 0, or pytest exited 5 (ExitCode.NO_TESTS_COLLECTED [source]).
fn empty_run(event: &Event, f: &Cmd) -> Option<Runner> {
    if f.masked {
        return None;
    }
    match (events::runner(&f.argv)?, event.status) {
        (Runner::Cargo, Status::Code(0))
            if event.tests.reports > 0 && !event.tests.ran && !event.tests.filtered =>
        {
            Some(Runner::Cargo)
        }
        (Runner::Pytest, Status::Code(5)) => Some(Runner::Pytest),
        _ => None,
    }
}

fn ran_tests(runner: Runner, event: &Event) -> bool {
    match runner {
        Runner::Cargo => event.tests.ran,
        // ExitCode.OK and TESTS_FAILED [source].
        Runner::Pytest => matches!(event.status, Status::Code(0 | 1)),
        Runner::NodeTest => false,
    }
}

fn empty_tests(runner: Runner, f: &Cmd, g_event: &Event, g: &Cmd) -> Option<Outcome> {
    if g.masked
        || events::runner(&g.argv) != Some(runner)
        || stem(&f.argv) != stem(&g.argv)
        || !ran_tests(runner, g_event)
    {
        return None;
    }
    let wrong = Wrong::Tests {
        dir: f.dir.clone(),
        argv: f.argv.clone(),
    };
    let (form, fix, nested) = if f.argv == g.argv {
        if f.same_place(g) {
            return None;
        }
        ("dir", g.dir.clone(), f.checkout != g.checkout)
    } else if f.same_place(g) {
        ("words", added_words(&f.argv, &g.argv)?.join(" "), false)
    } else {
        return None;
    };
    Some(Outcome {
        class: Class::EmptyTests,
        typo: false,
        key: vec![
            ("form", form.to_owned()),
            ("stem", stem(&f.argv)),
            ("fix", fix),
            ("from", f.dir.clone()),
        ],
        context: vec![],
        wrong,
        nested,
    })
}

fn test_pointer(event: &Event) -> Pointer {
    event
        .texts
        .iter()
        .find_map(|text| {
            let id = text.run.as_ref()?;
            (text.stream == "stdout").then(|| Pointer::Run {
                id: id.clone(),
                stream: "stdout",
            })
        })
        .unwrap_or_else(|| event.pointer.clone())
}

/// The piece of a failed command whose leading words are the missing program.
fn failed_piece<'e>(event: &'e Event, kind: Missing, operand: &str) -> Option<&'e Cmd> {
    event.commands().iter().find(|cmd| match kind {
        Missing::Module => {
            cmd.argv.len() >= 3
                && basename(&cmd.argv[0]).starts_with("python")
                && cmd.argv[1] == "-m"
                && cmd.argv[2] == operand
        }
        _ => cmd.argv[0] == operand,
    })
}

/// A later successful command that keeps the failed piece's arguments and changes
/// only its leading program words to a related program.
fn alternative(
    thread: &Thread,
    at: usize,
    kind: Missing,
    operand: &str,
    failed: &Cmd,
) -> Option<(usize, String, String)> {
    let event = thread.events[at];
    let lead = if kind == Missing::Module { 3 } else { 1 };
    let args = &failed.argv[lead..];
    let mut seen = 0;
    for later in at + 1..thread.events.len() {
        let next = thread.events[later];
        if next.time_ms.saturating_sub(event.time_ms) > WINDOW_MS {
            break;
        }
        if next.status == Status::Unknown {
            continue;
        }
        seen += 1;
        if seen > WINDOW_EVENTS {
            break;
        }
        if next.status != Status::Code(0) {
            continue;
        }
        for cmd in next.commands() {
            if cmd.argv.len() <= args.len() || cmd.argv[cmd.argv.len() - args.len()..] != *args {
                continue;
            }
            let words = &cmd.argv[..cmd.argv.len() - args.len()];
            if !related(kind, operand, failed, words) {
                continue;
            }
            let missing = self_evident(next)
                .into_iter()
                .any(|hit| matches!(hit, SelfHit::Missing(_, ref program) if *program == words[0]));
            if !missing {
                return Some((later, words.join(" "), cmd.dir.clone()));
            }
        }
    }
    None
}

fn related(kind: Missing, operand: &str, failed: &Cmd, words: &[String]) -> bool {
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let single = |prefix: &str| {
        words.len() == 1 && {
            let name = basename(words[0]);
            name != prefix
                && name.len().min(prefix.len()) >= 2
                && (name.starts_with(prefix) || prefix.starts_with(name))
        }
    };
    match kind {
        Missing::Program => {
            let name = basename(operand);
            let table: &[&[&str]] = match name {
                "python" => &[
                    &["python3"],
                    &["uv", "run", "python"],
                    &["poetry", "run", "python"],
                ],
                "pip" => &[&["pip3"], &["python3", "-m", "pip"], &["uv", "pip"]],
                "pytest" => &[
                    &["uv", "run", "pytest"],
                    &["poetry", "run", "pytest"],
                    &[".venv/bin/pytest"],
                ],
                "node" => &[&["nodejs"]],
                "fd" => &[&["fdfind"]],
                "bat" => &[&["batcat"]],
                "docker-compose" => &[&["docker", "compose"]],
                "sed" => &[&["gsed"]],
                "grep" => &[&["ggrep"]],
                "awk" => &[&["gawk"]],
                "find" => &[&["gfind"]],
                "tar" => &[&["gtar"]],
                "make" => &[&["gmake"]],
                _ => &[],
            };
            table.contains(&words.as_slice()) || single(name)
        }
        Missing::Module => {
            let python = failed.argv[0].as_str();
            match words.as_slice() {
                [interpreter, "-m", module] => {
                    *module == operand
                        && *interpreter != python
                        && basename(interpreter).starts_with("python")
                }
                ["uv" | "poetry", "run", "pytest"] => operand == "pytest",
                [program] => basename(program).starts_with(operand),
                _ => false,
            }
        }
        Missing::Relative => false,
    }
}

fn contradicts(candidate: &Candidate, event: &Event) -> bool {
    let Some(cmd) = event.simple() else {
        return false;
    };
    let Status::Code(code) = event.status else {
        return false;
    };
    if cmd.masked {
        return false;
    }
    candidate.wrong.iter().any(|wrong| match wrong {
        Wrong::Command { dir, argv } => {
            code == 0
                && cmd.dir == *dir
                && canonical(&cmd.argv) == *argv
                && event_hits(event).is_empty()
        }
        Wrong::Program(lead) => {
            code == 0
                && cmd.argv.len() >= lead.len()
                && basename(&cmd.argv[0]) == basename(&lead[0])
                && cmd.argv[1..lead.len()] == lead[1..]
                && !self_evident(event)
                    .iter()
                    .any(|hit| matches!(hit, SelfHit::Missing(..)))
        }
        Wrong::Tests { dir, argv } => {
            cmd.dir == *dir
                && cmd.argv == *argv
                && events::runner(argv).is_some_and(|runner| ran_tests(runner, event))
        }
    })
}

/// A search or comparison that exits 1 to say it found nothing.
fn query_exit(cmd: &Cmd, code: i32) -> bool {
    code == 1
        && matches!(
            (basename(cmd.program()), cmd.argv.get(1).map(String::as_str)),
            (
                "grep"
                    | "egrep"
                    | "fgrep"
                    | "rg"
                    | "ag"
                    | "ack"
                    | "diff"
                    | "cmp"
                    | "test"
                    | "["
                    | "which"
                    | "pgrep",
                _,
            ) | ("git", Some("grep"))
                | ("command", Some("-v"))
        )
}

/// The program and subcommand, as in `cargo test`, keeping a relative program as
/// written and naming test runners by their runner.
pub fn stem(argv: &[String]) -> String {
    let words = events::unwrap(argv);
    let Some(first) = words.first() else {
        return String::new();
    };
    let program = if first.contains('/') && !Path::new(first).is_absolute() {
        first.as_str()
    } else {
        basename(first)
    };
    match events::runner(words) {
        Some(Runner::Pytest) => return "pytest".into(),
        Some(Runner::NodeTest) => return "node --test".into(),
        _ => {}
    }
    if matches!(
        program,
        "rg" | "grep" | "ag" | "ack" | "find" | "ls" | "tree" | "fd" | "fdfind"
    ) {
        return program.to_owned();
    }
    match words.get(1) {
        Some(sub)
            if sub.starts_with(|c: char| c.is_ascii_lowercase())
                && sub.chars().all(|c| c.is_ascii_lowercase() || c == '-') =>
        {
            format!("{program} {sub}")
        }
        _ => program.to_owned(),
    }
}

/// `npm test` is `npm run test`.
fn canonical(argv: &[String]) -> Vec<String> {
    let mut words = argv.to_vec();
    if words.first().is_some_and(|w| w == "npm")
        && words
            .get(1)
            .is_some_and(|w| matches!(w.as_str(), "test" | "start" | "stop" | "restart"))
    {
        words.insert(1, "run".into());
    }
    words
}

/// Package-manager arguments with `run` dropped, so `npm run x` is `pnpm x`.
fn manager_args(words: &[String]) -> Vec<&str> {
    let mut args: Vec<&str> = words.iter().map(String::as_str).collect();
    if args.first() == Some(&"run") {
        args.remove(0);
    }
    if args.first() == Some(&"i") {
        args[0] = "install";
    }
    args
}

/// The single position where two equally long commands differ.
fn replaced(before: &[String], after: &[String]) -> Option<(usize, String, String)> {
    if before.len() != after.len() {
        return None;
    }
    let mut differences = before
        .iter()
        .zip(after)
        .enumerate()
        .filter(|(_, (a, b))| a != b);
    let (index, (wrong, right)) = differences.next()?;
    differences
        .next()
        .is_none()
        .then(|| (index, wrong.clone(), right.clone()))
}

/// The words a correction adds, when the failed command is a subsequence of it.
fn added_words(before: &[String], after: &[String]) -> Option<Vec<String>> {
    let mut added = Vec::new();
    let mut remaining = before.iter().peekable();
    for word in after {
        if remaining.peek() == Some(&word) {
            remaining.next();
        } else {
            added.push(word.clone());
        }
    }
    (remaining.peek().is_none() && !added.is_empty()).then_some(added)
}

fn node_id(path: &str) -> &str {
    path.split("::").next().unwrap_or(path)
}

fn path_like(word: &str) -> bool {
    !word.starts_with('-')
        && (word.contains('/')
            || [
                ".py", ".js", ".mjs", ".cjs", ".ts", ".mts", ".cts", ".jsx", ".tsx",
            ]
            .iter()
            .any(|extension| word.ends_with(extension)))
}

fn parent(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit_once('/')
        .map_or("", |(dir, _)| dir)
}

fn contains_word(text: &str, word: &str) -> bool {
    let boundary =
        |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '-'));
    text.match_indices(word).any(|(start, _)| {
        boundary(text[..start].chars().next_back())
            && boundary(text[start + word.len()..].chars().next())
    })
}

/// A correction of one or two characters between names of at least four.
fn typo(wrong: &str, right: &str) -> bool {
    wrong != right && wrong.len() >= 4 && right.len() >= 4 && distance(wrong, right) <= 2
}

/// Damerau–Levenshtein distance (optimal string alignment).
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut rows = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in rows.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in rows[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (rows[i - 1][j] + 1)
                .min(rows[i][j - 1] + 1)
                .min(rows[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(rows[i - 2][j - 2] + 1);
            }
            rows[i][j] = best;
        }
    }
    rows[a.len()][b.len()]
}

// SCOPES, CHECKS AND TEXT

pub fn scope(class: Class, slots: &BTreeMap<String, String>) -> Scope {
    let get = |name: &str| slots.get(name).map_or("", String::as_str);
    match class {
        Class::MissingProgram if get("kind") == "relative" => Scope::Project,
        Class::MissingProgram | Class::ZshGlob | Class::ZshEquals => Scope::Machine,
        Class::UnknownTarget if get("kind") == "subcommand" => Scope::Machine,
        Class::UnknownOption | Class::MissingOption => {
            if get("stem")
                .split(' ')
                .next()
                .is_some_and(|p| p.contains('/'))
            {
                Scope::Project
            } else {
                Scope::Machine
            }
        }
        _ => Scope::Project,
    }
}

/// Confirm a rule against the current tree, `PATH` or `$SHELL`, without running anything.
pub fn check(root: &Path, env: &Env, class: Class, slots: &BTreeMap<String, String>) -> Check {
    let get = |name: &str| slots.get(name).map_or("", String::as_str);
    let at = |dir: &str| {
        if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(dir)
        }
    };
    match class {
        Class::MissingProgram => match get("kind") {
            "relative" => {
                let path = at(get("dir")).join(get("program"));
                if path.exists() {
                    Check::Stale(format!("{} exists", get("program")))
                } else {
                    Check::Verified(format!("{} is missing", get("program")))
                }
            }
            "program" => {
                let program = get("program");
                match env.executable(program) {
                    None => Check::Unverified,
                    Some(true) => Check::Stale(format!("{program} is on PATH")),
                    Some(false) => {
                        let alternative = get("alternative").split(' ').next().unwrap_or("");
                        let usable = alternative.is_empty()
                            || if alternative.contains('/') {
                                is_executable(&at(get("alternative_dir")).join(alternative))
                            } else {
                                env.executable(alternative) == Some(true)
                            };
                        if usable {
                            Check::Verified("not on PATH".into())
                        } else {
                            Check::Stale(format!("{alternative} is not on PATH"))
                        }
                    }
                }
            }
            _ => Check::Unverified,
        },
        Class::ZshGlob | Class::ZshEquals => match env.shell.as_deref().map(basename) {
            Some("zsh") => Check::Verified("SHELL is zsh".into()),
            Some(shell) => Check::Stale(format!("SHELL is {shell}")),
            None if cfg!(windows) => Check::Stale("SHELL is unset".into()),
            None => Check::Unverified,
        },
        Class::WrongDirectory => {
            let manifest = get("manifest");
            let names: Vec<&str> = if manifest == "Makefile" {
                MAKEFILES.to_vec()
            } else {
                vec![manifest]
            };
            let upward = manifest != "Makefile";
            let find = |dir: &str| -> Option<String> {
                let mut dir = dir.to_owned();
                loop {
                    if let Some(name) = names.iter().find(|name| at(&dir).join(name).is_file()) {
                        return Some(joined(&dir, name));
                    }
                    if !upward || dir.is_empty() {
                        return None;
                    }
                    dir = parent(&dir).to_owned();
                }
            };
            if let Some(found) = find(get("from")) {
                Check::Stale(format!("{found} exists"))
            } else if let Some(found) = find(get("to")) {
                Check::Verified(found)
            } else {
                Check::Stale(format!("{} is missing", joined(get("to"), manifest)))
            }
        }
        Class::UnknownTarget => {
            let (x, y, dir) = (get("name"), get("replacement"), get("dir"));
            match get("kind") {
                "script" => {
                    let mut dir = dir.to_owned();
                    loop {
                        let path = joined(&dir, "package.json");
                        if let Some(scripts) = read_json(&root.join(&path))
                            .and_then(|json| json.get("scripts").cloned())
                        {
                            return if scripts.get(x).is_some() {
                                Check::Stale(format!("{path} has a {x} script"))
                            } else if scripts.get(y).is_some() {
                                Check::Verified(format!("{path} scripts"))
                            } else {
                                Check::Unverified
                            };
                        }
                        if dir.is_empty() {
                            return Check::Unverified;
                        }
                        dir = parent(&dir).to_owned();
                    }
                }
                "make" => {
                    let Some((name, text)) = MAKEFILES.iter().find_map(|name| {
                        Some((*name, std::fs::read_to_string(at(dir).join(name)).ok()?))
                    }) else {
                        return Check::Unverified;
                    };
                    let defines = |target: &str| {
                        Regex::new(&format!(r"(?m)^{}\s*:(?:[^=]|$)", regex::escape(target)))
                            .is_ok_and(|pattern| pattern.is_match(&text))
                    };
                    if defines(x) {
                        Check::Stale(format!("{} has {x}", joined(dir, name)))
                    } else if defines(y) {
                        Check::Verified(joined(dir, name))
                    } else {
                        Check::Unverified
                    }
                }
                "package" => {
                    let mut names = BTreeMap::new();
                    cargo_packages(root, root, 4, &mut names);
                    if let Some(path) = names.get(x) {
                        Check::Stale(format!("{path} names {x}"))
                    } else if let Some(path) = names.get(y) {
                        Check::Verified(path.clone())
                    } else {
                        Check::Unverified
                    }
                }
                "subcommand" if get("stem") == "cargo" => {
                    match env.executable(&format!("cargo-{x}")) {
                        None => Check::Unverified,
                        Some(true) => Check::Stale(format!("cargo-{x} is on PATH")),
                        Some(false) => Check::Verified(format!("cargo-{x} not on PATH")),
                    }
                }
                _ => Check::Unverified,
            }
        }
        Class::UnknownOption | Class::MissingOption => Check::Unverified,
        Class::WrongTestPath => {
            let base = at(get("dir"));
            if base.join(get("from")).exists() {
                Check::Stale(format!("{}/ exists", get("from")))
            } else if base.join(get("to")).is_dir() {
                Check::Verified(format!("{}/", joined(get("dir"), get("to"))))
            } else {
                Check::Unverified
            }
        }
        Class::EmptyTests => {
            if get("form") != "dir" {
                Check::Unverified
            } else if at(get("fix")).is_dir() {
                Check::Verified(format!("{}/", get("fix")))
            } else {
                Check::Stale(format!("{}/ is missing", get("fix")))
            }
        }
        Class::WrongPackageManager => {
            let (wrong, right) = (get("from"), get("to"));
            let lockfile = |manager: &str| match manager {
                "npm" => "package-lock.json",
                "pnpm" => "pnpm-lock.yaml",
                _ => "yarn.lock",
            };
            let mut dir = get("dir").to_owned();
            loop {
                let configured = read_json(&at(&dir).join("package.json"))
                    .and_then(|json| json.get("packageManager")?.as_str().map(str::to_owned))
                    .map(|value| value.split('@').next().unwrap_or("").to_owned());
                if configured.as_deref() == Some(right) {
                    return Check::Verified(format!(
                        "{} packageManager",
                        joined(&dir, "package.json")
                    ));
                }
                if configured.as_deref() == Some(wrong) {
                    return Check::Stale(format!("package.json names {wrong}"));
                }
                if at(&dir).join(lockfile(wrong)).is_file() {
                    return Check::Stale(format!("{} exists", joined(&dir, lockfile(wrong))));
                }
                if at(&dir).join(lockfile(right)).is_file() {
                    return Check::Verified(joined(&dir, lockfile(right)));
                }
                if dir.is_empty() {
                    return Check::Unverified;
                }
                dir = parent(&dir).to_owned();
            }
        }
    }
}

fn joined(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Package names declared by Cargo.toml files under the root, with their manifest.
fn cargo_packages(root: &Path, dir: &Path, depth: usize, names: &mut BTreeMap<String, String>) {
    if let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml"))
        && let Ok(document) = text.parse::<toml_edit::DocumentMut>()
        && let Some(name) = document
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(|name| name.as_str())
    {
        let path = dir.join("Cargo.toml");
        let relative = path.strip_prefix(root).unwrap_or(&path);
        names
            .entry(name.to_owned())
            .or_insert_with(|| relative.to_string_lossy().into_owned());
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    !matches!(name, "target" | "node_modules" | ".git" | ".justless")
                })
        })
        .collect();
    children.sort();
    for child in children {
        cargo_packages(root, &child, depth - 1, names);
    }
}

/// The glob example, from the most frequent operand when its shape is known.
fn glob_example(operand: &str) -> (String, String) {
    if let Some(captures) = EXAMPLE_FLAG.captures(operand) {
        (
            format!("{}*.{}", &captures[1], &captures[2]),
            format!("{}'*.{}'", &captures[1], &captures[2]),
        )
    } else if let Some(captures) = EXAMPLE_EXT.captures(operand) {
        (
            format!("*.{}", &captures[1]),
            format!("'*.{}'", &captures[1]),
        )
    } else {
        ("*.txt".into(), "'*.txt'".into())
    }
}

/// The rule's text from its slots, or `None` when a slot is not safe to show.
pub fn render(class: Class, slots: &BTreeMap<String, String>, check: &Check) -> Option<String> {
    let get = |name: &str| slots.get(name).map_or("", String::as_str);
    // Every substituted value must be an allowlisted token; the root is empty.
    let safe = |values: &[&str]| {
        values
            .iter()
            .all(|value| value.split(' ').all(privacy::safe_token))
    };
    let text = match class {
        Class::MissingProgram => {
            let (program, alternative) = (get("program"), get("alternative"));
            match get("kind") {
                "relative" => {
                    safe(&[program]).then_some(())?;
                    format!("`{program}` does not exist here; check the path before running it.")
                }
                "module" => {
                    let python = if get("python").is_empty() { "python3" } else { get("python") };
                    safe(&[program, python]).then_some(())?;
                    if alternative.is_empty() {
                        format!("`{python}` on this machine has no `{program}` module.")
                    } else {
                        safe(&[alternative]).then_some(())?;
                        format!(
                            "Use `{alternative}`, not `{python} -m {program}`; `{python}` on this machine has no `{program}` module."
                        )
                    }
                }
                _ => {
                    safe(&[program]).then_some(())?;
                    if alternative.is_empty() {
                        format!("`{program}` is not installed on this machine.")
                    } else {
                        safe(&[alternative]).then_some(())?;
                        format!(
                            "Use `{alternative}`, not `{program}`; `{program}` is not installed on this machine."
                        )
                    }
                }
            }
        }
        Class::ZshGlob => format!(
            "Quote glob patterns in shell arguments, as in `{}`; zsh stops a command at an unmatched glob.",
            glob_example(get("example")).1
        ),
        Class::ZshEquals => "Quote shell words that start with `=`, as in `echo '=== done'`; zsh expands them as commands.".to_owned(),
        Class::WrongDirectory => {
            let (program, from, to, manifest) =
                (get("program"), get("from"), get("to"), get("manifest"));
            safe(&[program, manifest]).then_some(())?;
            if to.is_empty() {
                safe(&[from]).then_some(())?;
                format!("Run `{program}` from the repository root, not `{from}/`.")
            } else if from.is_empty() {
                safe(&[to]).then_some(())?;
                format!("Run `{program}` from `{to}/`; the repository root has no {manifest}.")
            } else {
                safe(&[from, to]).then_some(())?;
                format!("Run `{program}` from `{to}/`; `{from}/` has no {manifest}.")
            }
        }
        Class::UnknownTarget => {
            let (stem, x, y) = (get("stem"), get("name"), get("replacement"));
            safe(&[stem, x, y]).then_some(())?;
            match get("kind") {
                "package" => {
                    let flag = if get("flag").is_empty() { "-p" } else { get("flag") };
                    safe(&[flag]).then_some(())?;
                    format!("Use `{stem} {flag} {y}`; there is no package `{x}`.")
                }
                "target" => {
                    let (flag, kind) = (get("flag"), get("target_kind"));
                    safe(&[flag, kind]).then_some(())?;
                    format!("Use `{stem} {flag} {y}`; there is no {kind} target `{x}`.")
                }
                "script" => format!("Run `npm run {y}`; there is no `{x}` script."),
                "make" => format!("Run `make {y}`; there is no `{x}` target."),
                _ => format!("Use `{stem} {y}`, not `{stem} {x}`; `{stem} {x}` is not available here."),
            }
        }
        Class::UnknownOption => {
            let (stem, flag, replacement) = (get("stem"), get("flag"), get("replacement"));
            safe(&[stem, flag]).then_some(())?;
            if replacement.is_empty() {
                format!("`{stem}` here does not accept `{flag}`.")
            } else {
                safe(&[replacement]).then_some(())?;
                format!("`{stem}` here does not accept `{flag}`; use `{replacement}`.")
            }
        }
        Class::MissingOption => {
            let (stem, flag) = (get("stem"), get("flag"));
            safe(&[stem, flag]).then_some(())?;
            format!("Pass `{flag}` to `{stem}`.")
        }
        Class::WrongTestPath => {
            let (stem, from, to) = (get("stem"), get("from"), get("to"));
            safe(&[stem, from, to]).then_some(())?;
            format!("`{stem}` tests are under `{to}/`, not `{from}/`.")
        }
        Class::EmptyTests => {
            let (stem, fix, from) = (get("stem"), get("fix"), get("from"));
            safe(&[stem, fix]).then_some(())?;
            let place = if from.is_empty() {
                "at the repository root".to_owned()
            } else {
                safe(&[from]).then_some(())?;
                format!("in `{from}/`")
            };
            if get("form") == "dir" {
                format!("Run `{stem}` from `{fix}/`; {place} it runs no tests.")
            } else if from.is_empty() {
                format!("Run tests with `{stem} {fix}`; plain `{stem}` here runs no tests.")
            } else {
                format!("Run tests with `{stem} {fix}`; plain `{stem}` {place} runs no tests.")
            }
        }
        Class::WrongPackageManager => {
            let (wrong, right) = (get("from"), get("to"));
            safe(&[wrong, right]).then_some(())?;
            match check {
                Check::Verified(detail) if detail.ends_with("packageManager") => format!(
                    "Use `{right}`, not `{wrong}`; package.json names {right} in packageManager."
                ),
                Check::Verified(detail) => {
                    let lockfile = basename(detail);
                    format!("Use `{right}`, not `{wrong}`; this project has {lockfile}.")
                }
                _ => format!("Use `{right}`, not `{wrong}`, in this project."),
            }
        }
    };
    privacy::safe_rule_text(&text).then_some(text)
}

/// A short wrong and right pair from the rule's slots, never from raw commands.
pub fn example(
    class: Class,
    slots: &BTreeMap<String, String>,
) -> Option<(String, String, &'static str)> {
    let get = |name: &str| slots.get(name).map_or("", String::as_str);
    let within = |dir: &str| {
        if dir.is_empty() {
            " (in the repository root)".to_owned()
        } else {
            format!(" (in {dir}/)")
        }
    };
    Some(match class {
        Class::MissingProgram | Class::ZshEquals => return None,
        Class::ZshGlob => {
            let (wrong, right) = glob_example(get("example"));
            (wrong, right, "zsh: no matches found")
        }
        Class::WrongDirectory => {
            let command = if get("command").is_empty() {
                get("program")
            } else {
                get("command")
            };
            if !command.split(' ').all(privacy::safe_token) {
                return None;
            }
            let wrong = if get("from").is_empty() {
                command.to_owned()
            } else {
                format!("{command}{}", within(get("from")))
            };
            let signal = match get("manifest") {
                "Makefile" => "make: no makefile",
                "Cargo.toml" => "cargo: no Cargo.toml",
                "package.json" => "npm: no package.json",
                _ => "uv: no pyproject.toml",
            };
            (wrong, format!("{command}{}", within(get("to"))), signal)
        }
        Class::UnknownTarget => {
            let (stem, x, y, flag) = (get("stem"), get("name"), get("replacement"), get("flag"));
            match get("kind") {
                "package" | "target" => {
                    let flag = if flag.is_empty() { "-p" } else { flag };
                    let signal = if get("kind") == "package" {
                        "cargo: unknown package"
                    } else {
                        "cargo: unknown target"
                    };
                    (
                        format!("{stem} {flag} {x}"),
                        format!("{stem} {flag} {y}"),
                        signal,
                    )
                }
                "script" => (
                    format!("npm run {x}"),
                    format!("npm run {y}"),
                    "npm: missing script",
                ),
                "make" => (
                    format!("make {x}"),
                    format!("make {y}"),
                    "make: no rule to make target",
                ),
                _ => (
                    format!("{stem} {x}"),
                    format!("{stem} {y}"),
                    "unknown subcommand",
                ),
            }
        }
        Class::UnknownOption => {
            let (stem, flag, replacement) = (get("stem"), get("flag"), get("replacement"));
            let right = if replacement.is_empty() {
                stem.to_owned()
            } else {
                format!("{stem} {replacement}")
            };
            (format!("{stem} {flag}"), right, "unknown option")
        }
        Class::MissingOption => (
            get("stem").to_owned(),
            format!("{} {}", get("stem"), get("flag")),
            "missing option",
        ),
        Class::WrongTestPath => (
            format!("{} {}/…", get("stem"), get("from")),
            format!("{} {}/…", get("stem"), get("to")),
            "test path not found",
        ),
        Class::EmptyTests => {
            let right = if get("form") == "dir" {
                format!("{}{}", get("stem"), within(get("fix")))
            } else {
                format!("{} {}", get("stem"), get("fix"))
            };
            (get("stem").to_owned(), right, "no tests ran")
        }
        Class::WrongPackageManager => (
            get("from").to_owned(),
            get("to").to_owned(),
            "package manager",
        ),
    })
}

/// Recency-weighted sessions: each session counts 0.5^(age/14) by its last failure.
pub fn score(candidate: &Candidate, today: u64) -> f64 {
    let score: f64 = candidate
        .sessions
        .values()
        .map(|last| {
            let age = today.saturating_sub(last / 86_400_000);
            0.5f64.powf(age as f64 / 14.0)
        })
        .sum();
    (score * 100.0).round() / 100.0
}

/// Whether one confirmed correction is enough for this rule.
pub fn single_correction(candidate: &Candidate) -> bool {
    candidate.corrections > 0
        && match candidate.class {
            Class::MissingProgram => {
                candidate
                    .slots
                    .get("kind")
                    .is_some_and(|kind| kind == "program")
                    && candidate.slots.contains_key("alternative")
            }
            Class::WrongDirectory | Class::WrongPackageManager => true,
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_typos() {
        assert_eq!(distance("ammend", "amend"), 1);
        assert_eq!(distance("tset", "test"), 1);
        assert!(typo("ammend", "amend"));
        assert!(!typo("-n", "--last"));
        assert!(!typo("test", "test:unit"));
    }

    #[test]
    fn names_stems_and_word_changes() {
        let words = |text: &str| -> Vec<String> { text.split(' ').map(str::to_owned).collect() };
        assert_eq!(stem(&words("./tool.py -n 4")), "./tool.py");
        assert_eq!(stem(&words("python3 -m pytest tests")), "pytest");
        assert_eq!(stem(&words("uv run pytest api")), "pytest");
        assert_eq!(stem(&words("cargo test -p core")), "cargo test");
        assert_eq!(
            added_words(&words("cargo test"), &words("cargo test -p core")),
            Some(words("-p core"))
        );
        assert_eq!(
            added_words(&words("cargo test"), &words("cargo test")),
            None
        );
        assert_eq!(canonical(&words("npm test")), words("npm run test"));
        assert!(contains_word("try --set-upstream origin", "--set-upstream"));
        assert!(!contains_word("try --set-upstream-to", "--set-upstream"));
        assert_eq!(
            unquoted_words("echo '===' ===== \"a b\""),
            ["echo", "====="]
        );
    }

    #[test]
    fn reads_every_signature_line() {
        let cases: &[(&str, Hit)] = &[
            (
                "make: *** No targets specified and no makefile found.  Stop.",
                Hit::NoMakefile,
            ),
            (
                "make: *** No rule to make target `lint'.  Stop.",
                Hit::NoRule("lint".into()),
            ),
            (
                "make: *** No rule to make target 'lint'.  Stop.",
                Hit::NoRule("lint".into()),
            ),
            (
                "error: could not find `Cargo.toml` in `/x` or any parent directory",
                Hit::NoCargoToml,
            ),
            (
                "npm error code ENOENT\nnpm error path /x/package.json",
                Hit::NoPackageJson,
            ),
            (
                "error: No `pyproject.toml` found in current directory or any parent directory",
                Hit::NoPyproject,
            ),
            (
                "error: package ID specification `core` did not match any packages",
                Hit::CargoPackage("core".into()),
            ),
            (
                "error: no test target named `api`.",
                Hit::CargoTarget("test", "api".into()),
            ),
            (
                "error: no such command: `nextest`",
                Hit::NoSuchCommand("nextest".into()),
            ),
            (
                "git: 'stauts' is not a git command. See 'git --help'.",
                Hit::NoSuchCommand("stauts".into()),
            ),
            (
                "npm error Missing script: \"test\"",
                Hit::MissingScript("test".into()),
            ),
            (
                "error: unexpected argument '--nope' found",
                Hit::UnknownOption("--nope".into()),
            ),
            (
                "error: Found argument '--nope' which wasn't expected, or isn't valid in this context",
                Hit::UnknownOption("--nope".into()),
            ),
            (
                "error: unknown option `ammend'",
                Hit::UnknownOption("--ammend".into()),
            ),
            ("error: unknown switch `x'", Hit::UnknownOption("-x".into())),
            (
                "fatal: unrecognized argument: --nope",
                Hit::UnknownOption("--nope".into()),
            ),
            (
                "ls: unrecognized option `--include=*.zz'",
                Hit::UnknownOption("--include".into()),
            ),
            (
                "head: invalid option -- 'z'",
                Hit::UnknownOption("-z".into()),
            ),
            ("cut: illegal option -- z", Hit::UnknownOption("-z".into())),
            (
                "tool.py: error: unrecognized arguments: -n",
                Hit::UnknownOption("-n".into()),
            ),
            (
                "tool.py: error: the following arguments are required: --x",
                Hit::RequiredOption("--x".into()),
            ),
            (
                "ERROR: file or directory not found: tests/test_api.py",
                Hit::TestPath("tests/test_api.py".into()),
            ),
            (
                "ERROR: not found: tests/test_api.py::test_x",
                Hit::TestPath("tests/test_api.py::test_x".into()),
            ),
            (
                "Could not find 'tests/x.test.js'",
                Hit::TestPath("tests/x.test.js".into()),
            ),
            (
                "npm error Unsupported URL Type \"workspace:\": workspace:*",
                Hit::WorkspaceProtocol,
            ),
            (
                "Usage Error: This project is configured to use pnpm because /x/package.json has a \"packageManager\" field",
                Hit::Corepack("pnpm".into()),
            ),
        ];
        for (line, expected) in cases {
            assert!(hits(line).contains(expected), "{line}: {:?}", hits(line));
        }
        assert!(hits("head: invalid option -- '-'").is_empty());
    }

    #[test]
    fn reads_every_self_evident_line() {
        let missing = |line: &str| -> Vec<(&'static str, String)> {
            MISSING
                .iter()
                .flat_map(|(pattern, kind)| {
                    pattern
                        .captures_iter(line)
                        .map(|captures| (kind.name(), captures[1].to_owned()))
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        for (line, kind, operand) in [
            ("zsh:1: command not found: gh", "program", "gh"),
            ("zsh: command not found: gh", "program", "gh"),
            ("(eval):1: command not found: gh", "program", "gh"),
            ("/bin/sh: gh: command not found", "program", "gh"),
            ("bash: line 3: gh: command not found", "program", "gh"),
            ("/bin/dash: 1: gh: not found", "program", "gh"),
            ("sh: 1: gh: not found", "program", "gh"),
            (
                "zsh:1: no such file or directory: ./gradlew",
                "relative",
                "./gradlew",
            ),
            (
                "justless: run gh: No such file or directory (os error 2)",
                "program",
                "gh",
            ),
            (
                "/Library/Developer/CommandLineTools/usr/bin/python3: No module named pytest",
                "module",
                "pytest",
            ),
        ] {
            assert_eq!(missing(line), [(kind, operand.to_owned())], "{line}");
        }
        assert!(missing("ModuleNotFoundError: No module named 'requests'").is_empty());
        let glob = GLOB
            .captures("zsh:1: no matches found: --include=*.zz")
            .unwrap();
        assert_eq!(&glob[1], "--include=*.zz");
        let equals = EQUALS.captures("zsh:1: ==== not found").unwrap();
        assert_eq!(&equals[1], "====");
        assert_eq!(glob_example("--include=*.zz").1, "--include='*.zz'");
        assert_eq!(glob_example("*.py").1, "'*.py'");
        assert_eq!(glob_example("src/**/x").1, "'*.txt'");
    }
}
