//! Recognize the default human-readable Docker and Kubernetes tables.
use crate::filters::Profile;
use crate::summary::{Item, Summary};
use regex::Regex;
use std::sync::LazyLock;

static COLUMN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\S(?:[^\t ]| [^\t ])*").unwrap());

pub struct Inventory {
    kind: Profile,
    header: Vec<(String, usize)>,
    count: usize,
    examples: Vec<String>,
    issues: Vec<Item>,
}

impl Inventory {
    pub fn new(kind: Profile) -> Self {
        Self {
            kind,
            header: Vec::new(),
            count: 0,
            examples: vec![],
            issues: vec![],
        }
    }

    pub fn observe(&mut self, line: &str, n: usize) -> bool {
        let line = line.trim_end();
        if line.is_empty() {
            return true;
        }
        if self.header.is_empty() {
            if self.kind == Profile::Containers {
                self.header = COLUMN
                    .find_iter(line)
                    .map(|m| (m.as_str().to_owned(), m.start()))
                    .collect();
                return self
                    .header
                    .first()
                    .is_some_and(|(h, _)| h == "CONTAINER ID")
                    && self.col("STATUS").is_some()
                    && self.col("NAMES").is_some();
            }
            self.header = line
                .split_ascii_whitespace()
                .map(|s| (s.to_owned(), 0))
                .collect();
            let columns: &[&str] = match self.kind {
                Profile::Pods => &["NAME", "READY", "STATUS", "RESTARTS"],
                Profile::Deployments => &["NAME", "READY", "UP-TO-DATE", "AVAILABLE"],
                Profile::StatefulSets => &["NAME", "READY"],
                Profile::Nodes => &["NAME", "STATUS"],
                _ => return false,
            };
            return columns.iter().all(|h| self.col(h).is_some());
        }
        let row = if self.kind == Profile::Containers {
            self.container(line)
        } else {
            self.kubernetes(line)
        };
        let Some((name, issue)) = row else {
            return false;
        };
        self.count += 1;
        if let Some(reason) = issue {
            if self.issues.len() >= 2048 {
                return false;
            }
            self.issues.push(Item::new(&name, &reason, n));
        } else if self.examples.len() < 4 {
            self.examples.push(name);
        }
        true
    }

    fn col(&self, name: &str) -> Option<usize> {
        self.header.iter().position(|(label, _)| label == name)
    }

    fn container(&self, line: &str) -> Option<(String, Option<String>)> {
        let field = |key| {
            let i = self.col(key)?;
            if line.contains('\t') {
                return line.split('\t').nth(i).map(str::trim);
            }
            let start = self.header[i].1;
            let end = self
                .header
                .get(i + 1)
                .map_or(line.len(), |(_, end)| (*end).min(line.len()));
            line.get(start..end).map(str::trim)
        };
        let name = field("NAMES")?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        {
            return None;
        }
        let status = field("STATUS")?;
        let lower = status.to_ascii_lowercase();
        let issue = if lower.starts_with("up ") {
            lower.split_once('(').and_then(|(_, s)| {
                let health = s.trim_end_matches(')');
                (health != "healthy").then(|| health.to_owned())
            })
        } else if [
            "exited",
            "created",
            "restarting",
            "dead",
            "removing",
            "paused",
        ]
        .iter()
        .any(|s| lower.starts_with(s))
        {
            Some(
                status
                    .split_once(')')
                    .map(|(s, _)| format!("{s})"))
                    .unwrap_or_else(|| status.to_string()),
            )
        } else {
            return None;
        };
        Some((name.to_owned(), issue))
    }

    fn kubernetes(&self, line: &str) -> Option<(String, Option<String>)> {
        let values: Vec<_> = line.split_ascii_whitespace().collect();
        let get = |col| values.get(self.col(col)?).copied();
        let name = get("NAME")?;
        let name = match self.col("NAMESPACE") {
            Some(i) => format!("{}/{name}", values.get(i)?),
            None => name.to_string(),
        };
        let issue = match self.kind {
            Profile::Nodes => {
                let s = get("STATUS")?;
                (s != "Ready").then(|| s.to_string())
            }
            Profile::Pods => {
                let ready = get("READY")?;
                let (actual, desired) = ratio(ready)?;
                let status = get("STATUS")?;
                let restarts: usize = get("RESTARTS")?.parse().ok()?;
                let ok = status == "Completed"
                    || status == "Succeeded"
                    || (status == "Running" && actual == desired && actual != 0);
                (!ok || restarts > 0).then(|| format!("{status} ready={ready} restarts={restarts}"))
            }
            Profile::StatefulSets | Profile::Deployments => {
                let ready = get("READY")?;
                let (actual, desired) = ratio(ready)?;
                if self.kind == Profile::Deployments {
                    let updated: usize = get("UP-TO-DATE")?.parse().ok()?;
                    let available: usize = get("AVAILABLE")?.parse().ok()?;
                    (actual != desired || updated != desired || available != desired)
                        .then(|| format!("ready={ready} updated={updated} available={available}"))
                } else {
                    (actual != desired).then(|| format!("ready={ready}"))
                }
            }
            _ => return None,
        };
        Some((name, issue))
    }

    pub fn finish(self) -> Summary {
        if self.header.is_empty() {
            return Summary::default();
        }
        let mut outcomes = vec![format!(
            "{}: {} listed, {} need attention",
            self.kind.label(),
            self.count,
            self.issues.len()
        )];
        if !self.examples.is_empty() {
            outcomes.push(format!("Other examples: {}", self.examples.join(", ")));
        }
        Summary {
            kind: "inventory".into(),
            outcomes,
            items: self.issues,
        }
    }
}

fn ratio(value: &str) -> Option<(usize, usize)> {
    let (a, b) = value.split_once('/')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}
