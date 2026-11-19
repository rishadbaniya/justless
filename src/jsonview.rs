//! A lossless table view of JSON output: arrays of objects and JSON lines name their
//! fields once instead of on every record.
//!
//! Scalars, keys and escapes keep their exact source text, so `"1"` and `1`, `1.0`
//! and `1`, or `"é"` and `"é"` stay apart. Every view is decoded back and must
//! equal the original data, compared as compact JSON with keys in their original
//! order, or no view is shown. Pretty-printing whitespace is the only thing a view
//! drops, and `justless log` keeps the exact bytes.
//!
//! Format, after one legend line:
//! - a table is `N objects; fields: a,b=V,c` followed by N rows. Each row holds the
//!   cells of the fields without `=` as compact JSON, separated by commas, and an
//!   empty cell is an absent field. `b=V` means every object holds `b` with value `V`
//!   at that position. A field name is bare when it is a plain identifier and a JSON
//!   string otherwise.
//! - with grouping, one field is marked `@a`, and a line `@ CELL` before a run of
//!   rows gives their shared value of that field.
//! - an object that holds a table is shown one member per line, `KEY: VALUE` with the
//!   value as compact JSON, `KEY: N objects; fields: …` with the rows indented two
//!   spaces, or `KEY:` with the members of a nested object indented two spaces.

/// The largest output this module reads.
pub const LIMIT: usize = 8 * 1024 * 1024;
const MIN_BYTES: usize = 256;
const DEPTH: usize = 128;
/// How deep objects are opened one member per line to reach a table.
const EXPAND_DEPTH: usize = 4;
const NOTE: &str = "fields are named once, NAME=VALUE holds in every row, an empty cell is an absent field; exact JSON in the full log";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Root {
    Array,
    Lines,
    Object,
}

impl Root {
    fn legend(self) -> String {
        let what = match self {
            Self::Array => "JSON array as a table",
            Self::Lines => "JSON lines as a table",
            Self::Object => "JSON object with arrays of objects as tables",
        };
        format!("[{what}: {NOTE}]\n")
    }

    fn from_legend(line: &str) -> Option<Self> {
        [Self::Array, Self::Lines, Self::Object]
            .into_iter()
            .find(|root| root.legend().trim_end() == line)
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

/// The view of JSON output, when it is smaller than `text` and decodes to the same
/// data. `group` also folds consecutive rows that share a field's value.
pub fn render(text: &str, group: bool) -> Option<String> {
    if text.len() < MIN_BYTES || text.len() > LIMIT {
        return None;
    }
    let first = text.trim_start().as_bytes().first().copied()?;
    if first != b'[' && first != b'{' {
        return None;
    }
    let (root, nodes) = match parse(text) {
        Some(node @ Node::Array(_)) => (Root::Array, vec![node]),
        Some(node @ Node::Object(_)) => (Root::Object, vec![node]),
        Some(Node::Raw(_)) => return None,
        None => {
            let mut records = Vec::new();
            for line in text.trim_end().split('\n') {
                match parse(line)? {
                    node @ Node::Object(_) => records.push(node),
                    _ => return None,
                }
            }
            (Root::Lines, vec![Node::Array(records)])
        }
    };
    let mut view = root.legend();
    let shown = match (root, &nodes[0]) {
        (Root::Object, Node::Object(members)) => expand(members, 0, group, 0, &mut view),
        (_, Node::Array(items)) => table(items, 0, group, &mut view),
        _ => false,
    };
    if !shown || view.len() >= text.len() {
        return None;
    }
    let original = match (root, &nodes[0]) {
        (Root::Lines, Node::Array(records)) => {
            records.iter().map(compacted).collect::<Vec<_>>().join("\n")
        }
        (_, node) => compacted(node),
    };
    (decode(&view)? == original).then_some(view)
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
    /// Each object's value for each column.
    cells: Vec<Vec<Option<&'n Node<'a>>>>,
}

/// The columns of an array of objects whose keys all follow one order, without
/// repeats.
fn columns<'n, 'a>(items: &'n [Node<'a>]) -> Option<Columns<'n, 'a>> {
    let mut keys: Vec<&'a str> = Vec::new();
    for item in items {
        let Node::Object(members) = item else {
            return None;
        };
        // Merge this object's keys into the order: a new key goes right before the
        // object's next known key, so every object keeps its own order.
        let mut merged = Vec::with_capacity(keys.len() + members.len());
        let mut known = 0;
        let mut new = Vec::new();
        for (key, _) in members {
            if new.contains(key) {
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
                None => new.push(*key),
            }
        }
        merged.extend_from_slice(&keys[known..]);
        merged.append(&mut new);
        keys = merged;
    }
    let cells = items
        .iter()
        .map(|item| {
            let Node::Object(members) = item else {
                unreachable!()
            };
            keys.iter()
                .map(|key| members.iter().find(|(k, _)| k == key).map(|(_, v)| v))
                .collect()
        })
        .collect();
    Some(Columns { keys, cells })
}

/// Write `items` as a table when that is smaller than their compact JSON.
fn table(items: &[Node], indent: usize, group: bool, out: &mut String) -> bool {
    if items.len() < 2 {
        return false;
    }
    let Some(Columns { keys, cells }) = columns(items) else {
        return false;
    };
    let text: Vec<Vec<Option<String>>> = cells
        .iter()
        .map(|row| row.iter().map(|cell| cell.map(compacted)).collect())
        .collect();
    let constant: Vec<bool> = (0..keys.len())
        .map(|column| {
            text[0][column].is_some() && text.iter().all(|row| row[column] == text[0][column])
        })
        .collect();
    let varying: Vec<usize> = (0..keys.len()).filter(|&c| !constant[c]).collect();
    if varying.is_empty() {
        return false;
    }
    let grouped = if group {
        grouping(&text, &varying, indent)
    } else {
        None
    };
    let pad = " ".repeat(indent);
    let mut block = format!("{} objects; fields: ", items.len());
    for (column, key) in keys.iter().enumerate() {
        if column > 0 {
            block.push(',');
        }
        if grouped == Some(column) {
            block.push('@');
        }
        block.push_str(field_name(key));
        if constant[column] {
            block.push('=');
            block.push_str(text[0][column].as_deref().unwrap_or_default());
        }
    }
    block.push('\n');
    let mut current: Option<Option<&str>> = None;
    for row in &text {
        if let Some(column) = grouped {
            let value = row[column].as_deref();
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
            block.push_str(row[column].as_deref().unwrap_or_default());
        }
        block.push('\n');
    }
    let inline: usize = items.iter().map(|item| compacted(item).len() + 1).sum();
    if block.len() >= inline {
        return false;
    }
    out.push_str(&block);
    true
}

/// The varying column whose runs of equal values save the most when each run is
/// named once.
fn grouping(rows: &[Vec<Option<String>>], varying: &[usize], indent: usize) -> Option<usize> {
    if varying.len() < 2 {
        return None;
    }
    varying
        .iter()
        .map(|&column| {
            let mut saved: isize = 0;
            let mut previous: Option<&Option<String>> = None;
            for row in rows {
                let cell = &row[column];
                let length = cell.as_deref().map_or(0, str::len) as isize;
                saved += length + 1;
                if previous != Some(cell) {
                    saved -= indent as isize + 3 + length;
                }
                previous = Some(cell);
            }
            (saved, column)
        })
        .filter(|&(saved, _)| saved > 0)
        .max_by_key(|&(saved, column)| (saved, std::cmp::Reverse(column)))
        .map(|(_, column)| column)
}

/// Write an object one member per line when a member holds a table.
fn expand(
    members: &[(&str, Node)],
    indent: usize,
    group: bool,
    depth: usize,
    out: &mut String,
) -> bool {
    if depth > EXPAND_DEPTH || members.is_empty() {
        return false;
    }
    let pad = " ".repeat(indent);
    let mut block = String::new();
    let mut any = false;
    for (key, value) in members {
        block.push_str(&pad);
        block.push_str(field_name(key));
        block.push(':');
        let mut nested = String::new();
        let shown = match value {
            Node::Array(items) => table(items, indent + 2, group, &mut nested),
            Node::Object(inner) => {
                nested.push('\n');
                expand(inner, indent + 2, group, depth + 1, &mut nested)
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
        out.push_str(&block);
    }
    any
}

/// Rebuild compact JSON from a view: the data the view claims to show.
fn decode(view: &str) -> Option<String> {
    let mut lines = view.strip_suffix('\n')?.split('\n');
    let root = Root::from_legend(lines.next()?)?;
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
    let end = text.find([',', '=', ':']).unwrap_or(text.len());
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

enum Field {
    Constant(String),
    Group,
    Varying,
}

fn decode_table(header: &str, lines: &mut Lines, indent: usize) -> Option<Vec<String>> {
    let (count, mut rest) = table_header(header)?;
    let mut fields: Vec<(String, Field)> = Vec::new();
    while !rest.is_empty() {
        if !fields.is_empty() {
            rest = rest.strip_prefix(',')?;
        }
        let grouped = rest.starts_with('@');
        let (key, after) = header_key(rest.strip_prefix('@').unwrap_or(rest))?;
        rest = after;
        let field = if let Some(after) = rest.strip_prefix('=') {
            let (value, end) = value_at(after, 0)?;
            rest = &after[end..];
            Field::Constant(value)
        } else if grouped {
            Field::Group
        } else {
            Field::Varying
        };
        if grouped && !matches!(field, Field::Group) {
            return None;
        }
        fields.push((key, field));
    }
    let varying = fields
        .iter()
        .filter(|(_, field)| matches!(field, Field::Varying))
        .count();
    let mut group: Option<Option<String>> = None;
    let mut rows = Vec::with_capacity(count);
    while rows.len() < count {
        let line = lines.next()?;
        let line = line.get(indent..).filter(|_| indent_of(line) >= indent)?;
        if let Some(rest) = line.strip_prefix('@') {
            group = Some(if rest.is_empty() {
                None
            } else {
                let cell = rest.strip_prefix(' ')?;
                let (value, end) = value_at(cell, 0)?;
                (end == cell.len()).then_some(())?;
                Some(value)
            });
            continue;
        }
        let mut cells = Vec::with_capacity(varying);
        let mut at = 0;
        for index in 0..varying {
            if index > 0 {
                (line.as_bytes().get(at) == Some(&b',')).then_some(())?;
                at += 1;
            }
            if at == line.len() || line.as_bytes()[at] == b',' {
                cells.push(None);
            } else {
                let (value, end) = value_at(line, at)?;
                cells.push(Some(value));
                at = end;
            }
        }
        if at != line.len() {
            return None;
        }
        let mut cells = cells.into_iter();
        let mut object = String::from("{");
        for (key, field) in &fields {
            let value = match field {
                Field::Constant(value) => Some(value.clone()),
                Field::Group => group.clone()?,
                Field::Varying => cells.next()?,
            };
            if let Some(value) = value {
                if object.len() > 1 {
                    object.push(',');
                }
                object.push_str(key);
                object.push(':');
                object.push_str(&value);
            }
        }
        object.push('}');
        rows.push(object);
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
                let view = render(&text, group).unwrap_or_else(|| panic!("no view for {case}"));
                assert!(view.len() < text.len());
                let original = match parse(&text) {
                    Some(node) => compacted(&node),
                    None => text
                        .trim_end()
                        .split('\n')
                        .map(|line| compacted(&parse(line).unwrap()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                assert_eq!(decode(&view).unwrap(), original, "{view}");
            }
        }
        let view = render(&padded(cases[0]), false).unwrap();
        assert!(
            view.contains("3 objects; fields: name,kind=\"lib\",n,tag,x,y\n"),
            "{view}"
        );
        assert!(view.contains("\"c,d\",,\"=,\\\"\",,true\n"), "{view}");
    }

    #[test]
    fn irregular_or_small_json_has_no_view() {
        for text in [
            // Keys in conflicting orders.
            padded(r#"[{"a":1,"b":2},{"b":3,"a":4}]"#),
            // A duplicate key.
            padded(r#"[{"a":1,"a":2},{"a":3,"a":4}]"#),
            // Not all objects, or not JSON at all.
            padded(r#"[{"a":1},2]"#),
            padded("{\"a\":1}\nnot json\n"),
            // Too small to help.
            r#"[{"a":1},{"a":2}]"#.to_owned(),
            // Every field shared: nothing varies.
            padded(r#"[{"a":1},{"a":1}]"#),
        ] {
            assert_eq!(render(&text, false), None, "{text}");
        }
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
        let view = render(
            &padded(r#"[{"a":1,"b":"x"},{"a":2,"b":"y"},{"a":3}]"#),
            false,
        )
        .unwrap();
        assert!(decode(&view).is_some());
        for damaged in [
            view.replacen("3 objects", "4 objects", 1),
            view.replacen("2,\"y\"", "2,\"y\",", 1),
            format!("{view}extra\n"),
        ] {
            assert_eq!(decode(&damaged), None, "{damaged}");
        }
    }
}
