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

/// An inclusive sed range or a tail read starting at a positive line number.
#[derive(Clone, Copy)]
pub(crate) struct LineRange {
    first: usize,
    last: Option<usize>,
}

fn decimal(text: &str) -> Option<usize> {
    if text.is_empty() || text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl LineRange {
    fn closed(text: &str, separator: char) -> Option<Self> {
        let (first, last) = text.split_once(separator)?;
        let (first, last) = (decimal(first)?, decimal(last)?);
        (first <= last).then_some(Self {
            first,
            last: Some(last),
        })
    }

    fn tail(text: &str) -> Option<Self> {
        Some(Self {
            first: decimal(text.strip_prefix('+')?)?,
            last: None,
        })
    }

    fn argument(self) -> String {
        match self.last {
            Some(last) => format!("{}-{last}", self.first),
            None => format!("+{}", self.first),
        }
    }
}

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
        "sed" => LineRange::closed(address.strip_suffix('p')?, ',')?,
        "tail" => LineRange::tail(address)?,
        _ => return None,
    };
    Some((file, range))
}

struct Selection {
    source: Snapshot,
    file: String,
    range: LineRange,
    last: usize,
    bytes: Range<usize>,
}

impl Selection {
    fn new(source: Snapshot, file: &str, range: LineRange) -> Option<Self> {
        let (last, bytes) = source.line_window(range.first, range.last.unwrap_or(usize::MAX))?;
        Some(Self {
            source,
            file: file.to_owned(),
            range,
            last,
            bytes,
        })
    }

    fn open(ws: &Workspace, cwd: &Path, file: &str, range: LineRange) -> Option<Self> {
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
        &self.source.text[self.bytes.clone()]
    }

    fn observe(&self, ws: &Workspace, outline: bool) {
        let digest = capture::source_digest(self.source.text.as_bytes());
        history::saw_range(ws, &self.file, self.range.first, self.last, digest, outline);
    }

    fn outline(&mut self, ws: &Workspace, baseline: usize) -> Option<String> {
        let settings = ws.settings().ok()?;
        if (self.last - self.range.first < 150 && self.bytes.len() <= 6144)
            || self.bytes.len() as u64 > settings.max_run_bytes
            || settings.max_runs == 0
            || self.bytes.len() as u64 > settings.max_archive_bytes
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
            .any(|&i| (self.range.first - 1..self.last).contains(&i))
        {
            return None;
        }
        let mut outline = format!("{} lines {}-{}\n", self.file, self.range.first, self.last);
        let mut count = 0;
        let mut shown = 0;
        for (name, kind, a, b) in self.source.symbols() {
            if kind == "text" {
                return None;
            }
            if kind == "import"
                || a > self.last
                || b < self.range.first
                || (a <= self.range.first && b >= self.last)
            {
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
        let (program, address) = match self.range.last {
            Some(_) => ("sed", format!("{},{}p", self.range.first, self.last)),
            None => ("tail", self.range.argument()),
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
    let read = Selection::open(ws, cwd, file, range)?;
    if range.last.is_some() && !read.text().ends_with('\n') {
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
        range.argument(),
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
    let range = LineRange::tail(range)
        .or_else(|| LineRange::closed(range, '-'))
        .context("expected a positive line range A-B or +N")?;
    let Some(mut read) = Selection::new(source, file, range) else {
        return Ok(());
    };
    let mut out = io::stdout().lock();
    if let Some(text) = read.compact(ws, read.bytes.len(), session, false) {
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
    let mut read = Selection::open(ws, &cwd, file, range)?;
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
    if range.last.is_some() && !selected.ends_with('\n') {
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
