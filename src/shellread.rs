//! Single-file shell commands that read an inclusive source range.
use std::ffi::OsString;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Component, Path};

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
        Self { spans: vec![span] }
    }

    /// A `sed -n` script of `A,Bp` addresses, one or several joined by `;`,
    /// such as `12,107p;209,260p`.
    fn sed(script: &str) -> Option<Self> {
        let spans = script
            .split(';')
            .map(|part| Span::closed(part.strip_suffix('p')?, ','))
            .collect::<Option<Vec<_>>>()?;
        Some(Self { spans })
    }

    /// The `read --shell-range` argument: `A-B` and `+N` windows joined by commas.
    fn parse(text: &str) -> Option<Self> {
        let spans = text
            .split(',')
            .map(|part| Span::tail(part).or_else(|| Span::closed(part, '-')))
            .collect::<Option<Vec<_>>>()?;
        Some(Self { spans })
    }
}

/// A standalone single-file read: `sed -n 'A,Bp[;C,Dp…]' FILE`,
/// `head -n N FILE`, `tail -n N FILE` or `tail -n +N FILE`.
pub(crate) fn command(words: &[String]) -> Option<(&str, LineRange)> {
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
    file: String,
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
            windows,
            text,
            closed,
        })
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
        let mut outline = format!("{} lines {shown}\n", self.file);
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
        let words = [program, "-n", &address, &self.file];
        let args: Vec<_> = words.iter().map(OsString::from).collect();
        let origin = Origin {
            command: shlex::try_join(words).ok()?,
            label: program.into(),
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
}

/// Other shell clients run a pure workspace read. Leave the native sed in
/// charge where implementations disagree on an unterminated last line.
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
    let read = Selection::open(ws, cwd, file, &range)?;
    if read.closed && !read.text().ends_with('\n') {
        return None;
    }
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
        "justless --root {} read --shell-range {}{session} -- {}",
        shell_quote(&ws.root),
        read.ranges(","),
        shell_quote(Path::new(&read.file))
    ))
}

pub(crate) fn read(
    ws: &Workspace,
    source: Snapshot,
    file: &str,
    range: &str,
    session: &str,
) -> Result<()> {
    let range = LineRange::parse(range)
        .context("expected positive line ranges A-B or +N, joined by commas")?;
    let Some(mut read) = Selection::new(source, file, &range) else {
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
