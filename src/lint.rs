//! Group compiler and linter findings by rule across both output streams.
//!
//! Recognized shapes:
//! - rustc, Clippy, Ruff, Deno (`deno lint` / `deno check`), Oxlint and Biome blocks:
//!   a `warning:`/`error[E0308]:`/`F401`/`TS2322 [ERROR]:` header followed by a
//!   ` --> path:line:col` or `    at file:///path:line:col` location and detail lines.
//! - ShellCheck's default `In path line N:` blocks with `^-- SC2086 (info): message` rows.
//! - One finding per line: `path:line[:col]: message` (Ruff and flake8 concise output,
//!   pylint, mypy, rustc `--message-format=short`, ShellCheck `-f gcc`, golangci-lint,
//!   go vet, go build, staticcheck, RuboCop, SwiftLint, swiftc, gcc, clang, clang-tidy,
//!   cppcheck, yamllint `-f parsable`, ktlint, ansible-lint `-p`, actionlint, and Credo
//!   `--format flycheck`), PHPStan raw (`path.php:line:message [id]`), cppcheck classic
//!   (`[path:line]: (severity) message [id]`), markdownlint/markdownlint-cli2
//!   (`path:line[:col] MD013/line-length message`), stylelint compact
//!   (`path: line N, col M, error - message (rule)`), TypeScript's
//!   `path(line,col): error TS2322: message`, MSBuild/`dotnet`'s
//!   `path(line,col): error CS0103: message [project.csproj]`, Maven's
//!   `[ERROR] path:[line,col] message`, and Kotlin/Gradle's
//!   `e: file://path:line:col message`. Indented lines and snippet rows that follow one
//!   of these are kept as its detail; follow-up `note:` lines extend only the archive
//!   range `[stream:A-B]` without being counted as findings.
//! - File-header tables and blocks: ESLint, stylelint default (`✖`/`⚠` rows), yamllint
//!   standard (`(rule-name)` rows), PHPStan tables (`Line   path` + `🪪  id`),
//!   SQLFluff (`== [path] FAIL` + `L: N | P: M | RULE | message`), Ansible-lint two-line
//!   blocks (`rule[sub]: message` + `path:line`), and Credo (`[W] ↗ message` +
//!   `┃   path:line:col` and `--format oneline`).
//! - Pyright's `  path:line:col - error: message (rule)` rows, hadolint's
//!   `path:line DL3008 warning: message`, and Dart/Flutter analyzer's bullet (`•`)
//!   and dash (`-`) diagnostic rows (`severity • message • path:line:col • rule`
//!   and `severity - path:line:col - message - rule`).
//!
//! Errors without a source location and the tools' own totals stay visible as outcome
//! lines. A stream that produced no finding is summarized by its line range, so other
//! output such as a crash is never silently hidden. Findings are only reported when
//! at least one was recognized, so other output keeps the ordinary preview.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::capture::Stream;
use crate::summary::{Item, Summary, short};

/// Named ecosystem linters supported by [`Lint`].
///
/// [`Lint::new`] auto-detects every recognized shape without a hint; callers that
/// already know the invoked tool may pass a [`Tool`] via [`Lint::for_tool`].
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    Phpstan,
    ClangTidy,
    Cppcheck,
    Yamllint,
    Markdownlint,
    Sqlfluff,
    Ktlint,
    Stylelint,
    Deno,
    AnsibleLint,
    Actionlint,
    Credo,
    DartAnalyze,
}

#[allow(dead_code)]
impl Tool {
    pub const ALL: &[Self] = &[
        Self::Phpstan,
        Self::ClangTidy,
        Self::Cppcheck,
        Self::Yamllint,
        Self::Markdownlint,
        Self::Sqlfluff,
        Self::Ktlint,
        Self::Stylelint,
        Self::Deno,
        Self::AnsibleLint,
        Self::Actionlint,
        Self::Credo,
        Self::DartAnalyze,
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "phpstan" => Some(Self::Phpstan),
            "clang-tidy" => Some(Self::ClangTidy),
            "cppcheck" => Some(Self::Cppcheck),
            "yamllint" => Some(Self::Yamllint),
            "markdownlint" | "markdownlint-cli2" => Some(Self::Markdownlint),
            "sqlfluff" => Some(Self::Sqlfluff),
            "ktlint" => Some(Self::Ktlint),
            "stylelint" => Some(Self::Stylelint),
            "deno" => Some(Self::Deno),
            "ansible-lint" => Some(Self::AnsibleLint),
            "actionlint" => Some(Self::Actionlint),
            "credo" | "mix credo" => Some(Self::Credo),
            "dart-analyze" | "dart analyze" | "flutter analyze" => Some(Self::DartAnalyze),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Phpstan => "phpstan",
            Self::ClangTidy => "clang-tidy",
            Self::Cppcheck => "cppcheck",
            Self::Yamllint => "yamllint",
            Self::Markdownlint => "markdownlint",
            Self::Sqlfluff => "sqlfluff",
            Self::Ktlint => "ktlint",
            Self::Stylelint => "stylelint",
            Self::Deno => "deno",
            Self::AnsibleLint => "ansible-lint",
            Self::Actionlint => "actionlint",
            Self::Credo => "credo",
            Self::DartAnalyze => "dart-analyze",
        }
    }
}

const MAX_FINDINGS: usize = 20_000;
const EXCERPT_LINES: usize = 10;
const SHOWN_LOCATIONS: usize = 4;
const SHOWN_OUTCOMES: usize = 4;
const SHOWN_CHANGES: usize = 24;
const SHOWN_PACKAGES: usize = 6;
const SHOWN_TARGETS: usize = 4;

static RUST_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(error|warning)(?:\[([A-Za-z0-9_/-]+)\])?: (.+)$").unwrap());
static DENO_CHECK_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(TS\d+)\s+\[(ERROR|WARNING)\]:\s+(.+)$").unwrap());
static CODE_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Z]{1,4}[0-9]{2,5}) (?:\[\*\] )?(.+)$").unwrap());
static OXLINT_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*([×✖!⚠])\s+([a-z0-9_-]+\([a-z0-9_/-]+\)):\s+(.+)$").unwrap());
static BIOME_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\S.*?):(\d+):(\d+)\s+((?:lint|format|organizeImports|assist|parse)[/\w-]+)\s+━+")
        .unwrap()
});
static BIOME_MSG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s+([×✖!⚠ℹ])\s+(.+)$").unwrap());
static ARROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:(?:-->|╭▸|┌─)\s*(.+?):(\d+)(?::(\d+))?\s*|╭─\[(.+?):(\d+)(?::(\d+))?\]\s*)$",
    )
    .unwrap()
});
static AT_LOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+at\s+(?:file://)?([^()\r\n]+?\.[A-Za-z0-9]+):(\d+)(?::(\d+))?\s*$").unwrap()
});
static ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"#\[(?:warn|deny|forbid|allow|expect)\(([A-Za-z0-9_:]+)\)\]|`-[DWF] ([A-Za-z0-9_:-]+)` implied by|index\.html#([a-z0-9_]+)",
    )
    .unwrap()
});
// Source excerpts and suggested edits inside a rustc block, which can quote attributes.
static SNIPPET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:\d*\s*\||\d+\s*[+~-]\s)").unwrap());
static TSC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\S.*?)\((\d+),(\d+)\): (error|warning) (TS\d+): (.+)$").unwrap()
});
static MSBUILD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(\S.*?)\((\d+),(\d+)\): (error|warning) ([A-Z]{1,6}\d+): (.+?)(?: \[[^\]]+\])?$",
    )
    .unwrap()
});
static MAVEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[(ERROR|WARNING)\] (.+?):\[(\d+)(?:,(\d+))?\] (.+)$").unwrap());
static JAVAC_LINT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[([a-z][a-z0-9_-]*)\]\s+(.+)$").unwrap());
static KOTLIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([ew]): (?:file://)?(.+?):(\d+):(\d+):?\s+(.+)$").unwrap());
static GO_PACKAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^# [A-Za-z0-9_./-]+(?: \[[A-Za-z0-9_./-]+\])?$").unwrap());
static CPPCHECK_BRACKET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:\[.+?:\d+(?::\d+)?\]\s*->\s*)?\[([^\[\]]+?):(\d+)(?::(\d+))?\]:\s+\((error|warning|style|performance|portability|information)\)\s+(.+)$",
    )
    .unwrap()
});
static MARKDOWNLINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(\S.*?):(\d+)(?::(\d+))?:?\s+(?:(error|warning)\s+)?(MD\d{3}(?:/[a-z0-9-]+)*)\s+(.+)$",
    )
    .unwrap()
});
static STYLELINT_COMPACT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(\S.*?):\s+line\s+(\d+),\s+col\s+(\d+),\s+(error|warning)\s+-\s+(.+?)(?:\s+\((@?[\w./-]+)\))?\s*$",
    )
    .unwrap()
});
static SQLFLUFF_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^== \[(.+?)\] (FAIL|WARNING|PASS|SKIP)\s*$").unwrap());
static SQLFLUFF_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^L:\s*(\d+)\s*\|\s*P:\s*(\d+)\s*\|\s*([A-Z][A-Z0-9]{1,5})\s*\|\s*(.+?)\s*$")
        .unwrap()
});
static PHPSTAN_BORDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*-{3,}\s+-{3,}\s*$").unwrap());
static PHPSTAN_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s+Line\s{2,}(\S.*?)\s*$").unwrap());
static PHPSTAN_GENERAL_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s+Error\s*$").unwrap());
static PHPSTAN_ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s+(\d+|-)\s{2,}(.+?)\s*$").unwrap());
static PHPSTAN_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*🪪\s+([A-Za-z][A-Za-z0-9_.-]*)\s*$").unwrap());
static PHPSTAN_RAW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^([^\s:](?:[^:]|:[^\s\d])*?\.(?:php|phtml|inc|module|install|theme)):(\d+):([^\s:].*)$",
    )
    .unwrap()
});
static CREDO_ONELINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:[┃|]\s*)?\[([WDRFC])\]\s+[↑↗→↘↓]\s+([^\s:](?:[^:]|:[^\s\d])*?\.[A-Za-z0-9]+):(\d+)(?::(\d+))?\s+(.+?)\s*$",
    )
    .unwrap()
});
static CREDO_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[┃|]\s*)?\[([WDRFC])\]\s+[↑↗→↘↓]\s+(.+?)\s*$").unwrap());
static CREDO_LOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:[┃|]\s*|\s+)([^\s:](?:[^:]|:[^\s\d])*?\.[A-Za-z0-9]+):(\d+)(?::(\d+))?(?:\s+#\([^)]*\))?\s*$",
    )
    .unwrap()
});
static ANSIBLE_BLOCK_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-z][a-z0-9_-]*(?:\[[a-z0-9_.-]+\])?):\s+(.+)$").unwrap());
static ANSIBLE_LOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^\s:](?:[^:]|:[^\s\d])*?\.[A-Za-z0-9]+):(\d+)(?::(\d+))?(?:$|\s+.*)$").unwrap()
});
// A path may contain spaces and a colon that is not followed by whitespace or a digit,
// such as a Windows drive, so `note: see a.rs:3: x` and `warning: …` never match.
static LOCATED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^\s:](?:[^:]|:[^\s\d])*?\.[A-Za-z0-9]+):(\d+)(?::(\d+))?:\s+(.+)$").unwrap()
});
static SEVERITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?i)(fatal error|error|warning|note|info|information|style|performance|portability|hint|refactor|convention)(?:\[([A-Za-z0-9_]+)\])?:\s*(.+)$",
    )
    .unwrap()
});
static BRACKET_SEVERITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?i)\[(error|warning)\]\s+(.+)$").unwrap());
static RUBOCOP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([CWEFR]): (?:\[Correctable\] )?([A-Z][A-Za-z]+/[A-Za-z0-9]+): (.+)$").unwrap()
});
static CREDO_FLYCHECK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([WDRFC]):\s+(.+)$").unwrap());
static ANSIBLE_SUB_RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-z][a-z0-9_-]*\[[a-z0-9_.-]+\]):\s+(.+)$").unwrap());
static ANSIBLE_KEBAB_RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-z][a-z0-9]*-[a-z0-9-]+):\s+(.+)$").unwrap());
static ANSIBLE_YAML_RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-z][a-z0-9_-]+):\s+(.+)$").unwrap());
static LEADING_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Z]{1,4}[0-9]{2,5})(:?) (?:\[\*\] )?(.+)$").unwrap());
static TRAILING_BRACKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+?)\s+\[(-?[A-Za-z][\w.:/,=-]*)\]$").unwrap());
static TRAILING_PAREN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(.+?)\s+\(([a-z][a-z0-9_-]*(?::[a-z][a-z0-9_-]+)?|[A-Z]{1,4}[0-9]{2,5})\)$")
        .unwrap()
});
// ESLint, stylelint and yamllint print file headers. Absolute names may contain spaces;
// relative names stay strict so source lines such as ShellCheck's `ls $DIR/*.txt` are never headers.
static ESLINT_FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(?:/|[A-Za-z]:[\\/])[^:]*?|(?:\.{0,2}/)?[^\s:]+)\.[A-Za-z0-9]+$").unwrap()
});
static YAMLLINT_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+(\d+):(\d+)\s+(error|warning)\s+(.+?)\s{2,}\(([a-z][a-z0-9_-]+)\)\s*$")
        .unwrap()
});
static STYLELINT_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+(\d+):(\d+)\s+(✖|×|⚠|‼|!)\s+(.+?)(?:\s{2,}(@?[\w./-]+))?\s*$").unwrap()
});
static ESLINT_ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+(\d+):(\d+)\s+(error|warning)\s+(.+?)(?:\s{2,}(@?[\w./-]+))?\s*$").unwrap()
});
static PYRIGHT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s+(\S.*?):(\d+):(\d+) - (error|warning|information): (.*)$").unwrap()
});
static PYRIGHT_RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.*?)\s*\((report[A-Za-z]+)\)\s*$").unwrap());
static HADOLINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\S.*?):(\d+)(?::(\d+))? ((?:DL|SC)\d{4}) (error|warning|info|style): (.+)$")
        .unwrap()
});
static DART_ANALYZE_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?i)(error|warning|info|hint)\s+(•|-)\s+(.+)$").unwrap());
static DART_ANALYZE_LOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([^\s:](?:[^:]|:[^\s\d])*?\.[A-Za-z0-9]+):(\d+)(?::(\d+))?$").unwrap()
});
static DART_ANALYZE_RULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9_.-]*$").unwrap());
static DART_ANALYZE_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"^(?:Analyzing .+\.\.\.(?:\s+\d+(?:\.\d+)?s)?$|No issues found!(?: \(ran in [^)]+\))?$|Running "flutter pub get" in .+\.\.\.(?:\s+\d+(?:\.\d+)?s)?$)"#,
    )
    .unwrap()
});
static SHELLCHECK_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^In (.+) line (\d+):$").unwrap());
static SHELLCHECK_ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\^-*\^?\s*(SC\d+) \((\w+)\): (.+)$").unwrap());
// A tool's own count, kept so a mismatch with the grouped header stays visible.
static TOTAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:Found \d+ (?:errors?|warnings?|problems?|issues?|diagnostics?)\b|\d+ (?:errors?|problems?|issues?|offenses?)\b|\d+ files? inspected\b|Summary: [1-9]\d* error\(s\)|Failed: \d+ failure\(s\)|\d+ mods/funs, found [1-9]\d* |\d+ (?:warnings? and \d+ )?errors? generated\.?$)",
    )
    .unwrap()
});
static CLANG_TIDY_GENERATED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+) warnings? generated\.?$").unwrap());
static CLANG_TIDY_SUPPRESSED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Suppressed (\d+) warnings? \([^)]*\)\.?$").unwrap());
static CLANG_TIDY_HEADER_FILTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Use -header-filter=\S+ to display ").unwrap());
static PHPSTAN_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:Note: Using configuration file \S|\s*\d+/\d+\s+\[[▓░= >#-]+\]\s+\d+%\s*$|\s*\[OK\] No errors\s*$)",
    )
    .unwrap()
});
static CPPCHECK_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:Checking \S.* \.\.\.$|\d+/\d+ files? checked \d+% done$|nofile:0:0: information: Active checkers: .*\[checkersReport\]$)",
    )
    .unwrap()
});
static MARKDOWNLINT_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:markdownlint-cli2 v\S+|Finding: \S|Linting: \d+ file\(s\)$|Summary: 0 error\(s\)$)",
    )
    .unwrap()
});
static SQLFLUFF_CLEAN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:== \[.+?\] (?:PASS|SKIP)\s*$|All Finished\b)").unwrap());
static DENO_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:Checked \d+ files?(?: (?:in \S+|\([^)]+\)))?\.?$|Check (?:file|https?)://\S+$|Download https?://\S+$)",
    )
    .unwrap()
});
static ANSIBLE_CLEAN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:Passed(?: with [^:]+)?: 0 failure\(s\), 0 warning\(s\)(?:$|\s|\.)|WARNING\s+Listing 0 violation\(s\)(?:$|\s|\.))",
    )
    .unwrap()
});
static CREDO_CLEAN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:Checking \d+ source files? \.\.\.$|Please report incorrect results: https://github\.com/rrrene/credo/issues$|Analysis took \S+ seconds? \(.+\)$|\d+ mods/funs, found no issues\.$|Use `mix credo explain` to explain issues\b)",
    )
    .unwrap()
});
// The package and, in recent Cargo, the target, such as `(lib test)` or `(bin "x")`.
static UNCOMPILED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^error: could not compile `([^`]+)`(?: \(([^)]+)\))?").unwrap());
pub(crate) static CARGO_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^ +(?:Compiling|Checking|Finished|Building|Blocking|Fresh|Documenting|Generated|Scraping|Downloaded|Downloading|Updating|Locking|Adding|Running|Packaging|Verifying) \S",
    )
    .unwrap()
});
static CMAKE_PROGRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\[\s*\d{1,3}%\] (?:Building|Linking|Built target|Generating|Scanning dependencies|Consolidate compiler generated dependencies|Automatic (?:MOC|UIC|RCC)|Creating library|Copying|Updating) \S",
    )
    .unwrap()
});
static CMAKE_DONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^-- (?:Configuring done|Generating done|Build files have been written to:)")
        .unwrap()
});
static CMAKE_BUILT_FINAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[\s*100%\] Built target \S").unwrap());
static STEP_PROGRESS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[\d+/\d+\] \S").unwrap());
static NINJA_STATUS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ninja: (?:no work to do\.|Entering directory )").unwrap());
static MAKE_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^g?make(?:\[\d+\])?: (?:Entering directory |Leaving directory |Nothing to be done for |.+ is up to date\.$)",
    )
    .unwrap()
});
static GRADLE_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:> Task :[A-Za-z0-9_.:-]+(?: (?:UP-TO-DATE|NO-SOURCE|FROM-CACHE|SKIPPED))?|> Configure project :[A-Za-z0-9_.:-]*|BUILD SUCCESSFUL(?: in .+)?|\d+ actionable tasks?: (?:\d+ (?:executed|up-to-date|from cache)(?:, )?)+)$",
    )
    .unwrap()
});
static DOTNET_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:MSBuild version \S|\s*Determining projects to restore\.\.\.|\s*All projects are up-to-date for restore\.|\s*Nothing to do\. None of the projects specified contain packages to restore\.|\s*Restored \S|\s*Restore complete\b|  +\S+ -> \S|  +\S+ succeeded \(\S+\)(?: → \S+)?$|Build succeeded(?:\.| in \S+)$|\s*0 Warning\(s\)$|\s*0 Error\(s\)$|Time Elapsed \d{2}:\d{2}:\d{2})",
    )
    .unwrap()
});
static SWIFT_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:Building for (?:debugging|release)\.\.\.|Planning build|Build (?:of product '[^']+' )?complete!(?: \(\S+\))?)$",
    )
    .unwrap()
});
static GO_STATUS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^go: downloading \S+ \S+$").unwrap());
// Unlocated linker and compiler/project errors that must stay visible even when
// the same stream also contains located warnings or errors.
static UNLOCATED_ERROR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:(?:[^\s:]*[/\\])?(?:ld(?:\.(?:lld|gold|bfd))?|lld|collect2|clang(?:\+\+)?|gcc|g\+\+|cc|c\+\+|link(?:\.exe)?|ar|ranlib|swiftc|swift-frontend|javac|kotlinc):.*?(?:\berror\b|undefined reference|undefined symbol|multiple definition|cannot find -l|returned \d+ exit status|linker command failed)|(?i:undefined (?:reference to|symbols? for)\b)|(?i:(?:csc|vbc|fsc|msbuild)\s*:\s*error\s+[A-Z]{1,6}\d+:)|\S+\.(?:csproj|fsproj|vbproj|vcxproj|sln|props|targets)\s*:\s*error\s+[A-Z]{1,6}\d+:)",
    )
    .unwrap()
});
static BUILD_OUTCOME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:g?make(?:\[\d+\])?: \*\*\*|ninja: build stopped:|FAILED: |FAILURE: |BUILD FAILED\b|Build FAILED\.)",
    )
    .unwrap()
});

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    Cargo,
    Build,
    Lint,
}

/// Recognize a clean build or linter status line, whether it carries the final
/// completion notice for the one-line clean summary, and whether it requires a
/// completion notice before [`Lint::clean`] may succeed.
fn build_status(line: &str) -> Option<(StatusKind, bool, bool)> {
    if CARGO_STATUS.is_match(line) {
        return Some((
            StatusKind::Cargo,
            line.trim_start().starts_with("Finished "),
            false,
        ));
    }
    if CMAKE_PROGRESS.is_match(line) {
        return Some((StatusKind::Build, CMAKE_BUILT_FINAL.is_match(line), false));
    }
    if CMAKE_DONE.is_match(line) {
        return Some((
            StatusKind::Build,
            line.starts_with("-- Build files have been written to:"),
            false,
        ));
    }
    if STEP_PROGRESS.is_match(line)
        && !line.contains("FAILED")
        && !line.contains("error:")
        && !line.contains("warning:")
    {
        return Some((StatusKind::Build, false, false));
    }
    if NINJA_STATUS.is_match(line) {
        return Some((StatusKind::Build, line == "ninja: no work to do.", false));
    }
    if MAKE_STATUS.is_match(line) {
        return Some((
            StatusKind::Build,
            line.contains("Nothing to be done for ") || line.ends_with(" is up to date."),
            false,
        ));
    }
    if GRADLE_STATUS.is_match(line) {
        return Some((
            StatusKind::Build,
            line.starts_with("BUILD SUCCESSFUL"),
            false,
        ));
    }
    if (line == "[INFO]" || line.starts_with("[INFO] "))
        && !line.starts_with("[INFO] Tests run:")
        && !line.contains("BUILD FAILURE")
    {
        return Some((
            StatusKind::Build,
            line.trim() == "[INFO] BUILD SUCCESS",
            false,
        ));
    }
    if DOTNET_STATUS.is_match(line) {
        return Some((
            StatusKind::Build,
            line.starts_with("Build succeeded"),
            false,
        ));
    }
    if SWIFT_STATUS.is_match(line) {
        return Some((
            StatusKind::Build,
            line.starts_with("Build ") && line.contains("complete!"),
            false,
        ));
    }
    if GO_STATUS.is_match(line) {
        return Some((StatusKind::Build, false, false));
    }
    if PHPSTAN_STATUS.is_match(line) {
        let finished = line.trim() == "[OK] No errors";
        return Some((StatusKind::Lint, finished, !finished));
    }
    if let Some(c) = CLANG_TIDY_GENERATED.captures(line) {
        let count: usize = c[1].parse().unwrap_or(1);
        return Some((StatusKind::Lint, count == 0, count > 0));
    }
    if CLANG_TIDY_SUPPRESSED.is_match(line) {
        return Some((StatusKind::Lint, true, false));
    }
    if CLANG_TIDY_HEADER_FILTER.is_match(line) {
        return Some((StatusKind::Lint, false, true));
    }
    if CPPCHECK_STATUS.is_match(line) {
        return Some((StatusKind::Lint, line.ends_with("100% done"), false));
    }
    if MARKDOWNLINT_STATUS.is_match(line) {
        let finished = line == "Summary: 0 error(s)";
        return Some((StatusKind::Lint, finished, !finished));
    }
    if SQLFLUFF_CLEAN.is_match(line) {
        let finished = line.starts_with("All Finished");
        return Some((StatusKind::Lint, finished, false));
    }
    if DENO_STATUS.is_match(line) {
        return Some((StatusKind::Lint, line.starts_with("Checked "), false));
    }
    if ANSIBLE_CLEAN.is_match(line) {
        let finished = line.starts_with("Passed");
        return Some((StatusKind::Lint, finished, !finished));
    }
    if CREDO_CLEAN.is_match(line) {
        let finished = line.ends_with("found no issues.");
        return Some((StatusKind::Lint, finished, !finished));
    }
    if DART_ANALYZE_STATUS.is_match(line) {
        let finished = line.starts_with("No issues found!");
        return Some((StatusKind::Lint, finished, !finished));
    }
    None
}

fn is_build_outcome(text: &str) -> bool {
    if let Some(rest) = text.strip_prefix("[ERROR]") {
        let rest = rest.trim();
        return !rest.is_empty()
            && !rest.chars().all(|c| c == '-' || c == '=')
            && !rest.starts_with("-> [Help ")
            && !rest.starts_with("To see the full stack trace")
            && !rest.starts_with("Re-run Maven")
            && !rest.starts_with("For more information");
    }
    BUILD_OUTCOME.is_match(text)
}
// rustc names a lint only on its first diagnostic, and the wording of later ones
// varies with the item kind and plural forms. `--message-format=short` names none.
static RUSTC_LINTS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (
            r"^value (?:assigned to|captured by) .* is never read",
            "unused_assignments",
        ),
        (
            r"^unused variables?: |^variables? .* (?:is|are) assigned to, but never used",
            "unused_variables",
        ),
        (r"^unused imports?: ", "unused_imports"),
        (r"^variables? does not need to be mutable", "unused_mut"),
        (r"^unused `[^`]+` that must be used", "unused_must_use"),
        (
            r"\b(?:is|are) never (?:used|constructed|read)\b",
            "dead_code",
        ),
        (r"\bshould have a snake case name\b", "non_snake_case"),
        (
            r"\bshould have an upper camel case name\b",
            "non_camel_case_types",
        ),
        (
            r"\bshould have an upper case name\b",
            "non_upper_case_globals",
        ),
        (
            r"^unreachable (?:statement|expression|block|call|arm)\b",
            "unreachable_code",
        ),
        (r"^unreachable patterns?\b", "unreachable_patterns"),
        (r"^unnecessary parentheses around ", "unused_parens"),
        (
            r"^unresolved link to `|^`[^`]+` is both a ",
            "rustdoc::broken_intra_doc_links",
        ),
        (r"^this URL is not a hyperlink$", "rustdoc::bare_urls"),
    ]
    .into_iter()
    .map(|(pattern, lint)| (Regex::new(pattern).unwrap(), lint))
    .collect()
});
static QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"`[^`]*`|'[^']*'|"[^"]*""#).unwrap());

#[derive(Clone)]
struct Finding {
    severity: Severity,
    rule: Option<String>,
    message: String,
    file: String,
    line: usize,
    column: Option<usize>,
    stream: Stream,
    first: usize,
    last: usize,
    excerpt: Vec<String>,
}

impl Finding {
    fn new(severity: Severity, rule: Option<String>, message: &str, stream: Stream) -> Self {
        Self {
            severity,
            rule,
            message: short(message.trim(), 300),
            file: String::new(),
            line: 0,
            column: None,
            stream,
            first: 0,
            last: 0,
            excerpt: Vec::new(),
        }
    }

    fn at(mut self, root: &str, file: &str, line: &str, column: Option<regex::Match>) -> Self {
        (self.file, self.line, self.column) = position(root, file, line, column);
        self
    }

    fn lines(mut self, n: usize, excerpt: &str) -> Self {
        self.first = n;
        self.last = n;
        self.excerpt = vec![short(excerpt, 240)];
        self
    }

    fn position(&self) -> String {
        match self.column {
            Some(column) => format!("{}:{column}", self.line),
            None => self.line.to_string(),
        }
    }

    fn place(&self) -> String {
        format!("{}:{}", self.file, self.position())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Error,
    Warning,
}

impl Severity {
    fn parse(text: &str) -> Self {
        let text = text.to_ascii_lowercase();
        if text.contains("error") || matches!(text.as_str(), "e" | "f") {
            Self::Error
        } else {
            Self::Warning
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// Lines of one stream that no shape recognized.
#[derive(Default)]
struct Other {
    count: usize,
    first: usize,
    last: usize,
    head: String,
    tail: VecDeque<String>,
}

impl Other {
    fn note(&mut self, n: usize, line: &str) {
        let line = short(line.trim_end(), 240);
        if self.count == 0 {
            self.first = n;
            self.head = line;
        } else {
            self.tail.push_back(line);
            if self.tail.len() > 2 {
                self.tail.pop_front();
            }
        }
        self.count += 1;
        self.last = n;
    }
}

/// Pending two-line Ansible-lint finding header awaiting its `path:line` location.
struct AnsiblePending {
    rule: String,
    message: String,
    severity: Severity,
    line_no: usize,
    stream: Stream,
    raw: String,
}

pub struct Lint {
    root: String,
    tool: Option<Tool>,
    findings: Vec<Finding>,
    block: Option<Finding>,
    /// Active Credo multi-line issue block (`[W] ↗ msg` + `┃   file:line:col`).
    credo_block: Option<Finding>,
    /// The last one-line finding, which indented detail lines extend.
    open: Option<usize>,
    /// Follow-up `note:` lines and their snippets extend only the preceding finding's
    /// archive span `[stream:A-B]` without adding to its excerpt or finding count.
    note_open: Option<usize>,
    last_located: Option<(usize, Stream)>,
    /// Active file header for ESLint, stylelint and yamllint standard rows.
    file_header: Option<String>,
    /// Active PHPStan file table: `(path, seen_header_separator)`.
    phpstan_file: Option<(String, bool)>,
    /// Active PHPStan general (unlocated) Error table: `0 = none, 1 = saw border, 2 = saw Error, 3 = in body`.
    phpstan_general: u8,
    /// Active SQLFluff `== [path] FAIL` file header.
    sqlfluff_file: Option<String>,
    /// Pending Ansible-lint `rule[sub]: message` header awaiting `path:line`.
    ansible_pending: Option<AnsiblePending>,
    shellcheck_line: Option<(String, String)>,
    /// Whether a `# pkg/path` Go compiler header was seen on this run.
    go_package: bool,
    outcomes: Vec<(String, Vec<String>)>,
    hidden_outcomes: usize,
    /// Packages that failed to compile, each with its failed targets in order. An
    /// empty target stands for a line that named none.
    uncompiled: Vec<(String, Vec<String>)>,
    uncompiled_line: String,
    uncompiled_at: String,
    unlocated_errors: usize,
    other: [Other; 2],
    /// Recognized build/lint status lines among `other`, per stream: count, first and last line.
    status: [(usize, usize, usize); 2],
    /// Whether every recognized status line so far came from Cargo.
    cargo_status_only: bool,
    /// Whether every recognized status line so far came from a linter.
    lint_status_only: bool,
    /// Whether a seen status line requires a completion notice before [`Self::clean`] fires.
    needs_completion: bool,
    /// Counts from `clang-tidy` `N warnings generated.` and `Suppressed M warnings (...)` lines.
    clang_generated: usize,
    clang_suppressed: usize,
    /// Non-blank lines per stream, whatever shape read them.
    lines: [usize; 2],
    /// The last completion status line (such as Cargo's `Finished` or PHPStan's `[OK] No errors`).
    finished: String,
    overflow: bool,
}

impl Lint {
    pub fn new(root: &Path) -> Self {
        let mut root = root.to_string_lossy().into_owned();
        if !root.ends_with('/') {
            root.push('/');
        }
        Self {
            root,
            tool: None,
            findings: Vec::new(),
            block: None,
            credo_block: None,
            open: None,
            note_open: None,
            last_located: None,
            file_header: None,
            phpstan_file: None,
            phpstan_general: 0,
            sqlfluff_file: None,
            ansible_pending: None,
            shellcheck_line: None,
            go_package: false,
            outcomes: Vec::new(),
            hidden_outcomes: 0,
            uncompiled: Vec::new(),
            uncompiled_line: String::new(),
            uncompiled_at: String::new(),
            unlocated_errors: 0,
            other: Default::default(),
            status: [(0, 0, 0); 2],
            cargo_status_only: true,
            lint_status_only: true,
            needs_completion: false,
            clang_generated: 0,
            clang_suppressed: 0,
            lines: [0; 2],
            finished: String::new(),
            overflow: false,
        }
    }

    #[allow(dead_code)]
    pub fn for_tool(root: &Path, tool: Tool) -> Self {
        let mut lint = Self::new(root);
        lint.tool = Some(tool);
        lint
    }

    pub fn observe(&mut self, text: &str, n: usize, stream: Stream) {
        if self.overflow {
            return;
        }
        if self.findings.len() >= MAX_FINDINGS || text.len() > 64 * 1024 {
            self.overflow = true;
            return;
        }
        let line = text.trim_end_matches(['\n', '\r']);
        if !line.trim().is_empty() {
            self.lines[stream as usize] += 1;
        }
        if !self.classify(line, n, stream) && !line.trim().is_empty() {
            self.other[stream as usize].note(n, line);
            if let Some((kind, finished, requires_completion)) = build_status(line) {
                let status = &mut self.status[stream as usize];
                *status = (status.0 + 1, if status.0 == 0 { n } else { status.1 }, n);
                if kind != StatusKind::Cargo {
                    self.cargo_status_only = false;
                }
                if kind != StatusKind::Lint {
                    self.lint_status_only = false;
                }
                if requires_completion {
                    self.needs_completion = true;
                }
                if let Some(c) = CLANG_TIDY_GENERATED.captures(line) {
                    self.clang_generated += c[1].parse::<usize>().unwrap_or(0);
                } else if let Some(c) = CLANG_TIDY_SUPPRESSED.captures(line) {
                    self.clang_suppressed += c[1].parse::<usize>().unwrap_or(0);
                }
                if finished {
                    self.finished = short(line.trim(), 200);
                }
            }
        }
    }

    /// One line for a run that printed only recognized build or lint status lines
    /// (Cargo, CMake, Ninja, Make, Gradle, Maven, dotnet, Swift, Go, PHPStan,
    /// clang-tidy, cppcheck, markdownlint-cli2, SQLFluff, Deno, Ansible-lint or
    /// Credo), with the range of those lines on each stream and any completion
    /// line. `None` when the run printed anything else.
    pub fn clean(&self, label: &str) -> Option<String> {
        // A shape can take a line without a finding, as a warning without a
        // location, so every line counts.
        let only_status = (0..2).all(|i| self.lines[i] == self.status[i].0);
        if self.overflow
            || !only_status
            || self.block.is_some()
            || self.credo_block.is_some()
            || self.ansible_pending.is_some()
            || !self.findings.is_empty()
            || !self.outcomes.is_empty()
            || !self.uncompiled.is_empty()
            || self.hidden_outcomes > 0
            || self.clang_generated != self.clang_suppressed
            || (self.needs_completion && self.finished.is_empty())
            || self.status.iter().all(|status| status.0 == 0)
        {
            return None;
        }
        let kind = if self.cargo_status_only {
            "cargo"
        } else if self.lint_status_only {
            "lint"
        } else {
            "build"
        };
        let mut line = format!("{label}: 0 findings");
        for stream in [Stream::Stdout, Stream::Stderr] {
            let (count, first, last) = self.status[stream as usize];
            if count > 0 {
                let name = stream.name();
                line.push_str(&format!(
                    " · {count} {kind} status line(s) [{name}:{first}-{last}]"
                ));
            }
        }
        if !self.finished.is_empty() {
            line.push_str(&format!(" · {}", self.finished));
        }
        line.push('\n');
        Some(line)
    }

    /// Record a line and return whether it belonged to a recognized shape.
    fn classify(&mut self, line: &str, n: usize, stream: Stream) -> bool {
        // Two-line Ansible-lint block: a pending `rule[sub]: message` header promotes to
        // a finding only when the immediately following line on the same stream is `path:line`.
        if let Some(pending) = self.ansible_pending.take()
            && pending.stream == stream
            && let Some(c) = ANSIBLE_LOC.captures(line)
        {
            let mut finding = Finding::new(
                pending.severity,
                Some(pending.rule),
                &pending.message,
                stream,
            )
            .at(&self.root, &c[1], &c[2], c.get(3))
            .lines(pending.line_no, &pending.raw);
            finding.last = n;
            finding.excerpt.push(short(line.trim(), 240));
            self.push(finding, false);
            return true;
        }
        // Active Credo multi-line issue block (`[W] ↗ msg` + `┃   path:line:col`).
        if let Some(credo) = self.credo_block.as_mut().filter(|b| b.stream == stream) {
            let ends = line.trim().is_empty()
                || CREDO_HEADER.is_match(line)
                || CREDO_ONELINE.is_match(line)
                || TOTAL.is_match(line.trim())
                || build_status(line).is_some()
                || (!credo.file.is_empty() && !line.starts_with('┃') && !line.starts_with('|'));
            if !ends {
                if credo.file.is_empty()
                    && let Some(c) = CREDO_LOC.captures(line)
                {
                    (credo.file, credo.line, credo.column) =
                        position(&self.root, &c[1], &c[2], c.get(3));
                    credo.last = n;
                    if credo.excerpt.len() < EXCERPT_LINES {
                        credo.excerpt.push(short(line.trim(), 240));
                    }
                    return true;
                }
                if credo.file.is_empty() && (line.starts_with('┃') || line.starts_with('|')) {
                    let body = line.trim_start_matches(['┃', '|']).trim();
                    if !body.is_empty() {
                        let combined = format!("{} {body}", credo.message);
                        credo.message = short(&combined, 300);
                        credo.last = n;
                        if credo.excerpt.len() < EXCERPT_LINES {
                            credo.excerpt.push(short(line.trim(), 240));
                        }
                        return true;
                    }
                }
                if !credo.file.is_empty() && (line.starts_with('┃') || line.starts_with('|')) {
                    credo.last = n;
                    if credo.excerpt.len() < EXCERPT_LINES {
                        credo.excerpt.push(short(line.trim(), 240));
                    }
                    return true;
                }
            }
            self.close_credo();
        }
        if let Some(block) = self.block.as_mut().filter(|b| b.stream == stream) {
            let ends = RUST_HEADER.is_match(line)
                || DENO_CHECK_HEADER.is_match(line)
                || CODE_HEADER.is_match(line)
                || OXLINT_HEADER.is_match(line)
                || BIOME_HEADER.is_match(line)
                || MAVEN.is_match(line)
                || KOTLIN.is_match(line)
                || MSBUILD.is_match(line)
                || TOTAL.is_match(line.trim())
                || build_status(line).is_some();
            if line.is_empty() && !ends {
                return true;
            }
            if !ends {
                block.last = n;
                if block.file.is_empty() {
                    if let Some(c) = ARROW.captures(line) {
                        let (file, row, col) = match (c.get(1), c.get(2)) {
                            (Some(f), Some(r)) => (f.as_str(), r.as_str(), c.get(3)),
                            _ => (&c[4], &c[5], c.get(6)),
                        };
                        (block.file, block.line, block.column) =
                            position(&self.root, file, row, col);
                    } else if let Some(c) = AT_LOC.captures(line) {
                        (block.file, block.line, block.column) =
                            position(&self.root, &c[1], &c[2], c.get(3));
                    }
                }
                if block.message.is_empty()
                    && let Some(c) = BIOME_MSG.captures(line)
                {
                    block.severity = match &c[1] {
                        "×" | "✖" => Severity::Error,
                        _ => Severity::Warning,
                    };
                    block.message = short(c[2].trim(), 240);
                }
                if let Some(rule) = attribute(line) {
                    block.rule.get_or_insert(rule);
                }
                if block.excerpt.len() == EXCERPT_LINES && block.file.is_empty() {
                    // Without a location, the cause of a failure usually comes last.
                    block.excerpt.remove(1);
                }
                if block.excerpt.len() < EXCERPT_LINES {
                    block.excerpt.push(short(line, 240));
                }
                return true;
            }
        }
        if let Some(index) = self.note_open.take()
            && self.findings[index].stream == stream
            && (line.starts_with(char::is_whitespace) || SNIPPET.is_match(line))
            && !line.trim().is_empty()
            && !ESLINT_ROW.is_match(line)
            && !STYLELINT_ROW.is_match(line)
            && !YAMLLINT_ROW.is_match(line)
            && !PYRIGHT.is_match(line)
            && !SHELLCHECK_ROW.is_match(line)
            && !OXLINT_HEADER.is_match(line)
            && !MSBUILD.is_match(line)
            && !CREDO_HEADER.is_match(line)
            && !CREDO_ONELINE.is_match(line)
            && !PHPSTAN_HEADER.is_match(line)
            && !PHPSTAN_BORDER.is_match(line)
            && !DART_ANALYZE_START.is_match(line)
            && !is_build_outcome(line.trim())
            && !TOTAL.is_match(line.trim())
            && build_status(line).is_none()
        {
            self.findings[index].last = n;
            self.note_open = Some(index);
            return true;
        }
        if let Some(index) = self.open.take()
            && self.findings[index].stream == stream
            && (line.starts_with(char::is_whitespace) || SNIPPET.is_match(line))
            && !line.trim().is_empty()
            && !ESLINT_ROW.is_match(line)
            && !STYLELINT_ROW.is_match(line)
            && !YAMLLINT_ROW.is_match(line)
            && !PYRIGHT.is_match(line)
            && !SHELLCHECK_ROW.is_match(line)
            && !OXLINT_HEADER.is_match(line)
            && !MSBUILD.is_match(line)
            && !CREDO_HEADER.is_match(line)
            && !CREDO_ONELINE.is_match(line)
            && !PHPSTAN_HEADER.is_match(line)
            && !PHPSTAN_BORDER.is_match(line)
            && !DART_ANALYZE_START.is_match(line)
            && !(self.phpstan_file.is_some() && PHPSTAN_ROW.is_match(line))
            && !is_build_outcome(line.trim())
            && !TOTAL.is_match(line.trim())
            && build_status(line).is_none()
        {
            let finding = &mut self.findings[index];
            finding.last = n;
            if self.phpstan_file.is_some() {
                if let Some(c) = PHPSTAN_ID.captures(line) {
                    finding.rule = Some(c[1].to_owned());
                } else {
                    let trimmed = line.trim();
                    let is_meta = trimmed.starts_with(['💡', '✏', '⏰']);
                    let has_meta = finding
                        .excerpt
                        .iter()
                        .skip(1)
                        .any(|l| l.trim_start().starts_with(['💡', '✏', '⏰']));
                    if !is_meta && !has_meta && finding.rule.is_none() {
                        let combined = format!("{} {trimmed}", finding.message);
                        finding.message = short(&combined, 300);
                    } else if finding.excerpt.len() < EXCERPT_LINES {
                        finding.excerpt.push(short(trimmed, 240));
                    }
                }
            } else {
                if finding.rule.is_none()
                    && let Some(c) = PYRIGHT_RULE.captures(line)
                {
                    finding.rule = Some(c[2].to_owned());
                }
                if finding.excerpt.len() < EXCERPT_LINES {
                    finding.excerpt.push(short(line, 240));
                }
            }
            self.open = Some(index);
            return true;
        }
        self.close();
        if build_status(line).is_some() {
            self.file_header = None;
            self.sqlfluff_file = None;
            self.last_located = None;
            return false;
        }
        if PHPSTAN_BORDER.is_match(line) {
            self.last_located = None;
            if let Some((_, seen_sep)) = &mut self.phpstan_file {
                if *seen_sep {
                    self.phpstan_file = None;
                } else {
                    *seen_sep = true;
                }
                return true;
            }
            match self.phpstan_general {
                0 => {
                    self.phpstan_general = 1;
                }
                2 => {
                    self.phpstan_general = 3;
                    return true;
                }
                3 => {
                    self.phpstan_general = 0;
                    return true;
                }
                _ => {
                    self.phpstan_general = 1;
                }
            }
            return false;
        }
        if let Some(c) = PHPSTAN_HEADER.captures(line) {
            let raw = c[1].trim();
            let file = raw.split(" (in context of ").next().unwrap_or(raw).trim();
            self.phpstan_file = Some((file.to_owned(), false));
            self.phpstan_general = 0;
            self.file_header = None;
            self.last_located = None;
            return true;
        }
        if self.phpstan_general == 1 && PHPSTAN_GENERAL_HEADER.is_match(line) {
            self.phpstan_general = 2;
            self.last_located = None;
            return true;
        }
        if self.phpstan_general == 3 {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                self.unlocated_errors += 1;
                self.outcome(trimmed, &[], stream, n);
                return true;
            }
            self.phpstan_general = 0;
        } else if self.phpstan_general > 0 {
            self.phpstan_general = 0;
        }
        if let Some((file, _)) = &self.phpstan_file
            && let Some(c) = PHPSTAN_ROW.captures(line)
        {
            let row = if &c[1] == "-" { "0" } else { &c[1] };
            let (message, rule) = match TRAILING_BRACKET.captures(c[2].trim()) {
                Some(t) => (t[1].trim().to_owned(), Some(t[2].to_owned())),
                None => (c[2].trim().to_owned(), None),
            };
            let finding = Finding::new(Severity::Error, rule, &message, stream)
                .at(&self.root, file, row, None)
                .lines(n, line.trim());
            self.push(finding, true);
            return true;
        }
        if let Some(c) = SQLFLUFF_HEADER.captures(line) {
            self.file_header = None;
            self.last_located = None;
            return match &c[2] {
                "FAIL" | "WARNING" => {
                    self.sqlfluff_file = Some(c[1].to_owned());
                    true
                }
                _ => {
                    self.sqlfluff_file = None;
                    false
                }
            };
        }
        if let Some(file) = &self.sqlfluff_file
            && let Some(c) = SQLFLUFF_ROW.captures(line)
        {
            let code = &c[3];
            let severity = if matches!(code, "PRS" | "TMP" | "LXR") {
                Severity::Error
            } else {
                Severity::Warning
            };
            let finding = Finding::new(severity, Some(code.to_owned()), &c[4], stream)
                .at(&self.root, file, &c[1], c.get(2))
                .lines(n, line.trim());
            self.push(finding, true);
            return true;
        }
        if let Some(c) = CREDO_ONELINE.captures(line) {
            let finding = Finding::new(Severity::Warning, None, &c[5], stream)
                .at(&self.root, &c[2], &c[3], c.get(4))
                .lines(n, line.trim());
            self.push(finding, false);
            return true;
        }
        if let Some(c) = CREDO_HEADER.captures(line) {
            self.last_located = None;
            self.credo_block =
                Some(Finding::new(Severity::Warning, None, &c[2], stream).lines(n, line.trim()));
            return true;
        }
        if GO_PACKAGE.is_match(line) {
            self.go_package = true;
            self.file_header = None;
            self.last_located = None;
            return true;
        }
        if let Some(c) = RUST_HEADER.captures(line) {
            self.last_located = None;
            let rule = c.get(2).map(|m| m.as_str().to_owned());
            self.block =
                Some(Finding::new(Severity::parse(&c[1]), rule, &c[3], stream).lines(n, line));
            return true;
        }
        if let Some(c) = DENO_CHECK_HEADER.captures(line) {
            self.last_located = None;
            self.block = Some(
                Finding::new(Severity::parse(&c[2]), Some(c[1].to_owned()), &c[3], stream)
                    .lines(n, line),
            );
            return true;
        }
        if let Some(c) = CODE_HEADER.captures(line) {
            self.last_located = None;
            let rule = Some(c[1].to_owned());
            self.block = Some(Finding::new(Severity::Warning, rule, &c[2], stream).lines(n, line));
            return true;
        }
        if let Some(c) = OXLINT_HEADER.captures(line) {
            self.last_located = None;
            let severity = match &c[1] {
                "×" | "✖" => Severity::Error,
                _ => Severity::Warning,
            };
            self.block = Some(
                Finding::new(severity, Some(c[2].to_owned()), &c[3], stream).lines(n, line.trim()),
            );
            return true;
        }
        if let Some(c) = BIOME_HEADER.captures(line) {
            self.last_located = None;
            let finding = Finding::new(Severity::Error, Some(c[4].to_owned()), "", stream)
                .at(&self.root, &c[1], &c[2], c.get(3))
                .lines(n, line);
            self.block = Some(finding);
            return true;
        }
        if let Some(c) = MAVEN.captures(line) {
            let (rule, message) = match JAVAC_LINT.captures(c[5].trim()) {
                Some(j) => (Some(j[1].to_owned()), j.get(2).unwrap().as_str()),
                None => (None, c[5].trim()),
            };
            let finding = Finding::new(Severity::parse(&c[1]), rule, message, stream)
                .at(&self.root, &c[2], &c[3], c.get(4))
                .lines(n, line);
            self.push(finding, true);
            return true;
        }
        if let Some(c) = KOTLIN.captures(line) {
            let finding = Finding::new(Severity::parse(&c[1]), None, &c[5], stream)
                .at(&self.root, &c[2], &c[3], c.get(4))
                .lines(n, line);
            self.push(finding, true);
            return true;
        }
        if let Some(c) = TSC.captures(line) {
            let finding =
                Finding::new(Severity::parse(&c[4]), Some(c[5].to_owned()), &c[6], stream)
                    .at(&self.root, &c[1], &c[2], c.get(3))
                    .lines(n, line);
            self.push(finding, true);
            return true;
        }
        if let Some(c) = MSBUILD.captures(line) {
            let finding =
                Finding::new(Severity::parse(&c[4]), Some(c[5].to_owned()), &c[6], stream)
                    .at(&self.root, &c[1], &c[2], c.get(3))
                    .lines(n, line.trim());
            self.push(finding, false);
            return true;
        }
        if let Some(c) = CPPCHECK_BRACKET.captures(line) {
            let (message, rule) = match TRAILING_BRACKET.captures(c[5].trim()) {
                Some(t) => (t[1].trim().to_owned(), Some(t[2].to_owned())),
                None => (c[5].trim().to_owned(), None),
            };
            let finding = Finding::new(Severity::parse(&c[4]), rule, &message, stream)
                .at(&self.root, &c[1], &c[2], c.get(3))
                .lines(n, line);
            self.push(finding, true);
            return true;
        }
        if let Some(c) = MARKDOWNLINT.captures(line) {
            let severity = c
                .get(4)
                .map_or(Severity::Error, |m| Severity::parse(m.as_str()));
            let finding = Finding::new(severity, Some(c[5].to_owned()), &c[6], stream)
                .at(&self.root, &c[1], &c[2], c.get(3))
                .lines(n, line);
            self.push(finding, false);
            return true;
        }
        if let Some(c) = STYLELINT_COMPACT.captures(line) {
            let rule = c.get(6).map(|m| m.as_str().to_owned());
            let finding = Finding::new(Severity::parse(&c[4]), rule, &c[5], stream)
                .at(&self.root, &c[1], &c[2], c.get(3))
                .lines(n, line);
            self.push(finding, false);
            return true;
        }
        if let Some(finding) = parse_dart_analyze(line, &self.root, n, stream) {
            self.push(finding, false);
            return true;
        }
        if let Some(c) = LOCATED.captures(line) {
            if let Some(mut finding) = located(&c[1], &c[4], stream, self.tool) {
                if self.go_package
                    && c[1].ends_with(".go")
                    && finding.rule.is_none()
                    && !SEVERITY.is_match(c[4].trim())
                {
                    finding.severity = Severity::Error;
                }
                let finding = finding
                    .at(&self.root, &c[1], &c[2], c.get(3))
                    .lines(n, line);
                self.push(finding, true);
                self.last_located = Some((self.findings.len() - 1, stream));
            } else if let Some((idx, prev_stream)) = self.last_located
                && prev_stream == stream
            {
                // Preserve follow-up `note:` lines via the preceding finding's archive
                // span `[stream:A-B]` without counting them as separate findings.
                self.findings[idx].last = n;
                self.note_open = Some(idx);
            }
            return true;
        }
        if let Some(c) = PHPSTAN_RAW.captures(line) {
            let rest = c[3].trim();
            let looks_like_col = rest.chars().all(|ch| ch.is_ascii_digit())
                || rest.split_once(':').is_some_and(|(head, _)| {
                    !head.is_empty() && head.chars().all(|ch| ch.is_ascii_digit())
                });
            if !looks_like_col {
                let (severity, text) = if let Some(s) = SEVERITY.captures(rest) {
                    (Severity::parse(&s[1]), s.get(3).unwrap().as_str())
                } else {
                    (Severity::Error, rest)
                };
                let (message, rule) = match TRAILING_BRACKET.captures(text) {
                    Some(t) => (t.get(1).unwrap().as_str().trim(), Some(t[2].to_owned())),
                    None => (text, None),
                };
                let finding = Finding::new(severity, rule, message, stream)
                    .at(&self.root, &c[1], &c[2], None)
                    .lines(n, line);
                self.push(finding, false);
                return true;
            }
        }
        if let Some(c) = PYRIGHT.captures(line) {
            let (message, rule) = match PYRIGHT_RULE.captures(&c[5]) {
                Some(r) => (r[1].to_owned(), Some(r[2].to_owned())),
                None => (c[5].to_owned(), None),
            };
            let finding = Finding::new(Severity::parse(&c[4]), rule, &message, stream)
                .at(&self.root, &c[1], &c[2], c.get(3))
                .lines(n, line.trim());
            self.push(finding, true);
            return true;
        }
        if let Some(c) = HADOLINT.captures(line) {
            let finding =
                Finding::new(Severity::parse(&c[5]), Some(c[4].to_owned()), &c[6], stream)
                    .at(&self.root, &c[1], &c[2], c.get(3))
                    .lines(n, line);
            self.push(finding, false);
            return true;
        }
        if let Some(c) = SHELLCHECK_FILE.captures(line) {
            self.last_located = None;
            self.shellcheck_line = Some((c[1].to_owned(), c[2].to_owned()));
            return true;
        }
        if let Some(c) = SHELLCHECK_ROW.captures(line)
            && let Some((file, row)) = &self.shellcheck_line
        {
            let finding =
                Finding::new(Severity::parse(&c[2]), Some(c[1].to_owned()), &c[3], stream)
                    .at(&self.root, file, row, None)
                    .lines(n, line.trim());
            self.push(finding, false);
            return true;
        }
        if let Some(file) = &self.file_header {
            if let Some(c) = YAMLLINT_ROW.captures(line) {
                let finding =
                    Finding::new(Severity::parse(&c[3]), Some(c[5].to_owned()), &c[4], stream)
                        .at(&self.root, file, &c[1], c.get(2))
                        .lines(n, line.trim());
                self.push(finding, false);
                return true;
            }
            if let Some(c) = STYLELINT_ROW.captures(line) {
                let severity = match &c[3] {
                    "✖" | "×" => Severity::Error,
                    _ => Severity::Warning,
                };
                let rule = c.get(5).map(|m| m.as_str().to_owned());
                let finding = Finding::new(severity, rule, &c[4], stream)
                    .at(&self.root, file, &c[1], c.get(2))
                    .lines(n, line.trim());
                self.push(finding, false);
                return true;
            }
            if let Some(c) = ESLINT_ROW.captures(line) {
                let rule = c.get(5).map(|m| m.as_str().to_owned());
                let finding = Finding::new(Severity::parse(&c[3]), rule, &c[4], stream)
                    .at(&self.root, file, &c[1], c.get(2))
                    .lines(n, line.trim());
                self.push(finding, false);
                return true;
            }
        }
        // ESLint, stylelint and yamllint rows belong to the file named since the last blank or other line.
        if line.trim().is_empty() {
            self.file_header = None;
            self.sqlfluff_file = None;
            self.last_located = None;
            return false;
        }
        self.last_located = None;
        if !line.starts_with(char::is_whitespace) {
            let trimmed_end = line.trim_end();
            if ESLINT_FILE.is_match(trimmed_end) {
                self.file_header = Some(trimmed_end.to_owned());
                return true;
            }
            self.file_header = None;
        }
        let text = line.trim();
        let lower = text.to_ascii_lowercase();
        if UNLOCATED_ERROR.is_match(text) {
            self.unlocated_errors += 1;
            self.outcome(text, &[], stream, n);
            return true;
        }
        if lower.starts_with("error")
            || lower.starts_with("fatal")
            || text.starts_with('✖')
            || TOTAL.is_match(text)
            || is_build_outcome(text)
        {
            self.outcome(text, &[], stream, n);
            return true;
        }
        // Hold a candidate Ansible-lint `rule[sub]: message` line until the next line
        // confirms it with `path:line`; return `false` so unrecognized lines stay in `other`.
        if let Some(c) = ANSIBLE_BLOCK_HEADER.captures(line) {
            let rule = &c[1];
            let is_severity_word = matches!(
                rule,
                "error"
                    | "warning"
                    | "note"
                    | "info"
                    | "information"
                    | "hint"
                    | "help"
                    | "fatal"
                    | "style"
                    | "performance"
                    | "portability"
                    | "refactor"
                    | "convention"
            );
            if !is_severity_word {
                let raw_msg = c[2].trim();
                let (severity, message) = match raw_msg.strip_suffix(" (warning)") {
                    Some(msg) => (Severity::Warning, msg.trim()),
                    None => (Severity::Error, raw_msg),
                };
                self.ansible_pending = Some(AnsiblePending {
                    rule: rule.to_owned(),
                    message: message.to_owned(),
                    severity,
                    line_no: n,
                    stream,
                    raw: line.to_owned(),
                });
            }
        }
        false
    }

    fn push(&mut self, finding: Finding, extends: bool) {
        self.findings.push(finding);
        self.open = extends.then(|| self.findings.len() - 1);
    }

    /// Keep a general outcome line, such as an error without a source location or a
    /// tool's total. `could not compile` lines collapse into one that names each
    /// failed package and target.
    fn outcome(&mut self, text: &str, detail: &[String], stream: Stream, n: usize) {
        let head = short(text, 240);
        let at = format!(" [{}:{n}]", stream.name());
        if let Some(c) = UNCOMPILED.captures(&head) {
            if self.uncompiled.is_empty() {
                self.uncompiled_line = format!("{head}{at}");
                self.uncompiled_at = at;
            }
            let target = c.get(2).map_or("", |m| m.as_str());
            match self.uncompiled.iter_mut().find(|(p, _)| p == &c[1]) {
                Some((_, targets)) if !targets.iter().any(|t| t == target) => {
                    targets.push(target.to_owned());
                }
                Some(_) => {}
                None => self
                    .uncompiled
                    .push((c[1].to_owned(), vec![target.to_owned()])),
            }
            return;
        }
        if self
            .outcomes
            .iter()
            .any(|(o, _)| o.strip_prefix(&head).is_some_and(|r| r.starts_with(" [")))
        {
            return;
        }
        if self.outcomes.len() >= SHOWN_OUTCOMES {
            self.hidden_outcomes += 1;
            return;
        }
        self.outcomes.push((format!("{head}{at}"), detail.to_vec()));
    }

    fn close_credo(&mut self) {
        let Some(block) = self.credo_block.take() else {
            return;
        };
        if !block.file.is_empty() {
            self.findings.push(block);
        }
    }

    fn close(&mut self) {
        self.close_credo();
        let Some(block) = self.block.take() else {
            return;
        };
        if !block.file.is_empty() {
            self.findings.push(block);
        } else if block.severity == Severity::Error {
            // An error without a source location, such as a failed build script or link
            // step, stays visible as a general outcome with its detail lines.
            if !UNCOMPILED.is_match(&block.excerpt[0])
                && !block.excerpt[0].starts_with("error: aborting due to")
                && block.excerpt[0] != "error: Type checking failed."
            {
                self.unlocated_errors += 1;
            }
            self.outcome(
                &block.excerpt[0],
                &block.excerpt[1..],
                block.stream,
                block.first,
            );
        }
    }

    /// Return the grouped report, fitted to `max_bytes`, and the comparison record,
    /// or `None` when no finding was recognized or the stream was too large to group.
    pub fn finish(
        mut self,
        label: &str,
        max_bytes: usize,
        previous: Option<(&str, &Summary)>,
    ) -> Option<(String, Summary)> {
        self.close();
        if self.overflow || self.findings.is_empty() {
            return None;
        }
        // Clippy and rustc name a lint only on its first occurrence. Learn the rule
        // for a normalized message and apply it to the remaining occurrences.
        let mut learned = BTreeMap::new();
        for finding in &self.findings {
            if let Some(rule) = &finding.rule {
                learned
                    .entry(normalize(&finding.message))
                    .or_insert_with(|| rule.clone());
            }
        }
        let mut streams = [false; 2];
        let mut groups: BTreeMap<(Severity, String), Vec<Finding>> = BTreeMap::new();
        for finding in self.findings {
            streams[finding.stream as usize] = true;
            let normalized = normalize(&finding.message);
            let key = finding
                .rule
                .clone()
                .or_else(|| learned.get(&normalized).cloned())
                .or_else(|| rustc_lint(&finding).map(str::to_owned))
                .unwrap_or(normalized);
            groups
                .entry((finding.severity, key))
                .or_default()
                .push(finding);
        }
        let errors: usize = groups
            .iter()
            .filter(|((s, _), _)| *s == Severity::Error)
            .map(|(_, f)| f.len())
            .sum();
        let total: usize = groups.values().map(Vec::len).sum();
        let files = groups
            .values()
            .flatten()
            .map(|f| f.file.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let mut ordered: Vec<_> = groups.into_iter().collect();
        ordered.sort_by(|((a, ak), af), ((b, bk), bf)| {
            a.cmp(b).then(bf.len().cmp(&af.len())).then(ak.cmp(bk))
        });

        let mut summary = Summary {
            kind: "lint".into(),
            ..Summary::default()
        };
        let mut header = format!(
            "{label}: {total} finding(s), {errors} error(s), {} warning(s) · {} rule(s) · {} file(s)",
            total - errors,
            ordered.len(),
            files
        );
        if self.unlocated_errors > 0 {
            header.push_str(&format!(
                " · {} error(s) without a location",
                self.unlocated_errors
            ));
        }
        header.push('\n');
        let mut sections = Vec::new();
        for ((severity, rule), findings) in &ordered {
            let first = &findings[0];
            let mut item = Item::new(rule, &first.message, first.first);
            item.digest = findings.len().to_string();
            item.last = first.last;
            summary.items.push(item);
            // The comparison record for each item: severity, rule, count and digests of
            // the reported messages per file and of the exact locations.
            let (messages, places) = fingerprints(findings);
            summary.outcomes.push(format!(
                "{} {rule} ×{} {messages}/{places}",
                severity.label(),
                findings.len()
            ));
            // Errors show the detail lines of their first occurrence that has any,
            // introduced by its original line when that is not the group's first.
            let mut detail = String::new();
            if *severity == Severity::Error
                && let Some(example) = findings.iter().find(|f| f.excerpt.len() > 1)
            {
                let skip = usize::from(std::ptr::eq(example, first));
                for line in example.excerpt.iter().skip(skip) {
                    detail.push_str(&format!("    {line}\n"));
                }
            }
            sections.push((
                format!(
                    "{} {rule} ×{}  {}\n",
                    severity.label(),
                    findings.len(),
                    first.message
                ),
                detail,
                format!("  {}\n", locations(findings)),
                findings.len(),
            ));
        }

        let mut tail = String::new();
        if let Some((id, old)) = previous.filter(|(_, s)| s.kind == "lint") {
            tail.push_str(&compare(id, old, &summary));
        }
        // Without a located error, an error without a location, such as a failed build
        // script, explains the failure, so the outcomes come right after the header.
        let mut lead = String::new();
        let outcomes = if errors == 0 && self.unlocated_errors > 0 {
            &mut lead
        } else {
            &mut tail
        };
        let mut detail_room = max_bytes / 2;
        for (head, detail) in &self.outcomes {
            outcomes.push_str(head);
            outcomes.push('\n');
            for line in detail {
                if line.len() + 5 > detail_room {
                    break;
                }
                detail_room -= line.len() + 5;
                outcomes.push_str(&format!("    {line}\n"));
            }
        }
        // One failed target keeps its original line. Several name each package with
        // its failed targets, such as `rt` (lib, lib test).
        match self.uncompiled.as_slice() {
            [] => {}
            [(_, targets)] if targets.len() == 1 => {
                tail.push_str(&self.uncompiled_line);
                tail.push('\n');
            }
            packages => {
                let names: Vec<_> = packages
                    .iter()
                    .take(SHOWN_PACKAGES)
                    .map(|(package, targets)| {
                        let named: Vec<_> = targets.iter().filter(|t| !t.is_empty()).collect();
                        let mut list = named
                            .iter()
                            .take(SHOWN_TARGETS)
                            .map(|t| t.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        if named.len() > SHOWN_TARGETS {
                            list.push_str(&format!(" +{} more", named.len() - SHOWN_TARGETS));
                        }
                        if list.is_empty() {
                            format!("`{package}`")
                        } else {
                            format!("`{package}` ({list})")
                        }
                    })
                    .collect();
                let more = match packages.len().saturating_sub(SHOWN_PACKAGES) {
                    0 => String::new(),
                    more => format!(" +{more} more"),
                };
                let count = match packages.len() {
                    1 => String::new(),
                    count => format!("{count} packages: "),
                };
                tail.push_str(&format!(
                    "error: could not compile {count}{}{more}{}\n",
                    names.join(", "),
                    self.uncompiled_at
                ));
            }
        }
        if self.hidden_outcomes > 0 {
            tail.push_str(&format!(
                "+{} more outcome line(s) in full log\n",
                self.hidden_outcomes
            ));
        }
        for stream in [Stream::Stdout, Stream::Stderr] {
            let other = &self.other[stream as usize];
            if streams[stream as usize] || other.count == 0 {
                continue;
            }
            let name = stream.name();
            tail.push_str(&format!(
                "{name}: {} other line(s) [{name}:{}-{}]\n  {}\n",
                other.count, other.first, other.last, other.head
            ));
            if other.count > 1 + other.tail.len() {
                tail.push_str("  …\n");
            }
            for line in &other.tail {
                tail.push_str(&format!("  {line}\n"));
            }
        }

        // Every group's rule line and locations come before any excerpt, so a long
        // report drops detail lines first and then whole groups with a count.
        let room = max_bytes.saturating_sub(header.len() + lead.len() + tail.len());
        let compact: usize = sections.iter().map(|(h, _, p, _)| h.len() + p.len()).sum();
        let (shown, mut spare) = if compact <= room {
            (sections.len(), room - compact)
        } else {
            let room = room.saturating_sub(64);
            let mut used = 0;
            let shown = sections
                .iter()
                .take_while(|(h, _, p, _)| {
                    used += h.len() + p.len();
                    used <= room
                })
                .count();
            let used: usize = sections[..shown]
                .iter()
                .map(|(h, _, p, _)| h.len() + p.len())
                .sum();
            (shown, room - used)
        };
        let mut report = header + &lead;
        for (head, detail, places, _) in &sections[..shown] {
            report.push_str(head);
            if detail.len() <= spare {
                report.push_str(detail);
                spare -= detail.len();
            }
            report.push_str(places);
        }
        if shown < sections.len() {
            let hidden = &sections[shown..];
            report.push_str(&format!(
                "+{} more rule(s) with {} finding(s) in full log\n",
                hidden.len(),
                hidden.iter().map(|s| s.3).sum::<usize>()
            ));
        }
        report.push_str(&tail);
        Some((report, summary))
    }
}

/// Counts and digests per severity and rule. Records written before severities were
/// stored have no keys, so both sides then compare by rule alone.
type Tally = BTreeMap<(String, String), (usize, String, String)>;

fn tally(summary: &Summary, keyed: bool) -> Tally {
    let mut tally = Tally::new();
    for (i, item) in summary.items.iter().enumerate() {
        let record = summary
            .outcomes
            .get(i)
            .filter(|_| keyed)
            .map_or("", String::as_str);
        let severity = record.split(' ').next().unwrap_or("");
        let (messages, places) = record
            .rsplit_once(' ')
            .and_then(|(_, digests)| digests.split_once('/'))
            .unwrap_or(("", ""));
        let entry = tally
            .entry((severity.to_owned(), item.name.clone()))
            .or_insert_with(|| (0, messages.to_owned(), places.to_owned()));
        entry.0 += item.digest.parse::<usize>().unwrap_or(0);
    }
    tally
}

fn compare(id: &str, old: &Summary, current: &Summary) -> String {
    let keyed = !old.items.is_empty() && old.outcomes.len() == old.items.len();
    let (before, now) = (tally(old, keyed), tally(current, keyed));
    let name = |(severity, rule): &(String, String)| {
        if severity.is_empty() {
            rule.clone()
        } else {
            format!("{severity} {rule}")
        }
    };
    let mut changes = Vec::new();
    let mut moved = false;
    for (key, (count, messages, places)) in &now {
        match before.get(key) {
            None => changes.push(format!("+ {} ×{count} (new)", name(key))),
            Some((old, ..)) if old != count => {
                changes.push(format!("~ {} ×{old} → ×{count}", name(key)))
            }
            Some((_, old, _)) if old != messages => {
                changes.push(format!("~ {} ×{count} with different findings", name(key)))
            }
            Some((_, _, old)) => moved |= old != places,
        }
    }
    for (key, (count, ..)) in &before {
        if !now.contains_key(key) {
            changes.push(format!("- {} ×{count} (no longer reported)", name(key)));
        }
    }
    if changes.is_empty() {
        return if !keyed {
            format!("Same rule counts as {id}.\n")
        } else if moved {
            format!("Same findings as {id}, some on other lines.\n")
        } else {
            format!("Same findings as {id}.\n")
        };
    }
    let mut out = format!("Compared with {id}:\n");
    let more = changes.len().saturating_sub(SHOWN_CHANGES);
    for change in changes.into_iter().take(SHOWN_CHANGES) {
        out.push_str(&change);
        out.push('\n');
    }
    if more > 0 {
        out.push_str(&format!("+{more} more change(s)\n"));
    }
    out
}

/// Short digests of the messages reported per file, which line shifts leave unchanged,
/// and of the exact locations.
fn fingerprints(findings: &[Finding]) -> (String, String) {
    let digest = |parts: BTreeSet<String>| {
        let mut hash = Sha256::new();
        for part in parts {
            hash.update(part.as_bytes());
            hash.update(b"\n");
        }
        format!("{:x}", hash.finalize())[..12].to_owned()
    };
    let messages = findings
        .iter()
        .map(|f| format!("{}\t{}", f.file, f.message))
        .collect();
    let places = findings.iter().map(Finding::place).collect();
    (digest(messages), digest(places))
}

fn attribute(line: &str) -> Option<String> {
    if SNIPPET.is_match(line) {
        return None;
    }
    let c = ATTRIBUTE.captures(line)?;
    if let Some(name) = c.get(1) {
        return Some(name.as_str().to_owned());
    }
    if let Some(name) = c.get(2) {
        return Some(name.as_str().replace('-', "_"));
    }
    Some(format!("clippy::{}", &c[3]))
}

fn located(file: &str, message: &str, stream: Stream, tool: Option<Tool>) -> Option<Finding> {
    let default_severity = match tool {
        Some(Tool::Phpstan) => Severity::Error,
        _ => Severity::Warning,
    };
    let (mut severity, mut text) = (default_severity, message.trim());
    let mut explicit = None;
    let mut rule = None;
    if let Some(c) = SEVERITY.captures(text) {
        let level = c.get(1).unwrap().as_str().to_ascii_lowercase();
        severity = Severity::parse(&level);
        rule = c.get(2).map(|m| m.as_str().to_owned());
        text = c.get(3).unwrap().as_str();
        explicit = Some(level);
    } else if let Some(c) = BRACKET_SEVERITY.captures(text) {
        let level = c.get(1).unwrap().as_str().to_ascii_lowercase();
        severity = Severity::parse(&level);
        text = c.get(2).unwrap().as_str();
        explicit = Some(level);
    }
    if rule.is_some() {
        // rustc's short format names the error code in the level: `error[E0308]:`.
    } else if let Some(c) = RUBOCOP.captures(text) {
        severity = Severity::parse(&c[1]);
        rule = Some(c[2].to_owned());
        text = c.get(3).unwrap().as_str();
    } else if let Some(c) = CREDO_FLYCHECK.captures(text) {
        severity = Severity::Warning;
        text = c.get(2).unwrap().as_str();
    } else if let Some(c) = LEADING_CODE.captures(text) {
        // pylint's `E0602:` and `F0001:` are errors. flake8's `E302` and Ruff's `F401`
        // have no colon and stay warnings.
        let code = &c[1];
        if !c[2].is_empty() && code.len() == 5 && (code.starts_with('E') || code.starts_with('F')) {
            severity = Severity::Error;
        }
        rule = Some(code.to_owned());
        text = c.get(3).unwrap().as_str();
    } else if explicit.is_none() {
        let is_yaml = file.ends_with(".yml") || file.ends_with(".yaml");
        let has_trailing_rule = TRAILING_BRACKET.is_match(text) || TRAILING_PAREN.is_match(text);
        let ansible = ANSIBLE_SUB_RULE.captures(text).or_else(|| {
            (!has_trailing_rule)
                .then(|| {
                    ANSIBLE_KEBAB_RULE.captures(text).or_else(|| {
                        (is_yaml || tool == Some(Tool::AnsibleLint))
                            .then(|| ANSIBLE_YAML_RULE.captures(text))
                            .flatten()
                    })
                })
                .flatten()
        });
        if let Some(c) = ansible {
            rule = Some(c[1].to_owned());
            let rest = c.get(2).unwrap().as_str().trim();
            if let Some(msg) = rest.strip_suffix(" (warning)") {
                severity = Severity::Warning;
                text = msg.trim();
            } else {
                severity = Severity::Error;
                text = rest;
            }
        }
    }
    // rustc's short format appends its help and notes to the message.
    if let Some(end) = text.find(": help: ").or_else(|| text.find(": note: "))
        && end > 0
    {
        text = &text[..end];
    }
    // It also appends the primary label, which can repeat the message:
    // `unreachable statement: unreachable statement`.
    if let Some(head) = text.get(..text.len().saturating_sub(2) / 2)
        && !head.is_empty()
        && text[head.len()..].strip_prefix(": ") == Some(head)
    {
        text = head;
    }
    if rule.is_none() {
        if let Some(c) = TRAILING_BRACKET.captures(text) {
            let raw_rule = &c[2];
            let cleaned = raw_rule
                .strip_suffix(",-warnings-as-errors")
                .unwrap_or(raw_rule);
            rule = Some(cleaned.to_owned());
            text = c.get(1).unwrap().as_str();
        } else if let Some(c) = TRAILING_PAREN.captures(text) {
            rule = Some(c[2].to_owned());
            text = c.get(1).unwrap().as_str();
        }
    }
    // mypy, clang-tidy, cppcheck and compilers print follow-up notes at the same
    // location. Count a note only when it names a rule, as ShellCheck notes do.
    if explicit.as_deref() == Some("note") && rule.is_none() {
        return None;
    }
    Some(Finding::new(severity, rule, text, stream))
}

/// Parse Dart and Flutter analyzer lines in both modern bullet (`•`) and
/// classic/Windows dash (`-`) formats, supporting both `message • loc • rule`
/// and `loc - message - rule` field orders.
fn parse_dart_analyze(line: &str, root: &str, n: usize, stream: Stream) -> Option<Finding> {
    let c = DART_ANALYZE_START.captures(line)?;
    let sep = if &c[2] == "•" { " • " } else { " - " };
    let rest = c[3].trim();
    let (mid, raw_rule) = rest.rsplit_once(sep)?;
    let rule = raw_rule.trim();
    if !DART_ANALYZE_RULE.is_match(rule) {
        return None;
    }
    let (loc, message) = if let Some((left, right)) = mid.split_once(sep)
        && let Some(loc) = DART_ANALYZE_LOC.captures(left.trim())
    {
        (loc, right.trim())
    } else if let Some((left, right)) = mid.rsplit_once(sep)
        && let Some(loc) = DART_ANALYZE_LOC.captures(right.trim())
    {
        (loc, left.trim())
    } else {
        return None;
    };
    if message.is_empty() {
        return None;
    }
    Some(
        Finding::new(
            Severity::parse(&c[1]),
            Some(rule.to_owned()),
            message,
            stream,
        )
        .at(root, &loc[1], &loc[2], loc.get(3))
        .lines(n, line.trim()),
    )
}

/// The rustc lint a Rust finding's wording implies. Other languages' compilers, such
/// as javac's `unreachable statement`, keep their message.
fn rustc_lint(finding: &Finding) -> Option<&'static str> {
    if !finding.file.ends_with(".rs") {
        return None;
    }
    RUSTC_LINTS
        .iter()
        .find(|(shape, _)| shape.is_match(&finding.message))
        .map(|(_, lint)| *lint)
}

fn position(
    root: &str,
    file: &str,
    line: &str,
    column: Option<regex::Match>,
) -> (String, usize, Option<usize>) {
    let file = file.trim();
    let file = match file.strip_prefix("file://") {
        Some(rest)
            if rest.len() >= 3
                && rest.as_bytes()[0] == b'/'
                && rest.as_bytes()[1].is_ascii_alphabetic()
                && rest.as_bytes()[2] == b':' =>
        {
            &rest[1..]
        }
        Some(rest) => rest,
        None => file,
    };
    let file = file.strip_prefix(root).unwrap_or(file);
    let file = file.strip_prefix("./").unwrap_or(file).to_owned();
    let column = column.and_then(|c| c.as_str().parse().ok());
    (file, line.parse().unwrap_or(0), column)
}

fn normalize(message: &str) -> String {
    QUOTED.replace_all(message, "…").into_owned()
}

fn locations(findings: &[Finding]) -> String {
    // Order by file and numeric position and drop repeats so same-file runs collapse.
    let mut places: Vec<&Finding> = findings.iter().collect();
    places.sort_by(|a, b| (&a.file, a.line, a.column).cmp(&(&b.file, b.line, b.column)));
    places.dedup_by(|a, b| (&a.file, a.line, a.column) == (&b.file, b.line, b.column));
    let distinct = places.len();
    let mut out = String::new();
    let mut previous_file = None;
    for finding in places.into_iter().take(SHOWN_LOCATIONS) {
        if !out.is_empty() {
            out.push_str(", ");
        }
        if previous_file == Some(finding.file.as_str()) {
            out.push_str(&finding.position());
        } else {
            out.push_str(&finding.place());
        }
        previous_file = Some(finding.file.as_str());
    }
    if distinct > SHOWN_LOCATIONS {
        out.push_str(&format!(" +{} more", distinct - SHOWN_LOCATIONS));
    }
    let first = &findings[0];
    out.push_str(&format!(
        " [{}:{}-{}]",
        first.stream.name(),
        first.first,
        first.last
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn biome_oxlint_and_ty_check_group_findings_by_rule() {
        let biome_out = "\
src/app.ts:12:5 lint/style/useConst ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  × This let declares a variable that is only assigned once.

src/util.ts:4:3 lint/style/useConst ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  × This let declares a variable that is only assigned once.

src/api.ts:9:1 lint/suspicious/noExplicitAny ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  ! Unexpected any. Specify a different type.

Checked 12 files in 8ms.
Found 2 errors.
";
        let mut biome = Lint::new(Path::new("/repo"));
        for (i, line) in biome_out.split_inclusive('\n').enumerate() {
            biome.observe(line, i + 1, Stream::Stderr);
        }
        let (report, _) = biome.finish("biome check", 4096, None).unwrap();
        assert!(report.contains("lint/style/useConst ×2"), "{report}");
        assert!(
            report.contains("lint/suspicious/noExplicitAny ×1"),
            "{report}"
        );
        assert!(report.contains("src/app.ts:12:5"), "{report}");

        let oxlint_out = "\
  × eslint(no-unused-vars): Variable 'x' is declared but never used.
   ╭─[src/app.ts:10:7]
  × eslint(no-unused-vars): Variable 'y' is declared but never used.
   ╭─[src/app.ts:14:7]
  ! eslint(no-console): Unexpected console statement.
   ╭─[src/log.ts:3:1]
Found 1 warning and 2 errors.
";
        let mut oxlint = Lint::new(Path::new("/repo"));
        for (i, line) in oxlint_out.split_inclusive('\n').enumerate() {
            oxlint.observe(line, i + 1, Stream::Stdout);
        }
        let (report, _) = oxlint.finish("oxlint", 4096, None).unwrap();
        assert!(report.contains("eslint(no-unused-vars) ×2"), "{report}");
        assert!(report.contains("eslint(no-console) ×1"), "{report}");
        assert!(report.contains("src/app.ts:10:7"), "{report}");

        let ty_out = "\
error[invalid-assignment]: Object of type `str` is not assignable to `int`
 --> src/models.py:22:9
  |
22 | x: int = \"bad\"
  |          ^^^^^
Found 1 diagnostic
";
        let mut ty = Lint::new(Path::new("/repo"));
        for (i, line) in ty_out.split_inclusive('\n').enumerate() {
            ty.observe(line, i + 1, Stream::Stderr);
        }
        let (report, _) = ty.finish("ty check", 4096, None).unwrap();
        assert!(report.contains("invalid-assignment ×1"), "{report}");
        assert!(report.contains("src/models.py:22:9"), "{report}");
    }

    fn feed(out: &str, err: &str) -> Lint {
        let mut lint = Lint::new(Path::new("/repo"));
        for (i, line) in out.split_inclusive('\n').enumerate() {
            lint.observe(line, i + 1, Stream::Stdout);
        }
        for (i, line) in err.split_inclusive('\n').enumerate() {
            lint.observe(line, i + 1, Stream::Stderr);
        }
        lint
    }

    #[test]
    fn maven_kotlin_msbuild_gcc_and_go_build_diagnostics_group_cleanly() {
        // Maven errors (with indented elaboration), deprecation warnings, and outcomes.
        let mvn_out = "\
[INFO] --- compiler:3.11.0:compile (default-compile) @ demo ---
[INFO] Compiling 4 source files with javac
[ERROR] /repo/src/main/java/App.java:[14,23] cannot find symbol
  symbol:   variable foo
  location: class App
[ERROR] /repo/src/main/java/App.java:[28,11] cannot find symbol
  symbol:   variable bar
  location: class App
[WARNING] /repo/src/main/java/Util.java:[8,12] [deprecation] oldCall() in Legacy has been deprecated
[WARNING] /repo/src/main/java/Util.java:[19,5] [deprecation] otherCall() in Legacy has been deprecated
[INFO] 2 errors
[ERROR] Failed to execute goal org.apache.maven.plugins:maven-compiler-plugin:3.11.0:compile (default-compile) on project demo: Compilation failure
[ERROR] -> [Help 1]
[ERROR] To see the full stack trace of the errors, re-run Maven with the -e switch.
";
        let (report, _) = feed(mvn_out, "").finish("mvn compile", 4096, None).unwrap();
        assert!(
            report.contains(
                "mvn compile: 4 finding(s), 2 error(s), 2 warning(s) · 2 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("error cannot find symbol ×2"), "{report}");
        assert!(report.contains("    symbol:   variable foo"), "{report}");
        assert!(
            report.contains("src/main/java/App.java:14:23, 28:11 [stdout:3-5]"),
            "{report}"
        );
        assert!(report.contains("warning deprecation ×2"), "{report}");
        assert!(
            report.contains("src/main/java/Util.java:8:12, 19:5 [stdout:9-9]"),
            "{report}"
        );
        assert!(
            report.contains("[ERROR] Failed to execute goal"),
            "{report}"
        );
        assert!(!report.contains("-> [Help 1]"), "{report}");

        // Kotlin / Gradle with file:// URIs and plain paths.
        let kotlin_err = "\
> Task :app:compileKotlin FAILED
e: file:///repo/src/main/kotlin/App.kt:12:5 Unresolved reference: foo
e: file:///repo/src/main/kotlin/App.kt:20:9 Unresolved reference: foo
w: /repo/src/main/kotlin/Util.kt:4:10 Parameter 'x' is never used
w: /repo/src/main/kotlin/Util.kt:8:10 Parameter 'y' is never used
FAILURE: Build failed with an exception.
";
        let (report, _) = feed("", kotlin_err)
            .finish("gradlew build", 4096, None)
            .unwrap();
        assert!(
            report.contains(
                "gradlew build: 4 finding(s), 2 error(s), 2 warning(s) · 2 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(
            report.contains("error Unresolved reference: foo ×2"),
            "{report}"
        );
        assert!(
            report.contains("src/main/kotlin/App.kt:12:5, 20:9 [stderr:2-2]"),
            "{report}"
        );
        assert!(
            report.contains("warning Parameter … is never used ×2"),
            "{report}"
        );
        assert!(
            report.contains("FAILURE: Build failed with an exception."),
            "{report}"
        );

        // MSBuild / dotnet (both flush and 2-space indented lines, plus unlocated error).
        let dotnet_out = "\
  Determining projects to restore...
  All projects are up-to-date for restore.
/repo/src/Program.cs(12,5): error CS0103: The name 'foo' does not exist in the current context [/repo/src/App.csproj]
  /repo/src/Program.cs(18,9): error CS0103: The name 'bar' does not exist in the current context [/repo/src/App.csproj]
  src/Helper.cs(4,1): warning CS0168: The variable 'x' is declared but never used [/repo/src/App.csproj]
  src/Helper.cs(9,1): warning CS0168: The variable 'y' is declared but never used [/repo/src/App.csproj]
CSC : error CS5001: Program does not contain a static 'Main' method suitable for an entry point
Build FAILED.
";
        let (report, _) = feed(dotnet_out, "")
            .finish("dotnet build", 4096, None)
            .unwrap();
        assert!(
            report.contains("dotnet build: 4 finding(s), 2 error(s), 2 warning(s) · 2 rule(s) · 2 file(s) · 1 error(s) without a location"),
            "{report}"
        );
        assert!(report.contains("error CS0103 ×2"), "{report}");
        assert!(
            report.contains("src/Program.cs:12:5, 18:9 [stdout:3-3]"),
            "{report}"
        );
        assert!(report.contains("warning CS0168 ×2"), "{report}");
        assert!(
            report.contains("src/Helper.cs:4:1, 9:1 [stdout:5-5]"),
            "{report}"
        );
        assert!(report.contains("CSC : error CS5001:"), "{report}");
        assert!(report.contains("Build FAILED."), "{report}");

        // gcc/clang warnings + unlocated linker failure hoisted above warnings.
        let gcc_err = "\
src/main.c: In function 'main':
src/main.c:12:9: warning: unused variable 'a' [-Wunused-variable]
   12 |     int a = 1;
      |         ^
src/main.c:15:9: warning: unused variable 'b' [-Wunused-variable]
src/main.c:20:5: note: in expansion of macro 'FOO'
/usr/bin/ld: main.o: undefined reference to 'missing_symbol'
collect2: error: ld returned 1 exit status
make: *** [Makefile:12: app] Error 1
";
        let (report, _) = feed("", gcc_err).finish("make", 4096, None).unwrap();
        assert!(
            report.contains(
                "make: 2 finding(s), 0 error(s), 2 warning(s) · 1 rule(s) · 1 file(s) · 2 error(s) without a location\n"
            ),
            "{report}"
        );
        // Unlocated linker errors come right after the header when there are 0 located errors.
        let ld_pos = report
            .find("/usr/bin/ld: main.o: undefined reference")
            .unwrap();
        let warn_pos = report.find("warning -Wunused-variable ×2").unwrap();
        assert!(ld_pos < warn_pos, "{report}");
        assert!(
            report.contains("collect2: error: ld returned 1 exit status"),
            "{report}"
        );
        assert!(
            report.contains("make: *** [Makefile:12: app] Error 1"),
            "{report}"
        );

        // gcc `-Werror=...` extracts the full bracketed rule name.
        let werror = "src/net.c:8:5: error: unused variable 'x' [-Werror=unused-variable]\n\
                      src/net.c:9:5: error: unused variable 'y' [-Werror=unused-variable]\n";
        let (report, _) = feed("", werror).finish("make", 4096, None).unwrap();
        assert!(
            report.contains("error -Werror=unused-variable ×2"),
            "{report}"
        );

        // Go build with `# pkg/path` headers, error severity, and indented detail lines.
        let go_err = "\
# example.com/app/internal/auth
internal/auth/token.go:14:9: imported and not used: \"fmt\"
internal/auth/token.go:15:9: imported and not used: \"os\"
internal/auth/token.go:22:15: cannot use x (variable of type int) as string value in assignment
\thave (int)
\twant (string)
# example.com/app/cmd/server
cmd/server/main.go:8:2: undefined: secretKey
";
        let (report, _) = feed("", go_err).finish("go build", 4096, None).unwrap();
        assert!(
            report.contains(
                "go build: 4 finding(s), 4 error(s), 0 warning(s) · 3 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(
            report.contains("error imported and not used: … ×2"),
            "{report}"
        );
        assert!(
            report.contains("internal/auth/token.go:14:9, 15:9 [stderr:2-2]"),
            "{report}"
        );
        assert!(report.contains("    \thave (int)"), "{report}");
        assert!(
            report.contains("cmd/server/main.go:8:2 [stderr:8-8]"),
            "{report}"
        );
    }

    #[test]
    fn clean_build_summaries_cover_each_ecosystem_and_abort_on_unrecognized_lines() {
        // Cargo preserves exact `cargo status line(s)` wording.
        let cargo = feed(
            "",
            "   Compiling demo v0.1.0 (/repo)\n    Finished `dev` profile in 0.42s\n",
        );
        assert_eq!(
            cargo.clean("cargo build").as_deref(),
            Some(
                "cargo build: 0 findings · 2 cargo status line(s) [stderr:1-2] · Finished `dev` profile in 0.42s\n"
            )
        );

        let cases: [(&str, &str, &str, &str); 8] = [
            (
                "cmake",
                "-- Configuring done (0.2s)\n\
                 -- Generating done (0.1s)\n\
                 -- Build files have been written to: /repo/build\n\
                 [ 50%] Building CXX object CMakeFiles/app.dir/src/main.cpp.o\n\
                 [100%] Linking CXX executable app\n\
                 [100%] Built target app\n",
                "",
                "cmake: 0 findings · 6 build status line(s) [stdout:1-6] · [100%] Built target app\n",
            ),
            (
                "ninja",
                "[1/3] Building C object src/error_handler.c.o\n\
                 [2/3] Building C object src/main.c.o\n\
                 [3/3] Linking C executable app\n",
                "",
                "ninja: 0 findings · 3 build status line(s) [stdout:1-3]\n",
            ),
            (
                "make",
                "make[1]: Entering directory '/repo/sub'\n\
                 make[1]: Nothing to be done for 'all'.\n\
                 make[1]: Leaving directory '/repo/sub'\n",
                "",
                "make: 0 findings · 3 build status line(s) [stdout:1-3] · make[1]: Nothing to be done for 'all'.\n",
            ),
            (
                "gradlew build",
                "> Task :compileJava UP-TO-DATE\n\
                 > Task :processResources NO-SOURCE\n\
                 > Task :classes FROM-CACHE\n\
                 \n\
                 BUILD SUCCESSFUL in 1s\n\
                 3 actionable tasks: 3 up-to-date\n",
                "",
                "gradlew build: 0 findings · 5 build status line(s) [stdout:1-6] · BUILD SUCCESSFUL in 1s\n",
            ),
            (
                "mvn compile",
                "[INFO] Scanning for projects...\n\
                 [INFO]\n\
                 [INFO] Building demo 1.0.0\n\
                 [INFO] Nothing to compile - all classes are up to date\n\
                 [INFO] BUILD SUCCESS\n",
                "",
                "mvn compile: 0 findings · 5 build status line(s) [stdout:1-5] · [INFO] BUILD SUCCESS\n",
            ),
            (
                "dotnet build",
                concat!(
                    "  Determining projects to restore...\n",
                    "  Restored /repo/App.csproj (in 120 ms).\n",
                    "  App -> /repo/bin/Debug/net8.0/App.dll\n",
                    "\n",
                    "Build succeeded.\n",
                    "    0 Warning(s)\n",
                    "    0 Error(s)\n",
                    "\n",
                    "Time Elapsed 00:00:01.42\n",
                ),
                "",
                "dotnet build: 0 findings · 7 build status line(s) [stdout:1-9] · Build succeeded.\n",
            ),
            (
                "swift build",
                "Building for debugging...\n\
                 [1/3] Compiling MyLib Helper.swift\n\
                 [2/3] Compiling MyApp main.swift\n\
                 [3/3] Linking MyApp\n\
                 Build complete! (1.42s)\n",
                "",
                "swift build: 0 findings · 5 build status line(s) [stdout:1-5] · Build complete! (1.42s)\n",
            ),
            (
                "go build",
                "",
                "go: downloading github.com/pkg/errors v0.9.1\n\
                 go: downloading golang.org/x/sys v0.20.0\n",
                "go build: 0 findings · 2 build status line(s) [stderr:1-2]\n",
            ),
        ];
        for (label, out, err, expected) in cases {
            assert_eq!(
                feed(out, err).clean(label).as_deref(),
                Some(expected),
                "{label}"
            );
        }

        // Any unrecognized line, warning, test summary, or failed task MUST abort `clean()`.
        for (label, out, err) in [
            (
                "cmake",
                "[ 50%] Building CXX object a.o\ncustom notice\n",
                "",
            ),
            ("ninja", "[1/2] Compiling a.c\n[2/2] FAILED: a.o\n", ""),
            ("gradlew", "> Task :compileJava FAILED\n", ""),
            (
                "mvn",
                "[INFO] Building demo\n[WARNING] deprecated plugin\n[INFO] BUILD SUCCESS\n",
                "",
            ),
            (
                "mvn",
                "[INFO] Tests run: 5, Failures: 0, Errors: 0\n[INFO] BUILD SUCCESS\n",
                "",
            ),
            (
                "dotnet",
                "Build succeeded.\n    1 Warning(s)\n    0 Error(s)\n",
                "",
            ),
            ("go", "", "# example.com/app\ngo: downloading foo v1.0.0\n"),
            ("empty", "", ""),
        ] {
            assert_eq!(feed(out, err).clean(label), None, "{label}: {out}{err}");
        }
    }

    #[test]
    fn tool_dispatch_names_round_trip() {
        assert_eq!(Tool::ALL.len(), 13);
        let expected = [
            ("phpstan", Tool::Phpstan),
            ("clang-tidy", Tool::ClangTidy),
            ("cppcheck", Tool::Cppcheck),
            ("yamllint", Tool::Yamllint),
            ("markdownlint", Tool::Markdownlint),
            ("sqlfluff", Tool::Sqlfluff),
            ("ktlint", Tool::Ktlint),
            ("stylelint", Tool::Stylelint),
            ("deno", Tool::Deno),
            ("ansible-lint", Tool::AnsibleLint),
            ("actionlint", Tool::Actionlint),
            ("credo", Tool::Credo),
            ("dart-analyze", Tool::DartAnalyze),
        ];
        for (name, tool) in expected {
            assert_eq!(Tool::from_name(name), Some(tool), "{name}");
            assert_eq!(tool.name(), name);
        }
        assert_eq!(
            Tool::from_name("markdownlint-cli2"),
            Some(Tool::Markdownlint)
        );
        assert_eq!(Tool::from_name("mix credo"), Some(Tool::Credo));
        assert_eq!(Tool::from_name("dart analyze"), Some(Tool::DartAnalyze));
        assert_eq!(Tool::from_name("flutter analyze"), Some(Tool::DartAnalyze));
        assert_eq!(Tool::from_name("unknown-tool"), None);

        let mut lint = Lint::for_tool(Path::new("/repo"), Tool::Phpstan);
        lint.observe(
            "src/Foo.php:42: Method App\\Foo::bar() should return int. [return.type]\n",
            1,
            Stream::Stdout,
        );
        let (report, _) = lint.finish("phpstan", 4096, None).unwrap();
        assert!(report.contains("error return.type ×1"), "{report}");
    }

    #[test]
    fn phpstan_table_and_raw_diagnostics_group_by_identifier() {
        let table_out = "\
Note: Using configuration file /repo/phpstan.neon.
 12/12 [▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓] 100%

 ------ --------------------------------------------------------------------- 
  Line   src/Foo.php                                                          
 ------ --------------------------------------------------------------------- 
  42     Method App\\Foo::bar() should return int but returns string.          
         🪪  return.type                                                       
  57     Method App\\Foo::baz() should return int but returns string.          
         🪪  return.type                                                       
  71     Variable $baz might not be defined.                                  
         💡 Consider initializing it before use.                              
         🪪  variable.undefined                                                
 ------ --------------------------------------------------------------------- 

 ------ --------------------------------------------------------------------- 
  Line   src/Bar.php (in context of class App\\ConcreteBar)                    
 ------ --------------------------------------------------------------------- 
  10     Method App\\Bar::qux() should return int                              
         but returns string.                                                  
         🪪  return.type                                                       
  24     Call to an undefined method App\\Bar::missing().                      
         🪪  method.notFound                                                   
 ------ --------------------------------------------------------------------- 

 ------ --------------------------------------------------------------------- 
  Error                                                                       
 ------ --------------------------------------------------------------------- 
  Ignored error pattern #^Foo$# was not matched in reported errors.           
 ------ --------------------------------------------------------------------- 

 [ERROR] Found 6 errors                                                       
";
        let (report, _) = feed(table_out, "")
            .finish("phpstan analyse", 4096, None)
            .unwrap();
        assert!(
            report.len() < table_out.len(),
            "expected byte reduction: {} >= {}",
            report.len(),
            table_out.len()
        );
        assert!(
            report.contains(
                "phpstan analyse: 5 finding(s), 5 error(s), 0 warning(s) · 3 rule(s) · 2 file(s) · 1 error(s) without a location"
            ),
            "{report}"
        );
        assert!(report.contains("error return.type ×3"), "{report}");
        assert!(
            report.contains("src/Bar.php:10, src/Foo.php:42, 57 [stdout:7-8]"),
            "{report}"
        );
        assert!(report.contains("error variable.undefined ×1"), "{report}");
        assert!(
            report.contains("💡 Consider initializing it before use."),
            "{report}"
        );
        assert!(report.contains("src/Foo.php:71 [stdout:11-13]"), "{report}");
        assert!(report.contains("error method.notFound ×1"), "{report}");
        assert!(
            report.contains("Ignored error pattern #^Foo$# was not matched"),
            "{report}"
        );
        assert!(
            report.contains("[ERROR] Found 6 errors [stdout:32]"),
            "{report}"
        );

        let raw_out = "\
/repo/src/Foo.php:42:Method App\\Foo::bar() should return int but returns string. [return.type]
/repo/src/Foo.php:57:Method App\\Foo::baz() should return int but returns string. [return.type]
/repo/src/Bar.php:10:Method App\\Bar::qux() should return int but returns string. [return.type]
/repo/src/Bar.php:24:Variable $baz might not be defined. [variable.undefined]
/repo/src/Baz.php:8:Variable $qux might not be defined. [variable.undefined]
";
        let (report, _) = feed(raw_out, "")
            .finish("phpstan analyse", 4096, None)
            .unwrap();
        assert!(
            report.len() < raw_out.len(),
            "expected raw byte reduction: {} >= {}",
            report.len(),
            raw_out.len()
        );
        assert!(
            report.contains(
                "phpstan analyse: 5 finding(s), 5 error(s), 0 warning(s) · 2 rule(s) · 3 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("error return.type ×3"), "{report}");
        assert!(
            report.contains("src/Bar.php:10, src/Foo.php:42, 57 [stdout:1-1]"),
            "{report}"
        );
        assert!(report.contains("error variable.undefined ×2"), "{report}");
        assert!(
            report.contains("src/Bar.php:24, src/Baz.php:8 [stdout:4-4]"),
            "{report}"
        );
    }

    #[test]
    fn clang_tidy_groups_by_check_and_preserves_notes_via_span_only() {
        let out = "\
/repo/src/math.cpp:42:10: warning: 42 is a magic number; consider replacing it with a named constant [readability-magic-numbers]
   42 |   return x * 42;
      |              ^
/repo/src/math.cpp:58:14: warning: 100 is a magic number; consider replacing it with a named constant [readability-magic-numbers]
   58 |   return y + 100;
      |              ^
/repo/src/math.cpp:72:14: warning: 256 is a magic number; consider replacing it with a named constant [readability-magic-numbers]
   72 |   return z + 256;
      |              ^
/repo/src/net.cpp:25:12: error: narrowing conversion from 'double' to 'int' [bugprone-narrowing-conversions,-warnings-as-errors]
   25 |   int n = d;
      |           ^
/repo/src/net.cpp:10:3: note: 'n' declared here
   10 |   int n;
      |   ^
/repo/src/net.cpp:31:12: error: narrowing conversion from 'float' to 'int' [bugprone-narrowing-conversions,-warnings-as-errors]
   31 |   int m = f;
      |           ^
/repo/src/net.cpp:44:5: warning: use auto when initializing with a cast [modernize-use-auto]
   44 |   int* p = static_cast<int*>(buf);
      |   ^~~~
";
        let err = "\
5 warnings and 2 errors generated.
Suppressed 2 warnings (2 in non-user code).
";
        let (report, _) = feed(out, err).finish("clang-tidy", 4096, None).unwrap();
        assert!(
            report.len() < out.len() + err.len(),
            "expected byte reduction: {} >= {}",
            report.len(),
            out.len() + err.len()
        );
        assert!(
            report.contains(
                "clang-tidy: 6 finding(s), 2 error(s), 4 warning(s) · 3 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(
            report.contains("error bugprone-narrowing-conversions ×2"),
            "{report}"
        );
        // Archive span covers lines 10-15 (the error, its snippet, AND the follow-up note + snippet),
        // while the note text itself is NOT duplicated into the report.
        assert!(
            report.contains("src/net.cpp:25:12, 31:12 [stdout:10-15]"),
            "{report}"
        );
        assert!(!report.contains("note: 'n' declared here"), "{report}");
        assert!(
            report.contains("warning readability-magic-numbers ×3"),
            "{report}"
        );
        assert!(
            report.contains("src/math.cpp:42:10, 58:14, 72:14 [stdout:1-3]"),
            "{report}"
        );
        assert!(report.contains("warning modernize-use-auto ×1"), "{report}");
        assert!(
            report.contains("5 warnings and 2 errors generated. [stderr:1]"),
            "{report}"
        );
    }

    #[test]
    fn cppcheck_modern_and_bracket_formats_group_by_id() {
        let modern_err = "\
/repo/src/net.c:42:10: error: Null pointer dereference: ptr [nullPointer]
 return *ptr;
         ^
/repo/src/net.c:38:15: note: Assignment 'ptr=NULL', assigned value is 0
 int *ptr = NULL;
               ^
/repo/src/net.c:64:10: error: Null pointer dereference: other [nullPointer]
 return *other;
         ^
/repo/src/util.c:19:5: style: Condition 'x==0' is always true [knownConditionTrueFalse]
 if (x == 0)
     ^
/repo/src/util.c:25:1: performance: Function parameter 's' should be passed by const reference. [passedByValue]
/repo/src/util.c:33:1: performance: Function parameter 't' should be passed by const reference. [passedByValue]
/repo/src/util.c:40:1: portability: Casting between integer* and float* has incompatible representation. [invalidPointerCast]
nofile:0:0: information: Active checkers: 113/592 (use --checkers-report=<filename> to see details) [checkersReport]
";
        let (report, _) = feed("", modern_err).finish("cppcheck", 4096, None).unwrap();
        assert!(
            report.len() < modern_err.len(),
            "expected byte reduction: {} >= {}",
            report.len(),
            modern_err.len()
        );
        assert!(
            report.contains(
                "cppcheck: 6 finding(s), 2 error(s), 4 warning(s) · 4 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("error nullPointer ×2"), "{report}");
        assert!(
            report.contains("src/net.c:42:10, 64:10 [stderr:1-6]"),
            "{report}"
        );
        assert!(report.contains("warning passedByValue ×2"), "{report}");
        assert!(
            report.contains("warning knownConditionTrueFalse ×1"),
            "{report}"
        );
        assert!(report.contains("warning invalidPointerCast ×1"), "{report}");

        let bracket_err = "\
[src/net.c:38] -> [src/net.c:42]: (error) Possible null pointer dereference: ptr [nullPointer]
[src/net.c:64]: (error) Null pointer dereference: other [nullPointer]
[src/util.c:19]: (warning) Uninitialized variable: a [uninitvar]
[src/util.c:28]: (warning) Uninitialized variable: b [uninitvar]
[src/util.c:35:4]: (style) Unused variable: tmp [unusedVariable]
";
        let (report, _) = feed("", bracket_err)
            .finish("cppcheck", 4096, None)
            .unwrap();
        assert!(
            report.len() < bracket_err.len(),
            "expected bracket byte reduction: {} >= {}",
            report.len(),
            bracket_err.len()
        );
        assert!(
            report.contains(
                "cppcheck: 5 finding(s), 2 error(s), 3 warning(s) · 3 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("error nullPointer ×2"), "{report}");
        assert!(report.contains("src/net.c:42, 64 [stderr:1-1]"), "{report}");
        assert!(report.contains("warning uninitvar ×2"), "{report}");
        assert!(
            report.contains("src/util.c:19, 28 [stderr:3-3]"),
            "{report}"
        );
        assert!(report.contains("warning unusedVariable ×1"), "{report}");
        assert!(report.contains("src/util.c:35:4 [stderr:5-5]"), "{report}");
    }

    #[test]
    fn yamllint_standard_and_parsable_formats_group_by_rule() {
        let standard_out = "\
.github/workflows/ci.yml
  12:1      error    too many blank lines (2 > 1)  (empty-lines)
  42:81     warning  line too long (94 > 80 characters)  (line-length)
  55:10     error    trailing spaces  (trailing-spaces)
  61:10     error    trailing spaces  (trailing-spaces)

config/app.yaml
  4:1       warning  missing document start \"---\"  (document-start)
  19:81     warning  line too long (102 > 80 characters)  (line-length)
  25:81     warning  line too long (115 > 80 characters)  (line-length)
";
        let (report, _) = feed(standard_out, "")
            .finish("yamllint", 4096, None)
            .unwrap();
        assert!(
            report.len() < standard_out.len(),
            "expected standard byte reduction: {} >= {}",
            report.len(),
            standard_out.len()
        );
        assert!(
            report.contains(
                "yamllint: 7 finding(s), 3 error(s), 4 warning(s) · 4 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("error trailing-spaces ×2"), "{report}");
        assert!(
            report.contains(".github/workflows/ci.yml:55:10, 61:10 [stdout:4-4]"),
            "{report}"
        );
        assert!(
            report.contains("error empty-lines ×1  too many blank lines (2 > 1)"),
            "{report}"
        );
        assert!(report.contains("warning line-length ×3"), "{report}");
        assert!(
            report.contains(
                ".github/workflows/ci.yml:42:81, config/app.yaml:19:81, 25:81 [stdout:3-3]"
            ),
            "{report}"
        );
        assert!(report.contains("warning document-start ×1"), "{report}");

        let parsable_out = "\
.github/workflows/ci.yml:12:1: [error] trailing spaces (trailing-spaces)
.github/workflows/ci.yml:42:10: [error] trailing spaces (trailing-spaces)
.github/workflows/ci.yml:55:81: [warning] line too long (94 > 80 characters) (line-length)
config/app.yaml:19:81: [warning] line too long (102 > 80 characters) (line-length)
config/app.yaml:25:81: [warning] line too long (115 > 80 characters) (line-length)
";
        let (report, _) = feed(parsable_out, "")
            .finish("yamllint", 4096, None)
            .unwrap();
        assert!(
            report.len() < parsable_out.len(),
            "expected parsable byte reduction: {} >= {}",
            report.len(),
            parsable_out.len()
        );
        assert!(
            report.contains(
                "yamllint: 5 finding(s), 2 error(s), 3 warning(s) · 2 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(
            report.contains("error trailing-spaces ×2  trailing spaces"),
            "{report}"
        );
        assert!(
            report.contains(".github/workflows/ci.yml:12:1, 42:10 [stdout:1-1]"),
            "{report}"
        );
        assert!(report.contains("warning line-length ×3"), "{report}");
    }

    #[test]
    fn markdownlint_and_cli2_group_by_rule_code() {
        let cli1_err = "\
README.md:42:10 MD013/line-length Line length [Expected: 80; Actual: 112]
README.md:58:10 MD013/line-length Line length [Expected: 80; Actual: 104]
README.md:70 MD033/no-inline-html Inline HTML [Element: br]
docs/guide.md:12:1 MD013/line-length Line length [Expected: 80; Actual: 95]
docs/guide.md:18 MD033/no-inline-html Inline HTML [Element: details]
docs/guide.md:25:1 MD022/blanks-around-headings/blanks-around-headers Headings should be surrounded by blank lines [Expected: 1; Actual: 0; Below]
";
        let (report, _) = feed("", cli1_err)
            .finish("markdownlint", 4096, None)
            .unwrap();
        assert!(
            report.len() < cli1_err.len(),
            "expected byte reduction: {} >= {}",
            report.len(),
            cli1_err.len()
        );
        assert!(
            report.contains(
                "markdownlint: 6 finding(s), 6 error(s), 0 warning(s) · 3 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("error MD013/line-length ×3"), "{report}");
        assert!(
            report.contains("README.md:42:10, 58:10, docs/guide.md:12:1 [stderr:1-1]"),
            "{report}"
        );
        assert!(report.contains("error MD033/no-inline-html ×2"), "{report}");
        assert!(
            report.contains("README.md:70, docs/guide.md:18 [stderr:3-3]"),
            "{report}"
        );
        assert!(
            report.contains("error MD022/blanks-around-headings/blanks-around-headers ×1"),
            "{report}"
        );

        let cli2_out = "\
markdownlint-cli2 v0.15.0 (markdownlint v0.35.0)
Finding: **/*.md
Linting: 12 file(s)
Summary: 3 error(s)
";
        let cli2_err = "\
README.md:42:10 error MD013/line-length Line length [Expected: 80; Actual: 112]
README.md:58 error MD013/line-length Line length [Expected: 80; Actual: 98]
docs/guide.md:12:1 warning MD009/no-trailing-spaces Trailing spaces [Expected: 0 or 2; Actual: 3]
";
        let (report, _) = feed(cli2_out, cli2_err)
            .finish("markdownlint-cli2", 4096, None)
            .unwrap();
        assert!(
            report.contains(
                "markdownlint-cli2: 3 finding(s), 2 error(s), 1 warning(s) · 2 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("error MD013/line-length ×2"), "{report}");
        assert!(
            report.contains("warning MD009/no-trailing-spaces ×1"),
            "{report}"
        );
        assert!(
            report.contains("Summary: 3 error(s) [stdout:4]"),
            "{report}"
        );
    }

    #[test]
    fn sqlfluff_groups_by_rule_code_and_preserves_parse_errors() {
        let out = "\
== [models/orders.sql] FAIL
L:   1 | P:   1 | CP01 | Keywords must be consistently upper case. [capitalisation.keywords]
L:  14 | P:   8 | LT01 | Expected single whitespace between comma and next token. [layout.spacing]
L:  42 | P:   5 | LT01 | Unnecessary trailing whitespace. [layout.spacing]
L:  48 | P:   5 | LT01 | Unnecessary trailing whitespace. [layout.spacing]
== [models/users.sql] PASS
== [models/billing.sql] FAIL
L:   7 | P:  12 | CP01 | Keywords must be consistently upper case. [capitalisation.keywords]
L:  11 | P:   4 | CP01 | Keywords must be consistently upper case. [capitalisation.keywords]
L:  19 | P:   1 |  PRS | Line 19, Position 1: Found unparsable section: 'SELECT * FROM'
                       | unexpected end of file
All Finished 📜 🎉!
";
        let (report, _) = feed(out, "").finish("sqlfluff lint", 4096, None).unwrap();
        assert!(
            report.len() < out.len(),
            "expected byte reduction: {} >= {}",
            report.len(),
            out.len()
        );
        assert!(
            report.contains(
                "sqlfluff lint: 7 finding(s), 1 error(s), 6 warning(s) · 3 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("error PRS ×1"), "{report}");
        assert!(
            report.contains("models/billing.sql:19:1 [stdout:10-11]"),
            "{report}"
        );
        assert!(report.contains("warning CP01 ×3"), "{report}");
        assert!(
            report.contains("models/billing.sql:7:12, 11:4, models/orders.sql:1:1 [stdout:2-2]"),
            "{report}"
        );
        assert!(report.contains("warning LT01 ×3"), "{report}");
        assert!(
            report.contains("models/orders.sql:14:8, 42:5, 48:5 [stdout:3-3]"),
            "{report}"
        );
    }

    #[test]
    fn ktlint_groups_standard_and_plain_rules() {
        let out = "\
/repo/src/main/kotlin/App.kt:42:10: Wildcard import (cannot be auto-corrected) (standard:no-wildcard-imports)
/repo/src/main/kotlin/App.kt:43:10: Wildcard import (cannot be auto-corrected) (standard:no-wildcard-imports)
/repo/src/main/kotlin/Util.kt:12:5: Unexpected indentation (2) (should be 4) (standard:indent)
/repo/src/main/kotlin/Util.kt:18:5: Unexpected indentation (6) (should be 4) (standard:indent)
/repo/src/main/kotlin/Util.kt:24:10: Wildcard import (standard:no-wildcard-imports)
src/test/kotlin/AppTest.kt:7:1: Missing newline at end of file (final-newline)
src/test/kotlin/UtilTest.kt:9:1: Missing newline at end of file (final-newline)
";
        let (report, _) = feed(out, "").finish("ktlint", 4096, None).unwrap();
        assert!(
            report.len() < out.len(),
            "expected byte reduction: {} >= {}",
            report.len(),
            out.len()
        );
        assert!(
            report
                .contains("ktlint: 7 finding(s), 0 error(s), 7 warning(s) · 3 rule(s) · 4 file(s)"),
            "{report}"
        );
        assert!(
            report.contains("warning standard:no-wildcard-imports ×3"),
            "{report}"
        );
        assert!(
            report.contains(
                "src/main/kotlin/App.kt:42:10, 43:10, src/main/kotlin/Util.kt:24:10 [stdout:1-1]"
            ),
            "{report}"
        );
        assert!(
            report.contains("warning standard:indent ×2  Unexpected indentation (2) (should be 4)"),
            "{report}"
        );
        assert!(report.contains("warning final-newline ×2"), "{report}");
    }

    #[test]
    fn stylelint_block_and_compact_formats_group_by_rule() {
        let block_out = "\
src/styles/app.css
 12:5   ✖  Expected \"#FFFFFF\" to be \"#ffffff\"         color-hex-case
 24:5   ✖  Expected \"#AABBCC\" to be \"#aabbcc\"         color-hex-case
 42:10  ✖  Unexpected duplicate selector \".btn\"       no-duplicate-selectors
 49:10  ✖  Unexpected duplicate selector \".card\"      no-duplicate-selectors
 54:10  ✖  Unexpected duplicate selector \".modal\"     no-duplicate-selectors
 58:3   ⚠  Expected empty line before comment         comment-empty-line-before

src/styles/theme.scss
  4:1   ✖  Expected \"#000000\" to be \"#000\"           color-hex-case
  9:12  ×  Unexpected unknown at-rule \"@bad\"          scss/at-rule-no-unknown
 14:3   ⚠  Expected empty line before comment         comment-empty-line-before
 22:3   ⚠  Expected empty line before comment         comment-empty-line-before

✖ 10 problems (7 errors, 3 warnings)
";
        let (report, _) = feed(block_out, "").finish("stylelint", 4096, None).unwrap();
        assert!(
            report.len() < block_out.len(),
            "expected block byte reduction: {} >= {}",
            report.len(),
            block_out.len()
        );
        assert!(
            report.contains(
                "stylelint: 10 finding(s), 7 error(s), 3 warning(s) · 4 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("error color-hex-case ×3"), "{report}");
        assert!(
            report
                .contains("src/styles/app.css:12:5, 24:5, src/styles/theme.scss:4:1 [stdout:2-2]"),
            "{report}"
        );
        assert!(
            report.contains("error no-duplicate-selectors ×3"),
            "{report}"
        );
        assert!(
            report.contains("error scss/at-rule-no-unknown ×1"),
            "{report}"
        );
        assert!(
            report.contains("warning comment-empty-line-before ×3"),
            "{report}"
        );
        assert!(
            report.contains("✖ 10 problems (7 errors, 3 warnings) [stdout:15]"),
            "{report}"
        );

        let compact_out = "\
src/styles/app.css: line 12, col 5, error - Expected \"#FFFFFF\" to be \"#ffffff\" (color-hex-case)
src/styles/app.css: line 24, col 5, error - Expected \"#AABBCC\" to be \"#aabbcc\" (color-hex-case)
src/styles/app.css: line 42, col 10, error - Unexpected duplicate selector \".btn\" (no-duplicate-selectors)
src/styles/app.css: line 49, col 10, error - Unexpected duplicate selector \".card\" (no-duplicate-selectors)
src/styles/app.css: line 58, col 3, warning - Expected empty line before comment (comment-empty-line-before)
src/styles/theme.scss: line 4, col 1, error - Expected \"#000000\" to be \"#000\" (color-hex-case)
src/styles/theme.scss: line 9, col 12, error - Unexpected unknown at-rule \"@bad\" (scss/at-rule-no-unknown)
src/styles/theme.scss: line 14, col 3, warning - Expected empty line before comment (comment-empty-line-before)
src/styles/theme.scss: line 22, col 1, error - Expected \"#EFEFEF\" to be \"#efefef\" (color-hex-case)
";
        let (report, _) = feed(compact_out, "")
            .finish("stylelint", 4096, None)
            .unwrap();
        assert!(
            report.len() < compact_out.len(),
            "expected compact byte reduction: {} >= {}",
            report.len(),
            compact_out.len()
        );
        assert!(
            report.contains(
                "stylelint: 9 finding(s), 7 error(s), 2 warning(s) · 4 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("error color-hex-case ×4"), "{report}");
        assert!(
            report.contains(
                "src/styles/app.css:12:5, 24:5, src/styles/theme.scss:4:1, 22:1 [stdout:1-1]"
            ),
            "{report}"
        );
        assert!(
            report.contains("warning comment-empty-line-before ×2"),
            "{report}"
        );
    }

    #[test]
    fn deno_lint_and_deno_check_group_by_rule_with_file_uris() {
        let lint_err = "\
error[no-unused-vars]: `unused` is never used
    at file:///repo/src/app.ts:42:10
  |
42 | const unused = 123;
  |       ^^^^^^
  = hint: If this is intentional, prefix it with an underscore like `_unused`
  = help: for further information visit https://docs.deno.com/lint/rules/no-unused-vars

error[no-unused-vars]: `other` is never used
    at file:///repo/src/util.ts:7:7
  |
7 | const other = 456;
  |       ^^^^^
  = hint: If this is intentional, prefix it with an underscore like `_other`

error[no-unused-vars]: `third` is never used
    at file:///repo/src/util.ts:19:7
  |
19 | const third = 789;
  |       ^^^^^

error[no-explicit-any]: `any` type is not allowed
    at file:///repo/src/api.ts:15:20
  |
15 | export function run(x: any) {}
  |                        ^^^

Found 4 problems
Checked 12 files
";
        let (report, _) = feed("", lint_err).finish("deno lint", 4096, None).unwrap();
        assert!(
            report.len() < lint_err.len(),
            "expected deno lint byte reduction: {} >= {}",
            report.len(),
            lint_err.len()
        );
        assert!(
            report.contains(
                "deno lint: 4 finding(s), 4 error(s), 0 warning(s) · 2 rule(s) · 3 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("error no-unused-vars ×3"), "{report}");
        assert!(
            report.contains("src/app.ts:42:10, src/util.ts:7:7, 19:7 [stderr:1-7]"),
            "{report}"
        );
        assert!(report.contains("error no-explicit-any ×1"), "{report}");
        assert!(
            report.contains("src/api.ts:15:20 [stderr:22-26]"),
            "{report}"
        );
        assert!(report.contains("Found 4 problems [stderr:28]"), "{report}");

        let check_err = "\
Check file:///repo/src/app.ts
TS2322 [ERROR]: Type 'string' is not assignable to type 'number'.
const count: number = \"hello\";
      ~~~~~
    at file:///repo/src/app.ts:12:7

TS2322 [ERROR]: Type 'boolean' is not assignable to type 'number'.
const total: number = true;
      ~~~~~
    at file:///repo/src/app.ts:20:7

TS2322 [ERROR]: Type 'null' is not assignable to type 'number'.
const retries: number = null;
      ~~~~~~~
    at file:///repo/src/app.ts:28:7

TS2339 [ERROR]: Property 'missing' does not exist on type 'User'.
console.log(user.missing);
                 ~~~~~~~
    at file:///repo/src/api.ts:8:18

Found 4 errors.
error: Type checking failed.
";
        let (report, _) = feed("", check_err)
            .finish("deno check", 4096, None)
            .unwrap();
        assert!(
            report.len() < check_err.len(),
            "expected deno check byte reduction: {} >= {}",
            report.len(),
            check_err.len()
        );
        assert!(
            report.contains(
                "deno check: 4 finding(s), 4 error(s), 0 warning(s) · 2 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(!report.contains("error(s) without a location"), "{report}");
        assert!(report.contains("error TS2322 ×3"), "{report}");
        assert!(
            report.contains("src/app.ts:12:7, 20:7, 28:7 [stderr:2-5]"),
            "{report}"
        );
        assert!(report.contains("error TS2339 ×1"), "{report}");
        assert!(
            report.contains("src/api.ts:8:18 [stderr:17-20]"),
            "{report}"
        );
        assert!(report.contains("Found 4 errors. [stderr:22]"), "{report}");
        assert!(
            report.contains("error: Type checking failed. [stderr:23]"),
            "{report}"
        );
    }

    #[test]
    fn ansible_lint_block_and_single_line_formats_group_by_rule() {
        let block_out = "\
WARNING  Listing 5 violation(s) that are fatal
yaml[line-length]: Line too long (185 > 160 characters)
playbooks/deploy.yml:42

fqcn[action-core]: Use FQCN for builtin module actions (copy).
playbooks/deploy.yml:18 Use `ansible.builtin.copy` or `ansible.legacy.copy` instead.

fqcn[action-core]: Use FQCN for builtin module actions (template).
roles/web/tasks/main.yml:9 Task/Handler: Render nginx config

fqcn[action-core]: Use FQCN for builtin module actions (file).
roles/web/tasks/main.yml:27 Task/Handler: Ensure directory exists

yaml[comments]: Too few spaces before comment (warning)
playbooks/deploy.yml:50

Read documentation for instructions on how to ignore specific rule violations.

Failed: 4 failure(s), 1 warning(s) on 6 files. Last profile that met the validation criteria was 'min'.
";
        let (report, _) = feed(block_out, "")
            .finish("ansible-lint", 4096, None)
            .unwrap();
        assert!(
            report.len() < block_out.len(),
            "expected block byte reduction: {} >= {}",
            report.len(),
            block_out.len()
        );
        assert!(
            report.contains(
                "ansible-lint: 5 finding(s), 4 error(s), 1 warning(s) · 3 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("error fqcn[action-core] ×3"), "{report}");
        assert!(
            report.contains("playbooks/deploy.yml:18, roles/web/tasks/main.yml:9, 27 [stdout:5-6]"),
            "{report}"
        );
        assert!(report.contains("error yaml[line-length] ×1"), "{report}");
        assert!(
            report.contains("playbooks/deploy.yml:42 [stdout:2-3]"),
            "{report}"
        );
        assert!(report.contains("warning yaml[comments] ×1"), "{report}");
        assert!(
            report.contains("Failed: 4 failure(s), 1 warning(s) on 6 files."),
            "{report}"
        );

        let pep8_out = "\
playbooks/deploy.yml:18: fqcn[action-core]: Use FQCN for builtin module actions (copy).
roles/web/tasks/main.yml:9: fqcn[action-core]: Use FQCN for builtin module actions (template).
roles/web/tasks/main.yml:27: fqcn[action-core]: Use FQCN for builtin module actions (file).
playbooks/deploy.yml:42: risky-shell-pipe: Shells that use pipes should set the pipefail option.
roles/web/tasks/main.yml:33: risky-shell-pipe: Shells that use pipes should set the pipefail option.
playbooks/deploy.yml:50: yaml[comments]: Too few spaces before comment (warning)
";
        let (report, _) = feed(pep8_out, "")
            .finish("ansible-lint", 4096, None)
            .unwrap();
        assert!(
            report.len() < pep8_out.len(),
            "expected pep8 byte reduction: {} >= {}",
            report.len(),
            pep8_out.len()
        );
        assert!(
            report.contains(
                "ansible-lint: 6 finding(s), 5 error(s), 1 warning(s) · 3 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("error fqcn[action-core] ×3"), "{report}");
        assert!(report.contains("error risky-shell-pipe ×2"), "{report}");
        assert!(report.contains("warning yaml[comments] ×1"), "{report}");
    }

    #[test]
    fn actionlint_groups_by_rule_and_captures_full_snippet_ranges() {
        let out = "\
.github/workflows/ci.yml:14:9: label \"ubuntu-18.04\" is unknown. available labels are \"ubuntu-latest\", \"macos-latest\" [runner-label]
   |
14 |     runs-on: ubuntu-18.04
   |         ^~~~~~~~~~~~
.github/workflows/ci.yml:28:22: property \"foo\" is not defined in object type {bar: string} [expression]
   |
28 |       if: ${{ github.foo == 'bar' }}
   |                      ^~~
.github/workflows/ci.yml:42:10: shellcheck reported issue in this script: SC2086:info:1:6: Double quote to prevent globbing and word splitting [shellcheck]
   |
42 |         run: echo $FOO
   |          ^~~~~~~~~~~~
.github/workflows/release.yml:19:9: label \"macos-11\" is unknown. available labels are \"ubuntu-latest\", \"macos-latest\" [runner-label]
   |
19 |     runs-on: macos-11
   |         ^~~~~~~~
.github/workflows/release.yml:31:5: run-step: shellcheck reported issue in this script: SC2046:warning:1:1: Quote this to prevent word splitting [shellcheck]
   |
31 |         run: rm $(ls)
   |     ^~~~~~~~~~~~~~~~~
";
        let (report, _) = feed(out, "").finish("actionlint", 4096, None).unwrap();
        assert!(
            report.len() < out.len(),
            "expected byte reduction: {} >= {}",
            report.len(),
            out.len()
        );
        assert!(
            report.contains(
                "actionlint: 5 finding(s), 0 error(s), 5 warning(s) · 3 rule(s) · 2 file(s)"
            ),
            "{report}"
        );
        assert!(report.contains("warning runner-label ×2"), "{report}");
        assert!(
            report.contains(
                ".github/workflows/ci.yml:14:9, .github/workflows/release.yml:19:9 [stdout:1-4]"
            ),
            "{report}"
        );
        assert!(report.contains("warning shellcheck ×2"), "{report}");
        assert!(
            report.contains(
                ".github/workflows/ci.yml:42:10, .github/workflows/release.yml:31:5 [stdout:9-12]"
            ),
            "{report}"
        );
        assert!(report.contains("warning expression ×1"), "{report}");
        assert!(
            report.contains(".github/workflows/ci.yml:28:22 [stdout:5-8]"),
            "{report}"
        );
    }

    #[test]
    fn credo_default_oneline_and_flycheck_formats_group_findings() {
        let default_out = "\
Checking 24 source files ...

  Software Design
┃ 
┃ [D] ↗ Modules should have a @moduledoc tag.
┃       lib/my_app/accounts.ex:1:11 #(MyApp.Accounts)
┃ [D] ↗ Modules should have a @moduledoc tag.
┃       lib/my_app/billing.ex:1:11 #(MyApp.Billing)

  Warnings - please take a look
┃ 
┃ [W] ↗ `IO.inspect/1` should not be used in production code.
┃       lib/my_app/orders.ex:42:10 #(MyApp.Orders.create)
┃ [W] ↗ `IO.inspect/2` should not be used in
┃       production code.
┃       lib/my_app/orders.ex:58:10 #(MyApp.Orders.update)
┃ [W] → Unused operation: `Enum.map/2` result is ignored.
┃       lib/my_app/worker.ex:19:5 #(MyApp.Worker.run)

Please report incorrect results: https://github.com/rrrene/credo/issues

Analysis took 0.1 seconds (0.04s to load, 0.06s running 52 checks on 24 files)
142 mods/funs, found 2 software design suggestions, 3 warnings.

Use `mix credo explain` to explain issues, `mix credo --help` for options.
";
        let (report, _) = feed(default_out, "")
            .finish("mix credo", 4096, None)
            .unwrap();
        assert!(
            report.len() < default_out.len(),
            "expected default byte reduction: {} >= {}",
            report.len(),
            default_out.len()
        );
        assert!(
            report.contains(
                "mix credo: 5 finding(s), 0 error(s), 5 warning(s) · 3 rule(s) · 4 file(s)"
            ),
            "{report}"
        );
        assert!(
            report.contains("warning Modules should have a @moduledoc tag. ×2"),
            "{report}"
        );
        assert!(
            report.contains("lib/my_app/accounts.ex:1:11, lib/my_app/billing.ex:1:11 [stdout:5-6]"),
            "{report}"
        );
        assert!(
            report.contains("warning … should not be used in production code. ×2"),
            "{report}"
        );
        assert!(
            report.contains("lib/my_app/orders.ex:42:10, 58:10 [stdout:12-13]"),
            "{report}"
        );
        assert!(
            report.contains("142 mods/funs, found 2 software design suggestions, 3 warnings."),
            "{report}"
        );

        // Plain `[W] ↗ message` without leading `┃` followed by `┃   path:line:col`.
        let plain_block = "\
[W] ↗ `IO.inspect/1` should not be used in production code.
┃   lib/app.ex:42:10
[W] ↗ `IO.inspect/2` should not be used in production code.
┃   lib/app.ex:55:10
";
        let (report, _) = feed(plain_block, "")
            .finish("mix credo", 4096, None)
            .unwrap();
        assert!(
            report.contains("warning … should not be used in production code. ×2"),
            "{report}"
        );
        assert!(
            report.contains("lib/app.ex:42:10, 55:10 [stdout:1-2]"),
            "{report}"
        );

        // `--format oneline` and `--format flycheck`.
        let oneline_out = "\
[W] ↗ lib/app.ex:42:10 `IO.inspect/1` should not be used in production code.
[W] ↗ lib/app.ex:58:10 `IO.inspect/2` should not be used in production code.
[R] → lib/util.ex:14:7 Function names should use snake_case.
";
        let (report, _) = feed(oneline_out, "")
            .finish("mix credo", 4096, None)
            .unwrap();
        assert!(
            report.contains("warning … should not be used in production code. ×2"),
            "{report}"
        );
        assert!(
            report.contains("lib/app.ex:42:10, 58:10 [stdout:1-1]"),
            "{report}"
        );

        let flycheck_out = "\
lib/app.ex:42:10: W: `IO.inspect/1` should not be used in production code.
lib/app.ex:58:10: W: `IO.inspect/2` should not be used in production code.
lib/orders.ex:14:5: W: `IO.inspect/1` should not be used in production code.
lib/orders.ex:29:5: W: `IO.inspect/1` should not be used in production code.
lib/worker.ex:88:7: W: `IO.inspect/2` should not be used in production code.
lib/accounts.ex:1: D: Modules should have a @moduledoc tag.
lib/billing.ex:1: D: Modules should have a @moduledoc tag.
lib/orders.ex:1: D: Modules should have a @moduledoc tag.
lib/shipping.ex:1: D: Modules should have a @moduledoc tag.
lib/worker.ex:1: D: Modules should have a @moduledoc tag.
lib/parser.ex:102:7: F: Function is too complex (cyclomatic complexity is 12, max is 9).
";
        let (report, _) = feed(flycheck_out, "")
            .finish("mix credo", 4096, None)
            .unwrap();
        assert!(
            report.len() < flycheck_out.len(),
            "expected flycheck byte reduction: {} >= {}",
            report.len(),
            flycheck_out.len()
        );
        assert!(
            report.contains(
                "mix credo: 11 finding(s), 0 error(s), 11 warning(s) · 3 rule(s) · 7 file(s)"
            ),
            "{report}"
        );
        assert!(
            report.contains("warning … should not be used in production code. ×5"),
            "{report}"
        );
        assert!(
            report.contains("warning Modules should have a @moduledoc tag. ×5"),
            "{report}"
        );
    }

    #[test]
    fn clean_lint_summaries_and_fail_closed_guards_cover_new_ecosystems() {
        let cases: [(&str, &str, &str, &str); 9] = [
            (
                "phpstan analyse",
                "Note: Using configuration file /repo/phpstan.neon.\n\
                 \x2012/12 [▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓] 100%\n\
                 \n\
                 \x20[OK] No errors\n",
                "",
                "phpstan analyse: 0 findings · 3 lint status line(s) [stdout:1-4] · [OK] No errors\n",
            ),
            (
                "clang-tidy",
                "",
                "5 warnings generated.\n\
                 Suppressed 5 warnings (5 in non-user code).\n\
                 Use -header-filter=.* to display errors from all non-system headers.\n",
                "clang-tidy: 0 findings · 3 lint status line(s) [stderr:1-3] · Suppressed 5 warnings (5 in non-user code).\n",
            ),
            (
                "cppcheck",
                "Checking src/a.c ...\n\
                 1/2 files checked 50% done\n\
                 Checking src/b.c ...\n\
                 2/2 files checked 100% done\n",
                "nofile:0:0: information: Active checkers: 113/592 (use --checkers-report=<filename> to see details) [checkersReport]\n",
                "cppcheck: 0 findings · 4 lint status line(s) [stdout:1-4] · 1 lint status line(s) [stderr:1-1] · 2/2 files checked 100% done\n",
            ),
            (
                "markdownlint-cli2",
                "markdownlint-cli2 v0.15.0 (markdownlint v0.35.0)\n\
                 Finding: **/*.md\n\
                 Linting: 12 file(s)\n\
                 Summary: 0 error(s)\n",
                "",
                "markdownlint-cli2: 0 findings · 4 lint status line(s) [stdout:1-4] · Summary: 0 error(s)\n",
            ),
            (
                "sqlfluff lint",
                "== [models/a.sql] PASS\n\
                 == [models/b.sql] PASS\n\
                 All Finished 📜 🎉!\n",
                "",
                "sqlfluff lint: 0 findings · 3 lint status line(s) [stdout:1-3] · All Finished 📜 🎉!\n",
            ),
            (
                "deno lint",
                "Checked 14 files\n",
                "",
                "deno lint: 0 findings · 1 lint status line(s) [stdout:1-1] · Checked 14 files\n",
            ),
            (
                "deno check",
                "",
                "Check file:///repo/src/main.ts\n\
                 Check file:///repo/src/util.ts\n",
                "deno check: 0 findings · 2 lint status line(s) [stderr:1-2]\n",
            ),
            (
                "ansible-lint",
                "Passed: 0 failure(s), 0 warning(s) on 12 files. Last profile that met the validation criteria was 'production'.\n",
                "",
                "ansible-lint: 0 findings · 1 lint status line(s) [stdout:1-1] · Passed: 0 failure(s), 0 warning(s) on 12 files. Last profile that met the validation criteria was 'production'.\n",
            ),
            (
                "mix credo",
                "Checking 24 source files ...\n\
                 \n\
                 Please report incorrect results: https://github.com/rrrene/credo/issues\n\
                 \n\
                 Analysis took 0.1 seconds (0.04s to load, 0.06s running 52 checks on 24 files)\n\
                 142 mods/funs, found no issues.\n\
                 \n\
                 Use `mix credo explain` to explain issues, `mix credo --help` for options.\n",
                "",
                "mix credo: 0 findings · 5 lint status line(s) [stdout:1-8] · 142 mods/funs, found no issues.\n",
            ),
        ];
        for (label, out, err, expected) in cases {
            assert_eq!(
                feed(out, err).clean(label).as_deref(),
                Some(expected),
                "{label}"
            );
        }

        // Fail closed: any missing completion, mismatched warning count, nonzero total,
        // or unrecognized failure with 0 parsed findings MUST abort both `clean()` and `finish()`.
        for (label, out, err) in [
            (
                "phpstan incomplete",
                "Note: Using configuration file /repo/phpstan.neon.\n",
                "",
            ),
            (
                "phpstan crash",
                "Internal error: Out of memory\n [ERROR] Found 1 error\n",
                "",
            ),
            ("clang-tidy unsuppressed", "", "5 warnings generated.\n"),
            (
                "markdownlint-cli2 nonzero",
                "markdownlint-cli2 v0.15.0\nSummary: 2 error(s)\n",
                "",
            ),
            (
                "sqlfluff fail without rows",
                "== [models/broken.sql] FAIL\nAll Finished 📜 🎉!\n",
                "",
            ),
            (
                "ansible-lint warnings",
                "Passed: 0 failure(s), 2 warning(s) on 12 files.\n",
                "",
            ),
            (
                "credo nonzero",
                "Checking 24 source files ...\n142 mods/funs, found 1 warning.\n",
                "",
            ),
            (
                "non-ansible colon line",
                "my-setting: some ordinary value\nnot a location line\n",
                "",
            ),
        ] {
            assert_eq!(feed(out, err).clean(label), None, "clean({label})");
            assert!(
                feed(out, err).finish(label, 4096, None).is_none(),
                "finish({label})"
            );
        }
    }

    #[test]
    fn measured_byte_reductions_across_all_12_linters() {
        let mut phpstan = String::from(
            "Note: Using configuration file /repo/phpstan.neon.\n 12/12 [▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓] 100%\n\n",
        );
        for f in ["src/Orders.php", "src/Billing.php", "src/Users.php"] {
            phpstan.push_str(&format!(
                " ------ ---------------------------------------------------------------------\n  Line   {f}\n ------ ---------------------------------------------------------------------\n"
            ));
            for line in [14, 28, 42, 65] {
                phpstan.push_str(&format!(
                    "  {line}     Method App\\Service::handle() should return int but returns string.\n         🪪  return.type\n"
                ));
            }
            phpstan.push_str(
                " ------ ---------------------------------------------------------------------\n\n",
            );
        }
        phpstan.push_str(" [ERROR] Found 12 errors\n");

        let mut clang_tidy = String::new();
        for (f, line) in [
            ("src/math.cpp", 12),
            ("src/math.cpp", 24),
            ("src/math.cpp", 36),
            ("src/math.cpp", 48),
            ("src/net.cpp", 15),
            ("src/net.cpp", 30),
            ("src/net.cpp", 45),
            ("src/net.cpp", 60),
        ] {
            clang_tidy.push_str(&format!(
                "/repo/{f}:{line}:10: warning: {line} is a magic number; consider replacing it with a named constant [readability-magic-numbers]\n   {line} |   return x * {line};\n      |              ^\n"
            ));
        }

        let mut cppcheck = String::new();
        for (f, line) in [
            ("src/net.c", 14),
            ("src/net.c", 28),
            ("src/net.c", 42),
            ("src/net.c", 56),
            ("src/buf.c", 10),
            ("src/buf.c", 22),
            ("src/buf.c", 34),
            ("src/buf.c", 46),
        ] {
            cppcheck.push_str(&format!(
                "/repo/{f}:{line}:10: error: Null pointer dereference: ptr [nullPointer]\n return *ptr;\n         ^\n/repo/{f}:8:15: note: Assignment 'ptr=NULL', assigned value is 0\n int *ptr = NULL;\n               ^\n"
            ));
        }

        let mut yamllint = String::new();
        for f in [
            ".github/workflows/ci.yml",
            ".github/workflows/release.yml",
            "config/app.yaml",
        ] {
            yamllint.push_str(&format!("{f}\n"));
            for line in [12, 24, 36, 48] {
                yamllint.push_str(&format!(
                    "  {line}:81     warning  line too long (104 > 80 characters)  (line-length)\n"
                ));
            }
            yamllint.push('\n');
        }

        let mut markdownlint = String::new();
        for f in ["README.md", "docs/guide.md", "docs/reference.md"] {
            for line in [10, 22, 34, 46] {
                markdownlint.push_str(&format!(
                    "{f}:{line}:81 MD013/line-length Line length [Expected: 80; Actual: 112]\n"
                ));
            }
        }

        let mut sqlfluff = String::new();
        for f in [
            "models/orders.sql",
            "models/billing.sql",
            "models/users.sql",
        ] {
            sqlfluff.push_str(&format!("== [{f}] FAIL\n"));
            for line in [5, 14, 29, 42] {
                sqlfluff.push_str(&format!(
                    "L:  {line:2} | P:   5 | LT01 | Expected single whitespace between comma and next token. [layout.spacing]\n"
                ));
            }
        }
        sqlfluff.push_str("All Finished 📜 🎉!\n");

        let mut ktlint = String::new();
        for f in [
            "/repo/src/main/kotlin/App.kt",
            "/repo/src/main/kotlin/Service.kt",
            "/repo/src/main/kotlin/Repo.kt",
        ] {
            for line in [4, 5, 6, 7] {
                ktlint.push_str(&format!(
                    "{f}:{line}:1: Wildcard import (cannot be auto-corrected) (standard:no-wildcard-imports)\n"
                ));
            }
        }

        let mut stylelint = String::new();
        for f in [
            "src/styles/app.css",
            "src/styles/theme.css",
            "src/styles/forms.css",
        ] {
            stylelint.push_str(&format!("{f}\n"));
            for line in [12, 24, 36, 48] {
                stylelint.push_str(&format!(
                    " {line}:5   ✖  Expected \"#FFFFFF\" to be \"#ffffff\"         color-hex-case\n"
                ));
            }
            stylelint.push('\n');
        }
        stylelint.push_str("✖ 12 problems (12 errors, 0 warnings)\n");

        let mut deno = String::new();
        for (f, line) in [
            ("src/app.ts", 10),
            ("src/app.ts", 22),
            ("src/util.ts", 8),
            ("src/util.ts", 19),
            ("src/api.ts", 14),
            ("src/api.ts", 28),
        ] {
            deno.push_str(&format!(
                "error[no-unused-vars]: `tmp_{line}` is never used\n    at file:///repo/{f}:{line}:7\n  |\n{line} | const tmp_{line} = 1;\n  |       ^^^^^^\n  = hint: If this is intentional, prefix it with an underscore\n\n"
            ));
        }
        deno.push_str("Found 6 problems\nChecked 12 files\n");

        let mut ansible = String::from("WARNING  Listing 8 violation(s) that are fatal\n");
        for (f, line) in [
            ("playbooks/deploy.yml", 12),
            ("playbooks/deploy.yml", 24),
            ("playbooks/deploy.yml", 36),
            ("playbooks/deploy.yml", 48),
            ("roles/web/tasks/main.yml", 9),
            ("roles/web/tasks/main.yml", 18),
            ("roles/web/tasks/main.yml", 27),
            ("roles/web/tasks/main.yml", 36),
        ] {
            ansible.push_str(&format!(
                "fqcn[action-core]: Use FQCN for builtin module actions (copy).\n{f}:{line} Use `ansible.builtin.copy` instead.\n\n"
            ));
        }
        ansible.push_str("Failed: 8 failure(s), 0 warning(s) on 6 files.\n");

        let mut actionlint = String::new();
        for (f, line) in [
            (".github/workflows/ci.yml", 14),
            (".github/workflows/ci.yml", 28),
            (".github/workflows/ci.yml", 42),
            (".github/workflows/release.yml", 19),
            (".github/workflows/release.yml", 33),
            (".github/workflows/deploy.yml", 22),
        ] {
            actionlint.push_str(&format!(
                "{f}:{line}:9: label \"ubuntu-18.04\" is unknown. available labels are \"ubuntu-latest\", \"macos-latest\" [runner-label]\n   |\n{line} |     runs-on: ubuntu-18.04\n   |         ^~~~~~~~~~~~\n"
            ));
        }

        let mut credo =
            String::from("Checking 24 source files ...\n\n  Warnings - please take a look\n┃ \n");
        for (f, line) in [
            ("lib/my_app/orders.ex", 14),
            ("lib/my_app/orders.ex", 28),
            ("lib/my_app/orders.ex", 42),
            ("lib/my_app/billing.ex", 19),
            ("lib/my_app/billing.ex", 33),
            ("lib/my_app/worker.ex", 11),
            ("lib/my_app/worker.ex", 25),
            ("lib/my_app/worker.ex", 39),
        ] {
            credo.push_str(&format!(
                "┃ [W] ↗ `IO.inspect/1` should not be used in production code.\n┃       {f}:{line}:10 #(MyApp.Worker.run)\n"
            ));
        }
        credo.push_str("\nPlease report incorrect results: https://github.com/rrrene/credo/issues\n\nAnalysis took 0.1 seconds (0.04s to load, 0.06s running 52 checks on 24 files)\n142 mods/funs, found 8 warnings.\n");

        let cases: [(&str, &str, &str); 12] = [
            ("phpstan", &phpstan, ""),
            ("clang-tidy", &clang_tidy, ""),
            ("cppcheck", "", &cppcheck),
            ("yamllint", &yamllint, ""),
            ("markdownlint", "", &markdownlint),
            ("sqlfluff", &sqlfluff, ""),
            ("ktlint", &ktlint, ""),
            ("stylelint", &stylelint, ""),
            ("deno", "", &deno),
            ("ansible-lint", &ansible, ""),
            ("actionlint", &actionlint, ""),
            ("credo", &credo, ""),
        ];
        for (name, out, err) in cases {
            let raw = out.len() + err.len();
            let (report, _) = feed(out, err).finish(name, 4096, None).unwrap();
            let saved = raw.saturating_sub(report.len());
            let pct = (saved as f64 / raw as f64) * 100.0;
            println!("{name:14} {raw:4} B -> {:4} B (-{pct:.1}%)", report.len());
            assert!(
                report.len() < raw,
                "{name}: expected byte reduction, got {} >= {raw}",
                report.len()
            );
        }
    }

    #[test]
    fn dart_and_flutter_analyze_bullet_and_dash_formats_group_by_rule() {
        // 1. Modern `flutter analyze` bullet (`•`) format with errors, warnings, and info.
        let mut bullet_out = String::from("Analyzing my_app...\n\n");
        for (file, line) in [
            ("lib/foo.dart", 12),
            ("lib/foo.dart", 14),
            ("lib/bar.dart", 5),
            ("lib/baz.dart", 9),
            ("lib/qux.dart", 3),
        ] {
            bullet_out.push_str(&format!(
                "   info • Unused import: 'dart:math' • {file}:{line}:8 • unused_import\n"
            ));
        }
        bullet_out.push_str(concat!(
            "warning • The value of the local variable 'x' isn't used • lib/bar.dart:42:9 • unused_local_variable\n",
            "warning • Dead code • lib/bar.dart:51:5 • dead_code\n",
            "  error • Target of URI doesn't exist: 'package:missing/x.dart' • lib/main.dart:3:8 • uri_does_not_exist\n",
            "  error • Target of URI doesn't exist: 'package:missing/y.dart' • lib/app.dart:7:8 • uri_does_not_exist\n",
            "\n9 issues found. (ran in 1.4s)\n",
        ));
        let (report, _) = feed(&bullet_out, "")
            .finish("flutter analyze", 4096, None)
            .unwrap();
        let saved = bullet_out.len().saturating_sub(report.len());
        let pct = (saved as f64 / bullet_out.len() as f64) * 100.0;
        println!(
            "flutter analyze: {} B -> {} B (-{pct:.1}%)",
            bullet_out.len(),
            report.len()
        );
        assert!(
            report.len() < bullet_out.len(),
            "expected byte reduction: {} >= {}",
            report.len(),
            bullet_out.len()
        );
        assert!(
            report.contains(
                "flutter analyze: 9 finding(s), 2 error(s), 7 warning(s) · 4 rule(s) · 6 file(s)"
            ),
            "{report}"
        );
        // Errors are pinned before warnings/info.
        let err_pos = report.find("error uri_does_not_exist ×2").expect(&report);
        let info_pos = report.find("warning unused_import ×5").expect(&report);
        assert!(
            err_pos < info_pos,
            "errors must precede warnings:\n{report}"
        );
        assert!(
            report.contains("lib/app.dart:7:8, lib/main.dart:3:8 [stdout:10-10]"),
            "{report}"
        );
        assert!(report.contains("[stdout:3-3]"), "{report}");
        assert!(
            report.contains("9 issues found. (ran in 1.4s) [stdout:13]"),
            "{report}"
        );

        // 2. Classic / Windows / `dart analyze` dash (`-`) format (both field orders + embedded ` - `).
        let dash_out = concat!(
            "Analyzing my_app...\n",
            "   info - lib/foo.dart:12:8 - Unused import: 'dart:math'. - unused_import\n",
            "   info - lib/bar.dart:18:8 - Unused import: 'dart:async'. - unused_import\n",
            "warning - Value 'a - b' isn't used - lib/bar.dart:42:9 - unused_local_variable\n",
            "  error - lib/main.dart:3:8 - Target of URI doesn't exist: 'package:missing/x.dart'. - uri_does_not_exist\n",
            "\n4 issues found.\n",
        );
        let (dash_report, _) = feed(dash_out, "")
            .finish("dart analyze", 4096, None)
            .unwrap();
        assert!(
            dash_report.contains(
                "dart analyze: 4 finding(s), 1 error(s), 3 warning(s) · 3 rule(s) · 3 file(s)"
            ),
            "{dash_report}"
        );
        assert!(
            dash_report.find("error uri_does_not_exist ×1").unwrap()
                < dash_report.find("warning unused_import ×2").unwrap(),
            "{dash_report}"
        );
        assert!(
            dash_report.contains("warning unused_local_variable ×1  Value 'a - b' isn't used"),
            "{dash_report}"
        );
        assert!(
            dash_report.contains("lib/main.dart:3:8 [stdout:5-5]"),
            "{dash_report}"
        );
        assert!(
            dash_report.contains("4 issues found. [stdout:7]"),
            "{dash_report}"
        );

        // 3. Clean `No issues found!` summary collapses, while incomplete progress fails closed.
        let clean_out = "Analyzing my_app...                                             1.4s\nNo issues found! (ran in 1.4s)\n";
        let clean = feed(clean_out, "").clean("flutter analyze").unwrap();
        assert!(
            clean.contains("flutter analyze: 0 findings")
                && clean.contains("No issues found! (ran in 1.4s)"),
            "{clean}"
        );
        assert_eq!(
            feed("Analyzing my_app...\n", "").clean("dart analyze"),
            None
        );
    }
}
