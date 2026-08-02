use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;

use anyhow::{Result, ensure};

use crate::context::{Report, relationship};
use crate::history;
use crate::index::{Fingerprint, Index, nul_paths};
use crate::summary::short;
use crate::workspace::{Location, Workspace};

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

struct Passage {
    score: usize,
    location: Location,
    text: String,
}

// Trigram fingerprints cheaply eliminate impossible files. Score verified lines by
// how rare their matching terms are in this repository, then promote enclosing symbols.
fn passages(
    workspace: &Workspace,
    index: &Index,
    words: &BTreeSet<String>,
    question: &str,
) -> Vec<Passage> {
    let mut needles: BTreeSet<String> = words.clone();
    for fragment in question
        .split_whitespace()
        .map(|s| s.trim_matches(['\'', '"', '`', '?', '.', ',']))
    {
        if fragment.len() > 2 && fragment.contains(['/', '-', ':']) {
            needles.insert(fragment.to_owned());
        }
    }
    let mut weighted = Vec::new();
    for word in needles {
        let fp = Fingerprint::new(&word);
        let docs = index
            .fingerprints
            .values()
            .filter(|candidate| candidate.includes(fp))
            .count();
        if docs == 0 {
            continue;
        }
        let weight = (1 + (index.files.len() / docs).ilog2() as usize) * word.len().min(16);
        weighted.push((word, fp, weight));
    }
    let mut files = Vec::new();
    for (file, fp) in &index.fingerprints {
        let score: usize = weighted
            .iter()
            .filter(|(_, needle, _)| fp.includes(*needle))
            .map(|(_, _, w)| w)
            .sum();
        if score > 0 {
            files.push((Reverse(score), file));
        }
    }
    files.sort_unstable();
    let mut results = Vec::new();
    for (_, file) in files.into_iter().take(96) {
        let Ok(path) = workspace.source_path(file) else {
            continue;
        };
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let mut best = 0;
        let mut passage = None;
        for (i, line) in text.lines().enumerate() {
            let folded = line.to_lowercase();
            let score: usize = weighted
                .iter()
                .filter(|(word, _, _)| folded.contains(word))
                .map(|(_, _, w)| w)
                .sum();
            if score > best {
                best = score;
                passage = Some(Passage {
                    score,
                    location: Location {
                        file: file.clone(),
                        start: i + 1,
                        end: i + 1,
                    },
                    text: short(line.trim(), 160),
                });
            }
        }
        results.extend(passage);
    }
    results.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.location.cmp(&b.location))
    });
    results.truncate(8);
    results
}

pub fn pack(workspace: &Workspace, index: &Index, task: &str, cap: usize) -> Result<()> {
    ensure!(!task.trim().is_empty(), "provide a task or symbol");
    let words = terms(task);
    let question = task.to_lowercase();
    let changes = changed_files(workspace);
    let failure_task = ["fail", "error", "panic", "diagnos", "regress"]
        .iter()
        .any(|term| question.contains(term));
    let mut ranks = BTreeMap::new();
    for (id, def) in index.definitions.iter().enumerate() {
        let name = def.qualified.rsplit("::").next().unwrap_or(&def.qualified);
        let exact = words.contains(&name.to_lowercase());
        let parts = terms(name);
        let hits = parts.intersection(&words).count();
        let file_hits = terms(&def.location.file).intersection(&words).count();
        let mut score = usize::from(exact) * 200 + hits * 16 + file_hits * 3;
        if score > 0 || (changes.contains(&def.location.file) && question.contains("chang")) {
            score += usize::from(changes.contains(&def.location.file)) * 4;
            ranks.insert(id, score);
        }
    }
    let passages = passages(workspace, index, &words, &question);
    for hit in &passages {
        for id in index.at(&hit.location) {
            *ranks.entry(id).or_default() += hit.score.min(80);
        }
    }
    let mut report = Report::new(cap);
    let mut found_run = false;
    let mut latest = BTreeSet::new();
    let mut sources = BTreeMap::new();
    for run in history::records(workspace, false)?.take(25) {
        let run = run?;
        if (!run.command_key.is_empty() && !latest.insert(run.command_key.clone()))
            || run.status == 0
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
    for (id, score) in ranked.into_iter().take(4) {
        if score < best / 2 || report.clipped {
            break;
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
    let mut inspected: BTreeMap<_, _> = sources
        .keys()
        .filter_map(|file| {
            index
                .files
                .get(file)
                .map(|hash| (file.clone(), hash.clone()))
        })
        .collect();
    let mut text_hits = 0;
    for hit in passages {
        if report.clipped || text_hits >= 5 {
            break;
        }
        let file = &hit.location.file;
        if seen.iter().any(|&id| {
            let d = &index.definitions[id];
            d.location.file == *file
                && (d.declaration..=d.declaration + 2).contains(&hit.location.start)
        }) {
            continue;
        }
        report.line(format!("Text {} {}", hit.location, hit.text));
        if let Some(hash) = index.files.get(file) {
            inspected.insert(file.clone(), hash.clone());
        }
        text_hits += 1;
    }
    if !found_run && seen.is_empty() && text_hits == 0 {
        report.line("No matching source or recorded failure. Try a symbol, file name, or an error from the log.");
    }
    history::saw_pack(workspace, inspected);
    report.finish()
}
