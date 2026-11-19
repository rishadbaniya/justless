//! The generic compaction floor every ordinary preview gets: carriage-return
//! progress frames collapse to their final state, long base64 and hex runs keep
//! their ends and a length, and consecutive repeated lines fold into a count.
//!
//! It changes only what is displayed. Each elision says what was cut, and is used
//! only where it is shorter than the text it replaces. Lines that look like a
//! failure, warning or assertion, and lines inside a patch hunk, are never changed,
//! folded or dropped.
use std::borrow::Cow;

/// A hex run longer than a SHA-512 digest.
const HEX_RUN: usize = 160;
/// A base64 run, long enough that paths and identifiers do not reach it.
const BASE64_RUN: usize = 200;
/// Characters kept at each end of an elided run.
const KEEP: usize = 12;

/// Whether a line must reach the agent unchanged.
fn kept(line: &str) -> bool {
    let trimmed = line.trim_start();
    crate::filters::diagnostic(trimmed)
        || ["Traceback", "thread '", "left:", "right:"]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix))
}

/// One line, without its line break, with progress frames collapsed and long
/// encoded runs elided.
pub fn line(text: &str) -> Cow<'_, str> {
    if kept(text) {
        return Cow::Borrowed(text);
    }
    let mut out = Cow::Borrowed(text);
    let body = text.strip_suffix('\r').unwrap_or(text);
    if body.contains('\r') {
        let frames: Vec<&str> = body.split('\r').collect();
        let last = frames
            .iter()
            .rposition(|frame| !frame.trim().is_empty())
            .unwrap_or(frames.len() - 1);
        let collapsed = format!(
            "{} [{} earlier progress frame(s) cut]",
            frames[last],
            frames.len() - 1
        );
        if collapsed.len() < body.len() {
            out = Cow::Owned(collapsed);
        }
    }
    match elide(&out) {
        Some(elided) => Cow::Owned(elided),
        None => out,
    }
}

fn encoded(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'_' | b'-')
}

/// The text with each long base64 or hex run cut to its ends and a length.
fn elide(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let (mut copied, mut at) = (0, 0);
    while at < bytes.len() {
        if !encoded(bytes[at]) {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && encoded(bytes[at]) {
            at += 1;
        }
        let run = &text[start..at];
        let Some(kind) = kind(run) else {
            continue;
        };
        let cut = run.len() - 2 * KEEP;
        let marker = format!("[… {cut} {kind} characters cut …]");
        if marker.len() >= cut {
            continue;
        }
        out.push_str(&text[copied..start + KEEP]);
        out.push_str(&marker);
        copied = at - KEEP;
    }
    (copied > 0).then(|| {
        out.push_str(&text[copied..]);
        out
    })
}

fn kind(run: &str) -> Option<&'static str> {
    let count = |test: fn(&u8) -> bool| run.bytes().filter(test).count();
    let length = run.len();
    // A hex dump uses most digits; a run of one padding character is no data.
    let distinct = run
        .bytes()
        .fold(0u128, |seen, b| seen | (1 << (b & 0x7f)))
        .count_ones();
    if length >= HEX_RUN && distinct >= 10 && run.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some("hex");
    }
    // Encoded data mixes cases and digits throughout; paths and identifiers do not.
    (length >= BASE64_RUN
        && count(u8::is_ascii_uppercase) * 10 >= length
        && count(u8::is_ascii_lowercase) * 10 >= length
        && count(u8::is_ascii_digit) * 30 >= length
        && count(|b| *b == b'/') * 16 <= length
        && count(|b| *b == b'-' || *b == b'_') * 16 <= length)
        .then_some("base64")
}

/// Whether `words` print a file's content, as `git show REV:PATH` and `git cat-file`
/// do. Such output shown whole stays byte for byte.
pub fn prints_file(words: &[&str]) -> bool {
    let owned = crate::wrappers::effective(words);
    let words: Vec<&str> = match &owned {
        Some(words) => words.iter().map(String::as_str).collect(),
        None => words.to_vec(),
    };
    match words.as_slice() {
        [git, "show", rest @ ..] if git.ends_with("git") => rest
            .iter()
            .any(|arg| !arg.starts_with('-') && arg.contains(':')),
        [git, "cat-file", ..] => git.ends_with("git"),
        _ => false,
    }
}

/// Applies the floor to a stream line by line, folding repeated lines.
#[derive(Default)]
pub struct Floor {
    patch: bool,
    /// The last line shown and how many identical lines followed it.
    last: Option<String>,
    repeats: usize,
}

impl Floor {
    /// The lines to show for one line of the stream, which keeps its line break:
    /// none while it repeats the line before.
    pub fn push(&mut self, text: &str, out: &mut Vec<String>) {
        let line = text.strip_suffix('\n').unwrap_or(text);
        let trimmed = line.trim_end_matches('\r');
        self.patch = trimmed.starts_with("@@ ")
            || self.patch && (trimmed.is_empty() || trimmed.starts_with([' ', '+', '-', '\\']));
        let shown = if self.patch {
            Cow::Borrowed(line)
        } else {
            self::line(line)
        };
        let foldable = !self.patch && !kept(line);
        if foldable && self.last.as_deref() == Some(&shown) {
            self.repeats += 1;
            return;
        }
        self.flush(out);
        let newline = if text.ends_with('\n') { "\n" } else { "" };
        out.push(format!("{shown}{newline}"));
        self.last = foldable.then(|| shown.into_owned());
    }

    /// Show what folding still holds back, as at the end of the stream.
    pub fn flush(&mut self, out: &mut Vec<String>) {
        let repeats = std::mem::take(&mut self.repeats);
        let Some(last) = self.last.take() else {
            return;
        };
        if repeats == 0 {
            return;
        }
        let marker = format!("[last line repeated {repeats} more time(s)]\n");
        if marker.len() < repeats * (last.len() + 1) {
            out.push(marker);
        } else {
            out.extend(std::iter::repeat_n(format!("{last}\n"), repeats));
        }
    }
}

/// The floor applied to a whole text, when it changed anything.
pub fn compact(text: &str) -> Option<String> {
    let mut floor = Floor::default();
    let mut lines = Vec::new();
    for line in text.split_inclusive('\n') {
        floor.push(line, &mut lines);
    }
    floor.flush(&mut lines);
    let out = lines.concat();
    (out != text).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_reads_are_recognized() {
        for (words, expected) in [
            (&["git", "show", "HEAD:data.json"][..], true),
            (&["git", "cat-file", "-p", "HEAD:x"][..], true),
            (&["git", "show", "HEAD"][..], false),
            (&["git", "show", "--format=%h:%s"][..], false),
        ] {
            assert_eq!(prints_file(words), expected, "{words:?}");
        }
    }

    #[test]
    fn floor_cuts_noise_and_says_what_it_cut() {
        let base64: String = (0..300)
            .map(|i| ["Qm", "x9", "aZ", "k+", "P0"][i % 5])
            .collect();
        let hex = "0123456789abcdef".repeat(12);
        let frames: String = (0..=50).map(|i| format!("\r{:>3}%", i * 2)).collect();
        for (input, expected) in [
            (
                format!("Downloading{frames}\n"),
                "100% [51 earlier progress frame(s) cut]\n".to_owned(),
            ),
            // CRLF line ends are not frames.
            ("ok\r\nok 2\r\n".to_owned(), "ok\r\nok 2\r\n".to_owned()),
            (
                format!("data: {base64} end\n"),
                format!(
                    "data: {}[… 576 base64 characters cut …]{} end\n",
                    &base64[..12],
                    &base64[588..]
                ),
            ),
            (
                format!("{hex}\n"),
                format!(
                    "{}[… 168 hex characters cut …]{}\n",
                    &hex[..12],
                    &hex[180..]
                ),
            ),
            // Digests, paths and identifiers stay.
            (
                format!("checksum {}\n", "ab".repeat(64)),
                format!("checksum {}\n", "ab".repeat(64)),
            ),
            (
                format!("/{}\n", "dir/".repeat(80)),
                format!("/{}\n", "dir/".repeat(80)),
            ),
            (
                "tick\n".repeat(30),
                "tick\n[last line repeated 29 more time(s)]\n".to_owned(),
            ),
            // A short repeat costs less than its marker, so it stays.
            ("a\na\nb\n".to_owned(), "a\na\nb\n".to_owned()),
            // Failures are never folded or changed, and neither is a patch hunk.
            ("error: x\n".repeat(3), "error: x\n".repeat(3)),
            (format!("error{frames}\n"), format!("error{frames}\n")),
            (
                format!("@@ -1 +1 @@\n{}", "+x\n".repeat(30)),
                format!("@@ -1 +1 @@\n{}", "+x\n".repeat(30)),
            ),
        ] {
            assert_eq!(
                compact(&input).unwrap_or_else(|| input.clone()),
                expected,
                "{input:?}"
            );
        }
    }
}
