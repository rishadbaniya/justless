//! The lines shown under a result: up to two lines that between them show
//! the most of the question's words, each clipped to the part that shows them.

use super::tokens::{Question, Tokenizer, words};

/// The longest line shown, in bytes.
const MAX_LINE: usize = 160;
/// How far a clipped line starts before the first word it is clipped around.
const LEAD: usize = 60;
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

/// A line trimmed and, when longer than [`MAX_LINE`], clipped to the window
/// that shows the rarest terms not in `shown`, then the rarest overall. Also
/// returns the terms the shown text contains.
fn clip(line: &str, mentions: &[Mention], idf: &[f64], shown: u64) -> (String, u64) {
    let trimmed = line.trim();
    if trimmed.len() <= MAX_LINE {
        return (trimmed.to_owned(), terms(mentions));
    }
    let indent = line.len() - line.trim_start().len();
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
fn best<'a, 'b>(
    lines: impl Iterator<Item = &'b Line<'a>>,
    score: impl Fn(&Line) -> (f64, f64),
) -> Option<&'b Line<'a>> {
    lines.fold(None, |best, line| match best {
        Some(best) if score(best) >= score(line) => Some(best),
        _ => Some(line),
    })
}

/// Up to two of `candidates`, in line order, as line numbers and shown text.
/// Each is the line with the rarest question terms that neither `header`
/// nor the line before it shows, then the rarest terms overall. The second
/// is never a copy of the first.
pub(crate) fn lines(
    candidates: &[(usize, &str)],
    question: &Question,
    idf: &[f64],
    header: &str,
) -> Vec<(usize, String)> {
    let mut tokenizer = Tokenizer::default();
    let seen = terms(&mentions(header, question, &mut tokenizer));
    let lines = candidates
        .iter()
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
    let Some(first) = best(lines.iter(), |line| {
        (weight(line.terms & !seen, idf), weight(line.terms, idf))
    }) else {
        return Vec::new();
    };
    let (text, visible) = clip(first.text, &first.mentions, idf, seen);
    let shown = seen | visible;
    let mut chosen = vec![(first.number, text)];
    let others = lines
        .iter()
        .filter(|line| line.number != first.number && line.text.trim() != first.text.trim());
    let second = best(others, |line| {
        (weight(line.terms & !shown, idf), weight(line.terms, idf))
    });
    if let Some(second) = second {
        let (text, _) = clip(second.text, &second.mentions, idf, shown);
        chosen.push((second.number, text));
    }
    chosen.sort_by_key(|(number, _)| *number);
    chosen
}

/// A line shown without a question term in it, clipped from its start.
pub(crate) fn plain(line: &str) -> String {
    clip(line, &[], &[], 0).0
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
        // A line that only repeats the first is never shown.
        let candidates = [
            (7, "    assert_ready(&map);"),
            (9, "assert_ready(&map);"),
            (12, "map.clear();"),
        ];
        assert_eq!(
            lines(&candidates, &question, &idf, ""),
            [
                (7, "assert_ready(&map);".to_owned()),
                (12, "map.clear();".to_owned())
            ]
        );
        assert_eq!(
            lines(&candidates[..2], &question, &idf, ""),
            [(7, "assert_ready(&map);".to_owned())]
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
