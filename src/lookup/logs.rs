//! Output captured by `justless run`, ranked in 40-line windows when lookup
//! is given `--logs`.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use super::chunks::{BODY, Chunk, Class, NAME, SIG};
use super::segment::Builder;
use crate::capture::{self, Run, Stream};
use crate::workspace::Workspace;

/// The same terminal escape pattern `capture` strips from previews.
static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").expect("ANSI pattern"));

/// Only the newest runs are ranked.
pub(crate) const MAX_RUNS: usize = 200;
const WINDOW: usize = 40;
const MAX_LINES: usize = 80_000;
const MAX_TOKENIZED: usize = 4 * 1024;
const MAX_LINE: usize = 1024 * 1024;

/// The newest completed runs, newest first.
pub(crate) fn recent(workspace: &Workspace) -> Vec<Run> {
    crate::history::records(workspace, false)
        .map(|records| records.filter_map(Result::ok).take(MAX_RUNS).collect())
        .unwrap_or_default()
}

/// The segment file name for one stream of a run.
pub(crate) fn key(run: &str, stream: Stream) -> String {
    format!("{run}\0{}", stream.name())
}

/// The run ID in a segment file name.
pub(crate) fn run_of(key: &str) -> &str {
    key.split('\0').next().unwrap_or(key)
}

/// The run ID and stream in a segment file name.
pub(crate) fn parse_key(key: &str) -> Option<(&str, Stream)> {
    let (run, stream) = key.split_once('\0')?;
    let stream = match stream {
        "stdout" => Stream::Stdout,
        "stderr" => Stream::Stderr,
        _ => return None,
    };
    Some((run, stream))
}

/// Up to `limit` lines of a captured stream as text without terminal escapes.
pub(crate) fn read_lines(path: &Path, limit: usize) -> Vec<String> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut reader = BufReader::new(file);
    let mut lines = Vec::new();
    let mut raw = Vec::new();
    while lines.len() < limit {
        raw.clear();
        match reader
            .by_ref()
            .take(MAX_LINE as u64)
            .read_until(b'\n', &mut raw)
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if !raw.ends_with(b"\n") && raw.len() == MAX_LINE {
            // Skip the rest of an enormous line.
            let _ = reader.by_ref().skip_until(b'\n');
        }
        let text = String::from_utf8_lossy(&raw);
        let text = ANSI.replace_all(text.trim_end_matches(['\n', '\r']), "");
        lines.push(text.into_owned());
    }
    lines
}

/// Adds both streams of a run to `builder`, stderr first to keep path order.
pub(crate) fn add(workspace: &Workspace, run: &Run, builder: &mut Builder) {
    let Ok((directory, _)) = capture::load(workspace, &run.id) else {
        return;
    };
    let family = if run.family.is_empty() {
        "other"
    } else {
        run.family.as_str()
    };
    for stream in [Stream::Stderr, Stream::Stdout] {
        let lines = read_lines(&directory.join(stream.name()), MAX_LINES);
        let mut chunks = Vec::new();
        for (window, group) in lines.chunks(WINDOW).enumerate() {
            if group.iter().all(|line| line.trim().is_empty()) {
                continue;
            }
            let start = window * WINDOW + 1;
            let mut chunk = Chunk {
                start,
                end: start + group.len() - 1,
                declaration: start,
                name: family.to_owned(),
                kind: "log".into(),
                test: false,
                class: Class::Log,
                fields: Default::default(),
            };
            chunk.fields[NAME].push(Cow::Borrowed(family));
            chunk.fields[SIG].push(Cow::Borrowed(family));
            for line in group {
                let end = line.floor_char_boundary(MAX_TOKENIZED);
                chunk.fields[BODY].push(Cow::Borrowed(&line[..end]));
            }
            chunks.push(chunk);
        }
        builder.add(&key(&run.id, stream), &chunks, false);
    }
}
