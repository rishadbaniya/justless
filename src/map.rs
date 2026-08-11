//! `justless map`: a budgeted orientation report built from the repository
//! index. It reads no source files, so its cost is the index refresh plus a
//! few linear passes over symbols and links.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde_json::{Map, Value, json};

use crate::context::{Report, relationship};
use crate::index::{Definition, Edge, Index};
use crate::polyglot::Grammar;
use crate::rust_syntax::Kind;
use crate::summary::short;
use crate::workspace::{Location, Workspace};

const NAME_BYTES: usize = 80;
const RANKED: usize = 48;

pub fn command() -> Command<'static> {
    Command::new("map")
        .about("Orient in the repository: areas, hubs, communities, cross-area links, entry points and tests")
        .arg(crate::budget())
        .arg(
            Arg::new("json")
                .long("json")
                .help("Print the selected items as JSON"),
        )
}

pub fn run(workspace: &Workspace, matches: &ArgMatches) -> Result<()> {
    let budget = crate::context_budget(workspace, matches)?;
    let index = Index::open(workspace)?;
    let json = matches.is_present("json");
    let (header, sections, summary) = build(&index);
    let order: Vec<_> = (0..sections.len()).collect();
    let footer = ["Next: justless expand FILE NAME, query NAME or affected NAME"];
    emit(
        &header,
        &sections,
        &footer.map(str::to_owned),
        &order,
        Fill::RoundRobin,
        budget,
        json.then_some(summary),
    )
}

// ---------------------------------------------------------------------------
// Budgeted sections, shared with `affected`

/// A titled group of ranked report lines and their JSON equivalents.
pub(crate) struct Section {
    pub key: &'static str,
    pub title: String,
    pub lines: Vec<String>,
    pub values: Vec<Value>,
    /// Lines placed in turn with the other sections before any section grows past it.
    pub floor: usize,
    /// Ranked items left out before rendering, counted in the omissions.
    pub unlisted: usize,
}

impl Section {
    pub fn new(key: &'static str, title: String, floor: usize) -> Self {
        Self {
            key,
            title,
            lines: Vec::new(),
            values: Vec::new(),
            floor,
            unlisted: 0,
        }
    }

    pub fn push(&mut self, line: String, value: Value) {
        self.lines.push(line);
        self.values.push(value);
    }

    /// Every item, including those never turned into lines.
    fn total(&self) -> usize {
        self.lines.len() + self.unlisted
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Fill {
    /// After the floors, sections keep taking turns.
    RoundRobin,
    /// After the floors, each section fills completely in priority order.
    InOrder,
}

/// Room for `Report`'s clipping marker, which should never be needed.
const RESERVE: usize = 32;

fn more(count: usize) -> String {
    format!("  … {count} more")
}

/// Writes the report lines for `shown` lines per section. A section with
/// nothing shown is listed on one "Not shown" line instead of under its title.
fn render(
    header: &[String],
    sections: &[Section],
    footer: &[String],
    shown: &[usize],
    sink: &mut dyn FnMut(&str),
) {
    for line in header {
        sink(line);
    }
    let mut hidden = Vec::new();
    for (section, &count) in sections.iter().zip(shown) {
        if count == 0 && section.total() > 0 {
            let heading = section.title.split(" · ").next().unwrap_or_default();
            hidden.push(format!("{heading} ({})", section.total()));
            continue;
        }
        sink(&section.title);
        for line in &section.lines[..count] {
            sink(line);
        }
        if count < section.total() {
            sink(&more(section.total() - count));
        }
    }
    if !hidden.is_empty() {
        sink(&format!(
            "Not shown: {} (raise --budget)",
            hidden.join(", ")
        ));
    }
    for line in footer {
        sink(line);
    }
}

/// How many lines of each section fit within `budget`. Sections take turns in
/// `order` up to their floors, then fill as `fill` directs.
fn fit(
    header: &[String],
    sections: &[Section],
    footer: &[String],
    order: &[usize],
    fill: Fill,
    budget: usize,
) -> Vec<usize> {
    let budget = budget.saturating_sub(RESERVE);
    let grow = |section: usize, shown: &mut Vec<usize>| {
        if shown[section] >= sections[section].lines.len() {
            return false;
        }
        shown[section] += 1;
        let mut size = 0;
        render(header, sections, footer, shown, &mut |line| {
            size += line.len() + 1
        });
        if size > budget {
            shown[section] -= 1;
        }
        size <= budget
    };
    let mut shown = vec![0; sections.len()];
    let rounds = |limit: &dyn Fn(&Section) -> usize, shown: &mut Vec<usize>| loop {
        let mut progress = false;
        for &section in order {
            if shown[section] < limit(&sections[section]) {
                progress |= grow(section, shown);
            }
        }
        if !progress {
            break;
        }
    };
    rounds(&|section| section.floor, &mut shown);
    match fill {
        Fill::RoundRobin => rounds(&|_| usize::MAX, &mut shown),
        Fill::InOrder => {
            for &section in order {
                while grow(section, &mut shown) {}
            }
        }
    }
    shown
}

/// Prints the header, each section's fitted lines and the footer, or the same
/// selection as JSON merged into `json`.
pub(crate) fn emit(
    header: &[String],
    sections: &[Section],
    footer: &[String],
    order: &[usize],
    fill: Fill,
    budget: usize,
    json: Option<Map<String, Value>>,
) -> Result<()> {
    let shown = fit(header, sections, footer, order, fill, budget);
    if let Some(mut object) = json {
        let mut omitted = Map::new();
        for (section, &count) in sections.iter().zip(&shown) {
            let values = object
                .entry(section.key)
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(values) = values {
                values.extend_from_slice(&section.values[..count]);
            }
            let missing = section.total() - count;
            if missing > 0 {
                let total = omitted.entry(section.key).or_insert(json!(0));
                *total = json!(total.as_u64().unwrap_or(0) + missing as u64);
            }
        }
        let capped = object.get("capped") == Some(&Value::Bool(true)) || !omitted.is_empty();
        object.insert("omitted".into(), Value::Object(omitted));
        object.insert("capped".into(), Value::Bool(capped));
        println!("{}", serde_json::to_string(&object)?);
        return Ok(());
    }
    let mut report = Report::new(budget);
    render(header, sections, footer, &shown, &mut |line| {
        report.line(line)
    });
    report.finish()
}

// ---------------------------------------------------------------------------
// Naming helpers, shared with `affected`

pub(crate) fn language(file: &str) -> Option<&'static str> {
    if Path::new(file)
        .extension()
        .is_some_and(|extension| extension == "rs")
    {
        return Some("Rust");
    }
    Some(match Grammar::for_file(file)? {
        Grammar::JavaScript => "JavaScript",
        Grammar::TypeScript => "TypeScript",
        Grammar::Tsx => "TSX",
        Grammar::Python => "Python",
        Grammar::Go => "Go",
        Grammar::Java => "Java",
        Grammar::C => "C",
        Grammar::Cpp => "C++",
        Grammar::CSharp => "C#",
        Grammar::Ruby => "Ruby",
        Grammar::Php => "PHP",
        Grammar::Bash => "Bash",
        Grammar::Sql => "SQL",
    })
}

/// The name an agent passes to `expand FILE NAME` or `query`: the path within
/// the file for Tree-sitter languages, and the module path within the crate for Rust.
pub(crate) fn name(definition: &Definition) -> &str {
    let qualified = definition.qualified.as_str();
    if definition.kind == "file" {
        return qualified;
    }
    if let Some(rest) = qualified
        .strip_prefix(definition.location.file.as_str())
        .and_then(|rest| rest.strip_prefix("::"))
    {
        return rest;
    }
    qualified
        .split_once("::")
        .map_or(qualified, |(_, rest)| rest)
}

fn kind(definition: &Definition) -> &str {
    if definition.test {
        "test"
    } else {
        &definition.kind
    }
}

/// `name [kind] file:start-end`, or `file:start-end [file]` for a file.
pub(crate) fn label(definition: &Definition) -> String {
    if definition.kind == "file" {
        return format!("{} [file]", definition.location);
    }
    format!(
        "{} [{}] {}",
        short(name(definition), NAME_BYTES),
        kind(definition),
        definition.location
    )
}

/// `name file:start-end` without the kind, for compact lists.
fn spot(definition: &Definition) -> String {
    if definition.kind == "file" {
        return definition.location.to_string();
    }
    format!(
        "{} {}",
        short(name(definition), NAME_BYTES),
        definition.location
    )
}

pub(crate) fn node(definition: &Definition) -> Value {
    json!({
        "name": name(definition),
        "qualified": definition.qualified,
        "kind": kind(definition),
        "file": definition.location.file,
        "start": definition.location.start,
        "end": definition.location.end,
    })
}

/// Source files that live where test runners look by convention.
pub(crate) fn is_test_file(file: &str) -> bool {
    if language(file).is_none() {
        return false;
    }
    let path = Path::new(file);
    let in_test_dir = path
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .any(|part| {
            matches!(
                part.as_os_str().to_str(),
                Some("tests" | "test" | "__tests__" | "spec" | "specs")
            )
        });
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    in_test_dir
        || name.contains(".test.")
        || name.contains(".spec.")
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_spec")
        || stem.ends_with("Test")
        || stem.ends_with("Tests")
}

pub(crate) fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn directory(file: &str) -> String {
    match Path::new(file).parent().and_then(Path::to_str) {
        Some("") | None => "(root)".into(),
        Some(parent) => format!("{parent}/"),
    }
}

// ---------------------------------------------------------------------------
// The map

/// Symbols that make sense as hubs. Files, module declarations and tests are
/// connected by construction rather than by design.
fn central(definition: &Definition) -> bool {
    !definition.test && !matches!(definition.kind.as_str(), "file" | "module")
}

/// Distinct dependents (callers, importers, file scopes) and dependencies of
/// each definition, counting all links and certain links separately.
struct Degrees {
    dependents: Vec<usize>,
    dependencies: Vec<usize>,
    certain_dependents: Vec<usize>,
    certain_dependencies: Vec<usize>,
}

impl Degrees {
    fn new(index: &Index) -> Self {
        let count = index.definitions.len();
        let mut degrees = Self {
            dependents: vec![0; count],
            dependencies: vec![0; count],
            certain_dependents: vec![0; count],
            certain_dependencies: vec![0; count],
        };
        let mut incoming = HashMap::new();
        let mut outgoing = HashMap::new();
        for edge in &index.edges {
            if edge.from == Some(edge.to) {
                continue;
            }
            let scope = if edge.from.is_none() {
                edge.location.file.as_str()
            } else {
                ""
            };
            *incoming.entry((edge.to, edge.from, scope)).or_insert(false) |= !edge.inferred;
            if let Some(from) = edge.from {
                *outgoing.entry((from, edge.to)).or_insert(false) |= !edge.inferred;
            }
        }
        for ((to, _, _), certain) in incoming {
            degrees.dependents[to] += 1;
            degrees.certain_dependents[to] += usize::from(certain);
        }
        for ((from, _), certain) in outgoing {
            degrees.dependencies[from] += 1;
            degrees.certain_dependencies[from] += usize::from(certain);
        }
        degrees
    }

    /// Ranks certain links first, so a common method name that only matches
    /// by inference cannot outrank a symbol with resolved callers.
    fn rank(&self, id: usize) -> (usize, usize) {
        (
            self.certain_dependents[id] + self.certain_dependencies[id],
            self.dependents[id] + self.dependencies[id],
        )
    }

    /// `in 12 · out 3`, noting how many of those links are only possible.
    fn describe(&self, id: usize) -> String {
        let part = |label: &str, all: usize, certain: usize| {
            if all == certain {
                format!("{label} {all}")
            } else {
                format!("{label} {all} ({} possible)", all - certain)
            }
        };
        format!(
            "{} · {}",
            part("in", self.dependents[id], self.certain_dependents[id]),
            part("out", self.dependencies[id], self.certain_dependencies[id])
        )
    }
}

/// Groups files into areas: top-level directories, with a directory that holds
/// most of the symbols split into its children so one area cannot swallow the map.
fn areas(index: &Index) -> BTreeMap<&str, String> {
    let mut symbols = BTreeMap::<&str, usize>::new();
    for definition in index.definitions.iter().filter(|d| d.kind != "file") {
        *symbols
            .entry(definition.location.file.as_str())
            .or_default() += 1;
    }
    let total: usize = symbols.values().sum();
    let mut splits = BTreeSet::new();
    let assign = |splits: &BTreeSet<String>| {
        index
            .files
            .keys()
            .map(|file| {
                let parts: Vec<&str> = file.split('/').collect();
                let mut depth = 1;
                while depth < parts.len() && splits.contains(&parts[..depth].join("/")) {
                    depth += 1;
                }
                let area = if parts.len() == 1 {
                    "(root)".to_owned()
                } else if depth == parts.len() {
                    file.clone()
                } else {
                    format!("{}/", parts[..depth].join("/"))
                };
                (file.as_str(), area)
            })
            .collect::<BTreeMap<_, _>>()
    };
    let mut assigned = assign(&splits);
    for _ in 0..4 {
        let mut sizes = BTreeMap::<&str, usize>::new();
        for (file, area) in &assigned {
            *sizes.entry(area).or_default() += symbols.get(file).copied().unwrap_or(0);
        }
        let Some((largest, size)) = sizes
            .into_iter()
            .max_by_key(|(area, size)| (*size, Reverse(*area)))
        else {
            break;
        };
        if total == 0 || size * 2 <= total || !largest.ends_with('/') || largest == "(root)" {
            break;
        }
        splits.insert(largest.trim_end_matches('/').to_owned());
        assigned = assign(&splits);
    }
    assigned
}

/// Asynchronous label propagation in a fixed order with the smallest label
/// winning ties, so the same index always yields the same communities. Each
/// symbol is also tied to its innermost enclosing definition or file, which
/// keeps a class with its methods unless its links pull it elsewhere.
fn communities(index: &Index) -> Vec<Vec<usize>> {
    let definitions = &index.definitions;
    // Rust files have no file definition, so they get a node after the definitions.
    let mut nodes = definitions.len();
    let mut files = BTreeMap::<&str, usize>::new();
    let mut by_file = BTreeMap::<&str, Vec<usize>>::new();
    for (id, definition) in definitions.iter().enumerate() {
        if definition.kind == "file" {
            files.insert(&definition.location.file, id);
        } else {
            by_file
                .entry(&definition.location.file)
                .or_default()
                .push(id);
        }
    }
    let mut file_node = |file| {
        *files.entry(file).or_insert_with(|| {
            nodes += 1;
            nodes - 1
        })
    };
    let mut links = HashMap::<(usize, usize), u32>::new();
    let mut link = |a: usize, b: usize, weight: u32| {
        if a != b {
            *links.entry((a.min(b), a.max(b))).or_default() += weight;
        }
    };
    for (file, mut ids) in by_file {
        let root = file_node(file);
        ids.sort_by_key(|&id| {
            let span = &definitions[id].location;
            (span.start, Reverse(span.end), id)
        });
        let mut stack: Vec<usize> = Vec::new();
        for id in ids {
            let end = definitions[id].location.end;
            while stack
                .last()
                .is_some_and(|&top| definitions[top].location.end < end)
            {
                stack.pop();
            }
            link(id, stack.last().copied().unwrap_or(root), 3);
            stack.push(id);
        }
    }
    for edge in &index.edges {
        let from = edge.from.unwrap_or_else(|| file_node(&edge.location.file));
        // Inferred calls match by name alone. Inferred routes and SQL relations
        // still describe how areas connect.
        let weight = match (edge.inferred, edge.kind) {
            (false, _) => 2,
            (true, Kind::Route | Kind::Data) => 1,
            (true, _) => continue,
        };
        link(from, edge.to, weight);
    }
    let mut pairs: Vec<_> = links.into_iter().collect();
    pairs.sort_unstable();
    let mut adjacency = vec![Vec::new(); nodes];
    for ((a, b), weight) in pairs {
        adjacency[a].push((b, weight));
        adjacency[b].push((a, weight));
    }
    let mut labels: Vec<usize> = (0..nodes).collect();
    let mut tally = HashMap::<usize, u32>::new();
    for _ in 0..32 {
        let mut changed = false;
        for node in 0..nodes {
            if adjacency[node].is_empty() {
                continue;
            }
            tally.clear();
            for &(other, weight) in &adjacency[node] {
                *tally.entry(labels[other]).or_default() += weight;
            }
            let best = tally.values().copied().max().unwrap_or(0);
            if tally.get(&labels[node]) == Some(&best) {
                continue;
            }
            let choice = tally
                .iter()
                .filter(|&(_, &weight)| weight == best)
                .map(|(&label, _)| label)
                .min()
                .unwrap();
            labels[node] = choice;
            changed = true;
        }
        if !changed {
            break;
        }
    }
    let mut groups = BTreeMap::<usize, Vec<usize>>::new();
    for (id, definition) in definitions.iter().enumerate() {
        if definition.kind != "file" {
            groups.entry(labels[id]).or_default().push(id);
        }
    }
    let mut groups: Vec<_> = groups
        .into_values()
        .filter(|members| members.len() > 1)
        .collect();
    groups.sort_by_key(|members| (Reverse(members.len()), members[0]));
    groups
}

/// The file holding most of a community's symbols, and how many other files it spans.
fn community_name(files: &BTreeMap<&str, usize>) -> String {
    let (top, _) = files
        .iter()
        .max_by_key(|(file, symbols)| (**symbols, Reverse(**file)))
        .unwrap();
    match files.len() {
        1 => (*top).to_owned(),
        count => format!("{top} +{}", plural(count - 1, "file")),
    }
}

fn kind_rank(kind: Kind) -> u8 {
    match kind {
        Kind::Route => 0,
        Kind::Data => 1,
        Kind::Call | Kind::Method => 2,
        Kind::Import => 3,
        Kind::Reference => 4,
    }
}

/// The index with the derived facts every section uses.
struct Graph<'a> {
    index: &'a Index,
    degrees: Degrees,
    areas: BTreeMap<&'a str, String>,
}

impl<'a> Graph<'a> {
    fn definition(&self, id: usize) -> &'a Definition {
        &self.index.definitions[id]
    }

    fn area(&self, file: &str) -> &str {
        self.areas.get(file).map_or("(root)", String::as_str)
    }

    /// Sort key that puts the most connected symbol first.
    fn rank(&self, id: usize) -> (Reverse<(usize, usize)>, &'a Location, usize) {
        (
            Reverse(self.degrees.rank(id)),
            &self.definition(id).location,
            id,
        )
    }

    /// The most connected hub-worthy symbol among `ids`, else the first by location.
    fn leader(&self, ids: &[usize]) -> Option<usize> {
        let central = ids
            .iter()
            .copied()
            .filter(|&id| central(self.definition(id)));
        central.min_by_key(|&id| self.rank(id)).or_else(|| {
            ids.iter()
                .copied()
                .min_by_key(|&id| (&self.definition(id).location, id))
        })
    }
}

fn build(index: &Index) -> (Vec<String>, Vec<Section>, Map<String, Value>) {
    let graph = Graph {
        index,
        degrees: Degrees::new(index),
        areas: areas(index),
    };
    let symbols = index
        .definitions
        .iter()
        .filter(|d| d.kind != "file")
        .count();
    let possible = index.edges.iter().filter(|edge| edge.inferred).count();
    let mut languages = BTreeMap::<&str, usize>::new();
    for file in index.files.keys() {
        if let Some(language) = language(file) {
            *languages.entry(language).or_default() += 1;
        }
    }
    let mut by_count: Vec<_> = languages.iter().map(|(&l, &n)| (l, n)).collect();
    by_count.sort_by_key(|&(language, files)| (Reverse(files), language));
    let mut header = vec![format!(
        "Map · {} · {} · {} ({possible} possible){}",
        plural(index.files.len(), "file"),
        plural(symbols, "symbol"),
        plural(index.edges.len(), "link"),
        by_count
            .iter()
            .map(|(language, files)| format!(" · {language} {files}"))
            .collect::<String>()
    )];
    if !index.errors.is_empty() {
        header.push(format!(
            "{} could not be parsed. justless index lists them.",
            plural(index.errors.len(), "file")
        ));
    }
    let mut summary = Map::new();
    summary.insert("files".into(), json!(index.files.len()));
    summary.insert("symbols".into(), json!(symbols));
    summary.insert("links".into(), json!(index.edges.len()));
    summary.insert("possible_links".into(), json!(possible));
    summary.insert("languages".into(), json!(languages));
    summary.insert("parse_errors".into(), json!(index.errors.len()));
    let sections = vec![
        area_section(&graph),
        hub_section(&graph),
        community_section(&graph),
        link_section(&graph),
        entry_section(&graph),
        test_section(&graph),
    ];
    (header, sections, summary)
}

fn area_section(graph: &Graph) -> Section {
    struct Area<'a> {
        files: Vec<&'a str>,
        symbols: Vec<usize>,
        languages: BTreeMap<&'static str, usize>,
    }
    let mut grouped = BTreeMap::<&str, Area>::new();
    for (file, name) in &graph.areas {
        let area = grouped.entry(name).or_insert_with(|| Area {
            files: Vec::new(),
            symbols: Vec::new(),
            languages: BTreeMap::new(),
        });
        area.files.push(file);
        if let Some(language) = language(file) {
            *area.languages.entry(language).or_default() += 1;
        }
    }
    for (id, definition) in graph.index.definitions.iter().enumerate() {
        if definition.kind != "file"
            && let Some(area) = grouped.get_mut(graph.area(&definition.location.file))
        {
            area.symbols.push(id);
        }
    }
    let mut ordered: Vec<_> = grouped.into_iter().collect();
    ordered.sort_by_key(|(name, area)| {
        (
            Reverse(area.symbols.len()),
            Reverse(area.files.len()),
            *name,
        )
    });
    let mut section = Section::new("areas", format!("Areas · {}", ordered.len()), 5);
    section.unlisted = ordered.len().saturating_sub(RANKED);
    for (name, area) in ordered.iter().take(RANKED) {
        let mut languages: Vec<_> = area.languages.iter().collect();
        languages.sort_by_key(|(language, files)| (Reverse(**files), **language));
        let languages: Vec<&str> = languages.iter().take(3).map(|(l, _)| **l).collect();
        let (anchor, anchor_json) = match graph.leader(&area.symbols) {
            Some(id) => (
                format!("hub {}", spot(graph.definition(id))),
                node(graph.definition(id)),
            ),
            None => (
                format!("{}:1", area.files[0]),
                json!({"file": area.files[0], "start": 1, "end": 1}),
            ),
        };
        section.push(
            format!(
                "  {name} · {} · {} · {} · {anchor}",
                plural(area.files.len(), "file"),
                plural(area.symbols.len(), "symbol"),
                if languages.is_empty() {
                    "text".into()
                } else {
                    languages.join(", ")
                },
            ),
            json!({
                "area": name,
                "files": area.files.len(),
                "symbols": area.symbols.len(),
                "languages": languages,
                "anchor": anchor_json,
            }),
        );
    }
    section
}

/// Hubs rank by distinct neighbours, so one busy caller cannot inflate them.
fn hub_section(graph: &Graph) -> Section {
    let degrees = &graph.degrees;
    let mut hubs: Vec<usize> = (0..graph.index.definitions.len())
        .filter(|&id| central(graph.definition(id)) && degrees.rank(id).1 > 0)
        .collect();
    hubs.sort_by_key(|&id| graph.rank(id));
    let mut section = Section::new(
        "hubs",
        "Hubs · in = dependents, out = dependencies".into(),
        5,
    );
    section.unlisted = hubs.len().saturating_sub(RANKED);
    for &id in hubs.iter().take(RANKED) {
        let mut value = node(graph.definition(id));
        value["in"] = json!(degrees.dependents[id]);
        value["out"] = json!(degrees.dependencies[id]);
        value["possible_in"] = json!(degrees.dependents[id] - degrees.certain_dependents[id]);
        value["possible_out"] = json!(degrees.dependencies[id] - degrees.certain_dependencies[id]);
        section.push(
            format!(
                "  {} · {}",
                label(graph.definition(id)),
                degrees.describe(id)
            ),
            value,
        );
    }
    if hubs.is_empty() {
        section.title = "Hubs · no indexed links".into();
    }
    section
}

fn community_section(graph: &Graph) -> Section {
    let groups = communities(graph.index);
    let mut section = Section::new(
        "communities",
        format!("Communities · {} groups of linked symbols", groups.len()),
        3,
    );
    section.unlisted = groups.len().saturating_sub(RANKED);
    for members in groups.iter().take(RANKED) {
        let mut files = BTreeMap::<&str, usize>::new();
        for &id in members {
            *files
                .entry(graph.definition(id).location.file.as_str())
                .or_default() += 1;
        }
        let mut leaders: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&id| central(graph.definition(id)))
            .collect();
        if leaders.is_empty() {
            leaders = members.clone();
        }
        leaders.sort_by_key(|&id| graph.rank(id));
        leaders.truncate(2);
        let title = community_name(&files);
        section.push(
            format!(
                "  {title} · {} · {}",
                plural(members.len(), "symbol"),
                leaders
                    .iter()
                    .map(|&id| spot(graph.definition(id)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            json!({
                "name": title,
                "symbols": members.len(),
                "files": files.keys().collect::<Vec<_>>(),
                "hubs": leaders.iter().map(|&id| node(graph.definition(id))).collect::<Vec<_>>(),
            }),
        );
    }
    if groups.is_empty() {
        section.title = "Communities · none with more than one symbol".into();
    }
    section
}

/// Links that cross areas or languages, grouped by direction.
fn link_section(graph: &Graph) -> Section {
    struct Crossing<'a> {
        count: usize,
        certain: usize,
        kinds: BTreeMap<(bool, &'static str), usize>,
        example: &'a Edge,
    }
    let example_rank = |edge: &Edge| (kind_rank(edge.kind), edge.inferred, edge.location.clone());
    let mut crossings = BTreeMap::<(&str, &str, &str, &str), Crossing>::new();
    for edge in &graph.index.edges {
        let source = &edge.location.file;
        let target = &graph.definition(edge.to).location.file;
        let key = (
            graph.area(source),
            graph.area(target),
            language(source).unwrap_or("text"),
            language(target).unwrap_or("text"),
        );
        if key.0 == key.1 && key.2 == key.3 {
            continue;
        }
        let crossing = crossings.entry(key).or_insert(Crossing {
            count: 0,
            certain: 0,
            kinds: BTreeMap::new(),
            example: edge,
        });
        crossing.count += 1;
        crossing.certain += usize::from(!edge.inferred);
        *crossing
            .kinds
            .entry((edge.inferred, relationship(edge.kind)))
            .or_default() += 1;
        if example_rank(edge) < example_rank(crossing.example) {
            crossing.example = edge;
        }
    }
    let mut crossings: Vec<_> = crossings.into_iter().collect();
    crossings.sort_by_key(|((from, to, source, target), crossing)| {
        (
            source == target,
            Reverse(crossing.certain),
            Reverse(crossing.count),
            *from,
            *to,
        )
    });
    let mut section = Section::new(
        "cross_area_links",
        format!("Cross-area links · {}", crossings.len()),
        4,
    );
    section.unlisted = crossings.len().saturating_sub(RANKED);
    for ((from, to, source, target), crossing) in crossings.iter().take(RANKED) {
        let mut kinds: Vec<_> = crossing.kinds.iter().collect();
        kinds.sort_by_key(|((inferred, kind), count)| (Reverse(**count), *inferred, *kind));
        let kinds = kinds
            .iter()
            .map(|((inferred, kind), count)| {
                format!("{}{kind} {count}", if *inferred { "possible " } else { "" })
            })
            .collect::<Vec<_>>()
            .join(", ");
        let languages = if source == target {
            String::new()
        } else {
            format!(" · {source} → {target}")
        };
        let example = crossing.example;
        let origin = match example.from.map(|id| graph.definition(id)) {
            Some(definition) if definition.kind != "file" => format!(
                "{} {}",
                short(name(definition), NAME_BYTES),
                example.location
            ),
            _ => example.location.to_string(),
        };
        section.push(
            format!(
                "  {from} → {to}{languages} · {kinds} · {origin} → {}",
                spot(graph.definition(example.to))
            ),
            json!({
                "from_area": from,
                "to_area": to,
                "from_language": source,
                "to_language": target,
                "count": crossing.count,
                "kinds": crossing.kinds.iter().map(|((inferred, kind), count)| json!({"kind": kind, "possible": inferred, "count": count})).collect::<Vec<_>>(),
                "example": {
                    "at": example.location.to_string(),
                    "kind": relationship(example.kind),
                    "possible": example.inferred,
                    "from": example.from.map(|id| node(graph.definition(id))),
                    "to": node(graph.definition(example.to)),
                },
            }),
        );
    }
    if crossings.is_empty() {
        section.title = "Cross-area links · none".into();
    }
    section
}

/// Main functions, HTTP route handlers, then uncalled symbols with many
/// dependencies, which are often framework or command handlers.
fn entry_section(graph: &Graph) -> Section {
    let definitions = &graph.index.definitions;
    let degrees = &graph.degrees;
    let mut entries: Vec<(u8, usize, String)> = Vec::new();
    for (id, definition) in definitions.iter().enumerate() {
        let last = name(definition).rsplit("::").next().unwrap_or_default();
        if !definition.test
            && matches!(definition.kind.as_str(), "function" | "method")
            && matches!(last, "main" | "Main")
        {
            entries.push((0, id, "main".into()));
        }
    }
    for (route, id) in graph.index.routes() {
        entries.push((1, id, format!("HTTP route {}", short(route, NAME_BYTES))));
    }
    let mut roots: Vec<usize> = (0..definitions.len())
        .filter(|&id| {
            let definition = &definitions[id];
            central(definition)
                && matches!(definition.kind.as_str(), "function" | "method")
                && degrees.dependents[id] == 0
                && degrees.dependencies[id] >= 3
                && !is_test_file(&definition.location.file)
        })
        .collect();
    roots.sort_by_key(|&id| graph.rank(id));
    for id in roots {
        entries.push((
            2,
            id,
            format!("no indexed callers · out {}", degrees.dependencies[id]),
        ));
    }
    let mut listed = BTreeSet::new();
    entries.retain(|(_, id, _)| listed.insert(*id));
    entries.sort_by_key(|(order, id, _)| (*order, graph.rank(*id)));
    let mut section = Section::new(
        "entry_points",
        format!("Entry points · {}", entries.len()),
        3,
    );
    section.unlisted = entries.len().saturating_sub(RANKED);
    for (_, id, reason) in entries.iter().take(RANKED) {
        let mut value = node(&definitions[*id]);
        value["reason"] = json!(reason);
        section.push(format!("  {} · {reason}", label(&definitions[*id])), value);
    }
    if entries.is_empty() {
        section.title = "Entry points · none detected".into();
    }
    section
}

/// Directories with test definitions or test-convention files.
fn test_section(graph: &Graph) -> Section {
    #[derive(Default)]
    struct Place<'a> {
        files: BTreeSet<&'a str>,
        tests: usize,
        example: Option<usize>,
    }
    let mut places = BTreeMap::<String, Place>::new();
    for (id, definition) in graph.index.definitions.iter().enumerate() {
        if definition.test {
            let place = places
                .entry(directory(&definition.location.file))
                .or_default();
            place.files.insert(&definition.location.file);
            place.tests += 1;
            place.example.get_or_insert(id);
        }
    }
    for file in graph.index.files.keys().filter(|file| is_test_file(file)) {
        places
            .entry(directory(file))
            .or_default()
            .files
            .insert(file);
    }
    let mut places: Vec<_> = places.into_iter().collect();
    places.sort_by_key(|(name, place)| {
        (
            Reverse(place.tests),
            Reverse(place.files.len()),
            name.clone(),
        )
    });
    let mut section = Section::new(
        "tests",
        format!("Tests · {}", plural(places.len(), "location")),
        3,
    );
    section.unlisted = places.len().saturating_sub(RANKED);
    for (directory, place) in places.iter().take(RANKED) {
        let first = place.files.first().copied().unwrap_or_default();
        let (example, example_json) = match place.example {
            Some(id) => (label(graph.definition(id)), node(graph.definition(id))),
            None => (
                format!("{first}:1"),
                json!({"file": first, "start": 1, "end": 1}),
            ),
        };
        let count = if place.tests > 0 {
            format!(" · {}", plural(place.tests, "test"))
        } else {
            String::new()
        };
        section.push(
            format!(
                "  {directory} · {}{count} · e.g. {example}",
                plural(place.files.len(), "file"),
            ),
            json!({
                "directory": directory,
                "files": place.files.len(),
                "tests": place.tests,
                "example": example_json,
            }),
        );
    }
    if places.is_empty() {
        section.title = "Tests · none detected".into();
    }
    section
}
