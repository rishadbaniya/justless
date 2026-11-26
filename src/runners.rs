//! Test runners for `justless test`: which runner a project or command uses,
//! where it starts, the facts Cargo reports about packages and targets, the
//! argument lists that select tests, and what the runner's output says ran.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use crate::index::Index;
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Cargo,
    Nextest,
    Pytest,
    Jest,
    Vitest,
    Bun,
    #[serde(rename = "node-test")]
    NodeTest,
    Playwright,
    #[serde(rename = "deno-test")]
    DenoTest,
    #[serde(rename = "dart-test")]
    DartTest,
    Rspec,
    Go,
    Command,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Nextest => "cargo nextest",
            Self::Pytest => "pytest",
            Self::Jest => "jest",
            Self::Vitest => "vitest",
            Self::Bun => "bun test",
            Self::NodeTest => "node --test",
            Self::Playwright => "playwright",
            Self::DenoTest => "deno test",
            Self::DartTest => "dart test",
            Self::Rspec => "rspec",
            Self::Go => "go",
            Self::Command => "command",
        }
    }

    /// The run family and command label recorded for this runner's steps.
    pub fn family(self) -> &'static str {
        match self {
            Self::Cargo => "cargo test",
            Self::Nextest => "cargo nextest",
            Self::Pytest => "pytest",
            Self::Jest => "jest",
            Self::Vitest => "vitest",
            Self::Bun => "bun test",
            Self::NodeTest => "node --test",
            Self::Playwright => "playwright test",
            Self::DenoTest => "deno test",
            Self::DartTest => "dart test",
            Self::Rspec => "rspec",
            Self::Go => "go test",
            Self::Command => "",
        }
    }

    /// What one selected unit is for this runner.
    pub fn unit(self) -> &'static str {
        match self {
            Self::Jest
            | Self::Vitest
            | Self::Bun
            | Self::NodeTest
            | Self::Playwright
            | Self::DenoTest
            | Self::DartTest => "test file",
            Self::Rspec => "spec file",
            _ => "test",
        }
    }

    pub fn is_cargo(self) -> bool {
        matches!(self, Self::Cargo | Self::Nextest)
    }

    pub fn is_js(self) -> bool {
        matches!(
            self,
            Self::Jest
                | Self::Vitest
                | Self::Bun
                | Self::NodeTest
                | Self::Playwright
                | Self::DenoTest
        )
    }
}

/// One runner and the directory it starts in.
pub(crate) struct Runner {
    pub kind: Kind,
    /// Workspace-relative directory, empty for the workspace root.
    pub root: String,
    /// The command that runs this runner's full suite.
    pub base: Vec<String>,
    /// Why the command cannot be narrowed: it already selects tests.
    pub selects: Option<String>,
    /// A command Justless does not know how to narrow.
    pub unknown: Option<String>,
    /// Why the runner's facts could not be read; the full suite runs instead.
    pub error: Option<String>,
    pub cargo: Option<Cargo>,
    /// Where a command the user gave runs: their own directory.
    pub cwd: Option<String>,
    /// pytest's `python_files` patterns, empty for its defaults.
    pub python_files: Vec<String>,
    /// pytest's `testpaths`, as workspace-relative directories.
    pub testpaths: Vec<String>,
}

impl Runner {
    fn new(kind: Kind, root: String, base: Vec<String>) -> Self {
        Self {
            kind,
            root,
            base,
            selects: None,
            unknown: None,
            error: None,
            cargo: None,
            cwd: None,
            python_files: Vec::new(),
            testpaths: Vec::new(),
        }
    }

    /// Whether pytest's full suite collects a workspace-relative file: its
    /// name matches `python_files` and it lies under `testpaths`, if set.
    pub fn pytest_collects(&self, file: &str) -> bool {
        let name = file.rsplit('/').next().unwrap_or(file);
        let defaults = ["test_*.py".to_owned(), "*_test.py".to_owned()];
        let patterns = if self.python_files.is_empty() {
            &defaults[..]
        } else {
            &self.python_files[..]
        };
        name.ends_with(".py")
            && patterns.iter().any(|pattern| wildcard(pattern, name))
            && (self.testpaths.is_empty()
                || self.testpaths.iter().any(|directory| {
                    under(file, directory)
                        || (directory.contains(['*', '?']) && wildcard(directory, file))
                }))
    }

    /// Whether a workspace-relative file lies under this runner's root.
    pub fn contains(&self, file: &str) -> bool {
        under(file, &self.root)
    }

    /// `file` relative to the runner root.
    pub fn relative<'a>(&self, file: &'a str) -> &'a str {
        if self.root.is_empty() {
            file
        } else {
            file.strip_prefix(&self.root)
                .and_then(|rest| rest.strip_prefix('/'))
                .unwrap_or(file)
        }
    }
}

/// Whether `file` is `directory` or lies below it. The empty directory holds everything.
pub(crate) fn under(file: &str, directory: &str) -> bool {
    directory.is_empty()
        || file == directory
        || file
            .strip_prefix(directory)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// A shell-style pattern with `*` and `?`, as pytest's `python_files` uses.
pub(crate) fn wildcard(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((at, matched)) = star {
            p = at + 1;
            t = matched + 1;
            star = Some((at, matched + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// pytest's `python_files` and `testpaths` from the configuration in
/// `directory`, checked in pytest's order, with testpaths made workspace-relative.
fn pytest_options(workspace: &Workspace, directory: &str) -> Option<(Vec<String>, Vec<String>)> {
    let read = |name: &str| fs::read_to_string(workspace.root.join(directory).join(name)).ok();
    let (files, paths) = if let Some(text) = read("pytest.ini") {
        ini_options(&text, "pytest")?
    } else if let Some(options) = read("pyproject.toml").and_then(|text| {
        let document = text.parse::<toml_edit::DocumentMut>().ok()?;
        let section = document.get("tool")?.get("pytest")?.get("ini_options")?;
        let list = |key: &str| -> Vec<String> {
            match section.get(key) {
                Some(item) if item.as_array().is_some() => item
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|value| value.as_str())
                    .map(str::to_owned)
                    .collect(),
                Some(item) => item
                    .as_str()
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect(),
                None => Vec::new(),
            }
        };
        Some((list("python_files"), list("testpaths")))
    }) {
        options
    } else if let Some(options) = read("tox.ini").and_then(|text| ini_options(&text, "pytest")) {
        options
    } else {
        read("setup.cfg").and_then(|text| ini_options(&text, "tool:pytest"))?
    };
    let paths = paths
        .iter()
        .map(|path| {
            let path = path.trim_start_matches("./").trim_end_matches('/');
            if directory.is_empty() {
                path.to_owned()
            } else {
                format!("{directory}/{path}")
            }
        })
        .collect();
    Some((files, paths))
}

/// `python_files` and `testpaths` in an ini file's section, or `None` without the section.
fn ini_options(text: &str, section: &str) -> Option<(Vec<String>, Vec<String>)> {
    let header = format!("[{section}]");
    let mut inside = false;
    let mut found = false;
    let mut key = String::new();
    let mut values = BTreeMap::<String, Vec<String>>::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed == header;
            found |= inside;
            key.clear();
            continue;
        }
        if !inside || trimmed.is_empty() || trimmed.starts_with(['#', ';']) {
            continue;
        }
        let words =
            |value: &str| -> Vec<String> { value.split_whitespace().map(str::to_owned).collect() };
        if line.starts_with(char::is_whitespace) && !key.is_empty() {
            values
                .entry(key.clone())
                .or_default()
                .extend(words(trimmed));
        } else if let Some((name, value)) = trimmed.split_once(['=', ':']) {
            key = name.trim().to_owned();
            values.entry(key.clone()).or_default().extend(words(value));
        }
    }
    found.then(|| {
        (
            values.remove("python_files").unwrap_or_default(),
            values.remove("testpaths").unwrap_or_default(),
        )
    })
}

fn parent(file: &str) -> &str {
    match file.rsplit_once('/') {
        Some((directory, _)) => directory,
        None => "",
    }
}

/// The facts `cargo metadata --no-deps` reports, plus what only the manifests say.
pub(crate) struct Cargo {
    pub packages: Vec<Package>,
    /// Packages the base command tests, by name.
    pub scope: BTreeSet<String>,
    /// Manifests of Cargo packages that are not members, such as fixtures.
    pub outside: Vec<String>,
    /// Workspace-relative manifest of the Cargo workspace root.
    pub root_manifest: String,
}

pub(crate) struct Package {
    /// The name as written in the manifest, which `-p` expects.
    pub name: String,
    pub dir: String,
    pub manifest: String,
    pub targets: Vec<Target>,
    /// Member packages this package depends on, of any dependency kind.
    pub depends: BTreeSet<String>,
    pub features: BTreeMap<String, Vec<String>>,
}

pub(crate) struct Target {
    pub kind: String,
    pub name: String,
    pub src: String,
    pub required: Vec<String>,
    pub test: bool,
    pub doctest: bool,
    pub harness: bool,
    /// The first segment of the index's qualified names for this target.
    pub index_name: String,
}

impl Target {
    /// The flag that selects this target in `cargo test`, if one does.
    pub fn flag(&self) -> Option<Vec<String>> {
        Some(match self.kind.as_str() {
            "lib" | "proc-macro" | "rlib" | "dylib" | "cdylib" | "staticlib" => {
                vec!["--lib".into()]
            }
            "bin" => vec!["--bin".into(), self.name.clone()],
            "test" => vec!["--test".into(), self.name.clone()],
            "example" if self.test => vec!["--example".into(), self.name.clone()],
            _ => return None,
        })
    }

    /// The flag that builds this target. Plain `cargo test` compiles every
    /// example, even one whose tests are off, so an example always has one.
    pub fn build_flag(&self) -> Option<Vec<String>> {
        match self.kind.as_str() {
            "example" => Some(vec!["--example".into(), self.name.clone()]),
            _ => self.flag(),
        }
    }

    /// The target's identity within its package. A name alone is ambiguous:
    /// `api` can be both a library and a binary, and `summary` both an
    /// integration test and an example.
    pub fn id(&self) -> String {
        format!("{}:{}", self.kind, self.name)
    }

    pub fn is_lib(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "lib" | "proc-macro" | "rlib" | "dylib" | "cdylib" | "staticlib"
        )
    }

    /// How the plan names the target: `ledger-core --lib`, `api --test cli`.
    pub fn describe(&self, package: &str) -> String {
        match self.build_flag() {
            Some(flag) => format!("{package} {}", flag.join(" ")),
            None => format!("{package} {} {}", self.kind, self.name),
        }
    }
    /// The binary ID `cargo nextest` prints for this target within `package`.
    pub fn nextest_id(&self, package: &str) -> String {
        match self.kind.as_str() {
            "lib" | "proc-macro" | "rlib" | "dylib" | "cdylib" | "staticlib" => package.to_owned(),
            "bin" => format!("{package}::bin/{}", self.name),
            "test" => format!("{package}::{}", self.name),
            "example" => format!("{package}::example/{}", self.name),
            "bench" => format!("{package}::bench/{}", self.name),
            _ => format!("{package}::{}", self.name),
        }
    }
}

/// The Cargo option slice from a `cargo test` or `cargo nextest run` command,
/// before any `--`.
pub(crate) fn cargo_user_args(base: &[String]) -> Vec<String> {
    let words: Vec<&str> = base.iter().map(String::as_str).collect();
    let unwrapped = crate::filters::unwrap_tool(&words);
    let tail = match unwrapped {
        [prog, "nextest", "run" | "r", rest @ ..]
            if Path::new(prog).file_name().and_then(|n| n.to_str()) == Some("cargo") =>
        {
            rest
        }
        [prog, "test", rest @ ..]
            if Path::new(prog).file_name().and_then(|n| n.to_str()) == Some("cargo") =>
        {
            rest
        }
        _ => &words[2.min(words.len())..],
    };
    tail.iter()
        .take_while(|word| **word != "--")
        .map(|word| (*word).to_owned())
        .collect()
}

impl Package {
    /// The target with an identity from [`Target::id`].
    pub fn target_by_id(&self, id: &str) -> Option<&Target> {
        self.targets.iter().find(|target| target.id() == id)
    }
}

impl Cargo {
    pub fn package(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|package| package.name == name)
    }

    /// The member package that owns a workspace-relative file, unless a nested
    /// non-member package owns it first.
    pub fn owner(&self, file: &str) -> Option<&Package> {
        let member = self
            .packages
            .iter()
            .filter(|package| under(file, &package.dir))
            .max_by_key(|package| package.dir.len())?;
        let nested = self.outside.iter().any(|manifest| {
            let directory = parent(manifest);
            directory.len() > member.dir.len() && under(file, directory)
        });
        (!nested).then_some(member)
    }

    /// The package and target for an index qualified-name prefix.
    pub fn target(&self, index_name: &str) -> Option<(&Package, &Target)> {
        self.packages.iter().find_map(|package| {
            package
                .targets
                .iter()
                .find(|target| target.index_name == index_name)
                .map(|target| (package, target))
        })
    }

    /// Packages in scope that depend on any of `names`, directly or not, plus `names`.
    pub fn dependents(&self, names: &BTreeSet<String>) -> BTreeSet<String> {
        let mut found = names.clone();
        loop {
            let before = found.len();
            for package in &self.packages {
                if package.depends.iter().any(|name| found.contains(name)) {
                    found.insert(package.name.clone());
                }
            }
            if found.len() == before {
                return found;
            }
        }
    }

    /// Packages `name` depends on among members, transitively, plus itself.
    pub fn closure(&self, name: &str) -> BTreeSet<String> {
        let mut found = BTreeSet::from([name.to_owned()]);
        let mut queue = vec![name.to_owned()];
        while let Some(next) = queue.pop() {
            if let Some(package) = self.package(&next) {
                for dependency in &package.depends {
                    if found.insert(dependency.clone()) {
                        queue.push(dependency.clone());
                    }
                }
            }
        }
        found
    }
}

/// Features the user's cargo arguments enable for `package`.
pub(crate) fn enabled_features(package: &Package, args: &[String]) -> Option<BTreeSet<String>> {
    let mut all = false;
    let mut defaults = true;
    let mut requested = BTreeSet::new();
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--" => break,
            "--all-features" => all = true,
            "--no-default-features" => defaults = false,
            "--features" | "-F" => {
                if let Some(value) = words.next() {
                    requested.extend(split_features(value));
                }
            }
            other => {
                if let Some(value) = other
                    .strip_prefix("--features=")
                    .or_else(|| other.strip_prefix("-F"))
                {
                    requested.extend(split_features(value));
                }
            }
        }
    }
    if all {
        return None;
    }
    if defaults {
        requested.insert("default".into());
    }
    let mut enabled = BTreeSet::new();
    let mut queue: Vec<String> = requested
        .into_iter()
        .map(|feature| {
            // `package/feature` names a dependency's feature; keep the local part.
            feature
                .rsplit_once('/')
                .map_or(feature.clone(), |(_, local)| local.to_owned())
        })
        .collect();
    while let Some(feature) = queue.pop() {
        if !enabled.insert(feature.clone()) {
            continue;
        }
        for implied in package.features.get(&feature).into_iter().flatten() {
            if !implied.contains(':') && !implied.contains('/') {
                queue.push(implied.clone());
            }
        }
    }
    Some(enabled)
}

fn split_features(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split([',', ' '])
        .filter(|feature| !feature.is_empty())
        .map(str::to_owned)
}

/// Runs `cargo metadata --no-deps --offline` and reads harness and package
/// facts from the manifests it names.
fn cargo_facts(
    workspace: &Workspace,
    index: &Index,
    directory: &Path,
    manifest_path: Option<&str>,
) -> Result<Cargo> {
    let mut command = Command::new("cargo");
    command
        .args([
            "metadata",
            "--no-deps",
            "--offline",
            "--format-version",
            "1",
        ])
        .current_dir(directory);
    if let Some(path) = manifest_path {
        command.args(["--manifest-path", path]);
    }
    let output = command.output().context("run cargo metadata")?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        let line = message
            .lines()
            .find(|line| line.starts_with("error"))
            .or_else(|| message.lines().next())
            .unwrap_or("no output");
        bail!("cargo metadata failed: {}", line.trim());
    }
    let metadata: Value =
        serde_json::from_slice(&output.stdout).context("decode cargo metadata")?;
    let relative = |path: &str| -> Option<String> {
        let path = Path::new(path);
        let canonical = path.canonicalize().ok();
        let path = canonical.as_deref().unwrap_or(path);
        path.strip_prefix(&workspace.root)
            .ok()?
            .to_str()
            .map(str::to_owned)
    };
    let root_dir = metadata["workspace_root"].as_str().unwrap_or_default();
    let root_manifest = relative(&format!("{root_dir}/Cargo.toml"))
        .context("the Cargo workspace lies outside the Justless workspace")?;
    let members: BTreeSet<&str> = metadata["workspace_members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let defaults: Option<BTreeSet<&str>> = metadata["workspace_default_members"]
        .as_array()
        .map(|list| list.iter().filter_map(Value::as_str).collect());
    let mut packages = Vec::new();
    let mut default_names = BTreeSet::new();
    let mut names = BTreeSet::new();
    for package in metadata["packages"].as_array().into_iter().flatten() {
        let id = package["id"].as_str().unwrap_or_default();
        if !members.contains(id) {
            continue;
        }
        let name = package["name"].as_str().unwrap_or_default().to_owned();
        names.insert(name.clone());
        if defaults.as_ref().is_none_or(|list| list.contains(id)) {
            default_names.insert(name.clone());
        }
        let manifest = relative(package["manifest_path"].as_str().unwrap_or_default())
            .context("a Cargo package lies outside the Justless workspace")?;
        let dir = parent(&manifest).to_owned();
        let document = fs::read_to_string(workspace.root.join(&manifest))
            .ok()
            .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok());
        let rust_package = package_index_name(&name);
        let mut targets = Vec::new();
        for target in package["targets"].as_array().into_iter().flatten() {
            let kind = target["kind"]
                .as_array()
                .and_then(|kinds| kinds.first())
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let target_name = target["name"].as_str().unwrap_or_default().to_owned();
            let Some(src) = relative(target["src_path"].as_str().unwrap_or_default()) else {
                continue;
            };
            let section = match kind.as_str() {
                "bin" => Some("bin"),
                "test" => Some("test"),
                "example" => Some("example"),
                "bench" => Some("bench"),
                _ => None,
            };
            let declared = section.and_then(|section| {
                document
                    .as_ref()?
                    .get(section)?
                    .as_array_of_tables()?
                    .iter()
                    .find(|item| {
                        item.get("name").and_then(|v| v.as_str()) == Some(target_name.as_str())
                    })
                    .cloned()
            });
            let harness = if let Some(item) = &declared {
                item.get("harness")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true)
            } else if kind == "lib" || kind == "proc-macro" {
                document
                    .as_ref()
                    .and_then(|doc| doc.get("lib")?.get("harness")?.as_bool())
                    .unwrap_or(true)
            } else {
                true
            };
            let explicit = declared
                .as_ref()
                .is_some_and(|item| item.get("path").is_some());
            let within = src
                .strip_prefix(&dir)
                .map(|rest| rest.trim_start_matches('/'))
                .unwrap_or(&src)
                .to_owned();
            let index_name = match kind.as_str() {
                "lib" | "proc-macro" | "rlib" | "dylib" | "cdylib" | "staticlib" => {
                    target_name.replace('-', "_")
                }
                _ if explicit => target_name.replace('-', "_"),
                "custom-build" => format!("{rust_package}_build"),
                _ => auto_index_name(&rust_package, &within)
                    .unwrap_or_else(|| target_name.replace('-', "_")),
            };
            targets.push(Target {
                kind,
                name: target_name,
                src,
                required: target["required-features"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                test: target["test"].as_bool().unwrap_or(true),
                doctest: target["doctest"].as_bool().unwrap_or(false),
                harness,
                index_name,
            });
        }
        let depends = package["dependencies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|dependency| dependency["path"].is_string())
            .filter_map(|dependency| dependency["name"].as_str().map(str::to_owned))
            .collect();
        let features = package["features"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, implied)| {
                (
                    name.clone(),
                    implied
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                )
            })
            .collect();
        packages.push(Package {
            name,
            dir,
            manifest,
            targets,
            depends,
            features,
        });
    }
    for package in &mut packages {
        package.depends.retain(|name| names.contains(name));
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    // The base command tests the package in the directory it runs in, or the
    // default members.
    let cwd = directory
        .canonicalize()
        .unwrap_or_else(|_| directory.to_path_buf())
        .strip_prefix(&workspace.root)
        .ok()
        .and_then(Path::to_str)
        .unwrap_or("")
        .to_owned();
    let root_dir_relative = parent(&root_manifest).to_owned();
    let scope = match packages
        .iter()
        .filter(|package| package.dir != root_dir_relative && under(&cwd, &package.dir))
        .max_by_key(|package| package.dir.len())
    {
        Some(package) if manifest_path.is_none() => BTreeSet::from([package.name.clone()]),
        _ => default_names,
    };
    let member_manifests: BTreeSet<&str> = packages
        .iter()
        .map(|package| package.manifest.as_str())
        .collect();
    let outside = index
        .files
        .keys()
        .filter(|file| {
            file.ends_with("Cargo.toml") && (*file == "Cargo.toml" || file.ends_with("/Cargo.toml"))
        })
        .filter(|file| !member_manifests.contains(file.as_str()) && **file != root_manifest)
        .cloned()
        .collect();
    Ok(Cargo {
        packages,
        scope,
        outside,
        root_manifest,
    })
}

fn package_index_name(name: &str) -> String {
    name.replace('-', "_")
}

/// The index names targets Cargo discovers by location as `{package}_{kind}_{name}`.
fn auto_index_name(package: &str, within: &str) -> Option<String> {
    if within == "src/main.rs" {
        return Some(format!("{package}_bin"));
    }
    if within == "build.rs" {
        return Some(format!("{package}_build"));
    }
    for (prefix, kind) in [
        ("src/bin/", "bin"),
        ("tests/", "test"),
        ("examples/", "example"),
        ("benches/", "bench"),
    ] {
        let Some(rest) = within.strip_prefix(prefix) else {
            continue;
        };
        let name = match rest.split_once('/') {
            Some((name, "main.rs")) => name,
            None => rest.strip_suffix(".rs")?,
            _ => return None,
        };
        return Some(format!("{package}_{kind}_{name}"));
    }
    None
}

/// The runners for a base command, or for the project's markers without one.
pub(crate) fn detect(
    workspace: &Workspace,
    index: &Index,
    command: Option<&[String]>,
) -> Result<Vec<Runner>> {
    let cwd = workspace
        .cwd
        .strip_prefix(&workspace.root)
        .ok()
        .and_then(Path::to_str)
        .unwrap_or("")
        .to_owned();
    if let Some(command) = command {
        let mut runner = classify(workspace, index, &cwd, command);
        runner.cwd = Some(cwd);
        return Ok(vec![runner]);
    }
    let mut runners = Vec::new();
    // Cargo: the workspace root manifest, or each top-level package or workspace.
    let manifests: Vec<&String> = index
        .files
        .keys()
        .filter(|file| *file == "Cargo.toml" || file.ends_with("/Cargo.toml"))
        .collect();
    let tops: Vec<&String> = manifests
        .iter()
        .copied()
        .filter(|manifest| {
            let directory = parent(manifest);
            !manifests.iter().any(|other| {
                let outer = parent(other);
                outer.len() < directory.len() && under(directory, outer)
            })
        })
        .collect();
    for manifest in tops {
        let directory = parent(manifest).to_owned();
        let mut runner = Runner::new(Kind::Cargo, directory.clone(), words(&["cargo", "test"]));
        match cargo_facts(workspace, index, &workspace.root.join(&directory), None) {
            Ok(facts) => {
                runner.root = parent(&facts.root_manifest).to_owned();
                runner.cargo = Some(facts);
            }
            Err(error) => runner.error = Some(format!("{error:#}")),
        }
        runners.push(runner);
    }
    // pytest: configuration files, or test files at the root without one.
    let mut pytest_roots = BTreeSet::new();
    for file in index.files.keys() {
        let name = file.rsplit('/').next().unwrap_or(file);
        let marker = match name {
            "pytest.ini" => true,
            "pyproject.toml" => contains(workspace, file, "[tool.pytest"),
            "setup.cfg" => contains(workspace, file, "[tool:pytest]"),
            "tox.ini" => contains(workspace, file, "[pytest]"),
            _ => false,
        };
        if marker {
            pytest_roots.insert(parent(file).to_owned());
        }
    }
    // Without configuration, only test files where pytest's own discovery from
    // the root would start count: at the root or in a top-level tests directory.
    // A stray script elsewhere, such as scripts/test_tool.py, is not a suite.
    if pytest_roots.is_empty()
        && index.files.keys().any(|file| {
            let name = file.rsplit('/').next().unwrap_or(file);
            let place =
                !file.contains('/') || file.starts_with("tests/") || file.starts_with("test/");
            place
                && file.ends_with(".py")
                && (name == "conftest.py"
                    || name.starts_with("test_")
                    || name.ends_with("_test.py"))
        })
    {
        pytest_roots.insert(String::new());
    }
    for root in pytest_roots {
        let venv = workspace.root.join(&root).join(".venv/bin/python");
        let base = if venv.is_file() {
            words(&[".venv/bin/python", "-m", "pytest"])
        } else {
            words(&["python3", "-m", "pytest"])
        };
        let mut runner = Runner::new(Kind::Pytest, root, base);
        pytest_configure(workspace, &mut runner);
        runners.push(runner);
    }
    // JavaScript/TypeScript runners: Jest, Vitest, Bun test, Node --test, Playwright.
    let mut js_roots = BTreeSet::new();
    for file in index.files.keys() {
        if !(file == "package.json" || file.ends_with("/package.json")) {
            continue;
        }
        let directory = parent(file).to_owned();
        let Ok(text) = fs::read_to_string(workspace.root.join(file)) else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let listed = |tool: &str| {
            ["dependencies", "devDependencies"]
                .iter()
                .any(|key| json[key].get(tool).is_some())
        };
        let configured = |prefix: &str| {
            index.files.keys().any(|other| {
                parent(other) == directory
                    && other
                        .rsplit('/')
                        .next()
                        .is_some_and(|name| name.starts_with(prefix))
            })
        };
        let has_file = |name: &str| {
            let path = if directory.is_empty() {
                name.to_owned()
            } else {
                format!("{directory}/{name}")
            };
            index.files.contains_key(&path)
        };
        let jest = listed("jest") || json.get("jest").is_some() || configured("jest.config.");
        let vitest = listed("vitest") || configured("vitest.config.");
        match (jest, vitest) {
            (true, true) => bail!(
                "{file} lists both jest and vitest; pass the runner after --, e.g. {} test --affected -- npx vitest run",
                workspace.justless()
            ),
            (true, false) | (false, true) => {
                let kind = if jest { Kind::Jest } else { Kind::Vitest };
                let base = match kind {
                    Kind::Jest => words(&["node_modules/.bin/jest"]),
                    _ => words(&["node_modules/.bin/vitest", "run"]),
                };
                let mut runner = Runner::new(kind, directory.clone(), base);
                if !workspace
                    .root
                    .join(&directory)
                    .join(&runner.base[0])
                    .is_file()
                {
                    runner.error = Some(format!(
                        "{} is not installed in {}",
                        runner.base[0],
                        if directory.is_empty() {
                            "."
                        } else {
                            &directory
                        }
                    ));
                }
                js_roots.insert(directory);
                runners.push(runner);
                continue;
            }
            (false, false) => {}
        }
        // Check scripts.test for explicit `bun test`, `node --test`, or `playwright test`.
        if let Some(script) = json["scripts"]["test"].as_str()
            && let Some(mut runner) = detect_script_runner(workspace, &directory, script)
        {
            if runner.kind == Kind::Playwright
                && runner
                    .base
                    .first()
                    .is_some_and(|b| b.starts_with("node_modules/"))
                && !workspace
                    .root
                    .join(&directory)
                    .join(&runner.base[0])
                    .is_file()
            {
                runner.error = Some(format!(
                    "{} is not installed in {}",
                    runner.base[0],
                    if directory.is_empty() {
                        "."
                    } else {
                        &directory
                    }
                ));
            }
            js_roots.insert(directory);
            runners.push(runner);
            continue;
        }
        // Bun test via bunfig.toml `[test]` or Bun lockfile + Bun types with test files.
        let bunfig_path = if directory.is_empty() {
            "bunfig.toml".to_owned()
        } else {
            format!("{directory}/bunfig.toml")
        };
        let bunfig_tests =
            index.files.contains_key(&bunfig_path) && contains(workspace, &bunfig_path, "[test]");
        let bun_project =
            (has_file("bun.lock") || has_file("bun.lockb") || has_file("bunfig.toml"))
                && (listed("@types/bun") || listed("bun-types"))
                && index
                    .files
                    .keys()
                    .any(|other| under(other, &directory) && is_bun_test_file(other));
        if bunfig_tests || bun_project {
            js_roots.insert(directory.clone());
            runners.push(Runner::new(Kind::Bun, directory, words(&["bun", "test"])));
        }
    }
    // Standalone bunfig.toml with [test] and no package.json in the same directory.
    for file in index.files.keys() {
        if !(file == "bunfig.toml" || file.ends_with("/bunfig.toml")) {
            continue;
        }
        let directory = parent(file).to_owned();
        if !js_roots.contains(&directory) && contains(workspace, file, "[test]") {
            js_roots.insert(directory.clone());
            runners.push(Runner::new(Kind::Bun, directory, words(&["bun", "test"])));
        }
    }
    // Deno: `deno.json` or `deno.jsonc` alongside at least one Deno test file.
    let mut deno_roots = BTreeSet::new();
    for file in index.files.keys() {
        let name = file.rsplit('/').next().unwrap_or(file);
        if matches!(name, "deno.json" | "deno.jsonc") {
            let directory = parent(file).to_owned();
            if !js_roots.contains(&directory)
                && index
                    .files
                    .keys()
                    .any(|other| under(other, &directory) && is_deno_test_file(other))
            {
                deno_roots.insert(directory);
            }
        }
    }
    for root in deno_roots {
        runners.push(Runner::new(Kind::DenoTest, root, words(&["deno", "test"])));
    }
    // Dart / Flutter: each `pubspec.yaml` package with at least one `test/*_test.dart`.
    let mut dart_roots = BTreeSet::new();
    for file in index.files.keys() {
        if !(file == "pubspec.yaml" || file.ends_with("/pubspec.yaml")) {
            continue;
        }
        let directory = parent(file).to_owned();
        if index
            .files
            .keys()
            .any(|other| is_dart_test_file(&directory, other))
        {
            dart_roots.insert((directory, file.as_str()));
        }
    }
    for (root, pubspec) in dart_roots {
        let base = if is_flutter_pubspec(workspace, pubspec) {
            words(&["flutter", "test"])
        } else {
            words(&["dart", "test"])
        };
        runners.push(Runner::new(Kind::DartTest, root, base));
    }
    // Ruby RSpec: `.rspec` or a Gemfile listing `rspec`, alongside at least one `*_spec.rb`.
    let mut rspec_roots = BTreeSet::new();
    for file in index.files.keys() {
        let name = file.rsplit('/').next().unwrap_or(file);
        let marker = name == ".rspec" || (name == "Gemfile" && contains(workspace, file, "rspec"));
        if marker {
            let directory = parent(file).to_owned();
            if index
                .files
                .keys()
                .any(|other| under(other, &directory) && other.ends_with("_spec.rb"))
            {
                rspec_roots.insert(directory);
            }
        }
    }
    for root in rspec_roots {
        let gemfile = if root.is_empty() {
            "Gemfile".to_owned()
        } else {
            format!("{root}/Gemfile")
        };
        let base = if index.files.contains_key(&gemfile) {
            words(&["bundle", "exec", "rspec"])
        } else {
            words(&["rspec"])
        };
        runners.push(Runner::new(Kind::Rspec, root, base));
    }
    // Go: each module.
    for file in index.files.keys() {
        if file == "go.mod" || file.ends_with("/go.mod") {
            runners.push(Runner::new(
                Kind::Go,
                parent(file).to_owned(),
                words(&["go", "test", "./..."]),
            ));
        }
    }
    // A Go module directory may hold no indexed go.mod if it is ignored; nothing to do.
    if runners.is_empty() {
        bail!(
            "no test runner detected; pass one after --, e.g. {} test --affected -- cargo test",
            workspace.justless()
        );
    }
    runners.sort_by(|a, b| (a.kind, &a.root).cmp(&(b.kind, &b.root)));
    Ok(runners)
}

fn words(list: &[&str]) -> Vec<String> {
    list.iter().map(|word| (*word).to_owned()).collect()
}

fn contains(workspace: &Workspace, file: &str, needle: &str) -> bool {
    fs::read_to_string(workspace.root.join(file)).is_ok_and(|text| text.contains(needle))
}

/// Whether `file` matches Bun's default test file discovery conventions.
pub(crate) fn is_bun_test_file(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    let Some(stem) = [".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"]
        .iter()
        .find_map(|ext| name.strip_suffix(ext))
    else {
        return false;
    };
    stem.ends_with(".test")
        || stem.ends_with(".spec")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
}

/// Whether `file` matches Node's `node --test` default discovery conventions.
pub(crate) fn is_node_test_file(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    let Some(stem) = [".js", ".mjs", ".cjs", ".ts", ".mts", ".cts", ".jsx", ".tsx"]
        .iter()
        .find_map(|ext| name.strip_suffix(ext))
    else {
        return false;
    };
    let in_test_dir = Path::new(file)
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .any(|part| part.as_os_str().to_str() == Some("test"));
    in_test_dir
        || stem == "test"
        || stem.starts_with("test-")
        || stem.ends_with(".test")
        || stem.ends_with("-test")
        || stem.ends_with("_test")
}

/// Whether `file` matches Playwright's / Vitest's default `*.test.*` or `*.spec.*` pattern.
pub(crate) fn is_dot_test_or_spec_file(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    let Some(stem) = [".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs"]
        .iter()
        .find_map(|ext| name.strip_suffix(ext))
    else {
        return false;
    };
    stem.ends_with(".test") || stem.ends_with(".spec")
}

/// Whether `file` matches Deno's default `deno test` discovery conventions
/// (`*_test.<ext>`, `*.test.<ext>`, or `test.<ext>`).
pub(crate) fn is_deno_test_file(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    let Some(stem) = [".ts", ".tsx", ".mts", ".js", ".mjs", ".jsx", ".cjs", ".cts"]
        .iter()
        .find_map(|ext| name.strip_suffix(ext))
    else {
        return false;
    };
    stem == "test" || stem.ends_with("_test") || stem.ends_with(".test")
}

/// Whether `file` is a Dart/Flutter test file under `<root>/test/` (`*_test.dart`).
pub(crate) fn is_dart_test_file(root: &str, file: &str) -> bool {
    if !under(file, root) || !file.ends_with("_test.dart") {
        return false;
    }
    let relative = if root.is_empty() {
        file
    } else {
        file.strip_prefix(root)
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(file)
    };
    relative.starts_with("test/")
}

/// Whether `file` looks like a Dart/Flutter test file (`test/.../*_test.dart`).
pub(crate) fn is_dart_test_path(file: &str) -> bool {
    file.ends_with("_test.dart") && (file.starts_with("test/") || file.contains("/test/"))
}

/// Whether a `pubspec.yaml` declares a Flutter SDK dependency (`sdk: flutter`).
pub(crate) fn is_flutter_pubspec(workspace: &Workspace, pubspec: &str) -> bool {
    let Ok(text) = fs::read_to_string(workspace.root.join(pubspec)) else {
        return false;
    };
    text.lines().any(|raw| {
        let line = raw.split('#').next().unwrap_or(raw).trim();
        line.strip_prefix("sdk:")
            .is_some_and(|rest| rest.trim().trim_matches(['"', '\'']) == "flutter")
    })
}

/// The runner behind a command the user gave after `--`.
fn classify(workspace: &Workspace, index: &Index, cwd: &str, command: &[String]) -> Runner {
    let all: Vec<&str> = command.iter().map(String::as_str).collect();
    let unwrapped = crate::filters::unwrap_tool(&all);
    let offset = all.len() - unwrapped.len();
    let program = unwrapped
        .first()
        .map(|word| {
            Path::new(word)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(word)
        })
        .unwrap_or("");
    let rest = &unwrapped[1.min(unwrapped.len())..];
    let base = command.to_vec();
    let mut runner = match (program, rest) {
        ("cargo", ["test", tail @ ..]) => {
            let mut runner = Runner::new(Kind::Cargo, cwd.to_owned(), base);
            runner.selects = cargo_selects(tail);
            let manifest_path = flag_value(tail, "--manifest-path");
            match cargo_facts(workspace, index, &workspace.cwd, manifest_path.as_deref()) {
                Ok(facts) => runner.cargo = Some(facts),
                Err(error) => runner.error = Some(format!("{error:#}")),
            }
            runner
        }
        ("cargo", ["nextest", "run" | "r", tail @ ..]) => {
            if has_flag(tail, &["--list"]) {
                let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                runner.unknown = Some("cargo nextest --list".into());
                return runner;
            }
            let mut runner = Runner::new(Kind::Nextest, cwd.to_owned(), base);
            runner.selects = nextest_selects(tail);
            let manifest_path = flag_value(tail, "--manifest-path");
            match cargo_facts(workspace, index, &workspace.cwd, manifest_path.as_deref()) {
                Ok(facts) => runner.cargo = Some(facts),
                Err(error) => runner.error = Some(format!("{error:#}")),
            }
            runner
        }
        ("cargo", ["nextest", ..]) => {
            let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
            runner.unknown = Some("cargo nextest".into());
            runner
        }
        ("pytest" | "py.test", tail) => {
            let mut runner = Runner::new(Kind::Pytest, cwd.to_owned(), base);
            runner.selects = pytest_selects(tail);
            runner
        }
        ("python" | "python3", ["-m", "pytest", tail @ ..]) => {
            let mut runner = Runner::new(Kind::Pytest, cwd.to_owned(), base);
            runner.selects = pytest_selects(tail);
            runner
        }
        ("jest", tail) => {
            if has_flag(tail, &["--watch", "--watchAll"]) {
                let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                runner.unknown = Some("jest in watch mode".into());
                return runner;
            }
            if has_flag(tail, &["--listTests", "--showConfig"]) {
                let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                runner.unknown = Some("jest listing/config mode".into());
                return runner;
            }
            let mut runner = Runner::new(Kind::Jest, cwd.to_owned(), base);
            runner.selects = js_selects(tail);
            runner
        }
        ("vitest", tail) => {
            let tail = match tail {
                ["run", tail @ ..] => tail,
                ["watch" | "dev", ..] => {
                    let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                    runner.unknown = Some("vitest in watch mode".into());
                    return runner;
                }
                ["bench" | "list" | "typecheck" | "init", ..] => {
                    let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                    runner.unknown = Some(format!("vitest {}", tail[0]));
                    return runner;
                }
                _ => tail,
            };
            if has_flag(tail, &["--watch", "-w"]) {
                let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                runner.unknown = Some("vitest in watch mode".into());
                return runner;
            }
            if has_flag(tail, &["--ui"]) {
                let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                runner.unknown = Some("vitest in --ui mode".into());
                return runner;
            }
            let mut runner = Runner::new(Kind::Vitest, cwd.to_owned(), base);
            runner.selects = js_selects(tail);
            if !runner.base[offset..]
                .iter()
                .any(|word| word == "run" || word == "--run")
            {
                // Without `run` Vitest watches; insert it so the step ends.
                runner.base.insert(offset + 1, "run".into());
            }
            runner
        }
        ("bun", ["test", tail @ ..]) => {
            if has_flag(tail, &["--watch"]) {
                let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                runner.unknown = Some("bun test in watch mode".into());
                return runner;
            }
            let mut runner = Runner::new(Kind::Bun, cwd.to_owned(), base);
            runner.selects = bun_selects(tail);
            runner
        }
        ("node", tail) if is_node_test_command(tail) => {
            if has_flag(tail, &["--watch"]) {
                let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                runner.unknown = Some("node --test in watch mode".into());
                return runner;
            }
            let mut runner = Runner::new(Kind::NodeTest, cwd.to_owned(), base);
            runner.selects = node_test_selects(tail);
            runner
        }
        ("playwright", ["test", tail @ ..]) => {
            if has_flag(tail, &["--ui", "--debug"]) {
                let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                runner.unknown = Some("playwright test interactive mode".into());
                return runner;
            }
            if has_flag(tail, &["--list"]) {
                let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                runner.unknown = Some("playwright test --list".into());
                return runner;
            }
            let mut runner = Runner::new(Kind::Playwright, cwd.to_owned(), base);
            runner.selects = playwright_selects(tail);
            runner
        }
        ("deno", ["test", tail @ ..]) => {
            if has_flag(tail, &["--watch", "--watch-hmr"]) {
                let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                runner.unknown = Some("deno test in watch mode".into());
                return runner;
            }
            if has_flag(tail, &["--doc", "--no-run", "--help", "-h"]) {
                let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                runner.unknown = Some("deno test non-subsettable mode".into());
                return runner;
            }
            let mut runner = Runner::new(Kind::DenoTest, cwd.to_owned(), base);
            runner.selects = deno_selects(tail);
            runner
        }
        ("dart" | "flutter", ["test", tail @ ..]) => {
            if has_flag(
                tail,
                &["--help", "-h", "--pause-after-load", "--start-paused"],
            ) {
                let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                runner.unknown = Some(format!("{program} test interactive/help mode"));
                return runner;
            }
            let mut runner = Runner::new(Kind::DartTest, cwd.to_owned(), base);
            runner.selects = dart_selects(tail);
            runner
        }
        ("rspec", tail) => {
            if has_flag(tail, &["--dry-run", "--init"]) {
                let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                runner.unknown = Some("rspec non-run mode".into());
                return runner;
            }
            let mut runner = Runner::new(Kind::Rspec, cwd.to_owned(), base);
            runner.selects = rspec_selects(tail);
            runner
        }
        ("go", ["test", tail @ ..]) => {
            let mut runner = Runner::new(Kind::Go, cwd.to_owned(), base);
            runner.selects = go_selects(tail);
            runner
        }
        ("npm" | "pnpm" | "yarn", ["test"] | ["run", "test"] | ["t"])
        | ("bun", ["run", "test"] | ["t"]) => script_runner(workspace, cwd, program, base),
        _ => {
            let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
            runner.unknown = Some(
                crate::history::label(&all)
                    .split(' ')
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            runner
        }
    };
    if runner.kind.is_cargo()
        && let Some(facts) = &runner.cargo
    {
        runner.root = parent(&facts.root_manifest).to_owned();
    }
    if runner.kind == Kind::Pytest {
        pytest_configure(workspace, &mut runner);
    }
    runner
}

fn has_flag(tail: &[&str], flags: &[&str]) -> bool {
    tail.iter().take_while(|word| **word != "--").any(|word| {
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        flags.contains(&flag)
    })
}

/// Reads pytest's options from the nearest configuration at or above the root.
fn pytest_configure(workspace: &Workspace, runner: &mut Runner) {
    let mut directory = runner.root.clone();
    loop {
        if let Some((files, paths)) = pytest_options(workspace, &directory) {
            runner.python_files = files;
            runner.testpaths = paths;
            return;
        }
        if directory.is_empty() {
            return;
        }
        directory = parent(&directory).to_owned();
    }
}

/// Parses a plain `package.json` `scripts.test` string into a known JS runner
/// (for auto-detection when neither Jest nor Vitest is installed directly).
fn detect_script_runner(_workspace: &Workspace, directory: &str, script: &str) -> Option<Runner> {
    let plain = !script.contains(['&', '|', ';', '>', '<', '$', '`', '(', '"', '\''])
        && !script.split_whitespace().any(|word| word.contains('='));
    if !plain {
        return None;
    }
    let parts = shlex::split(script)?;
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    let unwrapped = crate::filters::unwrap_tool(&refs);
    match unwrapped {
        ["bun", "test", tail @ ..] if !has_flag(tail, &["--watch"]) => {
            let mut command = words(&["bun", "test"]);
            command.extend(tail.iter().map(|s| (*s).to_owned()));
            let mut runner = Runner::new(Kind::Bun, directory.to_owned(), command);
            runner.selects = bun_selects(tail);
            Some(runner)
        }
        ["node", tail @ ..] if is_node_test_command(tail) && !has_flag(tail, &["--watch"]) => {
            let mut command = vec!["node".to_owned()];
            command.extend(tail.iter().map(|s| (*s).to_owned()));
            let mut runner = Runner::new(Kind::NodeTest, directory.to_owned(), command);
            runner.selects = node_test_selects(tail);
            Some(runner)
        }
        ["playwright", "test", tail @ ..] if !has_flag(tail, &["--ui", "--debug", "--list"]) => {
            let mut command = words(&["node_modules/.bin/playwright", "test"]);
            command.extend(tail.iter().map(|s| (*s).to_owned()));
            let mut runner = Runner::new(Kind::Playwright, directory.to_owned(), command);
            runner.selects = playwright_selects(tail);
            Some(runner)
        }
        ["deno", "test", tail @ ..]
            if !has_flag(
                tail,
                &[
                    "--watch",
                    "--watch-hmr",
                    "--doc",
                    "--no-run",
                    "--help",
                    "-h",
                ],
            ) =>
        {
            let mut command = words(&["deno", "test"]);
            command.extend(tail.iter().map(|s| (*s).to_owned()));
            let mut runner = Runner::new(Kind::DenoTest, directory.to_owned(), command);
            runner.selects = deno_selects(tail);
            Some(runner)
        }
        _ => None,
    }
}

/// `npm test` and friends: a plain `jest`, `vitest`, `bun test`, `node --test`,
/// or `playwright test` script is resolved directly with its flags; anything
/// else is run unchanged.
fn script_runner(workspace: &Workspace, cwd: &str, program: &str, base: Vec<String>) -> Runner {
    let script = fs::read_to_string(workspace.root.join(cwd).join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|json| json["scripts"]["test"].as_str().map(str::to_owned));
    let plain = script.as_deref().filter(|script| {
        !script.contains(['&', '|', ';', '>', '<', '$', '`', '(', '"', '\''])
            && !script.split_whitespace().any(|word| word.contains('='))
    });
    if let Some(script) = plain
        && let Some(parts) = shlex::split(script)
    {
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        let unwrapped = crate::filters::unwrap_tool(&refs);
        let binary = |name: &str| format!("node_modules/.bin/{name}");
        match unwrapped {
            ["jest", tail @ ..] => {
                if has_flag(
                    tail,
                    &["--watch", "--watchAll", "--listTests", "--showConfig"],
                ) {
                    let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                    runner.unknown = Some(format!("{program} test (jest watch/list)"));
                    return runner;
                }
                let mut command = vec![binary("jest")];
                command.extend(tail.iter().map(|s| (*s).to_owned()));
                let mut runner = Runner::new(Kind::Jest, cwd.to_owned(), command);
                runner.selects = js_selects(tail);
                return runner;
            }
            ["vitest", tail @ ..] => {
                let mut owned_tail: Vec<String> = tail.iter().map(|s| (*s).to_owned()).collect();
                if owned_tail.first().map(String::as_str) == Some("run") {
                    owned_tail.remove(0);
                } else if matches!(
                    owned_tail.first().map(String::as_str),
                    Some("watch" | "dev" | "bench" | "list" | "typecheck" | "init")
                ) {
                    let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                    runner.unknown = Some(format!("{program} test (vitest watch)"));
                    return runner;
                }
                let tail_refs: Vec<&str> = owned_tail.iter().map(String::as_str).collect();
                if has_flag(&tail_refs, &["--watch", "-w", "--ui"]) {
                    let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                    runner.unknown = Some(format!("{program} test (vitest watch)"));
                    return runner;
                }
                let mut command = vec![binary("vitest"), "run".into()];
                command.extend(owned_tail.iter().cloned());
                let mut runner = Runner::new(Kind::Vitest, cwd.to_owned(), command);
                runner.selects = js_selects(&tail_refs);
                return runner;
            }
            ["bun", "test", tail @ ..] => {
                if has_flag(tail, &["--watch"]) {
                    let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                    runner.unknown = Some(format!("{program} test (bun test watch)"));
                    return runner;
                }
                let mut command = words(&["bun", "test"]);
                command.extend(tail.iter().map(|s| (*s).to_owned()));
                let mut runner = Runner::new(Kind::Bun, cwd.to_owned(), command);
                runner.selects = bun_selects(tail);
                return runner;
            }
            ["node", tail @ ..] if is_node_test_command(tail) => {
                if has_flag(tail, &["--watch"]) {
                    let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                    runner.unknown = Some(format!("{program} test (node --test watch)"));
                    return runner;
                }
                let mut command = vec!["node".to_owned()];
                command.extend(tail.iter().map(|s| (*s).to_owned()));
                let mut runner = Runner::new(Kind::NodeTest, cwd.to_owned(), command);
                runner.selects = node_test_selects(tail);
                return runner;
            }
            ["playwright", "test", tail @ ..] => {
                if has_flag(tail, &["--ui", "--debug", "--list"]) {
                    let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                    runner.unknown = Some(format!("{program} test (playwright interactive)"));
                    return runner;
                }
                let mut command = vec![binary("playwright"), "test".into()];
                command.extend(tail.iter().map(|s| (*s).to_owned()));
                let mut runner = Runner::new(Kind::Playwright, cwd.to_owned(), command);
                runner.selects = playwright_selects(tail);
                return runner;
            }
            ["deno", "test", tail @ ..] => {
                if has_flag(
                    tail,
                    &[
                        "--watch",
                        "--watch-hmr",
                        "--doc",
                        "--no-run",
                        "--help",
                        "-h",
                    ],
                ) {
                    let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                    runner.unknown = Some(format!("{program} test (deno test)"));
                    return runner;
                }
                let mut command = words(&["deno", "test"]);
                command.extend(tail.iter().map(|s| (*s).to_owned()));
                let mut runner = Runner::new(Kind::DenoTest, cwd.to_owned(), command);
                runner.selects = deno_selects(tail);
                return runner;
            }
            _ => {}
        }
    }
    let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
    runner.unknown = Some(format!("{program} test"));
    runner
}

fn flag_value(words: &[&str], flag: &str) -> Option<String> {
    let mut iter = words.iter();
    while let Some(word) = iter.next() {
        if *word == "--" {
            return None;
        }
        if *word == flag {
            return iter.next().map(|value| (*value).to_owned());
        }
        if let Some(value) = word.strip_prefix(&format!("{flag}=")) {
            return Some(value.to_owned());
        }
    }
    None
}

/// Cargo options that take a separate value.
const CARGO_VALUES: &[&str] = &[
    "--features",
    "-F",
    "--target",
    "--target-dir",
    "-j",
    "--jobs",
    "--manifest-path",
    "--profile",
    "--color",
    "--message-format",
    "--config",
    "-Z",
    "--lockfile-path",
];

const CARGO_SELECTS: &[&str] = &[
    "-p",
    "--package",
    "--workspace",
    "--all",
    "--exclude",
    "--lib",
    "--bin",
    "--bins",
    "--test",
    "--tests",
    "--example",
    "--examples",
    "--bench",
    "--benches",
    "--all-targets",
    "--doc",
];

/// libtest options that take a separate value.
const LIBTEST_VALUES: &[&str] = &[
    "--test-threads",
    "--color",
    "--format",
    "-Z",
    "--logfile",
    "--report-time",
    "--shuffle-seed",
];

const LIBTEST_SELECTS: &[&str] = &["--exact", "--skip", "--ignored", "--include-ignored"];

fn cargo_selects(tail: &[&str]) -> Option<String> {
    let mut words = tail.iter();
    while let Some(word) = words.next() {
        if *word == "--" {
            break;
        }
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if CARGO_SELECTS.contains(&flag) || word.starts_with("-p") && word.len() > 2 {
            return Some((*word).to_owned());
        }
        if CARGO_VALUES.contains(word) {
            words.next();
            continue;
        }
        if !word.starts_with('-') {
            return Some((*word).to_owned());
        }
    }
    // After `--`, libtest filters or selection flags mean the command picks tests.
    while let Some(word) = words.next() {
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if LIBTEST_SELECTS.contains(&flag) {
            return Some((*word).to_owned());
        }
        if LIBTEST_VALUES.contains(word) {
            words.next();
            continue;
        }
        if !word.starts_with('-') {
            return Some((*word).to_owned());
        }
    }
    None
}

/// `cargo nextest run` options that take a separate value.
const NEXTEST_VALUES: &[&str] = &[
    "-P",
    "--profile",
    "--cargo-profile",
    "--build-jobs",
    "--config-file",
    "--user-config-file",
    "--tool-config-file",
    "--override-version-check",
    "--test-threads",
    "--retries",
    "--status-level",
    "--final-status-level",
    "--failure-output",
    "--success-output",
    "--no-tests",
    "--hide-progress-bar",
    "--show-progress",
    "--max-fail",
    "--workspace-remap",
    "--target-dir-remap",
    "--cargo-metadata",
    "--binaries-metadata",
    "--extract-to",
    "--cargo-message-format",
    "--message-format-version",
    "--stress-count",
    "--stress-duration",
    "--target-runner",
];

const NEXTEST_SELECTS: &[&str] = &[
    "-E",
    "--filterset",
    "--filter-expr",
    "--default-filter",
    "--run-ignored",
    "--partition",
    "--archive-file",
    "--rerun",
];

fn nextest_selects(tail: &[&str]) -> Option<String> {
    let mut words = tail.iter();
    while let Some(word) = words.next() {
        if *word == "--" {
            break;
        }
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if CARGO_SELECTS.contains(&flag)
            || NEXTEST_SELECTS.contains(&flag)
            || (word.starts_with("-p") && word.len() > 2)
            || (word.starts_with("-E") && word.len() > 2)
        {
            return Some((*word).to_owned());
        }
        if CARGO_VALUES.contains(word) || NEXTEST_VALUES.contains(word) {
            words.next();
            continue;
        }
        if !word.starts_with('-') {
            return Some((*word).to_owned());
        }
    }
    while let Some(word) = words.next() {
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if LIBTEST_SELECTS.contains(&flag) {
            return Some((*word).to_owned());
        }
        if LIBTEST_VALUES.contains(word) {
            words.next();
            continue;
        }
        if !word.starts_with('-') {
            return Some((*word).to_owned());
        }
    }
    None
}

const BUN_VALUES: &[&str] = &[
    "-t",
    "--test-name-pattern",
    "--timeout",
    "--concurrency",
    "--max-concurrency",
    "--rerun-each",
    "--reporter",
    "--reporter-outfile",
    "--preload",
    "-r",
    "--cwd",
    "--config",
    "-c",
    "--env-file",
    "--seed",
    "--coverage-reporter",
    "--coverage-dir",
    "--coverage-threshold",
];

fn bun_selects(tail: &[&str]) -> Option<String> {
    let mut words = tail.iter().peekable();
    while let Some(word) = words.next() {
        if *word == "--" {
            if let Some(next) = words.next() {
                return Some((*next).to_owned());
            }
            break;
        }
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if matches!(flag, "--only" | "--todo") {
            return Some((*word).to_owned());
        }
        if *word == "--bail" {
            if words
                .peek()
                .is_some_and(|next| !next.is_empty() && next.bytes().all(|b| b.is_ascii_digit()))
            {
                words.next();
            }
            continue;
        }
        if BUN_VALUES.contains(word) {
            words.next();
            continue;
        }
        if !word.starts_with('-') {
            return Some((*word).to_owned());
        }
    }
    None
}

const NODE_TEST_VALUES: &[&str] = &[
    "--test-reporter",
    "--test-reporter-destination",
    "--test-concurrency",
    "--test-timeout",
    "--test-coverage-branches",
    "--test-coverage-functions",
    "--test-coverage-lines",
    "--test-coverage-include",
    "--test-coverage-exclude",
    "--test-global-setup",
    "--test-rerun-failures",
    "--import",
    "-r",
    "--require",
    "--loader",
    "--experimental-loader",
    "--conditions",
    "-C",
    "--env-file",
    "--env-file-if-exists",
    "--max-old-space-size",
    "--cpu-prof-dir",
    "--diagnostic-dir",
];

fn is_node_test_command(tail: &[&str]) -> bool {
    let mut saw_test = false;
    let mut words = tail.iter();
    while let Some(word) = words.next() {
        if *word == "--" {
            break;
        }
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if matches!(flag, "-e" | "--eval" | "-p" | "--print" | "-c" | "--check") {
            return false;
        }
        if *word == "--test" {
            saw_test = true;
            continue;
        }
        if NODE_TEST_VALUES.contains(word) {
            words.next();
        }
    }
    saw_test
}

fn node_test_selects(tail: &[&str]) -> Option<String> {
    let mut words = tail.iter();
    while let Some(word) = words.next() {
        if *word == "--" {
            if let Some(next) = words.next() {
                return Some((*next).to_owned());
            }
            break;
        }
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if matches!(
            flag,
            "--test-name-pattern" | "--test-skip-pattern" | "--test-only" | "--test-shard"
        ) {
            return Some((*word).to_owned());
        }
        if NODE_TEST_VALUES.contains(word) {
            words.next();
            continue;
        }
        if !word.starts_with('-') {
            return Some((*word).to_owned());
        }
    }
    None
}

const PLAYWRIGHT_VALUES: &[&str] = &[
    "-c",
    "--config",
    "--project",
    "--reporter",
    "-j",
    "--workers",
    "--retries",
    "--timeout",
    "--global-timeout",
    "--max-failures",
    "--output",
    "--trace",
    "--repeat-each",
    "--tsconfig",
    "--fail-on-flaky-tests",
];

fn playwright_selects(tail: &[&str]) -> Option<String> {
    let mut words = tail.iter();
    while let Some(word) = words.next() {
        if *word == "--" {
            if let Some(next) = words.next() {
                return Some((*next).to_owned());
            }
            break;
        }
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if matches!(
            flag,
            "-g" | "--grep" | "--grep-invert" | "--shard" | "--last-failed" | "--only-changed"
        ) {
            return Some((*word).to_owned());
        }
        if PLAYWRIGHT_VALUES.contains(word) {
            words.next();
            continue;
        }
        if !word.starts_with('-') {
            return Some((*word).to_owned());
        }
    }
    None
}

const DENO_VALUES: &[&str] = &[
    "-c",
    "--config",
    "--import-map",
    "--lock",
    "--cert",
    "--seed",
    "--reporter",
    "--junit-path",
    "--location",
    "--env-file",
    "--v8-flags",
    "--preload",
    "--conditions",
    "-L",
    "--log-level",
    "-j",
    "--jobs",
    "--ext",
];

fn deno_selects(tail: &[&str]) -> Option<String> {
    let mut words = tail.iter();
    while let Some(word) = words.next() {
        // In `deno test [OPTIONS] [FILES...] [-- SCRIPT_ARGS...]`, anything
        // after `--` is forwarded to `Deno.args`, not treated as test files.
        if *word == "--" {
            break;
        }
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if matches!(flag, "--filter" | "--ignore") {
            return Some((*word).to_owned());
        }
        if DENO_VALUES.contains(word) {
            words.next();
            continue;
        }
        if !word.starts_with('-') {
            return Some((*word).to_owned());
        }
    }
    None
}

const DART_VALUES: &[&str] = &[
    "-r",
    "--reporter",
    "-p",
    "--platform",
    "-P",
    "--preset",
    "-j",
    "--concurrency",
    "--timeout",
    "--file-reporter",
    "--coverage-path",
    "--dart-define",
    "--dart-define-from-file",
    "--flavor",
    "--test-randomize-ordering-seed",
    "-d",
    "--device-id",
    "--compiler",
];

fn dart_selects(tail: &[&str]) -> Option<String> {
    let mut words = tail.iter();
    while let Some(word) = words.next() {
        if *word == "--" {
            if let Some(next) = words.next() {
                return Some((*next).to_owned());
            }
            break;
        }
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if matches!(
            flag,
            "-n" | "--name"
                | "-N"
                | "--plain-name"
                | "-t"
                | "--tags"
                | "-x"
                | "--exclude-tags"
                | "--total-shards"
                | "--shard-index"
        ) {
            return Some((*word).to_owned());
        }
        if DART_VALUES.contains(word) {
            words.next();
            continue;
        }
        if !word.starts_with('-') {
            return Some((*word).to_owned());
        }
    }
    None
}

const RSPEC_VALUES: &[&str] = &[
    "-f",
    "--format",
    "-o",
    "--out",
    "-I",
    "-r",
    "--require",
    "-O",
    "--options",
    "--order",
    "--seed",
    "--deprecation-out",
    "-P",
    "--pattern",
    "--exclude-pattern",
    "--default-path",
    "--failure-exit-code",
    "--error-exit-code",
];

fn rspec_selects(tail: &[&str]) -> Option<String> {
    let mut words = tail.iter();
    while let Some(word) = words.next() {
        if *word == "--" {
            if let Some(next) = words.next() {
                return Some((*next).to_owned());
            }
            break;
        }
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if matches!(
            flag,
            "-e" | "--example"
                | "-E"
                | "--example-matches"
                | "-t"
                | "--tag"
                | "--only-failures"
                | "--next-failure"
                | "--examples"
        ) {
            return Some((*word).to_owned());
        }
        if RSPEC_VALUES.contains(word) {
            words.next();
            continue;
        }
        if !word.starts_with('-') {
            return Some((*word).to_owned());
        }
    }
    None
}

const PYTEST_VALUES: &[&str] = &[
    "-k",
    "-m",
    "-c",
    "-o",
    "-p",
    "-n",
    "--rootdir",
    "--confcutdir",
    "--basetemp",
    "--tb",
    "--maxfail",
    "--durations",
    "--color",
    "--junitxml",
    "--cov",
    "--cov-report",
    "--deselect",
    "--ignore",
    "--ignore-glob",
    "--log-level",
    "--import-mode",
    "-W",
    "-r",
];

fn pytest_selects(tail: &[&str]) -> Option<String> {
    let mut words = tail.iter();
    while let Some(word) = words.next() {
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if matches!(
            flag,
            "--lf"
                | "--last-failed"
                | "--ff"
                | "--failed-first"
                | "--sw"
                | "--stepwise"
                | "--pyargs"
        ) {
            return Some((*word).to_owned());
        }
        if PYTEST_VALUES.contains(word) {
            words.next();
            continue;
        }
        if !word.starts_with('-') {
            return Some((*word).to_owned());
        }
    }
    None
}

const JS_VALUES: &[&str] = &[
    "-t",
    "--testNamePattern",
    "-c",
    "--config",
    "--reporter",
    "--reporters",
    "--maxWorkers",
    "-w",
    "--project",
    "--environment",
    "--testTimeout",
    "--shard",
    "--outputFile",
    "--root",
    "--dir",
];

fn js_selects(tail: &[&str]) -> Option<String> {
    let mut words = tail.iter();
    while let Some(word) = words.next() {
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if matches!(
            flag,
            "--testPathPattern"
                | "--testPathPatterns"
                | "--findRelatedTests"
                | "--onlyChanged"
                | "-o"
                | "--changedSince"
                | "--lastCommit"
                | "--related"
                | "--changed"
        ) {
            return Some((*word).to_owned());
        }
        if JS_VALUES.contains(word) {
            words.next();
            continue;
        }
        if !word.starts_with('-') {
            return Some((*word).to_owned());
        }
    }
    None
}

fn go_selects(tail: &[&str]) -> Option<String> {
    let mut words = tail.iter();
    while let Some(word) = words.next() {
        let flag = word.split_once('=').map_or(*word, |(flag, _)| flag);
        if matches!(flag, "-run" | "-skip" | "-list") {
            return Some((*word).to_owned());
        }
        if matches!(
            *word,
            "-count"
                | "-timeout"
                | "-p"
                | "-parallel"
                | "-tags"
                | "-cpu"
                | "-coverprofile"
                | "-covermode"
                | "-coverpkg"
                | "-exec"
                | "-o"
                | "-bench"
                | "-benchtime"
                | "-shuffle"
                | "-ldflags"
                | "-gcflags"
                | "-mod"
                | "-vet"
        ) {
            words.next();
            continue;
        }
        if !word.starts_with('-') && *word != "./..." {
            return Some((*word).to_owned());
        }
    }
    None
}

/// What the runner's archived output says ran.
#[derive(Default, Debug)]
pub(crate) struct Tally {
    /// Tests that reported a result.
    pub ran: usize,
    pub failed: Vec<String>,
    pub ignored: usize,
    /// Whether any outcome line was recognized at all.
    pub known: bool,
    /// Reported test names by target key (Cargo: the test binary's crate name).
    pub names: BTreeMap<String, BTreeSet<String>>,
    /// Tests each target's summary line reports, passed, failed or ignored.
    /// Under `cargo test -q` libtest prints counts but no names.
    pub counts: BTreeMap<String, usize>,
    /// Doc-test result lines.
    pub doctests: usize,
    /// Go packages served from the test cache, and packages that ran.
    pub cached: usize,
    pub packages: usize,
    /// pytest node IDs it could not find.
    pub not_found: Vec<String>,
    /// Cargo and pytest: each failing test with its target key and what the
    /// runner said about it. pytest `ERROR` lines count here, not in `failed`.
    pub failures: Vec<Failure>,
    /// Whether the runner explicitly reported that no tests matched the selection.
    pub no_tests_matched: bool,
    /// Non-test suite/load/compile errors reported by the runner (never widened as 0-ran).
    pub errors: usize,
}

/// A failing test and the runner's message about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Failure {
    /// The target key, as in [`Tally::names`]; empty for pytest.
    pub target: String,
    pub name: String,
    /// Cargo: the test's block under `failures:`; pytest: the reason on its
    /// summary line. Empty when the runner printed none.
    pub message: String,
}

impl Tally {
    fn failure(&mut self, target: &str, name: &str) -> &mut Failure {
        let at = match self
            .failures
            .iter()
            .position(|failure| failure.target == target && failure.name == name)
        {
            Some(at) => at,
            None => {
                self.failures.push(Failure {
                    target: target.to_owned(),
                    name: name.to_owned(),
                    message: String::new(),
                });
                self.failures.len() - 1
            }
        };
        &mut self.failures[at]
    }
}

/// Reads a finished step's archived streams.
pub(crate) fn tally(kind: Kind, stdout: &str, stderr: &str) -> Tally {
    match kind {
        Kind::Cargo => cargo_tally(stdout, stderr),
        Kind::Nextest => nextest_tally(stdout, stderr),
        Kind::Pytest => pytest_tally(&format!("{stdout}\n{stderr}")),
        Kind::Jest | Kind::Vitest => js_tally(&format!("{stdout}\n{stderr}")),
        Kind::Bun => bun_tally(&format!("{stdout}\n{stderr}")),
        Kind::NodeTest => node_test_tally(&format!("{stdout}\n{stderr}")),
        Kind::Playwright => playwright_tally(&format!("{stdout}\n{stderr}")),
        Kind::DenoTest => deno_tally(&format!("{stdout}\n{stderr}")),
        Kind::DartTest => dart_tally(&format!("{stdout}\n{stderr}")),
        Kind::Rspec => rspec_tally(&format!("{stdout}\n{stderr}")),
        Kind::Go => go_tally(stdout),
        Kind::Command => {
            let combined = format!("{stdout}\n{stderr}");
            let mut tally = cargo_tally(stdout, stderr);
            if !tally.known {
                tally = nextest_tally(stdout, stderr);
            }
            if !tally.known {
                tally = pytest_tally(stdout);
            }
            if !tally.known {
                tally = js_tally(&combined);
            }
            if !tally.known {
                tally = bun_tally(&combined);
            }
            if !tally.known {
                tally = node_test_tally(&combined);
            }
            if !tally.known {
                tally = playwright_tally(&combined);
            }
            if !tally.known {
                tally = deno_tally(&combined);
            }
            if !tally.known {
                tally = dart_tally(&combined);
            }
            if !tally.known {
                tally = rspec_tally(&combined);
            }
            if !tally.known {
                tally = go_tally(stdout);
            }
            tally
        }
    }
}

/// Parses `PASS`/`FAIL`/`TIMEOUT`/`SKIP`/`FLAKY` and `Summary [...]` lines from
/// `cargo nextest run`. Nextest writes its status stream to stderr (or stdout
/// when merged), and repeats `FAIL` lines after `Summary`, so outcomes are
/// deduplicated per `(binary_id, test_name)`.
fn nextest_tally(stdout: &str, stderr: &str) -> Tally {
    let mut tally = Tally::default();
    let mut seen = BTreeSet::<(String, String)>::new();
    let mut summary_run = 0usize;
    let mut summary_skipped = 0usize;
    let mut saw_summary = false;
    let mut output: Option<(String, String, Vec<String>)> = None;

    let finish_output = |tally: &mut Tally, output: &mut Option<(String, String, Vec<String>)>| {
        if let Some((target, name, lines)) = output.take() {
            let text = lines.join("\n").trim().to_owned();
            if !text.is_empty() {
                let failure = tally.failure(&target, &name);
                if failure.message.is_empty() {
                    failure.message = text;
                } else {
                    failure.message.push('\n');
                    failure.message.push_str(&text);
                }
            }
        }
    };

    for text in [stderr, stdout] {
        finish_output(&mut tally, &mut output);
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("Summary [")
                || trimmed.starts_with("--- STDOUT:")
                || trimmed.starts_with("--- STDERR:")
                || trimmed.starts_with("PASS [")
                || trimmed.starts_with("FAIL [")
                || trimmed.starts_with("TIMEOUT [")
                || trimmed.starts_with("SKIP [")
                || trimmed.starts_with("FLAKY ")
                || (trimmed.len() >= 6 && trimmed.bytes().all(|b| b == b'-'))
            {
                finish_output(&mut tally, &mut output);
            }
            if let Some(rest) = trimmed
                .strip_prefix("--- STDOUT:")
                .or_else(|| trimmed.strip_prefix("--- STDERR:"))
                .and_then(|s| s.strip_suffix(" ---"))
                .map(str::trim)
                && let Some((binary, name)) = split_nextest_target_name(rest)
            {
                output = Some((binary.to_owned(), name.to_owned(), Vec::new()));
                continue;
            }
            if let Some((_, _, lines)) = &mut output {
                lines.push(line.to_owned());
                continue;
            }
            if trimmed.starts_with("Summary [")
                && let Some((_, rest)) = trimmed.split_once("] ")
                && (rest.contains(" tests run:") || rest.contains(" test run:"))
            {
                tally.known = true;
                saw_summary = true;
                let ran = count_before(rest, "tests")
                    .or_else(|| count_before(rest, "test"))
                    .unwrap_or(0);
                let skipped = count_before(rest, "skipped").unwrap_or(0);
                summary_run += ran;
                summary_skipped += skipped;
                *tally.counts.entry("*".into()).or_default() += ran + skipped;
                if ran == 0 && skipped == 0 {
                    tally.no_tests_matched = true;
                }
                continue;
            }
            if trimmed.contains("no tests to run")
                && (trimmed.starts_with("error:") || trimmed.starts_with("WARN"))
            {
                tally.no_tests_matched = true;
                continue;
            }
            if let Some((status, binary, name)) = parse_nextest_outcome(trimmed) {
                tally.known = true;
                match status {
                    "PASS" | "FLAKY" => {
                        if seen.insert((binary.to_owned(), name.to_owned())) {
                            tally.ran += 1;
                            tally
                                .names
                                .entry(binary.to_owned())
                                .or_default()
                                .insert(name.to_owned());
                        }
                    }
                    "FAIL" | "TIMEOUT" => {
                        if seen.insert((binary.to_owned(), name.to_owned())) {
                            tally.ran += 1;
                            tally.failed.push(name.to_owned());
                            tally.failure(binary, name);
                            tally
                                .names
                                .entry(binary.to_owned())
                                .or_default()
                                .insert(name.to_owned());
                        }
                    }
                    "SKIP" if seen.insert((binary.to_owned(), name.to_owned())) => {
                        tally.ignored += 1;
                    }
                    _ => {}
                }
            }
        }
        finish_output(&mut tally, &mut output);
    }
    if saw_summary {
        tally.ran = tally.ran.max(summary_run);
        tally.ignored = tally.ignored.max(summary_skipped);
    }
    tally
}

fn parse_nextest_outcome(trimmed: &str) -> Option<(&'static str, &str, &str)> {
    let (status, after_bracket) = if let Some(rest) = trimmed.strip_prefix("PASS [") {
        ("PASS", rest.split_once("] ")?.1)
    } else if let Some(rest) = trimmed.strip_prefix("FAIL [") {
        ("FAIL", rest.split_once("] ")?.1)
    } else if let Some(rest) = trimmed.strip_prefix("TIMEOUT [") {
        ("TIMEOUT", rest.split_once("] ")?.1)
    } else if let Some(rest) = trimmed.strip_prefix("SKIP [") {
        ("SKIP", rest.split_once("] ")?.1)
    } else {
        let rest = trimmed.strip_prefix("FLAKY ")?;
        let (_, rest) = rest.split_once('[')?;
        ("FLAKY", rest.split_once("] ")?.1)
    };
    let tail = strip_nextest_retry_prefix(after_bracket.trim_start());
    let (binary, name) = split_nextest_target_name(tail)?;
    Some((status, binary, name))
}

fn strip_nextest_retry_prefix(tail: &str) -> &str {
    tail.strip_prefix('(')
        .and_then(|s| s.split_once(") "))
        .filter(|(tries, _)| {
            tries.split_once('/').is_some_and(|(a, b)| {
                !a.is_empty()
                    && !b.is_empty()
                    && a.bytes().all(|c| c.is_ascii_digit())
                    && b.bytes().all(|c| c.is_ascii_digit())
            })
        })
        .map_or(tail, |(_, rest)| rest.trim_start())
}

fn split_nextest_target_name(text: &str) -> Option<(&str, &str)> {
    let mut parts = text.split_whitespace();
    let binary = parts.next()?;
    let name = parts.next()?;
    (!binary.is_empty() && !name.is_empty()).then_some((binary, name))
}

/// Pairs each `running N tests` block on stdout with the k-th `Running` or
/// `Doc-tests` line on stderr, so names are attributed to their test binary.
fn cargo_tally(stdout: &str, stderr: &str) -> Tally {
    let mut tally = Tally::default();
    let binaries: Vec<String> = stderr
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("Running ") {
                // `Running unittests src/lib.rs (target/debug/deps/ledger_core-1a2b)`
                let binary = rest.rsplit_once('(')?.1.trim_end_matches(')');
                let stem = Path::new(binary).file_name()?.to_str()?;
                let name = stem.rsplit_once('-').map_or(stem, |(name, _)| name);
                Some(name.replace('-', "_"))
            } else {
                line.strip_prefix("Doc-tests ")
                    .map(|name| format!("doc:{}", name.replace('-', "_")))
            }
        })
        .collect();
    let blocks = stdout
        .lines()
        .filter(|line| {
            line.starts_with("running ") && line.ends_with(" test")
                || line.starts_with("running ") && line.ends_with(" tests")
        })
        .count();
    let paired = blocks == binaries.len();
    let mut block = 0usize;
    let mut current = String::from("*");
    // Tests the current block reported by name, to add what only its summary counts.
    let mut named = 0usize;
    // The `---- NAME stdout ----` block being read: its target, name and lines.
    let mut output: Option<(String, String, Vec<&str>)> = None;
    for line in stdout.lines() {
        if output.is_some()
            && (line == "failures:"
                || line.starts_with("test result:")
                || line.starts_with("running ")
                || line.starts_with("---- "))
            && let Some((target, name, lines)) = output.take()
        {
            tally.failure(&target, &name).message = lines.join("\n").trim().to_owned();
        }
        if let Some((_, _, lines)) = &mut output {
            lines.push(line);
        }
        if line.starts_with("running ") && (line.ends_with(" tests") || line.ends_with(" test")) {
            current = if paired {
                binaries[block].clone()
            } else {
                "*".into()
            };
            block += 1;
            named = 0;
            continue;
        }
        if let Some(rest) = line.strip_prefix("test ")
            && let Some((name, outcome)) = rest.rsplit_once(" ... ")
        {
            let name = name.trim_end_matches(" - should panic");
            let outcome = outcome.trim();
            tally.known = true;
            match outcome {
                "ok" => {}
                "FAILED" => {
                    tally.failed.push(name.to_owned());
                    tally.failure(&current, name);
                }
                _ if outcome.starts_with("ignored") => {
                    tally.ignored += 1;
                    continue;
                }
                _ => continue,
            }
            tally.ran += 1;
            named += 1;
            if current.starts_with("doc:") {
                tally.doctests += 1;
            }
            tally
                .names
                .entry(current.clone())
                .or_default()
                .insert(name.to_owned());
        } else if line.starts_with("test result:") {
            tally.known = true;
            // `test result: ok. 3 passed; 0 failed; 1 ignored; ...`
            let count = |word| count_before(line, word).unwrap_or(0);
            let ran = count("passed") + count("failed");
            *tally.counts.entry(current.clone()).or_default() += ran + count("ignored");
            // Quiet output prints dots, not names: the summary is the count.
            if ran > named {
                tally.ran += ran - named;
                if current.starts_with("doc:") {
                    tally.doctests += ran - named;
                }
            }
            named = ran.max(named);
        } else if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|rest| rest.strip_suffix(" stdout ----"))
        {
            // A failure's captured output names it, in quiet output too.
            if !tally.failed.iter().any(|failed| failed == name) {
                tally.failed.push(name.to_owned());
            }
            output = Some((current.clone(), name.to_owned(), Vec::new()));
        }
    }
    if let Some((target, name, lines)) = output {
        tally.failure(&target, &name).message = lines.join("\n").trim().to_owned();
    }
    tally
}

fn count_before(line: &str, word: &str) -> Option<usize> {
    let words: Vec<&str> = line.split_whitespace().collect();
    words.iter().enumerate().find_map(|(at, current)| {
        if current.trim_end_matches([',', ';']) == word && at > 0 {
            words[at - 1].parse().ok()
        } else {
            None
        }
    })
}

fn pytest_tally(stdout: &str) -> Tally {
    let mut tally = Tally::default();
    for line in stdout.lines() {
        let trimmed = line.trim_matches(['=', ' ']);
        if let Some(rest) = line.strip_prefix("ERROR: not found: ") {
            tally.not_found.push(rest.trim().to_owned());
        }
        // Short summary lines: `FAILED tests/test_a.py::test_b - assert 1 == 2`,
        // and with `-rA` also `PASSED` and `ERROR` lines.
        for (prefix, failing) in [("FAILED ", true), ("ERROR ", true), ("PASSED ", false)] {
            let Some(rest) = line.strip_prefix(prefix) else {
                continue;
            };
            let (name, message) = rest.split_once(" - ").unwrap_or((rest, ""));
            let name = name.trim();
            if prefix == "FAILED " {
                tally.failed.push(name.to_owned());
            }
            // `ERROR tests/test_a.py` reports a file that failed to collect.
            if prefix != "ERROR " || name.contains("::") {
                tally
                    .names
                    .entry(String::new())
                    .or_default()
                    .insert(name.to_owned());
            }
            if failing && name.contains("::") {
                tally.failure("", name).message = message.trim().to_owned();
            }
        }
        if trimmed.contains(" in ")
            && trimmed.starts_with(|c: char| c.is_ascii_digit())
            && (trimmed.contains(" passed")
                || trimmed.contains(" failed")
                || trimmed.contains(" error")
                || trimmed.contains(" skipped")
                || trimmed.contains(" deselected"))
        {
            tally.known = true;
            let passed = count_before(trimmed, "passed").unwrap_or(0);
            let failed = count_before(trimmed, "failed").unwrap_or(0);
            let errors = count_before(trimmed, "errors")
                .or_else(|| count_before(trimmed, "error"))
                .unwrap_or(0);
            tally.ignored = count_before(trimmed, "skipped").unwrap_or(0);
            tally.ran = passed + failed + errors;
        } else if trimmed.starts_with("no tests ran") {
            tally.known = true;
        }
    }
    tally
}

fn js_tally(text: &str) -> Tally {
    let mut tally = Tally::default();
    let mut failed_suites = 0usize;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("No tests found")
            || trimmed.starts_with("No test files found")
            || trimmed.contains("No test files found, exiting")
        {
            tally.no_tests_matched = true;
        }
        // Jest: `Test Suites: 1 failed, 1 total`
        if let Some(rest) = trimmed.strip_prefix("Test Suites:") {
            failed_suites = failed_suites.max(count_before(rest, "failed").unwrap_or(0));
        }
        // Jest: `Tests:       1 failed, 6 passed, 7 total`
        if let Some(rest) = trimmed.strip_prefix("Tests:") {
            tally.known = true;
            tally.ran = count_before(rest, "total").unwrap_or(0);
            tally.ignored = count_before(rest, "skipped").unwrap_or(0);
            tally.ran = tally.ran.saturating_sub(tally.ignored);
        }
        // Vitest: `Tests  2 failed | 5 passed (7)` or `Tests  no tests`
        else if let Some(rest) = trimmed.strip_prefix("Tests ") {
            let rest = rest.trim();
            if rest == "no tests" {
                tally.known = true;
                tally.no_tests_matched = true;
            } else if let Some(total) = rest
                .rsplit_once('(')
                .and_then(|(_, total)| total.trim_end_matches(')').trim().parse::<usize>().ok())
            {
                tally.known = true;
                tally.ignored = count_before(rest, "skipped").unwrap_or(0);
                tally.ran = total.saturating_sub(tally.ignored);
            }
        }
        // Vitest: `Errors  1 error`
        else if let Some(rest) = trimmed.strip_prefix("Errors ")
            && let Some(count) =
                count_before(rest, "error").or_else(|| count_before(rest, "errors"))
        {
            tally.errors += count;
        }
        if let Some(file) = trimmed.strip_prefix("FAIL ").or_else(|| {
            trimmed
                .strip_prefix("❯ ")
                .filter(|rest| rest.contains(".test.") || rest.contains(".spec."))
        }) {
            let file = file.trim();
            let file = file.split_whitespace().next().unwrap_or(file);
            if !file.is_empty() && !tally.failed.iter().any(|known| known == file) {
                tally.failed.push(file.to_owned());
            }
        }
    }
    if failed_suites > 0 && tally.ran == 0 {
        tally.errors += failed_suites;
    }
    if tally.known
        && tally.ran == 0
        && tally.ignored == 0
        && tally.errors == 0
        && tally.failed.is_empty()
    {
        tally.no_tests_matched = true;
    }
    tally
}

fn bun_tally(text: &str) -> Tally {
    let mut tally = Tally::default();
    let mut current_file = String::new();
    let mut pass_lines = 0usize;
    let mut fail_lines = 0usize;
    let mut skip_lines = 0usize;
    let mut summary_pass = 0usize;
    let mut summary_fail = 0usize;
    let mut summary_skip = 0usize;
    let mut summary_ran = 0usize;
    let mut saw_summary_counts = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "0 tests ran"
            || trimmed.starts_with("error: No tests found")
            || trimmed.starts_with("No tests found")
        {
            tally.known = true;
            tally.no_tests_matched = true;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Ran ")
            && rest.contains(" across ")
            && let Some(count) = count_before(rest, "tests").or_else(|| count_before(rest, "test"))
        {
            tally.known = true;
            summary_ran = count;
            continue;
        }
        let words: Vec<&str> = trimmed.split_whitespace().collect();
        if words.len() == 2
            && let Ok(count) = words[0].parse::<usize>()
        {
            match words[1] {
                "pass" => {
                    tally.known = true;
                    saw_summary_counts = true;
                    summary_pass = count;
                    continue;
                }
                "fail" => {
                    tally.known = true;
                    saw_summary_counts = true;
                    summary_fail = count;
                    continue;
                }
                "skip" | "todo" => {
                    tally.known = true;
                    summary_skip += count;
                    continue;
                }
                "error" | "errors" => {
                    tally.known = true;
                    tally.errors += count;
                    continue;
                }
                _ => {}
            }
        }
        if let Some(rest) = trimmed.strip_prefix("(pass) ") {
            tally.known = true;
            pass_lines += 1;
            let name = strip_trailing_bracket_timing(rest.trim());
            let key = if current_file.is_empty() {
                "*".to_owned()
            } else {
                current_file.clone()
            };
            tally.names.entry(key).or_default().insert(name.to_owned());
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("(fail) ") {
            tally.known = true;
            fail_lines += 1;
            let raw = strip_trailing_bracket_timing(rest.trim());
            let full = if !current_file.is_empty() && !raw.contains(&current_file) {
                format!("{current_file} › {raw}")
            } else {
                raw.to_owned()
            };
            if !tally.failed.iter().any(|known| known == &full) {
                tally.failed.push(full);
            }
            let key = if current_file.is_empty() {
                "*".to_owned()
            } else {
                current_file.clone()
            };
            tally.names.entry(key).or_default().insert(raw.to_owned());
            continue;
        }
        if trimmed.starts_with("(skip) ") || trimmed.starts_with("(todo) ") {
            tally.known = true;
            skip_lines += 1;
            continue;
        }
        if !line.starts_with(char::is_whitespace)
            && let Some(path) = trimmed.strip_suffix(':')
            && !path.contains(char::is_whitespace)
            && is_bun_test_file(path)
        {
            current_file = path.to_owned();
        }
    }
    let from_counts = if saw_summary_counts {
        summary_pass + summary_fail
    } else {
        summary_ran
    };
    tally.ran = from_counts.max(pass_lines + fail_lines);
    tally.ignored = summary_skip.max(skip_lines);
    if tally.known
        && tally.ran == 0
        && tally.ignored == 0
        && tally.errors == 0
        && tally.failed.is_empty()
    {
        tally.no_tests_matched = true;
    }
    tally
}

fn strip_trailing_bracket_timing(text: &str) -> &str {
    if let Some((head, tail)) = text.rsplit_once(" [")
        && tail.ends_with(']')
        && (tail.ends_with("ms]") || tail.ends_with("s]") || tail.ends_with("m]"))
    {
        head.trim_end()
    } else {
        text
    }
}

fn strip_trailing_paren_timing(text: &str) -> &str {
    if let Some((head, tail)) = text.rsplit_once(" (")
        && tail.ends_with(')')
        && (tail.ends_with("ms)") || tail.ends_with("s)") || tail.ends_with("m)"))
    {
        head.trim_end()
    } else {
        text
    }
}

fn node_test_tally(text: &str) -> Tally {
    let mut tally = Tally::default();
    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut cancelled = 0usize;
    let mut skipped = 0usize;
    let mut total_tests: Option<usize> = None;
    let mut in_failing_section = false;
    let mut pending_tap_fail: Option<String> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Could not find '") {
            tally.no_tests_matched = true;
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("# ")
            .or_else(|| trimmed.strip_prefix("ℹ "))
        {
            let mut parts = rest.split_whitespace();
            if let (Some(key), Some(val)) = (parts.next(), parts.next())
                && let Ok(num) = val.parse::<usize>()
            {
                match key {
                    "tests" => {
                        tally.known = true;
                        total_tests = Some(num);
                        continue;
                    }
                    "pass" => {
                        tally.known = true;
                        pass = num;
                        continue;
                    }
                    "fail" => {
                        tally.known = true;
                        fail = num;
                        continue;
                    }
                    "cancelled" => {
                        tally.known = true;
                        cancelled = num;
                        continue;
                    }
                    "skipped" | "todo" => {
                        tally.known = true;
                        skipped += num;
                        continue;
                    }
                    _ => {}
                }
            }
        }
        if trimmed == "✖ failing tests:" {
            in_failing_section = true;
            continue;
        }
        if in_failing_section && let Some(rest) = trimmed.strip_prefix("✖ ") {
            let name = strip_trailing_paren_timing(rest.trim());
            if !name.is_empty() && !tally.failed.iter().any(|known| known == name) {
                tally.failed.push(name.to_owned());
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("not ok ") {
            if let Some(prev) = pending_tap_fail.take()
                && !tally.failed.iter().any(|known| known == &prev)
            {
                tally.failed.push(prev);
            }
            if let Some((_, name)) = rest.split_once(" - ") {
                let name = name.trim();
                if !name.is_empty() {
                    pending_tap_fail = Some(name.to_owned());
                }
            }
            continue;
        }
        if trimmed.starts_with("failureType: 'subtestsFailed'")
            || trimmed.starts_with("failureType: \"subtestsFailed\"")
        {
            pending_tap_fail = None;
            continue;
        }
        if (trimmed.starts_with("ok ") || trimmed.starts_with("1.."))
            && let Some(prev) = pending_tap_fail.take()
            && !tally.failed.iter().any(|known| known == &prev)
        {
            tally.failed.push(prev);
        }
    }
    if let Some(prev) = pending_tap_fail.take()
        && !tally.failed.iter().any(|known| known == &prev)
    {
        tally.failed.push(prev);
    }
    tally.ran = pass + fail + cancelled;
    tally.ignored = skipped;
    if total_tests == Some(0) && tally.ran == 0 && tally.ignored == 0 && tally.failed.is_empty() {
        tally.no_tests_matched = true;
    }
    tally
}

fn playwright_tally(text: &str) -> Tally {
    let mut tally = Tally::default();
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut flaky = 0usize;
    let mut interrupted = 0usize;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "Error: No tests found"
            || trimmed == "Error: No tests found."
            || trimmed == "No tests found"
            || trimmed == "No tests found."
        {
            tally.no_tests_matched = true;
            continue;
        }
        if trimmed.contains("error was not a part of any test")
            || trimmed.contains("errors were not a part of any test")
        {
            let count = trimmed
                .split_whitespace()
                .next()
                .and_then(|word| word.parse::<usize>().ok())
                .unwrap_or(1);
            tally.errors += count;
            continue;
        }
        // Failure section header: `1) [chromium] › tests/auth.spec.ts:24:5 › Auth › rejects bad password ───`
        if let Some((num, rest)) = trimmed.split_once(") ")
            && !num.is_empty()
            && num.bytes().all(|b| b.is_ascii_digit())
            && rest.contains(" › ")
        {
            let name = rest.trim_end_matches(['─', '-', ' ']).trim();
            if !name.is_empty() && !tally.failed.iter().any(|known| known == name) {
                tally.failed.push(name.to_owned());
            }
            continue;
        }
        // Totals lines: `37 passed (18.4s)`, `2 failed`, `1 flaky`, `3 skipped`, `1 interrupted`
        let mut parts = trimmed.split_whitespace();
        if let (Some(num), Some(word)) = (parts.next(), parts.next())
            && let Ok(count) = num.parse::<usize>()
        {
            let rest: Vec<&str> = parts.collect();
            let timing_only = rest.is_empty()
                || (rest.len() == 1 && rest[0].starts_with('(') && rest[0].ends_with(')'));
            if timing_only {
                match word {
                    "passed" => {
                        tally.known = true;
                        passed = count;
                    }
                    "failed" => {
                        tally.known = true;
                        failed = count;
                    }
                    "flaky" => {
                        tally.known = true;
                        flaky = count;
                    }
                    "skipped" => {
                        tally.known = true;
                        tally.ignored = count;
                    }
                    "interrupted" => {
                        tally.known = true;
                        interrupted = count;
                    }
                    _ => {}
                }
            }
        }
    }
    tally.ran = passed + failed + flaky + interrupted;
    if tally.known
        && tally.ran == 0
        && tally.ignored == 0
        && tally.errors == 0
        && tally.failed.is_empty()
    {
        tally.no_tests_matched = true;
    }
    tally
}

fn deno_tally(text: &str) -> Tally {
    let mut tally = Tally::default();
    let mut in_errors_or_failures = false;
    let mut saw_error_line = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains("No test modules found") {
            tally.known = true;
            tally.no_tests_matched = true;
            continue;
        }
        if trimmed == "ERRORS" || trimmed == "FAILURES" {
            in_errors_or_failures = true;
            continue;
        }
        // Summary line: `ok | 4 passed (2 steps) | 0 failed | 1 ignored (45ms)`
        // or `FAILED | 1 passed | 1 failed (12ms)`.
        if (trimmed.starts_with("ok |") || trimmed.starts_with("FAILED |"))
            && trimmed.contains("passed")
            && trimmed.contains("failed")
        {
            in_errors_or_failures = false;
            let passed = count_before(trimmed, "passed").unwrap_or(0);
            let failed = count_before(trimmed, "failed").unwrap_or(0);
            let ignored = count_before(trimmed, "ignored").unwrap_or(0);
            tally.known = true;
            tally.ran = passed + failed;
            tally.ignored = ignored;
            if passed == 0 && failed == 0 && ignored == 0 && tally.errors == 0 {
                tally.no_tests_matched = true;
            }
            continue;
        }
        if in_errors_or_failures
            && !line.starts_with([' ', '\t'])
            && let Some((name, loc)) = trimmed.split_once(" => ")
        {
            let name = name.trim();
            if !name.is_empty() && !loc.trim().is_empty() {
                if !tally.failed.iter().any(|known| known == name) {
                    tally.failed.push(name.to_owned());
                }
                continue;
            }
        }
        if trimmed.starts_with("error:") {
            saw_error_line = true;
        }
    }
    if saw_error_line && tally.ran == 0 && tally.failed.is_empty() && !tally.no_tests_matched {
        tally.errors += 1;
    }
    tally
}

fn dart_tally(text: &str) -> Tally {
    let mut tally = Tally::default();
    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for line in text.split(['\n', '\r']) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains("No tests were found.")
            || trimmed.contains("No tests ran.")
            || trimmed.contains("No tests match")
        {
            tally.known = true;
            tally.no_tests_matched = true;
        }
        if trimmed.starts_with("Failed to load ") {
            tally.errors += 1;
            continue;
        }
        // Standard `dart test` / `flutter test` progress & summary lines:
        // `00:02 +5 ~1 -1: Some tests failed.`
        // `00:01 +1 -1: test/calc_test.dart: computes tax [E]`
        let Some((header, body)) = trimmed.split_once(':') else {
            continue;
        };
        // Expect `MM:SS ...` so `header` is digits and `body` starts with `SS `.
        if header.is_empty() || !header.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Some((counts_part, message)) = body.split_once(':') else {
            continue;
        };
        let mut tokens = counts_part.split_whitespace();
        let Some(secs) = tokens.next() else {
            continue;
        };
        if secs.is_empty() || !secs.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let mut saw_count = false;
        for tok in tokens {
            if let Some(n) = tok.strip_prefix('+').and_then(|s| s.parse::<usize>().ok()) {
                passed = n;
                saw_count = true;
            } else if let Some(n) = tok.strip_prefix('~').and_then(|s| s.parse::<usize>().ok()) {
                skipped = n;
                saw_count = true;
            } else if let Some(n) = tok.strip_prefix('-').and_then(|s| s.parse::<usize>().ok()) {
                failed = n;
                saw_count = true;
            }
        }
        if !saw_count {
            continue;
        }
        let msg = message.trim();
        if msg == "All tests passed!" || msg == "Some tests failed." || msg == "No tests ran." {
            tally.known = true;
        }
        if let Some(name) = msg.strip_suffix(" [E]") {
            let name = name.trim();
            if name.starts_with("loading ") {
                tally.errors += 1;
            } else if !name.is_empty() && !tally.failed.iter().any(|known| known == name) {
                tally.failed.push(name.to_owned());
            }
        }
    }
    if passed > 0 || failed > 0 || skipped > 0 {
        tally.known = true;
    }
    tally.ran = passed + failed;
    tally.ignored = skipped;
    if tally.known && tally.ran == 0 && tally.ignored == 0 && tally.errors == 0 && failed == 0 {
        tally.no_tests_matched = true;
    }
    tally
}

fn rspec_tally(text: &str) -> Tally {
    let mut tally = Tally::default();
    let mut in_failed = false;
    let mut saw_failed_item = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "No examples found." {
            tally.known = true;
            tally.no_tests_matched = true;
            continue;
        }
        if trimmed == "Failed examples:" {
            in_failed = true;
            saw_failed_item = false;
            continue;
        }
        if in_failed {
            if let Some(rest) = trimmed.strip_prefix("rspec ") {
                saw_failed_item = true;
                let target = rest.split_whitespace().next().unwrap_or(rest);
                let target = target.strip_prefix("./").unwrap_or(target);
                if !target.is_empty() && !tally.failed.iter().any(|known| known == target) {
                    tally.failed.push(target.to_owned());
                }
                continue;
            } else if trimmed.is_empty() {
                if saw_failed_item {
                    in_failed = false;
                }
                continue;
            } else {
                in_failed = false;
            }
        }
        // `12 examples, 1 failure, 2 pending` or `0 examples, 0 failures, 1 error occurred outside of examples`
        if (trimmed.contains(" example,") || trimmed.contains(" examples,"))
            && (trimmed.contains(" failure") || trimmed.contains(" failures"))
            && let Some(examples) =
                count_before(trimmed, "examples").or_else(|| count_before(trimmed, "example"))
        {
            tally.known = true;
            let pending = count_before(trimmed, "pending").unwrap_or(0);
            tally.ignored = pending;
            tally.ran = examples.saturating_sub(pending);
            if trimmed.contains("outside of examples") {
                let errors = count_before(trimmed, "errors")
                    .or_else(|| count_before(trimmed, "error"))
                    .unwrap_or(1);
                tally.errors += errors;
            }
            if examples == 0 && tally.errors == 0 {
                tally.no_tests_matched = true;
            }
        }
    }
    tally
}

fn go_tally(stdout: &str) -> Tally {
    let mut tally = Tally::default();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("ok ") || trimmed.starts_with("ok\t") {
            tally.known = true;
            tally.packages += 1;
            if trimmed.ends_with("(cached)") {
                tally.cached += 1;
            }
        } else if trimmed.starts_with("FAIL\t") || trimmed.starts_with("FAIL ") {
            tally.known = true;
            tally.packages += 1;
        } else if let Some((rest, failed)) = trimmed
            .strip_prefix("--- FAIL: ")
            .map(|rest| (rest, true))
            .or_else(|| trimmed.strip_prefix("--- PASS: ").map(|rest| (rest, false)))
        {
            let name = rest.split_whitespace().next().unwrap_or(rest);
            if failed {
                tally.failed.push(name.to_owned());
            }
            tally.ran += 1;
            // Unindented results are top-level tests, the names `-run` selects.
            if line.starts_with("--- ") {
                tally
                    .names
                    .entry("*".into())
                    .or_default()
                    .insert(name.to_owned());
            }
        } else if trimmed.starts_with("--- SKIP: ") {
            tally.ignored += 1;
        }
    }
    tally
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_names_pair_with_their_binaries() {
        let stderr = "     Running unittests src/lib.rs (target/debug/deps/ledger_core-1a2b)\n     Running tests/totals.rs (target/debug/deps/totals-9f)\n   Doc-tests ledger_core\n";
        let stdout = "\nrunning 2 tests\ntest tax::tests::a ... ok\ntest tax::tests::b ... FAILED\n\ntest result: FAILED. 1 passed; 1 failed\n\nrunning 0 tests\n\ntest result: ok. 0 passed\n\nrunning 1 test\ntest ledger/src/tax.rs - tax::vat (line 3) ... ok\n\ntest result: ok. 1 passed\n";
        let tally = tally(Kind::Cargo, stdout, stderr);
        assert_eq!(tally.ran, 3);
        assert_eq!(tally.failed, ["tax::tests::b"]);
        assert_eq!(tally.doctests, 1);
        assert!(tally.names["ledger_core"].contains("tax::tests::a"));
        assert!(!tally.names.contains_key("totals"));
    }

    #[test]
    fn quiet_cargo_output_is_counted_from_its_summaries() {
        // `cargo test -q` prints dots and no `Running` lines.
        let stdout = "\nrunning 3 tests\n..F\nfailures:\n\n---- tax::tests::b stdout ----\npanicked\n\nfailures:\n    tax::tests::b\n\ntest result: FAILED. 2 passed; 1 failed; 1 ignored; 0 measured; 9 filtered out\n\nrunning 1 test\n.\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
        let quiet = tally(Kind::Cargo, stdout, "");
        assert_eq!(quiet.ran, 4);
        assert_eq!(quiet.failed, ["tax::tests::b"]);
        assert!(quiet.names.is_empty());
        assert_eq!(quiet.counts["*"], 5);
        // Named output counts each test once.
        let stdout = "\nrunning 2 tests\ntest a ... ok\ntest b ... FAILED\n\n---- b stdout ----\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored\n";
        let named = tally(Kind::Cargo, stdout, "");
        assert_eq!((named.ran, named.failed.len()), (2, 1));
    }

    #[test]
    fn cargo_failures_keep_their_binary_and_panic_block() {
        let stderr = "     Running unittests src/lib.rs (target/debug/deps/shop-1a2b)\n     Running tests/cart.rs (target/debug/deps/cart-9f)\n";
        let stdout = "\nrunning 2 tests\ntest adds ... FAILED\ntest keeps ... ok\n\nfailures:\n\n---- adds stdout ----\nprinted first\n\nthread 'adds' panicked at src/lib.rs:7:9:\nassertion `left == right` failed\n  left: 4\n right: 5\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\nfailures:\n    adds\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored\n\nrunning 1 test\ntest adds ... FAILED\n\nfailures:\n\n---- adds stdout ----\nthread 'adds' panicked at tests/cart.rs:3:5:\nempty cart\n\nfailures:\n    adds\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored\n";
        let tally = tally(Kind::Cargo, stdout, stderr);
        assert_eq!(tally.failures.len(), 2);
        assert_eq!(
            (
                tally.failures[0].target.as_str(),
                tally.failures[0].name.as_str()
            ),
            ("shop", "adds")
        );
        assert!(tally.failures[0].message.starts_with("printed first"));
        assert!(tally.failures[0].message.ends_with("display a backtrace"));
        assert_eq!(tally.failures[1].target, "cart");
        assert!(tally.failures[1].message.ends_with("empty cart"));
    }

    #[test]
    fn pytest_summary_lines_name_failures_and_passes() {
        let stdout = "PASSED tests/test_a.py::test_ok\nFAILED tests/test_a.py::test_bad - assert 1 == 2\nERROR tests/test_b.py::test_fixture - RuntimeError: boom\nERROR tests/test_c.py\n==== 1 failed, 1 passed, 2 errors in 0.10s ====\n";
        let tally = tally(Kind::Pytest, stdout, "");
        assert_eq!(tally.failed, ["tests/test_a.py::test_bad"]);
        assert_eq!(tally.names[""].len(), 3);
        let failures: Vec<(&str, &str)> = tally
            .failures
            .iter()
            .map(|failure| (failure.name.as_str(), failure.message.as_str()))
            .collect();
        assert_eq!(
            failures,
            [
                ("tests/test_a.py::test_bad", "assert 1 == 2"),
                ("tests/test_b.py::test_fixture", "RuntimeError: boom")
            ]
        );
    }

    #[test]
    fn go_names_top_level_tests_only() {
        let stdout = "=== RUN   TestVat\n--- PASS: TestVat (0.00s)\n=== RUN   TestCartSuite\n    --- FAIL: TestCartSuite/TestCountTwo (0.00s)\n--- FAIL: TestCartSuite (0.00s)\nFAIL\nFAIL\texample.com/shop/cart\t0.01s\nok  \texample.com/shop/price\t0.01s [no tests to run]\n";
        let tally = go_tally(stdout);
        assert_eq!(
            tally.names["*"],
            BTreeSet::from(["TestVat".to_owned(), "TestCartSuite".to_owned()])
        );
        assert_eq!(tally.packages, 2);
        assert!(
            tally
                .failed
                .contains(&"TestCartSuite/TestCountTwo".to_owned())
        );
    }

    #[test]
    fn pytest_options_follow_python_files_and_testpaths() {
        assert!(wildcard("test_*.py", "test_rates.py"));
        assert!(wildcard("*_tests.py", "invoice_tests.py"));
        assert!(!wildcard("test_*.py", "invoice_tests.py"));
        assert!(wildcard("check_?.py", "check_a.py"));
        let (files, paths) = ini_options(
            "[tool:pytest]\npython_files = check_*.py\n    *_tests.py\ntestpaths: tests\n[other]\npython_files = nope.py\n",
            "tool:pytest",
        )
        .unwrap();
        assert_eq!(files, ["check_*.py", "*_tests.py"]);
        assert_eq!(paths, ["tests"]);
        assert!(ini_options("[tox]\nenvlist = py3\n", "pytest").is_none());
        let mut runner = Runner::new(Kind::Pytest, "api".into(), Vec::new());
        assert!(runner.pytest_collects("api/tests/test_rates.py"));
        assert!(!runner.pytest_collects("api/tests/invoice_tests.py"));
        runner.python_files = files;
        runner.testpaths = vec!["api/tests".into()];
        assert!(runner.pytest_collects("api/tests/invoice_tests.py"));
        assert!(!runner.pytest_collects("api/scripts/check_a.py"));
    }

    #[test]
    fn examples_always_build() {
        let example = Target {
            kind: "example".into(),
            name: "weekly".into(),
            src: "examples/weekly.rs".into(),
            required: Vec::new(),
            test: false,
            doctest: false,
            harness: true,
            index_name: "reports_example_weekly".into(),
        };
        assert_eq!(example.flag(), None);
        assert_eq!(
            example.build_flag(),
            Some(vec!["--example".to_owned(), "weekly".to_owned()])
        );
        assert_eq!(example.describe("reports"), "reports --example weekly");
    }

    #[test]
    fn cargo_selection_flags_are_detected() {
        assert_eq!(cargo_selects(&["--offline", "--features", "x"]), None);
        assert_eq!(cargo_selects(&["-p", "api"]).as_deref(), Some("-p"));
        assert_eq!(cargo_selects(&["vat"]).as_deref(), Some("vat"));
        assert_eq!(cargo_selects(&["--", "--nocapture"]), None);
        assert_eq!(cargo_selects(&["--", "vat"]).as_deref(), Some("vat"));
        assert_eq!(cargo_selects(&["--test", "cli"]).as_deref(), Some("--test"));
    }

    #[test]
    fn runner_summaries_are_counted() {
        let pytest = pytest_tally(
            "FAILED tests/test_a.py::test_x - assert 1 == 2\n==== 1 failed, 3 passed, 1 skipped in 0.12s ====\n",
        );
        assert_eq!((pytest.ran, pytest.ignored), (4, 1));
        assert_eq!(pytest.failed, ["tests/test_a.py::test_x"]);
        let jest = js_tally("FAIL src/cart.test.ts\nTests:       1 failed, 6 passed, 7 total\n");
        assert_eq!(jest.ran, 7);
        assert_eq!(jest.failed, ["src/cart.test.ts"]);
        let vitest = js_tally(" Test Files  1 passed (1)\n      Tests  5 passed (5)\n");
        assert_eq!(vitest.ran, 5);
        let go = go_tally(
            "ok  \texample.com/shop/price\t(cached)\nok  \texample.com/shop/cart\t0.01s\n",
        );
        assert_eq!((go.packages, go.cached), (2, 1));
    }

    #[test]
    fn discovered_targets_get_the_index_names() {
        assert_eq!(
            auto_index_name("api", "src/main.rs").as_deref(),
            Some("api_bin")
        );
        assert_eq!(
            auto_index_name("ledger_core", "tests/totals.rs").as_deref(),
            Some("ledger_core_test_totals")
        );
        assert_eq!(
            auto_index_name("api", "src/bin/tool/main.rs").as_deref(),
            Some("api_bin_tool")
        );
    }

    #[test]
    fn features_follow_defaults_and_flags() {
        let package = Package {
            name: "p".into(),
            dir: String::new(),
            manifest: "Cargo.toml".into(),
            targets: Vec::new(),
            depends: BTreeSet::new(),
            features: BTreeMap::from([
                ("default".into(), vec!["a".into()]),
                ("a".into(), vec![]),
                ("b".into(), vec!["c".into()]),
                ("c".into(), vec![]),
            ]),
        };
        let enabled = enabled_features(&package, &[]).unwrap();
        assert!(enabled.contains("a") && !enabled.contains("b"));
        let enabled = enabled_features(&package, &["--features".into(), "b".into()]).unwrap();
        assert!(enabled.contains("c"));
        assert!(enabled_features(&package, &["--all-features".into()]).is_none());
    }

    #[test]
    fn nextest_selection_flags_and_tally_handle_binaries_retries_and_zero_tests() {
        assert_eq!(
            nextest_selects(&["--offline", "--features", "x", "-P", "ci"]),
            None
        );
        assert_eq!(nextest_selects(&["-E", "test(vat)"]).as_deref(), Some("-E"));
        assert_eq!(
            nextest_selects(&["--filterset=package(api)"]).as_deref(),
            Some("--filterset=package(api)")
        );
        assert_eq!(nextest_selects(&["-p", "api"]).as_deref(), Some("-p"));
        assert_eq!(
            nextest_selects(&["--archive-file", "tests.tar.zst"]).as_deref(),
            Some("--archive-file")
        );
        assert_eq!(
            nextest_selects(&["--run-ignored", "all"]).as_deref(),
            Some("--run-ignored")
        );
        assert_eq!(nextest_selects(&["--", "--nocapture"]), None);
        assert_eq!(nextest_selects(&["--", "vat"]).as_deref(), Some("vat"));
        assert_eq!(
            cargo_user_args(&words(&[
                "cargo",
                "nextest",
                "run",
                "--features",
                "extra",
                "--",
                "--nocapture"
            ])),
            ["--features", "extra"]
        );

        let stderr = concat!(
            "    Starting 3 tests across 2 binaries\n",
            "        PASS [   0.004s] ledger-core tax::tests::a\n",
            "       FLAKY 2/2 [   0.008s] ledger-core::totals total_ok\n",
            "        FAIL [   0.015s] (1/2) ledger-core tax::tests::b\n",
            "--- STDERR:              ledger-core tax::tests::b ---\n",
            "thread 'tax::tests::b' panicked at src/tax.rs:10:5:\n",
            "assertion failed\n",
            "------------\n",
            "     Summary [   0.120s] 3 tests run: 2 passed (1 flaky), 1 failed, 1 skipped\n",
            "        FAIL [   0.015s] ledger-core tax::tests::b\n",
        );
        let t = tally(Kind::Nextest, "", stderr);
        assert!(t.known);
        assert_eq!(t.ran, 3);
        assert_eq!(t.ignored, 1);
        assert_eq!(t.failed, ["tax::tests::b"]);
        assert!(t.names["ledger-core"].contains("tax::tests::a"));
        assert!(t.names["ledger-core::totals"].contains("total_ok"));
        assert_eq!(t.failures.len(), 1);
        assert_eq!(t.failures[0].target, "ledger-core");
        assert!(t.failures[0].message.contains("assertion failed"));
        assert!(!t.no_tests_matched);

        let empty = "     Summary [   0.010s] 0 tests run: 0 passed, 0 skipped\n";
        let empty_t = tally(Kind::Nextest, "", empty);
        assert!(empty_t.known);
        assert_eq!(empty_t.ran, 0);
        assert!(empty_t.no_tests_matched);
    }

    #[test]
    fn polyglot_js_and_rspec_selection_flags_and_discovery_patterns_are_accurate() {
        // Bun
        assert_eq!(bun_selects(&["--bail", "1", "--timeout", "5000"]), None);
        assert_eq!(
            bun_selects(&["--bail", "src/a.test.ts"]).as_deref(),
            Some("src/a.test.ts")
        );
        assert_eq!(bun_selects(&["--only"]).as_deref(), Some("--only"));
        assert!(is_bun_test_file("src/cart.test.ts"));
        assert!(is_bun_test_file("src/cart_spec.jsx"));
        assert!(!is_bun_test_file("tests/helpers.ts"));

        // Node --test
        assert!(is_node_test_command(&[
            "--experimental-strip-types",
            "--test"
        ]));
        assert!(!is_node_test_command(&["-e", "console.log(1)", "--test"]));
        assert_eq!(
            node_test_selects(&["--test", "--test-concurrency", "2"]),
            None
        );
        assert_eq!(
            node_test_selects(&["--test", "--test-name-pattern=vat"]).as_deref(),
            Some("--test-name-pattern=vat")
        );
        assert_eq!(
            node_test_selects(&["--test", "test/math.test.js"]).as_deref(),
            Some("test/math.test.js")
        );
        assert!(is_node_test_file("test/math.js"));
        assert!(is_node_test_file("src/math.test.mjs"));
        assert!(is_node_test_file("src/math-test.js"));
        assert!(!is_node_test_file("src/math.spec.js"));
        assert!(!is_node_test_file("tests/helper.js"));

        // Playwright
        assert_eq!(
            playwright_selects(&["--project", "chromium", "--workers", "2"]),
            None
        );
        assert_eq!(
            playwright_selects(&["--grep", "login"]).as_deref(),
            Some("--grep")
        );
        assert_eq!(
            playwright_selects(&["tests/auth.spec.ts"]).as_deref(),
            Some("tests/auth.spec.ts")
        );
        assert!(is_dot_test_or_spec_file("e2e/auth.spec.ts"));
        assert!(!is_dot_test_or_spec_file("e2e/helpers.ts"));

        // RSpec
        assert_eq!(rspec_selects(&["--format", "documentation"]), None);
        assert_eq!(rspec_selects(&["-e", "adds tax"]).as_deref(), Some("-e"));
        assert_eq!(
            rspec_selects(&["spec/order_spec.rb:12"]).as_deref(),
            Some("spec/order_spec.rb:12")
        );
    }

    #[test]
    fn polyglot_tallies_distinguish_zero_ran_from_failures_and_suite_errors() {
        // Bun passing + failing + 0 ran
        let bun = tally(
            Kind::Bun,
            "",
            "bun test v1.1.38\n\nsrc/billing.test.ts:\n(pass) Billing > ok [0.10ms]\n(fail) Billing > bad [1.20ms]\n 1 pass\n 1 fail\nRan 2 tests across 1 file. [4.00ms]\n",
        );
        assert!(bun.known);
        assert_eq!(bun.ran, 2);
        assert_eq!(bun.failed, ["src/billing.test.ts › Billing > bad"]);
        assert!(!bun.no_tests_matched);
        let bun_zero = tally(Kind::Bun, "", "bun test v1.1.38\n\n0 tests ran\n");
        assert!(bun_zero.no_tests_matched);

        // Node --test TAP & spec & 0 tests
        let node_tap = tally(
            Kind::NodeTest,
            "TAP version 13\n    not ok 1 - divides safely\n      ---\n      failureType: 'testCodeFailure'\n      ...\nnot ok 1 - Math\n  ---\n  failureType: 'subtestsFailed'\n  ...\n# tests 2\n# pass 1\n# fail 1\n# skipped 0\n",
            "",
        );
        assert!(node_tap.known);
        assert_eq!(node_tap.ran, 2);
        assert_eq!(node_tap.failed, ["divides safely"]);
        let node_zero = tally(
            Kind::NodeTest,
            "ℹ tests 0\nℹ pass 0\nℹ fail 0\nℹ skipped 0\n",
            "",
        );
        assert!(node_zero.known);
        assert_eq!(node_zero.ran, 0);
        assert!(node_zero.no_tests_matched);

        // Playwright
        let pw = tally(
            Kind::Playwright,
            "Running 2 tests using 1 worker\n  1) [chromium] › tests/auth.spec.ts:5:3 › login ───\n  1 failed\n  1 passed (2.1s)\n",
            "",
        );
        assert!(pw.known);
        assert_eq!(pw.ran, 2);
        assert_eq!(pw.failed, ["[chromium] › tests/auth.spec.ts:5:3 › login"]);
        let pw_zero = tally(Kind::Playwright, "", "Error: No tests found.\n");
        assert!(pw_zero.no_tests_matched);

        // RSpec
        let rspec = tally(
            Kind::Rspec,
            "Failed examples:\n\nrspec ./spec/order_spec.rb:6 # Order adds tax\n\n3 examples, 1 failure, 1 pending\n",
            "",
        );
        assert!(rspec.known);
        assert_eq!((rspec.ran, rspec.ignored), (2, 1));
        assert_eq!(rspec.failed, ["spec/order_spec.rb:6"]);
        let rspec_zero = tally(
            Kind::Rspec,
            "No examples found.\n\n0 examples, 0 failures\n",
            "",
        );
        assert!(rspec_zero.no_tests_matched);
        let rspec_err = tally(
            Kind::Rspec,
            "0 examples, 0 failures, 1 error occurred outside of examples\n",
            "",
        );
        assert_eq!(rspec_err.errors, 1);
        assert!(!rspec_err.no_tests_matched);

        // Jest suite-load crash vs No tests found
        let jest_crash = tally(
            Kind::Jest,
            "",
            "FAIL src/broken.test.ts\nTest Suites: 1 failed, 1 total\nTests:       0 total\n",
        );
        assert_eq!(jest_crash.errors, 1);
        assert!(!jest_crash.no_tests_matched);
        let jest_zero = tally(Kind::Jest, "", "No tests found, exiting with code 1\n");
        assert!(jest_zero.no_tests_matched);

        // Vitest `no tests` vs unhandled error
        let vitest_zero = tally(
            Kind::Vitest,
            " Test Files  no tests\n      Tests  no tests\n",
            "",
        );
        assert!(vitest_zero.no_tests_matched);
        let vitest_err = tally(
            Kind::Vitest,
            " Test Files  0 passed (0)\n      Tests  0 passed (0)\n     Errors  1 error\n",
            "",
        );
        assert_eq!(vitest_err.errors, 1);
        assert!(!vitest_err.no_tests_matched);
    }
}
