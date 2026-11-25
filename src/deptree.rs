//! Zero-semantic-loss dependency tree and graph deduplicator.
//!
//! Compacts verbose dependency listings from `cargo tree` (forward, `-i`
//! inverted, and `-d` duplicates), `npm ls`, `pnpm list`/`ls`, `yarn list`,
//! `pipdeptree`, `go mod graph`, `mvn dependency:tree`, and `gradle
//! dependencies`.
//!
//! Contracts preserved:
//! - Fail closed (return `None`) on JSON, custom tables, machine-oriented
//!   flags, unrecognized output, or when the compacted report is not strictly
//!   smaller than `stdout` within `budget`.
//! - Preserve 100% of error, conflict, unmet, missing, extraneous, and invalid
//!   problem lines at the top with exact `[stdout:line]` pointers.
//! - Detect and highlight every multi-version duplicate package with all
//!   versions and first-seen `[stdout:line]` pointers.
//! - Attach exact `[stdout:A-B]` line ranges to every collapsed subtree,
//!   reverse path group, or graph hub so the caller can expand verbatim with
//!   `justless log`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::Path;

/// Maximum lines allowed in a single input before refusing to allocate unbounded
/// graph state.
const MAX_LINES: usize = 250_000;

/// Entry point: compact dependency tree or graph `stdout` for `tool` + `args`
/// within `budget` UTF-8 bytes. Returns `None` if the invocation/output is not
/// a supported dependency tree/graph or cannot be made strictly smaller than
/// `stdout` within `budget`.
#[allow(dead_code)]
pub fn format(tool: &str, args: &[String], stdout: &str, budget: usize) -> Option<String> {
    if budget == 0 || stdout.is_empty() {
        return None;
    }
    let trimmed = stdout.trim();
    if trimmed.is_empty() || looks_like_json(trimmed) {
        return None;
    }
    let kind = detect_tool(tool, args)?;
    let raw_len = stdout.len();

    let report = match kind {
        ToolKind::GoModGraph => {
            let graph = parse_go_mod_graph(stdout)?;
            render_go_graph(&graph, raw_len, budget)?
        }
        ToolKind::CargoTree {
            inverted,
            duplicates_mode,
        } => {
            let tree = parse_cargo_tree(stdout, inverted, duplicates_mode)?;
            if inverted {
                render_inverted_tree(&tree, raw_len, budget)?
            } else {
                render_forward_tree(&tree, raw_len, budget)?
            }
        }
        ToolKind::NpmLs | ToolKind::PnpmList | ToolKind::YarnList => {
            let tree = parse_js_tree(stdout, kind)?;
            render_forward_tree(&tree, raw_len, budget)?
        }
        ToolKind::Pipdeptree { reverse } => {
            let tree = parse_pipdeptree(stdout, reverse)?;
            if reverse {
                render_inverted_tree(&tree, raw_len, budget)?
            } else {
                render_forward_tree(&tree, raw_len, budget)?
            }
        }
        ToolKind::MvnDependencyTree => {
            let tree = parse_mvn_tree(stdout)?;
            render_forward_tree(&tree, raw_len, budget)?
        }
        ToolKind::GradleDependencies => {
            let tree = parse_gradle_tree(stdout)?;
            render_forward_tree(&tree, raw_len, budget)?
        }
    };

    fit_smaller(report, raw_len, budget)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolKind {
    CargoTree {
        inverted: bool,
        duplicates_mode: bool,
    },
    NpmLs,
    PnpmList,
    YarnList,
    Pipdeptree {
        reverse: bool,
    },
    GoModGraph,
    MvnDependencyTree,
    GradleDependencies,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisplayStyle {
    Cargo,
    Js,
    Pip,
    Maven,
    Gradle,
}

#[derive(Clone, Debug)]
struct TreeNode {
    name: String,
    version: String,
    requested_version: Option<String>,
    section: Option<String>,
    deduped: bool,
    proc_macro: bool,
    constraint: bool,
    status_tag: Option<String>,
    workspace_path: Option<String>,
    line: usize,
    subtree_last_line: usize,
    children: Vec<usize>,
}

#[derive(Clone, Debug)]
struct ProblemLine {
    text: String,
    line: usize,
}

#[derive(Clone, Debug)]
struct ParsedTree {
    tool_label: String,
    style: DisplayStyle,
    roots: Vec<usize>,
    nodes: Vec<TreeNode>,
    problems: Vec<ProblemLine>,
    config_roots: bool,
    package_forest: bool,
    project_title: Option<String>,
    empty_configs: usize,
    tree_lines: usize,
    max_depth: usize,
}

#[derive(Clone, Debug)]
struct GoEdge {
    parent_name: String,
    parent_ver: Option<String>,
    child_name: String,
    child_ver: Option<String>,
    line: usize,
}

#[derive(Clone, Debug)]
struct ParsedGoGraph {
    roots: Vec<String>,
    edges: Vec<GoEdge>,
    problems: Vec<ProblemLine>,
}

// ---------------------------------------------------------------------------
// Tool & flag detection
// ---------------------------------------------------------------------------

fn detect_tool(tool: &str, args: &[String]) -> Option<ToolKind> {
    let mut words: Vec<&str> = tool.split_whitespace().collect();
    words.extend(args.iter().map(String::as_str));
    let (&raw_prog, mut rest) = words.split_first()?;
    let mut prog = normalize_program(raw_prog)?;

    // Unwrap `python -m pipdeptree` / `python3 -m pipdeptree`.
    if matches!(prog.as_str(), "python" | "python3")
        && let ["-m", "pipdeptree", tail @ ..] = rest
    {
        prog = "pipdeptree".to_owned();
        rest = tail;
    }

    match prog.as_str() {
        "cargo" => detect_cargo_tree(rest),
        "npm" => detect_npm_ls(rest),
        "pnpm" => detect_pnpm_list(rest),
        "yarn" => detect_yarn_list(rest),
        "pipdeptree" => detect_pipdeptree(rest),
        "go" => detect_go_mod_graph(rest),
        "mvn" | "mvnw" | "mvnd" => detect_mvn_tree(rest),
        "gradle" | "gradlew" => detect_gradle_deps(rest),
        _ => None,
    }
}

fn normalize_program(raw: &str) -> Option<String> {
    let base = Path::new(raw)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(raw);
    let base = base.rsplit('\\').next().unwrap_or(base);
    let lower = base.to_ascii_lowercase();
    let stripped = lower
        .strip_suffix(".exe")
        .or_else(|| lower.strip_suffix(".cmd"))
        .or_else(|| lower.strip_suffix(".bat"))
        .unwrap_or(&lower);
    (!stripped.is_empty()).then(|| stripped.to_owned())
}

fn detect_cargo_tree(mut rest: &[&str]) -> Option<ToolKind> {
    while let Some((&arg, tail)) = rest.split_first() {
        if arg == "tree" {
            rest = tail;
            return parse_cargo_tree_flags(rest);
        }
        if arg.starts_with('+')
            || matches!(
                arg,
                "-q" | "--quiet" | "-v" | "--verbose" | "--offline" | "--locked" | "--frozen"
            )
            || arg.starts_with("--color=")
            || arg.starts_with("--manifest-path=")
            || arg.starts_with("--config=")
        {
            rest = tail;
        } else if matches!(
            arg,
            "--color" | "--manifest-path" | "--config" | "-Z" | "-C"
        ) && !tail.is_empty()
        {
            rest = &tail[1..];
        } else {
            return None;
        }
    }
    None
}

fn parse_cargo_tree_flags(args: &[&str]) -> Option<ToolKind> {
    let mut inverted = false;
    let mut duplicates_mode = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i];
        if arg == "--" {
            break;
        }
        if arg == "--prefix" {
            if args.get(i + 1).copied() == Some("none") {
                return None;
            }
            i += 2;
            continue;
        }
        if let Some(val) = arg.strip_prefix("--prefix=") {
            if val == "none" {
                return None;
            }
            i += 1;
            continue;
        }
        if arg == "-f" || arg == "--format" {
            if let Some(&fmt) = args.get(i + 1)
                && is_non_tree_cargo_format(fmt)
            {
                return None;
            }
            i += 2;
            continue;
        }
        if let Some(fmt) = arg.strip_prefix("--format=") {
            if is_non_tree_cargo_format(fmt) {
                return None;
            }
            i += 1;
            continue;
        }
        if arg == "--json" || arg == "--message-format=json" {
            return None;
        }
        if arg == "--invert" {
            inverted = true;
            if args.get(i + 1).is_some_and(|v| !v.starts_with('-')) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if arg.starts_with("--invert=") {
            inverted = true;
            i += 1;
            continue;
        }
        if arg == "-i" {
            inverted = true;
            if args.get(i + 1).is_some_and(|v| !v.starts_with('-')) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if arg.starts_with("-i") && !arg.starts_with("--") {
            inverted = true;
            i += 1;
            continue;
        }
        if arg == "-d" || arg == "--duplicates" {
            duplicates_mode = true;
            inverted = true;
            i += 1;
            continue;
        }
        // Combined short flags like `-di` or `-id`.
        if arg.starts_with('-')
            && !arg.starts_with("--")
            && arg.len() > 2
            && arg[1..]
                .bytes()
                .all(|b| matches!(b, b'd' | b'i' | b'q' | b'v'))
        {
            if arg.contains('d') {
                duplicates_mode = true;
                inverted = true;
            }
            if arg.contains('i') {
                inverted = true;
            }
            i += 1;
            continue;
        }
        if matches!(
            arg,
            "-p" | "--package"
                | "-e"
                | "--edges"
                | "--depth"
                | "--target"
                | "--charset"
                | "--manifest-path"
                | "--prune"
                | "-F"
                | "--features"
                | "--color"
                | "--exclude"
        ) {
            i += 2;
            continue;
        }
        i += 1;
    }
    Some(ToolKind::CargoTree {
        inverted,
        duplicates_mode,
    })
}

fn is_non_tree_cargo_format(fmt: &str) -> bool {
    fmt.contains('\t')
        || fmt.contains("\\t")
        || fmt.contains('|')
        || fmt.contains(',')
        || !fmt.contains("{p}")
}

fn detect_npm_ls(args: &[&str]) -> Option<ToolKind> {
    let (&sub, rest) = args.split_first()?;
    if !matches!(sub, "ls" | "list") {
        return None;
    }
    if has_js_machine_flag(rest) {
        return None;
    }
    Some(ToolKind::NpmLs)
}

fn detect_pnpm_list(mut args: &[&str]) -> Option<ToolKind> {
    // Allow global pnpm options before `list`/`ls` (e.g. `pnpm -r list`, `pnpm --filter app ls`).
    while let Some((&arg, tail)) = args.split_first() {
        if matches!(arg, "list" | "ls") {
            if has_js_machine_flag(tail) {
                return None;
            }
            return Some(ToolKind::PnpmList);
        }
        if matches!(arg, "-r" | "--recursive")
            || arg.starts_with("--filter=")
            || arg.starts_with("--dir=")
        {
            args = tail;
        } else if matches!(arg, "--filter" | "-F" | "--dir" | "-C") && !tail.is_empty() {
            args = &tail[1..];
        } else {
            return None;
        }
    }
    None
}

fn detect_yarn_list(args: &[&str]) -> Option<ToolKind> {
    let (&sub, rest) = args.split_first()?;
    if !matches!(sub, "list" | "ls") {
        return None;
    }
    if rest
        .iter()
        .any(|&a| matches!(a, "--json" | "-j" | "--ndjson") || a.starts_with("--json="))
    {
        return None;
    }
    Some(ToolKind::YarnList)
}

fn has_js_machine_flag(args: &[&str]) -> bool {
    args.iter().any(|&a| {
        matches!(
            a,
            "--json" | "-j" | "--parseable" | "-p" | "--long" | "-l" | "--ndjson" | "--yaml"
        ) || a.starts_with("--json=")
            || a.starts_with("--parseable=")
            || a.starts_with("--long=")
    })
}

fn detect_pipdeptree(args: &[&str]) -> Option<ToolKind> {
    let mut reverse = false;
    for &arg in args {
        if matches!(
            arg,
            "--json" | "--json-tree" | "-j" | "--graph-output" | "--mermaid" | "--freeze" | "-f"
        ) || arg.starts_with("--graph-output=")
            || arg.starts_with("--json")
        {
            return None;
        }
        if matches!(arg, "-r" | "--reverse") {
            reverse = true;
        }
    }
    Some(ToolKind::Pipdeptree { reverse })
}

fn detect_go_mod_graph(mut args: &[&str]) -> Option<ToolKind> {
    while let Some((&arg, tail)) = args.split_first() {
        if arg == "-C" && !tail.is_empty() {
            args = &tail[1..];
        } else {
            break;
        }
    }
    let ["mod", "graph", tail @ ..] = args else {
        return None;
    };
    if tail.iter().any(|&a| {
        matches!(a, "-json" | "--json") || a.starts_with("-f") || a.starts_with("--format")
    }) {
        return None;
    }
    Some(ToolKind::GoModGraph)
}

fn detect_mvn_tree(args: &[&str]) -> Option<ToolKind> {
    let mut has_goal = false;
    for &arg in args {
        if let Some(val) = arg.strip_prefix("-DoutputType=")
            && !val.eq_ignore_ascii_case("text")
        {
            return None;
        }
        if arg == "dependency:tree" || (arg.ends_with(":tree") && arg.contains("dependency")) {
            has_goal = true;
        }
    }
    has_goal.then_some(ToolKind::MvnDependencyTree)
}

fn detect_gradle_deps(args: &[&str]) -> Option<ToolKind> {
    let mut has_task = false;
    for &arg in args {
        if matches!(arg, "--json" | "-j") {
            return None;
        }
        if !arg.starts_with('-') && (arg == "dependencies" || arg.ends_with(":dependencies")) {
            has_task = true;
        }
    }
    has_task.then_some(ToolKind::GradleDependencies)
}

// ---------------------------------------------------------------------------
// Generic text helpers
// ---------------------------------------------------------------------------

fn looks_like_json(trimmed: &str) -> bool {
    if trimmed.starts_with('{') {
        return true;
    }
    if trimmed.starts_with('[')
        && !trimmed.starts_with("[INFO]")
        && !trimmed.starts_with("[WARNING]")
        && !trimmed.starts_with("[ERROR]")
        && !trimmed.starts_with("[build-dependencies]")
        && !trimmed.starts_with("[dev-dependencies]")
        && !trimmed.starts_with("[dependencies]")
        && (trimmed.starts_with("[{")
            || trimmed.starts_with("[\"")
            || trimmed.starts_with("[]")
            || serde_json::from_str::<serde_json::Value>(trimmed).is_ok())
    {
        return true;
    }
    false
}

/// Strip ANSI CSI escape sequences so colored tool output parses identically.
fn strip_ansi(line: &str) -> String {
    if !line.contains('\x1b') {
        return line.to_owned();
    }
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            continue;
        }
        let ch = line[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Parse leading tree gutter (`' '`, `'│'`, `'|'`) + branch connector
/// (`├── `, `└── `, `├─┬ `, `└─┬ `, `├─ `, `└─ `, `+--- `, `\--- `, `+-- `,
/// `|-- `, `` `-- ``, `\-- `, `+- `, `\- `, or optionally indented `- `).
/// Returns `(connector_char_col, remainder_after_connector)`.
fn split_tree_prefix(line: &str, allow_dash_bullet: bool) -> Option<(usize, &str)> {
    let mut char_col = 0usize;
    let mut byte_pos = 0usize;
    for (idx, ch) in line.char_indices() {
        if matches!(ch, ' ' | '│' | '|') {
            // Check if `|` starts an ASCII branch connector (`|-- ` or `|- `).
            if ch == '|' && ascii_branch_len(&line[idx..]).is_some() {
                byte_pos = idx;
                break;
            }
            char_col += 1;
            byte_pos = idx + ch.len_utf8();
        } else {
            byte_pos = idx;
            break;
        }
    }
    let tail = &line[byte_pos..];
    if tail.is_empty() {
        return None;
    }
    if let Some(len) = utf8_branch_len(tail) {
        return Some((char_col, &tail[len..]));
    }
    if let Some(len) = ascii_branch_len(tail) {
        return Some((char_col, &tail[len..]));
    }
    if allow_dash_bullet
        && char_col >= 2
        && let Some(rest) = tail.strip_prefix("- ")
    {
        return Some((char_col, rest));
    }
    None
}

fn utf8_branch_len(s: &str) -> Option<usize> {
    let mut chars = s.char_indices();
    let (_, first) = chars.next()?;
    if !matches!(first, '├' | '└') {
        return None;
    }
    let mut dashes = 0usize;
    let mut saw_tee = false;
    for (idx, ch) in chars {
        match ch {
            '─' if !saw_tee && dashes < 4 => dashes += 1,
            '┬' if dashes >= 1 && !saw_tee => saw_tee = true,
            ' ' if dashes >= 1 => return Some(idx + 1),
            _ => return None,
        }
    }
    None
}

fn ascii_branch_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let first = *bytes.first()?;
    if !matches!(first, b'+' | b'|' | b'`' | b'\\') {
        return None;
    }
    let mut dashes = 0usize;
    let mut i = 1;
    while i < bytes.len() && bytes[i] == b'-' && dashes < 4 {
        dashes += 1;
        i += 1;
    }
    if dashes >= 1 && bytes.get(i) == Some(&b' ') {
        Some(i + 1)
    } else {
        None
    }
}

/// Strip leading gutter characters (`' '`, `'│'`, `'|'`) and return `(gutter_char_width, remainder)`.
fn strip_gutter(line: &str) -> (usize, &str) {
    let mut char_col = 0usize;
    let mut byte_pos = 0usize;
    for (idx, ch) in line.char_indices() {
        if matches!(ch, ' ' | '│' | '|') {
            char_col += 1;
            byte_pos = idx + ch.len_utf8();
        } else {
            byte_pos = idx;
            break;
        }
    }
    (char_col, &line[byte_pos..])
}

fn stdout_ref(first: usize, last: usize) -> String {
    if first == last {
        format!("[stdout:{first}]")
    } else {
        format!("[stdout:{first}-{last}]")
    }
}

fn fit_smaller(report: String, raw_len: usize, max_bytes: usize) -> Option<String> {
    (max_bytes > 0 && report.len() < raw_len && report.len() <= max_bytes).then_some(report)
}

/// Natural version comparison (`v0.9.0` < `v0.10.0` < `v1.0.109` < `v2.0.87`).
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let a_s = a
        .strip_prefix('v')
        .or_else(|| a.strip_prefix('V'))
        .unwrap_or(a);
    let b_s = b
        .strip_prefix('v')
        .or_else(|| b.strip_prefix('V'))
        .unwrap_or(b);
    let mut ai = version_chunks(a_s);
    let mut bi = version_chunks(b_s);
    loop {
        match (ai.next(), bi.next()) {
            (Some(ac), Some(bc)) => {
                let ord = match (ac, bc) {
                    (VerChunk::Num(an), VerChunk::Num(bn)) => an.cmp(&bn),
                    (VerChunk::Num(_), VerChunk::Text(_)) => std::cmp::Ordering::Greater,
                    (VerChunk::Text(_), VerChunk::Num(_)) => std::cmp::Ordering::Less,
                    (VerChunk::Text(at), VerChunk::Text(bt)) => at.cmp(bt),
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (None, None) => return a.cmp(b),
        }
    }
}

enum VerChunk<'a> {
    Num(u64),
    Text(&'a str),
}

fn version_chunks(s: &str) -> impl Iterator<Item = VerChunk<'_>> {
    let mut rest = s;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let first_digit = rest.as_bytes()[0].is_ascii_digit();
        let len = rest
            .bytes()
            .take_while(|b| b.is_ascii_digit() == first_digit)
            .count();
        let (head, tail) = rest.split_at(len);
        rest = tail;
        if first_digit {
            match head.parse::<u64>() {
                Ok(n) => Some(VerChunk::Num(n)),
                Err(_) => Some(VerChunk::Text(head)),
            }
        } else {
            Some(VerChunk::Text(head))
        }
    })
}

fn push_node(
    nodes: &mut Vec<TreeNode>,
    roots: &mut Vec<usize>,
    stack: &mut Vec<(isize, usize)>,
    active_sections: &mut HashMap<usize, String>,
    col: isize,
    mut node: TreeNode,
    max_depth: &mut usize,
) -> usize {
    while let Some(&(top_col, top_idx)) = stack.last() {
        if top_col >= col {
            stack.pop();
            active_sections.remove(&top_idx);
        } else {
            break;
        }
    }
    let idx = nodes.len();
    if let Some(&(_, parent_idx)) = stack.last() {
        if node.section.is_none() {
            node.section = active_sections.get(&parent_idx).cloned();
        }
        let line = node.line;
        nodes.push(node);
        nodes[parent_idx].children.push(idx);
        for &(_, anc) in stack.iter() {
            if line > nodes[anc].subtree_last_line {
                nodes[anc].subtree_last_line = line;
            }
        }
    } else {
        nodes.push(node);
        roots.push(idx);
    }
    stack.push((col, idx));
    let depth = stack.len().saturating_sub(1);
    if depth > *max_depth {
        *max_depth = depth;
    }
    idx
}

fn update_ancestors_last_line(nodes: &mut [TreeNode], stack: &[(isize, usize)], line: usize) {
    for &(_, anc) in stack {
        if line > nodes[anc].subtree_last_line {
            nodes[anc].subtree_last_line = line;
        }
    }
}

// ---------------------------------------------------------------------------
// Parser 1: cargo tree
// ---------------------------------------------------------------------------

fn parse_cargo_tree(stdout: &str, inverted: bool, duplicates_mode: bool) -> Option<ParsedTree> {
    let mut nodes: Vec<TreeNode> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    let mut stack: Vec<(isize, usize)> = Vec::new();
    let mut active_sections: HashMap<usize, String> = HashMap::new();
    let mut problems: Vec<ProblemLine> = Vec::new();
    let mut tree_lines = 0usize;
    let mut connector_lines = 0usize;
    let mut unparsed = 0usize;
    let mut max_depth = 0usize;

    for (idx, raw) in stdout.lines().enumerate() {
        if idx >= MAX_LINES {
            return None;
        }
        let line_no = idx + 1;
        let clean = strip_ansi(raw);
        let line = clean.trim_end();
        if line.trim().is_empty() {
            stack.clear();
            active_sections.clear();
            continue;
        }

        // Capture cargo warnings / errors if present on stdout.
        let trimmed = line.trim_start();
        if trimmed.starts_with("warning:")
            || trimmed.starts_with("error:")
            || trimmed.starts_with("error[")
        {
            problems.push(ProblemLine {
                text: trimmed.to_owned(),
                line: line_no,
            });
            continue;
        }

        // Check tree connector lines (`├──`, `└──`, `|--`, `` `-- ``, `\--`).
        if let Some((col, rest)) = split_tree_prefix(line, false) {
            let Some(parsed) = parse_cargo_node(rest) else {
                unparsed += 1;
                continue;
            };
            if stack.is_empty() {
                unparsed += 1;
                continue;
            }
            push_node(
                &mut nodes,
                &mut roots,
                &mut stack,
                &mut active_sections,
                col as isize,
                parsed.into_tree_node(line_no),
                &mut max_depth,
            );
            tree_lines += 1;
            connector_lines += 1;
            continue;
        }

        // Check `[build-dependencies]` / `[dev-dependencies]` / `[dependencies]`.
        let (gutter_w, after_gutter) = strip_gutter(line);
        if let Some(sec) = parse_cargo_section(after_gutter) {
            while let Some(&(top_col, top_idx)) = stack.last() {
                if top_col >= gutter_w as isize {
                    stack.pop();
                    active_sections.remove(&top_idx);
                } else {
                    break;
                }
            }
            if let Some(&(_, parent_idx)) = stack.last() {
                if let Some(sec_name) = sec {
                    active_sections.insert(parent_idx, sec_name.to_owned());
                } else {
                    active_sections.remove(&parent_idx);
                }
                update_ancestors_last_line(&mut nodes, &stack, line_no);
                tree_lines += 1;
            } else {
                unparsed += 1;
            }
            continue;
        }

        // Support `--prefix depth` (`0my-app v0.1.0`, `1anyhow v1.0.86`).
        if let Some((depth, rest)) = split_cargo_depth_prefix(line) {
            if let Some(sec) = parse_cargo_section(rest) {
                while let Some(&(top_col, top_idx)) = stack.last() {
                    if top_col >= depth as isize {
                        stack.pop();
                        active_sections.remove(&top_idx);
                    } else {
                        break;
                    }
                }
                if let Some(&(_, parent_idx)) = stack.last() {
                    if let Some(sec_name) = sec {
                        active_sections.insert(parent_idx, sec_name.to_owned());
                    } else {
                        active_sections.remove(&parent_idx);
                    }
                    update_ancestors_last_line(&mut nodes, &stack, line_no);
                    tree_lines += 1;
                }
                continue;
            }
            if let Some(parsed) = parse_cargo_node(rest) {
                let col = if depth == 0 {
                    stack.clear();
                    active_sections.clear();
                    -1
                } else {
                    if stack.is_empty() {
                        unparsed += 1;
                        continue;
                    }
                    depth as isize
                };
                push_node(
                    &mut nodes,
                    &mut roots,
                    &mut stack,
                    &mut active_sections,
                    col,
                    parsed.into_tree_node(line_no),
                    &mut max_depth,
                );
                tree_lines += 1;
                if depth > 0 {
                    connector_lines += 1;
                }
                continue;
            }
        }

        // Otherwise expect a root package line at column 0.
        if !line.starts_with(' ')
            && let Some(parsed) = parse_cargo_node(line)
        {
            stack.clear();
            active_sections.clear();
            push_node(
                &mut nodes,
                &mut roots,
                &mut stack,
                &mut active_sections,
                -1,
                parsed.into_tree_node(line_no),
                &mut max_depth,
            );
            tree_lines += 1;
            continue;
        }

        unparsed += 1;
    }

    if roots.is_empty() || connector_lines == 0 {
        return None;
    }
    if unparsed > (tree_lines / 20).max(2) {
        return None;
    }

    let tool_label = if duplicates_mode {
        "cargo tree -d".to_owned()
    } else if inverted {
        "cargo tree -i".to_owned()
    } else {
        "cargo tree".to_owned()
    };

    Some(ParsedTree {
        tool_label,
        style: DisplayStyle::Cargo,
        roots,
        nodes,
        problems,
        config_roots: false,
        package_forest: false,
        project_title: None,
        empty_configs: 0,
        tree_lines,
        max_depth,
    })
}

fn parse_cargo_section(s: &str) -> Option<Option<&'static str>> {
    match s.trim() {
        "[build-dependencies]" => Some(Some("build")),
        "[dev-dependencies]" => Some(Some("dev")),
        "[dependencies]" => Some(None),
        _ => None,
    }
}

fn split_cargo_depth_prefix(line: &str) -> Option<(usize, &str)> {
    let digits = line.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = &line[digits..];
    if rest.is_empty() || rest.starts_with(' ') {
        return None;
    }
    let depth: usize = line[..digits].parse().ok()?;
    Some((depth, rest))
}

struct ParsedCargoNode {
    name: String,
    version: String,
    deduped: bool,
    proc_macro: bool,
    workspace_path: Option<String>,
}

impl ParsedCargoNode {
    fn into_tree_node(self, line: usize) -> TreeNode {
        TreeNode {
            name: self.name,
            version: self.version,
            requested_version: None,
            section: None,
            deduped: self.deduped,
            proc_macro: self.proc_macro,
            constraint: false,
            status_tag: None,
            workspace_path: self.workspace_path,
            line,
            subtree_last_line: line,
            children: Vec::new(),
        }
    }
}

fn parse_cargo_node(raw: &str) -> Option<ParsedCargoNode> {
    if raw.contains('\t') {
        return None;
    }
    let mut s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let mut deduped = false;
    let mut proc_macro = false;
    let mut workspace_path = None;

    // Peel off known parenthetical annotations from the right.
    loop {
        if let Some(before) = s.strip_suffix(" (*)") {
            deduped = true;
            s = before.trim_end();
        } else if let Some(before) = s.strip_suffix(" (proc-macro)") {
            proc_macro = true;
            s = before.trim_end();
        } else if s.ends_with(')')
            && let Some(open) = s.rfind(" (")
        {
            let inner = &s[open + 2..s.len() - 1];
            if inner.contains('/')
                || inner.contains('\\')
                || inner.contains("://")
                || inner.starts_with("http")
                || inner.starts_with("git+")
                || inner.starts_with("registry+")
            {
                workspace_path = Some(inner.to_owned());
                s = s[..open].trim_end();
            } else {
                break;
            }
        } else {
            break;
        }
    }

    // Handle `cargo tree -e features`: `<crate> feature "<name>"`.
    let mut parts = s.split_whitespace();
    let name = parts.next()?;
    if !is_valid_crate_name(name) {
        return None;
    }
    let second = parts.next()?;
    if second == "feature" {
        let feat = parts.next()?;
        if parts.next().is_some() || !feat.starts_with('"') {
            return None;
        }
        return Some(ParsedCargoNode {
            name: format!("{name} feature {feat}"),
            version: String::new(),
            deduped,
            proc_macro,
            workspace_path,
        });
    }

    // Standard `cargo tree`: `<name> v<version>` with no extra table columns.
    if parts.next().is_some() {
        return None;
    }
    if !is_cargo_version(second) {
        return None;
    }
    Some(ParsedCargoNode {
        name: name.to_owned(),
        version: second.to_owned(),
        deduped,
        proc_macro,
        workspace_path,
    })
}

fn is_valid_crate_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn is_cargo_version(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('v') else {
        return false;
    };
    rest.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

// ---------------------------------------------------------------------------
// Parser 2: npm ls / pnpm list / yarn list
// ---------------------------------------------------------------------------

fn parse_js_tree(stdout: &str, kind: ToolKind) -> Option<ParsedTree> {
    let mut nodes: Vec<TreeNode> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    let mut stack: Vec<(isize, usize)> = Vec::new();
    let mut active_sections: HashMap<usize, String> = HashMap::new();
    let mut problems: Vec<ProblemLine> = Vec::new();
    let mut tree_lines = 0usize;
    let mut connector_lines = 0usize;
    let mut unparsed = 0usize;
    let mut max_depth = 0usize;

    let is_pnpm = kind == ToolKind::PnpmList;
    let is_yarn = kind == ToolKind::YarnList;
    // Track whether we are inside a pnpm section (`dependencies:`, `devDependencies:`, ...).
    let mut pnpm_section: Option<Option<String>> = None;

    for (idx, raw) in stdout.lines().enumerate() {
        if idx >= MAX_LINES {
            return None;
        }
        let line_no = idx + 1;
        let clean = strip_ansi(raw);
        let line = clean.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Boilerplate lines for pnpm / yarn / npm.
        if trimmed.starts_with("Legend:")
            || trimmed.starts_with("yarn list v")
            || (is_yarn && trimmed.starts_with("Done in "))
            || (is_yarn && trimmed.starts_with("info "))
        {
            continue;
        }

        // Standalone npm/pnpm/yarn error/warning lines.
        if is_js_standalone_problem(trimmed) {
            problems.push(ProblemLine {
                text: trimmed.to_owned(),
                line: line_no,
            });
            continue;
        }

        // pnpm section headers (`dependencies:`, `devDependencies:`, ...).
        if is_pnpm && let Some(sec) = parse_pnpm_section(trimmed) {
            pnpm_section = Some(sec.map(str::to_owned));
            continue;
        }

        // Tree connector line (`├──`, `├─┬`, `└──`, `└─┬`, `├─`, `└─`, `+--`, etc.).
        if let Some((col, rest)) = split_tree_prefix(line, false) {
            let rest_trimmed = rest.trim();
            if rest_trimmed == "(empty)" {
                tree_lines += 1;
                connector_lines += 1;
                continue;
            }
            if is_js_inline_problem(rest_trimmed) {
                problems.push(ProblemLine {
                    text: rest_trimmed.to_owned(),
                    line: line_no,
                });
            }
            if let Some(parsed) = parse_js_node(rest_trimmed, is_pnpm) {
                let mut node = parsed.into_tree_node(line_no);
                // Yarn list often has no depth-0 root line: col 0 items are top-level roots.
                if is_yarn && roots.is_empty() && col == 0 {
                    node.section = None;
                }
                push_node(
                    &mut nodes,
                    &mut roots,
                    &mut stack,
                    &mut active_sections,
                    col as isize,
                    node,
                    &mut max_depth,
                );
                tree_lines += 1;
                connector_lines += 1;
                continue;
            }
            // If an UNMET line doesn't parse as normal `pkg@ver`, still keep tree alignment.
            if is_js_inline_problem(rest_trimmed) {
                let placeholder = TreeNode {
                    name: rest_trimmed.to_owned(),
                    version: String::new(),
                    requested_version: None,
                    section: None,
                    deduped: false,
                    proc_macro: false,
                    constraint: false,
                    status_tag: Some("problem".to_owned()),
                    workspace_path: None,
                    line: line_no,
                    subtree_last_line: line_no,
                    children: Vec::new(),
                };
                push_node(
                    &mut nodes,
                    &mut roots,
                    &mut stack,
                    &mut active_sections,
                    col as isize,
                    placeholder,
                    &mut max_depth,
                );
                tree_lines += 1;
                connector_lines += 1;
                continue;
            }
            unparsed += 1;
            continue;
        }

        // Unconnected lines: either a root project header (`app@1.0.0 /path`) or
        // a pnpm direct dependency under a section (`express 4.21.1`).
        if !line.starts_with(' ') {
            if is_pnpm && let Some(sec_opt) = &pnpm_section {
                // Distinguish a new monorepo project root (`pkg@1.0.0 /abs/path`) from a direct dep.
                if looks_like_js_project_root(trimmed) {
                    pnpm_section = None;
                    stack.clear();
                    active_sections.clear();
                    if let Some(parsed) = parse_js_node(trimmed, true) {
                        push_node(
                            &mut nodes,
                            &mut roots,
                            &mut stack,
                            &mut active_sections,
                            -2,
                            parsed.into_tree_node(line_no),
                            &mut max_depth,
                        );
                        tree_lines += 1;
                        continue;
                    }
                } else {
                    if is_js_inline_problem(trimmed) {
                        problems.push(ProblemLine {
                            text: trimmed.to_owned(),
                            line: line_no,
                        });
                    }
                    if let Some(parsed) = parse_js_node(trimmed, true) {
                        let mut node = parsed.into_tree_node(line_no);
                        node.section = sec_opt.clone();
                        push_node(
                            &mut nodes,
                            &mut roots,
                            &mut stack,
                            &mut active_sections,
                            -1,
                            node,
                            &mut max_depth,
                        );
                        tree_lines += 1;
                        connector_lines += 1;
                        continue;
                    }
                }
            }

            if let Some(parsed) = parse_js_node(trimmed, is_pnpm) {
                stack.clear();
                active_sections.clear();
                pnpm_section = None;
                push_node(
                    &mut nodes,
                    &mut roots,
                    &mut stack,
                    &mut active_sections,
                    -2,
                    parsed.into_tree_node(line_no),
                    &mut max_depth,
                );
                tree_lines += 1;
                continue;
            }
        }

        unparsed += 1;
    }

    if roots.is_empty() || connector_lines == 0 {
        return None;
    }
    if unparsed > (tree_lines / 15).max(3) {
        return None;
    }

    let tool_label = match kind {
        ToolKind::NpmLs => "npm ls",
        ToolKind::PnpmList => "pnpm list",
        ToolKind::YarnList => "yarn list",
        _ => unreachable!(),
    }
    .to_owned();

    // Yarn list without a depth-0 root produces a forest of top-level entries.
    let package_forest = is_yarn && roots.len() > 1;

    Some(ParsedTree {
        tool_label,
        style: DisplayStyle::Js,
        roots,
        nodes,
        problems,
        config_roots: false,
        package_forest,
        project_title: None,
        empty_configs: 0,
        tree_lines,
        max_depth,
    })
}

fn is_js_standalone_problem(trimmed: &str) -> bool {
    trimmed.starts_with("npm ERR!")
        || trimmed.starts_with("npm error")
        || trimmed.starts_with("npm WARN")
        || trimmed.starts_with("ERR_PNPM_")
        || trimmed.starts_with("WARN ")
        || trimmed.starts_with("ERROR ")
        || trimmed.starts_with("missing:")
        || trimmed.starts_with("extraneous:")
        || trimmed.starts_with("invalid:")
        || trimmed.starts_with("peer dep missing:")
        || trimmed.starts_with("UNMET ")
}

fn is_js_inline_problem(s: &str) -> bool {
    s.contains("UNMET DEPENDENCY")
        || s.contains("UNMET PEER DEPENDENCY")
        || s.contains("UNMET OPTIONAL DEPENDENCY")
        || s.contains("invalid:")
        || s.contains("missing:")
        || s.contains("extraneous:")
        || s.contains("peer missing")
        || s.ends_with(" missing")
        || s.ends_with(" extraneous")
        || s.ends_with(" invalid")
}

fn parse_pnpm_section(trimmed: &str) -> Option<Option<&'static str>> {
    match trimmed {
        "dependencies:" => Some(None),
        "devDependencies:" => Some(Some("dev")),
        "optionalDependencies:" => Some(Some("optional")),
        "peerDependencies:" => Some(Some("peer")),
        "unsavedDependencies:" => Some(Some("unsaved")),
        _ => None,
    }
}

fn looks_like_js_project_root(trimmed: &str) -> bool {
    if trimmed.contains(" (PRIVATE)") {
        return true;
    }
    let Some((_, rest)) = trimmed.split_once(' ') else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with('/')
        || rest.starts_with("./")
        || rest.starts_with("../")
        || (rest.len() >= 3 && rest.as_bytes()[1] == b':' && rest.as_bytes()[2] == b'\\')
}

struct ParsedJsNode {
    name: String,
    version: String,
    deduped: bool,
    status_tag: Option<String>,
    workspace_path: Option<String>,
}

impl ParsedJsNode {
    fn into_tree_node(self, line: usize) -> TreeNode {
        TreeNode {
            name: self.name,
            version: self.version,
            requested_version: None,
            section: None,
            deduped: self.deduped,
            proc_macro: false,
            constraint: false,
            status_tag: self.status_tag,
            workspace_path: self.workspace_path,
            line,
            subtree_last_line: line,
            children: Vec::new(),
        }
    }
}

fn parse_js_node(raw: &str, allow_space_version: bool) -> Option<ParsedJsNode> {
    let mut s = raw.trim();
    if s.is_empty() || s.contains('\t') {
        return None;
    }

    // Handle `UNMET [PEER|OPTIONAL] DEPENDENCY <spec>`.
    for prefix in [
        "UNMET PEER DEPENDENCY ",
        "UNMET OPTIONAL DEPENDENCY ",
        "UNMET DEPENDENCY ",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let name = split_js_pkg_ver(rest.split_whitespace().next()?)
                .map(|(n, _)| n.to_owned())
                .unwrap_or_else(|| rest.split_whitespace().next().unwrap_or(rest).to_owned());
            return Some(ParsedJsNode {
                name,
                version: String::new(),
                deduped: false,
                status_tag: Some("unmet".to_owned()),
                workspace_path: None,
            });
        }
    }

    let mut deduped = false;
    let mut status_tag: Option<String> = None;
    let mut workspace_path: Option<String> = None;

    // Strip workspace symlink target `-> ./packages/foo` or `<- ../foo`.
    if let Some(pos) = s.find(" -> ") {
        let target = s[pos + 4..].trim();
        let target = target.split_whitespace().next().unwrap_or(target);
        workspace_path = Some(target.to_owned());
        let after = s[pos + 4..].trim();
        if after.contains("deduped") {
            deduped = true;
        }
        s = s[..pos].trim_end();
    } else if let Some(pos) = s.find(" <- ") {
        workspace_path = Some(s[pos + 4..].trim().to_owned());
        s = s[..pos].trim_end();
    }

    // Strip `invalid: ...`, `missing: ...`, `extraneous: ...` trailers.
    for (kw, tag) in [
        (" invalid:", "invalid"),
        (" missing:", "missing"),
        (" extraneous:", "extraneous"),
    ] {
        if let Some(pos) = s.find(kw) {
            status_tag = Some(tag.to_owned());
            s = s[..pos].trim_end();
        }
    }

    // Peel trailing status words.
    loop {
        if let Some(before) = s.strip_suffix(" deduped") {
            deduped = true;
            s = before.trim_end();
        } else if let Some(before) = s.strip_suffix(" extraneous") {
            status_tag = Some("extraneous".to_owned());
            s = before.trim_end();
        } else if let Some(before) = s.strip_suffix(" missing") {
            status_tag = Some("missing".to_owned());
            s = before.trim_end();
        } else if let Some(before) = s.strip_suffix(" invalid") {
            status_tag = Some("invalid".to_owned());
            s = before.trim_end();
        } else if let Some(before) = s.strip_suffix(" (PRIVATE)") {
            s = before.trim_end();
        } else if let Some(before) = s.strip_suffix(" peer missing") {
            status_tag = Some("unmet".to_owned());
            s = before.trim_end();
        } else if let Some(before) = s.strip_suffix(" (peer)") {
            s = before.trim_end();
        } else {
            break;
        }
    }

    // Strip trailing filesystem path on root line (`my-app@1.0.0 /path/to/app`).
    if let Some((first, rest)) = s.split_once(' ') {
        let rest_t = rest.trim_start();
        if rest_t.starts_with('/')
            || rest_t.starts_with("./")
            || rest_t.starts_with("../")
            || (rest_t.len() >= 3 && rest_t.as_bytes()[1] == b':' && rest_t.as_bytes()[2] == b'\\')
        {
            workspace_path = Some(rest_t.to_owned());
            s = first;
        }
    }

    // Try `pkg@ver` (including scoped `@scope/pkg@ver`).
    if !s.contains(' ')
        && let Some((name, ver)) = split_js_pkg_ver(s)
    {
        if !is_valid_js_name(name) {
            return None;
        }
        let version = if status_tag.as_deref() == Some("missing") {
            String::new()
        } else {
            ver.to_owned()
        };
        return Some(ParsedJsNode {
            name: name.to_owned(),
            version,
            deduped,
            status_tag,
            workspace_path,
        });
    }

    // pnpm format: `<name> <version>` or `<name> link:../foo`.
    if allow_space_version && let Some((name, rest)) = s.split_once(' ') {
        let ver_tok = rest.trim();
        if is_valid_js_name(name) && !ver_tok.is_empty() && !ver_tok.contains(' ') {
            if let Some(link) = ver_tok.strip_prefix("link:") {
                return Some(ParsedJsNode {
                    name: name.to_owned(),
                    version: String::new(),
                    deduped,
                    status_tag,
                    workspace_path: Some(link.to_owned()),
                });
            }
            if is_js_version(ver_tok) {
                let version = if status_tag.as_deref() == Some("missing") {
                    String::new()
                } else {
                    ver_tok.to_owned()
                };
                return Some(ParsedJsNode {
                    name: name.to_owned(),
                    version,
                    deduped,
                    status_tag,
                    workspace_path,
                });
            }
        }
    }

    // Root without version (e.g., `my-app /path/to/app` where workspace_path was stripped).
    if workspace_path.is_some() && is_valid_js_name(s) {
        return Some(ParsedJsNode {
            name: s.to_owned(),
            version: String::new(),
            deduped,
            status_tag,
            workspace_path,
        });
    }

    None
}

fn split_js_pkg_ver(s: &str) -> Option<(&str, &str)> {
    // Handle npm alias `alias@npm:real@1.2.3` by finding the `@` after scope.
    let search_start = if s.starts_with('@') {
        s.find('/')? + 1
    } else {
        0
    };
    let rel = s[search_start..].find('@')?;
    let at = search_start + rel;
    let name = &s[..at];
    let ver = &s[at + 1..];
    if name.is_empty() || ver.is_empty() {
        return None;
    }
    Some((name, ver))
}

fn is_valid_js_name(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if let Some(rest) = s.strip_prefix('@') {
        let Some((scope, pkg)) = rest.split_once('/') else {
            return false;
        };
        !scope.is_empty()
            && !pkg.is_empty()
            && scope
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            && pkg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    } else {
        s.as_bytes()[0].is_ascii_alphanumeric()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    }
}

fn is_js_version(s: &str) -> bool {
    s.as_bytes().first().is_some_and(u8::is_ascii_digit)
        || s.starts_with("npm:")
        || s.starts_with("workspace:")
        || s.starts_with("file:")
        || s.starts_with("git+")
}

// ---------------------------------------------------------------------------
// Parser 3: pipdeptree
// ---------------------------------------------------------------------------

fn parse_pipdeptree(stdout: &str, reverse: bool) -> Option<ParsedTree> {
    let mut nodes: Vec<TreeNode> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    let mut stack: Vec<(isize, usize)> = Vec::new();
    let mut active_sections: HashMap<usize, String> = HashMap::new();
    let mut problems: Vec<ProblemLine> = Vec::new();
    let mut tree_lines = 0usize;
    let mut connector_lines = 0usize;
    let mut unparsed = 0usize;
    let mut max_depth = 0usize;
    let mut in_warning_block = false;

    for (idx, raw) in stdout.lines().enumerate() {
        if idx >= MAX_LINES {
            return None;
        }
        let line_no = idx + 1;
        let clean = strip_ansi(raw);
        let line = clean.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            in_warning_block = false;
            continue;
        }

        if trimmed.starts_with("Warning!!") || trimmed.starts_with("WARNING:") {
            in_warning_block = true;
            problems.push(ProblemLine {
                text: trimmed.to_owned(),
                line: line_no,
            });
            continue;
        }
        if in_warning_block && (trimmed.starts_with("* ") || trimmed.starts_with("- ")) {
            problems.push(ProblemLine {
                text: trimmed.to_owned(),
                line: line_no,
            });
            continue;
        }
        in_warning_block = false;

        // Child lines (`├── `, `└── `, `  - `).
        if let Some((col, rest)) = split_tree_prefix(line, true) {
            if stack.is_empty() {
                unparsed += 1;
                continue;
            }
            if let Some((node, missing_problem)) = parse_pip_child(rest.trim(), reverse, line_no) {
                if let Some(msg) = missing_problem {
                    problems.push(ProblemLine {
                        text: msg,
                        line: line_no,
                    });
                }
                push_node(
                    &mut nodes,
                    &mut roots,
                    &mut stack,
                    &mut active_sections,
                    col as isize,
                    node,
                    &mut max_depth,
                );
                tree_lines += 1;
                connector_lines += 1;
                continue;
            }
            unparsed += 1;
            continue;
        }

        // Top-level package line: `pkg==version`.
        if !line.starts_with(' ')
            && let Some((name, ver)) = parse_pip_eq(trimmed)
        {
            stack.clear();
            active_sections.clear();
            let node = TreeNode {
                name: name.to_owned(),
                version: ver.to_owned(),
                requested_version: None,
                section: None,
                deduped: false,
                proc_macro: false,
                constraint: false,
                status_tag: None,
                workspace_path: None,
                line: line_no,
                subtree_last_line: line_no,
                children: Vec::new(),
            };
            push_node(
                &mut nodes,
                &mut roots,
                &mut stack,
                &mut active_sections,
                -1,
                node,
                &mut max_depth,
            );
            tree_lines += 1;
            continue;
        }

        unparsed += 1;
    }

    if roots.is_empty() || (connector_lines == 0 && roots.len() < 3) {
        return None;
    }
    if unparsed > (tree_lines / 15).max(2) {
        return None;
    }

    Some(ParsedTree {
        tool_label: if reverse {
            "pipdeptree -r".to_owned()
        } else {
            "pipdeptree".to_owned()
        },
        style: DisplayStyle::Pip,
        roots,
        nodes,
        problems,
        config_roots: false,
        package_forest: !reverse,
        project_title: None,
        empty_configs: 0,
        tree_lines,
        max_depth,
    })
}

fn parse_pip_eq(s: &str) -> Option<(&str, &str)> {
    let (name, ver) = s.split_once("==")?;
    if !is_valid_pip_name(name) || ver.is_empty() || ver.contains(' ') {
        return None;
    }
    Some((name, ver))
}

fn parse_pip_child(s: &str, reverse: bool, line: usize) -> Option<(TreeNode, Option<String>)> {
    if reverse {
        // `pydantic==2.9.2 [requires: typing-extensions>=4.6.1]`
        let head = s.split(" [requires:").next().unwrap_or(s).trim();
        let (name, ver) = parse_pip_eq(head)?;
        return Some((
            TreeNode {
                name: name.to_owned(),
                version: ver.to_owned(),
                requested_version: None,
                section: None,
                deduped: false,
                proc_macro: false,
                constraint: false,
                status_tag: None,
                workspace_path: None,
                line,
                subtree_last_line: line,
                children: Vec::new(),
            },
            None,
        ));
    }

    // Forward: `certifi [required: >=2017.4.17, installed: 2024.8.30]`
    if let Some((pkg_part, bracket)) = s.split_once(" [") {
        let name = pkg_part.trim();
        if !is_valid_pip_name(name) {
            return None;
        }
        let inner = bracket.strip_suffix(']')?;
        let installed = inner
            .split("installed:")
            .nth(1)?
            .trim()
            .trim_end_matches(']');
        let (version, missing_problem) =
            if installed == "?" || installed.eq_ignore_ascii_case("missing") {
                (String::new(), Some(format!("missing: {s}")))
            } else {
                (installed.to_owned(), None)
            };
        return Some((
            TreeNode {
                name: name.to_owned(),
                version,
                requested_version: None,
                section: None,
                deduped: false,
                proc_macro: false,
                constraint: false,
                status_tag: missing_problem.as_ref().map(|_| "missing".to_owned()),
                workspace_path: None,
                line,
                subtree_last_line: line,
                children: Vec::new(),
            },
            missing_problem,
        ));
    }

    // Fallback: `pkg==ver` child (e.g. `pipdeptree --warn silence` or custom flags).
    let (name, ver) = parse_pip_eq(s)?;
    Some((
        TreeNode {
            name: name.to_owned(),
            version: ver.to_owned(),
            requested_version: None,
            section: None,
            deduped: false,
            proc_macro: false,
            constraint: false,
            status_tag: None,
            workspace_path: None,
            line,
            subtree_last_line: line,
            children: Vec::new(),
        },
        None,
    ))
}

fn is_valid_pip_name(s: &str) -> bool {
    !s.is_empty()
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

// ---------------------------------------------------------------------------
// Parser 4: mvn dependency:tree
// ---------------------------------------------------------------------------

fn parse_mvn_tree(stdout: &str) -> Option<ParsedTree> {
    let mut nodes: Vec<TreeNode> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    let mut stack: Vec<(isize, usize)> = Vec::new();
    let mut active_sections: HashMap<usize, String> = HashMap::new();
    let mut problems: Vec<ProblemLine> = Vec::new();
    let mut tree_lines = 0usize;
    let mut connector_lines = 0usize;
    let mut unparsed = 0usize;
    let mut max_depth = 0usize;

    for (idx, raw) in stdout.lines().enumerate() {
        if idx >= MAX_LINES {
            return None;
        }
        let line_no = idx + 1;
        let clean = strip_ansi(raw);
        let line = clean.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(err) = trimmed.strip_prefix("[ERROR]") {
            let msg = err.trim();
            if !msg.is_empty()
                && !msg.starts_with("-> [Help")
                && !msg.starts_with("For more information")
            {
                problems.push(ProblemLine {
                    text: trimmed.to_owned(),
                    line: line_no,
                });
            }
            continue;
        }
        if let Some(warn) = trimmed.strip_prefix("[WARNING]") {
            let msg = warn.trim();
            if !msg.is_empty() {
                problems.push(ProblemLine {
                    text: trimmed.to_owned(),
                    line: line_no,
                });
            }
            continue;
        }

        let body = if let Some(rest) = line.strip_prefix("[INFO] ") {
            rest
        } else if trimmed == "[INFO]" {
            continue;
        } else {
            line
        };

        let body_trimmed = body.trim();
        if body_trimmed.is_empty() || is_mvn_boilerplate(body_trimmed) {
            if body_trimmed.starts_with("--- ") && body_trimmed.contains("dependency") {
                stack.clear();
                active_sections.clear();
            }
            continue;
        }
        if body_trimmed.starts_with("BUILD FAILURE") {
            problems.push(ProblemLine {
                text: body_trimmed.to_owned(),
                line: line_no,
            });
            continue;
        }

        // Connector line: `+- group:artifact:jar:1.0:compile` or `|  \- ...`.
        if let Some((col, rest)) = split_tree_prefix(body, false) {
            if stack.is_empty() {
                unparsed += 1;
                continue;
            }
            if let Some((node, conflict_warn)) = parse_mvn_node(rest.trim(), line_no) {
                if let Some(warn) = conflict_warn {
                    problems.push(ProblemLine {
                        text: warn,
                        line: line_no,
                    });
                }
                push_node(
                    &mut nodes,
                    &mut roots,
                    &mut stack,
                    &mut active_sections,
                    col as isize,
                    node,
                    &mut max_depth,
                );
                tree_lines += 1;
                connector_lines += 1;
                continue;
            }
            unparsed += 1;
            continue;
        }

        // Root coordinate line: `group:artifact:jar:1.0.0-SNAPSHOT`.
        if !body.starts_with(' ')
            && let Some((node, _)) = parse_mvn_node(body_trimmed, line_no)
        {
            stack.clear();
            active_sections.clear();
            push_node(
                &mut nodes,
                &mut roots,
                &mut stack,
                &mut active_sections,
                -1,
                node,
                &mut max_depth,
            );
            tree_lines += 1;
            continue;
        }

        unparsed += 1;
    }

    if roots.is_empty() || connector_lines == 0 {
        return None;
    }
    if unparsed > (tree_lines / 10).max(5) {
        return None;
    }

    Some(ParsedTree {
        tool_label: "mvn dependency:tree".to_owned(),
        style: DisplayStyle::Maven,
        roots,
        nodes,
        problems,
        config_roots: false,
        package_forest: false,
        project_title: None,
        empty_configs: 0,
        tree_lines,
        max_depth,
    })
}

fn is_mvn_boilerplate(s: &str) -> bool {
    s.starts_with("Scanning for projects")
        || s.starts_with("Building ")
        || s.starts_with("from ")
        || s.starts_with("BUILD SUCCESS")
        || s.starts_with("Total time:")
        || s.starts_with("Finished at:")
        || s.starts_with("Final Memory:")
        || s.starts_with("Reactor ")
        || s.starts_with("Downloading ")
        || s.starts_with("Downloaded ")
        || s.starts_with("--- ")
        || (s.starts_with("---") && s.ends_with("---"))
}

fn parse_mvn_node(raw: &str, line: usize) -> Option<(TreeNode, Option<String>)> {
    let mut s = raw.trim();
    let mut deduped = false;
    let mut conflict_warn: Option<String> = None;

    // Verbose mode wraps omitted nodes in `(...)`.
    if let Some(inner) = s.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        if let Some((coord, note)) = inner.split_once(" - ") {
            if note.contains("omitted for duplicate") {
                deduped = true;
            } else if note.contains("omitted for conflict") {
                conflict_warn = Some(inner.to_owned());
                deduped = true;
            }
            s = coord.trim();
        } else {
            s = inner.trim();
        }
    } else if let Some((coord, note)) = s.split_once(" - ") {
        if note.contains("omitted for duplicate") {
            deduped = true;
        } else if note.contains("omitted for conflict") {
            conflict_warn = Some(s.to_owned());
        }
        s = coord.trim();
    }

    let s = s.strip_suffix(" (optional)").unwrap_or(s).trim();
    if s.contains(' ') || s.contains('\t') {
        return None;
    }

    let parts: Vec<&str> = s.split(':').collect();
    let (group, artifact, version, scope) = match parts.as_slice() {
        [g, a, _pkg, v] => (*g, *a, *v, None),
        [g, a, _pkg, v, sc] => (*g, *a, *v, Some(*sc)),
        [g, a, _pkg, _cls, v, sc] => (*g, *a, *v, Some(*sc)),
        _ => return None,
    };
    if group.is_empty() || artifact.is_empty() || version.is_empty() {
        return None;
    }
    let section = match scope {
        None | Some("compile") => None,
        Some(other) => Some(other.to_owned()),
    };

    Some((
        TreeNode {
            name: format!("{group}:{artifact}"),
            version: version.to_owned(),
            requested_version: None,
            section,
            deduped,
            proc_macro: false,
            constraint: false,
            status_tag: None,
            workspace_path: None,
            line,
            subtree_last_line: line,
            children: Vec::new(),
        },
        conflict_warn,
    ))
}

// ---------------------------------------------------------------------------
// Parser 5: gradle dependencies
// ---------------------------------------------------------------------------

fn parse_gradle_tree(stdout: &str) -> Option<ParsedTree> {
    let mut nodes: Vec<TreeNode> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    let mut stack: Vec<(isize, usize)> = Vec::new();
    let mut active_sections: HashMap<usize, String> = HashMap::new();
    let mut problems: Vec<ProblemLine> = Vec::new();
    let mut project_title: Option<String> = None;
    let mut empty_configs = 0usize;
    let mut tree_lines = 0usize;
    let mut connector_lines = 0usize;
    let mut unparsed = 0usize;
    let mut max_depth = 0usize;
    let mut pending_config: Option<(String, usize)> = None;

    for (idx, raw) in stdout.lines().enumerate() {
        if idx >= MAX_LINES {
            return None;
        }
        let line_no = idx + 1;
        let clean = strip_ansi(raw);
        let line = clean.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if pending_config.take().is_some() {
                empty_configs += 1;
            }
            stack.clear();
            active_sections.clear();
            continue;
        }

        if is_gradle_boilerplate(trimmed) {
            continue;
        }
        if trimmed.starts_with("Project '") || trimmed.starts_with("Root project '") {
            project_title = Some(trimmed.to_owned());
            continue;
        }
        if trimmed == "No dependencies" {
            if pending_config.take().is_some() {
                empty_configs += 1;
            }
            stack.clear();
            active_sections.clear();
            continue;
        }
        if is_gradle_standalone_problem(trimmed) {
            problems.push(ProblemLine {
                text: trimmed.to_owned(),
                line: line_no,
            });
            continue;
        }

        // Connector line (`+--- `, `\--- `, `|    \--- `).
        if let Some((col, rest)) = split_tree_prefix(line, false) {
            // Materialize pending configuration header on its first real dependency.
            if let Some((cfg_name, cfg_line)) = pending_config.take() {
                let cfg_node = TreeNode {
                    name: cfg_name,
                    version: String::new(),
                    requested_version: None,
                    section: None,
                    deduped: false,
                    proc_macro: false,
                    constraint: false,
                    status_tag: None,
                    workspace_path: None,
                    line: cfg_line,
                    subtree_last_line: cfg_line,
                    children: Vec::new(),
                };
                push_node(
                    &mut nodes,
                    &mut roots,
                    &mut stack,
                    &mut active_sections,
                    -1,
                    cfg_node,
                    &mut max_depth,
                );
            }
            if stack.is_empty() {
                // Allow a bare Gradle tree snippet without a configuration header.
                let synth = TreeNode {
                    name: "dependencies".to_owned(),
                    version: String::new(),
                    requested_version: None,
                    section: None,
                    deduped: false,
                    proc_macro: false,
                    constraint: false,
                    status_tag: None,
                    workspace_path: None,
                    line: line_no,
                    subtree_last_line: line_no,
                    children: Vec::new(),
                };
                push_node(
                    &mut nodes,
                    &mut roots,
                    &mut stack,
                    &mut active_sections,
                    -1,
                    synth,
                    &mut max_depth,
                );
            }
            if let Some((node, prob)) = parse_gradle_node(rest.trim(), line_no) {
                if let Some(msg) = prob {
                    problems.push(ProblemLine {
                        text: msg,
                        line: line_no,
                    });
                }
                push_node(
                    &mut nodes,
                    &mut roots,
                    &mut stack,
                    &mut active_sections,
                    col as isize,
                    node,
                    &mut max_depth,
                );
                tree_lines += 1;
                connector_lines += 1;
                continue;
            }
            unparsed += 1;
            continue;
        }

        // Configuration header (`compileClasspath - Compile classpath ...`).
        if !line.starts_with(' ')
            && let Some(cfg_name) = parse_gradle_config_header(trimmed)
        {
            if pending_config.take().is_some() {
                empty_configs += 1;
            }
            stack.clear();
            active_sections.clear();
            pending_config = Some((cfg_name.to_owned(), line_no));
            continue;
        }

        unparsed += 1;
    }
    if pending_config.take().is_some() {
        empty_configs += 1;
    }

    if roots.is_empty() || connector_lines == 0 {
        return None;
    }
    if unparsed > (tree_lines / 10).max(5) {
        return None;
    }

    Some(ParsedTree {
        tool_label: "gradle dependencies".to_owned(),
        style: DisplayStyle::Gradle,
        roots,
        nodes,
        problems,
        config_roots: true,
        package_forest: false,
        project_title,
        empty_configs,
        tree_lines,
        max_depth,
    })
}

fn is_gradle_boilerplate(s: &str) -> bool {
    s.starts_with("> Task ")
        || s.starts_with("(*) - ")
        || s.starts_with("(c) - ")
        || s.starts_with("(n) - ")
        || s.starts_with("A web-based, searchable dependency report")
        || s.starts_with("BUILD SUCCESSFUL")
        || s.starts_with("Deprecated Gradle features")
        || s.starts_with("You can use '--warning-mode")
        || s.starts_with("See https://docs.gradle.org")
        || (s.len() >= 10 && s.bytes().all(|b| b == b'-'))
        || (s.contains("actionable task") && s.as_bytes()[0].is_ascii_digit())
}

fn is_gradle_standalone_problem(s: &str) -> bool {
    s.starts_with("FAILURE:")
        || s.starts_with("BUILD FAILED")
        || s.starts_with("> Could not resolve")
        || s.starts_with("> Could not find")
        || s.starts_with("* What went wrong:")
}

fn parse_gradle_config_header(trimmed: &str) -> Option<&str> {
    let head = trimmed.split(" - ").next().unwrap_or(trimmed).trim();
    let head = head.strip_suffix(" (n)").unwrap_or(head).trim();
    if head.is_empty() || head.contains(' ') || head.contains(':') {
        return None;
    }
    if head.as_bytes()[0].is_ascii_alphabetic()
        && head
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Some(head)
    } else {
        None
    }
}

fn parse_gradle_node(raw: &str, line: usize) -> Option<(TreeNode, Option<String>)> {
    let mut s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let mut deduped = false;
    let mut constraint = false;
    let mut problem: Option<String> = None;

    loop {
        if let Some(before) = s.strip_suffix(" (*)") {
            deduped = true;
            s = before.trim_end();
        } else if let Some(before) = s.strip_suffix(" (c)") {
            constraint = true;
            s = before.trim_end();
        } else if let Some(before) = s.strip_suffix(" (n)") {
            problem = Some(raw.to_owned());
            s = before.trim_end();
        } else if let Some(before) = s.strip_suffix(" FAILED") {
            problem = Some(raw.to_owned());
            s = before.trim_end();
        } else {
            break;
        }
    }

    // Local subproject: `project :common`.
    if s.starts_with("project :") {
        return Some((
            TreeNode {
                name: s.to_owned(),
                version: String::new(),
                requested_version: None,
                section: None,
                deduped,
                proc_macro: false,
                constraint,
                status_tag: problem.as_ref().map(|_| "failed".to_owned()),
                workspace_path: None,
                line,
                subtree_last_line: line,
                children: Vec::new(),
            },
            problem,
        ));
    }

    // Split version resolution `group:artifact:1.0 -> 2.0` or `group:artifact -> 2.0`.
    let (lhs, resolved_arrow) = if let Some((l, r)) = s.split_once(" -> ") {
        (l.trim(), Some(r.trim()))
    } else {
        (s, None)
    };

    let parts: Vec<&str> = lhs.split(':').collect();
    if parts.len() < 2 {
        return None;
    }
    let group = parts[0].trim();
    let artifact = parts[1].trim();
    if group.is_empty() || artifact.is_empty() {
        return None;
    }

    let (version, requested_version) = match (parts.get(2).copied(), resolved_arrow) {
        (Some(req), Some(res)) => {
            let req_clean = req.trim();
            let req_opt = (!req_clean.is_empty() && req_clean != res).then(|| req_clean.to_owned());
            (res.to_owned(), req_opt)
        }
        (None, Some(res)) => (res.to_owned(), None),
        (Some(ver), None) => (ver.trim().to_owned(), None),
        (None, None) => (String::new(), None),
    };

    Some((
        TreeNode {
            name: format!("{group}:{artifact}"),
            version,
            requested_version,
            section: None,
            deduped,
            proc_macro: false,
            constraint,
            status_tag: problem.as_ref().map(|_| "failed".to_owned()),
            workspace_path: None,
            line,
            subtree_last_line: line,
            children: Vec::new(),
        },
        problem,
    ))
}

// ---------------------------------------------------------------------------
// Parser 6: go mod graph
// ---------------------------------------------------------------------------

fn parse_go_mod_graph(stdout: &str) -> Option<ParsedGoGraph> {
    let mut edges: Vec<GoEdge> = Vec::new();
    let mut problems: Vec<ProblemLine> = Vec::new();
    let mut root_set: BTreeSet<String> = BTreeSet::new();
    let mut roots: Vec<String> = Vec::new();
    let mut versioned_tokens = 0usize;

    for (idx, raw) in stdout.lines().enumerate() {
        if idx >= MAX_LINES {
            return None;
        }
        let line_no = idx + 1;
        let clean = strip_ansi(raw);
        let trimmed = clean.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("go: ") {
            problems.push(ProblemLine {
                text: trimmed.to_owned(),
                line: line_no,
            });
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        let parent = parts.next()?;
        let child = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        let (p_name, p_ver) = parse_go_module_token(parent)?;
        let (c_name, c_ver) = parse_go_module_token(child)?;
        if p_ver.is_some() {
            versioned_tokens += 1;
        }
        if c_ver.is_some() {
            versioned_tokens += 1;
        }
        if p_ver.is_none() && root_set.insert(p_name.to_owned()) {
            roots.push(p_name.to_owned());
        }
        edges.push(GoEdge {
            parent_name: p_name.to_owned(),
            parent_ver: p_ver.map(str::to_owned),
            child_name: c_name.to_owned(),
            child_ver: c_ver.map(str::to_owned),
            line: line_no,
        });
    }

    if edges.len() < 2 || versioned_tokens == 0 {
        return None;
    }

    Some(ParsedGoGraph {
        roots,
        edges,
        problems,
    })
}

fn parse_go_module_token(tok: &str) -> Option<(&str, Option<&str>)> {
    if tok.is_empty()
        || tok
            .bytes()
            .any(|b| matches!(b, b'{' | b'}' | b'[' | b']' | b'"' | b',' | b';' | b'='))
    {
        return None;
    }
    let (path, ver) = match tok.split_once('@') {
        Some((p, v)) => {
            if p.is_empty() || v.is_empty() {
                return None;
            }
            (p, Some(v))
        }
        None => (tok, None),
    };
    if !path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b'~' | b'+'))
    {
        return None;
    }
    // At least look like a Go module path or `go`/`toolchain` pseudo-module.
    if !path.contains('/')
        && !path.contains('.')
        && !matches!(path, "go" | "toolchain")
        && ver.is_none()
    {
        return None;
    }
    Some((path, ver))
}

// ---------------------------------------------------------------------------
// Common analysis helpers (duplicates, problems, transitive counts, hubs)
// ---------------------------------------------------------------------------

struct DuplicateEntry {
    name: String,
    /// `(version, first_line)` sorted by natural version order.
    versions: Vec<(String, usize)>,
}

fn collect_tree_duplicates(tree: &ParsedTree) -> Vec<DuplicateEntry> {
    // Key by normalized package name (PEP 503 for pip, exact otherwise).
    let mut by_pkg: BTreeMap<String, (String, BTreeMap<String, usize>)> = BTreeMap::new();
    for (idx, node) in tree.nodes.iter().enumerate() {
        if tree.config_roots && tree.roots.contains(&idx) {
            continue;
        }
        if node.version.is_empty() || node.constraint {
            continue;
        }
        let key = if tree.style == DisplayStyle::Pip {
            node.name.to_ascii_lowercase().replace('_', "-")
        } else {
            node.name.clone()
        };
        let entry = by_pkg
            .entry(key)
            .or_insert_with(|| (node.name.clone(), BTreeMap::new()));
        entry
            .1
            .entry(node.version.clone())
            .and_modify(|first| *first = (*first).min(node.line))
            .or_insert(node.line);
    }

    let mut dups = Vec::new();
    for (_key, (display_name, vers)) in by_pkg {
        if vers.len() >= 2 {
            let mut list: Vec<(String, usize)> = vers.into_iter().collect();
            list.sort_by(|(va, _), (vb, _)| version_cmp(va, vb));
            dups.push(DuplicateEntry {
                name: display_name,
                versions: list,
            });
        }
    }
    dups
}

fn format_problems(problems: &[ProblemLine]) -> String {
    if problems.is_empty() {
        return String::new();
    }
    // Preserve order of first occurrence while folding exact duplicate problem lines
    // without losing line numbers.
    struct Group<'a> {
        text: &'a str,
        lines: Vec<usize>,
    }
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut groups: Vec<Group<'_>> = Vec::new();
    for p in problems {
        if let Some(&pos) = index.get(p.text.as_str()) {
            groups[pos].lines.push(p.line);
        } else {
            index.insert(p.text.as_str(), groups.len());
            groups.push(Group {
                text: p.text.as_str(),
                lines: vec![p.line],
            });
        }
    }

    let mut out = format!("problems ({}):\n", problems.len());
    for g in groups {
        let mult = if g.lines.len() > 1 {
            format!(" ×{}", g.lines.len())
        } else {
            String::new()
        };
        let ptr = format_line_list(&g.lines);
        let _ = writeln!(out, "  {}{mult} {ptr}", g.text);
    }
    out
}

fn format_line_list(lines: &[usize]) -> String {
    match lines {
        [] => String::new(),
        [a] => format!("[stdout:{a}]"),
        [a, b] => format!("[stdout:{a}, {b}]"),
        [a, b, c] => format!("[stdout:{a}, {b}, {c}]"),
        _ => format!(
            "[stdout:{}, {}, …, {}]",
            lines[0],
            lines[1],
            lines[lines.len() - 1]
        ),
    }
}

fn format_duplicates_multiline(dups: &[DuplicateEntry]) -> String {
    if dups.is_empty() {
        return String::new();
    }
    let mut out = format!("duplicates ({}):\n", dups.len());
    for d in dups {
        let vers: Vec<String> = d
            .versions
            .iter()
            .map(|(v, line)| format!("{v} [stdout:{line}]"))
            .collect();
        let _ = writeln!(out, "  {} ({})", d.name, vers.join(", "));
    }
    out
}

fn format_duplicates_inline(dups: &[DuplicateEntry]) -> String {
    if dups.is_empty() {
        return String::new();
    }
    let items: Vec<String> = dups
        .iter()
        .map(|d| {
            let vers: Vec<String> = d
                .versions
                .iter()
                .map(|(v, line)| format!("{v} [stdout:{line}]"))
                .collect();
            format!("{} ({})", d.name, vers.join(", "))
        })
        .collect();
    format!("duplicates ({}): {}\n", dups.len(), items.join(", "))
}

fn format_pkg_ver(name: &str, version: &str, style: DisplayStyle) -> String {
    if version.is_empty() {
        return name.to_owned();
    }
    match style {
        DisplayStyle::Cargo => format!("{name} {version}"),
        DisplayStyle::Js => format!("{name}@{version}"),
        DisplayStyle::Pip => format!("{name}=={version}"),
        DisplayStyle::Maven | DisplayStyle::Gradle => format!("{name}:{version}"),
    }
}

fn format_direct_node_label(node: &TreeNode, style: DisplayStyle) -> String {
    let mut s = if let Some(req) = &node.requested_version {
        format!("{}:{req} -> {}", node.name, node.version)
    } else {
        format_pkg_ver(&node.name, &node.version, style)
    };
    if node.proc_macro {
        s.push_str(" (proc-macro)");
    }
    if node.constraint {
        s.push_str(" (c)");
    }
    if let Some(ws) = &node.workspace_path
        && style == DisplayStyle::Js
    {
        let _ = write!(s, " -> {ws}");
    }
    if let Some(sec) = &node.section {
        let _ = write!(s, " [{sec}]");
    }
    if let Some(tag) = &node.status_tag {
        let _ = write!(s, " [{tag}]");
    }
    if node.deduped {
        match style {
            DisplayStyle::Js => s.push_str(" (deduped)"),
            _ => s.push_str(" (*)"),
        }
    }
    s
}

/// Count unique transitive packages `(name, version)` inside `node_idx`'s
/// subtree (excluding `node_idx` itself).
fn count_unique_transitive(tree: &ParsedTree, node_idx: usize) -> usize {
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    let mut stack: Vec<usize> = tree.nodes[node_idx].children.clone();
    while let Some(curr) = stack.pop() {
        let n = &tree.nodes[curr];
        if !n.name.is_empty() && n.status_tag.as_deref() != Some("problem") {
            seen.insert((n.name.as_str(), n.version.as_str()));
        }
        stack.extend(&n.children);
    }
    seen.len()
}

fn collect_unique_packages(tree: &ParsedTree) -> usize {
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    for (idx, n) in tree.nodes.iter().enumerate() {
        if tree.config_roots && tree.roots.contains(&idx) {
            continue;
        }
        if n.status_tag.as_deref() == Some("problem") || n.name.is_empty() {
            continue;
        }
        seen.insert((n.name.as_str(), n.version.as_str()));
    }
    seen.len()
}

fn count_deduped_nodes(tree: &ParsedTree) -> usize {
    tree.nodes.iter().filter(|n| n.deduped).count()
}

/// Collect heavily repeated packages (occurring >= 3 times across the tree).
fn collect_shared_hubs(tree: &ParsedTree) -> Vec<(String, usize)> {
    let mut counts: HashMap<(&str, &str), usize> = HashMap::new();
    for (idx, n) in tree.nodes.iter().enumerate() {
        if tree.config_roots && tree.roots.contains(&idx) {
            continue;
        }
        if n.status_tag.as_deref() == Some("problem") || n.name.is_empty() {
            continue;
        }
        *counts
            .entry((n.name.as_str(), n.version.as_str()))
            .or_default() += 1;
    }
    let mut hubs: Vec<((&str, &str), usize)> =
        counts.into_iter().filter(|(_, c)| *c >= 3).collect();
    hubs.sort_by(|(ka, ca), (kb, cb)| cb.cmp(ca).then_with(|| ka.cmp(kb)));
    hubs.into_iter()
        .take(5)
        .map(|((name, ver), c)| (format_pkg_ver(name, ver, tree.style), c))
        .collect()
}

/// Collect Gradle `requested -> resolved` conflict upgrades across the entire tree
/// (including deep transitive nodes) so none are hidden when subtrees fold.
fn collect_version_resolutions(tree: &ParsedTree) -> Vec<(String, String, String, usize)> {
    let mut seen: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    for n in &tree.nodes {
        if let Some(req) = &n.requested_version
            && !n.version.is_empty()
            && req != &n.version
        {
            seen.entry((n.name.clone(), req.clone(), n.version.clone()))
                .and_modify(|first| *first = (*first).min(n.line))
                .or_insert(n.line);
        }
    }
    seen.into_iter()
        .map(|((name, req, res), line)| (name, req, res, line))
        .collect()
}

// ---------------------------------------------------------------------------
// Renderer A: Forward dependency trees
// ---------------------------------------------------------------------------

fn render_forward_tree(tree: &ParsedTree, raw_len: usize, budget: usize) -> Option<String> {
    let unique_pkgs = collect_unique_packages(tree);
    let deduped = count_deduped_nodes(tree);
    let dups = collect_tree_duplicates(tree);
    let hubs = collect_shared_hubs(tree);
    let resolutions = if tree.style == DisplayStyle::Gradle {
        collect_version_resolutions(tree)
    } else {
        Vec::new()
    };

    let header = build_forward_header(tree, unique_pkgs, deduped);
    let problems_block = format_problems(&tree.problems);
    let dups_multi = format_duplicates_multiline(&dups);
    let dups_inline = format_duplicates_inline(&dups);
    let resolutions_block = format_resolutions(&resolutions);
    let hubs_line = if hubs.is_empty() {
        String::new()
    } else {
        let items: Vec<String> = hubs
            .iter()
            .map(|(lbl, c)| format!("{lbl} (×{c})"))
            .collect();
        format!("shared hubs: {}\n", items.join(", "))
    };

    let direct_lines = build_forward_direct_lines(tree);

    // Tier 1: full report (header + problems + multiline dups + resolutions + all direct deps + shared hubs).
    let mut tier1 = String::new();
    tier1.push_str(&header);
    tier1.push_str(&problems_block);
    tier1.push_str(&dups_multi);
    tier1.push_str(&resolutions_block);
    for l in &direct_lines {
        tier1.push_str(&l.text);
    }
    tier1.push_str(&hubs_line);
    if let Some(out) = fit_smaller(tier1, raw_len, budget) {
        return Some(out);
    }

    // Tier 2: omit shared hubs bonus line.
    if !hubs_line.is_empty() {
        let mut tier2 = String::new();
        tier2.push_str(&header);
        tier2.push_str(&problems_block);
        tier2.push_str(&dups_multi);
        tier2.push_str(&resolutions_block);
        for l in &direct_lines {
            tier2.push_str(&l.text);
        }
        if let Some(out) = fit_smaller(tier2, raw_len, budget) {
            return Some(out);
        }
    }

    // Tier 3: compact inline duplicates + all direct deps.
    if !dups_inline.is_empty() {
        let mut tier3 = String::new();
        tier3.push_str(&header);
        tier3.push_str(&problems_block);
        tier3.push_str(&dups_inline);
        tier3.push_str(&resolutions_block);
        for l in &direct_lines {
            tier3.push_str(&l.text);
        }
        if let Some(out) = fit_smaller(tier3, raw_len, budget) {
            return Some(out);
        }
    }

    // Never drop entries just to beat `raw_len` when raw stdout already fits in `budget`.
    if budget >= raw_len {
        return None;
    }

    // Tier 4: keep header + 100% problems + 100% duplicates (inline), and fit as many
    // direct dependency lines as possible, folding the tail with an exact `[stdout:A-B]`.
    let fixed = format!("{header}{problems_block}{dups_inline}{resolutions_block}");
    if let Some(out) = fit_truncated_entries(&fixed, &direct_lines, "more entries", raw_len, budget)
    {
        return Some(out);
    }

    // Tier 5: also fold resolutions if Gradle had dozens of them, while still keeping
    // 100% of problems and 100% of duplicates.
    if !resolutions_block.is_empty() {
        let fixed_min = format!("{header}{problems_block}{dups_inline}");
        if let Some(out) =
            fit_truncated_entries(&fixed_min, &direct_lines, "more entries", raw_len, budget)
        {
            return Some(out);
        }
    }

    None
}

fn build_forward_header(tree: &ParsedTree, unique_pkgs: usize, deduped: usize) -> String {
    let mut tail = format!("{} lines, depth {}", tree.tree_lines, tree.max_depth);
    if deduped > 0 {
        let _ = write!(tail, ", {deduped} deduped");
    }

    if tree.config_roots {
        let subject = tree.project_title.as_deref().unwrap_or("dependencies");
        let empty_note = if tree.empty_configs > 0 {
            format!(", {} empty", tree.empty_configs)
        } else {
            String::new()
        };
        return format!(
            "{}: {subject} ({} configurations{empty_note}) · {unique_pkgs} unique packages ({tail})\n",
            tree.tool_label,
            tree.roots.len()
        );
    }

    if tree.package_forest {
        return format!(
            "{}: {} top-level · {unique_pkgs} unique packages ({tail})\n",
            tree.tool_label,
            tree.roots.len()
        );
    }

    if tree.roots.len() == 1 {
        let root = &tree.nodes[tree.roots[0]];
        let root_lbl = format_pkg_ver(&root.name, &root.version, tree.style);
        return format!(
            "{}: {root_lbl} · {unique_pkgs} unique packages ({tail})\n",
            tree.tool_label
        );
    }

    let preview: Vec<String> = tree
        .roots
        .iter()
        .take(4)
        .map(|&r| {
            let n = &tree.nodes[r];
            format_pkg_ver(&n.name, &n.version, tree.style)
        })
        .collect();
    let extra = if tree.roots.len() > 4 {
        format!(", +{} more", tree.roots.len() - 4)
    } else {
        String::new()
    };
    format!(
        "{}: {} roots ({}{extra}) · {unique_pkgs} unique packages ({tail})\n",
        tree.tool_label,
        tree.roots.len(),
        preview.join(", ")
    )
}

fn format_resolutions(resolutions: &[(String, String, String, usize)]) -> String {
    if resolutions.is_empty() {
        return String::new();
    }
    let items: Vec<String> = resolutions
        .iter()
        .map(|(name, req, res, line)| format!("{name} ({req} -> {res} [stdout:{line}])"))
        .collect();
    format!(
        "version resolutions ({}): {}\n",
        resolutions.len(),
        items.join(", ")
    )
}

struct RenderedEntry {
    text: String,
    first_line: usize,
    last_line: usize,
    /// True for group headers (e.g. `direct dependencies (12):`) that don't count as omitted items.
    is_header: bool,
}

fn build_forward_direct_lines(tree: &ParsedTree) -> Vec<RenderedEntry> {
    let mut entries: Vec<RenderedEntry> = Vec::new();

    if tree.package_forest {
        let first = tree.roots.first().map_or(1, |&r| tree.nodes[r].line);
        let last = tree
            .roots
            .last()
            .map_or(first, |&r| tree.nodes[r].subtree_last_line);
        entries.push(RenderedEntry {
            text: format!("packages ({}):\n", tree.roots.len()),
            first_line: first,
            last_line: last,
            is_header: true,
        });
        for &root_idx in &tree.roots {
            entries.push(render_direct_dep_entry(tree, root_idx));
        }
        return entries;
    }

    if tree.config_roots {
        // Deduplicate Gradle configurations whose direct dependency set is identical.
        type ConfigSig = Vec<(String, String, Option<String>, usize)>;
        let mut seen_sigs: HashMap<ConfigSig, String> = HashMap::new();
        for &root_idx in &tree.roots {
            let root = &tree.nodes[root_idx];
            let sig: ConfigSig = root
                .children
                .iter()
                .map(|&c| {
                    let cn = &tree.nodes[c];
                    (
                        cn.name.clone(),
                        cn.version.clone(),
                        cn.requested_version.clone(),
                        count_unique_transitive(tree, c),
                    )
                })
                .collect();
            if let Some(prev_cfg) = seen_sigs.get(&sig) {
                entries.push(RenderedEntry {
                    text: format!(
                        "{}: same as {prev_cfg} {}\n",
                        root.name,
                        stdout_ref(root.line, root.subtree_last_line)
                    ),
                    first_line: root.line,
                    last_line: root.subtree_last_line,
                    is_header: false,
                });
                continue;
            }
            seen_sigs.insert(sig, root.name.clone());
            entries.push(RenderedEntry {
                text: format!(
                    "{} ({} direct) {}:\n",
                    root.name,
                    root.children.len(),
                    stdout_ref(root.line, root.subtree_last_line)
                ),
                first_line: root.line,
                last_line: root.subtree_last_line,
                is_header: true,
            });
            for &child_idx in &root.children {
                entries.push(render_direct_dep_entry(tree, child_idx));
            }
        }
        return entries;
    }

    if tree.roots.len() == 1 {
        let root = &tree.nodes[tree.roots[0]];
        let direct: Vec<usize> = root
            .children
            .iter()
            .copied()
            .filter(|&c| {
                tree.nodes[c].status_tag.as_deref() != Some("unmet")
                    && tree.nodes[c].status_tag.as_deref() != Some("problem")
            })
            .collect();
        if !direct.is_empty() {
            entries.push(RenderedEntry {
                text: format!("direct dependencies ({}):\n", direct.len()),
                first_line: root.line,
                last_line: root.subtree_last_line,
                is_header: true,
            });
            for child_idx in direct {
                entries.push(render_direct_dep_entry(tree, child_idx));
            }
        }
        return entries;
    }

    // Multiple workspace roots (e.g. `cargo tree --workspace` or `pnpm -r list`).
    for &root_idx in &tree.roots {
        let root = &tree.nodes[root_idx];
        let direct: Vec<usize> = root
            .children
            .iter()
            .copied()
            .filter(|&c| {
                tree.nodes[c].status_tag.as_deref() != Some("unmet")
                    && tree.nodes[c].status_tag.as_deref() != Some("problem")
            })
            .collect();
        let root_lbl = format_pkg_ver(&root.name, &root.version, tree.style);
        entries.push(RenderedEntry {
            text: format!(
                "{root_lbl} ({} direct) {}:\n",
                direct.len(),
                stdout_ref(root.line, root.subtree_last_line)
            ),
            first_line: root.line,
            last_line: root.subtree_last_line,
            is_header: true,
        });
        for child_idx in direct {
            entries.push(render_direct_dep_entry(tree, child_idx));
        }
    }
    entries
}

fn render_direct_dep_entry(tree: &ParsedTree, node_idx: usize) -> RenderedEntry {
    let node = &tree.nodes[node_idx];
    let label = format_direct_node_label(node, tree.style);
    let trans = count_unique_transitive(tree, node_idx);
    let trans_str = if trans > 0 {
        format!(" (+{trans} transitive)")
    } else {
        String::new()
    };
    let ptr = stdout_ref(node.line, node.subtree_last_line);
    RenderedEntry {
        text: format!("  {label}{trans_str} {ptr}\n"),
        first_line: node.line,
        last_line: node.subtree_last_line,
        is_header: false,
    }
}

fn fit_truncated_entries(
    fixed: &str,
    entries: &[RenderedEntry],
    more_label: &str,
    raw_len: usize,
    budget: usize,
) -> Option<String> {
    let limit = budget.min(raw_len.saturating_sub(1));
    if fixed.len() >= limit {
        return None;
    }
    let mut out = fixed.to_owned();
    let mut shown_idx = 0;
    for (i, entry) in entries.iter().enumerate() {
        let rest_entries = &entries[i..];
        let rest_items = rest_entries.iter().filter(|e| !e.is_header).count();
        let footer_reserve = if rest_items > 0 { 64 } else { 0 };
        if out.len() + entry.text.len() + footer_reserve > limit {
            break;
        }
        out.push_str(&entry.text);
        shown_idx = i + 1;
    }

    if shown_idx < entries.len() {
        let rest = &entries[shown_idx..];
        let omitted = rest.iter().filter(|e| !e.is_header).count();
        if omitted > 0 {
            let first = rest.iter().map(|e| e.first_line).min().unwrap_or(1);
            let last = rest.iter().map(|e| e.last_line).max().unwrap_or(first);
            let tail = format!("  +{omitted} {more_label} {}\n", stdout_ref(first, last));
            if out.len() + tail.len() <= limit {
                out.push_str(&tail);
            } else {
                return None;
            }
        }
    }
    fit_smaller(out, raw_len, budget)
}

// ---------------------------------------------------------------------------
// Renderer B: Inverted / reverse trees (`cargo tree -i`, `cargo tree -d`, `pipdeptree -r`)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ReverseHop {
    section: Option<String>,
    label: String,
}

impl ReverseHop {
    fn display(&self) -> String {
        match &self.section {
            Some(sec) => format!("[{sec}] {}", self.label),
            None => self.label.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct ReverseChain {
    hops: Vec<ReverseHop>,
    first_line: usize,
    last_line: usize,
    count: usize,
}

#[derive(Clone, Debug)]
enum GroupedChain {
    Single {
        hops: Vec<ReverseHop>,
        count: usize,
        first_line: usize,
        last_line: usize,
    },
    /// `prefix -> {leaf1, leaf2, ...}`
    SharedPrefix {
        prefix: Vec<ReverseHop>,
        leaves: Vec<ReverseHop>,
        total_paths: usize,
        first_line: usize,
        last_line: usize,
    },
    /// `[section] {first1, first2, ...} -> suffix`
    SharedSuffix {
        section: Option<String>,
        firsts: Vec<String>,
        suffix: Vec<ReverseHop>,
        total_paths: usize,
        first_line: usize,
        last_line: usize,
    },
}

impl GroupedChain {
    fn first_line(&self) -> usize {
        match self {
            Self::Single { first_line, .. }
            | Self::SharedPrefix { first_line, .. }
            | Self::SharedSuffix { first_line, .. } => *first_line,
        }
    }

    fn last_line(&self) -> usize {
        match self {
            Self::Single { last_line, .. }
            | Self::SharedPrefix { last_line, .. }
            | Self::SharedSuffix { last_line, .. } => *last_line,
        }
    }

    fn render(&self, max_brace_items: Option<usize>) -> String {
        match self {
            Self::Single {
                hops,
                count,
                first_line,
                last_line,
            } => {
                let path = hops
                    .iter()
                    .map(ReverseHop::display)
                    .collect::<Vec<_>>()
                    .join(" -> ");
                let mult = if *count > 1 {
                    format!(" (×{count})")
                } else {
                    String::new()
                };
                format!("  {path}{mult} {}\n", stdout_ref(*first_line, *last_line))
            }
            Self::SharedPrefix {
                prefix,
                leaves,
                total_paths,
                first_line,
                last_line,
            } => {
                let pref = prefix
                    .iter()
                    .map(ReverseHop::display)
                    .collect::<Vec<_>>()
                    .join(" -> ");
                let leaf_strs: Vec<String> = leaves.iter().map(ReverseHop::display).collect();
                let group = format_brace_list(&leaf_strs, max_brace_items, *total_paths);
                format!(
                    "  {pref} -> {group} {}\n",
                    stdout_ref(*first_line, *last_line)
                )
            }
            Self::SharedSuffix {
                section,
                firsts,
                suffix,
                total_paths,
                first_line,
                last_line,
            } => {
                let sec_prefix = match section {
                    Some(sec) => format!("[{sec}] "),
                    None => String::new(),
                };
                let group = format_brace_list(firsts, max_brace_items, *total_paths);
                let suf = suffix
                    .iter()
                    .map(ReverseHop::display)
                    .collect::<Vec<_>>()
                    .join(" -> ");
                format!(
                    "  {sec_prefix}{group} -> {suf} {}\n",
                    stdout_ref(*first_line, *last_line)
                )
            }
        }
    }
}

fn format_brace_list(items: &[String], max_items: Option<usize>, total_paths: usize) -> String {
    let cap = max_items.unwrap_or(items.len());
    let shown: Vec<&str> = items.iter().take(cap).map(String::as_str).collect();
    let count_note = if total_paths > items.len() {
        format!(" ({total_paths} paths)")
    } else if items.len() > cap {
        format!(" (+{} more)", items.len() - cap)
    } else {
        format!(" (×{})", items.len())
    };
    if items.len() > cap {
        format!("{{{}, +{} more}}", shown.join(", "), items.len() - cap)
    } else {
        format!("{{{}}}{count_note}", shown.join(", "))
    }
}

fn render_inverted_tree(tree: &ParsedTree, raw_len: usize, budget: usize) -> Option<String> {
    let unique_pkgs = collect_unique_packages(tree);
    let dups = collect_tree_duplicates(tree);

    // Group root blocks by `(name, version)` while preserving first-seen order
    // (real `cargo tree -i` can print multiple blocks for the same crate version).
    let mut target_order: Vec<(String, String)> = Vec::new();
    let mut target_roots: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for &r in &tree.roots {
        let n = &tree.nodes[r];
        let key = (n.name.clone(), n.version.clone());
        if !target_roots.contains_key(&key) {
            target_order.push(key.clone());
        }
        target_roots.entry(key).or_default().push(r);
    }

    let mut total_paths = 0usize;
    let mut per_target: Vec<(String, usize, usize, Vec<GroupedChain>)> = Vec::new();
    for key in &target_order {
        let r_list = &target_roots[key];
        let mut raw_chains: Vec<ReverseChain> = Vec::new();
        let mut first_line = usize::MAX;
        let mut last_line = 0usize;
        for &r in r_list {
            let rn = &tree.nodes[r];
            first_line = first_line.min(rn.line);
            last_line = last_line.max(rn.subtree_last_line);
            collect_reverse_chains(tree, r, &mut raw_chains);
        }
        total_paths += raw_chains.len();
        let grouped = group_reverse_chains(raw_chains);
        let label = format_pkg_ver(&key.0, &key.1, tree.style);
        per_target.push((label, first_line, last_line, grouped));
    }

    let header = if per_target.len() == 1 {
        format!(
            "{}: {} · {unique_pkgs} unique, {total_paths} reverse paths ({} lines, depth {})\n",
            tree.tool_label, per_target[0].0, tree.tree_lines, tree.max_depth
        )
    } else {
        format!(
            "{}: {} targets · {unique_pkgs} unique, {total_paths} reverse paths ({} lines, depth {})\n",
            tree.tool_label,
            per_target.len(),
            tree.tree_lines,
            tree.max_depth
        )
    };

    let problems_block = format_problems(&tree.problems);
    let dups_multi = format_duplicates_multiline(&dups);
    let dups_inline = format_duplicates_inline(&dups);

    // Tier 1: full grouped reverse chains (all branch names preserved).
    let entries_full = build_inverted_entries(&per_target, None);
    let mut tier1 = format!("{header}{problems_block}{dups_multi}");
    for e in &entries_full {
        tier1.push_str(&e.text);
    }
    if let Some(out) = fit_smaller(tier1, raw_len, budget) {
        return Some(out);
    }

    // Tier 2: compact inline duplicates + full grouped reverse chains.
    if !dups_inline.is_empty() {
        let mut tier2 = format!("{header}{problems_block}{dups_inline}");
        for e in &entries_full {
            tier2.push_str(&e.text);
        }
        if let Some(out) = fit_smaller(tier2, raw_len, budget) {
            return Some(out);
        }
    }

    // Never truncate reverse paths just to beat `raw_len` when raw already fits in `budget`.
    if budget >= raw_len {
        return None;
    }

    // Tier 3: compact inline duplicates + cap brace lists at 4 items.
    let entries_cap4 = build_inverted_entries(&per_target, Some(4));
    let mut tier3 = format!("{header}{problems_block}{dups_inline}");
    for e in &entries_cap4 {
        tier3.push_str(&e.text);
    }
    if let Some(out) = fit_smaller(tier3, raw_len, budget) {
        return Some(out);
    }

    // Tier 4: cap brace lists at 2 items and truncate tail paths with `[stdout:A-B]`.
    let entries_cap2 = build_inverted_entries(&per_target, Some(2));
    let fixed = format!("{header}{problems_block}{dups_inline}");
    fit_truncated_entries(
        &fixed,
        &entries_cap2,
        "more reverse path groups",
        raw_len,
        budget,
    )
}

fn build_inverted_entries(
    per_target: &[(String, usize, usize, Vec<GroupedChain>)],
    max_brace_items: Option<usize>,
) -> Vec<RenderedEntry> {
    let mut entries = Vec::new();
    for (label, first, last, chains) in per_target {
        entries.push(RenderedEntry {
            text: format!("{label} {}:\n", stdout_ref(*first, *last)),
            first_line: *first,
            last_line: *last,
            is_header: true,
        });
        for ch in chains {
            entries.push(RenderedEntry {
                text: ch.render(max_brace_items),
                first_line: ch.first_line(),
                last_line: ch.last_line(),
                is_header: false,
            });
        }
    }
    entries
}

fn collect_reverse_chains(tree: &ParsedTree, root_idx: usize, out: &mut Vec<ReverseChain>) {
    let mut path_hops: Vec<ReverseHop> = Vec::new();
    let mut path_lines: Vec<usize> = Vec::new();
    for &child in &tree.nodes[root_idx].children {
        dfs_reverse_chains(tree, child, &mut path_hops, &mut path_lines, out);
    }
}

fn dfs_reverse_chains(
    tree: &ParsedTree,
    node_idx: usize,
    path_hops: &mut Vec<ReverseHop>,
    path_lines: &mut Vec<usize>,
    out: &mut Vec<ReverseChain>,
) {
    let node = &tree.nodes[node_idx];
    let mut label = format_pkg_ver(&node.name, &node.version, tree.style);
    if node.deduped {
        label.push_str(" (*)");
    }
    path_hops.push(ReverseHop {
        section: node.section.clone(),
        label,
    });
    path_lines.push(node.line);

    if node.children.is_empty() {
        let first_line = *path_lines.iter().min().unwrap_or(&node.line);
        let last_line = *path_lines.iter().max().unwrap_or(&node.line);
        out.push(ReverseChain {
            hops: path_hops.clone(),
            first_line,
            last_line,
            count: 1,
        });
    } else {
        for &child in &node.children {
            dfs_reverse_chains(tree, child, path_hops, path_lines, out);
        }
    }

    path_hops.pop();
    path_lines.pop();
}

/// Deduplicate reverse chains in three semantic-preserving passes:
/// 1. Exact duplicate chains -> `×count` with merged `[stdout:first-last]`.
/// 2. Chains sharing identical prefix `hops[..len-1]` -> `prefix -> {leaf1, leaf2, ...}`.
/// 3. Remaining chains sharing identical `(first.section, hops[1..])` -> `{first1, first2, ...} -> suffix`.
fn group_reverse_chains(raw: Vec<ReverseChain>) -> Vec<GroupedChain> {
    // Pass 1: exact dedup preserving order of first appearance.
    let mut deduped: Vec<ReverseChain> = Vec::new();
    let mut exact_map: HashMap<Vec<ReverseHop>, usize> = HashMap::new();
    for ch in raw {
        if let Some(&pos) = exact_map.get(&ch.hops) {
            deduped[pos].count += ch.count;
            deduped[pos].first_line = deduped[pos].first_line.min(ch.first_line);
            deduped[pos].last_line = deduped[pos].last_line.max(ch.last_line);
        } else {
            exact_map.insert(ch.hops.clone(), deduped.len());
            deduped.push(ch);
        }
    }

    // Pass 2: group chains with identical prefix `hops[..len-1]` (when len >= 2 and >= 2 chains).
    let mut prefix_counts: HashMap<&[ReverseHop], usize> = HashMap::new();
    for ch in &deduped {
        if ch.hops.len() >= 2 {
            *prefix_counts
                .entry(&ch.hops[..ch.hops.len() - 1])
                .or_default() += 1;
        }
    }
    let mut prefix_emitted: HashSet<Vec<ReverseHop>> = HashSet::new();
    let mut after_prefix: Vec<GroupedChain> = Vec::new();
    let mut ungrouped_indices: Vec<usize> = Vec::new();

    for (idx, ch) in deduped.iter().enumerate() {
        if ch.hops.len() >= 2
            && prefix_counts
                .get(&ch.hops[..ch.hops.len() - 1])
                .copied()
                .unwrap_or(0)
                >= 2
        {
            let key = ch.hops[..ch.hops.len() - 1].to_vec();
            if prefix_emitted.insert(key.clone()) {
                let mut leaves = Vec::new();
                let mut total_paths = 0usize;
                let mut first_line = usize::MAX;
                let mut last_line = 0usize;
                for other in &deduped {
                    if other.hops.len() >= 2 && other.hops[..other.hops.len() - 1] == key[..] {
                        leaves.push(other.hops[other.hops.len() - 1].clone());
                        total_paths += other.count;
                        first_line = first_line.min(other.first_line);
                        last_line = last_line.max(other.last_line);
                    }
                }
                after_prefix.push(GroupedChain::SharedPrefix {
                    prefix: key,
                    leaves,
                    total_paths,
                    first_line,
                    last_line,
                });
            }
        } else {
            ungrouped_indices.push(idx);
        }
    }

    // Pass 3: among remaining chains of len >= 2, group by identical `(first.section, hops[1..])`.
    type SuffixKey = (Option<String>, Vec<ReverseHop>);
    let mut suffix_counts: HashMap<SuffixKey, usize> = HashMap::new();
    for &idx in &ungrouped_indices {
        let ch = &deduped[idx];
        if ch.hops.len() >= 2 {
            let key = (ch.hops[0].section.clone(), ch.hops[1..].to_vec());
            *suffix_counts.entry(key).or_default() += 1;
        }
    }
    let mut suffix_emitted: HashSet<SuffixKey> = HashSet::new();
    let mut result = after_prefix;

    for &idx in &ungrouped_indices {
        let ch = &deduped[idx];
        if ch.hops.len() >= 2 {
            let key = (ch.hops[0].section.clone(), ch.hops[1..].to_vec());
            if suffix_counts.get(&key).copied().unwrap_or(0) >= 2 {
                if suffix_emitted.insert(key.clone()) {
                    let mut firsts = Vec::new();
                    let mut total_paths = 0usize;
                    let mut first_line = usize::MAX;
                    let mut last_line = 0usize;
                    for &oidx in &ungrouped_indices {
                        let other = &deduped[oidx];
                        if other.hops.len() >= 2
                            && other.hops[0].section == key.0
                            && other.hops[1..] == key.1[..]
                        {
                            firsts.push(other.hops[0].label.clone());
                            total_paths += other.count;
                            first_line = first_line.min(other.first_line);
                            last_line = last_line.max(other.last_line);
                        }
                    }
                    result.push(GroupedChain::SharedSuffix {
                        section: key.0,
                        firsts,
                        suffix: key.1,
                        total_paths,
                        first_line,
                        last_line,
                    });
                }
                continue;
            }
        }
        result.push(GroupedChain::Single {
            hops: ch.hops.clone(),
            count: ch.count,
            first_line: ch.first_line,
            last_line: ch.last_line,
        });
    }

    // Preserve natural stdout order by first_line.
    result.sort_by_key(GroupedChain::first_line);
    result
}

// ---------------------------------------------------------------------------
// Renderer C: `go mod graph`
// ---------------------------------------------------------------------------

fn render_go_graph(graph: &ParsedGoGraph, raw_len: usize, budget: usize) -> Option<String> {
    let mut unique_mods: HashSet<&str> = HashSet::new();
    let mut versioned_nodes: HashSet<(&str, &str)> = HashSet::new();
    let mut by_mod: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();

    type NodeKey<'a> = (&'a str, Option<&'a str>);
    let mut adj: HashMap<NodeKey<'_>, Vec<NodeKey<'_>>> = HashMap::new();
    let mut in_parents: HashMap<NodeKey<'_>, (HashSet<NodeKey<'_>>, usize, usize)> = HashMap::new();

    for e in &graph.edges {
        unique_mods.insert(e.parent_name.as_str());
        unique_mods.insert(e.child_name.as_str());
        if let Some(pv) = e.parent_ver.as_deref() {
            versioned_nodes.insert((e.parent_name.as_str(), pv));
            by_mod
                .entry(e.parent_name.as_str())
                .or_default()
                .entry(pv)
                .and_modify(|f| *f = (*f).min(e.line))
                .or_insert(e.line);
        }
        if let Some(cv) = e.child_ver.as_deref() {
            versioned_nodes.insert((e.child_name.as_str(), cv));
            by_mod
                .entry(e.child_name.as_str())
                .or_default()
                .entry(cv)
                .and_modify(|f| *f = (*f).min(e.line))
                .or_insert(e.line);
        }

        let pk = (e.parent_name.as_str(), e.parent_ver.as_deref());
        let ck = (e.child_name.as_str(), e.child_ver.as_deref());
        adj.entry(pk).or_default().push(ck);
        let entry = in_parents
            .entry(ck)
            .or_insert_with(|| (HashSet::new(), e.line, e.line));
        entry.0.insert(pk);
        entry.1 = entry.1.min(e.line);
        entry.2 = entry.2.max(e.line);
    }

    // Multi-version modules.
    let mut dups: Vec<DuplicateEntry> = Vec::new();
    for (mod_name, vers) in by_mod {
        if vers.len() >= 2 {
            let mut list: Vec<(String, usize)> =
                vers.into_iter().map(|(v, l)| (v.to_owned(), l)).collect();
            list.sort_by(|(va, _), (vb, _)| version_cmp(va, vb));
            dups.push(DuplicateEntry {
                name: mod_name.to_owned(),
                versions: list,
            });
        }
    }

    // Root direct edges.
    let root_set: HashSet<&str> = graph.roots.iter().map(String::as_str).collect();
    let direct_edges: Vec<&GoEdge> = graph
        .edges
        .iter()
        .filter(|e| e.parent_ver.is_none() && root_set.contains(e.parent_name.as_str()))
        .collect();

    // Reachable transitive module count per direct edge target.
    let mut direct_entries: Vec<RenderedEntry> = Vec::new();
    if !direct_edges.is_empty() {
        let first = direct_edges.first().map_or(1, |e| e.line);
        let last = direct_edges.last().map_or(first, |e| e.line);
        direct_entries.push(RenderedEntry {
            text: format!("direct dependencies ({}):\n", direct_edges.len()),
            first_line: first,
            last_line: last,
            is_header: true,
        });
        for e in &direct_edges {
            let start = (e.child_name.as_str(), e.child_ver.as_deref());
            let reach = bfs_reachable_count(&adj, start);
            let lbl = match &e.child_ver {
                Some(v) => format!("{}@{v}", e.child_name),
                None => e.child_name.clone(),
            };
            let reach_str = if reach > 0 {
                format!(" (+{reach} transitive)")
            } else {
                String::new()
            };
            direct_entries.push(RenderedEntry {
                text: format!("  {lbl}{reach_str} [stdout:{}]\n", e.line),
                first_line: e.line,
                last_line: e.line,
                is_header: false,
            });
        }
    }

    // Fan-in hubs (modules depended on by >= 2 distinct parent modules).
    let mut hubs: Vec<(String, usize, usize, usize)> = in_parents
        .into_iter()
        .filter(|(_, (parents, _, _))| parents.len() >= 2)
        .map(|((name, ver), (parents, first, last))| {
            let lbl = match ver {
                Some(v) => format!("{name}@{v}"),
                None => name.to_owned(),
            };
            (lbl, parents.len(), first, last)
        })
        .collect();
    hubs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let root_summary = match graph.roots.as_slice() {
        [] => "graph".to_owned(),
        [only] => only.clone(),
        many => format!("{} roots ({}, ...)", many.len(), many[0]),
    };
    let header = format!(
        "go mod graph: {root_summary} · {} unique modules ({} versions, {} edges)\n",
        unique_mods.len(),
        versioned_nodes.len(),
        graph.edges.len()
    );
    let problems_block = format_problems(&graph.problems);
    let dups_multi = format_duplicates_multiline(&dups);
    let dups_inline = format_duplicates_inline(&dups);

    let format_hubs = |limit: usize| -> String {
        if hubs.is_empty() || limit == 0 {
            return String::new();
        }
        let shown = hubs.len().min(limit);
        let mut s = format!("fan-in hubs ({} total, top {shown}):\n", hubs.len());
        for (lbl, deg, first, last) in hubs.iter().take(shown) {
            let _ = writeln!(
                s,
                "  {lbl} ({deg} dependents) {}",
                stdout_ref(*first, *last)
            );
        }
        s
    };

    // Tier 1: full (header + problems + multiline dups + all direct deps + top 8 hubs).
    let mut tier1 = format!("{header}{problems_block}{dups_multi}");
    for e in &direct_entries {
        tier1.push_str(&e.text);
    }
    tier1.push_str(&format_hubs(8));
    if let Some(out) = fit_smaller(tier1, raw_len, budget) {
        return Some(out);
    }

    // Tier 2: compact inline dups + all direct deps + top 4 hubs.
    let mut tier2 = format!("{header}{problems_block}{dups_inline}");
    for e in &direct_entries {
        tier2.push_str(&e.text);
    }
    tier2.push_str(&format_hubs(4));
    if let Some(out) = fit_smaller(tier2, raw_len, budget) {
        return Some(out);
    }

    // Never drop direct dependencies just to beat `raw_len` when raw already fits in `budget`.
    if budget >= raw_len {
        return None;
    }

    // Tier 3: compact inline dups + truncated direct deps + top 3 hubs (or without hubs).
    let hubs_top3 = format_hubs(3);
    let fixed_with_hubs = format!("{header}{problems_block}{dups_inline}{hubs_top3}");
    if let Some(out) = fit_truncated_entries(
        &fixed_with_hubs,
        &direct_entries,
        "more direct dependencies",
        raw_len,
        budget,
    ) {
        return Some(out);
    }

    let fixed_no_hubs = format!("{header}{problems_block}{dups_inline}");
    fit_truncated_entries(
        &fixed_no_hubs,
        &direct_entries,
        "more direct dependencies",
        raw_len,
        budget,
    )
}

type GoNodeKey<'a> = (&'a str, Option<&'a str>);
type GoAdj<'a> = HashMap<GoNodeKey<'a>, Vec<GoNodeKey<'a>>>;

fn bfs_reachable_count<'a>(adj: &GoAdj<'a>, start: GoNodeKey<'a>) -> usize {
    let mut visited: HashSet<GoNodeKey<'a>> = HashSet::new();
    let mut queue: VecDeque<GoNodeKey<'a>> = VecDeque::new();
    visited.insert(start);
    queue.push_back(start);
    while let Some(curr) = queue.pop_front() {
        if let Some(neighbors) = adj.get(&curr) {
            for &next in neighbors {
                if visited.insert(next) {
                    queue.push_back(next);
                }
            }
        }
    }
    visited.len().saturating_sub(1)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn s_args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Build a realistic 340+ line `cargo tree` output with `[build-dependencies]`,
    /// `[dev-dependencies]`, `(proc-macro)`, `(*)`, and 3 multi-version duplicates
    /// (`syn`, `windows-sys`, `bitflags`).
    fn build_large_cargo_tree() -> String {
        let mut lines: Vec<String> = vec![
            "enterprise-api v0.4.2 (/home/dev/workspace/enterprise-api)".into(),
            "├── anyhow v1.0.89".into(),
            // serde subtree
            "├── serde v1.0.215".into(),
            "│   └── serde_derive v1.0.215 (proc-macro)".into(),
            "│       ├── proc-macro2 v1.0.89".into(),
            "│       │   └── unicode-ident v1.0.13".into(),
            "│       ├── quote v1.0.37".into(),
            "│       │   └── proc-macro2 v1.0.89 (*)".into(),
            "│       └── syn v2.0.87".into(),
            "│           ├── proc-macro2 v1.0.89 (*)".into(),
            "│           ├── quote v1.0.37 (*)".into(),
            "│           └── unicode-ident v1.0.13 (*)".into(),
            // legacy-macro pulling syn v1.0.109 (duplicate #1!)
            "├── legacy-macro v0.3.1 (proc-macro)".into(),
            "│   ├── proc-macro2 v1.0.89 (*)".into(),
            "│   ├── quote v1.0.37 (*)".into(),
            "│   └── syn v1.0.109".into(),
            "│       ├── proc-macro2 v1.0.89 (*)".into(),
            "│       ├── quote v1.0.37 (*)".into(),
            "│       └── unicode-ident v1.0.13 (*)".into(),
            // clap pulling bitflags v1.3.2 (duplicate #2!)
            "├── clap v3.2.25".into(),
            "│   ├── bitflags v1.3.2".into(),
            "│   ├── clap_lex v0.2.4".into(),
            "│   │   └── os_str_bytes v6.6.1".into(),
            "│   ├── indexmap v1.9.3".into(),
            "│   │   ├── hashbrown v0.12.3".into(),
            "│   │   [build-dependencies]".into(),
            "│   │   └── autocfg v1.4.0".into(),
            "│   └── textwrap v0.16.1".into(),
            // tokio pulling windows-sys v0.52.0 & bitflags v2.6.0
            "├── tokio v1.40.0".into(),
            "│   ├── bytes v1.7.2".into(),
            "│   ├── mio v1.0.2".into(),
            "│   │   ├── libc v0.2.159".into(),
            "│   │   └── windows-sys v0.52.0".into(),
            "│   │       └── windows-targets v0.52.6".into(),
            "│   ├── parking_lot v0.12.3".into(),
            "│   │   ├── lock_api v0.4.12".into(),
            "│   │   │   └── scopeguard v1.2.0".into(),
            "│   │   └── parking_lot_core v0.9.10".into(),
            "│   │       ├── cfg-if v1.0.0".into(),
            "│   │       ├── libc v0.2.159 (*)".into(),
            "│   │       └── smallvec v1.13.2".into(),
            "│   ├── pin-project-lite v0.2.14".into(),
            "│   ├── socket2 v0.5.7".into(),
            "│   │   ├── libc v0.2.159 (*)".into(),
            "│   │   └── windows-sys v0.52.0 (*)".into(),
            "│   └── tokio-macros v2.4.0 (proc-macro)".into(),
            "│       ├── proc-macro2 v1.0.89 (*)".into(),
            "│       ├── quote v1.0.37 (*)".into(),
            "│       └── syn v2.0.87 (*)".into(),
            // reqwest pulling windows-sys v0.48.0 (duplicate #3!) and big transitive graph
            "├── reqwest v0.12.8".into(),
            "│   ├── bytes v1.7.2 (*)".into(),
            "│   ├── futures-core v0.3.31".into(),
            "│   ├── futures-util v0.3.31".into(),
            "│   │   ├── futures-core v0.3.31 (*)".into(),
            "│   │   ├── futures-task v0.3.31".into(),
            "│   │   ├── pin-project-lite v0.2.14 (*)".into(),
            "│   │   └── pin-utils v0.1.0".into(),
            "│   ├── http v1.1.0".into(),
            "│   │   ├── bytes v1.7.2 (*)".into(),
            "│   │   ├── fnv v1.0.7".into(),
            "│   │   └── itoa v1.0.11".into(),
            "│   ├── hyper v1.4.1".into(),
            "│   │   ├── bytes v1.7.2 (*)".into(),
            "│   │   ├── futures-channel v0.3.31".into(),
            "│   │   │   └── futures-core v0.3.31 (*)".into(),
            "│   │   ├── http v1.1.0 (*)".into(),
            "│   │   ├── http-body v1.0.1".into(),
            "│   │   │   ├── bytes v1.7.2 (*)".into(),
            "│   │   │   └── http v1.1.0 (*)".into(),
            "│   │   ├── httparse v1.9.5".into(),
            "│   │   ├── itoa v1.0.11 (*)".into(),
            "│   │   ├── pin-project-lite v0.2.14 (*)".into(),
            "│   │   ├── smallvec v1.13.2 (*)".into(),
            "│   │   └── tokio v1.40.0 (*)".into(),
            "│   ├── rustls v0.23.14".into(),
            "│   │   ├── once_cell v1.20.2".into(),
            "│   │   ├── ring v0.17.8".into(),
            "│   │   │   ├── cfg-if v1.0.0 (*)".into(),
            "│   │   │   ├── getrandom v0.2.15".into(),
            "│   │   │   │   ├── cfg-if v1.0.0 (*)".into(),
            "│   │   │   │   └── libc v0.2.159 (*)".into(),
            "│   │   │   ├── spin v0.9.8".into(),
            "│   │   │   ├── untrusted v0.9.0".into(),
            "│   │   │   [build-dependencies]".into(),
            "│   │   │   └── cc v1.1.30".into(),
            "│   │   ├── rustls-pki-types v1.9.0".into(),
            "│   │   ├── rustls-webpki v0.102.8".into(),
            "│   │   │   ├── ring v0.17.8 (*)".into(),
            "│   │   │   ├── rustls-pki-types v1.9.0 (*)".into(),
            "│   │   │   └── untrusted v0.9.0 (*)".into(),
            "│   │   ├── subtle v2.6.1".into(),
            "│   │   └── zeroize v1.8.1".into(),
            "│   ├── serde v1.0.215 (*)".into(),
            "│   ├── serde_json v1.0.128".into(),
            "│   │   ├── itoa v1.0.11 (*)".into(),
            "│   │   ├── memchr v2.7.4".into(),
            "│   │   ├── ryu v1.0.18".into(),
            "│   │   └── serde v1.0.215 (*)".into(),
            "│   ├── tokio v1.40.0 (*)".into(),
            "│   ├── tower v0.5.1".into(),
            "│   │   ├── futures-core v0.3.31 (*)".into(),
            "│   │   ├── futures-util v0.3.31 (*)".into(),
            "│   │   ├── pin-project-lite v0.2.14 (*)".into(),
            "│   │   ├── sync_wrapper v1.0.1".into(),
            "│   │   ├── tokio v1.40.0 (*)".into(),
            "│   │   └── tower-layer v0.3.3".into(),
            "│   ├── url v2.5.2".into(),
            "│   │   ├── form_urlencoded v1.2.1".into(),
            "│   │   │   └── percent-encoding v2.3.1".into(),
            "│   │   ├── idna v0.5.0".into(),
            "│   │   │   ├── unicode-bidi v0.3.17".into(),
            "│   │   │   └── unicode-normalization v0.1.24".into(),
            "│   │   │       └── tinyvec v1.8.0".into(),
            "│   │   │           └── tinyvec_macros v0.1.1".into(),
            "│   │   └── percent-encoding v2.3.1 (*)".into(),
            "│   └── windows-sys v0.48.0".into(),
            "│       └── windows-targets v0.48.5".into(),
        ];
        // Add 20 realistic grammar/plugin crates with repeated deep diamond subtrees to reach 340+ lines
        for i in 1..=20 {
            let connector = if i == 20 { "└──" } else { "├──" };
            let bar = if i == 20 { " " } else { "│" };
            lines.push(format!("{connector} plugin-mod-{i:02} v0.8.{i}"));
            lines.push(format!("{bar}   ├── serde v1.0.215 (*)"));
            lines.push(format!("{bar}   ├── serde_json v1.0.128 (*)"));
            lines.push(format!("{bar}   ├── thiserror v1.0.64"));
            lines.push(format!(
                "{bar}   │   └── thiserror-impl v1.0.64 (proc-macro)"
            ));
            lines.push(format!("{bar}   │       ├── proc-macro2 v1.0.89 (*)"));
            lines.push(format!("{bar}   │       ├── quote v1.0.37 (*)"));
            lines.push(format!("{bar}   │       └── syn v2.0.87 (*)"));
            lines.push(format!("{bar}   ├── tracing v0.1.40"));
            lines.push(format!("{bar}   │   ├── pin-project-lite v0.2.14 (*)"));
            lines.push(format!("{bar}   │   └── tracing-core v0.1.32"));
            lines.push(format!("{bar}   └── plugin-helper-{i:02} v0.1.0"));
            lines.push(format!("{bar}       [build-dependencies]"));
            lines.push(format!("{bar}       └── cc v1.1.30 (*)"));
        }
        lines.push("[build-dependencies]".into());
        lines.push("└── cc v1.1.30 (*)".into());
        lines.push("[dev-dependencies]".into());
        lines.push("├── tempfile v3.13.0".into());
        lines.push("│   ├── cfg-if v1.0.0 (*)".into());
        lines.push("│   ├── fastrand v2.1.1".into());
        lines.push("│   ├── once_cell v1.20.2 (*)".into());
        lines.push("│   └── rustix v0.38.37".into());
        lines.push("│       ├── bitflags v2.6.0".into());
        lines.push("│       ├── errno v0.3.9".into());
        lines.push("│       │   └── libc v0.2.159 (*)".into());
        lines.push("│       └── linux-raw-sys v0.4.14".into());
        lines.push("└── insta v1.40.0".into());
        lines.push("    ├── console v0.15.8".into());
        lines.push("    │   ├── libc v0.2.159 (*)".into());
        lines.push("    │   └── once_cell v1.20.2 (*)".into());
        lines.push("    ├── linked-hash-map v0.5.6".into());
        lines.push("    └── similar v2.6.0".into());

        lines.join("\n") + "\n"
    }

    #[test]
    fn cargo_tree_large_forward_compacts_over_85_percent_with_exact_dups_and_pointers() {
        let raw = build_large_cargo_tree();
        let line_count = raw.lines().count();
        assert!(line_count >= 350, "expected >=350 lines, got {line_count}");

        let out = format("cargo", &s_args(&["tree"]), &raw, 8_000).expect("should format");
        let reduction = 100.0 * (1.0 - (out.len() as f64 / raw.len() as f64));
        assert!(
            reduction > 85.0,
            "expected >85% reduction, got {reduction:.1}% ({} -> {} bytes)\n{out}",
            raw.len(),
            out.len()
        );
        assert!(out.len() < raw.len());
        assert!(out.len() <= 8_000);

        // Verify header has root & stats.
        assert!(out.contains("cargo tree: enterprise-api v0.4.2"));
        assert!(out.contains("deduped"));

        // Verify ALL 4 multi-version duplicates + exact first-seen line numbers!
        let find_line = |needle: &str| {
            raw.lines()
                .enumerate()
                .find(|(_, l)| l.contains(needle))
                .map(|(i, _)| i + 1)
                .unwrap()
        };
        let syn1 = find_line("syn v1.0.109");
        let syn2 = find_line("syn v2.0.87");
        let win48 = find_line("windows-sys v0.48.0");
        let win52 = find_line("windows-sys v0.52.0");
        let wt48 = find_line("windows-targets v0.48.5");
        let wt52 = find_line("windows-targets v0.52.6");
        let bf1 = find_line("bitflags v1.3.2");
        let bf2 = find_line("bitflags v2.6.0");

        assert!(out.contains("duplicates (4):"), "{out}");
        assert!(
            out.contains(&format!(
                "syn (v1.0.109 [stdout:{syn1}], v2.0.87 [stdout:{syn2}])"
            )),
            "missing syn duplicate with exact lines:\n{out}"
        );
        assert!(
            out.contains(&format!(
                "windows-sys (v0.48.0 [stdout:{win48}], v0.52.0 [stdout:{win52}])"
            )),
            "missing windows-sys duplicate:\n{out}"
        );
        assert!(
            out.contains(&format!(
                "windows-targets (v0.48.5 [stdout:{wt48}], v0.52.6 [stdout:{wt52}])"
            )),
            "missing windows-targets duplicate:\n{out}"
        );
        assert!(
            out.contains(&format!(
                "bitflags (v1.3.2 [stdout:{bf1}], v2.6.0 [stdout:{bf2}])"
            )),
            "missing bitflags duplicate:\n{out}"
        );

        // Verify exact `[stdout:A-B]` span for serde (lines 3..12) and leaf anyhow (line 2).
        assert!(out.contains("anyhow v1.0.89 [stdout:2]"), "{out}");
        assert!(
            out.contains("serde v1.0.215 (+5 transitive) [stdout:3-12]"),
            "{out}"
        );
        // Verify sections `[build]` and `[dev]` are preserved on direct deps.
        assert!(out.contains("cc v1.1.30 [build] (*)"), "{out}");
        assert!(
            out.contains("tempfile v3.13.0 [dev] (+8 transitive)"),
            "{out}"
        );
        assert!(out.contains("insta v1.40.0 [dev] (+5 transitive)"), "{out}");
    }

    #[test]
    fn cargo_tree_ascii_charset_works() {
        let raw = "\
my-app v0.1.0 (/ws/my-app)
|-- anyhow v1.0.89
|-- serde v1.0.215
|   `-- serde_derive v1.0.215 (proc-macro)
|       |-- proc-macro2 v1.0.89
|       |   `-- unicode-ident v1.0.13
|       |-- quote v1.0.37
|       |   \\-- proc-macro2 v1.0.89 (*)
|       `-- syn v2.0.87
|           |-- proc-macro2 v1.0.89 (*)
|           |-- quote v1.0.37 (*)
|           `-- unicode-ident v1.0.13 (*)
`-- tokio v1.40.0
    |-- bytes v1.7.2
    |-- mio v1.0.2
    |   `-- libc v0.2.159
    `-- pin-project-lite v0.2.14
[dev-dependencies]
\\-- tempfile v3.13.0
    |-- cfg-if v1.0.0
    `-- fastrand v2.1.1
";
        let out = format("cargo", &s_args(&["tree", "--charset", "ascii"]), raw, 4000)
            .expect("ascii cargo tree should format");
        assert!(out.len() < raw.len());
        assert!(
            out.contains("serde v1.0.215 (+5 transitive) [stdout:3-12]"),
            "{out}"
        );
        assert!(
            out.contains("tokio v1.40.0 (+4 transitive) [stdout:13-17]"),
            "{out}"
        );
        assert!(
            out.contains("tempfile v3.13.0 [dev] (+2 transitive) [stdout:19-21]"),
            "{out}"
        );
    }

    #[test]
    fn cargo_tree_inverted_preserves_full_reverse_paths_and_deduplicates_branches() {
        let raw = "\
syn v2.0.87
├── serde_derive v1.0.215 (proc-macro)
│   └── serde v1.0.215
│       ├── api-server v0.1.0 (/home/dev/enterprise-monorepo/crates/api-server)
│       ├── cli-tool v0.1.0 (/home/dev/enterprise-monorepo/crates/cli-tool)
│       └── worker-svc v0.1.0 (/home/dev/enterprise-monorepo/crates/worker-svc)
├── thiserror-impl v1.0.64 (proc-macro)
│   └── thiserror v1.0.64
│       └── api-server v0.1.0 (/home/dev/enterprise-monorepo/crates/api-server)
├── clap_derive v4.5.18 (proc-macro)
│   └── clap v4.5.20
│       └── cli-tool v0.1.0 (/home/dev/enterprise-monorepo/crates/cli-tool)
[build-dependencies]
├── tree-sitter-rust v0.23.0
│   └── api-server v0.1.0 (/home/dev/enterprise-monorepo/crates/api-server)
├── tree-sitter-python v0.25.0
│   └── api-server v0.1.0 (/home/dev/enterprise-monorepo/crates/api-server)
├── tree-sitter-go v0.25.0
│   └── api-server v0.1.0 (/home/dev/enterprise-monorepo/crates/api-server)
└── tree-sitter-bash v0.25.0
    └── api-server v0.1.0 (/home/dev/enterprise-monorepo/crates/api-server)
";
        let out = format("cargo", &s_args(&["tree", "-i", "syn"]), raw, 4000)
            .expect("inverted tree should format");
        assert!(out.len() < raw.len(), "expected smaller output:\n{out}");
        assert!(out.contains("cargo tree -i: syn v2.0.87"), "{out}");
        // Shared prefix fan-out collapsed: serde_derive -> serde -> {api-server, cli-tool, worker-svc}
        assert!(
            out.contains("serde_derive v1.0.215 -> serde v1.0.215 -> {api-server v0.1.0, cli-tool v0.1.0, worker-svc v0.1.0} (×3) [stdout:2-6]"),
            "expected shared-prefix grouping:\n{out}"
        );
        // Linear chains preserved in full
        assert!(
            out.contains(
                "thiserror-impl v1.0.64 -> thiserror v1.0.64 -> api-server v0.1.0 [stdout:7-9]"
            ),
            "{out}"
        );
        assert!(
            out.contains("clap_derive v4.5.18 -> clap v4.5.20 -> cli-tool v0.1.0 [stdout:10-12]"),
            "{out}"
        );
        // Shared suffix fan-in under [build] grouped with count and exact line range
        assert!(
            out.contains("[build] {tree-sitter-rust v0.23.0, tree-sitter-python v0.25.0, tree-sitter-go v0.25.0, tree-sitter-bash v0.25.0} (×4) -> api-server v0.1.0 [stdout:14-21]"),
            "expected shared-suffix grouping:\n{out}"
        );
    }

    #[test]
    fn cargo_tree_duplicates_flag_formats_inverted_duplicate_blocks() {
        // Tiny 9-line `-d` output where lossless report would exceed raw length returns None
        // instead of dropping targets when raw already fits in budget.
        let tiny = "\
bitflags v1.3.2
└── clap v3.2.25
    └── app v0.1.0 (/ws/app)

bitflags v2.6.0
└── rustix v0.38.37
    └── app v0.1.0 (/ws/app)
";
        assert!(format("cargo", &s_args(&["tree", "-d"]), tiny, 4000).is_none());

        // Realistic `cargo tree -d` with multiple duplicate crates and deep reverse paths.
        let raw = "\
bitflags v1.3.2
└── clap v3.2.25
    ├── my-workspace-app v0.1.0 (/home/dev/enterprise-monorepo/crates/my-workspace-app)
    ├── my-workspace-cli v0.1.0 (/home/dev/enterprise-monorepo/crates/my-workspace-cli)
    └── my-workspace-daemon v0.1.0 (/home/dev/enterprise-monorepo/crates/my-workspace-daemon)

bitflags v2.6.0
└── rustix v0.38.37
    ├── my-workspace-app v0.1.0 (/home/dev/enterprise-monorepo/crates/my-workspace-app)
    └── tempfile v3.13.0
        ├── my-workspace-app v0.1.0 (/home/dev/enterprise-monorepo/crates/my-workspace-app)
        └── my-workspace-cli v0.1.0 (/home/dev/enterprise-monorepo/crates/my-workspace-cli)

syn v1.0.109
└── legacy-macro v0.3.1 (proc-macro)
    └── my-workspace-app v0.1.0 (/home/dev/enterprise-monorepo/crates/my-workspace-app)

syn v2.0.87
├── serde_derive v1.0.215 (proc-macro)
│   └── serde v1.0.215
│       ├── my-workspace-app v0.1.0 (/home/dev/enterprise-monorepo/crates/my-workspace-app)
│       └── my-workspace-cli v0.1.0 (/home/dev/enterprise-monorepo/crates/my-workspace-cli)
└── thiserror-impl v1.0.64 (proc-macro)
    └── thiserror v1.0.64
        └── my-workspace-app v0.1.0 (/home/dev/enterprise-monorepo/crates/my-workspace-app)
";
        let out = format("cargo", &s_args(&["tree", "-d"]), raw, 4000)
            .expect("cargo tree -d should format");
        assert!(out.len() < raw.len());
        assert!(out.contains("cargo tree -d: 4 targets"), "{out}");
        assert!(
            out.contains("bitflags (v1.3.2 [stdout:1], v2.6.0 [stdout:7])"),
            "{out}"
        );
        assert!(
            out.contains("syn (v1.0.109 [stdout:14], v2.0.87 [stdout:18])"),
            "{out}"
        );
        assert!(
            out.contains("clap v3.2.25 -> {my-workspace-app v0.1.0, my-workspace-cli v0.1.0, my-workspace-daemon v0.1.0} (×3) [stdout:2-5]"),
            "{out}"
        );
        assert!(
            out.contains("rustix v0.38.37 -> tempfile v3.13.0 -> {my-workspace-app v0.1.0, my-workspace-cli v0.1.0} (×2) [stdout:8-12]"),
            "{out}"
        );
        assert!(
            out.contains("thiserror-impl v1.0.64 -> thiserror v1.0.64 -> my-workspace-app v0.1.0 [stdout:23-25]"),
            "{out}"
        );
    }

    #[test]
    fn npm_ls_pins_all_errors_at_top_and_collapses_deduped_subtrees() {
        let raw = "\
my-web-app@1.2.0 /home/dev/my-web-app
├─┬ @babel/core@7.25.2
│ ├─┬ @babel/code-frame@7.24.7
│ │ └── @babel/highlight@7.24.7
│ ├── @babel/generator@7.25.6
│ ├── @babel/parser@7.25.6
│ └── semver@6.3.1
├─┬ express@4.21.1
│ ├─┬ body-parser@1.20.3
│ │ ├── bytes@3.1.2
│ │ ├── debug@2.6.9
│ │ │ └── ms@2.0.0
│ │ └── raw-body@2.5.2
│ ├─┬ cookie@0.7.1
│ ├── debug@2.6.9 deduped
│ └── semver@7.6.3
├── UNMET PEER DEPENDENCY react@^18.0.0
├── UNMET DEPENDENCY react-dom@^18.0.0
├── lodash@4.17.21 deduped
├── chalk@5.3.0 extraneous
├── bad-pkg@1.0.0 invalid: \"^2.0.0\" from the root project
├── @org/shared@0.1.0 -> ./packages/shared
└── legacy-pkg@0.9.0 missing
npm ERR! missing: react-dom@^18.0.0, required by my-web-app@1.2.0
npm ERR! extraneous: chalk@5.3.0 /home/dev/my-web-app/node_modules/chalk
";
        let out =
            format("npm", &s_args(&["ls", "--all"]), raw, 4000).expect("npm ls should format");
        assert!(out.len() < raw.len());

        // Problems must appear BEFORE direct dependencies and duplicates!
        let prob_pos = out.find("problems (").expect("problems section missing");
        let dups_pos = out
            .find("duplicates (")
            .expect("duplicates section missing");
        let direct_pos = out
            .find("direct dependencies (")
            .expect("direct deps missing");
        assert!(prob_pos < dups_pos && dups_pos < direct_pos, "{out}");

        // Every single error / problem line pinned with exact line number!
        assert!(
            out.contains("UNMET PEER DEPENDENCY react@^18.0.0 [stdout:17]"),
            "{out}"
        );
        assert!(
            out.contains("UNMET DEPENDENCY react-dom@^18.0.0 [stdout:18]"),
            "{out}"
        );
        assert!(out.contains("chalk@5.3.0 extraneous [stdout:20]"), "{out}");
        assert!(
            out.contains("bad-pkg@1.0.0 invalid: \"^2.0.0\" from the root project [stdout:21]"),
            "{out}"
        );
        assert!(
            out.contains("legacy-pkg@0.9.0 missing [stdout:23]"),
            "{out}"
        );
        assert!(
            out.contains(
                "npm ERR! missing: react-dom@^18.0.0, required by my-web-app@1.2.0 [stdout:24]"
            ),
            "{out}"
        );
        assert!(out.contains("npm ERR! extraneous: chalk@5.3.0 /home/dev/my-web-app/node_modules/chalk [stdout:25]"), "{out}");

        // Multi-version duplicate `semver` (6.3.1 vs 7.6.3) detected!
        assert!(
            out.contains("semver (6.3.1 [stdout:7], 7.6.3 [stdout:16])"),
            "{out}"
        );

        // Workspace package & transitive counts with exact ranges.
        assert!(
            out.contains("@babel/core@7.25.2 (+5 transitive) [stdout:2-7]"),
            "{out}"
        );
        assert!(
            out.contains("express@4.21.1 (+7 transitive) [stdout:8-16]"),
            "{out}"
        );
        assert!(
            out.contains("@org/shared@0.1.0 -> ./packages/shared [stdout:22]"),
            "{out}"
        );
    }

    #[test]
    fn pnpm_list_and_yarn_list_format_cleanly() {
        let pnpm_raw = "\
Legend: production dependency, optional only, dev only

my-app@1.0.0 /home/dev/my-app

dependencies:
express 4.21.1
├─┬ body-parser 1.20.3
│ ├── bytes 3.1.2
│ └── iconv-lite 0.4.24
├─┬ debug 2.6.9
│ └── ms 2.0.0
└── qs 6.13.0
@org/shared link:../shared

devDependencies:
vitest 2.1.4
├─┬ vite 5.4.10
│ ├── esbuild 0.21.5
│ ├── postcss 8.4.47
│ └── rollup 4.24.0
├── debug 4.3.7
│ └── ms 2.1.3
└── tinybench 2.9.0
";
        let out = format("pnpm", &s_args(&["list", "--depth", "5"]), pnpm_raw, 4000)
            .expect("pnpm list should format");
        assert!(out.len() < pnpm_raw.len());
        assert!(out.contains("pnpm list: my-app@1.0.0"), "{out}");
        assert!(
            out.contains("debug (2.6.9 [stdout:10], 4.3.7 [stdout:21])"),
            "{out}"
        );
        assert!(
            out.contains("ms (2.0.0 [stdout:11], 2.1.3 [stdout:22])"),
            "{out}"
        );
        assert!(
            out.contains("express@4.21.1 (+6 transitive) [stdout:6-12]"),
            "{out}"
        );
        assert!(
            out.contains("@org/shared -> ../shared [stdout:13]"),
            "{out}"
        );
        assert!(
            out.contains("vitest@2.1.4 [dev] (+7 transitive) [stdout:16-23]"),
            "{out}"
        );

        let yarn_raw = "\
yarn list v1.22.22
├─ @babel/core@7.25.2
│  ├─ @babel/code-frame@7.24.7
│  ├─ @babel/generator@7.25.6
│  └─ semver@6.3.1
├─ express@4.21.1
│  ├─ accepts@1.3.8
│  ├─ body-parser@1.20.3
│  │  ├─ bytes@3.1.2
│  │  └─ raw-body@2.5.2
│  └─ semver@7.6.3
└─ lodash@4.17.21
Done in 0.42s.
";
        let y_out =
            format("yarn", &s_args(&["list"]), yarn_raw, 4000).expect("yarn list should format");
        assert!(y_out.len() < yarn_raw.len());
        assert!(y_out.contains("yarn list: 3 top-level"), "{y_out}");
        assert!(
            y_out.contains("semver (6.3.1 [stdout:5], 7.6.3 [stdout:11])"),
            "{y_out}"
        );
        assert!(
            y_out.contains("express@4.21.1 (+5 transitive) [stdout:6-11]"),
            "{y_out}"
        );
    }

    #[test]
    fn pipdeptree_forward_and_warnings() {
        let raw = "\
Warning!!! Possibly conflicting dependencies found:
* foo==1.0.0
 - urllib3 [required: <2.0, installed: 2.2.3]
requests==2.32.3
├── certifi [required: >=2017.4.17, installed: 2024.8.30]
├── charset-normalizer [required: >=2,<4, installed: 3.3.2]
├── idna [required: >=2.5,<4, installed: 3.10]
└── urllib3 [required: >=1.21.1,<3, installed: 2.2.3]
fastapi==0.115.0
├── pydantic [required: >=1.7.4,<3.0.0, installed: 2.9.2]
│   ├── annotated-types [required: >=0.6.0, installed: 0.7.0]
│   ├── pydantic-core [required: ==2.23.4, installed: 2.23.4]
│   │   └── typing-extensions [required: >=4.6.0, installed: 4.12.2]
│   └── typing-extensions [required: >=4.6.1, installed: 4.12.2]
└── starlette [required: >=0.37.2,<0.39.0, installed: 0.38.6]
    └── anyio [required: >=3.4.0,<5, installed: 4.6.0]
        ├── idna [required: >=2.8, installed: 3.10]
        └── sniffio [required: >=1.1, installed: 1.3.0]
broken-pkg==0.1.0
└── missing-dep [required: >=1.0, installed: ?]
";
        let out = format("pipdeptree", &[], raw, 4000).expect("pipdeptree should format");
        assert!(out.len() < raw.len());
        assert!(out.contains("problems (4):"), "{out}");
        assert!(
            out.contains("Warning!!! Possibly conflicting dependencies found: [stdout:1]"),
            "{out}"
        );
        assert!(
            out.contains("missing: missing-dep [required: >=1.0, installed: ?] [stdout:20]"),
            "{out}"
        );
        assert!(
            out.contains("requests==2.32.3 (+4 transitive) [stdout:4-8]"),
            "{out}"
        );
        assert!(
            out.contains("fastapi==0.115.0 (+8 transitive) [stdout:9-18]"),
            "{out}"
        );
    }

    #[test]
    fn go_mod_graph_summarizes_roots_duplicates_and_fan_in_hubs() {
        let raw = "\
github.com/myorg/myapp github.com/gin-gonic/gin@v1.10.0
github.com/myorg/myapp go.uber.org/zap@v1.27.0
github.com/myorg/myapp golang.org/x/sync@v0.8.0
github.com/myorg/myapp github.com/prometheus/client_golang@v1.20.4
github.com/gin-gonic/gin@v1.10.0 github.com/bytedance/sonic@v1.11.6
github.com/gin-gonic/gin@v1.10.0 golang.org/x/net@v0.25.0
github.com/gin-gonic/gin@v1.10.0 golang.org/x/sys@v0.25.0
github.com/gin-gonic/gin@v1.10.0 google.golang.org/protobuf@v1.34.1
go.uber.org/zap@v1.27.0 go.uber.org/multierr@v1.10.0
go.uber.org/zap@v1.27.0 golang.org/x/sys@v0.25.0
github.com/prometheus/client_golang@v1.20.4 github.com/beorn7/perks@v1.0.1
github.com/prometheus/client_golang@v1.20.4 golang.org/x/sys@v0.25.0
github.com/prometheus/client_golang@v1.20.4 google.golang.org/protobuf@v1.34.2
golang.org/x/net@v0.25.0 golang.org/x/sys@v0.25.0
golang.org/x/net@v0.25.0 golang.org/x/text@v0.15.0
github.com/bytedance/sonic@v1.11.6 golang.org/x/sys@v0.25.0
github.com/bytedance/sonic@v1.11.6 golang.org/x/arch@v0.8.0
github.com/other/dep@v0.1.0 golang.org/x/net@v0.29.0
github.com/other/dep@v0.1.0 golang.org/x/sys@v0.25.0
";
        let out = format("go", &s_args(&["mod", "graph"]), raw, 4000)
            .expect("go mod graph should format");
        assert!(out.len() < raw.len());
        assert!(
            out.contains("go mod graph: github.com/myorg/myapp"),
            "{out}"
        );
        assert!(out.contains("19 edges"), "{out}");
        // Multi-version modules detected
        assert!(
            out.contains("golang.org/x/net (v0.25.0 [stdout:6], v0.29.0 [stdout:18])"),
            "{out}"
        );
        assert!(
            out.contains("google.golang.org/protobuf (v1.34.1 [stdout:8], v1.34.2 [stdout:13])"),
            "{out}"
        );
        // Root direct edges with transitive reachability
        assert!(
            out.contains("github.com/gin-gonic/gin@v1.10.0 (+6 transitive) [stdout:1]"),
            "{out}"
        );
        assert!(out.contains("golang.org/x/sync@v0.8.0 [stdout:3]"), "{out}");
        // Top fan-in hub: golang.org/x/sys@v0.25.0 (6 dependents, lines 7-19)
        assert!(
            out.contains("golang.org/x/sys@v0.25.0 (6 dependents) [stdout:7-19]"),
            "{out}"
        );
    }

    #[test]
    fn mvn_dependency_tree_handles_info_scopes_and_verbose_conflicts() {
        let raw = "\
[INFO] Scanning for projects...
[INFO] ----------------------< com.example:my-service >-----------------------
[INFO] Building my-service 1.0.0-SNAPSHOT
[INFO] --- dependency:3.6.1:tree (default-cli) @ my-service ---
[INFO] com.example:my-service:jar:1.0.0-SNAPSHOT
[INFO] +- org.springframework.boot:spring-boot-starter-web:jar:3.3.4:compile
[INFO] |  +- org.springframework.boot:spring-boot-starter:jar:3.3.4:compile
[INFO] |  |  +- org.springframework.boot:spring-boot:jar:3.3.4:compile
[INFO] |  |  +- org.yaml:snakeyaml:jar:2.2:compile
[INFO] |  |  \\- (org.slf4j:slf4j-api:jar:1.7.36:compile - omitted for conflict with 2.0.16)
[INFO] |  \\- org.springframework:spring-webmvc:jar:6.1.13:compile
[INFO] |     +- org.springframework:spring-core:jar:6.1.13:compile
[INFO] |     \\- (org.springframework:spring-beans:jar:6.1.13:compile - omitted for duplicate)
[INFO] +- org.slf4j:slf4j-api:jar:2.0.16:compile
[INFO] \\- org.junit.jupiter:junit-jupiter:jar:5.10.3:test
[INFO]    +- org.junit.jupiter:junit-jupiter-api:jar:5.10.3:test
[INFO]    \\- org.junit.jupiter:junit-jupiter-engine:jar:5.10.3:test
[INFO] ------------------------------------------------------------------------
[INFO] BUILD SUCCESS
[INFO] ------------------------------------------------------------------------
[INFO] Total time:  2.345 s
";
        let out = format("mvn", &s_args(&["dependency:tree", "-Dverbose"]), raw, 4000)
            .expect("mvn dependency:tree should format");
        assert!(out.len() < raw.len());
        assert!(
            out.contains("mvn dependency:tree: com.example:my-service:1.0.0-SNAPSHOT"),
            "{out}"
        );
        // Conflict warning pinned at top!
        assert!(
            out.contains("org.slf4j:slf4j-api:jar:1.7.36:compile - omitted for conflict with 2.0.16 [stdout:10]"),
            "{out}"
        );
        assert!(
            out.contains("org.springframework.boot:spring-boot-starter-web:3.3.4 (+7 transitive) [stdout:6-13]"),
            "{out}"
        );
        assert!(
            out.contains(
                "org.junit.jupiter:junit-jupiter:5.10.3 [test] (+2 transitive) [stdout:15-17]"
            ),
            "{out}"
        );
    }

    #[test]
    fn gradle_dependencies_handles_configs_upgrades_dedup_and_failures() {
        let raw = "\
> Task :app:dependencies

------------------------------------------------------------
Project ':app'
------------------------------------------------------------

archives - Configuration for archive artifacts. (n)
No dependencies

compileClasspath - Compile classpath for source set 'main'.
+--- org.springframework.boot:spring-boot-starter-web:3.3.4
|    +--- org.springframework.boot:spring-boot-starter:3.3.4
|    |    +--- org.springframework.boot:spring-boot:3.3.4
|    |    \\--- org.yaml:snakeyaml:2.2
|    \\--- org.springframework:spring-webmvc:6.1.13
|         \\--- org.springframework:spring-core:6.1.13 (*)
+--- org.slf4j:slf4j-api:1.7.36 -> 2.0.16
+--- project :common
\\--- com.broken:missing-lib:9.9.9 FAILED

runtimeClasspath - Runtime classpath of source set 'main'.
+--- org.springframework.boot:spring-boot-starter-web:3.3.4
|    +--- org.springframework.boot:spring-boot-starter:3.3.4
|    |    +--- org.springframework.boot:spring-boot:3.3.4
|    |    \\--- org.yaml:snakeyaml:2.2
|    \\--- org.springframework:spring-webmvc:6.1.13
|         \\--- org.springframework:spring-core:6.1.13 (*)
+--- org.slf4j:slf4j-api:1.7.36 -> 2.0.16
+--- project :common
\\--- com.broken:missing-lib:9.9.9 FAILED

testCompileClasspath - Compile classpath for source set 'test'.
+--- org.junit.jupiter:junit-jupiter:5.10.3
|    +--- org.junit.jupiter:junit-jupiter-api:5.10.3
|    \\--- org.junit.jupiter:junit-jupiter-engine:5.10.3
\\--- org.slf4j:slf4j-api:2.1.0

(*) - indicates repeated occurrences of a transitive dependency subtree.

BUILD SUCCESSFUL in 1s
1 actionable task: 1 executed
";
        let out = format("./gradlew", &s_args(&[":app:dependencies"]), raw, 4000)
            .expect("gradle dependencies should format");
        assert!(out.len() < raw.len());
        assert!(
            out.contains("Project ':app' (3 configurations, 1 empty)"),
            "{out}"
        );
        // FAILED dependency pinned at top!
        assert!(out.contains("com.broken:missing-lib:9.9.9 FAILED"), "{out}");
        // Version upgrade `1.7.36 -> 2.0.16` preserved!
        assert!(
            out.contains("org.slf4j:slf4j-api (1.7.36 -> 2.0.16 [stdout:17])"),
            "{out}"
        );
        // Multi-version duplicate across configs (2.0.16 vs 2.1.0) detected!
        assert!(
            out.contains("org.slf4j:slf4j-api (2.0.16 [stdout:17], 2.1.0 [stdout:36])"),
            "{out}"
        );
        // Identical runtimeClasspath deduplicated!
        assert!(
            out.contains("runtimeClasspath: same as compileClasspath [stdout:21-30]"),
            "{out}"
        );
        assert!(out.contains("project :common [stdout:18]"), "{out}");
    }

    #[test]
    fn fails_closed_on_json_custom_tables_and_unrecognized_output() {
        let npm_json = r#"{
  "version": "1.0.0",
  "name": "my-app",
  "dependencies": {
    "express": { "version": "4.21.1" },
    "lodash": { "version": "4.17.21" }
  }
}"#;
        // Fails closed with `--json` flag AND without flag when stdout is JSON.
        assert!(format("npm", &s_args(&["ls", "--json"]), npm_json, 4000).is_none());
        assert!(format("npm", &s_args(&["ls"]), npm_json, 4000).is_none());
        assert!(format("pnpm", &s_args(&["list"]), "[{\"name\":\"app\"}]", 4000).is_none());
        assert!(
            format(
                "pipdeptree",
                &s_args(&["--json"]),
                "[{\"package\":{}}]",
                4000
            )
            .is_none()
        );

        // Custom `--format` table in `cargo tree`.
        let cargo_table = "\
my-app v0.1.0\tMIT
├── anyhow v1.0.89\tMIT OR Apache-2.0
└── serde v1.0.215\tMIT OR Apache-2.0
";
        assert!(
            format(
                "cargo",
                &s_args(&["tree", "--format", "{p}\t{l}"]),
                cargo_table,
                4000
            )
            .is_none()
        );
        assert!(format("cargo", &s_args(&["tree"]), cargo_table, 4000).is_none());

        // Non-tree custom `--format "{l}"` without crate versions.
        let licenses_only = "MIT\n├── Apache-2.0\n└── BSD-3-Clause\n";
        assert!(
            format(
                "cargo",
                &s_args(&["tree", "-f", "{l}"]),
                licenses_only,
                4000
            )
            .is_none()
        );

        // `--prefix none` fails closed.
        assert!(
            format(
                "cargo",
                &s_args(&["tree", "--prefix", "none"]),
                "a v1.0.0\nb v1.0.0\n",
                4000
            )
            .is_none()
        );

        // Maven DOT output fails closed.
        assert!(
            format(
                "mvn",
                &s_args(&["dependency:tree", "-DoutputType=dot"]),
                "digraph \"a\" {}\n",
                4000
            )
            .is_none()
        );

        // Unrecognized plain text fails closed for every tool.
        let prose = "This is just some random text\nthat is definitely not a dependency tree\nor a go module graph at all.\n";
        assert!(format("cargo", &s_args(&["tree"]), prose, 4000).is_none());
        assert!(format("npm", &s_args(&["ls"]), prose, 4000).is_none());
        assert!(format("pipdeptree", &[], prose, 4000).is_none());
        assert!(format("go", &s_args(&["mod", "graph"]), prose, 4000).is_none());
        assert!(format("mvn", &s_args(&["dependency:tree"]), prose, 4000).is_none());
        assert!(format("gradle", &s_args(&["dependencies"]), prose, 4000).is_none());

        // Unknown tool fails closed.
        assert!(
            format(
                "ls",
                &s_args(&["-la"]),
                "my-app v0.1.0\n└── foo v1.0.0\n",
                4000
            )
            .is_none()
        );
    }

    #[test]
    fn respects_strictly_smaller_and_tight_budget() {
        // Tiny 2-line tree where summary would be larger than raw stdout -> must return None!
        let tiny = "a v0.1.0\n└── b v1.0.0\n";
        assert!(format("cargo", &s_args(&["tree"]), tiny, 4000).is_none());

        // Large tree with tight budget -> output stays <= budget and < raw.len() while preserving duplicates.
        let large = build_large_cargo_tree();
        let tight_budget = 650;
        let out = format("cargo", &s_args(&["tree"]), &large, tight_budget)
            .expect("should degrade gracefully within tight budget");
        assert!(
            out.len() <= tight_budget,
            "exceeded budget: {} > {tight_budget}\n{out}",
            out.len()
        );
        assert!(out.len() < large.len());
        // Duplicates must still be 100% preserved even under tight budget!
        assert!(out.contains("bitflags ("), "{out}");
        assert!(out.contains("syn ("), "{out}");
        assert!(out.contains("windows-sys ("), "{out}");
        assert!(out.contains("more entries [stdout:"), "{out}");

        // Impossibly tiny budget (smaller than header) -> fails closed cleanly.
        assert!(format("cargo", &s_args(&["tree"]), &large, 20).is_none());
    }

    #[test]
    fn ansi_colored_output_parses_with_exact_line_numbers() {
        let raw = "\x1b[1mmy-app v0.1.0\x1b[0m (/path/to/my-app)\n\
├── \x1b[32mserde v1.0.215\x1b[0m\n\
│   └── serde_derive v1.0.215 (proc-macro)\n\
│       ├── proc-macro2 v1.0.89\n\
│       ├── quote v1.0.37\n\
│       └── syn v2.0.87\n\
└── \x1b[32mlegacy v0.1.0\x1b[0m\n\
    ├── proc-macro2 v1.0.89 (*)\n\
    └── syn v1.0.109\n";
        let out =
            format("cargo", &s_args(&["tree"]), raw, 4000).expect("ANSI cargo tree should format");
        assert!(out.len() < raw.len());
        assert!(
            out.contains("syn (v1.0.109 [stdout:9], v2.0.87 [stdout:6])"),
            "{out}"
        );
        assert!(
            out.contains("serde v1.0.215 (+4 transitive) [stdout:2-6]"),
            "{out}"
        );
    }

    #[test]
    fn pipdeptree_legacy_ascii_reverse_and_python_module_wrapper() {
        let legacy = "\
requests==2.32.3
  - certifi [required: >=2017.4.17, installed: 2024.8.30]
  - charset-normalizer [required: >=2,<4, installed: 3.3.2]
  - idna [required: >=2.5,<4, installed: 3.10]
  - urllib3 [required: >=1.21.1,<3, installed: 2.2.3]
httpx==0.27.2
  - anyio [required: Any, installed: 4.6.0]
    - idna [required: >=2.8, installed: 3.10]
    - sniffio [required: >=1.1, installed: 1.3.0]
  - certifi [required: Any, installed: 2024.8.30]
  - httpcore [required: ==1.*, installed: 1.0.6]
    - certifi [required: Any, installed: 2024.8.30]
    - h11 [required: >=0.13,<0.15, installed: 0.14.0]
";
        let out = format("python3", &s_args(&["-m", "pipdeptree"]), legacy, 4000)
            .expect("python3 -m pipdeptree legacy ascii should format");
        assert!(out.len() < legacy.len());
        assert!(
            out.contains("requests==2.32.3 (+4 transitive) [stdout:1-5]"),
            "{out}"
        );
        assert!(
            out.contains("httpx==0.27.2 (+6 transitive) [stdout:6-13]"),
            "{out}"
        );

        let rev = "\
typing-extensions==4.12.2
├── pydantic-core==2.23.4 [requires: typing-extensions>=4.6.0,!=4.7.0]
│   └── pydantic==2.9.2 [requires: pydantic-core==2.23.4]
│       ├── fastapi==0.115.0 [requires: pydantic>=1.7.4,<3.0.0]
│       ├── sqlmodel==0.0.22 [requires: pydantic>=1.10.13,<3.0.0]
│       └── instructor==1.5.2 [requires: pydantic>=2.0.2,<3.0.0]
└── starlette==0.38.6 [requires: typing-extensions>=3.10.0]
    └── fastapi==0.115.0 [requires: starlette>=0.37.2,<0.39.0]
";
        let rev_out = format(
            "pipdeptree",
            &s_args(&["-r", "-p", "typing-extensions"]),
            rev,
            4000,
        )
        .expect("pipdeptree -r should format");
        assert!(rev_out.len() < rev.len());
        assert!(
            rev_out.contains("pipdeptree -r: typing-extensions==4.12.2"),
            "{rev_out}"
        );
        assert!(
            rev_out.contains("pydantic-core==2.23.4 -> pydantic==2.9.2 -> {fastapi==0.115.0, sqlmodel==0.0.22, instructor==1.5.2} (×3) [stdout:2-6]"),
            "{rev_out}"
        );
    }

    #[test]
    fn cargo_tree_workspace_and_depth_prefix_and_combined_tool_name() {
        let ws = "\
api v0.1.0 (/ws/crates/api)
├── core v0.1.0 (/ws/crates/core)
│   ├── anyhow v1.0.89
│   ├── serde v1.0.215
│   │   └── serde_derive v1.0.215 (proc-macro)
│   │       ├── proc-macro2 v1.0.89
│   │       └── quote v1.0.37
│   └── thiserror v1.0.64
└── tokio v1.40.0
    ├── bytes v1.7.2
    └── mio v1.0.2

cli v0.1.0 (/ws/crates/cli)
├── clap v4.5.20
│   ├── clap_builder v4.5.20
│   └── clap_derive v4.5.18 (proc-macro)
│       ├── proc-macro2 v1.0.89 (*)
│       └── quote v1.0.37 (*)
└── core v0.1.0 (/ws/crates/core) (*)
";
        let out = format("cargo tree", &s_args(&["--workspace"]), ws, 4000)
            .expect("multi-root cargo tree should format");
        assert!(out.len() < ws.len());
        assert!(
            out.contains("cargo tree: 2 roots (api v0.1.0, cli v0.1.0)"),
            "{out}"
        );
        assert!(
            out.contains("api v0.1.0 (2 direct) [stdout:1-11]:"),
            "{out}"
        );
        assert!(
            out.contains("cli v0.1.0 (2 direct) [stdout:13-19]:"),
            "{out}"
        );

        let depth_raw = "\
0my-app v0.1.0 (/ws/my-app)
1serde v1.0.215
2serde_derive v1.0.215 (proc-macro)
3proc-macro2 v1.0.89
4unicode-ident v1.0.13
3quote v1.0.37
4proc-macro2 v1.0.89 (*)
3syn v2.0.87
4proc-macro2 v1.0.89 (*)
4quote v1.0.37 (*)
4unicode-ident v1.0.13 (*)
";
        let d_out = format(
            "cargo",
            &s_args(&["tree", "--prefix", "depth"]),
            depth_raw,
            4000,
        )
        .expect("depth-prefixed cargo tree should format");
        assert!(d_out.len() < depth_raw.len());
        assert!(
            d_out.contains("serde v1.0.215 (+5 transitive) [stdout:2-11]"),
            "{d_out}"
        );
    }

    #[test]
    fn byte_reduction_measurements_across_formats() {
        let pct = |raw: usize, out: usize| 100.0 * (1.0 - out as f64 / raw as f64);

        let cargo_large = build_large_cargo_tree();
        let cargo_out = format("cargo", &s_args(&["tree"]), &cargo_large, 8_000).unwrap();
        let cargo_pct = pct(cargo_large.len(), cargo_out.len());
        println!(
            "MEASURED cargo tree (415L): {} -> {} B ({cargo_pct:.1}%)",
            cargo_large.len(),
            cargo_out.len()
        );
        assert!(cargo_pct > 85.0);

        let mut inv = vec![
            "syn v2.0.87".to_owned(),
            "├── serde_derive v1.0.215 (proc-macro)".to_owned(),
            "│   └── serde v1.0.215".to_owned(),
            "│       ├── api-server v0.1.0 (/home/dev/enterprise-monorepo/crates/api-server)"
                .to_owned(),
            "│       ├── cli-tool v0.1.0 (/home/dev/enterprise-monorepo/crates/cli-tool)"
                .to_owned(),
            "│       └── worker-svc v0.1.0 (/home/dev/enterprise-monorepo/crates/worker-svc)"
                .to_owned(),
            "[build-dependencies]".to_owned(),
        ];
        for i in 1..=15 {
            let conn = if i == 15 { "└──" } else { "├──" };
            let bar = if i == 15 { " " } else { "│" };
            inv.push(format!("{conn} grammar-crate-{i:02} v0.2.{i}"));
            inv.push(format!(
                "{bar}   └── api-server v0.1.0 (/home/dev/enterprise-monorepo/crates/api-server)"
            ));
        }
        let inv_raw = inv.join("\n") + "\n";
        let inv_out = format("cargo", &s_args(&["tree", "-i", "syn"]), &inv_raw, 4000).unwrap();
        let inv_pct = pct(inv_raw.len(), inv_out.len());
        println!(
            "MEASURED cargo tree -i (37L): {} -> {} B ({inv_pct:.1}%)",
            inv_raw.len(),
            inv_out.len()
        );
        assert!(inv_pct > 60.0);

        // Large realistic go mod graph (100+ edges)
        let mut go_lines = vec![
            "github.com/myorg/platform github.com/gin-gonic/gin@v1.10.0".to_owned(),
            "github.com/myorg/platform go.uber.org/zap@v1.27.0".to_owned(),
            "github.com/myorg/platform github.com/prometheus/client_golang@v1.20.4".to_owned(),
            "github.com/myorg/platform google.golang.org/grpc@v1.67.1".to_owned(),
            "github.com/myorg/platform github.com/jackc/pgx/v5@v5.7.1".to_owned(),
        ];
        for i in 1..=30 {
            let m = format!("github.com/vendor/lib-{i:02}@v1.{i}.0");
            go_lines.push(format!("github.com/gin-gonic/gin@v1.10.0 {m}"));
            go_lines.push(format!("{m} golang.org/x/sys@v0.25.0"));
            go_lines.push(format!("{m} golang.org/x/net@v0.29.0"));
            if i % 10 == 0 {
                go_lines.push(format!("{m} golang.org/x/net@v0.25.0"));
            }
        }
        let go_raw = go_lines.join("\n") + "\n";
        let go_out = format("go", &s_args(&["mod", "graph"]), &go_raw, 4000).unwrap();
        let go_pct = pct(go_raw.len(), go_out.len());
        println!(
            "MEASURED go mod graph ({}L): {} -> {} B ({go_pct:.1}%)",
            go_lines.len(),
            go_raw.len(),
            go_out.len()
        );
        assert!(go_pct > 80.0);

        // Large realistic Gradle multi-configuration output
        let mut gradle_lines = vec![
            "> Task :app:dependencies".to_owned(),
            "------------------------------------------------------------".to_owned(),
            "Project ':app'".to_owned(),
            "------------------------------------------------------------".to_owned(),
        ];
        for cfg in [
            "compileClasspath",
            "runtimeClasspath",
            "testCompileClasspath",
            "testRuntimeClasspath",
        ] {
            gradle_lines.push(format!("{cfg} - Classpath for {cfg}."));
            for i in 1..=5 {
                let conn = if i == 5 { "\\---" } else { "+---" };
                let bar = if i == 5 { " " } else { "|" };
                gradle_lines.push(format!("{conn} org.springframework:spring-mod-{i}:6.1.13"));
                for j in 1..=6 {
                    let c2 = if j == 6 { "\\---" } else { "+---" };
                    gradle_lines.push(format!(
                        "{bar}    {c2} org.example:transitive-{i}-{j}:2.3.{j}"
                    ));
                }
            }
            gradle_lines.push(String::new());
        }
        let gradle_raw = gradle_lines.join("\n") + "\n";
        let gradle_out = format(
            "./gradlew",
            &s_args(&[":app:dependencies"]),
            &gradle_raw,
            4000,
        )
        .unwrap();
        let gradle_pct = pct(gradle_raw.len(), gradle_out.len());
        println!(
            "MEASURED gradle dependencies ({}L): {} -> {} B ({gradle_pct:.1}%)",
            gradle_lines.len(),
            gradle_raw.len(),
            gradle_out.len()
        );
        assert!(gradle_pct > 85.0);
    }
}
