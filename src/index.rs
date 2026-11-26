use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

use crate::capture::source_digest;
use crate::polyglot::{self, Grammar, Parsers};
use crate::rust_syntax::{self, Import, Kind, Reference, Syntax};
use crate::workspace::{Location, Workspace, display_path, lock_state};

pub(crate) const VERSION: u32 = 10;

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    version: u32,
    files: BTreeMap<String, Parsed>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Parsed {
    digest: String,
    syntax: Syntax,
    error: Option<String>,
    foreign: Option<polyglot::Facts>,
    fingerprint: Fingerprint,
    stamp: Option<FileStamp>,
}

impl Parsed {
    /// Whether a non-Rust file was parsed as source, successfully or not.
    fn structured(&self) -> bool {
        self.foreign.is_some() || self.error.is_some()
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct FileStamp {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    pub(crate) bytes: u64,
    pub(crate) modified: (i64, i64),
    pub(crate) changed: (i64, i64),
}

#[cfg(unix)]
pub(crate) fn file_stamp(metadata: &fs::Metadata) -> Option<FileStamp> {
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
pub(crate) fn file_stamp(_: &fs::Metadata) -> Option<FileStamp> {
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
    /// Files left out of the index, with the reason, such as their size.
    pub skipped: BTreeMap<String, String>,
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
    /// How a command printed beside these results starts, so it reaches the same
    /// workspace. See [`Workspace::justless`].
    pub justless: String,
    by_file: BTreeMap<String, Vec<usize>>,
    by_name: BTreeMap<String, Vec<usize>>,
    incoming: Vec<Vec<usize>>,
    outgoing: Vec<Vec<usize>>,
    /// The parsed files behind the index, kept to rebuild it with removed
    /// definitions restored. An index built that way has none.
    source: Option<Box<Source>>,
}

enum Source {
    Loaded {
        cache: Cache,
        manifests: BTreeMap<String, toml_edit::DocumentMut>,
    },
    Lazy {
        raw_bytes: Vec<u8>,
        grammars: BTreeMap<String, Grammar>,
        loaded: OnceLock<Option<(Cache, BTreeMap<String, toml_edit::DocumentMut>)>>,
    },
}

impl Source {
    fn load(&self) -> Option<(&Cache, &BTreeMap<String, toml_edit::DocumentMut>)> {
        match self {
            Self::Loaded { cache, manifests } => Some((cache, manifests)),
            Self::Lazy {
                raw_bytes, loaded, ..
            } => loaded
                .get_or_init(|| binary::decode_raw_section(raw_bytes).ok())
                .as_ref()
                .map(|(cache, manifests)| (cache, manifests)),
        }
    }

    fn grammar(&self, file: &str) -> Option<Grammar> {
        match self {
            Self::Loaded { cache, .. } => Some(cache.files.get(file)?.foreign.as_ref()?.grammar),
            Self::Lazy { grammars, .. } => grammars.get(file).copied(),
        }
    }
}

/// A definition that a change removed, as the committed version of its file
/// declared it.
pub(crate) enum Removed {
    Rust(rust_syntax::Symbol),
    Foreign(polyglot::Definition),
}

/// Parsed files that replace their cached versions while an index is built,
/// with removed definitions appended after the file's own.
#[derive(Default)]
struct Overlay {
    files: BTreeMap<String, Parsed>,
    /// Per file, how many definitions it has of its own, and which removed
    /// definition each appended one restores.
    appended: BTreeMap<String, (usize, Vec<usize>)>,
}

impl Overlay {
    fn parsed<'a>(&'a self, cache: &'a Cache, file: &str) -> &'a Parsed {
        self.files.get(file).unwrap_or(&cache.files[file])
    }

    /// The removed definition behind the definition at `position` in `file`.
    fn restores(&self, file: &str, position: usize) -> Option<usize> {
        let (own, entries) = self.appended.get(file)?;
        entries.get(position.checked_sub(*own)?).copied()
    }
}

impl Index {
    pub fn open(workspace: &Workspace) -> Result<Self> {
        workspace.prepare_state()?;
        let json_path = workspace.state.join("index.json");
        let bin_path = workspace.state.join("index.bin");
        // Writers replace the cache with an atomic rename, and every entry is
        // checked against its file before reuse, so reading needs no lock.
        let bin_bytes = read_bin_if_active(&bin_path, &json_path)?;
        let mut refresh = Refresh::default();
        let candidates = inputs(workspace, &mut refresh.skipped)?;
        let max_bytes = workspace.settings()?.max_file_bytes;

        // Warm fast path: if `.justless/index.bin` has a valid pre-assembled
        // graph and every workspace file matches its cached stamp, skip
        // decoding raw AST references and re-running the resolver.
        if let Some(bytes) = bin_bytes.as_deref()
            && let Ok(sections) = binary::split_sections(bytes)
            && sections.version == VERSION
            && let Some(mut index) = try_fast_open(
                workspace,
                &candidates,
                max_bytes,
                &refresh.skipped,
                sections,
            )
        {
            index.justless = workspace.justless();
            return Ok(index);
        }

        let (mut cache, migrated) = load_raw_cache(bin_bytes.as_deref(), &json_path)?;
        let rebuild = cache.version != VERSION;
        if rebuild {
            cache = Cache {
                version: VERSION,
                files: BTreeMap::new(),
            };
        }
        let mut manifests = BTreeMap::new();
        let mut manifests_raw = BTreeMap::new();
        let mut found = BTreeSet::new();
        let mut changed = rebuild || migrated;
        let mut parsers = Parsers::default();
        for file in candidates {
            let Ok(path) = workspace.source_path(&file) else {
                continue;
            };
            // One unreadable file must not stop navigation for the whole project.
            let metadata = match path.metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    refresh.skipped.insert(file, format!("unreadable: {error}"));
                    continue;
                }
            };
            if metadata.len() > max_bytes {
                // Large binaries such as media are not text, whatever the limit.
                if !looks_binary(&path) {
                    refresh.skipped.insert(
                        file,
                        format!(
                            "{} bytes, over max_file_bytes ({max_bytes})",
                            metadata.len()
                        ),
                    );
                }
                continue;
            }
            let stamp = file_stamp(&metadata);
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
                refresh.reused += usize::from(is_rust(&file) || cache.files[&file].structured());
                found.insert(file);
                continue;
            }
            let bytes = match fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    refresh.skipped.insert(file, format!("unreadable: {error}"));
                    continue;
                }
            };
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            if text.contains('\0') {
                continue;
            }
            if manifest && let Ok(document) = text.parse::<toml_edit::DocumentMut>() {
                manifests.insert(file.clone(), document);
                manifests_raw.insert(file.clone(), text.to_owned());
            }
            let digest = source_digest(&bytes);
            // Extensionless scripts and `.m` files need their text to pick a grammar.
            let grammar = Grammar::detect(&file, text);
            let structured = is_rust(&file) || grammar.is_some();
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
        cache.files.retain(|file, parsed| {
            if found.contains(file) {
                return true;
            }
            changed = true;
            refresh.removed += usize::from(is_rust(file) || parsed.structured());
            false
        });
        let (mut index, _) = Self::assemble(&cache, &manifests, refresh, &Overlay::default());
        if changed {
            // A writer that cannot get the lock still answers from what it parsed.
            match lock_state(&workspace.state.join("index.lock")) {
                Ok(_guard) => save_cache(&bin_path, &json_path, &cache, &manifests_raw, &index)?,
                Err(error) => eprintln!("justless: index cache not saved: {error:#}"),
            }
        }
        index.source = Some(Box::new(Source::Loaded { cache, manifests }));
        index.justless = workspace.justless();
        Ok(index)
    }

    /// This index rebuilt with each removed definition restored to its file,
    /// so references resolve as if it still existed, and the ids each one took
    /// there (several when its file belongs to several targets, none when it
    /// could not be restored). Lines of surviving code keep their numbers.
    /// The Rust files in `wider`, given with their current text, also yield the
    /// references inside every macro call that parses as expressions.
    pub(crate) fn with_removed(
        &self,
        removed: &[(&str, &Removed)],
        wider: &[(&str, &str)],
    ) -> Option<(Index, Vec<Vec<usize>>)> {
        let (cache, manifests) = self.source.as_ref()?.load()?;
        let mut overlay = Overlay::default();
        for &(file, text) in wider {
            if let Some(cached) = cache.files.get(file)
                && cached.error.is_none()
                && is_rust(file)
                && let Ok(syntax) = rust_syntax::parse_macros(text)
            {
                let mut parsed = cached.clone();
                parsed.syntax = syntax;
                overlay.files.insert(file.to_owned(), parsed);
            }
        }
        for (entry, &(file, definition)) in removed.iter().enumerate() {
            let Some(cached) = cache.files.get(file) else {
                continue;
            };
            let parsed = overlay
                .files
                .entry(file.to_owned())
                .or_insert_with(|| cached.clone());
            let own = match &parsed.foreign {
                Some(facts) => facts.definitions.len(),
                None => parsed.syntax.symbols.len(),
            };
            match (definition, &mut parsed.foreign) {
                (Removed::Rust(symbol), None) if is_rust(file) => {
                    parsed.syntax.symbols.push(symbol.clone())
                }
                (Removed::Foreign(tag), Some(facts)) => facts.definitions.push(tag.clone()),
                _ => continue,
            }
            overlay
                .appended
                .entry(file.to_owned())
                .or_insert((own, Vec::new()))
                .1
                .push(entry);
        }
        let (mut index, ids) = Self::assemble(cache, manifests, Refresh::default(), &overlay);
        let mut restored = vec![Vec::new(); removed.len()];
        for (entry, id) in ids {
            restored[entry].push(id);
        }
        // Keep the files parsed for this index, for `rust_paths_at`.
        index.source = Some(Box::new(Source::Loaded {
            cache: Cache {
                version: VERSION,
                files: overlay.files,
            },
            manifests: BTreeMap::new(),
        }));
        Some((index, restored))
    }

    /// The paths the Rust parser recorded on `line` of `file`, with their
    /// kind, or `None` when this index did not parse `file` itself.
    pub(crate) fn rust_paths_at(&self, file: &str, line: usize) -> Option<Vec<(Kind, &[String])>> {
        let (cache, _) = self.source.as_ref()?.load()?;
        let parsed = cache.files.get(file)?;
        Some(
            parsed
                .syntax
                .references
                .iter()
                .filter(|reference| reference.line == line)
                .map(|reference| (reference.kind, reference.path.as_slice()))
                .collect(),
        )
    }

    /// Builds the index from parsed files, with `overlay` in place of the
    /// cached files it replaces. Returns each definition the overlay restored,
    /// as (removed entry, id).
    fn assemble(
        cache: &Cache,
        manifests: &BTreeMap<String, toml_edit::DocumentMut>,
        refresh: Refresh,
        overlay: &Overlay,
    ) -> (Self, Vec<(usize, usize)>) {
        let mut restored = Vec::new();
        let targets = targets(&cache.files, manifests);
        let libraries = libraries(&targets);
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
            justless: "justless".into(),
            by_file: BTreeMap::new(),
            by_name: BTreeMap::new(),
            incoming: Vec::new(),
            outgoing: Vec::new(),
            source: None,
        };
        let parsed = |file: &str| overlay.parsed(cache, file);
        let mut precise = HashMap::<usize, HashMap<Vec<&str>, Vec<usize>>>::new();
        let mut origins = Vec::with_capacity(units.len());
        let mut owners = Vec::new();
        for unit in &units {
            origins.push(index.definitions.len());
            for (position, symbol) in parsed(&unit.file).syntax.symbols.iter().enumerate() {
                let id = index.definitions.len();
                if let Some(entry) = overlay.restores(&unit.file, position) {
                    restored.push((entry, id));
                }
                owners.push(unit.target);
                let mut path = Vec::with_capacity(unit.module.len() + symbol.path.len());
                path.extend(unit.module.iter().map(String::as_str));
                path.extend(symbol.path.iter().map(String::as_str));
                let prefix = unit
                    .target
                    .map(|target| targets[target].name.as_str())
                    .unwrap_or("crate");
                index.definitions.push(Definition {
                    qualified: display_path(&format!("{prefix}::{}", path.join("::"))).into_owned(),
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
                    .entry(unit.namespace())
                    .or_default()
                    .entry(path)
                    .or_default()
                    .push(id);
            }
        }

        let mut imports = HashMap::<usize, HashMap<Vec<&str>, Vec<Binding<'_>>>>::new();
        for (unit, origin) in units.iter().zip(&origins) {
            for import in &parsed(&unit.file).syntax.imports {
                let mut module = Vec::with_capacity(unit.module.len() + import.module.len());
                module.extend(unit.module.iter().map(String::as_str));
                module.extend(import.module.iter().map(String::as_str));
                imports
                    .entry(unit.namespace())
                    .or_default()
                    .entry(module)
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
            aliases: imports
                .values()
                .flat_map(HashMap::values)
                .flatten()
                .filter_map(|binding| binding.import.alias.as_deref())
                .collect(),
            precise,
            imports,
            libraries,
            reach: reachable_packages(&targets),
            owners,
            targets: &targets,
            definitions: &index.definitions,
            by_name: index
                .by_name
                .iter()
                .map(|(name, ids)| (name.as_str(), ids.as_slice()))
                .collect(),
        };
        let mut seen = HashSet::new();
        let mut resolved = Vec::new();
        let mut unit_module = Vec::new();
        let mut ref_path = Vec::new();
        let mut ref_module = Vec::new();
        for (unit, origin) in units.iter().zip(&origins) {
            unit_module.clear();
            unit_module.extend(unit.module.iter().map(String::as_str));
            for reference in &parsed(&unit.file).syntax.references {
                let from = reference.from.map(|id| origin + id);
                resolver.resolve(
                    unit,
                    &unit_module,
                    from,
                    reference,
                    &mut ref_path,
                    &mut ref_module,
                    &mut resolved,
                );
                for &(to, kind, inferred) in &resolved {
                    if seen.insert((from, to, unit.file.as_str(), reference.line, kind as u8)) {
                        index.edges.push(Edge {
                            from,
                            to,
                            kind,
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
        }
        index.add_foreign(cache, overlay, &mut restored);
        index.incoming = vec![Vec::new(); index.definitions.len()];
        index.outgoing = vec![Vec::new(); index.definitions.len()];
        for (id, edge) in index.edges.iter().enumerate() {
            index.incoming[edge.to].push(id);
            if let Some(from) = edge.from {
                index.outgoing[from].push(id);
            }
        }
        (index, restored)
    }

    fn add_foreign(
        &mut self,
        cache: &Cache,
        overlay: &Overlay,
        restored: &mut Vec<(usize, usize)>,
    ) {
        let mut units = BTreeMap::new();
        let mut routes = BTreeMap::<&str, Vec<usize>>::new();
        let mut tables = BTreeMap::<&str, Vec<usize>>::new();
        let mut families = BTreeMap::<(Grammar, &str), Vec<usize>>::new();
        let mut units_by_family = BTreeMap::<Grammar, Vec<&str>>::new();
        for file in cache.files.keys() {
            let Some(facts) = &overlay.parsed(cache, file).foreign else {
                continue;
            };
            let module = self.add_definition(
                file,
                Definition {
                    qualified: display_path(file).into_owned(),
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
            for (position, tag) in facts.definitions.iter().enumerate() {
                if let Some(entry) = overlay.restores(file, position) {
                    restored.push((entry, self.definitions.len()));
                }
                let id = self.add_definition(
                    &tag.name,
                    Definition {
                        qualified: display_path(&format!("{file}::{}", tag.path)).into_owned(),
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
                        // Queries may name a schema-qualified table by its short name.
                        self.by_name.entry(short.to_owned()).or_default().push(id);
                    }
                }
            }
            for (route, handler) in &facts.routes {
                let id = origin + handler;
                routes.entry(route).or_default().push(id);
                self.by_name.entry(route.clone()).or_default().push(id);
            }
        }
        let mut seen = HashSet::new();
        for (&file, &(module, origin, facts)) in &units {
            let family = facts.grammar.family();
            let mut aliases = BTreeMap::<&str, Vec<usize>>::new();
            let mut locals = HashMap::<&str, Option<usize>>::new();
            for (id, tag) in facts.definitions.iter().enumerate() {
                if tag.name == tag.path {
                    locals
                        .entry(tag.name.as_str())
                        .and_modify(|slot| *slot = None)
                        .or_insert(Some(origin + id));
                }
            }
            let mut attach = |from, to, kind, line, inferred| {
                if seen.insert((from, to, kind as u8, line)) {
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
                    _ => locals
                        .get(name)
                        .copied()
                        .flatten()
                        .or_else(|| only(aliases.get(name)))
                        .map(|id| (id, false))
                        .or_else(|| only(families.get(&(family, name))).map(|id| (id, true))),
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

    /// Selects one definition by name, qualified name, or `NAME@FILE:LINE` for
    /// definitions that share a qualified name, such as cfg-gated twins.
    pub fn select(&self, name: &str) -> Result<usize> {
        let name = name.trim_start_matches("::");
        if let Some((symbol, place)) = name.rsplit_once('@')
            && let Some((file, line)) = place.rsplit_once(':')
            && let Ok(line) = line.parse::<usize>()
            && self.by_file.contains_key(file)
        {
            let here = Location {
                file: file.to_owned(),
                start: line,
                end: line,
            };
            let selected = self.select_all(symbol);
            return self
                .at(&here)
                .into_iter()
                .find(|id| selected.contains(id))
                .with_context(|| format!("symbol not found: {symbol} at {file}:{line}"));
        }
        let matches = self.select_all(name);
        match matches.as_slice() {
            [symbol] => Ok(*symbol),
            [] => Err(anyhow!(
                "symbol not found: {name}{}",
                self.skipped_note(true)
                    .map_or_else(String::new, |note| format!("\n{note}"))
            )),
            many => {
                let mut names = BTreeMap::<&str, usize>::new();
                for &id in many {
                    *names.entry(&self.definitions[id].qualified).or_default() += 1;
                }
                let mut locations = many
                    .iter()
                    .take(12)
                    .map(|&id| {
                        let definition = &self.definitions[id];
                        let location = &definition.location;
                        let selector = if names[definition.qualified.as_str()] > 1 {
                            format!(
                                " (select {}@{}:{})",
                                definition.qualified, location.file, location.start
                            )
                        } else {
                            String::new()
                        };
                        format!("  {} at {location}{selector}", definition.qualified)
                    })
                    .collect::<Vec<_>>();
                if many.len() > 12 {
                    locations.push(format!("  … and {} more", many.len() - 12));
                }
                Err(anyhow!("ambiguous symbol {name}\n{}", locations.join("\n")))
            }
        }
    }

    /// The grammar the index parsed a file with, which for extensionless
    /// scripts comes from their `#!` line. An index built with removed
    /// definitions restored answers from the file name alone.
    pub fn grammar(&self, file: &str) -> Option<Grammar> {
        match &self.source {
            Some(source) => source.grammar(file),
            None => Grammar::for_file(file),
        }
    }

    /// Names files that were left out of the index, for a search that found
    /// nothing. A symbol search only mentions files that could define symbols.
    pub fn skipped_note(&self, symbols: bool) -> Option<String> {
        self.skipped_note_where(symbols, |_| true)
    }

    /// [`Index::skipped_note`] for the skipped files `keep` accepts, such as
    /// those a narrowed search would have read.
    pub fn skipped_note_where(&self, symbols: bool, keep: impl Fn(&str) -> bool) -> Option<String> {
        let skipped: Vec<_> = self
            .refresh
            .skipped
            .iter()
            .filter(|(file, reason)| {
                keep(file)
                    && (!symbols
                        || reason.starts_with("unreadable")
                        || is_rust(file)
                        || Grammar::for_file(file).is_some())
            })
            .collect();
        let (file, reason) = skipped.first()?;
        let more = match skipped.len() {
            1 => String::new(),
            count => format!(" and {} more", count - 1),
        };
        Some(format!(
            "Not indexed: {file} ({reason}){more}. Run {} index to list them.",
            self.justless
        ))
    }

    fn select_all(&self, name: &str) -> Vec<usize> {
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
        matches
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
        // Innermost first. A function that spans its whole file wins the tie
        // with the file or module that holds it.
        ids.sort_by_key(|&id| {
            let definition = &self.definitions[id];
            let span = &definition.location;
            (
                span.end - span.start,
                matches!(definition.kind.as_str(), "file" | "module"),
                span.start,
                id,
            )
        });
        ids
    }

    /// HTTP routes and their handlers. Route names start with `/`, so they never
    /// collide with symbol or file names.
    pub fn routes(&self) -> impl Iterator<Item = (&str, usize)> {
        self.by_name
            .iter()
            .filter(|(name, _)| name.starts_with('/'))
            .flat_map(|(name, ids)| ids.iter().map(move |&id| (name.as_str(), id)))
    }

    pub fn incoming(&self, id: usize) -> impl Iterator<Item = &Edge> {
        self.incoming[id].iter().map(|&edge| &self.edges[edge])
    }

    pub fn outgoing(&self, id: usize) -> impl Iterator<Item = &Edge> {
        self.outgoing[id].iter().map(|&edge| &self.edges[edge])
    }

    /// Shortest link chain between `start` and `end`, preferring paths with fewer
    /// inferred (`possible`) links before minimizing hop count so exact calls are
    /// never bypassed by a shorter guessed link.
    pub fn path(&self, start: usize, end: usize) -> Option<Vec<(&Edge, bool)>> {
        let mut best = BTreeMap::from([(start, (0usize, 0usize))]);
        let mut previous = BTreeMap::<usize, Option<(usize, usize, bool)>>::from([(start, None)]);
        let mut heap = BinaryHeap::from([Reverse((0usize, 0usize, start))]);
        while let Some(Reverse((inferred, hops, node))) = heap.pop() {
            if best.get(&node).is_some_and(|&cost| cost < (inferred, hops)) {
                continue;
            }
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
                let next_cost = (inferred + usize::from(self.edges[edge].inferred), hops + 1);
                if best.get(&target).is_none_or(|&cost| next_cost < cost) {
                    best.insert(target, next_cost);
                    previous.insert(target, Some((node, edge, reverse)));
                    heap.push(Reverse((next_cost.0, next_cost.1, target)));
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
        // `#include "x.h"` and Julia's `include("x.jl")` name a path from the file.
        Grammar::C | Grammar::Julia => exact(&normalize(&parent.join(source))?),
        _ => None,
    }
}

pub(crate) fn inputs(
    workspace: &Workspace,
    skipped: &mut BTreeMap<String, String>,
) -> Result<BTreeSet<String>> {
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
        // `-t` tags untracked files with `?`, so a tracked `src/target/` module
        // is indexed while an unignored build directory is not.
        let output = workspace.git(&[
            "ls-files",
            "-z",
            "-t",
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
            .filter_map(|record| {
                let (tag, file) = record.split_at_checked(2)?;
                is_input(file, tag != "? ").then_some(file)
            })
            .filter(|file| {
                !project_ignores
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
        // An unreadable directory leaves out its own files, not the project.
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let path = walk_error_path(&error)
                    .and_then(|path| path.strip_prefix(&workspace.root).ok())
                    .and_then(Path::to_str)
                    .filter(|path| !path.is_empty())
                    .unwrap_or(".");
                // The walker wraps the OS error in messages that repeat the path.
                let code = std::iter::successors(
                    error.io_error().map(|io| io as &dyn std::error::Error),
                    |inner| inner.source(),
                )
                .find_map(|inner| inner.downcast_ref::<std::io::Error>()?.raw_os_error());
                let reason = code.map_or_else(
                    || error.to_string(),
                    |code| std::io::Error::from_raw_os_error(code).to_string(),
                );
                skipped.insert(path.to_owned(), format!("unreadable: {reason}"));
                continue;
            }
        };
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && let Some(file) = entry.path().strip_prefix(&workspace.root)?.to_str()
            && is_input(file, false)
        {
            files.insert(file.to_owned());
        }
    }
    Ok(files)
}

fn walk_error_path(error: &ignore::Error) -> Option<&Path> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            walk_error_path(err)
        }
        ignore::Error::Partial(errors) => errors.iter().find_map(walk_error_path),
        _ => None,
    }
}

fn excluded(name: &str) -> bool {
    matches!(
        name,
        ".git" | ".hg" | ".svn" | ".justless" | "target" | "node_modules" | ".venv" | "__pycache__"
    )
}

/// Tracked files may live under a directory named `target`, such as a Rust
/// `mod target;`. Build output under that name is untracked.
fn is_input(file: &str, tracked: bool) -> bool {
    let path = Path::new(file);
    !path
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .map(|part| part.as_os_str().to_string_lossy())
        .any(|part| excluded(&part) && !(tracked && part == "target"))
}

fn looks_binary(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0; 8192];
    fs::File::open(path)
        .and_then(|mut file| file.read(&mut head))
        .map_or(true, |read| head[..read].contains(&0))
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
    /// Directory of the manifest that declares the target.
    directory: String,
    library: bool,
    /// Crate names from the manifest's dependency tables, as `dependencies()` reads them.
    dependencies: BTreeMap<String, Dependency>,
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
        let Some(manifest_directory) = normalize(directory) else {
            continue;
        };
        let dependencies = dependencies(directory, manifest, manifests);
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
                    directory: manifest_directory.clone(),
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

/// Where a crate name from a manifest's dependency tables leads.
#[derive(Clone)]
enum Dependency {
    /// A path dependency: the directory of the crate it points at.
    Path(String),
    /// `name.workspace = true` whose workspace root manifest is outside the
    /// index, so its source is unknown. It reaches a library with that name
    /// when exactly one is indexed.
    Inherited,
    /// A registry or git dependency, or a path outside the index.
    External,
}

/// The crate names a manifest's dependency tables bring into scope, including
/// platform-specific `[target.….dependencies]`. A path dependency maps to the
/// directory of the crate it points at. A registry or git dependency stays
/// external even when a workspace crate has the same package name, as when a
/// crate tests itself against its published release. `name.workspace = true`
/// takes the source from `[workspace.dependencies]` in the workspace root.
fn dependencies(
    directory: &Path,
    manifest: &toml_edit::DocumentMut,
    manifests: &BTreeMap<String, toml_edit::DocumentMut>,
) -> BTreeMap<String, Dependency> {
    let workspace = workspace_root(directory, manifest, manifests);
    let platforms = manifest
        .get("target")
        .and_then(|item| item.as_table_like())
        .into_iter()
        .flat_map(|table| table.iter().map(|(_, item)| item));
    let mut dependencies = BTreeMap::new();
    for table in std::iter::once(manifest.as_item())
        .chain(platforms)
        .flat_map(|scope| {
            ["dependencies", "dev-dependencies", "build-dependencies"]
                .into_iter()
                .filter_map(|kind| scope.get(kind).and_then(|item| item.as_table_like()))
        })
    {
        for (name, item) in table.iter() {
            let source = if item.get("workspace").and_then(|item| item.as_bool()) == Some(true) {
                match &workspace {
                    Some((root, document)) => document
                        .get("workspace")
                        .and_then(|item| item.get("dependencies"))
                        .and_then(|item| item.get(name))
                        .map_or(Dependency::External, |entry| path_dependency(root, entry)),
                    None => Dependency::Inherited,
                }
            } else {
                path_dependency(directory, item)
            };
            // A crate declared in several tables, such as `[dependencies]` and a
            // `[target.….dependencies]`, resolves through the first path any of them names.
            let entry = dependencies
                .entry(rust_name(name))
                .or_insert(Dependency::External);
            match (&*entry, &source) {
                (Dependency::Path(_), _) | (Dependency::Inherited, Dependency::External) => {}
                _ => *entry = source,
            }
        }
    }
    dependencies
}

fn path_dependency(base: &Path, entry: &toml_edit::Item) -> Dependency {
    entry
        .get("path")
        .and_then(|item| item.as_str())
        .and_then(|path| normalize(&base.join(path)))
        .map_or(Dependency::External, Dependency::Path)
}

/// The workspace root manifest and its directory for the manifest in
/// `directory`: the manifest itself when it declares `[workspace]`, the root
/// that `package.workspace` names, or the nearest enclosing `[workspace]`.
fn workspace_root<'a>(
    directory: &Path,
    manifest: &'a toml_edit::DocumentMut,
    manifests: &'a BTreeMap<String, toml_edit::DocumentMut>,
) -> Option<(PathBuf, &'a toml_edit::DocumentMut)> {
    if manifest.contains_key("workspace") {
        return Some((directory.to_path_buf(), manifest));
    }
    let candidates = match manifest
        .get("package")
        .and_then(|item| item.get("workspace"))
        .and_then(|item| item.as_str())
    {
        Some(root) => vec![directory.join(root)],
        None => directory
            .ancestors()
            .skip(1)
            .map(Path::to_path_buf)
            .collect(),
    };
    candidates.into_iter().find_map(|root| {
        let root = PathBuf::from(normalize(&root)?);
        let document = manifests.get(&normalize(&root.join("Cargo.toml"))?)?;
        document
            .contains_key("workspace")
            .then_some((root, document))
    })
}

fn rust_name(name: &str) -> String {
    name.replace('-', "_")
}

/// The library target of each manifest directory.
fn libraries(targets: &[Target]) -> BTreeMap<&str, usize> {
    targets
        .iter()
        .enumerate()
        .filter(|(_, target)| target.library)
        .map(|(id, target)| (target.directory.as_str(), id))
        .collect()
}

/// The package directories whose types code in each target can hold: its own
/// and those it depends on through path dependencies, directly or through other
/// packages in the workspace. A registry or git dependency reaches no indexed
/// package, even one with the same name.
fn reachable_packages(targets: &[Target]) -> Vec<BTreeSet<&str>> {
    let mut dependencies = BTreeMap::<&str, BTreeSet<&str>>::new();
    for target in targets {
        let reached = dependencies.entry(target.directory.as_str()).or_default();
        for (name, dependency) in &target.dependencies {
            match dependency {
                Dependency::Path(directory) => {
                    reached.insert(directory.as_str());
                }
                // Resolved as `resolve_path` does: the one indexed library of that name.
                Dependency::Inherited => {
                    let mut named = targets
                        .iter()
                        .filter(|t| t.library && (t.name == *name || t.package == *name));
                    if let (Some(only), None) = (named.next(), named.next()) {
                        reached.insert(only.directory.as_str());
                    }
                }
                Dependency::External => {}
            }
        }
    }
    targets
        .iter()
        .map(|target| {
            let mut reached = BTreeSet::new();
            let mut queue = vec![target.directory.as_str()];
            while let Some(directory) = queue.pop() {
                if reached.insert(directory) {
                    queue.extend(dependencies.get(directory).into_iter().flatten().copied());
                }
            }
            reached
        })
        .collect()
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

fn joined_slices<'a>(left: &'a [String], right: &[&'a str]) -> Vec<&'a str> {
    let mut out = Vec::with_capacity(left.len() + right.len());
    out.extend(left.iter().map(String::as_str));
    out.extend_from_slice(right);
    out
}

struct Binding<'a> {
    from: Option<usize>,
    import: &'a Import,
}

struct Resolver<'a> {
    aliases: HashSet<&'a str>,
    precise: HashMap<usize, HashMap<Vec<&'a str>, Vec<usize>>>,
    imports: HashMap<usize, HashMap<Vec<&'a str>, Vec<Binding<'a>>>>,
    libraries: BTreeMap<&'a str, usize>,
    /// The package directories each target can reach, by target.
    reach: Vec<BTreeSet<&'a str>>,
    /// The target that defines each Rust definition, by definition.
    owners: Vec<Option<usize>>,
    targets: &'a [Target],
    definitions: &'a [Definition],
    by_name: HashMap<&'a str, &'a [usize]>,
}

/// Import hops one reference may follow. A glob such as `use super::*` spends a
/// hop and can reach the imports of the module it names. A named import ends the chain.
const IMPORT_HOPS: u8 = 3;

impl<'a> Resolver<'a> {
    /// Whether code in `unit` can call a method that definition `id` declares.
    /// A crate can only hold values of its own types and its dependencies', so
    /// `thread::Builder::new().name(…)` in one crate is not a call to a `name`
    /// method of a crate it does not depend on. Only a library is shared: a
    /// method of a test, example, bench, binary or build script is reachable
    /// from that target alone. Code outside a known target can reach any method.
    fn reaches(&self, unit: &Unit, id: usize) -> bool {
        match (unit.target, self.owners[id]) {
            (Some(from), Some(owner)) => {
                owner == from
                    || (self.targets[owner].library
                        && self.reach[from].contains(self.targets[owner].directory.as_str()))
            }
            _ => true,
        }
    }

    /// Targets of one reference: its own target, or every definition that shares
    /// that path (marked possible), plus the type that owns an enum variant or
    /// associated item, such as `Status` in `Status::Open` or `Widget` in `Widget::new()`.
    #[allow(clippy::too_many_arguments)]
    fn resolve(
        &self,
        unit: &'a Unit,
        unit_module: &[&'a str],
        from: Option<usize>,
        reference: &'a Reference,
        ref_path: &mut Vec<&'a str>,
        ref_module: &mut Vec<&'a str>,
        resolved: &mut Vec<(usize, Kind, bool)>,
    ) {
        resolved.clear();
        let path = &reference.path;
        if reference.inferred_receiver {
            let Some(name) = path.first() else { return };
            // Ubiquitous Rust standard-library method names on untyped receivers
            // (such as `Iterator::next`, `len`, `get`, `push`) almost always call
            // stdlib rather than a local method that happens to share the name.
            // Calls with a known receiver type (`self.next()`, `req.next()`) have
            // `inferred_receiver == false` and still resolve normally.
            if is_std_inferred_method(name) {
                return;
            }
            let candidates = self
                .by_name
                .get(name.as_str())
                .copied()
                .into_iter()
                .flatten()
                .copied()
                .filter(|&id| self.definitions[id].kind == "method" && self.reaches(unit, id));
            // A method that calls its own name on a value of unknown type most
            // often forwards to another type, as `alloc` does with
            // `System.alloc(layout)`. A call through `self` already resolved.
            if let Some(id) = unique(candidates).filter(|&id| Some(id) != from) {
                resolved.push((id, reference.kind, true));
            }
            return;
        }
        // Every resolution keeps the last segment, unless a single-segment path
        // is an import alias. Most unresolvable names (std items, locals) stop here.
        let want_full = path.last().is_some_and(|last| {
            self.by_name.contains_key(last.as_str())
                || (path.len() == 1 && self.aliases.contains(last.as_str()))
        });
        let want_parent = path.len() > 1 && {
            let prev = path[path.len() - 2].as_str();
            self.by_name.contains_key(prev) || (path.len() == 2 && self.aliases.contains(prev))
        };
        if !want_full && !want_parent {
            return;
        }
        ref_path.clear();
        ref_path.extend(path.iter().map(String::as_str));
        let module = if reference.module.is_empty() {
            unit_module
        } else {
            ref_module.clear();
            ref_module.extend_from_slice(unit_module);
            ref_module.extend(reference.module.iter().map(String::as_str));
            ref_module.as_slice()
        };
        if want_full && let Some(ids) = self.lookup(unit, module, from, reference, ref_path) {
            let inferred = ids.len() > 1;
            resolved.extend(ids.iter().map(|&id| (id, reference.kind, inferred)));
        }
        if want_parent
            && let Some(ids) = self.lookup(
                unit,
                module,
                from,
                reference,
                &ref_path[..ref_path.len() - 1],
            )
        {
            let kind = if reference.kind == Kind::Import {
                Kind::Import
            } else {
                Kind::Reference
            };
            let inferred = ids.len() > 1;
            resolved.extend(
                ids.iter()
                    .filter(|&&id| {
                        matches!(
                            self.definitions[id].kind.as_str(),
                            "struct" | "enum" | "union" | "trait" | "trait alias" | "type"
                        )
                    })
                    .map(|&id| (id, kind, inferred)),
            );
        }
    }

    fn lookup(
        &self,
        unit: &Unit,
        module: &[&'a str],
        from: Option<usize>,
        reference: &Reference,
        path: &[&'a str],
    ) -> Option<&[usize]> {
        let namespace = unit.namespace();
        if let Some(from) = from
            && reference.kind != Kind::Import
            && path.len() == 1
        {
            let definition = &self.definitions[from];
            let mut scope: Vec<&'a str> = definition.qualified.split("::").skip(1).collect();
            if definition.kind == "function"
                && let Some(ids) = self.exact(namespace, &scope, path)
            {
                return Some(ids);
            }
            if scope.pop().is_some()
                && scope.len() > module.len()
                && let Some(ids) = self.exact(namespace, &scope, path)
            {
                return Some(ids);
            }
        }
        let hops = if reference.kind == Kind::Import {
            0
        } else {
            IMPORT_HOPS
        };
        self.resolve_path(namespace, module, from, path, hops, 0)
    }

    fn exact(&self, namespace: usize, module: &[&'a str], path: &[&'a str]) -> Option<&[usize]> {
        let table = self.precise.get(&namespace)?;
        if module.is_empty() {
            return table.get(path).map(Vec::as_slice);
        }
        if path.is_empty() {
            return table.get(module).map(Vec::as_slice);
        }
        let mut key = Vec::with_capacity(module.len() + path.len());
        key.extend_from_slice(module);
        key.extend_from_slice(path);
        table.get(key.as_slice()).map(Vec::as_slice)
    }

    fn resolve_path(
        &self,
        namespace: usize,
        module: &[&'a str],
        from: Option<usize>,
        path: &[&'a str],
        hops: u8,
        depth: usize,
    ) -> Option<&[usize]> {
        let (&first, rest) = path.split_first()?;
        if depth >= 12 {
            return None;
        }
        match first {
            "crate" => return self.resolve_path(namespace, &[], None, rest, hops, depth + 1),
            "self" => {
                return self.resolve_path(namespace, module, None, rest, hops, depth + 1);
            }
            "super" => {
                return self.resolve_path(
                    namespace,
                    module.split_last()?.1,
                    None,
                    rest,
                    hops,
                    depth + 1,
                );
            }
            _ => {}
        }
        if let Some(ids) = self.exact(namespace, module, path) {
            return Some(ids);
        }
        if hops > 0
            && let Some(bindings) = self
                .imports
                .get(&namespace)
                .and_then(|table| table.get(module))
        {
            let mut found: Option<&[usize]> = None;
            let mut ambiguous = false;
            for binding in bindings
                .iter()
                .filter(|binding| binding.from.is_none() || binding.from == from)
            {
                let (expanded, hops) = match &binding.import.alias {
                    Some(alias) if alias == first => (joined_slices(&binding.import.path, rest), 0),
                    None => (joined_slices(&binding.import.path, path), hops - 1),
                    _ => continue,
                };
                if let Some(ids) =
                    self.resolve_path(namespace, module, from, &expanded, hops, depth + 1)
                {
                    match found {
                        Some(previous) if previous != ids => ambiguous = true,
                        _ => found = Some(ids),
                    }
                }
            }
            if !ambiguous && let Some(ids) = found {
                return Some(ids);
            }
        }
        // An import that binds the first segment shadows any crate of that name,
        // as `use upstream_ctrlc as ctrlc;` does for the local `ctrlc` crate.
        if hops > 0 && self.binds(namespace, module, from, first, hops) {
            return None;
        }
        let target = self.targets.get(namespace)?;
        let library = match target.dependencies.get(first) {
            // A path dependency reaches the library in the directory it names.
            Some(Dependency::Path(directory)) => *self.libraries.get(directory.as_str())?,
            Some(Dependency::Inherited) => {
                unique(self.libraries.values().copied().filter(|&id| {
                    let library = &self.targets[id];
                    library.name == first || library.package == first
                }))?
            }
            // A registry or git dependency is external, whatever its package name.
            Some(Dependency::External) => return None,
            // Binaries, tests, examples and benches reach their own package's library.
            None => {
                let id = *self.libraries.get(target.directory.as_str())?;
                let library = &self.targets[id];
                (library.name == first || library.package == first).then_some(id)?
            }
        };
        self.resolve_path(library, &[], None, rest, IMPORT_HOPS, depth + 1)
    }

    /// Whether an import in `module` binds `name`: a named import or rename, or
    /// a glob of a module of the same crate that binds it, such as `use super::*`
    /// below `use upstream_ctrlc as ctrlc;`. Globs spend hops as in `resolve_path`.
    fn binds(
        &self,
        namespace: usize,
        module: &[&'a str],
        from: Option<usize>,
        name: &str,
        hops: u8,
    ) -> bool {
        let Some(bindings) = self
            .imports
            .get(&namespace)
            .and_then(|table| table.get(module))
        else {
            return false;
        };
        bindings
            .iter()
            .filter(|binding| binding.from.is_none() || binding.from == from)
            .any(|binding| match &binding.import.alias {
                Some(alias) => alias == name,
                None => {
                    hops > 1
                        && local_module(module, &binding.import.path).is_some_and(|source| {
                            source.as_slice() != module
                                && self.binds(namespace, &source, None, name, hops - 1)
                        })
                }
            })
    }
}

/// The module of the same crate that an import path starting with `crate`,
/// `self` or `super` names.
/// Common Rust standard-library trait and collection method names that should not
/// be guessed to point at an unrelated local method when the receiver type is
/// unknown.
fn is_std_inferred_method(name: &str) -> bool {
    matches!(
        name,
        "next"
            | "next_back"
            | "peek"
            | "peekable"
            | "iter"
            | "iter_mut"
            | "into_iter"
            | "map"
            | "filter"
            | "filter_map"
            | "flat_map"
            | "flatten"
            | "collect"
            | "fold"
            | "try_fold"
            | "for_each"
            | "any"
            | "all"
            | "find"
            | "find_map"
            | "position"
            | "rposition"
            | "count"
            | "take"
            | "take_while"
            | "skip"
            | "skip_while"
            | "enumerate"
            | "zip"
            | "chain"
            | "rev"
            | "copied"
            | "cloned"
            | "fuse"
            | "nth"
            | "last"
            | "max"
            | "min"
            | "max_by"
            | "min_by"
            | "max_by_key"
            | "min_by_key"
            | "sum"
            | "product"
            | "partition"
            | "unzip"
            | "dedup"
            | "unwrap"
            | "unwrap_or"
            | "unwrap_or_default"
            | "unwrap_or_else"
            | "expect"
            | "ok"
            | "err"
            | "ok_or"
            | "ok_or_else"
            | "is_some"
            | "is_some_and"
            | "is_none"
            | "is_none_or"
            | "is_ok"
            | "is_err"
            | "and_then"
            | "or_else"
            | "map_or"
            | "map_or_else"
            | "as_ref"
            | "as_mut"
            | "as_deref"
            | "as_deref_mut"
            | "replace"
            | "transpose"
            | "context"
            | "with_context"
            | "clone"
            | "clone_from"
            | "default"
            | "from"
            | "into"
            | "try_from"
            | "try_into"
            | "eq"
            | "ne"
            | "cmp"
            | "partial_cmp"
            | "hash"
            | "fmt"
            | "drop"
            | "deref"
            | "deref_mut"
            | "borrow"
            | "borrow_mut"
            | "to_owned"
            | "to_string"
            | "as_str"
            | "as_bytes"
            | "as_slice"
            | "as_path"
            | "as_os_str"
            | "into_owned"
            | "into_bytes"
            | "into_string"
            | "len"
            | "is_empty"
            | "push"
            | "push_str"
            | "push_front"
            | "push_back"
            | "pop"
            | "pop_front"
            | "pop_back"
            | "get"
            | "get_mut"
            | "first"
            | "first_mut"
            | "last_mut"
            | "split_first"
            | "split_last"
            | "insert"
            | "remove"
            | "retain"
            | "clear"
            | "truncate"
            | "extend"
            | "append"
            | "drain"
            | "contains"
            | "contains_key"
            | "entry"
            | "keys"
            | "values"
            | "values_mut"
            | "sort"
            | "sort_by"
            | "sort_by_key"
            | "sort_unstable"
            | "binary_search"
            | "starts_with"
            | "ends_with"
            | "strip_prefix"
            | "strip_suffix"
            | "trim"
            | "trim_start"
            | "trim_end"
            | "trim_matches"
            | "trim_start_matches"
            | "trim_end_matches"
            | "split"
            | "split_once"
            | "rsplit"
            | "rsplit_once"
            | "split_whitespace"
            | "lines"
            | "chars"
            | "bytes"
            | "char_indices"
            | "to_lowercase"
            | "to_uppercase"
            | "to_ascii_lowercase"
            | "to_ascii_uppercase"
            | "repeat"
            | "join"
            | "parent"
            | "file_name"
            | "file_stem"
            | "extension"
            | "components"
            | "exists"
            | "is_file"
            | "is_dir"
            | "canonicalize"
            | "to_path_buf"
            | "to_string_lossy"
            | "display"
            | "read"
            | "read_to_end"
            | "read_to_string"
            | "write"
            | "write_all"
            | "flush"
            | "saturating_add"
            | "saturating_sub"
            | "saturating_mul"
            | "checked_add"
            | "checked_sub"
            | "saturating_div"
            | "div_ceil"
            | "clamp"
    )
}

fn local_module<'a>(module: &[&'a str], path: &'a [String]) -> Option<Vec<&'a str>> {
    let (first, mut rest) = path.split_first()?;
    let mut base = match first.as_str() {
        "crate" => Vec::new(),
        "self" => module.to_vec(),
        "super" => module.split_last()?.1.to_vec(),
        _ => return None,
    };
    while let Some((first, tail)) = rest.split_first()
        && first == "super"
    {
        base.pop()?;
        rest = tail;
    }
    base.extend(rest.iter().map(String::as_str));
    Some(base)
}

fn unique(mut items: impl Iterator<Item = usize>) -> Option<usize> {
    let first = items.next()?;
    items.all(|item| item == first).then_some(first)
}

/// Reads `.justless/index.bin` when `.justless/index.json` is present.
/// Removing `.justless/index.json` clears the cache so the next open rebuilds.
fn read_bin_if_active(bin_path: &Path, json_path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::metadata(json_path) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("open source index"),
    }
    match fs::read(bin_path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("open source index"),
    }
}

/// Validates workspace files against Section 1 of `.justless/index.bin` and, if
/// every file is unchanged, decodes the pre-assembled graph directly without
/// decoding raw AST references or re-running the resolver.
fn try_fast_open(
    workspace: &Workspace,
    candidates: &BTreeSet<String>,
    max_bytes: u64,
    initial_skipped: &BTreeMap<String, String>,
    sections: binary::Sections<'_>,
) -> Option<Index> {
    let (metas, graph_bytes) = binary::decode_file_metas(sections.fast).ok()?;
    let by_file: HashMap<&str, &binary::FileMeta> = metas
        .iter()
        .map(|meta| (meta.file.as_str(), meta))
        .collect();

    let mut skipped = initial_skipped.clone();
    let mut found_count = 0usize;
    let mut reused = 0usize;
    for file in candidates {
        let Ok(path) = workspace.source_path(file) else {
            continue;
        };
        let metadata = match path.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                skipped.insert(file.clone(), format!("unreadable: {error}"));
                continue;
            }
        };
        if metadata.len() > max_bytes {
            if !looks_binary(&path) {
                skipped.insert(
                    file.clone(),
                    format!(
                        "{} bytes, over max_file_bytes ({max_bytes})",
                        metadata.len()
                    ),
                );
            }
            continue;
        }
        let stamp = file_stamp(&metadata);
        if let Some(meta) = by_file.get(file.as_str()) {
            if stamp.is_some() && meta.stamp == stamp {
                reused += usize::from(meta.structured);
                found_count += 1;
                continue;
            }
            if stamp.is_none() && meta.stamp.is_none() {
                let bytes = fs::read(&path).ok()?;
                if std::str::from_utf8(&bytes).is_err() || bytes.contains(&0) {
                    return None;
                }
                if source_digest(&bytes) != meta.digest {
                    return None;
                }
                reused += usize::from(meta.structured);
                found_count += 1;
                continue;
            }
            return None;
        }
        // Candidate is not in the cached text-file table: only valid if it is a
        // binary or non-UTF-8 file that the index skips.
        match fs::read(&path) {
            Ok(bytes) if std::str::from_utf8(&bytes).is_err() || bytes.contains(&0) => continue,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                skipped.insert(file.clone(), format!("unreadable: {error}"));
                continue;
            }
            Ok(_) => return None,
        }
    }
    if found_count != metas.len() {
        return None;
    }

    let (definitions, edges, by_name) = binary::decode_graph(graph_bytes).ok()?;
    let mut index_by_file: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (id, def) in definitions.iter().enumerate() {
        index_by_file
            .entry(def.location.file.clone())
            .or_default()
            .push(id);
    }
    let mut incoming = vec![Vec::new(); definitions.len()];
    let mut outgoing = vec![Vec::new(); definitions.len()];
    for (id, edge) in edges.iter().enumerate() {
        if edge.to >= incoming.len() {
            return None;
        }
        incoming[edge.to].push(id);
        if let Some(from) = edge.from {
            if from >= outgoing.len() {
                return None;
            }
            outgoing[from].push(id);
        }
    }

    let mut files = BTreeMap::new();
    let mut errors = BTreeMap::new();
    let mut fingerprints = BTreeMap::new();
    let mut grammars = BTreeMap::new();
    for meta in metas {
        if let Some(error) = meta.error {
            errors.insert(meta.file.clone(), error);
        }
        if let Some(grammar) = meta.grammar {
            grammars.insert(meta.file.clone(), grammar);
        }
        fingerprints.insert(meta.file.clone(), meta.fingerprint);
        files.insert(meta.file, meta.digest);
    }

    Some(Index {
        definitions,
        edges,
        errors,
        refresh: Refresh {
            parsed: 0,
            reused,
            removed: 0,
            skipped,
        },
        files,
        fingerprints,
        justless: "justless".into(),
        by_file: index_by_file,
        by_name,
        incoming,
        outgoing,
        source: Some(Box::new(Source::Lazy {
            raw_bytes: sections.raw.to_vec(),
            grammars,
            loaded: OnceLock::new(),
        })),
    })
}

/// Loads the raw parsed-file `Cache` from Section 2 of `.justless/index.bin`,
/// falling back to a legacy `.justless/index.json` when present. Returns
/// `(cache, migrated)` where `migrated` is true whenever `.justless/index.bin`
/// was not used and should be written on save.
fn load_raw_cache(bin_bytes: Option<&[u8]>, json_path: &Path) -> Result<(Cache, bool)> {
    if let Some(bytes) = bin_bytes
        && let Ok(sections) = binary::split_sections(bytes)
        && sections.version == VERSION
        && let Ok(cache) = binary::decode_raw_cache(sections.raw)
    {
        return Ok((cache, false));
    }
    let cache = match fs::read(json_path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(error) if error.kind() == ErrorKind::NotFound => Cache::default(),
        Err(error) => return Err(error).context("open source index"),
    };
    Ok((cache, true))
}

fn save_cache(
    bin_path: &Path,
    json_path: &Path,
    cache: &Cache,
    manifests_raw: &BTreeMap<String, String>,
    index: &Index,
) -> Result<()> {
    let encoded = binary::encode(cache, manifests_raw, index);
    write_bytes(bin_path, &encoded)?;
    let stub = format!("{{\"cache\":\"index.bin\",\"version\":{VERSION}}}\n");
    write_bytes(json_path, stub.as_bytes())?;
    Ok(())
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temporary =
        tempfile::NamedTempFile::new_in(path.parent().context("output directory")?)?;
    temporary.write_all(bytes)?;
    temporary.persist(path)?;
    Ok(())
}

/// Compact two-section string-interned binary codec for `.justless/index.bin`.
///
/// Section 1 holds the file-validation table and the pre-assembled graph so
/// warm `Index::open` calls skip decoding raw AST references and re-running the
/// resolver. Section 2 holds the full string-interned `Cache` and manifest
/// sources for incremental updates and lazy `with_removed` / `rust_paths_at`.
mod binary {
    use std::collections::{BTreeMap, HashMap};

    use anyhow::{Result, bail, ensure};

    use super::{Cache, Definition, Edge, FileStamp, Fingerprint, Index, Parsed, is_rust};
    use crate::polyglot::{self, Grammar};
    use crate::rust_syntax::{Import, Kind, ModuleFile, Reference, Symbol, Syntax};
    use crate::workspace::Location;

    const MAGIC: &[u8; 4] = b"JLIX";
    const FORMAT_VERSION: u32 = 2;

    const META_STRUCTURED: u8 = 1 << 0;
    const META_GRAMMAR: u8 = 1 << 1;
    const META_ERROR: u8 = 1 << 2;
    const META_STAMP: u8 = 1 << 3;
    const META_FINGERPRINT: u8 = 1 << 4;

    const DEF_TEST: u8 = 1 << 0;
    const DEF_SAME_FILE: u8 = 1 << 1;
    const DEF_END_DELTA: u8 = 1 << 2;
    const DEF_DECL_DELTA: u8 = 1 << 3;

    const EDGE_INFERRED: u8 = 1 << 3;
    const EDGE_SAME_FROM: u8 = 1 << 4;
    const EDGE_SAME_FILE: u8 = 1 << 5;
    const EDGE_DISTINCT_END: u8 = 1 << 6;

    const ENTRY_SYNTAX: u8 = 1 << 0;
    const ENTRY_ERROR: u8 = 1 << 1;
    const ENTRY_FOREIGN: u8 = 1 << 2;
    const ENTRY_STAMP: u8 = 1 << 3;
    const ENTRY_FINGERPRINT: u8 = 1 << 4;

    const SYM_TEST: u8 = 1 << 0;
    const SYM_END_DELTA: u8 = 1 << 3;
    const SYM_DECL_DELTA: u8 = 1 << 4;

    const REF_INFERRED: u8 = 1 << 3;
    const REF_SAME_FROM: u8 = 1 << 4;
    const REF_SAME_MODULE: u8 = 1 << 5;

    pub(super) struct Sections<'a> {
        pub(super) version: u32,
        pub(super) fast: &'a [u8],
        pub(super) raw: &'a [u8],
    }

    pub(super) struct FileMeta {
        pub(super) file: String,
        pub(super) digest: String,
        pub(super) structured: bool,
        pub(super) grammar: Option<Grammar>,
        pub(super) error: Option<String>,
        pub(super) fingerprint: Fingerprint,
        pub(super) stamp: Option<FileStamp>,
    }

    pub(super) fn encode(
        cache: &Cache,
        manifests_raw: &BTreeMap<String, String>,
        index: &Index,
    ) -> Vec<u8> {
        let fast = encode_fast_section(cache, index);
        let raw = encode_raw_section(cache, manifests_raw);

        let mut payload = Vec::with_capacity(16 + fast.len() + raw.len());
        payload.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        payload.extend_from_slice(&cache.version.to_le_bytes());
        payload.extend_from_slice(&(fast.len() as u64).to_le_bytes());
        payload.extend_from_slice(&fast);
        payload.extend_from_slice(&raw);

        let sum = checksum(&payload);
        let mut out = Vec::with_capacity(MAGIC.len() + 8 + payload.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&sum.to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    pub(super) fn split_sections(bytes: &[u8]) -> Result<Sections<'_>> {
        ensure!(bytes.len() >= 28, "index cache truncated");
        ensure!(&bytes[..4] == MAGIC, "invalid index cache magic");
        let expected_sum = u64::from_le_bytes(bytes[4..12].try_into().unwrap());
        let payload = &bytes[12..];
        ensure!(
            checksum(payload) == expected_sum,
            "index cache checksum mismatch"
        );

        let format_version = u32::from_le_bytes(payload[0..4].try_into().unwrap());
        ensure!(
            format_version == FORMAT_VERSION,
            "unsupported index cache format version {format_version}"
        );
        let version = u32::from_le_bytes(payload[4..8].try_into().unwrap());
        let fast_len = usize::try_from(u64::from_le_bytes(payload[8..16].try_into().unwrap()))?;
        let rest = &payload[16..];
        ensure!(fast_len <= rest.len(), "index cache section truncated");
        Ok(Sections {
            version,
            fast: &rest[..fast_len],
            raw: &rest[fast_len..],
        })
    }

    fn encode_fast_section(cache: &Cache, index: &Index) -> Vec<u8> {
        let mut out = Vec::with_capacity(cache.files.len() * 200 + index.edges.len() * 6);
        write_varint(&mut out, cache.files.len() as u64);
        for (file, parsed) in &cache.files {
            let has_fingerprint = parsed.fingerprint.0.iter().any(|&word| word != 0);
            let mut flags = 0u8;
            if is_rust(file) || parsed.structured() {
                flags |= META_STRUCTURED;
            }
            if parsed.foreign.is_some() {
                flags |= META_GRAMMAR;
            }
            if parsed.error.is_some() {
                flags |= META_ERROR;
            }
            if parsed.stamp.is_some() {
                flags |= META_STAMP;
            }
            if has_fingerprint {
                flags |= META_FINGERPRINT;
            }
            out.push(flags);
            write_raw_str(&mut out, file);
            write_raw_str(&mut out, &parsed.digest);
            if let Some(facts) = &parsed.foreign {
                out.push(encode_grammar(facts.grammar));
            }
            if let Some(error) = &parsed.error {
                write_raw_str(&mut out, error);
            }
            if has_fingerprint {
                for word in parsed.fingerprint.0 {
                    out.extend_from_slice(&word.to_le_bytes());
                }
            }
            if let Some(stamp) = parsed.stamp {
                write_stamp(&mut out, stamp);
            }
        }

        // Pre-assembled graph body with its own compact string table for paths & kinds.
        let mut interner = Interner::default();
        let mut graph = Vec::with_capacity(index.definitions.len() * 24 + index.edges.len() * 6);

        write_varint(&mut graph, index.definitions.len() as u64);
        let mut prev_qualified = "";
        let mut prev_file: Option<u32> = None;
        for def in &index.definitions {
            let file_id = interner.intern(&def.location.file);
            let kind_id = interner.intern(&def.kind);
            let same_file = prev_file == Some(file_id);
            let mut flags = 0u8;
            if def.test {
                flags |= DEF_TEST;
            }
            if same_file {
                flags |= DEF_SAME_FILE;
            }
            if def.location.end >= def.location.start {
                flags |= DEF_END_DELTA;
            }
            if def.declaration >= def.location.start {
                flags |= DEF_DECL_DELTA;
            }
            graph.push(flags);
            let prefix = common_prefix_len(prev_qualified, &def.qualified);
            write_varint(&mut graph, prefix as u64);
            write_raw_str(&mut graph, &def.qualified[prefix..]);
            prev_qualified = &def.qualified;
            write_varint(&mut graph, u64::from(kind_id));
            if !same_file {
                write_varint(&mut graph, u64::from(file_id));
                prev_file = Some(file_id);
            }
            write_varint(&mut graph, def.location.start as u64);
            let end_val = if def.location.end >= def.location.start {
                def.location.end - def.location.start
            } else {
                def.location.end
            };
            write_varint(&mut graph, end_val as u64);
            let decl_val = if def.declaration >= def.location.start {
                def.declaration - def.location.start
            } else {
                def.declaration
            };
            write_varint(&mut graph, decl_val as u64);
        }

        write_varint(&mut graph, index.edges.len() as u64);
        let mut prev_from: Option<usize> = None;
        let mut prev_edge_file: Option<u32> = None;
        let mut prev_line: u64 = 0;
        for edge in &index.edges {
            let file_id = interner.intern(&edge.location.file);
            let same_from = edge.from == prev_from;
            let same_file = prev_edge_file == Some(file_id);
            let distinct_end = edge.location.end != edge.location.start;
            let mut flags = encode_kind(edge.kind);
            if edge.inferred {
                flags |= EDGE_INFERRED;
            }
            if same_from {
                flags |= EDGE_SAME_FROM;
            }
            if same_file {
                flags |= EDGE_SAME_FILE;
            }
            if distinct_end {
                flags |= EDGE_DISTINCT_END;
            }
            graph.push(flags);
            write_varint(&mut graph, edge.to as u64);
            let line = edge.location.start as u64;
            write_zigzag(&mut graph, line.wrapping_sub(prev_line) as i64);
            prev_line = line;
            if !same_from {
                write_opt_usize(&mut graph, edge.from);
                prev_from = edge.from;
            }
            if !same_file {
                write_varint(&mut graph, u64::from(file_id));
                prev_edge_file = Some(file_id);
            }
            if distinct_end {
                write_varint(&mut graph, edge.location.end as u64);
            }
        }

        write_varint(&mut graph, index.by_name.len() as u64);
        for (name, ids) in &index.by_name {
            write_raw_str(&mut graph, name);
            write_varint(&mut graph, ids.len() as u64);
            let mut prev_id: u64 = 0;
            for &id in ids {
                let id_u64 = id as u64;
                write_zigzag(&mut graph, id_u64.wrapping_sub(prev_id) as i64);
                prev_id = id_u64;
            }
        }

        write_varint(&mut out, interner.strings.len() as u64);
        for value in &interner.strings {
            write_raw_str(&mut out, value);
        }
        out.extend_from_slice(&graph);
        out
    }

    pub(super) fn decode_file_metas(bytes: &[u8]) -> Result<(Vec<FileMeta>, &[u8])> {
        let mut reader = Reader::new(bytes);
        let count = reader.read_usize()?;
        let mut metas = Vec::with_capacity(count.min(reader.remaining()));
        for _ in 0..count {
            let flags = reader.read_u8()?;
            let file = reader.read_raw_string()?;
            let digest = reader.read_raw_string()?;
            let grammar = if flags & META_GRAMMAR != 0 {
                Some(decode_grammar(reader.read_u8()?)?)
            } else {
                None
            };
            let error = if flags & META_ERROR != 0 {
                Some(reader.read_raw_string()?)
            } else {
                None
            };
            let mut words = [0u64; 16];
            if flags & META_FINGERPRINT != 0 {
                for word in &mut words {
                    *word = reader.read_u64()?;
                }
            }
            let stamp = if flags & META_STAMP != 0 {
                Some(read_stamp(&mut reader)?)
            } else {
                None
            };
            metas.push(FileMeta {
                file,
                digest,
                structured: flags & META_STRUCTURED != 0,
                grammar,
                error,
                fingerprint: Fingerprint(words),
                stamp,
            });
        }
        Ok((metas, &bytes[reader.pos..]))
    }

    type DecodedGraph = (Vec<Definition>, Vec<Edge>, BTreeMap<String, Vec<usize>>);

    pub(super) fn decode_graph(bytes: &[u8]) -> Result<DecodedGraph> {
        let mut reader = Reader::new(bytes);
        let string_count = reader.read_usize()?;
        let mut strings = Vec::with_capacity(string_count.min(reader.remaining()));
        for _ in 0..string_count {
            strings.push(reader.read_raw_string()?);
        }

        let def_count = reader.read_usize()?;
        let mut definitions = Vec::with_capacity(def_count.min(reader.remaining()));
        let mut prev_qualified = String::new();
        let mut prev_file: Option<String> = None;
        for _ in 0..def_count {
            let flags = reader.read_u8()?;
            let prefix_len = reader.read_usize()?;
            ensure!(
                prefix_len <= prev_qualified.len() && prev_qualified.is_char_boundary(prefix_len),
                "invalid qualified prefix length"
            );
            let suffix_len = reader.read_usize()?;
            let suffix = std::str::from_utf8(reader.take(suffix_len)?)?;
            let mut qualified = String::with_capacity(prefix_len + suffix_len);
            qualified.push_str(&prev_qualified[..prefix_len]);
            qualified.push_str(suffix);
            prev_qualified = qualified.clone();

            let kind = reader.read_interned(&strings)?;
            let file = if flags & DEF_SAME_FILE != 0 {
                prev_file
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("missing previous definition file"))?
            } else {
                let value = reader.read_interned(&strings)?;
                prev_file = Some(value.clone());
                value
            };
            let start = reader.read_usize()?;
            let end_raw = reader.read_usize()?;
            let end = if flags & DEF_END_DELTA != 0 {
                start
                    .checked_add(end_raw)
                    .ok_or_else(|| anyhow::anyhow!("definition end overflow"))?
            } else {
                end_raw
            };
            let decl_raw = reader.read_usize()?;
            let declaration = if flags & DEF_DECL_DELTA != 0 {
                start
                    .checked_add(decl_raw)
                    .ok_or_else(|| anyhow::anyhow!("definition declaration overflow"))?
            } else {
                decl_raw
            };
            definitions.push(Definition {
                qualified,
                kind,
                location: Location { file, start, end },
                declaration,
                test: flags & DEF_TEST != 0,
            });
        }

        let edge_count = reader.read_usize()?;
        let mut edges = Vec::with_capacity(edge_count.min(reader.remaining()));
        let mut prev_from: Option<usize> = None;
        let mut prev_edge_file: Option<String> = None;
        let mut prev_line: u64 = 0;
        for _ in 0..edge_count {
            let flags = reader.read_u8()?;
            let kind = decode_kind(flags & 0x07)?;
            let inferred = flags & EDGE_INFERRED != 0;
            let to = reader.read_usize()?;
            let delta = reader.read_zigzag()?;
            let line_u64 = prev_line.wrapping_add(delta as u64);
            prev_line = line_u64;
            let start = usize::try_from(line_u64)?;
            let from = if flags & EDGE_SAME_FROM != 0 {
                prev_from
            } else {
                let value = reader.read_opt_usize()?;
                prev_from = value;
                value
            };
            let file = if flags & EDGE_SAME_FILE != 0 {
                prev_edge_file
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("missing previous edge file"))?
            } else {
                let value = reader.read_interned(&strings)?;
                prev_edge_file = Some(value.clone());
                value
            };
            let end = if flags & EDGE_DISTINCT_END != 0 {
                reader.read_usize()?
            } else {
                start
            };
            edges.push(Edge {
                from,
                to,
                kind,
                location: Location { file, start, end },
                inferred,
            });
        }

        let name_count = reader.read_usize()?;
        let mut by_name = BTreeMap::new();
        for _ in 0..name_count {
            let name = reader.read_raw_string()?;
            let id_count = reader.read_usize()?;
            let mut ids = Vec::with_capacity(id_count.min(reader.remaining()));
            let mut prev_id: u64 = 0;
            for _ in 0..id_count {
                let delta = reader.read_zigzag()?;
                let id_u64 = prev_id.wrapping_add(delta as u64);
                prev_id = id_u64;
                ids.push(usize::try_from(id_u64)?);
            }
            by_name.insert(name, ids);
        }

        ensure!(reader.remaining() == 0, "trailing bytes in index graph");
        Ok((definitions, edges, by_name))
    }

    pub(super) fn encode_raw_section(
        cache: &Cache,
        manifests_raw: &BTreeMap<String, String>,
    ) -> Vec<u8> {
        let mut interner = Interner::default();
        let mut body = Vec::with_capacity(cache.files.len() * 256);
        write_varint(&mut body, cache.files.len() as u64);
        for (file, parsed) in &cache.files {
            let file_id = interner.intern(file);
            write_varint(&mut body, u64::from(file_id));
            encode_parsed(&mut body, &mut interner, parsed);
        }
        write_varint(&mut body, manifests_raw.len() as u64);
        for (file, text) in manifests_raw {
            write_raw_str(&mut body, file);
            write_raw_str(&mut body, text);
        }

        let mut out = Vec::with_capacity(4 + interner.table_bytes + body.len());
        out.extend_from_slice(&cache.version.to_le_bytes());
        write_varint(&mut out, interner.strings.len() as u64);
        for value in &interner.strings {
            write_varint(&mut out, value.len() as u64);
            out.extend_from_slice(value.as_bytes());
        }
        out.extend_from_slice(&body);
        out
    }

    pub(super) fn decode_raw_cache(bytes: &[u8]) -> Result<Cache> {
        let (cache, _) = decode_raw_inner(bytes, false)?;
        Ok(cache)
    }

    pub(super) fn decode_raw_section(
        bytes: &[u8],
    ) -> Result<(Cache, BTreeMap<String, toml_edit::DocumentMut>)> {
        decode_raw_inner(bytes, true)
    }

    fn decode_raw_inner(
        bytes: &[u8],
        parse_manifests: bool,
    ) -> Result<(Cache, BTreeMap<String, toml_edit::DocumentMut>)> {
        let mut reader = Reader::new(bytes);
        let version = reader.read_u32()?;

        let string_count = reader.read_usize()?;
        let mut strings = Vec::with_capacity(string_count.min(reader.remaining()));
        for _ in 0..string_count {
            strings.push(reader.read_raw_string()?);
        }

        let file_count = reader.read_usize()?;
        let mut files = BTreeMap::new();
        for _ in 0..file_count {
            let file = reader.read_interned(&strings)?;
            let parsed = decode_parsed(&mut reader, &strings)?;
            files.insert(file, parsed);
        }

        let manifest_count = reader.read_usize()?;
        let mut manifests = BTreeMap::new();
        for _ in 0..manifest_count {
            let file = reader.read_raw_string()?;
            let text = reader.read_raw_string()?;
            if parse_manifests && let Ok(document) = text.parse::<toml_edit::DocumentMut>() {
                manifests.insert(file, document);
            }
        }
        ensure!(reader.remaining() == 0, "trailing bytes in index cache");
        Ok((Cache { version, files }, manifests))
    }

    fn common_prefix_len(a: &str, b: &str) -> usize {
        let mut len = a
            .as_bytes()
            .iter()
            .zip(b.as_bytes())
            .take_while(|(x, y)| x == y)
            .count();
        while len > 0 && !a.is_char_boundary(len) {
            len -= 1;
        }
        len
    }

    fn write_stamp(out: &mut Vec<u8>, stamp: FileStamp) {
        out.extend_from_slice(&stamp.device.to_le_bytes());
        out.extend_from_slice(&stamp.inode.to_le_bytes());
        out.extend_from_slice(&stamp.bytes.to_le_bytes());
        out.extend_from_slice(&stamp.modified.0.to_le_bytes());
        out.extend_from_slice(&stamp.modified.1.to_le_bytes());
        out.extend_from_slice(&stamp.changed.0.to_le_bytes());
        out.extend_from_slice(&stamp.changed.1.to_le_bytes());
    }

    fn read_stamp(reader: &mut Reader<'_>) -> Result<FileStamp> {
        Ok(FileStamp {
            device: reader.read_u64()?,
            inode: reader.read_u64()?,
            bytes: reader.read_u64()?,
            modified: (reader.read_i64()?, reader.read_i64()?),
            changed: (reader.read_i64()?, reader.read_i64()?),
        })
    }

    #[derive(Default)]
    struct Interner<'a> {
        index: HashMap<&'a str, u32>,
        strings: Vec<&'a str>,
        table_bytes: usize,
    }

    impl<'a> Interner<'a> {
        fn intern(&mut self, value: &'a str) -> u32 {
            if let Some(&id) = self.index.get(value) {
                return id;
            }
            let id = self.strings.len() as u32;
            self.index.insert(value, id);
            self.strings.push(value);
            self.table_bytes += value.len() + 2;
            id
        }
    }

    fn encode_parsed<'a>(out: &mut Vec<u8>, interner: &mut Interner<'a>, parsed: &'a Parsed) {
        let has_syntax = !parsed.syntax.symbols.is_empty()
            || !parsed.syntax.references.is_empty()
            || !parsed.syntax.imports.is_empty()
            || !parsed.syntax.modules.is_empty();
        let has_fingerprint = parsed.fingerprint.0.iter().any(|&word| word != 0);
        let mut flags = 0u8;
        if has_syntax {
            flags |= ENTRY_SYNTAX;
        }
        if parsed.error.is_some() {
            flags |= ENTRY_ERROR;
        }
        if parsed.foreign.is_some() {
            flags |= ENTRY_FOREIGN;
        }
        if parsed.stamp.is_some() {
            flags |= ENTRY_STAMP;
        }
        if has_fingerprint {
            flags |= ENTRY_FINGERPRINT;
        }
        out.push(flags);
        write_raw_str(out, &parsed.digest);
        if let Some(error) = &parsed.error {
            write_raw_str(out, error);
        }
        if has_fingerprint {
            for word in parsed.fingerprint.0 {
                out.extend_from_slice(&word.to_le_bytes());
            }
        }
        if let Some(stamp) = parsed.stamp {
            out.extend_from_slice(&stamp.device.to_le_bytes());
            out.extend_from_slice(&stamp.inode.to_le_bytes());
            out.extend_from_slice(&stamp.bytes.to_le_bytes());
            out.extend_from_slice(&stamp.modified.0.to_le_bytes());
            out.extend_from_slice(&stamp.modified.1.to_le_bytes());
            out.extend_from_slice(&stamp.changed.0.to_le_bytes());
            out.extend_from_slice(&stamp.changed.1.to_le_bytes());
        }
        if has_syntax {
            encode_syntax(out, interner, &parsed.syntax);
        }
        if let Some(facts) = &parsed.foreign {
            encode_foreign(out, interner, facts);
        }
    }

    fn decode_parsed(reader: &mut Reader<'_>, strings: &[String]) -> Result<Parsed> {
        let flags = reader.read_u8()?;
        let digest = reader.read_raw_string()?;
        let error = if flags & ENTRY_ERROR != 0 {
            Some(reader.read_raw_string()?)
        } else {
            None
        };
        let mut words = [0u64; 16];
        if flags & ENTRY_FINGERPRINT != 0 {
            for word in &mut words {
                *word = reader.read_u64()?;
            }
        }
        let stamp = if flags & ENTRY_STAMP != 0 {
            Some(FileStamp {
                device: reader.read_u64()?,
                inode: reader.read_u64()?,
                bytes: reader.read_u64()?,
                modified: (reader.read_i64()?, reader.read_i64()?),
                changed: (reader.read_i64()?, reader.read_i64()?),
            })
        } else {
            None
        };
        let syntax = if flags & ENTRY_SYNTAX != 0 {
            decode_syntax(reader, strings)?
        } else {
            Syntax::default()
        };
        let foreign = if flags & ENTRY_FOREIGN != 0 {
            Some(decode_foreign(reader, strings)?)
        } else {
            None
        };
        Ok(Parsed {
            digest,
            syntax,
            error,
            foreign,
            fingerprint: Fingerprint(words),
            stamp,
        })
    }

    fn encode_syntax<'a>(out: &mut Vec<u8>, interner: &mut Interner<'a>, syntax: &'a Syntax) {
        write_varint(out, syntax.symbols.len() as u64);
        for symbol in &syntax.symbols {
            let path_tag = symbol.path.len().min(3) as u8;
            let mut flags = path_tag << 1;
            if symbol.test {
                flags |= SYM_TEST;
            }
            if symbol.end >= symbol.start {
                flags |= SYM_END_DELTA;
            }
            if symbol.declaration >= symbol.start {
                flags |= SYM_DECL_DELTA;
            }
            out.push(flags);
            write_varint(out, u64::from(interner.intern(&symbol.name)));
            write_varint(out, u64::from(interner.intern(&symbol.kind)));
            if path_tag == 3 {
                write_varint(out, symbol.path.len() as u64);
            }
            for segment in &symbol.path {
                write_varint(out, u64::from(interner.intern(segment)));
            }
            write_varint(out, symbol.start as u64);
            let end_val = if symbol.end >= symbol.start {
                symbol.end - symbol.start
            } else {
                symbol.end
            };
            write_varint(out, end_val as u64);
            let decl_val = if symbol.declaration >= symbol.start {
                symbol.declaration - symbol.start
            } else {
                symbol.declaration
            };
            write_varint(out, decl_val as u64);
        }

        write_varint(out, syntax.references.len() as u64);
        let mut prev_from: Option<usize> = None;
        let mut prev_module: &[String] = &[];
        let mut prev_line: u64 = 0;
        for reference in &syntax.references {
            let path_tag = reference.path.len().min(3) as u8;
            let mut flags = encode_kind(reference.kind) | (path_tag << 6);
            if reference.inferred_receiver {
                flags |= REF_INFERRED;
            }
            let same_from = reference.from == prev_from;
            if same_from {
                flags |= REF_SAME_FROM;
            }
            let same_module = reference.module.as_slice() == prev_module;
            if same_module {
                flags |= REF_SAME_MODULE;
            }
            out.push(flags);
            let line = reference.line as u64;
            write_zigzag(out, line.wrapping_sub(prev_line) as i64);
            prev_line = line;
            if !same_from {
                write_opt_usize(out, reference.from);
                prev_from = reference.from;
            }
            if !same_module {
                write_varint(out, reference.module.len() as u64);
                for segment in &reference.module {
                    write_varint(out, u64::from(interner.intern(segment)));
                }
                prev_module = &reference.module;
            }
            if path_tag == 3 {
                write_varint(out, reference.path.len() as u64);
            }
            for segment in &reference.path {
                write_varint(out, u64::from(interner.intern(segment)));
            }
        }

        write_varint(out, syntax.imports.len() as u64);
        for import in &syntax.imports {
            write_opt_usize(out, import.from);
            write_opt_interned(out, interner, import.alias.as_deref());
            write_interned_slice(out, interner, &import.module);
            write_interned_slice(out, interner, &import.path);
        }

        write_varint(out, syntax.modules.len() as u64);
        for module in &syntax.modules {
            write_opt_interned(out, interner, module.override_path.as_deref());
            write_interned_slice(out, interner, &module.path);
        }
    }

    fn decode_syntax(reader: &mut Reader<'_>, strings: &[String]) -> Result<Syntax> {
        let sym_count = reader.read_usize()?;
        let mut symbols = Vec::with_capacity(sym_count.min(reader.remaining()));
        for _ in 0..sym_count {
            let flags = reader.read_u8()?;
            let name = reader.read_interned(strings)?;
            let kind = reader.read_interned(strings)?;
            let path_tag = (flags >> 1) & 0x03;
            let path_len = if path_tag == 3 {
                reader.read_usize()?
            } else {
                usize::from(path_tag)
            };
            let path = reader.read_interned_vec(strings, path_len)?;
            let start = reader.read_usize()?;
            let end_raw = reader.read_usize()?;
            let end = if flags & SYM_END_DELTA != 0 {
                start
                    .checked_add(end_raw)
                    .ok_or_else(|| anyhow::anyhow!("symbol end overflow"))?
            } else {
                end_raw
            };
            let decl_raw = reader.read_usize()?;
            let declaration = if flags & SYM_DECL_DELTA != 0 {
                start
                    .checked_add(decl_raw)
                    .ok_or_else(|| anyhow::anyhow!("symbol declaration overflow"))?
            } else {
                decl_raw
            };
            symbols.push(Symbol {
                bytes: 0..0,
                name,
                path,
                kind,
                start,
                end,
                declaration,
                test: flags & SYM_TEST != 0,
            });
        }

        let ref_count = reader.read_usize()?;
        let mut references = Vec::with_capacity(ref_count.min(reader.remaining()));
        let mut prev_from: Option<usize> = None;
        let mut prev_module: Vec<String> = Vec::new();
        let mut prev_line: u64 = 0;
        for _ in 0..ref_count {
            let flags = reader.read_u8()?;
            let kind = decode_kind(flags & 0x07)?;
            let inferred_receiver = flags & REF_INFERRED != 0;
            let delta = reader.read_zigzag()?;
            let line_u64 = prev_line.wrapping_add(delta as u64);
            prev_line = line_u64;
            let line = usize::try_from(line_u64)?;
            let from = if flags & REF_SAME_FROM != 0 {
                prev_from
            } else {
                let value = reader.read_opt_usize()?;
                prev_from = value;
                value
            };
            let module = if flags & REF_SAME_MODULE != 0 {
                prev_module.clone()
            } else {
                let len = reader.read_usize()?;
                let value = reader.read_interned_vec(strings, len)?;
                prev_module = value.clone();
                value
            };
            let path_tag = flags >> 6;
            let path_len = if path_tag == 3 {
                reader.read_usize()?
            } else {
                usize::from(path_tag)
            };
            let path = reader.read_interned_vec(strings, path_len)?;
            references.push(Reference {
                path,
                module,
                from,
                kind,
                line,
                inferred_receiver,
            });
        }

        let import_count = reader.read_usize()?;
        let mut imports = Vec::with_capacity(import_count.min(reader.remaining()));
        for _ in 0..import_count {
            let from = reader.read_opt_usize()?;
            let alias = reader.read_opt_interned(strings)?;
            let module = reader.read_interned_slice(strings)?;
            let path = reader.read_interned_slice(strings)?;
            imports.push(Import {
                module,
                from,
                alias,
                path,
            });
        }

        let mod_count = reader.read_usize()?;
        let mut modules = Vec::with_capacity(mod_count.min(reader.remaining()));
        for _ in 0..mod_count {
            let override_path = reader.read_opt_interned(strings)?;
            let path = reader.read_interned_slice(strings)?;
            modules.push(ModuleFile {
                path,
                override_path,
            });
        }

        Ok(Syntax {
            use_ranges: Vec::new(),
            symbols,
            references,
            imports,
            modules,
        })
    }

    fn encode_foreign<'a>(
        out: &mut Vec<u8>,
        interner: &mut Interner<'a>,
        facts: &'a polyglot::Facts,
    ) {
        out.push(encode_grammar(facts.grammar));
        write_varint(out, facts.lines as u64);

        write_varint(out, facts.definitions.len() as u64);
        for def in &facts.definitions {
            write_varint(out, u64::from(interner.intern(&def.name)));
            write_varint(out, u64::from(interner.intern(&def.kind)));
            write_varint(out, u64::from(interner.intern(&def.path)));
            write_varint(out, def.start as u64);
            write_varint(out, def.end as u64);
            write_varint(out, def.declaration as u64);
            out.push(u8::from(def.test));
        }

        write_varint(out, facts.references.len() as u64);
        let mut prev_line: u64 = 0;
        for reference in &facts.references {
            out.push(encode_kind(reference.kind));
            write_varint(out, u64::from(interner.intern(&reference.name)));
            write_opt_usize(out, reference.from);
            let line = reference.line as u64;
            write_zigzag(out, line.wrapping_sub(prev_line) as i64);
            prev_line = line;
        }

        write_varint(out, facts.imports.len() as u64);
        for import in &facts.imports {
            write_varint(out, u64::from(interner.intern(&import.path)));
            write_varint(out, import.line as u64);
            write_varint(out, import.bindings.len() as u64);
            for (alias, name) in &import.bindings {
                write_varint(out, u64::from(interner.intern(alias)));
                write_varint(out, u64::from(interner.intern(name)));
            }
        }

        write_varint(out, facts.routes.len() as u64);
        for (route, handler) in &facts.routes {
            write_varint(out, u64::from(interner.intern(route)));
            write_varint(out, *handler as u64);
        }
    }

    fn decode_foreign(reader: &mut Reader<'_>, strings: &[String]) -> Result<polyglot::Facts> {
        let grammar = decode_grammar(reader.read_u8()?)?;
        let lines = reader.read_usize()?;

        let def_count = reader.read_usize()?;
        let mut definitions = Vec::with_capacity(def_count.min(reader.remaining()));
        for _ in 0..def_count {
            let name = reader.read_interned(strings)?;
            let kind = reader.read_interned(strings)?;
            let path = reader.read_interned(strings)?;
            let start = reader.read_usize()?;
            let end = reader.read_usize()?;
            let declaration = reader.read_usize()?;
            let test = match reader.read_u8()? {
                0 => false,
                1 => true,
                other => bail!("invalid bool byte {other}"),
            };
            definitions.push(polyglot::Definition {
                bytes: 0..0,
                signature_start: 0,
                header_end: 0,
                name,
                kind,
                path,
                start,
                end,
                declaration,
                test,
            });
        }

        let ref_count = reader.read_usize()?;
        let mut references = Vec::with_capacity(ref_count.min(reader.remaining()));
        let mut prev_line: u64 = 0;
        for _ in 0..ref_count {
            let kind = decode_kind(reader.read_u8()?)?;
            let name = reader.read_interned(strings)?;
            let from = reader.read_opt_usize()?;
            let delta = reader.read_zigzag()?;
            let line_u64 = prev_line.wrapping_add(delta as u64);
            prev_line = line_u64;
            let line = usize::try_from(line_u64)?;
            references.push(polyglot::Reference {
                name,
                from,
                line,
                kind,
            });
        }

        let import_count = reader.read_usize()?;
        let mut imports = Vec::with_capacity(import_count.min(reader.remaining()));
        for _ in 0..import_count {
            let path = reader.read_interned(strings)?;
            let line = reader.read_usize()?;
            let binding_count = reader.read_usize()?;
            let mut bindings = BTreeMap::new();
            for _ in 0..binding_count {
                let alias = reader.read_interned(strings)?;
                let name = reader.read_interned(strings)?;
                bindings.insert(alias, name);
            }
            imports.push(polyglot::Import {
                path,
                line,
                bindings,
            });
        }

        let route_count = reader.read_usize()?;
        let mut routes = Vec::with_capacity(route_count.min(reader.remaining()));
        for _ in 0..route_count {
            let route = reader.read_interned(strings)?;
            let handler = reader.read_usize()?;
            routes.push((route, handler));
        }

        Ok(polyglot::Facts {
            import_ranges: Vec::new(),
            grammar,
            lines,
            definitions,
            references,
            imports,
            routes,
        })
    }

    fn encode_kind(kind: Kind) -> u8 {
        match kind {
            Kind::Call => 0,
            Kind::Method => 1,
            Kind::Reference => 2,
            Kind::Import => 3,
            Kind::Route => 4,
            Kind::Data => 5,
        }
    }

    fn decode_kind(tag: u8) -> Result<Kind> {
        Ok(match tag {
            0 => Kind::Call,
            1 => Kind::Method,
            2 => Kind::Reference,
            3 => Kind::Import,
            4 => Kind::Route,
            5 => Kind::Data,
            other => bail!("invalid kind tag {other}"),
        })
    }

    fn encode_grammar(grammar: Grammar) -> u8 {
        match grammar {
            Grammar::JavaScript => 0,
            Grammar::TypeScript => 1,
            Grammar::Tsx => 2,
            Grammar::Python => 3,
            Grammar::Go => 4,
            Grammar::Java => 5,
            Grammar::C => 6,
            Grammar::Cpp => 7,
            Grammar::CSharp => 8,
            Grammar::Ruby => 9,
            Grammar::Php => 10,
            Grammar::Bash => 11,
            Grammar::Sql => 12,
            Grammar::Kotlin => 13,
            Grammar::Swift => 14,
            Grammar::Scala => 15,
            Grammar::Dart => 16,
            Grammar::Hcl => 17,
            Grammar::Lua => 18,
            Grammar::Zig => 19,
            Grammar::Elixir => 20,
            Grammar::ObjectiveC => 21,
            Grammar::PowerShell => 22,
            Grammar::Julia => 23,
            Grammar::Groovy => 24,
            Grammar::Vue => 25,
            Grammar::Svelte => 26,
            Grammar::Astro => 27,
        }
    }

    fn decode_grammar(tag: u8) -> Result<Grammar> {
        Ok(match tag {
            0 => Grammar::JavaScript,
            1 => Grammar::TypeScript,
            2 => Grammar::Tsx,
            3 => Grammar::Python,
            4 => Grammar::Go,
            5 => Grammar::Java,
            6 => Grammar::C,
            7 => Grammar::Cpp,
            8 => Grammar::CSharp,
            9 => Grammar::Ruby,
            10 => Grammar::Php,
            11 => Grammar::Bash,
            12 => Grammar::Sql,
            13 => Grammar::Kotlin,
            14 => Grammar::Swift,
            15 => Grammar::Scala,
            16 => Grammar::Dart,
            17 => Grammar::Hcl,
            18 => Grammar::Lua,
            19 => Grammar::Zig,
            20 => Grammar::Elixir,
            21 => Grammar::ObjectiveC,
            22 => Grammar::PowerShell,
            23 => Grammar::Julia,
            24 => Grammar::Groovy,
            25 => Grammar::Vue,
            26 => Grammar::Svelte,
            27 => Grammar::Astro,
            other => bail!("invalid grammar tag {other}"),
        })
    }

    fn write_varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn write_zigzag(out: &mut Vec<u8>, value: i64) {
        let encoded = ((value << 1) ^ (value >> 63)) as u64;
        write_varint(out, encoded);
    }

    fn write_opt_usize(out: &mut Vec<u8>, value: Option<usize>) {
        match value {
            None => write_varint(out, 0),
            Some(index) => write_varint(out, (index as u64).saturating_add(1)),
        }
    }

    fn write_opt_interned<'a>(
        out: &mut Vec<u8>,
        interner: &mut Interner<'a>,
        value: Option<&'a str>,
    ) {
        match value {
            None => write_varint(out, 0),
            Some(text) => write_varint(out, u64::from(interner.intern(text)) + 1),
        }
    }

    fn write_interned_slice<'a>(
        out: &mut Vec<u8>,
        interner: &mut Interner<'a>,
        values: &'a [String],
    ) {
        write_varint(out, values.len() as u64);
        for value in values {
            write_varint(out, u64::from(interner.intern(value)));
        }
    }

    fn write_raw_str(out: &mut Vec<u8>, value: &str) {
        write_varint(out, value.len() as u64);
        out.extend_from_slice(value.as_bytes());
    }

    fn checksum(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let (chunks, remainder) = bytes.as_chunks::<8>();
        for &chunk in chunks {
            hash ^= u64::from_le_bytes(chunk);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        for &byte in remainder {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    struct Reader<'a> {
        bytes: &'a [u8],
        pos: usize,
    }

    impl<'a> Reader<'a> {
        fn new(bytes: &'a [u8]) -> Self {
            Self { bytes, pos: 0 }
        }

        fn remaining(&self) -> usize {
            self.bytes.len().saturating_sub(self.pos)
        }

        fn take(&mut self, len: usize) -> Result<&'a [u8]> {
            let end = self
                .pos
                .checked_add(len)
                .ok_or_else(|| anyhow::anyhow!("index cache length overflow"))?;
            let slice = self
                .bytes
                .get(self.pos..end)
                .ok_or_else(|| anyhow::anyhow!("unexpected EOF in index cache"))?;
            self.pos = end;
            Ok(slice)
        }

        fn read_u8(&mut self) -> Result<u8> {
            Ok(self.take(1)?[0])
        }

        fn read_u32(&mut self) -> Result<u32> {
            Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
        }

        fn read_u64(&mut self) -> Result<u64> {
            Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
        }

        fn read_i64(&mut self) -> Result<i64> {
            Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
        }

        fn read_varint(&mut self) -> Result<u64> {
            let mut value = 0u64;
            let mut shift = 0u32;
            loop {
                let byte = self.read_u8()?;
                if shift == 63 && byte > 1 {
                    bail!("varint overflow in index cache");
                }
                value |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return Ok(value);
                }
                shift += 7;
                if shift >= 64 {
                    bail!("varint overflow in index cache");
                }
            }
        }

        fn read_zigzag(&mut self) -> Result<i64> {
            let encoded = self.read_varint()?;
            Ok(((encoded >> 1) as i64) ^ -((encoded & 1) as i64))
        }

        fn read_usize(&mut self) -> Result<usize> {
            Ok(usize::try_from(self.read_varint()?)?)
        }

        fn read_opt_usize(&mut self) -> Result<Option<usize>> {
            match self.read_varint()? {
                0 => Ok(None),
                nonzero => Ok(Some(usize::try_from(nonzero - 1)?)),
            }
        }

        fn read_raw_string(&mut self) -> Result<String> {
            let len = self.read_usize()?;
            let slice = self.take(len)?;
            Ok(std::str::from_utf8(slice)?.to_owned())
        }

        fn read_interned(&mut self, strings: &[String]) -> Result<String> {
            let id = self.read_usize()?;
            strings
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("invalid string id {id} in index cache"))
        }

        fn read_opt_interned(&mut self, strings: &[String]) -> Result<Option<String>> {
            match self.read_varint()? {
                0 => Ok(None),
                nonzero => {
                    let id = usize::try_from(nonzero - 1)?;
                    strings
                        .get(id)
                        .cloned()
                        .map(Some)
                        .ok_or_else(|| anyhow::anyhow!("invalid string id {id} in index cache"))
                }
            }
        }

        fn read_interned_vec(&mut self, strings: &[String], len: usize) -> Result<Vec<String>> {
            let mut out = Vec::with_capacity(len.min(self.remaining()));
            for _ in 0..len {
                out.push(self.read_interned(strings)?);
            }
            Ok(out)
        }

        fn read_interned_slice(&mut self, strings: &[String]) -> Result<Vec<String>> {
            let len = self.read_usize()?;
            self.read_interned_vec(strings, len)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_workspace(root: &Path) -> Workspace {
        Workspace {
            root: root.to_path_buf(),
            cwd: root.to_path_buf(),
            state: root.join(".justless"),
            origin: root.to_path_buf(),
        }
    }

    fn sample_cache() -> Cache {
        let mut files = BTreeMap::new();
        files.insert(
            "src/lib.rs".into(),
            Parsed {
                digest: "a".repeat(64),
                syntax: Syntax {
                    use_ranges: Vec::new(),
                    symbols: vec![
                        rust_syntax::Symbol {
                            bytes: 0..0,
                            name: "alpha".into(),
                            path: vec!["alpha".into()],
                            kind: "function".into(),
                            start: 1,
                            end: 10,
                            declaration: 1,
                            test: false,
                        },
                        rust_syntax::Symbol {
                            bytes: 0..0,
                            name: "test_alpha".into(),
                            path: vec!["tests".into(), "test_alpha".into()],
                            kind: "function".into(),
                            start: 12,
                            end: 18,
                            declaration: 13,
                            test: true,
                        },
                    ],
                    references: vec![
                        Reference {
                            path: vec!["alpha".into()],
                            module: vec!["tests".into()],
                            from: Some(1),
                            kind: Kind::Call,
                            line: 15,
                            inferred_receiver: false,
                        },
                        Reference {
                            path: vec![
                                "crate".into(),
                                "alpha".into(),
                                "extra".into(),
                                "deep".into(),
                            ],
                            module: vec!["tests".into()],
                            from: Some(1),
                            kind: Kind::Method,
                            line: 14,
                            inferred_receiver: true,
                        },
                        Reference {
                            path: vec!["Vec".into(), "new".into()],
                            module: Vec::new(),
                            from: None,
                            kind: Kind::Reference,
                            line: 5,
                            inferred_receiver: false,
                        },
                    ],
                    imports: vec![Import {
                        module: vec!["tests".into()],
                        from: Some(1),
                        alias: Some("a".into()),
                        path: vec!["super".into(), "alpha".into()],
                    }],
                    modules: vec![rust_syntax::ModuleFile {
                        path: vec!["sub".into()],
                        override_path: Some("alt/sub.rs".into()),
                    }],
                },
                error: None,
                foreign: None,
                fingerprint: Fingerprint::new("pub fn alpha() { test_alpha(); }"),
                stamp: Some(FileStamp {
                    device: 42,
                    inode: 99,
                    bytes: 128,
                    modified: (1_700_000_000, 123_456),
                    changed: (1_700_000_001, 654_321),
                }),
            },
        );
        files.insert(
            "web/app.ts".into(),
            Parsed {
                digest: "b".repeat(64),
                syntax: Syntax::default(),
                error: Some("syntax note".into()),
                foreign: Some(polyglot::Facts {
                    import_ranges: Vec::new(),
                    grammar: Grammar::TypeScript,
                    lines: 42,
                    definitions: vec![polyglot::Definition {
                        bytes: 0..0,
                        signature_start: 0,
                        header_end: 0,
                        name: "handleOrder".into(),
                        kind: "function".into(),
                        path: "handleOrder".into(),
                        start: 3,
                        end: 20,
                        declaration: 3,
                        test: false,
                    }],
                    references: vec![
                        polyglot::Reference {
                            name: "/orders".into(),
                            from: Some(0),
                            line: 10,
                            kind: Kind::Route,
                        },
                        polyglot::Reference {
                            name: "orders".into(),
                            from: None,
                            line: 8,
                            kind: Kind::Data,
                        },
                    ],
                    imports: vec![polyglot::Import {
                        path: "./db".into(),
                        line: 1,
                        bindings: BTreeMap::from([("db".into(), "connect".into())]),
                    }],
                    routes: vec![("/orders".into(), 0)],
                }),
                fingerprint: Fingerprint::default(),
                stamp: None,
            },
        );
        Cache {
            version: VERSION,
            files,
        }
    }

    #[test]
    fn binary_cache_round_trips_losslessly() {
        let original = sample_cache();
        let manifests_raw = BTreeMap::new();
        let (assembled, _) = Index::assemble(
            &original,
            &BTreeMap::new(),
            Refresh::default(),
            &Overlay::default(),
        );
        let bytes = binary::encode(&original, &manifests_raw, &assembled);
        let sections = binary::split_sections(&bytes).unwrap();
        assert_eq!(sections.version, VERSION);
        let decoded = binary::decode_raw_cache(sections.raw).unwrap();
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
        let (metas, graph_bytes) = binary::decode_file_metas(sections.fast).unwrap();
        assert_eq!(metas.len(), original.files.len());
        let (defs, edges, by_name) = binary::decode_graph(graph_bytes).unwrap();
        assert_eq!(defs.len(), assembled.definitions.len());
        assert_eq!(edges.len(), assembled.edges.len());
        assert_eq!(by_name, assembled.by_name);
    }

    #[test]
    fn corrupt_or_wrong_version_binary_cache_falls_back_to_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn helper() -> u32 { 1 }\npub fn entry() -> u32 { helper() }\n",
        )
        .unwrap();
        let ws = test_workspace(root);
        let first = Index::open(&ws).unwrap();
        assert_eq!(first.refresh.parsed, 1);

        let bin = ws.state.join("index.bin");
        let valid = fs::read(&bin).unwrap();
        let mut flipped = valid.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0xff;
        let mut wrong_ver = sample_cache();
        wrong_ver.version = VERSION + 1;
        let (wrong_idx, _) = Index::assemble(
            &wrong_ver,
            &BTreeMap::new(),
            Refresh::default(),
            &Overlay::default(),
        );
        let wrong_ver_bytes = binary::encode(&wrong_ver, &BTreeMap::new(), &wrong_idx);
        let mut trailing = valid.clone();
        trailing.push(0);

        for bad in [
            b"NOPE".to_vec(),
            valid[..valid.len() / 2].to_vec(),
            flipped,
            trailing,
            wrong_ver_bytes,
        ] {
            assert!(
                !binary::split_sections(&bad).is_ok_and(|sections| sections.version == VERSION)
            );
            fs::write(&bin, &bad).unwrap();
            let rebuilt = Index::open(&ws).unwrap();
            assert_eq!(rebuilt.refresh.parsed, 1);
            assert_eq!(rebuilt.refresh.reused, 0);
            assert_eq!(rebuilt.definitions.len(), first.definitions.len());
            assert_eq!(rebuilt.edges.len(), first.edges.len());
            // Rebuilt cache is valid again for subsequent warm opens.
            let warm = Index::open(&ws).unwrap();
            assert_eq!(warm.refresh.parsed, 0);
            assert_eq!(warm.refresh.reused, 1);
        }
    }

    #[test]
    fn warm_open_matches_fresh_build_and_skips_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "mod inner;\npub fn run() -> u32 { inner::work() }\n",
        )
        .unwrap();
        fs::write(
            root.join("src/inner.rs"),
            "pub fn work() -> u32 { 7 }\n#[test]\nfn checks_work() { assert_eq!(work(), 7); }\n",
        )
        .unwrap();
        fs::write(
            root.join("src/app.py"),
            "def greet():\n    return 1\ndef main():\n    return greet()\n",
        )
        .unwrap();

        let ws = test_workspace(root);
        let fresh = Index::open(&ws).unwrap();
        assert_eq!(fresh.refresh.parsed, 3);
        let bin_path = ws.state.join("index.bin");
        let json_path = ws.state.join("index.json");
        assert!(bin_path.is_file());
        assert!(json_path.is_file());
        let saved_bytes = fs::read(&bin_path).unwrap();

        let warm = Index::open(&ws).unwrap();
        assert_eq!(warm.refresh.parsed, 0);
        assert_eq!(warm.refresh.reused, 3);
        assert_eq!(fs::read(&bin_path).unwrap(), saved_bytes);

        fn defs(idx: &Index) -> Vec<(&str, &str, &Location, usize, bool)> {
            idx.definitions
                .iter()
                .map(|d| {
                    (
                        d.qualified.as_str(),
                        d.kind.as_str(),
                        &d.location,
                        d.declaration,
                        d.test,
                    )
                })
                .collect()
        }
        fn edges(idx: &Index) -> Vec<(Option<usize>, usize, Kind, &Location, bool)> {
            idx.edges
                .iter()
                .map(|e| (e.from, e.to, e.kind, &e.location, e.inferred))
                .collect()
        }
        assert_eq!(defs(&warm), defs(&fresh));
        assert_eq!(edges(&warm), edges(&fresh));
        assert_eq!(warm.by_file, fresh.by_file);
        assert_eq!(warm.by_name, fresh.by_name);
        assert_eq!(warm.incoming, fresh.incoming);
        assert_eq!(warm.outgoing, fresh.outgoing);
        assert_eq!(warm.errors, fresh.errors);
        assert_eq!(warm.files, fresh.files);
        // Lazy source preserves grammar, rust_paths_at, and with_removed.
        assert_eq!(warm.grammar("src/app.py"), Some(Grammar::Python));
        assert_eq!(
            warm.rust_paths_at("src/lib.rs", 2),
            fresh.rust_paths_at("src/lib.rs", 2)
        );
        let (rebuilt_warm, _) = warm.with_removed(&[], &[]).unwrap();
        assert_eq!(defs(&rebuilt_warm), defs(&fresh));
        assert_eq!(edges(&rebuilt_warm), edges(&fresh));
    }

    #[test]
    fn migrates_legacy_json_cache_without_reparsing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn ping() -> u32 { 1 }\n").unwrap();

        let ws = test_workspace(root);
        let first = Index::open(&ws).unwrap();
        assert_eq!(first.refresh.parsed, 1);

        // Simulate a legacy repo that only has full JSON in `.justless/index.json`.
        let legacy_json =
            serde_json::to_vec(first.source.as_ref().unwrap().load().unwrap().0).unwrap();
        fs::remove_file(ws.state.join("index.bin")).unwrap();
        fs::write(ws.state.join("index.json"), &legacy_json).unwrap();

        let migrated = Index::open(&ws).unwrap();
        assert_eq!(migrated.refresh.parsed, 0);
        assert_eq!(migrated.refresh.reused, 1);
        assert!(ws.state.join("index.bin").is_file());
        assert!(
            fs::metadata(ws.state.join("index.json")).unwrap().len() < legacy_json.len() as u64
        );
    }
}
