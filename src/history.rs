use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, BufReader, ErrorKind, Write};
use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::capture::{self, Run, Stream};
use crate::context::Report;
use crate::mcp::MAX_LIMIT;
use crate::workspace::{Workspace, lock_state, write_json};

pub fn now_ms() -> u64 {
    millis(SystemTime::now())
}

/// Programs whose second word names a subcommand rather than an argument.
const SUBCOMMANDS: &[&str] = &[
    "bazel",
    "brew",
    "bun",
    "bundle",
    "cabal",
    "cargo",
    "composer",
    "conda",
    "dart",
    "deno",
    "docker",
    "dotnet",
    "flutter",
    "gh",
    "git",
    "go",
    "golangci-lint",
    "gradle",
    "gradlew",
    "hatch",
    "helm",
    "justless",
    "kubectl",
    "make",
    "mix",
    "mvn",
    "mvnw",
    "nix",
    "npm",
    "pdm",
    "pip",
    "pip3",
    "pixi",
    "pnpm",
    "podman",
    "poetry",
    "rustup",
    "rye",
    "ruff",
    "stack",
    "swift",
    "terraform",
    "tofu",
    "uv",
    "vitest",
    "yarn",
    "zig",
];

/// Known Kubernetes resource nouns kept as a third word of `kubectl get …`
/// labels so distinct resource inventories (`kubectl get events`, `svc`, `pods`,
/// …) stay separate without ever exposing arbitrary names, namespaces or secrets.
const KUBECTL_RESOURCES: &[&str] = &[
    "pods",
    "po",
    "deployments",
    "deploy",
    "statefulsets",
    "sts",
    "daemonsets",
    "ds",
    "replicasets",
    "rs",
    "jobs",
    "nodes",
    "no",
    "events",
    "ev",
    "svc",
    "services",
    "ing",
    "ingresses",
    "pvc",
    "pvcs",
    "cj",
    "cronjobs",
];

/// A command label without arguments, such as `cargo test`, `make` or `mysql`.
/// Only programs known to take subcommands keep their second word, so host,
/// database and file names never appear in a label.
pub fn label(words: &[&str]) -> String {
    let owned = crate::wrappers::hook_effective(words);
    let refs: Vec<&str> = match &owned {
        Some(unwrapped) => unwrapped.iter().map(String::as_str).collect(),
        None => words.to_vec(),
    };
    let words = crate::filters::unwrap_tool(&refs);
    let Some((program, rest)) = words.split_first() else {
        return "other".into();
    };
    let program = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    if program.is_empty()
        || program.starts_with(['-', '.'])
        || !program
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+'))
    {
        return "other".into();
    }
    if matches!(program, "python" | "python3")
        && let ["-m", module @ ("pytest" | "unittest"), ..] = rest
    {
        return format!("{program} -m {module}");
    }
    let rest = if program == "git" {
        crate::filters::git_display_subcommand(rest)
    } else {
        rest
    };
    if program == "kubectl"
        && let ["get", resource, ..] = rest
        && KUBECTL_RESOURCES.contains(resource)
    {
        return format!("kubectl get {resource}");
    }
    match rest.first() {
        Some(sub)
            if SUBCOMMANDS.contains(&program)
                && sub.starts_with(|c: char| c.is_ascii_lowercase())
                && sub.chars().all(|c| c.is_ascii_lowercase() || c == '-') =>
        {
            format!("{program} {sub}")
        }
        _ => program.to_owned(),
    }
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn age(ms: u64) -> String {
    let seconds = now_ms().saturating_sub(ms) / 1000;
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

fn skipped(id: &str, error: &anyhow::Error) {
    eprintln!("justless: skipped run {id}: {error:#}");
}

/// A run record is written once, when its command finishes. Records modified
/// this long before a `since_ms` window are skipped without being read.
const RECORD_SLACK_MS: u64 = 60_000;

fn valid_run_id(id: &str) -> bool {
    id.strip_prefix("run-").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

/// Largest `record.json` kept in memory while sorting `records`. Larger files
/// stream their completion stamp and re-stream on demand so `resume` (`.take(5)`)
/// never buffers huge diagnostic records for runs it will not read.
const INLINE_RECORD_CAP: u64 = 64 * 1024;

/// Recorded runs ordered by completion time, newest first unless `oldest`.
/// A record that cannot be read is skipped with a warning naming its run, so
/// the others stay readable. The iterator never yields an error.
pub fn records(
    workspace: &Workspace,
    oldest: bool,
) -> Result<impl Iterator<Item = Result<Run>> + '_> {
    /// The ordering field, read without decoding the whole record.
    #[derive(Deserialize)]
    struct Stamp {
        #[serde(default)]
        completed_ms: u64,
    }
    enum Source {
        /// Raw `record.json` bytes already read from disk, decoded lazily when
        /// the caller consumes this entry so small files are opened at most once.
        Json(Vec<u8>),
        /// Oversized `record.json` whose stamp was streamed; decoded on demand.
        OnDisk,
        /// Unfinished run directory that has `stdout` but no `record.json`.
        Unfinished,
    }
    let path = workspace.state.join("runs");
    let mut entries = Vec::new();
    if path.exists() {
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let directory = entry.path();
            let record = directory.join("record.json");
            let file = match fs::File::open(&record) {
                Ok(file) => file,
                // A run that Justless could not finish has streams but may lack a
                // record. It sorts by the last write to its output.
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    if let Ok(modified) =
                        fs::metadata(directory.join("stdout")).and_then(|meta| meta.modified())
                    {
                        entries.push((millis(modified), id, Source::Unfinished));
                    }
                    continue;
                }
                Err(e) => {
                    skipped(&id, &anyhow::Error::new(e).context("read run record"));
                    continue;
                }
            };
            let meta = file.metadata().ok();
            let fallback_ms = || {
                meta.as_ref()
                    .and_then(|m| m.modified().ok())
                    .map_or(0, millis)
            };
            if meta.as_ref().is_some_and(|m| m.len() <= INLINE_RECORD_CAP) {
                let mut bytes = Vec::with_capacity(meta.as_ref().map_or(0, |m| m.len() as usize));
                if let Err(e) = io::Read::read_to_end(&mut &file, &mut bytes) {
                    skipped(&id, &anyhow::Error::new(e).context("read run record"));
                    continue;
                }
                let completed =
                    match serde_json::from_slice::<Stamp>(&bytes).context("decode run record") {
                        Ok(stamp) if stamp.completed_ms > 0 => stamp.completed_ms,
                        Ok(_) => fallback_ms(),
                        Err(error) => {
                            skipped(&id, &error);
                            continue;
                        }
                    };
                entries.push((completed, id, Source::Json(bytes)));
            } else {
                let completed = match serde_json::from_reader::<_, Stamp>(BufReader::new(file))
                    .context("decode run record")
                {
                    Ok(stamp) if stamp.completed_ms > 0 => stamp.completed_ms,
                    Ok(_) => fallback_ms(),
                    Err(error) => {
                        skipped(&id, &error);
                        continue;
                    }
                };
                entries.push((completed, id, Source::OnDisk));
            }
        }
    }
    entries.sort_unstable_by(|a, b| {
        let order = a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1));
        if oldest { order } else { order.reverse() }
    });
    Ok(entries
        .into_iter()
        .filter_map(move |(completed, id, source)| {
            let loaded = match source {
                Source::Json(bytes) => decode_run(&id, &bytes, completed),
                Source::OnDisk => {
                    decode_run_file(&id, &path.join(&id).join("record.json"), completed)
                }
                Source::Unfinished => capture::load(workspace, &id).map(|(_, mut run)| {
                    if run.completed_ms == 0 {
                        run.completed_ms = completed;
                    }
                    if run.family.is_empty() {
                        run.family = "legacy".into();
                    }
                    run
                }),
            };
            match loaded {
                Ok(run) => Some(Ok(run)),
                Err(error) => {
                    skipped(&id, &error);
                    None
                }
            }
        }))
}

fn finalize_run(id: &str, mut run: Run, completed: u64) -> Result<Run> {
    if run.id != id {
        anyhow::bail!("run record does not match its directory");
    }
    if run.completed_ms == 0 {
        run.completed_ms = completed;
    }
    if run.family.is_empty() {
        run.family = "legacy".into();
    }
    Ok(run)
}

fn decode_run(id: &str, bytes: &[u8], completed: u64) -> Result<Run> {
    if !valid_run_id(id) {
        anyhow::bail!("invalid run ID");
    }
    let run: Run = serde_json::from_slice(bytes).context("decode run record")?;
    finalize_run(id, run, completed)
}

fn decode_run_file(id: &str, path: &Path, completed: u64) -> Result<Run> {
    if !valid_run_id(id) {
        anyhow::bail!("invalid run ID");
    }
    let file = fs::File::open(path).context("read run record")?;
    let run: Run = serde_json::from_reader(BufReader::new(file)).context("decode run record")?;
    finalize_run(id, run, completed)
}

/// A later request that printed part of a run's output again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retrieval {
    /// `log` replaying whole streams.
    Full,
    /// `log` with a line range.
    Range,
    /// `search` within the run.
    Search,
    /// `context --run`.
    Context,
    /// Lines of the run that `history TEXT`, `lookup --logs` or `pack` printed.
    Excerpt,
    /// A replay with `--unredacted`, counted on top of its full, range or search
    /// request, which carries the bytes.
    Unredacted,
}

/// How often a run's output was retrieved after its preview, in `recalls.json`
/// beside the run. Records written before bytes were counted hold only the counts.
#[derive(Default, Serialize, Deserialize)]
struct Recalls {
    #[serde(default)]
    full: u64,
    #[serde(default)]
    range: u64,
    #[serde(default)]
    search: u64,
    #[serde(default)]
    context: u64,
    /// Replays with `--unredacted`, counted on top of the full, range or search call.
    #[serde(default)]
    unredacted: u64,
    #[serde(default)]
    excerpts: u64,
    /// The bytes printed by the retrievals that `measured` counts.
    #[serde(default)]
    bytes: u64,
    /// Retrievals whose bytes are in `bytes`. The rest of each count was made
    /// before bytes were counted, or by a caller that failed to record them.
    #[serde(default)]
    measured: Measured,
}

#[derive(Default, Serialize, Deserialize)]
struct Measured {
    #[serde(default)]
    full: u64,
    #[serde(default)]
    range: u64,
    #[serde(default)]
    search: u64,
    #[serde(default)]
    context: u64,
    #[serde(default)]
    excerpts: u64,
}

impl Recalls {
    fn count(&mut self, retrieval: Retrieval) {
        match retrieval {
            Retrieval::Full => self.full += 1,
            Retrieval::Range => self.range += 1,
            Retrieval::Search => self.search += 1,
            Retrieval::Context => self.context += 1,
            Retrieval::Excerpt => self.excerpts += 1,
            Retrieval::Unredacted => self.unredacted += 1,
        }
    }

    fn measure(&mut self, retrieval: Retrieval, bytes: u64) {
        let measured = &mut self.measured;
        match retrieval {
            Retrieval::Full => measured.full += 1,
            Retrieval::Range => measured.range += 1,
            Retrieval::Search => measured.search += 1,
            Retrieval::Context => measured.context += 1,
            Retrieval::Excerpt => measured.excerpts += 1,
            Retrieval::Unredacted => return,
        }
        self.bytes = self.bytes.saturating_add(bytes);
    }

    fn replayed(&self, directory: &Path) -> Replayed {
        let measured = &self.measured;
        let full = self.full.saturating_sub(measured.full);
        // A whole replay prints both archived streams, so their size bounds it.
        let archive = || {
            ["stdout", "stderr"]
                .iter()
                .filter_map(|stream| fs::metadata(directory.join(stream)).ok())
                .map(|meta| meta.len())
                .sum::<u64>()
        };
        Replayed {
            measured: self.bytes,
            estimated: if full > 0 {
                full.saturating_mul(archive())
            } else {
                0
            },
            estimated_replays: full,
            unmeasured: self.range.saturating_sub(measured.range)
                + self.search.saturating_sub(measured.search)
                + self.context.saturating_sub(measured.context)
                + self.excerpts.saturating_sub(measured.excerpts),
        }
    }
}

/// What later retrievals of a run printed after its preview.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Replayed {
    /// Bytes measured as they were printed.
    pub measured: u64,
    /// Whole replays recorded without their size, each estimated as the size of
    /// the run's archived streams.
    pub estimated: u64,
    pub estimated_replays: u64,
    /// Range, search, context and excerpt requests recorded without their size.
    /// Nothing bounds them, so they are counted but not subtracted.
    pub unmeasured: u64,
}

impl Replayed {
    /// Every byte subtracted from the run's savings: measured and estimated.
    pub fn bytes(&self) -> u64 {
        self.measured.saturating_add(self.estimated)
    }

    fn add(&mut self, other: Replayed) {
        self.measured += other.measured;
        self.estimated += other.estimated;
        self.estimated_replays += other.estimated_replays;
        self.unmeasured += other.unmeasured;
    }
}

/// What later retrievals of the run in `directory` printed. A damaged
/// `recalls.json` counts as no retrieval, with a warning unless `quiet`.
pub fn replayed(directory: &Path, quiet: bool) -> Replayed {
    let path = directory.join("recalls.json");
    let recalls: Recalls = if quiet {
        get(&path).unwrap_or_default()
    } else {
        get_or_warn(&path)
    };
    recalls.replayed(directory)
}

/// Savings net of later retrievals: raw output minus what the agent received at
/// first and every byte it retrieved afterwards. Negative when the retrievals
/// cost more than the preview saved.
pub fn net_bytes(raw: u64, shown: u64, replayed: u64) -> i64 {
    let signed = |bytes: u64| i64::try_from(bytes).unwrap_or(i64::MAX);
    signed(raw)
        .saturating_sub(signed(shown))
        .saturating_sub(signed(replayed))
}

#[derive(Default, Serialize, Deserialize)]
struct Activity {
    bypasses: BTreeMap<String, u64>,
    reads: Vec<Read>,
    repeat_reads: u64,
    #[serde(default)]
    last_pack: BTreeMap<String, String>,
    /// Views of symbols read before that printed only what changed, see
    /// `crate::seen`.
    #[serde(default)]
    deltas: Deltas,
    /// Source outlines printed by `justless read` (CLI or MCP) instead of reading
    /// the full file, alongside follow-up `expand` bytes on those outlined files.
    #[serde(default)]
    outlines: Outlines,
    /// Semantic code-graph invocations (`map`, `query`, `path`, `context`,
    /// `pack`, `review`) and their measured bytes vs. referenced files.
    #[serde(default, skip_serializing_if = "GraphUsage::is_empty")]
    graph: GraphUsage,
    /// Commands the Claude Code command hook left as written for the permission
    /// rules, by the permission mode the session reported. See `left_by_permissions`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    permission_leaves: BTreeMap<String, u64>,
}

/// Measured semantic code-graph invocations (`map`, `query`, `path`, `context`,
/// `pack`, `review`) and their printed bytes vs. the full on-disk sizes of the
/// distinct workspace files they referenced.
#[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GraphUsage {
    pub calls: u64,
    pub shown_bytes: u64,
    pub referenced_file_bytes: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_command: BTreeMap<String, u64>,
}

impl GraphUsage {
    pub fn is_empty(&self) -> bool {
        self.calls == 0
            && self.shown_bytes == 0
            && self.referenced_file_bytes == 0
            && self.by_command.is_empty()
    }

    pub fn net_saved_bytes(&self) -> i64 {
        net_bytes(self.referenced_file_bytes, self.shown_bytes, 0)
    }

    pub fn json(&self) -> serde_json::Value {
        let net = self.net_saved_bytes();
        serde_json::json!({
            "calls": self.calls,
            "shown_bytes": self.shown_bytes,
            "referenced_file_bytes": self.referenced_file_bytes,
            "net_saved_bytes": net,
            "estimated_tokens_net_saved": crate::tokens::Content::Code.tokens_signed(net),
            "by_command": self.by_command,
        })
    }

    pub fn line(&self) -> String {
        let net = self.net_saved_bytes();
        let breakdown = if self.by_command.is_empty() {
            String::new()
        } else {
            let parts: Vec<String> = self
                .by_command
                .iter()
                .map(|(cmd, n)| format!("{cmd} ×{n}"))
                .collect();
            format!(" ({})", parts.join(", "))
        };
        format!(
            "Code graph: {} call(s){breakdown} printed {} instead of {} across referenced files, net {} kept out (≈{} tokens, estimated at {:.2} bytes per token for code)",
            self.calls,
            human_bytes(self.shown_bytes),
            human_bytes(self.referenced_file_bytes),
            signed_bytes(net),
            crate::tokens::Content::Code.tokens_signed(net),
            crate::tokens::Content::Code.bytes_per_token(),
        )
    }
}

/// Measured code-graph usage totals from a project's `activity.json`, quietly
/// returning zeros when unreadable or absent.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn graph_usage(workspace: &Workspace) -> GraphUsage {
    let activity: Activity = get(&workspace.state.join("activity.json")).unwrap_or_default();
    activity.graph
}

/// Records one semantic code-graph command invocation (`map`, `query`, `path`,
/// `context`, `pack`, `review`) with its printed bytes and the sum of distinct
/// referenced workspace file sizes. Fail-silent and only writes when the
/// workspace's `.justless/` state directory already exists.
pub fn record_graph_use(
    workspace: &Workspace,
    command: &str,
    shown_bytes: u64,
    referenced_file_bytes: u64,
) {
    if !workspace.state.is_dir() || command.is_empty() {
        return;
    }
    let _ = update(
        &workspace.state.join("activity.json"),
        |a: &mut Activity| {
            a.graph.calls = a.graph.calls.saturating_add(1);
            a.graph.shown_bytes = a.graph.shown_bytes.saturating_add(shown_bytes);
            a.graph.referenced_file_bytes = a
                .graph
                .referenced_file_bytes
                .saturating_add(referenced_file_bytes);
            *a.graph.by_command.entry(command.to_owned()).or_default() += 1;
        },
    );
}

/// Sums on-disk byte lengths of distinct workspace-relative `files` using
/// metadata only (never reading file bodies). Rejects non-normal path
/// components and skips symlinks or missing files silently.
pub(crate) fn referenced_file_bytes<'a>(
    workspace: &Workspace,
    files: impl IntoIterator<Item = &'a str>,
) -> u64 {
    let mut seen = HashSet::new();
    let mut total = 0u64;
    for file in files {
        if file.is_empty() || !seen.insert(file) {
            continue;
        }
        let rel = Path::new(file);
        if !rel
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            continue;
        }
        if let Ok(meta) = fs::symlink_metadata(workspace.root.join(rel))
            && meta.is_file()
        {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// Records one code-graph invocation by stat-ing distinct referenced files only
/// when the workspace's `.justless/` state directory already exists.
pub(crate) fn record_graph_files<'a>(
    workspace: &Workspace,
    command: &str,
    shown_bytes: u64,
    files: impl IntoIterator<Item = &'a str>,
) {
    if !workspace.state.is_dir() || command.is_empty() {
        return;
    }
    let referenced = referenced_file_bytes(workspace, files);
    record_graph_use(workspace, command, shown_bytes, referenced);
}

/// How many views printed changes since an earlier read instead of the whole
/// source, and the bytes of the whole views and of what they printed.
#[derive(Default, Serialize, Deserialize)]
struct Deltas {
    views: u64,
    whole_bytes: u64,
    shown_bytes: u64,
}

/// Maximum distinct outlined files remembered in `Outlines::seen` so
/// `activity.json` stays bounded while preventing double-counting past the
/// 32-entry `Activity::reads` window.
const MAX_OUTLINE_SEEN: usize = 512;

/// Measured bytes for `justless read` outlines and follow-up `expand` calls on
/// those outlined files.
#[derive(Default, Serialize, Deserialize)]
struct Outlines {
    views: u64,
    whole_bytes: u64,
    shown_bytes: u64,
    expanded_bytes: u64,
    /// Last outlined digest per workspace-relative file path, used to avoid
    /// double-counting `whole_bytes` and to attribute follow-up `expand` bytes
    /// even after `Activity::reads` rotates past 32 entries.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    seen: BTreeMap<String, String>,
}

/// Measured source outline and follow-up `expand` totals for `stats` and
/// `stats --all`.
#[derive(Clone, Copy, Default)]
pub(crate) struct OutlinesSummary {
    pub views: u64,
    pub whole_bytes: u64,
    pub shown_bytes: u64,
    pub expanded_bytes: u64,
}

impl OutlinesSummary {
    fn from_activity(outlines: &Outlines) -> Self {
        Self {
            views: outlines.views,
            whole_bytes: outlines.whole_bytes,
            shown_bytes: outlines.shown_bytes,
            expanded_bytes: outlines.expanded_bytes,
        }
    }

    pub fn add(&mut self, other: &Self) {
        self.views += other.views;
        self.whole_bytes += other.whole_bytes;
        self.shown_bytes += other.shown_bytes;
        self.expanded_bytes += other.expanded_bytes;
    }

    pub fn saved_bytes(&self) -> i64 {
        net_bytes(self.whole_bytes, self.shown_bytes, 0)
    }

    pub fn net_saved_bytes(&self) -> i64 {
        net_bytes(self.whole_bytes, self.shown_bytes, self.expanded_bytes)
    }

    pub fn json(&self) -> serde_json::Value {
        let saved = self.saved_bytes();
        let net = self.net_saved_bytes();
        serde_json::json!({
            "views": self.views,
            "whole_bytes": self.whole_bytes,
            "shown_bytes": self.shown_bytes,
            "expanded_bytes": self.expanded_bytes,
            "saved_bytes": saved,
            "net_saved_bytes": net,
            "estimated_tokens_saved": crate::tokens::Content::Code.tokens_signed(saved),
            "estimated_tokens_net_saved": crate::tokens::Content::Code.tokens_signed(net),
        })
    }

    pub fn line(&self) -> String {
        let net = self.net_saved_bytes();
        let expanded = if self.expanded_bytes > 0 {
            format!(" (+{} expanded)", human_bytes(self.expanded_bytes))
        } else {
            String::new()
        };
        format!(
            "Source outlines: {} view(s) printed {} instead of {}{expanded}, net {} kept out (≈{} tokens, estimated at {:.2} bytes per token for code)",
            self.views,
            human_bytes(self.shown_bytes),
            human_bytes(self.whole_bytes),
            signed_bytes(net),
            crate::tokens::Content::Code.tokens_signed(net),
            crate::tokens::Content::Code.bytes_per_token()
        )
    }
}

/// Measured source outline totals from a project's `activity.json`, quietly
/// returning zeros when unreadable or absent.
pub(crate) fn outlines_summary(workspace: &Workspace) -> OutlinesSummary {
    let activity: Activity = get(&workspace.state.join("activity.json")).unwrap_or_default();
    OutlinesSummary::from_activity(&activity.outlines)
}

/// Measured repeat-read / delta totals recorded by `seen::Memory` (`context`,
/// `pack`, and session `read`/`expand` dedup) for `stats` and `stats --all`.
#[derive(Clone, Copy, Default)]
pub(crate) struct DeltasSummary {
    pub views: u64,
    pub whole_bytes: u64,
    pub shown_bytes: u64,
}

impl DeltasSummary {
    fn from_activity(deltas: &Deltas) -> Self {
        Self {
            views: deltas.views,
            whole_bytes: deltas.whole_bytes,
            shown_bytes: deltas.shown_bytes,
        }
    }

    pub fn add(&mut self, other: &Self) {
        self.views += other.views;
        self.whole_bytes += other.whole_bytes;
        self.shown_bytes += other.shown_bytes;
    }

    pub fn saved_bytes(&self) -> u64 {
        self.whole_bytes.saturating_sub(self.shown_bytes)
    }

    pub fn json(&self) -> serde_json::Value {
        let saved = self.saved_bytes();
        serde_json::json!({
            "views": self.views,
            "whole_bytes": self.whole_bytes,
            "shown_bytes": self.shown_bytes,
            "saved_bytes": saved,
            "estimated_tokens_saved": crate::tokens::Content::Code.tokens(saved),
        })
    }

    pub fn line(&self) -> String {
        let saved = self.saved_bytes();
        format!(
            "Changed since read: {} view(s) printed {} instead of {}, {} kept out (≈{} tokens, estimated at {:.2} bytes per token for code)",
            self.views,
            human_bytes(self.shown_bytes),
            human_bytes(self.whole_bytes),
            human_bytes(saved),
            crate::tokens::Content::Code.tokens(saved),
            crate::tokens::Content::Code.bytes_per_token()
        )
    }
}

/// Measured repeat-read delta totals from a project's `activity.json`, quietly
/// returning zeros when unreadable or absent.
pub(crate) fn deltas_summary(workspace: &Workspace) -> DeltasSummary {
    let activity: Activity = get(&workspace.state.join("activity.json")).unwrap_or_default();
    DeltasSummary::from_activity(&activity.deltas)
}

/// Measured on-disk archived tool results under `.justless/observations/`
/// (created by `justless mask` or `justless proxy`) for `stats` and `stats --all`.
#[derive(Clone, Copy, Default)]
pub(crate) struct ObservationsSummary {
    pub archived_results: u64,
    pub archived_bytes: u64,
}

impl ObservationsSummary {
    pub fn add(&mut self, other: &Self) {
        self.archived_results += other.archived_results;
        self.archived_bytes += other.archived_bytes;
    }

    pub fn json(&self, recalls: &ArchiveRecalls) -> serde_json::Value {
        let net = net_bytes(self.archived_bytes, 0, recalls.bytes);
        serde_json::json!({
            "archived_results": self.archived_results,
            "archived_bytes": self.archived_bytes,
            "recalled_count": recalls.count,
            "recalled_bytes": recalls.bytes,
            "net_archived_bytes": net,
            "estimated_tokens_archived": crate::tokens::Content::Logs.tokens(self.archived_bytes),
        })
    }

    pub fn line(&self, recalls: &ArchiveRecalls) -> String {
        let ratio = crate::tokens::Content::Logs.bytes_per_token();
        let tokens = crate::tokens::Content::Logs.tokens(self.archived_bytes);
        if recalls.count > 0 {
            let net = net_bytes(self.archived_bytes, 0, recalls.bytes);
            format!(
                "Archived tool results: {} result(s) ({}, ≈{} tokens estimated at {:.2} bytes per token for logs), {} recalled ({} net)",
                self.archived_results,
                human_bytes(self.archived_bytes),
                tokens,
                ratio,
                human_bytes(recalls.bytes),
                signed_bytes(net),
            )
        } else {
            format!(
                "Archived tool results: {} result(s) ({}, ≈{} tokens estimated at {:.2} bytes per token for logs)",
                self.archived_results,
                human_bytes(self.archived_bytes),
                tokens,
                ratio,
            )
        }
    }
}

fn is_observation_file(name: &str) -> bool {
    name.strip_suffix(".json").is_some_and(|id| {
        id.len() == 64
            && id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// Count unique archived tool-result files stored under `.justless/observations/`
/// and their measured bytes on disk.
pub(crate) fn observations_summary(workspace: &Workspace) -> ObservationsSummary {
    let Ok(entries) = fs::read_dir(workspace.state.join("observations")) else {
        return ObservationsSummary::default();
    };
    let mut summary = ObservationsSummary::default();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_observation_file(name) || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            summary.archived_results += 1;
            summary.archived_bytes += meta.len();
        }
    }
    summary
}

#[derive(Serialize, Deserialize)]
struct Read {
    file: String,
    digest: String,
    from: usize,
    to: usize,
    /// The file's outline rather than its lines, with `from` and `to` 0.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    outline: bool,
}

fn get<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::File::open(path) {
        Ok(file) => Ok(serde_json::from_reader(BufReader::new(file))?),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

/// Reads state for a report, warning about a damaged file instead of failing.
fn get_or_warn<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    get(path).unwrap_or_else(|error| {
        eprintln!(
            "justless: ignored unreadable {}: {error:#} (run `justless doctor` to check local state)",
            path.display()
        );
        T::default()
    })
}

fn update<T: serde::de::DeserializeOwned + Serialize + Default>(
    path: &Path,
    f: impl FnOnce(&mut T),
) -> Result<()> {
    let _guard = lock_state(&path.with_extension("lock"))?;
    let mut value = get::<T>(path)?;
    f(&mut value);
    write_json(path, &value)
}

/// Records a retrieval of the run in `directory` and the bytes it printed.
/// Unredacted reads also increment a separate counter without adding bytes twice.
pub fn recall(directory: &Path, retrieval: Retrieval, bytes: Option<u64>) {
    let _ = update(&directory.join("recalls.json"), |r: &mut Recalls| {
        r.count(retrieval);
        if let Some(bytes) = bytes {
            r.measure(retrieval, bytes);
        }
    });
}

/// Records the bytes that excerpts of several runs printed, such as the
/// matching lines of `history TEXT`, as one retrieval of each run.
pub fn excerpts(workspace: &Workspace, printed: &BTreeMap<String, u64>) {
    for (id, &bytes) in printed {
        if let Ok((directory, _)) = capture::load(workspace, id) {
            recall(&directory, Retrieval::Excerpt, Some(bytes));
        }
    }
}

/// The archived tool results that `justless recall` printed, one JSON line each
/// with its time and size. They came from the agent's client, not from a run.
const ARCHIVE_RECALLS: &str = "archive-recalls.jsonl";

#[derive(Serialize, Deserialize)]
struct ArchiveRecall {
    ms: u64,
    bytes: u64,
    #[serde(default)]
    proxy: bool,
}

/// Records that `justless recall` printed `bytes` of an archived tool result.
pub fn recalled_archive(workspace: &Workspace, bytes: u64, proxy: bool) {
    if workspace.prepare_state().is_err() {
        return;
    }
    let Ok(mut line) = serde_json::to_vec(&ArchiveRecall {
        ms: now_ms(),
        bytes,
        proxy,
    }) else {
        return;
    };
    line.push(b'\n');
    crate::statusline::append(&workspace.state.join(ARCHIVE_RECALLS), &line);
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ArchiveRecalls {
    pub(crate) count: u64,
    pub(crate) bytes: u64,
    pub(crate) proxy_bytes: u64,
}

impl ArchiveRecalls {
    pub(crate) fn add(&mut self, other: &Self) {
        self.count += other.count;
        self.bytes += other.bytes;
        self.proxy_bytes += other.proxy_bytes;
    }

    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "recalls": self.count,
            "bytes": self.bytes,
        })
    }
}

/// How many archived tool results `justless recall` printed since `since_ms`,
/// and their bytes. Streams line by line so long logs stay O(1) in memory and
/// truncated lines are skipped cleanly.
pub(crate) fn archive_recalls(state: &Path, since_ms: u64) -> ArchiveRecalls {
    let Ok(file) = fs::File::open(state.join(ARCHIVE_RECALLS)) else {
        return ArchiveRecalls::default();
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut total = ArchiveRecalls::default();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(recall) = serde_json::from_str::<ArchiveRecall>(trimmed)
                    && recall.ms >= since_ms
                {
                    total.count += 1;
                    total.bytes += recall.bytes;
                    if recall.proxy {
                        total.proxy_bytes += recall.bytes;
                    }
                }
            }
        }
    }
    total
}

/// Records that the agent was shown lines `from..=to` of the workspace file
/// `file`, whose bytes have `digest` ([`capture::source_digest`]), for `resume`,
/// the compaction handoff and their `[source changed]` notices. Each command
/// that prints source lines calls it once per range it printed. The same range
/// of unchanged source shown again counts as a repeated read in `stats`.
pub fn saw_source(workspace: &Workspace, file: &str, from: usize, to: usize, digest: String) {
    saw_range(workspace, file, from, to, digest, false);
}

/// Records that the agent was shown `file`'s outline: its revision, rows and
/// line ranges, but not its lines. A file keeps one outline entry whatever the
/// page, and it never counts as a repeated read.
pub fn saw_outline(workspace: &Workspace, file: &str, digest: String) {
    saw_range(workspace, file, 0, 0, digest, true);
}

/// Records an outline printed by `justless read` (CLI or MCP) together with its
/// measured file and output sizes for `stats`. Re-outlining or paging through an
/// already-outlined unchanged file adds only `shown_bytes`, never double-counting
/// `whole_bytes`.
pub fn saw_outline_measured(
    workspace: &Workspace,
    file: &str,
    digest: String,
    file_bytes: u64,
    shown_bytes: u64,
) {
    if workspace.prepare_state().is_err() {
        return;
    }
    let _ = update(
        &workspace.state.join("activity.json"),
        |a: &mut Activity| {
            let already_outlined = a
                .outlines
                .seen
                .get(file)
                .is_some_and(|seen| seen == &digest)
                || a.reads
                    .iter()
                    .any(|r| r.file == file && r.digest == digest && r.outline);
            if !already_outlined {
                a.outlines.whole_bytes += file_bytes;
            }
            a.outlines.views += 1;
            a.outlines.shown_bytes += shown_bytes;
            if !a.outlines.seen.contains_key(file)
                && a.outlines.seen.len() >= MAX_OUTLINE_SEEN
                && let Some(victim) = a
                    .outlines
                    .seen
                    .keys()
                    .find(|path| !a.reads.iter().any(|r| &r.file == *path))
                    .cloned()
            {
                a.outlines.seen.remove(&victim);
            }
            if a.outlines.seen.contains_key(file) || a.outlines.seen.len() < MAX_OUTLINE_SEEN {
                a.outlines.seen.insert(file.to_owned(), digest.clone());
            }
            record_read(a, file, 0, 0, digest, true);
        },
    );
}

/// Records a source slice printed by `justless expand` (CLI or MCP). When the
/// same unchanged file was outlined, its `shown_bytes` are subtracted from net
/// outline savings in `stats`.
pub fn saw_expand(
    workspace: &Workspace,
    file: &str,
    from: usize,
    to: usize,
    digest: String,
    shown_bytes: u64,
) {
    if workspace.prepare_state().is_err() {
        return;
    }
    let _ = update(
        &workspace.state.join("activity.json"),
        |a: &mut Activity| {
            let was_outlined = a
                .outlines
                .seen
                .get(file)
                .is_some_and(|seen| seen == &digest)
                || a.reads
                    .iter()
                    .any(|r| r.file == file && r.digest == digest && r.outline);
            if was_outlined {
                a.outlines.expanded_bytes += shown_bytes;
            }
            record_read(a, file, from, to, digest, false);
        },
    );
}

pub(crate) fn saw_range(
    workspace: &Workspace,
    file: &str,
    from: usize,
    to: usize,
    digest: String,
    outline: bool,
) {
    if workspace.prepare_state().is_err() {
        return;
    }
    let _ = update(
        &workspace.state.join("activity.json"),
        |a: &mut Activity| record_read(a, file, from, to, digest, outline),
    );
}

fn record_read(
    a: &mut Activity,
    file: &str,
    from: usize,
    to: usize,
    digest: String,
    outline: bool,
) {
    if let Some(i) = a
        .reads
        .iter()
        .position(|r| r.file == file && r.from == from && r.to == to && r.outline == outline)
    {
        let old = a.reads.remove(i);
        if old.digest == digest && !outline {
            a.repeat_reads += 1;
        }
    }
    a.reads.insert(
        0,
        Read {
            file: file.into(),
            digest,
            from,
            to,
            outline,
        },
    );
    a.reads.truncate(32);
}

/// Records that `views` views printed `shown` bytes of changes since an
/// earlier read where their whole views would have printed `whole`.
pub fn saw_delta(workspace: &Workspace, views: u64, whole: u64, shown: u64) {
    if workspace.prepare_state().is_err() {
        return;
    }
    let _ = update(
        &workspace.state.join("activity.json"),
        |a: &mut Activity| {
            a.deltas.views += views;
            a.deltas.whole_bytes += whole;
            a.deltas.shown_bytes += shown;
        },
    );
}

pub fn saw_pack(workspace: &Workspace, files: BTreeMap<String, String>) {
    if !files.is_empty() {
        let _ = update(
            &workspace.state.join("activity.json"),
            |a: &mut Activity| a.last_pack = files,
        );
    }
}

pub fn bypass(workspace: &Workspace, command: &str) {
    if !workspace.state.is_dir() || crate::rewrite::previewing() {
        return;
    }
    let Some(word) = command.split_whitespace().next() else {
        return;
    };
    let family = match word {
        "justless" => return,
        "git" | "cargo" | "pytest" | "go" | "npm" | "rg" | "python" | "python3" | "cat" | "ls"
        | "sed" | "grep" | "find" | "pnpm" | "yarn" | "make" => word,
        _ => "other",
    };
    let _ = update(
        &workspace.state.join("activity.json"),
        |a: &mut Activity| *a.bypasses.entry(family.to_owned()).or_default() += 1,
    );
}

/// Counts a command the Claude Code command hook left as written because of the
/// permission rules in the permission mode `mode` that the session reported, so
/// `doctor` sees a mode no settings file shows, such as one from
/// `--permission-mode`, `--settings` or a switch during the session. Only a count
/// per mode is kept, never the command: a mode Claude Code documents by name, and
/// any other as `other`.
pub fn left_by_permissions(workspace: &Workspace, mode: &str) {
    if !workspace.state.is_dir() {
        return;
    }
    let mode = match mode {
        "default" | "acceptEdits" | "dontAsk" | "bypassPermissions" | "auto" | "plan" => mode,
        _ => "other",
    };
    let _ = update(
        &workspace.state.join("activity.json"),
        |a: &mut Activity| *a.permission_leaves.entry(mode.to_owned()).or_default() += 1,
    );
}

/// Byte totals for a group of runs. `preview_bytes` is what the agent received
/// at first, footers included. Later retrievals are kept apart from it.
#[derive(Default, Serialize)]
struct Totals {
    raw_bytes: u64,
    preview_bytes: u64,
    /// Bytes that later retrievals printed, as measured.
    replayed_bytes: u64,
    /// Whole replays recorded without their size, estimated from the archive.
    estimated_replay_bytes: u64,
    /// Raw output minus the preview and every measured or estimated retrieval.
    net_saved_bytes: i64,
}

impl Totals {
    fn add(&mut self, raw: u64, shown: u64, replayed: Replayed) {
        self.raw_bytes += raw;
        self.preview_bytes += shown;
        self.replayed_bytes += replayed.measured;
        self.estimated_replay_bytes += replayed.estimated;
        self.net_saved_bytes =
            self.net_saved_bytes
                .saturating_add(net_bytes(raw, shown, replayed.bytes()));
    }

    /// Savings at first preview, before any retrieval.
    fn initial(&self) -> u64 {
        self.raw_bytes.saturating_sub(self.preview_bytes)
    }
}

#[derive(Default, Serialize)]
struct FamilyStats {
    family: String,
    runs: u64,
    #[serde(flatten)]
    totals: Totals,
    reopened_runs: u64,
    reopen_percent: f64,
    estimated_tokens_net_saved: i64,
    full_reads: u64,
    targeted_lookups: u64,
    /// Lines of these runs that `history TEXT`, `lookup --logs` or `pack` printed.
    excerpts: u64,
    unchanged_runs: u64,
    /// Runs whose printed preview and footer were larger than the raw output.
    grown_runs: u64,
}

/// How a captured run's output was presented to the agent, derived strictly from
/// fields already stored in `record.json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewKind {
    StructuredView,
    TestSummary,
    DiffSummary,
    PlanSummary,
    Unchanged,
    ClippedPreview,
    Verbatim,
}

impl ViewKind {
    const COUNT: usize = 7;
    const ALL: [Self; Self::COUNT] = [
        Self::StructuredView,
        Self::TestSummary,
        Self::DiffSummary,
        Self::PlanSummary,
        Self::Unchanged,
        Self::ClippedPreview,
        Self::Verbatim,
    ];

    fn index(self) -> usize {
        self as usize
    }

    fn key(self) -> &'static str {
        match self {
            Self::StructuredView => "structured",
            Self::TestSummary => "tests",
            Self::DiffSummary => "diff",
            Self::PlanSummary => "plan",
            Self::Unchanged => "unchanged",
            Self::ClippedPreview => "preview",
            Self::Verbatim => "verbatim",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::StructuredView => "structured view",
            Self::TestSummary => "test summary",
            Self::DiffSummary => "diff summary",
            Self::PlanSummary => "plan summary",
            Self::Unchanged => "unchanged rerun",
            Self::ClippedPreview => "clipped preview",
            Self::Verbatim => "verbatim",
        }
    }
}

fn classify_view(
    unchanged: bool,
    replaced: Option<&str>,
    summary_kind: &str,
    raw_bytes: u64,
    shown: u64,
) -> ViewKind {
    if unchanged {
        ViewKind::Unchanged
    } else if replaced.is_some() {
        match summary_kind {
            "tests" => ViewKind::TestSummary,
            "diff" => ViewKind::DiffSummary,
            "plan" => ViewKind::PlanSummary,
            _ => ViewKind::StructuredView,
        }
    } else if shown < raw_bytes {
        ViewKind::ClippedPreview
    } else {
        ViewKind::Verbatim
    }
}

#[derive(Default, Serialize)]
struct ViewStats {
    kind: &'static str,
    #[serde(skip)]
    label: &'static str,
    runs: u64,
    #[serde(flatten)]
    totals: Totals,
    reopened_runs: u64,
    reopen_percent: f64,
    estimated_tokens_net_saved: i64,
}

#[derive(Default, Serialize)]
struct DayStats {
    date: String,
    runs: u64,
    #[serde(flatten)]
    totals: Totals,
}

/// Tokens in command output, at the bytes per token measured for logs, for the status
/// line and `discover`, which do not read archives. `stats` classifies each run with
/// [`crate::tokens`].
pub fn estimated_tokens(bytes: u64) -> u64 {
    crate::tokens::Content::Logs.tokens(bytes)
}

/// [`human_bytes`] for net savings, which can be negative.
pub(crate) fn signed_bytes(bytes: i64) -> String {
    let size = human_bytes(bytes.unsigned_abs());
    if bytes < 0 { format!("-{size}") } else { size }
}

fn signed_percent(part: i64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

fn rounded(percent: f64) -> f64 {
    (percent * 10.0).round() / 10.0
}

pub fn human_bytes(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

/// The UTC calendar date for a Unix timestamp in milliseconds.
pub fn utc_date(ms: u64) -> String {
    utc_date_from_day(ms / 86_400_000)
}

fn utc_date_from_day(day: u64) -> String {
    // Civil-from-days conversion for the proleptic Gregorian calendar.
    let z = day as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// One project's run count and byte totals, counted as `stats` counts them.
#[derive(Default, Serialize)]
pub(crate) struct Summary {
    pub runs: u64,
    pub raw_bytes: u64,
    pub preview_bytes: u64,
    /// Bytes later retrievals printed, measured or estimated.
    pub replayed_bytes: u64,
    pub net_saved_bytes: i64,
    /// Saved bytes by content type, for the token estimate.
    #[serde(skip)]
    pub estimate: crate::tokens::Estimate,
}

impl Summary {
    pub fn add(&mut self, other: &Summary) {
        self.runs += other.runs;
        self.raw_bytes += other.raw_bytes;
        self.preview_bytes += other.preview_bytes;
        self.replayed_bytes += other.replayed_bytes;
        self.net_saved_bytes = self.net_saved_bytes.saturating_add(other.net_saved_bytes);
        self.estimate.merge(&other.estimate);
    }

    pub fn saved_bytes(&self) -> u64 {
        self.raw_bytes.saturating_sub(self.preview_bytes)
    }
}

/// Lightweight projection of `capture::Run` for `summary`, validating all
/// required `Run` fields while skipping unused collections with zero allocations.
#[derive(Deserialize)]
struct SummaryRecord {
    id: String,
    #[serde(rename = "status")]
    _status: serde::de::IgnoredAny,
    raw_bytes: u64,
    preview_bytes: usize,
    #[serde(default)]
    completed_ms: u64,
    #[serde(default)]
    footer_bytes: usize,
    #[serde(rename = "diagnostics")]
    _diagnostics: serde::de::IgnoredAny,
    #[serde(rename = "source_digests")]
    _source_digests: serde::de::IgnoredAny,
}

#[derive(Default, Deserialize)]
struct SummaryKind {
    #[serde(default)]
    kind: String,
}

/// Lightweight projection of `capture::Run` for `stats`, validating all
/// required `Run` fields while skipping unused collections with zero allocations.
#[derive(Deserialize)]
struct StatsRecord {
    id: String,
    #[serde(rename = "status")]
    _status: serde::de::IgnoredAny,
    raw_bytes: u64,
    preview_bytes: usize,
    #[serde(default)]
    completed_ms: u64,
    #[serde(default)]
    family: String,
    #[serde(default)]
    unchanged: bool,
    #[serde(default)]
    footer_bytes: usize,
    #[serde(default)]
    redaction: Option<crate::redact::Found>,
    #[serde(default)]
    replaced: Option<String>,
    #[serde(default)]
    summary: SummaryKind,
    #[serde(rename = "diagnostics")]
    _diagnostics: serde::de::IgnoredAny,
    #[serde(rename = "source_digests")]
    _source_digests: serde::de::IgnoredAny,
}

fn add_run_estimate(
    estimate: &mut crate::tokens::Estimate,
    directory: &Path,
    raw_bytes: u64,
    shown: u64,
    replayed_bytes: u64,
) -> Option<(crate::tokens::Content, i64, i64)> {
    let initial = net_bytes(raw_bytes, shown, 0);
    let net = net_bytes(raw_bytes, shown, replayed_bytes);
    // An uncompacted run with zero initial and zero net savings adds (0, 0) to
    // its bucket, so skip opening and classifying its archived streams.
    if initial != 0 || net != 0 {
        let content = crate::tokens::run_content(directory);
        estimate.add(content, initial, net);
        Some((content, initial, net))
    } else {
        None
    }
}

/// The totals of the runs in a project's archive that finished at or after
/// `since_ms`, as the `stats` timeline counts them, quietly skipping unreadable runs.
pub(crate) fn summary(workspace: &Workspace, since_ms: u64) -> Summary {
    let mut totals = Totals::default();
    let mut estimate = crate::tokens::Estimate::default();
    let mut runs = 0;
    let Ok(entries) = fs::read_dir(workspace.state.join("runs")) else {
        return Summary::default();
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let Some(id) = name.to_str() else {
            continue;
        };
        if !valid_run_id(id) {
            continue;
        }
        let directory = entry.path();
        let record_path = directory.join("record.json");
        let (raw_bytes, preview_bytes, footer_bytes, completed_ms) =
            match fs::metadata(&record_path) {
                Ok(meta) => {
                    let modified = meta.modified().map_or(u64::MAX, millis);
                    if since_ms > 0 && modified.saturating_add(RECORD_SLACK_MS) < since_ms {
                        continue;
                    }
                    let Ok(file) = fs::File::open(&record_path) else {
                        continue;
                    };
                    let Ok(record) =
                        serde_json::from_reader::<_, SummaryRecord>(BufReader::new(file))
                    else {
                        continue;
                    };
                    if record.id != id {
                        continue;
                    }
                    let completed = if record.completed_ms > 0 {
                        record.completed_ms
                    } else {
                        meta.modified().map_or(0, millis)
                    };
                    (
                        record.raw_bytes,
                        record.preview_bytes,
                        record.footer_bytes,
                        completed,
                    )
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    let Ok(modified) =
                        fs::metadata(directory.join("stdout")).and_then(|meta| meta.modified())
                    else {
                        continue;
                    };
                    (0, 0, 0, millis(modified))
                }
                Err(_) => continue,
            };
        if completed_ms < since_ms || completed_ms == 0 {
            continue;
        }
        let recalls: Recalls = get(&directory.join("recalls.json")).unwrap_or_default();
        let shown = (preview_bytes + footer_bytes) as u64;
        let later = recalls.replayed(&directory);
        totals.add(raw_bytes, shown, later);
        let _ = add_run_estimate(&mut estimate, &directory, raw_bytes, shown, later.bytes());
        runs += 1;
    }
    Summary {
        runs,
        raw_bytes: totals.raw_bytes,
        preview_bytes: totals.preview_bytes,
        replayed_bytes: totals.replayed_bytes + totals.estimated_replay_bytes,
        net_saved_bytes: totals.net_saved_bytes,
        estimate,
    }
}

struct StatsData {
    count: u64,
    skipped_runs: u64,
    totals: Totals,
    estimate: crate::tokens::Estimate,
    replayed: Replayed,
    secrets: u64,
    unredacted: u64,
    commands: Vec<FamilyStats>,
    views: Vec<ViewStats>,
    timeline: BTreeMap<u64, DayStats>,
}

#[derive(Default)]
struct FamilyAccum {
    stats: FamilyStats,
    estimate: crate::tokens::Estimate,
}

#[derive(Default)]
struct ViewAccum {
    runs: u64,
    totals: Totals,
    reopened_runs: u64,
    estimate: crate::tokens::Estimate,
}

/// Single-pass streaming scan of `.justless/runs/` for `stats`.
fn collect_stats(workspace: &Workspace, since: u64) -> Result<StatsData> {
    let mut count = 0u64;
    let mut skipped_runs = 0u64;
    let mut totals = Totals::default();
    let mut estimate = crate::tokens::Estimate::default();
    let mut replayed = Replayed::default();
    let (mut secrets, mut unredacted) = (0u64, 0u64);
    let mut families: BTreeMap<String, FamilyAccum> = BTreeMap::new();
    let mut view_accums: [ViewAccum; ViewKind::COUNT] = Default::default();
    let mut timeline: BTreeMap<u64, DayStats> = BTreeMap::new();
    let runs_path = workspace.state.join("runs");
    if runs_path.exists() {
        for entry in fs::read_dir(&runs_path)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let directory = entry.path();
            let record_path = directory.join("record.json");
            if !valid_run_id(&id) {
                if record_path.exists() || directory.join("stdout").exists() {
                    skipped(&id, &anyhow::anyhow!("invalid run ID"));
                    skipped_runs += 1;
                }
                continue;
            }
            let (
                raw_bytes,
                preview_bytes,
                footer_bytes,
                completed_ms,
                family,
                unchanged,
                found,
                replaced,
                summary_kind,
            ) = match fs::File::open(&record_path) {
                Ok(file) => {
                    let parsed = serde_json::from_reader::<_, StatsRecord>(BufReader::new(&file))
                        .context("decode run record");
                    match parsed {
                        Ok(record) => {
                            if record.id != id {
                                skipped(
                                    &id,
                                    &anyhow::anyhow!("run record does not match its directory"),
                                );
                                skipped_runs += 1;
                                continue;
                            }
                            let completed = if record.completed_ms > 0 {
                                record.completed_ms
                            } else {
                                file.metadata()
                                    .and_then(|meta| meta.modified())
                                    .map_or(0, millis)
                            };
                            let family = if record.family.is_empty() {
                                "legacy".into()
                            } else {
                                record.family
                            };
                            (
                                record.raw_bytes,
                                record.preview_bytes,
                                record.footer_bytes,
                                completed,
                                family,
                                record.unchanged,
                                record.redaction.map_or(0, |found| found.count),
                                record.replaced,
                                record.summary.kind,
                            )
                        }
                        Err(error) => {
                            skipped(&id, &error);
                            skipped_runs += 1;
                            continue;
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    let Ok(modified) =
                        fs::metadata(directory.join("stdout")).and_then(|meta| meta.modified())
                    else {
                        continue;
                    };
                    (
                        0,
                        0,
                        0,
                        millis(modified),
                        "unknown".into(),
                        false,
                        0,
                        None,
                        String::new(),
                    )
                }
                Err(e) => {
                    skipped(&id, &anyhow::Error::new(e).context("read run record"));
                    skipped_runs += 1;
                    continue;
                }
            };
            // Count everything the agent received, including the footer.
            let shown = (preview_bytes + footer_bytes) as u64;
            count += 1;
            secrets += found;
            let c: Recalls = get_or_warn(&directory.join("recalls.json"));
            let later = c.replayed(&directory);
            totals.add(raw_bytes, shown, later);
            let classified =
                add_run_estimate(&mut estimate, &directory, raw_bytes, shown, later.bytes());
            replayed.add(later);
            unredacted += c.unredacted;

            let view = classify_view(
                unchanged,
                replaced.as_deref(),
                &summary_kind,
                raw_bytes,
                shown,
            );
            let v = &mut view_accums[view.index()];
            v.runs += 1;
            v.totals.add(raw_bytes, shown, later);
            v.reopened_runs += u64::from(c.full > 0);
            if let Some((content, initial, net)) = classified {
                v.estimate.add(content, initial, net);
            }

            let f = if let Some(existing) = families.get_mut(family.as_str()) {
                existing
            } else {
                let key = family.clone();
                families.entry(key).or_insert_with(|| FamilyAccum {
                    stats: FamilyStats {
                        family,
                        ..Default::default()
                    },
                    estimate: crate::tokens::Estimate::default(),
                })
            };
            f.stats.runs += 1;
            f.stats.totals.add(raw_bytes, shown, later);
            f.stats.reopened_runs += u64::from(c.full > 0);
            f.stats.full_reads += c.full;
            f.stats.targeted_lookups += c.range + c.search + c.context;
            f.stats.excerpts += c.excerpts;
            f.stats.unchanged_runs += u64::from(unchanged);
            f.stats.grown_runs += u64::from(shown > raw_bytes);
            if let Some((content, initial, net)) = classified {
                f.estimate.add(content, initial, net);
            }

            // Retrievals are not timed, so they count on the day their run completed.
            if completed_ms >= since && completed_ms > 0 {
                let day = completed_ms / 86_400_000;
                let d = timeline.entry(day).or_insert_with(|| DayStats {
                    date: utc_date_from_day(day),
                    ..Default::default()
                });
                d.runs += 1;
                d.totals.add(raw_bytes, shown, later);
            }
        }
    }
    let mut commands: Vec<FamilyStats> = families
        .into_values()
        .map(|mut accum| {
            accum.stats.reopen_percent =
                rounded(percent(accum.stats.reopened_runs, accum.stats.runs));
            accum.stats.estimated_tokens_net_saved = accum.estimate.net_tokens();
            accum.stats
        })
        .collect();
    commands.sort_unstable_by(|a, b| {
        b.totals
            .net_saved_bytes
            .cmp(&a.totals.net_saved_bytes)
            .then_with(|| a.family.cmp(&b.family))
    });
    let mut views: Vec<ViewStats> = ViewKind::ALL
        .into_iter()
        .filter_map(|kind| {
            let accum = std::mem::take(&mut view_accums[kind.index()]);
            (accum.runs > 0).then(|| ViewStats {
                kind: kind.key(),
                label: kind.label(),
                runs: accum.runs,
                reopen_percent: rounded(percent(accum.reopened_runs, accum.runs)),
                reopened_runs: accum.reopened_runs,
                estimated_tokens_net_saved: accum.estimate.net_tokens(),
                totals: accum.totals,
            })
        })
        .collect();
    views.sort_unstable_by(|a, b| {
        b.totals
            .net_saved_bytes
            .cmp(&a.totals.net_saved_bytes)
            .then_with(|| a.kind.cmp(b.kind))
    });
    Ok(StatsData {
        count,
        skipped_runs,
        totals,
        estimate,
        replayed,
        secrets,
        unredacted,
        commands,
        views,
        timeline,
    })
}

fn activity_summary_lines(
    deltas: &DeltasSummary,
    outlines: &OutlinesSummary,
    graph: &GraphUsage,
    observations: &ObservationsSummary,
    recalls: &ArchiveRecalls,
    has_proxy: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    if deltas.views > 0 {
        lines.push(deltas.line());
    }
    if outlines.views > 0 {
        lines.push(outlines.line());
    }
    if graph.calls > 0 {
        lines.push(graph.line());
    }
    if observations.archived_results > 0 && !has_proxy {
        lines.push(observations.line(recalls));
    }
    lines
}

#[allow(clippy::too_many_arguments)]
fn build_stats_json(
    workspace: &Workspace,
    data: StatsData,
    activity: &Activity,
    recalls: &ArchiveRecalls,
    proxy: Option<serde_json::Value>,
    codex: Option<&serde_json::Value>,
    agy: Option<&serde_json::Value>,
    claude_code_sessions: Option<serde_json::Value>,
    outlines: &OutlinesSummary,
    deltas: &DeltasSummary,
    observations: &ObservationsSummary,
) -> serde_json::Value {
    let count = data.count;
    let (raw, preview, saved, net) = (
        data.totals.raw_bytes,
        data.totals.preview_bytes,
        data.totals.initial(),
        data.totals.net_saved_bytes,
    );
    // The keys from before retrievals were measured keep their meaning:
    // `saved_*` and `estimated_tokens_saved` are savings at first preview.
    let mut report = serde_json::json!({
        "runs": count,
        "skipped_runs": data.skipped_runs,
        "raw_bytes": raw,
        "preview_bytes": preview,
        "saved_bytes": saved,
        "saved_percent": rounded(percent(saved, raw)),
        "estimated_tokens_saved": data.estimate.saved_tokens(),
        "replayed_bytes": data.replayed.measured,
        "estimated_replay_bytes": data.replayed.estimated,
        "estimated_replays": data.replayed.estimated_replays,
        "unmeasured_retrievals": data.replayed.unmeasured,
        "net_saved_bytes": net,
        "net_saved_percent": rounded(signed_percent(net, raw)),
        "estimated_tokens_net_saved": data.estimate.net_tokens(),
        "token_estimate": crate::tokens::method(),
        "estimate_by_content": data.estimate.json(),
        "days": data.timeline.into_values().collect::<Vec<_>>(),
        "commands": data.commands,
        "views": data.views,
        "repeated_source_reads": activity.repeat_reads,
        "changed_since_read": deltas.json(),
        "source_outlines": outlines.json(),
        "graph": activity.graph.json(),
        "observations": observations.json(recalls),
        "redacted_secrets": data.secrets,
        "unredacted_replays": data.unredacted,
        "hook_pass_through": activity.bypasses,
        "proxy": proxy,
        "archive_recalls": recalls.json(),
        "preview_budgets": crate::adaptive::stats_json(workspace),
        "codex": codex,
        "agy": agy,
    });
    if let Some(sessions_json) = claude_code_sessions {
        report["claude_code_sessions"] = sessions_json;
    }
    report
}

pub fn stats(workspace: &Workspace, days: u64, as_json: bool, sessions: bool) -> Result<()> {
    let since = now_ms().saturating_sub(days.saturating_mul(86_400_000));
    let data = collect_stats(workspace, since)?;
    let activity: Activity = get_or_warn(&workspace.state.join("activity.json"));
    let recalls = archive_recalls(&workspace.state, since);
    let proxy = crate::proxy::report(workspace, since, recalls.proxy_bytes)?;
    let codex = crate::rollouts::stats(workspace, since);
    let agy = crate::agy_sessions::stats(workspace, since);
    let usage = sessions
        .then(|| crate::tokens::sessions(&workspace.root, since))
        .flatten();
    let outlines = OutlinesSummary::from_activity(&activity.outlines);
    let deltas = DeltasSummary::from_activity(&activity.deltas);
    let observations = observations_summary(workspace);
    if as_json {
        let report = build_stats_json(
            workspace,
            data,
            &activity,
            &recalls,
            proxy.as_ref().map(|p| p.json()),
            codex.as_ref().map(|codex| &codex.json),
            agy.as_ref().map(|agy| &agy.json),
            sessions.then(|| crate::tokens::sessions_json(usage.as_ref(), days)),
            &outlines,
            &deltas,
            &observations,
        );
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let count = data.count;
    let (raw, saved, net) = (
        data.totals.raw_bytes,
        data.totals.initial(),
        data.totals.net_saved_bytes,
    );
    if count == 0 {
        let justless = workspace.justless();
        println!(
            "No captured runs yet. Run a command with `{justless} run -- COMMAND`, install hooks with `{justless} init --hooks`, or check setup with `{justless} doctor`."
        );
        if data.skipped_runs > 0 {
            println!(
                "Skipped {} unreadable run record(s); run `{justless} doctor` to check local state.",
                data.skipped_runs
            );
        }
    } else {
        println!(
            "Kept {} of {} command output out of context at first preview ({:.1}%) across {count} run(s)",
            human_bytes(saved),
            human_bytes(raw),
            percent(saved, raw)
        );
        let net_saved = format!(
            "net saved: {} ({:.1}%)",
            signed_bytes(net),
            signed_percent(net, raw)
        );
        if data.replayed.bytes() == 0 && data.replayed.unmeasured == 0 {
            println!("No run output was retrieved again, so {net_saved}");
        } else {
            let estimate = if data.replayed.estimated_replays > 0 {
                format!(
                    " and about {} estimated for {} whole replay(s) recorded without their size",
                    human_bytes(data.replayed.estimated),
                    data.replayed.estimated_replays
                )
            } else {
                String::new()
            };
            println!(
                "Later replays and lookups printed {} of run output again{estimate}, so {net_saved}",
                human_bytes(data.replayed.measured)
            );
        }
        if data.replayed.unmeasured > 0 {
            println!(
                "{} earlier range, search or context request(s) were recorded without their size and are not subtracted",
                data.replayed.unmeasured
            );
        }
        println!(
            "≈{} tokens saved at first preview, ≈{} net (estimated at {})",
            data.estimate.saved_tokens(),
            data.estimate.net_tokens(),
            data.estimate.basis()
        );
        if data.secrets > 0 {
            println!("Redacted {} secret(s) in captured output", data.secrets);
        }
        if data.skipped_runs > 0 {
            println!(
                "Skipped {} unreadable run record(s); run `{} doctor` to check local state.",
                data.skipped_runs,
                workspace.justless()
            );
        }
        if !data.timeline.is_empty() {
            println!("\nLast {days} day(s), UTC, with retrievals on the day their run completed");
            let peak = data
                .timeline
                .values()
                .map(|d| d.totals.net_saved_bytes)
                .max()
                .unwrap_or(0)
                .max(1) as u64;
            for d in data.timeline.values() {
                let t = &d.totals;
                let bar = t.net_saved_bytes.max(0) as u64;
                let width = (bar * 20).div_ceil(peak) as usize;
                let row = format!(
                    "  {}  {:>4} run(s)  {:>9} → {:<9} {:>5.1}%  net {:>6.1}%  {}",
                    d.date,
                    d.runs,
                    human_bytes(t.raw_bytes),
                    human_bytes(t.preview_bytes),
                    percent(t.initial(), t.raw_bytes),
                    signed_percent(t.net_saved_bytes, t.raw_bytes),
                    "█".repeat(width)
                );
                println!("{}", row.trim_end());
            }
        }
        println!("\nBy command, most net savings first");
        let wide = data
            .commands
            .iter()
            .map(|f| f.family.chars().count())
            .max()
            .unwrap_or(0)
            .min(28);
        for f in &data.commands {
            let t = &f.totals;
            let mut notes = Vec::new();
            if f.reopened_runs > 0 {
                notes.push(format!("{}/{} reopened in full", f.reopened_runs, f.runs));
            } else {
                notes.push("0 reopened".to_owned());
            }
            if f.targeted_lookups > 0 {
                notes.push(format!("{} targeted lookup(s)", f.targeted_lookups));
            }
            if f.excerpts > 0 {
                notes.push(format!("{} log excerpt(s)", f.excerpts));
            }
            let later = t.replayed_bytes + t.estimated_replay_bytes;
            if later > 0 {
                notes.push(format!(
                    "{}{} retrieved later",
                    if t.estimated_replay_bytes > 0 {
                        "about "
                    } else {
                        ""
                    },
                    human_bytes(later)
                ));
            }
            if f.unchanged_runs > 0 {
                notes.push(format!("{} unchanged", f.unchanged_runs));
            }
            if f.grown_runs > 0 {
                notes.push(format!("{} grew", f.grown_runs));
            }
            let family = crate::summary::short(&f.family, 28);
            println!(
                "  {family:<wide$}  {:>4} run(s)  {:>9} → {:<9} {:>5.1}%  net {:>6.1}%  · {}",
                f.runs,
                human_bytes(t.raw_bytes),
                human_bytes(t.preview_bytes),
                percent(t.initial(), t.raw_bytes),
                signed_percent(t.net_saved_bytes, t.raw_bytes),
                notes.join(" · ")
            );
        }
        if !data.views.is_empty() {
            println!("\nBy view, most net savings first");
            let view_wide = data.views.iter().map(|v| v.label.len()).max().unwrap_or(0);
            for v in &data.views {
                let t = &v.totals;
                let reopen = if v.reopened_runs > 0 {
                    format!("{}/{} reopened in full", v.reopened_runs, v.runs)
                } else {
                    "0 reopened".to_owned()
                };
                let tokens = if v.estimated_tokens_net_saved != 0 {
                    format!(" · ≈{} net tokens est.", v.estimated_tokens_net_saved)
                } else {
                    String::new()
                };
                println!(
                    "  {:<view_wide$}  {:>4} run(s)  {:>9} → {:<9} {:>5.1}%  net {:>6.1}%  · {reopen}{tokens}",
                    v.label,
                    v.runs,
                    human_bytes(t.raw_bytes),
                    human_bytes(t.preview_bytes),
                    percent(t.initial(), t.raw_bytes),
                    signed_percent(t.net_saved_bytes, t.raw_bytes),
                );
            }
        }
        crate::adaptive::print_stats(workspace);
    }
    let reopened: u64 = data.commands.iter().map(|f| f.reopened_runs).sum();
    let targeted: u64 = data.commands.iter().map(|f| f.targeted_lookups).sum();
    let excerpts: u64 = data.commands.iter().map(|f| f.excerpts).sum();
    if count > 0 || activity.repeat_reads > 0 {
        println!(
            "\nRecovery: {reopened} run(s) reopened in full · {targeted} targeted lookup(s){} · {} repeated source-range read(s){}",
            if excerpts > 0 {
                format!(" · {excerpts} log excerpt(s)")
            } else {
                String::new()
            },
            activity.repeat_reads,
            if data.unredacted > 0 {
                format!(" · {} unredacted replay(s)", data.unredacted)
            } else {
                String::new()
            }
        );
    }
    for line in activity_summary_lines(
        &deltas,
        &outlines,
        &activity.graph,
        &observations,
        &recalls,
        proxy.is_some(),
    ) {
        println!("{line}");
    }
    if !activity.bypasses.is_empty() {
        let passed: Vec<_> = activity
            .bypasses
            .iter()
            .map(|(family, n)| format!("{family} ×{n}"))
            .collect();
        println!(
            "Hooks passed through: {}. Run `{} discover` to see what agents ran without Justless.",
            passed.join(", "),
            workspace.justless()
        );
    }
    if let Some(codex) = &codex {
        println!("{}", codex.line);
    }
    if let Some(agy) = &agy {
        println!("{}", agy.line);
    }
    if let Some(proxy) = &proxy {
        proxy.print(days, &workspace.justless());
    }
    if recalls.count > 0 {
        println!(
            "\nArchived tool results printed by `justless recall`, last {days} day(s): {}, {} back in context.",
            recalls.count,
            human_bytes(recalls.bytes)
        );
    }
    if sessions {
        crate::tokens::print_sessions(usage.as_ref(), days);
    }
    Ok(())
}

pub fn history(
    workspace: &Workspace,
    needle: Option<&str>,
    oldest: bool,
    limit: usize,
    cap: usize,
) -> Result<()> {
    let runs = records(workspace, oldest)?;
    if let Some(needle) = needle {
        return search_runs(workspace, runs, needle, oldest, limit, cap);
    }
    let mut report = Report::new(cap);
    let justless = workspace.justless();
    let mut count = 0;
    for run in runs {
        let run = run?;
        report.line(format!(
            "{} · {} · {} · {} (epoch ms {})",
            run.id,
            command(&run),
            outcome(&run, &justless),
            age(run.completed_ms),
            run.completed_ms
        ));
        count += 1;
        if count >= limit || report.clipped {
            break;
        }
    }
    if count == 0 {
        report.line("No matching captured runs.");
    }
    report.finish()
}

/// Lists the lines that contain `needle` in `runs`, in their order, up to
/// `limit` and within `cap` bytes. Every run is searched to the end, so a last
/// line can count the matches left out. Within a run that the limit cuts,
/// each stream keeps a place and a failed run's failure lines come first, as
/// in `search`. The first failed run listed keeps a place when the runs before
/// it passed, and so does a stream whose matches all lie past the cut.
fn search_runs(
    workspace: &Workspace,
    runs: impl Iterator<Item = Result<Run>>,
    needle: &str,
    oldest: bool,
    limit: usize,
    cap: usize,
) -> Result<()> {
    let mut found = Vec::new();
    let mut earlier = 0;
    for run in runs {
        let run = run?;
        let directory = workspace.state.join("runs").join(&run.id);
        // A run past the limit keeps only the first line of each kind, which
        // a stream left out may need.
        let keep = limit.saturating_sub(earlier).max(1);
        match capture::Found::scan(
            &directory,
            Stream::select("both"),
            needle,
            run.may_have_failed(),
            keep,
        ) {
            Ok(matches) if matches.total() > 0 => {
                earlier += matches.total();
                found.push((run, matches));
            }
            Ok(_) => {}
            Err(error) => skipped(&run.id, &error),
        }
    }
    let mut all = capture::Tally::default();
    for (_, matches) in &found {
        all.add(&matches.tally());
    }
    let total = all.total();
    if total == 0 {
        let mut report = Report::new(cap);
        report.line("No matching captured runs.");
        return report.finish();
    }
    let failed = found.iter().position(|(run, _)| run.may_have_failed());
    // The lines for `want` places. Each run gets the places the runs before it
    // left. When that leaves out every match of the runs that failed, such as
    // an older failure under a newer passing run's matches, the first failed
    // run shows its best line in the place of the last line listed. When a
    // stream's matches are all left out, such as an older failed run's stderr
    // under a newer run's stdout, the first run with one shows it in the place
    // of the last line listed before that.
    let lines = |want: usize| {
        let mut places = Vec::with_capacity(found.len());
        let mut left = want;
        for (_, matches) in &found {
            let take = left.min(matches.total());
            places.push(take);
            left -= take;
        }
        let cut = want < total && want >= 2;
        let mut reserved = None;
        if cut
            && let Some(failed) = failed
            && places[failed] == 0
            && let Some(last) = places.iter().rposition(|&take| take > 0)
        {
            places[last] -= 1;
            places[failed] = 1;
            reserved = Some(failed);
        }
        let mut pinned = Vec::new();
        if cut {
            for stream in [Stream::Stdout, Stream::Stderr] {
                let listed = found.iter().zip(&places).any(|((_, matches), &take)| {
                    matches.choose(take).iter().any(|line| line.0 == stream)
                });
                let first = found
                    .iter()
                    .enumerate()
                    .find_map(|(index, (_, matches))| Some((index, matches.first(stream)?)));
                let last = (0..places.len())
                    .rev()
                    .find(|&index| places[index] > 0 && Some(index) != reserved);
                if let (false, Some(first), Some(last)) = (listed, first, last) {
                    places[last] -= 1;
                    pinned.push(first);
                }
            }
        }
        let mut lines = Vec::new();
        for (index, ((run, matches), &take)) in found.iter().zip(&places).enumerate() {
            let mut chosen = matches.choose(take);
            for (_, line) in pinned.iter().filter(|(pinned, _)| *pinned == index) {
                if !chosen.contains(line) {
                    chosen.push(*line);
                }
            }
            for (stream, n, excerpt) in chosen {
                let line = format!(
                    "[{}] {} {} {}:{n}: {excerpt}",
                    age(run.completed_ms),
                    run.id,
                    run.family,
                    stream.name()
                );
                lines.push((index, stream, line));
            }
        }
        lines
    };
    // The notice in its fullest form first. When that leaves no room for a
    // match, as under a small --budget, the run count and the hint are dropped,
    // and then the counts per stream.
    let notice = |form: usize, tally: &capture::Tally, runs: usize, budget: bool| {
        let shown = tally.shown();
        if form == 2 {
            return format!("[{shown} of {total} matches shown]\n");
        }
        if form == 1 {
            return format!("[{shown} of {total} matches shown ({tally})]\n");
        }
        // MCP takes no `limit` above its cap, so past it the hint narrows
        // the search or turns it around instead.
        let hint = match (total > limit, budget) {
            (true, _) if total > MAX_LIMIT && oldest => {
                "narrow TEXT, or drop --oldest for the newest first".to_owned()
            }
            (true, _) if total > MAX_LIMIT => {
                "narrow TEXT, or use --oldest for the oldest first".to_owned()
            }
            (true, true) => format!("--limit {total} and a larger --budget show all"),
            (true, false) => format!("--limit {total} shows all"),
            _ => "a larger --budget shows all".to_owned(),
        };
        format!(
            "[{shown} of {total} matches shown from {runs} of {} matching runs ({tally}); {hint}]\n",
            found.len()
        )
    };
    // The output with one form of the notice, and the matches it shows.
    let attempt = |form: usize| {
        // Room for the notice, set aside once matches are left out. Every
        // number in it is at most the one in this form.
        let widest = notice(form, &all.every_shown(), found.len(), true).len();
        let mut want = limit.min(total);
        let mut room = if want < total { widest } else { 0 };
        loop {
            let mut report = Report::new(cap.saturating_sub(room));
            let mut tally = all;
            let mut listed = Vec::new();
            let mut shown = 0;
            let mut printed = BTreeMap::new();
            for (index, stream, line) in lines(want) {
                let before = report.left();
                report.line(line);
                let bytes = before - report.left();
                if bytes > 0 {
                    *printed.entry(found[index].0.id.clone()).or_insert(0u64) += bytes as u64;
                }
                if report.clipped {
                    break;
                }
                tally.show(stream);
                if listed.last() != Some(&index) {
                    listed.push(index);
                }
                shown += 1;
            }
            // When the budget leaves out lines the limit allowed, the lines
            // that fit are chosen again, so that cut also keeps each stream's
            // place and puts a failure first.
            if report.clipped && room == 0 {
                room = widest;
                continue;
            }
            if report.clipped && shown > 0 && shown < want {
                want = shown;
                continue;
            }
            let budget = shown < limit.min(total);
            // Lines chosen again to fit still mark the budget as reached.
            report.clipped |= budget;
            let mut output = report.into_string();
            if tally.shown() < total {
                output.push_str(&notice(form, &tally, listed.len(), budget));
            }
            return (output, tally, printed);
        }
    };
    // The fullest notice that leaves room for matches of as many streams as
    // any form does, so a larger budget never shows fewer streams. With no
    // room for any match, the shortest.
    let covered = |tally: &capture::Tally| {
        [Stream::Stdout, Stream::Stderr]
            .into_iter()
            .filter(|&stream| tally.of(stream).0 > 0)
            .count()
    };
    let mut attempts = (0..3).map(attempt).collect::<Vec<_>>();
    let most = attempts
        .iter()
        .map(|(_, tally, _)| covered(tally))
        .max()
        .unwrap_or_default();
    let (output, _, printed) = match attempts
        .iter()
        .position(|(_, tally, _)| most > 0 && covered(tally) == most)
    {
        Some(form) => attempts.swap_remove(form),
        None => attempts.pop().unwrap_or_default(),
    };
    io::stdout().lock().write_all(output.as_bytes())?;
    excerpts(workspace, &printed);
    Ok(())
}

/// How lists name a run: its command label, or the output family when that is
/// more specific, as for `kubectl get pods`, or adds detail, as for `pytest`.
fn command(run: &Run) -> String {
    let (label, family) = (run.label.as_str(), run.family.as_str());
    if label.is_empty() || family.starts_with(label) {
        family.to_owned()
    } else if matches!(family, "other" | "legacy") || label.starts_with(family) {
        label.to_owned()
    } else {
        format!("{label} ({family})")
    }
}

pub fn resume(workspace: &Workspace, cap: usize) -> Result<()> {
    print!("{}", activity_text(workspace, cap)?);
    Ok(())
}

/// The compaction handoff: [`activity_text`] for the summary or the restarted
/// session. The hook also forgets the symbols `context` and `pack` remember for
/// the session (see `crate::seen::forget`), since compaction drops the source the
/// agent was shown.
pub fn resume_text(workspace: &Workspace, cap: usize) -> Result<String> {
    activity_text(workspace, cap)
}

/// Whether `resume` has recorded runs, reads or a pack to show.
pub(crate) fn has_activity(workspace: &Workspace) -> bool {
    let activity: Activity = get_or_warn(&workspace.state.join("activity.json"));
    !activity.reads.is_empty() || !activity.last_pack.is_empty() || has_any_run(workspace)
}

/// Checks whether at least one readable run exists, stopping at the first match
/// instead of reading and sorting the whole run archive.
fn has_any_run(workspace: &Workspace) -> bool {
    let Ok(entries) = fs::read_dir(workspace.state.join("runs")) else {
        return false;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let Some(id) = name.to_str() else {
            continue;
        };
        if !valid_run_id(id) {
            continue;
        }
        let directory = entry.path();
        match fs::File::open(directory.join("record.json")) {
            Ok(file) => {
                if serde_json::from_reader::<_, SummaryRecord>(BufReader::new(file))
                    .is_ok_and(|record| record.id == id)
                {
                    return true;
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                if fs::metadata(directory.join("stdout"))
                    .and_then(|meta| meta.modified())
                    .is_ok()
                {
                    return true;
                }
            }
            Err(_) => {}
        }
    }
    false
}

/// Recent runs, inspected source and the last pack, with stale source marked.
/// Returns `None` inside the text when the project has no recorded activity.
fn activity_text(workspace: &Workspace, cap: usize) -> Result<String> {
    let mut report = Report::new(cap);
    let activity: Activity = get_or_warn(&workspace.state.join("activity.json"));
    let justless = workspace.justless();
    let mut has_runs = false;
    let mut redactor: Option<Option<&'static crate::redact::Redactor>> = None;
    let mut digests: HashMap<String, Option<String>> = HashMap::new();
    for run in records(workspace, false)?.take(5) {
        let run = run?;
        has_runs = true;
        report.line(format!(
            "{} · {} · {}{}{}",
            run.id,
            command(&run),
            outcome(&run, &justless),
            if run.unchanged {
                " · unchanged output"
            } else {
                ""
            },
            run.summary
                .headline()
                .map(|h| {
                    match *redactor.get_or_insert_with(|| crate::redact::for_workspace(workspace)) {
                        Some(redactor) => redactor.text(&h).into_owned(),
                        None => h,
                    }
                })
                .map(|h| format!(" · {}", crate::summary::short(&h, 160)))
                .unwrap_or_default()
        ));
        for d in run.diagnostics.iter().take(2) {
            let stale = run
                .source_digests
                .get(&d.location.file)
                .is_some_and(|old| !current_cached(workspace, &mut digests, &d.location.file, old));
            report.line(format!(
                "  {}:{} -> {}{}",
                d.stream.name(),
                d.log_line,
                d.location,
                if stale { " [source changed]" } else { "" }
            ));
        }
    }
    // Source ranges and the last pack share the space the runs leave. The pack
    // keeps up to half of it, so neither list crowds out the other. A line that
    // does not fit whole is left out, and files are hashed only while their
    // lines can still fit.
    let room = report.left();
    let mut pack = Vec::new();
    let mut pack_bytes = 0;
    for (file, digest) in &activity.last_pack {
        if pack_bytes >= room {
            break;
        }
        let line = format!(
            "Last pack: {file}{}",
            if current_cached(workspace, &mut digests, file, digest) {
                ""
            } else {
                " [source changed]"
            }
        );
        pack_bytes += line.len() + 1;
        pack.push(line);
    }
    let reserve = pack_bytes.min(room / 2);
    let mut dropped = pack.len() < activity.last_pack.len();
    // Activity keeps the 32 most recent ranges and outlines, newest first.
    // Multiple non-outline `Read` ranges on the same file with the same
    // staleness coalesce onto one compact line (`Read file:10-40, 120-160`),
    // keeping each file's position at its most recent read and preserving
    // `[source changed]` accurately per range.
    if report.left() <= reserve {
        dropped |= !activity.reads.is_empty();
    } else {
        let items = coalesce_reads(workspace, &activity.reads, &mut digests);
        for item in items {
            if report.clipped {
                break;
            }
            let avail = report.left().saturating_sub(reserve);
            match item {
                ResumeRead::Outline {
                    file,
                    from,
                    to,
                    stale,
                } => {
                    let suffix = if stale { " [source changed]" } else { "" };
                    let line = if from > 0 {
                        format!("Outline {file}:{from}-{to}{suffix}")
                    } else {
                        format!("Outline {file}{suffix}")
                    };
                    if line.len() + 1 > avail {
                        dropped = true;
                        break;
                    }
                    report.line(line);
                }
                ResumeRead::Ranges {
                    file,
                    stale,
                    ranges,
                } => {
                    let full = format_read_ranges(file, &ranges, stale);
                    if full.len() < avail {
                        report.line(full);
                        continue;
                    }
                    // When the budget is too tight for every range on this file,
                    // keep as many newest ranges as fit and mark the rest clipped.
                    let fit = (1..ranges.len()).rev().find_map(|keep| {
                        let line = format_read_ranges(file, &ranges[..keep], stale);
                        (line.len() < avail).then_some(line)
                    });
                    if let Some(line) = fit {
                        report.line(line);
                    }
                    dropped = true;
                    break;
                }
            }
        }
    }
    for line in pack {
        if line.len() + 1 > report.left() {
            dropped = true;
            break;
        }
        report.line(line);
    }
    report.clipped |= dropped;
    if !has_runs && activity.reads.is_empty() && activity.last_pack.is_empty() {
        report.line("No recorded project activity yet.");
    }
    Ok(report.into_string())
}

enum ResumeRead<'a> {
    Outline {
        file: &'a str,
        from: usize,
        to: usize,
        stale: bool,
    },
    Ranges {
        file: &'a str,
        stale: bool,
        /// Inspected `(from, to)` line spans in newest-first order.
        ranges: Vec<(usize, usize)>,
    },
}

fn coalesce_reads<'a>(
    workspace: &Workspace,
    reads: &'a [Read],
    digests: &mut HashMap<String, Option<String>>,
) -> Vec<ResumeRead<'a>> {
    let mut items: Vec<ResumeRead<'a>> = Vec::new();
    let mut groups: HashMap<(&'a str, bool), usize> = HashMap::new();
    for read in reads.iter().take(32) {
        let stale = !current_cached(workspace, digests, &read.file, &read.digest);
        if read.outline {
            items.push(ResumeRead::Outline {
                file: &read.file,
                from: read.from,
                to: read.to,
                stale,
            });
            continue;
        }
        let key = (read.file.as_str(), stale);
        if let Some(&index) = groups.get(&key) {
            if let ResumeRead::Ranges { ranges, .. } = &mut items[index]
                && !ranges.contains(&(read.from, read.to))
            {
                ranges.push((read.from, read.to));
            }
        } else {
            groups.insert(key, items.len());
            items.push(ResumeRead::Ranges {
                file: &read.file,
                stale,
                ranges: vec![(read.from, read.to)],
            });
        }
    }
    items
}

fn format_read_ranges(file: &str, ranges: &[(usize, usize)], stale: bool) -> String {
    let mut sorted = ranges.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out =
        String::with_capacity(6 + file.len() + sorted.len() * 12 + usize::from(stale) * 17);
    let _ = write!(out, "Read {file}:");
    for (i, (from, to)) in sorted.into_iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let _ = write!(out, "{from}-{to}");
    }
    if stale {
        out.push_str(" [source changed]");
    }
    out
}

fn current_cached(
    workspace: &Workspace,
    cache: &mut HashMap<String, Option<String>>,
    file: &str,
    old: &str,
) -> bool {
    if let Some(cached) = cache.get(file) {
        return cached.as_deref() == Some(old);
    }
    let digest = workspace
        .source_path(file)
        .ok()
        .and_then(|p| fs::read(p).ok())
        .map(|b| capture::source_digest(&b));
    let matches = digest.as_deref() == Some(old);
    cache.insert(file.to_owned(), digest);
    matches
}

/// `justless` starts the command that replays an unfinished run.
fn outcome(run: &Run, justless: &str) -> String {
    if run.unfinished {
        format!("unfinished, output so far in {justless} log")
    } else {
        // A run the Bash post-command hook recorded from a pipe or a list holds the
        // whole line's status, and says so.
        format!("exit {}", crate::postroute::exit_status(run))
    }
}

/// How long a run directory without a record or a lock can still be starting.
/// Justless locks a new run before it writes the run's first record.
const STARTING_MS: u64 = 60 * 1000;
/// How long an unfinished run without a lock file, recorded where locks are
/// unavailable or by an earlier Justless, counts as still in progress.
const UNLOCKED_MS: u64 = 24 * 60 * 60 * 1000;

/// Whether another Justless process is still recording this run. That process
/// holds the run's lock for as long as it lives, so a free lock means it was killed
/// and the run is pruned like any other. A finished run has no lock file.
fn in_flight(directory: &std::path::Path, age_ms: u64) -> bool {
    #[derive(Deserialize)]
    struct State {
        #[serde(default)]
        unfinished: bool,
    }
    let record = directory.join("record.json");
    match capture::recording(directory) {
        // The first record is written once the lock is held, so a free lock without
        // a record can belong to a run that is just starting.
        Some(recording) => recording || age_ms < STARTING_MS && !record.exists(),
        // Only a recent run can still be in progress, so older records stay unread.
        None => {
            age_ms < UNLOCKED_MS
                && match fs::read(&record) {
                    Ok(bytes) => {
                        serde_json::from_slice::<State>(&bytes).is_ok_and(|state| state.unfinished)
                    }
                    Err(_) => age_ms < STARTING_MS,
                }
        }
    }
}

struct Stored {
    id: String,
    modified: u64,
    referenced: bool,
    files: Vec<(u64, u64, u64)>,
}

/// Delete the oldest runs beyond `max_runs` or `max_bytes`. The current run and runs
/// that another live Justless process is still recording are always kept, and runs
/// that a rerun compares against go last. The caller holds the capture lock.
/// Returns the number of deleted runs.
pub fn prune(workspace: &Workspace, current: &str, max_runs: usize, max_bytes: u64) -> usize {
    let runs = workspace.state.join("runs");
    let Ok(entries) = fs::read_dir(&runs) else {
        return 0;
    };
    let baselines: BTreeMap<String, Vec<std::path::PathBuf>> =
        fs::read_dir(workspace.state.join("baselines"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let id = fs::read_to_string(entry.path()).ok()?;
                Some((id.trim().to_owned(), entry.path()))
            })
            .fold(BTreeMap::new(), |mut map, (id, path)| {
                map.entry(id).or_default().push(path);
                map
            });
    let now = now_ms();
    let mut stored = Vec::new();
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        if !id.starts_with("run-") || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let directory = entry.path();
        let modified = fs::metadata(directory.join("record.json"))
            .or_else(|_| fs::metadata(&directory))
            .and_then(|meta| meta.modified())
            .map_or(now, millis);
        if id != current && in_flight(&directory, now.saturating_sub(modified)) {
            continue;
        }
        let files = fs::read_dir(&directory)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|file| file.metadata().ok())
            .map(|meta| file_identity(&meta))
            .collect();
        stored.push(Stored {
            referenced: baselines.contains_key(&id),
            id,
            modified,
            files,
        });
    }
    // Keep the current run, then runs that reruns compare against, then the newest.
    stored.sort_by_key(|run| {
        (
            run.id != current,
            !run.referenced,
            std::cmp::Reverse(run.modified),
        )
    });
    // Files that kept runs hold. A file shared through hard links stays on disk while
    // any kept run links it, so it counts once, against the first run kept with it.
    // A deleted run holds nothing, so an older run that shares its file pays for it.
    let mut held = std::collections::BTreeSet::new();
    let (mut count, mut bytes, mut deleted) = (0, 0, 0);
    for run in stored {
        let added: Vec<_> = run
            .files
            .iter()
            .filter(|(device, inode, _)| *inode == 0 || !held.contains(&(*device, *inode)))
            .collect();
        let size: u64 = added.iter().map(|(_, _, size)| size).sum();
        let keep = run.id == current || (count < max_runs && bytes + size <= max_bytes);
        if !keep && fs::remove_dir_all(runs.join(&run.id)).is_ok() {
            deleted += 1;
            for baseline in baselines.get(&run.id).into_iter().flatten() {
                let _ = fs::remove_file(baseline);
            }
            continue;
        }
        count += 1;
        bytes += size;
        held.extend(added.iter().map(|(device, inode, _)| (*device, *inode)));
    }
    deleted
}

fn file_identity(meta: &fs::Metadata) -> (u64, u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino(), meta.len())
    }
    #[cfg(not(unix))]
    {
        (0, 0, meta.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_workspace() -> (TempDir, Workspace) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ws = Workspace::discover(Some(&root)).unwrap();
        (dir, ws)
    }

    #[test]
    fn resume_coalesces_same_file_ranges_preserving_recency_and_staleness() {
        let (_dir, ws) = temp_workspace();
        let agents_body = "fn a() {}\n".repeat(500);
        let history_body = "fn h() {}\n".repeat(100);
        fs::create_dir_all(ws.root.join("src")).unwrap();
        fs::write(ws.root.join("src/agents.rs"), &agents_body).unwrap();
        fs::write(ws.root.join("src/history.rs"), &history_body).unwrap();
        let agents_digest = capture::source_digest(agents_body.as_bytes());
        let history_digest = capture::source_digest(history_body.as_bytes());

        // Older stale ranges, an outline, another file, and three current ranges
        // on `src/agents.rs` (interleaved with `src/history.rs`).
        saw_source(&ws, "src/agents.rs", 60, 80, "old-digest".into());
        saw_source(&ws, "src/agents.rs", 1, 5, "old-digest".into());
        saw_outline(&ws, "src/agents.rs", agents_digest.clone());
        saw_source(&ws, "src/agents.rs", 10, 40, agents_digest.clone());
        saw_source(&ws, "src/history.rs", 5, 20, history_digest);
        saw_source(&ws, "src/agents.rs", 120, 160, agents_digest.clone());
        saw_source(&ws, "src/agents.rs", 400, 440, agents_digest);

        let text = resume_text(&ws, 4000).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec![
                "Read src/agents.rs:10-40, 120-160, 400-440",
                "Read src/history.rs:5-20",
                "Outline src/agents.rs",
                "Read src/agents.rs:1-5, 60-80 [source changed]",
            ],
            "{text}"
        );
    }

    #[test]
    fn resume_coalescing_frees_budget_and_trims_oldest_ranges_when_tight() {
        let (_dir, ws) = temp_workspace();
        fs::create_dir_all(ws.root.join("src")).unwrap();
        let body = "pub fn line() {}\n".repeat(1000);
        for name in ["alpha_module.rs", "beta_module.rs", "gamma_module.rs"] {
            let path = format!("src/{name}");
            fs::write(ws.root.join(&path), &body).unwrap();
            let digest = capture::source_digest(body.as_bytes());
            for block in 0..8 {
                let from = block * 50 + 1;
                let to = from + 30;
                saw_source(&ws, &path, from, to, digest.clone());
            }
        }

        // 24 distinct ranges across 3 files collapse to 3 lines, well under 1800 bytes.
        let text = resume_text(&ws, 1800).unwrap();
        assert!(text.len() <= 1800, "len={}", text.len());
        assert!(!text.contains("[context budget reached]"), "{text}");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].starts_with("Read src/gamma_module.rs:1-31, 51-81, "));
        assert!(lines[2].starts_with("Read src/alpha_module.rs:1-31, 51-81, "));

        // Under a tight cap smaller than a full 8-range line, keep newest ranges
        // that fit and emit the budget marker instead of dropping the whole file.
        let tight = resume_text(&ws, 85).unwrap();
        assert!(tight.len() <= 85, "tight len={}: {tight}", tight.len());
        assert!(tight.contains("Read src/gamma_module.rs:"), "{tight}");
        assert!(tight.contains("351-381"), "newest range missing: {tight}");
        assert!(tight.ends_with("[context budget reached]\n"), "{tight}");
    }

    #[test]
    fn label_unwraps_wave2_wrappers_and_keeps_allowlisted_subcommands() {
        for (words, expected) in [
            (&["timeout", "60", "cargo", "test"][..], "cargo test"),
            (&["nice", "-n", "5", "bazel", "test", "//..."], "bazel test"),
            (&["stdbuf", "-oL", "zig", "build"], "zig build"),
            (
                &["env", "RUST_BACKTRACE=1", "podman", "images"],
                "podman images",
            ),
            (
                &["python3", "-m", "unittest", "discover"],
                "python3 -m unittest",
            ),
            (&["python", "-m", "pytest", "tests/"], "python -m pytest"),
            (
                &["kubectl", "get", "events", "-n", "secret-ns"],
                "kubectl get events",
            ),
            (
                &["kubectl", "get", "svc", "my-secret-svc"],
                "kubectl get svc",
            ),
            (&["kubectl", "get", "secrets", "my-secret"], "kubectl get"),
            (&["sh", "-c", "echo secret-arg"], "sh"),
        ] {
            assert_eq!(label(words), expected, "{words:?}");
        }
    }

    #[test]
    fn deltas_and_observations_summaries_measure_bytes_and_label_estimates() {
        let (_dir, ws) = temp_workspace();
        saw_delta(&ws, 3, 12_000, 1_500);
        let deltas = deltas_summary(&ws);
        assert_eq!(deltas.views, 3);
        assert_eq!(deltas.saved_bytes(), 10_500);
        let deltas_json = deltas.json();
        assert_eq!(deltas_json["saved_bytes"], 10_500);
        assert!(deltas_json["estimated_tokens_saved"].as_u64().unwrap() > 0);
        let deltas_line = deltas.line();
        assert!(deltas_line.contains("Changed since read: 3 view(s)"));
        assert!(deltas_line.contains("estimated at"));
        assert!(!deltas_line.contains("÷ 4"));

        // Write two valid 64-hex observation archives plus decoy entries that must be ignored.
        let obs_dir = ws.state.join("observations");
        fs::create_dir_all(obs_dir.join("nested")).unwrap();
        let id_a = format!("{}.json", "a".repeat(64));
        let id_b = format!("{}.json", "1".repeat(64));
        fs::write(obs_dir.join(id_a), "x".repeat(4_096)).unwrap();
        fs::write(obs_dir.join(id_b), "y".repeat(2_048)).unwrap();
        fs::write(obs_dir.join("not-a-hash.json"), "z".repeat(9_999)).unwrap();
        fs::write(
            obs_dir
                .join("nested")
                .join(format!("{}.json", "b".repeat(64))),
            "w",
        )
        .unwrap();

        recalled_archive(&ws, 1_024, false);
        let recalls = archive_recalls(&ws.state, 0);
        assert_eq!(recalls.count, 1);
        assert_eq!(recalls.bytes, 1_024);

        let obs = observations_summary(&ws);
        assert_eq!(obs.archived_results, 2);
        assert_eq!(obs.archived_bytes, 6_144);
        let obs_json = obs.json(&recalls);
        assert_eq!(obs_json["archived_results"], 2);
        assert_eq!(obs_json["archived_bytes"], 6_144);
        assert_eq!(obs_json["recalled_bytes"], 1_024);
        assert_eq!(obs_json["net_archived_bytes"], 5_120);
        let obs_line = obs.line(&recalls);
        assert!(obs_line.contains("Archived tool results: 2 result(s) (6.0 KB"));
        assert!(obs_line.contains("1.0 KB recalled (5.0 KB net)"));
        assert!(obs_line.contains("estimated at"));
        assert!(!obs_line.contains("÷ 4"));
    }

    #[test]
    fn summary_has_activity_and_records_honor_windows_and_skip_corrupt_runs() {
        let (_dir, ws) = temp_workspace();
        assert!(!has_activity(&ws));

        let runs_dir = ws.state.join("runs");
        fs::create_dir_all(&runs_dir).unwrap();

        // Corrupt and partial records alone must not count as activity.
        for (id, body) in [
            ("run-empty", ""),
            (
                "run-partial",
                r#"{"id":"run-partial","status":0,"completed_ms":99999999999999}"#,
            ),
        ] {
            let d = runs_dir.join(id);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("record.json"), body).unwrap();
        }
        assert!(!has_activity(&ws));

        let now = now_ms();
        let since = now.saturating_sub(7 * 86_400_000);
        let write_run = |id: &str, completed_ms: u64, raw: u64, preview: usize, stdout: &str| {
            let d = runs_dir.join(id);
            fs::create_dir_all(&d).unwrap();
            let rec = serde_json::json!({
                "id": id,
                "status": 0,
                "raw_bytes": raw,
                "preview_bytes": preview,
                "footer_bytes": 0,
                "completed_ms": completed_ms,
                "family": "cargo test",
                "diagnostics": [],
                "source_digests": {},
            });
            let path = d.join("record.json");
            fs::write(&path, rec.to_string()).unwrap();
            fs::write(d.join("stdout"), stdout).unwrap();
            fs::write(d.join("stderr"), "").unwrap();
            path
        };

        // 1. Synthetic old completed_ms with fresh mtime: must still be excluded by since_ms.
        write_run("run-synthold", 1_000, 10_000, 500, "old\n");

        // 2. Old run with old mtime (> 14 days ago): skipped by mtime pre-filter.
        let old_path = write_run(
            "run-aged",
            now.saturating_sub(14 * 86_400_000),
            10_000,
            500,
            "aged\n",
        );
        let aged_time = SystemTime::now() - std::time::Duration::from_secs(14 * 86_400);
        fs::File::options()
            .write(true)
            .open(&old_path)
            .unwrap()
            .set_modified(aged_time)
            .unwrap();

        // 3. Recent uncompacted run (zero saved, zero net).
        write_run("run-small", now - 2_000, 128, 128, "small\n");

        // 4. Recent compacted code run (newest completed_ms, but backdated mtime by 100s).
        let code_sample = "pub fn value() -> usize { 42 }\n".repeat(40);
        let newest_path = write_run(
            "run-newest",
            now - 1_000,
            code_sample.len() as u64,
            100,
            &code_sample,
        );
        let backdated = SystemTime::now() - std::time::Duration::from_secs(100);
        fs::File::options()
            .write(true)
            .open(&newest_path)
            .unwrap()
            .set_modified(backdated)
            .unwrap();

        assert!(has_activity(&ws));

        // `records` must still order strictly by completed_ms despite backdated mtime.
        let ordered: Vec<String> = records(&ws, false)
            .unwrap()
            .map(|r| r.unwrap().id)
            .collect();
        assert_eq!(
            ordered,
            vec!["run-newest", "run-small", "run-aged", "run-synthold"]
        );

        // `summary` must count only the 2 in-window runs and classify code savings accurately.
        let sum = summary(&ws, since);
        assert_eq!(sum.runs, 2);
        assert_eq!(sum.raw_bytes, 128 + code_sample.len() as u64);
        assert_eq!(sum.preview_bytes, 128 + 100);
        assert_eq!(sum.saved_bytes(), code_sample.len() as u64 - 100);
        assert_eq!(
            sum.estimate.json()["code"]["saved_bytes"],
            code_sample.len() as u64 - 100
        );
    }

    #[test]
    fn stats_attributes_views_ranks_families_and_skips_corrupt_records() {
        let (_dir, ws) = temp_workspace();
        let runs_dir = ws.state.join("runs");
        fs::create_dir_all(&runs_dir).unwrap();

        // Corrupt, partial, mismatched-ID, and invalid-ID records must never panic
        // and must be counted in `skipped_runs`.
        for (id, body) in [
            ("run-empty", ""),
            (
                "run-partial",
                r#"{"id":"run-partial","status":0,"completed_ms":99999999999999}"#,
            ),
            (
                "run-mismatch",
                r#"{"id":"run-other","status":0,"raw_bytes":100,"preview_bytes":50,"diagnostics":[],"source_digests":{}}"#,
            ),
            (
                "not-a-valid-run",
                r#"{"id":"not-a-valid-run","status":0,"raw_bytes":100,"preview_bytes":50,"diagnostics":[],"source_digests":{}}"#,
            ),
        ] {
            let d = runs_dir.join(id);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("record.json"), body).unwrap();
        }

        let now = now_ms();
        let since = now.saturating_sub(7 * 86_400_000);
        let write_run = |id: &str,
                         family: &str,
                         raw: u64,
                         preview: usize,
                         unchanged: bool,
                         replaced: Option<&str>,
                         summary_kind: &str| {
            let d = runs_dir.join(id);
            fs::create_dir_all(&d).unwrap();
            let mut rec = serde_json::json!({
                "id": id,
                "status": 0,
                "raw_bytes": raw,
                "preview_bytes": preview,
                "footer_bytes": 0,
                "completed_ms": now - 1_000,
                "family": family,
                "unchanged": unchanged,
                "summary": { "kind": summary_kind },
                "diagnostics": [],
                "source_digests": {},
            });
            if let Some(r) = replaced {
                rec["replaced"] = serde_json::json!(r);
            }
            fs::write(d.join("record.json"), rec.to_string()).unwrap();
            fs::write(d.join("stdout"), "INFO: synthetic log line\n".repeat(10)).unwrap();
            fs::write(d.join("stderr"), "").unwrap();
            d
        };

        // Structured lint view (biggest net saver: 19,000 net bytes, 0 reopened).
        write_run("run-lint1", "mypy", 20_000, 1_000, false, Some("both"), "");
        // Test summary: 2 runs, 1 reopened in full for 8,000 bytes (net: 20,000 - 2,000 - 8,000 = 10,000).
        let t1 = write_run(
            "run-test1",
            "cargo test",
            10_000,
            1_000,
            false,
            Some("both"),
            "tests",
        );
        recall(&t1, Retrieval::Full, Some(8_000));
        write_run(
            "run-test2",
            "cargo test",
            10_000,
            1_000,
            false,
            Some("both"),
            "tests",
        );
        // Diff summary + unchanged rerun.
        write_run(
            "run-diff1",
            "git diff",
            6_000,
            1_000,
            false,
            Some("stdout"),
            "diff",
        );
        write_run(
            "run-diff2",
            "git diff",
            6_000,
            200,
            true,
            Some("stdout"),
            "diff",
        );
        // Clipped preview (no structured replacement) + verbatim small run.
        write_run("run-cat1", "cat", 5_000, 2_000, false, None, "");
        write_run("run-sh1", "sh", 100, 100, false, None, "");

        let data = collect_stats(&ws, since).unwrap();
        assert_eq!(data.count, 7);
        assert_eq!(data.skipped_runs, 4);
        assert_eq!(data.totals.raw_bytes, 57_100);
        assert_eq!(data.totals.preview_bytes, 6_300);
        assert_eq!(data.totals.replayed_bytes, 8_000);
        assert_eq!(data.totals.net_saved_bytes, 42_800);

        // Families are sorted strictly by net_saved_bytes descending.
        let families: Vec<&str> = data.commands.iter().map(|f| f.family.as_str()).collect();
        assert_eq!(
            families,
            vec!["mypy", "git diff", "cargo test", "cat", "sh"]
        );
        assert_eq!(data.commands[0].reopened_runs, 0);
        assert_eq!(data.commands[0].reopen_percent, 0.0);
        assert_eq!(data.commands[2].reopened_runs, 1);
        assert_eq!(data.commands[2].reopen_percent, 50.0);
        assert!(data.commands[0].estimated_tokens_net_saved > 0);

        // Views attribute every run cleanly and sum exactly to overall totals.
        let view_kinds: Vec<&str> = data.views.iter().map(|v| v.kind).collect();
        assert_eq!(
            view_kinds,
            vec![
                "structured",
                "tests",
                "unchanged",
                "diff",
                "preview",
                "verbatim"
            ]
        );
        assert_eq!(data.views.iter().map(|v| v.runs).sum::<u64>(), data.count);
        assert_eq!(
            data.views
                .iter()
                .map(|v| v.totals.net_saved_bytes)
                .sum::<i64>(),
            data.totals.net_saved_bytes
        );
        let test_view = data.views.iter().find(|v| v.kind == "tests").unwrap();
        assert_eq!(test_view.runs, 2);
        assert_eq!(test_view.reopened_runs, 1);
        assert_eq!(test_view.reopen_percent, 50.0);
        assert_eq!(
            test_view.estimated_tokens_net_saved,
            crate::tokens::Content::Logs.tokens_signed(10_000)
        );
        assert_eq!(data.timeline.len(), 1);
    }

    #[test]
    fn archive_recalls_streams_jsonl_and_skips_corrupt_or_old_lines() {
        let (_dir, ws) = temp_workspace();
        ws.prepare_state().unwrap();
        let now = now_ms();
        let since = now.saturating_sub(10_000);
        let log = format!(
            "{{\"ms\":{},\"bytes\":999,\"proxy\":false}}\n\
             \n\
             {{\"ms\":{},\"bytes\":1200,\"proxy\":false}}\n\
             {{\"ms\":{},\"bytes\":truncated\n\
             {{\"ms\":{},\"bytes\":3400,\"proxy\":true}}\n",
            now.saturating_sub(60_000),
            now.saturating_sub(2_000),
            now.saturating_sub(1_000),
            now.saturating_sub(500),
        );
        fs::write(ws.state.join(ARCHIVE_RECALLS), log).unwrap();

        let recalls = archive_recalls(&ws.state, since);
        assert_eq!(recalls.count, 2);
        assert_eq!(recalls.bytes, 4_600);
        assert_eq!(recalls.proxy_bytes, 3_400);
    }

    #[test]
    fn graph_usage_records_and_renders_in_stats_and_json() {
        let (_dir, ws) = temp_workspace();
        let alpha = "fn alpha() -> u32 { 1 }\n".repeat(40);
        let beta = "fn beta() -> u32 { 2 }\n".repeat(60);
        fs::write(ws.root.join("alpha.rs"), &alpha).unwrap();
        fs::write(ws.root.join("beta.rs"), &beta).unwrap();

        // Without `.justless/`, recording must stay silent and never create state.
        record_graph_use(&ws, "query", 200, 4_000);
        assert!(!ws.state.exists());
        assert_eq!(graph_usage(&ws), GraphUsage::default());

        let render_json = |ws: &Workspace| -> serde_json::Value {
            let since = now_ms().saturating_sub(14 * 86_400_000);
            let data = collect_stats(ws, since).unwrap();
            let activity: Activity = get(&ws.state.join("activity.json")).unwrap_or_default();
            let recalls = archive_recalls(&ws.state, since);
            let outlines = OutlinesSummary::from_activity(&activity.outlines);
            let deltas = DeltasSummary::from_activity(&activity.deltas);
            let observations = observations_summary(ws);
            build_stats_json(
                ws,
                data,
                &activity,
                &recalls,
                None,
                None,
                None,
                None,
                &outlines,
                &deltas,
                &observations,
            )
        };

        // 0-call workspaces omit the `Code graph:` line in text stats and report zeros in JSON.
        let zero_lines = activity_summary_lines(
            &DeltasSummary::default(),
            &OutlinesSummary::default(),
            &graph_usage(&ws),
            &ObservationsSummary::default(),
            &ArchiveRecalls::default(),
            false,
        );
        assert!(
            zero_lines.iter().all(|l| !l.contains("Code graph:")),
            "0-call workspace must omit Code graph line: {zero_lines:?}"
        );
        let zero_json = render_json(&ws);
        assert_eq!(zero_json["graph"]["calls"], 0);
        assert_eq!(zero_json["graph"]["shown_bytes"], 0);
        assert_eq!(zero_json["graph"]["referenced_file_bytes"], 0);
        assert_eq!(zero_json["graph"]["net_saved_bytes"], 0);
        assert_eq!(zero_json["graph"]["estimated_tokens_net_saved"], 0);
        assert_eq!(zero_json["graph"]["by_command"], serde_json::json!({}));

        // Metadata sizing deduplicates paths and skips traversal/missing paths safely.
        assert_eq!(
            referenced_file_bytes(
                &ws,
                [
                    "alpha.rs",
                    "alpha.rs",
                    "beta.rs",
                    "../outside.rs",
                    "missing.rs"
                ]
            ),
            (alpha.len() + beta.len()) as u64
        );

        // Once `.justless/` exists, record invocations and verify stats JSON + text.
        ws.prepare_state().unwrap();
        record_graph_use(&ws, "map", 1_024, 10_240);
        record_graph_use(&ws, "query", 512, 5_120);
        record_graph_use(&ws, "query", 512, 5_120);
        record_graph_use(&ws, "pack", 2_048, 20_480);
        record_graph_use(&ws, "review", 1_024, 10_240);

        let usage = graph_usage(&ws);
        assert_eq!(usage.calls, 5);
        assert_eq!(usage.shown_bytes, 5_120);
        assert_eq!(usage.referenced_file_bytes, 51_200);
        assert_eq!(usage.net_saved_bytes(), 46_080);
        assert_eq!(usage.by_command.get("map"), Some(&1));
        assert_eq!(usage.by_command.get("query"), Some(&2));
        assert_eq!(usage.by_command.get("pack"), Some(&1));
        assert_eq!(usage.by_command.get("review"), Some(&1));

        let expected_tokens = crate::tokens::Content::Code.tokens_signed(46_080);
        let json = render_json(&ws);
        assert_eq!(json["runs"], 0);
        assert_eq!(json["graph"]["calls"], 5);
        assert_eq!(json["graph"]["shown_bytes"], 5_120);
        assert_eq!(json["graph"]["referenced_file_bytes"], 51_200);
        assert_eq!(json["graph"]["net_saved_bytes"], 46_080);
        assert_eq!(json["graph"]["estimated_tokens_net_saved"], expected_tokens);
        assert_eq!(
            json["graph"]["by_command"],
            serde_json::json!({"map": 1, "pack": 1, "query": 2, "review": 1})
        );

        let lines = activity_summary_lines(
            &DeltasSummary::default(),
            &OutlinesSummary::default(),
            &usage,
            &ObservationsSummary::default(),
            &ArchiveRecalls::default(),
            false,
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        let expected_ratio = format!("{:.2}", crate::tokens::Content::Code.bytes_per_token());
        assert!(
            line.contains("Code graph: 5 call(s) (map ×1, pack ×1, query ×2, review ×1)"),
            "{line}"
        );
        assert!(
            line.contains(
                "printed 5.0 KB instead of 50.0 KB across referenced files, net 45.0 KB kept out"
            ),
            "{line}"
        );
        assert!(
            line.contains(&format!(
                "(≈{expected_tokens} tokens, estimated at {expected_ratio} bytes per token for code)"
            )),
            "{line}"
        );
    }
}
