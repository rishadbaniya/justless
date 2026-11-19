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
        numbered: Numbers,
    },
    /// `cat A B …`: each whole file in turn.
    Files(Vec<String>),
    /// `git show REV:path`: a file as a revision or the index holds it.
    Object(String),
    Git(Vec<String>),
    Search(Vec<String>),
    /// What the shell prints itself: a literal `echo` or `printf`, or `true`.
    Literal(String),
    /// `pwd`: the directory the shell is in, as it names it.
    Pwd,
    /// `ls` or `wc` of workspace paths, rerun with the same words.
    Listing(Vec<String>),
}

/// How a file read numbers its lines.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Numbers {
    None,
    /// `nl -ba`, which always ends a numbered line with a newline.
    Nl,
    /// `cat -n`, which keeps each line's own ending.
    Cat,
}

impl Source {
    /// The reads a list has to include before it runs as written.
    fn read(&self) -> bool {
        match self {
            Self::File { .. } | Self::Files(_) | Self::Object(_) | Self::Search(_) => true,
            Self::Git(words) => matches!(words[1].as_str(), "show" | "diff"),
            Self::Literal(_) | Self::Pwd | Self::Listing(_) => false,
        }
    }
}

/// Whether every piece is a read the command hook has always left to the
/// post-run hook. A list that mixes in other pieces keeps the command hook's
/// route, such as a `git log` it runs through Justless, so the post-run hook
/// compacts it only when the command hook left it as written.
pub(super) fn reads_only(pieces: &[Piece<'_>]) -> bool {
    pieces.iter().all(|piece| {
        piece.source.read()
            && !matches!(
                piece.source,
                Source::File {
                    numbered: Numbers::Cat,
                    ..
                }
            )
    })
}

/// The text `echo` prints, when the shells agree on it: bash and zsh both
/// print words without a backslash or a leading option as they are, except
/// that zsh ends its options at a lone `-` and expands a leading `=`.
fn echo(args: &[String]) -> Option<String> {
    let option = |w: &str| {
        w.strip_prefix('-')
            .is_some_and(|f| f.is_empty() || f.bytes().all(|b| b"neE".contains(&b)))
    };
    if args.first().is_some_and(|w| option(w))
        || args.iter().any(|w| w.contains('\\') || w.starts_with('='))
    {
        return None;
    }
    Some(args.join(" ") + "\n")
}

/// The text `printf FORMAT` prints for a format of plain text, `%%` and the
/// common escapes, without further arguments.
fn printf(args: &[String]) -> Option<String> {
    let [format] = args else { return None };
    if format.starts_with('-') {
        return None;
    }
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        out.push(match c {
            '%' => chars.next().filter(|&c| c == '%')?,
            '\\' => match chars.next()? {
                'n' => '\n',
                't' => '\t',
                '\\' => '\\',
                _ => return None,
            },
            c => c,
        });
    }
    Some(out)
}

/// Whether `ls` or `wc` lists the operands of `args` in a form the recheck
/// reproduces: names alone, or counts of lines, words and bytes of files.
fn listing(program: &str, args: &[String]) -> bool {
    let (flags, operands): (Vec<_>, Vec<_>) = args.iter().partition(|a| a.starts_with('-'));
    let allowed: &[u8] = if program == "ls" { b"1aA" } else { b"lwc" };
    flags
        .iter()
        .all(|f| f.len() > 1 && f[1..].bytes().all(|b| allowed.contains(&b)))
        && operands.iter().all(|p| local(p))
        // BSD tools take every word after the first operand as an operand.
        && args.iter().skip_while(|a| a.starts_with('-')).all(|a| !a.starts_with('-'))
        && (program == "ls" || !operands.is_empty())
        && operands.len() <= 32
}

/// The operands of an `ls` or `wc`.
pub(super) fn operands(words: &[String]) -> impl Iterator<Item = &String> {
    words[1..].iter().filter(|a| !a.starts_with('-'))
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
    let last = stages.len().checked_sub(1)?;
    let mut stages = stages.iter().map(|s| shlex::split(s)).enumerate();
    let words = stages.next()?.1?;
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
        "echo" => Source::Literal(echo(args)?),
        "printf" => Source::Literal(printf(args)?),
        "true" | ":" if args.is_empty() => Source::Literal(String::new()),
        "pwd" if args.is_empty() || args == ["-L"] => Source::Pwd,
        "ls" | "wc" if listing(program, args) => Source::Listing(words),
        "cat" if args.len() == 2 && args[0] == "-n" && local(&args[1]) => Source::File {
            file: args[1].clone(),
            select: Filter::All,
            numbered: Numbers::Cat,
        },
        "cat" if args.len() > 1 => {
            if !args.iter().all(|file| local(file)) {
                return None;
            }
            Source::Files(args.to_vec())
        }
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
                numbered: if program == "nl" {
                    Numbers::Nl
                } else {
                    Numbers::None
                },
            }
        }
        _ => return None,
    };
    let mut filters = Vec::new();
    for (at, words) in stages {
        let words = words?;
        let (program, args) = words.split_first()?;
        filters.push(match program.as_str() {
            // A last `| cat` or `| cat -` copies its input unchanged. It stays a
            // stage, so a failing search before it is still told apart.
            "cat" if at == last && (args.is_empty() || args == ["-"]) => Filter::All,
            "sed"
                if filters.is_empty()
                    && matches!(
                        source,
                        Source::File {
                            numbered: Numbers::Nl | Numbers::Cat,
                            ..
                        } | Source::File {
                            select: Filter::All,
                            ..
                        } | Source::Object(_)
                    ) =>
            {
                sed(args)?
            }
            "cut" => columns(args)?,
            "head" | "tail" => count(program, args)?,
            _ => return None,
        });
    }
    // A filter applies to the concatenation, not to each file.
    if matches!(source, Source::Files(_)) && filters.iter().any(|f| !matches!(f, Filter::All)) {
        return None;
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
    // Only a list mixes in other pieces.
    piece(command, &[command], Join::Always, None)
        .map(|piece| vec![piece])
        .filter(|pieces| reads_only(pieces) && outputs(pieces) <= 32)
}

/// How many outputs the pieces print: one for each file of a `cat A B …`.
pub(super) fn outputs(pieces: &[Piece<'_>]) -> usize {
    pieces
        .iter()
        .map(|piece| match &piece.source {
            Source::Files(files) => files.len(),
            _ => 1,
        })
        .sum()
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
                        // entered it. Among the pieces only a search can fail:
                        // the recheck rejects any other piece that fails.
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
                        if outputs(&pieces) > 32 {
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
    // Output the shell prints itself, a listing, `git log` or `git status`
    // joins a list of reads; alone it keeps the command hook's route.
    (pieces.iter().any(|piece| piece.source.read()) && (1..=32).contains(&outputs(&pieces)))
        .then_some(pieces)
}

#[cfg(test)]
mod tests {
    use super::Source;

    /// What a list's second piece is when the list parses.
    fn joined(piece: &str) -> Option<String> {
        let list = format!("cat a; {piece}");
        let pieces = super::list(&list)?;
        Some(match &pieces[1].source {
            Source::Literal(text) => format!("literal {text:?}"),
            Source::Pwd => "pwd".into(),
            Source::Listing(words) => format!("listing {}", words.join(" ")),
            Source::Git(words) => format!("git {}", words[1]),
            Source::File { .. } => "file".into(),
            _ => "other".into(),
        })
    }

    #[test]
    fn output_the_shell_prints_and_listings_join_reads() {
        for (piece, expected) in [
            ("echo ----", Some(r#"literal "----\n""#)),
            ("echo", Some(r#"literal "\n""#)),
            ("echo 'a  b' c", Some(r#"literal "a  b c\n""#)),
            ("echo --", Some(r#"literal "--\n""#)),
            ("printf '%%\\t\\n'", Some(r#"literal "%\t\n""#)),
            ("true", Some(r#"literal """#)),
            (":", Some(r#"literal """#)),
            ("pwd", Some("pwd")),
            ("ls", Some("listing ls")),
            ("ls -A -1 src docs", Some("listing ls -A -1 src docs")),
            ("wc -l a b", Some("listing wc -l a b")),
            ("cat -n b", Some("file")),
            ("git status --short", Some("git status")),
            ("git status --porcelain -uno -- src", Some("git status")),
            ("git log --oneline -3 main -- src", Some("git log")),
            ("git log --format='%h %s%n%b' -n2", Some("git log")),
            ("git log --oneline main..HEAD | head -5", Some("git log")),
            // Shells disagree, or the text is not literal.
            ("echo -n x", None),
            ("echo -", None),
            ("echo -e x", None),
            ("echo 'a\\nb'", None),
            ("echo =x", None),
            ("printf '%s' x", None),
            ("printf '%d'", None),
            ("printf '\\x41'", None),
            ("true x", None),
            ("pwd -P", None),
            // Listings that change with time, leave the workspace or read stdin.
            ("ls -l", None),
            ("ls -la src", None),
            ("ls ../x", None),
            ("ls .git", None),
            ("ls src -a", None),
            ("wc", None),
            ("wc -m a", None),
            ("cat -n a b", None),
            ("cat -n ../a", None),
            // Git forms the isolated repository cannot reproduce or that run more.
            ("git status", None),
            ("git status -sb", None),
            ("git status --short --branch", None),
            ("git log --show-signature -1", None),
            ("git log --format=%G? -1", None),
            ("git log --pretty='%+GS' -1", None),
            ("git log -n 3", None),
            ("git log --oneline -3 -- ../x", None),
        ] {
            assert_eq!(joined(piece).as_deref(), expected, "{piece}");
        }
    }

    #[test]
    fn a_list_needs_a_read_and_a_single_command_stays_a_read() {
        for list in [
            "echo a; pwd",
            "git status --short && git log --oneline -5",
            "ls src; wc -l a",
            "true; echo done",
        ] {
            assert!(super::list(list).is_none(), "{list}");
        }
        assert!(super::list("git diff; echo ---; git log -1 --oneline").is_some());
        for single in [
            "timeout 5 ls",
            "timeout 5 echo x",
            "env A=1 pwd",
            "cat -n a",
        ] {
            assert!(super::single(single).is_none(), "{single}");
        }
    }

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
