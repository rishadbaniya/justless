//! A lossless table view of JSON output: arrays of objects and JSON lines name their
//! fields once instead of on every record. Other JSON, such as a pretty-printed
//! dependency tree, is shown compact: the same tokens without the whitespace
//! between them, after a legend line.
//!
//! Scalars, keys and escapes keep their exact source text, so `"1"` and `1`, `1.0`
//! and `1`, or `"é"` and `"\u00e9"` stay apart. Every view is decoded back and must
//! equal the original data, compared as compact JSON with keys in their original
//! order, or no view is shown. Pretty-printing whitespace is the only thing a view
//! drops, and `justless log` keeps the exact bytes.
//!
//! Format, after one legend line:
//! - a table is `N objects; fields: a,b=V,c?=D,d{x,y=W},e:[u,v]` followed by N rows.
//!   - `b=V` means every conforming object holds `b` with value `V` at that position.
//!   - `c?=D` means an empty cell decodes to `D`, and `~` decodes to an absent field.
//!   - `d{...}` unpacks a nested object column (up to depth 3); present unpacked
//!     objects render as positional tuples `(...)` with trailing empty cells omitted,
//!     and `null` or non-conforming objects stay compact JSON.
//!   - `e:[...]` unpacks an array-of-objects column as an inline sub-table `[ (...), ... ]`.
//!   - A row prefixed with `!` (`!{...}`) is a verbatim outlier object (used when a
//!     minority of <=15% of rows reorder keys or hold duplicate keys).
//!   - Each normal row holds the cells of the varying fields separated by commas; an
//!     empty cell is absent (or the column's `?=D` default). A field name is bare
//!     when it is a plain identifier and a JSON string otherwise.
//! - with grouping, one field is marked `@a`, and a line `@ CELL` before a run of
//!   rows gives their shared value of that field.
//! - an object that holds a table is shown one member per line, `KEY: VALUE` with the
//!   value as compact JSON, `KEY: N objects; fields: …` with the rows indented two
//!   spaces, or `KEY:` with the members of a nested object indented two spaces.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

/// The largest output this module reads.
pub const LIMIT: usize = 8 * 1024 * 1024;
const MIN_BYTES: usize = 256;
const DEPTH: usize = 128;
/// How deep objects are opened one member per line to reach a table.
const EXPAND_DEPTH: usize = 4;
/// How deep nested object columns and inline sub-tables unpack recursively.
const MAX_UNPACK_DEPTH: usize = 3;
const LEGEND_PREFIX_NOTE: &str =
    "fields are named once, NAME=VALUE holds in every row, an empty cell is an absent field";
const LEGEND_SUFFIX: &str = "; exact JSON in the full log]";
/// The legend of JSON shown without its pretty-printing whitespace.
const COMPACT: &str = "[JSON without its whitespace; exact JSON in the full log]";

#[derive(Clone, Copy, Default)]
struct LegendFlags {
    nested: bool,
    subtable: bool,
    default: bool,
    outlier: bool,
}

impl LegendFlags {
    fn merge(&mut self, other: Self) {
        self.nested |= other.nested;
        self.subtable |= other.subtable;
        self.default |= other.default;
        self.outlier |= other.outlier;
    }

    fn extra_len(self) -> usize {
        usize::from(self.nested) * ", {..} nested".len()
            + usize::from(self.subtable) * ", :[..] sub-table".len()
            + usize::from(self.default) * ", ?=V default (~ absent)".len()
            + usize::from(self.outlier) * ", !JSON verbatim".len()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Root {
    Array,
    Lines,
    Object,
}

impl Root {
    fn legend(self, flags: LegendFlags) -> String {
        let what = match self {
            Self::Array => "JSON array as a table",
            Self::Lines => "JSON lines as a table",
            Self::Object => "JSON object with arrays of objects as tables",
        };
        let mut out = format!("[{what}: {LEGEND_PREFIX_NOTE}");
        if flags.nested {
            out.push_str(", {..} nested");
        }
        if flags.subtable {
            out.push_str(", :[..] sub-table");
        }
        if flags.default {
            out.push_str(", ?=V default (~ absent)");
        }
        if flags.outlier {
            out.push_str(", !JSON verbatim");
        }
        out.push_str(LEGEND_SUFFIX);
        out.push('\n');
        out
    }

    fn from_legend(line: &str) -> Option<Self> {
        let rest = line.strip_prefix('[')?;
        let (root, mut rest) = [
            (Self::Array, "JSON array as a table: "),
            (Self::Lines, "JSON lines as a table: "),
            (
                Self::Object,
                "JSON object with arrays of objects as tables: ",
            ),
        ]
        .into_iter()
        .find_map(|(root, prefix)| rest.strip_prefix(prefix).map(|r| (root, r)))?;
        rest = rest.strip_prefix(LEGEND_PREFIX_NOTE)?;
        rest = rest.strip_prefix(", {..} nested").unwrap_or(rest);
        rest = rest.strip_prefix(", :[..] sub-table").unwrap_or(rest);
        rest = rest
            .strip_prefix(", ?=V default (~ absent)")
            .unwrap_or(rest);
        rest = rest.strip_prefix(", !JSON verbatim").unwrap_or(rest);
        (rest == LEGEND_SUFFIX).then_some(root)
    }
}

enum Node<'a> {
    /// A string, number or literal, as written.
    Raw(&'a str),
    Array(Vec<Node<'a>>),
    /// Keys as written, with their quotes.
    Object(Vec<(&'a str, Node<'a>)>),
}

struct Reader<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(text: &'a str, at: usize) -> Self {
        Self { text, at }
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> Option<()> {
        (self.peek() == Some(byte)).then(|| self.at += 1)
    }

    fn value(&mut self, depth: usize) -> Option<Node<'a>> {
        if depth > DEPTH {
            return None;
        }
        self.space();
        let start = self.at;
        match self.peek()? {
            b'"' => self.string().map(Node::Raw),
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                self.space();
                if self.eat(b']').is_none() {
                    loop {
                        items.push(self.value(depth + 1)?);
                        self.space();
                        if self.eat(b']').is_some() {
                            break;
                        }
                        self.eat(b',')?;
                    }
                }
                Some(Node::Array(items))
            }
            b'{' => {
                self.at += 1;
                let mut members = Vec::new();
                self.space();
                if self.eat(b'}').is_none() {
                    loop {
                        self.space();
                        let key = self.string()?;
                        self.space();
                        self.eat(b':')?;
                        members.push((key, self.value(depth + 1)?));
                        self.space();
                        if self.eat(b'}').is_some() {
                            break;
                        }
                        self.eat(b',')?;
                    }
                }
                Some(Node::Object(members))
            }
            b't' | b'f' | b'n' => {
                let word = ["true", "false", "null"]
                    .into_iter()
                    .find(|word| self.text[start..].starts_with(word))?;
                self.at += word.len();
                Some(Node::Raw(word))
            }
            _ => self.number().map(Node::Raw),
        }
    }

    fn string(&mut self) -> Option<&'a str> {
        let start = self.at;
        self.eat(b'"')?;
        loop {
            match self.peek()? {
                b'"' => {
                    self.at += 1;
                    return Some(&self.text[start..self.at]);
                }
                b'\\' => {
                    self.at += 1;
                    match self.peek()? {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.at += 1,
                        b'u' => {
                            let hex = self.text.get(self.at + 1..self.at + 5)?;
                            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                                return None;
                            }
                            self.at += 5;
                        }
                        _ => return None,
                    }
                }
                byte if byte < 0x20 => return None,
                _ => self.at += 1,
            }
        }
    }

    fn number(&mut self) -> Option<&'a str> {
        let start = self.at;
        let digits = |reader: &mut Self| {
            let from = reader.at;
            while reader.peek().is_some_and(|b| b.is_ascii_digit()) {
                reader.at += 1;
            }
            reader.at > from
        };
        self.eat(b'-');
        if self.eat(b'0').is_none() && !digits(self) {
            return None;
        }
        if self.eat(b'.').is_some() && !digits(self) {
            return None;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !digits(self) {
                return None;
            }
        }
        Some(&self.text[start..self.at])
    }
}

/// The text as one JSON value, with nothing but whitespace around it.
fn parse(text: &str) -> Option<Node<'_>> {
    let mut reader = Reader::new(text, 0);
    let node = reader.value(0)?;
    reader.space();
    (reader.at == text.len()).then_some(node)
}

fn compact(node: &Node, out: &mut String) {
    match node {
        Node::Raw(raw) => out.push_str(raw),
        Node::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                compact(item, out);
            }
            out.push(']');
        }
        Node::Object(members) => {
            out.push('{');
            for (index, (key, value)) in members.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(key);
                out.push(':');
                compact(value, out);
            }
            out.push('}');
        }
    }
}

fn compacted(node: &Node) -> String {
    let mut out = String::new();
    compact(node, &mut out);
    out
}

fn compacted_len(node: &Node) -> usize {
    match node {
        Node::Raw(raw) => raw.len(),
        Node::Array(items) => {
            2 + items.len().saturating_sub(1) + items.iter().map(compacted_len).sum::<usize>()
        }
        Node::Object(members) => {
            2 + members.len().saturating_sub(1)
                + members
                    .iter()
                    .map(|(k, v)| k.len() + 1 + compacted_len(v))
                    .sum::<usize>()
        }
    }
}

/// The views of JSON output that are smaller than `text` and decode to the same
/// data, smallest first: the tables, when a table helps, and the compact JSON.
/// `group` also folds consecutive rows that share a field's value.
pub fn views(text: &str, group: bool) -> Vec<String> {
    if text.len() < MIN_BYTES || text.len() > LIMIT {
        return Vec::new();
    }
    let Some(json) = Json::read(text) else {
        return Vec::new();
    };
    let mut views: Vec<String> = [json.tables(group), json.compact()]
        .into_iter()
        .flatten()
        .filter(|view| view.len() < text.len())
        .collect();
    views.sort_by_key(String::len);
    views
}

/// Output read as one JSON array or object, or as JSON lines whose records are
/// objects, with its data as compact JSON.
struct Json<'a> {
    root: Root,
    /// The value, or for JSON lines an array of the records.
    node: Node<'a>,
    /// What every view must decode to: compact JSON, one line per record for
    /// JSON lines.
    original: String,
}

impl<'a> Json<'a> {
    fn read(text: &'a str) -> Option<Self> {
        let first = text.trim_start().as_bytes().first().copied()?;
        if first != b'[' && first != b'{' {
            return None;
        }
        let (root, node) = match parse(text) {
            Some(node @ Node::Array(_)) => (Root::Array, node),
            Some(node @ Node::Object(_)) => (Root::Object, node),
            Some(Node::Raw(_)) => return None,
            None => {
                let mut records = Vec::new();
                for line in text.trim_end().split('\n') {
                    match parse(line)? {
                        node @ Node::Object(_) => records.push(node),
                        _ => return None,
                    }
                }
                (Root::Lines, Node::Array(records))
            }
        };
        let original = match (root, &node) {
            (Root::Lines, Node::Array(records)) => {
                records.iter().map(compacted).collect::<Vec<_>>().join("\n")
            }
            (_, node) => compacted(node),
        };
        Some(Self {
            root,
            node,
            original,
        })
    }

    fn render_tables(&self, group: bool, deep: bool) -> Option<(String, LegendFlags, usize)> {
        let mut body = String::new();
        let mut flags = LegendFlags::default();
        let mut deep_savings = 0usize;
        let shown = match (self.root, &self.node) {
            (Root::Object, Node::Object(members)) => expand(
                members,
                0,
                group,
                deep,
                0,
                &mut body,
                &mut flags,
                &mut deep_savings,
            ),
            (_, Node::Array(items)) => table(
                items,
                0,
                group,
                deep,
                &mut body,
                &mut flags,
                &mut deep_savings,
            ),
            _ => false,
        };
        if !shown {
            return None;
        }
        let mut view = self.root.legend(flags);
        view.push_str(&body);
        (decode(&view)? == self.original).then_some((view, flags, deep_savings))
    }

    /// The table view, when a table helps and it decodes to the same data.
    fn tables(&self, group: bool) -> Option<String> {
        let deep = self.render_tables(group, true);
        match deep {
            Some((deep_view, flags, deep_savings)) => {
                // Only fall back to comparing against the flat view if the extra
                // legend text could outweigh the deep-schema savings.
                if flags.extra_len() > 0
                    && deep_savings <= flags.extra_len()
                    && !flags.outlier
                    && let Some((flat_view, _, _)) = self.render_tables(group, false)
                    && flat_view.len() <= deep_view.len()
                {
                    return Some(flat_view);
                }
                Some(deep_view)
            }
            None => self.render_tables(group, false).map(|(view, _, _)| view),
        }
    }

    /// The same JSON without the whitespace between its tokens, after a legend
    /// line, when it decodes to the same data.
    fn compact(&self) -> Option<String> {
        let view = format!("{COMPACT}\n{}\n", self.original);
        (decode(&view)? == self.original).then_some(view)
    }
}

/// A field name as the header shows it: bare when it is a plain identifier.
fn field_name(key: &str) -> &str {
    let inner = &key[1..key.len() - 1];
    if bare(inner) { inner } else { key }
}

fn bare(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.' | '/' | '-'))
}

struct Columns<'n, 'a> {
    keys: Vec<&'a str>,
    /// For each item: `Some(cells)` when conforming, `None` when an outlier row.
    rows: Vec<Option<Vec<Option<&'n Node<'a>>>>>,
    conforming: usize,
    /// The objects as compact JSON lines, in bytes.
    inline: usize,
}

/// The most fields a table holds.
const MAX_FIELDS: usize = 256;

/// Try merging one object's keys into `keys` while preserving every object's exact
/// key order and rejecting duplicate keys or overflow beyond `MAX_FIELDS`.
fn try_merge_keys<'a>(keys: &[&'a str], members: &[(&'a str, Node<'a>)]) -> Option<Vec<&'a str>> {
    let mut merged = Vec::with_capacity(keys.len() + members.len());
    let mut known = 0;
    let mut new = Vec::new();
    for (key, _) in members {
        if new.contains(key) || merged.contains(key) {
            return None;
        }
        match keys.iter().position(|k| k == key) {
            Some(position) if position >= known => {
                merged.extend_from_slice(&keys[known..position]);
                merged.append(&mut new);
                merged.push(*key);
                known = position + 1;
            }
            Some(_) => return None,
            None if keys.len() + new.len() >= MAX_FIELDS => return None,
            None => new.push(*key),
        }
    }
    merged.extend_from_slice(&keys[known..]);
    merged.append(&mut new);
    Some(merged)
}

fn has_duplicate_keys(members: &[(&str, Node)]) -> bool {
    if members.len() <= 16 {
        for (i, (k, _)) in members.iter().enumerate() {
            if members[i + 1..].iter().any(|(other, _)| other == k) {
                return true;
            }
        }
        false
    } else {
        let mut seen = HashSet::with_capacity(members.len());
        members.iter().any(|(k, _)| !seen.insert(*k))
    }
}

fn extract_row<'n, 'a>(
    keys: &[&'a str],
    members: &'n [(&'a str, Node<'a>)],
) -> Vec<Option<&'n Node<'a>>> {
    let mut row = Vec::with_capacity(keys.len());
    let mut m = 0;
    for &key in keys {
        if let Some((mk, mv)) = members.get(m)
            && *mk == key
        {
            row.push(Some(mv));
            m += 1;
        } else {
            row.push(None);
        }
    }
    debug_assert_eq!(m, members.len());
    row
}

/// The columns of an array of objects whose keys follow one order, without repeats,
/// optionally escaping up to 15% outlier objects (reordered or duplicate keys).
fn columns_from_refs<'n, 'a>(
    items: &[&'n Node<'a>],
    allow_outliers: bool,
) -> Option<Columns<'n, 'a>> {
    let mut member_slices: Vec<&'n [(&'a str, Node<'a>)]> = Vec::with_capacity(items.len());
    for item in items {
        let Node::Object(members) = item else {
            return None;
        };
        member_slices.push(members.as_slice());
    }

    // Fast path: every object conforms cleanly with zero outliers.
    let mut keys: Vec<&'a str> = Vec::new();
    let mut fast_ok = true;
    let (mut widest, mut present) = (0usize, 0usize);
    for members in &member_slices {
        if let Some(merged) = try_merge_keys(&keys, members) {
            keys = merged;
            widest = members.len().max(widest);
            present += members.len();
        } else {
            fast_ok = false;
            break;
        }
    }

    let inline: usize = items.iter().map(|item| compacted_len(item) + 1).sum();

    if fast_ok {
        if keys.is_empty() {
            return None;
        }
        let absent = items.len() * keys.len() - present;
        if keys.len() > 2 * widest || absent >= inline + items.len() {
            return None;
        }
        let rows = member_slices
            .iter()
            .map(|members| Some(extract_row(&keys, members)))
            .collect();
        return Some(Columns {
            keys,
            rows,
            conforming: items.len(),
            inline,
        });
    }

    // Slow path: allow <=15% outliers (conforming >= 85%, i.e. conforming * 20 >= total * 17).
    // Requires at least 7 rows so 1 outlier still leaves >= 85% conforming (6/7 = 85.7%).
    if !allow_outliers || items.len() < 7 {
        return None;
    }

    let mut valid_indices: Vec<usize> = Vec::with_capacity(items.len());
    for (idx, members) in member_slices.iter().enumerate() {
        if !has_duplicate_keys(members) {
            valid_indices.push(idx);
        }
    }
    if valid_indices.len() * 20 < items.len() * 17 {
        return None;
    }

    // Score candidate rows so dominant key orders merge before rare conflicting orders.
    let mut sig_freq: HashMap<Vec<&'a str>, usize> = HashMap::new();
    let mut pair_freq: HashMap<(&'a str, &'a str), usize> = HashMap::new();
    for &idx in &valid_indices {
        let sig: Vec<&'a str> = member_slices[idx].iter().map(|(k, _)| *k).collect();
        *sig_freq.entry(sig).or_insert(0) += 1;
    }
    let sample_limit = valid_indices.len().min(256);
    for &idx in &valid_indices[..sample_limit] {
        let members = member_slices[idx];
        if members.len() <= 32 {
            for i in 0..members.len() {
                for j in (i + 1)..members.len() {
                    *pair_freq.entry((members[i].0, members[j].0)).or_insert(0) += 1;
                }
            }
        } else {
            for pair in members.windows(2) {
                *pair_freq.entry((pair[0].0, pair[1].0)).or_insert(0) += 1;
            }
        }
    }

    let mut ranked: Vec<(bool, usize, isize, usize, usize)> = valid_indices
        .iter()
        .map(|&idx| {
            let members = member_slices[idx];
            let sig: Vec<&'a str> = members.iter().map(|(k, _)| *k).collect();
            let freq = sig_freq.get(&sig).copied().unwrap_or(0);
            let mut supported = true;
            let mut min_margin = 0isize;
            let mut has_pair = false;
            let mut check_pair = |u: &'a str, v: &'a str| {
                let fwd = pair_freq.get(&(u, v)).copied().unwrap_or(0) as isize;
                let rev = pair_freq.get(&(v, u)).copied().unwrap_or(0) as isize;
                let margin = fwd - rev;
                if margin < 0 {
                    supported = false;
                }
                if !has_pair || margin < min_margin {
                    min_margin = margin;
                    has_pair = true;
                }
            };
            if members.len() <= 32 {
                for i in 0..members.len() {
                    for j in (i + 1)..members.len() {
                        check_pair(members[i].0, members[j].0);
                    }
                }
            } else {
                for pair in members.windows(2) {
                    check_pair(pair[0].0, pair[1].0);
                }
            }
            (supported, freq, min_margin, members.len(), idx)
        })
        .collect();
    ranked.sort_by_key(|&(supported, freq, margin, len, idx)| {
        (
            Reverse(supported),
            Reverse(freq),
            Reverse(margin),
            Reverse(len),
            idx,
        )
    });

    let mut keys: Vec<&'a str> = Vec::new();
    let mut is_conforming = vec![false; items.len()];
    let mut conforming = 0usize;
    let (mut widest, mut present) = (0usize, 0usize);
    for &(_, _, _, _, idx) in &ranked {
        let members = member_slices[idx];
        if let Some(merged) = try_merge_keys(&keys, members) {
            keys = merged;
            is_conforming[idx] = true;
            conforming += 1;
            widest = members.len().max(widest);
            present += members.len();
        }
    }

    if keys.is_empty() || conforming < 2 || conforming * 20 < items.len() * 17 {
        return None;
    }
    let absent = conforming * keys.len() - present;
    if keys.len() > 2 * widest || absent >= inline + conforming {
        return None;
    }
    let rows = member_slices
        .iter()
        .enumerate()
        .map(|(idx, members)| is_conforming[idx].then(|| extract_row(&keys, members)))
        .collect();
    Some(Columns {
        keys,
        rows,
        conforming,
        inline,
    })
}

#[cfg(test)]
fn columns<'n, 'a>(items: &'n [Node<'a>]) -> Option<Columns<'n, 'a>> {
    let refs: Vec<&Node> = items.iter().collect();
    columns_from_refs(&refs, true)
}

/// Format a nested object or sub-table element tuple, omitting trailing empty cells.
fn format_trimmed_tuple(cells: &[Option<String>]) -> String {
    let Some(last) = cells.iter().rposition(Option::is_some) else {
        return String::from("()");
    };
    let mut out = String::from("(");
    for (index, cell) in cells[..=last].iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        if let Some(text) = cell {
            out.push_str(text);
        }
    }
    out.push(')');
    out
}

/// Pick a majority-default value `V` when it appears in >=50% of rows (strictly more
/// than any runner-up) and strictly saves bytes after header `?=V` and absent `~` costs.
fn choose_majority_default(
    rendered: &[Option<String>],
    inside_tuple: bool,
) -> Option<(String, Vec<Option<String>>, usize)> {
    let total = rendered.len();
    if total < 2 {
        return None;
    }
    let mut absent_count = 0usize;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for cell in rendered {
        match cell {
            Some(val) => *counts.entry(val.as_str()).or_insert(0) += 1,
            None => absent_count += 1,
        }
    }
    let mut ranked: Vec<(&str, usize)> = counts.into_iter().collect();
    ranked.sort_by_key(|&(val, count)| (Reverse(count), Reverse(val.len()), val));
    let &(best_val, best_count) = ranked.first()?;
    let second_count = ranked.get(1).map_or(0, |&(_, c)| c);
    if best_count * 2 < total || best_count <= second_count {
        return None;
    }
    let header_cost = 2 + best_val.len();
    let saved_bytes = best_count * best_val.len();
    // Inside a trimmed tuple, replacing a previously trimmed trailing absent cell with
    // `~` can also reintroduce one comma, so charge 2 bytes per absent cell there.
    let absent_penalty = absent_count * if inside_tuple { 2 } else { 1 };
    if saved_bytes <= header_cost + absent_penalty {
        return None;
    }
    let net_saved = saved_bytes - header_cost - absent_penalty;
    let rewritten = rendered
        .iter()
        .map(|cell| match cell {
            Some(val) if val == best_val => None,
            Some(val) => Some(val.clone()),
            None => Some(String::from("~")),
        })
        .collect();
    Some((best_val.to_owned(), rewritten, net_saved))
}

struct ColumnChoice {
    header_suffix: String,
    cells: Vec<Option<String>>,
    /// Shape header suffix without `?=DEFAULT`, for `@` grouping at depth 0.
    group_header_suffix: String,
    /// Rendered values without `?=DEFAULT` applied (None = absent), for `@` grouping.
    group_values: Vec<Option<String>>,
    /// Legend flags needed if this column is selected for `@` grouping (drops outer `?=`).
    group_flags: LegendFlags,
    cost: usize,
    flags: LegendFlags,
    /// Bytes saved relative to a plain column without defaults or unpacking.
    savings_vs_plain: usize,
    is_constant: bool,
}

fn cells_byte_len(cells: &[Option<String>]) -> usize {
    cells.iter().map(|c| c.as_deref().map_or(0, str::len)).sum()
}

/// Build a nested schema (for an unpacked object column or an inline sub-table's
/// elements) at `depth` (`1..=MAX_UNPACK_DEPTH`).
fn build_nested_schema<'n, 'a>(
    objects: &[&'n Node<'a>],
    depth: usize,
) -> Option<(String, Vec<String>, LegendFlags)> {
    let Columns {
        keys,
        rows,
        conforming,
        ..
    } = columns_from_refs(objects, true)?;

    let conforming_rows: Vec<&[Option<&Node>]> =
        rows.iter().filter_map(|row| row.as_deref()).collect();
    debug_assert_eq!(conforming_rows.len(), conforming);

    let mut header = String::new();
    let mut varying_columns: Vec<Vec<Option<String>>> = Vec::new();
    let mut flags = LegendFlags::default();

    for (col_idx, &key) in keys.iter().enumerate() {
        let col_nodes: Vec<Option<&Node>> = conforming_rows.iter().map(|r| r[col_idx]).collect();
        let choice = plan_column(&col_nodes, depth, true);
        if col_idx > 0 {
            header.push(',');
        }
        header.push_str(field_name(key));
        header.push_str(&choice.header_suffix);
        flags.merge(choice.flags);
        if !choice.is_constant {
            varying_columns.push(choice.cells);
        }
    }

    let mut rendered = Vec::with_capacity(objects.len());
    let mut conf_cursor = 0usize;
    let mut row_buf: Vec<Option<String>> = Vec::with_capacity(varying_columns.len());
    for (idx, row) in rows.iter().enumerate() {
        if row.is_some() {
            row_buf.clear();
            for col in &varying_columns {
                row_buf.push(col[conf_cursor].clone());
            }
            rendered.push(format_trimmed_tuple(&row_buf));
            conf_cursor += 1;
        } else {
            // Outlier nested object / sub-table element stays verbatim compact JSON `{...}`.
            rendered.push(compacted(objects[idx]));
        }
    }
    Some((header, rendered, flags))
}

/// Plan the smallest lossless encoding for one column across `conforming` rows.
fn plan_column<'n, 'a>(cells: &[Option<&'n Node<'a>>], depth: usize, deep: bool) -> ColumnChoice {
    let raw_texts: Vec<Option<String>> = cells.iter().map(|cell| cell.map(compacted)).collect();

    // Constant column: every conforming row holds this key with the exact same value.
    if let Some(first) = &raw_texts[0]
        && raw_texts.iter().all(|cell| cell.as_ref() == Some(first))
    {
        let header_suffix = format!("={first}");
        let cost = header_suffix.len();
        return ColumnChoice {
            header_suffix,
            cells: Vec::new(),
            group_header_suffix: String::new(),
            group_values: Vec::new(),
            group_flags: LegendFlags::default(),
            cost,
            flags: LegendFlags::default(),
            savings_vs_plain: 0,
            is_constant: true,
        };
    }

    let plain_cost = cells_byte_len(&raw_texts);
    let mut best = ColumnChoice {
        header_suffix: String::new(),
        cells: raw_texts.clone(),
        group_header_suffix: String::new(),
        group_values: raw_texts.clone(),
        group_flags: LegendFlags::default(),
        cost: plain_cost,
        flags: LegendFlags::default(),
        savings_vs_plain: 0,
        is_constant: false,
    };

    if !deep {
        return best;
    }

    let inside_tuple = depth > 0;

    // Candidate 1: Plain column with majority default `?=V`.
    if let Some((default_val, default_cells, _)) = choose_majority_default(&raw_texts, inside_tuple)
    {
        let header_suffix = format!("?={default_val}");
        let cost = header_suffix.len() + cells_byte_len(&default_cells);
        if cost < best.cost {
            let flags = LegendFlags {
                default: true,
                ..LegendFlags::default()
            };
            best = ColumnChoice {
                header_suffix,
                cells: default_cells,
                group_header_suffix: String::new(),
                group_values: raw_texts.clone(),
                group_flags: LegendFlags::default(),
                cost,
                flags,
                savings_vs_plain: plain_cost - cost,
                is_constant: false,
            };
        }
    }

    if depth >= MAX_UNPACK_DEPTH {
        return best;
    }

    // Candidate 2: Unpacked nested object column `col{...}` (optionally with `?=D`).
    let obj_nodes: Vec<&Node> = cells
        .iter()
        .filter_map(|cell| match cell {
            Some(node @ Node::Object(_)) => Some(*node),
            _ => None,
        })
        .collect();
    if obj_nodes.len() >= 2
        && let Some((sub_header, obj_renders, mut sub_flags)) =
            build_nested_schema(&obj_nodes, depth + 1)
    {
        sub_flags.nested = true;
        let mut obj_iter = obj_renders.into_iter();
        let mapped: Vec<Option<String>> = cells
            .iter()
            .enumerate()
            .map(|(i, cell)| match cell {
                None => None,
                Some(Node::Object(_)) => obj_iter.next(),
                Some(_) => raw_texts[i].clone(),
            })
            .collect();
        let base_suffix = format!("{{{sub_header}}}");
        let base_cost = base_suffix.len() + cells_byte_len(&mapped);
        if base_cost < best.cost {
            best = ColumnChoice {
                header_suffix: base_suffix.clone(),
                cells: mapped.clone(),
                group_header_suffix: base_suffix.clone(),
                group_values: mapped.clone(),
                group_flags: sub_flags,
                cost: base_cost,
                flags: sub_flags,
                savings_vs_plain: plain_cost - base_cost,
                is_constant: false,
            };
        }
        if let Some((default_val, default_cells, _)) =
            choose_majority_default(&mapped, inside_tuple)
        {
            let header_suffix = format!("{base_suffix}?={default_val}");
            let cost = header_suffix.len() + cells_byte_len(&default_cells);
            if cost < best.cost {
                let flags = LegendFlags {
                    default: true,
                    ..sub_flags
                };
                best = ColumnChoice {
                    header_suffix,
                    cells: default_cells,
                    group_header_suffix: base_suffix,
                    group_values: mapped,
                    group_flags: sub_flags,
                    cost,
                    flags,
                    savings_vs_plain: plain_cost - cost,
                    is_constant: false,
                };
            }
        }
    }

    // Candidate 3: Inline sub-table for array-of-objects column `col:[...]` (optionally with `?=D`).
    let all_arrays_hold_objects = cells.iter().all(|cell| match cell {
        Some(Node::Array(elems)) => elems.iter().all(|e| matches!(e, Node::Object(_))),
        _ => true,
    });
    if all_arrays_hold_objects {
        let mut all_elems: Vec<&Node> = Vec::new();
        for cell in cells {
            if let Some(Node::Array(elems)) = cell {
                all_elems.extend(elems.iter());
            }
        }
        if all_elems.len() >= 2
            && let Some((sub_header, elem_renders, mut sub_flags)) =
                build_nested_schema(&all_elems, depth + 1)
        {
            sub_flags.subtable = true;
            let mut elem_iter = elem_renders.into_iter();
            let mapped: Vec<Option<String>> = cells
                .iter()
                .enumerate()
                .map(|(i, cell)| match cell {
                    None => None,
                    Some(Node::Array(elems)) => {
                        let mut arr = String::from("[");
                        for idx in 0..elems.len() {
                            if idx > 0 {
                                arr.push(',');
                            }
                            arr.push_str(&elem_iter.next().unwrap_or_default());
                        }
                        arr.push(']');
                        Some(arr)
                    }
                    Some(_) => raw_texts[i].clone(),
                })
                .collect();
            let base_suffix = format!(":[{sub_header}]");
            let base_cost = base_suffix.len() + cells_byte_len(&mapped);
            if base_cost < best.cost {
                best = ColumnChoice {
                    header_suffix: base_suffix.clone(),
                    cells: mapped.clone(),
                    group_header_suffix: base_suffix.clone(),
                    group_values: mapped.clone(),
                    group_flags: sub_flags,
                    cost: base_cost,
                    flags: sub_flags,
                    savings_vs_plain: plain_cost - base_cost,
                    is_constant: false,
                };
            }
            if let Some((default_val, default_cells, _)) =
                choose_majority_default(&mapped, inside_tuple)
            {
                let header_suffix = format!("{base_suffix}?={default_val}");
                let cost = header_suffix.len() + cells_byte_len(&default_cells);
                if cost < best.cost {
                    let flags = LegendFlags {
                        default: true,
                        ..sub_flags
                    };
                    best = ColumnChoice {
                        header_suffix,
                        cells: default_cells,
                        group_header_suffix: base_suffix,
                        group_values: mapped,
                        group_flags: sub_flags,
                        cost,
                        flags,
                        savings_vs_plain: plain_cost - cost,
                        is_constant: false,
                    };
                }
            }
        }
    }

    best
}

/// Write `items` as a table when that is smaller than their compact JSON.
fn table(
    items: &[Node],
    indent: usize,
    group: bool,
    deep: bool,
    out: &mut String,
    flags: &mut LegendFlags,
    deep_savings: &mut usize,
) -> bool {
    if items.len() < 2 {
        return false;
    }
    let refs: Vec<&Node> = items.iter().collect();
    let Some(Columns {
        keys,
        rows,
        conforming,
        inline,
    }) = columns_from_refs(&refs, deep)
    else {
        return false;
    };
    let has_outliers = conforming < items.len();
    let conforming_rows: Vec<&[Option<&Node>]> =
        rows.iter().filter_map(|row| row.as_deref()).collect();

    let mut choices: Vec<ColumnChoice> = Vec::with_capacity(keys.len());
    for col_idx in 0..keys.len() {
        let col_nodes: Vec<Option<&Node>> = conforming_rows.iter().map(|r| r[col_idx]).collect();
        choices.push(plan_column(&col_nodes, 0, deep));
    }

    let varying: Vec<usize> = (0..keys.len())
        .filter(|&c| !choices[c].is_constant)
        .collect();
    if varying.is_empty() {
        return false;
    }

    let grouped = if group {
        grouping(&choices, &varying, indent)
    } else {
        None
    };

    let mut table_flags = LegendFlags::default();
    let mut table_savings = 0usize;
    if has_outliers {
        table_flags.outlier = true;
    }
    for (col_idx, choice) in choices.iter().enumerate() {
        if grouped == Some(col_idx) {
            table_flags.merge(choice.group_flags);
        } else {
            table_flags.merge(choice.flags);
            table_savings += choice.savings_vs_plain;
        }
    }

    let pad = " ".repeat(indent);
    let mut block = format!("{} objects; fields: ", items.len());
    for (column, key) in keys.iter().enumerate() {
        if column > 0 {
            block.push(',');
        }
        if grouped == Some(column) {
            block.push('@');
            block.push_str(field_name(key));
            block.push_str(&choices[column].group_header_suffix);
        } else {
            block.push_str(field_name(key));
            block.push_str(&choices[column].header_suffix);
        }
    }
    block.push('\n');

    let mut current: Option<Option<&str>> = None;
    let mut conf_cursor = 0usize;
    for (item_idx, row) in rows.iter().enumerate() {
        if row.is_none() {
            block.push_str(&pad);
            block.push('!');
            compact(&items[item_idx], &mut block);
            block.push('\n');
            continue;
        }
        if let Some(column) = grouped {
            let value = choices[column].group_values[conf_cursor].as_deref();
            if current != Some(value) {
                current = Some(value);
                block.push_str(&pad);
                block.push('@');
                if let Some(value) = value {
                    block.push(' ');
                    block.push_str(value);
                }
                block.push('\n');
            }
        }
        block.push_str(&pad);
        let mut first = true;
        for &column in varying.iter().filter(|&&c| grouped != Some(c)) {
            if !first {
                block.push(',');
            }
            first = false;
            if let Some(cell) = &choices[column].cells[conf_cursor] {
                block.push_str(cell);
            }
        }
        block.push('\n');
        conf_cursor += 1;
    }

    if block.len() >= inline {
        return false;
    }
    flags.merge(table_flags);
    *deep_savings += table_savings;
    out.push_str(&block);
    true
}

/// The varying column whose runs of equal values save the most when each run is
/// named once.
fn grouping(choices: &[ColumnChoice], varying: &[usize], indent: usize) -> Option<usize> {
    if varying.len() < 2 {
        return None;
    }
    varying
        .iter()
        .map(|&column| {
            let choice = &choices[column];
            // Ungrouped cost = header suffix + cell bytes + 1 comma per conforming row.
            let ungrouped = choice.cost as isize + choice.group_values.len() as isize;
            // Grouped cost = `@` + shape suffix without default + `@ CELL\n` run headers.
            let mut grouped = 1isize + choice.group_header_suffix.len() as isize;
            let mut previous: Option<&Option<String>> = None;
            for cell in &choice.group_values {
                if previous != Some(cell) {
                    let length = cell.as_deref().map_or(0, str::len) as isize;
                    grouped += indent as isize + 3 + length;
                    previous = Some(cell);
                }
            }
            (ungrouped - grouped, column)
        })
        .filter(|&(saved, _)| saved > 0)
        .max_by_key(|&(saved, column)| (saved, Reverse(column)))
        .map(|(_, column)| column)
}

/// Write an object one member per line when a member holds a table.
#[allow(clippy::too_many_arguments)]
fn expand(
    members: &[(&str, Node)],
    indent: usize,
    group: bool,
    deep: bool,
    depth: usize,
    out: &mut String,
    flags: &mut LegendFlags,
    deep_savings: &mut usize,
) -> bool {
    if depth > EXPAND_DEPTH || members.is_empty() {
        return false;
    }
    let pad = " ".repeat(indent);
    let mut block = String::new();
    let mut any = false;
    let mut local_flags = LegendFlags::default();
    let mut local_savings = 0usize;
    for (key, value) in members {
        block.push_str(&pad);
        block.push_str(field_name(key));
        block.push(':');
        let mut nested = String::new();
        let shown = match value {
            Node::Array(items) => table(
                items,
                indent + 2,
                group,
                deep,
                &mut nested,
                &mut local_flags,
                &mut local_savings,
            ),
            Node::Object(inner) => {
                nested.push('\n');
                expand(
                    inner,
                    indent + 2,
                    group,
                    deep,
                    depth + 1,
                    &mut nested,
                    &mut local_flags,
                    &mut local_savings,
                )
            }
            Node::Raw(_) => false,
        };
        if shown {
            any = true;
            if !nested.starts_with('\n') {
                block.push(' ');
            }
            block.push_str(&nested);
        } else {
            block.push(' ');
            compact(value, &mut block);
            block.push('\n');
        }
    }
    if any {
        flags.merge(local_flags);
        *deep_savings += local_savings;
        out.push_str(&block);
    }
    any
}

/// Rebuild compact JSON from a view: the data the view claims to show.
fn decode(view: &str) -> Option<String> {
    let mut lines = view.strip_suffix('\n')?.split('\n');
    let legend = lines.next()?;
    if legend == COMPACT {
        // Each line is one value written compact: the document, or one JSON line's
        // record.
        return lines
            .map(|line| {
                value_at(line, 0)
                    .filter(|(value, end)| *end == line.len() && value == line)
                    .map(|(value, _)| value)
            })
            .collect::<Option<Vec<_>>>()
            .filter(|values| !values.is_empty())
            .map(|values| values.join("\n"));
    }
    let root = Root::from_legend(legend)?;
    let mut lines = lines.peekable();
    let mut out = String::new();
    match root {
        Root::Object => {
            members(&mut lines, 0, &mut out)?;
        }
        Root::Array | Root::Lines => {
            let header = lines.next()?;
            let rows = decode_table(header, &mut lines, 0)?;
            if root == Root::Array {
                out.push('[');
                out.push_str(&rows.join(","));
                out.push(']');
            } else {
                out.push_str(&rows.join("\n"));
            }
        }
    }
    lines.next().is_none().then_some(out)
}

type Lines<'v> = std::iter::Peekable<std::str::Split<'v, char>>;

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn members(lines: &mut Lines, indent: usize, out: &mut String) -> Option<()> {
    out.push('{');
    let mut first = true;
    while let Some(line) = lines.peek() {
        if indent_of(line) != indent || line.len() == indent {
            break;
        }
        let line = &lines.next()?[indent..];
        let (key, after) = header_key(line)?;
        let rest = after.strip_prefix(':')?;
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&key);
        out.push(':');
        if rest.is_empty() {
            members(lines, indent + 2, out)?;
        } else {
            let rest = rest.strip_prefix(' ')?;
            if table_header(rest).is_some() {
                let rows = decode_table(rest, lines, indent + 2)?;
                out.push('[');
                out.push_str(&rows.join(","));
                out.push(']');
            } else {
                let (value, end) = value_at(rest, 0)?;
                if end != rest.len() {
                    return None;
                }
                out.push_str(&value);
            }
        }
    }
    if first {
        return None;
    }
    out.push('}');
    Some(())
}

/// One key at the start of `text`, bare or quoted, as a JSON string, and the rest.
fn header_key(text: &str) -> Option<(String, &str)> {
    if text.starts_with('"') {
        let mut reader = Reader::new(text, 0);
        let key = reader.string()?;
        return Some((key.to_owned(), &text[reader.at..]));
    }
    let end = text
        .find([',', '=', ':', '?', '{', '}', ']'])
        .unwrap_or(text.len());
    let name = &text[..end];
    bare(name).then(|| (format!("\"{name}\""), &text[end..]))
}

/// The row count and fields of a table header.
fn table_header(line: &str) -> Option<(usize, &str)> {
    let (count, fields) = line.split_once(" objects; fields: ")?;
    if count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((count.parse().ok()?, fields))
}

/// One compact JSON value at `at` in `text`, and where it ends.
fn value_at(text: &str, at: usize) -> Option<(String, usize)> {
    let mut reader = Reader::new(text, at);
    if matches!(reader.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        return None;
    }
    let node = reader.value(0)?;
    Some((compacted(&node), reader.at))
}

struct DecodedSchema {
    fields: Vec<DecodedField>,
}

struct DecodedField {
    key: String,
    shape: DecodedShape,
    mode: DecodedMode,
}

enum DecodedShape {
    Plain,
    Object(DecodedSchema),
    SubTable(DecodedSchema),
}

enum DecodedMode {
    Constant(String),
    Group,
    Varying { default: Option<String> },
}

fn parse_schema(
    text: &str,
    mut at: usize,
    stop_at: Option<u8>,
    allow_group: bool,
    depth: usize,
) -> Option<(DecodedSchema, usize)> {
    if depth > MAX_UNPACK_DEPTH {
        return None;
    }
    let mut fields = Vec::new();
    let mut seen_group = false;
    while at < text.len() {
        if stop_at.is_some_and(|b| text.as_bytes()[at] == b) {
            break;
        }
        if !fields.is_empty() {
            (text.as_bytes().get(at) == Some(&b',')).then_some(())?;
            at += 1;
            if at == text.len() || stop_at.is_some_and(|b| text.as_bytes()[at] == b) {
                return None;
            }
        }
        let grouped = text.as_bytes().get(at) == Some(&b'@');
        if grouped {
            if !allow_group || seen_group {
                return None;
            }
            seen_group = true;
            at += 1;
        }
        let (key, rest) = header_key(&text[at..])?;
        at = text.len() - rest.len();

        let shape = if text[at..].starts_with('{') {
            let (sub, next) = parse_schema(text, at + 1, Some(b'}'), false, depth + 1)?;
            (text.as_bytes().get(next) == Some(&b'}')).then_some(())?;
            at = next + 1;
            DecodedShape::Object(sub)
        } else if text[at..].starts_with(":[") {
            let (sub, next) = parse_schema(text, at + 2, Some(b']'), false, depth + 1)?;
            (text.as_bytes().get(next) == Some(&b']')).then_some(())?;
            at = next + 1;
            DecodedShape::SubTable(sub)
        } else {
            DecodedShape::Plain
        };

        let mode = if text[at..].starts_with('=') {
            if grouped || !matches!(shape, DecodedShape::Plain) {
                return None;
            }
            let (value, next) = value_at(text, at + 1)?;
            at = next;
            DecodedMode::Constant(value)
        } else if text[at..].starts_with("?=") {
            if grouped {
                return None;
            }
            let (default_val, next) = decode_present_value(&shape, text, at + 2)?;
            at = next;
            DecodedMode::Varying {
                default: Some(default_val),
            }
        } else if grouped {
            DecodedMode::Group
        } else {
            DecodedMode::Varying { default: None }
        };

        fields.push(DecodedField { key, shape, mode });
    }
    (!fields.is_empty()).then_some((DecodedSchema { fields }, at))
}

fn decode_present_value(shape: &DecodedShape, text: &str, at: usize) -> Option<(String, usize)> {
    match shape {
        DecodedShape::Plain => value_at(text, at),
        DecodedShape::Object(sub) => {
            if text.as_bytes().get(at) == Some(&b'(') {
                decode_tuple(sub, text, at)
            } else {
                value_at(text, at)
            }
        }
        DecodedShape::SubTable(sub) => {
            if text.as_bytes().get(at) == Some(&b'[') {
                decode_subtable_array(sub, text, at)
            } else {
                value_at(text, at)
            }
        }
    }
}

fn assemble_object(
    schema: &DecodedSchema,
    group: Option<&Option<String>>,
    cells: Vec<Option<String>>,
) -> Option<String> {
    let mut cells = cells.into_iter();
    let mut object = String::from("{");
    for field in &schema.fields {
        let value = match &field.mode {
            DecodedMode::Constant(value) => Some(value.clone()),
            DecodedMode::Group => group?.clone(),
            DecodedMode::Varying { .. } => cells.next()?,
        };
        if let Some(value) = value {
            if object.len() > 1 {
                object.push(',');
            }
            object.push_str(&field.key);
            object.push(':');
            object.push_str(&value);
        }
    }
    object.push('}');
    Some(object)
}

fn decode_tuple(schema: &DecodedSchema, text: &str, mut at: usize) -> Option<(String, usize)> {
    (text.as_bytes().get(at) == Some(&b'(')).then_some(())?;
    at += 1;
    let varying: Vec<&DecodedField> = schema
        .fields
        .iter()
        .filter(|field| matches!(field.mode, DecodedMode::Varying { .. }))
        .collect();
    let mut cells = Vec::with_capacity(varying.len());
    if text.as_bytes().get(at) == Some(&b')') {
        at += 1;
    } else {
        loop {
            let field = *varying.get(cells.len())?;
            let DecodedMode::Varying { default } = &field.mode else {
                unreachable!()
            };
            match text.as_bytes().get(at).copied()? {
                b',' | b')' => cells.push(default.clone()),
                b'~' => {
                    default.as_ref()?;
                    at += 1;
                    cells.push(None);
                }
                _ => {
                    let (value, next) = decode_present_value(&field.shape, text, at)?;
                    at = next;
                    cells.push(Some(value));
                }
            }
            match text.as_bytes().get(at).copied()? {
                b',' => at += 1,
                b')' => {
                    at += 1;
                    break;
                }
                _ => return None,
            }
        }
    }
    for field in &varying[cells.len()..] {
        let DecodedMode::Varying { default } = &field.mode else {
            unreachable!()
        };
        cells.push(default.clone());
    }
    let object = assemble_object(schema, None, cells)?;
    Some((object, at))
}

fn decode_subtable_array(
    schema: &DecodedSchema,
    text: &str,
    mut at: usize,
) -> Option<(String, usize)> {
    (text.as_bytes().get(at) == Some(&b'[')).then_some(())?;
    at += 1;
    let mut out = String::from("[");
    if text.as_bytes().get(at) == Some(&b']') {
        out.push(']');
        return Some((out, at + 1));
    }
    loop {
        if out.len() > 1 {
            out.push(',');
        }
        let (item, next) = if text.as_bytes().get(at) == Some(&b'(') {
            decode_tuple(schema, text, at)?
        } else {
            value_at(text, at)?
        };
        out.push_str(&item);
        at = next;
        match text.as_bytes().get(at).copied()? {
            b',' => at += 1,
            b']' => {
                out.push(']');
                return Some((out, at + 1));
            }
            _ => return None,
        }
    }
}

fn decode_table(header: &str, lines: &mut Lines, indent: usize) -> Option<Vec<String>> {
    let (count, rest) = table_header(header)?;
    let (schema, end) = parse_schema(rest, 0, None, true, 0)?;
    if end != rest.len() {
        return None;
    }
    let group_field = schema
        .fields
        .iter()
        .find(|field| matches!(field.mode, DecodedMode::Group));
    let varying: Vec<&DecodedField> = schema
        .fields
        .iter()
        .filter(|field| matches!(field.mode, DecodedMode::Varying { .. }))
        .collect();

    let mut group: Option<Option<String>> = None;
    let mut rows = Vec::with_capacity(count);
    while rows.len() < count {
        let line = lines.next()?;
        let line = line.get(indent..).filter(|_| indent_of(line) >= indent)?;
        if let Some(rest) = line.strip_prefix('@') {
            let gfield = group_field?;
            group = Some(if rest.is_empty() {
                None
            } else {
                let cell = rest.strip_prefix(' ')?;
                let (value, end) = decode_present_value(&gfield.shape, cell, 0)?;
                (end == cell.len()).then_some(())?;
                Some(value)
            });
            continue;
        }
        if let Some(rest) = line.strip_prefix('!') {
            if !rest.starts_with('{') {
                return None;
            }
            let (value, end) = value_at(rest, 0)?;
            if end != rest.len() {
                return None;
            }
            rows.push(value);
            continue;
        }
        let mut cells = Vec::with_capacity(varying.len());
        let mut at = 0;
        for (index, field) in varying.iter().enumerate() {
            if index > 0 {
                (line.as_bytes().get(at) == Some(&b',')).then_some(())?;
                at += 1;
            }
            let DecodedMode::Varying { default } = &field.mode else {
                unreachable!()
            };
            if at == line.len() || line.as_bytes()[at] == b',' {
                cells.push(default.clone());
            } else if line.as_bytes()[at] == b'~' {
                default.as_ref()?;
                at += 1;
                cells.push(None);
            } else {
                let (value, end) = decode_present_value(&field.shape, line, at)?;
                cells.push(Some(value));
                at = end;
            }
        }
        if at != line.len() {
            return None;
        }
        rows.push(assemble_object(&schema, group.as_ref(), cells)?);
    }
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn padded(text: &str) -> String {
        // Views apply only to output of at least MIN_BYTES.
        format!("{text}{}", " ".repeat(MIN_BYTES))
    }

    /// The smallest view.
    fn render(text: &str, group: bool) -> Option<String> {
        views(text, group).into_iter().next()
    }

    /// The table view alone, which `render` gives unless the compact JSON is smaller.
    fn tables(text: &str, group: bool) -> Option<String> {
        Json::read(text)?
            .tables(group)
            .filter(|view| view.len() < text.len())
    }

    /// The legacy flat table view (without deep-schema upgrades), for savings comparisons.
    fn flat_tables(text: &str, group: bool) -> Option<String> {
        Json::read(text)?
            .render_tables(group, false)
            .map(|(view, _, _)| view)
            .filter(|view| view.len() < text.len())
    }

    fn assert_lossless(text: &str, group: bool) -> String {
        let original = Json::read(text).unwrap().original;
        let view = tables(text, group).unwrap_or_else(|| panic!("no table for {text}"));
        assert_eq!(
            decode(&view).unwrap(),
            original,
            "round-trip failed:\n{view}"
        );
        let best = render(text, group).unwrap();
        assert!(best.len() <= view.len());
        assert_eq!(decode(&best).unwrap(), original);
        view
    }

    #[test]
    fn views_round_trip_exact_values() {
        let cases = [
            // Constant column lifted, "1" vs 1 vs 1.0, null vs absent, escapes, nesting.
            r#"[{"name":"a","kind":"lib","n":1,"tag":"1","x":null},
                {"name":"b","kind":"lib","n":1.0,"tag":"café","x":{"k":[1,2]}},
                {"name":"c,d","kind":"lib","tag":"=,\"","y":true}]"#,
            // JSON lines with an odd key and a missing first field.
            "{\"a b\":1,\"c\":2}\n{\"c\":3}\n{\"a b\":4,\"c\":5}\n",
            // A wrapper object: the array becomes a table, other members stay inline.
            r#"{"apiVersion":"v1","items":[{"metadata":{"name":"p1"},"status":"Running"},
                {"metadata":{"name":"p2"},"status":"Running"},{"metadata":{"name":"p3"},"status":"Failed"}],
                "kind":"List","nested":{"rows":[{"i":1},{"i":2},{"i":3}],"z":0}}"#,
        ];
        for case in cases {
            for group in [false, true] {
                let text = padded(case);
                let original = Json::read(&text).unwrap().original;
                let table = tables(&text, group).unwrap_or_else(|| panic!("no table for {case}"));
                let view = render(&text, group).unwrap();
                assert!(view.len() <= table.len());
                for view in [table, view] {
                    assert_eq!(decode(&view).unwrap(), original, "{view}");
                }
            }
        }
        let view = tables(&padded(cases[0]), false).unwrap();
        assert!(
            view.contains("3 objects; fields: name,kind=\"lib\",n,tag,x,y\n"),
            "{view}"
        );
        assert!(view.contains("\"c,d\",,\"=,\\\"\",,true\n"), "{view}");
    }

    #[test]
    fn irregular_or_small_json_has_no_table() {
        for text in [
            // Keys in conflicting orders.
            padded(r#"[{"a":1,"b":2},{"b":3,"a":4}]"#),
            // A duplicate key.
            padded(r#"[{"a":1,"a":2},{"a":3,"a":4}]"#),
            // Not all objects.
            padded(r#"[{"a":1},2]"#),
            // Every field shared: nothing varies.
            padded(r#"[{"a":1},{"a":1}]"#),
        ] {
            assert_eq!(tables(&text, false), None, "{text}");
            // Only the whitespace goes.
            let compact = text.trim_end();
            assert_eq!(
                render(&text, false),
                Some(format!("{COMPACT}\n{compact}\n")),
                "{text}"
            );
        }
        // Not JSON at all, or too small to help.
        for text in [
            padded("{\"a\":1}\nnot json\n"),
            r#"[{"a":1},{"a":2}]"#.to_owned(),
        ] {
            assert_eq!(render(&text, false), None, "{text}");
        }
    }

    #[test]
    fn pretty_json_without_tables_is_shown_compact() {
        // A dependency tree, as `npm ls --json` prints it: nested objects, no arrays
        // of objects. Scalars keep their source text, escapes and key order.
        let tree = |name: &str, deps: &str| {
            format!(
                "{{\n  \"version\": \"1.0.0\",\n  \"name\": \"{name}\",\n  \"dependencies\": {{{deps}\n  }}\n}}\n"
            )
        };
        let deps: String = (0..8)
            .map(|i| format!("\n    \"pkg-{i}\": {{\n      \"version\": \"{i}.0\",\n      \"n\": 1.0,\n      \"path\": \"a\\/b\\u00e9\"\n    }},"))
            .collect();
        let text = tree("app", deps.trim_end_matches(','));
        let view = render(&text, false).unwrap();
        let body = view.strip_prefix(&format!("{COMPACT}\n")).unwrap();
        assert!(
            body.starts_with(r#"{"version":"1.0.0","name":"app","dependencies":{"pkg-0":{"version":"0.0","n":1.0,"path":"a\/b\u00e9"},"#),
            "{view}"
        );
        assert_eq!(body.lines().count(), 1);
        assert_eq!(decode(&view).unwrap(), Json::read(&text).unwrap().original);
        assert!(
            view.len() * 4 < text.len() * 3,
            "{} of {}",
            view.len(),
            text.len()
        );
        // JSON lines keep one record per line.
        let lines = "{\"a\": 1, \"b\": [1, 2]}\n{\"b\": [3], \"a\": 2}\n".repeat(16);
        let view = render(&lines, false).unwrap();
        assert_eq!(
            view,
            format!(
                "{COMPACT}\n{}",
                "{\"a\":1,\"b\":[1,2]}\n{\"b\":[3],\"a\":2}\n".repeat(16)
            )
        );
        // Already compact JSON has nothing to drop.
        assert_eq!(render(&format!("{}\n", body.trim_end()), false), None);
    }

    #[test]
    fn grouping_names_a_shared_value_once() {
        let rows: Vec<String> = (0..12)
            .map(|i| {
                format!(
                    r#"{{"package":"pkg-{}","kind":"lib","name":"target-{i}"}}"#,
                    i / 4
                )
            })
            .collect();
        let text = format!("[{}]", rows.join(","));
        let view = render(&text, true).unwrap();
        assert!(
            view.contains("fields: @package,kind=\"lib\",name\n@ \"pkg-0\"\n\"target-0\"\n"),
            "{view}"
        );
        assert!(render(&text, false).unwrap().len() > view.len());
    }

    #[test]
    fn decoding_rejects_a_damaged_view() {
        let text = padded(r#"[{"a":1,"b":"x"},{"a":2,"b":"y"},{"a":3}]"#);
        let view = tables(&text, false).unwrap();
        let compact = Json::read(&text).unwrap().compact().unwrap();
        for view in [&view, &compact] {
            assert!(decode(view).is_some());
        }
        for damaged in [
            view.replacen("3 objects", "4 objects", 1),
            view.replacen("2,\"y\"", "2,\"y\",", 1),
            format!("{view}extra\n"),
            compact.replacen("[{", "[ {", 1),
            compact.replacen("]\n", "]x\n", 1),
            format!("{COMPACT}\n"),
        ] {
            assert_eq!(decode(&damaged), None, "{damaged}");
        }
    }

    #[test]
    fn fields_that_cannot_make_a_table_build_no_cells() {
        fn objects(lines: &[String]) -> Vec<Node<'_>> {
            lines.iter().map(|line| parse(line).unwrap()).collect()
        }
        // One key per object, as with 600 distinct keys.
        let distinct: Vec<String> = (0..600).map(|i| format!("{{\"key{i}\":{i}}}")).collect();
        // Five fields, no object with more than two.
        let sparse: Vec<String> = [r#"{"a":1,"b":2}"#, r#"{"c":3}"#, r#"{"d":4,"e":5}"#]
            .map(str::to_owned)
            .to_vec();
        // A wide object beside many narrow ones: every narrow row would be mostly
        // empty commas, more than the JSON's bytes.
        let wide = format!(
            "{{{}}}",
            (0..100)
                .map(|i| format!("\"k{i}\":{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let narrow: Vec<String> = std::iter::once(wide)
            .chain((0..100).map(|i| format!("{{\"k{}\":0}}", i % 100)))
            .collect();
        for lines in [&distinct, &sparse, &narrow] {
            assert!(columns(&objects(lines)).is_none(), "{lines:?}");
        }
        // Optional fields still make a table.
        let optional: Vec<String> = (0..4)
            .map(|i| {
                format!(
                    "{{\"a\":{i},\"b\":{i}{}}}",
                    if i % 2 == 0 { ",\"c\":1" } else { "" }
                )
            })
            .collect();
        assert_eq!(
            columns(&objects(&optional)).unwrap().keys,
            ["\"a\"", "\"b\"", "\"c\""]
        );
    }

    #[test]
    fn gh_api_issues_deep_schema_saves_over_40_percent_vs_flat() {
        let issues: Vec<String> = (0..40)
            .map(|i| {
                let state = if i < 34 { "open" } else { "closed" };
                let login = ["alice", "bob", "carol", "dave"][i % 4];
                let uid = 1000 + (i % 4);
                let assignee = if i % 4 == 0 {
                    let alogin = ["alice", "bob", "carol", "dave"][(i / 4) % 4];
                    let auid = 1000 + ((i / 4) % 4);
                    format!(
                        r#"{{"login":"{alogin}","id":{auid},"type":"User","site_admin":false}}"#
                    )
                } else {
                    String::from("null")
                };
                let labels = if i % 3 == 0 {
                    format!(
                        r#"[{{"id":{},"name":"bug","color":"d73a4a","default":true,"description":"Something isn't working"}}]"#,
                        200 + (i % 2)
                    )
                } else {
                    String::from("[]")
                };
                let plus_one = if i % 8 == 0 { 3 } else { 0 };
                let total = plus_one;
                format!(
                    r#"{{"number":{},"state":"{state}","title":"Fix issue #{i}","locked":false,"user":{{"login":"{login}","id":{uid},"type":"User","site_admin":false}},"assignee":{assignee},"labels":{labels},"reactions":{{"total_count":{total},"+1":{plus_one},"-1":0,"laugh":0,"hooray":0,"confused":0,"heart":0,"rocket":0,"eyes":0}}}}"#,
                    100 + i
                )
            })
            .collect();
        let text = format!("[{}]", issues.join(","));
        let flat = flat_tables(&text, false).unwrap();
        let deep = assert_lossless(&text, false);
        let deep_grouped = assert_lossless(&text, true);
        assert!(
            deep.contains("user{login,id,type=\"User\",site_admin=false}"),
            "{deep}"
        );
        assert!(deep.contains("state?=\"open\""), "{deep}");
        assert!(
            deep.contains("assignee{login,id,type=\"User\",site_admin=false}?=null"),
            "{deep}"
        );
        assert!(deep.contains("labels:[id,name=\"bug\",color=\"d73a4a\",default=true,description=\"Something isn't working\"]?=[]"), "{deep}");
        assert!(
            deep.contains("reactions{total_count?=0,\"+1\"?=0,\"-1\"=0"),
            "{deep}"
        );
        // Deep schema saves >= 40% over the flat table AND over compact JSON.
        println!(
            "gh_api_issues: orig={} flat={} deep={} (vs_flat=-{:.1}%, vs_orig=-{:.1}%)",
            text.len(),
            flat.len(),
            deep.len(),
            100.0 * (1.0 - deep.len() as f64 / flat.len() as f64),
            100.0 * (1.0 - deep.len() as f64 / text.len() as f64),
        );
        assert!(
            deep.len() * 100 <= flat.len() * 60,
            "expected >=40% savings vs flat: deep={} flat={} orig={}\n{deep}",
            deep.len(),
            flat.len(),
            text.len()
        );
        assert!(deep_grouped.len() <= deep.len());
    }

    #[test]
    fn kubectl_pods_nested_objects_and_subtables_save_over_40_percent() {
        let pods: Vec<String> = (0..15)
            .map(|i| {
                let phase = if i < 13 { "Running" } else { "Pending" };
                let ready = if i < 13 { "True" } else { "False" };
                let app = ["api", "worker", "web"][i % 3];
                let port = 8080 + (i % 3);
                format!(
                    r#"{{"apiVersion":"v1","kind":"Pod","metadata":{{"name":"{app}-{i}","namespace":"prod","uid":"uid-{i}","labels":{{"app":"{app}","tier":"backend","env":"production"}}}},"spec":{{"restartPolicy":"Always","serviceAccountName":"default","nodeName":"node-{}","containers":[{{"name":"{app}","image":"registry.internal/{app}:1.4.2","imagePullPolicy":"IfNotPresent","ports":[{{"containerPort":{port},"protocol":"TCP"}}]}},{{"name":"sidecar","image":"registry.internal/envoy:1.29","imagePullPolicy":"IfNotPresent","ports":[{{"containerPort":15001,"protocol":"TCP"}}]}}]}},"status":{{"phase":"{phase}","podIP":"10.0.1.{i}","conditions":[{{"type":"Initialized","status":"True"}},{{"type":"Ready","status":"{ready}"}},{{"type":"ContainersReady","status":"{ready}"}},{{"type":"PodScheduled","status":"True"}}]}}}}"#,
                    i % 3
                )
            })
            .collect();
        let text = format!(
            r#"{{"apiVersion":"v1","kind":"List","items":[{}]}}"#,
            pods.join(",")
        );
        let flat = flat_tables(&text, false).unwrap();
        let deep = assert_lossless(&text, false);
        let _grouped = assert_lossless(&text, true);
        assert!(deep.contains("metadata{name,namespace=\"prod\",uid,labels{app,tier=\"backend\",env=\"production\"}}"), "{deep}");
        assert!(
            deep.contains("containers:[name?=\"sidecar\",image?=\"registry.internal/envoy:1.29\",imagePullPolicy=\"IfNotPresent\",ports:[containerPort?=15001,protocol=\"TCP\"]?=[()]]"),
            "{deep}"
        );
        assert!(
            deep.contains("conditions:[type,status?=\"True\"]?=[(\"Initialized\"),(\"Ready\"),(\"ContainersReady\"),(\"PodScheduled\")]"),
            "{deep}"
        );
        println!(
            "kubectl_pods: orig={} flat={} deep={} (vs_flat=-{:.1}%, vs_orig=-{:.1}%)",
            text.len(),
            flat.len(),
            deep.len(),
            100.0 * (1.0 - deep.len() as f64 / flat.len() as f64),
            100.0 * (1.0 - deep.len() as f64 / text.len() as f64),
        );
        assert!(
            deep.len() * 100 <= flat.len() * 60,
            "expected >=40% savings vs flat: deep={} flat={} orig={}\n{deep}",
            deep.len(),
            flat.len(),
            text.len()
        );
    }

    #[test]
    fn cargo_metadata_packages_with_dependencies_subtable_saves_over_40_percent() {
        let crates_io = "registry+https://github.com/rust-lang/crates.io-index";
        let packages: Vec<String> = (0..25)
            .map(|i| {
                let edition = if i == 24 { "2018" } else { "2021" };
                let deps = if i == 0 {
                    String::from("[]")
                } else {
                    let mut d = vec![
                        format!(r#"{{"name":"serde","source":"{crates_io}","req":"^1.0","kind":null,"rename":null,"optional":false,"uses_default_features":true,"features":[],"target":null}}"#),
                        format!(r#"{{"name":"anyhow","source":"{crates_io}","req":"^1.0","kind":null,"rename":null,"optional":false,"uses_default_features":true,"features":[],"target":null}}"#),
                    ];
                    if i % 3 == 0 {
                        d.push(format!(r#"{{"name":"insta","source":"{crates_io}","req":"^1.38","kind":"dev","rename":null,"optional":false,"uses_default_features":true,"features":["json"],"target":null}}"#));
                    }
                    if i % 5 == 0 {
                        d.push(r#"{"name":"local-core","source":null,"req":"^0.1","kind":null,"rename":null,"optional":true,"uses_default_features":false,"features":[],"target":"cfg(unix)"}"#.to_owned());
                    }
                    format!("[{}]", d.join(","))
                };
                format!(
                    r#"{{"name":"crate-{i}","version":"0.{i}.0","id":"crate-{i} 0.{i}.0 (registry)","license":"MIT OR Apache-2.0","edition":"{edition}","dependencies":{deps},"targets":[{{"kind":["lib"],"crate_types":["lib"],"name":"crate_{i}","src_path":"/src/crate-{i}/src/lib.rs","edition":"{edition}","doc":true,"doctest":true,"test":true}}],"publish":null}}"#
                )
            })
            .collect();
        let text = format!(
            r#"{{"packages":[{}],"workspace_members":["crate-0 0.0.0 (registry)"],"version":1}}"#,
            packages.join(",")
        );
        let flat = flat_tables(&text, false).unwrap();
        let deep = assert_lossless(&text, false);
        let _grouped = assert_lossless(&text, true);
        assert!(
            deep.contains(&format!("dependencies:[name,source?=\"{crates_io}\"")),
            "{deep}"
        );
        assert!(deep.contains("kind?=null"), "{deep}");
        assert!(deep.contains("optional?=false"), "{deep}");
        assert!(deep.contains("uses_default_features?=true"), "{deep}");
        assert!(deep.contains("targets:[kind=[\"lib\"]"), "{deep}");
        println!(
            "cargo_metadata: orig={} flat={} deep={} (vs_flat=-{:.1}%, vs_orig=-{:.1}%)",
            text.len(),
            flat.len(),
            deep.len(),
            100.0 * (1.0 - deep.len() as f64 / flat.len() as f64),
            100.0 * (1.0 - deep.len() as f64 / text.len() as f64),
        );
        assert!(
            deep.len() * 100 <= flat.len() * 60,
            "expected >=40% savings vs flat: deep={} flat={} orig={}\n{deep}",
            deep.len(),
            flat.len(),
            text.len()
        );
    }

    #[test]
    fn outlier_rows_escape_reordered_and_duplicate_keys() {
        // Row 0 is reordered, row 7 has a duplicate key, rows 1..6 & 8..19 conform (18/20 = 90% >= 85%).
        let mut rows: Vec<String> = Vec::new();
        rows.push(r#"{"status":"open","id":0,"name":"item-0","kind":"widget"}"#.to_owned());
        for i in 1..20 {
            if i == 7 {
                rows.push(
                    r#"{"id":7,"id":70,"name":"dup-7","kind":"widget","status":"open"}"#.to_owned(),
                );
            } else {
                let status = if i < 15 { "open" } else { "closed" };
                rows.push(format!(
                    r#"{{"id":{i},"name":"item-{i}","kind":"widget","status":"{status}"}}"#
                ));
            }
        }
        let text = format!("[{}]", rows.join(","));
        // Flat tables reject this because of the reordered & duplicate rows.
        assert!(flat_tables(&text, false).is_none());
        let deep = assert_lossless(&text, false);
        let deep_grouped = assert_lossless(&text, true);
        assert!(deep.contains("!JSON verbatim"), "{deep}");
        assert!(
            deep.contains(
                "!{\"status\":\"open\",\"id\":0,\"name\":\"item-0\",\"kind\":\"widget\"}\n"
            ),
            "{deep}"
        );
        assert!(
            deep.contains(
                "!{\"id\":7,\"id\":70,\"name\":\"dup-7\",\"kind\":\"widget\",\"status\":\"open\"}\n"
            ),
            "{deep}"
        );
        assert!(deep.len() < text.len());
        assert!(deep_grouped.len() < text.len());

        // Below 85% conforming (e.g. 8/10 = 80%) must NOT make a table.
        let mut below: Vec<String> = (0..8)
            .map(|i| format!(r#"{{"id":{i},"name":"item-{i}","kind":"widget"}}"#))
            .collect();
        below.push(r#"{"name":"bad-8","id":8,"kind":"widget"}"#.to_owned());
        below.push(r#"{"name":"bad-9","id":9,"kind":"widget"}"#.to_owned());
        let below_text = padded(&format!("[{}]", below.join(",")));
        assert!(tables(&below_text, false).is_none());
    }

    #[test]
    fn outlier_nested_objects_and_subtable_elements_round_trip() {
        // Nested object has 1 reordered outlier out of 10 (90% conforming), and sub-table
        // has 1 duplicate-key element out of 12 (91.7% conforming).
        let rows: Vec<String> = (0..10)
            .map(|i| {
                let meta = if i == 4 {
                    r#"{"env":"prod","name":"reordered-4","owner":"team-a"}"#.to_owned()
                } else {
                    format!(r#"{{"name":"svc-{i}","env":"prod","owner":"team-a"}}"#)
                };
                let items = if i == 6 {
                    r#"[{"k":"x","k":"dup","v":1},{"k":"y","v":2}]"#.to_owned()
                } else {
                    format!(r#"[{{"k":"a-{i}","v":1}},{{"k":"b-{i}","v":2}}]"#)
                };
                format!(r#"{{"id":{i},"meta":{meta},"items":{items}}}"#)
            })
            .collect();
        let text = format!("[{}]", rows.join(","));
        let deep = assert_lossless(&text, false);
        assert!(
            deep.contains("meta{name,env=\"prod\",owner=\"team-a\"}"),
            "{deep}"
        );
        assert!(deep.contains("items:[k,v?=2]"), "{deep}");
        assert!(
            deep.contains(r#"{"env":"prod","name":"reordered-4","owner":"team-a"}"#),
            "{deep}"
        );
        assert!(
            deep.contains(r#"[{"k":"x","k":"dup","v":1},("y")]"#),
            "{deep}"
        );
    }

    #[test]
    fn edge_cases_null_absent_default_special_strings_numbers_and_depth_limit() {
        // Test:
        // - null vs absent (~) vs majority default ("open")
        // - nested absent parent vs null parent vs empty object {} vs partial object
        // - empty array [] vs absent (~) vs null in sub-table with ?=[]
        // - strings containing ~, !, !{}, (a,b), commas, quotes, backslashes, \u00e9 vs é, \/
        // - 1.0 vs 1 vs 1e0 vs "1"
        // - depth > 3 gracefully stays compact JSON at depth 4
        let mut rows: Vec<String> = Vec::new();
        for i in 0..12 {
            let state = match i {
                0 => None,              // absent -> ~
                1 => Some("null"),      // literal null
                2 => Some("\"~\""),     // string "~"
                3 => Some("\"!{}\""),   // string "!{}"
                4 => Some("\"(a,b)\""), // string "(a,b)"
                _ => Some("\"open\""),  // majority default (7/12 = 58.3%)
            };
            let num = match i {
                0 => "1",
                1 => "1.0",
                2 => "1e0",
                3 => "\"1\"",
                _ => "1",
            };
            let user = match i {
                0 => None,                                // absent parent
                1 => Some("null".to_owned()),             // null parent
                2 => Some("{}".to_owned()),               // empty present parent
                3 => Some(r#"{"login":"~"}"#.to_owned()), // nested default absent + string "~"
                4 => Some(r#"{"login":"a\/b\u00e9","role":"admin"}"#.to_owned()),
                5 => Some(r#"{"login":"café, \"x\"","role":"user"}"#.to_owned()),
                _ => Some(format!(r#"{{"login":"u{i}","role":"user"}}"#)),
            };
            let tags = match i {
                0 => None,                    // absent sub-table -> ~
                1 => Some("null".to_owned()), // null sub-table
                2 => Some(r#"[{"key":"~","val":"!x"},{"key":"a,b"}]"#.to_owned()),
                3 => Some(r#"[{"key":"k","val":"default"},{"key":"k2","val":null}]"#.to_owned()),
                _ => Some("[]".to_owned()), // majority default ?=[]
            };
            // Depth 4 nesting: d1 -> d2 -> d3 -> d4 (d4 stays compact JSON).
            let deep_nest = format!(
                r#"{{"d2":{{"d3":{{"d4":{{"leaf":{i},"fixed":"ok"}},"tag":"t"}},"fixed2":"y"}},"fixed1":"x"}}"#
            );
            let mut parts = vec![format!(r#""id":{i}"#), format!(r#""num":{num}"#)];
            if let Some(s) = state {
                parts.push(format!(r#""state":{s}"#));
            }
            if let Some(u) = user {
                parts.push(format!(r#""user":{u}"#));
            }
            if let Some(t) = tags {
                parts.push(format!(r#""tags":{t}"#));
            }
            parts.push(format!(r#""a?b":{i}"#));
            parts.push(format!(r#""d1":{deep_nest}"#));
            rows.push(format!("{{{}}}", parts.join(",")));
        }
        let text = format!("[{}]", rows.join(","));
        let deep = assert_lossless(&text, false);
        let _grouped = assert_lossless(&text, true);
        assert!(deep.contains("state?=\"open\""), "{deep}");
        assert!(deep.contains("user{login,role?=\"user\"}"), "{deep}");
        assert!(deep.contains("tags:[key,val]?=[]"), "{deep}");
        // Special key "a?b" must stay quoted in header.
        assert!(deep.contains("\"a?b\""), "{deep}");
        // Absent parent, null parent, empty object {}, and ~ absent sentinel all present & distinct.
        assert!(deep.contains(",~,"), "{deep}");
        assert!(deep.contains("(,~)"), "{deep}");
        assert!(deep.contains("\"~\""), "{deep}");
        assert!(deep.contains("\"!{}\""), "{deep}");
        assert!(deep.contains("\"(a,b)\""), "{deep}");
        assert!(deep.contains("a\\/b\\u00e9"), "{deep}");
        assert!(deep.contains(",1.0,"), "{deep}");
        assert!(deep.contains(",1e0,"), "{deep}");
        assert!(deep.contains(",\"1\","), "{deep}");
        // Depth 3 unpacks d1{d2{d3{d4,tag="t"},fixed2="y"},fixed1="x"} while d4 stays compact.
        assert!(
            deep.contains("d1{d2{d3{d4,tag=\"t\"},fixed2=\"y\"},fixed1=\"x\"}"),
            "{deep}"
        );
    }

    #[test]
    fn decoding_rejects_damaged_deep_schema_views() {
        let base_legend = Root::Array.legend(LegendFlags {
            nested: true,
            subtable: true,
            default: true,
            outlier: true,
        });
        let valid = format!(
            "{base_legend}3 objects; fields: id,state?=\"open\",u{{a,b?=0}},t:[x,y?=1]?=[]\n\
             1,,(\"hi\"),[(\"p\")]\n\
             2,~,(,~),~\n\
             !{{\"id\":3,\"extra\":true}}\n"
        );
        assert_eq!(
            decode(&valid).unwrap(),
            r#"[{"id":1,"state":"open","u":{"a":"hi","b":0},"t":[{"x":"p","y":1}]},{"id":2,"u":{}},{"id":3,"extra":true}]"#
        );

        let damaged_cases = [
            // `~` in a column without `?=DEFAULT`.
            valid.replacen("1,,", "~,,", 1),
            // Trailing junk after `~`.
            valid.replacen("2,~,", "2,~x,", 1),
            // Too many cells inside nested tuple.
            valid.replacen("(\"hi\")", "(\"hi\",1,2)", 1),
            // Unclosed nested tuple.
            valid.replacen("(\"hi\")", "(\"hi\"", 1),
            // Non-object verbatim `!` row.
            valid.replacen("!{\"id\":3,\"extra\":true}", "![1,2,3]", 1),
            // Empty nested object schema `u{}`.
            valid.replacen("u{a,b?=0}", "u{}", 1),
            // Empty sub-table schema `t:[]`.
            valid.replacen("t:[x,y?=1]", "t:[]", 1),
            // Corrupted legend clause.
            valid.replacen(", !JSON verbatim", ", !bogus", 1),
            // Duplicate `@` group marker.
            format!("{base_legend}1 objects; fields: @a,@b\n@ 1\n"),
            // Stray `@` line when no field is grouped.
            valid.replacen("1,,", "@ 1\n1,,", 1),
        ];
        for damaged in damaged_cases {
            assert_eq!(decode(&damaged), None, "expected None for:\n{damaged}");
        }
    }

    #[test]
    fn majority_default_requires_strict_mode_and_net_savings() {
        // 50/50 tie (4 "a", 4 "b"): must NOT emit ?=.
        let tied: Vec<String> = (0..8)
            .map(|i| {
                format!(
                    r#"{{"id":{i},"flag":"{}"}}"#,
                    if i < 4 { "alpha-long" } else { "bravo-long" }
                )
            })
            .collect();
        let tied_text = padded(&format!("[{}]", tied.join(",")));
        let view = assert_lossless(&tied_text, false);
        assert!(!view.contains("?="), "{view}");

        // 50% unique mode (4 "alpha-long", 2 "bravo-long", 2 "charlie-long"): SHOULD emit ?=.
        let modal: Vec<String> = (0..8)
            .map(|i| {
                let flag = if i < 4 {
                    "alpha-long"
                } else if i < 6 {
                    "bravo-long"
                } else {
                    "charlie-long"
                };
                format!(r#"{{"id":{i},"flag":"{flag}"}}"#)
            })
            .collect();
        let modal_text = padded(&format!("[{}]", modal.join(",")));
        let view = assert_lossless(&modal_text, false);
        assert!(view.contains("flag?=\"alpha-long\""), "{view}");
    }

    #[test]
    fn gh_pr_list_json_deep_schema_round_trips_and_saves_over_40_percent() {
        let prs: Vec<String> = (0..30)
            .map(|i| {
                let author = ["alice", "bob", "carol"][i % 3];
                let draft = if i % 6 == 0 { "true" } else { "false" };
                let state = if i < 25 { "OPEN" } else { "MERGED" };
                let review = if i < 20 { "APPROVED" } else { "REVIEW_REQUIRED" };
                let checks = format!(
                    r#"[{{"__typename":"CheckRun","name":"ci/lint","status":"COMPLETED","conclusion":"SUCCESS"}},{{"__typename":"CheckRun","name":"ci/test","status":"COMPLETED","conclusion":"{}"}}]"#,
                    if i % 7 == 0 { "FAILURE" } else { "SUCCESS" }
                );
                let labels = if i % 4 == 0 {
                    r#"[{"id":"LA_1","name":"enhancement","color":"a2eeef"}]"#
                } else {
                    "[]"
                };
                format!(
                    r#"{{"number":{},"title":"PR #{i}: refine parser","state":"{state}","isDraft":{draft},"reviewDecision":"{review}","author":{{"login":"{author}","is_bot":false,"name":"{author} dev"}},"labels":{labels},"statusCheckRollup":{checks}}}"#,
                    500 + i
                )
            })
            .collect();
        let text = format!("[{}]", prs.join(","));
        let flat = flat_tables(&text, false).unwrap();
        let deep = assert_lossless(&text, false);
        let _grouped = assert_lossless(&text, true);
        assert!(deep.contains("author{login,is_bot=false,name}"), "{deep}");
        assert!(deep.contains("state?=\"OPEN\""), "{deep}");
        assert!(deep.contains("isDraft?=false"), "{deep}");
        assert!(deep.contains("statusCheckRollup:[__typename=\"CheckRun\",name,status=\"COMPLETED\",conclusion?=\"SUCCESS\"]"), "{deep}");
        println!(
            "gh_pr_list: orig={} flat={} deep={} (vs_flat=-{:.1}%, vs_orig=-{:.1}%)",
            text.len(),
            flat.len(),
            deep.len(),
            100.0 * (1.0 - deep.len() as f64 / flat.len() as f64),
            100.0 * (1.0 - deep.len() as f64 / text.len() as f64),
        );
        assert!(
            deep.len() * 100 <= flat.len() * 60,
            "expected >=40% savings vs flat: deep={} flat={} orig={}\n{deep}",
            deep.len(),
            flat.len(),
            text.len()
        );
    }

    #[test]
    fn combinatorial_round_trip_property_across_roots_and_features() {
        // Deterministic pseudo-random generator exercising combinations of:
        // Root::Array, Root::Lines, Root::Object; group = false/true;
        // nested objects (present/absent/null/empty/outlier), sub-tables (0..3 items/null/absent),
        // majority defaults, duplicate keys, special strings, and exact numeric forms.
        let mut seed: u64 = 0x5eed_cafe_beef_1234;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            seed >> 33
        };
        let strings = [
            "\"ok\"",
            "\"~\"",
            "\"!{}\"",
            "\"(a,b)\"",
            "\"x,y=z?w\"",
            "\"a\\/b\\u00e9\"",
            "\"café\"",
            "\"\"",
        ];
        let nums = ["0", "1", "1.0", "1e0", "-0", "\"1\""];

        for trial in 0..24 {
            let n = 10 + (trial % 15);
            let mut records = Vec::with_capacity(n);
            for i in 0..n {
                // Inject 1 outlier (reordered or duplicate key) in some trials (~7-10% <= 15%).
                if trial % 3 == 0 && i == trial % n {
                    if trial % 2 == 0 {
                        records.push(format!(
                            r#"{{"status":"err","id":{i},"meta":{{"env":"prod","k":"v{i}"}}}}"#
                        ));
                    } else {
                        records.push(format!(
                            r#"{{"id":{i},"id":{},"status":"dup","meta":{{"k":"v{i}","env":"prod"}}}}"#,
                            i + 100
                        ));
                    }
                    continue;
                }
                let mut fields = vec![format!(r#""id":{i}"#)];
                let r = next() % 10;
                if r < 7 {
                    fields.push(r#""status":"ready""#.to_owned());
                } else if r == 7 {
                    fields.push(format!(
                        r#""status":{}"#,
                        strings[(next() as usize) % strings.len()]
                    ));
                } else if r == 8 {
                    fields.push(r#""status":null"#.to_owned());
                }
                // r == 9: status absent

                let nval = if next() % 10 < 7 {
                    "1"
                } else {
                    nums[(next() as usize) % nums.len()]
                };
                fields.push(format!(r#""n":{nval}"#));

                // Nested object `meta`: present, empty {}, null, absent, or nested outlier
                match next() % 12 {
                    0 => {} // absent
                    1 => fields.push(r#""meta":null"#.to_owned()),
                    2 => fields.push(r#""meta":{}"#.to_owned()),
                    3 => fields.push(format!(r#""meta":{{"k":"v{i}"}}"#)),
                    _ => {
                        let env = if next() % 5 < 4 {
                            "\"prod\""
                        } else {
                            "\"dev\""
                        };
                        let tag = strings[(next() as usize) % strings.len()];
                        fields.push(format!(r#""meta":{{"k":"v{i}","env":{env},"tag":{tag}}}"#));
                    }
                }

                // Sub-table `deps`: [], 1-2 items, null, absent
                match next() % 10 {
                    0 => {} // absent
                    1 => fields.push(r#""deps":null"#.to_owned()),
                    2..=6 => fields.push(r#""deps":[]"#.to_owned()),
                    _ => {
                        let count = 1 + (next() as usize % 2);
                        let mut elems = Vec::with_capacity(count);
                        for j in 0..count {
                            let opt = if next() % 4 < 3 { "false" } else { "true" };
                            let kind = if next() % 5 < 4 { "null" } else { "\"dev\"" };
                            elems.push(format!(
                                r#"{{"name":"d{i}-{j}","opt":{opt},"kind":{kind}}}"#
                            ));
                        }
                        fields.push(format!(r#""deps":[{}]"#, elems.join(",")));
                    }
                }

                records.push(format!("{{{}}}", fields.join(",")));
            }

            let doc = match trial % 3 {
                0 => format!("[{}]", records.join(",")),
                1 => format!("{}\n", records.join("\n")),
                _ => format!(
                    r#"{{"version":1,"rows":[{}],"tail":"end"}}"#,
                    records.join(",")
                ),
            };
            let padded_doc = padded(&doc);
            let original = Json::read(&padded_doc).unwrap().original;
            for group in [false, true] {
                let all_views = views(&padded_doc, group);
                assert!(!all_views.is_empty(), "trial {trial} produced no view");
                for v in &all_views {
                    assert!(v.len() < padded_doc.len());
                    assert_eq!(
                        decode(v).unwrap(),
                        original,
                        "trial {trial} group={group} failed round-trip:\n{v}"
                    );
                }
            }
        }
    }
}
