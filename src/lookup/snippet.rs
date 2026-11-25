//! The lines shown under a result: up to two lines that between them show
//! the most of the question's words, each clipped to the part that shows them.

use std::collections::HashSet;

use super::tokens::{Question, Tokenizer, qualified, words};

/// The longest line shown, in bytes.
const MAX_LINE: usize = 160;
/// How far a clipped line starts before the first word it is clipped around.
const LEAD: usize = 60;
/// Maximum relative leading indentation preserved after trimming shared indent.
const MAX_RELATIVE_INDENT: usize = 8;
const CUT: &str = "…";

/// A question term in a line, with the byte range of the word it came from.
struct Mention {
    term: usize,
    start: usize,
    end: usize,
}

/// A candidate line and the question terms it contains, as a bit set.
struct Line<'a> {
    number: usize,
    text: &'a str,
    mentions: Vec<Mention>,
    terms: u64,
}

fn mentions(line: &str, question: &Question, tokenizer: &mut Tokenizer) -> Vec<Mention> {
    let mut found = Vec::new();
    words(line, |start, word| {
        tokenizer.word(word, |token| {
            if let Ok(term) = question
                .terms
                .binary_search_by(|term| term.as_str().cmp(token))
            {
                found.push(Mention {
                    term,
                    start,
                    end: start + word.len(),
                });
            }
        });
    });
    qualified(line, |start, end, token| {
        if let Ok(term) = question
            .terms
            .binary_search_by(|term| term.as_str().cmp(token))
        {
            found.push(Mention { term, start, end });
        }
    });
    found
}

fn terms(mentions: &[Mention]) -> u64 {
    mentions
        .iter()
        .fold(0, |terms, mention| terms | 1 << mention.term)
}

/// The summed rarity of a set of terms.
fn weight(terms: u64, idf: &[f64]) -> f64 {
    idf.iter()
        .enumerate()
        .filter(|(term, _)| terms & 1 << term != 0)
        .map(|(_, idf)| idf)
        .sum()
}

/// The part of a trimmed line shown when it is clipped from `start`, and
/// whether text is cut before and after it.
fn window(trimmed: &str, start: usize) -> (usize, usize, bool, bool) {
    let start = trimmed.floor_char_boundary(start.min(trimmed.len()));
    let before = start > 0;
    let room = MAX_LINE - if before { CUT.len() } else { 0 };
    if trimmed.len() - start <= room {
        return (start, trimmed.len(), before, false);
    }
    (
        start,
        trimmed.floor_char_boundary(start + room - CUT.len()),
        before,
        true,
    )
}

/// Leading ASCII indentation slice (`' '` and `'\t'`) of `line`.
fn leading_indent(line: &str) -> &str {
    let end = line
        .bytes()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count();
    &line[..end]
}

/// Longest common leading indentation byte length across all non-blank lines.
fn common_indent_len(lines: &[&str]) -> usize {
    let mut shared: Option<&[u8]> = None;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let ws = leading_indent(line).as_bytes();
        shared = Some(match shared {
            None => ws,
            Some(current) => {
                let common = current
                    .iter()
                    .zip(ws)
                    .take_while(|(left, right)| left == right)
                    .count();
                &current[..common]
            }
        });
    }
    shared.map_or(0, <[u8]>::len)
}

/// Byte offset to strip from `line` given `shared` common indentation: drops
/// `shared` and caps any remaining relative indentation to [`MAX_RELATIVE_INDENT`].
fn strip_offset(line: &str, shared: usize) -> usize {
    let ws = leading_indent(line).len();
    let relative = ws.saturating_sub(shared);
    let kept = relative.min(MAX_RELATIVE_INDENT);
    ws - kept
}

/// Formats multi-line code excerpts by skipping blank padding lines, trimming
/// shared leading indentation (while preserving relative indentation up to
/// [`MAX_RELATIVE_INDENT`]), and removing trailing whitespace.
#[cfg(test)]
pub(crate) fn trim_excerpt_lines(lines: &[(usize, &str)]) -> Vec<(usize, String)> {
    let non_blank = lines
        .iter()
        .copied()
        .filter(|(_, text)| !text.trim().is_empty())
        .collect::<Vec<_>>();
    let raw = non_blank.iter().map(|(_, text)| *text).collect::<Vec<_>>();
    let shared = common_indent_len(&raw);
    non_blank
        .into_iter()
        .map(|(number, text)| {
            let strip = strip_offset(text, shared);
            (number, text[strip..].trim_end().to_owned())
        })
        .collect()
}

/// A line trimmed from `strip` and, when longer than [`MAX_LINE`], clipped to
/// the window that shows the rarest terms not in `shown`, then the rarest
/// overall. Also returns the terms the shown text contains.
fn clip(line: &str, strip: usize, mentions: &[Mention], idf: &[f64], shown: u64) -> (String, u64) {
    let trimmed = line[strip..].trim_end();
    if trimmed.len() <= MAX_LINE {
        return (trimmed.to_owned(), terms(mentions));
    }
    let indent = strip;
    let visible = |(from, to, _, _): (usize, usize, bool, bool)| {
        mentions
            .iter()
            .filter(|mention| mention.start >= indent + from && mention.end <= indent + to)
            .fold(0, |terms, mention| terms | 1 << mention.term)
    };
    let mut best = (window(trimmed, 0), visible(window(trimmed, 0)));
    for mention in mentions {
        let candidate = window(
            trimmed,
            mention.start.saturating_sub(indent).saturating_sub(LEAD),
        );
        let terms = visible(candidate);
        let better = (weight(terms & !shown, idf), weight(terms, idf));
        let current = (weight(best.1 & !shown, idf), weight(best.1, idf));
        if better.0 > current.0 || (better.0 == current.0 && better.1 > current.1) {
            best = (candidate, terms);
        }
    }
    let ((from, to, before, after), terms) = best;
    let mut text = String::with_capacity(MAX_LINE);
    if before {
        text.push_str(CUT);
    }
    text.push_str(&trimmed[from..to]);
    if after {
        text.push_str(CUT);
    }
    (text, terms)
}

/// The line with the highest score, the earlier one on ties.
fn best<'a, 'b, S: PartialOrd>(
    lines: impl Iterator<Item = &'b Line<'a>>,
    score: impl Fn(&Line) -> S,
) -> Option<&'b Line<'a>> {
    lines.fold(None, |best, line| match best {
        Some(best) if score(best) >= score(line) => Some(best),
        _ => Some(line),
    })
}

/// Up to two of `candidates`, in line order, as line numbers and shown text.
/// Each is the line with the rarest question terms that neither `header`
/// nor the line before it shows, then the rarest terms overall. The second
/// is never a copy of the first. Shared leading indentation is trimmed while
/// preserving relative indentation across multi-line excerpts, and blank
/// padding lines are skipped.
#[cfg(test)]
pub(crate) fn lines(
    candidates: &[(usize, &str)],
    question: &Question,
    idf: &[f64],
    header: &str,
) -> Vec<(usize, String)> {
    lines_excluding(
        candidates,
        question,
        idf,
        header,
        &HashSet::new(),
        &HashSet::new(),
    )
}

/// Like [`lines`], while skipping line numbers already shown in `excluded_numbers`
/// (from an earlier hit in the same file) and preferring lines whose trimmed text
/// is not already in `excluded_texts`.
pub(crate) fn lines_excluding(
    candidates: &[(usize, &str)],
    question: &Question,
    idf: &[f64],
    header: &str,
    excluded_numbers: &HashSet<usize>,
    excluded_texts: &HashSet<String>,
) -> Vec<(usize, String)> {
    let mut tokenizer = Tokenizer::default();
    let seen = terms(&mentions(header, question, &mut tokenizer));
    let lines = candidates
        .iter()
        .filter(|&&(number, text)| !text.trim().is_empty() && !excluded_numbers.contains(&number))
        .map(|&(number, text)| {
            let mentions = mentions(text, question, &mut tokenizer);
            Line {
                number,
                text,
                terms: terms(&mentions),
                mentions,
            }
        })
        .filter(|line| weight(line.terms, idf) > 0.0)
        .collect::<Vec<_>>();
    let fresh = |line: &Line| !excluded_texts.contains(line.text.trim());
    let Some(first) = best(lines.iter(), |line| {
        (
            fresh(line),
            weight(line.terms & !seen, idf),
            weight(line.terms, idf),
        )
    }) else {
        return Vec::new();
    };
    let (_, first_visible) = clip(
        first.text,
        leading_indent(first.text).len(),
        &first.mentions,
        idf,
        seen,
    );
    let shown = seen | first_visible;
    let others = lines
        .iter()
        .filter(|line| line.number != first.number && line.text.trim() != first.text.trim());
    let second = best(others, |line| {
        (
            fresh(line),
            weight(line.terms & !shown, idf),
            weight(line.terms, idf),
        )
    });
    let shared = match second {
        Some(second) => common_indent_len(&[first.text, second.text]),
        None => leading_indent(first.text).len(),
    };
    let (first_text, visible) = clip(
        first.text,
        strip_offset(first.text, shared),
        &first.mentions,
        idf,
        seen,
    );
    let mut chosen = vec![(first.number, first_text)];
    if let Some(second) = second {
        let (second_text, _) = clip(
            second.text,
            strip_offset(second.text, shared),
            &second.mentions,
            idf,
            seen | visible,
        );
        chosen.push((second.number, second_text));
    }
    chosen.sort_by_key(|(number, _)| *number);
    chosen
}

/// A line shown without a question term in it, clipped from its start.
pub(crate) fn plain(line: &str) -> String {
    clip(line, leading_indent(line).len(), &[], &[], 0).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_line_has_the_rarest_words() {
        let question = Question::parse("retry policy").unwrap();
        assert_eq!(question.terms, ["polici", "retri"]);
        let idf = [0.5, 2.0];
        let candidates = [
            (3, "let policy = 1;"),
            (4, "fn retry() {}"),
            (5, "// no match"),
            (6, "retry with policy"),
        ];
        // No line adds a word, so the next best one follows.
        assert_eq!(
            lines(&candidates, &question, &idf, ""),
            [
                (4, "fn retry() {}".to_owned()),
                (6, "retry with policy".to_owned())
            ]
        );
        assert!(lines(&candidates[2..3], &question, &idf, "").is_empty());
    }

    #[test]
    fn the_second_line_adds_words_the_first_lacks() {
        let question = Question::parse("leader map").unwrap();
        assert_eq!(question.terms, ["leader", "map"]);
        let idf = [1.5, 2.0];
        let candidates = [
            (1, "# change the leader from ctrl-b"),
            (2, "set leader alt-a"),
            (3, "map h focus left"),
            (4, "map l focus right"),
        ];
        let shown = lines(&candidates, &question, &idf, "");
        assert_eq!(
            shown,
            [
                (1, "# change the leader from ctrl-b".to_owned()),
                (3, "map h focus left".to_owned())
            ]
        );
        // A line that only repeats the first is never shown; relative indent is preserved.
        let candidates = [
            (7, "    assert_ready(&map);"),
            (9, "assert_ready(&map);"),
            (12, "map.clear();"),
        ];
        assert_eq!(
            lines(&candidates, &question, &idf, ""),
            [
                (7, "    assert_ready(&map);".to_owned()),
                (12, "map.clear();".to_owned())
            ]
        );
        assert_eq!(
            lines(&candidates[..2], &question, &idf, ""),
            [(7, "assert_ready(&map);".to_owned())]
        );
    }

    #[test]
    fn shared_indent_is_trimmed_preserving_relative_indent_and_skipping_blanks() {
        let question = Question::parse("retry backoff").unwrap();
        let idf = vec![1.5; question.terms.len()];
        let candidates = [
            (10, "            "),
            (11, "            if should_retry(attempt) {"),
            (12, ""),
            (13, "                schedule_backoff(attempt);"),
            (14, "            }"),
        ];
        let shown = lines(&candidates, &question, &idf, "");
        assert_eq!(
            shown,
            [
                (11, "if should_retry(attempt) {".to_owned()),
                (13, "    schedule_backoff(attempt);".to_owned()),
            ]
        );

        // Excessive relative indentation (> MAX_RELATIVE_INDENT) is capped while
        // still preserving nesting and dropping blank padding lines.
        let deep = [
            (1, "        "),
            (2, "        if outer {"),
            (3, "\t"),
            (4, "                            inner_call();"),
            (5, "        }"),
        ];
        assert_eq!(
            trim_excerpt_lines(&deep),
            [
                (2, "if outer {".to_owned()),
                (4, "        inner_call();".to_owned()),
                (5, "}".to_owned()),
            ]
        );

        // Tabs share indentation cleanly too.
        let tabbed = [(1, "\t\tif a {"), (2, "\t\t\tb();")];
        assert_eq!(
            trim_excerpt_lines(&tabbed),
            [(1, "if a {".to_owned()), (2, "\tb();".to_owned())]
        );
    }

    #[test]
    fn excluded_lines_prevent_cross_hit_snippet_duplication() {
        let question = Question::parse("retry backoff").unwrap();
        let idf = vec![1.5; question.terms.len()];
        let candidates = [
            (10, "let retry = true;"),
            (11, "let retry = true;"),
            (12, "let backoff = 250;"),
        ];
        let excluded_numbers = HashSet::from([10]);
        let excluded_texts = HashSet::from(["let retry = true;".to_owned()]);
        let shown = lines_excluding(
            &candidates,
            &question,
            &idf,
            "",
            &excluded_numbers,
            &excluded_texts,
        );
        // Line 10 is excluded by number; line 12 is preferred first over duplicate text on line 11.
        assert_eq!(shown[0].0, 11);
        assert_eq!(shown[1].0, 12);
        // When line 12 alone covers a single-term query, duplicate text on line 11 is skipped
        // in favor of fresh text.
        let single = Question::parse("retry").unwrap();
        let candidates = [
            (10, "let retry = true;"),
            (11, "let retry = true;"),
            (12, "retry_with_limit(5);"),
        ];
        let shown = lines_excluding(
            &candidates,
            &single,
            &[1.0],
            "",
            &excluded_numbers,
            &excluded_texts,
        );
        assert!(shown.iter().any(|(n, _)| *n == 12), "{shown:?}");
        assert!(!shown.iter().any(|(n, _)| *n == 10), "{shown:?}");
    }

    #[test]
    fn qualified_mentions_are_scored_in_snippets() {
        let question = Question::parse("Segment::open").unwrap();
        let idf = question
            .terms
            .iter()
            .map(|term| if term == "segment::open" { 5.0 } else { 1.0 })
            .collect::<Vec<_>>();
        let candidates = [
            (1, "let open = true;"),
            (2, "let segment = 1;"),
            (3, "let s = Segment::open(&path)?;"),
        ];
        let shown = lines(&candidates, &question, &idf, "");
        assert!(
            shown
                .iter()
                .any(|(n, text)| *n == 3 && text.contains("Segment::open")),
            "{shown:?}"
        );
    }

    #[test]
    fn words_in_the_header_count_as_shown() {
        let question = Question::parse("kitty leader map").unwrap();
        let idf = question
            .terms
            .iter()
            .map(|term| match term.as_str() {
                "map" => 2.0,
                "leader" => 1.5,
                _ => 1.8,
            })
            .collect::<Vec<_>>();
        let candidates = [
            (1, "# change the leader from ctrl-b"),
            (6, "map h focus left"),
            (12, "include kitty-themes/dark.conf"),
        ];
        let numbers = |header| {
            lines(&candidates, &question, &idf, header)
                .into_iter()
                .map(|(number, _)| number)
                .collect::<Vec<_>>()
        };
        assert_eq!(numbers(""), [6, 12]);
        assert_eq!(numbers("kitty/kitty.conf text kitty.conf"), [1, 6]);
    }

    #[test]
    fn long_lines_are_clipped_around_words_not_yet_shown() {
        let question = Question::parse("switch the alarm from quiet mode to loud").unwrap();
        let idf = vec![1.0; question.terms.len()];
        let filler = "and some other words here ".repeat(5);
        let long = format!(
            "  The alarm has two settings. {filler}It starts in quiet mode, and loud mode pages the whole team."
        );
        let candidates = [(3, "## Alarm"), (5, long.as_str())];
        let shown = lines(&candidates, &question, &idf, "");
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert_eq!(shown[0], (3, "## Alarm".to_owned()));
        let (number, text) = &shown[1];
        assert_eq!(*number, 5);
        assert!(text.len() <= MAX_LINE, "{}", text.len());
        assert!(text.starts_with(CUT), "{text}");
        assert!(
            text.contains("quiet mode") && text.contains("loud mode"),
            "{text}"
        );
    }

    #[test]
    fn plain_lines_are_trimmed_and_clipped_from_the_start() {
        assert_eq!(plain("  short  \r"), "short");
        let head = plain(&"é".repeat(100));
        assert!(head.len() <= MAX_LINE && head.ends_with(CUT));
        let line = format!("    {}needle{}", "a ".repeat(100), " b".repeat(100));
        let question = Question::parse("needle").unwrap();
        let shown = lines(&[(1, line.as_str())], &question, &[1.0], "");
        let text = &shown[0].1;
        assert!(text.len() <= MAX_LINE, "{}", text.len());
        assert!(text.starts_with(CUT) && text.ends_with(CUT), "{text}");
        assert!(text.contains("needle"));
    }
}
