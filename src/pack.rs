use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;

use anyhow::{Result, ensure};

use crate::context::{Report, relationship};
use crate::history;
use crate::index::{Fingerprint, Index, nul_paths};
use crate::summary::short;
use crate::workspace::{Location, Workspace, display_path};

fn terms(text: &str) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    for word in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let lower = word.to_lowercase();
        if lower.len() < 2
            || matches!(
                lower.as_str(),
                "the"
                    | "what"
                    | "which"
                    | "when"
                    | "where"
                    | "why"
                    | "how"
                    | "does"
                    | "from"
                    | "with"
                    | "that"
                    | "this"
                    | "will"
                    | "would"
                    | "can"
                    | "and"
                    | "for"
                    | "into"
                    | "rename"
                    | "breaks"
                    | "change"
                    | "changed"
                    | "fix"
                    | "are"
                    | "our"
                    | "was"
                    | "did"
                    | "after"
                    | "before"
                    | "understand"
                    | "please"
                    | "test"
                    | "tests"
                    | "failing"
                    | "failed"
                    | "failure"
                    | "is"
                    | "it"
                    | "in"
                    | "on"
                    | "of"
                    | "to"
                    | "by"
                    | "be"
                    | "do"
                    | "an"
                    | "as"
                    | "at"
                    | "or"
                    | "if"
                    | "we"
                    | "my"
                    | "me"
                    | "has"
                    | "have"
                    | "been"
                    | "there"
                    | "they"
                    | "its"
                    | "who"
                    | "should"
                    | "could"
            )
        {
            continue;
        }
        result.insert(lower);
        let mut part = String::new();
        let mut prev_lower = false;
        for letter in word.chars() {
            if letter == '_' || (letter.is_uppercase() && prev_lower) {
                if part.len() > 1 {
                    result.insert(part.to_lowercase());
                }
                part.clear();
            }
            if letter != '_' {
                part.push(letter);
            }
            prev_lower = letter.is_lowercase();
        }
        if part.len() > 1 {
            result.insert(part.to_lowercase());
        }
    }
    result
}

/// A light stem so that `updated`, `updates` and `update` meet, as do
/// `applied` and `apply`.
fn stem(word: &str) -> String {
    let long = |base: &&str| base.len() >= 3;
    let base = word
        .strip_suffix("ing")
        .filter(long)
        .or_else(|| word.strip_suffix("ed").filter(long))
        .or_else(|| {
            word.strip_suffix("es")
                .filter(|base| long(base) && base.ends_with(['s', 'x', 'h']))
        })
        .or_else(|| {
            word.strip_suffix('s')
                .filter(|base| long(base) && !base.ends_with('s'))
        })
        .unwrap_or(word);
    let base = base.strip_suffix('e').filter(long).unwrap_or(base);
    match base.strip_suffix('y') {
        Some(root) if root.len() >= 3 => format!("{root}i"),
        _ => base.to_owned(),
    }
}

/// Words the task spells like code: snake_case, camelCase, a capitalized word
/// after the first, or a task that is a single name.
fn identifiers(task: &str) -> BTreeSet<String> {
    let tokens: Vec<&str> = task
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|token| !token.is_empty())
        .collect();
    let single = tokens.len() == 1;
    tokens
        .iter()
        .enumerate()
        .filter(|&(at, token)| {
            single
                || token.contains('_')
                || token.chars().skip(1).any(char::is_uppercase)
                || (at > 0 && token.starts_with(char::is_uppercase))
        })
        .map(|(_, token)| token.to_lowercase())
        .collect()
}

fn changed_files(workspace: &Workspace) -> BTreeSet<String> {
    if !workspace.root.join(".git").exists() {
        return BTreeSet::new();
    }
    workspace
        .git(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--no-renames",
            "--untracked-files=all",
        ])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            nul_paths(&o.stdout)
                .filter_map(|record| record.get(3..).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn excerpt(
    workspace: &Workspace,
    cache: &mut BTreeMap<String, String>,
    file: &str,
    from: usize,
    to: usize,
    report: &mut Report,
) {
    let text = cache.entry(file.to_owned()).or_insert_with(|| {
        workspace
            .source_path(file)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .unwrap_or_default()
    });
    for (offset, line) in text
        .lines()
        .enumerate()
        .skip(from.saturating_sub(1))
        .take(to.saturating_sub(from) + 1)
        .take(3)
    {
        report.line(format!("    {} | {}", offset + 1, short(line.trim(), 150)));
    }
}

/// A word without its inflection, for matching text: `hooks` to `hook`,
/// `notes` to `note`, `updated` to `updat`, `entries` to `entry`. Unlike
/// `stem`, it keeps a final `e` and `y`, so `notes` does not become `not`.
fn base(word: &str) -> String {
    let long = |base: &&str| base.len() >= 3;
    if let Some(root) = word
        .strip_suffix("ies")
        .or_else(|| word.strip_suffix("ied"))
        .filter(long)
    {
        return format!("{root}y");
    }
    word.strip_suffix("ing")
        .filter(long)
        .or_else(|| word.strip_suffix("ed").filter(long))
        .or_else(|| {
            word.strip_suffix("es")
                .filter(|base| long(base) && base.ends_with(['s', 'x', 'h']))
        })
        .or_else(|| {
            word.strip_suffix('s')
                .filter(|base| long(base) && !base.ends_with('s'))
        })
        .unwrap_or(word)
        .to_owned()
}

/// Whether `token`, in lower case, is an inflection of `base`: the base itself
/// or the base with `s`, `es`, `e`, `ed`, `ing`, `er` or `ers`, allowing for a
/// final `e` or `y` that the suffix replaces and a doubled final consonant.
/// So `hook` meets `hooks` and `hooked`, `note` meets `noted` and `noting`,
/// and `run` meets `running`, but `note` does not meet `not` or `nothing`.
fn inflects(token: &str, base: &str) -> bool {
    const SUFFIXES: [&str; 8] = ["", "s", "es", "e", "ed", "ing", "er", "ers"];
    let Some(last) = base.chars().next_back() else {
        return false;
    };
    let cut = &base[..base.len() - last.len_utf8()];
    let rest = |prefix: &str| token.strip_prefix(prefix);
    rest(base).is_some_and(|rest| {
        SUFFIXES.contains(&rest)
            || (last == 'e' && ["d", "r", "rs"].contains(&rest))
            || (!"aeiouy".contains(last)
                && rest.strip_prefix(last).is_some_and(|rest| {
                    ["ed", "ing", "er", "ers"].contains(&rest)
                }))
    }) || (last == 'e' && rest(cut) == Some("ing"))
        || (last == 'y' && rest(cut).is_some_and(|rest| ["ies", "ied", "ier"].contains(&rest)))
        // `running` leaves `runn`, which meets `run` and `runs`.
        || (cut.len() >= 3 && ends_doubled(base) && matches!(rest(cut), Some("" | "s")))
}

/// A question term and the base its inflections share, such as `hook` for
/// `hooks`. A line matches the term anywhere, or an inflection of its base as
/// a whole word or identifier part, so `hooks` meets `hook-codex` and
/// `set_hook`, but `notes` does not meet `not` or `Nothing`.
struct Needle {
    word: String,
    base: Option<String>,
    /// What every form that matches starts with, to skip a line cheaply.
    prefix: String,
    prints: [Fingerprint; 2],
}

impl Needle {
    fn new(word: String, inflected: bool) -> Self {
        let base = inflected.then(|| base(&word));
        let prefix = match &base {
            Some(base) if base.ends_with(['e', 'y']) || ends_doubled(base) => {
                base[..base.len() - 1].to_owned()
            }
            Some(base) => base.clone(),
            None => word.clone(),
        };
        Self {
            prints: [Fingerprint::new(&word), Fingerprint::new(&prefix)],
            word,
            base,
            prefix,
        }
    }

    fn possible(&self, fingerprint: Fingerprint) -> bool {
        self.prints.iter().any(|&print| fingerprint.includes(print))
    }

    /// Whether the file's name, such as `hooks.json`, holds the term. A
    /// directory is shared by every file under it, so only a term written as a
    /// path, such as `src/pack.rs`, is matched against the whole path.
    fn names(&self, file: &str) -> bool {
        let name = if self.word.contains('/') {
            file
        } else {
            Path::new(file)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(file)
        };
        self.occurs(&name.to_lowercase(), name)
    }

    /// `folded` is `text` in lower case.
    fn occurs(&self, folded: &str, text: &str) -> bool {
        if folded.contains(&self.word) {
            return true;
        }
        let Some(base) = &self.base else {
            return false;
        };
        folded.contains(&self.prefix)
            && words(text).any(|word| inflects(&word.to_lowercase(), base))
    }
}

/// Whether `base` ends in a doubled consonant, as `runn` from `running` does.
fn ends_doubled(base: &str) -> bool {
    let mut letters = base.chars().rev();
    match (letters.next(), letters.next()) {
        (Some(last), Some(before)) => {
            last == before && last.is_ascii_alphabetic() && !"aeiouy".contains(last)
        }
        _ => false,
    }
}

/// The words of `text` and the parts of each identifier: `hook-codex` gives
/// `hook` and `codex`, `set_hook` gives `set` and `hook`, and `HookCodex`
/// gives `HookCodex`, `Hook` and `Codex`.
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .flat_map(|word| {
            let mut parts = Vec::new();
            let mut start = 0;
            let letters: Vec<(usize, char)> = word.char_indices().collect();
            for window in letters.windows(3) {
                let [(_, a), (at, b), (_, c)] = [window[0], window[1], window[2]];
                // `fooBar` splits before `B`, and `HTTPServer` before `S`.
                if (a.is_lowercase() && b.is_uppercase())
                    || (a.is_uppercase() && b.is_uppercase() && c.is_lowercase())
                {
                    parts.push(&word[start..at]);
                    start = at;
                }
            }
            if let [.., (_, a), (at, b)] = letters[..]
                && a.is_lowercase()
                && b.is_uppercase()
            {
                parts.push(&word[start..at]);
                start = at;
            }
            if start > 0 {
                parts.push(&word[start..]);
            }
            std::iter::once(word).chain(parts)
        })
}

/// How rare a term is among `total` files, from 1 for a term in every file to
/// 6 for one in under 1/32 of them. Names and text passages share this scale.
fn rarity(total: usize, documents: usize) -> usize {
    1 + ((total / documents.max(1)).max(1).ilog2() as usize).min(5)
}

/// The most that matching text adds to a rank, whether the text sits inside a
/// definition or in a file without one. Names, of definitions or of the file
/// that holds the text, count beyond it, so a long paragraph that mentions many
/// task words does not outrank code by length alone.
const TEXT_LIMIT: usize = 80;

struct Passage {
    score: usize,
    /// The part of `score` that the file's path earns.
    named: usize,
    /// The line that best matches the question.
    location: Location,
    /// Up to three distinct matching lines, in file order.
    lines: Vec<(usize, String)>,
}

impl Passage {
    /// The rank of text that no definition encloses, on the scale of definitions.
    fn rank(&self) -> usize {
        self.named + (self.score - self.named).min(TEXT_LIMIT)
    }
}

// Trigram fingerprints cheaply eliminate impossible files, but a large file's
// fingerprint admits almost any short word, so term rarity is estimated from the
// lines actually read. Files whose name holds a question term are read first.
// In a file without definitions, such as configuration or prose, that name
// counts like a definition's name, since it says what the whole file is about.
// Matching lines then promote their enclosing definitions.
fn passages(
    workspace: &Workspace,
    index: &Index,
    words: &BTreeSet<String>,
    question: &str,
) -> Vec<Passage> {
    let mut needles: Vec<Needle> = words
        .iter()
        .map(|word| Needle::new(word.clone(), true))
        .collect();
    for fragment in question
        .split_whitespace()
        .map(|s| s.trim_matches(['\'', '"', '`', '?', '.', ',']))
    {
        if fragment.len() > 2
            && fragment.contains(['/', '-', ':'])
            && !needles.iter().any(|needle| needle.word == fragment)
        {
            needles.push(Needle::new(fragment.to_owned(), false));
        }
    }
    let total = index.fingerprints.len().max(1);
    let (needles, possible): (Vec<Needle>, Vec<usize>) = needles
        .into_iter()
        .map(|needle| {
            let documents = index
                .fingerprints
                .values()
                .filter(|&&print| needle.possible(print))
                .count();
            (needle, documents)
        })
        .filter(|&(_, documents)| documents > 0)
        .unzip();
    let mut files = Vec::new();
    for (file, &print) in &index.fingerprints {
        let (mut named, mut likely) = (0, 0);
        for (needle, &documents) in needles.iter().zip(&possible) {
            let weight = rarity(total, documents);
            if needle.names(file) {
                named += weight;
            }
            if needle.possible(print) {
                likely += weight;
            }
        }
        if named + likely > 0 {
            files.push((Reverse(named), Reverse(likely), file));
        }
    }
    files.sort_unstable();
    let mut read = Vec::new();
    let mut checked = vec![0; needles.len()];
    let mut found = vec![0; needles.len()];
    for (_, _, file) in files.into_iter().take(96) {
        let Ok(path) = workspace.source_path(file) else {
            continue;
        };
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        // Definitions carry the names in source files, including a file entry
        // for most languages, so only a file without any is ranked by its name.
        let whole = Location {
            file: file.clone(),
            start: 1,
            end: usize::MAX,
        };
        let named: Vec<usize> = if index.at(&whole).is_empty() {
            (0..needles.len())
                .filter(|&n| needles[n].names(file))
                .collect()
        } else {
            Vec::new()
        };
        let mut present = vec![false; needles.len()];
        let mut lines = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let folded = line.to_lowercase();
            let covered: Vec<usize> = (0..needles.len())
                .filter(|&n| needles[n].occurs(&folded, line))
                .collect();
            for &n in &covered {
                present[n] = true;
            }
            if !covered.is_empty() {
                lines.push((i + 1, covered, line.trim().to_owned()));
            }
        }
        let print = index.fingerprints[file];
        for (n, needle) in needles.iter().enumerate() {
            if needle.possible(print) {
                checked[n] += 1;
                found[n] += usize::from(present[n]);
            }
        }
        read.push((file, named, lines));
    }
    // A term's file count is its fingerprint matches scaled by the share of
    // those that the files read confirmed. It is exact when all were read.
    let weights: Vec<usize> = (0..needles.len())
        .map(|n| {
            let documents = if checked[n] == 0 {
                possible[n]
            } else {
                (possible[n] * found[n]).div_ceil(checked[n])
            };
            16 * rarity(total, documents)
        })
        .collect();
    let mut ranked = Vec::new();
    for (file, named, lines) in read {
        let path_score: usize = named.iter().map(|&n| weights[n]).sum();
        let mut scored: Vec<_> = lines
            .into_iter()
            .map(|(line, covered, text)| {
                let score = covered.iter().map(|&n| weights[n]).sum::<usize>() + path_score;
                let words = text
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|word| !word.is_empty())
                    .count();
                (score, words, line, text)
            })
            .collect();
        let Some(best) = scored.iter().map(|&(score, ..)| score).max() else {
            continue;
        };
        // Among the lines that match about as well as the best, show the ones
        // that say the most, so a configured command outranks the bare key
        // above it. Repeated lines show once.
        scored.retain(|&(score, ..)| score * 2 >= best);
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        let mut distinct = BTreeSet::new();
        scored.retain(|(.., text)| distinct.insert(text.clone()));
        scored.truncate(3);
        let primary = scored[0].2;
        let mut lines: Vec<_> = scored
            .into_iter()
            .map(|(_, _, line, text)| (line, short(&text, 160)))
            .collect();
        lines.sort();
        ranked.push(Passage {
            score: best,
            named: path_score,
            location: Location {
                file: file.clone(),
                start: primary,
                end: primary,
            },
            lines,
        });
    }
    ranked.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.location.cmp(&b.location))
    });
    ranked.truncate(8);
    ranked
}

/// The best-matching line heads the passage, and the others follow in file order.
fn show_text(report: &mut Report, hit: &Passage) {
    let file = display_path(&hit.location.file);
    let primary = hit.location.start;
    for (line, text) in hit.lines.iter().filter(|(line, _)| *line == primary) {
        report.line(format!("Text {file}:{line} {text}"));
    }
    for (line, text) in hit.lines.iter().filter(|(line, _)| *line != primary) {
        report.line(format!("    {line} | {text}"));
    }
}

pub fn pack(workspace: &Workspace, index: &Index, task: &str, cap: usize) -> Result<()> {
    ensure!(!task.trim().is_empty(), "provide a task or symbol");
    let words = terms(task);
    let question = task.to_lowercase();
    let changes = changed_files(workspace);
    let failure_task = ["fail", "error", "panic", "diagnos", "regress"]
        .iter()
        .any(|term| question.contains(term));
    // A definition ranks by the task words its name covers, weighted by how few
    // names share each word, so `update_order_book` outranks `Order` for "where
    // is the order book updated?". A name the task spells as code ranks first.
    let stems: BTreeSet<String> = words.iter().map(|word| stem(word)).collect();
    let code_names = identifiers(task);
    let names: Vec<(&str, BTreeSet<String>)> = index
        .definitions
        .iter()
        .map(|def| {
            let name = def.qualified.rsplit("::").next().unwrap_or(&def.qualified);
            let parts = terms(name)
                .iter()
                .map(|part| stem(part))
                .filter(|part| stems.contains(part))
                .collect();
            (name, parts)
        })
        .collect();
    let mut frequency = BTreeMap::<&str, usize>::new();
    for (_, parts) in &names {
        for part in parts {
            *frequency.entry(part.as_str()).or_default() += 1;
        }
    }
    let total = index.definitions.len().max(1);
    let weight = |part: &str| rarity(total, frequency.get(part).copied().unwrap_or(0) + 1);
    let mut ranks = BTreeMap::new();
    for (id, (def, (name, parts))) in index.definitions.iter().zip(&names).enumerate() {
        let lower = name.to_lowercase();
        let container = matches!(def.kind.as_str(), "module" | "file");
        let exact = if code_names.contains(&lower) {
            400
        } else if words.contains(&lower) {
            16 * weight(&stem(&lower))
        } else {
            0
        } / if container { 2 } else { 1 };
        let covered: usize = parts.iter().map(|part| 16 * weight(part)).sum();
        let breadth = 40 * parts.len().saturating_sub(1);
        let file_hits = terms(&def.location.file).intersection(&words).count();
        let mut score = exact + covered + breadth + file_hits * 3;
        if score > 0 || (changes.contains(&def.location.file) && question.contains("chang")) {
            score += usize::from(changes.contains(&def.location.file)) * 4;
            ranks.insert(id, score);
        }
    }
    let passages = passages(workspace, index, &words, &question);
    // A matching line promotes the definitions around it, except the one it
    // declares, whose name has already been scored. Text that no definition
    // encloses, such as configuration or prose, ranks on the same scale as
    // definitions and leads when it ranks higher.
    let mut standalone = Vec::new();
    for (at, hit) in passages.iter().enumerate() {
        let enclosing = index.at(&hit.location);
        if enclosing
            .iter()
            .all(|&id| matches!(index.definitions[id].kind.as_str(), "file" | "module"))
        {
            standalone.push(at);
        }
        for id in enclosing {
            if index.definitions[id].declaration != hit.location.start {
                *ranks.entry(id).or_default() += hit.score.min(TEXT_LIMIT);
            }
        }
    }
    standalone.sort_by_key(|&at| Reverse(passages[at].rank()));
    let mut report = Report::new(cap);
    let mut found_run = false;
    let mut latest = BTreeSet::new();
    let mut sources = BTreeMap::new();
    for run in history::records(workspace, false)?.take(25) {
        let run = run?;
        // A search that found nothing exits 1, and its matches are not diagnostics.
        if (!run.command_key.is_empty() && !latest.insert(run.command_key.clone()))
            || run.status == 0
            || matches!(run.summary.kind.as_str(), "search" | "listing")
        {
            continue;
        }
        let relevant = run.diagnostics.iter().any(|d| {
            index
                .at(&d.location)
                .iter()
                .any(|id| ranks.get(id).is_some_and(|&score| score >= 16))
        });
        if !failure_task && !relevant {
            continue;
        }
        report.line(format!(
            "Recorded {} failure: {} (exit {})",
            run.family, run.id, run.status
        ));
        for d in run.diagnostics.iter().take(3) {
            let stale =
                run.source_digests.get(&d.location.file) != index.files.get(&d.location.file);
            report.line(format!(
                "  {}:{} -> {}{}",
                d.stream.name(),
                d.log_line,
                d.location,
                if stale { " [source changed]" } else { "" }
            ));
            if let Ok(lines) =
                crate::capture::diagnostic_excerpt(&workspace.state.join("runs").join(&run.id), d)
            {
                for line in lines.into_iter().skip(2).take(3) {
                    report.line(line);
                }
            }
            if !stale {
                excerpt(
                    workspace,
                    &mut sources,
                    &d.location.file,
                    d.location.start,
                    d.location.start,
                    &mut report,
                );
                for id in index.at(&d.location) {
                    *ranks.entry(id).or_default() += 90;
                }
            }
        }
        found_run = true;
        break;
    }
    let mut ranked: Vec<_> = ranks.into_iter().collect();
    ranked.sort_by_key(|(id, score)| (Reverse(*score), &index.definitions[*id].location, *id));
    let best = ranked.first().map_or(0, |(_, s)| *s);
    let mut queue = VecDeque::new();
    let mut seen = BTreeSet::new();
    let mut shown_text = BTreeSet::new();
    let mut text_files = BTreeSet::new();
    let mut standalone = standalone.into_iter().peekable();
    for (id, score) in ranked.into_iter().take(4) {
        if score < best / 2 || report.clipped {
            break;
        }
        while let Some(&at) = standalone.peek()
            && passages[at].rank() > score
            && shown_text.len() < 5
            && !report.clipped
        {
            show_text(&mut report, &passages[at]);
            shown_text.insert(at);
            text_files.insert(passages[at].location.file.clone());
            standalone.next();
        }
        let def = &index.definitions[id];
        report.line(format!(
            "{} [{}] {}{}",
            def.qualified,
            def.kind,
            def.location,
            if changes.contains(&def.location.file) {
                " [working tree change]"
            } else {
                ""
            }
        ));
        excerpt(
            workspace,
            &mut sources,
            &def.location.file,
            def.declaration,
            def.declaration,
            &mut report,
        );
        seen.insert(id);
        queue.push_back((id, 0));
    }
    let mut shown_links = BTreeSet::new();
    while let Some((id, depth)) = queue.pop_front() {
        if depth >= 3 || seen.len() >= 24 || report.clipped {
            continue;
        }
        let mut neighbors = Vec::new();
        for edge in index.incoming(id) {
            if let Some(from) = edge.from {
                neighbors.push((from, edge, "from"));
            }
        }
        for edge in index.outgoing(id) {
            neighbors.push((edge.to, edge, "to"));
        }
        neighbors.sort_by_key(|(other, edge, _)| {
            (
                !index.definitions[*other].test,
                edge.inferred,
                &edge.location,
                *other,
            )
        });
        for (other, edge, direction) in neighbors {
            if seen.len() >= 24 || report.clipped {
                break;
            }
            if shown_links.len() >= 32 || !shown_links.insert((edge.from, edge.to, edge.kind)) {
                continue;
            }
            let discovered = seen.insert(other);
            let target = &index.definitions[other];
            report.line(format!(
                "  {} {}{} {direction} {} [{}] {} (link {})",
                index.definitions[id].qualified,
                if edge.inferred { "possible " } else { "" },
                relationship(edge.kind),
                target.qualified,
                if target.test { "test" } else { &target.kind },
                target.location,
                edge.location
            ));
            if discovered {
                excerpt(
                    workspace,
                    &mut sources,
                    &target.location.file,
                    target.declaration,
                    target.location.end.min(target.declaration + 2),
                    &mut report,
                );
                queue.push_back((other, depth + 1));
            }
        }
    }
    for (at, hit) in passages.iter().enumerate() {
        if report.clipped || shown_text.len() >= 5 {
            break;
        }
        let file = &hit.location.file;
        if shown_text.contains(&at)
            || seen.iter().any(|&id| {
                let d = &index.definitions[id];
                d.location.file == *file
                    && (d.declaration..=d.declaration + 2).contains(&hit.location.start)
            })
        {
            continue;
        }
        show_text(&mut report, hit);
        shown_text.insert(at);
        text_files.insert(file.clone());
    }
    let inspected: BTreeMap<_, _> = sources
        .keys()
        .chain(&text_files)
        .filter_map(|file| {
            index
                .files
                .get(file)
                .map(|hash| (file.clone(), hash.clone()))
        })
        .collect();
    if !found_run && seen.is_empty() && shown_text.is_empty() {
        report.line("No matching source or recorded failure. Try a symbol, file name, or an error from the log.");
    }
    history::saw_pack(workspace, inspected);
    report.finish()
}
