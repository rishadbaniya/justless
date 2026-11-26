//! `justless find`: literal text across indexed files, narrowed to paths and
//! globs, one page at a time. A cut page says which bound cut it and prints the
//! command for the next page, whose `--after` cursor names the search and the
//! line the page stopped at.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read as _, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Arg, ArgMatches, Command};
use ignore::overrides::{Override, OverrideBuilder};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::index::{Fingerprint, Index};
use crate::workspace::{Workspace, display_path, shell_quote, shell_text};

pub fn command() -> Command<'static> {
    Command::new("find")
        .about("Find exact text across source, docs, and configuration")
        .arg(Arg::new("text").required(true).value_name("TEXT"))
        .arg(
            Arg::new("ignore-case")
                .long("ignore-case")
                .short('i')
                .help("Ignore ASCII case"),
        )
        .arg(
            Arg::new("path")
                .long("path")
                .value_name("PATH")
                .takes_value(true)
                .multiple_occurrences(true)
                .help("Search only this file or directory (repeatable)"),
        )
        .arg(
            Arg::new("glob")
                .long("glob")
                .short('g')
                .value_name("GLOB")
                .takes_value(true)
                .multiple_occurrences(true)
                .help("Include paths matching a gitignore-style glob, or exclude them with a leading ! (repeatable, as rg -g)"),
        )
        .arg(crate::number("limit", "20", "Maximum matching lines"))
        .arg(
            Arg::new("after")
                .long("after")
                .value_name("CURSOR")
                .help("Continue after the page that printed this cursor"),
        )
        .arg(Arg::new("json").long("json").help("Print a JSON report"))
        .arg(crate::budget())
}

/// A `--path` resolved to a workspace-relative file or directory. The root is
/// the empty string.
struct Target {
    relative: String,
    dir: bool,
}

impl Target {
    fn label(&self) -> String {
        match (self.relative.as_str(), self.dir) {
            ("", _) => "./".into(),
            (relative, true) => format!("{relative}/"),
            (relative, false) => relative.into(),
        }
    }

    fn contains(&self, file: &str) -> bool {
        if self.relative.is_empty() {
            true
        } else if self.dir {
            file.strip_prefix(&self.relative)
                .is_some_and(|rest| rest.starts_with('/'))
        } else {
            file == self.relative
        }
    }
}

/// The indexed files a search reads: those under any `--path`, then filtered
/// by the globs as `rg -g` would while walking the tree.
struct Scope {
    targets: Vec<Target>,
    globs: Vec<String>,
    matcher: Option<Override>,
    notes: Vec<String>,
}

impl Scope {
    fn new(
        workspace: &Workspace,
        index: &Index,
        paths: &[&str],
        globs: &[&str],
        root_given: bool,
    ) -> Result<Self> {
        let mut targets: Vec<Target> = Vec::new();
        for given in paths {
            let target = resolve(workspace, given, root_given)?;
            if !targets.iter().any(|seen| seen.relative == target.relative) {
                targets.push(target);
            }
        }
        let matcher = if globs.is_empty() {
            None
        } else {
            let mut builder = OverrideBuilder::new(&workspace.root);
            for glob in globs {
                builder
                    .add(glob)
                    .with_context(|| format!("--glob {glob}"))?;
            }
            Some(builder.build().context("--glob")?)
        };
        let mut notes = Vec::new();
        for target in &targets {
            if !index.files.keys().any(|file| target.contains(file)) {
                notes.push(format!(
                    "[--path {} has no indexed text files]",
                    target.label()
                ));
            }
        }
        for glob in globs {
            let pattern = glob.strip_prefix('!').unwrap_or(glob);
            let single = OverrideBuilder::new(&workspace.root)
                .add(pattern)
                .and_then(|builder| builder.build())
                .with_context(|| format!("--glob {glob}"))?;
            if !index
                .files
                .keys()
                .any(|file| touches(&single, file, |found| found.is_whitelist()))
            {
                notes.push(format!("[--glob {glob} matches no indexed file]"));
            }
        }
        Ok(Self {
            targets,
            globs: globs.iter().map(|glob| (*glob).to_owned()).collect(),
            matcher,
            notes,
        })
    }

    fn narrowed(&self) -> bool {
        !self.targets.is_empty() || !self.globs.is_empty()
    }

    fn contains(&self, file: &str) -> bool {
        (self.targets.is_empty() || self.targets.iter().any(|target| target.contains(file)))
            && self
                .matcher
                .as_ref()
                .is_none_or(|matcher| !touches(matcher, file, |found| found.is_ignore()))
    }

    /// `[scope: docs/ · glob *.md · 3 of 120 indexed files]`
    fn line(&self, files: usize, indexed: usize) -> String {
        let mut parts = Vec::new();
        if !self.targets.is_empty() {
            parts.push(
                self.targets
                    .iter()
                    .map(Target::label)
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        if !self.globs.is_empty() {
            parts.push(format!("glob {}", self.globs.join(", ")));
        }
        parts.push(format!("{files} of {indexed} indexed files"));
        format!("[scope: {}]", parts.join(" · "))
    }
}

/// Whether `test` holds for the match of `file` or of any directory above it,
/// the way a directory walk meets them: an excluded directory takes its files
/// with it.
fn touches(
    matcher: &Override,
    file: &str,
    test: impl Fn(&ignore::Match<ignore::overrides::Glob<'_>>) -> bool,
) -> bool {
    file.match_indices('/')
        .any(|(end, _)| test(&matcher.matched(&file[..end], true)))
        || test(&matcher.matched(file, false))
}

/// Resolves `--path` like other path arguments: a bare path comes from the
/// current directory and falls back to the root, or the reverse with `--root`.
/// Paths that start with `./` or `../`, and absolute paths, resolve from the
/// current directory.
fn resolve(workspace: &Workspace, given: &str, root_given: bool) -> Result<Target> {
    ensure!(!given.is_empty(), "--path needs a file or directory");
    let path = Path::new(given);
    let bare = path
        .components()
        .all(|part| matches!(part, Component::Normal(_)));
    let bases = match (bare, root_given) {
        (false, _) => vec![&workspace.cwd],
        (true, false) => vec![&workspace.cwd, &workspace.root],
        (true, true) => vec![&workspace.root, &workspace.cwd],
    };
    let mut outside = false;
    for base in bases {
        let joined = lexical(&base.join(path));
        outside |= !joined.starts_with(&workspace.root);
        let Ok(metadata) = fs::metadata(&joined) else {
            continue;
        };
        // Resolve the directories above it, such as macOS's /tmp, but not a
        // linked file or directory itself.
        let real = match (joined.parent(), joined.file_name()) {
            (Some(parent), Some(name)) => parent
                .canonicalize()
                .map_or_else(|_| joined.clone(), |parent| parent.join(name)),
            _ => joined.canonicalize().unwrap_or(joined),
        };
        let Ok(relative) = real.strip_prefix(&workspace.root) else {
            outside = true;
            continue;
        };
        let relative = relative
            .components()
            .map(|part| part.as_os_str().to_str())
            .collect::<Option<Vec<_>>>()
            .with_context(|| format!("--path {given} is not UTF-8"))?
            .join("/");
        return Ok(Target {
            relative,
            dir: metadata.is_dir(),
        });
    }
    if outside {
        bail!("--path {given} is outside the workspace");
    }
    bail!("--path {given}: no such file or directory in the workspace")
}

/// `path` with `.` and `..` applied without touching the file system.
pub(crate) fn lexical(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            part => clean.push(part),
        }
    }
    clean
}

/// Where a page stopped: a hash of the search, of the file, of the file's
/// indexed content, and the line. Lowercase hex and digits only, so it survives
/// any shell and splits on whitespace.
#[derive(PartialEq, Eq)]
struct Cursor {
    search: String,
    file: String,
    digest: String,
    line: usize,
}

const CURSOR_PREFIX: &str = "f1.";

fn short_hash(parts: &[&[u8]], bytes: usize) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(bytes * 2);
    for &byte in &digest[..bytes] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

impl Cursor {
    fn at(search: &str, file: &str, digest: &str, line: usize) -> Self {
        Self {
            search: search.to_owned(),
            file: short_hash(&[file.as_bytes()], 6),
            digest: short_hash(&[digest.as_bytes()], 6),
            line,
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let (hex, line) = text.strip_prefix(CURSOR_PREFIX)?.split_once('.')?;
        let hex_ok = hex.len() == 32
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        let line_ok = !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit());
        if !hex_ok || !line_ok {
            return None;
        }
        Some(Self {
            search: hex[..8].to_owned(),
            file: hex[8..20].to_owned(),
            digest: hex[20..].to_owned(),
            line: line.parse().ok().filter(|&line| line > 0)?,
        })
    }

    fn text(&self) -> String {
        format!(
            "{CURSOR_PREFIX}{}{}{}.{}",
            self.search, self.file, self.digest, self.line
        )
    }
}

/// Lines at or below this length after trimming leading/trailing whitespace stay whole.
const LINE_BYTES: usize = 200;
/// Normal match-centered window width for long lines (parity with `search::WINDOW_SNIPPET_BYTES`).
const SNIPPET_BYTES: usize = 168;
const ELLIPSIS: &str = "…";
const ELLIPSIS_BYTES: usize = ELLIPSIS.len();

struct Hit {
    file: String,
    line: usize,
    text: String,
    match_start: usize,
    match_end: usize,
    clipped_left: bool,
    clipped_right: bool,
}

impl Hit {
    fn from_line(file: String, line_number: usize, line: &str, pos: usize, len: usize) -> Self {
        let (text, match_start, match_end, clipped_left, clipped_right) =
            window_match_line(line, pos, pos.saturating_add(len));
        Self {
            file,
            line: line_number,
            text,
            match_start,
            match_end,
            clipped_left,
            clipped_right,
        }
    }

    /// Re-windows `self.text` around the matched term to fit within `max_bytes`
    /// without duplicating edge ellipses.
    fn window_to(&self, max_bytes: usize) -> String {
        if self.text.len() <= max_bytes {
            return self.text.clone();
        }
        let mut inner = self.text.as_str();
        let mut m_start = self.match_start;
        let mut m_end = self.match_end;
        if self.clipped_left
            && let Some(rest) = inner.strip_prefix(ELLIPSIS)
        {
            inner = rest;
            m_start = m_start.saturating_sub(ELLIPSIS_BYTES);
            m_end = m_end.saturating_sub(ELLIPSIS_BYTES);
        }
        if self.clipped_right
            && let Some(rest) = inner.strip_suffix(ELLIPSIS)
        {
            inner = rest;
            m_start = m_start.min(inner.len());
            m_end = m_end.min(inner.len());
        }
        let (windowed, _, _, _, _) = window_snippet_exact(
            inner,
            m_start,
            m_end,
            self.clipped_left,
            self.clipped_right,
            max_bytes,
        );
        windowed
    }
}

/// Extracts a UTF-8-safe match-centered window from `line` around
/// `[match_start..match_end]`, trimming leading whitespace when anchored at the
/// start of the line without ever cutting into the match itself.
fn window_match_line(
    line: &str,
    match_start: usize,
    match_end: usize,
) -> (String, usize, usize, bool, bool) {
    let m_start = line.floor_char_boundary(match_start.min(line.len()));
    let m_end = line
        .ceil_char_boundary(match_end.min(line.len()))
        .max(m_start);
    let leading_ws = line.len() - line.trim_start().len();
    let lead_trim = leading_ws.min(m_start);
    let trimmed = &line[lead_trim..];
    let m_start = m_start - lead_trim;
    let m_end = m_end - lead_trim;
    let trail_end = trimmed.trim_end().len().max(m_end);
    let content = &trimmed[..trail_end];

    if content.len() <= LINE_BYTES {
        return (content.to_owned(), m_start, m_end, false, false);
    }
    let m_len = m_end.saturating_sub(m_start);
    let max_bytes = SNIPPET_BYTES.max(m_len + 2 * ELLIPSIS_BYTES);
    window_snippet_exact(content, m_start, m_end, false, false, max_bytes)
}

/// Windows `inner` around `[m_start..m_end]` so the output (including any `…`
/// edge markers) fits within `max_bytes` whenever `max_bytes >= ELLIPSIS_BYTES`,
/// keeping the full matched span visible whenever `max_bytes >= m_end - m_start`.
fn window_snippet_exact(
    inner: &str,
    m_start: usize,
    m_end: usize,
    force_left: bool,
    force_right: bool,
    max_bytes: usize,
) -> (String, usize, usize, bool, bool) {
    let m_start = inner.floor_char_boundary(m_start.min(inner.len()));
    let m_end = inner
        .ceil_char_boundary(m_end.min(inner.len()))
        .max(m_start);
    let m_len = m_end - m_start;

    let full_len = inner.len()
        + if force_left { ELLIPSIS_BYTES } else { 0 }
        + if force_right { ELLIPSIS_BYTES } else { 0 };
    if full_len <= max_bytes {
        let mut out = String::with_capacity(full_len);
        let shift = if force_left {
            out.push_str(ELLIPSIS);
            ELLIPSIS_BYTES
        } else {
            0
        };
        out.push_str(inner);
        if force_right {
            out.push_str(ELLIPSIS);
        }
        return (out, m_start + shift, m_end + shift, force_left, force_right);
    }

    if max_bytes < m_len {
        if max_bytes < ELLIPSIS_BYTES {
            return (ELLIPSIS.to_owned(), 0, 0, true, true);
        }
        let body_cap = max_bytes - ELLIPSIS_BYTES;
        let end = inner.floor_char_boundary((m_start + body_cap).min(inner.len()));
        if end <= m_start {
            return (ELLIPSIS.to_owned(), 0, 0, true, true);
        }
        let mut out = String::with_capacity((end - m_start) + ELLIPSIS_BYTES);
        out.push_str(&inner[m_start..end]);
        out.push_str(ELLIPSIS);
        return (out, 0, end - m_start, force_left || m_start > 0, true);
    }

    // Anchor at byte 0 only when not already clipped on the left and the match
    // sits comfortably inside the leading window with room after it.
    let anchor_start = !force_left
        && (m_start == 0 || m_end.saturating_add(24) <= max_bytes.saturating_sub(ELLIPSIS_BYTES));

    let (mut start, mut end) = if anchor_start {
        let content_cap = max_bytes.saturating_sub(ELLIPSIS_BYTES);
        if content_cap >= m_end {
            let end = inner.floor_char_boundary(content_cap.min(inner.len()));
            (0, end.max(m_end))
        } else {
            (0, m_end)
        }
    } else {
        let want_left = force_left || m_start > 0;
        let want_right = force_right || m_end < inner.len();
        let edge_cost = if want_left { ELLIPSIS_BYTES } else { 0 }
            + if want_right { ELLIPSIS_BYTES } else { 0 };

        if m_len + edge_cost <= max_bytes {
            let content_cap = max_bytes - edge_cost;
            let slack = content_cap - m_len;
            let before = slack * 2 / 5;
            let after = slack - before;
            let (mut raw_start, mut raw_end) = if m_start <= before {
                (0, (m_end + after + (before - m_start)).min(inner.len()))
            } else if m_end + after >= inner.len() {
                let overflow = m_end + after - inner.len();
                (m_start.saturating_sub(before + overflow), inner.len())
            } else {
                (m_start - before, m_end + after)
            };
            if want_left && !force_left && raw_start == 0 && raw_end < inner.len() {
                raw_end = (raw_end + ELLIPSIS_BYTES).min(inner.len());
            }
            if want_right && !force_right && raw_end == inner.len() && raw_start > 0 {
                raw_start = raw_start.saturating_sub(ELLIPSIS_BYTES);
            }
            let start = inner.ceil_char_boundary(raw_start).min(m_start);
            let end = inner
                .floor_char_boundary(raw_end)
                .max(m_end)
                .min(inner.len());
            (start, end)
        } else {
            // Tight budget: full match fits, at most one ellipsis fits.
            let mut out = String::with_capacity(max_bytes);
            let (left, right) = if m_len + ELLIPSIS_BYTES <= max_bytes {
                if want_right {
                    (false, true)
                } else if want_left {
                    (true, false)
                } else {
                    (false, false)
                }
            } else {
                (false, false)
            };
            let shift = if left {
                out.push_str(ELLIPSIS);
                ELLIPSIS_BYTES
            } else {
                0
            };
            out.push_str(&inner[m_start..m_end]);
            if right {
                out.push_str(ELLIPSIS);
            }
            return (
                out,
                shift,
                shift + m_len,
                force_left || m_start > 0,
                force_right || m_end < inner.len(),
            );
        }
    };

    // Never spend a 3-byte `…` just to elide <= 3 real bytes at an unforced edge.
    if !force_left && start > 0 && start <= ELLIPSIS_BYTES {
        let right_bytes = if force_right || end < inner.len() {
            ELLIPSIS_BYTES
        } else {
            0
        };
        if end + right_bytes <= max_bytes {
            start = 0;
        }
    }
    if !force_right && end < inner.len() && inner.len() - end <= ELLIPSIS_BYTES {
        let left_bytes = if force_left || start > 0 {
            ELLIPSIS_BYTES
        } else {
            0
        };
        if (inner.len() - start) + left_bytes <= max_bytes {
            end = inner.len();
        }
    }

    let need_left = force_left || start > 0;
    let need_right = force_right || end < inner.len();
    let final_len = (end - start)
        + if need_left { ELLIPSIS_BYTES } else { 0 }
        + if need_right { ELLIPSIS_BYTES } else { 0 };
    let emit_right = need_right && final_len <= max_bytes;
    let emit_left = need_left
        && (end - start) + if emit_right { ELLIPSIS_BYTES } else { 0 } + ELLIPSIS_BYTES
            <= max_bytes;

    let mut out = String::with_capacity(
        (end - start)
            + if emit_left { ELLIPSIS_BYTES } else { 0 }
            + if emit_right { ELLIPSIS_BYTES } else { 0 },
    );
    let shift = if emit_left {
        out.push_str(ELLIPSIS);
        ELLIPSIS_BYTES
    } else {
        0
    };
    out.push_str(&inner[start..end]);
    if emit_right {
        out.push_str(ELLIPSIS);
    }
    (
        out,
        (m_start - start) + shift,
        (m_end - start) + shift,
        need_left,
        need_right,
    )
}

/// The search as its options gave it, for the next page's command.
struct Request<'a> {
    term: &'a str,
    insensitive: bool,
    paths: Vec<String>,
    globs: &'a [&'a str],
    limit: usize,
    limit_given: bool,
    budget: usize,
    budget_given: bool,
    json: bool,
    after_given: bool,
    justless: String,
}

impl Request<'_> {
    fn next(&self, cursor: &str) -> String {
        let mut words = vec![self.justless.clone(), "find".into()];
        if self.insensitive {
            words.push("-i".into());
        }
        // A value joined to its option by `=` is never read as an option, even
        // when it starts with `-`.
        words.extend(
            self.paths
                .iter()
                .map(|path| format!("--path={}", shell_quote(Path::new(path)))),
        );
        words.extend(
            self.globs
                .iter()
                .map(|glob| shell_quote(Path::new(&format!("--glob={glob}")))),
        );
        if self.limit_given {
            words.push(format!("--limit={}", self.limit));
        }
        if self.budget_given {
            words.push(format!("--budget={}", self.budget));
        }
        if self.json {
            words.push("--json".into());
        }
        words.push(format!("--after={cursor}"));
        words.push(shell_text(self.term));
        words.join(" ")
    }
}

/// What the scan found: every match in scope is counted, and the page keeps
/// the first `limit` after the cursor.
struct Found {
    total: usize,
    files: usize,
    before: usize,
    page: Vec<Hit>,
}

impl Found {
    fn remaining(&self) -> usize {
        self.total - self.before
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

pub fn run(workspace: &Workspace, matches: &ArgMatches, root_given: bool) -> Result<()> {
    let term = matches.value_of("text").unwrap();
    ensure!(
        !term.is_empty() && !term.contains('\n'),
        "enter a non-empty, single-line search term"
    );
    let insensitive = matches.is_present("ignore-case");
    let paths: Vec<&str> = matches.values_of("path").into_iter().flatten().collect();
    let globs: Vec<&str> = matches.values_of("glob").into_iter().flatten().collect();
    let limit = crate::read_num(matches, "limit", 1)?;
    let budget = crate::context_budget(workspace, matches)?;
    let index = Index::open(workspace)?;
    let scope = Scope::new(workspace, &index, &paths, &globs, root_given)?;
    let folded = if insensitive {
        term.to_ascii_lowercase()
    } else {
        term.to_owned()
    };
    let search = {
        let mut parts: Vec<&[u8]> = vec![b"find", folded.as_bytes()];
        if insensitive {
            parts.push(b"-i");
        }
        let labels: Vec<String> = scope.targets.iter().map(Target::label).collect();
        for label in &labels {
            parts.extend([b"--path".as_slice(), label.as_bytes()]);
        }
        for glob in &globs {
            parts.extend([b"--glob".as_slice(), glob.as_bytes()]);
        }
        short_hash(&parts, 4)
    };
    let in_scope: Vec<&String> = index
        .files
        .keys()
        .filter(|file| scope.contains(file))
        .collect();
    let after = match matches.value_of("after") {
        None => None,
        Some(text) => {
            let cursor = Cursor::parse(text).ok_or_else(|| {
                anyhow!("invalid --after cursor {text}; pass the cursor a find page printed")
            })?;
            ensure!(
                cursor.search == search,
                "this --after cursor belongs to another search; repeat that search's text, -i, --path and --glob, or drop --after"
            );
            let file = in_scope
                .iter()
                .find(|file| short_hash(&[file.as_bytes()], 6) == cursor.file)
                .ok_or_else(|| {
                    anyhow!(
                        "the file that page stopped in is no longer indexed; run the search again without --after"
                    )
                })?;
            ensure!(
                short_hash(&[index.files[*file].as_bytes()], 6) == cursor.digest,
                "{} changed since that page; run the search again without --after",
                display_path(file)
            );
            Some((file.as_str(), cursor.line))
        }
    };
    let found = scan(
        workspace,
        &index,
        &in_scope,
        &folded,
        insensitive,
        after,
        limit,
    );
    let invocation = workspace.invocation();
    let request = Request {
        term,
        insensitive,
        paths: scope
            .targets
            .iter()
            .map(|target| match target.relative.as_str() {
                // `.` resolves from the current directory, so a command that
                // may run elsewhere names the root itself.
                "" if invocation.files == "justless" => ".".into(),
                "" => workspace.root.to_string_lossy().into_owned(),
                relative => relative.to_owned(),
            })
            .collect(),
        globs: &globs,
        limit,
        limit_given: matches.occurrences_of("limit") > 0,
        budget,
        budget_given: matches.is_present("budget"),
        json: matches.is_present("json"),
        after_given: after.is_some(),
        // Paths in the command are root-relative, which a bare path reads only
        // from the root or with `--root`.
        justless: if scope.targets.is_empty() {
            invocation.command
        } else {
            invocation.files
        },
    };
    let mut notes = scope.notes.clone();
    if in_scope.is_empty() {
        notes.push("No indexed file is in scope.".into());
    } else if found.total == 0 {
        notes.push("No matching indexed text.".into());
        let note = if scope.narrowed() {
            index.skipped_note_where(false, |file| scope.contains(file))
        } else {
            index.skipped_note(false)
        };
        notes.extend(note);
    }
    let page = Page {
        request: &request,
        found: &found,
        search: &search,
        index: &index,
    };
    let scope_line = scope
        .narrowed()
        .then(|| scope.line(in_scope.len(), index.files.len()));
    let output = if request.json {
        let scope_value = scope.narrowed().then(|| {
            json!({
                "paths": scope.targets.iter().map(Target::label).collect::<Vec<_>>(),
                "globs": globs,
                "files": in_scope.len(),
                "indexed": index.files.len(),
            })
        });
        page.json(scope_value, &notes)?
    } else {
        let mut header = String::new();
        for line in scope_line.iter().chain(&notes) {
            header.push_str(line);
            header.push('\n');
        }
        page.text(header)
    };
    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

/// Zero-allocation ASCII case-insensitive substring search (`needle_lower` must
/// already have its ASCII letters lowercased). Matches `haystack.to_ascii_lowercase().find(needle_lower)`
/// identically for all UTF-8 strings because ASCII bytes `0x00..=0x7f` never occur inside
/// multi-byte UTF-8 sequences.
fn find_ascii_ci(haystack: &str, needle_lower: &str) -> Option<usize> {
    let n = needle_lower.as_bytes();
    let h = haystack.as_bytes();
    if n.is_empty() {
        return Some(0);
    }
    if n.len() > h.len() {
        return None;
    }
    let first = n[0];
    let first_alt = if first.is_ascii_lowercase() {
        first.to_ascii_uppercase()
    } else {
        first
    };
    let tail = &n[1..];
    let limit = h.len() - n.len();
    let mut i = 0;
    while i <= limit {
        let b = h[i];
        if (b == first || b == first_alt)
            && h[i + 1..i + n.len()]
                .iter()
                .zip(tail)
                .all(|(hb, &nb)| hb.to_ascii_lowercase() == nb)
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Reads every candidate file in index order, counting all matches in scope
/// and keeping up to `limit` after the cursor. One match per line.
fn scan(
    workspace: &Workspace,
    index: &Index,
    files: &[&String],
    term: &str,
    insensitive: bool,
    after: Option<(&str, usize)>,
    limit: usize,
) -> Found {
    let fingerprint = Fingerprint::new(term);
    let mut found = Found {
        total: 0,
        files: 0,
        before: 0,
        page: Vec::with_capacity(limit.min(64)),
    };
    let mut buf = String::new();
    for file in files {
        if !index
            .fingerprints
            .get(*file)
            .is_some_and(|candidate| candidate.includes(fingerprint))
        {
            continue;
        }
        let Ok(path) = workspace.source_path(file) else {
            continue;
        };
        buf.clear();
        let Ok(mut handle) = fs::File::open(path) else {
            continue;
        };
        if handle.read_to_string(&mut buf).is_err() {
            continue;
        }
        let text = buf.as_str();
        let bytes = text.as_bytes();
        let file_before = after.is_some_and(|(stop, _)| file.as_str() < stop);
        let file_is_stop = after.is_some_and(|(stop, _)| file.as_str() == stop);
        let stop_line = after.map_or(0, |(_, l)| l);
        let mut any = false;
        let mut cursor = 0usize;
        let mut line_no = 1usize;
        let mut counted_to = 0usize;

        while cursor < bytes.len() {
            let rel = if insensitive {
                find_ascii_ci(&text[cursor..], term)
            } else {
                text[cursor..].find(term)
            };
            let Some(rel) = rel else {
                break;
            };
            let abs_pos = cursor + rel;
            let line_start = bytes[cursor..abs_pos]
                .iter()
                .rposition(|&b| b == b'\n')
                .map_or(cursor, |off| cursor + off + 1);
            let (line_end, next_cursor) = match bytes[abs_pos..].iter().position(|&b| b == b'\n') {
                Some(rel_nl) => {
                    let nl = abs_pos + rel_nl;
                    let end = if nl > line_start && bytes[nl - 1] == b'\r' {
                        nl - 1
                    } else {
                        nl
                    };
                    (end, nl + 1)
                }
                None => (bytes.len(), bytes.len()),
            };
            // Match str::lines() semantics: `\r` immediately before `\n` is part of the
            // line terminator, not line content.
            if abs_pos + term.len() > line_end {
                cursor = next_cursor;
                continue;
            }
            any = true;
            found.total += 1;
            if file_before {
                found.before += 1;
                cursor = next_cursor;
                continue;
            }
            if file_is_stop || found.page.len() < limit {
                line_no += bytes[counted_to..line_start]
                    .iter()
                    .filter(|&&b| b == b'\n')
                    .count();
                counted_to = line_start;
                if file_is_stop && line_no <= stop_line {
                    found.before += 1;
                } else if found.page.len() < limit {
                    found.page.push(Hit::from_line(
                        (*file).clone(),
                        line_no,
                        &text[line_start..line_end],
                        abs_pos - line_start,
                        term.len(),
                    ));
                }
            }
            cursor = next_cursor;
        }
        found.files += usize::from(any);
    }
    found
}

/// Why a page ended before the last match.
#[derive(Clone, Copy)]
enum Cut {
    Limit,
    Budget,
}

struct Page<'a> {
    request: &'a Request<'a>,
    found: &'a Found,
    search: &'a str,
    index: &'a Index,
}

impl Page<'_> {
    /// The bound that cut a page of `shown` matches, if more follow.
    fn cut(&self, shown: usize) -> Option<Cut> {
        let cut = if shown == self.request.limit {
            Cut::Limit
        } else {
            Cut::Budget
        };
        (shown < self.found.remaining()).then_some(cut)
    }

    fn cursor(&self, shown: usize) -> String {
        let hit = &self.found.page[shown - 1];
        Cursor::at(
            self.search,
            &hit.file,
            &self.index.files[&hit.file],
            hit.line,
        )
        .text()
    }

    fn counts(&self) -> String {
        format!(
            "{} in {}",
            plural(self.found.total, "match", "matches"),
            plural(self.found.files, "file", "files")
        )
    }

    /// The lines after `shown` matches: which bound cut them and the command
    /// for the next page, or the end of a paged search.
    fn footer(&self, shown: usize) -> String {
        let last = self.found.before + shown;
        let limit = self.request.limit;
        let counts = self.counts();
        let notice = match self.cut(shown) {
            Some(Cut::Limit) => {
                format!("[--limit {limit} reached after {last} of {counts}]")
            }
            Some(Cut::Budget) => {
                format!("[budget reached after {last} of {counts}, before --limit {limit}]")
            }
            None if self.request.after_given => return format!("[end of {counts}]\n"),
            None => return String::new(),
        };
        format!(
            "{notice}\nNext page: {}\n",
            self.request.next(&self.cursor(shown))
        )
    }

    /// Whole match lines while they fit with the footer, hoisting shared deep
    /// directory prefixes, grouping repeated same-file hits, and collapsing
    /// identical lines within a file while keeping `Cursor` anchored to the
    /// exact last represented `Hit` in `found.page`. The first match is always
    /// shown, centered around the matched term when it would not fit.
    fn text(&self, header: String) -> String {
        let total = self.found.page.len();
        if total == 0 {
            return format!("{header}{}", self.footer(0));
        }
        let budget = self.request.budget;
        let full = self.render_prefix(&header, total);
        if full.len() <= budget {
            return full;
        }
        let mut lo = 1usize;
        let mut hi = total - 1;
        let mut best: Option<(usize, String)> = None;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let candidate = self.render_prefix(&header, mid);
            if candidate.len() <= budget {
                best = Some((mid, candidate));
                lo = mid + 1;
            } else if mid == 1 {
                break;
            } else {
                hi = mid - 1;
            }
        }
        if let Some((mut shown, mut output)) = best {
            // A file crossing the grouping threshold can briefly dip back under
            // budget; probe a few hits ahead to catch the largest fitting prefix.
            let mut probe = shown + 1;
            while probe < total && probe <= shown + 4 {
                let candidate = self.render_prefix(&header, probe);
                if candidate.len() <= budget {
                    shown = probe;
                    output = candidate;
                }
                probe += 1;
            }
            return output;
        }
        // Even the first hit alone exceeds budget: window it around the match
        // so the search term remains visible within the remaining room.
        let hit = &self.found.page[0];
        let footer = self.footer(1);
        let prefix = format!("{}:{}: ", display_path(&hit.file), hit.line);
        let room = budget.saturating_sub(header.len() + footer.len());
        let keep = room.saturating_sub(prefix.len() + 1);
        let snippet = hit.window_to(keep);
        format!("{header}{prefix}{snippet}\n{footer}")
    }

    fn render_prefix(&self, header: &str, shown: usize) -> String {
        let body = render_body(&self.found.page[..shown]);
        let footer = self.footer(shown);
        let mut out = String::with_capacity(header.len() + body.len() + footer.len());
        out.push_str(header);
        out.push_str(&body);
        out.push_str(&footer);
        out
    }

    fn json(&self, scope: Option<Value>, notes: &[String]) -> Result<String> {
        // One byte stays free for the closing newline.
        let budget = self.request.budget.saturating_sub(1);
        let base = |shown: usize| {
            let mut value = Map::new();
            value.insert("total".into(), json!(self.found.total));
            value.insert("files".into(), json!(self.found.files));
            value.insert("first".into(), json!(self.found.before + 1));
            if let Some(scope) = &scope {
                value.insert("scope".into(), scope.clone());
            }
            if !notes.is_empty() {
                value.insert("notes".into(), json!(notes));
            }
            value.insert("matches".into(), json!([]));
            if let Some(cut) = self.cut(shown) {
                let cursor = self.cursor(shown);
                let cut = match cut {
                    Cut::Limit => "limit",
                    Cut::Budget => "budget",
                };
                value.insert("cut".into(), json!(cut));
                value.insert("next".into(), json!(self.request.next(&cursor)));
                value.insert("after".into(), json!(cursor));
            }
            value
        };
        let hit_json =
            |hit: &Hit, text: &str| json!({"file": hit.file, "line": hit.line, "text": text});
        let mut kept: Vec<Value> = Vec::new();
        let mut used = 0;
        for hit in &self.found.page {
            let item = hit_json(hit, &hit.text);
            let size = serde_json::to_string(&item)?.len() + usize::from(!kept.is_empty());
            let frame = serde_json::to_string(&base(kept.len() + 1))?.len();
            if frame + used + size <= budget {
                used += size;
                kept.push(item);
                continue;
            }
            if kept.is_empty() {
                // Shorten the first match's text around the match until the page fits.
                let mut cap = hit.text.len().saturating_sub(1);
                loop {
                    let text = hit.window_to(cap);
                    let item = hit_json(hit, &text);
                    let size = serde_json::to_string(&item)?.len();
                    if cap == 0 || frame + size <= budget {
                        kept.push(item);
                        break;
                    }
                    let over = (frame + size - budget).min(cap);
                    cap -= over;
                }
            }
            break;
        }
        let mut value = base(kept.len());
        value.insert("matches".into(), Value::Array(kept));
        Ok(serde_json::to_string(&Value::Object(value))? + "\n")
    }
}

struct FileGroup<'a> {
    file: &'a str,
    hits: &'a [Hit],
}

struct CollapsedRow<'a> {
    lines: Vec<usize>,
    key: Cow<'a, str>,
    first_raw: &'a str,
    raw_identical: bool,
}

enum BodyPrefix<'a> {
    None,
    Full(&'a str),
    Partial(&'a str),
}

fn group_by_file(hits: &[Hit]) -> Vec<FileGroup<'_>> {
    let mut groups = Vec::with_capacity(hits.len().min(32));
    let mut start = 0;
    while start < hits.len() {
        let file = hits[start].file.as_str();
        let mut end = start + 1;
        while end < hits.len() && hits[end].file == file {
            end += 1;
        }
        groups.push(FileGroup {
            file,
            hits: &hits[start..end],
        });
        start = end;
    }
    groups
}

fn real_dir_depth(prefix: &str) -> usize {
    prefix
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .count()
}

fn common_dir_prefix_iter<'a>(mut paths: impl Iterator<Item = &'a str>) -> &'a str {
    fn dir_of(path: &str) -> &str {
        path.rfind('/').map_or("", |i| &path[..=i])
    }
    let Some(first) = paths.next() else {
        return "";
    };
    let mut common = dir_of(first);
    for path in paths {
        let dir = dir_of(path);
        let shared = common
            .bytes()
            .zip(dir.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        let cut = common[..common.floor_char_boundary(shared)]
            .rfind('/')
            .map_or(0, |i| i + 1);
        common = &common[..cut];
        if common.is_empty() {
            break;
        }
    }
    common
}

#[cfg(test)]
fn common_dir_prefix(paths: &[&str]) -> String {
    common_dir_prefix_iter(paths.iter().copied()).to_owned()
}

fn hoistable_prefix<'a>(groups: &[FileGroup<'a>]) -> Option<&'a str> {
    if groups.len() < 2 {
        return None;
    }
    if groups
        .iter()
        .any(|group| display_path(group.file) != group.file)
    {
        return None;
    }
    let common = common_dir_prefix_iter(groups.iter().map(|group| group.file));
    if real_dir_depth(common) < 2 {
        return None;
    }
    Some(common)
}

/// When full hoisting does not apply (for example because outlier files sit outside a
/// deep monorepo subtree), find a 3+-segment directory prefix shared by at least 3 files
/// whose `base: … = {prefix}` hoisting saves bytes cleanly.
fn partial_hoistable_prefix<'a>(groups: &[FileGroup<'a>]) -> Option<&'a str> {
    if groups.len() < 3 {
        return None;
    }
    let mut counts: HashMap<&'a str, usize> = HashMap::with_capacity(groups.len());
    for group in groups {
        if display_path(group.file) != group.file {
            continue;
        }
        let mut depth = 0usize;
        let mut seg_start = 0usize;
        for (i, _) in group.file.match_indices('/') {
            let seg = &group.file[seg_start..i];
            if !seg.is_empty() && seg != "." {
                depth += 1;
            }
            seg_start = i + 1;
            if depth >= 3 && i + 1 >= 16 {
                *counts.entry(&group.file[..=i]).or_default() += 1;
            }
        }
    }
    let marker_len = "…/".len();
    let header_fixed = "base: … = \n".len();
    let mut best: Option<(&'a str, usize)> = None;
    for (prefix, count) in counts {
        if count < 3 {
            continue;
        }
        let gross = count.saturating_mul(prefix.len().saturating_sub(marker_len));
        let cost = header_fixed + prefix.len();
        let savings = gross.saturating_sub(cost);
        if savings == 0 {
            continue;
        }
        let replace = match best {
            None => true,
            Some((best_prefix, best_savings)) => {
                savings > best_savings
                    || (savings == best_savings && prefix.len() > best_prefix.len())
                    || (savings == best_savings
                        && prefix.len() == best_prefix.len()
                        && prefix < best_prefix)
            }
        };
        if replace {
            best = Some((prefix, savings));
        }
    }
    best.map(|(prefix, _)| prefix)
}

/// Collapse runs of unquoted ASCII horizontal whitespace (`' '` / `'\t'`) into a single
/// space with a zero-allocation fast path when no tabs or consecutive spaces appear.
fn normalize_ws(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut prev_space = false;
    let mut needs_norm = false;
    for &b in bytes {
        if b == b'\t' || (b == b' ' && prev_space) {
            needs_norm = true;
            break;
        }
        prev_space = b == b' ';
    }
    if !needs_norm {
        return Cow::Borrowed(text);
    }
    let mut out = String::new();
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut copy_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == q {
                quote = None;
            }
            i += 1;
        } else if b == b' ' || b == b'\t' {
            let ws_start = i;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            if i - ws_start > 1 || b == b'\t' {
                if out.is_empty() {
                    out.reserve(text.len());
                }
                out.push_str(&text[copy_start..ws_start]);
                out.push(' ');
                copy_start = i;
            }
        } else {
            if b == b'"' || b == b'`' {
                quote = Some(b);
            } else if b == b'\'' {
                let apostrophe =
                    i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
                let lifetime = i > 0
                    && matches!(bytes[i - 1], b'<' | b'&')
                    && bytes.get(i + 2) != Some(&b'\'');
                if !apostrophe && !lifetime {
                    quote = Some(b);
                }
            }
            i += 1;
        }
    }
    if copy_start == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[copy_start..]);
    Cow::Owned(out)
}

fn collapse_file_hits(hits: &[Hit]) -> Vec<CollapsedRow<'_>> {
    let mut rows: Vec<CollapsedRow<'_>> = Vec::with_capacity(hits.len());
    if hits.len() <= 8 {
        for hit in hits {
            let raw = hit.text.as_str();
            let key = normalize_ws(raw);
            if let Some(row) = rows.iter_mut().find(|r| r.key.as_ref() == key.as_ref()) {
                row.lines.push(hit.line);
                if raw != row.first_raw {
                    row.raw_identical = false;
                }
            } else {
                rows.push(CollapsedRow {
                    lines: vec![hit.line],
                    key,
                    first_raw: raw,
                    raw_identical: true,
                });
            }
        }
        return rows;
    }
    let mut index_by_text: HashMap<Cow<'_, str>, usize> = HashMap::with_capacity(hits.len());
    for hit in hits {
        let raw = hit.text.as_str();
        let key = normalize_ws(raw);
        if let Some(&idx) = index_by_text.get(key.as_ref()) {
            let row = &mut rows[idx];
            row.lines.push(hit.line);
            if raw != row.first_raw {
                row.raw_identical = false;
            }
        } else {
            let idx = rows.len();
            index_by_text.insert(key.clone(), idx);
            rows.push(CollapsedRow {
                lines: vec![hit.line],
                key,
                first_raw: raw,
                raw_identical: true,
            });
        }
    }
    rows
}

fn write_row(out: &mut String, row: &CollapsedRow<'_>) {
    if row.lines.len() == 1 {
        let _ = write!(out, "{}: {}", row.lines[0], row.first_raw);
        return;
    }
    let mut first_piece = true;
    let mut i = 0usize;
    while i < row.lines.len() {
        let start = row.lines[i];
        let mut end = start;
        let mut j = i + 1;
        while j < row.lines.len() && row.lines[j] == end + 1 {
            end = row.lines[j];
            j += 1;
        }
        if !first_piece {
            out.push_str(", ");
        }
        first_piece = false;
        let run_len = j - i;
        if run_len >= 3 {
            let _ = write!(out, "{start}-{end}");
        } else if run_len == 2 {
            let _ = write!(out, "{start}, {end}");
        } else {
            let _ = write!(out, "{start}");
        }
        i = j;
    }
    let snippet = if row.raw_identical {
        row.first_raw
    } else {
        row.key.as_ref()
    };
    let _ = write!(out, " ({}×): {snippet}", row.lines.len());
}

fn render_file_group(out: &mut String, rel_path: &str, group: &FileGroup<'_>) {
    if let [hit] = group.hits {
        let _ = writeln!(out, "{rel_path}:{}: {}", hit.line, hit.text);
        return;
    }
    let rows = collapse_file_hits(group.hits);
    if rows.len() == 1 {
        out.push_str(rel_path);
        out.push(':');
        write_row(out, &rows[0]);
        out.push('\n');
        return;
    }
    let path_len = rel_path.len();
    let row_count = rows.len();
    let flat_overhead = row_count * (path_len + 1);
    let grouped_overhead = path_len + 2 + row_count * 2;
    let savings = flat_overhead.saturating_sub(grouped_overhead);
    let has_collapsed = rows.iter().any(|row| row.lines.len() >= 2);
    let use_group = savings > 0 && (group.hits.len() >= 4 || savings >= 24 || has_collapsed);
    if use_group {
        let _ = writeln!(out, "{rel_path}:");
        for row in &rows {
            out.push_str("  ");
            write_row(out, row);
            out.push('\n');
        }
    } else {
        for row in &rows {
            out.push_str(rel_path);
            out.push(':');
            write_row(out, row);
            out.push('\n');
        }
    }
}

fn render_groups(groups: &[FileGroup<'_>], prefix: BodyPrefix<'_>) -> String {
    let est: usize = groups.iter().map(|g| g.hits.len() * 64 + 32).sum();
    let mut out = String::with_capacity(est);
    match prefix {
        BodyPrefix::None => {}
        BodyPrefix::Full(base) => {
            let _ = writeln!(out, "base: {base}");
        }
        BodyPrefix::Partial(base) => {
            let _ = writeln!(out, "base: … = {base}");
        }
    }
    let mut partial_buf = String::new();
    for group in groups {
        match prefix {
            BodyPrefix::Full(base) => {
                let rel = group.file.strip_prefix(base).unwrap_or(group.file);
                render_file_group(&mut out, rel, group);
            }
            BodyPrefix::Partial(base) => {
                if display_path(group.file) == group.file
                    && let Some(rel) = group.file.strip_prefix(base)
                {
                    partial_buf.clear();
                    partial_buf.push_str("…/");
                    partial_buf.push_str(rel);
                    render_file_group(&mut out, &partial_buf, group);
                } else {
                    let shown_path = display_path(group.file);
                    render_file_group(&mut out, &shown_path, group);
                }
            }
            BodyPrefix::None => {
                let shown_path = display_path(group.file);
                render_file_group(&mut out, &shown_path, group);
            }
        }
    }
    out
}

fn render_body(hits: &[Hit]) -> String {
    let groups = group_by_file(hits);
    let plain = render_groups(&groups, BodyPrefix::None);
    if let Some(prefix) = hoistable_prefix(&groups) {
        let hoisted = render_groups(&groups, BodyPrefix::Full(prefix));
        if hoisted.len() < plain.len() {
            return hoisted;
        }
    } else if let Some(prefix) = partial_hoistable_prefix(&groups) {
        let hoisted = render_groups(&groups, BodyPrefix::Partial(prefix));
        if hoisted.len() < plain.len() {
            return hoisted;
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_reject_other_shapes() {
        let cursor = Cursor::at("0123abcd", "src/a.rs", "digest", 42);
        let text = cursor.text();
        assert!(
            text.starts_with("f1.0123abcd") && text.ends_with(".42"),
            "{text}"
        );
        assert!(Cursor::parse(&text) == Some(cursor));
        for bad in [
            "f1.nonsense",
            "f2.0123abcd0123456789ab0123456789ab.4",
            "f1.0123ABCD0123456789ab0123456789ab.4",
            "f1.0123abcd0123456789ab0123456789ab.0",
            "f1.0123abcd0123456789ab0123456789ab.",
            "f1.0123abcd0123456789ab0123456789a.4",
        ] {
            assert!(Cursor::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn targets_contain_their_files_only() {
        let dir = Target {
            relative: "src".into(),
            dir: true,
        };
        assert!(dir.contains("src/a.rs") && !dir.contains("srcs/a.rs") && !dir.contains("src"));
        assert_eq!(dir.label(), "src/");
        let root = Target {
            relative: String::new(),
            dir: true,
        };
        assert!(root.contains("a.rs") && root.label() == "./");
    }

    fn temp_workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace, Index) {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join(".justless.toml"), "").unwrap();
        for (rel, content) in files {
            let path = dir.path().join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
        let ws = Workspace::discover(Some(dir.path())).unwrap();
        let index = Index::open(&ws).unwrap();
        (dir, ws, index)
    }

    fn test_request<'a>(term: &'a str, limit: usize, budget: usize) -> Request<'a> {
        Request {
            term,
            insensitive: false,
            paths: Vec::new(),
            globs: &[],
            limit,
            limit_given: false,
            budget,
            budget_given: true,
            json: false,
            after_given: false,
            justless: "justless".into(),
        }
    }

    /// Naive pre-upgrade flat text rendering for exact byte-reduction comparisons.
    fn baseline_flat(hits: &[Hit]) -> String {
        let mut out = String::new();
        for hit in hits {
            let _ = writeln!(
                out,
                "{}:{}: {}",
                display_path(&hit.file),
                hit.line,
                hit.text
            );
        }
        out
    }

    #[test]
    fn common_dir_prefix_and_depth_handle_shallow_deep_and_unicode() {
        assert_eq!(
            common_dir_prefix(&["packages/billing/src/a.rs", "packages/billing/src/b.rs"]),
            "packages/billing/src/"
        );
        assert_eq!(real_dir_depth("packages/billing/src/"), 3);
        assert_eq!(real_dir_depth("src/"), 1);
        assert_eq!(real_dir_depth("./src/"), 1);
        assert_eq!(common_dir_prefix(&["dir/αβ/a.rs", "dir/αγ/b.rs"]), "dir/");

        // Shallow 1-segment prefixes stay inline.
        let shallow = [
            Hit::from_line("src/a.rs".into(), 1, "needle", 0, 6),
            Hit::from_line("src/b.rs".into(), 1, "needle", 0, 6),
        ];
        assert!(!render_body(&shallow).starts_with("base: "));

        // Tiny 2-segment prefix (`a/b/`) across only 2 files would cost more with `base:`,
        // so hoisting is skipped; across 4 files it strictly saves bytes and hoists.
        let tiny_two = [
            Hit::from_line("a/b/c.rs".into(), 1, "needle", 0, 6),
            Hit::from_line("a/b/d.rs".into(), 1, "needle", 0, 6),
        ];
        assert!(!render_body(&tiny_two).starts_with("base: "));

        let tiny_four = [
            Hit::from_line("a/b/c1.rs".into(), 1, "needle", 0, 6),
            Hit::from_line("a/b/c2.rs".into(), 1, "needle", 0, 6),
            Hit::from_line("a/b/c3.rs".into(), 1, "needle", 0, 6),
            Hit::from_line("a/b/c4.rs".into(), 1, "needle", 0, 6),
        ];
        let rendered_four = render_body(&tiny_four);
        assert!(rendered_four.starts_with("base: a/b/\n"), "{rendered_four}");
        assert!(rendered_four.len() < baseline_flat(&tiny_four).len());

        // Quoted filenames with control chars skip hoisting safely.
        let quoted = [
            Hit::from_line("packages/billing/a\nb.rs".into(), 1, "needle", 0, 6),
            Hit::from_line("packages/billing/c.rs".into(), 1, "needle", 0, 6),
        ];
        let rendered_quoted = render_body(&quoted);
        assert!(!rendered_quoted.starts_with("base: "), "{rendered_quoted}");
        assert!(rendered_quoted.contains("\"packages/billing/a\\nb.rs\":1: needle\n"));
    }

    #[test]
    fn deep_multi_file_page_hoists_prefix_and_reduces_bytes() {
        let base = "packages/billing/services/checkout/src";
        let files: Vec<(String, String)> = (1..=8)
            .map(|i| {
                let path = format!("{base}/handlers/step_{i:02}.rs");
                let body = if i == 3 {
                    (1..=5)
                        .map(|n| format!("    let x_{n} = retry_budget({n});\n"))
                        .collect()
                } else {
                    format!("pub fn handle_{i}() {{ retry_budget({i}); }}\n")
                };
                (path, body)
            })
            .collect();
        let file_refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        let (_dir, ws, index) = temp_workspace(&file_refs);
        let in_scope: Vec<&String> = index.files.keys().collect();
        let found = scan(&ws, &index, &in_scope, "retry_budget", false, None, 50);
        assert_eq!(found.total, 12);
        assert_eq!(found.files, 8);

        let request = test_request("retry_budget", 50, 4096);
        let page = Page {
            request: &request,
            found: &found,
            search: "0123abcd",
            index: &index,
        };
        let compact = page.text(String::new());
        let flat = baseline_flat(&found.page);

        assert!(
            compact.starts_with(&format!("base: {base}/handlers/\n")),
            "{compact}"
        );
        // Single-hit files stay compact (`{rel}:{line}: {snippet}`).
        assert!(
            compact.contains("step_01.rs:1: pub fn handle_1() { retry_budget(1); }\n"),
            "{compact}"
        );
        // Multi-hit file groups cleanly under `{rel}:`.
        assert!(
            compact.contains("step_03.rs:\n  1: let x_1 = retry_budget(1);\n"),
            "{compact}"
        );
        assert!(
            compact.len() * 100 <= flat.len() * 60,
            "expected >=40% reduction: compact={} flat={}\n{compact}",
            compact.len(),
            flat.len()
        );
    }

    #[test]
    fn repeated_intra_file_lines_collapse_with_exact_lines_and_cursor_parity() {
        let source = "\
fn process(mode: u8) -> Result<(), Error> {
    if mode == 1 {
        return Err(Error::RetryBudgetExhausted);
    }
    log_warning(\"RetryBudgetExhausted check\");
    if mode == 2 {
            return Err(Error::RetryBudgetExhausted);
    }
    if mode == 3 {
        return Err(Error::RetryBudgetExhausted);
    }
    if mode == 4 {
        return Err(Error::RetryBudgetExhausted);
    }
    metrics::count(\"RetryBudgetExhausted\");
    return Err(Error::RetryBudgetExhausted);
}
";
        let (_dir, ws, index) = temp_workspace(&[("services/billing/src/retry.rs", source)]);
        let in_scope: Vec<&String> = index.files.keys().collect();
        let found = scan(
            &ws,
            &index,
            &in_scope,
            "RetryBudgetExhausted",
            false,
            None,
            20,
        );
        assert_eq!(found.total, 7);

        let request = test_request("RetryBudgetExhausted", 20, 4096);
        let page = Page {
            request: &request,
            found: &found,
            search: "0123abcd",
            index: &index,
        };
        let compact = page.text(String::new());
        let flat = baseline_flat(&found.page);

        assert!(
            compact.contains(
                "services/billing/src/retry.rs:\n  3, 7, 10, 13, 16 (5×): return Err(Error::RetryBudgetExhausted);\n"
            ),
            "{compact}"
        );
        assert!(
            compact.contains("  5: log_warning(\"RetryBudgetExhausted check\");\n"),
            "{compact}"
        );
        assert!(
            compact.contains("  15: metrics::count(\"RetryBudgetExhausted\");\n"),
            "{compact}"
        );
        assert!(
            compact.len() * 100 <= flat.len() * 55,
            "expected >=45% reduction: compact={} flat={}\n{compact}",
            compact.len(),
            flat.len()
        );

        // Cut mid-file with a tight budget on a larger repeated-line file: Cursor
        // must point to the exact last represented Hit in found.page, and the next
        // page must resume cleanly without skipping or duplicating any hit.
        let bigger = format!(
            "{source}fn extra() {{\n{}\n}}\n",
            (1..=8)
                .map(|i| format!("    record_event(\"RetryBudgetExhausted step {i}\");"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let (_dir2, ws2, index2) = temp_workspace(&[("services/billing/src/retry.rs", &bigger)]);
        let in_scope2: Vec<&String> = index2.files.keys().collect();
        let found2 = scan(
            &ws2,
            &index2,
            &in_scope2,
            "RetryBudgetExhausted",
            false,
            None,
            20,
        );
        assert_eq!(found2.total, 15);
        let tight_req = test_request("RetryBudgetExhausted", 20, 420);
        let tight_page = Page {
            request: &tight_req,
            found: &found2,
            search: "0123abcd",
            index: &index2,
        };
        let first = tight_page.text(String::new());
        assert!(first.len() <= 420, "len={}\n{first}", first.len());
        let cursor_str = first
            .split_whitespace()
            .find_map(|w| w.strip_prefix("--after="))
            .expect("expected budget-cut cursor");
        let cursor = Cursor::parse(cursor_str).unwrap();
        let stop_idx = found2
            .page
            .iter()
            .position(|h| h.line == cursor.line)
            .expect("cursor line must be a hit in found.page");
        assert!(
            first.contains(&format!(
                "[budget reached after {} of 15 matches in 1 file, before --limit 20]",
                stop_idx + 1
            )),
            "{first}"
        );
        for hit in &found2.page[..=stop_idx] {
            assert!(
                first.contains(&hit.line.to_string()),
                "missing line {} on page 1:\n{first}",
                hit.line
            );
        }
        let next_found = scan(
            &ws2,
            &index2,
            &in_scope2,
            "RetryBudgetExhausted",
            false,
            Some(("services/billing/src/retry.rs", cursor.line)),
            20,
        );
        assert_eq!(next_found.before, stop_idx + 1);
        assert_eq!(next_found.page.len(), found2.total - (stop_idx + 1));
        assert_eq!(next_found.page[0].line, found2.page[stop_idx + 1].line);
    }

    #[test]
    fn short_three_hit_files_stay_flat_while_four_plus_and_long_paths_group() {
        // Single-hit file stays compact (`db/schema.sql:1: ...`).
        let single = [Hit::from_line(
            "db/schema.sql".into(),
            1,
            "CREATE TABLE purchase_orders (id INT);",
            13,
            15,
        )];
        assert_eq!(
            render_body(&single),
            "db/schema.sql:1: CREATE TABLE purchase_orders (id INT);\n"
        );

        // Short 3-hit distinct files stay flat.
        let three_short = [
            Hit::from_line("notes.txt".into(), 1, "marker 1", 0, 6),
            Hit::from_line("notes.txt".into(), 2, "marker 2", 0, 6),
            Hit::from_line("notes.txt".into(), 3, "marker 3", 0, 6),
        ];
        assert_eq!(render_body(&three_short).lines().count(), 3);

        // 4+ distinct hits in a short file group cleanly and save bytes.
        let five_short: Vec<Hit> = (1..=5)
            .map(|i| Hit::from_line("notes.txt".into(), i, &format!("marker {i}"), 0, 6))
            .collect();
        let grouped_short = render_body(&five_short);
        assert!(grouped_short.starts_with("notes.txt:\n  1: marker 1\n"));
        assert!(grouped_short.len() < baseline_flat(&five_short).len());

        // 2 distinct hits in a long path group cleanly and save bytes.
        let two_long = [
            Hit::from_line(
                "services/billing/src/invoice_processor.rs".into(),
                12,
                "let total = compute_tax(order);",
                12,
                11,
            ),
            Hit::from_line(
                "services/billing/src/invoice_processor.rs".into(),
                48,
                "assert_eq!(compute_tax(&sample), 42);",
                11,
                11,
            ),
        ];
        let grouped_long = render_body(&two_long);
        assert!(
            grouped_long.starts_with("services/billing/src/invoice_processor.rs:\n  12: "),
            "{grouped_long}"
        );
        assert!(grouped_long.len() < baseline_flat(&two_long).len());
    }

    #[test]
    fn minified_4kb_line_windows_around_match_on_tight_budgets() {
        let term = "checkout_retry_budget";
        let prefix_js = "var a=1;function f(){return a+1;}".repeat(62); // ~2046 bytes
        let suffix_js = ";var b=2;function g(){return b*2;}".repeat(62); // ~2108 bytes
        let minified = format!("{prefix_js}const cfg={{{term}:42}}{suffix_js}");
        assert!(minified.len() > 4096);

        let (_dir, ws, index) = temp_workspace(&[("dist/bundle.min.js", &minified)]);
        let in_scope: Vec<&String> = index.files.keys().collect();
        let found = scan(&ws, &index, &in_scope, term, false, None, 20);
        assert_eq!(found.page.len(), 1);
        let hit = &found.page[0];
        // Match-centered window fits in SNIPPET_BYTES (vs old 281-byte raw slice)
        // with edge ellipses and visible term.
        assert!(hit.text.len() <= SNIPPET_BYTES, "len={}", hit.text.len());
        assert!(hit.text.starts_with(ELLIPSIS) && hit.text.ends_with(ELLIPSIS));
        assert!(hit.text.contains(term), "{}", hit.text);
        assert!(!hit.text.contains("……"), "{}", hit.text);

        // Sweep every tight budget from 0..=220 bytes: never panics, always <= cap
        // (for cap >= 3), never duplicates ellipses, and keeps `term` visible
        // whenever `cap >= term.len()`.
        for cap in 0..=220 {
            let windowed = hit.window_to(cap);
            if cap >= ELLIPSIS_BYTES {
                assert!(
                    windowed.len() <= cap,
                    "cap={cap}, got len={}: {windowed}",
                    windowed.len()
                );
            }
            assert!(!windowed.contains("……"), "cap={cap}: {windowed}");
            if cap >= term.len() {
                assert!(
                    windowed.contains(term),
                    "term lost at cap={cap}: {windowed}"
                );
            }
        }

        // Tight Page::text budget (72 bytes total): old blind right-truncation
        // would have kept only leading junk and deleted `term`!
        let req = test_request(term, 20, 72);
        let page = Page {
            request: &req,
            found: &found,
            search: "0123abcd",
            index: &index,
        };
        let rendered = page.text(String::new());
        assert!(rendered.len() <= 72, "len={}: {rendered}", rendered.len());
        assert!(rendered.starts_with("dist/bundle.min.js:1: "), "{rendered}");
        assert!(
            rendered.contains(term),
            "term lost on tight page: {rendered}"
        );

        // Tight Page::json preserves schema and keeps `term` visible within budget.
        let mut json_req = test_request(term, 20, 140);
        json_req.json = true;
        let json_page = Page {
            request: &json_req,
            found: &found,
            search: "0123abcd",
            index: &index,
        };
        let raw_json = json_page.json(None, &[]).unwrap();
        assert!(raw_json.len() <= 140, "len={}: {raw_json}", raw_json.len());
        let parsed: Value = serde_json::from_str(&raw_json).unwrap();
        assert_eq!(parsed["total"], 1);
        assert_eq!(parsed["files"], 1);
        assert_eq!(parsed["first"], 1);
        let snippet = parsed["matches"][0]["text"].as_str().unwrap();
        assert!(
            snippet.contains(term),
            "term lost in tight json: {raw_json}"
        );

        // Multi-byte UTF-8 boundaries and leading whitespace trimming.
        let utf8_line = format!("{}café_token{}", "🔥".repeat(80), "🚀".repeat(80));
        let utf8_pos = utf8_line.find("café_token").unwrap();
        let utf8_hit = Hit::from_line(
            "i18n.txt".into(),
            1,
            &utf8_line,
            utf8_pos,
            "café_token".len(),
        );
        assert!(utf8_hit.text.contains("café_token"));
        for cap in 0..=120 {
            let w = utf8_hit.window_to(cap);
            if cap >= ELLIPSIS_BYTES {
                assert!(w.len() <= cap, "utf8 cap={cap}, len={}: {w}", w.len());
            }
            if cap >= "café_token".len() {
                assert!(w.contains("café_token"), "utf8 term lost at cap={cap}: {w}");
            }
        }

        // Leading whitespace trimmed when start == 0, without cutting term's own spaces.
        let indented = Hit::from_line("a.rs".into(), 1, "        let v = token();  ", 16, 5);
        assert_eq!(indented.text, "let v = token();");
        let space_term = Hit::from_line("a.rs".into(), 1, "    foo", 2, 5);
        assert_eq!(space_term.text, "  foo");
    }

    #[test]
    fn budget_paging_round_trip_visits_every_hit_once_across_hoist_group_and_collapse() {
        let base = "packages/platform/billing/src";
        let repeated = "\
fn a() {
    retry_budget();
    retry_budget();
    let other = retry_budget();
    retry_budget();
    retry_budget();
}
";
        let long_line = format!("{} retry_budget() {}\n", "x".repeat(1500), "y".repeat(1500));
        let files = [
            (format!("{base}/alpha.rs"), repeated.to_owned()),
            (
                format!("{base}/beta.rs"),
                "fn b1() { retry_budget(); }\nfn b2() { retry_budget(); }\n".to_owned(),
            ),
            (format!("{base}/gamma.rs"), long_line),
            (
                format!("{base}/delta.rs"),
                (1..=6)
                    .map(|i| format!("    let d{i} = retry_budget({i});\n"))
                    .collect(),
            ),
        ];
        let file_refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        let (_dir, ws, index) = temp_workspace(&file_refs);
        let in_scope: Vec<&String> = index.files.keys().collect();
        let all = scan(&ws, &index, &in_scope, "retry_budget", false, None, 100);
        let expected_coords: Vec<(String, usize)> =
            all.page.iter().map(|h| (h.file.clone(), h.line)).collect();
        assert_eq!(expected_coords.len(), 14);

        for budget in [260usize, 380, 560] {
            let mut after: Option<(String, usize)> = None;
            let mut visited: Vec<(String, usize)> = Vec::new();
            let mut pages = 0usize;
            loop {
                let after_ref = after.as_ref().map(|(f, l)| (f.as_str(), *l));
                let found = scan(&ws, &index, &in_scope, "retry_budget", false, after_ref, 50);
                let mut req = test_request("retry_budget", 50, budget);
                req.after_given = after.is_some();
                let page = Page {
                    request: &req,
                    found: &found,
                    search: "0123abcd",
                    index: &index,
                };
                let text = page.text(String::new());
                assert!(
                    text.len() <= budget,
                    "budget={budget}, len={}\n{text}",
                    text.len()
                );
                pages += 1;
                if let Some(cursor_str) = text
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix("--after="))
                {
                    let cursor = Cursor::parse(cursor_str).unwrap();
                    let stop_idx = found
                        .page
                        .iter()
                        .position(|h| {
                            short_hash(&[h.file.as_bytes()], 6) == cursor.file
                                && h.line == cursor.line
                        })
                        .expect("cursor must match a hit in found.page");
                    for hit in &found.page[..=stop_idx] {
                        visited.push((hit.file.clone(), hit.line));
                    }
                    after = Some((found.page[stop_idx].file.clone(), cursor.line));
                } else {
                    for hit in &found.page {
                        visited.push((hit.file.clone(), hit.line));
                    }
                    break;
                }
            }
            assert_eq!(visited, expected_coords, "budget={budget}");
            assert!(pages >= 2, "budget={budget}, pages={pages}");
        }
    }

    #[test]
    fn deep_partial_prefix_hoisting_and_contiguous_range_collapse_stay_within_budget() {
        let deep = "packages/platform/services/billing/v2/handlers";
        let contiguous = format!(
            "fn init() {{\n{}    let unique = retry_budget(99);\n}}\n",
            [
                "    retry_budget(1);\n",
                "    retry_budget(1);\n",
                "\tretry_budget(1);\n",
                "    retry_budget(1);\n",
                "    retry_budget(1);\n",
                "    retry_budget(1);\n",
            ]
            .join("")
        );
        let mut files: Vec<(String, String)> = vec![
            ("Cargo.toml".into(), "retry_budget = 3\n".into()),
            (format!("{deep}/batch_01.rs"), contiguous),
        ];
        for i in 2..=7 {
            files.push((
                format!("{deep}/batch_{i:02}.rs"),
                format!("pub fn run_{i}() {{ retry_budget({i}); }}\n"),
            ));
        }
        files.push((
            "tests/e2e.rs".into(),
            "fn test_all() { retry_budget(0); }\n".into(),
        ));
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        let (_dir, ws, index) = temp_workspace(&refs);
        let in_scope: Vec<&String> = index.files.keys().collect();
        let found = scan(&ws, &index, &in_scope, "retry_budget", false, None, 50);
        assert_eq!(found.total, 15);
        assert_eq!(found.files, 9);

        let full_req = test_request("retry_budget", 50, 4096);
        let page = Page {
            request: &full_req,
            found: &found,
            search: "0123abcd",
            index: &index,
        };
        let compact = page.text(String::new());
        let flat = baseline_flat(&found.page);
        assert!(
            compact.starts_with(&format!("base: … = {deep}/\n")),
            "{compact}"
        );
        assert!(
            compact.contains("Cargo.toml:1: retry_budget = 3\n"),
            "{compact}"
        );
        assert!(
            compact.contains("…/batch_01.rs:\n  2-7 (6×): retry_budget(1);\n  8: let unique = retry_budget(99);\n"),
            "{compact}"
        );
        assert!(
            compact.contains("…/batch_07.rs:1: pub fn run_7() { retry_budget(7); }\n"),
            "{compact}"
        );
        assert!(
            compact.contains("tests/e2e.rs:1: fn test_all() { retry_budget(0); }\n"),
            "{compact}"
        );
        assert!(
            compact.len() * 100 <= flat.len() * 55,
            "expected >=45% savings from partial deep hoist + range collapse: {} vs {}\n{compact}",
            compact.len(),
            flat.len()
        );

        // Tight budget paging round-trip across partial hoist + contiguous collapse visits
        // every hit exactly once without exceeding budget.
        let expected: Vec<(String, usize)> = found
            .page
            .iter()
            .map(|h| (h.file.clone(), h.line))
            .collect();
        let mut after: Option<(String, usize)> = None;
        let mut visited = Vec::new();
        loop {
            let after_ref = after.as_ref().map(|(f, l)| (f.as_str(), *l));
            let f = scan(&ws, &index, &in_scope, "retry_budget", false, after_ref, 50);
            let mut req = test_request("retry_budget", 50, 320);
            req.after_given = after.is_some();
            let p = Page {
                request: &req,
                found: &f,
                search: "0123abcd",
                index: &index,
            };
            let text = p.text(String::new());
            assert!(text.len() <= 320, "len={}\n{text}", text.len());
            if let Some(c_str) = text
                .split_whitespace()
                .find_map(|w| w.strip_prefix("--after="))
            {
                let cursor = Cursor::parse(c_str).unwrap();
                let stop_idx = f
                    .page
                    .iter()
                    .position(|h| {
                        short_hash(&[h.file.as_bytes()], 6) == cursor.file && h.line == cursor.line
                    })
                    .unwrap();
                for hit in &f.page[..=stop_idx] {
                    visited.push((hit.file.clone(), hit.line));
                }
                after = Some((f.page[stop_idx].file.clone(), cursor.line));
            } else {
                for hit in &f.page {
                    visited.push((hit.file.clone(), hit.line));
                }
                break;
            }
        }
        assert_eq!(visited, expected);
    }

    #[test]
    fn ascii_case_insensitive_fast_scan_handles_crlf_utf8_and_cursor_resumption() {
        let (_dir, ws, index) = temp_workspace(&[
            (
                "a.txt",
                "first\r\nCAFÉ_Token once Café_TOKEN twice\r\nplain\r\n",
            ),
            ("b.txt", "no match here\njust filler\n"),
            ("c.txt", "line1\n  caFé_tOkEn final"),
        ]);
        let in_scope: Vec<&String> = index.files.keys().collect();
        let term = "café_token";
        // Notice UTF-8 `É` (U+00C9) != `é` (U+00E9) under ASCII case folding, matching
        // `to_ascii_lowercase()` semantics: only `Café_TOKEN` and `caFé_tOkEn` match!
        let found = scan(&ws, &index, &in_scope, term, true, None, 10);
        assert_eq!(found.total, 2);
        assert_eq!(found.files, 2);
        assert_eq!(found.page.len(), 2);
        assert_eq!(found.page[0].file, "a.txt");
        assert_eq!(found.page[0].line, 2);
        assert_eq!(found.page[0].text, "CAFÉ_Token once Café_TOKEN twice");
        assert_eq!(found.page[1].file, "c.txt");
        assert_eq!(found.page[1].line, 2);
        assert_eq!(found.page[1].text, "caFé_tOkEn final");

        // Resuming after `a.txt:2` counts 1 before and returns only `c.txt:2`.
        let resumed = scan(&ws, &index, &in_scope, term, true, Some(("a.txt", 2)), 10);
        assert_eq!(resumed.total, 2);
        assert_eq!(resumed.before, 1);
        assert_eq!(resumed.page.len(), 1);
        assert_eq!(resumed.page[0].file, "c.txt");
        assert_eq!(resumed.page[0].line, 2);

        // Quoted spaces inside a single file stay separate under collapse_file_hits.
        let hits = [
            Hit::from_line("q.rs".into(), 1, "assert_eq!(s, \"a  b\");", 0, 9),
            Hit::from_line("q.rs".into(), 2, "assert_eq!(s, \"a  b\");", 0, 9),
            Hit::from_line("q.rs".into(), 3, "assert_eq!(s, \"a b\");", 0, 9),
        ];
        let body = render_body(&hits);
        assert_eq!(
            body,
            "q.rs:1, 2 (2×): assert_eq!(s, \"a  b\");\nq.rs:3: assert_eq!(s, \"a b\");\n"
        );
    }
}
