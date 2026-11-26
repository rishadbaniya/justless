//! Claude executes the original list. Only attribute stdout to individual
//! commands when their independently reproduced outputs agree with its capture.
mod git;
mod grep;
mod parse;
mod reads;

use std::ffi::OsString;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::capture::{self, Origin, RunPiece, SourceOutline};
use crate::postroute::{MAX_BYTES, MAX_LINES, PREVIEW_CHARS};
use crate::workspace::Workspace;
use parse::{Join, Piece, Source};
use serde_json::{Value, json};

pub(crate) struct Batch<'a>(Vec<Piece<'a>>);

/// Output at or under this many bytes is kept whole unless Claude Code saved it to
/// a file.
pub(crate) const FLOOR: usize = 4096;

/// The recheck only runs builtins with explicit arguments. Cap both its stdout
/// and its lifetime, including children that close stdout before exiting.
fn bounded(cmd: &mut Command, cap: usize, deadline: Instant) -> Option<(i32, String)> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?.take(cap as u64 + 1);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .read_to_end(&mut bytes)
            .ok()
            .and_then(|n| (n <= cap).then_some(bytes));
        let _ = tx.send(result);
    });
    let result = (|| {
        let bytes = rx.recv_timeout(remaining).ok()??;
        loop {
            if let Some(status) = child.try_wait().ok()? {
                return Some((status.code()?, String::from_utf8(bytes).ok()?));
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })();
    if result.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn search(
    ws: &Workspace,
    cwd: &Path,
    words: &[String],
    deadline: Instant,
) -> Option<(i32, String)> {
    let rg = words[0] == "rg";
    for operand in parse::search_paths(words)? {
        let path = cwd.join(operand).canonicalize().ok()?;
        if !path.starts_with(&ws.root) || !(path.is_file() || rg && path.is_dir()) {
            return None;
        }
    }
    let binary = if rg {
        [
            "/opt/homebrew/bin/rg",
            "/usr/local/bin/rg",
            "/usr/bin/rg",
            "/bin/rg",
        ]
        .into_iter()
        .find(|p| Path::new(p).is_file())?
    } else {
        "/usr/bin/grep"
    };
    let mut cmd = Command::new(binary);
    cmd.current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin");
    if rg {
        cmd.arg("--no-config");
    } else {
        cmd.args(["-D", "skip"]);
    }
    let (code, text) = bounded(cmd.args(&words[1..]), MAX_BYTES, deadline)?;
    matches!(code, 0 | 1).then_some((code, text))
}

/// Rerun an `ls` or `wc` of workspace paths. Neither writes anything; both
/// sort and count in the locale the shell had.
fn listing(ws: &Workspace, cwd: &Path, words: &[String], deadline: Instant) -> Option<String> {
    let ls = words[0] == "ls";
    for operand in parse::operands(words) {
        let path = cwd.join(operand).canonicalize().ok()?;
        if !path.starts_with(&ws.root) || !(path.is_file() || ls && path.is_dir()) {
            return None;
        }
    }
    let candidates: &[&str] = if ls {
        &["/bin/ls", "/usr/bin/ls"]
    } else {
        &["/usr/bin/wc", "/bin/wc"]
    };
    let binary = candidates.iter().find(|p| Path::new(p).is_file())?;
    let mut cmd = quiet(Path::new(binary), cwd);
    let (code, text) = bounded(cmd.args(&words[1..]), MAX_BYTES, deadline)?;
    (code == 0).then_some(text)
}

/// A rerun of `binary` in `cwd` with a cleared environment but the shell's
/// locale, which decides how it sorts, counts and matches characters.
fn quiet(binary: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(binary);
    cmd.current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin");
    for key in ["LANG", "LC_ALL", "LC_COLLATE", "LC_CTYPE"] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd
}

/// The line as the shell ran it, for the read parsers: with the plain variables
/// it sets itself filled in (see [`parse::expand`]) and each glob replaced by
/// the files it names in `cwd`, where the line starts (see [`parse::glob`]).
/// Without `cwd`, for a line recorded in a transcript, a glob stands for one
/// file. `None` when the line has neither.
pub(crate) fn as_run(command: &str, cwd: Option<&Path>) -> Option<String> {
    let filled = parse::expand(command);
    let globbed = parse::glob(filled.as_deref().unwrap_or(command), cwd);
    globbed.or(filled)
}

struct Verified<'a> {
    piece: &'a Piece<'a>,
    /// For one file of a `cat A B …`, the `cat` of that file alone.
    header: Option<String>,
    range: RunPiece,
    source: Option<reads::SourceLines>,
    dir: PathBuf,
}

impl<'a> Batch<'a> {
    pub(crate) fn parse_list(command: &'a str) -> Option<Self> {
        parse::list(command).map(Self)
    }

    pub(crate) fn parse(command: &'a str) -> Option<Self> {
        Self::parse_list(command).or_else(|| parse::single(command, false).map(Self))
    }

    /// One plain read of a file, such as `sed -n '1,90p' FILE`. The single-file
    /// read takes these in the workspace, so [`Self::parse`] leaves them to it;
    /// outside the workspace only the recheck of a list can show one.
    /// Whether every piece prints a file's lines or a literal, so the list
    /// stays as printed up to `read_outline_lines` lines.
    pub(crate) fn files_only(&self) -> bool {
        self.0.iter().all(|piece| {
            matches!(
                piece.source,
                Source::File { .. } | Source::Files(_) | Source::Object(_) | Source::Literal(_)
            )
        })
    }

    pub(crate) fn parse_read(command: &'a str) -> Option<Self> {
        parse::single(command, true)
            .filter(|pieces| matches!(pieces[0].source, Source::File { .. }))
            .map(Self)
    }

    /// Whether the command hook leaves the list as written for the post-run
    /// hook: every piece is a read. Other lists keep the command hook's route.
    pub(crate) fn reads_only(&self) -> bool {
        parse::reads_only(&self.0)
    }

    /// Whether the recheck reaches every piece when the list starts from `cwd`,
    /// judged from the command alone, for a list recorded in a transcript: each
    /// runs in `root`, except that a file read, and a `grep` that walks nothing,
    /// may name a file outside it that [`crate::agents::outside_file`] allows,
    /// and a literal prints anywhere.
    pub(crate) fn reachable(&self, root: &Path, cwd: &Path) -> bool {
        self.0.iter().all(|piece| {
            let dir = crate::find::lexical(&cwd.join(piece.dir.as_deref().unwrap_or("")));
            let within = |file: &String| crate::find::lexical(&dir.join(file)).starts_with(root);
            let read = |file: &String| {
                within(file)
                    || crate::agents::outside_file(root, &crate::find::lexical(&dir.join(file)))
            };
            match &piece.source {
                Source::File { file, .. } => read(file),
                Source::Files(files) => files.iter().all(read),
                Source::Literal(_) => true,
                Source::Listing(words) => {
                    dir.starts_with(root) && parse::operands(words).all(within)
                }
                Source::Grep(grep) => grep.reaches(root, &dir),
                _ => dir.starts_with(root),
            }
        })
    }

    pub(crate) fn compact(&self, ws: &Workspace, input: &Value) -> Option<Value> {
        let response = &input["tool_response"];
        // The note about a reset directory stays in `stderr` as it is.
        if !crate::postroute::no_stderr(response["stderr"].as_str()?) {
            return None;
        }
        let stdout = response["stdout"].as_str()?;
        let persisted = response["persistedOutputPath"].as_str();
        let settings = ws.settings().ok()?;
        if settings.max_runs == 0 || (persisted.is_none() && stdout.len() <= FLOOR) {
            return None;
        }
        let stored;
        let (actual, baseline) = if let Some(path) = persisted {
            stored =
                crate::postroute::read_persisted(path, &response["persistedOutputSize"], stdout)?;
            (
                std::str::from_utf8(&stored).ok()?,
                stdout.chars().take(PREVIEW_CHARS).map(char::len_utf8).sum(),
            )
        } else {
            (stdout, stdout.len())
        };
        if actual.len() <= 4096
            || actual.len() > MAX_BYTES
            || actual.len() as u64 > settings.max_run_bytes.min(settings.max_archive_bytes)
            || actual.bytes().filter(|&b| b == b'\n').count() > MAX_LINES
        {
            return None;
        }
        let cwd = input["cwd"]
            .as_str()
            .map(Path::new)
            .unwrap_or(&ws.cwd)
            .canonicalize()
            .ok()?;
        if !cwd.starts_with(&ws.root) {
            return None;
        }
        let mut run_ws = ws.working_in(&cwd);
        run_ws.cwd = cwd.clone();
        // Claude Code's main session keeps a `cd`, so its cwd can already be the
        // directory the list entered, while a subagent's is where the list started.
        let mut start = cwd.clone();
        // `pwd` prints the directory as the shell names it, before links resolve.
        let mut logical =
            crate::find::lexical(input["cwd"].as_str().map(Path::new).unwrap_or(&ws.cwd));
        if let Some(dir) = self.0.last().and_then(|piece| piece.dir.as_deref()) {
            let entered: PathBuf = Path::new(dir)
                .components()
                .filter(|c| *c != Component::CurDir)
                .collect();
            if entered.is_relative() && !cwd.join(&entered).is_dir() && cwd.ends_with(&entered) {
                for _ in entered.components() {
                    start.pop();
                    logical.pop();
                }
            }
        }
        let ceiling = (baseline * 3 / 4).checked_sub(192 + 2 * run_ws.justless().len())?;
        let budget = settings.context_budget.min(ceiling);
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut files = reads::Files::new();
        let mut repo: Option<(PathBuf, git::Repository)> = None;
        let mut status = 0;
        let mut may_have_closed_pipe = false;
        let mut output = String::with_capacity(actual.len());
        let mut verified = Vec::with_capacity(self.0.len());
        let mut line = 1;
        for (index, piece) in self.0.iter().enumerate() {
            if Instant::now() >= deadline {
                return None;
            }
            // A head downstream can close its input early. With pipefail this
            // changes whether the shell executes the following conditional.
            if piece.join != Join::Always && may_have_closed_pipe {
                return None;
            }
            if (piece.join == Join::Success && status != 0)
                || (piece.join == Join::Failure && status == 0)
            {
                continue;
            }
            may_have_closed_pipe = piece
                .filters
                .iter()
                .any(|f| matches!(f, parse::Filter::First(_)));
            let dir = start
                .join(piece.dir.as_deref().unwrap_or(""))
                .canonicalize()
                .ok()?;
            // The workspace root is already canonical. Only a file read and a
            // `grep` of named files, both held to the outside-read rules, and a
            // literal leave it.
            let leaves = matches!(
                piece.source,
                Source::File { .. } | Source::Files(_) | Source::Grep(_) | Source::Literal(_)
            );
            if !dir.is_dir() || !(leaves || dir.starts_with(&ws.root)) {
                return None;
            }
            let named = crate::find::lexical(&logical.join(piece.dir.as_deref().unwrap_or("")));
            let place = (dir.as_path(), named.as_path());
            let mut outputs = Vec::new();
            match &piece.source {
                Source::File {
                    file,
                    select,
                    numbered,
                } => {
                    let (text, source) =
                        files.read(ws, place, file, select, *numbered, &piece.filters)?;
                    status = 0;
                    outputs.push((None, text, Some(source)));
                }
                // `cat` prints each file in turn, so each is its own piece.
                Source::Files(names) => {
                    for name in names {
                        let (text, source) = files.read(
                            ws,
                            place,
                            name,
                            &parse::Filter::All,
                            parse::Numbers::None,
                            &[],
                        )?;
                        let header = format!("cat {}", shlex::try_quote(name).ok()?);
                        outputs.push((Some(header), text, Some(source)));
                    }
                    status = 0;
                }
                Source::Object(spec) => {
                    let (text, source) = files.object(&dir, spec, &piece.filters)?;
                    status = 0;
                    outputs.push((None, text, Some(source)));
                }
                Source::Git(words) => {
                    if repo.as_ref().is_none_or(|(repo_dir, _)| repo_dir != &dir) {
                        repo = Some((dir.clone(), git::Repository::open(ws, &dir, deadline)?));
                    }
                    status = 0;
                    let text = repo.as_ref()?.1.output(&dir, &words[1..], deadline)?;
                    outputs.push((None, reads::filter(text, &piece.filters)?, None));
                }
                Source::Search(words) => {
                    let (code, text) = search(ws, &dir, words, deadline)?;
                    // With pipefail set a successful tail can still report the search's failure.
                    if !piece.filters.is_empty() && code != 0 {
                        return None;
                    }
                    status = code;
                    outputs.push((None, reads::filter(text, &piece.filters)?, None));
                }
                Source::Grep(grep) => {
                    let rest = &actual[output.len().min(actual.len())..];
                    let trimmed = persisted.is_none();
                    let (code, text) =
                        grep::reproduce(ws, place, grep, &piece.filters, rest, trimmed, deadline)?;
                    if !piece.filters.is_empty() && code != 0 {
                        return None;
                    }
                    status = code;
                    outputs.push((None, text, None));
                }
                Source::Literal(text) => {
                    status = 0;
                    outputs.push((None, reads::filter(text.clone(), &piece.filters)?, None));
                }
                Source::Pwd => {
                    let path =
                        crate::find::lexical(&logical.join(piece.dir.as_deref().unwrap_or("")));
                    let text = format!("{}\n", path.to_str()?);
                    status = 0;
                    outputs.push((None, reads::filter(text, &piece.filters)?, None));
                }
                Source::Listing(words) => {
                    let text = listing(ws, &dir, words, deadline)?;
                    status = 0;
                    outputs.push((None, reads::filter(text, &piece.filters)?, None));
                }
            }
            for (header, text, source) in outputs {
                let start = output.len();
                if start + text.len() > MAX_BYTES {
                    return None;
                }
                let first_line = (!text.is_empty()).then_some(line);
                line += text.bytes().filter(|&b| b == b'\n').count();
                let last_line = first_line.map(|_| line - usize::from(text.ends_with('\n')));
                output.push_str(&text);
                let range = RunPiece {
                    command_index: index + 1,
                    start_byte: start,
                    end_byte: output.len(),
                    first_line,
                    last_line,
                };
                verified.push(Verified {
                    piece,
                    header,
                    range,
                    source,
                    dir: dir.clone(),
                });
            }
        }
        let expected_status = response
            .as_object()
            .map_or(0, crate::postroute::response_exit_status);
        if status != expected_status {
            return None;
        }
        // Claude strips trailing LF bytes from inline Bash results. Compare its
        // wire representation exactly; persisted files must match in full.
        if output != actual && (persisted.is_some() || output.trim_end_matches('\n') != actual) {
            return None;
        }
        // Archive only bytes present in the client result. In particular, a
        // recheck cannot establish how many final newlines Claude discarded.
        if actual.len() < output.len() {
            for item in &mut verified {
                let span = &mut item.range;
                if span.end_byte <= actual.len() {
                    continue;
                }
                span.end_byte = actual.len();
                span.start_byte = span.start_byte.min(span.end_byte);
                let kept = &actual[span.start_byte..span.end_byte];
                if kept.is_empty() {
                    span.first_line = None;
                    span.last_line = None;
                } else {
                    span.last_line = span.first_line.map(|n| {
                        n + kept.bytes().filter(|&b| b == b'\n').count()
                            - usize::from(kept.ends_with('\n'))
                    });
                }
            }
        }
        // File reads of at most `read_outline_lines` lines in all stay as
        // printed, as one such read does, unless another piece is large, a file
        // is minified onto one or two huge lines, or the client saved the
        // result to a file and shows only its start.
        if persisted.is_none() {
            let (mut lines, mut other) = (0, 0);
            let mut minified = false;
            for item in &verified {
                let text = &actual[item.range.start_byte..item.range.end_byte];
                match item.piece.source {
                    Source::File { .. } | Source::Files(_) | Source::Object(_) => {
                        let count = text.lines().count() as u64;
                        lines += count;
                        if count <= 2 && text.len() > FLOOR {
                            minified = true;
                        }
                    }
                    _ => other += text.len(),
                }
            }
            if !minified && lines > 0 && lines <= settings.read_outline_lines && other <= FLOOR {
                return None;
            }
        }
        // A small file of a `cat A B …` is shown whole, as its own `cat` would
        // be, outside the budget for the other pieces but within the ceiling.
        let small: Vec<bool> = verified
            .iter()
            .map(|item| {
                let text = &actual[item.range.start_byte..item.range.end_byte];
                matches!(item.piece.source, Source::Files(_))
                    && text.len() <= 6144
                    && text.bytes().filter(|&b| b == b'\n').count() <= 150
            })
            .collect();
        let (text, verbatim) = render(&mut files, &verified, actual, budget, &small)
            .filter(|(text, _)| text.len() <= ceiling)
            .or_else(|| render(&mut files, &verified, actual, budget, &[]))?;
        if Instant::now() >= deadline {
            return None;
        }
        const NOTE: &str = "Verified against Claude Code's Bash result after execution. Piece offsets refer to the archived Claude Code stdout.";
        let origin = Origin {
            command: input["tool_input"]["command"].as_str()?.to_owned(),
            label: "read batch".into(),
            note: NOTE.into(),
            outline: Some(SourceOutline {
                text,
                baseline,
                trim_footer: true,
            }),
        };
        let session = input["session_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| capture::source_digest(s.as_bytes())[..24].to_owned());
        let ranges = verified.iter().map(|v| v.range.clone()).collect::<Vec<_>>();
        let config = crate::redact::for_run(ws).ok()?;
        if config.is_some_and(|r| matches!(r.text(actual), std::borrow::Cow::Owned(_))) {
            return None;
        }
        let result = capture::record_with_pieces(
            &run_ws,
            &[OsString::from("read batch")],
            actual.as_bytes(),
            status,
            settings.context_budget,
            session.as_deref().unwrap_or("terminal"),
            (&origin, &ranges),
        )
        .ok()?;
        let view_cap =
            result.previews.iter().map(|(_, b)| b.len()).sum::<usize>() + result.footer.len();
        let mut view = String::with_capacity(view_cap);
        for (_, bytes) in result.previews {
            view.push_str(std::str::from_utf8(&bytes).ok()?);
        }
        view.push_str(&result.footer);
        let mut updated = response.as_object()?.clone();
        updated.insert("stdout".into(), json!(view));
        for key in [
            "persistedOutputPath",
            "persistedOutputSize",
            "stdoutHoldsOutputEnd",
        ] {
            updated.remove(key);
        }
        // Only a list whose result is replaced records what the agent saw.
        for (item, verbatim) in verified.iter().zip(verbatim) {
            if let Some(source) = &item.source
                && item.range.first_line.is_some()
            {
                files.observe(ws, source, verbatim);
            }
        }
        Some(
            json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":updated}}),
        )
    }
}

fn excerpt(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let half = limit.saturating_sub(10) / 2;
    let head = &text[..text.floor_char_boundary(half)];
    let tail = &text[text.ceil_char_boundary(text.len() - half)..];
    let head = head.rsplit_once('\n').map_or(head, |(h, _)| h);
    let tail = tail
        .split_once('\n')
        .filter(|(_, t)| !t.is_empty())
        .map_or(tail, |(_, t)| t);
    format!("{head}\n…\n{tail}")
}

fn render(
    files: &mut reads::Files,
    items: &[Verified<'_>],
    output: &str,
    budget: usize,
    whole: &[bool],
) -> Option<(String, Vec<bool>)> {
    let whole = |i: usize| whole.get(i).copied().unwrap_or(false);
    let headers: Vec<_> = items
        .iter()
        .map(|item| {
            let header = item.header.as_deref().unwrap_or(item.piece.header);
            match (item.range.first_line, item.range.last_line) {
                (Some(a), Some(b)) => format!("$ {header} [stdout:{a}-{b}]\n"),
                _ => format!("$ {header} [no output]\n"),
            }
        })
        .collect();
    let mut remaining = budget.checked_sub(headers.iter().map(|s| s.len() + 1).sum())?;
    let mut order = (0..items.len()).collect::<Vec<_>>();
    order.sort_by_key(|&i| items[i].range.end_byte - items[i].range.start_byte);
    let mut bodies = vec![String::new(); items.len()];
    // Whether each piece's bytes are shown as they are.
    let mut verbatim = vec![false; items.len()];
    let mut left = (0..items.len()).filter(|&i| !whole(i)).count();
    for i in order {
        let item = &items[i];
        let text = &output[item.range.start_byte..item.range.end_byte];
        if whole(i) {
            bodies[i] = text.to_owned();
            verbatim[i] = true;
            continue;
        }
        let limit = remaining / left;
        left -= 1;
        if !text.is_empty() && limit < 32 && text.len() > limit {
            return None;
        }
        let shaped = if text.len() <= limit {
            None
        } else {
            match &item.piece.source {
                Source::File { .. } | Source::Files(_) | Source::Object(_) => {
                    files.outline(item.source.as_ref()?, text, limit)
                }
                Source::Search(words) | Source::Grep(grep::Grep { words, .. }) => {
                    let args = words.iter().map(String::as_str).collect::<Vec<_>>();
                    let mut engine = crate::search::Search::new(&item.dir, &args);
                    for (n, line) in text.lines().enumerate() {
                        engine.observe(line, item.range.first_line? + n);
                    }
                    engine.finish(&words[0], limit, None, true).map(|(s, _)| s)
                }
                Source::Git(words) if matches!(words[1].as_str(), "show" | "diff") => {
                    let mut engine = crate::summary::Parser::new("git diff", None);
                    for (n, line) in text.lines().enumerate() {
                        engine.observe(line, item.range.first_line? + n);
                    }
                    engine
                        .finish()
                        .render(None, 0)
                        .map(|report| excerpt(&report, limit))
                }
                Source::Git(_) | Source::Literal(_) | Source::Pwd | Source::Listing(_) => None,
            }
        };
        let body = shaped
            .filter(|s| !s.is_empty() && s.len() <= limit && s.len() < text.len())
            .unwrap_or_else(|| excerpt(text, limit));
        remaining = remaining.checked_sub(body.len())?;
        verbatim[i] = text.len() <= limit;
        bodies[i] = body;
    }
    let total_len: usize = headers.iter().map(String::len).sum::<usize>()
        + bodies.iter().map(String::len).sum::<usize>()
        + headers.len();
    let mut out = String::with_capacity(total_len);
    for (header, body) in headers.iter().zip(bodies) {
        out.push_str(header);
        out.push_str(&body);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    Some((out, verbatim))
}

#[cfg(test)]
mod tests {
    use super::Batch;

    #[test]
    fn a_wrapped_single_command_parses() {
        assert!(Batch::parse("timeout 5 sed -n 1,2p Cargo.toml").is_some());
        assert!(Batch::parse("env -u HOME cat Cargo.toml").is_some());
        assert!(Batch::parse("bash -c 'sed -n 1,2p Cargo.toml'").is_some());
        assert!(Batch::parse("sed -n 1,2p Cargo.toml").is_none());
        assert!(Batch::parse("env FOO=$(cat Cargo.toml) cat Cargo.toml").is_none());
        assert!(Batch::parse("env FOO=1 cat *").is_none());
    }

    #[test]
    fn several_files_and_a_last_cat_parse_as_reads() {
        for (command, outputs) in [
            ("cat a b", Some(2)),
            ("timeout 5 cat a b c", Some(3)),
            ("cd x && cat a b | cat", Some(2)),
            ("cat a b; sed -n 1,2p c | cat -", Some(3)),
            ("rg -n x src | cat", Some(1)),
            ("cat a b | head -n 1", None),
            ("cat a b | cut -c 1-3 | cat", None),
            ("cat a | cat | head -n 1", None),
            ("cat a | cat -n", None),
            ("cat -n a b", None),
            ("cat a - b", None),
            ("cat a ../b", None),
            ("cat a .git/config", None),
        ] {
            let batch = Batch::parse(command);
            assert_eq!(
                batch.map(|b| super::parse::outputs(&b.0)),
                outputs,
                "{command}"
            );
        }
        let files: Vec<String> = (0..33).map(|i| format!("f{i}")).collect();
        assert!(Batch::parse(&format!("cat {}", files.join(" "))).is_none());
        assert!(Batch::parse(&format!("cat {}", files[1..].join(" "))).is_some());
    }

    #[test]
    fn a_cd_prefixed_list_parses_in_that_directory() {
        let batch = Batch::parse_list("cd fixtures/mixed && git diff && sed -n 1,2p db/schema.sql")
            .unwrap();
        assert_eq!(batch.0.len(), 2);
        assert!(
            batch
                .0
                .iter()
                .all(|p| p.dir.as_deref() == Some("fixtures/mixed"))
        );
        assert_eq!(batch.0[1].header, "sed -n 1,2p db/schema.sql");

        let batch = Batch::parse_list("cd /repo && cd sub && git diff").unwrap();
        assert_eq!(batch.0.len(), 1);
        assert_eq!(batch.0[0].dir.as_deref(), Some("/repo/sub"));
        assert_eq!(batch.0[0].header, "git diff");

        let batch = Batch::parse_list("cd fixtures/mixed && FOO=1 && git diff").unwrap();
        assert_eq!(batch.0.len(), 1);
        assert_eq!(batch.0[0].dir.as_deref(), Some("fixtures/mixed"));

        assert!(Batch::parse_list("cd ../elsewhere && git diff").is_none());
        assert!(Batch::parse_list("cd .git && git diff").is_none());
        assert!(Batch::parse_list("cd /repo || git diff").is_none());
        assert!(Batch::parse_list("git diff || cd sub && git diff").is_none());
        assert!(Batch::parse_list("rg -n x src && cd sub; git diff").is_none());
        assert!(Batch::parse_list("git diff && cd sub && git diff").is_some());
        assert!(Batch::parse_list("cd /repo && rm -rf x && git diff").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn mixed_batch_compacts_sed_tail_attached_counts_and_config_outline_formats() {
        use std::fs;

        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join(".justless.toml"), "read_outline_lines = 0\n").unwrap();

        let makefile: String = (0..30)
            .map(|i| {
                format!(
                    "target_{i:02}:\n\t@echo building target_{i:02}\n\t@echo step 1 for target_{i:02}\n\t@echo step 2 for target_{i:02}\n\n"
                )
            })
            .collect();
        let dockerfile: String = (0..18)
            .map(|i| {
                format!(
                    "FROM alpine:3.20 AS stage_{i:02}\nWORKDIR /app/stage_{i:02}\nRUN echo step 1 in stage_{i:02}\nRUN echo step 2 in stage_{i:02}\nCOPY . /app/stage_{i:02}\n\n"
                )
            })
            .collect();
        let css: String = (0..35)
            .map(|i| {
                format!(
                    ".component-{i:02} {{\n  display: flex;\n  padding: {i}px;\n  color: #222222;\n}}\n\n"
                )
            })
            .collect();
        let proto: String = std::iter::once("syntax = \"proto3\";\npackage acme;\n\n".to_owned())
            .chain((0..30).map(|i| {
                format!(
                    "message Order{i:02} {{\n  string id = 1;\n  int64 amount_cents = 2;\n  string currency = 3;\n}}\n\n"
                )
            }))
            .collect();
        let graphql: String = (0..30)
            .map(|i| {
                format!(
                    "type Record{i:02} {{\n  id: ID!\n  title: String!\n  createdAt: String!\n}}\n\n"
                )
            })
            .collect();
        let prisma: String = std::iter::once(
            "datasource db {\n  provider = \"postgresql\"\n  url = env(\"DATABASE_URL\")\n}\n\n"
                .to_owned(),
        )
        .chain((0..25).map(|i| {
            format!(
                "model Entity{i:02} {{\n  id Int @id @default(autoincrement())\n  name String\n  active Boolean @default(true)\n}}\n\n"
            )
        }))
        .collect();
        let html: String = format!(
            "<!doctype html>\n<html>\n<head>\n  <title>App</title>\n</head>\n<body>\n{}\n</body>\n</html>\n",
            (0..30)
                .map(|i| format!(
                    "  <section id=\"panel-{i:02}\">\n    <h2>Panel {i:02}</h2>\n    <p>Details for panel {i:02} in dashboard.</p>\n  </section>"
                ))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let mdx: String = (0..25)
            .map(|i| {
                format!(
                    "## Section {i:02}\n\nimport {{ Widget{i:02} }} from './w';\n\nDetailed documentation paragraph for section {i:02}.\n\n"
                )
            })
            .collect();
        let big_payload = "x".repeat(1600);
        let minified_json = format!(
            "{{\"alpha\":{{\"blob\":\"{big_payload}\"}},\"beta\":{{\"blob\":\"{big_payload}\"}},\"gamma\":{{\"blob\":\"{big_payload}\"}},\"delta\":{{\"blob\":\"{big_payload}\"}}}}\n"
        );
        assert!(minified_json.len() > 6000);

        for (name, content) in [
            ("Makefile", &makefile),
            ("Dockerfile", &dockerfile),
            ("styles.css", &css),
            ("api.proto", &proto),
            ("schema.graphql", &graphql),
            ("schema.prisma", &prisma),
            ("index.html", &html),
            ("docs.mdx", &mdx),
            ("bundle.json", &minified_json),
        ] {
            fs::write(root.join(name), content).unwrap();
        }
        fs::write(root.join("note.txt"), "ok\n").unwrap();

        let ws = crate::workspace::Workspace {
            state: root.join(".justless"),
            origin: root.clone(),
            cwd: root.clone(),
            root: root.clone(),
        };

        let sh = |script: &str| {
            let out = std::process::Command::new("/bin/sh")
                .args(["-c", script])
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(out.status.success(), "{script}");
            String::from_utf8(out.stdout).unwrap()
        };

        let compact_list = |script: &str| {
            let raw = sh(script);
            let batch = Batch::parse(script).unwrap_or_else(|| panic!("did not parse: {script}"));
            let input = serde_json::json!({
                "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
                "session_id": "unit-batch",
                "cwd": root,
                "tool_input": { "command": script },
                "tool_response": {
                    "stdout": raw,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }
            });
            let reply = batch
                .compact(&ws, &input)
                .unwrap_or_else(|| panic!("did not compact: {script}"));
            let view = reply["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(view.len() < raw.len(), "{script}: {view}");
            let exact = view.lines().find(|l| l.starts_with("Exact: ")).unwrap();
            let id = exact.split_whitespace().last().unwrap();
            assert_eq!(
                fs::read(ws.state.join("runs").join(id).join("stdout")).unwrap(),
                raw.as_bytes(),
                "archive replay mismatch for {script}"
            );
            view
        };

        // `sed -n 'A,$p'`, attached `head -n50`, `tail -n+50`, and `config_outline` formats.
        let view = compact_list(
            "sed -n '10,$p' Makefile; head -n90 Dockerfile; tail -n+15 styles.css; cat note.txt",
        );
        assert!(view.contains("target target_"), "{view}");
        assert!(view.contains("stage stage_"), "{view}");
        assert!(view.contains("styles.css lines"), "{view}");

        let view = compact_list(
            "sed -ne '5,$p' api.proto; head --lines=100 schema.graphql; tail --lines=+5 schema.prisma",
        );
        assert!(view.contains("Order"), "{view}");
        assert!(view.contains("Record"), "{view}");
        assert!(view.contains("Entity"), "{view}");

        let view = compact_list("cat index.html docs.mdx");
        assert!(view.contains("index.html lines"), "{view}");
        assert!(view.contains("h2 Section "), "{view}");

        // Even under the default 600-line `read_outline_lines` threshold, a huge
        // 1-line minified JSON file in a mixed read list compacts and outlines its keys.
        fs::remove_file(root.join(".justless.toml")).unwrap();
        let view = compact_list("cat bundle.json; cat note.txt");
        assert!(view.contains("key alpha"), "{view}");
        assert!(view.contains("key beta"), "{view}");
        assert!(view.contains("key delta"), "{view}");

        // Redaction safety: a secret in a batch read keeps the output uncompacted.
        let token = format!("{}{}{}", "gh", "p_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
        fs::write(
            root.join("secret.json"),
            minified_json.replace("alpha", &token),
        )
        .unwrap();
        let script = "cat secret.json; cat note.txt";
        let raw = sh(script);
        let batch = Batch::parse(script).unwrap();
        let input = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "session_id": "unit-batch-secret",
            "cwd": root,
            "tool_input": { "command": script },
            "tool_response": {
                "stdout": raw,
                "stderr": "",
                "interrupted": false,
                "isImage": false,
            }
        });
        assert!(batch.compact(&ws, &input).is_none());
    }
}
