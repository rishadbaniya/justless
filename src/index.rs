use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

use crate::capture::source_digest;
use crate::polyglot::{self, Grammar, Parsers};
use crate::rust_syntax::{self, Import, Kind, Reference, Syntax};
use crate::workspace::{Location, Workspace, lock_state, write_json};

const VERSION: u32 = 3;

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    version: u32,
    files: BTreeMap<String, Parsed>,
}

#[derive(Serialize, Deserialize)]
struct Parsed {
    digest: String,
    syntax: Syntax,
    error: Option<String>,
    foreign: Option<polyglot::Facts>,
    fingerprint: Fingerprint,
    stamp: Option<FileStamp>,
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
struct FileStamp {
    device: u64,
    inode: u64,
    bytes: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

#[cfg(unix)]
fn file_stamp(metadata: &fs::Metadata) -> Option<FileStamp> {
    use std::os::unix::fs::MetadataExt;
    // Coarse file times can miss same-second edits. Hash those files as well.
    (metadata.ctime_nsec() != 0).then(|| FileStamp {
        device: metadata.dev(),
        inode: metadata.ino(),
        bytes: metadata.len(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    })
}

#[cfg(not(unix))]
fn file_stamp(_: &fs::Metadata) -> Option<FileStamp> {
    None
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub struct Fingerprint([u64; 16]);

impl Fingerprint {
    pub fn new(text: &str) -> Self {
        let mut fingerprint = Self::default();
        for window in text.as_bytes().windows(3) {
            let mut hash = 2166136261u32;
            for byte in window {
                hash = (hash ^ u32::from(byte.to_ascii_lowercase())).wrapping_mul(16777619);
            }
            let bit = (hash >> 22) as usize;
            fingerprint.0[bit / 64] |= 1 << (bit % 64);
        }
        fingerprint
    }

    pub fn includes(self, query: Self) -> bool {
        self.0
            .into_iter()
            .zip(query.0)
            .all(|(file, term)| file & term == term)
    }
}

#[derive(Default)]
pub struct Refresh {
    pub parsed: usize,
    pub reused: usize,
    pub removed: usize,
}

#[derive(Clone, Debug)]
pub struct Definition {
    pub qualified: String,
    pub kind: String,
    pub location: Location,
    pub declaration: usize,
    pub test: bool,
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub from: Option<usize>,
    pub to: usize,
    pub kind: Kind,
    pub location: Location,
    pub inferred: bool,
}

pub struct Index {
    pub definitions: Vec<Definition>,
    pub edges: Vec<Edge>,
    pub errors: BTreeMap<String, String>,
    pub refresh: Refresh,
    pub files: BTreeMap<String, String>,
    pub fingerprints: BTreeMap<String, Fingerprint>,
    by_file: BTreeMap<String, Vec<usize>>,
    by_name: BTreeMap<String, Vec<usize>>,
    incoming: Vec<Vec<usize>>,
    outgoing: Vec<Vec<usize>>,
}

impl Index {
    pub fn open(workspace: &Workspace) -> Result<Self> {
        workspace.prepare_state()?;
        let path = workspace.state.join("index.json");
        let _guard = lock_state(&workspace.state.join("index.lock"))?;
        let mut cache: Cache = match File::open(&path) {
            Ok(file) => serde_json::from_reader(file).unwrap_or_default(),
            Err(error) if error.kind() == ErrorKind::NotFound => Cache::default(),
            Err(error) => return Err(error).context("open source index"),
        };
        let rebuild = cache.version != VERSION;
        if rebuild {
            cache = Cache {
                version: VERSION,
                files: BTreeMap::new(),
            };
        }
        let candidates = inputs(workspace)?;
        let mut manifests = BTreeMap::new();
        let mut found = BTreeSet::new();
        let mut refresh = Refresh::default();
        let mut changed = rebuild;
        let mut parsers = Parsers::default();
        let max_bytes = workspace.settings()?.max_file_bytes;
        for file in candidates {
            let Ok(path) = workspace.source_path(&file) else {
                continue;
            };
            let metadata = match path.metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error).with_context(|| format!("stat {file}")),
            };
            if metadata.len() > max_bytes {
                continue;
            }
            let stamp = file_stamp(&metadata);
            let grammar = Grammar::for_file(&file);
            let structured = is_rust(&file) || grammar.is_some();
            let manifest = Path::new(&file)
                .file_name()
                .is_some_and(|name| name == "Cargo.toml");
            if !manifest
                && stamp.is_some()
                && cache
                    .files
                    .get(&file)
                    .is_some_and(|parsed| parsed.stamp == stamp)
            {
                found.insert(file);
                refresh.reused += usize::from(structured);
                continue;
            }
            let bytes = match fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error).with_context(|| format!("read {file}")),
            };
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            if text.contains('\0') {
                continue;
            }
            if manifest && let Ok(document) = text.parse::<toml_edit::DocumentMut>() {
                manifests.insert(file.clone(), document);
            }
            let digest = source_digest(&bytes);
            found.insert(file.clone());
            if let Some(parsed) = cache.files.get_mut(&file)
                && parsed.digest == digest
            {
                refresh.reused += usize::from(structured);
                if parsed.stamp != stamp {
                    parsed.stamp = stamp;
                    changed = true;
                }
                continue;
            }
            let (syntax, foreign, error) = if is_rust(&file) {
                match rust_syntax::parse(text) {
                    Ok(syntax) => (syntax, None, None),
                    Err(error) => (
                        Syntax::default(),
                        None,
                        Some(format!("line {}: {error}", error.span().start().line)),
                    ),
                }
            } else if let Some(grammar) = grammar {
                match parsers.parse(grammar, text) {
                    Ok(facts) => (Syntax::default(), Some(facts), None),
                    Err(error) => (Syntax::default(), None, Some(error.to_string())),
                }
            } else {
                (Syntax::default(), None, None)
            };
            cache.files.insert(
                file,
                Parsed {
                    digest,
                    syntax,
                    error,
                    foreign,
                    fingerprint: Fingerprint::new(text),
                    stamp,
                },
            );
            refresh.parsed += usize::from(structured);
            changed = true;
        }
        cache.files.retain(|file, _| {
            if found.contains(file) {
                return true;
            }
            changed = true;
            refresh.removed += usize::from(is_rust(file) || Grammar::for_file(file).is_some());
            false
        });
        if changed {
            write_json(&path, &cache)?;
        }
        drop(_guard);
        Ok(Self::assemble(cache, &manifests, refresh))
    }

    fn assemble(
        cache: Cache,
        manifests: &BTreeMap<String, toml_edit::DocumentMut>,
        refresh: Refresh,
    ) -> Self {
        let targets = targets(&cache.files, manifests);
        let package_libraries = libraries(&targets);
        let units = source_units(&cache.files, &targets);
        let mut index = Self {
            definitions: Vec::new(),
            edges: Vec::new(),
            errors: cache
                .files
                .iter()
                .filter_map(|(file, parsed)| {
                    parsed
                        .error
                        .as_ref()
                        .map(|error| (file.clone(), error.clone()))
                })
                .collect(),
            files: cache
                .files
                .iter()
                .map(|(file, parsed)| (file.clone(), parsed.digest.clone()))
                .collect(),
            fingerprints: cache
                .files
                .iter()
                .map(|(file, parsed)| (file.clone(), parsed.fingerprint))
                .collect(),
            refresh,
            by_file: BTreeMap::new(),
            by_name: BTreeMap::new(),
            incoming: Vec::new(),
            outgoing: Vec::new(),
        };
        let mut precise = BTreeMap::<(usize, Vec<String>), Vec<usize>>::new();
        let mut origins = Vec::new();
        for unit in &units {
            let parsed = &cache.files[&unit.file];
            origins.push(index.definitions.len());
            for symbol in &parsed.syntax.symbols {
                let id = index.definitions.len();
                let mut path = unit.module.clone();
                path.extend_from_slice(&symbol.path);
                let prefix = unit
                    .target
                    .map(|target| targets[target].name.as_str())
                    .unwrap_or("crate");
                index.definitions.push(Definition {
                    qualified: format!("{prefix}::{}", path.join("::")),
                    kind: symbol.kind.clone(),
                    location: Location {
                        file: unit.file.clone(),
                        start: symbol.start,
                        end: symbol.end,
                    },
                    declaration: symbol.declaration,
                    test: symbol.test,
                });
                index.by_file.entry(unit.file.clone()).or_default().push(id);
                index
                    .by_name
                    .entry(symbol.name.clone())
                    .or_default()
                    .push(id);
                precise
                    .entry((unit.namespace(), path))
                    .or_default()
                    .push(id);
            }
        }

        let mut imports = BTreeMap::<(usize, Vec<String>), Vec<Binding>>::new();
        for (unit, origin) in units.iter().zip(&origins) {
            for import in &cache.files[&unit.file].syntax.imports {
                let module = joined(&unit.module, &import.module);
                imports
                    .entry((unit.namespace(), module))
                    .or_default()
                    .push(Binding {
                        from: import
                            .from
                            .map(|id| origin + id)
                            .filter(|id| index.definitions[*id].kind != "module"),
                        import,
                    });
            }
        }
        let resolver = Resolver {
            precise,
            imports,
            package_libraries,
            targets: &targets,
            definitions: &index.definitions,
            by_name: &index.by_name,
        };
        let mut seen = BTreeSet::new();
        for (unit, origin) in units.iter().zip(&origins) {
            for reference in &cache.files[&unit.file].syntax.references {
                let from = reference.from.map(|id| origin + id);
                if let Some((to, inferred)) = resolver.resolve(unit, from, reference)
                    && seen.insert((from, to, unit.file.clone(), reference.line, reference.kind))
                {
                    index.edges.push(Edge {
                        from,
                        to,
                        kind: reference.kind,
                        inferred,
                        location: Location {
                            file: unit.file.clone(),
                            start: reference.line,
                            end: reference.line,
                        },
                    });
                }
            }
        }
        index.add_foreign(&cache.files);
        index.incoming = vec![Vec::new(); index.definitions.len()];
        index.outgoing = vec![Vec::new(); index.definitions.len()];
        for (id, edge) in index.edges.iter().enumerate() {
            index.incoming[edge.to].push(id);
            if let Some(from) = edge.from {
                index.outgoing[from].push(id);
            }
        }
        index
    }

    fn add_foreign(&mut self, files: &BTreeMap<String, Parsed>) {
        let mut units = BTreeMap::new();
        let mut routes = BTreeMap::<&str, Vec<usize>>::new();
        let mut tables = BTreeMap::<&str, Vec<usize>>::new();
        let mut families = BTreeMap::<(Grammar, &str), Vec<usize>>::new();
        let mut units_by_family = BTreeMap::<Grammar, Vec<&str>>::new();
        for (file, parsed) in files {
            let Some(facts) = &parsed.foreign else {
                continue;
            };
            let module = self.add_definition(
                file,
                Definition {
                    qualified: file.clone(),
                    kind: "file".into(),
                    location: Location {
                        file: file.clone(),
                        start: 1,
                        end: facts.lines,
                    },
                    declaration: 1,
                    test: false,
                },
            );
            let origin = self.definitions.len();
            let family = facts.grammar.family();
            units.insert(file.as_str(), (module, origin, facts));
            units_by_family.entry(family).or_default().push(file);
            for tag in &facts.definitions {
                let id = self.add_definition(
                    &tag.name,
                    Definition {
                        qualified: format!("{file}::{}", tag.path),
                        kind: tag.kind.clone(),
                        location: Location {
                            file: file.clone(),
                            start: tag.start,
                            end: tag.end,
                        },
                        declaration: tag.declaration,
                        test: tag.test,
                    },
                );
                families.entry((family, &tag.name)).or_default().push(id);
                if matches!(tag.kind.as_str(), "table" | "view") {
                    tables.entry(&tag.name).or_default().push(id);
                    if let Some((_, short)) = tag.name.rsplit_once('.') {
                        tables.entry(short).or_default().push(id);
                    }
                }
            }
            for (route, handler) in &facts.routes {
                let id = origin + handler;
                routes.entry(route).or_default().push(id);
                self.by_name.entry(route.clone()).or_default().push(id);
            }
        }
        let mut seen = BTreeSet::new();
        for (&file, &(module, origin, facts)) in &units {
            let family = facts.grammar.family();
            let mut aliases = BTreeMap::<&str, Vec<usize>>::new();
            let mut attach = |from, to, kind, line, inferred| {
                if seen.insert((from, to, kind, line)) {
                    self.edges.push(Edge {
                        from: Some(from),
                        to,
                        kind,
                        inferred,
                        location: Location {
                            file: file.to_owned(),
                            start: line,
                            end: line,
                        },
                    });
                }
            };
            for import in &facts.imports {
                let Some(target) =
                    import_target(file, family, &import.path, &units, &units_by_family)
                else {
                    continue;
                };
                let &(target_module, target_origin, target_facts) = &units[target];
                attach(module, target_module, Kind::Import, import.line, false);
                for (alias, name) in &import.bindings {
                    for (offset, tag) in target_facts.definitions.iter().enumerate() {
                        if name == &tag.name && tag.path == tag.name {
                            aliases
                                .entry(alias)
                                .or_default()
                                .push(target_origin + offset);
                            attach(
                                module,
                                target_origin + offset,
                                Kind::Import,
                                import.line,
                                false,
                            );
                        }
                    }
                }
            }
            for reference in &facts.references {
                let from = reference.from.map_or(module, |id| origin + id);
                let name = reference.name.as_str();
                let resolution = match reference.kind {
                    Kind::Route => only(routes.get(name)).map(|id| (id, true)),
                    Kind::Data => only(tables.get(name)).map(|id| (id, true)),
                    _ => {
                        let local = facts
                            .definitions
                            .iter()
                            .enumerate()
                            .filter(|(_, tag)| tag.name == name && tag.name == tag.path)
                            .map(|(id, _)| origin + id);
                        unique(local)
                            .or_else(|| only(aliases.get(name)))
                            .map(|id| (id, false))
                            .or_else(|| only(families.get(&(family, name))).map(|id| (id, true)))
                    }
                };
                if let Some((to, inferred)) = resolution {
                    attach(from, to, reference.kind, reference.line, inferred);
                }
            }
        }
    }

    fn add_definition(&mut self, name: &str, definition: Definition) -> usize {
        let id = self.definitions.len();
        self.by_file
            .entry(definition.location.file.clone())
            .or_default()
            .push(id);
        self.by_name.entry(name.to_owned()).or_default().push(id);
        self.definitions.push(definition);
        id
    }

    pub fn select(&self, name: &str) -> Result<usize> {
        let name = name.trim_start_matches("::");
        let suffix = format!("::{name}");
        let mut matches: Vec<_> = if name.contains("::") {
            self.definitions
                .iter()
                .enumerate()
                .filter_map(|(id, definition)| {
                    (definition.qualified == name || definition.qualified.ends_with(&suffix))
                        .then_some(id)
                })
                .collect()
        } else {
            self.by_name.get(name).cloned().unwrap_or_default()
        };
        let mut locations = BTreeSet::new();
        matches.retain(|&id| locations.insert(&self.definitions[id].location));
        match matches.as_slice() {
            [symbol] => Ok(*symbol),
            [] => Err(anyhow!("symbol not found: {name}")),
            many => {
                let locations = many
                    .iter()
                    .take(12)
                    .map(|&id| {
                        let definition = &self.definitions[id];
                        format!("  {} at {}", definition.qualified, definition.location)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Err(anyhow!("ambiguous symbol {name}\n{locations}"))
            }
        }
    }

    pub fn at(&self, location: &Location) -> Vec<usize> {
        let mut ids: Vec<_> = self
            .by_file
            .get(&location.file)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&id| {
                let span = &self.definitions[id].location;
                span.start <= location.end && span.end >= location.start
            })
            .collect();
        ids.sort_by_key(|&id| {
            let span = &self.definitions[id].location;
            (span.end - span.start, span.start, id)
        });
        ids
    }

    pub fn incoming(&self, id: usize) -> impl Iterator<Item = &Edge> {
        self.incoming[id].iter().map(|&edge| &self.edges[edge])
    }

    pub fn outgoing(&self, id: usize) -> impl Iterator<Item = &Edge> {
        self.outgoing[id].iter().map(|&edge| &self.edges[edge])
    }

    pub fn path(&self, start: usize, end: usize) -> Option<Vec<(&Edge, bool)>> {
        let mut queue = VecDeque::from([start]);
        let mut previous = BTreeMap::from([(start, None)]);
        while let Some(node) = queue.pop_front() {
            if node == end {
                let mut route = Vec::new();
                let mut current = node;
                while let Some((parent, edge, reverse)) = previous[&current] {
                    route.push((&self.edges[edge], reverse));
                    current = parent;
                }
                route.reverse();
                return Some(route);
            }
            let adjacent = self.outgoing[node]
                .iter()
                .map(|&id| (self.edges[id].to, id, false))
                .chain(
                    self.incoming[node]
                        .iter()
                        .filter_map(|&id| Some((self.edges[id].from?, id, true))),
                );
            for (target, edge, reverse) in adjacent {
                if let std::collections::btree_map::Entry::Vacant(entry) = previous.entry(target) {
                    entry.insert(Some((node, edge, reverse)));
                    queue.push_back(target);
                }
            }
        }
        None
    }

    pub fn related_tests(&self, id: usize, limit: usize) -> Vec<(usize, usize, bool)> {
        let mut queue = VecDeque::from([(id, 0, false)]);
        let mut seen = BTreeSet::from([id]);
        let mut tests = Vec::new();
        while let Some((id, hops, inferred)) = queue.pop_front()
            && tests.len() < limit
        {
            if hops > 0 && self.definitions[id].test {
                tests.push((id, hops, inferred));
            }
            if hops < 8 {
                for edge in self.incoming(id).filter(|edge| is_call(edge.kind)) {
                    if let Some(from) = edge.from
                        && seen.insert(from)
                    {
                        queue.push_back((from, hops + 1, inferred || edge.inferred));
                    }
                }
            }
        }
        tests
    }
}

pub fn is_call(kind: Kind) -> bool {
    matches!(kind, Kind::Call | Kind::Method)
}

fn only(matches: Option<&Vec<usize>>) -> Option<usize> {
    unique(matches?.iter().copied())
}

fn import_target<'a>(
    file: &str,
    family: Grammar,
    source: &str,
    units: &BTreeMap<&'a str, (usize, usize, &polyglot::Facts)>,
    by_family: &BTreeMap<Grammar, Vec<&'a str>>,
) -> Option<&'a str> {
    let parent = Path::new(file).parent()?;
    let exact = |path: &str| units.get_key_value(path).map(|(&file, _)| file);
    match family {
        Grammar::JavaScript if source.starts_with('.') => {
            let path = normalize(&parent.join(source))?;
            exact(&path).or_else(|| {
                let path = Path::new(&path);
                let has_extension = path.extension().is_some();
                for extension in ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"] {
                    if !has_extension {
                        if let Some(file) = exact(path.with_extension(extension).to_str()?) {
                            return Some(file);
                        }
                        if let Some(file) = exact(path.join(format!("index.{extension}")).to_str()?)
                        {
                            return Some(file);
                        }
                    } else if matches!(path.extension()?.to_str()?, "js" | "jsx" | "mjs" | "cjs")
                        && let Some(file) = exact(path.with_extension(extension).to_str()?)
                    {
                        return Some(file);
                    }
                }
                None
            })
        }
        Grammar::Python => {
            let dots = source.bytes().take_while(|&byte| byte == b'.').count();
            let module = source[dots..].replace('.', "/");
            if dots > 0 {
                let base = parent.ancestors().nth(dots - 1)?.join(&module);
                let base = normalize(&base)?;
                if module.is_empty() {
                    return exact(Path::new(&base).join("__init__.py").to_str()?);
                }
                return exact(&format!("{base}.py"))
                    .or_else(|| exact(&format!("{base}/__init__.py")));
            }
            let simple = format!("{module}.py");
            let package = format!("{module}/__init__.py");
            let mut matches = by_family.get(&family)?.iter().copied().filter(|file| {
                Path::new(file).ends_with(&simple) || Path::new(file).ends_with(&package)
            });
            let first = matches.next()?;
            matches.next().is_none().then_some(first)
        }
        Grammar::C => exact(&normalize(&parent.join(source))?),
        _ => None,
    }
}

fn inputs(workspace: &Workspace) -> Result<BTreeSet<String>> {
    let mut patterns = ignore::gitignore::GitignoreBuilder::new(&workspace.root);
    let project_ignores = workspace.root.join(".justlessignore");
    if project_ignores.is_file()
        && let Some(error) = patterns.add(&project_ignores)
    {
        return Err(error).context("read .justlessignore");
    }
    let project_ignores = patterns.build()?;
    if workspace
        .root
        .ancestors()
        .any(|path| path.join(".git").exists())
    {
        let output = workspace.git(&[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            ".",
        ])?;
        ensure!(
            output.status.success(),
            "list repository files: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return Ok(nul_paths(&output.stdout)
            .filter(|file| {
                is_input(file)
                    && !project_ignores
                        .matched_path_or_any_parents(file, false)
                        .is_ignore()
            })
            .map(str::to_owned)
            .collect());
    }
    let mut files = BTreeSet::new();
    for entry in ignore::WalkBuilder::new(&workspace.root)
        .hidden(false)
        .require_git(false)
        .add_custom_ignore_filename(".justlessignore")
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry.file_type().is_some_and(|kind| kind.is_dir())
                || !excluded(&entry.file_name().to_string_lossy())
        })
        .build()
    {
        let entry = entry?;
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && let Some(file) = entry.path().strip_prefix(&workspace.root)?.to_str()
            && is_input(file)
        {
            files.insert(file.to_owned());
        }
    }
    Ok(files)
}

fn excluded(name: &str) -> bool {
    matches!(
        name,
        ".git" | ".hg" | ".svn" | ".justless" | "target" | "node_modules" | ".venv" | "__pycache__"
    )
}

fn is_input(file: &str) -> bool {
    let path = Path::new(file);
    !path
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .any(|part| excluded(&part.as_os_str().to_string_lossy()))
}

fn is_rust(file: &str) -> bool {
    Path::new(file)
        .extension()
        .is_some_and(|extension| extension == "rs")
}

pub fn nul_paths(bytes: &[u8]) -> impl Iterator<Item = &str> {
    bytes.split(|&byte| byte == 0).filter_map(|bytes| {
        std::str::from_utf8(bytes)
            .ok()
            .filter(|value| !value.is_empty())
    })
}

struct Target {
    entry: String,
    name: String,
    package: String,
    library: bool,
    dependencies: BTreeMap<String, String>,
}

fn targets(
    files: &BTreeMap<String, Parsed>,
    manifests: &BTreeMap<String, toml_edit::DocumentMut>,
) -> Vec<Target> {
    let mut targets = Vec::new();
    for (path, manifest) in manifests {
        let Some(package) = manifest
            .get("package")
            .and_then(|item| item.get("name"))
            .and_then(|item| item.as_str())
        else {
            continue;
        };
        let package = rust_name(package);
        let directory = Path::new(path).parent().unwrap();
        let dependencies = ["dependencies", "dev-dependencies", "build-dependencies"]
            .into_iter()
            .filter_map(|kind| manifest.get(kind).and_then(|item| item.as_table_like()))
            .flat_map(|table| table.iter())
            .map(|(name, item)| {
                let declared = item
                    .get("package")
                    .and_then(|item| item.as_str())
                    .unwrap_or(name);
                (rust_name(name), rust_name(declared))
            })
            .collect::<BTreeMap<_, _>>();
        let mut entries = BTreeSet::new();
        let mut add = |path: &str, name: &str, library| {
            if let Some(entry) = normalize(&directory.join(path))
                && files.contains_key(&entry)
                && entries.insert(entry.clone())
            {
                targets.push(Target {
                    entry,
                    name: rust_name(name),
                    package: package.clone(),
                    library,
                    dependencies: dependencies.clone(),
                });
            }
        };
        let lib = manifest.get("lib");
        add(
            lib.and_then(|it| it.get("path"))
                .and_then(|it| it.as_str())
                .unwrap_or("src/lib.rs"),
            lib.and_then(|it| it.get("name"))
                .and_then(|it| it.as_str())
                .unwrap_or(&package),
            true,
        );
        for kind in ["bin", "test", "example", "bench"] {
            for item in manifest
                .get(kind)
                .and_then(|item| item.as_array_of_tables())
                .into_iter()
                .flatten()
            {
                if let Some(name) = item.get("name").and_then(|item| item.as_str())
                    && let Some(path) = item.get("path").and_then(|item| item.as_str())
                {
                    add(path, name, false);
                }
            }
        }
        add("src/main.rs", &format!("{package}_bin"), false);
        add("build.rs", &format!("{package}_build"), false);
        for (prefix, kind) in [
            ("src/bin", "bin"),
            ("tests", "test"),
            ("examples", "example"),
            ("benches", "bench"),
        ] {
            let base = directory.join(prefix);
            for file in files.keys() {
                let Ok(relative) = Path::new(file).strip_prefix(&base) else {
                    continue;
                };
                let name = match relative.components().count() {
                    1 if relative.file_name().is_some_and(|name| name != "mod.rs") => {
                        relative.file_stem().and_then(|name| name.to_str())
                    }
                    2 if relative.file_name().is_some_and(|name| name == "main.rs") => {
                        relative.parent().and_then(|path| path.to_str())
                    }
                    _ => None,
                };
                if let Some(name) = name {
                    let path = Path::new(file)
                        .strip_prefix(directory)
                        .unwrap()
                        .to_str()
                        .unwrap();
                    add(path, &format!("{package}_{kind}_{name}"), false);
                }
            }
        }
    }
    targets
}

fn rust_name(name: &str) -> String {
    name.replace('-', "_")
}

fn libraries(targets: &[Target]) -> BTreeMap<String, Vec<usize>> {
    let mut names = BTreeMap::<String, Vec<usize>>::new();
    for (id, target) in targets
        .iter()
        .enumerate()
        .filter(|(_, target)| target.library)
    {
        names.entry(target.name.clone()).or_default().push(id);
        if target.package != target.name {
            names.entry(target.package.clone()).or_default().push(id);
        }
    }
    names
}

fn normalize(path: &Path) -> Option<String> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir if relative.pop() => {}
            _ => return None,
        }
    }
    relative.to_str().map(str::to_owned)
}

struct Unit {
    file: String,
    target: Option<usize>,
    module: Vec<String>,
}

impl Unit {
    fn namespace(&self) -> usize {
        self.target.unwrap_or(usize::MAX)
    }
}

fn source_units(files: &BTreeMap<String, Parsed>, targets: &[Target]) -> Vec<Unit> {
    let mut units = Vec::new();
    let mut covered = BTreeSet::new();
    for (target_id, target) in targets.iter().enumerate() {
        let mut queue = VecDeque::from([(target.entry.clone(), Vec::new(), BTreeSet::new())]);
        while let Some((file, module, mut ancestry)) = queue.pop_front() {
            if !ancestry.insert(file.clone()) {
                continue;
            }
            covered.insert(file.clone());
            let path = Path::new(&file);
            let directory = path.parent().unwrap();
            let base =
                if file == target.entry || path.file_name().is_some_and(|name| name == "mod.rs") {
                    directory.to_path_buf()
                } else {
                    directory.join(path.file_stem().unwrap())
                };
            for child in &files[&file].syntax.modules {
                let Some((name, inline)) = child.path.split_last() else {
                    continue;
                };
                let mut directory = base.clone();
                directory.extend(inline);
                let options = if let Some(path) = &child.override_path {
                    let parent = if inline.is_empty() {
                        Path::new(&file).parent().unwrap()
                    } else {
                        &directory
                    };
                    vec![parent.join(path)]
                } else {
                    vec![
                        directory.join(format!("{name}.rs")),
                        directory.join(name).join("mod.rs"),
                    ]
                };
                for child_file in options
                    .iter()
                    .filter_map(|path| normalize(path))
                    .filter(|path| files.contains_key(path))
                {
                    queue.push_back((child_file, joined(&module, &child.path), ancestry.clone()));
                }
            }
            units.push(Unit {
                file,
                target: Some(target_id),
                module,
            });
        }
    }
    for file in files
        .keys()
        .filter(|file| is_rust(file) && !covered.contains(*file))
    {
        let path = Path::new(file);
        let target = targets
            .iter()
            .enumerate()
            .filter(|(_, target)| {
                target.library
                    && Path::new(&target.entry)
                        .parent()
                        .is_some_and(|parent| path.starts_with(parent))
            })
            .max_by_key(|(_, target)| Path::new(&target.entry).components().count());
        let base = target
            .map(|(_, target)| Path::new(&target.entry).parent().unwrap())
            .unwrap_or(Path::new(""));
        let relative = path.strip_prefix(base).unwrap().with_extension("");
        let module = relative
            .components()
            .filter_map(|part| part.as_os_str().to_str())
            .filter(|name| *name != "mod")
            .map(str::to_owned)
            .collect();
        units.push(Unit {
            file: file.clone(),
            target: target.map(|(id, _)| id),
            module,
        });
    }
    units
}

fn joined(left: &[String], right: &[String]) -> Vec<String> {
    left.iter().chain(right).cloned().collect()
}

struct Binding<'a> {
    from: Option<usize>,
    import: &'a Import,
}

struct Resolver<'a> {
    precise: BTreeMap<(usize, Vec<String>), Vec<usize>>,
    imports: BTreeMap<(usize, Vec<String>), Vec<Binding<'a>>>,
    package_libraries: BTreeMap<String, Vec<usize>>,
    targets: &'a [Target],
    definitions: &'a [Definition],
    by_name: &'a BTreeMap<String, Vec<usize>>,
}

impl Resolver<'_> {
    fn resolve(
        &self,
        unit: &Unit,
        from: Option<usize>,
        reference: &Reference,
    ) -> Option<(usize, bool)> {
        let namespace = unit.namespace();
        let path = &reference.path;
        let module = joined(&unit.module, &reference.module);
        if reference.inferred_receiver {
            let name = path.first()?;
            let candidates = self
                .by_name
                .get(name)?
                .iter()
                .copied()
                .filter(|&id| self.definitions[id].kind == "method");
            return unique(candidates).map(|id| (id, true));
        }
        if let Some(from) = from
            && reference.kind != Kind::Import
            && path.len() == 1
        {
            let definition = &self.definitions[from];
            let mut scope: Vec<_> = definition
                .qualified
                .split("::")
                .skip(1)
                .map(str::to_owned)
                .collect();
            if definition.kind == "function"
                && let Some(id) = self.exact(namespace, joined(&scope, path))
            {
                return Some((id, false));
            }
            if scope.pop().is_some()
                && scope.len() > module.len()
                && let Some(id) = self.exact(namespace, joined(&scope, path))
            {
                return Some((id, false));
            }
        }
        if let Some(id) = self.resolve_path(
            namespace,
            &module,
            from,
            path,
            reference.kind == Kind::Import,
            0,
        ) {
            return Some((id, false));
        }
        None
    }

    fn exact(&self, namespace: usize, path: Vec<String>) -> Option<usize> {
        let matches = self.precise.get(&(namespace, path))?;
        unique(matches.iter().copied())
    }

    fn resolve_path(
        &self,
        namespace: usize,
        module: &[String],
        from: Option<usize>,
        path: &[String],
        importing: bool,
        depth: usize,
    ) -> Option<usize> {
        let (first, rest) = path.split_first()?;
        if depth >= 12 {
            return None;
        }
        match first.as_str() {
            "crate" => return self.resolve_path(namespace, &[], None, rest, importing, depth + 1),
            "self" => {
                return self.resolve_path(namespace, module, None, rest, importing, depth + 1);
            }
            "super" => {
                return self.resolve_path(
                    namespace,
                    module.split_last()?.1,
                    None,
                    rest,
                    importing,
                    depth + 1,
                );
            }
            _ => {}
        }
        if let Some(id) = self.exact(namespace, joined(module, path)) {
            return Some(id);
        }
        if !importing && let Some(bindings) = self.imports.get(&(namespace, module.to_vec())) {
            let mut candidates = BTreeSet::new();
            for binding in bindings
                .iter()
                .filter(|binding| binding.from.is_none() || binding.from == from)
            {
                let expanded = match &binding.import.alias {
                    Some(alias) if alias == first => joined(&binding.import.path, rest),
                    None => joined(&binding.import.path, path),
                    _ => continue,
                };
                if let Some(id) =
                    self.resolve_path(namespace, module, from, &expanded, true, depth + 1)
                {
                    candidates.insert(id);
                }
            }
            if let Some(id) = unique(candidates.into_iter()) {
                return Some(id);
            }
        }
        let target = self.targets.get(namespace)?;
        let package = target.dependencies.get(first).unwrap_or(first);
        let library = self.package_libraries.get(package).and_then(|ids| {
            unique(ids.iter().copied().filter(|&id| {
                target.dependencies.contains_key(first)
                    || self.targets[id].package == target.package
            }))
        })?;
        self.resolve_path(library, &[], None, rest, false, depth + 1)
    }
}

fn unique(mut items: impl Iterator<Item = usize>) -> Option<usize> {
    let first = items.next()?;
    items.all(|item| item == first).then_some(first)
}
