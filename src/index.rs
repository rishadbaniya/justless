use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

use crate::capture::source_digest;
use crate::polyglot::{self, Grammar, Parsers};
use crate::rust_syntax::{self, Import, Kind, Reference, Syntax};
use crate::workspace::{Location, Workspace, display_path, lock_state, write_json};

pub(crate) const VERSION: u32 = 8;

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

struct Source {
    cache: Cache,
    manifests: BTreeMap<String, toml_edit::DocumentMut>,
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
        let path = workspace.state.join("index.json");
        // Writers replace the cache with an atomic rename, and every entry is
        // checked against its file before reuse, so reading needs no lock.
        // Read the whole cache at once: serde_json pulls an unbuffered reader one
        // byte per read(2), which made a 20 MB cache cost seconds of system time.
        let mut cache: Cache = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
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
        let mut refresh = Refresh::default();
        let candidates = inputs(workspace, &mut refresh.skipped)?;
        let mut manifests = BTreeMap::new();
        let mut found = BTreeSet::new();
        let mut changed = rebuild;
        let mut parsers = Parsers::default();
        let max_bytes = workspace.settings()?.max_file_bytes;
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
            // A writer that cannot get the lock still answers from what it parsed.
            match lock_state(&workspace.state.join("index.lock")) {
                Ok(_guard) => write_json(&path, &cache)?,
                Err(error) => eprintln!("justless: index cache not saved: {error:#}"),
            }
        }
        let (mut index, _) = Self::assemble(&cache, &manifests, refresh, &Overlay::default());
        index.source = Some(Box::new(Source { cache, manifests }));
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
        let source = self.source.as_ref()?;
        let mut overlay = Overlay::default();
        for &(file, text) in wider {
            if let Some(cached) = source.cache.files.get(file)
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
            let Some(cached) = source.cache.files.get(file) else {
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
        let (mut index, ids) = Self::assemble(
            &source.cache,
            &source.manifests,
            Refresh::default(),
            &overlay,
        );
        let mut restored = vec![Vec::new(); removed.len()];
        for (entry, id) in ids {
            restored[entry].push(id);
        }
        // Keep the files parsed for this index, for `rust_paths_at`.
        index.source = Some(Box::new(Source {
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
        let parsed = self.source.as_ref()?.cache.files.get(file)?;
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
        let mut precise = HashMap::<(usize, Vec<String>), Vec<usize>>::new();
        let mut origins = Vec::new();
        let mut owners = Vec::new();
        for unit in &units {
            origins.push(index.definitions.len());
            for (position, symbol) in parsed(&unit.file).syntax.symbols.iter().enumerate() {
                let id = index.definitions.len();
                if let Some(entry) = overlay.restores(&unit.file, position) {
                    restored.push((entry, id));
                }
                owners.push(unit.target);
                let mut path = unit.module.clone();
                path.extend_from_slice(&symbol.path);
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
                    .entry((unit.namespace(), path))
                    .or_default()
                    .push(id);
            }
        }

        let mut imports = HashMap::<(usize, Vec<String>), Vec<Binding>>::new();
        for (unit, origin) in units.iter().zip(&origins) {
            for import in &parsed(&unit.file).syntax.imports {
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
            aliases: imports
                .values()
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
            by_name: &index.by_name,
        };
        let mut seen = BTreeSet::new();
        let mut resolved = Vec::new();
        for (unit, origin) in units.iter().zip(&origins) {
            for reference in &parsed(&unit.file).syntax.references {
                let from = reference.from.map(|id| origin + id);
                resolver.resolve(unit, from, reference, &mut resolved);
                for &(to, kind, inferred) in &resolved {
                    if seen.insert((from, to, unit.file.as_str(), reference.line, kind)) {
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

    /// Names files that were left out of the index, for a search that found
    /// nothing. A symbol search only mentions files that could define symbols.
    pub fn skipped_note(&self, symbols: bool) -> Option<String> {
        let skipped: Vec<_> = self
            .refresh
            .skipped
            .iter()
            .filter(|(file, reason)| {
                !symbols
                    || reason.starts_with("unreadable")
                    || is_rust(file)
                    || Grammar::for_file(file).is_some()
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

struct Binding<'a> {
    from: Option<usize>,
    import: &'a Import,
}

struct Resolver<'a> {
    aliases: BTreeSet<&'a str>,
    precise: HashMap<(usize, Vec<String>), Vec<usize>>,
    imports: HashMap<(usize, Vec<String>), Vec<Binding<'a>>>,
    libraries: BTreeMap<&'a str, usize>,
    /// The package directories each target can reach, by target.
    reach: Vec<BTreeSet<&'a str>>,
    /// The target that defines each Rust definition, by definition.
    owners: Vec<Option<usize>>,
    targets: &'a [Target],
    definitions: &'a [Definition],
    by_name: &'a BTreeMap<String, Vec<usize>>,
}

/// Import hops one reference may follow. A glob such as `use super::*` spends a
/// hop and can reach the imports of the module it names. A named import ends the chain.
const IMPORT_HOPS: u8 = 3;

impl Resolver<'_> {
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
    fn resolve(
        &self,
        unit: &Unit,
        from: Option<usize>,
        reference: &Reference,
        resolved: &mut Vec<(usize, Kind, bool)>,
    ) {
        resolved.clear();
        let path = &reference.path;
        if reference.inferred_receiver {
            let Some(name) = path.first() else { return };
            let candidates = self
                .by_name
                .get(name)
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
        if let Some(ids) = self.lookup(unit, from, reference, path) {
            let inferred = ids.len() > 1;
            resolved.extend(ids.iter().map(|&id| (id, reference.kind, inferred)));
        }
        if path.len() > 1
            && let Some(ids) = self.lookup(unit, from, reference, &path[..path.len() - 1])
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
                            "struct" | "enum" | "trait" | "type"
                        )
                    })
                    .map(|&id| (id, kind, inferred)),
            );
        }
    }

    fn lookup(
        &self,
        unit: &Unit,
        from: Option<usize>,
        reference: &Reference,
        path: &[String],
    ) -> Option<&[usize]> {
        // Every resolution keeps the last segment, unless a single-segment path
        // is an import alias. Most unresolvable names (std items, locals) stop here.
        let last = path.last()?;
        if !self.by_name.contains_key(last)
            && (path.len() > 1 || !self.aliases.contains(last.as_str()))
        {
            return None;
        }
        let namespace = unit.namespace();
        let module = joined(&unit.module, &reference.module);
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
                && let Some(ids) = self.exact(namespace, joined(&scope, path))
            {
                return Some(ids);
            }
            if scope.pop().is_some()
                && scope.len() > module.len()
                && let Some(ids) = self.exact(namespace, joined(&scope, path))
            {
                return Some(ids);
            }
        }
        let hops = if reference.kind == Kind::Import {
            0
        } else {
            IMPORT_HOPS
        };
        self.resolve_path(namespace, &module, from, path, hops, 0)
    }

    fn exact(&self, namespace: usize, path: Vec<String>) -> Option<&[usize]> {
        self.precise.get(&(namespace, path)).map(Vec::as_slice)
    }

    fn resolve_path(
        &self,
        namespace: usize,
        module: &[String],
        from: Option<usize>,
        path: &[String],
        hops: u8,
        depth: usize,
    ) -> Option<&[usize]> {
        let (first, rest) = path.split_first()?;
        if depth >= 12 {
            return None;
        }
        match first.as_str() {
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
        if let Some(ids) = self.exact(namespace, joined(module, path)) {
            return Some(ids);
        }
        if hops > 0
            && let Some(bindings) = self.imports.get(&(namespace, module.to_vec()))
        {
            let mut found: Option<&[usize]> = None;
            let mut ambiguous = false;
            for binding in bindings
                .iter()
                .filter(|binding| binding.from.is_none() || binding.from == from)
            {
                let (expanded, hops) = match &binding.import.alias {
                    Some(alias) if alias == first => (joined(&binding.import.path, rest), 0),
                    None => (joined(&binding.import.path, path), hops - 1),
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
                    library.name == *first || library.package == *first
                }))?
            }
            // A registry or git dependency is external, whatever its package name.
            Some(Dependency::External) => return None,
            // Binaries, tests, examples and benches reach their own package's library.
            None => {
                let id = *self.libraries.get(target.directory.as_str())?;
                let library = &self.targets[id];
                (library.name == *first || library.package == *first).then_some(id)?
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
        module: &[String],
        from: Option<usize>,
        name: &str,
        hops: u8,
    ) -> bool {
        let Some(bindings) = self.imports.get(&(namespace, module.to_vec())) else {
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
                            source != module && self.binds(namespace, &source, None, name, hops - 1)
                        })
                }
            })
    }
}

/// The module of the same crate that an import path starting with `crate`,
/// `self` or `super` names.
fn local_module(module: &[String], path: &[String]) -> Option<Vec<String>> {
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
    base.extend_from_slice(rest);
    Some(base)
}

fn unique(mut items: impl Iterator<Item = usize>) -> Option<usize> {
    let first = items.next()?;
    items.all(|item| item == first).then_some(first)
}
