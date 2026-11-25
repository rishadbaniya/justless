//! `justless affected`: a reverse walk from a symbol, a file or the current Git
//! changes to the code that depends on it, followed by the tests that reach it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Result, bail, ensure};
use clap::{Arg, ArgGroup, ArgMatches, Command};
use serde_json::{Map, json};

use crate::context::{Change, Deletion, Since, changes, relationship};
use crate::index::{Definition, Edge, Index};
use crate::map::{Fill, Section, emit, is_test_file, label, name, node, plural};
use crate::polyglot::Grammar;
use crate::rust_syntax::Kind;
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
        .arg(
            crate::base_arg()
                .requires("changes")
                .conflicts_with("staged"),
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
    let (subject, seeds, possible) = match matches.value_of("target") {
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
                BTreeSet::new(),
            )
        }
        None => {
            let since = Since::resolve(
                workspace,
                matches.is_present("staged"),
                matches.value_of("base"),
            )?;
            let (seeds, section) = changed(workspace, &index, &since)?;
            let Seeds {
                all: seeds,
                possible,
            } = seeds;
            let seeds: Vec<usize> = seeds.into_iter().collect();
            if section.lines.is_empty() {
                if matches.is_present("json") {
                    summary.insert("changed".into(), json!([]));
                    summary.insert("reached".into(), json!(0));
                    println!("{}", serde_json::to_string(&summary)?);
                } else {
                    println!("{}", since.nothing_changed(workspace));
                }
                return Ok(());
            }
            sections.push(section);
            (
                format!("changes ({})", plural(seeds.len(), "changed symbol")),
                seeds,
                possible,
            )
        }
    };

    let walk = walk_from(&index, &seeds, &possible, depth);
    let reached: usize = walk.levels.iter().map(Vec::len).sum();
    let header = vec![format!(
        "Affected by {subject} · {} within {}",
        plural(reached, "dependent"),
        plural(depth, "hop")
    )];
    let first_distance = sections.len();
    sections.extend(distance_sections(&index, &walk, seeds.len() > 1));
    let (tests, files, extra_notes) = tests(workspace, &index, &seeds, &possible, &walk);
    let tests_at = sections.len();
    sections.push(test_section(&index, &tests, &files));
    sections.extend(command_section(workspace, &index, &tests, &files));
    let mut footer = notes(&index, &walk, depth);
    footer.extend(extra_notes);

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
    for (file, info) in files {
        let note = info
            .note
            .as_deref()
            .map(|why| format!(" ({why})"))
            .unwrap_or_default();
        let mut value = json!({
            "file": file,
            "kind": "test file",
            "at": info.at.to_string(),
            "hops": info.hops,
            "possible": info.possible,
        });
        if let Some(why) = &info.note {
            value["note"] = json!(why);
        }
        section.push(
            format!(
                "  {file} · test file at {}{note} · {}{}",
                info.at,
                possible(info.possible),
                distance(info.hops)
            ),
            value,
        );
    }
    if section.lines.is_empty() {
        section.title = format!(
            "Tests · none reach the target through indexed links. Search with {} find.",
            index.justless
        );
    }
    section
}

fn command_section(
    workspace: &Workspace,
    index: &Index,
    tests: &[(usize, usize, bool)],
    files: &TestFiles,
) -> Option<Section> {
    let (commands, left) = commands(workspace, index, tests, files);
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
            "{file} has no indexed symbols. Use {} find for its text.",
            index.justless
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

/// Symbols the walk starts from, and those among them only possibly affected.
#[derive(Default)]
struct Seeds {
    all: BTreeSet<usize>,
    possible: BTreeSet<usize>,
}

impl Seeds {
    /// Adds a seed. Says whether that is worth a line: a new seed, or a certain
    /// reason for one that was only possible.
    fn add(&mut self, id: usize, possible: bool) -> bool {
        if self.all.insert(id) {
            if possible {
                self.possible.insert(id);
            }
            true
        } else {
            !possible && self.possible.remove(&id)
        }
    }
}

/// What the symbols removed by a set of changes are followed with.
struct Removals {
    /// The index rebuilt with every removed definition restored, and the ids
    /// each took there, in the order the deletions list them.
    rebuilt: Option<(Index, Vec<Vec<usize>>)>,
    /// The remaining mentions of each removed name.
    searches: BTreeMap<String, Mentions>,
}

/// The name code uses for a symbol listed as `Type::name` or `name`.
fn short_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Searches for each removed name, then restores every removed definition at
/// once in a rebuilt index, so the references that remain resolve as if each
/// still existed. The Rust files that name one are parsed again with their
/// macro bodies there, so a call such as `ensure!(check(x))` resolves too.
fn removals(workspace: &Workspace, index: &Index, changes: &[(String, Change)]) -> Removals {
    let mut removed = Vec::new();
    let mut searches = BTreeMap::new();
    for (file, change) in changes {
        let Change::Deleted(deletions) = change else {
            continue;
        };
        for deletion in deletions {
            removed.extend(
                deletion
                    .removed
                    .iter()
                    .map(|(_, definition)| (file.as_str(), definition)),
            );
            for (name, kind) in &deletion.symbols {
                let short = short_name(name);
                if kind != "import" && !searches.contains_key(short) {
                    searches.insert(short.to_owned(), mentions(workspace, index, short));
                }
            }
        }
    }
    let rust: BTreeSet<&String> = searches
        .values()
        .flat_map(|search: &Mentions| &search.lines)
        .map(|mention| &mention.file)
        .filter(|file| file.ends_with(".rs"))
        .collect();
    let texts: Vec<(&str, String)> = rust
        .into_iter()
        .filter_map(|file| {
            let text = fs::read_to_string(workspace.source_path(file).ok()?).ok()?;
            Some((file.as_str(), text))
        })
        .collect();
    let wider: Vec<(&str, &str)> = texts
        .iter()
        .map(|(file, text)| (*file, text.as_str()))
        .collect();
    let rebuilt = if removed.is_empty() && wider.is_empty() {
        None
    } else {
        index.with_removed(&removed, &wider)
    };
    Removals { rebuilt, searches }
}

/// Seed the walk for lines removed without a replacement. A removed symbol no
/// longer exists, so its impact starts at the code that still references it.
/// Lines removed inside a surviving symbol change that symbol. Anything else,
/// such as an import, is reported as unknown rather than pinned on a neighbor.
fn deleted(
    index: &Index,
    removals: &Removals,
    restored: &[Vec<usize>],
    file: &str,
    deletion: &Deletion,
    seeds: &mut Seeds,
    section: &mut Section,
) {
    let definitions = &index.definitions;
    let (first, last) = deletion.old;
    // An import is not a definition, so removing only imports removes lines.
    if deletion.symbols.iter().all(|(_, kind)| kind == "import") {
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
                if seeds.add(id, false) {
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
    let mut imports = false;
    for (position, (name, kind)) in deletion.symbols.iter().enumerate() {
        if kind == "import" {
            imports = true;
            continue;
        }
        section.push(
            format!("  {name} [{kind}] · deleted from {file} (committed lines {first}-{last})"),
            json!({"file": file, "deleted": name, "kind": kind, "removed": [first, last]}),
        );
        // The removed definition and its members, as restored in the rebuilt index.
        let ids: Vec<usize> = deletion
            .removed
            .iter()
            .zip(restored)
            .filter(|((owner, _), _)| *owner == position)
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect();
        let short = short_name(name);
        let removal = Removal {
            name,
            short,
            member: is_member(kind),
        };
        follow(
            index,
            removals.rebuilt.as_ref().map(|(rebuilt, _)| rebuilt),
            &ids,
            &removals.searches[short],
            &removal,
            seeds,
            section,
        );
    }
    if imports {
        section.push(
            format!("  {file} · code that used the imports removed with them is not traced"),
            json!({"file": file, "removed": [first, last], "note": "imports not traced"}),
        );
    }
}

/// A symbol a change removed, as `affected` follows it.
struct Removal<'a> {
    /// Its name within its file, as the deletion lists it.
    name: &'a str,
    /// The name code uses for it.
    short: &'a str,
    /// Whether it was a method or other member, which a value can reach with `.`.
    member: bool,
}

/// Whether a definition of this kind is a member, which a value or type can
/// reach with `.` or `::`.
fn is_member(kind: &str) -> bool {
    kind == "method" || kind.starts_with("associated")
}

/// How surely a remaining line references a removed symbol.
#[derive(Clone, Copy, PartialEq)]
enum Doubt {
    Certain,
    Ambiguous,
    NameOnly,
}

/// Seeds the code that still references a removed symbol, restored as `ids`
/// in the `rebuilt` index. A link that resolves to it is certain, and an
/// ambiguous one is possible. A remaining mention of its name does not count
/// when it resolves to or declares another definition, or when, in Rust, it is
/// plainly a field, binding, comment or other path. Any other mention is a
/// possible reference, because the index cannot place it.
fn follow(
    index: &Index,
    rebuilt: Option<&Index>,
    ids: &[usize],
    search: &Mentions,
    removed: &Removal,
    seeds: &mut Seeds,
    section: &mut Section,
) {
    let definitions = &index.definitions;
    let Removal {
        name,
        short,
        member,
    } = *removed;
    let named = |definition: &Definition| short_name(crate::map::name(definition)) == short;
    let shown = match rebuilt.zip(ids.first()) {
        Some((rebuilt, &id)) => {
            let definition = &rebuilt.definitions[id];
            if definition.qualified.starts_with(&definition.location.file) {
                definition.qualified.clone()
            } else {
                crate::map::name(definition).to_owned()
            }
        }
        None => name.to_owned(),
    };

    // Lines that link to the restored symbol: certain unless every link is ambiguous.
    let mut found = BTreeMap::<(String, usize), Doubt>::new();
    for edge in rebuilt
        .into_iter()
        .flat_map(|rebuilt| ids.iter().flat_map(|&id| rebuilt.incoming(id)))
    {
        let doubt = if edge.inferred {
            Doubt::Ambiguous
        } else {
            Doubt::Certain
        };
        let entry = found
            .entry((edge.location.file.clone(), edge.location.start))
            .or_insert(doubt);
        if doubt == Doubt::Certain {
            *entry = doubt;
        }
    }

    // The remaining mentions of its name.
    let resolving = rebuilt.unwrap_or(index);
    let lines: BTreeSet<(&str, usize)> = search
        .lines
        .iter()
        .map(|mention| (mention.file.as_str(), mention.line))
        .collect();
    // A path into the standard library names none of the workspace's items,
    // unless the workspace defines a module or crate of that name, such as `mod core;`.
    let standard: Vec<&str> = STANDARD
        .into_iter()
        .filter(|root| {
            !resolving.definitions.iter().any(|definition| {
                short_name(crate::map::name(definition)) == *root
                    || definition
                        .qualified
                        .strip_prefix(root)
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
            })
        })
        .collect();
    // Members of the same name are tied through a trait, as an implementation
    // is to the trait item it implements, so for a removed member a link to or
    // declaration of one proves nothing when either belongs to a trait. A
    // removed member that could not be restored may belong to one.
    let traits: BTreeSet<&str> = resolving
        .definitions
        .iter()
        .filter(|definition| definition.kind == "trait")
        .map(|definition| definition.qualified.as_str())
        .collect();
    let in_trait = |definition: &Definition| {
        definition
            .qualified
            .rsplit_once("::")
            .is_some_and(|(owner, _)| traits.contains(owner))
    };
    let trait_member = member
        && rebuilt.is_none_or(|rebuilt| {
            ids.is_empty() || ids.iter().any(|&id| in_trait(&rebuilt.definitions[id]))
        });
    let tied = |definition: &Definition| {
        member && is_member(&definition.kind) && (trait_member || in_trait(definition))
    };
    // A certain link from a mention line to another definition of that name.
    // The index can bind a bare name inside a method to that method, which Rust
    // does not, so a link to a method only counts for a removed member.
    let mut other = BTreeMap::<(&str, usize), String>::new();
    for edge in &resolving.edges {
        let at = (edge.location.file.as_str(), edge.location.start);
        let target = &resolving.definitions[edge.to];
        if !edge.inferred
            && !ids.contains(&edge.to)
            && lines.contains(&at)
            && named(target)
            && (member || target.kind != "method")
            && !tied(target)
        {
            other.entry(at).or_insert_with(|| label(target));
        }
    }
    let mut elsewhere = Vec::new();
    for mention in &search.lines {
        let at = (mention.file.as_str(), mention.line);
        if found.contains_key(&(mention.file.clone(), mention.line)) {
            continue;
        }
        let rust = mention.file.ends_with(".rs");
        let occurrences: Vec<usize> = whole_words(&mention.text, short).collect();
        // Links carry a line, not a column, so a link to another definition
        // explains a line only where the name appears once. Outside Rust, the
        // index links only the bare name, so a link says nothing about
        // `module.name` on the same line.
        let attribute = !rust && occurrences.iter().any(|&at| after_dot(&mention.text[..at]));
        if occurrences.len() == 1
            && !attribute
            && let Some(target) = other.get(&at)
        {
            elsewhere.push(target.clone());
            continue;
        }
        // A declaration of another definition explains its own name only.
        let declared = index
            .at(&Location {
                file: mention.file.clone(),
                start: mention.line,
                end: mention.line,
            })
            .into_iter()
            .find(|&id| definitions[id].declaration == mention.line && named(&definitions[id]));
        if occurrences.len() == 1
            && let Some(id) = declared
            && !tied(&definitions[id])
        {
            elsewhere.push(label(&definitions[id]));
            continue;
        }
        if rust {
            // The paths parsed on the line, macro bodies included where the
            // rebuilt index read them. A `use` resolves without following
            // re-exports, so it never shows a path to be another item.
            let paths = rebuilt
                .and_then(|rebuilt| rebuilt.rust_paths_at(&mention.file, mention.line))
                .or_else(|| index.rust_paths_at(&mention.file, mention.line))
                .unwrap_or_default();
            let paths: Vec<&[String]> = paths
                .into_iter()
                .filter(|(kind, _)| *kind != Kind::Import)
                .map(|(_, path)| path)
                .collect();
            if plainly_not_a_reference(&mention.text, short, member, &paths, &standard) {
                continue;
            }
        }
        found.insert((mention.file.clone(), mention.line), Doubt::NameOnly);
    }

    let mut referenced = false;
    let mut outside = Vec::new();
    for ((path, line), doubt) in found {
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
        let possible = doubt != Doubt::Certain;
        let (verb, reason) = match doubt {
            Doubt::Certain => ("still references", ""),
            Doubt::Ambiguous => ("possibly references", " (ambiguous link)"),
            Doubt::NameOnly => ("possibly references", " (name match only)"),
        };
        let Some(id) = enclosing else {
            // Text outside every definition, such as documentation, is listed
            // in one line. A link from file scope, such as a `use`, is a reference.
            if doubt == Doubt::NameOnly {
                outside.push(format!("{path}:{line}"));
            } else {
                referenced = true;
                section.push(
                    format!("  {path}:{line} · file scope {verb} deleted {short}{reason}"),
                    json!({"file": path, "line": line, "references_deleted": name, "possible": possible, "note": "file scope"}),
                );
            }
            continue;
        };
        referenced = true;
        if seeds.add(id, possible) {
            let mut value = node(&definitions[id]);
            value["references_deleted"] = json!(name);
            value["changed"] = json!(format!("{path}:{line}"));
            value["possible"] = json!(possible);
            section.push(
                format!(
                    "  {} · {verb} deleted {short} at {path}:{line}{reason}",
                    label(&definitions[id])
                ),
                value,
            );
        }
    }
    if let Some(first) = outside.first() {
        section.push(
            format!(
                "  {} of {short} outside indexed definitions, such as {first} · not followed",
                plural(outside.len(), "mention")
            ),
            json!({"deleted": name, "note": "mentions outside definitions", "count": outside.len(), "first": first}),
        );
    }
    if !referenced {
        let mut text = format!("  no remaining references to {shown} found");
        // Mentions outside definitions, such as a `macro_rules!` body, were not read.
        if !outside.is_empty() {
            text.push_str(" inside indexed definitions");
        }
        if let Some(first) = elsewhere.first() {
            text.push_str(&format!(
                " · {} resolve to other definitions, such as {first}",
                plural(elsewhere.len(), "mention")
            ));
        }
        section.push(
            text,
            json!({"deleted": name, "note": "no remaining references", "elsewhere": elsewhere.len(), "outside": outside.len()}),
        );
    }
    if search.capped {
        section.push(
            format!(
                "  name search stopped after {MENTIONS} matches; later mentions of {short} were not checked"
            ),
            json!({"deleted": name, "note": "name search capped", "limit": MENTIONS}),
        );
    }
}

/// Whether no whole-word `name` on a line of Rust can be a path to a removed
/// item. Each must be a field or method after `.` (unless the item was a
/// member), a field, parameter or binding name before a single `:`, a `let`
/// binding, part of a `//` comment that is not documentation, or the end of a
/// path the parser recorded on the line that starts at one of the `standard`
/// library crates. Any other qualified path stays, because resolution can miss
/// one that reaches the item, such as `T::name` or a path through a type alias.
/// So does a name inside a string, which a format string such as `"{name:?}"`
/// can capture.
fn plainly_not_a_reference(
    text: &str,
    name: &str,
    member: bool,
    paths: &[&[String]],
    standard: &[&str],
) -> bool {
    let comment = plain_comment(text);
    let standard_root = |segment: &str| standard.contains(&segment);
    let standard = paths.iter().any(|path| {
        path.len() > 1 && path.last().is_some_and(|last| last == name) && standard_root(&path[0])
    });
    whole_words(text, name).all(|at| {
        if comment.is_some_and(|start| at > start) {
            return true;
        }
        if in_string(text, at) {
            return false;
        }
        let before = text[..at].trim_end();
        let after = text[at + name.len()..].trim_start();
        (after_dot(before) && !member)
            || (after.starts_with(':') && !after.starts_with("::"))
            || binding(before, name, after)
            || (standard && path_root(before).is_some_and(standard_root))
    })
}

/// Whether byte `at` of a line of Rust falls inside a string literal that
/// opens on that line.
fn in_string(text: &str, at: usize) -> bool {
    let bytes = text.as_bytes();
    let mut inside = false;
    let mut index = 0;
    while index < at {
        match bytes[index] {
            b'\\' if inside => index += 1,
            b'"' => inside = !inside,
            // A character literal such as '"' or '\"' does not open a string.
            b'\'' if !inside => {
                if bytes.get(index + 2) == Some(&b'\'') {
                    index += 2;
                } else if bytes.get(index + 1) == Some(&b'\\')
                    && bytes.get(index + 3) == Some(&b'\'')
                {
                    index += 3;
                }
            }
            _ => {}
        }
        index += 1;
    }
    inside
}

/// The standard library crates.
const STANDARD: [&str; 3] = ["std", "core", "alloc"];

/// The first segment of the path that the text before a name ends with, such
/// as `std` for `std::env::` or `::std::env::`.
fn path_root(before: &str) -> Option<&str> {
    let start = before
        .trim_end_matches(|c: char| c.is_alphanumeric() || c == '_' || c == ':')
        .len();
    let path = &before[start..];
    path.ends_with("::")
        .then(|| path.trim_start_matches("::").split("::").next())
        .flatten()
}

/// Whether the text before a name ends with a single `.`, as in `value.name`.
fn after_dot(before: &str) -> bool {
    let before = before.trim_end();
    before.ends_with('.') && !before.ends_with("..")
}

/// Where a `//` comment starts on a line of Rust, unless it is documentation,
/// which can hold a compiled example, or a quote before it could make it part
/// of a string.
fn plain_comment(text: &str) -> Option<usize> {
    let start = text.find("//")?;
    if text[..start].contains(['"', '\'']) {
        return None;
    }
    let rest = &text[start..];
    let documentation =
        (rest.starts_with("///") && !rest.starts_with("////")) || rest.starts_with("//!");
    (!documentation).then_some(start)
}

/// Whether a name follows `let` or `let mut` as a variable it binds. A name
/// that opens a path, tuple struct or struct pattern, as in `let Config { host }
/// = c;`, names a type, and so may a capitalized one, such as a unit struct
/// or constant matched by `let Marker = marker;`.
fn binding(before: &str, name: &str, after: &str) -> bool {
    let before = before.strip_suffix("mut").map_or(before, str::trim_end);
    before
        .strip_suffix("let")
        .is_some_and(|rest| !rest.ends_with(|c: char| c.is_alphanumeric() || c == '_'))
        && name.starts_with(|c: char| c.is_lowercase() || c == '_')
        && !after.starts_with(['(', '{'])
        && !after.starts_with("::")
}

/// Byte offsets where `name` appears in `line` as a whole identifier.
fn whole_words<'a>(line: &'a str, name: &'a str) -> impl Iterator<Item = usize> + 'a {
    let identifier = |c: char| c.is_alphanumeric() || c == '_';
    line.match_indices(name)
        .map(|(at, _)| at)
        .filter(move |&at| {
            !line[..at].chars().next_back().is_some_and(identifier)
                && !line[at + name.len()..]
                    .chars()
                    .next()
                    .is_some_and(identifier)
        })
}

/// Lines a name search stops at.
const MENTIONS: usize = 256;

/// Lines of indexed text that name a removed symbol.
struct Mentions {
    lines: Vec<Mention>,
    /// Whether the search stopped at `MENTIONS` lines.
    capped: bool,
}

struct Mention {
    file: String,
    line: usize,
    text: String,
}

/// Lines in indexed files that mention `name` as a whole identifier.
fn mentions(workspace: &Workspace, index: &Index, name: &str) -> Mentions {
    let fingerprint = crate::index::Fingerprint::new(name);
    let mut lines = Vec::new();
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
            if whole_words(line, name).next().is_some() {
                if lines.len() == MENTIONS {
                    return Mentions {
                        lines,
                        capped: true,
                    };
                }
                lines.push(Mention {
                    file: file.clone(),
                    line: offset + 1,
                    text: line.to_owned(),
                });
            }
        }
    }
    Mentions {
        lines,
        capped: false,
    }
}

/// Symbols touched by the current changes: the innermost definition around each
/// changed range, and any definition whose declaration line changed.
fn changed(workspace: &Workspace, index: &Index, since: &Since) -> Result<(Seeds, Section)> {
    Ok(seeded(workspace, index, &changes(workspace, index, since)?))
}

/// [`changed`] over a change set the caller already collected.
pub(crate) fn changed_in(
    workspace: &Workspace,
    index: &Index,
    changes: &[(String, Change)],
) -> (Vec<usize>, Section) {
    let (seeds, section) = seeded(workspace, index, changes);
    (seeds.all.into_iter().collect(), section)
}

/// [`changed_in`], keeping which seeds are only possibly affected.
fn seeded(workspace: &Workspace, index: &Index, changes: &[(String, Change)]) -> (Seeds, Section) {
    let definitions = &index.definitions;
    let mut seeds = Seeds::default();
    let mut section = Section::new("changed", String::new(), 4);
    let removals = removals(workspace, index, changes);
    let mut next = 0;
    for (file, change) in changes {
        let ranges = match change {
            Change::Ranges(ranges) => ranges,
            Change::Deleted(deletions) => {
                for deletion in deletions {
                    let count = deletion.removed.len();
                    let restored = removals
                        .rebuilt
                        .as_ref()
                        .map_or(&[][..], |(_, ids)| &ids[next..next + count]);
                    next += count;
                    deleted(
                        index,
                        &removals,
                        restored,
                        file,
                        deletion,
                        &mut seeds,
                        &mut section,
                    );
                }
                continue;
            }
            other => {
                let reason = match other {
                    Change::Unindexed => "not indexed",
                    Change::StagedDiffers => "working tree differs from staged source",
                    Change::FileLevel(what) | Change::Origin(what) => what,
                    Change::Empty | Change::Ranges(_) | Change::Deleted(_) => "empty after changes",
                };
                section.push(
                    format!("  {file} · {reason}"),
                    json!({"file": file, "note": reason}),
                );
                continue;
            }
        };
        for &(start, end) in ranges {
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
                if seeds.add(id, false) {
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
    section.title = format!("Changed · {}", plural(seeds.all.len(), "symbol"));
    (seeds, section)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Dependent<'a> {
    Symbol(usize),
    /// A file-scope reference, such as a Rust `use` outside any item.
    Scope(&'a str),
}

pub(crate) struct Reach<'a> {
    pub dependent: Dependent<'a>,
    pub edge: &'a Edge,
    /// The symbol this dependent links to, one hop nearer the target.
    pub via: usize,
    /// Whether any link on the way is inferred.
    pub possible: bool,
}

pub(crate) struct Walk<'a> {
    pub levels: Vec<Vec<Reach<'a>>>,
    /// Distinct dependents one hop past the depth limit.
    pub beyond: usize,
    /// Symbols with more new dependents than the fan-out cap, and how many they had.
    pub fanned: Vec<(usize, usize)>,
    /// Whether the walk stopped at `MAX_REACHED`.
    pub full: bool,
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
pub(crate) fn walk<'a>(index: &'a Index, seeds: &[usize], depth: usize) -> Walk<'a> {
    walk_from(index, seeds, &BTreeSet::new(), depth)
}

/// [`walk`], where whatever a `possible` seed reaches is possible too.
fn walk_from<'a>(
    index: &'a Index,
    seeds: &[usize],
    possible: &BTreeSet<usize>,
    depth: usize,
) -> Walk<'a> {
    let definitions = &index.definitions;
    let dependent = |edge: &'a Edge| match edge.from {
        Some(from) => Dependent::Symbol(from),
        None => Dependent::Scope(&edge.location.file),
    };
    let mut seen: BTreeSet<Dependent> = seeds.iter().map(|&id| Dependent::Symbol(id)).collect();
    let mut frontier: Vec<(usize, bool)> = seeds
        .iter()
        .map(|&id| (id, possible.contains(&id)))
        .collect();
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

#[derive(Debug)]
struct TestFileHit {
    hops: usize,
    possible: bool,
    at: Location,
    note: Option<String>,
}

type TestFiles = BTreeMap<String, TestFileHit>;

/// Whether a file belongs to the JavaScript/TypeScript/component family.
fn is_js_file(file: &str) -> bool {
    Grammar::for_file(file).is_some_and(|grammar| grammar.family() == Grammar::JavaScript)
}

/// Test definitions that reach a seed, from the walk and from call chains up to
/// eight hops regardless of `--depth`, plus Wave 1 indexed JS/TS (`describe`/
/// `it`/`test`) and Ruby RSpec (`describe`/`context`/`it`/`specify`) test
/// blocks, and test-convention files reached at file scope or conservatively
/// widened for barrel re-exports, dynamic imports, or shared configs.
fn tests(
    workspace: &Workspace,
    index: &Index,
    seeds: &[usize],
    possible: &BTreeSet<usize>,
    walk: &Walk<'_>,
) -> (Vec<(usize, usize, bool)>, TestFiles, Vec<String>) {
    let definitions = &index.definitions;
    let mut found = BTreeMap::<usize, (bool, usize)>::new();
    let mut keep = |id: usize, hops: usize, possible: bool| {
        let entry = found.entry(id).or_insert((possible, hops));
        *entry = (*entry).min((possible, hops));
    };
    for &seed in seeds {
        let doubtful = possible.contains(&seed);
        if definitions[seed].test {
            keep(seed, 0, doubtful);
        }
        for (id, hops, inferred) in index.related_tests(seed, 32) {
            keep(id, hops, inferred || doubtful);
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

    // Collect every reached symbol and file to drive JS/TS and Ruby augmentations.
    let mut all_ids =
        Vec::with_capacity(seeds.len() + walk.levels.iter().map(Vec::len).sum::<usize>());
    let mut seen_ids = BTreeSet::new();
    for &seed in seeds {
        if seen_ids.insert(seed) {
            all_ids.push(seed);
        }
    }
    for level in &walk.levels {
        for reach in level {
            if let Dependent::Symbol(id) = reach.dependent
                && seen_ids.insert(id)
            {
                all_ids.push(id);
            }
        }
    }

    let mut files = TestFiles::new();
    let mut insert_file = |file: String, hit: TestFileHit| {
        let entry = files.entry(file).or_insert_with(|| TestFileHit {
            hops: hit.hops,
            possible: hit.possible,
            at: hit.at.clone(),
            note: hit.note.clone(),
        });
        if (hit.possible, hit.hops) < (entry.possible, entry.hops)
            || (entry.note.is_none() && hit.note.is_some())
        {
            *entry = hit;
        }
    };
    let mut extra_notes = Vec::new();

    // Ruby: match indexed RSpec `describe`/`context`/`it` blocks by subject
    // class/module and follow `require_relative`/`require` links.
    let ruby_files: Vec<String> = all_ids
        .iter()
        .map(|&id| definitions[id].location.file.clone())
        .filter(|file| file.ends_with(".rb"))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if !ruby_files.is_empty() {
        for (test_id, hops, inferred) in rspec_subject_tests(index, &all_ids) {
            keep(test_id, hops, inferred);
        }
        let ruby_graph = RubyGraph::build(workspace, index);
        let reach = ruby_graph.reach_from(&ruby_files);
        for (spec_file, (line, why)) in reach.test_files {
            insert_file(
                spec_file.clone(),
                TestFileHit {
                    hops: 1,
                    possible: false,
                    at: Location {
                        file: spec_file,
                        start: line,
                        end: line,
                    },
                    note: Some(why),
                },
            );
        }
        let touches_shared_ruby = ruby_files.iter().any(|file| is_shared_ruby_helper(file));
        if touches_shared_ruby {
            extra_notes.push(
                "Shared Ruby test helper or config touched; run the full RSpec suite.".into(),
            );
        } else if !ruby_graph.dynamic_loaders.is_empty()
            && ruby_files.iter().any(|file| !file.ends_with("_spec.rb"))
        {
            let (loader_file, line, kind) = &ruby_graph.dynamic_loaders[0];
            extra_notes.push(format!(
                "Dynamic Ruby loading ({kind} at {loader_file}:{line}); additional specs may be affected."
            ));
        }
    }

    // JS/TS: follow barrel re-exports, static dynamic imports / require(), and
    // widen conservatively to full test files when reached through a barrel or
    // dynamic import (or note full-suite impact for shared configs / computed imports).
    let js_files: Vec<String> = all_ids
        .iter()
        .map(|&id| definitions[id].location.file.clone())
        .filter(|file| is_js_file(file))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut widened_files = BTreeSet::<String>::new();
    if !js_files.is_empty() {
        let js_graph = JsGraph::build(workspace, index);
        let reach = js_graph.reach_from(index, &js_files);
        for (test_file, (line, why, widen_whole_file)) in reach.test_files {
            if widen_whole_file {
                widened_files.insert(test_file.clone());
            }
            insert_file(
                test_file.clone(),
                TestFileHit {
                    hops: 1,
                    possible: false,
                    at: Location {
                        file: test_file,
                        start: line,
                        end: line,
                    },
                    note: Some(why),
                },
            );
        }
        if !reach.alias_barrels.is_empty() {
            let (barrel, importer, line, spec) = &reach.alias_barrels[0];
            extra_notes.push(format!(
                "Barrel re-export {barrel} is imported via path alias '{spec}' at {importer}:{line}; run the full suite."
            ));
        }
        if !reach.unresolved_barrels.is_empty() {
            let (barrel, line, spec) = &reach.unresolved_barrels[0];
            extra_notes.push(format!(
                "Barrel {barrel}:{line} has unresolved re-export '{spec}'; run the full suite."
            ));
        }
        if !reach.computed_hits.is_empty() {
            let (importer, line, kind, target) = &reach.computed_hits[0];
            extra_notes.push(format!(
                "Computed {kind} at {importer}:{line} may load {target}; run the full suite."
            ));
        }
        for file in &js_files {
            if is_shared_js_config_or_setup(file) {
                extra_notes.push(format!(
                    "Shared JS/TS test config or setup file {file} touched; run the full suite."
                ));
            } else if let Some(config) = config_reference(workspace, index, "", file) {
                extra_notes.push(format!(
                    "{file} is referenced by runner config {config}; run the full suite."
                ));
            }
        }
    }

    // When a JS/TS test file is conservatively widened (barrel re-export or
    // dynamic import), report the whole test file instead of a partial subset
    // of its `it`/`describe` blocks.
    found.retain(|&id, _| !widened_files.contains(&definitions[id].location.file));

    let mut tests: Vec<_> = found
        .into_iter()
        .map(|(id, (possible, hops))| (id, hops, possible))
        .collect();
    tests.sort_by_key(|&(id, hops, possible)| (possible, hops, &definitions[id].location, id));
    let covered: BTreeSet<&str> = tests
        .iter()
        .map(|&(id, _, _)| definitions[id].location.file.as_str())
        .collect();
    let seed_files = seeds
        .iter()
        .map(|&id| (&definitions[id].location, 0, possible.contains(&id)));
    let reached = walk.levels.iter().enumerate().flat_map(|(offset, level)| {
        level
            .iter()
            .map(move |reach| (&reach.edge.location, offset + 1, reach.possible))
    });
    for (location, hops, possible) in seed_files.chain(reached) {
        let file = location.file.as_str();
        if is_test_file(file) && !covered.contains(file) {
            insert_file(
                file.to_owned(),
                TestFileHit {
                    hops,
                    possible,
                    at: location.clone(),
                    note: None,
                },
            );
        }
    }
    extra_notes.sort();
    extra_notes.dedup();
    (tests, files, extra_notes)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Runner {
    Cargo,
    Pytest,
    Js,
    Rspec,
    Go,
}

/// Commands that run just the given tests, where the file layout makes the
/// runner and test name unambiguous. Returns the commands and how many
/// matching tests did not fit in them.
fn commands(
    workspace: &Workspace,
    index: &Index,
    tests: &[(usize, usize, bool)],
    files: &TestFiles,
) -> (Vec<String>, usize) {
    let mut groups = BTreeMap::<(Runner, String), Vec<String>>::new();
    let mut push = |runner: Runner, prefix: String, filter: String| {
        let filters = groups.entry((runner, prefix)).or_default();
        if !filters.contains(&filter) {
            filters.push(filter);
        }
    };
    for &(id, _, _) in tests {
        let definition = &index.definitions[id];
        let selected = cargo(index, definition)
            .map(|(prefix, filter)| (Runner::Cargo, prefix, filter))
            .or_else(|| pytest(definition).map(|filter| (Runner::Pytest, "pytest".into(), filter)))
            .or_else(|| go(index, definition).map(|(prefix, filter)| (Runner::Go, prefix, filter)))
            .or_else(|| {
                js_command(workspace, index, &definition.location.file)
                    .map(|(prefix, filter)| (Runner::Js, prefix, filter))
            })
            .or_else(|| {
                rspec_command(index, &definition.location.file)
                    .map(|(prefix, filter)| (Runner::Rspec, prefix, filter))
            });
        if let Some((runner, prefix, filter)) = selected {
            push(runner, prefix, filter);
        }
    }
    for file in files.keys() {
        if let Some((prefix, filter)) = js_command(workspace, index, file) {
            push(Runner::Js, prefix, filter);
        } else if let Some((prefix, filter)) = rspec_command(index, file) {
            push(Runner::Rspec, prefix, filter);
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
                Runner::Pytest | Runner::Js | Runner::Rspec => {
                    format!("{prefix} {}", filters.join(" "))
                }
                Runner::Go if filters.len() == 1 => format!("{prefix} -run '^{}$'", filters[0]),
                Runner::Go => format!("{prefix} -run '^({})$'", filters.join("|")),
            }
        })
        .collect();
    (commands, left)
}

/// Suggests the unambiguous JS/TS runner command for a test file by inspecting
/// the nearest `package.json` or `bunfig.toml` above `file`.
fn js_command(workspace: &Workspace, index: &Index, file: &str) -> Option<(String, String)> {
    if !is_js_file(file) || !is_test_file(file) {
        return None;
    }
    let path = Path::new(file);
    for directory in path.ancestors().skip(1) {
        let dir = directory.to_str()?;
        let rel_path = |name: &str| {
            if dir.is_empty() {
                name.to_owned()
            } else {
                format!("{dir}/{name}")
            }
        };
        let pkg_rel = rel_path("package.json");
        if index.files.contains_key(&pkg_rel)
            && let Ok(text) = fs::read_to_string(workspace.root.join(&pkg_rel))
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
        {
            let listed = |tool: &str| {
                ["dependencies", "devDependencies"]
                    .iter()
                    .any(|key| json[key].get(tool).is_some())
            };
            let configured = |prefix: &str| {
                index.files.keys().any(|other| {
                    let parent = other.rsplit_once('/').map_or("", |(p, _)| p);
                    parent == dir
                        && other
                            .rsplit('/')
                            .next()
                            .is_some_and(|name| name.starts_with(prefix))
                })
            };
            let jest = listed("jest") || json.get("jest").is_some() || configured("jest.config.");
            let vitest = listed("vitest") || configured("vitest.config.");
            let script = json["scripts"]["test"].as_str().unwrap_or_default();
            let prefix = match (jest, vitest) {
                (true, true) => return None,
                (false, true) => "npx vitest run",
                (true, false) => "npx jest",
                (false, false) => {
                    if script.starts_with("bun test") {
                        "bun test"
                    } else if script.starts_with("node ") && script.contains("--test") {
                        "node --test"
                    } else if script.starts_with("playwright test") {
                        "npx playwright test"
                    } else if index.files.contains_key(&rel_path("bunfig.toml")) {
                        "bun test"
                    } else {
                        return None;
                    }
                }
            };
            return Some((prefix.to_owned(), quote(file)));
        }
        let bunfig_rel = rel_path("bunfig.toml");
        if index.files.contains_key(&bunfig_rel) {
            return Some(("bun test".to_owned(), quote(file)));
        }
    }
    None
}

/// Suggests `bundle exec rspec` or `rspec` for an RSpec file (`*_spec.rb`).
fn rspec_command(index: &Index, file: &str) -> Option<(String, String)> {
    if !file.ends_with("_spec.rb") {
        return None;
    }
    let path = Path::new(file);
    for directory in path.ancestors().skip(1) {
        let dir = directory.to_str()?;
        let rel = |name: &str| {
            if dir.is_empty() {
                name.to_owned()
            } else {
                format!("{dir}/{name}")
            }
        };
        if index.files.contains_key(&rel("Gemfile")) {
            return Some(("bundle exec rspec".to_owned(), quote(file)));
        }
        if index.files.contains_key(&rel(".rspec")) {
            return Some(("rspec".to_owned(), quote(file)));
        }
    }
    Some(("rspec".to_owned(), quote(file)))
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

// ---------------------------------------------------------------------------
// JS/TS barrel re-exports, dynamic imports, and Ruby RSpec / require graphs
// ---------------------------------------------------------------------------

/// Whether a JS/TS file is a shared test setup or runner configuration file.
pub(crate) fn is_shared_js_config_or_setup(file: &str) -> bool {
    let base = file.rsplit('/').next().unwrap_or(file);
    let stem = [
        ".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs", ".json", ".toml",
    ]
    .iter()
    .find_map(|ext| base.strip_suffix(ext))
    .unwrap_or(base);
    base.starts_with("jest.config.")
        || base.starts_with("vitest.config.")
        || base.starts_with("vite.config.")
        || base.starts_with("playwright.config.")
        || base.starts_with("jest.setup.")
        || base.starts_with("vitest.setup.")
        || base == "bunfig.toml"
        || matches!(
            stem,
            "setupTests"
                | "setup-tests"
                | "test-setup"
                | "testSetup"
                | "global-setup"
                | "globalSetup"
                | "global-teardown"
                | "globalTeardown"
        )
}

/// Whether a Ruby file is a shared RSpec/test helper or configuration file.
pub(crate) fn is_shared_ruby_helper(file: &str) -> bool {
    let base = file.rsplit('/').next().unwrap_or(file);
    matches!(
        base,
        "spec_helper.rb"
            | "rails_helper.rb"
            | "test_helper.rb"
            | ".rspec"
            | "Gemfile"
            | "Gemfile.lock"
    ) || file.starts_with("spec/support/")
        || file.contains("/spec/support/")
        || file.starts_with("test/support/")
        || file.contains("/test/support/")
}

#[derive(Clone, Debug)]
pub(crate) struct ModuleEdge {
    pub from: String,
    pub line: usize,
    pub kind: &'static str,
}

#[derive(Clone, Debug)]
pub(crate) struct ComputedImport {
    pub file: String,
    pub line: usize,
    pub prefix: Option<String>,
    pub kind: &'static str,
}

impl ComputedImport {
    pub(crate) fn matches(&self, changed_file: &str, runner_root: &str) -> bool {
        if !crate::runners::under(&self.file, runner_root)
            || !crate::runners::under(changed_file, runner_root)
        {
            return false;
        }
        match &self.prefix {
            Some(prefix) => changed_file.starts_with(prefix.as_str()),
            None => true,
        }
    }
}

/// Augments the index's static JS/TS `import` graph with barrel re-exports
/// (`export ... from`), CommonJS `require(...)`, dynamic `import(...)`, test
/// mocks (`vi.mock`/`jest.mock`), and non-relative alias imports.
#[derive(Default)]
pub(crate) struct JsGraph {
    /// Files that re-export symbols from another file (barrel files).
    pub barrels: BTreeSet<String>,
    /// `target_file -> [re-exporting barrel edges]`.
    pub reexports: BTreeMap<String, Vec<ModuleEdge>>,
    /// `barrel_file -> [(line, non-relative specifier)]`.
    pub unresolved_reexports: BTreeMap<String, Vec<(usize, String)>>,
    /// `target_file -> [static dynamic import / require / mock edges]`.
    pub dynamic_imports: BTreeMap<String, Vec<ModuleEdge>>,
    /// Non-literal `import(expr)` / `require(expr)` sites that cannot be statically resolved.
    pub computed_dynamic: Vec<ComputedImport>,
    /// Non-relative path-alias imports (`@/...`, `~/...`, `#...`, `src/...`).
    pub alias_imports: Vec<(String, usize, String)>,
}

#[derive(Default)]
pub(crate) struct JsReach {
    /// `test_file -> (line, why, widen_whole_file)`.
    pub test_files: BTreeMap<String, (usize, String, bool)>,
    /// All non-test files reached via barrels or dynamic imports.
    pub reached_files: BTreeSet<String>,
    /// Barrel files touched (either changed directly or traversed on the way).
    pub touched_barrels: BTreeSet<String>,
    /// `(barrel, importer_file, line, specifier)` where a touched barrel may be
    /// imported via an unresolved path alias.
    pub alias_barrels: Vec<(String, String, usize, String)>,
    /// `(barrel, line, specifier)` for unresolved re-exports in a touched barrel.
    pub unresolved_barrels: Vec<(String, usize, String)>,
    /// `(importer_file, line, kind, target_file)` for computed dynamic imports
    /// that may load a changed or reached file.
    pub computed_hits: Vec<(String, usize, &'static str, String)>,
}

impl JsGraph {
    pub(crate) fn build(workspace: &Workspace, index: &Index) -> Self {
        let mut graph = Self::default();
        for file in index.files.keys() {
            if !is_js_file(file) {
                continue;
            }
            let Ok(path) = workspace.source_path(file) else {
                continue;
            };
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            graph.scan_file(index, file, &text);
        }
        graph
    }

    fn scan_file(&mut self, index: &Index, file: &str, text: &str) {
        let mut in_export_clause = false;
        let mut in_block_comment = false;
        for (offset, raw_line) in text.lines().enumerate() {
            let line_no = offset + 1;
            let trimmed = raw_line.trim();
            if in_block_comment {
                if let Some((_, rest)) = trimmed.split_once("*/") {
                    in_block_comment = false;
                    if rest.trim().is_empty() {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            if trimmed.starts_with("/*") {
                if !trimmed.contains("*/") {
                    in_block_comment = true;
                }
                continue;
            }
            if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('*') {
                continue;
            }
            let code = strip_js_line_comment(raw_line);
            let code_trimmed = code.trim();

            // 1. Barrel re-exports: `export ... from '...'` (single-line or multi-line).
            if code_trimmed.starts_with("export ")
                && !code_trimmed.starts_with("export default ")
                && !code_trimmed.starts_with("export function ")
                && !code_trimmed.starts_with("export async function ")
                && !code_trimmed.starts_with("export class ")
                && !code_trimmed.starts_with("export const ")
                && !code_trimmed.starts_with("export let ")
                && !code_trimmed.starts_with("export var ")
                && !code_trimmed.starts_with("export interface ")
                && !code_trimmed.starts_with("export enum ")
            {
                if let Some(spec) = extract_from_specifier(code_trimmed) {
                    self.record_reexport(index, file, line_no, &spec);
                    in_export_clause = false;
                } else if code_trimmed.contains('{') && !code_trimmed.contains('}') {
                    in_export_clause = true;
                }
            } else if in_export_clause {
                if let Some(spec) = extract_from_specifier(code_trimmed) {
                    self.record_reexport(index, file, line_no, &spec);
                    in_export_clause = false;
                } else if code_trimmed.contains('}') || code_trimmed.contains(';') {
                    in_export_clause = false;
                }
            }

            // 2. Static path-alias imports (`import ... from '@/...'`).
            if (code_trimmed.starts_with("import ") || code_trimmed.starts_with("import{"))
                && let Some(spec) = extract_from_specifier(code_trimmed)
                && is_alias_specifier(&spec)
            {
                self.alias_imports.push((file.to_owned(), line_no, spec));
            }

            // 3. `import(...)`, `require(...)`, `require.resolve(...)`, `vi.mock(...)`,
            //    `jest.mock(...)`, `mock.module(...)`.
            self.scan_calls(index, file, line_no, code);
        }
    }

    fn record_reexport(&mut self, index: &Index, file: &str, line: usize, spec: &str) {
        self.barrels.insert(file.to_owned());
        if spec.starts_with('.') {
            if let Some(target) = resolve_js_specifier(index, file, spec) {
                self.reexports.entry(target).or_default().push(ModuleEdge {
                    from: file.to_owned(),
                    line,
                    kind: "barrel re-export",
                });
            }
        } else {
            if is_alias_specifier(spec) {
                self.alias_imports
                    .push((file.to_owned(), line, spec.to_owned()));
            }
            self.unresolved_reexports
                .entry(file.to_owned())
                .or_default()
                .push((line, spec.to_owned()));
        }
    }

    fn scan_calls(&mut self, index: &Index, file: &str, line: usize, code: &str) {
        let cjs_reexport = code.contains("module.exports")
            || code.contains("exports.")
            || code.contains("Object.assign(exports");
        for (needle, kind, check_computed) in [
            ("import(", "dynamic import", true),
            ("require(", "require", true),
            ("require.resolve(", "require", false),
            ("vi.mock(", "mock", false),
            ("jest.mock(", "mock", false),
            ("mock.module(", "mock", false),
        ] {
            for (at, _) in code.match_indices(needle) {
                if at > 0 {
                    let prev = code[..at].chars().next_back().unwrap_or(' ');
                    if prev.is_alphanumeric() || prev == '_' || prev == '$' || prev == '.' {
                        continue;
                    }
                }
                let rest = code[at + needle.len()..].trim_start();
                let arg = extract_first_call_arg(rest);
                let arg = arg.trim();
                if arg.is_empty() {
                    continue;
                }
                if let Some(spec) = unquote_static_js_string(arg) {
                    if spec.starts_with('.') {
                        if let Some(target) = resolve_js_specifier(index, file, spec) {
                            if cjs_reexport && kind == "require" {
                                self.barrels.insert(file.to_owned());
                                self.reexports.entry(target).or_default().push(ModuleEdge {
                                    from: file.to_owned(),
                                    line,
                                    kind: "barrel re-export",
                                });
                            } else {
                                self.dynamic_imports
                                    .entry(target)
                                    .or_default()
                                    .push(ModuleEdge {
                                        from: file.to_owned(),
                                        line,
                                        kind,
                                    });
                            }
                        }
                    } else if is_alias_specifier(spec) {
                        self.alias_imports
                            .push((file.to_owned(), line, spec.to_owned()));
                    }
                } else if check_computed {
                    // Computed expression or template string with `${...}`.
                    let prefix = template_static_prefix(file, arg);
                    // Ignore obviously non-relative template strings like `` `node:${mod}` ``.
                    if let Some(body) = arg.strip_prefix('`')
                        && !body.starts_with('.')
                        && !body.starts_with("${")
                    {
                        continue;
                    }
                    self.computed_dynamic.push(ComputedImport {
                        file: file.to_owned(),
                        line,
                        prefix,
                        kind,
                    });
                }
            }
        }
    }

    /// Checks whether any non-relative alias import (such as `@/billing`) could
    /// target `barrel_file`.
    pub(crate) fn barrel_alias_importer(
        &self,
        barrel_file: &str,
    ) -> Option<(String, usize, String)> {
        let dir = barrel_file.rsplit_once('/').map_or("", |(d, _)| d);
        let base = barrel_file.rsplit('/').next().unwrap_or(barrel_file);
        let stem = base.split('.').next().unwrap_or(base);
        let mut keys = Vec::new();
        if stem == "index" && !dir.is_empty() {
            keys.push(dir.to_owned());
            if let Some((_, tail)) = dir.split_once('/') {
                keys.push(tail.to_owned());
            }
            if let Some(last) = dir.rsplit('/').next() {
                keys.push(last.to_owned());
            }
        } else if !stem.is_empty() {
            let no_ext = if dir.is_empty() {
                stem.to_owned()
            } else {
                format!("{dir}/{stem}")
            };
            keys.push(no_ext.clone());
            if let Some((_, tail)) = no_ext.split_once('/') {
                keys.push(tail.to_owned());
            }
        }
        for (importer, line, spec) in &self.alias_imports {
            if importer == barrel_file {
                continue;
            }
            let normalized = spec
                .trim_start_matches("@/")
                .trim_start_matches("~/")
                .trim_start_matches('#')
                .trim_end_matches('/');
            if keys.iter().any(|key| {
                normalized == key
                    || normalized.ends_with(&format!("/{key}"))
                    || key.ends_with(&format!("/{normalized}"))
            }) {
                return Some((importer.clone(), *line, spec.clone()));
            }
        }
        None
    }

    /// Transitively follows barrel re-exports, static `require`/`import()`/mock
    /// edges, and static `index` file imports starting from `starts`.
    pub(crate) fn reach_from(&self, index: &Index, starts: &[String]) -> JsReach {
        let mut reach = JsReach::default();
        // Map file path -> file-node definition ID for fast static import traversal.
        let file_nodes: BTreeMap<&str, usize> = index
            .definitions
            .iter()
            .enumerate()
            .filter(|(_, def)| def.kind == "file" && is_js_file(&def.location.file))
            .map(|(id, def)| (def.location.file.as_str(), id))
            .collect();

        // Queue entries: (file, via_barrel_or_dynamic, first_line, why_chain).
        let mut visited = BTreeSet::<String>::new();
        let mut queue = std::collections::VecDeque::<(String, bool, usize, String)>::new();
        for start in starts {
            if visited.insert(start.clone()) {
                let is_barrel = self.barrels.contains(start);
                if is_barrel {
                    reach.touched_barrels.insert(start.clone());
                }
                queue.push_back((start.clone(), is_barrel, 1, String::new()));
            }
        }

        while let Some((current, widened, _, prefix_why)) = queue.pop_front() {
            for computed in &self.computed_dynamic {
                if computed.file != current && computed.matches(&current, "") {
                    reach.computed_hits.push((
                        computed.file.clone(),
                        computed.line,
                        computed.kind,
                        current.clone(),
                    ));
                }
            }
            // 1. Barrel re-exports of `current`.
            if let Some(edges) = self.reexports.get(&current) {
                for edge in edges {
                    reach.touched_barrels.insert(edge.from.clone());
                    let step_why =
                        format!("{} at {}:{} → {current}", edge.kind, edge.from, edge.line);
                    let why = join_why(&prefix_why, &step_why);
                    if is_test_file(&edge.from) {
                        reach
                            .test_files
                            .entry(edge.from.clone())
                            .or_insert((edge.line, why, true));
                    } else if visited.insert(edge.from.clone()) {
                        reach.reached_files.insert(edge.from.clone());
                        queue.push_back((edge.from.clone(), true, edge.line, why));
                    }
                }
            }
            // 2. Static `import('./...')`, `require('./...')`, `vi.mock('./...')`.
            if let Some(edges) = self.dynamic_imports.get(&current) {
                for edge in edges {
                    let step_why =
                        format!("{} at {}:{} → {current}", edge.kind, edge.from, edge.line);
                    let why = join_why(&prefix_why, &step_why);
                    if is_test_file(&edge.from) {
                        reach
                            .test_files
                            .entry(edge.from.clone())
                            .or_insert((edge.line, why, true));
                    } else if visited.insert(edge.from.clone()) {
                        reach.reached_files.insert(edge.from.clone());
                        queue.push_back((edge.from.clone(), true, edge.line, why));
                    }
                }
            }
            // 3. Static `import` edges already indexed in `index` (needed once
            //    we have stepped through a barrel or dynamic import!).
            if widened && let Some(&node_id) = file_nodes.get(current.as_str()) {
                for edge in index.incoming(node_id) {
                    if edge.kind != Kind::Import {
                        continue;
                    }
                    let importer = &edge.location.file;
                    if importer == &current || !is_js_file(importer) {
                        continue;
                    }
                    let step_why =
                        format!("import at {}:{} → {current}", importer, edge.location.start);
                    let why = join_why(&prefix_why, &step_why);
                    if is_test_file(importer) {
                        reach.test_files.entry(importer.clone()).or_insert((
                            edge.location.start,
                            why,
                            true,
                        ));
                    } else if visited.insert(importer.clone()) {
                        reach.reached_files.insert(importer.clone());
                        queue.push_back((importer.clone(), true, edge.location.start, why));
                    }
                }
            }
        }

        for barrel in &reach.touched_barrels {
            if let Some((importer, line, spec)) = self.barrel_alias_importer(barrel) {
                reach
                    .alias_barrels
                    .push((barrel.clone(), importer, line, spec));
            }
            if let Some(list) = self.unresolved_reexports.get(barrel) {
                for (line, spec) in list {
                    reach
                        .unresolved_barrels
                        .push((barrel.clone(), *line, spec.clone()));
                }
            }
        }
        reach
    }
}

fn join_why(prefix: &str, next: &str) -> String {
    if prefix.is_empty() {
        next.to_owned()
    } else {
        format!("{next} · {prefix}")
    }
}

fn strip_js_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        match (quote, bytes[i]) {
            (Some(_), b'\\') => i += 1,
            (Some(q), b) if b == q => quote = None,
            (None, b'"' | b'\'' | b'`') => quote = Some(bytes[i]),
            (None, b'/') if bytes.get(i + 1) == Some(&b'/') => return &line[..i],
            _ => {}
        }
        i += 1;
    }
    line
}

fn extract_from_specifier(line: &str) -> Option<String> {
    let idx = line.rfind("from")?;
    let before = line[..idx].chars().next_back();
    if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$') {
        return None;
    }
    let after = &line[idx + 4..];
    if !after.starts_with(|c: char| c.is_whitespace() || c == '\'' || c == '"') {
        return None;
    }
    let after = after.trim_start();
    let quote = after.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let rest = &after[1..];
    let end = rest.find(quote)?;
    let spec = &rest[..end];
    (!spec.is_empty()).then(|| spec.to_owned())
}

fn extract_first_call_arg(rest: &str) -> &str {
    let bytes = rest.as_bytes();
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        match (quote, bytes[i]) {
            (Some(_), b'\\') => i += 1,
            (Some(q), b) if b == q => quote = None,
            (None, b'"' | b'\'' | b'`') => quote = Some(bytes[i]),
            (None, b'(' | b'[' | b'{') => depth += 1,
            (None, b')' | b']' | b'}') => {
                if depth == 0 {
                    return &rest[..i];
                }
                depth -= 1;
            }
            (None, b',') if depth == 0 => return &rest[..i],
            _ => {}
        }
        i += 1;
    }
    rest
}

fn unquote_static_js_string(arg: &str) -> Option<&str> {
    for q in ['"', '\''] {
        if let Some(body) = arg.strip_prefix(q).and_then(|s| s.strip_suffix(q))
            && !body.contains(q)
        {
            return Some(body);
        }
    }
    if let Some(body) = arg.strip_prefix('`').and_then(|s| s.strip_suffix('`'))
        && !body.contains("${")
        && !body.contains('`')
    {
        return Some(body);
    }
    None
}

fn is_alias_specifier(spec: &str) -> bool {
    spec.starts_with("@/")
        || spec.starts_with("~/")
        || spec.starts_with('#')
        || spec.starts_with("src/")
}

fn template_static_prefix(from_file: &str, arg: &str) -> Option<String> {
    let body = arg.strip_prefix('`')?;
    let before_expr = body.split("${").next()?;
    if !before_expr.starts_with('.') {
        return None;
    }
    let dir_part = before_expr.rsplit_once('/').map_or(before_expr, |(d, _)| d);
    let resolved = resolve_relative_path(from_file, dir_part)?;
    Some(if resolved.is_empty() {
        String::new()
    } else {
        format!("{resolved}/")
    })
}

fn resolve_relative_path(from: &str, literal: &str) -> Option<String> {
    if literal.is_empty() || literal.starts_with('/') || literal.contains("://") {
        return None;
    }
    let mut parts: Vec<&str> = match from.rsplit_once('/') {
        Some((directory, _)) => directory.split('/').collect(),
        None => Vec::new(),
    };
    for part in literal.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

fn resolve_js_specifier(index: &Index, from: &str, spec: &str) -> Option<String> {
    let base = resolve_relative_path(from, spec)?;
    if !base.is_empty() && index.files.contains_key(&base) {
        return Some(base);
    }
    let extensions = ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];
    if base.is_empty() {
        for ext in extensions {
            let candidate = format!("index.{ext}");
            if index.files.contains_key(&candidate) {
                return Some(candidate);
            }
        }
        return None;
    }
    let path = Path::new(&base);
    let ext = path.extension().and_then(|e| e.to_str());
    for candidate_ext in extensions {
        if ext.is_none() {
            let file = format!("{base}.{candidate_ext}");
            if index.files.contains_key(&file) {
                return Some(file);
            }
            let barrel = format!("{base}/index.{candidate_ext}");
            if index.files.contains_key(&barrel) {
                return Some(barrel);
            }
        } else if matches!(ext, Some("js" | "jsx" | "mjs" | "cjs"))
            && let Some(replaced) = path.with_extension(candidate_ext).to_str()
            && index.files.contains_key(replaced)
        {
            return Some(replaced.to_owned());
        }
    }
    None
}

/// Checks if a runner configuration file (`jest.config.*`, `vitest.config.*`,
/// `vite.config.*`, `playwright.config.*`, `bunfig.toml`) under `runner_root`
/// references `changed_file` (e.g. via `setupFiles`, `setupFilesAfterEnv`,
/// `preload`, or `globalSetup`).
pub(crate) fn config_reference(
    workspace: &Workspace,
    index: &Index,
    runner_root: &str,
    changed_file: &str,
) -> Option<String> {
    if is_shared_js_config_or_setup(changed_file) {
        return None;
    }
    let changed_base = changed_file.rsplit('/').next().unwrap_or(changed_file);
    let changed_stem = changed_base.split('.').next().unwrap_or(changed_base);
    for config in index.files.keys() {
        if !crate::runners::under(config, runner_root) {
            continue;
        }
        let name = config.rsplit('/').next().unwrap_or(config);
        let is_config = name.starts_with("jest.config.")
            || name.starts_with("vitest.config.")
            || name.starts_with("vite.config.")
            || name.starts_with("playwright.config.")
            || name == "bunfig.toml";
        if !is_config || config == changed_file {
            continue;
        }
        let Ok(path) = workspace.source_path(config) else {
            continue;
        };
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            for quote in ['"', '\''] {
                let mut parts = line.split(quote);
                parts.next();
                while let Some(lit) = parts.next() {
                    if lit.contains(changed_stem)
                        && (lit == changed_file
                            || lit.trim_start_matches("./") == changed_file
                            || resolve_js_specifier(index, config, lit).as_deref()
                                == Some(changed_file)
                            || resolve_relative_path(config, lit).as_deref() == Some(changed_file)
                            || lit.ends_with(&format!("/{changed_base}")))
                    {
                        return Some(config.clone());
                    }
                    parts.next();
                }
            }
        }
    }
    None
}

/// Ruby `require_relative`/`require` graph and dynamic-loader detector.
#[derive(Default)]
pub(crate) struct RubyGraph {
    /// `target_rb_file -> [requiring rb edges]`.
    pub requires: BTreeMap<String, Vec<ModuleEdge>>,
    /// Dynamic Ruby loaders (`Dir.glob` + `require`, `Zeitwerk`, `autoload`, `require(expr)`).
    pub dynamic_loaders: Vec<(String, usize, &'static str)>,
}

#[derive(Default)]
pub(crate) struct RubyReach {
    /// `spec_file -> (line, why)`.
    pub test_files: BTreeMap<String, (usize, String)>,
    /// Non-spec Ruby files reached transitively via `require_relative`/`require`.
    pub reached_files: BTreeSet<String>,
}

impl RubyGraph {
    pub(crate) fn build(workspace: &Workspace, index: &Index) -> Self {
        let mut graph = Self::default();
        let rb_files: Vec<String> = index
            .files
            .keys()
            .filter(|file| file.ends_with(".rb"))
            .cloned()
            .collect();
        for file in &rb_files {
            let Ok(path) = workspace.source_path(file) else {
                continue;
            };
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            for (offset, raw_line) in text.lines().enumerate() {
                let line_no = offset + 1;
                let trimmed = raw_line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if trimmed.contains("Zeitwerk") {
                    graph
                        .dynamic_loaders
                        .push((file.clone(), line_no, "Zeitwerk autoloader"));
                } else if trimmed.starts_with("autoload ") || trimmed.starts_with("autoload(") {
                    graph
                        .dynamic_loaders
                        .push((file.clone(), line_no, "Ruby autoload"));
                } else if (trimmed.contains("Dir.glob") || trimmed.contains("Dir["))
                    && trimmed.contains("require")
                {
                    graph
                        .dynamic_loaders
                        .push((file.clone(), line_no, "dynamic Dir glob require"));
                }
                if let Some(rest) = strip_ruby_keyword(trimmed, "require_relative") {
                    if let Some(lit) = extract_ruby_string(rest) {
                        if let Some(target) = resolve_ruby_relative(index, file, lit) {
                            graph.requires.entry(target).or_default().push(ModuleEdge {
                                from: file.clone(),
                                line: line_no,
                                kind: "require_relative",
                            });
                        }
                    } else if !rest.is_empty() {
                        graph.dynamic_loaders.push((
                            file.clone(),
                            line_no,
                            "dynamic require_relative",
                        ));
                    }
                } else if let Some(rest) = strip_ruby_keyword(trimmed, "require") {
                    if let Some(lit) = extract_ruby_string(rest) {
                        if let Some(target) = resolve_ruby_require(index, &rb_files, lit) {
                            graph.requires.entry(target).or_default().push(ModuleEdge {
                                from: file.clone(),
                                line: line_no,
                                kind: "require",
                            });
                        }
                    } else if !rest.is_empty() && !rest.starts_with('=') {
                        graph
                            .dynamic_loaders
                            .push((file.clone(), line_no, "dynamic require"));
                    }
                }
            }
        }
        graph
    }

    pub(crate) fn reach_from(&self, starts: &[String]) -> RubyReach {
        let mut reach = RubyReach::default();
        let mut visited = BTreeSet::<String>::new();
        let mut queue = std::collections::VecDeque::<(String, String)>::new();
        for start in starts {
            if visited.insert(start.clone()) {
                queue.push_back((start.clone(), String::new()));
            }
        }
        while let Some((current, prefix_why)) = queue.pop_front() {
            if let Some(edges) = self.requires.get(&current) {
                for edge in edges {
                    let step_why =
                        format!("{} at {}:{} → {current}", edge.kind, edge.from, edge.line);
                    let why = join_why(&prefix_why, &step_why);
                    if edge.from.ends_with("_spec.rb") || is_test_file(&edge.from) {
                        reach
                            .test_files
                            .entry(edge.from.clone())
                            .or_insert((edge.line, why));
                    } else if visited.insert(edge.from.clone()) {
                        reach.reached_files.insert(edge.from.clone());
                        queue.push_back((edge.from.clone(), why));
                    }
                }
            }
        }
        reach
    }
}

fn strip_ruby_keyword<'a>(trimmed: &'a str, kw: &str) -> Option<&'a str> {
    let rest = trimmed.strip_prefix(kw)?;
    if rest.starts_with(char::is_whitespace) || rest.starts_with('(') {
        Some(rest.trim_start().trim_start_matches('(').trim_start())
    } else {
        None
    }
}

fn extract_ruby_string(rest: &str) -> Option<&str> {
    for q in ['\'', '"'] {
        if let Some(after) = rest.strip_prefix(q)
            && let Some(end) = after.find(q)
        {
            let body = &after[..end];
            if !body.is_empty() && !(q == '"' && body.contains("#{")) {
                return Some(body);
            }
        }
    }
    None
}

fn resolve_ruby_relative(index: &Index, from: &str, lit: &str) -> Option<String> {
    let base = resolve_relative_path(from, lit)?;
    if index.files.contains_key(&base) {
        return Some(base);
    }
    let with_rb = format!("{base}.rb");
    index.files.contains_key(&with_rb).then_some(with_rb)
}

fn resolve_ruby_require(index: &Index, rb_files: &[String], lit: &str) -> Option<String> {
    let normalized = lit.strip_suffix(".rb").unwrap_or(lit);
    let direct = format!("{normalized}.rb");
    if index.files.contains_key(&direct) {
        return Some(direct);
    }
    let in_lib = format!("lib/{normalized}.rb");
    if index.files.contains_key(&in_lib) {
        return Some(in_lib);
    }
    let suffix = format!("/{normalized}.rb");
    let mut matches = rb_files.iter().filter(|file| file.ends_with(&suffix));
    let first = matches.next()?;
    matches.next().is_none().then(|| first.clone())
}

/// Leverages Wave 1's indexed Ruby RSpec `describe`/`context`/`it`/`specify`
/// test blocks: matches changed/reached Ruby classes, modules, and methods
/// against `RSpec.describe MyApp::OrderService` and nested `describe '#total'`
/// blocks even when examples call `described_class` without repeating the class
/// name.
pub(crate) fn rspec_subject_tests(index: &Index, ids: &[usize]) -> Vec<(usize, usize, bool)> {
    // Collect target (full_subject, short_subject, optional_method_name) from Ruby definitions.
    let mut subjects = BTreeSet::<String>::new();
    let mut short_subjects = BTreeSet::<String>::new();
    let mut methods_by_subject = BTreeMap::<String, BTreeSet<String>>::new();
    for &id in ids {
        let def = &index.definitions[id];
        if !def.location.file.ends_with(".rb")
            || def.test
            || matches!(def.kind.as_str(), "file" | "module" if def.kind == "file")
        {
            continue;
        }
        let rel_name = name(def);
        if matches!(def.kind.as_str(), "class" | "module") {
            subjects.insert(rel_name.to_owned());
            short_subjects.insert(short_name(rel_name).to_owned());
        } else if let Some((owner, method)) = rel_name.rsplit_once("::") {
            subjects.insert(owner.to_owned());
            short_subjects.insert(short_name(owner).to_owned());
            methods_by_subject
                .entry(owner.to_owned())
                .or_default()
                .insert(method.to_owned());
            methods_by_subject
                .entry(short_name(owner).to_owned())
                .or_default()
                .insert(method.to_owned());
        }
    }
    if subjects.is_empty() {
        return Vec::new();
    }
    // Check which short class names are unambiguous across indexed Ruby classes/modules.
    let mut short_counts = BTreeMap::<&str, usize>::new();
    for def in &index.definitions {
        if def.location.file.ends_with(".rb") && matches!(def.kind.as_str(), "class" | "module") {
            *short_counts.entry(short_name(name(def))).or_default() += 1;
        }
    }
    let unique_shorts: BTreeSet<&str> = short_subjects
        .iter()
        .map(String::as_str)
        .filter(|short| short_counts.get(short).copied().unwrap_or(0) == 1)
        .collect();

    // Group indexed RSpec test blocks by their top-level unquoted subject.
    let mut top_matches = BTreeMap::<String, Vec<(usize, Vec<String>)>>::new();
    for (id, def) in index.definitions.iter().enumerate() {
        if !def.test || !def.location.file.ends_with(".rb") {
            continue;
        }
        let segments: Vec<String> = split_rspec_segments(name(def));
        let Some(first) = segments.first() else {
            continue;
        };
        let matched = subjects.contains(first.as_str()) || unique_shorts.contains(first.as_str());
        if matched {
            top_matches
                .entry(def.location.file.clone())
                .or_default()
                .push((id, segments));
        }
    }

    let mut hits = Vec::new();
    for (_file, blocks) in top_matches {
        // Prefer nested `describe '#method'` / `describe '.method'` blocks when
        // a specific method changed and has matching nested RSpec blocks;
        // otherwise select the top-level `RSpec.describe` block.
        let mut method_specific = Vec::new();
        for &(id, ref segments) in &blocks {
            if segments.len() >= 2
                && let Some(first) = segments.first()
                && let Some(methods) = methods_by_subject.get(first)
                && segments[1..].iter().any(|seg| {
                    let clean = seg.trim_start_matches(['#', '.']);
                    methods.contains(clean)
                })
            {
                method_specific.push(id);
            }
        }
        if !method_specific.is_empty() {
            for id in method_specific {
                hits.push((id, 1, true));
            }
        } else if let Some((top_id, _)) = blocks.iter().min_by_key(|(_, segs)| segs.len()) {
            hits.push((*top_id, 1, true));
        }
    }
    hits
}

/// Splits an indexed RSpec test path like `"\"MyApp::OrderService\"::\"#total\""`
/// into unquoted segments `["MyApp::OrderService", "#total"]`.
fn split_rspec_segments(path: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut rest = path;
    while !rest.is_empty() {
        if let Some(after_open) = rest.strip_prefix('"')
            && let Some(close) = after_open.find('"')
        {
            segments.push(after_open[..close].to_owned());
            rest = &after_open[close + 1..];
            rest = rest.strip_prefix("::").unwrap_or(rest);
            continue;
        }
        match rest.split_once("::") {
            Some((head, tail)) => {
                segments.push(head.trim_matches('"').to_owned());
                rest = tail;
            }
            None => {
                segments.push(rest.trim_matches('"').to_owned());
                break;
            }
        }
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_repo(files: &[(&str, &str)]) -> (TempDir, Workspace, Index) {
        let dir = TempDir::new().unwrap();
        for (rel, content) in files {
            let path = dir.path().join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
        let workspace = Workspace::discover(Some(dir.path())).unwrap();
        let index = Index::open(&workspace).unwrap();
        (dir, workspace, index)
    }

    #[test]
    fn js_ts_indexed_test_blocks_map_direct_calls_and_suggest_runner_command() {
        let (_dir, workspace, index) = write_repo(&[
            ("package.json", r#"{"devDependencies":{"vitest":"2.0.0"}}"#),
            (
                "src/calc.ts",
                "export function calcTax(cents: number): number {\n  return cents * 0.2;\n}\nexport function other(): number { return 1; }\n",
            ),
            (
                "src/calc.test.ts",
                concat!(
                    "import { describe, it, expect } from 'vitest';\n",
                    "import { calcTax, other } from './calc';\n",
                    "describe('calc', () => {\n",
                    "  it('computes tax', () => {\n",
                    "    expect(calcTax(100)).toBe(20);\n",
                    "  });\n",
                    "  it('does other', () => {\n",
                    "    expect(other()).toBe(1);\n",
                    "  });\n",
                    "});\n",
                ),
            ),
        ]);
        let (_, seeds) = resolve(&index, "calcTax").unwrap();
        let walk = walk(&index, &seeds, 3);
        let (found_tests, found_files, notes) =
            tests(&workspace, &index, &seeds, &BTreeSet::new(), &walk);
        let test_names: Vec<&str> = found_tests
            .iter()
            .map(|&(id, _, _)| name(&index.definitions[id]))
            .collect();
        assert_eq!(test_names, ["\"calc\"::\"computes tax\""]);
        assert!(found_files.is_empty());
        assert!(notes.is_empty(), "{notes:?}");
        let (cmds, left) = commands(&workspace, &index, &found_tests, &found_files);
        assert_eq!(left, 0);
        assert_eq!(cmds, ["npx vitest run src/calc.test.ts"]);
    }

    #[test]
    fn js_barrel_reexport_and_dynamic_import_widen_to_full_test_file() {
        let (_dir, workspace, index) = write_repo(&[
            ("package.json", r#"{"scripts":{"test":"bun test"}}"#),
            (
                "src/tax.ts",
                "export function vat(n: number) { return n / 5; }\n",
            ),
            (
                "src/index.ts",
                "export { vat as computeVat } from './tax';\n",
            ),
            (
                "src/barrel.test.ts",
                concat!(
                    "import { computeVat } from './index';\n",
                    "describe('barrel', () => {\n",
                    "  it('uses alias', () => { computeVat(100); });\n",
                    "  it('other case', () => { computeVat(200); });\n",
                    "});\n",
                ),
            ),
            (
                "src/dynamic.test.ts",
                concat!(
                    "test('loads lazily', async () => {\n",
                    "  const mod = await import('./tax');\n",
                    "  mod.vat(10);\n",
                    "});\n",
                ),
            ),
        ]);
        let (_, seeds) = resolve(&index, "vat").unwrap();
        let walk = walk(&index, &seeds, 3);
        let (found_tests, found_files, _notes) =
            tests(&workspace, &index, &seeds, &BTreeSet::new(), &walk);
        // Both files were reached through barrel re-export / dynamic import, so
        // they widen conservatively to full test files.
        assert!(found_tests.is_empty(), "{found_tests:?}");
        assert!(
            found_files.contains_key("src/barrel.test.ts"),
            "{found_files:?}"
        );
        assert!(
            found_files.contains_key("src/dynamic.test.ts"),
            "{found_files:?}"
        );
        let (cmds, _) = commands(&workspace, &index, &found_tests, &found_files);
        assert_eq!(cmds, ["bun test src/barrel.test.ts src/dynamic.test.ts"]);
    }

    #[test]
    fn js_computed_dynamic_import_and_shared_setup_emit_conservative_notes() {
        let (_dir, workspace, index) = write_repo(&[
            (
                "jest.config.js",
                "module.exports = { setupFilesAfterEnv: ['./src/custom-setup.ts'] };\n",
            ),
            ("src/custom-setup.ts", "export function initTestEnv() {}\n"),
            (
                "src/plugins/billing.ts",
                "export function run() { return 1; }\n",
            ),
            (
                "src/loader.ts",
                "export async function loadPlugin(name: string) {\n  return await import(`./plugins/${name}`);\n}\n",
            ),
        ]);
        let (_, plugin_seeds) = resolve(&index, "src/plugins/billing.ts").unwrap();
        let plugin_walk = walk(&index, &plugin_seeds, 3);
        let (_, _, plugin_notes) = tests(
            &workspace,
            &index,
            &plugin_seeds,
            &BTreeSet::new(),
            &plugin_walk,
        );
        assert!(
            plugin_notes
                .iter()
                .any(|n| n.contains("Computed dynamic import at src/loader.ts:2")),
            "{plugin_notes:?}"
        );

        let (_, setup_seeds) = resolve(&index, "src/custom-setup.ts").unwrap();
        let setup_walk = walk(&index, &setup_seeds, 3);
        let (_, _, setup_notes) = tests(
            &workspace,
            &index,
            &setup_seeds,
            &BTreeSet::new(),
            &setup_walk,
        );
        assert!(
            setup_notes
                .iter()
                .any(|n| n.contains("referenced by runner config jest.config.js")),
            "{setup_notes:?}"
        );
    }

    #[test]
    fn ruby_rspec_test_blocks_map_by_subject_method_and_require_relative() {
        let (_dir, workspace, index) = write_repo(&[
            ("Gemfile", "gem 'rspec'\n"),
            (
                "lib/billing/order.rb",
                concat!(
                    "module Billing\n",
                    "  class Order\n",
                    "    def total(cents)\n",
                    "      cents + 10\n",
                    "    end\n",
                    "    def refund(cents)\n",
                    "      cents\n",
                    "    end\n",
                    "  end\n",
                    "end\n",
                ),
            ),
            (
                "spec/billing/order_spec.rb",
                concat!(
                    "require_relative '../../lib/billing/order'\n",
                    "RSpec.describe Billing::Order do\n",
                    "  describe '#total' do\n",
                    "    it 'adds fee' do\n",
                    "      expect(described_class.new.total(100)).to eq(110)\n",
                    "    end\n",
                    "  end\n",
                    "  describe '#refund' do\n",
                    "    it 'returns cents' do\n",
                    "      expect(described_class.new.refund(50)).to eq(50)\n",
                    "    end\n",
                    "  end\n",
                    "end\n",
                ),
            ),
        ]);
        let (_, seeds) = resolve(&index, "Billing::Order::total").unwrap();
        let walk = walk(&index, &seeds, 3);
        let (found_tests, found_files, _notes) =
            tests(&workspace, &index, &seeds, &BTreeSet::new(), &walk);
        let test_names: Vec<&str> = found_tests
            .iter()
            .map(|&(id, _, _)| name(&index.definitions[id]))
            .collect();
        assert!(
            test_names
                .iter()
                .any(|n| *n == "\"Billing::Order\"::\"#total\"::\"adds fee\""
                    || *n == "\"Billing::Order\"::\"#total\""),
            "{test_names:?}"
        );
        assert!(
            !test_names.iter().any(|n| n.contains("#refund")),
            "{test_names:?}"
        );
        let (cmds, _) = commands(&workspace, &index, &found_tests, &found_files);
        assert_eq!(cmds, ["bundle exec rspec spec/billing/order_spec.rb"]);
    }
}
