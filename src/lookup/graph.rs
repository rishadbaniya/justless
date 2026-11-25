//! Bounded code-graph path traversal for `justless lookup --graph` and
//! natural-language relational queries ("how does X connect to Y?").

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use serde_json::{Value, json};

use crate::context::relationship;
use crate::index::{Definition, Edge, Index};
use crate::map;
use crate::rust_syntax::Kind;
use crate::workspace::Workspace;

use super::tokens::{self, Question};
use super::{Hit, Source};

/// Maximum graph traversal hops for connecting candidate symbols.
const MAX_HOPS: usize = 5;
/// Maximum candidate endpoints evaluated on each side of a relational query.
const MAX_SIDE_CANDIDATES: usize = 6;
/// Maximum distinct connecting paths returned in a single lookup response.
const MAX_PATHS: usize = 3;

static RELATIONAL_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)\bconnect(?:s|ed|ing)?\b.+\b(?:to|with|into|and)\b",
        r"(?i)\b(?:connection|path|flow|links?|relationship|chain|bridge)\s+between\b.+\band\b",
        r"(?i)\b(?:connection|path|flow)\s+between\b",
        r"(?i)\b(?:path|flow)\s+from\b.+\b(?:to|into)\b",
        r"(?i)\bhow\s+(?:does|do|did|can)\b.+\breach(?:es)?\b",
        r"(?i)\bhow\s+(?:does|do|did|can)\b.+\b(?:flow|flows|lead|leads|talk|talks|interact|interacts|get|gets)\b.+\b(?:to|with|into)\b",
        r"(?i)\btrace\b.+\b(?:to|into)\b",
        r"(?i)(?:\bwhat\s+links\b|\blinks\b.+\b(?:and|to|with)\b)",
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("relational pattern"))
    .collect()
});

static CLAUSE_SPLITTERS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)\bbetween\s+(.+?)\s+and\s+(.+)",
        r"(?i)\bfrom\s+(.+?)\s+(?:to|into)\s+(.+)",
        r"(?i)(.+?)\bconnect(?:s|ed|ing)?\s+(?:to|with|into)\s+(.+)",
        r"(?i)\bconnect(?:s|ed|ing)?\s+(.+?)\s+(?:and|with|to|into)\s+(.+)",
        r"(?i)(.+?)\breach(?:es)?\s+(.+)",
        r"(?i)\btrace\s+(.+?)\s+(?:to|into)\s+(.+)",
        r"(?i)(.+?)\blinks?\s+(?:to|with)\s+(.+)",
        r"(?i)\blinks?\s+(.+?)\s+(?:and|with|to)\s+(.+)",
        r"(?i)(.+?)\b(?:flow|flows|lead|leads|talk|talks|interact|interacts|get|gets)\s+(?:to|with|into)\s+(.+)",
        r"(?i)(.+?)\s+(?:to|with|and|->|→)\s+(.+)",
    ]
    .into_iter()
    .map(|pattern| Regex::new(pattern).expect("clause splitter"))
    .collect()
});

/// Words stripped from split clauses because they express the relational prompt
/// rather than a repository concept.
const RELATIONAL_NOISE: &[&str] = &[
    "connect",
    "connects",
    "connected",
    "connecting",
    "connection",
    "path",
    "flow",
    "flows",
    "trace",
    "reach",
    "reaches",
    "link",
    "links",
    "relationship",
    "chain",
    "bridge",
    "between",
    "show",
    "find",
    "explain",
];

#[derive(Clone, Debug)]
pub(crate) struct NodeInfo {
    pub name: String,
    pub qualified: String,
    pub kind: String,
    pub language: Option<&'static str>,
    pub file: String,
    pub start: usize,
    pub end: usize,
}

impl NodeInfo {
    fn from_def(index: &Index, id: usize) -> Self {
        let def = &index.definitions[id];
        let kind = if def.test {
            "test".to_owned()
        } else {
            def.kind.clone()
        };
        Self {
            name: map::name(def).to_owned(),
            qualified: def.qualified.clone(),
            kind,
            language: map::language(&def.location.file)
                .or_else(|| index.grammar(&def.location.file).map(|g| g.name())),
            file: def.location.file.clone(),
            start: def.location.start,
            end: def.location.end,
        }
    }

    fn location(&self) -> String {
        if self.start == self.end {
            format!("{}:{}", self.file, self.start)
        } else {
            format!("{}:{}-{}", self.file, self.start, self.end)
        }
    }

    fn label(&self, show_language: bool) -> String {
        if self.kind == "file" {
            return format!("{} [file]", self.location());
        }
        if show_language && let Some(lang) = self.language {
            return format!("{} [{} · {lang}] {}", self.name, self.kind, self.location());
        }
        format!("{} [{}] {}", self.name, self.kind, self.location())
    }

    fn to_json(&self) -> Value {
        let mut value = json!({
            "name": self.name,
            "qualified": self.qualified,
            "kind": self.kind,
            "file": self.file,
            "start": self.start,
            "end": self.end,
        });
        if let Some(lang) = self.language {
            value["language"] = json!(lang);
        }
        value
    }
}

#[derive(Clone, Debug)]
pub(crate) struct StepInfo {
    pub reverse: bool,
    pub kind: &'static str,
    pub possible: bool,
    pub at: String,
    pub from: NodeInfo,
    pub to: NodeInfo,
}

#[derive(Clone, Debug)]
pub(crate) struct GraphPath {
    pub start: NodeInfo,
    pub end: NodeInfo,
    pub hops: usize,
    pub directed: bool,
    pub possible: bool,
    pub cross_language: bool,
    pub steps: Vec<StepInfo>,
    nodes: Vec<usize>,
    pair_score: f64,
}

/// Whether a natural-language question asks how two concepts connect in the
/// code graph. Carefully scoped so ordinary factual questions like "where are
/// retries configured?" or "connection reset" do not trigger graph loading.
pub(crate) fn is_relational_question(question: &str) -> bool {
    let trimmed = question.trim();
    if trimmed.is_empty() {
        return false;
    }
    RELATIONAL_PATTERNS
        .iter()
        .any(|pattern| pattern.is_match(trimmed))
}

fn clean_clause(raw: &str) -> String {
    let mut kept = Vec::new();
    for piece in raw.split_whitespace() {
        let trimmed = piece.trim_matches(|ch: char| {
            matches!(
                ch,
                '?' | '!' | '.' | ',' | ';' | ':' | '"' | '\'' | '`' | '(' | ')' | '[' | ']'
            )
        });
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if tokens::STOPWORDS.contains(&lower.as_str()) || RELATIONAL_NOISE.contains(&lower.as_str())
        {
            continue;
        }
        kept.push(trimmed);
    }
    kept.join(" ")
}

fn split_relational_clauses(question: &str) -> Option<(String, String)> {
    let trimmed = question.trim().trim_end_matches(['?', '!', '.', ',']);
    for splitter in CLAUSE_SPLITTERS.iter() {
        let Some(captures) = splitter.captures(trimmed) else {
            continue;
        };
        let left = clean_clause(captures.get(1)?.as_str());
        let right = clean_clause(captures.get(2)?.as_str());
        if !left.is_empty()
            && !right.is_empty()
            && Question::parse(&left).is_some()
            && Question::parse(&right).is_some()
        {
            return Some((left, right));
        }
    }
    None
}

/// Opens the workspace index and traces shortest connecting paths for ranked
/// code symbols. Callers must invoke this only when `--graph` is set or
/// [`is_relational_question`] returns true.
pub(crate) fn trace(workspace: &Workspace, question: &str, hits: &[Hit]) -> Result<Vec<GraphPath>> {
    let index = Index::open(workspace)?;
    Ok(trace_in(&index, question, hits))
}

fn kind_rank(kind: Kind) -> u8 {
    match kind {
        Kind::Route => 0,
        Kind::Data => 1,
        Kind::Call | Kind::Method => 2,
        Kind::Import => 3,
        Kind::Reference => 4,
    }
}

/// Resolves a ranked code `Hit` to the most specific definition in `index`.
fn hit_definition(index: &Index, hit: &Hit) -> Option<usize> {
    if hit.source != Source::Code {
        return None;
    }
    let candidates = index.at(&hit.location);
    if candidates.is_empty() {
        return None;
    }
    let hit_compact = tokens::compact_ident(&hit.name);
    candidates.into_iter().min_by_key(|&id| {
        let def = &index.definitions[id];
        let exact_span =
            def.location.start == hit.location.start && def.location.end == hit.location.end;
        let short_name = map::name(def);
        let tail = short_name.rsplit("::").next().unwrap_or(short_name);
        let name_match = short_name == hit.name
            || tail == hit.name
            || (!hit_compact.is_empty() && tokens::compact_ident(tail) == hit_compact);
        let is_container = matches!(def.kind.as_str(), "file" | "module");
        let span_len = def.location.end.saturating_sub(def.location.start);
        (!exact_span, !name_match, is_container, span_len, id)
    })
}

fn text_terms(text: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    let mut tokenizer = tokens::Tokenizer::default();
    tokens::words(text, |_, word| {
        tokenizer.word(word, |token| {
            set.insert(token.to_owned());
        });
    });
    tokens::qualified(text, |_, _, token| {
        set.insert(token.to_owned());
    });
    set
}

fn clause_words(clause: &str) -> Vec<String> {
    let mut out = Vec::new();
    tokens::words(clause, |_, word| {
        let compact = tokens::compact_ident(word);
        if !compact.is_empty() && !out.contains(&compact) {
            out.push(compact);
        }
    });
    out
}

fn direct_symbol_matches(index: &Index, clause: &str) -> Vec<usize> {
    let mut found = Vec::new();
    for piece in clause.split_whitespace() {
        let clean = piece.trim_matches(|ch: char| {
            matches!(
                ch,
                '?' | '!' | '.' | ',' | ';' | '"' | '\'' | '`' | '(' | ')' | '[' | ']'
            )
        });
        if clean.len() < 2 {
            continue;
        }
        if let Ok(id) = index.select(clean) {
            if !found.contains(&id) {
                found.push(id);
            }
            continue;
        }
        let target = tokens::compact_ident(clean);
        if target.len() < 3 {
            continue;
        }
        let mut exact = Vec::new();
        for (id, def) in index.definitions.iter().enumerate() {
            if def.kind == "file" || def.test {
                continue;
            }
            let short = map::name(def);
            let tail = short.rsplit("::").next().unwrap_or(short);
            if tokens::compact_ident(tail) == target || tokens::compact_ident(short) == target {
                exact.push(id);
            }
        }
        if (1..=4).contains(&exact.len()) {
            for id in exact {
                if !found.contains(&id) {
                    found.push(id);
                }
            }
        }
    }
    found
}

fn clause_score(
    def: &Definition,
    clause_terms: &BTreeSet<String>,
    exclusive_terms: &BTreeSet<String>,
    raw_words: &[String],
    hit_rank: Option<usize>,
) -> f64 {
    let short_name = map::name(def);
    let tail = short_name.rsplit("::").next().unwrap_or(short_name);
    let tail_compact = tokens::compact_ident(tail);
    let short_compact = tokens::compact_ident(short_name);
    let file_stem = std::path::Path::new(&def.location.file)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(tokens::compact_ident)
        .unwrap_or_default();

    let name_terms = text_terms(short_name);
    let file_terms = text_terms(&def.location.file);

    let mut score = 0.0;
    for word in raw_words {
        if *word == tail_compact || *word == short_compact {
            score += 12.0;
        } else if !file_stem.is_empty() && *word == file_stem {
            score += 6.0;
        }
    }
    for term in clause_terms {
        let exclusive = exclusive_terms.contains(term);
        if name_terms.contains(term) {
            score += if exclusive { 5.0 } else { 1.5 };
        }
        if file_terms.contains(term) {
            score += if exclusive { 3.0 } else { 1.0 };
        }
    }
    if score > 0.0 {
        if def.test {
            score *= 0.6;
        }
        if def.kind == "file" {
            score *= 0.5;
        }
        if let Some(rank) = hit_rank {
            score += 0.5 / (1.0 + rank as f64);
        }
    }
    score
}

/// Picks up to `limit` top-scoring candidate definitions, preferring distinct
/// files before taking second symbols from an already-represented file.
fn pick_diverse(index: &Index, mut scored: Vec<(usize, f64)>, limit: usize) -> Vec<(usize, f64)> {
    scored.sort_by(|&(a_id, a_score), &(b_id, b_score)| {
        b_score
            .total_cmp(&a_score)
            .then_with(|| {
                index.definitions[a_id]
                    .test
                    .cmp(&index.definitions[b_id].test)
            })
            .then_with(|| {
                index.definitions[a_id]
                    .location
                    .cmp(&index.definitions[b_id].location)
            })
            .then(a_id.cmp(&b_id))
    });
    let mut picked = Vec::new();
    let mut seen_files = BTreeSet::new();
    let mut overflow = Vec::new();
    for item in scored {
        let file = index.definitions[item.0].location.file.as_str();
        if seen_files.insert(file) {
            picked.push(item);
            if picked.len() == limit {
                return picked;
            }
        } else {
            overflow.push(item);
        }
    }
    for item in overflow {
        picked.push(item);
        if picked.len() == limit {
            break;
        }
    }
    picked
}

/// Bounded directed shortest path from `start` to `end` (up to `MAX_HOPS`),
/// minimizing `(inferred_count, file_hops, hops)` so exact symbol-level calls
/// beat guessed links and generic file-import detours.
fn directed_path(index: &Index, start: usize, end: usize) -> Option<Vec<(&Edge, bool)>> {
    if start == end {
        return None;
    }
    let mut best = BTreeMap::from([(start, (0usize, 0usize, 0usize))]);
    let mut previous = BTreeMap::<usize, Option<(usize, &Edge)>>::from([(start, None)]);
    let mut heap = BinaryHeap::from([Reverse((0usize, 0usize, 0usize, start))]);

    while let Some(Reverse((inferred, file_hops, hops, node))) = heap.pop() {
        if best
            .get(&node)
            .is_some_and(|&cost| cost < (inferred, file_hops, hops))
        {
            continue;
        }
        if node == end {
            let mut steps = Vec::new();
            let mut curr = node;
            while let Some((parent, edge)) = previous[&curr] {
                steps.push((edge, false));
                curr = parent;
            }
            steps.reverse();
            return Some(steps);
        }
        if hops >= MAX_HOPS {
            continue;
        }
        let mut edges: Vec<&Edge> = index.outgoing(node).filter(|e| e.to != node).collect();
        edges.sort_by_key(|e| {
            (
                e.inferred,
                index.definitions[e.to].kind == "file",
                kind_rank(e.kind),
                &e.location,
                e.to,
            )
        });
        for edge in edges {
            let is_file_hop = edge.to != end && index.definitions[edge.to].kind == "file";
            let next_cost = (
                inferred + usize::from(edge.inferred),
                file_hops + usize::from(is_file_hop),
                hops + 1,
            );
            if best.get(&edge.to).is_none_or(|&cost| next_cost < cost) {
                best.insert(edge.to, next_cost);
                previous.insert(edge.to, Some((node, edge)));
                heap.push(Reverse((next_cost.0, next_cost.1, next_cost.2, edge.to)));
            }
        }
    }
    None
}

/// Bounded undirected fallback shortest path (up to `MAX_HOPS`), minimizing
/// `(inferred_count, file_hops, reverse_count, hops)` and labeling reversed
/// edges.
fn undirected_path(index: &Index, start: usize, end: usize) -> Option<Vec<(&Edge, bool)>> {
    if start == end {
        return None;
    }
    let mut best = BTreeMap::from([(start, (0usize, 0usize, 0usize, 0usize))]);
    let mut previous = BTreeMap::<usize, Option<(usize, &Edge, bool)>>::from([(start, None)]);
    let mut heap = BinaryHeap::from([Reverse((0usize, 0usize, 0usize, 0usize, start))]);

    while let Some(Reverse((inferred, file_hops, reverses, hops, node))) = heap.pop() {
        if best
            .get(&node)
            .is_some_and(|&cost| cost < (inferred, file_hops, reverses, hops))
        {
            continue;
        }
        if node == end {
            let mut steps = Vec::new();
            let mut curr = node;
            while let Some((parent, edge, reverse)) = previous[&curr] {
                steps.push((edge, reverse));
                curr = parent;
            }
            steps.reverse();
            return Some(steps);
        }
        if hops >= MAX_HOPS {
            continue;
        }
        let mut neighbors: Vec<(usize, &Edge, bool)> = index
            .outgoing(node)
            .filter(|e| e.to != node)
            .map(|e| (e.to, e, false))
            .chain(index.incoming(node).filter_map(|e| {
                let from = e.from?;
                (from != node).then_some((from, e, true))
            }))
            .collect();
        neighbors.sort_by_key(|&(target, e, rev)| {
            (
                e.inferred,
                index.definitions[target].kind == "file",
                rev,
                kind_rank(e.kind),
                &e.location,
                target,
            )
        });
        for (target, edge, reverse) in neighbors {
            let is_file_hop = target != end && index.definitions[target].kind == "file";
            let next_cost = (
                inferred + usize::from(edge.inferred),
                file_hops + usize::from(is_file_hop),
                reverses + usize::from(reverse),
                hops + 1,
            );
            if best.get(&target).is_none_or(|&cost| next_cost < cost) {
                best.insert(target, next_cost);
                previous.insert(target, Some((node, edge, reverse)));
                heap.push(Reverse((
                    next_cost.0,
                    next_cost.1,
                    next_cost.2,
                    next_cost.3,
                    target,
                )));
            }
        }
    }
    None
}

fn build_graph_path(
    index: &Index,
    start_id: usize,
    raw_steps: Vec<(&Edge, bool)>,
    pair_score: f64,
) -> GraphPath {
    let mut nodes = vec![start_id];
    let mut steps = Vec::with_capacity(raw_steps.len());
    let mut curr = start_id;
    let mut directed = true;
    let mut possible = false;
    let mut languages = BTreeSet::new();

    let start = NodeInfo::from_def(index, start_id);
    if let Some(lang) = start.language {
        languages.insert(lang);
    }

    for (edge, reverse) in raw_steps {
        let next_id = if reverse {
            directed = false;
            edge.from.expect("reverse edge has source definition")
        } else {
            edge.to
        };
        possible |= edge.inferred;
        let from_node = NodeInfo::from_def(index, curr);
        let to_node = NodeInfo::from_def(index, next_id);
        if let Some(lang) = to_node.language {
            languages.insert(lang);
        }
        steps.push(StepInfo {
            reverse,
            kind: relationship(edge.kind),
            possible: edge.inferred,
            at: edge.location.to_string(),
            from: from_node,
            to: to_node,
        });
        nodes.push(next_id);
        curr = next_id;
    }

    let end = NodeInfo::from_def(index, curr);
    GraphPath {
        start,
        end,
        hops: steps.len(),
        directed,
        possible,
        cross_language: languages.len() > 1,
        steps,
        nodes,
        pair_score,
    }
}

fn is_contiguous_subsequence(needle: &[usize], haystack: &[usize]) -> bool {
    needle.len() < haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

pub(crate) fn trace_in(index: &Index, question: &str, hits: &[Hit]) -> Vec<GraphPath> {
    let mut hit_ranks = BTreeMap::<usize, (usize, f64)>::new();
    for (rank, hit) in hits.iter().enumerate() {
        if let Some(id) = hit_definition(index, hit) {
            hit_ranks.entry(id).or_insert((rank, hit.score));
        }
    }

    let pairs: Vec<(usize, usize, f64)> = if let Some((left_clause, right_clause)) =
        split_relational_clauses(question)
    {
        let left_terms = text_terms(&left_clause);
        let right_terms = text_terms(&right_clause);
        let left_only: BTreeSet<String> = left_terms.difference(&right_terms).cloned().collect();
        let right_only: BTreeSet<String> = right_terms.difference(&left_terms).cloned().collect();
        let left_words = clause_words(&left_clause);
        let right_words = clause_words(&right_clause);

        let mut pool: BTreeSet<usize> = hit_ranks.keys().copied().collect();
        pool.extend(direct_symbol_matches(index, &left_clause));
        pool.extend(direct_symbol_matches(index, &right_clause));

        let mut left_scored = Vec::new();
        let mut right_scored = Vec::new();
        for id in pool {
            let def = &index.definitions[id];
            let rank = hit_ranks.get(&id).map(|&(r, _)| r);
            let l_score = clause_score(def, &left_terms, &left_only, &left_words, rank);
            let r_score = clause_score(def, &right_terms, &right_only, &right_words, rank);
            if l_score > 0.0 && l_score >= r_score {
                left_scored.push((id, l_score));
            }
            if r_score > 0.0 && r_score >= l_score {
                right_scored.push((id, r_score));
            }
        }
        // If one side got no exclusive owner (e.g. shared terms), allow its
        // positive-scoring matches as secondary candidates.
        if left_scored.is_empty() {
            for &id in hit_ranks.keys() {
                let s = clause_score(
                    &index.definitions[id],
                    &left_terms,
                    &left_only,
                    &left_words,
                    hit_ranks.get(&id).map(|&(r, _)| r),
                );
                if s > 0.0 {
                    left_scored.push((id, s));
                }
            }
        }
        if right_scored.is_empty() {
            for &id in hit_ranks.keys() {
                let s = clause_score(
                    &index.definitions[id],
                    &right_terms,
                    &right_only,
                    &right_words,
                    hit_ranks.get(&id).map(|&(r, _)| r),
                );
                if s > 0.0 {
                    right_scored.push((id, s));
                }
            }
        }

        let left_top = pick_diverse(index, left_scored, MAX_SIDE_CANDIDATES);
        let right_top = pick_diverse(index, right_scored, MAX_SIDE_CANDIDATES);
        let mut pairs = Vec::new();
        for &(l_id, l_score) in &left_top {
            for &(r_id, r_score) in &right_top {
                if l_id != r_id {
                    pairs.push((l_id, r_id, l_score + r_score));
                }
            }
        }
        pairs
    } else {
        let mut pool: BTreeSet<usize> = hit_ranks.keys().copied().collect();
        pool.extend(direct_symbol_matches(index, question));
        let scored: Vec<(usize, f64)> = pool
            .into_iter()
            .map(|id| {
                let s = hit_ranks
                    .get(&id)
                    .map_or(1.0, |&(rank, score)| score + 1.0 / (1.0 + rank as f64));
                (id, s)
            })
            .collect();
        let top = pick_diverse(index, scored, MAX_SIDE_CANDIDATES + 2);
        let mut pairs = Vec::new();
        for i in 0..top.len() {
            for j in (i + 1)..top.len() {
                pairs.push((top[i].0, top[j].0, top[i].1 + top[j].1));
            }
        }
        pairs
    };

    // Prefer cross-file pairs first, falling back to same-file pairs if none connect.
    let mut found = Vec::<GraphPath>::new();
    let mut seen_routes = BTreeSet::<Vec<usize>>::new();

    for allow_same_file in [false, true] {
        if allow_same_file && !found.is_empty() {
            break;
        }
        for &(src, dst, pair_score) in &pairs {
            let same_file =
                index.definitions[src].location.file == index.definitions[dst].location.file;
            if same_file != allow_same_file {
                continue;
            }
            let path = if let Some(steps) = directed_path(index, src, dst) {
                Some(build_graph_path(index, src, steps, pair_score))
            } else if let Some(steps) = directed_path(index, dst, src) {
                Some(build_graph_path(index, dst, steps, pair_score))
            } else {
                undirected_path(index, src, dst)
                    .map(|steps| build_graph_path(index, src, steps, pair_score))
            };
            if let Some(graph_path) = path
                && seen_routes.insert(graph_path.nodes.clone())
            {
                found.push(graph_path);
            }
        }
    }

    // Drop a path whose entire node chain is already a contiguous subpath of a
    // directed end-to-end chain (e.g. `list_orders -> purchase_orders` inside
    // `fetchOrders -> list_orders -> purchase_orders`).
    let directed_chains: Vec<Vec<usize>> = found
        .iter()
        .filter(|p| p.directed)
        .map(|p| p.nodes.clone())
        .collect();
    found.retain(|candidate| {
        !directed_chains
            .iter()
            .any(|chain| is_contiguous_subsequence(&candidate.nodes, chain))
    });

    // Rank: directed paths first, cross-file / cross-language connections ahead
    // of same-file hops, stronger endpoint match, exact edges, then fewer hops.
    found.sort_by(|a, b| {
        let a_same_file = a.start.file == a.end.file;
        let b_same_file = b.start.file == b.end.file;
        (!a.directed)
            .cmp(&(!b.directed))
            .then(a_same_file.cmp(&b_same_file))
            .then((!a.cross_language).cmp(&(!b.cross_language)))
            .then_with(|| b.pair_score.total_cmp(&a.pair_score))
            .then(a.hops.cmp(&b.hops))
            .then(a.possible.cmp(&b.possible))
            .then_with(|| a.start.file.cmp(&b.start.file))
            .then(a.start.start.cmp(&b.start.start))
            .then_with(|| a.end.file.cmp(&b.end.file))
            .then(a.end.start.cmp(&b.end.start))
    });
    found.truncate(MAX_PATHS);
    found
}

pub(crate) fn render_path(path: &GraphPath) -> String {
    let mut out = String::new();
    let show_lang = path.cross_language;
    let mode = if path.directed { "" } else { " · via reverse" };
    out.push_str(&format!(
        "  {} ({}{mode})\n",
        path.start.label(show_lang),
        map::plural(path.hops, "hop"),
    ));
    for step in &path.steps {
        let arrow = if step.reverse { "←" } else { "→" };
        let conf = if step.possible { "possible " } else { "" };
        out.push_str(&format!(
            "    {arrow} {conf}{} at {} → {}\n",
            step.kind,
            step.at,
            step.to.label(show_lang),
        ));
    }
    out
}

/// Minimum byte size needed to render the `Graph paths` header and its first
/// path (plus any `+N more` note when additional paths exist).
pub(crate) fn first_path_bytes(paths: &[GraphPath], explicit: bool) -> usize {
    if paths.is_empty() {
        return if explicit {
            "Graph paths · none within 5 hops\n".len()
        } else {
            0
        };
    }
    let header = format!("Graph paths · {}\n", paths.len());
    let first = render_path(&paths[0]);
    let more = if paths.len() > 1 {
        format!("  … +{} more\n", paths.len() - 1).len()
    } else {
        0
    };
    header.len() + first.len() + more
}

/// Renders the `Graph paths` section fitting whole paths within `max_bytes`.
pub(crate) fn render_section(paths: &[GraphPath], explicit: bool, max_bytes: usize) -> String {
    if paths.is_empty() {
        if !explicit {
            return String::new();
        }
        let empty = "Graph paths · none within 5 hops\n";
        return if empty.len() <= max_bytes {
            empty.to_owned()
        } else {
            String::new()
        };
    }
    let header = format!("Graph paths · {}\n", paths.len());
    if header.len() > max_bytes {
        return String::new();
    }
    let rendered: Vec<String> = paths.iter().map(render_path).collect();
    let full_len = header.len() + rendered.iter().map(String::len).sum::<usize>();
    if full_len <= max_bytes {
        return header + &rendered.concat();
    }
    let mut out = header;
    let mut shown = 0usize;
    for (idx, block) in rendered.iter().enumerate() {
        let remaining = rendered.len() - idx - 1;
        let more_len = if remaining > 0 {
            format!("  … +{remaining} more\n").len()
        } else {
            0
        };
        if out.len() + block.len() + more_len > max_bytes {
            break;
        }
        out.push_str(block);
        shown += 1;
    }
    if shown == 0 {
        return String::new();
    }
    if shown < rendered.len() {
        out.push_str(&format!("  … +{} more\n", rendered.len() - shown));
    }
    out
}

pub(crate) fn path_json(path: &GraphPath) -> Value {
    let steps: Vec<Value> = path
        .steps
        .iter()
        .map(|step| {
            json!({
                "direction": if step.reverse { "reverse" } else { "forward" },
                "kind": step.kind,
                "possible": step.possible,
                "at": step.at,
                "from": step.from.to_json(),
                "to": step.to.to_json(),
            })
        })
        .collect();
    json!({
        "from": path.start.to_json(),
        "to": path.end.to_json(),
        "hops": path.hops,
        "directed": path.directed,
        "possible": path.possible,
        "steps": steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relational_intent_detection_is_precise() {
        for relational in [
            "how does orders connect to purchase_orders?",
            "how do frontend orders connect to purchase_orders?",
            "connection between fetchOrders and purchase_orders",
            "path between frontend and sql",
            "path from checkout_total to apply_fee",
            "flow from api to database",
            "how does fetchOrders reach purchase_orders?",
            "trace orders to purchase_orders",
            "what links orders and purchase_orders?",
            "how does handler talk to storage?",
        ] {
            assert!(
                is_relational_question(relational),
                "expected relational intent for: {relational}"
            );
        }

        for ordinary in [
            "where are retries configured?",
            "how are retries configured?",
            "how does retry backoff work?",
            "connection reset",
            "upload retrying",
            "broken links in markdown",
            "RetryPolicy",
            "quorum notes",
        ] {
            assert!(
                !is_relational_question(ordinary),
                "unexpected relational trigger for: {ordinary}"
            );
        }
    }

    #[test]
    fn clause_splitting_extracts_both_concepts() {
        assert_eq!(
            split_relational_clauses("how do frontend orders connect to purchase_orders?"),
            Some(("frontend orders".into(), "purchase_orders".into()))
        );
        assert_eq!(
            split_relational_clauses("connection between fetchOrders and purchase_orders"),
            Some(("fetchOrders".into(), "purchase_orders".into()))
        );
        assert_eq!(
            split_relational_clauses("path from checkout_total to apply_fee"),
            Some(("checkout_total".into(), "apply_fee".into()))
        );
        assert_eq!(
            split_relational_clauses("how does fetchOrders reach purchase_orders?"),
            Some(("fetchOrders".into(), "purchase_orders".into()))
        );
    }
}
