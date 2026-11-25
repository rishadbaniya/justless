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
mod snippet;
mod store;
mod tokens;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};

use anyhow::{Result, anyhow};
use clap::{Arg, ArgMatches, Command};
use serde_json::{Value, json};

use crate::capture::{self, Stream};
use crate::workspace::{Invocation, Location, Workspace};
use chunks::Class;
use score::{Part, Picked, Scores};
use tokens::Question;

const MARKER: &str = "[context budget reached]\n";
const EXCERPT_LINES: usize = 40;
/// The most log windows `--logs` adds.
const LOG_RESULTS: usize = 3;
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
    /// The size of the whole result in bytes, when its file could be read.
    pub bytes: Option<usize>,
    /// Other files with the same lines, because their contents are identical.
    pub same: Vec<String>,
}

pub(crate) struct Ranking {
    pub hits: Vec<Hit>,
    /// Live chunks with a score above zero.
    pub total: usize,
    pub matched: Vec<String>,
    pub unmatched: Vec<String>,
    /// The question had more terms than are used.
    pub truncated: bool,
    /// How the printed commands start, so they reach this workspace.
    pub justless: Invocation,
    /// With `--logs`, the matching log windows per stream.
    pub logs: Option<capture::Tally>,
}

const SOURCE: usize = 0;
const LOGS: usize = 1;

#[derive(Clone, Copy)]
struct Candidate {
    /// [`SOURCE`] or [`LOGS`].
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
    // Postings are read only while scoring, so damage there shows up here.
    match rank_in(workspace, &loaded, &question, options) {
        Err(error) if segment::is_corrupt(&error) => {
            drop(loaded);
            eprintln!("lookup index was damaged and has been rebuilt");
            let loaded = store::rebuild(workspace, options.logs)?;
            rank_in(workspace, &loaded, &question, options)
        }
        result => result,
    }
}

fn rank_in(
    workspace: &Workspace,
    loaded: &store::Loaded,
    question: &Question,
    options: &Options,
) -> Result<Ranking> {
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
        .map(|parts| score::score(parts, question))
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
    // Source files with the same contents give the same lines at the same
    // place, so one result stands for all of them, and they share the
    // two-per-file limit as one file.
    let digest = |candidate: &Candidate| {
        let digest = loaded.digest(file(candidate).0)?;
        (candidate.group == SOURCE).then_some(*digest)
    };
    let copy = |candidate: &Candidate| {
        let row = file(candidate).1;
        Some((digest(candidate)?, row.start, row.end))
    };
    let contents = |candidate: &Candidate| match digest(candidate) {
        Some(digest) => (Some(digest), ""),
        None => (None, file(candidate).0.as_str()),
    };
    let overlaps = |left: &Candidate, right: &Candidate| {
        let left_seg = groups[left.group][left.part].segment;
        let right_seg = groups[right.group][right.part].segment;
        let left_row = left_seg.row(left.chunk);
        let right_row = right_seg.row(right.chunk);
        if left_row.start <= right_row.end && right_row.start <= left_row.end {
            return true;
        }
        let adjacent = left_row.end.saturating_add(1) == right_row.start
            || right_row.end.saturating_add(1) == left_row.start;
        adjacent
            && left_seg.kind(&left_row) == "text"
            && right_seg.kind(&right_row) == "text"
            && left_seg.name(&left_row) == right_seg.name(&right_row)
    };
    let mut total = 0;
    let mut chosen = Vec::<Picked<Candidate>>::new();
    let mut logs = None;
    // Log scores use their own statistics, so logs are not ranked against
    // source. The few best log windows come first, because they were asked
    // for, and take no places from source results.
    for group in (0..scores.len()).rev() {
        let candidates = scores[group]
            .scored
            .iter()
            .map(|scored| Candidate {
                group,
                part: scored.part,
                chunk: scored.chunk,
                score: scored.score,
            })
            .collect::<Vec<_>>();
        total += candidates.len();
        if group != LOGS {
            chosen.extend(score::select(
                candidates,
                options.limit,
                compare,
                contents,
                copy,
                overlaps,
            ));
            continue;
        }
        let stream = |candidate: &Candidate| logs::parse_key(file(candidate).0).map(|key| key.1);
        let mut found = capture::Tally::default();
        for candidate in &candidates {
            found.found(stream(candidate).unwrap_or(Stream::Stdout), 1);
        }
        logs = Some(found);
        let limit = LOG_RESULTS.min(options.limit);
        let mut picked = score::select(
            candidates.clone(),
            limit,
            compare,
            contents,
            copy,
            |_, _| false,
        );
        // A stream whose windows all rank below the other stream's still keeps
        // a place, such as the one stderr line that explains a failure under
        // pages of progress. It takes the place of the other stream's last and
        // is listed second, so a budget that leaves out the last windows still
        // shows both streams.
        for missing in [Stream::Stderr, Stream::Stdout] {
            if picked.len() < 2
                || picked
                    .iter()
                    .any(|pick| stream(&pick.item) == Some(missing))
            {
                continue;
            }
            let Some(best) = candidates
                .iter()
                .filter(|candidate| stream(candidate) == Some(missing))
                .min_by(|left, right| compare(left, right))
            else {
                continue;
            };
            picked.pop();
            picked.insert(
                1,
                Picked {
                    item: *best,
                    copies: Vec::new(),
                    merged: Vec::new(),
                },
            );
        }
        chosen.extend(picked);
    }
    let mut seen_snippets = HashMap::<String, (HashSet<usize>, HashSet<String>)>::new();
    let hits = chosen
        .iter()
        .map(|picked| {
            let candidate = &picked.item;
            let segment = groups[candidate.group][candidate.part].segment;
            let primary_file = file(candidate).0;
            let same_file_merged = picked
                .merged
                .iter()
                .filter(|other| {
                    other.group == candidate.group
                        && other.part == candidate.part
                        && file(other).0 == primary_file
                })
                .copied()
                .collect::<Vec<_>>();
            let idf = &scores[candidate.group].idf;
            let empty_nums = HashSet::new();
            let empty_texts = HashSet::new();
            let (excluded_numbers, excluded_texts) = seen_snippets
                .get(primary_file)
                .map_or((&empty_nums, &empty_texts), |(nums, texts)| (nums, texts));
            let mut hit = hit(
                workspace,
                segment,
                candidate,
                &same_file_merged,
                question,
                idf,
                excluded_numbers,
                excluded_texts,
            );
            if hit.run.is_none() {
                let entry = seen_snippets.entry(hit.location.file.clone()).or_default();
                for (number, text) in &hit.lines {
                    entry.0.insert(*number);
                    entry.1.insert(text.trim().to_owned());
                }
            }
            for copy in &picked.copies {
                let other = file(copy).0;
                if *other != hit.location.file && !hit.same.contains(other) {
                    hit.same.push(other.clone());
                }
            }
            hit
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
        justless: workspace.invocation(),
        logs,
    })
}

#[allow(clippy::too_many_arguments)]
fn hit(
    workspace: &Workspace,
    segment: &segment::Segment,
    candidate: &Candidate,
    merged: &[Candidate],
    question: &Question,
    idf: &[f64],
    excluded_numbers: &HashSet<usize>,
    excluded_texts: &HashSet<String>,
) -> Hit {
    let row = segment.row(candidate.chunk);
    let class = row.class();
    let file = &segment.files()[row.file as usize];
    let (mut start, mut end) = (row.start as usize, row.end as usize);
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
        bytes: None,
        same: Vec::new(),
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
        hit.bytes = Some(
            lines
                .iter()
                .skip(start - 1)
                .map(|line| line.len() + 1)
                .sum(),
        );
        (lines, (start..=end).collect())
    } else {
        let Some(text) = workspace
            .source_path(file)
            .ok()
            .and_then(|path| fs::read_to_string(path).ok())
        else {
            return hit;
        };
        // Expand adjacent same-name plain-text continuations (from TEXT_WINDOW splits).
        if segment.kind(&row) == "text" {
            let mut changed = true;
            while changed {
                changed = false;
                for other in merged {
                    let other_row = segment.row(other.chunk);
                    let (os, oe) = (other_row.start as usize, other_row.end as usize);
                    if segment.kind(&other_row) == "text"
                        && segment.name(&other_row) == hit.name
                        && (oe + 1 == start || end + 1 == os)
                    {
                        let new_start = start.min(os);
                        let new_end = end.max(oe);
                        if (new_start, new_end) != (start, end) {
                            start = new_start;
                            end = new_end;
                            changed = true;
                        }
                    }
                }
            }
            hit.location.start = start;
            hit.location.end = end;
        }
        let others = segment.ranges()[row.file as usize]
            .clone()
            .filter(|&id| id != candidate.chunk)
            .map(|id| {
                let other = segment.row(id);
                (other.start as usize, other.end as usize)
            });
        let mut indexed =
            chunks::indexed_lines(class, (row.start as usize, row.end as usize), others);
        // Merge candidate lines from absorbed overlapping/adjacent chunks that
        // fall inside this hit's displayed span so one hit carries the best
        // evidence without duplicating lines.
        for other in merged {
            let other_row = segment.row(other.chunk);
            let (os, oe) = (other_row.start as usize, other_row.end as usize);
            let other_others = segment.ranges()[other_row.file as usize]
                .clone()
                .filter(|&id| id != other.chunk)
                .map(|id| {
                    let sibling = segment.row(id);
                    (sibling.start as usize, sibling.end as usize)
                });
            for line in chunks::indexed_lines(other_row.class(), (os, oe), other_others) {
                if (start..=end).contains(&line) {
                    indexed.push(line);
                }
            }
        }
        indexed.sort_unstable();
        indexed.dedup();
        hit.bytes = Some(
            text.split_inclusive('\n')
                .skip(start - 1)
                .take(end + 1 - start)
                .map(str::len)
                .sum(),
        );
        (text.lines().map(str::to_owned).collect(), indexed)
    };
    let lines = candidates
        .iter()
        .filter_map(|&number| Some((number, text.get(number.checked_sub(1)?)?.as_str())))
        .collect::<Vec<_>>();
    // Words the header already shows count as shown.
    let header = format!("{} {} {}", hit.location.file, hit.kind, hit.name);
    hit.lines = snippet::lines_excluding(
        &lines,
        question,
        idf,
        &header,
        excluded_numbers,
        excluded_texts,
    );
    if hit.lines.is_empty() {
        let declaration = row.declaration as usize;
        let non_blank = |number: usize| {
            text.get(number.saturating_sub(1))
                .is_some_and(|line| !line.trim().is_empty())
        };
        let fallback = if class == Class::Code
            && candidates.contains(&declaration)
            && non_blank(declaration)
            && !excluded_numbers.contains(&declaration)
        {
            Some(declaration)
        } else {
            lines
                .iter()
                .find(|(number, line)| {
                    !line.trim().is_empty() && !excluded_numbers.contains(number)
                })
                .or_else(|| lines.iter().find(|(_, line)| !line.trim().is_empty()))
                .map(|(number, _)| *number)
        };
        hit.lines.extend(
            fallback.and_then(|number| Some((number, snippet::plain(text.get(number - 1)?)))),
        );
    }
    hit
}

fn quoted(file: &str) -> String {
    crate::workspace::shell_path(file)
}

/// The command that opens the top result: its exact bytes when the whole
/// result fits `budget`, because `expand` and `log` have no budget of their
/// own. Otherwise a budgeted excerpt around its best lines, or for a log just
/// the lines shown. `justless` starts it, so it reaches the same workspace.
fn next(hit: &Hit, budget: usize, justless: &Invocation) -> String {
    let Location { file, start, end } = &hit.location;
    let fits = hit.bytes.is_some_and(|bytes| bytes <= budget);
    if let Some((run, stream)) = &hit.run {
        let (from, to) = match (hit.lines.first(), hit.lines.last()) {
            (Some((first, _)), Some((last, _))) if !fits => (*first, *last),
            _ => (*start, *end),
        };
        return format!(
            "{} log {run} --stream {} --from {from} --to {to}",
            justless.command,
            stream.name()
        );
    }
    if fits {
        return format!("{} expand {} L{start}-{end}", justless.files, quoted(file));
    }
    let first = hit.lines.first().map_or(*start, |(line, _)| *line);
    let from = (*start).max(first.saturating_sub(3));
    let to = (*end).min(from + EXCERPT_LINES - 1);
    format!(
        "{} code {} --from {from} --to {to}",
        justless.command,
        quoted(file)
    )
}

/// Shortens a consecutive same-file path to `…/<basename>` when its directory
/// prefix is longer than `…`, freeing bytes for evidence while keeping the
/// filename and line range unambiguous.
fn compact_continuation_path(file: &str) -> String {
    if let Some((dir, name)) = file.rsplit_once('/')
        && dir.len() > "…".len()
    {
        return format!("…/{name}");
    }
    file.to_owned()
}

fn format_location(file: &str, start: usize, end: usize) -> String {
    if start == end {
        format!("{file}:{start}")
    } else {
        format!("{file}:{start}-{end}")
    }
}

/// Renders every hit in rank order, tightening repeated same-file path prefixes,
/// repeated `same in` copy annotations, and repeated log exit statuses across
/// grouped consecutive hits.
fn render_blocks(hits: &[Hit]) -> Vec<String> {
    hits.iter()
        .enumerate()
        .map(|(index, hit)| {
            let previous = index.checked_sub(1).map(|prev| &hits[prev]);
            let mut block = match &hit.run {
                Some((run, stream)) => {
                    let repeat_run = previous.is_some_and(|prev| {
                        prev.run
                            .as_ref()
                            .is_some_and(|(prev_run, _)| prev_run == run)
                            && prev.status == hit.status
                    });
                    let status = if repeat_run {
                        String::new()
                    } else {
                        hit.status
                            .map(|status| format!(" · exit {status}"))
                            .unwrap_or_default()
                    };
                    format!(
                        "{run} {}:{}-{} log {}{status}\n",
                        stream.name(),
                        hit.location.start,
                        hit.location.end,
                        hit.name
                    )
                }
                None => {
                    let same_as_prev = previous.is_some_and(|prev| {
                        prev.run.is_none() && prev.location.file == hit.location.file
                    });
                    let display_file = if same_as_prev {
                        compact_continuation_path(&hit.location.file)
                    } else {
                        hit.location.file.clone()
                    };
                    let same_already_shown = !hit.same.is_empty()
                        && hits[..index].iter().any(|prev| {
                            prev.run.is_none()
                                && prev.location.file == hit.location.file
                                && prev.same == hit.same
                        });
                    let same = if same_already_shown {
                        String::new()
                    } else {
                        match hit.same.as_slice() {
                            [] => String::new(),
                            [one] => format!(" · same in {one}"),
                            [first, rest @ ..] => {
                                format!(" · same in {first} +{} more", rest.len())
                            }
                        }
                    };
                    let location =
                        format_location(&display_file, hit.location.start, hit.location.end);
                    format!("{location} {} {}{same}\n", hit.kind, hit.name)
                }
            };
            for (line, text) in &hit.lines {
                block.push_str(&format!("  {line} | {text}\n"));
            }
            block
        })
        .collect()
}

fn words(words: &[String], limit: usize) -> String {
    chunks::clip(&words.join(", "), limit)
}

/// The matching log windows per stream, with those among the first `shown`
/// results counted as shown.
fn log_windows(ranking: &Ranking, shown: usize) -> Option<capture::Tally> {
    let mut tally = ranking.logs?;
    for hit in &ranking.hits[..shown.min(ranking.hits.len())] {
        if let Some((_, stream)) = &hit.run {
            tally.show(*stream);
        }
    }
    Some(tally)
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
    // Log windows the three places left out are counted per stream. The clause
    // is there whatever the budget shows, so the footer it reserves fits. More
    // `--limit` adds no log windows, so the full form names the search that
    // lists every matching line.
    let ranked = ranking.hits.iter().filter(|hit| hit.run.is_some()).count();
    if let Some(windows) = log_windows(ranking, shown).filter(|tally| tally.total() > ranked) {
        line.push_str(&format!(" · log windows {windows}"));
        if limit.is_some() {
            line.push_str(" (justless history TEXT lists every matching line)");
        }
    }
    line.push('\n');
    line
}

fn render_text(ranking: &Ranking, budget: usize) -> String {
    if ranking.hits.is_empty() {
        let mut limit = MAX_WORDS;
        loop {
            let line = format!(
                "No ranked matches for: {}. Use {} find TEXT for exact text.\n",
                words(&ranking.unmatched, limit),
                ranking.justless.command
            );
            if line.len() <= budget || limit <= 8 {
                return line;
            }
            limit -= 8;
        }
    }
    let blocks = render_blocks(&ranking.hits);
    let next = format!(
        "Next: {}\n",
        next(&ranking.hits[0], budget, &ranking.justless)
    );
    let all = counts(ranking, blocks.len(), Some(MAX_WORDS));
    if blocks.iter().map(String::len).sum::<usize>() + all.len() + next.len() <= budget {
        return blocks.concat() + &all + &next;
    }
    // Reserve the footer, then add whole results in rank order while they
    // fit. A footer too large for the budget shortens its word lists first,
    // then drops the closing command.
    let shown = ranking.hits.len();
    let mut limit = Some(MAX_WORDS);
    let mut next = Some(next);
    let footer = |limit, next: &Option<String>| {
        counts(ranking, shown, limit).len() + MARKER.len() + next.as_ref().map_or(0, String::len)
    };
    while footer(limit, &next) > budget && limit.is_some() {
        limit = limit.filter(|&words| words > 16).map(|words| words - 8);
    }
    if footer(limit, &next) > budget {
        next = None;
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
    if shown == 0
        && let Some(output) = squeezed(ranking, &blocks[0], next.as_deref(), budget)
    {
        return output;
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

/// The top result alone, for a budget too small to show it beside the word
/// lists and the closing command. They give way to it and come back only
/// while they still fit. A result too large on its own keeps just its header.
fn squeezed(ranking: &Ranking, top: &str, next: Option<&str>, budget: usize) -> Option<String> {
    let bare = counts(ranking, 1, None);
    let fixed = bare.len() + MARKER.len();
    let header = &top[..top.find('\n').map_or(top.len(), |at| at + 1)];
    let top = [top, header]
        .into_iter()
        .find(|top| fixed + top.len() <= budget)?;
    let mut used = fixed + top.len();
    let next = next.filter(|next| used + next.len() <= budget);
    used += next.map_or(0, str::len);
    let mut limit = MAX_WORDS;
    let counts = loop {
        let line = counts(ranking, 1, Some(limit));
        if used - bare.len() + line.len() <= budget {
            break line;
        }
        if limit <= 16 {
            break bare;
        }
        limit -= 8;
    };
    Some(format!("{top}{counts}{MARKER}{}", next.unwrap_or_default()))
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
    if !hit.same.is_empty() {
        result["same"] = json!(hit.same);
    }
    result
}

/// The words that fit in `limit` bytes when joined, and how many did not.
fn cut(words: &[String], limit: usize) -> (&[String], usize) {
    let mut used = 0;
    let kept = words
        .iter()
        .take_while(|word| {
            used += word.len() + 2;
            used <= limit.saturating_add(2)
        })
        .count();
    (&words[..kept], words.len() - kept)
}

fn render_json(question: &str, ranking: &Ranking, budget: usize) -> Result<String> {
    let results = ranking
        .hits
        .iter()
        .map(|hit| serde_json::to_string(&result_json(hit)))
        .collect::<serde_json::Result<Vec<_>>>()?;
    // `limit` bounds the echoed question and each word list, in bytes.
    let envelope = |kept: usize, limit: usize| {
        let (matched, more) = cut(&ranking.matched, limit);
        let (unmatched, less) = cut(&ranking.unmatched, limit);
        let mut value = json!({
            "query": chunks::clip(question, limit),
            "matched": matched,
            "unmatched": unmatched,
        });
        if more + less > 0 {
            value["words_omitted"] = json!(more + less);
        }
        value["total"] = json!(ranking.total);
        value["omitted"] = json!(ranking.hits.len() - kept);
        if let Some(windows) = log_windows(ranking, kept) {
            let stream = |stream: Stream| {
                let (shown, total) = windows.of(stream);
                json!({"total": total, "shown": shown})
            };
            value["logs"] = json!({
                "total": windows.total(),
                "omitted": windows.total() - windows.shown(),
                "stdout": stream(Stream::Stdout),
                "stderr": stream(Stream::Stderr),
            });
        }
        value["next"] = json!(ranking.hits.first().filter(|_| kept > 0).map(|hit| next(
            hit,
            budget,
            &ranking.justless
        )));
        value["results"] = json!([]);
        value.to_string()
    };
    let size = |base: &str, kept: usize| {
        base.len()
            + results[..kept].iter().map(String::len).sum::<usize>()
            + kept.saturating_sub(1)
            + 1
    };
    let fits = |kept: usize, limit: usize| {
        let base = envelope(kept, limit);
        (size(&base, kept) <= budget).then_some((kept, base))
    };
    // Shorter lists, down to none, for the budgets too small for them.
    let limits = || std::iter::successors(Some(MAX_WORDS), |limit| limit.checked_sub(8));
    // As many results as fit beside the full envelope; otherwise the top
    // result with a shorter question and word lists; otherwise none.
    let (kept, base) = (1..=results.len())
        .rev()
        .find_map(|kept| fits(kept, usize::MAX))
        .or_else(|| {
            (!results.is_empty())
                .then(|| limits().find_map(|limit| fits(1, limit)))
                .flatten()
        })
        .or_else(|| fits(0, usize::MAX))
        .or_else(|| limits().find_map(|limit| fits(0, limit)))
        .unwrap_or_else(|| (0, envelope(0, 0)));
    let body = format!("[{}]", results[..kept].join(","));
    let base = base
        .strip_suffix("[]}")
        .expect("results come last in the envelope");
    Ok(format!("{base}{body}}}\n"))
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
    let json = matches.is_present("json");
    let output = if json {
        render_json(&question, &ranking, budget)?
    } else {
        render_text(&ranking, budget)
    };
    io::stdout().lock().write_all(output.as_bytes())?;
    note_log_windows(workspace, &ranking, &output, json);
    Ok(())
}

/// Records the log windows that `output` shows as retrievals of their runs, so
/// `stats` subtracts them from the runs' savings. A window cut down to its
/// header counts as the header.
fn note_log_windows(workspace: &Workspace, ranking: &Ranking, output: &str, json: bool) {
    if !ranking.hits.iter().any(|hit| hit.run.is_some()) {
        return;
    }
    let mut printed = std::collections::BTreeMap::<String, u64>::new();
    let mut charge = |hit: &Hit, bytes: usize| {
        if let Some((run, _)) = &hit.run {
            *printed.entry(run.clone()).or_default() += bytes as u64;
        }
    };
    if json {
        if let Ok(value) = serde_json::from_str::<Value>(output)
            && let Some(results) = value["results"].as_array()
        {
            for (hit, result) in ranking.hits.iter().zip(results) {
                charge(hit, result.to_string().len());
            }
        }
    } else {
        let mut rest = output;
        let blocks = render_blocks(&ranking.hits);
        for (hit, full) in ranking.hits.iter().zip(&blocks) {
            let header = full.split_inclusive('\n').next().unwrap_or_default();
            if let Some(next) = rest.strip_prefix(full.as_str()) {
                charge(hit, full.len());
                rest = next;
            } else {
                if rest.starts_with(header) {
                    charge(hit, header.len());
                }
                break;
            }
        }
    }
    crate::history::excerpts(workspace, &printed);
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
            bytes: Some((end + 1 - start) * 20),
            same: Vec::new(),
        }
    }

    fn ranking(hits: Vec<Hit>) -> Ranking {
        Ranking {
            total: hits.len() + 3,
            hits,
            matched: vec!["retries".into()],
            unmatched: vec!["configured".into()],
            truncated: false,
            justless: Invocation::plain(),
            logs: None,
        }
    }

    #[test]
    fn next_offers_expand_only_when_the_whole_result_fits() {
        assert_eq!(
            next(&hit("src/a b.rs", 5, 84, &[]), 2400, &Invocation::plain()),
            "justless expand 'src/a b.rs' L5-84"
        );
        // A few long lines are too large for the budget.
        let mut long = hit("src/a.rs", 5, 15, &[(10, "x")]);
        long.bytes = Some(3659);
        assert_eq!(
            next(&long, 2400, &Invocation::plain()),
            "justless code src/a.rs --from 7 --to 15"
        );
        assert_eq!(
            next(&long, 4000, &Invocation::plain()),
            "justless expand src/a.rs L5-15"
        );
        long.bytes = None;
        assert_eq!(
            next(&long, 100_000, &Invocation::plain()),
            "justless code src/a.rs --from 7 --to 15"
        );
        assert_eq!(
            next(
                &hit("src/a.rs", 5, 300, &[(6, "x")]),
                2400,
                &Invocation::plain()
            ),
            "justless code src/a.rs --from 5 --to 44"
        );
        let mut log = hit("run-1", 1, 40, &[(7, "x"), (9, "y")]);
        log.run = Some(("run-1".into(), Stream::Stderr));
        assert_eq!(
            next(&log, 2400, &Invocation::plain()),
            "justless log run-1 --stream stderr --from 1 --to 40"
        );
        assert_eq!(
            next(&log, 500, &Invocation::plain()),
            "justless log run-1 --stream stderr --from 7 --to 9"
        );
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
        for budget in 128..full.len() {
            let output = render_text(&ranking, budget);
            assert!(output.len() <= budget, "{budget}: {output}");
            assert!(output.contains(MARKER), "{output}");
            // The top result always shows, before the word lists and Next.
            assert!(
                output.starts_with("src/retry.rs:1-5 function retry\n  2 | let retries = 3;\n"),
                "{budget}: {output}"
            );
        }
        assert_eq!(render_text(&ranking, full.len()), full);
        assert_eq!(
            render_text(&ranking, 128),
            "src/retry.rs:1-5 function retry\n  2 | let retries = 3;\n1 of 8 ranked\n[context budget reached]\n"
        );
        assert_eq!(
            render_text(&ranking, 200),
            "src/retry.rs:1-5 function retry\n  2 | let retries = 3;\n1 of 8 ranked · matched retries · no match configured\n[context budget reached]\nNext: justless expand src/retry.rs L1-5\n"
        );
        // A top result too large for the budget keeps its header.
        let long = "x".repeat(150);
        let ranking = self::ranking(vec![hit("src/retry.rs", 1, 5, &[(2, &long)])]);
        assert_eq!(
            render_text(&ranking, 128),
            "src/retry.rs:1-5 function retry\n1 of 4 ranked\n[context budget reached]\nNext: justless expand src/retry.rs L1-5\n"
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
        assert!(value.get("words_omitted").is_none());
        for budget in 128..full.len() {
            let output = render_json("retries", &ranking, budget).unwrap();
            assert!(output.len() <= budget, "{budget}: {output}");
            serde_json::from_str::<Value>(&output).unwrap();
        }
    }

    #[test]
    fn json_shortens_the_question_and_word_lists_last() {
        let words = (0..40)
            .map(|index| format!("missingword{index}"))
            .collect::<Vec<_>>();
        let question = words.join(" ");
        let mut ranking = ranking(vec![hit("src/retry.rs", 1, 1, &[(1, "retry")])]);
        ranking.unmatched = words;
        let full = render_json(&question, &ranking, 100_000).unwrap();
        for budget in 128..full.len() {
            let output = render_json(&question, &ranking, budget).unwrap();
            assert!(output.len() <= budget, "{budget}: {output}");
            serde_json::from_str::<Value>(&output).unwrap();
        }
        let value: Value =
            serde_json::from_str(&render_json(&question, &ranking, 128).unwrap()).unwrap();
        assert!(value["words_omitted"].as_u64().unwrap() > 30, "{value}");
        assert!(value["query"].as_str().unwrap().ends_with('…'), "{value}");
        // The top result is kept before the full word lists.
        let value: Value =
            serde_json::from_str(&render_json(&question, &ranking, 600).unwrap()).unwrap();
        assert_eq!(value["results"].as_array().unwrap().len(), 1, "{value}");
        assert!(value["words_omitted"].as_u64().unwrap() > 0, "{value}");
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
        assert!(output.contains(MARKER), "{output}");
        let empty = Ranking {
            hits: Vec::new(),
            total: 0,
            matched: Vec::new(),
            unmatched: (0..40).map(|index| format!("missing{index}")).collect(),
            truncated: false,
            justless: Invocation::plain(),
            logs: None,
        };
        let output = render_text(&empty, 128);
        assert!(output.len() <= 128, "{output}");
        assert!(
            output.starts_with("No ranked matches for: missing0, "),
            "{output}"
        );
    }

    #[test]
    fn next_starts_with_the_workspace_invocation() {
        let justless = Invocation {
            command: "justless --root /w".into(),
            files: "justless --root /w".into(),
        };
        assert_eq!(
            next(&hit("src/a.rs", 5, 15, &[]), 2400, &justless),
            "justless --root /w expand src/a.rs L5-15"
        );
        // From a subdirectory only `expand` needs the root.
        let below = Invocation {
            command: "justless".into(),
            files: "justless --root /w".into(),
        };
        assert_eq!(
            next(&hit("src/a.rs", 5, 300, &[(6, "x")]), 2400, &below),
            "justless code src/a.rs --from 5 --to 44"
        );
        assert_eq!(
            next(&hit("src/a.rs", 5, 15, &[]), 2400, &below),
            "justless --root /w expand src/a.rs L5-15"
        );
        let mut log = hit("run-1", 1, 40, &[(7, "x")]);
        log.run = Some(("run-1".into(), Stream::Stdout));
        assert_eq!(
            next(&log, 2400, &justless),
            "justless --root /w log run-1 --stream stdout --from 1 --to 40"
        );
        let mut empty = ranking(Vec::new());
        empty.justless = justless;
        let output = render_text(&empty, 2400);
        assert!(
            output.contains("Use justless --root /w find TEXT"),
            "{output}"
        );
    }

    #[test]
    fn log_windows_left_out_are_counted_per_stream_within_the_budget() {
        let log = |stream: Stream, start: usize| {
            let mut hit = hit(
                "run-Ab12",
                start,
                start + 39,
                &[(start, "order-42 progress")],
            );
            hit.source = Source::Log;
            hit.run = Some(("run-Ab12".into(), stream));
            hit
        };
        let mut found = capture::Tally::default();
        found.found(Stream::Stdout, 4);
        found.found(Stream::Stderr, 1);
        let mut ranking = ranking(vec![
            log(Stream::Stdout, 1),
            log(Stream::Stdout, 41),
            log(Stream::Stderr, 1),
            hit("src/a.rs", 5, 9, &[(6, "x")]),
        ]);
        ranking.logs = Some(found);
        let full = render_text(&ranking, 100_000);
        assert!(
            full.contains(
                " · log windows stdout 2 of 4, stderr 1 of 1 (justless history TEXT lists every matching line)\n"
            ),
            "{full}"
        );
        for budget in (128..full.len()).step_by(3) {
            let output = render_text(&ranking, budget);
            assert!(output.len() <= budget, "{budget}: {output}");
            assert!(output.contains(" · log windows stdout "), "{output}");
        }
        let value: Value =
            serde_json::from_str(&render_json("order-42", &ranking, 100_000).unwrap()).unwrap();
        assert_eq!(
            value["logs"],
            json!({"total": 5, "omitted": 2, "stdout": {"total": 4, "shown": 2}, "stderr": {"total": 1, "shown": 1}})
        );
        // When every window is shown, the text adds nothing.
        found = capture::Tally::default();
        found.found(Stream::Stdout, 2);
        found.found(Stream::Stderr, 1);
        ranking.logs = Some(found);
        assert!(!render_text(&ranking, 100_000).contains("log windows"));
    }

    #[test]
    fn consecutive_same_file_hits_compact_headers_and_save_bytes() {
        let path = "packages/billing/src/services/retry/policy_manager.rs";
        let mut first = hit(path, 10, 25, &[(10, "pub struct RetryPolicy {")]);
        first.kind = "struct".into();
        first.name = "RetryPolicy".into();
        first.same = vec!["vendor/billing/policy_manager.rs".into()];

        let mut second = hit(path, 40, 55, &[(42, "pub fn delay(&self) -> u64 {")]);
        second.kind = "method".into();
        second.name = "RetryPolicy::delay".into();
        second.same = vec!["vendor/billing/policy_manager.rs".into()];

        let ranking = ranking(vec![first, second]);
        let rendered = render_text(&ranking, 100_000);
        // First hit has full path and `same in`, second compacts directory and drops duplicate `same in`.
        assert!(rendered.contains(&format!(
            "{path}:10-25 struct RetryPolicy · same in vendor/billing/policy_manager.rs\n"
        )));
        assert!(rendered.contains("…/policy_manager.rs:40-55 method RetryPolicy::delay\n"));
        assert_eq!(rendered.matches("same in ").count(), 1, "{rendered}");
        assert!(rendered.ends_with(&format!("Next: justless expand {path} L10-25\n")));

        // Measure savings vs uncompacted repetition: 33 bytes of dir + 44 bytes of ` · same in ...` = 77 bytes saved.
        let naive_second_header = format!(
            "{path}:40-55 method RetryPolicy::delay · same in vendor/billing/policy_manager.rs\n"
        );
        let compact_second_header = "…/policy_manager.rs:40-55 method RetryPolicy::delay\n";
        assert!(
            naive_second_header.len() - compact_second_header.len() >= 70,
            "expected >= 70 bytes saved on second header"
        );

        // Consecutive log hits from the same run drop repeated ` · exit N`.
        let mut log1 = hit("run-1", 1, 40, &[(3, "error one")]);
        log1.source = Source::Log;
        log1.run = Some(("run-1".into(), Stream::Stderr));
        log1.status = Some(2);
        let mut log2 = hit("run-1", 41, 80, &[(45, "error two")]);
        log2.source = Source::Log;
        log2.run = Some(("run-1".into(), Stream::Stderr));
        log2.status = Some(2);
        let log_rendered = render_text(&self::ranking(vec![log1, log2]), 100_000);
        assert_eq!(
            log_rendered.matches(" · exit 2").count(),
            1,
            "{log_rendered}"
        );
    }

    #[test]
    fn rank_deduplicates_overlapping_class_and_method_and_merges_text_windows() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let write = |rel: &str, body: &str| {
            let full = root.join(rel);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, body).unwrap();
        };
        // A class whose method overlaps the class span: both match "quasar" and "beacon".
        write(
            "web/beacon.ts",
            "\
export class QuasarBeacon {
  private readonly active = true;

  emitQuasarBeacon(level: number): void {
    if (this.active) {
      console.log('quasar beacon', level);
    }
  }
}
",
        );
        // Distinct files that should not be crowded out by the overlapping class/method pair.
        write(
            "src/alpha.rs",
            "/// Quasar beacon sender.\npub fn send_quasar_beacon() {}\n",
        );
        write(
            "src/beta.rs",
            "/// Quasar beacon receiver.\npub fn recv_quasar_beacon() {}\n",
        );
        // Adjacent 60-line text windows in the same section.
        let mut long_cfg = String::from("[quasar]\n");
        for line in 2..=90 {
            if line == 5 {
                long_cfg.push_str("beacon_alpha = true\n");
            } else if line == 65 {
                long_cfg.push_str("beacon_beta = true\n");
            } else {
                long_cfg.push_str(&format!("entry_{line} = {line}\n"));
            }
        }
        write("config/quasar.ini", &long_cfg);

        let workspace = Workspace {
            root: root.clone(),
            cwd: root.clone(),
            state: root.join(".justless"),
            origin: root.clone(),
        };
        let ranked = rank(
            &workspace,
            "quasar beacon",
            &Options {
                limit: 4,
                logs: false,
            },
        )
        .unwrap();
        let files = ranked
            .hits
            .iter()
            .map(|h| h.location.file.as_str())
            .collect::<Vec<_>>();
        // web/beacon.ts takes only 1 slot (not 2 for class + overlapping method),
        // and config/quasar.ini merges its adjacent 1-60 & 61-90 windows into 1-90!
        assert_eq!(
            files.iter().filter(|&&f| f == "web/beacon.ts").count(),
            1,
            "{files:?}"
        );
        assert_eq!(
            files.iter().filter(|&&f| f == "config/quasar.ini").count(),
            1,
            "{files:?}"
        );
        assert!(files.contains(&"src/alpha.rs"), "{files:?}");
        assert!(files.contains(&"src/beta.rs"), "{files:?}");
        let cfg_hit = ranked
            .hits
            .iter()
            .find(|h| h.location.file == "config/quasar.ini")
            .unwrap();
        assert_eq!((cfg_hit.location.start, cfg_hit.location.end), (1, 90));
    }
}
