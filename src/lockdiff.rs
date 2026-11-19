//! Dependency changes in a `Cargo.lock` patch.
//!
//! A lockfile hunk says where lines changed, not which packages did. For each
//! `Cargo.lock` section of a `git diff` or `git show`, both revisions are read by
//! the blob ids on its `index OLD..NEW` line, and the section is described as
//! packages added, removed and updated and dependency links changed. A `source` or
//! `checksum` that changed on a version present in both revisions, or a source added
//! to or removed from such a version, is flagged on its own line, since that is how a
//! substituted package looks.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::summary::{Item, Summary};

/// The largest patch this module reads.
pub const LIMIT: u64 = 8 * 1024 * 1024;

/// One `Cargo.lock` section of a patch, described by its dependency changes.
pub struct Section {
    /// The file as the patch names it, such as `Cargo.lock` or `tool/Cargo.lock`.
    pub path: String,
    /// The section's first and last stdout lines, from `diff --git` on.
    pub first: usize,
    pub last: usize,
    /// The line that names the file, what changed and its lines.
    head: String,
    /// A source or checksum that changed on an existing version, each a line.
    pub flags: Vec<String>,
    /// Indented lists of the packages and links that changed.
    body: String,
}

impl Section {
    /// The description, flags first so they stay when the rest is clipped.
    pub fn text(&self) -> String {
        let mut text = self.head.clone();
        for flag in &self.flags {
            text.push_str(&format!("  ! {flag}\n"));
        }
        text.push_str(&self.body);
        text
    }

    fn holds(&self, item: &Item) -> bool {
        item.name == self.path && (self.first..=self.last).contains(&item.first)
    }
}

/// Whether `words` run `git diff` or `git show`, whose output may hold a patch, and
/// do not print a file's content, as `git show REV:PATH` does.
pub fn applies(words: &[&str]) -> bool {
    if crate::floor::prints_file(words) {
        return false;
    }
    // Options such as `-C DIR` or `--git-dir` read another repository, where these
    // blob ids would not resolve, and wrappers drop them.
    if words.iter().any(|word| {
        word.starts_with("-C") || word.starts_with("--git-dir") || word.starts_with("--work-tree")
    }) {
        return false;
    }
    let owned = crate::wrappers::effective(words);
    let words: Vec<&str> = match &owned {
        Some(words) => words.iter().map(String::as_str).collect(),
        None => words.to_vec(),
    };
    let Some((program, mut rest)) = words.split_first() else {
        return false;
    };
    if Path::new(program).file_name().and_then(|n| n.to_str()) != Some("git") {
        return false;
    }
    while let Some((first, tail)) = rest.split_first() {
        match *first {
            "--no-pager" | "-p" | "--paginate" | "--no-optional-locks" => rest = tail,
            "-c" if !tail.is_empty() => rest = &tail[1..],
            _ => break,
        }
    }
    matches!(rest.first(), Some(&("diff" | "show")))
}

/// Describe each `Cargo.lock` section of `patch` whose revisions Git in `cwd` can
/// read. Line numbers start at 1, as in `justless log`.
pub fn sections(patch: &str, cwd: &Path) -> Vec<Section> {
    if !patch.contains("Cargo.lock") {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut current: Option<(String, usize, usize, Option<String>, bool)> = None;
    let finish = |section: Option<(String, usize, usize, Option<String>, bool)>,
                  found: &mut Vec<Section>| {
        if let Some((path, first, last, Some(index), _)) = section
            && let Some(section) = describe(&path, first, last, &index, cwd)
        {
            found.push(section);
        }
    };
    for (index, line) in patch.lines().enumerate() {
        let number = index + 1;
        if let Some(names) = line.strip_prefix("diff --git ") {
            finish(current.take(), &mut found);
            current = names
                .rsplit_once(" b/")
                .map(|(_, path)| path)
                .filter(|path| {
                    !path.starts_with('"')
                        && Path::new(path).file_name().and_then(|n| n.to_str())
                            == Some("Cargo.lock")
                })
                .map(|path| (path.to_owned(), number, number, None, false));
            continue;
        }
        let Some((_, _, last, blobs, hunks)) = current.as_mut() else {
            continue;
        };
        let part = if *hunks {
            line.starts_with([' ', '+', '-', '\\']) || line.starts_with("@@ ")
        } else if line.starts_with("@@ ") {
            *hunks = true;
            true
        } else {
            if let Some(ids) = line.strip_prefix("index ") {
                *blobs = Some(ids.split(' ').next().unwrap_or_default().to_owned());
            }
            !line.is_empty() && !line.starts_with("commit ")
        };
        if part {
            *last = number;
        } else {
            finish(current.take(), &mut found);
        }
    }
    finish(current, &mut found);
    found
}

fn describe(path: &str, first: usize, last: usize, ids: &str, cwd: &Path) -> Option<Section> {
    let (old, new) = ids.split_once("..")?;
    let old = revision(old, None, cwd)?;
    let new = revision(new, Some(path), cwd)?;
    let changes = compare(&old, &new)?;
    let (head, body) = changes.render(path, first, last);
    Some(Section {
        path: path.to_owned(),
        first,
        last,
        head,
        flags: changes.flags,
        body,
    })
}

/// A blob's text, empty for an absent side. A working-tree side has no blob yet, so
/// the file is read when its hash matches the patch.
fn revision(id: &str, path: Option<&str>, cwd: &Path) -> Option<String> {
    if !id.is_empty() && id.bytes().all(|b| b == b'0') {
        return Some(String::new());
    }
    if !id.bytes().all(|b| b.is_ascii_hexdigit()) || id.len() < 4 {
        return None;
    }
    if let Some(text) = git(cwd, &["cat-file", "blob", id]) {
        return Some(text);
    }
    let top = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let file = Path::new(top.trim_end()).join(path?);
    let hash = git(cwd, &["hash-object", "--", file.to_str()?])?;
    hash.starts_with(id)
        .then(|| std::fs::read_to_string(&file).ok())
        .flatten()
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        // A partial clone would otherwise fetch a missing blob from its remote.
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
}

#[derive(Deserialize)]
struct Lock {
    #[serde(default)]
    package: Vec<Package>,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Default)]
struct Changes {
    added: Vec<String>,
    removed: Vec<String>,
    updated: Vec<String>,
    links: Vec<String>,
    flags: Vec<String>,
}

/// A package's name, version and source: one version can be locked from several
/// sources, such as the registry and a repository.
type Key = (String, String, Option<String>);

/// The packages of a lockfile by key, or `None` when one repeats, which Cargo does
/// not write.
fn packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let lock: Lock = toml_edit::de::from_str(text).ok()?;
    let mut packages = BTreeMap::new();
    for package in lock.package {
        let key = (
            package.name.clone(),
            package.version.clone(),
            package.source.clone(),
        );
        if packages.insert(key, package).is_some() {
            return None;
        }
    }
    Some(packages)
}

/// How many sources each name and version is locked from.
fn versions(packages: &BTreeMap<Key, Package>) -> BTreeMap<(&str, &str), usize> {
    let mut versions = BTreeMap::new();
    for (name, version, _) in packages.keys() {
        *versions
            .entry((name.as_str(), version.as_str()))
            .or_default() += 1;
    }
    versions
}

/// The names a package depends on. An entry such as `foo 1.0.0 (registry+…)`
/// carries a version only when several versions of `foo` are locked.
fn links(package: &Package) -> BTreeSet<&str> {
    package
        .dependencies
        .iter()
        .map(|entry| entry.split(' ').next().unwrap_or(entry))
        .collect()
}

fn compare(old: &str, new: &str) -> Option<Changes> {
    let old = packages(old)?;
    let new = packages(new)?;
    let (before, after) = (versions(&old), versions(&new));
    let mut changes = Changes::default();
    // Packages whose name, version and source the other revision does not lock, by
    // name and version.
    let mut left: BTreeMap<(&str, &str), Vec<&Package>> = BTreeMap::new();
    let mut right: BTreeMap<(&str, &str), Vec<&Package>> = BTreeMap::new();
    for (key, package) in &old {
        match new.get(key) {
            Some(now) => changes.kept(package, now),
            None => left
                .entry((&package.name, &package.version))
                .or_default()
                .push(package),
        }
    }
    for (key, package) in &new {
        if !old.contains_key(key) {
            right
                .entry((&package.name, &package.version))
                .or_default()
                .push(package);
        }
    }
    let mut gone: BTreeMap<&str, Vec<&Package>> = BTreeMap::new();
    let mut came: BTreeMap<&str, Vec<&Package>> = BTreeMap::new();
    for (version, was) in left {
        let now = right.remove(&version).unwrap_or_default();
        match (was.as_slice(), now.as_slice()) {
            // A version locked from one source in each revision, and they differ.
            ([was], [now]) => changes.kept(was, now),
            // A version still locked, from other sources than before.
            _ if after.contains_key(&version) => {
                changes.sources("removed from", &was);
                changes.sources("added to", &now);
            }
            _ => gone.entry(version.0).or_default().extend(was),
        }
    }
    for (version, now) in right {
        if before.contains_key(&version) {
            changes.sources("added to", &now);
        } else {
            came.entry(version.0).or_default().extend(now);
        }
    }
    for (name, was) in &gone {
        match (was.as_slice(), came.get(name).map(Vec::as_slice)) {
            ([was], Some([now])) => {
                changes
                    .updated
                    .push(format!("{name} {} -> {}", was.version, now.version));
                changes.link(was, now, name);
                came.remove(name);
            }
            _ => changes
                .removed
                .extend(was.iter().map(|p| entry(&before, p))),
        }
    }
    changes.added = came.values().flatten().map(|p| entry(&after, p)).collect();
    Some(changes)
}

/// A package as the lists name it, with its source when its version is locked
/// from several.
fn entry(versions: &BTreeMap<(&str, &str), usize>, package: &Package) -> String {
    let several = versions
        .get(&(package.name.as_str(), package.version.as_str()))
        .is_some_and(|&count| count > 1);
    match &package.source {
        Some(source) if several => format!("{} {} ({source})", package.name, package.version),
        _ => format!("{} {}", package.name, package.version),
    }
}

impl Changes {
    /// A version present in both revisions: flag a source or checksum that changed,
    /// and note its dependency changes.
    fn kept(&mut self, was: &Package, now: &Package) {
        for (what, before, after) in [
            ("source", &was.source, &now.source),
            ("checksum", &was.checksum, &now.checksum),
        ] {
            if before != after {
                self.flags.push(format!(
                    "{what} changed on existing version {} {}: {} -> {}",
                    was.name,
                    was.version,
                    before.as_deref().unwrap_or("none"),
                    after.as_deref().unwrap_or("none")
                ));
            }
        }
        self.link(was, now, &was.name);
    }

    /// Flag each source `how` (`added to` or `removed from`) a version that both
    /// revisions lock.
    fn sources(&mut self, how: &str, packages: &[&Package]) {
        for package in packages {
            self.flags.push(format!(
                "source {how} existing version {} {}: {}",
                package.name,
                package.version,
                package.source.as_deref().unwrap_or("none")
            ));
        }
    }

    fn link(&mut self, before: &Package, after: &Package, name: &str) {
        let (before, after) = (links(before), links(after));
        let mut change: Vec<String> = after
            .difference(&before)
            .map(|dep| format!("+{dep}"))
            .collect();
        change.extend(before.difference(&after).map(|dep| format!("-{dep}")));
        if !change.is_empty() {
            self.links.push(format!("{name} {}", change.join(" ")));
        }
    }

    /// The heading line and the lists that follow the flags.
    fn render(&self, path: &str, first: usize, last: usize) -> (String, String) {
        let mut counts = Vec::new();
        for (count, what) in [
            (self.added.len(), "added"),
            (self.removed.len(), "removed"),
            (self.updated.len(), "updated"),
            (self.links.len(), "with changed dependencies"),
        ] {
            if count > 0 {
                counts.push(format!("{count} {what}"));
            }
        }
        if !self.flags.is_empty() {
            counts.push(format!("{} source or checksum change(s)", self.flags.len()));
        }
        let summary = if counts.is_empty() {
            "no package changes".to_owned()
        } else {
            counts.join(", ")
        };
        let head = format!("{path} dependencies: {summary} [stdout:{first}-{last}]\n");
        let mut body = String::new();
        for (label, list, separator) in [
            ("added", &self.added, ", "),
            ("removed", &self.removed, ", "),
            ("updated", &self.updated, ", "),
            ("dependencies", &self.links, "; "),
        ] {
            if !list.is_empty() {
                body.push_str(&format!("  {label}: {}\n", list.join(separator)));
            }
        }
        (head, body)
    }
}

/// `patch` with each section replaced by its description, keeping its `diff --git`
/// line.
pub fn view(patch: &str, sections: &[Section]) -> String {
    let mut out = String::with_capacity(patch.len());
    let mut lines = patch.split_inclusive('\n').enumerate().peekable();
    while let Some((index, line)) = lines.next() {
        let number = index + 1;
        if let Some(section) = sections.iter().find(|s| s.first == number) {
            out.push_str(line);
            out.push_str(&section.text());
            while lines.peek().is_some_and(|(i, _)| *i < section.last) {
                lines.next();
            }
        } else {
            out.push_str(line);
        }
    }
    out
}

/// The flags of `sections` as lines that name the file, within about `budget` bytes:
/// at least the first, then a count of the rest.
pub fn flag_lines(sections: &[Section], budget: usize) -> String {
    let lines: Vec<String> = sections
        .iter()
        .flat_map(|s| s.flags.iter().map(|flag| format!("! {}: {flag}\n", s.path)))
        .collect();
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 && out.len() + line.len() > budget {
            let rest = lines.len() - index;
            out.push_str(&format!("! +{rest} more source or checksum change(s)\n"));
            break;
        }
        out.push_str(line);
    }
    out
}

/// A patch summary whose `Cargo.lock` hunks are replaced by their description when
/// that is shorter, with the flags, within `budget` bytes, right after the first line.
pub fn summarize(summary: &Summary, sections: &[Section], budget: usize) -> String {
    let mut out = format!("{}\n", summary.patch_heading());
    out.push_str(&flag_lines(sections, budget));
    let described: Vec<bool> = sections
        .iter()
        .map(|section| {
            let hunks: usize = summary
                .items
                .iter()
                .filter(|item| section.holds(item))
                .map(|item| item.display().len() + 1)
                .sum();
            section.head.len() + section.body.len() < hunks
        })
        .collect();
    let mut done = vec![false; sections.len()];
    for item in &summary.items {
        match sections.iter().position(|section| section.holds(item)) {
            Some(index) if described[index] => {
                if !done[index] {
                    done[index] = true;
                    out.push_str(&sections[index].head);
                    out.push_str(&sections[index].body);
                }
            }
            _ => {
                out.push_str(&item.display());
                out.push('\n');
            }
        }
    }
    for line in &summary.outcomes {
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock(packages: &[(&str, &str, &str, &[&str])]) -> String {
        let mut text = String::from("version = 4\n");
        for (name, version, checksum, deps) in packages {
            text.push_str(&format!(
                "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{checksum}\"\n"
            ));
            if !deps.is_empty() {
                let deps: Vec<String> = deps.iter().map(|d| format!(" \"{d}\",\n")).collect();
                text.push_str(&format!("dependencies = [\n{}]\n", deps.concat()));
            }
        }
        text
    }

    #[test]
    fn changes_name_packages_links_and_flag_substitutions() {
        let old = lock(&[
            ("tokio", "1.40.0", "aa", &["mio"]),
            ("mio", "1.0.0", "bb", &[]),
            ("serde", "1.0.1", "cc", &[]),
            ("gone", "0.1.0", "dd", &[]),
        ]);
        let new = lock(&[
            ("tokio", "1.41.0", "ee", &["mio", "signal-hook-registry"]),
            ("mio", "1.0.0", "ff", &[]),
            ("serde", "1.0.1", "cc", &[]),
            ("signal-hook-registry", "1.4.2", "11", &[]),
        ]);
        let changes = compare(&old, &new).unwrap();
        let (head, body) = changes.render("Cargo.lock", 3, 40);
        let section = Section {
            path: "Cargo.lock".into(),
            first: 3,
            last: 40,
            head,
            flags: changes.flags,
            body,
        };
        assert_eq!(
            section.text(),
            "Cargo.lock dependencies: 1 added, 1 removed, 1 updated, 1 with changed dependencies, 1 source or checksum change(s) [stdout:3-40]\n  \
             ! checksum changed on existing version mio 1.0.0: bb -> ff\n  \
             added: signal-hook-registry 1.4.2\n  removed: gone 0.1.0\n  \
             updated: tokio 1.40.0 -> 1.41.0\n  dependencies: tokio +signal-hook-registry\n"
        );
        // A new lockfile lists every package as added; a reformat changes none.
        let (head, _) = compare("", &new).unwrap().render("Cargo.lock", 1, 9);
        assert!(head.contains("4 added"), "{head}");
        let (head, _) = compare(&new, &new).unwrap().render("Cargo.lock", 1, 9);
        assert!(head.contains("no package changes"), "{head}");
        assert!(compare("not toml [", &new).is_none());
    }

    #[test]
    fn hunk_summaries_describe_lock_sections_when_shorter() {
        let item =
            |name: &str, n: usize| Item::new(name, &format!("@@ -{n},7 +{n},8 @@ [[package]]"), n);
        let summary = Summary {
            kind: "diff".into(),
            items: vec![
                item("Cargo.lock", 5),
                item("Cargo.lock", 30),
                item("src/lib.rs", 50),
            ],
            outcomes: Vec::new(),
            ..Summary::default()
        };
        let section = |head: &str| Section {
            path: "Cargo.lock".into(),
            first: 1,
            last: 40,
            head: head.into(),
            flags: vec!["source changed on existing version x 1.0.0: a -> b".into()],
            body: "  added: y 1.0.0\n".into(),
        };
        assert_eq!(
            summarize(
                &summary,
                &[section("Cargo.lock dependencies: 1 added\n")],
                4096
            ),
            "3 diff section(s)\n! Cargo.lock: source changed on existing version x 1.0.0: a -> b\n\
             Cargo.lock dependencies: 1 added\n  added: y 1.0.0\n\
             src/lib.rs @@ -50,7 +50,8 @@ [[package]] [stdout:50-50]\n"
        );
        // A description longer than the hunk lines it replaces leaves them.
        let long = format!("Cargo.lock dependencies: {}\n", "x".repeat(200));
        assert!(summarize(&summary, &[section(&long)], 4096).contains("Cargo.lock @@ -30,7"));
    }

    #[test]
    fn only_git_diff_and_show_apply() {
        for (words, applies_to) in [
            (&["git", "diff", "HEAD~1"][..], true),
            (&["git", "--no-pager", "show"][..], true),
            (&["git", "-c", "color.ui=never", "diff"][..], true),
            (&["git", "-C", "elsewhere", "diff"][..], false),
            (&["git", "log", "-p"][..], false),
            (&["git", "show", "HEAD:Cargo.lock"][..], false),
            (&["cargo", "diff"][..], false),
        ] {
            assert_eq!(applies(words), applies_to, "{words:?}");
        }
    }

    /// A lockfile of `(name, version, source)` packages.
    fn sourced(packages: &[(&str, &str, &str)]) -> String {
        let mut text = String::from("version = 4\n");
        for (name, version, source) in packages {
            text.push_str(&format!(
                "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = \"{source}\"\n"
            ));
        }
        text
    }

    #[test]
    fn each_source_of_a_version_is_compared() {
        let (reg, a, b) = ("registry+r", "git+a", "git+b");
        let flags = |old: &[(&str, &str, &str)], new: &[(&str, &str, &str)]| {
            compare(&sourced(old), &sourced(new)).unwrap().flags
        };
        // One of two sources of a version swapped for another, one added and one
        // removed.
        assert_eq!(
            flags(
                &[("foo", "1.0.0", a), ("foo", "1.0.0", reg)],
                &[("foo", "1.0.0", b), ("foo", "1.0.0", reg)]
            ),
            ["source changed on existing version foo 1.0.0: git+a -> git+b"]
        );
        assert_eq!(
            flags(
                &[("foo", "1.0.0", reg)],
                &[("foo", "1.0.0", b), ("foo", "1.0.0", reg)]
            ),
            ["source added to existing version foo 1.0.0: git+b"]
        );
        assert_eq!(
            flags(
                &[("foo", "1.0.0", a), ("foo", "1.0.0", reg)],
                &[("foo", "1.0.0", reg)]
            ),
            ["source removed from existing version foo 1.0.0: git+a"]
        );
        // A new version locked from two sources names both.
        let changes = compare(
            &sourced(&[("foo", "1.0.0", reg)]),
            &sourced(&[("foo", "2.0.0", a), ("foo", "2.0.0", reg)]),
        )
        .unwrap();
        assert_eq!(
            (changes.added, changes.removed, changes.flags.len()),
            (
                vec![
                    "foo 2.0.0 (git+a)".to_owned(),
                    "foo 2.0.0 (registry+r)".to_owned()
                ],
                vec!["foo 1.0.0".to_owned()],
                0
            )
        );
        // A package locked twice from one source is not read.
        let twice = sourced(&[("foo", "1.0.0", reg), ("foo", "1.0.0", reg)]);
        assert!(compare(&twice, &twice).is_none());
    }

    #[test]
    fn flag_lines_keep_the_first_and_count_the_rest_past_their_budget() {
        let section = |path: &str, flags: &[&str]| Section {
            path: path.into(),
            first: 1,
            last: 9,
            head: String::new(),
            flags: flags.iter().map(|f| f.to_string()).collect(),
            body: String::new(),
        };
        let sections = [
            section(
                "Cargo.lock",
                &["checksum changed on existing version a 1.0.0: x -> y"],
            ),
            section("tool/Cargo.lock", &["one", "two"]),
        ];
        assert_eq!(
            flag_lines(&sections, 10),
            "! Cargo.lock: checksum changed on existing version a 1.0.0: x -> y\n\
             ! +2 more source or checksum change(s)\n"
        );
        assert_eq!(
            flag_lines(&sections, 4096),
            "! Cargo.lock: checksum changed on existing version a 1.0.0: x -> y\n\
             ! tool/Cargo.lock: one\n! tool/Cargo.lock: two\n"
        );
    }
}
