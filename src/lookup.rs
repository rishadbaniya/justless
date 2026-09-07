//! `justless lookup QUESTION`: the few symbols, Markdown sections and
//! configuration blocks most likely to answer a question about the repository.
//!
//! Each file is split along the symbols the index's parsers find, Markdown
//! headings, `[section]` headers and fixed windows. Chunks are ranked by a
//! field-weighted relevance score over identifier-aware words, where a match
//! in a name counts more than one in a signature, doc comment or body.

mod chunks;
mod logs;
mod score;
mod segment;
mod store;
mod tokens;

use std::fs;
use std::io::{self, Write};

use anyhow::{Result, anyhow};
use clap::{Arg, ArgMatches, Command};
use serde_json::{Value, json};

use crate::capture::{self, Stream};
use crate::workspace::{Location, Workspace};
use chunks::Class;
use score::{Part, Scores};
use tokens::Question;

const MARKER: &str = "[context budget reached]\n";
/// The longest result `expand` is offered for, because it has no budget.
const EXPAND_LINES: usize = 80;
const EXCERPT_LINES: usize = 40;
const MAX_LINE: usize = 160;
const LEAD: usize = 60;
const MAX_WORDS: usize = 120;

pub fn command() -> Command<'static> {
    Command::new("lookup")
        .about("Rank the code, docs and config that best answer a question")
        .arg(
            Arg::new("question")
                .required(true)
                .multiple_values(true)
                .value_name("QUESTION"),
        )
        .arg(crate::number("limit", "8", "Maximum results"))
        .arg(
            Arg::new("json")
                .long("json")
                .help("Print structured results"),
        )
        .arg(
            Arg::new("logs")
                .long("logs")
                .help("Also rank output captured by justless run"),
        )
        .arg(crate::budget())
}

/// Where a result came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Code,
    Doc,
    Text,
    Data,
    Log,
}

impl Source {
    fn from_class(class: Class) -> Self {
        match class {
            Class::Code => Self::Code,
            Class::Doc => Self::Doc,
            Class::Text => Self::Text,
            Class::Data => Self::Data,
            Class::Log => Self::Log,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Code => Class::Code.source(),
            Self::Doc => Class::Doc.source(),
            Self::Text => Class::Text.source(),
            Self::Data => Class::Data.source(),
            Self::Log => Class::Log.source(),
        }
    }
}

pub(crate) struct Options {
    pub limit: usize,
    pub logs: bool,
}

/// One ranked place. For a log result, `location.file` is the run ID and
/// `run` names the run and stream.
pub(crate) struct Hit {
    pub source: Source,
    pub location: Location,
    pub run: Option<(String, Stream)>,
    /// The exit status of a log result's run.
    pub status: Option<i32>,
    pub kind: String,
    pub name: String,
    pub score: f64,
    /// Up to two best-matching lines, in line order.
    pub lines: Vec<(usize, String)>,
}

pub(crate) struct Ranking {
    pub hits: Vec<Hit>,
    /// Live chunks with a score above zero.
    pub total: usize,
    pub matched: Vec<String>,
    pub unmatched: Vec<String>,
    /// The question had more terms than are used.
    pub truncated: bool,
}

#[derive(Clone, Copy)]
struct Candidate {
    /// 0 for source, 1 for logs.
    group: usize,
    part: usize,
    chunk: u32,
    score: f64,
}

/// Ranks chunks for `question`, refreshing the lookup store first.
pub(crate) fn rank(workspace: &Workspace, question: &str, options: &Options) -> Result<Ranking> {
    let question =
        Question::parse(question).ok_or_else(|| anyhow!("enter a question or keywords"))?;
    let loaded = store::load(workspace, options.logs)?;
    let mut groups = vec![
        loaded
            .source
            .iter()
            .map(|(segment, live)| Part { segment, live })
            .collect::<Vec<_>>(),
    ];
    if let Some((segment, live)) = &loaded.logs {
        groups.push(vec![Part { segment, live }]);
    }
    let scores = groups
        .iter()
        .map(|parts| score::score(parts, &question))
        .collect::<Result<Vec<Scores>>>()?;
    let file = |candidate: &Candidate| {
        let segment = groups[candidate.group][candidate.part].segment;
        let row = segment.row(candidate.chunk);
        (&segment.files()[row.file as usize], row)
    };
    let compare = |left: &Candidate, right: &Candidate| {
        let (left_file, left_row) = file(left);
        let (right_file, right_row) = file(right);
        let segment = |candidate: &Candidate| groups[candidate.group][candidate.part].segment;
        right
            .score
            .total_cmp(&left.score)
            .then(left.group.cmp(&right.group))
            .then_with(|| left_file.cmp(right_file))
            .then(left_row.start.cmp(&right_row.start))
            .then(left_row.end.cmp(&right_row.end))
            .then_with(|| {
                segment(left)
                    .name(&left_row)
                    .cmp(segment(right).name(&right_row))
            })
            .then_with(|| {
                segment(left)
                    .kind(&left_row)
                    .cmp(segment(right).kind(&right_row))
            })
    };
    let candidates = scores
        .iter()
        .enumerate()
        .flat_map(|(group, scores)| {
            scores.scored.iter().map(move |scored| Candidate {
                group,
                part: scored.part,
                chunk: scored.chunk,
                score: scored.score,
            })
        })
        .collect::<Vec<_>>();
    let total = candidates.len();
    let chosen = score::select(candidates, options.limit, compare, |candidate| {
        (candidate.group, file(candidate).0.as_str())
    });
    let hits = chosen
        .iter()
        .map(|candidate| {
            let segment = groups[candidate.group][candidate.part].segment;
            let idf = &scores[candidate.group].idf;
            hit(workspace, segment, candidate, &question, idf)
        })
        .collect();
    let mut matched = Vec::new();
    let mut unmatched = Vec::new();
    for word in &question.words {
        let found = word
            .terms
            .iter()
            .any(|&term| scores.iter().any(|scores| scores.df[term] > 0));
        if found {
            matched.push(word.text.clone());
        } else {
            unmatched.push(word.text.clone());
        }
    }
    Ok(Ranking {
        hits,
        total,
        matched,
        unmatched,
        truncated: question.truncated,
    })
}

fn hit(
    workspace: &Workspace,
    segment: &segment::Segment,
    candidate: &Candidate,
    question: &Question,
    idf: &[f64],
) -> Hit {
    let row = segment.row(candidate.chunk);
    let class = row.class();
    let file = &segment.files()[row.file as usize];
    let (start, end) = (row.start as usize, row.end as usize);
    let kind = if row.test() {
        "test".to_owned()
    } else {
        segment.kind(&row).to_owned()
    };
    let mut hit = Hit {
        source: Source::from_class(class),
        location: Location {
            file: file.clone(),
            start,
            end,
        },
        run: None,
        status: None,
        kind,
        name: segment.name(&row).to_owned(),
        score: candidate.score,
        lines: Vec::new(),
    };
    let (text, candidates): (Vec<String>, Vec<usize>) = if class == Class::Log {
        let Some((run, stream)) = logs::parse_key(file) else {
            return hit;
        };
        hit.location.file = run.to_owned();
        hit.run = Some((run.to_owned(), stream));
        let Ok((directory, record)) = capture::load(workspace, run) else {
            return hit;
        };
        hit.status = Some(record.status);
        let lines = logs::read_lines(&directory.join(stream.name()), end);
        (lines, (start..=end).collect())
    } else {
        let Some(text) = workspace
            .source_path(file)
            .ok()
            .and_then(|path| fs::read_to_string(path).ok())
        else {
            return hit;
        };
        let others = segment.ranges()[row.file as usize]
            .clone()
            .filter(|&id| id != candidate.chunk)
            .map(|id| {
                let other = segment.row(id);
                (other.start as usize, other.end as usize)
            });
        let indexed = chunks::indexed_lines(class, (start, end), others);
        (text.lines().map(str::to_owned).collect(), indexed)
    };
    let lines = candidates
        .iter()
        .filter_map(|&number| Some((number, text.get(number.checked_sub(1)?)?.as_str())))
        .collect::<Vec<_>>();
    let mut best = score::best_lines(&lines, question, idf);
    if best.is_empty() {
        let declaration = row.declaration as usize;
        let fallback = if class == Class::Code && candidates.contains(&declaration) {
            Some(declaration)
        } else {
            lines
                .iter()
                .find(|(_, line)| !line.trim().is_empty())
                .map(|(number, _)| *number)
        };
        best.extend(fallback.map(|number| (number, None)));
    }
    hit.lines = best
        .into_iter()
        .filter_map(|(number, first)| Some((number, excerpt(text.get(number - 1)?, first))))
        .collect();
    hit
}

/// The trimmed line, clipped to 160 bytes. A longer line is windowed from 60
/// bytes before its first matching word.
fn excerpt(line: &str, first: Option<usize>) -> String {
    let trimmed = line.trim();
    if trimmed.len() <= MAX_LINE {
        return trimmed.to_owned();
    }
    let indent = line.len() - line.trim_start().len();
    let anchor = first.map_or(0, |first| first.saturating_sub(indent));
    let start = trimmed.floor_char_boundary(anchor.saturating_sub(LEAD).min(trimmed.len()));
    let prefix = if start > 0 { "…" } else { "" };
    let room = MAX_LINE - prefix.len();
    if trimmed.len() - start <= room {
        return format!("{prefix}{}", &trimmed[start..]);
    }
    let end = trimmed.floor_char_boundary(start + room - '…'.len_utf8());
    format!("{prefix}{}…", &trimmed[start..end])
}

fn quoted(file: &str) -> String {
    shlex::try_quote(file).map_or_else(|_| file.to_owned(), |quoted| quoted.into_owned())
}

/// The command that opens the top result: exact bytes for a short source
/// result, a budgeted excerpt for a longer one, or the log lines.
fn next(hit: &Hit) -> String {
    let Location { file, start, end } = &hit.location;
    if let Some((run, stream)) = &hit.run {
        return format!(
            "justless log {run} --stream {} --from {start} --to {end}",
            stream.name()
        );
    }
    if end - start < EXPAND_LINES {
        return format!("justless expand {} L{start}-{end}", quoted(file));
    }
    let first = hit.lines.first().map_or(*start, |(line, _)| *line);
    let from = (*start).max(first.saturating_sub(3));
    let to = (*end).min(from + EXCERPT_LINES - 1);
    format!("justless code {} --from {from} --to {to}", quoted(file))
}

fn block(hit: &Hit) -> String {
    let mut block = match &hit.run {
        Some((run, stream)) => {
            let status = hit
                .status
                .map(|status| format!(" · exit {status}"))
                .unwrap_or_default();
            format!(
                "{run} {}:{}-{} log {}{status}\n",
                stream.name(),
                hit.location.start,
                hit.location.end,
                hit.name
            )
        }
        None => format!("{} {} {}\n", hit.location, hit.kind, hit.name),
    };
    for (line, text) in &hit.lines {
        block.push_str(&format!("  {line} | {text}\n"));
    }
    block
}

fn words(words: &[String], limit: usize) -> String {
    chunks::clip(&words.join(", "), limit)
}

fn counts(ranking: &Ranking, shown: usize, limit: Option<usize>) -> String {
    let mut line = format!("{shown} of {} ranked", ranking.total);
    if let Some(limit) = limit {
        line.push_str(&format!(" · matched {}", words(&ranking.matched, limit)));
        if !ranking.unmatched.is_empty() {
            line.push_str(&format!(" · no match {}", words(&ranking.unmatched, limit)));
        }
    }
    if ranking.truncated {
        line.push_str(&format!(" · first {} terms used", tokens::MAX_TERMS));
    }
    line.push('\n');
    line
}

fn render_text(ranking: &Ranking, budget: usize) -> String {
    if ranking.hits.is_empty() {
        let mut limit = MAX_WORDS;
        loop {
            let line = format!(
                "No ranked matches for: {}. Use justless find TEXT for exact text.\n",
                words(&ranking.unmatched, limit)
            );
            if line.len() <= budget || limit <= 8 {
                return line;
            }
            limit -= 8;
        }
    }
    let blocks = ranking.hits.iter().map(block).collect::<Vec<_>>();
    let next = format!("Next: {}\n", next(&ranking.hits[0]));
    let all = counts(ranking, blocks.len(), Some(MAX_WORDS));
    if blocks.iter().map(String::len).sum::<usize>() + all.len() + next.len() <= budget {
        return blocks.concat() + &all + &next;
    }
    // Reserve the footer, then add whole results in rank order while they fit.
    let shown = ranking.hits.len();
    let mut limit = Some(MAX_WORDS);
    let mut next = Some(next);
    let footer = |limit, next: &Option<String>| {
        counts(ranking, shown, limit).len() + MARKER.len() + next.as_ref().map_or(0, String::len)
    };
    if footer(limit, &next) > budget {
        next = None;
    }
    while footer(limit, &next) > budget {
        limit = match limit {
            Some(words) if words > 16 => Some(words - 8),
            _ => None,
        };
        if limit.is_none() {
            break;
        }
    }
    let mut used = footer(limit, &next);
    let mut shown = 0;
    for block in &blocks {
        if used + block.len() > budget {
            break;
        }
        used += block.len();
        shown += 1;
    }
    if shown == 0 {
        next = None;
    }
    let mut output = blocks[..shown].concat();
    output.push_str(&counts(ranking, shown, limit));
    output.push_str(MARKER);
    output.push_str(next.as_deref().unwrap_or_default());
    output
}

fn result_json(hit: &Hit) -> Value {
    let lines = hit
        .lines
        .iter()
        .map(|(line, text)| json!({"line": line, "text": text}))
        .collect::<Vec<_>>();
    let score = (hit.score * 1000.0).round() / 1000.0;
    let mut result = json!({"source": hit.source.name()});
    match &hit.run {
        Some((run, stream)) => {
            result["run"] = json!(run);
            result["stream"] = json!(stream.name());
            result["status"] = json!(hit.status);
        }
        None => result["file"] = json!(hit.location.file),
    }
    result["start"] = json!(hit.location.start);
    result["end"] = json!(hit.location.end);
    result["kind"] = json!(hit.kind);
    result["name"] = json!(hit.name);
    result["score"] = json!(score);
    result["lines"] = json!(lines);
    result
}

fn render_json(question: &str, ranking: &Ranking, budget: usize) -> Result<String> {
    let results = ranking
        .hits
        .iter()
        .map(|hit| serde_json::to_string(&result_json(hit)))
        .collect::<serde_json::Result<Vec<_>>>()?;
    let envelope = |kept: usize| {
        json!({
            "query": question,
            "matched": ranking.matched,
            "unmatched": ranking.unmatched,
            "total": ranking.total,
            "omitted": ranking.hits.len() - kept,
            "next": ranking.hits.first().filter(|_| kept > 0).map(next),
            "results": [],
        })
        .to_string()
    };
    let mut kept = results.len();
    loop {
        let base = envelope(kept);
        let size = base.len()
            + results[..kept].iter().map(String::len).sum::<usize>()
            + kept.saturating_sub(1)
            + 1;
        if size <= budget || kept == 0 {
            let body = format!("[{}]", results[..kept].join(","));
            let base = base
                .strip_suffix("[]}")
                .expect("results come last in the envelope");
            return Ok(format!("{base}{body}}}\n"));
        }
        kept -= 1;
    }
}

pub fn run(workspace: &Workspace, matches: &ArgMatches) -> Result<()> {
    let question = matches
        .values_of("question")
        .unwrap_or_default()
        .collect::<Vec<_>>()
        .join(" ");
    let limit = crate::read_num(matches, "limit", 1)?;
    let budget = crate::context_budget(workspace, matches)?;
    let options = Options {
        limit,
        logs: matches.is_present("logs"),
    };
    let ranking = rank(workspace, &question, &options)?;
    let output = if matches.is_present("json") {
        render_json(&question, &ranking, budget)?
    } else {
        render_text(&ranking, budget)
    };
    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(file: &str, start: usize, end: usize, lines: &[(usize, &str)]) -> Hit {
        Hit {
            source: Source::Code,
            location: Location {
                file: file.into(),
                start,
                end,
            },
            run: None,
            status: None,
            kind: "function".into(),
            name: "retry".into(),
            score: 1.23456,
            lines: lines
                .iter()
                .map(|(line, text)| (*line, (*text).to_owned()))
                .collect(),
        }
    }

    fn ranking(hits: Vec<Hit>) -> Ranking {
        Ranking {
            total: hits.len() + 3,
            hits,
            matched: vec!["retries".into()],
            unmatched: vec!["configured".into()],
            truncated: false,
        }
    }

    #[test]
    fn next_offers_expand_only_for_short_results() {
        assert_eq!(
            next(&hit("src/a b.rs", 5, 84, &[])),
            "justless expand 'src/a b.rs' L5-84"
        );
        assert_eq!(
            next(&hit("src/a.rs", 5, 85, &[(50, "x")])),
            "justless code src/a.rs --from 47 --to 85"
        );
        assert_eq!(
            next(&hit("src/a.rs", 5, 300, &[(6, "x")])),
            "justless code src/a.rs --from 5 --to 44"
        );
        let mut log = hit("run-1", 1, 40, &[]);
        log.run = Some(("run-1".into(), Stream::Stderr));
        assert_eq!(
            next(&log),
            "justless log run-1 --stream stderr --from 1 --to 40"
        );
    }

    #[test]
    fn long_lines_are_windowed_around_the_match() {
        assert_eq!(excerpt("  short  \r", None), "short");
        let line = format!("    {}needle{}", "a ".repeat(100), " b".repeat(100));
        let clipped = excerpt(&line, Some(204));
        assert!(clipped.len() <= MAX_LINE, "{}", clipped.len());
        assert!(clipped.starts_with('…') && clipped.ends_with('…'));
        assert!(clipped.contains("needle"));
        let head = excerpt(&"é".repeat(100), None);
        assert!(head.len() <= MAX_LINE && head.ends_with('…'));
    }

    #[test]
    fn text_output_keeps_whole_results_within_the_budget() {
        let hits = (0..5)
            .map(|index| {
                hit(
                    "src/retry.rs",
                    index * 10 + 1,
                    index * 10 + 5,
                    &[(index * 10 + 2, "let retries = 3;")],
                )
            })
            .collect();
        let ranking = ranking(hits);
        let full = render_text(&ranking, 100_000);
        assert!(
            full.ends_with("Next: justless expand src/retry.rs L1-5\n"),
            "{full}"
        );
        assert!(full.contains("5 of 8 ranked · matched retries · no match configured\n"));
        for budget in [128, 200, 300, full.len() - 1] {
            let output = render_text(&ranking, budget);
            assert!(output.len() <= budget, "{budget}: {output}");
            assert!(output.contains(MARKER), "{output}");
        }
        assert_eq!(render_text(&ranking, full.len()), full);
        let small = render_text(&ranking, 128);
        assert_eq!(
            small,
            "0 of 8 ranked · matched retries · no match configured\n[context budget reached]\n"
        );
    }

    #[test]
    fn json_drops_trailing_results_for_the_budget() {
        let hits = (0..4)
            .map(|index| {
                hit(
                    "src/retry.rs",
                    index + 1,
                    index + 1,
                    &[(index + 1, "retry")],
                )
            })
            .collect();
        let ranking = ranking(hits);
        let full = render_json("retries", &ranking, 100_000).unwrap();
        let value: Value = serde_json::from_str(&full).unwrap();
        assert_eq!(value["omitted"], 0);
        assert_eq!(value["results"].as_array().unwrap().len(), 4);
        assert_eq!(value["results"][0]["score"], json!(1.235));
        assert_eq!(value["next"], "justless expand src/retry.rs L1-1");
        let small = render_json("retries", &ranking, full.len() - 1).unwrap();
        assert!(small.len() < full.len());
        let value: Value = serde_json::from_str(&small).unwrap();
        assert_eq!(value["omitted"], 1);
        let none = render_json("retries", &ranking, 128).unwrap();
        let value: Value = serde_json::from_str(&none).unwrap();
        assert_eq!(value["omitted"], 4);
        assert_eq!(value["next"], Value::Null);
        assert_eq!(value["total"], 7);
    }

    #[test]
    fn counts_clip_word_lists_and_note_dropped_terms() {
        let mut ranking = ranking(vec![hit("src/a.rs", 1, 2, &[])]);
        ranking.unmatched = (0..40).map(|index| format!("missing{index}")).collect();
        ranking.truncated = true;
        let line = counts(&ranking, 1, Some(MAX_WORDS));
        let unmatched = line
            .split(" · no match ")
            .nth(1)
            .unwrap()
            .split(" · ")
            .next()
            .unwrap();
        assert!(
            unmatched.len() <= MAX_WORDS && unmatched.ends_with('…'),
            "{line}"
        );
        assert!(line.ends_with(" · first 64 terms used\n"), "{line}");
        let output = render_text(&ranking, 128);
        assert!(output.len() <= 128, "{output}");
        assert!(output.ends_with(MARKER), "{output}");
        let empty = Ranking {
            hits: Vec::new(),
            total: 0,
            matched: Vec::new(),
            unmatched: (0..40).map(|index| format!("missing{index}")).collect(),
            truncated: false,
        };
        let output = render_text(&empty, 128);
        assert!(output.len() <= 128, "{output}");
        assert!(
            output.starts_with("No ranked matches for: missing0, "),
            "{output}"
        );
    }
}
