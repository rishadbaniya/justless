//! Dependency changes in lockfile patches (`Cargo.lock`, `package-lock.json`,
//! `pnpm-lock.yaml`, `yarn.lock`, `bun.lock`, `uv.lock`, `go.sum`, `poetry.lock`,
//! `Gemfile.lock`, `composer.lock`, `flake.lock`, `Package.resolved`, and `mix.lock`)
//! plus generated/snapshot file folding.
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
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::summary::Summary;

/// The largest patch this module reads.
pub const LIMIT: u64 = 8 * 1024 * 1024;

/// Minimum patch section length (in lines) before a generated/snapshot file is folded.
pub(crate) const GENERATED_FOLD_LINES: usize = 25;

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
        let mut text = self.head.clone();
        for flag in &self.flags {
            text.push_str(&format!("  ! {flag}\n"));
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
    name.ends_with(".snap")
        || name.ends_with(".min.js")
        || name.ends_with(".min.css")
        || name.ends_with(".pb.go")
        || name.ends_with("_pb2.py")
        || name.ends_with(".g.dart")
        || name.contains(".generated.")
}

fn diff_section_path(line: &str) -> Option<&str> {
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
    let at = line.bytes().take_while(|&b| b == b'@').count();
    if at < 3 {
        return None;
    }
    let rest = line.get(at..)?;
    if !rest.starts_with(" -") {
        return None;
    }
    let closing = format!(" {}", "@".repeat(at));
    rest.contains(&closing).then_some(at - 1)
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
    const NEEDLES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lock",
        "uv.lock",
        "go.sum",
        "poetry.lock",
        "Gemfile.lock",
        "composer.lock",
        "flake.lock",
        "Package.resolved",
        "mix.lock",
        ".snap",
        ".min.js",
        ".min.css",
        ".pb.go",
        ".generated.",
        "_pb2.py",
        ".g.dart",
    ];
    if !NEEDLES.iter().any(|name| patch.contains(name)) {
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
            ActiveKind::Generated { added, deleted, .. } => {
                if section.last.saturating_sub(section.first) + 1 > GENERATED_FOLD_LINES {
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
            } else if is_generated_path(path) {
                Some(ActiveSection {
                    path: path.to_owned(),
                    first: number,
                    last: number,
                    hunks: false,
                    kind: ActiveKind::Generated {
                        added: 0,
                        deleted: 0,
                        parents: 0,
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
    if spec.len() < 3 {
        return None;
    }
    let at = spec[1..].find('@')? + 1;
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

fn parse_inline_yaml_map(map: &str) -> Vec<(String, String)> {
    let inner = map
        .trim()
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or("");
    let mut pairs = Vec::new();
    let mut start = 0;
    let mut quote = None;
    for (idx, ch) in inner.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == ',' => {
                if let Some((k, v)) = inner[start..idx].split_once(':') {
                    pairs.push((
                        unquote_lock_scalar(k).to_owned(),
                        unquote_lock_scalar(v).to_owned(),
                    ));
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    if start < inner.len()
        && let Some((k, v)) = inner[start..].split_once(':')
    {
        pairs.push((
            unquote_lock_scalar(k).to_owned(),
            unquote_lock_scalar(v).to_owned(),
        ));
    }
    pairs
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
                        if rest.starts_with('{') && rest.ends_with('}') {
                            for (k, v) in parse_inline_yaml_map(rest) {
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
fn strip_jsonc_trailing_commas(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let ch = text[i..].chars().next().unwrap();
        let len = ch.len_utf8();
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            i += len;
            continue;
        }
        if ch == '"' {
            in_string = true;
            out.push(ch);
            i += len;
            continue;
        }
        if ch == ',' {
            let mut j = i + len;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if matches!(bytes.get(j), Some(b']' | b'}')) {
                i += len;
                continue;
            }
        }
        out.push(ch);
        i += len;
    }
    out
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
    let stripped: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
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

/// Fallback hunk-based dependency diff when git blobs are absent locally (such as
/// remote `gh pr diff` / `glab mr diff` patches). Conservative: returns `None` if
/// changed lines cannot be confidently attributed to parsed packages.
fn compare_hunks(format: LockFormat, hunk_lines: &[&str]) -> Option<Changes> {
    let hunks = split_hunk_sides(hunk_lines)?;
    let mut old_pkgs: BTreeMap<Key, Package> = BTreeMap::new();
    let mut new_pkgs: BTreeMap<Key, Package> = BTreeMap::new();
    match format {
        LockFormat::GoSum => {
            let mut old_text = String::new();
            let mut new_text = String::new();
            for (old, new) in &hunks {
                for line in old {
                    old_text.push_str(line);
                    old_text.push('\n');
                }
                for line in new {
                    new_text.push_str(line);
                    new_text.push('\n');
                }
            }
            return compare_format(LockFormat::GoSum, &old_text, &new_text);
        }
        LockFormat::Mix => {
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

        // Generated-path matcher covers all 7 documented patterns and ignores ordinary code.
        for path in [
            "a/b.snap",
            "dist/app.min.js",
            "dist/site.min.css",
            "proto/api.pb.go",
            "src/schema.generated.ts",
            "proto/api_pb2.py",
            "lib/model.g.dart",
        ] {
            assert!(is_generated_path(path), "{path}");
        }
        for code in ["src/lib.rs", "src/app.js", "src/api.go", "tests/test.py"] {
            assert!(!is_generated_path(code), "{code}");
        }
    }
}
