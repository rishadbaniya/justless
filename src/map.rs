//! `justless map`: a budgeted orientation report built from the repository
//! index. It reads no source files, so its cost is the index refresh plus a
//! few linear passes over symbols and links.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::Path;

use anyhow::Result;
use clap::{Arg, ArgMatches, Command};
use serde_json::{Map, Value, json};

use crate::context::{Report, relationship};
use crate::index::{Definition, Edge, Index};
use crate::polyglot::Grammar;
use crate::rust_syntax::Kind;
use crate::summary::short;
use crate::workspace::{Location, Workspace, display_path, shell_path};

const NAME_BYTES: usize = 80;
const RANKED: usize = 48;
const MAX_RANKED: usize = 256;

/// Priority order when fitting `map` sections into a tight budget without bridges:
/// areas, hubs (scheduled twice per round so core types fill out the architecture map),
/// entry points, cross-area links, tests, then communities. Visual rendering
/// still follows the natural section order.
const MAP_ORDER: [usize; 7] = [0, 1, 4, 3, 5, 1, 2];

/// Priority order when a non-empty `Bridges` section sits at index 3 (between
/// communities and cross-area links).
const MAP_ORDER_WITH_BRIDGES: [usize; 8] = [0, 1, 5, 4, 3, 6, 1, 2];

/// Priority order for `map --explain SYMBOL`: cross-community bridges, callers,
/// dependencies, then reachable tests.
const EXPLAIN_ORDER: [usize; 4] = [0, 1, 2, 3];

pub fn command() -> Command<'static> {
    Command::new("map")
        .about("Orient in the repository: areas, hubs, communities, bridges, cross-area links, entry points and tests")
        .arg(crate::budget().alias("max-bytes"))
        .arg(
            Arg::new("explain")
                .long("explain")
                .takes_value(true)
                .value_name("SYMBOL")
                .help("Explain one symbol's architectural role, centrality, community, bridges and impact"),
        )
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
    if let Some(symbol) = matches.value_of("explain") {
        return explain(workspace, &index, symbol, budget, json);
    }
    let (header, sections, order, summary) = build(&index, budget);
    // `--root` for `expand` also serves `query` and `affected`.
    let footer = [format!(
        "Next: {} expand FILE NAME, query NAME or affected NAME",
        workspace.invocation().files
    )];
    emit(
        &header,
        &sections,
        &footer,
        order,
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

/// `name rel:start-end` when `dir` is a nested directory prefix already shown
/// on the same line; otherwise the full `spot`.
fn spot_under_dir(definition: &Definition, dir: &str) -> String {
    if definition.kind != "file"
        && is_nested_dir(dir)
        && let Some(rel) = definition.location.file.strip_prefix(dir)
    {
        let rel = rel.trim_start_matches("./").trim_start_matches('/');
        if !rel.is_empty() {
            return format!(
                "{} {}{}",
                short(name(definition), NAME_BYTES),
                display_path(rel),
                line_span(&definition.location)
            );
        }
    }
    spot(definition)
}

/// `name [kind] rel:start-end` when `dir` is a nested directory prefix already
/// shown on the same line; otherwise the full `label`.
fn label_under_dir(definition: &Definition, dir: &str) -> String {
    if definition.kind != "file"
        && is_nested_dir(dir)
        && let Some(rel) = definition.location.file.strip_prefix(dir)
    {
        let rel = rel.trim_start_matches("./").trim_start_matches('/');
        if !rel.is_empty() {
            return format!(
                "{} [{}] {}{}",
                short(name(definition), NAME_BYTES),
                kind(definition),
                display_path(rel),
                line_span(&definition.location)
            );
        }
    }
    label(definition)
}

/// Formats an integer with ASCII thousands separators (`8321` → `"8,321"`).
fn format_count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in digits.chars().enumerate() {
        let rem = digits.len() - idx;
        if idx > 0 && rem.is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
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

/// Minimum symbol count for a single-file cluster to qualify as an architectural
/// community (multi-file clusters always qualify once they have >= 2 files).
const MIN_SINGLE_FILE_COMMUNITY: usize = 5;

struct Community<'a> {
    members: Vec<usize>,
    files: BTreeMap<&'a str, usize>,
    name: String,
    support_only: bool,
}

struct CommunityMap<'a> {
    groups: Vec<Community<'a>>,
    /// Maps each definition id to its owning architectural community index.
    of: Vec<Option<usize>>,
    /// Non-file definitions not belonging to any architectural community.
    isolated_symbols: usize,
}

/// Asynchronous label propagation in a fixed order with the smallest label
/// winning ties, followed by deterministic satellite consolidation and
/// filtering to genuine architectural communities (>= 2 files OR >= 5 symbols).
/// Isolated / leaf symbols are counted cleanly instead of inflating communities.
fn communities(index: &Index) -> CommunityMap<'_> {
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

    let mut raw_groups = BTreeMap::<usize, Vec<usize>>::new();
    let mut total_non_file = 0usize;
    for (id, definition) in definitions.iter().enumerate() {
        if definition.kind != "file" {
            total_non_file += 1;
            raw_groups.entry(labels[id]).or_default().push(id);
        }
    }
    let mut groups: Vec<Vec<usize>> = raw_groups.into_values().collect();
    groups.sort_by_key(|members| members[0]);

    let single_file_of = |members: &[usize]| -> Option<&str> {
        let first = definitions[*members.first()?].location.file.as_str();
        members
            .iter()
            .all(|&id| definitions[id].location.file.as_str() == first)
            .then_some(first)
    };
    let is_architectural = |members: &[usize]| -> bool {
        members.len() >= MIN_SINGLE_FILE_COMMUNITY
            || (!members.is_empty() && single_file_of(members).is_none())
    };
    let is_test_or_module_only = |members: &[usize]| -> bool {
        members
            .iter()
            .all(|&id| definitions[id].test || definitions[id].kind == "module")
    };

    // Pass 1: attach small single-file satellite groups (< 5 symbols) to a
    // neighboring architectural community when at least half of their certain
    // external links point to that one community. Inferred edges are never
    // merged so cross-boundary HTTP/SQL bridges stay distinct.
    for _ in 0..2 {
        let mut group_of = vec![None; definitions.len()];
        for (g_idx, members) in groups.iter().enumerate() {
            for &id in members {
                group_of[id] = Some(g_idx);
            }
        }
        let mut ext_weights = BTreeMap::<usize, BTreeMap<usize, u32>>::new();
        for edge in &index.edges {
            if edge.inferred {
                continue;
            }
            let Some(from) = edge.from else {
                continue;
            };
            let (Some(g1), Some(g2)) = (group_of[from], group_of[edge.to]) else {
                continue;
            };
            if g1 == g2 {
                continue;
            }
            *ext_weights.entry(g1).or_default().entry(g2).or_default() += 1;
            *ext_weights.entry(g2).or_default().entry(g1).or_default() += 1;
        }
        let mut candidates: Vec<usize> = (0..groups.len())
            .filter(|&g| {
                !groups[g].is_empty()
                    && groups[g].len() < MIN_SINGLE_FILE_COMMUNITY
                    && single_file_of(&groups[g]).is_some()
            })
            .collect();
        candidates.sort_by_key(|&g| (groups[g].len(), groups[g][0]));

        let mut merged_any = false;
        for g in candidates {
            let Some(neighbors) = ext_weights.get(&g) else {
                continue;
            };
            let total: u32 = neighbors.values().sum();
            if total == 0 {
                continue;
            }
            let best = neighbors
                .iter()
                .filter(|&(&other, _)| {
                    !groups[other].is_empty() && is_architectural(&groups[other])
                })
                .max_by_key(|&(&other, &w)| (w, groups[other].len(), Reverse(groups[other][0])));
            if let Some((&target, &best_w)) = best
                && best_w.saturating_mul(2) >= total
            {
                let moved = std::mem::take(&mut groups[g]);
                groups[target].extend(moved);
                groups[target].sort_unstable();
                merged_any = true;
            }
        }
        if !merged_any {
            break;
        }
    }

    // Pass 2: consolidate remaining single-file sub-clusters (e.g. inline
    // `mod tests` or local helper cliques split off by LPA) into their file's
    // dominant community so one file does not emit multiple duplicate
    // single-file communities while preserving genuine multi-file communities.
    let mut dominant_by_file = BTreeMap::<&str, (bool, bool, usize, usize, usize)>::new();
    for (g_idx, members) in groups.iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        let arch = is_architectural(members);
        let test_only = is_test_or_module_only(members);
        let mut counts = BTreeMap::<&str, usize>::new();
        for &id in members {
            *counts
                .entry(definitions[id].location.file.as_str())
                .or_default() += 1;
        }
        for (file, in_file) in counts {
            let candidate = (
                arch,
                !test_only,
                in_file,
                members.len(),
                usize::MAX - members[0],
            );
            dominant_by_file
                .entry(file)
                .and_modify(|best| {
                    let best_key = (
                        best.0,
                        best.1,
                        best.2,
                        best.3,
                        usize::MAX - groups[best.4][0],
                    );
                    if candidate > best_key {
                        *best = (arch, !test_only, in_file, members.len(), g_idx);
                    }
                })
                .or_insert((arch, !test_only, in_file, members.len(), g_idx));
        }
    }
    let mut same_file_merges = Vec::<(usize, usize)>::new();
    for (g_idx, members) in groups.iter().enumerate() {
        let Some(file) = single_file_of(members) else {
            continue;
        };
        let Some(&(_, _, _, _, target)) = dominant_by_file.get(file) else {
            continue;
        };
        if target != g_idx {
            same_file_merges.push((g_idx, target));
        }
    }
    for (from_g, to_g) in same_file_merges {
        let moved = std::mem::take(&mut groups[from_g]);
        groups[to_g].extend(moved);
        groups[to_g].sort_unstable();
    }

    let mut architectural = Vec::<Community<'_>>::new();
    let mut accounted = 0usize;
    for mut members in groups {
        if members.is_empty() {
            continue;
        }
        members.sort_unstable();
        if !is_architectural(&members) {
            continue;
        }
        accounted += members.len();
        let mut comm_files = BTreeMap::<&str, usize>::new();
        for &id in &members {
            *comm_files
                .entry(definitions[id].location.file.as_str())
                .or_default() += 1;
        }
        let name = community_name(&comm_files);
        let support_only = members.iter().all(|&id| {
            let def = &definitions[id];
            def.test || is_support_file(&def.location.file)
        });
        architectural.push(Community {
            members,
            files: comm_files,
            name,
            support_only,
        });
    }
    architectural.sort_by_key(|c| (c.support_only, Reverse(c.members.len()), c.members[0]));

    let mut of = vec![None; definitions.len()];
    for (idx, community) in architectural.iter().enumerate() {
        for &id in &community.members {
            of[id] = Some(idx);
        }
    }
    CommunityMap {
        groups: architectural,
        of,
        isolated_symbols: total_non_file.saturating_sub(accounted),
    }
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
    communities: CommunityMap<'a>,
    ranked_limit: usize,
}

impl<'a> Graph<'a> {
    fn new(index: &'a Index, budget: usize) -> Self {
        let degrees = Degrees::new(index);
        let areas = areas(index);
        let centrality = Centrality::new(index, &areas);
        let communities = communities(index);
        let ranked_limit = (budget / 40).clamp(RANKED, MAX_RANKED);
        Self {
            index,
            degrees,
            centrality,
            areas,
            communities,
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

fn build(
    index: &Index,
    budget: usize,
) -> (
    Vec<String>,
    Vec<Section>,
    &'static [usize],
    Map<String, Value>,
) {
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
    summary.insert(
        "isolated_symbols".into(),
        json!(graph.communities.isolated_symbols),
    );
    let bridges = bridge_section(&graph);
    let (sections, order): (Vec<Section>, &'static [usize]) = if bridges.total() > 0 {
        (
            vec![
                area_section(&graph),
                hub_section(&graph),
                community_section(&graph),
                bridges,
                link_section(&graph),
                entry_section(&graph),
                test_section(&graph),
            ],
            &MAP_ORDER_WITH_BRIDGES,
        )
    } else {
        summary.insert("bridges".into(), json!([]));
        (
            vec![
                area_section(&graph),
                hub_section(&graph),
                community_section(&graph),
                link_section(&graph),
                entry_section(&graph),
                test_section(&graph),
            ],
            &MAP_ORDER,
        )
    };
    (header, sections, order, summary)
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
                    let rel = rel.trim_start_matches("./").trim_start_matches('/');
                    if rel.is_empty() {
                        format!("{first}:1")
                    } else {
                        format!("{}:1", display_path(rel))
                    }
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
    let groups = &graph.communities.groups;
    let isolated = graph.communities.isolated_symbols;
    let title = if groups.is_empty() {
        if isolated > 0 {
            format!(
                "Communities · none · {} isolated",
                plural(isolated, "symbol")
            )
        } else {
            "Communities · none".into()
        }
    } else if isolated > 0 {
        format!("Communities · {} · {isolated} isolated", groups.len())
    } else {
        format!("Communities · {}", groups.len())
    };
    let mut section = Section::new("communities", title, 2);
    let limit = graph.ranked_limit;
    section.unlisted = groups.len().saturating_sub(limit);
    for community in groups.iter().take(limit) {
        let members = &community.members;
        let files = &community.files;
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
        let title = &community.name;
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
    section
}

struct Bridge<'a> {
    edge: &'a Edge,
    from_id: usize,
    to_id: usize,
    from_comm: usize,
    to_comm: usize,
    from_lang: &'static str,
    to_lang: &'static str,
    pair_links: usize,
    score: f64,
    support_tier: usize,
}

/// Surprising edges connecting distinct architectural communities across
/// different directories or languages, ranked by pair rarity, target
/// specificity, community scale, centrality and boundary crossing.
fn find_bridges<'a>(graph: &'a Graph<'a>) -> Vec<Bridge<'a>> {
    let mut pair_counts = BTreeMap::<(usize, usize), usize>::new();
    let mut target_comm_fanin = vec![BTreeSet::<usize>::new(); graph.index.definitions.len()];
    let mut qualifying = Vec::<(&'a Edge, usize, usize, usize, &'static str, &'static str)>::new();

    for edge in &graph.index.edges {
        let Some(from_id) = edge.from else {
            continue;
        };
        if from_id == edge.to {
            continue;
        }
        let from_def = graph.definition(from_id);
        let to_def = graph.definition(edge.to);
        if matches!(from_def.kind.as_str(), "file" | "module")
            || matches!(to_def.kind.as_str(), "file" | "module")
            || from_def.test
            || to_def.test
        {
            continue;
        }
        let (Some(c1), Some(c2)) = (graph.communities.of[from_id], graph.communities.of[edge.to])
        else {
            continue;
        };
        if c1 == c2 {
            continue;
        }
        let from_file = from_def.location.file.as_str();
        let to_file = to_def.location.file.as_str();
        let from_lang = indexed_language(graph.index, from_file).unwrap_or("text");
        let to_lang = indexed_language(graph.index, to_file).unwrap_or("text");
        if directory(from_file) == directory(to_file) && from_lang == to_lang {
            continue;
        }
        let pair = (c1.min(c2), c1.max(c2));
        *pair_counts.entry(pair).or_default() += 1;
        target_comm_fanin[edge.to].insert(c1);
        qualifying.push((edge, from_id, c1, c2, from_lang, to_lang));
    }

    let mut best_by_pair = BTreeMap::<(usize, usize), Bridge<'a>>::new();
    for (edge, from_id, c1, c2, from_lang, to_lang) in qualifying {
        let pair = (c1.min(c2), c1.max(c2));
        let pair_links = pair_counts[&pair];
        let rarity = 1.0 / (pair_links as f64).powf(0.75);
        let fanin = target_comm_fanin[edge.to].len().max(1) as f64;
        let specificity = 1.0 / fanin.sqrt();
        let c1_len = graph.communities.groups[c1].members.len() as f64;
        let c2_len = graph.communities.groups[c2].members.len() as f64;
        let comm_scale = (c1_len.ln_1p() * c2_len.ln_1p()).sqrt();
        let centrality =
            1.0 + (graph.centrality.score[from_id] + graph.centrality.score[edge.to]).ln_1p();
        let from_file = graph.definition(from_id).location.file.as_str();
        let to_file = graph.definition(edge.to).location.file.as_str();
        let boundary = if from_lang != to_lang {
            3.0
        } else if graph.area(from_file) != graph.area(to_file) {
            2.0
        } else {
            1.3
        };
        let confidence = if !edge.inferred {
            1.0
        } else if matches!(edge.kind, Kind::Route | Kind::Data) {
            0.85
        } else {
            0.35
        };
        let score = rarity * specificity * comm_scale * centrality * boundary * confidence;
        let support_tier =
            usize::from(is_support_file(from_file)) + usize::from(is_support_file(to_file));
        let candidate = Bridge {
            edge,
            from_id,
            to_id: edge.to,
            from_comm: c1,
            to_comm: c2,
            from_lang,
            to_lang,
            pair_links,
            score,
            support_tier,
        };
        let rank_of = |b: &Bridge<'a>| {
            (
                b.support_tier,
                Reverse(Centrality::quantize(b.score)),
                b.from_lang == b.to_lang,
                b.edge.inferred,
                kind_rank(b.edge.kind),
                &b.edge.location,
                b.to_id,
            )
        };
        match best_by_pair.get(&pair) {
            Some(existing) if rank_of(existing) <= rank_of(&candidate) => {}
            _ => {
                best_by_pair.insert(pair, candidate);
            }
        }
    }

    let mut bridges: Vec<Bridge<'a>> = best_by_pair.into_values().collect();
    bridges.sort_by_key(|b| {
        (
            b.support_tier,
            Reverse(Centrality::quantize(b.score)),
            b.from_lang == b.to_lang,
            b.edge.inferred,
            kind_rank(b.edge.kind),
            &b.edge.location,
            b.to_id,
        )
    });
    bridges
}

fn bridge_section(graph: &Graph) -> Section {
    let bridges = find_bridges(graph);
    let mut section = Section::new("bridges", format!("Bridges · {}", bridges.len()), 2);
    let limit = graph.ranked_limit;
    section.unlisted = bridges.len().saturating_sub(limit);
    for bridge in bridges.iter().take(limit) {
        let from_def = graph.definition(bridge.from_id);
        let to_def = graph.definition(bridge.to_id);
        let origin = format!(
            "{} {}",
            short(name(from_def), NAME_BYTES),
            bridge.edge.location
        );
        let target = spot(to_def);
        let langs = if bridge.from_lang != bridge.to_lang {
            format!(" · {} → {}", bridge.from_lang, bridge.to_lang)
        } else {
            String::new()
        };
        let conf = if bridge.edge.inferred {
            "possible "
        } else {
            ""
        };
        let links_note = if bridge.pair_links > 1 {
            format!(" · {}", plural(bridge.pair_links, "link"))
        } else {
            String::new()
        };
        section.push(
            format!(
                "  {origin} → {target}{langs} · {conf}{}{links_note}",
                relationship(bridge.edge.kind)
            ),
            json!({
                "from": node(from_def),
                "to": node(to_def),
                "at": bridge.edge.location.to_string(),
                "kind": relationship(bridge.edge.kind),
                "possible": bridge.edge.inferred,
                "from_language": bridge.from_lang,
                "to_language": bridge.to_lang,
                "from_community": graph.communities.groups[bridge.from_comm].name,
                "to_community": graph.communities.groups[bridge.to_comm].name,
                "pair_links": bridge.pair_links,
                "score": (bridge.score * 1000.0).round() / 1000.0,
            }),
        );
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
                    let rel = rel.trim_start_matches("./").trim_start_matches('/');
                    if rel.is_empty() {
                        format!("{first}:1")
                    } else {
                        format!("{}:1", display_path(rel))
                    }
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

// ---------------------------------------------------------------------------
// Architectural node explainer: `justless map --explain SYMBOL`

struct BlastRadius {
    hop1_dependents: usize,
    hop3_dependents: usize,
    reachable_tests: Vec<(usize, usize, bool)>,
}

/// Reverse BFS over incoming edges up to `max_depth` hops to compute 1-hop and
/// 3-hop blast-radius counts plus all reachable tests within `max_depth` hops.
fn blast_radius(index: &Index, id: usize, max_depth: usize) -> BlastRadius {
    let mut visited = BTreeMap::<usize, (usize, bool)>::new();
    let mut file_scopes_1 = BTreeSet::<&str>::new();
    let mut file_scopes_all = BTreeSet::<&str>::new();
    let mut queue = VecDeque::from([(id, 0usize, false)]);

    while let Some((curr, depth, path_inferred)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        let next_depth = depth + 1;
        for edge in index.incoming(curr) {
            let next_inferred = path_inferred || edge.inferred;
            match edge.from {
                Some(from) => {
                    if from == id {
                        continue;
                    }
                    let update = match visited.get(&from) {
                        None => true,
                        Some(&(prev_depth, prev_inferred)) => {
                            next_depth < prev_depth
                                || (next_depth == prev_depth && prev_inferred && !next_inferred)
                        }
                    };
                    if update {
                        visited.insert(from, (next_depth, next_inferred));
                        queue.push_back((from, next_depth, next_inferred));
                    }
                }
                None => {
                    let file = edge.location.file.as_str();
                    if next_depth == 1 {
                        file_scopes_1.insert(file);
                    }
                    file_scopes_all.insert(file);
                }
            }
        }
    }

    let mut hop1_files = BTreeSet::<&str>::new();
    let mut hop3_files = BTreeSet::<&str>::new();
    let mut hop1_syms = 0usize;
    let mut hop3_syms = 0usize;
    let mut reachable_tests = Vec::<(usize, usize, bool)>::new();

    for (&dep_id, &(hops, inferred)) in &visited {
        let def = &index.definitions[dep_id];
        if hops == 1 {
            hop1_syms += 1;
            hop1_files.insert(def.location.file.as_str());
        }
        hop3_syms += 1;
        hop3_files.insert(def.location.file.as_str());
        if def.test {
            reachable_tests.push((dep_id, hops, inferred));
        }
    }
    reachable_tests.sort_by_key(|&(test_id, hops, inferred)| {
        (
            hops,
            inferred,
            &index.definitions[test_id].location,
            test_id,
        )
    });

    let extra_1 = file_scopes_1
        .iter()
        .filter(|file| !hop1_files.contains(**file))
        .count();
    let extra_3 = file_scopes_all
        .iter()
        .filter(|file| !hop3_files.contains(**file))
        .count();

    BlastRadius {
        hop1_dependents: hop1_syms + extra_1,
        hop3_dependents: hop3_syms + extra_3,
        reachable_tests,
    }
}

/// 1-based PageRank rank of `id` among all non-file definitions (or all
/// definitions if `id` is a file definition).
fn symbol_rank(graph: &Graph, id: usize) -> (usize, usize) {
    let def = graph.definition(id);
    let include_files = def.kind == "file";
    let target_key = graph.rank(id);
    let mut better = 0usize;
    let mut total = 0usize;
    for (other_id, other_def) in graph.index.definitions.iter().enumerate() {
        if !include_files && other_def.kind == "file" {
            continue;
        }
        total += 1;
        if other_id != id && graph.rank(other_id) < target_key {
            better += 1;
        }
    }
    (better + 1, total.max(1))
}

#[allow(clippy::too_many_arguments)]
fn classify_role(
    graph: &Graph,
    id: usize,
    rank: usize,
    total: usize,
    bridges_count: usize,
    cross_lang_bridge: bool,
    external_comms: usize,
) -> &'static str {
    let def = graph.definition(id);
    if def.test {
        return "test";
    }
    let last = name(def).rsplit("::").next().unwrap_or_default();
    let deg_in = graph.degrees.dependents[id];
    let deg_out = graph.degrees.dependencies[id];
    let is_main =
        matches!(def.kind.as_str(), "function" | "method") && matches!(last, "main" | "Main");
    let is_route = graph.index.routes().any(|(_, route_id)| route_id == id);
    let is_orchestrator = central(def)
        && matches!(def.kind.as_str(), "function" | "method")
        && deg_in == 0
        && deg_out >= 2
        && !is_test_file(&def.location.file);
    if is_main || is_route || is_orchestrator {
        return "entry point";
    }
    let is_hub = central(def)
        && (graph.centrality.caller_areas[id] >= 2
            || (graph.centrality.caller_files[id] >= 3 && deg_in >= 5)
            || (deg_in >= 3 && rank <= (total / 20).max(3) && graph.centrality.score[id] > 0.0));
    if is_hub && !(deg_in < 5 && cross_lang_bridge && deg_out > 0) {
        return "hub";
    }
    if bridges_count > 0
        && (cross_lang_bridge || (deg_in > 0 && deg_out > 0) || external_comms >= 2)
    {
        return "bridge";
    }
    if deg_out == 0 || (deg_in == 0 && deg_out <= 1) {
        return "leaf";
    }
    "internal"
}

fn edge_rank(edge: &Edge) -> (bool, u8, &Location) {
    (edge.inferred, kind_rank(edge.kind), &edge.location)
}

fn build_explain<'a>(
    index: &'a Index,
    id: usize,
    budget: usize,
) -> (Vec<String>, Vec<Section>, Map<String, Value>) {
    let graph = Graph::new(index, budget);
    let def = graph.definition(id);
    let (rank, total_symbols) = symbol_rank(&graph, id);
    let blast = blast_radius(index, id, 3);
    let self_file = def.location.file.as_str();
    let self_dir = directory(self_file);
    let self_lang = indexed_language(index, self_file).unwrap_or("text");
    let self_comm = graph.communities.of[id];

    // Cross-community & cross-language bridges incident to `id`.
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum Dir {
        Out,
        In,
    }
    struct NodeBridge<'b> {
        dir: Dir,
        other: usize,
        edge: &'b Edge,
        sites: usize,
        other_lang: &'static str,
        other_comm: Option<usize>,
    }
    let mut bridge_map = BTreeMap::<(Dir, usize), NodeBridge<'a>>::new();
    let mut record_bridge = |dir: Dir, other: usize, edge: &'a Edge| {
        if other == id {
            return;
        }
        let other_def = graph.definition(other);
        if matches!(other_def.kind.as_str(), "file" | "module") || other_def.test {
            return;
        }
        let other_file = other_def.location.file.as_str();
        let other_lang = indexed_language(index, other_file).unwrap_or("text");
        let other_comm = graph.communities.of[other];
        let crosses_lang = other_lang != self_lang;
        let crosses_comm = match (self_comm, other_comm) {
            (Some(c1), Some(c2)) => c1 != c2,
            (Some(_), None) | (None, Some(_)) => directory(other_file) != self_dir,
            (None, None) => false,
        };
        if !crosses_lang && !crosses_comm {
            return;
        }
        bridge_map
            .entry((dir, other))
            .and_modify(|entry| {
                entry.sites += 1;
                if edge_rank(edge) < edge_rank(entry.edge) {
                    entry.edge = edge;
                }
            })
            .or_insert(NodeBridge {
                dir,
                other,
                edge,
                sites: 1,
                other_lang,
                other_comm,
            });
    };
    for edge in index.outgoing(id) {
        record_bridge(Dir::Out, edge.to, edge);
    }
    for edge in index.incoming(id) {
        if let Some(from) = edge.from {
            record_bridge(Dir::In, from, edge);
        }
    }
    let mut node_bridges: Vec<NodeBridge<'_>> = bridge_map.into_values().collect();
    node_bridges.sort_by_key(|b| {
        (
            b.other_lang == self_lang,
            b.edge.inferred,
            kind_rank(b.edge.kind),
            graph.rank(b.other),
        )
    });
    let cross_lang_bridge = node_bridges.iter().any(|b| b.other_lang != self_lang);
    let external_comms = node_bridges
        .iter()
        .filter_map(|b| b.other_comm)
        .filter(|&c| Some(c) != self_comm)
        .collect::<BTreeSet<_>>()
        .len();

    let role = classify_role(
        &graph,
        id,
        rank,
        total_symbols,
        node_bridges.len(),
        cross_lang_bridge,
        external_comms,
    );

    // Header lines: Identity & Role, Centrality & Blast Radius, Owning Community.
    let mut header = vec![
        format!(
            "Explain · {} [{}] {} · role: {role}",
            def.qualified,
            kind(def),
            def.location
        ),
        format!(
            "Centrality · rank #{} of {} · {} · blast: {} (1-hop), {} (3-hop), {}",
            format_count(rank),
            format_count(total_symbols),
            graph.degrees.describe(id),
            plural(blast.hop1_dependents, "dependent"),
            plural(blast.hop3_dependents, "dependent"),
            plural(blast.reachable_tests.len(), "test"),
        ),
    ];

    let community_json = match self_comm {
        Some(comm_idx) => {
            let comm = &graph.communities.groups[comm_idx];
            let mut peers: Vec<usize> = comm
                .members
                .iter()
                .copied()
                .filter(|&member| member != id && central(graph.definition(member)))
                .collect();
            peers.sort_by_key(|&member| graph.rank(member));
            peers.truncate(3);
            let peer_text = if peers.is_empty() {
                String::new()
            } else {
                let names = peers
                    .iter()
                    .map(|&peer| short(name(graph.definition(peer)), NAME_BYTES))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(" · peers: {names}")
            };
            header.push(format!(
                "Community · {} · {}, {}{peer_text}",
                comm.name,
                plural(comm.members.len(), "symbol"),
                plural(comm.files.len(), "file"),
            ));
            json!({
                "name": comm.name,
                "symbols": comm.members.len(),
                "files": comm.files.keys().collect::<Vec<_>>(),
                "peers": peers.iter().map(|&peer| node(graph.definition(peer))).collect::<Vec<_>>(),
            })
        }
        None => {
            header.push(format!("Community · isolated ({self_file})"));
            Value::Null
        }
    };

    // Section 0: Cross-community bridges.
    let mut bridges_section = Section::new(
        "bridges",
        if node_bridges.is_empty() {
            "Cross-community bridges · none".into()
        } else {
            format!("Cross-community bridges · {}", node_bridges.len())
        },
        3,
    );
    let limit = graph.ranked_limit;
    bridges_section.unlisted = node_bridges.len().saturating_sub(limit);
    for bridge in node_bridges.iter().take(limit) {
        let other_def = graph.definition(bridge.other);
        let arrow = match bridge.dir {
            Dir::Out => "→",
            Dir::In => "←",
        };
        let direction = match bridge.dir {
            Dir::Out => "outgoing",
            Dir::In => "incoming",
        };
        let conf = if bridge.edge.inferred {
            "possible "
        } else {
            ""
        };
        let sites = if bridge.sites > 1 {
            format!(" ×{}", bridge.sites)
        } else {
            String::new()
        };
        let lang_note = if bridge.other_lang != self_lang {
            match bridge.dir {
                Dir::Out => format!(" · {self_lang} → {}", bridge.other_lang),
                Dir::In => format!(" · {} → {self_lang}", bridge.other_lang),
            }
        } else {
            String::new()
        };
        let comm_name = bridge
            .other_comm
            .map(|c| graph.communities.groups[c].name.as_str());
        let comm_note = comm_name
            .map(|name| format!(" · {name}"))
            .unwrap_or_default();
        bridges_section.push(
            format!(
                "  {arrow} {} · {conf}{} at {}{sites}{lang_note}{comm_note}",
                label(other_def),
                relationship(bridge.edge.kind),
                bridge.edge.location,
            ),
            json!({
                "direction": direction,
                "symbol": node(other_def),
                "kind": relationship(bridge.edge.kind),
                "possible": bridge.edge.inferred,
                "at": bridge.edge.location.to_string(),
                "sites": bridge.sites,
                "language": bridge.other_lang,
                "community": comm_name,
            }),
        );
    }

    // Section 1: Grouped inward callers.
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum CallerKey<'a> {
        Symbol(usize),
        FileScope(&'a str),
    }
    struct GroupedEdge<'a> {
        edge: &'a Edge,
        sites: usize,
    }
    let mut callers_map = BTreeMap::<CallerKey<'_>, GroupedEdge<'_>>::new();
    for edge in index.incoming(id) {
        if edge.from == Some(id) {
            continue;
        }
        let key = match edge.from {
            Some(from) => CallerKey::Symbol(from),
            None => CallerKey::FileScope(edge.location.file.as_str()),
        };
        callers_map
            .entry(key)
            .and_modify(|entry| {
                entry.sites += 1;
                if edge_rank(edge) < edge_rank(entry.edge) {
                    entry.edge = edge;
                }
            })
            .or_insert(GroupedEdge { edge, sites: 1 });
    }
    let mut callers: Vec<(CallerKey<'_>, GroupedEdge<'_>)> = callers_map.into_iter().collect();
    callers.sort_by_key(|(key, grouped)| {
        let is_test = match *key {
            CallerKey::Symbol(from) => graph.definition(from).test,
            CallerKey::FileScope(file) => is_test_file(file),
        };
        let central_score = match *key {
            CallerKey::Symbol(from) => graph.centrality.rank_key[from],
            CallerKey::FileScope(_) => 0,
        };
        (
            is_test,
            grouped.edge.inferred,
            Reverse(central_score),
            Reverse(grouped.sites),
            &grouped.edge.location,
        )
    });
    let mut callers_section = Section::new(
        "callers",
        if callers.is_empty() {
            "Callers · none".into()
        } else {
            format!("Callers · {}", callers.len())
        },
        4,
    );
    callers_section.unlisted = callers.len().saturating_sub(limit);
    for (key, grouped) in callers.iter().take(limit) {
        let conf = if grouped.edge.inferred {
            "possible "
        } else {
            ""
        };
        let sites = if grouped.sites > 1 {
            format!(" ×{}", grouped.sites)
        } else {
            String::new()
        };
        match *key {
            CallerKey::Symbol(from) => {
                let caller_def = graph.definition(from);
                callers_section.push(
                    format!(
                        "  {} · {conf}{} at {}{sites}",
                        label(caller_def),
                        relationship(grouped.edge.kind),
                        grouped.edge.location,
                    ),
                    json!({
                        "symbol": node(caller_def),
                        "kind": relationship(grouped.edge.kind),
                        "possible": grouped.edge.inferred,
                        "at": grouped.edge.location.to_string(),
                        "sites": grouped.sites,
                    }),
                );
            }
            CallerKey::FileScope(file) => {
                callers_section.push(
                    format!(
                        "  file scope · {conf}{} at {}{sites}",
                        relationship(grouped.edge.kind),
                        grouped.edge.location,
                    ),
                    json!({
                        "file_scope": file,
                        "kind": relationship(grouped.edge.kind),
                        "possible": grouped.edge.inferred,
                        "at": grouped.edge.location.to_string(),
                        "sites": grouped.sites,
                    }),
                );
            }
        }
    }

    // Section 2: Grouped outward dependencies.
    let mut deps_map = BTreeMap::<usize, GroupedEdge<'_>>::new();
    for edge in index.outgoing(id) {
        if edge.to == id {
            continue;
        }
        deps_map
            .entry(edge.to)
            .and_modify(|entry| {
                entry.sites += 1;
                if edge_rank(edge) < edge_rank(entry.edge) {
                    entry.edge = edge;
                }
            })
            .or_insert(GroupedEdge { edge, sites: 1 });
    }
    let mut deps: Vec<(usize, GroupedEdge<'_>)> = deps_map.into_iter().collect();
    deps.sort_by_key(|&(target, ref grouped)| {
        (
            grouped.edge.inferred,
            kind_rank(grouped.edge.kind),
            graph.rank(target),
        )
    });
    let mut deps_section = Section::new(
        "dependencies",
        if deps.is_empty() {
            "Dependencies · none".into()
        } else {
            format!("Dependencies · {}", deps.len())
        },
        4,
    );
    deps_section.unlisted = deps.len().saturating_sub(limit);
    for (target, grouped) in deps.iter().take(limit) {
        let target_def = graph.definition(*target);
        let conf = if grouped.edge.inferred {
            "possible "
        } else {
            ""
        };
        let sites = if grouped.sites > 1 {
            format!(" ×{}", grouped.sites)
        } else {
            String::new()
        };
        deps_section.push(
            format!(
                "  {} · {conf}{} at {}{sites}",
                label(target_def),
                relationship(grouped.edge.kind),
                grouped.edge.location,
            ),
            json!({
                "symbol": node(target_def),
                "kind": relationship(grouped.edge.kind),
                "possible": grouped.edge.inferred,
                "at": grouped.edge.location.to_string(),
                "sites": grouped.sites,
            }),
        );
    }

    // Section 3: Reachable tests (up to 3 hops).
    let mut tests_section = Section::new(
        "reachable_tests",
        if blast.reachable_tests.is_empty() {
            "Reachable tests (≤3 hops) · none".into()
        } else {
            format!(
                "Reachable tests (≤3 hops) · {}",
                blast.reachable_tests.len()
            )
        },
        3,
    );
    tests_section.unlisted = blast.reachable_tests.len().saturating_sub(limit);
    for &(test_id, hops, inferred) in blast.reachable_tests.iter().take(limit) {
        let test_def = graph.definition(test_id);
        let possible = if inferred { " (possible)" } else { "" };
        tests_section.push(
            format!("  {} · {}{possible}", label(test_def), plural(hops, "hop")),
            json!({
                "symbol": node(test_def),
                "hops": hops,
                "possible": inferred,
            }),
        );
    }

    let mut summary = Map::new();
    summary.insert("symbol".into(), node(def));
    summary.insert("role".into(), json!(role));
    summary.insert(
        "centrality".into(),
        json!({
            "rank": rank,
            "total_symbols": total_symbols,
            "score": (graph.centrality.score[id] * 1000.0).round() / 1000.0,
            "caller_files": graph.centrality.caller_files[id],
            "caller_areas": graph.centrality.caller_areas[id],
        }),
    );
    summary.insert(
        "degree".into(),
        json!({
            "in": graph.degrees.dependents[id],
            "out": graph.degrees.dependencies[id],
            "possible_in": graph.degrees.dependents[id] - graph.degrees.certain_dependents[id],
            "possible_out": graph.degrees.dependencies[id] - graph.degrees.certain_dependencies[id],
        }),
    );
    summary.insert(
        "blast_radius".into(),
        json!({
            "hop_1": blast.hop1_dependents,
            "hop_3": blast.hop3_dependents,
            "reachable_tests": blast.reachable_tests.len(),
        }),
    );
    summary.insert("community".into(), community_json);

    let sections = vec![
        bridges_section,
        callers_section,
        deps_section,
        tests_section,
    ];
    (header, sections, summary)
}

fn explain(
    workspace: &Workspace,
    index: &Index,
    symbol: &str,
    budget: usize,
    json: bool,
) -> Result<()> {
    let id = index.select(symbol)?;
    let def = &index.definitions[id];
    let (header, sections, summary) = build_explain(index, id, budget);
    let sym_name = short(name(def), NAME_BYTES);
    let footer = [format!(
        "Next: {} expand {} L{}-{}, query {sym_name} or affected {sym_name}",
        workspace.invocation().files,
        shell_path(&def.location.file),
        def.location.start,
        def.location.end,
    )];
    emit(
        &header,
        &sections,
        &footer,
        &EXPLAIN_ORDER,
        Fill::RoundRobin,
        budget,
        json.then_some(summary),
    )
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
        let (header, sections, order, _) = build(index, budget);
        let footer = ["Next: justless expand FILE NAME, query NAME or affected NAME".to_owned()];
        let shown = fit(&header, &sections, &footer, order, Fill::RoundRobin, budget);
        let mut report = Report::new(budget);
        render(&header, &sections, &footer, &shown, &mut |line| {
            report.line(line)
        });
        report.into_string()
    }

    fn render_explain(index: &Index, symbol: &str, budget: usize) -> String {
        let id = index.select(symbol).unwrap();
        let (header, sections, _) = build_explain(index, id, budget);
        let def = &index.definitions[id];
        let footer = [format!(
            "Next: justless expand {} L{}-{}, query {} or affected {}",
            shell_path(&def.location.file),
            def.location.start,
            def.location.end,
            name(def),
            name(def),
        )];
        let shown = fit(
            &header,
            &sections,
            &footer,
            &EXPLAIN_ORDER,
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
        let (_, sections, _, _) = build(index, budget);
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

        let (_, sections, _, _) = build(&index, 2400);
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
                "def format_event(kind):\n    return kind\ndef emit_event(kind):\n    return format_event(kind)\ndef emit_warn(kind):\n    return format_event(kind)\ndef emit_err(kind):\n    return format_event(kind)\ndef emit_debug(kind):\n    return format_event(kind)\n",
            ),
            (
                "services/tiny/leaf.py",
                "def orphan_a():\n    return 1\ndef orphan_b():\n    return orphan_a()\n",
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
        // Nested test directory deduplicates its repeated directory prefix cleanly
        // without stray `./`.
        assert!(
            tight.contains(" test_rates.py:") && !tight.contains("./test_rates.py:"),
            "expected clean nested test dir prefix deduplication without `./`:\n{tight}"
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
        // leader all deduplicate repeated paths cleanly without `./`.
        assert!(
            wide.contains("services/billing/core/ · 2 files · 12 symbols · Python · hub calc_base rates.py:21-22"),
            "nested area should deduplicate repeated directory prefix without `./`:\n{wide}"
        );
        assert!(
            wide.contains("services/audit/log.py · 5 symbols · format_event :1-2"),
            "single-file community (>= 5 symbols) should deduplicate repeated file path:\n{wide}"
        );
        assert!(
            !wide.contains("services/tiny/leaf.py · 2 symbols"),
            "tiny 2-symbol isolated file should not inflate communities:\n{wide}"
        );
        assert!(
            wide.contains("isolated"),
            "isolated symbols should be reported in community summary:\n{wide}"
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

    #[test]
    fn nested_test_and_area_paths_omit_stray_dot_slash() {
        let (_dir, index) = write_repo(&[
            (
                "Cargo.toml",
                "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn alpha() -> usize { 1 }\n"),
            (
                "tests/testsuite/lookup.rs",
                "#[test]\nfn logs_add_ranked_run_output() {\n    assert_eq!(1, 1);\n}\n",
            ),
            (
                "tests/testsuite/plain_test.py",
                "# comment-only test file with no symbols\n",
            ),
            ("docs/guides/intro.md", "# Intro\n"),
        ]);

        let rendered = render_map(&index, 4096);
        assert!(
            !rendered.contains("./"),
            "map output must never contain stray `./` paths:\n{rendered}"
        );
        assert!(
            rendered.contains(
                "tests/testsuite/ · 2 files · 1 test · e.g. tests::testsuite::lookup::logs_add_ranked_run_output [test] lookup.rs:1-4"
            ),
            "expected clean relative path inside nested test directory:\n{rendered}"
        );
    }

    #[test]
    fn communities_consolidate_satellites_and_surface_cross_community_bridges() {
        // Two multi-file architectural communities in different directories &
        // languages (`frontend/` TypeScript and `backend/` Python) connected by
        // a cross-language HTTP bridge, plus a single-file 5-symbol `storage/`
        // community bridged via SQL/Python, and several tiny isolated 2-symbol
        // islands that must NOT inflate the community count.
        let (_dir, index) = write_repo(&[
            (
                "frontend/format.ts",
                "export function formatOrder(v: string): string {\n  return v.trim();\n}\n",
            ),
            (
                "frontend/orders.ts",
                "import { formatOrder } from './format.js';\nexport async function fetchOrders() {\n  const r = await fetch('/api/orders');\n  return formatOrder(await r.text());\n}\n",
            ),
            (
                "backend/orders.py",
                "from fastapi import APIRouter\nfrom backend.repo import query_orders\nrouter = APIRouter()\n@router.get('/api/orders')\ndef list_orders():\n    return query_orders()\n",
            ),
            (
                "backend/repo.py",
                "from backend.helpers import normalize_rows\ndef query_orders():\n    return normalize_rows(database.execute('SELECT id FROM purchase_orders'))\n",
            ),
            (
                "backend/helpers.py",
                "def normalize_rows(rows):\n    return rows\n",
            ),
            (
                "db/schema.sql",
                "CREATE TABLE purchase_orders (id INTEGER PRIMARY KEY);\nCREATE TABLE audit_logs (id INTEGER PRIMARY KEY);\nCREATE TABLE invoices (id INTEGER PRIMARY KEY);\nCREATE TABLE customers (id INTEGER PRIMARY KEY);\nCREATE TABLE line_items (id INTEGER PRIMARY KEY);\n",
            ),
            (
                "misc/island_a.py",
                "def lone_a1():\n    return lone_a2()\ndef lone_a2():\n    return 1\n",
            ),
            (
                "misc/island_b.py",
                "def lone_b1():\n    return lone_b2()\ndef lone_b2():\n    return 2\n",
            ),
        ]);

        let (_, sections, _, summary) = build(&index, 4096);
        let comms = sections.iter().find(|s| s.key == "communities").unwrap();
        assert_eq!(
            comms.values.len(),
            3,
            "expected 3 genuine architectural communities (frontend, backend, db), got:\n{}",
            comms.lines.join("\n")
        );
        assert!(
            summary["isolated_symbols"].as_u64().unwrap() >= 4,
            "tiny 2-symbol islands must be counted as isolated_symbols: {summary:?}"
        );
        let bridges = sections
            .iter()
            .find(|s| s.key == "bridges")
            .expect("expected Bridges section for cross-community edges");
        let joined = bridges.lines.join("\n");
        assert!(
            joined.contains("fetchOrders frontend/orders.ts:3 → list_orders backend/orders.py:4-6")
                && joined.contains("TypeScript → Python")
                && joined.contains("possible HTTP route"),
            "expected TS → Python HTTP bridge:\n{joined}"
        );
        assert!(
            joined.contains("query_orders backend/repo.py:3 → purchase_orders db/schema.sql:1")
                && joined.contains("Python → SQL")
                && joined.contains("possible SQL relation"),
            "expected Python → SQL bridge:\n{joined}"
        );
    }

    #[test]
    fn explain_emits_role_centrality_community_bridges_and_blast_radius() {
        let (_dir, index) = write_repo(&[
            (
                "frontend/format.ts",
                "export function formatOrder(v: string): string {\n  return v.trim();\n}\n",
            ),
            (
                "frontend/orders.ts",
                "import { formatOrder } from './format.js';\nexport async function fetchOrders() {\n  const r = await fetch('/api/orders');\n  return formatOrder(await r.text());\n}\n",
            ),
            (
                "backend/orders.py",
                "from fastapi import APIRouter\nfrom backend.repo import query_orders\nrouter = APIRouter()\n@router.get('/api/orders')\ndef list_orders():\n    return query_orders()\n",
            ),
            (
                "backend/repo.py",
                "from backend.helpers import normalize_rows\ndef query_orders():\n    return normalize_rows(database.execute('SELECT id FROM purchase_orders'))\n",
            ),
            (
                "backend/helpers.py",
                "def normalize_rows(rows):\n    return rows\n",
            ),
            (
                "backend/tests/test_orders.py",
                "from backend.orders import list_orders\ndef test_list_orders():\n    return list_orders()\n",
            ),
            (
                "db/schema.sql",
                "CREATE TABLE purchase_orders (id INTEGER PRIMARY KEY);\n",
            ),
        ]);

        // Entry point (`list_orders` HTTP route) with TS->Python bridge, dep, and reachable test.
        let explained = render_explain(&index, "list_orders", 2400);
        assert_eq!(
            explained,
            render_explain(&index, "list_orders", 2400),
            "explain must be deterministic"
        );
        assert!(explained.len() <= 2400, "{explained}");
        assert!(explained.contains("role: entry point"), "{explained}");
        assert!(explained.contains("Centrality · rank #"), "{explained}");
        assert!(explained.contains("blast: "), "{explained}");
        assert!(explained.contains("Community · "), "{explained}");
        assert!(
            explained.contains("Cross-community bridges · "),
            "{explained}"
        );
        assert!(
            explained.contains("fetchOrders") && explained.contains("possible HTTP route"),
            "{explained}"
        );
        assert!(explained.contains("Dependencies · "), "{explained}");
        assert!(explained.contains("query_orders"), "{explained}");
        assert!(
            explained.contains("Reachable tests (≤3 hops) · 1"),
            "{explained}"
        );
        assert!(explained.contains("test_list_orders"), "{explained}");

        // Transitive 2-hop blast radius on `query_orders` reaches `test_list_orders`.
        let q_id = index.select("query_orders").unwrap();
        let (_, _, q_summary) = build_explain(&index, q_id, 2400);
        assert_eq!(q_summary["role"], "bridge", "{q_summary:?}");
        assert_eq!(
            q_summary["blast_radius"]["reachable_tests"], 1,
            "{q_summary:?}"
        );
        assert!(
            q_summary["blast_radius"]["hop_3"].as_u64().unwrap() >= 2,
            "{q_summary:?}"
        );

        // Leaf & test roles.
        let leaf_id = index.select("normalize_rows").unwrap();
        let (_, _, leaf_summary) = build_explain(&index, leaf_id, 2400);
        assert_eq!(leaf_summary["role"], "leaf", "{leaf_summary:?}");

        let test_id = index.select("test_list_orders").unwrap();
        let (_, _, test_summary) = build_explain(&index, test_id, 2400);
        assert_eq!(test_summary["role"], "test", "{test_summary:?}");

        // Tight budget stays strictly within ceiling.
        let tight = render_explain(&index, "list_orders", 420);
        assert!(tight.len() <= 420, "{} bytes\n{tight}", tight.len());
    }
}
