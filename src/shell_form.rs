//! The shell words a command hook keeps around a command it wraps.
//!
//! Leading `NAME=value` assignments stay in front of `justless run`, so the variables
//! reach both Justless and the command, while the command words alone choose the
//! output profile. A final `2>&1` stays at the end, so it merges the stdout and
//! stderr Justless prints, and the hook adds `--merge`, so the command writes both
//! streams in order to one archive, the way the original `2>&1` merged them.
//! `RUST_BACKTRACE=1 cargo test 2>&1` becomes
//! `RUST_BACKTRACE=1 justless run --hook --merge -- cargo test 2>&1`.
use std::borrow::Cow;
use std::ops::Range;

/// The redirection that merges stderr into stdout.
const MERGE: &str = "2>&1";

/// One simple command, split around the words a hook wraps.
pub struct Form {
    /// Leading `NAME=value` assignments, as written.
    assignments: Vec<Range<usize>>,
    /// The command words, from the program to the last argument, without a final
    /// `2>&1`.
    pub command: Range<usize>,
}

/// Whether `bytes[at]` is the `>` of a `2>&1` that ends its command: it follows a
/// blank, and only blanks lie between it and the end of the line, a `;`, a
/// newline, or a blank-separated `&&` or `||`. A command list may contain it there, and `parse`
/// keeps it out of the command words. Anywhere else, such as before `cd` in
/// `2>&1 cd /tmp`, the list passes through.
pub fn merge_at(bytes: &[u8], at: usize) -> bool {
    let blank = |byte: &u8| matches!(byte, b' ' | b'\t');
    if at < 2 || !blank(&bytes[at - 2]) || !bytes[at - 1..].starts_with(MERGE.as_bytes()) {
        return false;
    }
    let rest = &bytes[at + 3..];
    let next = &rest[rest
        .iter()
        .position(|byte| !blank(byte))
        .unwrap_or(rest.len())..];
    let single_pipe = |slice: &[u8]| {
        slice.first() == Some(&b'|') && !slice.starts_with(b"||") && !slice.starts_with(b"|&")
    };
    rest.first()
        .is_none_or(|byte| blank(byte) || matches!(byte, b';' | b'\n') || single_pipe(rest))
        && (next.is_empty()
            || matches!(next[0], b';' | b'\n')
            || next.starts_with(b"&&")
            || next.starts_with(b"||")
            || single_pipe(next))
}

/// Whether the text that follows a wrapped command's words starts with the final
/// `2>&1` that `parse` left out of them.
pub fn merges(rest: &str) -> bool {
    rest.trim_start_matches([' ', '\t'])
        .strip_prefix(MERGE)
        .is_some_and(|after| {
            after.is_empty()
                || after.starts_with([' ', '\t', ';', '\n'])
                || (after.starts_with('|') && !after.starts_with("||") && !after.starts_with("|&"))
        })
}

/// The blanks that separate shell words.
const BLANKS: [char; 2] = [' ', '\t'];

/// Nesting frame tracked while splitting a shell stage into pipe stages or words.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    SingleQuote,
    DoubleQuote,
    CmdSubst,
    Param,
    Paren,
    Brace,
    DoubleBracket,
}

/// Whether `name` is a valid POSIX shell variable identifier (`[A-Za-z_][A-Za-z0-9_]*`).
fn is_ident(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Whether `bytes` contains no pipe, quote, escape, or nesting delimiter, so the
/// segment is guaranteed to be a single balanced stage.
#[inline]
fn is_plain_single_stage(bytes: &[u8]) -> bool {
    !bytes.iter().any(|&b| {
        matches!(
            b,
            b'|' | b'\'' | b'"' | b'\\' | b'(' | b')' | b'{' | b'}' | b'[' | b']'
        )
    })
}

/// Split a shell command segment on top-level unquoted `|` and `|&` (never `||`),
/// tracking single/double quotes, backslash escapes outside single quotes, and
/// `$(...)`, `${...}`, `(...)`, `{...}`, `[[...]]` nesting. Returns `None` on
/// unbalanced quotes/brackets or empty pipe stages.
pub(crate) fn pipe_stages(segment: &str) -> Option<Vec<Range<usize>>> {
    let bytes = segment.as_bytes();
    if is_plain_single_stage(bytes) {
        let lead = segment.len() - segment.trim_start_matches(BLANKS).len();
        let trimmed = segment.trim_matches(BLANKS);
        return (!trimmed.is_empty())
            .then(|| std::iter::once(lead..lead + trimmed.len()).collect());
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut stages = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut push = |from: usize, to: usize| -> Option<()> {
        let text = &segment[from..to];
        let lead = text.len() - text.trim_start_matches(BLANKS).len();
        let trimmed = text.trim_matches(BLANKS);
        (!trimmed.is_empty()).then(|| stages.push(from + lead..from + lead + trimmed.len()))
    };
    while i < bytes.len() {
        let c = bytes[i];
        match stack.last().copied() {
            Some(Frame::SingleQuote) => {
                if c == b'\'' {
                    stack.pop();
                }
                i += 1;
            }
            Some(Frame::DoubleQuote) => {
                if c == b'\\' {
                    let next = segment[i + 1..].chars().next()?;
                    i += 1 + next.len_utf8();
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'(') {
                    stack.push(Frame::CmdSubst);
                    i += 2;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    stack.push(Frame::Param);
                    i += 2;
                } else {
                    if c == b'"' {
                        stack.pop();
                    }
                    i += 1;
                }
            }
            Some(Frame::DoubleBracket) => {
                if c == b'\\' {
                    let next = segment[i + 1..].chars().next()?;
                    i += 1 + next.len_utf8();
                } else if c == b'\'' {
                    stack.push(Frame::SingleQuote);
                    i += 1;
                } else if c == b'"' {
                    stack.push(Frame::DoubleQuote);
                    i += 1;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'(') {
                    stack.push(Frame::CmdSubst);
                    i += 2;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    stack.push(Frame::Param);
                    i += 2;
                } else if c == b']'
                    && bytes.get(i + 1) == Some(&b']')
                    && bytes
                        .get(i + 2)
                        .is_none_or(|b| matches!(b, b' ' | b'\t' | b';' | b'&' | b'|' | b')'))
                {
                    stack.pop();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            top => {
                let at_top = top.is_none();
                if c == b'\\' {
                    let next = segment[i + 1..].chars().next()?;
                    i += 1 + next.len_utf8();
                } else if c == b'\'' {
                    stack.push(Frame::SingleQuote);
                    i += 1;
                } else if c == b'"' {
                    stack.push(Frame::DoubleQuote);
                    i += 1;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'(') {
                    stack.push(Frame::CmdSubst);
                    i += 2;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    stack.push(Frame::Param);
                    i += 2;
                } else if c == b'(' {
                    stack.push(Frame::Paren);
                    i += 1;
                } else if c == b')' {
                    if !matches!(stack.pop(), Some(Frame::CmdSubst | Frame::Paren)) {
                        return None;
                    }
                    i += 1;
                } else if c == b'{' {
                    stack.push(Frame::Brace);
                    i += 1;
                } else if c == b'}' {
                    if !matches!(stack.pop(), Some(Frame::Param | Frame::Brace)) {
                        return None;
                    }
                    i += 1;
                } else if c == b'['
                    && bytes.get(i + 1) == Some(&b'[')
                    && (i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b';' | b'&' | b'|' | b'('))
                    && bytes.get(i + 2).is_some_and(|b| matches!(b, b' ' | b'\t'))
                {
                    stack.push(Frame::DoubleBracket);
                    i += 2;
                } else if at_top && c == b'|' {
                    if bytes.get(i + 1) == Some(&b'|') {
                        i += 2;
                    } else {
                        push(start, i)?;
                        i += if bytes.get(i + 1) == Some(&b'&') {
                            2
                        } else {
                            1
                        };
                        start = i;
                    }
                } else {
                    i += 1;
                }
            }
        }
    }
    if !stack.is_empty() {
        return None;
    }
    push(start, segment.len())?;
    Some(stages)
}

/// Whether `stage` is a parenthesized subshell `(...)` (never `((...))` arithmetic)
/// followed only by optional whitespace and non-mutating redirections. A subshell
/// runs in a child process and cannot change the outer shell's directory or variables.
pub(crate) fn is_subshell(stage: &str) -> bool {
    let trimmed = stage.trim_matches(BLANKS);
    let bytes = trimmed.as_bytes();
    if bytes.first() != Some(&b'(') || bytes.get(1) == Some(&b'(') {
        return false;
    }
    let mut stack: Vec<Frame> = vec![Frame::Paren];
    let mut i = 1usize;
    let mut close = None;
    while i < bytes.len() {
        let c = bytes[i];
        match stack.last().copied() {
            Some(Frame::SingleQuote) => {
                if c == b'\'' {
                    stack.pop();
                }
                i += 1;
            }
            Some(Frame::DoubleQuote) => {
                if c == b'\\' {
                    let Some(next) = trimmed[i + 1..].chars().next() else {
                        return false;
                    };
                    i += 1 + next.len_utf8();
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'(') {
                    stack.push(Frame::CmdSubst);
                    i += 2;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    stack.push(Frame::Param);
                    i += 2;
                } else {
                    if c == b'"' {
                        stack.pop();
                    }
                    i += 1;
                }
            }
            _ => {
                if c == b'\\' {
                    let Some(next) = trimmed[i + 1..].chars().next() else {
                        return false;
                    };
                    i += 1 + next.len_utf8();
                } else if c == b'\'' {
                    stack.push(Frame::SingleQuote);
                    i += 1;
                } else if c == b'"' {
                    stack.push(Frame::DoubleQuote);
                    i += 1;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'(') {
                    stack.push(Frame::CmdSubst);
                    i += 2;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    stack.push(Frame::Param);
                    i += 2;
                } else if c == b'(' {
                    stack.push(Frame::Paren);
                    i += 1;
                } else if c == b')' {
                    if !matches!(stack.pop(), Some(Frame::CmdSubst | Frame::Paren)) {
                        return false;
                    }
                    if stack.is_empty() {
                        close = Some(i);
                        break;
                    }
                    i += 1;
                } else if c == b'{' {
                    stack.push(Frame::Brace);
                    i += 1;
                } else if c == b'}' {
                    if !matches!(stack.pop(), Some(Frame::Param | Frame::Brace)) {
                        return false;
                    }
                    i += 1;
                } else {
                    i += 1;
                }
            }
        }
    }
    let Some(close) = close else {
        return false;
    };
    let after = trimmed[close + 1..].trim_matches(BLANKS);
    if after.is_empty() {
        return true;
    }
    if mutates_via_expansion(after) {
        return false;
    }
    words(after).is_some_and(|w| strip_stage_redirections(after, &w).is_empty())
}

/// Whether `stage` contains shell expansion syntax that can mutate shell variables
/// as a side effect: arithmetic evaluation/expansion (`$((...))` or `((...))`) or
/// an assignment-capable `${...=...}` parameter expansion outside single quotes.
pub(crate) fn mutates_via_expansion(stage: &str) -> bool {
    let bytes = stage.as_bytes();
    if !bytes.iter().any(|&b| matches!(b, b'$' | b'(' | b'\\')) {
        return false;
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        match stack.last().copied() {
            Some(Frame::SingleQuote) => {
                if c == b'\'' {
                    stack.pop();
                }
                i += 1;
            }
            Some(Frame::DoubleQuote) => {
                if c == b'\\' {
                    let Some(next) = stage[i + 1..].chars().next() else {
                        return true;
                    };
                    i += 1 + next.len_utf8();
                } else if c == b'$' && bytes.get(i + 1..i + 3) == Some(b"((") {
                    return true;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'(') {
                    stack.push(Frame::CmdSubst);
                    i += 2;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    stack.push(Frame::Param);
                    i += 2;
                } else {
                    if c == b'"' {
                        stack.pop();
                    }
                    i += 1;
                }
            }
            Some(Frame::Param) => {
                if c == b'=' {
                    return true;
                } else if c == b'\\' {
                    let Some(next) = stage[i + 1..].chars().next() else {
                        return true;
                    };
                    i += 1 + next.len_utf8();
                } else if c == b'\'' {
                    stack.push(Frame::SingleQuote);
                    i += 1;
                } else if c == b'"' {
                    stack.push(Frame::DoubleQuote);
                    i += 1;
                } else if c == b'$' && bytes.get(i + 1..i + 3) == Some(b"((") {
                    return true;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    stack.push(Frame::Param);
                    i += 2;
                } else if c == b'}' {
                    stack.pop();
                    i += 1;
                } else {
                    i += 1;
                }
            }
            _ => {
                if c == b'\\' {
                    let Some(next) = stage[i + 1..].chars().next() else {
                        return true;
                    };
                    i += 1 + next.len_utf8();
                } else if c == b'\'' {
                    stack.push(Frame::SingleQuote);
                    i += 1;
                } else if c == b'"' {
                    stack.push(Frame::DoubleQuote);
                    i += 1;
                } else if (c == b'$' && bytes.get(i + 1..i + 3) == Some(b"(("))
                    || (c == b'(' && bytes.get(i + 1) == Some(&b'('))
                {
                    return true;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'(') {
                    stack.push(Frame::CmdSubst);
                    i += 2;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    stack.push(Frame::Param);
                    i += 2;
                } else if c == b'(' {
                    stack.push(Frame::Paren);
                    i += 1;
                } else if c == b')' {
                    let _ = stack.pop();
                    i += 1;
                } else {
                    i += 1;
                }
            }
        }
    }
    false
}

/// First unquoted `<` or `>` offset in a raw word (outside quotes and nested
/// expansions/subshells), if any.
fn first_unquoted_redirect(raw: &str) -> Option<usize> {
    let bytes = raw.as_bytes();
    if !bytes.iter().any(|&b| b == b'<' || b == b'>') {
        return None;
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        match stack.last().copied() {
            Some(Frame::SingleQuote) => {
                if c == b'\'' {
                    stack.pop();
                }
                i += 1;
            }
            Some(Frame::DoubleQuote) => {
                if c == b'\\' {
                    let next = raw[i + 1..].chars().next()?;
                    i += 1 + next.len_utf8();
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'(') {
                    stack.push(Frame::CmdSubst);
                    i += 2;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    stack.push(Frame::Param);
                    i += 2;
                } else {
                    if c == b'"' {
                        stack.pop();
                    }
                    i += 1;
                }
            }
            None => {
                if c == b'\\' {
                    let next = raw[i + 1..].chars().next()?;
                    i += 1 + next.len_utf8();
                } else if c == b'\'' {
                    stack.push(Frame::SingleQuote);
                    i += 1;
                } else if c == b'"' {
                    stack.push(Frame::DoubleQuote);
                    i += 1;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'(') {
                    stack.push(Frame::CmdSubst);
                    i += 2;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    stack.push(Frame::Param);
                    i += 2;
                } else if c == b'(' {
                    stack.push(Frame::Paren);
                    i += 1;
                } else if c == b'{' {
                    stack.push(Frame::Brace);
                    i += 1;
                } else if matches!(c, b'<' | b'>') {
                    return Some(i);
                } else {
                    i += 1;
                }
            }
            _ => {
                if c == b'\\' {
                    let next = raw[i + 1..].chars().next()?;
                    i += 1 + next.len_utf8();
                } else if c == b'\'' {
                    stack.push(Frame::SingleQuote);
                    i += 1;
                } else if c == b'"' {
                    stack.push(Frame::DoubleQuote);
                    i += 1;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'(') {
                    stack.push(Frame::CmdSubst);
                    i += 2;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    stack.push(Frame::Param);
                    i += 2;
                } else if c == b'(' {
                    stack.push(Frame::Paren);
                    i += 1;
                } else if c == b')' {
                    let _ = stack.pop();
                    i += 1;
                } else if c == b'{' {
                    stack.push(Frame::Brace);
                    i += 1;
                } else if c == b'}' {
                    let _ = stack.pop();
                    i += 1;
                } else {
                    i += 1;
                }
            }
        }
    }
    None
}

/// Whether the redirection operator starting at `redir` (`>` or `<`) has no
/// attached operand and therefore consumes the next shell word as its target.
fn redirect_takes_next(redir: &str) -> bool {
    let rest = if redir.starts_with(">>")
        || redir.starts_with("<<")
        || redir.starts_with("<>")
        || redir.starts_with(">&")
        || redir.starts_with("<&")
        || redir.starts_with(">|")
    {
        &redir[2..]
    } else {
        &redir[1..]
    };
    rest.is_empty()
}

/// Drop redirection tokens (and separate filename targets) from a stage's raw
/// words, splitting any glued suffix redirection (such as `PATH=/x>out`) so the
/// leading word stays visible to safety checks.
pub(crate) fn strip_stage_redirections(
    stage: &str,
    raw_words: &[Range<usize>],
) -> Vec<Range<usize>> {
    let mut kept = Vec::with_capacity(raw_words.len());
    let mut skip_next = false;
    for range in raw_words {
        if skip_next {
            skip_next = false;
            continue;
        }
        let raw = &stage[range.clone()];
        let Some(pos) = first_unquoted_redirect(raw) else {
            kept.push(range.clone());
            continue;
        };
        let prefix = &raw[..pos];
        let redir = &raw[pos..];
        let pure_redir = prefix.is_empty()
            || prefix.bytes().all(|b| b.is_ascii_digit())
            || (prefix == "&" && redir.starts_with('>'));
        if !pure_redir {
            kept.push(range.start..range.start + pos);
        }
        skip_next = redirect_takes_next(redir);
    }
    kept
}

/// Whether a raw shell word may dynamically expand (variable/command substitution,
/// grouping, brace/glob/tilde expansion) when interpreted as a program or variable
/// name. Exact `[` and `[[` are the literal test builtins and are not dynamic.
pub(crate) fn raw_word_is_dynamic(raw: &str) -> bool {
    if raw.is_empty() {
        return true;
    }
    let literal_test = matches!(raw, "[" | "[[");
    let bytes = raw.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(b'\'') => {
                if c == b'\'' {
                    quote = None;
                }
                i += 1;
            }
            Some(b'"') => {
                if c == b'\\' {
                    let Some(next) = raw[i + 1..].chars().next() else {
                        return true;
                    };
                    i += 1 + next.len_utf8();
                } else if matches!(c, b'$' | b'`') {
                    return true;
                } else {
                    if c == b'"' {
                        quote = None;
                    }
                    i += 1;
                }
            }
            _ => {
                if c == b'\\' {
                    let Some(next) = raw[i + 1..].chars().next() else {
                        return true;
                    };
                    i += 1 + next.len_utf8();
                } else if matches!(c, b'\'' | b'"') {
                    quote = Some(c);
                    i += 1;
                } else if matches!(c, b'$' | b'`' | b'(' | b'{' | b'*' | b'?')
                    || (c == b'[' && !literal_test)
                    || (c == b'~' && i == 0)
                {
                    return true;
                } else {
                    i += 1;
                }
            }
        }
    }
    quote.is_some()
}

/// Extract the unquoted identifier name of a leading `NAME=value` or `NAME+=value`
/// assignment from a raw shell word.
fn raw_assignment_name(raw: &str) -> Option<&str> {
    let (head, _) = raw.split_once('=')?;
    let name = head.strip_suffix('+').unwrap_or(head);
    is_ident(name).then_some(name)
}

/// First unquoted `=` offset inside `raw`, if any.
fn first_unquoted_eq(raw: &str) -> Option<usize> {
    let bytes = raw.as_bytes();
    let mut quote: Option<u8> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(b'\'') => {
                if c == b'\'' {
                    quote = None;
                }
                i += 1;
            }
            Some(b'"') => {
                if c == b'\\' {
                    let next = raw[i + 1..].chars().next()?;
                    i += 1 + next.len_utf8();
                } else {
                    if c == b'"' {
                        quote = None;
                    }
                    i += 1;
                }
            }
            _ => {
                if c == b'\\' {
                    let next = raw[i + 1..].chars().next()?;
                    i += 1 + next.len_utf8();
                } else if matches!(c, b'\'' | b'"') {
                    quote = Some(c);
                    i += 1;
                } else if c == b'=' {
                    return Some(i);
                } else {
                    i += 1;
                }
            }
        }
    }
    None
}

/// Unquote a single raw shell word, borrowing `raw` without allocation when it
/// consists solely of plain unquoted ASCII characters that `shlex::split` keeps verbatim.
fn unquote_word(raw: &str) -> Option<Cow<'_, str>> {
    if !raw.is_empty()
        && raw
            .bytes()
            .all(|b| matches!(b, b'!'..=b'~') && !matches!(b, b'\'' | b'"' | b'\\' | b'#'))
    {
        return Some(Cow::Borrowed(raw));
    }
    match shlex::split(raw).as_deref() {
        Some([word]) => Some(Cow::Owned(word.clone())),
        _ => None,
    }
}

/// Whether an argument to `export`, `declare`, `typeset`, `readonly`, `local` or
/// `unset` names (or may dynamically expand to) a variable that steers Justless.
fn declares_steering_var(raw: &str) -> bool {
    let raw_name = match first_unquoted_eq(raw) {
        Some(eq) => {
            let head = &raw[..eq];
            head.strip_suffix('+').unwrap_or(head)
        }
        None => raw,
    };
    if raw_word_is_dynamic(raw_name) {
        return true;
    }
    let Some(unquoted) = unquote_word(raw_name) else {
        return true;
    };
    let name = unquoted
        .split_once('=')
        .map_or(unquoted.as_ref(), |(n, _)| n);
    let name = name.strip_suffix('+').unwrap_or(name);
    steers(name)
}

/// Whether a command of a list may change, for the commands after it, a variable
/// that decides which Justless runs or how. That is a command made only of
/// assignments, such as `PATH=/x` or `PATH=/x 2>&1`, an assignment before a POSIX
/// special builtin such as `:`, which the shell keeps, `export`, `declare`,
/// `typeset`, `readonly`, `local` or `unset` naming (or dynamically naming) such
/// a variable, also behind `builtin`, `command` or control keywords, or a dynamic
/// command word, brace group, `eval`/`source`/`.`/`exec`/`alias`/`function` or
/// variable-mutating expansion (`${...=...}`, `$((...))`, `((...))`). A hook then
/// passes the whole list through, as for a change of directory.
pub fn steers_justless(segment: &str) -> bool {
    if is_plain_single_stage(segment.as_bytes()) {
        let trimmed = segment.trim_matches(BLANKS);
        return trimmed.is_empty() || steers_justless_stage(trimmed);
    }
    let Some(stages) = pipe_stages(segment) else {
        return true;
    };
    stages
        .into_iter()
        .any(|range| steers_justless_stage(&segment[range]))
}

fn steers_justless_stage(stage: &str) -> bool {
    if is_subshell(stage) {
        return false;
    }
    if mutates_via_expansion(stage) {
        return true;
    }
    let Some(raw_words) = words(stage) else {
        return true;
    };
    let raw_words: Cow<'_, [Range<usize>]> =
        if stage.as_bytes().iter().any(|&b| b == b'<' || b == b'>') {
            Cow::Owned(strip_stage_redirections(stage, &raw_words))
        } else {
            Cow::Borrowed(&raw_words)
        };
    // Skip leading shell control-flow keywords before assignments/program.
    let start = raw_words
        .iter()
        .position(|range| {
            let raw = &stage[range.clone()];
            !matches!(
                raw,
                "!" | "if" | "then" | "else" | "elif" | "while" | "until" | "do" | "time"
            )
        })
        .unwrap_or(raw_words.len());
    let raw_words = &raw_words[start..];
    let program_idx = raw_words
        .iter()
        .position(|range| raw_assignment_name(&stage[range.clone()]).is_none());
    let assigned = raw_words[..program_idx.unwrap_or(raw_words.len())]
        .iter()
        .filter_map(|range| raw_assignment_name(&stage[range.clone()]))
        .any(steers);
    let Some(program_idx) = program_idx else {
        return assigned;
    };
    let mut idx = program_idx;
    let raw_program = loop {
        let Some(range) = raw_words.get(idx) else {
            return assigned;
        };
        idx += 1;
        let word = &stage[range.clone()];
        if !matches!(word, "builtin" | "command" | "noglob" | "nocorrect") {
            break word;
        }
    };
    if raw_word_is_dynamic(raw_program) {
        return true;
    }
    let Some(program) = unquote_word(raw_program) else {
        return true;
    };
    let rest = &raw_words[idx..];
    if program.ends_with("()") || rest.first().is_some_and(|r| &stage[r.clone()] == "()") {
        return true;
    }
    if matches!(
        program.as_ref(),
        "alias" | "unalias" | "function" | "exec" | "eval" | "source" | "."
    ) {
        return true;
    }
    if assigned && SPECIAL.contains(&program.as_ref()) {
        return true;
    }
    match program.as_ref() {
        "export" | "declare" | "typeset" | "readonly" | "local" | "unset" => rest
            .iter()
            .any(|r| declares_steering_var(&stage[r.clone()])),
        "for" => rest
            .first()
            .is_none_or(|r| declares_steering_var(&stage[r.clone()])),
        "read" | "mapfile" | "readarray" | "getopts" | "vared" => rest
            .iter()
            .map(|r| &stage[r.clone()])
            .filter(|arg| !arg.starts_with('-'))
            .any(declares_steering_var),
        "printf" => {
            let mut iter = rest.iter().map(|r| &stage[r.clone()]);
            while let Some(arg) = iter.next() {
                if arg == "-v" {
                    if iter.next().is_none_or(declares_steering_var) {
                        return true;
                    }
                } else if let Some(var) = arg.strip_prefix("-v")
                    && !var.is_empty()
                    && declares_steering_var(var)
                {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

/// POSIX special builtins, after which a shell keeps the assignments that precede
/// them.
const SPECIAL: &[&str] = &[
    ":", ".", "break", "continue", "eval", "exec", "exit", "export", "readonly", "return", "set",
    "shift", "times", "trap", "unset",
];

/// Whether a variable decides which Justless the shell starts or how it runs:
/// `PATH`, and `path`, which zsh ties to it, `LD_*` and `DYLD_*`, which load code
/// into it, and `JUSTLESS_*`, which configure it. An empty name stands for an
/// assignment the shell would not take literally.
pub(crate) fn steers(name: &str) -> bool {
    name.is_empty()
        || name == "PATH"
        || name == "path"
        || name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || name.starts_with("JUSTLESS_")
}

/// Whether `name=value` is a display/diagnostic assignment with a plain value,
/// safe for a client that auto-approves rewritten commands (outside `make`).
pub(crate) fn display_assignment(name: &str, value: &str) -> bool {
    (DISPLAY.contains(&name) || name.starts_with("LC_"))
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.,:=@%+-".contains(c))
}

/// Split a trimmed simple command into its assignments and command words, leaving
/// out a final `2>&1`. Returns `None` for unbalanced quotes or when nothing but
/// assignments remains. A `2>&1` anywhere else stays in the command words, where
/// the hooks' literal-command check rejects it.
pub fn parse(segment: &str) -> Option<Form> {
    let mut words = words(segment)?;
    if words.len() > 1
        && words
            .last()
            .is_some_and(|word| &segment[word.clone()] == MERGE)
    {
        words.pop();
    }
    let program = words
        .iter()
        .position(|word| !assignment(&segment[word.clone()]))?;
    Some(Form {
        assignments: words[..program].to_vec(),
        command: words[program].start..words.last()?.end,
    })
}

impl Form {
    /// Whether Justless itself runs unchanged under these assignments. A command
    /// with a variable that `steers` Justless is left whole.
    pub fn keeps_justless(&self, segment: &str) -> bool {
        self.variables(segment).all(|(name, _)| !steers(&name))
    }

    /// Whether a client that may run the rewritten command without asking can take
    /// these assignments: display and diagnostic settings with plain values only.
    /// Variables such as `RUSTFLAGS`, `NODE_OPTIONS`, `PYTEST_ADDOPTS` or
    /// `GIT_EXTERNAL_DIFF` can run other code or write elsewhere, so they are left
    /// to the client's own approval, as is a value with a path. `make` reads every
    /// environment variable as a make variable, which a recipe may use, so it takes
    /// no assignment, as it takes no `NAME=value` argument.
    pub fn approvable(&self, segment: &str) -> bool {
        if self.assignments.is_empty() {
            return true;
        }
        if !self
            .variables(segment)
            .all(|(name, value)| display_assignment(&name, &value))
        {
            return false;
        }
        let make = shlex::split(&segment[self.command.clone()]).is_none_or(|words| {
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            let Some(unwrapped) = crate::wrappers::hook_effective_slice(&words) else {
                return true;
            };
            crate::filters::unwrap_tool(unwrapped)
                .first()
                .is_some_and(|program| {
                    let name = std::path::Path::new(program).file_name();
                    name.is_some_and(|name| name == "make" || name == "gmake")
                })
        });
        !make
    }

    /// Whether leading assignments are safe for a mixed, non-approving client
    /// (Claude Code, Gemini CLI, Antigravity Tier 2 `ask`): every assignment is
    /// a non-steering `NAME=value` whose value consists only of literal text,
    /// single/double quotes and pure shell variable references (`$var` or `${var}`),
    /// with no command substitution (`$(...)` or backticks), arithmetic expansion,
    /// side-effect parameter expansion (`${var:=...}`), backslash escapes outside
    /// single quotes, tilde expansion or shell metacharacters.
    pub fn safe_dynamic_assignments(&self, segment: &str) -> bool {
        self.assignments
            .iter()
            .all(|range| safe_dynamic_assignment(&segment[range.clone()]))
    }

    /// Each assignment's name and its value after quote removal. An assignment the
    /// shell would not take literally yields an empty name, which no rule accepts.
    fn variables<'a>(
        &'a self,
        segment: &'a str,
    ) -> impl Iterator<Item = (Cow<'a, str>, Cow<'a, str>)> + 'a {
        self.assignments.iter().map(|range| {
            let raw = &segment[range.clone()];
            match unquote_word(raw) {
                Some(Cow::Borrowed(word)) => word
                    .split_once('=')
                    .map(|(name, value)| (Cow::Borrowed(name), Cow::Borrowed(value)))
                    .unwrap_or_default(),
                Some(Cow::Owned(word)) => word
                    .split_once('=')
                    .map(|(name, value)| {
                        (Cow::Owned(name.to_owned()), Cow::Owned(value.to_owned()))
                    })
                    .unwrap_or_default(),
                None => Default::default(),
            }
        })
    }
}

/// Whether one `NAME=value` assignment has a non-steering identifier and a value
/// built only from literal text, quotes, and pure `$var` / `${var}` expansions.
fn safe_dynamic_assignment(raw: &str) -> bool {
    let Some((name, value)) = raw.split_once('=') else {
        return false;
    };
    if !is_ident(name) || steers(name) {
        return false;
    }
    let mut quote: Option<char> = None;
    let mut previous: Option<char> = None;
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character.is_control() && character != '\t' {
            return false;
        }
        match quote {
            Some('\'') => {
                if character == '\\' && chars.peek() == Some(&'\'') {
                    return false;
                }
                if character == '\'' {
                    quote = None;
                }
            }
            Some('"') => match character {
                '\\' | '`' => return false,
                '"' => quote = None,
                '$' if !consume_simple_var(&mut chars) => return false,
                '$' => {}
                _ => {}
            },
            None => match character {
                '\\' | '`' => return false,
                '\'' | '"' => quote = Some(character),
                '$' if !consume_simple_var(&mut chars) => return false,
                '$' => {}
                '~' if previous.is_none_or(|p| p == ':') => return false,
                c if "|;&<>*?[]{}#!()".contains(c) => return false,
                _ => {}
            },
            Some(_) => unreachable!(),
        }
        previous = Some(character);
    }
    quote.is_none()
}

/// Consume a pure shell variable reference immediately after `$`: either `IDENT`
/// or `{IDENT}`, where `IDENT` is `[A-Za-z_][A-Za-z0-9_]*`. Rejects command
/// substitution `$(...)`, arithmetic `$((...))`, parameter operators (`${a:=b}`),
/// positional/special parameters (`$1`, `$?`), and bare `$`.
fn consume_simple_var(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    let braced = chars.peek() == Some(&'{');
    if braced {
        chars.next();
    }
    let Some(&first) = chars.peek() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.next();
    while chars
        .peek()
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
    {
        chars.next();
    }
    if braced {
        chars.next() == Some('}')
    } else {
        true
    }
}

/// Variables that only change how a tool reports, colors, localizes or paces its
/// output, so setting them cannot make a build or test run other code. `CI` and
/// `NODE_ENV` are left out: builds, test setups and Makefiles branch on them, for
/// example to choose a production configuration or skip a step.
const DISPLAY: &[&str] = &[
    "NO_COLOR",
    "FORCE_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "TERM",
    "COLUMNS",
    "LINES",
    "LANG",
    "LANGUAGE",
    "RUST_BACKTRACE",
    "RUST_LIB_BACKTRACE",
    "RUST_LOG",
    "RUST_LOG_STYLE",
    "RUST_TEST_THREADS",
    "RUST_TEST_NOCAPTURE",
    "CARGO_TERM_COLOR",
    "CARGO_TERM_VERBOSE",
    "CARGO_TERM_QUIET",
    "CARGO_INCREMENTAL",
    "CARGO_BUILD_JOBS",
    "PYTHONUNBUFFERED",
    "PYTHONDONTWRITEBYTECODE",
    "PYTHONHASHSEED",
    "PYTHONFAULTHANDLER",
    "PYTHONUTF8",
    "PYTHONIOENCODING",
    "GOTRACEBACK",
    "GOMAXPROCS",
];

/// Whether a word assigns a variable: an unquoted shell name followed by `=`.
fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| is_ident(name))
}

/// The byte range of each word, split at blanks outside quotes and nested
/// `$(...)`, `${...}`, `(...)`, `{...}` groups, keeping backslash escapes intact.
pub(crate) fn words(text: &str) -> Option<Vec<Range<usize>>> {
    let bytes = text.as_bytes();
    let mut words = Vec::new();
    let mut start: Option<usize> = None;
    let mut stack: Vec<Frame> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        match stack.last().copied() {
            Some(Frame::SingleQuote) => {
                start.get_or_insert(i);
                if c == b'\'' {
                    stack.pop();
                }
                i += 1;
            }
            Some(Frame::DoubleQuote) => {
                start.get_or_insert(i);
                if c == b'\\' {
                    let next = text[i + 1..].chars().next()?;
                    i += 1 + next.len_utf8();
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'(') {
                    stack.push(Frame::CmdSubst);
                    i += 2;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    stack.push(Frame::Param);
                    i += 2;
                } else {
                    if c == b'"' {
                        stack.pop();
                    }
                    i += 1;
                }
            }
            top => {
                let at_top = top.is_none();
                if at_top && matches!(c, b' ' | b'\t') {
                    if let Some(s) = start.take() {
                        words.push(s..i);
                    }
                    i += 1;
                    continue;
                }
                start.get_or_insert(i);
                if c == b'\\' {
                    let next = text[i + 1..].chars().next()?;
                    i += 1 + next.len_utf8();
                } else if c == b'\'' {
                    stack.push(Frame::SingleQuote);
                    i += 1;
                } else if c == b'"' {
                    stack.push(Frame::DoubleQuote);
                    i += 1;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'(') {
                    stack.push(Frame::CmdSubst);
                    i += 2;
                } else if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
                    stack.push(Frame::Param);
                    i += 2;
                } else if c == b'(' {
                    stack.push(Frame::Paren);
                    i += 1;
                } else if c == b')' {
                    if !matches!(stack.pop(), Some(Frame::CmdSubst | Frame::Paren)) {
                        return None;
                    }
                    i += 1;
                } else if c == b'{' {
                    stack.push(Frame::Brace);
                    i += 1;
                } else if c == b'}' {
                    if !matches!(stack.pop(), Some(Frame::Param | Frame::Brace)) {
                        return None;
                    }
                    i += 1;
                } else {
                    i += 1;
                }
            }
        }
    }
    if !stack.is_empty() {
        return None;
    }
    if let Some(s) = start {
        words.push(s..text.len());
    }
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::{is_subshell, parse, pipe_stages, steers_justless};

    #[test]
    fn safe_dynamic_assignments_accept_pure_vars_and_reject_substitutions() {
        for ok in [
            r#"TMPDIR="$tmp" cargo test --locked"#,
            "TMPDIR=$tmp cargo test",
            r#"TMPDIR="${tmp}/sub" cargo test"#,
            "TMPDIR=${tmp}/sub cargo test",
            "RUST_BACKTRACE=$LEVEL cargo test",
            r#"FOO="$A:$B" BAR='literal $x' cargo test"#,
            "FOO= cargo test",
        ] {
            let form = parse(ok).unwrap_or_else(|| panic!("parse {ok}"));
            assert!(form.keeps_justless(ok), "keeps_justless {ok}");
            assert!(form.safe_dynamic_assignments(ok), "safe_dynamic {ok}");
            assert!(!form.approvable(ok), "must not be approvable: {ok}");
        }

        for rejected in [
            "FOO=$(id) cargo test",
            r#"FOO="$(id)" cargo test"#,
            "FOO=`id` cargo test",
            r#"FOO="`id`" cargo test"#,
            "FOO=$((1+1)) cargo test",
            r#"FOO="${PATH:=/evil}" cargo test"#,
            r#"FOO="${tmp:-/tmp}" cargo test"#,
            "FOO=~/x cargo test",
            "FOO=a:~/x cargo test",
            "FOO=$1 cargo test",
            r#"FOO="$" cargo test"#,
            r#"PATH="$x" cargo test"#,
            r#"LD_PRELOAD="$x" cargo test"#,
            r#"JUSTLESS_DISABLE="$x" cargo test"#,
        ] {
            if let Some(form) = parse(rejected) {
                assert!(
                    !(form.keeps_justless(rejected) && form.safe_dynamic_assignments(rejected)),
                    "must reject {rejected}"
                );
            }
        }
    }

    #[test]
    fn steers_justless_catches_dynamic_and_redirected_mutations() {
        for steering in [
            r#"PATH="$x""#,
            r#"export PATH="$x""#,
            r#"export "$VAR""#,
            r#"unset "$VAR""#,
            "PATH=$(echo /a /b)",
            "2>&1 PATH=/x",
            ">out.txt export PATH=/x",
            "PATH=/x>out.txt",
            "then PATH=/x",
            "! PATH=/x",
            r#"FOO="${PATH:=/evil}""#,
            "echo $((PATH=1))",
            "(( PATH = 1 ))",
            "$(touch x)",
            r#""$CMD" arg"#,
            r#"eval "$x""#,
            "source ./env.sh",
            ". ./env.sh",
            "exec >out.txt",
            "alias justless=true",
            "justless() { true; }",
            "{ PATH=/evil; }",
            "echo x | PATH=/evil",
            "echo x | read PATH",
            "for PATH in /a /b",
            "printf -v PATH %s /evil",
        ] {
            assert!(steers_justless(steering), "must steer: {steering}");
        }

        for safe in [
            "tmp=/var/tmp/justless-test-tmp",
            "tmp=$(mktemp -d)",
            r#"mkdir -p "$tmp""#,
            r#"trap 'rm -rf "$tmp"' EXIT"#,
            r#"TMPDIR="$tmp" cargo test --locked"#,
            r#"echo "branch=$BRANCH""#,
            "git diff > /tmp/out.patch",
            "rg 'foo|bar' src | wc -l",
            r#"export FOO="$BAR""#,
            "PATH=/tmp/bin cargo test",
            r#"[ -f "$tmp" ]"#,
            r#"[[ -f "$a" && -f "$b" ]]"#,
            "(cd /tmp && ls)",
            "(cd /tmp && ls) > /dev/null",
        ] {
            assert!(!steers_justless(safe), "must not steer: {safe}");
        }
    }

    #[test]
    fn pipe_stages_and_subshell_handle_quotes_and_nesting() {
        let s = r#"rg 'a|b' src | wc -l"#;
        let ranges = pipe_stages(s).unwrap();
        let parts: Vec<&str> = ranges.into_iter().map(|r| &s[r]).collect();
        assert_eq!(parts, vec!["rg 'a|b' src", "wc -l"]);

        assert!(is_subshell("(cd /tmp && ls)"));
        assert!(is_subshell("(cd /tmp && ls) 2>&1"));
        assert!(!is_subshell("(( x = 1 ))"));
        assert!(!is_subshell("(cd /tmp) extra"));
    }

    #[test]
    fn fast_path_and_quoted_shell_form_preserve_parity() {
        // Plain single-stage commands match quoted forms for `parse`, `approvable`, and `steers_justless`.
        let plain = "RUST_BACKTRACE=1 cargo test";
        let quoted = r#"RUST_BACKTRACE="1" cargo test"#;
        for cmd in [plain, quoted] {
            let form = parse(cmd).unwrap();
            assert!(form.keeps_justless(cmd), "{cmd}");
            assert!(form.approvable(cmd), "{cmd}");
            assert!(!steers_justless(cmd), "{cmd}");
        }
        // Display assignments stay rejected before `make` even behind safe wrappers.
        let wrapped_make = "RUST_BACKTRACE=1 timeout 30 make test";
        assert!(!parse(wrapped_make).unwrap().approvable(wrapped_make));
        // Bare empty/whitespace segments fail closed in both `pipe_stages` and `steers_justless`.
        assert_eq!(pipe_stages("   "), None);
        assert!(steers_justless("   "));
        // Quoted steering variable names are still caught after unquote fast-path checks.
        assert!(steers_justless(r#"export "PATH"=/tmp"#));
        assert!(steers_justless("export 'LD_PRELOAD'=./x.so"));
        assert!(!steers_justless("cargo clippy --all-targets"));
    }
}
