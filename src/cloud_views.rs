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

/// Strip leading YAML node properties (`!tag`, `!!str`, `&anchor`) before a scalar/flow value.
fn strip_yaml_properties(mut s: &str) -> &str {
    loop {
        s = s.trim_start_matches([' ', '\t']);
        if matches!(s.bytes().next(), Some(b'!' | b'&')) {
            if let Some((_, after)) = s.split_once([' ', '\t']) {
                s = after;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    s.trim_start_matches([' ', '\t'])
}

/// Strip an unquoted trailing YAML comment (` #...` or `\t#...`) from a plain slice.
#[inline]
fn strip_yaml_inline_comment(s: &str) -> &str {
    let s = s.split_once(" #").map_or(s, |(v, _)| v);
    s.split_once("\t#").map_or(s, |(v, _)| v)
}

fn unquote_yaml(raw: &str) -> &str {
    let s = strip_yaml_properties(raw.trim());
    let bytes = s.as_bytes();
    if let Some(&q @ (b'"' | b'\'')) = bytes.first() {
        let mut i = 1;
        while i < bytes.len() {
            let b = bytes[i];
            if q == b'"' && b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                if q == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                let tail = s[i + 1..].trim_start_matches([' ', '\t']);
                if tail.is_empty() || tail.starts_with('#') {
                    return &s[1..i];
                }
                break;
            }
            i += 1;
        }
    }
    let plain = strip_yaml_inline_comment(s).trim();
    if plain.starts_with('#') {
        return "";
    }
    plain
}

fn collapse_spaces(s: &str) -> Cow<'_, str> {
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
fn contains_ascii_icase(haystack: &str, needle_upper: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle_upper.as_bytes();
    if n.is_empty() {
        return true;
    }
    if h.len() < n.len() {
        return false;
    }
    let first_hi = n[0];
    let first_lo = first_hi.to_ascii_lowercase();
    let limit = h.len() - n.len();
    let mut i = 0;
    while i <= limit {
        let b = h[i];
        if (b == first_hi || b == first_lo) && h[i..i + n.len()].eq_ignore_ascii_case(n) {
            return true;
        }
        i += 1;
    }
    false
}

#[inline]
fn ends_with_ascii_icase(haystack: &str, suffix: &str) -> bool {
    haystack.len() >= suffix.len()
        && haystack.as_bytes()[haystack.len() - suffix.len()..]
            .eq_ignore_ascii_case(suffix.as_bytes())
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

    let mut header = String::with_capacity(128 + warnings.len() * 64);
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
    let estimated_full = header.len()
        + rendered
            .iter()
            .map(|r| {
                if r.metadata_only {
                    96
                } else {
                    r.body.len() + 80
                }
            })
            .sum::<usize>();
    let mut full = String::with_capacity(estimated_full);
    full.push_str(&header);
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
    let mut kind: Option<&str> = None;
    let mut name: Option<&str> = None;
    let mut namespace: Option<&str> = None;
    let mut kind_fallback: Option<&str> = None;
    let mut name_fallback: Option<&str> = None;
    let mut ns_fallback: Option<&str> = None;

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
                                kind_fallback = Some(v);
                            }
                        } else if kind.is_none() {
                            kind = Some(v);
                        }
                    }
                }
            } else if in_metadata && ind == 2 {
                if let Some(val) = trimmed.strip_prefix("name:") {
                    let v = unquote_yaml(val);
                    if !v.is_empty() {
                        if prefix == '-' {
                            if name_fallback.is_none() {
                                name_fallback = Some(v);
                            }
                        } else if name.is_none() {
                            name = Some(v);
                        }
                    }
                } else if let Some(val) = trimmed.strip_prefix("namespace:") {
                    let v = unquote_yaml(val);
                    if !v.is_empty() {
                        if prefix == '-' {
                            if ns_fallback.is_none() {
                                ns_fallback = Some(v);
                            }
                        } else if namespace.is_none() {
                            namespace = Some(v);
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

struct HelmManifestDoc<'a> {
    first_line: usize,
    last_line: usize,
    source: Option<&'a str>,
    kind: &'a str,
    name: &'a str,
    namespace: Option<&'a str>,
    summary: String,
}

#[inline]
fn is_helm_notes_header(trimmed: &str) -> bool {
    trimmed == "NOTES:" || trimmed.starts_with("NOTES: ")
}

/// Whether `line` opens a YAML block scalar (`|`, `>`, `|-`, `|+`, `|2-`, `!!str |`, `&a |`, etc.).
fn starts_yaml_block_scalar(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return false;
    }
    let rest = match (trimmed.find(": "), trimmed.find(":\t")) {
        (Some(a), Some(b)) => &trimmed[a.min(b) + 2..],
        (Some(a), None) => &trimmed[a + 2..],
        (None, Some(b)) => &trimmed[b + 2..],
        (None, None) => {
            if let Some(after) = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("-\t"))
            {
                after
            } else {
                return false;
            }
        }
    };
    let val = strip_yaml_inline_comment(rest).trim();
    let val = strip_yaml_properties(val);
    let Some(tail) = val.strip_prefix(['|', '>']) else {
        return false;
    };
    if tail.len() > 2 {
        return false;
    }
    let mut saw_chomp = false;
    let mut saw_indent = false;
    for b in tail.bytes() {
        match b {
            b'-' | b'+' if !saw_chomp => saw_chomp = true,
            b'1'..=b'9' if !saw_indent => saw_indent = true,
            _ => return false,
        }
    }
    true
}

/// Multiline YAML scalar & flow-collection nesting state tracked across lines so
/// scalar bodies and multiline `{...}`/`[...]` flow mappings cannot forge col-0
/// directives (`kind:`, `type:`, `WARNING:`, `NOTES:`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct YamlState {
    open_quote: Option<u8>,
    flow_depth: usize,
    /// Whether the current entry at `flow_depth == 1` has already seen its key/value `:`.
    flow_entry_has_colon: bool,
}

impl YamlState {
    #[inline]
    fn in_multiline(self) -> bool {
        self.open_quote.is_some() || self.flow_depth > 0
    }
}

/// Advance quote (`\"`/`\\` and `''` escape-aware) and flow (`{...}`/`[...]`) nesting
/// across `line`, opening quotes only at YAML value/token starts (never mid-plain-scalar).
/// Returns `(next_state, top_level_flow_keys_seen)` for safe key counting without retaining values.
fn advance_yaml_state(line: &str, mut state: YamlState) -> (YamlState, usize) {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut flow_keys = 0usize;
    let mut expect_token = state.open_quote.is_none();
    let mut just_closed_quote = false;
    let mut saw_block_colon = false;

    while i < bytes.len() {
        let b = bytes[i];
        match state.open_quote {
            Some(b'"') => {
                if b == b'\\' {
                    // Escape the next byte on this line; a trailing `\` escapes the line break.
                    i += 2;
                    continue;
                }
                if b == b'"' {
                    state.open_quote = None;
                    expect_token = false;
                    just_closed_quote = true;
                }
                i += 1;
            }
            Some(b'\'') => {
                if b == b'\'' {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    state.open_quote = None;
                    expect_token = false;
                    just_closed_quote = true;
                }
                i += 1;
            }
            _ => {
                if b == b'#' && (i == 0 || matches!(bytes[i - 1], b' ' | b'\t')) {
                    break;
                }
                if matches!(b, b' ' | b'\t') {
                    i += 1;
                    continue;
                }
                if expect_token && matches!(b, b'!' | b'&') {
                    // Skip YAML tag (`!!str`, `!tag`) or anchor (`&a`) before the token.
                    while i < bytes.len() && !matches!(bytes[i], b' ' | b'\t' | b',' | b']' | b'}')
                    {
                        i += 1;
                    }
                    continue;
                }
                if expect_token && matches!(b, b'"' | b'\'') {
                    state.open_quote = Some(b);
                    expect_token = false;
                    just_closed_quote = false;
                    i += 1;
                    continue;
                }
                if (state.flow_depth > 0 || expect_token) && matches!(b, b'{' | b'[') {
                    if state.flow_depth == 0 {
                        state.flow_entry_has_colon = false;
                    }
                    state.flow_depth += 1;
                    expect_token = true;
                    just_closed_quote = false;
                    i += 1;
                    continue;
                }
                if state.flow_depth > 0 && matches!(b, b'}' | b']') {
                    state.flow_depth -= 1;
                    if state.flow_depth == 0 {
                        state.flow_entry_has_colon = false;
                    }
                    expect_token = false;
                    just_closed_quote = false;
                    i += 1;
                    continue;
                }
                if state.flow_depth > 0 && b == b',' {
                    if state.flow_depth == 1 {
                        state.flow_entry_has_colon = false;
                    }
                    expect_token = true;
                    just_closed_quote = false;
                    i += 1;
                    continue;
                }
                if b == b':' {
                    let next_ws_or_eol =
                        bytes.get(i + 1).is_none_or(|&n| matches!(n, b' ' | b'\t'));
                    if state.flow_depth > 0 {
                        let next_flow_sep = bytes
                            .get(i + 1)
                            .is_none_or(|&n| matches!(n, b' ' | b'\t' | b',' | b']' | b'}'));
                        if just_closed_quote || next_flow_sep {
                            if state.flow_depth == 1 && !state.flow_entry_has_colon {
                                flow_keys += 1;
                                state.flow_entry_has_colon = true;
                            }
                            expect_token = true;
                            just_closed_quote = false;
                            i += 1;
                            continue;
                        }
                    } else if !saw_block_colon && (just_closed_quote || next_ws_or_eol) {
                        saw_block_colon = true;
                        expect_token = true;
                        just_closed_quote = false;
                        i += 1;
                        continue;
                    }
                }
                if state.flow_depth == 0
                    && expect_token
                    && matches!(b, b'-' | b'?')
                    && bytes.get(i + 1).is_none_or(|&n| matches!(n, b' ' | b'\t'))
                {
                    just_closed_quote = false;
                    i += 1;
                    continue;
                }
                expect_token = false;
                just_closed_quote = false;
                i += 1;
            }
        }
    }
    (state, flow_keys)
}

#[inline]
fn is_yaml_doc_sep(line: &str, trimmed: &str) -> bool {
    indent_of(line) == 0 && (trimmed == "---" || trimmed.starts_with("--- "))
}

#[inline]
fn starts_yaml_doc_key(trimmed: &str) -> bool {
    let Some((k, _)) = trimmed.split_once(':') else {
        return false;
    };
    matches!(
        unquote_yaml(k),
        "apiVersion"
            | "kind"
            | "metadata"
            | "data"
            | "binaryData"
            | "stringData"
            | "type"
            | "immutable"
            | "items"
            | "spec"
    )
}

/// Detect `kind: Secret` / `kind: SecretList` inside inline or multiline YAML flow syntax.
fn flow_line_has_secret_kind(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut quote: u8 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if quote != 0 {
            if quote == b'"' && b == b'\\' {
                i += 2;
                continue;
            }
            if b == quote {
                if quote == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                quote = 0;
            }
            i += 1;
            continue;
        }
        if b == b'#' && (i == 0 || matches!(bytes[i - 1], b' ' | b'\t')) {
            break;
        }
        let at_key_boundary = i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'{' | b',' | b'[');
        if at_key_boundary {
            let key_end =
                if bytes[i..].starts_with(b"\"kind\"") || bytes[i..].starts_with(b"'kind'") {
                    Some(i + 6)
                } else if bytes[i..].starts_with(b"kind") {
                    Some(i + 4)
                } else {
                    None
                };
            if let Some(mut j) = key_end {
                while j < bytes.len() && matches!(bytes[j], b' ' | b'\t') {
                    j += 1;
                }
                if bytes.get(j) == Some(&b':') {
                    let rest = &line[j + 1..];
                    let val_raw = rest.split_once([',', '}', ']']).map_or(rest, |(v, _)| v);
                    if matches!(unquote_yaml(val_raw), "Secret" | "SecretList") {
                        return true;
                    }
                }
            }
        }
        if matches!(b, b'"' | b'\'') {
            quote = b;
        }
        i += 1;
    }
    false
}

/// First-pass check over a `---`-delimited YAML document chunk (1-based inclusive line range):
/// returns `true` for `Secret`, `SecretList`, or a `List` containing any `Secret`/`SecretList`,
/// regardless of YAML key order (e.g. `data:`/`stringData:` before `kind: Secret`).
fn chunk_is_secret(lines: &[Cow<'_, str>], start: usize, end: usize) -> bool {
    let mut yaml_state = YamlState::default();
    let mut block_scalar_indent: Option<usize> = None;
    let mut is_list = false;
    let mut has_nested_secret = false;

    for line_no in start..=end {
        let raw = lines[line_no - 1].as_ref();
        let trimmed = raw.trim();
        if trimmed.is_empty() || is_yaml_doc_sep(raw, trimmed) {
            continue;
        }
        let ind = indent_of(raw);
        if yaml_state.in_multiline() {
            if yaml_state.open_quote.is_none() && flow_line_has_secret_kind(raw) {
                has_nested_secret = true;
            }
            yaml_state = advance_yaml_state(raw, yaml_state).0;
            continue;
        }
        if let Some(base_ind) = block_scalar_indent {
            if ind > base_ind {
                continue;
            }
            block_scalar_indent = None;
        }
        if trimmed.starts_with('#') {
            continue;
        }

        if ind == 0 && !trimmed.starts_with("- ") && !trimmed.starts_with("-\t") {
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = unquote_yaml(k);
                let val = unquote_yaml(v);
                if key == "kind" {
                    if matches!(val, "Secret" | "SecretList") {
                        return true;
                    }
                    if val == "List" {
                        is_list = true;
                    }
                } else if v.contains('{') && flow_line_has_secret_kind(v) {
                    has_nested_secret = true;
                }
            }
        } else {
            let content = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("-\t"))
                .unwrap_or(trimmed)
                .trim_start();
            if let Some((k, v)) = content.split_once(':')
                && unquote_yaml(k) == "kind"
                && matches!(unquote_yaml(v), "Secret" | "SecretList")
            {
                has_nested_secret = true;
            } else if content.contains('{') && flow_line_has_secret_kind(content) {
                has_nested_secret = true;
            }
        }

        if starts_yaml_block_scalar(raw) {
            block_scalar_indent = Some(ind);
        } else {
            yaml_state = advance_yaml_state(raw, YamlState::default()).0;
        }
    }

    is_list && has_nested_secret
}

fn format_helm_manifest(lines: &[Cow<'_, str>], label: &str, max_bytes: usize) -> Option<String> {
    // Reject JSON outright (fail closed).
    if let Some(first_non_empty) = lines.iter().map(|l| l.trim()).find(|l| !l.is_empty())
        && (first_non_empty.starts_with('{') || first_non_empty.starts_with('['))
    {
        return None;
    }

    // Pass 1: split into col-0 `---`-delimited chunks so every doc's kind (`Secret`,
    // `SecretList`, or Secret-bearing `List`) is known before any warning/note/field is emitted.
    let mut chunks: Vec<(usize, usize)> = Vec::new();
    let mut chunk_start = 1usize;
    for (idx, cow) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let line = cow.as_ref();
        if line_no > 1 && is_yaml_doc_sep(line, line.trim()) {
            chunks.push((chunk_start, line_no - 1));
            chunk_start = line_no;
        }
    }
    if !lines.is_empty() && chunk_start <= lines.len() {
        chunks.push((chunk_start, lines.len()));
    }

    let mut warnings: Vec<(usize, &str)> = Vec::new();
    let mut notes: Vec<(usize, &str)> = Vec::new();
    let mut doc_ranges: Vec<(usize, usize, bool)> = Vec::new();

    for (c_start, c_end) in chunks {
        let chunk_secret = chunk_is_secret(lines, c_start, c_end);
        let mut cur_start: Option<usize> = None;
        let mut in_notes = false;
        let mut yaml_state = YamlState::default();
        let mut block_scalar_indent: Option<usize> = None;

        for line_no in c_start..=c_end {
            let line = lines[line_no - 1].as_ref();
            let trimmed = line.trim();
            let ind = indent_of(line);

            if is_yaml_doc_sep(line, trimmed) {
                cur_start = Some(line_no);
                continue;
            }

            if trimmed.is_empty() {
                continue;
            }

            if !in_notes && yaml_state.in_multiline() {
                yaml_state = advance_yaml_state(line, yaml_state).0;
                continue;
            }

            if !in_notes && let Some(base_ind) = block_scalar_indent {
                if ind > base_ind {
                    continue;
                }
                block_scalar_indent = None;
            }

            if in_notes {
                if ind == 0
                    && (trimmed.starts_with("# Source:")
                        || trimmed.starts_with("apiVersion:")
                        || trimmed.starts_with("kind:"))
                {
                    in_notes = false;
                    yaml_state = YamlState::default();
                    block_scalar_indent = None;
                    cur_start = Some(line_no);
                } else {
                    if ind == 0 && is_warning_or_error(line) {
                        warnings.push((line_no, trimmed));
                        continue;
                    }
                    notes.push((line_no, line.trim_end()));
                    continue;
                }
            }

            if !chunk_secret && ind == 0 && is_helm_notes_header(trimmed) {
                if let Some(start) = cur_start.take()
                    && line_no > start
                {
                    doc_ranges.push((start, line_no - 1, false));
                }
                in_notes = true;
                yaml_state = YamlState::default();
                block_scalar_indent = None;
                notes.push((line_no, trimmed));
                continue;
            }

            if cur_start.is_none() {
                if ind == 0 && is_warning_or_error(line) {
                    warnings.push((line_no, trimmed));
                    continue;
                }
                if ind == 0 && (trimmed.starts_with("# Source:") || starts_yaml_doc_key(trimmed)) {
                    cur_start = Some(line_no);
                }
            }

            if cur_start.is_some() {
                if !chunk_secret
                    && ind == 0
                    && is_warning_or_error(line)
                    && !trimmed.starts_with("apiVersion:")
                {
                    warnings.push((line_no, trimmed));
                }
                if !trimmed.starts_with('#') {
                    if starts_yaml_block_scalar(line) {
                        block_scalar_indent = Some(ind);
                    } else {
                        yaml_state = advance_yaml_state(line, YamlState::default()).0;
                    }
                }
            }
        }
        if let Some(start) = cur_start.take() {
            doc_ranges.push((start, c_end, chunk_secret));
        }
    }

    let mut docs: Vec<HelmManifestDoc<'_>> = Vec::with_capacity(doc_ranges.len());
    for (start, end, doc_is_secret) in doc_ranges {
        // Trim trailing blank and (outside Secret docs) warning/error lines so
        // `[stdout:A-B]` covers the YAML doc tightly without truncating Secret ranges.
        let mut tight_end = end;
        while tight_end > start {
            let l = lines[tight_end - 1].as_ref();
            let t = l.trim();
            if t.is_empty() || (!doc_is_secret && indent_of(l) == 0 && is_warning_or_error(l)) {
                tight_end -= 1;
            } else {
                break;
            }
        }
        if let Some(doc) = parse_helm_doc(lines, start, tight_end, doc_is_secret) {
            docs.push(doc);
        }
    }

    if docs.is_empty() {
        return None;
    }

    // Check whether all namespaced resources share one namespace.
    let namespaces: BTreeSet<&str> = docs.iter().filter_map(|d| d.namespace).collect();
    let shared_ns = (namespaces.len() == 1)
        .then(|| namespaces.iter().next().copied())
        .flatten();

    // Group indices by Kind in operational priority order.
    let mut by_kind: BTreeMap<(usize, &str), Vec<usize>> = BTreeMap::new();
    for (idx, doc) in docs.iter().enumerate() {
        let key = (kind_priority(doc.kind), doc.kind);
        by_kind.entry(key).or_default().push(idx);
    }

    let mut header = String::with_capacity(128 + by_kind.len() * 20 + warnings.len() * 64);
    let _ = write!(header, "{label}: {} manifest(s)", docs.len());
    if let Some(ns) = shared_ns {
        let _ = write!(header, " (ns={ns})");
    }
    header.push_str(" [");
    for (i, ((_, kind), idxs)) in by_kind.iter().enumerate() {
        if i > 0 {
            header.push_str(", ");
        }
        let _ = write!(header, "{kind} ×{}", idxs.len());
    }
    let _ = writeln!(header, "] {}", range_tag(1, lines.len()));
    for (n, warn) in &warnings {
        let _ = writeln!(header, "{warn} {}", range_tag(*n, *n));
    }

    let mut notes_block = String::new();
    if !notes.is_empty() {
        notes_block.reserve(notes.iter().map(|(_, l)| l.len() + 2).sum::<usize>() + 32);
        let first_note = notes[0].0;
        let last_note = notes[notes.len() - 1].0;
        if notes.len() == 1 {
            let _ = writeln!(
                notes_block,
                "{} {}",
                notes[0].1,
                range_tag(first_note, last_note)
            );
        } else {
            let _ = writeln!(
                notes_block,
                "{} {}",
                notes[0].1,
                range_tag(first_note, last_note)
            );
            for &(_, note_line) in &notes[1..] {
                notes_block.push_str(note_line);
                notes_block.push('\n');
            }
        }
    }

    // Detect if multiple subcharts exist via `# Source: chart/...`.
    let chart_prefixes: BTreeSet<&str> = docs
        .iter()
        .filter_map(|d| d.source)
        .filter_map(|s| s.split('/').next())
        .collect();
    let show_chart_prefix = chart_prefixes.len() > 1;

    let write_doc_line = |dst: &mut String, doc: &HelmManifestDoc<'_>| {
        dst.push_str("  ");
        match (doc.namespace, shared_ns) {
            (Some(ns), None) => {
                let _ = write!(dst, "{}/{ns}/{}", doc.kind, doc.name);
            }
            _ => {
                let _ = write!(dst, "{}/{}", doc.kind, doc.name);
            }
        }
        if !doc.summary.is_empty() {
            let _ = write!(dst, " ({})", doc.summary);
        }
        if show_chart_prefix
            && let Some(src) = doc.source
            && let Some(chart) = src.split('/').next()
        {
            let _ = write!(dst, " [{chart}]");
        }
        let _ = writeln!(dst, " {}", range_tag(doc.first_line, doc.last_line));
    };

    let mut full =
        String::with_capacity(header.len() + notes_block.len() + docs.len().saturating_mul(72));
    full.push_str(&header);
    for idxs in by_kind.values() {
        for &idx in idxs {
            write_doc_line(&mut full, &docs[idx]);
        }
    }
    full.push_str(&notes_block);
    if full.len() <= max_bytes {
        return Some(full);
    }

    // Budgeted rendering: preserve warnings + NOTES verbatim, prioritize workloads/networking,
    // and fold overflow per kind.
    let mut out = String::with_capacity(max_bytes.min(full.len()));
    out.push_str(&header);
    let notes_reserve = if out.len() + notes_block.len() <= max_bytes {
        notes_block.len()
    } else {
        0
    };
    let mut line_buf = String::with_capacity(128);
    let mut remaining_kinds = by_kind.len();
    for ((_, kind), idxs) in &by_kind {
        remaining_kinds -= 1;
        let reserve = notes_reserve + remaining_kinds * 44 + 64;
        let mut shown = 0usize;
        for &idx in idxs {
            line_buf.clear();
            write_doc_line(&mut line_buf, &docs[idx]);
            if out.len() + line_buf.len() + reserve <= max_bytes {
                out.push_str(&line_buf);
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
            line_buf.clear();
            let _ = writeln!(
                line_buf,
                "  ... +{} more {kind} manifest(s) {}",
                omitted.len(),
                range_tag(first, last)
            );
            if out.len() + line_buf.len() + notes_reserve <= max_bytes {
                out.push_str(&line_buf);
            }
        }
    }
    if !notes_block.is_empty() && out.len() + notes_block.len() <= max_bytes {
        out.push_str(&notes_block);
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
    doc_is_secret: bool,
) -> Option<HelmManifestDoc<'a>> {
    let mut source: Option<&'a str> = None;
    let mut api_version: &'a str = "";
    let mut kind: &'a str = "";
    let mut name: &'a str = "";
    let mut namespace: Option<&'a str> = None;
    let mut secret_type: Option<&'a str> = None;

    // Kind-specific metadata collected during a single borrowed pass (zero per-line clones).
    let mut replicas: Option<&'a str> = None;
    let mut schedule: Option<&'a str> = None;
    let mut job_completions: Option<&'a str> = None;
    let mut job_parallelism: Option<&'a str> = None;
    let mut host_network = false;
    let mut vct_count: usize = 0;
    let mut in_vct = false;
    let mut vct_indent = 0usize;
    let mut vct_item_indent: Option<usize> = None;
    let mut images: Vec<&'a str> = Vec::new();
    let mut svc_type: Option<&'a str> = None;
    let mut svc_ports: Vec<&'a str> = Vec::new();
    let mut ingress_hosts: Vec<&'a str> = Vec::new();
    let mut data_keys: usize = 0;
    let mut data_entry_indent: Option<usize> = None;
    let mut hpa_min: Option<&'a str> = None;
    let mut hpa_max: Option<&'a str> = None;
    let mut hpa_target: Option<&'a str> = None;
    let mut pvc_storage: Option<&'a str> = None;
    let mut pvc_class: Option<&'a str> = None;
    let mut rules_count: usize = 0;
    let mut rules_item_indent: Option<usize> = None;
    let mut has_aggregation_rule = false;
    let mut role_ref_name: Option<&'a str> = None;
    let mut crd_group: Option<&'a str> = None;
    let mut crd_kind: Option<&'a str> = None;
    let mut sa_automount: Option<&'a str> = None;
    let mut sa_pull_secrets: Vec<&'a str> = Vec::new();
    let mut sa_secrets_count: usize = 0;
    let mut sa_list_item_indent: Option<usize> = None;

    let mut top_section = "";
    let mut in_scale_target_ref = false;
    let mut scale_ref_indent = 0usize;
    let mut in_crd_names = false;
    let mut crd_names_indent = 0usize;
    let mut yaml_state = YamlState::default();
    let mut block_scalar_indent: Option<usize> = None;

    for line_no in start..=end {
        let raw = lines[line_no - 1].as_ref();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let ind = indent_of(raw);
        if is_yaml_doc_sep(raw, trimmed) {
            yaml_state = YamlState::default();
            block_scalar_indent = None;
            continue;
        }
        if yaml_state.in_multiline() {
            let (next_state, flow_keys) = advance_yaml_state(raw, yaml_state);
            if matches!(top_section, "data" | "binaryData" | "stringData") {
                data_keys += flow_keys;
            }
            yaml_state = next_state;
            continue;
        }
        if let Some(base_ind) = block_scalar_indent {
            if ind > base_ind {
                continue;
            }
            block_scalar_indent = None;
        }
        if let Some(src) = trimmed.strip_prefix("# Source:") {
            if source.is_none() {
                source = Some(src.trim());
            }
            continue;
        }
        if trimmed.starts_with('#') {
            continue;
        }

        let flow_keys = if starts_yaml_block_scalar(raw) {
            block_scalar_indent = Some(ind);
            0
        } else {
            let (next_state, keys) = advance_yaml_state(raw, YamlState::default());
            yaml_state = next_state;
            keys
        };

        // Handle indent-0 YAML sequence items under top-level lists (`rules:`, `imagePullSecrets:`, `secrets:`).
        if ind == 0 && (trimmed == "-" || trimmed.starts_with("- ")) {
            if doc_is_secret {
                continue;
            }
            match top_section {
                "rules" => {
                    let item_ind = *rules_item_indent.get_or_insert(0);
                    if item_ind == 0 {
                        rules_count += 1;
                    }
                    continue;
                }
                "imagePullSecrets" => {
                    if let Some(rest) = trimmed.strip_prefix("- ")
                        && let Some(val) = rest.trim_start().strip_prefix("name:")
                    {
                        let v = unquote_yaml(val);
                        if !v.is_empty() {
                            sa_pull_secrets.push(v);
                        }
                    }
                    continue;
                }
                "secrets" => {
                    let item_ind = *sa_list_item_indent.get_or_insert(0);
                    if item_ind == 0 {
                        sa_secrets_count += 1;
                    }
                    continue;
                }
                _ => {}
            }
        }

        if ind == 0 {
            if !doc_is_secret && is_warning_or_error(raw) && !trimmed.starts_with("apiVersion:") {
                continue;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = unquote_yaml(k);
                let val = unquote_yaml(v);
                if doc_is_secret
                    && !matches!(
                        key,
                        "apiVersion"
                            | "kind"
                            | "metadata"
                            | "type"
                            | "immutable"
                            | "data"
                            | "binaryData"
                            | "stringData"
                    )
                {
                    top_section = "";
                    data_entry_indent = None;
                    continue;
                }
                data_entry_indent = None;
                rules_item_indent = None;
                sa_list_item_indent = None;
                in_scale_target_ref = false;
                in_crd_names = false;
                in_vct = false;
                vct_item_indent = None;
                top_section = key;
                match key {
                    "apiVersion" if api_version.is_empty() && !val.is_empty() => {
                        api_version = val;
                    }
                    "kind" if kind.is_empty() && !val.is_empty() => kind = val,
                    "type" if secret_type.is_none() && !val.is_empty() => secret_type = Some(val),
                    "automountServiceAccountToken" if !doc_is_secret && !val.is_empty() => {
                        sa_automount = Some(val);
                    }
                    "aggregationRule" if !doc_is_secret => has_aggregation_rule = true,
                    "data" | "binaryData" | "stringData"
                        if strip_yaml_properties(v).starts_with('{') =>
                    {
                        data_keys += flow_keys;
                    }
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
                        if name.is_empty() && !v.is_empty() {
                            name = v;
                        }
                    } else if let Some(v) = trimmed.strip_prefix("namespace:") {
                        let v = unquote_yaml(v);
                        if namespace.is_none() && !v.is_empty() {
                            namespace = Some(v);
                        }
                    }
                }
            }
            "data" | "binaryData" | "stringData" => {
                // Count ONLY direct keys of data/binaryData/stringData, ignoring block/quoted
                // scalar bodies and NEVER retaining any values (especially for Secret manifests).
                let entry_ind = *data_entry_indent.get_or_insert(ind);
                if ind == entry_ind && trimmed.contains(':') {
                    data_keys += 1;
                }
            }
            _ if doc_is_secret => {}
            "rules" => {
                let item_ind = *rules_item_indent.get_or_insert(ind);
                if ind == item_ind && (trimmed == "-" || trimmed.starts_with("- ")) {
                    rules_count += 1;
                }
            }
            "imagePullSecrets" => {
                let content = trimmed.strip_prefix("- ").unwrap_or(trimmed).trim_start();
                if let Some(val) = content.strip_prefix("name:") {
                    let v = unquote_yaml(val);
                    if !v.is_empty() {
                        sa_pull_secrets.push(v);
                    }
                }
            }
            "secrets" => {
                let item_ind = *sa_list_item_indent.get_or_insert(ind);
                if ind == item_ind && (trimmed == "-" || trimmed.starts_with("- ")) {
                    sa_secrets_count += 1;
                }
            }
            "roleRef" => {
                if ind == 2
                    && let Some(v) = trimmed.strip_prefix("name:")
                {
                    let v = unquote_yaml(v);
                    if !v.is_empty() {
                        role_ref_name = Some(v);
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
                if in_vct {
                    if ind < vct_indent || (ind == vct_indent && !trimmed.starts_with('-')) {
                        in_vct = false;
                        vct_item_indent = None;
                    } else if trimmed == "-" || trimmed.starts_with("- ") {
                        let item_ind = *vct_item_indent.get_or_insert(ind);
                        if ind == item_ind {
                            vct_count += 1;
                        }
                    }
                }
                let content = trimmed.strip_prefix("- ").unwrap_or(trimmed).trim_start();
                if let Some((k, v)) = content.split_once(':') {
                    let key = unquote_yaml(k);
                    let val = unquote_yaml(v);
                    match (kind, key, ind) {
                        (_, "replicas", 2) if !val.is_empty() => replicas = Some(val),
                        ("StatefulSet", "volumeClaimTemplates", 2) => {
                            in_vct = true;
                            vct_indent = ind;
                            vct_item_indent = None;
                        }
                        ("DaemonSet", "hostNetwork", _) if val.eq_ignore_ascii_case("true") => {
                            host_network = true;
                        }
                        ("Job", "completions", 2) if !val.is_empty() => {
                            job_completions = Some(val);
                        }
                        ("Job", "parallelism", 2) if !val.is_empty() => {
                            job_parallelism = Some(val);
                        }
                        ("CronJob", "schedule", 2) if !val.is_empty() => schedule = Some(val),
                        (
                            "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" | "Job"
                            | "CronJob" | "Pod",
                            "image",
                            _,
                        ) if !val.is_empty() => {
                            if !images.contains(&val) {
                                images.push(val);
                            }
                        }
                        ("Service", "type", 2) if !val.is_empty() => svc_type = Some(val),
                        ("Service", "port", _) if !val.is_empty() => {
                            if !svc_ports.contains(&val) {
                                svc_ports.push(val);
                            }
                        }
                        ("Ingress", "host", _) if !val.is_empty() => {
                            if !ingress_hosts.contains(&val) {
                                ingress_hosts.push(val);
                            }
                        }
                        ("HorizontalPodAutoscaler", "minReplicas", 2) if !val.is_empty() => {
                            hpa_min = Some(val);
                        }
                        ("HorizontalPodAutoscaler", "maxReplicas", 2) if !val.is_empty() => {
                            hpa_max = Some(val);
                        }
                        ("HorizontalPodAutoscaler", "scaleTargetRef", 2) => {
                            in_scale_target_ref = true;
                            scale_ref_indent = ind;
                        }
                        ("HorizontalPodAutoscaler", "name", _)
                            if in_scale_target_ref && !val.is_empty() =>
                        {
                            hpa_target = Some(val);
                        }
                        ("PersistentVolumeClaim", "storageClassName", 2) if !val.is_empty() => {
                            pvc_class = Some(val);
                        }
                        ("PersistentVolumeClaim", "storage", _) if !val.is_empty() => {
                            pvc_storage = Some(val);
                        }
                        ("CustomResourceDefinition", "group", 2) if !val.is_empty() => {
                            crd_group = Some(val);
                        }
                        ("CustomResourceDefinition", "names", 2) => {
                            in_crd_names = true;
                            crd_names_indent = ind;
                        }
                        ("CustomResourceDefinition", "kind", _)
                            if in_crd_names && !val.is_empty() =>
                        {
                            crd_kind = Some(val);
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
        name = "(unnamed)";
    }

    let summary = match kind {
        "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet" | "Job" | "CronJob" | "Pod" => {
            let mut out = String::new();
            let push_sep = |buf: &mut String| {
                if !buf.is_empty() {
                    buf.push_str(", ");
                }
            };
            if let Some(r) = replicas {
                let _ = write!(out, "{r} replicas");
            }
            if let Some(c) = job_completions {
                push_sep(&mut out);
                let _ = write!(out, "completions={c}");
            }
            if let Some(p) = job_parallelism
                && p != "1"
            {
                push_sep(&mut out);
                let _ = write!(out, "parallelism={p}");
            }
            if let Some(s) = schedule {
                push_sep(&mut out);
                let _ = write!(out, "schedule={s}");
            }
            if images.len() == 1 {
                push_sep(&mut out);
                let _ = write!(out, "image={}", images[0]);
            } else if images.len() > 1 {
                push_sep(&mut out);
                let extra = images.len().saturating_sub(2);
                let _ = write!(out, "images={}, {}", images[0], images[1]);
                if extra > 0 {
                    let _ = write!(out, ", +{extra}");
                }
            }
            if vct_count > 0 {
                push_sep(&mut out);
                let unit = if vct_count == 1 {
                    "pvc-template"
                } else {
                    "pvc-templates"
                };
                let _ = write!(out, "{vct_count} {unit}");
            }
            if host_network {
                push_sep(&mut out);
                out.push_str("hostNetwork");
            }
            out
        }
        "Service" => {
            let mut out = String::from(svc_type.unwrap_or("ClusterIP"));
            if !svc_ports.is_empty() {
                out.push(' ');
                for (i, port) in svc_ports.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push(':');
                    out.push_str(port);
                }
            }
            out
        }
        "Ingress" => {
            if ingress_hosts.is_empty() {
                String::new()
            } else {
                let mut out = String::from("hosts=");
                for (i, h) in ingress_hosts.iter().take(3).enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(h);
                }
                let extra = ingress_hosts.len().saturating_sub(3);
                if extra > 0 {
                    let _ = write!(out, ", +{extra}");
                }
                out
            }
        }
        "ConfigMap" => format!("{data_keys} keys"),
        "Secret" => {
            // Security: never emit Secret values, only type + key count.
            match secret_type {
                Some(ty) if ty != "Opaque" => format!("{ty}, {data_keys} keys"),
                _ => format!("{data_keys} keys"),
            }
        }
        "HorizontalPodAutoscaler" => {
            let mut out = String::new();
            if let Some(target) = hpa_target {
                let _ = write!(out, "-> {target}");
            }
            match (hpa_min, hpa_max) {
                (Some(mn), Some(mx)) => {
                    if !out.is_empty() {
                        out.push_str(", ");
                    }
                    let _ = write!(out, "{mn}..{mx} replicas");
                }
                (None, Some(mx)) => {
                    if !out.is_empty() {
                        out.push_str(", ");
                    }
                    let _ = write!(out, "max {mx} replicas");
                }
                _ => {}
            }
            out
        }
        "PersistentVolumeClaim" => {
            let mut out = String::new();
            if let Some(st) = pvc_storage {
                out.push_str(st);
            }
            if let Some(sc) = pvc_class {
                if !out.is_empty() {
                    out.push_str(", ");
                }
                let _ = write!(out, "class={sc}");
            }
            out
        }
        "ServiceAccount" => {
            let mut out = String::new();
            let push_sep = |buf: &mut String| {
                if !buf.is_empty() {
                    buf.push_str(", ");
                }
            };
            if let Some(am) = sa_automount {
                let _ = write!(out, "automount={am}");
            }
            if sa_pull_secrets.len() == 1 {
                push_sep(&mut out);
                let _ = write!(out, "pullSecret={}", sa_pull_secrets[0]);
            } else if sa_pull_secrets.len() > 1 {
                push_sep(&mut out);
                let _ = write!(out, "{} pullSecrets", sa_pull_secrets.len());
            }
            if sa_secrets_count > 0 {
                push_sep(&mut out);
                let unit = if sa_secrets_count == 1 {
                    "secret"
                } else {
                    "secrets"
                };
                let _ = write!(out, "{sa_secrets_count} {unit}");
            }
            out
        }
        "Role" | "ClusterRole" if rules_count > 0 => format!("{rules_count} rules"),
        "Role" | "ClusterRole" if has_aggregation_rule => "aggregated".to_owned(),
        "RoleBinding" | "ClusterRoleBinding" => {
            role_ref_name.map(|r| format!("-> {r}")).unwrap_or_default()
        }
        "CustomResourceDefinition" => match (crd_group, crd_kind) {
            (Some(g), Some(k)) => format!("{g}/{k}"),
            (Some(g), None) => g.to_owned(),
            (None, Some(k)) => k.to_owned(),
            (None, None) => String::new(),
        },
        _ => {
            if !api_version.is_empty() && api_version != "v1" {
                api_version.to_owned()
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
struct S3DirStats<'a> {
    count: usize,
    total_bytes: u64,
    extensions: BTreeMap<Cow<'a, str>, usize>,
    first_line: usize,
    last_line: usize,
    first_name: &'a str,
}

fn format_aws_s3_ls(lines: &[Cow<'_, str>], max_bytes: usize) -> Option<String> {
    let mut pre_dirs: Vec<(usize, &str)> = Vec::new();
    let mut objects: Vec<S3Object<'_>> = Vec::with_capacity(lines.len().min(4096));
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
        if trimmed.starts_with("PRE ")
            && let Some(cap) = S3_PRE.captures(line)
        {
            recognized += 1;
            pre_dirs.push((line_no, cap.get(1)?.as_str()));
            continue;
        }
        if trimmed.as_bytes().first().is_some_and(u8::is_ascii_digit) {
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

fn s3_prefix(path: &str, max_depth: Option<usize>) -> &str {
    let Some(last_slash) = path.rfind('/') else {
        return "./";
    };
    if last_slash == 0 {
        return "./";
    }
    match max_depth {
        None => &path[..=last_slash],
        Some(depth) => {
            if depth == 0 {
                return "./";
            }
            let mut seen = 0usize;
            for (idx, b) in path[..=last_slash].bytes().enumerate() {
                if b == b'/' {
                    seen += 1;
                    if seen == depth {
                        return if idx == 0 { "./" } else { &path[..=idx] };
                    }
                }
            }
            &path[..=last_slash]
        }
    }
}

fn s3_extension(path: &str) -> Cow<'_, str> {
    let file = path.rsplit('/').next().unwrap_or(path);
    if file.is_empty() {
        return Cow::Borrowed("(dir)");
    }
    for compound in [
        ".tar.gz",
        ".tar.zst",
        ".log.gz",
        ".json.gz",
        ".csv.gz",
        ".ndjson.gz",
    ] {
        if ends_with_ascii_icase(file, compound) {
            return Cow::Borrowed(compound);
        }
    }
    if let Some(dot) = file.rfind('.')
        && dot > 0
        && dot + 1 < file.len()
    {
        let ext = &file[dot..];
        if ext.len() <= 12 && ext[1..].bytes().all(|b| b.is_ascii_alphanumeric()) {
            if ext.bytes().any(|b| b.is_ascii_uppercase()) {
                return Cow::Owned(ext.to_ascii_lowercase());
            }
            return Cow::Borrowed(ext);
        }
    }
    Cow::Borrowed("(no ext)")
}

fn group_s3_objects<'a>(
    objects: &[S3Object<'a>],
    max_depth: Option<usize>,
) -> BTreeMap<&'a str, S3DirStats<'a>> {
    let mut map: BTreeMap<&'a str, S3DirStats<'a>> = BTreeMap::new();
    for obj in objects {
        let dir = s3_prefix(obj.path, max_depth);
        let ext = s3_extension(obj.path);
        let entry = map.entry(dir).or_insert_with(|| S3DirStats {
            first_line: obj.line_no,
            last_line: obj.line_no,
            first_name: obj.path.rsplit('/').next().unwrap_or(obj.path),
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
    grouped: &BTreeMap<&str, S3DirStats<'_>>,
    warnings: &[(usize, &str)],
    total_objects: usize,
    total_bytes: u64,
    tilde: &str,
    total_lines: usize,
    max_bytes: usize,
    allow_tail_fold: bool,
) -> Option<String> {
    let mut out = String::with_capacity(
        max_bytes.min(128 + pre_dirs.len() * 32 + grouped.len().saturating_mul(84)),
    );
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
    let mut line_buf = String::with_capacity(128);
    for &(n, dir) in pre_dirs {
        line_buf.clear();
        let _ = writeln!(line_buf, "PRE {dir} {}", range_tag(n, n));
        if out.len() + line_buf.len() + pre_reserve > max_bytes && allow_tail_fold {
            break;
        }
        out.push_str(&line_buf);
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
        let mut entries: Vec<(&&str, &S3DirStats<'_>)> = grouped.iter().collect();
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
            line_buf.clear();
            write_s3_dir_line(&mut line_buf, dir, stats, tilde);
            if out.len() + line_buf.len() + 72 > max_bytes {
                if !allow_tail_fold {
                    return None;
                }
                break;
            }
            out.push_str(&line_buf);
            shown += 1;
        }
        if shown < entries.len() {
            let rem = &entries[shown..];
            let rem_files: usize = rem.iter().map(|(_, s)| s.count).sum();
            let rem_bytes: u64 = rem.iter().map(|(_, s)| s.total_bytes).sum();
            let first = rem.iter().map(|(_, s)| s.first_line).min().unwrap_or(1);
            let last = rem.iter().map(|(_, s)| s.last_line).max().unwrap_or(first);
            line_buf.clear();
            let _ = writeln!(
                line_buf,
                "  ... +{} more prefix(es) ({rem_files} files, {tilde}{}) {}",
                rem.len(),
                human_bytes_bin(rem_bytes),
                range_tag(first, last)
            );
            if out.len() + line_buf.len() <= max_bytes {
                out.push_str(&line_buf);
            }
        }
    }

    (out.len() <= max_bytes).then_some(out)
}

fn write_s3_dir_line(dst: &mut String, dir: &str, stats: &S3DirStats<'_>, tilde: &str) {
    let noun = if stats.count == 1 { "file" } else { "files" };
    let _ = write!(
        dst,
        "  {dir}  {} {noun}  {tilde}{}  (",
        stats.count,
        human_bytes_bin(stats.total_bytes),
    );
    if stats.count == 1 && !stats.first_name.is_empty() {
        dst.push_str(stats.first_name);
    } else {
        let mut exts: Vec<(&str, usize)> = stats
            .extensions
            .iter()
            .map(|(k, &v)| (k.as_ref(), v))
            .collect();
        exts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        for (i, (ext, count)) in exts.iter().take(3).enumerate() {
            if i > 0 {
                dst.push_str(", ");
            }
            let _ = write!(dst, "{ext} ×{count}");
        }
        let ext_extra = exts.len().saturating_sub(3);
        if ext_extra > 0 {
            let _ = write!(dst, ", +{ext_extra} types");
        }
    }
    let _ = writeln!(dst, ") {}", range_tag(stats.first_line, stats.last_line));
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

struct CloudRow<'a> {
    line_no: usize,
    cells: Vec<Cow<'a, str>>,
}

struct ParsedCloudTable<'a> {
    headers: Vec<Cow<'a, str>>,
    rows: Vec<CloudRow<'a>>,
    warnings: Vec<(usize, &'a str)>,
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

    let mut unhealthy: Vec<&CloudRow<'_>> = Vec::new();
    let mut healthy: Vec<&CloudRow<'_>> = Vec::with_capacity(parsed.rows.len());

    for row in &parsed.rows {
        if is_row_unhealthy(row, &status_cols, reason_col, table_kind) {
            unhealthy.push(row);
        } else {
            healthy.push(row);
        }
    }

    let mut out = String::with_capacity(max_bytes.min(256 + unhealthy.len() * 112 + 256));
    let _ = writeln!(
        out,
        "{label}: {} row(s) ({} unhealthy/attention, {} healthy) {}",
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
        let mut line_buf = String::with_capacity(160);
        for row in &unhealthy {
            line_buf.clear();
            line_buf.push_str("  ");
            write_pinned_row(&mut line_buf, row, &upper_headers, table_kind);
            let _ = writeln!(line_buf, " {}", range_tag(row.line_no, row.line_no));
            if out.len() + line_buf.len() + healthy_reserve > max_bytes {
                break;
            }
            out.push_str(&line_buf);
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
        let mut candidate = String::with_capacity(detailed_groups.len() * 80 + 24);
        let _ = writeln!(candidate, "Healthy ({}):", healthy.len());
        for (key, stats) in &detailed_groups {
            candidate.push_str("  ");
            write_healthy_group(&mut candidate, key, stats);
            candidate.push('\n');
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
            let mut line_buf = String::with_capacity(128);
            for (key, stats) in &items {
                line_buf.clear();
                line_buf.push_str("  ");
                write_healthy_group(&mut line_buf, key, stats);
                line_buf.push('\n');
                if out.len() + line_buf.len() + 64 > max_bytes {
                    break;
                }
                out.push_str(&line_buf);
                shown += 1;
            }
            if shown < items.len() {
                let rem = &items[shown..];
                let rem_rows: usize = rem.iter().map(|(_, s)| s.count).sum();
                let first = rem.iter().map(|(_, s)| s.first_line).min().unwrap_or(1);
                let last = rem.iter().map(|(_, s)| s.last_line).max().unwrap_or(first);
                line_buf.clear();
                let _ = writeln!(
                    line_buf,
                    "  ... +{} more healthy group(s) ({rem_rows} rows) {}",
                    rem.len(),
                    range_tag(first, last)
                );
                if out.len() + line_buf.len() <= max_bytes {
                    out.push_str(&line_buf);
                }
            }
        }
    }

    (out.len() <= max_bytes).then_some(out)
}

struct HealthyGroupStats<'a> {
    count: usize,
    first_line: usize,
    last_line: usize,
    samples: Vec<&'a str>,
}

fn build_healthy_groups<'a>(
    rows: &[&'a CloudRow<'a>],
    status_cols: &[usize],
    loc_col: Option<usize>,
    type_col: Option<usize>,
    name_col: Option<usize>,
    detailed: bool,
) -> BTreeMap<String, HealthyGroupStats<'a>> {
    let mut map: BTreeMap<String, HealthyGroupStats<'a>> = BTreeMap::new();
    let mut key_buf = String::with_capacity(64);
    for row in rows {
        key_buf.clear();
        write_status_label(&mut key_buf, row, status_cols);
        if detailed {
            let loc = loc_col
                .and_then(|c| row.cells.get(c))
                .map(Cow::as_ref)
                .filter(|v| !v.is_empty());
            let ty = type_col
                .and_then(|c| row.cells.get(c))
                .map(Cow::as_ref)
                .filter(|v| !v.is_empty());
            match (loc, ty) {
                (Some(l), Some(t)) => {
                    let _ = write!(key_buf, " ({l}, {t})");
                }
                (Some(v), None) | (None, Some(v)) => {
                    let _ = write!(key_buf, " ({v})");
                }
                (None, None) => {}
            }
        }
        let sample = name_col
            .and_then(|c| row.cells.get(c))
            .map(Cow::as_ref)
            .filter(|v| !v.is_empty())
            .or_else(|| row.cells.iter().map(Cow::as_ref).find(|v| !v.is_empty()));

        if let Some(entry) = map.get_mut(key_buf.as_str()) {
            entry.count += 1;
            entry.first_line = entry.first_line.min(row.line_no);
            entry.last_line = entry.last_line.max(row.line_no);
            if let Some(s) = sample
                && entry.samples.len() < 2
                && !entry.samples.contains(&s)
            {
                entry.samples.push(s);
            }
        } else {
            let mut samples = Vec::with_capacity(2);
            if let Some(s) = sample {
                samples.push(s);
            }
            map.insert(
                key_buf.clone(),
                HealthyGroupStats {
                    count: 1,
                    first_line: row.line_no,
                    last_line: row.line_no,
                    samples,
                },
            );
        }
    }
    map
}

fn write_healthy_group(dst: &mut String, key: &str, stats: &HealthyGroupStats<'_>) {
    let _ = write!(dst, "{key} ×{}", stats.count);
    if !stats.samples.is_empty() {
        if stats.count == 1 {
            let _ = write!(dst, " ({})", stats.samples[0]);
        } else {
            dst.push_str(" (e.g. ");
            for (i, s) in stats.samples.iter().enumerate() {
                if i > 0 {
                    dst.push_str(", ");
                }
                dst.push_str(s);
            }
            dst.push(')');
        }
    }
    let _ = write!(dst, " {}", range_tag(stats.first_line, stats.last_line));
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

fn write_status_label(dst: &mut String, row: &CloudRow<'_>, status_cols: &[usize]) {
    let mut count = 0usize;
    let mut first = "";
    for &c in status_cols {
        if let Some(v) = row.cells.get(c)
            && !v.is_empty()
        {
            if count == 0 {
                first = v.as_ref();
            } else {
                if count == 1 {
                    dst.push_str(first);
                }
                dst.push_str(" / ");
                dst.push_str(v.as_ref());
            }
            count += 1;
        }
    }
    if count == 0 {
        dst.push_str("OK");
    } else if count == 1 {
        if first == "✔" || first == "+" {
            dst.push_str("READY");
        } else {
            dst.push_str(first);
        }
    }
}

fn is_healthy_cloud_status(val: &str) -> bool {
    const HEALTHY: &[&str] = &[
        "RUNNING",
        "ACTIVE",
        "READY",
        "SUCCEEDED",
        "SUCCESS",
        "OK",
        "AVAILABLE",
        "ENABLED",
        "HEALTHY",
        "VM RUNNING",
        "ONLINE",
        "TRUE",
        "✔",
        "+",
    ];
    HEALTHY.iter().any(|h| val.eq_ignore_ascii_case(h))
}

fn is_row_unhealthy(
    row: &CloudRow<'_>,
    status_cols: &[usize],
    reason_col: Option<usize>,
    kind: CloudTableKind,
) -> bool {
    if let Some(rc) = reason_col
        && let Some(reason) = row.cells.get(rc)
    {
        let r = reason.as_ref();
        if contains_ascii_icase(r, "FAIL")
            || contains_ascii_icase(r, "ERROR")
            || contains_ascii_icase(r, "DENIED")
            || contains_ascii_icase(r, "EXCEEDED")
            || contains_ascii_icase(r, "ROLLBACK")
            || contains_ascii_icase(r, "CANCELLED")
            || contains_ascii_icase(r, "CANCELED")
            || contains_ascii_icase(r, "TIMEOUT")
        {
            return true;
        }
    }

    for &col in status_cols {
        let val = row.cells.get(col).map_or("", Cow::as_ref).trim();
        if val.is_empty() {
            continue;
        }
        match kind {
            CloudTableKind::AwsStackEvents => {
                if contains_ascii_icase(val, "FAILED")
                    || contains_ascii_icase(val, "ROLLBACK")
                    || contains_ascii_icase(val, "ERROR")
                    || contains_ascii_icase(val, "ABORT")
                    || contains_ascii_icase(val, "TIMEOUT")
                    || contains_ascii_icase(val, "CANCEL")
                {
                    return true;
                }
            }
            CloudTableKind::GcloudList | CloudTableKind::AzList => {
                if !is_healthy_cloud_status(val) {
                    return true;
                }
            }
        }
    }
    false
}

fn write_pinned_row(
    dst: &mut String,
    row: &CloudRow<'_>,
    upper_headers: &[String],
    kind: CloudTableKind,
) {
    if kind == CloudTableKind::AwsStackEvents {
        // Show ResourceStatus, LogicalResourceId, ResourceType, full Reason, and Timestamp!
        let mut wrote = false;
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
            if let Some(c) = find_col(upper_headers, names)
                && let Some(v) = row.cells.get(c)
                && !v.is_empty()
            {
                if wrote {
                    dst.push_str("  ");
                }
                dst.push_str(&collapse_spaces(v));
                wrote = true;
            }
        }
        if wrote {
            return;
        }
    }
    let mut wrote = false;
    for c in &row.cells {
        let clean = collapse_spaces(c);
        if !clean.is_empty() {
            if wrote {
                dst.push_str("  ");
            }
            dst.push_str(&clean);
            wrote = true;
        }
    }
}

/// Parse AWS CLI `--output table` (`+---+`, `| ... |`).
fn parse_aws_bordered_table<'a>(lines: &'a [Cow<'a, str>]) -> Option<ParsedCloudTable<'a>> {
    let mut warnings = Vec::new();
    let mut headers: Vec<Cow<'a, str>> = Vec::new();
    let mut rows: Vec<CloudRow<'a>> = Vec::new();
    let mut saw_border = false;

    for (idx, line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.as_ref().trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_aws_border_line(trimmed) {
            saw_border = true;
            continue;
        }
        if trimmed.starts_with('|') && trimmed.ends_with('|') {
            let cells: Vec<Cow<'a, str>> = trimmed[1..trimmed.len() - 1]
                .split('|')
                .map(|c| Cow::Borrowed(c.trim()))
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
            warnings.push((line_no, trimmed));
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
fn parse_aws_stack_events_text<'a>(lines: &'a [Cow<'a, str>]) -> Option<ParsedCloudTable<'a>> {
    let mut warnings = Vec::new();
    let mut rows: Vec<CloudRow<'a>> = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let line_no = idx + 1;
        let raw = line.as_ref();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = raw.strip_prefix("STACKEVENTS\t") {
            let fields: Vec<&str> = rest.split('\t').collect();
            if fields.len() < 5 {
                continue;
            }
            // Locate the CloudFormation ResourceStatus token (e.g. CREATE_COMPLETE, UPDATE_FAILED, ROLLBACK_IN_PROGRESS).
            let status_idx = fields.iter().position(|f| is_cfn_status_token(f));
            let Some(si) = status_idx else {
                continue;
            };
            let logical_id = Cow::Borrowed(fields.get(1).copied().unwrap_or("").trim());
            let status = Cow::Borrowed(fields[si].trim());
            let type_idx = fields[si + 1..]
                .iter()
                .position(|f| f.contains("::"))
                .map(|off| si + 1 + off);
            let resource_type = Cow::Borrowed(
                type_idx
                    .and_then(|ti| fields.get(ti))
                    .copied()
                    .unwrap_or("")
                    .trim(),
            );
            // Reason sits between status and resource_type when present.
            let reason = match type_idx {
                Some(ti) if ti == si + 2 => Cow::Borrowed(fields[si + 1].trim()),
                Some(ti) if ti > si + 2 => Cow::Owned(
                    fields[si + 1..ti]
                        .iter()
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                _ => Cow::Borrowed(""),
            };
            let timestamp = Cow::Borrowed(
                fields
                    .iter()
                    .rev()
                    .find(|f| f.contains('T') && f.len() >= 19 && f.as_bytes()[4] == b'-')
                    .copied()
                    .unwrap_or("")
                    .trim(),
            );
            rows.push(CloudRow {
                line_no,
                cells: vec![timestamp, status, logical_id, resource_type, reason],
            });
            continue;
        }
        if is_warning_or_error(raw) {
            warnings.push((line_no, trimmed));
        }
    }

    if rows.is_empty() {
        return None;
    }
    Some(ParsedCloudTable {
        headers: vec![
            Cow::Borrowed("Timestamp"),
            Cow::Borrowed("ResourceStatus"),
            Cow::Borrowed("LogicalResourceId"),
            Cow::Borrowed("ResourceType"),
            Cow::Borrowed("ResourceStatusReason"),
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
fn parse_az_dashed_table<'a>(lines: &'a [Cow<'a, str>]) -> Option<ParsedCloudTable<'a>> {
    let mut warnings = Vec::new();
    let mut header_idx = None;
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.as_ref().trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_warning_or_error(line) {
            warnings.push((idx + 1, trimmed));
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
    let mut rows = Vec::with_capacity(lines.len().saturating_sub(h_idx + 2));
    for (offset, line) in lines[h_idx + 2..].iter().enumerate() {
        let line_no = h_idx + 3 + offset;
        let raw = line.as_ref();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_warning_or_error(raw) {
            warnings.push((line_no, trimmed));
            continue;
        }
        let cells = slice_by_char_spans(raw, &spans);
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
fn parse_columnar_table<'a>(lines: &'a [Cow<'a, str>]) -> Option<ParsedCloudTable<'a>> {
    let mut warnings = Vec::new();
    let mut header_idx = None;
    for (idx, line) in lines.iter().enumerate() {
        let raw = line.as_ref();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_warning_or_error(raw) {
            warnings.push((idx + 1, trimmed));
            continue;
        }
        if looks_like_table_header(raw) {
            header_idx = Some(idx);
            break;
        }
        return None;
    }
    let h_idx = header_idx?;
    let header_line = lines[h_idx].as_ref();

    let mut headers: Vec<Cow<'a, str>> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    let header_is_ascii = header_line.is_ascii();
    for m in TABLE_HEADER_COL.find_iter(header_line) {
        let char_start = if header_is_ascii {
            m.start()
        } else {
            header_line[..m.start()].chars().count()
        };
        headers.push(Cow::Borrowed(m.as_str()));
        starts.push(char_start);
    }
    if headers.len() < 2 {
        return None;
    }
    // `gcloud run services list` leaves a leading glyph column (`✔` / `X`) with blank header.
    if starts[0] >= 2 {
        headers.insert(0, Cow::Borrowed("READY"));
        starts.insert(0, 0);
    }

    let mut spans = Vec::with_capacity(starts.len());
    for i in 0..starts.len() {
        spans.push((starts[i], starts.get(i + 1).copied()));
    }

    let mut rows = Vec::with_capacity(lines.len().saturating_sub(h_idx + 1));
    for (offset, line) in lines[h_idx + 1..].iter().enumerate() {
        let line_no = h_idx + 2 + offset;
        let raw = line.as_ref();
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with("Listed ") {
            continue;
        }
        if is_warning_or_error(raw) {
            warnings.push((line_no, trimmed));
            continue;
        }
        let cells = slice_by_char_spans(raw, &spans);
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
    let mut count = 0usize;
    for m in TABLE_HEADER_COL.find_iter(line) {
        let c = m.as_str();
        let valid = c.bytes().any(|b| b.is_ascii_uppercase())
            && c.bytes().all(|b| {
                b.is_ascii_uppercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'_' | b'-' | b' ' | b'/' | b'(' | b')')
            });
        if !valid {
            return false;
        }
        count += 1;
    }
    count >= 2
}

fn slice_by_char_spans<'a>(line: &'a str, spans: &[(usize, Option<usize>)]) -> Vec<Cow<'a, str>> {
    let mut cells = Vec::with_capacity(spans.len());
    if line.is_ascii() {
        let len = line.len();
        for &(start, end) in spans {
            let b_start = start.min(len);
            let b_end = end.map_or(len, |e| e.min(len));
            if b_start >= b_end {
                cells.push(Cow::Borrowed(""));
            } else {
                let cell = line[b_start..b_end].trim();
                if cell.len() > 512 {
                    cells.push(Cow::Owned(format!("{}...", clip(cell, 509))));
                } else {
                    cells.push(Cow::Borrowed(cell));
                }
            }
        }
        return cells;
    }

    let char_bytes: Vec<usize> = line.char_indices().map(|(b, _)| b).collect();
    let total_chars = char_bytes.len();
    let byte_at = |char_idx: usize| -> usize {
        if char_idx >= total_chars {
            line.len()
        } else {
            char_bytes[char_idx]
        }
    };

    for &(start, end) in spans {
        let b_start = byte_at(start);
        let b_end = end.map_or(line.len(), byte_at);
        if b_start >= b_end {
            cells.push(Cow::Borrowed(""));
        } else {
            let cell = line[b_start..b_end].trim();
            if cell.len() > 512 {
                cells.push(Cow::Owned(format!("{}...", clip(cell, 509))));
            } else {
                cells.push(Cow::Borrowed(cell));
            }
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

    #[test]
    fn helm_template_preserves_notes_warnings_errors_summarizes_k8s_kinds_and_hides_secrets() {
        let secret_val_a = ["c3VwZXIt", "c2VjcmV0", "LXBhc3M="].concat();
        let secret_val_b = ["cGxhaW50ZXh0", "LXNlY3JldA=="].concat();
        let secret_val_c = ["Zmxvdy1z", "ZWNyZXQ="].concat();
        let raw = format!(
            "\x1b[33mWARNING: chart uses deprecated API version\x1b[0m\n\
---
# Source: stack/templates/statefulset.yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: pg-primary
  namespace: prod
spec:
  replicas: 3
  serviceName: pg-headless
  template:
    spec:
      containers:
        - name: postgres
          image: postgres:16.4
          env:
            - name: PGDATA
              value: /var/lib/postgresql/data/pgdata
  volumeClaimTemplates:
    - metadata:
        name: pgdata
      spec:
        accessModes: [\"ReadWriteOnce\"]
        resources:
          requests:
            storage: 50Gi
---
# Source: stack/templates/daemonset.yaml
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: node-agent
  namespace: prod
spec:
  template:
    spec:
      hostNetwork: true
      containers:
        - name: agent
          image: ghcr.io/acme/node-agent:2.1.0
---
# Source: stack/templates/job.yaml
apiVersion: batch/v1
kind: Job
metadata:
  name: db-migrate
  namespace: prod
spec:
  completions: 2
  parallelism: 2
  template:
    spec:
      containers:
        - name: migrate
          image: ghcr.io/acme/migrate:1.9.0
---
# Source: stack/templates/clusterrole.yaml
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: stack-reader
rules:
- apiGroups: [\"\"]
  resources: [\"pods\", \"services\"]
  verbs: [\"get\", \"list\", \"watch\"]
- apiGroups: [\"apps\"]
  resources: [\"deployments\", \"statefulsets\"]
  verbs: [\"get\", \"list\"]
---
# Source: stack/templates/secret-block.yaml
apiVersion: v1
kind: Secret
metadata:
  name: app-creds
  namespace: prod
type: kubernetes.io/tls
data:
  tls.crt: {secret_val_a}
stringData:
  api_token: |
    {secret_val_b}
    WARNING: must never leak secret block text
---
# Source: stack/templates/secret-flow.yaml
apiVersion: v1
kind: Secret
metadata:
  name: inline-creds
  namespace: prod
data: {{ KEY_A: \"{secret_val_c}\", KEY_B: \"{secret_val_a}\" }}
---
# Source: stack/templates/serviceaccount.yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: stack-sa
  namespace: prod
automountServiceAccountToken: false
imagePullSecrets:
- name: ghcr-pull
Error: non-fatal template render hint
NOTES:
1. Verify rollout status:
  kubectl -n prod rollout status statefulset/pg-primary
2. Inspect logs if pods fail to start.
"
        );

        let view = format(
            &["helm", "template", "stack", "./charts/stack"],
            &raw,
            4_096,
        )
        .expect("helm template compacted");

        // Verbatim WARNING:, Error:, and multi-line NOTES: with exact archive pointers.
        assert!(
            view.contains("WARNING: chart uses deprecated API version [stdout:1]"),
            "{view}"
        );
        assert!(
            view.contains("Error: non-fatal template render hint [stdout:"),
            "{view}"
        );
        assert!(view.contains("NOTES: [stdout:"), "{view}");
        assert!(
            view.contains(
                "1. Verify rollout status:\n  kubectl -n prod rollout status statefulset/pg-primary"
            ),
            "{view}"
        );
        assert!(
            view.contains("2. Inspect logs if pods fail to start."),
            "{view}"
        );

        // Concise summaries for StatefulSet / DaemonSet / Job / ClusterRole / ServiceAccount.
        assert!(
            view.contains("StatefulSet/pg-primary (3 replicas, image=postgres:16.4, 1 pvc-template) [stdout:2-27]"),
            "{view}"
        );
        assert!(
            view.contains("DaemonSet/node-agent (image=ghcr.io/acme/node-agent:2.1.0, hostNetwork) [stdout:28-41]"),
            "{view}"
        );
        assert!(
            view.contains("Job/db-migrate (completions=2, parallelism=2, image=ghcr.io/acme/migrate:1.9.0) [stdout:42-56]"),
            "{view}"
        );
        assert!(
            view.contains("ClusterRole/stack-reader (2 rules) [stdout:57-69]"),
            "{view}"
        );
        assert!(
            view.contains(
                "ServiceAccount/stack-sa (automount=false, pullSecret=ghcr-pull) [stdout:92-101]"
            ),
            "{view}"
        );

        // Secret safety: key counts only, ZERO leaked values from data, stringData, or flow mappings.
        assert!(
            view.contains("Secret/app-creds (kubernetes.io/tls, 2 keys) [stdout:70-83]"),
            "{view}"
        );
        assert!(
            view.contains("Secret/inline-creds (2 keys) [stdout:84-91]"),
            "{view}"
        );
        assert!(
            view.contains("Error: non-fatal template render hint [stdout:102]"),
            "{view}"
        );
        assert!(view.contains("NOTES: [stdout:103-106]"), "{view}");
        assert!(!view.contains(&secret_val_a), "leaked secret A:\n{view}");
        assert!(!view.contains(&secret_val_b), "leaked secret B:\n{view}");
        assert!(!view.contains(&secret_val_c), "leaked secret C:\n{view}");
        assert!(
            !view.contains("must never leak secret block text"),
            "leaked secret warning line:\n{view}"
        );
        assert!(!view.contains('\x1b'), "ANSI leaked:\n{view}");
    }

    #[test]
    fn helm_secret_multiline_quoted_scalar_and_indented_doc_sep_never_leak() {
        let leak_warn = "TOP_SECRET_WARN_TOKEN_991";
        let leak_type = "TOP_SECRET_TYPE_OVERRIDE_992";
        let leak_pass = "TOP_SECRET_DB_PASSWORD_993";
        let leak_img = "registry.evil/leaked-secret-img:994";
        let leak_single = "TOP_SECRET_SINGLE_QUOTED_995";
        let leak_block = "TOP_SECRET_AFTER_INDENTED_SEP_996";
        let raw = format!(
            "\
---
# Source: app/templates/secret-quoted.yaml
apiVersion: v1
kind: Secret
metadata:
  name: quoted-creds
  namespace: prod
type: kubernetes.io/tls
stringData:
  pem: \"-----BEGIN PRIVATE KEY-----
WARNING: {leak_warn}
type: {leak_type}
NOTES:
  db_password: {leak_pass}
kind: Deployment
spec:
  template:
    spec:
      containers:
        - image: {leak_img}
\\\"escaped-quote-stays-inside
\\\\double-backslash-closes\\\\\"
  single_cert: 'line1 ''escaped''
WARNING: {leak_single}
NOTES:
  nested_secret: {leak_single}
kind: Deployment
'
  tls.crt: |
    -----BEGIN CERTIFICATE-----
    ---
    {leak_block}
    WARNING: {leak_block}
    -----END CERTIFICATE-----
---
# Source: app/templates/deploy.yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  namespace: prod
spec:
  replicas: 2
  template:
    spec:
      containers:
        - name: web
          image: ghcr.io/acme/web:1.0.0
"
        );
        let view = format(&["helm", "template", "app", "./charts/app"], &raw, 4_096)
            .expect("helm template compacted");

        // Secret keeps its real kind/type, exact 3 keys, and full line span [stdout:1-34].
        assert!(
            view.contains("Secret/quoted-creds (kubernetes.io/tls, 3 keys) [stdout:1-34]"),
            "{view}"
        );
        assert!(
            view.contains(
                "Deployment/web (2 replicas, image=ghcr.io/acme/web:1.0.0) [stdout:35-48]"
            ),
            "{view}"
        );
        // Zero leaked secret substrings, zero forged warnings/notes/deployments.
        for forbidden in [
            leak_warn,
            leak_type,
            leak_pass,
            leak_img,
            leak_single,
            leak_block,
            "WARNING:",
            "NOTES:",
            "db_password",
            "Deployment ×2",
        ] {
            assert!(!view.contains(forbidden), "leaked {forbidden:?}:\n{view}");
        }
    }

    #[test]
    fn helm_secret_all_seven_yaml_leak_vectors_emit_zero_secret_substrings() {
        let s1 = "SECRET_LEAK_BLOCK_COMMENT_101";
        let s2 = "SECRET_LEAK_MID_PLAIN_QUOTE_102";
        let s3 = "SECRET_LEAK_DATA_BEFORE_KIND_103";
        let s4 = "SECRET_LEAK_FLOW_BEFORE_KIND_104";
        let s5 = "SECRET_LEAK_FLOW_TYPE_FORGE_105";
        let s6 = "SECRET_LEAK_KIND_COMMENT_106";
        let s7 = "SECRET_LEAK_SECRETLIST_107";
        let s8 = "SECRET_LEAK_LIST_WITH_SECRET_108";
        let s9 = "SECRET_LEAK_TAGGED_BLOCK_109";

        let raw = format!(
            "\
---
# 1. Block scalar with trailing `# comment` (and YAML tags/anchors)
apiVersion: v1
kind: Secret
metadata:
  name: sec-block-comment
  namespace: prod
stringData:
  cert: | # tls cert
    -----BEGIN PRIVATE KEY-----
    {s1}
    WARNING: {s1}
    type: {s1}
    -----END PRIVATE KEY-----
  tagged: &a !!str |- # anchored tagged scalar
    {s9}
    WARNING: {s9}
---
# 2. Mid-plain-scalar quote before `kind: Secret`
apiVersion: v1
metadata:
  name: sec-plain-quote
  namespace: prod
  annotations:
    note: hello \"world
kind: Secret
stringData:
  token: \"{s2}\"
WARNING: {s2}
NOTES:
  {s2}
---
# 3. `data:` before `kind: Secret` (single-line) with WARNING/NOTES before kind
apiVersion: v1
data:
  api_key: \"{s3}\"
WARNING: {s3}
NOTES:
  {s3}
spec:
  template:
    spec:
      containers:
        - image: registry.evil/{s3}:1.0
kind: Secret
metadata:
  name: sec-data-before-kind
  namespace: prod
---
# 4. `data:` before `kind: Secret` (multiline flow)
apiVersion: v1
data: {{
WARNING: {s4},
NOTES: {s4},
type: {s4}
}}
kind: Secret
metadata:
  name: sec-flow-before-kind
  namespace: prod
---
# 5. Multiline flow `stringData` forging col-0 `type:`
apiVersion: v1
kind: Secret
metadata:
  name: sec-flow-type
  namespace: prod
stringData: {{
type: {s5}
}}
---
# 6. `kind: Secret # managed` inline comment
apiVersion: v1
kind: Secret # managed
metadata:
  name: sec-kind-comment # name comment
  namespace: prod # ns comment
stringData:
  pass: \"{s6}\"
WARNING: {s6}
NOTES:
  {s6}
---
# 7a. `kind: SecretList`
apiVersion: v1
kind: SecretList
items:
- apiVersion: v1
  kind: Secret
  metadata:
    name: inner-secret
  stringData:
    pw: \"{s7}\"
WARNING: {s7}
NOTES:
  {s7}
---
# 7b. `kind: List` containing a `Secret` (block & inline flow)
apiVersion: v1
kind: List
items:
- apiVersion: v1
  kind: ConfigMap
  metadata:
    name: benign-cm
- {{ apiVersion: v1, kind: Secret, metadata: {{ name: flow-sec }}, stringData: {{ pw: \"{s8}\" }} }}
WARNING: {s8}
NOTES:
  {s8}
---
# Benign Deployment alongside all secrets must still render cleanly
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api
  namespace: prod
spec:
  replicas: 2
  template:
    spec:
      containers:
        - name: api
          image: ghcr.io/acme/api:3.0.0
"
        );

        let view = format(&["helm", "template", "app", "./charts/app"], &raw, 8_192)
            .expect("helm template compacted");

        assert!(view.contains("Secret/sec-block-comment (2 keys)"), "{view}");
        assert!(view.contains("Secret/sec-plain-quote (1 keys)"), "{view}");
        assert!(
            view.contains("Secret/sec-data-before-kind (1 keys)"),
            "{view}"
        );
        assert!(
            view.contains("Secret/sec-flow-before-kind (3 keys)"),
            "{view}"
        );
        assert!(view.contains("Secret/sec-flow-type (1 keys)"), "{view}");
        assert!(view.contains("Secret/sec-kind-comment (1 keys)"), "{view}");
        assert!(view.contains("SecretList/(unnamed)"), "{view}");
        assert!(view.contains("List/(unnamed)"), "{view}");
        assert!(
            view.contains("Deployment/api (2 replicas, image=ghcr.io/acme/api:3.0.0)"),
            "{view}"
        );

        for forbidden in [s1, s2, s3, s4, s5, s6, s7, s8, s9, "WARNING:", "NOTES:"] {
            assert!(!view.contains(forbidden), "leaked {forbidden:?}:\n{view}");
        }
    }
}
