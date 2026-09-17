//! Deletions from JSON text that keep every other byte.
//!
//! `serde_json` reads a document into values and writes it back in its own layout,
//! which would reflow a config file the user formatted by hand. This module finds
//! the byte span of each member and element instead, and deletes a selected child
//! together with the comma and blank space that separate it from a neighbor.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

/// One step of a path from the document root to a member or element.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Key(String),
    Index(usize),
}

/// Delete the member or element at the end of each path. A container whose
/// children are all deleted becomes empty, as in `{}`. Paths must not name the
/// root. A path inside another deleted path is ignored.
pub fn delete(text: &str, paths: &[Vec<Step>]) -> Result<String> {
    let root = parse(text)?;
    let mut targets: BTreeMap<usize, (&Node, BTreeSet<usize>)> = BTreeMap::new();
    for path in paths {
        let (last, parents) = path.split_last().context("cannot delete the root")?;
        let mut node = &root;
        for step in parents {
            node = node.children()?[child(node, step)?].1;
        }
        let index = child(node, last)?;
        targets
            .entry(node.span.start)
            .or_insert_with(|| (node, BTreeSet::new()))
            .1
            .insert(index);
    }
    let mut ranges = Vec::new();
    for (node, deleted) in targets.values() {
        let spans: Vec<Range<usize>> = node.children()?.into_iter().map(|(s, _)| s).collect();
        if deleted.len() == spans.len() {
            ranges.push(node.span.start + 1..node.span.end - 1);
            continue;
        }
        // A run of deleted children takes the separator before the next kept child,
        // or, at the end, the separator after the last kept one.
        let mut at = 0;
        while at < spans.len() {
            if !deleted.contains(&at) {
                at += 1;
                continue;
            }
            let first = at;
            while at < spans.len() && deleted.contains(&at) {
                at += 1;
            }
            ranges.push(if at < spans.len() {
                spans[first].start..spans[at].start
            } else {
                spans[first - 1].end..spans[at - 1].end
            });
        }
    }
    ranges.sort_by_key(|range| (range.start, std::cmp::Reverse(range.end)));
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    for range in ranges {
        if range.end <= copied && range.start < copied {
            continue;
        }
        ensure!(range.start >= copied, "overlapping JSON deletions");
        output.push_str(&text[copied..range.start]);
        copied = range.end;
    }
    output.push_str(&text[copied..]);
    Ok(output)
}

/// Apply the same deletions to a parsed value, for checking the edited text.
pub fn delete_value(value: &mut Value, paths: &[Vec<Step>]) {
    let mut paths = paths.to_vec();
    // Later elements first, so earlier indices stay valid.
    paths.sort();
    paths.dedup();
    for path in paths.iter().rev() {
        let Some((last, parents)) = path.split_last() else {
            continue;
        };
        let mut node = &mut *value;
        for step in parents {
            node = match step {
                Step::Key(key) => &mut node[key.as_str()],
                Step::Index(index) => &mut node[*index],
            };
        }
        match (last, node) {
            (Step::Key(key), Value::Object(map)) => {
                map.shift_remove(key.as_str());
            }
            (Step::Index(index), Value::Array(items)) if *index < items.len() => {
                items.remove(*index);
            }
            _ => {}
        }
    }
}

#[derive(Debug)]
struct Node {
    span: Range<usize>,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    /// Members with the byte offset where each key starts.
    Object(Vec<(String, usize, Node)>),
    Array(Vec<Node>),
    Scalar,
}

impl Node {
    /// Each child's deletable span, from its key or first byte to its last byte.
    fn children(&self) -> Result<Vec<(Range<usize>, &Node)>> {
        Ok(match &self.kind {
            Kind::Object(members) => members
                .iter()
                .map(|(_, start, value)| (*start..value.span.end, value))
                .collect(),
            Kind::Array(items) => items.iter().map(|item| (item.span.clone(), item)).collect(),
            Kind::Scalar => bail!("JSON path continues past a scalar"),
        })
    }
}

/// The index of a child. A repeated key resolves to its last occurrence, the one
/// `serde_json` keeps.
fn child(node: &Node, step: &Step) -> Result<usize> {
    match (&node.kind, step) {
        (Kind::Object(members), Step::Key(key)) => members
            .iter()
            .rposition(|(name, _, _)| name == key)
            .with_context(|| format!("no JSON member {key}")),
        (Kind::Array(items), Step::Index(index)) if *index < items.len() => Ok(*index),
        _ => bail!("JSON path does not match the document"),
    }
}

fn parse(text: &str) -> Result<Node> {
    let mut parser = Parser { text, at: 0 };
    parser.blank();
    let root = parser.value()?;
    parser.blank();
    ensure!(parser.at == text.len(), "trailing characters after JSON");
    Ok(root)
}

struct Parser<'a> {
    text: &'a str,
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn blank(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<()> {
        ensure!(
            self.peek() == Some(byte),
            "expected {:?} at byte {}",
            byte as char,
            self.at
        );
        self.at += 1;
        Ok(())
    }

    fn value(&mut self) -> Result<Node> {
        let start = self.at;
        let kind = match self.peek() {
            Some(b'{') => {
                self.at += 1;
                let mut members = Vec::new();
                self.blank();
                if self.peek() == Some(b'}') {
                    self.at += 1;
                } else {
                    loop {
                        self.blank();
                        let key_start = self.at;
                        let key = self.string()?;
                        let key: String = serde_json::from_str(&self.text[key])?;
                        self.blank();
                        self.expect(b':')?;
                        self.blank();
                        members.push((key, key_start, self.value()?));
                        self.blank();
                        if self.peek() == Some(b',') {
                            self.at += 1;
                        } else {
                            self.expect(b'}')?;
                            break;
                        }
                    }
                }
                Kind::Object(members)
            }
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.blank();
                if self.peek() == Some(b']') {
                    self.at += 1;
                } else {
                    loop {
                        self.blank();
                        items.push(self.value()?);
                        self.blank();
                        if self.peek() == Some(b',') {
                            self.at += 1;
                        } else {
                            self.expect(b']')?;
                            break;
                        }
                    }
                }
                Kind::Array(items)
            }
            Some(b'"') => {
                self.string()?;
                Kind::Scalar
            }
            Some(_) => {
                while !matches!(
                    self.peek(),
                    None | Some(b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r')
                ) {
                    self.at += 1;
                }
                ensure!(self.at > start, "expected a JSON value at byte {start}");
                Kind::Scalar
            }
            None => bail!("unexpected end of JSON"),
        };
        Ok(Node {
            span: start..self.at,
            kind,
        })
    }

    fn string(&mut self) -> Result<Range<usize>> {
        let start = self.at;
        self.expect(b'"')?;
        loop {
            match self.peek() {
                Some(b'\\') => self.at += 2,
                Some(b'"') => {
                    self.at += 1;
                    return Ok(start..self.at);
                }
                Some(_) => self.at += 1,
                None => bail!("unterminated JSON string"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> Step {
        Step::Key(name.into())
    }

    fn check(text: &str, paths: &[Vec<Step>], expected: &str) {
        let edited = delete(text, paths).unwrap();
        assert_eq!(edited, expected);
        let mut value: Value = serde_json::from_str(text).unwrap();
        delete_value(&mut value, paths);
        assert_eq!(serde_json::from_str::<Value>(&edited).unwrap(), value);
    }

    #[test]
    fn deletes_members_and_elements_with_their_separators() {
        let pretty = "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2,\n    3\n  ],\n  \"c\": {}\n}\n";
        check(
            pretty,
            &[vec![key("a")]],
            "{\n  \"b\": [\n    1,\n    2,\n    3\n  ],\n  \"c\": {}\n}\n",
        );
        check(
            pretty,
            &[vec![key("c")]],
            "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2,\n    3\n  ]\n}\n",
        );
        check(
            pretty,
            &[
                vec![key("b"), Step::Index(1)],
                vec![key("b"), Step::Index(2)],
            ],
            "{\n  \"a\": 1,\n  \"b\": [\n    1\n  ],\n  \"c\": {}\n}\n",
        );
        check(
            pretty,
            &[
                vec![key("b"), Step::Index(0)],
                vec![key("b"), Step::Index(2)],
            ],
            "{\n  \"a\": 1,\n  \"b\": [\n    2\n  ],\n  \"c\": {}\n}\n",
        );
        // A nested path inside a deleted member is ignored.
        check(
            pretty,
            &[vec![key("b")], vec![key("b"), Step::Index(0)]],
            "{\n  \"a\": 1,\n  \"c\": {}\n}\n",
        );
        check("{\"a\":1}", &[vec![key("a")]], "{}");
    }

    #[test]
    fn keeps_hand_formatting_escapes_and_unicode() {
        let text = "{ \"k\\\"ey\": [\"x,]}\" , \"y\"],\t\"ü\" : null ,\"z\":true }";
        check(
            text,
            &[vec![key("k\"ey"), Step::Index(0)]],
            "{ \"k\\\"ey\": [\"y\"],\t\"ü\" : null ,\"z\":true }",
        );
        check(
            text,
            &[vec![key("ü")]],
            "{ \"k\\\"ey\": [\"x,]}\" , \"y\"],\t\"z\":true }",
        );
        assert!(delete(text, &[vec![key("missing")]]).is_err());
        assert!(delete(text, &[vec![]]).is_err());
    }
}
