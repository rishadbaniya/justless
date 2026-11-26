//! Deletions from JSON text that keep every other byte.
//!
//! `serde_json` reads a document into values and writes it back in its own layout,
//! which would reflow a config file the user formatted by hand. This module finds
//! the byte span of each member and element instead, and deletes a selected child
//! together with the comma and blank space that separate it from a neighbor.

use std::borrow::Cow;
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
    let mut targets: BTreeMap<usize, (&Node<'_>, BTreeSet<usize>)> = BTreeMap::new();
    for path in paths {
        let (last, parents) = path.split_last().context("cannot delete the root")?;
        let mut node = &root;
        for step in parents {
            let index = child(node, step)?;
            node = node.child_node(index)?;
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
        let count = node.child_count()?;
        if deleted.len() == count {
            ranges.push(node.span.start + 1..node.span.end - 1);
            continue;
        }
        // A run of deleted children takes the separator before the next kept child,
        // or, at the end, the separator after the last kept one.
        let mut at = 0;
        while at < count {
            if !deleted.contains(&at) {
                at += 1;
                continue;
            }
            let first = at;
            while at < count && deleted.contains(&at) {
                at += 1;
            }
            ranges.push(if at < count {
                node.child_span(first).start..node.child_span(at).start
            } else {
                node.child_span(first - 1).end..node.child_span(at - 1).end
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
    let mut paths: Vec<&[Step]> = paths.iter().map(Vec::as_slice).collect();
    // Later elements first, so earlier indices stay valid.
    paths.sort_unstable();
    paths.dedup();
    for path in paths.into_iter().rev() {
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
struct Node<'a> {
    span: Range<usize>,
    kind: Kind<'a>,
}

#[derive(Debug)]
enum Kind<'a> {
    /// Members with the byte offset where each key starts.
    Object(Vec<(Cow<'a, str>, usize, Node<'a>)>),
    Array(Vec<Node<'a>>),
    Scalar,
}

impl<'a> Node<'a> {
    fn child_count(&self) -> Result<usize> {
        match &self.kind {
            Kind::Object(members) => Ok(members.len()),
            Kind::Array(items) => Ok(items.len()),
            Kind::Scalar => bail!("JSON path continues past a scalar"),
        }
    }

    fn child_node(&self, index: usize) -> Result<&Node<'a>> {
        match &self.kind {
            Kind::Object(members) => Ok(&members[index].2),
            Kind::Array(items) => Ok(&items[index]),
            Kind::Scalar => bail!("JSON path continues past a scalar"),
        }
    }

    /// One child's deletable span, from its key or first byte to its last byte.
    fn child_span(&self, index: usize) -> Range<usize> {
        match &self.kind {
            Kind::Object(members) => {
                let (_, start, value) = &members[index];
                *start..value.span.end
            }
            Kind::Array(items) => items[index].span.clone(),
            Kind::Scalar => unreachable!("scalar has no children"),
        }
    }
}

/// The index of a child. A repeated key resolves to its last occurrence, the one
/// `serde_json` keeps.
fn child(node: &Node<'_>, step: &Step) -> Result<usize> {
    match (&node.kind, step) {
        (Kind::Object(members), Step::Key(key)) => members
            .iter()
            .rposition(|(name, _, _)| name.as_ref() == key.as_str())
            .with_context(|| format!("no JSON member {key}")),
        (Kind::Array(items), Step::Index(index)) if *index < items.len() => Ok(*index),
        _ => bail!("JSON path does not match the document"),
    }
}

fn parse(text: &str) -> Result<Node<'_>> {
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

impl<'a> Parser<'a> {
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

    fn value(&mut self) -> Result<Node<'a>> {
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
                        let key_span = self.string()?;
                        let raw_key = &self.text[key_span];
                        let inner = &raw_key[1..raw_key.len() - 1];
                        let key: Cow<'a, str> = if !inner.bytes().any(|b| b == b'\\' || b < 0x20) {
                            Cow::Borrowed(inner)
                        } else {
                            Cow::Owned(serde_json::from_str::<String>(raw_key)?)
                        };
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
                Some(b'\\') => {
                    self.at += 1;
                    let Some(escaped) = self.text[self.at..].chars().next() else {
                        bail!("unterminated JSON string");
                    };
                    self.at += escaped.len_utf8();
                }
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

    #[test]
    fn handles_multibyte_utf8_keys_escapes_and_malformed_backslashes() {
        // Multi-byte UTF-8 keys (CJK, emoji, accented) and escaped unicode/quotes keep exact bytes.
        let doc = "{\n  \"日本語\": \"値🚀\",\n  \"🚀key\": [\"α\", \"β\", \"γ\"],\n  \"esc\\\"\\u00fc\": true\n}\n";
        check(
            doc,
            &[vec![key("🚀key"), Step::Index(1)]],
            "{\n  \"日本語\": \"値🚀\",\n  \"🚀key\": [\"α\", \"γ\"],\n  \"esc\\\"\\u00fc\": true\n}\n",
        );
        check(
            doc,
            &[vec![key("日本語")], vec![key("esc\"ü")]],
            "{\n  \"🚀key\": [\"α\", \"β\", \"γ\"]\n}\n",
        );
        // Backslash followed by multi-byte UTF-8 or EOF must never panic on char boundaries.
        assert!(delete("{\"\\é\": 1}", &[vec![key("x")]]).is_err());
        assert!(delete("{\"a\": \"\\", &[vec![key("a")]]).is_err());
        assert!(delete("{\"a\": \"\\é\", \"b\": 2}", &[vec![key("b")]]).is_ok());
    }
}
