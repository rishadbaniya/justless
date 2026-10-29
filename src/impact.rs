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
    let (tests, files) = tests(&index, &seeds, &possible, &walk);
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
            member: kind == "method" || kind.starts_with("associated"),
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
        // Outside Rust, the index links only the bare name, so a link says
        // nothing about `module.name` on the same line.
        let attribute = !rust && occurrences.iter().any(|&at| after_dot(&mention.text[..at]));
        if !attribute && let Some(target) = other.get(&at) {
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
            if plainly_not_a_reference(&mention.text, short, member, &paths) {
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
        if let Some(first) = elsewhere.first() {
            text.push_str(&format!(
                " · {} resolve to other definitions, such as {first}",
                plural(elsewhere.len(), "mention")
            ));
        }
        section.push(
            text,
            json!({"deleted": name, "note": "no remaining references", "elsewhere": elsewhere.len()}),
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
/// binding, part of a `//` comment that is not documentation, or a qualified
/// path the parser recorded on the line, which resolution with the item
/// restored did not link to it.
fn plainly_not_a_reference(text: &str, name: &str, member: bool, paths: &[&[String]]) -> bool {
    let comment = plain_comment(text);
    let qualified = paths
        .iter()
        .any(|path| path.len() > 1 && path.last().is_some_and(|last| last == name));
    whole_words(text, name).all(|at| {
        if comment.is_some_and(|start| at > start) {
            return true;
        }
        let before = text[..at].trim_end();
        let after = text[at + name.len()..].trim_start();
        (after_dot(before) && !member)
            || (after.starts_with(':') && !after.starts_with("::"))
            || binding(before)
            || (before.ends_with("::") && qualified)
    })
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

/// Whether the text before a name ends with `let` or `let mut`, so the name is bound there.
fn binding(before: &str) -> bool {
    let before = before.strip_suffix("mut").map_or(before, str::trim_end);
    before
        .strip_suffix("let")
        .is_some_and(|rest| !rest.ends_with(|c: char| c.is_alphanumeric() || c == '_'))
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
    Ok(seeded(
        workspace,
        index,
        &changes(workspace, index, since)?,
    ))
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
                    _ => "empty after changes",
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

type TestFiles<'a> = BTreeMap<&'a str, (usize, bool, &'a Location)>;

/// Test definitions that reach a seed, from the walk and from call chains up to
/// eight hops regardless of `--depth`, and test-convention files reached without
/// a detected test definition, such as JavaScript suites. A test that only a
/// `possible` seed leads to is possible.
fn tests<'a>(
    index: &'a Index,
    seeds: &[usize],
    possible: &BTreeSet<usize>,
    walk: &Walk<'a>,
) -> (Vec<(usize, usize, bool)>, TestFiles<'a>) {
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
        .map(|&id| (&definitions[id].location, 0, possible.contains(&id)));
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
