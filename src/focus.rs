//! Ranks the lines of a long stdout against an intent such as "payments timeouts", so
//! a bounded preview shows the lines that matter instead of the head and tail.
//!
//! Ranking is deterministic and offline: BM25 over each line's words, with a light
//! stemmer applied to both sides, plus double-quoted phrases. Lines that contain every
//! term rank first. Each shown line keeps its `stdout:N` marker for `justless log`.

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

/// Bytes of one line that are read; the rest of the line is skipped.
const LINE_LIMIT: usize = 256 * 1024;
const MAX_TERMS: usize = 16;
/// Longest excerpt of a matching line, and of an anchor or fallback line.
const EXCERPT: usize = 240;
const ANCHOR: usize = 160;
/// Bytes kept before the first match when a long line is windowed.
const LEAD: usize = 80;
const INTENT_SHOWN: usize = 60;
const WORD_SHOWN: usize = 40;
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
/// Words with a digit, such as E0382, stay whole.
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

/// Term frequencies of one line, the first match of each term, and its word count.
struct Analysis {
    counts: Vec<u32>,
    /// Byte offset and length of each term's first match.
    first: Vec<Option<(usize, usize)>>,
    words: usize,
}

impl Analysis {
    fn new(terms: usize) -> Self {
        Self {
            counts: vec![0; terms],
            first: vec![None; terms],
            words: 0,
        }
    }

    fn run(&mut self, line: &str, terms: &[Term]) {
        self.counts.fill(0);
        self.first.fill(None);
        self.words = 0;
        let has_words = terms.iter().any(|term| !term.phrase);
        tokens(line, |start, token| {
            self.words += 1;
            if !has_words {
                return;
            }
            let word = stem(&token.to_lowercase());
            for (index, term) in terms.iter().enumerate() {
                if !term.phrase && term.text == word {
                    self.counts[index] += 1;
                    self.first[index].get_or_insert((start, token.len()));
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
                        self.first[index].get_or_insert((start, term.text.len()));
                    }
                }
            }
        }
    }

    fn distinct(&self) -> usize {
        self.counts.iter().filter(|&&count| count > 0).count()
    }

    fn focus(&self) -> usize {
        self.first
            .iter()
            .flatten()
            .map(|&(start, _)| start)
            .min()
            .unwrap_or(0)
    }
}

/// Up to 240 bytes of a line around its first match, enough for any excerpt.
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

    /// The excerpt within `cap` bytes, marking each clipped side with an ellipsis.
    fn render(&self, cap: usize) -> String {
        if !self.before && !self.after && self.text.len() <= cap {
            return self.text.clone();
        }
        let room = cap.saturating_sub(2 * '…'.len_utf8());
        let text = &self.text;
        // A narrow excerpt keeps less before the match, so the match stays visible.
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
        excerpt
    }
}

struct Candidate {
    line: usize,
    distinct: usize,
    score: f64,
    excerpt: Excerpt,
    /// Text of the line's rarest matched term, as printed, for `justless search`.
    word: String,
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

/// A line chosen for the preview.
struct Shown {
    line: usize,
    distinct: usize,
    entry: String,
}

/// Replaces a long stdout preview with the lines most relevant to `intent`, in log
/// order, within `max_bytes`. The output is UTF-8 and the same for the same input.
pub fn preview(stdout: &Path, intent: &str, max_bytes: usize, run_id: &str) -> Result<Vec<u8>> {
    let terms = terms(intent);
    ensure!(!terms.is_empty(), "the intent has no search terms");
    let mut analysis = Analysis::new(terms.len());

    let mut lines = 0;
    let mut words = 0;
    let mut frequency = vec![0usize; terms.len()];
    each_line(stdout, |_, line| {
        analysis.run(line, &terms);
        lines += 1;
        words += analysis.words;
        for (count, &n) in frequency.iter_mut().zip(&analysis.counts) {
            *count += usize::from(n > 0);
        }
    })?;
    let shown_intent = clip_intent(intent);
    if frequency.iter().all(|&count| count == 0) {
        return fallback(stdout, &shown_intent, lines, max_bytes);
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
    // The term to search for: the rarest one, which narrows `justless search` most.
    let mut rarity: Vec<usize> = (0..terms.len()).collect();
    rarity.sort_by_key(|&index| frequency[index]);
    let keep = max_bytes / 24 + 1;
    let mut best = BinaryHeap::new();
    let (mut every, mut some) = (0, 0);
    let mut anchors: Vec<(usize, usize, Excerpt)> = Vec::new();
    each_line(stdout, |number, line| {
        analysis.run(line, &terms);
        let distinct = analysis.distinct();
        if (number == 1 || number == lines) && !line.trim().is_empty() {
            anchors.push((number, distinct, Excerpt::new(line, 0)));
        }
        if distinct == 0 {
            return;
        }
        if distinct == terms.len() {
            every += 1;
        } else {
            some += 1;
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
        // Lines that would be dropped at once need no excerpt.
        if best.len() >= keep
            && best
                .peek()
                .is_some_and(|Reverse(worst): &Reverse<Candidate>| {
                    rank((distinct, score, number), worst.key()).is_le()
                })
        {
            return;
        }
        let word = rarity
            .iter()
            .find_map(|&index| analysis.first[index])
            .map(|(start, length)| line[start..start + length].to_owned())
            .unwrap_or_default();
        best.push(Reverse(Candidate {
            line: number,
            distinct,
            score,
            excerpt: Excerpt::new(line, analysis.focus()),
            word,
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

    let several = terms.len() > 1;
    let header = if several {
        format!(
            "[focus] \"{shown_intent}\": {every} of {lines} lines match every term, {some} match some\n"
        )
    } else {
        format!("[focus] \"{shown_intent}\": {every} of {lines} lines match\n")
    };
    let search = ranked
        .first()
        .and_then(|top| search_clause(run_id, &top.word))
        .unwrap_or_default();
    let trailer = |every: usize, some: usize| {
        if every == 0 && some == 0 {
            "[focus] every matching line is shown\n".to_owned()
        } else if several {
            format!("[focus] {every} more match every term, {some} more match some{search}\n")
        } else {
            format!("[focus] {every} more match{search}\n")
        }
    };
    // The trailer's counts only shrink as lines are shown, so this reserves enough.
    let reserve = trailer(every, some).len().max(trailer(0, 0).len());
    let body = max_bytes.saturating_sub(header.len() + reserve);

    let mut shown: Vec<Shown> = Vec::new();
    let mut used = 0;
    let mut place = |line: usize, distinct: usize, excerpt: &Excerpt, limit: usize| {
        if shown.iter().any(|shown| shown.line == line) {
            return;
        }
        let marker = format!("stdout:{line}: ");
        let cap = limit.min(body.saturating_sub(marker.len() + 1));
        let entry = format!("{marker}{}\n", excerpt.render(cap));
        if cap > 0 && used + entry.len() <= body {
            used += entry.len();
            shown.push(Shown {
                line,
                distinct,
                entry,
            });
        }
    };
    let mut ranked = ranked.iter();
    // The best line is always shown, clipped to fit; then the last and first lines,
    // which often hold a total and a title; then the rest in rank order.
    if let Some(top) = ranked.next() {
        place(top.line, top.distinct, &top.excerpt, EXCERPT);
    }
    for (line, distinct, excerpt) in anchors.iter().rev() {
        place(*line, *distinct, excerpt, ANCHOR);
    }
    for candidate in ranked {
        place(
            candidate.line,
            candidate.distinct,
            &candidate.excerpt,
            EXCERPT,
        );
    }
    shown.sort_by_key(|shown| shown.line);
    let shown_every = shown
        .iter()
        .filter(|shown| shown.distinct == terms.len())
        .count();
    let shown_some = shown
        .iter()
        .filter(|shown| shown.distinct > 0 && shown.distinct < terms.len())
        .count();

    let mut output = header;
    for shown in &shown {
        output.push_str(&shown.entry);
    }
    output.push_str(&trailer(every - shown_every, some - shown_some));
    ensure!(
        output.len() <= max_bytes,
        "the focused preview needs more than {max_bytes} bytes"
    );
    Ok(output.into_bytes())
}

/// When no line matches, the first and last lines with their markers.
fn fallback(stdout: &Path, intent: &str, lines: usize, max_bytes: usize) -> Result<Vec<u8>> {
    let header =
        format!("[focus] no line matches \"{intent}\"; first and last of {lines} lines:\n");
    let body = max_bytes.saturating_sub(header.len());
    let entry = |number: usize, line: &str| {
        let marker = format!("stdout:{number}: ");
        let cap = ANCHOR.min(body.saturating_sub(marker.len() + 1));
        format!("{marker}{}\n", Excerpt::new(line, 0).render(cap))
    };
    let mut head = Vec::new();
    let mut head_bytes = 0;
    let mut tail = VecDeque::new();
    let mut tail_bytes = 0;
    each_line(stdout, |number, line| {
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
    ensure!(
        output.len() <= max_bytes,
        "the focused preview needs more than {max_bytes} bytes"
    );
    Ok(output.into_bytes())
}

/// ` · justless search RUN WORD`, or nothing when the word cannot be quoted compactly.
fn search_clause(run_id: &str, word: &str) -> Option<String> {
    let word = word.get(..word.floor_char_boundary(word.len().min(WORD_SHOWN)))?;
    let word = word.trim();
    if word.is_empty() {
        return None;
    }
    let quoted = shlex::try_quote(word).ok()?;
    (quoted.len() <= WORD_SHOWN + 16).then(|| format!(" · justless search {run_id} {quoted}"))
}

/// The intent on one line, at most 60 bytes.
fn clip_intent(intent: &str) -> String {
    let flat: String = intent
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if flat.len() <= INTENT_SHOWN {
        return flat;
    }
    format!("{}…", &flat[..flat.floor_char_boundary(INTENT_SHOWN)])
}

/// Calls `visit` with each line's number and text, without ANSI escapes or the line
/// ending. A line longer than 256 KiB is cut there. A final unterminated line counts.
fn each_line(path: &Path, mut visit: impl FnMut(usize, &str)) -> io::Result<()> {
    let mut file = BufReader::new(File::open(path)?);
    let mut raw = Vec::new();
    let mut number = 0;
    while read_line(&mut file, &mut raw)? {
        number += 1;
        let text = String::from_utf8_lossy(&raw);
        let text = ANSI.replace_all(&text, "");
        visit(number, text.trim_end_matches(['\n', '\r']));
        raw.clear();
    }
    Ok(())
}

fn read_line(file: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<bool> {
    let mut read = false;
    loop {
        let chunk = file.fill_buf()?;
        if chunk.is_empty() {
            return Ok(read);
        }
        read = true;
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let length = newline.map_or(chunk.len(), |index| index + 1);
        let kept = length.min(LINE_LIMIT.saturating_sub(line.len()));
        line.extend_from_slice(&chunk[..kept]);
        file.consume(length);
        if newline.is_some() {
            return Ok(true);
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
            let shown = excerpt.render(cap);
            assert!(shown.len() <= cap, "{cap}: {}", shown.len());
            assert!(shown.starts_with('…') && shown.ends_with('…'), "{shown}");
            assert!(shown.contains("pay"), "{cap}: {shown}");
        }
        assert!(excerpt.render(240).contains("payments timeout"));
        assert_eq!(Excerpt::new("short", 0).render(240), "short");
    }

    fn write(lines: &[String]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            std::io::Write::write_all(&mut file, line.as_bytes()).unwrap();
            std::io::Write::write_all(&mut file, b"\n").unwrap();
        }
        file
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
                let preview = preview(file.path(), intent, budget, "run-Ab12Cd34Ef56").unwrap();
                assert!(preview.len() <= budget, "{budget}: {}", preview.len());
                let text = String::from_utf8(preview).unwrap();
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
        let preview = preview(file.path(), "zebra", 1024, "run-X").unwrap();
        let text = String::from_utf8(preview).unwrap();
        assert!(text.len() <= 1024);
        assert!(
            text.starts_with("[focus] no line matches \"zebra\"; first and last of 3000 lines:\n"),
            "{text}"
        );
        assert!(text.contains("stdout:1: line 1 ok\n"), "{text}");
        assert!(text.ends_with("stdout:3000: line 3000 ok\n"), "{text}");
    }
}
