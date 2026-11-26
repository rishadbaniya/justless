//! Recognize human-readable Docker, Podman, Helm and Kubernetes tables.
use crate::filters::Profile;
use crate::summary::{Item, Summary};
use regex::Regex;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::LazyLock;

static COLUMN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\S(?:[^\t ]| [^\t ])*").unwrap());

const MAX_HEALTHY_GROUP_LINES: usize = 24;

#[inline]
fn strip_ansi(text: &str) -> Cow<'_, str> {
    if text.contains('\x1b') {
        crate::capture::ANSI.replace_all(text, "")
    } else {
        Cow::Borrowed(text)
    }
}

#[inline]
fn starts_with_ascii_icase(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

#[inline]
fn contains_ascii_icase(haystack: &str, needle_lower: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle_lower.as_bytes();
    if n.is_empty() {
        return true;
    }
    if h.len() < n.len() {
        return false;
    }
    let first_lo = n[0];
    let first_hi = first_lo.to_ascii_uppercase();
    let limit = h.len() - n.len();
    let mut i = 0;
    while i <= limit {
        let b = h[i];
        if (b == first_lo || b == first_hi) && h[i..i + n.len()].eq_ignore_ascii_case(n) {
            return true;
        }
        i += 1;
    }
    false
}

/// Severity rank for pinning critical container/pod/node/workload issues first.
/// Lower rank prints first; equal ranks preserve original stdout line order.
fn issue_severity_tier(detail: &str) -> u8 {
    if contains_ascii_icase(detail, "crashloopbackoff")
        || contains_ascii_icase(detail, "imagepullbackoff")
        || contains_ascii_icase(detail, "errimagepull")
        || contains_ascii_icase(detail, "oomkilled")
        || contains_ascii_icase(detail, "oomkilling")
        || contains_ascii_icase(detail, "createcontainerconfigerror")
        || contains_ascii_icase(detail, "createcontainererror")
        || contains_ascii_icase(detail, "invalidimagename")
        || contains_ascii_icase(detail, "runcontainererror")
        || contains_ascii_icase(detail, "unhealthy")
        || contains_ascii_icase(detail, "error")
        || contains_ascii_icase(detail, "failed")
        || contains_ascii_icase(detail, "backoff")
        || contains_ascii_icase(detail, "notready")
        || starts_with_ascii_icase(detail, "dead")
        || starts_with_ascii_icase(detail, "restarting")
    {
        return 0;
    }
    if starts_with_ascii_icase(detail, "exited (") && !starts_with_ascii_icase(detail, "exited (0)")
    {
        return 0;
    }
    if starts_with_ascii_icase(detail, "running ")
        && let Some(rest) = detail.strip_prefix("Running ready=")
        && let Some((ratio_tok, _)) = rest.split_once(' ')
        && let Some((actual, desired)) = ratio(ratio_tok)
        && actual == desired
        && actual != 0
    {
        return 2;
    }
    1
}

#[derive(Clone, Copy)]
struct StdoutRef(usize, usize);

impl std::fmt::Display for StdoutRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == self.1 {
            write!(f, "[stdout:{}]", self.0)
        } else {
            write!(f, "[stdout:{}-{}]", self.0, self.1)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct GroupKey {
    namespace: String,
    prefix: String,
    status: Cow<'static, str>,
}

#[derive(Clone, Debug)]
struct HealthyGroup {
    count: usize,
    first: usize,
    last: usize,
    first_name: String,
}

struct ObservedRow<'a> {
    namespace: Option<&'a str>,
    bare_name: &'a str,
    group_fallback: Option<&'a str>,
    healthy_status: Cow<'static, str>,
    issue: Option<Cow<'a, str>>,
}

#[derive(Default)]
struct ColIndices {
    name: Option<usize>,
    namespace: Option<usize>,
    service: Option<usize>,
    status: Option<usize>,
    ready: Option<usize>,
    restarts: Option<usize>,
    desired: Option<usize>,
    current: Option<usize>,
    updated: Option<usize>,
    available: Option<usize>,
    completions: Option<usize>,
}

pub struct Inventory {
    kind: Profile,
    header: Vec<(String, usize)>,
    cols: ColIndices,
    count: usize,
    examples: Vec<String>,
    healthy_groups: BTreeMap<GroupKey, HealthyGroup>,
    issues: Vec<Item>,
}

impl Inventory {
    pub fn new(kind: Profile) -> Self {
        Self {
            kind,
            header: Vec::new(),
            cols: ColIndices::default(),
            count: 0,
            examples: Vec::with_capacity(4),
            healthy_groups: BTreeMap::new(),
            issues: Vec::new(),
        }
    }

    fn cache_cols(&mut self) {
        let name_col = if self.kind == Profile::Containers {
            "NAMES"
        } else {
            "NAME"
        };
        self.cols = ColIndices {
            name: col_index(&self.header, name_col),
            namespace: col_index(&self.header, "NAMESPACE"),
            service: col_index(&self.header, "SERVICE"),
            status: col_index(&self.header, "STATUS"),
            ready: col_index(&self.header, "READY"),
            restarts: col_index(&self.header, "RESTARTS"),
            desired: col_index(&self.header, "DESIRED"),
            current: col_index(&self.header, "CURRENT"),
            updated: col_index(&self.header, "UP-TO-DATE"),
            available: col_index(&self.header, "AVAILABLE"),
            completions: col_index(&self.header, "COMPLETIONS"),
        };
    }

    pub fn observe(&mut self, line: &str, n: usize) -> bool {
        let clean = strip_ansi(line);
        let line = clean.trim_end();
        if line.is_empty() {
            return true;
        }
        if self.header.is_empty() {
            if matches!(self.kind, Profile::Containers | Profile::ComposeContainers) {
                self.header = parse_header(line);
                self.cache_cols();
                return match self.kind {
                    Profile::Containers => {
                        self.header
                            .first()
                            .is_some_and(|(h, _)| h == "CONTAINER ID")
                            && self.cols.status.is_some()
                            && self.cols.name.is_some()
                    }
                    Profile::ComposeContainers => {
                        self.header.first().is_some_and(|(h, _)| h == "NAME")
                            && self.cols.service.is_some()
                            && self.cols.status.is_some()
                    }
                    _ => false,
                };
            }
            self.header = line
                .split_ascii_whitespace()
                .map(|s| (s.to_owned(), 0))
                .collect();
            self.cache_cols();
            let columns: &[&str] = match self.kind {
                Profile::Pods => &["NAME", "READY", "STATUS", "RESTARTS"],
                Profile::Deployments => &["NAME", "READY", "UP-TO-DATE", "AVAILABLE"],
                Profile::StatefulSets => &["NAME", "READY"],
                Profile::DaemonSets => &[
                    "NAME",
                    "DESIRED",
                    "CURRENT",
                    "READY",
                    "UP-TO-DATE",
                    "AVAILABLE",
                ],
                Profile::ReplicaSets => &["NAME", "DESIRED", "CURRENT", "READY"],
                Profile::Jobs => &["NAME", "COMPLETIONS"],
                Profile::Nodes => &["NAME", "STATUS"],
                _ => return false,
            };
            return columns.iter().all(|h| self.col(h).is_some());
        }
        let row = if matches!(self.kind, Profile::Containers | Profile::ComposeContainers) {
            self.container(line)
        } else {
            self.kubernetes(line)
        };
        let Some(row) = row else {
            return false;
        };
        self.count += 1;
        let full_name = || match row.namespace {
            Some(ns) => format!("{ns}/{}", row.bare_name),
            None => row.bare_name.to_owned(),
        };
        if let Some(reason) = row.issue {
            if self.issues.len() >= 2048 {
                return false;
            }
            let name = full_name();
            self.issues.push(Item::new(&name, reason.as_ref(), n));
        } else {
            if self.examples.len() < 4 {
                self.examples.push(full_name());
            }
            if matches!(
                self.kind,
                Profile::Pods | Profile::Containers | Profile::ComposeContainers
            ) {
                let prefix = replica_prefix(row.bare_name)
                    .or_else(|| row.group_fallback.map(|s| format!("{s} (service)")))
                    .unwrap_or_else(|| row.bare_name.to_owned());
                let key = GroupKey {
                    namespace: row.namespace.unwrap_or("").to_owned(),
                    prefix,
                    status: row.healthy_status,
                };
                self.healthy_groups
                    .entry(key)
                    .and_modify(|group| {
                        group.count += 1;
                        group.first = group.first.min(n);
                        group.last = group.last.max(n);
                    })
                    .or_insert_with(|| HealthyGroup {
                        count: 1,
                        first: n,
                        last: n,
                        first_name: full_name(),
                    });
            }
        }
        true
    }

    fn col(&self, name: &str) -> Option<usize> {
        col_index(&self.header, name)
    }

    fn container<'a>(&self, line: &'a str) -> Option<ObservedRow<'a>> {
        let has_tab = line.contains('\t');
        let field = |col_idx: Option<usize>| -> Option<&'a str> {
            let i = col_idx?;
            if has_tab {
                return line.split('\t').nth(i).map(str::trim);
            }
            let start = self.header[i].1;
            let end = self.header.get(i + 1).map(|(_, end)| *end);
            slice_cols(line, start, end).map(str::trim)
        };
        let name = field(self.cols.name)?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        {
            return None;
        }
        let status = field(self.cols.status)?;
        let mut healthy_status = Cow::Borrowed("");
        let issue = if status.eq_ignore_ascii_case("up") || starts_with_ascii_icase(status, "up ") {
            let issue = status.split_once('(').and_then(|(_, s)| {
                let health = s.trim_end_matches(')');
                (!health.eq_ignore_ascii_case("healthy")).then(|| {
                    if health.bytes().any(|b| b.is_ascii_uppercase()) {
                        Cow::Owned(health.to_ascii_lowercase())
                    } else {
                        Cow::Borrowed(health)
                    }
                })
            });
            if issue.is_none() {
                healthy_status = if contains_ascii_icase(status, "(healthy)") {
                    Cow::Borrowed("Up (healthy)")
                } else {
                    Cow::Borrowed("Up")
                };
            }
            issue
        } else if [
            "exited",
            "created",
            "restarting",
            "dead",
            "removing",
            "removal",
            "paused",
            "oomkilled",
            "error",
            "unhealthy",
            "crashloopbackoff",
        ]
        .iter()
        .any(|s| starts_with_ascii_icase(status, s))
        {
            Some(match status.split_once(')') {
                Some((s, _)) => Cow::Owned(format!("{s})")),
                None => Cow::Borrowed(status),
            })
        } else {
            return None;
        };
        let group_fallback = if self.kind == Profile::ComposeContainers {
            field(self.cols.service).filter(|s| !s.is_empty())
        } else {
            None
        };
        Some(ObservedRow {
            namespace: None,
            bare_name: name,
            group_fallback,
            healthy_status,
            issue,
        })
    }

    fn kubernetes<'a>(&self, line: &'a str) -> Option<ObservedRow<'a>> {
        let mut stack = [""; 16];
        let mut len = 0usize;
        let mut overflow: Option<Vec<&'a str>> = None;
        for tok in line.split_ascii_whitespace() {
            if let Some(vec) = overflow.as_mut() {
                vec.push(tok);
            } else if len < stack.len() {
                stack[len] = tok;
                len += 1;
            } else {
                let mut v = Vec::with_capacity(24);
                v.extend_from_slice(&stack);
                v.push(tok);
                overflow = Some(v);
            }
        }
        let values: &[&'a str] = match overflow.as_deref() {
            Some(v) => v,
            None => &stack[..len],
        };
        let get = |col_idx: Option<usize>| values.get(col_idx?).copied();
        let bare = get(self.cols.name)?;
        let namespace = match self.cols.namespace {
            Some(i) => Some(*values.get(i)?),
            None => None,
        };
        let mut healthy_status = Cow::Borrowed("");
        let issue = match self.kind {
            Profile::Nodes => {
                let s = get(self.cols.status)?;
                (s != "Ready").then_some(Cow::Borrowed(s))
            }
            Profile::Pods => {
                let ready = get(self.cols.ready)?;
                let (actual, desired) = ratio(ready)?;
                let status = get(self.cols.status)?;
                let restarts: usize = get(self.cols.restarts)?.parse().ok()?;
                let ok = status == "Completed"
                    || status == "Succeeded"
                    || (status == "Running" && actual == desired && actual != 0);
                if ok && restarts == 0 {
                    healthy_status = match (status, ready) {
                        ("Running", "1/1") => Cow::Borrowed("1/1 Running"),
                        ("Running", "2/2") => Cow::Borrowed("2/2 Running"),
                        ("Running", "3/3") => Cow::Borrowed("3/3 Running"),
                        ("Running", _) => Cow::Owned(format!("{ready} Running")),
                        ("Completed", _) => Cow::Borrowed("Completed"),
                        ("Succeeded", _) => Cow::Borrowed("Succeeded"),
                        _ => Cow::Owned(status.to_owned()),
                    };
                }
                (!ok || restarts > 0)
                    .then(|| Cow::Owned(format!("{status} ready={ready} restarts={restarts}")))
            }
            Profile::StatefulSets | Profile::Deployments => {
                let ready = get(self.cols.ready)?;
                let (actual, desired) = ratio(ready)?;
                if self.kind == Profile::Deployments {
                    let updated: usize = get(self.cols.updated)?.parse().ok()?;
                    let available: usize = get(self.cols.available)?.parse().ok()?;
                    (actual != desired || updated != desired || available != desired).then(|| {
                        Cow::Owned(format!(
                            "ready={ready} updated={updated} available={available}"
                        ))
                    })
                } else {
                    (actual != desired).then(|| Cow::Owned(format!("ready={ready}")))
                }
            }
            Profile::DaemonSets => {
                let desired: usize = get(self.cols.desired)?.parse().ok()?;
                let current: usize = get(self.cols.current)?.parse().ok()?;
                let ready: usize = get(self.cols.ready)?.parse().ok()?;
                let updated: usize = get(self.cols.updated)?.parse().ok()?;
                let available: usize = get(self.cols.available)?.parse().ok()?;
                (current != desired
                    || ready != desired
                    || updated != desired
                    || available != desired)
                    .then(|| {
                        Cow::Owned(format!(
                            "desired={desired} current={current} ready={ready} updated={updated} available={available}"
                        ))
                    })
            }
            Profile::ReplicaSets => {
                let desired: usize = get(self.cols.desired)?.parse().ok()?;
                let current: usize = get(self.cols.current)?.parse().ok()?;
                let ready: usize = get(self.cols.ready)?.parse().ok()?;
                (current != desired || ready != desired).then(|| {
                    Cow::Owned(format!("desired={desired} current={current} ready={ready}"))
                })
            }
            Profile::Jobs => {
                let completions = get(self.cols.completions)?;
                let (done, total) = ratio(completions)?;
                let status = get(self.cols.status);
                let bad_status = status.is_some_and(|s| s != "Complete");
                (done != total || bad_status).then(|| match status {
                    Some(s) => Cow::Owned(format!("{s} completions={completions}")),
                    None => Cow::Owned(format!("completions={completions}")),
                })
            }
            _ => return None,
        };
        Some(ObservedRow {
            namespace,
            bare_name: bare,
            group_fallback: None,
            healthy_status,
            issue,
        })
    }

    pub fn finish(mut self) -> Summary {
        if self.header.is_empty() {
            return Summary::default();
        }
        // Pin critical CrashLoopBackOff/ImagePullBackOff/OOMKilled/Error/Unhealthy rows first,
        // preserving exact input line order within each severity tier.
        self.issues
            .sort_by_key(|item| issue_severity_tier(&item.detail));

        let mut outcomes =
            Vec::with_capacity(2 + self.healthy_groups.len().min(MAX_HEALTHY_GROUP_LINES) + 2);
        outcomes.push(format!(
            "{}: {} listed, {} need attention",
            self.kind.label(),
            self.count,
            self.issues.len()
        ));
        let has_replica_groups = self.healthy_groups.values().any(|g| g.count >= 2);
        if has_replica_groups {
            let mut multi: Vec<(&GroupKey, &HealthyGroup)> = self
                .healthy_groups
                .iter()
                .filter(|(_, g)| g.count >= 2)
                .collect();
            multi.sort_by(|(ka, ga), (kb, gb)| gb.count.cmp(&ga.count).then_with(|| ka.cmp(kb)));

            let mut singles: Vec<&HealthyGroup> = self
                .healthy_groups
                .values()
                .filter(|g| g.count == 1)
                .collect();
            singles.sort_by_key(|g| g.first);

            outcomes.push("Healthy groups:".to_owned());
            for (key, group) in multi.iter().take(MAX_HEALTHY_GROUP_LINES) {
                let mut line = String::with_capacity(64);
                line.push_str("  ");
                if !key.namespace.is_empty() {
                    line.push_str(&key.namespace);
                    line.push('/');
                }
                let _ = write!(
                    line,
                    "{} ×{} · {} {}",
                    key.prefix,
                    group.count,
                    key.status,
                    stdout_ref(group.first, group.last)
                );
                outcomes.push(line);
            }
            if multi.len() > MAX_HEALTHY_GROUP_LINES {
                let rest = &multi[MAX_HEALTHY_GROUP_LINES..];
                let total_rows: usize = rest.iter().map(|(_, g)| g.count).sum();
                let first = rest.iter().map(|(_, g)| g.first).min().unwrap_or(1);
                let last = rest.iter().map(|(_, g)| g.last).max().unwrap_or(first);
                outcomes.push(format!(
                    "  +{} more healthy group(s) ({total_rows} listed) {}",
                    rest.len(),
                    stdout_ref(first, last)
                ));
            }
            if !singles.is_empty() {
                let first = singles.iter().map(|g| g.first).min().unwrap_or(1);
                let last = singles.iter().map(|g| g.last).max().unwrap_or(first);
                let mut line = String::with_capacity(80);
                let _ = write!(line, "  +{} other healthy: ", singles.len());
                for (idx, g) in singles.iter().take(3).enumerate() {
                    if idx > 0 {
                        line.push_str(", ");
                    }
                    line.push_str(&g.first_name);
                }
                if singles.len() > 3 {
                    line.push_str(", …");
                }
                let _ = write!(line, " {}", stdout_ref(first, last));
                outcomes.push(line);
            }
        } else if !self.examples.is_empty() {
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

/// Extract a wildcard replica/pod prefix (`api-7f9b8c-*`, `redis-*`, `shop-worker-*`,
/// `shop_worker_*`) when `name` ends with a generated pod/replica suffix.
fn replica_prefix(name: &str) -> Option<String> {
    if let Some((base, suffix)) = name.rsplit_once('-')
        && !base.is_empty()
        && is_dash_replica_suffix(base, suffix)
    {
        return Some(format!("{base}-*"));
    }
    if let Some((base, suffix)) = name.rsplit_once('_')
        && !base.is_empty()
        && !suffix.is_empty()
        && suffix.bytes().all(|b| b.is_ascii_digit())
    {
        return Some(format!("{base}_*"));
    }
    None
}

fn is_dash_replica_suffix(base: &str, suffix: &str) -> bool {
    if suffix.is_empty() {
        return false;
    }
    // StatefulSet / Compose / numeric replica ordinal (`redis-0`, `shop-worker-12`).
    if suffix.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    let all_lower_alnum = suffix
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !all_lower_alnum {
        return false;
    }
    let has_digit = suffix.bytes().any(|b| b.is_ascii_digit());
    // Exact 5-char Kubernetes safe-alphanumeric random suffix (no vowels or 0/1/3).
    if suffix.len() == 5
        && suffix
            .bytes()
            .all(|b| b"bcdfghjklmnpqrstvwxz2456789".contains(&b))
    {
        return true;
    }
    // 5-char lowercase alphanumeric suffix containing a digit (pod hash like `x9k2p`, `a1b2c`).
    if suffix.len() == 5 && has_digit {
        return true;
    }
    // 4..=12 char lowercase hex hash with at least one digit (container/pod hash).
    if (4..=12).contains(&suffix.len())
        && has_digit
        && suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return true;
    }
    // Synthetic Deployment pod suffix after a ReplicaSet hash (`api-7f9b8c-abcde`).
    if (4..=6).contains(&suffix.len()) && ends_with_hash_segment(base) {
        return true;
    }
    false
}

fn ends_with_hash_segment(base: &str) -> bool {
    let Some((prefix, seg)) = base.rsplit_once('-') else {
        return false;
    };
    !prefix.is_empty()
        && (4..=10).contains(&seg.len())
        && seg
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && seg.bytes().any(|b| b.is_ascii_digit())
}

fn parse_header(line: &str) -> Vec<(String, usize)> {
    COLUMN
        .find_iter(line)
        .map(|m| {
            let char_start = if line.is_ascii() {
                m.start()
            } else {
                line[..m.start()].chars().count()
            };
            (m.as_str().to_owned(), char_start)
        })
        .collect()
}

fn col_index(header: &[(String, usize)], name: &str) -> Option<usize> {
    header.iter().position(|(label, _)| label == name)
}

#[inline]
fn stdout_ref(first: usize, last: usize) -> StdoutRef {
    StdoutRef(first, last)
}

fn fit_smaller(report: String, raw_len: usize, max_bytes: usize) -> Option<String> {
    (max_bytes > 0 && report.len() < raw_len && report.len() <= max_bytes).then_some(report)
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

/// True when character column `start_col` lands cleanly on a boundary (start of
/// line, past line end, or immediately preceded by whitespace).
#[inline]
fn clean_col_boundary(line: &str, start_col: usize) -> bool {
    if start_col == 0 {
        return true;
    }
    if line.is_ascii() {
        return line
            .as_bytes()
            .get(start_col - 1)
            .is_none_or(|b| b.is_ascii_whitespace());
    }
    match line.chars().nth(start_col - 1) {
        None => true,
        Some(ch) => ch.is_whitespace(),
    }
}

fn slice_header_col<'a>(line: &'a str, header: &[(String, usize)], idx: usize) -> Option<&'a str> {
    let start = header.get(idx)?.1;
    let end = header.get(idx + 1).map(|(_, col)| *col);
    if !clean_col_boundary(line, start) {
        return None;
    }
    if let Some(end_col) = end
        && !clean_col_boundary(line, end_col)
    {
        return None;
    }
    Some(slice_cols(line, start, end).map_or("", str::trim))
}

fn ratio(value: &str) -> Option<(usize, usize)> {
    let (a, b) = value.split_once('/')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Format `kubectl get events` output: keep 100% of `Warning` / non-Normal
/// events verbatim (pinning critical crash/pull/OOM/error/unhealthy warnings
/// first and deduplicating identical `(object, reason, message)` with counts
/// and `[stdout:...]` pointers) and fold routine `Normal` events by reason
/// with counts and exact archive line ranges.
pub fn events(text: &str, max_bytes: usize) -> Option<String> {
    if max_bytes == 0 || text.is_empty() {
        return None;
    }
    let clean = strip_ansi(text);
    let mut lines = clean.lines().enumerate();
    let mut header = Vec::new();
    for (_, raw) in &mut lines {
        let trimmed = raw.trim_end_matches('\r').trim_end();
        if trimmed.is_empty() {
            continue;
        }
        header = parse_header(trimmed);
        break;
    }
    let type_idx = col_index(&header, "TYPE")?;
    let reason_idx = col_index(&header, "REASON")?;
    let object_idx = col_index(&header, "OBJECT")?;
    let message_idx = col_index(&header, "MESSAGE")?;
    let ns_idx = col_index(&header, "NAMESPACE");
    let has_last_seen = col_index(&header, "LAST SEEN").is_some();

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct WarnKey<'a> {
        namespace: &'a str,
        kind: &'a str,
        object: &'a str,
        reason: &'a str,
        message: &'a str,
    }
    struct WarnGroup<'a> {
        key: WarnKey<'a>,
        count: usize,
        first: usize,
        last: usize,
    }
    struct NormalGroup {
        count: usize,
        first: usize,
        last: usize,
    }

    let event_warning_tier = |key: &WarnKey<'_>| -> u8 {
        if !key.kind.eq_ignore_ascii_case("warning")
            || contains_ascii_icase(key.reason, "crashloopbackoff")
            || contains_ascii_icase(key.reason, "imagepullbackoff")
            || contains_ascii_icase(key.reason, "errimagepull")
            || contains_ascii_icase(key.reason, "oomkill")
            || contains_ascii_icase(key.reason, "backoff")
            || contains_ascii_icase(key.reason, "unhealthy")
            || contains_ascii_icase(key.reason, "failed")
            || contains_ascii_icase(key.reason, "error")
            || contains_ascii_icase(key.message, "oomkill")
            || contains_ascii_icase(key.message, "crashloopbackoff")
            || contains_ascii_icase(key.message, "imagepullbackoff")
        {
            0
        } else {
            1
        }
    };

    let mut total = 0usize;
    let mut warn_total = 0usize;
    let mut normal_total = 0usize;
    let mut warn_index: BTreeMap<WarnKey<'_>, usize> = BTreeMap::new();
    let mut warnings: Vec<WarnGroup<'_>> = Vec::new();
    let mut normals: BTreeMap<&str, NormalGroup> = BTreeMap::new();

    for (idx, raw) in lines {
        let line_no = idx + 1;
        let line = raw.trim_end_matches('\r').trim_end();
        if line.is_empty() {
            continue;
        }
        let parsed = parse_event_row(
            line,
            &header,
            ns_idx,
            has_last_seen,
            type_idx,
            reason_idx,
            object_idx,
            message_idx,
        )?;
        total += 1;
        if parsed.event_type == "Normal" {
            normal_total += 1;
            normals
                .entry(parsed.reason)
                .and_modify(|g| {
                    g.count += 1;
                    g.first = g.first.min(line_no);
                    g.last = g.last.max(line_no);
                })
                .or_insert(NormalGroup {
                    count: 1,
                    first: line_no,
                    last: line_no,
                });
        } else {
            warn_total += 1;
            let key = WarnKey {
                namespace: parsed.namespace,
                kind: parsed.event_type,
                object: parsed.object,
                reason: parsed.reason,
                message: parsed.message,
            };
            if let Some(&pos) = warn_index.get(&key) {
                let g = &mut warnings[pos];
                g.count += 1;
                g.first = g.first.min(line_no);
                g.last = g.last.max(line_no);
            } else {
                warn_index.insert(key, warnings.len());
                warnings.push(WarnGroup {
                    key,
                    count: 1,
                    first: line_no,
                    last: line_no,
                });
            }
        }
    }

    if total == 0 {
        return None;
    }

    // Pin critical Error/OOM/CrashLoop/ImagePull/BackOff/Unhealthy/Failed warnings first.
    warnings.sort_by_key(|w| event_warning_tier(&w.key));

    let mut head = String::with_capacity(96 + warnings.len().saturating_mul(112));
    if warn_total > warnings.len() {
        let _ = writeln!(
            head,
            "kubectl get events: {total} listed, {warn_total} warning/non-normal ({} unique), {normal_total} normal",
            warnings.len()
        );
    } else {
        let _ = writeln!(
            head,
            "kubectl get events: {total} listed, {warn_total} warning/non-normal, {normal_total} normal"
        );
    }
    for w in &warnings {
        let _ = write!(head, "{} {} ", w.key.kind, w.key.reason);
        if !w.key.namespace.is_empty() {
            head.push_str(w.key.namespace);
            head.push(' ');
        }
        head.push_str(w.key.object);
        if w.count > 1 {
            let _ = write!(head, " ×{}", w.count);
        }
        let _ = writeln!(head, ": {} {}", w.key.message, stdout_ref(w.first, w.last));
    }

    let mut normal_list: Vec<(&str, &NormalGroup)> = normals.iter().map(|(&r, g)| (r, g)).collect();
    normal_list.sort_by(|(ra, ga), (rb, gb)| gb.count.cmp(&ga.count).then_with(|| ra.cmp(rb)));

    // Try full multi-line Normal breakdown first.
    let mut full = String::with_capacity(head.len() + normal_list.len().saturating_mul(40) + 24);
    full.push_str(&head);
    if !normal_list.is_empty() {
        full.push_str("Normal events:\n");
        for (reason, g) in &normal_list {
            let _ = writeln!(
                full,
                "  {reason} ×{} {}",
                g.count,
                stdout_ref(g.first, g.last)
            );
        }
    }
    if let Some(out) = fit_smaller(full, text.len(), max_bytes) {
        return Some(out);
    }

    // Fallback when tight on `max_bytes`: fold Normal reasons onto one compact line
    // while preserving 100% of warnings verbatim.
    if !normal_list.is_empty() {
        let n_first = normal_list.iter().map(|(_, g)| g.first).min().unwrap_or(1);
        let n_last = normal_list
            .iter()
            .map(|(_, g)| g.last)
            .max()
            .unwrap_or(n_first);
        let range = stdout_ref(n_first, n_last);
        let mut compact = String::with_capacity(head.len() + normal_list.len() * 18 + 48);
        compact.push_str(&head);
        let _ = write!(compact, "Normal events ({normal_total}): ");
        for (idx, (r, g)) in normal_list.iter().enumerate() {
            if idx > 0 {
                compact.push_str(", ");
            }
            let _ = write!(compact, "{r} ×{}", g.count);
        }
        let _ = writeln!(compact, " {range}");
        if let Some(out) = fit_smaller(compact, text.len(), max_bytes) {
            return Some(out);
        }

        let mut minimal = head;
        let _ = writeln!(
            minimal,
            "Normal events: {normal_total} across {} reason(s) {range}",
            normal_list.len()
        );
        return fit_smaller(minimal, text.len(), max_bytes);
    }

    None
}

struct EventRow<'a> {
    namespace: &'a str,
    event_type: &'a str,
    reason: &'a str,
    object: &'a str,
    message: &'a str,
}

#[allow(clippy::too_many_arguments)]
fn parse_event_row<'a>(
    line: &'a str,
    header: &[(String, usize)],
    ns_idx: Option<usize>,
    has_last_seen: bool,
    type_idx: usize,
    reason_idx: usize,
    object_idx: usize,
    message_idx: usize,
) -> Option<EventRow<'a>> {
    // Primary path: fixed-width column slicing.
    if let (Some(event_type), Some(reason), Some(object), Some(message)) = (
        slice_header_col(line, header, type_idx),
        slice_header_col(line, header, reason_idx),
        slice_header_col(line, header, object_idx),
        slice_header_col(line, header, message_idx),
    ) {
        let namespace = match ns_idx {
            Some(i) => slice_header_col(line, header, i)?,
            None => "",
        };
        if valid_event_tokens(event_type, reason, object) && !message.is_empty() {
            return Some(EventRow {
                namespace,
                event_type,
                reason,
                object,
                message,
            });
        }
    }

    // Fallback: handle rows where long `LAST SEEN` (`3m (x12 over 15m)`) or `OBJECT`
    // overflows chunk-1 column widths, or loosely space-separated tables.
    let mut rest = line.trim_start();
    let namespace = if ns_idx.is_some() {
        let (tok, tail) = split_first_token(rest)?;
        rest = tail;
        tok
    } else {
        ""
    };
    if has_last_seen {
        let (_dur, tail) = split_first_token(rest)?;
        rest = tail;
        // Skip optional `(xN over ...)` parenthetical in `LAST SEEN`.
        if rest.starts_with('(') {
            let close = rest.find(')')?;
            rest = rest[close + 1..].trim_start();
        }
    }
    let (event_type, tail) = split_first_token(rest)?;
    let (reason, tail) = split_first_token(tail)?;
    let (object, message) = split_first_token(tail)?;
    let message = message.trim();
    if !valid_event_tokens(event_type, reason, object) || message.is_empty() {
        return None;
    }
    Some(EventRow {
        namespace,
        event_type,
        reason,
        object,
        message,
    })
}

fn valid_event_tokens(event_type: &str, reason: &str, object: &str) -> bool {
    !event_type.is_empty()
        && !event_type.contains(char::is_whitespace)
        && event_type
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic())
        && !reason.is_empty()
        && !reason.contains(char::is_whitespace)
        && !object.is_empty()
        && !object.contains(char::is_whitespace)
}

fn split_first_token(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    match s.find(char::is_whitespace) {
        Some(pos) => Some((&s[..pos], s[pos..].trim_start())),
        None => Some((s, "")),
    }
}

/// Format `helm list` / `helm ls` output: highlight every non-`deployed`
/// release (`failed`, `pending-upgrade`, `pending-rollback`, `pending-install`,
/// `uninstalling`, etc.) verbatim with exact `[stdout:line]`, and summarize
/// healthy `deployed` releases compactly by namespace and chart.
pub fn helm_list(text: &str, max_bytes: usize) -> Option<String> {
    if max_bytes == 0 || text.is_empty() {
        return None;
    }
    let clean = strip_ansi(text);
    let mut lines = clean.lines().enumerate();
    let mut header_raw = "";
    for (_, raw) in &mut lines {
        let trimmed = raw.trim_end_matches('\r').trim_end();
        if trimmed.is_empty() {
            continue;
        }
        header_raw = trimmed;
        break;
    }
    if header_raw.is_empty() {
        return None;
    }
    let tab_mode = header_raw.contains('\t');
    let multi_tab = header_raw.contains("\t\t");
    let header: Vec<(String, usize)> = if tab_mode {
        split_tabs(header_raw, multi_tab)
            .into_iter()
            .map(|h| (h.to_owned(), 0))
            .collect()
    } else {
        parse_header(header_raw)
    };

    let name_idx = col_index(&header, "NAME")?;
    let ns_idx = col_index(&header, "NAMESPACE")?;
    let rev_idx = col_index(&header, "REVISION")?;
    let status_idx = col_index(&header, "STATUS")?;
    let chart_idx = col_index(&header, "CHART")?;
    let app_idx = col_index(&header, "APP VERSION");

    struct AttentionRelease<'a> {
        namespace: &'a str,
        name: &'a str,
        status: &'a str,
        revision: &'a str,
        chart: &'a str,
        app_version: &'a str,
        line: usize,
    }
    struct NsDeployed<'a> {
        count: usize,
        first: usize,
        last: usize,
        charts: BTreeMap<&'a str, usize>,
    }

    let mut total = 0usize;
    let mut attention: Vec<AttentionRelease<'_>> = Vec::new();
    let mut deployed_by_ns: BTreeMap<&str, NsDeployed<'_>> = BTreeMap::new();

    for (idx, raw) in lines {
        let line_no = idx + 1;
        let line = raw.trim_end_matches('\r').trim_end();
        if line.is_empty() {
            continue;
        }
        let row = parse_helm_row(
            line, &header, tab_mode, multi_tab, name_idx, ns_idx, rev_idx, status_idx, chart_idx,
            app_idx,
        )?;
        total += 1;
        if row.status.eq_ignore_ascii_case("deployed") {
            deployed_by_ns
                .entry(row.namespace)
                .and_modify(|g| {
                    g.count += 1;
                    g.first = g.first.min(line_no);
                    g.last = g.last.max(line_no);
                    *g.charts.entry(row.chart).or_insert(0) += 1;
                })
                .or_insert_with(|| {
                    let mut charts = BTreeMap::new();
                    charts.insert(row.chart, 1);
                    NsDeployed {
                        count: 1,
                        first: line_no,
                        last: line_no,
                        charts,
                    }
                });
        } else {
            attention.push(AttentionRelease {
                namespace: row.namespace,
                name: row.name,
                status: row.status,
                revision: row.revision,
                chart: row.chart,
                app_version: row.app_version,
                line: line_no,
            });
        }
    }

    if total == 0 {
        return None;
    }

    let mut head = String::with_capacity(64 + attention.len().saturating_mul(84));
    if attention.is_empty() {
        let _ = writeln!(head, "helm list: {total} listed, all deployed");
    } else {
        let _ = writeln!(
            head,
            "helm list: {total} listed, {} need attention",
            attention.len()
        );
    }
    for item in &attention {
        let _ = write!(
            head,
            "{}/{} {} rev={} chart={}",
            item.namespace, item.name, item.status, item.revision, item.chart
        );
        if !item.app_version.is_empty() {
            let _ = write!(head, " app={}", item.app_version);
        }
        let _ = writeln!(head, " {}", stdout_ref(item.line, item.line));
    }

    let deployed_total: usize = deployed_by_ns.values().map(|g| g.count).sum();
    if deployed_total == 0 {
        return fit_smaller(head, text.len(), max_bytes);
    }

    let mut ns_list: Vec<(&str, &NsDeployed<'_>)> =
        deployed_by_ns.iter().map(|(&ns, g)| (ns, g)).collect();
    ns_list.sort_by(|(na, ga), (nb, gb)| gb.count.cmp(&ga.count).then_with(|| na.cmp(nb)));

    let mut full = String::with_capacity(head.len() + ns_list.len().saturating_mul(80) + 24);
    full.push_str(&head);
    full.push_str("Deployed releases:\n");
    for &(ns, g) in &ns_list {
        let mut charts: Vec<(&str, usize)> = g.charts.iter().map(|(&c, &n)| (c, n)).collect();
        charts.sort_by(|(ca, na), (cb, nb)| nb.cmp(na).then_with(|| ca.cmp(cb)));
        let _ = write!(full, "  {ns} ({} deployed): ", g.count);
        for (idx, &(chart, count)) in charts.iter().take(4).enumerate() {
            if idx > 0 {
                full.push_str(", ");
            }
            if count > 1 {
                let _ = write!(full, "{chart} ×{count}");
            } else {
                full.push_str(chart);
            }
        }
        if charts.len() > 4 {
            let _ = write!(full, " +{} more", charts.len() - 4);
        }
        let _ = writeln!(full, " {}", stdout_ref(g.first, g.last));
    }
    if let Some(out) = fit_smaller(full, text.len(), max_bytes) {
        return Some(out);
    }

    // Compact fallback for tight `max_bytes`: one line summarizing all deployed namespaces.
    let first = ns_list.iter().map(|(_, g)| g.first).min().unwrap_or(1);
    let last = ns_list.iter().map(|(_, g)| g.last).max().unwrap_or(first);
    let mut compact = String::with_capacity(head.len() + 128);
    compact.push_str(&head);
    let _ = write!(compact, "Deployed ({deployed_total}): ");
    for (idx, &(ns, g)) in ns_list.iter().take(6).enumerate() {
        if idx > 0 {
            compact.push_str(", ");
        }
        let _ = write!(compact, "{ns} ×{}", g.count);
    }
    if ns_list.len() > 6 {
        let _ = write!(compact, " +{} more ns", ns_list.len() - 6);
    }
    let _ = writeln!(compact, " {}", stdout_ref(first, last));
    if let Some(out) = fit_smaller(compact, text.len(), max_bytes) {
        return Some(out);
    }

    let mut minimal = head;
    let _ = writeln!(
        minimal,
        "Deployed: {deployed_total} release(s) across {} namespace(s) {}",
        ns_list.len(),
        stdout_ref(first, last)
    );
    fit_smaller(minimal, text.len(), max_bytes)
}

struct HelmRow<'a> {
    namespace: &'a str,
    name: &'a str,
    revision: &'a str,
    status: &'a str,
    chart: &'a str,
    app_version: &'a str,
}

fn split_tabs(line: &str, multi_tab: bool) -> Vec<&str> {
    if multi_tab {
        line.split('\t')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        line.split('\t').map(str::trim).collect()
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_helm_row<'a>(
    line: &'a str,
    header: &[(String, usize)],
    tab_mode: bool,
    multi_tab: bool,
    name_idx: usize,
    ns_idx: usize,
    rev_idx: usize,
    status_idx: usize,
    chart_idx: usize,
    app_idx: Option<usize>,
) -> Option<HelmRow<'a>> {
    let valid = |name: &str, ns: &str, rev: &str, status: &str, chart: &str| {
        !name.is_empty()
            && !name.contains(char::is_whitespace)
            && !ns.is_empty()
            && !ns.contains(char::is_whitespace)
            && !rev.is_empty()
            && rev.bytes().all(|b| b.is_ascii_digit())
            && !status.is_empty()
            && !status.contains(char::is_whitespace)
            && !chart.is_empty()
            && !chart.contains(char::is_whitespace)
    };

    if tab_mode {
        let cells = split_tabs(line, multi_tab);
        let name = *cells.get(name_idx)?;
        let ns = *cells.get(ns_idx)?;
        let rev = *cells.get(rev_idx)?;
        let status = *cells.get(status_idx)?;
        let chart = *cells.get(chart_idx)?;
        let app = app_idx.and_then(|i| cells.get(i).copied()).unwrap_or("");
        return valid(name, ns, rev, status, chart).then_some(HelmRow {
            namespace: ns,
            name,
            revision: rev,
            status,
            chart,
            app_version: app,
        });
    }

    // Primary path: fixed-width column slicing.
    if let (Some(name), Some(ns), Some(rev), Some(status), Some(chart)) = (
        slice_header_col(line, header, name_idx),
        slice_header_col(line, header, ns_idx),
        slice_header_col(line, header, rev_idx),
        slice_header_col(line, header, status_idx),
        slice_header_col(line, header, chart_idx),
    ) && valid(name, ns, rev, status, chart)
    {
        let app = app_idx
            .and_then(|i| slice_header_col(line, header, i))
            .unwrap_or("");
        return Some(HelmRow {
            namespace: ns,
            name,
            revision: rev,
            status,
            chart,
            app_version: app,
        });
    }

    // Fallback: split on 2+ spaces (`COLUMN` preserves spaces inside `UPDATED`).
    let cells: Vec<&str> = COLUMN.find_iter(line).map(|m| m.as_str()).collect();
    if cells.len() == header.len() || (app_idx.is_some() && cells.len() + 1 == header.len()) {
        let name = *cells.get(name_idx)?;
        let ns = *cells.get(ns_idx)?;
        let rev = *cells.get(rev_idx)?;
        let status = *cells.get(status_idx)?;
        let chart = *cells.get(chart_idx)?;
        let app = app_idx.and_then(|i| cells.get(i).copied()).unwrap_or("");
        if valid(name, ns, rev, status, chart) {
            return Some(HelmRow {
                namespace: ns,
                name,
                revision: rev,
                status,
                chart,
                app_version: app,
            });
        }
    }
    None
}

/// Format `docker images` / `podman images` output: group dangling
/// `<none>:<none>` images (showing count, total size and `[stdout:A-B]`), and
/// group multi-tag repositories compactly while keeping exact image IDs and
/// sizes retrievable via `[stdout:...]`.
pub fn images(text: &str, max_bytes: usize) -> Option<String> {
    if max_bytes == 0 || text.is_empty() {
        return None;
    }
    let clean = strip_ansi(text);
    let mut lines = clean.lines().enumerate();
    let mut header = Vec::new();
    for (_, raw) in &mut lines {
        let trimmed = raw.trim_end_matches('\r').trim_end();
        if trimmed.is_empty() {
            continue;
        }
        header = parse_header(trimmed);
        break;
    }
    let repo_idx = col_index(&header, "REPOSITORY")?;
    let tag_idx = col_index(&header, "TAG")?;
    let id_idx = col_index(&header, "IMAGE ID")?;
    let size_idx = col_index(&header, "SIZE")?;
    let digest_idx = col_index(&header, "DIGEST");

    struct RepoGroup<'a> {
        tags: Vec<&'a str>,
        ids: BTreeSet<&'a str>,
        first_id: &'a str,
        first_size_raw: Cow<'a, str>,
        unique_bytes: BTreeMap<&'a str, u64>,
        count: usize,
        first: usize,
        last: usize,
    }

    let mut total = 0usize;
    let mut all_unique_bytes: BTreeMap<&str, u64> = BTreeMap::new();
    let mut dangling_count = 0usize;
    let mut dangling_first = usize::MAX;
    let mut dangling_last = 0usize;
    let mut dangling_ids: Vec<(&str, Cow<'_, str>)> = Vec::with_capacity(3);
    let mut dangling_unique_bytes: BTreeMap<&str, u64> = BTreeMap::new();
    let mut repos: BTreeMap<&str, RepoGroup<'_>> = BTreeMap::new();

    for (idx, raw) in lines {
        let line_no = idx + 1;
        let line = raw.trim_end_matches('\r').trim_end();
        if line.is_empty() {
            continue;
        }
        let row = parse_image_row(
            line, &header, repo_idx, tag_idx, digest_idx, id_idx, size_idx,
        )?;
        total += 1;
        if let Some(bytes) = row.size_bytes {
            all_unique_bytes.entry(row.id).or_insert(bytes);
        }
        if row.repo == "<none>" && row.tag == "<none>" {
            dangling_count += 1;
            dangling_first = dangling_first.min(line_no);
            dangling_last = dangling_last.max(line_no);
            if dangling_ids.len() < 3 {
                dangling_ids.push((row.id, row.size_raw));
            }
            if let Some(bytes) = row.size_bytes {
                dangling_unique_bytes.entry(row.id).or_insert(bytes);
            }
        } else {
            repos
                .entry(row.repo)
                .and_modify(|g| {
                    g.count += 1;
                    g.first = g.first.min(line_no);
                    g.last = g.last.max(line_no);
                    g.tags.push(row.tag);
                    g.ids.insert(row.id);
                    if let Some(bytes) = row.size_bytes {
                        g.unique_bytes.entry(row.id).or_insert(bytes);
                    }
                })
                .or_insert_with(|| {
                    let mut ids = BTreeSet::new();
                    ids.insert(row.id);
                    let mut unique_bytes = BTreeMap::new();
                    if let Some(bytes) = row.size_bytes {
                        unique_bytes.insert(row.id, bytes);
                    }
                    RepoGroup {
                        tags: vec![row.tag],
                        ids,
                        first_id: row.id,
                        first_size_raw: row.size_raw,
                        unique_bytes,
                        count: 1,
                        first: line_no,
                        last: line_no,
                    }
                });
        }
    }

    if total == 0 {
        return None;
    }

    let total_bytes: u64 = all_unique_bytes.values().sum();
    let mut head = String::with_capacity(160);
    let _ = writeln!(
        head,
        "docker images: {total} listed ({} repos, {dangling_count} dangling, {})",
        repos.len(),
        format_decimal_bytes(total_bytes)
    );
    if dangling_count > 0 {
        let d_bytes: u64 = dangling_unique_bytes.values().sum();
        if dangling_count == 1 {
            let (id, size_raw) = &dangling_ids[0];
            let _ = writeln!(
                head,
                "dangling (<none>:<none>): 1 image ({id}, {size_raw}) {}",
                stdout_ref(dangling_first, dangling_last)
            );
        } else {
            let _ = write!(
                head,
                "dangling (<none>:<none>): {dangling_count} images, {} (ids: ",
                format_decimal_bytes(d_bytes),
            );
            for (idx, (id, _)) in dangling_ids.iter().enumerate() {
                if idx > 0 {
                    head.push_str(", ");
                }
                head.push_str(id);
            }
            if dangling_count > dangling_ids.len() {
                head.push_str(", …");
            }
            let _ = writeln!(head, ") {}", stdout_ref(dangling_first, dangling_last));
        }
    }

    let mut multi: Vec<(&str, &RepoGroup<'_>)> = repos
        .iter()
        .filter(|(_, g)| g.count >= 2)
        .map(|(&r, g)| (r, g))
        .collect();
    multi.sort_by(|(ra, ga), (rb, gb)| gb.count.cmp(&ga.count).then_with(|| ra.cmp(rb)));

    let mut singles: Vec<(&str, &RepoGroup<'_>)> = repos
        .iter()
        .filter(|(_, g)| g.count == 1)
        .map(|(&r, g)| (r, g))
        .collect();
    singles.sort_by_key(|(_, g)| g.first);

    let write_multi = |dst: &mut String, repo: &str, g: &RepoGroup<'_>| {
        let bytes: u64 = g.unique_bytes.values().sum();
        let _ = write!(dst, "{repo} ({} tags: ", g.count);
        for (idx, tag) in g.tags.iter().take(3).enumerate() {
            if idx > 0 {
                dst.push_str(", ");
            }
            dst.push_str(tag);
        }
        if g.tags.len() > 3 {
            dst.push_str(", …");
        }
        if g.ids.len() == 1 {
            let _ = writeln!(
                dst,
                "; 1 id {}, {}) {}",
                g.first_id,
                format_decimal_bytes(bytes),
                stdout_ref(g.first, g.last)
            );
        } else {
            let _ = writeln!(
                dst,
                "; {} ids, {}) {}",
                g.ids.len(),
                format_decimal_bytes(bytes),
                stdout_ref(g.first, g.last)
            );
        }
    };

    let write_single = |dst: &mut String, repo: &str, g: &RepoGroup<'_>| {
        let _ = writeln!(
            dst,
            "{repo}:{} ({}, {}) {}",
            g.tags[0],
            g.first_id,
            g.first_size_raw,
            stdout_ref(g.first, g.last)
        );
    };

    let sum_unique = |slice: &[(&str, &RepoGroup<'_>)]| -> u64 {
        let mut seen: BTreeMap<&str, u64> = BTreeMap::new();
        for (_, g) in slice {
            for (&id, &bytes) in &g.unique_bytes {
                seen.entry(id).or_insert(bytes);
            }
        }
        seen.values().sum()
    };

    // Pass 1: full multi-tag groups + all single-tag images (up to 16 before folding tail).
    let single_limit = 16usize;
    let mut full = String::with_capacity(
        head.len() + (multi.len() + singles.len().min(single_limit)).saturating_mul(84) + 64,
    );
    full.push_str(&head);
    for &(repo, g) in &multi {
        write_multi(&mut full, repo, g);
    }
    for &(repo, g) in singles.iter().take(single_limit) {
        write_single(&mut full, repo, g);
    }
    if singles.len() > single_limit {
        let rest = &singles[single_limit..];
        let rest_bytes = sum_unique(rest);
        let first = rest.iter().map(|(_, g)| g.first).min().unwrap_or(1);
        let last = rest.iter().map(|(_, g)| g.last).max().unwrap_or(first);
        let _ = writeln!(
            full,
            "+{} other image(s) ({}) {}",
            rest.len(),
            format_decimal_bytes(rest_bytes),
            stdout_ref(first, last)
        );
    }
    if let Some(out) = fit_smaller(full, text.len(), max_bytes) {
        return Some(out);
    }

    // Pass 2: fold all single-tag images into one summary line, keep multi-tag repos within budget.
    let mut compact = head;
    let mut shown_multi = 0usize;
    let singles_summary = if !singles.is_empty() {
        let s_bytes = sum_unique(&singles);
        let first = singles.iter().map(|(_, g)| g.first).min().unwrap_or(1);
        let last = singles.iter().map(|(_, g)| g.last).max().unwrap_or(first);
        format!(
            "+{} single-tag image(s) ({}) {}\n",
            singles.len(),
            format_decimal_bytes(s_bytes),
            stdout_ref(first, last)
        )
    } else {
        String::new()
    };
    let tail_reserve = singles_summary.len() + 64;
    let mut line_buf = String::with_capacity(128);
    for &(repo, g) in &multi {
        line_buf.clear();
        write_multi(&mut line_buf, repo, g);
        if compact.len() + line_buf.len() + tail_reserve <= max_bytes {
            compact.push_str(&line_buf);
            shown_multi += 1;
        } else {
            break;
        }
    }
    if shown_multi < multi.len() {
        let rest = &multi[shown_multi..];
        let rest_tags: usize = rest.iter().map(|(_, g)| g.count).sum();
        let first = rest.iter().map(|(_, g)| g.first).min().unwrap_or(1);
        let last = rest.iter().map(|(_, g)| g.last).max().unwrap_or(first);
        let _ = writeln!(
            compact,
            "+{} more repo(s) ({rest_tags} tags) {}",
            rest.len(),
            stdout_ref(first, last)
        );
    }
    compact.push_str(&singles_summary);
    fit_smaller(compact, text.len(), max_bytes)
}

struct ImageRow<'a> {
    repo: &'a str,
    tag: &'a str,
    id: &'a str,
    size_raw: Cow<'a, str>,
    size_bytes: Option<u64>,
}

fn parse_image_row<'a>(
    line: &'a str,
    header: &[(String, usize)],
    repo_idx: usize,
    tag_idx: usize,
    digest_idx: Option<usize>,
    id_idx: usize,
    size_idx: usize,
) -> Option<ImageRow<'a>> {
    let valid = |repo: &str, tag: &str, id: &str, size: &str| {
        !repo.is_empty()
            && !repo.contains(char::is_whitespace)
            && !tag.is_empty()
            && !tag.contains(char::is_whitespace)
            && !id.is_empty()
            && !id.contains(char::is_whitespace)
            && parse_size_bytes(size).is_some()
    };

    // Primary path: fixed-width column slicing.
    if let (Some(repo), Some(tag), Some(id), Some(size)) = (
        slice_header_col(line, header, repo_idx),
        slice_header_col(line, header, tag_idx),
        slice_header_col(line, header, id_idx),
        slice_header_col(line, header, size_idx),
    ) && valid(repo, tag, id, size)
    {
        return Some(ImageRow {
            repo,
            tag,
            id,
            size_bytes: parse_size_bytes(size),
            size_raw: Cow::Borrowed(size),
        });
    }

    // Fallback: handle long repository names shifting later columns to the right.
    let cells: Vec<&str> = COLUMN.find_iter(line).map(|m| m.as_str()).collect();
    if cells.len() == header.len() {
        let repo = cells[repo_idx];
        let tag = cells[tag_idx];
        let id = cells[id_idx];
        let size = cells[size_idx];
        if valid(repo, tag, id, size) {
            return Some(ImageRow {
                repo,
                tag,
                id,
                size_bytes: parse_size_bytes(size),
                size_raw: Cow::Borrowed(size),
            });
        }
    }

    // Secondary fallback: single-space separated leading tokens + trailing size token(s).
    let tokens: Vec<&str> = line.split_ascii_whitespace().collect();
    let expected_id_tok = if digest_idx.is_some() { 3 } else { 2 };
    if tokens.len() >= expected_id_tok + 2 {
        let repo = tokens[0];
        let tag = tokens[1];
        let id = tokens[expected_id_tok];
        // Handle both `245MB` and Podman's `256 MB`.
        let last_one = *tokens.last()?;
        let size_candidate: Cow<'a, str> = if parse_size_bytes(last_one).is_some() {
            Cow::Borrowed(last_one)
        } else if tokens.len() >= expected_id_tok + 3 {
            Cow::Owned(format!("{} {}", tokens[tokens.len() - 2], last_one))
        } else {
            return None;
        };
        if valid(repo, tag, id, &size_candidate) {
            let size_bytes = parse_size_bytes(&size_candidate);
            return Some(ImageRow {
                repo,
                tag,
                id,
                size_raw: size_candidate,
                size_bytes,
            });
        }
    }
    None
}

fn parse_size_bytes(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let num_end = s
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b'.')
        .count();
    if num_end == 0 {
        return None;
    }
    let val: f64 = s[..num_end].parse().ok()?;
    if !val.is_finite() || val < 0.0 {
        return None;
    }
    let unit = s[num_end..].trim();
    let mult: f64 = match unit {
        "B" | "b" => 1.0,
        "kB" | "KB" | "kb" => 1_000.0,
        "KiB" | "kib" => 1_024.0,
        "MB" | "mb" => 1_000_000.0,
        "MiB" | "mib" => 1_048_576.0,
        "GB" | "gb" => 1_000_000_000.0,
        "GiB" | "gib" => 1_073_741_824.0,
        "TB" | "tb" => 1_000_000_000_000.0,
        "TiB" | "tib" => 1_099_511_627_776.0,
        _ => return None,
    };
    Some((val * mult).round() as u64)
}

fn format_decimal_bytes(bytes: u64) -> String {
    const TB: f64 = 1_000_000_000_000.0;
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    const KB: f64 = 1_000.0;
    let b = bytes as f64;
    if b >= TB {
        format!("{:.2}TB", b / TB)
    } else if b >= GB {
        format!("{:.2}GB", b / GB)
    } else if b >= 100.0 * MB {
        format!("{:.0}MB", b / MB)
    } else if b >= MB {
        format!("{:.1}MB", b / MB)
    } else if b >= KB {
        format!("{:.1}kB", b / KB)
    } else {
        format!("{bytes}B")
    }
}

fn parse_k8s_quantity(raw: &str) -> Option<u64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let num_end = s
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b'.')
        .count();
    if num_end == 0 {
        return None;
    }
    let val: f64 = s[..num_end].parse().ok()?;
    if !val.is_finite() || val < 0.0 {
        return None;
    }
    let mult: f64 = match s[num_end..].trim() {
        "" => 1.0,
        "Ki" => 1024.0,
        "Mi" => 1024.0 * 1024.0,
        "Gi" => 1024.0 * 1024.0 * 1024.0,
        "Ti" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "Pi" => 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "k" | "K" => 1_000.0,
        "M" => 1_000_000.0,
        "G" => 1_000_000_000.0,
        "T" => 1_000_000_000_000.0,
        _ => return None,
    };
    Some((val * mult).round() as u64)
}

fn format_k8s_quantity(bytes: u64) -> String {
    const TI: u64 = 1024 * 1024 * 1024 * 1024;
    const GI: u64 = 1024 * 1024 * 1024;
    const MI: u64 = 1024 * 1024;
    const KI: u64 = 1024;
    if bytes >= TI {
        if bytes.is_multiple_of(TI) {
            format!("{}Ti", bytes / TI)
        } else {
            format!("{:.1}Ti", bytes as f64 / TI as f64)
        }
    } else if bytes >= GI {
        if bytes.is_multiple_of(GI) {
            format!("{}Gi", bytes / GI)
        } else {
            format!("{:.1}Gi", bytes as f64 / GI as f64)
        }
    } else if bytes >= MI {
        if bytes.is_multiple_of(MI) {
            format!("{}Mi", bytes / MI)
        } else {
            format!("{:.1}Mi", bytes as f64 / MI as f64)
        }
    } else if bytes >= KI {
        format!("{}Ki", bytes / KI)
    } else {
        format!("{bytes}B")
    }
}

/// Format `kubectl get` tables for `services`, `ingresses`, `pvcs`, and
/// `cronjobs`: highlight Pending/Lost/Terminating PVCs, suspended CronJobs,
/// pending/external LoadBalancer services, and Ingress endpoints while
/// collapsing uniform healthy rows cleanly with `[stdout:A-B]` links.
pub fn resources(kind: &str, text: &str, max_bytes: usize) -> Option<String> {
    if max_bytes == 0 || text.is_empty() {
        return None;
    }
    let clean = strip_ansi(text);
    let raw_len = text.len();
    let normalized = kind.trim();
    let base = normalized.split('.').next().unwrap_or("");
    if base.eq_ignore_ascii_case("svc")
        || base.eq_ignore_ascii_case("service")
        || base.eq_ignore_ascii_case("services")
    {
        services_table(&clean, raw_len, max_bytes)
    } else if base.eq_ignore_ascii_case("ing")
        || base.eq_ignore_ascii_case("ingress")
        || base.eq_ignore_ascii_case("ingresses")
    {
        ingresses_table(&clean, raw_len, max_bytes)
    } else if base.eq_ignore_ascii_case("pvc")
        || base.eq_ignore_ascii_case("pvcs")
        || base.eq_ignore_ascii_case("persistentvolumeclaim")
        || base.eq_ignore_ascii_case("persistentvolumeclaims")
    {
        pvcs_table(&clean, raw_len, max_bytes)
    } else if base.eq_ignore_ascii_case("cj")
        || base.eq_ignore_ascii_case("cronjob")
        || base.eq_ignore_ascii_case("cronjobs")
    {
        cronjobs_table(&clean, raw_len, max_bytes)
    } else {
        None
    }
}

fn services_table(text: &str, raw_len: usize, max_bytes: usize) -> Option<String> {
    let mut lines = text.lines().enumerate();
    let mut header = Vec::new();
    for (_, raw) in &mut lines {
        let trimmed = raw.trim_end_matches('\r').trim_end();
        if trimmed.is_empty() {
            continue;
        }
        header = trimmed
            .split_ascii_whitespace()
            .map(|s| (s.to_owned(), 0))
            .collect();
        break;
    }
    let name_idx = col_index(&header, "NAME")?;
    let type_idx = col_index(&header, "TYPE")?;
    let ext_idx = col_index(&header, "EXTERNAL-IP")?;
    let ports_idx = col_index(&header, "PORT(S)")?;
    let ns_idx = col_index(&header, "NAMESPACE");

    struct HighlightSvc<'a> {
        pending: bool,
        ns: &'a str,
        name: &'a str,
        svc_type: &'a str,
        external: &'a str,
        ports: &'a str,
        line: usize,
    }
    struct InternalGroup<'a> {
        count: usize,
        first: usize,
        last: usize,
        samples: Vec<&'a str>,
    }

    let mut total = 0usize;
    let mut pending_count = 0usize;
    let mut external_count = 0usize;
    let mut highlights: Vec<HighlightSvc<'_>> = Vec::new();
    let mut internal: BTreeMap<(&str, &str), InternalGroup<'_>> = BTreeMap::new();

    for (idx, raw) in lines {
        let line_no = idx + 1;
        let line = raw.trim_end_matches('\r').trim_end();
        if line.is_empty() {
            continue;
        }
        let values: Vec<&str> = line.split_ascii_whitespace().collect();
        let name = *values.get(name_idx)?;
        let svc_type = *values.get(type_idx)?;
        let external = *values.get(ext_idx)?;
        let ports = *values.get(ports_idx)?;
        let ns = match ns_idx {
            Some(i) => *values.get(i)?,
            None => "",
        };
        total += 1;

        let is_pending = external.eq_ignore_ascii_case("<pending>");
        let is_external = is_pending
            || svc_type == "LoadBalancer"
            || svc_type == "ExternalName"
            || (external != "<none>" && !external.is_empty());

        if is_external {
            if is_pending {
                pending_count += 1;
            } else {
                external_count += 1;
            }
            highlights.push(HighlightSvc {
                pending: is_pending,
                ns,
                name,
                svc_type,
                external,
                ports,
                line: line_no,
            });
        } else {
            internal
                .entry((ns, svc_type))
                .and_modify(|g| {
                    g.count += 1;
                    g.first = g.first.min(line_no);
                    g.last = g.last.max(line_no);
                    if g.samples.len() < 3 {
                        g.samples.push(name);
                    }
                })
                .or_insert_with(|| InternalGroup {
                    count: 1,
                    first: line_no,
                    last: line_no,
                    samples: vec![name],
                });
        }
    }

    if total == 0 {
        return None;
    }

    // Show pending services first, then external endpoints.
    highlights.sort_by_key(|s| (!s.pending, s.line));

    let mut head = String::with_capacity(80 + highlights.len().saturating_mul(84));
    let _ = writeln!(
        head,
        "kubectl get services: {total} listed, {pending_count} pending, {external_count} external"
    );
    for svc in &highlights {
        let tag = if svc.pending { "attention" } else { "external" };
        let _ = write!(head, "{tag}: ");
        if !svc.ns.is_empty() {
            head.push_str(svc.ns);
            head.push('/');
        }
        let _ = write!(
            head,
            "{} {} external={}",
            svc.name, svc.svc_type, svc.external
        );
        if svc.ports != "<none>" {
            let _ = write!(head, " ports={}", svc.ports);
        }
        let _ = writeln!(head, " {}", stdout_ref(svc.line, svc.line));
    }

    if internal.is_empty() {
        return fit_smaller(head, raw_len, max_bytes);
    }

    let mut groups: Vec<((&str, &str), &InternalGroup<'_>)> =
        internal.iter().map(|(&k, g)| (k, g)).collect();
    groups.sort_by(|(ka, ga), (kb, gb)| gb.count.cmp(&ga.count).then_with(|| ka.cmp(kb)));

    let mut full = String::with_capacity(head.len() + groups.len().saturating_mul(72) + 24);
    full.push_str(&head);
    full.push_str("Internal services:\n");
    for &((ns, svc_type), g) in &groups {
        full.push_str("  ");
        if !ns.is_empty() {
            full.push_str(ns);
            full.push_str(": ");
        }
        let _ = write!(full, "{svc_type} ×{} (", g.count);
        for (idx, s) in g.samples.iter().enumerate() {
            if idx > 0 {
                full.push_str(", ");
            }
            full.push_str(s);
        }
        if g.count > g.samples.len() {
            full.push_str(", …");
        }
        let _ = writeln!(full, ") {}", stdout_ref(g.first, g.last));
    }
    if let Some(out) = fit_smaller(full, raw_len, max_bytes) {
        return Some(out);
    }

    let internal_total: usize = groups.iter().map(|(_, g)| g.count).sum();
    let first = groups.iter().map(|(_, g)| g.first).min().unwrap_or(1);
    let last = groups.iter().map(|(_, g)| g.last).max().unwrap_or(first);
    let mut compact = head;
    let _ = writeln!(
        compact,
        "Internal services: {internal_total} across {} group(s) {}",
        groups.len(),
        stdout_ref(first, last)
    );
    fit_smaller(compact, raw_len, max_bytes)
}

fn ingresses_table(text: &str, raw_len: usize, max_bytes: usize) -> Option<String> {
    let mut lines = text.lines().enumerate();
    let mut header = Vec::new();
    for (_, raw) in &mut lines {
        let trimmed = raw.trim_end_matches('\r').trim_end();
        if trimmed.is_empty() {
            continue;
        }
        header = parse_header(trimmed);
        break;
    }
    let name_idx = col_index(&header, "NAME")?;
    let hosts_idx = col_index(&header, "HOSTS")?;
    let addr_idx = col_index(&header, "ADDRESS")?;
    let ports_idx = col_index(&header, "PORTS")?;
    let class_idx = col_index(&header, "CLASS");
    let ns_idx = col_index(&header, "NAMESPACE");

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct IngGroupKey<'a> {
        ns: &'a str,
        class: &'a str,
        address: &'a str,
        ports: &'a str,
    }

    let mut total = 0usize;
    let mut missing_addr: Vec<IngressRow<'_>> = Vec::new();
    let mut by_endpoint: BTreeMap<IngGroupKey<'_>, Vec<IngressRow<'_>>> = BTreeMap::new();

    for (idx, raw) in lines {
        let line_no = idx + 1;
        let line = raw.trim_end_matches('\r').trim_end();
        if line.is_empty() {
            continue;
        }
        let row = parse_ingress_row(
            line, &header, ns_idx, name_idx, class_idx, hosts_idx, addr_idx, ports_idx, line_no,
        )?;
        total += 1;
        if row.address.is_empty()
            || row.address == "<none>"
            || row.address.eq_ignore_ascii_case("<pending>")
        {
            missing_addr.push(row);
        } else {
            let key = IngGroupKey {
                ns: row.ns,
                class: row.class,
                address: row.address,
                ports: row.ports,
            };
            by_endpoint.entry(key).or_default().push(row);
        }
    }

    if total == 0 {
        return None;
    }

    let mut head = String::with_capacity(80 + missing_addr.len().saturating_mul(96));
    let _ = writeln!(
        head,
        "kubectl get ingresses: {total} listed, {} missing address",
        missing_addr.len()
    );
    for ing in &missing_addr {
        head.push_str("attention: ");
        if !ing.ns.is_empty() {
            head.push_str(ing.ns);
            head.push('/');
        }
        head.push_str(ing.name);
        if !ing.class.is_empty() && ing.class != "<none>" {
            let _ = write!(head, " class={}", ing.class);
        }
        let _ = writeln!(
            head,
            " hosts={} address=<none> ports={} {}",
            ing.hosts,
            ing.ports,
            stdout_ref(ing.line, ing.line)
        );
    }

    let mut groups: Vec<(IngGroupKey<'_>, &Vec<IngressRow<'_>>)> =
        by_endpoint.iter().map(|(&k, v)| (k, v)).collect();
    groups.sort_by(|(ka, ra), (kb, rb)| rb.len().cmp(&ra.len()).then_with(|| ka.cmp(kb)));

    let mut full = String::with_capacity(head.len() + groups.len().saturating_mul(96));
    full.push_str(&head);
    for &(key, rows) in &groups {
        if rows.len() <= 3 {
            for ing in rows {
                full.push_str("external: ");
                if !ing.ns.is_empty() {
                    full.push_str(ing.ns);
                    full.push('/');
                }
                let _ = writeln!(
                    full,
                    "{} hosts={} addr={} ports={} {}",
                    ing.name,
                    ing.hosts,
                    ing.address,
                    ing.ports,
                    stdout_ref(ing.line, ing.line)
                );
            }
        } else {
            let first = rows.iter().map(|r| r.line).min().unwrap_or(1);
            let last = rows.iter().map(|r| r.line).max().unwrap_or(first);
            full.push_str("external: ");
            if !key.ns.is_empty() {
                full.push_str(key.ns);
                full.push(' ');
            }
            full.push('(');
            if !key.class.is_empty() && key.class != "<none>" {
                full.push_str(key.class);
                full.push_str(" → ");
            }
            let _ = write!(
                full,
                "{}, ports {}) ×{}: ",
                key.address,
                key.ports,
                rows.len()
            );
            for (idx, r) in rows.iter().take(3).enumerate() {
                if idx > 0 {
                    full.push_str(", ");
                }
                full.push_str(r.hosts);
            }
            let _ = writeln!(full, ", … {}", stdout_ref(first, last));
        }
    }
    if let Some(out) = fit_smaller(full, raw_len, max_bytes) {
        return Some(out);
    }

    let ext_total: usize = groups.iter().map(|(_, r)| r.len()).sum();
    if ext_total > 0 {
        let first = groups
            .iter()
            .flat_map(|(_, r)| r.iter().map(|x| x.line))
            .min()
            .unwrap_or(1);
        let last = groups
            .iter()
            .flat_map(|(_, r)| r.iter().map(|x| x.line))
            .max()
            .unwrap_or(first);
        let mut compact = head;
        let _ = writeln!(
            compact,
            "External ingresses: {ext_total} across {} endpoint group(s) {}",
            groups.len(),
            stdout_ref(first, last)
        );
        return fit_smaller(compact, raw_len, max_bytes);
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn parse_ingress_row<'a>(
    line: &'a str,
    header: &[(String, usize)],
    ns_idx: Option<usize>,
    name_idx: usize,
    class_idx: Option<usize>,
    hosts_idx: usize,
    addr_idx: usize,
    ports_idx: usize,
    line_no: usize,
) -> Option<IngressRow<'a>> {
    if let (Some(name), Some(hosts), Some(address), Some(ports)) = (
        slice_header_col(line, header, name_idx),
        slice_header_col(line, header, hosts_idx),
        slice_header_col(line, header, addr_idx),
        slice_header_col(line, header, ports_idx),
    ) && !name.is_empty()
        && !name.contains(char::is_whitespace)
        && !hosts.is_empty()
        && !ports.is_empty()
    {
        let ns = match ns_idx {
            Some(i) => slice_header_col(line, header, i)?,
            None => "",
        };
        let class = class_idx
            .and_then(|i| slice_header_col(line, header, i))
            .unwrap_or("");
        return Some(IngressRow {
            ns,
            name,
            class,
            hosts,
            address,
            ports,
            line: line_no,
        });
    }
    let cells: Vec<&str> = COLUMN.find_iter(line).map(|m| m.as_str()).collect();
    if cells.len() == header.len() {
        let name = cells[name_idx];
        let hosts = cells[hosts_idx];
        let address = cells[addr_idx];
        let ports = cells[ports_idx];
        let ns = ns_idx.map_or("", |i| cells[i]);
        let class = class_idx.map_or("", |i| cells[i]);
        if !name.is_empty() && !name.contains(char::is_whitespace) && !hosts.is_empty() {
            return Some(IngressRow {
                ns,
                name,
                class,
                hosts,
                address,
                ports,
                line: line_no,
            });
        }
    }
    None
}

struct IngressRow<'a> {
    ns: &'a str,
    name: &'a str,
    class: &'a str,
    hosts: &'a str,
    address: &'a str,
    ports: &'a str,
    line: usize,
}

fn pvcs_table(text: &str, raw_len: usize, max_bytes: usize) -> Option<String> {
    let mut lines = text.lines().enumerate();
    let mut header = Vec::new();
    for (_, raw) in &mut lines {
        let trimmed = raw.trim_end_matches('\r').trim_end();
        if trimmed.is_empty() {
            continue;
        }
        header = parse_header(trimmed);
        break;
    }
    let name_idx = col_index(&header, "NAME")?;
    let status_idx = col_index(&header, "STATUS")?;
    let vol_idx = col_index(&header, "VOLUME")?;
    let cap_idx = col_index(&header, "CAPACITY")?;
    let mode_idx = col_index(&header, "ACCESS MODES");
    let class_idx = col_index(&header, "STORAGECLASS");
    let ns_idx = col_index(&header, "NAMESPACE");

    struct AttentionPvc<'a> {
        ns: &'a str,
        name: &'a str,
        status: &'a str,
        capacity: &'a str,
        storage_class: &'a str,
        volume: &'a str,
        line: usize,
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct BoundKey<'a> {
        ns: &'a str,
        storage_class: &'a str,
        access_modes: &'a str,
    }
    struct BoundGroup<'a> {
        count: usize,
        first: usize,
        last: usize,
        uniform_cap: Option<&'a str>,
        mixed_cap: bool,
        total_bytes: u64,
        all_parsed_bytes: bool,
        samples: Vec<&'a str>,
    }

    let mut total = 0usize;
    let mut attention: Vec<AttentionPvc<'_>> = Vec::new();
    let mut bound: BTreeMap<BoundKey<'_>, BoundGroup<'_>> = BTreeMap::new();

    for (idx, raw) in lines {
        let line_no = idx + 1;
        let line = raw.trim_end_matches('\r').trim_end();
        if line.is_empty() {
            continue;
        }
        let row = parse_pvc_row(
            line, &header, ns_idx, name_idx, status_idx, vol_idx, cap_idx, mode_idx, class_idx,
        )?;
        total += 1;
        if row.status == "Bound" {
            let key = BoundKey {
                ns: row.ns,
                storage_class: if row.storage_class.is_empty() {
                    "<default>"
                } else {
                    row.storage_class
                },
                access_modes: row.access_modes,
            };
            let parsed_bytes = parse_k8s_quantity(row.capacity);
            bound
                .entry(key)
                .and_modify(|g| {
                    g.count += 1;
                    g.first = g.first.min(line_no);
                    g.last = g.last.max(line_no);
                    if g.uniform_cap != Some(row.capacity) {
                        g.mixed_cap = true;
                    }
                    match parsed_bytes {
                        Some(b) => g.total_bytes = g.total_bytes.saturating_add(b),
                        None => g.all_parsed_bytes = false,
                    }
                    if g.samples.len() < 3 {
                        g.samples.push(row.name);
                    }
                })
                .or_insert_with(|| BoundGroup {
                    count: 1,
                    first: line_no,
                    last: line_no,
                    uniform_cap: Some(row.capacity),
                    mixed_cap: false,
                    total_bytes: parsed_bytes.unwrap_or(0),
                    all_parsed_bytes: parsed_bytes.is_some(),
                    samples: vec![row.name],
                });
        } else {
            attention.push(AttentionPvc {
                ns: row.ns,
                name: row.name,
                status: row.status,
                capacity: row.capacity,
                storage_class: row.storage_class,
                volume: row.volume,
                line: line_no,
            });
        }
    }

    if total == 0 {
        return None;
    }

    let mut head = String::with_capacity(64 + attention.len().saturating_mul(84));
    let _ = writeln!(
        head,
        "kubectl get pvc: {total} listed, {} need attention",
        attention.len()
    );
    for pvc in &attention {
        if !pvc.ns.is_empty() {
            head.push_str(pvc.ns);
            head.push('/');
        }
        let _ = write!(head, "{} {}", pvc.name, pvc.status);
        if !pvc.capacity.is_empty() {
            let _ = write!(head, " capacity={}", pvc.capacity);
        }
        if !pvc.storage_class.is_empty() {
            let _ = write!(head, " class={}", pvc.storage_class);
        }
        if !pvc.volume.is_empty() {
            let _ = write!(head, " vol={}", pvc.volume);
        }
        let _ = writeln!(head, " {}", stdout_ref(pvc.line, pvc.line));
    }

    if bound.is_empty() {
        return fit_smaller(head, raw_len, max_bytes);
    }

    let mut groups: Vec<(BoundKey<'_>, &BoundGroup<'_>)> =
        bound.iter().map(|(&k, g)| (k, g)).collect();
    groups.sort_by(|(ka, ga), (kb, gb)| gb.count.cmp(&ga.count).then_with(|| ka.cmp(kb)));

    let mut full = String::with_capacity(head.len() + groups.len().saturating_mul(84) + 24);
    full.push_str(&head);
    full.push_str("Bound PVCs:\n");
    for &(key, g) in &groups {
        full.push_str("  ");
        if !key.ns.is_empty() {
            full.push_str(key.ns);
            full.push_str(": ");
        }
        full.push_str(key.storage_class);
        if !key.access_modes.is_empty() {
            full.push(' ');
            full.push_str(key.access_modes);
        }
        if !g.mixed_cap {
            if let Some(c) = g.uniform_cap.filter(|c| !c.is_empty()) {
                full.push(' ');
                full.push_str(c);
            }
        } else if g.all_parsed_bytes && g.total_bytes > 0 {
            let _ = write!(full, " (total {})", format_k8s_quantity(g.total_bytes));
        }
        let _ = write!(full, " ×{} (", g.count);
        for (idx, s) in g.samples.iter().enumerate() {
            if idx > 0 {
                full.push_str(", ");
            }
            full.push_str(s);
        }
        if g.count > g.samples.len() {
            full.push_str(", …");
        }
        let _ = writeln!(full, ") {}", stdout_ref(g.first, g.last));
    }
    if let Some(out) = fit_smaller(full, raw_len, max_bytes) {
        return Some(out);
    }

    let bound_total: usize = groups.iter().map(|(_, g)| g.count).sum();
    let first = groups.iter().map(|(_, g)| g.first).min().unwrap_or(1);
    let last = groups.iter().map(|(_, g)| g.last).max().unwrap_or(first);
    let mut compact = head;
    let _ = writeln!(
        compact,
        "Bound PVCs: {bound_total} across {} group(s) {}",
        groups.len(),
        stdout_ref(first, last)
    );
    fit_smaller(compact, raw_len, max_bytes)
}

struct PvcRow<'a> {
    ns: &'a str,
    name: &'a str,
    status: &'a str,
    volume: &'a str,
    capacity: &'a str,
    access_modes: &'a str,
    storage_class: &'a str,
}

#[allow(clippy::too_many_arguments)]
fn parse_pvc_row<'a>(
    line: &'a str,
    header: &[(String, usize)],
    ns_idx: Option<usize>,
    name_idx: usize,
    status_idx: usize,
    vol_idx: usize,
    cap_idx: usize,
    mode_idx: Option<usize>,
    class_idx: Option<usize>,
) -> Option<PvcRow<'a>> {
    if let (Some(name), Some(status), Some(vol), Some(cap)) = (
        slice_header_col(line, header, name_idx),
        slice_header_col(line, header, status_idx),
        slice_header_col(line, header, vol_idx),
        slice_header_col(line, header, cap_idx),
    ) && !name.is_empty()
        && !name.contains(char::is_whitespace)
        && !status.is_empty()
        && !status.contains(char::is_whitespace)
    {
        let ns = match ns_idx {
            Some(i) => slice_header_col(line, header, i)?,
            None => "",
        };
        let access_modes = mode_idx
            .and_then(|i| slice_header_col(line, header, i))
            .unwrap_or("");
        let storage_class = class_idx
            .and_then(|i| slice_header_col(line, header, i))
            .unwrap_or("");
        return Some(PvcRow {
            ns,
            name,
            status,
            volume: vol,
            capacity: cap,
            access_modes,
            storage_class,
        });
    }
    let cells: Vec<&str> = COLUMN.find_iter(line).map(|m| m.as_str()).collect();
    if cells.len() == header.len() {
        let name = cells[name_idx];
        let status = cells[status_idx];
        if !name.is_empty()
            && !name.contains(char::is_whitespace)
            && !status.is_empty()
            && !status.contains(char::is_whitespace)
        {
            return Some(PvcRow {
                ns: ns_idx.map_or("", |i| cells[i]),
                name,
                status,
                volume: cells[vol_idx],
                capacity: cells[cap_idx],
                access_modes: mode_idx.map_or("", |i| cells[i]),
                storage_class: class_idx.map_or("", |i| cells[i]),
            });
        }
    }
    None
}

fn cronjobs_table(text: &str, raw_len: usize, max_bytes: usize) -> Option<String> {
    let mut lines = text.lines().enumerate();
    let mut header = Vec::new();
    for (_, raw) in &mut lines {
        let trimmed = raw.trim_end_matches('\r').trim_end();
        if trimmed.is_empty() {
            continue;
        }
        header = parse_header(trimmed);
        break;
    }
    let name_idx = col_index(&header, "NAME")?;
    let sched_idx = col_index(&header, "SCHEDULE")?;
    let suspend_idx = col_index(&header, "SUSPEND")?;
    let active_idx = col_index(&header, "ACTIVE")?;
    let last_idx = col_index(&header, "LAST SCHEDULE");
    let ns_idx = col_index(&header, "NAMESPACE");

    struct SuspendedJob<'a> {
        ns: &'a str,
        name: &'a str,
        schedule: &'a str,
        active: usize,
        last_schedule: &'a str,
        line: usize,
    }
    struct ActiveNsGroup<'a> {
        count: usize,
        active_sum: usize,
        first: usize,
        last: usize,
        samples: Vec<&'a str>,
    }

    let mut total = 0usize;
    let mut suspended: Vec<SuspendedJob<'_>> = Vec::new();
    let mut active_by_ns: BTreeMap<&str, ActiveNsGroup<'_>> = BTreeMap::new();

    for (idx, raw) in lines {
        let line_no = idx + 1;
        let line = raw.trim_end_matches('\r').trim_end();
        if line.is_empty() {
            continue;
        }
        let row = parse_cronjob_row(
            line,
            &header,
            ns_idx,
            name_idx,
            sched_idx,
            suspend_idx,
            active_idx,
            last_idx,
        )?;
        total += 1;
        if row.suspended {
            suspended.push(SuspendedJob {
                ns: row.ns,
                name: row.name,
                schedule: row.schedule,
                active: row.active,
                last_schedule: row.last_schedule,
                line: line_no,
            });
        } else {
            active_by_ns
                .entry(row.ns)
                .and_modify(|g| {
                    g.count += 1;
                    g.active_sum += row.active;
                    g.first = g.first.min(line_no);
                    g.last = g.last.max(line_no);
                    if g.samples.len() < 3 {
                        g.samples.push(row.name);
                    }
                })
                .or_insert_with(|| ActiveNsGroup {
                    count: 1,
                    active_sum: row.active,
                    first: line_no,
                    last: line_no,
                    samples: vec![row.name],
                });
        }
    }

    if total == 0 {
        return None;
    }

    let mut head = String::with_capacity(64 + suspended.len().saturating_mul(84));
    let _ = writeln!(
        head,
        "kubectl get cronjobs: {total} listed, {} suspended",
        suspended.len()
    );
    for cj in &suspended {
        if !cj.ns.is_empty() {
            head.push_str(cj.ns);
            head.push('/');
        }
        let _ = write!(
            head,
            "{} SUSPENDED schedule=\"{}\" active={}",
            cj.name, cj.schedule, cj.active
        );
        if !cj.last_schedule.is_empty() {
            let _ = write!(head, " last={}", cj.last_schedule);
        }
        let _ = writeln!(head, " {}", stdout_ref(cj.line, cj.line));
    }

    if active_by_ns.is_empty() {
        return fit_smaller(head, raw_len, max_bytes);
    }

    let mut groups: Vec<(&str, &ActiveNsGroup<'_>)> =
        active_by_ns.iter().map(|(&ns, g)| (ns, g)).collect();
    groups.sort_by(|(na, ga), (nb, gb)| gb.count.cmp(&ga.count).then_with(|| na.cmp(nb)));

    let mut full = String::with_capacity(head.len() + groups.len().saturating_mul(72) + 24);
    full.push_str(&head);
    full.push_str("Active cronjobs:\n");
    for &(ns, g) in &groups {
        full.push_str("  ");
        if !ns.is_empty() {
            full.push_str(ns);
            full.push_str(": ");
        }
        let _ = write!(full, "×{} ({} running; ", g.count, g.active_sum);
        for (idx, s) in g.samples.iter().enumerate() {
            if idx > 0 {
                full.push_str(", ");
            }
            full.push_str(s);
        }
        if g.count > g.samples.len() {
            full.push_str(", …");
        }
        let _ = writeln!(full, ") {}", stdout_ref(g.first, g.last));
    }
    if let Some(out) = fit_smaller(full, raw_len, max_bytes) {
        return Some(out);
    }

    let active_total: usize = groups.iter().map(|(_, g)| g.count).sum();
    let running_total: usize = groups.iter().map(|(_, g)| g.active_sum).sum();
    let first = groups.iter().map(|(_, g)| g.first).min().unwrap_or(1);
    let last = groups.iter().map(|(_, g)| g.last).max().unwrap_or(first);
    let mut compact = head;
    let _ = writeln!(
        compact,
        "Active cronjobs: {active_total} ({running_total} running) across {} namespace(s) {}",
        groups.len(),
        stdout_ref(first, last)
    );
    fit_smaller(compact, raw_len, max_bytes)
}

struct CronJobRow<'a> {
    ns: &'a str,
    name: &'a str,
    schedule: &'a str,
    suspended: bool,
    active: usize,
    last_schedule: &'a str,
}

#[allow(clippy::too_many_arguments)]
fn parse_cronjob_row<'a>(
    line: &'a str,
    header: &[(String, usize)],
    ns_idx: Option<usize>,
    name_idx: usize,
    sched_idx: usize,
    suspend_idx: usize,
    active_idx: usize,
    last_idx: Option<usize>,
) -> Option<CronJobRow<'a>> {
    let parse_bool = |s: &str| {
        if s.eq_ignore_ascii_case("true") {
            Some(true)
        } else if s.eq_ignore_ascii_case("false") {
            Some(false)
        } else {
            None
        }
    };

    if let (Some(name), Some(schedule), Some(suspend), Some(active_str)) = (
        slice_header_col(line, header, name_idx),
        slice_header_col(line, header, sched_idx),
        slice_header_col(line, header, suspend_idx),
        slice_header_col(line, header, active_idx),
    ) && !name.is_empty()
        && !name.contains(char::is_whitespace)
        && !schedule.is_empty()
        && let (Some(suspended), Ok(active)) = (parse_bool(suspend), active_str.parse::<usize>())
    {
        let ns = match ns_idx {
            Some(i) => slice_header_col(line, header, i)?,
            None => "",
        };
        let last_schedule = last_idx
            .and_then(|i| slice_header_col(line, header, i))
            .unwrap_or("");
        return Some(CronJobRow {
            ns,
            name,
            schedule,
            suspended,
            active,
            last_schedule,
        });
    }

    let cells: Vec<&str> = COLUMN.find_iter(line).map(|m| m.as_str()).collect();
    if cells.len() == header.len() {
        let name = cells[name_idx];
        let schedule = cells[sched_idx];
        let suspended = parse_bool(cells[suspend_idx])?;
        let active: usize = cells[active_idx].parse().ok()?;
        if !name.is_empty() && !name.contains(char::is_whitespace) && !schedule.is_empty() {
            return Some(CronJobRow {
                ns: ns_idx.map_or("", |i| cells[i]),
                name,
                schedule,
                suspended,
                active,
                last_schedule: last_idx.map_or("", |i| cells[i]),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{Inventory, events, helm_list, images, replica_prefix, resources};
    use crate::filters::Profile;

    fn savings_pct(label: &str, raw: &str, compact: &str) -> f64 {
        let pct = (1.0 - (compact.len() as f64 / raw.len() as f64)) * 100.0;
        println!(
            "measured {label}: {} B -> {} B (-{pct:.1}%)",
            raw.len(),
            compact.len()
        );
        pct
    }

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

    #[test]
    fn docker_compose_ps_and_kubectl_daemonsets_replicasets_and_jobs_summarize_attention_items() {
        let compose = [
            "NAME            IMAGE         COMMAND                  SERVICE   CREATED       STATUS                     PORTS",
            "app-db-1        postgres:16   \"docker-entrypoint.s…\"   db        2 hours ago   Up 2 hours (healthy)       5432/tcp",
            "app-web-1       node:22       \"node server.js\"         web       2 hours ago   Exited (1) 5 minutes ago   3000/tcp",
        ];
        let mut inv = Inventory::new(Profile::ComposeContainers);
        for (i, line) in compose.iter().enumerate() {
            assert!(inv.observe(line, i + 1), "row {}: {line}", i + 1);
        }
        let summary = inv.finish();
        assert_eq!(
            summary.outcomes[0],
            "docker compose ps: 2 listed, 1 need attention"
        );
        assert_eq!(summary.items[0].name, "app-web-1");
        assert_eq!(summary.items[0].detail, "Exited (1)");

        let ds = [
            "NAME        DESIRED   CURRENT   READY   UP-TO-DATE   AVAILABLE   AGE",
            "fluentbit   5         5         5       5            5           10d",
            "node-exp    5         5         3       5            3           10d",
        ];
        let mut inv = Inventory::new(Profile::DaemonSets);
        for (i, line) in ds.iter().enumerate() {
            assert!(inv.observe(line, i + 1));
        }
        let summary = inv.finish();
        assert_eq!(
            summary.outcomes[0],
            "kubectl get daemonsets: 2 listed, 1 need attention"
        );
        assert_eq!(summary.items[0].name, "node-exp");

        let rs = [
            "NAME          DESIRED   CURRENT   READY   AGE",
            "api-old       0         0         0       9d",
            "api-live      3         3         3       2d",
            "api-rollout   3         3         1       1m",
        ];
        let mut inv = Inventory::new(Profile::ReplicaSets);
        for (i, line) in rs.iter().enumerate() {
            assert!(inv.observe(line, i + 1));
        }
        let summary = inv.finish();
        assert_eq!(
            summary.outcomes[0],
            "kubectl get replicasets: 3 listed, 1 need attention"
        );
        assert_eq!(summary.items[0].name, "api-rollout");

        let jobs = [
            "NAME         STATUS     COMPLETIONS   DURATION   AGE",
            "migrate-01   Complete   1/1           12s        3d",
            "backfill-2   Running    2/5           4m         4m",
        ];
        let mut inv = Inventory::new(Profile::Jobs);
        for (i, line) in jobs.iter().enumerate() {
            assert!(inv.observe(line, i + 1));
        }
        let summary = inv.finish();
        assert_eq!(
            summary.outcomes[0],
            "kubectl get jobs: 2 listed, 1 need attention"
        );
        assert_eq!(summary.items[0].name, "backfill-2");
        assert_eq!(summary.items[0].detail, "Running completions=2/5");
    }

    #[test]
    fn replica_prefix_extracts_pod_and_container_prefixes_without_false_collapses() {
        assert_eq!(
            replica_prefix("api-7f9b8c-x9k2p").as_deref(),
            Some("api-7f9b8c-*")
        );
        assert_eq!(
            replica_prefix("api-7f9b8c-abcde").as_deref(),
            Some("api-7f9b8c-*")
        );
        assert_eq!(
            replica_prefix("worker-4a2e-9b1c2").as_deref(),
            Some("worker-4a2e-*")
        );
        assert_eq!(
            replica_prefix("fluentbit-bcdfg").as_deref(),
            Some("fluentbit-*")
        );
        assert_eq!(replica_prefix("redis-0").as_deref(), Some("redis-*"));
        assert_eq!(
            replica_prefix("shop-worker-12").as_deref(),
            Some("shop-worker-*")
        );
        assert_eq!(
            replica_prefix("shop_worker_2").as_deref(),
            Some("shop_worker_*")
        );
        // Distinct English service names must never collapse together.
        assert_eq!(replica_prefix("app-cache"), None);
        assert_eq!(replica_prefix("app-queue"), None);
        assert_eq!(replica_prefix("app-proxy"), None);
        assert_eq!(replica_prefix("api-canary"), None);
        assert_eq!(replica_prefix("web"), None);
    }

    #[test]
    fn kubectl_pods_groups_healthy_replicas_by_prefix_and_preserves_every_unhealthy_pod() {
        let mut raw = String::from(
            "NAMESPACE     NAME                        READY   STATUS               RESTARTS        AGE\n",
        );
        // Lines 2..=13: 12 healthy api-7f9b8c-* pods in prod.
        for i in 0..12 {
            raw.push_str(&format!(
                "prod          api-7f9b8c-x9k{i:02}            1/1     Running              0               4d\n"
            ));
        }
        // Lines 14..=21: 8 healthy worker-4a2e-* pods in prod.
        for i in 0..8 {
            raw.push_str(&format!(
                "prod          worker-4a2e-m7p{i:02}           1/1     Running              0               4d\n"
            ));
        }
        // Lines 22..=25: 4 healthy coredns-5d78c9869d-* pods in kube-system.
        for i in 0..4 {
            raw.push_str(&format!(
                "kube-system   coredns-5d78c9869d-bcdf{i}    1/1     Running              0               30d\n"
            ));
        }
        // Lines 26..=32: 7 distinct unhealthy / warning pods that must ALL stay verbatim.
        raw.push_str(
            "prod          api-7f9b8c-bad01            0/1     CrashLoopBackOff     14 (2m ago)     1h\n",
        );
        raw.push_str(
            "prod          checkout-9d1a-err02         0/1     ImagePullBackOff     0               5m\n",
        );
        raw.push_str(
            "prod          ml-train-0                  0/1     Pending              0               8m\n",
        );
        raw.push_str(
            "prod          batch-8c2f-evict            0/1     Evicted              0               2h\n",
        );
        raw.push_str(
            "prod          analytics-5e1a-oom01        0/1     OOMKilled            3 (4m ago)      45m\n",
        );
        raw.push_str(
            "prod          api-7f9b8c-hi99r            1/1     Running              9 (12m ago)     2d\n",
        );
        raw.push_str(
            "prod          gateway-3b2a-unr01          1/2     Running              0               10m\n",
        );
        // Line 33: 1 singleton healthy pod.
        raw.push_str(
            "ops           bastion                     1/1     Running              0               12d\n",
        );
        // Lines 34..=37: 4 completed job pods in jobs namespace.
        for i in 0..4 {
            raw.push_str(&format!(
                "jobs          nightly-28491020-k8z{i:02}      0/1     Completed            0               6h\n"
            ));
        }

        let mut inv = Inventory::new(Profile::Pods);
        for (i, line) in raw.lines().enumerate() {
            assert!(inv.observe(line, i + 1), "row {}: {line}", i + 1);
        }
        let summary = inv.finish();
        let rendered = summary.render(None, 0).expect("inventory summary renders");

        assert_eq!(
            summary.outcomes[0],
            "kubectl get pods: 36 listed, 7 need attention"
        );
        // Every single unhealthy pod is preserved verbatim with exact stdout line.
        for expected in [
            "prod/api-7f9b8c-bad01 CrashLoopBackOff ready=0/1 restarts=14 [stdout:26-26]",
            "prod/checkout-9d1a-err02 ImagePullBackOff ready=0/1 restarts=0 [stdout:27-27]",
            "prod/ml-train-0 Pending ready=0/1 restarts=0 [stdout:28-28]",
            "prod/batch-8c2f-evict Evicted ready=0/1 restarts=0 [stdout:29-29]",
            "prod/analytics-5e1a-oom01 OOMKilled ready=0/1 restarts=3 [stdout:30-30]",
            "prod/api-7f9b8c-hi99r Running ready=1/1 restarts=9 [stdout:31-31]",
            "prod/gateway-3b2a-unr01 Running ready=1/2 restarts=0 [stdout:32-32]",
        ] {
            assert!(
                rendered.contains(expected),
                "missing unhealthy pod `{expected}` in:\n{rendered}"
            );
        }
        // Every healthy replica group shows exact deployment mix + [stdout:A-B].
        for expected in [
            "Healthy groups:",
            "prod/api-7f9b8c-* ×12 · 1/1 Running [stdout:2-13]",
            "prod/worker-4a2e-* ×8 · 1/1 Running [stdout:14-21]",
            "kube-system/coredns-5d78c9869d-* ×4 · 1/1 Running [stdout:22-25]",
            "jobs/nightly-28491020-* ×4 · Completed [stdout:34-37]",
            "+1 other healthy: ops/bastion [stdout:33]",
        ] {
            assert!(
                rendered.contains(expected),
                "missing healthy group `{expected}` in:\n{rendered}"
            );
        }
        let pct = savings_pct("kubectl get pods -A (36 pods)", &raw, &rendered);
        assert!(
            pct >= 60.0,
            "expected >=60% byte savings on pods table, got {pct:.1}% ({} -> {} B):\n{rendered}",
            raw.len(),
            rendered.len()
        );
    }

    #[test]
    fn docker_ps_and_compose_group_healthy_container_replicas_with_line_links() {
        let mut ps = String::from(
            "CONTAINER ID   IMAGE              COMMAND                  CREATED       STATUS                       PORTS      NAMES\n",
        );
        for i in 1..=12 {
            ps.push_str(&format!(
                "a1b2c3d4e5{i:02}   registry/api:1.4   \"docker-entrypoint.s…\"   3 hours ago   Up 3 hours (healthy)         8080/tcp   shop-api-{i}\n"
            ));
        }
        for i in 1..=8 {
            ps.push_str(&format!(
                "b2c3d4e5f6{i:02}   registry/wrk:1.4   \"worker --queues=all\"    2 hours ago   Up 2 hours                              shop-worker-{i}\n"
            ));
        }
        ps.push_str(
            "c3d4e5f6a199   postgres:16        \"docker-entrypoint.s…\"   5 hours ago   Up 1 minute (unhealthy)      5432/tcp   shop-db-1\n",
        );
        ps.push_str(
            "d4e5f6a1b299   registry/cron:1.4  \"./run-cron.sh\"          1 hour ago    Exited (137) 4 minutes ago              shop-cron-1\n",
        );

        let mut inv = Inventory::new(Profile::Containers);
        for (i, line) in ps.lines().enumerate() {
            assert!(inv.observe(line, i + 1), "row {}: {line}", i + 1);
        }
        let summary = inv.finish();
        let rendered = summary.render(None, 0).expect("docker ps summary renders");
        assert!(rendered.contains("docker ps: 22 listed, 2 need attention"));
        assert!(rendered.contains("shop-db-1 unhealthy [stdout:22-22]"));
        assert!(rendered.contains("shop-cron-1 Exited (137) [stdout:23-23]"));
        assert!(rendered.contains("shop-api-* ×12 · Up (healthy) [stdout:2-13]"));
        assert!(rendered.contains("shop-worker-* ×8 · Up [stdout:14-21]"));
        let pct = savings_pct("docker ps (22 containers)", &ps, &rendered);
        assert!(
            pct >= 75.0,
            "expected >=75% savings on docker ps, got {pct:.1}% ({} -> {} B)",
            ps.len(),
            rendered.len()
        );

        // `docker compose ps` groups replicas by prefix as well.
        let mut compose = String::from(
            "NAME              IMAGE              COMMAND                SERVICE   CREATED       STATUS                     PORTS\n",
        );
        for i in 1..=10 {
            compose.push_str(&format!(
                "app-worker-{i:<2}     registry/wrk:1.4   \"worker --queues=all\"  worker    2 hours ago   Up 2 hours (healthy)       8080/tcp\n"
            ));
        }
        compose.push_str(
            "app-migrate-1     registry/api:1.4   \"./migrate\"            migrate   5 min ago     Exited (1) 4 minutes ago\n",
        );
        let mut inv = Inventory::new(Profile::ComposeContainers);
        for (i, line) in compose.lines().enumerate() {
            assert!(inv.observe(line, i + 1), "row {}: {line}", i + 1);
        }
        let rendered = inv.finish().render(None, 0).expect("compose summary");
        assert!(rendered.contains("docker compose ps: 11 listed, 1 need attention"));
        assert!(rendered.contains("app-migrate-1 Exited (1) [stdout:12-12]"));
        assert!(rendered.contains("app-worker-* ×10 · Up (healthy) [stdout:2-11]"));
        assert!(savings_pct("docker compose ps (11 containers)", &compose, &rendered) >= 70.0);
    }

    #[test]
    fn events_preserves_and_deduplicates_warnings_while_folding_normal_events() {
        let mut raw = String::from(
            "NAMESPACE     LAST SEEN              TYPE      REASON              OBJECT                          MESSAGE\n",
        );
        for i in 0..20 {
            let pod = format!("pod/api-7f9b8c-p{i:02}x9");
            let node = (i % 4) + 1;
            raw.push_str(&format!(
                "prod          10m                    Normal    Scheduled           {pod:<30}  Successfully assigned prod/api-7f9b8c-p{i:02}x9 to node-{node}\n"
            ));
            raw.push_str(&format!(
                "prod          10m                    Normal    Pulled              {pod:<30}  Successfully pulled image \"registry.io/api:v1.4.2\" in 2.3s\n"
            ));
            raw.push_str(&format!(
                "prod          10m                    Normal    Created             {pod:<30}  Created container api\n"
            ));
            raw.push_str(&format!(
                "prod          10m                    Normal    Started             {pod:<30}  Started container api\n"
            ));
        }
        // Repeated identical warning events (3 occurrences) -> deduplicated with count + span.
        raw.push_str(
            "prod          4m (x5 over 12m)       Warning   BackOff             pod/worker-4a2e-bad01           Back-off restarting failed container worker in pod worker-4a2e-bad01\n",
        );
        raw.push_str(
            "prod          2m (x8 over 12m)       Warning   BackOff             pod/worker-4a2e-bad01           Back-off restarting failed container worker in pod worker-4a2e-bad01\n",
        );
        raw.push_str(
            "prod          30s (x12 over 12m)     Warning   BackOff             pod/worker-4a2e-bad01           Back-off restarting failed container worker in pod worker-4a2e-bad01\n",
        );
        // Distinct warning events -> preserved verbatim with exact single-line pointer.
        raw.push_str(
            "prod          1m                     Warning   FailedScheduling    pod/ml-train-0                  0/5 nodes are available: 5 Insufficient nvidia.com/gpu.\n",
        );
        raw.push_str(
            "kube-system   45s                    Warning   Unhealthy           pod/ingress-nginx-ctrl-0        Readiness probe failed: HTTP probe failed with statuscode: 503\n",
        );

        let out = events(&raw, 4096).expect("events output compacted");
        assert!(
            out.starts_with(
                "kubectl get events: 85 listed, 5 warning/non-normal (3 unique), 80 normal\n"
            ),
            "{out}"
        );
        // 100% of Warning events retained verbatim with deduplication and line pointers.
        assert!(
            out.contains("Warning BackOff prod pod/worker-4a2e-bad01 ×3: Back-off restarting failed container worker in pod worker-4a2e-bad01 [stdout:82-84]"),
            "{out}"
        );
        assert!(
            out.contains("Warning FailedScheduling prod pod/ml-train-0: 0/5 nodes are available: 5 Insufficient nvidia.com/gpu. [stdout:85]"),
            "{out}"
        );
        assert!(
            out.contains("Warning Unhealthy kube-system pod/ingress-nginx-ctrl-0: Readiness probe failed: HTTP probe failed with statuscode: 503 [stdout:86]"),
            "{out}"
        );
        // Routine Normal events folded by reason with exact line ranges.
        assert!(out.contains("Scheduled ×20 [stdout:2-78]"), "{out}");
        assert!(out.contains("Pulled ×20 [stdout:3-79]"), "{out}");
        assert!(out.contains("Created ×20 [stdout:4-80]"), "{out}");
        assert!(out.contains("Started ×20 [stdout:5-81]"), "{out}");

        let pct = savings_pct("kubectl get events -A (85 events)", &raw, &out);
        assert!(
            pct >= 85.0,
            "expected >=85% byte savings on kubectl events, got {pct:.1}% ({} -> {} B)",
            raw.len(),
            out.len()
        );

        // Single-namespace format (no NAMESPACE column) also works.
        let single_ns = concat!(
            "LAST SEEN   TYPE      REASON      OBJECT                 MESSAGE\n",
            "5m          Normal    Scheduled   pod/api-1              Successfully assigned default/api-1 to node-1\n",
            "5m          Normal    Scheduled   pod/api-2              Successfully assigned default/api-2 to node-2\n",
            "5m          Normal    Pulled      pod/api-1              Successfully pulled image \"app:1\"\n",
            "5m          Normal    Pulled      pod/api-2              Successfully pulled image \"app:1\"\n",
            "2m          Warning   OOMKilling  node/node-1            Memory cgroup out of memory: Killed process 912\n",
        );
        let single_out = events(single_ns, 4096).expect("single-ns events");
        assert!(
            single_out.contains("Warning OOMKilling node/node-1: Memory cgroup out of memory: Killed process 912 [stdout:6]"),
            "{single_out}"
        );
        assert!(
            single_out.contains("Scheduled ×2 [stdout:2-3]"),
            "{single_out}"
        );

        // Fail-closed on non-table output and JSON.
        assert!(events("No resources found in prod namespace.\n", 4096).is_none());
        assert!(events("{\"items\":[]}\n", 4096).is_none());
        assert!(events(&raw, 40).is_none());
    }

    #[test]
    fn helm_list_highlights_non_deployed_releases_and_groups_deployed_by_namespace() {
        let mut raw = String::from(
            "NAME                  NAMESPACE    REVISION   UPDATED                                 STATUS            CHART                     APP VERSION\n",
        );
        for i in 1..=15 {
            raw.push_str(&format!(
                "svc-{i:02}                prod         {i:<10} 2026-10-12 09:14:22.123456 +0000 UTC    deployed          service-chart-2.1.0       1.4.{i}\n"
            ));
        }
        for i in 1..=10 {
            raw.push_str(&format!(
                "stg-svc-{i:02}            staging      {i:<10} 2026-10-12 08:00:00.000000 +0000 UTC    deployed          service-chart-2.1.0       1.4.{i}\n"
            ));
        }
        for (name, chart) in [
            ("prometheus", "kube-prometheus-stack-61.2.0"),
            ("grafana", "grafana-8.4.0"),
            ("loki", "loki-6.7.0"),
            ("tempo", "tempo-1.10.0"),
            ("alertmanager", "alertmanager-1.12.0"),
        ] {
            raw.push_str(&format!(
                "{name:<20}  monitoring   4          2026-10-10 11:20:00.000000 +0000 UTC    deployed          {chart:<24}  2.0.0\n"
            ));
        }
        // Every non-deployed Helm state preserved verbatim with exact line pointer!
        raw.push_str(
            "payments              prod         7          2026-10-12 09:18:01.654321 +0000 UTC    failed            payments-2.3.1            2.3.1\n",
        );
        raw.push_str(
            "checkout              prod         11         2026-10-12 09:19:10.000000 +0000 UTC    pending-upgrade   checkout-3.0.0            3.0.0\n",
        );
        raw.push_str(
            "search                staging      5          2026-10-12 09:20:00.000000 +0000 UTC    pending-rollback  search-1.8.2              1.8.2\n",
        );
        raw.push_str(
            "analytics             data         1          2026-10-12 09:21:00.000000 +0000 UTC    pending-install   analytics-0.9.0           0.9.0\n",
        );
        raw.push_str(
            "legacy-proxy          ops          9          2026-10-12 09:22:00.000000 +0000 UTC    uninstalling      legacy-proxy-0.4.0\n",
        );

        let out = helm_list(&raw, 4096).expect("helm_list compacted");
        assert!(
            out.starts_with("helm list: 35 listed, 5 need attention\n"),
            "{out}"
        );
        for expected in [
            "prod/payments failed rev=7 chart=payments-2.3.1 app=2.3.1 [stdout:32]",
            "prod/checkout pending-upgrade rev=11 chart=checkout-3.0.0 app=3.0.0 [stdout:33]",
            "staging/search pending-rollback rev=5 chart=search-1.8.2 app=1.8.2 [stdout:34]",
            "data/analytics pending-install rev=1 chart=analytics-0.9.0 app=0.9.0 [stdout:35]",
            "ops/legacy-proxy uninstalling rev=9 chart=legacy-proxy-0.4.0 [stdout:36]",
            "prod (15 deployed): service-chart-2.1.0 ×15 [stdout:2-16]",
            "staging (10 deployed): service-chart-2.1.0 ×10 [stdout:17-26]",
            "monitoring (5 deployed):",
            "+1 more [stdout:27-31]",
        ] {
            assert!(out.contains(expected), "missing `{expected}` in:\n{out}");
        }
        let pct = savings_pct("helm list -A (35 releases)", &raw, &out);
        assert!(
            pct >= 75.0,
            "expected >=75% savings on helm list, got {pct:.1}% ({} -> {} B)",
            raw.len(),
            out.len()
        );

        // Tab-separated Helm output also parses cleanly.
        let mut tabbed =
            String::from("NAME\tNAMESPACE\tREVISION\tUPDATED\tSTATUS\tCHART\tAPP VERSION\n");
        for i in 1..=8 {
            tabbed.push_str(&format!(
                "app-{i}\tprod\t{i}\t2026-10-12 09:00:00 +0000 UTC\tdeployed\tapp-1.0.0\t1.0.0\n"
            ));
        }
        tabbed.push_str(
            "broken\tprod\t2\t2026-10-12 09:05:00 +0000 UTC\tfailed\tbroken-0.2.0\t0.2.0\n",
        );
        let tab_out = helm_list(&tabbed, 4096).expect("tabbed helm list");
        assert!(
            tab_out.contains("prod/broken failed rev=2 chart=broken-0.2.0 app=0.2.0 [stdout:10]"),
            "{tab_out}"
        );
        assert!(
            tab_out.contains("prod (8 deployed): app-1.0.0 ×8 [stdout:2-9]"),
            "{tab_out}"
        );

        // Rejects JSON / `--short` / empty.
        assert!(helm_list("[{\"name\":\"api\"}]\n", 4096).is_none());
        assert!(helm_list("api\npayments\n", 4096).is_none());
    }

    #[test]
    fn images_groups_dangling_and_multi_tag_repositories_for_docker_and_podman() {
        let mut raw = String::from(
            "REPOSITORY                         TAG       IMAGE ID       CREATED        SIZE\n",
        );
        // Lines 2..=11: 10 tags of registry.io/app.
        for i in 0..10 {
            let tag = if i == 0 {
                "latest".to_owned()
            } else {
                format!("v1.4.{i}")
            };
            raw.push_str(&format!(
                "registry.io/app                    {tag:<8}  a1b2c3d4e5{i:02}   {i} days ago     245MB\n"
            ));
        }
        // Lines 12..=16: 5 tags of registry.io/worker sharing 1 image ID!
        for tag in ["latest", "stable", "v2.0", "v2.0.1", "sha-9f8e12"] {
            raw.push_str(&format!(
                "registry.io/worker                 {tag:<10} b2c3d4e5f600   2 days ago     180MB\n"
            ));
        }
        // Lines 17..=30: 14 dangling `<none>:<none>` images (100MB each = 1.40GB).
        for i in 0..14 {
            raw.push_str(&format!(
                "<none>                             <none>    d00d000000{i:02}   3 days ago     100MB\n"
            ));
        }
        // Lines 31..=33: 3 single-tag base images.
        raw.push_str(
            "postgres                           16        777788889999   2 weeks ago    432MB\n",
        );
        raw.push_str(
            "redis                              7-alpine  888899990000   3 weeks ago    41.2MB\n",
        );
        raw.push_str(
            "alpine                             3.20      999900001111   1 month ago    7.8MB\n",
        );

        let out = images(&raw, 4096).expect("docker images compacted");
        assert!(
            out.starts_with("docker images: 32 listed (5 repos, 14 dangling,"),
            "{out}"
        );
        assert!(
            out.contains("dangling (<none>:<none>): 14 images, 1.40GB (ids: d00d00000000, d00d00000001, d00d00000002, …) [stdout:17-30]"),
            "{out}"
        );
        assert!(
            out.contains(
                "registry.io/app (10 tags: latest, v1.4.1, v1.4.2, …; 10 ids, 2.45GB) [stdout:2-11]"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "registry.io/worker (5 tags: latest, stable, v2.0, …; 1 id b2c3d4e5f600, 180MB) [stdout:12-16]"
            ),
            "{out}"
        );
        assert!(
            out.contains("postgres:16 (777788889999, 432MB) [stdout:31]"),
            "{out}"
        );
        let pct = savings_pct("docker images (32 images)", &raw, &out);
        assert!(
            pct >= 75.0,
            "expected >=75% savings on docker images, got {pct:.1}% ({} -> {} B)",
            raw.len(),
            out.len()
        );

        // Podman images format (space before size unit: `256 MB`, `1.20 GB`).
        let mut podman = String::from(
            "REPOSITORY                    TAG         IMAGE ID      CREATED        SIZE\n",
        );
        for i in 0..6 {
            podman.push_str(&format!(
                "localhost/api                 v1.{i}        aa00bb11cc{i:02}  2 days ago     250 MB\n"
            ));
        }
        for i in 0..4 {
            podman.push_str(&format!(
                "<none>                        <none>      ff00ee11dd{i:02}  5 days ago     500 MB\n"
            ));
        }
        let podman_out = images(&podman, 4096).expect("podman images compacted");
        assert!(
            podman_out.contains("dangling (<none>:<none>): 4 images, 2.00GB"),
            "{podman_out}"
        );
        assert!(
            podman_out.contains(
                "localhost/api (6 tags: v1.0, v1.1, v1.2, …; 6 ids, 1.50GB) [stdout:2-7]"
            ),
            "{podman_out}"
        );

        // Rejects `-q` ID-only output and JSON.
        assert!(images("a1b2c3d4e5f6\nb2c3d4e5f6a1\n", 4096).is_none());
        assert!(images("{\"Repository\":\"app\"}\n", 4096).is_none());
    }

    #[test]
    fn resources_handles_services_ingresses_pvcs_and_cronjobs_with_high_savings() {
        // 1. Services: pending LB + external LB/ExternalName highlighted, ClusterIP collapsed.
        let mut svcs = String::from(
            "NAMESPACE     NAME             TYPE           CLUSTER-IP      EXTERNAL-IP      PORT(S)                      AGE\n",
        );
        for i in 1..=25 {
            svcs.push_str(&format!(
                "prod          svc-{i:02}           ClusterIP      10.96.1.{i:<6}  <none>           80/TCP                       14d\n"
            ));
        }
        for i in 1..=10 {
            svcs.push_str(&format!(
                "staging       stg-{i:02}           ClusterIP      10.96.2.{i:<6}  <none>           8080/TCP                     7d\n"
            ));
        }
        svcs.push_str(
            "prod          web-lb           LoadBalancer   10.96.55.12     34.120.10.5      80:31200/TCP,443:31443/TCP   14d\n",
        );
        svcs.push_str(
            "prod          payments-lb      LoadBalancer   10.96.55.99     <pending>        443:32100/TCP                3m\n",
        );
        svcs.push_str(
            "prod          ext-db           ExternalName   <none>          db.example.com   <none>                       30d\n",
        );
        let svc_out = resources("services", &svcs, 4096).expect("services compacted");
        assert!(
            svc_out.starts_with("kubectl get services: 38 listed, 1 pending, 2 external\n"),
            "{svc_out}"
        );
        assert!(
            svc_out.contains("attention: prod/payments-lb LoadBalancer external=<pending> ports=443:32100/TCP [stdout:38]"),
            "{svc_out}"
        );
        assert!(
            svc_out.contains("external: prod/web-lb LoadBalancer external=34.120.10.5 ports=80:31200/TCP,443:31443/TCP [stdout:37]"),
            "{svc_out}"
        );
        assert!(
            svc_out
                .contains("external: prod/ext-db ExternalName external=db.example.com [stdout:39]"),
            "{svc_out}"
        );
        assert!(
            svc_out.contains("prod: ClusterIP ×25 (svc-01, svc-02, svc-03, …) [stdout:2-26]"),
            "{svc_out}"
        );
        assert!(
            svc_out.contains("staging: ClusterIP ×10 (stg-01, stg-02, stg-03, …) [stdout:27-36]"),
            "{svc_out}"
        );
        let svc_pct = savings_pct("kubectl get services (38 svcs)", &svcs, &svc_out);
        assert!(svc_pct >= 80.0, "services savings too low: {svc_pct:.1}%");

        // 2. Ingresses: missing address highlighted, spaced `80, 443` ports parsed, shared LB collapsed.
        let mut ings = String::from(
            "NAMESPACE   NAME            CLASS   HOSTS                    ADDRESS          PORTS     AGE\n",
        );
        for i in 1..=20 {
            ings.push_str(&format!(
                "prod        svc-{i:02}-ing      nginx   svc{i:02}.example.com      34.120.10.5      80, 443   14d\n"
            ));
        }
        ings.push_str(
            "prod        admin-ing       alb     admin.example.com        k8s-adm.elb.io   443       30d\n",
        );
        ings.push_str(
            "prod        beta-ing        nginx   beta.example.com                          80        2m\n",
        );
        let ing_out = resources("ing", &ings, 4096).expect("ingresses compacted");
        assert!(
            ing_out.contains("kubectl get ingresses: 22 listed, 1 missing address"),
            "{ing_out}"
        );
        assert!(
            ing_out.contains("attention: prod/beta-ing class=nginx hosts=beta.example.com address=<none> ports=80 [stdout:23]"),
            "{ing_out}"
        );
        assert!(
            ing_out.contains("external: prod/admin-ing hosts=admin.example.com addr=k8s-adm.elb.io ports=443 [stdout:22]"),
            "{ing_out}"
        );
        assert!(
            ing_out.contains("external: prod (nginx → 34.120.10.5, ports 80, 443) ×20: svc01.example.com, svc02.example.com, svc03.example.com, … [stdout:2-21]"),
            "{ing_out}"
        );
        let ing_pct = savings_pct("kubectl get ingresses (22 ing)", &ings, &ing_out);
        assert!(ing_pct >= 75.0, "ingresses savings too low: {ing_pct:.1}%");

        // 3. PVCs: Pending (with blank volume/capacity), Lost, Terminating highlighted; Bound grouped.
        let mut pvcs = String::from(
            "NAMESPACE   NAME             STATUS        VOLUME                                     CAPACITY   ACCESS MODES   STORAGECLASS   AGE\n",
        );
        for i in 0..18 {
            pvcs.push_str(&format!(
                "prod        data-pg-{i:<2}       Bound         pvc-a1b2c3d4-e5f6-4789-abcd-ef0123456{i:03}  100Gi      RWO            gp3            45d\n"
            ));
        }
        for i in 0..6 {
            let cap = if i % 2 == 0 { "20Gi" } else { "40Gi" };
            pvcs.push_str(&format!(
                "staging     stg-vol-{i:<2}       Bound         pvc-b2c3d4e5-f6a1-4789-abcd-ef0123456{i:03}  {cap:<9}  RWO            gp3            12d\n"
            ));
        }
        pvcs.push_str(
            "prod        cache-upload     Pending                                                                            gp3-fast       12m\n",
        );
        pvcs.push_str(
            "prod        old-archive      Lost          pvc-99998888-7777-6666-5555-444433332222   50Gi       RWO            gp2            120d\n",
        );
        pvcs.push_str(
            "staging     scratch-0        Terminating   pvc-11112222-3333-4444-5555-666677778888   20Gi       RWO            gp3            3d\n",
        );
        let pvc_out = resources("pvcs", &pvcs, 4096).expect("pvcs compacted");
        assert!(
            pvc_out.starts_with("kubectl get pvc: 27 listed, 3 need attention\n"),
            "{pvc_out}"
        );
        assert!(
            pvc_out.contains("prod/cache-upload Pending class=gp3-fast [stdout:26]"),
            "{pvc_out}"
        );
        assert!(
            pvc_out.contains("prod/old-archive Lost capacity=50Gi class=gp2 vol=pvc-99998888-7777-6666-5555-444433332222 [stdout:27]"),
            "{pvc_out}"
        );
        assert!(
            pvc_out.contains("staging/scratch-0 Terminating capacity=20Gi class=gp3 vol=pvc-11112222-3333-4444-5555-666677778888 [stdout:28]"),
            "{pvc_out}"
        );
        assert!(
            pvc_out.contains(
                "prod: gp3 RWO 100Gi ×18 (data-pg-0, data-pg-1, data-pg-2, …) [stdout:2-19]"
            ),
            "{pvc_out}"
        );
        assert!(
            pvc_out.contains("staging: gp3 RWO (total 180Gi) ×6 (stg-vol-0, stg-vol-1, stg-vol-2, …) [stdout:20-25]"),
            "{pvc_out}"
        );
        let pvc_pct = savings_pct("kubectl get pvcs (27 pvcs)", &pvcs, &pvc_out);
        assert!(pvc_pct >= 80.0, "pvcs savings too low: {pvc_pct:.1}%");

        // 4. CronJobs: suspended highlighted verbatim, spaced cron schedule parsed, active collapsed.
        let mut cjs = String::from(
            "NAMESPACE   NAME                SCHEDULE       TIMEZONE   SUSPEND   ACTIVE   LAST SCHEDULE   AGE\n",
        );
        for i in 1..=18 {
            let active = usize::from(i <= 2);
            cjs.push_str(&format!(
                "prod        job-sync-{i:02}         0 {h} * * *      UTC        False     {active}        14m             60d\n",
                h = i % 10
            ));
        }
        for i in 1..=10 {
            cjs.push_str(&format!(
                "staging     stg-clean-{i:02}        */15 * * * *   <none>     False     0        4m              30d\n"
            ));
        }
        cjs.push_str(
            "prod        billing-export      0 4 * * 1      UTC        True      0        8d              120d\n",
        );
        cjs.push_str(
            "staging     legacy-etl          0 3 * * *      <none>     True      0        30d             200d\n",
        );
        let cj_out = resources("cronjobs.batch", &cjs, 4096).expect("cronjobs compacted");
        assert!(
            cj_out.starts_with("kubectl get cronjobs: 30 listed, 2 suspended\n"),
            "{cj_out}"
        );
        assert!(
            cj_out.contains(
                "prod/billing-export SUSPENDED schedule=\"0 4 * * 1\" active=0 last=8d [stdout:30]"
            ),
            "{cj_out}"
        );
        assert!(
            cj_out.contains(
                "staging/legacy-etl SUSPENDED schedule=\"0 3 * * *\" active=0 last=30d [stdout:31]"
            ),
            "{cj_out}"
        );
        assert!(
            cj_out.contains(
                "prod: ×18 (2 running; job-sync-01, job-sync-02, job-sync-03, …) [stdout:2-19]"
            ),
            "{cj_out}"
        );
        assert!(
            cj_out.contains("staging: ×10 (0 running; stg-clean-01, stg-clean-02, stg-clean-03, …) [stdout:20-29]"),
            "{cj_out}"
        );
        let cj_pct = savings_pct("kubectl get cronjobs (30 cjs)", &cjs, &cj_out);
        assert!(cj_pct >= 75.0, "cronjobs savings too low: {cj_pct:.1}%");

        // Unknown kind or non-table text returns None.
        assert!(resources("configmaps", &svcs, 4096).is_none());
        assert!(resources("services", "No resources found.\n", 4096).is_none());
        assert!(resources("pvcs", "{\"items\":[]}\n", 4096).is_none());
    }

    #[test]
    fn formatters_return_none_when_output_would_not_shrink_or_exceeds_budget() {
        // Tiny 1-row outputs that cannot shrink strictly smaller return None.
        let tiny_events = concat!(
            "LAST SEEN  TYPE  REASON  OBJECT  MESSAGE\n",
            "1s  Warning  BackOff  pod/a  fail\n",
        );
        assert!(events(tiny_events, 4096).is_none());

        let tiny_helm = "NAME\tNAMESPACE\tREVISION\tUPDATED\tSTATUS\tCHART\tAPP VERSION\na\tns\t1\tnow\tfailed\tc-1\t1\n";
        assert!(helm_list(tiny_helm, 4096).is_none());

        let tiny_img = concat!(
            "REPOSITORY  TAG  IMAGE ID  CREATED  SIZE\n",
            "a  1  abc123  1d  1MB\n",
        );
        assert!(images(tiny_img, 4096).is_none());

        let tiny_svc = concat!(
            "NAME  TYPE  CLUSTER-IP  EXTERNAL-IP  PORT(S)  AGE\n",
            "a  ClusterIP  10.0.0.1  <none>  80/TCP  1d\n",
        );
        assert!(resources("svc", tiny_svc, 4096).is_none());

        // Zero budget always returns None.
        assert!(events(tiny_events, 0).is_none());
        assert!(helm_list(tiny_helm, 0).is_none());
        assert!(images(tiny_img, 0).is_none());
        assert!(resources("svc", tiny_svc, 0).is_none());
    }

    #[test]
    fn tight_budgets_and_edge_cases_preserve_all_warnings_and_line_links() {
        // 1. Single-namespace pods with an unhealthy pod in the middle of a healthy ReplicaSet.
        let mut pods =
            String::from("NAME                  READY   STATUS             RESTARTS   AGE\n");
        for i in 0..5 {
            pods.push_str(&format!(
                "api-7f9b8c-a{i}b2c      1/1     Running            0          2d\n"
            ));
        }
        pods.push_str("api-7f9b8c-bad99      0/1     CrashLoopBackOff   7          10m\n");
        for i in 5..10 {
            pods.push_str(&format!(
                "api-7f9b8c-a{i}b2c      1/1     Running            0          2d\n"
            ));
        }
        let mut inv = Inventory::new(Profile::Pods);
        for (i, line) in pods.lines().enumerate() {
            assert!(inv.observe(line, i + 1));
        }
        let rendered = inv.finish().render(None, 0).expect("single-ns pods");
        assert!(
            rendered
                .contains("api-7f9b8c-bad99 CrashLoopBackOff ready=0/1 restarts=7 [stdout:7-7]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("  api-7f9b8c-* ×10 · 1/1 Running [stdout:2-12]"),
            "{rendered}"
        );

        // 2. Compose custom non-numbered container names group via SERVICE fallback.
        let compose = concat!(
            "NAME            IMAGE         COMMAND       SERVICE   CREATED       STATUS                 PORTS\n",
            "worker-alpha    app:latest    \"./worker\"    worker    2 hours ago   Up 2 hours (healthy)   8080/tcp\n",
            "worker-beta     app:latest    \"./worker\"    worker    2 hours ago   Up 2 hours (healthy)   8080/tcp\n",
            "worker-gamma    app:latest    \"./worker\"    worker    2 hours ago   Up 2 hours (healthy)   8080/tcp\n",
        );
        let mut inv = Inventory::new(Profile::ComposeContainers);
        for (i, line) in compose.lines().enumerate() {
            assert!(inv.observe(line, i + 1));
        }
        let rendered = inv.finish().render(None, 0).expect("compose service group");
        assert!(
            rendered.contains("worker (service) ×3 · Up (healthy) [stdout:2-4]"),
            "{rendered}"
        );

        // 3. Over-24 healthy pod groups fold overflow into a single summary line with [stdout:A-B].
        let mut many_groups =
            String::from("NAME                  READY   STATUS    RESTARTS   AGE\n");
        for g in 0..28 {
            for r in 0..2 {
                many_groups.push_str(&format!(
                    "svc{g:02}-7f9b8c-x{r}k2p   1/1     Running   0          5d\n"
                ));
            }
        }
        let mut inv = Inventory::new(Profile::Pods);
        for (i, line) in many_groups.lines().enumerate() {
            assert!(inv.observe(line, i + 1));
        }
        let rendered = inv.finish().render(None, 0).expect("many groups");
        assert!(
            rendered.contains("+4 more healthy group(s) (8 listed) [stdout:"),
            "{rendered}"
        );

        // 4. `events` tight max_bytes compacts Normal section while retaining 100% of warnings.
        let mut ev = String::from("LAST SEEN   TYPE      REASON         OBJECT        MESSAGE\n");
        for reason in [
            "Scheduled",
            "Pulling",
            "Pulled",
            "Created",
            "Started",
            "ScalingReplicaSet",
            "SuccessfulCreate",
            "Killing",
        ] {
            for i in 0..3 {
                ev.push_str(&format!(
                    "5m          Normal    {reason:<14} pod/api-{i}     Normal lifecycle step {reason} for pod/api-{i}\n"
                ));
            }
        }
        ev.push_str(
            "1m          Warning   FailedMount    pod/db-0      MountVolume.SetUp failed for volume \"cfg\"\n",
        );
        let full_ev = events(&ev, 4096).expect("full events");
        let tight_budget = full_ev.len() - 40;
        let tight_ev = events(&ev, tight_budget).expect("tight events compacts normal section");
        assert!(tight_ev.len() <= tight_budget, "{tight_ev}");
        assert!(
            tight_ev.contains("Warning FailedMount pod/db-0: MountVolume.SetUp failed for volume \"cfg\" [stdout:26]"),
            "{tight_ev}"
        );
        assert!(tight_ev.contains("[stdout:2-25]"), "{tight_ev}");

        // 5. `images` handles `--digests`, single dangling image, and long-repo overflow.
        let mut digests = String::from(
            "REPOSITORY         TAG       DIGEST              IMAGE ID       CREATED       SIZE\n",
        );
        for i in 0..5 {
            digests.push_str(&format!(
                "123456789012.dkr.ecr.us-east-1.amazonaws.com/platform/api  v1.{i}  sha256:00000000000{i}  aabbccdd000{i}  2 days ago  210MB\n"
            ));
        }
        digests.push_str(
            "<none>             <none>    <none>              deadbeef1234   4 days ago    320MB\n",
        );
        let dig_out = images(&digests, 4096).expect("images --digests");
        assert!(
            dig_out.contains("dangling (<none>:<none>): 1 image (deadbeef1234, 320MB) [stdout:7]"),
            "{dig_out}"
        );
        assert!(
            dig_out.contains("123456789012.dkr.ecr.us-east-1.amazonaws.com/platform/api (5 tags: v1.0, v1.1, v1.2, …; 5 ids, 1.05GB) [stdout:2-6]"),
            "{dig_out}"
        );

        // 6. Tight `helm_list` budget compacts deployed namespaces while retaining failed release verbatim.
        let mut helm = String::from(
            "NAME        NAMESPACE   REVISION   UPDATED                                 STATUS     CHART           APP VERSION\n",
        );
        for ns in ["ns1", "ns2", "ns3", "ns4", "ns5", "ns6", "ns7", "ns8"] {
            for i in 0..3 {
                helm.push_str(&format!(
                    "app-{i}       {ns:<11} 1          2026-10-12 09:00:00.000000 +0000 UTC    deployed   chart-{i}-1.0.0   1.0.0\n"
                ));
            }
        }
        helm.push_str(
            "payments    prod        9          2026-10-12 09:10:00.000000 +0000 UTC    failed     payments-2.0    2.0\n",
        );
        let full_helm = helm_list(&helm, 4096).expect("full helm");
        let tight_helm_budget = full_helm.len() - 60;
        let tight_helm = helm_list(&helm, tight_helm_budget).expect("tight helm");
        assert!(tight_helm.len() <= tight_helm_budget, "{tight_helm}");
        assert!(
            tight_helm
                .contains("prod/payments failed rev=9 chart=payments-2.0 app=2.0 [stdout:26]"),
            "{tight_helm}"
        );
        assert!(tight_helm.contains("[stdout:2-25]"), "{tight_helm}");
    }

    #[test]
    fn pins_crashloop_oom_imagepull_error_unhealthy_warning_first_with_ansi_fast_path() {
        // 1. `kubectl get pods` with ANSI escapes + lower-severity issues appearing BEFORE critical crashes.
        let mut pods = String::from(
            "\x1b[1mNAMESPACE   NAME                   READY   STATUS             RESTARTS      AGE\x1b[0m\n",
        );
        for i in 0..8 {
            pods.push_str(&format!(
                "prod        web-7f9b8c-a{i}k2p       1/1     \x1b[32mRunning\x1b[0m            0             2d\n"
            ));
        }
        pods.push_str(
            "prod        web-7f9b8c-rst01       1/1     Running            2 (10m ago)   1d\n",
        );
        pods.push_str(
            "prod        ml-job-0               0/1     Pending            0             4m\n",
        );
        pods.push_str(
            "prod        pay-4a2e-crash         0/1     \x1b[31mCrashLoopBackOff\x1b[0m   9 (1m ago)    25m\n",
        );
        pods.push_str(
            "prod        img-9d1a-pull          0/1     \x1b[31mImagePullBackOff\x1b[0m   0             6m\n",
        );
        pods.push_str(
            "prod        agg-5e1a-oom           0/1     \x1b[31mOOMKilled\x1b[0m          4 (2m ago)    40m\n",
        );
        pods.push_str(
            "prod        init-8c2f-err          0/1     \x1b[31mError\x1b[0m              1             9m\n",
        );

        let mut inv = Inventory::new(Profile::Pods);
        for (i, line) in pods.lines().enumerate() {
            assert!(inv.observe(line, i + 1), "row {}: {line}", i + 1);
        }
        let summary = inv.finish();
        let names: Vec<&str> = summary.items.iter().map(|it| it.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "prod/pay-4a2e-crash",
                "prod/img-9d1a-pull",
                "prod/agg-5e1a-oom",
                "prod/init-8c2f-err",
                "prod/ml-job-0",
                "prod/web-7f9b8c-rst01",
            ]
        );
        let rendered = summary.render(None, 0).expect("pods rendered");
        assert!(!rendered.contains('\x1b'), "ANSI leaked:\n{rendered}");
        assert!(
            rendered.contains(
                "prod/pay-4a2e-crash CrashLoopBackOff ready=0/1 restarts=9 [stdout:12-12]"
            ),
            "{rendered}"
        );
        assert!(
            rendered.contains("prod/agg-5e1a-oom OOMKilled ready=0/1 restarts=4 [stdout:14-14]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("prod/web-7f9b8c-* ×8 · 1/1 Running [stdout:2-9]"),
            "{rendered}"
        );

        // 2. `docker ps` with ANSI escapes pins unhealthy + Exited (137) ahead of Paused/Created.
        let mut ps = String::from(
            "CONTAINER ID   IMAGE       COMMAND      CREATED      STATUS                       PORTS      NAMES\n",
        );
        for i in 1..=6 {
            ps.push_str(&format!(
                "aa11bb22cc{i:02}   api:1.0     \"./api\"      2h ago       \x1b[32mUp 2 hours (healthy)\x1b[0m         8080/tcp   api-{i}\n"
            ));
        }
        ps.push_str(
            "dd11ee22ff01   cache:7     \"redis\"      2h ago       Up 1 hour (Paused)           6379/tcp   cache-1\n",
        );
        ps.push_str(
            "dd11ee22ff02   db:16       \"postgres\"   3h ago       \x1b[31mUp 2 minutes (unhealthy)\x1b[0m     5432/tcp   db-1\n",
        );
        ps.push_str(
            "dd11ee22ff03   worker:1    \"./work\"     1h ago       \x1b[31mExited (137) 1 minute ago\x1b[0m               worker-1\n",
        );
        let mut inv = Inventory::new(Profile::Containers);
        for (i, line) in ps.lines().enumerate() {
            assert!(inv.observe(line, i + 1), "row {}: {line}", i + 1);
        }
        let summary = inv.finish();
        let names: Vec<&str> = summary.items.iter().map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["db-1", "worker-1", "cache-1"]);
        let rendered = summary.render(None, 0).expect("docker ps rendered");
        assert!(
            rendered.contains("db-1 unhealthy [stdout:9-9]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("worker-1 Exited (137) [stdout:10-10]"),
            "{rendered}"
        );
        assert!(
            rendered.contains("api-* ×6 · Up (healthy) [stdout:2-7]"),
            "{rendered}"
        );

        // 3. `kubectl get events` with ANSI escapes pins critical OOMKilling/Unhealthy before minor warnings and Normal rows.
        let mut ev =
            String::from("LAST SEEN   TYPE      REASON            OBJECT        MESSAGE\n");
        for i in 0..8 {
            ev.push_str(&format!(
                "5m          \x1b[32mNormal\x1b[0m    Scheduled         pod/web-{i}     Successfully assigned default/web-{i} to node-1\n"
            ));
        }
        ev.push_str(
            "4m          \x1b[33mWarning\x1b[0m   DNSConfigForming  pod/web-0     Search line limits were exceeded\n",
        );
        ev.push_str(
            "1m          \x1b[31mWarning\x1b[0m   OOMKilling        node/node-1   Memory cgroup out of memory: Killed process 4412\n",
        );
        ev.push_str(
            "30s         \x1b[31mWarning\x1b[0m   Unhealthy         pod/db-0      Readiness probe failed: connection refused\n",
        );
        let ev_out = events(&ev, 4096).expect("events compacted");
        assert!(!ev_out.contains('\x1b'), "ANSI leaked:\n{ev_out}");
        let oom_pos = ev_out.find("Warning OOMKilling node/node-1:").unwrap();
        let unhealth_pos = ev_out.find("Warning Unhealthy pod/db-0:").unwrap();
        let dns_pos = ev_out.find("Warning DNSConfigForming pod/web-0:").unwrap();
        let norm_pos = ev_out.find("Normal events:").unwrap();
        assert!(
            oom_pos < dns_pos && unhealth_pos < dns_pos && dns_pos < norm_pos,
            "{ev_out}"
        );
        assert!(ev_out.contains("[stdout:11]"), "{ev_out}");
        assert!(ev_out.contains("[stdout:12]"), "{ev_out}");
        assert!(ev_out.contains("Scheduled ×8 [stdout:2-9]"), "{ev_out}");
    }
}
