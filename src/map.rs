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
use crate::workspace::{Location, Workspace, display_path};

const NAME_BYTES: usize = 80;
const RANKED: usize = 48;
const MAX_RANKED: usize = 256;

/// Priority order when fitting `map` sections into a tight budget: areas, hubs
/// (scheduled twice per round so core types fill out the architecture map),
/// entry points, cross-area links, tests, then communities. Visual rendering
/// still follows the natural section order.
const MAP_ORDER: [usize; 7] = [0, 1, 4, 3, 5, 1, 2];

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
    let (header, sections, summary) = build(&index, budget);
    // `--root` for `expand` also serves `query` and `affected`.
    let footer = [format!(
        "Next: {} expand FILE NAME, query NAME or affected NAME",
        workspace.invocation().files
    )];
    emit(
        &header,
        &sections,
        &footer,
        &MAP_ORDER,
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

/// Exact byte length of `Report::CLIPPED` (`"[context budget reached]\n"`),
/// which `Report` reserves so fitted lines never trigger clipping.
const RESERVE: usize = 25;

fn more(count: usize) -> String {
    format!("  … +{count} more")
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
    Grammar::for_file(file).map(Grammar::name)
}

/// The language the index parsed a file as, which also covers scripts named
/// by their `#!` line and tells Objective-C from MATLAB in `.m` files.
fn indexed_language(index: &Index, file: &str) -> Option<&'static str> {
    if Path::new(file)
        .extension()
        .is_some_and(|extension| extension == "rs")
    {
        return Some("Rust");
    }
    index.grammar(file).map(Grammar::name)
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

/// `:start` or `:start-end`, matching `Location`'s line formatting without the file path.
fn line_span(location: &Location) -> String {
    if location.start == location.end {
        format!(":{}", location.start)
    } else {
        format!(":{}-{}", location.start, location.end)
    }
}

/// Whether `dir` (ending with `/`) has at least two path segments, making
/// prefix deduplication worthwhile while keeping shallow `src/` / `tests/` paths intact.
fn is_nested_dir(dir: &str) -> bool {
    dir.ends_with('/') && dir.bytes().filter(|&b| b == b'/').count() >= 2
}

/// `name :start-end` when the enclosing file is already named earlier on the line.
fn spot_in_file(definition: &Definition) -> String {
    if definition.kind == "file" {
        return line_span(&definition.location);
    }
    format!(
        "{} {}",
        short(name(definition), NAME_BYTES),
        line_span(&definition.location)
    )
}

/// `name ./rel:start-end` when `dir` is a nested directory prefix already shown
/// on the same line; otherwise the full `spot`.
fn spot_under_dir(definition: &Definition, dir: &str) -> String {
    if definition.kind != "file"
        && is_nested_dir(dir)
        && let Some(rel) = definition.location.file.strip_prefix(dir)
    {
        return format!(
            "{} ./{}{}",
            short(name(definition), NAME_BYTES),
            display_path(rel),
            line_span(&definition.location)
        );
    }
    spot(definition)
}

/// `name [kind] ./rel:start-end` when `dir` is a nested directory prefix already
/// shown on the same line; otherwise the full `label`.
fn label_under_dir(definition: &Definition, dir: &str) -> String {
    if definition.kind != "file"
        && is_nested_dir(dir)
        && let Some(rel) = definition.location.file.strip_prefix(dir)
    {
        return format!(
            "{} [{}] ./{}{}",
            short(name(definition), NAME_BYTES),
            kind(definition),
            display_path(rel),
            line_span(&definition.location)
        );
    }
    label(definition)
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

/// Files that belong to tests, fixtures, scripts, examples or benchmarks rather
/// than the project's primary source. Used to keep production code ahead of
/// support code when ranking hubs, communities, links, entry points and packs.
pub(crate) fn is_support_file(file: &str) -> bool {
    is_test_file(file)
        || Path::new(file)
            .parent()
            .into_iter()
            .flat_map(Path::components)
            .any(|part| {
                matches!(
                    part.as_os_str().to_str(),
                    Some(
                        "tests"
                            | "test"
                            | "__tests__"
                            | "spec"
                            | "specs"
                            | "fixtures"
                            | "fixture"
                            | "scripts"
                            | "script"
                            | "examples"
                            | "benches"
                            | "bench"
                    )
                )
            })
}

/// Whether an area name such as `tests/` or `fixtures/` represents support code.
fn is_support_area(area: &str) -> bool {
    Path::new(area).components().any(|part| {
        matches!(
            part.as_os_str().to_str(),
            Some(
                "tests"
                    | "test"
                    | "__tests__"
                    | "spec"
                    | "specs"
                    | "fixtures"
                    | "fixture"
                    | "scripts"
                    | "script"
                    | "examples"
                    | "benches"
                    | "bench"
            )
        )
    })
}

/// Whether a test directory belongs to fixtures, examples or benchmarks rather
/// than the project's primary test suites.
fn is_fixture_test_dir(dir: &str) -> bool {
    Path::new(dir).components().any(|part| {
        matches!(
            part.as_os_str().to_str(),
            Some("fixtures" | "fixture" | "examples" | "benches" | "bench")
        )
    })
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

/// Weighted iterative PageRank centrality and outgoing architectural reach.
///
/// Key properties:
/// - **Transitive hub authority**: symbols referenced by important hubs inherit
///   their callers' authority across up to 32 damped power iterations.
/// - **Per-caller-file diminishing returns** (`1 / (k + 1)^2` decay across
///   callers from the same file): a utility called 50 times inside one file
///   receives at most `pi^2 / 6 ≈ 1.645×` one caller's flow from that file, so
///   cross-file and cross-area connectors decisively outrank local helpers.
/// - **Boundary & certainty weighting**: cross-area links (`3.0`) outrank
///   cross-file (`1.5`) and same-file (`0.5`) links; resolved links and
///   cross-boundary routes/SQL relations outrank name-heuristic inferences;
///   support/test callers are damped when scoring production hubs.
/// - **Deterministic**: all graphs, edge lists, and accumulations iterate in
///   sorted order and quantize to fixed-point `u64` rank keys.
struct Centrality {
    score: Vec<f64>,
    rank_key: Vec<u64>,
    out_reach: Vec<u64>,
    caller_files: Vec<usize>,
    caller_areas: Vec<usize>,
}

impl Centrality {
    const DAMPING: f64 = 0.85;
    /// Virtual self-retention added to each node's outgoing weight denominator so
    /// a single weak same-file or inferred edge does not re-normalize to 1.0.
    const LEAK: f64 = 1.0;
    const MAX_ITERS: usize = 32;
    const EPSILON: f64 = 1e-6;
    const SCALE: f64 = 1_000_000.0;

    fn quantize(value: f64) -> u64 {
        if !value.is_finite() || value <= 0.0 {
            return 0;
        }
        (value * Self::SCALE).round().min(u64::MAX as f64) as u64
    }

    fn new(index: &Index, areas: &BTreeMap<&str, String>) -> Self {
        let def_count = index.definitions.len();
        let area_of = |file: &str| areas.get(file).map_or("(root)", String::as_str);

        // Map files that already have a `"file"` definition, then allocate
        // deterministic virtual file-scope nodes for any remaining files that
        // emit file-scope edges (`edge.from == None`, e.g. Rust imports).
        let mut file_nodes = BTreeMap::<&str, usize>::new();
        for (id, def) in index.definitions.iter().enumerate() {
            if def.kind == "file" {
                file_nodes.insert(def.location.file.as_str(), id);
            }
        }
        let mut extra_files = BTreeSet::<&str>::new();
        for edge in &index.edges {
            if edge.from.is_none() && !file_nodes.contains_key(edge.location.file.as_str()) {
                extra_files.insert(edge.location.file.as_str());
            }
        }
        let mut node_file: Vec<&str> = index
            .definitions
            .iter()
            .map(|d| d.location.file.as_str())
            .collect();
        for file in extra_files {
            let id = node_file.len();
            file_nodes.insert(file, id);
            node_file.push(file);
        }
        let node_count = node_file.len();
        let node_area: Vec<&str> = node_file.iter().map(|&f| area_of(f)).collect();
        let node_support: Vec<bool> = node_file.iter().map(|&f| is_support_file(f)).collect();

        // Deduplicate directed edges `(src, dst)` keeping the strongest edge weight
        // and tracking per-definition external file/area diversity.
        let mut edge_weights = BTreeMap::<(usize, usize), f64>::new();
        let mut ext_in_files = vec![BTreeSet::<&str>::new(); def_count];
        let mut ext_in_areas = vec![BTreeSet::<&str>::new(); def_count];
        let mut ext_out_areas = vec![BTreeSet::<&str>::new(); def_count];
        let mut has_strong_in = vec![false; def_count];

        for edge in &index.edges {
            let src = match edge.from {
                Some(id) => id,
                None => file_nodes[edge.location.file.as_str()],
            };
            let dst = edge.to;
            if src == dst {
                continue;
            }
            let src_file = node_file[src];
            let dst_file = node_file[dst];
            let src_area = node_area[src];
            let dst_area = node_area[dst];

            let boundary = if src_area != dst_area {
                3.0
            } else if src_file != dst_file {
                1.5
            } else {
                0.5
            };
            let certainty = if !edge.inferred {
                1.0
            } else if matches!(edge.kind, Kind::Route | Kind::Data) {
                0.7
            } else {
                0.2
            };
            let kind_w = match edge.kind {
                Kind::Route | Kind::Data => 1.2,
                Kind::Call | Kind::Method => 1.0,
                Kind::Import => 0.9,
                Kind::Reference => 0.8,
            };
            let support = if node_support[src] && !node_support[dst] {
                0.35
            } else {
                1.0
            };
            let weight = boundary * certainty * kind_w * support;
            edge_weights
                .entry((src, dst))
                .and_modify(|existing| {
                    if weight > *existing {
                        *existing = weight;
                    }
                })
                .or_insert(weight);

            if dst < def_count {
                let count_caller = !node_support[src] || node_support[dst];
                if count_caller && src_file != dst_file {
                    ext_in_files[dst].insert(src_file);
                }
                if count_caller && src_area != dst_area {
                    ext_in_areas[dst].insert(src_area);
                }
                if !edge.inferred || matches!(edge.kind, Kind::Route | Kind::Data) {
                    has_strong_in[dst] = true;
                }
            }
            if src < def_count && src_area != dst_area && (!node_support[dst] || node_support[src])
            {
                ext_out_areas[src].insert(dst_area);
            }
        }

        let mut out_sum = vec![0.0f64; node_count];
        let mut outgoing = vec![Vec::<(usize, f64)>::new(); def_count];
        for (&(src, dst), &weight) in &edge_weights {
            out_sum[src] += weight;
            if src < def_count {
                outgoing[src].push((dst, weight));
            }
        }

        // Pre-group each target's incoming edges by caller file in deterministic
        // order so the power-iteration loop does zero per-iteration allocations.
        let mut by_dst_file = vec![BTreeMap::<&str, Vec<(usize, f64)>>::new(); node_count];
        for (&(src, dst), &weight) in &edge_weights {
            let trans = weight / (out_sum[src] + Self::LEAK);
            by_dst_file[dst]
                .entry(node_file[src])
                .or_default()
                .push((src, trans));
        }
        let incoming_groups: Vec<Vec<Vec<(usize, f64)>>> = by_dst_file
            .into_iter()
            .map(|groups| groups.into_values().collect())
            .collect();

        // Active nodes start with base teleportation mass (support files damped).
        let base: Vec<f64> = (0..node_count)
            .map(|u| {
                if out_sum[u] == 0.0 && incoming_groups[u].is_empty() {
                    0.0
                } else if node_support[u] {
                    0.25
                } else {
                    1.0
                }
            })
            .collect();

        let mut rank = base.clone();
        let mut next = vec![0.0f64; node_count];
        let mut incoming_flow = vec![0.0f64; node_count];
        let mut flows = Vec::<f64>::new();

        for _ in 0..Self::MAX_ITERS {
            let mut max_delta = 0.0f64;
            for dst in 0..node_count {
                let mut received = 0.0f64;
                for group in &incoming_groups[dst] {
                    if group.len() == 1 {
                        let (src, trans) = group[0];
                        received += rank[src] * trans;
                    } else {
                        flows.clear();
                        for &(src, trans) in group {
                            flows.push(rank[src] * trans);
                        }
                        flows.sort_unstable_by(|a, b| b.total_cmp(a));
                        for (k, &flow) in flows.iter().enumerate() {
                            let step = (k + 1) as f64;
                            received += flow / (step * step);
                        }
                    }
                }
                incoming_flow[dst] = received;
                let updated = (1.0 - Self::DAMPING) * base[dst] + Self::DAMPING * received;
                let delta = (updated - rank[dst]).abs();
                if delta > max_delta {
                    max_delta = delta;
                }
                next[dst] = updated;
            }
            std::mem::swap(&mut rank, &mut next);
            if max_delta < Self::EPSILON {
                break;
            }
        }

        let mut score = vec![0.0f64; def_count];
        let mut rank_key = vec![0u64; def_count];
        let mut caller_files = vec![0usize; def_count];
        let mut caller_areas = vec![0usize; def_count];
        for id in 0..def_count {
            let files = ext_in_files[id].len();
            let in_areas = ext_in_areas[id].len();
            let out_areas = ext_out_areas[id].len();
            caller_files[id] = files;
            caller_areas[id] = in_areas;
            let mut value = incoming_flow[id];
            if value > 0.0 {
                let area_factor = 1.0 + 0.35 * (in_areas as f64 + 0.5 * out_areas as f64).ln_1p();
                let file_factor = 1.0 + 0.15 * (files as f64).ln_1p();
                let kind_factor = if matches!(
                    index.definitions[id].kind.as_str(),
                    "struct" | "class" | "enum" | "trait" | "interface" | "table" | "protocol"
                ) {
                    1.25
                } else {
                    1.0
                };
                value *= area_factor * file_factor * kind_factor;
                if !has_strong_in[id] {
                    value *= 0.25;
                }
            }
            score[id] = value;
            rank_key[id] = Self::quantize(value);
        }

        // Propagate outgoing reach a few hops backward so thin entry-point
        // wrappers (`main -> run -> dispatch -> core hubs`) inherit the
        // centrality of the architecture they orchestrate.
        let direct_reach: Vec<f64> = (0..def_count)
            .map(|src| {
                outgoing[src]
                    .iter()
                    .map(|&(dst, weight)| {
                        let authority = if dst < def_count {
                            0.25 + score[dst]
                        } else {
                            0.25
                        };
                        weight * authority
                    })
                    .sum()
            })
            .collect();
        let mut reach = direct_reach.clone();
        let mut next_reach = vec![0.0f64; def_count];
        for _ in 0..4 {
            for src in 0..def_count {
                let denom = out_sum[src] + 0.25;
                let downstream: f64 = outgoing[src]
                    .iter()
                    .filter(|&&(dst, _)| dst < def_count)
                    .map(|&(dst, weight)| (weight / denom) * reach[dst])
                    .sum();
                next_reach[src] = direct_reach[src] + 0.65 * downstream;
            }
            std::mem::swap(&mut reach, &mut next_reach);
        }
        let out_reach = reach.into_iter().map(Self::quantize).collect();

        Self {
            score,
            rank_key,
            out_reach,
            caller_files,
            caller_areas,
        }
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
        // Keep a flat directory of many files (such as `src/` with dozens of
        // sibling modules) intact instead of shattering it into single-file
        // areas that all sink below `tests/` or `fixtures/`. Split when it has
        // a small set of direct children or when subdirectories hold at least
        // half of its symbols.
        let mut direct_children = BTreeSet::new();
        let mut subdir_symbols = 0;
        for (file, area) in &assigned {
            if *area != largest {
                continue;
            }
            if let Some(rest) = file.strip_prefix(largest) {
                direct_children.insert(rest.split('/').next().unwrap_or(rest));
                if rest.contains('/') {
                    subdir_symbols += symbols.get(file).copied().unwrap_or(0);
                }
            }
        }
        if direct_children.len() > 12 && subdir_symbols * 2 < size {
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

/// Formats one or more cross-language pairs (`source != target`), preserving
/// every language tag while collapsing pairs that share the same target
/// (e.g. `Astro/Svelte/Vue → TypeScript`).
fn format_language_pairs(pairs: &BTreeMap<(&str, &str), usize>) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    if pairs.len() == 1 {
        let (&(source, target), _) = pairs.iter().next().unwrap();
        return format!(" · {source} → {target}");
    }
    let mut by_target = BTreeMap::<&str, (usize, Vec<(&str, usize)>)>::new();
    for (&(source, target), &count) in pairs {
        let entry = by_target.entry(target).or_insert((0, Vec::new()));
        entry.0 += count;
        entry.1.push((source, count));
    }
    let mut grouped: Vec<_> = by_target.into_iter().collect();
    grouped.sort_by_key(|(target, (total, _))| (Reverse(*total), *target));
    let rendered = grouped
        .into_iter()
        .map(|(target, (_, mut sources))| {
            sources.sort_by_key(|&(source, count)| (Reverse(count), source));
            let left = sources
                .into_iter()
                .map(|(source, _)| source)
                .collect::<Vec<_>>()
                .join("/");
            format!("{left} → {target}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(" · {rendered}")
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct RankKey<'a> {
    centrality: Reverse<u64>,
    out_reach: Reverse<u64>,
    certain_degree: Reverse<usize>,
    total_degree: Reverse<usize>,
    location: &'a Location,
    id: usize,
}

/// The index with the derived facts every section uses.
struct Graph<'a> {
    index: &'a Index,
    degrees: Degrees,
    centrality: Centrality,
    areas: BTreeMap<&'a str, String>,
    ranked_limit: usize,
}

impl<'a> Graph<'a> {
    fn new(index: &'a Index, budget: usize) -> Self {
        let degrees = Degrees::new(index);
        let areas = areas(index);
        let centrality = Centrality::new(index, &areas);
        let ranked_limit = (budget / 40).clamp(RANKED, MAX_RANKED);
        Self {
            index,
            degrees,
            centrality,
            areas,
            ranked_limit,
        }
    }

    fn definition(&self, id: usize) -> &'a Definition {
        &self.index.definitions[id]
    }

    fn area(&self, file: &str) -> &str {
        self.areas.get(file).map_or("(root)", String::as_str)
    }

    /// Sort key that puts the most architecturally central symbol first,
    /// falling back to outgoing reach and degree when incoming centrality ties.
    fn rank(&self, id: usize) -> RankKey<'a> {
        let (certain, total) = self.degrees.rank(id);
        RankKey {
            centrality: Reverse(self.centrality.rank_key[id]),
            out_reach: Reverse(self.centrality.out_reach[id]),
            certain_degree: Reverse(certain),
            total_degree: Reverse(total),
            location: &self.definition(id).location,
            id,
        }
    }

    /// The most central hub-worthy symbol among `ids`, else the first by location.
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

fn format_header(
    files: usize,
    symbols: usize,
    links: usize,
    possible: usize,
    by_count: &[(&str, usize)],
    budget: usize,
) -> String {
    let possible_part = if possible > 0 {
        format!(" ({possible} possible)")
    } else {
        String::new()
    };
    let prefix = format!(
        "Map · {} · {} · {}{possible_part}",
        plural(files, "file"),
        plural(symbols, "symbol"),
        plural(links, "link"),
    );
    let all_langs: String = by_count
        .iter()
        .map(|(language, count)| format!(" · {language} {count}"))
        .collect();
    let full = format!("{prefix}{all_langs}");
    // Preserve every language verbatim on normal repos and generous budgets;
    // fold only the long tail on fixture-heavy polyglot repos under tight budgets.
    const KEEP_TOP: usize = 6;
    if by_count.len() <= KEEP_TOP + 1 || full.len().saturating_mul(8) <= budget {
        return full;
    }
    let top: String = by_count[..KEEP_TOP]
        .iter()
        .map(|(language, count)| format!(" · {language} {count}"))
        .collect();
    let rest_langs = by_count.len() - KEEP_TOP;
    let rest_files: usize = by_count[KEEP_TOP..].iter().map(|(_, count)| *count).sum();
    format!("{prefix}{top} · +{rest_langs} langs ({rest_files})")
}

fn build(index: &Index, budget: usize) -> (Vec<String>, Vec<Section>, Map<String, Value>) {
    let graph = Graph::new(index, budget);
    let symbols = index
        .definitions
        .iter()
        .filter(|d| d.kind != "file")
        .count();
    let possible = index.edges.iter().filter(|edge| edge.inferred).count();
    let mut languages = BTreeMap::<&str, usize>::new();
    for file in index.files.keys() {
        if let Some(language) = indexed_language(index, file) {
            *languages.entry(language).or_default() += 1;
        }
    }
    let mut by_count: Vec<_> = languages.iter().map(|(&l, &n)| (l, n)).collect();
    by_count.sort_by_key(|&(language, files)| (Reverse(files), language));
    let mut header = vec![format_header(
        index.files.len(),
        symbols,
        index.edges.len(),
        possible,
        &by_count,
        budget,
    )];
    if !index.errors.is_empty() {
        header.push(format!(
            "{} could not be parsed. {} index lists them.",
            plural(index.errors.len(), "file"),
            index.justless
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
        if let Some(language) = indexed_language(graph.index, file) {
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
        let tier = if area.symbols.is_empty() {
            2u8
        } else if is_support_area(name) {
            1u8
        } else {
            0u8
        };
        (
            tier,
            Reverse(area.symbols.len()),
            Reverse(area.files.len()),
            *name,
        )
    });
    let mut section = Section::new("areas", format!("Areas · {}", ordered.len()), 5);
    let limit = graph.ranked_limit;
    section.unlisted = ordered.len().saturating_sub(limit);
    for (name, area) in ordered.iter().take(limit) {
        let mut languages: Vec<_> = area.languages.iter().collect();
        languages.sort_by_key(|(language, files)| (Reverse(**files), **language));
        let languages: Vec<&str> = languages.iter().take(3).map(|(l, _)| **l).collect();
        let (anchor, anchor_json) = match graph.leader(&area.symbols) {
            Some(id) => {
                let def = graph.definition(id);
                let shown = if def.location.file == *name {
                    spot_in_file(def)
                } else {
                    spot_under_dir(def, name)
                };
                (format!("hub {shown}"), node(def))
            }
            None => {
                let first = area.files[0];
                let shown = if is_nested_dir(name)
                    && let Some(rel) = first.strip_prefix(name)
                {
                    format!("./{}:1", display_path(rel))
                } else {
                    format!("{first}:1")
                };
                (shown, json!({"file": first, "start": 1, "end": 1}))
            }
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

/// Hubs rank by weighted PageRank centrality so symbols referenced across
/// multiple areas and by other important hubs outrank single-file utilities.
fn hub_section(graph: &Graph) -> Section {
    let degrees = &graph.degrees;
    let centrality = &graph.centrality;
    let mut hubs: Vec<usize> = (0..graph.index.definitions.len())
        .filter(|&id| central(graph.definition(id)) && degrees.rank(id).1 > 0)
        .collect();
    hubs.sort_by_key(|&id| {
        (
            is_support_file(&graph.definition(id).location.file),
            graph.rank(id),
        )
    });
    let mut section = Section::new("hubs", format!("Hubs · {} · in/out", hubs.len()), 6);
    let limit = graph.ranked_limit;
    section.unlisted = hubs.len().saturating_sub(limit);
    for &id in hubs.iter().take(limit) {
        let mut value = node(graph.definition(id));
        value["in"] = json!(degrees.dependents[id]);
        value["out"] = json!(degrees.dependencies[id]);
        value["possible_in"] = json!(degrees.dependents[id] - degrees.certain_dependents[id]);
        value["possible_out"] = json!(degrees.dependencies[id] - degrees.certain_dependencies[id]);
        value["centrality"] = json!((centrality.score[id] * 1000.0).round() / 1000.0);
        value["caller_files"] = json!(centrality.caller_files[id]);
        value["caller_areas"] = json!(centrality.caller_areas[id]);
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
    let mut groups = communities(graph.index);
    groups.sort_by_key(|members| {
        let support_only = members.iter().all(|&id| {
            let def = graph.definition(id);
            def.test || is_support_file(&def.location.file)
        });
        (support_only, Reverse(members.len()), members[0])
    });
    let mut section = Section::new("communities", format!("Communities · {}", groups.len()), 2);
    let limit = graph.ranked_limit;
    section.unlisted = groups.len().saturating_sub(limit);
    for members in groups.iter().take(limit) {
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
        let single_file = files.len() == 1;
        let mut leader_spots = Vec::with_capacity(leaders.len());
        let mut prev_file: Option<&str> = None;
        for &id in &leaders {
            let def = graph.definition(id);
            let file = def.location.file.as_str();
            let rendered = if single_file || prev_file == Some(file) {
                spot_in_file(def)
            } else if let Some(prev) = prev_file {
                let prev_dir = directory(prev);
                if prev_dir != "(root)" && directory(file) == prev_dir {
                    spot_under_dir(def, &prev_dir)
                } else {
                    spot(def)
                }
            } else {
                spot(def)
            };
            prev_file = Some(file);
            leader_spots.push(rendered);
        }
        section.push(
            format!(
                "  {title} · {} · {}",
                plural(members.len(), "symbol"),
                leader_spots.join(", ")
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

/// Links that cross areas or languages, grouped by unique `(from_area, to_area)`
/// pair so multi-language pairs never emit duplicate area-pair rows.
fn link_section(graph: &Graph) -> Section {
    struct Crossing<'a> {
        count: usize,
        certain: usize,
        kinds: BTreeMap<(bool, &'static str), usize>,
        language_pairs: BTreeMap<(&'static str, &'static str), usize>,
        example: &'a Edge,
    }
    let example_rank = |edge: &Edge| {
        (
            kind_rank(edge.kind),
            edge.inferred,
            Reverse(graph.centrality.rank_key[edge.to]),
            edge.location.clone(),
        )
    };
    let mut crossings = BTreeMap::<(&str, &str), Crossing>::new();
    for edge in &graph.index.edges {
        let source = &edge.location.file;
        let target = &graph.definition(edge.to).location.file;
        let from_area = graph.area(source);
        let to_area = graph.area(target);
        let from_lang = language(source).unwrap_or("text");
        let to_lang = language(target).unwrap_or("text");
        if from_area == to_area && from_lang == to_lang {
            continue;
        }
        let crossing = crossings.entry((from_area, to_area)).or_insert(Crossing {
            count: 0,
            certain: 0,
            kinds: BTreeMap::new(),
            language_pairs: BTreeMap::new(),
            example: edge,
        });
        crossing.count += 1;
        crossing.certain += usize::from(!edge.inferred);
        *crossing
            .kinds
            .entry((edge.inferred, relationship(edge.kind)))
            .or_default() += 1;
        if from_lang != to_lang {
            *crossing
                .language_pairs
                .entry((from_lang, to_lang))
                .or_default() += 1;
        }
        if example_rank(edge) < example_rank(crossing.example) {
            crossing.example = edge;
        }
    }
    let mut crossings: Vec<_> = crossings.into_iter().collect();
    crossings.sort_by_key(|((from, to), crossing)| {
        let support_tier = usize::from(is_support_area(from)) + usize::from(is_support_area(to));
        (
            support_tier,
            from == to,
            crossing.language_pairs.is_empty(),
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
    let limit = graph.ranked_limit;
    section.unlisted = crossings.len().saturating_sub(limit);
    for ((from, to), crossing) in crossings.iter().take(limit) {
        let mut kinds: Vec<_> = crossing.kinds.iter().collect();
        kinds.sort_by_key(|((inferred, kind), count)| (Reverse(**count), *inferred, *kind));
        let kinds = kinds
            .iter()
            .map(|((inferred, kind), count)| {
                format!("{}{kind} {count}", if *inferred { "possible " } else { "" })
            })
            .collect::<Vec<_>>()
            .join(", ");
        let languages = format_language_pairs(&crossing.language_pairs);
        let example = crossing.example;
        let target_def = graph.definition(example.to);
        let origin = match example.from.map(|id| graph.definition(id)) {
            Some(definition) if definition.kind != "file" => format!(
                "{} {}",
                short(name(definition), NAME_BYTES),
                example.location
            ),
            _ => example.location.to_string(),
        };
        let target_spot = if from == to {
            let origin_dir = directory(&example.location.file);
            if origin_dir != "(root)" && directory(&target_def.location.file) == origin_dir {
                spot_under_dir(target_def, &origin_dir)
            } else {
                spot(target_def)
            }
        } else {
            spot(target_def)
        };
        let example_from_lang = language(&example.location.file).unwrap_or("text");
        let example_to_lang = language(&target_def.location.file).unwrap_or("text");
        let mut value = json!({
            "from_area": from,
            "to_area": to,
            "from_language": example_from_lang,
            "to_language": example_to_lang,
            "count": crossing.count,
            "kinds": crossing.kinds.iter().map(|((inferred, kind), count)| json!({"kind": kind, "possible": inferred, "count": count})).collect::<Vec<_>>(),
            "example": {
                "at": example.location.to_string(),
                "kind": relationship(example.kind),
                "possible": example.inferred,
                "from": example.from.map(|id| node(graph.definition(id))),
                "to": node(target_def),
            },
        });
        if crossing.language_pairs.len() > 1 {
            value["language_pairs"] = json!(
                crossing
                    .language_pairs
                    .iter()
                    .map(|((src, dst), count)| json!({"from": src, "to": dst, "count": count}))
                    .collect::<Vec<_>>()
            );
        }
        section.push(
            format!("  {from} → {to}{languages} · {kinds} · {origin} → {target_spot}"),
            value,
        );
    }
    if crossings.is_empty() {
        section.title = "Cross-area links · none".into();
    }
    section
}

/// Main functions, HTTP route handlers, then uncalled orchestrators ranked by
/// their weighted reach into central symbols.
fn entry_section(graph: &Graph) -> Section {
    let definitions = &graph.index.definitions;
    let degrees = &graph.degrees;
    let centrality = &graph.centrality;

    // Track which definitions have at least one cross-file outgoing edge so
    // secondary uncalled entry points (`out == 2`) surface only true orchestrators.
    let mut has_cross_file_out = vec![false; definitions.len()];
    for edge in &graph.index.edges {
        if let Some(from) = edge.from
            && definitions[from].location.file != definitions[edge.to].location.file
        {
            has_cross_file_out[from] = true;
        }
    }

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
    let mut roots: Vec<(u8, usize)> = (0..definitions.len())
        .filter_map(|id| {
            let definition = &definitions[id];
            if !central(definition)
                || !matches!(definition.kind.as_str(), "function" | "method")
                || degrees.dependents[id] != 0
                || is_test_file(&definition.location.file)
            {
                return None;
            }
            if degrees.dependencies[id] >= 3 {
                Some((2, id))
            } else if degrees.dependencies[id] == 2
                && has_cross_file_out[id]
                && !is_support_file(&definition.location.file)
            {
                Some((3, id))
            } else {
                None
            }
        })
        .collect();
    roots.sort_by_key(|&(tier, id)| (tier, Reverse(centrality.out_reach[id]), graph.rank(id)));
    for (tier, id) in roots {
        entries.push((
            tier,
            id,
            format!("uncalled · out {}", degrees.dependencies[id]),
        ));
    }
    let mut listed = BTreeSet::new();
    entries.retain(|(_, id, _)| listed.insert(*id));
    entries.sort_by_key(|(order, id, _)| {
        (
            is_support_file(&definitions[*id].location.file),
            *order,
            Reverse(centrality.out_reach[*id]),
            graph.rank(*id),
        )
    });
    let mut section = Section::new(
        "entry_points",
        format!("Entry points · {}", entries.len()),
        4,
    );
    let limit = graph.ranked_limit;
    section.unlisted = entries.len().saturating_sub(limit);
    for (_, id, reason) in entries.iter().take(limit) {
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
            // Pick a concise representative test name (tie-break by location) so
            // long descriptive test names do not consume disproportionate budget.
            let replace = match place.example {
                None => true,
                Some(current) => {
                    let prev = graph.definition(current);
                    (name(definition).len(), &definition.location, id)
                        < (name(prev).len(), &prev.location, current)
                }
            };
            if replace {
                place.example = Some(id);
            }
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
            is_fixture_test_dir(name),
            Reverse(place.tests),
            Reverse(place.files.len()),
            name.clone(),
        )
    });
    let mut section = Section::new("tests", format!("Tests · {}", places.len()), 3);
    let limit = graph.ranked_limit;
    section.unlisted = places.len().saturating_sub(limit);
    for (dir, place) in places.iter().take(limit) {
        let first = place.files.first().copied().unwrap_or_default();
        let (example, example_json) = match place.example {
            Some(id) => (
                label_under_dir(graph.definition(id), dir),
                node(graph.definition(id)),
            ),
            None => {
                let shown = if is_nested_dir(dir)
                    && let Some(rel) = first.strip_prefix(dir.as_str())
                {
                    format!("./{}:1", display_path(rel))
                } else {
                    format!("{first}:1")
                };
                (shown, json!({"file": first, "start": 1, "end": 1}))
            }
        };
        let count = if place.tests > 0 {
            format!(" · {}", plural(place.tests, "test"))
        } else {
            String::new()
        };
        section.push(
            format!(
                "  {dir} · {}{count} · e.g. {example}",
                plural(place.files.len(), "file"),
            ),
            json!({
                "directory": dir,
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

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn write_repo(files: &[(&str, &str)]) -> (TempDir, Index) {
        let dir = TempDir::new().unwrap();
        for (rel, content) in files {
            let path = dir.path().join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
        }
        let ws = Workspace::discover(Some(dir.path())).unwrap();
        let index = Index::open(&ws).unwrap();
        (dir, index)
    }

    fn render_map(index: &Index, budget: usize) -> String {
        let (header, sections, _) = build(index, budget);
        let footer = ["Next: justless expand FILE NAME, query NAME or affected NAME".to_owned()];
        let shown = fit(
            &header,
            &sections,
            &footer,
            &MAP_ORDER,
            Fill::RoundRobin,
            budget,
        );
        let mut report = Report::new(budget);
        render(&header, &sections, &footer, &shown, &mut |line| {
            report.line(line)
        });
        report.into_string()
    }

    fn hub_names(index: &Index, budget: usize) -> Vec<String> {
        let (_, sections, _) = build(index, budget);
        let hubs = sections.iter().find(|s| s.key == "hubs").unwrap();
        hubs.values
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn weighted_centrality_ranks_cross_area_hub_above_single_file_utility() {
        // One noisy report file calls `fmt_cents` from 50 leaf functions, while
        // `domain::Order` is used across 3 distinct production areas (api, billing,
        // storage), and `orchestrate` has high outgoing degree (in 1, out 12).
        let mut report_src = String::from("from util.fmt import fmt_cents\n");
        for i in 0..50 {
            report_src.push_str(&format!(
                "def render_col_{i:02}(n):\n    return fmt_cents(n)\n"
            ));
        }
        let mut god_src = String::from("from domain.order import Order\n");
        for i in 0..12 {
            god_src.push_str(&format!("def step_{i:02}():\n    return {i}\n"));
        }
        god_src.push_str("def orchestrate():\n    Order()\n");
        for i in 0..12 {
            god_src.push_str(&format!("    step_{i:02}()\n"));
        }

        let (_dir, index) = write_repo(&[
            ("util/fmt.py", "def fmt_cents(n):\n    return str(n)\n"),
            ("reports/render.py", &report_src),
            ("domain/order.py", "class Order:\n    pass\n"),
            (
                "api/orders.py",
                "from domain.order import Order\ndef create_order():\n    return Order()\n",
            ),
            (
                "billing/charge.py",
                "from domain.order import Order\nfrom api.orders import create_order\ndef bill_order():\n    create_order()\n    return Order()\n",
            ),
            (
                "storage/repo.py",
                "from domain.order import Order\nfrom billing.charge import bill_order\ndef save_order():\n    bill_order()\n    return Order()\n",
            ),
            ("worker/job.py", &god_src),
            (
                "cli/main.py",
                "from worker.job import orchestrate\nfrom storage.repo import save_order\ndef main():\n    orchestrate()\n    save_order()\n",
            ),
        ]);

        let names = hub_names(&index, 2400);
        let pos_order = names
            .iter()
            .position(|n| n == "Order")
            .expect("Order must appear in hubs");
        let pos_util = names
            .iter()
            .position(|n| n == "fmt_cents")
            .expect("fmt_cents must appear in hubs");
        let pos_god = names
            .iter()
            .position(|n| n == "orchestrate")
            .expect("orchestrate must appear in hubs");
        assert!(
            pos_order < pos_util,
            "cross-area hub Order (#{pos_order}) should outrank 50-caller single-file utility fmt_cents (#{pos_util}): {names:?}"
        );
        assert!(
            pos_order < pos_god,
            "cross-area hub Order (#{pos_order}) should outrank high-out-degree orchestrate (#{pos_god}): {names:?}"
        );
    }

    #[test]
    fn weighted_centrality_propagates_authority_through_hubs() {
        // `core_target` and `leaf_target` both have exactly 2 callers in the same
        // area (`svc/`), each from 2 distinct files. `core_target`'s callers are
        // themselves important hubs referenced across 3 other areas; `leaf_target`'s
        // callers are unreferenced leaves.
        let (_dir, index) = write_repo(&[
            (
                "svc/targets.py",
                "def core_target():\n    return 1\ndef leaf_target():\n    return 2\n",
            ),
            (
                "svc/hub_a.py",
                "from svc.targets import core_target\ndef hub_a():\n    return core_target()\n",
            ),
            (
                "svc/hub_b.py",
                "from svc.targets import core_target\ndef hub_b():\n    return core_target()\n",
            ),
            (
                "svc/leaf_a.py",
                "from svc.targets import leaf_target\ndef leaf_a():\n    return leaf_target()\n",
            ),
            (
                "svc/leaf_b.py",
                "from svc.targets import leaf_target\ndef leaf_b():\n    return leaf_target()\n",
            ),
            (
                "api/routes.py",
                "from svc.hub_a import hub_a\nfrom svc.hub_b import hub_b\ndef route():\n    return hub_a() + hub_b()\n",
            ),
            (
                "jobs/sync.py",
                "from svc.hub_a import hub_a\nfrom svc.hub_b import hub_b\ndef sync():\n    return hub_a() + hub_b()\n",
            ),
            (
                "cli/cmd.py",
                "from svc.hub_a import hub_a\nfrom svc.hub_b import hub_b\ndef main():\n    return hub_a() + hub_b()\n",
            ),
        ]);

        let names = hub_names(&index, 2400);
        let pos_core = names
            .iter()
            .position(|n| n == "core_target")
            .expect("core_target must appear in hubs");
        let pos_leaf = names
            .iter()
            .position(|n| n == "leaf_target")
            .expect("leaf_target must appear in hubs");
        assert!(
            pos_core < pos_leaf,
            "hub-referenced core_target (#{pos_core}) must outrank leaf-referenced leaf_target (#{pos_leaf}): {names:?}"
        );
    }

    #[test]
    fn same_pair_cross_area_links_collapse_preserving_languages_and_counts() {
        // Two different languages in `frontend/` (TypeScript + JavaScript) both call
        // the Python route in `backend/`. They must collapse into ONE `frontend/ → backend/`
        // row while preserving all language tags, exact counts, and `possible` markers.
        let (_dir, index) = write_repo(&[
            (
                "frontend/orders.ts",
                "export async function fetchOrders() {\n  return fetch(\"/api/orders\");\n}\n",
            ),
            (
                "frontend/admin.js",
                "export async function adminOrders() {\n  return fetch(\"/api/orders\");\n}\n",
            ),
            (
                "backend/orders.py",
                "from fastapi import APIRouter\nrouter = APIRouter()\n@router.get(\"/api/orders\")\ndef list_orders():\n    return database.execute(\"SELECT id FROM purchase_orders\")\n",
            ),
            (
                "db/schema.sql",
                "CREATE TABLE purchase_orders (\n  id INTEGER PRIMARY KEY\n);\n",
            ),
        ]);

        let (_, sections, _) = build(&index, 2400);
        let links = sections
            .iter()
            .find(|s| s.key == "cross_area_links")
            .unwrap();
        let fe_to_be: Vec<&String> = links
            .lines
            .iter()
            .filter(|line| line.starts_with("  frontend/ → backend/"))
            .collect();
        assert_eq!(
            fe_to_be.len(),
            1,
            "expected deduplicated frontend/ → backend/ row, got:\n{}",
            links.lines.join("\n")
        );
        let row = fe_to_be[0];
        assert!(row.contains("JavaScript"), "{row}");
        assert!(row.contains("TypeScript"), "{row}");
        assert!(row.contains("→ Python"), "{row}");
        assert!(row.contains("possible HTTP route 2"), "{row}");
        // Single-pair backend/ → db/ preserves the exact canonical format.
        let be_to_db = links
            .lines
            .iter()
            .find(|line| line.starts_with("  backend/ → db/"))
            .expect("missing backend/ → db/");
        assert!(
            be_to_db.contains("Python → SQL · possible SQL relation 1"),
            "{be_to_db}"
        );
    }

    #[test]
    fn tight_and_generous_budgets_pack_high_signal_and_deduplicate_prefixes() {
        // Multi-area workspace with nested paths, single-file communities, entrypoints,
        // and many symbols to exercise tight truncation (`… +N more`), nested path
        // prefix deduplication, and generous budget utilization.
        let mut billing_core = String::new();
        for i in 0..10 {
            billing_core.push_str(&format!(
                "def calc_tier_{i:02}(x):\n    return calc_base(x) + {i}\n"
            ));
        }
        billing_core.push_str("def calc_base(x):\n    return x * 2\n");

        let mut billing_tests = String::from("from services.billing.core.rates import calc_base\n");
        for i in 0..6 {
            billing_tests.push_str(&format!(
                "def test_rate_{i:02}():\n    assert calc_base({i}) == {}\n",
                i * 2
            ));
        }

        let (_dir, index) = write_repo(&[
            ("services/billing/core/rates.py", &billing_core),
            (
                "services/billing/core/invoice.py",
                "from services.billing.core.rates import calc_base\ndef build_invoice(n):\n    return calc_base(n)\n",
            ),
            ("services/billing/tests/test_rates.py", &billing_tests),
            (
                "services/orders/api/handler.py",
                "from services.billing.core.invoice import build_invoice\nfrom services.orders.db.store import store_order\ndef handle_order(n):\n    return store_order(build_invoice(n))\ndef handle_refund(n):\n    return store_order(build_invoice(-n))\n",
            ),
            (
                "services/orders/db/store.py",
                "def store_order(total):\n    return total\n",
            ),
            (
                "services/audit/log.py",
                "def emit_event(kind):\n    return format_event(kind)\ndef format_event(kind):\n    return kind\n",
            ),
            (
                "cli/app.py",
                "from services.orders.api.handler import handle_order\ndef main():\n    return handle_order(42)\n",
            ),
        ]);

        // Tight budget (1200 B): must stay strictly within budget, be deterministic,
        // surface areas/hubs/entrypoints/links/tests, and report exact `… +N more`.
        let tight = render_map(&index, 1200);
        assert_eq!(tight, render_map(&index, 1200), "map must be deterministic");
        assert!(tight.len() <= 1200, "{} bytes\n{tight}", tight.len());
        assert!(tight.contains("Areas · "), "{tight}");
        assert!(tight.contains("Hubs · "), "{tight}");
        assert!(tight.contains("Entry points · "), "{tight}");
        assert!(tight.contains("Cross-area links · "), "{tight}");
        assert!(tight.contains("Tests · "), "{tight}");
        assert!(
            tight.contains("  … +"),
            "expected `… +N more` marker:\n{tight}"
        );
        // Nested test directory deduplicates its repeated directory prefix.
        assert!(
            tight.contains("./test_rates.py:"),
            "expected nested test dir prefix deduplication:\n{tight}"
        );

        // Generous budget (3600 B): utilizes extra room to surface secondary hubs and
        // cross-file uncalled entry points (`handle_refund` with out = 2).
        let wide = render_map(&index, 3600);
        assert!(wide.len() <= 3600, "{} bytes\n{wide}", wide.len());
        assert!(
            wide.len() > tight.len(),
            "generous budget should surface more signal ({} vs {} bytes)",
            wide.len(),
            tight.len()
        );
        assert!(
            wide.contains("handle_refund") && wide.contains("uncalled · out 2"),
            "generous budget should surface cross-file uncalled entry point:\n{wide}"
        );
        // Nested areas, single-file community, and same-file multi-file community
        // leader all deduplicate repeated paths cleanly.
        assert!(
            wide.contains("services/billing/core/ · 2 files · 12 symbols · Python · hub calc_base ./rates.py:21-22"),
            "nested area should deduplicate repeated directory prefix:\n{wide}"
        );
        assert!(
            wide.contains("services/audit/log.py · 2 symbols · format_event :3-4, emit_event :1-2"),
            "single-file community should deduplicate repeated file path:\n{wide}"
        );
        assert!(
            wide.contains("calc_base services/billing/core/rates.py:21-22, calc_tier_00 :1-2"),
            "multi-file community should deduplicate same-file second leader:\n{wide}"
        );

        // Smoke-check format_language_pairs directly for multi-target grouping.
        let mut pairs = BTreeMap::new();
        pairs.insert(("Astro", "TypeScript"), 3);
        pairs.insert(("Svelte", "TypeScript"), 3);
        pairs.insert(("Vue", "TypeScript"), 3);
        pairs.insert(("Python", "SQL"), 1);
        assert_eq!(
            format_language_pairs(&pairs),
            " · Astro/Svelte/Vue → TypeScript, Python → SQL"
        );
    }
}
