use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;

use anyhow::{Result, ensure};

use crate::context::{Report, relationship};
use crate::history;
use crate::index::{Fingerprint, Index, nul_paths};
use crate::seen::Memory;
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
/// `applied` and `apply`, `discovery` and `discover`, and `redaction`,
/// `redactor` and `redact`.
fn stem(word: &str) -> String {
    let long = |base: &&str| base.len() >= 3;
    let base = word
        .strip_suffix("ation")
        .filter(|base| base.len() >= 4)
        .or_else(|| {
            word.strip_suffix("ion")
                .filter(|base| base.len() >= 4 && base.ends_with(['t', 's']))
        })
        .or_else(|| {
            word.strip_suffix("or")
                .filter(|base| base.len() >= 4 && base.ends_with('t'))
        })
        .or_else(|| {
            word.strip_suffix('y')
                .filter(|base| base.len() >= 5 && base.ends_with("er"))
        })
        .or_else(|| word.strip_suffix("ing").filter(long))
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

/// Words the task spells like code: snake_case, camelCase, a mixed-case
/// capitalized word after the first, or a task that is a single name.
/// All-uppercase tokens (`CLI`, `API`, `SIGINT`) are returned separately so they
/// exact-match only all-uppercase symbols (`const SIGINT`), never lowercase
/// functions such as `fn cli()`.
fn identifiers(task: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let tokens: Vec<&str> = task
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|token| !token.is_empty())
        .collect();
    let single = tokens.len() == 1;
    let mut code = BTreeSet::new();
    let mut all_caps = BTreeSet::new();
    for (at, token) in tokens.iter().enumerate() {
        let has_upper = token.chars().any(char::is_uppercase);
        let has_lower = token.chars().any(char::is_lowercase);
        if single || token.contains('_') {
            code.insert(token.to_lowercase());
        } else if has_upper && has_lower {
            if token.chars().skip(1).any(char::is_uppercase)
                || (at > 0 && token.starts_with(char::is_uppercase))
            {
                code.insert(token.to_lowercase());
            }
        } else if has_upper && !has_lower && token.len() >= 2 {
            all_caps.insert((*token).to_owned());
        }
    }
    (code, all_caps)
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

/// Up to three lines of `file` from `from`. In a session, lines printed
/// exactly the same way before become one line that says so, when that is at
/// most half their size.
#[allow(clippy::too_many_arguments)]
fn excerpt(
    workspace: &Workspace,
    index: &Index,
    cache: &mut BTreeMap<String, String>,
    file: &str,
    from: usize,
    to: usize,
    report: &mut Report,
    memory: Option<&mut Memory>,
) {
    let text = cache.entry(file.to_owned()).or_insert_with(|| {
        workspace
            .source_path(file)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .unwrap_or_default()
    });
    let lines: Vec<String> = text
        .lines()
        .enumerate()
        .skip(from.saturating_sub(1))
        .take(to.saturating_sub(from) + 1)
        .take(3)
        .map(|(offset, line)| format!("    {} | {}", offset + 1, short(line.trim(), 150)))
        .collect();
    let Some(memory) = memory else {
        for line in &lines {
            report.line(line);
        }
        return;
    };
    let revision = crate::source::revision_for(file, text.as_bytes());
    let last = from + lines.len().saturating_sub(1);
    let note = if lines.len() > 1 {
        format!("    {from}-{last} | unchanged since you read them")
    } else {
        format!("    {from} | unchanged since you read it")
    };
    // A short declaration says more than the note, so it stays.
    let bytes: usize = lines.iter().map(|line| line.len() + 1).sum();
    if !lines.is_empty()
        && 2 * (note.len() + 1) <= bytes
        && memory.saw_excerpt(file, &revision, &lines)
    {
        let room = report.left();
        report.line(note);
        memory.count_saved(bytes.min(room), room.saturating_sub(report.left()));
        return;
    }
    let clipped = report.clipped;
    for line in &lines {
        report.line(line);
    }
    if !clipped && !lines.is_empty() {
        memory.showed(
            file,
            &revision,
            &crate::context::seen_names(index, file, from, last),
        );
    }
    if !report.clipped && !lines.is_empty() {
        memory.remember_excerpt(file, &revision, &lines);
    }
}

/// A word without its inflection, for matching text: `hooks` to `hook`,
/// `notes` to `note`, `updated` to `updat`, `entries` to `entry`. Unlike
/// `stem`, it keeps a final `e` and `y`, so `notes` does not become `not`.
/// The flag says whether `ed` or `ing` came off, which may have taken a final
/// `e` with it, as in `updated`.
fn base(word: &str) -> (String, bool) {
    let long = |base: &&str| base.len() >= 3;
    if let Some(root) = word
        .strip_suffix("ies")
        .or_else(|| word.strip_suffix("ied"))
        .filter(long)
    {
        return (format!("{root}y"), false);
    }
    if let Some(root) = word
        .strip_suffix("ion")
        .filter(|base| base.len() >= 4 && base.ends_with(['t', 's']))
        .or_else(|| {
            word.strip_suffix("or")
                .filter(|base| base.len() >= 4 && base.ends_with('t'))
        })
        .or_else(|| {
            word.strip_suffix('y')
                .filter(|base| base.len() >= 5 && base.ends_with("er"))
        })
    {
        return (root.to_owned(), false);
    }
    if let Some(root) = word
        .strip_suffix("ing")
        .filter(long)
        .or_else(|| word.strip_suffix("ed").filter(long))
    {
        return (root.to_owned(), true);
    }
    let root = word
        .strip_suffix("es")
        .filter(|base| long(base) && base.ends_with(['s', 'x', 'h']))
        .or_else(|| {
            word.strip_suffix('s')
                .filter(|base| long(base) && !base.ends_with('s'))
        })
        .unwrap_or(word);
    (root.to_owned(), false)
}

/// Whether `token`, in lower case, is an inflection of `base`: the base itself
/// or the base with `s`, `ed`, `ing`, `er` or `ers`, or `es` after s, x, h or
/// z, allowing for a final `e` or `y` that the suffix replaces and a doubled
/// final consonant. A base that lost `ed` or `ing` (`elided`) may take back an
/// `e`. So `hook` meets `hooks` and `hooked`, `note` meets `noted` and
/// `noting`, `updated` meets `update`, and `run` meets `running`, but `note`
/// does not meet `not` or `nothing`, and `stats` does not meet `state`.
fn inflects(token: &str, base: &str, elided: bool) -> bool {
    const SUFFIXES: [&str; 6] = ["", "s", "ed", "ing", "er", "ers"];
    let Some(last) = base.chars().next_back() else {
        return false;
    };
    let cut = &base[..base.len() - last.len_utf8()];
    let rest = |prefix: &str| token.strip_prefix(prefix);
    rest(base).is_some_and(|rest| {
        SUFFIXES.contains(&rest)
            || (rest == "es" && ("sxhz".contains(last) || elided))
            || (rest == "e" && elided)
            || (last == 'e' && ["d", "r", "rs"].contains(&rest))
            || ("ts".contains(last) && ["ion", "ions", "or", "ors"].contains(&rest))
            || (base.ends_with("er") && rest == "y")
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
    /// The base and whether `ed` or `ing` came off it, for a term that inflects.
    base: Option<(String, bool)>,
    /// What every form that matches starts with, to skip a line cheaply.
    prefix: String,
    prints: [Fingerprint; 2],
}

impl Needle {
    fn new(word: String, inflected: bool) -> Self {
        let base = inflected.then(|| base(&word));
        let prefix = match &base {
            Some((base, _)) if base.ends_with(['e', 'y']) || ends_doubled(base) => {
                base[..base.len() - 1].to_owned()
            }
            Some((base, _)) => base.clone(),
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
        let Some((base, elided)) = &self.base else {
            return false;
        };
        folded.contains(&self.prefix)
            && words(text).any(|word| inflects(&word.to_lowercase(), base, *elided))
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

/// Lines on which a term must recur for a file without definitions to be
/// about it, as a name would say. It must also be on one line in sixteen, so a
/// lockfile that lists a crate a few times among thousands of lines is not.
const RECURS: usize = 3;

struct Passage {
    score: usize,
    /// The part of `score` that the file's path earns.
    named: usize,
    /// In a file without definitions, the weight of the rarest term the file
    /// is about: one its name holds or that recurs on several of its lines.
    focus: usize,
    /// The weight of the rarest term on the best line.
    rarest: usize,
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
// counts like a definition's name, since it says what the whole file is about,
// and so do terms that recur through the file. Matching lines then promote
// their enclosing definitions. Each task word's weight is also returned, keyed
// by the stem that definition names are matched by.
fn passages(
    workspace: &Workspace,
    index: &Index,
    words: &BTreeSet<String>,
    question: &str,
) -> (Vec<Passage>, BTreeMap<String, usize>) {
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
        let bare = index.at(&whole).is_empty();
        let named: Vec<usize> = if bare {
            (0..needles.len())
                .filter(|&n| needles[n].names(file))
                .collect()
        } else {
            Vec::new()
        };
        let mut counts = vec![0; needles.len()];
        let mut lines = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let folded = line.to_lowercase();
            let covered: Vec<usize> = (0..needles.len())
                .filter(|&n| needles[n].occurs(&folded, line))
                .collect();
            for &n in &covered {
                counts[n] += 1;
            }
            if !covered.is_empty() {
                lines.push((i + 1, covered, line.trim().to_owned()));
            }
        }
        let print = index.fingerprints[file];
        for (n, needle) in needles.iter().enumerate() {
            if needle.possible(print) {
                checked[n] += 1;
                found[n] += usize::from(counts[n] > 0);
            }
        }
        // A word in the file's directory is shared by every file there, as
        // `ctrlc` is by ctrlc/Cargo.toml and ctrlc/README.md, so recurring does
        // not make it what one of them is about.
        let length = text.lines().count();
        let directory = Path::new(file)
            .parent()
            .and_then(|path| path.to_str())
            .unwrap_or("");
        let about: Vec<usize> = if bare {
            (0..needles.len())
                .filter(|&n| {
                    named.contains(&n)
                        || (counts[n] >= RECURS
                            && counts[n] * 16 >= length
                            && !needles[n].occurs(&directory.to_lowercase(), directory))
                })
                .collect()
        } else {
            Vec::new()
        };
        read.push((file, named, about, lines));
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
    for (file, named, about, lines) in read {
        let path_score: usize = named.iter().map(|&n| weights[n]).sum();
        let focus = about.iter().map(|&n| weights[n]).max().unwrap_or(0);
        let mut scored: Vec<_> = lines
            .into_iter()
            .map(|(line, covered, text)| {
                let score = covered.iter().map(|&n| weights[n]).sum::<usize>() + path_score;
                let rarest = covered.iter().map(|&n| weights[n]).max().unwrap_or(0);
                let words = text
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|word| !word.is_empty())
                    .count();
                (score, words, line, rarest, text)
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
        let (primary, rarest) = (scored[0].2, scored[0].3);
        let mut lines: Vec<_> = scored
            .into_iter()
            .map(|(_, _, line, _, text)| (line, short(&text, 160)))
            .collect();
        lines.sort();
        ranked.push(Passage {
            score: best,
            named: path_score,
            focus,
            rarest,
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
    // Each task word's weight, by the stem that definition names are matched by.
    let mut by_stem = BTreeMap::<String, usize>::new();
    for (needle, &weight) in needles.iter().zip(&weights) {
        if needle.base.is_some() {
            let entry = by_stem.entry(stem(&needle.word)).or_default();
            *entry = (*entry).max(weight);
        }
    }
    (ranked, by_stem)
}

/// The best-matching line heads the passage, and the others follow in file order.
/// In a session, an unchanged passage shown before becomes a single line with
/// an exact `expand` pointer when that is shorter than reprinting the passage.
#[allow(clippy::too_many_arguments)]
fn show_text(
    workspace: &Workspace,
    index: &Index,
    cache: &mut BTreeMap<String, String>,
    hit: &Passage,
    report: &mut Report,
    memory: Option<&mut Memory>,
) {
    let file = display_path(&hit.location.file);
    let primary = hit.location.start;
    let mut lines = Vec::with_capacity(hit.lines.len());
    for (line, text) in hit.lines.iter().filter(|(line, _)| *line == primary) {
        lines.push(format!("Text {file}:{line} {text}"));
    }
    for (line, text) in hit.lines.iter().filter(|(line, _)| *line != primary) {
        lines.push(format!("    {line} | {text}"));
    }
    let Some(memory) = memory else {
        for line in &lines {
            report.line(line);
        }
        return;
    };
    let text = cache.entry(hit.location.file.clone()).or_insert_with(|| {
        workspace
            .source_path(&hit.location.file)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .unwrap_or_default()
    });
    let revision = crate::source::revision_for(&hit.location.file, text.as_bytes());
    let first = hit.lines.first().map_or(primary, |(line, _)| *line);
    let last = hit.lines.last().map_or(primary, |(line, _)| *line);
    let line_list = hit
        .lines
        .iter()
        .map(|(line, _)| line.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let selector = hit
        .lines
        .iter()
        .map(|(line, _)| format!("L{line}"))
        .collect::<Vec<_>>()
        .join(",");
    let note = format!(
        "Text {file}:{line_list} · unchanged since you read it · {} expand {} {selector}",
        workspace.invocation().files,
        crate::workspace::shell_path(&hit.location.file),
    );
    let bytes: usize = lines.iter().map(|line| line.len() + 1).sum();
    if !lines.is_empty()
        && note.len() + 1 < bytes
        && memory.saw_excerpt(&hit.location.file, &revision, &lines)
    {
        let room = report.left();
        report.line(note);
        memory.count_saved(bytes.min(room), room.saturating_sub(report.left()));
        return;
    }
    let clipped = report.clipped;
    for line in &lines {
        report.line(line);
    }
    if !clipped && !lines.is_empty() {
        memory.showed(
            &hit.location.file,
            &revision,
            &crate::context::seen_names(index, &hit.location.file, first, last),
        );
    }
    if !report.clipped && !lines.is_empty() {
        memory.remember_excerpt(&hit.location.file, &revision, &lines);
    }
}

pub fn pack(
    workspace: &Workspace,
    index: &Index,
    task: &str,
    cap: usize,
    memory: Option<&mut Memory>,
) -> Result<()> {
    let (report, failure) = pack_report(workspace, index, task, cap, memory)?;
    report.finish()?;
    if let Some((id, bytes)) = failure.filter(|(_, bytes)| *bytes > 0) {
        history::excerpts(workspace, &BTreeMap::from([(id, bytes)]));
    }
    Ok(())
}

fn pack_report(
    workspace: &Workspace,
    index: &Index,
    task: &str,
    cap: usize,
    mut memory: Option<&mut Memory>,
) -> Result<(Report, Option<(String, u64)>)> {
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
    let (code_names, all_caps) = identifiers(task);
    let is_code_name =
        |name: &str, lower: &str| code_names.contains(lower) || all_caps.contains(name);
    let wants_support = words.iter().any(|word| {
        matches!(
            word.as_str(),
            "test"
                | "tests"
                | "testing"
                | "bench"
                | "benchmark"
                | "benchmarks"
                | "fixture"
                | "fixtures"
                | "script"
                | "scripts"
                | "example"
                | "examples"
        )
    });
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
        let name_stem = stem(&lower);
        let exact = if is_code_name(name, &lower) {
            400
        } else if words.contains(&lower) || (terms(name).len() == 1 && stems.contains(&name_stem)) {
            16 * weight(&name_stem)
        } else {
            0
        } / if container { 2 } else { 1 };
        let covered: usize = parts.iter().map(|part| 16 * weight(part)).sum();
        let breadth = 40 * parts.len().saturating_sub(1);
        let file_hits = terms(&def.location.file)
            .iter()
            .filter(|part| stems.contains(&stem(part)))
            .count();
        let mut score = exact + covered + breadth + file_hits * 3;
        if score > 0 || (changes.contains(&def.location.file) && question.contains("chang")) {
            score += usize::from(changes.contains(&def.location.file)) * 4;
            ranks.insert(id, score);
        }
    }
    let (passages, term_weights) = passages(workspace, index, &words, &question);
    // A matching line promotes the definitions around it, except the one it
    // declares, whose name has already been scored. Text that no definition
    // encloses, such as configuration or prose, ranks on the same scale as
    // definitions and leads when it ranks higher.
    let mut standalone = Vec::new();
    let mut text_covers = BTreeMap::<usize, usize>::new();
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
                let said = text_covers.entry(id).or_default();
                *said = hit.rarest.max(*said);
            }
        }
    }
    // A file about a task word comes before a line that only mentions some.
    standalone.sort_by_key(|&at| Reverse(passages[at].rank() + passages[at].focus));
    let mut report = Report::new(cap);
    let mut diagnosed = BTreeSet::new();
    let mut found_run = false;
    // The recorded failure shown, and the bytes printed about it.
    let mut failure = None;
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
        // The lines about the run retrieve its output again; the source does not.
        let (room, mut source) = (report.left(), 0);
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
                let before = report.left();
                excerpt(
                    workspace,
                    index,
                    &mut sources,
                    &d.location.file,
                    d.location.start,
                    d.location.start,
                    &mut report,
                    memory.as_deref_mut(),
                );
                source += before.saturating_sub(report.left());
                for id in index.at(&d.location) {
                    *ranks.entry(id).or_default() += 90;
                    diagnosed.insert(id);
                }
            }
        }
        let shown = room.saturating_sub(report.left()).saturating_sub(source);
        failure = Some((run.id.clone(), shown as u64));
        found_run = true;
        break;
    }
    // The rarest task word, by the files that hold it, that a definition's
    // name or the text that promoted it covers. A name the task spells as code
    // and the code a recorded failure points at are what was asked for.
    let covers = |id: usize| {
        let (name, parts) = &names[id];
        if is_code_name(name, &name.to_lowercase()) || diagnosed.contains(&id) {
            return usize::MAX;
        }
        parts
            .iter()
            .filter_map(|part| term_weights.get(part))
            .chain(text_covers.get(&id))
            .copied()
            .max()
            .unwrap_or(0)
    };
    // Keep production definitions ahead of tests, scripts, benchmarks and
    // fixtures unless the task asks about them or names them directly.
    let support_rank = |id: usize| {
        if wants_support || failure_task || diagnosed.contains(&id) {
            return false;
        }
        let def = &index.definitions[id];
        let name = names[id].0;
        let lower = name.to_lowercase();
        !is_code_name(name, &lower)
            && (def.test
                || def.qualified.contains("::tests::")
                || def.qualified.contains("::test::")
                || def.qualified.ends_with("::tests")
                || def.qualified.ends_with("::test")
                || crate::map::is_support_file(&def.location.file))
    };
    let mut ranked: Vec<_> = ranks.into_iter().collect();
    ranked.sort_by_key(|(id, score)| {
        (
            support_rank(*id),
            Reverse(*score),
            &index.definitions[*id].location,
            *id,
        )
    });
    let best_tier = ranked.first().is_some_and(|(id, _)| support_rank(*id));
    let best = ranked.first().map_or(0, |(_, s)| *s);
    let mut queue = VecDeque::new();
    let mut seen = BTreeSet::new();
    let mut shown_text = BTreeSet::new();
    let mut text_files = BTreeSet::new();
    for (id, score) in ranked.into_iter().take(4) {
        if support_rank(id) != best_tier || score < best / 2 || report.clipped {
            break;
        }
        // Text leads a definition when it ranks higher, or when its file is
        // about a task word rarer than any the definition's name or promoting
        // text covers. A test whose name holds `running`, which most files
        // say, does not outrank the one file that registers hooks for "what
        // hooks are configured and running", whatever that file is called.
        let covered = covers(id);
        for &at in &standalone {
            let hit = &passages[at];
            if shown_text.len() >= 5 || report.clipped {
                break;
            }
            if !shown_text.contains(&at)
                && (hit.rank() > score
                    || (covered < usize::MAX
                        && (hit.rank() + hit.focus > score || hit.focus > covered)))
            {
                show_text(
                    workspace,
                    index,
                    &mut sources,
                    hit,
                    &mut report,
                    memory.as_deref_mut(),
                );
                shown_text.insert(at);
                text_files.insert(hit.location.file.clone());
            }
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
            index,
            &mut sources,
            &def.location.file,
            def.declaration,
            def.declaration,
            &mut report,
            memory.as_deref_mut(),
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
        let mut header_shown = false;
        for (other, edge, direction) in neighbors {
            if seen.len() >= 24 || report.clipped {
                break;
            }
            if shown_links.len() >= 32 || !shown_links.insert((edge.from, edge.to, edge.kind)) {
                continue;
            }
            if !header_shown {
                report.line(format!("  {} links:", index.definitions[id].qualified));
                header_shown = true;
                if report.clipped {
                    break;
                }
            }
            let discovered = seen.insert(other);
            let target = &index.definitions[other];
            let link = if edge.location.file == target.location.file {
                if edge.location.start == edge.location.end {
                    format!(":{}", edge.location.start)
                } else {
                    format!(":{}-{}", edge.location.start, edge.location.end)
                }
            } else {
                edge.location.to_string()
            };
            report.line(format!(
                "    {}{} {direction} {} [{}] {} (link {link})",
                if edge.inferred { "possible " } else { "" },
                relationship(edge.kind),
                target.qualified,
                if target.test { "test" } else { &target.kind },
                target.location,
            ));
            if discovered {
                excerpt(
                    workspace,
                    index,
                    &mut sources,
                    &target.location.file,
                    target.declaration,
                    target.location.end.min(target.declaration + 2),
                    &mut report,
                    memory.as_deref_mut(),
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
        show_text(
            workspace,
            index,
            &mut sources,
            hit,
            &mut report,
            memory.as_deref_mut(),
        );
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
    Ok((report, failure))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        for (rel, body) in files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, body).unwrap();
        }
        let workspace = Workspace::discover(Some(root)).unwrap();
        (dir, workspace)
    }

    #[test]
    fn pack_groups_bfs_links_and_shortens_same_file_locations() {
        let manifest =
            "[package]\nname = \"billing_service\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let lib = "pub mod audit;\npub mod handlers;\n\n\
            pub fn apply_tax(amount: u64) -> u64 {\n    amount + amount / 10\n}\n\n\
            pub fn checkout_total(amount: u64) -> u64 {\n    audit::record_sale(amount);\n    apply_tax(amount)\n}\n\n\
            pub fn bulk_checkout(a: u64, b: u64) -> u64 {\n    checkout_total(a) + checkout_total(b)\n}\n\n\
            pub fn preview_checkout(a: u64) -> u64 {\n    checkout_total(a)\n}\n";
        let handlers = "use crate::checkout_total;\n\n\
            pub fn handle_web(a: u64) -> u64 {\n    checkout_total(a)\n}\n\n\
            pub fn handle_cli(a: u64) -> u64 {\n    checkout_total(a)\n}\n\n\
            pub fn handle_batch(a: u64) -> u64 {\n    checkout_total(a)\n}\n";
        let audit = "pub fn record_sale(amount: u64) -> u64 {\n    amount\n}\n";
        let (_dir, workspace) = temp_workspace(&[
            ("Cargo.toml", manifest),
            ("src/lib.rs", lib),
            ("src/handlers.rs", handlers),
            ("src/audit.rs", audit),
        ]);
        let index = Index::open(&workspace).unwrap();

        let (report, _) = pack_report(&workspace, &index, "checkout_total", 8000, None).unwrap();
        let out = report.into_string();

        // Parent header appears once (indented so it never acts as a column-0 lead).
        assert_eq!(
            out.matches("  billing_service::checkout_total links:\n")
                .count(),
            1,
            "{out}"
        );
        // Link lines omit the repeated parent qualified prefix.
        let link_lines: Vec<&str> = out
            .lines()
            .filter(|line| line.starts_with("    ") && line.contains(" (link "))
            .collect();
        assert!(link_lines.len() >= 6, "{out}");
        assert!(
            link_lines
                .iter()
                .all(|line| !line.starts_with("    billing_service::checkout_total ")),
            "{out}"
        );
        // Same-file links (both incoming and same-file outgoing) shorten `(link file:line)` to `(link :line)`.
        assert!(
            out.contains("handle_web [function] src/handlers.rs:3-5 (link :4)\n"),
            "{out}"
        );
        assert!(
            out.contains("apply_tax [function] src/lib.rs:4-6 (link :10)\n"),
            "{out}"
        );
        // Cross-file outgoing link retains its source file path so no location information is lost.
        assert!(
            out.contains("audit::record_sale [function] src/audit.rs:1-3 (link src/lib.rs:9)\n"),
            "{out}"
        );

        // Measure byte savings vs repeating `parent.qualified` + full `(link file:line)` on every link.
        let grouped_link_bytes: usize = out
            .lines()
            .filter(|line| {
                line.ends_with(" links:") || (line.starts_with("    ") && line.contains(" (link "))
            })
            .map(|line| line.len() + 1)
            .sum();
        let mut ungrouped_link_bytes = 0usize;
        let mut current_parent = "";
        for line in out.lines() {
            if let Some(parent) = line
                .strip_prefix("  ")
                .and_then(|s| s.strip_suffix(" links:"))
            {
                current_parent = parent;
            } else if let Some(rest) = line.strip_prefix("    ")
                && rest.contains(" (link ")
            {
                // Reconstruct old format length: `  {parent} {rest_with_full_file}`.
                let extra_file = if rest.contains("(link :") {
                    // Extract file preceding ` (link :` from `{target.location}`.
                    let before = rest.split(" (link :").next().unwrap();
                    let loc = before.rsplit(' ').next().unwrap();
                    loc.rsplit_once(':').map_or(0, |(file, _)| file.len())
                } else {
                    0
                };
                ungrouped_link_bytes += 2 + current_parent.len() + 1 + rest.len() + extra_file + 1;
            }
        }
        assert!(
            grouped_link_bytes < ungrouped_link_bytes,
            "expected byte reduction: grouped={grouped_link_bytes} vs ungrouped={ungrouped_link_bytes}\n{out}"
        );
        // Expect >= 25% savings on link headers/lines for this 6-edge hub.
        assert!(
            grouped_link_bytes * 100 <= ungrouped_link_bytes * 75,
            "expected >=25% link byte savings: grouped={grouped_link_bytes} vs ungrouped={ungrouped_link_bytes}\n{out}"
        );
    }

    #[test]
    fn pack_deduplicates_unchanged_doc_passages_in_session_memory() {
        let doc = "# Deployment Policy\n\
            The canary_rollout_policy requires 5 percent traffic for 30 minutes before expanding to 50 percent.\n\
            Operators verify canary_rollout_policy error budgets and latency histograms in Grafana before promoting.\n\
            Automatic rollback triggers whenever canary_rollout_policy sees error rates above 0.5 percent.\n";
        let short_cfg = "short_toggle = true\n";
        let (_dir, workspace) =
            temp_workspace(&[("docs/rollout.md", doc), ("tiny.toml", short_cfg)]);
        let index = Index::open(&workspace).unwrap();
        let question = "how does canary_rollout_policy work";

        // Baseline without session.
        let plain = pack_report(&workspace, &index, question, 8000, None)
            .unwrap()
            .0
            .into_string();
        assert!(plain.contains("Text docs/rollout.md:"), "{plain}");

        // First call in session prints identically and remembers the passage.
        let mut mem1 = Memory::open(&workspace, Some("sess-doc"), false);
        let first = pack_report(&workspace, &index, question, 8000, mem1.as_mut())
            .unwrap()
            .0
            .into_string();
        mem1.unwrap().save(&workspace);
        assert_eq!(first, plain);

        // Second call in the same session replaces the unchanged passage with a compact pointer.
        let mut mem2 = Memory::open(&workspace, Some("sess-doc"), false);
        let second = pack_report(&workspace, &index, question, 8000, mem2.as_mut())
            .unwrap()
            .0
            .into_string();
        mem2.unwrap().save(&workspace);
        assert!(
            second.contains("Text docs/rollout.md:2,3,4 · unchanged since you read it · "),
            "{second}"
        );
        assert!(
            second.contains("expand docs/rollout.md L2,L3,L4"),
            "{second}"
        );
        assert!(!second.contains("Grafana"), "{second}");
        assert!(
            second.len() * 2 < first.len(),
            "expected >50% reduction on repeated doc pack: second={} vs first={}\n{second}",
            second.len(),
            first.len()
        );

        // `--fresh` reprints the full passage.
        let mut mem_fresh = Memory::open(&workspace, Some("sess-doc"), true);
        let fresh = pack_report(&workspace, &index, question, 8000, mem_fresh.as_mut())
            .unwrap()
            .0
            .into_string();
        mem_fresh.unwrap().save(&workspace);
        assert_eq!(fresh, first);

        // Editing the doc invalidates the seen passage and prints the updated lines.
        let edited = doc.replace("0.5 percent", "0.2 percent");
        fs::write(workspace.root.join("docs/rollout.md"), &edited).unwrap();
        let index_edited = Index::open(&workspace).unwrap();
        let mut mem3 = Memory::open(&workspace, Some("sess-doc"), false);
        let after_edit = pack_report(&workspace, &index_edited, question, 8000, mem3.as_mut())
            .unwrap()
            .0
            .into_string();
        mem3.unwrap().save(&workspace);
        assert!(after_edit.contains("0.2 percent"), "{after_edit}");
        assert!(
            !after_edit.contains("unchanged since you read it"),
            "{after_edit}"
        );

        // Reverting back to the original content prints whole again (not falsely unchanged).
        fs::write(workspace.root.join("docs/rollout.md"), doc).unwrap();
        let index_reverted = Index::open(&workspace).unwrap();
        let mut mem4 = Memory::open(&workspace, Some("sess-doc"), false);
        let after_revert = pack_report(&workspace, &index_reverted, question, 8000, mem4.as_mut())
            .unwrap()
            .0
            .into_string();
        mem4.unwrap().save(&workspace);
        assert!(after_revert.contains("0.5 percent"), "{after_revert}");
        assert!(
            !after_revert.contains("unchanged since you read it"),
            "{after_revert}"
        );

        // A very short 1-line passage stays verbatim rather than inflating with a longer note.
        let short_q = "short_toggle";
        let mut mem_s1 = Memory::open(&workspace, Some("sess-short"), false);
        let short1 = pack_report(&workspace, &index_reverted, short_q, 8000, mem_s1.as_mut())
            .unwrap()
            .0
            .into_string();
        mem_s1.unwrap().save(&workspace);
        let mut mem_s2 = Memory::open(&workspace, Some("sess-short"), false);
        let short2 = pack_report(&workspace, &index_reverted, short_q, 8000, mem_s2.as_mut())
            .unwrap()
            .0
            .into_string();
        mem_s2.unwrap().save(&workspace);
        assert_eq!(short2, short1);
        assert!(
            short2.contains("Text tiny.toml:1 short_toggle = true"),
            "{short2}"
        );
        assert!(!short2.contains("unchanged since you read it"), "{short2}");
    }

    #[test]
    fn pack_excerpts_trim_deep_indentation_and_preserve_line_numbers() {
        let src = concat!(
            "pub mod outer {\n",
            "    pub mod inner {\n",
            "        pub fn compute_tariff_rate(amount: u64) -> u64 {\n",
            "            let base = amount * 2;\n",
            "            base + 10\n",
            "        }\n",
            "    }\n",
            "}\n",
        );
        let (_dir, workspace) = temp_workspace(&[
            (
                "Cargo.toml",
                "[package]\nname = \"indentdemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("src/lib.rs", src),
        ]);
        let index = Index::open(&workspace).unwrap();
        let out = pack_report(&workspace, &index, "compute_tariff_rate", 8000, None)
            .unwrap()
            .0
            .into_string();
        assert!(
            out.contains("    3 | pub fn compute_tariff_rate(amount: u64) -> u64 {\n"),
            "{out}"
        );
        assert!(
            !out.contains("    3 |         pub fn"),
            "deep leading indent should be trimmed:\n{out}"
        );
    }
}
