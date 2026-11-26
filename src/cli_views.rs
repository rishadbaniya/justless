//! Compact, zero-semantic-loss views for high-frequency CLI raw dumps:
//! - `kubectl describe` / `oc describe`
//! - `gh` / `glab` human views (`gh pr checks`, `gh run view`, `gh pr/issue view`,
//!   `glab mr/issue view`, `pr`/`issue`/`run`/`mr`/`pipeline` lists)
//! - Security audits (`npm audit`, `pnpm audit`, `yarn audit`, `cargo audit`)
//! - Package & state inventories (`terraform`/`tofu state list`, `npm`/`pnpm`/`yarn`/`cargo` `outdated`,
//!   `pip list`/`freeze`, `uv pip list`)
//!
//! Every view keeps 100% of failures, warnings, unhealthy states, advisories, and
//! major version bumps, links every folded region back to exact 1-based
//! `[stdout:A-B]` archive line numbers, declines machine-formatted or unrecognized
//! output, and replaces the raw text only when strictly smaller and within
//! `max_bytes`.
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

/// Refuse inputs with pathologically long single lines (matches `capture::SCAN_LINE_LIMIT`).
const MAX_LINE_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewKind {
    KubectlDescribe,
    GhPrChecks,
    GhRunView,
    ForgeView(&'static str),
    ForgeList(&'static str, bool), // (label, prioritize_failures)
    SecurityAudit(&'static str),
    TerraformStateList(&'static str),
    Outdated(&'static str),
    PipList,
    PipFreeze,
}

impl ViewKind {
    fn tag(self) -> &'static str {
        match self {
            Self::KubectlDescribe => "kubectl_describe",
            Self::GhPrChecks => "gh_pr_checks",
            Self::GhRunView => "gh_run_view",
            Self::ForgeView(_) => "forge_view",
            Self::ForgeList(..) => "forge_list",
            Self::SecurityAudit(_) => "security_audit",
            Self::TerraformStateList(_) => "tf_state_list",
            Self::Outdated(_) => "outdated",
            Self::PipList => "pip_list",
            Self::PipFreeze => "pip_freeze",
        }
    }
}

/// Tag identifying which CLI view handles `words`, if any, for post-run planning.
pub(crate) fn kind_tag(words: &[&str]) -> Option<&'static str> {
    classify_words(words).map(ViewKind::tag)
}

/// Format raw stdout `text` produced by `words` into a strictly smaller view
/// within `max_bytes`, preserving 100% of critical signals with exact
/// `[stdout:A-B]` pointers. Returns `None` when the command or output is not
/// recognized, or when compaction does not beat the raw text within `max_bytes`.
pub fn format(words: &[&str], text: &str, max_bytes: usize) -> Option<String> {
    if text.is_empty() || max_bytes == 0 || text.contains('\0') {
        return None;
    }
    let kind = classify_words(words)?;
    let has_ansi = text.contains('\x1b');
    let mut lines: Vec<Cow<'_, str>> = Vec::with_capacity((text.len() / 48).clamp(16, 16_384));
    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let clean = if has_ansi && line.contains('\x1b') {
            crate::capture::ANSI.replace_all(line, "")
        } else {
            Cow::Borrowed(line)
        };
        if clean.len() >= MAX_LINE_BYTES {
            return None;
        }
        lines.push(clean);
    }
    if lines.is_empty() {
        return None;
    }
    let rendered = match kind {
        ViewKind::KubectlDescribe => format_kubectl_describe(&lines, max_bytes),
        ViewKind::GhPrChecks => format_gh_pr_checks(&lines, max_bytes),
        ViewKind::GhRunView => format_gh_run_view(&lines, max_bytes),
        ViewKind::ForgeView(label) => format_forge_view(&lines, text.len(), label, max_bytes),
        ViewKind::ForgeList(label, prioritize_failures) => {
            format_forge_list(&lines, text.len(), label, prioritize_failures, max_bytes)
        }
        ViewKind::SecurityAudit(label) => format_security_audit(&lines, label, max_bytes),
        ViewKind::TerraformStateList(label) => {
            format_terraform_state_list(&lines, label, max_bytes)
        }
        ViewKind::Outdated(label) => format_outdated(&lines, label, max_bytes),
        ViewKind::PipList => format_pip_list(&lines, text.len(), max_bytes),
        ViewKind::PipFreeze => format_pip_freeze(&lines, text.len(), max_bytes),
    }?;
    (rendered.len() <= max_bytes && rendered.len() < text.len()).then_some(rendered)
}

fn basename(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program)
}

fn skip_flags<'a>(mut args: &'a [&'a str], valued: &[&str], boolean: &[&str]) -> &'a [&'a str] {
    while let Some((&arg, rest)) = args.split_first() {
        if valued.contains(&arg) && rest.first().is_some_and(|v| !v.starts_with('-')) {
            args = &rest[1..];
        } else if let Some((flag, _)) = arg.split_once('=')
            && valued.contains(&flag)
        {
            args = rest;
        } else if boolean.contains(&arg) {
            args = rest;
        } else {
            break;
        }
    }
    args
}

fn has_flag(args: &[&str], blocked: &[&str]) -> bool {
    args.iter().any(|&arg| {
        let flag = arg.split_once('=').map_or(arg, |(f, _)| f);
        blocked.contains(&flag)
    })
}

fn has_flag_value(args: &[&str], flags: &[&str], bad_values: &[&str]) -> bool {
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        if let Some((flag, val)) = arg.split_once('=') {
            if flags.contains(&flag) && bad_values.iter().any(|bad| val.eq_ignore_ascii_case(bad)) {
                return true;
            }
        } else if flags.contains(&arg)
            && let Some(val) = iter.next()
            && bad_values.iter().any(|bad| val.eq_ignore_ascii_case(bad))
        {
            return true;
        }
    }
    false
}

fn classify_words(words: &[&str]) -> Option<ViewKind> {
    let owned = crate::wrappers::effective(words);
    let refs: Vec<&str> = match &owned {
        Some(w) => w.iter().map(String::as_str).collect(),
        None => words.to_vec(),
    };
    let tool = crate::filters::unwrap_tool(&refs);
    let (&program, args) = tool.split_first()?;
    if has_flag(args, &["-h", "--help", "--version", "-V"]) {
        return None;
    }
    match basename(program) {
        "kubectl" | "oc" => {
            let args = skip_flags(
                args,
                &[
                    "-n",
                    "--namespace",
                    "--context",
                    "--kubeconfig",
                    "--cluster",
                    "--user",
                    "--as",
                    "--as-group",
                    "--request-timeout",
                    "-v",
                    "--v",
                ],
                &["--insecure-skip-tls-verify"],
            );
            let ["describe", tail @ ..] = args else {
                return None;
            };
            if has_flag(tail, &["-h", "--help"]) {
                return None;
            }
            Some(ViewKind::KubectlDescribe)
        }
        "gh" => {
            let args = skip_flags(args, &["-R", "--repo", "--hostname"], &[]);
            match args {
                ["pr", "checks", tail @ ..] => {
                    if has_flag(
                        tail,
                        &[
                            "--json",
                            "-q",
                            "--jq",
                            "-t",
                            "--template",
                            "-w",
                            "--web",
                            "--watch",
                        ],
                    ) {
                        return None;
                    }
                    Some(ViewKind::GhPrChecks)
                }
                ["run", "view", tail @ ..] => {
                    if has_flag(
                        tail,
                        &[
                            "--json",
                            "-q",
                            "--jq",
                            "-t",
                            "--template",
                            "-w",
                            "--web",
                            "--log",
                            "--log-failed",
                        ],
                    ) {
                        return None;
                    }
                    Some(ViewKind::GhRunView)
                }
                [sub @ ("pr" | "issue"), "view", tail @ ..] => {
                    if has_flag(
                        tail,
                        &["--json", "-q", "--jq", "-t", "--template", "-w", "--web"],
                    ) {
                        return None;
                    }
                    let label = if *sub == "pr" {
                        "gh pr view"
                    } else {
                        "gh issue view"
                    };
                    Some(ViewKind::ForgeView(label))
                }
                [
                    sub @ ("pr" | "issue" | "run" | "release"),
                    "list" | "ls",
                    tail @ ..,
                ] => {
                    if has_flag(
                        tail,
                        &["--json", "-q", "--jq", "-t", "--template", "-w", "--web"],
                    ) {
                        return None;
                    }
                    let label = match *sub {
                        "pr" => "gh pr list",
                        "issue" => "gh issue list",
                        "run" => "gh run list",
                        _ => "gh release list",
                    };
                    Some(ViewKind::ForgeList(label, *sub == "run"))
                }
                _ => None,
            }
        }
        "glab" => {
            let args = skip_flags(args, &["-R", "--repo"], &[]);
            match args {
                [sub @ ("mr" | "issue"), "view", tail @ ..] => {
                    if has_flag(
                        tail,
                        &[
                            "-F",
                            "--output",
                            "-o",
                            "--json",
                            "-q",
                            "--jq",
                            "-t",
                            "--template",
                            "-w",
                            "--web",
                        ],
                    ) {
                        return None;
                    }
                    let label = if *sub == "mr" {
                        "glab mr view"
                    } else {
                        "glab issue view"
                    };
                    Some(ViewKind::ForgeView(label))
                }
                [
                    sub @ ("mr" | "issue" | "pipeline" | "ci" | "release"),
                    "list" | "ls",
                    tail @ ..,
                ] => {
                    if has_flag(tail, &["-F", "--output", "-o", "-w", "--web"]) {
                        return None;
                    }
                    let label = match *sub {
                        "mr" => "glab mr list",
                        "issue" => "glab issue list",
                        "pipeline" | "ci" => "glab pipeline list",
                        _ => "glab release list",
                    };
                    Some(ViewKind::ForgeList(
                        label,
                        matches!(*sub, "pipeline" | "ci"),
                    ))
                }
                _ => None,
            }
        }
        prog @ ("npm" | "pnpm" | "yarn") => match args {
            ["audit", tail @ ..] => {
                if tail.contains(&"fix")
                    || has_flag(tail, &["--fix", "--json", "--parseable", "-p"])
                {
                    return None;
                }
                let label = match prog {
                    "npm" => "npm audit",
                    "pnpm" => "pnpm audit",
                    _ => "yarn audit",
                };
                Some(ViewKind::SecurityAudit(label))
            }
            ["outdated", tail @ ..] => {
                if has_flag(tail, &["--json", "--parseable", "-p"]) {
                    return None;
                }
                let label = match prog {
                    "npm" => "npm outdated",
                    "pnpm" => "pnpm outdated",
                    _ => "yarn outdated",
                };
                Some(ViewKind::Outdated(label))
            }
            _ => None,
        },
        "cargo" => match args {
            ["audit", tail @ ..] => {
                if tail.contains(&"fix") || has_flag(tail, &["--fix", "--json"]) {
                    return None;
                }
                Some(ViewKind::SecurityAudit("cargo audit"))
            }
            ["outdated", tail @ ..] => {
                if has_flag(tail, &["--json"]) || has_flag_value(tail, &["--format"], &["json"]) {
                    return None;
                }
                Some(ViewKind::Outdated("cargo outdated"))
            }
            _ => None,
        },
        "pip" | "pip3" => classify_pip(args),
        "uv" => match args {
            ["pip", rest @ ..] => classify_pip(rest),
            _ => None,
        },
        prog @ ("terraform" | "tofu") => {
            let args = skip_flags(args, &["-chdir"], &[]);
            let ["state", "list", tail @ ..] = args else {
                return None;
            };
            if has_flag(tail, &["-h", "--help"]) {
                return None;
            }
            Some(ViewKind::TerraformStateList(if prog == "tofu" {
                "tofu state list"
            } else {
                "terraform state list"
            }))
        }
        _ => None,
    }
}

fn classify_pip(args: &[&str]) -> Option<ViewKind> {
    match args {
        ["list", tail @ ..] => {
            if has_flag_value(tail, &["--format"], &["json", "freeze"]) {
                return None;
            }
            if has_flag(tail, &["-o", "--outdated"]) {
                Some(ViewKind::Outdated("pip list --outdated"))
            } else {
                Some(ViewKind::PipList)
            }
        }
        ["freeze", tail @ ..] => {
            if has_flag(tail, &["-h", "--help"]) {
                return None;
            }
            Some(ViewKind::PipFreeze)
        }
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct RangeTag(usize, usize);

impl std::fmt::Display for RangeTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == self.1 {
            write!(f, "[stdout:{}]", self.0)
        } else {
            write!(f, "[stdout:{}-{}]", self.0, self.1)
        }
    }
}

#[inline]
fn range_tag(first: usize, last: usize) -> RangeTag {
    RangeTag(first, last)
}

#[inline]
fn indent_of(line: &str) -> usize {
    line.bytes().take_while(|&b| b == b' ').count()
}

fn tighten_spaces(s: &str) -> Cow<'_, str> {
    let trimmed = s.trim();
    if !trimmed.contains('\t') && !trimmed.contains("   ") {
        return Cow::Borrowed(trimmed);
    }
    let mut out = String::with_capacity(trimmed.len());
    let mut space_run = 0usize;
    for ch in trimmed.chars() {
        if ch == '\t' {
            if space_run < 2 && !out.is_empty() {
                while space_run < 2 {
                    out.push(' ');
                    space_run += 1;
                }
            }
        } else if ch == ' ' {
            if space_run < 2 {
                out.push(' ');
                space_run += 1;
            }
        } else {
            space_run = 0;
            out.push(ch);
        }
    }
    Cow::Owned(out)
}

fn collapse_single_spaces(s: &str) -> Cow<'_, str> {
    let trimmed = s.trim();
    if !trimmed.contains("  ")
        && !trimmed.contains('\t')
        && !trimmed.contains('\n')
        && !trimmed.contains('\r')
    {
        return Cow::Borrowed(trimmed);
    }
    let mut out = String::with_capacity(trimmed.len());
    for (idx, part) in trimmed.split_whitespace().enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        out.push_str(part);
    }
    Cow::Owned(out)
}

#[inline]
fn contains_ascii_icase(haystack: &str, needle_lower: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle_lower.as_bytes();
    if n.is_empty() {
        return true;
    }
    if h.len() < n.len() {
        return false;
    }
    let first_lo = n[0];
    let first_hi = first_lo.to_ascii_uppercase();
    let limit = h.len() - n.len();
    let mut i = 0;
    while i <= limit {
        let b = h[i];
        if (b == first_lo || b == first_hi) && h[i..i + n.len()].eq_ignore_ascii_case(n) {
            return true;
        }
        i += 1;
    }
    false
}

fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..s.floor_char_boundary(max)]
    }
}

// ===========================================================================
// 1. kubectl describe / oc describe
// ===========================================================================

struct KubeSection<'a> {
    key: &'a str,
    inline_value: &'a str,
    first_line: usize,
    last_line: usize,
    body: Vec<(usize, &'a str)>,
}

struct KubeResource<'a> {
    first_line: usize,
    last_line: usize,
    sections: Vec<KubeSection<'a>>,
}

fn parse_top_kv(line: &str) -> Option<(&str, &str)> {
    if line.is_empty() || line.starts_with([' ', '\t']) {
        return None;
    }
    let (key, val) = line.split_once(':')?;
    if key.is_empty()
        || !key.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
        || key.contains("  ")
    {
        return None;
    }
    Some((key.trim(), val.trim()))
}

fn format_kubectl_describe(lines: &[Cow<'_, str>], max_bytes: usize) -> Option<String> {
    let mut resources: Vec<KubeResource<'_>> = Vec::new();
    let mut trailing_errors: Vec<(usize, &str)> = Vec::new();
    let mut current: Option<KubeResource<'_>> = None;

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let line = cow.as_ref();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if line.starts_with("Error from server") || line.starts_with("error:") {
            trailing_errors.push((line_no, trimmed));
            continue;
        }
        if let Some((key, val)) = parse_top_kv(line) {
            if key == "Name" {
                if let Some(res) = current.take() {
                    resources.push(res);
                }
                current = Some(KubeResource {
                    first_line: line_no,
                    last_line: line_no,
                    sections: vec![KubeSection {
                        key,
                        inline_value: val,
                        first_line: line_no,
                        last_line: line_no,
                        body: Vec::new(),
                    }],
                });
            } else {
                // Unrecognized leading text before any `Name:` block.
                let res = current.as_mut()?;
                res.last_line = line_no;
                res.sections.push(KubeSection {
                    key,
                    inline_value: val,
                    first_line: line_no,
                    last_line: line_no,
                    body: Vec::new(),
                });
            }
        } else {
            let res = current.as_mut()?;
            res.last_line = line_no;
            if let Some(sec) = res.sections.last_mut() {
                sec.last_line = line_no;
                sec.body.push((line_no, line));
            }
        }
    }
    if let Some(res) = current {
        resources.push(res);
    }
    if resources.is_empty() || resources.iter().all(|r| r.sections.len() < 2) {
        return None;
    }

    let rendered: Vec<(bool, usize, usize, String, String)> = resources
        .iter()
        .map(|res| {
            let (unhealthy, summary_name, text) = render_kube_resource(res);
            (unhealthy, res.first_line, res.last_line, summary_name, text)
        })
        .collect();

    let mut error_tail = String::with_capacity(trailing_errors.len() * 64);
    for (n, err) in &trailing_errors {
        let _ = writeln!(error_tail, "{err} {}", range_tag(*n, *n));
    }

    // Order unhealthy resources ahead of healthy ones (preserving input order within each group).
    let mut ordered: Vec<usize> = (0..rendered.len()).collect();
    if rendered.len() > 1 {
        ordered.sort_by_key(|&i| (!rendered[i].0, i));
    }

    // Try rendering all compacted resources first.
    let estimated_full: usize =
        rendered.iter().map(|r| r.4.len() + 1).sum::<usize>() + error_tail.len();
    let mut full = String::with_capacity(estimated_full);
    for (pos, &i) in ordered.iter().enumerate() {
        if pos > 0 {
            full.push('\n');
        }
        full.push_str(&rendered[i].4);
    }
    full.push_str(&error_tail);
    if full.len() <= max_bytes {
        return Some(full);
    }

    // Multiple resources exceeding budget: keep unhealthy resources first, then healthy,
    // folding overflow behind counts + exact `[stdout:A-B]` pointers.
    if rendered.len() > 1 {
        let mut out = String::with_capacity(max_bytes.min(full.len()));
        let unhealthy_count = rendered.iter().filter(|r| r.0).count();
        let _ = writeln!(
            out,
            "kubectl describe: {} resources ({} unhealthy)",
            rendered.len(),
            unhealthy_count
        );
        let reserve = error_tail.len() + 96;
        let mut omitted: Vec<(bool, usize, usize)> = Vec::new();
        for &i in &ordered {
            let (unhealthy, first, last, ref short_name, ref body) = rendered[i];
            let sep = usize::from(!out.is_empty());
            if out.len() + sep + body.len() + reserve <= max_bytes {
                out.push_str(body);
            } else if !unhealthy {
                let one_line = format!("  {short_name} {}\n", range_tag(first, last));
                if out.len() + one_line.len() + reserve <= max_bytes {
                    out.push_str(&one_line);
                } else {
                    omitted.push((unhealthy, first, last));
                }
            } else {
                // Keep at least a clipped prefix of an unhealthy resource rather than dropping it.
                let avail = max_bytes.saturating_sub(out.len() + reserve);
                if avail > 120 {
                    out.push_str(&fit_lines_with_tail(body, avail, first, last));
                } else {
                    omitted.push((unhealthy, first, last));
                }
            }
        }
        if !omitted.is_empty() {
            let first = omitted.iter().map(|o| o.1).min().unwrap_or(1);
            let last = omitted.iter().map(|o| o.2).max().unwrap_or(first);
            let _ = writeln!(
                out,
                "+{} more resources {}",
                omitted.len(),
                range_tag(first, last)
            );
        }
        out.push_str(&error_tail);
        if out.len() <= max_bytes {
            return Some(out);
        }
    }

    // Single huge resource still exceeding `max_bytes`: keep top identity + unhealthy
    // details and trim tail lines cleanly with `[stdout:A-B]`.
    let (_, first, last, _, _) = &rendered[ordered[0]];
    let avail = max_bytes.saturating_sub(error_tail.len());
    let mut trimmed = fit_lines_with_tail(&full, avail, *first, *last);
    trimmed.push_str(&error_tail);
    (trimmed.len() <= max_bytes).then_some(trimmed)
}

fn fit_lines_with_tail(body: &str, max_bytes: usize, first: usize, last: usize) -> String {
    let footer = format!(
        "... [truncated; full describe {}]\n",
        range_tag(first, last)
    );
    if body.len() <= max_bytes {
        return body.to_owned();
    }
    let cap = max_bytes.saturating_sub(footer.len());
    let mut out = String::with_capacity(max_bytes);
    for line in body.split_inclusive('\n') {
        if out.len() + line.len() > cap {
            break;
        }
        out.push_str(line);
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&footer);
    out
}

fn render_kube_resource(res: &KubeResource<'_>) -> (bool, String, String) {
    let mut out = String::with_capacity(res.sections.len().saturating_mul(64) + 128);
    let mut unhealthy = false;
    let mut name = "";
    let mut namespace = "";
    let mut status = "";

    for sec in &res.sections {
        match sec.key {
            "Name" => {
                name = sec.inline_value;
                let _ = writeln!(out, "Name: {}", sec.inline_value);
            }
            "Namespace" => {
                namespace = sec.inline_value;
                let _ = writeln!(out, "Namespace: {}", sec.inline_value);
            }
            "Status" | "Phase" => {
                status = sec.inline_value;
                if !matches!(
                    sec.inline_value,
                    "Running" | "Succeeded" | "Completed" | "Active" | "Bound" | "Ready"
                ) {
                    unhealthy = true;
                }
                let _ = writeln!(out, "{}: {}", sec.key, sec.inline_value);
            }
            "Node" | "IP" | "Controlled By" | "Replicas" | "StrategyType" | "Selector" | "Type"
            | "ClusterIP" | "External-IP" | "Port" | "TargetPort" | "Endpoints" | "Reason"
            | "Message" | "Nominated Node" | "Taints" => {
                if sec.key == "Reason"
                    || sec.key == "Message"
                    || (sec.key == "Replicas"
                        && contains_ascii_icase(sec.inline_value, "unavailable")
                        && !contains_ascii_icase(sec.inline_value, "0 unavailable"))
                {
                    unhealthy = true;
                }
                if sec.body.is_empty() {
                    let _ = writeln!(out, "{}: {}", sec.key, sec.inline_value);
                } else if sec.body.len() <= 3 {
                    let _ = writeln!(out, "{}: {}", sec.key, sec.inline_value);
                    for (_, line) in &sec.body {
                        let _ = writeln!(out, "  {}", collapse_single_spaces(line));
                    }
                } else {
                    let _ = writeln!(
                        out,
                        "{}: {} entries {}",
                        sec.key,
                        sec.body.len(),
                        range_tag(sec.first_line, sec.last_line)
                    );
                }
            }
            "IPs" => {
                let ips: Vec<&str> = sec
                    .body
                    .iter()
                    .filter_map(|(_, l)| l.trim().strip_prefix("IP:").map(str::trim))
                    .collect();
                if !ips.is_empty() {
                    let _ = writeln!(out, "IPs: {}", ips.join(", "));
                } else if !sec.inline_value.is_empty() {
                    let _ = writeln!(out, "IPs: {}", sec.inline_value);
                }
            }
            "Annotations" => {
                if sec.inline_value == "<none>" && sec.body.is_empty() {
                    continue;
                }
                let count = count_kv_entries(sec.inline_value, &sec.body);
                let has_last_applied = sec.inline_value.contains("last-applied-configuration")
                    || sec
                        .body
                        .iter()
                        .any(|(_, l)| l.contains("last-applied-configuration"));
                let note = if has_last_applied {
                    " (includes last-applied-configuration)"
                } else {
                    ""
                };
                let _ = writeln!(
                    out,
                    "Annotations: {count} {}{note} {}",
                    if count == 1 { "entry" } else { "entries" },
                    range_tag(sec.first_line, sec.last_line)
                );
            }
            "Labels" => {
                if sec.inline_value == "<none>" && sec.body.is_empty() {
                    continue;
                }
                let count = count_kv_entries(sec.inline_value, &sec.body);
                if count <= 2 && sec.body.len() <= 1 && sec.inline_value.len() <= 80 {
                    let mut items = Vec::new();
                    if !sec.inline_value.is_empty() {
                        items.push(sec.inline_value);
                    }
                    for (_, l) in &sec.body {
                        items.push(l.trim());
                    }
                    let _ = writeln!(out, "Labels: {}", items.join(", "));
                } else {
                    let _ = writeln!(
                        out,
                        "Labels: {count} {} {}",
                        if count == 1 { "entry" } else { "entries" },
                        range_tag(sec.first_line, sec.last_line)
                    );
                }
            }
            "Volumes" => {
                if sec.inline_value == "<none>" && sec.body.is_empty() {
                    continue;
                }
                let count = count_named_blocks(&sec.body, 2).max(1);
                let _ = writeln!(
                    out,
                    "Volumes: {count} {} {}",
                    if count == 1 { "volume" } else { "volumes" },
                    range_tag(sec.first_line, sec.last_line)
                );
            }
            "Containers" | "Init Containers" | "Ephemeral Containers" => {
                let (sec_unhealthy, rendered_sec) = render_kube_containers(sec.key, &sec.body, 2);
                unhealthy |= sec_unhealthy;
                out.push_str(&rendered_sec);
            }
            "Pod Template" => {
                let (tpl_unhealthy, rendered_tpl) = render_kube_pod_template(sec);
                unhealthy |= tpl_unhealthy;
                out.push_str(&rendered_tpl);
            }
            "Conditions" => {
                let (cond_unhealthy, rendered_cond) = render_kube_conditions(sec);
                unhealthy |= cond_unhealthy;
                out.push_str(&rendered_cond);
            }
            "Events" => {
                let (ev_unhealthy, rendered_ev) = render_kube_events(sec);
                unhealthy |= ev_unhealthy;
                out.push_str(&rendered_ev);
            }
            "Tolerations"
            | "Node-Selectors"
            | "System Info"
            | "Non-terminated Pods"
            | "Allocated resources"
            | "Images"
            | "Addresses" => {
                if sec.inline_value == "<none>" && sec.body.is_empty() {
                    continue;
                }
                let total_lines = usize::from(!sec.inline_value.is_empty()) + sec.body.len();
                if total_lines > 2 {
                    let _ = writeln!(
                        out,
                        "{}: {} lines {}",
                        sec.key,
                        total_lines,
                        range_tag(sec.first_line, sec.last_line)
                    );
                }
            }
            _ => {
                // Keep short unknown fields only if single-line; fold large multi-line sections.
                if sec.body.len() >= 4 {
                    let _ = writeln!(
                        out,
                        "{}: {} lines {}",
                        sec.key,
                        sec.body.len(),
                        range_tag(sec.first_line, sec.last_line)
                    );
                }
            }
        }
    }

    let summary_name = match (namespace.is_empty(), status.is_empty()) {
        (false, false) => format!("{namespace}/{name} ({status})"),
        (true, false) => format!("{name} ({status})"),
        (false, true) => format!("{namespace}/{name}"),
        (true, true) => name.to_owned(),
    };
    (unhealthy, summary_name, out)
}

fn count_kv_entries(inline: &str, body: &[(usize, &str)]) -> usize {
    let min_indent = body.iter().map(|(_, l)| indent_of(l)).min().unwrap_or(2);
    let body_entries = body
        .iter()
        .filter(|(_, l)| indent_of(l) == min_indent)
        .count();
    usize::from(!inline.is_empty() && inline != "<none>") + body_entries
}

fn count_named_blocks(body: &[(usize, &str)], target_indent: usize) -> usize {
    body.iter()
        .filter(|(_, l)| indent_of(l) == target_indent && l.trim_end().ends_with(':'))
        .count()
}

fn render_kube_pod_template(sec: &KubeSection<'_>) -> (bool, String) {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Pod Template: {}",
        range_tag(sec.first_line, sec.last_line)
    );
    let mut unhealthy = false;
    let mut i = 0;
    while i < sec.body.len() {
        let (line_no, line) = sec.body[i];
        let ind = indent_of(line);
        let trimmed = line.trim();
        if ind == 2
            && let Some((sub_key, sub_val)) = trimmed.split_once(':')
        {
            let sub_key = sub_key.trim();
            let sub_val = sub_val.trim();
            let mut j = i + 1;
            let mut sub_body = Vec::new();
            while j < sec.body.len() && indent_of(sec.body[j].1) > 2 {
                sub_body.push(sec.body[j]);
                j += 1;
            }
            let last_line = sub_body.last().map_or(line_no, |b| b.0);
            match sub_key {
                "Containers" | "Init Containers" => {
                    let (u, rendered) = render_kube_containers(sub_key, &sub_body, 4);
                    unhealthy |= u;
                    for rline in rendered.lines() {
                        let _ = writeln!(out, "  {rline}");
                    }
                }
                "Labels" | "Annotations" => {
                    let count = count_kv_entries(sub_val, &sub_body);
                    if count > 0 {
                        let _ = writeln!(
                            out,
                            "  {sub_key}: {count} {} {}",
                            if count == 1 { "entry" } else { "entries" },
                            range_tag(line_no, last_line)
                        );
                    }
                }
                "Volumes" => {
                    let count = count_named_blocks(&sub_body, 4).max(1);
                    let _ = writeln!(
                        out,
                        "  Volumes: {count} {} {}",
                        if count == 1 { "volume" } else { "volumes" },
                        range_tag(line_no, last_line)
                    );
                }
                _ => {}
            }
            i = j;
            continue;
        }
        i += 1;
    }
    (unhealthy, out)
}

fn render_kube_containers(
    section_name: &str,
    body: &[(usize, &str)],
    name_indent: usize,
) -> (bool, String) {
    // Split body into per-container blocks starting at `name_indent` ending with `:`.
    struct ContainerBlock<'a> {
        name: &'a str,
        first_line: usize,
        last_line: usize,
        lines: Vec<(usize, &'a str)>,
    }

    let mut blocks: Vec<ContainerBlock<'_>> = Vec::new();
    for &(line_no, line) in body {
        let ind = indent_of(line);
        let trimmed = line.trim();
        if ind == name_indent
            && let Some(cname) = trimmed.strip_suffix(':')
            && !cname.contains(' ')
        {
            blocks.push(ContainerBlock {
                name: cname,
                first_line: line_no,
                last_line: line_no,
                lines: Vec::new(),
            });
        } else if let Some(block) = blocks.last_mut() {
            block.last_line = line_no;
            block.lines.push((line_no, line));
        }
    }
    if blocks.is_empty() {
        return (false, String::new());
    }

    let field_indent = name_indent + 2;
    let mut out = String::with_capacity(blocks.len().saturating_mul(128) + 32);
    let _ = writeln!(out, "{section_name}:");
    let mut any_unhealthy = false;
    let mut healthy_summary_buf = String::new();

    for block in &blocks {
        let mut image = "";
        let mut state = "";
        let mut state_reason = "";
        let mut state_exit_code: Option<i64> = None;
        let mut has_last_state = false;
        let mut ready: Option<&str> = None;
        let mut started: Option<&str> = None;
        let mut restart_count: Option<u64> = None;
        let mut in_state_block = false;

        for &(_, line) in &block.lines {
            let ind = indent_of(line);
            let trimmed = line.trim();
            if ind == field_indent {
                in_state_block = false;
                if let Some((k, v)) = trimmed.split_once(':') {
                    let k = k.trim();
                    let v = v.trim();
                    match k {
                        "Image" => image = v,
                        "State" => {
                            state = v;
                            in_state_block = true;
                        }
                        "Last State" => has_last_state = true,
                        "Ready" => ready = Some(v),
                        "Started" => started = Some(v),
                        "Restart Count" => restart_count = v.parse::<u64>().ok(),
                        _ => {}
                    }
                }
            } else if in_state_block && let Some((k, v)) = trimmed.split_once(':') {
                let k = k.trim();
                let v = v.trim();
                if k == "Reason" {
                    state_reason = v;
                } else if k == "Exit Code" {
                    state_exit_code = v.parse::<i64>().ok();
                }
            }
        }

        let is_live = !state.is_empty() || ready.is_some() || restart_count.is_some();
        let completed_ok = state.eq_ignore_ascii_case("Terminated")
            && state_reason.eq_ignore_ascii_case("Completed")
            && state_exit_code == Some(0);
        let state_unhealthy =
            !state.is_empty() && !state.eq_ignore_ascii_case("Running") && !completed_ok;
        let ready_unhealthy =
            ready.is_some_and(|r| !r.eq_ignore_ascii_case("True")) && !completed_ok;
        let started_unhealthy = started.is_some_and(|s| s.eq_ignore_ascii_case("False"));
        let restarts_unhealthy = restart_count.is_some_and(|r| r > 0);
        let container_unhealthy = state_unhealthy
            || ready_unhealthy
            || started_unhealthy
            || restarts_unhealthy
            || has_last_state;
        any_unhealthy |= container_unhealthy;

        if is_live && !container_unhealthy {
            let state_label = if state.is_empty() { "Running" } else { state };
            let ready_label = ready.unwrap_or("True");
            let restarts = restart_count.unwrap_or(0);
            let _ = write!(
                healthy_summary_buf,
                "  {}: {state_label}, Ready={ready_label}, Restarts={restarts}",
                block.name
            );
            if !image.is_empty() {
                let _ = write!(healthy_summary_buf, " ({image})");
            }
            let _ = writeln!(
                healthy_summary_buf,
                " {}",
                range_tag(block.first_line, block.last_line)
            );
            continue;
        }

        // Unhealthy live container OR Pod Template container: preserve all critical state
        // fields verbatim at top and fold noisy Environment/Mounts/huge Args sections.
        let _ = writeln!(
            out,
            "  {}: {}",
            block.name,
            range_tag(block.first_line, block.last_line)
        );
        let mut i = 0;
        while i < block.lines.len() {
            let (line_no, line) = block.lines[i];
            let ind = indent_of(line);
            let trimmed = line.trim();
            if ind == field_indent
                && let Some((k, v)) = trimmed.split_once(':')
            {
                let k = k.trim();
                let v = v.trim();
                let mut j = i + 1;
                let mut sub = Vec::new();
                while j < block.lines.len() && indent_of(block.lines[j].1) > field_indent {
                    sub.push(block.lines[j]);
                    j += 1;
                }
                let last_line = sub.last().map_or(line_no, |s| s.0);
                match k {
                    "Environment" | "Environment Variables from" | "Mounts" => {
                        if v != "<none>" || !sub.is_empty() {
                            let count = sub.len().max(usize::from(!v.is_empty() && v != "<none>"));
                            let unit = match k {
                                "Mounts" => {
                                    if count == 1 {
                                        "mount"
                                    } else {
                                        "mounts"
                                    }
                                }
                                "Environment Variables from" => {
                                    if count == 1 {
                                        "source"
                                    } else {
                                        "sources"
                                    }
                                }
                                _ => {
                                    if count == 1 {
                                        "var"
                                    } else {
                                        "vars"
                                    }
                                }
                            };
                            let _ = writeln!(
                                out,
                                "    {k}: {count} {unit} {}",
                                range_tag(line_no, last_line)
                            );
                        }
                    }
                    "Args" if sub.len() > 6 => {
                        let _ = writeln!(
                            out,
                            "    Args: {} entries {}",
                            sub.len(),
                            range_tag(line_no, last_line)
                        );
                    }
                    "State" | "Last State" | "Ready" | "Started" | "Restart Count" | "Image"
                    | "Limits" | "Requests" | "Liveness" | "Readiness" | "Startup" | "Command"
                    | "Args" | "Port" | "Ports" => {
                        if v.is_empty() {
                            let _ = writeln!(out, "    {k}:");
                        } else {
                            let _ = writeln!(out, "    {k}: {v}");
                        }
                        for &(_, sline) in &sub {
                            let _ = writeln!(out, "      {}", collapse_single_spaces(sline));
                        }
                    }
                    _ => {
                        // Skip boilerplate container IDs / image IDs / Host Port unless unhealthy context needs them.
                    }
                }
                i = j;
                continue;
            }
            i += 1;
        }
    }
    out.push_str(&healthy_summary_buf);
    (any_unhealthy, out)
}

fn is_healthy_condition(cond_type: &str, status: &str) -> bool {
    const INVERTED: &[&str] = &[
        "MemoryPressure",
        "DiskPressure",
        "PIDPressure",
        "NetworkUnavailable",
        "DisruptionTarget",
        "ReplicaFailure",
    ];
    if INVERTED.contains(&cond_type) || cond_type.ends_with("Pressure") {
        status.eq_ignore_ascii_case("False")
    } else {
        status.eq_ignore_ascii_case("True")
    }
}

fn render_kube_conditions(sec: &KubeSection<'_>) -> (bool, String) {
    let mut healthy_first = 0usize;
    let mut healthy_last = 0usize;
    let mut healthy_count = 0usize;
    let mut unhealthy_rows: Vec<(usize, Cow<'_, str>)> = Vec::new();

    for &(line_no, line) in &sec.body {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("Type ")
            || trimmed.starts_with("Type\t")
            || trimmed.starts_with("----")
        {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(ctype) = parts.next() else {
            continue;
        };
        let Some(cstatus) = parts.next() else {
            continue;
        };
        if is_healthy_condition(ctype, cstatus) {
            healthy_count += 1;
            if healthy_first == 0 {
                healthy_first = line_no;
            }
            healthy_last = line_no;
        } else {
            unhealthy_rows.push((line_no, tighten_spaces(trimmed)));
        }
    }

    let mut out = String::with_capacity(unhealthy_rows.len() * 64 + 48);
    if unhealthy_rows.is_empty() {
        if healthy_count > 0 {
            let _ = writeln!(
                out,
                "Conditions: {healthy_count} healthy {}",
                range_tag(sec.first_line, sec.last_line)
            );
        }
        return (false, out);
    }

    let _ = writeln!(out, "Conditions:");
    for (line_no, row) in &unhealthy_rows {
        let _ = writeln!(out, "  {row} {}", range_tag(*line_no, *line_no));
    }
    if healthy_count > 0 {
        let _ = writeln!(
            out,
            "  (+{healthy_count} healthy {})",
            range_tag(healthy_first, healthy_last)
        );
    }
    (true, out)
}

fn render_kube_events(sec: &KubeSection<'_>) -> (bool, String) {
    if sec.inline_value == "<none>" && sec.body.is_empty() {
        return (false, "Events: <none>\n".to_owned());
    }
    let mut normal_count = 0usize;
    let mut normal_first = 0usize;
    let mut normal_last = 0usize;
    let mut warnings: Vec<(usize, usize, Cow<'_, str>)> = Vec::new();
    let mut current_is_normal = false;

    for &(line_no, line) in &sec.body {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || (trimmed.starts_with("Type")
                && trimmed.contains("Reason")
                && trimmed.contains("Message"))
            || trimmed.starts_with("----")
        {
            continue;
        }
        if trimmed.starts_with("Normal ") || trimmed.starts_with("Normal\t") {
            current_is_normal = true;
            normal_count += 1;
            if normal_first == 0 {
                normal_first = line_no;
            }
            normal_last = line_no;
        } else if trimmed.starts_with("Warning ") || trimmed.starts_with("Warning\t") {
            current_is_normal = false;
            warnings.push((line_no, line_no, tighten_spaces(trimmed)));
        } else if current_is_normal {
            normal_last = line_no;
        } else if let Some(last) = warnings.last_mut() {
            last.1 = line_no;
            let buf = last.2.to_mut();
            buf.push(' ');
            buf.push_str(trimmed);
        } else {
            warnings.push((line_no, line_no, tighten_spaces(trimmed)));
        }
    }

    let mut out = String::with_capacity(warnings.len() * 96 + 48);
    if warnings.is_empty() && normal_count == 0 {
        return (false, out);
    }
    let _ = writeln!(out, "Events:");
    for (first, last, msg) in &warnings {
        let _ = writeln!(out, "  {msg} {}", range_tag(*first, *last));
    }
    if normal_count > 0 {
        let _ = writeln!(
            out,
            "  Normal events: {normal_count} {}",
            range_tag(normal_first, normal_last)
        );
    }
    (!warnings.is_empty(), out)
}

// ===========================================================================
// 2. gh / glab human views
// ===========================================================================

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CheckState {
    Fail,
    Pending,
    Skipped,
    Pass,
}

fn parse_check_status_word(word: &str) -> Option<CheckState> {
    if word.eq_ignore_ascii_case("pass")
        || word.eq_ignore_ascii_case("passed")
        || word.eq_ignore_ascii_case("success")
    {
        Some(CheckState::Pass)
    } else if word.eq_ignore_ascii_case("fail")
        || word.eq_ignore_ascii_case("failed")
        || word.eq_ignore_ascii_case("failure")
        || word.eq_ignore_ascii_case("error")
        || word.eq_ignore_ascii_case("timed_out")
        || word.eq_ignore_ascii_case("startup_failure")
        || word.eq_ignore_ascii_case("action_required")
    {
        Some(CheckState::Fail)
    } else if word.eq_ignore_ascii_case("pending")
        || word.eq_ignore_ascii_case("queued")
        || word.eq_ignore_ascii_case("in_progress")
        || word.eq_ignore_ascii_case("expected")
        || word.eq_ignore_ascii_case("waiting")
        || word.eq_ignore_ascii_case("requested")
    {
        Some(CheckState::Pending)
    } else if word.eq_ignore_ascii_case("skipping")
        || word.eq_ignore_ascii_case("skipped")
        || word.eq_ignore_ascii_case("cancel")
        || word.eq_ignore_ascii_case("cancelled")
        || word.eq_ignore_ascii_case("canceled")
        || word.eq_ignore_ascii_case("neutral")
        || word.eq_ignore_ascii_case("stale")
    {
        Some(CheckState::Skipped)
    } else {
        None
    }
}

/// Classify a `gh pr checks` line. For `Pass` rows, returns `Cow::Borrowed("")`
/// without allocating/formatting the line since passing rows fold to a count.
fn classify_check_line(line: &str) -> Option<(CheckState, Cow<'_, str>)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Format A: TTY icon prefix (`✓  name  34s  url`, `X  name  2m  url`, `*  ...`, `-  ...`).
    if let Some((icon, rest)) = trimmed.split_once(char::is_whitespace) {
        let state = match icon {
            "✓" | "✔" => Some(CheckState::Pass),
            "X" | "✗" | "×" | "!" => Some(CheckState::Fail),
            "*" | "○" | "●" | "⏳" => Some(CheckState::Pending),
            "-" => Some(CheckState::Skipped),
            _ => None,
        };
        if let Some(state) = state
            && !rest.trim().is_empty()
        {
            let formatted = if state == CheckState::Pass {
                Cow::Borrowed("")
            } else {
                tighten_spaces(trimmed)
            };
            return Some((state, formatted));
        }
    }
    // Format B: TSV (`name\tstatus\tduration\turl`) or multi-space columns.
    let cols: Vec<&str> = if trimmed.contains('\t') {
        trimmed.split('\t').map(str::trim).collect()
    } else {
        split_multi_space(trimmed)
    };
    if cols.len() >= 2
        && let Some(state) = parse_check_status_word(cols[1])
    {
        if state == CheckState::Pass {
            return Some((state, Cow::Borrowed("")));
        }
        let mut compact = String::with_capacity(trimmed.len());
        let mut first = true;
        for col in cols {
            if !col.is_empty() {
                if !first {
                    compact.push_str("  ");
                }
                compact.push_str(col);
                first = false;
            }
        }
        return Some((state, Cow::Owned(compact)));
    }
    None
}

fn split_multi_space(line: &str) -> Vec<&str> {
    let mut cols = Vec::new();
    let bytes = line.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b' ' {
            let run_start = i;
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
            if i - run_start >= 2 {
                let cell = line[start..run_start].trim();
                if !cell.is_empty() {
                    cols.push(cell);
                }
                start = i;
            }
        } else {
            i += 1;
        }
    }
    let tail = line[start..].trim();
    if !tail.is_empty() {
        cols.push(tail);
    }
    cols
}

fn format_gh_pr_checks(lines: &[Cow<'_, str>], max_bytes: usize) -> Option<String> {
    let mut header_lines: Vec<&str> = Vec::new();
    let mut passed_count = 0usize;
    let mut passed_first = 0usize;
    let mut passed_last = 0usize;
    let mut attention: Vec<(CheckState, usize, Cow<'_, str>)> = Vec::new();
    let mut parsed = 0usize;

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = cow.as_ref().trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((state, formatted)) = classify_check_line(trimmed) {
            parsed += 1;
            if state == CheckState::Pass {
                passed_count += 1;
                if passed_first == 0 {
                    passed_first = line_no;
                }
                passed_last = line_no;
            } else {
                attention.push((state, line_no, formatted));
            }
        } else if parsed == 0 && header_lines.len() < 3 {
            header_lines.push(trimmed);
        } else {
            // Unrecognized line mid-output: refuse to risk dropping unknown data.
            return None;
        }
    }

    if parsed == 0 || passed_count == 0 {
        return None;
    }

    attention.sort_by_key(|(state, line_no, _)| (*state, *line_no));

    let mut out = String::with_capacity(
        max_bytes.min(128 + header_lines.len() * 48 + attention.len().saturating_mul(96)),
    );
    for h in &header_lines {
        let _ = writeln!(out, "{h}");
    }
    if attention.is_empty() {
        let _ = writeln!(
            out,
            "gh pr checks: {passed_count} passed {}",
            range_tag(passed_first, passed_last)
        );
        return (out.len() <= max_bytes).then_some(out);
    }

    let pass_line = format!(
        "{passed_count} passed {}\n",
        range_tag(passed_first, passed_last)
    );
    let mut kept = 0usize;
    let reserve = pass_line.len() + 64;
    let mut line_buf = String::with_capacity(128);
    for (_, line_no, formatted) in &attention {
        line_buf.clear();
        let _ = writeln!(line_buf, "{formatted} {}", range_tag(*line_no, *line_no));
        if out.len() + line_buf.len() + reserve <= max_bytes {
            out.push_str(&line_buf);
            kept += 1;
        } else {
            break;
        }
    }
    if kept < attention.len() {
        let rest_first = attention[kept].1;
        let rest_last = attention.last().map_or(rest_first, |a| a.1);
        let _ = writeln!(
            out,
            "+{} more non-passing checks {}",
            attention.len() - kept,
            range_tag(rest_first, rest_last)
        );
    }
    out.push_str(&pass_line);
    (out.len() <= max_bytes).then_some(out)
}

fn classify_status_icon_line(trimmed: &str) -> Option<CheckState> {
    let (icon, rest) = trimmed.split_once(char::is_whitespace)?;
    if rest.trim().is_empty() {
        return None;
    }
    match icon {
        "✓" | "✔" => Some(CheckState::Pass),
        "X" | "✗" | "×" | "!" => Some(CheckState::Fail),
        "*" | "○" | "●" | "⏳" => Some(CheckState::Pending),
        "-" => Some(CheckState::Skipped),
        _ => None,
    }
}

fn format_gh_run_view(lines: &[Cow<'_, str>], max_bytes: usize) -> Option<String> {
    struct RunJob<'a> {
        line_no: usize,
        last_line: usize,
        text: &'a str,
        state: CheckState,
        steps: Vec<(usize, &'a str, CheckState)>,
    }

    let mut lead: Vec<&str> = Vec::new();
    let mut jobs: Vec<RunJob<'_>> = Vec::new();
    let mut tail: Vec<&str> = Vec::new();
    let mut in_jobs = false;
    let mut saw_jobs_header = false;
    let mut after_jobs = false;

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let line = cow.as_ref();
        let trimmed = line.trim();
        if trimmed == "JOBS" {
            in_jobs = true;
            saw_jobs_header = true;
            continue;
        }
        if in_jobs
            && (trimmed == "ANNOTATIONS"
                || trimmed.starts_with("For more information")
                || trimmed.starts_with("View this run on GitHub:")
                || trimmed.starts_with("To see what failed"))
        {
            in_jobs = false;
            after_jobs = true;
            tail.push(trimmed);
            continue;
        }
        if after_jobs {
            if !trimmed.is_empty() {
                tail.push(trimmed);
            }
            continue;
        }
        if !in_jobs {
            if !trimmed.is_empty() {
                lead.push(trimmed);
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let ind = indent_of(line);
        let Some(state) = classify_status_icon_line(trimmed) else {
            // Non-icon line inside JOBS (e.g. wrapped annotation or error detail): keep as failure detail.
            if let Some(job) = jobs.last_mut() {
                job.state = job.state.min(CheckState::Fail);
                job.last_line = line_no;
                job.steps.push((line_no, trimmed, CheckState::Fail));
            }
            continue;
        };
        if ind == 0 {
            jobs.push(RunJob {
                line_no,
                last_line: line_no,
                text: trimmed,
                state,
                steps: Vec::new(),
            });
        } else if let Some(job) = jobs.last_mut() {
            job.last_line = line_no;
            if state < job.state {
                job.state = state;
            }
            job.steps.push((line_no, trimmed, state));
        } else {
            jobs.push(RunJob {
                line_no,
                last_line: line_no,
                text: trimmed,
                state,
                steps: Vec::new(),
            });
        }
    }

    if !saw_jobs_header && jobs.len() < 2 {
        return None;
    }
    if jobs.is_empty() {
        return None;
    }

    let mut non_passing: Vec<&RunJob<'_>> = Vec::new();
    let mut passed_count = 0usize;
    let mut passed_first = usize::MAX;
    let mut passed_last = 0usize;
    for job in &jobs {
        if job.state == CheckState::Pass {
            passed_count += 1;
            passed_first = passed_first.min(job.line_no);
            passed_last = passed_last.max(job.last_line);
        } else {
            non_passing.push(job);
        }
    }
    // Pin failing jobs first, then pending/in-progress, then skipped/cancelled.
    non_passing.sort_by_key(|j| (j.state, j.line_no));

    let mut lead_block = String::with_capacity(lead.len() * 64 + 16);
    for line in &lead {
        let _ = writeln!(lead_block, "{line}");
    }
    if saw_jobs_header {
        lead_block.push_str("JOBS\n");
    }

    let mut tail_block = String::with_capacity(tail.len() * 64);
    for line in &tail {
        let _ = writeln!(tail_block, "{line}");
    }

    let passed_summary = if passed_count > 0 {
        let unit = if passed_count == 1 { "job" } else { "jobs" };
        format!(
            "✓ {passed_count} passed {unit} {}\n",
            range_tag(passed_first, passed_last)
        )
    } else {
        String::new()
    };

    let render_job = |dst: &mut String, job: &RunJob<'_>| {
        let _ = writeln!(
            dst,
            "{} {}",
            job.text,
            range_tag(job.line_no, job.last_line)
        );
        // Inside a non-passing job, collapse passing steps to counts + `[stdout:A-B]`
        // and keep 100% of failing/pending/cancelled/skipped steps verbatim.
        let mut step_pass = 0usize;
        let mut step_first = 0usize;
        let mut step_last = 0usize;
        let flush_steps = |out: &mut String, c: &mut usize, f: &mut usize, l: &mut usize| {
            if *c > 0 {
                let unit = if *c == 1 { "step" } else { "steps" };
                let _ = writeln!(out, "  ✓ {c} passed {unit} {}", range_tag(*f, *l));
                *c = 0;
                *f = 0;
                *l = 0;
            }
        };
        for &(sno, stext, sstate) in &job.steps {
            if sstate == CheckState::Pass {
                step_pass += 1;
                if step_first == 0 {
                    step_first = sno;
                }
                step_last = sno;
            } else {
                flush_steps(dst, &mut step_pass, &mut step_first, &mut step_last);
                let _ = writeln!(dst, "  {stext} {}", range_tag(sno, sno));
            }
        }
        flush_steps(dst, &mut step_pass, &mut step_first, &mut step_last);
    };

    let mut full = String::with_capacity(
        lead_block.len()
            + non_passing.len().saturating_mul(160)
            + passed_summary.len()
            + tail_block.len(),
    );
    full.push_str(&lead_block);
    for job in &non_passing {
        render_job(&mut full, job);
    }
    full.push_str(&passed_summary);
    full.push_str(&tail_block);

    if full.len() <= max_bytes {
        return Some(full);
    }

    // Over budget: preserve lead, failing/pending jobs first, passed summary, and tail ANNOTATIONS.
    let mut out = String::with_capacity(max_bytes);
    out.push_str(&lead_block);
    let fixed_tail = passed_summary.len() + tail_block.len();
    let reserve = fixed_tail + 64;
    let mut job_buf = String::with_capacity(256);
    let mut kept = 0usize;
    for job in &non_passing {
        job_buf.clear();
        render_job(&mut job_buf, job);
        if out.len() + job_buf.len() + reserve <= max_bytes {
            out.push_str(&job_buf);
            kept += 1;
        } else {
            break;
        }
    }
    if kept < non_passing.len() {
        let rest = &non_passing[kept..];
        let first = rest.iter().map(|j| j.line_no).min().unwrap_or(1);
        let last = rest.iter().map(|j| j.last_line).max().unwrap_or(first);
        let _ = writeln!(
            out,
            "+{} more non-passing jobs {}",
            rest.len(),
            range_tag(first, last)
        );
    }
    if out.len() + passed_summary.len() + tail_block.len() <= max_bytes {
        out.push_str(&passed_summary);
        out.push_str(&tail_block);
        return Some(out);
    }
    if out.len() + tail_block.len() <= max_bytes {
        out.push_str(&tail_block);
        return Some(out);
    }

    let first = jobs.first().map_or(1, |j| j.line_no);
    let last = lines.len();
    let trimmed = fit_lines_with_tail(&full, max_bytes, first, last);
    (trimmed.len() <= max_bytes).then_some(trimmed)
}

fn is_forge_failure_row(row: &str) -> bool {
    let first = row.split_whitespace().next().unwrap_or("");
    matches!(first, "X" | "✗" | "×" | "!" | "*" | "failure" | "failed")
        || row.contains("  failure  ")
        || row.contains("  failed  ")
        || row.contains("  in_progress  ")
        || row.contains("  running  ")
}

fn format_forge_list(
    lines: &[Cow<'_, str>],
    raw_len: usize,
    label: &str,
    prioritize_failures: bool,
    max_bytes: usize,
) -> Option<String> {
    let mut preamble: Vec<&str> = Vec::new();
    let mut header: Option<Cow<'_, str>> = None;
    let mut rows: Vec<(usize, Cow<'_, str>)> = Vec::with_capacity(lines.len().min(512));

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = cow.as_ref().trim();
        if trimmed.is_empty() {
            continue;
        }
        if rows.is_empty()
            && header.is_none()
            && (trimmed.starts_with("Showing ") || trimmed.starts_with("showing "))
        {
            preamble.push(trimmed);
            continue;
        }
        let tightened = tighten_spaces(trimmed);
        if rows.is_empty() && header.is_none() && is_upper_table_header(trimmed) {
            header = Some(tightened);
            continue;
        }
        rows.push((line_no, tightened));
    }

    if rows.len() < 3 {
        return None;
    }

    // First try keeping all tightened rows.
    let mut full = String::with_capacity(raw_len.min(max_bytes + 256));
    for p in &preamble {
        let _ = writeln!(full, "{p}");
    }
    if let Some(h) = &header {
        let _ = writeln!(full, "{h}");
    }
    for (_, row) in &rows {
        let _ = writeln!(full, "{row}");
    }
    if full.len() <= max_bytes {
        // Only replace an inline fitting list if tightening saved at least 10% (or raw exceeded budget).
        if raw_len > max_bytes || full.len() * 10 <= raw_len * 9 {
            return Some(full);
        }
        return None;
    }

    // Over budget: keep preamble, header, priority rows (failing runs when applicable),
    // and as many top rows as fit, folding the rest behind `+N more ... [stdout:A-B]`.
    let mut out = String::with_capacity(max_bytes);
    if preamble.is_empty() {
        let _ = writeln!(out, "{label}: {} items", rows.len());
    } else {
        for p in &preamble {
            let _ = writeln!(out, "{p}");
        }
    }
    if let Some(h) = &header {
        let _ = writeln!(out, "{h}");
    }

    let reserve = 72usize;
    let mut kept_indices = vec![false; rows.len()];
    let mut kept_count = 0usize;
    let mut line_buf = String::with_capacity(128);

    if prioritize_failures {
        for (i, (line_no, row)) in rows.iter().enumerate() {
            if !is_forge_failure_row(row) {
                continue;
            }
            line_buf.clear();
            let _ = writeln!(line_buf, "{row} {}", range_tag(*line_no, *line_no));
            if out.len() + line_buf.len() + reserve <= max_bytes {
                out.push_str(&line_buf);
                kept_indices[i] = true;
                kept_count += 1;
            }
        }
    }
    for (i, (_, row)) in rows.iter().enumerate() {
        if kept_indices[i] {
            continue;
        }
        let needed = row.len() + 1;
        if out.len() + needed + reserve <= max_bytes {
            out.push_str(row);
            out.push('\n');
            kept_indices[i] = true;
            kept_count += 1;
        } else {
            break;
        }
    }

    let omitted_count = rows.len() - kept_count;
    if omitted_count > 0 {
        let mut first = usize::MAX;
        let mut last = 0usize;
        for (i, (line_no, _)) in rows.iter().enumerate() {
            if !kept_indices[i] {
                first = first.min(*line_no);
                last = last.max(*line_no);
            }
        }
        let _ = writeln!(
            out,
            "+{omitted_count} more {} {}",
            if omitted_count == 1 { "item" } else { "items" },
            range_tag(first, last)
        );
    }
    (kept_count > 0 && out.len() <= max_bytes).then_some(out)
}

fn is_upper_table_header(trimmed: &str) -> bool {
    let cols = if trimmed.contains('\t') {
        trimmed.split('\t').map(str::trim).collect::<Vec<_>>()
    } else {
        split_multi_space(trimmed)
    };
    cols.len() >= 3
        && cols.iter().all(|c| {
            !c.is_empty()
                && c.bytes()
                    .all(|b| b.is_ascii_uppercase() || matches!(b, b' ' | b'_' | b'-' | b'/'))
        })
}

// ===========================================================================
// 2b. gh pr/issue view & glab mr/issue view
// ===========================================================================

static HTML_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^>]+>").expect("valid html tag regex"));
static MD_LINKED_IMG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\[!\[[^\]]*\]\([^)]*\)\]\([^)]*\)|\[!\[[^\]]*\]\[[^\]]*\]\]\[[^\]]*\]")
        .expect("valid markdown linked image regex")
});
static MD_IMG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"!\[[^\]]*\]\([^)]*\)|!\[[^\]]*\]\[[^\]]*\]").expect("valid markdown image regex")
});
static HTML_LINKED_IMG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<a\b[^>]*>\s*(?:<picture\b[^>]*>.*?</picture>|<img\b[^>]*>)\s*</a>")
        .expect("valid html linked image regex")
});
static HTML_IMG_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<(?:img|source)\b[^>]*>|(?:</?(?:p|div|span|picture|center|br|hr)\b[^>]*>)")
        .expect("valid html image/wrapper regex")
});
static MD_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]+)\]\([^)]+\)").expect("valid markdown link regex"));

struct ForgeComment<'a> {
    author: String,
    header_display: String,
    first_line: usize,
    last_line: usize,
    body_lines: Vec<(usize, &'a str)>,
}

struct FoldedEntry {
    first_line: usize,
    last_line: usize,
    text: String,
}

fn is_forge_meta_key(key: &str) -> bool {
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "title"
            | "state"
            | "draft"
            | "author"
            | "labels"
            | "assignees"
            | "reviewers"
            | "projects"
            | "milestone"
            | "number"
            | "url"
            | "additions"
            | "deletions"
            | "comments"
            | "auto-merge"
            | "base"
            | "head"
            | "branch"
            | "branches"
            | "source"
            | "target"
            | "source branch"
            | "target branch"
            | "mergeable"
            | "locked"
            | "upvotes"
            | "downvotes"
            | "created"
            | "updated"
            | "merged"
            | "closed"
            | "time estimate"
            | "time spent"
            | "weight"
            | "epic"
            | "iteration"
    )
}

fn is_comment_meta_key(key: &str) -> bool {
    matches!(
        key.trim().to_ascii_lowercase().as_str(),
        "author"
            | "association"
            | "edited"
            | "status"
            | "created"
            | "updated"
            | "date"
            | "id"
            | "url"
    )
}

fn parse_kv_line(line: &str) -> Option<(&str, &str)> {
    let (key, rest) = line.split_once(':')?;
    if key.is_empty() || key.starts_with(char::is_whitespace) {
        return None;
    }
    if !rest.is_empty() && !rest.starts_with(['\t', ' ']) {
        return None;
    }
    Some((key.trim(), rest.trim()))
}

fn is_bot_author(author: &str) -> bool {
    let clean = author.trim().trim_start_matches('@').to_ascii_lowercase();
    if clean.is_empty() {
        return false;
    }
    if clean.ends_with("[bot]")
        || clean.ends_with("-bot")
        || clean.ends_with("_bot")
        || clean.ends_with("-robot")
    {
        return true;
    }
    matches!(
        clean.as_str(),
        "codecov"
            | "codecov-commenter"
            | "codecov-io"
            | "vercel"
            | "dependabot"
            | "dependabot-preview"
            | "github-actions"
            | "sonarcloud"
            | "sonarqubecloud"
            | "netlify"
            | "renovate"
            | "mergify"
            | "coveralls"
            | "circleci"
            | "netlify-cms"
            | "cloudflare-workers-and-pages"
            | "coderabbitai"
            | "sourcery-ai"
            | "deepsource-autofix"
            | "greenkeeper"
            | "allcontributors"
            | "size-limit"
            | "bundlewatch"
            | "danger"
            | "gitlab-qa"
            | "bot"
    )
}

fn is_valid_username_token(token: &str) -> bool {
    let u = token.trim().trim_start_matches('@');
    if u.is_empty() || u.len() > 64 {
        return false;
    }
    let Some(first) = u.bytes().next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    if !u
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'[' | b']'))
    {
        return false;
    }
    !matches!(
        u.to_ascii_lowercase().as_str(),
        "open"
            | "closed"
            | "merged"
            | "draft"
            | "view"
            | "labels"
            | "reviewers"
            | "assignees"
            | "milestone"
            | "projects"
            | "comments"
            | "state"
            | "source"
            | "target"
            | "title"
            | "author"
            | "url"
    )
}

fn parse_tty_comment_header(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.starts_with(['#', '-', '*', '>', '|', '`', '<', '[', '+'])
        || trimmed.starts_with("View this ")
    {
        return None;
    }
    if let Some((left, right)) = trimmed
        .split_once(" • ")
        .or_else(|| trimmed.split_once(" · "))
    {
        let left = left.trim();
        let right = right.trim();
        let user = if let Some((u, role_rest)) = left.split_once(" (") {
            if !role_rest.ends_with(')') {
                return None;
            }
            u.trim()
        } else {
            left
        };
        if !is_valid_username_token(user) {
            return None;
        }
        let low = right.to_ascii_lowercase();
        let looks_like_comment_meta = low.contains("ago")
            || low.contains("yesterday")
            || low.contains("just now")
            || low.contains("edited")
            || low.contains("approved")
            || low.contains("changes requested")
            || low.contains("commented")
            || low.contains("202")
            || low.contains("201")
            || right.ends_with('•')
            || right.ends_with('·');
        if looks_like_comment_meta {
            let author = user.trim_start_matches('@').to_owned();
            let header = trimmed
                .trim_end_matches(|c: char| c == '•' || c == '·' || c.is_whitespace())
                .to_owned();
            return Some((author, header));
        }
    }
    if let Some((left, right)) = trimmed.split_once(" commented ") {
        let user = left.trim();
        if is_valid_username_token(user) && !right.trim().is_empty() {
            return Some((user.trim_start_matches('@').to_owned(), trimmed.to_owned()));
        }
    }
    None
}

fn is_divider_line(trimmed: &str) -> bool {
    !trimmed.is_empty()
        && trimmed.len() >= 3
        && trimmed
            .chars()
            .all(|c| matches!(c, '-' | '─' | '—' | '=' | '_'))
}

fn is_forge_footer_line(trimmed: &str) -> bool {
    trimmed.starts_with("View this pull request on ")
        || trimmed.starts_with("View this issue on ")
        || trimmed.starts_with("View this merge request on ")
}

fn is_badge_or_image_line(trimmed: &str) -> bool {
    if trimmed.is_empty() {
        return false;
    }
    // Markdown reference-style badge definition: `[ci-badge]: https://img.shields.io/...`
    if let Some((label, url)) = trimmed.split_once("]: ")
        && label.starts_with('[')
        && !label[1..].contains(']')
    {
        let low = url.to_ascii_lowercase();
        if low.contains("shields.io")
            || low.contains("badgen.net")
            || low.contains("badge.svg")
            || low.contains("badge.png")
            || low.contains("codecov.io")
            || low.contains("coveralls.io")
            || low.contains("sonarcloud.io/api/project_badges")
            || low.contains("vercel.com/button")
        {
            return true;
        }
    }
    let low = trimmed.to_ascii_lowercase();
    let has_img_marker = trimmed.contains("![")
        || low.contains("<img")
        || low.contains("<source")
        || low.contains("shields.io")
        || low.contains("badge.svg")
        || low.contains("badgen.net");
    if !has_img_marker {
        return false;
    }
    let stripped = MD_LINKED_IMG_RE.replace_all(trimmed, "");
    let stripped = MD_IMG_RE.replace_all(&stripped, "");
    let stripped = HTML_LINKED_IMG_RE.replace_all(&stripped, "");
    let stripped = HTML_IMG_TAG_RE.replace_all(&stripped, "");
    let residual = stripped.replace("&nbsp;", "");
    residual
        .trim()
        .chars()
        .all(|c| c.is_whitespace() || matches!(c, '|' | '•' | '·' | '-' | '/'))
}

fn is_unchecked_checkbox(trimmed: &str) -> bool {
    trimmed.starts_with("- [ ] ")
        || trimmed.starts_with("* [ ] ")
        || trimmed.starts_with("+ [ ] ")
        || matches!(trimmed, "- [ ]" | "* [ ]" | "+ [ ]")
}

fn is_checked_checkbox(trimmed: &str) -> bool {
    trimmed.starts_with("- [x] ")
        || trimmed.starts_with("- [X] ")
        || trimmed.starts_with("* [x] ")
        || trimmed.starts_with("* [X] ")
        || trimmed.starts_with("+ [x] ")
        || trimmed.starts_with("+ [X] ")
}

fn count_tag_occurrences(lower: &str, needle: &str) -> usize {
    let mut count = 0usize;
    let mut rest = lower;
    while let Some(pos) = rest.find(needle) {
        let after = &rest[pos + needle.len()..];
        if needle == "<details" {
            if after.is_empty() || after.starts_with(['>', ' ', '\t', '/']) {
                count += 1;
            }
        } else {
            count += 1;
        }
        rest = after;
    }
    count
}

fn extract_details_summary(slice: &[(usize, &str)]) -> Option<String> {
    let mut capturing = false;
    let mut raw = String::new();
    for &(_, line) in slice {
        let low = line.to_ascii_lowercase();
        if !capturing {
            if let Some(open_pos) = low.find("<summary")
                && let Some(gt_rel) = low[open_pos..].find('>')
            {
                let start = open_pos + gt_rel + 1;
                if let Some(close_rel) = low[start..].find("</summary>") {
                    raw.push_str(&line[start..start + close_rel]);
                    break;
                }
                raw.push_str(&line[start..]);
                raw.push(' ');
                capturing = true;
            }
        } else if let Some(close_pos) = low.find("</summary>") {
            raw.push_str(&line[..close_pos]);
            break;
        } else {
            raw.push_str(line);
            raw.push(' ');
        }
    }
    let no_tags = HTML_TAG_RE.replace_all(&raw, " ");
    let cleaned = collapse_single_spaces(&no_tags);
    if cleaned.is_empty() {
        return None;
    }
    let clipped = clip(&cleaned, 60).trim_end();
    if clipped.len() < cleaned.len() {
        Some(format!("{clipped}..."))
    } else {
        Some(clipped.to_owned())
    }
}

fn fence_marker(trimmed: &str) -> Option<(char, usize)> {
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|&c| c == first).count();
    (len >= 3).then_some((first, len))
}

fn fold_markdown_lines(items: &[(usize, &str)]) -> (Vec<FoldedEntry>, bool) {
    let mut out: Vec<FoldedEntry> = Vec::new();
    let mut did_fold = false;
    let mut in_fence: Option<(char, usize)> = None;
    let mut i = 0usize;

    let push_blank_if_needed = |out: &mut Vec<FoldedEntry>, line_no: usize| {
        if out.last().is_some_and(|e| !e.text.is_empty()) {
            out.push(FoldedEntry {
                first_line: line_no,
                last_line: line_no,
                text: String::new(),
            });
        }
    };

    while i < items.len() {
        let (line_no, raw_line) = items[i];
        let trimmed = raw_line.trim();

        if let Some((fch, flen)) = in_fence {
            out.push(FoldedEntry {
                first_line: line_no,
                last_line: line_no,
                text: raw_line.trim_end().to_owned(),
            });
            if let Some((c, len)) = fence_marker(trimmed)
                && c == fch
                && len >= flen
                && trimmed.chars().skip(len).all(char::is_whitespace)
            {
                in_fence = None;
            }
            i += 1;
            continue;
        }

        if let Some(fence) = fence_marker(trimmed) {
            in_fence = Some(fence);
            out.push(FoldedEntry {
                first_line: line_no,
                last_line: line_no,
                text: raw_line.trim_end().to_owned(),
            });
            i += 1;
            continue;
        }

        if trimmed.is_empty() {
            push_blank_if_needed(&mut out, line_no);
            i += 1;
            continue;
        }

        // 1. Collapsible `<details>...</details>` blocks (supports nested `<details>`).
        let low = trimmed.to_ascii_lowercase();
        if low.starts_with("<details>") || low.starts_with("<details ") {
            let mut depth = 0isize;
            let mut end = None;
            for (j, &(_, candidate)) in items.iter().enumerate().skip(i) {
                let clow = candidate.to_ascii_lowercase();
                depth += count_tag_occurrences(&clow, "<details") as isize;
                depth -= count_tag_occurrences(&clow, "</details>") as isize;
                if depth <= 0 {
                    end = Some(j);
                    break;
                }
            }
            if let Some(end_idx) = end {
                let first_no = items[i].0;
                let last_no = items[end_idx].0;
                let count = end_idx - i + 1;
                let raw_bytes: usize = items[i..=end_idx].iter().map(|(_, s)| s.len() + 1).sum();
                let summary = extract_details_summary(&items[i..=end_idx]);
                let unit = if count == 1 { "line" } else { "lines" };
                let folded = match summary {
                    Some(s) => format!(
                        "[folded <details>: \"{s}\" ({count} {unit}) {}]",
                        range_tag(first_no, last_no)
                    ),
                    None => format!(
                        "[folded <details> ({count} {unit}) {}]",
                        range_tag(first_no, last_no)
                    ),
                };
                if end_idx > i || folded.len() < raw_bytes {
                    out.push(FoldedEntry {
                        first_line: first_no,
                        last_line: last_no,
                        text: folded,
                    });
                    did_fold = true;
                    i = end_idx + 1;
                    continue;
                }
            }
        }

        // 2. HTML comments `<!-- ... -->` (single-line or multi-line; merges adjacent comment blocks).
        if trimmed.starts_with("<!--") {
            let start_i = i;
            let mut end_i = None;
            let mut cursor = i;
            let mut comment_lines = 0usize;
            while cursor < items.len() {
                let cur_trim = items[cursor].1.trim();
                if cur_trim.is_empty() {
                    // Look ahead to see if another HTML comment follows immediately.
                    let mut next = cursor + 1;
                    while next < items.len() && items[next].1.trim().is_empty() {
                        next += 1;
                    }
                    if next < items.len() && items[next].1.trim().starts_with("<!--") {
                        cursor = next;
                        continue;
                    }
                    break;
                }
                if !cur_trim.starts_with("<!--") {
                    break;
                }
                let mut found_close = None;
                for (j, &(_, cand)) in items.iter().enumerate().skip(cursor) {
                    let ct = cand.trim();
                    if let Some(close_pos) = ct.rfind("-->") {
                        if ct[close_pos + 3..].trim().is_empty() {
                            found_close = Some(j);
                        }
                        break;
                    }
                }
                let Some(close_idx) = found_close else {
                    break;
                };
                comment_lines += close_idx - cursor + 1;
                end_i = Some(close_idx);
                cursor = close_idx + 1;
            }
            if let Some(end_idx) = end_i {
                let first_no = items[start_i].0;
                let last_no = items[end_idx].0;
                let raw_bytes: usize = items[start_i..=end_idx]
                    .iter()
                    .map(|(_, s)| s.len() + 1)
                    .sum();
                let unit = if comment_lines == 1 { "line" } else { "lines" };
                let folded = format!(
                    "[folded HTML comment ({comment_lines} {unit}) {}]",
                    range_tag(first_no, last_no)
                );
                if comment_lines >= 2 || folded.len() < raw_bytes {
                    out.push(FoldedEntry {
                        first_line: first_no,
                        last_line: last_no,
                        text: folded,
                    });
                    did_fold = true;
                    i = end_idx + 1;
                    continue;
                }
            }
        }

        // 3. Markdown badge / image shield walls.
        if is_badge_or_image_line(trimmed) {
            let start_i = i;
            let mut last_badge_i = i;
            let mut badge_count = 1usize;
            let mut j = i + 1;
            while j < items.len() {
                let jt = items[j].1.trim();
                if jt.is_empty() {
                    j += 1;
                    continue;
                }
                if is_badge_or_image_line(jt) {
                    last_badge_i = j;
                    badge_count += 1;
                    j += 1;
                } else {
                    break;
                }
            }
            let first_no = items[start_i].0;
            let last_no = items[last_badge_i].0;
            let raw_bytes: usize = items[start_i..=last_badge_i]
                .iter()
                .map(|(_, s)| s.len() + 1)
                .sum();
            let unit = if badge_count == 1 { "line" } else { "lines" };
            let folded = format!(
                "[folded {badge_count} badge/image {unit} {}]",
                range_tag(first_no, last_no)
            );
            if folded.len() < raw_bytes && (badge_count >= 2 || raw_bytes > 100) {
                out.push(FoldedEntry {
                    first_line: first_no,
                    last_line: last_no,
                    text: folded,
                });
                did_fold = true;
                i = last_badge_i + 1;
                continue;
            }
        }

        // 4. Long runs of unchecked PR template checklist items (`- [ ] ...`).
        //    Checked items (`- [x]`) are NEVER folded.
        if is_unchecked_checkbox(trimmed) {
            let start_i = i;
            let base_indent = indent_of(raw_line);
            let mut last_item_i = i;
            let mut item_count = 1usize;
            let mut j = i + 1;
            while j < items.len() {
                let (_, jraw) = items[j];
                let jt = jraw.trim();
                if jt.is_empty() {
                    j += 1;
                    continue;
                }
                if is_checked_checkbox(jt) {
                    break;
                }
                if is_unchecked_checkbox(jt) {
                    last_item_i = j;
                    item_count += 1;
                    j += 1;
                    continue;
                }
                // Allow indented continuation lines belonging to the current unchecked item.
                if indent_of(jraw) > base_indent
                    && !jt.starts_with('#')
                    && !jt.starts_with("```")
                    && !jt.starts_with("~~~")
                {
                    last_item_i = j;
                    j += 1;
                    continue;
                }
                break;
            }
            if item_count >= 3 {
                let first_no = items[start_i].0;
                let last_no = items[last_item_i].0;
                let raw_bytes: usize = items[start_i..=last_item_i]
                    .iter()
                    .map(|(_, s)| s.len() + 1)
                    .sum();
                let folded = format!(
                    "[folded {item_count} unchecked checklist items {}]",
                    range_tag(first_no, last_no)
                );
                if folded.len() < raw_bytes {
                    out.push(FoldedEntry {
                        first_line: first_no,
                        last_line: last_no,
                        text: folded,
                    });
                    did_fold = true;
                    i = last_item_i + 1;
                    continue;
                }
            }
        }

        out.push(FoldedEntry {
            first_line: line_no,
            last_line: line_no,
            text: raw_line.trim_end().to_owned(),
        });
        i += 1;
    }

    while out.last().is_some_and(|e| e.text.is_empty()) {
        out.pop();
    }
    (out, did_fold)
}

fn clean_bot_snippet_line(raw: &str) -> String {
    let stripped = MD_LINKED_IMG_RE.replace_all(raw, "");
    let stripped = MD_IMG_RE.replace_all(&stripped, "");
    let stripped = MD_LINK_RE.replace_all(&stripped, "$1");
    let stripped = HTML_TAG_RE.replace_all(&stripped, " ");
    let no_md = stripped
        .replace("**", "")
        .replace("__", "")
        .replace('`', "");
    let trimmed = no_md
        .trim()
        .trim_start_matches(['#', '>', '-', '*', '|', ' ', '\t'])
        .trim_end_matches(['|', ' ', '\t']);
    collapse_single_spaces(trimmed).into_owned()
}

fn is_md_table_separator(trimmed: &str) -> bool {
    trimmed.contains('-')
        && trimmed
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

fn extract_bot_snippet(body_lines: &[(usize, &str)]) -> Option<String> {
    let mut fallback: Option<String> = None;
    let mut in_comment = false;
    let mut details_depth = 0isize;
    let mut scanned = 0usize;

    for (idx, &(_, raw)) in body_lines.iter().enumerate() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if in_comment {
            if trimmed.contains("-->") {
                in_comment = false;
            }
            continue;
        }
        if trimmed.starts_with("<!--") {
            if !trimmed.contains("-->") {
                in_comment = true;
            }
            continue;
        }
        let low = trimmed.to_ascii_lowercase();
        if details_depth > 0 {
            details_depth += count_tag_occurrences(&low, "<details") as isize;
            details_depth -= count_tag_occurrences(&low, "</details>") as isize;
            continue;
        }
        if low.starts_with("<details>") || low.starts_with("<details ") {
            details_depth = count_tag_occurrences(&low, "<details") as isize
                - count_tag_occurrences(&low, "</details>") as isize;
            continue;
        }
        if is_badge_or_image_line(trimmed)
            || is_divider_line(trimmed)
            || is_md_table_separator(trimmed)
            || (trimmed.contains('|')
                && body_lines
                    .get(idx + 1)
                    .is_some_and(|(_, next)| is_md_table_separator(next.trim())))
        {
            continue;
        }
        scanned += 1;
        if scanned > 40 {
            break;
        }
        let cleaned = clean_bot_snippet_line(trimmed);
        if cleaned.len() < 4 {
            continue;
        }
        let clow = cleaned.to_ascii_lowercase();
        let high_signal = clow.contains("fail")
            || clow.contains("error")
            || clow.contains("blocked")
            || clow.contains("vulnerab")
            || clow.contains("coverage")
            || clow.contains("deploy")
            || clow.contains("preview")
            || clow.contains("ready")
            || clow.contains("passed")
            || clow.contains("status")
            || clow.contains("quality gate")
            || clow.contains("warning");
        if high_signal {
            let clipped = clip(&cleaned, 72).trim_end();
            return Some(if clipped.len() < cleaned.len() {
                format!("{clipped}...")
            } else {
                clipped.to_owned()
            });
        }
        if fallback.is_none() {
            let clipped = clip(&cleaned, 72).trim_end();
            fallback = Some(if clipped.len() < cleaned.len() {
                format!("{clipped}...")
            } else {
                clipped.to_owned()
            });
        }
    }
    fallback
}

/// Checks whether `lines[idx..]` starts a `gh`/`glab` non-TTY comment metadata block:
/// `author: ...`, optional `association:`/`edited:`/`status:`/... lines, then `--`.
/// Returns `(author, optional_status, body_start_idx)`.
fn parse_non_tty_comment_header(
    lines: &[Cow<'_, str>],
    idx: usize,
) -> Option<(String, Option<String>, usize)> {
    let (k, v) = parse_kv_line(lines.get(idx)?.as_ref())?;
    if !k.eq_ignore_ascii_case("author") || v.is_empty() {
        return None;
    }
    let author = v.trim_start_matches('@').to_owned();
    let mut status: Option<String> = None;
    let mut j = idx + 1;
    let max_j = (idx + 12).min(lines.len());
    while j < max_j {
        let t = lines[j].trim();
        if t == "--" {
            return Some((author, status, j + 1));
        }
        let (mk, mv) = parse_kv_line(lines[j].as_ref())?;
        if !is_comment_meta_key(mk) {
            return None;
        }
        if mk.eq_ignore_ascii_case("status") && !mv.is_empty() && !mv.eq_ignore_ascii_case("none") {
            status = Some(mv.to_owned());
        }
        j += 1;
    }
    None
}

fn split_non_tty_body_and_comments<'a>(
    lines: &'a [Cow<'a, str>],
    start: usize,
) -> (Vec<(usize, &'a str)>, Vec<ForgeComment<'a>>) {
    let mut body: Vec<(usize, &'a str)> = Vec::new();
    let mut comments: Vec<ForgeComment<'a>> = Vec::new();
    let mut i = start;

    while i < lines.len() {
        // Check for `--` followed by a comment header block, or a direct comment header block.
        let mut header_start = None;
        if lines[i].trim() == "--" {
            let mut next = i + 1;
            while next < lines.len() && lines[next].trim().is_empty() {
                next += 1;
            }
            if parse_non_tty_comment_header(lines, next).is_some() {
                header_start = Some(next);
            }
        } else if parse_non_tty_comment_header(lines, i).is_some() {
            header_start = Some(i);
        }

        if let Some(hstart) = header_start
            && let Some((author, status, body_start)) = parse_non_tty_comment_header(lines, hstart)
        {
            let mut cbody: Vec<(usize, &'a str)> = Vec::new();
            let mut j = body_start;
            while j < lines.len() {
                if lines[j].trim() == "--" {
                    let mut next = j + 1;
                    while next < lines.len() && lines[next].trim().is_empty() {
                        next += 1;
                    }
                    if parse_non_tty_comment_header(lines, next).is_some() {
                        break;
                    }
                } else if parse_non_tty_comment_header(lines, j).is_some() {
                    break;
                }
                cbody.push((j + 1, lines[j].as_ref()));
                j += 1;
            }
            while cbody.last().is_some_and(|(_, s)| s.trim().is_empty()) {
                cbody.pop();
            }
            let first_line = hstart + 1;
            let last_line = cbody.last().map_or(body_start, |(ln, _)| *ln);
            let header_display = match status {
                Some(st) => format!("{author} ({st})"),
                None => author.clone(),
            };
            comments.push(ForgeComment {
                author,
                header_display,
                first_line,
                last_line,
                body_lines: cbody,
            });
            i = j;
            continue;
        }

        body.push((i + 1, lines[i].as_ref()));
        i += 1;
    }

    while body.last().is_some_and(|(_, s)| s.trim().is_empty()) {
        body.pop();
    }
    (body, comments)
}

struct TtyForgeSections<'a> {
    header: Vec<String>,
    body: Vec<(usize, &'a str)>,
    comments: Vec<ForgeComment<'a>>,
    footer: Vec<String>,
}

fn split_tty_sections<'a>(lines: &'a [Cow<'a, str>]) -> Option<TtyForgeSections<'a>> {
    let mut recognized = false;
    for line in lines.iter().take(15) {
        let t = line.trim();
        if (t.contains(" • ") || t.contains(" · "))
            && (t.contains("Open")
                || t.contains("open")
                || t.contains("Closed")
                || t.contains("closed")
                || t.contains("Merged")
                || t.contains("merged")
                || t.contains("Draft")
                || t.contains("draft")
                || t.contains("wants to merge")
                || t.contains("opened "))
        {
            recognized = true;
            break;
        }
    }
    if !recognized {
        recognized = lines.iter().any(|l| is_forge_footer_line(l.trim()));
    }
    if !recognized {
        return None;
    }

    // Collect trailing footer lines (`View this ...` and glab trailing metadata block).
    let mut end_idx = lines.len();
    let mut footer_rev: Vec<String> = Vec::new();
    while end_idx > 0 {
        let t = lines[end_idx - 1].trim();
        if t.is_empty() {
            end_idx -= 1;
            continue;
        }
        if is_forge_footer_line(t)
            || (!footer_rev.is_empty()
                && (parse_kv_line(t).is_some_and(|(k, _)| is_forge_meta_key(k))
                    || (t.contains(" • ") && t.contains("comment"))))
        {
            footer_rev.push(t.to_owned());
            end_idx -= 1;
        } else {
            break;
        }
    }
    footer_rev.reverse();

    // Collect top metadata header block until first blank line (or body start).
    let mut header: Vec<String> = Vec::new();
    let mut idx = 0usize;
    while idx < end_idx && lines[idx].trim().is_empty() {
        idx += 1;
    }
    let mut saw_header_lines = 0usize;
    while idx < end_idx {
        let t = lines[idx].trim();
        if t.is_empty() {
            idx += 1;
            break;
        }
        if saw_header_lines >= 2
            && (t.starts_with('#')
                || t.starts_with("<!--")
                || t.to_ascii_lowercase().starts_with("<details")
                || is_badge_or_image_line(t)
                || is_unchecked_checkbox(t)
                || parse_tty_comment_header(t).is_some())
        {
            break;
        }
        header.push(t.to_owned());
        saw_header_lines += 1;
        idx += 1;
        if saw_header_lines >= 12 {
            break;
        }
    }

    if header.is_empty() {
        return None;
    }

    // Split middle region into body + TTY comments.
    let mut body: Vec<(usize, &'a str)> = Vec::new();
    let mut comments: Vec<ForgeComment<'a>> = Vec::new();
    let mut cur_comment: Option<ForgeComment<'a>> = None;
    let mut in_fence: Option<(char, usize)> = None;

    for (k, cow) in lines.iter().enumerate().take(end_idx).skip(idx) {
        let line_no = k + 1;
        let raw = cow.as_ref();
        let t = raw.trim();

        if let Some((fch, flen)) = in_fence {
            if let Some(c) = cur_comment.as_mut() {
                c.body_lines.push((line_no, raw));
                c.last_line = line_no;
            } else {
                body.push((line_no, raw));
            }
            if let Some((c, len)) = fence_marker(t)
                && c == fch
                && len >= flen
                && t.chars().skip(len).all(char::is_whitespace)
            {
                in_fence = None;
            }
            continue;
        }
        if let Some(fence) = fence_marker(t) {
            in_fence = Some(fence);
            if let Some(c) = cur_comment.as_mut() {
                c.body_lines.push((line_no, raw));
                c.last_line = line_no;
            } else {
                body.push((line_no, raw));
            }
            continue;
        }

        if let Some((author, display)) = parse_tty_comment_header(raw) {
            if let Some(mut prev) = cur_comment.take() {
                while prev.body_lines.last().is_some_and(|(_, s)| {
                    let st = s.trim();
                    st.is_empty() || is_divider_line(st)
                }) {
                    prev.body_lines.pop();
                }
                prev.last_line = prev
                    .body_lines
                    .last()
                    .map_or(prev.first_line, |(ln, _)| *ln);
                comments.push(prev);
            } else {
                // Trim trailing section divider / "Comments:" label before the first comment.
                while body.last().is_some_and(|(_, s)| {
                    let st = s.trim();
                    st.is_empty()
                        || is_divider_line(st)
                        || st.eq_ignore_ascii_case("comments:")
                        || st.eq_ignore_ascii_case("comments/notes:")
                }) {
                    body.pop();
                }
            }
            cur_comment = Some(ForgeComment {
                author,
                header_display: display,
                first_line: line_no,
                last_line: line_no,
                body_lines: Vec::new(),
            });
            continue;
        }

        if let Some(c) = cur_comment.as_mut() {
            c.body_lines.push((line_no, raw));
            c.last_line = line_no;
        } else {
            body.push((line_no, raw));
        }
    }

    if let Some(mut prev) = cur_comment.take() {
        while prev.body_lines.last().is_some_and(|(_, s)| {
            let st = s.trim();
            st.is_empty() || is_divider_line(st)
        }) {
            prev.body_lines.pop();
        }
        prev.last_line = prev
            .body_lines
            .last()
            .map_or(prev.first_line, |(ln, _)| *ln);
        comments.push(prev);
    }
    while body.last().is_some_and(|(_, s)| s.trim().is_empty()) {
        body.pop();
    }
    Some(TtyForgeSections {
        header,
        body,
        comments,
        footer: footer_rev,
    })
}

fn fit_forge_lines(body: &str, max_bytes: usize, first: usize, last: usize) -> String {
    let footer = format!("... [truncated; full view {}]\n", range_tag(first, last));
    if body.len() <= max_bytes {
        return body.to_owned();
    }
    let cap = max_bytes.saturating_sub(footer.len());
    let mut out = String::new();
    for line in body.split_inclusive('\n') {
        if out.len() + line.len() > cap {
            break;
        }
        out.push_str(line);
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&footer);
    out
}

fn format_forge_view(
    lines: &[Cow<'_, str>],
    raw_len: usize,
    _label: &str,
    max_bytes: usize,
) -> Option<String> {
    let first_non_empty = lines.iter().map(|l| l.trim()).find(|l| !l.is_empty())?;
    // Refuse JSON / machine-structured output.
    if first_non_empty.starts_with('{') || first_non_empty.starts_with('[') {
        return None;
    }

    // Try Format A: non-TTY `key:\tvalue` header + `--` separator.
    let mut header: Vec<String> = Vec::new();
    let mut body: Vec<(usize, &str)> = Vec::new();
    let mut comments: Vec<ForgeComment<'_>> = Vec::new();
    let mut footer: Vec<String> = Vec::new();
    let mut did_fold = false;

    let sep_idx = lines.iter().take(35).position(|l| l.trim() == "--");
    let mut parsed_non_tty = false;
    if let Some(sep) = sep_idx {
        let mut saw_title = false;
        let mut meta_count = 0usize;
        let mut all_valid = true;
        let mut temp_header = Vec::new();
        for line in &lines[..sep] {
            if line.trim().is_empty() {
                continue;
            }
            if let Some((k, v)) = parse_kv_line(line.as_ref())
                && is_forge_meta_key(k)
            {
                if k.eq_ignore_ascii_case("title") {
                    saw_title = true;
                }
                meta_count += 1;
                if !v.is_empty() {
                    temp_header.push(format!("{k}: {v}"));
                } else {
                    did_fold = true;
                }
            } else {
                all_valid = false;
                break;
            }
        }
        if all_valid && saw_title && meta_count >= 2 {
            header = temp_header;
            let (b, c) = split_non_tty_body_and_comments(lines, sep + 1);
            body = b;
            comments = c;
            parsed_non_tty = true;
        }
    }

    if !parsed_non_tty {
        let sections = split_tty_sections(lines)?;
        header = sections.header;
        body = sections.body;
        comments = sections.comments;
        footer = sections.footer;
    }

    let (folded_body, body_folded) = fold_markdown_lines(&body);
    did_fold |= body_folded;

    // Render comments: collapse bot comments to 1 summary line with `[stdout:A-B]`,
    // keep human review comments intact (folding only inner boilerplate).
    struct RenderedComment {
        first_line: usize,
        last_line: usize,
        is_bot: bool,
        text: String,
    }
    let mut rendered_comments: Vec<RenderedComment> = Vec::new();
    for comment in &comments {
        if is_bot_author(&comment.author) {
            let line_count = comment.last_line.saturating_sub(comment.first_line) + 1;
            let unit = if line_count == 1 { "line" } else { "lines" };
            let tag = range_tag(comment.first_line, comment.last_line);
            let line = match extract_bot_snippet(&comment.body_lines) {
                Some(snippet) => {
                    format!(
                        "- bot: {} — {snippet} ({line_count} {unit}) {tag}",
                        comment.author
                    )
                }
                None => format!("- bot: {} ({line_count} {unit}) {tag}", comment.author),
            };
            rendered_comments.push(RenderedComment {
                first_line: comment.first_line,
                last_line: comment.last_line,
                is_bot: true,
                text: line,
            });
            did_fold = true;
        } else {
            let (c_entries, c_folded) = fold_markdown_lines(&comment.body_lines);
            did_fold |= c_folded;
            let mut block = String::new();
            let _ = writeln!(block, "{}", comment.header_display);
            for entry in &c_entries {
                if entry.text.is_empty() {
                    block.push('\n');
                } else {
                    let _ = writeln!(block, "  {}", entry.text);
                }
            }
            rendered_comments.push(RenderedComment {
                first_line: comment.first_line,
                last_line: comment.last_line,
                is_bot: false,
                text: block.trim_end_matches('\n').to_owned(),
            });
        }
    }

    let mut header_block = String::new();
    for line in &header {
        let _ = writeln!(header_block, "{line}");
    }

    let mut footer_block = String::new();
    for line in &footer {
        let _ = writeln!(footer_block, "{line}");
    }

    let mut body_block = String::new();
    if !folded_body.is_empty() {
        body_block.push_str("--\n");
        for entry in &folded_body {
            let _ = writeln!(body_block, "{}", entry.text);
        }
    }

    let mut comments_block = String::new();
    if !rendered_comments.is_empty() {
        comments_block.push_str("-- Comments --\n");
        for rc in &rendered_comments {
            let _ = writeln!(comments_block, "{}", rc.text);
        }
    }

    let mut full = String::with_capacity(
        header_block.len() + body_block.len() + comments_block.len() + footer_block.len(),
    );
    full.push_str(&header_block);
    full.push_str(&body_block);
    full.push_str(&comments_block);
    full.push_str(&footer_block);

    if full.len() <= max_bytes {
        if did_fold || raw_len > max_bytes || full.len() * 10 <= raw_len * 9 {
            return Some(full);
        }
        return None;
    }

    // Over budget: preserve header, footer, and human review comments first,
    // then fit as much body + bot comment summaries as budget allows with exact `[stdout:A-B]`.
    let fixed_len = header_block.len() + footer_block.len();
    if fixed_len >= max_bytes {
        let trimmed = fit_forge_lines(&full, max_bytes, 1, lines.len());
        return (trimmed.len() <= max_bytes).then_some(trimmed);
    }

    let mut kept_comments = String::new();
    let mut omitted_comments_first = usize::MAX;
    let mut omitted_comments_last = 0usize;
    let mut omitted_comments_count = 0usize;
    let comment_budget = (max_bytes - fixed_len) * 2 / 3;

    if !rendered_comments.is_empty() {
        kept_comments.push_str("-- Comments --\n");
        // Prioritize human comments before bot summaries if comments overflow.
        let mut keep_flags = vec![false; rendered_comments.len()];
        let mut used = kept_comments.len();
        let reserve = 64usize;
        for pass_bot in [false, true] {
            for (idx, rc) in rendered_comments.iter().enumerate() {
                if rc.is_bot != pass_bot {
                    continue;
                }
                let needed = rc.text.len() + 1;
                if used + needed + reserve <= comment_budget {
                    keep_flags[idx] = true;
                    used += needed;
                }
            }
        }
        for (idx, rc) in rendered_comments.iter().enumerate() {
            if keep_flags[idx] {
                let _ = writeln!(kept_comments, "{}", rc.text);
            } else {
                omitted_comments_count += 1;
                omitted_comments_first = omitted_comments_first.min(rc.first_line);
                omitted_comments_last = omitted_comments_last.max(rc.last_line);
            }
        }
        if omitted_comments_count > 0 {
            let unit = if omitted_comments_count == 1 {
                "comment"
            } else {
                "comments"
            };
            let _ = writeln!(
                kept_comments,
                "+{omitted_comments_count} more {unit} {}",
                range_tag(omitted_comments_first, omitted_comments_last)
            );
        }
    }

    let body_budget = max_bytes.saturating_sub(fixed_len + kept_comments.len());
    let mut kept_body = String::new();
    if !folded_body.is_empty() && body_budget > 48 {
        kept_body.push_str("--\n");
        let reserve = 56usize;
        let mut cut_idx = folded_body.len();
        for (idx, entry) in folded_body.iter().enumerate() {
            let needed = entry.text.len() + 1;
            if kept_body.len() + needed + reserve <= body_budget {
                let _ = writeln!(kept_body, "{}", entry.text);
            } else {
                cut_idx = idx;
                break;
            }
        }
        if cut_idx < folded_body.len() {
            let first_omitted = folded_body[cut_idx].first_line;
            let last_omitted = folded_body.last().map_or(first_omitted, |e| e.last_line);
            let _ = writeln!(
                kept_body,
                "... [truncated body {}]",
                range_tag(first_omitted, last_omitted)
            );
        }
    } else if !folded_body.is_empty() {
        let first_omitted = folded_body.first().map_or(1, |e| e.first_line);
        let last_omitted = folded_body.last().map_or(first_omitted, |e| e.last_line);
        let note = format!(
            "--\n... [truncated body {}]\n",
            range_tag(first_omitted, last_omitted)
        );
        if note.len() <= body_budget {
            kept_body.push_str(&note);
        }
    }

    let mut out = String::with_capacity(
        header_block.len() + kept_body.len() + kept_comments.len() + footer_block.len(),
    );
    out.push_str(&header_block);
    out.push_str(&kept_body);
    out.push_str(&kept_comments);
    out.push_str(&footer_block);

    if out.len() > max_bytes {
        out = fit_forge_lines(&out, max_bytes, 1, lines.len());
    }
    (out.len() <= max_bytes).then_some(out)
}

// ===========================================================================
// 3. Security audits (npm / pnpm / yarn / cargo audit)
// ===========================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Critical,
    High,
    Moderate,
    Low,
    Info,
    Warning,
}

impl Severity {
    /// Strict match for a standalone severity cell (e.g. box-art left column),
    /// avoiding false positives on field labels such as `"More info"`.
    fn parse_exact(raw: &str) -> Option<Self> {
        let t = raw.trim();
        if t.eq_ignore_ascii_case("critical") {
            Some(Self::Critical)
        } else if t.eq_ignore_ascii_case("high") {
            Some(Self::High)
        } else if t.eq_ignore_ascii_case("moderate") || t.eq_ignore_ascii_case("medium") {
            Some(Self::Moderate)
        } else if t.eq_ignore_ascii_case("low") {
            Some(Self::Low)
        } else if t.eq_ignore_ascii_case("info") {
            Some(Self::Info)
        } else {
            None
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        let t = raw.trim();
        if let Some(exact) = Self::parse_exact(t) {
            return Some(exact);
        }
        if contains_ascii_icase(t, "critical") {
            Some(Self::Critical)
        } else if contains_ascii_icase(t, "high") {
            Some(Self::High)
        } else if contains_ascii_icase(t, "moderate") || contains_ascii_icase(t, "medium") {
            Some(Self::Moderate)
        } else if contains_ascii_icase(t, "low") {
            Some(Self::Low)
        } else if contains_ascii_icase(t, "warn")
            || contains_ascii_icase(t, "unmaintained")
            || contains_ascii_icase(t, "yanked")
            || contains_ascii_icase(t, "unsound")
        {
            Some(Self::Warning)
        } else if let Some(score) = t
            .split_whitespace()
            .next()
            .and_then(|tok| tok.parse::<f32>().ok())
            && score.is_finite()
            && score > 0.0
        {
            if score >= 9.0 {
                Some(Self::Critical)
            } else if score >= 7.0 {
                Some(Self::High)
            } else if score >= 4.0 {
                Some(Self::Moderate)
            } else {
                Some(Self::Low)
            }
        } else {
            None
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Moderate => "moderate",
            Self::Low => "low",
            Self::Info => "info",
            Self::Warning => "warning",
        }
    }
}

#[derive(Clone, Debug)]
struct Advisory {
    severity: Severity,
    severity_tag: String,
    package: String,
    id: String,
    title: String,
    patched: String,
    first_line: usize,
    last_line: usize,
}

static ADVISORY_ID_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\b(GHSA-[a-z0-9]{4}-[a-z0-9]{4}-[a-z0-9]{4}|CVE-\d{4}-\d{4,}|RUSTSEC-\d{4}-\d{4})\b",
    )
    .unwrap()
});

fn extract_advisory_id(text: &str) -> Option<&str> {
    ADVISORY_ID_RE.find(text).map(|m| m.as_str())
}

fn format_security_audit(lines: &[Cow<'_, str>], label: &str, max_bytes: usize) -> Option<String> {
    let (advisories, summary_tail) = if label == "cargo audit" {
        parse_cargo_audit(lines)
    } else {
        let box_parsed = parse_box_audit(lines);
        if !box_parsed.0.is_empty() {
            box_parsed
        } else {
            parse_npm_v7_audit(lines)
        }
    };

    if advisories.is_empty() {
        return None;
    }

    // Deduplicate identical advisories appearing across multiple dependency paths.
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    struct GroupKey {
        severity: Severity,
        package: String,
        id: String,
        title: String,
        patched: String,
        severity_tag: String,
    }

    struct Grouped {
        count: usize,
        first_line: usize,
        last_line: usize,
    }

    let mut groups: BTreeMap<GroupKey, Grouped> = BTreeMap::new();
    for adv in advisories {
        let key = GroupKey {
            severity: adv.severity,
            package: adv.package,
            id: adv.id,
            title: adv.title,
            patched: adv.patched,
            severity_tag: adv.severity_tag,
        };
        groups
            .entry(key)
            .and_modify(|g| {
                g.count += 1;
                g.first_line = g.first_line.min(adv.first_line);
                g.last_line = g.last_line.max(adv.last_line);
            })
            .or_insert(Grouped {
                count: 1,
                first_line: adv.first_line,
                last_line: adv.last_line,
            });
    }

    let mut counts = [0usize; 6];
    let mut total_occurrences = 0usize;
    for (k, g) in &groups {
        counts[k.severity as usize] += 1;
        total_occurrences += g.count;
    }
    let breakdown: Vec<String> = [
        (Severity::Critical, counts[0]),
        (Severity::High, counts[1]),
        (Severity::Moderate, counts[2]),
        (Severity::Low, counts[3]),
        (Severity::Info, counts[4]),
        (Severity::Warning, counts[5]),
    ]
    .into_iter()
    .filter(|(_, c)| *c > 0)
    .map(|(s, c)| format!("{c} {}", s.label()))
    .collect();

    let mut out = String::with_capacity(max_bytes.min(groups.len().saturating_mul(128) + 256));
    let distinct = groups.len();
    if total_occurrences > distinct {
        let _ = writeln!(
            out,
            "{label}: {distinct} distinct advisories across {total_occurrences} paths ({})",
            breakdown.join(", ")
        );
    } else {
        let _ = writeln!(
            out,
            "{label}: {distinct} {} ({})",
            if distinct == 1 {
                "advisory"
            } else {
                "advisories"
            },
            breakdown.join(", ")
        );
    }

    let mut footer = String::with_capacity(summary_tail.len() * 48);
    for s in &summary_tail {
        footer.push_str(s);
        footer.push('\n');
    }
    let reserve = footer.len() + 72;

    let mut kept = 0usize;
    let mut omitted_count = 0usize;
    let mut omitted_first = usize::MAX;
    let mut omitted_last = 0usize;
    let mut omitted_has_high_or_crit = false;
    let mut line_buf = String::with_capacity(192);
    let total_groups = groups.len();

    for (idx, (k, g)) in groups.into_iter().enumerate() {
        if omitted_count > 0 {
            omitted_count += 1;
            omitted_first = omitted_first.min(g.first_line);
            omitted_last = omitted_last.max(g.last_line);
            if matches!(k.severity, Severity::Critical | Severity::High) {
                omitted_has_high_or_crit = true;
            }
            continue;
        }
        line_buf.clear();
        let _ = write!(line_buf, "[{}] {}", k.severity_tag, k.package);
        if !k.id.is_empty() {
            let _ = write!(line_buf, " ({})", k.id);
        }
        if !k.title.is_empty() {
            let _ = write!(line_buf, ": {}", clip(&k.title, 120));
        }
        if !k.patched.is_empty() {
            let _ = write!(line_buf, " · fix: {}", clip(&k.patched, 64));
        }
        if g.count > 1 {
            let _ = write!(line_buf, " (×{} paths)", g.count);
        }
        let _ = writeln!(line_buf, " {}", range_tag(g.first_line, g.last_line));

        // When this is the final group, no folding note is needed so only reserve `footer.len()`.
        let needed_reserve = if idx + 1 == total_groups {
            footer.len()
        } else {
            reserve
        };
        if out.len() + line_buf.len() + needed_reserve <= max_bytes {
            out.push_str(&line_buf);
            kept += 1;
        } else {
            omitted_count = 1;
            omitted_first = g.first_line;
            omitted_last = g.last_line;
            if matches!(k.severity, Severity::Critical | Severity::High) {
                omitted_has_high_or_crit = true;
            }
        }
    }
    if omitted_count > 0 {
        let kind_label = if omitted_has_high_or_crit {
            "more advisories"
        } else {
            "lower-severity advisories"
        };
        let _ = writeln!(
            out,
            "+{omitted_count} {kind_label} {}",
            range_tag(omitted_first, omitted_last)
        );
    }
    if out.len() + footer.len() <= max_bytes {
        out.push_str(&footer);
    }
    (kept > 0 && out.len() <= max_bytes).then_some(out)
}

fn parse_box_row(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim();
    let sep = if trimmed.starts_with('│') && trimmed.ends_with('│') {
        '│'
    } else if trimmed.starts_with('|') && trimmed.ends_with('|') {
        '|'
    } else {
        return None;
    };
    let inner = trimmed.strip_prefix(sep)?.strip_suffix(sep)?;
    let (left, right) = inner.split_once(sep)?;
    Some((left.trim(), right.trim()))
}

fn is_box_border(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && (trimmed.starts_with(['┌', '├', '└'])
            || (trimmed.starts_with('+') && trimmed.ends_with('+') && trimmed.contains("---")))
}

fn parse_box_audit(lines: &[Cow<'_, str>]) -> (Vec<Advisory>, Vec<String>) {
    let mut advisories = Vec::new();
    let mut tail = Vec::new();

    let mut in_box = false;
    let mut first_line = 0usize;
    let mut last_line = 0usize;
    let mut severity: Option<Severity> = None;
    let mut title = String::new();
    let mut package = String::new();
    let mut patched = String::new();
    let mut id = String::new();
    let mut last_field = "";

    let flush = |advisories: &mut Vec<Advisory>,
                 in_box: &mut bool,
                 first: usize,
                 last: usize,
                 sev: &mut Option<Severity>,
                 title: &mut String,
                 pkg: &mut String,
                 patched: &mut String,
                 id: &mut String,
                 last_field: &mut &str| {
        if let Some(s) = sev.take()
            && !pkg.is_empty()
        {
            advisories.push(Advisory {
                severity: s,
                severity_tag: s.label().to_owned(),
                package: std::mem::take(pkg),
                id: std::mem::take(id),
                title: std::mem::take(title),
                patched: std::mem::take(patched),
                first_line: first,
                last_line: last,
            });
        }
        *in_box = false;
        title.clear();
        pkg.clear();
        patched.clear();
        id.clear();
        *last_field = "";
    };

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = cow.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('┌')
            || (!in_box
                && trimmed.starts_with('+')
                && trimmed.ends_with('+')
                && trimmed.contains("---"))
        {
            if in_box {
                flush(
                    &mut advisories,
                    &mut in_box,
                    first_line,
                    last_line,
                    &mut severity,
                    &mut title,
                    &mut package,
                    &mut patched,
                    &mut id,
                    &mut last_field,
                );
            }
            in_box = true;
            first_line = line_no;
            last_line = line_no;
            continue;
        }
        if in_box && (trimmed.starts_with('└')) {
            last_line = line_no;
            flush(
                &mut advisories,
                &mut in_box,
                first_line,
                last_line,
                &mut severity,
                &mut title,
                &mut package,
                &mut patched,
                &mut id,
                &mut last_field,
            );
            continue;
        }
        if in_box && is_box_border(trimmed) {
            last_line = line_no;
            continue;
        }
        if let Some((left, right)) = parse_box_row(trimmed) {
            // Back-to-back ASCII `+---+` boxes share the same border character for
            // top/middle/bottom; start a new box when a fresh severity row arrives
            // after `Package` was already seen.
            if in_box
                && severity.is_some()
                && !package.is_empty()
                && Severity::parse_exact(left).is_some()
            {
                let prev_end = line_no.saturating_sub(2).max(first_line);
                flush(
                    &mut advisories,
                    &mut in_box,
                    first_line,
                    prev_end,
                    &mut severity,
                    &mut title,
                    &mut package,
                    &mut patched,
                    &mut id,
                    &mut last_field,
                );
                in_box = true;
                first_line = line_no.saturating_sub(1).max(1);
            }
            if !in_box {
                in_box = true;
                first_line = line_no;
            }
            last_line = line_no;
            if severity.is_none()
                && let Some(sev) = Severity::parse_exact(left)
            {
                severity = Some(sev);
                title = right.to_owned();
                last_field = "title";
                if let Some(found) = extract_advisory_id(right) {
                    id = found.to_owned();
                }
                continue;
            }
            match left {
                "Package" => {
                    package = right.to_owned();
                    last_field = "package";
                }
                "Patched in" | "Patched versions" => {
                    patched = right.to_owned();
                    last_field = "patched";
                }
                "More info" => {
                    if let Some(found) = extract_advisory_id(right) {
                        id = found.to_owned();
                    } else if id.is_empty() && !right.is_empty() {
                        id = right.to_owned();
                    }
                    last_field = "more_info";
                }
                "" => match last_field {
                    "title" if !right.is_empty() => {
                        title.push(' ');
                        title.push_str(right);
                    }
                    "patched" if !right.is_empty() => {
                        patched.push(' ');
                        patched.push_str(right);
                    }
                    "more_info" if id.is_empty() => {
                        if let Some(found) = extract_advisory_id(right) {
                            id = found.to_owned();
                        }
                    }
                    _ => {}
                },
                _ => last_field = "",
            }
            continue;
        }
        if in_box {
            flush(
                &mut advisories,
                &mut in_box,
                first_line,
                last_line,
                &mut severity,
                &mut title,
                &mut package,
                &mut patched,
                &mut id,
                &mut last_field,
            );
        }
        if trimmed.contains("vulnerabilities found")
            || trimmed.contains("vulnerability found")
            || trimmed.starts_with("found ")
        {
            tail.push(trimmed.to_owned());
        }
    }
    if in_box {
        flush(
            &mut advisories,
            &mut in_box,
            first_line,
            last_line,
            &mut severity,
            &mut title,
            &mut package,
            &mut patched,
            &mut id,
            &mut last_field,
        );
    }
    (advisories, tail)
}

fn parse_npm_v7_audit(lines: &[Cow<'_, str>]) -> (Vec<Advisory>, Vec<String>) {
    let mut advisories = Vec::new();
    let mut tail = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() || trimmed == "# npm audit report" {
            i += 1;
            continue;
        }
        if (trimmed.contains("vulnerabilities") || trimmed.contains("vulnerability"))
            && trimmed.starts_with(|c: char| c.is_ascii_digit())
        {
            tail.push(trimmed.to_owned());
            i += 1;
            continue;
        }
        // An npm v7 package block has `<pkg>  <range>` (at indent 0) followed by `Severity: <level>`.
        if indent_of(lines[i].as_ref()) == 0
            && i + 1 < lines.len()
            && let Some(sev_raw) = lines[i + 1].trim().strip_prefix("Severity:")
            && let Some(severity) = Severity::parse(sev_raw)
        {
            let first_line = i + 1;
            let pkg_header = collapse_single_spaces(trimmed).into_owned();
            let mut last_line = i + 2;
            let mut titles: Vec<(String, String)> = Vec::new();
            let mut fix = String::new();
            let mut j = i + 2;
            while j < lines.len() {
                let raw_j = lines[j].as_ref();
                let t = raw_j.trim();
                if t.is_empty() {
                    break;
                }
                last_line = j + 1;
                if let Some(id_found) = extract_advisory_id(t) {
                    let title = t.split(" - http").next().unwrap_or(t).trim().to_owned();
                    titles.push((id_found.to_owned(), title));
                } else if t.starts_with("fix available") || t.starts_with("No fix available") {
                    fix = t.to_owned();
                } else if t.starts_with("Depends on vulnerable versions of ") && titles.is_empty() {
                    titles.push((String::new(), t.to_owned()));
                }
                j += 1;
            }
            if titles.is_empty() {
                advisories.push(Advisory {
                    severity,
                    severity_tag: severity.label().to_owned(),
                    package: pkg_header,
                    id: String::new(),
                    title: String::new(),
                    patched: fix,
                    first_line,
                    last_line,
                });
            } else {
                for (adv_id, title) in titles {
                    advisories.push(Advisory {
                        severity,
                        severity_tag: severity.label().to_owned(),
                        package: pkg_header.clone(),
                        id: adv_id,
                        title,
                        patched: fix.clone(),
                        first_line,
                        last_line,
                    });
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }

    (advisories, tail)
}

fn parse_cargo_audit(lines: &[Cow<'_, str>]) -> (Vec<Advisory>, Vec<String>) {
    let mut advisories = Vec::new();
    let mut tail = Vec::new();

    let mut active = false;
    let mut first_line = 0usize;
    let mut last_line = 0usize;
    let mut crate_name = String::new();
    let mut version = String::new();
    let mut id = String::new();
    let mut title = String::new();
    let mut severity: Option<Severity> = None;
    let mut warning_kind = String::new();
    let mut solution = String::new();

    let flush = |advisories: &mut Vec<Advisory>,
                 active: &mut bool,
                 first: usize,
                 last: usize,
                 crate_name: &mut String,
                 version: &mut String,
                 id: &mut String,
                 title: &mut String,
                 severity: &mut Option<Severity>,
                 warning_kind: &mut String,
                 solution: &mut String| {
        if *active && !crate_name.is_empty() {
            let pkg = if version.is_empty() {
                std::mem::take(crate_name)
            } else {
                format!("{} {}", std::mem::take(crate_name), std::mem::take(version))
            };
            let (sev, sev_tag) = if let Some(s) = severity.take() {
                (s, s.label().to_owned())
            } else if !warning_kind.is_empty() {
                (
                    Severity::Warning,
                    format!("warning:{}", std::mem::take(warning_kind)),
                )
            } else {
                // Vulnerability without explicit CVSS score: rank as High so it is never hidden.
                (Severity::High, "vulnerability".to_owned())
            };
            advisories.push(Advisory {
                severity: sev,
                severity_tag: sev_tag,
                package: pkg,
                id: std::mem::take(id),
                title: std::mem::take(title),
                patched: std::mem::take(solution),
                first_line: first,
                last_line: last,
            });
        }
        *active = false;
        crate_name.clear();
        version.clear();
        id.clear();
        title.clear();
        *severity = None;
        warning_kind.clear();
        solution.clear();
    };

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = cow.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Crate:") {
            flush(
                &mut advisories,
                &mut active,
                first_line,
                last_line,
                &mut crate_name,
                &mut version,
                &mut id,
                &mut title,
                &mut severity,
                &mut warning_kind,
                &mut solution,
            );
            active = true;
            first_line = line_no;
            last_line = line_no;
            crate_name = rest.trim().to_owned();
            continue;
        }
        if trimmed.starts_with("error:") || trimmed.starts_with("warning:") {
            flush(
                &mut advisories,
                &mut active,
                first_line,
                last_line,
                &mut crate_name,
                &mut version,
                &mut id,
                &mut title,
                &mut severity,
                &mut warning_kind,
                &mut solution,
            );
            tail.push(trimmed.to_owned());
            continue;
        }
        if !active {
            continue;
        }
        last_line = line_no;
        if let Some(rest) = trimmed.strip_prefix("Version:") {
            version = rest.trim().to_owned();
        } else if let Some(rest) = trimmed.strip_prefix("Title:") {
            title = rest.trim().to_owned();
        } else if let Some(rest) = trimmed.strip_prefix("ID:") {
            id = rest.trim().to_owned();
        } else if let Some(rest) = trimmed.strip_prefix("Severity:") {
            severity = Severity::parse(rest);
        } else if let Some(rest) = trimmed.strip_prefix("Warning:") {
            warning_kind = rest.trim().to_owned();
        } else if let Some(rest) = trimmed.strip_prefix("Solution:") {
            solution = rest.trim().to_owned();
        }
    }
    flush(
        &mut advisories,
        &mut active,
        first_line,
        last_line,
        &mut crate_name,
        &mut version,
        &mut id,
        &mut title,
        &mut severity,
        &mut warning_kind,
        &mut solution,
    );
    (advisories, tail)
}

// ===========================================================================
// 4. Package & state inventories
// ===========================================================================

fn is_terraform_address(line: &str) -> bool {
    if line.is_empty() || line.contains(char::is_whitespace) || !line.contains('.') {
        return false;
    }
    line.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'_' | b'-' | b'.' | b'[' | b']' | b'"' | b'\'' | b'/' | b':'
            )
    })
}

/// Split a Terraform address into dot-separated segments outside `[...]`,
/// then build the `(module_prefix, resource_type)` group key.
fn terraform_state_group(addr: &str) -> Option<String> {
    let mut segs: Vec<&str> = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, b) in addr.bytes().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => depth = depth.saturating_sub(1),
            b'.' if depth == 0 => {
                segs.push(&addr[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    segs.push(&addr[start..]);
    if segs.len() < 2 {
        return None;
    }

    let mut prefix = Vec::new();
    let mut i = 0;
    while i + 1 < segs.len() && segs[i] == "module" {
        prefix.push("module");
        // Strip index `[...]` from module instances so `module.net[0].aws_subnet.a` groups cleanly.
        let mod_name = segs[i + 1].split('[').next().unwrap_or(segs[i + 1]);
        prefix.push(mod_name);
        i += 2;
    }
    if i >= segs.len() {
        return None;
    }
    if segs[i] == "data" && i + 1 < segs.len() {
        prefix.push("data");
        prefix.push(segs[i + 1]);
    } else {
        prefix.push(segs[i]);
    }
    Some(prefix.join("."))
}

fn format_terraform_state_list(
    lines: &[Cow<'_, str>],
    label: &str,
    max_bytes: usize,
) -> Option<String> {
    struct StateGroup<'a> {
        count: usize,
        first: usize,
        last: usize,
        single_addr: &'a str,
    }

    let mut groups: BTreeMap<String, StateGroup<'_>> = BTreeMap::new();
    let mut total = 0usize;

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = cow.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !is_terraform_address(trimmed) {
            return None;
        }
        let group = terraform_state_group(trimmed)?;
        total += 1;
        groups
            .entry(group)
            .and_modify(|g| {
                g.count += 1;
                g.first = g.first.min(line_no);
                g.last = g.last.max(line_no);
            })
            .or_insert(StateGroup {
                count: 1,
                first: line_no,
                last: line_no,
                single_addr: trimmed,
            });
    }

    if total < 4 {
        return None;
    }

    let mut ranked: Vec<(String, StateGroup<'_>)> = groups.into_iter().collect();
    ranked.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(&b.0)));

    let mut out = String::with_capacity(max_bytes.min(ranked.len().saturating_mul(56) + 64));
    let _ = writeln!(
        out,
        "{label}: {total} resources across {} {}",
        ranked.len(),
        if ranked.len() == 1 { "type" } else { "types" }
    );
    let reserve = 72usize;
    let mut kept = 0usize;
    let mut line_buf = String::with_capacity(96);
    for (group, info) in &ranked {
        line_buf.clear();
        if info.count == 1 {
            let _ = writeln!(
                line_buf,
                "  {} {}",
                info.single_addr,
                range_tag(info.first, info.last)
            );
        } else {
            let _ = writeln!(
                line_buf,
                "  {group} ×{} {}",
                info.count,
                range_tag(info.first, info.last)
            );
        }
        if out.len() + line_buf.len() + reserve <= max_bytes {
            out.push_str(&line_buf);
            kept += 1;
        } else {
            break;
        }
    }
    if kept < ranked.len() {
        let rest_resources: usize = ranked[kept..].iter().map(|(_, g)| g.count).sum();
        let first = ranked[kept..]
            .iter()
            .map(|(_, g)| g.first)
            .min()
            .unwrap_or(1);
        let last = ranked[kept..]
            .iter()
            .map(|(_, g)| g.last)
            .max()
            .unwrap_or(first);
        let _ = writeln!(
            out,
            "  +{} more types ({rest_resources} resources) {}",
            ranked.len() - kept,
            range_tag(first, last)
        );
    }
    (kept > 0 && out.len() <= max_bytes).then_some(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum BumpKind {
    Major,
    Minor,
    Patch,
}

fn parse_semver(raw: &str) -> Option<(u64, u64, u64)> {
    let cleaned = raw.trim().trim_start_matches('v');
    let core = cleaned.split(['-', '+', ' ']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts
        .next()
        .and_then(|p| p.parse::<u64>().ok())
        .unwrap_or(0);
    let patch = parts
        .next()
        .and_then(|p| p.parse::<u64>().ok())
        .unwrap_or(0);
    Some((major, minor, patch))
}

fn classify_bump(current: &str, latest: &str) -> BumpKind {
    let (Some((cmaj, cmin, _)), Some((lmaj, lmin, _))) =
        (parse_semver(current), parse_semver(latest))
    else {
        return BumpKind::Major;
    };
    if lmaj > cmaj || (cmaj == 0 && lmin > cmin) {
        BumpKind::Major
    } else if lmaj == cmaj && lmin > cmin {
        BumpKind::Minor
    } else {
        BumpKind::Patch
    }
}

fn split_table_cells(line: &str) -> Vec<&str> {
    let trimmed = line.trim();
    if (trimmed.starts_with('│') && trimmed.ends_with('│'))
        || (trimmed.starts_with('|') && trimmed.ends_with('|'))
    {
        let sep = if trimmed.starts_with('│') {
            '│'
        } else {
            '|'
        };
        return trimmed
            .strip_prefix(sep)
            .and_then(|s| s.strip_suffix(sep))
            .unwrap_or(trimmed)
            .split(sep)
            .map(str::trim)
            .collect();
    }
    if trimmed.contains('\t') {
        trimmed.split('\t').map(str::trim).collect()
    } else {
        trimmed.split_whitespace().collect()
    }
}

fn format_outdated(lines: &[Cow<'_, str>], label: &str, max_bytes: usize) -> Option<String> {
    struct OutdatedRow<'a> {
        name: &'a str,
        current: &'a str,
        latest: &'a str,
        bump: BumpKind,
        line_no: usize,
    }

    let mut header_cols: Option<(usize, usize, usize)> = None; // (name_idx, current_idx, latest_idx)
    let mut rows: Vec<OutdatedRow<'_>> = Vec::with_capacity(lines.len().min(512));

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = cow.as_ref().trim();
        if trimmed.is_empty()
            || is_box_border(trimmed)
            || trimmed
                .bytes()
                .all(|b| matches!(b, b'-' | b'=' | b' ' | b'\t'))
            || trimmed.starts_with("Color legend")
            || trimmed.starts_with("info ")
        {
            continue;
        }
        let cells = split_table_cells(trimmed);
        if cells.len() < 3 {
            continue;
        }
        if header_cols.is_none() {
            let name_i = cells
                .iter()
                .position(|c| c.eq_ignore_ascii_case("package") || c.eq_ignore_ascii_case("name"));
            let cur_i = cells.iter().position(|c| {
                c.eq_ignore_ascii_case("current")
                    || c.eq_ignore_ascii_case("project")
                    || c.eq_ignore_ascii_case("version")
            });
            let lat_i = cells.iter().position(|c| c.eq_ignore_ascii_case("latest"));
            if let (Some(n), Some(c), Some(l)) = (name_i, cur_i, lat_i) {
                header_cols = Some((n, c, l));
                continue;
            }
            // Ignore leading banner lines like `=== Crate ===` in cargo-outdated workspace output.
            continue;
        }
        let (ni, ci, li) = header_cols?;
        if cells.len() <= ni.max(ci).max(li) {
            continue;
        }
        let name = cells[ni];
        let current = cells[ci];
        let latest = cells[li];
        // Skip repeated headers in multi-workspace `cargo outdated`.
        if name.eq_ignore_ascii_case("name") || name.eq_ignore_ascii_case("package") {
            continue;
        }
        if name.is_empty() || current.is_empty() || latest.is_empty() {
            continue;
        }
        let bump = classify_bump(current, latest);
        rows.push(OutdatedRow {
            name,
            current,
            latest,
            bump,
            line_no,
        });
    }

    if rows.len() < 3 {
        return None;
    }

    let major: Vec<&OutdatedRow<'_>> = rows.iter().filter(|r| r.bump == BumpKind::Major).collect();
    let minor: Vec<&OutdatedRow<'_>> = rows.iter().filter(|r| r.bump == BumpKind::Minor).collect();
    let patch: Vec<&OutdatedRow<'_>> = rows.iter().filter(|r| r.bump == BumpKind::Patch).collect();

    let mut out = String::with_capacity(max_bytes.min(rows.len().saturating_mul(36) + 128));
    let _ = writeln!(
        out,
        "{label}: {} packages ({} major, {} minor, {} patch)",
        rows.len(),
        major.len(),
        minor.len(),
        patch.len()
    );
    let reserve = 72usize;

    // 1. Highlight 100% of major bumps first.
    if !major.is_empty() {
        let _ = writeln!(out, "Major ({}):", major.len());
        let mut kept_major = 0usize;
        let mut line_buf = String::with_capacity(96);
        for row in &major {
            line_buf.clear();
            let _ = writeln!(
                line_buf,
                "  {} {} -> {} {}",
                row.name,
                row.current,
                row.latest,
                range_tag(row.line_no, row.line_no)
            );
            if out.len() + line_buf.len() + reserve <= max_bytes {
                out.push_str(&line_buf);
                kept_major += 1;
            } else {
                break;
            }
        }
        if kept_major < major.len() {
            let first = major[kept_major].line_no;
            let last = major.last().map_or(first, |r| r.line_no);
            let _ = writeln!(
                out,
                "  +{} more major {}",
                major.len() - kept_major,
                range_tag(first, last)
            );
        }
    }

    // 2. Pack minor and patch bumps compactly, folding overflow with exact `[stdout:A-B]`.
    let append_packed = |out: &mut String, title: &str, group: &[&OutdatedRow<'_>]| {
        if group.is_empty() {
            return;
        }
        let first_all = group.first().map_or(1, |r| r.line_no);
        let last_all = group.last().map_or(first_all, |r| r.line_no);
        let heading = format!(
            "{title} ({}) {}:\n",
            group.len(),
            range_tag(first_all, last_all)
        );
        if out.len() + heading.len() + 64 > max_bytes {
            let summary = format!(
                "{title}: {} updates {}\n",
                group.len(),
                range_tag(first_all, last_all)
            );
            if out.len() + summary.len() <= max_bytes {
                out.push_str(&summary);
            }
            return;
        }
        out.push_str(&heading);
        let mut current_line = String::with_capacity(104);
        current_line.push_str("  ");
        let mut kept = 0usize;
        for row in group {
            let token_len = row.name.len() + 1 + row.current.len() + 2 + row.latest.len();
            let sep = if current_line.len() > 2 { ", " } else { "" };
            if current_line.len() + sep.len() + token_len > 96 && current_line.len() > 2 {
                current_line.push('\n');
                if out.len() + current_line.len() + reserve <= max_bytes {
                    out.push_str(&current_line);
                    current_line.clear();
                    let _ = write!(
                        current_line,
                        "  {} {}->{}",
                        row.name, row.current, row.latest
                    );
                    kept += 1;
                } else {
                    current_line.clear();
                    break;
                }
            } else if out.len() + current_line.len() + sep.len() + token_len + 1 + reserve
                <= max_bytes
            {
                current_line.push_str(sep);
                let _ = write!(current_line, "{} {}->{}", row.name, row.current, row.latest);
                kept += 1;
            } else {
                break;
            }
        }
        if current_line.len() > 2 {
            current_line.push('\n');
            if out.len() + current_line.len() + 64 <= max_bytes {
                out.push_str(&current_line);
            }
        }
        if kept < group.len() {
            let first = group[kept].line_no;
            let last = group.last().map_or(first, |r| r.line_no);
            let _ = writeln!(
                out,
                "  +{} more {} {}",
                group.len() - kept,
                title.to_ascii_lowercase(),
                range_tag(first, last)
            );
        }
    };

    append_packed(&mut out, "Minor", &minor);
    append_packed(&mut out, "Patch", &patch);

    (out.len() <= max_bytes).then_some(out)
}

fn format_pip_list(lines: &[Cow<'_, str>], raw_len: usize, max_bytes: usize) -> Option<String> {
    let mut saw_header = false;
    let mut editable: Vec<(usize, Cow<'_, str>)> = Vec::new();
    let mut packages: Vec<(usize, Cow<'_, str>)> = Vec::with_capacity(lines.len().min(512));

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = cow.as_ref().trim();
        if trimmed.is_empty() {
            continue;
        }
        if !saw_header {
            let mut parts = trimmed.split_whitespace();
            let first = parts.next().unwrap_or("");
            let second = parts.next().unwrap_or("");
            if first.eq_ignore_ascii_case("package") && second.eq_ignore_ascii_case("version") {
                saw_header = true;
                continue;
            }
            return None;
        }
        if trimmed.bytes().all(|b| matches!(b, b'-' | b' ')) {
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 2 {
            return None;
        }
        if parts.len() >= 3 {
            // Editable install with local path: preserve verbatim at top.
            editable.push((
                line_no,
                Cow::Owned(format!(
                    "{}=={} ({})",
                    parts[0],
                    parts[1],
                    parts[2..].join(" ")
                )),
            ));
        } else {
            packages.push((line_no, Cow::Owned(format!("{}=={}", parts[0], parts[1]))));
        }
    }

    if !saw_header || packages.len() + editable.len() < 6 {
        return None;
    }

    pack_package_list("pip list", &editable, &packages, raw_len, max_bytes)
}

fn format_pip_freeze(lines: &[Cow<'_, str>], raw_len: usize, max_bytes: usize) -> Option<String> {
    let mut editable: Vec<(usize, Cow<'_, str>)> = Vec::new();
    let mut packages: Vec<(usize, Cow<'_, str>)> = Vec::with_capacity(lines.len().min(512));

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = cow.as_ref().trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with("-e ") || trimmed.contains(" @ ") {
            editable.push((line_no, Cow::Borrowed(trimmed)));
        } else if let Some((name, ver)) = trimmed.split_once("==")
            && !name.is_empty()
            && !ver.is_empty()
            && !name.contains(char::is_whitespace)
            && !ver.contains(char::is_whitespace)
        {
            packages.push((line_no, Cow::Borrowed(trimmed)));
        } else {
            return None;
        }
    }

    if packages.len() + editable.len() < 6 {
        return None;
    }

    pack_package_list("pip freeze", &editable, &packages, raw_len, max_bytes)
}

fn pack_package_list(
    label: &str,
    editable: &[(usize, Cow<'_, str>)],
    packages: &[(usize, Cow<'_, str>)],
    raw_len: usize,
    max_bytes: usize,
) -> Option<String> {
    let total = editable.len() + packages.len();
    let mut out = String::with_capacity(max_bytes.min(raw_len));
    if editable.is_empty() {
        let _ = writeln!(out, "{label}: {total} packages");
    } else {
        let _ = writeln!(
            out,
            "{label}: {total} packages ({} editable/local)",
            editable.len()
        );
    }
    let reserve = 72usize;
    let mut line_buf = String::with_capacity(128);
    for (line_no, entry) in editable {
        line_buf.clear();
        let _ = writeln!(line_buf, "  {entry} {}", range_tag(*line_no, *line_no));
        if out.len() + line_buf.len() + reserve <= max_bytes {
            out.push_str(&line_buf);
        }
    }

    // Build wrapped lines (~96 chars each) tracking first/last stdout line per packed row.
    let mut packed_rows: Vec<(usize, usize, String)> = Vec::new();
    let mut cur = String::with_capacity(104);
    let mut cur_first = 0usize;
    let mut cur_last = 0usize;
    for (line_no, pkg) in packages {
        let sep = if cur.is_empty() { "" } else { ", " };
        if !cur.is_empty() && cur.len() + sep.len() + pkg.len() > 96 {
            packed_rows.push((cur_first, cur_last, std::mem::take(&mut cur)));
            cur.reserve(104);
            cur_first = *line_no;
            cur_last = *line_no;
            cur.push_str(pkg);
        } else {
            if cur.is_empty() {
                cur_first = *line_no;
            }
            cur_last = *line_no;
            cur.push_str(sep);
            cur.push_str(pkg);
        }
    }
    if !cur.is_empty() {
        packed_rows.push((cur_first, cur_last, cur));
    }

    // Check if all packed rows fit.
    let all_rows_bytes: usize = packed_rows.iter().map(|(_, _, r)| r.len() + 1).sum();
    if out.len() + all_rows_bytes <= max_bytes {
        for (_, _, r) in &packed_rows {
            out.push_str(r);
            out.push('\n');
        }
        // When raw already fit in budget, require >=15% savings so we don't churn tiny lists.
        if raw_len > max_bytes || out.len() * 20 <= raw_len * 17 {
            return Some(out);
        }
        return None;
    }

    // Over budget: keep head rows + tail row and fold the middle with exact `[stdout:A-B]`.
    let tail_row = packed_rows.last().cloned();
    let tail_reserve = tail_row.as_ref().map_or(0, |(_, _, r)| r.len() + 1);
    let mut kept_head = 0usize;
    for (_, _, r) in packed_rows.iter().take(packed_rows.len().saturating_sub(1)) {
        if out.len() + r.len() + 1 + reserve + tail_reserve <= max_bytes {
            out.push_str(r);
            out.push('\n');
            kept_head += 1;
        } else {
            break;
        }
    }
    let has_tail = tail_row.as_ref().is_some_and(|(_, _, r)| {
        packed_rows.len() > kept_head + 1 && out.len() + r.len() + 1 + reserve <= max_bytes
    });
    let omitted_end = if has_tail {
        packed_rows.len() - 1
    } else {
        packed_rows.len()
    };
    if kept_head < omitted_end {
        let first_line = packed_rows[kept_head].0;
        let last_line = packed_rows[omitted_end - 1].1;
        let omitted_pkgs = packages
            .iter()
            .filter(|(n, _)| *n >= first_line && *n <= last_line)
            .count();
        let _ = writeln!(
            out,
            "+{omitted_pkgs} more packages {}",
            range_tag(first_line, last_line)
        );
    }
    if has_tail && let Some((_, _, r)) = tail_row {
        out.push_str(&r);
        out.push('\n');
    }
    (out.len() <= max_bytes).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubectl_describe_preserves_unhealthy_state_and_folds_boilerplate() {
        let last_applied = format!(
            "{{\"apiVersion\":\"v1\",\"kind\":\"Pod\",\"metadata\":{{\"name\":\"api-7f8d\"}},\"spec\":{{\"containers\":[{{\"name\":\"app\",\"env\":[{}]}}]}}}}",
            (0..60)
                .map(|i| format!("{{\"name\":\"VAR_{i}\",\"value\":\"payload-value-{i:04}\"}}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut raw = format!(
            "\
Name:             api-7f8d9c6b5-abcde
Namespace:        prod
Priority:         0
Service Account:  api-sa
Node:             worker-node-03/10.0.1.42
Start Time:       Wed, 25 Nov 2026 10:00:00 +0000
Labels:           app=api
                  pod-template-hash=7f8d9c6b5
                  tier=backend
                  version=v2.4.1
Annotations:      cni.projectcalico.org/podIP: 10.244.1.5/32
                  kubectl.kubernetes.io/last-applied-configuration: {last_applied}
                  prometheus.io/port: 9090
                  prometheus.io/scrape: true
Status:           Running
IP:               10.244.1.5
IPs:
  IP:           10.244.1.5
Controlled By:    ReplicaSet/api-7f8d9c6b5
Containers:
  app:
    Container ID:   containerd://1234567890abcdef
    Image:          ghcr.io/acme/api:v2.4.1
    Port:           8080/TCP
    State:          Waiting
      Reason:       CrashLoopBackOff
    Last State:     Terminated
      Reason:       OOMKilled
      Exit Code:    137
      Started:      Wed, 25 Nov 2026 10:04:00 +0000
      Finished:     Wed, 25 Nov 2026 10:04:12 +0000
    Ready:          False
    Restart Count:  7
    Limits:
      memory:  256Mi
    Environment:
"
        );
        for i in 0..40 {
            let _ = writeln!(raw, "      APP_SETTING_{i:02}:  value-{i:04}");
        }
        raw.push_str(
            "    Mounts:
      /etc/config from app-config (ro)
      /var/run/secrets/kubernetes.io/serviceaccount from kube-api-access-xyz (ro)
  sidecar:
    Container ID:   containerd://abcdef1234567890
    Image:          envoyproxy/envoy:v1.31.0
    State:          Running
      Started:      Wed, 25 Nov 2026 10:00:05 +0000
    Ready:          True
    Restart Count:  0
    Environment:
      ENVOY_LOG_LEVEL:  info
    Mounts:
      /etc/envoy from envoy-cfg (ro)
Conditions:
  Type                        Status
  PodReadyToStartContainers   True 
  Initialized                 True 
  Ready                       False 
  ContainersReady             False 
  PodScheduled                True 
Volumes:
  app-config:
    Type:      ConfigMap (a volume populated by a ConfigMap)
    Name:      api-config
  envoy-cfg:
    Type:      ConfigMap (a volume populated by a ConfigMap)
    Name:      envoy-config
  kube-api-access-xyz:
    Type:                    Projected (a volume that contains injected data)
    TokenExpirationSeconds:  3607
Events:
  Type     Reason     Age                From               Message
  ----     ------     ----               ----               -------
  Normal   Scheduled  10m                default-scheduler  Successfully assigned prod/api-7f8d9c6b5-abcde to worker-node-03
  Normal   Pulled     9m                 kubelet            Container image \"ghcr.io/acme/api:v2.4.1\" already present on machine
  Normal   Created    9m                 kubelet            Created container app
  Normal   Started    9m                 kubelet            Started container app
  Warning  BackOff    2m (x18 over 8m)   kubelet            Back-off restarting failed container app in pod api-7f8d9c6b5-abcde
",
        );

        let view = format(
            &["kubectl", "describe", "pod", "api-7f8d9c6b5-abcde"],
            &raw,
            4096,
        )
        .expect("kubectl describe compacted");
        let reduction_pct = (raw.len() - view.len()) * 100 / raw.len();
        assert!(
            reduction_pct >= 70,
            "expected >=70% reduction, got {reduction_pct}% ({} -> {})\n{view}",
            raw.len(),
            view.len()
        );
        // Identity preserved.
        assert!(view.contains("Name: api-7f8d9c6b5-abcde"), "{view}");
        assert!(view.contains("Namespace: prod"), "{view}");
        assert!(
            view.contains("Controlled By: ReplicaSet/api-7f8d9c6b5"),
            "{view}"
        );
        // Unhealthy container preserved verbatim (Wait/CrashLoopBackOff/OOMKilled/Exit Code 137/Restart Count 7).
        assert!(view.contains("CrashLoopBackOff"), "{view}");
        assert!(view.contains("OOMKilled"), "{view}");
        assert!(view.contains("Exit Code: 137"), "{view}");
        assert!(view.contains("Restart Count: 7"), "{view}");
        // Healthy container collapsed to 1 line.
        assert!(
            view.contains("sidecar: Running, Ready=True, Restarts=0 (envoyproxy/envoy:v1.31.0)"),
            "{view}"
        );
        // Boilerplate folded with exact pointers.
        assert!(
            view.contains(
                "Annotations: 4 entries (includes last-applied-configuration) [stdout:11-14]"
            ),
            "{view}"
        );
        assert!(view.contains("Environment: 40 vars [stdout:"), "{view}");
        assert!(view.contains("Volumes: 3 volumes [stdout:"), "{view}");
        // Non-True conditions kept verbatim; True conditions folded.
        assert!(view.contains("Ready  False"), "{view}");
        assert!(view.contains("ContainersReady  False"), "{view}");
        assert!(view.contains("(+3 healthy [stdout:"), "{view}");
        // Warning event verbatim; Normal events folded.
        assert!(view.contains("Warning  BackOff"), "{view}");
        assert!(view.contains("Normal events: 4 [stdout:"), "{view}");
        assert!(!view.contains("APP_SETTING_20"), "{view}");

        // Error output from kubectl is declined so it stays verbatim.
        let err = "Error from server (NotFound): pods \"missing\" not found\n";
        assert!(format(&["kubectl", "describe", "pod", "missing"], err, 4096).is_none());
    }

    #[test]
    fn gh_pr_checks_and_run_view_and_lists_compact_with_zero_failure_loss() {
        // 1. `gh pr checks`
        let mut checks = String::new();
        for i in 1..=32 {
            let _ = writeln!(
                checks,
                "ci / check-{i:02}\tpass\t{}s\thttps://github.com/acme/repo/actions/runs/99/job/{i}",
                10 + i
            );
        }
        checks.push_str(
            "ci / unit-tests\tfail\t2m14s\thttps://github.com/acme/repo/actions/runs/99/job/33\n",
        );
        checks.push_str(
            "ci / build-macos\tpending\t0\thttps://github.com/acme/repo/actions/runs/99/job/34\n",
        );
        checks.push_str("ci / deploy-preview\tskipping\t0\thttps://github.com/acme/repo/actions/runs/99/job/35\n");

        let checks_view = format(&["gh", "pr", "checks", "42"], &checks, 2048).unwrap();
        let checks_pct = (checks.len() - checks_view.len()) * 100 / checks.len();
        assert!(
            checks_pct >= 80,
            "expected >=80% reduction, got {checks_pct}% ({} -> {})\n{checks_view}",
            checks.len(),
            checks_view.len()
        );
        assert!(
            checks_view.contains("32 passed [stdout:1-32]"),
            "{checks_view}"
        );
        assert!(checks_view.contains("ci / unit-tests  fail  2m14s  https://github.com/acme/repo/actions/runs/99/job/33 [stdout:33]"), "{checks_view}");
        assert!(
            checks_view.contains("ci / build-macos  pending"),
            "{checks_view}"
        );
        assert!(
            checks_view.contains("ci / deploy-preview  skipping"),
            "{checks_view}"
        );
        assert!(
            format(
                &["gh", "pr", "checks", "42", "--json", "name"],
                &checks,
                2048
            )
            .is_none()
        );

        // 2. `gh run view`
        let mut run_view_raw = String::from(
            "X main CI · 1234567890\nTriggered via push about 5 minutes ago\n\nJOBS\n",
        );
        for i in 1..=14 {
            let _ = writeln!(run_view_raw, "✓ build-target-{i:02} in 1m10s (ID {i}00)");
            let _ = writeln!(run_view_raw, "  ✓ Set up job");
            let _ = writeln!(run_view_raw, "  ✓ Run actions/checkout@v4");
            let _ = writeln!(run_view_raw, "  ✓ Build");
            let _ = writeln!(run_view_raw, "  ✓ Complete job");
        }
        run_view_raw.push_str(
            "\
X test (ubuntu-latest) in 2m45s (ID 9999)
  ✓ Set up job
  ✓ Run actions/checkout@v4
  ✓ Install dependencies
  X Run integration tests
  - Upload coverage
  ✓ Post Run actions/checkout@v4
  ✓ Complete job

ANNOTATIONS
X Process completed with exit code 1.
test (ubuntu-latest): .github#L1

View this run on GitHub: https://github.com/acme/repo/actions/runs/1234567890
",
        );
        let rv = format(&["gh", "run", "view", "1234567890"], &run_view_raw, 2048).unwrap();
        let rv_pct = (run_view_raw.len() - rv.len()) * 100 / run_view_raw.len();
        assert!(
            rv_pct >= 70,
            "expected >=70% reduction, got {rv_pct}% ({} -> {})\n{rv}",
            run_view_raw.len(),
            rv.len()
        );
        assert!(rv.contains("✓ 14 passed jobs [stdout:5-74]"), "{rv}");
        assert!(
            rv.contains("X test (ubuntu-latest) in 2m45s (ID 9999) [stdout:75-82]"),
            "{rv}"
        );
        assert!(rv.contains("  ✓ 3 passed steps [stdout:76-78]"), "{rv}");
        assert!(rv.contains("  X Run integration tests [stdout:79]"), "{rv}");
        assert!(rv.contains("  - Upload coverage [stdout:80]"), "{rv}");
        assert!(rv.contains("X Process completed with exit code 1."), "{rv}");
        assert!(
            format(
                &["gh", "run", "view", "1234567890", "--log-failed"],
                &run_view_raw,
                2048
            )
            .is_none()
        );

        // 3. `gh pr list` / `glab mr list`
        let mut prs = String::from(
            "Showing 50 of 120 open pull requests in acme/widget\n\nID      TITLE                                                      BRANCH                          CREATED AT\n",
        );
        for i in 1..=50 {
            let _ = writeln!(
                prs,
                "#{:<6} {:<58} {:<31} about {} hours ago",
                500 + i,
                format!("feat(api): add paginated cursor support step {i}"),
                format!("feat/cursor-page-{i:02}"),
                i
            );
        }
        let pr_view = format(&["gh", "pr", "list"], &prs, 1200).unwrap();
        assert!(pr_view.len() <= 1200, "len={}: {pr_view}", pr_view.len());
        assert!(
            pr_view.contains("#501  feat(api): add paginated cursor support step 1"),
            "{pr_view}"
        );
        assert!(pr_view.contains("more items [stdout:"), "{pr_view}");
    }

    #[test]
    fn security_audits_group_and_compact_npm_pnpm_yarn_and_cargo_with_zero_loss() {
        // 1. pnpm/yarn box-art audit with repeated dependency paths for the same advisory.
        let mut pnpm_raw = String::new();
        for i in 0..10 {
            let _ = write!(
                pnpm_raw,
                "\
┌───────────────┬──────────────────────────────────────────────────────────────┐
│ high          │ Prototype Pollution in lodash                                │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ Package       │ lodash                                                       │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ Patched in    │ >=4.17.21                                                    │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ Dependency of │ service-{i:02}                                                   │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ Path          │ service-{i:02} > express > body-parser > lodash                  │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ More info     │ https://github.com/advisories/GHSA-jf85-cpcp-j695            │
└───────────────┴──────────────────────────────────────────────────────────────┘
"
            );
        }
        pnpm_raw.push_str(
            "\
┌───────────────┬──────────────────────────────────────────────────────────────┐
│ critical      │ Remote Code Execution in minimist                            │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ Package       │ minimist                                                     │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ Patched in    │ >=1.2.6                                                      │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ Dependency of │ cli-core                                                     │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ Path          │ cli-core > optimist > minimist                               │
├───────────────┼──────────────────────────────────────────────────────────────┤
│ More info     │ https://github.com/advisories/GHSA-xvch-5gv4-984h            │
└───────────────┴──────────────────────────────────────────────────────────────┘
11 vulnerabilities found
",
        );
        let pnpm_view = format(&["pnpm", "audit"], &pnpm_raw, 2048).unwrap();
        let ratio = pnpm_raw.len() / pnpm_view.len().max(1);
        assert!(
            ratio >= 20,
            "expected >=20x cut on pnpm box audit, got {ratio}x ({} -> {})\n{pnpm_view}",
            pnpm_raw.len(),
            pnpm_view.len()
        );
        let crit_pos = pnpm_view
            .find("[critical] minimist (GHSA-xvch-5gv4-984h)")
            .unwrap();
        let high_pos = pnpm_view
            .find("[high] lodash (GHSA-jf85-cpcp-j695)")
            .unwrap();
        assert!(
            crit_pos < high_pos,
            "critical must sort before high: {pnpm_view}"
        );
        assert!(pnpm_view.contains("(×10 paths)"), "{pnpm_view}");
        assert!(pnpm_view.contains("fix: >=4.17.21"), "{pnpm_view}");
        assert!(pnpm_view.contains("fix: >=1.2.6"), "{pnpm_view}");
        assert!(format(&["pnpm", "audit", "--fix"], &pnpm_raw, 2048).is_none());

        // 2. npm v7+ block format
        let npm_raw = "\
# npm audit report

minimist  <1.2.6
Severity: critical
Prototype Pollution in minimist - https://github.com/advisories/GHSA-xvch-5gv4-984h
fix available via `npm audit fix --force`
node_modules/minimist
  mkdirp  0.4.1 - 0.5.1
  Depends on vulnerable versions of minimist
  node_modules/mkdirp
    webpack-bundle  1.0.0 - 2.3.0
    Depends on vulnerable versions of mkdirp
    node_modules/webpack-bundle

qs  6.7.0 - 6.10.2
Severity: moderate
qs vulnerable to Prototype Pollution - https://github.com/advisories/GHSA-hrpp-h998-j3pp
fix available via `npm audit fix`
node_modules/qs
  express  4.17.0 - 4.18.1
  Depends on vulnerable versions of qs
  node_modules/express

2 vulnerabilities (1 moderate, 1 critical)
";
        let npm_view = format(&["npm", "audit"], npm_raw, 2048).unwrap();
        assert!(npm_view.len() < npm_raw.len());
        assert!(
            npm_view.contains("[critical] minimist <1.2.6 (GHSA-xvch-5gv4-984h)"),
            "{npm_view}"
        );
        assert!(
            npm_view.contains("[moderate] qs 6.7.0 - 6.10.2 (GHSA-hrpp-h998-j3pp)"),
            "{npm_view}"
        );
        assert!(format(&["npm", "audit", "fix"], npm_raw, 2048).is_none());

        // 3. cargo audit
        let mut cargo_raw = String::from(
            "\
    Fetching advisory database from `https://github.com/RustSec/advisory-db.git`
      Loaded 712 security advisories (from /home/dev/.cargo/advisory-db)
    Updating crates.io index
    Scanning Cargo.lock for vulnerabilities (312 crate dependencies)
Crate:     openssl
Version:   0.10.45
Title:     Use-after-free in SSL_select_next_proto
Date:      2024-04-09
ID:        RUSTSEC-2024-0357
URL:       https://rustsec.org/advisories/RUSTSEC-2024-0357
Severity:  7.5 (high)
Solution:  Upgrade to >=0.10.66
Dependency tree:
openssl 0.10.45
",
        );
        for i in 0..25 {
            let _ = writeln!(cargo_raw, "├── crate-dep-{i:02} 1.0.{i}");
            let _ = writeln!(cargo_raw, "│   └── app-service-{i:02} 0.1.0");
        }
        cargo_raw.push_str(
            "\nCrate:     atty
Version:   0.2.14
Warning:   unmaintained
Title:     Potential unmaintained crate
Date:      2021-11-18
ID:        RUSTSEC-2021-0145
URL:       https://rustsec.org/advisories/RUSTSEC-2021-0145
Dependency tree:
atty 0.2.14
└── clap 3.2.25
    └── app 0.1.0

error: 1 vulnerability found!
warning: 1 allowed warning found
",
        );
        let cargo_view = format(&["cargo", "audit"], &cargo_raw, 2048).unwrap();
        let cargo_pct = (cargo_raw.len() - cargo_view.len()) * 100 / cargo_raw.len();
        assert!(
            cargo_pct >= 80,
            "expected >=80% reduction, got {cargo_pct}% ({} -> {})\n{cargo_view}",
            cargo_raw.len(),
            cargo_view.len()
        );
        assert!(
            cargo_view.contains("[high] openssl 0.10.45 (RUSTSEC-2024-0357)"),
            "{cargo_view}"
        );
        assert!(
            cargo_view.contains("fix: Upgrade to >=0.10.66"),
            "{cargo_view}"
        );
        assert!(
            cargo_view.contains("[warning:unmaintained] atty 0.2.14 (RUSTSEC-2021-0145)"),
            "{cargo_view}"
        );
        assert!(
            cargo_view.contains("error: 1 vulnerability found!"),
            "{cargo_view}"
        );
        assert!(format(&["cargo", "audit", "fix"], &cargo_raw, 2048).is_none());
    }

    #[test]
    fn terraform_state_list_outdated_and_pip_lists_compact_accurately() {
        // 1. terraform state list
        let mut tf = String::new();
        for i in 0..12 {
            let _ = writeln!(tf, "aws_instance.web[{i}]");
        }
        for i in 0..16 {
            let _ = writeln!(tf, "module.network.aws_subnet.private[{i}]");
        }
        for i in 0..4 {
            let _ = writeln!(tf, "module.network.aws_route_table.rt[{i}]");
        }
        tf.push_str("data.aws_ami.ubuntu\n");
        tf.push_str("aws_s3_bucket.assets\n");

        let tf_view = format(&["terraform", "state", "list"], &tf, 1024).unwrap();
        let tf_pct = (tf.len() - tf_view.len()) * 100 / tf.len();
        assert!(
            tf_pct >= 75,
            "expected >=75% reduction, got {tf_pct}% ({} -> {})\n{tf_view}",
            tf.len(),
            tf_view.len()
        );
        assert!(
            tf_view.contains("module.network.aws_subnet ×16 [stdout:13-28]"),
            "{tf_view}"
        );
        assert!(
            tf_view.contains("aws_instance ×12 [stdout:1-12]"),
            "{tf_view}"
        );
        assert!(
            tf_view.contains("module.network.aws_route_table ×4 [stdout:29-32]"),
            "{tf_view}"
        );
        assert!(
            tf_view.contains("aws_s3_bucket.assets [stdout:34]"),
            "{tf_view}"
        );

        // 2. npm / cargo outdated
        let mut outdated = String::from(
            "Package                    Current   Wanted    Latest    Location                          Depended by\n",
        );
        outdated.push_str("react                      17.0.2    17.0.2    18.3.1    node_modules/react                app\n");
        outdated.push_str("typescript                 4.9.5     4.9.5     5.6.3     node_modules/typescript           app\n");
        for i in 0..20 {
            let _ = writeln!(
                outdated,
                "pkg-minor-{i:02}               1.{i}.0     1.25.0    1.25.0    node_modules/pkg-minor-{i:02}         app"
            );
        }
        for i in 0..10 {
            let _ = writeln!(
                outdated,
                "pkg-patch-{i:02}               2.1.{i}     2.1.20    2.1.20    node_modules/pkg-patch-{i:02}         app"
            );
        }
        let out_view = format(&["npm", "outdated"], &outdated, 1200).unwrap();
        assert!(out_view.len() <= 1200, "{out_view}");
        assert!(
            out_view.contains("npm outdated: 32 packages (2 major, 20 minor, 10 patch)"),
            "{out_view}"
        );
        assert!(
            out_view.contains("react 17.0.2 -> 18.3.1 [stdout:2]"),
            "{out_view}"
        );
        assert!(
            out_view.contains("typescript 4.9.5 -> 5.6.3 [stdout:3]"),
            "{out_view}"
        );

        // 3. pip list & pip freeze
        let mut pip_list = String::from(
            "Package                      Version\n---------------------------- -----------\n",
        );
        pip_list.push_str("local-app                    0.1.0       /workspace/app\n");
        for i in 0..80 {
            let _ = writeln!(
                pip_list,
                "python-package-{i:03}           1.{}.{}",
                i % 10,
                i % 5
            );
        }
        let pl_view = format(&["pip", "list"], &pip_list, 1200).unwrap();
        assert!(pl_view.len() <= 1200, "len={}: {pl_view}", pl_view.len());
        assert!(
            pl_view.contains("local-app==0.1.0 (/workspace/app) [stdout:3]"),
            "{pl_view}"
        );
        assert!(pl_view.contains("more packages [stdout:"), "{pl_view}");

        let mut pip_freeze = String::from("-e git+https://github.com/acme/lib.git@abc#egg=lib\n");
        for i in 0..100 {
            let _ = writeln!(pip_freeze, "dep-package-{i:03}==2.{}.{i}", i % 7);
        }
        let pf_view = format(&["pip3", "freeze"], &pip_freeze, 900).unwrap();
        assert!(pf_view.len() <= 900, "len={}: {pf_view}", pf_view.len());
        assert!(
            pf_view.contains("-e git+https://github.com/acme/lib.git@abc#egg=lib [stdout:1]"),
            "{pf_view}"
        );
        assert!(pf_view.contains("more packages [stdout:"), "{pf_view}");

        // 4. cargo outdated + glab mr list + oc describe + yarn audit (ASCII box)
        let cargo_outdated = "\
Name                 Project  Compat   Latest   Kind    Platform
----                 -------  ------   ------   ----    --------
clap                 3.2.25   3.2.25   4.5.20   Normal  ---
serde                1.0.180  1.0.210  1.0.210  Normal  ---
tokio                1.32.0   1.41.0   1.41.0   Normal  ---
anyhow               1.0.70   1.0.93   1.0.93   Normal  ---
thiserror            1.0.50   1.0.65   2.0.3    Normal  ---
regex                1.10.2   1.11.1   1.11.1   Normal  ---
";
        let co_view = format(&["cargo", "outdated"], cargo_outdated, 1024).unwrap();
        assert!(
            co_view.contains("cargo outdated: 6 packages (2 major, 2 minor, 2 patch)"),
            "{co_view}"
        );
        assert!(
            co_view.contains("clap 3.2.25 -> 4.5.20 [stdout:3]"),
            "{co_view}"
        );
        assert!(
            co_view.contains("thiserror 1.0.50 -> 2.0.3 [stdout:7]"),
            "{co_view}"
        );
        assert!(
            format(
                &["cargo", "outdated", "--format", "json"],
                cargo_outdated,
                1024
            )
            .is_none()
        );

        let mut mrs = String::from("Showing 30 open merge requests in acme/widget\n\n");
        for i in 1..=30 {
            let _ = writeln!(
                mrs,
                "!{:<5}  {:<55}  (main) <- (feat/mr-{i:02})",
                100 + i,
                format!("fix(billing): handle retry edge case {i}")
            );
        }
        let mr_view = format(&["glab", "mr", "list"], &mrs, 900).unwrap();
        assert!(mr_view.len() <= 900, "{mr_view}");
        assert!(
            mr_view.contains("!101  fix(billing): handle retry edge case 1"),
            "{mr_view}"
        );
        assert!(mr_view.contains("more items [stdout:"), "{mr_view}");

        let mut yarn_raw = String::new();
        for i in 0..6 {
            let _ = write!(
                yarn_raw,
                "\
+---------------+--------------------------------------------------------------+
| high          | Regular Expression Denial of Service in semver               |
+---------------+--------------------------------------------------------------+
| Package       | semver                                                       |
+---------------+--------------------------------------------------------------+
| Patched in    | >=7.5.2                                                      |
+---------------+--------------------------------------------------------------+
| Dependency of | tool-{i:02}                                                      |
+---------------+--------------------------------------------------------------+
| More info     | https://github.com/advisories/GHSA-c2qf-rxjj-qqgw            |
+---------------+--------------------------------------------------------------+
"
            );
        }
        yarn_raw.push_str("6 vulnerabilities found\n");
        let yarn_view = format(&["yarn", "audit"], &yarn_raw, 1024).unwrap();
        assert!(
            yarn_view.contains("[high] semver (GHSA-c2qf-rxjj-qqgw)"),
            "{yarn_view}"
        );
        assert!(yarn_view.contains("(×6 paths)"), "{yarn_view}");
        assert!(yarn_view.len() * 15 < yarn_raw.len(), "{yarn_view}");
    }

    #[test]
    fn gh_pr_view_folds_boilerplate_and_bot_comments_preserving_metadata_and_human_reviews() {
        let raw = "\
title:\tfeat(billing): support multi-currency invoices
state:\tOPEN
author:\talice
labels:\tenhancement, billing
assignees:\talice, bob
reviewers:\tcarol (APPROVED), dave (CHANGES_REQUESTED)
projects:\t
milestone:\tv2.4
number:\t482
url:\thttps://github.com/acme/widget/pull/482
additions:\t312
deletions:\t48
--
<!--
PR Template Instructions:
Please fill out all sections below and delete unused options before merging.
Remember to link related issues and attach screenshots when UI changes.
-->
[![CI](https://github.com/acme/widget/actions/workflows/ci.yml/badge.svg)](https://github.com/acme/widget/actions) [![Coverage](https://img.shields.io/codecov/c/github/acme/widget)](https://codecov.io/gh/acme/widget)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![Docs](https://img.shields.io/badge/docs-passing-brightgreen.svg)](https://docs.example.com)

## Summary
Adds zero-decimal currency handling for JPY and KRW invoices.

```html
<!-- Keep code-fenced HTML comment verbatim -->
<details><summary>Keep fenced details</summary></details>
```

<details>
<summary>Benchmark & Migration Logs</summary>

Running 24 migration checks...
Step 1: verified USD rounding
Step 2: verified JPY rounding
Step 3: verified EUR rounding
Step 4: verified KRW rounding
Step 5: verified GBP rounding
Step 6: verified CHF rounding
</details>

## Type of change
- [x] Bug fix / feature with unit tests
- [ ] Breaking change (fix or feature that would cause existing functionality to change)
- [ ] Documentation update only
- [ ] Performance refactor without behavior change
- [ ] CI / workflow configuration update
- [ ] Dependency version bump
- [ ] Security backport
--
author:\tcodecov[bot]
association:\tnone
edited:\tfalse
status:\tnone
--
<!-- codecov-comment -->
[![codecov](https://codecov.io/gh/acme/widget/branch/main/graph/badge.svg)](https://codecov.io/gh/acme/widget)
## [Codecov](https://app.codecov.io/gh/acme/widget/pull/482) Report
Patch coverage is 94.2% with 3 missing lines.
| File | Coverage |
| --- | --- |
| src/billing.rs | 95.0% |
| src/currency.rs | 93.4% |
<details><summary>Additional details</summary>
verbose table row 1
verbose table row 2
verbose table row 3
</details>
--
author:\tvercel[bot]
association:\tnone
edited:\tfalse
status:\tnone
--
The latest updates on your projects. Learn more about Vercel for Git.
| Name | Status | Preview | Updated |
| --- | --- | --- | --- |
| widget-web | Ready | https://widget-preview.vercel.app | Nov 25, 2026 |
| widget-docs | Ready | https://widget-docs.vercel.app | Nov 25, 2026 |
--
author:\tcarol
association:\tmember
edited:\tfalse
status:\tAPPROVED
--
Looks clean! One small note: please double-check negative credit memos in JPY.
";

        assert_eq!(kind_tag(&["gh", "pr", "view", "482"]), Some("forge_view"));
        let view = format(&["gh", "pr", "view", "482", "--comments"], raw, 4096).unwrap();
        assert!(
            view.len() < raw.len(),
            "expected compaction: {} vs {}",
            view.len(),
            raw.len()
        );
        assert!(view.len() <= 4096, "{view}");

        // 100% metadata preservation.
        for must_keep in [
            "title: feat(billing): support multi-currency invoices",
            "state: OPEN",
            "author: alice",
            "labels: enhancement, billing",
            "reviewers: carol (APPROVED), dave (CHANGES_REQUESTED)",
            "url: https://github.com/acme/widget/pull/482",
            "Adds zero-decimal currency handling for JPY and KRW invoices.",
            "<!-- Keep code-fenced HTML comment verbatim -->",
            "- [x] Bug fix / feature with unit tests",
            "carol (APPROVED)",
            "Looks clean! One small note: please double-check negative credit memos in JPY.",
        ] {
            assert!(
                view.contains(must_keep),
                "missing `{must_keep}` in:\n{view}"
            );
        }

        // Boilerplate folded with exact `[stdout:A-B]` pointers.
        assert!(
            view.contains("[folded HTML comment (5 lines) [stdout:14-18]]"),
            "{view}"
        );
        assert!(
            view.contains("[folded 2 badge/image lines [stdout:19-20]]"),
            "{view}"
        );
        assert!(
            view.contains(
                "[folded <details>: \"Benchmark & Migration Logs\" (11 lines) [stdout:30-40]]"
            ),
            "{view}"
        );
        assert!(
            view.contains("[folded 6 unchecked checklist items [stdout:44-49]]"),
            "{view}"
        );
        assert!(
            view.contains("- bot: codecov[bot] — Patch coverage is 94.2% with 3 missing lines. (18 lines) [stdout:51-68]"),
            "{view}"
        );
        assert!(
            view.contains("- bot: vercel[bot] — widget-web | Ready | https://widget-preview.vercel.app | Nov 25, 2026 (10 lines) [stdout:70-79]"),
            "{view}"
        );

        // Machine / web / help flags are declined.
        for declined in [
            &["gh", "pr", "view", "482", "--json", "title,body"][..],
            &["gh", "pr", "view", "482", "--jq", ".title"][..],
            &["gh", "pr", "view", "482", "--template", "{{.title}}"][..],
            &["gh", "pr", "view", "482", "--web"][..],
            &["gh", "pr", "view", "482", "-w"][..],
            &["gh", "pr", "view", "--help"][..],
        ] {
            assert_eq!(kind_tag(declined), None, "{declined:?}");
            assert!(format(declined, raw, 4096).is_none(), "{declined:?}");
        }
    }

    #[test]
    fn gh_issue_view_and_glab_mr_view_tty_compact_and_honor_budget() {
        // 1. `glab mr view` TTY format with bots, details, badges, and human reviewer.
        let mut glab_mr = String::from(
            "\
open • opened by bob 2 hours ago
fix(auth): rotate expired refresh tokens !128
Source branch: fix/token-rotation -> main
Labels: security, auth
Reviewers: dave (Approved)

<!-- gitlab MR template instructions
Please verify all rollout steps before merging to production.
-->
<details>
<summary>Automated SonarCloud Analysis Table</summary>
",
        );
        for i in 1..=25 {
            let _ = writeln!(
                glab_mr,
                "| src/auth/mod_{i:02}.rs | 0 bugs | 0 hotspots | A |"
            );
        }
        glab_mr.push_str(
            "\
</details>

Fixes token reuse race condition when two refresh calls overlap.

github-actions[bot] • 1 hour ago •
  [![Build](https://img.shields.io/badge/build-passing-brightgreen.svg)](https://ci.example.com)
  Automated CI matrix completed: 48 jobs passed across linux/macOS.
  More details in workflow summary table...
  Row 1 of 48
  Row 2 of 48
  Row 3 of 48

dave (Maintainer) • 20 minutes ago •
  LGTM — tested locally with concurrent token refresh requests.

View this merge request on GitLab: https://gitlab.com/acme/widget/-/merge_requests/128
",
        );

        assert_eq!(kind_tag(&["glab", "mr", "view", "128"]), Some("forge_view"));
        assert_eq!(
            kind_tag(&["glab", "issue", "view", "7"]),
            Some("forge_view")
        );
        assert_eq!(kind_tag(&["gh", "issue", "view", "7"]), Some("forge_view"));
        assert_eq!(kind_tag(&["glab", "mr", "view", "128", "-F", "json"]), None);
        assert_eq!(kind_tag(&["glab", "mr", "view", "128", "-w"]), None);

        let view = format(&["glab", "mr", "view", "128", "--comments"], &glab_mr, 2048).unwrap();
        assert!(view.len() <= 2048, "{view}");
        assert!(
            view.len() * 2 < glab_mr.len(),
            "expected >=50% compaction: {} vs {}\n{view}",
            view.len(),
            glab_mr.len()
        );
        for must_keep in [
            "fix(auth): rotate expired refresh tokens !128",
            "Source branch: fix/token-rotation -> main",
            "Labels: security, auth",
            "Reviewers: dave (Approved)",
            "Fixes token reuse race condition",
            "dave (Maintainer) • 20 minutes ago",
            "LGTM — tested locally with concurrent token refresh requests.",
            "https://gitlab.com/acme/widget/-/merge_requests/128",
        ] {
            assert!(
                view.contains(must_keep),
                "missing `{must_keep}` in:\n{view}"
            );
        }
        assert!(view.contains("[folded HTML comment"), "{view}");
        assert!(
            view.contains("[folded <details>: \"Automated SonarCloud Analysis Table\""),
            "{view}"
        );
        assert!(view.contains("- bot: github-actions[bot]"), "{view}");

        // 2. Tight `max_bytes` strictly enforced while preserving header, human comment & URL.
        let tight = format(&["glab", "mr", "view", "128", "--comments"], &glab_mr, 650).unwrap();
        assert!(tight.len() <= 650, "len={}: {tight}", tight.len());
        assert!(
            tight.contains("fix(auth): rotate expired refresh tokens !128"),
            "{tight}"
        );
        assert!(tight.contains("LGTM — tested locally"), "{tight}");
        assert!(
            tight.contains("https://gitlab.com/acme/widget/-/merge_requests/128"),
            "{tight}"
        );

        // 3. `gh issue view` with unchecked issue template checklist & dependabot comment.
        let issue = "\
title:\tMemory spike during large workspace indexing
state:\tOPEN
author:\teve
labels:\tbug, performance
number:\t917
url:\thttps://github.com/acme/widget/issues/917
--
<!-- Please provide reproduction steps and environment info -->
Indexing a 50k-file monorepo spikes RSS to 2.4 GB.

- [ ] Tested on Windows
- [ ] Tested on macOS
- [ ] Tested on Alpine musl
- [ ] Reproduced with --no-cache
- [ ] Attached flamegraph
--
author:\tsonarcloud[bot]
association:\tnone
edited:\tfalse
status:\tnone
--
[![Quality Gate Passed](https://sonarcloud.io/images/project_badges/sonarcloud-white.svg)](https://sonarcloud.io)
Quality Gate Passed: 0 new bugs, 0 vulnerabilities.
Extra automated report details line 1
Extra automated report details line 2
Extra automated report details line 3
";
        let issue_view =
            format(&["gh", "issue", "view", "917", "--comments"], issue, 2048).unwrap();
        assert!(issue_view.len() < issue.len(), "{issue_view}");
        assert!(
            issue_view.contains("title: Memory spike during large workspace indexing"),
            "{issue_view}"
        );
        assert!(
            issue_view.contains("[folded 5 unchecked checklist items"),
            "{issue_view}"
        );
        assert!(
            issue_view.contains("- bot: sonarcloud[bot] — Quality Gate Passed"),
            "{issue_view}"
        );

        // 4. Unrecognized JSON or plain error text is cleanly declined.
        assert!(
            format(
                &["gh", "pr", "view", "1"],
                r#"{"title":"x","state":"OPEN"}"#,
                1024
            )
            .is_none()
        );
        assert!(
            format(
                &["gh", "pr", "view", "1"],
                "no pull requests found for branch\n",
                1024
            )
            .is_none()
        );
    }

    #[test]
    fn pins_failing_pending_and_critical_rows_at_top_with_ansi_parity() {
        // 1. `gh run view`: scattered passing jobs around failing + pending jobs under ANSI escapes.
        let mut run_raw =
            String::from("\x1b[31mX\x1b[0m main CI · 4242\nTriggered via push 2m ago\n\nJOBS\n");
        for i in 1..=6 {
            let _ = writeln!(run_raw, "\x1b[32m✓\x1b[0m lint-{i:02} in 12s (ID {i}01)");
            let _ = writeln!(run_raw, "  ✓ Checkout");
            let _ = writeln!(run_raw, "  ✓ Run linter");
        }
        run_raw.push_str(
            "\x1b[33m*\x1b[0m deploy-staging in 0s (ID 800)\n  ✓ Checkout\n  * Wait for approval\n",
        );
        for i in 7..=12 {
            let _ = writeln!(run_raw, "\x1b[32m✓\x1b[0m unit-{i:02} in 45s (ID {i}01)");
            let _ = writeln!(run_raw, "  ✓ Checkout");
            let _ = writeln!(run_raw, "  ✓ Test");
        }
        run_raw.push_str(
            "\x1b[31mX\x1b[0m e2e-suite in 3m10s (ID 900)\n  ✓ Checkout\n  X Run e2e\n  - Upload trace\n\nANNOTATIONS\nX Process completed with exit code 1.\n",
        );

        let rv = format(&["gh", "run", "view", "4242"], &run_raw, 1024).expect("gh run view");
        assert!(!rv.contains('\x1b'), "ANSI leaked:\n{rv}");
        let fail_pos = rv.find("X e2e-suite in 3m10s (ID 900)").unwrap();
        let pend_pos = rv.find("* deploy-staging in 0s (ID 800)").unwrap();
        let pass_pos = rv.find("✓ 12 passed jobs [stdout:5-43]").unwrap();
        assert!(
            fail_pos < pend_pos && pend_pos < pass_pos,
            "expected fail < pending < passed:\n{rv}"
        );
        assert!(rv.contains("X Process completed with exit code 1."), "{rv}");

        // 2. `gh pr checks`: ANSI parity + Fail -> Pending -> Skipped -> passed count.
        let mut checks_raw = String::new();
        for i in 1..=20 {
            let _ = writeln!(
                checks_raw,
                "ci / ok-{i:02}\t\x1b[32mpass\x1b[0m\t{i}s\thttps://example.test/job/{i}"
            );
        }
        checks_raw.push_str("ci / opt\tskipped\t0s\thttps://example.test/job/21\n");
        checks_raw
            .push_str("ci / deploy\t\x1b[33mqueued\x1b[0m\t0s\thttps://example.test/job/22\n");
        checks_raw.push_str(
            "ci / sec\t\x1b[31maction_required\x1b[0m\t5s\thttps://example.test/job/23\n",
        );
        let checks_view = format(&["gh", "pr", "checks", "7"], &checks_raw, 1024).unwrap();
        assert!(!checks_view.contains('\x1b'), "ANSI leaked:\n{checks_view}");
        let f_pos = checks_view.find("ci / sec  action_required").unwrap();
        let p_pos = checks_view.find("ci / deploy  queued").unwrap();
        let s_pos = checks_view.find("ci / opt  skipped").unwrap();
        let ok_pos = checks_view.find("20 passed [stdout:1-20]").unwrap();
        assert!(
            f_pos < p_pos && p_pos < s_pos && s_pos < ok_pos,
            "{checks_view}"
        );

        // 3. `kubectl describe` multi-pod: healthy pod before CrashLoopBackOff pod, and healthy sidecar before OOMKilled container.
        let mut desc = String::from(
            "\
Name:         healthy-web-0
Namespace:    prod
Status:       Running
Labels:       app=web
              tier=frontend
              version=v1
Annotations:  a: 1
              b: 2
              c: 3
Containers:
  web:
    Image:          nginx:1.27
    State:          Running
    Ready:          True
    Restart Count:  0
    Environment:
      A: 1
      B: 2
      C: 3
      D: 4
Conditions:
  Type              Status
  Initialized       True
  Ready             True
  ContainersReady   True
  PodScheduled      True
Events:             <none>

Name:         broken-api-0
Namespace:    prod
Status:       Running
Containers:
  envoy:
    Image:          envoyproxy/envoy:v1.31
    State:          Running
    Ready:          True
    Restart Count:  0
    Environment:
      LOG_LEVEL: info
      ADMIN_PORT: 9901
      CLUSTER: prod
      REGION: us-east-1
  api:
    Image:          ghcr.io/acme/api:2.0
    State:          Waiting
      Reason:       CrashLoopBackOff
    Last State:     Terminated
      Reason:       OOMKilled
      Exit Code:    137
    Ready:          False
    Restart Count:  6
    Environment:
",
        );
        for i in 0..15 {
            let _ = writeln!(desc, "      VAR_{i:02}: val-{i:02}");
        }
        desc.push_str(
            "Conditions:\n  Type   Status\n  Ready  False\nEvents:\n  Type     Reason   Age  From     Message\n  ----     ------   ---  ----     -------\n  Normal   Pulled   5m   kubelet  Pulled image\n  Warning  BackOff  1m   kubelet  Back-off restarting failed container api\n",
        );
        let dview = format(&["kubectl", "describe", "pods", "-n", "prod"], &desc, 4096).unwrap();
        let broken_pos = dview.find("Name: broken-api-0").unwrap();
        let healthy_pos = dview.find("Name: healthy-web-0").unwrap();
        assert!(
            broken_pos < healthy_pos,
            "unhealthy pod must be pinned ahead of healthy pod:\n{dview}"
        );
        let api_pos = dview.find("  api: [stdout:").unwrap();
        let envoy_pos = dview.find("  envoy: Running, Ready=True").unwrap();
        assert!(
            api_pos < envoy_pos,
            "unhealthy container must be pinned ahead of healthy sidecar:\n{dview}"
        );
        assert!(
            dview.contains("CrashLoopBackOff") && dview.contains("OOMKilled"),
            "{dview}"
        );

        // 4. Security audit: ANSI + numeric CVSS critical/high pinned ahead of repeated warnings under tight budget.
        let mut audit = String::new();
        for i in 0..12 {
            let _ = write!(
                audit,
                "Crate:     legacy-{i:02}\nVersion:   0.1.{i}\nWarning:   unmaintained\nTitle:     Unmaintained helper crate {i:02}\nID:        RUSTSEC-2023-{i:04}\nDependency tree:\nlegacy-{i:02} 0.1.{i}\n└── app 1.0.0\n\n"
            );
        }
        audit.push_str(
            "\x1b[31mCrate:\x1b[0m     auth-core\nVersion:   1.2.3\nTitle:     Authentication bypass\nID:        RUSTSEC-2026-0099\nSeverity:  9.8\nSolution:  Upgrade to >=1.2.4\nDependency tree:\nauth-core 1.2.3\n└── app 1.0.0\n\nerror: 1 vulnerability found!\nwarning: 12 allowed warnings found\n",
        );
        let aview = format(&["cargo", "audit"], &audit, 640).expect("cargo audit compacted");
        assert!(!aview.contains('\x1b'), "ANSI leaked:\n{aview}");
        let crit_pos = aview
            .find("[critical] auth-core 1.2.3 (RUSTSEC-2026-0099)")
            .expect("critical advisory preserved");
        assert!(
            aview.contains("lower-severity advisories [stdout:"),
            "expected lower-severity tail fold under tight budget:\n{aview}"
        );
        assert!(
            crit_pos < aview.find("lower-severity advisories").unwrap(),
            "{aview}"
        );
    }

    #[test]
    fn ansi_and_plain_fast_paths_have_byte_parity_and_tight_run_view_preserves_annotations() {
        let mut plain_checks = String::new();
        let mut ansi_checks = String::new();
        for i in 1..=16 {
            let _ = writeln!(
                plain_checks,
                "✓  build-{i:02}  12s  https://example.test/{i}"
            );
            let _ = writeln!(
                ansi_checks,
                "\x1b[32m✓\x1b[0m  build-{i:02}  12s  https://example.test/{i}"
            );
        }
        plain_checks.push_str("X  test-e2e  2m  https://example.test/17\n");
        ansi_checks.push_str("\x1b[31mX\x1b[0m  test-e2e  2m  https://example.test/17\n");
        plain_checks.push_str("*  deploy  0s  https://example.test/18\n");
        ansi_checks.push_str("\x1b[33m*\x1b[0m  deploy  0s  https://example.test/18\n");

        let plain_out = format(&["gh", "pr", "checks", "99"], &plain_checks, 1024).unwrap();
        let ansi_out = format(&["gh", "pr", "checks", "99"], &ansi_checks, 1024).unwrap();
        assert_eq!(plain_out, ansi_out, "ANSI vs plain parity mismatch");

        // Tight budget on `gh run view` with multiple failing jobs must still pin failing jobs + ANNOTATIONS.
        let mut run_raw = String::from("X main Release · 777\n\nJOBS\n");
        for i in 1..=10 {
            let _ = writeln!(run_raw, "✓ build-{i:02} in 30s (ID {i})");
        }
        for i in 1..=8 {
            let _ = writeln!(run_raw, "X failing-suite-{i:02} in 1m (ID 90{i})");
            let _ = writeln!(run_raw, "  ✓ Setup");
            let _ = writeln!(run_raw, "  X Run suite {i:02}");
        }
        run_raw.push_str("\nANNOTATIONS\nX Fatal assertion in suite 01\n");

        let tight = format(&["gh", "run", "view", "777"], &run_raw, 420).unwrap();
        assert!(tight.len() <= 420, "len={}: {tight}", tight.len());
        assert!(
            tight.contains("X failing-suite-01 in 1m (ID 901)"),
            "{tight}"
        );
        assert!(
            tight.contains("X Fatal assertion in suite 01"),
            "ANNOTATIONS lost under tight budget:\n{tight}"
        );
    }
}
