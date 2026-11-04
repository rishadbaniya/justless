//! Ranks the lines of a long stdout against an intent such as "payments timeouts", so
//! a bounded preview shows the lines that matter instead of the head and tail.
//!
//! Ranking is deterministic and offline: BM25 over each line's words, with a light
//! stemmer applied to both sides, plus double-quoted phrases. Lines that contain every
//! term rank first. A long line shows the windows of it that hold the most terms. Each
//! excerpt keeps its `stdout:N` marker for `justless log`.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, VecDeque};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Result, ensure};
use regex::Regex;

static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]").expect("ANSI pattern"));

/// Bytes of one line that are ranked; the rest of the line is skipped.
const LINE_LIMIT: usize = 4 * 1024 * 1024;
const LINE_LIMIT_SHOWN: &str = "4 MiB";
const MAX_TERMS: usize = 16;
/// Matches remembered per line, which bounds the windows a long line can offer.
const SPAN_LIMIT: usize = 4096;
/// Runs of matching lines remembered for the trailer.
const RANGE_LIMIT: usize = 1024;
const RANGES_SHOWN: usize = 3;
/// Lines that hold different terms within this distance may be one record.
const NEARBY: usize = 8;
/// Longest excerpt of a matching line, and of an anchor or fallback line.
const EXCERPT: usize = 240;
const ANCHOR: usize = 160;
/// Bytes kept before the first match when a long line is windowed.
const LEAD: usize = 80;
/// Smallest body worth the full trailer; below it, the trailer drops line numbers.
const ROOMY: usize = 200;
const INTENT_SHOWN: usize = 60;
const K1: f64 = 1.2;
const B: f64 = 0.75;
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "of", "to", "in", "for", "and", "or", "is", "are", "was", "were", "be",
    "been", "what", "which", "who", "how", "many", "much", "show", "me", "find", "all", "any",
    "each", "with", "by", "on", "at", "from", "that", "this", "these", "those", "it", "its", "i",
    "my", "we", "our", "where", "why", "when", "does", "do", "did", "get", "give", "list", "per",
    "please", "there", "their",
];

/// A stemmed word, or a phrase matched as a case-insensitive substring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    text: String,
    phrase: bool,
}

/// The terms of an intent: double-quoted phrases and the remaining words, without
/// common words, in order and at most 16.
pub fn terms(intent: &str) -> Vec<Term> {
    let mut terms = Vec::new();
    let mut add = |term: Term| {
        if terms.len() < MAX_TERMS && !terms.contains(&term) {
            terms.push(term);
        }
    };
    let parts: Vec<&str> = intent.split('"').collect();
    // An unclosed quote leaves the text after it as ordinary words.
    let unclosed = parts.len().is_multiple_of(2);
    for (index, part) in parts.iter().enumerate() {
        let quoted = index % 2 == 1 && !(unclosed && index == parts.len() - 1);
        if quoted {
            let phrase = part.trim();
            if !phrase.is_empty() {
                add(Term {
                    text: phrase.to_ascii_lowercase(),
                    phrase: true,
                });
            }
            continue;
        }
        tokens(part, |_, token| {
            let word = token.to_lowercase();
            let single = word.chars().count() == 1 && !word.chars().all(|c| c.is_ascii_digit());
            if !single && !STOPWORDS.contains(&word.as_str()) {
                add(Term {
                    text: stem(&word),
                    phrase: false,
                });
            }
        });
    }
    terms
}

/// Calls `emit` with the byte offset and text of each word: a run of letters and
/// digits, split where a lowercase letter meets an uppercase one.
fn tokens(text: &str, mut emit: impl FnMut(usize, &str)) {
    let mut start = None;
    let mut lower = false;
    for (index, c) in text.char_indices() {
        if c.is_alphanumeric() {
            match start {
                None => start = Some(index),
                Some(first) if lower && c.is_uppercase() => {
                    emit(first, &text[first..index]);
                    start = Some(index);
                }
                Some(_) => {}
            }
            lower = c.is_lowercase();
        } else {
            if let Some(first) = start.take() {
                emit(first, &text[first..index]);
            }
            lower = false;
        }
    }
    if let Some(first) = start {
        emit(first, &text[first..]);
    }
}

/// Folds plurals, `-ing`, `-ed` and a final `e`, so parse, parsed and parsing meet.
/// Words with a digit, such as E0382, stay whole. A stem is at most 5 bytes shorter
/// than its word and never longer.
fn stem(word: &str) -> String {
    let mut word = word.to_owned();
    if word.bytes().any(|b| b.is_ascii_digit()) {
        return word;
    }
    if word.len() > 4 && word.ends_with("ies") {
        word.truncate(word.len() - 3);
        word.push('y');
    } else if ["sses", "ches", "shes", "xes", "zes"]
        .iter()
        .any(|suffix| word.ends_with(suffix))
    {
        word.truncate(word.len() - 2);
    } else if word.len() > 3
        && word.ends_with('s')
        && !["ss", "us", "is"]
            .iter()
            .any(|suffix| word.ends_with(suffix))
    {
        word.pop();
    }
    if word.len() >= 6 && word.ends_with("ing") {
        word.truncate(word.len() - 3);
    } else if word.len() >= 5 && word.ends_with("ed") {
        word.truncate(word.len() - 2);
    }
    if word.len() > 3 && word.ends_with('e') {
        word.pop();
    }
    word
}

/// One match of a term in a line.
#[derive(Clone, Copy, Debug)]
struct Span {
    start: usize,
    len: usize,
    term: usize,
}

/// Term frequencies of one line, where its matches lie, and its word count.
struct Analysis {
    counts: Vec<u32>,
    /// Matches in line order, at most `SPAN_LIMIT`.
    spans: Vec<Span>,
    words: usize,
    /// Byte lengths an ASCII word needs to stem to a word term.
    shortest: usize,
    longest: usize,
}

impl Analysis {
    fn new(terms: &[Term]) -> Self {
        let words = terms.iter().filter(|term| !term.phrase);
        Self {
            counts: vec![0; terms.len()],
            spans: Vec::new(),
            words: 0,
            shortest: words.clone().map(|term| term.text.len()).min().unwrap_or(0),
            longest: words.map(|term| term.text.len() + 5).max().unwrap_or(0),
        }
    }

    fn run(&mut self, line: &str, terms: &[Term]) {
        self.counts.fill(0);
        self.spans.clear();
        self.words = 0;
        let has_words = terms.iter().any(|term| !term.phrase);
        let (shortest, longest) = (self.shortest, self.longest);
        tokens(line, |start, token| {
            self.words += 1;
            // Most words cannot stem to a term, and skipping them saves an allocation.
            if !has_words || token.is_ascii() && !(shortest..=longest).contains(&token.len()) {
                return;
            }
            let word = stem(&token.to_lowercase());
            for (index, term) in terms.iter().enumerate() {
                if !term.phrase && term.text == word {
                    self.counts[index] += 1;
                    if self.spans.len() < SPAN_LIMIT {
                        self.spans.push(Span {
                            start,
                            len: token.len(),
                            term: index,
                        });
                    }
                }
            }
        });
        if terms.iter().any(|term| term.phrase) {
            // ASCII lowercasing keeps byte offsets, so matches map back to the line.
            let lower = line.to_ascii_lowercase();
            for (index, term) in terms.iter().enumerate() {
                if term.phrase {
                    for (start, _) in lower.match_indices(&term.text) {
                        self.counts[index] += 1;
                        if self.spans.len() < SPAN_LIMIT {
                            self.spans.push(Span {
                                start,
                                len: term.text.len(),
                                term: index,
                            });
                        }
                    }
                }
            }
            self.spans.sort_by_key(|span| span.start);
        }
    }

    fn distinct(&self) -> usize {
        self.counts.iter().filter(|&&count| count > 0).count()
    }

    /// The terms present, one bit each.
    fn mask(&self) -> u32 {
        self.counts
            .iter()
            .enumerate()
            .filter(|&(_, &count)| count > 0)
            .fold(0, |mask, (index, _)| mask | 1 << index)
    }

    fn matches(&self) -> usize {
        self.counts.iter().map(|&count| count as usize).sum()
    }
}

/// Up to 240 bytes of a line, with the byte offset to keep in view when it is narrowed.
#[derive(Clone)]
struct Excerpt {
    text: String,
    focus: usize,
    /// The line continues before or after `text`.
    before: bool,
    after: bool,
}

impl Excerpt {
    fn new(line: &str, focus: usize) -> Self {
        if line.len() <= EXCERPT {
            return Self {
                text: line.to_owned(),
                focus,
                before: false,
                after: false,
            };
        }
        let start = line.floor_char_boundary(
            focus
                .saturating_sub(LEAD)
                .min(line.len().saturating_sub(EXCERPT)),
        );
        let end = line.floor_char_boundary((start + EXCERPT).min(line.len()));
        Self {
            text: line[start..end].to_owned(),
            focus: focus.saturating_sub(start),
            before: start > 0,
            after: end < line.len(),
        }
    }

    /// The excerpt within `cap` bytes, marking each clipped side with an ellipsis, and
    /// the range of `text` it shows.
    fn render(&self, cap: usize) -> (String, usize, usize) {
        if !self.before && !self.after && self.text.len() <= cap {
            return (self.text.clone(), 0, self.text.len());
        }
        let room = cap.saturating_sub(2 * '…'.len_utf8());
        let text = &self.text;
        // A narrow excerpt keeps less before the focus, so the focus stays visible.
        let lead = LEAD.min(room / 3);
        let start = text.floor_char_boundary(
            self.focus
                .saturating_sub(lead)
                .min(text.len().saturating_sub(room)),
        );
        let end = text.floor_char_boundary((start + room).min(text.len()));
        let mut excerpt = String::new();
        if self.before || start > 0 {
            excerpt.push('…');
        }
        excerpt.push_str(&text[start..end]);
        if self.after || end < text.len() {
            excerpt.push('…');
        }
        (excerpt, start, end)
    }
}

/// A part of a line with the matches inside it.
#[derive(Clone)]
struct Window {
    /// Byte offset of the excerpt in the line.
    start: usize,
    excerpt: Excerpt,
    /// Matches inside the excerpt, as byte ranges of its text.
    spans: Vec<(usize, usize)>,
}

impl Window {
    fn at(line: &str, start: usize, spans: &[Span]) -> Self {
        let end = line.floor_char_boundary((start + EXCERPT).min(line.len()));
        let inside: Vec<(usize, usize)> = spans
            .iter()
            .filter(|span| span.start >= start && span.start + span.len <= end)
            .map(|span| (span.start - start, span.len))
            .collect();
        Self {
            start,
            excerpt: Excerpt {
                text: line[start..end].to_owned(),
                focus: inside.first().map_or(0, |&(offset, _)| offset),
                before: start > 0,
                after: end < line.len(),
            },
            spans: inside,
        }
    }
}

/// Up to `limit` windows of `line` that do not overlap, best first: the most distinct
/// terms weighted by rarity, then the closest together, then the most matches, then
/// the earliest. Terms close together usually belong to one record.
fn windows(line: &str, spans: &[Span], idf: &[f64], limit: usize) -> Vec<Window> {
    if line.len() <= EXCERPT || spans.is_empty() {
        return vec![Window::at(line, 0, spans)];
    }
    let last = line.len() - EXCERPT;
    // A window that starts `LEAD` bytes before each match, scored by what it holds.
    let mut scored: Vec<(usize, u32, f64, usize, usize)> = Vec::new();
    for span in spans {
        let start = line.floor_char_boundary(span.start.saturating_sub(LEAD).min(last));
        if scored
            .last()
            .is_some_and(|&(previous, ..)| previous == start)
        {
            continue;
        }
        let end = start + EXCERPT;
        let first = spans.partition_point(|inner| inner.start < start);
        let inside: Vec<Span> = spans[first..]
            .iter()
            .take_while(|inner| inner.start < end)
            .filter(|inner| inner.start + inner.len <= end)
            .copied()
            .collect();
        let (mut seen, mut weight) = (0u32, 0.0);
        for inner in &inside {
            if seen & 1 << inner.term == 0 {
                seen |= 1 << inner.term;
                weight += idf[inner.term];
            }
        }
        scored.push((
            start,
            seen.count_ones(),
            weight,
            tightest(&inside, seen),
            inside.len(),
        ));
    }
    scored.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then(b.2.total_cmp(&a.2))
            .then(a.3.cmp(&b.3))
            .then(b.4.cmp(&a.4))
            .then(a.0.cmp(&b.0))
    });
    let mut chosen: Vec<usize> = Vec::new();
    for (start, ..) in scored {
        if chosen.len() == limit {
            break;
        }
        if chosen
            .iter()
            .all(|&other| start + EXCERPT <= other || other + EXCERPT <= start)
        {
            chosen.push(start);
        }
    }
    chosen
        .into_iter()
        .map(|start| Window::at(line, start, spans))
        .collect()
}

/// Bytes of the shortest run of `spans`, in line order, that holds every term in `terms`.
fn tightest(spans: &[Span], terms: u32) -> usize {
    let mut counts = [0u32; MAX_TERMS];
    let mut covered = 0u32;
    let mut best = usize::MAX;
    let mut left = 0;
    for right in spans {
        counts[right.term] += 1;
        covered |= 1 << right.term;
        while covered == terms && left < spans.len() {
            let first = spans[left];
            best = best.min((right.start + right.len).saturating_sub(first.start));
            counts[first.term] -= 1;
            if counts[first.term] == 0 {
                covered &= !(1 << first.term);
            }
            left += 1;
        }
    }
    best
}

struct Candidate {
    line: usize,
    distinct: usize,
    score: f64,
    /// Matches of any term in the line, counted in full.
    matches: usize,
    /// The line's best window; other windows are found only if there is room for them.
    first: Window,
}

/// Ranks distinct terms matched, then score, then the earlier line: greater is better.
fn rank(
    (distinct, score, line): (usize, f64, usize),
    (other_distinct, other_score, other_line): (usize, f64, usize),
) -> Ordering {
    distinct
        .cmp(&other_distinct)
        .then(score.total_cmp(&other_score))
        .then(other_line.cmp(&line))
}

impl Candidate {
    fn key(&self) -> (usize, f64, usize) {
        (self.distinct, self.score, self.line)
    }
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        rank(self.key(), other.key())
    }
}

/// Line numbers in ascending runs, at most `RANGE_LIMIT` runs.
#[derive(Default)]
struct Runs(Vec<(usize, usize)>);

impl Runs {
    fn add(&mut self, line: usize) {
        if let Some((_, end)) = self.0.last_mut()
            && *end + 1 == line
        {
            *end = line;
        } else if self.0.len() < RANGE_LIMIT {
            self.0.push((line, line));
        }
    }

    /// The first runs that remain once the `shown` lines, in ascending order, are
    /// taken out.
    fn without(&self, shown: &[usize], wanted: usize) -> Vec<(usize, usize)> {
        let mut left = Vec::new();
        for &(first, last) in &self.0 {
            let mut start = first;
            let from = shown.partition_point(|&line| line < first);
            for &line in shown[from..].iter().take_while(|&&line| line <= last) {
                if line > start {
                    left.push((start, line - 1));
                }
                start = line + 1;
            }
            if start <= last {
                left.push((start, last));
            }
            if left.len() >= wanted {
                left.truncate(wanted);
                break;
            }
        }
        left
    }
}

/// An excerpt chosen for the preview.
struct Shown {
    line: usize,
    start: usize,
    distinct: usize,
    /// Matches inside the shown text.
    matches: usize,
    entry: String,
}

/// Excerpts placed within the body budget.
struct Layout {
    body: usize,
    used: usize,
    shown: Vec<Shown>,
}

impl Layout {
    fn has(&self, line: usize) -> bool {
        self.shown.iter().any(|shown| shown.line == line)
    }

    /// Adds the window clipped to `limit` when it fits. An anchor is skipped once any
    /// part of its line is shown.
    fn place(
        &mut self,
        line: usize,
        distinct: usize,
        window: &Window,
        limit: usize,
        anchor: bool,
    ) -> bool {
        if self
            .shown
            .iter()
            .any(|shown| shown.line == line && (anchor || shown.start == window.start))
        {
            return false;
        }
        let marker = format!("stdout:{line}: ");
        let cap = limit.min(self.body.saturating_sub(marker.len() + 1));
        let (text, from, to) = window.excerpt.render(cap);
        let entry = format!("{marker}{text}\n");
        if cap == 0 || self.used + entry.len() > self.body {
            return false;
        }
        self.used += entry.len();
        let matches = window
            .spans
            .iter()
            .filter(|&&(start, len)| start >= from && start + len <= to)
            .count();
        self.shown.push(Shown {
            line,
            start: window.start,
            distinct,
            matches,
            entry,
        });
        true
    }
}

/// What the last line of a preview reports.
struct Trailer<'a> {
    several: bool,
    /// Matching lines not shown: with every term, and with only some.
    every: usize,
    some: usize,
    /// Matches inside shown lines that their excerpts leave out.
    inside: usize,
    /// Some line was longer than `LINE_LIMIT`.
    cut: bool,
    /// ` (12-14, 20, …)` after the count the runs belong to, and the command for the
    /// first of them.
    runs: &'a str,
    log: &'a str,
}

impl Trailer<'_> {
    fn render(&self) -> String {
        let mut text = String::from("[focus] ");
        if self.every == 0 && self.some == 0 {
            if self.cut {
                // A cut line may match past the part that was read.
            } else if self.inside == 0 {
                return "[focus] every match is shown\n".to_owned();
            } else {
                text.push_str("every matching line is shown");
            }
        } else if self.several {
            let (every_runs, some_runs) = if self.every > 0 {
                (self.runs, "")
            } else {
                ("", self.runs)
            };
            text.push_str(&format!(
                "{} more match every term{every_runs}, {} more match some{some_runs}",
                self.every, self.some
            ));
        } else {
            text.push_str(&format!("{} more match{}", self.every, self.runs));
        }
        let mut clauses = Vec::new();
        if self.inside > 0 {
            clauses.push(format!(
                "{} more matches inside shown lines; print one record per line to rank them",
                self.inside
            ));
        }
        if self.cut {
            clauses.push(format!(
                "lines over {LINE_LIMIT_SHOWN} were ranked on their first {LINE_LIMIT_SHOWN}"
            ));
        }
        for clause in clauses {
            if !text.ends_with(' ') {
                text.push_str("; ");
            }
            text.push_str(&clause);
        }
        text.push_str(self.log);
        text.push('\n');
        text
    }
}

/// Replaces a long stdout preview with the lines most relevant to `intent`, in log
/// order, within `max_bytes`. The output is UTF-8 and the same for the same input.
/// `justless` starts the commands it names, so they reach run `run_id`.
pub fn preview(
    stdout: &Path,
    intent: &str,
    max_bytes: usize,
    justless: &str,
    run_id: &str,
) -> Result<Vec<u8>> {
    let terms = terms(intent);
    ensure!(!terms.is_empty(), "the intent has no search terms");
    let mut analysis = Analysis::new(&terms);

    let mut lines = 0;
    let mut words = 0;
    let mut cut = false;
    let mut frequency = vec![0usize; terms.len()];
    each_line(stdout, |_, line, clipped| {
        analysis.run(line, &terms);
        lines += 1;
        words += analysis.words;
        cut |= clipped;
        for (count, &n) in frequency.iter_mut().zip(&analysis.counts) {
            *count += usize::from(n > 0);
        }
    })?;
    let shown_intent = show_intent(intent);
    if frequency.iter().all(|&count| count == 0) {
        let cut = cut.then_some((justless, run_id));
        return fallback(stdout, &shown_intent, lines, max_bytes, cut);
    }

    let average = if words == 0 {
        1.0
    } else {
        words as f64 / lines as f64
    };
    let idf: Vec<f64> = frequency
        .iter()
        .map(|&df| (1.0 + (lines as f64 - df as f64 + 0.5) / (df as f64 + 0.5)).ln())
        .collect();
    let all = (1u32 << terms.len()) - 1;
    let several = terms.len() > 1;
    let keep = max_bytes / 24 + 1;
    let mut best = BinaryHeap::new();
    let (mut every, mut some, mut total) = (0, 0, 0);
    let (mut every_runs, mut some_runs) = (Runs::default(), Runs::default());
    let mut recent: VecDeque<(usize, u32)> = VecDeque::new();
    let mut nearby = false;
    let mut anchors: Vec<(usize, usize, usize, Window)> = Vec::new();
    each_line(stdout, |number, line, _| {
        analysis.run(line, &terms);
        let distinct = analysis.distinct();
        let anchor = (number == 1 || number == lines) && !line.trim().is_empty();
        if distinct == 0 {
            if anchor {
                anchors.push((number, 0, 0, Window::at(line, 0, &[])));
            }
            return;
        }
        let matches = analysis.matches();
        total += matches;
        if distinct == terms.len() {
            every += 1;
            every_runs.add(number);
        } else {
            some += 1;
            some_runs.add(number);
            // A record printed over several lines spreads its terms among them.
            let mask = analysis.mask();
            while recent
                .front()
                .is_some_and(|&(line, _)| line + NEARBY <= number)
            {
                recent.pop_front();
            }
            nearby |= recent.iter().fold(mask, |union, &(_, other)| union | other) == all;
            recent.push_back((number, mask));
        }
        let length = analysis.words as f64 / average;
        let score: f64 = analysis
            .counts
            .iter()
            .zip(&idf)
            .filter(|&(&count, _)| count > 0)
            .map(|(&count, idf)| {
                let tf = f64::from(count);
                idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * length))
            })
            .sum();
        // Lines that would be dropped at once need no window.
        let dropped = best.len() >= keep
            && best
                .peek()
                .is_some_and(|Reverse(worst): &Reverse<Candidate>| {
                    rank((distinct, score, number), worst.key()).is_le()
                });
        if dropped && !anchor {
            return;
        }
        let first = windows(line, &analysis.spans, &idf, 1).remove(0);
        if anchor {
            anchors.push((number, distinct, matches, first.clone()));
        }
        if dropped {
            return;
        }
        best.push(Reverse(Candidate {
            line: number,
            distinct,
            score,
            matches,
            first,
        }));
        if best.len() > keep {
            best.pop();
        }
    })?;
    let ranked: Vec<Candidate> = best
        .into_sorted_vec()
        .into_iter()
        .map(|Reverse(candidate)| candidate)
        .collect();

    let mut header = if several {
        format!(
            "[focus] {shown_intent}: {every} of {lines} lines match every term, {some} match some\n"
        )
    } else {
        format!("[focus] {shown_intent}: {every} of {lines} lines match\n")
    };
    if several && every == 0 && nearby {
        header.push_str(
            "[focus] no line has every term, but neighboring lines do; print one record per line\n",
        );
    }
    // The trailer's counts only shrink as lines are shown, and its line numbers are
    // at most `lines`, so the widest form bounds it.
    let widest = "9".repeat(lines.to_string().len());
    let widest_runs = format!(" ({widest}-{widest}, {widest}-{widest}, {widest}-{widest}, …)");
    let widest_log = log_clause(justless, run_id, (lines, lines));
    let reserve = |runs: &str, log: &str| {
        [(every, some), (0, 0)]
            .into_iter()
            .map(|(every, some)| {
                Trailer {
                    several,
                    every,
                    some,
                    inside: total,
                    cut,
                    runs,
                    log,
                }
                .render()
                .len()
            })
            .max()
            .unwrap_or(0)
    };
    let full = max_bytes.saturating_sub(header.len() + reserve(&widest_runs, &widest_log));
    let roomy = full >= ROOMY;
    let body = if roomy {
        full
    } else {
        max_bytes.saturating_sub(header.len() + reserve("", ""))
    };

    let mut layout = Layout {
        body,
        used: 0,
        shown: Vec::new(),
    };
    // The best line is always shown, clipped to fit; then the last and first lines,
    // which often hold a total and a title; then the rest in rank order.
    if let Some(top) = ranked.first() {
        layout.place(top.line, top.distinct, &top.first, EXCERPT, false);
    }
    for (line, distinct, _, window) in anchors.iter().rev() {
        layout.place(*line, *distinct, window, ANCHOR, true);
    }
    for candidate in ranked.iter().skip(1) {
        layout.place(
            candidate.line,
            candidate.distinct,
            &candidate.first,
            EXCERPT,
            false,
        );
    }
    // Then, while room remains, more windows of long lines already shown, one per line
    // in rank order at a time. They are found in another pass, only for those lines.
    let room = layout.body.saturating_sub(layout.used) / EXCERPT;
    let mut longer: Vec<&Candidate> = ranked
        .iter()
        .filter(|candidate| {
            candidate.matches > candidate.first.spans.len() && layout.has(candidate.line)
        })
        .collect();
    if room > 0 && !longer.is_empty() {
        let each = room / longer.len() + 2;
        let mut wanted: Vec<usize> = longer.iter().map(|candidate| candidate.line).collect();
        wanted.sort_unstable();
        let mut found: Vec<(usize, Vec<Window>)> = Vec::new();
        each_line(stdout, |number, line, _| {
            if wanted.binary_search(&number).is_ok() {
                analysis.run(line, &terms);
                found.push((number, windows(line, &analysis.spans, &idf, each)));
            }
        })?;
        for index in 1..each {
            let mut placed = false;
            longer.retain(|candidate| {
                let window = found
                    .iter()
                    .find(|(line, _)| *line == candidate.line)
                    .and_then(|(_, windows)| windows.get(index));
                let Some(window) = window else {
                    return false;
                };
                placed |= layout.place(candidate.line, candidate.distinct, window, EXCERPT, false);
                true
            });
            if !placed {
                break;
            }
        }
    }
    let mut shown = layout.shown;
    shown.sort_by_key(|shown| (shown.line, shown.start));

    let mut lines_shown: Vec<(usize, usize)> = Vec::new();
    let mut inside = 0;
    for entry in &shown {
        if lines_shown
            .last()
            .is_none_or(|&(line, _)| line != entry.line)
        {
            lines_shown.push((entry.line, entry.distinct));
            let matches = ranked
                .iter()
                .find(|candidate| candidate.line == entry.line)
                .map(|candidate| candidate.matches)
                .or_else(|| {
                    anchors
                        .iter()
                        .find(|anchor| anchor.0 == entry.line)
                        .map(|anchor| anchor.2)
                })
                .unwrap_or(0);
            let visible: usize = shown
                .iter()
                .filter(|other| other.line == entry.line)
                .map(|other| other.matches)
                .sum();
            inside += matches.saturating_sub(visible);
        }
    }
    let shown_every = lines_shown
        .iter()
        .filter(|&&(_, distinct)| distinct == terms.len())
        .count();
    let shown_some = lines_shown
        .iter()
        .filter(|&&(_, distinct)| distinct > 0 && distinct < terms.len())
        .count();
    let (every, some) = (every - shown_every, some - shown_some);

    let numbers: Vec<usize> = lines_shown.iter().map(|&(line, _)| line).collect();
    let (runs, left) = if every > 0 {
        (&every_runs, every)
    } else {
        (&some_runs, some)
    };
    let next = if roomy {
        runs.without(&numbers, RANGES_SHOWN)
    } else {
        Vec::new()
    };
    let listed: usize = next.iter().map(|&(first, last)| last - first + 1).sum();
    let runs_text = if next.is_empty() {
        String::new()
    } else {
        let mut parts: Vec<String> = next
            .iter()
            .map(|&(first, last)| {
                if first == last {
                    first.to_string()
                } else {
                    format!("{first}-{last}")
                }
            })
            .collect();
        if listed < left {
            parts.push("…".to_owned());
        }
        format!(" ({})", parts.join(", "))
    };
    let log = next
        .first()
        .map(|&range| log_clause(justless, run_id, range))
        .unwrap_or_default();

    let mut output = header;
    for shown in &shown {
        output.push_str(&shown.entry);
    }
    output.push_str(
        &Trailer {
            several,
            every,
            some,
            inside,
            cut,
            runs: &runs_text,
            log: &log,
        }
        .render(),
    );
    ensure!(
        output.len() <= max_bytes,
        "the focused preview needs more than {max_bytes} bytes"
    );
    Ok(output.into_bytes())
}

/// ` · justless log RUN --stream stdout --from A --to B`.
fn log_clause(justless: &str, run_id: &str, (first, last): (usize, usize)) -> String {
    format!(" · {justless} log {run_id} --stream stdout --from {first} --to {last}")
}

/// A long stdout without an intent: whole lines from its start and its end within
/// `max_bytes`, around one line that names the lines left out and the command that
/// replays them. A line too long for its half is cut and marked with an ellipsis, and
/// counts as left out. Returns `None` when the usual preview should stay.
pub fn outline(
    stdout: &Path,
    max_bytes: usize,
    justless: &str,
    run_id: &str,
) -> Result<Option<Vec<u8>>> {
    let mut file = File::open(stdout)?;
    let size = file.metadata()?.len();
    let mut newlines = 0;
    let mut last = b'\n';
    {
        let mut reader = BufReader::new(&file);
        loop {
            let chunk = reader.fill_buf()?;
            let Some(&end) = chunk.last() else { break };
            newlines += newlines_in(chunk);
            last = end;
            let length = chunk.len();
            reader.consume(length);
        }
    }
    let lines = newlines + usize::from(last != b'\n');
    let marker = |from: usize, to: usize, cut: bool| {
        let range = if from == to {
            format!("line {from}")
        } else {
            format!("lines {from}-{to}")
        };
        let what = if cut { "not shown in full" } else { "left out" };
        format!(
            "[justless] stdout {range} {what}{}\n",
            log_clause(justless, run_id, (from, to))
        )
    };
    // Room for the widest marker and two ellipses with their newline.
    let widest = marker(lines, lines + 1, true).len();
    let body = max_bytes.saturating_sub(widest + 2 * '…'.len_utf8() + 1);
    if body as u64 >= size {
        return Ok(None);
    }

    let head = read_at(&mut file, 0, body / 2)?;
    let mut cut = false;
    let (head, head_end, head_lines) = match head.iter().rposition(|&byte| byte == b'\n') {
        Some(index) => (
            head[..=index].to_vec(),
            index + 1,
            newlines_in(&head[..=index]),
        ),
        None => {
            cut = true;
            let end = whole_chars(&head);
            ([&head[..end], "…\n".as_bytes()].concat(), end, 0)
        }
    };
    let room = body - head.len();
    let start = size.saturating_sub(room as u64).max(head_end as u64);
    let at_line = start == 0 || read_at(&mut file, start - 1, 1)? == b"\n";
    let tail = read_at(&mut file, start, room)?;
    let inner = tail.strip_suffix(b"\n").unwrap_or(&tail);
    let (tail, whole) = match inner.iter().position(|&byte| byte == b'\n') {
        found if at_line || found.is_some() => {
            let tail = found
                .filter(|_| !at_line)
                .map_or(tail.clone(), |index| tail[index + 1..].to_vec());
            let whole =
                newlines_in(&tail) + usize::from(!tail.is_empty() && !tail.ends_with(b"\n"));
            (tail, whole)
        }
        _ => {
            // Part of the last line; a character the start cuts through is dropped.
            cut = true;
            let from = tail.iter().take_while(|&&byte| byte & 0xC0 == 0x80).count();
            (["…".as_bytes(), &tail[from..]].concat(), 0)
        }
    };
    let (from, to) = (head_lines + 1, lines.saturating_sub(whole));
    if from > to {
        return Ok(None);
    }
    let mut output = String::from_utf8_lossy(&head).into_owned();
    output.push_str(&marker(from, to, cut));
    output.push_str(&String::from_utf8_lossy(&tail));
    // Invalid UTF-8 can grow when it is replaced; the usual preview handles it.
    Ok((output.len() <= max_bytes).then(|| output.into_bytes()))
}

fn read_at(file: &mut File, offset: u64, length: usize) -> io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::with_capacity(length);
    file.take(length as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn newlines_in(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&byte| byte == b'\n').count()
}

/// The length of `bytes` without a UTF-8 character that their end cuts through.
fn whole_chars(bytes: &[u8]) -> usize {
    let mut start = bytes.len();
    while start > 0 && bytes.len() - start < 3 && bytes[start - 1] & 0xC0 == 0x80 {
        start -= 1;
    }
    match start.checked_sub(1).map(|lead| (lead, bytes[lead])) {
        Some((lead, byte)) if byte >= 0xC0 => {
            let width = match byte {
                0xF0.. => 4,
                0xE0.. => 3,
                _ => 2,
            };
            if lead + width > bytes.len() {
                lead
            } else {
                bytes.len()
            }
        }
        _ => bytes.len(),
    }
}

/// When no line matches, the first and last lines with their markers. With the run ID
/// of a stdout that had lines too long to read whole, it says so and names the search
/// that reads them.
fn fallback(
    stdout: &Path,
    intent: &str,
    lines: usize,
    max_bytes: usize,
    cut: Option<(&str, &str)>,
) -> Result<Vec<u8>> {
    let (header, trailer) = match cut {
        None => (
            format!("[focus] no line matches {intent}; first and last of {lines} lines:\n"),
            String::new(),
        ),
        Some((justless, run_id)) => (
            format!(
                "[focus] no line matches {intent} in its first {LINE_LIMIT_SHOWN}; first and last of {lines} lines:\n"
            ),
            format!("[focus] {justless} search {run_id} TEXT reads whole lines\n"),
        ),
    };
    let body = max_bytes.saturating_sub(header.len() + trailer.len());
    let entry = |number: usize, line: &str| {
        let marker = format!("stdout:{number}: ");
        let cap = ANCHOR.min(body.saturating_sub(marker.len() + 1));
        format!("{marker}{}\n", Excerpt::new(line, 0).render(cap).0)
    };
    let mut head = Vec::new();
    let mut head_bytes = 0;
    let mut tail = VecDeque::new();
    let mut tail_bytes = 0;
    each_line(stdout, |number, line, _| {
        let entry = entry(number, line);
        if tail.is_empty() && head_bytes + entry.len() <= body / 3 {
            head_bytes += entry.len();
            head.push(entry);
            return;
        }
        tail_bytes += entry.len();
        tail.push_back(entry);
        while tail_bytes > body - head_bytes {
            tail_bytes -= tail.pop_front().map_or(0, |entry: String| entry.len());
        }
    })?;
    let mut output = header;
    for entry in head.iter().chain(&tail) {
        output.push_str(entry);
    }
    output.push_str(&trailer);
    ensure!(
        output.len() <= max_bytes,
        "the focused preview needs more than {max_bytes} bytes"
    );
    Ok(output.into_bytes())
}

/// The intent on one line, at most 60 bytes, in double quotes unless it has its own.
fn show_intent(intent: &str) -> String {
    let flat: String = intent
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let clipped = if flat.len() <= INTENT_SHOWN {
        flat
    } else {
        format!("{}…", &flat[..flat.floor_char_boundary(INTENT_SHOWN)])
    };
    if clipped.contains('"') {
        clipped
    } else {
        format!("\"{clipped}\"")
    }
}

/// Calls `visit` with each line's number, its text without ANSI escapes or the line
/// ending, and whether it was cut at `LINE_LIMIT`. A final unterminated line counts.
fn each_line(path: &Path, mut visit: impl FnMut(usize, &str, bool)) -> io::Result<()> {
    let mut file = BufReader::new(File::open(path)?);
    let mut raw = Vec::new();
    let mut number = 0;
    while let Some(cut) = read_line(&mut file, &mut raw)? {
        number += 1;
        let text = String::from_utf8_lossy(&raw);
        let text = ANSI.replace_all(&text, "");
        visit(number, text.trim_end_matches(['\n', '\r']), cut);
        raw.clear();
    }
    Ok(())
}

/// Reads one line, keeping at most `LINE_LIMIT` bytes of it. Returns whether bytes
/// were dropped, or `None` at the end of the file.
fn read_line(file: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<Option<bool>> {
    let mut read = false;
    let mut cut = false;
    loop {
        let chunk = file.fill_buf()?;
        if chunk.is_empty() {
            return Ok(read.then_some(cut));
        }
        read = true;
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let length = newline.map_or(chunk.len(), |index| index + 1);
        let kept = length.min(LINE_LIMIT.saturating_sub(line.len()));
        // The newline itself may be the byte that does not fit.
        cut |= chunk[..length][kept..].iter().any(|&byte| byte != b'\n');
        line.extend_from_slice(&chunk[..kept]);
        file.consume(length);
        if newline.is_some() {
            return Ok(Some(cut));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(intent: &str) -> Vec<String> {
        terms(intent).into_iter().map(|term| term.text).collect()
    }

    #[test]
    fn stems_fold_plurals_and_verb_endings() {
        for (word, stemmed) in [
            ("timeouts", "timeout"),
            ("parse", "pars"),
            ("parsed", "pars"),
            ("parsing", "pars"),
            ("parses", "pars"),
            ("queries", "query"),
            ("query", "query"),
            ("status", "status"),
            ("class", "class"),
            ("classes", "class"),
            ("matches", "match"),
            ("boxes", "box"),
            ("errors", "error"),
            ("this", "this"),
            ("ring", "ring"),
            ("need", "need"),
            ("e0382", "e0382"),
            ("sha256s", "sha256s"),
        ] {
            assert_eq!(stem(word), stemmed, "{word}");
        }
        // The length filter in `Analysis::run` relies on this bound.
        for word in ["seeings", "queries", "classes", "buildings", "parsed", "ab"] {
            let stemmed = stem(word);
            assert!(stemmed.len() <= word.len() && stemmed.len() + 5 >= word.len());
        }
    }

    #[test]
    fn terms_drop_common_words_and_keep_phrases_and_digits() {
        assert_eq!(
            texts("what are the payments timeouts"),
            ["payment", "timeout"]
        );
        assert_eq!(texts("what is the"), Vec::<String>::new());
        assert_eq!(
            texts("errors \"Connection Reset\" in E0382"),
            ["error", "connection reset", "e0382"]
        );
        assert_eq!(texts("a b 7 x"), ["7"]);
        assert_eq!(
            texts("parseConfig request_id"),
            ["pars", "config", "request", "id"]
        );
        assert_eq!(texts("timeout timeouts TIMEOUT"), ["timeout"]);
        assert_eq!(texts("unclosed \"quote here"), ["unclos", "quot", "her"]);
        assert_eq!(texts("\"\" empty"), ["empty"]);
        let many: String = (0..40).map(|n| format!("w{n} ")).collect();
        assert_eq!(terms(&many).len(), MAX_TERMS);
        assert!(terms("\"the\"")[0].phrase);
    }

    #[test]
    fn tokens_split_camel_case_but_not_digits() {
        let mut seen = Vec::new();
        tokens(
            "fooBar E0382 sha256 HTTPServer café-✓",
            |start, token| seen.push((start, token.to_owned())),
        );
        assert_eq!(
            seen,
            [
                (0, "foo".into()),
                (3, "Bar".into()),
                (7, "E0382".into()),
                (13, "sha256".into()),
                (20, "HTTPServer".into()),
                (31, "café".into()),
            ]
        );
    }

    #[test]
    fn excerpts_window_long_lines_within_the_cap() {
        let line = format!("{}payments timeout{}", "é".repeat(400), "x".repeat(500));
        let focus = line.find("payments").unwrap();
        let excerpt = Excerpt::new(&line, focus);
        for cap in [20, 64, 100, 240] {
            let (shown, ..) = excerpt.render(cap);
            assert!(shown.len() <= cap, "{cap}: {}", shown.len());
            assert!(shown.starts_with('…') && shown.ends_with('…'), "{shown}");
            assert!(shown.contains("pay"), "{cap}: {shown}");
        }
        assert!(excerpt.render(240).0.contains("payments timeout"));
        assert_eq!(Excerpt::new("short", 0).render(240).0, "short");
    }

    #[test]
    fn windows_prefer_the_part_with_every_term() {
        // "sell" appears early alone; "pnut" and "sell" appear together later.
        let line = format!(
            "{}side sell{}market pnut side buy{}market pnut side sell{}",
            "x ".repeat(100),
            " filler".repeat(60),
            " filler".repeat(60),
            " filler".repeat(60)
        );
        let terms = terms("pnut sell");
        let mut analysis = Analysis::new(&terms);
        analysis.run(&line, &terms);
        let found = windows(&line, &analysis.spans, &[1.0, 1.0], 3);
        assert!(
            found[0].excerpt.text.contains("market pnut side sell"),
            "{}",
            found[0].excerpt.text
        );
        assert_eq!(found.len(), 3);
        for pair in found.windows(2) {
            assert!(pair[0].start.abs_diff(pair[1].start) >= EXCERPT);
        }
    }

    #[test]
    fn runs_leave_out_shown_lines() {
        let mut runs = Runs::default();
        for line in [3, 4, 5, 6, 9, 12, 13] {
            runs.add(line);
        }
        assert_eq!(runs.without(&[], 5), [(3, 6), (9, 9), (12, 13)]);
        assert_eq!(runs.without(&[3, 5, 9, 13], 5), [(4, 4), (6, 6), (12, 12)]);
        assert_eq!(runs.without(&[1, 4], 2), [(3, 3), (5, 6)]);
        assert!(runs.without(&[3, 4, 5, 6, 9, 12, 13], 3).is_empty());
    }

    #[test]
    fn long_lines_report_their_cut() {
        let mut reader = io::Cursor::new(format!("{}\nshort\n", "a".repeat(LINE_LIMIT + 3)));
        let mut line = Vec::new();
        assert_eq!(read_line(&mut reader, &mut line).unwrap(), Some(true));
        assert_eq!(line.len(), LINE_LIMIT);
        line.clear();
        assert_eq!(read_line(&mut reader, &mut line).unwrap(), Some(false));
        assert_eq!(line, b"short\n");
        line.clear();
        assert_eq!(read_line(&mut reader, &mut line).unwrap(), None);
        // A line of exactly the limit keeps everything but its newline.
        let mut reader = io::Cursor::new(format!("{}\n", "b".repeat(LINE_LIMIT)));
        assert_eq!(read_line(&mut reader, &mut line).unwrap(), Some(false));
    }

    #[test]
    fn intents_with_their_own_quotes_are_not_quoted_again() {
        assert_eq!(show_intent("payments timeouts"), "\"payments timeouts\"");
        assert_eq!(show_intent("\"connection reset\""), "\"connection reset\"");
        assert_eq!(show_intent("a\nb"), "\"a b\"");
    }

    fn write(lines: &[String]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            std::io::Write::write_all(&mut file, line.as_bytes()).unwrap();
            std::io::Write::write_all(&mut file, b"\n").unwrap();
        }
        file
    }

    fn focused(file: &tempfile::NamedTempFile, intent: &str, budget: usize) -> String {
        let preview = preview(file.path(), intent, budget, "justless", "run-Ab12Cd34Ef56").unwrap();
        assert!(preview.len() <= budget, "{budget}: {}", preview.len());
        String::from_utf8(preview).unwrap()
    }

    #[test]
    fn preview_fits_the_smallest_budget_with_long_terms_and_multibyte_text() {
        let long = "x".repeat(300);
        let lines: Vec<String> = (1..=400)
            .map(|n| {
                if n % 7 == 0 {
                    format!(
                        "{} café ✓ payments {long} timeout {n} {}",
                        "é".repeat(2000),
                        "✓".repeat(2000)
                    )
                } else {
                    format!("line {n} ok")
                }
            })
            .collect();
        let file = write(&lines);
        let intents = [
            format!("payments timeout {long}"),
            "payments ".to_owned() + &(0..15).map(|n| format!("term{n}word ")).collect::<String>(),
            "\"café ✓ payments\" timeouts".to_owned(),
        ];
        for intent in &intents {
            for budget in [512, 700, 1024, 4096] {
                let text = focused(&file, intent, budget);
                assert!(text.starts_with("[focus] \""), "{text}");
                assert!(text.contains("stdout:7: "), "{budget} {intent}: {text}");
                assert!(
                    text.lines().last().unwrap().starts_with("[focus] "),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn preview_without_matches_shows_marked_head_and_tail() {
        let lines: Vec<String> = (1..=3000).map(|n| format!("line {n} ok")).collect();
        let file = write(&lines);
        let text = focused(&file, "zebra", 1024);
        assert!(
            text.starts_with("[focus] no line matches \"zebra\"; first and last of 3000 lines:\n"),
            "{text}"
        );
        assert!(text.contains("stdout:1: line 1 ok\n"), "{text}");
        assert!(text.ends_with("stdout:3000: line 3000 ok\n"), "{text}");
    }

    #[test]
    fn preview_fills_the_budget_from_one_long_line() {
        let record = |n: usize, symbol: &str, side: &str| {
            format!("{{\"order_id\": {n}, \"market_symbol\": \"{symbol}\", \"side\": \"{side}\"}}")
        };
        let mut records = Vec::new();
        for n in 0..300 {
            let (symbol, side) = match n % 10 {
                3 => ("PNUTUSDT", "SELL"),
                7 => ("PNUTUSDT", "BUY"),
                _ => ("BTCUSDT", if n % 2 == 0 { "SELL" } else { "BUY" }),
            };
            records.push(record(n, symbol, side));
        }
        let file = write(&[format!("[{}]", records.join(", "))]);
        let text = focused(&file, "PNUTUSDT SELL", 4096);
        let excerpts: Vec<&str> = text
            .lines()
            .filter(|line| line.starts_with("stdout:1: "))
            .collect();
        assert!(excerpts.len() > 8, "{text}");
        // The first window holds both terms, not a SELL of another symbol.
        assert!(
            excerpts[0].contains("\"PNUTUSDT\", \"side\": \"SELL\""),
            "{text}"
        );
        assert!(text.len() > 3000, "{}", text.len());
        let trailer = text.lines().last().unwrap();
        assert!(
            trailer.starts_with("[focus] every matching line is shown; ")
                && trailer.ends_with(
                    " more matches inside shown lines; print one record per line to rank them"
                ),
            "{trailer}"
        );
    }

    #[test]
    fn preview_lists_lines_left_out_for_log() {
        let lines: Vec<String> = (1..=600)
            .map(|n| match n % 3 {
                0 => format!("{n}: payments timeout request_id={n}"),
                1 => format!("{n}: orders timeout"),
                _ => format!("{n}: fine"),
            })
            .collect();
        let file = write(&lines);
        let text = focused(&file, "payments timeouts", 1024);
        let shown: Vec<usize> = text
            .lines()
            .filter_map(|line| line.strip_prefix("stdout:"))
            .map(|rest| rest.split(':').next().unwrap().parse().unwrap())
            .collect();
        let trailer = text.lines().last().unwrap();
        // The listed lines match every term and none of them is shown already.
        let listed = trailer
            .split_once("every term (")
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(list, _)| list)
            .unwrap_or_else(|| panic!("{trailer}"));
        let first: usize = listed.split([',', '-']).next().unwrap().parse().unwrap();
        assert_eq!(first % 3, 0, "{trailer}");
        assert!(!shown.contains(&first), "{text}");
        assert!(
            trailer.ends_with(&format!(
                " · justless log run-Ab12Cd34Ef56 --stream stdout --from {first} --to {first}"
            )),
            "{trailer}"
        );
    }

    #[test]
    fn preview_hints_at_records_split_across_lines() {
        let mut lines = Vec::new();
        for n in 0..400 {
            lines.push("{".to_owned());
            lines.push(format!(
                "  \"symbol\": \"{}\",",
                if n % 5 == 0 { "PNUT" } else { "BTC" }
            ));
            lines.push(format!(
                "  \"side\": \"{}\"",
                if n % 2 == 0 { "SELL" } else { "BUY" }
            ));
            lines.push("},".to_owned());
        }
        let file = write(&lines);
        let text = focused(&file, "PNUT SELL", 4096);
        assert!(
            text.lines().nth(1).unwrap()
                == "[focus] no line has every term, but neighboring lines do; print one record per line",
            "{text}"
        );
        let text = focused(&file, "PNUT", 4096);
        assert!(!text.contains("neighboring"), "{text}");
    }

    #[test]
    fn matches_past_the_line_limit_are_not_denied() {
        let tail = "quota_exceeded";
        let line = format!("{}{tail}", "ok ".repeat(LINE_LIMIT / 3 + 10));
        let file = write(&[line.clone(), "other".to_owned()]);
        // Nothing matched in what was read, which the preview says, with a way to
        // search whole lines.
        let text = focused(&file, tail, 1024);
        assert!(
            text.starts_with(
                "[focus] no line matches \"quota_exceeded\" in its first 4 MiB; first and last of 2 lines:\n"
            ),
            "{text}"
        );
        assert!(
            text.ends_with(
                "stdout:2: other\n[focus] justless search run-Ab12Cd34Ef56 TEXT reads whole lines\n"
            ),
            "{text}"
        );
        let file = write(&[line, format!("the {tail} line")]);
        let text = focused(&file, tail, 4096);
        assert!(text.contains("stdout:2: the quota_exceeded line"), "{text}");
        assert!(!text.contains("every match"), "{text}");
        assert!(
            text.lines()
                .last()
                .unwrap()
                .ends_with("lines over 4 MiB were ranked on their first 4 MiB"),
            "{text}"
        );
    }

    fn outlined(bytes: &[u8], budget: usize) -> String {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, bytes).unwrap();
        let shown = outline(file.path(), budget, "justless", "run-Ab12Cd34Ef56")
            .unwrap()
            .expect("an outline");
        assert!(shown.len() <= budget, "{budget}: {}", shown.len());
        String::from_utf8(shown).unwrap()
    }

    #[test]
    fn outline_keeps_whole_lines_and_names_the_rest() {
        let lines: Vec<String> = (1..=3000).map(|n| format!("row {n} ok")).collect();
        let text = outlined(format!("{}\n", lines.join("\n")).as_bytes(), 1024);
        let marker = text
            .lines()
            .find(|line| line.starts_with("[justless] "))
            .unwrap();
        let (from, to) = marker
            .split_once(" --from ")
            .and_then(|(_, range)| range.split_once(" --to "))
            .map(|(from, to)| (from.parse::<usize>().unwrap(), to.parse::<usize>().unwrap()))
            .unwrap();
        assert_eq!(
            marker,
            format!(
                "[justless] stdout lines {from}-{to} left out · justless log run-Ab12Cd34Ef56 --stream stdout --from {from} --to {to}"
            )
        );
        // The shown lines and the range add up to the whole stream, in order.
        let shown: Vec<&str> = text
            .lines()
            .filter(|line| !line.starts_with("[justless]"))
            .collect();
        let mut expected: Vec<&str> = lines[..from - 1].iter().map(String::as_str).collect();
        expected.extend(lines[to..].iter().map(String::as_str));
        assert_eq!(shown, expected);
        // Most of the budget is used.
        assert!(text.len() > 900, "{}", text.len());
    }

    #[test]
    fn outline_cuts_a_long_line_on_character_boundaries() {
        let text = outlined(format!("{}\n", "é✓".repeat(5000)).as_bytes(), 1024);
        let mut parts = text.lines();
        let head = parts.next().unwrap();
        assert!(head.starts_with("é✓") && head.ends_with('…'), "{head}");
        assert_eq!(
            parts.next().unwrap(),
            "[justless] stdout line 1 not shown in full · justless log run-Ab12Cd34Ef56 --stream stdout --from 1 --to 1"
        );
        let tail = parts.next().unwrap();
        assert!(tail.starts_with('…') && tail.ends_with("é✓"), "{tail}");
        assert!(text.len() > 900, "{}", text.len());

        // An unterminated last line is kept whole when it fits.
        let mut bytes: String = (1..=500).map(|n| format!("line {n}\n")).collect();
        bytes.push_str("last words");
        let text = outlined(bytes.as_bytes(), 512);
        assert!(text.ends_with("line 500\nlast words"), "{text}");
        assert!(text.starts_with("line 1\n"), "{text}");
    }

    #[test]
    fn whole_chars_drops_a_character_cut_at_the_end() {
        let text = "aé✓😀".as_bytes();
        assert_eq!(whole_chars(text), text.len());
        assert_eq!(whole_chars(&text[..text.len() - 1]), text.len() - 4);
        assert_eq!(whole_chars(&text[..4]), 3);
        assert_eq!(whole_chars(&text[..2]), 1);
        assert_eq!(whole_chars(b""), 0);
    }
}
