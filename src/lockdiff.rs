//! Dependency changes in lockfile patches (`Cargo.lock`, `package-lock.json`,
//! `pnpm-lock.yaml`, `yarn.lock`, `bun.lock`, `uv.lock`, and `go.sum`).
//!
//! A lockfile hunk says where lines changed, not which packages did. For each
//! supported lockfile section of a `git diff` or `git show`, both revisions are
//! read by the blob ids on its `index OLD..NEW` line, and the section is
//! described as packages added, removed and updated and dependency links changed.
//! A `source` or `checksum` that changed on a version present in both revisions,
//! or a source added to or removed from such a version, is flagged on its own
//! line, since that is how a substituted package looks.
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
    matches!(rest.first(), Some(&("diff" | "show")))
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
            _ => None,
        }
    }
}

/// Describe each supported lockfile section of `patch` whose revisions Git in
/// `cwd` can read. Line numbers start at 1, as in `justless log`.
pub fn sections(patch: &str, cwd: &Path) -> Vec<Section> {
    if ![
        "Cargo.lock",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lock",
        "uv.lock",
        "go.sum",
    ]
    .iter()
    .any(|name| patch.contains(name))
    {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut current: Option<(String, LockFormat, usize, usize, Option<String>, bool)> = None;
    let finish = |section: Option<(String, LockFormat, usize, usize, Option<String>, bool)>,
                  found: &mut Vec<Section>| {
        if let Some((path, format, first, last, Some(index), _)) = section
            && let Some(section) = describe(&path, format, first, last, &index, cwd)
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
                .filter(|path| !path.starts_with('"'))
                .and_then(|path| {
                    LockFormat::of_path(path)
                        .map(|format| (path.to_owned(), format, number, number, None, false))
                });
            continue;
        }
        let Some((_, _, _, last, blobs, hunks)) = current.as_mut() else {
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

fn parse_format(format: LockFormat, text: &str) -> Option<BTreeMap<Key, Package>> {
    match format {
        LockFormat::Cargo => packages(text),
        LockFormat::Npm => npm_packages(text),
        LockFormat::Yarn => yarn_packages(text),
        LockFormat::Pnpm => pnpm_packages(text),
        LockFormat::Bun => bun_packages(text),
        LockFormat::Uv => uv_packages(text),
        LockFormat::GoSum => gosum_packages(text),
    }
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
            (&["git", "-P", "diff"][..], true),
            (&["git", "--no-optional-locks", "show"][..], true),
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
}
