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
    Pytest,
    Jest,
    Vitest,
    Go,
    Command,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Pytest => "pytest",
            Self::Jest => "jest",
            Self::Vitest => "vitest",
            Self::Go => "go",
            Self::Command => "command",
        }
    }

    /// The run family and command label recorded for this runner's steps.
    pub fn family(self) -> &'static str {
        match self {
            Self::Cargo => "cargo test",
            Self::Pytest => "pytest",
            Self::Jest => "jest",
            Self::Vitest => "vitest",
            Self::Go => "go test",
            Self::Command => "",
        }
    }

    /// What one selected unit is for this runner.
    pub fn unit(self) -> &'static str {
        match self {
            Self::Jest | Self::Vitest => "test file",
            _ => "test",
        }
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
        }
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

    pub fn is_lib(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "lib" | "proc-macro" | "rlib" | "dylib" | "cdylib" | "staticlib"
        )
    }

    /// How the plan names the target: `ledger-core --lib`, `api --test cli`.
    pub fn describe(&self, package: &str) -> String {
        match self.flag() {
            Some(flag) => format!("{package} {}", flag.join(" ")),
            None => format!("{package} {} {}", self.kind, self.name),
        }
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
    if pytest_roots.is_empty()
        && index.files.keys().any(|file| {
            let name = file.rsplit('/').next().unwrap_or(file);
            file.ends_with(".py")
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
        runners.push(Runner::new(Kind::Pytest, root, base));
    }
    // Jest and Vitest: package.json files that name them.
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
        let jest = listed("jest") || json.get("jest").is_some() || configured("jest.config.");
        let vitest = listed("vitest") || configured("vitest.config.");
        let kind = match (jest, vitest) {
            (true, true) => bail!(
                "{file} lists both jest and vitest; pass the runner after --, e.g. justless test --affected -- npx vitest run"
            ),
            (true, false) => Kind::Jest,
            (false, true) => Kind::Vitest,
            (false, false) => continue,
        };
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
        runners.push(runner);
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
            "no test runner detected; pass one after --, e.g. justless test --affected -- cargo test"
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
                _ => tail,
            };
            let mut runner = Runner::new(Kind::Vitest, cwd.to_owned(), base);
            runner.selects = js_selects(tail);
            if !runner.base[offset..].iter().any(|word| word == "run") {
                // Without `run` Vitest watches; insert it so the step ends.
                runner.base.insert(offset + 1, "run".into());
            }
            runner
        }
        ("go", ["test", tail @ ..]) => {
            let mut runner = Runner::new(Kind::Go, cwd.to_owned(), base);
            runner.selects = go_selects(tail);
            runner
        }
        ("npm" | "pnpm" | "yarn" | "bun", ["test"] | ["run", "test"] | ["t"]) => {
            script_runner(workspace, cwd, program, base)
        }
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
    if runner.kind == Kind::Cargo
        && let Some(facts) = &runner.cargo
    {
        runner.root = parent(&facts.root_manifest).to_owned();
    }
    runner
}

/// `npm test` and friends: a plain `jest` or `vitest` test script is run from
/// node_modules/.bin with its flags; anything else is run unchanged.
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
        let tool = parts.first().map(String::as_str);
        let binary = |name: &str| format!("node_modules/.bin/{name}");
        match tool {
            Some("jest") => {
                let mut command = vec![binary("jest")];
                command.extend(parts[1..].iter().cloned());
                let mut runner = Runner::new(Kind::Jest, cwd.to_owned(), command);
                let tail: Vec<&str> = parts[1..].iter().map(String::as_str).collect();
                runner.selects = js_selects(&tail);
                return runner;
            }
            Some("vitest") => {
                let mut tail: Vec<String> = parts[1..].to_vec();
                if tail.first().map(String::as_str) == Some("run") {
                    tail.remove(0);
                } else if matches!(tail.first().map(String::as_str), Some("watch" | "dev")) {
                    let mut runner = Runner::new(Kind::Command, cwd.to_owned(), base);
                    runner.unknown = Some(format!("{program} test (vitest watch)"));
                    return runner;
                }
                let mut command = vec![binary("vitest"), "run".into()];
                command.extend(tail.iter().cloned());
                let mut runner = Runner::new(Kind::Vitest, cwd.to_owned(), command);
                let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
                runner.selects = js_selects(&tail);
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
    /// Doc-test result lines.
    pub doctests: usize,
    /// Go packages served from the test cache, and packages that ran.
    pub cached: usize,
    pub packages: usize,
    /// pytest node IDs it could not find.
    pub not_found: Vec<String>,
}

/// Reads a finished step's archived streams.
pub(crate) fn tally(kind: Kind, stdout: &str, stderr: &str) -> Tally {
    match kind {
        Kind::Cargo => cargo_tally(stdout, stderr),
        Kind::Pytest => pytest_tally(&format!("{stdout}\n{stderr}")),
        Kind::Jest | Kind::Vitest => js_tally(&format!("{stdout}\n{stderr}")),
        Kind::Go => go_tally(stdout),
        Kind::Command => {
            let mut tally = cargo_tally(stdout, stderr);
            if !tally.known {
                tally = pytest_tally(stdout);
            }
            if !tally.known {
                tally = js_tally(&format!("{stdout}\n{stderr}"));
            }
            if !tally.known {
                tally = go_tally(stdout);
            }
            tally
        }
    }
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
    for line in stdout.lines() {
        if line.starts_with("running ") && (line.ends_with(" tests") || line.ends_with(" test")) {
            current = if paired {
                binaries[block].clone()
            } else {
                "*".into()
            };
            block += 1;
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
                "FAILED" => tally.failed.push(name.to_owned()),
                _ if outcome.starts_with("ignored") => {
                    tally.ignored += 1;
                    continue;
                }
                _ => continue,
            }
            tally.ran += 1;
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
        }
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
        if let Some(name) = line.strip_prefix("FAILED ") {
            let name = name.split(" - ").next().unwrap_or(name).trim();
            tally.failed.push(name.to_owned());
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
    for line in text.lines() {
        let trimmed = line.trim();
        // Jest: `Tests:       1 failed, 6 passed, 7 total`
        if let Some(rest) = trimmed.strip_prefix("Tests:") {
            tally.known = true;
            tally.ran = count_before(rest, "total").unwrap_or(0);
            tally.ignored = count_before(rest, "skipped").unwrap_or(0);
            tally.ran = tally.ran.saturating_sub(tally.ignored);
        }
        // Vitest: `Tests  2 failed | 5 passed (7)`
        else if let Some(rest) = trimmed.strip_prefix("Tests ")
            && let Some(total) = rest
                .rsplit_once('(')
                .and_then(|(_, total)| total.trim_end_matches(')').trim().parse::<usize>().ok())
        {
            tally.known = true;
            tally.ignored = count_before(rest, "skipped").unwrap_or(0);
            tally.ran = total.saturating_sub(tally.ignored);
        }
        if let Some(file) = trimmed.strip_prefix("FAIL ").or_else(|| {
            trimmed
                .strip_prefix("❯ ")
                .filter(|rest| rest.contains(".test.") || rest.contains(".spec."))
        }) {
            let file = file.split_whitespace().next().unwrap_or(file);
            if !tally.failed.iter().any(|known| known == file) {
                tally.failed.push(file.to_owned());
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
        } else if let Some(rest) = trimmed.strip_prefix("--- FAIL: ") {
            let name = rest.split_whitespace().next().unwrap_or(rest);
            tally.failed.push(name.to_owned());
            tally.ran += 1;
        } else if trimmed.starts_with("--- PASS: ") {
            tally.ran += 1;
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
}
