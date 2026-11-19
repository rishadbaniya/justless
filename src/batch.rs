//! Claude executes the original list. Only attribute stdout to individual
//! commands when their independently reproduced outputs agree with its capture.
mod git;
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

struct Verified<'a> {
    piece: &'a Piece<'a>,
    range: RunPiece,
    source: Option<reads::SourceLines>,
    dir: PathBuf,
}

impl<'a> Batch<'a> {
    pub(crate) fn parse_list(command: &'a str) -> Option<Self> {
        parse::list(command).map(Self)
    }

    pub(crate) fn parse(command: &'a str) -> Option<Self> {
        Self::parse_list(command).or_else(|| parse::single(command).map(Self))
    }

    pub(crate) fn compact(&self, ws: &Workspace, input: &Value) -> Option<Value> {
        let response = &input["tool_response"];
        if !response["stderr"].as_str()?.is_empty() {
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
        if let Some(dir) = self.0.last().and_then(|piece| piece.dir.as_deref()) {
            let entered: PathBuf = Path::new(dir)
                .components()
                .filter(|c| *c != Component::CurDir)
                .collect();
            if entered.is_relative() && !cwd.join(&entered).is_dir() && cwd.ends_with(&entered) {
                for _ in entered.components() {
                    start.pop();
                }
            }
        }
        let budget = settings
            .context_budget
            .min(baseline * 3 / 4)
            .checked_sub(192 + 2 * run_ws.justless().len())?;
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut files = reads::Files::new();
        let mut repo: Option<(PathBuf, git::Repository)> = None;
        let mut status = 0;
        let mut may_have_closed_pipe = false;
        let mut output = String::new();
        let mut verified = Vec::new();
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
            // The workspace root is already canonical.
            if !dir.is_dir() || !dir.starts_with(&ws.root) {
                return None;
            }
            let (text, source) = match &piece.source {
                Source::File {
                    file,
                    select,
                    numbered,
                } => {
                    let (text, source) =
                        files.read(ws, &dir, file, select, *numbered, &piece.filters)?;
                    status = 0;
                    (text, Some(source))
                }
                Source::Object(spec) => {
                    let (text, source) = files.object(&dir, spec, &piece.filters)?;
                    status = 0;
                    (text, Some(source))
                }
                Source::Git(words) => {
                    if repo.as_ref().is_none_or(|(repo_dir, _)| repo_dir != &dir) {
                        repo = Some((dir.clone(), git::Repository::open(ws, &dir, deadline)?));
                    }
                    status = 0;
                    (
                        reads::filter(
                            repo.as_ref()?.1.output(&dir, &words[1..], deadline)?,
                            &piece.filters,
                        )?,
                        None,
                    )
                }
                Source::Search(words) => {
                    let (code, text) = search(ws, &dir, words, deadline)?;
                    // With pipefail set a successful tail can still report the search's failure.
                    if !piece.filters.is_empty() && code != 0 {
                        return None;
                    }
                    status = code;
                    (reads::filter(text, &piece.filters)?, None)
                }
            };
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
                range,
                source,
                dir,
            });
        }
        let expected_status = i32::from(
            response["returnCodeInterpretation"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
        );
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
        let (text, verbatim) = render(&mut files, &verified, actual, budget)?;
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
        let mut view = String::new();
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
) -> Option<(String, Vec<bool>)> {
    let headers: Vec<_> = items
        .iter()
        .map(|item| {
            let span = match (item.range.first_line, item.range.last_line) {
                (Some(a), Some(b)) => format!(" [stdout:{a}-{b}]"),
                _ => " [no output]".into(),
            };
            format!("$ {}{span}\n", item.piece.header)
        })
        .collect();
    let mut remaining = budget.checked_sub(headers.iter().map(|s| s.len() + 1).sum())?;
    let mut order = (0..items.len()).collect::<Vec<_>>();
    order.sort_by_key(|&i| items[i].range.end_byte - items[i].range.start_byte);
    let mut bodies = vec![String::new(); items.len()];
    // Whether each piece's bytes are shown as they are.
    let mut verbatim = vec![false; items.len()];
    for (pos, i) in order.into_iter().enumerate() {
        let item = &items[i];
        let limit = remaining / (items.len() - pos);
        let text = &output[item.range.start_byte..item.range.end_byte];
        if !text.is_empty() && limit < 32 && text.len() > limit {
            return None;
        }
        let shaped = if text.len() <= limit {
            None
        } else {
            match &item.piece.source {
                Source::File { .. } | Source::Object(_) => {
                    files.outline(item.source.as_ref()?, text, limit)
                }
                Source::Search(words) => {
                    let args = words.iter().map(String::as_str).collect::<Vec<_>>();
                    let mut engine = crate::search::Search::new(&item.dir, &args);
                    for (n, line) in text.lines().enumerate() {
                        engine.observe(line, item.range.first_line? + n);
                    }
                    engine.finish(&words[0], limit, None, true).map(|(s, _)| s)
                }
                Source::Git(_) => {
                    let mut engine = crate::summary::Parser::new("git diff", None);
                    for (n, line) in text.lines().enumerate() {
                        engine.observe(line, item.range.first_line? + n);
                    }
                    engine
                        .finish()
                        .render(None, 0)
                        .map(|report| excerpt(&report, limit))
                }
            }
        };
        let body = shaped
            .filter(|s| !s.is_empty() && s.len() <= limit && s.len() < text.len())
            .unwrap_or_else(|| excerpt(text, limit));
        remaining = remaining.checked_sub(body.len())?;
        verbatim[i] = text.len() <= limit;
        bodies[i] = body;
    }
    let mut out = String::new();
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
}
