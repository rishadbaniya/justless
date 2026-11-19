//! Dependency changes in a `Cargo.lock` patch.
//!
//! A lockfile hunk says where lines changed, not which packages did. For each
//! `Cargo.lock` section of a `git diff` or `git show`, both revisions are read by
//! the blob ids on its `index OLD..NEW` line, and the section is described as
//! packages added, removed and updated and dependency links changed. A `source` or
//! `checksum` that changed on a version present in both revisions is flagged on its
//! own line, since that is how a substituted package looks.
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

    /// The flags as lines that name the file.
    pub fn flag_lines(&self) -> String {
        self.flags
            .iter()
            .map(|flag| format!("! {}: {flag}\n", self.path))
            .collect()
    }

    fn holds(&self, item: &Item) -> bool {
        item.name == self.path && (self.first..=self.last).contains(&item.first)
    }
}

/// Whether `words` run `git diff` or `git show`, whose output may hold a patch.
pub fn applies(words: &[&str]) -> bool {
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

fn packages(text: &str) -> Option<BTreeMap<(String, String), Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let lock: Lock = toml_edit::de::from_str(text).ok()?;
    Some(
        lock.package
            .into_iter()
            .map(|p| ((p.name.clone(), p.version.clone()), p))
            .collect(),
    )
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
    let mut changes = Changes::default();
    let mut gone: BTreeMap<&str, Vec<&Package>> = BTreeMap::new();
    let mut came: BTreeMap<&str, Vec<&Package>> = BTreeMap::new();
    for (key, package) in &old {
        match new.get(key) {
            Some(now) => {
                for (what, before, after) in [
                    ("source", &package.source, &now.source),
                    ("checksum", &package.checksum, &now.checksum),
                ] {
                    if before != after {
                        changes.flags.push(format!(
                            "{what} changed on existing version {} {}: {} -> {}",
                            package.name,
                            package.version,
                            before.as_deref().unwrap_or("none"),
                            after.as_deref().unwrap_or("none")
                        ));
                    }
                }
                changes.link(package, now, &package.name);
            }
            None => gone.entry(&package.name).or_default().push(package),
        }
    }
    for (key, package) in &new {
        if !old.contains_key(key) {
            came.entry(&package.name).or_default().push(package);
        }
    }
    for (name, before) in &gone {
        match (before.as_slice(), came.get(name).map(Vec::as_slice)) {
            ([before], Some([after])) => {
                changes
                    .updated
                    .push(format!("{name} {} -> {}", before.version, after.version));
                changes.link(before, after, name);
                came.remove(name);
            }
            _ => changes
                .removed
                .extend(before.iter().map(|p| format!("{} {}", p.name, p.version))),
        }
    }
    changes.added = came
        .values()
        .flatten()
        .map(|p| format!("{} {}", p.name, p.version))
        .collect();
    Some(changes)
}

impl Changes {
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

/// A patch summary whose `Cargo.lock` hunks are replaced by their description when
/// that is shorter, with every flag right after the first line.
pub fn summarize(summary: &Summary, sections: &[Section]) -> String {
    let mut out = format!("{} diff section(s)\n", summary.items.len());
    for section in sections {
        out.push_str(&section.flag_lines());
    }
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
            summarize(&summary, &[section("Cargo.lock dependencies: 1 added\n")]),
            "3 diff section(s)\n! Cargo.lock: source changed on existing version x 1.0.0: a -> b\n\
             Cargo.lock dependencies: 1 added\n  added: y 1.0.0\n\
             src/lib.rs @@ -50,7 +50,8 @@ [[package]] [stdout:50-50]\n"
        );
        // A description longer than the hunk lines it replaces leaves them.
        let long = format!("Cargo.lock dependencies: {}\n", "x".repeat(200));
        assert!(summarize(&summary, &[section(&long)]).contains("Cargo.lock @@ -30,7"));
    }

    #[test]
    fn only_git_diff_and_show_apply() {
        for (words, applies_to) in [
            (&["git", "diff", "HEAD~1"][..], true),
            (&["git", "--no-pager", "show"][..], true),
            (&["git", "-c", "color.ui=never", "diff"][..], true),
            (&["git", "-C", "elsewhere", "diff"][..], false),
            (&["git", "log", "-p"][..], false),
            (&["cargo", "diff"][..], false),
        ] {
            assert_eq!(applies(words), applies_to, "{words:?}");
        }
    }
}
