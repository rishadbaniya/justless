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
    /// A `grep`, rerun with the grep the shell ran.
    Grep(super::grep::Grep),
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
            Self::File { .. }
            | Self::Files(_)
            | Self::Object(_)
            | Self::Search(_)
            | Self::Grep(_) => true,
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

pub(super) fn number(s: &str) -> Option<usize> {
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

/// The lines an awk program prints when it is only a range of line numbers:
/// `NR>=A && NR<=B` or `NR==A,NR==B`, with or without a plain `{print}`.
fn awk(script: &str) -> Option<Filter> {
    let script: String = script.chars().filter(|c| !c.is_whitespace()).collect();
    let script = script
        .strip_suffix("{print}")
        .or_else(|| script.strip_suffix("{print$0}"))
        .unwrap_or(&script);
    let (first, last) = match script.strip_prefix("NR>=") {
        Some(rest) => rest.split_once("&&NR<=")?,
        None => script.strip_prefix("NR==")?.split_once(",NR==")?,
    };
    let (first, last) = (address(first)?, address(last)?);
    (first <= last).then(|| Filter::Sed(vec![(first, last)]))
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
        if flags.is_empty() || !flags.bytes().all(|c| b"nHhiFvwxslES".contains(&c)) {
            return None;
        }
        // rg uses -E for encoding (valued) and -S for smart-case (boolean); grep has no -S.
        if (words[0] == "rg" && flags.contains('E')) || (words[0] != "rg" && flags.contains('S')) {
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
    let original = stages.next()?.1?;
    let words = {
        let refs: Vec<&str> = original.iter().map(String::as_str).collect();
        crate::wrappers::effective(&refs)?
    };
    let (program, args) = words.split_first()?;
    let object = crate::shellread::show(&words).map(str::to_owned);
    let source = match program.as_str() {
        "git" if object.is_some() => Source::Object(object?),
        "git" if super::git::supported(args) => Source::Git(words),
        "rg" if search_paths(&words).is_some() => Source::Search(words),
        "grep" => Source::Grep(super::grep::parse(&original, words)?),
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
        "awk" => {
            let [script, file] = args else { return None };
            if !local(file) {
                return None;
            }
            Source::File {
                file: file.clone(),
                select: awk(script)?,
                numbered: Numbers::None,
            }
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

/// A `NAME=value` that stands alone as a command, and its value when the value
/// is a word of plain path characters, quoted or not: one bash and zsh both
/// expand to itself wherever `$NAME` is used, without splitting or globbing.
fn assigned(stage: &str) -> Option<(String, Option<String>)> {
    let words = shlex::split(stage)?;
    let [word] = words.as_slice() else {
        return None;
    };
    if !crate::wrappers::assignment(word) {
        return None;
    }
    let (name, value) = word.split_once('=')?;
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+:,./-".contains(c))
        && !stage.contains(['\\', '$', '`', '~']);
    Some((name.to_owned(), plain.then(|| value.to_owned())))
}

/// `stage` with each `$NAME` and `${NAME}` of `values` filled in, bare or in
/// double quotes, but not in single quotes or after a backslash. A name followed by `[` or `:` stays, since zsh reads a
/// subscript or a modifier there, and so does every other use of `$`, which
/// the list parser refuses.
fn fill(stage: &str, values: &[(String, String)], used: &mut bool) -> String {
    let mut out = String::with_capacity(stage.len());
    let mut quote = None;
    let mut rest = stage;
    while let Some(c) = rest.chars().next() {
        let mut take = c.len_utf8();
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            }
        } else if c == '\\' {
            take += rest[take..].chars().next().map_or(0, char::len_utf8);
        } else if quote == Some(c) {
            quote = None;
        } else if quote.is_none() && matches!(c, '\'' | '"') {
            quote = Some(c);
        } else if c == '$' {
            let (name, braced) = match rest[1..].strip_prefix('{') {
                Some(inner) => (inner.split_once('}').map_or("", |(name, _)| name), true),
                None => {
                    let end = rest[1..]
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .unwrap_or(rest.len() - 1);
                    (&rest[1..1 + end], false)
                }
            };
            let after = 1 + name.len() + 2 * usize::from(braced);
            let value = values
                .iter()
                .rev()
                .find(|(known, _)| known == name)
                .filter(|_| braced || !rest[after..].starts_with(['[', ':']));
            if let Some((_, value)) = value {
                out.push_str(value);
                *used = true;
                rest = &rest[after..];
                continue;
            }
        }
        out.push_str(&rest[..take]);
        rest = &rest[take..];
    }
    out
}

/// The line the shell runs once the variables it sets itself are filled in:
/// `D=/tmp/notes; sed -n 1,90p $D/a.md` reads `/tmp/notes/a.md`. An assignment
/// counts when it stands alone with a plain value (see [`assigned`]) where the
/// shell surely runs it and goes on: after the start of the line, `;` or a
/// newline, or `&&` behind nothing but assignments, and before `;`, a newline
/// or `&&`. It is dropped from the result, as it prints nothing. Any other
/// standalone assignment, such as one from a command substitution, leaves the
/// line as written, and so does a line that uses none of its names. A prefix
/// assignment, as in `D=/x cat $D/f`, sets nothing the line can use, which is
/// how the shell reads it too. The recheck still compares every byte.
pub(super) fn expand(command: &str) -> Option<String> {
    if command.len() > 16 * 1024 || !command.contains('$') || !command.contains('=') {
        return None;
    }
    let mut stages = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut chars = command.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if q == '"' && c == '\\' {
                chars.next()?;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '\\' => {
                chars.next()?;
            }
            ';' | '\n' | '&' | '|' => {
                let paired =
                    c != ';' && c != '\n' && chars.peek().is_some_and(|(_, next)| *next == c);
                if paired {
                    chars.next();
                }
                let end = i + 1 + usize::from(paired);
                stages.push((&command[start..i], &command[i..end]));
                start = end;
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return None;
    }
    stages.push((&command[start..], ""));
    let mut values: Vec<(String, String)> = Vec::new();
    let mut out = String::with_capacity(command.len());
    let (mut used, mut only_assignments, mut before) = (false, true, ";");
    for (stage, separator) in stages {
        let Some((name, value)) = assigned(stage.trim()) else {
            only_assignments = false;
            out.push_str(&fill(stage, &values, &mut used));
            out.push_str(separator);
            before = separator;
            continue;
        };
        let runs = matches!(before, ";" | "\n") || before == "&&" && only_assignments;
        if !runs || !matches!(separator, ";" | "\n" | "&&" | "") {
            return None;
        }
        values.push((name, value?));
        before = separator;
    }
    used.then(|| out.trim().to_owned())
}

/// Whether `name` matches a pattern of literal characters, `*` and `?`.
fn wildcard(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len()).any(|skip| wildcard(rest, &name[skip..])),
        Some((c, rest)) => name
            .split_first()
            .is_some_and(|(first, name)| (*c == '?' || c == first) && wildcard(rest, name)),
    }
}

/// The words the shell puts in place of `word`, whose last path component holds
/// `*` or `?`, in `base`: the names in that folder it matches, in byte order,
/// without dotfiles unless the pattern names the dot. `None` for no match, which
/// zsh refuses and bash leaves as written, for more than 64, and for any other
/// pattern, such as a class or a wildcard in a folder name. Without `base`, for
/// a line recorded in a transcript, the word stands for one file of its folder.
fn matches(word: &str, base: Option<&Path>) -> Option<Vec<String>> {
    let (folder, pattern) = match word.rsplit_once('/') {
        Some((folder, pattern)) => (format!("{folder}/"), pattern),
        None => (String::new(), word),
    };
    if folder.contains(['*', '?'])
        || word.contains(['[', ']', '{', '}', '~'])
        || word.starts_with('-')
        || !local(word)
    {
        return None;
    }
    let Some(base) = base else {
        return Some(vec![format!(
            "{folder}{}",
            pattern.replace(['*', '?'], "_")
        )]);
    };
    let pattern: Vec<char> = pattern.chars().collect();
    let mut names = Vec::new();
    for entry in std::fs::read_dir(base.join(&folder)).ok()?.flatten() {
        let name = entry.file_name().into_string().ok()?;
        let chars: Vec<char> = name.chars().collect();
        if (pattern.first() == Some(&'.') || !name.starts_with('.')) && wildcard(&pattern, &chars) {
            names.push(format!("{folder}{name}"));
        }
    }
    names.sort();
    (1..=64).contains(&names.len()).then_some(names)
}

/// The line with each bare word that holds `*` or `?` replaced by the files the
/// shell names for it (see [`matches`]), read from `base`, where the line
/// starts, and the folders a plain `cd` enters after it. Claude Code's main
/// session keeps a `cd`, so `base` can already be the folder a relative `cd`
/// entered. A word with a quote or a backslash is left as it is, as the shell
/// leaves it, and a word that names no file leaves the whole line as written.
/// The order is the shell's for plain names; where it is not, the recheck finds
/// the bytes differ and keeps the result.
pub(super) fn glob(command: &str, base: Option<&Path>) -> Option<String> {
    if command.len() > 16 * 1024 || !command.contains(['*', '?']) {
        return None;
    }
    let mut out = String::with_capacity(command.len());
    let mut folder = base.map(Path::to_path_buf);
    // The words of the stage so far, to spot a `cd`.
    let mut stage: Vec<&str> = Vec::new();
    let mut found = false;
    let mut rest = command;
    while !rest.is_empty() {
        let space = rest
            .find(|c: char| !c.is_whitespace() || c == '\n')
            .unwrap_or(rest.len());
        out.push_str(&rest[..space]);
        rest = &rest[space..];
        let Some(c) = rest.chars().next() else { break };
        if matches!(c, ';' | '\n' | '&' | '|') {
            if let ["cd", dir] = stage.as_slice()
                && let Some(folder) = &mut folder
            {
                let entered = folder.join(dir);
                // Already entered when the session's folder ends in it.
                if entered.is_dir() || !folder.ends_with(dir) {
                    *folder = entered;
                }
            }
            stage.clear();
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        // One word, with its quoted parts.
        let mut quote = None;
        let mut end = rest.len();
        let mut chars = rest.char_indices();
        while let Some((i, c)) = chars.next() {
            match quote {
                Some(q) if c == q => quote = None,
                Some('"') if c == '\\' => {
                    chars.next();
                }
                Some(_) => {}
                None if matches!(c, '\'' | '"') => quote = Some(c),
                None if c == '\\' => {
                    chars.next();
                }
                None if c.is_whitespace() || matches!(c, ';' | '&' | '|') => {
                    end = i;
                    break;
                }
                None => {}
            }
        }
        let word = &rest[..end];
        rest = &rest[end..];
        let bare = !word.contains(['\'', '"', '\\', '$', '`']);
        if bare && word.contains(['*', '?']) && !stage.is_empty() {
            let names = matches(word, folder.as_deref())?;
            let quoted: Option<Vec<_>> = names
                .iter()
                .map(|name| {
                    shlex::try_quote(name)
                        .ok()
                        .map(|quoted| quoted.into_owned())
                })
                .collect();
            out.push_str(&quoted?.join(" "));
            found = true;
        } else {
            out.push_str(word);
        }
        stage.push(word);
    }
    found.then_some(out)
}

/// One command that is not a list: a wrapped read, a `cat` or an awk range.
/// With `plain`, any read, which the single-file read otherwise takes.
pub(super) fn single(command: &str, plain: bool) -> Option<Vec<Piece<'_>>> {
    if command.len() > 16 * 1024 || !literal(command) {
        return None;
    }
    let words = shlex::split(command)?;
    let refs: Vec<&str> = words.iter().map(String::as_str).collect();
    let effective = crate::wrappers::effective(&refs)?;
    let differs =
        effective.len() != refs.len() || effective.iter().zip(&refs).any(|(a, b)| a.as_str() != *b);
    let program = effective.first().map(String::as_str);
    // The single-file read takes neither several files nor an awk range.
    if !plain && !differs && !matches!(program, Some("cat" | "awk")) {
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
                            Join::Success => pieces.iter().any(|p: &Piece| {
                                matches!(p.source, Source::Search(_) | Source::Grep(_))
                            }),
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
            assert!(super::single(single, false).is_none(), "{single}");
        }
    }

    #[test]
    fn variables_a_line_sets_to_plain_words_are_filled_in() {
        for (line, expected) in [
            (
                "D=/tmp/n; sed -n 1,9p $D/a.md",
                Some("sed -n 1,9p /tmp/n/a.md"),
            ),
            (
                "D=/tmp/n\ncat \"$D/a b\" ${D}x",
                Some("cat \"/tmp/n/a b\" /tmp/nx"),
            ),
            (
                "D='/tmp/n' && E=\"sub\" && cat $D/$E/a",
                Some("cat /tmp/n/sub/a"),
            ),
            (
                "cat a; D=b; cat $D; D=c; cat $D",
                Some("cat a; cat b; cat c"),
            ),
            // A prefix assignment sets nothing the line can use.
            ("D=/x; D=/y cat $D/f", Some("D=/y cat /x/f")),
            ("D=/y cat $D/f", None),
            // Single quotes and a backslash keep the dollar.
            ("D=/x; cat '$D' \\$D \"it's\" '$D'", None),
            ("D=/x; cat \"it's $D\"", Some("cat \"it's /x\"")),
            // A value the shells may change, or a name zsh reads further.
            ("D=~/x; cat $D/f", None),
            ("D=$(pwd); cat $D/f", None),
            ("D='a b'; cat $D", None),
            ("D=a*; cat $D", None),
            ("D=/x; cat $D[1] $D:h", None),
            ("D=/x; cat $E/f ${D:-y}", None),
            // An assignment the shell may skip, or whose status decides more.
            ("cat a && D=/x; cat $D/f", None),
            ("D=/x || cat $D/f", None),
            ("D=/x | cat $D/f", None),
            ("D=/x & cat $D/f", None),
            ("cat a; D=/x", None),
        ] {
            assert_eq!(super::expand(line).as_deref(), expected, "{line}");
        }
        let expanded = super::expand("D=sub; cat $D/a; sed -n 1,2p $D/b").unwrap();
        assert_eq!(super::list(&expanded).unwrap().len(), 2);
    }

    #[test]
    fn an_awk_line_range_is_a_file_read() {
        for (script, lines) in [
            ("NR>=10 && NR<=20", Some((10, 20))),
            ("NR>=3&&NR<=3 {print}", Some((3, 3))),
            ("NR==5,NR==9", Some((5, 9))),
            ("NR >= 1 && NR <= 400 { print $0 }", Some((1, 400))),
            ("NR>=20 && NR<=10", None),
            ("NR>=0 && NR<=10", None),
            ("NR>=10", None),
            ("NR>=10 && NR<=20 {print $1}", None),
            ("/x/ && NR<=20", None),
            ("NR>=10 && NR<=20 || /x/", None),
        ] {
            let range = super::awk(script).map(|filter| match filter {
                super::Filter::Sed(ranges) => ranges[0],
                _ => unreachable!(),
            });
            assert_eq!(range, lines, "{script}");
        }
        assert!(super::single("awk 'NR>=10 && NR<=20' a.rs", false).is_some());
        assert!(super::single("awk '{print $1}' a.rs", false).is_none());
        assert!(super::single("awk -F: 'NR>=10 && NR<=20' a.rs", false).is_none());
        assert!(super::list("cd src && awk 'NR==5,NR==9' a.rs; cat b").is_some());
    }

    #[test]
    fn a_glob_names_the_files_the_shell_would() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path();
        std::fs::create_dir(dir.join("src")).unwrap();
        for name in [
            "src/b.rs",
            "src/a.rs",
            "src/c d.rs",
            "src/.hidden.rs",
            "notes.md",
        ] {
            std::fs::write(dir.join(name), "x\n").unwrap();
        }
        for (line, expected) in [
            (
                "cat a; grep -n x src/*.rs",
                Some("cat a; grep -n x src/a.rs src/b.rs 'src/c d.rs'"),
            ),
            ("cd src && wc -l ?.rs", Some("cd src && wc -l a.rs b.rs")),
            ("cat src/.h*", Some("cat src/.hidden.rs")),
            (
                "cat *.md src/*.rs",
                Some("cat notes.md src/a.rs src/b.rs 'src/c d.rs'"),
            ),
            // A quoted or escaped wildcard is the program's own.
            ("grep -n 'a*' notes.md", None),
            ("grep -n a\\* notes.md", None),
            // No file, a class, a wildcard folder, and a word that starts a command.
            ("cat src/*.zz", None),
            ("cat src/[ab].rs", None),
            ("cat */a.rs", None),
            ("cat **/*.rs", None),
            ("*.sh", None),
            ("cat a; grep -n x src/*.rs src/*.zz", None),
        ] {
            assert_eq!(super::glob(line, Some(dir)).as_deref(), expected, "{line}");
        }
        // A line from a transcript: one file stands for each glob.
        assert_eq!(
            super::glob("cd src && grep -n x *.rs", None).as_deref(),
            Some("cd src && grep -n x _.rs")
        );
    }

    #[test]
    fn only_a_tilde_inside_a_word_is_literal() {
        assert!(super::list("git show HEAD~1:a.rs; sed -n 1,2p b").is_some());
        assert!(super::single("timeout 5 git show main~2:a.rs", false).is_some());
        assert!(super::list("rg -nS foo src; cat a").is_some());
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
