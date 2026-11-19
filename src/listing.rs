//! Group file listings by directory: `find`, `fd`, `git ls-files`, `ls -l`/`-R`,
//! `tree`, and the file lists printed by `rg --files`, `rg -l` and `grep -l`.
//!
//! Every listed path counts once, so totals stay exact. Dependency, build and cache
//! directories collapse into a count unless the command names them. Each group links
//! to the original lines it came from. When the groups do not fit the budget, deeper
//! directories fold into their ancestors with a count.
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::search::{STORED, clip, compare, plural};
use crate::summary::{Item, Summary};

const NOISE: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".next",
    ".justless",
];
const NAMES: usize = 6;
const MAX_DIRS: usize = 100_000;

static PERMISSIONS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-dlcbpsDnw?][-rwxsStTl]{9}[@+.]?\s").unwrap());
// Mode, links, owner, group, size, month, day, then a time or year before the name.
static LONG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^([-dlcbpsDnw?])[-rwxsStTl]{9}[@+.]?\s+\d+\s+\S+\s+\S+\s+\S+\s+\S+\s+\S+\s+(?:\d{1,2}:\d{2}|\d{4})\s+(.+)$",
    )
    .unwrap()
});
static TREE_COUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+ director(?:y|ies)(?:, \d+ files?)?$").unwrap());

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Paths,
    Ls,
    Tree,
}

struct Dir {
    entries: usize,
    first: usize,
    last: usize,
    names: Vec<String>,
    noise: bool,
}

/// Groups listed paths by directory while the stream is scanned.
pub struct Listing {
    format: Format,
    keep: Vec<String>,
    roots: Vec<String>,
    base: String,
    dirs: BTreeMap<String, Dir>,
    total: usize,
    records: usize,
    unparsed: usize,
    other: Option<(usize, String)>,
    section: Option<String>,
    started: bool,
    blank: bool,
    root: Option<String>,
    stack: Vec<String>,
    count_line: Option<String>,
    overflow: bool,
}

impl Listing {
    pub fn new(words: &[&str]) -> Self {
        let words = crate::filters::unwrap_tool(words);
        let program = words
            .first()
            .and_then(|p| Path::new(p).file_name())
            .and_then(|p| p.to_str())
            .unwrap_or("");
        let format = match program {
            "ls" => Format::Ls,
            "tree" => Format::Tree,
            _ => Format::Paths,
        };
        let mut listing = Self::with_format(format);
        listing.keep = named_noise(words);
        // `find` prints each starting point itself before its contents.
        if program == "find" {
            listing.roots = words[1..]
                .iter()
                .skip_while(|w| matches!(**w, "-H" | "-L" | "-P"))
                .take_while(|w| !w.starts_with('-') && !matches!(**w, "(" | ")" | "!" | ","))
                .map(|w| normalize(w).to_owned())
                .collect();
        }
        if format == Format::Ls
            && let Some(parsed) = crate::search::parse(&words[1..], &crate::search::LS)
            && let [operand] = parsed.positional.as_slice()
        {
            listing.base = normalize(operand).to_owned();
        }
        listing
    }

    /// A listing fed one plain path per call, such as each path of a status.
    /// `whole` keeps dependency and build directories instead of collapsing them.
    pub(crate) fn of_paths(whole: bool) -> Self {
        let mut listing = Self::with_format(Format::Paths);
        if whole {
            listing.keep = NOISE.iter().map(|name| (*name).to_owned()).collect();
        }
        listing
    }

    fn with_format(format: Format) -> Self {
        Self {
            format,
            keep: Vec::new(),
            roots: Vec::new(),
            base: String::new(),
            dirs: BTreeMap::new(),
            total: 0,
            records: 0,
            unparsed: 0,
            other: None,
            section: None,
            started: false,
            blank: false,
            root: None,
            stack: Vec::new(),
            count_line: None,
            overflow: false,
        }
    }

    pub fn observe(&mut self, text: &str, n: usize) {
        if self.overflow {
            return;
        }
        let line = text.trim_end_matches(['\n', '\r']);
        match self.format {
            Format::Paths if !line.is_empty() => {
                self.records += 1;
                self.add(line, n, false);
            }
            Format::Paths => {}
            Format::Ls => self.ls(line, n),
            Format::Tree => self.tree(line, n),
        }
    }

    fn ls(&mut self, line: &str, n: usize) {
        if line.is_empty() {
            self.blank = true;
            return;
        }
        let long = PERMISSIONS.is_match(line);
        let header = (self.blank || !self.started) && line.ends_with(':') && !long;
        self.started = true;
        self.blank = false;
        if header {
            self.section = Some(normalize(&line[..line.len() - 1]).to_owned());
            return;
        }
        if line
            .strip_prefix("total ")
            .is_some_and(|size| !size.contains(' '))
        {
            return;
        }
        let (name, dir) = if long {
            let Some(c) = LONG.captures(line) else {
                self.records += 1;
                self.unparsed_line(n, line);
                return;
            };
            let name = c.get(2).unwrap().as_str();
            let name = if &c[1] == "l" {
                name.split(" -> ").next().unwrap_or(name)
            } else {
                name
            };
            (name, &c[1] == "d")
        } else {
            (line, false)
        };
        if name == "." || name == ".." {
            return;
        }
        self.records += 1;
        let parent = self.section.as_deref().unwrap_or(&self.base);
        let path = join(parent, name);
        self.add(&path, n, dir);
    }

    fn tree(&mut self, line: &str, n: usize) {
        if line.trim().is_empty() {
            return;
        }
        let Some(root) = self.root.clone() else {
            self.root = Some(normalize(line).to_owned());
            return;
        };
        if TREE_COUNT.is_match(line) {
            self.count_line = Some(line.to_owned());
            return;
        }
        self.records += 1;
        let Some((depth, name)) = tree_entry(line).filter(|(d, _)| *d <= self.stack.len()) else {
            self.unparsed_line(n, line);
            return;
        };
        self.stack.truncate(depth);
        let parent = if depth == 0 {
            root
        } else {
            self.stack[depth - 1].clone()
        };
        let name = name.split(" -> ").next().unwrap_or(name);
        let clean = normalize(name);
        // `tree -f` prints each entry's full path.
        let path = if !parent.is_empty() && clean.starts_with(&format!("{parent}/")) {
            clean.to_owned()
        } else {
            join(&parent, clean)
        };
        self.stack.push(path.clone());
        self.add(&path, n, name.ends_with('/'));
    }

    fn unparsed_line(&mut self, n: usize, line: &str) {
        self.unparsed += 1;
        self.other.get_or_insert_with(|| (n, line.to_owned()));
    }

    fn add(&mut self, path: &str, n: usize, dir: bool) {
        let dir = dir || path.len() > 1 && path.ends_with('/');
        let path = normalize(path);
        self.total += 1;
        if path.is_empty() || path == "/" || self.roots.iter().any(|r| r == path) {
            return;
        }
        if let Some(prefix) = noise_prefix(path, &self.keep) {
            if let Some(group) = self.dir(prefix, n) {
                group.noise = true;
                group.entries += 1;
            }
            return;
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let parent = if parent.is_empty() && path.starts_with('/') {
            "/"
        } else {
            parent
        };
        let name = if dir {
            format!("{name}/")
        } else {
            name.to_owned()
        };
        if let Some(group) = self.dir(parent, n) {
            group.entries += 1;
            if group.names.len() < NAMES {
                group.names.push(name);
            }
        }
    }

    fn dir(&mut self, key: &str, n: usize) -> Option<&mut Dir> {
        if !self.dirs.contains_key(key) {
            if self.dirs.len() >= MAX_DIRS {
                self.overflow = true;
                return None;
            }
            self.dirs.insert(
                key.to_owned(),
                Dir {
                    entries: 0,
                    first: n,
                    last: n,
                    names: Vec::new(),
                    noise: false,
                },
            );
        }
        let dir = self.dirs.get_mut(key)?;
        dir.first = dir.first.min(n);
        dir.last = dir.last.max(n);
        Some(dir)
    }

    /// The grouped report within `budget` bytes and the comparison record, or
    /// `None` when the output was not a recognizable listing.
    pub fn finish(
        self,
        label: &str,
        budget: usize,
        previous: Option<(&str, &Summary)>,
        pointers: bool,
    ) -> Option<(String, Summary)> {
        if self.overflow || self.unparsed > (self.records / 10).max(16) {
            return None;
        }
        // Name the directory every group shares once, such as the absolute path
        // given to `find`, and show groups relative to it.
        let mut keys = self.dirs.keys();
        let mut shared: Vec<&str> = keys
            .next()
            .map_or_else(Vec::new, |k| k.split('/').collect());
        for key in keys {
            let same = shared
                .iter()
                .zip(key.split('/'))
                .take_while(|(a, b)| **a == *b)
                .count();
            shared.truncate(same);
        }
        let common = shared.join("/");
        let mut header = format!(
            "{label}: {} in {}",
            plural(self.total, "entry", "entries"),
            plural(self.dirs.len(), "directory", "directories")
        );
        if !common.is_empty() {
            header.push_str(&format!(" under {common}/"));
        }
        let mut noise: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for (key, dir) in self.dirs.iter().filter(|(_, d)| d.noise) {
            let name = key.rsplit('/').next().unwrap_or(key);
            let entry = noise.entry(name).or_default();
            entry.0 += dir.entries;
            entry.1 += 1;
        }
        if !noise.is_empty() {
            let parts: Vec<_> = noise
                .iter()
                .map(|(name, (entries, dirs))| {
                    if *dirs > 1 {
                        format!("{name} {entries} in {dirs} places")
                    } else {
                        format!("{name} {entries}")
                    }
                })
                .collect();
            header.push_str(&format!(" · collapsed {}", parts.join(", ")));
        }
        if let Some(line) = &self.count_line {
            header.push_str(&format!(" · {line}"));
        }
        let mut largest: Vec<_> = self.dirs.iter().collect();
        largest.sort_by(|a, b| b.1.entries.cmp(&a.1.entries).then(a.0.cmp(b.0)));
        let summary = Summary {
            kind: "listing".into(),
            outcomes: vec![header.clone()],
            items: largest
                .iter()
                .take(STORED)
                .map(|(key, dir)| {
                    let mut item = Item::new(display(key), "", dir.first);
                    item.digest = dir.entries.to_string();
                    item.last = dir.last;
                    item
                })
                .collect(),
            ..Summary::default()
        };
        if self.total == 0 {
            return Some((String::new(), summary));
        }
        header.push('\n');
        let current = self.dirs.iter().map(|(k, d)| (display(k), d.entries));
        header.push_str(&compare(previous, "listing", current, "no longer listed"));
        let other = self.other.as_ref().map_or_else(String::new, |(n, line)| {
            format!(
                "{} unrecognized line(s), first: {}{}\n",
                self.unparsed,
                clip(line),
                if pointers {
                    format!(" [stdout:{n}]")
                } else {
                    String::new()
                }
            )
        });
        let room = budget.saturating_sub(header.len() + other.len() + 64);
        let deepest = self.dirs.keys().map(|k| components(k)).max().unwrap_or(0);
        let top = (components(&common) + 1).min(deepest).max(1);
        for depth in (top..=deepest.max(1)).rev() {
            let views = self.views(depth);
            for k in [NAMES, 4, 2] {
                let mut body = String::new();
                for view in &views {
                    body.push_str(&self.line(view, k, pointers, &common));
                    if body.len() > room {
                        break;
                    }
                }
                if body.len() <= room {
                    return Some((format!("{header}{body}{other}"), summary));
                }
            }
        }
        // Too many top-level directories: show them in order, then count the rest.
        let views = self.views(top);
        let mut body = String::new();
        let mut shown = 0;
        for view in &views {
            let line = self.line(view, 2, pointers, &common);
            if body.len() + line.len() > room {
                break;
            }
            body.push_str(&line);
            shown += 1;
        }
        let rest = &views[shown..];
        let entries: usize = rest.iter().map(|v| v.own + v.nested).sum();
        body.push_str(&format!(
            "+{} with {}\n",
            plural(rest.len(), "more directory", "more directories"),
            plural(entries, "entry", "entries")
        ));
        Some((format!("{header}{body}{other}"), summary))
    }

    /// Directories at most `depth` levels deep, with deeper ones folded into them.
    fn views(&self, depth: usize) -> Vec<View<'_>> {
        let mut views: BTreeMap<Vec<&str>, View> = BTreeMap::new();
        for (key, dir) in &self.dirs {
            let (target, own) = if components(key) <= depth {
                (key.as_str(), true)
            } else {
                (ancestor(key, depth), false)
            };
            let parts = if target.is_empty() {
                Vec::new()
            } else {
                target.split('/').collect()
            };
            let view = views.entry(parts).or_insert(View {
                key: target,
                dir: None,
                own: 0,
                nested: 0,
                nested_dirs: 0,
                first: dir.first,
                last: dir.last,
            });
            view.first = view.first.min(dir.first);
            view.last = view.last.max(dir.last);
            if own {
                view.dir = Some(dir);
                view.own = dir.entries;
            } else {
                view.nested += dir.entries;
                view.nested_dirs += 1;
            }
        }
        views.into_values().collect()
    }

    fn line(&self, view: &View, k: usize, pointers: bool, common: &str) -> String {
        let relative = if common.is_empty() {
            view.key
        } else if view.key == common {
            ""
        } else {
            view.key
                .strip_prefix(common)
                .and_then(|rest| rest.strip_prefix('/'))
                .unwrap_or(view.key)
        };
        let mut text = if relative.is_empty() {
            "./".to_owned()
        } else if relative == "/" {
            "/".to_owned()
        } else {
            format!("{relative}/")
        };
        let nested = (view.nested > 0).then(|| {
            format!(
                "+{} in {}",
                view.nested,
                plural(view.nested_dirs, "subdirectory", "subdirectories")
            )
        });
        match view.dir {
            Some(dir) if dir.noise => text.push_str(&format!(
                " {} collapsed",
                plural(view.own + view.nested, "entry", "entries")
            )),
            Some(dir) => {
                text.push_str(&format!(" {}", view.own));
                if let Some(nested) = nested {
                    text.push_str(&format!(" ({nested})"));
                }
                let names: Vec<_> = dir
                    .names
                    .iter()
                    .take(k)
                    .map(|name| self.mark(view.key, name))
                    .collect();
                text.push_str(&format!(": {}", names.join(", ")));
                if view.own > names.len() {
                    text.push_str(&format!(" +{} more", view.own - names.len()));
                }
            }
            None => text.push_str(&format!(" ({})", nested.unwrap_or_default())),
        }
        if pointers {
            if view.first == view.last {
                text.push_str(&format!(" [stdout:{}]", view.first));
            } else {
                text.push_str(&format!(" [stdout:{}-{}]", view.first, view.last));
            }
        }
        text.push('\n');
        text
    }

    /// Mark a name as a directory when other listed paths sit inside it.
    fn mark(&self, parent: &str, name: &str) -> String {
        if !name.ends_with('/') && self.dirs.contains_key(&join(parent, name)) {
            format!("{name}/")
        } else {
            name.to_owned()
        }
    }
}

/// Dependency and build directory names that a command names as its subject, so
/// they are not collapsed.
pub(crate) fn named_noise(words: &[&str]) -> Vec<String> {
    words
        .iter()
        .skip(1)
        .filter(|w| !w.starts_with('-'))
        .flat_map(|w| w.split('/'))
        .filter(|c| NOISE.contains(c))
        .map(str::to_owned)
        .collect()
}

/// The path up to the first dependency or build directory that contains it.
pub(crate) fn noise_prefix<'a>(path: &'a str, keep: &[String]) -> Option<&'a str> {
    let segments: Vec<&str> = path.split('/').collect();
    let mut end = 0;
    for (i, segment) in segments.iter().enumerate() {
        end += segment.len();
        if i + 1 < segments.len() && NOISE.contains(segment) && !keep.iter().any(|k| k == segment) {
            return Some(&path[..end]);
        }
        end += 1;
    }
    None
}

struct View<'a> {
    key: &'a str,
    dir: Option<&'a Dir>,
    own: usize,
    nested: usize,
    nested_dirs: usize,
    first: usize,
    last: usize,
}

/// Compact a file list from an agent tool, such as Claude Code's Glob, relative to
/// `base` when every path sits under it.
pub fn paths(
    tool: &str,
    names: &[&str],
    base: &Path,
    words: &[&str],
    budget: usize,
) -> Option<String> {
    let base = base.to_str()?.trim_end_matches('/');
    let prefix = format!("{base}/");
    let relative = !base.is_empty() && names.iter().all(|n| n.starts_with(&prefix));
    let mut listing = Listing::with_format(Format::Paths);
    listing.keep = named_noise(words);
    for (i, name) in names.iter().enumerate() {
        listing.observe(
            if relative {
                &name[prefix.len()..]
            } else {
                name
            },
            i + 1,
        );
    }
    let label = if relative {
        format!("{tool} in {base}")
    } else {
        tool.to_owned()
    };
    listing
        .finish(&label, budget, None, false)
        .map(|(report, _)| report)
}

fn tree_entry(line: &str) -> Option<(usize, &str)> {
    let blank = |c: char| c == ' ' || c == '\u{a0}';
    let mut rest = line;
    let mut depth = 0;
    loop {
        let unit: Vec<char> = rest.chars().take(4).collect();
        let [a, b, c, d] = unit[..] else {
            return None;
        };
        let width: usize = unit.iter().map(|c| c.len_utf8()).sum();
        if (matches!(a, '├' | '└') && b == '─' && c == '─'
            || matches!(a, '|' | '`') && b == '-' && c == '-')
            && blank(d)
        {
            return Some((depth, &rest[width..]));
        }
        if !(matches!(a, '│' | '|') || blank(a)) || !blank(b) || !blank(c) || !blank(d) {
            return None;
        }
        rest = &rest[width..];
        depth += 1;
    }
}

fn normalize(path: &str) -> &str {
    let mut path = path;
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() && path.starts_with('/') {
        "/"
    } else if trimmed == "." {
        ""
    } else {
        trimmed
    }
}

fn join(parent: &str, name: &str) -> String {
    match parent {
        "" => name.to_owned(),
        "/" => format!("/{name}"),
        _ => format!("{parent}/{name}"),
    }
}

fn components(key: &str) -> usize {
    if key.is_empty() {
        0
    } else {
        key.split('/').count()
    }
}

fn ancestor(key: &str, depth: usize) -> &str {
    if depth == 0 {
        return "";
    }
    key.match_indices('/')
        .nth(depth - 1)
        .map_or(key, |(i, _)| &key[..i])
}

fn display(key: &str) -> &str {
    if key.is_empty() { "." } else { key }
}
