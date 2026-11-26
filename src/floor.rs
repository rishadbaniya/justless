//! The generic compaction floor every ordinary preview gets: carriage-return
//! progress frames collapse to their final state, long base64 and hex runs keep
//! their ends and a length, and consecutive repeated lines fold into a count.
//!
//! It changes only what is displayed. Each elision says what was cut, and is used
//! only where it is shorter than the text it replaces. Lines that look like a
//! failure, warning or assertion, the lines that explain a failure as the
//! `[diagnostics]` scan reads them, such as a panic's message or a traceback's
//! frames, pytest's `E` lines, and lines inside a patch hunk, also a combined or
//! coloured one, are never changed, folded or dropped.
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
        || crate::stacktrace::is_summary_line(trimmed)
        || ["Traceback", "thread '", "left:", "right:"]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix))
        // pytest explains a failed assertion on lines that start with `E`.
        || line == "E"
        || line.starts_with("E ")
}

/// Whether `line`, without its line break and colours, is inside a patch hunk, given
/// whether the line before it was: a `@@ ` hunk header, a combined diff's `@@@`
/// one, and the context, added, removed and no-newline lines after it.
pub(crate) fn in_hunk(before: bool, line: &str) -> bool {
    line.starts_with("@@ ")
        || line.starts_with("@@@")
        || before && (line.is_empty() || line.starts_with([' ', '+', '-', '\\']))
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
        let mut count = 0usize;
        let mut last = "";
        let mut last_non_empty = None;
        for frame in body.split('\r') {
            count += 1;
            last = frame;
            if !frame.trim().is_empty() {
                last_non_empty = Some(frame);
            }
        }
        let shown = last_non_empty.unwrap_or(last);
        let collapsed = format!("{shown} [{} earlier progress frame(s) cut]", count - 1);
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
    if text.len() < HEX_RUN {
        return None;
    }
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
        if at - start < HEX_RUN {
            continue;
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
    let length = run.len();
    let mut seen = 0u128;
    let mut all_hex = true;
    let (mut upper, mut lower, mut digit, mut slash, mut dash_under) = (0usize, 0, 0, 0, 0);
    for b in run.bytes() {
        seen |= 1u128 << (b & 0x7f);
        all_hex &= b.is_ascii_hexdigit();
        upper += usize::from(b.is_ascii_uppercase());
        lower += usize::from(b.is_ascii_lowercase());
        digit += usize::from(b.is_ascii_digit());
        slash += usize::from(b == b'/');
        dash_under += usize::from(b == b'-' || b == b'_');
    }
    // A hex dump uses most digits; a run of one padding character is no data.
    if length >= HEX_RUN && seen.count_ones() >= 10 && all_hex {
        return Some("hex");
    }
    // Encoded data mixes cases and digits throughout; paths and identifiers do not.
    (length >= BASE64_RUN
        && upper * 10 >= length
        && lower * 10 >= length
        && digit * 30 >= length
        && slash * 16 <= length
        && dash_under * 16 <= length)
        .then_some("base64")
}

/// Whether `words` print a file's content, as `git show REV:PATH` and `git cat-file`
/// do. Such output shown whole stays byte for byte.
pub fn prints_file(words: &[&str]) -> bool {
    let effective = crate::wrappers::effective_words(words);
    let owned_refs: Vec<&str>;
    let words: &[&str] = match &effective {
        Some(crate::wrappers::EffectiveWords::Borrowed(slice)) => slice,
        Some(crate::wrappers::EffectiveWords::Owned(vec)) => {
            owned_refs = vec.iter().map(String::as_str).collect();
            &owned_refs
        }
        None => words,
    };
    let is_git = |program: &str| {
        std::path::Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            == Some("git")
    };
    match words {
        [git, "show", rest @ ..] if is_git(git) => {
            let mut iter = rest.iter().copied();
            while let Some(arg) = iter.next() {
                if arg == "--" {
                    break;
                }
                if matches!(
                    arg,
                    "-S" | "-G"
                        | "-L"
                        | "-n"
                        | "-U"
                        | "--max-count"
                        | "--since"
                        | "--after"
                        | "--until"
                        | "--before"
                        | "--author"
                        | "--committer"
                        | "--grep"
                        | "--format"
                        | "--pretty"
                        | "--date"
                        | "--output"
                        | "--unified"
                ) {
                    iter.next();
                    continue;
                }
                if arg.starts_with('-') {
                    continue;
                }
                if git_show_object_is_path(arg) {
                    return true;
                }
            }
            false
        }
        [git, "cat-file", ..] => is_git(git),
        _ => false,
    }
}

/// Whether a `git show` positional object spec names a blob/tree path (`REV:PATH`,
/// `:PATH`, `:0:PATH`) rather than a commit search (`:/msg`) or a reflog timestamp
/// (`HEAD@{2026-01-01 12:00:00}`).
fn git_show_object_is_path(arg: &str) -> bool {
    if arg.starts_with(":/") {
        return false;
    }
    let mut depth = 0usize;
    let bytes = arg.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' && i > 0 && matches!(bytes[i - 1], b'@' | b'^') {
            depth += 1;
        } else if bytes[i] == b'}' && depth > 0 {
            depth -= 1;
        } else if bytes[i] == b':' && depth == 0 {
            return true;
        }
        i += 1;
    }
    false
}

/// A line the floor shows, with its line break, and whether it is the
/// `[last line repeated N more time(s)]` marker that counts the line before it.
pub type Shown = (String, bool);

/// Applies the floor to a stream line by line, folding repeated lines (including
/// consecutive lines that differ only by a leading timestamp).
#[derive(Default)]
pub struct Floor {
    patch: bool,
    /// The last line shown and how many identical lines followed it.
    last: Option<String>,
    repeats: usize,
    /// Timestamp-stripped key of `last` when `last` carried a leading timestamp,
    /// plus any consecutive lines that matched after stripping their timestamp.
    last_ts_key: Option<String>,
    ts_repeats: usize,
    ts_bytes: usize,
    ts_buffered: Vec<String>,
}

impl Floor {
    /// The lines to show for one line of the stream, which keeps its line break:
    /// none while it repeats the line before. A `marked` line belongs to a failure
    /// block and is shown as it is.
    pub fn push(&mut self, text: &str, marked: bool, out: &mut Vec<Shown>) {
        let line = text.strip_suffix('\n').unwrap_or(text);
        // Colours, as `git diff --color` prints them, do not hide a hunk.
        let plain = if line.contains('\x1b') {
            crate::capture::ANSI.replace_all(line, "")
        } else {
            Cow::Borrowed(line)
        };
        let plain = plain.trim_end_matches('\r');
        self.patch = in_hunk(self.patch, plain);
        let keep = marked || self.patch || kept(plain);
        let shown = if keep {
            Cow::Borrowed(line)
        } else {
            self::line(line)
        };
        let newline = if text.ends_with('\n') { "\n" } else { "" };
        if !keep && self.ts_repeats == 0 && self.last.as_deref() == Some(shown.as_ref()) {
            self.repeats += 1;
            return;
        }
        let ts_key = if !keep {
            let trimmed = plain.trim();
            let key = crate::filters::log_dedup_key(trimmed);
            (key != trimmed && !key.is_empty()).then_some(key)
        } else {
            None
        };
        if !keep
            && let Some(key) = ts_key
            && self.last_ts_key.as_deref() == Some(key)
        {
            if self.repeats > 0 {
                let exact = std::mem::take(&mut self.repeats);
                let prev = self.last.as_deref().unwrap_or("");
                self.ts_repeats += exact;
                self.ts_bytes += exact * (prev.len() + 1);
                if self.ts_bytes <= 96 {
                    self.ts_buffered
                        .extend(std::iter::repeat_n(format!("{prev}\n"), exact));
                } else {
                    self.ts_buffered.clear();
                }
            }
            self.ts_repeats += 1;
            self.ts_bytes += shown.len() + newline.len();
            if self.ts_bytes <= 96 {
                let mut entry = String::with_capacity(shown.len() + newline.len());
                entry.push_str(&shown);
                entry.push_str(newline);
                self.ts_buffered.push(entry);
            } else {
                self.ts_buffered.clear();
            }
            return;
        }
        let ts_key = ts_key.map(str::to_owned);
        self.flush(out);
        let mut entry = String::with_capacity(shown.len() + newline.len());
        entry.push_str(&shown);
        entry.push_str(newline);
        out.push((entry, false));
        self.last = (!keep).then(|| shown.into_owned());
        self.last_ts_key = ts_key;
    }

    /// Show what folding still holds back, as at the end of the stream.
    pub fn flush(&mut self, out: &mut Vec<Shown>) {
        let repeats = std::mem::take(&mut self.repeats);
        let ts_repeats = std::mem::take(&mut self.ts_repeats);
        let ts_bytes = std::mem::take(&mut self.ts_bytes);
        let ts_buffered = std::mem::take(&mut self.ts_buffered);
        self.last_ts_key = None;
        let Some(last) = self.last.take() else {
            return;
        };
        if ts_repeats > 0 {
            let marker = format!(
                "[similar line repeated {ts_repeats} more time(s) with different timestamps]\n"
            );
            if marker.len() < ts_bytes {
                out.push((marker, true));
            } else {
                out.extend(ts_buffered.into_iter().map(|line| (line, false)));
            }
            return;
        }
        if repeats == 0 {
            return;
        }
        let marker = format!("[last line repeated {repeats} more time(s)]\n");
        if marker.len() < repeats * (last.len() + 1) {
            out.push((marker, true));
        } else {
            out.extend(std::iter::repeat_n((format!("{last}\n"), false), repeats));
        }
    }
}

/// The floor applied to a whole text, when it changed anything.
pub fn compact(text: &str) -> Option<String> {
    compact_with(text, None, None)
}

/// The floor applied to a captured stream whose workspace root and stream name
/// are known, collapsing external stack-trace frames with exact archive line
/// pointers before applying line-level folding.
pub fn compact_stream(
    text: &str,
    workspace_root: &std::path::Path,
    stream: &str,
) -> Option<String> {
    compact_with(text, Some(workspace_root), Some(stream))
}

fn compact_with(
    text: &str,
    workspace_root: Option<&std::path::Path>,
    stream: Option<&str>,
) -> Option<String> {
    let stacked = crate::stacktrace::collapse(text, workspace_root, stream);
    let source = stacked.as_deref().unwrap_or(text);
    let marks = crate::capture::failure_lines(source);
    let mut floor = Floor::default();
    let mut lines = Vec::new();
    for (line, marked) in source.split_inclusive('\n').zip(marks) {
        floor.push(line, marked, &mut lines);
    }
    floor.flush(&mut lines);
    let total_len: usize = lines.iter().map(|(line, _)| line.len()).sum();
    if total_len >= text.len() {
        return None;
    }
    let mut out = String::with_capacity(total_len);
    for (line, _) in lines {
        out.push_str(&line);
    }
    (out != text).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_reads_are_recognized() {
        for (words, expected) in [
            (&["git", "show", "HEAD:data.json"][..], true),
            (&["git", "show", ":0:Cargo.toml"][..], true),
            (&["git", "cat-file", "-p", "HEAD:x"][..], true),
            (
                &["/usr/bin/git", "--no-pager", "show", "HEAD:src/lib.rs"][..],
                true,
            ),
            (&["git", "show", "HEAD"][..], false),
            (&["git", "show", "--format=%h:%s"][..], false),
            (&["git", "show", "-S", "foo:bar", "HEAD"][..], false),
            (&["git", "show", "-G", "err:.*", "HEAD"][..], false),
            (
                &["git", "show", "--since", "2026-01-01T12:00:00", "HEAD"][..],
                false,
            ),
            (&["git", "show", "HEAD@{2026-01-01 12:00:00}"][..], false),
            (&["git", "show", ":/fix-bug"][..], false),
            (&["git", "show", "HEAD", "--", "foo:bar.txt"][..], false),
            (&["lazygit", "show", "HEAD:data.json"][..], false),
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

    #[test]
    fn failure_blocks_and_every_patch_stay_whole() {
        let base64: String = (0..150)
            .map(|i| ["Qm", "x9", "aZ", "k+", "P0"][i % 5])
            .collect();
        let hex: String = (0..200)
            .map(|i| char::from(b"0123456789abcdef"[i * 7 % 16]))
            .collect();
        let e = "\u{1b}";
        for input in [
            // A panic's message.
            format!(
                "thread 'tests::digest' panicked at src/lib.rs:10:5:\nunexpected digest {hex}\n"
            ),
            // A traceback's frames, one of them repeated.
            format!(
                "Traceback (most recent call last):\n  File \"a.py\", line 3, in <module>\n    load('{base64}')\n    load('{base64}')\nValueError: bad\n"
            ),
            // pytest's explanation of a failed assertion.
            format!(
                "E       AssertionError: assert left == right\nE         + {base64}\n{}",
                "E\n".repeat(30)
            ),
            // A combined diff's hunk.
            format!(
                "@@@ -1,3 -1,3 +1,30 @@@\n+ {base64}\n{}",
                "+ same line\n".repeat(25)
            ),
            // A coloured hunk.
            format!(
                "{e}[36m@@ -1,3 +1,12 @@{e}[m\n{e}[32m+{base64}{e}[m\n{}",
                format!("{e}[32m+same line{e}[m\n").repeat(10)
            ),
        ] {
            assert_eq!(compact(&input), None, "{input:?}");
        }
    }

    #[test]
    fn floor_folds_consecutive_timestamped_repeats_without_touching_errors() {
        let mut log = String::new();
        for s in 1..=10 {
            log.push_str(&format!(
                "2026-11-25T14:01:{s:02}.000Z INFO heartbeat ok worker=1\n"
            ));
        }
        log.push_str("2026-11-25T14:01:11.000Z ERROR db down\n");
        log.push_str("2026-11-25T14:01:12.000Z ERROR db down\n");
        let out = compact(&log).unwrap();
        assert!(
            out.contains("2026-11-25T14:01:01.000Z INFO heartbeat ok worker=1\n[similar line repeated 9 more time(s) with different timestamps]\n"),
            "{out}"
        );
        // Errors stay verbatim even when repeated with different timestamps.
        assert_eq!(out.matches("ERROR db down").count(), 2, "{out}");
    }

    #[test]
    fn floor_collapses_external_stack_frames_while_preserving_archive_line_numbers() {
        let mut log = String::new();
        for _ in 0..20 {
            log.push_str("waiting\n");
        }
        log.push_str("Error: connection lost\n");
        log.push_str("    at throwSite (node:net:1495:16)\n");
        for i in 0..8 {
            log.push_str(&format!(
                "    at Client.step{i} (/work/node_modules/pg/lib/client.js:{}:10)\n",
                100 + i
            ));
        }
        log.push_str("    at settle (/work/src/orders.ts:42:9)\n");
        let out = compact_stream(&log, std::path::Path::new("/work"), "stderr").unwrap();
        // Repeat folding folded lines 2..20, while stacktrace pointer still
        // points to exact original archive lines 23..30!
        assert!(
            out.contains("[last line repeated 19 more time(s)]\n"),
            "{out}"
        );
        assert!(
            out.contains("    [… 8 runtime/dependency frames (node_modules/pg) in stderr:23-30]\n"),
            "{out}"
        );
        assert!(
            out.contains("    at settle (/work/src/orders.ts:42:9)\n"),
            "{out}"
        );
    }

    #[test]
    fn floor_preserves_multibyte_utf8_across_progress_elision_and_ansi() {
        let hex = "0123456789abcdef".repeat(12);
        let frames: String = (0..=40)
            .map(|i| format!("\r進捗🚀 {:>3}%", i * 2))
            .collect();
        let e = "\u{1b}";
        let mut input = format!("開始{frames}\n日本語データ: {hex} 完了✨\n{e}[32m緑の行{e}[0m\n");
        for s in 1..=8 {
            input.push_str(&format!(
                "2026-11-26T10:00:{s:02}.000Z INFO 同期完了✨ node=α\n"
            ));
        }
        let out = compact(&input).expect("compacted");
        assert!(
            out.contains("進捗🚀  80% [41 earlier progress frame(s) cut]\n"),
            "{out}"
        );
        assert!(
            out.contains(&format!(
                "日本語データ: {}[… 168 hex characters cut …]{} 完了✨\n",
                &hex[..12],
                &hex[180..]
            )),
            "{out}"
        );
        assert!(out.contains(&format!("{e}[32m緑の行{e}[0m\n")), "{out}");
        assert!(
            out.contains(
                "2026-11-26T10:00:01.000Z INFO 同期完了✨ node=α\n[similar line repeated 7 more time(s) with different timestamps]\n"
            ),
            "{out}"
        );
    }
}
