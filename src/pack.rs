use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::{Result, ensure};

use crate::context::{
    Report, SourceCache, format_line_spans_with_base, format_location_with_base,
    format_same_file_spans, hoistable_dir_prefix, relationship,
};
use crate::history;
use crate::index::{Definition, Edge, Fingerprint, Index, is_call, nul_paths};
use crate::rust_syntax::Kind;
use crate::seen::Memory;
use crate::summary::short;
use crate::workspace::{Location, Workspace, display_path};

fn is_stopword(lower: &str) -> bool {
    lower.len() < 2
        || matches!(
            lower,
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
}

fn task_terms(text: &str) -> (BTreeSet<String>, HashSet<String>) {
    let mut result = BTreeSet::new();
    let mut explicit = HashSet::new();
    let mut part = String::new();
    for word in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let lower = word.to_lowercase();
        if is_stopword(&lower) {
            continue;
        }
        explicit.insert(lower.clone());
        result.insert(lower);
        part.clear();
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
    (result, explicit)
}

fn terms(text: &str) -> BTreeSet<String> {
    task_terms(text).0
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
    cache: &mut SourceCache,
    shown_files: &mut BTreeSet<String>,
    file: &str,
    from: usize,
    to: usize,
    report: &mut Report,
    memory: Option<&mut Memory>,
) {
    shown_files.insert(file.to_owned());
    let Some(source) = cache.get_or_try_read(workspace, file) else {
        return;
    };
    let first = from.max(1);
    let max_end = to.max(first).min(first.saturating_add(2));
    let mut lines: Vec<String> = Vec::with_capacity(3);
    for line_no in first..=max_end {
        let Some(line) = source.line(line_no) else {
            break;
        };
        lines.push(format!("    {line_no} | {}", short(line.trim(), 150)));
    }
    let Some(memory) = memory else {
        for line in &lines {
            report.line(line);
        }
        return;
    };
    let revision = source.revision(file).to_owned();
    let last = first + lines.len().saturating_sub(1);
    let note = if lines.len() > 1 {
        format!("    {first}-{last} | unchanged since you read them")
    } else {
        format!("    {first} | unchanged since you read it")
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
            &crate::context::seen_names(index, file, first, last),
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
    /// Whether this term was typed directly by the user (or as a path/hyphen
    /// fragment), rather than synthesized only by splitting a code identifier.
    explicit: bool,
}

impl Needle {
    fn new(word: String, inflected: bool, explicit: bool) -> Self {
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
            explicit,
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
            let mut has_upper = false;
            let mut has_lower = false;
            for ch in word.chars() {
                has_upper |= ch.is_uppercase();
                has_lower |= ch.is_lowercase();
                if has_upper && has_lower {
                    break;
                }
            }
            if !(has_upper && has_lower) {
                return std::iter::once(word).chain(Vec::new());
            }
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
    /// Whether this passage matched at least one explicit user task term (or
    /// full code identifier) rather than only a split sub-word of an identifier.
    matched_explicit: bool,
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

fn fold_lowercase_into(src: &str, dst: &mut String) {
    dst.clear();
    if src.is_ascii() {
        dst.reserve(src.len());
        for byte in src.bytes() {
            dst.push(byte.to_ascii_lowercase() as char);
        }
    } else {
        for ch in src.chars() {
            for lower in ch.to_lowercase() {
                dst.push(lower);
            }
        }
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
    cache: &mut SourceCache,
    index: &Index,
    defined_files: &HashSet<&str>,
    words: &BTreeSet<String>,
    explicit_words: &HashSet<String>,
    question: &str,
) -> (Vec<Passage>, BTreeMap<String, usize>) {
    let mut needles: Vec<Needle> = words
        .iter()
        .map(|word| Needle::new(word.clone(), true, explicit_words.contains(word)))
        .collect();
    for fragment in question
        .split_whitespace()
        .map(|s| s.trim_matches(['\'', '"', '`', '?', '.', ',']))
    {
        if fragment.len() > 2
            && fragment.contains(['/', '-', ':'])
            && !needles.iter().any(|needle| needle.word == fragment)
        {
            needles.push(Needle::new(fragment.to_owned(), false, true));
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
    let mut folded = String::with_capacity(256);
    let mut covered: Vec<usize> = Vec::with_capacity(needles.len());
    for (_, _, file) in files.into_iter().take(96) {
        let Some(cached) = cache.get_or_try_read(workspace, file) else {
            continue;
        };
        let text = cached.text.as_str();
        // Definitions carry the names in source files, including a file entry
        // for most languages, so only a file without any is ranked by its name.
        let bare = !defined_files.contains(file.as_str());
        let named: Vec<usize> = if bare {
            (0..needles.len())
                .filter(|&n| needles[n].names(file))
                .collect()
        } else {
            Vec::new()
        };
        let mut counts = vec![0; needles.len()];
        let mut lines = Vec::new();
        let mut length = 0usize;
        for (i, line) in text.lines().enumerate() {
            length += 1;
            fold_lowercase_into(line, &mut folded);
            covered.clear();
            for (n, needle) in needles.iter().enumerate() {
                if needle.occurs(&folded, line) {
                    covered.push(n);
                    counts[n] += 1;
                }
            }
            if !covered.is_empty() {
                lines.push((i + 1, covered.clone(), line.trim().to_owned()));
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
        let directory = Path::new(file)
            .parent()
            .and_then(|path| path.to_str())
            .unwrap_or("");
        let directory_lower = if bare {
            directory.to_lowercase()
        } else {
            String::new()
        };
        let about: Vec<usize> = if bare {
            (0..needles.len())
                .filter(|&n| {
                    named.contains(&n)
                        || (counts[n] >= RECURS
                            && counts[n] * 16 >= length
                            && !needles[n].occurs(&directory_lower, directory))
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
        let path_explicit = named.iter().any(|&n| needles[n].explicit);
        let focus = about.iter().map(|&n| weights[n]).max().unwrap_or(0);
        let mut scored: Vec<_> = lines
            .into_iter()
            .map(|(line, covered, text)| {
                let score = covered.iter().map(|&n| weights[n]).sum::<usize>() + path_score;
                let rarest = covered.iter().map(|&n| weights[n]).max().unwrap_or(0);
                let explicit = path_explicit || covered.iter().any(|&n| needles[n].explicit);
                let words = text
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|word| !word.is_empty())
                    .count();
                (score, words, line, rarest, explicit, text)
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
        let mut distinct: HashSet<String> = HashSet::with_capacity(scored.len());
        scored.retain(|(.., text)| distinct.insert(text.clone()));
        scored.truncate(3);
        let (primary, rarest) = (scored[0].2, scored[0].3);
        let matched_explicit = path_explicit || scored.iter().any(|&(_, _, _, _, exp, _)| exp);
        let mut lines: Vec<_> = scored
            .into_iter()
            .map(|(_, _, line, _, _, text)| (line, short(&text, 160)))
            .collect();
        lines.sort();
        ranked.push(Passage {
            score: best,
            named: path_score,
            focus,
            rarest,
            matched_explicit,
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
    cache: &mut SourceCache,
    shown_files: &mut BTreeSet<String>,
    hit: &Passage,
    report: &mut Report,
    memory: Option<&mut Memory>,
) {
    shown_files.insert(hit.location.file.clone());
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
    let revision = cache
        .get_or_try_read(workspace, &hit.location.file)
        .map(|source| source.revision(&hit.location.file).to_owned())
        .unwrap_or_else(|| crate::source::revision_for(&hit.location.file, &[]));
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

/// True when `def.qualified` already starts with `"{file}::"` (or equals
/// `file`), as Tree-sitter polyglot definitions do, so repeating `file` before
/// `:start-end` on the same line is redundant.
fn qualified_includes_file(def: &Definition) -> bool {
    let file = def.location.file.as_str();
    display_path(file) == file
        && (def.qualified == file
            || (def.qualified.starts_with(file) && def.qualified[file.len()..].starts_with("::")))
}

fn format_pack_location(def: &Definition, base: Option<&str>) -> String {
    if qualified_includes_file(def) {
        if def.location.start == def.location.end {
            format!(":{}", def.location.start)
        } else {
            format!(":{}-{}", def.location.start, def.location.end)
        }
    } else {
        format_location_with_base(&def.location, base)
    }
}

fn is_support_definition(def: &Definition) -> bool {
    def.test
        || def.qualified.contains("::tests::")
        || def.qualified.contains("::test::")
        || def.qualified.ends_with("::tests")
        || def.qualified.ends_with("::test")
        || crate::map::is_support_file(&def.location.file)
}

fn is_cross_language(index: &Index, left_file: &str, right_file: &str) -> bool {
    if left_file == right_file {
        return false;
    }
    (left_file.ends_with(".rs"), index.grammar(left_file))
        != (right_file.ends_with(".rs"), index.grammar(right_file))
}

/// Priority tier for dependency-cone BFS edges so direct structural calls and
/// cross-language HTTP/SQL/polyglot bridges outrank weak imports/references.
fn cone_edge_tier(index: &Index, parent: &Definition, target: &Definition, edge: &Edge) -> usize {
    if is_call(edge.kind) && !edge.inferred {
        0
    } else if matches!(edge.kind, Kind::Route | Kind::Data)
        || is_cross_language(index, &parent.location.file, &target.location.file)
    {
        1
    } else if is_call(edge.kind) {
        2
    } else if !edge.inferred {
        3
    } else {
        4
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
    memory: Option<&mut Memory>,
) -> Result<(Report, Option<(String, u64)>)> {
    let mut cache = SourceCache::default();
    pack_report_with_cache(workspace, &mut cache, index, task, cap, memory)
}

fn pack_report_with_cache(
    workspace: &Workspace,
    cache: &mut SourceCache,
    index: &Index,
    task: &str,
    cap: usize,
    mut memory: Option<&mut Memory>,
) -> Result<(Report, Option<(String, u64)>)> {
    ensure!(!task.trim().is_empty(), "provide a task or symbol");
    let (words, explicit_words) = task_terms(task);
    let question = task.to_lowercase();
    let changes = changed_files(workspace);
    let failure_task = ["fail", "error", "panic", "diagnos", "regress"]
        .iter()
        .any(|term| question.contains(term));
    // A definition ranks by the task words its name covers, weighted by how few
    // names share each word, so `update_order_book` outranks `Order` for "where
    // is the order book updated?". A name the task spells as code ranks first.
    let stems: HashSet<String> = words.iter().map(|word| stem(word)).collect();
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

    struct DefMeta {
        lower: String,
        is_code: bool,
        single_term: bool,
        parts: Vec<String>,
    }

    let mut defined_files: HashSet<&str> = HashSet::with_capacity(index.files.len());
    let mut names: Vec<DefMeta> = Vec::with_capacity(index.definitions.len());
    let mut frequency: HashMap<String, usize> = HashMap::new();
    for def in &index.definitions {
        defined_files.insert(def.location.file.as_str());
        let name = def.qualified.rsplit("::").next().unwrap_or(&def.qualified);
        let lower = name.to_lowercase();
        let is_code = is_code_name(name, &lower);
        let name_terms = terms(name);
        let single_term = name_terms.len() == 1;
        let mut parts_set = BTreeSet::new();
        for part in &name_terms {
            let stemmed = stem(part);
            if stems.contains(&stemmed) {
                parts_set.insert(stemmed);
            }
        }
        let parts: Vec<String> = parts_set.into_iter().collect();
        for part in &parts {
            *frequency.entry(part.clone()).or_default() += 1;
        }
        names.push(DefMeta {
            lower,
            is_code,
            single_term,
            parts,
        });
    }
    let total = index.definitions.len().max(1);
    let weight = |part: &str| rarity(total, frequency.get(part).copied().unwrap_or(0) + 1);
    let mut file_hits_cache: HashMap<&str, usize> = HashMap::new();
    let mut ranks: HashMap<usize, usize> = HashMap::new();
    let mut name_scores: HashMap<usize, usize> = HashMap::new();
    for (id, (def, meta)) in index.definitions.iter().zip(&names).enumerate() {
        let container = matches!(def.kind.as_str(), "module" | "file");
        let name_stem = stem(&meta.lower);
        let exact = if meta.is_code {
            400
        } else if words.contains(&meta.lower) || (meta.single_term && stems.contains(&name_stem)) {
            16 * weight(&name_stem)
        } else {
            0
        } / if container { 2 } else { 1 };
        let covered: usize = meta.parts.iter().map(|part| 16 * weight(part)).sum();
        let breadth = 40 * meta.parts.len().saturating_sub(1);
        let file_hits = *file_hits_cache
            .entry(def.location.file.as_str())
            .or_insert_with(|| {
                terms(&def.location.file)
                    .iter()
                    .filter(|part| stems.contains(&stem(part)))
                    .count()
            });
        let mut score = exact + covered + breadth + file_hits * 3;
        if score > 0 || (changes.contains(&def.location.file) && question.contains("chang")) {
            score += usize::from(changes.contains(&def.location.file)) * 4;
            ranks.insert(id, score);
            name_scores.insert(id, score);
        }
    }
    let (passages, term_weights) = passages(
        workspace,
        cache,
        index,
        &defined_files,
        &words,
        &explicit_words,
        &question,
    );
    // A matching line promotes the definitions around it, except the one it
    // declares, whose name has already been scored. Text that no definition
    // encloses, such as configuration or prose, ranks on the same scale as
    // definitions and leads when it ranks higher.
    let mut standalone = Vec::new();
    let mut text_covers: HashMap<usize, usize> = HashMap::new();
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
    let mut diagnosed: HashSet<usize> = HashSet::new();
    let mut found_run = false;
    // The recorded failure shown, and the bytes printed about it.
    let mut failure = None;
    let mut latest: HashSet<String> = HashSet::new();
    let mut shown_files: BTreeSet<String> = BTreeSet::new();
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
                    cache,
                    &mut shown_files,
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
        let meta = &names[id];
        if meta.is_code || diagnosed.contains(&id) {
            return usize::MAX;
        }
        meta.parts
            .iter()
            .filter_map(|part| term_weights.get(part))
            .chain(text_covers.get(&id))
            .copied()
            .max()
            .unwrap_or(0)
    };
    // Keep production definitions ahead of tests, scripts, benchmarks and
    // fixtures unless the task asks about them or names them directly.
    let is_support = |id: usize| {
        if wants_support || failure_task || diagnosed.contains(&id) {
            return false;
        }
        !names[id].is_code && is_support_definition(&index.definitions[id])
    };
    let mut ranked: Vec<(bool, usize, usize)> = ranks
        .iter()
        .map(|(&id, &score)| (is_support(id), score, id))
        .collect();
    ranked.sort_by(|&(sup_a, score_a, id_a), &(sup_b, score_b, id_b)| {
        sup_a
            .cmp(&sup_b)
            .then_with(|| score_b.cmp(&score_a))
            .then_with(|| {
                index.definitions[id_a]
                    .location
                    .cmp(&index.definitions[id_b].location)
            })
            .then_with(|| id_a.cmp(&id_b))
    });
    let best_tier = ranked.first().is_some_and(|&(sup, _, _)| sup);
    let best = ranked.first().map_or(0, |&(_, score, _)| score);
    let best_has_name = ranked
        .first()
        .is_some_and(|&(_, _, id)| name_scores.contains_key(&id) || diagnosed.contains(&id));
    let mut queue = VecDeque::new();
    let mut seen: HashSet<usize> = HashSet::new();
    let mut shown_text: HashSet<usize> = HashSet::new();
    let mut found_exact_code_seed = false;

    // Text leads definitions when it outranks the top definition, or when its
    // file is about a task word rarer than any the top definition covers.
    // Once a strong definition leads, inferior standalone text waits until after
    // the structural dependency cone so tight budgets pack architectural signal.
    let top_covered = ranked.first().map_or(0, |&(_, _, id)| covers(id));
    for &at in &standalone {
        let hit = &passages[at];
        if shown_text.len() >= 5 || report.clipped {
            break;
        }
        let leads_top = ranked.is_empty()
            || hit.rank() > best
            || (top_covered < usize::MAX
                && (hit.rank() + hit.focus > best || hit.focus > top_covered));
        if leads_top && !shown_text.contains(&at) {
            show_text(
                workspace,
                index,
                cache,
                &mut shown_files,
                hit,
                &mut report,
                memory.as_deref_mut(),
            );
            shown_text.insert(at);
        }
    }

    let mut seed_neighbors: HashSet<usize> = HashSet::new();
    let mut seeds_shown = 0usize;
    for (sup, score, id) in ranked {
        if seeds_shown >= 4 || sup != best_tier || score < best / 2 || report.clipped {
            break;
        }
        // When the primary seed is a structural name match, do not let an
        // incidental text-only non-neighbor function steal seed budget ahead of
        // the primary seed's direct callers/callees and cross-language bridges.
        if best_has_name
            && seeds_shown > 0
            && !name_scores.contains_key(&id)
            && !diagnosed.contains(&id)
            && !seed_neighbors.contains(&id)
        {
            continue;
        }
        let def = &index.definitions[id];
        report.line(format!(
            "{} [{}] {}{}",
            def.qualified,
            def.kind,
            format_pack_location(def, None),
            if changes.contains(&def.location.file) {
                " [working tree change]"
            } else {
                ""
            }
        ));
        excerpt(
            workspace,
            index,
            cache,
            &mut shown_files,
            &def.location.file,
            def.declaration,
            def.declaration,
            &mut report,
            memory.as_deref_mut(),
        );
        seen.insert(id);
        found_exact_code_seed |= names[id].is_code;
        for edge in index.incoming(id) {
            if let Some(from) = edge.from {
                seed_neighbors.insert(from);
            }
        }
        for edge in index.outgoing(id) {
            seed_neighbors.insert(edge.to);
        }
        queue.push_back((id, 0));
        seeds_shown += 1;
    }

    // Plan the BFS dependency cone in memory (capped at 24 symbols / 32 unique
    // links), prioritizing direct structural callers/callees and cross-language
    // bridges ahead of weak imports/references, collapsing duplicate edge labels
    // between the same pair, and hoisting deep `>= 2`-segment shared paths.
    struct PlannedEdgeLabel<'a> {
        kind_label: &'static str,
        inferred: bool,
        file: &'a str,
        spans: BTreeSet<(usize, usize)>,
    }
    struct PlannedLink<'a> {
        other: usize,
        direction: &'static str,
        discovered: bool,
        labels: Vec<PlannedEdgeLabel<'a>>,
    }
    struct PlannedParent<'a> {
        parent_id: usize,
        links: Vec<PlannedLink<'a>>,
    }

    let mut shown_links: BTreeSet<(Option<usize>, usize, Kind)> = BTreeSet::new();
    let mut planned_parents: Vec<PlannedParent<'_>> = Vec::new();
    let mut cone_paths: Vec<&str> = Vec::new();

    while let Some((id, depth)) = queue.pop_front() {
        if depth >= 3 || seen.len() >= 24 {
            continue;
        }
        let parent_def = &index.definitions[id];

        type LabelKey<'a> = (&'static str, bool, &'a str);
        struct NeighborAcc<'a> {
            other: usize,
            direction: &'static str,
            best_key: (bool, usize, Reverse<usize>, &'a Location, usize),
            raw_keys: Vec<(Option<usize>, usize, Kind)>,
            labels: Vec<(LabelKey<'a>, BTreeSet<(usize, usize)>)>,
            label_idx: HashMap<LabelKey<'a>, usize>,
        }

        let mut grouped: Vec<NeighborAcc<'_>> = Vec::new();
        let mut by_neighbor: HashMap<(usize, &'static str), usize> = HashMap::new();

        let adjacent = index
            .incoming(id)
            .filter_map(|edge| edge.from.map(|from| (from, edge, "from")))
            .chain(index.outgoing(id).map(|edge| (edge.to, edge, "to")));

        for (other, edge, direction) in adjacent {
            let raw_key = (edge.from, edge.to, edge.kind);
            if shown_links.contains(&raw_key) {
                continue;
            }
            let other_def = &index.definitions[other];
            let support_tier = if wants_support || failure_task {
                !other_def.test
            } else {
                is_support_definition(other_def)
            };
            let edge_tier = cone_edge_tier(index, parent_def, other_def, edge);
            let affinity = ranks.get(&other).copied().unwrap_or(0);
            let sort_key = (
                support_tier,
                edge_tier,
                Reverse(affinity),
                &edge.location,
                other,
            );
            let label_key = (
                relationship(edge.kind),
                edge.inferred,
                edge.location.file.as_str(),
            );
            let slot = *by_neighbor.entry((other, direction)).or_insert_with(|| {
                let pos = grouped.len();
                grouped.push(NeighborAcc {
                    other,
                    direction,
                    best_key: sort_key,
                    raw_keys: Vec::new(),
                    labels: Vec::new(),
                    label_idx: HashMap::new(),
                });
                pos
            });
            let acc = &mut grouped[slot];
            if sort_key < acc.best_key {
                acc.best_key = sort_key;
            }
            if !acc.raw_keys.contains(&raw_key) {
                acc.raw_keys.push(raw_key);
            }
            let lslot = *acc.label_idx.entry(label_key).or_insert_with(|| {
                let pos = acc.labels.len();
                acc.labels.push((label_key, BTreeSet::new()));
                pos
            });
            acc.labels[lslot]
                .1
                .insert((edge.location.start, edge.location.end));
        }

        grouped.sort_by(|a, b| a.best_key.cmp(&b.best_key));

        let mut parent_links: Vec<PlannedLink<'_>> = Vec::new();
        for acc in grouped {
            if seen.len() >= 24 || shown_links.len() >= 32 {
                break;
            }
            let mut added_any = false;
            for raw_key in acc.raw_keys {
                if shown_links.len() < 32 && shown_links.insert(raw_key) {
                    added_any = true;
                }
            }
            if !added_any {
                continue;
            }
            let discovered = seen.insert(acc.other);
            let target = &index.definitions[acc.other];
            if !qualified_includes_file(target) {
                cone_paths.push(target.location.file.as_str());
            }
            let labels: Vec<PlannedEdgeLabel<'_>> = acc
                .labels
                .into_iter()
                .map(|((kind_label, inferred, file), spans)| {
                    if file != target.location.file.as_str() {
                        cone_paths.push(file);
                    }
                    PlannedEdgeLabel {
                        kind_label,
                        inferred,
                        file,
                        spans,
                    }
                })
                .collect();
            if discovered {
                queue.push_back((acc.other, depth + 1));
            }
            parent_links.push(PlannedLink {
                other: acc.other,
                direction: acc.direction,
                discovered,
                labels,
            });
        }
        if !parent_links.is_empty() {
            planned_parents.push(PlannedParent {
                parent_id: id,
                links: parent_links,
            });
        }
    }

    let cone_base = hoistable_dir_prefix(&cone_paths, "  base: ");
    if let Some(prefix) = &cone_base
        && !planned_parents.is_empty()
        && !report.clipped
    {
        report.line(format!("  base: {prefix}"));
    }

    for parent in planned_parents {
        if report.clipped {
            break;
        }
        report.line(format!(
            "  {} links:",
            index.definitions[parent.parent_id].qualified
        ));
        if report.clipped {
            break;
        }
        for link in parent.links {
            if report.clipped {
                break;
            }
            let target = &index.definitions[link.other];
            let target_kind = if target.test { "test" } else { &target.kind };
            let target_loc = format_pack_location(target, cone_base.as_deref());
            let format_link_span = |label: &PlannedEdgeLabel<'_>| {
                if label.file == target.location.file {
                    format_same_file_spans(&label.spans)
                } else {
                    format_line_spans_with_base(label.file, &label.spans, cone_base.as_deref())
                }
            };
            if link.labels.len() == 1 {
                let label = &link.labels[0];
                let confidence = if label.inferred { "possible " } else { "" };
                report.line(format!(
                    "    {confidence}{} {} {} [{target_kind}] {target_loc} (link {})",
                    label.kind_label,
                    link.direction,
                    target.qualified,
                    format_link_span(label),
                ));
            } else {
                let parts: Vec<String> = link
                    .labels
                    .iter()
                    .map(|label| {
                        let confidence = if label.inferred { "possible " } else { "" };
                        format!(
                            "{confidence}{} ({})",
                            label.kind_label,
                            format_link_span(label)
                        )
                    })
                    .collect();
                report.line(format!(
                    "    {} {} {} [{target_kind}] {target_loc}",
                    parts.join(", "),
                    link.direction,
                    target.qualified,
                ));
            }
            if link.discovered && !report.clipped {
                excerpt(
                    workspace,
                    index,
                    cache,
                    &mut shown_files,
                    &target.location.file,
                    target.declaration,
                    target.location.end.min(target.declaration + 2),
                    &mut report,
                    memory.as_deref_mut(),
                );
            }
        }
    }

    for (at, hit) in passages.iter().enumerate() {
        if report.clipped || shown_text.len() >= 5 {
            break;
        }
        // When the task explicitly named a code identifier that resolved to a
        // symbol in the dependency cone, skip incidental plain-text matches that
        // only hit a split sub-word (e.g. "order" from "purchase_orders").
        if found_exact_code_seed && !hit.matched_explicit {
            continue;
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
            cache,
            &mut shown_files,
            hit,
            &mut report,
            memory.as_deref_mut(),
        );
        shown_text.insert(at);
    }
    let inspected: BTreeMap<_, _> = shown_files
        .iter()
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
    use std::fs;

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

    #[test]
    fn pack_caches_file_reads_across_passages_and_multi_symbol_excerpts() {
        let manifest = "[package]\nname = \"cachedemo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let lib = concat!(
            "pub fn core_settlement(v: u64) -> u64 {\n    v + 1\n}\n\n",
            "pub fn settle_card(v: u64) -> u64 {\n    core_settlement(v)\n}\n\n",
            "pub fn settle_wire(v: u64) -> u64 {\n    core_settlement(v) + core_settlement(v + 1)\n}\n\n",
            "pub fn settle_batch(v: u64) -> u64 {\n    let a = settle_card(v);\n    settle_wire(a)\n}\n",
        );
        let (_dir, workspace) = temp_workspace(&[("Cargo.toml", manifest), ("src/lib.rs", lib)]);
        let index = Index::open(&workspace).unwrap();

        let mut cache = SourceCache::default();
        let first = pack_report_with_cache(
            &workspace,
            &mut cache,
            &index,
            "core_settlement",
            8000,
            None,
        )
        .unwrap()
        .0
        .into_string();
        assert_eq!(
            cache.reads(),
            1,
            "multi-symbol pack touching src/lib.rs across passages + 4 excerpts must read disk once"
        );
        let second = pack_report_with_cache(
            &workspace,
            &mut cache,
            &index,
            "core_settlement",
            8000,
            None,
        )
        .unwrap()
        .0
        .into_string();
        assert_eq!(
            cache.reads(),
            1,
            "second pack on warm cache must do 0 reads"
        );
        assert_eq!(first, second, "cached pack output must stay deterministic");
        assert!(first.contains("core_settlement"), "{first}");
        assert!(first.contains("settle_card"), "{first}");
        assert!(first.contains("settle_wire"), "{first}");
    }

    #[test]
    fn cross_language_dependency_cone_outranks_incidental_text_under_tight_budget() {
        let (_dir, workspace) = temp_workspace(&[
            (
                "db/schema.sql",
                "CREATE TABLE purchase_orders (\n  order_id INTEGER PRIMARY KEY\n);\n",
            ),
            (
                "backend/orders.py",
                "from fastapi import APIRouter\n\nrouter = APIRouter()\n\n@router.get(\"/api/orders\")\ndef list_orders():\n    return database.execute(\"SELECT order_id FROM purchase_orders\")\n",
            ),
            (
                "frontend/orders.ts",
                "export async function fetchOrders() {\n  const response = await fetch('/api/orders');\n  return response.json();\n}\n",
            ),
            (
                "docs/glossary.md",
                "# Order glossary\n\nEvery order has an order status and order history.\nAn order is created by an order form.\n",
            ),
            (
                "docs/incidental.md",
                "# Release checklist\n\nVerify purchase_orders metrics in dashboard after deploy.\n",
            ),
        ]);
        let index = Index::open(&workspace).unwrap();

        // Tight budget must pack the cross-language SQL + HTTP dependency cone
        // (`purchase_orders` <- `list_orders` <- `fetchOrders`) ahead of
        // incidental plain-text mentions (`docs/incidental.md`).
        let tight = pack_report(&workspace, &index, "purchase_orders", 640, None)
            .unwrap()
            .0
            .into_string();
        assert!(tight.len() <= 640, "len={} > 640\n{tight}", tight.len());
        assert!(
            tight.contains("db/schema.sql::purchase_orders [table] :1-3\n"),
            "polyglot header must drop redundant file repetition:\n{tight}"
        );
        assert!(
            tight.contains("possible SQL relation from backend/orders.py::list_orders [function] :5-7 (link :7)\n"),
            "direct SQL bridge must appear:\n{tight}"
        );
        assert!(
            tight.contains("possible HTTP route from frontend/orders.ts::fetchOrders [function] :1-4 (link :2)\n"),
            "cross-language HTTP bridge must appear before incidental text:\n{tight}"
        );
        assert!(
            !tight.contains("docs/glossary.md"),
            "split sub-token 'order' noise must not displace code cone:\n{tight}"
        );

        // With generous budget, an explicit mention of `purchase_orders` in docs
        // still appears AFTER the dependency cone, while split-only `glossary.md` stays excluded.
        let full = pack_report(&workspace, &index, "purchase_orders", 8000, None)
            .unwrap()
            .0
            .into_string();
        let http_pos = full.find("fetchOrders").unwrap();
        let doc_pos = full.find("docs/incidental.md").unwrap();
        assert!(
            http_pos < doc_pos,
            "cross-language cone must precede incidental text:\n{full}"
        );
        assert!(!full.contains("docs/glossary.md"), "{full}");
    }
}
