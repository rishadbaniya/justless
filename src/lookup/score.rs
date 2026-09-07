//! Field-weighted ranking of chunks for a question, result selection and
//! snippet line choice.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::Hash;

use anyhow::Result;

use super::chunks::{Class, FIELDS};
use super::segment::{Posting, Row, Segment};
use super::tokens::{self, Question};

/// Field weights: name, signature, doc comments or parent headings, body, path.
pub(crate) const WEIGHTS: [f64; FIELDS] = [4.0, 3.5, 3.0, 1.0, 0.5];
/// Length normalization per field.
pub(crate) const NORMALIZE: [f64; FIELDS] = [0.3, 0.5, 0.6, 0.75, 0.3];
const SATURATION: f64 = 1.2;
const TEST_PRIOR: f64 = 0.6;

/// A segment and which of its files are live.
pub(crate) struct Part<'a> {
    pub(crate) segment: &'a Segment,
    pub(crate) live: &'a [bool],
}

/// A scored chunk, by its part and chunk ID.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Scored {
    pub(crate) part: usize,
    pub(crate) chunk: u32,
    pub(crate) score: f64,
}

/// Collection statistics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Stats {
    pub(crate) chunks: u64,
    pub(crate) average: [f64; FIELDS],
}

/// One question scored against a collection of parts.
pub(crate) struct Scores {
    pub(crate) scored: Vec<Scored>,
    /// Live document frequency of each query term.
    pub(crate) df: Vec<u64>,
    /// Inverse document frequency of each query term (0 when absent).
    pub(crate) idf: Vec<f64>,
}

fn live(part: &Part, row: &Row) -> bool {
    part.live.get(row.file as usize).copied().unwrap_or(false)
}

/// Live chunk count and average field lengths across `parts`.
pub(crate) fn stats(parts: &[Part]) -> Stats {
    let mut chunks = 0u64;
    let mut sums = [0u64; FIELDS];
    for part in parts {
        let mut part_sums = part.segment.sums;
        for (file, range) in part.segment.ranges().iter().enumerate() {
            if part.live.get(file).copied().unwrap_or(false) {
                chunks += u64::from(range.end - range.start);
                continue;
            }
            for id in range.clone() {
                let row = part.segment.row(id);
                for (sum, &length) in part_sums.iter_mut().zip(&row.lengths) {
                    *sum = sum.saturating_sub(u64::from(length));
                }
            }
        }
        for (sum, part_sum) in sums.iter_mut().zip(part_sums) {
            *sum += part_sum;
        }
    }
    let mut average = [1.0; FIELDS];
    for (average, sum) in average.iter_mut().zip(sums) {
        if chunks > 0 && sum > 0 {
            *average = sum as f64 / chunks as f64;
        }
    }
    Stats { chunks, average }
}

pub(crate) fn idf(chunks: u64, df: u64) -> f64 {
    let (chunks, df) = (chunks as f64, df as f64);
    (1.0 + (chunks - df + 0.5) / (df + 0.5)).ln()
}

/// The field-weighted, length-normalized term frequency of one posting.
pub(crate) fn weighted_frequency(posting: &Posting, row: &Row, stats: &Stats) -> f64 {
    (0..FIELDS)
        .filter(|&field| posting.frequency[field] > 0)
        .map(|field| {
            let length = f64::from(row.lengths[field]) / stats.average[field];
            let norm = 1.0 - NORMALIZE[field] + NORMALIZE[field] * length;
            WEIGHTS[field] * f64::from(posting.frequency[field]) / norm
        })
        .sum()
}

/// Rewards chunks that match more of the question's terms.
pub(crate) fn coordination(matched: usize, terms: usize) -> f64 {
    (1.0 + matched as f64) / (1.0 + terms as f64)
}

/// The prior of a chunk: data files and logs rank low, tests lower unless asked for.
pub(crate) fn prior(class: Class, test: bool, tests_wanted: bool) -> f64 {
    let prior = class.prior();
    if test && !tests_wanted {
        prior * TEST_PRIOR
    } else {
        prior
    }
}

/// Scores every live chunk that matches at least one query term.
pub(crate) fn score(parts: &[Part], question: &Question) -> Result<Scores> {
    let stats = stats(parts);
    let terms = question.terms.len();
    let tests_wanted = question.mentions_tests();
    let mut sums = parts.iter().map(|_| Vec::new()).collect::<Vec<Vec<f64>>>();
    let mut matched = parts.iter().map(|_| Vec::new()).collect::<Vec<Vec<u8>>>();
    let mut df = vec![0u64; terms];
    let mut idf = vec![0f64; terms];
    let mut postings = Vec::new();
    let mut hits = Vec::<(usize, Posting)>::new();
    for (term, text) in question.terms.iter().enumerate() {
        hits.clear();
        for (index, part) in parts.iter().enumerate() {
            let Some(entry) = part.segment.find(text) else {
                continue;
            };
            part.segment.postings(&entry, &mut postings)?;
            hits.extend(
                postings
                    .iter()
                    .filter(|posting| live(part, &part.segment.row(posting.chunk)))
                    .map(|posting| (index, *posting)),
            );
        }
        if hits.is_empty() {
            continue;
        }
        df[term] = hits.len() as u64;
        idf[term] = self::idf(stats.chunks, df[term]);
        for &(index, posting) in &hits {
            let part = &parts[index];
            if sums[index].is_empty() {
                let count = part.segment.chunk_count() as usize;
                sums[index] = vec![0.0; count];
                matched[index] = vec![0; count];
            }
            let row = part.segment.row(posting.chunk);
            let frequency = weighted_frequency(&posting, &row, &stats);
            sums[index][posting.chunk as usize] += idf[term] * frequency / (SATURATION + frequency);
            matched[index][posting.chunk as usize] += 1;
        }
    }
    let mut scored = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        for (chunk, &count) in matched[index].iter().enumerate() {
            if count == 0 {
                continue;
            }
            let row = part.segment.row(chunk as u32);
            let score = sums[index][chunk]
                * coordination(usize::from(count), terms)
                * prior(row.class(), row.test(), tests_wanted);
            if score > 0.0 {
                scored.push(Scored {
                    part: index,
                    chunk: chunk as u32,
                    score,
                });
            }
        }
    }
    Ok(Scores { scored, df, idf })
}

/// The best `limit` items in `compare` order, at most two from one file.
pub(crate) fn select<T: Copy, K: Eq + Hash>(
    mut items: Vec<T>,
    limit: usize,
    compare: impl Fn(&T, &T) -> Ordering,
    file: impl Fn(&T) -> K,
) -> Vec<T> {
    let mut window = limit.saturating_mul(4).max(32);
    loop {
        let take = window.min(items.len());
        if take == 0 {
            return Vec::new();
        }
        if take < items.len() {
            items.select_nth_unstable_by(take - 1, &compare);
        }
        items[..take].sort_by(&compare);
        let mut per_file = HashMap::new();
        let mut chosen = Vec::new();
        for item in &items[..take] {
            let count = per_file.entry(file(item)).or_insert(0);
            if *count < 2 {
                *count += 1;
                chosen.push(*item);
                if chosen.len() == limit {
                    return chosen;
                }
            }
        }
        if take == items.len() {
            return chosen;
        }
        window = window.saturating_mul(4);
    }
}

/// A line's score for a question and the offset of its first matching word.
pub(crate) fn line_score(line: &str, question: &Question, idf: &[f64]) -> (f64, Option<usize>) {
    let mut seen = vec![false; question.terms.len()];
    let mut first = None;
    tokens::tokens(line, |token, offset| {
        if let Ok(term) = question
            .terms
            .binary_search_by(|term| term.as_str().cmp(token))
        {
            seen[term] = true;
            if first.is_none_or(|first| offset < first) {
                first = Some(offset);
            }
        }
    });
    let score = seen
        .iter()
        .zip(idf)
        .filter(|(seen, _)| **seen)
        .map(|(_, idf)| idf)
        .sum();
    (score, first)
}

/// The (up to) two best lines among `candidates`, in line order. Each is a
/// line number and the byte offset of its first matching word.
pub(crate) fn best_lines(
    candidates: &[(usize, &str)],
    question: &Question,
    idf: &[f64],
) -> Vec<(usize, Option<usize>)> {
    let mut scored = candidates
        .iter()
        .map(|&(number, text)| {
            let (score, first) = line_score(text, question, idf);
            (score, number, first)
        })
        .filter(|(score, _, _)| *score > 0.0)
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| right.0.total_cmp(&left.0).then(left.1.cmp(&right.1)));
    scored.truncate(2);
    let mut lines = scored
        .into_iter()
        .map(|(_, number, first)| (number, first))
        .collect::<Vec<_>>();
    lines.sort_unstable();
    lines
}

#[cfg(test)]
mod tests {
    use super::super::chunks::Chunk;
    use super::super::segment::Builder;
    use super::*;
    use std::borrow::Cow;

    fn chunk<'a>(start: usize, class: Class, test: bool, fields: [&'a str; 4]) -> Chunk<'a> {
        Chunk {
            start,
            end: start,
            declaration: start,
            name: format!("chunk{start}"),
            kind: "function".into(),
            test,
            class,
            fields: fields.map(|text| {
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![Cow::Borrowed(text)]
                }
            }),
        }
    }

    fn collection(files: &[(&str, Vec<Chunk>)]) -> Segment {
        let mut builder = Builder::default();
        for (file, chunks) in files {
            builder.add(file, chunks, false);
        }
        builder.finish().unwrap()
    }

    fn ranked(segment: &Segment, question: &str) -> Vec<(String, f64)> {
        let live = vec![true; segment.files().len()];
        let parts = [Part {
            segment,
            live: &live,
        }];
        let question = Question::parse(question).unwrap();
        let mut scored = score(&parts, &question).unwrap().scored;
        scored.sort_by(|left, right| right.score.total_cmp(&left.score));
        scored
            .iter()
            .map(|scored| {
                let row = segment.row(scored.chunk);
                (segment.name(&row).to_owned(), scored.score)
            })
            .collect()
    }

    #[test]
    fn field_weighted_arithmetic() {
        let segment = collection(&[(
            "a.rs",
            vec![
                chunk(1, Class::Code, false, ["retry", "", "", "loop loop"]),
                chunk(2, Class::Code, false, ["other", "", "retry policy", ""]),
                chunk(3, Class::Code, false, ["", "", "", "unrelated words here"]),
            ],
        )]);
        let scores = ranked(&segment, "retry");
        // N = 3, df = 2. Field lengths: name 1, 1, 0; doc 0, 2, 0; body 2, 0, 3.
        let idf = (1.0f64 + (3.0 - 2.0 + 0.5) / (2.0 + 0.5)).ln();
        let name_average = 2.0 / 3.0;
        let doc_average = 2.0 / 3.0;
        let first = 4.0 / (1.0 - 0.3 + 0.3 * (1.0 / name_average));
        let second = 3.0 / (1.0 - 0.6 + 0.6 * (2.0 / doc_average));
        let expected = |frequency: f64| idf * frequency / (1.2 + frequency) * (2.0 / 2.0);
        assert_eq!(scores.len(), 2);
        assert_eq!(scores[0].0, "chunk1");
        assert!((scores[0].1 - expected(first)).abs() < 1e-9, "{scores:?}");
        assert!((scores[1].1 - expected(second)).abs() < 1e-9, "{scores:?}");
    }

    #[test]
    fn coordination_rewards_matching_more_words() {
        // Both chunks have the same raw score for "alpha"; only one also has "beta".
        let segment = collection(&[(
            "a.rs",
            vec![
                chunk(1, Class::Code, false, ["", "", "", "alpha gamma"]),
                chunk(2, Class::Code, false, ["", "", "", "alpha beta"]),
                chunk(3, Class::Code, false, ["", "", "", "delta gamma"]),
                chunk(4, Class::Code, false, ["", "", "", "beta delta"]),
            ],
        )]);
        let scores = ranked(&segment, "alpha beta");
        assert_eq!(scores[0].0, "chunk2");
        // At an equal raw score, matching both words beats matching one.
        assert!(coordination(2, 2) > coordination(1, 2));
        assert_eq!(coordination(1, 2), 2.0 / 3.0);
        assert_eq!(coordination(2, 2), 1.0);
    }

    #[test]
    fn priors_push_data_and_tests_down() {
        let segment = collection(&[(
            "a.rs",
            vec![
                chunk(1, Class::Data, false, ["retry", "", "", ""]),
                chunk(2, Class::Code, true, ["retry", "", "", ""]),
                chunk(3, Class::Code, false, ["retry", "", "", ""]),
                chunk(4, Class::Text, false, ["retry", "", "", ""]),
            ],
        )]);
        let order = ranked(&segment, "retry")
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        assert_eq!(order, ["chunk3", "chunk4", "chunk2", "chunk1"]);
        let order = ranked(&segment, "retry tests")
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        assert_eq!(order[..2], ["chunk2", "chunk3"]);
        assert!(prior(Class::Data, false, false) < prior(Class::Log, false, false));
    }

    #[test]
    fn dead_files_do_not_count() {
        let segment = collection(&[
            (
                "a.rs",
                vec![chunk(1, Class::Code, false, ["retry", "", "", ""])],
            ),
            (
                "b.rs",
                vec![chunk(1, Class::Code, false, ["retry", "", "", "x y z"])],
            ),
        ]);
        let live = [true, false];
        let parts = [Part {
            segment: &segment,
            live: &live,
        }];
        let stats = stats(&parts);
        assert_eq!(stats.chunks, 1);
        assert_eq!(stats.average[0], 1.0);
        let question = Question::parse("retry").unwrap();
        let scores = score(&parts, &question).unwrap();
        assert_eq!(scores.df, [1]);
        assert_eq!(scores.scored.len(), 1);
    }

    #[test]
    fn selection_orders_ties_and_caps_files() {
        let items = vec![
            (1.0, "b.rs", 3),
            (1.0, "a.rs", 9),
            (1.0, "a.rs", 2),
            (2.0, "c.rs", 1),
            (0.5, "a.rs", 1),
            (0.9, "a.rs", 5),
        ];
        let compare = |left: &(f64, &str, usize), right: &(f64, &str, usize)| {
            right
                .0
                .total_cmp(&left.0)
                .then(left.1.cmp(right.1))
                .then(left.2.cmp(&right.2))
        };
        let chosen = select(items.clone(), 10, compare, |item| item.1);
        assert_eq!(
            chosen,
            [
                (2.0, "c.rs", 1),
                (1.0, "a.rs", 2),
                (1.0, "a.rs", 9),
                (1.0, "b.rs", 3)
            ]
        );
        assert_eq!(select(items, 2, compare, |item| item.1).len(), 2);
        let many = (0..500).map(|n| (f64::from(n), "x", n as usize)).collect();
        assert_eq!(select(many, 3, compare, |item| item.2).len(), 3);
    }

    #[test]
    fn snippet_lines_prefer_rarer_words() {
        let question = Question::parse("retry policy").unwrap();
        let idf = [2.0, 0.5];
        let lines = [
            (3, "let policy = 1;"),
            (4, "fn retry() {}"),
            (5, "// no match"),
            (6, "retry with policy"),
        ];
        assert_eq!(question.terms, ["polici", "retri"]);
        assert_eq!(
            best_lines(&lines, &question, &idf),
            [(3, Some(4)), (6, Some(0))]
        );
        assert!(best_lines(&lines[2..3], &question, &idf).is_empty());
    }
}
