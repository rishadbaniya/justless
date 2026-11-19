//! Recognize literal lists. Operators inside shell quotes are argument text.
use std::path::{Component, Path};

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Join {
    Always,
    Success,
    Failure,
}

pub(super) struct Piece<'a> {
    pub header: &'a str,
    pub join: Join,
    pub dir: Option<String>,
    pub source: Source,
    pub filters: Vec<Filter>,
}

pub(super) enum Source {
    File {
        file: String,
        select: Filter,
        numbered: bool,
    },
    /// `git show REV:path`: a file as a revision or the index holds it.
    Object(String),
    Git(Vec<String>),
    Search(Vec<String>),
}

#[derive(Clone)]
pub(super) enum Filter {
    All,
    First(usize),
    Last(usize),
    From(usize),
    Sed(Vec<(usize, usize)>),
    Columns(Vec<(usize, usize)>),
}

fn number(s: &str) -> Option<usize> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn address(s: &str) -> Option<usize> {
    number(s).filter(|n| *n > 0)
}

fn count(program: &str, args: &[String]) -> Option<Filter> {
    let value = match args {
        [] => "10",
        [n] => n.strip_prefix('-')?,
        [flag, n] if flag == "-n" => n,
        _ => return None,
    };
    if program == "head" {
        return Some(Filter::First(number(value)?));
    }
    match value.strip_prefix('+') {
        Some(n) => Some(Filter::From(address(n)?)),
        None => Some(Filter::Last(number(value)?)),
    }
}

fn sed(args: &[String]) -> Option<Filter> {
    let [flag, script] = args else { return None };
    if flag != "-n" {
        return None;
    }
    let ranges = script
        .split(';')
        .map(|item| {
            let span = item.trim().strip_suffix('p')?;
            let (a, b) = span.split_once(',').unwrap_or((span, span));
            let first = address(a)?;
            let last = if b == "$" { usize::MAX } else { address(b)? };
            Some((first, last.max(first)))
        })
        .collect::<Option<Vec<_>>>()?;
    (ranges.len() <= 32).then_some(Filter::Sed(ranges))
}

fn columns(args: &[String]) -> Option<Filter> {
    let spec = match args {
        [flag, spec] if flag == "-c" => spec.as_str(),
        [flag] => flag.strip_prefix("-c")?,
        _ => return None,
    };
    let spans = spec
        .split(',')
        .map(|item| {
            let (a, b) = item.split_once('-').unwrap_or((item, item));
            if a.is_empty() && b.is_empty() {
                return None;
            }
            let first = if a.is_empty() { 1 } else { address(a)? };
            let last = if b.is_empty() {
                usize::MAX
            } else {
                address(b)?
            };
            (first <= last).then_some((first, last))
        })
        .collect::<Option<Vec<_>>>()?;
    (spans.len() <= 32).then_some(Filter::Columns(spans))
}

pub(super) fn local(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('-')
        && !path.chars().any(char::is_control)
        && !Path::new(path).components().any(|c| {
            c == Component::ParentDir || c.as_os_str() == ".git" || c.as_os_str() == ".justless"
        })
}

pub(super) fn search_paths(words: &[String]) -> Option<&[String]> {
    let mut i = 1;
    while let Some(word) = words.get(i) {
        if word == "--" {
            i += 1;
            break;
        }
        let Some(flags) = word.strip_prefix('-') else {
            break;
        };
        if flags.is_empty() || !flags.bytes().all(|c| b"nHhiFvwxslE".contains(&c)) {
            return None;
        }
        // rg uses -E for encoding, which takes a value.
        if words[0] == "rg" && flags.contains('E') {
            return None;
        }
        i += 1;
    }
    // The pattern is one operand. Never forward later options, even on programs
    // that permute options after operands.
    (words.get(i).is_some()
        && words[i + 1..].iter().all(|p| local(p))
        && (words[0] == "rg" || i + 1 < words.len()))
    .then(|| &words[i + 1..])
}

fn piece<'a>(
    header: &'a str,
    stages: &[&str],
    join: Join,
    dir: Option<String>,
) -> Option<Piece<'a>> {
    let mut stages = stages.iter().map(|s| shlex::split(s));
    let words = stages.next()??;
    let words = {
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        crate::wrappers::effective(&refs)?
    };
    let (program, args) = words.split_first()?;
    let object = crate::shellread::show(&words).map(str::to_owned);
    let source = match program.as_str() {
        "git" if object.is_some() => Source::Object(object?),
        "git" if super::git::supported(args) => Source::Git(words),
        "rg" | "grep" if search_paths(&words).is_some() => Source::Search(words),
        "sed" | "cat" | "head" | "tail" | "nl" => {
            let (file, opts) = args.split_last()?;
            if !local(file) {
                return None;
            }
            let select = match program.as_str() {
                "cat" if opts.is_empty() => Filter::All,
                "nl" if opts == ["-ba"] || opts == ["-b", "a"] => Filter::All,
                "head" | "tail" => count(program, opts)?,
                "sed" => sed(opts)?,
                _ => return None,
            };
            Source::File {
                file: file.clone(),
                select,
                numbered: program == "nl",
            }
        }
        _ => return None,
    };
    let mut filters = Vec::new();
    for words in stages {
        let words = words?;
        let (program, args) = words.split_first()?;
        filters.push(match program.as_str() {
            "sed"
                if filters.is_empty()
                    && matches!(
                        source,
                        Source::File { numbered: true, .. } | Source::Object(_)
                    ) =>
            {
                sed(args)?
            }
            "cut" => columns(args)?,
            "head" | "tail" => count(program, args)?,
            _ => return None,
        });
    }
    Some(Piece {
        header,
        join,
        dir,
        source,
        filters,
    })
}

fn change(stage: &str) -> Option<Option<String>> {
    let words = shlex::split(stage)?;
    match words.as_slice() {
        [word] if crate::wrappers::assignment(word) => Some(None),
        [word, dir] if word == "cd" => local(dir).then(|| Some(dir.clone())),
        _ => None,
    }
}

fn enter(base: &mut Option<String>, dir: &str) {
    let joined = match (base.as_deref(), Path::new(dir).is_absolute()) {
        (_, true) => dir.to_owned(),
        (Some(base), false) => Path::new(base).join(dir).to_string_lossy().into_owned(),
        (None, false) => dir.to_owned(),
    };
    *base = Some(joined);
}

/// The shell expands an unquoted `~` at the start of a word or after `=` or
/// `:`, so only one inside a name such as `HEAD~1` is literal.
fn tilde(command: &str, at: usize) -> bool {
    command[..at]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_alphanumeric() || "^~_.-/@".contains(c))
}

fn literal(command: &str) -> bool {
    let mut quote = None;
    let mut chars = command.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if q == '"' && c == '\\' {
                match chars.next() {
                    Some((_, next)) if !next.is_control() => {}
                    _ => return false,
                }
            } else if q == '"' && matches!(c, '$' | '`') || c.is_control() {
                return false;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '\\' => match chars.next() {
                Some((_, next)) if !next.is_control() => {}
                _ => return false,
            },
            '~' if tilde(command, i) => {}
            ';' | '|' | '&' | '$' | '`' | '(' | ')' | '<' | '>' | '{' | '}' | '[' | ']' | '*'
            | '?' | '#' | '~' | '!' => return false,
            c if c.is_control() && c != '\t' => return false,
            _ => {}
        }
    }
    quote.is_none()
}

pub(super) fn single(command: &str) -> Option<Vec<Piece<'_>>> {
    if command.len() > 16 * 1024 || !literal(command) {
        return None;
    }
    let words = shlex::split(command)?;
    let refs: Vec<&str> = words.iter().map(String::as_str).collect();
    let effective = crate::wrappers::effective(&refs)?;
    let differs =
        effective.len() != refs.len() || effective.iter().zip(&refs).any(|(a, b)| a.as_str() != *b);
    let program = effective.first().map(String::as_str);
    if !differs && !matches!(program, Some("cat")) {
        return None;
    }
    piece(command, &[command], Join::Always, None).map(|piece| vec![piece])
}

pub(super) fn list(command: &str) -> Option<Vec<Piece<'_>>> {
    if command.len() > 16 * 1024 || !command.contains([';', '\n', '&', '|']) {
        return None;
    }
    let mut pieces = Vec::new();
    let mut stages = Vec::new();
    let (mut start, mut segment) = (0, 0);
    let mut quote = None;
    let mut join = Join::Always;
    let mut base = None;
    let mut chars = command.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if q == '"' && c == '\\' {
                chars.next()?;
            } else if q == '"' && matches!(c, '$' | '`') || c.is_control() {
                return None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '\\' => {
                let (_, next) = chars.next()?;
                if next.is_control() {
                    return None;
                }
            }
            ';' | '\n' | '&' | '|' => {
                let stage = command[start..i].trim();
                if stage.is_empty() {
                    if c == '\n' && stages.is_empty() {
                        start = i + 1;
                        segment = start;
                        continue;
                    }
                    return None;
                }
                stages.push(stage);
                if stages.len() > 8 {
                    return None;
                }
                let paired =
                    matches!(c, '&' | '|') && chars.peek().is_some_and(|(_, next)| *next == c);
                if paired {
                    chars.next();
                }
                start = i + 1 + usize::from(paired);
                if c == '|' && !paired {
                    continue;
                }
                if c == '&' && !paired {
                    return None;
                }
                let next = match c {
                    '&' => Join::Success,
                    '|' => Join::Failure,
                    _ => Join::Always,
                };
                match (stages.len() == 1).then(|| change(stages[0])).flatten() {
                    Some(Some(dir)) => {
                        // Later reads start in `dir` only if the shell surely
                        // entered it. Among the pieces only a search can fail.
                        let skippable = match join {
                            Join::Always => false,
                            Join::Success => pieces
                                .iter()
                                .any(|p: &Piece| matches!(p.source, Source::Search(_))),
                            Join::Failure => true,
                        };
                        if next == Join::Failure || skippable {
                            return None;
                        }
                        enter(&mut base, &dir);
                    }
                    Some(None) => {}
                    None => {
                        pieces.push(piece(
                            command[segment..i].trim(),
                            &stages,
                            join,
                            base.clone(),
                        )?);
                        if pieces.len() > 32 {
                            return None;
                        }
                    }
                }
                stages.clear();
                segment = start;
                join = next;
            }
            '~' if tilde(command, i) => {}
            '$' | '`' | '(' | ')' | '<' | '>' | '{' | '}' | '[' | ']' | '*' | '?' | '#' | '~'
            | '!' => return None,
            c if c.is_control() && c != '\t' => return None,
            _ => {}
        }
    }
    if quote.is_some() {
        return None;
    }
    let final_stage = command[start..].trim();
    if !final_stage.is_empty() {
        stages.push(final_stage);
        if stages.len() > 8 {
            return None;
        }
        if stages.len() != 1 || change(stages[0]).is_none() {
            pieces.push(piece(
                command[segment..].trim(),
                &stages,
                join,
                base.clone(),
            )?);
        }
    } else if !stages.is_empty() || join != Join::Always {
        return None;
    }
    (1..=32).contains(&pieces.len()).then_some(pieces)
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_tilde_inside_a_word_is_literal() {
        assert!(super::list("git show HEAD~1:a.rs; sed -n 1,2p b").is_some());
        assert!(super::single("timeout 5 git show main~2:a.rs").is_some());
        for list in [
            "cat ~/a; cat b",
            "cat a; cat x=~/b",
            "git show HEAD:~/a; cat b",
            "cat a;~/b",
        ] {
            assert!(super::list(list).is_none(), "{list}");
        }
    }
}
