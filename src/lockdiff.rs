//! Dependency changes in lockfile patches (`Cargo.lock`, `package-lock.json`,
//! `pnpm-lock.yaml`, `yarn.lock`, `bun.lock`, `uv.lock`, `go.sum`, `poetry.lock`,
//! `Gemfile.lock`, `composer.lock`, `flake.lock`, `Package.resolved`, `mix.lock`,
//! `pubspec.lock`, `gradle.lockfile`, `Podfile.lock`, `packages.lock.json`, and
//! `Pipfile.lock`) plus generated/snapshot file folding.
//!
//! A lockfile hunk says where lines changed, not which packages did. For each
//! supported lockfile section of a `git diff`, `git show`, `git log -p`,
//! `gh pr diff`, or `glab mr diff`, both revisions are read by the blob ids on
//! its `index OLD..NEW` line when present locally, falling back to hunk-based
//! parsing when remote blobs are absent. The section is described as packages
//! added, removed and updated and dependency links changed. A `source` or
//! `checksum` that changed on a version present in both revisions, or a source
//! added to or removed from such a version, is flagged on its own line, since
//! that is how a substituted package looks.
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::summary::Summary;

/// The largest patch this module reads.
pub const LIMIT: u64 = 8 * 1024 * 1024;

/// Minimum patch section length (in lines) before a generated/snapshot file is folded.
pub(crate) const GENERATED_FOLD_LINES: usize = 25;

/// Minimum changed (`+` plus `-`) lines before a `.svg` file is folded as a
/// large generated/exported graphic; small hand-edited SVGs stay inline.
pub(crate) const SVG_FOLD_CHANGED_LINES: usize = 100;

/// One lockfile or folded generated section of a patch.
pub struct Section {
    /// The file as the patch names it, such as `Cargo.lock` or `tool/Cargo.lock`.
    pub path: String,
    /// The section's first and last stdout lines, from `diff --git` / `diff --cc` on.
    pub first: usize,
    pub last: usize,
    /// The line that names the file, what changed and its lines.
    head: String,
    /// A source or checksum that changed on an existing version, each a line.
    pub flags: Vec<String>,
    /// Indented lists of the packages and links that changed.
    body: String,
    /// True for folded generated/snapshot files whose `head` replaces the section header.
    generated: bool,
}

impl Section {
    /// The description, flags first so they stay when the rest is clipped.
    pub fn text(&self) -> String {
        let flags_len: usize = self.flags.iter().map(|f| f.len() + 5).sum();
        let mut text = String::with_capacity(self.head.len() + flags_len + self.body.len());
        text.push_str(&self.head);
        for flag in &self.flags {
            text.push_str("  ! ");
            text.push_str(flag);
            text.push('\n');
        }
        text.push_str(&self.body);
        text
    }
}

/// Whether `words` run `git diff`, `git show`, `git log -p`, `gh pr diff` or
/// `glab mr diff`, whose output may hold a patch, and do not print a file's
/// content, as `git show REV:PATH` does.
pub fn applies(words: &[&str]) -> bool {
    if crate::floor::prints_file(words) {
        return false;
    }
    let raw_words = words;
    let owned = crate::wrappers::effective(words);
    let words: Vec<&str> = match &owned {
        Some(words) => words.iter().map(String::as_str).collect(),
        None => words.to_vec(),
    };
    let Some((program, mut rest)) = words.split_first() else {
        return false;
    };
    let Some(name) = Path::new(program).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    match name {
        "gh" => matches!(rest, ["pr", "diff", ..]),
        "glab" => matches!(rest, ["mr", "diff", ..]),
        "git" => {
            // Options such as `-C DIR` or `--git-dir` read another repository, where
            // these blob ids would not resolve, and wrappers drop them.
            if raw_words.iter().any(|word| {
                word.starts_with("-C")
                    || word.starts_with("--git-dir")
                    || word.starts_with("--work-tree")
            }) {
                return false;
            }
            while let Some((first, tail)) = rest.split_first() {
                let stripped = crate::filters::git_display_subcommand(rest);
                if stripped.len() != rest.len() {
                    rest = stripped;
                    continue;
                }
                match *first {
                    "-p" | "--paginate" => rest = tail,
                    "-c" if !tail.is_empty() => rest = &tail[1..],
                    _ => break,
                }
            }
            match rest.split_first() {
                Some((&("diff" | "show"), _)) => true,
                Some((&"log", args)) => args.iter().take_while(|w| **w != "--").any(|w| {
                    matches!(*w, "-p" | "-u" | "--patch")
                        || w.starts_with("-U")
                        || w.starts_with("--unified=")
                }),
                _ => false,
            }
        }
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockFormat {
    Cargo,
    Npm,
    Yarn,
    Pnpm,
    Bun,
    Uv,
    GoSum,
    Poetry,
    Gemfile,
    Composer,
    Flake,
    SwiftResolved,
    Mix,
    Pubspec,
    Gradle,
    Podfile,
    NuGet,
    Pipfile,
}

impl LockFormat {
    fn of_path(path: &str) -> Option<Self> {
        match Path::new(path).file_name().and_then(|n| n.to_str())? {
            "Cargo.lock" => Some(Self::Cargo),
            "package-lock.json" => Some(Self::Npm),
            "yarn.lock" => Some(Self::Yarn),
            "pnpm-lock.yaml" => Some(Self::Pnpm),
            "bun.lock" => Some(Self::Bun),
            "uv.lock" => Some(Self::Uv),
            "go.sum" => Some(Self::GoSum),
            "poetry.lock" => Some(Self::Poetry),
            "Gemfile.lock" => Some(Self::Gemfile),
            "composer.lock" => Some(Self::Composer),
            "flake.lock" => Some(Self::Flake),
            "Package.resolved" => Some(Self::SwiftResolved),
            "mix.lock" => Some(Self::Mix),
            "pubspec.lock" => Some(Self::Pubspec),
            "Podfile.lock" => Some(Self::Podfile),
            "packages.lock.json" => Some(Self::NuGet),
            "Pipfile.lock" => Some(Self::Pipfile),
            name if name == "gradle.lockfile" || name.ends_with("-gradle.lockfile") => {
                Some(Self::Gradle)
            }
            _ => None,
        }
    }
}

/// Whether `path` names a lockfile with a dedicated dependency-diff parser.
pub(crate) fn is_lockfile_path(path: &str) -> bool {
    LockFormat::of_path(path).is_some()
}

/// Whether `path` is a generated artifact or snapshot whose large diff sections
/// fold to a single summary line.
pub(crate) fn is_generated_path(path: &str) -> bool {
    let Some(name) = Path::new(path).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name == ".terraform.lock.hcl"
        || name.ends_with(".snap")
        || name.ends_with(".snap.new")
        || name.ends_with(".min.js")
        || name.ends_with(".min.css")
        || name.ends_with(".pb.go")
        || name.ends_with(".pb.rs")
        || name.ends_with(".pb.ts")
        || name.ends_with(".pb.js")
        || name.ends_with("_pb2.py")
        || name.ends_with(".g.dart")
        || name.ends_with(".freezed.dart")
        || name.ends_with(".Designer.cs")
        || name.ends_with(".g.cs")
        || name.ends_with(".g.i.cs")
        || name.contains(".generated.")
}

/// Whether `path` names an SVG file, which only folds when its diff changes
/// more than [`SVG_FOLD_CHANGED_LINES`] lines.
pub(crate) fn is_svg_path(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.ends_with(".svg") || name.ends_with(".SVG"))
}

fn diff_section_path(line: &str) -> Option<&str> {
    if !line.starts_with("diff --") {
        return None;
    }
    if let Some(names) = line.strip_prefix("diff --git ") {
        names
            .rsplit_once(" b/")
            .map(|(_, path)| path)
            .filter(|path| !path.starts_with('"'))
    } else if let Some(path) = line
        .strip_prefix("diff --cc ")
        .or_else(|| line.strip_prefix("diff --combined "))
    {
        let path = path.trim();
        (!path.is_empty() && !path.starts_with('"')).then_some(path)
    } else {
        None
    }
}

fn combined_parents(line: &str) -> Option<usize> {
    if !line.starts_with("@@@") {
        return None;
    }
    let at = line.bytes().take_while(|&b| b == b'@').count();
    let rest = line.get(at..)?;
    if !rest.starts_with(" -") {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut i = 2;
    while i + at < bytes.len() {
        if bytes[i] == b' ' && bytes[i + 1..i + 1 + at].iter().all(|&b| b == b'@') {
            return Some(at - 1);
        }
        i += 1;
    }
    None
}

fn is_hunk_line(line: &str) -> bool {
    line.starts_with("@@ ") || combined_parents(line).is_some()
}

enum ActiveKind<'a> {
    Lock {
        format: LockFormat,
        blobs: Option<String>,
        new_file: bool,
        deleted_file: bool,
        hunk_lines: Vec<&'a str>,
    },
    Generated {
        added: usize,
        deleted: usize,
        parents: usize,
        svg: bool,
    },
}

struct ActiveSection<'a> {
    path: String,
    first: usize,
    last: usize,
    hunks: bool,
    kind: ActiveKind<'a>,
}

/// Describe each supported lockfile section of `patch` (from local git blobs or
/// remote hunks) and fold large generated/snapshot file sections. Line numbers
/// start at 1, as in `justless log`.
pub fn sections(patch: &str, cwd: &Path) -> Vec<Section> {
    let clean: Cow<'_, str> = if patch.contains('\x1b') {
        crate::capture::ANSI.replace_all(patch, "")
    } else {
        Cow::Borrowed(patch)
    };
    let patch = clean.as_ref();
    if !patch.contains("diff --") {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut current: Option<ActiveSection<'_>> = None;
    let finish = |section: Option<ActiveSection<'_>>, found: &mut Vec<Section>| {
        let Some(section) = section else {
            return;
        };
        match section.kind {
            ActiveKind::Lock {
                format,
                blobs,
                new_file,
                deleted_file,
                hunk_lines,
            } => {
                let described = blobs
                    .as_deref()
                    .and_then(|ids| {
                        describe(&section.path, format, section.first, section.last, ids, cwd)
                    })
                    .or_else(|| {
                        describe_from_hunks(
                            &section.path,
                            format,
                            section.first,
                            section.last,
                            new_file,
                            deleted_file,
                            &hunk_lines,
                        )
                    });
                if let Some(sec) = described {
                    found.push(sec);
                }
            }
            ActiveKind::Generated {
                added,
                deleted,
                svg,
                ..
            } => {
                let should_fold = if svg {
                    added + deleted > SVG_FOLD_CHANGED_LINES
                } else {
                    section.last.saturating_sub(section.first) + 1 > GENERATED_FOLD_LINES
                };
                if should_fold {
                    let head = format!(
                        "=== {} (generated, +{added} -{deleted}) [stdout:{}-{}]\n",
                        section.path, section.first, section.last
                    );
                    found.push(Section {
                        path: section.path,
                        first: section.first,
                        last: section.last,
                        head,
                        flags: Vec::new(),
                        body: String::new(),
                        generated: true,
                    });
                }
            }
        }
    };
    for (index, line) in patch.lines().enumerate() {
        let number = index + 1;
        if let Some(path) = diff_section_path(line) {
            finish(current.take(), &mut found);
            current = if let Some(format) = LockFormat::of_path(path) {
                Some(ActiveSection {
                    path: path.to_owned(),
                    first: number,
                    last: number,
                    hunks: false,
                    kind: ActiveKind::Lock {
                        format,
                        blobs: None,
                        new_file: false,
                        deleted_file: false,
                        hunk_lines: Vec::new(),
                    },
                })
            } else if is_generated_path(path) || is_svg_path(path) {
                Some(ActiveSection {
                    path: path.to_owned(),
                    first: number,
                    last: number,
                    hunks: false,
                    kind: ActiveKind::Generated {
                        added: 0,
                        deleted: 0,
                        parents: 0,
                        svg: !is_generated_path(path),
                    },
                })
            } else {
                None
            };
            continue;
        }
        let Some(active) = current.as_mut() else {
            continue;
        };
        let part = if active.hunks {
            line.starts_with([' ', '+', '-', '\\']) || is_hunk_line(line)
        } else if is_hunk_line(line) {
            active.hunks = true;
            true
        } else {
            if let ActiveKind::Lock {
                blobs,
                new_file,
                deleted_file,
                ..
            } = &mut active.kind
            {
                if let Some(ids) = line.strip_prefix("index ") {
                    *blobs = Some(ids.split(' ').next().unwrap_or_default().to_owned());
                } else if line == "--- /dev/null" {
                    *new_file = true;
                } else if line == "+++ /dev/null" {
                    *deleted_file = true;
                }
            }
            !line.is_empty() && !line.starts_with("commit ")
        };
        if part {
            active.last = number;
            if active.hunks {
                match &mut active.kind {
                    ActiveKind::Lock { hunk_lines, .. } => {
                        hunk_lines.push(line);
                    }
                    ActiveKind::Generated {
                        added,
                        deleted,
                        parents,
                        ..
                    } => {
                        if let Some(p) = combined_parents(line) {
                            *parents = p;
                        } else if line.starts_with("@@ ") {
                            *parents = 0;
                        } else if *parents >= 2 {
                            let prefix = line.as_bytes().get(..*parents).unwrap_or(&[]);
                            if prefix.contains(&b'+') {
                                *added += 1;
                            } else if prefix.contains(&b'-') {
                                *deleted += 1;
                            }
                        } else if line.starts_with('+') {
                            *added += 1;
                        } else if line.starts_with('-') {
                            *deleted += 1;
                        }
                    }
                }
            }
        } else {
            finish(current.take(), &mut found);
        }
    }
    finish(current, &mut found);
    found
}

fn describe(
    path: &str,
    format: LockFormat,
    first: usize,
    last: usize,
    ids: &str,
    cwd: &Path,
) -> Option<Section> {
    let (old, new) = ids.split_once("..")?;
    let old = revision(old, None, cwd)?;
    let new = revision(new, Some(path), cwd)?;
    let changes = compare_format(format, &old, &new)?;
    let (head, body) = changes.render(path, first, last);
    Some(Section {
        path: path.to_owned(),
        first,
        last,
        head,
        flags: changes.flags,
        body,
        generated: false,
    })
}

fn describe_from_hunks(
    path: &str,
    format: LockFormat,
    first: usize,
    last: usize,
    new_file: bool,
    deleted_file: bool,
    hunk_lines: &[&str],
) -> Option<Section> {
    let changes = if new_file {
        let mut new_text = String::new();
        for &line in hunk_lines {
            if let Some(rest) = line.strip_prefix('+') {
                new_text.push_str(rest);
                new_text.push('\n');
            }
        }
        compare_format(format, "", &new_text)?
    } else if deleted_file {
        let mut old_text = String::new();
        for &line in hunk_lines {
            if let Some(rest) = line.strip_prefix('-') {
                old_text.push_str(rest);
                old_text.push('\n');
            }
        }
        compare_format(format, &old_text, "")?
    } else {
        compare_hunks(format, hunk_lines)?
    };
    let (head, body) = changes.render(path, first, last);
    Some(Section {
        path: path.to_owned(),
        first,
        last,
        head,
        flags: changes.flags,
        body,
        generated: false,
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

/// The packages of a `Cargo.lock` by key, or `None` when one repeats, which Cargo
/// does not write.
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

/// The locked third-party packages of an npm `package-lock.json` (v2/v3 `"packages"`).
fn npm_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let root: serde_json::Value = serde_json::from_str(text).ok()?;
    let entries = root.get("packages")?.as_object()?;
    let mut packages: BTreeMap<Key, Package> = BTreeMap::new();
    for (pkg_path, value) in entries {
        if pkg_path.is_empty()
            || value.get("link").and_then(serde_json::Value::as_bool) == Some(true)
        {
            continue;
        }
        let Some((_, tail)) = pkg_path
            .strip_prefix("node_modules/")
            .map(|rest| ("", rest))
            .or_else(|| pkg_path.rsplit_once("/node_modules/"))
        else {
            continue;
        };
        let name = value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(tail)
            .to_owned();
        let version = value.get("version")?.as_str()?.to_owned();
        let source = value
            .get("resolved")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let checksum = value
            .get("integrity")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let mut deps = BTreeSet::new();
        for field in ["dependencies", "optionalDependencies"] {
            if let Some(map) = value.get(field).and_then(serde_json::Value::as_object) {
                deps.extend(map.keys().cloned());
            }
        }
        let key = (name.clone(), version.clone(), source.clone());
        match packages.get_mut(&key) {
            Some(existing) => {
                if existing.checksum != checksum {
                    return None;
                }
                let mut merged: BTreeSet<String> = existing.dependencies.drain(..).collect();
                merged.extend(deps);
                existing.dependencies = merged.into_iter().collect();
            }
            None => {
                packages.insert(
                    key,
                    Package {
                        name,
                        version,
                        source,
                        checksum,
                        dependencies: deps.into_iter().collect(),
                    },
                );
            }
        }
    }
    Some(packages)
}

#[derive(Deserialize)]
struct UvLock {
    #[serde(default)]
    package: Vec<UvPackage>,
}

#[derive(Deserialize)]
struct UvPackage {
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    source: Option<serde_json::Value>,
    #[serde(default)]
    dependencies: Vec<UvDep>,
    #[serde(default)]
    sdist: Option<UvArtifact>,
    #[serde(default)]
    wheels: Vec<UvArtifact>,
}

#[derive(Deserialize)]
struct UvDep {
    name: String,
}

#[derive(Deserialize)]
struct UvArtifact {
    #[serde(default)]
    hash: Option<String>,
}

fn format_uv_source(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(map) if !map.is_empty() => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| match v {
                    serde_json::Value::String(s) => format!("{k}={s}"),
                    other => format!("{k}={other}"),
                })
                .collect();
            Some(parts.join(", "))
        }
        _ => None,
    }
}

/// The locked packages of a Python `uv.lock`.
fn uv_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let lock: UvLock = toml_edit::de::from_str(text).ok()?;
    let mut packages: BTreeMap<Key, Package> = BTreeMap::new();
    for pkg in lock.package {
        let Some(version) = pkg.version.filter(|v| !v.is_empty()) else {
            continue;
        };
        let source = pkg.source.as_ref().and_then(format_uv_source);
        let mut hashes: BTreeSet<String> = BTreeSet::new();
        if let Some(hash) = pkg.sdist.and_then(|a| a.hash) {
            hashes.insert(hash);
        }
        for wheel in pkg.wheels {
            if let Some(hash) = wheel.hash {
                hashes.insert(hash);
            }
        }
        let deps: BTreeSet<String> = pkg.dependencies.into_iter().map(|d| d.name).collect();
        let key = (pkg.name.clone(), version.clone(), source.clone());
        match packages.get_mut(&key) {
            Some(existing) => {
                let mut merged_hashes: BTreeSet<String> = existing
                    .checksum
                    .take()
                    .map(|c| c.split(", ").map(str::to_owned).collect())
                    .unwrap_or_default();
                merged_hashes.extend(hashes);
                existing.checksum = (!merged_hashes.is_empty())
                    .then(|| merged_hashes.into_iter().collect::<Vec<_>>().join(", "));
                let mut merged_deps: BTreeSet<String> = existing.dependencies.drain(..).collect();
                merged_deps.extend(deps);
                existing.dependencies = merged_deps.into_iter().collect();
            }
            None => {
                let checksum =
                    (!hashes.is_empty()).then(|| hashes.into_iter().collect::<Vec<_>>().join(", "));
                packages.insert(
                    key,
                    Package {
                        name: pkg.name,
                        version,
                        source,
                        checksum,
                        dependencies: deps.into_iter().collect(),
                    },
                );
            }
        }
    }
    Some(packages)
}

/// The locked module checksums of a Go `go.sum`.
fn gosum_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let mut grouped: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(module), Some(raw_version), Some(hash), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return None;
        };
        if !hash.contains(':') {
            return None;
        }
        let (version, kind) = match raw_version.strip_suffix("/go.mod") {
            Some(version) => (version, "go.mod"),
            None => (raw_version, "zip"),
        };
        grouped
            .entry((module.to_owned(), version.to_owned()))
            .or_default()
            .insert(format!("{kind}:{hash}"));
    }
    let mut packages = BTreeMap::new();
    for ((name, version), hashes) in grouped {
        let checksum = Some(hashes.into_iter().collect::<Vec<_>>().join(", "));
        packages.insert(
            (name.clone(), version.clone(), None),
            Package {
                name,
                version,
                source: None,
                checksum,
                dependencies: Vec::new(),
            },
        );
    }
    Some(packages)
}

/// Insert a parsed package, merging dependency sets when the same
/// `(name, version, source)` appears in several locations with matching checksums.
/// Returns `None` if checksums conflict for the same key.
fn insert_merged(
    packages: &mut BTreeMap<Key, Package>,
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    deps: BTreeSet<String>,
) -> Option<()> {
    let key = (name.clone(), version.clone(), source.clone());
    match packages.get_mut(&key) {
        Some(existing) => {
            if existing.checksum != checksum {
                return None;
            }
            let mut merged: BTreeSet<String> = existing.dependencies.drain(..).collect();
            merged.extend(deps);
            existing.dependencies = merged.into_iter().collect();
        }
        None => {
            packages.insert(
                key,
                Package {
                    name,
                    version,
                    source,
                    checksum,
                    dependencies: deps.into_iter().collect(),
                },
            );
        }
    }
    Some(())
}

fn unquote_lock_scalar(value: &str) -> &str {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn split_npm_descriptor(spec: &str) -> Option<(&str, &str)> {
    let spec = unquote_lock_scalar(spec);
    let start = spec.chars().next()?.len_utf8();
    let at = spec[start..].find('@')? + start;
    let name = &spec[..at];
    let rest = &spec[at + 1..];
    (!name.is_empty() && !rest.is_empty()).then_some((name, rest))
}

fn first_unquoted_item(header: &str) -> &str {
    let mut quote = None;
    for (idx, ch) in header.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == ',' => return &header[..idx],
            _ => {}
        }
    }
    header
}

fn yaml_key(line: &str) -> Option<&str> {
    let mut quote = None;
    for (idx, ch) in line.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == ':' => return Some(unquote_lock_scalar(&line[..idx])),
            _ => {}
        }
    }
    None
}

fn yarn_dep_name(line: &str) -> Option<&str> {
    if let Some(key) = yaml_key(line)
        && !key.is_empty()
    {
        return Some(key);
    }
    let mut quote = None;
    for (idx, ch) in line.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch.is_ascii_whitespace() => {
                let name = unquote_lock_scalar(&line[..idx]);
                return (!name.is_empty()).then_some(name);
            }
            _ => {}
        }
    }
    None
}

/// The locked packages of a Yarn lockfile (Classic v1 or Berry v2+ YAML).
fn yarn_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    #[derive(Default)]
    struct Entry {
        header_name: String,
        version: Option<String>,
        source: Option<String>,
        checksum: Option<String>,
        deps: BTreeSet<String>,
        sub: Option<&'static str>,
        skip: bool,
    }
    let flush = |entry: Entry, packages: &mut BTreeMap<Key, Package>| -> Option<()> {
        if entry.skip {
            return Some(());
        }
        let version = entry.version?;
        let name = entry
            .source
            .as_deref()
            .filter(|s| !s.contains("://") && !s.contains(' '))
            .and_then(|s| split_npm_descriptor(s).map(|(n, _)| n.to_owned()))
            .unwrap_or(entry.header_name);
        insert_merged(
            packages,
            name,
            version,
            entry.source,
            entry.checksum,
            entry.deps,
        )
    };
    let mut packages = BTreeMap::new();
    let mut current: Option<Entry> = None;
    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        if indent == 0 {
            if let Some(entry) = current.take() {
                flush(entry, &mut packages)?;
            }
            let header = line.trim_end().strip_suffix(':')?;
            if unquote_lock_scalar(header) == "__metadata" {
                current = Some(Entry {
                    skip: true,
                    ..Entry::default()
                });
                continue;
            }
            let first = first_unquoted_item(header);
            let (name, _) = split_npm_descriptor(first)?;
            current = Some(Entry {
                header_name: name.to_owned(),
                ..Entry::default()
            });
            continue;
        }
        let entry = current.as_mut()?;
        if entry.skip {
            continue;
        }
        if indent == 2 {
            entry.sub = None;
            if matches!(trimmed, "dependencies:" | "optionalDependencies:") {
                entry.sub = Some("deps");
            } else if trimmed.ends_with(':') {
                entry.sub = Some("other");
            } else if let Some((k, v)) = trimmed.split_once(':')
                && !k.contains(' ')
            {
                let v = unquote_lock_scalar(v);
                if !v.is_empty() {
                    match k {
                        "version" => entry.version = Some(v.to_owned()),
                        "resolution" | "resolved" => entry.source = Some(v.to_owned()),
                        "checksum" | "integrity" => entry.checksum = Some(v.to_owned()),
                        _ => {}
                    }
                }
            } else if let Some((k, v)) = trimmed.split_once(' ') {
                let v = unquote_lock_scalar(v);
                if !v.is_empty() {
                    match k {
                        "version" => entry.version = Some(v.to_owned()),
                        "resolved" => entry.source = Some(v.to_owned()),
                        "integrity" => entry.checksum = Some(v.to_owned()),
                        _ => {}
                    }
                }
            }
        } else if indent >= 4
            && entry.sub == Some("deps")
            && let Some(dep) = yarn_dep_name(trimmed)
        {
            entry.deps.insert(dep.to_owned());
        }
    }
    if let Some(entry) = current {
        flush(entry, &mut packages)?;
    }
    Some(packages)
}

fn parse_pnpm_key(raw: &str) -> Option<(String, String)> {
    let key = unquote_lock_scalar(raw.trim().strip_suffix(':')?).trim_start_matches('/');
    let base = key.split('(').next().unwrap_or(key);
    let (name, version) = split_npm_descriptor(base)?;
    Some((name.to_owned(), version.to_owned()))
}

fn parse_inline_yaml_map(map: &str) -> Option<Vec<(String, String)>> {
    let inner = map.trim().strip_prefix('{')?.strip_suffix('}')?;
    let mut pairs = Vec::new();
    let mut start = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut chars = inner.char_indices().peekable();
    let push_item = |raw: &str, pairs: &mut Vec<(String, String)>| -> Option<()> {
        let item = raw.trim();
        if item.is_empty() {
            return Some(());
        }
        let (k, v) = item.split_once(':')?;
        let key = unquote_lock_scalar(k);
        if key.is_empty() {
            return None;
        }
        pairs.push((key.to_owned(), unquote_lock_scalar(v).to_owned()));
        Some(())
    };
    while let Some((idx, ch)) = chars.next() {
        match quote {
            Some('"') => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    quote = None;
                }
            }
            Some('\'') => {
                if ch == '\'' {
                    if chars.peek().is_some_and(|&(_, next)| next == '\'') {
                        chars.next();
                    } else {
                        quote = None;
                    }
                }
            }
            Some(_) => unreachable!(),
            None => match ch {
                '"' | '\'' => quote = Some(ch),
                '{' | '}' => return None,
                ',' => {
                    push_item(&inner[start..idx], &mut pairs)?;
                    start = idx + 1;
                }
                _ => {}
            },
        }
    }
    if quote.is_some() || escaped {
        return None;
    }
    if start < inner.len() {
        push_item(&inner[start..], &mut pairs)?;
    }
    Some(pairs)
}

/// The locked packages of a pnpm `pnpm-lock.yaml` (v6 and v9).
fn pnpm_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Section {
        Other,
        Packages,
        Snapshots,
    }
    struct PkgEntry {
        name: String,
        version: String,
        source: Option<String>,
        repo: Option<String>,
        commit: Option<String>,
        checksum: Option<String>,
        deps: BTreeSet<String>,
        sub: Option<&'static str>,
    }
    let flush_pkg = |entry: PkgEntry, packages: &mut BTreeMap<Key, Package>| -> Option<()> {
        let source = entry.source.or_else(|| match (entry.repo, entry.commit) {
            (Some(repo), Some(commit)) => Some(format!("{repo}@{commit}")),
            (Some(repo), None) => Some(repo),
            _ => None,
        });
        insert_merged(
            packages,
            entry.name,
            entry.version,
            source,
            entry.checksum,
            entry.deps,
        )
    };
    let mut packages = BTreeMap::new();
    let mut snapshot_deps: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut section = Section::Other;
    let mut saw_header = false;
    let mut current_pkg: Option<PkgEntry> = None;
    let mut current_snap: Option<(String, String, Option<&'static str>)> = None;

    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        if indent == 0 {
            if let Some(pkg) = current_pkg.take() {
                flush_pkg(pkg, &mut packages)?;
            }
            current_snap = None;
            if trimmed.starts_with("lockfileVersion:") {
                saw_header = true;
                section = Section::Other;
            } else if trimmed == "packages:" {
                saw_header = true;
                section = Section::Packages;
            } else if trimmed == "packages: {}" {
                saw_header = true;
                section = Section::Other;
            } else if trimmed == "snapshots:" {
                section = Section::Snapshots;
            } else {
                section = Section::Other;
            }
            continue;
        }
        match section {
            Section::Packages => {
                if indent == 2 {
                    if let Some(pkg) = current_pkg.take() {
                        flush_pkg(pkg, &mut packages)?;
                    }
                    let (name, version) = parse_pnpm_key(trimmed)?;
                    current_pkg = Some(PkgEntry {
                        name,
                        version,
                        source: None,
                        repo: None,
                        commit: None,
                        checksum: None,
                        deps: BTreeSet::new(),
                        sub: None,
                    });
                } else if indent == 4
                    && let Some(pkg) = current_pkg.as_mut()
                {
                    pkg.sub = None;
                    if let Some(rest) = trimmed.strip_prefix("resolution:") {
                        let rest = rest.trim();
                        if rest.starts_with('{') {
                            for (k, v) in parse_inline_yaml_map(rest)? {
                                if v.is_empty() {
                                    continue;
                                }
                                match k.as_str() {
                                    "integrity" => pkg.checksum = Some(v),
                                    "tarball" | "directory" => pkg.source = Some(v),
                                    "repo" => pkg.repo = Some(v),
                                    "commit" => pkg.commit = Some(v),
                                    _ => {}
                                }
                            }
                        } else if rest.is_empty() {
                            pkg.sub = Some("resolution");
                        }
                    } else if matches!(trimmed, "dependencies:" | "optionalDependencies:") {
                        pkg.sub = Some("deps");
                    } else if trimmed.ends_with(':') {
                        pkg.sub = Some("other");
                    }
                } else if indent >= 6
                    && let Some(pkg) = current_pkg.as_mut()
                {
                    match pkg.sub {
                        Some("resolution") => {
                            if let Some((k, v)) = trimmed.split_once(':') {
                                let v = unquote_lock_scalar(v);
                                if !v.is_empty() {
                                    match unquote_lock_scalar(k) {
                                        "integrity" => pkg.checksum = Some(v.to_owned()),
                                        "tarball" | "directory" => pkg.source = Some(v.to_owned()),
                                        "repo" => pkg.repo = Some(v.to_owned()),
                                        "commit" => pkg.commit = Some(v.to_owned()),
                                        _ => {}
                                    }
                                }
                            }
                        }
                        Some("deps") => {
                            if let Some(dep) = yaml_key(trimmed)
                                && !dep.is_empty()
                            {
                                pkg.deps.insert(dep.to_owned());
                            }
                        }
                        _ => {}
                    }
                }
            }
            Section::Snapshots => {
                if indent == 2 {
                    current_snap = parse_pnpm_key(trimmed).map(|(n, v)| (n, v, None));
                } else if indent == 4
                    && let Some((_, _, sub)) = current_snap.as_mut()
                {
                    *sub = matches!(trimmed, "dependencies:" | "optionalDependencies:")
                        .then_some("deps");
                } else if indent >= 6
                    && let Some((name, version, Some("deps"))) = current_snap.as_ref()
                    && let Some(dep) = yaml_key(trimmed)
                    && !dep.is_empty()
                {
                    snapshot_deps
                        .entry((name.clone(), version.clone()))
                        .or_default()
                        .insert(dep.to_owned());
                }
            }
            Section::Other => {}
        }
    }
    if let Some(pkg) = current_pkg {
        flush_pkg(pkg, &mut packages)?;
    }
    if !saw_header {
        return None;
    }
    for ((name, version), deps) in snapshot_deps {
        for pkg in packages.values_mut() {
            if pkg.name == name && pkg.version == version {
                let mut merged: BTreeSet<String> = pkg.dependencies.drain(..).collect();
                merged.extend(deps.iter().cloned());
                pkg.dependencies = merged.into_iter().collect();
            }
        }
    }
    Some(packages)
}

/// Remove trailing commas before `]` or `}` outside JSON strings so Bun's JSONC
/// `bun.lock` parses as standard JSON.
fn strip_jsonc_trailing_commas(text: &str) -> Cow<'_, str> {
    if !text.contains(',') {
        return Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut chunk_start = 0;
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if b == b',' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if matches!(bytes.get(j), Some(b']' | b'}')) {
                out.push_str(&text[chunk_start..i]);
                i += 1;
                chunk_start = i;
                continue;
            }
        }
        i += 1;
    }
    if chunk_start == 0 {
        return Cow::Borrowed(text);
    }
    out.push_str(&text[chunk_start..]);
    Cow::Owned(out)
}

/// The locked third-party packages of a Bun `bun.lock` (text JSONC format).
fn bun_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let clean = strip_jsonc_trailing_commas(text);
    let root: serde_json::Value = serde_json::from_str(&clean).ok()?;
    root.get("lockfileVersion")?;
    let entries = root.get("packages")?.as_object()?;
    let mut packages = BTreeMap::new();
    for value in entries.values() {
        let arr = value.as_array()?;
        let ident = arr.first()?.as_str()?;
        let (name, version) = split_npm_descriptor(ident)?;
        if version.starts_with("workspace:") {
            continue;
        }
        let source = arr
            .get(1)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let mut deps = BTreeSet::new();
        if let Some(meta) = arr.get(2).and_then(serde_json::Value::as_object) {
            for field in ["dependencies", "optionalDependencies"] {
                if let Some(map) = meta.get(field).and_then(serde_json::Value::as_object) {
                    deps.extend(map.keys().cloned());
                }
            }
        }
        let checksum = arr
            .get(3)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        insert_merged(
            &mut packages,
            name.to_owned(),
            version.to_owned(),
            source,
            checksum,
            deps,
        )?;
    }
    Some(packages)
}

#[derive(Deserialize)]
struct PoetryLock {
    #[serde(default)]
    package: Vec<PoetryPackage>,
    #[serde(default)]
    metadata: Option<PoetryMetadata>,
}

#[derive(Deserialize)]
struct PoetryMetadata {
    #[serde(default)]
    files: BTreeMap<String, Vec<PoetryFile>>,
}

#[derive(Deserialize)]
struct PoetryPackage {
    name: String,
    version: String,
    #[serde(default)]
    source: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    files: Vec<PoetryFile>,
    #[serde(default)]
    dependencies: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct PoetryFile {
    #[serde(default)]
    hash: Option<String>,
}

fn format_sorted_map(map: &BTreeMap<String, serde_json::Value>) -> Option<String> {
    if map.is_empty() {
        return None;
    }
    let parts: Vec<String> = map
        .iter()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => format!("{k}={s}"),
            other => format!("{k}={other}"),
        })
        .collect();
    Some(parts.join(", "))
}

/// The locked packages of a Python `poetry.lock` (v1 and v2).
fn poetry_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let lock: PoetryLock = toml_edit::de::from_str(text).ok()?;
    if lock.metadata.is_none() && lock.package.is_empty() {
        return None;
    }
    let meta_files = lock.metadata.as_ref().map(|m| &m.files);
    let mut packages = BTreeMap::new();
    for pkg in lock.package {
        let source = pkg.source.as_ref().and_then(format_sorted_map);
        let mut hashes: BTreeSet<String> = pkg
            .files
            .into_iter()
            .filter_map(|f| f.hash.filter(|h| !h.is_empty()))
            .collect();
        if hashes.is_empty()
            && let Some(files) = meta_files.and_then(|m| m.get(&pkg.name))
        {
            hashes.extend(
                files
                    .iter()
                    .filter_map(|f| f.hash.as_ref().filter(|h| !h.is_empty()).cloned()),
            );
        }
        let checksum =
            (!hashes.is_empty()).then(|| hashes.into_iter().collect::<Vec<_>>().join(", "));
        let deps: BTreeSet<String> = pkg.dependencies.into_keys().collect();
        insert_merged(&mut packages, pkg.name, pkg.version, source, checksum, deps)?;
    }
    Some(packages)
}

fn parse_gem_spec(trimmed: &str) -> Option<(String, String)> {
    let (name, rest) = trimmed.split_once(" (")?;
    let version = rest.strip_suffix(')')?;
    if name.is_empty() || name.contains(' ') || version.is_empty() {
        return None;
    }
    Some((name.to_owned(), version.to_owned()))
}

/// The locked gems of a Ruby Bundler `Gemfile.lock`.
fn gemfile_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Section {
        None,
        Gem,
        Git,
        Path,
        Checksums,
        Other,
    }
    let mut section = Section::None;
    let mut saw_header = false;
    let mut in_specs = false;
    let mut remotes: Vec<String> = Vec::new();
    let mut revision: Option<String> = None;
    let mut current_gem: Option<(String, String, Option<String>, BTreeSet<String>)> = None;
    let mut packages: BTreeMap<Key, Package> = BTreeMap::new();
    let mut checksums: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();

    let flush_gem = |gem: Option<(String, String, Option<String>, BTreeSet<String>)>,
                     packages: &mut BTreeMap<Key, Package>|
     -> Option<()> {
        if let Some((name, version, source, deps)) = gem {
            insert_merged(packages, name, version, source, None, deps)?;
        }
        Some(())
    };

    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush_gem(current_gem.take(), &mut packages)?;
            in_specs = false;
            continue;
        }
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        if indent == 0 {
            flush_gem(current_gem.take(), &mut packages)?;
            in_specs = false;
            remotes.clear();
            revision = None;
            section = match trimmed {
                "GEM" => {
                    saw_header = true;
                    Section::Gem
                }
                "GIT" => {
                    saw_header = true;
                    Section::Git
                }
                "PATH" => {
                    saw_header = true;
                    Section::Path
                }
                "CHECKSUMS" => {
                    saw_header = true;
                    Section::Checksums
                }
                "PLATFORMS" | "DEPENDENCIES" | "RUBY VERSION" | "BUNDLED WITH" => {
                    saw_header = true;
                    Section::Other
                }
                _ => return None,
            };
            continue;
        }
        match section {
            Section::Gem | Section::Git | Section::Path => {
                if indent == 2 {
                    flush_gem(current_gem.take(), &mut packages)?;
                    if let Some(remote) = trimmed.strip_prefix("remote:") {
                        let remote = remote.trim();
                        if !remote.is_empty() {
                            remotes.push(remote.to_owned());
                        }
                    } else if let Some(rev) = trimmed.strip_prefix("revision:") {
                        let rev = rev.trim();
                        if !rev.is_empty() {
                            revision = Some(rev.to_owned());
                        }
                    } else if trimmed == "specs:" {
                        in_specs = true;
                    }
                } else if in_specs && indent == 4 {
                    flush_gem(current_gem.take(), &mut packages)?;
                    let (name, version) = parse_gem_spec(trimmed)?;
                    let source = match section {
                        Section::Git => {
                            let remote = remotes.join(", ");
                            match (remote.is_empty(), &revision) {
                                (false, Some(rev)) => Some(format!("git:{remote}@{rev}")),
                                (false, None) => Some(format!("git:{remote}")),
                                (true, Some(rev)) => Some(format!("git:{rev}")),
                                (true, None) => None,
                            }
                        }
                        Section::Path => {
                            (!remotes.is_empty()).then(|| format!("path:{}", remotes.join(", ")))
                        }
                        Section::Gem => (!remotes.is_empty()).then(|| remotes.join(", ")),
                        _ => None,
                    };
                    current_gem = Some((name, version, source, BTreeSet::new()));
                } else if in_specs
                    && indent >= 6
                    && let Some((_, _, _, deps)) = current_gem.as_mut()
                {
                    let dep = trimmed.split_whitespace().next().unwrap_or("");
                    if !dep.is_empty() {
                        deps.insert(dep.to_owned());
                    }
                }
            }
            Section::Checksums if indent == 2 => {
                // Format: `gem_name (1.2.3) sha256=abcdef...`
                if let Some((spec_part, rest)) = trimmed.split_once(") ") {
                    let with_paren = format!("{spec_part})");
                    if let Some((name, version)) = parse_gem_spec(&with_paren) {
                        let hash = rest.trim();
                        if !hash.is_empty() {
                            checksums
                                .entry((name, version))
                                .or_default()
                                .insert(hash.to_owned());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    flush_gem(current_gem.take(), &mut packages)?;
    if !saw_header {
        return None;
    }
    for ((name, version), hashes) in checksums {
        let joined = hashes.into_iter().collect::<Vec<_>>().join(", ");
        for pkg in packages.values_mut() {
            if pkg.name == name && pkg.version == version {
                pkg.checksum = Some(joined.clone());
            }
        }
    }
    Some(packages)
}

/// The locked packages of a PHP Composer `composer.lock`.
fn composer_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let root: serde_json::Value = serde_json::from_str(text).ok()?;
    let main = root.get("packages")?.as_array()?;
    let mut packages = BTreeMap::new();
    let lists = [
        Some(main),
        root.get("packages-dev")
            .and_then(serde_json::Value::as_array),
    ];
    for list in lists.into_iter().flatten() {
        for entry in list {
            let name = entry.get("name")?.as_str()?.to_owned();
            let version = entry.get("version")?.as_str()?.to_owned();
            let src_obj = entry.get("source").and_then(serde_json::Value::as_object);
            let dist_obj = entry.get("dist").and_then(serde_json::Value::as_object);
            let src_url = src_obj
                .and_then(|o| o.get("url"))
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty());
            let dist_url = dist_obj
                .and_then(|o| o.get("url"))
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty());
            let source = match (src_url, dist_url) {
                (Some(s), Some(d)) if s != d => Some(format!("{s} | {d}")),
                (Some(s), _) => Some(s.to_owned()),
                (None, Some(d)) => Some(d.to_owned()),
                (None, None) => None,
            };
            let mut hashes = BTreeSet::new();
            if let Some(shasum) = dist_obj
                .and_then(|o| o.get("shasum"))
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
            {
                hashes.insert(format!("shasum:{shasum}"));
            }
            if let Some(reference) = dist_obj
                .and_then(|o| o.get("reference"))
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
            {
                hashes.insert(format!("dist-ref:{reference}"));
            }
            if let Some(reference) = src_obj
                .and_then(|o| o.get("reference"))
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
            {
                hashes.insert(format!("src-ref:{reference}"));
            }
            let checksum =
                (!hashes.is_empty()).then(|| hashes.into_iter().collect::<Vec<_>>().join(", "));
            let mut deps = BTreeSet::new();
            if let Some(req) = entry.get("require").and_then(serde_json::Value::as_object) {
                for dep in req.keys() {
                    if dep.contains('/') {
                        deps.insert(dep.clone());
                    }
                }
            }
            insert_merged(&mut packages, name, version, source, checksum, deps)?;
        }
    }
    Some(packages)
}

fn short_rev(rev: &str) -> &str {
    let n = rev.floor_char_boundary(rev.len().min(12));
    &rev[..n]
}

fn format_flake_source(locked: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let kind = locked
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    if let (Some(owner), Some(repo)) = (
        locked.get("owner").and_then(serde_json::Value::as_str),
        locked.get("repo").and_then(serde_json::Value::as_str),
    ) {
        if let Some(host) = locked.get("host").and_then(serde_json::Value::as_str) {
            return Some(format!("{kind}:{host}/{owner}/{repo}"));
        }
        return Some(format!("{kind}:{owner}/{repo}"));
    }
    if let Some(url) = locked.get("url").and_then(serde_json::Value::as_str) {
        return Some(format!("{kind}:{url}"));
    }
    if let Some(path) = locked.get("path").and_then(serde_json::Value::as_str) {
        return Some(format!("{kind}:{path}"));
    }
    Some(kind.to_owned())
}

/// The locked flake inputs of a Nix `flake.lock`.
fn flake_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let root: serde_json::Value = serde_json::from_str(text).ok()?;
    let root_name = root.get("root")?.as_str()?;
    root.get("version")?.as_u64()?;
    let nodes = root.get("nodes")?.as_object()?;
    let mut packages = BTreeMap::new();
    for (name, node) in nodes {
        if name == root_name {
            continue;
        }
        let Some(locked) = node.get("locked").and_then(serde_json::Value::as_object) else {
            continue;
        };
        let version = if let Some(rev) = locked.get("rev").and_then(serde_json::Value::as_str) {
            short_rev(rev).to_owned()
        } else if let Some(ts) = locked
            .get("lastModified")
            .and_then(serde_json::Value::as_i64)
        {
            format!("t{ts}")
        } else {
            "unversioned".to_owned()
        };
        let source = format_flake_source(locked);
        let checksum = locked
            .get("narHash")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let mut deps = BTreeSet::new();
        if let Some(inputs) = node.get("inputs").and_then(serde_json::Value::as_object) {
            deps.extend(inputs.keys().cloned());
        }
        insert_merged(&mut packages, name.clone(), version, source, checksum, deps)?;
    }
    Some(packages)
}

/// The locked pins of a Swift Package Manager `Package.resolved` (v1, v2 and v3).
fn swift_resolved_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let root: serde_json::Value = serde_json::from_str(text).ok()?;
    root.get("version")?.as_u64()?;
    let pins = root
        .get("pins")
        .or_else(|| root.get("object").and_then(|o| o.get("pins")))?
        .as_array()?;
    let mut packages = BTreeMap::new();
    for pin in pins {
        let name = pin
            .get("identity")
            .or_else(|| pin.get("package"))?
            .as_str()?
            .to_owned();
        let source = pin
            .get("location")
            .or_else(|| pin.get("repositoryURL"))
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let state = pin.get("state")?.as_object()?;
        let revision = state
            .get("revision")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let version = if let Some(ver) = state
            .get("version")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
        {
            ver.to_owned()
        } else {
            let short = revision.as_deref().map(short_rev).unwrap_or("unversioned");
            match state
                .get("branch")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
            {
                Some(branch) => format!("{branch}@{short}"),
                None => short.to_owned(),
            }
        };
        insert_merged(
            &mut packages,
            name,
            version,
            source,
            revision,
            BTreeSet::new(),
        )?;
    }
    Some(packages)
}

/// Split top-level comma-separated elements inside balanced Elixir `{}`, `[]`, `""`.
fn split_elixir_top_level(inner: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (idx, ch) in inner.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let piece = inner[start..idx].trim();
                if !piece.is_empty() {
                    parts.push(piece);
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    let tail = inner[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

fn parse_elixir_string(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let inner = trimmed.strip_prefix('"')?.strip_suffix('"')?;
    Some(inner.replace("\\\"", "\"").replace("\\\\", "\\"))
}

fn parse_mix_deps(raw: &str) -> BTreeSet<String> {
    let mut deps = BTreeSet::new();
    let Some(inner) = raw
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
    else {
        return deps;
    };
    for elem in split_elixir_top_level(inner) {
        let Some(tuple) = elem.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
            continue;
        };
        let fields = split_elixir_top_level(tuple);
        if let Some(first) = fields.first().and_then(|s| s.strip_prefix(':')) {
            let dep = unquote_lock_scalar(first);
            if !dep.is_empty() {
                deps.insert(dep.to_owned());
            }
        }
    }
    deps
}

/// The locked dependencies of an Elixir `mix.lock`.
fn mix_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let stripped: Cow<'_, str> = if text.contains('#') {
        let mut buf = String::with_capacity(text.len());
        for line in text.lines() {
            if !line.trim_start().starts_with('#') {
                buf.push_str(line);
                buf.push('\n');
            }
        }
        Cow::Owned(buf)
    } else {
        Cow::Borrowed(text)
    };
    let trimmed = stripped.trim();
    let inner = trimmed.strip_prefix("%{")?.strip_suffix('}')?;
    let mut packages = BTreeMap::new();
    for entry in split_elixir_top_level(inner) {
        let (key_raw, value_raw) = if let Some(pair) = entry.split_once("\":") {
            (format!("{}\"", pair.0.trim()), pair.1.trim())
        } else {
            let pair = entry.split_once("=>")?;
            (pair.0.trim().to_owned(), pair.1.trim())
        };
        let name = parse_elixir_string(&key_raw)
            .or_else(|| key_raw.strip_prefix(':').map(str::to_owned))?;
        let tuple_inner = value_raw.strip_prefix('{')?.strip_suffix('}')?;
        let fields = split_elixir_top_level(tuple_inner);
        match fields.first().copied() {
            Some(":hex") => {
                // {:hex, :pkg, "ver", "inner_hash", [:mix], [deps], "repo"[, "outer_hash"]}
                let version = parse_elixir_string(fields.get(2)?)?;
                let inner_hash = parse_elixir_string(fields.get(3)?)?;
                let deps = fields
                    .get(5)
                    .map(|raw| parse_mix_deps(raw))
                    .unwrap_or_default();
                let repo = fields
                    .get(6)
                    .and_then(|raw| parse_elixir_string(raw))
                    .filter(|s| !s.is_empty());
                let outer_hash = fields
                    .get(7)
                    .and_then(|raw| parse_elixir_string(raw))
                    .filter(|s| !s.is_empty());
                let source = Some(match repo {
                    Some(r) => format!("hex:{r}"),
                    None => "hex".to_owned(),
                });
                let checksum = match outer_hash {
                    Some(outer) if outer != inner_hash => Some(format!("{inner_hash}, {outer}")),
                    _ => Some(inner_hash),
                };
                insert_merged(&mut packages, name, version, source, checksum, deps)?;
            }
            Some(":git") => {
                // {:git, "url", "ref", [opts...]}
                let url = parse_elixir_string(fields.get(1)?)?;
                let reference = parse_elixir_string(fields.get(2)?)?;
                let version = short_rev(&reference).to_owned();
                let source = Some(format!("git:{url}"));
                insert_merged(
                    &mut packages,
                    name,
                    version,
                    source,
                    Some(reference),
                    BTreeSet::new(),
                )?;
            }
            _ => return None,
        }
    }
    Some(packages)
}

/// The locked dependencies of a Dart / Flutter `pubspec.lock`.
fn pubspec_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum TopSection {
        None,
        Packages,
        Sdks,
    }
    #[derive(Default)]
    struct PubspecEntry {
        name: Option<String>,
        version: Option<String>,
        source_kind: Option<String>,
        desc_scalar: Option<String>,
        desc_name: Option<String>,
        url: Option<String>,
        sha256: Option<String>,
        resolved_ref: Option<String>,
        git_ref: Option<String>,
        path: Option<String>,
        relative: Option<String>,
        in_description: bool,
    }
    let flush = |entry: PubspecEntry, packages: &mut BTreeMap<Key, Package>| -> Option<()> {
        let Some(name) = entry.name else {
            return Some(());
        };
        let version = entry.version?;
        let source = match entry.source_kind.as_deref() {
            Some("hosted") => {
                if entry.path.is_some() || entry.git_ref.is_some() || entry.relative.is_some() {
                    return None;
                }
                let base = match (entry.url, entry.desc_name) {
                    (Some(url), Some(dname)) => format!("hosted:{url}/{dname}"),
                    (Some(url), None) => format!("hosted:{url}"),
                    (None, Some(dname)) => format!("hosted:{dname}"),
                    (None, None) => "hosted".to_owned(),
                };
                Some(base)
            }
            Some("git") => {
                let mut s = match entry.url {
                    Some(url) => format!("git:{url}"),
                    None => "git".to_owned(),
                };
                if let Some(path) = entry.path {
                    s.push('#');
                    s.push_str(&path);
                }
                if let Some(git_ref) = entry.git_ref {
                    s.push('@');
                    s.push_str(&git_ref);
                }
                if let Some(dname) = entry.desc_name {
                    s.push_str(";name=");
                    s.push_str(&dname);
                }
                if let Some(rel) = entry.relative {
                    s.push_str(";relative=");
                    s.push_str(&rel);
                }
                Some(s)
            }
            Some("path") => {
                if entry.url.is_some() || entry.git_ref.is_some() || entry.desc_name.is_some() {
                    return None;
                }
                let mut s = match entry.path {
                    Some(path) => format!("path:{path}"),
                    None => "path".to_owned(),
                };
                if let Some(rel) = entry.relative {
                    s.push_str(";relative=");
                    s.push_str(&rel);
                }
                Some(s)
            }
            Some("sdk") => {
                if entry.url.is_some()
                    || entry.path.is_some()
                    || entry.git_ref.is_some()
                    || entry.relative.is_some()
                {
                    return None;
                }
                Some(match entry.desc_scalar.or(entry.desc_name) {
                    Some(sdk) => format!("sdk:{sdk}"),
                    None => "sdk".to_owned(),
                })
            }
            Some(_) => return None,
            None => entry.url.or(entry.path),
        };
        let checksum = match (entry.sha256, entry.resolved_ref) {
            (Some(sha), Some(rev)) => Some(format!("{sha} ({rev})")),
            (sha, rev) => sha.or(rev),
        };
        insert_merged(packages, name, version, source, checksum, BTreeSet::new())
    };
    let mut top = TopSection::None;
    let mut saw_packages = false;
    let mut cur = PubspecEntry::default();
    let mut packages = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        if indent == 0 {
            flush(std::mem::take(&mut cur), &mut packages)?;
            match trimmed {
                "packages:" => {
                    saw_packages = true;
                    top = TopSection::Packages;
                }
                "packages: {}" => {
                    saw_packages = true;
                    top = TopSection::None;
                }
                "sdks:" => top = TopSection::Sdks,
                "sdks: {}" => top = TopSection::None,
                _ => return None,
            }
            continue;
        }
        match top {
            TopSection::Sdks => {
                if indent != 2 {
                    return None;
                }
                let (k, v) = trimmed.split_once(':')?;
                let sdk = unquote_lock_scalar(k);
                let constraint = unquote_lock_scalar(v);
                if sdk.is_empty() || constraint.is_empty() {
                    return None;
                }
                insert_merged(
                    &mut packages,
                    format!("sdk:{sdk}"),
                    constraint.to_owned(),
                    Some("sdk-constraint".to_owned()),
                    None,
                    BTreeSet::new(),
                )?;
            }
            TopSection::Packages => match indent {
                2 => {
                    flush(std::mem::take(&mut cur), &mut packages)?;
                    let raw_name = trimmed.strip_suffix(':')?;
                    let name = unquote_lock_scalar(raw_name);
                    if name.is_empty() {
                        return None;
                    }
                    cur.name = Some(name.to_owned());
                }
                4 => {
                    cur.name.as_ref()?;
                    let (k, v) = trimmed.split_once(':')?;
                    let key = unquote_lock_scalar(k);
                    let val = v.trim();
                    cur.in_description = false;
                    match key {
                        "version" => {
                            let s = unquote_lock_scalar(val);
                            if s.is_empty() {
                                return None;
                            }
                            cur.version = Some(s.to_owned());
                        }
                        "source" => {
                            let s = unquote_lock_scalar(val);
                            if s.is_empty() {
                                return None;
                            }
                            cur.source_kind = Some(s.to_owned());
                        }
                        "dependency" => {
                            if unquote_lock_scalar(val).is_empty() {
                                return None;
                            }
                        }
                        "description" => {
                            if val.is_empty() {
                                cur.in_description = true;
                            } else if val.starts_with('{') {
                                let pairs = parse_inline_yaml_map(val)?;
                                for (dk, dv) in pairs {
                                    match dk.as_str() {
                                        "name" => {
                                            if !dv.is_empty() {
                                                cur.desc_name = Some(dv);
                                            }
                                        }
                                        "url" => {
                                            if !dv.is_empty() {
                                                cur.url = Some(dv);
                                            }
                                        }
                                        "sha256" => {
                                            if !dv.is_empty() {
                                                cur.sha256 = Some(dv);
                                            }
                                        }
                                        "resolved-ref" => {
                                            if !dv.is_empty() {
                                                cur.resolved_ref = Some(dv);
                                            }
                                        }
                                        "ref" => {
                                            if !dv.is_empty() {
                                                cur.git_ref = Some(dv);
                                            }
                                        }
                                        "path" => {
                                            if !dv.is_empty() {
                                                cur.path = Some(dv);
                                            }
                                        }
                                        "relative" => {
                                            if !dv.is_empty() {
                                                cur.relative = Some(dv);
                                            }
                                        }
                                        _ => return None,
                                    }
                                }
                            } else {
                                let s = unquote_lock_scalar(val);
                                if !s.is_empty() {
                                    cur.desc_scalar = Some(s.to_owned());
                                }
                            }
                        }
                        _ => return None,
                    }
                }
                6 if cur.in_description => {
                    let (k, v) = trimmed.split_once(':')?;
                    let key = unquote_lock_scalar(k);
                    let val = unquote_lock_scalar(v);
                    match key {
                        "name" => {
                            if !val.is_empty() {
                                cur.desc_name = Some(val.to_owned());
                            }
                        }
                        "url" => {
                            if !val.is_empty() {
                                cur.url = Some(val.to_owned());
                            }
                        }
                        "sha256" => {
                            if !val.is_empty() {
                                cur.sha256 = Some(val.to_owned());
                            }
                        }
                        "resolved-ref" => {
                            if !val.is_empty() {
                                cur.resolved_ref = Some(val.to_owned());
                            }
                        }
                        "ref" => {
                            if !val.is_empty() {
                                cur.git_ref = Some(val.to_owned());
                            }
                        }
                        "path" => {
                            if !val.is_empty() {
                                cur.path = Some(val.to_owned());
                            }
                        }
                        "relative" => {
                            if !val.is_empty() {
                                cur.relative = Some(val.to_owned());
                            }
                        }
                        _ => return None,
                    }
                }
                _ => return None,
            },
            TopSection::None => return None,
        }
    }
    flush(cur, &mut packages)?;
    saw_packages.then_some(packages)
}

/// The locked `group:artifact:version=configs` coordinates of a Gradle `gradle.lockfile`.
fn gradle_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let mut packages = BTreeMap::new();
    let mut saw_entry = false;
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (coord, configs_raw) = trimmed.split_once('=')?;
        if coord == "empty" {
            if configs_raw.chars().any(char::is_whitespace) {
                return None;
            }
            saw_entry = true;
            continue;
        }
        if coord.chars().any(char::is_whitespace) || configs_raw.chars().any(char::is_whitespace) {
            return None;
        }
        let (group, rest) = coord.split_once(':')?;
        let (artifact, version) = rest.split_once(':')?;
        if group.is_empty() || artifact.is_empty() || version.is_empty() || version.contains(':') {
            return None;
        }
        let configs: BTreeSet<String> = configs_raw
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        let name = format!("{group}:{artifact}");
        insert_merged(&mut packages, name, version.to_owned(), None, None, configs)?;
        saw_entry = true;
    }
    saw_entry.then_some(packages)
}

fn parse_pod_spec(raw: &str) -> Option<(String, String)> {
    let rest = raw.strip_prefix("- ")?.trim();
    let rest = rest.strip_suffix(':').unwrap_or(rest).trim();
    let unquoted = unquote_lock_scalar(rest);
    let (name, ver_rest) = unquoted.split_once(" (")?;
    let version = ver_rest.strip_suffix(')')?;
    if name.is_empty()
        || name.contains(' ')
        || version.is_empty()
        || version.chars().any(char::is_whitespace)
        || version.starts_with(['~', '=', '>', '<'])
    {
        return None;
    }
    Some((name.to_owned(), version.to_owned()))
}

fn parse_pod_dep_name(raw: &str) -> Option<&str> {
    let rest = raw.strip_prefix("- ")?.trim();
    let unquoted = unquote_lock_scalar(rest);
    let name = unquoted
        .split_once(" (")
        .map_or(unquoted, |(n, _)| n)
        .trim();
    (!name.is_empty() && !name.contains(' ')).then_some(name)
}

/// The locked pods of a CocoaPods `Podfile.lock`.
fn podfile_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Section {
        None,
        Pods,
        SpecRepos,
        SpecChecksums,
        ExternalSources,
        CheckoutOptions,
        CocoaPodsPlugins,
        Other,
    }
    #[derive(Default)]
    struct PodExternal {
        sources: BTreeMap<&'static str, String>,
        pins: BTreeMap<&'static str, String>,
    }
    let insert_field =
        |map: &mut BTreeMap<&'static str, String>, key: &'static str, val: String| -> Option<()> {
            if let Some(prev) = map.get(key)
                && prev != &val
            {
                return None;
            }
            map.insert(key, val);
            Some(())
        };
    let mut section = Section::None;
    let mut saw_pods = false;
    let mut pods: Vec<(String, String, BTreeSet<String>)> = Vec::new();
    let mut repos: BTreeMap<String, String> = BTreeMap::new();
    let mut ext_sources: BTreeMap<String, PodExternal> = BTreeMap::new();
    let mut checksums: BTreeMap<String, String> = BTreeMap::new();
    let mut plugins: Vec<(String, String)> = Vec::new();
    let mut current_repo: Option<String> = None;
    let mut current_ext_pod: Option<String> = None;

    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.bytes().take_while(|&b| b == b' ').count();
        if indent == 0 {
            current_repo = None;
            current_ext_pod = None;
            section = match trimmed {
                "PODS:" => {
                    saw_pods = true;
                    Section::Pods
                }
                "SPEC REPOS:" => Section::SpecRepos,
                "SPEC CHECKSUMS:" => Section::SpecChecksums,
                "EXTERNAL SOURCES:" => Section::ExternalSources,
                "CHECKOUT OPTIONS:" => Section::CheckoutOptions,
                "COCOAPODS PLUGINS:" => Section::CocoaPodsPlugins,
                "COCOAPODS PLUGINS: {}" | "DEPENDENCIES:" => Section::Other,
                _ if trimmed.starts_with("PODFILE CHECKSUM:")
                    || trimmed.starts_with("COCOAPODS:") =>
                {
                    Section::Other
                }
                _ => return None,
            };
            continue;
        }
        match section {
            Section::Pods => match indent {
                2 => {
                    let (name, version) = parse_pod_spec(trimmed)?;
                    pods.push((name, version, BTreeSet::new()));
                }
                4 => {
                    let dep = parse_pod_dep_name(trimmed)?;
                    pods.last_mut()?.2.insert(dep.to_owned());
                }
                _ => return None,
            },
            Section::SpecRepos => match indent {
                2 => {
                    let repo = unquote_lock_scalar(trimmed.strip_suffix(':')?);
                    if repo.is_empty() {
                        return None;
                    }
                    current_repo = Some(repo.to_owned());
                }
                4 => {
                    let repo = current_repo.as_ref()?;
                    let pod = unquote_lock_scalar(trimmed.strip_prefix("- ")?);
                    if pod.is_empty() {
                        return None;
                    }
                    repos.insert(pod.to_owned(), repo.clone());
                }
                _ => return None,
            },
            Section::ExternalSources | Section::CheckoutOptions => match indent {
                2 => {
                    let pod = unquote_lock_scalar(trimmed.strip_suffix(':')?);
                    if pod.is_empty() {
                        return None;
                    }
                    current_ext_pod = Some(pod.to_owned());
                }
                4 => {
                    let pod = current_ext_pod.as_ref()?;
                    let (k, v) = trimmed.split_once(": ")?;
                    let key = unquote_lock_scalar(k);
                    let val = unquote_lock_scalar(v);
                    if val.is_empty() {
                        return None;
                    }
                    let entry = ext_sources.entry(pod.clone()).or_default();
                    match key {
                        ":git" => insert_field(&mut entry.sources, "git", format!("git:{val}"))?,
                        ":path" => insert_field(&mut entry.sources, "path", format!("path:{val}"))?,
                        ":http" => insert_field(&mut entry.sources, "http", format!("http:{val}"))?,
                        ":podspec" => {
                            insert_field(&mut entry.sources, "podspec", format!("podspec:{val}"))?
                        }
                        ":branch" => {
                            insert_field(&mut entry.sources, "branch", format!("branch={val}"))?
                        }
                        ":tag" => insert_field(&mut entry.sources, "tag", format!("tag={val}"))?,
                        ":commit" => {
                            insert_field(&mut entry.pins, "commit", format!("commit={val}"))?
                        }
                        ":sha256" => {
                            insert_field(&mut entry.pins, "sha256", format!("sha256={val}"))?
                        }
                        ":sha1" => insert_field(&mut entry.pins, "sha1", format!("sha1={val}"))?,
                        _ => return None,
                    }
                }
                _ => return None,
            },
            Section::SpecChecksums if indent == 2 => {
                let (k, v) = trimmed.split_once(':')?;
                let pod = unquote_lock_scalar(k);
                let hash = unquote_lock_scalar(v);
                if pod.is_empty() || hash.is_empty() {
                    return None;
                }
                checksums.insert(pod.to_owned(), hash.to_owned());
            }
            Section::CocoaPodsPlugins if indent == 2 => {
                let (k, v) = trimmed.split_once(':')?;
                let plugin = unquote_lock_scalar(k);
                let version = unquote_lock_scalar(v);
                if plugin.is_empty() || version.is_empty() {
                    return None;
                }
                plugins.push((plugin.to_owned(), version.to_owned()));
            }
            Section::Other => {}
            _ => return None,
        }
    }
    if !saw_pods {
        return None;
    }
    let exact_names: BTreeSet<String> = pods.iter().map(|(n, _, _)| n.clone()).collect();
    let pod_roots: BTreeSet<String> = pods
        .iter()
        .map(|(n, _, _)| n.split('/').next().unwrap_or(n).to_owned())
        .collect();
    let mut normalized_ext: BTreeMap<String, PodExternal> = BTreeMap::new();
    for (key, ext) in ext_sources {
        let target = if exact_names.contains(&key) {
            key
        } else {
            let root = key.split('/').next().unwrap_or(&key);
            if pod_roots.contains(root) {
                root.to_owned()
            } else {
                key
            }
        };
        let dst = normalized_ext.entry(target).or_default();
        for (field, val) in ext.sources {
            insert_field(&mut dst.sources, field, val)?;
        }
        for (field, val) in ext.pins {
            insert_field(&mut dst.pins, field, val)?;
        }
    }
    let mut normalized_checksums: BTreeMap<String, String> = BTreeMap::new();
    for (key, hash) in checksums {
        let target = if exact_names.contains(&key) {
            key
        } else {
            let root = key.split('/').next().unwrap_or(&key);
            if pod_roots.contains(root) {
                root.to_owned()
            } else {
                key
            }
        };
        if let Some(prev) = normalized_checksums.get(&target)
            && prev != &hash
        {
            return None;
        }
        normalized_checksums.insert(target, hash);
    }
    let mut attached_ext: BTreeSet<String> = BTreeSet::new();
    let mut attached_checksums: BTreeSet<String> = BTreeSet::new();
    let mut packages = BTreeMap::new();
    for (name, version, deps) in pods {
        let root = name.split('/').next().unwrap_or(&name);
        let ext = if let Some(exact) = normalized_ext.get(&name) {
            attached_ext.insert(name.clone());
            Some(exact)
        } else if let Some(by_root) = normalized_ext.get(root) {
            attached_ext.insert(root.to_owned());
            Some(by_root)
        } else {
            None
        };
        let source = if let Some(ext) = ext
            && !ext.sources.is_empty()
        {
            Some(ext.sources.values().cloned().collect::<Vec<_>>().join(" "))
        } else {
            repos.get(root).cloned()
        };
        let spec_hash = if let Some(hash) = normalized_checksums.get(&name) {
            attached_checksums.insert(name.clone());
            Some(hash.clone())
        } else {
            normalized_checksums.get(root).map(|h| {
                attached_checksums.insert(root.to_owned());
                h.clone()
            })
        };
        let ext_pins = ext
            .filter(|e| !e.pins.is_empty())
            .map(|e| e.pins.values().cloned().collect::<Vec<_>>().join(", "));
        let checksum = match (spec_hash, ext_pins) {
            (Some(h), Some(pins)) => Some(format!("{h} ({pins})")),
            (Some(h), None) => Some(h),
            (None, Some(pins)) => Some(pins),
            (None, None) => None,
        };
        insert_merged(&mut packages, name, version, source, checksum, deps)?;
    }
    let orphan_keys: BTreeSet<String> = normalized_ext
        .keys()
        .filter(|k| !attached_ext.contains(*k))
        .chain(
            normalized_checksums
                .keys()
                .filter(|k| !attached_checksums.contains(*k)),
        )
        .cloned()
        .collect();
    for key in orphan_keys {
        let ext = normalized_ext.get(&key);
        let source = ext
            .filter(|e| !e.sources.is_empty())
            .map(|e| e.sources.values().cloned().collect::<Vec<_>>().join(" "));
        let spec_hash = normalized_checksums.get(&key).cloned();
        let ext_pins = ext
            .filter(|e| !e.pins.is_empty())
            .map(|e| e.pins.values().cloned().collect::<Vec<_>>().join(", "));
        let checksum = match (spec_hash, ext_pins) {
            (Some(h), Some(pins)) => Some(format!("{h} ({pins})")),
            (Some(h), None) => Some(h),
            (None, Some(pins)) => Some(pins),
            (None, None) => None,
        };
        insert_merged(
            &mut packages,
            format!("external:{key}"),
            "external".to_owned(),
            source,
            checksum,
            BTreeSet::new(),
        )?;
    }
    for (plugin, version) in plugins {
        insert_merged(
            &mut packages,
            format!("plugin:{plugin}"),
            version,
            None,
            None,
            BTreeSet::new(),
        )?;
    }
    Some(packages)
}

/// The locked packages of a .NET NuGet `packages.lock.json`.
fn nuget_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let root: serde_json::Value = serde_json::from_str(text).ok()?;
    root.get("version")?.as_u64()?;
    let tfms = root.get("dependencies")?.as_object()?;
    let mut packages = BTreeMap::new();
    for tfm_val in tfms.values() {
        let tfm_map = tfm_val.as_object()?;
        for (name, pkg_val) in tfm_map {
            let pkg = pkg_val.as_object()?;
            let pkg_type = pkg.get("type")?.as_str()?;
            let (version, source) = match pkg
                .get("resolved")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
            {
                Some(ver) => (
                    ver.to_owned(),
                    (pkg_type == "Project").then(|| "project".to_owned()),
                ),
                None if pkg_type == "Project" => ("project".to_owned(), Some("project".to_owned())),
                None => return None,
            };
            let checksum = pkg
                .get("contentHash")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let mut deps = BTreeSet::new();
            if let Some(dep_map) = pkg
                .get("dependencies")
                .and_then(serde_json::Value::as_object)
            {
                deps.extend(dep_map.keys().cloned());
            }
            insert_merged(&mut packages, name.clone(), version, source, checksum, deps)?;
        }
    }
    Some(packages)
}

/// The locked packages of a Python Pipenv `Pipfile.lock`.
fn pipfile_packages(text: &str) -> Option<BTreeMap<Key, Package>> {
    if text.is_empty() {
        return Some(BTreeMap::new());
    }
    let root: serde_json::Value = serde_json::from_str(text).ok()?;
    let root_obj = root.as_object()?;
    for key in root_obj.keys() {
        if !matches!(key.as_str(), "_meta" | "default" | "develop") {
            return None;
        }
    }
    let meta = root_obj.get("_meta")?.as_object()?;
    for key in meta.keys() {
        if !matches!(
            key.as_str(),
            "pipfile-spec" | "hash" | "requires" | "sources"
        ) {
            return None;
        }
    }
    meta.get("pipfile-spec")?.as_u64()?;
    if let Some(hash_val) = meta.get("hash") {
        hash_val.as_object()?;
    }
    let default_map = root_obj.get("default")?.as_object()?;
    let mut packages = BTreeMap::new();
    let mut sources_map: BTreeMap<&str, String> = BTreeMap::new();
    let mut default_source: Option<String> = None;
    if let Some(sources_val) = meta.get("sources") {
        let sources = sources_val.as_array()?;
        for src in sources {
            let obj = src.as_object()?;
            for key in obj.keys() {
                if !matches!(key.as_str(), "name" | "url" | "verify_ssl") {
                    return None;
                }
            }
            let name = obj.get("name")?.as_str()?;
            let url = obj.get("url")?.as_str()?;
            if name.is_empty() || url.is_empty() {
                return None;
            }
            let verify_ssl = match obj.get("verify_ssl") {
                Some(v) => v.as_bool()?,
                None => true,
            };
            let encoded = if verify_ssl {
                url.to_owned()
            } else {
                format!("{url} (verify_ssl=false)")
            };
            if let Some(prev) = sources_map.get(name)
                && prev != &encoded
            {
                return None;
            }
            if default_source.is_none() {
                default_source = Some(encoded.clone());
            }
            sources_map.insert(name, encoded);
            insert_merged(
                &mut packages,
                format!("source:{name}"),
                url.to_owned(),
                Some(format!("verify_ssl={verify_ssl}")),
                None,
                BTreeSet::new(),
            )?;
        }
    }
    if let Some(req_val) = meta.get("requires") {
        let req_obj = req_val.as_object()?;
        for (req_key, req_ver) in req_obj {
            let ver = req_ver.as_str()?;
            if req_key.is_empty() || ver.is_empty() {
                return None;
            }
            insert_merged(
                &mut packages,
                format!("requires:{req_key}"),
                ver.to_owned(),
                None,
                None,
                BTreeSet::new(),
            )?;
        }
    }
    let develop_map = match root_obj.get("develop") {
        Some(val) => Some(val.as_object()?),
        None => None,
    };
    fn opt_str<'a>(
        pkg: &'a serde_json::Map<String, serde_json::Value>,
        key: &str,
    ) -> Option<Option<&'a str>> {
        match pkg.get(key) {
            Some(v) => {
                let s = v.as_str()?;
                Some((!s.is_empty()).then_some(s))
            }
            None => Some(None),
        }
    }
    for (scope, map) in [("default", Some(default_map)), ("develop", develop_map)] {
        let Some(map) = map else {
            continue;
        };
        for (name, pkg_val) in map {
            let pkg = pkg_val.as_object()?;
            for key in pkg.keys() {
                if !matches!(
                    key.as_str(),
                    "version"
                        | "hashes"
                        | "index"
                        | "git"
                        | "hg"
                        | "svn"
                        | "bzr"
                        | "uri"
                        | "file"
                        | "path"
                        | "ref"
                        | "subdirectory"
                        | "editable"
                        | "markers"
                        | "extras"
                ) {
                    return None;
                }
            }
            let git_ref = opt_str(pkg, "ref")?;
            let index = opt_str(pkg, "index")?;
            let git = opt_str(pkg, "git")?;
            let hg = opt_str(pkg, "hg")?;
            let svn = opt_str(pkg, "svn")?;
            let bzr = opt_str(pkg, "bzr")?;
            let uri = opt_str(pkg, "uri")?;
            let file = opt_str(pkg, "file")?;
            let path = opt_str(pkg, "path")?;
            let subdirectory = opt_str(pkg, "subdirectory")?;
            let markers = opt_str(pkg, "markers")?;
            let editable = match pkg.get("editable") {
                Some(v) => Some(v.as_bool()?),
                None => None,
            };
            let version_raw = opt_str(pkg, "version")?;
            let version = if let Some(ver) = version_raw {
                let v = ver.strip_prefix("==").unwrap_or(ver).trim();
                if v.is_empty() {
                    return None;
                }
                v.to_owned()
            } else if let Some(r) = git_ref {
                short_rev(r).to_owned()
            } else if path.is_some() || file.is_some() {
                "local".to_owned()
            } else if uri.is_some() {
                "uri".to_owned()
            } else if git.is_some() || hg.is_some() || svn.is_some() || bzr.is_some() {
                "vcs".to_owned()
            } else {
                return None;
            };

            let mut src_parts: Vec<String> = Vec::new();
            if let Some(idx) = index {
                src_parts.push(
                    sources_map
                        .get(idx)
                        .cloned()
                        .unwrap_or_else(|| format!("index:{idx}")),
                );
            }
            if let Some(url) = git {
                src_parts.push(format!("git:{url}"));
            }
            if let Some(url) = hg {
                src_parts.push(format!("hg:{url}"));
            }
            if let Some(url) = svn {
                src_parts.push(format!("svn:{url}"));
            }
            if let Some(url) = bzr {
                src_parts.push(format!("bzr:{url}"));
            }
            if let Some(u) = uri {
                src_parts.push(format!("uri:{u}"));
            }
            if let Some(f) = file {
                src_parts.push(format!("file:{f}"));
            }
            if let Some(p) = path {
                src_parts.push(format!("path:{p}"));
            }
            // Attach default `_meta.sources[0]` to registry packages lacking explicit `"index"`.
            if src_parts.is_empty()
                && version_raw.is_some()
                && let Some(def) = &default_source
            {
                src_parts.push(def.clone());
            }
            if let Some(sub) = subdirectory {
                src_parts.push(format!("subdir:{sub}"));
            }
            if editable == Some(true) {
                src_parts.push("editable=true".to_owned());
            }
            if let Some(extras_val) = pkg.get("extras") {
                let arr = extras_val.as_array()?;
                let mut extras: BTreeSet<&str> = BTreeSet::new();
                for item in arr {
                    let s = item.as_str()?;
                    if !s.is_empty() {
                        extras.insert(s);
                    }
                }
                if !extras.is_empty() {
                    src_parts.push(format!(
                        "extras={}",
                        extras.into_iter().collect::<Vec<_>>().join(",")
                    ));
                }
            }
            if let Some(m) = markers {
                src_parts.push(format!("markers={m}"));
            }
            let source = if src_parts.is_empty() {
                Some(format!("[{scope}]"))
            } else {
                Some(format!("{} [{scope}]", src_parts.join(" ")))
            };

            let mut checksum_parts: Vec<String> = Vec::new();
            if let Some(val) = pkg.get("hashes") {
                let arr = val.as_array()?;
                let mut hashes: BTreeSet<&str> = BTreeSet::new();
                for item in arr {
                    let s = item.as_str()?;
                    if !s.is_empty() {
                        hashes.insert(s);
                    }
                }
                if !hashes.is_empty() {
                    checksum_parts.push(hashes.into_iter().collect::<Vec<_>>().join(", "));
                }
            }
            if let Some(r) = git_ref {
                checksum_parts.push(if checksum_parts.is_empty() {
                    r.to_owned()
                } else {
                    format!("ref:{r}")
                });
            }
            let checksum = (!checksum_parts.is_empty()).then(|| checksum_parts.join(" ; "));
            insert_merged(
                &mut packages,
                name.clone(),
                version,
                source,
                checksum,
                BTreeSet::new(),
            )?;
        }
    }
    Some(packages)
}

fn parse_format(format: LockFormat, text: &str) -> Option<BTreeMap<Key, Package>> {
    match format {
        LockFormat::Cargo => packages(text),
        LockFormat::Npm => npm_packages(text),
        LockFormat::Yarn => yarn_packages(text),
        LockFormat::Pnpm => pnpm_packages(text),
        LockFormat::Bun => bun_packages(text),
        LockFormat::Uv => uv_packages(text),
        LockFormat::GoSum => gosum_packages(text),
        LockFormat::Poetry => poetry_packages(text),
        LockFormat::Gemfile => gemfile_packages(text),
        LockFormat::Composer => composer_packages(text),
        LockFormat::Flake => flake_packages(text),
        LockFormat::SwiftResolved => swift_resolved_packages(text),
        LockFormat::Mix => mix_packages(text),
        LockFormat::Pubspec => pubspec_packages(text),
        LockFormat::Gradle => gradle_packages(text),
        LockFormat::Podfile => podfile_packages(text),
        LockFormat::NuGet => nuget_packages(text),
        LockFormat::Pipfile => pipfile_packages(text),
    }
}

/// Split raw lockfile hunk lines into per-hunk `(old_lines, new_lines)` streams,
/// returning `None` if there are no actual `+`/`-` change lines.
fn split_hunk_sides<'a>(hunk_lines: &[&'a str]) -> Option<Vec<(Vec<&'a str>, Vec<&'a str>)>> {
    let mut hunks = Vec::new();
    let mut old = Vec::new();
    let mut new = Vec::new();
    let mut saw_change = false;
    for &line in hunk_lines {
        if is_hunk_line(line) {
            if !old.is_empty() || !new.is_empty() {
                hunks.push((std::mem::take(&mut old), std::mem::take(&mut new)));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix(' ') {
            old.push(rest);
            new.push(rest);
        } else if let Some(rest) = line.strip_prefix('-') {
            saw_change = true;
            old.push(rest);
        } else if let Some(rest) = line.strip_prefix('+') {
            saw_change = true;
            new.push(rest);
        }
    }
    if !old.is_empty() || !new.is_empty() {
        hunks.push((old, new));
    }
    saw_change.then_some(hunks)
}

/// Parse complete TOML `[[package]]` stanzas (including a leading fragment starting
/// at `name = ...`) from one side of a `Cargo.lock` / `uv.lock` / `poetry.lock` hunk.
fn parse_toml_hunk_side(lines: &[&str], out: &mut BTreeMap<Key, Package>) -> Option<()> {
    #[derive(Default)]
    struct Stanza {
        name: Option<String>,
        version: Option<String>,
        source: Option<String>,
        hashes: BTreeSet<String>,
        in_source_table: bool,
        source_pairs: BTreeMap<String, String>,
    }
    let flush = |stanza: Stanza, out: &mut BTreeMap<Key, Package>| -> Option<()> {
        let (Some(name), Some(version)) = (stanza.name, stanza.version) else {
            return Some(());
        };
        let source = stanza.source.or_else(|| {
            (!stanza.source_pairs.is_empty()).then(|| {
                stanza
                    .source_pairs
                    .into_iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
        });
        let checksum = (!stanza.hashes.is_empty())
            .then(|| stanza.hashes.into_iter().collect::<Vec<_>>().join(", "));
        insert_merged(out, name, version, source, checksum, BTreeSet::new())
    };
    let mut cur = Stanza::default();
    for &raw in lines {
        let trimmed = raw.trim();
        if trimmed == "[[package]]" {
            flush(std::mem::take(&mut cur), out)?;
            continue;
        }
        if trimmed == "[package.source]" {
            cur.in_source_table = true;
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            cur.in_source_table = false;
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            let key = k.trim();
            let val = v.trim();
            if cur.in_source_table {
                let s = unquote_lock_scalar(val);
                if !s.is_empty() {
                    cur.source_pairs.insert(key.to_owned(), s.to_owned());
                }
                continue;
            }
            match key {
                "name" => cur.name = Some(unquote_lock_scalar(val).to_owned()),
                "version" => cur.version = Some(unquote_lock_scalar(val).to_owned()),
                "source" => {
                    let s = unquote_lock_scalar(val);
                    if !s.is_empty() {
                        cur.source = Some(s.to_owned());
                    }
                }
                "checksum" => {
                    let s = unquote_lock_scalar(val);
                    if !s.is_empty() {
                        cur.hashes.insert(s.to_owned());
                    }
                }
                _ => {}
            }
        }
        // Extract inline `hash = "sha256:..."` entries on `uv.lock` / `poetry.lock` artifact lines.
        if let Some(pos) = trimmed.find("hash = \"") {
            let after = &trimmed[pos + 8..];
            if let Some(end) = after.find('"') {
                let hash = &after[..end];
                if !hash.is_empty() {
                    cur.hashes.insert(hash.to_owned());
                }
            }
        }
    }
    flush(cur, out)
}

/// Extract brace-balanced JSON objects from one side of a hunk for JSON lockfiles
/// (`package-lock.json`, `composer.lock`, `flake.lock`, `Package.resolved`).
fn extract_json_hunk_entries(
    format: LockFormat,
    lines: &[&str],
    out: &mut BTreeMap<Key, Package>,
) -> Option<()> {
    match format {
        LockFormat::Npm => {
            let mut key_path: Option<String> = None;
            let mut buf = String::new();
            let mut depth = 0usize;
            for &line in lines {
                let trimmed = line.trim();
                if depth == 0 {
                    if let Some((k, rest)) = trimmed.split_once(':')
                        && rest.trim().starts_with('{')
                    {
                        let k = unquote_lock_scalar(k);
                        if k.starts_with("node_modules/") || k.contains("/node_modules/") {
                            key_path = Some(k.to_owned());
                            buf.clear();
                            buf.push_str(rest.trim());
                            depth = 1;
                            if buf.ends_with("},") || buf.ends_with('}') {
                                depth = 0;
                            }
                        }
                    }
                } else {
                    buf.push('\n');
                    buf.push_str(trimmed);
                    for ch in trimmed.chars() {
                        match ch {
                            '{' => depth += 1,
                            '}' => depth = depth.saturating_sub(1),
                            _ => {}
                        }
                    }
                }
                if depth == 0
                    && let Some(path) = key_path.take()
                {
                    let obj_str = buf.trim_end_matches(',');
                    let wrapped = format!("{{\"packages\":{{\"{path}\":{obj_str}}}}}");
                    if let Some(parsed) = npm_packages(&wrapped) {
                        for (_, pkg) in parsed {
                            insert_merged(
                                out,
                                pkg.name,
                                pkg.version,
                                pkg.source,
                                pkg.checksum,
                                pkg.dependencies.into_iter().collect(),
                            )?;
                        }
                    }
                }
            }
            Some(())
        }
        LockFormat::Composer | LockFormat::SwiftResolved => {
            let mut objects = Vec::new();
            let mut buf = String::new();
            let mut depth = 0usize;
            for &line in lines {
                let trimmed = line.trim();
                if depth == 0 {
                    if trimmed == "{" {
                        buf.clear();
                        buf.push('{');
                        depth = 1;
                    }
                } else {
                    buf.push('\n');
                    buf.push_str(trimmed);
                    for ch in trimmed.chars() {
                        match ch {
                            '{' => depth += 1,
                            '}' => depth = depth.saturating_sub(1),
                            _ => {}
                        }
                    }
                    if depth == 0 {
                        objects.push(buf.trim_end_matches(',').to_owned());
                        buf.clear();
                    }
                }
            }
            if objects.is_empty() {
                return Some(());
            }
            let wrapped = match format {
                LockFormat::Composer => format!("{{\"packages\":[{}]}}", objects.join(",")),
                LockFormat::SwiftResolved => {
                    format!("{{\"version\":2,\"pins\":[{}]}}", objects.join(","))
                }
                _ => unreachable!(),
            };
            if let Some(parsed) = parse_format(format, &wrapped) {
                for (_, pkg) in parsed {
                    insert_merged(
                        out,
                        pkg.name,
                        pkg.version,
                        pkg.source,
                        pkg.checksum,
                        pkg.dependencies.into_iter().collect(),
                    )?;
                }
            }
            Some(())
        }
        LockFormat::Flake => {
            let mut nodes = Vec::new();
            let mut key: Option<String> = None;
            let mut buf = String::new();
            let mut depth = 0usize;
            for &line in lines {
                let trimmed = line.trim();
                if depth == 0 {
                    if let Some((k, rest)) = trimmed.split_once(':')
                        && rest.trim() == "{"
                    {
                        let name = unquote_lock_scalar(k);
                        if !name.is_empty()
                            && !matches!(name, "nodes" | "locked" | "original" | "inputs")
                        {
                            key = Some(name.to_owned());
                            buf.clear();
                            buf.push('{');
                            depth = 1;
                        }
                    }
                } else {
                    buf.push('\n');
                    buf.push_str(trimmed);
                    for ch in trimmed.chars() {
                        match ch {
                            '{' => depth += 1,
                            '}' => depth = depth.saturating_sub(1),
                            _ => {}
                        }
                    }
                    if depth == 0
                        && let Some(name) = key.take()
                    {
                        let obj = buf.trim_end_matches(',');
                        nodes.push(format!("\"{name}\":{obj}"));
                        buf.clear();
                    }
                }
            }
            if nodes.is_empty() {
                return Some(());
            }
            let wrapped = format!(
                "{{\"root\":\"root\",\"version\":7,\"nodes\":{{{}}}}}",
                nodes.join(",")
            );
            if let Some(parsed) = flake_packages(&wrapped) {
                for (_, pkg) in parsed {
                    insert_merged(
                        out,
                        pkg.name,
                        pkg.version,
                        pkg.source,
                        pkg.checksum,
                        pkg.dependencies.into_iter().collect(),
                    )?;
                }
            }
            Some(())
        }
        _ => Some(()),
    }
}

fn json_brace_delta(line: &str) -> isize {
    let mut delta = 0isize;
    let mut in_string = false;
    let mut escaped = false;
    for b in line.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => delta += 1,
            b'}' => delta -= 1,
            _ => {}
        }
    }
    delta
}

fn split_raw_hunks<'a>(hunk_lines: &[&'a str]) -> Option<Vec<Vec<(char, &'a str)>>> {
    let mut hunks = Vec::new();
    let mut cur = Vec::new();
    let mut saw_change = false;
    for &line in hunk_lines {
        if is_hunk_line(line) {
            if !cur.is_empty() {
                hunks.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix(' ') {
            cur.push((' ', rest));
        } else if let Some(rest) = line.strip_prefix('-') {
            saw_change = true;
            cur.push(('-', rest));
        } else if let Some(rest) = line.strip_prefix('+') {
            saw_change = true;
            cur.push(('+', rest));
        }
    }
    if !cur.is_empty() {
        hunks.push(cur);
    }
    saw_change.then_some(hunks)
}

fn merge_parsed(parsed: BTreeMap<Key, Package>, out: &mut BTreeMap<Key, Package>) -> Option<()> {
    for (_, pkg) in parsed {
        insert_merged(
            out,
            pkg.name,
            pkg.version,
            pkg.source,
            pkg.checksum,
            pkg.dependencies.into_iter().collect(),
        )?;
    }
    Some(())
}

fn compare_pubspec_hunks(
    hunk_lines: &[&str],
    old_pkgs: &mut BTreeMap<Key, Package>,
    new_pkgs: &mut BTreeMap<Key, Package>,
) -> Option<()> {
    let hunks = split_raw_hunks(hunk_lines)?;
    let flush_block = |old_lines: &mut Vec<&str>,
                       new_lines: &mut Vec<&str>,
                       changed: &mut bool,
                       old_pkgs: &mut BTreeMap<Key, Package>,
                       new_pkgs: &mut BTreeMap<Key, Package>|
     -> Option<()> {
        let was_changed = *changed;
        *changed = false;
        let old_blk = std::mem::take(old_lines);
        let new_blk = std::mem::take(new_lines);
        if !was_changed {
            return Some(());
        }
        if !old_blk.is_empty() {
            let snippet = format!("packages:\n{}\n", old_blk.join("\n"));
            merge_parsed(pubspec_packages(&snippet)?, old_pkgs)?;
        }
        if !new_blk.is_empty() {
            let snippet = format!("packages:\n{}\n", new_blk.join("\n"));
            merge_parsed(pubspec_packages(&snippet)?, new_pkgs)?;
        }
        Some(())
    };
    for hunk in hunks {
        let mut in_block = false;
        let mut in_sdks = false;
        let mut blk_old: Vec<&str> = Vec::new();
        let mut blk_new: Vec<&str> = Vec::new();
        let mut blk_changed = false;
        for (kind, rest) in hunk {
            let line = rest.strip_suffix('\r').unwrap_or(rest);
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let indent = line.bytes().take_while(|&b| b == b' ').count();
            if indent == 0 {
                flush_block(
                    &mut blk_old,
                    &mut blk_new,
                    &mut blk_changed,
                    old_pkgs,
                    new_pkgs,
                )?;
                in_block = false;
                match trimmed {
                    "packages:" | "packages: {}" => in_sdks = false,
                    "sdks:" => {
                        if kind != ' ' {
                            return None;
                        }
                        in_sdks = true;
                    }
                    "sdks: {}" => {
                        if kind != ' ' {
                            return None;
                        }
                        in_sdks = false;
                    }
                    _ if kind != ' ' => return None,
                    _ => {}
                }
                continue;
            }
            if in_sdks {
                if kind != ' ' {
                    return None;
                }
                continue;
            }
            if indent == 2 {
                flush_block(
                    &mut blk_old,
                    &mut blk_new,
                    &mut blk_changed,
                    old_pkgs,
                    new_pkgs,
                )?;
                in_block = true;
                if kind != '+' {
                    blk_old.push(line);
                }
                if kind != '-' {
                    blk_new.push(line);
                }
                if kind != ' ' {
                    blk_changed = true;
                }
            } else {
                if !in_block {
                    if kind != ' ' {
                        return None;
                    }
                    continue;
                }
                if kind != '+' {
                    blk_old.push(line);
                }
                if kind != '-' {
                    blk_new.push(line);
                }
                if kind != ' ' {
                    blk_changed = true;
                }
            }
        }
        flush_block(
            &mut blk_old,
            &mut blk_new,
            &mut blk_changed,
            old_pkgs,
            new_pkgs,
        )?;
    }
    Some(())
}

fn compare_podfile_hunks(
    hunk_lines: &[&str],
    old_pkgs: &mut BTreeMap<Key, Package>,
    new_pkgs: &mut BTreeMap<Key, Package>,
) -> Option<()> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum HunkSection {
        Unknown,
        Pods,
        SpecChecksums,
        Ignored,
        FailClosed,
    }
    let hunks = split_raw_hunks(hunk_lines)?;
    let mut old_checksums: BTreeMap<String, String> = BTreeMap::new();
    let mut new_checksums: BTreeMap<String, String> = BTreeMap::new();
    let flush_pod = |pod: Option<(String, String, BTreeSet<String>)>,
                     out: &mut BTreeMap<Key, Package>| {
        if let Some((name, version, deps)) = pod {
            insert_merged(out, name, version, None, None, deps)?;
        }
        Some(())
    };
    for hunk in hunks {
        let mut section = HunkSection::Unknown;
        let mut old_cur: Option<(String, String, BTreeSet<String>)> = None;
        let mut new_cur: Option<(String, String, BTreeSet<String>)> = None;
        for (kind, rest) in hunk {
            let line = rest.strip_suffix('\r').unwrap_or(rest);
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let indent = line.bytes().take_while(|&b| b == b' ').count();
            if indent == 0 {
                flush_pod(old_cur.take(), old_pkgs)?;
                flush_pod(new_cur.take(), new_pkgs)?;
                section = match trimmed {
                    "PODS:" => HunkSection::Pods,
                    "SPEC CHECKSUMS:" => HunkSection::SpecChecksums,
                    "DEPENDENCIES:" => HunkSection::Ignored,
                    "SPEC REPOS:" | "EXTERNAL SOURCES:" | "CHECKOUT OPTIONS:"
                    | "COCOAPODS PLUGINS:" => {
                        if kind != ' ' {
                            return None;
                        }
                        HunkSection::FailClosed
                    }
                    _ if trimmed.starts_with("COCOAPODS PLUGINS:") => {
                        if kind != ' ' {
                            return None;
                        }
                        HunkSection::FailClosed
                    }
                    _ if trimmed.starts_with("PODFILE CHECKSUM:")
                        || trimmed.starts_with("COCOAPODS:") =>
                    {
                        HunkSection::Ignored
                    }
                    _ => {
                        if kind != ' ' {
                            return None;
                        }
                        HunkSection::Unknown
                    }
                };
                continue;
            }
            match section {
                HunkSection::Ignored => continue,
                HunkSection::FailClosed => {
                    if kind != ' ' {
                        return None;
                    }
                    continue;
                }
                HunkSection::Pods | HunkSection::SpecChecksums | HunkSection::Unknown => {}
            }
            if indent == 2 {
                if section != HunkSection::SpecChecksums
                    && let Some((name, version)) = parse_pod_spec(trimmed)
                {
                    if kind != '+' {
                        flush_pod(old_cur.take(), old_pkgs)?;
                        old_cur = Some((name.clone(), version.clone(), BTreeSet::new()));
                    }
                    if kind != '-' {
                        flush_pod(new_cur.take(), new_pkgs)?;
                        new_cur = Some((name, version, BTreeSet::new()));
                    }
                    continue;
                }
                if section != HunkSection::Pods
                    && let Some((k, v)) = trimmed.split_once(':')
                {
                    let pod = unquote_lock_scalar(k);
                    let hash = unquote_lock_scalar(v);
                    if !pod.is_empty()
                        && hash.len() == 40
                        && hash.bytes().all(|b| b.is_ascii_hexdigit())
                    {
                        if kind != '+' {
                            old_checksums.insert(pod.to_owned(), hash.to_owned());
                        }
                        if kind != '-' {
                            new_checksums.insert(pod.to_owned(), hash.to_owned());
                        }
                        continue;
                    }
                }
                if kind != ' ' {
                    return None;
                }
            } else if indent == 4
                && section != HunkSection::SpecChecksums
                && trimmed.starts_with("- ")
            {
                let Some(dep) = parse_pod_dep_name(trimmed) else {
                    if kind != ' ' {
                        return None;
                    }
                    continue;
                };
                if kind != '+' {
                    if let Some((_, _, deps)) = old_cur.as_mut() {
                        deps.insert(dep.to_owned());
                    } else if kind == '-' {
                        return None;
                    }
                }
                if kind != '-' {
                    if let Some((_, _, deps)) = new_cur.as_mut() {
                        deps.insert(dep.to_owned());
                    } else if kind == '+' {
                        return None;
                    }
                }
            } else if kind != ' ' {
                return None;
            }
        }
        flush_pod(old_cur.take(), old_pkgs)?;
        flush_pod(new_cur.take(), new_pkgs)?;
    }
    let attach = |checksums: &BTreeMap<String, String>, pkgs: &mut BTreeMap<Key, Package>| {
        for (pod, hash) in checksums {
            for pkg in pkgs.values_mut() {
                if pkg.name == *pod {
                    pkg.checksum = Some(hash.clone());
                }
            }
        }
    };
    attach(&old_checksums, old_pkgs);
    attach(&new_checksums, new_pkgs);
    // Fail closed if a changed SPEC CHECKSUMS entry belongs to a pod absent from PODS hunks.
    for pod in old_checksums.keys().chain(new_checksums.keys()) {
        if old_checksums.get(pod) != new_checksums.get(pod)
            && !old_pkgs.values().any(|p| p.name == *pod)
            && !new_pkgs.values().any(|p| p.name == *pod)
        {
            return None;
        }
    }
    Some(())
}

fn compare_json_map_hunks(
    format: LockFormat,
    hunk_lines: &[&str],
    old_pkgs: &mut BTreeMap<Key, Package>,
    new_pkgs: &mut BTreeMap<Key, Package>,
) -> Option<()> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PipfileSection {
        Unknown,
        Meta(isize),
        Default,
        Develop,
    }
    let hunks = split_raw_hunks(hunk_lines)?;
    let parse_side = |side_char: char, out: &mut BTreeMap<Key, Package>| -> Option<()> {
        let mut entries: Vec<(Option<&'static str>, String)> = Vec::new();
        for hunk in &hunks {
            let side: Vec<(bool, &str)> = hunk
                .iter()
                .filter_map(|&(kind, rest)| {
                    (kind == ' ' || kind == side_char).then_some((kind == side_char, rest))
                })
                .collect();
            let mut pipfile_section = PipfileSection::Unknown;
            let mut in_meta_hash = false;
            let mut key: Option<String> = None;
            let mut key_section: Option<&'static str> = None;
            let mut buf = String::new();
            let mut depth = 0isize;
            let mut obj_changed = false;
            for (idx, &(changed, raw)) in side.iter().enumerate() {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if depth == 0 {
                    if format == LockFormat::Pipfile
                        && let PipfileSection::Meta(meta_depth) = pipfile_section
                    {
                        let delta = json_brace_delta(trimmed);
                        if meta_depth == 1 && !changed && trimmed == "\"hash\": {" {
                            in_meta_hash = true;
                        }
                        if in_meta_hash && meta_depth >= 2 && delta > 0 {
                            return None;
                        }
                        if changed {
                            if !in_meta_hash || meta_depth != 2 || delta != 0 {
                                return None;
                            }
                            let obj: serde_json::Map<String, serde_json::Value> =
                                serde_json::from_str(&format!(
                                    "{{{}}}",
                                    trimmed.strip_suffix(',').unwrap_or(trimmed)
                                ))
                                .ok()?;
                            if obj.len() != 1
                                || !obj.get("sha256").is_some_and(serde_json::Value::is_string)
                            {
                                return None;
                            }
                        }
                        let next_depth = meta_depth + delta;
                        if next_depth <= 0 {
                            pipfile_section = PipfileSection::Unknown;
                            in_meta_hash = false;
                        } else {
                            if next_depth <= 1 {
                                in_meta_hash = false;
                            }
                            pipfile_section = PipfileSection::Meta(next_depth);
                        }
                        continue;
                    }
                    if let Some((k, rest)) = trimmed.split_once(':') {
                        let rest_t = rest.trim();
                        if rest_t.starts_with('{') {
                            let name = unquote_lock_scalar(k);
                            if format == LockFormat::Pipfile {
                                match name {
                                    "_meta" => {
                                        if pipfile_section != PipfileSection::Unknown || changed {
                                            return None;
                                        }
                                        if rest_t == "{" {
                                            pipfile_section = PipfileSection::Meta(1);
                                            in_meta_hash = false;
                                            continue;
                                        }
                                        if rest_t.strip_suffix(',').unwrap_or(rest_t) == "{}" {
                                            continue;
                                        }
                                        return None;
                                    }
                                    "default" | "develop" => {
                                        if pipfile_section != PipfileSection::Unknown {
                                            return None;
                                        }
                                        if rest_t == "{" {
                                            pipfile_section = if name == "default" {
                                                PipfileSection::Default
                                            } else {
                                                PipfileSection::Develop
                                            };
                                            continue;
                                        }
                                        if rest_t.strip_suffix(',').unwrap_or(rest_t) == "{}" {
                                            continue;
                                        }
                                        return None;
                                    }
                                    "hash" | "requires" | "" => return None,
                                    _ => {}
                                }
                            }
                            let pkg_section = match format {
                                LockFormat::NuGet => {
                                    let is_pkg = !name.is_empty()
                                        && name != "dependencies"
                                        && (rest_t.contains("\"type\"")
                                            || side[idx + 1..]
                                                .iter()
                                                .find(|(_, l)| !l.trim().is_empty())
                                                .is_some_and(|(_, l)| {
                                                    l.trim().starts_with("\"type\"")
                                                }));
                                    is_pkg.then_some(None)
                                }
                                LockFormat::Pipfile => match pipfile_section {
                                    PipfileSection::Default => Some(Some("default")),
                                    PipfileSection::Develop => Some(Some("develop")),
                                    // Ambiguous Pipfile.lock section context: fail closed.
                                    PipfileSection::Unknown | PipfileSection::Meta(_) => {
                                        return None;
                                    }
                                },
                                _ => None,
                            };
                            if let Some(sec) = pkg_section {
                                key = Some(name.to_owned());
                                key_section = sec;
                                buf.clear();
                                buf.push_str(rest_t);
                                obj_changed = changed;
                                depth = json_brace_delta(rest_t);
                                if depth <= 0 {
                                    depth = 0;
                                    if let Some(name) = key.take()
                                        && obj_changed
                                    {
                                        entries.push((
                                            key_section.take(),
                                            format!("\"{name}\":{}", buf.trim_end_matches(',')),
                                        ));
                                    } else {
                                        key_section = None;
                                    }
                                    buf.clear();
                                    obj_changed = false;
                                }
                                continue;
                            }
                        }
                    }
                    if format == LockFormat::Pipfile
                        && matches!(
                            pipfile_section,
                            PipfileSection::Default | PipfileSection::Develop
                        )
                        && matches!(trimmed, "}" | "},")
                    {
                        pipfile_section = PipfileSection::Unknown;
                    }
                    if changed {
                        let ok_outside = matches!(trimmed, "{" | "}" | "}," | "]" | "]," | ",");
                        if !ok_outside {
                            return None;
                        }
                    }
                } else {
                    buf.push('\n');
                    buf.push_str(trimmed);
                    let delta = json_brace_delta(trimmed);
                    let closing_brace_only = matches!(trimmed, "}" | "},") && depth + delta <= 0;
                    if !closing_brace_only {
                        obj_changed |= changed;
                    }
                    depth += delta;
                    if depth <= 0 {
                        depth = 0;
                        if let Some(name) = key.take()
                            && obj_changed
                        {
                            entries.push((
                                key_section.take(),
                                format!("\"{name}\":{}", buf.trim_end_matches(',')),
                            ));
                        } else {
                            key_section = None;
                        }
                        buf.clear();
                        obj_changed = false;
                    }
                }
            }
            if depth > 0 && obj_changed {
                return None;
            }
        }
        if entries.is_empty() {
            return Some(());
        }
        for (sec, entry) in &entries {
            let wrapped = match format {
                LockFormat::NuGet => {
                    format!("{{\"version\":1,\"dependencies\":{{\"hunk\":{{{entry}}}}}}}")
                }
                LockFormat::Pipfile => match *sec {
                    Some("default") => {
                        format!("{{\"_meta\":{{\"pipfile-spec\":6}},\"default\":{{{entry}}}}}")
                    }
                    Some("develop") => {
                        format!(
                            "{{\"_meta\":{{\"pipfile-spec\":6}},\"default\":{{}},\"develop\":{{{entry}}}}}"
                        )
                    }
                    _ => return None,
                },
                _ => unreachable!(),
            };
            let parsed = parse_format(format, &wrapped)?;
            merge_parsed(parsed, out)?;
        }
        Some(())
    };
    parse_side('-', old_pkgs)?;
    parse_side('+', new_pkgs)?;
    Some(())
}

/// Fallback hunk-based dependency diff when git blobs are absent locally (such as
/// remote `gh pr diff` / `glab mr diff` patches). Conservative: returns `None` if
/// changed lines cannot be confidently attributed to parsed packages.
fn compare_hunks(format: LockFormat, hunk_lines: &[&str]) -> Option<Changes> {
    let hunks = split_hunk_sides(hunk_lines)?;
    let mut old_pkgs: BTreeMap<Key, Package> = BTreeMap::new();
    let mut new_pkgs: BTreeMap<Key, Package> = BTreeMap::new();
    match format {
        LockFormat::GoSum | LockFormat::Gradle => {
            let mut old_text = String::new();
            let mut new_text = String::new();
            let skip_comment = format == LockFormat::Gradle;
            for (old, new) in &hunks {
                for line in old {
                    let trimmed = line.trim();
                    if skip_comment && (trimmed.is_empty() || trimmed.starts_with('#')) {
                        continue;
                    }
                    old_text.push_str(line);
                    old_text.push('\n');
                }
                for line in new {
                    let trimmed = line.trim();
                    if skip_comment && (trimmed.is_empty() || trimmed.starts_with('#')) {
                        continue;
                    }
                    new_text.push_str(line);
                    new_text.push('\n');
                }
            }
            old_pkgs = parse_format(format, &old_text)?;
            new_pkgs = parse_format(format, &new_text)?;
        }
        LockFormat::Mix => {
            for &line in hunk_lines {
                if let Some(rest) = line.strip_prefix('+').or_else(|| line.strip_prefix('-')) {
                    let trimmed = rest.trim();
                    if trimmed.is_empty()
                        || trimmed.starts_with('#')
                        || matches!(trimmed, "%{" | "}")
                    {
                        continue;
                    }
                    if !(trimmed.starts_with('"')
                        && (trimmed.ends_with("},") || trimmed.ends_with('}')))
                    {
                        return None;
                    }
                }
            }
            let build = |sides: Vec<&str>| -> Option<BTreeMap<Key, Package>> {
                let mut entries = String::from("%{\n");
                for line in sides {
                    let trimmed = line.trim();
                    if trimmed.starts_with('"')
                        && (trimmed.ends_with("},") || trimmed.ends_with('}'))
                    {
                        entries.push_str(trimmed);
                        if !trimmed.ends_with(',') {
                            entries.push(',');
                        }
                        entries.push('\n');
                    }
                }
                entries.push_str("}\n");
                mix_packages(&entries)
            };
            let old_all: Vec<&str> = hunks.iter().flat_map(|(o, _)| o.iter().copied()).collect();
            let new_all: Vec<&str> = hunks.iter().flat_map(|(_, n)| n.iter().copied()).collect();
            old_pkgs = build(old_all)?;
            new_pkgs = build(new_all)?;
        }
        LockFormat::Pubspec => {
            compare_pubspec_hunks(hunk_lines, &mut old_pkgs, &mut new_pkgs)?;
        }
        LockFormat::Podfile => {
            compare_podfile_hunks(hunk_lines, &mut old_pkgs, &mut new_pkgs)?;
        }
        LockFormat::NuGet | LockFormat::Pipfile => {
            compare_json_map_hunks(format, hunk_lines, &mut old_pkgs, &mut new_pkgs)?;
        }
        LockFormat::Bun => {
            let build = |sides: Vec<&str>| -> Option<BTreeMap<Key, Package>> {
                let mut entries = String::from("{\"lockfileVersion\":1,\"packages\":{\n");
                for line in sides {
                    let trimmed = line.trim();
                    if trimmed.starts_with('"')
                        && trimmed.contains(": [")
                        && (trimmed.ends_with("],") || trimmed.ends_with(']'))
                    {
                        entries.push_str(trimmed);
                        if !trimmed.ends_with(',') {
                            entries.push(',');
                        }
                        entries.push('\n');
                    }
                }
                entries.push_str("}}\n");
                bun_packages(&entries)
            };
            let old_all: Vec<&str> = hunks.iter().flat_map(|(o, _)| o.iter().copied()).collect();
            let new_all: Vec<&str> = hunks.iter().flat_map(|(_, n)| n.iter().copied()).collect();
            old_pkgs = build(old_all)?;
            new_pkgs = build(new_all)?;
        }
        LockFormat::Cargo | LockFormat::Uv | LockFormat::Poetry => {
            for (old, new) in &hunks {
                parse_toml_hunk_side(old, &mut old_pkgs)?;
                parse_toml_hunk_side(new, &mut new_pkgs)?;
            }
        }
        LockFormat::Gemfile => {
            let parse_side = |lines: &[&str], out: &mut BTreeMap<Key, Package>| -> Option<()> {
                let mut checksums: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
                for &line in lines {
                    let trimmed = line.trim();
                    let indent = line.bytes().take_while(|&b| b == b' ').count();
                    if indent == 4
                        && let Some((name, version)) = parse_gem_spec(trimmed)
                    {
                        insert_merged(out, name, version, None, None, BTreeSet::new())?;
                    } else if indent == 2
                        && let Some((spec_part, rest)) = trimmed.split_once(") ")
                    {
                        let with_paren = format!("{spec_part})");
                        if let Some((name, version)) = parse_gem_spec(&with_paren)
                            && rest.trim().starts_with("sha")
                        {
                            checksums
                                .entry((name, version))
                                .or_default()
                                .insert(rest.trim().to_owned());
                        }
                    }
                }
                for ((name, version), hashes) in checksums {
                    let joined = hashes.into_iter().collect::<Vec<_>>().join(", ");
                    let mut matched = false;
                    for pkg in out.values_mut() {
                        if pkg.name == name && pkg.version == version {
                            pkg.checksum = Some(joined.clone());
                            matched = true;
                        }
                    }
                    if !matched {
                        insert_merged(out, name, version, None, Some(joined), BTreeSet::new())?;
                    }
                }
                Some(())
            };
            for (old, new) in &hunks {
                parse_side(old, &mut old_pkgs)?;
                parse_side(new, &mut new_pkgs)?;
            }
        }
        LockFormat::Yarn => {
            let parse_side = |lines: &[&str], out: &mut BTreeMap<Key, Package>| -> Option<()> {
                let start = lines.iter().position(|l| {
                    !l.trim().is_empty() && !l.starts_with(' ') && l.trim_end().ends_with(':')
                });
                let Some(start) = start else {
                    return Some(());
                };
                let snippet = lines[start..].join("\n");
                if let Some(parsed) = yarn_packages(&snippet) {
                    for (_, pkg) in parsed {
                        insert_merged(
                            out,
                            pkg.name,
                            pkg.version,
                            pkg.source,
                            pkg.checksum,
                            pkg.dependencies.into_iter().collect(),
                        )?;
                    }
                }
                Some(())
            };
            for (old, new) in &hunks {
                parse_side(old, &mut old_pkgs)?;
                parse_side(new, &mut new_pkgs)?;
            }
        }
        LockFormat::Pnpm => {
            let parse_side = |lines: &[&str], out: &mut BTreeMap<Key, Package>| -> Option<()> {
                let start = lines.iter().position(|l| {
                    l.bytes().take_while(|&b| b == b' ').count() == 2
                        && parse_pnpm_key(l.trim()).is_some()
                });
                let Some(start) = start else {
                    return Some(());
                };
                let snippet = format!(
                    "lockfileVersion: '9.0'\npackages:\n{}\n",
                    lines[start..].join("\n")
                );
                if let Some(parsed) = pnpm_packages(&snippet) {
                    for (_, pkg) in parsed {
                        insert_merged(
                            out,
                            pkg.name,
                            pkg.version,
                            pkg.source,
                            pkg.checksum,
                            pkg.dependencies.into_iter().collect(),
                        )?;
                    }
                }
                Some(())
            };
            for (old, new) in &hunks {
                parse_side(old, &mut old_pkgs)?;
                parse_side(new, &mut new_pkgs)?;
            }
        }
        LockFormat::Npm | LockFormat::Composer | LockFormat::Flake | LockFormat::SwiftResolved => {
            for (old, new) in &hunks {
                extract_json_hunk_entries(format, old, &mut old_pkgs)?;
                extract_json_hunk_entries(format, new, &mut new_pkgs)?;
            }
        }
    }
    if old_pkgs.is_empty() && new_pkgs.is_empty() {
        return None;
    }
    let changes = compare_packages(&old_pkgs, &new_pkgs)?;
    let has_any = !changes.added.is_empty()
        || !changes.removed.is_empty()
        || !changes.updated.is_empty()
        || !changes.links.is_empty()
        || !changes.flags.is_empty();
    has_any.then_some(changes)
}

#[cfg(test)]
fn compare(old: &str, new: &str) -> Option<Changes> {
    compare_format(LockFormat::Cargo, old, new)
}

fn compare_format(format: LockFormat, old: &str, new: &str) -> Option<Changes> {
    let old = parse_format(format, old)?;
    let new = parse_format(format, new)?;
    compare_packages(&old, &new)
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

fn compare_packages(old: &BTreeMap<Key, Package>, new: &BTreeMap<Key, Package>) -> Option<Changes> {
    let (before, after) = (versions(old), versions(new));
    let mut changes = Changes::default();
    // Packages whose name, version and source the other revision does not lock, by
    // name and version.
    let mut left: BTreeMap<(&str, &str), Vec<&Package>> = BTreeMap::new();
    let mut right: BTreeMap<(&str, &str), Vec<&Package>> = BTreeMap::new();
    for (key, package) in old {
        match new.get(key) {
            Some(now) => changes.kept(package, now),
            None => left
                .entry((&package.name, &package.version))
                .or_default()
                .push(package),
        }
    }
    for (key, package) in new {
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

/// `patch` with each lockfile section replaced by its description (keeping its
/// `diff --git` line) and each large generated/snapshot section folded to one line.
pub fn view(patch: &str, sections: &[Section]) -> String {
    let mut out = String::with_capacity(patch.len());
    let mut lines = patch.split_inclusive('\n').enumerate().peekable();
    while let Some((index, line)) = lines.next() {
        let number = index + 1;
        if let Some(section) = sections.iter().find(|s| s.first == number) {
            if !section.generated {
                out.push_str(line);
            }
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

/// A patch summary whose lockfile sections are replaced by their dependency
/// description when shorter and whose large generated/snapshot sections fold to
/// one line, with supply-chain flags (within `budget` bytes) right after the
/// heading. Tries the compact diff tier first when it fits within `budget * 4`,
/// otherwise emits the file-grouped hunk index.
pub fn summarize(summary: &Summary, sections: &[Section], budget: usize) -> String {
    let flags = flag_lines(sections, budget);
    let max_bytes = budget.saturating_mul(4).max(budget);
    summary.render_diff_with(max_bytes, &flags, |path, first, last, default_len| {
        let section = sections
            .iter()
            .find(|s| s.path == path && s.first <= last && s.last >= first)?;
        let replacement = format!("{}{}", section.head, section.body);
        if section.generated || default_len.is_none_or(|len| replacement.len() < len) {
            Some(replacement)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summary::Item;

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
            generated: false,
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
            generated: false,
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
    fn only_patch_producing_commands_apply() {
        for (words, applies_to) in [
            (&["git", "diff", "HEAD~1"][..], true),
            (&["git", "--no-pager", "show"][..], true),
            (&["git", "-P", "diff"][..], true),
            (&["git", "--no-optional-locks", "show"][..], true),
            (&["git", "-c", "color.ui=never", "diff"][..], true),
            (&["git", "log", "-p"][..], true),
            (&["git", "log", "--patch", "-3"][..], true),
            (&["git", "log", "-U1", "HEAD~2..HEAD"][..], true),
            (&["gh", "pr", "diff", "42"][..], true),
            (&["glab", "mr", "diff", "17"][..], true),
            (&["git", "log", "--oneline"][..], false),
            (&["git", "log", "--", "-p"][..], false),
            (&["git", "-C", "elsewhere", "diff"][..], false),
            (&["git", "show", "HEAD:Cargo.lock"][..], false),
            (&["gh", "pr", "view", "42"][..], false),
            (&["glab", "mr", "view", "17"][..], false),
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
            generated: false,
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

    #[test]
    fn npm_uv_and_gosum_lockfiles_describe_changes_and_flag_substitutions() {
        let npm_old = r#"{
          "name": "app",
          "lockfileVersion": 3,
          "packages": {
            "": { "name": "app", "version": "1.0.0" },
            "node_modules/react": {
              "version": "18.2.0",
              "resolved": "https://registry.npmjs.org/react/-/react-18.2.0.tgz",
              "integrity": "sha512-oldreact",
              "dependencies": { "loose-envify": "^1.1.0" }
            },
            "node_modules/loose-envify": {
              "version": "1.4.0",
              "resolved": "https://registry.npmjs.org/loose-envify/-/loose-envify-1.4.0.tgz",
              "integrity": "sha512-aaa"
            },
            "node_modules/legacy": {
              "version": "0.1.0",
              "resolved": "https://registry.npmjs.org/legacy/-/legacy-0.1.0.tgz",
              "integrity": "sha512-old"
            }
          }
        }"#;
        let npm_new = r#"{
          "name": "app",
          "lockfileVersion": 3,
          "packages": {
            "": { "name": "app", "version": "1.0.0" },
            "node_modules/react": {
              "version": "18.3.1",
              "resolved": "https://registry.npmjs.org/react/-/react-18.3.1.tgz",
              "integrity": "sha512-newreact",
              "dependencies": { "loose-envify": "^1.1.0" }
            },
            "node_modules/loose-envify": {
              "version": "1.4.0",
              "resolved": "https://registry.npmjs.org/loose-envify/-/loose-envify-1.4.0.tgz",
              "integrity": "sha512-tampered"
            },
            "node_modules/@types/node": {
              "version": "22.0.0",
              "resolved": "https://registry.npmjs.org/@types/node/-/node-22.0.0.tgz",
              "integrity": "sha512-types"
            }
          }
        }"#;
        let npm = compare_format(LockFormat::Npm, npm_old, npm_new).unwrap();
        let (head, body) = npm.render("package-lock.json", 1, 200);
        assert!(head.contains("1 added, 1 removed, 1 updated"), "{head}");
        assert!(head.contains("1 source or checksum change(s)"), "{head}");
        assert!(body.contains("added: @types/node 22.0.0"), "{body}");
        assert!(body.contains("removed: legacy 0.1.0"), "{body}");
        assert!(body.contains("updated: react 18.2.0 -> 18.3.1"), "{body}");
        assert_eq!(
            npm.flags,
            [
                "checksum changed on existing version loose-envify 1.4.0: sha512-aaa -> sha512-tampered"
            ]
        );

        let uv_old = concat!(
            "version = 1\n",
            "[[package]]\nname = \"httpx\"\nversion = \"0.27.0\"\n",
            "source = { registry = \"https://pypi.org/simple\" }\n",
            "dependencies = [{ name = \"anyio\" }]\n",
            "wheels = [{ url = \"u1\", hash = \"sha256:111\" }]\n",
            "[[package]]\nname = \"anyio\"\nversion = \"4.4.0\"\n",
            "source = { registry = \"https://pypi.org/simple\" }\n",
            "wheels = [{ url = \"u2\", hash = \"sha256:222\" }]\n",
        );
        let uv_new = concat!(
            "version = 1\n",
            "[[package]]\nname = \"httpx\"\nversion = \"0.28.1\"\n",
            "source = { registry = \"https://pypi.org/simple\" }\n",
            "dependencies = [{ name = \"anyio\" }, { name = \"idna\" }]\n",
            "wheels = [{ url = \"u3\", hash = \"sha256:333\" }]\n",
            "[[package]]\nname = \"anyio\"\nversion = \"4.4.0\"\n",
            "source = { registry = \"https://evil.example/simple\" }\n",
            "wheels = [{ url = \"u2\", hash = \"sha256:222\" }]\n",
            "[[package]]\nname = \"idna\"\nversion = \"3.10\"\n",
            "source = { registry = \"https://pypi.org/simple\" }\n",
        );
        let uv = compare_format(LockFormat::Uv, uv_old, uv_new).unwrap();
        let (head, body) = uv.render("uv.lock", 1, 150);
        assert!(
            head.contains("1 added, 1 updated, 1 with changed dependencies"),
            "{head}"
        );
        assert!(body.contains("added: idna 3.10"), "{body}");
        assert!(body.contains("updated: httpx 0.27.0 -> 0.28.1"), "{body}");
        assert!(body.contains("dependencies: httpx +idna"), "{body}");
        assert!(
            uv.flags[0].contains("source changed on existing version anyio 4.4.0"),
            "{:?}",
            uv.flags
        );

        let go_old = concat!(
            "github.com/pkg/errors v0.9.1 h1:oldzip=\n",
            "github.com/pkg/errors v0.9.1/go.mod h1:mod1=\n",
            "golang.org/x/sys v0.20.0 h1:sysold=\n",
        );
        let go_new = concat!(
            "github.com/pkg/errors v0.9.1 h1:newzip=\n",
            "github.com/pkg/errors v0.9.1/go.mod h1:mod1=\n",
            "golang.org/x/sys v0.25.0 h1:sysnew=\n",
            "golang.org/x/text v0.18.0 h1:txt=\n",
        );
        let gosum = compare_format(LockFormat::GoSum, go_old, go_new).unwrap();
        let (head, body) = gosum.render("go.sum", 1, 90);
        assert!(head.contains("1 added, 1 updated"), "{head}");
        assert!(body.contains("added: golang.org/x/text v0.18.0"), "{body}");
        assert!(
            body.contains("updated: golang.org/x/sys v0.20.0 -> v0.25.0"),
            "{body}"
        );
        assert!(
            gosum.flags[0]
                .contains("checksum changed on existing version github.com/pkg/errors v0.9.1"),
            "{:?}",
            gosum.flags
        );
        assert!(compare_format(LockFormat::GoSum, "not a go.sum line", go_new).is_none());
    }

    #[test]
    fn yarn_pnpm_and_bun_lockfiles_describe_changes_and_flag_substitutions() {
        // Yarn Classic v1
        let yarn_v1_old = concat!(
            "# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n",
            "# yarn lockfile v1\n\n",
            "\"lodash@^4.17.20\", lodash@^4.17.21:\n",
            "  version \"4.17.21\"\n",
            "  resolved \"https://registry.yarnpkg.com/lodash/-/lodash-4.17.21.tgz#aaa\"\n",
            "  integrity sha512-oldlodash\n\n",
            "react@^18.2.0:\n",
            "  version \"18.2.0\"\n",
            "  resolved \"https://registry.yarnpkg.com/react/-/react-18.2.0.tgz#bbb\"\n",
            "  integrity sha512-react182\n",
            "  dependencies:\n",
            "    loose-envify \"^1.1.0\"\n\n",
            "old-pkg@^0.1.0:\n",
            "  version \"0.1.0\"\n",
            "  resolved \"https://registry.yarnpkg.com/old-pkg/-/old-pkg-0.1.0.tgz\"\n",
            "  integrity sha512-oldpkg\n",
        );
        let yarn_v1_new = concat!(
            "# yarn lockfile v1\n\n",
            "\"lodash@^4.17.20\", lodash@^4.17.21:\n",
            "  version \"4.17.21\"\n",
            "  resolved \"https://registry.yarnpkg.com/lodash/-/lodash-4.17.21.tgz#aaa\"\n",
            "  integrity sha512-tampered\n\n",
            "react@^18.3.0:\n",
            "  version \"18.3.1\"\n",
            "  resolved \"https://registry.yarnpkg.com/react/-/react-18.3.1.tgz#ccc\"\n",
            "  integrity sha512-react183\n",
            "  dependencies:\n",
            "    loose-envify \"^1.1.0\"\n",
            "    scheduler \"^0.23.0\"\n\n",
            "\"@types/node@^22.0.0\":\n",
            "  version \"22.0.0\"\n",
            "  resolved \"https://registry.yarnpkg.com/@types/node/-/node-22.0.0.tgz\"\n",
            "  integrity sha512-types\n",
        );
        let y1 = compare_format(LockFormat::Yarn, yarn_v1_old, yarn_v1_new).unwrap();
        let (head, body) = y1.render("yarn.lock", 1, 200);
        assert!(
            head.contains("1 added, 1 removed, 1 updated, 1 with changed dependencies"),
            "{head}"
        );
        assert!(body.contains("added: @types/node 22.0.0"), "{body}");
        assert!(body.contains("removed: old-pkg 0.1.0"), "{body}");
        assert!(body.contains("updated: react 18.2.0 -> 18.3.1"), "{body}");
        assert!(body.contains("dependencies: react +scheduler"), "{body}");
        assert_eq!(
            y1.flags,
            [
                "checksum changed on existing version lodash 4.17.21: sha512-oldlodash -> sha512-tampered"
            ]
        );
        assert!(compare_format(LockFormat::Yarn, "not a yarn stanza", yarn_v1_new).is_none());

        // Yarn Berry v2+ YAML
        let berry_old = concat!(
            "__metadata:\n",
            "  version: 8\n",
            "  cacheKey: 10c0\n\n",
            "\"@scope/pkg@npm:^1.0.0\":\n",
            "  version: 1.0.0\n",
            "  resolution: \"@scope/pkg@npm:1.0.0\"\n",
            "  checksum: 10c0/aaa\n",
            "  dependencies:\n",
            "    dep-a: \"npm:^1.0.0\"\n",
        );
        let berry_new = concat!(
            "__metadata:\n",
            "  version: 8\n",
            "  cacheKey: 10c0\n\n",
            "\"@scope/pkg@npm:^1.0.0\":\n",
            "  version: 1.0.0\n",
            "  resolution: \"@scope/pkg@npm:1.0.0\"\n",
            "  checksum: 10c0/bbb\n",
            "  dependencies:\n",
            "    dep-a: \"npm:^1.0.0\"\n",
            "    dep-b: \"npm:^2.0.0\"\n",
        );
        let yb = compare_format(LockFormat::Yarn, berry_old, berry_new).unwrap();
        assert_eq!(
            yb.flags,
            ["checksum changed on existing version @scope/pkg 1.0.0: 10c0/aaa -> 10c0/bbb"]
        );
        assert_eq!(yb.links, ["@scope/pkg +dep-b"]);

        // pnpm-lock.yaml v9 (`packages:` + `snapshots:`)
        let pnpm_old = concat!(
            "lockfileVersion: '9.0'\n\n",
            "packages:\n",
            "  zod@3.23.0:\n",
            "    resolution: {integrity: sha512-zodold}\n",
            "  '@scope/core@1.2.0':\n",
            "    resolution: {integrity: sha512-core1}\n\n",
            "snapshots:\n",
            "  zod@3.23.0: {}\n",
            "  '@scope/core@1.2.0':\n",
            "    dependencies:\n",
            "      zod: 3.23.0\n",
        );
        let pnpm_new = concat!(
            "lockfileVersion: '9.0'\n\n",
            "packages:\n",
            "  zod@3.23.8:\n",
            "    resolution: {integrity: sha512-zodnew}\n",
            "  '@scope/core@1.2.0':\n",
            "    resolution: {integrity: sha512-coretampered}\n",
            "  ms@2.1.3:\n",
            "    resolution: {integrity: sha512-ms}\n\n",
            "snapshots:\n",
            "  zod@3.23.8: {}\n",
            "  '@scope/core@1.2.0':\n",
            "    dependencies:\n",
            "      zod: 3.23.8\n",
            "      ms: 2.1.3\n",
        );
        let pnpm = compare_format(LockFormat::Pnpm, pnpm_old, pnpm_new).unwrap();
        let (head, body) = pnpm.render("pnpm-lock.yaml", 1, 180);
        assert!(
            head.contains("1 added, 1 updated, 1 with changed dependencies"),
            "{head}"
        );
        assert!(body.contains("added: ms 2.1.3"), "{body}");
        assert!(body.contains("updated: zod 3.23.0 -> 3.23.8"), "{body}");
        assert!(body.contains("dependencies: @scope/core +ms"), "{body}");
        assert_eq!(
            pnpm.flags,
            [
                "checksum changed on existing version @scope/core 1.2.0: sha512-core1 -> sha512-coretampered"
            ]
        );
        assert!(compare_format(LockFormat::Pnpm, "random: yaml\n", pnpm_new).is_none());

        // bun.lock (JSONC with trailing commas)
        let bun_old = r#"{
          "lockfileVersion": 1,
          "packages": {
            "app": ["app@workspace:.", {}],
            "hono": ["hono@4.6.0", "", { "dependencies": { "a": "^1.0.0" } }, "sha512-hono1"],
            "a": ["a@1.0.0", "", {}, "sha512-a1"],
          },
        }"#;
        let bun_new = r#"{
          "lockfileVersion": 1,
          "packages": {
            "app": ["app@workspace:.", {}],
            "hono": ["hono@4.7.0", "", { "dependencies": { "a": "^1.0.0", "b": "^2.0.0" } }, "sha512-hono2"],
            "a": ["a@1.0.0", "", {}, "sha512-a-tampered"],
            "b": ["b@2.0.0", "", {}, "sha512-b1"],
          },
        }"#;
        let bun = compare_format(LockFormat::Bun, bun_old, bun_new).unwrap();
        let (head, body) = bun.render("bun.lock", 1, 120);
        assert!(
            head.contains("1 added, 1 updated, 1 with changed dependencies"),
            "{head}"
        );
        assert!(body.contains("added: b 2.0.0"), "{body}");
        assert!(body.contains("updated: hono 4.6.0 -> 4.7.0"), "{body}");
        assert!(body.contains("dependencies: hono +b"), "{body}");
        assert_eq!(
            bun.flags,
            ["checksum changed on existing version a 1.0.0: sha512-a1 -> sha512-a-tampered"]
        );
    }

    #[test]
    fn poetry_gemfile_composer_flake_swift_and_mix_lockfiles_describe_changes_and_flag_substitutions()
     {
        // 1. poetry.lock
        let poetry_old = concat!(
            "[[package]]\nname = \"httpx\"\nversion = \"0.27.0\"\n",
            "files = [{file = \"httpx-0.27.0.whl\", hash = \"sha256:aaa\"}]\n",
            "[package.dependencies]\nanyio = \"*\"\n\n",
            "[[package]]\nname = \"anyio\"\nversion = \"4.4.0\"\n",
            "files = [{file = \"anyio-4.4.0.whl\", hash = \"sha256:bbb\"}]\n",
            "[package.source]\ntype = \"legacy\"\nurl = \"https://pypi.org/simple\"\n\n",
            "[[package]]\nname = \"legacy\"\nversion = \"0.1.0\"\n",
            "[metadata]\nlock-version = \"2.0\"\n",
        );
        let poetry_new = concat!(
            "[[package]]\nname = \"httpx\"\nversion = \"0.28.0\"\n",
            "files = [{file = \"httpx-0.28.0.whl\", hash = \"sha256:ccc\"}]\n",
            "[package.dependencies]\nanyio = \"*\"\nidna = \"*\"\n\n",
            "[[package]]\nname = \"anyio\"\nversion = \"4.4.0\"\n",
            "files = [{file = \"anyio-4.4.0.whl\", hash = \"sha256:tampered\"}]\n",
            "[package.source]\ntype = \"legacy\"\nurl = \"https://evil.example/simple\"\n\n",
            "[[package]]\nname = \"idna\"\nversion = \"3.10\"\n",
            "[metadata]\nlock-version = \"2.0\"\n",
        );
        let poetry = compare_format(LockFormat::Poetry, poetry_old, poetry_new).unwrap();
        let (head, body) = poetry.render("poetry.lock", 1, 120);
        assert!(
            head.contains("1 added, 1 removed, 1 updated, 1 with changed dependencies, 2 source or checksum change(s)"),
            "{head}"
        );
        assert!(body.contains("added: idna 3.10"), "{body}");
        assert!(body.contains("removed: legacy 0.1.0"), "{body}");
        assert!(body.contains("updated: httpx 0.27.0 -> 0.28.0"), "{body}");
        assert!(body.contains("dependencies: httpx +idna"), "{body}");
        assert!(
            poetry
                .flags
                .iter()
                .any(|f| f.contains("source changed on existing version anyio 4.4.0")),
            "{:?}",
            poetry.flags
        );
        assert!(
            poetry
                .flags
                .iter()
                .any(|f| f.contains("checksum changed on existing version anyio 4.4.0")),
            "{:?}",
            poetry.flags
        );
        assert!(compare_format(LockFormat::Poetry, "not = [toml", poetry_new).is_none());

        // 2. Gemfile.lock
        let gem_old = concat!(
            "GIT\n  remote: https://github.com/org/mygem.git\n  revision: aaaa1111\n  specs:\n    mygem (1.0.0)\n\n",
            "GEM\n  remote: https://rubygems.org/\n  specs:\n",
            "    rack (3.0.10)\n",
            "    rails (7.1.0)\n      rack (>= 3.0)\n",
            "    old_gem (0.9.0)\n\n",
            "CHECKSUMS\n  rack (3.0.10) sha256=oldrack\n\n",
            "BUNDLED WITH\n   2.5.10\n",
        );
        let gem_new = concat!(
            "GIT\n  remote: https://github.com/org/mygem.git\n  revision: bbbb2222\n  specs:\n    mygem (1.0.0)\n\n",
            "GEM\n  remote: https://rubygems.org/\n  specs:\n",
            "    rack (3.0.10)\n",
            "    rails (7.2.0)\n      rack (>= 3.0)\n      zeitwerk (~> 2.6)\n",
            "    zeitwerk (2.6.18)\n\n",
            "CHECKSUMS\n  rack (3.0.10) sha256=tamperedrack\n\n",
            "BUNDLED WITH\n   2.5.10\n",
        );
        let gem = compare_format(LockFormat::Gemfile, gem_old, gem_new).unwrap();
        let (head, body) = gem.render("Gemfile.lock", 1, 90);
        assert!(
            head.contains("1 added, 1 removed, 1 updated, 1 with changed dependencies, 2 source or checksum change(s)"),
            "{head}"
        );
        assert!(body.contains("added: zeitwerk 2.6.18"), "{body}");
        assert!(body.contains("removed: old_gem 0.9.0"), "{body}");
        assert!(body.contains("updated: rails 7.1.0 -> 7.2.0"), "{body}");
        assert!(body.contains("dependencies: rails +zeitwerk"), "{body}");
        assert!(
            gem.flags
                .iter()
                .any(|f| f.contains("source changed on existing version mygem 1.0.0")),
            "{:?}",
            gem.flags
        );
        assert!(
            gem.flags
                .iter()
                .any(|f| f.contains("checksum changed on existing version rack 3.0.10")),
            "{:?}",
            gem.flags
        );
        assert!(compare_format(LockFormat::Gemfile, "random text\n", gem_new).is_none());

        // 3. composer.lock
        let composer_old = r#"{
          "content-hash": "111",
          "packages": [
            {
              "name": "symfony/console",
              "version": "v6.4.8",
              "source": { "type": "git", "url": "https://github.com/symfony/console.git", "reference": "ref1" },
              "dist": { "type": "zip", "url": "https://api.github.com/repos/symfony/console/zipball/ref1", "reference": "ref1", "shasum": "sha1" },
              "require": { "php": ">=8.1", "symfony/string": "^6.0" }
            },
            {
              "name": "symfony/string",
              "version": "v6.4.8",
              "dist": { "type": "zip", "url": "https://dist.example/string.zip", "reference": "refa", "shasum": "goodsha" }
            }
          ],
          "packages-dev": [
            { "name": "phpunit/phpunit", "version": "10.5.0" }
          ]
        }"#;
        let composer_new = r#"{
          "content-hash": "222",
          "packages": [
            {
              "name": "symfony/console",
              "version": "v6.4.12",
              "source": { "type": "git", "url": "https://github.com/symfony/console.git", "reference": "ref2" },
              "dist": { "type": "zip", "url": "https://api.github.com/repos/symfony/console/zipball/ref2", "reference": "ref2", "shasum": "sha2" },
              "require": { "php": ">=8.1", "symfony/string": "^6.0", "psr/log": "^3.0" }
            },
            {
              "name": "symfony/string",
              "version": "v6.4.8",
              "dist": { "type": "zip", "url": "https://dist.example/string.zip", "reference": "refa", "shasum": "evilsha" }
            },
            { "name": "psr/log", "version": "3.0.2" }
          ],
          "packages-dev": []
        }"#;
        let comp = compare_format(LockFormat::Composer, composer_old, composer_new).unwrap();
        let (head, body) = comp.render("composer.lock", 1, 200);
        assert!(
            head.contains("1 added, 1 removed, 1 updated, 1 with changed dependencies, 1 source or checksum change(s)"),
            "{head}"
        );
        assert!(body.contains("added: psr/log 3.0.2"), "{body}");
        assert!(body.contains("removed: phpunit/phpunit 10.5.0"), "{body}");
        assert!(
            body.contains("updated: symfony/console v6.4.8 -> v6.4.12"),
            "{body}"
        );
        assert!(
            body.contains("dependencies: symfony/console +psr/log"),
            "{body}"
        );
        assert!(
            comp.flags[0].contains("checksum changed on existing version symfony/string v6.4.8"),
            "{:?}",
            comp.flags
        );

        // 4. flake.lock
        let flake_old = r#"{
          "nodes": {
            "nixpkgs": {
              "locked": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "111111111111aaaa", "narHash": "sha256-old=" }
            },
            "flake-utils": {
              "locked": { "type": "github", "owner": "numtide", "repo": "flake-utils", "rev": "222222222222bbbb", "narHash": "sha256-fu1=" }
            },
            "root": { "inputs": { "nixpkgs": "nixpkgs", "flake-utils": "flake-utils" } }
          },
          "root": "root",
          "version": 7
        }"#;
        let flake_new = r#"{
          "nodes": {
            "nixpkgs": {
              "locked": { "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "333333333333cccc", "narHash": "sha256-new=" }
            },
            "flake-utils": {
              "locked": { "type": "github", "owner": "numtide", "repo": "flake-utils", "rev": "222222222222bbbb", "narHash": "sha256-tampered=" }
            },
            "systems": {
              "locked": { "type": "github", "owner": "nix-systems", "repo": "default", "rev": "444444444444dddd", "narHash": "sha256-sys=" }
            },
            "root": { "inputs": { "nixpkgs": "nixpkgs", "flake-utils": "flake-utils", "systems": "systems" } }
          },
          "root": "root",
          "version": 7
        }"#;
        let flake = compare_format(LockFormat::Flake, flake_old, flake_new).unwrap();
        let (head, body) = flake.render("flake.lock", 1, 80);
        assert!(
            head.contains("1 added, 1 updated, 1 source or checksum change(s)"),
            "{head}"
        );
        assert!(body.contains("added: systems 444444444444"), "{body}");
        assert!(
            body.contains("updated: nixpkgs 111111111111 -> 333333333333"),
            "{body}"
        );
        assert!(
            flake.flags[0]
                .contains("checksum changed on existing version flake-utils 222222222222"),
            "{:?}",
            flake.flags
        );

        // 5. Package.resolved (v2 and v1)
        let swift_old = r#"{
          "pins": [
            {
              "identity": "swift-argument-parser",
              "kind": "remoteSourceControl",
              "location": "https://github.com/apple/swift-argument-parser.git",
              "state": { "revision": "aaaa1111", "version": "1.4.0" }
            },
            {
              "identity": "swift-log",
              "kind": "remoteSourceControl",
              "location": "https://github.com/apple/swift-log.git",
              "state": { "revision": "bbbb1111", "version": "1.5.4" }
            }
          ],
          "version": 2
        }"#;
        let swift_new = r#"{
          "pins": [
            {
              "identity": "swift-argument-parser",
              "kind": "remoteSourceControl",
              "location": "https://github.com/apple/swift-argument-parser.git",
              "state": { "revision": "aaaa2222", "version": "1.5.0" }
            },
            {
              "identity": "swift-log",
              "kind": "remoteSourceControl",
              "location": "https://github.com/apple/swift-log.git",
              "state": { "revision": "evil9999", "version": "1.5.4" }
            },
            {
              "identity": "swift-nio",
              "kind": "remoteSourceControl",
              "location": "https://github.com/apple/swift-nio.git",
              "state": { "revision": "cccc1111", "version": "2.70.0" }
            }
          ],
          "version": 2
        }"#;
        let swift = compare_format(LockFormat::SwiftResolved, swift_old, swift_new).unwrap();
        let (head, body) = swift.render("Package.resolved", 1, 90);
        assert!(
            head.contains("1 added, 1 updated, 1 source or checksum change(s)"),
            "{head}"
        );
        assert!(body.contains("added: swift-nio 2.70.0"), "{body}");
        assert!(
            body.contains("updated: swift-argument-parser 1.4.0 -> 1.5.0"),
            "{body}"
        );
        assert!(
            swift.flags[0].contains("checksum changed on existing version swift-log 1.5.4"),
            "{:?}",
            swift.flags
        );
        // Swift v1 format works too
        let swift_v1 = r#"{
          "object": {
            "pins": [
              {
                "package": "swift-log",
                "repositoryURL": "https://evil.example/swift-log.git",
                "state": { "branch": null, "revision": "bbbb1111", "version": "1.5.4" }
              }
            ]
          },
          "version": 1
        }"#;
        let swift1 = compare_format(LockFormat::SwiftResolved, swift_old, swift_v1).unwrap();
        assert!(
            swift1
                .flags
                .iter()
                .any(|f| f.contains("source changed on existing version swift-log 1.5.4")),
            "{:?}",
            swift1.flags
        );

        // 6. mix.lock
        let mix_old = r#"%{
  "jason": {:hex, :jason, "1.4.1", "aaa111", [:mix], [], "hexpm", "aaa222"},
  "plug": {:hex, :plug, "1.16.0", "bbb111", [:mix], [{:mime, "~> 2.0", [hex: :mime, repo: "hexpm", optional: false]}], "hexpm", "bbb222"},
  "old_dep": {:hex, :old_dep, "0.1.0", "ccc111", [:mix], [], "hexpm"},
}"#;
        let mix_new = r#"%{
  "jason": {:hex, :jason, "1.4.1", "aaa111", [:mix], [], "hexpm", "tampered999"},
  "plug": {:hex, :plug, "1.16.1", "bbb333", [:mix], [{:mime, "~> 2.0", [hex: :mime, repo: "hexpm", optional: false]}, {:plug_crypto, "~> 2.0", [hex: :plug_crypto, repo: "hexpm", optional: false]}], "hexpm", "bbb444"},
  "plug_crypto": {:hex, :plug_crypto, "2.1.0", "ddd111", [:mix], [], "hexpm", "ddd222"},
}"#;
        let mix = compare_format(LockFormat::Mix, mix_old, mix_new).unwrap();
        let (head, body) = mix.render("mix.lock", 1, 50);
        assert!(
            head.contains("1 added, 1 removed, 1 updated, 1 with changed dependencies, 1 source or checksum change(s)"),
            "{head}"
        );
        assert!(body.contains("added: plug_crypto 2.1.0"), "{body}");
        assert!(body.contains("removed: old_dep 0.1.0"), "{body}");
        assert!(body.contains("updated: plug 1.16.0 -> 1.16.1"), "{body}");
        assert!(body.contains("dependencies: plug +plug_crypto"), "{body}");
        assert!(
            mix.flags[0].contains("checksum changed on existing version jason 1.4.1"),
            "{:?}",
            mix.flags
        );
        assert!(compare_format(LockFormat::Mix, "not elixir", mix_new).is_none());
    }

    #[test]
    fn remote_hunk_fallback_and_generated_file_folding_work_without_local_blobs() {
        let dir = tempfile::TempDir::new().unwrap();
        // Remote patch with nonexistent blob ids: hunk fallback extracts package
        // updates and supply-chain checksum changes for Cargo.lock and go.sum.
        let mut snap_added = String::new();
        for i in 0..40 {
            snap_added.push_str(&format!("+snapshot line {i}\n"));
        }
        let remote_patch = format!(
            concat!(
                "diff --git a/Cargo.lock b/Cargo.lock\n",
                "index deadbeef..cafebabe 100644\n",
                "--- a/Cargo.lock\n",
                "+++ b/Cargo.lock\n",
                "@@ -10,9 +10,9 @@\n",
                " [[package]]\n",
                " name = \"tokio\"\n",
                "-version = \"1.40.0\"\n",
                "+version = \"1.41.0\"\n",
                " source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
                "-checksum = \"aa\"\n",
                "+checksum = \"bb\"\n",
                " dependencies = [\n",
                "  \"mio\",\n",
                "@@ -40,6 +40,6 @@\n",
                " [[package]]\n",
                " name = \"mio\"\n",
                " version = \"1.0.0\"\n",
                " source = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
                "-checksum = \"111\"\n",
                "+checksum = \"tampered\"\n",
                "diff --git a/go.sum b/go.sum\n",
                "index 1234567..7654321 100644\n",
                "--- a/go.sum\n",
                "+++ b/go.sum\n",
                "@@ -1,2 +1,3 @@\n",
                " github.com/pkg/errors v0.9.1/go.mod h1:mod1=\n",
                "-github.com/pkg/errors v0.9.1 h1:oldzip=\n",
                "+github.com/pkg/errors v0.9.1 h1:evilzip=\n",
                "+golang.org/x/text v0.18.0 h1:txt=\n",
                "diff --git a/tests/ui.snap b/tests/ui.snap\n",
                "index 1111111..2222222 100644\n",
                "--- a/tests/ui.snap\n",
                "+++ b/tests/ui.snap\n",
                "@@ -1,2 +1,42 @@\n",
                " header\n",
                "{}",
                "diff --git a/tests/tiny.snap b/tests/tiny.snap\n",
                "index 3333333..4444444 100644\n",
                "--- a/tests/tiny.snap\n",
                "+++ b/tests/tiny.snap\n",
                "@@ -1,2 +1,2 @@\n",
                "-old\n",
                "+new\n",
            ),
            snap_added
        );
        let found = sections(&remote_patch, dir.path());
        // Cargo.lock, go.sum, and the 40-line ui.snap are found; tiny.snap (<= 25 lines) is not folded.
        assert_eq!(
            found.len(),
            3,
            "{}",
            found
                .iter()
                .map(|s| s.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert_eq!(found[0].path, "Cargo.lock");
        assert!(
            found[0].body.contains("updated: tokio 1.40.0 -> 1.41.0"),
            "{}",
            found[0].text()
        );
        assert!(
            found[0]
                .flags
                .iter()
                .any(|f| f
                    .contains("checksum changed on existing version mio 1.0.0: 111 -> tampered")),
            "{:?}",
            found[0].flags
        );
        assert_eq!(found[1].path, "go.sum");
        assert!(
            found[1].body.contains("added: golang.org/x/text v0.18.0"),
            "{}",
            found[1].text()
        );
        assert!(
            found[1]
                .flags
                .iter()
                .any(|f| f
                    .contains("checksum changed on existing version github.com/pkg/errors v0.9.1")),
            "{:?}",
            found[1].flags
        );
        assert_eq!(found[2].path, "tests/ui.snap");
        assert!(found[2].generated);
        assert!(
            found[2]
                .head
                .starts_with("=== tests/ui.snap (generated, +40 -0) [stdout:"),
            "{}",
            found[2].head
        );

        // Generated-path matcher covers all documented patterns and ignores ordinary code & plain SVGs.
        for path in [
            "a/b.snap",
            "tests/out.snap.new",
            "dist/app.min.js",
            "dist/site.min.css",
            "proto/api.pb.go",
            "proto/service_grpc.pb.go",
            "proto/api.pb.rs",
            "web/proto/api.pb.ts",
            "web/proto/api.pb.js",
            "src/schema.generated.ts",
            "proto/api_pb2.py",
            "lib/model.g.dart",
            "lib/model.freezed.dart",
            "ui/Form1.Designer.cs",
            "obj/App.g.cs",
            "obj/MainWindow.g.i.cs",
            ".terraform.lock.hcl",
            "infra/prod/.terraform.lock.hcl",
        ] {
            assert!(is_generated_path(path), "{path}");
        }
        for code in [
            "src/lib.rs",
            "src/app.js",
            "src/api.go",
            "tests/test.py",
            "assets/icon.svg",
            "ui/Form1.cs",
            "infra/main.tf",
        ] {
            assert!(!is_generated_path(code), "{code}");
        }
        assert!(is_svg_path("assets/icon.svg"));
        assert!(is_svg_path("assets/LOGO.SVG"));
        assert!(!is_svg_path("assets/icon.png"));
    }

    #[test]
    fn expanded_generated_paths_and_large_svg_threshold_fold_accurately() {
        let dir = tempfile::tempdir().unwrap();
        let mut big_svg_lines = String::new();
        for i in 0..105 {
            big_svg_lines.push_str(&format!("+<path d=\"M{i} 0 L{i} 10\"/>\n"));
        }
        let mut small_svg_lines = String::new();
        for i in 0..40 {
            small_svg_lines.push_str(&format!("+<path d=\"M{i} 0 L{i} 5\"/>\n"));
        }
        let mut tf_lock_lines = String::new();
        for i in 0..30 {
            tf_lock_lines.push_str(&format!("+  \"h1:hash{i:02}=\",\n"));
        }
        let patch = format!(
            concat!(
                "diff --git a/infra/.terraform.lock.hcl b/infra/.terraform.lock.hcl\n",
                "index 111..222 100644\n",
                "--- a/infra/.terraform.lock.hcl\n",
                "+++ b/infra/.terraform.lock.hcl\n",
                "@@ -1,1 +1,31 @@\n",
                " hashes = [\n",
                "{}",
                "diff --git a/assets/icon.svg b/assets/icon.svg\n",
                "index 333..444 100644\n",
                "--- a/assets/icon.svg\n",
                "+++ b/assets/icon.svg\n",
                "@@ -1,1 +1,41 @@\n",
                " <svg>\n",
                "{}",
                "diff --git a/assets/diagram.svg b/assets/diagram.svg\n",
                "index 555..666 100644\n",
                "--- a/assets/diagram.svg\n",
                "+++ b/assets/diagram.svg\n",
                "@@ -1,2 +1,106 @@\n",
                "-<old/>\n",
                "{}",
            ),
            tf_lock_lines, small_svg_lines, big_svg_lines
        );
        let found = sections(&patch, dir.path());
        // `.terraform.lock.hcl` (>25 lines) and `diagram.svg` (+105 -1 = 106 > 100 changed lines) fold;
        // `icon.svg` (+40 -0 <= 100 changed lines) must NOT fold!
        assert_eq!(
            found.len(),
            2,
            "{:?}",
            found.iter().map(|s| &s.path).collect::<Vec<_>>()
        );
        assert_eq!(found[0].path, "infra/.terraform.lock.hcl");
        assert!(found[0].generated);
        assert_eq!(
            found[0].head,
            "=== infra/.terraform.lock.hcl (generated, +30 -0) [stdout:1-36]\n"
        );
        assert_eq!(found[1].path, "assets/diagram.svg");
        assert!(found[1].generated);
        assert_eq!(
            found[1].head,
            "=== assets/diagram.svg (generated, +105 -1) [stdout:83-193]\n"
        );
    }

    #[test]
    fn all_eighteen_lockfile_formats_recognized() {
        let cases = [
            ("Cargo.lock", LockFormat::Cargo),
            ("web/package-lock.json", LockFormat::Npm),
            ("yarn.lock", LockFormat::Yarn),
            ("pnpm-lock.yaml", LockFormat::Pnpm),
            ("bun.lock", LockFormat::Bun),
            ("uv.lock", LockFormat::Uv),
            ("go.sum", LockFormat::GoSum),
            ("poetry.lock", LockFormat::Poetry),
            ("Gemfile.lock", LockFormat::Gemfile),
            ("composer.lock", LockFormat::Composer),
            ("flake.lock", LockFormat::Flake),
            ("ios/Package.resolved", LockFormat::SwiftResolved),
            ("mix.lock", LockFormat::Mix),
            ("app/pubspec.lock", LockFormat::Pubspec),
            ("gradle.lockfile", LockFormat::Gradle),
            ("buildscript-gradle.lockfile", LockFormat::Gradle),
            ("ios/Podfile.lock", LockFormat::Podfile),
            ("src/App/packages.lock.json", LockFormat::NuGet),
            ("Pipfile.lock", LockFormat::Pipfile),
        ];
        let mut distinct = BTreeSet::new();
        for (path, expected) in cases {
            assert_eq!(LockFormat::of_path(path), Some(expected), "{path}");
            assert!(is_lockfile_path(path), "{path}");
            distinct.insert(format!("{expected:?}"));
        }
        assert_eq!(distinct.len(), 18);
        assert!(!is_lockfile_path("src/lib.rs"));
        assert!(!is_lockfile_path("gradle.lockfile.bak"));
    }

    #[test]
    fn pubspec_lock_full_hunk_and_tamper_detection() {
        let old = "\
# Generated by pub
packages:
  async:
    dependency: transitive
    description:
      name: async
      sha256: \"aaa111\"
      url: \"https://pub.dev\"
    source: hosted
    version: \"2.11.0\"
  http:
    dependency: \"direct main\"
    description:
      name: http
      sha256: \"bbb222\"
      url: \"https://pub.dev\"
    source: hosted
    version: \"1.2.1\"
  old_pkg:
    dependency: transitive
    description:
      name: old_pkg
      sha256: \"ccc333\"
      url: \"https://pub.dev\"
    source: hosted
    version: \"0.9.0\"
  flutter:
    dependency: \"direct main\"
    description: flutter
    source: sdk
    version: \"0.0.0\"
sdks:
  dart: \">=3.2.0 <4.0.0\"
";
        let new = "\
# Generated by pub
packages:
  async:
    dependency: transitive
    description:
      name: async
      sha256: \"tampered\"
      url: \"https://pub.dev\"
    source: hosted
    version: \"2.11.0\"
  http:
    dependency: \"direct main\"
    description:
      name: http
      sha256: \"bbb999\"
      url: \"https://pub.dev\"
    source: hosted
    version: \"1.2.2\"
  path:
    dependency: \"direct main\"
    description:
      name: path
      sha256: \"ddd444\"
      url: \"https://pub.dev\"
    source: hosted
    version: \"1.9.0\"
  flutter:
    dependency: \"direct main\"
    description: flutter
    source: sdk
    version: \"0.0.0\"
sdks:
  dart: \">=3.2.0 <4.0.0\"
";
        let changes = compare_format(LockFormat::Pubspec, old, new).unwrap();
        assert_eq!(changes.added, vec!["path 1.9.0"]);
        assert_eq!(changes.removed, vec!["old_pkg 0.9.0"]);
        assert_eq!(changes.updated, vec!["http 1.2.1 -> 1.2.2"]);
        assert!(
            changes.flags.iter().any(|f| f
                .contains("checksum changed on existing version async 2.11.0: aaa111 -> tampered")),
            "{:?}",
            changes.flags
        );
        assert!(pubspec_packages("not_a_pubspec: true\n").is_none());

        // Remote hunk fallback (no local git blobs) + byte-reduction check.
        let dir = tempfile::tempdir().unwrap();
        let mut hunk = String::from(concat!(
            "diff --git a/pubspec.lock b/pubspec.lock\n",
            "index 1111111..2222222 100644\n",
            "--- a/pubspec.lock\n",
            "+++ b/pubspec.lock\n",
            "@@ -2,20 +2,20 @@\n",
            " packages:\n",
        ));
        for i in 0..8 {
            hunk.push_str(&format!(
                concat!(
                    "-  pkg_{i:02}:\n",
                    "-    dependency: transitive\n",
                    "-    description:\n",
                    "-      name: pkg_{i:02}\n",
                    "-      sha256: \"old{i:02}\"\n",
                    "-      url: \"https://pub.dev\"\n",
                    "-    source: hosted\n",
                    "-    version: \"1.{i}.0\"\n",
                    "+  pkg_{i:02}:\n",
                    "+    dependency: transitive\n",
                    "+    description:\n",
                    "+      name: pkg_{i:02}\n",
                    "+      sha256: \"new{i:02}\"\n",
                    "+      url: \"https://pub.dev\"\n",
                    "+    source: hosted\n",
                    "+    version: \"1.{i}.1\"\n",
                ),
                i = i
            ));
        }
        let found = sections(&hunk, dir.path());
        assert_eq!(found.len(), 1);
        let rendered = found[0].text();
        assert!(rendered.contains("8 updated"), "{rendered}");
        assert!(rendered.contains("pkg_00 1.0.0 -> 1.0.1"), "{rendered}");
        assert!(
            rendered.len() * 5 < hunk.len(),
            "raw={} rendered={}\n{rendered}",
            hunk.len(),
            rendered.len()
        );

        // Fail closed when hunk edits `sdks:` or lacks enclosing package header.
        let orphan = "\
diff --git a/pubspec.lock b/pubspec.lock
index 1111111..2222222 100644
--- a/pubspec.lock
+++ b/pubspec.lock
@@ -10,3 +10,3 @@
-      sha256: \"old\"
+      sha256: \"new\"
";
        assert!(sections(orphan, dir.path()).is_empty());
    }

    #[test]
    fn gradle_lockfile_full_hunk_and_variant_names() {
        let old = "\
# This is a Gradle generated file for dependency locking.
com.google.guava:guava:32.1.3-jre=compileClasspath,runtimeClasspath
org.slf4j:slf4j-api:2.0.12=compileClasspath
org.legacy:old-lib:1.0.0=runtimeClasspath
empty=annotationProcessor
";
        let new = "\
# This is a Gradle generated file for dependency locking.
com.google.guava:guava:32.1.3-jre=compileClasspath,runtimeClasspath,testCompileClasspath
org.slf4j:slf4j-api:2.0.13=compileClasspath
org.junit.jupiter:junit-jupiter-api:5.10.2=testCompileClasspath
empty=annotationProcessor
";
        let changes = compare_format(LockFormat::Gradle, old, new).unwrap();
        assert_eq!(
            changes.added,
            vec!["org.junit.jupiter:junit-jupiter-api 5.10.2"]
        );
        assert_eq!(changes.removed, vec!["org.legacy:old-lib 1.0.0"]);
        assert_eq!(
            changes.updated,
            vec!["org.slf4j:slf4j-api 2.0.12 -> 2.0.13"]
        );
        assert_eq!(
            changes.links,
            vec!["com.google.guava:guava +testCompileClasspath"]
        );
        assert!(gradle_packages("not valid gradle line\n").is_none());

        // Remote hunk fallback adding right below `#` header comments & `*-gradle.lockfile`.
        let dir = tempfile::tempdir().unwrap();
        let mut patch = String::from(concat!(
            "diff --git a/buildscript-gradle.lockfile b/buildscript-gradle.lockfile\n",
            "index 1111111..2222222 100644\n",
            "--- a/buildscript-gradle.lockfile\n",
            "+++ b/buildscript-gradle.lockfile\n",
            "@@ -1,4 +1,14 @@\n",
            " # This is a Gradle generated file for dependency locking.\n",
        ));
        for i in 0..10 {
            patch.push_str(&format!(
                "+com.example.longgroup.service:module-{i:02}:2.{i}.0=compileClasspath,runtimeClasspath,testCompileClasspath\n"
            ));
        }
        let found = sections(&patch, dir.path());
        assert_eq!(found.len(), 1);
        let rendered = found[0].text();
        assert!(rendered.contains("10 added"), "{rendered}");
        assert!(
            rendered.len() * 2 < patch.len(),
            "raw={} rendered={}\n{rendered}",
            patch.len(),
            rendered.len()
        );
    }

    #[test]
    fn podfile_lock_full_hunk_and_tamper_detection() {
        let old = "\
PODS:
  - Alamofire (5.9.0)
  - FirebaseCore (10.24.0):
    - GoogleUtilities/Environment (~> 7.12)
  - OldPod (1.0.0)

DEPENDENCIES:
  - Alamofire (~> 5.9)
  - FirebaseCore

SPEC REPOS:
  trunk:
    - Alamofire
    - FirebaseCore
    - OldPod

EXTERNAL SOURCES:
  LocalKit:
    :path: ../LocalKit

SPEC CHECKSUMS:
  Alamofire: 1111111111111111111111111111111111111111
  FirebaseCore: 2222222222222222222222222222222222222222
  OldPod: 3333333333333333333333333333333333333333

PODFILE CHECKSUM: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa

COCOAPODS: 1.15.2
";
        let new = "\
PODS:
  - Alamofire (5.9.1)
  - FirebaseCore (10.24.0):
    - GoogleUtilities/Environment (~> 7.12)
  - SnapKit (5.7.1)

DEPENDENCIES:
  - Alamofire (~> 5.9)
  - FirebaseCore
  - SnapKit

SPEC REPOS:
  trunk:
    - Alamofire
    - FirebaseCore
    - SnapKit

EXTERNAL SOURCES:
  LocalKit:
    :path: ../LocalKit

SPEC CHECKSUMS:
  Alamofire: 4444444444444444444444444444444444444444
  FirebaseCore: 9999999999999999999999999999999999999999
  SnapKit: 5555555555555555555555555555555555555555

PODFILE CHECKSUM: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb

COCOAPODS: 1.15.2
";
        let changes = compare_format(LockFormat::Podfile, old, new).unwrap();
        assert_eq!(changes.added, vec!["SnapKit 5.7.1"]);
        assert_eq!(changes.removed, vec!["OldPod 1.0.0"]);
        assert_eq!(changes.updated, vec!["Alamofire 5.9.0 -> 5.9.1"]);
        assert!(
            changes.flags.iter().any(|f| f.contains(
                "checksum changed on existing version FirebaseCore 10.24.0: 2222222222222222222222222222222222222222 -> 9999999999999999999999999999999999999999"
            )),
            "{:?}",
            changes.flags
        );
        assert!(podfile_packages("DEPENDENCIES:\n  - Foo\n").is_none());

        // Remote hunk fallback: ignores DEPENDENCIES constraints (`~> 5.9`) and SPEC REPOS
        // while flagging SPEC CHECKSUMS tampering and failing closed on EXTERNAL SOURCES edits.
        let dir = tempfile::tempdir().unwrap();
        let patch = "\
diff --git a/ios/Podfile.lock b/ios/Podfile.lock
index 1111111..2222222 100644
--- a/ios/Podfile.lock
+++ b/ios/Podfile.lock
@@ -1,6 +1,6 @@
 PODS:
-  - Alamofire (5.9.0)
+  - Alamofire (5.9.1)
   - FirebaseCore (10.24.0)
+  - SnapKit (5.7.1)
@@ -10,4 +10,5 @@
 DEPENDENCIES:
-  - Alamofire (~> 5.8)
+  - Alamofire (~> 5.9)
+  - SnapKit (~> 5.7)
@@ -20,3 +21,4 @@
 SPEC CHECKSUMS:
-  Alamofire: 1111111111111111111111111111111111111111
+  Alamofire: 4444444444444444444444444444444444444444
-  FirebaseCore: 2222222222222222222222222222222222222222
+  FirebaseCore: 9999999999999999999999999999999999999999
+  SnapKit: 5555555555555555555555555555555555555555
@@ -30,1 +32,1 @@
-PODFILE CHECKSUM: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
+PODFILE CHECKSUM: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
";
        let found = sections(patch, dir.path());
        assert_eq!(found.len(), 1);
        let rendered = found[0].text();
        assert!(rendered.contains("added: SnapKit 5.7.1"), "{rendered}");
        assert!(
            rendered.contains("updated: Alamofire 5.9.0 -> 5.9.1"),
            "{rendered}"
        );
        assert!(!rendered.contains("~>"), "{rendered}");
        assert!(
            found[0]
                .flags
                .iter()
                .any(|f| f.contains("checksum changed on existing version FirebaseCore 10.24.0")),
            "{:?}",
            found[0].flags
        );

        let ext_swap = "\
diff --git a/ios/Podfile.lock b/ios/Podfile.lock
index 1111111..2222222 100644
--- a/ios/Podfile.lock
+++ b/ios/Podfile.lock
@@ -15,3 +15,3 @@
 EXTERNAL SOURCES:
   MyPod:
-    :git: https://github.com/org/pod.git
+    :git: https://evil.example/pod.git
";
        assert!(sections(ext_swap, dir.path()).is_empty());
    }

    #[test]
    fn nuget_packages_lock_full_hunk_and_multi_tfm() {
        let old = r#"{
  "version": 1,
  "dependencies": {
    "net8.0": {
      "Newtonsoft.Json": {
        "type": "Direct",
        "requested": "[13.0.1, )",
        "resolved": "13.0.1",
        "contentHash": "hashOld=="
      },
      "Serilog": {
        "type": "Direct",
        "requested": "[3.1.1, )",
        "resolved": "3.1.1",
        "contentHash": "serilogHash=="
      },
      "Legacy.Pkg": {
        "type": "Transitive",
        "resolved": "1.0.0",
        "contentHash": "legacyHash=="
      },
      "App.Core": {
        "type": "Project",
        "dependencies": {
          "Serilog": "[3.1.1, )"
        }
      }
    }
  }
}"#;
        let new = r#"{
  "version": 1,
  "dependencies": {
    "net8.0": {
      "Newtonsoft.Json": {
        "type": "Direct",
        "requested": "[13.0.3, )",
        "resolved": "13.0.3",
        "contentHash": "hashNew=="
      },
      "Serilog": {
        "type": "Direct",
        "requested": "[3.1.1, )",
        "resolved": "3.1.1",
        "contentHash": "tamperedHash=="
      },
      "Polly": {
        "type": "Direct",
        "requested": "[8.4.0, )",
        "resolved": "8.4.0",
        "contentHash": "pollyHash=="
      },
      "App.Core": {
        "type": "Project",
        "dependencies": {
          "Serilog": "[3.1.1, )",
          "Polly": "[8.4.0, )"
        }
      }
    }
  }
}"#;
        let changes = compare_format(LockFormat::NuGet, old, new).unwrap();
        assert_eq!(changes.added, vec!["Polly 8.4.0"]);
        assert_eq!(changes.removed, vec!["Legacy.Pkg 1.0.0"]);
        assert_eq!(changes.updated, vec!["Newtonsoft.Json 13.0.1 -> 13.0.3"]);
        assert_eq!(changes.links, vec!["App.Core +Polly"]);
        assert!(
            changes.flags.iter().any(|f| f.contains(
                "checksum changed on existing version Serilog 3.1.1: serilogHash== -> tamperedHash=="
            )),
            "{:?}",
            changes.flags
        );
        assert!(nuget_packages("{\"version\":1}").is_none());

        // Remote hunk fallback with multi-TFM entries for the same package name:
        // a checksum conflict between TFMs must fail closed rather than silently overwrite.
        let dir = tempfile::tempdir().unwrap();
        let mut patch = String::from(concat!(
            "diff --git a/src/App/packages.lock.json b/src/App/packages.lock.json\n",
            "index 1111111..2222222 100644\n",
            "--- a/src/App/packages.lock.json\n",
            "+++ b/src/App/packages.lock.json\n",
            "@@ -4,30 +4,30 @@\n",
        ));
        for i in 0..8 {
            patch.push_str(&format!(
                concat!(
                    "       \"Company.Package.{i:02}\": {{\n",
                    "-        \"type\": \"Direct\",\n",
                    "-        \"requested\": \"[1.{i}.0, )\",\n",
                    "-        \"resolved\": \"1.{i}.0\",\n",
                    "-        \"contentHash\": \"oldHash{i:02}==\"\n",
                    "+        \"type\": \"Direct\",\n",
                    "+        \"requested\": \"[1.{i}.1, )\",\n",
                    "+        \"resolved\": \"1.{i}.1\",\n",
                    "+        \"contentHash\": \"newHash{i:02}==\"\n",
                    "       }},\n",
                ),
                i = i
            ));
        }
        let found = sections(&patch, dir.path());
        assert_eq!(found.len(), 1);
        let rendered = found[0].text();
        assert!(rendered.contains("8 updated"), "{rendered}");
        assert!(
            rendered.len() * 4 < patch.len(),
            "raw={} rendered={}\n{rendered}",
            patch.len(),
            rendered.len()
        );

        let conflicting_tfm = "\
diff --git a/packages.lock.json b/packages.lock.json
index 1111111..2222222 100644
--- a/packages.lock.json
+++ b/packages.lock.json
@@ -4,12 +4,12 @@
       \"Shared.Pkg\": {
         \"type\": \"Direct\",
         \"resolved\": \"1.0.0\",
-        \"contentHash\": \"orig==\"
+        \"contentHash\": \"tampered==\"
       },
@@ -40,12 +40,12 @@
       \"Shared.Pkg\": {
         \"type\": \"Direct\",
         \"resolved\": \"1.0.0\",
-        \"contentHash\": \"orig==\"
+        \"contentHash\": \"orig==\"
       },
";
        // Per-entry merging catches conflicting hashes across TFMs and fails closed!
        assert!(sections(conflicting_tfm, dir.path()).is_empty());
    }

    #[test]
    fn pipfile_lock_full_hunk_and_tamper_detection() {
        let old = r#"{
  "_meta": {
    "hash": { "sha256": "metaold" },
    "pipfile-spec": 6,
    "sources": [
      { "name": "pypi", "url": "https://pypi.org/simple", "verify_ssl": true }
    ]
  },
  "default": {
    "requests": {
      "hashes": ["sha256:aaa", "sha256:bbb"],
      "index": "pypi",
      "version": "==2.31.0"
    },
    "urllib3": {
      "hashes": ["sha256:ccc"],
      "index": "pypi",
      "version": "==2.2.1"
    },
    "oldlib": {
      "hashes": ["sha256:ddd"],
      "index": "pypi",
      "version": "==0.5.0"
    }
  },
  "develop": {
    "pytest": {
      "hashes": ["sha256:eee"],
      "index": "pypi",
      "version": "==8.2.0"
    }
  }
}"#;
        let new = r#"{
  "_meta": {
    "hash": { "sha256": "metanew" },
    "pipfile-spec": 6,
    "sources": [
      { "name": "pypi", "url": "https://pypi.org/simple", "verify_ssl": true }
    ]
  },
  "default": {
    "requests": {
      "hashes": ["sha256:zzz"],
      "index": "pypi",
      "version": "==2.32.3"
    },
    "urllib3": {
      "hashes": ["sha256:tampered"],
      "index": "pypi",
      "version": "==2.2.1"
    },
    "httpx": {
      "hashes": ["sha256:fff"],
      "index": "pypi",
      "version": "==0.27.0"
    }
  },
  "develop": {
    "pytest": {
      "hashes": ["sha256:eee"],
      "index": "pypi",
      "version": "==8.2.0"
    }
  }
}"#;
        let changes = compare_format(LockFormat::Pipfile, old, new).unwrap();
        assert_eq!(changes.added, vec!["httpx 0.27.0"]);
        assert_eq!(changes.removed, vec!["oldlib 0.5.0"]);
        assert_eq!(changes.updated, vec!["requests 2.31.0 -> 2.32.3"]);
        assert!(
            changes.flags.iter().any(|f| f.contains(
                "checksum changed on existing version urllib3 2.2.1: sha256:ccc -> sha256:tampered"
            )),
            "{:?}",
            changes.flags
        );
        assert!(pipfile_packages("{\"default\":{}}").is_none());

        // Remote hunk fallback handles `_meta.hash.sha256` change + multiple updated packages.
        let dir = tempfile::tempdir().unwrap();
        let mut patch = String::from(concat!(
            "diff --git a/Pipfile.lock b/Pipfile.lock\n",
            "index 1111111..2222222 100644\n",
            "--- a/Pipfile.lock\n",
            "+++ b/Pipfile.lock\n",
            "@@ -2,4 +2,4 @@\n",
            "   \"_meta\": {\n",
            "     \"hash\": {\n",
            "-      \"sha256\": \"metaold\"\n",
            "+      \"sha256\": \"metanew\"\n",
            "     },\n",
            "@@ -20,40 +20,40 @@\n",
            "   \"default\": {\n",
        ));
        for i in 0..8 {
            patch.push_str(&format!(
                concat!(
                    "     \"pkg_{i:02}\": {{\n",
                    "-      \"hashes\": [\n",
                    "-        \"sha256:old_a_{i:02}\",\n",
                    "-        \"sha256:old_b_{i:02}\"\n",
                    "-      ],\n",
                    "-      \"index\": \"pypi\",\n",
                    "-      \"version\": \"==1.{i}.0\"\n",
                    "+      \"hashes\": [\n",
                    "+        \"sha256:new_a_{i:02}\",\n",
                    "+        \"sha256:new_b_{i:02}\"\n",
                    "+      ],\n",
                    "+      \"index\": \"pypi\",\n",
                    "+      \"version\": \"==1.{i}.1\"\n",
                    "     }},\n",
                ),
                i = i
            ));
        }
        let found = sections(&patch, dir.path());
        assert_eq!(found.len(), 1);
        let rendered = found[0].text();
        assert!(rendered.contains("8 updated"), "{rendered}");
        assert!(
            rendered.len() * 5 < patch.len(),
            "raw={} rendered={}\n{rendered}",
            patch.len(),
            rendered.len()
        );
    }

    #[test]
    fn ansi_colored_lockfile_patch_folds_identically_to_plain() {
        let dir = tempfile::tempdir().unwrap();
        let plain = "\
diff --git a/gradle.lockfile b/gradle.lockfile
index 1111111..2222222 100644
--- a/gradle.lockfile
+++ b/gradle.lockfile
@@ -1,3 +1,3 @@
 # This is a Gradle generated file for dependency locking.
-org.slf4j:slf4j-api:2.0.12=compileClasspath
+org.slf4j:slf4j-api:2.0.13=compileClasspath
";
        let colored = "\
\x1b[1mdiff --git a/gradle.lockfile b/gradle.lockfile\x1b[m
\x1b[1mindex 1111111..2222222 100644\x1b[m
\x1b[1m--- a/gradle.lockfile\x1b[m
\x1b[1m+++ b/gradle.lockfile\x1b[m
\x1b[36m@@ -1,3 +1,3 @@\x1b[m
 # This is a Gradle generated file for dependency locking.
\x1b[31m-org.slf4j:slf4j-api:2.0.12=compileClasspath\x1b[m
\x1b[32m+org.slf4j:slf4j-api:2.0.13=compileClasspath\x1b[m
";
        let plain_sec = sections(plain, dir.path());
        let color_sec = sections(colored, dir.path());
        assert_eq!(plain_sec.len(), 1);
        assert_eq!(color_sec.len(), 1);
        assert_eq!(color_sec[0].text(), plain_sec[0].text());
    }

    #[test]
    fn split_npm_descriptor_handles_multibyte_utf8_prefix() {
        assert_eq!(split_npm_descriptor("é@1.0.0"), Some(("é", "1.0.0")));
        assert_eq!(
            split_npm_descriptor("@é/pkg@2.3.4"),
            Some(("@é/pkg", "2.3.4"))
        );
        assert_eq!(split_npm_descriptor("é"), None);
    }

    #[test]
    fn podfile_pubspec_and_pipfile_supply_chain_tamper_defenses() {
        let dir = tempfile::tempdir().unwrap();

        // 1. Podfile.lock: `:commit` swap flagged, `:http`/`:podspec` recorded,
        //    unrecognized EXTERNAL SOURCES/CHECKOUT OPTIONS keys fail closed,
        //    and `SPEC REPOS:` hijack fails closed in hunk fallback.
        let pod_old = "\
PODS:
  - MyPod (1.0.0)
  - ZipPod (2.0.0)

EXTERNAL SOURCES:
  MyPod:
    :git: https://github.com/org/MyPod.git
    :branch: main
  ZipPod:
    :http: https://example.com/ZipPod-2.0.0.zip
    :sha256: aaaa

CHECKOUT OPTIONS:
  MyPod:
    :git: https://github.com/org/MyPod.git
    :commit: 1111111111111111111111111111111111111111

SPEC CHECKSUMS:
  MyPod: 9999999999999999999999999999999999999999
  ZipPod: 8888888888888888888888888888888888888888
";
        let pod_new = "\
PODS:
  - MyPod (1.0.0)
  - ZipPod (2.0.0)

EXTERNAL SOURCES:
  MyPod:
    :git: https://github.com/org/MyPod.git
    :branch: main
  ZipPod:
    :http: https://evil.example/ZipPod-2.0.0.zip
    :sha256: aaaa

CHECKOUT OPTIONS:
  MyPod:
    :git: https://github.com/org/MyPod.git
    :commit: 2222222222222222222222222222222222222222

SPEC CHECKSUMS:
  MyPod: 9999999999999999999999999999999999999999
  ZipPod: 8888888888888888888888888888888888888888
";
        let pod_changes = compare_format(LockFormat::Podfile, pod_old, pod_new).unwrap();
        assert!(
            pod_changes.flags.iter().any(|f| f
                .contains("checksum changed on existing version MyPod 1.0.0")
                && f.contains("commit=1111")
                && f.contains("commit=2222")),
            "{:?}",
            pod_changes.flags
        );
        assert!(
            pod_changes.flags.iter().any(|f| f
                .contains("source changed on existing version ZipPod 2.0.0")
                && f.contains("http:https://evil.example/ZipPod-2.0.0.zip")),
            "{:?}",
            pod_changes.flags
        );
        let pod_unknown = "\
PODS:
  - MyPod (1.0.0)

EXTERNAL SOURCES:
  MyPod:
    :git: https://github.com/org/MyPod.git
    :submodules: true
";
        assert!(podfile_packages(pod_unknown).is_none());

        let spec_repo_hijack = "\
diff --git a/ios/Podfile.lock b/ios/Podfile.lock
index 1111111..2222222 100644
--- a/ios/Podfile.lock
+++ b/ios/Podfile.lock
@@ -10,4 +10,4 @@
 SPEC REPOS:
-  trunk:
+  https://evil.example/specs.git:
     - Alamofire
";
        assert!(sections(spec_repo_hijack, dir.path()).is_empty());

        // 2. pubspec.lock: `description.path`, `description.name`, `ref`, `relative`,
        //    `sdks:` changes tracked, and unrecognized description keys fail closed.
        let pub_old = "\
packages:
  my_git:
    dependency: \"direct main\"
    description:
      path: \"packages/safe\"
      ref: main
      resolved-ref: \"abc123\"
      url: \"https://github.com/org/repo.git\"
    source: git
    version: \"1.0.0\"
  aliased:
    dependency: \"direct main\"
    description:
      name: safe_pkg
      sha256: \"aaa\"
      url: \"https://pub.dev\"
    source: hosted
    version: \"1.0.0\"
sdks:
  dart: \">=3.2.0 <4.0.0\"
";
        let pub_new = "\
packages:
  my_git:
    dependency: \"direct main\"
    description:
      path: \"packages/evil\"
      ref: main
      resolved-ref: \"abc123\"
      url: \"https://github.com/org/repo.git\"
    source: git
    version: \"1.0.0\"
  aliased:
    dependency: \"direct main\"
    description:
      name: evil_pkg
      sha256: \"aaa\"
      url: \"https://pub.dev\"
    source: hosted
    version: \"1.0.0\"
sdks:
  dart: \">=3.5.0 <4.0.0\"
";
        let pub_changes = compare_format(LockFormat::Pubspec, pub_old, pub_new).unwrap();
        assert!(
            pub_changes.flags.iter().any(|f| f
                .contains("source changed on existing version my_git 1.0.0")
                && f.contains("packages/safe")
                && f.contains("packages/evil")),
            "{:?}",
            pub_changes.flags
        );
        assert!(
            pub_changes.flags.iter().any(|f| f
                .contains("source changed on existing version aliased 1.0.0")
                && f.contains("safe_pkg")
                && f.contains("evil_pkg")),
            "{:?}",
            pub_changes.flags
        );
        assert!(
            pub_changes
                .updated
                .iter()
                .any(|u| u == "sdk:dart >=3.2.0 <4.0.0 -> >=3.5.0 <4.0.0"),
            "{:?}",
            pub_changes.updated
        );
        let pub_unknown = "\
packages:
  foo:
    dependency: \"direct main\"
    description:
      name: foo
      url: \"https://pub.dev\"
      unexpected_key: \"evil\"
    source: hosted
    version: \"1.0.0\"
";
        assert!(pubspec_packages(pub_unknown).is_none());

        // 3. Pipfile.lock: `verify_ssl: false`, default-index hijack without explicit `"index"`,
        //    `"hashes": []` + `"ref"` swap, `develop -> default` promotion, and unknown keys.
        let pip_old = r#"{
  "_meta": {
    "pipfile-spec": 6,
    "sources": [
      { "name": "pypi", "url": "https://pypi.org/simple", "verify_ssl": true }
    ]
  },
  "default": {
    "implicit_idx": {
      "hashes": ["sha256:aaa"],
      "version": "==1.0.0"
    },
    "git_empty_hashes": {
      "git": "https://github.com/org/lib.git",
      "hashes": [],
      "ref": "aaaaaaaaaaaaaaaaaaaa",
      "subdirectory": "pkg_a",
      "version": "==1.0.0"
    }
  },
  "develop": {
    "dev_only": {
      "hashes": ["sha256:bbb"],
      "index": "pypi",
      "version": "==2.0.0"
    }
  }
}"#;
        let pip_new = r#"{
  "_meta": {
    "pipfile-spec": 6,
    "sources": [
      { "name": "pypi", "url": "https://evil.example/simple", "verify_ssl": false }
    ]
  },
  "default": {
    "implicit_idx": {
      "hashes": ["sha256:aaa"],
      "version": "==1.0.0"
    },
    "git_empty_hashes": {
      "git": "https://github.com/org/lib.git",
      "hashes": [],
      "ref": "bbbbbbbbbbbbbbbbbbbb",
      "subdirectory": "pkg_b",
      "version": "==1.0.0"
    },
    "dev_only": {
      "hashes": ["sha256:bbb"],
      "index": "pypi",
      "version": "==2.0.0"
    }
  },
  "develop": {}
}"#;
        let pip_changes = compare_format(LockFormat::Pipfile, pip_old, pip_new).unwrap();
        assert!(
            pip_changes.flags.iter().any(|f| f
                .contains("source changed on existing version implicit_idx 1.0.0")
                && f.contains("https://evil.example/simple (verify_ssl=false)")),
            "{:?}",
            pip_changes.flags
        );
        assert!(
            pip_changes.flags.iter().any(|f| f
                .contains("checksum changed on existing version git_empty_hashes 1.0.0")
                && f.contains("aaaaaaaaaaaaaaaaaaaa -> bbbbbbbbbbbbbbbbbbbb")),
            "{:?}",
            pip_changes.flags
        );
        assert!(
            pip_changes.flags.iter().any(|f| f
                .contains("source changed on existing version git_empty_hashes 1.0.0")
                && f.contains("subdir:pkg_a")
                && f.contains("subdir:pkg_b")),
            "{:?}",
            pip_changes.flags
        );
        assert!(
            pip_changes.flags.iter().any(|f| f
                .contains("source changed on existing version dev_only 2.0.0")
                && f.contains("[develop]")
                && f.contains("[default]")),
            "{:?}",
            pip_changes.flags
        );
        let pip_unknown = r#"{
  "_meta": { "pipfile-spec": 6 },
  "default": { "foo": { "version": "==1.0.0", "evil_hook": "rm -rf /" } }
}"#;
        assert!(pipfile_packages(pip_unknown).is_none());
    }

    #[test]
    fn q4_supply_chain_hardening_pipfile_podfile_and_pubspec() {
        let dir = tempfile::tempdir().unwrap();

        // (a) Pipfile.lock: secondary `_meta.sources[1]` `verify_ssl: false` hijack,
        //     `_meta.requires` tracking, and unknown `_meta` key fail-closed.
        let pip_old = r#"{
  "_meta": {
    "pipfile-spec": 6,
    "requires": { "python_version": "3.11" },
    "sources": [
      { "name": "pypi", "url": "https://pypi.org/simple", "verify_ssl": true },
      { "name": "corp", "url": "https://corp.example/simple", "verify_ssl": true }
    ]
  },
  "default": {
    "requests": { "hashes": ["sha256:aaa"], "index": "pypi", "version": "==2.31.0" }
  },
  "develop": {}
}"#;
        let pip_new = r#"{
  "_meta": {
    "pipfile-spec": 6,
    "requires": { "python_version": "3.12" },
    "sources": [
      { "name": "pypi", "url": "https://pypi.org/simple", "verify_ssl": true },
      { "name": "corp", "url": "https://corp.example/simple", "verify_ssl": false },
      { "name": "evil", "url": "https://evil.example/simple", "verify_ssl": false }
    ]
  },
  "default": {
    "requests": { "hashes": ["sha256:aaa"], "index": "pypi", "version": "==2.31.0" }
  },
  "develop": {}
}"#;
        let pip_changes = compare_format(LockFormat::Pipfile, pip_old, pip_new).unwrap();
        assert!(
            pip_changes.flags.iter().any(|f| f.contains(
                "source changed on existing version source:corp https://corp.example/simple: verify_ssl=true -> verify_ssl=false"
            )),
            "{:?}",
            pip_changes.flags
        );
        assert!(
            pip_changes
                .added
                .iter()
                .any(|a| a == "source:evil https://evil.example/simple"),
            "{:?}",
            pip_changes.added
        );
        assert!(
            pip_changes
                .updated
                .iter()
                .any(|u| u == "requires:python_version 3.11 -> 3.12"),
            "{:?}",
            pip_changes.updated
        );
        let pip_unknown_meta = r#"{
  "_meta": { "pipfile-spec": 6, "unexpected": { "a": 1 } },
  "default": {}
}"#;
        assert!(pipfile_packages(pip_unknown_meta).is_none());

        // (b) Pipfile.lock remote `gh pr diff` hunk fallback: `develop -> default` promotion
        //     flagged AND unchanged neighbor whose closing `}` gained `,` is NOT falsely removed,
        //     while an ambiguous-section hunk fails closed.
        let mut promote_patch = String::from(concat!(
            "diff --git a/Pipfile.lock b/Pipfile.lock\n",
            "index 1111111..2222222 100644\n",
            "--- a/Pipfile.lock\n",
            "+++ b/Pipfile.lock\n",
            "@@ -10,20 +10,20 @@\n",
            "   \"default\": {\n",
            "     \"neighbor\": {\n",
            "       \"hashes\": [\n",
            "         \"sha256:neighbor_hash_0001\",\n",
            "         \"sha256:neighbor_hash_0002\"\n",
            "       ],\n",
            "       \"index\": \"pypi\",\n",
            "       \"version\": \"==1.0.0\"\n",
            "-    }\n",
            "+    },\n",
            "+    \"promoted\": {\n",
            "+      \"hashes\": [\n",
            "+        \"sha256:promoted_hash_0001\",\n",
            "+        \"sha256:promoted_hash_0002\"\n",
            "+      ],\n",
            "+      \"index\": \"pypi\",\n",
            "+      \"version\": \"==2.0.0\"\n",
            "+    }\n",
            "   },\n",
            "   \"develop\": {\n",
            "     \"dev_keep\": {\n",
            "       \"hashes\": [\n",
            "         \"sha256:dev_keep_hash_0001\",\n",
            "         \"sha256:dev_keep_hash_0002\"\n",
            "       ],\n",
            "       \"index\": \"pypi\",\n",
            "       \"version\": \"==3.0.0\"\n",
            "-    },\n",
            "-    \"promoted\": {\n",
            "-      \"hashes\": [\n",
            "-        \"sha256:promoted_hash_0001\",\n",
            "-        \"sha256:promoted_hash_0002\"\n",
            "-      ],\n",
            "-      \"index\": \"pypi\",\n",
            "-      \"version\": \"==2.0.0\"\n",
            "     }\n",
            "   }\n",
        ));
        let promote_sections = sections(&promote_patch, dir.path());
        assert_eq!(promote_sections.len(), 1);
        let promote_text = promote_sections[0].text();
        assert!(
            promote_sections[0].flags.iter().any(|f| f
                .contains("source changed on existing version promoted 2.0.0")
                && f.contains("[develop]")
                && f.contains("[default]")),
            "flags={:?}\ntext={promote_text}",
            promote_sections[0].flags
        );
        assert!(
            !promote_text.contains("neighbor") && !promote_text.contains("dev_keep"),
            "trailing-comma neighbors must not appear as added/removed:\n{promote_text}"
        );
        // Ambiguous Pipfile.lock hunk (no `"default": {` or `"develop": {` header) fails closed.
        promote_patch.clear();
        promote_patch.push_str(concat!(
            "diff --git a/Pipfile.lock b/Pipfile.lock\n",
            "index 1111111..2222222 100644\n",
            "--- a/Pipfile.lock\n",
            "+++ b/Pipfile.lock\n",
            "@@ -40,8 +40,8 @@\n",
            "     \"mystery\": {\n",
            "-      \"version\": \"==1.0.0\"\n",
            "+      \"version\": \"==1.0.1\"\n",
            "     },\n",
        ));
        assert!(sections(&promote_patch, dir.path()).is_empty());

        // (c) Podfile.lock: `COCOAPODS PLUGINS:` injection tracked full-file & fails closed
        //     in hunks, and orphan/subspec EXTERNAL SOURCES/SPEC CHECKSUMS are never dropped.
        let pod_old = "\
PODS:
  - MyPod (1.0.0)

SPEC CHECKSUMS:
  MyPod: 1111111111111111111111111111111111111111

COCOAPODS PLUGINS:
  cocoapods-keys: 2.2.1
";
        let pod_new = "\
PODS:
  - MyPod (1.0.0)

EXTERNAL SOURCES:
  MyPod/Sub:
    :git: https://evil.example/MyPod.git
  OrphanPod:
    :git: https://evil.example/OrphanPod.git

SPEC CHECKSUMS:
  MyPod: 1111111111111111111111111111111111111111
  OrphanPod: 2222222222222222222222222222222222222222

COCOAPODS PLUGINS:
  cocoapods-keys: 2.2.1
  cocoapods-evil: 0.1.0
";
        let pod_changes = compare_format(LockFormat::Podfile, pod_old, pod_new).unwrap();
        assert!(
            pod_changes
                .added
                .iter()
                .any(|a| a == "plugin:cocoapods-evil 0.1.0"),
            "{:?}",
            pod_changes.added
        );
        assert!(
            pod_changes
                .added
                .iter()
                .any(|a| a == "external:OrphanPod external"),
            "{:?}",
            pod_changes.added
        );
        assert!(
            pod_changes.flags.iter().any(|f| f
                .contains("source changed on existing version MyPod 1.0.0")
                && f.contains("git:https://evil.example/MyPod.git")),
            "{:?}",
            pod_changes.flags
        );
        let pod_plugin_hunk = "\
diff --git a/ios/Podfile.lock b/ios/Podfile.lock
index 1111111..2222222 100644
--- a/ios/Podfile.lock
+++ b/ios/Podfile.lock
@@ -20,2 +20,3 @@
 COCOAPODS PLUGINS:
   cocoapods-keys: 2.2.1
+  cocoapods-evil: 0.1.0
";
        assert!(sections(pod_plugin_hunk, dir.path()).is_empty());

        // (d) pubspec.lock: malformed inline YAML flow maps fail closed instead of dropping garbage.
        for bad_desc in [
            "{name: foo, garbage_without_colon}",
            "{name: foo, nested: {url: \"https://evil.example\"}}",
            "{name: foo, url: \"https://unclosed}",
            "{name: foo, url: 'unclosed",
            "{: missing_key}",
        ] {
            let bad_pubspec = format!(
                "\
packages:
  foo:
    dependency: \"direct main\"
    description: {bad_desc}
    source: hosted
    version: \"1.0.0\"
"
            );
            assert!(
                pubspec_packages(&bad_pubspec).is_none(),
                "must fail closed on malformed inline map: {bad_desc}"
            );
        }
    }

    #[test]
    fn q7_pipfile_hunk_single_line_injection_fails_closed_and_podfile_subspecs_keep_checksum() {
        let dir = tempfile::tempdir().unwrap();

        // (a) Pipfile.lock hunk injection vector 1: sneaking extra keys or brace-balanced
        //     objects onto `_meta.hash.sha256` or `"hash":` entry MUST fail closed.
        let sha256_brace_smuggle = concat!(
            "diff --git a/Pipfile.lock b/Pipfile.lock\n",
            "index 1111111..2222222 100644\n",
            "--- a/Pipfile.lock\n",
            "+++ b/Pipfile.lock\n",
            "@@ -2,5 +2,5 @@\n",
            "   \"_meta\": {\n",
            "     \"hash\": {\n",
            "-      \"sha256\": \"aaa\"\n",
            "+      \"sha256\": \"bbb\" }, \"sources\": [{\"url\": \"https://evil.example\", \"verify_ssl\": false}], \"x\": {\n",
            "     },\n",
        );
        assert!(
            sections(sha256_brace_smuggle, dir.path()).is_empty(),
            "brace-balanced smuggling inside _meta.hash.sha256 must fail closed"
        );

        let sha256_extra_key = concat!(
            "diff --git a/Pipfile.lock b/Pipfile.lock\n",
            "index 1111111..2222222 100644\n",
            "--- a/Pipfile.lock\n",
            "+++ b/Pipfile.lock\n",
            "@@ -2,5 +2,5 @@\n",
            "   \"_meta\": {\n",
            "     \"hash\": {\n",
            "-      \"sha256\": \"aaa\"\n",
            "+      \"sha256\": \"bbb\", \"evil\": \"injected\"\n",
            "     },\n",
        );
        assert!(
            sections(sha256_extra_key, dir.path()).is_empty(),
            "extra key on _meta.hash.sha256 line must fail closed"
        );

        let hash_header_smuggle = concat!(
            "diff --git a/Pipfile.lock b/Pipfile.lock\n",
            "index 1111111..2222222 100644\n",
            "--- a/Pipfile.lock\n",
            "+++ b/Pipfile.lock\n",
            "@@ -2,5 +2,5 @@\n",
            "   \"_meta\": {\n",
            "     \"hash\": { \"evil\": 1, \"nested\": {\n",
            "-      \"sha256\": \"aaa\"\n",
            "+      \"sha256\": \"bbb\"\n",
            "     },\n",
        );
        assert!(
            sections(hash_header_smuggle, dir.path()).is_empty(),
            "non-exact `\"hash\": {{` header must fail closed"
        );

        // (b) Pipfile.lock hunk injection vector 2: inline payload on `"_meta"`, `"default"`,
        //     or `"develop"` section opening line MUST fail closed.
        for bad_section_patch in [
            concat!(
                "diff --git a/Pipfile.lock b/Pipfile.lock\n",
                "index 1111111..2222222 100644\n",
                "--- a/Pipfile.lock\n",
                "+++ b/Pipfile.lock\n",
                "@@ -2,4 +2,4 @@\n",
                "   \"_meta\": { \"sources\": [{\"url\": \"https://evil.example\", \"verify_ssl\": false}], \"hash\": {\n",
                "-      \"sha256\": \"aaa\"\n",
                "+      \"sha256\": \"bbb\"\n",
                "   },\n",
            ),
            concat!(
                "diff --git a/Pipfile.lock b/Pipfile.lock\n",
                "index 1111111..2222222 100644\n",
                "--- a/Pipfile.lock\n",
                "+++ b/Pipfile.lock\n",
                "@@ -10,4 +10,4 @@\n",
                "-  \"default\": {}\n",
                "+  \"default\": { \"evil\": { \"version\": \"==1.0.0\" } }\n",
            ),
            concat!(
                "diff --git a/Pipfile.lock b/Pipfile.lock\n",
                "index 1111111..2222222 100644\n",
                "--- a/Pipfile.lock\n",
                "+++ b/Pipfile.lock\n",
                "@@ -10,5 +10,5 @@\n",
                "   \"develop\": { \"evil\": { \"version\": \"==1.0.0\" }, \"nested\": {\n",
                "-    \"pytest\": {\"version\": \"==8.0.0\"}\n",
                "+    \"pytest\": {\"version\": \"==8.1.0\"}\n",
                "   }\n",
            ),
        ] {
            assert!(
                sections(bad_section_patch, dir.path()).is_empty(),
                "inline section header smuggling must fail closed:\n{bad_section_patch}"
            );
        }

        // (c) Podfile.lock sibling subspec addition (`Firebase/Auth` before `Firebase/Core`,
        //     or root `Firebase` added next to `Firebase/Core`) MUST NOT emit a false checksum
        //     tamper alert, while a genuine root checksum swap STILL fires a tamper alert.
        let pod_one_subspec = "\
PODS:
  - Firebase/Core (10.24.0)

EXTERNAL SOURCES:
  Firebase:
    :git: https://github.com/firebase/firebase-ios-sdk.git
    :tag: 10.24.0

CHECKOUT OPTIONS:
  Firebase:
    :git: https://github.com/firebase/firebase-ios-sdk.git
    :tag: 10.24.0

SPEC CHECKSUMS:
  Firebase: 1111111111111111111111111111111111111111
";
        let pod_two_subspecs = "\
PODS:
  - Firebase/Auth (10.24.0)
  - Firebase/Core (10.24.0)

EXTERNAL SOURCES:
  Firebase:
    :git: https://github.com/firebase/firebase-ios-sdk.git
    :tag: 10.24.0

CHECKOUT OPTIONS:
  Firebase:
    :git: https://github.com/firebase/firebase-ios-sdk.git
    :tag: 10.24.0

SPEC CHECKSUMS:
  Firebase: 1111111111111111111111111111111111111111
";
        let pod_with_root = "\
PODS:
  - Firebase (10.24.0)
  - Firebase/Core (10.24.0)

EXTERNAL SOURCES:
  Firebase:
    :git: https://github.com/firebase/firebase-ios-sdk.git
    :tag: 10.24.0

CHECKOUT OPTIONS:
  Firebase:
    :git: https://github.com/firebase/firebase-ios-sdk.git
    :tag: 10.24.0

SPEC CHECKSUMS:
  Firebase: 1111111111111111111111111111111111111111
";
        let pod_tampered_checksum = pod_two_subspecs.replace(
            "1111111111111111111111111111111111111111",
            "9999999999999999999999999999999999999999",
        );

        let add_sibling =
            compare_format(LockFormat::Podfile, pod_one_subspec, pod_two_subspecs).unwrap();
        assert_eq!(add_sibling.added, vec!["Firebase/Auth 10.24.0"]);
        assert!(
            add_sibling.flags.is_empty(),
            "adding sibling subspec must not trigger false checksum alert: {:?}",
            add_sibling.flags
        );

        let add_root = compare_format(LockFormat::Podfile, pod_one_subspec, pod_with_root).unwrap();
        assert_eq!(add_root.added, vec!["Firebase 10.24.0"]);
        assert!(
            add_root.flags.is_empty(),
            "adding root pod next to subspec must not trigger false checksum alert: {:?}",
            add_root.flags
        );

        let real_tamper = compare_format(
            LockFormat::Podfile,
            pod_two_subspecs,
            &pod_tampered_checksum,
        )
        .unwrap();
        assert!(
            real_tamper
                .flags
                .iter()
                .any(|f| f.contains("checksum changed on existing version Firebase/Core 10.24.0")),
            "genuine root checksum tamper must still be flagged: {:?}",
            real_tamper.flags
        );
    }

    #[test]
    fn q11_pipfile_meta_hash_nested_object_fails_closed() {
        let dir = tempfile::tempdir().unwrap();

        let nested_meta_hash_patch = concat!(
            "diff --git a/Pipfile.lock b/Pipfile.lock\n",
            "index 1111111..2222222 100644\n",
            "--- a/Pipfile.lock\n",
            "+++ b/Pipfile.lock\n",
            "@@ -2,8 +2,8 @@\n",
            "   \"_meta\": {\n",
            "     \"hash\": {\n",
            "       \"nested\": {\n",
            "-        \"sha256\": \"aaa\"\n",
            "+        \"sha256\": \"bbb\"\n",
            "       }\n",
            "     },\n",
            "   },\n",
            "@@ -20,8 +20,8 @@\n",
            "   \"default\": {\n",
            "     \"requests\": {\n",
            "       \"hashes\": [\"sha256:111\"],\n",
            "       \"index\": \"pypi\",\n",
            "-      \"version\": \"==2.31.0\"\n",
            "+      \"version\": \"==2.32.0\"\n",
            "     }\n",
            "   }\n",
        );
        assert!(
            sections(nested_meta_hash_patch, dir.path()).is_empty(),
            "nested object inside _meta.hash must fail closed"
        );

        // Benign control: direct `_meta.hash.sha256` update at depth 2 alongside a package bump still succeeds.
        let valid_meta_hash_patch = concat!(
            "diff --git a/Pipfile.lock b/Pipfile.lock\n",
            "index 1111111..2222222 100644\n",
            "--- a/Pipfile.lock\n",
            "+++ b/Pipfile.lock\n",
            "@@ -2,6 +2,6 @@\n",
            "   \"_meta\": {\n",
            "     \"hash\": {\n",
            "-      \"sha256\": \"aaa\"\n",
            "+      \"sha256\": \"bbb\"\n",
            "     },\n",
            "   },\n",
            "@@ -20,8 +20,8 @@\n",
            "   \"default\": {\n",
            "     \"requests\": {\n",
            "       \"hashes\": [\"sha256:111\"],\n",
            "       \"index\": \"pypi\",\n",
            "-      \"version\": \"==2.31.0\"\n",
            "+      \"version\": \"==2.32.0\"\n",
            "     }\n",
            "   }\n",
        );
        let valid_sections = sections(valid_meta_hash_patch, dir.path());
        assert_eq!(valid_sections.len(), 1);
        assert!(
            valid_sections[0]
                .text()
                .contains("requests 2.31.0 -> 2.32.0"),
            "{}",
            valid_sections[0].text()
        );
    }
}
