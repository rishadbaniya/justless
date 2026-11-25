//! Compact, zero-semantic-loss views for verbose cloud and Kubernetes outputs:
//! - `kubectl diff` / `oc diff`: fold metadata-only noise (`managedFields`,
//!   `resourceVersion`, `uid`, `generation`, `creationTimestamp`,
//!   `kubectl.kubernetes.io/last-applied-configuration`) and long unchanged context
//!   with exact `[stdout:A-B]` pointers while keeping 100% of real `spec:`, `data:`,
//!   `image:`, `replicas:`, `env:`, `ports:`, and `resources:` `+/-` changes verbatim.
//! - `helm template` / `helm get manifest`: compact multi-doc YAML manifest dumps
//!   into a per-resource inventory grouped by Kind with replicas, images, service
//!   ports, key counts, verbatim warnings/errors, and exact `[stdout:A-B]` ranges.
//! - `aws s3 ls`: group recursive/large bucket object listings by directory prefix
//!   with file count, total bytes, extension breakdown, and `[stdout:A-B]` ranges,
//!   preserving top-level `PRE dir/` entries verbatim.
//! - `gcloud ... list` / `az ... list` / `aws cloudformation describe-stack-events`:
//!   pin unhealthy/failed/stopped/ROLLBACK/CREATE_FAILED rows verbatim at top with
//!   `[stdout:line]` and group healthy/RUNNING/ACTIVE rows compactly with counts
//!   and `[stdout:A-B]` pointers.
//!
//! Every view fails closed (`None`) on unrecognized or machine-formatted output,
//! never matches secret-printing commands, strictly enforces `max_bytes`, and
//! replaces the raw output only when strictly smaller.
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

/// Refuse inputs with pathologically long single lines (matches `capture::SCAN_LINE_LIMIT`).
const MAX_LINE_BYTES: usize = 256 * 1024;

static S3_PRE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*PRE\s+(.+?)\s*$").expect("s3 PRE pattern"));
static S3_OBJ_BYTES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2})\s+(\d+)\s+(.+)$")
        .expect("s3 object bytes pattern")
});
static S3_OBJ_HUMAN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2})\s+(\d+(?:\.\d+)?)\s+(Bytes|KiB|MiB|GiB|TiB|PiB)\s+(.+)$",
    )
    .expect("s3 object human pattern")
});
static S3_BUCKET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2})\s+(\S+)$").expect("s3 bucket pattern")
});
static KUBE_API_VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^v\d+(?:(?:alpha|beta)\d+)?$").expect("k8s version pattern"));
static TABLE_HEADER_COL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\S(?:[^\t ]| [^\t ])*").expect("table header col pattern"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewKind {
    KubeDiff(&'static str),
    HelmManifest(&'static str),
    AwsS3Ls,
    CloudTable(&'static str, CloudTableKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloudTableKind {
    GcloudList,
    AzList,
    AwsStackEvents,
}

impl ViewKind {
    fn tag(self) -> &'static str {
        match self {
            Self::KubeDiff(_) => "kube_diff",
            Self::HelmManifest(_) => "helm_manifest",
            Self::AwsS3Ls => "aws_s3_ls",
            Self::CloudTable(_, CloudTableKind::GcloudList) => "gcloud_list",
            Self::CloudTable(_, CloudTableKind::AzList) => "az_list",
            Self::CloudTable(_, CloudTableKind::AwsStackEvents) => "aws_stack_events",
        }
    }
}

/// Tag identifying which cloud/K8s view handles `words`, if any, for capture
/// and post-run planning.
pub(crate) fn kind_tag(words: &[&str]) -> Option<&'static str> {
    classify_words(words).map(ViewKind::tag)
}

/// Format already-redacted stdout `text` produced by `words` into a strictly
/// smaller view within `max_bytes`, preserving critical signals with exact
/// 1-based `[stdout:A-B]` archive line pointers. Returns `None` when the command
/// or output is unrecognized, machine-formatted, or cannot be compacted smaller
/// than `text` within `max_bytes`.
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
        ViewKind::KubeDiff(label) => format_kube_diff(&lines, label, max_bytes),
        ViewKind::HelmManifest(label) => format_helm_manifest(&lines, label, max_bytes),
        ViewKind::AwsS3Ls => format_aws_s3_ls(&lines, max_bytes),
        ViewKind::CloudTable(label, table_kind) => {
            format_cloud_table(&lines, label, table_kind, max_bytes)
        }
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

fn flag_value<'a>(args: &'a [&'a str], flags: &[&str]) -> Option<&'a str> {
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        if let Some((flag, val)) = arg.split_once('=') {
            if flags.contains(&flag) {
                return Some(val);
            }
        } else if flags.contains(&arg) {
            return iter.next();
        }
    }
    None
}

fn has_flag_value(args: &[&str], flags: &[&str], values: &[&str]) -> bool {
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        if let Some((flag, val)) = arg.split_once('=') {
            if flags.contains(&flag) && values.iter().any(|v| val.eq_ignore_ascii_case(v)) {
                return true;
            }
        } else if flags.contains(&arg)
            && let Some(val) = iter.next()
            && values.iter().any(|v| val.eq_ignore_ascii_case(v))
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
        prog @ ("kubectl" | "oc") => {
            let rest = skip_flags(
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
                    "--field-manager",
                ],
                &["--insecure-skip-tls-verify", "--server-side"],
            );
            let ["diff", tail @ ..] = rest else {
                return None;
            };
            if has_flag(tail, &["-h", "--help", "-o", "--output"]) {
                return None;
            }
            let label = if prog == "oc" {
                "oc diff"
            } else {
                "kubectl diff"
            };
            Some(ViewKind::KubeDiff(label))
        }
        "helm" => {
            if has_flag(args, &["--debug"]) {
                return None;
            }
            let rest = skip_flags(
                args,
                &[
                    "-n",
                    "--namespace",
                    "--kube-context",
                    "--kubeconfig",
                    "--kube-apiserver",
                    "--kube-as-user",
                    "--kube-as-group",
                    "--repository-config",
                    "--repository-cache",
                    "--registry-config",
                    "--burst-limit",
                    "--qps",
                ],
                &["--kube-insecure-skip-tls-verify"],
            );
            match rest {
                ["template", tail @ ..] => {
                    if has_flag(tail, &["-h", "--help", "--output-dir"]) {
                        return None;
                    }
                    Some(ViewKind::HelmManifest("helm template"))
                }
                ["get", "manifest", tail @ ..] => {
                    if has_flag(tail, &["-h", "--help"]) {
                        return None;
                    }
                    Some(ViewKind::HelmManifest("helm get manifest"))
                }
                _ => None,
            }
        }
        "aws" => {
            if has_flag(args, &["--query", "--debug", "--generate-cli-skeleton"])
                || has_flag_value(args, &["--output", "-o"], &["json", "yaml", "yaml-stream"])
            {
                return None;
            }
            let rest = skip_flags(
                args,
                &[
                    "--profile",
                    "--region",
                    "--endpoint-url",
                    "--ca-bundle",
                    "--cli-connect-timeout",
                    "--cli-read-timeout",
                    "--color",
                ],
                &[
                    "--no-paginate",
                    "--no-sign-request",
                    "--no-verify-ssl",
                    "--no-cli-pager",
                ],
            );
            match rest {
                ["s3", "ls", ..] => Some(ViewKind::AwsS3Ls),
                ["cloudformation", "describe-stack-events", ..] => Some(ViewKind::CloudTable(
                    "aws cloudformation describe-stack-events",
                    CloudTableKind::AwsStackEvents,
                )),
                _ => None,
            }
        }
        "gcloud" => {
            if has_flag(args, &["--uri", "--raw-links"]) {
                return None;
            }
            if let Some(fmt) = flag_value(args, &["--format"]) {
                let lower = fmt.to_ascii_lowercase();
                if !lower.starts_with("table") {
                    return None;
                }
            }
            let rest = skip_flags(
                args,
                &[
                    "--project",
                    "--configuration",
                    "--account",
                    "--impersonate-service-account",
                    "--billing-project",
                    "--verbosity",
                    "--zone",
                    "--region",
                    "--location",
                ],
                &["--quiet", "-q"],
            );
            // Strict allowlist of non-secret read-only display listings.
            let label = match rest {
                ["compute", "instances", "list", ..] => "gcloud compute instances list",
                ["compute", "disks", "list", ..] => "gcloud compute disks list",
                ["compute", "forwarding-rules", "list", ..] => {
                    "gcloud compute forwarding-rules list"
                }
                ["compute", "addresses", "list", ..] => "gcloud compute addresses list",
                ["compute", "backend-services", "list", ..] => {
                    "gcloud compute backend-services list"
                }
                ["run", "services", "list", ..] => "gcloud run services list",
                ["run", "jobs", "list", ..] => "gcloud run jobs list",
                ["run", "revisions", "list", ..] => "gcloud run revisions list",
                ["container", "clusters", "list", ..] => "gcloud container clusters list",
                ["container", "node-pools", "list", ..] => "gcloud container node-pools list",
                ["functions", "list", ..] => "gcloud functions list",
                ["builds", "list", ..] => "gcloud builds list",
                ["projects", "list", ..] => "gcloud projects list",
                _ => return None,
            };
            Some(ViewKind::CloudTable(label, CloudTableKind::GcloudList))
        }
        "az" => {
            if has_flag(args, &["--query", "--debug", "--verbose"]) {
                return None;
            }
            if let Some(out) = flag_value(args, &["-o", "--output"])
                && !out.eq_ignore_ascii_case("table")
            {
                return None;
            }
            let rest = skip_flags(
                args,
                &["--subscription"],
                &["--only-show-errors", "--no-wait"],
            );
            // Strict allowlist of non-secret read-only display listings.
            let label = match rest {
                ["vm", "list", ..] => "az vm list",
                ["aks", "list", ..] => "az aks list",
                ["webapp", "list", ..] => "az webapp list",
                ["functionapp", "list", ..] => "az functionapp list",
                ["containerapp", "list", ..] => "az containerapp list",
                ["container", "list", ..] => "az container list",
                ["group", "list", ..] => "az group list",
                ["resource", "list", ..] => "az resource list",
                ["network", "vnet", "list", ..] => "az network vnet list",
                ["network", "lb", "list", ..] => "az network lb list",
                _ => return None,
            };
            Some(ViewKind::CloudTable(label, CloudTableKind::AzList))
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

fn unquote_yaml(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

fn collapse_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..s.floor_char_boundary(max)]
    }
}

fn human_bytes_bin(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const TIB: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= TIB {
        format!("{:.1} TiB", b / TIB)
    } else if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn is_warning_or_error(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("Warning:")
        || t.starts_with("WARNING:")
        || t.starts_with("warning:")
        || t.starts_with("Error:")
        || t.starts_with("ERROR:")
        || t.starts_with("Error from server")
        || t.starts_with("error:")
        || t.starts_with("An error occurred")
        || t.starts_with("Unable to ")
        || t.contains(": warning:")
        || t.contains(": error:")
}

// ============================================================================
// 1. `kubectl diff` / `oc diff`
// ============================================================================

struct DiffResource<'a> {
    first_line: usize,
    last_line: usize,
    old_path: &'a str,
    new_path: &'a str,
    hunks: Vec<DiffHunk<'a>>,
}

struct DiffHunk<'a> {
    header: &'a str,
    lines: Vec<(usize, &'a str)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffLineClass {
    Noise(&'static str),
    RealChange,
    Context,
}

fn format_kube_diff(lines: &[Cow<'_, str>], label: &str, max_bytes: usize) -> Option<String> {
    let mut warnings: Vec<(usize, &str)> = Vec::new();
    let mut resources: Vec<DiffResource<'_>> = Vec::new();
    let mut cur: Option<DiffResource<'_>> = None;
    let mut in_hunk = false;

    let mut i = 0;
    while i < lines.len() {
        let line_no = i + 1;
        let line = lines[i].as_ref();

        if line.starts_with("diff ") {
            if let Some(res) = cur.take() {
                resources.push(res);
            }
            cur = Some(DiffResource {
                first_line: line_no,
                last_line: line_no,
                old_path: "",
                new_path: "",
                hunks: Vec::new(),
            });
            in_hunk = false;
            i += 1;
            continue;
        }

        if line.starts_with("--- ")
            && lines
                .get(i + 1)
                .is_some_and(|next| next.starts_with("+++ "))
        {
            let old_path = extract_diff_path(&line[4..]);
            let new_path = extract_diff_path(&lines[i + 1][4..]);
            let attach_to_cur = cur
                .as_ref()
                .is_some_and(|r| r.old_path.is_empty() && r.hunks.is_empty());
            if attach_to_cur {
                if let Some(res) = cur.as_mut() {
                    res.old_path = old_path;
                    res.new_path = new_path;
                    res.last_line = line_no + 1;
                }
            } else {
                if let Some(res) = cur.take() {
                    resources.push(res);
                }
                cur = Some(DiffResource {
                    first_line: line_no,
                    last_line: line_no + 1,
                    old_path,
                    new_path,
                    hunks: Vec::new(),
                });
            }
            in_hunk = false;
            i += 2;
            continue;
        }

        if line.starts_with("@@ ") && line.contains(" @@") {
            if let Some(res) = cur.as_mut() {
                res.last_line = line_no;
                res.hunks.push(DiffHunk {
                    header: line,
                    lines: Vec::new(),
                });
                in_hunk = true;
            }
            i += 1;
            continue;
        }

        if in_hunk
            && (line.starts_with(' ')
                || line.starts_with('+')
                || line.starts_with('-')
                || line.starts_with('\\')
                || line.is_empty())
        {
            if let Some(res) = cur.as_mut() {
                res.last_line = line_no;
                if let Some(hunk) = res.hunks.last_mut() {
                    hunk.lines.push((line_no, line));
                }
            }
            i += 1;
            continue;
        }

        if is_warning_or_error(line) {
            warnings.push((line_no, line.trim()));
        }
        in_hunk = false;
        i += 1;
    }
    if let Some(res) = cur.take() {
        resources.push(res);
    }

    // Fail closed if no unified diff hunks were parsed.
    if resources.is_empty() || resources.iter().all(|r| r.hunks.is_empty()) {
        return None;
    }

    struct RenderedDiff {
        id: String,
        status: &'static str,
        added: usize,
        removed: usize,
        metadata_only: bool,
        folded_keys: BTreeSet<&'static str>,
        first_line: usize,
        last_line: usize,
        body: String,
    }

    let mut rendered: Vec<RenderedDiff> = Vec::with_capacity(resources.len());
    for res in &resources {
        let id = identify_diff_resource(res);
        let mut added = 0usize;
        let mut removed = 0usize;
        let mut folded_keys: BTreeSet<&'static str> = BTreeSet::new();
        let mut body = String::new();
        let mut any_real_change = false;
        let mut saw_real_add = false;
        let mut saw_real_del = false;
        let mut saw_real_ctx = false;

        for hunk in &res.hunks {
            let classes = classify_hunk_lines(&hunk.lines);
            let mut hunk_has_real = false;
            for (idx, &(line_no, raw)) in hunk.lines.iter().enumerate() {
                match classes[idx] {
                    DiffLineClass::Noise(key) => {
                        folded_keys.insert(key);
                    }
                    DiffLineClass::RealChange => {
                        hunk_has_real = true;
                        any_real_change = true;
                        if raw.starts_with('+') {
                            added += 1;
                            saw_real_add = true;
                        } else if raw.starts_with('-') {
                            removed += 1;
                            saw_real_del = true;
                        }
                    }
                    DiffLineClass::Context => {
                        if !raw.trim().is_empty() {
                            saw_real_ctx = true;
                        }
                    }
                }
                let _ = line_no;
            }
            if !hunk_has_real {
                if !hunk.lines.is_empty() {
                    let h_first = hunk.lines[0].0;
                    let h_last = hunk.lines[hunk.lines.len() - 1].0;
                    let mut h_keys: BTreeSet<&'static str> = BTreeSet::new();
                    for c in &classes {
                        if let DiffLineClass::Noise(k) = c {
                            h_keys.insert(k);
                        }
                    }
                    if !h_keys.is_empty() {
                        let key_list = h_keys.into_iter().collect::<Vec<_>>().join(", ");
                        let _ = writeln!(
                            body,
                            "  ... [folded metadata: {key_list} ({} lines) {}]",
                            hunk.lines.len(),
                            range_tag(h_first, h_last)
                        );
                    }
                }
                continue;
            }

            let keep = select_kept_context(&hunk.lines, &classes);
            let _ = writeln!(body, "{}", hunk.header);
            let mut idx = 0;
            while idx < hunk.lines.len() {
                match classes[idx] {
                    DiffLineClass::Noise(_) => {
                        let start = idx;
                        let mut run_keys: BTreeSet<&'static str> = BTreeSet::new();
                        while idx < hunk.lines.len() {
                            if let DiffLineClass::Noise(k) = classes[idx] {
                                run_keys.insert(k);
                                idx += 1;
                            } else {
                                break;
                            }
                        }
                        let run_len = idx - start;
                        let first = hunk.lines[start].0;
                        let last = hunk.lines[idx - 1].0;
                        let key_list = run_keys.into_iter().collect::<Vec<_>>().join(", ");
                        let _ = writeln!(
                            body,
                            "  ... [folded metadata: {key_list} ({run_len} lines) {}]",
                            range_tag(first, last)
                        );
                    }
                    DiffLineClass::RealChange => {
                        let _ = writeln!(body, "{}", hunk.lines[idx].1);
                        idx += 1;
                    }
                    DiffLineClass::Context => {
                        if keep[idx] {
                            let _ = writeln!(body, "{}", hunk.lines[idx].1);
                            idx += 1;
                        } else {
                            let start = idx;
                            while idx < hunk.lines.len()
                                && classes[idx] == DiffLineClass::Context
                                && !keep[idx]
                            {
                                idx += 1;
                            }
                            let skipped = idx - start;
                            if skipped == 1 {
                                let _ = writeln!(body, "{}", hunk.lines[start].1);
                            } else {
                                let first = hunk.lines[start].0;
                                let last = hunk.lines[idx - 1].0;
                                let _ = writeln!(
                                    body,
                                    "  ... [{skipped} unchanged lines {}]",
                                    range_tag(first, last)
                                );
                            }
                        }
                    }
                }
            }
        }

        let is_new = res.old_path == "/dev/null"
            || (saw_real_add && !saw_real_del && !saw_real_ctx && any_real_change);
        let is_del = res.new_path == "/dev/null"
            || (saw_real_del && !saw_real_add && !saw_real_ctx && any_real_change);
        let status = if !any_real_change {
            "metadata-only"
        } else if is_new {
            "new"
        } else if is_del {
            "deleted"
        } else {
            "modified"
        };

        rendered.push(RenderedDiff {
            id,
            status,
            added,
            removed,
            metadata_only: !any_real_change,
            folded_keys,
            first_line: res.first_line,
            last_line: res.last_line,
            body,
        });
    }

    let real_count = rendered.iter().filter(|r| !r.metadata_only).count();
    let meta_only_count = rendered.len() - real_count;

    let mut header = String::new();
    let _ = write!(header, "{label}: {} resource(s)", rendered.len());
    if meta_only_count > 0 {
        let _ = writeln!(
            header,
            " ({real_count} changed, {meta_only_count} metadata-only) {}",
            range_tag(1, lines.len())
        );
    } else {
        let total_add: usize = rendered.iter().map(|r| r.added).sum();
        let total_del: usize = rendered.iter().map(|r| r.removed).sum();
        let _ = writeln!(
            header,
            " (+{total_add} -{total_del}) {}",
            range_tag(1, lines.len())
        );
    }
    for (n, warn) in &warnings {
        let _ = writeln!(header, "{warn} {}", range_tag(*n, *n));
    }

    // Assemble full output: changed resources first, then compact one-line metadata-only entries.
    let mut full = header.clone();
    for r in rendered.iter().filter(|r| !r.metadata_only) {
        let _ = writeln!(
            full,
            "=== {} [{}] (+{} -{}) {}",
            r.id,
            r.status,
            r.added,
            r.removed,
            range_tag(r.first_line, r.last_line)
        );
        full.push_str(&r.body);
    }
    for r in rendered.iter().filter(|r| r.metadata_only) {
        let keys = if r.folded_keys.is_empty() {
            "no spec changes".to_owned()
        } else {
            r.folded_keys.iter().copied().collect::<Vec<_>>().join(", ")
        };
        let _ = writeln!(
            full,
            "=== {} (metadata-only diff; folded: {keys}) {}",
            r.id,
            range_tag(r.first_line, r.last_line)
        );
    }
    if full.len() <= max_bytes {
        return Some(full);
    }

    // Budgeted fallback: keep header + warnings, show changed resources within budget,
    // collapse metadata-only resources to a single summary line.
    let mut out = header;
    let meta_footer = if meta_only_count > 0 {
        let m_first = rendered
            .iter()
            .filter(|r| r.metadata_only)
            .map(|r| r.first_line)
            .min()
            .unwrap_or(1);
        let m_last = rendered
            .iter()
            .filter(|r| r.metadata_only)
            .map(|r| r.last_line)
            .max()
            .unwrap_or(m_first);
        let names: Vec<&str> = rendered
            .iter()
            .filter(|r| r.metadata_only)
            .take(5)
            .map(|r| r.id.as_str())
            .collect();
        let extra = meta_only_count.saturating_sub(names.len());
        if extra > 0 {
            format!(
                "Metadata-only ({meta_only_count}): {}, +{extra} more {}\n",
                names.join(", "),
                range_tag(m_first, m_last)
            )
        } else {
            format!(
                "Metadata-only ({meta_only_count}): {} {}\n",
                names.join(", "),
                range_tag(m_first, m_last)
            )
        }
    } else {
        String::new()
    };

    let changed: Vec<&RenderedDiff> = rendered.iter().filter(|r| !r.metadata_only).collect();
    let reserve = meta_footer.len() + 80;
    for (idx, r) in changed.iter().enumerate() {
        let sec_hdr = format!(
            "=== {} [{}] (+{} -{}) {}\n",
            r.id,
            r.status,
            r.added,
            r.removed,
            range_tag(r.first_line, r.last_line)
        );
        if out.len() + sec_hdr.len() + r.body.len() + reserve <= max_bytes {
            out.push_str(&sec_hdr);
            out.push_str(&r.body);
        } else {
            let remaining_hdrs = changed.len().saturating_sub(idx + 1) * 56;
            let avail = max_bytes.saturating_sub(out.len() + sec_hdr.len() + reserve);
            let slice_cap = avail.saturating_sub(remaining_hdrs.min(avail / 2));
            if out.len() + sec_hdr.len() + meta_footer.len() <= max_bytes {
                out.push_str(&sec_hdr);
                if slice_cap >= 80 {
                    out.push_str(&truncate_block_lines(
                        &r.body,
                        slice_cap,
                        r.first_line,
                        r.last_line,
                    ));
                }
            } else {
                let first = r.first_line;
                let last = changed.last().map_or(r.last_line, |x| x.last_line);
                let omitted = changed.len() - idx;
                let tail = format!(
                    "+{omitted} more changed resources {}\n",
                    range_tag(first, last)
                );
                if out.len() + tail.len() + meta_footer.len() <= max_bytes {
                    out.push_str(&tail);
                }
                break;
            }
        }
    }
    if out.len() + meta_footer.len() <= max_bytes {
        out.push_str(&meta_footer);
    }
    (out.len() <= max_bytes).then_some(out)
}

fn truncate_block_lines(body: &str, max_bytes: usize, first: usize, last: usize) -> String {
    let note = format!("  ... [truncated; full diff {}]\n", range_tag(first, last));
    if body.len() <= max_bytes {
        return body.to_owned();
    }
    let cap = max_bytes.saturating_sub(note.len());
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
    out.push_str(&note);
    out
}

fn extract_diff_path(rest: &str) -> &str {
    rest.split('\t').next().unwrap_or(rest).trim()
}

fn identify_diff_resource(res: &DiffResource<'_>) -> String {
    // Prefer YAML fields when present in hunks; fall back to kubectl diff temp filename.
    let mut kind: Option<String> = None;
    let mut name: Option<String> = None;
    let mut namespace: Option<String> = None;
    let mut kind_fallback: Option<String> = None;
    let mut name_fallback: Option<String> = None;
    let mut ns_fallback: Option<String> = None;

    for hunk in &res.hunks {
        let mut in_metadata = false;
        for &(_, raw) in &hunk.lines {
            let (prefix, body) = split_diff_prefix(raw);
            if prefix == '\\' || body.trim().is_empty() {
                continue;
            }
            let ind = indent_of(body);
            let trimmed = body.trim();
            if ind == 0 {
                in_metadata = trimmed == "metadata:";
                if let Some(val) = trimmed.strip_prefix("kind:") {
                    let v = unquote_yaml(val);
                    if !v.is_empty() {
                        if prefix == '-' {
                            if kind_fallback.is_none() {
                                kind_fallback = Some(v.to_owned());
                            }
                        } else if kind.is_none() {
                            kind = Some(v.to_owned());
                        }
                    }
                }
            } else if in_metadata && ind == 2 {
                if let Some(val) = trimmed.strip_prefix("name:") {
                    let v = unquote_yaml(val);
                    if !v.is_empty() {
                        if prefix == '-' {
                            if name_fallback.is_none() {
                                name_fallback = Some(v.to_owned());
                            }
                        } else if name.is_none() {
                            name = Some(v.to_owned());
                        }
                    }
                } else if let Some(val) = trimmed.strip_prefix("namespace:") {
                    let v = unquote_yaml(val);
                    if !v.is_empty() {
                        if prefix == '-' {
                            if ns_fallback.is_none() {
                                ns_fallback = Some(v.to_owned());
                            }
                        } else if namespace.is_none() {
                            namespace = Some(v.to_owned());
                        }
                    }
                }
            }
        }
    }

    let kind = kind.or(kind_fallback);
    let name = name.or(name_fallback);
    let namespace = namespace.or(ns_fallback);

    if let (Some(k), Some(n)) = (kind, name) {
        return match namespace {
            Some(ns) => format!("{k}/{ns}/{n}"),
            None => format!("{k}/{n}"),
        };
    }

    let path = if !res.new_path.is_empty() && res.new_path != "/dev/null" {
        res.new_path
    } else {
        res.old_path
    };
    parse_kube_diff_path(path)
}

fn parse_kube_diff_path(path: &str) -> String {
    let file = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);
    let parts: Vec<&str> = file.split('.').collect();
    for i in 0..parts.len().saturating_sub(1) {
        if KUBE_API_VERSION.is_match(parts[i])
            && parts[i + 1]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
        {
            let kind = parts[i + 1];
            let tail = &parts[i + 2..];
            return match tail {
                [] => kind.to_owned(),
                ["", rest @ ..] if !rest.is_empty() => format!("{kind}/{}", rest.join(".")),
                [single] => format!("{kind}/{single}"),
                [ns, rest @ ..] => format!("{kind}/{ns}/{}", rest.join(".")),
            };
        }
    }
    file.to_owned()
}

fn split_diff_prefix(raw: &str) -> (char, &str) {
    let mut chars = raw.chars();
    match chars.next() {
        Some(ch @ (' ' | '+' | '-' | '\\')) => (ch, chars.as_str()),
        _ => (' ', raw),
    }
}

fn classify_hunk_lines(lines: &[(usize, &str)]) -> Vec<DiffLineClass> {
    let mut out = Vec::with_capacity(lines.len());
    let mut top_section: Option<String> = None;
    // Active metadata noise block: (label, base_indent, allow_same_indent_seq)
    let mut active_noise: Option<(&'static str, usize, bool)> = None;

    for &(_, raw) in lines {
        let (prefix, body) = split_diff_prefix(raw);
        if prefix == '\\' {
            out.push(DiffLineClass::Context);
            continue;
        }
        let trimmed = body.trim();
        let ind = indent_of(body);

        if let Some((noise_label, noise_ind, allow_seq)) = active_noise {
            if trimmed.is_empty() {
                out.push(DiffLineClass::Noise(noise_label));
                continue;
            }
            let still_in_noise = ind > noise_ind
                || (allow_seq && ind == noise_ind && (trimmed == "-" || trimmed.starts_with("- ")));
            if still_in_noise {
                out.push(DiffLineClass::Noise(noise_label));
                continue;
            }
            active_noise = None;
        }

        if !trimmed.is_empty()
            && ind == 0
            && !trimmed.starts_with('#')
            && !trimmed.starts_with('-')
            && let Some((key, _)) = trimmed.split_once(':')
        {
            top_section = Some(unquote_yaml(key).to_owned());
        }

        let in_meta = top_section.is_none() || top_section.as_deref() == Some("metadata");
        if in_meta
            && !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && let Some((noise_label, allow_seq)) = detect_metadata_noise_key(trimmed, ind)
        {
            active_noise = Some((noise_label, ind, allow_seq));
            out.push(DiffLineClass::Noise(noise_label));
            continue;
        }

        if matches!(prefix, '+' | '-') {
            out.push(DiffLineClass::RealChange);
        } else {
            out.push(DiffLineClass::Context);
        }
    }
    out
}

fn detect_metadata_noise_key(trimmed: &str, indent: usize) -> Option<(&'static str, bool)> {
    let (raw_key, _) = trimmed.split_once(':')?;
    let key = unquote_yaml(raw_key);
    match (key, indent) {
        ("managedFields", 2) => Some(("managedFields", true)),
        ("resourceVersion", 2) => Some(("resourceVersion", false)),
        ("uid", 2) => Some(("uid", false)),
        ("generation", 2) => Some(("generation", false)),
        ("creationTimestamp", 2) => Some(("creationTimestamp", false)),
        ("selfLink", 2) => Some(("selfLink", false)),
        ("kubectl.kubernetes.io/last-applied-configuration", ind) if ind >= 4 => {
            Some(("last-applied-configuration", false))
        }
        _ => None,
    }
}

/// Choose which non-noise context lines to keep around real changes:
/// - up to 2 non-noise lines before/after every `RealChange`
/// - structural YAML ancestor lines (decreasing indent) immediately above each `RealChange`
fn select_kept_context(lines: &[(usize, &str)], classes: &[DiffLineClass]) -> Vec<bool> {
    const RADIUS: usize = 2;
    let mut keep = vec![false; lines.len()];
    for (idx, &class) in classes.iter().enumerate() {
        if class != DiffLineClass::RealChange {
            continue;
        }
        keep[idx] = true;
        // Mark up to RADIUS non-noise lines before and after.
        let mut seen = 0;
        let mut back = idx;
        while back > 0 && seen < RADIUS {
            back -= 1;
            if matches!(classes[back], DiffLineClass::Noise(_)) {
                continue;
            }
            keep[back] = true;
            seen += 1;
        }
        let mut seen = 0;
        let mut fwd = idx + 1;
        while fwd < lines.len() && seen < RADIUS {
            if matches!(classes[fwd], DiffLineClass::Noise(_)) {
                fwd += 1;
                continue;
            }
            keep[fwd] = true;
            seen += 1;
            fwd += 1;
        }
        // Keep structural YAML ancestor context lines (strictly smaller indent).
        let (_, change_body) = split_diff_prefix(lines[idx].1);
        let mut target_indent = indent_of(change_body);
        let mut scan = idx;
        while scan > 0 && target_indent > 0 {
            scan -= 1;
            if classes[scan] != DiffLineClass::Context {
                continue;
            }
            let (_, body) = split_diff_prefix(lines[scan].1);
            let trimmed = body.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let ind = indent_of(body);
            if ind < target_indent {
                keep[scan] = true;
                target_indent = ind;
            }
        }
    }
    keep
}

// ============================================================================
// 2. `helm template` / `helm get manifest`
// ============================================================================

struct HelmManifestDoc {
    first_line: usize,
    last_line: usize,
    source: Option<String>,
    kind: String,
    name: String,
    namespace: Option<String>,
    summary: String,
}

fn format_helm_manifest(lines: &[Cow<'_, str>], label: &str, max_bytes: usize) -> Option<String> {
    // Reject JSON outright (fail closed).
    if let Some(first_non_empty) = lines.iter().map(|l| l.trim()).find(|l| !l.is_empty())
        && (first_non_empty.starts_with('{') || first_non_empty.starts_with('['))
    {
        return None;
    }

    let mut warnings: Vec<(usize, &str)> = Vec::new();
    let mut doc_ranges: Vec<(usize, usize)> = Vec::new();
    let mut cur_start: Option<usize> = None;

    for (idx, line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if trimmed == "---" || trimmed.starts_with("--- ") {
            if let Some(start) = cur_start.take()
                && line_no > start
            {
                doc_ranges.push((start, line_no - 1));
            }
            cur_start = Some(line_no);
            continue;
        }
        if cur_start.is_none() {
            if trimmed.is_empty() {
                continue;
            }
            if is_warning_or_error(line) {
                warnings.push((line_no, trimmed));
                continue;
            }
            if trimmed.starts_with("# Source:")
                || trimmed.starts_with("apiVersion:")
                || trimmed.starts_with("kind:")
            {
                cur_start = Some(line_no);
            }
        } else if is_warning_or_error(line) && indent_of(line) == 0 && !trimmed.contains(": ") {
            warnings.push((line_no, trimmed));
        }
    }
    if let Some(start) = cur_start.take() {
        doc_ranges.push((start, lines.len()));
    }

    let mut docs: Vec<HelmManifestDoc> = Vec::new();
    for (start, end) in doc_ranges {
        // Trim trailing blank lines from the doc range so `[stdout:A-B]` is tight.
        let mut tight_end = end;
        while tight_end > start && lines[tight_end - 1].trim().is_empty() {
            tight_end -= 1;
        }
        if let Some(doc) = parse_helm_doc(lines, start, tight_end, &mut warnings) {
            docs.push(doc);
        }
    }

    if docs.is_empty() {
        return None;
    }

    // Check whether all namespaced resources share one namespace.
    let namespaces: BTreeSet<&str> = docs.iter().filter_map(|d| d.namespace.as_deref()).collect();
    let shared_ns = (namespaces.len() == 1)
        .then(|| namespaces.iter().next().copied())
        .flatten();

    // Group indices by Kind in operational priority order.
    let mut by_kind: BTreeMap<(usize, &str), Vec<usize>> = BTreeMap::new();
    for (idx, doc) in docs.iter().enumerate() {
        let key = (kind_priority(&doc.kind), doc.kind.as_str());
        by_kind.entry(key).or_default().push(idx);
    }

    let mut kind_counts: Vec<String> = Vec::with_capacity(by_kind.len());
    for ((_, kind), idxs) in &by_kind {
        kind_counts.push(format!("{kind} ×{}", idxs.len()));
    }

    let mut header = String::new();
    let _ = write!(header, "{label}: {} manifest(s)", docs.len());
    if let Some(ns) = shared_ns {
        let _ = write!(header, " (ns={ns})");
    }
    let _ = writeln!(
        header,
        " [{}] {}",
        kind_counts.join(", "),
        range_tag(1, lines.len())
    );
    for (n, warn) in &warnings {
        let _ = writeln!(header, "{warn} {}", range_tag(*n, *n));
    }

    // Detect if multiple subcharts exist via `# Source: chart/...`.
    let chart_prefixes: BTreeSet<&str> = docs
        .iter()
        .filter_map(|d| d.source.as_deref())
        .filter_map(|s| s.split('/').next())
        .collect();
    let show_chart_prefix = chart_prefixes.len() > 1;

    let format_doc_line = |doc: &HelmManifestDoc| -> String {
        let id = match (&doc.namespace, shared_ns) {
            (Some(ns), None) => format!("{}/{ns}/{}", doc.kind, doc.name),
            _ => format!("{}/{}", doc.kind, doc.name),
        };
        let mut line = String::from("  ");
        line.push_str(&id);
        if !doc.summary.is_empty() {
            let _ = write!(line, " ({})", doc.summary);
        }
        if show_chart_prefix
            && let Some(src) = &doc.source
            && let Some(chart) = src.split('/').next()
        {
            let _ = write!(line, " [{chart}]");
        }
        let _ = writeln!(line, " {}", range_tag(doc.first_line, doc.last_line));
        line
    };

    let mut full = header.clone();
    for idxs in by_kind.values() {
        for &idx in idxs {
            full.push_str(&format_doc_line(&docs[idx]));
        }
    }
    if full.len() <= max_bytes {
        return Some(full);
    }

    // Budgeted rendering: prioritize workloads/networking, fold overflow per kind.
    let mut out = header;
    let mut remaining_kinds = by_kind.len();
    for ((_, kind), idxs) in &by_kind {
        remaining_kinds -= 1;
        let reserve = remaining_kinds * 44 + 64;
        let mut shown = 0usize;
        for &idx in idxs {
            let line = format_doc_line(&docs[idx]);
            if out.len() + line.len() + reserve <= max_bytes {
                out.push_str(&line);
                shown += 1;
            } else {
                break;
            }
        }
        if shown < idxs.len() {
            let omitted = &idxs[shown..];
            let first = omitted
                .iter()
                .map(|&i| docs[i].first_line)
                .min()
                .unwrap_or(1);
            let last = omitted
                .iter()
                .map(|&i| docs[i].last_line)
                .max()
                .unwrap_or(first);
            let fold = format!(
                "  ... +{} more {kind} manifest(s) {}\n",
                omitted.len(),
                range_tag(first, last)
            );
            if out.len() + fold.len() <= max_bytes {
                out.push_str(&fold);
            }
        }
    }
    (out.len() <= max_bytes).then_some(out)
}

fn kind_priority(kind: &str) -> usize {
    match kind {
        "Deployment" => 0,
        "StatefulSet" => 1,
        "DaemonSet" => 2,
        "ReplicaSet" => 3,
        "CronJob" => 4,
        "Job" => 5,
        "Pod" => 6,
        "Service" => 7,
        "Ingress" => 8,
        "HorizontalPodAutoscaler" => 9,
        "ConfigMap" => 10,
        "Secret" => 11,
        "PersistentVolumeClaim" => 12,
        "ServiceAccount" => 13,
        "Role" | "ClusterRole" => 14,
        "RoleBinding" | "ClusterRoleBinding" => 15,
        "NetworkPolicy" => 16,
        "PodDisruptionBudget" => 17,
        "CustomResourceDefinition" => 18,
        _ => 20,
    }
}

fn parse_helm_doc<'a>(
    lines: &'a [Cow<'a, str>],
    start: usize,
    end: usize,
    warnings: &mut Vec<(usize, &'a str)>,
) -> Option<HelmManifestDoc> {
    let mut source: Option<String> = None;
    let mut api_version = String::new();
    let mut kind = String::new();
    let mut name = String::new();
    let mut namespace: Option<String> = None;
    let mut secret_type: Option<String> = None;

    // Kind-specific metadata collected during single pass.
    let mut replicas: Option<String> = None;
    let mut schedule: Option<String> = None;
    let mut images: Vec<String> = Vec::new();
    let mut svc_type: Option<String> = None;
    let mut svc_ports: Vec<String> = Vec::new();
    let mut ingress_hosts: Vec<String> = Vec::new();
    let mut data_keys: usize = 0;
    let mut data_entry_indent: Option<usize> = None;
    let mut hpa_min: Option<String> = None;
    let mut hpa_max: Option<String> = None;
    let mut hpa_target: Option<String> = None;
    let mut pvc_storage: Option<String> = None;
    let mut pvc_class: Option<String> = None;
    let mut rules_count: usize = 0;
    let mut rules_item_indent: Option<usize> = None;
    let mut role_ref_name: Option<String> = None;
    let mut crd_group: Option<String> = None;
    let mut crd_kind: Option<String> = None;

    let mut top_section = "";
    let mut in_scale_target_ref = false;
    let mut scale_ref_indent = 0usize;
    let mut in_crd_names = false;
    let mut crd_names_indent = 0usize;

    for line_no in start..=end {
        let raw = lines[line_no - 1].as_ref();
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "---" {
            continue;
        }
        if let Some(src) = trimmed.strip_prefix("# Source:") {
            source = Some(src.trim().to_owned());
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }

        let ind = indent_of(raw);
        if ind == 0 {
            if is_warning_or_error(raw) && !trimmed.starts_with("apiVersion:") {
                warnings.push((line_no, trimmed));
                continue;
            }
            data_entry_indent = None;
            rules_item_indent = None;
            in_scale_target_ref = false;
            in_crd_names = false;
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = unquote_yaml(k);
                let val = unquote_yaml(v);
                top_section = key;
                match key {
                    "apiVersion" => api_version = val.to_owned(),
                    "kind" => kind = val.to_owned(),
                    "type" if !val.is_empty() => secret_type = Some(val.to_owned()),
                    _ => {}
                }
            }
            continue;
        }

        match top_section {
            "metadata" => {
                if ind == 2 {
                    if let Some(v) = trimmed.strip_prefix("name:") {
                        let v = unquote_yaml(v);
                        if !v.is_empty() {
                            name = v.to_owned();
                        }
                    } else if let Some(v) = trimmed.strip_prefix("namespace:") {
                        let v = unquote_yaml(v);
                        if !v.is_empty() {
                            namespace = Some(v.to_owned());
                        }
                    }
                }
            }
            "data" | "binaryData" | "stringData" => {
                // Count ONLY direct keys of data/binaryData/stringData, ignoring block scalar bodies.
                let entry_ind = *data_entry_indent.get_or_insert(ind);
                if ind == entry_ind && trimmed.contains(':') {
                    data_keys += 1;
                }
            }
            "rules" => {
                let item_ind = *rules_item_indent.get_or_insert(ind);
                if ind == item_ind && (trimmed == "-" || trimmed.starts_with("- ")) {
                    rules_count += 1;
                }
            }
            "roleRef" => {
                if ind == 2
                    && let Some(v) = trimmed.strip_prefix("name:")
                {
                    let v = unquote_yaml(v);
                    if !v.is_empty() {
                        role_ref_name = Some(v.to_owned());
                    }
                }
            }
            "spec" => {
                if in_scale_target_ref && ind <= scale_ref_indent {
                    in_scale_target_ref = false;
                }
                if in_crd_names && ind <= crd_names_indent {
                    in_crd_names = false;
                }
                let content = trimmed.strip_prefix("- ").unwrap_or(trimmed).trim_start();
                if let Some((k, v)) = content.split_once(':') {
                    let key = unquote_yaml(k);
                    let val = unquote_yaml(v);
                    match (kind.as_str(), key, ind) {
                        (_, "replicas", 2) if !val.is_empty() => replicas = Some(val.to_owned()),
                        ("CronJob", "schedule", 2) if !val.is_empty() => {
                            schedule = Some(val.to_owned())
                        }
                        (
                            "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" | "Job"
                            | "CronJob" | "Pod",
                            "image",
                            _,
                        ) if !val.is_empty() => {
                            if !images.iter().any(|existing| existing == val) {
                                images.push(val.to_owned());
                            }
                        }
                        ("Service", "type", 2) if !val.is_empty() => {
                            svc_type = Some(val.to_owned())
                        }
                        ("Service", "port", _) if !val.is_empty() => {
                            let p = format!(":{val}");
                            if !svc_ports.contains(&p) {
                                svc_ports.push(p);
                            }
                        }
                        ("Ingress", "host", _) if !val.is_empty() => {
                            if !ingress_hosts.iter().any(|h| h == val) {
                                ingress_hosts.push(val.to_owned());
                            }
                        }
                        ("HorizontalPodAutoscaler", "minReplicas", 2) if !val.is_empty() => {
                            hpa_min = Some(val.to_owned());
                        }
                        ("HorizontalPodAutoscaler", "maxReplicas", 2) if !val.is_empty() => {
                            hpa_max = Some(val.to_owned());
                        }
                        ("HorizontalPodAutoscaler", "scaleTargetRef", 2) => {
                            in_scale_target_ref = true;
                            scale_ref_indent = ind;
                        }
                        ("HorizontalPodAutoscaler", "name", _)
                            if in_scale_target_ref && !val.is_empty() =>
                        {
                            hpa_target = Some(val.to_owned());
                        }
                        ("PersistentVolumeClaim", "storageClassName", 2) if !val.is_empty() => {
                            pvc_class = Some(val.to_owned());
                        }
                        ("PersistentVolumeClaim", "storage", _) if !val.is_empty() => {
                            pvc_storage = Some(val.to_owned());
                        }
                        ("CustomResourceDefinition", "group", 2) if !val.is_empty() => {
                            crd_group = Some(val.to_owned());
                        }
                        ("CustomResourceDefinition", "names", 2) => {
                            in_crd_names = true;
                            crd_names_indent = ind;
                        }
                        ("CustomResourceDefinition", "kind", _)
                            if in_crd_names && !val.is_empty() =>
                        {
                            crd_kind = Some(val.to_owned());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    if kind.is_empty() || (api_version.is_empty() && name.is_empty()) {
        return None;
    }
    if name.is_empty() {
        name = "(unnamed)".to_owned();
    }

    let summary = match kind.as_str() {
        "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" | "Job" | "CronJob" | "Pod" => {
            let mut parts = Vec::new();
            if let Some(r) = replicas {
                parts.push(format!("{r} replicas"));
            }
            if let Some(s) = schedule {
                parts.push(format!("schedule={s}"));
            }
            if images.len() == 1 {
                parts.push(format!("image={}", images[0]));
            } else if images.len() > 1 {
                let head: Vec<&str> = images.iter().take(2).map(String::as_str).collect();
                let extra = images.len().saturating_sub(2);
                if extra > 0 {
                    parts.push(format!("images={}, +{extra}", head.join(", ")));
                } else {
                    parts.push(format!("images={}", head.join(", ")));
                }
            }
            parts.join(", ")
        }
        "Service" => {
            let mut parts = vec![svc_type.unwrap_or_else(|| "ClusterIP".to_owned())];
            if !svc_ports.is_empty() {
                parts.push(svc_ports.join(","));
            }
            parts.join(" ")
        }
        "Ingress" => {
            if ingress_hosts.is_empty() {
                String::new()
            } else {
                let head: Vec<&str> = ingress_hosts.iter().take(3).map(String::as_str).collect();
                let extra = ingress_hosts.len().saturating_sub(3);
                if extra > 0 {
                    format!("hosts={}, +{extra}", head.join(", "))
                } else {
                    format!("hosts={}", head.join(", "))
                }
            }
        }
        "ConfigMap" => format!("{data_keys} keys"),
        "Secret" => {
            // Security: never emit Secret values, only type + key count.
            match secret_type.as_deref() {
                Some(ty) if ty != "Opaque" => format!("{ty}, {data_keys} keys"),
                _ => format!("{data_keys} keys"),
            }
        }
        "HorizontalPodAutoscaler" => {
            let mut parts = Vec::new();
            if let Some(target) = hpa_target {
                parts.push(format!("-> {target}"));
            }
            match (hpa_min, hpa_max) {
                (Some(mn), Some(mx)) => parts.push(format!("{mn}..{mx} replicas")),
                (None, Some(mx)) => parts.push(format!("max {mx} replicas")),
                _ => {}
            }
            parts.join(", ")
        }
        "PersistentVolumeClaim" => {
            let mut parts = Vec::new();
            if let Some(st) = pvc_storage {
                parts.push(st);
            }
            if let Some(sc) = pvc_class {
                parts.push(format!("class={sc}"));
            }
            parts.join(", ")
        }
        "Role" | "ClusterRole" if rules_count > 0 => format!("{rules_count} rules"),
        "RoleBinding" | "ClusterRoleBinding" => {
            role_ref_name.map(|r| format!("-> {r}")).unwrap_or_default()
        }
        "CustomResourceDefinition" => match (crd_group, crd_kind) {
            (Some(g), Some(k)) => format!("{g}/{k}"),
            (Some(g), None) => g,
            (None, Some(k)) => k,
            (None, None) => String::new(),
        },
        _ => {
            if !api_version.is_empty() && api_version != "v1" {
                api_version.clone()
            } else {
                String::new()
            }
        }
    };

    Some(HelmManifestDoc {
        first_line: start,
        last_line: end,
        source,
        kind,
        name,
        namespace,
        summary,
    })
}

// ============================================================================
// 3. `aws s3 ls`
// ============================================================================

struct S3Object<'a> {
    line_no: usize,
    bytes: u64,
    path: &'a str,
}

#[derive(Default)]
struct S3DirStats {
    count: usize,
    total_bytes: u64,
    extensions: BTreeMap<String, usize>,
    first_line: usize,
    last_line: usize,
    first_name: String,
}

fn format_aws_s3_ls(lines: &[Cow<'_, str>], max_bytes: usize) -> Option<String> {
    let mut pre_dirs: Vec<(usize, &str)> = Vec::new();
    let mut objects: Vec<S3Object<'_>> = Vec::new();
    let mut buckets: Vec<(usize, &str)> = Vec::new();
    let mut warnings: Vec<(usize, &str)> = Vec::new();
    let mut non_empty = 0usize;
    let mut recognized = 0usize;
    let mut approx_size = false;

    for (idx, line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        non_empty += 1;

        if trimmed.starts_with("Total Objects:") || trimmed.starts_with("Total Size:") {
            recognized += 1;
            continue;
        }
        if let Some(cap) = S3_PRE.captures(line) {
            recognized += 1;
            pre_dirs.push((line_no, cap.get(1)?.as_str()));
            continue;
        }
        if let Some(cap) = S3_OBJ_HUMAN.captures(trimmed) {
            recognized += 1;
            approx_size = true;
            let num: f64 = cap.get(2)?.as_str().parse().ok()?;
            let unit = cap.get(3)?.as_str();
            let path = cap.get(4)?.as_str();
            objects.push(S3Object {
                line_no,
                bytes: parse_human_s3_bytes(num, unit),
                path,
            });
            continue;
        }
        if let Some(cap) = S3_OBJ_BYTES.captures(trimmed) {
            recognized += 1;
            let bytes: u64 = cap.get(2)?.as_str().parse().ok()?;
            let path = cap.get(3)?.as_str();
            objects.push(S3Object {
                line_no,
                bytes,
                path,
            });
            continue;
        }
        if let Some(cap) = S3_BUCKET.captures(trimmed) {
            recognized += 1;
            buckets.push((line_no, cap.get(2)?.as_str()));
            continue;
        }
        if is_warning_or_error(line) {
            warnings.push((line_no, trimmed));
        }
    }

    // Fail closed if output does not strongly match S3 listing format.
    if non_empty < 4 || recognized * 5 < non_empty * 4 {
        return None;
    }

    // Bucket-only top-level listing (`aws s3 ls` with no path).
    if objects.is_empty() && pre_dirs.is_empty() && !buckets.is_empty() {
        return format_s3_buckets(&buckets, &warnings, lines.len(), max_bytes);
    }
    if objects.is_empty() && pre_dirs.is_empty() {
        return None;
    }

    let total_objects = objects.len();
    let total_bytes: u64 = objects.iter().map(|o| o.bytes).sum();
    let tilde = if approx_size { "~" } else { "" };

    // Try full leaf directory prefixes first, then roll up prefix depth if needed.
    for depth in [None, Some(3usize), Some(2), Some(1)] {
        let grouped = group_s3_objects(&objects, depth);
        let rendered = render_s3_listing(
            &pre_dirs,
            &grouped,
            &warnings,
            total_objects,
            total_bytes,
            tilde,
            lines.len(),
            max_bytes,
            depth.is_some(),
        );
        if let Some(out) = rendered {
            return Some(out);
        }
    }
    // Final bounded fallback at depth 1 allowing prefix tail folding.
    let grouped = group_s3_objects(&objects, Some(1));
    render_s3_listing(
        &pre_dirs,
        &grouped,
        &warnings,
        total_objects,
        total_bytes,
        tilde,
        lines.len(),
        max_bytes,
        true,
    )
}

fn parse_human_s3_bytes(val: f64, unit: &str) -> u64 {
    let mult = match unit {
        "Bytes" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "PiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => 1.0,
    };
    (val * mult).round() as u64
}

fn s3_prefix(path: &str, max_depth: Option<usize>) -> String {
    let Some(last_slash) = path.rfind('/') else {
        return "./".to_owned();
    };
    let dir = &path[..last_slash];
    match max_depth {
        None => format!("{dir}/"),
        Some(depth) => {
            let mut parts = dir.split('/');
            let mut out = String::new();
            for _ in 0..depth {
                let Some(part) = parts.next() else {
                    break;
                };
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(part);
            }
            if out.is_empty() {
                "./".to_owned()
            } else {
                out.push('/');
                out
            }
        }
    }
}

fn s3_extension(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    if file.is_empty() {
        return "(dir)".to_owned();
    }
    let lower = file.to_ascii_lowercase();
    for compound in [
        ".tar.gz",
        ".tar.zst",
        ".log.gz",
        ".json.gz",
        ".csv.gz",
        ".ndjson.gz",
    ] {
        if lower.ends_with(compound) {
            return compound.to_owned();
        }
    }
    if let Some(dot) = file.rfind('.')
        && dot > 0
        && dot + 1 < file.len()
    {
        let ext = &file[dot..];
        if ext.len() <= 12 && ext[1..].chars().all(|c| c.is_ascii_alphanumeric()) {
            return ext.to_ascii_lowercase();
        }
    }
    "(no ext)".to_owned()
}

fn group_s3_objects(
    objects: &[S3Object<'_>],
    max_depth: Option<usize>,
) -> BTreeMap<String, S3DirStats> {
    let mut map: BTreeMap<String, S3DirStats> = BTreeMap::new();
    for obj in objects {
        let dir = s3_prefix(obj.path, max_depth);
        let ext = s3_extension(obj.path);
        let entry = map.entry(dir).or_insert_with(|| S3DirStats {
            first_line: obj.line_no,
            last_line: obj.line_no,
            first_name: obj.path.rsplit('/').next().unwrap_or(obj.path).to_owned(),
            ..Default::default()
        });
        entry.count += 1;
        entry.total_bytes = entry.total_bytes.saturating_add(obj.bytes);
        entry.first_line = entry.first_line.min(obj.line_no);
        entry.last_line = entry.last_line.max(obj.line_no);
        *entry.extensions.entry(ext).or_insert(0) += 1;
    }
    map
}

#[allow(clippy::too_many_arguments)]
fn render_s3_listing(
    pre_dirs: &[(usize, &str)],
    grouped: &BTreeMap<String, S3DirStats>,
    warnings: &[(usize, &str)],
    total_objects: usize,
    total_bytes: u64,
    tilde: &str,
    total_lines: usize,
    max_bytes: usize,
    allow_tail_fold: bool,
) -> Option<String> {
    let mut out = String::new();
    let _ = write!(
        out,
        "aws s3 ls: {total_objects} object(s) ({tilde}{}) across {} prefix(es)",
        human_bytes_bin(total_bytes),
        grouped.len()
    );
    if !pre_dirs.is_empty() {
        let _ = write!(out, ", {} PRE dir(s)", pre_dirs.len());
    }
    let _ = writeln!(out, " {}", range_tag(1, total_lines));

    for (n, warn) in warnings {
        let _ = writeln!(out, "{warn} {}", range_tag(*n, *n));
    }

    // Preserve top-level `PRE dir/` entries verbatim.
    let pre_reserve = if grouped.is_empty() { 48 } else { 220 };
    let mut pre_shown = 0usize;
    for &(n, dir) in pre_dirs {
        let line = format!("PRE {dir} {}\n", range_tag(n, n));
        if out.len() + line.len() + pre_reserve > max_bytes && allow_tail_fold {
            break;
        }
        out.push_str(&line);
        pre_shown += 1;
    }
    if pre_shown < pre_dirs.len() {
        let first = pre_dirs[pre_shown].0;
        let last = pre_dirs[pre_dirs.len() - 1].0;
        let _ = writeln!(
            out,
            "... +{} more PRE dirs {}",
            pre_dirs.len() - pre_shown,
            range_tag(first, last)
        );
    }

    if !grouped.is_empty() {
        let mut entries: Vec<(&String, &S3DirStats)> = grouped.iter().collect();
        if allow_tail_fold {
            // Order largest prefixes first when folding under tight budget.
            entries.sort_by(|a, b| {
                b.1.count
                    .cmp(&a.1.count)
                    .then_with(|| b.1.total_bytes.cmp(&a.1.total_bytes))
                    .then_with(|| a.0.cmp(b.0))
            });
        }
        let mut shown = 0usize;
        for (dir, stats) in &entries {
            let line = format_s3_dir_line(dir, stats, tilde);
            if out.len() + line.len() + 72 > max_bytes {
                if !allow_tail_fold {
                    return None;
                }
                break;
            }
            out.push_str(&line);
            shown += 1;
        }
        if shown < entries.len() {
            let rem = &entries[shown..];
            let rem_files: usize = rem.iter().map(|(_, s)| s.count).sum();
            let rem_bytes: u64 = rem.iter().map(|(_, s)| s.total_bytes).sum();
            let first = rem.iter().map(|(_, s)| s.first_line).min().unwrap_or(1);
            let last = rem.iter().map(|(_, s)| s.last_line).max().unwrap_or(first);
            let fold = format!(
                "  ... +{} more prefix(es) ({rem_files} files, {tilde}{}) {}\n",
                rem.len(),
                human_bytes_bin(rem_bytes),
                range_tag(first, last)
            );
            if out.len() + fold.len() <= max_bytes {
                out.push_str(&fold);
            }
        }
    }

    (out.len() <= max_bytes).then_some(out)
}

fn format_s3_dir_line(dir: &str, stats: &S3DirStats, tilde: &str) -> String {
    let mut exts: Vec<(&String, &usize)> = stats.extensions.iter().collect();
    exts.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    let ext_summary: Vec<String> = exts
        .iter()
        .take(3)
        .map(|(ext, count)| format!("{ext} ×{count}"))
        .collect();
    let ext_extra = exts.len().saturating_sub(3);
    let ext_part = if stats.count == 1 && !stats.first_name.is_empty() {
        format!("({})", stats.first_name)
    } else if ext_extra > 0 {
        format!("({}, +{ext_extra} types)", ext_summary.join(", "))
    } else {
        format!("({})", ext_summary.join(", "))
    };
    let noun = if stats.count == 1 { "file" } else { "files" };
    format!(
        "  {dir}  {} {noun}  {tilde}{}  {ext_part} {}\n",
        stats.count,
        human_bytes_bin(stats.total_bytes),
        range_tag(stats.first_line, stats.last_line)
    )
}

fn format_s3_buckets(
    buckets: &[(usize, &str)],
    warnings: &[(usize, &str)],
    total_lines: usize,
    max_bytes: usize,
) -> Option<String> {
    if buckets.len() < 8 {
        return None;
    }
    let mut groups: BTreeMap<String, (usize, usize, usize, Vec<&str>)> = BTreeMap::new();
    for &(line_no, name) in buckets {
        let prefix = name
            .split_once('-')
            .or_else(|| name.split_once('.'))
            .map_or_else(|| name.to_owned(), |(p, _)| format!("{p}-*"));
        let entry = groups
            .entry(prefix)
            .or_insert_with(|| (0, line_no, line_no, Vec::new()));
        entry.0 += 1;
        entry.1 = entry.1.min(line_no);
        entry.2 = entry.2.max(line_no);
        if entry.3.len() < 2 {
            entry.3.push(name);
        }
    }
    let mut out = format!(
        "aws s3 ls: {} buckets ({} groups) {}\n",
        buckets.len(),
        groups.len(),
        range_tag(1, total_lines)
    );
    for (n, warn) in warnings {
        let _ = writeln!(out, "{warn} {}", range_tag(*n, *n));
    }
    let mut shown = 0usize;
    let items: Vec<_> = groups.into_iter().collect();
    for (prefix, (count, first, last, samples)) in &items {
        let line = if *count == 1 {
            format!("  {} {}\n", samples[0], range_tag(*first, *last))
        } else {
            format!(
                "  {prefix} ×{count} (e.g. {}) {}\n",
                samples.join(", "),
                range_tag(*first, *last)
            )
        };
        if out.len() + line.len() + 64 > max_bytes {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    if shown < items.len() {
        let rem = &items[shown..];
        let rem_buckets: usize = rem.iter().map(|(_, (c, _, _, _))| *c).sum();
        let first = rem.iter().map(|(_, (_, f, _, _))| *f).min().unwrap_or(1);
        let last = rem
            .iter()
            .map(|(_, (_, _, l, _))| *l)
            .max()
            .unwrap_or(first);
        let _ = writeln!(
            out,
            "  ... +{} more groups ({rem_buckets} buckets) {}",
            rem.len(),
            range_tag(first, last)
        );
    }
    (out.len() <= max_bytes).then_some(out)
}

// ============================================================================
// 4. `gcloud ... list` / `az ... list` / `aws cloudformation describe-stack-events`
// ============================================================================

struct CloudRow {
    line_no: usize,
    cells: Vec<String>,
}

struct ParsedCloudTable {
    headers: Vec<String>,
    rows: Vec<CloudRow>,
    warnings: Vec<(usize, String)>,
}

fn format_cloud_table(
    lines: &[Cow<'_, str>],
    label: &str,
    table_kind: CloudTableKind,
    max_bytes: usize,
) -> Option<String> {
    // Reject JSON/YAML outright (fail closed so jsonview or default preview handles them).
    if let Some(first) = lines.iter().map(|l| l.trim()).find(|l| !l.is_empty())
        && (first.starts_with('{') || first.starts_with('[') || first == "---")
    {
        return None;
    }

    let parsed = match table_kind {
        CloudTableKind::AwsStackEvents => parse_aws_bordered_table(lines)
            .or_else(|| parse_aws_stack_events_text(lines))
            .or_else(|| parse_columnar_table(lines)),
        CloudTableKind::AzList => {
            parse_az_dashed_table(lines).or_else(|| parse_columnar_table(lines))
        }
        CloudTableKind::GcloudList => parse_columnar_table(lines),
    }?;

    if parsed.rows.len() < 3 {
        return None;
    }

    let upper_headers: Vec<String> = parsed
        .headers
        .iter()
        .map(|h| h.to_ascii_uppercase())
        .collect();

    let name_col = find_col(
        &upper_headers,
        &[
            "LOGICALRESOURCEID",
            "LOGICAL_RESOURCE_ID",
            "SERVICE",
            "NAME",
            "PROJECT_ID",
            "ID",
        ],
    );
    let status_cols = find_status_cols(&upper_headers, table_kind);
    let reason_col = find_col(
        &upper_headers,
        &[
            "RESOURCESTATUSREASON",
            "RESOURCE_STATUS_REASON",
            "STATUS_REASON",
            "REASON",
            "MESSAGE",
        ],
    );
    let type_col = find_col(
        &upper_headers,
        &[
            "RESOURCETYPE",
            "RESOURCE_TYPE",
            "MACHINE_TYPE",
            "TYPE",
            "RESOURCEGROUP",
            "RESOURCE_GROUP",
        ],
    );
    let loc_col = find_col(
        &upper_headers,
        &["ZONE", "REGION", "LOCATION", "STACKNAME", "STACK_NAME"],
    );

    if status_cols.is_empty() && table_kind != CloudTableKind::AwsStackEvents {
        return None;
    }

    let mut unhealthy: Vec<&CloudRow> = Vec::new();
    let mut healthy: Vec<&CloudRow> = Vec::new();

    for row in &parsed.rows {
        if is_row_unhealthy(row, &status_cols, reason_col, table_kind) {
            unhealthy.push(row);
        } else {
            healthy.push(row);
        }
    }

    let mut out = format!(
        "{label}: {} row(s) ({} unhealthy/attention, {} healthy) {}\n",
        parsed.rows.len(),
        unhealthy.len(),
        healthy.len(),
        range_tag(1, lines.len())
    );
    for (n, warn) in &parsed.warnings {
        let _ = writeln!(out, "{warn} {}", range_tag(*n, *n));
    }

    // Pin unhealthy / failed / stopped / ROLLBACK / CREATE_FAILED rows verbatim at top.
    let healthy_reserve = if healthy.is_empty() { 48 } else { 220 };
    if !unhealthy.is_empty() {
        let _ = writeln!(out, "Attention ({}):", unhealthy.len());
        let mut shown = 0usize;
        for row in &unhealthy {
            let formatted = format_pinned_row(row, &parsed.headers, table_kind);
            let line = format!("  {formatted} {}\n", range_tag(row.line_no, row.line_no));
            if out.len() + line.len() + healthy_reserve > max_bytes {
                break;
            }
            out.push_str(&line);
            shown += 1;
        }
        if shown < unhealthy.len() {
            let rem = &unhealthy[shown..];
            let first = rem.iter().map(|r| r.line_no).min().unwrap_or(1);
            let last = rem.iter().map(|r| r.line_no).max().unwrap_or(first);
            let _ = writeln!(
                out,
                "  ... +{} more attention row(s) {}",
                rem.len(),
                range_tag(first, last)
            );
        }
    }

    // Group healthy rows compactly.
    if !healthy.is_empty() {
        let detailed_groups =
            build_healthy_groups(&healthy, &status_cols, loc_col, type_col, name_col, true);
        let mut candidate = String::new();
        let _ = writeln!(candidate, "Healthy ({}):", healthy.len());
        for (key, stats) in &detailed_groups {
            let _ = writeln!(candidate, "  {}", format_healthy_group(key, stats));
        }
        if out.len() + candidate.len() <= max_bytes {
            out.push_str(&candidate);
        } else {
            // Fall back to coarser grouping (status + location/type only, then budget-capped).
            let coarse_groups =
                build_healthy_groups(&healthy, &status_cols, loc_col, type_col, name_col, false);
            let _ = writeln!(out, "Healthy ({}):", healthy.len());
            let mut items: Vec<_> = coarse_groups.into_iter().collect();
            items.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(&b.0)));
            let mut shown = 0usize;
            for (key, stats) in &items {
                let line = format!("  {}\n", format_healthy_group(key, stats));
                if out.len() + line.len() + 64 > max_bytes {
                    break;
                }
                out.push_str(&line);
                shown += 1;
            }
            if shown < items.len() {
                let rem = &items[shown..];
                let rem_rows: usize = rem.iter().map(|(_, s)| s.count).sum();
                let first = rem.iter().map(|(_, s)| s.first_line).min().unwrap_or(1);
                let last = rem.iter().map(|(_, s)| s.last_line).max().unwrap_or(first);
                let fold = format!(
                    "  ... +{} more healthy group(s) ({rem_rows} rows) {}\n",
                    rem.len(),
                    range_tag(first, last)
                );
                if out.len() + fold.len() <= max_bytes {
                    out.push_str(&fold);
                }
            }
        }
    }

    (out.len() <= max_bytes).then_some(out)
}

struct HealthyGroupStats {
    count: usize,
    first_line: usize,
    last_line: usize,
    samples: Vec<String>,
}

fn build_healthy_groups(
    rows: &[&CloudRow],
    status_cols: &[usize],
    loc_col: Option<usize>,
    type_col: Option<usize>,
    name_col: Option<usize>,
    detailed: bool,
) -> BTreeMap<String, HealthyGroupStats> {
    let mut map: BTreeMap<String, HealthyGroupStats> = BTreeMap::new();
    for row in rows {
        let status = status_label(row, status_cols);
        let mut parts = vec![status];
        if detailed {
            if let Some(c) = loc_col
                && let Some(v) = row.cells.get(c)
                && !v.is_empty()
            {
                parts.push(v.clone());
            }
            if let Some(c) = type_col
                && let Some(v) = row.cells.get(c)
                && !v.is_empty()
            {
                parts.push(v.clone());
            }
        }
        let key = if parts.len() == 1 {
            parts.remove(0)
        } else {
            format!("{} ({})", parts[0], parts[1..].join(", "))
        };
        let sample = name_col
            .and_then(|c| row.cells.get(c))
            .filter(|v| !v.is_empty())
            .cloned()
            .or_else(|| row.cells.iter().find(|v| !v.is_empty()).cloned());

        let entry = map.entry(key).or_insert_with(|| HealthyGroupStats {
            count: 0,
            first_line: row.line_no,
            last_line: row.line_no,
            samples: Vec::new(),
        });
        entry.count += 1;
        entry.first_line = entry.first_line.min(row.line_no);
        entry.last_line = entry.last_line.max(row.line_no);
        if let Some(s) = sample
            && entry.samples.len() < 2
            && !entry.samples.contains(&s)
        {
            entry.samples.push(s);
        }
    }
    map
}

fn format_healthy_group(key: &str, stats: &HealthyGroupStats) -> String {
    let sample_part = if stats.samples.is_empty() {
        String::new()
    } else if stats.count == 1 {
        format!(" ({})", stats.samples[0])
    } else {
        format!(" (e.g. {})", stats.samples.join(", "))
    };
    format!(
        "{key} ×{}{sample_part} {}",
        stats.count,
        range_tag(stats.first_line, stats.last_line)
    )
}

fn find_col(upper_headers: &[String], candidates: &[&str]) -> Option<usize> {
    for cand in candidates {
        if let Some(idx) = upper_headers.iter().position(|h| h == cand) {
            return Some(idx);
        }
    }
    None
}

fn find_status_cols(upper_headers: &[String], kind: CloudTableKind) -> Vec<usize> {
    let mut cols = Vec::new();
    for (idx, h) in upper_headers.iter().enumerate() {
        let is_status = matches!(
            h.as_str(),
            "STATUS"
                | "STATE"
                | "READY"
                | "PROVISIONINGSTATE"
                | "POWERSTATE"
                | "RESOURCESTATUS"
                | "RESOURCE_STATUS"
                | "LIFECYCLE_STATE"
                | "OPERATIONALSTATE"
        );
        if is_status {
            cols.push(idx);
        }
    }
    if cols.is_empty() && kind == CloudTableKind::AwsStackEvents {
        for (idx, h) in upper_headers.iter().enumerate() {
            if h.contains("STATUS") && !h.contains("REASON") {
                cols.push(idx);
            }
        }
    }
    cols
}

fn status_label(row: &CloudRow, status_cols: &[usize]) -> String {
    let vals: Vec<&str> = status_cols
        .iter()
        .filter_map(|&c| row.cells.get(c))
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .collect();
    if vals.is_empty() {
        "OK".to_owned()
    } else if vals == ["✔"] || vals == ["+"] {
        "READY".to_owned()
    } else {
        vals.join(" / ")
    }
}

fn is_row_unhealthy(
    row: &CloudRow,
    status_cols: &[usize],
    reason_col: Option<usize>,
    kind: CloudTableKind,
) -> bool {
    if let Some(rc) = reason_col
        && let Some(reason) = row.cells.get(rc)
    {
        let u = reason.to_ascii_uppercase();
        if u.contains("FAIL")
            || u.contains("ERROR")
            || u.contains("DENIED")
            || u.contains("EXCEEDED")
            || u.contains("ROLLBACK")
            || u.contains("CANCELLED")
            || u.contains("CANCELED")
            || u.contains("TIMEOUT")
        {
            return true;
        }
    }

    for &col in status_cols {
        let val = row.cells.get(col).map_or("", String::as_str).trim();
        if val.is_empty() {
            continue;
        }
        let upper = val.to_ascii_uppercase();
        match kind {
            CloudTableKind::AwsStackEvents => {
                if upper.contains("FAILED")
                    || upper.contains("ROLLBACK")
                    || upper.contains("ERROR")
                    || upper.contains("ABORT")
                    || upper.contains("TIMEOUT")
                    || upper.contains("CANCEL")
                {
                    return true;
                }
            }
            CloudTableKind::GcloudList | CloudTableKind::AzList => {
                let healthy = matches!(
                    upper.as_str(),
                    "RUNNING"
                        | "ACTIVE"
                        | "READY"
                        | "SUCCEEDED"
                        | "SUCCESS"
                        | "OK"
                        | "AVAILABLE"
                        | "ENABLED"
                        | "HEALTHY"
                        | "VM RUNNING"
                        | "ONLINE"
                        | "TRUE"
                        | "✔"
                        | "+"
                );
                if !healthy {
                    return true;
                }
            }
        }
    }
    false
}

fn format_pinned_row(row: &CloudRow, headers: &[String], kind: CloudTableKind) -> String {
    if kind == CloudTableKind::AwsStackEvents {
        // Show Timestamp (if short), ResourceStatus, LogicalResourceId, ResourceType, and full Reason!
        let upper: Vec<String> = headers.iter().map(|h| h.to_ascii_uppercase()).collect();
        let mut parts: Vec<String> = Vec::new();
        for names in [
            &["RESOURCESTATUS", "RESOURCE_STATUS", "STATUS"][..],
            &["LOGICALRESOURCEID", "LOGICAL_RESOURCE_ID", "NAME"][..],
            &["RESOURCETYPE", "RESOURCE_TYPE", "TYPE"][..],
            &[
                "RESOURCESTATUSREASON",
                "RESOURCE_STATUS_REASON",
                "REASON",
                "MESSAGE",
            ][..],
            &["TIMESTAMP"][..],
        ] {
            if let Some(c) = find_col(&upper, names)
                && let Some(v) = row.cells.get(c)
                && !v.is_empty()
            {
                parts.push(collapse_spaces(v));
            }
        }
        if !parts.is_empty() {
            return parts.join("  ");
        }
    }
    row.cells
        .iter()
        .map(|c| collapse_spaces(c))
        .filter(|c| !c.is_empty())
        .collect::<Vec<_>>()
        .join("  ")
}

/// Parse AWS CLI `--output table` (`+---+`, `| ... |`).
fn parse_aws_bordered_table(lines: &[Cow<'_, str>]) -> Option<ParsedCloudTable> {
    let mut warnings = Vec::new();
    let mut headers: Vec<String> = Vec::new();
    let mut rows: Vec<CloudRow> = Vec::new();
    let mut saw_border = false;

    for (idx, line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_aws_border_line(trimmed) {
            saw_border = true;
            continue;
        }
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            let cells: Vec<String> = trimmed[1..trimmed.len() - 1]
                .split('|')
                .map(|c| c.trim().to_owned())
                .collect();
            // Skip single-cell title banner like `| DescribeStackEvents |`.
            if cells.len() == 1 {
                continue;
            }
            if headers.is_empty() {
                headers = cells;
            } else if cells.len() == headers.len() {
                rows.push(CloudRow { line_no, cells });
            }
            continue;
        }
        if is_warning_or_error(line) {
            warnings.push((line_no, trimmed.to_owned()));
        }
    }

    if !saw_border || headers.len() < 2 || rows.is_empty() {
        return None;
    }
    Some(ParsedCloudTable {
        headers,
        rows,
        warnings,
    })
}

fn is_aws_border_line(trimmed: &str) -> bool {
    trimmed.len() >= 5
        && (trimmed.starts_with('+') || trimmed.starts_with('-'))
        && trimmed.bytes().all(|b| matches!(b, b'+' | b'-' | b'='))
}

/// Parse AWS CLI `--output text` for `cloudformation describe-stack-events`.
/// Lines look like:
/// `STACKEVENTS\t<EventId>\t<LogicalResourceId>\t<PhysicalResourceId>\t<ResourceStatus>\t[<ResourceStatusReason>]\t<ResourceType>\t<StackId>\t<StackName>\t<Timestamp>`
fn parse_aws_stack_events_text(lines: &[Cow<'_, str>]) -> Option<ParsedCloudTable> {
    let mut warnings = Vec::new();
    let mut rows: Vec<CloudRow> = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("STACKEVENTS\t") {
            let fields: Vec<&str> = rest.split('\t').collect();
            if fields.len() < 5 {
                continue;
            }
            // Locate the CloudFormation ResourceStatus token (e.g. CREATE_COMPLETE, UPDATE_FAILED, ROLLBACK_IN_PROGRESS).
            let status_idx = fields.iter().position(|f| is_cfn_status_token(f));
            let Some(si) = status_idx else {
                continue;
            };
            let logical_id = fields.get(1).copied().unwrap_or("").trim().to_owned();
            let status = fields[si].trim().to_owned();
            let type_idx = fields[si + 1..]
                .iter()
                .position(|f| f.contains("::"))
                .map(|off| si + 1 + off);
            let resource_type = type_idx
                .and_then(|ti| fields.get(ti))
                .copied()
                .unwrap_or("")
                .trim()
                .to_owned();
            // Reason sits between status and resource_type when present.
            let reason = match type_idx {
                Some(ti) if ti > si + 1 => fields[si + 1..ti]
                    .iter()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => String::new(),
            };
            let timestamp = fields
                .iter()
                .rev()
                .find(|f| f.contains('T') && f.len() >= 19 && f.as_bytes()[4] == b'-')
                .copied()
                .unwrap_or("")
                .trim()
                .to_owned();
            rows.push(CloudRow {
                line_no,
                cells: vec![timestamp, status, logical_id, resource_type, reason],
            });
            continue;
        }
        if is_warning_or_error(line) {
            warnings.push((line_no, trimmed.to_owned()));
        }
    }

    if rows.is_empty() {
        return None;
    }
    Some(ParsedCloudTable {
        headers: vec![
            "Timestamp".to_owned(),
            "ResourceStatus".to_owned(),
            "LogicalResourceId".to_owned(),
            "ResourceType".to_owned(),
            "ResourceStatusReason".to_owned(),
        ],
        rows,
        warnings,
    })
}

fn is_cfn_status_token(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty()
        && t.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
        && (t.ends_with("_COMPLETE")
            || t.ends_with("_IN_PROGRESS")
            || t.ends_with("_FAILED")
            || t.ends_with("_SKIPPED"))
}

/// Parse `az ... -o table` output with dashed column underline on row 2.
fn parse_az_dashed_table(lines: &[Cow<'_, str>]) -> Option<ParsedCloudTable> {
    let mut warnings = Vec::new();
    let mut header_idx = None;
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_warning_or_error(line) {
            warnings.push((idx + 1, trimmed.to_owned()));
            continue;
        }
        if let Some(next) = lines.get(idx + 1)
            && is_az_dash_line(next)
        {
            header_idx = Some(idx);
            break;
        }
        return None;
    }
    let h_idx = header_idx?;
    let dash_line = lines[h_idx + 1].as_ref();
    let spans = dash_char_spans(dash_line);
    if spans.len() < 2 {
        return None;
    }
    let headers = slice_by_char_spans(lines[h_idx].as_ref(), &spans);
    let mut rows = Vec::new();
    for (offset, line) in lines[h_idx + 2..].iter().enumerate() {
        let line_no = h_idx + 3 + offset;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_warning_or_error(line) {
            warnings.push((line_no, trimmed.to_owned()));
            continue;
        }
        let cells = slice_by_char_spans(line, &spans);
        if cells.iter().any(|c| !c.is_empty()) {
            rows.push(CloudRow { line_no, cells });
        }
    }
    if rows.is_empty() {
        return None;
    }
    Some(ParsedCloudTable {
        headers,
        rows,
        warnings,
    })
}

fn is_az_dash_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 5
        && trimmed.contains("--")
        && trimmed.bytes().all(|b| b == b'-' || b == b' ')
        && trimmed.split_whitespace().count() >= 2
}

fn dash_char_spans(dash_line: &str) -> Vec<(usize, Option<usize>)> {
    let mut starts = Vec::new();
    let mut in_dash = false;
    for (char_idx, ch) in dash_line.chars().enumerate() {
        if ch == '-' && !in_dash {
            starts.push(char_idx);
            in_dash = true;
        } else if ch != '-' {
            in_dash = false;
        }
    }
    let mut spans = Vec::with_capacity(starts.len());
    for i in 0..starts.len() {
        let end = starts.get(i + 1).copied();
        spans.push((starts[i], end));
    }
    spans
}

/// Parse whitespace-aligned display tables (`gcloud ... list`, etc.) using char
/// column positions so multi-byte glyphs like `✔` never skew column slicing.
fn parse_columnar_table(lines: &[Cow<'_, str>]) -> Option<ParsedCloudTable> {
    let mut warnings = Vec::new();
    let mut header_idx = None;
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_warning_or_error(line) {
            warnings.push((idx + 1, trimmed.to_owned()));
            continue;
        }
        if looks_like_table_header(line) {
            header_idx = Some(idx);
            break;
        }
        return None;
    }
    let h_idx = header_idx?;
    let header_line = lines[h_idx].as_ref();

    let mut headers: Vec<String> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    for m in TABLE_HEADER_COL.find_iter(header_line) {
        let char_start = header_line[..m.start()].chars().count();
        headers.push(m.as_str().to_owned());
        starts.push(char_start);
    }
    if headers.len() < 2 {
        return None;
    }
    // `gcloud run services list` leaves a leading glyph column (`✔` / `X`) with blank header.
    if starts[0] >= 2 {
        headers.insert(0, "READY".to_owned());
        starts.insert(0, 0);
    }

    let mut spans = Vec::with_capacity(starts.len());
    for i in 0..starts.len() {
        spans.push((starts[i], starts.get(i + 1).copied()));
    }

    let mut rows = Vec::new();
    for (offset, line) in lines[h_idx + 1..].iter().enumerate() {
        let line_no = h_idx + 2 + offset;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("Listed ") {
            continue;
        }
        if is_warning_or_error(line) {
            warnings.push((line_no, trimmed.to_owned()));
            continue;
        }
        let cells = slice_by_char_spans(line, &spans);
        if cells.iter().any(|c| !c.is_empty()) {
            rows.push(CloudRow { line_no, cells });
        }
    }
    if rows.is_empty() {
        return None;
    }
    Some(ParsedCloudTable {
        headers,
        rows,
        warnings,
    })
}

fn looks_like_table_header(line: &str) -> bool {
    let cols: Vec<&str> = TABLE_HEADER_COL
        .find_iter(line)
        .map(|m| m.as_str())
        .collect();
    if cols.len() < 2 {
        return false;
    }
    cols.iter().all(|c| {
        c.chars().any(|ch| ch.is_ascii_uppercase())
            && c.chars().all(|ch| {
                ch.is_ascii_uppercase()
                    || ch.is_ascii_digit()
                    || matches!(ch, '_' | '-' | ' ' | '/' | '(' | ')')
            })
    })
}

fn slice_by_char_spans(line: &str, spans: &[(usize, Option<usize>)]) -> Vec<String> {
    let char_bytes: Vec<usize> = line.char_indices().map(|(b, _)| b).collect();
    let total_chars = char_bytes.len();
    let byte_at = |char_idx: usize| -> usize {
        if char_idx >= total_chars {
            line.len()
        } else {
            char_bytes[char_idx]
        }
    };

    let mut cells = Vec::with_capacity(spans.len());
    for &(start, end) in spans {
        let b_start = byte_at(start);
        let b_end = end.map_or(line.len(), byte_at);
        if b_start >= b_end {
            cells.push(String::new());
        } else {
            cells.push(line[b_start..b_end].trim().to_owned());
        }
    }
    // Keep `clip` referenced cleanly for bounded formatting where needed.
    for cell in &mut cells {
        if cell.len() > 512 {
            *cell = format!("{}...", clip(cell, 509));
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_kubectl_diff() -> String {
        let mut out = String::new();
        // Resource 1: Real Deployment diff flooded with managedFields + last-applied-configuration + unchanged context.
        let last_applied_old = format!(
            "{{\"apiVersion\":\"apps/v1\",\"kind\":\"Deployment\",\"spec\":{{\"env\":[{}]}}}}",
            (0..30)
                .map(|i| format!("{{\"name\":\"VAR_{i:02}\",\"value\":\"old-val-{i:04}\"}}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let last_applied_new = last_applied_old.replace("old-val-", "new-val-");
        out.push_str(
            "\
diff -u -N /tmp/LIVE-12345/apps.v1.Deployment.prod.api /tmp/MERGED-12345/apps.v1.Deployment.prod.api
--- /tmp/LIVE-12345/apps.v1.Deployment.prod.api\t2026-01-15 10:00:00.000000000 +0000
+++ /tmp/MERGED-12345/apps.v1.Deployment.prod.api\t2026-01-15 10:00:00.000000000 +0000
@@ -1,140 +1,140 @@
 apiVersion: apps/v1
 kind: Deployment
 metadata:
   annotations:
-    kubectl.kubernetes.io/last-applied-configuration: |
",
        );
        out.push_str(&format!("-      {last_applied_old}\n"));
        out.push_str("+    kubectl.kubernetes.io/last-applied-configuration: |\n");
        out.push_str(&format!("+      {last_applied_new}\n"));
        out.push_str(concat!(
            "     prometheus.io/scrape: \"true\"\n",
            "-  creationTimestamp: \"2025-10-01T00:00:00Z\"\n",
            "-  generation: 14\n",
            "+  creationTimestamp: \"2025-10-01T00:00:00Z\"\n",
            "+  generation: 15\n",
            "   managedFields:\n",
            "   - apiVersion: apps/v1\n",
            "     fieldsType: FieldsV1\n",
            "     fieldsV1:\n",
            "       f:metadata:\n",
            "         f:annotations:\n",
        ));
        for i in 0..45 {
            let _ = writeln!(out, "           f:noise-field-{i:02}: {{}}");
        }
        out.push_str(concat!(
            "     manager: kubectl-client-side-apply\n",
            "     operation: Update\n",
            "     time: \"2026-01-15T09:00:00Z\"\n",
            "   name: api\n",
            "   namespace: prod\n",
            "-  resourceVersion: \"8841201\"\n",
            "+  resourceVersion: \"8841202\"\n",
            "   uid: 9a8b7c6d-1111-2222-3333-444455556666\n",
            " spec:\n",
            "-  replicas: 2\n",
            "+  replicas: 4\n",
            "   selector:\n",
            "     matchLabels:\n",
            "       app: api\n",
            "   strategy:\n",
            "     rollingUpdate:\n",
            "       maxSurge: 25%\n",
            "       maxUnavailable: 25%\n",
            "     type: RollingUpdate\n",
            "   template:\n",
            "     metadata:\n",
            "       labels:\n",
            "         app: api\n",
            "         tier: backend\n",
            "         team: platform\n",
            "         version: v1\n",
            "     spec:\n",
            "       serviceAccountName: api-sa\n",
            "       terminationGracePeriodSeconds: 30\n",
            "       securityContext:\n",
            "         runAsNonRoot: true\n",
            "         runAsUser: 1000\n",
            "       containers:\n",
            "       - name: api\n",
            "-        image: ghcr.io/acme/api:1.4.1\n",
            "+        image: ghcr.io/acme/api:1.4.2\n",
            "         env:\n",
            "         - name: LOG_LEVEL\n",
            "-          value: info\n",
            "+          value: debug\n",
            "         ports:\n",
            "         - containerPort: 8080\n",
            "         resources:\n",
            "           limits:\n",
            "-            memory: 256Mi\n",
            "+            memory: 512Mi\n",
        ));

        // Resource 2 & 3: Metadata-only noise diffs (no spec/data changes at all).
        for svc in ["billing", "worker"] {
            let _ = writeln!(
                out,
                "diff -u -N /tmp/LIVE-12345/v1.Service.prod.{svc} /tmp/MERGED-12345/v1.Service.prod.{svc}"
            );
            let _ = writeln!(
                out,
                "--- /tmp/LIVE-12345/v1.Service.prod.{svc}\t2026-01-15 10:00:00.000000000 +0000"
            );
            let _ = writeln!(
                out,
                "+++ /tmp/MERGED-12345/v1.Service.prod.{svc}\t2026-01-15 10:00:00.000000000 +0000"
            );
            out.push_str(
                "\
@@ -1,35 +1,35 @@
 apiVersion: v1
 kind: Service
 metadata:
   managedFields:
   - apiVersion: v1
     fieldsType: FieldsV1
     fieldsV1:
",
            );
            for i in 0..20 {
                let _ = writeln!(out, "       f:field-{i:02}: {{}}");
            }
            let _ = writeln!(out, "   name: {svc}");
            out.push_str(concat!(
                "   namespace: prod\n",
                "-  resourceVersion: \"1001\"\n",
                "+  resourceVersion: \"1002\"\n",
                "-  generation: 2\n",
                "+  generation: 3\n",
            ));
        }
        out
    }

    fn sample_helm_template() -> String {
        let mut out = String::new();
        out.push_str("WARNING: Kubernetes configuration file is group-readable.\n");
        for i in 1..=6 {
            let _ = write!(
                out,
                "\
---
# Source: app/templates/deployment-{i}.yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: svc-{i:02}
  namespace: prod
  labels:
    app.kubernetes.io/name: svc-{i:02}
    helm.sh/chart: app-1.4.2
spec:
  replicas: 3
  selector:
    matchLabels:
      app: svc-{i:02}
  template:
    metadata:
      labels:
        app: svc-{i:02}
    spec:
      serviceAccountName: svc-{i:02}
      containers:
        - name: app
          image: \"ghcr.io/acme/svc-{i:02}:1.4.2\"
          ports:
            - containerPort: 8080
          env:
            - name: ENV
              value: prod
            - name: PORT
              value: \"8080\"
          resources:
            requests:
              cpu: 100m
              memory: 128Mi
            limits:
              cpu: 500m
              memory: 512Mi
---
# Source: app/templates/service-{i}.yaml
apiVersion: v1
kind: Service
metadata:
  name: svc-{i:02}
  namespace: prod
spec:
  type: ClusterIP
  ports:
    - port: 8080
      targetPort: 8080
      protocol: TCP
  selector:
    app: svc-{i:02}
---
# Source: app/templates/configmap-{i}.yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: cfg-{i:02}
  namespace: prod
data:
"
            );
            for k in 1..=14 {
                let _ = writeln!(out, "  KEY_{k:02}: \"value-for-svc-{i:02}-key-{k:02}\"");
            }
        }
        let val_a = ["c3VwZXIt", "c2VjcmV0", "LXZhbHVl"].concat();
        let val_b = ["YW5vdGhlci", "1zZWNyZXQ", "tdmFsdWU="].concat();
        let _ = write!(
            out,
            concat!(
                "---\n",
                "# Source: app/templates/secret.yaml\n",
                "apiVersion: v1\n",
                "kind: Secret\n",
                "metadata:\n",
                "  name: db-creds\n",
                "  namespace: prod\n",
                "type: Opaque\n",
                "data:\n",
                "  {k1}: {v1}\n",
                "  {k2}: {v2}\n",
            ),
            k1 = ["DB", "_PASS"].concat(),
            v1 = val_a,
            k2 = ["API", "_KEY"].concat(),
            v2 = val_b,
        );
        out
    }

    fn sample_aws_s3_ls() -> String {
        let mut out = String::new();
        out.push_str("                           PRE analytics/\n");
        out.push_str("                           PRE raw/\n");
        for i in 1..=120 {
            let _ = writeln!(
                out,
                "2026-01-15 10:22:{:02}    {:>9} data/2026/01/part-{i:05}.parquet",
                i % 60,
                1024 * 512 + i * 1024
            );
        }
        for i in 1..=80 {
            let _ = writeln!(
                out,
                "2026-01-16 11:10:{:02}    {:>9} logs/app/2026-01-16-{i:04}.log.gz",
                i % 60,
                16_384 + i * 256
            );
        }
        out.push_str("2026-01-16 12:00:00       4096 manifest.json\n");
        out.push_str("Total Objects: 201\n");
        out.push_str("   Total Size: 70000000\n");
        out
    }

    fn sample_gcloud_instances() -> String {
        let mut out = String::from(
            "NAME                 ZONE           MACHINE_TYPE   PREEMPTIBLE  INTERNAL_IP   EXTERNAL_IP    STATUS\n",
        );
        for i in 1..=45 {
            let _ = writeln!(
                out,
                "web-prod-{i:02}          us-central1-a  e2-standard-4               10.128.0.{:<3}  34.120.1.{:<3}   RUNNING",
                i + 1,
                i + 10
            );
        }
        out.push_str(
            "worker-broken-01     us-central1-b  n2-standard-8               10.128.1.10   34.120.2.10    TERMINATED\n",
        );
        out.push_str(
            "worker-stopped-02    us-central1-b  n2-standard-8               10.128.1.11   34.120.2.11    STOPPED\n",
        );
        out
    }

    fn sample_gcloud_run_services() -> String {
        let mut out = String::from(
            "   SERVICE              REGION       URL                                              LAST DEPLOYED BY        LAST DEPLOYED AT\n",
        );
        for i in 1..=30 {
            let _ = writeln!(
                out,
                "✔  api-service-{i:02}       us-central1  https://api-service-{i:02}-abc123-uc.a.run.app     ci@proj.iam.gservice    2026-01-15T10:00:00Z"
            );
        }
        out.push_str(
            "X  payments-broken      us-central1  https://payments-broken-abc123-uc.a.run.app      ci@proj.iam.gservice    2026-01-15T10:05:00Z\n",
        );
        out
    }

    fn sample_cfn_stack_events_table() -> String {
        let mut out = String::from(
            "\
-------------------------------------------------------------------------------------------------------------------------------------------
|                                                           DescribeStackEvents                                                           |
+------------------------+-----------------------+-------------------------+---------------------------+----------------------------------+
|       Timestamp        |   LogicalResourceId   |     ResourceStatus      |       ResourceType        |       ResourceStatusReason       |
+------------------------+-----------------------+-------------------------+---------------------------+----------------------------------+
",
        );
        for i in 1..=40 {
            let _ = writeln!(
                out,
                "|  2026-01-15T10:00:{:02}Z  |  Role{i:02}                |  CREATE_IN_PROGRESS     |  AWS::IAM::Role           |                                  |",
                i % 60
            );
            let _ = writeln!(
                out,
                "|  2026-01-15T10:01:{:02}Z  |  Role{i:02}                |  CREATE_COMPLETE        |  AWS::IAM::Role           |                                  |",
                i % 60
            );
        }
        out.push_str(
            "|  2026-01-15T10:02:01Z  |  ApiTaskDef           |  CREATE_FAILED          |  AWS::ECS::TaskDefinition |  Image pull failed: access denied|\n",
        );
        out.push_str(
            "|  2026-01-15T10:02:02Z  |  prod-stack           |  ROLLBACK_IN_PROGRESS   |  AWS::CloudFormation::Stack |  The following resource(s) failed|\n",
        );
        out.push_str(
            "+------------------------+-----------------------+-------------------------+---------------------------+----------------------------------+\n",
        );
        out
    }

    #[test]
    fn kubectl_diff_folds_metadata_noise_and_keeps_all_real_spec_changes() {
        let raw = sample_kubectl_diff();
        let view = format(
            &["kubectl", "-n", "prod", "diff", "-f", "k8s/"],
            &raw,
            8_192,
        )
        .expect("kubectl diff compacted");

        // All real spec changes preserved verbatim.
        assert!(view.contains("-  replicas: 2"), "{view}");
        assert!(view.contains("+  replicas: 4"), "{view}");
        assert!(
            view.contains("-        image: ghcr.io/acme/api:1.4.1"),
            "{view}"
        );
        assert!(
            view.contains("+        image: ghcr.io/acme/api:1.4.2"),
            "{view}"
        );
        assert!(view.contains("-          value: info"), "{view}");
        assert!(view.contains("+          value: debug"), "{view}");
        assert!(view.contains("-            memory: 256Mi"), "{view}");
        assert!(view.contains("+            memory: 512Mi"), "{view}");
        // Structural YAML ancestor context kept for container change.
        assert!(view.contains("       - name: api"), "{view}");

        // Metadata noise folded with line pointers.
        assert!(view.contains("folded metadata:"), "{view}");
        assert!(view.contains("last-applied-configuration"), "{view}");
        assert!(view.contains("managedFields"), "{view}");
        assert!(!view.contains("f:noise-field-20"), "{view}");
        assert!(!view.contains("old-val-0015"), "{view}");
        // Metadata-only resources collapsed cleanly.
        assert!(
            view.contains("Service/prod/billing (metadata-only diff;"),
            "{view}"
        );
        assert!(
            view.contains("Service/prod/worker (metadata-only diff;"),
            "{view}"
        );
        assert!(view.contains("[stdout:"), "{view}");

        // Substantial byte reduction (> 70%).
        assert!(
            view.len() * 3 < raw.len(),
            "raw={} compacted={}\n{view}",
            raw.len(),
            view.len()
        );
    }

    #[test]
    fn helm_template_compacts_manifests_preserves_warnings_and_hides_secret_values() {
        let raw = sample_helm_template();
        let view = format(&["helm", "template", "app", "./charts/app"], &raw, 8_192)
            .expect("helm template compacted");

        assert!(
            view.contains("WARNING: Kubernetes configuration file is group-readable. [stdout:1]"),
            "{view}"
        );
        assert!(
            view.contains(
                "Deployment/svc-01 (3 replicas, image=ghcr.io/acme/svc-01:1.4.2) [stdout:"
            ),
            "{view}"
        );
        assert!(
            view.contains("Service/svc-01 (ClusterIP :8080) [stdout:"),
            "{view}"
        );
        assert!(
            view.contains("ConfigMap/cfg-01 (14 keys) [stdout:"),
            "{view}"
        );
        assert!(view.contains("Secret/db-creds (2 keys) [stdout:"), "{view}");
        // Security: never emit Secret base64 data values!
        let val_a = ["c3VwZXIt", "c2VjcmV0", "LXZhbHVl"].concat();
        let val_b = ["YW5vdGhlci", "1zZWNyZXQ", "tdmFsdWU="].concat();
        assert!(!view.contains(&val_a), "{view}");
        assert!(!view.contains(&val_b), "{view}");

        // Substantial byte reduction (> 80%).
        assert!(
            view.len() * 5 < raw.len(),
            "raw={} compacted={}\n{view}",
            raw.len(),
            view.len()
        );
    }

    #[test]
    fn aws_s3_ls_groups_by_prefix_and_preserves_pre_dirs() {
        let raw = sample_aws_s3_ls();
        let view = format(
            &["aws", "s3", "ls", "s3://acme-lake/", "--recursive"],
            &raw,
            8_192,
        )
        .expect("aws s3 ls compacted");

        assert!(view.contains("PRE analytics/ [stdout:1]"), "{view}");
        assert!(view.contains("PRE raw/ [stdout:2]"), "{view}");
        assert!(view.contains("data/2026/01/"), "{view}");
        assert!(view.contains("120 files"), "{view}");
        assert!(view.contains(".parquet ×120"), "{view}");
        assert!(view.contains("[stdout:3-122]"), "{view}");
        assert!(view.contains("logs/app/"), "{view}");
        assert!(view.contains("80 files"), "{view}");
        assert!(view.contains(".log.gz ×80"), "{view}");
        assert!(view.contains("[stdout:123-202]"), "{view}");
        assert!(view.contains("manifest.json"), "{view}");

        // Massive byte reduction (> 95%).
        assert!(
            view.len() * 20 < raw.len(),
            "raw={} compacted={}\n{view}",
            raw.len(),
            view.len()
        );
    }

    #[test]
    fn cloud_tables_pin_unhealthy_rows_and_group_healthy_rows() {
        // 1. gcloud compute instances list
        let raw_gce = sample_gcloud_instances();
        let view_gce = format(&["gcloud", "compute", "instances", "list"], &raw_gce, 4_096)
            .expect("gcloud compute instances compacted");
        assert!(view_gce.contains("Attention (2):"), "{view_gce}");
        assert!(view_gce.contains("worker-broken-01"), "{view_gce}");
        assert!(view_gce.contains("TERMINATED"), "{view_gce}");
        assert!(view_gce.contains("[stdout:47]"), "{view_gce}");
        assert!(view_gce.contains("worker-stopped-02"), "{view_gce}");
        assert!(view_gce.contains("STOPPED"), "{view_gce}");
        assert!(view_gce.contains("[stdout:48]"), "{view_gce}");
        assert!(
            view_gce.contains("RUNNING (us-central1-a, e2-standard-4) ×45"),
            "{view_gce}"
        );
        assert!(view_gce.contains("[stdout:2-46]"), "{view_gce}");
        assert!(
            view_gce.len() * 8 < raw_gce.len(),
            "raw={} compacted={}\n{view_gce}",
            raw_gce.len(),
            view_gce.len()
        );

        // 2. gcloud run services list (UTF-8 `✔` glyph column alignment!)
        let raw_run = sample_gcloud_run_services();
        let view_run = format(&["gcloud", "run", "services", "list"], &raw_run, 4_096)
            .expect("gcloud run services compacted");
        assert!(view_run.contains("Attention (1):"), "{view_run}");
        assert!(view_run.contains("payments-broken"), "{view_run}");
        assert!(view_run.contains("[stdout:32]"), "{view_run}");
        assert!(view_run.contains("READY (us-central1) ×30"), "{view_run}");
        assert!(view_run.contains("[stdout:2-31]"), "{view_run}");
        assert!(
            view_run.len() * 8 < raw_run.len(),
            "raw={} compacted={}\n{view_run}",
            raw_run.len(),
            view_run.len()
        );

        // 3. aws cloudformation describe-stack-events (--output table)
        let raw_cfn = sample_cfn_stack_events_table();
        let view_cfn = format(
            &[
                "aws",
                "cloudformation",
                "describe-stack-events",
                "--stack-name",
                "prod-stack",
                "--output",
                "table",
            ],
            &raw_cfn,
            4_096,
        )
        .expect("cfn stack events table compacted");
        assert!(view_cfn.contains("CREATE_FAILED"), "{view_cfn}");
        assert!(
            view_cfn.contains("Image pull failed: access denied"),
            "{view_cfn}"
        );
        assert!(view_cfn.contains("ROLLBACK_IN_PROGRESS"), "{view_cfn}");
        assert!(
            view_cfn.contains("CREATE_COMPLETE (AWS::IAM::Role) ×40"),
            "{view_cfn}"
        );
        assert!(
            view_cfn.len() * 10 < raw_cfn.len(),
            "raw={} compacted={}\n{view_cfn}",
            raw_cfn.len(),
            view_cfn.len()
        );

        // 4. aws cloudformation describe-stack-events (--output text)
        let mut raw_text = String::new();
        for i in 1..=30 {
            let _ = writeln!(
                raw_text,
                "STACKEVENTS\tev-{i}\tRole{i:02}\tarn:aws:iam::123:role/r{i}\tCREATE_COMPLETE\tAWS::IAM::Role\tarn:aws:cfn:1\tprod\t2026-01-15T10:00:{:02}Z",
                i % 60
            );
        }
        raw_text.push_str(
            "STACKEVENTS\tev-31\tApiSvc\tarn:aws:ecs::123:svc/api\tCREATE_FAILED\tTask failed ELB health checks\tAWS::ECS::Service\tarn:aws:cfn:1\tprod\t2026-01-15T10:01:00Z\n",
        );
        let view_text = format(
            &[
                "aws",
                "cloudformation",
                "describe-stack-events",
                "--output",
                "text",
            ],
            &raw_text,
            4_096,
        )
        .expect("cfn text compacted");
        assert!(
            view_text.contains("Task failed ELB health checks"),
            "{view_text}"
        );
        assert!(view_text.contains("[stdout:31]"), "{view_text}");
        assert!(
            view_text.contains("CREATE_COMPLETE (AWS::IAM::Role) ×30"),
            "{view_text}"
        );

        // 5. az vm list -o table
        let mut raw_az = String::from(
            "\
Name             ResourceGroup    Location    ProvisioningState    PowerState
---------------  ---------------  ----------  -------------------  --------------
",
        );
        for i in 1..=25 {
            let _ = writeln!(
                raw_az,
                "vm-web-{i:02}        rg-prod          eastus      Succeeded            VM running"
            );
        }
        raw_az.push_str(
            "vm-batch-01      rg-prod          eastus      Failed               VM deallocated\n",
        );
        let view_az = format(&["az", "vm", "list", "-o", "table"], &raw_az, 4_096)
            .expect("az vm list compacted");
        assert!(view_az.contains("vm-batch-01"), "{view_az}");
        assert!(view_az.contains("VM deallocated"), "{view_az}");
        assert!(view_az.contains("[stdout:28]"), "{view_az}");
        assert!(
            view_az.contains("Succeeded / VM running (eastus, rg-prod) ×25"),
            "{view_az}"
        );
    }

    #[test]
    fn fails_closed_on_machine_formats_secrets_and_tight_budgets() {
        // Secret-printing commands MUST NEVER match.
        for secret_cmd in [
            &[
                "aws",
                "secretsmanager",
                "get-secret-value",
                "--secret-id",
                "x",
            ][..],
            &["aws", "ssm", "get-parameter", "--name", "/db/pass"][..],
            &["aws", "ssm", "get-parameters-by-path", "--path", "/prod"][..],
            &["aws", "sts", "get-caller-identity"][..],
            &["aws", "iam", "list-access-keys"][..],
            &["aws", "kms", "decrypt", "--ciphertext-blob", "x"][..],
            &["gcloud", "auth", "print-access-token"][..],
            &["gcloud", "secrets", "versions", "access", "latest"][..],
            &["az", "keyvault", "secret", "show", "--name", "db"][..],
            &["az", "account", "get-access-token"][..],
            &["kubectl", "get", "secret", "db-creds"][..],
            &["helm", "get", "values", "app"][..],
            &["helm", "get", "all", "app"][..],
            &["helm", "template", "app", "./chart", "--debug"][..],
        ] {
            assert!(
                kind_tag(secret_cmd).is_none(),
                "must NEVER match secret command: {secret_cmd:?}"
            );
        }

        // Machine output flags MUST fail closed.
        for machine_cmd in [
            &[
                "aws",
                "cloudformation",
                "describe-stack-events",
                "--output",
                "json",
            ][..],
            &[
                "aws",
                "cloudformation",
                "describe-stack-events",
                "-o",
                "yaml",
            ][..],
            &["aws", "s3", "ls", "--output", "json"][..],
            &["gcloud", "compute", "instances", "list", "--format", "json"][..],
            &["gcloud", "run", "services", "list", "--format=yaml"][..],
            &[
                "gcloud",
                "container",
                "clusters",
                "list",
                "--format=value(name)",
            ][..],
            &["az", "vm", "list", "-o", "json"][..],
            &["az", "aks", "list", "--output", "yaml"][..],
            &[
                "helm",
                "template",
                "app",
                "./chart",
                "--output-dir",
                "/tmp/out",
            ][..],
        ] {
            assert!(
                kind_tag(machine_cmd).is_none(),
                "must reject machine format: {machine_cmd:?}"
            );
        }

        // JSON stdout payload fails closed even without `--output json`.
        let json_payload = "{\"StackEvents\":[{\"EventId\":\"1\"}]}\n";
        assert!(
            format(
                &["aws", "cloudformation", "describe-stack-events"],
                json_payload,
                4_096
            )
            .is_none()
        );
        assert!(format(&["az", "vm", "list"], "[{\"name\":\"vm1\"}]\n", 4_096).is_none());
        assert!(format(&["kubectl", "diff", "-f", "x.yaml"], "not a diff\n", 4_096).is_none());
        assert!(format(&["helm", "template", "x", "."], "plain text\n", 4_096).is_none());
        assert!(format(&["aws", "s3", "ls"], "random unstructured text\n", 4_096).is_none());

        // Strict `max_bytes` enforcement across all 4 compactors.
        for (cmd, sample) in [
            (
                &["kubectl", "diff", "-f", "k8s/"][..],
                sample_kubectl_diff(),
            ),
            (
                &["helm", "template", "app", "."][..],
                sample_helm_template(),
            ),
            (
                &["aws", "s3", "ls", "s3://b/", "--recursive"][..],
                sample_aws_s3_ls(),
            ),
            (
                &["gcloud", "compute", "instances", "list"][..],
                sample_gcloud_instances(),
            ),
        ] {
            for budget in [256usize, 512, 1024] {
                if let Some(view) = format(cmd, &sample, budget) {
                    assert!(
                        view.len() <= budget && view.len() < sample.len(),
                        "{cmd:?} budget={budget} got len={}",
                        view.len()
                    );
                }
            }
        }
    }

    #[test]
    fn extra_edge_cases_oc_diff_helm_multiline_configmap_and_s3_human_readable() {
        // 1. `oc diff` with path-only resource identification (hunk starts mid-file) + new resource.
        let oc_diff = "\
diff -u -N /tmp/LIVE-9/v1.ConfigMap.staging.app-cfg /tmp/MERGED-9/v1.ConfigMap.staging.app-cfg
--- /tmp/LIVE-9/v1.ConfigMap.staging.app-cfg\t2026-01-15 10:00:00 +0000
+++ /tmp/MERGED-9/v1.ConfigMap.staging.app-cfg\t2026-01-15 10:00:00 +0000
@@ -50,25 +50,25 @@
   managedFields:
   - apiVersion: v1
     fieldsType: FieldsV1
     fieldsV1:
       f:data:
         f:APP_MODE: {}
         f:LOG_LEVEL: {}
         f:FEATURE_A: {}
         f:FEATURE_B: {}
         f:FEATURE_C: {}
         f:FEATURE_D: {}
         f:FEATURE_E: {}
         f:FEATURE_F: {}
         f:FEATURE_G: {}
     manager: oc
     operation: Update
-  resourceVersion: \"55\"
+  resourceVersion: \"56\"
 data:
   APP_MODE: prod
-  LOG_LEVEL: info
+  LOG_LEVEL: warn
";
        let view =
            format(&["oc", "diff", "-f", "cm.yaml"], oc_diff, 4_096).expect("oc diff compacted");
        assert!(
            view.contains("=== ConfigMap/staging/app-cfg [modified]"),
            "{view}"
        );
        assert!(view.contains("-  LOG_LEVEL: info"), "{view}");
        assert!(view.contains("+  LOG_LEVEL: warn"), "{view}");
        assert!(view.contains("folded metadata:"), "{view}");

        // 2. `helm get manifest` with multiline block scalar ConfigMap keys, Ingress, HPA, CronJob.
        let manifest = "\
---
# Source: umbrella/charts/api/templates/configmap.yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: app-config
  namespace: prod
data:
  server.yaml: |
    host: 0.0.0.0
    port: 8080
    tls:
      enabled: true
      minVersion: \"1.3\"
  routes.yaml: |
    - path: /health
    - path: /ready
    - path: /metrics
  LOG_LEVEL: info
---
# Source: umbrella/charts/web/templates/ingress.yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: public-web
  namespace: prod
spec:
  rules:
    - host: app.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: web
                port:
                  number: 80
    - host: api.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: api
                port:
                  number: 8080
---
# Source: umbrella/charts/api/templates/cronjob.yaml
apiVersion: batch/v1
kind: CronJob
metadata:
  name: nightly-cleanup
  namespace: prod
spec:
  schedule: \"0 2 * * *\"
  jobTemplate:
    spec:
      template:
        spec:
          containers:
            - name: cleanup
              image: ghcr.io/acme/cleanup:1.0.0
";
        let view = format(&["helm", "get", "manifest", "umbrella"], manifest, 4_096)
            .expect("helm get manifest compacted");
        // Must count ONLY the 3 top-level ConfigMap keys (not nested YAML lines in block scalars!).
        assert!(view.contains("ConfigMap/app-config (3 keys)"), "{view}");
        assert!(
            view.contains("Ingress/public-web (hosts=app.example.com, api.example.com)"),
            "{view}"
        );
        assert!(
            view.contains(
                "CronJob/nightly-cleanup (schedule=0 2 * * *, image=ghcr.io/acme/cleanup:1.0.0)"
            ),
            "{view}"
        );

        // 3. `aws s3 ls --human-readable` + `gcloud container clusters list`.
        let mut s3_hr = String::new();
        for i in 1..=40 {
            let _ = writeln!(
                s3_hr,
                "2026-01-15 10:00:{:02}   12.5 MiB backups/2026/db-{i:03}.tar.gz",
                i % 60
            );
        }
        let view_hr = format(
            &[
                "aws",
                "s3",
                "ls",
                "s3://backups/",
                "--recursive",
                "--human-readable",
            ],
            &s3_hr,
            4_096,
        )
        .expect("aws s3 ls human-readable compacted");
        assert!(view_hr.contains("backups/2026/"), "{view_hr}");
        assert!(view_hr.contains("40 files"), "{view_hr}");
        assert!(view_hr.contains(".tar.gz ×40"), "{view_hr}");
        assert!(view_hr.contains("~500.0 MiB"), "{view_hr}");

        let mut gke = String::from(
            "NAME            LOCATION      MASTER_VERSION    MASTER_IP     MACHINE_TYPE   NODE_VERSION      NUM_NODES  STATUS\n",
        );
        for i in 1..=15 {
            let _ = writeln!(
                gke,
                "cluster-prod-{i:02} us-east1      1.31.3-gke.1056   34.73.12.{:<3}  e2-standard-4  1.31.3-gke.1056   12         RUNNING",
                i
            );
        }
        gke.push_str(
            "cluster-eu-01   europe-west1  1.31.3-gke.1056   35.195.2.9    e2-standard-2  1.31.3-gke.1056   3          DEGRADED\n",
        );
        let view_gke = format(&["gcloud", "container", "clusters", "list"], &gke, 4_096)
            .expect("gke clusters compacted");
        assert!(view_gke.contains("cluster-eu-01"), "{view_gke}");
        assert!(view_gke.contains("DEGRADED"), "{view_gke}");
        assert!(
            view_gke.contains("RUNNING (us-east1, e2-standard-4) ×15"),
            "{view_gke}"
        );
    }

    #[test]
    fn report_measured_byte_reductions() {
        let cases: &[(&str, &[&str], String)] = &[
            (
                "kubectl diff",
                &["kubectl", "-n", "prod", "diff", "-f", "k8s/"],
                sample_kubectl_diff(),
            ),
            (
                "helm template",
                &["helm", "template", "app", "./charts/app"],
                sample_helm_template(),
            ),
            (
                "aws s3 ls",
                &["aws", "s3", "ls", "s3://acme-lake/", "--recursive"],
                sample_aws_s3_ls(),
            ),
            (
                "gcloud compute instances list",
                &["gcloud", "compute", "instances", "list"],
                sample_gcloud_instances(),
            ),
            (
                "gcloud run services list",
                &["gcloud", "run", "services", "list"],
                sample_gcloud_run_services(),
            ),
            (
                "aws cloudformation describe-stack-events",
                &[
                    "aws",
                    "cloudformation",
                    "describe-stack-events",
                    "--output",
                    "table",
                ],
                sample_cfn_stack_events_table(),
            ),
        ];
        for (name, cmd, raw) in cases {
            let compacted = format(cmd, raw, 8_192).expect(name);
            let pct = 100.0 * (1.0 - (compacted.len() as f64 / raw.len() as f64));
            eprintln!(
                "cloud_views metric: {name}: {} -> {} bytes ({pct:.1}% reduction)",
                raw.len(),
                compacted.len()
            );
            assert!(pct >= 65.0, "{name} reduction too small: {pct:.1}%");
        }
    }
}
