//! `justless affected`: a reverse walk from a symbol, a file or the current Git
//! changes to the code that depends on it, followed by the tests that reach it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, bail, ensure};
use clap::{Arg, ArgGroup, ArgMatches, Command};
use serde_json::{Map, json};

use crate::context::{Change, changes, relationship};
use crate::index::{Edge, Index};
use crate::map::{Fill, Section, emit, is_test_file, label, name, node, plural};
use crate::workspace::{Location, Workspace};

/// New dependents followed from any one symbol. Certain links sort first.
const FAN_OUT: usize = 256;
/// Dependents gathered across the whole walk.
const MAX_REACHED: usize = 5000;
/// Tests named in one suggested command.
const FILTERS: usize = 8;

pub fn command() -> Command<'static> {
    Command::new("affected")
        .about("Show what depends on a symbol, file or the current Git changes, and the tests that reach it")
        .arg(
            Arg::new("target")
                .value_name("TARGET")
                .help("Symbol name or workspace-relative file"),
        )
        .arg(
            Arg::new("changes")
                .long("changes")
                .help("Start from the symbols in current Git changes"),
        )
        .arg(
            Arg::new("staged")
                .long("staged")
                .requires("changes")
                .help("Use staged changes"),
        )
        .group(
            ArgGroup::new("input")
                .required(true)
                .args(&["target", "changes"]),
        )
        .arg(crate::number("depth", "3", "Dependency hops to follow"))
        .arg(crate::budget())
        .arg(
            Arg::new("json")
                .long("json")
                .help("Print the selected items as JSON"),
        )
}

pub fn run(workspace: &Workspace, matches: &ArgMatches) -> Result<()> {
    let budget = crate::context_budget(workspace, matches)?;
    let depth = crate::read_num(matches, "depth", 1)?;
    let index = Index::open(workspace)?;
    let definitions = &index.definitions;
    let mut summary = Map::new();
    for key in ["dependents", "tests", "commands"] {
        summary.insert(key.into(), json!([]));
    }
    let mut sections = Vec::new();
    let (subject, seeds) = match matches.value_of("target") {
        Some(target) => {
            let (subject, seeds) = resolve(&index, target)?;
            summary.insert(
                "target".into(),
                match seeds.as_slice() {
                    [id] if subject.is_none() => node(&definitions[*id]),
                    _ => json!({"file": target.trim_start_matches("./")}),
                },
            );
            (
                subject.unwrap_or_else(|| label(&definitions[seeds[0]])),
                seeds,
            )
        }
        None => {
            let (seeds, section) = changed(workspace, &index, matches.is_present("staged"))?;
            if section.lines.is_empty() {
                if matches.is_present("json") {
                    summary.insert("changed".into(), json!([]));
                    summary.insert("reached".into(), json!(0));
                    println!("{}", serde_json::to_string(&summary)?);
                } else {
                    println!("No changed files.");
                }
                return Ok(());
            }
            sections.push(section);
            (
                format!("changes ({})", plural(seeds.len(), "changed symbol")),
                seeds,
            )
        }
    };

    let walk = walk(&index, &seeds, depth);
    let reached: usize = walk.levels.iter().map(Vec::len).sum();
    let header = vec![format!(
        "Affected by {subject} · {} within {}",
        plural(reached, "dependent"),
        plural(depth, "hop")
    )];
    let first_distance = sections.len();
    sections.extend(distance_sections(&index, &walk, seeds.len() > 1));
    let (tests, files) = tests(&index, &seeds, &walk);
    let tests_at = sections.len();
    sections.push(test_section(&index, &tests, &files));
    sections.extend(command_section(&index, &tests));
    let footer = notes(&index, &walk, depth);

    // Commands and tests are the payoff, so they claim space first. Nearer
    // dependents come next, and the list of changed symbols fills last.
    let mut order: Vec<usize> = (tests_at..sections.len()).rev().collect();
    order.extend(first_distance..tests_at);
    order.extend(0..first_distance);
    summary.insert("depth".into(), json!(depth));
    summary.insert("reached".into(), json!(reached));
    summary.insert("beyond_depth".into(), json!(walk.beyond));
    summary.insert("notes".into(), json!(footer));
    summary.insert("capped".into(), json!(walk.full || !walk.fanned.is_empty()));
    emit(
        &header,
        &sections,
        &footer,
        &order,
        Fill::InOrder,
        budget,
        matches.is_present("json").then_some(summary),
    )
}

fn distance_sections(index: &Index, walk: &Walk, several: bool) -> Vec<Section> {
    let definitions = &index.definitions;
    let mut sections = Vec::new();
    for (offset, level) in walk.levels.iter().enumerate() {
        let distance = offset + 1;
        let mut section = Section::new(
            "dependents",
            format!("Distance {distance} · {}", level.len()),
            4,
        );
        for reach in level {
            let (who, dependent) = match reach.dependent {
                Dependent::Symbol(id) => (label(&definitions[id]), node(&definitions[id])),
                Dependent::Scope(file) => (
                    "file scope".to_owned(),
                    json!({"kind": "file scope", "file": file, "start": reach.edge.location.start, "end": reach.edge.location.end}),
                ),
            };
            let via = name(&definitions[reach.via]);
            section.push(
                format!(
                    "  {who} · {}{} at {}{}",
                    if reach.possible { "possible " } else { "" },
                    relationship(reach.edge.kind),
                    reach.edge.location,
                    if distance > 1 || several {
                        format!(" → {via}")
                    } else {
                        String::new()
                    }
                ),
                json!({
                    "distance": distance,
                    "dependent": dependent,
                    "relationship": relationship(reach.edge.kind),
                    "at": reach.edge.location.to_string(),
                    "possible": reach.possible,
                    "via": via,
                }),
            );
        }
        sections.push(section);
    }
    sections
}

fn test_section(index: &Index, tests: &[(usize, usize, bool)], files: &TestFiles) -> Section {
    let definitions = &index.definitions;
    let mut section = Section::new("tests", format!("Tests · {}", tests.len() + files.len()), 6);
    let possible = |possible: bool| if possible { "possible · " } else { "" };
    for &(id, hops, inferred) in tests {
        let mut value = node(&definitions[id]);
        value["hops"] = json!(hops);
        value["possible"] = json!(inferred);
        section.push(
            format!(
                "  {} · {}{}",
                label(&definitions[id]),
                possible(inferred),
                distance(hops)
            ),
            value,
        );
    }
    for (file, &(hops, inferred, at)) in files {
        section.push(
            format!(
                "  {file} · test file at {at} · {}{}",
                possible(inferred),
                distance(hops)
            ),
            json!({"file": file, "kind": "test file", "at": at.to_string(), "hops": hops, "possible": inferred}),
        );
    }
    if section.lines.is_empty() {
        section.title =
            "Tests · none reach the target through indexed links. Search with justless find."
                .into();
    }
    section
}

fn command_section(index: &Index, tests: &[(usize, usize, bool)]) -> Option<Section> {
    let (commands, left) = commands(index, tests);
    if commands.is_empty() {
        return None;
    }
    let title = if left > 0 {
        format!("Run · first {FILTERS} tests per command, {left} more not included")
    } else {
        "Run".into()
    };
    let mut section = Section::new("commands", title, usize::MAX);
    for command in commands {
        section.push(format!("  {command}"), json!(command));
    }
    Some(section)
}

/// Says where the walk stopped short of every dependent.
fn notes(index: &Index, walk: &Walk, depth: usize) -> Vec<String> {
    let mut notes = Vec::new();
    if walk.beyond > 0 {
        notes.push(format!(
            "Depth limit: {} past {}. Raise --depth to follow them.",
            plural(walk.beyond, "more dependent"),
            plural(depth, "hop")
        ));
    }
    for &(id, dependents) in walk.fanned.iter().take(2) {
        notes.push(format!(
            "Fan-out cap: followed {FAN_OUT} of {dependents} dependents of {}.",
            label(&index.definitions[id])
        ));
    }
    if walk.fanned.len() > 2 {
        notes.push(format!(
            "Fan-out cap also applied to {} more.",
            plural(walk.fanned.len() - 2, "symbol")
        ));
    }
    if walk.full {
        notes.push(format!("Walk cap: stopped after {MAX_REACHED} dependents."));
    }
    notes
}

fn distance(hops: usize) -> String {
    if hops == 0 {
        "target itself".into()
    } else {
        plural(hops, "hop")
    }
}

/// A file path selects every symbol in the file. Otherwise the target names one symbol.
fn resolve(index: &Index, target: &str) -> Result<(Option<String>, Vec<usize>)> {
    let file = target.trim_start_matches("./");
    if index.files.contains_key(file) {
        let mut seeds = index.at(&Location {
            file: file.to_owned(),
            start: 1,
            end: usize::MAX,
        });
        ensure!(
            !seeds.is_empty(),
            "{file} has no indexed symbols. Use justless find for its text."
        );
        seeds.sort_by_key(|&id| (&index.definitions[id].location, id));
        let subject = format!("{file} ({})", plural(seeds.len(), "symbol"));
        return Ok((Some(subject), seeds));
    }
    match index.select(target) {
        Ok(id) => Ok((None, vec![id])),
        Err(error) if target.contains('/') && error.to_string().starts_with("symbol not found") => {
            bail!("{target} is not an indexed file or symbol")
        }
        Err(error) => Err(error),
    }
}

/// Seed the walk for lines removed without a replacement. A removed symbol no
/// longer exists, so its impact starts at the code that still names it. Lines
/// removed inside a surviving symbol change that symbol. Anything else is reported
/// as unknown rather than pinned on a neighbor.
fn deleted(
    workspace: &Workspace,
    index: &Index,
    file: &str,
    deletion: &crate::context::Deletion,
    seeds: &mut BTreeSet<usize>,
    section: &mut Section,
) {
    let definitions = &index.definitions;
    let (first, last) = deletion.old;
    if deletion.symbols.is_empty() {
        // The lines on both sides of the removal belong to one symbol: it changed.
        let around = index
            .at(&Location {
                file: file.to_owned(),
                start: deletion.after.max(1),
                end: deletion.after + 1,
            })
            .into_iter()
            .filter(|&id| {
                let span = &definitions[id].location;
                span.start <= deletion.after && deletion.after < span.end
            })
            .min_by_key(|&id| {
                let span = &definitions[id].location;
                span.end - span.start
            });
        match around {
            Some(id) => {
                if seeds.insert(id) {
                    let mut value = node(&definitions[id]);
                    value["changed"] =
                        json!(format!("{file}: lines removed after {}", deletion.after));
                    section.push(
                        format!(
                            "  {} · lines removed after {file}:{}",
                            label(&definitions[id]),
                            deletion.after
                        ),
                        value,
                    );
                }
            }
            None => section.push(
                format!("  {file} · lines {first}-{last} removed between symbols · impact unknown"),
                json!({"file": file, "removed": [first, last], "note": "impact unknown"}),
            ),
        }
        return;
    }
    for (name, kind) in &deletion.symbols {
        section.push(
            format!("  {name} [{kind}] · deleted from {file} (committed lines {first}-{last})"),
            json!({"file": file, "deleted": name, "kind": kind, "removed": [first, last]}),
        );
        let short = name.rsplit("::").next().unwrap_or(name);
        let mut referenced = false;
        for (path, line) in mentions(workspace, index, short) {
            let enclosing = index
                .at(&Location {
                    file: path.clone(),
                    start: line,
                    end: line,
                })
                .into_iter()
                .min_by_key(|&id| {
                    let span = &definitions[id].location;
                    span.end - span.start
                });
            let Some(id) = enclosing else { continue };
            referenced = true;
            if seeds.insert(id) {
                let mut value = node(&definitions[id]);
                value["references_deleted"] = json!(name);
                value["changed"] = json!(format!("{path}:{line}"));
                section.push(
                    format!(
                        "  {} · still references deleted {short} at {path}:{line}",
                        label(&definitions[id])
                    ),
                    value,
                );
            }
        }
        if !referenced {
            section.push(
                format!("  no remaining references to {short} found in indexed text"),
                json!({"deleted": name, "note": "no remaining references"}),
            );
        }
    }
}

/// Lines in indexed files that mention `name` as a whole identifier.
fn mentions(workspace: &Workspace, index: &Index, name: &str) -> Vec<(String, usize)> {
    const LIMIT: usize = 256;
    let identifier = |c: char| c.is_alphanumeric() || c == '_';
    let fingerprint = crate::index::Fingerprint::new(name);
    let mut found = Vec::new();
    for (file, candidate) in &index.fingerprints {
        if !candidate.includes(fingerprint) {
            continue;
        }
        let Ok(path) = workspace.source_path(file) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for (offset, line) in text.lines().enumerate() {
            let whole = line.match_indices(name).any(|(at, _)| {
                !line[..at].chars().next_back().is_some_and(identifier)
                    && !line[at + name.len()..]
                        .chars()
                        .next()
                        .is_some_and(identifier)
            });
            if whole {
                found.push((file.clone(), offset + 1));
                if found.len() == LIMIT {
                    return found;
                }
            }
        }
    }
    found
}

/// Symbols touched by the current changes: the innermost definition around each
/// changed range, and any definition whose declaration line changed.
fn changed(workspace: &Workspace, index: &Index, staged: bool) -> Result<(Vec<usize>, Section)> {
    let definitions = &index.definitions;
    let mut seeds = BTreeSet::new();
    let mut section = Section::new("changed", String::new(), 4);
    for (file, change) in changes(workspace, index, staged)? {
        let ranges = match change {
            Change::Ranges(ranges) => ranges,
            Change::Deleted(deletions) => {
                for deletion in deletions {
                    deleted(workspace, index, &file, &deletion, &mut seeds, &mut section);
                }
                continue;
            }
            other => {
                let reason = match other {
                    Change::Unindexed => "not indexed",
                    Change::StagedDiffers => "working tree differs from staged source",
                    _ => "empty after changes",
                };
                section.push(
                    format!("  {file} · {reason}"),
                    json!({"file": file, "note": reason}),
                );
                continue;
            }
        };
        for (start, end) in ranges {
            let overlapping = index.at(&Location {
                file: file.clone(),
                start,
                end,
            });
            let picked: Vec<usize> = overlapping
                .iter()
                .copied()
                .filter(|&id| {
                    let span = &definitions[id].location;
                    let declared = (start..=end).contains(&definitions[id].declaration);
                    let innermost = !overlapping.iter().any(|&other| {
                        let inner = &definitions[other].location;
                        inner != span && span.start <= inner.start && inner.end <= span.end
                    });
                    declared || innermost
                })
                .collect();
            if picked.is_empty() {
                section.push(
                    format!("  {file}:{start}-{end} · no enclosing indexed symbol"),
                    json!({"file": file, "start": start, "end": end, "note": "no enclosing indexed symbol"}),
                );
            }
            for id in picked {
                if seeds.insert(id) {
                    let mut value = node(&definitions[id]);
                    value["changed"] = json!(format!("{file}:{start}-{end}"));
                    section.push(
                        format!(
                            "  {} · changed {file}:{start}-{end}",
                            label(&definitions[id])
                        ),
                        value,
                    );
                }
            }
        }
    }
    section.title = format!("Changed · {}", plural(seeds.len(), "symbol"));
    Ok((seeds.into_iter().collect(), section))
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Dependent<'a> {
    Symbol(usize),
    /// A file-scope reference, such as a Rust `use` outside any item.
    Scope(&'a str),
}

struct Reach<'a> {
    dependent: Dependent<'a>,
    edge: &'a Edge,
    /// The symbol this dependent links to, one hop nearer the target.
    via: usize,
    /// Whether any link on the way is inferred.
    possible: bool,
}

struct Walk<'a> {
    levels: Vec<Vec<Reach<'a>>>,
    /// Distinct dependents one hop past the depth limit.
    beyond: usize,
    /// Symbols with more new dependents than the fan-out cap, and how many they had.
    fanned: Vec<(usize, usize)>,
    /// Whether the walk stopped at `MAX_REACHED`.
    full: bool,
}

/// Links into `id` from other symbols, and from file scopes in other files.
/// A file scope in the defining file is an `impl` header or similar, not a user.
fn links_into(index: &Index, id: usize) -> impl Iterator<Item = &Edge> {
    let file = &index.definitions[id].location.file;
    index.incoming(id).filter(move |edge| match edge.from {
        Some(from) => from != id,
        None => edge.location.file != *file,
    })
}

/// Breadth-first over incoming links. Each dependent appears once, at its
/// shortest distance, through its most certain link, so cycles terminate.
fn walk<'a>(index: &'a Index, seeds: &[usize], depth: usize) -> Walk<'a> {
    let definitions = &index.definitions;
    let dependent = |edge: &'a Edge| match edge.from {
        Some(from) => Dependent::Symbol(from),
        None => Dependent::Scope(&edge.location.file),
    };
    let mut seen: BTreeSet<Dependent> = seeds.iter().map(|&id| Dependent::Symbol(id)).collect();
    let mut frontier: Vec<(usize, bool)> = seeds.iter().map(|&id| (id, false)).collect();
    let mut levels = Vec::new();
    let mut fanned = Vec::new();
    let mut full = false;
    for _ in 0..depth {
        let mut next = BTreeMap::<Dependent, Reach>::new();
        for &(id, possible) in &frontier {
            // One link per new dependent, the most certain and then the first.
            let mut links = BTreeMap::<Dependent, &Edge>::new();
            for edge in links_into(index, id) {
                let key = dependent(edge);
                if seen.contains(&key) {
                    continue;
                }
                let link = links.entry(key).or_insert(edge);
                if (edge.inferred, &edge.location) < (link.inferred, &link.location) {
                    *link = edge;
                }
            }
            let mut links: Vec<_> = links.into_iter().collect();
            links.sort_by_key(|(key, edge)| (edge.inferred, &edge.location, *key));
            if links.len() > FAN_OUT {
                fanned.push((id, links.len()));
                links.truncate(FAN_OUT);
            }
            for (key, edge) in links {
                let reach = Reach {
                    dependent: key,
                    edge,
                    via: id,
                    possible: possible || edge.inferred,
                };
                if next.get(&key).is_none_or(|current| {
                    (reach.possible, &reach.edge.location)
                        < (current.possible, &current.edge.location)
                }) {
                    next.insert(key, reach);
                }
            }
        }
        let mut level: Vec<Reach> = next.into_values().collect();
        level.sort_by(|a, b| {
            let place = |reach: &Reach| match reach.dependent {
                Dependent::Symbol(id) => (false, definitions[id].location.clone()),
                Dependent::Scope(_) => (true, reach.edge.location.clone()),
            };
            (a.possible, place(a)).cmp(&(b.possible, place(b)))
        });
        if level.is_empty() {
            frontier.clear();
            break;
        }
        let room = MAX_REACHED.saturating_sub(seen.len().saturating_sub(seeds.len()));
        if level.len() > room {
            full = true;
            level.truncate(room);
        }
        seen.extend(level.iter().map(|reach| reach.dependent));
        frontier = level
            .iter()
            .filter_map(|reach| match reach.dependent {
                Dependent::Symbol(id) => Some((id, reach.possible)),
                Dependent::Scope(_) => None,
            })
            .collect();
        levels.push(level);
        if frontier.is_empty() || full {
            break;
        }
    }
    let mut beyond = BTreeSet::new();
    if !full {
        for &(id, _) in &frontier {
            for edge in links_into(index, id) {
                let key = dependent(edge);
                if !seen.contains(&key) {
                    beyond.insert(key);
                }
            }
        }
    }
    Walk {
        levels,
        beyond: beyond.len(),
        fanned,
        full,
    }
}

type TestFiles<'a> = BTreeMap<&'a str, (usize, bool, &'a Location)>;

/// Test definitions that reach a seed, from the walk and from call chains up to
/// eight hops regardless of `--depth`, and test-convention files reached without
/// a detected test definition, such as JavaScript suites.
fn tests<'a>(
    index: &'a Index,
    seeds: &[usize],
    walk: &Walk<'a>,
) -> (Vec<(usize, usize, bool)>, TestFiles<'a>) {
    let definitions = &index.definitions;
    let mut found = BTreeMap::<usize, (bool, usize)>::new();
    let mut keep = |id: usize, hops: usize, possible: bool| {
        let entry = found.entry(id).or_insert((possible, hops));
        *entry = (*entry).min((possible, hops));
    };
    for &seed in seeds {
        if definitions[seed].test {
            keep(seed, 0, false);
        }
        for (id, hops, inferred) in index.related_tests(seed, 32) {
            keep(id, hops, inferred);
        }
    }
    for (offset, level) in walk.levels.iter().enumerate() {
        for reach in level {
            if let Dependent::Symbol(id) = reach.dependent
                && definitions[id].test
            {
                keep(id, offset + 1, reach.possible);
            }
        }
    }
    let mut tests: Vec<_> = found
        .into_iter()
        .map(|(id, (possible, hops))| (id, hops, possible))
        .collect();
    tests.sort_by_key(|&(id, hops, possible)| (possible, hops, &definitions[id].location, id));
    let covered: BTreeSet<&str> = tests
        .iter()
        .map(|&(id, _, _)| definitions[id].location.file.as_str())
        .collect();
    let mut files = TestFiles::new();
    let seed_files = seeds
        .iter()
        .map(|&id| (&definitions[id].location, 0, false));
    let reached = walk.levels.iter().enumerate().flat_map(|(offset, level)| {
        level
            .iter()
            .map(move |reach| (&reach.edge.location, offset + 1, reach.possible))
    });
    for (location, hops, possible) in seed_files.chain(reached) {
        let file = location.file.as_str();
        if is_test_file(file) && !covered.contains(file) {
            let entry = files.entry(file).or_insert((hops, possible, location));
            if (possible, hops) < (entry.1, entry.0) {
                *entry = (hops, possible, location);
            }
        }
    }
    (tests, files)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Runner {
    Cargo,
    Pytest,
    Go,
}

/// Commands that run just the given tests, where the file layout makes the
/// runner and test name unambiguous. Returns the commands and how many
/// matching tests did not fit in them.
fn commands(index: &Index, tests: &[(usize, usize, bool)]) -> (Vec<String>, usize) {
    let mut groups = BTreeMap::<(Runner, String), Vec<String>>::new();
    for &(id, _, _) in tests {
        let definition = &index.definitions[id];
        let selected = cargo(index, definition)
            .map(|(prefix, filter)| (Runner::Cargo, prefix, filter))
            .or_else(|| pytest(definition).map(|filter| (Runner::Pytest, "pytest".into(), filter)))
            .or_else(|| go(index, definition).map(|(prefix, filter)| (Runner::Go, prefix, filter)));
        if let Some((runner, prefix, filter)) = selected {
            let filters = groups.entry((runner, prefix)).or_default();
            if !filters.contains(&filter) {
                filters.push(filter);
            }
        }
    }
    let mut left = 0;
    let commands = groups
        .into_iter()
        .map(|((runner, prefix), mut filters)| {
            left += filters.len().saturating_sub(FILTERS);
            filters.truncate(FILTERS);
            match runner {
                Runner::Cargo if filters.len() == 1 => format!("{prefix} {}", filters[0]),
                Runner::Cargo => format!("{prefix} -- {}", filters.join(" ")),
                Runner::Pytest => format!("{prefix} {}", filters.join(" ")),
                Runner::Go if filters.len() == 1 => format!("{prefix} -run '^{}$'", filters[0]),
                Runner::Go => format!("{prefix} -run '^({})$'", filters.join("|")),
            }
        })
        .collect();
    (commands, left)
}

/// `cargo test` with the test's path within its crate as the filter. Tests in
/// an automatically discovered `tests/NAME.rs` target also select `--test NAME`.
fn cargo(index: &Index, definition: &crate::index::Definition) -> Option<(String, String)> {
    let file = Path::new(&definition.location.file);
    if file.extension()? != "rs" {
        return None;
    }
    let (target, filter) = definition.qualified.split_once("::")?;
    if target == "crate" {
        return None;
    }
    let package = file.ancestors().skip(1).find(|directory| {
        directory
            .join("Cargo.toml")
            .to_str()
            .is_some_and(|manifest| index.files.contains_key(manifest))
    })?;
    let mut command = "cargo test".to_owned();
    if !package.as_os_str().is_empty() {
        command.push_str(&format!(
            " --manifest-path {}",
            quote(package.join("Cargo.toml").to_str()?)
        ));
    }
    let relative = file.strip_prefix(package).ok()?;
    let stem = relative.file_stem()?.to_str()?;
    if relative.parent() == Some(Path::new("tests")) && target.ends_with(&format!("_test_{stem}")) {
        command.push_str(&format!(" --test {}", quote(stem)));
    }
    Some((command, quote(filter)))
}

/// A pytest node ID, which is the indexed qualified name, for test files that
/// pytest collects by default.
fn pytest(definition: &crate::index::Definition) -> Option<String> {
    let file = Path::new(&definition.location.file);
    if file.extension()? != "py" {
        return None;
    }
    let stem = file.file_stem()?.to_str()?;
    if !(stem.starts_with("test_") || stem.ends_with("_test")) {
        return None;
    }
    let parts: Vec<&str> = name(definition).split("::").collect();
    let collected = match parts.as_slice() {
        [_] => true,
        [class, _] => class.starts_with("Test"),
        _ => false,
    };
    collected.then(|| quote(&definition.qualified))
}

/// `go test` for the package directory of a `_test.go` file in a module rooted
/// at the workspace.
fn go(index: &Index, definition: &crate::index::Definition) -> Option<(String, String)> {
    let file = &definition.location.file;
    let test = name(definition);
    if !file.ends_with("_test.go") || !index.files.contains_key("go.mod") || test.contains("::") {
        return None;
    }
    if !test
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    let package = match Path::new(file).parent().and_then(Path::to_str) {
        Some("") | None => ".".to_owned(),
        Some(directory) => format!("./{directory}"),
    };
    Some((format!("go test {}", quote(&package)), test.to_owned()))
}

/// Quotes a shell word unless it is plainly safe.
fn quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_./:=@%+,-".contains(&byte))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}
