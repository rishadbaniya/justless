//! Field-weighted ranking of chunks for a question, result selection and
//! snippet line choice.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::Hash;

use anyhow::Result;

use super::chunks::{Class, FIELDS};
use super::segment::{Posting, Row, Segment};
use super::tokens::{
    Question, compact_ident, compact_ident_into, normalize_qualified, normalize_qualified_into,
};

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

fn live_file(part: &Part, file: u32) -> bool {
    part.live.get(file as usize).copied().unwrap_or(false)
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

/// One chosen result together with identical copies in other files and
/// overlapping or adjacent same-file chunks absorbed into it.
#[derive(Clone, Debug)]
pub(crate) struct Picked<T> {
    pub(crate) item: T,
    pub(crate) copies: Vec<T>,
    pub(crate) merged: Vec<T>,
}

fn strip_extension(path: &str) -> &str {
    let slash = path.rfind('/').map_or(0, |index| index + 1);
    match path[slash..].rfind('.') {
        Some(dot) if dot > 0 => &path[..slash + dot],
        _ => path,
    }
}

fn matches_path_suffix(path: &str, suffix: &str) -> bool {
    if path.eq_ignore_ascii_case(suffix) {
        return true;
    }
    path.len() > suffix.len()
        && path.as_bytes()[path.len() - suffix.len() - 1] == b'/'
        && path[path.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

/// Precomputed query forms for [`exact_boost`] so scoring thousands of chunks
/// allocates zero temporary strings.
struct BoostQuery<'a> {
    raw: &'a str,
    raw_single: bool,
    norm_raw: String,
    compact_raw: String,
    identifiers: Vec<IdentHint<'a>>,
    path_hints: &'a [String],
}

enum IdentHint<'a> {
    Qualified { raw: &'a str, normalized: String },
    Simple { raw: &'a str, compact: String },
}

#[derive(Default)]
struct BoostScratch {
    norm_name: String,
    compact_simple: String,
}

impl<'a> BoostQuery<'a> {
    fn new(question: &'a Question) -> Self {
        let raw = question.raw.as_str();
        let raw_single = !raw.is_empty() && !raw.contains(char::is_whitespace);
        let (norm_raw, compact_raw) = if raw_single {
            (normalize_qualified(raw), compact_ident(raw))
        } else {
            (String::new(), String::new())
        };
        let identifiers = question
            .identifiers
            .iter()
            .map(|ident| {
                if ident.contains("::") || ident.contains('.') {
                    IdentHint::Qualified {
                        raw: ident.as_str(),
                        normalized: normalize_qualified(ident),
                    }
                } else {
                    IdentHint::Simple {
                        raw: ident.as_str(),
                        compact: compact_ident(ident),
                    }
                }
            })
            .collect();
        Self {
            raw,
            raw_single,
            norm_raw,
            compact_raw,
            identifiers,
            path_hints: &question.path_hints,
        }
    }

    fn boost(&self, name: &str, kind: &str, file: &str, scratch: &mut BoostScratch) -> f64 {
        let symbol_or_heading = !matches!(kind, "file" | "data" | "log");
        let simple = name.rsplit("::").next().unwrap_or(name);
        let mut boost = 1.0f64;
        let mut norm_name_ready = false;
        let mut compact_simple_ready = false;

        if symbol_or_heading && !self.raw.is_empty() {
            if name == self.raw {
                boost = boost.max(1.50);
            } else if name.eq_ignore_ascii_case(self.raw) || (self.raw_single && simple == self.raw)
            {
                boost = boost.max(1.35);
            } else if self.raw_single && simple.eq_ignore_ascii_case(self.raw) {
                boost = boost.max(1.25);
            } else if self.raw_single {
                let matched_norm = if self.norm_raw.len() >= 2 {
                    normalize_qualified_into(name, &mut scratch.norm_name);
                    norm_name_ready = true;
                    scratch.norm_name == self.norm_raw
                } else {
                    false
                };
                if matched_norm {
                    boost = boost.max(1.20);
                } else if self.compact_raw.len() >= 4 {
                    compact_ident_into(simple, &mut scratch.compact_simple);
                    compact_simple_ready = true;
                    if scratch.compact_simple == self.compact_raw {
                        boost = boost.max(1.15);
                    }
                }
            }
        }

        if symbol_or_heading {
            for hint in &self.identifiers {
                match hint {
                    IdentHint::Qualified { raw, normalized } => {
                        if name == *raw {
                            boost = boost.max(1.45);
                        } else {
                            if !norm_name_ready {
                                normalize_qualified_into(name, &mut scratch.norm_name);
                                norm_name_ready = true;
                            }
                            if scratch.norm_name == *normalized {
                                boost = boost.max(1.35);
                            }
                        }
                    }
                    IdentHint::Simple { raw, compact } => {
                        if name == *raw {
                            boost = boost.max(1.45);
                        } else if simple == *raw {
                            boost = boost.max(1.35);
                        } else if compact.len() >= 4 {
                            if !compact_simple_ready {
                                compact_ident_into(simple, &mut scratch.compact_simple);
                                compact_simple_ready = true;
                            }
                            if scratch.compact_simple == *compact {
                                boost = boost.max(1.15);
                            }
                        }
                    }
                }
            }
        }

        for hint in self.path_hints {
            if matches_path_suffix(file, hint) {
                boost *= 1.30;
                break;
            }
            if hint.contains('/') && matches_path_suffix(strip_extension(file), hint) {
                boost *= 1.20;
                break;
            }
        }

        boost
    }
}

/// Scores every live chunk that matches at least one query term.
pub(crate) fn score(parts: &[Part], question: &Question) -> Result<Scores> {
    let stats = stats(parts);
    let terms = question.terms.len();
    let tests_wanted = question.mentions_tests();
    let all_live: Vec<bool> = parts
        .iter()
        .map(|part| {
            part.live.len() >= part.segment.files().len()
                && part
                    .live
                    .iter()
                    .take(part.segment.files().len())
                    .all(|&l| l)
        })
        .collect();
    let mut sums = parts.iter().map(|_| Vec::new()).collect::<Vec<Vec<f64>>>();
    let mut matched = parts.iter().map(|_| Vec::new()).collect::<Vec<Vec<u8>>>();
    let mut touched = parts.iter().map(|_| Vec::new()).collect::<Vec<Vec<u32>>>();
    let mut df = vec![0u64; terms];
    let mut idf = vec![0f64; terms];
    let mut raw = Vec::new();
    let mut postings = Vec::new();
    let mut hits = Vec::<(usize, Posting)>::new();
    for (term, text) in question.terms.iter().enumerate() {
        hits.clear();
        for (index, part) in parts.iter().enumerate() {
            let Some(entry) = part.segment.find(text) else {
                continue;
            };
            part.segment
                .postings_into(&entry, &mut raw, &mut postings)?;
            if all_live[index] {
                hits.extend(postings.iter().map(|posting| (index, *posting)));
            } else {
                hits.extend(
                    postings
                        .iter()
                        .filter(|posting| live_file(part, part.segment.row_file(posting.chunk)))
                        .map(|posting| (index, *posting)),
                );
            }
        }
        if hits.is_empty() {
            continue;
        }
        df[term] = hits.len() as u64;
        idf[term] = self::idf(stats.chunks, df[term]);
        let term_idf = idf[term];
        for &(index, posting) in &hits {
            let part = &parts[index];
            if sums[index].is_empty() {
                let count = part.segment.chunk_count() as usize;
                sums[index] = vec![0.0; count];
                matched[index] = vec![0; count];
            }
            let chunk_idx = posting.chunk as usize;
            if matched[index][chunk_idx] == 0 {
                touched[index].push(posting.chunk);
            }
            let row = part.segment.row(posting.chunk);
            let frequency = weighted_frequency(&posting, &row, &stats);
            sums[index][chunk_idx] += term_idf * frequency / (SATURATION + frequency);
            matched[index][chunk_idx] += 1;
        }
    }
    let total_touched: usize = touched.iter().map(Vec::len).sum();
    let mut scored = Vec::with_capacity(total_touched);
    let boost_query = BoostQuery::new(question);
    let mut boost_scratch = BoostScratch::default();
    for (index, part) in parts.iter().enumerate() {
        let touched_part = &mut touched[index];
        if touched_part.is_empty() {
            continue;
        }
        let score_chunk = |chunk_id: u32,
                           count: u8,
                           boost_scratch: &mut BoostScratch,
                           scored: &mut Vec<Scored>| {
            let row = part.segment.row(chunk_id);
            let class = row.class();
            let boost = if matches!(class, Class::Log | Class::Data) {
                1.0
            } else {
                let name = part.segment.name(&row);
                let kind = part.segment.kind(&row);
                let file = &part.segment.files()[row.file as usize];
                boost_query.boost(name, kind, file, boost_scratch)
            };
            let score = sums[index][chunk_id as usize]
                * coordination(usize::from(count), terms)
                * prior(class, row.test(), tests_wanted)
                * boost;
            if score > 0.0 {
                scored.push(Scored {
                    part: index,
                    chunk: chunk_id,
                    score,
                });
            }
        };
        if touched_part.len().saturating_mul(4) < matched[index].len() {
            touched_part.sort_unstable();
            for &chunk_id in touched_part.iter() {
                let count = matched[index][chunk_id as usize];
                score_chunk(chunk_id, count, &mut boost_scratch, &mut scored);
            }
        } else {
            for (chunk, &count) in matched[index].iter().enumerate() {
                if count != 0 {
                    score_chunk(chunk as u32, count, &mut boost_scratch, &mut scored);
                }
            }
        }
    }
    Ok(Scores { scored, df, idf })
}

/// Up to two chosen slot indices per file, stored inline without heap allocation.
#[derive(Clone, Copy, Default)]
struct Slots {
    items: [usize; 2],
    len: u8,
}

impl Slots {
    fn as_slice(&self) -> &[usize] {
        &self.items[..usize::from(self.len)]
    }

    fn len(&self) -> usize {
        usize::from(self.len)
    }

    fn push(&mut self, index: usize) {
        self.items[usize::from(self.len)] = index;
        self.len += 1;
    }
}

/// The best `limit` items in `compare` order, at most two non-overlapping
/// results from one file. An item with the same `copy` key as a chosen one,
/// such as the same lines of an identical file, is listed with it instead of
/// taking a place. A lower-ranked item from the same file that satisfies
/// `overlaps` with an already-chosen item is absorbed into `merged` instead
/// of taking another slot.
pub(crate) fn select<T: Copy, K: Eq + Hash, C: Eq + Hash>(
    mut items: Vec<T>,
    limit: usize,
    compare: impl Fn(&T, &T) -> Ordering,
    file: impl Fn(&T) -> K,
    copy: impl Fn(&T) -> Option<C>,
    overlaps: impl Fn(&T, &T) -> bool,
) -> Vec<Picked<T>> {
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
        let mut per_file = HashMap::<K, Slots>::with_capacity(limit);
        let mut copies = HashMap::<C, usize>::with_capacity(limit);
        let mut chosen = Vec::<Picked<T>>::with_capacity(limit);
        for item in &items[..take] {
            let key = copy(item);
            if let Some(&index) = key.as_ref().and_then(|key| copies.get(key)) {
                chosen[index].copies.push(*item);
                continue;
            }
            let file_key = file(item);
            if let Some(existing) = per_file.get(&file_key)
                && let Some(&index) = existing.as_slice().iter().find(|&&index| {
                    overlaps(&chosen[index].item, item)
                        || chosen[index]
                            .merged
                            .iter()
                            .any(|other| overlaps(other, item))
                })
            {
                chosen[index].merged.push(*item);
                continue;
            }
            // Once full, the rest of the window is read only for copies and overlaps.
            if chosen.len() == limit {
                continue;
            }
            let slots = per_file.entry(file_key).or_default();
            if slots.len() < 2 {
                let index = chosen.len();
                slots.push(index);
                if let Some(key) = key {
                    copies.insert(key, index);
                }
                chosen.push(Picked {
                    item: *item,
                    copies: Vec::new(),
                    merged: Vec::new(),
                });
            }
        }
        if chosen.len() == limit || take == items.len() {
            return chosen;
        }
        window = window.saturating_mul(4);
    }
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
        let none = |_: &(f64, &str, usize)| None::<usize>;
        let no_overlap = |_: &(f64, &str, usize), _: &(f64, &str, usize)| false;
        let chosen = select(items.clone(), 10, compare, |item| item.1, none, no_overlap);
        assert_eq!(
            chosen.iter().map(|picked| picked.item).collect::<Vec<_>>(),
            [
                (2.0, "c.rs", 1),
                (1.0, "a.rs", 2),
                (1.0, "a.rs", 9),
                (1.0, "b.rs", 3)
            ]
        );
        assert_eq!(
            select(items, 2, compare, |item| item.1, none, no_overlap).len(),
            2
        );
        let many = (0..500).map(|n| (f64::from(n), "x", n as usize)).collect();
        assert_eq!(
            select(many, 3, compare, |item| item.2, none, no_overlap).len(),
            3
        );
    }

    #[test]
    fn copies_join_the_result_they_repeat() {
        // The third field is the content key: d.rs repeats a.rs, and f.rs
        // repeats e.rs after the last place is taken.
        let items = vec![
            (5.0, "a.rs", 1),
            (4.0, "d.rs", 1),
            (3.0, "b.rs", 2),
            (2.0, "c.rs", 3),
            (1.5, "e.rs", 4),
            (1.0, "f.rs", 4),
        ];
        let compare = |left: &(f64, &str, usize), right: &(f64, &str, usize)| {
            right.0.total_cmp(&left.0).then(left.1.cmp(right.1))
        };
        let chosen = select(
            items,
            4,
            compare,
            |item| item.1,
            |item| Some(item.2),
            |_, _| false,
        );
        fn names(items: &[(f64, &'static str, usize)]) -> Vec<&'static str> {
            items.iter().map(|item| item.1).collect()
        }
        let shown = chosen
            .iter()
            .map(|picked| picked.item.1)
            .collect::<Vec<_>>();
        assert_eq!(shown, ["a.rs", "b.rs", "c.rs", "e.rs"]);
        assert_eq!(names(&chosen[0].copies), ["d.rs"]);
        assert!(chosen[1].copies.is_empty());
        assert_eq!(names(&chosen[3].copies), ["f.rs"]);
    }

    #[test]
    fn exact_match_ranks_target_symbol_and_path_first() {
        fn named<'a>(start: usize, name: &str, kind: &str, fields: [&'a str; 4]) -> Chunk<'a> {
            let mut c = chunk(start, Class::Code, false, fields);
            c.name = name.to_owned();
            c.kind = kind.to_owned();
            c
        }
        let segment = collection(&[
            (
                "src/a_parser.rs",
                vec![named(
                    1,
                    "Parser::order",
                    "method",
                    ["order Parser::order", "fn order(&self)", "", ""],
                )],
            ),
            (
                "src/b_order.rs",
                vec![named(
                    1,
                    "Order::parse",
                    "method",
                    ["parse Order::parse", "fn parse(&self)", "", ""],
                )],
            ),
            (
                "src/c_camel.ts",
                vec![named(
                    1,
                    "parseHttpHeader",
                    "function",
                    ["parseHttpHeader", "function parseHttpHeader()", "", ""],
                )],
            ),
            (
                "src/d_snake.rs",
                vec![named(
                    1,
                    "parse_http_header",
                    "function",
                    ["parse_http_header", "fn parse_http_header()", "", ""],
                )],
            ),
        ]);
        // Qualified symbol query ranks exact Order::parse above swapped Parser::order.
        let qualified_order = ranked(&segment, "Order::parse");
        assert_eq!(qualified_order[0].0, "Order::parse", "{qualified_order:?}");
        assert!(
            qualified_order[0].1 > qualified_order[1].1 * 1.4,
            "{qualified_order:?}"
        );

        // Exact spelling wins #1 across camelCase / snake_case twins.
        assert_eq!(ranked(&segment, "parseHttpHeader")[0].0, "parseHttpHeader");
        assert_eq!(
            ranked(&segment, "parse_http_header")[0].0,
            "parse_http_header"
        );

        // Explicit file path hint boosts chunks in that file.
        let path_ranked = ranked(&segment, "b_order.rs order");
        assert_eq!(path_ranked[0].0, "Order::parse", "{path_ranked:?}");
    }

    #[test]
    fn overlapping_same_file_items_are_absorbed_freeing_slots_for_other_files() {
        // (score, file, start, end)
        let items = vec![
            (10.0, "a.rs", 12usize, 14usize), // child method
            (9.0, "a.rs", 9, 20),             // overlapping enclosing class
            (8.0, "a.rs", 12, 14),            // exact duplicate span
            (7.0, "b.rs", 1, 10),
            (6.0, "c.rs", 1, 10),
            (5.0, "d.rs", 1, 10),
        ];
        let compare = |left: &(f64, &str, usize, usize), right: &(f64, &str, usize, usize)| {
            right.0.total_cmp(&left.0)
        };
        let overlaps = |left: &(f64, &str, usize, usize), right: &(f64, &str, usize, usize)| {
            left.2 <= right.3 && right.2 <= left.3
        };
        let chosen = select(
            items,
            3,
            compare,
            |item| item.1,
            |_| None::<usize>,
            overlaps,
        );
        let files = chosen
            .iter()
            .map(|picked| picked.item.1)
            .collect::<Vec<_>>();
        assert_eq!(files, ["a.rs", "b.rs", "c.rs"]);
        assert_eq!(chosen[0].merged.len(), 2);
    }
}
