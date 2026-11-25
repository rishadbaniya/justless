//! Compact, zero-semantic-loss views for high-frequency CLI raw dumps:
//! - `kubectl describe` / `oc describe`
//! - `gh` / `glab` human views (`gh pr checks`, `gh run view`, `pr`/`issue`/`run`/`mr`/`pipeline` lists)
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
    let lines: Vec<Cow<'_, str>> = text
        .lines()
        .map(|raw| {
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if line.contains('\x1b') {
                crate::capture::ANSI.replace_all(line, "")
            } else {
                Cow::Borrowed(line)
            }
        })
        .collect();
    if lines.is_empty() || lines.iter().any(|l| l.len() >= MAX_LINE_BYTES) {
        return None;
    }
    let rendered = match kind {
        ViewKind::KubectlDescribe => format_kubectl_describe(&lines, max_bytes),
        ViewKind::GhPrChecks => format_gh_pr_checks(&lines, max_bytes),
        ViewKind::GhRunView => format_gh_run_view(&lines, max_bytes),
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

fn range_tag(first: usize, last: usize) -> String {
    if first == last {
        format!("[stdout:{first}]")
    } else {
        format!("[stdout:{first}-{last}]")
    }
}

fn indent_of(line: &str) -> usize {
    line.bytes().take_while(|&b| b == b' ').count()
}

fn tighten_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space_run = 0usize;
    for ch in s.trim().chars() {
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
    out
}

fn collapse_single_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
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

    let mut error_tail = String::new();
    for (n, err) in &trailing_errors {
        let _ = writeln!(error_tail, "{err} {}", range_tag(*n, *n));
    }

    // Try rendering all compacted resources first.
    let mut full = String::new();
    for (i, (_, _, _, _, body)) in rendered.iter().enumerate() {
        if i > 0 {
            full.push('\n');
        }
        full.push_str(body);
    }
    full.push_str(&error_tail);
    if full.len() <= max_bytes {
        return Some(full);
    }

    // Multiple resources exceeding budget: keep unhealthy resources first, then healthy,
    // folding overflow behind counts + exact `[stdout:A-B]` pointers.
    if rendered.len() > 1 {
        let mut ordered: Vec<usize> = (0..rendered.len()).collect();
        ordered.sort_by_key(|&i| (!rendered[i].0, i));
        let mut out = String::new();
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
    let (_, first, last, _, _) = &rendered[0];
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

fn render_kube_resource(res: &KubeResource<'_>) -> (bool, String, String) {
    let mut out = String::new();
    let mut unhealthy = false;
    let mut name = String::new();
    let mut namespace = String::new();
    let mut status = String::new();

    for sec in &res.sections {
        match sec.key {
            "Name" => {
                name = sec.inline_value.to_owned();
                let _ = writeln!(out, "Name: {}", sec.inline_value);
            }
            "Namespace" => {
                namespace = sec.inline_value.to_owned();
                let _ = writeln!(out, "Namespace: {}", sec.inline_value);
            }
            "Status" | "Phase" => {
                status = sec.inline_value.to_owned();
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
                if sec.key == "Reason" || sec.key == "Message" {
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
        (true, true) => name,
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
    let mut out = String::new();
    let _ = writeln!(out, "{section_name}:");
    let mut any_unhealthy = false;

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
            let img_part = if image.is_empty() {
                String::new()
            } else {
                format!(" ({image})")
            };
            let _ = writeln!(
                out,
                "  {}: {state_label}, Ready={ready_label}, Restarts={restarts}{img_part} {}",
                block.name,
                range_tag(block.first_line, block.last_line)
            );
            continue;
        }

        // Unhealthy live container OR Pod Template container: preserve all critical state
        // fields verbatim and fold noisy Environment/Mounts/huge Args sections.
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
    let mut unhealthy_rows: Vec<(usize, String)> = Vec::new();

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

    let mut out = String::new();
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
    let mut warnings: Vec<(usize, usize, String)> = Vec::new();
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
            last.2.push(' ');
            last.2.push_str(trimmed);
        } else {
            warnings.push((line_no, line_no, tighten_spaces(trimmed)));
        }
    }

    let mut out = String::new();
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

fn classify_check_line(line: &str) -> Option<(CheckState, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Format A: TTY icon prefix (`✓  name  34s  url`, `X  name  2m  url`, `*  ...`, `-  ...`).
    if let Some((icon, rest)) = trimmed.split_once(char::is_whitespace) {
        let state = match icon {
            "✓" | "✔" => Some(CheckState::Pass),
            "X" | "✗" | "×" | "!" => Some(CheckState::Fail),
            "*" | "○" | "●" => Some(CheckState::Pending),
            "-" => Some(CheckState::Skipped),
            _ => None,
        };
        if let Some(state) = state
            && !rest.trim().is_empty()
        {
            return Some((state, tighten_spaces(trimmed)));
        }
    }
    // Format B: TSV (`name\tstatus\tduration\turl`) or multi-space columns.
    let cols: Vec<&str> = if trimmed.contains('\t') {
        trimmed.split('\t').map(str::trim).collect()
    } else {
        split_multi_space(trimmed)
    };
    if cols.len() >= 2 {
        let state = match cols[1].to_ascii_lowercase().as_str() {
            "pass" | "passed" | "success" => Some(CheckState::Pass),
            "fail" | "failed" | "failure" | "error" | "timed_out" | "startup_failure" => {
                Some(CheckState::Fail)
            }
            "pending" | "queued" | "in_progress" | "expected" | "waiting" => {
                Some(CheckState::Pending)
            }
            "skipping" | "skipped" | "cancel" | "cancelled" | "canceled" | "neutral" => {
                Some(CheckState::Skipped)
            }
            _ => None,
        };
        if let Some(state) = state {
            let compact = cols
                .iter()
                .filter(|c| !c.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join("  ");
            return Some((state, compact));
        }
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
    let mut attention: Vec<(CheckState, usize, String)> = Vec::new();
    let mut parsed = 0usize;

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = cow.trim();
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

    let mut out = String::new();
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
    for (_, line_no, formatted) in &attention {
        let entry = format!("{formatted} {}\n", range_tag(*line_no, *line_no));
        if out.len() + entry.len() + reserve <= max_bytes {
            out.push_str(&entry);
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

fn is_status_icon_line(trimmed: &str) -> Option<bool> {
    let (icon, rest) = trimmed.split_once(char::is_whitespace)?;
    if rest.trim().is_empty() {
        return None;
    }
    match icon {
        "✓" | "✔" => Some(true),
        "X" | "✗" | "×" | "!" | "*" | "-" | "○" | "●" => Some(false),
        _ => None,
    }
}

fn format_gh_run_view(lines: &[Cow<'_, str>], max_bytes: usize) -> Option<String> {
    struct RunJob<'a> {
        line_no: usize,
        last_line: usize,
        text: &'a str,
        passed: bool,
        steps: Vec<(usize, &'a str, bool)>,
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
        let Some(passed) = is_status_icon_line(trimmed) else {
            // Non-icon line inside JOBS (e.g. wrapped annotation or error detail): keep in tail.
            if let Some(job) = jobs.last_mut() {
                job.passed = false;
                job.last_line = line_no;
                job.steps.push((line_no, trimmed, false));
            }
            continue;
        };
        if ind == 0 {
            jobs.push(RunJob {
                line_no,
                last_line: line_no,
                text: trimmed,
                passed,
                steps: Vec::new(),
            });
        } else if let Some(job) = jobs.last_mut() {
            job.last_line = line_no;
            if !passed {
                job.passed = false;
            }
            job.steps.push((line_no, trimmed, passed));
        } else {
            jobs.push(RunJob {
                line_no,
                last_line: line_no,
                text: trimmed,
                passed,
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

    let mut out = String::new();
    for line in &lead {
        let _ = writeln!(out, "{line}");
    }
    if saw_jobs_header {
        let _ = writeln!(out, "JOBS");
    }

    let mut pass_run_count = 0usize;
    let mut pass_run_first = 0usize;
    let mut pass_run_last = 0usize;
    let flush_pass_jobs =
        |out: &mut String, count: &mut usize, first: &mut usize, last: &mut usize| {
            if *count > 0 {
                let unit = if *count == 1 { "job" } else { "jobs" };
                let _ = writeln!(out, "✓ {count} passed {unit} {}", range_tag(*first, *last));
                *count = 0;
                *first = 0;
                *last = 0;
            }
        };

    for job in &jobs {
        if job.passed {
            pass_run_count += 1;
            if pass_run_first == 0 {
                pass_run_first = job.line_no;
            }
            pass_run_last = job.last_line;
            continue;
        }
        flush_pass_jobs(
            &mut out,
            &mut pass_run_count,
            &mut pass_run_first,
            &mut pass_run_last,
        );
        let _ = writeln!(
            out,
            "{} {}",
            job.text,
            range_tag(job.line_no, job.last_line)
        );
        // Inside a non-passing job, collapse passing steps to counts + `[stdout:A-B]`
        // and keep 100% of failing/cancelled/skipped steps verbatim.
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
        for &(sno, stext, spassed) in &job.steps {
            if spassed {
                step_pass += 1;
                if step_first == 0 {
                    step_first = sno;
                }
                step_last = sno;
            } else {
                flush_steps(&mut out, &mut step_pass, &mut step_first, &mut step_last);
                let _ = writeln!(out, "  {stext} {}", range_tag(sno, sno));
            }
        }
        flush_steps(&mut out, &mut step_pass, &mut step_first, &mut step_last);
    }
    flush_pass_jobs(
        &mut out,
        &mut pass_run_count,
        &mut pass_run_first,
        &mut pass_run_last,
    );

    for line in &tail {
        let _ = writeln!(out, "{line}");
    }

    if out.len() <= max_bytes {
        return Some(out);
    }
    // If still over budget, trim cleanly preserving header, failing jobs, and tail annotations.
    let first = jobs.first().map_or(1, |j| j.line_no);
    let last = lines.len();
    let trimmed = fit_lines_with_tail(&out, max_bytes, first, last);
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
    let mut preamble: Vec<String> = Vec::new();
    let mut header: Option<String> = None;
    let mut rows: Vec<(usize, String)> = Vec::new();

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = cow.trim();
        if trimmed.is_empty() {
            continue;
        }
        if rows.is_empty()
            && header.is_none()
            && (trimmed.starts_with("Showing ") || trimmed.starts_with("showing "))
        {
            preamble.push(trimmed.to_owned());
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
    let mut full = String::new();
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
    let mut out = String::new();
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

    if prioritize_failures {
        for (i, (line_no, row)) in rows.iter().enumerate() {
            if !is_forge_failure_row(row) {
                continue;
            }
            let entry = format!("{row} {}\n", range_tag(*line_no, *line_no));
            if out.len() + entry.len() + reserve <= max_bytes {
                out.push_str(&entry);
                kept_indices[i] = true;
                kept_count += 1;
            }
        }
    }
    for (i, (_, row)) in rows.iter().enumerate() {
        if kept_indices[i] {
            continue;
        }
        let entry = format!("{row}\n");
        if out.len() + entry.len() + reserve <= max_bytes {
            out.push_str(&entry);
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
        match raw.trim().to_ascii_lowercase().as_str() {
            "critical" => Some(Self::Critical),
            "high" => Some(Self::High),
            "moderate" | "medium" => Some(Self::Moderate),
            "low" => Some(Self::Low),
            "info" => Some(Self::Info),
            _ => None,
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        let low = raw.trim().to_ascii_lowercase();
        if let Some(exact) = Self::parse_exact(&low) {
            return Some(exact);
        }
        if low.contains("critical") {
            Some(Self::Critical)
        } else if low.contains("high") {
            Some(Self::High)
        } else if low.contains("moderate") || low.contains("medium") {
            Some(Self::Moderate)
        } else if low.contains("low") {
            Some(Self::Low)
        } else if low.contains("warn")
            || low.contains("unmaintained")
            || low.contains("yanked")
            || low.contains("unsound")
        {
            Some(Self::Warning)
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

    let mut out = String::new();
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

    let footer: String = summary_tail.iter().map(|s| format!("{s}\n")).collect();
    let reserve = footer.len() + 72;

    let entries: Vec<(usize, usize, String)> = groups
        .into_iter()
        .map(|(k, g)| {
            let id_part = if k.id.is_empty() {
                String::new()
            } else {
                format!(" ({})", k.id)
            };
            let title_part = if k.title.is_empty() {
                String::new()
            } else {
                format!(": {}", clip(&k.title, 120))
            };
            let fix_part = if k.patched.is_empty() {
                String::new()
            } else {
                format!(" · fix: {}", clip(&k.patched, 64))
            };
            let paths_part = if g.count > 1 {
                format!(" (×{} paths)", g.count)
            } else {
                String::new()
            };
            let line = format!(
                "[{}] {}{id_part}{title_part}{fix_part}{paths_part} {}\n",
                k.severity_tag,
                k.package,
                range_tag(g.first_line, g.last_line)
            );
            (g.first_line, g.last_line, line)
        })
        .collect();

    let mut kept = 0usize;
    for (_, _, line) in &entries {
        if out.len() + line.len() + reserve <= max_bytes {
            out.push_str(line);
            kept += 1;
        } else {
            break;
        }
    }
    if kept < entries.len() {
        let first = entries[kept..].iter().map(|e| e.0).min().unwrap_or(1);
        let last = entries[kept..].iter().map(|e| e.1).max().unwrap_or(first);
        let _ = writeln!(
            out,
            "+{} lower-severity advisories {}",
            entries.len() - kept,
            range_tag(first, last)
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
            let pkg_header = collapse_single_spaces(trimmed);
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

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{label}: {total} resources across {} {}",
        ranked.len(),
        if ranked.len() == 1 { "type" } else { "types" }
    );
    let reserve = 72usize;
    let mut kept = 0usize;
    for (group, info) in &ranked {
        let line = if info.count == 1 {
            format!(
                "  {} {}\n",
                info.single_addr,
                range_tag(info.first, info.last)
            )
        } else {
            format!(
                "  {group} ×{} {}\n",
                info.count,
                range_tag(info.first, info.last)
            )
        };
        if out.len() + line.len() + reserve <= max_bytes {
            out.push_str(&line);
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
    struct OutdatedRow {
        name: String,
        current: String,
        latest: String,
        bump: BumpKind,
        line_no: usize,
    }

    let mut header_cols: Option<(usize, usize, usize)> = None; // (name_idx, current_idx, latest_idx)
    let mut rows: Vec<OutdatedRow> = Vec::new();

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = cow.trim();
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
                .position(|c| matches!(c.to_ascii_lowercase().as_str(), "package" | "name"));
            let cur_i = cells.iter().position(|c| {
                matches!(
                    c.to_ascii_lowercase().as_str(),
                    "current" | "project" | "version"
                )
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
            name: name.to_owned(),
            current: current.to_owned(),
            latest: latest.to_owned(),
            bump,
            line_no,
        });
    }

    if rows.len() < 3 {
        return None;
    }

    let major: Vec<&OutdatedRow> = rows.iter().filter(|r| r.bump == BumpKind::Major).collect();
    let minor: Vec<&OutdatedRow> = rows.iter().filter(|r| r.bump == BumpKind::Minor).collect();
    let patch: Vec<&OutdatedRow> = rows.iter().filter(|r| r.bump == BumpKind::Patch).collect();

    let mut out = String::new();
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
        for row in &major {
            let line = format!(
                "  {} {} -> {} {}\n",
                row.name,
                row.current,
                row.latest,
                range_tag(row.line_no, row.line_no)
            );
            if out.len() + line.len() + reserve <= max_bytes {
                out.push_str(&line);
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
    let append_packed = |out: &mut String, title: &str, group: &[&OutdatedRow]| {
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
        let mut current_line = String::from("  ");
        let mut kept = 0usize;
        for row in group {
            let token = format!("{} {}->{}", row.name, row.current, row.latest);
            let sep = if current_line.len() > 2 { ", " } else { "" };
            if current_line.len() + sep.len() + token.len() > 96 && current_line.len() > 2 {
                current_line.push('\n');
                if out.len() + current_line.len() + reserve <= max_bytes {
                    out.push_str(&current_line);
                    current_line = format!("  {token}");
                    kept += 1;
                } else {
                    current_line.clear();
                    break;
                }
            } else if out.len() + current_line.len() + sep.len() + token.len() + 1 + reserve
                <= max_bytes
            {
                current_line.push_str(sep);
                current_line.push_str(&token);
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
    let mut editable: Vec<(usize, String)> = Vec::new();
    let mut packages: Vec<(usize, String)> = Vec::new();

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = cow.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !saw_header {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2
                && parts[0].eq_ignore_ascii_case("package")
                && parts[1].eq_ignore_ascii_case("version")
            {
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
                format!("{}=={} ({})", parts[0], parts[1], parts[2..].join(" ")),
            ));
        } else {
            packages.push((line_no, format!("{}=={}", parts[0], parts[1])));
        }
    }

    if !saw_header || packages.len() + editable.len() < 6 {
        return None;
    }

    pack_package_list("pip list", &editable, &packages, raw_len, max_bytes)
}

fn format_pip_freeze(lines: &[Cow<'_, str>], raw_len: usize, max_bytes: usize) -> Option<String> {
    let mut editable: Vec<(usize, String)> = Vec::new();
    let mut packages: Vec<(usize, String)> = Vec::new();

    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = cow.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with("-e ") || trimmed.contains(" @ ") {
            editable.push((line_no, trimmed.to_owned()));
        } else if let Some((name, ver)) = trimmed.split_once("==")
            && !name.is_empty()
            && !ver.is_empty()
            && !name.contains(char::is_whitespace)
            && !ver.contains(char::is_whitespace)
        {
            packages.push((line_no, trimmed.to_owned()));
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
    editable: &[(usize, String)],
    packages: &[(usize, String)],
    raw_len: usize,
    max_bytes: usize,
) -> Option<String> {
    let total = editable.len() + packages.len();
    let mut out = String::new();
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
    for (line_no, entry) in editable {
        let line = format!("  {entry} {}\n", range_tag(*line_no, *line_no));
        if out.len() + line.len() + reserve <= max_bytes {
            out.push_str(&line);
        }
    }

    // Build wrapped lines (~96 chars each) tracking first/last stdout line per packed row.
    let mut packed_rows: Vec<(usize, usize, String)> = Vec::new();
    let mut cur = String::new();
    let mut cur_first = 0usize;
    let mut cur_last = 0usize;
    for (line_no, pkg) in packages {
        let sep = if cur.is_empty() { "" } else { ", " };
        if !cur.is_empty() && cur.len() + sep.len() + pkg.len() > 96 {
            packed_rows.push((cur_first, cur_last, std::mem::take(&mut cur)));
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
}
