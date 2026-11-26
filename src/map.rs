//! `justless map`: a budgeted orientation report built from the repository
//! index. It reads no source files, so its cost is the index refresh plus a
//! few linear passes over symbols and links.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
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
                .conflicts_with("html")
                .help("Print the selected items as JSON"),
        )
        .arg(
            Arg::new("html")
                .long("html")
                .conflicts_with("json")
                .help("Render a self-contained offline interactive HTML graph explorer"),
        )
        .arg(
            Arg::new("out")
                .long("out")
                .takes_value(true)
                .allow_invalid_utf8(true)
                .value_name("PATH")
                .requires("html")
                .help("With --html, write the HTML explorer to PATH instead of stdout"),
        )
}

pub fn run(workspace: &Workspace, matches: &ArgMatches) -> Result<()> {
    let budget = crate::context_budget(workspace, matches)?;
    let index = Index::open(workspace)?;
    run_with_index(workspace, &index, matches, budget)
}

pub(crate) fn run_with_index(
    workspace: &Workspace,
    index: &Index,
    matches: &ArgMatches,
    budget: usize,
) -> Result<()> {
    if matches.is_present("html") {
        let focus = matches
            .value_of("explain")
            .map(|symbol| index.select(symbol))
            .transpose()?;
        let title = workspace
            .root
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("repository");
        let rendered = build_html(index, title, &workspace.invocation().files, focus);
        let shown_bytes = if let Some(out) = matches.value_of_os("out") {
            let path = Path::new(out);
            std::fs::write(path, &rendered.html)
                .with_context(|| format!("could not write {}", path.display()))?;
            let comms = if rendered.communities == 1 {
                "1 community".to_owned()
            } else {
                format!("{} communities", rendered.communities)
            };
            let status = format!(
                "Wrote {} · {} bytes · {} · {} · {comms}\n",
                path.display(),
                rendered.html.len(),
                plural(rendered.nodes, "node"),
                plural(rendered.edges, "edge"),
            );
            std::io::stdout().lock().write_all(status.as_bytes())?;
            status.len() as u64
        } else {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(rendered.html.as_bytes())?;
            stdout.flush()?;
            rendered.html.len() as u64
        };
        crate::history::record_graph_files(
            workspace,
            "map",
            shown_bytes,
            rendered.referenced_files.iter().map(String::as_str),
        );
        return Ok(());
    }
    let json = matches.is_present("json");
    if let Some(symbol) = matches.value_of("explain") {
        return explain(workspace, index, symbol, budget, json);
    }
    let (header, sections, order, summary) = build(index, budget);
    // `--root` for `expand` also serves `query` and `affected`.
    let footer = [format!(
        "Next: {} expand FILE NAME, query NAME or affected NAME",
        workspace.invocation().files
    )];
    let (shown_bytes, shown) = emit_measured(
        &header,
        &sections,
        &footer,
        order,
        Fill::RoundRobin,
        budget,
        json.then_some(summary),
    )?;
    let files = collect_section_files(&sections, &shown);
    crate::history::record_graph_files(workspace, "map", shown_bytes, files);
    Ok(())
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
    emit_measured(header, sections, footer, order, fill, budget, json)?;
    Ok(())
}

fn emit_measured(
    header: &[String],
    sections: &[Section],
    footer: &[String],
    order: &[usize],
    fill: Fill,
    budget: usize,
    json: Option<Map<String, Value>>,
) -> Result<(u64, Vec<usize>)> {
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
        let mut out = serde_json::to_string(&object)?;
        out.push('\n');
        std::io::stdout().lock().write_all(out.as_bytes())?;
        return Ok((out.len() as u64, shown));
    }
    let mut report = Report::new(budget);
    render(header, sections, footer, &shown, &mut |line| {
        report.line(line)
    });
    let text = report.into_string();
    let bytes = text.len() as u64;
    std::io::stdout().lock().write_all(text.as_bytes())?;
    Ok((bytes, shown))
}

fn collect_value_files<'a>(value: &'a Value, out: &mut BTreeSet<&'a str>) {
    match value {
        Value::Object(map) => {
            for key in ["file", "file_scope"] {
                if let Some(Value::String(file)) = map.get(key) {
                    out.insert(file.as_str());
                }
            }
            for child in map.values() {
                collect_value_files(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_value_files(item, out);
            }
        }
        _ => {}
    }
}

fn collect_section_files<'a>(sections: &'a [Section], shown: &[usize]) -> BTreeSet<&'a str> {
    let mut files = BTreeSet::new();
    for (section, &count) in sections.iter().zip(shown) {
        for value in &section.values[..count] {
            collect_value_files(value, &mut files);
        }
    }
    files
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
    is_support_path(file, is_test_file(file))
}

fn is_support_path(file: &str, is_test: bool) -> bool {
    is_test
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

// ---------------------------------------------------------------------------
// The map

/// Symbols that make sense as hubs. Files, module declarations and tests are
/// connected by construction rather than by design.
fn central(definition: &Definition) -> bool {
    !definition.test && !matches!(definition.kind.as_str(), "file" | "module")
}

/// Precomputed per-file metadata indexed by dense `file_id` (sorted
/// lexicographically by file path, so `file_id` order matches `&str` order).
pub(crate) struct FileMeta<'a> {
    pub(crate) path: &'a str,
    pub(crate) dir: String,
    pub(crate) dir_id: usize,
    pub(crate) is_test: bool,
    pub(crate) is_support: bool,
    pub(crate) lang: Option<&'static str>,
    pub(crate) indexed_lang: Option<&'static str>,
    pub(crate) in_index_files: bool,
}

/// Dense per-file table shared across graph passes (`map`, `map --explain`,
/// `review`) so hot edge/symbol loops use `usize` indexing instead of repeated
/// path parsing, string allocations, or map lookups.
pub(crate) struct FileIndex<'a> {
    pub(crate) files: Vec<FileMeta<'a>>,
    by_path: HashMap<&'a str, usize>,
    pub(crate) def_file: Vec<usize>,
    pub(crate) file_def: Vec<Option<usize>>,
}

impl<'a> FileIndex<'a> {
    pub(crate) fn new(index: &'a Index) -> Self {
        let mut paths: Vec<&'a str> = index.files.keys().map(String::as_str).collect();
        let mut extras = Vec::<&'a str>::new();
        let mut last_seen = "";
        let mut check_path = |file: &'a str| {
            if file != last_seen {
                last_seen = file;
                if !index.files.contains_key(file) {
                    extras.push(file);
                }
            }
        };
        for def in &index.definitions {
            check_path(def.location.file.as_str());
        }
        for edge in &index.edges {
            check_path(edge.location.file.as_str());
        }
        if !extras.is_empty() {
            paths.extend(extras);
            paths.sort_unstable();
            paths.dedup();
        }

        let mut by_path = HashMap::<&'a str, usize>::with_capacity(paths.len());
        let mut dir_ids = HashMap::<&'a str, usize>::new();
        let mut files = Vec::with_capacity(paths.len());
        for (fid, path) in paths.into_iter().enumerate() {
            by_path.insert(path, fid);
            let parent = Path::new(path)
                .parent()
                .and_then(Path::to_str)
                .unwrap_or("");
            let next_dir_id = dir_ids.len();
            let dir_id = *dir_ids.entry(parent).or_insert(next_dir_id);
            let dir = if parent.is_empty() {
                "(root)".to_owned()
            } else {
                format!("{parent}/")
            };
            let is_test = is_test_file(path);
            let is_support = is_support_path(path, is_test);
            let lang = language(path);
            let indexed_lang = indexed_language(index, path);
            let in_index_files = index.files.contains_key(path);
            files.push(FileMeta {
                path,
                dir,
                dir_id,
                is_test,
                is_support,
                lang,
                indexed_lang,
                in_index_files,
            });
        }

        let mut def_file = Vec::with_capacity(index.definitions.len());
        let mut file_def = vec![None; files.len()];
        let mut cached_path = "";
        let mut cached_fid = 0usize;
        for (id, def) in index.definitions.iter().enumerate() {
            let p = def.location.file.as_str();
            let fid = if !def_file.is_empty() && p == cached_path {
                cached_fid
            } else {
                cached_path = p;
                cached_fid = by_path[p];
                cached_fid
            };
            def_file.push(fid);
            if def.kind == "file" {
                file_def[fid] = Some(id);
            }
        }

        Self {
            files,
            by_path,
            def_file,
            file_def,
        }
    }

    #[inline]
    pub(crate) fn id_of(&self, path: &str) -> usize {
        self.by_path[path]
    }
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
    fn new(index: &Index, files: &FileIndex<'_>) -> Self {
        let count = index.definitions.len();
        let mut dependents = vec![0usize; count];
        let mut dependencies = vec![0usize; count];
        let mut certain_dependents = vec![0usize; count];
        let mut certain_dependencies = vec![0usize; count];

        let mut scratch = Vec::<(usize, bool)>::with_capacity(32);
        let tally = |buf: &mut Vec<(usize, bool)>| -> (usize, usize) {
            match buf.len() {
                0 => (0, 0),
                1 => (1, usize::from(buf[0].1)),
                _ => {
                    buf.sort_unstable_by_key(|&(k, _)| k);
                    let mut total = 1usize;
                    let mut cur_key = buf[0].0;
                    let mut cur_certain = buf[0].1;
                    let mut certain_count = 0usize;
                    for &(k, c) in &buf[1..] {
                        if k == cur_key {
                            cur_certain |= c;
                        } else {
                            certain_count += usize::from(cur_certain);
                            total += 1;
                            cur_key = k;
                            cur_certain = c;
                        }
                    }
                    certain_count += usize::from(cur_certain);
                    (total, certain_count)
                }
            }
        };

        for to in 0..count {
            scratch.clear();
            for edge in index.incoming(to) {
                if edge.from == Some(to) {
                    continue;
                }
                let key = match edge.from {
                    Some(from) => from,
                    None => count + files.id_of(edge.location.file.as_str()),
                };
                scratch.push((key, !edge.inferred));
            }
            let (all, cert) = tally(&mut scratch);
            dependents[to] = all;
            certain_dependents[to] = cert;
        }

        for from in 0..count {
            scratch.clear();
            for edge in index.outgoing(from) {
                if edge.to == from {
                    continue;
                }
                scratch.push((edge.to, !edge.inferred));
            }
            let (all, cert) = tally(&mut scratch);
            dependencies[from] = all;
            certain_dependencies[from] = cert;
        }

        Self {
            dependents,
            dependencies,
            certain_dependents,
            certain_dependencies,
        }
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

/// Dense area mapping for all files in a `FileIndex`.
struct AreaMap {
    /// Unique area names (e.g. `"src/"`, `"(root)"`, `"tests/"`).
    names: Vec<String>,
    is_support: Vec<bool>,
    /// Maps each `file_id` to its dense `area_id`.
    file_area: Vec<usize>,
}

impl AreaMap {
    fn new(index: &Index, files: &FileIndex<'_>) -> Self {
        let assigned = areas(index, files);
        let mut names = Vec::<String>::new();
        let mut by_name = HashMap::<String, usize>::new();
        let mut get_id = |area: &str| -> usize {
            if let Some(&id) = by_name.get(area) {
                return id;
            }
            let id = names.len();
            names.push(area.to_owned());
            by_name.insert(area.to_owned(), id);
            id
        };
        let mut file_area = Vec::with_capacity(files.files.len());
        for (fid, meta) in files.files.iter().enumerate() {
            let name = if meta.in_index_files {
                assigned[fid].as_str()
            } else {
                "(root)"
            };
            file_area.push(get_id(name));
        }
        let is_support = names.iter().map(|n| is_support_area(n)).collect();
        Self {
            names,
            is_support,
            file_area,
        }
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

    fn new(index: &Index, files: &FileIndex<'_>, area_map: &AreaMap) -> Self {
        let def_count = index.definitions.len();
        let num_files = files.files.len();
        let num_areas = area_map.names.len().max(1);

        // Reuse existing `"file"` definitions and allocate deterministic virtual
        // file-scope nodes in lexicographical file order for any remaining files
        // that emit file-scope edges (`edge.from == None`).
        let mut file_nodes = files.file_def.clone();
        let mut needs_virtual = vec![false; num_files];
        let mut cached_path = "";
        let mut cached_fid = 0usize;
        for edge in &index.edges {
            if edge.from.is_none() {
                let p = edge.location.file.as_str();
                let fid = if p == cached_path && !cached_path.is_empty() {
                    cached_fid
                } else {
                    cached_path = p;
                    cached_fid = files.id_of(p);
                    cached_fid
                };
                if file_nodes[fid].is_none() {
                    needs_virtual[fid] = true;
                }
            }
        }
        let mut node_file: Vec<usize> = files.def_file.clone();
        for (fid, &need) in needs_virtual.iter().enumerate() {
            if need {
                let id = node_file.len();
                file_nodes[fid] = Some(id);
                node_file.push(fid);
            }
        }
        let node_count = node_file.len();
        let node_area: Vec<usize> = node_file.iter().map(|&f| area_map.file_area[f]).collect();
        let node_support: Vec<bool> = node_file
            .iter()
            .map(|&f| files.files[f].is_support)
            .collect();

        // Single pass over raw edges to compute weights, `has_strong_in`, and
        // per-source raw edge counts for CSR bucketing.
        let mut has_strong_in = vec![false; def_count];
        let mut out_deg = vec![0u32; node_count];
        let mut raw_edges = Vec::<(u32, u32, f64)>::with_capacity(index.edges.len());

        cached_path = "";
        cached_fid = 0;
        for edge in &index.edges {
            let src = match edge.from {
                Some(id) => id,
                None => {
                    let p = edge.location.file.as_str();
                    let fid = if p == cached_path && !cached_path.is_empty() {
                        cached_fid
                    } else {
                        cached_path = p;
                        cached_fid = files.id_of(p);
                        cached_fid
                    };
                    file_nodes[fid].unwrap()
                }
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
            raw_edges.push((src as u32, dst as u32, weight));
            out_deg[src] += 1;

            if dst < def_count && (!edge.inferred || matches!(edge.kind, Kind::Route | Kind::Data))
            {
                has_strong_in[dst] = true;
            }
        }

        // Bucket by `src` into outgoing CSR, sort each source's slice by `dst`,
        // deduplicate keeping max weight, and tally `out_sum` + `ext_out_areas`.
        let mut raw_offsets = vec![0u32; node_count + 1];
        for src in 0..node_count {
            raw_offsets[src + 1] = raw_offsets[src] + out_deg[src];
        }
        let mut out_edges = vec![(0u32, 0.0f64); raw_edges.len()];
        let mut cursor = raw_offsets[..node_count].to_vec();
        for (src, dst, weight) in raw_edges {
            let slot = cursor[src as usize] as usize;
            cursor[src as usize] += 1;
            out_edges[slot] = (dst, weight);
        }

        let mut out_offsets = vec![0u32; node_count + 1];
        let mut out_sum = vec![0.0f64; node_count];
        let mut in_deg = vec![0u32; node_count];
        let mut ext_out_areas = vec![0usize; def_count];
        let mut area_stamp = vec![0u32; num_areas];
        let mut write = 0usize;

        for src in 0..node_count {
            let start = raw_offsets[src] as usize;
            let end = raw_offsets[src + 1] as usize;
            out_offsets[src] = write as u32;
            if start == end {
                continue;
            }
            if end - start > 1 {
                out_edges[start..end].sort_unstable_by_key(|&(dst, _)| dst);
            }
            let stamp = (src + 1) as u32;
            let src_area = node_area[src];
            let src_sup = node_support[src];
            let mut out_area_cnt = 0usize;
            let mut sum = 0.0f64;

            let mut cur_dst = out_edges[start].0;
            let mut cur_w = out_edges[start].1;
            let mut emit_edge = |dst: u32, w: f64, buf: &mut [(u32, f64)]| {
                buf[write] = (dst, w);
                write += 1;
                sum += w;
                let d = dst as usize;
                in_deg[d] += 1;
                if src < def_count
                    && src_area != node_area[d]
                    && (!node_support[d] || src_sup)
                    && area_stamp[node_area[d]] != stamp
                {
                    area_stamp[node_area[d]] = stamp;
                    out_area_cnt += 1;
                }
            };
            for r in (start + 1)..end {
                let (dst, w) = out_edges[r];
                if dst == cur_dst {
                    if w > cur_w {
                        cur_w = w;
                    }
                } else {
                    emit_edge(cur_dst, cur_w, &mut out_edges);
                    cur_dst = dst;
                    cur_w = w;
                }
            }
            emit_edge(cur_dst, cur_w, &mut out_edges);
            out_sum[src] = sum;
            if src < def_count {
                ext_out_areas[src] = out_area_cnt;
            }
        }
        out_offsets[node_count] = write as u32;
        out_edges.truncate(write);

        // Bucket deduplicated edges by `dst` to form flat caller-file groups in
        // deterministic `(caller_file_id, src)` order for PageRank, while
        // counting `caller_files` and `caller_areas` in the same pass.
        let mut in_offsets = vec![0u32; node_count + 1];
        for dst in 0..node_count {
            in_offsets[dst + 1] = in_offsets[dst] + in_deg[dst];
        }
        let mut in_raw = vec![(0u32, 0u32, 0.0f64); write];
        cursor.copy_from_slice(&in_offsets[..node_count]);
        for src in 0..node_count {
            let denom = out_sum[src] + Self::LEAK;
            let s_start = out_offsets[src] as usize;
            let s_end = out_offsets[src + 1] as usize;
            let src_file = node_file[src] as u32;
            for &(dst, weight) in &out_edges[s_start..s_end] {
                let d = dst as usize;
                let slot = cursor[d] as usize;
                cursor[d] += 1;
                in_raw[slot] = (src_file, src as u32, weight / denom);
            }
        }

        let mut in_flat = Vec::<(u32, f64)>::with_capacity(write);
        let mut in_groups = Vec::<(u32, u32)>::new();
        let mut dst_groups = vec![(0u32, 0u32); node_count];
        let mut caller_files = vec![0usize; def_count];
        let mut caller_areas = vec![0usize; def_count];
        area_stamp.fill(0);

        for dst in 0..node_count {
            let start = in_offsets[dst] as usize;
            let end = in_offsets[dst + 1] as usize;
            if start == end {
                continue;
            }
            if end - start > 1 {
                in_raw[start..end].sort_unstable_by_key(|&(fid, src, _)| (fid, src));
            }
            let grp_start = in_groups.len() as u32;
            let dst_file = node_file[dst];
            let dst_area = node_area[dst];
            let dst_sup = node_support[dst];
            let stamp = (dst + 1) as u32;
            let mut file_cnt = 0usize;
            let mut area_cnt = 0usize;
            let mut last_counted_file = usize::MAX;

            let mut run_file = in_raw[start].0;
            let mut run_start = in_flat.len() as u32;
            for &(fid, src, trans) in &in_raw[start..end] {
                if fid != run_file {
                    in_groups.push((run_start, in_flat.len() as u32));
                    run_file = fid;
                    run_start = in_flat.len() as u32;
                }
                in_flat.push((src, trans));
                if dst < def_count {
                    let s = src as usize;
                    let count_caller = !node_support[s] || dst_sup;
                    if count_caller {
                        let sf = fid as usize;
                        if sf != dst_file && sf != last_counted_file {
                            last_counted_file = sf;
                            file_cnt += 1;
                        }
                        let sa = node_area[s];
                        if sa != dst_area && area_stamp[sa] != stamp {
                            area_stamp[sa] = stamp;
                            area_cnt += 1;
                        }
                    }
                }
            }
            in_groups.push((run_start, in_flat.len() as u32));
            dst_groups[dst] = (grp_start, in_groups.len() as u32);
            if dst < def_count {
                caller_files[dst] = file_cnt;
                caller_areas[dst] = area_cnt;
            }
        }

        // Active nodes start with base teleportation mass (support files damped).
        let base: Vec<f64> = (0..node_count)
            .map(|u| {
                if out_sum[u] == 0.0 && dst_groups[u].0 == dst_groups[u].1 {
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
                let (g_start, g_end) = dst_groups[dst];
                let mut received = 0.0f64;
                for &(e_start, e_end) in &in_groups[g_start as usize..g_end as usize] {
                    let group = &in_flat[e_start as usize..e_end as usize];
                    if group.len() == 1 {
                        let (src, trans) = group[0];
                        received += rank[src as usize] * trans;
                    } else {
                        flows.clear();
                        for &(src, trans) in group {
                            flows.push(rank[src as usize] * trans);
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
        for id in 0..def_count {
            let files_n = caller_files[id];
            let in_areas = caller_areas[id];
            let out_areas = ext_out_areas[id];
            let mut value = incoming_flow[id];
            if value > 0.0 {
                let area_factor = 1.0 + 0.35 * (in_areas as f64 + 0.5 * out_areas as f64).ln_1p();
                let file_factor = 1.0 + 0.15 * (files_n as f64).ln_1p();
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
                let s_start = out_offsets[src] as usize;
                let s_end = out_offsets[src + 1] as usize;
                out_edges[s_start..s_end]
                    .iter()
                    .map(|&(dst, weight)| {
                        let d = dst as usize;
                        let authority = if d < def_count { 0.25 + score[d] } else { 0.25 };
                        weight * authority
                    })
                    .sum()
            })
            .collect();
        let mut reach = direct_reach.clone();
        let mut next_reach = vec![0.0f64; def_count];
        for _ in 0..4 {
            for src in 0..def_count {
                let s_start = out_offsets[src] as usize;
                let s_end = out_offsets[src + 1] as usize;
                let denom = out_sum[src] + 0.25;
                let downstream: f64 = out_edges[s_start..s_end]
                    .iter()
                    .filter(|&&(dst, _)| (dst as usize) < def_count)
                    .map(|&(dst, weight)| (weight / denom) * reach[dst as usize])
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
fn areas(index: &Index, files: &FileIndex<'_>) -> Vec<String> {
    let num_files = files.files.len();
    let mut symbols = vec![0usize; num_files];
    let mut total = 0usize;
    for (id, definition) in index.definitions.iter().enumerate() {
        if definition.kind != "file" {
            symbols[files.def_file[id]] += 1;
            total += 1;
        }
    }
    let mut splits = BTreeSet::new();
    let assign = |splits: &BTreeSet<String>| -> Vec<String> {
        files
            .files
            .iter()
            .map(|meta| {
                if !meta.in_index_files {
                    return "(root)".to_owned();
                }
                let file = meta.path;
                let parts: Vec<&str> = file.split('/').collect();
                let mut depth = 1;
                while depth < parts.len() && splits.contains(&parts[..depth].join("/")) {
                    depth += 1;
                }
                if parts.len() == 1 {
                    "(root)".to_owned()
                } else if depth == parts.len() {
                    file.to_owned()
                } else {
                    format!("{}/", parts[..depth].join("/"))
                }
            })
            .collect()
    };
    let mut assigned = assign(&splits);
    for _ in 0..4 {
        let mut sizes = BTreeMap::<&str, usize>::new();
        for (fid, meta) in files.files.iter().enumerate() {
            if meta.in_index_files {
                *sizes.entry(assigned[fid].as_str()).or_default() += symbols[fid];
            }
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
        for (fid, meta) in files.files.iter().enumerate() {
            if !meta.in_index_files || assigned[fid].as_str() != largest {
                continue;
            }
            if let Some(rest) = meta.path.strip_prefix(largest) {
                direct_children.insert(rest.split('/').next().unwrap_or(rest));
                if rest.contains('/') {
                    subdir_symbols += symbols[fid];
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

/// Dense CSR asynchronous label propagation shared by `map` and `review`.
/// Preserves exact deterministic node ordering and smallest-label tie-breaking
/// while avoiding per-node hash tables via dense scratch buffers + dirty flags.
fn run_lpa(index: &Index, files: &FileIndex<'_>, max_iters: usize) -> Vec<usize> {
    let definitions = &index.definitions;
    let def_count = definitions.len();
    let num_files = files.files.len();
    // Rust files have no file definition, so they get a node after the definitions.
    let mut nodes = def_count;
    let mut file_nodes = files.file_def.clone();
    let mut by_file = vec![Vec::<usize>::new(); num_files];
    for (id, definition) in definitions.iter().enumerate() {
        if definition.kind != "file" {
            by_file[files.def_file[id]].push(id);
        }
    }

    let mut raw = Vec::<(u32, u32, u32)>::with_capacity(def_count + index.edges.len());
    let mut add_link = |a: usize, b: usize, weight: u32| {
        if a != b {
            raw.push((a as u32, b as u32, weight));
        }
    };

    for (fid, mut ids) in by_file.into_iter().enumerate() {
        if ids.is_empty() {
            continue;
        }
        let root = *file_nodes[fid].get_or_insert_with(|| {
            let id = nodes;
            nodes += 1;
            id
        });
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
            add_link(id, stack.last().copied().unwrap_or(root), 3);
            stack.push(id);
        }
    }

    let mut cached_path = "";
    let mut cached_fid = 0usize;
    for edge in &index.edges {
        let from = match edge.from {
            Some(id) => id,
            None => {
                let p = edge.location.file.as_str();
                let fid = if p == cached_path && !cached_path.is_empty() {
                    cached_fid
                } else {
                    cached_path = p;
                    cached_fid = files.id_of(p);
                    cached_fid
                };
                *file_nodes[fid].get_or_insert_with(|| {
                    let id = nodes;
                    nodes += 1;
                    id
                })
            }
        };
        // Inferred calls match by name alone. Inferred routes and SQL relations
        // still describe how areas connect.
        let weight = match (edge.inferred, edge.kind) {
            (false, _) => 2,
            (true, Kind::Route | Kind::Data) => 1,
            (true, _) => continue,
        };
        add_link(from, edge.to, weight);
    }

    // Build flat undirected CSR adjacency and coalesce duplicate neighbor pairs.
    let mut deg = vec![0u32; nodes];
    for &(a, b, _) in &raw {
        deg[a as usize] += 1;
        deg[b as usize] += 1;
    }
    let mut raw_offsets = vec![0u32; nodes + 1];
    for u in 0..nodes {
        raw_offsets[u + 1] = raw_offsets[u] + deg[u];
    }
    let mut adj = vec![(0u32, 0u32); raw.len() * 2];
    let mut cursor = raw_offsets[..nodes].to_vec();
    for (a, b, w) in raw {
        let sa = cursor[a as usize] as usize;
        cursor[a as usize] += 1;
        adj[sa] = (b, w);
        let sb = cursor[b as usize] as usize;
        cursor[b as usize] += 1;
        adj[sb] = (a, w);
    }

    let mut offsets = vec![0u32; nodes + 1];
    let mut write = 0usize;
    for u in 0..nodes {
        let start = raw_offsets[u] as usize;
        let end = raw_offsets[u + 1] as usize;
        offsets[u] = write as u32;
        if start == end {
            continue;
        }
        if end - start > 1 {
            adj[start..end].sort_unstable_by_key(|&(other, _)| other);
        }
        let mut cur_other = adj[start].0;
        let mut cur_w = adj[start].1;
        for r in (start + 1)..end {
            let (other, w) = adj[r];
            if other == cur_other {
                cur_w += w;
            } else {
                adj[write] = (cur_other, cur_w);
                write += 1;
                cur_other = other;
                cur_w = w;
            }
        }
        adj[write] = (cur_other, cur_w);
        write += 1;
    }
    offsets[nodes] = write as u32;
    adj.truncate(write);

    let mut labels: Vec<usize> = (0..nodes).collect();
    let mut dirty = vec![true; nodes];
    let mut weights = vec![0u32; nodes];
    let mut touched = Vec::<usize>::with_capacity(64);

    for _ in 0..max_iters {
        let mut changed = false;
        for node in 0..nodes {
            if !dirty[node] {
                continue;
            }
            dirty[node] = false;
            let start = offsets[node] as usize;
            let end = offsets[node + 1] as usize;
            let nbrs = &adj[start..end];
            if nbrs.is_empty() {
                continue;
            }
            let cur = labels[node];
            let choice = if nbrs.len() == 1 {
                labels[nbrs[0].0 as usize]
            } else if nbrs.len() == 2 {
                let l0 = labels[nbrs[0].0 as usize];
                let l1 = labels[nbrs[1].0 as usize];
                let w0 = nbrs[0].1;
                let w1 = nbrs[1].1;
                if l0 == l1 || w0 > w1 {
                    l0
                } else if w1 > w0 {
                    l1
                } else if cur == l0 || cur == l1 {
                    cur
                } else {
                    l0.min(l1)
                }
            } else {
                touched.clear();
                let mut best = 0u32;
                for &(other, w) in nbrs {
                    let lbl = labels[other as usize];
                    let slot = &mut weights[lbl];
                    if *slot == 0 {
                        touched.push(lbl);
                    }
                    *slot += w;
                    if *slot > best {
                        best = *slot;
                    }
                }
                let picked = if weights[cur] == best {
                    cur
                } else {
                    let mut min_lbl = usize::MAX;
                    for &lbl in &touched {
                        if weights[lbl] == best && lbl < min_lbl {
                            min_lbl = lbl;
                        }
                    }
                    min_lbl
                };
                for &lbl in &touched {
                    weights[lbl] = 0;
                }
                picked
            };
            if choice != cur {
                labels[node] = choice;
                changed = true;
                for &(other, _) in nbrs {
                    dirty[other as usize] = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    labels
}

/// Lightweight community membership for `justless review`: runs dense LPA and
/// keeps architectural clusters (`>= 2` files or `>= 5` symbols).
pub(crate) fn lpa_communities(
    index: &Index,
    files: &FileIndex<'_>,
    max_iters: usize,
) -> (Vec<Option<usize>>, usize) {
    let definitions = &index.definitions;
    let labels = run_lpa(index, files, max_iters);
    let mut label_to_group = vec![u32::MAX; labels.len()];
    let mut groups = Vec::<(usize, Vec<usize>)>::new();
    for (id, def) in definitions.iter().enumerate() {
        if def.kind == "file" {
            continue;
        }
        let lbl = labels[id];
        let slot = label_to_group[lbl];
        if slot == u32::MAX {
            label_to_group[lbl] = groups.len() as u32;
            groups.push((lbl, vec![id]));
        } else {
            groups[slot as usize].1.push(id);
        }
    }
    groups.sort_unstable_by_key(|&(lbl, _)| lbl);

    let mut community_of = vec![None; definitions.len()];
    let mut next_comm = 0usize;
    for (_, members) in groups {
        let multi_file = if let Some(&first) = members.first() {
            let f0 = files.def_file[first];
            members[1..].iter().any(|&id| files.def_file[id] != f0)
        } else {
            false
        };
        if multi_file || members.len() >= MIN_SINGLE_FILE_COMMUNITY {
            for id in members {
                community_of[id] = Some(next_comm);
            }
            next_comm += 1;
        }
    }
    (community_of, next_comm)
}

/// Asynchronous label propagation in a fixed order with the smallest label
/// winning ties, followed by deterministic satellite consolidation and
/// filtering to genuine architectural communities (>= 2 files OR >= 5 symbols).
/// Isolated / leaf symbols are counted cleanly instead of inflating communities.
fn communities<'a>(index: &'a Index, files: &FileIndex<'a>) -> CommunityMap<'a> {
    let definitions = &index.definitions;
    let num_files = files.files.len();
    let labels = run_lpa(index, files, 32);

    // Grouping in ascending `id` order naturally orders groups by `members[0]`.
    let mut label_to_group = vec![u32::MAX; labels.len()];
    let mut groups = Vec::<Vec<usize>>::new();
    let mut total_non_file = 0usize;
    for (id, definition) in definitions.iter().enumerate() {
        if definition.kind != "file" {
            total_non_file += 1;
            let lbl = labels[id];
            let slot = label_to_group[lbl];
            if slot == u32::MAX {
                label_to_group[lbl] = groups.len() as u32;
                groups.push(vec![id]);
            } else {
                groups[slot as usize].push(id);
            }
        }
    }

    let single_file_of = |members: &[usize]| -> Option<usize> {
        let first = files.def_file[*members.first()?];
        members[1..]
            .iter()
            .all(|&id| files.def_file[id] == first)
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
    let mut group_of = vec![u32::MAX; definitions.len()];
    let mut ext_pairs = Vec::<(u32, u32)>::new();
    for _ in 0..2 {
        group_of.fill(u32::MAX);
        for (g_idx, members) in groups.iter().enumerate() {
            for &id in members {
                group_of[id] = g_idx as u32;
            }
        }
        ext_pairs.clear();
        for edge in &index.edges {
            if edge.inferred {
                continue;
            }
            let Some(from) = edge.from else {
                continue;
            };
            let g1 = group_of[from];
            let g2 = group_of[edge.to];
            if g1 == u32::MAX || g2 == u32::MAX || g1 == g2 {
                continue;
            }
            ext_pairs.push((g1, g2));
            ext_pairs.push((g2, g1));
        }
        ext_pairs.sort_unstable();
        let mut ext_adj = vec![Vec::<(usize, u32)>::new(); groups.len()];
        let mut i = 0usize;
        while i < ext_pairs.len() {
            let (g1, g2) = ext_pairs[i];
            let mut count = 1u32;
            i += 1;
            while i < ext_pairs.len() && ext_pairs[i] == (g1, g2) {
                count += 1;
                i += 1;
            }
            ext_adj[g1 as usize].push((g2 as usize, count));
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
            let neighbors = &ext_adj[g];
            if neighbors.is_empty() {
                continue;
            }
            let total: u32 = neighbors.iter().map(|&(_, w)| w).sum();
            if total == 0 {
                continue;
            }
            let best = neighbors
                .iter()
                .copied()
                .filter(|&(other, _)| !groups[other].is_empty() && is_architectural(&groups[other]))
                .max_by_key(|&(other, w)| (w, groups[other].len(), Reverse(groups[other][0])));
            if let Some((target, best_w)) = best
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
    #[derive(Clone, Copy)]
    struct FileDom {
        arch: bool,
        non_test: bool,
        in_file: usize,
        total: usize,
        group: usize,
    }
    let mut dominant_by_file: Vec<Option<FileDom>> = vec![None; num_files];
    let mut file_counts = vec![0usize; num_files];
    let mut touched_files = Vec::<usize>::new();
    for (g_idx, members) in groups.iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        let arch = is_architectural(members);
        let non_test = !is_test_or_module_only(members);
        touched_files.clear();
        for &id in members {
            let fid = files.def_file[id];
            if file_counts[fid] == 0 {
                touched_files.push(fid);
            }
            file_counts[fid] += 1;
        }
        for &fid in &touched_files {
            let in_file = file_counts[fid];
            file_counts[fid] = 0;
            let candidate = (
                arch,
                non_test,
                in_file,
                members.len(),
                usize::MAX - members[0],
            );
            match &mut dominant_by_file[fid] {
                Some(best) => {
                    let best_key = (
                        best.arch,
                        best.non_test,
                        best.in_file,
                        best.total,
                        usize::MAX - groups[best.group][0],
                    );
                    if candidate > best_key {
                        *best = FileDom {
                            arch,
                            non_test,
                            in_file,
                            total: members.len(),
                            group: g_idx,
                        };
                    }
                }
                None => {
                    dominant_by_file[fid] = Some(FileDom {
                        arch,
                        non_test,
                        in_file,
                        total: members.len(),
                        group: g_idx,
                    });
                }
            }
        }
    }
    let mut same_file_merges = Vec::<(usize, usize)>::new();
    for (g_idx, members) in groups.iter().enumerate() {
        let Some(fid) = single_file_of(members) else {
            continue;
        };
        let Some(dom) = dominant_by_file[fid] else {
            continue;
        };
        if dom.group != g_idx {
            same_file_merges.push((g_idx, dom.group));
        }
    }
    for (from_g, to_g) in same_file_merges {
        let moved = std::mem::take(&mut groups[from_g]);
        groups[to_g].extend(moved);
        groups[to_g].sort_unstable();
    }

    let mut architectural = Vec::<Community<'a>>::new();
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
        touched_files.clear();
        for &id in &members {
            let fid = files.def_file[id];
            if file_counts[fid] == 0 {
                touched_files.push(fid);
            }
            file_counts[fid] += 1;
        }
        let mut comm_files = BTreeMap::<&'a str, usize>::new();
        for &fid in &touched_files {
            comm_files.insert(files.files[fid].path, file_counts[fid]);
            file_counts[fid] = 0;
        }
        let name = community_name(&comm_files);
        let support_only = members
            .iter()
            .all(|&id| definitions[id].test || files.files[files.def_file[id]].is_support);
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
    files: FileIndex<'a>,
    area_map: AreaMap,
    degrees: Degrees,
    centrality: Centrality,
    communities: CommunityMap<'a>,
    ranked_limit: usize,
}

impl<'a> Graph<'a> {
    fn new(index: &'a Index, budget: usize) -> Self {
        let files = FileIndex::new(index);
        let degrees = Degrees::new(index, &files);
        let area_map = AreaMap::new(index, &files);
        let centrality = Centrality::new(index, &files, &area_map);
        let communities = communities(index, &files);
        let ranked_limit = (budget / 40).clamp(RANKED, MAX_RANKED);
        Self {
            index,
            files,
            area_map,
            degrees,
            centrality,
            communities,
            ranked_limit,
        }
    }

    #[inline]
    fn definition(&self, id: usize) -> &'a Definition {
        &self.index.definitions[id]
    }

    #[inline]
    fn def_meta(&self, id: usize) -> &FileMeta<'a> {
        &self.files.files[self.files.def_file[id]]
    }

    #[inline]
    fn def_area_id(&self, id: usize) -> usize {
        self.area_map.file_area[self.files.def_file[id]]
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
    for meta in &graph.files.files {
        if meta.in_index_files
            && let Some(language) = meta.indexed_lang
        {
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
    let num_areas = graph.area_map.names.len();
    let mut grouped: Vec<Option<Area<'_>>> = (0..num_areas).map(|_| None).collect();
    for (fid, meta) in graph.files.files.iter().enumerate() {
        if !meta.in_index_files {
            continue;
        }
        let aid = graph.area_map.file_area[fid];
        let area = grouped[aid].get_or_insert_with(|| Area {
            files: Vec::new(),
            symbols: Vec::new(),
            languages: BTreeMap::new(),
        });
        area.files.push(meta.path);
        if let Some(language) = meta.indexed_lang {
            *area.languages.entry(language).or_default() += 1;
        }
    }
    for (id, definition) in graph.index.definitions.iter().enumerate() {
        if definition.kind != "file"
            && let Some(area) = grouped[graph.def_area_id(id)].as_mut()
        {
            area.symbols.push(id);
        }
    }
    let mut ordered: Vec<(usize, Area<'_>)> = grouped
        .into_iter()
        .enumerate()
        .filter_map(|(aid, area)| area.map(|a| (aid, a)))
        .collect();
    ordered.sort_by_key(|(aid, area)| {
        let name = graph.area_map.names[*aid].as_str();
        let tier = if area.symbols.is_empty() {
            2u8
        } else if graph.area_map.is_support[*aid] {
            1u8
        } else {
            0u8
        };
        (
            tier,
            Reverse(area.symbols.len()),
            Reverse(area.files.len()),
            name,
        )
    });
    let mut section = Section::new("areas", format!("Areas · {}", ordered.len()), 5);
    let limit = graph.ranked_limit;
    section.unlisted = ordered.len().saturating_sub(limit);
    for (aid, area) in ordered.iter().take(limit) {
        let name = graph.area_map.names[*aid].as_str();
        let mut languages: Vec<_> = area.languages.iter().collect();
        languages.sort_by_key(|(language, files)| (Reverse(**files), **language));
        let languages: Vec<&str> = languages.iter().take(3).map(|(l, _)| **l).collect();
        let (anchor, anchor_json) = match graph.leader(&area.symbols) {
            Some(id) => {
                let def = graph.definition(id);
                let shown = if def.location.file == name {
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
    let limit = graph.ranked_limit;
    let hub_key = |id: usize| (graph.def_meta(id).is_support, graph.rank(id));
    if hubs.len() > limit {
        hubs.select_nth_unstable_by_key(limit, |&id| hub_key(id));
        hubs[..limit].sort_unstable_by_key(|&id| hub_key(id));
    } else {
        hubs.sort_unstable_by_key(|&id| hub_key(id));
    }
    let mut section = Section::new("hubs", format!("Hubs · {} · in/out", hubs.len()), 6);
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
    let top_two = |ids: &[usize], central_only: bool| -> Vec<usize> {
        let mut best: [Option<usize>; 2] = [None, None];
        for &id in ids {
            if central_only && !central(graph.definition(id)) {
                continue;
            }
            let key = graph.rank(id);
            if best[0].is_none_or(|b0| key < graph.rank(b0)) {
                best[1] = best[0];
                best[0] = Some(id);
            } else if best[1].is_none_or(|b1| key < graph.rank(b1)) {
                best[1] = Some(id);
            }
        }
        best.into_iter().flatten().collect()
    };
    for community in groups.iter().take(limit) {
        let members = &community.members;
        let files = &community.files;
        let mut leaders = top_two(members, true);
        if leaders.is_empty() {
            leaders = top_two(members, false);
        }
        let title = &community.name;
        let single_file = files.len() == 1;
        let mut leader_spots = Vec::with_capacity(leaders.len());
        let mut prev_fid: Option<usize> = None;
        for &id in &leaders {
            let def = graph.definition(id);
            let fid = graph.files.def_file[id];
            let rendered = if single_file || prev_fid == Some(fid) {
                spot_in_file(def)
            } else if let Some(prev) = prev_fid {
                let prev_meta = &graph.files.files[prev];
                if prev_meta.dir != "(root)" && graph.files.files[fid].dir_id == prev_meta.dir_id {
                    spot_under_dir(def, &prev_meta.dir)
                } else {
                    spot(def)
                }
            } else {
                spot(def)
            };
            prev_fid = Some(fid);
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
    let mut pair_counts = HashMap::<(usize, usize), usize>::new();
    let mut fanin_pairs = Vec::<(usize, usize)>::new();
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
        let from_meta = graph.def_meta(from_id);
        let to_meta = graph.def_meta(edge.to);
        let from_lang = from_meta.indexed_lang.unwrap_or("text");
        let to_lang = to_meta.indexed_lang.unwrap_or("text");
        if from_meta.dir_id == to_meta.dir_id && from_lang == to_lang {
            continue;
        }
        let pair = (c1.min(c2), c1.max(c2));
        *pair_counts.entry(pair).or_default() += 1;
        fanin_pairs.push((edge.to, c1));
        qualifying.push((edge, from_id, c1, c2, from_lang, to_lang));
    }

    fanin_pairs.sort_unstable();
    fanin_pairs.dedup();
    let mut target_comm_fanin = HashMap::<usize, usize>::with_capacity(fanin_pairs.len());
    for (to_id, _) in fanin_pairs {
        *target_comm_fanin.entry(to_id).or_default() += 1;
    }

    let mut best_by_pair = HashMap::<(usize, usize), Bridge<'a>>::with_capacity(pair_counts.len());
    for (edge, from_id, c1, c2, from_lang, to_lang) in qualifying {
        let pair = (c1.min(c2), c1.max(c2));
        let pair_links = pair_counts[&pair];
        let rarity = 1.0 / (pair_links as f64).powf(0.75);
        let fanin = target_comm_fanin.get(&edge.to).copied().unwrap_or(0).max(1) as f64;
        let specificity = 1.0 / fanin.sqrt();
        let c1_len = graph.communities.groups[c1].members.len() as f64;
        let c2_len = graph.communities.groups[c2].members.len() as f64;
        let comm_scale = (c1_len.ln_1p() * c2_len.ln_1p()).sqrt();
        let centrality =
            1.0 + (graph.centrality.score[from_id] + graph.centrality.score[edge.to]).ln_1p();
        let boundary = if from_lang != to_lang {
            3.0
        } else if graph.def_area_id(from_id) != graph.def_area_id(edge.to) {
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
        let support_tier = usize::from(graph.def_meta(from_id).is_support)
            + usize::from(graph.def_meta(edge.to).is_support);
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
    fn example_rank<'e>(
        rank_key: &[u64],
        edge: &'e Edge,
    ) -> (u8, bool, Reverse<u64>, &'e Location) {
        (
            kind_rank(edge.kind),
            edge.inferred,
            Reverse(rank_key[edge.to]),
            &edge.location,
        )
    }
    let rank_key = &graph.centrality.rank_key;
    let mut crossings = HashMap::<(usize, usize), Crossing<'_>>::new();
    let mut cached_path = "";
    let mut cached_fid = 0usize;
    for edge in &graph.index.edges {
        let src_path = edge.location.file.as_str();
        let src_fid = if src_path == cached_path && !cached_path.is_empty() {
            cached_fid
        } else {
            cached_path = src_path;
            cached_fid = graph.files.id_of(src_path);
            cached_fid
        };
        let dst_fid = graph.files.def_file[edge.to];
        let from_area = graph.area_map.file_area[src_fid];
        let to_area = graph.area_map.file_area[dst_fid];
        let from_lang = graph.files.files[src_fid].lang.unwrap_or("text");
        let to_lang = graph.files.files[dst_fid].lang.unwrap_or("text");
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
        if example_rank(rank_key, edge) < example_rank(rank_key, crossing.example) {
            crossing.example = edge;
        }
    }
    let mut crossings: Vec<_> = crossings.into_iter().collect();
    crossings.sort_by_key(|((from_aid, to_aid), crossing)| {
        let support_tier = usize::from(graph.area_map.is_support[*from_aid])
            + usize::from(graph.area_map.is_support[*to_aid]);
        (
            support_tier,
            from_aid == to_aid,
            crossing.language_pairs.is_empty(),
            Reverse(crossing.certain),
            Reverse(crossing.count),
            graph.area_map.names[*from_aid].as_str(),
            graph.area_map.names[*to_aid].as_str(),
        )
    });
    let mut section = Section::new(
        "cross_area_links",
        format!("Cross-area links · {}", crossings.len()),
        4,
    );
    let limit = graph.ranked_limit;
    section.unlisted = crossings.len().saturating_sub(limit);
    for ((from_aid, to_aid), crossing) in crossings.iter().take(limit) {
        let from = graph.area_map.names[*from_aid].as_str();
        let to = graph.area_map.names[*to_aid].as_str();
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
        let example_src_meta = &graph.files.files[graph.files.id_of(&example.location.file)];
        let example_dst_meta = graph.def_meta(example.to);
        let target_spot = if from_aid == to_aid {
            if example_src_meta.dir != "(root)"
                && example_dst_meta.dir_id == example_src_meta.dir_id
            {
                spot_under_dir(target_def, &example_src_meta.dir)
            } else {
                spot(target_def)
            }
        } else {
            spot(target_def)
        };
        let example_from_lang = example_src_meta.lang.unwrap_or("text");
        let example_to_lang = example_dst_meta.lang.unwrap_or("text");
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
            && graph.files.def_file[from] != graph.files.def_file[edge.to]
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
            {
                return None;
            }
            let meta = graph.def_meta(id);
            if meta.is_test {
                return None;
            }
            if degrees.dependencies[id] >= 3 {
                Some((2, id))
            } else if degrees.dependencies[id] == 2 && has_cross_file_out[id] && !meta.is_support {
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
    let mut listed = vec![false; definitions.len()];
    entries.retain(|(_, id, _)| {
        let first = !listed[*id];
        listed[*id] = true;
        first
    });
    entries.sort_by_key(|(order, id, _)| {
        (
            graph.def_meta(*id).is_support,
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
    let mut places = BTreeMap::<&str, Place>::new();
    for (id, definition) in graph.index.definitions.iter().enumerate() {
        if definition.test {
            let meta = graph.def_meta(id);
            let place = places.entry(meta.dir.as_str()).or_default();
            place.files.insert(meta.path);
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
    for meta in &graph.files.files {
        if meta.in_index_files && meta.is_test {
            places
                .entry(meta.dir.as_str())
                .or_default()
                .files
                .insert(meta.path);
        }
    }
    let mut places: Vec<_> = places.into_iter().collect();
    places.sort_by_key(|&(name, ref place)| {
        (
            is_fixture_test_dir(name),
            Reverse(place.tests),
            Reverse(place.files.len()),
            name,
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
                    && let Some(rel) = first.strip_prefix(dir)
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
        && !graph.def_meta(id).is_test;
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
    let self_meta = graph.def_meta(id);
    let self_file = self_meta.path;
    let self_dir_id = self_meta.dir_id;
    let self_lang = self_meta.indexed_lang.unwrap_or("text");
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
        let other_meta = graph.def_meta(other);
        let other_lang = other_meta.indexed_lang.unwrap_or("text");
        let other_comm = graph.communities.of[other];
        let crosses_lang = other_lang != self_lang;
        let crosses_comm = match (self_comm, other_comm) {
            (Some(c1), Some(c2)) => c1 != c2,
            (Some(_), None) | (None, Some(_)) => other_meta.dir_id != self_dir_id,
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
    let (shown_bytes, shown) = emit_measured(
        &header,
        &sections,
        &footer,
        &EXPLAIN_ORDER,
        Fill::RoundRobin,
        budget,
        json.then_some(summary),
    )?;
    let mut files = collect_section_files(&sections, &shown);
    files.insert(def.location.file.as_str());
    crate::history::record_graph_files(workspace, "map", shown_bytes, files);
    Ok(())
}

// ---------------------------------------------------------------------------
// Offline interactive HTML graph explorer (`justless map --html`)

const HTML_MAX_NODES: usize = 250;
const HTML_MAX_EDGES: usize = 1200;
const HTML_MAX_BRIDGES: usize = 40;
const HTML_MAX_COMMUNITIES: usize = 48;
const HTML_FOCUS_NEIGHBORS: usize = 60;

struct HtmlOutput {
    html: String,
    nodes: usize,
    edges: usize,
    communities: usize,
    referenced_files: Vec<String>,
}

/// Escapes text for safe insertion into HTML element content (such as `<title>`).
fn escape_html_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\u{2028}' => out.push_str("&#x2028;"),
            '\u{2029}' => out.push_str("&#x2029;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Escapes serialized JSON for safe embedding inside a `<script>` element.
/// Every `<`, `>`, `&`, U+2028, and U+2029 becomes a standard JSON `\uXXXX`
/// escape so untrusted symbol or file names can never break out of `</script>`.
fn escape_script_json(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(ch),
        }
    }
    out
}

/// Renders a self-contained, zero-CDN, XSS-safe interactive HTML graph explorer.
#[cfg(test)]
pub(crate) fn render_html(
    index: &Index,
    title: &str,
    invocation: &str,
    focus: Option<usize>,
) -> String {
    build_html(index, title, invocation, focus).html
}

fn build_html(index: &Index, title: &str, invocation: &str, focus: Option<usize>) -> HtmlOutput {
    let graph = Graph::new(index, MAX_RANKED * 40);
    let all_bridges = find_bridges(&graph);
    let def_count = index.definitions.len();

    // Select up to HTML_MAX_NODES symbols: focused neighborhood (if `--explain`),
    // top cross-community bridge endpoints, leaders from each community, and the
    // highest-PageRank remaining hubs.
    let mut selected_mask = vec![false; def_count];
    let mut selected = Vec::<usize>::with_capacity(HTML_MAX_NODES.min(def_count));
    fn try_add(
        graph: &Graph<'_>,
        selected_mask: &mut [bool],
        selected: &mut Vec<usize>,
        id: usize,
        allow_file: bool,
    ) -> bool {
        if id >= selected_mask.len() || selected_mask[id] || selected.len() >= HTML_MAX_NODES {
            return false;
        }
        if !allow_file && graph.definition(id).kind == "file" {
            return false;
        }
        selected_mask[id] = true;
        selected.push(id);
        true
    }

    if let Some(fid) = focus
        && fid < def_count
    {
        try_add(&graph, &mut selected_mask, &mut selected, fid, true);
        let mut incoming: Vec<(bool, usize)> = index
            .incoming(fid)
            .filter_map(|e| e.from.filter(|&u| u != fid).map(|u| (e.inferred, u)))
            .collect();
        incoming.sort_by_key(|&(inf, id)| (inf, graph.rank(id)));
        incoming.dedup_by_key(|item| item.1);
        for (_, id) in incoming.into_iter().take(HTML_FOCUS_NEIGHBORS) {
            try_add(&graph, &mut selected_mask, &mut selected, id, false);
        }
        let mut outgoing: Vec<(bool, usize)> = index
            .outgoing(fid)
            .filter(|e| e.to != fid)
            .map(|e| (e.inferred, e.to))
            .collect();
        outgoing.sort_by_key(|&(inf, id)| (inf, graph.rank(id)));
        outgoing.dedup_by_key(|item| item.1);
        for (_, id) in outgoing.into_iter().take(HTML_FOCUS_NEIGHBORS) {
            try_add(&graph, &mut selected_mask, &mut selected, id, false);
        }
    }

    for bridge in all_bridges.iter().take(HTML_MAX_BRIDGES) {
        let need =
            usize::from(!selected_mask[bridge.from_id]) + usize::from(!selected_mask[bridge.to_id]);
        if selected.len() + need <= HTML_MAX_NODES {
            try_add(
                &graph,
                &mut selected_mask,
                &mut selected,
                bridge.from_id,
                false,
            );
            try_add(
                &graph,
                &mut selected_mask,
                &mut selected,
                bridge.to_id,
                false,
            );
        }
    }

    for community in graph.communities.groups.iter().take(HTML_MAX_COMMUNITIES) {
        if selected.len() >= HTML_MAX_NODES {
            break;
        }
        let mut best: [Option<usize>; 2] = [None, None];
        for &id in &community.members {
            let key = (!central(graph.definition(id)), graph.rank(id));
            if best[0].is_none_or(|b| key < (!central(graph.definition(b)), graph.rank(b))) {
                best[1] = best[0];
                best[0] = Some(id);
            } else if best[1].is_none_or(|b| key < (!central(graph.definition(b)), graph.rank(b))) {
                best[1] = Some(id);
            }
        }
        for id in best.into_iter().flatten() {
            try_add(&graph, &mut selected_mask, &mut selected, id, false);
        }
    }

    if selected.len() < HTML_MAX_NODES {
        let remaining_slots = HTML_MAX_NODES - selected.len();
        let mut rest: Vec<usize> = (0..def_count)
            .filter(|&id| !selected_mask[id] && graph.definition(id).kind != "file")
            .collect();
        let rank_key = |id: usize| {
            (
                graph.def_meta(id).is_support,
                !central(graph.definition(id)),
                graph.rank(id),
            )
        };
        if rest.len() > remaining_slots {
            rest.select_nth_unstable_by_key(remaining_slots, |&id| rank_key(id));
            rest.truncate(remaining_slots);
        }
        rest.sort_unstable_by_key(|&id| rank_key(id));
        for id in rest {
            try_add(&graph, &mut selected_mask, &mut selected, id, false);
        }
    }

    selected.sort_unstable_by_key(|&id| {
        (
            focus != Some(id),
            graph.def_meta(id).is_support,
            !central(graph.definition(id)),
            graph.rank(id),
        )
    });

    let mut def_to_node = vec![usize::MAX; def_count];
    for (idx, &def_id) in selected.iter().enumerate() {
        def_to_node[def_id] = idx;
    }
    let focus_idx = focus
        .filter(|&id| id < def_count && def_to_node[id] != usize::MAX)
        .map(|id| def_to_node[id]);

    // Areas summary (aligned with `area_section` ordering).
    let num_areas = graph.area_map.names.len();
    let mut area_files = vec![0usize; num_areas];
    let mut area_symbols = vec![0usize; num_areas];
    for (fid, meta) in graph.files.files.iter().enumerate() {
        if meta.in_index_files {
            area_files[graph.area_map.file_area[fid]] += 1;
        }
    }
    for (id, def) in index.definitions.iter().enumerate() {
        if def.kind != "file" {
            area_symbols[graph.def_area_id(id)] += 1;
        }
    }
    let mut ordered_areas: Vec<usize> = (0..num_areas).filter(|&a| area_files[a] > 0).collect();
    ordered_areas.sort_by_key(|&a| {
        let tier = if area_symbols[a] == 0 {
            2u8
        } else if graph.area_map.is_support[a] {
            1u8
        } else {
            0u8
        };
        (
            tier,
            Reverse(area_symbols[a]),
            Reverse(area_files[a]),
            graph.area_map.names[a].as_str(),
        )
    });
    let mut area_to_idx = vec![0usize; num_areas];
    let mut areas_json = Vec::with_capacity(ordered_areas.len());
    for (idx, &aid) in ordered_areas.iter().enumerate() {
        area_to_idx[aid] = idx;
        areas_json.push(json!({
            "id": idx,
            "name": graph.area_map.names[aid],
            "files": area_files[aid],
            "symbols": area_symbols[aid],
            "support": graph.area_map.is_support[aid],
        }));
    }

    // Communities represented in the graph.
    let mut comm_shown = vec![0usize; graph.communities.groups.len()];
    for &def_id in &selected {
        if let Some(cid) = graph.communities.of[def_id] {
            comm_shown[cid] += 1;
        }
    }
    let mut comm_to_idx = vec![None::<usize>; graph.communities.groups.len()];
    let mut communities_json = Vec::new();
    for (cid, comm) in graph.communities.groups.iter().enumerate() {
        if comm_shown[cid] == 0 {
            continue;
        }
        let idx = communities_json.len();
        comm_to_idx[cid] = Some(idx);
        communities_json.push(json!({
            "id": idx,
            "name": comm.name,
            "symbols": comm.members.len(),
            "shown": comm_shown[cid],
            "files": comm.files.len(),
            "support": comm.support_only,
        }));
    }

    // Top ranked architectural bridges whose endpoints are both in the canvas.
    let mut top_bridge_pairs = HashMap::<(usize, usize), f64>::new();
    let mut bridges_json = Vec::new();
    for bridge in all_bridges.iter().take(HTML_MAX_BRIDGES) {
        let from_n = def_to_node[bridge.from_id];
        let to_n = def_to_node[bridge.to_id];
        if from_n == usize::MAX || to_n == usize::MAX {
            continue;
        }
        top_bridge_pairs.insert((bridge.from_id, bridge.to_id), bridge.score);
        bridges_json.push(json!({
            "from": from_n,
            "to": to_n,
            "from_comm": comm_to_idx[bridge.from_comm],
            "to_comm": comm_to_idx[bridge.to_comm],
            "from_lang": bridge.from_lang,
            "to_lang": bridge.to_lang,
            "kind": relationship(bridge.edge.kind),
            "possible": bridge.edge.inferred,
            "pair_links": bridge.pair_links,
            "score": (bridge.score * 1000.0).round() / 1000.0,
        }));
    }

    // Coalesce edges between included nodes.
    struct EdgeAcc {
        kind: Kind,
        possible: bool,
        bridge: bool,
        top_bridge: bool,
        count: usize,
        priority: (bool, u8),
    }
    let mut edge_map = HashMap::<(usize, usize), EdgeAcc>::new();
    for edge in &index.edges {
        let Some(from_id) = edge.from else {
            continue;
        };
        if from_id == edge.to {
            continue;
        }
        let s = def_to_node[from_id];
        let t = def_to_node[edge.to];
        if s == usize::MAX || t == usize::MAX {
            continue;
        }
        let cross_comm = matches!(
            (graph.communities.of[from_id], graph.communities.of[edge.to]),
            (Some(c1), Some(c2)) if c1 != c2
        );
        let top_bridge = top_bridge_pairs.contains_key(&(from_id, edge.to));
        let prio = (edge.inferred, kind_rank(edge.kind));
        edge_map
            .entry((s, t))
            .and_modify(|acc| {
                acc.count += 1;
                acc.bridge |= cross_comm;
                acc.top_bridge |= top_bridge;
                if prio < acc.priority {
                    acc.kind = edge.kind;
                    acc.possible = edge.inferred;
                    acc.priority = prio;
                }
            })
            .or_insert(EdgeAcc {
                kind: edge.kind,
                possible: edge.inferred,
                bridge: cross_comm,
                top_bridge,
                count: 1,
                priority: prio,
            });
    }

    let mut edge_list: Vec<((usize, usize), EdgeAcc)> = edge_map.into_iter().collect();
    if edge_list.len() > HTML_MAX_EDGES {
        edge_list.sort_unstable_by_key(|&((s, t), ref acc)| {
            let touches_focus = focus_idx.is_some_and(|f| f == s || f == t);
            let combined_pr = graph.centrality.rank_key[selected[s]]
                .saturating_add(graph.centrality.rank_key[selected[t]]);
            (
                !touches_focus,
                !acc.top_bridge,
                !acc.bridge,
                acc.possible,
                Reverse(acc.count),
                Reverse(combined_pr),
                s,
                t,
            )
        });
        edge_list.truncate(HTML_MAX_EDGES);
    }
    edge_list.sort_unstable_by_key(|&((s, t), _)| (s, t));

    let edges_json: Vec<Value> = edge_list
        .iter()
        .map(|&((s, t), ref acc)| {
            json!({
                "from": s,
                "to": t,
                "kind": relationship(acc.kind),
                "possible": acc.possible,
                "bridge": acc.bridge,
                "top_bridge": acc.top_bridge,
                "count": acc.count,
            })
        })
        .collect();

    let nodes_json: Vec<Value> = selected
        .iter()
        .enumerate()
        .map(|(idx, &def_id)| {
            let def = graph.definition(def_id);
            let meta = graph.def_meta(def_id);
            let comm = graph.communities.of[def_id].and_then(|cid| comm_to_idx[cid]);
            let pr = (graph.centrality.score[def_id] * 1000.0).round() / 1000.0;
            json!({
                "id": idx,
                "name": name(def),
                "qualified": def.qualified,
                "kind": kind(def),
                "file": def.location.file,
                "start": def.location.start,
                "end": def.location.end,
                "lang": meta.indexed_lang.unwrap_or("text"),
                "area": area_to_idx[graph.def_area_id(def_id)],
                "comm": comm,
                "pr": pr,
                "in_deg": graph.degrees.dependents[def_id],
                "out_deg": graph.degrees.dependencies[def_id],
                "caller_files": graph.centrality.caller_files[def_id],
                "caller_areas": graph.centrality.caller_areas[def_id],
                "support": meta.is_support,
            })
        })
        .collect();

    // Repository summary metrics & language breakdown.
    let total_symbols = index
        .definitions
        .iter()
        .filter(|d| d.kind != "file")
        .count();
    let possible_links = index.edges.iter().filter(|e| e.inferred).count();
    let mut lang_counts = BTreeMap::<&str, usize>::new();
    for meta in &graph.files.files {
        if meta.in_index_files
            && let Some(lang) = meta.indexed_lang
        {
            *lang_counts.entry(lang).or_default() += 1;
        }
    }
    let mut languages: Vec<(&str, usize)> = lang_counts.into_iter().collect();
    languages.sort_by_key(|&(lang, count)| (Reverse(count), lang));

    let focus_symbol = focus_idx.map(|idx| name(graph.definition(selected[idx])));
    let payload = json!({
        "summary": {
            "title": title,
            "files": index.files.len(),
            "symbols": total_symbols,
            "links": index.edges.len(),
            "possible_links": possible_links,
            "shown_nodes": nodes_json.len(),
            "shown_edges": edges_json.len(),
            "communities": graph.communities.groups.len(),
            "isolated_symbols": graph.communities.isolated_symbols,
            "bridges": all_bridges.len(),
            "languages": languages,
            "invocation": invocation,
            "focus_symbol": focus_symbol,
        },
        "focus": focus_idx,
        "areas": areas_json,
        "communities": communities_json,
        "bridges": bridges_json,
        "nodes": nodes_json,
        "edges": edges_json,
    });

    let raw_json = serde_json::to_string(&payload).expect("map HTML payload serializes");
    let safe_json = escape_script_json(&raw_json);
    let page_title = match focus_symbol {
        Some(sym) => format!(
            "Justless Map · {} · {}",
            escape_html_text(sym),
            escape_html_text(title)
        ),
        None => format!("Justless Map · {}", escape_html_text(title)),
    };
    let (before_title, rest) = HTML_TEMPLATE
        .split_once("__JUSTLESS_TITLE__")
        .expect("HTML template contains title placeholder");
    let (between, after_json) = rest
        .split_once("__JUSTLESS_JSON__")
        .expect("HTML template contains JSON placeholder");
    let mut html = String::with_capacity(
        before_title.len() + page_title.len() + between.len() + safe_json.len() + after_json.len(),
    );
    html.push_str(before_title);
    html.push_str(&page_title);
    html.push_str(between);
    html.push_str(&safe_json);
    html.push_str(after_json);

    let referenced_files: Vec<String> = selected
        .iter()
        .map(|&id| index.definitions[id].location.file.as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(str::to_owned)
        .collect();

    HtmlOutput {
        html,
        nodes: selected.len(),
        edges: edge_list.len(),
        communities: communities_json.len(),
        referenced_files,
    }
}

const HTML_TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
<title>__JUSTLESS_TITLE__</title>
<style>
:root{color-scheme:dark;--bg:#0b0f17;--panel:#111827;--panel2:#182133;--border:#243044;--text:#e5edf7;--muted:#8a99b2;--accent:#5b8def;--bridge:#f59e0b;--in:#2dd4bf;--out:#818cf8;--iso:#7b8798;--pill:#162033}
:root[data-theme="light"]{color-scheme:light;--bg:#f6f8fb;--panel:#ffffff;--panel2:#eef2f7;--border:#d4dce8;--text:#0f172a;--muted:#526077;--accent:#2563eb;--bridge:#d97706;--in:#0d9488;--out:#4f46e5;--iso:#64748b;--pill:#edf2fa}
*{box-sizing:border-box}
html,body{margin:0;height:100%;overflow:hidden;background:var(--bg);color:var(--text);font:13px/1.4 ui-sans-serif,system-ui,-apple-system,Segoe UI,Roboto,sans-serif}
.app{display:flex;flex-direction:column;height:100%}
header{display:flex;align-items:center;gap:10px;padding:8px 12px;background:var(--panel);border-bottom:1px solid var(--border);flex-wrap:wrap}
.brand{font-weight:700;letter-spacing:-.01em;white-space:nowrap}
.stats{color:var(--muted);font-size:12px;white-space:nowrap}
.spacer{flex:1}
.search-wrap{display:flex;align-items:center;gap:6px;min-width:190px;max-width:320px;flex:1}
input[type="search"]{width:100%;padding:5px 9px;border-radius:6px;border:1px solid var(--border);background:var(--panel2);color:var(--text);font:inherit;outline:none}
input[type="search"]:focus{border-color:var(--accent)}
.btn{padding:5px 9px;border-radius:6px;border:1px solid var(--border);background:var(--panel2);color:var(--text);font:12px/1.2 inherit;cursor:pointer;white-space:nowrap}
.btn:hover{border-color:var(--accent)}
.btn.active{background:var(--accent);border-color:var(--accent);color:#fff}
.btn.bridge.active{background:var(--bridge);border-color:var(--bridge);color:#111}
.bar{display:flex;align-items:center;gap:6px;padding:6px 12px;background:var(--panel);border-bottom:1px solid var(--border);overflow-x:auto;scrollbar-width:thin}
.pill{display:inline-flex;align-items:center;gap:6px;padding:3px 9px;border-radius:999px;border:1px solid var(--border);background:var(--pill);color:var(--text);font-size:12px;cursor:pointer;white-space:nowrap}
.pill.active{border-color:var(--accent);box-shadow:inset 0 0 0 1px var(--accent)}
.dot{width:8px;height:8px;border-radius:50%;flex:none}
.main{display:flex;flex:1;min-height:0}
.stage{position:relative;flex:1;min-width:0}
canvas{display:block;width:100%;height:100%;cursor:grab}
canvas.dragging{cursor:grabbing}
canvas.hover{cursor:pointer}
.tip{position:absolute;pointer-events:none;padding:4px 8px;border-radius:6px;background:var(--panel);border:1px solid var(--border);font-size:12px;box-shadow:0 6px 18px rgba(0,0,0,.28);display:none;max-width:320px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.hint{position:absolute;left:10px;bottom:8px;font-size:11px;color:var(--muted);background:color-mix(in srgb,var(--panel) 84%,transparent);padding:3px 8px;border-radius:6px;border:1px solid var(--border);pointer-events:none}
aside{width:330px;max-width:42vw;background:var(--panel);border-left:1px solid var(--border);overflow-y:auto;padding:12px;display:flex;flex-direction:column;gap:12px}
.section-title{font-size:11px;font-weight:700;text-transform:uppercase;letter-spacing:.06em;color:var(--muted);margin:0 0 6px}
.kv-grid{display:grid;grid-template-columns:1fr 1fr;gap:6px}
.card{padding:7px 9px;border-radius:7px;background:var(--panel2);border:1px solid var(--border)}
.card .label{font-size:11px;color:var(--muted)}
.card .val{font-size:14px;font-weight:600;margin-top:1px}
.sym-head{display:flex;align-items:flex-start;justify-content:space-between;gap:8px}
.sym-name{font-size:15px;font-weight:700;word-break:break-word}
.mono{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:11.5px;color:var(--muted);word-break:break-all}
.badges{display:flex;flex-wrap:wrap;gap:5px;margin-top:6px}
.badge{padding:2px 7px;border-radius:999px;background:var(--panel2);border:1px solid var(--border);font-size:11px}
.cmd-row{display:flex;align-items:center;gap:6px;padding:5px 7px;border-radius:6px;background:var(--panel2);border:1px solid var(--border);margin-bottom:5px}
.cmd-row code{flex:1;font:11.5px/1.35 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;overflow-x:auto;white-space:nowrap}
.list{display:flex;flex-direction:column;gap:4px}
.item{display:flex;align-items:center;justify-content:space-between;gap:8px;width:100%;text-align:left;padding:6px 8px;border-radius:6px;border:1px solid var(--border);background:var(--panel2);color:var(--text);font:inherit;cursor:pointer}
.item:hover{border-color:var(--accent)}
.item .sub{font-size:11px;color:var(--muted);white-space:nowrap}
.item .title{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;min-width:0}
.tag-b{color:var(--bridge);font-weight:600}
@media(max-width:860px){.main{flex-direction:column}aside{width:100%;max-width:none;height:38vh;border-left:0;border-top:1px solid var(--border)}}
</style>
</head>
<body>
<div class="app">
  <header>
    <span class="brand" id="brand"></span>
    <span class="stats" id="stats"></span>
    <span class="spacer"></span>
    <div class="search-wrap">
      <input id="search" type="search" placeholder="Search symbol, file, kind (/)..." aria-label="Search symbols">
      <span class="stats" id="matchCount"></span>
    </div>
    <button class="btn bridge" id="bridgeBtn" type="button"></button>
    <button class="btn" id="resetBtn" type="button">Reset</button>
    <button class="btn" id="themeBtn" type="button"></button>
  </header>
  <div class="bar" id="commBar"></div>
  <div class="main">
    <div class="stage" id="stage">
      <canvas id="cv"></canvas>
      <div class="tip" id="tip"></div>
      <div class="hint">Drag to pan · Scroll to zoom · Drag nodes · / Search · R Reset · Esc Clear</div>
    </div>
    <aside id="sidebar"></aside>
  </div>
</div>
<script id="justless-graph" type="application/json">__JUSTLESS_JSON__</script>
<script>
(() => {
  const DATA = JSON.parse(document.getElementById('justless-graph').textContent);
  const PALETTE = ['#5b8def','#38bdf8','#34d399','#a78bfa','#f472b6','#fbbf24','#fb7185','#2dd4bf','#818cf8','#a3e635','#f97316','#e879f9'];
  const nodes = DATA.nodes, edges = DATA.edges, comms = DATA.communities, bridges = DATA.bridges, sum = DATA.summary;
  const outEdges = nodes.map(() => []), inEdges = nodes.map(() => []), nbrs = nodes.map(() => new Set());
  const bridgeNodes = new Set();
  let bridgeEdgeCount = 0;
  for (const e of edges) {
    outEdges[e.from].push(e); inEdges[e.to].push(e);
    nbrs[e.from].add(e.to); nbrs[e.to].add(e.from);
    if (e.bridge) { bridgeNodes.add(e.from); bridgeNodes.add(e.to); bridgeEdgeCount++; }
  }
  const el = (tag, cls, text) => { const n = document.createElement(tag); if (cls) n.className = cls; if (text !== undefined) n.textContent = text; return n; };
  const shQuote = s => /^[A-Za-z0-9_./:@+-]+$/.test(s) ? s : "'" + s.replace(/'/g, "'\\''") + "'";
  const colorOf = c => c === null || c === undefined ? getCss('--iso') : PALETTE[c % PALETTE.length];
  const getCss = k => getComputedStyle(document.documentElement).getPropertyValue(k).trim();

  const cv = document.getElementById('cv'), ctx = cv.getContext('2d');
  const stage = document.getElementById('stage'), tip = document.getElementById('tip');
  const sidebar = document.getElementById('sidebar'), commBar = document.getElementById('commBar');
  const searchEl = document.getElementById('search'), matchCount = document.getElementById('matchCount');
  const bridgeBtn = document.getElementById('bridgeBtn'), resetBtn = document.getElementById('resetBtn'), themeBtn = document.getElementById('themeBtn');

  document.getElementById('brand').textContent = 'Justless · ' + sum.title;
  const capNote = sum.symbols > sum.shown_nodes ? ' (top ' + sum.shown_nodes + ' shown)' : '';
  document.getElementById('stats').textContent = sum.files + ' files · ' + sum.symbols + ' symbols' + capNote + ' · ' + sum.links + ' links · ' + sum.communities + ' communities';
  bridgeBtn.textContent = 'Bridges (' + bridgeEdgeCount + ')';

  const state = { selected: DATA.focus, hovered: null, comm: null, bridgesOnly: false, query: '', matches: new Set(), theme: 'dark' };
  const cam = { x: 0, y: 0, k: 1 };
  let W = 800, H = 600, dpr = 1;

  // Compute radii and community-clustered initial coordinates.
  const commAnchors = [];
  const C = comms.length, R = C <= 1 ? 0 : Math.max(185, Math.sqrt(Math.max(nodes.length, 1)) * 30);
  for (let i = 0; i < C; i++) {
    const a = (i / C) * Math.PI * 2 - Math.PI / 2;
    commAnchors.push({ x: Math.cos(a) * R, y: Math.sin(a) * R });
  }
  const commIdxCount = comms.map(() => 0);
  let isoIdx = 0;
  for (const n of nodes) {
    n.r = 5.5 + Math.min(15, Math.log1p(n.pr * 6 + (n.in_deg + n.out_deg) * 0.35) * 3.4);
    n.vx = 0; n.vy = 0;
    if (n.comm !== null && n.comm !== undefined && commAnchors[n.comm]) {
      const k = commIdxCount[n.comm]++, a = k * 2.39996323, rad = k === 0 ? 0 : 22 + Math.sqrt(k) * 26;
      n.ax = commAnchors[n.comm].x; n.ay = commAnchors[n.comm].y;
      n.x = n.ax + Math.cos(a) * rad; n.y = n.ay + Math.sin(a) * rad;
    } else {
      const k = isoIdx++, a = k * 2.39996323, rad = R + 125 + (k % 3) * 28;
      n.ax = Math.cos(a) * rad; n.ay = Math.sin(a) * rad;
      n.x = n.ax; n.y = n.ay;
    }
  }

  let alpha = 1, ticks = 0, raf = 0, pinned = null;
  function simTick() {
    const N = nodes.length;
    if (N === 0) return false;
    for (let i = 0; i < N; i++) {
      const a = nodes[i];
      for (let j = i + 1; j < N; j++) {
        const b = nodes[j];
        const dx = b.x - a.x, dy = b.y - a.y, d2 = dx * dx + dy * dy + 420;
        const f = (1900 * alpha) / d2, d = Math.sqrt(d2);
        const fx = (dx / d) * f, fy = (dy / d) * f;
        a.vx -= fx; a.vy -= fy; b.vx += fx; b.vy += fy;
      }
    }
    for (const e of edges) {
      const a = nodes[e.from], b = nodes[e.to];
      const dx = b.x - a.x, dy = b.y - a.y, d = Math.sqrt(dx * dx + dy * dy) || 1;
      const target = e.bridge ? 205 : 72, str = (e.bridge ? 0.015 : 0.055) * alpha;
      const f = (d - target) * str, fx = (dx / d) * f, fy = (dy / d) * f;
      a.vx += fx; a.vy += fy; b.vx -= fx; b.vy -= fy;
    }
    for (const n of nodes) {
      if (n.id === pinned) { n.vx = 0; n.vy = 0; continue; }
      const g = (n.comm === null ? 0.016 : 0.048) * alpha;
      n.vx = (n.vx + (n.ax - n.x) * g) * 0.78;
      n.vy = (n.vy + (n.ay - n.y) * g) * 0.78;
      n.x += Math.max(-24, Math.min(24, n.vx));
      n.y += Math.max(-24, Math.min(24, n.vy));
    }
    alpha *= 0.965; ticks++;
    return alpha >= 0.015 && ticks < 140;
  }
  for (let i = 0; i < 60 && simTick(); i++) {}

  function resize() {
    const r = stage.getBoundingClientRect();
    W = Math.max(1, r.width); H = Math.max(1, r.height);
    dpr = window.devicePixelRatio || 1;
    cv.width = Math.round(W * dpr); cv.height = Math.round(H * dpr);
    draw();
  }
  function fitView(subset) {
    const list = subset && subset.length ? subset : nodes;
    if (!list.length) { cam.x = W / 2; cam.y = H / 2; cam.k = 1; draw(); return; }
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const n of list) { x0 = Math.min(x0, n.x - n.r); y0 = Math.min(y0, n.y - n.r); x1 = Math.max(x1, n.x + n.r); y1 = Math.max(y1, n.y + n.r); }
    const pad = 64, bw = Math.max(80, x1 - x0), bh = Math.max(80, y1 - y0);
    cam.k = Math.max(0.2, Math.min(2.2, Math.min((W - pad * 2) / bw, (H - pad * 2) / bh)));
    cam.x = W / 2 - ((x0 + x1) / 2) * cam.k;
    cam.y = H / 2 - ((y0 + y1) / 2) * cam.k;
    draw();
  }
  function centerOn(id) {
    const n = nodes[id]; if (!n) return;
    cam.k = Math.max(cam.k, 1.05);
    cam.x = W / 2 - n.x * cam.k; cam.y = H / 2 - n.y * cam.k;
    draw();
  }
  function nodeDimmed(n) {
    if (state.comm !== null) {
      const inComm = state.comm === -1 ? n.comm === null : n.comm === state.comm;
      if (!inComm) return true;
    }
    if (state.bridgesOnly && !bridgeNodes.has(n.id)) return true;
    if (state.query && !state.matches.has(n.id)) return true;
    const focus = state.hovered !== null ? state.hovered : state.selected;
    if (focus !== null && n.id !== focus && !nbrs[focus].has(n.id)) return true;
    return false;
  }

  function draw() {
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = getCss('--bg');
    ctx.fillRect(0, 0, W, H);
    if (!nodes.length) {
      ctx.fillStyle = getCss('--muted'); ctx.font = '14px system-ui,sans-serif'; ctx.textAlign = 'center';
      ctx.fillText('No indexed symbols in this repository.', W / 2, H / 2);
      return;
    }
    const textCol = getCss('--text'), mutedCol = getCss('--muted'), bgCol = getCss('--bg');
    const bridgeCol = getCss('--bridge'), inCol = getCss('--in'), outCol = getCss('--out'), accCol = getCss('--accent');
    const focus = state.hovered !== null ? state.hovered : state.selected;

    ctx.save();
    ctx.translate(cam.x, cam.y);
    ctx.scale(cam.k, cam.k);

    // Subtle community halos and cluster names.
    for (let c = 0; c < comms.length; c++) {
      if (state.comm !== null && state.comm !== c) continue;
      let sx = 0, sy = 0, cnt = 0;
      for (const n of nodes) if (n.comm === c) { sx += n.x; sy += n.y; cnt++; }
      if (!cnt) continue;
      const cx = sx / cnt, cy = sy / cnt;
      let maxD = 36;
      for (const n of nodes) if (n.comm === c) maxD = Math.max(maxD, Math.hypot(n.x - cx, n.y - cy) + n.r + 18);
      ctx.beginPath(); ctx.arc(cx, cy, maxD, 0, Math.PI * 2);
      ctx.fillStyle = colorOf(c) + '12'; ctx.fill();
      ctx.strokeStyle = colorOf(c) + '30'; ctx.lineWidth = 1 / cam.k; ctx.stroke();
      ctx.fillStyle = mutedCol; ctx.font = (11 / Math.sqrt(Math.max(0.5, cam.k))) + 'px system-ui,sans-serif';
      ctx.textAlign = 'center'; ctx.fillText(comms[c].name, cx, cy - maxD - 6 / cam.k);
    }

    // Directed curved edges.
    for (const e of edges) {
      const a = nodes[e.from], b = nodes[e.to];
      const touchesFocus = focus !== null && (e.from === focus || e.to === focus);
      const dimmed = state.bridgesOnly ? !e.bridge : (nodeDimmed(a) && nodeDimmed(b) && !touchesFocus);
      const isBridge = e.bridge;
      ctx.save();
      ctx.globalAlpha = touchesFocus ? 0.92 : dimmed ? 0.07 : isBridge ? (state.bridgesOnly ? 0.92 : 0.62) : 0.28;
      ctx.strokeStyle = touchesFocus ? (e.to === focus ? inCol : outCol) : isBridge ? bridgeCol : mutedCol;
      ctx.lineWidth = (touchesFocus ? 2 : isBridge && (state.bridgesOnly || e.top_bridge) ? 1.8 : 1.1) / Math.sqrt(Math.max(0.4, cam.k));
      if (e.possible) ctx.setLineDash([4 / cam.k, 3 / cam.k]);
      const dx = b.x - a.x, dy = b.y - a.y, dist = Math.hypot(dx, dy) || 1;
      const bend = Math.min(24, dist * 0.12);
      const mx = (a.x + b.x) / 2 - (dy / dist) * bend, my = (a.y + b.y) / 2 + (dx / dist) * bend;
      ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.quadraticCurveTo(mx, my, b.x, b.y); ctx.stroke();
      if (touchesFocus || (isBridge && state.bridgesOnly) || cam.k > 0.9) {
        const ang = Math.atan2(b.y - my, b.x - mx), tipX = b.x - Math.cos(ang) * (b.r + 3), tipY = b.y - Math.sin(ang) * (b.r + 3), sz = 5.5 / Math.sqrt(Math.max(0.5, cam.k));
        ctx.setLineDash([]); ctx.fillStyle = ctx.strokeStyle; ctx.beginPath();
        ctx.moveTo(tipX, tipY); ctx.lineTo(tipX - Math.cos(ang - 0.45) * sz, tipY - Math.sin(ang - 0.45) * sz);
        ctx.lineTo(tipX - Math.cos(ang + 0.45) * sz, tipY - Math.sin(ang + 0.45) * sz); ctx.closePath(); ctx.fill();
      }
      ctx.restore();
    }

    // Nodes.
    for (const n of nodes) {
      const dim = nodeDimmed(n), sel = n.id === state.selected, hov = n.id === state.hovered, matched = state.matches.has(n.id);
      ctx.save();
      ctx.globalAlpha = dim ? 0.16 : 1;
      ctx.beginPath(); ctx.arc(n.x, n.y, n.r, 0, Math.PI * 2);
      ctx.fillStyle = colorOf(n.comm); ctx.fill();
      ctx.lineWidth = (sel || hov || matched ? 2.4 : 1.1) / cam.k;
      ctx.strokeStyle = sel || matched ? accCol : hov ? textCol : bgCol;
      ctx.stroke();
      if (sel) {
        ctx.beginPath(); ctx.arc(n.x, n.y, n.r + 4.5 / cam.k, 0, Math.PI * 2);
        ctx.strokeStyle = accCol; ctx.lineWidth = 1.6 / cam.k; ctx.stroke();
      }
      const showLabel = sel || hov || matched || (focus !== null && nbrs[focus].has(n.id)) || (!dim && (n.id < 16 || cam.k > 1.15));
      if (showLabel) {
        ctx.font = (sel || hov ? '600 ' : '') + (11.5 / Math.sqrt(Math.max(0.55, cam.k))) + 'px system-ui,sans-serif';
        ctx.textAlign = 'center'; ctx.textBaseline = 'top';
        const chars = [...n.name];
        const label = chars.length > 28 ? chars.slice(0, 27).join('') + '…' : n.name;
        ctx.lineWidth = 3 / cam.k; ctx.strokeStyle = bgCol; ctx.strokeText(label, n.x, n.y + n.r + 3 / cam.k);
        ctx.fillStyle = textCol; ctx.fillText(label, n.x, n.y + n.r + 3 / cam.k);
      }
      ctx.restore();
    }
    ctx.restore();
  }

  function renderCommBar() {
    commBar.replaceChildren();
    const mk = (label, color, val) => {
      const b = el('button', 'pill' + (state.comm === val ? ' active' : '')); b.type = 'button';
      if (color) { const d = el('span', 'dot'); d.style.background = color; b.appendChild(d); }
      b.appendChild(document.createTextNode(label));
      b.onclick = () => {
        state.comm = state.comm === val ? null : val;
        renderCommBar();
        if (state.comm === null) fitView(nodes);
        else fitView(nodes.filter(n => state.comm === -1 ? n.comm === null : n.comm === state.comm));
      };
      commBar.appendChild(b);
    };
    mk('All (' + nodes.length + ')', null, null);
    for (const c of comms) mk(c.name + ' (' + c.shown + ')', PALETTE[c.id % PALETTE.length], c.id);
    const isoCnt = nodes.filter(n => n.comm === null).length;
    if (isoCnt) mk('Isolated (' + isoCnt + ')', getCss('--iso'), -1);
  }

  function copyText(text, btn) {
    const done = () => { const prev = btn.textContent; btn.textContent = 'Copied'; setTimeout(() => { btn.textContent = prev; }, 1100); };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done).catch(() => fallbackCopy(text, done));
    } else fallbackCopy(text, done);
  }
  function fallbackCopy(text, done) {
    const ta = document.createElement('textarea'); ta.value = text; ta.style.position = 'fixed'; ta.style.opacity = '0';
    document.body.appendChild(ta); ta.select(); try { document.execCommand('copy'); } catch (_) {} document.body.removeChild(ta); done();
  }

  function renderSidebar() {
    sidebar.replaceChildren();
    if (state.selected !== null && nodes[state.selected]) {
      const n = nodes[state.selected];
      const back = el('button', 'btn', '← Overview'); back.type = 'button';
      back.onclick = () => { state.selected = null; renderSidebar(); draw(); };
      const head = el('div', 'sym-head');
      head.append(el('div', 'sym-name', n.name), back);
      sidebar.appendChild(head);
      sidebar.appendChild(el('div', 'mono', n.qualified));
      sidebar.appendChild(el('div', 'mono', n.file + ':' + n.start + (n.end > n.start ? '-' + n.end : '')));

      const badges = el('div', 'badges');
      badges.append(el('span', 'badge', n.kind), el('span', 'badge', n.lang));
      if (n.comm !== null && comms[n.comm]) {
        const cb = el('button', 'pill', ''); cb.type = 'button';
        const d = el('span', 'dot'); d.style.background = PALETTE[n.comm % PALETTE.length];
        cb.append(d, document.createTextNode(comms[n.comm].name));
        cb.onclick = () => { state.comm = n.comm; renderCommBar(); fitView(nodes.filter(x => x.comm === n.comm)); };
        badges.appendChild(cb);
      }
      sidebar.appendChild(badges);

      const grid = el('div', 'kv-grid');
      const card = (k, v) => { const c = el('div', 'card'); c.append(el('div', 'label', k), el('div', 'val', String(v))); return c; };
      grid.append(card('PageRank', n.pr.toFixed(3)), card('Degree (in / out)', n.in_deg + ' / ' + n.out_deg), card('Caller files', n.caller_files), card('Caller areas', n.caller_areas));
      sidebar.appendChild(grid);

      const cmdSec = el('div');
      cmdSec.appendChild(el('div', 'section-title', 'Justless commands'));
      const inv = sum.invocation || 'justless';
      for (const cmd of [
        inv + ' query -- ' + shQuote(n.name),
        inv + ' map --explain=' + shQuote(n.name),
        inv + ' expand -- ' + shQuote(n.file) + ' L' + n.start + '-' + n.end,
      ]) {
        const row = el('div', 'cmd-row');
        const code = el('code', '', cmd);
        const btn = el('button', 'btn', 'Copy'); btn.type = 'button'; btn.onclick = () => copyText(cmd, btn);
        row.append(code, btn); cmdSec.appendChild(row);
      }
      sidebar.appendChild(cmdSec);

      const addRelList = (title, list, isIn, total) => {
        const sec = el('div');
        const extra = total > list.length ? ' (+' + (total - list.length) + ' outside view)' : '';
        sec.appendChild(el('div', 'section-title', title + ' (' + list.length + ')' + extra));
        if (!list.length) { sec.appendChild(el('div', 'stats', 'None in view')); }
        else {
          const box = el('div', 'list');
          const sorted = [...list].sort((a, b) => (b.bridge - a.bridge) || (nodes[isIn ? b.from : b.to].pr - nodes[isIn ? a.from : a.to].pr));
          for (const e of sorted) {
            const other = nodes[isIn ? e.from : e.to];
            const b = el('button', 'item'); b.type = 'button';
            b.append(el('span', 'title', other.name), el('span', 'sub' + (e.bridge ? ' tag-b' : ''), e.kind + (e.bridge ? ' · bridge' : '')));
            b.onclick = () => { state.selected = other.id; renderSidebar(); centerOn(other.id); };
            box.appendChild(b);
          }
          sec.appendChild(box);
        }
        sidebar.appendChild(sec);
      };
      addRelList('Incoming callers', inEdges[n.id], true, n.in_deg);
      addRelList('Outgoing callees', outEdges[n.id], false, n.out_deg);
      return;
    }

    const grid = el('div', 'kv-grid');
    const card = (k, v) => { const c = el('div', 'card'); c.append(el('div', 'label', k), el('div', 'val', String(v))); return c; };
    grid.append(card('Files', sum.files), card('Symbols', sum.symbols), card('Links', sum.links), card('Bridges', sum.bridges));
    sidebar.appendChild(grid);

    if (sum.languages && sum.languages.length) {
      const sec = el('div'); sec.appendChild(el('div', 'section-title', 'Languages'));
      const row = el('div', 'badges');
      for (const [lang, cnt] of sum.languages.slice(0, 10)) row.appendChild(el('span', 'badge', lang + ' · ' + cnt));
      sec.appendChild(row); sidebar.appendChild(sec);
    }

    if (nodes.length) {
      const sec = el('div'); sec.appendChild(el('div', 'section-title', 'Top Hubs'));
      const box = el('div', 'list');
      for (const n of nodes.slice(0, 12)) {
        const b = el('button', 'item'); b.type = 'button';
        b.append(el('span', 'title', n.name), el('span', 'sub', 'in ' + n.in_deg + ' · out ' + n.out_deg));
        b.onclick = () => { state.selected = n.id; renderSidebar(); centerOn(n.id); };
        box.appendChild(b);
      }
      sec.appendChild(box); sidebar.appendChild(sec);
    }

    if (bridges.length) {
      const sec = el('div'); sec.appendChild(el('div', 'section-title', 'Cross-community Bridges'));
      const box = el('div', 'list');
      for (const br of bridges.slice(0, 12)) {
        const a = nodes[br.from], bNode = nodes[br.to];
        if (!a || !bNode) continue;
        const btn = el('button', 'item'); btn.type = 'button';
        btn.append(el('span', 'title', a.name + ' → ' + bNode.name), el('span', 'sub tag-b', br.kind));
        btn.onclick = () => { state.selected = a.id; renderSidebar(); centerOn(a.id); };
        box.appendChild(btn);
      }
      sec.appendChild(box); sidebar.appendChild(sec);
    }
  }

  function screenToWorld(sx, sy) { return { x: (sx - cam.x) / cam.k, y: (sy - cam.y) / cam.k }; }
  function hitTest(sx, sy) {
    const p = screenToWorld(sx, sy);
    for (let i = nodes.length - 1; i >= 0; i--) {
      const n = nodes[i], hit = Math.max(n.r + 4, 10 / cam.k);
      if (Math.hypot(n.x - p.x, n.y - p.y) <= hit) return n;
    }
    return null;
  }

  let dragMode = null, dragNode = null, dragStart = null, moved = false;
  cv.addEventListener('pointerdown', e => {
    const r = cv.getBoundingClientRect(), sx = e.clientX - r.left, sy = e.clientY - r.top;
    const hit = hitTest(sx, sy);
    dragStart = { x: e.clientX, y: e.clientY, cx: cam.x, cy: cam.y }; moved = false;
    if (hit) { dragMode = 'node'; dragNode = hit; pinned = hit.id; }
    else { dragMode = 'pan'; cv.classList.add('dragging'); }
    cv.setPointerCapture(e.pointerId);
  });
  cv.addEventListener('pointermove', e => {
    const r = cv.getBoundingClientRect(), sx = e.clientX - r.left, sy = e.clientY - r.top;
    if (dragMode) {
      if (Math.hypot(e.clientX - dragStart.x, e.clientY - dragStart.y) > 4) moved = true;
      if (dragMode === 'pan') { cam.x = dragStart.cx + (e.clientX - dragStart.x); cam.y = dragStart.cy + (e.clientY - dragStart.y); draw(); }
      else if (dragMode === 'node' && dragNode) { const p = screenToWorld(sx, sy); dragNode.x = p.x; dragNode.y = p.y; draw(); }
      return;
    }
    const hit = hitTest(sx, sy);
    const next = hit ? hit.id : null;
    cv.classList.toggle('hover', !!hit);
    if (hit) {
      tip.style.display = 'block';
      tip.style.left = Math.min(W - 220, sx + 14) + 'px';
      tip.style.top = Math.max(8, sy - 28) + 'px';
      tip.textContent = hit.name + ' [' + hit.kind + '] · ' + hit.file + ':' + hit.start;
    } else tip.style.display = 'none';
    if (next !== state.hovered) { state.hovered = next; draw(); }
  });
  cv.addEventListener('pointerup', e => {
    if (dragMode && !moved) {
      state.selected = dragMode === 'node' && dragNode ? dragNode.id : null;
      renderSidebar(); draw();
    }
    dragMode = null; dragNode = null; pinned = null; cv.classList.remove('dragging');
    try { cv.releasePointerCapture(e.pointerId); } catch (_) {}
  });
  cv.addEventListener('dblclick', e => {
    const r = cv.getBoundingClientRect(), hit = hitTest(e.clientX - r.left, e.clientY - r.top);
    if (hit) { state.selected = hit.id; renderSidebar(); centerOn(hit.id); }
  });
  cv.addEventListener('wheel', e => {
    e.preventDefault();
    const r = cv.getBoundingClientRect(), sx = e.clientX - r.left, sy = e.clientY - r.top;
    const factor = e.deltaY < 0 ? 1.14 : 1 / 1.14;
    const nk = Math.max(0.15, Math.min(6, cam.k * factor));
    cam.x = sx - ((sx - cam.x) / cam.k) * nk;
    cam.y = sy - ((sy - cam.y) / cam.k) * nk;
    cam.k = nk; draw();
  }, { passive: false });

  searchEl.addEventListener('input', () => {
    const q = searchEl.value.trim().toLowerCase();
    state.query = q; state.matches.clear();
    if (q) {
      for (const n of nodes) {
        if (n.name.toLowerCase().includes(q) || n.qualified.toLowerCase().includes(q) || n.file.toLowerCase().includes(q) || n.kind.toLowerCase().includes(q) || n.lang.toLowerCase().includes(q)) {
          state.matches.add(n.id);
        }
      }
      matchCount.textContent = state.matches.size + ' match' + (state.matches.size === 1 ? '' : 'es');
    } else matchCount.textContent = '';
    draw();
  });
  searchEl.addEventListener('keydown', e => {
    if (e.key === 'Enter' && state.matches.size) {
      const first = [...state.matches][0];
      state.selected = first; renderSidebar(); centerOn(first);
    }
  });
  bridgeBtn.onclick = () => { state.bridgesOnly = !state.bridgesOnly; bridgeBtn.classList.toggle('active', state.bridgesOnly); draw(); };
  resetBtn.onclick = () => { state.comm = null; state.bridgesOnly = false; state.query = ''; state.matches.clear(); searchEl.value = ''; matchCount.textContent = ''; bridgeBtn.classList.remove('active'); renderCommBar(); fitView(nodes); };
  const setTheme = t => { state.theme = t; document.documentElement.dataset.theme = t; themeBtn.textContent = t === 'dark' ? 'Light' : 'Dark'; renderCommBar(); draw(); };
  themeBtn.onclick = () => setTheme(state.theme === 'dark' ? 'light' : 'dark');

  window.addEventListener('keydown', e => {
    if (e.key === '/' && document.activeElement !== searchEl) { e.preventDefault(); searchEl.focus(); searchEl.select(); }
    else if (e.key === 'Escape') {
      if (searchEl.value) { searchEl.value = ''; searchEl.dispatchEvent(new Event('input')); }
      else if (state.selected !== null) { state.selected = null; renderSidebar(); draw(); }
      searchEl.blur();
    } else if ((e.key === 'r' || e.key === 'R') && document.activeElement !== searchEl) resetBtn.click();
  });
  window.addEventListener('resize', resize);

  const initTheme = window.matchMedia && window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
  setTheme(initTheme);
  resize();
  fitView(nodes);
  renderSidebar();
  if (state.selected !== null && nodes[state.selected]) centerOn(state.selected);
  (function settle() { if (simTick()) { draw(); raf = requestAnimationFrame(settle); } else { draw(); cancelAnimationFrame(raf); } })();
})();
</script>
</body>
</html>
"##;

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

    fn extract_embedded_json(html: &str) -> Value {
        let marker = r#"<script id="justless-graph" type="application/json">"#;
        let start = html.find(marker).expect("graph script tag") + marker.len();
        let end = html[start..].find("</script>").expect("closing script tag") + start;
        serde_json::from_str(&html[start..end]).expect("valid embedded JSON")
    }

    #[test]
    fn map_html_is_offline_self_contained_and_contains_nodes_communities_and_bridges() {
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
                "CREATE TABLE purchase_orders (id INTEGER PRIMARY KEY);\n",
            ),
        ]);

        let html = render_html(&index, "mixed-demo", "justless", None);
        assert!(html.starts_with("<!DOCTYPE html>"), "missing doctype");
        assert!(html.contains("<canvas id=\"cv\"></canvas>"));
        assert!(!html.contains("http://"), "must not contain http:// URLs");
        assert!(!html.contains("https://"), "must not contain https:// URLs");
        assert!(
            !html.contains("<script src"),
            "must not load external scripts"
        );
        assert!(
            !html.contains("<link "),
            "must not load external stylesheets"
        );
        assert!(!html.contains("innerHTML"), "must not use innerHTML");

        let payload = extract_embedded_json(&html);
        let node_names: Vec<&str> = payload["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap())
            .collect();
        assert!(node_names.contains(&"fetchOrders"), "{node_names:?}");
        assert!(node_names.contains(&"list_orders"), "{node_names:?}");
        assert!(node_names.contains(&"query_orders"), "{node_names:?}");
        assert!(
            !payload["communities"].as_array().unwrap().is_empty(),
            "expected communities: {payload}"
        );
        assert!(
            !payload["bridges"].as_array().unwrap().is_empty(),
            "expected cross-community bridges: {payload}"
        );
        assert!(
            !payload["areas"].as_array().unwrap().is_empty(),
            "expected areas: {payload}"
        );
        assert_eq!(payload["summary"]["title"], "mixed-demo");
    }

    #[test]
    fn map_html_escapes_xss_payloads_in_symbol_and_file_names() {
        let (_dir, mut index) = write_repo(&[(
            "src/app.py",
            "def safe_helper():\n    return 1\n\ndef entry():\n    return safe_helper()\n",
        )]);
        let xss = "</script><img src=x onerror=alert(1)>";
        let weird = format!("{xss}<!-- & \u{2028}\u{2029} -->");
        let old_file = index.definitions[0].location.file.clone();
        if let Some(content) = index.files.remove(&old_file) {
            index.files.insert(format!("evil/{xss}.py"), content);
        }
        for def in &mut index.definitions {
            def.location.file = format!("evil/{xss}.py");
            if def.kind != "file" {
                def.qualified = format!("evil/{xss}.py::{weird}");
            } else {
                def.qualified = format!("evil/{xss}.py");
            }
        }
        for edge in &mut index.edges {
            edge.location.file = format!("evil/{xss}.py");
        }

        let html = render_html(&index, &weird, "justless", None);
        assert!(
            !html.contains(xss),
            "unescaped XSS payload appeared in HTML"
        );
        assert!(
            !html.contains("<img src=x"),
            "unescaped <img> tag appeared in HTML"
        );
        assert!(
            !html.contains('\u{2028}') && !html.contains('\u{2029}'),
            "raw line/paragraph separators must be escaped"
        );
        assert_eq!(
            html.matches("</script>").count(),
            2,
            "must have exactly 2 closing script tags (data + app)"
        );

        let payload = extract_embedded_json(&html);
        assert_eq!(payload["summary"]["title"], weird);
        assert!(
            payload["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["name"].as_str().unwrap() == weird),
            "escaped JSON must round-trip original symbol string losslessly"
        );

        // P1-1: literal `__JUSTLESS_JSON__` in title must never expand into JSON.
        let placeholder_html = render_html(&index, "__JUSTLESS_JSON__", "justless", None);
        assert!(
            placeholder_html.contains("<title>Justless Map · __JUSTLESS_JSON__</title>"),
            "title placeholder must stay literal and never interpolate JSON"
        );
        // P1-4 & P2-1: option-terminating commands and code-point slicing.
        assert!(placeholder_html.contains("inv + ' query -- ' + shQuote(n.name)"));
        assert!(placeholder_html.contains("inv + ' map --explain=' + shQuote(n.name)"));
        assert!(
            placeholder_html
                .contains("inv + ' expand -- ' + shQuote(n.file) + ' L' + n.start + '-' + n.end")
        );
        assert!(placeholder_html.contains("const chars = [...n.name];"));
    }

    #[test]
    fn map_html_cli_supports_explain_and_out_and_rejects_json_conflict() {
        // `--html` conflicts with `--json`.
        let conflict = command().try_get_matches_from(["map", "--html", "--json"]);
        assert!(conflict.is_err(), "--html and --json must conflict");

        // `--out` requires `--html`.
        let orphan_out = command().try_get_matches_from(["map", "--out", "graph.html"]);
        assert!(orphan_out.is_err(), "--out without --html must fail");

        // `--html --explain SYMBOL --out PATH` parses and executes end-to-end.
        let (dir, _index) = write_repo(&[
            ("src/core.py", "def compute_total(x):\n    return x + 1\n"),
            (
                "src/api.py",
                "from src.core import compute_total\ndef handle_request():\n    return compute_total(41)\n",
            ),
        ]);
        let out_path = dir.path().join("explorer.html");
        let matches = command()
            .try_get_matches_from([
                "map",
                "--html",
                "--explain",
                "compute_total",
                "--out",
                out_path.to_str().unwrap(),
            ])
            .expect("--html --explain --out must parse cleanly");
        let ws = Workspace::discover(Some(dir.path())).unwrap();
        run(&ws, &matches).expect("map --html --explain --out must succeed");

        let written = fs::read_to_string(&out_path).expect("HTML file written");
        assert!(written.starts_with("<!DOCTYPE html>"));
        let payload = extract_embedded_json(&written);
        let focus = payload["focus"].as_u64().expect("focus node index set") as usize;
        assert_eq!(payload["nodes"][focus]["name"], "compute_total");
        assert_eq!(payload["summary"]["focus_symbol"], "compute_total");
    }

    #[test]
    fn map_and_review_record_graph_telemetry_only_with_state() {
        let core_src = concat!(
            "def compute_total(x):\n",
            "    \"\"\"Compute a padded total for billing.\"\"\"\n",
            "    return x + 1\n",
        );
        let api_src = concat!(
            "from src.core import compute_total\n\n",
            "def handle_request(v):\n",
            "    return compute_total(v)\n",
        );
        let test_src = concat!(
            "from src.api import handle_request\n\n",
            "def test_handle_request():\n",
            "    assert handle_request(41) == 42\n",
        );
        let (dir, _index) = write_repo(&[
            ("src/core.py", core_src),
            ("src/api.py", api_src),
            ("tests/test_api.py", test_src),
        ]);
        let ws = Workspace::discover(Some(dir.path())).unwrap();
        for args in [
            &["init", "-q"][..],
            &["config", "user.name", "Test"][..],
            &["config", "user.email", "test@example.com"][..],
            &["add", "."][..],
            &["commit", "-qm", "init"][..],
        ] {
            let out = ws.git(args).unwrap();
            assert!(out.status.success(), "{args:?}: {:?}", out.stderr);
        }
        // Mutate `src/core.py` so `review` has real diff + blast radius + reachable test.
        let modified_core = core_src.replace("return x + 1", "return x + 2");
        fs::write(dir.path().join("src/core.py"), &modified_core).unwrap();
        let index = Index::open(&ws).unwrap();

        let map_plain = command().try_get_matches_from(["map"]).unwrap();
        let map_explain = command()
            .try_get_matches_from(["map", "--explain", "compute_total"])
            .unwrap();
        let out_html = dir.path().join("graph.html");
        let map_html = command()
            .try_get_matches_from(["map", "--html", "--out", out_html.to_str().unwrap()])
            .unwrap();
        run(&ws, &map_plain).unwrap();
        run(&ws, &map_explain).unwrap();
        run(&ws, &map_html).unwrap();

        let review_plain = crate::review::command()
            .try_get_matches_from(["review"])
            .unwrap();
        let review_json = crate::review::command()
            .try_get_matches_from(["review", "--json"])
            .unwrap();
        crate::review::run(&ws, &review_plain).unwrap();
        crate::review::run(&ws, &review_json).unwrap();

        let usage = crate::history::graph_usage(&ws);
        assert_eq!(usage.calls, 5, "{usage:?}");
        assert_eq!(usage.by_command.get("map"), Some(&3), "{usage:?}");
        assert_eq!(usage.by_command.get("review"), Some(&2), "{usage:?}");
        assert!(usage.shown_bytes > 0, "{usage:?}");
        let min_expected_ref = (modified_core.len() + api_src.len()) as u64;
        assert!(
            usage.referenced_file_bytes >= min_expected_ref,
            "expected referenced_file_bytes >= {min_expected_ref}, got {usage:?}"
        );

        // Removing `.justless/` must keep both `map` and `review` silent with zero state created.
        fs::remove_dir_all(&ws.state).unwrap();
        assert!(!ws.state.exists());
        run_with_index(&ws, &index, &map_plain, 4096).unwrap();
        explain(&ws, &index, "compute_total", 4096, false).unwrap();
        let since = crate::context::Since::resolve(&ws, false, None).unwrap();
        crate::review::run_with_index(&ws, &index, &since, 2, 4096, false).unwrap();
        assert!(
            !ws.state.exists(),
            "map and review must stay silent and not create .justless/ when absent"
        );
    }
}
