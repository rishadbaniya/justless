//! Single-file shell commands that read an inclusive source range, and
//! `git show REV:path`, which prints a file as a revision or the index holds it.
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::ops::Range;
use std::path::{Component, Path};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::capture::{self, Origin, SourceOutline};
use crate::history;
use crate::source::Snapshot;
use crate::workspace::{Workspace, shell_quote};

/// One window a read prints: lines `first..=last`, or `first` to the end of
/// the file. With `from_end`, `first` counts lines back from the end, as
/// `tail -n N` does.
#[derive(Clone, Copy)]
struct Span {
    first: usize,
    last: Option<usize>,
    from_end: bool,
}

/// The windows of a recognized read, in the order the command prints them.
#[derive(Clone)]
pub(crate) struct LineRange {
    spans: Vec<Span>,
    /// The operand names a Git object, such as `HEAD~1:src/a.rs`, rather than
    /// a file in the working tree.
    object: bool,
}

fn decimal(text: &str) -> Option<usize> {
    if text.is_empty() || text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl Span {
    fn closed(text: &str, separator: char) -> Option<Self> {
        let (first, last) = text.split_once(separator)?;
        let (first, last) = (decimal(first)?, decimal(last)?);
        (first <= last).then_some(Self {
            first,
            last: Some(last),
            from_end: false,
        })
    }

    fn tail(text: &str) -> Option<Self> {
        Some(Self {
            first: decimal(text.strip_prefix('+')?)?,
            last: None,
            from_end: false,
        })
    }
}

impl LineRange {
    fn one(span: Span) -> Self {
        Self {
            spans: vec![span],
            object: false,
        }
    }

    /// Whether the read is a `git show` of an object, which falls back to the
    /// ordinary run report when it cannot be outlined.
    pub(crate) fn object(&self) -> bool {
        self.object
    }

    /// A `sed -n` script of `A,Bp` addresses, one or several joined by `;`,
    /// such as `12,107p;209,260p`.
    fn sed(script: &str) -> Option<Self> {
        let spans = script
            .split(';')
            .map(|part| Span::closed(part.strip_suffix('p')?, ','))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            spans,
            object: false,
        })
    }

    /// The `read --shell-range` argument: `A-B` and `+N` windows joined by commas.
    fn parse(text: &str) -> Option<Self> {
        let spans = text
            .split(',')
            .map(|part| Span::tail(part).or_else(|| Span::closed(part, '-')))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            spans,
            object: false,
        })
    }
}

/// The object name in `git show REV:path`, `git show REV:./path` or
/// `git show :path` (the index), also after `git --no-pager`.
pub(crate) fn show(words: &[String]) -> Option<&str> {
    let spec = match words {
        [git, show, spec] if git == "git" && show == "show" => spec,
        [git, pager, show, spec] if git == "git" && pager == "--no-pager" && show == "show" => spec,
        _ => return None,
    };
    blob_path(spec).map(|_| spec.as_str())
}

/// The path part of an object name that names a file, such as `src/a.rs` in
/// `HEAD~1:./src/a.rs`. The revision is a plain name with `~`, `^` and `@`, so
/// a `:/text` message search, a `:N:path` stage or a `^{type}` peel is not one.
pub(crate) fn blob_path(spec: &str) -> Option<&str> {
    let (rev, path) = spec.split_once(':')?;
    if rev.starts_with('-')
        || !rev
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/~^@".contains(&b))
    {
        return None;
    }
    let path = path.strip_prefix("./").unwrap_or(path);
    let normal = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    (normal
        && !path.is_empty()
        && !path.starts_with('-')
        && !path.ends_with('/')
        && !path.contains(':')
        && !path.chars().any(char::is_control))
    .then_some(path)
}

/// A Git invocation that only reads: no optional locks, no fsmonitor hook, and
/// no fetch of an object a partial clone left out.
fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["--no-pager", "-c", "core.fsmonitor=false"])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// The text of blob `id`, as `git show` prints it: no text conversion, at most
/// `cap` bytes, UTF-8 without NUL bytes. A tree, a submodule commit or any
/// other object type fails `cat-file blob`.
fn blob(dir: &Path, id: &str, cap: u64) -> Option<String> {
    if ![40, 64].contains(&id.len()) || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut child = git(dir)
        .args(["cat-file", "blob", id])
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()?
        .take(cap.saturating_add(1))
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() as u64 > cap {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    if !child.wait().ok()?.success() || bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// The blob that `git show SPEC` prints in `dir`, and its id.
pub(crate) fn object(dir: &Path, spec: &str, cap: u64) -> Option<(String, String)> {
    blob_path(spec)?;
    let output = git(dir)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options", spec])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8(output.stdout).ok()?;
    let id = id.strip_suffix('\n')?.to_owned();
    let text = blob(dir, &id, cap)?;
    Some((id, text))
}

/// A standalone single-file read: `sed -n 'A,Bp[;C,Dp…]' FILE`,
/// `head -n N FILE`, `tail -n N FILE`, `tail -n +N FILE` or `git show REV:path`,
/// whose operand is then the object name.
pub(crate) fn command(words: &[String]) -> Option<(&str, LineRange)> {
    if let Some(spec) = show(words) {
        let mut range = LineRange::one(Span::tail("+1")?);
        range.object = true;
        return Some((spec, range));
    }
    let [program, flag, address, file] = words else {
        return None;
    };
    if flag != "-n"
        || file.is_empty()
        || file.starts_with(['-', '='])
        || file.chars().any(char::is_control)
    {
        return None;
    }
    let range = match program.as_str() {
        "sed" => LineRange::sed(address)?,
        "head" => LineRange::one(Span {
            first: 1,
            last: Some(decimal(address)?),
            from_end: false,
        }),
        "tail" => LineRange::one(Span::tail(address).or_else(|| {
            Some(Span {
                first: decimal(address)?,
                last: None,
                from_end: true,
            })
        })?),
        _ => return None,
    };
    Some((file, range))
}

/// One printed window, resolved against the file.
struct Window {
    first: usize,
    last: usize,
    bytes: Range<usize>,
}

struct Selection {
    source: Snapshot,
    /// The workspace file, or for a Git object its path, which picks the parser.
    file: String,
    /// For `git show`: the object name as the command gave it, and the blob id.
    object: Option<(String, String)>,
    windows: Vec<Window>,
    text: String,
    /// The last window stops at a requested line rather than at the end of
    /// the file, where sed and head implementations differ on a missing
    /// final newline.
    closed: bool,
}

impl Selection {
    fn new(source: Snapshot, file: &str, range: &LineRange) -> Option<Self> {
        let total = source.line_count();
        let mut windows: Vec<Window> = Vec::new();
        for span in &range.spans {
            let first = if span.from_end {
                total.checked_sub(span.first).map_or(1, |rest| rest + 1)
            } else {
                span.first
            };
            // sed prints overlapping or out-of-order addresses line by line,
            // repeating lines, so only ascending, disjoint windows are exact.
            if windows.last().is_some_and(|window| first <= window.last) {
                return None;
            }
            let (last, bytes) = source.line_window(first, span.last.unwrap_or(usize::MAX))?;
            windows.push(Window { first, last, bytes });
        }
        let text = windows
            .iter()
            .map(|window| &source.text[window.bytes.clone()])
            .collect();
        let closed = range.spans.last()?.last.is_some();
        Some(Self {
            source,
            file: file.to_owned(),
            object: None,
            windows,
            text,
            closed,
        })
    }

    /// The blob `git show SPEC` prints in `dir`, as `id` when the hook already
    /// resolved the name.
    fn object(
        ws: &Workspace,
        dir: &Path,
        spec: &str,
        id: Option<&str>,
        range: &LineRange,
    ) -> Option<Self> {
        let path = blob_path(spec)?;
        let cap = ws.settings().ok()?.max_file_bytes;
        let (id, text) = match id {
            Some(id) => (id.to_owned(), blob(dir, id, cap)?),
            None => object(dir, spec, cap)?,
        };
        let mut read = Self::new(Snapshot::from_text(path, text), path, range)?;
        read.object = Some((spec.to_owned(), id));
        Some(read)
    }

    /// How the outline header names the read.
    fn name(&self) -> &str {
        self.object.as_ref().map_or(&self.file, |(spec, _)| spec)
    }

    /// The windows as `read --shell-range` and the outline header name them.
    fn ranges(&self, separator: &str) -> String {
        let last = self.windows.len() - 1;
        self.windows
            .iter()
            .enumerate()
            .map(|(i, window)| {
                if i == last && !self.closed {
                    format!("+{}", window.first)
                } else {
                    format!("{}-{}", window.first, window.last)
                }
            })
            .collect::<Vec<_>>()
            .join(separator)
    }

    fn open(ws: &Workspace, cwd: &Path, file: &str, range: &LineRange) -> Option<Self> {
        if range.object {
            return cwd
                .starts_with(&ws.root)
                .then(|| Self::object(ws, cwd, file, None, range))?;
        }
        let path = cwd.join(file);
        let relative = path.strip_prefix(&ws.root).ok()?;
        if relative.components().any(|c| c == Component::ParentDir) {
            return None;
        }
        let relative: std::path::PathBuf = relative.components().collect();
        let file = relative.to_str()?;
        Self::new(Snapshot::open(ws, file).ok()?, file, range)
    }

    fn text(&self) -> &str {
        &self.text
    }

    fn observe(&self, ws: &Workspace, outline: bool) {
        // A blob from a revision or the index is not the working file.
        if self.object.is_some() {
            return;
        }
        let digest = capture::source_digest(self.source.text.as_bytes());
        for window in &self.windows {
            history::saw_range(
                ws,
                &self.file,
                window.first,
                window.last,
                digest.clone(),
                outline,
            );
        }
    }

    fn outline(&mut self, ws: &Workspace, baseline: usize) -> Option<String> {
        let settings = ws.settings().ok()?;
        let lines: usize = self
            .windows
            .iter()
            .map(|window| window.last + 1 - window.first)
            .sum();
        let bytes = self.text.len() as u64;
        if (lines <= 150 && bytes <= 6144)
            || bytes > settings.max_run_bytes
            || settings.max_runs == 0
            || bytes > settings.max_archive_bytes
        {
            return None;
        }
        // Leave room for the exact command and the capture footer, including
        // the root when a nested project needs it.
        let budget = settings
            .context_budget
            .min(baseline / 2)
            .checked_sub(160 + 2 * ws.justless().len())?;
        self.source.index(&self.file).ok()?;
        if crate::agents::uncovered_code(&self.file, &self.source)?
            .iter()
            .any(|&i| {
                self.windows
                    .iter()
                    .any(|window| (window.first - 1..window.last).contains(&i))
            })
        {
            return None;
        }
        let shown = self
            .windows
            .iter()
            .map(|window| format!("{}-{}", window.first, window.last))
            .collect::<Vec<_>>()
            .join(", ");
        let mut outline = format!("{} lines {shown}\n", self.name());
        let mut count = 0;
        let mut shown = 0;
        for (name, kind, a, b) in self.source.symbols() {
            if kind == "text" {
                return None;
            }
            // A row for each symbol a window shows part of, not one that only
            // encloses a whole window, such as the impl around a method.
            let listed = self.windows.iter().any(|window| {
                a <= window.last && b >= window.first && !(a <= window.first && b >= window.last)
            });
            if kind == "import" || !listed {
                continue;
            }
            let row = format!("  L{a}-{b} {kind} {name}\n");
            count += 1;
            if shown + 1 == count && outline.len() + row.len() + 32 <= budget {
                outline.push_str(&row);
                shown += 1;
            }
        }
        if shown < 2 {
            return None;
        }
        if count > shown {
            outline.push_str(&format!("  … {} more symbols\n", count - shown));
        }
        // A digest or name cut out of a sensitive line must not escape redaction.
        if let Some(redactor) = crate::redact::for_run(ws).ok()?
            && matches!(redactor.text(self.text()), std::borrow::Cow::Owned(_))
        {
            return None;
        }
        Some(outline)
    }

    fn compact(
        &mut self,
        ws: &Workspace,
        baseline: usize,
        session: &str,
        claude: bool,
    ) -> Option<String> {
        let outline = self.outline(ws, baseline)?;
        let words = self.command();
        let args: Vec<_> = words.iter().map(OsString::from).collect();
        let origin = Origin {
            command: shlex::try_join(words.iter().map(String::as_str)).ok()?,
            // Kept apart from `git show` patches, whose preview budget adapts
            // to how often their clipped runs are replayed.
            label: if self.object.is_some() {
                "git show REV:path".into()
            } else {
                words[0].clone()
            },
            note: String::new(),
            outline: Some(SourceOutline {
                text: outline,
                baseline,
                trim_footer: claude,
            }),
        };
        let result = capture::record(
            ws,
            &args,
            self.text().as_bytes(),
            0,
            ws.settings().ok()?.context_budget,
            session,
            &origin,
        )
        .ok()?;
        let mut text = String::new();
        for (_, bytes) in result.previews {
            text.push_str(std::str::from_utf8(&bytes).ok()?);
        }
        text.push_str(&result.footer);
        self.observe(ws, true);
        Some(text)
    }

    /// The command that prints these bytes, which keys the archived run.
    fn command(&self) -> Vec<String> {
        if let Some((spec, _)) = &self.object {
            return vec!["git".into(), "show".into(), spec.clone()];
        }
        let (program, address) = match (&self.windows[..], self.closed) {
            ([window], false) => ("tail", format!("+{}", window.first)),
            (windows, closed) => {
                let last = windows.len() - 1;
                let script = windows
                    .iter()
                    .enumerate()
                    .map(|(i, window)| {
                        if i == last && !closed {
                            format!("{},$p", window.first)
                        } else {
                            format!("{},{}p", window.first, window.last)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(";");
                ("sed", script)
            }
        };
        [program, "-n", &address, &self.file]
            .map(str::to_owned)
            .to_vec()
    }
}

/// Other shell clients run a pure workspace read. Leave the native sed in
/// charge where implementations disagree on an unterminated last line. A
/// `git show` is pinned to the blob its name resolves to now, and rerouted only
/// when the outline will shorten it.
pub(crate) fn rewrite(
    ws: &Workspace,
    cwd: &Path,
    file: &str,
    range: LineRange,
    session: Option<&str>,
) -> Option<String> {
    // A count from the end is resolved here, before the command runs, so a file
    // that grows in between would print other lines. Only the post-run hook,
    // which checks the bytes the command printed, takes `tail -n N`.
    if range.spans.iter().any(|span| span.from_end) {
        return None;
    }
    let mut read = Selection::open(ws, cwd, file, &range)?;
    if read.closed && !read.text().ends_with('\n') {
        return None;
    }
    let object = match read.object.clone() {
        Some((_, id)) => {
            read.outline(ws, read.text.len())?;
            format!(" --shell-object {id}")
        }
        None => String::new(),
    };
    let session = session
        .filter(|id| !id.is_empty())
        .map(|id| {
            format!(
                " --shell-session {}",
                &capture::source_digest(id.as_bytes())[..24]
            )
        })
        .unwrap_or_default();
    Some(format!(
        "justless --root {} read --shell-range {}{object}{session} -- {}",
        shell_quote(&ws.root),
        read.ranges(","),
        shell_quote(Path::new(read.name()))
    ))
}

/// `read --shell-range`: `file` is a workspace file, or with `object` the name
/// a `git show` gave blob `object`.
pub(crate) fn read(
    ws: &Workspace,
    file: &str,
    object: Option<&str>,
    range: &str,
    session: &str,
) -> Result<()> {
    let range = LineRange::parse(range)
        .context("expected positive line ranges A-B or +N, joined by commas")?;
    let read = match object {
        Some(id) => Some(
            Selection::object(ws, &ws.cwd, file, Some(id), &range)
                .with_context(|| format!("read Git blob {id} as {file}"))?,
        ),
        None => Selection::new(Snapshot::open(ws, file)?, file, &range),
    };
    let Some(mut read) = read else {
        return Ok(());
    };
    let mut out = io::stdout().lock();
    if let Some(text) = read.compact(ws, read.text.len(), session, false) {
        out.write_all(text.as_bytes())?;
    } else {
        out.write_all(read.text().as_bytes())?;
        read.observe(ws, false);
    }
    Ok(())
}

/// Claude executes the original command itself and checks permissions on the original command.
/// Accept the result only if it matches the selected source bytes.
pub(crate) fn claude(ws: &Workspace, input: &Value, file: &str, range: LineRange) -> Option<Value> {
    let response = &input["tool_response"];
    if !response["stderr"].as_str()?.is_empty()
        || response["returnCodeInterpretation"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    {
        return None;
    }
    let cwd = input["cwd"]
        .as_str()
        .map(Path::new)
        .unwrap_or(&ws.cwd)
        .canonicalize()
        .ok()?;
    let mut read = Selection::open(ws, &cwd, file, &range)?;
    let stdout = response["stdout"].as_str()?;
    let persisted = response["persistedOutputPath"].as_str();
    let stored;
    let (output, baseline) = if let Some(path) = persisted {
        stored = crate::postroute::read_persisted(path, &response["persistedOutputSize"], stdout)?;
        (
            std::str::from_utf8(&stored).ok()?,
            stdout
                .chars()
                .take(crate::postroute::PREVIEW_CHARS)
                .map(char::len_utf8)
                .sum(),
        )
    } else {
        (stdout, stdout.len())
    };
    let selected = read.text();
    if output != selected && (persisted.is_some() || selected.trim_end_matches('\n') != output) {
        return None;
    }
    // BSD and GNU sed differ at an unterminated EOF. A post hook can keep
    // either client's original result without reconstructing that final byte.
    if read.closed && !selected.ends_with('\n') {
        return None;
    }
    let session = input["session_id"]
        .as_str()
        .map(|s| capture::source_digest(s.as_bytes())[..24].to_owned());
    let Some(text) = read.compact(
        &ws.working_in(&cwd),
        baseline,
        session.as_deref().unwrap_or("terminal"),
        true,
    ) else {
        read.observe(ws, false);
        return None;
    };
    let mut updated = response.as_object()?.clone();
    updated.insert("stdout".into(), json!(text));
    for key in [
        "persistedOutputPath",
        "persistedOutputSize",
        "stdoutHoldsOutputEnd",
    ] {
        updated.remove(key);
    }
    Some(json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":updated}}))
}

#[cfg(test)]
mod tests {
    use super::{blob_path, command};

    #[test]
    fn git_show_names_a_file_in_a_revision_or_the_index() {
        let words = |text: &str| shlex::split(text).unwrap();
        for (spec, path) in [
            ("HEAD:src/a.rs", "src/a.rs"),
            ("HEAD~1:./a.rs", "a.rs"),
            (":a.rs", "a.rs"),
            ("main^:a", "a"),
            ("@~2:a/b.py", "a/b.py"),
        ] {
            assert_eq!(blob_path(spec), Some(path), "{spec}");
        }
        for spec in [
            "HEAD",
            "HEAD:",
            "HEAD:./",
            ":/fix",
            ":0:a.rs",
            "HEAD:../a",
            "HEAD^{tree}:a",
            "-p:a",
            "HEAD:src/",
            "a b:c",
        ] {
            assert_eq!(blob_path(spec), None, "{spec}");
        }
        let unpaged = words("git --no-pager show HEAD:a.rs");
        let (spec, range) = command(&unpaged).unwrap();
        assert!(spec == "HEAD:a.rs" && range.object());
        for text in [
            "git show HEAD",
            "git show HEAD:a.rs b.rs",
            "git show -p HEAD:a.rs",
            "git -C x show HEAD:a.rs",
        ] {
            assert!(command(&words(text)).is_none(), "{text}");
        }
    }
}
