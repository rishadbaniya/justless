//! Group file listings by directory: `find`, `fd`, `git ls-files`, `ls -l`/`-R`,
//! `tree`, and the file lists printed by `rg --files`, `rg -l` and `grep -l`.
//!
//! Every listed path counts once, so totals stay exact. Dependency, build and cache
//! directories collapse into a count unless the command names them. Single-child
//! directory chains compact onto one path, dense directories report their exact
//! extension breakdown and first/last entries, and non-recursive `ls -l` listings
//! hoist repeated permissions, link count and ownership into a default header while
//! preserving every row's size, timestamp, filename, symlink target and metadata
//! override. Each group links to the original lines it came from. When the groups do
//! not fit the budget, deeper directories fold into their ancestors with a count.
use std::collections::{BTreeMap, HashMap, HashSet};
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
const DENSE_DIR: usize = 12;
const MAX_DIRS: usize = 100_000;
const MAX_LONG_ROWS: usize = 2_000;
const MAX_BARE_CANDIDATES: usize = 2_048;
const MAX_TRACKED_EXTS: usize = 64;

static PERMISSIONS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-dlcbpsDnw?][-rwxsStTl]{9}[@+.]?\s").unwrap());
// Mode, links, owner, group, size, month, day, then a time or year before the name.
static LONG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^([-dlcbpsDnw?][-rwxsStTl]{9}[@+.]?)\s+(\d+)\s+(\S+)\s+(\S+)\s+((?:\d+,\s+)?\S+)\s+(\d{4}-\d{2}-\d{2}(?:\s+\d{2}:\d{2}(?::\d{2}(?:\.\d+)?(?:\s+(?:[+-]\d{4}|Z))?)?)?|\d{2}-\d{2}\s+\d{2}:\d{2}|\S+\s+\S+\s+(?:\d{1,2}:\d{2}|\d{4}))\s+(.+)$",
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

struct LongRow {
    line: usize,
    dir_key: String,
    perms: String,
    links: String,
    owner: String,
    group: String,
    size: String,
    date: String,
    name: String,
}

impl LongRow {
    fn kind(&self) -> EntryKind {
        match self.perms.as_bytes().first() {
            Some(b'-') => EntryKind::File,
            Some(b'd') => EntryKind::Dir,
            Some(b'l') => EntryKind::Symlink,
            _ => EntryKind::Other,
        }
    }

    fn plain_name(&self) -> &str {
        if self.kind() == EntryKind::Symlink {
            self.name.split(" -> ").next().unwrap_or(&self.name)
        } else {
            self.name.trim_end_matches('/')
        }
    }
}

struct Dir {
    entries: usize,
    files: usize,
    subdirs: usize,
    first: usize,
    last: usize,
    names: Vec<String>,
    min_name: Option<String>,
    max_name: Option<String>,
    exts: BTreeMap<String, usize>,
    other_exts: usize,
    last_bare: Option<String>,
    bare_candidates: Option<HashSet<String>>,
    noise: bool,
}

impl Dir {
    fn new(n: usize) -> Self {
        Self {
            entries: 0,
            files: 0,
            subdirs: 0,
            first: n,
            last: n,
            names: Vec::new(),
            min_name: None,
            max_name: None,
            exts: BTreeMap::new(),
            other_exts: 0,
            last_bare: None,
            bare_candidates: Some(HashSet::new()),
            noise: false,
        }
    }

    fn record_entry(&mut self, name: String, is_dir: bool) {
        self.entries += 1;
        let bare = name.trim_end_matches('/');
        if self
            .min_name
            .as_deref()
            .is_none_or(|min| bare < min.trim_end_matches('/'))
        {
            self.min_name = Some(name.clone());
        }
        if self
            .max_name
            .as_deref()
            .is_none_or(|max| bare > max.trim_end_matches('/'))
        {
            self.max_name = Some(name.clone());
        }
        if is_dir {
            self.subdirs += 1;
            self.last_bare = None;
        } else {
            self.files += 1;
            let ext = extension_of(&name);
            add_ext(&mut self.exts, &mut self.other_exts, ext, 1);
            self.last_bare = Some(name.clone());
            if let Some(set) = self.bare_candidates.as_mut() {
                if set.len() < MAX_BARE_CANDIDATES {
                    set.insert(name.clone());
                } else {
                    self.bare_candidates = None;
                }
            }
        }
        if self.names.len() < NAMES {
            self.names.push(name);
        }
    }

    /// Reclassify an entry previously seen without a trailing `/` once a child path
    /// proves it is a directory.
    fn promote_to_subdir(&mut self, name: &str) {
        let was_last = self.last_bare.as_deref() == Some(name);
        let was_candidate = self
            .bare_candidates
            .as_mut()
            .is_some_and(|set| set.remove(name));
        if !was_last && !was_candidate {
            return;
        }
        if was_last {
            self.last_bare = None;
        }
        if self.files > 0 {
            self.files -= 1;
        }
        self.subdirs += 1;
        let ext = extension_of(name);
        if let Some(count) = self.exts.get_mut(&ext) {
            if *count <= 1 {
                self.exts.remove(&ext);
            } else {
                *count -= 1;
            }
        } else if self.other_exts > 0 {
            self.other_exts -= 1;
        }
    }
}

/// Groups listed paths by directory while the stream is scanned.
pub struct Listing {
    format: Format,
    recursive: bool,
    dense_breakdown: bool,
    keep: Vec<String>,
    roots: Vec<String>,
    base: String,
    dirs: BTreeMap<String, Dir>,
    long_rows: Vec<LongRow>,
    long_overflow: bool,
    has_short: bool,
    raw_bytes: usize,
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
        {
            listing.recursive = parsed.has(&["-R", "--recursive"]);
            if let [operand] = parsed.positional.as_slice() {
                listing.base = normalize(operand).to_owned();
            }
        }
        listing
    }

    /// A listing fed one plain path per call, such as each path of a status.
    /// `whole` keeps dependency and build directories instead of collapsing them.
    pub(crate) fn of_paths(whole: bool) -> Self {
        let mut listing = Self::with_format(Format::Paths);
        listing.dense_breakdown = false;
        if whole {
            listing.keep = NOISE.iter().map(|name| (*name).to_owned()).collect();
        }
        listing
    }

    fn with_format(format: Format) -> Self {
        Self {
            format,
            recursive: false,
            dense_breakdown: true,
            keep: Vec::new(),
            roots: Vec::new(),
            base: String::new(),
            dirs: BTreeMap::new(),
            long_rows: Vec::new(),
            long_overflow: false,
            has_short: false,
            raw_bytes: 0,
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
        self.raw_bytes = self.raw_bytes.saturating_add(line.len() + 1);
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
        if long {
            let Some(c) = LONG.captures(line) else {
                self.records += 1;
                self.unparsed_line(n, line);
                return;
            };
            let perms = c.get(1).unwrap().as_str();
            let links = c.get(2).unwrap().as_str();
            let owner = c.get(3).unwrap().as_str();
            let group = c.get(4).unwrap().as_str();
            let size = collapse_spaces(c.get(5).unwrap().as_str());
            let date = collapse_spaces(c.get(6).unwrap().as_str());
            let raw_name = c.get(7).unwrap().as_str();
            let is_dir = perms.starts_with('d');
            let is_link = perms.starts_with('l');
            let name = if is_link {
                raw_name.split(" -> ").next().unwrap_or(raw_name)
            } else {
                raw_name
            };
            if name == "." || name == ".." {
                return;
            }
            self.records += 1;
            let parent = self.section.as_deref().unwrap_or(&self.base);
            let path = join(parent, name);
            if !self.recursive && !self.long_overflow {
                if self.long_rows.len() >= MAX_LONG_ROWS {
                    self.long_rows.clear();
                    self.long_overflow = true;
                } else {
                    let norm = normalize(&path);
                    let dir_key = parent_key(norm).to_owned();
                    let display_name = if is_dir && !raw_name.ends_with('/') {
                        format!("{raw_name}/")
                    } else {
                        raw_name.to_owned()
                    };
                    self.long_rows.push(LongRow {
                        line: n,
                        dir_key,
                        perms: perms.to_owned(),
                        links: links.to_owned(),
                        owner: owner.to_owned(),
                        group: group.to_owned(),
                        size,
                        date,
                        name: display_name,
                    });
                }
            }
            self.add(&path, n, is_dir);
        } else {
            let name = line;
            if name == "." || name == ".." {
                return;
            }
            self.has_short = true;
            self.records += 1;
            let parent = self.section.as_deref().unwrap_or(&self.base);
            let path = join(parent, name);
            self.add(&path, n, false);
        }
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
        let trailing_slash = path.len() > 1 && path.ends_with('/');
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
        let dir = dir || trailing_slash || self.dirs.contains_key(path);
        let (parent, name) = split_parent(path);
        let name = if dir {
            format!("{name}/")
        } else {
            name.to_owned()
        };
        if let Some(group) = self.dir(parent, n) {
            group.record_entry(name, dir);
        }
    }

    fn dir(&mut self, key: &str, n: usize) -> Option<&mut Dir> {
        if !self.dirs.contains_key(key) {
            if self.dirs.len() >= MAX_DIRS {
                self.overflow = true;
                return None;
            }
            // Creating `key` proves `key` is a directory, so promote it in its parent
            // if the parent previously saw it as a bare name without a trailing `/`.
            if !key.is_empty() && key != "/" {
                let (grandparent, child_name) = split_parent(key);
                if let Some(parent_dir) = self.dirs.get_mut(grandparent) {
                    parent_dir.promote_to_subdir(child_name);
                }
            }
            self.dirs.insert(key.to_owned(), Dir::new(n));
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
        let hoist_eligible = self.format == Format::Ls
            && !self.recursive
            && !self.has_short
            && !self.long_overflow
            && !self.long_rows.is_empty()
            && noise.is_empty();
        if self.count_line.is_none() {
            let breakdown = if hoist_eligible {
                self.long_kind_breakdown()
            } else {
                self.dir_kind_breakdown()
            };
            if !breakdown.is_empty() {
                header.push_str(&format!(" · {breakdown}"));
            }
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
        if hoist_eligible
            && let Some(hoisted) =
                self.render_hoisted_ls(&header, &other, budget, pointers, &common)
        {
            return Some((hoisted, summary));
        }
        let active = self.active_dirs();
        let room = budget.saturating_sub(header.len() + other.len() + 64);
        let deepest = active.iter().map(|d| components(d.key)).max().unwrap_or(0);
        let top = (components(&common) + 1).min(deepest).max(1);
        for depth in (top..=deepest.max(1)).rev() {
            let views = self.views(&active, depth);
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
        let views = self.views(&active, top);
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

    fn long_kind_breakdown(&self) -> String {
        let (mut files, mut dirs, mut links, mut other) = (0, 0, 0, 0);
        for row in &self.long_rows {
            match row.kind() {
                EntryKind::File => files += 1,
                EntryKind::Dir => dirs += 1,
                EntryKind::Symlink => links += 1,
                EntryKind::Other => other += 1,
            }
        }
        format_kind_counts(files, dirs, links, other)
    }

    fn dir_kind_breakdown(&self) -> String {
        let (mut files, mut dirs) = (0, 0);
        for dir in self.dirs.values().filter(|d| !d.noise) {
            files += dir.files;
            dirs += dir.subdirs;
        }
        format_kind_counts(files, dirs, 0, 0)
    }

    /// Hoist modal `(perms, links, owner, group)` on non-recursive `ls -l` output,
    /// preserving every file size, timestamp, filename, symlink target and metadata
    /// override. Returns `None` unless the result fits `budget` and is strictly
    /// smaller than the raw `ls -l` bytes.
    fn render_hoisted_ls(
        &self,
        header: &str,
        other: &str,
        budget: usize,
        pointers: bool,
        common: &str,
    ) -> Option<String> {
        let modal_perms = modal_str(self.long_rows.iter().map(|r| r.perms.as_str()));
        let modal_links = modal_str(self.long_rows.iter().map(|r| r.links.as_str()));
        let (modal_owner, modal_group) = modal_owner_group(&self.long_rows);
        let single_dir = self.dirs.len() == 1;
        let default_line = if single_dir {
            let first = self.long_rows.iter().map(|r| r.line).min()?;
            let last = self.long_rows.iter().map(|r| r.line).max()?;
            let ptr = format_pointer(pointers, first, last);
            format!("default: {modal_perms} {modal_links} {modal_owner}:{modal_group}{ptr}\n")
        } else {
            format!("default: {modal_perms} {modal_links} {modal_owner}:{modal_group}\n")
        };
        let fixed = header.len() + default_line.len() + other.len();
        if fixed >= budget || fixed >= self.raw_bytes {
            return None;
        }
        let room = budget - fixed;

        if single_dir {
            let formatted: Vec<String> = self
                .long_rows
                .iter()
                .map(|row| format_long_row(row, modal_perms, modal_links, modal_owner, modal_group))
                .collect();
            let total_body: usize = formatted.iter().map(String::len).sum();
            if total_body <= room && fixed + total_body < self.raw_bytes {
                let mut out = String::with_capacity(fixed + total_body);
                out.push_str(header);
                out.push_str(&default_line);
                for row in &formatted {
                    out.push_str(row);
                }
                out.push_str(other);
                return Some(out);
            }
            // Huge single-directory `ls -l`: keep hoisted head & tail rows with an
            // exact extension breakdown and line-range pointer for the omitted middle.
            const MIDDLE_RESERVE: usize = 192;
            let avail = room.checked_sub(MIDDLE_RESERVE)?;
            let head_budget = (avail * 2) / 3;
            let mut head_n = 0;
            let mut head_bytes = 0;
            for line in &formatted {
                if head_bytes + line.len() > head_budget {
                    break;
                }
                head_bytes += line.len();
                head_n += 1;
            }
            let tail_budget = avail.saturating_sub(head_bytes);
            let mut tail_n = 0;
            let mut tail_bytes = 0;
            for line in formatted[head_n..].iter().rev() {
                if tail_bytes + line.len() > tail_budget {
                    break;
                }
                tail_bytes += line.len();
                tail_n += 1;
            }
            if head_n < 2 || tail_n < 1 || head_n + tail_n >= formatted.len() {
                return None;
            }
            let tail_start = formatted.len() - tail_n;
            let middle = &self.long_rows[head_n..tail_start];
            let fold = format_hoisted_middle(
                middle,
                modal_perms,
                modal_links,
                modal_owner,
                modal_group,
                pointers,
            );
            let total_len = fixed + head_bytes + fold.len() + tail_bytes;
            if total_len <= budget && total_len < self.raw_bytes {
                let mut out = String::with_capacity(total_len);
                out.push_str(header);
                out.push_str(&default_line);
                for row in &formatted[..head_n] {
                    out.push_str(row);
                }
                out.push_str(&fold);
                for row in &formatted[tail_start..] {
                    out.push_str(row);
                }
                out.push_str(other);
                return Some(out);
            }
            return None;
        }

        // Multi-directory non-recursive `ls -l`: keep every section when it fits whole
        // and saves bytes over the raw listing.
        let mut body = String::new();
        let mut i = 0;
        while i < self.long_rows.len() {
            let dir_key = &self.long_rows[i].dir_key;
            let start = i;
            let mut first = self.long_rows[i].line;
            let mut last = first;
            while i < self.long_rows.len() && self.long_rows[i].dir_key == *dir_key {
                first = first.min(self.long_rows[i].line);
                last = last.max(self.long_rows[i].line);
                i += 1;
            }
            let section = &self.long_rows[start..i];
            let label = relative_dir(dir_key, common);
            let ptr = format_pointer(pointers, first, last);
            body.push_str(&format!(
                "{label} ({}){ptr}\n",
                plural(section.len(), "entry", "entries")
            ));
            for row in section {
                body.push_str("  ");
                body.push_str(&format_long_row(
                    row,
                    modal_perms,
                    modal_links,
                    modal_owner,
                    modal_group,
                ));
                if body.len() > room {
                    return None;
                }
            }
        }
        let total_len = fixed + body.len();
        if total_len <= budget && total_len < self.raw_bytes {
            Some(format!("{header}{default_line}{body}{other}"))
        } else {
            None
        }
    }

    /// Collapse single-child pass-through directory chains (such as
    /// `src/main/java/com/acme/billing/`) onto their first branching or non-empty
    /// descendant, merging line-range pointers across the collapsed chain.
    fn active_dirs(&self) -> Vec<ActiveDir<'_>> {
        let mut pass_through: HashMap<&str, String> = HashMap::new();
        for (key, dir) in &self.dirs {
            if !dir.noise && dir.entries == 1 && dir.names.len() == 1 {
                let child_name = dir.names[0].trim_end_matches('/');
                let child_key = join(key, child_name);
                if child_key.len() > key.len() && self.dirs.contains_key(&child_key) {
                    pass_through.insert(key.as_str(), child_key);
                }
            }
        }
        let mut spans: HashMap<&str, (usize, usize)> = HashMap::new();
        for (key, dir) in &self.dirs {
            if !pass_through.contains_key(key.as_str()) {
                spans.insert(key.as_str(), (dir.first, dir.last));
            }
        }
        for (key, dir) in &self.dirs {
            if !pass_through.contains_key(key.as_str()) {
                continue;
            }
            let mut cur = key.as_str();
            while let Some(next) = pass_through.get(cur) {
                cur = next.as_str();
            }
            if let Some((first, last)) = spans.get_mut(cur) {
                *first = (*first).min(dir.first);
                *last = (*last).max(dir.last);
            }
        }
        self.dirs
            .iter()
            .filter(|(k, _)| !pass_through.contains_key(k.as_str()))
            .map(|(k, dir)| {
                let (first, last) = spans
                    .get(k.as_str())
                    .copied()
                    .unwrap_or((dir.first, dir.last));
                ActiveDir {
                    key: k.as_str(),
                    dir,
                    first,
                    last,
                }
            })
            .collect()
    }

    /// Directories at most `depth` levels deep, with deeper ones folded into them.
    fn views<'a>(&'a self, active: &[ActiveDir<'a>], depth: usize) -> Vec<View<'a>> {
        let mut views: BTreeMap<Vec<&'a str>, View<'a>> = BTreeMap::new();
        for item in active {
            let (target, own) = if components(item.key) <= depth {
                (item.key, true)
            } else {
                (ancestor(item.key, depth), false)
            };
            let parts = if target.is_empty() {
                Vec::new()
            } else {
                target.split('/').collect()
            };
            let view = views.entry(parts).or_insert(View {
                key: target,
                dir: None,
                sole: None,
                own: 0,
                nested: 0,
                nested_dirs: 0,
                nested_exts: BTreeMap::new(),
                nested_other_exts: 0,
                nested_common: None,
                first: item.first,
                last: item.last,
            });
            view.first = view.first.min(item.first);
            view.last = view.last.max(item.last);
            if own {
                view.dir = Some(item.dir);
                view.own = item.dir.entries;
            } else {
                view.nested += item.dir.entries;
                view.nested_dirs += 1;
                if view.nested_dirs == 1 {
                    view.sole = Some((item.key, item.dir));
                    view.nested_common = Some(item.key);
                } else {
                    view.sole = None;
                    view.nested_common = view
                        .nested_common
                        .map(|prev| common_path_prefix(prev, item.key));
                }
                if !item.dir.noise {
                    merge_exts(
                        &mut view.nested_exts,
                        &mut view.nested_other_exts,
                        &item.dir.exts,
                        item.dir.other_exts,
                    );
                }
            }
        }
        let mut result: Vec<View<'a>> = views.into_values().collect();
        for view in &mut result {
            if view.dir.is_none() {
                if let Some((sole_key, sole_dir)) = view.sole {
                    // Promote a lone folded directory directly instead of hiding its path
                    // behind a shallow ancestor with `(+N in 1 subdirectory)`.
                    view.key = sole_key;
                    view.dir = Some(sole_dir);
                    view.own = sole_dir.entries;
                    view.nested = 0;
                    view.nested_dirs = 0;
                    view.nested_exts.clear();
                    view.nested_other_exts = 0;
                } else if let Some(prefix) = view.nested_common
                    && prefix.len() > view.key.len()
                {
                    // Preserve the deepest common single-child prefix of folded subdirs.
                    view.key = prefix;
                }
            }
        }
        result
    }

    fn line(&self, view: &View, k: usize, pointers: bool, common: &str) -> String {
        let mut text = relative_dir(view.key, common);
        let nested = (view.nested > 0).then(|| {
            let base = format!(
                "+{} in {}",
                view.nested,
                plural(view.nested_dirs, "subdirectory", "subdirectories")
            );
            if !view.nested_exts.is_empty() || view.nested_other_exts > 0 {
                let max_exts = match k {
                    6.. => 4,
                    4.. => 3,
                    _ => 2,
                };
                format!(
                    "{base}: {}",
                    format_exts(&view.nested_exts, view.nested_other_exts, max_exts)
                )
            } else {
                base
            }
        });
        match view.dir {
            Some(dir) if dir.noise => text.push_str(&format!(
                " {} collapsed",
                plural(view.own + view.nested, "entry", "entries")
            )),
            Some(dir) if self.dense_breakdown && view.own > DENSE_DIR => {
                text.push(' ');
                text.push_str(&self.format_dense_dir(view.key, dir, k, nested.as_deref()));
            }
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
        text.push_str(&format_pointer(pointers, view.first, view.last));
        text.push('\n');
        text
    }

    fn format_dense_dir(&self, key: &str, dir: &Dir, k: usize, nested: Option<&str>) -> String {
        let max_exts = match k {
            6.. => 5,
            4.. => 3,
            _ => 2,
        };
        let mut out = String::from("(");
        if dir.files > 0 && dir.subdirs == 0 {
            out.push_str(&format!(
                "{}: {}",
                plural(dir.files, "file", "files"),
                format_exts(&dir.exts, dir.other_exts, max_exts)
            ));
        } else if dir.files > 0 && dir.subdirs > 0 {
            out.push_str(&format!(
                "{}, {}: {}",
                plural(dir.files, "file", "files"),
                plural(dir.subdirs, "dir", "dirs"),
                format_exts(&dir.exts, dir.other_exts, max_exts)
            ));
        } else {
            out.push_str(&plural(dir.subdirs, "dir", "dirs"));
        }
        if let Some(nested) = nested {
            out.push_str("; ");
            out.push_str(nested);
        }
        if k >= 4
            && let (Some(first), Some(last)) = (&dir.min_name, &dir.max_name)
        {
            let first = self.mark(key, first);
            let last = self.mark(key, last);
            if first == last {
                out.push_str(&format!("; first: {first}"));
            } else {
                out.push_str(&format!("; first: {first} … last: {last}"));
            }
        }
        out.push(')');
        out
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

struct ActiveDir<'a> {
    key: &'a str,
    dir: &'a Dir,
    first: usize,
    last: usize,
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
    sole: Option<(&'a str, &'a Dir)>,
    own: usize,
    nested: usize,
    nested_dirs: usize,
    nested_exts: BTreeMap<String, usize>,
    nested_other_exts: usize,
    nested_common: Option<&'a str>,
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

fn format_pointer(pointers: bool, first: usize, last: usize) -> String {
    if !pointers {
        String::new()
    } else if first == last {
        format!(" [stdout:{first}]")
    } else {
        format!(" [stdout:{first}-{last}]")
    }
}

fn relative_dir(key: &str, common: &str) -> String {
    let relative = if common.is_empty() {
        key
    } else if key == common {
        ""
    } else {
        key.strip_prefix(common)
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(key)
    };
    if relative.is_empty() {
        "./".to_owned()
    } else if relative == "/" {
        "/".to_owned()
    } else {
        format!("{relative}/")
    }
}

fn format_kind_counts(files: usize, dirs: usize, links: usize, other: usize) -> String {
    let mut parts = Vec::new();
    if files > 0 {
        parts.push(plural(files, "file", "files"));
    }
    if dirs > 0 {
        parts.push(plural(dirs, "dir", "dirs"));
    }
    if links > 0 {
        parts.push(plural(links, "symlink", "symlinks"));
    }
    if other > 0 {
        parts.push(plural(other, "special", "special"));
    }
    parts.join(", ")
}

fn modal_str<'a>(iter: impl Iterator<Item = &'a str>) -> &'a str {
    let mut counts: BTreeMap<&'a str, usize> = BTreeMap::new();
    for item in iter {
        *counts.entry(item).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| a.0.len().cmp(&b.0.len()))
                .then_with(|| b.0.cmp(a.0))
        })
        .map_or("", |(k, _)| k)
}

fn modal_owner_group(rows: &[LongRow]) -> (&str, &str) {
    let mut counts: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for row in rows {
        *counts
            .entry((row.owner.as_str(), row.group.as_str()))
            .or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| (a.0.0.len() + a.0.1.len()).cmp(&(b.0.0.len() + b.0.1.len())))
                .then_with(|| b.0.cmp(&a.0))
        })
        .map_or(("", ""), |(k, _)| k)
}

fn format_long_row(
    row: &LongRow,
    modal_perms: &str,
    modal_links: &str,
    modal_owner: &str,
    modal_group: &str,
) -> String {
    let mut out = String::new();
    let perms_diff = row.perms != modal_perms;
    let links_diff = row.links != modal_links;
    let og_diff = row.owner != modal_owner || row.group != modal_group;
    if perms_diff {
        out.push_str(&row.perms);
        out.push(' ');
        if links_diff {
            out.push_str(&row.links);
            out.push(' ');
        }
    } else if links_diff {
        out.push_str("links=");
        out.push_str(&row.links);
        out.push(' ');
    }
    if og_diff {
        out.push_str(&row.owner);
        out.push(':');
        out.push_str(&row.group);
        out.push(' ');
    }
    out.push_str(&row.size);
    out.push(' ');
    out.push_str(&row.date);
    out.push(' ');
    out.push_str(&row.name);
    out.push('\n');
    out
}

fn format_hoisted_middle(
    middle: &[LongRow],
    modal_perms: &str,
    modal_links: &str,
    modal_owner: &str,
    modal_group: &str,
    pointers: bool,
) -> String {
    let (mut files, mut dirs, mut links, mut other, mut non_default) = (0, 0, 0, 0, 0);
    let mut exts = BTreeMap::new();
    let mut other_exts = 0;
    let mut first = usize::MAX;
    let mut last = 0;
    for row in middle {
        first = first.min(row.line);
        last = last.max(row.line);
        if row.perms != modal_perms
            || row.links != modal_links
            || row.owner != modal_owner
            || row.group != modal_group
        {
            non_default += 1;
        }
        match row.kind() {
            EntryKind::File => {
                files += 1;
                add_ext(
                    &mut exts,
                    &mut other_exts,
                    extension_of(row.plain_name()),
                    1,
                );
            }
            EntryKind::Dir => dirs += 1,
            EntryKind::Symlink => links += 1,
            EntryKind::Other => other += 1,
        }
    }
    let mut details = Vec::new();
    if files > 0 {
        details.push(format!(
            "{}: {}",
            plural(files, "file", "files"),
            format_exts(&exts, other_exts, 4)
        ));
    }
    if dirs > 0 {
        details.push(plural(dirs, "dir", "dirs"));
    }
    if links > 0 {
        details.push(plural(links, "symlink", "symlinks"));
    }
    if other > 0 {
        details.push(plural(other, "special", "special"));
    }
    if non_default > 0 {
        details.push(format!("{non_default} non-default"));
    }
    let ptr = format_pointer(pointers, first, last);
    format!("… +{} more ({}){ptr}\n", middle.len(), details.join("; "))
}

fn extension_of(name: &str) -> String {
    let bare = name.trim_end_matches('/');
    match Path::new(bare).extension().and_then(|e| e.to_str()) {
        Some(ext) if !ext.is_empty() && ext.len() <= 16 && !ext.contains(char::is_whitespace) => {
            format!(".{}", ext.to_ascii_lowercase())
        }
        _ => "(no ext)".to_owned(),
    }
}

fn add_ext(exts: &mut BTreeMap<String, usize>, other: &mut usize, ext: String, count: usize) {
    if let Some(existing) = exts.get_mut(&ext) {
        *existing += count;
    } else if exts.len() < MAX_TRACKED_EXTS {
        exts.insert(ext, count);
    } else {
        *other += count;
    }
}

fn merge_exts(
    target: &mut BTreeMap<String, usize>,
    target_other: &mut usize,
    src: &BTreeMap<String, usize>,
    src_other: usize,
) {
    for (ext, count) in src {
        add_ext(target, target_other, ext.clone(), *count);
    }
    *target_other += src_other;
}

fn format_exts(exts: &BTreeMap<String, usize>, other: usize, max_shown: usize) -> String {
    let mut ranked: Vec<(&str, usize)> = exts.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let mut parts: Vec<String> = ranked
        .iter()
        .take(max_shown)
        .map(|(ext, count)| format!("{count} {ext}"))
        .collect();
    let rest: usize = ranked
        .iter()
        .skip(max_shown)
        .map(|(_, c)| *c)
        .sum::<usize>()
        + other;
    if rest > 0 {
        parts.push(format!("+{rest} other"));
    }
    parts.join(", ")
}

fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for ch in text.chars() {
        if ch.is_ascii_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

fn split_parent(path: &str) -> (&str, &str) {
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    let parent = if parent.is_empty() && path.starts_with('/') {
        "/"
    } else {
        parent
    };
    (parent, name)
}

fn parent_key(path: &str) -> &str {
    split_parent(path).0
}

fn common_path_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let mut len = 0;
    let mut matched = 0;
    for (sa, sb) in a.split('/').zip(b.split('/')) {
        if sa != sb {
            break;
        }
        if matched > 0 {
            len += 1;
        }
        len += sa.len();
        matched += 1;
    }
    if matched == 1 && len == 0 && a.starts_with('/') {
        "/"
    } else {
        &a[..len]
    }
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

#[cfg(test)]
mod tests {
    use super::Listing;

    fn run_listing(words: &[&str], label: &str, raw: &str, budget: usize) -> String {
        let mut listing = Listing::new(words);
        for (i, line) in raw.lines().enumerate() {
            listing.observe(line, i + 1);
        }
        assert_eq!(listing.unparsed, 0, "{:?}", listing.other);
        listing
            .finish(label, budget, None, true)
            .expect("listing report")
            .0
    }

    #[test]
    fn long_listings_parse_posix_iso_full_time_and_device_rows() {
        let lines = [
            "total 32",
            "drwxr-xr-x 2 user group 4096 Nov 25 14:43 .",
            "drwxr-xr-x 4 user group 4096 Nov 25  2025 ..",
            "-rw-r--r-- 1 user group 1234 Nov 25 14:43 posix_recent.rs",
            "-rw-r--r-- 1 user group 1234 Nov 25  2025 posix_old.rs",
            "-rw-r--r-- 1 user group 1234 25 Nov 14:43 euro_recent.rs",
            "-rw-r--r-- 1 user group 1234 2026-11-25 14:43 long iso file.rs",
            "-rw-r--r-- 1 user group 1234 2025-05-01 iso_old.rs",
            "-rw-r--r-- 1 user group 1234 11-25 14:43 iso_recent.rs",
            "drwxr-xr-x 2 user group 4096 2026-11-25 14:43:12.012345678 +0545 full_iso_dir",
            "lrwxrwxrwx 1 user group   11 2026-11-25 14:43 latest -> release-1.0",
            "crw-rw-rw- 1 root root 1, 3 Nov 25 14:43 null",
        ];
        let mut listing = Listing::new(&["ls", "-la"]);
        for (i, line) in lines.iter().enumerate() {
            listing.observe(line, i + 1);
        }
        assert_eq!(listing.unparsed, 0, "{:?}", listing.other);
        let (report, _) = listing.finish("ls", 4096, None, true).unwrap();
        assert!(report.contains("ls: 9 entries in 1 directory"), "{report}");
        for expected in [
            "posix_recent.rs",
            "long iso file.rs",
            "full_iso_dir/",
            "latest",
            "null",
        ] {
            assert!(
                report.contains(expected) || report.contains("entries"),
                "{report}"
            );
        }
        // Verify exact parsed names in the single root directory.
        let mut names = Listing::new(&["ls", "-la"]);
        for (i, line) in [
            "-rw-r--r-- 1 u g 10 2026-11-25 14:43 my file.rs",
            "-rw-r--r-- 1 u g 10 2025-05-01 14:43",
            "drwxr-xr-x 2 u g 4096 2026-11-25 14:43:12.000000000 Z sub",
            "lrwxrwxrwx 1 u g 3 11-25 14:43 link -> target",
            "crw-rw-rw- 1 root root 1, 3 2026-11-25 14:43 null",
        ]
        .iter()
        .enumerate()
        {
            names.observe(line, i + 1);
        }
        assert_eq!(names.unparsed, 0, "{:?}", names.other);
        let (report, _) = names.finish("ls", 4096, None, true).unwrap();
        for expected in ["my file.rs", "14:43", "sub/", "link", "null"] {
            assert!(
                report.contains(expected),
                "missing {expected} in:\n{report}"
            );
        }
    }

    #[test]
    fn ls_long_hoists_uniform_metadata_and_preserves_overrides() {
        let mut raw = String::from(
            "total 512\n\
             drwxr-xr-x 12 rishad rishad  4096 Nov 25 14:43 .\n\
             drwxr-xr-x  5 rishad rishad  4096 Nov 25 14:00 ..\n",
        );
        for i in 0..52 {
            raw.push_str(&format!(
                "-rw-r--r--  1 rishad rishad {:5} Nov 25 14:{:02} module_{i:02}.rs\n",
                1024 + i * 73,
                i % 60
            ));
        }
        raw.push_str("-rwxr-xr-x  1 rishad rishad   890 Nov 25 14:40 deploy.sh\n");
        raw.push_str("-rwxr-xr-x  1 rishad rishad  1420 Nov 25 14:41 build.sh\n");
        raw.push_str("drwxr-xr-x  4 rishad rishad  4096 Nov 25 14:43 src\n");
        raw.push_str("drwxr-xr-x  2 rishad rishad  4096 Nov 25 14:43 docs\n");
        raw.push_str("lrwxrwxrwx  1 rishad rishad    11 Nov 25 14:43 latest -> release-1.0\n");
        raw.push_str("-rw-r--r--  1 root   root     512 Nov 20  2025 system.conf\n");
        raw.push_str("-rw-r--r--  2 rishad rishad  2048 Nov 25 14:42 hardlink.rs\n");
        raw.push_str("-rw-r--r--+ 1 rishad rishad   640 Nov 25 14:42 acl.txt\n");

        let report = run_listing(&["ls", "-la"], "ls", &raw, 4096);
        assert!(report.len() <= 4096, "len={}\n{report}", report.len());
        // Cutting repeated perms/links/owner/group reduces realistic ls -la by >= 40%.
        assert!(
            report.len() * 100 <= raw.len() * 60,
            "expected >=40% savings: raw={} report={}\n{report}",
            raw.len(),
            report.len()
        );
        assert!(
            report.contains("ls: 60 entries in 1 directory · 57 files, 2 dirs, 1 symlink\n"),
            "{report}"
        );
        assert!(
            report.contains("default: -rw-r--r-- 1 rishad:rishad [stdout:4-63]\n"),
            "{report}"
        );
        // Matching rows drop repeated perms/owner/group while keeping exact size, date, name.
        assert!(
            report.contains("\n1024 Nov 25 14:00 module_00.rs\n"),
            "{report}"
        );
        assert!(
            report.contains("\n4747 Nov 25 14:51 module_51.rs\n"),
            "{report}"
        );
        // Non-uniform rows print exact overrides inline.
        assert!(
            report.contains("\n-rwxr-xr-x 890 Nov 25 14:40 deploy.sh\n"),
            "{report}"
        );
        assert!(
            report.contains("\ndrwxr-xr-x 4 4096 Nov 25 14:43 src/\n"),
            "{report}"
        );
        assert!(
            report.contains("\ndrwxr-xr-x 2 4096 Nov 25 14:43 docs/\n"),
            "{report}"
        );
        assert!(
            report.contains("\nlrwxrwxrwx 11 Nov 25 14:43 latest -> release-1.0\n"),
            "{report}"
        );
        assert!(
            report.contains("\nroot:root 512 Nov 20 2025 system.conf\n"),
            "{report}"
        );
        assert!(
            report.contains("\nlinks=2 2048 Nov 25 14:42 hardlink.rs\n"),
            "{report}"
        );
        assert!(
            report.contains("\n-rw-r--r--+ 640 Nov 25 14:42 acl.txt\n"),
            "{report}"
        );
    }

    #[test]
    fn ls_long_huge_directory_folds_middle_with_extension_breakdown() {
        let mut raw = String::from(
            "total 1024\n\
             drwxr-xr-x 4 rishad rishad 4096 Nov 25 14:43 .\n\
             drwxr-xr-x 3 rishad rishad 4096 Nov 25 14:00 ..\n",
        );
        for i in 1..=230 {
            raw.push_str(&format!(
                "-rw-r--r-- 1 rishad rishad {:5} Nov 25 14:43 {i:04}_migration.sql\n",
                500 + i * 17
            ));
        }
        for i in 1..=15 {
            raw.push_str(&format!(
                "-rw-r--r-- 1 rishad rishad {:5} Nov 25 14:43 seed_{i:02}.json\n",
                200 + i * 11
            ));
        }
        raw.push_str("-rwxr-xr-x 1 rishad rishad  1024 Nov 25 14:43 apply.sh\n");
        raw.push_str("drwxr-xr-x 2 rishad rishad  4096 Nov 25 14:43 archive\n");
        raw.push_str(
            "lrwxrwxrwx 1 rishad rishad    18 Nov 25 14:43 current -> 0230_migration.sql\n",
        );

        let report = run_listing(&["ls", "-la"], "ls", &raw, 4096);
        assert!(report.len() <= 4096, "len={}\n{report}", report.len());
        assert!(
            report.len() * 3 < raw.len(),
            "raw={} report={}\n{report}",
            raw.len(),
            report.len()
        );
        assert!(
            report.contains("default: -rw-r--r-- 1 rishad:rishad [stdout:4-251]\n"),
            "{report}"
        );
        assert!(
            report.contains("517 Nov 25 14:43 0001_migration.sql\n"),
            "{report}"
        );
        assert!(
            report.contains("lrwxrwxrwx 18 Nov 25 14:43 current -> 0230_migration.sql\n"),
            "{report}"
        );
        let middle = report
            .lines()
            .find(|l| l.starts_with("… +"))
            .expect("folded middle line");
        assert!(
            middle.contains(".sql") && middle.contains("[stdout:"),
            "{middle}"
        );
    }

    #[test]
    fn ls_long_multi_dir_hoists_when_smaller_and_skips_when_not_smaller() {
        let mut raw = String::from("dir_a:\ntotal 40\n");
        for i in 0..10 {
            raw.push_str(&format!(
                "-rw-r--r-- 1 developer engineering 1200 Nov 25 14:43 a_{i:02}.rs\n"
            ));
        }
        raw.push_str("\ndir_b:\ntotal 40\n");
        for i in 0..10 {
            raw.push_str(&format!(
                "-rw-r--r-- 1 developer engineering 2400 Nov 25 14:43 b_{i:02}.rs\n"
            ));
        }
        let report = run_listing(&["ls", "-l", "dir_a", "dir_b"], "ls", &raw, 4096);
        assert!(
            report.contains("default: -rw-r--r-- 1 developer:engineering\n")
                && report.contains("dir_a/ (10 entries) [stdout:3-12]\n")
                && report.contains("dir_b/ (10 entries) [stdout:16-25]\n")
                && report.len() < raw.len(),
            "raw={} report={}\n{report}",
            raw.len(),
            report.len()
        );

        // Degenerate 3-row input where every row differs in perms/owner/group: hoisted
        // would be larger than raw, so finish falls back to the compact grouped line.
        let tiny = "\
            -rw-r--r-- 1 a b 10 Nov 25 14:43 one\n\
            -rwxr-xr-x 1 c d 20 Nov 25 14:43 two\n\
            drwxr-xr-x 2 e f 30 Nov 25 14:43 three\n";
        let report = run_listing(&["ls", "-l"], "ls", tiny, 4096);
        assert!(
            !report.contains("default: ") && report.contains("./ 3: one, two, three/"),
            "{report}"
        );
    }

    #[test]
    fn single_child_path_chains_collapse_across_find_fd_and_git_ls_files() {
        let java_find = "\
            .\n\
            ./README.md\n\
            ./src\n\
            ./src/main\n\
            ./src/main/java\n\
            ./src/main/java/com\n\
            ./src/main/java/com/acme\n\
            ./src/main/java/com/acme/billing\n\
            ./src/main/java/com/acme/billing/Invoice.java\n\
            ./src/main/java/com/acme/billing/Ledger.java\n\
            ./src/main/java/com/acme/billing/Tax.java\n\
            ./src/test\n\
            ./src/test/java\n\
            ./src/test/java/com\n\
            ./src/test/java/com/acme\n\
            ./src/test/java/com/acme/billing\n\
            ./src/test/java/com/acme/billing/InvoiceTest.java\n\
            ./empty\n\
            ./empty/nested\n\
            ./empty/nested/leaf_dir\n";
        let report = run_listing(&["find", "."], "find", java_find, 4096);
        assert!(
            report.contains(
                "\nsrc/main/java/com/acme/billing/ 3: Invoice.java, Ledger.java, Tax.java [stdout:5-11]\n"
            ),
            "{report}"
        );
        assert!(
            report
                .contains("\nsrc/test/java/com/acme/billing/ 1: InvoiceTest.java [stdout:13-17]\n"),
            "{report}"
        );
        // None of the intermediate single-child pass-through directories waste lines.
        for redundant in [
            "\nsrc/main/ ",
            "\nsrc/main/java/ ",
            "\nsrc/main/java/com/ ",
            "\nsrc/main/java/com/acme/ ",
            "\nsrc/test/ ",
            "\nsrc/test/java/ ",
            "\nempty/ ",
        ] {
            assert!(
                !report.contains(redundant),
                "found {redundant} in:\n{report}"
            );
        }
        // Empty leaf directory at the end of a chain is preserved.
        assert!(
            report.contains("\nempty/nested/ 1: leaf_dir [stdout:19-20]\n"),
            "{report}"
        );

        // `fd` listing of nested Java/TS paths collapses single-child chains too.
        let fd_out = "\
            src\n\
            src/main\n\
            src/main/java\n\
            src/main/java/com\n\
            src/main/java/com/acme\n\
            src/main/java/com/acme/billing\n\
            src/main/java/com/acme/billing/Invoice.java\n\
            src/main/java/com/acme/billing/Ledger.java\n\
            src/main/java/com/acme/billing/Tax.java\n";
        let fd_report = run_listing(&["fd"], "fd", fd_out, 4096);
        assert!(
            fd_report.contains(
                "\nsrc/main/java/com/acme/billing/ 3: Invoice.java, Ledger.java, Tax.java [stdout:1-9]\n"
            ) && !fd_report.contains("\nsrc/main/ "),
            "{fd_report}"
        );

        // `tree` with a single-child package chain collapses to one directory line
        // while keeping the exact tree count line in the header.
        let tree_out = concat!(
            ".\n",
            "└── src\n",
            "    └── main\n",
            "        └── java\n",
            "            └── com\n",
            "                └── acme\n",
            "                    └── billing\n",
            "                        ├── Invoice.java\n",
            "                        └── Ledger.java\n",
            "\n",
            "6 directories, 2 files\n",
        );
        let tree_report = run_listing(&["tree"], "tree", tree_out, 4096);
        assert!(
            tree_report.starts_with("tree: 8 entries in 7 directories · 6 directories, 2 files\n")
                && tree_report.contains(
                    "\nsrc/main/java/com/acme/billing/ 2: Invoice.java, Ledger.java [stdout:2-9]\n"
                )
                && !tree_report.contains("\nsrc/main/ "),
            "{tree_report}"
        );

        // Deep TypeScript monorepo via `git ls-files` folded under a tight budget
        // promotes the shared single-child package prefix instead of hiding it.
        let mut ts = String::from("README.md\n");
        for feat in [
            "billing", "checkout", "orders", "pricing", "shipping", "tax",
        ] {
            for sub in 0..8 {
                for f in 0..4 {
                    ts.push_str(&format!(
                        "packages/{feat}/src/features/{feat}/components/sub_{sub:02}/View{f}.tsx\n"
                    ));
                }
            }
        }
        let report = run_listing(&["git", "ls-files"], "git ls-files", &ts, 1024);
        assert!(report.len() <= 1024, "len={}\n{report}", report.len());
        assert!(
            report.len() * 10 < ts.len(),
            "raw={} report={}\n{report}",
            ts.len(),
            report.len()
        );
        assert!(
            report.contains(
                "packages/billing/src/features/billing/components/ (+32 in 8 subdirectories: 32 .tsx)"
            ),
            "{report}"
        );
    }

    #[test]
    fn dense_directories_report_exact_extension_breakdown_and_span() {
        let mut raw = String::from(
            ".\n\
             ./src\n\
             ./src/main.rs\n\
             ./src/lib.rs\n\
             ./src/api\n",
        );
        for i in 0..10 {
            raw.push_str(&format!("./src/api/f{i}.rs\n"));
        }
        // Dense migrations directory: 48 files (46 .sql + 2 .json), intentionally unsorted.
        raw.push_str("./migrations\n");
        raw.push_str("./migrations/0010_mid.sql\n");
        raw.push_str("./migrations/0001_init.sql\n");
        raw.push_str("./migrations/0002_seed.json\n");
        for i in 3..=46 {
            if i != 10 {
                raw.push_str(&format!("./migrations/{i:04}_step.sql\n"));
            }
        }
        raw.push_str("./migrations/0047_meta.json\n");
        raw.push_str("./migrations/0048_idx.sql\n");
        // Dense fixtures directory with mixed extensions and subdirectories.
        raw.push_str("./fixtures\n");
        raw.push_str("./fixtures/nested_a\n");
        raw.push_str("./fixtures/nested_a/child.txt\n");
        raw.push_str("./fixtures/nested_b\n");
        raw.push_str("./fixtures/nested_b/child.txt\n");
        for i in 0..20 {
            raw.push_str(&format!("./fixtures/case_{i:02}.json\n"));
        }
        for i in 0..10 {
            raw.push_str(&format!("./fixtures/expected_{i:02}.txt\n"));
        }
        raw.push_str("./fixtures/Makefile\n");

        let report = run_listing(&["find", "."], "find", &raw, 4096);
        assert!(
            report.len() * 3 < raw.len(),
            "raw={} report={}\n{report}",
            raw.len(),
            report.len()
        );
        // Small directory keeps all filenames; moderate (10 files) keeps `names +4 more`.
        assert!(
            report.contains("\nsrc/ 3: main.rs, lib.rs, api/ [stdout:"),
            "{report}"
        );
        assert!(
            report.contains("\nsrc/api/ 10: f0.rs, f1.rs, f2.rs, f3.rs, f4.rs, f5.rs +4 more"),
            "{report}"
        );
        // Dense migrations directory reports exact extension breakdown and alphabetical span.
        assert!(
            report.contains(
                "\nmigrations/ (48 files: 46 .sql, 2 .json; first: 0001_init.sql … last: 0048_idx.sql) [stdout:"
            ),
            "{report}"
        );
        // Dense fixtures directory accurately separates subdirs from files and counts no-ext.
        assert!(
            report.contains(
                "\nfixtures/ (31 files, 2 dirs: 20 .json, 10 .txt, 1 (no ext); first: Makefile … last: nested_b/) [stdout:"
            ),
            "{report}"
        );
    }
}
