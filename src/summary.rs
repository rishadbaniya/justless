//! Compact records point back to inclusive line ranges in the original stdout.
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Summary {
    pub kind: String,
    pub items: Vec<Item>,
    pub outcomes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub name: String,
    pub detail: String,
    pub digest: String,
    pub first: usize,
    pub last: usize,
}

pub fn family(args: &[OsString]) -> String {
    let mut words: Vec<_> = args
        .iter()
        .take(3)
        .map(|a| a.to_str().unwrap_or(""))
        .collect();
    if let Some(command) = words.first_mut() {
        *command = std::path::Path::new(*command)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
    }
    match words.as_slice() {
        ["git", "diff", ..] => "git diff",
        ["cargo", "test", ..] => "cargo test",
        ["go", "test", ..] => "go test",
        ["npm", "test", ..] => "npm test",
        ["pytest", ..] | ["python" | "python3", "-m", "pytest"] => "pytest",
        [cmd @ ("cargo" | "git" | "go" | "npm" | "rg"), ..] => cmd,
        _ => "other",
    }
    .into()
}

const LIMIT: usize = 2048;

pub struct Parser {
    inventory: Option<crate::inventory::Inventory>,
    summary: Summary,
    hash: Sha256,
    file: String,
    pending: Option<Item>,
    failures: BTreeMap<String, Item>,
    current_test: String,
    discarded: bool,
}

impl Parser {
    pub fn new(family: &str, profile: Option<crate::filters::Profile>) -> Self {
        let kind = match family {
            _ if profile.is_some() => "",
            "git diff" => "diff",
            "pytest" | "cargo test" | "go test" | "npm test" => "tests",
            _ => "",
        };
        Self {
            inventory: profile
                .filter(|p| p.inventory())
                .map(crate::inventory::Inventory::new),
            summary: Summary {
                kind: kind.into(),
                ..Default::default()
            },
            hash: Sha256::new(),
            file: String::new(),
            pending: None,
            failures: BTreeMap::new(),
            current_test: String::new(),
            discarded: false,
        }
    }

    pub fn observe(&mut self, text: &str, number: usize) {
        if (self.summary.kind.is_empty() && self.inventory.is_none()) || self.discarded {
            return;
        }
        if text.len() >= 256 * 1024
            || text.contains('\0')
            || self.summary.items.len() + self.failures.len() >= LIMIT
            || self.summary.outcomes.len() >= 128
        {
            self.discarded = true;
            return;
        }
        if let Some(inventory) = &mut self.inventory {
            self.discarded = !inventory.observe(text, number);
            return;
        }
        let line = text.trim_end_matches('\n');
        if self.summary.kind == "diff"
            && self.file.is_empty()
            && !line.is_empty()
            && !line.starts_with("diff --git ")
        {
            self.discarded = true;
            return;
        }
        if self.summary.kind == "diff" {
            self.diff(line, number);
        } else {
            self.test(line, number);
        }
    }

    fn flush(&mut self) {
        if let Some(mut item) = self.pending.take() {
            item.digest = format!("{:x}", self.hash.finalize_reset());
            self.summary.items.push(item);
        }
    }

    fn diff(&mut self, line: &str, n: usize) {
        if let Some(names) = line.strip_prefix("diff --git ") {
            self.flush();
            self.file = names
                .rsplit_once(" b/")
                .map_or(names, |(_, new)| new)
                .to_owned();
            self.pending = Some(Item::new(&self.file, "file metadata", n));
        } else if line.starts_with("@@ ") && !self.file.is_empty() {
            // Ordinary index/---/+++ headers contain blob hashes for the entire file.
            // Avoid invalidating independent hunks when just one of them changes.
            if self
                .pending
                .as_ref()
                .is_some_and(|i| i.detail == "file metadata")
            {
                self.pending = None;
                self.hash = Sha256::new();
            }
            self.flush();
            self.pending = Some(Item::new(&self.file, line, n));
        } else if (line.starts_with("old mode ")
            || line.starts_with("rename from ")
            || line.starts_with("new file mode ")
            || line.starts_with("deleted file mode ")
            || line.starts_with("copy from "))
            && let Some(item) = &mut self.pending
        {
            item.detail = short(line, 360);
        }
        if let Some(item) = &mut self.pending {
            self.hash.update(line.as_bytes());
            self.hash.update(b"\n");
            item.last = n;
        }
    }

    fn test(&mut self, line: &str, n: usize) {
        let line = line.trim();
        let failed = if let Some(name) = line
            .strip_prefix("test ")
            .and_then(|s| s.strip_suffix(" ... FAILED"))
        {
            Some((name, ""))
        } else if let Some(entry) = line
            .strip_prefix("FAILED ")
            .or_else(|| line.strip_prefix("ERROR "))
        {
            Some(entry.split_once(" - ").unwrap_or((entry, "")))
        } else {
            line.strip_prefix("--- FAIL: ")
                .map(|entry| (entry.split_whitespace().next().unwrap_or(entry), ""))
        };
        if let Some((name, detail)) = failed {
            let item = self
                .failures
                .entry(short(name, 240))
                .or_insert_with(|| Item::new(name, "", n));
            if !detail.is_empty() {
                item.digest = crate::capture::source_digest(detail.as_bytes());
                item.detail = short(detail, 320);
                item.last = n;
            }
        }
        if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|s| s.strip_suffix(" stdout ----"))
        {
            self.current_test = short(name, 240);
        }
        if let Some((_, location)) = line.split_once(" panicked at ")
            && let Some(item) = self.failures.get_mut(&self.current_test)
        {
            item.first = n;
            item.last = n;
            item.digest = crate::capture::source_digest(location.as_bytes());
            item.detail = short(location, 200);
        }
        if (line.starts_with("assertion ")
            || line.starts_with("left:")
            || line.starts_with("right:"))
            && let Some(item) = self.failures.get_mut(&self.current_test)
            && n.saturating_sub(item.last) <= 2
        {
            item.digest =
                crate::capture::source_digest(format!("{}{}", item.digest, line).as_bytes());
            item.detail = short(&format!("{} {}", item.detail, line), 440);
            item.last = n;
        }
        if line.starts_with("test result:")
            || line.starts_with("Tests:")
            || line.starts_with("Test Suites:")
            || line.starts_with("ok  \t")
            || ((line.contains(" passed") || line.contains(" failed"))
                && line.contains(" in ")
                && line
                    .trim_start_matches(['=', ' '])
                    .starts_with(|c: char| c.is_ascii_digit()))
        {
            self.summary
                .outcomes
                .push(short(line.trim_matches(['=', ' ']), 220));
        }
    }

    pub fn finish(mut self) -> Summary {
        if self.discarded {
            return Summary::default();
        }
        if let Some(inventory) = self.inventory {
            return inventory.finish();
        }
        self.flush();
        self.summary.items.extend(self.failures.into_values());
        self.summary
    }
}

impl Item {
    pub(crate) fn new(name: &str, detail: &str, n: usize) -> Self {
        Self {
            name: short(name, 320),
            detail: short(detail, 360),
            digest: String::new(),
            first: n,
            last: n,
        }
    }
    fn key(&self) -> (&str, &str, &str) {
        (&self.name, &self.detail, &self.digest)
    }
    fn display(&self) -> String {
        format!(
            "{} {} [stdout:{}-{}]",
            self.name, self.detail, self.first, self.last
        )
    }
}

pub fn short(s: &str, len: usize) -> String {
    s[..s.floor_char_boundary(s.len().min(len))].to_owned()
}

impl Summary {
    pub fn render(&self, previous: Option<(&str, &Summary)>, status: i32) -> Option<String> {
        if self.kind.is_empty()
            || (self.items.is_empty()
                && self.outcomes.is_empty()
                && !(self.kind == "diff" && previous.is_some_and(|(_, s)| !s.items.is_empty())))
        {
            return None;
        }
        let mut report = String::new();
        if self.kind == "inventory" {
            if status != 0 {
                return None;
            }
            report.push_str(self.outcomes.first()?);
            report.push('\n');
        }
        if self.kind == "tests" {
            if status != 0 && self.items.is_empty() {
                return None;
            }
            report.push_str(&format!("{} reported failing test(s)\n", self.items.len()));
        }
        let previous = previous.filter(|(_, s)| s.kind == self.kind);
        if let Some((id, old)) = previous {
            let known: BTreeSet<_> = old.items.iter().map(Item::key).collect();
            let current: BTreeSet<_> = self.items.iter().map(Item::key).collect();
            report.push_str(&format!("Compared with {id}:\n"));
            for item in self.items.iter().filter(|i| !known.contains(&i.key())) {
                report.push_str(&format!("+ {}\n", item.display()));
            }
            for item in old.items.iter().filter(|i| !current.contains(&i.key())) {
                report.push_str(&format!(
                    "- {} {} [prior stdout:{}-{}]\n",
                    item.name, item.detail, item.first, item.last
                ));
            }
            if known == current {
                report.push_str(if self.kind == "inventory" {
                    "Attention list unchanged.\n"
                } else {
                    "Indexed items unchanged; raw output may differ.\n"
                });
            }
        } else if self.kind == "tests" {
            let mut groups: BTreeMap<(&str, &str), Vec<&Item>> = BTreeMap::new();
            for item in &self.items {
                groups
                    .entry((&item.detail, &item.digest))
                    .or_default()
                    .push(item);
            }
            for ((detail, _), items) in groups {
                if detail.is_empty() {
                    for item in items {
                        report.push_str(&format!("{}\n", item.display()));
                    }
                } else {
                    report.push_str(&format!(
                        "{} test(s): {}\n",
                        items.len(),
                        items[0].display()
                    ));
                    for item in &items[1..] {
                        report.push_str(&format!("  {} [stdout:{}]\n", item.name, item.first));
                    }
                }
            }
        } else {
            if self.kind == "diff" {
                report.push_str(&format!("{} diff section(s)\n", self.items.len()));
            }
            for item in &self.items {
                report.push_str(&format!("{}\n", item.display()));
            }
        }
        for line in self
            .outcomes
            .iter()
            .skip(usize::from(self.kind == "inventory"))
        {
            report.push_str(line);
            report.push('\n');
        }
        Some(report)
    }
}
