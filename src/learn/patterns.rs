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
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use super::events::{self, Cmd, Event, Pointer, Runner, Status, basename};
use super::privacy;

/// A correction must follow within this many events of the thread and this long.
const WINDOW_EVENTS: usize = 6;
const WINDOW_MS: u64 = 15 * 60 * 1000;
const MANAGERS: [&str; 4] = ["npm", "pnpm", "yarn", "bun"];
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
    PrerequisiteStep,
    ToolchainWrapper,
}

impl Class {
    const ALL: [Self; 12] = [
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
        Self::PrerequisiteStep,
        Self::ToolchainWrapper,
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
            Self::PrerequisiteStep => "prerequisite_step",
            Self::ToolchainWrapper => "toolchain_wrapper",
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
            Self::PrerequisiteStep => "prerequisite step",
            Self::ToolchainWrapper => "toolchain wrapper",
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

/// Self-evident hits, each with the bytes of the error line that shows it.
fn self_evident(event: &Event) -> Vec<(SelfHit, u64)> {
    let mut hits = Vec::new();
    let text = event.transcript_text();
    if !event.transcript || event.status == Status::Unknown {
        return hits;
    }
    let maybe_missing = text.contains("not found")
        || text.contains("no such file or directory")
        || text.contains("No such file or directory")
        || text.contains("No module named");
    let line = |found: regex::Match| found.as_str().len() as u64 + 1;
    if maybe_missing {
        let words: Vec<&str> = event.raw.split_whitespace().collect();
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
                        |(hit, _)| matches!(hit, SelfHit::Missing(k, o) if k == kind && *o == operand),
                    )
                {
                    hits.push((
                        SelfHit::Missing(*kind, operand),
                        line(captures.get(0).expect("match")),
                    ));
                }
            }
        }
    }
    if text.contains("no matches found")
        && let Some(captures) = GLOB
            .captures_iter(text)
            .find(|captures| event.raw.contains(&captures[1]))
    {
        let bytes = line(captures.get(0).expect("match"));
        hits.push((SelfHit::Glob(captures[1].to_owned()), bytes));
    }
    if text.contains(" not found") {
        let mut eq = EQUALS.captures_iter(text).peekable();
        if eq.peek().is_some() {
            let unquoted = unquoted_words(&event.raw);
            if let Some(captures) = eq.find(|captures| {
                unquoted
                    .iter()
                    .any(|word| word.strip_prefix('=') == Some(&captures[1]))
            }) {
                hits.push((SelfHit::Equals, line(captures.get(0).expect("match"))));
            }
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
    NoGoMod,
    NoPubspecYaml,
    NoTerraformConfig,
    NoDockerfile,
    NoChartYaml,
    CargoPackage(String),
    CargoTarget(&'static str, String),
    NoSuchCommand(String),
    MissingScript(String),
    UnknownOption(String),
    RequiredOption(String),
    TestPath(String),
    WorkspaceProtocol,
    Corepack(String),
    FlutterPubGet,
    DartCodegen,
    FlutterSdkMismatch,
    TerraformUpgrade,
    TerraformInit,
    TerraformStateLock,
    TerraformWorkspace(String),
    TerraformWorkspaceError,
    HelmDeps,
    KubectlNotFound,
    KubectlEmptyNamespace(String),
    KubectlContext,
    DockerDaemon,
    DockerBuildx,
    DockerBuildFailed,
    UvSync,
    ModuleNotFound,
    UvPackage(String),
    PnpmFilter,
}

impl Hit {
    fn manifest(&self) -> Option<&'static str> {
        match self {
            Self::NoMakefile | Self::NoRule(_) => Some("Makefile"),
            Self::NoCargoToml => Some("Cargo.toml"),
            Self::NoPackageJson => Some("package.json"),
            Self::NoPyproject => Some("pyproject.toml"),
            Self::NoGoMod => Some("go.mod"),
            Self::NoPubspecYaml => Some("pubspec.yaml"),
            Self::NoTerraformConfig => Some(".tf"),
            Self::NoDockerfile => Some("Dockerfile"),
            Self::NoChartYaml => Some("Chart.yaml"),
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
    no_go_mod: Regex,
    no_pubspec_yaml: Regex,
    no_terraform_config: Regex,
    no_dockerfile: Regex,
    no_chart_yaml: Regex,
    cargo_package: Regex,
    cargo_target: Regex,
    cargo_command: Regex,
    git_command: Regex,
    missing_script: Vec<Regex>,
    options: Vec<Regex>,
    git_option: Regex,
    getopt: Regex,
    required: Regex,
    pytest_path: Regex,
    node_path: Regex,
    workspace: Regex,
    corepack: Regex,
    flutter_pub_get: Regex,
    dart_codegen: Regex,
    flutter_sdk_mismatch: Regex,
    terraform_upgrade: Regex,
    terraform_init: Regex,
    terraform_state_lock: Regex,
    terraform_workspace: Regex,
    terraform_workspace_error: Regex,
    helm_deps: Regex,
    kubectl_not_found: Regex,
    kubectl_empty_ns: Regex,
    kubectl_context: Regex,
    docker_daemon: Regex,
    docker_buildx: Regex,
    docker_build_failed: Regex,
    uv_sync: Regex,
    module_not_found: Regex,
    uv_package: Regex,
    pnpm_filter: Regex,
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
        // go [here]: `go: go.mod file not found in current directory or any parent directory; see 'go help modules'`,
        // and `go test ./...` / `go build ./...` outside a module:
        // `pattern ./...: directory prefix . does not contain main module or its selected dependencies`
        no_go_mod: re(
            r"(?m)(?:go\.mod file not found in current directory or any parent directory|directory prefix \. does not contain main module or its selected dependencies)",
        ),
        // flutter/dart [source]
        no_pubspec_yaml: re(
            r#"(?m)(?:No pubspec\.yaml file found|Could not find a file named "pubspec\.yaml")"#,
        ),
        // terraform / opentofu [source]
        no_terraform_config: re(
            r"(?m)(?:Error: No configuration files|(?:contains|has) no (?:Terraform|OpenTofu) configuration files)",
        ),
        // docker / buildkit [source]
        no_dockerfile: re(
            r"(?m)(?:failed to read dockerfile|unable to evaluate symlinks in Dockerfile path|open Dockerfile: no such file or directory)",
        ),
        // helm [source]
        no_chart_yaml: re(r"(?m)Chart\.yaml file is missing"),
        // cargo [here]
        cargo_package: re(r"(?m)package ID specification `([^`]+)` did not match any packages"),
        cargo_target: re(r"(?m)no (bin|test|example|bench) target named `([^`]+)`"),
        cargo_command: re(r"(?m)no such command: `([^`]+)`"),
        // git [here]
        git_command: re(r"(?m)git: '([^']+)' is not a git command"),
        missing_script: vec![
            // npm & pnpm [here]: `npm error Missing script: "test"`,
            // `ERR_PNPM_NO_SCRIPT  Missing script: nope`
            re(r#"(?m)Missing script: "?([^"\s]+)"?"#),
            // yarn v1 & pnpm [here]: `error Command "nope" not found.`,
            // `ERR_PNPM_RECURSIVE_EXEC_FIRST_FAIL  Command "nope" not found`
            re(r#"(?m)Command "([^"\s]+)" not found\.?"#),
            // yarn berry [source]: `Usage Error: Couldn't find a script named "nope".`
            re(r#"(?m)Couldn't find a script named "([^"\s]+)""#),
            // bun [source]: `error: Script not found "nope"`
            re(r#"(?m)Script not found "([^"\s]+)""#),
        ],
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
        corepack: re(r"(?m)This project is configured to use (npm|pnpm|yarn|bun) because"),
        // flutter/dart pub get [source: dart pub & analyzer]
        flutter_pub_get: re(
            r#"(?m)(?:please run ["'](?:flutter|dart) pub get["']|pubspec\.yaml has been modified since the last 'pub get'|Target of URI doesn't exist: 'package:|Couldn't resolve the package '|package_config\.json|pub get has not been run)"#,
        ),
        // dart/flutter build_runner codegen [source: analyzer & CFE]
        dart_codegen: re(
            r"(?m)(?:Target of URI hasn't been generated:|Error when reading '[^']*\.(?:g|freezed|mocks|gr)\.dart')",
        ),
        // flutter/dart SDK version constraint mismatch [source: pub & analyzer]
        flutter_sdk_mismatch: re(
            r"(?m)(?:requires (?:Flutter |a higher minimum Dart |a Dart )?SDK version|The current (?:Dart|Flutter) SDK version is|language version override can't specify a version greater than)",
        ),
        // terraform/opentofu provider lock upgrade [source]
        terraform_upgrade: re(
            r"(?m)(?:Inconsistent dependency lock file|(?:terraform|tofu|terragrunt) init -upgrade|does not match configured version constraint)",
        ),
        // terraform/opentofu/terragrunt initialization required [source]
        terraform_init: re(
            r#"(?m)(?:Initialization required|Plugin reinitialization required|Module not installed|Backend initialization required|Backend configuration changed|Required plugins are not installed|run "(?:terraform|tofu|terragrunt) init")"#,
        ),
        // terraform/opentofu state lock [source]
        terraform_state_lock: re(r"(?m)(?:Error acquiring the state lock|Error locking state:)"),
        // terraform/opentofu unknown workspace [source]
        terraform_workspace: re(r#"(?m)Workspace "([^"]+)" doesn't exist"#),
        // terraform/opentofu wrong active workspace [source]
        terraform_workspace_error: re(
            r#"(?m)(?:currently selected workspace|not allowed in the "default" workspace|cannot be used with the "default" workspace)"#,
        ),
        // helm missing chart dependencies [source]
        helm_deps: re(
            r"(?m)(?:found in Chart\.yaml, but missing in charts/|helm dependency (?:update|build)|Chart\.lock is out of sync)",
        ),
        // kubectl NotFound [source]
        kubectl_not_found: re(r"(?m)Error from server \(NotFound\):"),
        // kubectl empty namespace (often exit 0) [source]
        kubectl_empty_ns: re(r"(?m)No resources found in ([A-Za-z0-9._-]+) namespace\."),
        // kubectl unreachable / unset context [source]
        kubectl_context: re(
            r#"(?m)(?:connection to the server localhost:8080 was refused|current-context is not set|context "[^"]+" does not exist|Unable to connect to the server:)"#,
        ),
        // docker daemon / context unreachable [source]
        docker_daemon: re(
            r"(?m)(?:Cannot connect to the Docker daemon|Is the docker daemon running\?|failed to connect to the docker (?:API|daemon))",
        ),
        // docker buildx required [source]
        docker_buildx: re(
            r"(?m)(?:Multi-platform build is not supported for the docker driver|Legacy builder is deprecated)",
        ),
        // docker BuildKit build failure [source]
        docker_build_failed: re(
            r"(?m)(?:ERROR: failed to solve:|ERROR: failed to build:|failed to compute cache key:|invalid reference format|missing build-arg|required build arg)",
        ),
        // uv lockfile / venv out of sync [source]
        uv_sync: re(
            r"(?m)(?:The lockfile at `uv\.lock` needs to be updated|No virtual environment found; run `uv venv`)",
        ),
        // python missing module (only pairs with `uv run` or `uv sync`) [here]
        module_not_found: re(r"(?m)ModuleNotFoundError: No module named '[^']+'"),
        // uv unknown workspace package [source]
        uv_package: re(r"(?m)Package `([^`]+)` not found in workspace"),
        // pnpm workspace filter mismatch [source]
        pnpm_filter: re(r"(?m)No projects matched the filters"),
    }
});

/// Words at least one of which every signature and self-evident error contains. A
/// text without any of them can match neither, so its body need not be kept.
static SIGNALS: LazyLock<Regex> = LazyLock::new(|| {
    let words = [
        "not found",
        "no such file or directory",
        "No such file or directory",
        "No module named",
        "no matches found",
        "no makefile found",
        "No rule to make target",
        "Cargo.toml",
        "ENOENT",
        "pyproject.toml",
        "go.mod",
        "does not contain main module",
        "pubspec.yaml",
        "No configuration files",
        "configuration files",
        "failed to read dockerfile",
        "Dockerfile path",
        "Chart.yaml",
        "Chart.lock",
        "did not match any packages",
        "target named",
        "no such command",
        "is not a git command",
        "Missing script",
        "Couldn't find a script named",
        "unexpected argument",
        "which wasn't expected",
        "unrecognized argument",
        "unrecognized option",
        "unknown option",
        "unknown switch",
        "invalid option",
        "illegal option",
        "arguments are required",
        "Could not find",
        "Unsupported URL Type",
        "is configured to use",
        "pub get",
        "Target of URI doesn't exist",
        "Couldn't resolve the package",
        "package_config.json",
        "hasn't been generated",
        "Error when reading '",
        "SDK version",
        "language version override",
        "Inconsistent dependency lock file",
        "init -upgrade",
        "configured version constraint",
        "Initialization required",
        "Plugin reinitialization required",
        "Module not installed",
        "Backend initialization required",
        "Backend configuration changed",
        "Required plugins are not installed",
        "terraform init",
        "tofu init",
        "terragrunt init",
        "state lock",
        "Error locking state",
        "doesn't exist",
        "selected workspace",
        "\"default\" workspace",
        "helm dependency",
        "Error from server (NotFound)",
        "No resources found in ",
        "localhost:8080 was refused",
        "current-context is not set",
        "does not exist",
        "Unable to connect to the server",
        "Cannot connect to the Docker daemon",
        "docker daemon running",
        "connect to the docker",
        "not supported for the docker driver",
        "Legacy builder is deprecated",
        "failed to solve:",
        "failed to build:",
        "failed to compute cache key",
        "invalid reference format",
        "build-arg",
        "required build arg",
        "uv.lock",
        "No virtual environment found",
        "ModuleNotFoundError",
        "not found in workspace",
        "No projects matched the filters",
    ];
    let pattern = words.map(regex::escape).join("|");
    Regex::new(&pattern).expect("signal words")
});

pub fn may_signal(text: &str) -> bool {
    SIGNALS.is_match(text)
}

fn hits(text: &str) -> Vec<Hit> {
    if !may_signal(text) {
        return Vec::new();
    }
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
    if s.no_go_mod.is_match(text) {
        add(Hit::NoGoMod);
    }
    if s.no_pubspec_yaml.is_match(text) {
        add(Hit::NoPubspecYaml);
    }
    if s.no_terraform_config.is_match(text) {
        add(Hit::NoTerraformConfig);
    }
    if s.no_dockerfile.is_match(text) {
        add(Hit::NoDockerfile);
    }
    if s.no_chart_yaml.is_match(text) {
        add(Hit::NoChartYaml);
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
    for pattern in &s.missing_script {
        for captures in pattern.captures_iter(text) {
            add(Hit::MissingScript(captures[1].to_owned()));
        }
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
    if s.dart_codegen.is_match(text) {
        add(Hit::DartCodegen);
    }
    if s.flutter_pub_get.is_match(text) {
        add(Hit::FlutterPubGet);
    }
    if s.flutter_sdk_mismatch.is_match(text) {
        add(Hit::FlutterSdkMismatch);
    }
    if s.terraform_upgrade.is_match(text) {
        add(Hit::TerraformUpgrade);
    }
    if s.terraform_init.is_match(text) {
        add(Hit::TerraformInit);
    }
    if s.terraform_state_lock.is_match(text) {
        add(Hit::TerraformStateLock);
    }
    for captures in s.terraform_workspace.captures_iter(text) {
        add(Hit::TerraformWorkspace(captures[1].to_owned()));
    }
    if s.terraform_workspace_error.is_match(text) {
        add(Hit::TerraformWorkspaceError);
    }
    if s.helm_deps.is_match(text) {
        add(Hit::HelmDeps);
    }
    if s.kubectl_not_found.is_match(text) {
        add(Hit::KubectlNotFound);
    }
    for captures in s.kubectl_empty_ns.captures_iter(text) {
        add(Hit::KubectlEmptyNamespace(captures[1].to_owned()));
    }
    if s.kubectl_context.is_match(text) {
        add(Hit::KubectlContext);
    }
    if s.docker_daemon.is_match(text) {
        add(Hit::DockerDaemon);
    }
    if s.docker_buildx.is_match(text) {
        add(Hit::DockerBuildx);
    }
    if s.docker_build_failed.is_match(text) {
        add(Hit::DockerBuildFailed);
    }
    if s.uv_sync.is_match(text) {
        add(Hit::UvSync);
    }
    if s.module_not_found.is_match(text) {
        add(Hit::ModuleNotFound);
    }
    for captures in s.uv_package.captures_iter(text) {
        add(Hit::UvPackage(captures[1].to_owned()));
    }
    if s.pnpm_filter.is_match(text) {
        add(Hit::PnpmFilter);
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

    fn fail(&mut self, at: (usize, usize), event: &Event, pointer: Pointer, bytes: u64) {
        if !self.counted.insert(at) {
            return;
        }
        self.failures += 1;
        let last = self.sessions.entry(event.session.clone()).or_default();
        *last = (*last).max(event.time_ms);
        self.threads.insert(event.thread.clone());
        self.failure_bytes += bytes;
        self.first_ms = self.first_ms.min(event.time_ms);
        self.last_ms = self.last_ms.max(event.time_ms);
        self.transcript |= event.transcript && !event.codex;
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
    wrong: Option<Wrong>,
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
        let hits = self_evident(event);
        if hits.is_empty() {
            return;
        }
        // A command with a credential teaches nothing, and a remote shell's error
        // describes another machine. `PATH` and zsh do not depend on the directory, so
        // a path outside the workspace is no reason to drop these; a missing relative
        // path needs a parsed command, whose directory is always inside the root.
        if privacy::carries_credential(&event.raw, &event.env) {
            self.credentials += 1;
            return;
        }
        // A simple failed command cost its whole result; in a list or an exit-0
        // result, only the error line and the exit header are the mistake's.
        let whole = event.simple().is_some() && event.status != Status::Code(0);
        let header = match event.status {
            Status::Code(code) if code != 0 => format!("Exit code {code}\n").len() as u64,
            _ => 0,
        };
        for (hit, line) in hits {
            let bytes = if whole {
                event.failure_bytes
            } else {
                (line + header).min(event.failure_bytes)
            };
            match hit {
                SelfHit::Missing(kind, operand) => {
                    let failed = failed_piece(event, kind, &operand);
                    // A missing relative path is checked in the directory that ran it,
                    // which only a parsed command shows.
                    if kind == Missing::Relative && failed.is_none() {
                        continue;
                    }
                    let python = failed
                        .filter(|_| kind == Missing::Module)
                        .map(|cmd| basename(&cmd.argv[0]).to_owned());
                    let dir = failed.map(|cmd| cmd.dir.clone()).unwrap_or_default();
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
                    candidate.fail(place, event, event.pointer.clone(), bytes);
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
                    candidate.fail(place, event, event.pointer.clone(), bytes);
                    candidate.vote("example", &operand);
                }
                SelfHit::Equals => {
                    self.entry(Class::ZshEquals, &[]).fail(
                        place,
                        event,
                        event.pointer.clone(),
                        bytes,
                    );
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
        if code == 0 && !f.masked {
            // `kubectl get` exits 0 when printing "No resources found in <ns> namespace.".
            if basename(f.program()) == "kubectl" {
                let hits = thread.hits(at);
                if let Some((hit, pointer)) = hits
                    .iter()
                    .find(|(hit, _)| matches!(hit, Hit::KubectlEmptyNamespace(_)))
                    .cloned()
                    && let Some((g, outcome)) = find(thread, at, false, |g_event, g| {
                        if thread_has(thread, g_event, &hit) {
                            return None;
                        }
                        kubectl_option(&hit, f, g)
                    })
                {
                    self.record(thread, at, g, outcome, pointer, Some(&hit));
                }
            }
            return;
        }
        if query_exit(f, code) {
            return;
        }
        if thread.hits(at).is_empty() {
            if code != 0 && !test_run(f, event) {
                // Any failure text that names the flag the correction adds. A failed test
                // run is left out: cargo's `to rerun pass --lib` is not a missing option.
                let found = find(thread, at, false, |_, g| missing_option(f, g, None, event));
                if let Some((g, outcome)) = found {
                    self.record(thread, at, g, outcome, event.pointer.clone(), None);
                }
            }
            return;
        }
        let hits = thread.hits(at).to_vec();
        for (hit, pointer) in &hits {
            let direct = find(thread, at, false, |g_event, g| {
                if g.masked && thread_has(thread, g_event, hit) {
                    return None;
                }
                pair(hit, event, f, code, g)
            });
            let prereq = find_prereq(thread, at, hit, f);
            let found = match (direct, prereq) {
                (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            };
            if let Some((g, outcome)) = found {
                self.record(thread, at, g, outcome, pointer.clone(), Some(hit));
                return;
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
        candidate.fail((thread.index, at), event, pointer, event.failure_bytes);
        candidate.correct((thread.index, g));
        candidate.context(event.time_ms, &outcome.context);
        if let Some(wrong) = outcome.wrong {
            candidate.expect(wrong);
        }
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
            candidate.fail(
                (thread.index, before),
                earlier,
                pointer,
                earlier.failure_bytes,
            );
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

/// The first later successful prerequisite command (standalone or inside a chain)
/// in the same directory within the window, provided an immediate retry of `f` did
/// not fail again with the same hit.
fn find_prereq(thread: &Thread, at: usize, hit: &Hit, f: &Cmd) -> Option<(usize, Outcome)> {
    if !matches!(
        hit,
        Hit::FlutterPubGet
            | Hit::DartCodegen
            | Hit::TerraformUpgrade
            | Hit::TerraformInit
            | Hit::TerraformWorkspace(_)
            | Hit::TerraformWorkspaceError
            | Hit::HelmDeps
            | Hit::KubectlContext
            | Hit::DockerDaemon
            | Hit::UvSync
            | Hit::ModuleNotFound
    ) {
        return None;
    }
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
        if event.status != Status::Code(0) {
            continue;
        }
        for cmd in event.commands() {
            let Some(outcome) = prerequisite_step(hit, f, cmd) else {
                continue;
            };
            // Reject the prerequisite if `f` immediately fails again with the same hit.
            let still_broken = thread.events[later + 1..]
                .iter()
                .enumerate()
                .take(WINDOW_EVENTS)
                .any(|(offset, retry)| {
                    retry.time_ms.saturating_sub(event.time_ms) <= WINDOW_MS
                        && retry.status != Status::Code(0)
                        && retry.status != Status::Unknown
                        && retry
                            .simple()
                            .is_some_and(|c| c.argv == f.argv && c.same_place(f))
                        && thread.has(later + 1 + offset, hit)
                });
            if !still_broken {
                return Some((later, outcome));
            }
        }
    }
    None
}

fn pair(hit: &Hit, event: &Event, f: &Cmd, code: i32, g: &Cmd) -> Option<Outcome> {
    match hit {
        Hit::NoMakefile
        | Hit::NoCargoToml
        | Hit::NoPackageJson
        | Hit::NoPyproject
        | Hit::NoGoMod
        | Hit::NoPubspecYaml
        | Hit::NoTerraformConfig
        | Hit::NoChartYaml => wrong_directory(hit, f, g),
        Hit::NoDockerfile => wrong_directory(hit, f, g).or_else(|| docker_option(hit, f, g)),
        Hit::NoRule(_) => wrong_directory(hit, f, g).or_else(|| unknown_target(hit, f, g)),
        Hit::CargoPackage(_)
        | Hit::CargoTarget(..)
        | Hit::NoSuchCommand(_)
        | Hit::MissingScript(_)
        | Hit::TerraformWorkspace(_)
        | Hit::UvPackage(_)
        | Hit::PnpmFilter => unknown_target(hit, f, g),
        Hit::UnknownOption(flag) => unknown_option(flag, f, g),
        Hit::RequiredOption(flag) => missing_option(f, g, Some(flag), event),
        Hit::TestPath(path) => wrong_test_path(path, f, code, g),
        Hit::WorkspaceProtocol | Hit::Corepack(_) => wrong_package_manager(hit, f, g),
        Hit::FlutterSdkMismatch | Hit::ModuleNotFound | Hit::DockerBuildx => {
            toolchain_wrapper(hit, f, g)
        }
        Hit::TerraformUpgrade | Hit::TerraformStateLock => terraform_option(hit, f, g),
        Hit::KubectlNotFound | Hit::KubectlEmptyNamespace(_) | Hit::KubectlContext => {
            kubectl_option(hit, f, g)
        }
        Hit::DockerDaemon | Hit::DockerBuildFailed => docker_option(hit, f, g),
        Hit::FlutterPubGet
        | Hit::DartCodegen
        | Hit::TerraformInit
        | Hit::TerraformWorkspaceError
        | Hit::HelmDeps
        | Hit::UvSync => None,
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
        wrong: Some(Wrong::Command {
            dir: f.dir.clone(),
            argv: f.argv.clone(),
        }),
        typo: false,
        nested,
    })
}

/// Split a package-manager script invocation (`npm run X ...`, `npm test ...`,
/// or `pnpm|yarn|bun [run] X ...`) into `(manager, script, trailing_args)`.
fn script_invocation(argv: &[String]) -> Option<(&str, &str, &[String])> {
    let manager = basename(argv.first()?.as_str());
    if !MANAGERS.contains(&manager) {
        return None;
    }
    match argv.get(1).map(String::as_str) {
        Some("run") => {
            let script = argv.get(2)?.as_str();
            (!script.is_empty() && !script.starts_with('-')).then(|| (manager, script, &argv[3..]))
        }
        Some(word) if manager == "npm" => matches!(word, "test" | "start" | "stop" | "restart")
            .then(|| (manager, word, &argv[2..])),
        Some(word) if !word.is_empty() && !word.starts_with('-') => {
            Some((manager, word, &argv[2..]))
        }
        _ => None,
    }
}

fn missing_script(name: &str, f: &Cmd, g: &Cmd) -> Option<Outcome> {
    let (f_mgr, x, f_tail) = script_invocation(&f.argv)?;
    let (g_mgr, y, g_tail) = script_invocation(&g.argv)?;
    if f_mgr != g_mgr || x != name || y == x || f_tail != g_tail {
        return None;
    }
    Some(Outcome {
        class: Class::UnknownTarget,
        typo: typo(x, y),
        key: vec![
            ("kind", "script".to_owned()),
            ("stem", format!("{f_mgr} run")),
            ("name", x.to_owned()),
            ("replacement", y.to_owned()),
        ],
        context: vec![("dir", g.dir.clone())],
        wrong: Some(Wrong::Command {
            dir: f.dir.clone(),
            argv: canonical(&f.argv),
        }),
        nested: false,
    })
}

fn unknown_target(hit: &Hit, f: &Cmd, g: &Cmd) -> Option<Outcome> {
    if !f.same_place(g) {
        return None;
    }
    if let Hit::MissingScript(name) = hit {
        return missing_script(name, f, g);
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
        Hit::TerraformWorkspace(name)
            if matches!(program, "terraform" | "tofu")
                && before.get(1).map(String::as_str) == Some("workspace")
                && before.get(2).map(String::as_str) == Some("select")
                && index == 3
                && wrong == *name =>
        {
            (
                "workspace",
                format!("{program} workspace select"),
                wrong.clone(),
                right.clone(),
                vec![],
            )
        }
        Hit::UvPackage(name) if program == "uv" => {
            let (x, y, flag) = flag_of(&["-p", "--package"])?;
            (x == *name).then_some(())?;
            let uv_stem = match f.argv.get(1).map(String::as_str) {
                Some(sub) if !sub.starts_with('-') => format!("uv {sub}"),
                _ => "uv".to_owned(),
            };
            ("package", uv_stem, x, y, vec![("flag", flag)])
        }
        Hit::PnpmFilter if program == "pnpm" => {
            let (x, y, flag) = flag_of(&["-F", "--filter"])?;
            ("filter", "pnpm".to_owned(), x, y, vec![("flag", flag)])
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
        wrong: Some(Wrong::Command {
            dir: f.dir.clone(),
            argv: before,
        }),
        nested: false,
    })
}

fn unknown_option(flag: &str, f: &Cmd, g: &Cmd) -> Option<Outcome> {
    if !flag.starts_with('-') || !f.same_place(g) || stem(&f.argv) != stem(&g.argv) {
        return None;
    }
    let index = f.argv.iter().position(|word| {
        word.strip_prefix(flag)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('='))
    })?;
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
        wrong: Some(Wrong::Command {
            dir: f.dir.clone(),
            argv: f.argv.clone(),
        }),
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
        wrong: Some(Wrong::Command {
            dir: f.dir.clone(),
            argv: f.argv.clone(),
        }),
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
        path_like(word)
            && (word == target
                || target
                    .strip_suffix(word)
                    .is_some_and(|prefix| prefix.ends_with('/')))
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
        wrong: Some(Wrong::Command {
            dir: f.dir.clone(),
            argv: f.argv.clone(),
        }),
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
        wrong: Some(Wrong::Command {
            dir: f.dir.clone(),
            argv: f.argv.clone(),
        }),
        nested: false,
    })
}

fn prerequisite_step(hit: &Hit, f: &Cmd, g: &Cmd) -> Option<Outcome> {
    if !f.same_place(g) {
        return None;
    }
    let f_words = events::unwrap(&f.argv);
    let f_prog = basename(f_words.first()?);
    let f_sub = f_words.get(1).map(String::as_str).unwrap_or("");
    let g_words = events::unwrap(&g.argv);
    let g_prog = basename(g_words.first()?);
    let g_has_fvm = basename(g.program()) == "fvm";
    let mut context = vec![("dir", g.dir.clone())];

    let (kind, prereq, stem_text): (&'static str, String, String) = match hit {
        Hit::FlutterPubGet
            if matches!(f_prog, "flutter" | "dart")
                && !f_sub.is_empty()
                && f_sub != "pub"
                && matches!(g_prog, "flutter" | "dart")
                && g_words.get(1).map(String::as_str) == Some("pub")
                && g_words.get(2).map(String::as_str) == Some("get") =>
        {
            let prefix = if g_has_fvm { "fvm " } else { "" };
            (
                "pub_get",
                format!("{prefix}{g_prog} pub get"),
                stem(&f.argv),
            )
        }
        Hit::DartCodegen if matches!(f_prog, "flutter" | "dart") && !f_sub.is_empty() => {
            let base = match g_words
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice()
            {
                ["dart", "run", "build_runner", "build", ..] => "dart run build_runner build",
                ["flutter", "pub", "run", "build_runner", "build", ..] => {
                    "flutter pub run build_runner build"
                }
                _ => return None,
            };
            let prefix = if g_has_fvm { "fvm " } else { "" };
            let delete_flag = if g_words
                .iter()
                .any(|w| w == "--delete-conflicting-outputs" || w == "-d")
            {
                " --delete-conflicting-outputs"
            } else {
                ""
            };
            (
                "build_runner",
                format!("{prefix}{base}{delete_flag}"),
                stem(&f.argv),
            )
        }
        Hit::TerraformInit | Hit::TerraformUpgrade
            if matches!(f_prog, "terraform" | "tofu" | "terragrunt")
                && !f_sub.is_empty()
                && f_sub != "init"
                && g_prog == f_prog =>
        {
            let (is_init, run_all) = match (
                g_words.get(1).map(String::as_str),
                g_words.get(2).map(String::as_str),
            ) {
                (Some("init"), _) => (true, false),
                (Some("run-all"), Some("init")) => (true, true),
                _ => (false, false),
            };
            if !is_init {
                return None;
            }
            let upgrade = g_words.iter().any(|w| w == "-upgrade");
            let base = if run_all {
                format!("{g_prog} run-all init")
            } else {
                format!("{g_prog} init")
            };
            let (kind, prereq) = if upgrade {
                ("tf_upgrade", format!("{base} -upgrade"))
            } else if matches!(hit, Hit::TerraformUpgrade) {
                return None;
            } else {
                ("tf_init", base)
            };
            (kind, prereq, stem(&f.argv))
        }
        Hit::TerraformWorkspace(_) | Hit::TerraformWorkspaceError
            if matches!(f_prog, "terraform" | "tofu" | "terragrunt")
                && !f_sub.is_empty()
                && f_sub != "workspace"
                && g_prog == f_prog
                && g_words.get(1).map(String::as_str) == Some("workspace")
                && matches!(g_words.get(2).map(String::as_str), Some("select" | "new")) =>
        {
            let action = g_words[2].as_str();
            let ws = g_words.get(3)?.as_str();
            if ws.is_empty() || ws.starts_with('-') {
                return None;
            }
            (
                "tf_workspace",
                format!("{g_prog} workspace {action} {ws}"),
                stem(&f.argv),
            )
        }
        Hit::HelmDeps
            if f_prog == "helm"
                && !f_sub.is_empty()
                && !matches!(f_sub, "dependency" | "dep")
                && g_prog == "helm"
                && matches!(
                    g_words.get(1).map(String::as_str),
                    Some("dependency" | "dep")
                ) =>
        {
            let action = match g_words.get(2).map(String::as_str) {
                Some("update" | "up") => "update",
                Some("build") => "build",
                _ => return None,
            };
            if let Some(chart_dir) = g_words
                .iter()
                .skip(3)
                .find(|w| !w.starts_with('-') && !w.starts_with('/'))
            {
                context.push(("chart_dir", chart_dir.trim_start_matches("./").to_owned()));
            }
            (
                "helm_deps",
                format!("helm dependency {action}"),
                stem(&f.argv),
            )
        }
        Hit::KubectlContext
            if f_prog == "kubectl"
                && f_sub != "config"
                && g_prog == "kubectl"
                && g_words.get(1).map(String::as_str) == Some("config")
                && g_words.get(2).map(String::as_str) == Some("use-context") =>
        {
            let ctx = g_words.get(3)?.as_str();
            if !safe_kubectl_context(ctx) {
                return None;
            }
            (
                "kubectl_context",
                format!("kubectl config use-context {ctx}"),
                stem(&f.argv),
            )
        }
        Hit::DockerDaemon if f_prog == "docker" && g_prog == "docker" => {
            let prereq = match (
                g_words.get(1).map(String::as_str),
                g_words.get(2).map(String::as_str),
                g_words.get(3).map(String::as_str),
            ) {
                (Some("context"), Some("use"), Some(ctx))
                    if !ctx.is_empty() && !ctx.starts_with('-') =>
                {
                    format!("docker context use {ctx}")
                }
                (Some("buildx"), Some("use"), Some(builder))
                    if !builder.is_empty() && !builder.starts_with('-') =>
                {
                    format!("docker buildx use {builder}")
                }
                _ => return None,
            };
            ("docker_context", prereq, docker_build_stem(&f.argv))
        }
        Hit::UvSync | Hit::ModuleNotFound
            if g_prog == "uv"
                && matches!(g_words.get(1).map(String::as_str), Some("sync" | "lock"))
                && (basename(f.program()) == "uv"
                    && !matches!(
                        f.argv.get(1).map(String::as_str),
                        Some("sync" | "lock" | "venv")
                    )
                    || matches!(f_prog, "python" | "python3" | "pytest" | "ruff" | "mypy")) =>
        {
            let action = g_words[1].as_str();
            let target_stem = if basename(f.program()) == "uv" {
                match f.argv.get(1).map(String::as_str) {
                    Some(sub) if !sub.starts_with('-') => format!("uv {sub}"),
                    _ => "uv".to_owned(),
                }
            } else {
                stem(&f.argv)
            };
            ("uv_sync", format!("uv {action}"), target_stem)
        }
        _ => return None,
    };

    Some(Outcome {
        class: Class::PrerequisiteStep,
        typo: false,
        key: vec![
            ("kind", kind.to_owned()),
            ("prereq", prereq),
            ("stem", stem_text),
        ],
        context,
        wrong: None,
        nested: false,
    })
}

fn toolchain_wrapper(hit: &Hit, f: &Cmd, g: &Cmd) -> Option<Outcome> {
    if !f.same_place(g) {
        return None;
    }
    let f_prog = basename(f.program());
    let (kind, from, to): (&'static str, String, String) = match hit {
        Hit::FlutterSdkMismatch if matches!(f_prog, "flutter" | "dart") => {
            if basename(g.program()) != "fvm"
                || g.argv.len() != f.argv.len() + 1
                || g.argv.get(1).map(String::as_str) != Some(f_prog)
                || g.argv[2..] != f.argv[1..]
            {
                return None;
            }
            ("fvm", f_prog.to_owned(), format!("fvm {f_prog}"))
        }
        Hit::ModuleNotFound
            if matches!(f_prog, "python" | "python3" | "pytest" | "ruff" | "mypy") =>
        {
            if basename(g.program()) != "uv"
                || g.argv.get(1).map(String::as_str) != Some("run")
                || g.argv.len() != f.argv.len() + 2
                || g.argv[3..] != f.argv[1..]
            {
                return None;
            }
            let inner = basename(&g.argv[2]);
            let same_tool = inner == f_prog
                || matches!(
                    (f_prog, inner),
                    ("python", "python3") | ("python3", "python")
                );
            if !same_tool {
                return None;
            }
            ("uv", f_prog.to_owned(), format!("uv run {inner}"))
        }
        Hit::DockerBuildx
            if f_prog == "docker" && f.argv.get(1).map(String::as_str) == Some("build") =>
        {
            if basename(g.program()) != "docker"
                || g.argv.get(1).map(String::as_str) != Some("buildx")
                || g.argv.get(2).map(String::as_str) != Some("build")
                || g.argv[3..] != f.argv[2..]
            {
                return None;
            }
            (
                "buildx",
                "docker build".to_owned(),
                "docker buildx build".to_owned(),
            )
        }
        _ => return None,
    };
    Some(Outcome {
        class: Class::ToolchainWrapper,
        typo: false,
        key: vec![("kind", kind.to_owned()), ("from", from), ("to", to)],
        context: vec![("dir", g.dir.clone())],
        wrong: Some(Wrong::Command {
            dir: f.dir.clone(),
            argv: f.argv.clone(),
        }),
        nested: false,
    })
}

fn terraform_option(hit: &Hit, f: &Cmd, g: &Cmd) -> Option<Outcome> {
    let prog = basename(f.program());
    if !matches!(prog, "terraform" | "tofu" | "terragrunt")
        || basename(g.program()) != prog
        || !f.same_place(g)
        || stem(&f.argv) != stem(&g.argv)
    {
        return None;
    }
    let added = added_words(&f.argv, &g.argv)?;
    let [flag] = added.as_slice() else {
        return None;
    };
    let valid = match hit {
        Hit::TerraformUpgrade => {
            matches!(f.argv.get(1).map(String::as_str), Some("init")) && flag == "-upgrade"
        }
        Hit::TerraformStateLock => flag == "-lock=false",
        _ => false,
    };
    valid.then(|| Outcome {
        class: Class::MissingOption,
        typo: false,
        key: vec![("stem", stem(&f.argv)), ("flag", flag.clone())],
        context: vec![("dir", g.dir.clone())],
        wrong: Some(Wrong::Command {
            dir: f.dir.clone(),
            argv: f.argv.clone(),
        }),
        nested: false,
    })
}

/// Remove a single `flag value` or `flag=value` occurrence from `argv`.
fn strip_opt<'a>(argv: &'a [String], flags: &[&str]) -> (Option<(&'a str, &'a str)>, Vec<&'a str>) {
    let mut found = None;
    let mut rest = Vec::with_capacity(argv.len());
    let mut i = 0;
    while i < argv.len() {
        let word = argv[i].as_str();
        if found.is_none() {
            if let Some((flag, value)) = word.split_once('=')
                && flags.contains(&flag)
                && !value.is_empty()
            {
                found = Some((flag, value));
                i += 1;
                continue;
            }
            if flags.contains(&word)
                && let Some(value) = argv.get(i + 1).map(String::as_str)
                && !value.is_empty()
                && !value.starts_with('-')
            {
                found = Some((word, value));
                i += 2;
                continue;
            }
        }
        rest.push(word);
        i += 1;
    }
    (found, rest)
}

/// Reject empty, flag-like, or colon-qualified kubectl context names (such as
/// AWS EKS ARNs `arn:aws:eks:...:123456789012:cluster/prod`) so cloud account
/// identifiers never enter learned rules.
fn safe_kubectl_context(ctx: &str) -> bool {
    !ctx.is_empty() && !ctx.starts_with('-') && !ctx.contains(':')
}

fn kubectl_option(hit: &Hit, f: &Cmd, g: &Cmd) -> Option<Outcome> {
    if basename(f.program()) != "kubectl" || basename(g.program()) != "kubectl" || !f.same_place(g)
    {
        return None;
    }
    let groups: &[&[&str]] = match hit {
        Hit::KubectlNotFound | Hit::KubectlEmptyNamespace(_) => {
            &[&["-n", "--namespace"], &["--context"], &["--kubeconfig"]]
        }
        Hit::KubectlContext => &[&["--context"], &["--kubeconfig"]],
        _ => return None,
    };
    for &group in groups {
        let (f_opt, f_rest) = strip_opt(&f.argv, group);
        let (g_opt, g_rest) = strip_opt(&g.argv, group);
        let Some((flag, value)) = g_opt else {
            continue;
        };
        if f_rest != g_rest {
            continue;
        }
        if flag == "--kubeconfig"
            && (value.starts_with('/') || value.starts_with('~') || value.contains(".."))
        {
            continue;
        }
        let from_value = f_opt.map(|(_, v)| v).unwrap_or("");
        if flag == "--context"
            && (!safe_kubectl_context(value)
                || (!from_value.is_empty() && !safe_kubectl_context(from_value)))
        {
            continue;
        }
        if from_value == value {
            continue;
        }
        let mut key = vec![
            ("stem", "kubectl".to_owned()),
            ("flag", flag.to_owned()),
            ("value", value.to_owned()),
        ];
        if !from_value.is_empty() {
            key.push(("from", from_value.to_owned()));
        }
        return Some(Outcome {
            class: Class::MissingOption,
            typo: !from_value.is_empty() && typo(from_value, value),
            key,
            context: vec![("dir", g.dir.clone())],
            wrong: Some(Wrong::Command {
                dir: f.dir.clone(),
                argv: f.argv.clone(),
            }),
            nested: false,
        });
    }
    // Also recognize adding `-A` / `--all-namespaces` on empty namespace output.
    if matches!(hit, Hit::KubectlEmptyNamespace(_))
        && let Some(added) = added_words(&f.argv, &g.argv)
        && let [flag] = added.as_slice()
        && matches!(flag.as_str(), "-A" | "--all-namespaces")
    {
        return Some(Outcome {
            class: Class::MissingOption,
            typo: false,
            key: vec![("stem", "kubectl".to_owned()), ("flag", flag.clone())],
            context: vec![("dir", g.dir.clone())],
            wrong: Some(Wrong::Command {
                dir: f.dir.clone(),
                argv: f.argv.clone(),
            }),
            nested: false,
        });
    }
    None
}

fn is_docker_build(argv: &[String]) -> bool {
    let prog = argv.first().map(|s| basename(s)).unwrap_or("");
    matches!(
        (
            prog,
            argv.get(1).map(String::as_str),
            argv.get(2).map(String::as_str)
        ),
        ("docker" | "podman", Some("build"), _)
            | ("docker", Some("buildx"), Some("build"))
            | ("docker" | "podman", Some("compose"), Some("build"))
    )
}

fn docker_build_stem(argv: &[String]) -> String {
    let prog = argv.first().map(|s| basename(s)).unwrap_or("");
    match (
        prog,
        argv.get(1).map(String::as_str),
        argv.get(2).map(String::as_str),
    ) {
        ("docker", Some("buildx"), Some("build")) => "docker buildx build".to_owned(),
        ("docker" | "podman", Some("compose"), Some("build")) => format!("{prog} compose build"),
        _ => stem(argv),
    }
}

fn docker_option(hit: &Hit, f: &Cmd, g: &Cmd) -> Option<Outcome> {
    let prog = basename(f.program());
    if !matches!(prog, "docker" | "podman") || basename(g.program()) != prog || !f.same_place(g) {
        return None;
    }
    let added = added_words(&f.argv, &g.argv)?;
    let (stem_text, flag, value): (String, String, Option<String>) = match hit {
        Hit::DockerBuildFailed if is_docker_build(&f.argv) => match added.as_slice() {
            [flag] if matches!(flag.as_str(), "--no-cache" | "--pull") => {
                (docker_build_stem(&f.argv), flag.clone(), None)
            }
            [flag, spec] if flag == "--build-arg" => {
                let arg_name = spec.split_once('=').map_or(spec.as_str(), |(k, _)| k);
                if arg_name.is_empty()
                    || arg_name.starts_with('-')
                    || privacy::is_secret_name(arg_name)
                {
                    return None;
                }
                (
                    docker_build_stem(&f.argv),
                    "--build-arg".to_owned(),
                    Some(arg_name.to_owned()),
                )
            }
            [word] if word.starts_with("--build-arg=") => {
                let spec = &word["--build-arg=".len()..];
                let arg_name = spec.split_once('=').map_or(spec, |(k, _)| k);
                if arg_name.is_empty()
                    || arg_name.starts_with('-')
                    || privacy::is_secret_name(arg_name)
                {
                    return None;
                }
                (
                    docker_build_stem(&f.argv),
                    "--build-arg".to_owned(),
                    Some(arg_name.to_owned()),
                )
            }
            _ => return None,
        },
        Hit::NoDockerfile if is_docker_build(&f.argv) => match added.as_slice() {
            [flag, path] if matches!(flag.as_str(), "-f" | "--file") => {
                if path.is_empty()
                    || path.starts_with('-')
                    || path.starts_with('/')
                    || path.contains("..")
                {
                    return None;
                }
                (docker_build_stem(&f.argv), flag.clone(), Some(path.clone()))
            }
            [word] if word.starts_with("--file=") => {
                let path = &word["--file=".len()..];
                if path.is_empty() || path.starts_with('/') || path.contains("..") {
                    return None;
                }
                (
                    docker_build_stem(&f.argv),
                    "--file".to_owned(),
                    Some(path.to_owned()),
                )
            }
            _ => return None,
        },
        Hit::DockerDaemon => match added.as_slice() {
            [flag, val] if matches!(flag.as_str(), "--context" | "--builder") => {
                if val.is_empty() || val.starts_with('-') {
                    return None;
                }
                let st = if flag == "--context" {
                    prog.to_owned()
                } else {
                    docker_build_stem(&f.argv)
                };
                (st, flag.clone(), Some(val.clone()))
            }
            [word] if word.starts_with("--context=") || word.starts_with("--builder=") => {
                let (flag, val) = word.split_once('=')?;
                if val.is_empty() || val.starts_with('-') {
                    return None;
                }
                let st = if flag == "--context" {
                    prog.to_owned()
                } else {
                    docker_build_stem(&f.argv)
                };
                (st, flag.to_owned(), Some(val.to_owned()))
            }
            _ => return None,
        },
        _ => return None,
    };
    let mut key = vec![("stem", stem_text), ("flag", flag)];
    if let Some(value) = value {
        key.push(("value", value));
    }
    Some(Outcome {
        class: Class::MissingOption,
        typo: false,
        key,
        context: vec![("dir", g.dir.clone())],
        wrong: Some(Wrong::Command {
            dir: f.dir.clone(),
            argv: f.argv.clone(),
        }),
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
        wrong: Some(wrong),
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
            let missing = self_evident(next).into_iter().any(
                |(hit, _)| matches!(hit, SelfHit::Missing(_, ref program) if *program == words[0]),
            );
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
                    &[".venv/bin/python"],
                    &[".venv/bin/python3"],
                ],
                "python3" => &[
                    &["uv", "run", "python3"],
                    &["uv", "run", "python"],
                    &[".venv/bin/python3"],
                    &[".venv/bin/python"],
                ],
                "pip" => &[&["pip3"], &["python3", "-m", "pip"], &["uv", "pip"]],
                "pytest" => &[
                    &["uv", "run", "pytest"],
                    &["poetry", "run", "pytest"],
                    &[".venv/bin/pytest"],
                ],
                "ruff" => &[&["uv", "run", "ruff"], &[".venv/bin/ruff"]],
                "mypy" => &[&["uv", "run", "mypy"], &[".venv/bin/mypy"]],
                "flutter" => &[&["fvm", "flutter"]],
                "dart" => &[&["fvm", "dart"]],
                "terraform" => &[&["tofu"]],
                "tofu" => &[&["terraform"]],
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
    let same_argv = |a: &[String], b: &[String]| {
        if a.first().map(String::as_str) == Some("npm")
            || b.first().map(String::as_str) == Some("npm")
        {
            canonical(a) == canonical(b)
        } else {
            a == b
        }
    };
    candidate.wrong.iter().any(|wrong| match wrong {
        Wrong::Command { dir, argv } => {
            code == 0
                && cmd.dir == *dir
                && same_argv(&cmd.argv, argv)
                && event_hits(event).is_empty()
        }
        Wrong::Program(lead) => {
            code == 0
                && cmd.argv.len() >= lead.len()
                && basename(&cmd.argv[0]) == basename(&lead[0])
                && cmd.argv[1..lead.len()] == lead[1..]
                && !self_evident(event)
                    .iter()
                    .any(|(hit, _)| matches!(hit, SelfHit::Missing(..)))
        }
        Wrong::Tests { dir, argv } => {
            cmd.dir == *dir
                && same_argv(&cmd.argv, argv)
                && events::runner(argv).is_some_and(|runner| ran_tests(runner, event))
        }
    })
}

/// Test runners that do not name themselves with `test`.
const TEST_PROGRAMS: &[&str] = &[
    "jest",
    "mocha",
    "rspec",
    "phpunit",
    "ava",
    "tox",
    "nox",
    "karma",
    "cypress",
    "playwright",
];
/// Lines test runners print when tests fail [here: cargo, go test, unittest].
const TEST_FAILED: &[&str] = &[
    "test result: FAILED",
    "to rerun pass",
    "--- FAIL",
    "FAILED (",
    "tests failed",
    "Tests failed",
];

/// A test run: a known runner, a program or subcommand named for tests, or output
/// that reports failed tests.
fn test_run(cmd: &Cmd, event: &Event) -> bool {
    let words = canonical(events::unwrap(&cmd.argv));
    events::runner(&cmd.argv).is_some()
        || words
            .first()
            .is_some_and(|program| TEST_PROGRAMS.contains(&basename(program)))
        || words.iter().take(3).enumerate().any(|(index, word)| {
            let word = if index == 0 { basename(word) } else { word };
            word.to_ascii_lowercase().contains("test")
        })
        || event
            .texts
            .iter()
            .any(|text| TEST_FAILED.iter().any(|marker| text.body.contains(marker)))
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
    wrong != right
        && wrong.len() >= 4
        && right.len() >= 4
        && wrong.chars().count().abs_diff(right.chars().count()) <= 2
        && distance(wrong, right) <= 2
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
        Class::UnknownOption => {
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
        Class::MissingOption => {
            let head = get("stem").split(' ').next().unwrap_or("");
            if head == "kubectl" && get("flag") == "--context" {
                Scope::Machine
            } else if head.contains('/')
                || slots.contains_key("value")
                || matches!(
                    head,
                    "terraform"
                        | "tofu"
                        | "terragrunt"
                        | "kubectl"
                        | "helm"
                        | "docker"
                        | "podman"
                        | "flutter"
                        | "dart"
                        | "uv"
                )
            {
                Scope::Project
            } else {
                Scope::Machine
            }
        }
        Class::PrerequisiteStep if get("kind") == "kubectl_context" => Scope::Machine,
        _ => Scope::Project,
    }
}

/// Find the first `.tf` file directly inside `dir`.
fn find_tf_file(dir: &Path) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".tf"))
        .collect();
    names.sort();
    names.into_iter().next()
}

/// Walk from `start` up to the repository root looking for the first matching file.
fn find_upward(root: &Path, start: &str, names: &[&str]) -> Option<String> {
    let mut dir = start.to_owned();
    loop {
        let base = if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&dir)
        };
        if let Some(name) = names.iter().find(|name| base.join(name).is_file()) {
            return Some(joined(&dir, name));
        }
        if dir.is_empty() {
            return None;
        }
        dir = parent(&dir).to_owned();
    }
}

/// Find a Terraform/OpenTofu/Terragrunt manifest in `dir`.
fn find_tf_manifest(root: &Path, dir: &str) -> Option<String> {
    let base = if dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(dir)
    };
    for name in ["terragrunt.hcl", ".terraform.lock.hcl"] {
        if base.join(name).is_file() {
            return Some(joined(dir, name));
        }
    }
    find_tf_file(&base).map(|name| joined(dir, &name))
}

/// Verify that `dir/value` is a regular file (not a symlink) whose canonical path
/// remains inside the canonical workspace `root`.
fn workspace_regular_file(root: &Path, dir: &str, value: &str) -> bool {
    let rel = Path::new(value);
    if value.is_empty()
        || !rel
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return false;
    }
    let base = if dir.is_empty() {
        root.to_path_buf()
    } else {
        let dir_path = Path::new(dir);
        if !dir_path
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
        {
            return false;
        }
        root.join(dir_path)
    };
    let candidate = base.join(rel);
    let Ok(meta) = std::fs::symlink_metadata(&candidate) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    let Ok(canonical_root) = root.canonicalize() else {
        return false;
    };
    let Ok(canonical_file) = candidate.canonicalize() else {
        return false;
    };
    canonical_file.starts_with(&canonical_root)
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
            if manifest == ".tf" {
                if let Some(found) = find_tf_manifest(root, get("from")) {
                    return Check::Stale(format!("{found} exists"));
                } else if let Some(found) = find_tf_manifest(root, get("to")) {
                    return Check::Verified(found);
                } else {
                    return Check::Stale(format!("{} has no .tf files", get("to")));
                }
            }
            let names: Vec<&str> = if manifest == "Makefile" {
                MAKEFILES.to_vec()
            } else {
                vec![manifest]
            };
            let upward = !matches!(manifest, "Makefile" | "Dockerfile" | "Chart.yaml");
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
                "package" if get("stem").starts_with("cargo") => {
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
                "workspace" => match find_tf_manifest(root, dir) {
                    Some(found) => Check::Verified(found),
                    None => Check::Unverified,
                },
                "filter" => match find_upward(root, dir, &["pnpm-workspace.yaml"]) {
                    Some(found) => Check::Verified(found),
                    None => Check::Unverified,
                },
                _ => Check::Unverified,
            }
        }
        Class::UnknownOption => Check::Unverified,
        Class::MissingOption => {
            let (stem_text, flag, value, dir) =
                (get("stem"), get("flag"), get("value"), get("dir"));
            if matches!(flag, "-f" | "--file" | "--kubeconfig") && !value.is_empty() {
                if workspace_regular_file(root, dir, value) {
                    return Check::Verified(joined(dir, value));
                }
                return Check::Stale(format!("{} is missing", joined(dir, value)));
            }
            let head = stem_text.split(' ').next().unwrap_or("");
            match head {
                "terraform" | "tofu" | "terragrunt" => match find_tf_manifest(root, dir) {
                    Some(found) => Check::Verified(found),
                    None => Check::Unverified,
                },
                "docker" | "podman" => match find_upward(
                    root,
                    dir,
                    &[
                        "Dockerfile",
                        "compose.yaml",
                        "compose.yml",
                        "docker-compose.yml",
                        "docker-compose.yaml",
                    ],
                ) {
                    Some(found) => Check::Verified(found),
                    None => Check::Unverified,
                },
                _ => Check::Unverified,
            }
        }
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
            let lockfiles = |manager: &str| -> &'static [&'static str] {
                match manager {
                    "npm" => &["package-lock.json"],
                    "pnpm" => &["pnpm-lock.yaml"],
                    "yarn" => &["yarn.lock"],
                    "bun" => &["bun.lock", "bun.lockb"],
                    _ => &[],
                }
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
                if let Some(lock) = lockfiles(wrong)
                    .iter()
                    .find(|lock| at(&dir).join(lock).is_file())
                {
                    return Check::Stale(format!("{} exists", joined(&dir, lock)));
                }
                if let Some(lock) = lockfiles(right)
                    .iter()
                    .find(|lock| at(&dir).join(lock).is_file())
                {
                    return Check::Verified(joined(&dir, lock));
                }
                if dir.is_empty() {
                    return Check::Unverified;
                }
                dir = parent(&dir).to_owned();
            }
        }
        Class::PrerequisiteStep => {
            let dir = get("dir");
            let found = match get("kind") {
                "pub_get" | "build_runner" => find_upward(root, dir, &["pubspec.yaml"]),
                "tf_init" | "tf_upgrade" | "tf_workspace" => find_tf_manifest(root, dir),
                "helm_deps" => {
                    let chart_dir = get("chart_dir");
                    if !chart_dir.is_empty() && at(dir).join(chart_dir).join("Chart.yaml").is_file()
                    {
                        Some(joined(&joined(dir, chart_dir), "Chart.yaml"))
                    } else if at(dir).join("Chart.yaml").is_file() {
                        Some(joined(dir, "Chart.yaml"))
                    } else {
                        None
                    }
                }
                "uv_sync" => find_upward(root, dir, &["uv.lock", "pyproject.toml"]),
                "docker_context" => find_upward(
                    root,
                    dir,
                    &[
                        "Dockerfile",
                        "compose.yaml",
                        "compose.yml",
                        "docker-compose.yml",
                    ],
                ),
                _ => None,
            };
            found.map_or(Check::Unverified, Check::Verified)
        }
        Class::ToolchainWrapper => {
            let dir = get("dir");
            let found = match get("kind") {
                "fvm" => find_upward(root, dir, &[".fvmrc", ".fvm/fvm_config.json"]),
                "uv" => find_upward(root, dir, &["uv.lock", "pyproject.toml"]),
                "buildx" => find_upward(
                    root,
                    dir,
                    &[
                        "Dockerfile",
                        "compose.yaml",
                        "compose.yml",
                        "docker-compose.yml",
                    ],
                ),
                _ => None,
            };
            found.map_or(Check::Unverified, Check::Verified)
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
            let manifest_label = if manifest == ".tf" {
                "Terraform files"
            } else {
                manifest
            };
            if to.is_empty() {
                safe(&[from]).then_some(())?;
                format!("Run `{program}` from the repository root, not `{from}/`.")
            } else if from.is_empty() {
                safe(&[to]).then_some(())?;
                format!("Run `{program}` from `{to}/`; the repository root has no {manifest_label}.")
            } else {
                safe(&[from, to]).then_some(())?;
                format!("Run `{program}` from `{to}/`; `{from}/` has no {manifest_label}.")
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
                "workspace" => format!("Use `{stem} {y}`; there is no workspace `{x}`."),
                "filter" => {
                    let flag = if get("flag").is_empty() {
                        "--filter"
                    } else {
                        get("flag")
                    };
                    safe(&[flag]).then_some(())?;
                    format!("Use `{stem} {flag} {y}`; no workspace matched `{x}`.")
                }
                "script" => format!("Run `{stem} {y}`; there is no `{x}` script."),
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
            let (stem, flag, value, from) =
                (get("stem"), get("flag"), get("value"), get("from"));
            safe(&[stem, flag]).then_some(())?;
            if flag == "--build-arg" && !value.is_empty() {
                safe(&[value]).then_some(())?;
                format!("Pass `--build-arg {value}=...` to `{stem}`.")
            } else if !from.is_empty() && !value.is_empty() {
                safe(&[from, value]).then_some(())?;
                format!("Use `{flag} {value}`, not `{flag} {from}`, with `{stem}`.")
            } else if !value.is_empty() {
                safe(&[value]).then_some(())?;
                format!("Pass `{flag} {value}` to `{stem}`.")
            } else {
                format!("Pass `{flag}` to `{stem}`.")
            }
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
        Class::PrerequisiteStep => {
            let (kind, prereq, stem) = (get("kind"), get("prereq"), get("stem"));
            safe(&[prereq, stem]).then_some(())?;
            let reason = match kind {
                "pub_get" => "when dependencies are out of date",
                "build_runner" => "when generated files are missing",
                "tf_init" => "when the working directory is not initialized",
                "tf_upgrade" => "when provider locks are stale",
                "tf_workspace" => "when the workspace is not selected",
                "helm_deps" => "when chart dependencies are missing",
                "kubectl_context" => "when the cluster context is unset",
                "docker_context" => "when the daemon context is unset",
                "uv_sync" => "when the environment or uv.lock is out of date",
                _ => "before running it",
            };
            format!("Run `{prereq}` before `{stem}` {reason}.")
        }
        Class::ToolchainWrapper => {
            let (kind, wrong, right) = (get("kind"), get("from"), get("to"));
            safe(&[wrong, right]).then_some(())?;
            match (kind, check) {
                ("fvm", Check::Verified(detail)) => {
                    let file = basename(detail);
                    format!("Use `{right}`, not `{wrong}`; this project pins its SDK in {file}.")
                }
                ("uv", Check::Verified(detail)) => {
                    let file = basename(detail);
                    format!("Use `{right}`, not `{wrong}`; this project has {file}.")
                }
                ("buildx", _) => {
                    format!("Use `{right}`, not `{wrong}`, for BuildKit features here.")
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
                "go.mod" => "go: no go.mod",
                "pubspec.yaml" => "flutter: no pubspec.yaml",
                ".tf" => "terraform: no configuration files",
                "Dockerfile" => "docker: no Dockerfile",
                "Chart.yaml" => "helm: no Chart.yaml",
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
                        if stem.starts_with("uv") {
                            "uv: unknown package"
                        } else {
                            "cargo: unknown package"
                        }
                    } else {
                        "cargo: unknown target"
                    };
                    (
                        format!("{stem} {flag} {x}"),
                        format!("{stem} {flag} {y}"),
                        signal,
                    )
                }
                "workspace" => (
                    format!("{stem} {x}"),
                    format!("{stem} {y}"),
                    "terraform: unknown workspace",
                ),
                "filter" => {
                    let flag = if flag.is_empty() { "--filter" } else { flag };
                    (
                        format!("{stem} {flag} {x}"),
                        format!("{stem} {flag} {y}"),
                        "pnpm: unmatched filter",
                    )
                }
                "script" => {
                    let signal = match stem.split(' ').next().unwrap_or("npm") {
                        "pnpm" => "pnpm: missing script",
                        "yarn" => "yarn: missing script",
                        "bun" => "bun: missing script",
                        _ => "npm: missing script",
                    };
                    (format!("{stem} {x}"), format!("{stem} {y}"), signal)
                }
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
        Class::MissingOption => {
            let (stem, flag, value, from) = (get("stem"), get("flag"), get("value"), get("from"));
            if flag == "--build-arg" && !value.is_empty() {
                (
                    stem.to_owned(),
                    format!("{stem} --build-arg {value}=..."),
                    "docker: missing build-arg",
                )
            } else if !from.is_empty() && !value.is_empty() {
                (
                    format!("{stem} {flag} {from}"),
                    format!("{stem} {flag} {value}"),
                    "wrong option value",
                )
            } else if !value.is_empty() {
                (
                    stem.to_owned(),
                    format!("{stem} {flag} {value}"),
                    "missing option",
                )
            } else {
                (stem.to_owned(), format!("{stem} {flag}"), "missing option")
            }
        }
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
        Class::PrerequisiteStep => {
            let (kind, prereq, stem) = (get("kind"), get("prereq"), get("stem"));
            let signal = match kind {
                "pub_get" => "flutter: pub get required",
                "build_runner" => "dart: codegen required",
                "tf_init" => "terraform: init required",
                "tf_upgrade" => "terraform: init -upgrade required",
                "tf_workspace" => "terraform: workspace required",
                "helm_deps" => "helm: dependency update required",
                "kubectl_context" => "kubectl: context required",
                "docker_context" => "docker: daemon context required",
                "uv_sync" => "uv: sync required",
                _ => "prerequisite step required",
            };
            (stem.to_owned(), format!("{prereq} && {stem}"), signal)
        }
        Class::ToolchainWrapper => {
            let (kind, wrong, right) = (get("kind"), get("from"), get("to"));
            let signal = match kind {
                "fvm" => "flutter: SDK version mismatch",
                "uv" => "python: module missing outside uv",
                "buildx" => "docker: buildx required",
                _ => "toolchain wrapper",
            };
            (wrong.to_owned(), right.to_owned(), signal)
        }
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
            Class::WrongDirectory
            | Class::WrongPackageManager
            | Class::PrerequisiteStep
            | Class::ToolchainWrapper => true,
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
                "go: go.mod file not found in current directory or any parent directory; see 'go help modules'",
                Hit::NoGoMod,
            ),
            (
                "pattern ./...: directory prefix . does not contain main module or its selected dependencies",
                Hit::NoGoMod,
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
                "\u{2009}ERR_PNPM_NO_SCRIPT\u{2009} Missing script: lint",
                Hit::MissingScript("lint".into()),
            ),
            (
                "\u{2009}ERR_PNPM_RECURSIVE_EXEC_FIRST_FAIL\u{2009} Command \"lint\" not found",
                Hit::MissingScript("lint".into()),
            ),
            (
                "error Command \"lint\" not found.",
                Hit::MissingScript("lint".into()),
            ),
            (
                "Usage Error: Couldn't find a script named \"lint\".",
                Hit::MissingScript("lint".into()),
            ),
            (
                "error: Script not found \"lint\"",
                Hit::MissingScript("lint".into()),
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
            (
                "Usage Error: This project is configured to use bun because /x/package.json has a \"packageManager\" field",
                Hit::Corepack("bun".into()),
            ),
            ("Error: No pubspec.yaml file found.", Hit::NoPubspecYaml),
            ("Error: No configuration files", Hit::NoTerraformConfig),
            (
                "ERROR: failed to solve: failed to read dockerfile: open Dockerfile: no such file or directory",
                Hit::NoDockerfile,
            ),
            ("Error: Chart.yaml file is missing", Hit::NoChartYaml),
            (
                "error - Target of URI doesn't exist: 'package:app/main.dart'.",
                Hit::FlutterPubGet,
            ),
            (
                "error - Target of URI hasn't been generated: 'package:app/user.g.dart'.",
                Hit::DartCodegen,
            ),
            (
                "Because app requires SDK version >=3.5.0 <4.0.0, version solving failed.",
                Hit::FlutterSdkMismatch,
            ),
            (
                "Error: Inconsistent dependency lock file",
                Hit::TerraformUpgrade,
            ),
            ("Error: Initialization required.", Hit::TerraformInit),
            (
                "Error: Error acquiring the state lock",
                Hit::TerraformStateLock,
            ),
            (
                "Error: Workspace \"prd\" doesn't exist.",
                Hit::TerraformWorkspace("prd".into()),
            ),
            (
                "Error: Resource cannot be used with the \"default\" workspace",
                Hit::TerraformWorkspaceError,
            ),
            (
                "Error: found in Chart.yaml, but missing in charts/ directory: redis",
                Hit::HelmDeps,
            ),
            (
                "Error from server (NotFound): pods \"api-0\" not found",
                Hit::KubectlNotFound,
            ),
            (
                "No resources found in default namespace.",
                Hit::KubectlEmptyNamespace("default".into()),
            ),
            ("error: current-context is not set", Hit::KubectlContext),
            (
                "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?",
                Hit::DockerDaemon,
            ),
            (
                "ERROR: Multi-platform build is not supported for the docker driver.",
                Hit::DockerBuildx,
            ),
            (
                "ERROR: failed to solve: failed to compute cache key: not found",
                Hit::DockerBuildFailed,
            ),
            (
                "error: The lockfile at `uv.lock` needs to be updated, but `--locked` was provided.",
                Hit::UvSync,
            ),
            (
                "ModuleNotFoundError: No module named 'fastapi'",
                Hit::ModuleNotFound,
            ),
            (
                "error: Package `api` not found in workspace",
                Hit::UvPackage("api".into()),
            ),
            (
                "ERR_PNPM_NO_MATCHING_WORKSPACE  No projects matched the filters in /repo",
                Hit::PnpmFilter,
            ),
        ];
        for (line, expected) in cases {
            assert!(hits(line).contains(expected), "{line}: {:?}", hits(line));
            assert!(may_signal(line), "{line}");
        }
        assert!(!may_signal(
            "   Compiling x v0.1.0\n    Finished `dev` profile\ntest result: ok."
        ));
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
            assert!(may_signal(line), "{line}");
        }
        for line in [
            "zsh:1: no matches found: --include=*.zz",
            "zsh:1: ==== not found",
        ] {
            assert!(may_signal(line), "{line}");
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

    #[test]
    fn learns_pnpm_yarn_bun_missing_scripts_bun_manager_and_go_mod_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts":{"check":"echo ok"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("backend")).unwrap();
        std::fs::write(root.join("backend/go.mod"), "module example.com/backend\n").unwrap();
        std::fs::write(root.join("bun.lock"), "{}\n").unwrap();

        let make_event =
            |thread: &str, time_ms: u64, dir_rel: &str, argv: &[&str], code: i32, body: &str| {
                let raw = argv.join(" ");
                let cmd = Cmd {
                    checkout: String::new(),
                    dir: dir_rel.to_owned(),
                    argv: argv.iter().map(|s| (*s).to_owned()).collect(),
                    masked: false,
                };
                Event {
                    thread: thread.to_owned(),
                    session: thread.to_owned(),
                    transcript: false,
                    codex: false,
                    time_ms,
                    raw,
                    env: Vec::new(),
                    shape: events::Shape::Simple(cmd),
                    status: Status::Code(code),
                    texts: vec![events::Text {
                        run: Some(format!("run-{time_ms}")),
                        stream: "stderr",
                        body: body.to_owned(),
                    }],
                    tests: events::Tests::default(),
                    failure_bytes: body.len() as u64,
                    pointer: Pointer::Run {
                        id: format!("run-{time_ms}"),
                        stream: "stderr",
                    },
                }
            };

        let events = vec![
            // pnpm missing script (without `run`, paired with `pnpm run check`)
            make_event(
                "s1",
                1_000,
                "",
                &["pnpm", "lint"],
                1,
                "\u{2009}ERR_PNPM_NO_SCRIPT\u{2009} Missing script: lint\n\nCommand \"lint\" not found.\n",
            ),
            make_event("s1", 2_000, "", &["pnpm", "run", "check"], 0, "ok\n"),
            // yarn v1 missing script
            make_event(
                "s2",
                3_000,
                "",
                &["yarn", "lint"],
                1,
                "error Command \"lint\" not found.\n",
            ),
            make_event("s2", 4_000, "", &["yarn", "check"], 0, "ok\n"),
            // yarn berry missing script (different script name to keep distinct candidate)
            make_event(
                "s3",
                5_000,
                "",
                &["yarn", "run", "format"],
                1,
                "Usage Error: Couldn't find a script named \"format\".\n",
            ),
            make_event("s3", 6_000, "", &["yarn", "run", "check"], 0, "ok\n"),
            // bun missing script
            make_event(
                "s4",
                7_000,
                "",
                &["bun", "run", "lint"],
                1,
                "error: Script not found \"lint\"\n",
            ),
            make_event("s4", 8_000, "", &["bun", "run", "check"], 0, "ok\n"),
            // WrongPackageManager: npm -> bun via workspace protocol
            make_event(
                "s5",
                9_000,
                "",
                &["npm", "install"],
                1,
                "npm error Unsupported URL Type \"workspace:\": workspace:*\n",
            ),
            make_event("s5", 10_000, "", &["bun", "install"], 0, "done\n"),
            // WrongDirectory: go.mod missing at root, succeeds in backend/
            make_event(
                "s6",
                11_000,
                "",
                &["go", "test", "./..."],
                1,
                "go: go.mod file not found in current directory or any parent directory; see 'go help modules'\n",
            ),
            make_event("s6", 12_000, "backend", &["go", "test", "./..."], 0, "ok\n"),
        ];

        let analysis = analyze(root, &events);
        let env = Env {
            path: None,
            shell: None,
        };
        let rendered: Vec<_> = analysis
            .candidates
            .iter()
            .map(|c| {
                let chk = check(root, &env, c.class, &c.slots);
                let text = render(c.class, &c.slots, &chk).expect("renderable");
                let ex = example(c.class, &c.slots);
                (c.class, chk, text, ex)
            })
            .collect();

        assert!(
            rendered.iter().any(|(class, chk, text, ex)| {
                *class == Class::UnknownTarget
                    && *chk == Check::Verified("package.json scripts".into())
                    && text == "Run `pnpm run check`; there is no `lint` script."
                    && ex.as_ref().map(|e| e.2) == Some("pnpm: missing script")
            }),
            "missing pnpm script rule: {rendered:#?}"
        );
        assert!(
            rendered.iter().any(|(class, chk, text, ex)| {
                *class == Class::UnknownTarget
                    && *chk == Check::Verified("package.json scripts".into())
                    && text == "Run `yarn run check`; there is no `lint` script."
                    && ex.as_ref().map(|e| e.2) == Some("yarn: missing script")
            }),
            "missing yarn v1 script rule: {rendered:#?}"
        );
        assert!(
            rendered.iter().any(|(class, chk, text, ex)| {
                *class == Class::UnknownTarget
                    && *chk == Check::Verified("package.json scripts".into())
                    && text == "Run `yarn run check`; there is no `format` script."
                    && ex.as_ref().map(|e| e.2) == Some("yarn: missing script")
            }),
            "missing yarn berry script rule: {rendered:#?}"
        );
        assert!(
            rendered.iter().any(|(class, chk, text, ex)| {
                *class == Class::UnknownTarget
                    && *chk == Check::Verified("package.json scripts".into())
                    && text == "Run `bun run check`; there is no `lint` script."
                    && ex.as_ref().map(|e| e.2) == Some("bun: missing script")
            }),
            "missing bun script rule: {rendered:#?}"
        );
        assert!(
            rendered.iter().any(|(class, chk, text, _)| {
                *class == Class::WrongPackageManager
                    && *chk == Check::Verified("bun.lock".into())
                    && text == "Use `bun`, not `npm`; this project has bun.lock."
            }),
            "missing bun package manager rule: {rendered:#?}"
        );
        assert!(
            rendered.iter().any(|(class, chk, text, ex)| {
                *class == Class::WrongDirectory
                    && *chk == Check::Verified("backend/go.mod".into())
                    && text == "Run `go` from `backend/`; the repository root has no go.mod."
                    && ex.as_ref().map(|e| e.2) == Some("go: no go.mod")
            }),
            "missing go.mod wrong directory rule: {rendered:#?}"
        );

        // bun.lockb also verifies WrongPackageManager for bun, and a root go.mod
        // makes the WrongDirectory rule stale.
        std::fs::remove_file(root.join("bun.lock")).unwrap();
        std::fs::write(root.join("bun.lockb"), b"bin").unwrap();
        let bun_candidate = analysis
            .candidates
            .iter()
            .find(|c| c.class == Class::WrongPackageManager)
            .unwrap();
        assert_eq!(
            check(root, &env, bun_candidate.class, &bun_candidate.slots),
            Check::Verified("bun.lockb".into())
        );

        std::fs::write(root.join("go.mod"), "module example.com/root\n").unwrap();
        let go_candidate = analysis
            .candidates
            .iter()
            .find(|c| c.class == Class::WrongDirectory)
            .unwrap();
        assert_eq!(
            check(root, &env, go_candidate.class, &go_candidate.slots),
            Check::Stale("go.mod exists".into())
        );
    }

    #[test]
    fn learns_modern_toolchain_recovery_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("pubspec.yaml"), "name: app\n").unwrap();
        std::fs::write(root.join(".fvmrc"), "{\"flutter\":\"3.24.0\"}\n").unwrap();
        std::fs::create_dir_all(root.join("infra")).unwrap();
        std::fs::write(root.join("infra/main.tf"), "terraform {}\n").unwrap();
        std::fs::create_dir_all(root.join("live")).unwrap();
        std::fs::write(root.join("live/terragrunt.hcl"), "terraform {}\n").unwrap();
        std::fs::create_dir_all(root.join("charts/app")).unwrap();
        std::fs::write(root.join("charts/app/Chart.yaml"), "name: app\n").unwrap();
        std::fs::create_dir_all(root.join("docker")).unwrap();
        std::fs::write(root.join("docker/Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(root.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(root.join("uv.lock"), "version = 1\n").unwrap();
        std::fs::write(root.join("pnpm-workspace.yaml"), "packages:\n  - apps/*\n").unwrap();

        let mk =
            |thread: &str, time_ms: u64, dir_rel: &str, argv: &[&str], code: i32, body: &str| {
                let cmd = Cmd {
                    checkout: String::new(),
                    dir: dir_rel.to_owned(),
                    argv: argv.iter().map(|s| (*s).to_owned()).collect(),
                    masked: false,
                };
                Event {
                    thread: thread.to_owned(),
                    session: thread.to_owned(),
                    transcript: false,
                    codex: false,
                    time_ms,
                    raw: argv.join(" "),
                    env: Vec::new(),
                    shape: events::Shape::Simple(cmd),
                    status: Status::Code(code),
                    texts: vec![events::Text {
                        run: Some(format!("run-{time_ms}")),
                        stream: "stderr",
                        body: body.to_owned(),
                    }],
                    tests: events::Tests::default(),
                    failure_bytes: body.len() as u64,
                    pointer: Pointer::Run {
                        id: format!("run-{time_ms}"),
                        stream: "stderr",
                    },
                }
            };

        let events = vec![
            // Flutter pub get prerequisite
            mk(
                "f1",
                1_000,
                "",
                &["flutter", "analyze"],
                1,
                "error - Target of URI doesn't exist: 'package:flutter/material.dart'.\n",
            ),
            mk(
                "f1",
                2_000,
                "",
                &["flutter", "pub", "get"],
                0,
                "Got dependencies!\n",
            ),
            mk(
                "f1",
                3_000,
                "",
                &["flutter", "analyze"],
                0,
                "No issues found!\n",
            ),
            // Dart build_runner codegen prerequisite
            mk(
                "f2",
                4_000,
                "",
                &["flutter", "test"],
                1,
                "error - Target of URI hasn't been generated: 'package:app/model.g.dart'.\n",
            ),
            mk(
                "f2",
                5_000,
                "",
                &[
                    "dart",
                    "run",
                    "build_runner",
                    "build",
                    "--delete-conflicting-outputs",
                ],
                0,
                "Succeeded\n",
            ),
            // Flutter FVM SDK mismatch wrapper
            mk(
                "f3",
                6_000,
                "",
                &["flutter", "test"],
                1,
                "Because app requires SDK version >=3.5.0 <4.0.0, version solving failed.\n",
            ),
            mk(
                "f3",
                7_000,
                "",
                &["fvm", "flutter", "test"],
                0,
                "All tests passed!\n",
            ),
            // Terraform init before plan (follow-up plan success must NOT contradict!)
            mk(
                "t1",
                10_000,
                "infra",
                &["terraform", "plan"],
                1,
                "Error: Initialization required. Please run \"terraform init\"\n",
            ),
            mk(
                "t1",
                11_000,
                "infra",
                &["terraform", "init"],
                0,
                "Terraform has been successfully initialized!\n",
            ),
            mk(
                "t1",
                12_000,
                "infra",
                &["terraform", "plan"],
                0,
                "No changes.\n",
            ),
            // OpenTofu init -upgrade
            mk(
                "t2",
                13_000,
                "infra",
                &["tofu", "init"],
                1,
                "Error: Inconsistent dependency lock file\nRun tofu init -upgrade\n",
            ),
            mk(
                "t2",
                14_000,
                "infra",
                &["tofu", "init", "-upgrade"],
                0,
                "OpenTofu initialized!\n",
            ),
            // Terragrunt init before validate
            mk(
                "t3",
                15_000,
                "live",
                &["terragrunt", "validate"],
                1,
                "Error: Module not installed\n",
            ),
            mk("t3", 16_000, "live", &["terragrunt", "init"], 0, "ok\n"),
            // Terraform state lock -> -lock=false
            mk(
                "t4",
                17_000,
                "infra",
                &["terraform", "plan"],
                1,
                "Error: Error acquiring the state lock\n",
            ),
            mk(
                "t4",
                18_000,
                "infra",
                &["terraform", "plan", "-lock=false"],
                0,
                "No changes.\n",
            ),
            // Terraform workspace mismatch (unknown workspace)
            mk(
                "t5",
                19_000,
                "infra",
                &["terraform", "workspace", "select", "stage"],
                1,
                "Error: Workspace \"stage\" doesn't exist.\n",
            ),
            mk(
                "t5",
                20_000,
                "infra",
                &["terraform", "workspace", "select", "production"],
                0,
                "Switched to workspace \"production\".\n",
            ),
            // Helm dependency update
            mk(
                "k1",
                21_000,
                "",
                &["helm", "template", "app", "charts/app"],
                1,
                "Error: found in Chart.yaml, but missing in charts/ directory: redis\n",
            ),
            mk(
                "k1",
                22_000,
                "",
                &["helm", "dependency", "update", "charts/app"],
                0,
                "Saving 1 charts\n",
            ),
            // kubectl exit-0 empty namespace + exit-1 NotFound -> -n staging (aggregated!)
            mk(
                "k2",
                23_000,
                "",
                &["kubectl", "get", "pods"],
                0,
                "No resources found in default namespace.\n",
            ),
            mk(
                "k2",
                24_000,
                "",
                &["kubectl", "-n", "staging", "get", "pods"],
                0,
                "NAME   READY   STATUS\napi-0  1/1     Running\n",
            ),
            mk(
                "k3",
                25_000,
                "",
                &["kubectl", "describe", "pod", "api-0"],
                1,
                "Error from server (NotFound): pods \"api-0\" not found\n",
            ),
            mk(
                "k3",
                26_000,
                "",
                &["kubectl", "describe", "pod", "api-0", "-n", "staging"],
                0,
                "Name: api-0\n",
            ),
            // Privacy guard: kubectl secrets must be dropped as credentials!
            mk(
                "k4",
                27_000,
                "",
                &["kubectl", "get", "secrets"],
                0,
                "No resources found in default namespace.\n",
            ),
            mk(
                "k4",
                28_000,
                "",
                &["kubectl", "-n", "prod", "get", "secrets"],
                0,
                "NAME TYPE\n",
            ),
            // Docker BuildKit: --build-arg (arg name only, never value!), --no-cache, buildx, -f
            mk(
                "d1",
                30_000,
                "",
                &["docker", "build", "."],
                1,
                "ERROR: failed to solve: invalid reference format\n",
            ),
            mk(
                "d1",
                31_000,
                "",
                &["docker", "build", "--build-arg", "APP_ENV=staging", "."],
                0,
                "DONE\n",
            ),
            // Privacy guard: secret build-arg must be dropped!
            mk(
                "d2",
                32_000,
                "",
                &["docker", "build", "."],
                1,
                "ERROR: failed to solve: missing build-arg\n",
            ),
            mk(
                "d2",
                33_000,
                "",
                &["docker", "build", "--build-arg=API_TOKEN=secret123", "."],
                0,
                "DONE\n",
            ),
            mk(
                "d3",
                34_000,
                "",
                &["docker", "build", "."],
                1,
                "ERROR: failed to solve: failed to compute cache key: not found\n",
            ),
            mk(
                "d3",
                35_000,
                "",
                &["docker", "build", "--no-cache", "."],
                0,
                "DONE\n",
            ),
            mk(
                "d4",
                36_000,
                "",
                &[
                    "docker",
                    "build",
                    "--platform",
                    "linux/amd64,linux/arm64",
                    ".",
                ],
                1,
                "ERROR: Multi-platform build is not supported for the docker driver.\n",
            ),
            mk(
                "d4",
                37_000,
                "",
                &[
                    "docker",
                    "buildx",
                    "build",
                    "--platform",
                    "linux/amd64,linux/arm64",
                    ".",
                ],
                0,
                "DONE\n",
            ),
            mk(
                "d5",
                38_000,
                "",
                &["docker", "build", "."],
                1,
                "ERROR: failed to solve: failed to read dockerfile: open Dockerfile: no such file or directory\n",
            ),
            mk(
                "d5",
                39_000,
                "",
                &["docker", "build", "-f", "docker/Dockerfile", "."],
                0,
                "DONE\n",
            ),
            // uv workflow + pnpm monorepo filter
            mk(
                "u1",
                40_000,
                "",
                &["pytest", "tests/"],
                1,
                "ModuleNotFoundError: No module named 'fastapi'\n",
            ),
            mk(
                "u1",
                41_000,
                "",
                &["uv", "run", "pytest", "tests/"],
                0,
                "1 passed\n",
            ),
            mk(
                "u2",
                42_000,
                "",
                &["uv", "run", "--locked", "pytest"],
                1,
                "error: The lockfile at `uv.lock` needs to be updated, but `--locked` was provided.\n",
            ),
            mk(
                "u2",
                43_000,
                "",
                &["uv", "sync"],
                0,
                "Resolved 5 packages\n",
            ),
            mk(
                "u3",
                44_000,
                "",
                &["pnpm", "--filter", "web", "build"],
                1,
                "ERR_PNPM_NO_MATCHING_WORKSPACE  No projects matched the filters\n",
            ),
            mk(
                "u3",
                45_000,
                "",
                &["pnpm", "--filter", "@app/web", "build"],
                0,
                "done\n",
            ),
        ];

        let analysis = analyze(root, &events);
        assert!(
            analysis.credentials >= 1,
            "expected credential drops for kubectl secrets / secret build-arg: {}",
            analysis.credentials
        );
        let env = Env {
            path: None,
            shell: None,
        };
        let rendered: Vec<_> = analysis
            .candidates
            .iter()
            .map(|c| {
                assert!(!c.contradicted, "unexpected contradiction on {:?}", c.slots);
                let chk = check(root, &env, c.class, &c.slots);
                let text = render(c.class, &c.slots, &chk).expect("renderable");
                (c.class, chk, text, c.sessions.len())
            })
            .collect();

        for (expected_class, expected_text, expected_check) in [
            (
                Class::PrerequisiteStep,
                "Run `flutter pub get` before `flutter analyze` when dependencies are out of date.",
                Some(Check::Verified("pubspec.yaml".into())),
            ),
            (
                Class::PrerequisiteStep,
                "Run `dart run build_runner build --delete-conflicting-outputs` before `flutter test` when generated files are missing.",
                Some(Check::Verified("pubspec.yaml".into())),
            ),
            (
                Class::ToolchainWrapper,
                "Use `fvm flutter`, not `flutter`; this project pins its SDK in .fvmrc.",
                Some(Check::Verified(".fvmrc".into())),
            ),
            (
                Class::PrerequisiteStep,
                "Run `terraform init` before `terraform plan` when the working directory is not initialized.",
                Some(Check::Verified("infra/main.tf".into())),
            ),
            (
                Class::MissingOption,
                "Pass `-upgrade` to `tofu init`.",
                Some(Check::Verified("infra/main.tf".into())),
            ),
            (
                Class::PrerequisiteStep,
                "Run `terragrunt init` before `terragrunt validate` when the working directory is not initialized.",
                Some(Check::Verified("live/terragrunt.hcl".into())),
            ),
            (
                Class::MissingOption,
                "Pass `-lock=false` to `terraform plan`.",
                Some(Check::Verified("infra/main.tf".into())),
            ),
            (
                Class::UnknownTarget,
                "Use `terraform workspace select production`; there is no workspace `stage`.",
                Some(Check::Verified("infra/main.tf".into())),
            ),
            (
                Class::PrerequisiteStep,
                "Run `helm dependency update` before `helm template` when chart dependencies are missing.",
                Some(Check::Verified("charts/app/Chart.yaml".into())),
            ),
            (
                Class::MissingOption,
                "Pass `-n staging` to `kubectl`.",
                Some(Check::Unverified),
            ),
            (
                Class::MissingOption,
                "Pass `--build-arg APP_ENV=...` to `docker build`.",
                Some(Check::Verified("Dockerfile".into())),
            ),
            (
                Class::MissingOption,
                "Pass `--no-cache` to `docker build`.",
                Some(Check::Verified("Dockerfile".into())),
            ),
            (
                Class::ToolchainWrapper,
                "Use `docker buildx build`, not `docker build`, for BuildKit features here.",
                Some(Check::Verified("Dockerfile".into())),
            ),
            (
                Class::MissingOption,
                "Pass `-f docker/Dockerfile` to `docker build`.",
                Some(Check::Verified("docker/Dockerfile".into())),
            ),
            (
                Class::ToolchainWrapper,
                "Use `uv run pytest`, not `pytest`; this project has uv.lock.",
                Some(Check::Verified("uv.lock".into())),
            ),
            (
                Class::PrerequisiteStep,
                "Run `uv sync` before `uv run` when the environment or uv.lock is out of date.",
                Some(Check::Verified("uv.lock".into())),
            ),
            (
                Class::UnknownTarget,
                "Use `pnpm --filter @app/web`; no workspace matched `web`.",
                Some(Check::Verified("pnpm-workspace.yaml".into())),
            ),
        ] {
            assert!(
                rendered.iter().any(|(c, chk, text, _)| {
                    *c == expected_class
                        && text == expected_text
                        && expected_check.as_ref().is_none_or(|want| chk == want)
                }),
                "missing expected rule {expected_text:?} ({expected_class:?}) in:\n{rendered:#?}"
            );
        }

        // kubectl `-n staging` aggregated across both k2 (exit 0 empty ns) and k3 (exit 1 NotFound)!
        let k_sessions = rendered
            .iter()
            .find(|(_, _, text, _)| text == "Pass `-n staging` to `kubectl`.")
            .unwrap()
            .3;
        assert_eq!(k_sessions, 2);

        // No secret or build-arg value ever leaked into any rendered rule.
        for (_, _, text, _) in &rendered {
            assert!(!text.contains("API_TOKEN"), "{text}");
            assert!(!text.contains("secret123"), "{text}");
            assert!(!text.contains("APP_ENV=staging"), "{text}");
            assert!(!text.contains("-n prod"), "{text}");
        }
    }

    #[test]
    fn rejects_eks_arn_contexts_and_scopes_kubectl_contexts_to_machine() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let mk = |thread: &str, time_ms: u64, argv: &[&str], code: i32, body: &str| {
            let cmd = Cmd {
                checkout: String::new(),
                dir: String::new(),
                argv: argv.iter().map(|s| (*s).to_owned()).collect(),
                masked: false,
            };
            Event {
                thread: thread.to_owned(),
                session: thread.to_owned(),
                transcript: false,
                codex: false,
                time_ms,
                raw: argv.join(" "),
                env: Vec::new(),
                shape: events::Shape::Simple(cmd),
                status: Status::Code(code),
                texts: vec![events::Text {
                    run: Some(format!("run-{time_ms}")),
                    stream: "stderr",
                    body: body.to_owned(),
                }],
                tests: events::Tests::default(),
                failure_bytes: body.len() as u64,
                pointer: Pointer::Run {
                    id: format!("run-{time_ms}"),
                    stream: "stderr",
                },
            }
        };
        let arn = "arn:aws:eks:us-east-1:123456789012:cluster/prod";
        let unset = "The connection to the server localhost:8080 was refused - did you specify the right host or port?\n";
        let events = vec![
            // ARN via `--context`
            mk("a1", 1_000, &["kubectl", "get", "pods"], 1, unset),
            mk(
                "a1",
                2_000,
                &["kubectl", "--context", arn, "get", "pods"],
                0,
                "NAME\napi-0\n",
            ),
            // ARN via `kubectl config use-context`
            mk("a2", 3_000, &["kubectl", "get", "pods"], 1, unset),
            mk(
                "a2",
                4_000,
                &["kubectl", "config", "use-context", arn],
                0,
                "Switched to context\n",
            ),
            // Plain local context via `--context`
            mk("b1", 5_000, &["kubectl", "get", "pods"], 1, unset),
            mk(
                "b1",
                6_000,
                &["kubectl", "--context", "dev-cluster", "get", "pods"],
                0,
                "NAME\napi-0\n",
            ),
            // Plain local context via `kubectl config use-context`
            mk("b2", 7_000, &["kubectl", "get", "pods"], 1, unset),
            mk(
                "b2",
                8_000,
                &["kubectl", "config", "use-context", "dev-cluster"],
                0,
                "Switched to context\n",
            ),
        ];
        let analysis = analyze(root, &events);
        let env = Env {
            path: None,
            shell: None,
        };
        assert_eq!(analysis.candidates.len(), 2);
        for c in &analysis.candidates {
            assert_eq!(
                scope(c.class, &c.slots),
                Scope::Machine,
                "kubectl context rule must be machine-scoped: {:?}",
                c.slots
            );
            let chk = check(root, &env, c.class, &c.slots);
            let text = render(c.class, &c.slots, &chk).unwrap();
            assert!(!text.contains("123456789012"), "{text}");
            assert!(!text.contains("arn:"), "{text}");
            assert!(text.contains("dev-cluster"), "{text}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn check_rejects_symlinked_file_and_kubeconfig_outside_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let outside = tempfile::TempDir::new().unwrap();
        let outside_kube = outside.path().join("config");
        std::fs::write(&outside_kube, "apiVersion: v1\n").unwrap();

        std::fs::write(root.join("local-kube.yaml"), "apiVersion: v1\n").unwrap();
        std::os::unix::fs::symlink(&outside_kube, root.join("link-kube.yaml")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("symdir")).unwrap();

        let env = Env {
            path: None,
            shell: None,
        };
        let slots_for = |flag: &str, value: &str| {
            BTreeMap::from([
                ("stem".to_owned(), "kubectl".to_owned()),
                ("flag".to_owned(), flag.to_owned()),
                ("value".to_owned(), value.to_owned()),
                ("dir".to_owned(), String::new()),
            ])
        };

        assert_eq!(
            check(
                root,
                &env,
                Class::MissingOption,
                &slots_for("--kubeconfig", "local-kube.yaml")
            ),
            Check::Verified("local-kube.yaml".into())
        );
        for (flag, value) in [
            ("--kubeconfig", "link-kube.yaml"),
            ("--kubeconfig", "symdir/config"),
            ("-f", "link-kube.yaml"),
            ("--file", "symdir/config"),
            ("--kubeconfig", "../config"),
        ] {
            assert!(
                matches!(
                    check(root, &env, Class::MissingOption, &slots_for(flag, value)),
                    Check::Stale(_)
                ),
                "expected stale for {flag} {value}"
            );
        }
    }
}
