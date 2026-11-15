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
    pub source: Source,
    pub filters: Vec<Filter>,
}

pub(super) enum Source {
    File {
        file: String,
        select: Filter,
        numbered: bool,
    },
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

fn piece<'a>(header: &'a str, stages: &[&str], join: Join) -> Option<Piece<'a>> {
    let mut stages = stages.iter().map(|s| shlex::split(s));
    let words = stages.next()??;
    let words = {
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        crate::filters::effective(&refs)?
    };
    let (program, args) = words.split_first()?;
    let source = match program.as_str() {
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
                if filters.is_empty() && matches!(source, Source::File { numbered: true, .. }) =>
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
        source,
        filters,
    })
}

pub(super) fn single(command: &str) -> Option<Vec<Piece<'_>>> {
    let words = shlex::split(command)?;
    let refs: Vec<&str> = words.iter().map(String::as_str).collect();
    let effective = crate::filters::effective(&refs)?;
    if effective.len() == refs.len() && effective.iter().zip(&refs).all(|(a, b)| a == b) {
        return None;
    }
    piece(command, &[command], Join::Always).map(|piece| vec![piece])
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
                pieces.push(piece(command[segment..i].trim(), &stages, join)?);
                if pieces.len() > 32 {
                    return None;
                }
                stages.clear();
                segment = start;
                join = match c {
                    '&' => Join::Success,
                    '|' => Join::Failure,
                    _ => Join::Always,
                };
            }
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
        pieces.push(piece(command[segment..].trim(), &stages, join)?);
    } else if !stages.is_empty() || join != Join::Always {
        return None;
    }
    (2..=32).contains(&pieces.len()).then_some(pieces)
}
