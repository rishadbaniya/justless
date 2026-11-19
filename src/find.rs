//! `justless find`: literal text across indexed files, narrowed to paths and
//! globs, one page at a time. A cut page says which bound cut it and prints the
//! command for the next page, whose `--after` cursor names the search and the
//! line the page stopped at.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Arg, ArgMatches, Command};
use ignore::overrides::{Override, OverrideBuilder};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::index::{Fingerprint, Index};
use crate::workspace::{Workspace, display_path, shell_quote, shell_text};

pub fn command() -> Command<'static> {
    Command::new("find")
        .about("Find exact text across source, docs, and configuration")
        .arg(Arg::new("text").required(true).value_name("TEXT"))
        .arg(
            Arg::new("ignore-case")
                .long("ignore-case")
                .short('i')
                .help("Ignore ASCII case"),
        )
        .arg(
            Arg::new("path")
                .long("path")
                .value_name("PATH")
                .takes_value(true)
                .multiple_occurrences(true)
                .help("Search only this file or directory (repeatable)"),
        )
        .arg(
            Arg::new("glob")
                .long("glob")
                .short('g')
                .value_name("GLOB")
                .takes_value(true)
                .multiple_occurrences(true)
                .help("Include paths matching a gitignore-style glob, or exclude them with a leading ! (repeatable, as rg -g)"),
        )
        .arg(crate::number("limit", "20", "Maximum matching lines"))
        .arg(
            Arg::new("after")
                .long("after")
                .value_name("CURSOR")
                .help("Continue after the page that printed this cursor"),
        )
        .arg(Arg::new("json").long("json").help("Print a JSON report"))
        .arg(crate::budget())
}

/// A `--path` resolved to a workspace-relative file or directory. The root is
/// the empty string.
struct Target {
    relative: String,
    dir: bool,
}

impl Target {
    fn label(&self) -> String {
        match (self.relative.as_str(), self.dir) {
            ("", _) => "./".into(),
            (relative, true) => format!("{relative}/"),
            (relative, false) => relative.into(),
        }
    }

    fn contains(&self, file: &str) -> bool {
        if self.relative.is_empty() {
            true
        } else if self.dir {
            file.strip_prefix(&self.relative)
                .is_some_and(|rest| rest.starts_with('/'))
        } else {
            file == self.relative
        }
    }
}

/// The indexed files a search reads: those under any `--path`, then filtered
/// by the globs as `rg -g` would while walking the tree.
struct Scope {
    targets: Vec<Target>,
    globs: Vec<String>,
    matcher: Option<Override>,
    notes: Vec<String>,
}

impl Scope {
    fn new(
        workspace: &Workspace,
        index: &Index,
        paths: &[&str],
        globs: &[&str],
        root_given: bool,
    ) -> Result<Self> {
        let mut targets: Vec<Target> = Vec::new();
        for given in paths {
            let target = resolve(workspace, given, root_given)?;
            if !targets.iter().any(|seen| seen.relative == target.relative) {
                targets.push(target);
            }
        }
        let matcher = if globs.is_empty() {
            None
        } else {
            let mut builder = OverrideBuilder::new(&workspace.root);
            for glob in globs {
                builder
                    .add(glob)
                    .with_context(|| format!("--glob {glob}"))?;
            }
            Some(builder.build().context("--glob")?)
        };
        let mut notes = Vec::new();
        for target in &targets {
            if !index.files.keys().any(|file| target.contains(file)) {
                notes.push(format!(
                    "[--path {} has no indexed text files]",
                    target.label()
                ));
            }
        }
        for glob in globs {
            let pattern = glob.strip_prefix('!').unwrap_or(glob);
            let single = OverrideBuilder::new(&workspace.root)
                .add(pattern)
                .and_then(|builder| builder.build())
                .with_context(|| format!("--glob {glob}"))?;
            if !index
                .files
                .keys()
                .any(|file| touches(&single, file, |found| found.is_whitelist()))
            {
                notes.push(format!("[--glob {glob} matches no indexed file]"));
            }
        }
        Ok(Self {
            targets,
            globs: globs.iter().map(|glob| (*glob).to_owned()).collect(),
            matcher,
            notes,
        })
    }

    fn narrowed(&self) -> bool {
        !self.targets.is_empty() || !self.globs.is_empty()
    }

    fn contains(&self, file: &str) -> bool {
        (self.targets.is_empty() || self.targets.iter().any(|target| target.contains(file)))
            && self
                .matcher
                .as_ref()
                .is_none_or(|matcher| !touches(matcher, file, |found| found.is_ignore()))
    }

    /// `[scope: docs/ · glob *.md · 3 of 120 indexed files]`
    fn line(&self, files: usize, indexed: usize) -> String {
        let mut parts = Vec::new();
        if !self.targets.is_empty() {
            parts.push(
                self.targets
                    .iter()
                    .map(Target::label)
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        if !self.globs.is_empty() {
            parts.push(format!("glob {}", self.globs.join(", ")));
        }
        parts.push(format!("{files} of {indexed} indexed files"));
        format!("[scope: {}]", parts.join(" · "))
    }
}

/// Whether `test` holds for the match of `file` or of any directory above it,
/// the way a directory walk meets them: an excluded directory takes its files
/// with it.
fn touches(
    matcher: &Override,
    file: &str,
    test: impl Fn(&ignore::Match<ignore::overrides::Glob<'_>>) -> bool,
) -> bool {
    file.match_indices('/')
        .any(|(end, _)| test(&matcher.matched(&file[..end], true)))
        || test(&matcher.matched(file, false))
}

/// Resolves `--path` like other path arguments: a bare path comes from the
/// current directory and falls back to the root, or the reverse with `--root`.
/// Paths that start with `./` or `../`, and absolute paths, resolve from the
/// current directory.
fn resolve(workspace: &Workspace, given: &str, root_given: bool) -> Result<Target> {
    ensure!(!given.is_empty(), "--path needs a file or directory");
    let path = Path::new(given);
    let bare = path
        .components()
        .all(|part| matches!(part, Component::Normal(_)));
    let bases = match (bare, root_given) {
        (false, _) => vec![&workspace.cwd],
        (true, false) => vec![&workspace.cwd, &workspace.root],
        (true, true) => vec![&workspace.root, &workspace.cwd],
    };
    let mut outside = false;
    for base in bases {
        let joined = lexical(&base.join(path));
        outside |= !joined.starts_with(&workspace.root);
        let Ok(metadata) = fs::metadata(&joined) else {
            continue;
        };
        // Resolve the directories above it, such as macOS's /tmp, but not a
        // linked file or directory itself.
        let real = match (joined.parent(), joined.file_name()) {
            (Some(parent), Some(name)) => parent
                .canonicalize()
                .map_or_else(|_| joined.clone(), |parent| parent.join(name)),
            _ => joined.canonicalize().unwrap_or(joined),
        };
        let Ok(relative) = real.strip_prefix(&workspace.root) else {
            outside = true;
            continue;
        };
        let relative = relative
            .components()
            .map(|part| part.as_os_str().to_str())
            .collect::<Option<Vec<_>>>()
            .with_context(|| format!("--path {given} is not UTF-8"))?
            .join("/");
        return Ok(Target {
            relative,
            dir: metadata.is_dir(),
        });
    }
    if outside {
        bail!("--path {given} is outside the workspace");
    }
    bail!("--path {given}: no such file or directory in the workspace")
}

/// `path` with `.` and `..` applied without touching the file system.
fn lexical(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            part => clean.push(part),
        }
    }
    clean
}

/// Where a page stopped: a hash of the search, of the file, of the file's
/// indexed content, and the line. Lowercase hex and digits only, so it survives
/// any shell and splits on whitespace.
#[derive(PartialEq, Eq)]
struct Cursor {
    search: String,
    file: String,
    digest: String,
    line: usize,
}

const CURSOR_PREFIX: &str = "f1.";

fn short_hash(parts: &[&[u8]], bytes: usize) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hasher.finalize()[..bytes]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Cursor {
    fn at(search: &str, file: &str, digest: &str, line: usize) -> Self {
        Self {
            search: search.to_owned(),
            file: short_hash(&[file.as_bytes()], 6),
            digest: short_hash(&[digest.as_bytes()], 6),
            line,
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let (hex, line) = text.strip_prefix(CURSOR_PREFIX)?.split_once('.')?;
        let hex_ok = hex.len() == 32
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        let line_ok = !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit());
        if !hex_ok || !line_ok {
            return None;
        }
        Some(Self {
            search: hex[..8].to_owned(),
            file: hex[8..20].to_owned(),
            digest: hex[20..].to_owned(),
            line: line.parse().ok().filter(|&line| line > 0)?,
        })
    }

    fn text(&self) -> String {
        format!(
            "{CURSOR_PREFIX}{}{}{}.{}",
            self.search, self.file, self.digest, self.line
        )
    }
}

struct Hit {
    file: String,
    line: usize,
    text: String,
}

/// The search as its options gave it, for the next page's command.
struct Request<'a> {
    term: &'a str,
    insensitive: bool,
    paths: Vec<String>,
    globs: &'a [&'a str],
    limit: usize,
    limit_given: bool,
    budget: usize,
    budget_given: bool,
    json: bool,
    after_given: bool,
    justless: String,
}

impl Request<'_> {
    fn next(&self, cursor: &str) -> String {
        let mut words = vec![self.justless.clone(), "find".into()];
        if self.insensitive {
            words.push("-i".into());
        }
        // A value joined to its option by `=` is never read as an option, even
        // when it starts with `-`.
        words.extend(
            self.paths
                .iter()
                .map(|path| format!("--path={}", shell_quote(Path::new(path)))),
        );
        words.extend(
            self.globs
                .iter()
                .map(|glob| shell_quote(Path::new(&format!("--glob={glob}")))),
        );
        if self.limit_given {
            words.push(format!("--limit={}", self.limit));
        }
        if self.budget_given {
            words.push(format!("--budget={}", self.budget));
        }
        if self.json {
            words.push("--json".into());
        }
        words.push(format!("--after={cursor}"));
        words.push(shell_text(self.term));
        words.join(" ")
    }
}

/// What the scan found: every match in scope is counted, and the page keeps
/// the first `limit` after the cursor.
struct Found {
    total: usize,
    files: usize,
    before: usize,
    page: Vec<Hit>,
}

impl Found {
    fn remaining(&self) -> usize {
        self.total - self.before
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

pub fn run(workspace: &Workspace, matches: &ArgMatches, root_given: bool) -> Result<()> {
    let term = matches.value_of("text").unwrap();
    ensure!(
        !term.is_empty() && !term.contains('\n'),
        "enter a non-empty, single-line search term"
    );
    let insensitive = matches.is_present("ignore-case");
    let paths: Vec<&str> = matches.values_of("path").into_iter().flatten().collect();
    let globs: Vec<&str> = matches.values_of("glob").into_iter().flatten().collect();
    let limit = crate::read_num(matches, "limit", 1)?;
    let budget = crate::context_budget(workspace, matches)?;
    let index = Index::open(workspace)?;
    let scope = Scope::new(workspace, &index, &paths, &globs, root_given)?;
    let folded = if insensitive {
        term.to_ascii_lowercase()
    } else {
        term.to_owned()
    };
    let search = {
        let mut parts: Vec<&[u8]> = vec![b"find", folded.as_bytes()];
        if insensitive {
            parts.push(b"-i");
        }
        let labels: Vec<String> = scope.targets.iter().map(Target::label).collect();
        for label in &labels {
            parts.extend([b"--path".as_slice(), label.as_bytes()]);
        }
        for glob in &globs {
            parts.extend([b"--glob".as_slice(), glob.as_bytes()]);
        }
        short_hash(&parts, 4)
    };
    let in_scope: Vec<&String> = index
        .files
        .keys()
        .filter(|file| scope.contains(file))
        .collect();
    let after = match matches.value_of("after") {
        None => None,
        Some(text) => {
            let cursor = Cursor::parse(text).ok_or_else(|| {
                anyhow!("invalid --after cursor {text}; pass the cursor a find page printed")
            })?;
            ensure!(
                cursor.search == search,
                "this --after cursor belongs to another search; repeat that search's text, -i, --path and --glob, or drop --after"
            );
            let file = in_scope
                .iter()
                .find(|file| short_hash(&[file.as_bytes()], 6) == cursor.file)
                .ok_or_else(|| {
                    anyhow!(
                        "the file that page stopped in is no longer indexed; run the search again without --after"
                    )
                })?;
            ensure!(
                short_hash(&[index.files[*file].as_bytes()], 6) == cursor.digest,
                "{} changed since that page; run the search again without --after",
                display_path(file)
            );
            Some((file.as_str(), cursor.line))
        }
    };
    let found = scan(
        workspace,
        &index,
        &in_scope,
        &folded,
        insensitive,
        after,
        limit,
    );
    let invocation = workspace.invocation();
    let request = Request {
        term,
        insensitive,
        paths: scope
            .targets
            .iter()
            .map(|target| match target.relative.as_str() {
                // `.` resolves from the current directory, so a command that
                // may run elsewhere names the root itself.
                "" if invocation.files == "justless" => ".".into(),
                "" => workspace.root.to_string_lossy().into_owned(),
                relative => relative.to_owned(),
            })
            .collect(),
        globs: &globs,
        limit,
        limit_given: matches.occurrences_of("limit") > 0,
        budget,
        budget_given: matches.is_present("budget"),
        json: matches.is_present("json"),
        after_given: after.is_some(),
        // Paths in the command are root-relative, which a bare path reads only
        // from the root or with `--root`.
        justless: if scope.targets.is_empty() {
            invocation.command
        } else {
            invocation.files
        },
    };
    let mut notes = scope.notes.clone();
    if in_scope.is_empty() {
        notes.push("No indexed file is in scope.".into());
    } else if found.total == 0 {
        notes.push("No matching indexed text.".into());
        let note = if scope.narrowed() {
            index.skipped_note_where(false, |file| scope.contains(file))
        } else {
            index.skipped_note(false)
        };
        notes.extend(note);
    }
    let page = Page {
        request: &request,
        found: &found,
        search: &search,
        index: &index,
    };
    let scope_line = scope
        .narrowed()
        .then(|| scope.line(in_scope.len(), index.files.len()));
    let output = if request.json {
        let scope_value = scope.narrowed().then(|| {
            json!({
                "paths": scope.targets.iter().map(Target::label).collect::<Vec<_>>(),
                "globs": globs,
                "files": in_scope.len(),
                "indexed": index.files.len(),
            })
        });
        page.json(scope_value, &notes)?
    } else {
        let mut header = String::new();
        for line in scope_line.iter().chain(&notes) {
            header.push_str(line);
            header.push('\n');
        }
        page.text(header)
    };
    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

/// Reads every candidate file in index order, counting all matches in scope
/// and keeping up to `limit` after the cursor. One match per line.
fn scan(
    workspace: &Workspace,
    index: &Index,
    files: &[&String],
    term: &str,
    insensitive: bool,
    after: Option<(&str, usize)>,
    limit: usize,
) -> Found {
    let fingerprint = Fingerprint::new(term);
    let mut found = Found {
        total: 0,
        files: 0,
        before: 0,
        page: Vec::new(),
    };
    for file in files {
        if !index
            .fingerprints
            .get(*file)
            .is_some_and(|candidate| candidate.includes(fingerprint))
        {
            continue;
        }
        let Ok(path) = workspace.source_path(file) else {
            continue;
        };
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let mut any = false;
        for (offset, line) in text.lines().enumerate() {
            let position = if insensitive {
                line.to_ascii_lowercase().find(term)
            } else {
                line.find(term)
            };
            let Some(position) = position else {
                continue;
            };
            any = true;
            found.total += 1;
            let number = offset + 1;
            let seen = after.is_some_and(|(stop, stop_line)| {
                file.as_str() < stop || (file.as_str() == stop && number <= stop_line)
            });
            if seen {
                found.before += 1;
            } else if found.page.len() < limit {
                let start = line.ceil_char_boundary(position.saturating_sub(80));
                let end = line.floor_char_boundary(
                    (position + term.len()).saturating_add(180).min(line.len()),
                );
                found.page.push(Hit {
                    file: (*file).clone(),
                    line: number,
                    text: line[start..end].to_owned(),
                });
            }
        }
        found.files += usize::from(any);
    }
    found
}

/// Why a page ended before the last match.
#[derive(Clone, Copy)]
enum Cut {
    Limit,
    Budget,
}

struct Page<'a> {
    request: &'a Request<'a>,
    found: &'a Found,
    search: &'a str,
    index: &'a Index,
}

impl Page<'_> {
    /// The bound that cut a page of `shown` matches, if more follow.
    fn cut(&self, shown: usize) -> Option<Cut> {
        let cut = if shown == self.request.limit {
            Cut::Limit
        } else {
            Cut::Budget
        };
        (shown < self.found.remaining()).then_some(cut)
    }

    fn cursor(&self, shown: usize) -> String {
        let hit = &self.found.page[shown - 1];
        Cursor::at(
            self.search,
            &hit.file,
            &self.index.files[&hit.file],
            hit.line,
        )
        .text()
    }

    fn counts(&self) -> String {
        format!(
            "{} in {}",
            plural(self.found.total, "match", "matches"),
            plural(self.found.files, "file", "files")
        )
    }

    /// The lines after `shown` matches: which bound cut them and the command
    /// for the next page, or the end of a paged search.
    fn footer(&self, shown: usize) -> String {
        let last = self.found.before + shown;
        let limit = self.request.limit;
        let counts = self.counts();
        let notice = match self.cut(shown) {
            Some(Cut::Limit) => {
                format!("[--limit {limit} reached after {last} of {counts}]")
            }
            Some(Cut::Budget) => {
                format!("[budget reached after {last} of {counts}, before --limit {limit}]")
            }
            None if self.request.after_given => return format!("[end of {counts}]\n"),
            None => return String::new(),
        };
        format!(
            "{notice}\nNext page: {}\n",
            self.request.next(&self.cursor(shown))
        )
    }

    /// Whole match lines while they fit with the footer. The first match is
    /// always shown, cut short and alone when it would not fit.
    fn text(&self, header: String) -> String {
        let budget = self.request.budget;
        let mut output = header;
        let mut shown = 0;
        for hit in &self.found.page {
            let prefix = format!("{}:{}: ", display_path(&hit.file), hit.line);
            let line = format!("{prefix}{}\n", hit.text);
            let room = budget.saturating_sub(output.len() + self.footer(shown + 1).len());
            if line.len() <= room {
                output.push_str(&line);
            } else if shown == 0 {
                let keep = room.saturating_sub(prefix.len() + "…\n".len());
                let end = hit.text.floor_char_boundary(keep);
                output.push_str(&format!("{prefix}{}…\n", &hit.text[..end]));
                shown = 1;
                break;
            } else {
                break;
            }
            shown += 1;
        }
        output.push_str(&self.footer(shown));
        output
    }

    fn json(&self, scope: Option<Value>, notes: &[String]) -> Result<String> {
        let budget = self.request.budget;
        let base = |shown: usize| {
            let mut value = Map::new();
            value.insert("total".into(), json!(self.found.total));
            value.insert("files".into(), json!(self.found.files));
            value.insert("first".into(), json!(self.found.before + 1));
            if let Some(scope) = &scope {
                value.insert("scope".into(), scope.clone());
            }
            if !notes.is_empty() {
                value.insert("notes".into(), json!(notes));
            }
            value.insert("matches".into(), json!([]));
            if let Some(cut) = self.cut(shown) {
                let cursor = self.cursor(shown);
                let cut = match cut {
                    Cut::Limit => "limit",
                    Cut::Budget => "budget",
                };
                value.insert("cut".into(), json!(cut));
                value.insert("next".into(), json!(self.request.next(&cursor)));
                value.insert("after".into(), json!(cursor));
            }
            value
        };
        let hit_json =
            |hit: &Hit, text: &str| json!({"file": hit.file, "line": hit.line, "text": text});
        let mut kept: Vec<Value> = Vec::new();
        let mut used = 0;
        for hit in &self.found.page {
            let item = hit_json(hit, &hit.text);
            let size = serde_json::to_string(&item)?.len() + usize::from(!kept.is_empty());
            let frame = serde_json::to_string(&base(kept.len() + 1))?.len();
            if frame + used + size <= budget {
                used += size;
                kept.push(item);
                continue;
            }
            if kept.is_empty() {
                // Shorten the first match's text until the page fits.
                let mut end = hit.text.len();
                loop {
                    let text = format!("{}…", &hit.text[..end]);
                    let item = hit_json(hit, &text);
                    let size = serde_json::to_string(&item)?.len();
                    if end == 0 || frame + size <= budget {
                        kept.push(item);
                        break;
                    }
                    let over = (frame + size - budget).min(end);
                    end = hit.text.floor_char_boundary(end - over);
                }
            }
            break;
        }
        let mut value = base(kept.len());
        value.insert("matches".into(), Value::Array(kept));
        Ok(serde_json::to_string(&Value::Object(value))? + "\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_reject_other_shapes() {
        let cursor = Cursor::at("0123abcd", "src/a.rs", "digest", 42);
        let text = cursor.text();
        assert!(
            text.starts_with("f1.0123abcd") && text.ends_with(".42"),
            "{text}"
        );
        assert!(Cursor::parse(&text) == Some(cursor));
        for bad in [
            "f1.nonsense",
            "f2.0123abcd0123456789ab0123456789ab.4",
            "f1.0123ABCD0123456789ab0123456789ab.4",
            "f1.0123abcd0123456789ab0123456789ab.0",
            "f1.0123abcd0123456789ab0123456789ab.",
            "f1.0123abcd0123456789ab0123456789a.4",
        ] {
            assert!(Cursor::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn targets_contain_their_files_only() {
        let dir = Target {
            relative: "src".into(),
            dir: true,
        };
        assert!(dir.contains("src/a.rs") && !dir.contains("srcs/a.rs") && !dir.contains("src"));
        assert_eq!(dir.label(), "src/");
        let root = Target {
            relative: String::new(),
            dir: true,
        };
        assert!(root.contains("a.rs") && root.label() == "./");
    }
}
