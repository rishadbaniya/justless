//! `justless review`: a budget-bounded blast-radius PR / branch review pack.
//!
//! Combines Git diff analysis (folding lockfiles, generated artifacts, pure
//! renames and whitespace-only hunks), the incremental symbol & cross-language
//! graph, lightweight centrality/community bridge detection, reverse blast-radius
//! walking and per-symbol test reachability into one deterministic, strictly
//! budgeted review dossier.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Result, ensure};
use clap::{Arg, ArgMatches, Command};
use serde_json::{Map, Value, json};

use crate::context::{Change, Deletion, Report, Since, changes, relationship};
use crate::impact::{
    Dependent, JsGraph, RubyGraph, Walk, changed_in, config_reference,
    is_shared_js_config_or_setup, is_shared_ruby_helper, rspec_subject_tests, walk,
};
use crate::index::{Definition, Edge, Index, nul_paths};
use crate::lockdiff::{SVG_FOLD_CHANGED_LINES, is_generated_path, is_lockfile_path, is_svg_path};
use crate::map::{Section, is_support_file, is_test_file, label, language, name, node, plural};
use crate::rust_syntax::Kind;
use crate::summary::short;
use crate::workspace::{Location, Settings, Workspace, display_path};

/// Default blast-radius hop limit.
const DEFAULT_DEPTH: &str = "2";
/// Maximum allowed `--depth`.
const MAX_DEPTH: usize = 5;
/// Exact byte length of `Report::CLIPPED` (`"[context budget reached]\n"`).
const RESERVE: usize = 25;
/// Maximum test filters per suggested runner command.
const FILTERS: usize = 8;
/// Maximum symbol name bytes in compact lines.
const NAME_BYTES: usize = 80;
/// Minimum symbol count for a single-file cluster to count as a community.
const MIN_SINGLE_FILE_COMMUNITY: usize = 5;

pub fn command() -> Command<'static> {
    Command::new("review")
        .about("Build a budget-bounded blast-radius review pack for Git changes or a branch")
        .arg(crate::base_arg())
        .arg(
            Arg::new("staged")
                .long("staged")
                .conflicts_with("base")
                .help("Review staged changes against HEAD"),
        )
        .arg(crate::number(
            "depth",
            DEFAULT_DEPTH,
            "Blast-radius dependency hops to follow (1..=5)",
        ))
        .arg(
            crate::budget()
                .alias("max-bytes")
                .help("Maximum review pack UTF-8 bytes (default 4096)"),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .help("Print the structured review dossier as JSON"),
        )
}

pub fn run(workspace: &Workspace, matches: &ArgMatches) -> Result<()> {
    let budget = review_budget(workspace, matches)?;
    let depth = crate::read_num(matches, "depth", 1)?;
    ensure!(
        depth <= MAX_DEPTH,
        "--depth must be between 1 and {MAX_DEPTH}"
    );
    let since = Since::resolve(
        workspace,
        matches.is_present("staged"),
        matches.value_of("base"),
    )?;
    let index = Index::open(workspace)?;
    let output = render_review(
        workspace,
        &index,
        &since,
        depth,
        budget,
        matches.is_present("json"),
    )?;
    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

fn review_budget(workspace: &Workspace, matches: &ArgMatches) -> Result<usize> {
    if matches.is_present("budget") {
        crate::read_num(matches, "budget", 128)
    } else {
        let settings = workspace.settings()?;
        let default = Settings::default();
        if settings.context_budget != default.context_budget {
            Ok(settings.context_budget)
        } else {
            Ok(settings.max_output_bytes)
        }
    }
}

// ---------------------------------------------------------------------------
// File categorization & Git diff parsing

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FileCategory {
    Source,
    Test,
    Generated,
    Other,
}

impl FileCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Test => "test",
            Self::Generated => "generated",
            Self::Other => "other",
        }
    }
}

fn classify_file(index: &Index, path: &str, churn: usize) -> FileCategory {
    if is_lockfile_path(path)
        || is_generated_path(path)
        || (is_svg_path(path) && churn > SVG_FOLD_CHANGED_LINES)
    {
        FileCategory::Generated
    } else if is_test_file(path) {
        FileCategory::Test
    } else if language(path).is_some() || index.grammar(path).is_some() {
        FileCategory::Source
    } else {
        FileCategory::Other
    }
}

#[derive(Clone, Debug)]
struct ParsedHunk {
    new_start: usize,
    new_count: usize,
    deleted: usize,
    added_lines: Vec<usize>,
    deleted_after: usize,
    whitespace_only: bool,
    context_hint: Option<String>,
}

impl ParsedHunk {
    fn new_range(&self) -> Option<(usize, usize)> {
        if self.new_count == 0 {
            None
        } else {
            let start = self.new_start.max(1);
            Some((start, start + self.new_count - 1))
        }
    }
}

#[derive(Clone, Debug)]
struct FileDiff {
    path: String,
    old_path: Option<String>,
    similarity: Option<usize>,
    new_file: bool,
    deleted_file: bool,
    untracked: bool,
    binary: bool,
    mode_change: bool,
    added: usize,
    deleted: usize,
    semantic_added: usize,
    semantic_deleted: usize,
    whitespace_hunks: usize,
    hunks: Vec<ParsedHunk>,
}

impl FileDiff {
    fn is_pure_rename(&self) -> bool {
        self.old_path.is_some()
            && self.similarity == Some(100)
            && self.hunks.is_empty()
            && self.added == 0
            && self.deleted == 0
            && !self.mode_change
    }

    fn is_whitespace_only(&self) -> bool {
        !self.hunks.is_empty()
            && self.whitespace_hunks == self.hunks.len()
            && !self.new_file
            && !self.deleted_file
            && !self.mode_change
            && self.old_path.is_none()
    }

    fn semantic_ranges(&self) -> Vec<(usize, usize)> {
        let mut ranges: Vec<(usize, usize)> = self
            .hunks
            .iter()
            .filter(|h| !h.whitespace_only)
            .filter_map(ParsedHunk::new_range)
            .collect();
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in ranges {
            if let Some(last) = merged.last_mut()
                && start <= last.1 + 1
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        merged
    }
}

fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn is_indent_significant_path(path: &str) -> bool {
    let Some(name) = Path::new(path).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower == "makefile"
        || lower == "gnumakefile"
        || lower.starts_with("makefile.")
        || [
            ".py", ".pyi", ".yml", ".yaml", ".mk", ".haml", ".pug", ".nim", ".hs", ".lhs",
        ]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

fn normalize_ws_outside_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut prev_ws = false;
    for ch in s.chars() {
        if let Some(q) = quote {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' && q != '`' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
            prev_ws = false;
            continue;
        }
        if matches!(ch, '"' | '\'' | '`') {
            quote = Some(ch);
            out.push(ch);
            prev_ws = false;
        } else if ch == ' ' || ch == '\t' {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

fn ws_only_line_eq(r: &str, a: &str) -> bool {
    let rt = r.trim_matches([' ', '\t', '\r']);
    let at = a.trim_matches([' ', '\t', '\r']);
    if rt == at {
        return true;
    }
    normalize_ws_outside_quotes(rt) == normalize_ws_outside_quotes(at)
}

fn is_ws_only_block(rem: &[String], add: &[String], indent_significant: bool) -> bool {
    if rem.is_empty() && add.is_empty() {
        return true;
    }
    if indent_significant {
        return rem.len() == add.len()
            && rem.iter().zip(add.iter()).all(|(r, a)| {
                r.trim_end_matches([' ', '\t', '\r']) == a.trim_end_matches([' ', '\t', '\r'])
            });
    }
    let mut r_iter = rem
        .iter()
        .map(String::as_str)
        .filter(|s| !s.trim_matches([' ', '\t', '\r']).is_empty());
    let mut a_iter = add
        .iter()
        .map(String::as_str)
        .filter(|s| !s.trim_matches([' ', '\t', '\r']).is_empty());
    loop {
        match (r_iter.next(), a_iter.next()) {
            (Some(r), Some(a)) => {
                if !ws_only_line_eq(r, a) {
                    return false;
                }
            }
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn parse_hunk_header(line: &str) -> Option<(usize, usize, usize, usize, Option<String>)> {
    let rest = line.strip_prefix("@@ -")?;
    let (ranges, tail) = rest.split_once(" @@")?;
    let (old_part, new_part) = ranges.split_once(" +")?;
    let parse_span = |s: &str| -> Option<(usize, usize)> {
        if let Some((a, b)) = s.split_once(',') {
            Some((a.parse().ok()?, b.parse().ok()?))
        } else {
            Some((s.parse().ok()?, 1))
        }
    };
    let (old_start, old_count) = parse_span(old_part)?;
    let (new_start, new_count) = parse_span(new_part)?;
    let hint = tail.trim();
    let context_hint = (!hint.is_empty()).then(|| hint.to_owned());
    Some((old_start, old_count, new_start, new_count, context_hint))
}

fn parse_diff_git_paths(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("diff --git ")?;
    let rest = rest.strip_prefix("a/")?;
    let (old, new) = rest.rsplit_once(" b/")?;
    Some((old.to_owned(), new.to_owned()))
}

fn collect_diff(workspace: &Workspace, since: &Since) -> Result<Vec<FileDiff>> {
    let inside = workspace.git(&["rev-parse", "--is-inside-work-tree"])?;
    ensure!(
        inside.status.success() && inside.stdout.starts_with(b"true"),
        "review needs a Git repository: {}",
        first_line(&inside.stderr)
    );
    let head_exists = workspace
        .git(&["rev-parse", "--verify", "--quiet", "HEAD"])?
        .status
        .success();

    let mut args = vec![
        "-c",
        "core.quotepath=false",
        "diff",
        "--relative",
        "--no-ext-diff",
        "--no-textconv",
        "--ignore-submodules=none",
        "-M",
        "--unified=0",
    ];
    match since {
        Since::Staged => args.push("--cached"),
        Since::Base { commit, .. } => args.push(commit.as_str()),
        Since::Head if head_exists => args.push("HEAD"),
        Since::Head => args.push("--cached"),
    }
    args.extend(["--", "."]);

    let output = workspace.git(&args)?;
    ensure!(
        output.status.success(),
        "git diff failed: {}",
        first_line(&output.stderr)
    );
    let patch = String::from_utf8_lossy(&output.stdout);
    let mut files = parse_unified_diff(&patch);

    // On unborn HEAD with Since::Head, also pick up unstaged edits over staged files
    // and untracked files. For normal Head / Base, pick up untracked files.
    if !matches!(since, Since::Staged) {
        let mut seen: BTreeSet<String> = files.iter().map(|f| f.path.clone()).collect();
        let ls = workspace.git(&[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ])?;
        ensure!(
            ls.status.success(),
            "git ls-files failed: {}",
            first_line(&ls.stderr)
        );
        for rel in nul_paths(&ls.stdout) {
            if !seen.insert(rel.to_owned()) {
                continue;
            }
            let full = workspace.root.join(rel);
            let Ok(bytes) = fs::read(&full) else {
                continue;
            };
            let binary = bytes.iter().take(8192).any(|&b| b == 0);
            let line_count = if binary || bytes.is_empty() {
                0
            } else {
                let text = String::from_utf8_lossy(&bytes);
                text.lines().count()
            };
            let hunks = if !binary && line_count > 0 {
                vec![ParsedHunk {
                    new_start: 1,
                    new_count: line_count,
                    deleted: 0,
                    added_lines: (1..=line_count).collect(),
                    deleted_after: 0,
                    whitespace_only: false,
                    context_hint: None,
                }]
            } else {
                Vec::new()
            };
            files.push(FileDiff {
                path: rel.to_owned(),
                old_path: None,
                similarity: None,
                new_file: true,
                deleted_file: false,
                untracked: true,
                binary,
                mode_change: false,
                added: line_count,
                deleted: 0,
                semantic_added: line_count,
                semantic_deleted: 0,
                whitespace_hunks: 0,
                hunks,
            });
        }
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn parse_unified_diff(patch: &str) -> Vec<FileDiff> {
    struct ActiveHunk {
        new_start: usize,
        new_count: usize,
        added: usize,
        deleted: usize,
        next_new_line: usize,
        added_lines: Vec<usize>,
        rem_lines: Vec<String>,
        add_lines: Vec<String>,
        saw_no_newline: bool,
        context_hint: Option<String>,
    }

    let mut files = Vec::<FileDiff>::new();
    let mut current: Option<FileDiff> = None;
    let mut hunk: Option<ActiveHunk> = None;

    let flush_hunk = |file: &mut FileDiff, active: &mut Option<ActiveHunk>| {
        let Some(h) = active.take() else {
            return;
        };
        let indent_sig = is_indent_significant_path(&file.path);
        let whitespace_only = !h.saw_no_newline
            && (h.added > 0 || h.deleted > 0)
            && is_ws_only_block(&h.rem_lines, &h.add_lines, indent_sig);
        file.added += h.added;
        file.deleted += h.deleted;
        if whitespace_only {
            file.whitespace_hunks += 1;
        } else {
            file.semantic_added += h.added;
            file.semantic_deleted += h.deleted;
        }
        file.hunks.push(ParsedHunk {
            new_start: h.new_start,
            new_count: h.new_count,
            deleted: h.deleted,
            added_lines: h.added_lines,
            deleted_after: h.new_start,
            whitespace_only,
            context_hint: h.context_hint,
        });
    };

    for raw_line in patch.lines() {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if let Some((old, new)) = parse_diff_git_paths(line) {
            if let Some(mut prev) = current.take() {
                flush_hunk(&mut prev, &mut hunk);
                files.push(prev);
            }
            let _ = old;
            current = Some(FileDiff {
                path: new,
                old_path: None,
                similarity: None,
                new_file: false,
                deleted_file: false,
                untracked: false,
                binary: false,
                mode_change: false,
                added: 0,
                deleted: 0,
                semantic_added: 0,
                semantic_deleted: 0,
                whitespace_hunks: 0,
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(file) = current.as_mut() else {
            continue;
        };
        if let Some((_, _, new_start, new_count, hint)) = parse_hunk_header(line) {
            flush_hunk(file, &mut hunk);
            hunk = Some(ActiveHunk {
                new_start,
                new_count,
                added: 0,
                deleted: 0,
                next_new_line: new_start.max(1),
                added_lines: Vec::new(),
                rem_lines: Vec::new(),
                add_lines: Vec::new(),
                saw_no_newline: false,
                context_hint: hint,
            });
            continue;
        }
        if let Some(active) = hunk.as_mut() {
            if let Some(added) = line.strip_prefix('+') {
                active.added += 1;
                active.added_lines.push(active.next_new_line);
                active.next_new_line += 1;
                active.add_lines.push(added.to_owned());
            } else if let Some(removed) = line.strip_prefix('-') {
                active.deleted += 1;
                active.rem_lines.push(removed.to_owned());
            } else if line.starts_with("\\ ") {
                active.saw_no_newline = true;
            } else if line.starts_with(' ') {
                active.next_new_line += 1;
            }
            continue;
        }
        // File header metadata before first hunk.
        if line.starts_with("new file mode ") {
            file.new_file = true;
        } else if line.starts_with("deleted file mode ") {
            file.deleted_file = true;
        } else if line.starts_with("old mode ") || line.starts_with("new mode ") {
            file.mode_change = true;
        } else if let Some(rest) = line.strip_prefix("similarity index ") {
            if let Some(pct) = rest.strip_suffix('%') {
                file.similarity = pct.parse().ok();
            }
        } else if let Some(from) = line.strip_prefix("rename from ") {
            file.old_path = Some(from.to_owned());
        } else if let Some(to) = line.strip_prefix("rename to ") {
            file.path = to.to_owned();
        } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
            file.binary = true;
        }
    }
    if let Some(mut prev) = current.take() {
        flush_hunk(&mut prev, &mut hunk);
        files.push(prev);
    }
    files
}

// ---------------------------------------------------------------------------
// Lightweight centrality, communities & cross-community bridges

fn file_area(file: &str) -> String {
    let parts: Vec<&str> = file.split('/').collect();
    match parts.as_slice() {
        [_] => "(root)".to_owned(),
        [top, _] => format!("{top}/"),
        [top, second, ..] => format!("{top}/{second}/"),
        [] => "(root)".to_owned(),
    }
}

fn indexed_language(index: &Index, file: &str) -> &'static str {
    if Path::new(file).extension().is_some_and(|ext| ext == "rs") {
        return "Rust";
    }
    index
        .grammar(file)
        .map(|g| g.name())
        .or_else(|| language(file))
        .unwrap_or("text")
}

fn central(def: &Definition) -> bool {
    !def.test && !matches!(def.kind.as_str(), "file" | "module")
}

struct ReviewGraph {
    in_deg: Vec<usize>,
    caller_files: Vec<usize>,
    caller_areas: Vec<usize>,
    is_hub: Vec<bool>,
    community_of: Vec<Option<usize>>,
    community_count: usize,
}

impl ReviewGraph {
    fn build(index: &Index) -> Self {
        let n = index.definitions.len();
        let mut in_deg = vec![0usize; n];
        let mut caller_files = vec![0usize; n];
        let mut caller_areas = vec![0usize; n];

        let mut incoming_pairs = BTreeSet::<(usize, Option<usize>, &str)>::new();
        let mut file_sets = vec![BTreeSet::<&str>::new(); n];
        let mut area_sets = vec![BTreeSet::<String>::new(); n];

        for edge in &index.edges {
            if edge.from == Some(edge.to) {
                continue;
            }
            let target_file = index.definitions[edge.to].location.file.as_str();
            let scope = if edge.from.is_none() {
                if edge.location.file == target_file {
                    continue;
                }
                edge.location.file.as_str()
            } else {
                ""
            };
            if incoming_pairs.insert((edge.to, edge.from, scope)) {
                in_deg[edge.to] += 1;
            }
            let caller_file = match edge.from {
                Some(from) => index.definitions[from].location.file.as_str(),
                None => edge.location.file.as_str(),
            };
            let caller_is_test = edge.from.is_some_and(|from| index.definitions[from].test)
                || is_test_file(caller_file);
            if caller_file != target_file && !caller_is_test {
                file_sets[edge.to].insert(caller_file);
                area_sets[edge.to].insert(file_area(caller_file));
            }
        }

        for id in 0..n {
            caller_files[id] = file_sets[id].len();
            caller_areas[id] = area_sets[id].len();
        }

        // Rank non-test production symbols by weighted hub score to detect top hubs.
        let mut prod_scores: Vec<(usize, usize, usize, usize)> = (0..n)
            .filter(|&id| {
                central(&index.definitions[id])
                    && !is_support_file(&index.definitions[id].location.file)
            })
            .map(|id| (caller_areas[id], caller_files[id], in_deg[id], id))
            .collect();
        prod_scores.sort_by_key(|&(areas, files, deg, id)| {
            (Reverse(areas), Reverse(files), Reverse(deg), id)
        });
        let top_cutoff = (prod_scores.len() / 10).max(3);
        let top_ids: BTreeSet<usize> = prod_scores
            .iter()
            .take(top_cutoff)
            .filter(|&&(areas, files, deg, _)| deg >= 3 && (files >= 2 || areas >= 2))
            .map(|&(_, _, _, id)| id)
            .collect();

        let mut is_hub = vec![false; n];
        for id in 0..n {
            let def = &index.definitions[id];
            if !central(def) || is_support_file(&def.location.file) {
                continue;
            }
            is_hub[id] = caller_areas[id] >= 2
                || (caller_files[id] >= 3 && in_deg[id] >= 4)
                || top_ids.contains(&id);
        }

        let (community_of, community_count) = detect_communities(index);

        Self {
            in_deg,
            caller_files,
            caller_areas,
            is_hub,
            community_of,
            community_count,
        }
    }

    /// Returns `(is_bridge, cross_language, description)` for a non-test symbol.
    fn bridge_info(&self, index: &Index, id: usize) -> (bool, bool, Option<String>) {
        let def = &index.definitions[id];
        if !central(def) {
            return (false, false, None);
        }
        let self_file = def.location.file.as_str();
        let self_lang = indexed_language(index, self_file);
        let self_area = file_area(self_file);
        let self_comm = self.community_of[id];

        let mut other_langs = BTreeSet::<&'static str>::new();
        let mut in_comms = BTreeSet::<usize>::new();
        let mut out_comms = BTreeSet::<usize>::new();
        let mut in_areas = BTreeSet::<String>::new();
        let mut out_areas = BTreeSet::<String>::new();

        let mut inspect = |other: usize, incoming: bool| {
            if other == id {
                return;
            }
            let other_def = &index.definitions[other];
            if !central(other_def) {
                return;
            }
            let other_file = other_def.location.file.as_str();
            let other_lang = indexed_language(index, other_file);
            if other_lang != self_lang {
                other_langs.insert(other_lang);
            }
            if let (Some(c1), Some(c2)) = (self_comm, self.community_of[other])
                && c1 != c2
            {
                if incoming {
                    in_comms.insert(c2);
                } else {
                    out_comms.insert(c2);
                }
            }
            let other_area = file_area(other_file);
            if other_area != self_area {
                if incoming {
                    in_areas.insert(other_area);
                } else {
                    out_areas.insert(other_area);
                }
            }
        };

        for edge in index.incoming(id) {
            if let Some(from) = edge.from {
                inspect(from, true);
            }
        }
        for edge in index.outgoing(id) {
            inspect(edge.to, false);
        }

        if !other_langs.is_empty() {
            let langs: Vec<&str> = other_langs.into_iter().collect();
            return (true, true, Some(format!("{self_lang}↔{}", langs.join("/"))));
        }
        let cross_comm = (!in_comms.is_empty() && !out_comms.is_empty()) || in_comms.len() >= 2;
        if cross_comm {
            let all_comms: BTreeSet<usize> = in_comms.union(&out_comms).copied().collect();
            let count = all_comms.len() + 1;
            return (true, false, Some(format!("{count} communities")));
        }
        let cross_area = self.community_count < 2 && !in_areas.is_empty() && !out_areas.is_empty();
        if cross_area {
            let all_areas: BTreeSet<&String> = in_areas.union(&out_areas).collect();
            let count = all_areas.len() + 1;
            return (true, false, Some(format!("{count} areas")));
        }
        (false, false, None)
    }
}

fn detect_communities(index: &Index) -> (Vec<Option<usize>>, usize) {
    let definitions = &index.definitions;
    let mut nodes = definitions.len();
    let mut file_nodes = BTreeMap::<&str, usize>::new();
    let mut by_file = BTreeMap::<&str, Vec<usize>>::new();
    for (id, def) in definitions.iter().enumerate() {
        if def.kind == "file" {
            file_nodes.insert(&def.location.file, id);
        } else {
            by_file.entry(&def.location.file).or_default().push(id);
        }
    }

    let mut links = HashMap::<(usize, usize), u32>::new();
    let mut add_link = |a: usize, b: usize, w: u32| {
        if a != b {
            *links.entry((a.min(b), a.max(b))).or_default() += w;
        }
    };

    for (file, mut ids) in by_file {
        let root = *file_nodes.entry(file).or_insert_with(|| {
            nodes += 1;
            nodes - 1
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

    for edge in &index.edges {
        let from = match edge.from {
            Some(f) => f,
            None => *file_nodes
                .entry(edge.location.file.as_str())
                .or_insert_with(|| {
                    nodes += 1;
                    nodes - 1
                }),
        };
        let weight = match (edge.inferred, edge.kind) {
            (false, _) => 2,
            (true, Kind::Route | Kind::Data) => 1,
            (true, _) => continue,
        };
        add_link(from, edge.to, weight);
    }

    let mut pairs: Vec<_> = links.into_iter().collect();
    pairs.sort_unstable();
    let mut adj = vec![Vec::new(); nodes];
    for ((a, b), w) in pairs {
        adj[a].push((b, w));
        adj[b].push((a, w));
    }

    let mut labels: Vec<usize> = (0..nodes).collect();
    let mut tally = HashMap::<usize, u32>::new();
    for _ in 0..24 {
        let mut moved = false;
        for node in 0..nodes {
            if adj[node].is_empty() {
                continue;
            }
            tally.clear();
            for &(other, w) in &adj[node] {
                *tally.entry(labels[other]).or_default() += w;
            }
            let best = tally.values().copied().max().unwrap_or(0);
            if tally.get(&labels[node]) == Some(&best) {
                continue;
            }
            let choice = tally
                .iter()
                .filter(|&(_, &w)| w == best)
                .map(|(&l, _)| l)
                .min()
                .unwrap_or(labels[node]);
            if choice != labels[node] {
                labels[node] = choice;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }

    let mut groups = BTreeMap::<usize, Vec<usize>>::new();
    for (id, def) in definitions.iter().enumerate() {
        if def.kind == "file" {
            continue;
        }
        groups.entry(labels[id]).or_default().push(id);
    }

    let mut community_of = vec![None; definitions.len()];
    let mut next_comm = 0usize;
    for members in groups.into_values() {
        let distinct_files: BTreeSet<&str> = members
            .iter()
            .map(|&id| definitions[id].location.file.as_str())
            .collect();
        if distinct_files.len() >= 2 || members.len() >= MIN_SINGLE_FILE_COMMUNITY {
            for id in members {
                community_of[id] = Some(next_comm);
            }
            next_comm += 1;
        }
    }
    (community_of, next_comm)
}

// ---------------------------------------------------------------------------
// Per-symbol test reachability & runner command synthesis

fn is_callable(def: &Definition) -> bool {
    matches!(def.kind.as_str(), "function" | "method")
}

fn is_entrypoint_main(def: &Definition) -> bool {
    is_callable(def) && matches!(name(def).rsplit("::").next(), Some("main" | "Main"))
}

fn is_js_file(file: &str) -> bool {
    matches!(
        Path::new(file).extension().and_then(|e| e.to_str()),
        Some("js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts")
    )
}

#[derive(Clone)]
struct TestFileHit {
    hops: usize,
    possible: bool,
    at: Location,
    note: Option<String>,
}

type TestHit = (usize, usize, bool);
type SymbolTests = (Vec<TestHit>, BTreeMap<String, TestFileHit>);

/// Tests reaching one specific symbol `id`: returns `(test_symbol_ids, test_file_hits)`.
fn tests_for_symbol(index: &Index, id: usize, walk_depth: usize) -> SymbolTests {
    let definitions = &index.definitions;
    let mut found = BTreeMap::<usize, (bool, usize)>::new();
    let mut keep = |tid: usize, hops: usize, possible: bool| {
        let entry = found.entry(tid).or_insert((possible, hops));
        *entry = (*entry).min((possible, hops));
    };

    if definitions[id].test {
        keep(id, 0, false);
    }
    for (tid, hops, inferred) in index.related_tests(id, 32) {
        keep(tid, hops, inferred);
    }

    let w = walk(index, &[id], walk_depth);
    let mut file_hits = BTreeMap::<String, TestFileHit>::new();
    for (offset, level) in w.levels.iter().enumerate() {
        let hops = offset + 1;
        for reach in level {
            match reach.dependent {
                Dependent::Symbol(tid) if definitions[tid].test => {
                    keep(tid, hops, reach.possible);
                }
                Dependent::Scope(file) if is_test_file(file) => {
                    file_hits
                        .entry(file.to_owned())
                        .or_insert_with(|| TestFileHit {
                            hops,
                            possible: reach.possible,
                            at: reach.edge.location.clone(),
                            note: None,
                        });
                }
                _ => {}
            }
        }
    }

    if definitions[id].location.file.ends_with(".rb") {
        let mut all_ids = vec![id];
        for level in &w.levels {
            for reach in level {
                if let Dependent::Symbol(dep) = reach.dependent {
                    all_ids.push(dep);
                }
            }
        }
        for (tid, hops, inferred) in rspec_subject_tests(index, &all_ids) {
            keep(tid, hops, inferred);
        }
    }

    let list: Vec<(usize, usize, bool)> = found
        .into_iter()
        .map(|(tid, (possible, hops))| (tid, hops, possible))
        .collect();
    (list, file_hits)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Runner {
    Cargo,
    Pytest,
    Js,
    Rspec,
    Go,
}

fn quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@".contains(&b))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

fn cargo_cmd(index: &Index, def: &Definition) -> Option<(String, String)> {
    let file = Path::new(&def.location.file);
    if file.extension()? != "rs" {
        return None;
    }
    let (target, filter) = def.qualified.split_once("::")?;
    if target == "crate" {
        return None;
    }
    let package = file.ancestors().skip(1).find(|dir| {
        dir.join("Cargo.toml")
            .to_str()
            .is_some_and(|m| index.files.contains_key(m))
    })?;
    let mut cmd = "cargo test".to_owned();
    if !package.as_os_str().is_empty() {
        cmd.push_str(&format!(
            " --manifest-path {}",
            quote(package.join("Cargo.toml").to_str()?)
        ));
    }
    let relative = file.strip_prefix(package).ok()?;
    let stem = relative.file_stem()?.to_str()?;
    if relative.parent() == Some(Path::new("tests")) && target.ends_with(&format!("_test_{stem}")) {
        cmd.push_str(&format!(" --test {}", quote(stem)));
    }
    Some((cmd, quote(filter)))
}

fn pytest_cmd(def: &Definition) -> Option<String> {
    let file = Path::new(&def.location.file);
    if file.extension()? != "py" {
        return None;
    }
    let stem = file.file_stem()?.to_str()?;
    if !(stem.starts_with("test_") || stem.ends_with("_test")) {
        return None;
    }
    let parts: Vec<&str> = name(def).split("::").collect();
    let collected = match parts.as_slice() {
        [_] => true,
        [class, _] => class.starts_with("Test"),
        _ => false,
    };
    collected.then(|| quote(&def.qualified))
}

fn go_cmd(index: &Index, def: &Definition) -> Option<(String, String)> {
    let file = &def.location.file;
    let test = name(def);
    if !file.ends_with("_test.go") || !index.files.contains_key("go.mod") || test.contains("::") {
        return None;
    }
    if !test.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return None;
    }
    let package = match Path::new(file).parent().and_then(Path::to_str) {
        Some("") | None => ".".to_owned(),
        Some(dir) => format!("./{dir}"),
    };
    Some((format!("go test {}", quote(&package)), test.to_owned()))
}

fn js_cmd(workspace: &Workspace, index: &Index, file: &str) -> Option<(String, String)> {
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
            && let Ok(pkg) = serde_json::from_str::<Value>(&text)
        {
            let listed = |tool: &str| {
                ["dependencies", "devDependencies"]
                    .iter()
                    .any(|key| pkg[key].get(tool).is_some())
            };
            let configured = |prefix: &str| {
                index.files.keys().any(|other| {
                    let parent = other.rsplit_once('/').map_or("", |(p, _)| p);
                    parent == dir
                        && other
                            .rsplit('/')
                            .next()
                            .is_some_and(|n| n.starts_with(prefix))
                })
            };
            let jest = listed("jest") || pkg.get("jest").is_some() || configured("jest.config.");
            let vitest = listed("vitest") || configured("vitest.config.");
            let script = pkg["scripts"]["test"].as_str().unwrap_or_default();
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
        if index.files.contains_key(&rel_path("bunfig.toml")) {
            return Some(("bun test".to_owned(), quote(file)));
        }
    }
    None
}

fn rspec_cmd(index: &Index, file: &str) -> Option<(String, String)> {
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

fn synthesize_commands(
    workspace: &Workspace,
    index: &Index,
    tests: &[(usize, usize, bool)],
    files: &BTreeMap<String, TestFileHit>,
) -> (Vec<String>, usize) {
    let mut groups = BTreeMap::<(Runner, String), Vec<String>>::new();
    let mut push = |runner: Runner, prefix: String, filter: String| {
        let list = groups.entry((runner, prefix)).or_default();
        if !list.contains(&filter) {
            list.push(filter);
        }
    };
    for &(id, _, _) in tests {
        let def = &index.definitions[id];
        let picked = cargo_cmd(index, def)
            .map(|(p, f)| (Runner::Cargo, p, f))
            .or_else(|| pytest_cmd(def).map(|f| (Runner::Pytest, "pytest".into(), f)))
            .or_else(|| go_cmd(index, def).map(|(p, f)| (Runner::Go, p, f)))
            .or_else(|| {
                js_cmd(workspace, index, &def.location.file).map(|(p, f)| (Runner::Js, p, f))
            })
            .or_else(|| rspec_cmd(index, &def.location.file).map(|(p, f)| (Runner::Rspec, p, f)));
        if let Some((runner, prefix, filter)) = picked {
            push(runner, prefix, filter);
        }
    }
    for file in files.keys() {
        if let Some((p, f)) = js_cmd(workspace, index, file) {
            push(Runner::Js, p, f);
        } else if let Some((p, f)) = rspec_cmd(index, file) {
            push(Runner::Rspec, p, f);
        }
    }
    let mut left = 0usize;
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

// ---------------------------------------------------------------------------
// Core review computation & rendering

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RiskTier {
    Low,
    Medium,
    High,
}

impl RiskTier {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

struct ChangedSymbolInfo {
    id: usize,
    added: usize,
    deleted: usize,
    is_new: bool,
    is_hub: bool,
    is_bridge: bool,
    cross_lang: bool,
    bridge_desc: Option<String>,
    reachable_tests: usize,
}

struct DeletedSymbolInfo {
    name: String,
    kind: String,
    file: String,
    old: (usize, usize),
    removed_lines: usize,
    external_callers: usize,
}

struct BlastItem<'a> {
    hop: usize,
    dependent: Dependent<'a>,
    edge: &'a Edge,
    via: usize,
    possible: bool,
    via_hub: bool,
    via_bridge: bool,
}

fn scope_label(since: &Since) -> String {
    match since {
        Since::Head => "HEAD".to_owned(),
        Since::Staged => "staged".to_owned(),
        Since::Base { name, short, .. } => format!("base {name} @ {short}"),
    }
}

fn scope_json(since: &Since) -> (String, Option<Value>) {
    match since {
        Since::Head => ("HEAD".to_owned(), None),
        Since::Staged => ("staged".to_owned(), None),
        Since::Base {
            name,
            commit,
            short,
        } => (
            format!("base:{name}"),
            Some(json!({"ref": name, "merge_base": commit, "short": short})),
        ),
    }
}

fn format_delta(added: usize, deleted: usize) -> String {
    format!("+{added} -{deleted}")
}

fn format_line_ranges(ranges: &[(usize, usize)]) -> String {
    let shown: Vec<String> = ranges
        .iter()
        .take(4)
        .map(|&(start, end)| {
            if start == end {
                format!("L{start}")
            } else {
                format!("L{start}-{end}")
            }
        })
        .collect();
    if ranges.len() > 4 {
        format!("{}, +{} more", shown.join(", "), ranges.len() - 4)
    } else {
        shown.join(", ")
    }
}

/// Filter `context::changes` to remove generated/lockfile files, pure renames,
/// and pure whitespace-only hunks, returning filtered changes suitable for
/// symbol attribution & deleted-symbol caller seeding.
fn filter_semantic_changes(
    index: &Index,
    raw_changes: Vec<(String, Change)>,
    diffs_by_path: &BTreeMap<String, &FileDiff>,
    pure_rename_old: &BTreeSet<String>,
    edited_rename_old: &BTreeSet<String>,
) -> Vec<(String, Change)> {
    let mut filtered = Vec::new();
    for (file, change) in raw_changes {
        if pure_rename_old.contains(&file) || edited_rename_old.contains(&file) {
            continue;
        }
        let churn = diffs_by_path.get(&file).map_or(0, |d| d.added + d.deleted);
        if classify_file(index, &file, churn) == FileCategory::Generated {
            continue;
        }
        if let Some(diff) = diffs_by_path.get(&file) {
            if diff.is_pure_rename() || diff.is_whitespace_only() {
                continue;
            }
            match change {
                Change::Ranges(_) => {
                    // For edited renames or mixed whitespace/semantic files, prefer
                    // the exact non-whitespace hunk ranges from `git diff -M`.
                    let ranges = diff.semantic_ranges();
                    if !ranges.is_empty() {
                        filtered.push((file, Change::Ranges(ranges)));
                    }
                }
                Change::Origin(_) => {}
                Change::Deleted(deletions) => {
                    if diff.semantic_deleted > 0 || diff.deleted_file {
                        filtered.push((file, Change::Deleted(deletions)));
                    }
                }
                other => filtered.push((file, other)),
            }
        } else {
            filtered.push((file, change));
        }
    }
    filtered
}

/// Attribute per-symbol `(+added, -deleted)` counts from non-whitespace hunks
/// of `diff` against the symbols in `index`.
fn symbol_churn(index: &Index, diff: &FileDiff) -> BTreeMap<usize, (usize, usize)> {
    let mut counts = BTreeMap::<usize, (usize, usize)>::new();
    let pick_innermost = |line: usize| -> Option<usize> {
        let overlapping = index.at(&Location {
            file: diff.path.clone(),
            start: line,
            end: line,
        });
        overlapping
            .iter()
            .copied()
            .find(|&id| !matches!(index.definitions[id].kind.as_str(), "file" | "module"))
            .or_else(|| overlapping.first().copied())
    };
    for hunk in diff.hunks.iter().filter(|h| !h.whitespace_only) {
        for &line in &hunk.added_lines {
            if let Some(id) = pick_innermost(line) {
                counts.entry(id).or_default().0 += 1;
            }
        }
        if hunk.deleted > 0 {
            let probe = hunk.deleted_after.max(1);
            if let Some(id) = pick_innermost(probe).or_else(|| pick_innermost(probe + 1)) {
                counts.entry(id).or_default().1 += hunk.deleted;
            }
        }
    }
    counts
}

/// Extract the exact touched definitions in `file` for semantic line ranges
/// (same innermost + declaration rule as `impact::seeded`, letting concrete
/// symbols win ties against `"file"`/`"module"` wrappers).
fn touched_symbols_in_ranges(index: &Index, file: &str, ranges: &[(usize, usize)]) -> Vec<usize> {
    let definitions = &index.definitions;
    let mut ids = BTreeSet::new();
    for &(start, end) in ranges {
        let overlapping = index.at(&Location {
            file: file.to_owned(),
            start,
            end,
        });
        let has_concrete = overlapping
            .iter()
            .any(|&id| !matches!(definitions[id].kind.as_str(), "file" | "module"));
        for &id in &overlapping {
            if has_concrete && matches!(definitions[id].kind.as_str(), "file" | "module") {
                continue;
            }
            let span = &definitions[id].location;
            let declared = (start..=end).contains(&definitions[id].declaration);
            let innermost = !overlapping.iter().any(|&other| {
                if has_concrete && matches!(definitions[other].kind.as_str(), "file" | "module") {
                    return false;
                }
                let inner = &definitions[other].location;
                inner != span && span.start <= inner.start && inner.end <= span.end
            });
            if declared || innermost {
                ids.insert(id);
            }
        }
    }
    ids.into_iter().collect()
}

fn collect_deleted_symbols(
    deleted_specs: &[(String, String, String, (usize, usize))],
    index: &Index,
    deletion_seed_ids: &BTreeSet<usize>,
    modified_files: &BTreeSet<String>,
) -> Vec<DeletedSymbolInfo> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let external_callers = deletion_seed_ids
        .iter()
        .filter(|&&id| {
            let def = &index.definitions[id];
            !def.test && !modified_files.contains(&def.location.file)
        })
        .count();
    for (file, sym_name, sym_kind, old) in deleted_specs {
        if !seen.insert((file.clone(), sym_name.clone(), *old)) {
            continue;
        }
        out.push(DeletedSymbolInfo {
            name: sym_name.clone(),
            kind: sym_kind.clone(),
            file: file.clone(),
            old: *old,
            removed_lines: old.1.saturating_sub(old.0) + 1,
            external_callers,
        });
    }
    out
}

/// Catches symbols deleted inside mixed `(-D +A)` hunks where Git Myers paired
/// a deleted function's closing delimiter with an adjacent edited function, so
/// `context::changes` reported `Change::Ranges` instead of `Change::Deleted`.
fn detect_mixed_hunk_deletions(
    workspace: &Workspace,
    index: &Index,
    since: &Since,
    file_diffs: &[FileDiff],
    already_deleted: &[(String, String, String, (usize, usize))],
) -> Vec<(String, String, String, (usize, usize))> {
    let rev = since.rev();
    let mut existing_deleted: BTreeSet<(String, String)> = already_deleted
        .iter()
        .map(|(f, n, _, _)| (f.clone(), n.clone()))
        .collect();
    let mut found = Vec::new();

    for diff in file_diffs {
        if diff.semantic_deleted == 0
            || diff.new_file
            || diff.deleted_file
            || diff.is_pure_rename()
            || diff.is_whitespace_only()
            || classify_file(index, &diff.path, diff.added + diff.deleted)
                == FileCategory::Generated
        {
            continue;
        }
        let Ok(out) = workspace.git(&["show", &format!("{rev}:./{}", diff.path)]) else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let Ok(text) = String::from_utf8(out.stdout) else {
            continue;
        };
        let mut snap = crate::source::Snapshot::from_text(&diff.path, text);
        if snap.index(&diff.path).is_err() {
            continue;
        }
        let old_syms = snap.symbols();
        if old_syms.is_empty() {
            continue;
        }
        let current_names: BTreeSet<&str> = index
            .definitions
            .iter()
            .filter(|d| d.location.file == diff.path)
            .flat_map(|d| {
                let n = name(d);
                let tail = n.rsplit("::").next().unwrap_or(n);
                [n, tail]
            })
            .collect();
        for (sym_name, sym_kind, start, end) in &old_syms {
            if matches!(sym_kind.as_str(), "file" | "module" | "imports") {
                continue;
            }
            let tail = sym_name.rsplit("::").next().unwrap_or(sym_name.as_str());
            if current_names.contains(sym_name.as_str()) || current_names.contains(tail) {
                continue;
            }
            // Keep outermost removed symbol (skip members of a removed outer item).
            let nested_in_removed = old_syms.iter().any(|(on, _, os, oe)| {
                (*os, *oe) != (*start, *end)
                    && *os <= *start
                    && *end <= *oe
                    && !current_names.contains(on.as_str())
                    && !current_names.contains(on.rsplit("::").next().unwrap_or(on.as_str()))
            });
            if nested_in_removed {
                continue;
            }
            if existing_deleted.insert((diff.path.clone(), sym_name.clone())) {
                found.push((
                    diff.path.clone(),
                    sym_name.clone(),
                    sym_kind.clone(),
                    (*start, *end),
                ));
            }
        }
    }
    found
}

pub(crate) fn render_review(
    workspace: &Workspace,
    index: &Index,
    since: &Since,
    depth: usize,
    budget: usize,
    as_json: bool,
) -> Result<String> {
    let file_diffs = collect_diff(workspace, since)?;
    let raw_changes = changes(workspace, index, since)?;

    if file_diffs.is_empty() && raw_changes.is_empty() {
        let msg = since.nothing_changed(workspace);
        if as_json {
            let (scope, base) = scope_json(since);
            let mut obj = Map::new();
            obj.insert("scope".into(), json!(scope));
            if let Some(base) = base {
                obj.insert("base".into(), base);
            }
            obj.insert("depth".into(), json!(depth));
            obj.insert("risk".into(), json!("low"));
            obj.insert("risk_reasons".into(), json!([]));
            obj.insert(
                "files".into(),
                json!({"total": 0, "source": 0, "test": 0, "generated": 0, "other": 0}),
            );
            obj.insert(
                "lines".into(),
                json!({"added": 0, "deleted": 0, "semantic_added": 0, "semantic_deleted": 0}),
            );
            for key in [
                "changed_symbols",
                "blast_radius",
                "untested_symbols",
                "tests",
                "commands",
                "diff_summary",
                "notes",
            ] {
                obj.insert(key.into(), json!([]));
            }
            obj.insert("message".into(), json!(msg));
            obj.insert("omitted".into(), json!({}));
            obj.insert("capped".into(), json!(false));
            return Ok(format!("{}\n", serde_json::to_string(&obj)?));
        }
        return Ok(format!("{msg}\n"));
    }

    let diffs_by_path: BTreeMap<String, &FileDiff> =
        file_diffs.iter().map(|d| (d.path.clone(), d)).collect();
    let mut pure_rename_old = BTreeSet::new();
    let mut edited_rename_old = BTreeSet::new();
    for diff in &file_diffs {
        if let Some(old) = &diff.old_path {
            if diff.is_pure_rename() {
                pure_rename_old.insert(old.clone());
            } else {
                edited_rename_old.insert(old.clone());
            }
        }
    }

    // Count files by category & total lines.
    let mut source_files = 0usize;
    let mut test_files = 0usize;
    let mut generated_files = 0usize;
    let mut other_files = 0usize;
    let mut total_added = 0usize;
    let mut total_deleted = 0usize;
    let mut semantic_added = 0usize;
    let mut semantic_deleted = 0usize;
    let mut prod_semantic_churn = 0usize;

    let mut modified_files = BTreeSet::<String>::new();
    for diff in &file_diffs {
        let cat = classify_file(index, &diff.path, diff.added + diff.deleted);
        match cat {
            FileCategory::Source => source_files += 1,
            FileCategory::Test => test_files += 1,
            FileCategory::Generated => generated_files += 1,
            FileCategory::Other => other_files += 1,
        }
        total_added += diff.added;
        total_deleted += diff.deleted;
        if cat != FileCategory::Generated {
            semantic_added += diff.semantic_added;
            semantic_deleted += diff.semantic_deleted;
            if cat == FileCategory::Source {
                prod_semantic_churn += diff.semantic_added + diff.semantic_deleted;
            }
            if !diff.is_pure_rename() && !diff.is_whitespace_only() {
                modified_files.insert(diff.path.clone());
            }
        }
    }

    // Filter semantic changes for symbol resolution.
    let filtered_changes = filter_semantic_changes(
        index,
        raw_changes,
        &diffs_by_path,
        &pure_rename_old,
        &edited_rename_old,
    );

    // Resolve changed symbols in current files, plus deleted-symbol caller seeds via impact::changed_in.
    let mut touched_ids = BTreeSet::<usize>::new();
    let mut per_symbol_churn = BTreeMap::<usize, (usize, usize)>::new();
    let mut deleted_specs = Vec::<(String, String, String, (usize, usize))>::new();
    let mut deletion_only = Vec::<(String, Change)>::new();
    for (file, change) in filtered_changes {
        match change {
            Change::Ranges(ranges) => {
                let ids = touched_symbols_in_ranges(index, &file, &ranges);
                touched_ids.extend(ids.iter().copied());
                if let Some(diff) = diffs_by_path.get(&file) {
                    for (id, delta) in symbol_churn(index, diff) {
                        let entry = per_symbol_churn.entry(id).or_default();
                        entry.0 += delta.0;
                        entry.1 += delta.1;
                    }
                }
            }
            Change::Deleted(dels) => {
                for d in &dels {
                    for (sym_name, sym_kind) in &d.symbols {
                        deleted_specs.push((
                            file.clone(),
                            sym_name.clone(),
                            sym_kind.clone(),
                            d.old,
                        ));
                    }
                }
                if dels.iter().any(|d| !d.symbols.is_empty()) {
                    deletion_only.push((file, Change::Deleted(dels)));
                }
            }
            _ => {}
        }
    }

    let mixed_deleted =
        detect_mixed_hunk_deletions(workspace, index, since, &file_diffs, &deleted_specs);
    for (file, sym_name, sym_kind, old) in mixed_deleted {
        deletion_only.push((
            file.clone(),
            Change::Deleted(vec![Deletion {
                after: old.0,
                old,
                symbols: vec![(sym_name.clone(), sym_kind.clone())],
                lines: Vec::new(),
                removed: Vec::new(),
            }]),
        ));
        deleted_specs.push((file, sym_name, sym_kind, old));
    }

    let deletion_seed_ids: BTreeSet<usize> = if deletion_only.is_empty() {
        BTreeSet::new()
    } else {
        let (seeds, _) = changed_in(workspace, index, &deletion_only);
        seeds.into_iter().collect()
    };
    let deleted_symbols =
        collect_deleted_symbols(&deleted_specs, index, &deletion_seed_ids, &modified_files);

    // Build architectural graph (hub centrality & cross-community/cross-language bridges).
    let graph = ReviewGraph::build(index);

    // Seed set for blast radius walk = all touched symbols + callers of deleted symbols.
    let mut seed_set = touched_ids.clone();
    seed_set.extend(deletion_seed_ids.iter().copied());
    let seed_vec: Vec<usize> = seed_set.iter().copied().collect();

    // Reverse walk for blast radius up to `depth`.
    let blast_walk: Walk<'_> = walk(index, &seed_vec, depth);

    // Collect per-symbol test coverage & aggregate reachable tests.
    let mut aggregate_tests = BTreeMap::<usize, (bool, usize)>::new();
    let mut aggregate_test_files = BTreeMap::<String, TestFileHit>::new();
    let mut keep_test = |tid: usize, hops: usize, possible: bool| {
        let entry = aggregate_tests.entry(tid).or_insert((possible, hops));
        *entry = (*entry).min((possible, hops));
    };

    let mut changed_infos = Vec::<ChangedSymbolInfo>::new();
    let mut hub_ids = BTreeSet::<usize>::new();
    let mut bridge_ids = BTreeSet::<usize>::new();

    for &id in &touched_ids {
        let def = &index.definitions[id];
        let (mut added, deleted) = per_symbol_churn.get(&id).copied().unwrap_or((0, 0));
        let is_new = diffs_by_path
            .get(&def.location.file)
            .is_some_and(|d| d.new_file);
        if added == 0 && deleted == 0 && is_new {
            added = def.location.end.saturating_sub(def.location.start) + 1;
        }
        let is_hub = graph.is_hub[id];
        let (is_bridge, cross_lang, bridge_desc) = graph.bridge_info(index, id);
        if is_hub {
            hub_ids.insert(id);
        }
        if is_bridge {
            bridge_ids.insert(id);
        }

        let (sym_tests, sym_files) = tests_for_symbol(index, id, depth.max(4));
        let reachable_tests = sym_tests.len() + sym_files.len();
        for (tid, hops, possible) in sym_tests {
            keep_test(tid, hops, possible);
        }
        for (file, hit) in sym_files {
            aggregate_test_files
                .entry(file)
                .and_modify(|existing| {
                    if (hit.possible, hit.hops) < (existing.possible, existing.hops) {
                        *existing = hit.clone();
                    }
                })
                .or_insert(hit);
        }

        changed_infos.push(ChangedSymbolInfo {
            id,
            added,
            deleted,
            is_new,
            is_hub,
            is_bridge,
            cross_lang,
            bridge_desc,
            reachable_tests,
        });
    }

    // Augment aggregate test files with JS/TS & Ruby module graphs (barrels,
    // require_relative, shared test configs) without masking untested Rust/Py functions.
    let mut extra_notes = Vec::<String>::new();
    let all_reached_files: BTreeSet<String> = seed_vec
        .iter()
        .map(|&id| index.definitions[id].location.file.clone())
        .chain(blast_walk.levels.iter().flat_map(|lvl| {
            lvl.iter().map(|r| match r.dependent {
                Dependent::Symbol(id) => index.definitions[id].location.file.clone(),
                Dependent::Scope(file) => file.to_owned(),
            })
        }))
        .collect();

    let js_files: Vec<String> = all_reached_files
        .iter()
        .filter(|f| is_js_file(f))
        .cloned()
        .collect();
    let mut js_covered_source_files = BTreeSet::<String>::new();
    if !js_files.is_empty() {
        let js_graph = JsGraph::build(workspace, index);
        let reach = js_graph.reach_from(index, &js_files);
        if !reach.test_files.is_empty() {
            js_covered_source_files.extend(js_files.iter().cloned());
        }
        for (test_file, (line, why, _)) in reach.test_files {
            aggregate_test_files
                .entry(test_file.clone())
                .or_insert_with(|| TestFileHit {
                    hops: 1,
                    possible: false,
                    at: Location {
                        file: test_file,
                        start: line,
                        end: line,
                    },
                    note: Some(why),
                });
        }
        for file in &js_files {
            if is_shared_js_config_or_setup(file) {
                extra_notes.push(format!(
                    "Shared JS/TS test config or setup {file} touched; run full suite."
                ));
            } else if let Some(cfg) = config_reference(workspace, index, "", file) {
                extra_notes.push(format!(
                    "{file} is referenced by runner config {cfg}; run full suite."
                ));
            }
        }
    }

    let ruby_files: Vec<String> = all_reached_files
        .iter()
        .filter(|f| f.ends_with(".rb"))
        .cloned()
        .collect();
    if !ruby_files.is_empty() {
        let ruby_graph = RubyGraph::build(workspace, index);
        let reach = ruby_graph.reach_from(&ruby_files);
        for (spec_file, (line, why)) in reach.test_files {
            aggregate_test_files
                .entry(spec_file.clone())
                .or_insert_with(|| TestFileHit {
                    hops: 1,
                    possible: false,
                    at: Location {
                        file: spec_file,
                        start: line,
                        end: line,
                    },
                    note: Some(why),
                });
        }
        if ruby_files.iter().any(|f| is_shared_ruby_helper(f)) {
            extra_notes.push("Shared Ruby test helper touched; run full RSpec suite.".into());
        }
    }

    // If a JS/TS file has a test file reaching it via barrel/dynamic import and
    // zero indexed test symbols reached individual functions inside it, check
    // whether the test file directly mentions the function's file. Only keep
    // untested warning for functions with 0 direct & file-level test hits.
    for info in &mut changed_infos {
        let file = &index.definitions[info.id].location.file;
        if info.reachable_tests == 0 && js_covered_source_files.contains(file) {
            // Check if a JS test file imports the whole module (no indexed per-fn edge).
            let has_indexed_js_tests = aggregate_tests
                .keys()
                .any(|&tid| is_js_file(&index.definitions[tid].location.file));
            if !has_indexed_js_tests {
                info.reachable_tests = 1;
            }
        }
    }

    // Sort changed symbols: production first, hubs/bridges first, higher churn first.
    changed_infos.sort_by_key(|info| {
        let def = &index.definitions[info.id];
        let is_test = def.test || is_test_file(&def.location.file);
        (
            is_test,
            !(info.is_hub || info.is_bridge),
            !info.is_hub,
            Reverse(info.added + info.deleted),
            &def.location,
            info.id,
        )
    });

    // Build external Blast Radius (strictly OUTSIDE modified files, non-test dependents).
    let mut blast_items = Vec::<BlastItem<'_>>::new();
    for (offset, level) in blast_walk.levels.iter().enumerate() {
        let hop = offset + 1;
        for reach in level {
            let (dep_file, is_test_dep) = match reach.dependent {
                Dependent::Symbol(id) => {
                    let def = &index.definitions[id];
                    (
                        def.location.file.as_str(),
                        def.test || is_test_file(&def.location.file),
                    )
                }
                Dependent::Scope(file) => (file, is_test_file(file)),
            };
            if is_test_dep || modified_files.contains(dep_file) {
                continue;
            }
            blast_items.push(BlastItem {
                hop,
                dependent: reach.dependent,
                edge: reach.edge,
                via: reach.via,
                possible: reach.possible,
                via_hub: hub_ids.contains(&reach.via),
                via_bridge: bridge_ids.contains(&reach.via),
            });
        }
    }
    // Also surface direct external callers of deleted symbols as 1-hop blast items!
    for &caller_id in &deletion_seed_ids {
        let def = &index.definitions[caller_id];
        if def.test
            || is_test_file(&def.location.file)
            || modified_files.contains(&def.location.file)
        {
            continue;
        }
        let already = blast_items
            .iter()
            .any(|b| b.dependent == Dependent::Symbol(caller_id));
        if !already && let Some(edge) = index.outgoing(caller_id).next() {
            blast_items.push(BlastItem {
                hop: 1,
                dependent: Dependent::Symbol(caller_id),
                edge,
                via: caller_id,
                possible: true,
                via_hub: false,
                via_bridge: false,
            });
        }
    }
    // Suppress redundant `file scope` import lines when a concrete symbol in the
    // same file already appears as a dependent of the same `via` symbol.
    let concrete_file_via: BTreeSet<(&str, usize)> = blast_items
        .iter()
        .filter_map(|b| match b.dependent {
            Dependent::Symbol(id) => Some((index.definitions[id].location.file.as_str(), b.via)),
            Dependent::Scope(_) => None,
        })
        .collect();
    blast_items.retain(|b| match b.dependent {
        Dependent::Scope(file) => !concrete_file_via.contains(&(file, b.via)),
        Dependent::Symbol(_) => true,
    });
    let mut by_hop_counts = BTreeMap::<usize, usize>::new();
    for item in &blast_items {
        *by_hop_counts.entry(item.hop).or_default() += 1;
    }
    blast_items.sort_by_key(|b| {
        (
            b.hop,
            b.possible,
            !(b.via_hub || b.via_bridge),
            &b.edge.location,
        )
    });

    // Identify untested modified non-test functions/methods (0 reachable tests).
    let untested: Vec<&ChangedSymbolInfo> = changed_infos
        .iter()
        .filter(|info| {
            let def = &index.definitions[info.id];
            info.reachable_tests == 0
                && !def.test
                && !is_test_file(&def.location.file)
                && is_callable(def)
                && !is_entrypoint_main(def)
        })
        .collect();

    // Sort aggregate tests & filter out test files already covered by individual test defs.
    let mut sorted_tests: Vec<(usize, usize, bool)> = aggregate_tests
        .into_iter()
        .map(|(tid, (possible, hops))| (tid, hops, possible))
        .collect();
    sorted_tests.sort_by_key(|&(tid, hops, possible)| {
        (possible, hops, &index.definitions[tid].location, tid)
    });
    let covered_test_files: BTreeSet<&str> = sorted_tests
        .iter()
        .map(|&(tid, _, _)| index.definitions[tid].location.file.as_str())
        .collect();
    aggregate_test_files.retain(|file, _| !covered_test_files.contains(file.as_str()));

    let (run_commands, omitted_filters) =
        synthesize_commands(workspace, index, &sorted_tests, &aggregate_test_files);

    // Compute overall risk tier (`low` / `medium` / `high`).
    let hubs_touched = hub_ids.len();
    let bridges_touched = bridge_ids.len();
    let deleted_used = deleted_symbols
        .iter()
        .filter(|d| d.external_callers > 0)
        .count();
    let external_dependents = blast_items.len();
    let untested_hub_or_bridge = untested.iter().any(|u| u.is_hub || u.is_bridge);

    let risk = if deleted_used > 0
        || untested_hub_or_bridge
        || (hubs_touched > 0 && external_dependents >= 5)
        || external_dependents >= 10
        || (bridges_touched > 0 && external_dependents >= 4)
        || (untested.len() >= 3 && external_dependents >= 3)
    {
        RiskTier::High
    } else if hubs_touched > 0
        || bridges_touched > 0
        || external_dependents >= 2
        || !untested.is_empty()
        || prod_semantic_churn >= 150
    {
        RiskTier::Medium
    } else {
        RiskTier::Low
    };

    let mut risk_reasons = Vec::<String>::new();
    if deleted_used > 0 {
        risk_reasons.push(format!(
            "{} with external refs",
            plural(deleted_used, "deleted symbol")
        ));
    }
    if hubs_touched > 0 {
        risk_reasons.push(plural(hubs_touched, "hub"));
    }
    if bridges_touched > 0 {
        risk_reasons.push(plural(bridges_touched, "bridge"));
    }
    if !untested.is_empty() {
        risk_reasons.push(format!("{} untested", untested.len()));
    }
    if external_dependents > 0 {
        risk_reasons.push(plural(external_dependents, "external dependent"));
    }
    if risk_reasons.is_empty() {
        if source_files == 0 && test_files > 0 {
            risk_reasons.push("test-only".into());
        } else if semantic_added == 0 && semantic_deleted == 0 {
            risk_reasons.push("no semantic code edits".into());
        } else {
            risk_reasons.push("isolated & tested".into());
        }
    }

    // 1. Change Header
    let mut file_parts = Vec::new();
    if source_files > 0 {
        file_parts.push(format!("{source_files} source"));
    }
    if test_files > 0 {
        file_parts.push(format!("{test_files} test"));
    }
    if generated_files > 0 {
        file_parts.push(format!("{generated_files} generated"));
    }
    if other_files > 0 {
        file_parts.push(format!("{other_files} other"));
    }
    let files_breakdown = if file_parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", file_parts.join(", "))
    };
    let header = vec![format!(
        "Review [{}] · risk: {} ({}) · {}{} · {}",
        scope_label(since),
        risk.as_str(),
        risk_reasons.join(", "),
        plural(file_diffs.len(), "file"),
        files_breakdown,
        format_delta(total_added, total_deleted),
    )];

    // 2. Changed Symbols section
    let total_changed_syms = changed_infos.len() + deleted_symbols.len();
    let mut changed_section = Section::new(
        "changed_symbols",
        format!("Changed symbols · {total_changed_syms}"),
        4,
    );
    for info in &changed_infos {
        let def = &index.definitions[info.id];
        let mut tags = Vec::new();
        if info.is_hub {
            tags.push("hub".to_owned());
        }
        if info.is_bridge {
            match &info.bridge_desc {
                Some(desc) => tags.push(format!("bridge:{desc}")),
                None => tags.push("bridge".to_owned()),
            }
        }
        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tags.join(", "))
        };
        let status = if info.is_new { "added" } else { "modified" };
        let line = format!(
            "  {} ({}){tag_str}",
            label(def),
            format_delta(info.added, info.deleted)
        );
        let mut value = node(def);
        value["status"] = json!(status);
        value["added"] = json!(info.added);
        value["deleted"] = json!(info.deleted);
        value["hub"] = json!(info.is_hub);
        value["bridge"] = json!(info.is_bridge);
        value["cross_language_bridge"] = json!(info.cross_lang);
        if let Some(desc) = &info.bridge_desc {
            value["bridge_detail"] = json!(desc);
        }
        value["in_degree"] = json!(graph.in_deg[info.id]);
        value["caller_files"] = json!(graph.caller_files[info.id]);
        value["caller_areas"] = json!(graph.caller_areas[info.id]);
        value["reachable_tests"] = json!(info.reachable_tests);
        value["tested"] = json!(info.reachable_tests > 0);
        changed_section.push(line, value);
    }
    for del in &deleted_symbols {
        let warn = if del.external_callers > 0 {
            format!(" · ! {}", plural(del.external_callers, "external ref"))
        } else {
            String::new()
        };
        let line = format!(
            "  {} [{}] deleted from {}:{}-{} (-{}){warn}",
            short(&del.name, NAME_BYTES),
            del.kind,
            display_path(&del.file),
            del.old.0,
            del.old.1,
            del.removed_lines
        );
        changed_section.push(
            line,
            json!({
                "name": del.name,
                "kind": del.kind,
                "file": del.file,
                "old_start": del.old.0,
                "old_end": del.old.1,
                "status": "deleted",
                "deleted": del.removed_lines,
                "external_refs": del.external_callers,
            }),
        );
    }
    if changed_section.lines.is_empty() {
        changed_section.title = "Changed symbols · none (no semantic symbol edits)".into();
    }

    // 3. Blast Radius section (external callers/dependents grouped by hop)
    let hop_breakdown: Vec<String> = by_hop_counts
        .iter()
        .map(|(h, count)| format!("{count} at {h}-hop"))
        .collect();
    let mut blast_title = if blast_items.is_empty() {
        format!("Blast radius (≤{depth} hops) · 0 external dependents")
    } else {
        format!(
            "Blast radius (≤{depth} hops) · {} ({})",
            plural(blast_items.len(), "external dependent"),
            hop_breakdown.join(", ")
        )
    };
    if hubs_touched > 0 || bridges_touched > 0 {
        let mut arch = Vec::new();
        if hubs_touched > 0 {
            arch.push(plural(hubs_touched, "hub"));
        }
        if bridges_touched > 0 {
            arch.push(plural(bridges_touched, "bridge"));
        }
        blast_title.push_str(&format!(" · {}", arch.join(", ")));
    }
    let mut blast_section = Section::new("blast_radius", blast_title, 4);
    for item in &blast_items {
        let (who, dep_json) = match item.dependent {
            Dependent::Symbol(id) => (label(&index.definitions[id]), node(&index.definitions[id])),
            Dependent::Scope(file) => (
                format!("file scope {}", display_path(file)),
                json!({
                    "kind": "file scope",
                    "file": file,
                    "start": item.edge.location.start,
                    "end": item.edge.location.end,
                }),
            ),
        };
        let via_name = name(&index.definitions[item.via]);
        let mut via_roles = Vec::<&'static str>::new();
        if item.via_hub {
            via_roles.push("hub");
        }
        if item.via_bridge {
            via_roles.push("bridge");
        }
        let role_suffix = if via_roles.is_empty() {
            String::new()
        } else {
            format!(" [{}]", via_roles.join(", "))
        };
        let rel = relationship(item.edge.kind);
        let possible_tag = if item.possible { "possible " } else { "" };
        let line = format!(
            "  [{}-hop] {who} · {possible_tag}{rel} at {} → {}{role_suffix}",
            item.hop,
            item.edge.location,
            short(via_name, NAME_BYTES),
        );
        blast_section.push(
            line,
            json!({
                "hop": item.hop,
                "dependent": dep_json,
                "relationship": rel,
                "at": item.edge.location.to_string(),
                "possible": item.possible,
                "via": via_name,
                "via_roles": via_roles,
            }),
        );
    }

    // 4a. Untested changed symbols warning (Coverage gaps)
    let mut untested_section = (!untested.is_empty()).then(|| {
        let mut sec = Section::new(
            "untested_symbols",
            format!(
                "Coverage gaps · {} with 0 reachable tests",
                plural(untested.len(), "untested changed symbol")
            ),
            4,
        );
        for info in &untested {
            let def = &index.definitions[info.id];
            let mut tags = Vec::new();
            if info.is_hub {
                tags.push("hub");
            }
            if info.is_bridge {
                tags.push("bridge");
            }
            let tag_str = if tags.is_empty() {
                String::new()
            } else {
                format!(" [{}]", tags.join(", "))
            };
            let line = format!(
                "  ! {} ({}){tag_str} · 0 reachable tests",
                label(def),
                format_delta(info.added, info.deleted)
            );
            let mut val = node(def);
            val["added"] = json!(info.added);
            val["deleted"] = json!(info.deleted);
            val["hub"] = json!(info.is_hub);
            val["bridge"] = json!(info.is_bridge);
            sec.push(line, val);
        }
        sec
    });

    // 4b. Reachable Tests & Runnable Commands sections
    let total_tests = sorted_tests.len() + aggregate_test_files.len();
    let mut tests_section = Section::new(
        "tests",
        if total_tests == 0 {
            "Tests · none reach changed symbols through indexed links".into()
        } else {
            format!("Tests · {total_tests} reachable")
        },
        3,
    );
    for &(tid, hops, possible) in &sorted_tests {
        let def = &index.definitions[tid];
        let hop_text = if hops == 0 {
            "modified test".to_owned()
        } else {
            plural(hops, "hop")
        };
        let poss = if possible { "possible · " } else { "" };
        let mut val = node(def);
        val["hops"] = json!(hops);
        val["possible"] = json!(possible);
        tests_section.push(format!("  {} · {poss}{hop_text}", label(def)), val);
    }
    for (file, hit) in &aggregate_test_files {
        let note = hit
            .note
            .as_deref()
            .map(|w| format!(" ({w})"))
            .unwrap_or_default();
        let poss = if hit.possible { "possible · " } else { "" };
        let hop_text = if hit.hops == 0 {
            "modified test".to_owned()
        } else {
            plural(hit.hops, "hop")
        };
        let mut val = json!({
            "file": file,
            "kind": "test file",
            "at": hit.at.to_string(),
            "hops": hit.hops,
            "possible": hit.possible,
        });
        if let Some(w) = &hit.note {
            val["note"] = json!(w);
        }
        tests_section.push(
            format!(
                "  {} · test file at {}{note} · {poss}{hop_text}",
                display_path(file),
                hit.at
            ),
            val,
        );
    }

    let commands_section = (!run_commands.is_empty()).then(|| {
        let title = if omitted_filters > 0 {
            format!("Run · first {FILTERS} tests per command, {omitted_filters} more not included")
        } else {
            "Run".into()
        };
        let mut sec = Section::new("commands", title, 2);
        for cmd in &run_commands {
            sec.push(format!("  {cmd}"), json!(cmd));
        }
        sec
    });

    // 5. Compact Semantic Diff Summary
    let mut diff_section = Section::new(
        "diff_summary",
        format!("Diff summary · {}", plural(file_diffs.len(), "file")),
        3,
    );
    // Order: source files first, test files, other, generated/lockfiles last.
    let mut ordered_diffs: Vec<&FileDiff> = file_diffs.iter().collect();
    ordered_diffs.sort_by_key(|d| {
        let cat = classify_file(index, &d.path, d.added + d.deleted);
        let fold_rank = usize::from(d.is_pure_rename() || d.is_whitespace_only());
        (
            cat,
            fold_rank,
            Reverse(d.semantic_added + d.semantic_deleted),
            &d.path,
        )
    });
    for diff in ordered_diffs {
        let cat = classify_file(index, &diff.path, diff.added + diff.deleted);
        let shown_path = display_path(&diff.path);
        let (line, val) = if diff.is_pure_rename() {
            let old = diff.old_path.as_deref().unwrap_or("");
            (
                format!("  {} → {shown_path} · pure rename", display_path(old)),
                json!({
                    "file": diff.path,
                    "old_file": old,
                    "category": cat.as_str(),
                    "status": "pure_rename",
                    "folded": true,
                }),
            )
        } else if cat == FileCategory::Generated {
            let kind = if is_lockfile_path(&diff.path) {
                "lockfile"
            } else {
                "generated"
            };
            (
                format!(
                    "  {shown_path} · {kind} folded ({})",
                    format_delta(diff.added, diff.deleted)
                ),
                json!({
                    "file": diff.path,
                    "category": cat.as_str(),
                    "status": format!("{kind}_folded"),
                    "added": diff.added,
                    "deleted": diff.deleted,
                    "folded": true,
                }),
            )
        } else if diff.is_whitespace_only() {
            (
                format!(
                    "  {shown_path} · whitespace/indent only ({}, {} folded)",
                    format_delta(diff.added, diff.deleted),
                    plural(diff.whitespace_hunks, "hunk"),
                ),
                json!({
                    "file": diff.path,
                    "category": cat.as_str(),
                    "status": "whitespace_only",
                    "added": diff.added,
                    "deleted": diff.deleted,
                    "whitespace_hunks": diff.whitespace_hunks,
                    "folded": true,
                }),
            )
        } else if diff.binary {
            (
                format!("  {shown_path} · binary"),
                json!({
                    "file": diff.path,
                    "category": cat.as_str(),
                    "status": "binary",
                }),
            )
        } else if diff.deleted_file {
            (
                format!("  {shown_path} · deleted (-{})", diff.deleted),
                json!({
                    "file": diff.path,
                    "category": cat.as_str(),
                    "status": "deleted",
                    "added": 0,
                    "deleted": diff.deleted,
                }),
            )
        } else if diff.hunks.is_empty() && diff.mode_change {
            (
                format!("  {shown_path} · mode change"),
                json!({
                    "file": diff.path,
                    "category": cat.as_str(),
                    "status": "mode_change",
                }),
            )
        } else {
            let ranges = diff.semantic_ranges();
            let ptr = if ranges.is_empty() {
                diff.hunks
                    .iter()
                    .find(|h| !h.whitespace_only)
                    .map(|h| format!("L{}", h.deleted_after.max(1)))
                    .unwrap_or_default()
            } else {
                format_line_ranges(&ranges)
            };
            let hint = diff
                .hunks
                .iter()
                .filter(|h| !h.whitespace_only)
                .find_map(|h| h.context_hint.as_deref())
                .map(|h| format!(" ({})", short(h, 40)))
                .unwrap_or_default();
            let ws_note = if diff.whitespace_hunks > 0 {
                format!(", {} ws folded", plural(diff.whitespace_hunks, "hunk"))
            } else {
                String::new()
            };
            let (prefix, status) = if let Some(old) = &diff.old_path {
                (
                    format!("{} → {shown_path} · renamed", display_path(old)),
                    "renamed",
                )
            } else if diff.new_file {
                (
                    format!(
                        "{shown_path} · {}",
                        if diff.untracked { "untracked" } else { "added" }
                    ),
                    "added",
                )
            } else {
                (shown_path.into_owned(), "modified")
            };
            let loc_part = if ptr.is_empty() {
                String::new()
            } else {
                format!(" · {ptr}{hint}")
            };
            let line = format!(
                "  {prefix} ({}){loc_part}{ws_note}",
                format_delta(diff.semantic_added, diff.semantic_deleted),
            );
            let mut val = json!({
                "file": diff.path,
                "category": cat.as_str(),
                "status": status,
                "added": diff.added,
                "deleted": diff.deleted,
                "semantic_added": diff.semantic_added,
                "semantic_deleted": diff.semantic_deleted,
                "whitespace_hunks_folded": diff.whitespace_hunks,
                "ranges": ranges.iter().map(|&(s, e)| json!([s, e])).collect::<Vec<_>>(),
            });
            if let Some(old) = &diff.old_path {
                val["old_file"] = json!(old);
            }
            (line, val)
        };
        diff_section.push(line, val);
    }

    // Assemble sections & priority order (Coverage gaps > Changed symbols > Blast radius > Run/Tests > Diff summary).
    let mut sections = Vec::<Section>::new();
    let idx_changed = sections.len();
    sections.push(changed_section);
    let idx_blast = sections.len();
    sections.push(blast_section);
    let idx_untested = untested_section.take().map(|sec| {
        let i = sections.len();
        sections.push(sec);
        i
    });
    let idx_tests = sections.len();
    sections.push(tests_section);
    let idx_commands = commands_section.map(|sec| {
        let i = sections.len();
        sections.push(sec);
        i
    });
    let idx_diff = sections.len();
    sections.push(diff_section);

    let mut order = Vec::with_capacity(sections.len());
    if let Some(i) = idx_untested {
        order.push(i);
    }
    order.push(idx_changed);
    order.push(idx_blast);
    if let Some(i) = idx_commands {
        order.push(i);
    }
    order.push(idx_tests);
    order.push(idx_diff);

    // 6. Notes & Footer
    let mut footer = Vec::<String>::new();
    if blast_walk.beyond > 0 {
        footer.push(format!(
            "Depth limit: {} past {}. Raise --depth (up to {MAX_DEPTH}) to follow them.",
            plural(blast_walk.beyond, "more dependent"),
            plural(depth, "hop")
        ));
    }
    if !blast_walk.fanned.is_empty() {
        footer.push(format!(
            "Fan-out cap applied to {}.",
            plural(blast_walk.fanned.len(), "symbol")
        ));
    }
    extra_notes.sort();
    extra_notes.dedup();
    footer.extend(extra_notes.clone());

    let next_line = format!(
        "Next: {} expand FILE NAME, map --explain NAME or test --affected",
        workspace.invocation().files
    );
    footer.push(next_line.clone());

    let shown = fit_sections(&header, &sections, &footer, &order, budget);

    if as_json {
        let (scope, base) = scope_json(since);
        let mut object = Map::new();
        object.insert("scope".into(), json!(scope));
        if let Some(base) = base {
            object.insert("base".into(), base);
        }
        object.insert("depth".into(), json!(depth));
        object.insert("risk".into(), json!(risk.as_str()));
        object.insert("risk_reasons".into(), json!(risk_reasons));
        object.insert(
            "files".into(),
            json!({
                "total": file_diffs.len(),
                "source": source_files,
                "test": test_files,
                "generated": generated_files,
                "other": other_files,
            }),
        );
        object.insert(
            "lines".into(),
            json!({
                "added": total_added,
                "deleted": total_deleted,
                "semantic_added": semantic_added,
                "semantic_deleted": semantic_deleted,
            }),
        );
        object.insert(
            "blast_radius_summary".into(),
            json!({
                "total": blast_items.len(),
                "by_hop": by_hop_counts.iter().map(|(h, c)| (h.to_string(), json!(c))).collect::<Map<_, _>>(),
                "beyond_depth": blast_walk.beyond,
                "hubs_touched": hubs_touched,
                "bridges_touched": bridges_touched,
            }),
        );
        for key in [
            "changed_symbols",
            "blast_radius",
            "untested_symbols",
            "tests",
            "commands",
            "diff_summary",
        ] {
            object.insert(key.into(), json!([]));
        }
        let mut omitted = Map::new();
        for (section, &count) in sections.iter().zip(&shown) {
            if let Some(Value::Array(arr)) = object.get_mut(section.key) {
                arr.extend_from_slice(&section.values[..count]);
            }
            let total = section.lines.len() + section.unlisted;
            let missing = total.saturating_sub(count);
            if missing > 0 {
                let prev = omitted
                    .get(section.key)
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                omitted.insert(section.key.into(), json!(prev + missing as u64));
            }
        }
        let capped = blast_walk.full || !blast_walk.fanned.is_empty() || !omitted.is_empty();
        object.insert("notes".into(), json!(extra_notes));
        object.insert("next".into(), json!(next_line));
        object.insert("omitted".into(), Value::Object(omitted));
        object.insert("capped".into(), Value::Bool(capped));
        return Ok(format!("{}\n", serde_json::to_string(&object)?));
    }

    let mut report = Report::new(budget);
    render_sections(&header, &sections, &footer, &shown, &mut |line| {
        report.line(line);
    });
    Ok(report.into_string())
}

// ---------------------------------------------------------------------------
// Budgeted section fitting (mirrors `map::fit` / `map::render`)

fn render_sections(
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
        let total = section.lines.len() + section.unlisted;
        if count == 0 && total > 0 {
            let heading = section.title.split(" · ").next().unwrap_or_default();
            hidden.push(format!("{heading} ({total})"));
            continue;
        }
        sink(&section.title);
        for line in &section.lines[..count] {
            sink(line);
        }
        if count < total {
            sink(&format!("  … +{} more", total - count));
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

fn fit_sections(
    header: &[String],
    sections: &[Section],
    footer: &[String],
    order: &[usize],
    budget: usize,
) -> Vec<usize> {
    let cap = budget.saturating_sub(RESERVE);
    let grow = |section: usize, shown: &mut Vec<usize>| {
        if shown[section] >= sections[section].lines.len() {
            return false;
        }
        shown[section] += 1;
        let mut size = 0usize;
        render_sections(header, sections, footer, shown, &mut |line| {
            size += line.len() + 1;
        });
        if size > cap {
            shown[section] -= 1;
        }
        size <= cap
    };
    let mut shown = vec![0usize; sections.len()];
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
    rounds(&|_| usize::MAX, &mut shown);
    shown
}

// ---------------------------------------------------------------------------
// Unit tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as ProcessCommand;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let out = ProcessCommand::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Tester")
            .env("GIT_AUTHOR_EMAIL", "tester@example.com")
            .env("GIT_COMMITTER_NAME", "Tester")
            .env("GIT_COMMITTER_EMAIL", "tester@example.com")
            .output()
            .expect("git should run");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn write_files(root: &Path, files: &[(&str, &str)]) {
        for (rel, body) in files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, body).unwrap();
        }
    }

    fn init_polyglot_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        git(root, &["init", "-b", "main"]);
        git(root, &["config", "user.name", "Tester"]);
        git(root, &["config", "user.email", "tester@example.com"]);

        write_files(
            root,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"shop\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                ),
                (
                    "Cargo.lock",
                    "# This file is automatically @generated by Cargo.\nversion = 3\n\n[[package]]\nname = \"shop\"\nversion = \"0.1.0\"\n",
                ),
                (
                    "src/lib.rs",
                    "pub mod pricing;\npub mod checkout;\npub mod api;\npub mod formatting;\npub mod moved;\n",
                ),
                (
                    "src/pricing.rs",
                    concat!(
                        "pub fn compute_tax(cents: i64) -> i64 {\n",
                        "    cents * 10 / 100\n",
                        "}\n\n",
                        "pub fn apply_discount(cents: i64, pct: i64) -> i64 {\n",
                        "    cents - (cents * pct / 100)\n",
                        "}\n\n",
                        "#[cfg(test)]\n",
                        "mod tests {\n",
                        "    use super::*;\n",
                        "    #[test]\n",
                        "    fn test_compute_tax() {\n",
                        "        assert_eq!(compute_tax(100), 10);\n",
                        "    }\n",
                        "}\n",
                    ),
                ),
                (
                    "src/checkout.rs",
                    concat!(
                        "use crate::pricing::compute_tax;\n\n",
                        "pub fn checkout_total(subtotal: i64) -> i64 {\n",
                        "    subtotal + compute_tax(subtotal)\n",
                        "}\n",
                    ),
                ),
                (
                    "src/api.rs",
                    concat!(
                        "use crate::checkout::checkout_total;\n\n",
                        "pub fn handle_checkout(subtotal: i64) -> i64 {\n",
                        "    checkout_total(subtotal)\n",
                        "}\n",
                    ),
                ),
                (
                    "src/formatting.rs",
                    "pub fn fmt_cents(c: i64) -> String {\n    format!(\"{c}\")\n}\n",
                ),
                (
                    "src/moved.rs",
                    "pub fn stable_helper(x: i64) -> i64 {\n    x + 1\n}\n",
                ),
                (
                    "frontend/orders.ts",
                    concat!(
                        "export async function fetchOrders() {\n",
                        "  const res = await fetch('/api/orders');\n",
                        "  return res.json();\n",
                        "}\n",
                    ),
                ),
                (
                    "backend/orders.py",
                    concat!(
                        "from fastapi import APIRouter\n",
                        "router = APIRouter()\n\n",
                        "@router.get('/api/orders')\n",
                        "def list_orders():\n",
                        "    return db.execute('SELECT id FROM purchase_orders')\n",
                    ),
                ),
                (
                    "db/schema.sql",
                    "CREATE TABLE purchase_orders (id INTEGER PRIMARY KEY);\n",
                ),
                ("tests/__snapshots__/ui.snap", "line 1\nline 2\nline 3\n"),
            ],
        );
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "initial"]);
        dir
    }

    #[test]
    fn review_reports_blast_radius_untested_gap_cross_lang_and_folded_noise() {
        let dir = init_polyglot_repo();
        let root = dir.path();

        // 1. Modify tested function `compute_tax` (has 1-hop caller `checkout_total`,
        //    2-hop caller `handle_checkout`, and test `test_compute_tax`), AND
        //    modify untested function `apply_discount` (0 reachable tests!).
        fs::write(
            root.join("src/pricing.rs"),
            concat!(
                "pub fn compute_tax(cents: i64) -> i64 {\n",
                "    // updated tax rate\n",
                "    cents * 12 / 100\n",
                "}\n\n",
                "pub fn apply_discount(cents: i64, pct: i64) -> i64 {\n",
                "    let clamped = pct.clamp(0, 100);\n",
                "    cents - (cents * clamped / 100)\n",
                "}\n\n",
                "#[cfg(test)]\n",
                "mod tests {\n",
                "    use super::*;\n",
                "    #[test]\n",
                "    fn test_compute_tax() {\n",
                "        assert_eq!(compute_tax(100), 12);\n",
                "    }\n",
                "}\n",
            ),
        )
        .unwrap();

        // 2. Modify cross-language Python HTTP route handler `list_orders`
        //    (called by TS `fetchOrders` via possible HTTP route!).
        fs::write(
            root.join("backend/orders.py"),
            concat!(
                "from fastapi import APIRouter\n",
                "router = APIRouter()\n\n",
                "@router.get('/api/orders')\n",
                "def list_orders():\n",
                "    # include status\n",
                "    return db.execute('SELECT id, status FROM purchase_orders')\n",
            ),
        )
        .unwrap();

        // 3. Pure whitespace edit in `src/formatting.rs` — must be skipped from
        //    Changed symbols and folded in Diff summary!
        fs::write(
            root.join("src/formatting.rs"),
            "pub fn fmt_cents(c: i64) -> String {\n        format!(\"{c}\")\n}\n\n",
        )
        .unwrap();

        // 4. Lockfile + generated snapshot edits — must be folded & skipped from symbols.
        fs::write(
            root.join("Cargo.lock"),
            "# This file is automatically @generated by Cargo.\nversion = 3\n\n[[package]]\nname = \"shop\"\nversion = \"0.2.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("tests/__snapshots__/ui.snap"),
            "line 1\nline 2 updated\nline 3\nline 4\n",
        )
        .unwrap();

        // 5. Pure rename `src/moved.rs` -> `src/relocated.rs`.
        git(root, &["mv", "src/moved.rs", "src/relocated.rs"]);

        let ws = Workspace::discover(Some(root)).unwrap();
        let index = Index::open(&ws).unwrap();
        let since = Since::resolve(&ws, false, None).unwrap();

        let text = render_review(&ws, &index, &since, 2, 4096, false).unwrap();
        // Determinism check.
        assert_eq!(
            text,
            render_review(&ws, &index, &since, 2, 4096, false).unwrap()
        );
        assert!(text.len() <= 4096, "exceeded 4096 bytes:\n{text}");

        // Header checks.
        assert!(text.contains("Review [HEAD]"), "{text}");
        assert!(text.contains("source"), "{text}");
        assert!(text.contains("generated"), "{text}");
        assert!(
            text.contains("risk: medium") || text.contains("risk: high"),
            "{text}"
        );

        // Changed symbols: semantic edits present, whitespace/generated/rename absent.
        assert!(text.contains("compute_tax"), "{text}");
        assert!(text.contains("apply_discount"), "{text}");
        assert!(text.contains("list_orders"), "{text}");
        assert!(text.contains("bridge:"), "{text}");
        assert!(!text.contains("fmt_cents [function]"), "{text}");
        assert!(!text.contains("stable_helper [function]"), "{text}");

        // Blast radius: 1-hop & 2-hop Rust callers + cross-language possible HTTP route
        // (without redundant file-scope import lines when the file's caller is shown).
        assert!(
            text.contains("[1-hop]") && text.contains("checkout_total"),
            "{text}"
        );
        assert!(!text.contains("file scope src/checkout.rs"), "{text}");
        assert!(
            text.contains("[2-hop]") && text.contains("handle_checkout"),
            "{text}"
        );
        assert!(
            text.contains("fetchOrders") && text.contains("possible HTTP route"),
            "{text}"
        );

        // Coverage gap: `apply_discount` explicitly flagged as 0 reachable tests;
        // `compute_tax` is covered by `test_compute_tax` and NOT in untested!
        assert!(text.contains("Coverage gaps"), "{text}");
        assert!(text.contains("! pricing::apply_discount"), "{text}");
        assert!(!text.contains("! pricing::compute_tax"), "{text}");
        assert!(text.contains("test_compute_tax"), "{text}");
        assert!(text.contains("cargo test"), "{text}");

        // Diff summary folds lockfile, snapshot, whitespace, and pure rename cleanly.
        assert!(text.contains("Cargo.lock · lockfile folded"), "{text}");
        assert!(text.contains("ui.snap · generated folded"), "{text}");
        assert!(
            text.contains("formatting.rs · whitespace/indent only"),
            "{text}"
        );
        assert!(
            text.contains("src/moved.rs → src/relocated.rs · pure rename"),
            "{text}"
        );

        // Footer contract.
        assert!(
            text.contains("Next: justless ")
                && text.contains("expand FILE NAME, map --explain NAME or test --affected"),
            "{text}"
        );

        // JSON dossier check.
        let json_str = render_review(&ws, &index, &since, 2, 4096, true).unwrap();
        let dossier: Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(dossier["scope"], "HEAD");
        assert!(dossier["files"]["generated"].as_u64().unwrap() >= 2);
        let untested_names: Vec<&str> = dossier["untested_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert!(
            untested_names.iter().any(|n| n.ends_with("apply_discount")),
            "{untested_names:?}"
        );
        assert!(
            !untested_names.iter().any(|n| n.ends_with("compute_tax")),
            "{untested_names:?}"
        );
        let blast = dossier["blast_radius"].as_array().unwrap();
        assert!(
            blast.iter().any(|b| b["hop"] == 1
                && b["dependent"]["name"]
                    .as_str()
                    .is_some_and(|n| n.ends_with("checkout_total"))),
            "{blast:#?}"
        );
        assert!(
            blast.iter().any(|b| b["hop"] == 2
                && b["dependent"]["name"]
                    .as_str()
                    .is_some_and(|n| n.ends_with("handle_checkout"))),
            "{blast:#?}"
        );
        assert!(
            blast.iter().any(|b| b["possible"] == true
                && b["relationship"] == "HTTP route"
                && b["dependent"]["name"] == "fetchOrders"),
            "{blast:#?}"
        );
        assert!(
            dossier["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c.as_str().unwrap().contains("cargo test")),
            "{dossier:#}"
        );

        // Strict `--max-bytes` / `--budget` enforcement across tight ceilings.
        for cap in [256usize, 512, 900] {
            let bounded = render_review(&ws, &index, &since, 2, cap, false).unwrap();
            assert!(
                bounded.len() <= cap,
                "budget {cap} exceeded: {} bytes\n{bounded}",
                bounded.len()
            );
        }
    }

    #[test]
    fn review_base_ref_and_clean_tree_handling() {
        let dir = init_polyglot_repo();
        let root = dir.path();

        // Clean tree on main: reports nothing changed + valid JSON dossier.
        let ws = Workspace::discover(Some(root)).unwrap();
        let index = Index::open(&ws).unwrap();
        let head_since = Since::resolve(&ws, false, None).unwrap();
        let clean_txt = render_review(&ws, &index, &head_since, 2, 4096, false).unwrap();
        assert!(clean_txt.contains("No changed files"), "{clean_txt}");
        let clean_json: Value =
            serde_json::from_str(&render_review(&ws, &index, &head_since, 2, 4096, true).unwrap())
                .unwrap();
        assert_eq!(clean_json["risk"], "low");
        assert_eq!(clean_json["files"]["total"], 0);

        // Commit a change on a feature branch, keeping working tree completely clean!
        git(root, &["checkout", "-b", "feat/tax-update"]);
        fs::write(
            root.join("src/pricing.rs"),
            concat!(
                "pub fn compute_tax(cents: i64) -> i64 {\n",
                "    cents * 15 / 100\n",
                "}\n\n",
                "pub fn apply_discount(cents: i64, pct: i64) -> i64 {\n",
                "    cents - (cents * pct / 100)\n",
                "}\n\n",
                "#[cfg(test)]\n",
                "mod tests {\n",
                "    use super::*;\n",
                "    #[test]\n",
                "    fn test_compute_tax() {\n",
                "        assert_eq!(compute_tax(100), 15);\n",
                "    }\n",
                "}\n",
            ),
        )
        .unwrap();
        git(root, &["commit", "-am", "bump tax rate"]);

        let index = Index::open(&ws).unwrap();
        let base_since = Since::resolve(&ws, false, Some("main")).unwrap();
        let branch_txt = render_review(&ws, &index, &base_since, 2, 4096, false).unwrap();
        assert!(branch_txt.contains("Review [base main @"), "{branch_txt}");
        assert!(branch_txt.contains("compute_tax"), "{branch_txt}");
        assert!(branch_txt.contains("checkout_total"), "{branch_txt}");
        assert!(branch_txt.contains("handle_checkout"), "{branch_txt}");
        assert!(branch_txt.contains("test_compute_tax"), "{branch_txt}");
        // Only `compute_tax` (which IS tested) changed -> no coverage gap!
        assert!(!branch_txt.contains("Coverage gaps"), "{branch_txt}");

        let branch_json: Value =
            serde_json::from_str(&render_review(&ws, &index, &base_since, 2, 4096, true).unwrap())
                .unwrap();
        assert_eq!(branch_json["scope"], "base:main");
        assert_eq!(branch_json["base"]["ref"], "main");
        assert_eq!(branch_json["untested_symbols"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn review_cli_validates_depth_and_max_bytes_alias() {
        let cmd = command();
        let m = cmd
            .clone()
            .try_get_matches_from([
                "review",
                "--base",
                "main",
                "--depth",
                "3",
                "--max-bytes",
                "1024",
                "--json",
            ])
            .unwrap();
        assert_eq!(m.value_of("base"), Some("main"));
        assert_eq!(m.value_of("depth"), Some("3"));
        assert_eq!(m.value_of("budget"), Some("1024"));
        assert!(m.is_present("json"));

        // `--staged` and `--base` conflict.
        assert!(
            cmd.clone()
                .try_get_matches_from(["review", "--base", "main", "--staged"])
                .is_err()
        );
    }

    #[test]
    fn review_flags_hub_high_risk_deleted_external_refs_and_python_indent() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        git(root, &["init", "-b", "main"]);
        git(root, &["config", "user.name", "Tester"]);
        git(root, &["config", "user.email", "tester@example.com"]);

        write_files(
            root,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"hubapp\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                ),
                (
                    "src/lib.rs",
                    "pub mod core;\npub mod billing;\npub mod shipping;\n",
                ),
                (
                    "src/core/mod.rs",
                    concat!(
                        "pub fn normalize(v: i64) -> i64 {\n",
                        "    v.max(0)\n",
                        "}\n\n",
                        "pub fn legacy_calc(v: i64) -> i64 {\n",
                        "    v + 1\n",
                        "}\n",
                    ),
                ),
                (
                    "src/billing/mod.rs",
                    concat!(
                        "use crate::core::{normalize, legacy_calc};\n",
                        "pub fn bill(v: i64) -> i64 { normalize(v) + legacy_calc(v) }\n",
                    ),
                ),
                (
                    "src/shipping/mod.rs",
                    concat!(
                        "use crate::core::normalize;\n",
                        "pub fn ship(v: i64) -> i64 { normalize(v) * 2 }\n",
                    ),
                ),
                (
                    "scripts/worker.py",
                    concat!(
                        "def run_job(flag):\n",
                        "    if flag:\n",
                        "        return 1\n",
                        "    return 2\n",
                    ),
                ),
            ],
        );
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "init"]);

        // Modify multi-area hub `normalize` (0 tests!) AND delete `legacy_calc` (still called by `bill`!)
        fs::write(
            root.join("src/core/mod.rs"),
            "pub fn normalize(v: i64) -> i64 {\n    v.clamp(0, 1000)\n}\n",
        )
        .unwrap();
        // Change Python indentation — indent-significant, MUST NOT fold as whitespace-only!
        fs::write(
            root.join("scripts/worker.py"),
            concat!(
                "def run_job(flag):\n",
                "    if flag:\n",
                "        return 1\n",
                "        return 2\n",
            ),
        )
        .unwrap();

        let ws = Workspace::discover(Some(root)).unwrap();
        let index = Index::open(&ws).unwrap();
        let since = Since::resolve(&ws, false, None).unwrap();

        let text = render_review(&ws, &index, &since, 2, 4096, false).unwrap();
        assert!(text.contains("risk: high"), "{text}");
        assert!(text.contains("[hub"), "{text}");
        assert!(text.contains("legacy_calc"), "{text}");
        assert!(text.contains("deleted from src/core/mod.rs"), "{text}");
        assert!(text.contains("! core::normalize"), "{text}");
        assert!(text.contains("run_job"), "{text}");
        assert!(!text.contains("[file]"), "{text}");
        assert!(
            !text.contains("worker.py · whitespace/indent only"),
            "{text}"
        );
    }
}
