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
            let end = self.header.get(i + 1).map(|(_, end)| *end);
            slice_cols(line, start, end).map(str::trim)
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
            "removal",
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
            ..Summary::default()
        }
    }
}

/// Slice `line` by character column offsets so UTF-8 ellipsis truncation (`…`)
/// in earlier columns of `docker ps` does not shift later columns.
fn slice_cols(line: &str, start_col: usize, end_col: Option<usize>) -> Option<&str> {
    if line.is_ascii() {
        let end = end_col.map_or(line.len(), |end| end.min(line.len()));
        return line.get(start_col..end);
    }
    let start = line.char_indices().nth(start_col).map(|(byte, _)| byte)?;
    let end = match end_col {
        Some(col) => line
            .char_indices()
            .nth(col)
            .map_or(line.len(), |(byte, _)| byte),
        None => line.len(),
    };
    line.get(start..end)
}

fn ratio(value: &str) -> Option<(usize, usize)> {
    let (a, b) = value.split_once('/')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::Inventory;
    use crate::filters::Profile;

    #[test]
    fn docker_ps_handles_utf8_command_ellipsis_and_removal_status() {
        let table = [
            "CONTAINER ID   IMAGE         COMMAND                  CREATED       STATUS                      PORTS      NAMES",
            "a1b2c3d4e5f6   postgres:16   \"docker-entrypoint.s…\"   2 hours ago   Up 2 hours (healthy)        5432/tcp   app-db-1",
            "b2c3d4e5f6a1   node:22       \"docker-entrypoint.s…\"   2 hours ago   Up 3 minutes (unhealthy)    3000/tcp   app-web-1",
            "c3d4e5f6a1b2   redis:7       \"docker-entrypoint.s…\"   2 hours ago   Up 1 hour (Paused)          6379/tcp   app-cache-1",
            "d4e5f6a1b2c3   alpine:3      \"/bin/sh -c 'sleep 1…\"   2 hours ago   Removal In Progress                    app-old-1",
        ];
        let mut inv = Inventory::new(Profile::Containers);
        for (i, line) in table.iter().enumerate() {
            assert!(inv.observe(line, i + 1), "row {}: {line}", i + 1);
        }
        let summary = inv.finish();
        assert_eq!(summary.outcomes[0], "docker ps: 4 listed, 3 need attention");
        assert_eq!(summary.outcomes[1], "Other examples: app-db-1");
        let issues: Vec<_> = summary
            .items
            .iter()
            .map(|item| (item.name.as_str(), item.detail.as_str()))
            .collect();
        assert_eq!(
            issues,
            [
                ("app-web-1", "unhealthy"),
                ("app-cache-1", "paused"),
                ("app-old-1", "Removal In Progress"),
            ]
        );
    }

    #[test]
    fn kubectl_pods_handles_restart_age_parentheticals() {
        let table = [
            "NAMESPACE   NAME        READY   STATUS             RESTARTS        AGE",
            "prod        api-0       1/1     Running            0               3d",
            "prod        worker-0    0/1     CrashLoopBackOff   5 (3m12s ago)   12m",
            "prod        cron-0      1/1     Running            1 (2h ago)      5d",
        ];
        let mut inv = Inventory::new(Profile::Pods);
        for (i, line) in table.iter().enumerate() {
            assert!(inv.observe(line, i + 1), "row {}: {line}", i + 1);
        }
        let summary = inv.finish();
        assert_eq!(
            summary.outcomes[0],
            "kubectl get pods: 3 listed, 2 need attention"
        );
        assert_eq!(summary.outcomes[1], "Other examples: prod/api-0");
        assert_eq!(summary.items[0].name, "prod/worker-0");
        assert_eq!(
            summary.items[0].detail,
            "CrashLoopBackOff ready=0/1 restarts=5"
        );
        assert_eq!(summary.items[1].name, "prod/cron-0");
        assert_eq!(summary.items[1].detail, "Running ready=1/1 restarts=1");
    }
}
