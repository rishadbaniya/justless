//! Vue, Svelte and Astro components keep their code in `<script>` elements, and
//! Astro also in a `---` frontmatter fence at the top of the file. Blanking the
//! markup and styles around that code, byte for byte, lets the TypeScript
//! grammar parse it with every offset and line number unchanged.

use std::ops::Range;

/// `source` with everything outside its scripts (and, with `frontmatter`, its
/// Astro frontmatter) replaced by spaces, keeping line breaks, and whether any
/// script is JSX or TSX.
pub(crate) fn scripts(source: &str, frontmatter: bool) -> (String, bool) {
    let mut keep = Vec::<Range<usize>>::new();
    let mut jsx = false;
    let mut at = 0;
    if frontmatter && let Some(fence) = fence(source) {
        at = fence.end;
        keep.push(fence);
    }
    while let Some(offset) = source[at..].find('<') {
        let start = at + offset;
        let rest = &source[start..];
        if rest.starts_with("<!--") {
            at = rest.find("-->").map_or(source.len(), |end| start + end + 3);
            continue;
        }
        if opens_tag(source, start, b"<style") {
            let Some(tag_end) = tag_end(source, start) else {
                at = start + 1;
                continue;
            };
            let tag = &source[start + "<style".len()..tag_end - 1];
            at = if tag.trim_end().ends_with('/') {
                tag_end
            } else {
                find_close_tag(source, tag_end, b"</style").unwrap_or(source.len())
            };
            continue;
        }
        if !opens_tag(source, start, b"<script") {
            at = if is_start_tag(source, start) {
                tag_end(source, start).unwrap_or(start + 1)
            } else {
                start + 1
            };
            continue;
        }
        let Some(tag_end) = tag_end(source, start) else {
            at = start + 1;
            continue;
        };
        let tag = &source[start + "<script".len()..tag_end - 1];
        if tag.trim_end().ends_with('/') {
            at = tag_end;
            continue;
        }
        let close = find_close_tag(source, tag_end, b"</script").unwrap_or(source.len());
        if let Some(dialect_jsx) = dialect(tag) {
            jsx |= dialect_jsx;
            keep.push(tag_end..close);
        }
        at = close;
    }
    let mut masked = String::with_capacity(source.len());
    let mut pos = 0;
    for range in keep {
        push_blanked(&mut masked, &source[pos..range.start]);
        masked.push_str(&source[range.start..range.end]);
        pos = range.end;
    }
    push_blanked(&mut masked, &source[pos..]);
    (masked, jsx)
}

/// Whether `source[start..]` begins with an ASCII-case-insensitive `<tag` boundary
/// (followed by whitespace, `>`, `/`, or end of input).
fn opens_tag(source: &str, start: usize, tag: &[u8]) -> bool {
    let bytes = source.as_bytes();
    bytes[start..]
        .get(..tag.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(tag))
        && {
            let after = &source[start + tag.len()..];
            after.is_empty()
                || after.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/')
        }
}

/// Whether `source[start..]` starts an HTML/component opening tag (`<tagName ...>`),
/// as opposed to a text comparison such as `a < b`.
fn is_start_tag(source: &str, start: usize) -> bool {
    let bytes = source.as_bytes();
    let Some(&first) = bytes.get(start + 1) else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    let mut i = start + 2;
    while let Some(&b) = bytes.get(i) {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.') {
            i += 1;
        } else {
            return b.is_ascii_whitespace() || matches!(b, b'>' | b'/');
        }
    }
    false
}

/// Find the byte offset of the next ASCII-case-insensitive closing tag starting at `from`.
fn find_close_tag(source: &str, mut from: usize, close: &[u8]) -> Option<usize> {
    let bytes = source.as_bytes();
    while let Some(offset) = source[from..].find('<') {
        let at = from + offset;
        if bytes[at..]
            .get(..close.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(close))
        {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// Append `segment` with every non-newline byte replaced by an ASCII space.
/// Multi-byte UTF-8 sequences only contain bytes >= 0x80 (never `\n` or `\r`),
/// so replacing each non-`\n`/`\r` byte with `' '` preserves exact UTF-8 byte length.
fn push_blanked(out: &mut String, segment: &str) {
    for byte in segment.bytes() {
        match byte {
            b'\n' => out.push('\n'),
            b'\r' => out.push('\r'),
            _ => out.push(' '),
        }
    }
}

/// The body of an Astro frontmatter fence: from the line after an opening
/// `---` at the top of the file to the line before the closing one.
fn fence(source: &str) -> Option<Range<usize>> {
    let start = source.len() - source.trim_start().len();
    let first = source[start..].split_inclusive('\n').next()?;
    if first.trim() != "---" {
        return None;
    }
    let body = start + first.len();
    let mut line_start = body;
    for line in source[body..].split_inclusive('\n') {
        if line.trim() == "---" {
            return Some(body..line_start);
        }
        line_start += line.len();
    }
    None
}

/// The offset just past the `>` that ends the tag starting at `start`,
/// skipping quoted attribute values (only when preceded by `=`) and aborting
/// on an unquoted `<` so unclosed text comparisons like `x<y don't` never
/// swallow a following tag or `<script>` block.
fn tag_end(source: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    let mut expect_attr_value = false;
    for (offset, byte) in source.bytes().enumerate().skip(start) {
        match (quote, byte) {
            (Some(open), _) if byte == open => quote = None,
            (Some(_), _) => {}
            (None, b'=') => expect_attr_value = true,
            (None, b'"' | b'\'') if expect_attr_value => {
                quote = Some(byte);
                expect_attr_value = false;
            }
            (None, b'>') => return Some(offset + 1),
            (None, b'<') if offset > start => return None,
            (None, b) if b.is_ascii_whitespace() => {}
            (None, _) => expect_attr_value = false,
        }
    }
    None
}

/// Whether a script with these attributes is JavaScript or TypeScript, and if
/// so whether it is JSX. JSON, templates and other languages are not.
fn dialect(tag: &str) -> Option<bool> {
    let mut jsx = false;
    let mut rest = tag;
    while let Some((name, value, next)) = next_attribute(rest) {
        rest = next;
        if name.eq_ignore_ascii_case("lang") {
            if value.eq_ignore_ascii_case("js")
                || value.eq_ignore_ascii_case("javascript")
                || value.eq_ignore_ascii_case("ts")
                || value.eq_ignore_ascii_case("typescript")
            {
            } else if value.eq_ignore_ascii_case("jsx") || value.eq_ignore_ascii_case("tsx") {
                jsx = true;
            } else {
                return None;
            }
        } else if name.eq_ignore_ascii_case("type")
            && !(value.is_empty()
                || value.eq_ignore_ascii_case("module")
                || value.eq_ignore_ascii_case("text/javascript")
                || value.eq_ignore_ascii_case("application/javascript")
                || value.eq_ignore_ascii_case("text/typescript")
                || value.eq_ignore_ascii_case("application/typescript"))
        {
            return None;
        }
    }
    Some(jsx)
}

/// The next `(name, value, remainder)` attribute of a tag without allocations.
/// An attribute without a value has an empty one.
fn next_attribute(tag: &str) -> Option<(&str, &str, &str)> {
    let rest = tag.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
    if rest.is_empty() {
        return None;
    }
    let name_end = rest
        .find(|c: char| c.is_whitespace() || c == '=' || c == '/')
        .unwrap_or(rest.len());
    let name = &rest[..name_end];
    let mut rest = rest[name_end..].trim_start();
    let mut value = "";
    if let Some(after) = rest.strip_prefix('=') {
        let after = after.trim_start();
        if let Some(quote) = after.chars().next().filter(|c| matches!(c, '"' | '\'')) {
            let body = &after[1..];
            let end = body.find(quote).unwrap_or(body.len());
            value = &body[..end];
            rest = body.get(end + 1..).unwrap_or("");
        } else {
            let end = after.find(char::is_whitespace).unwrap_or(after.len());
            value = &after[..end];
            rest = &after[end..];
        }
    }
    Some((name, value, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_script_code_survives_at_its_own_offsets() {
        for (source, frontmatter, kept, jsx) in [
            (
                "<template>\n  <p>{{ total }}</p>\n</template>\n<script setup lang=\"ts\">\nconst total = 1\n</script>\n<style>p { color: red }</style>\n",
                false,
                vec!["\nconst total = 1\n"],
                false,
            ),
            (
                "<script context=\"module\">export const x = 1</script>\n<!-- <script>no()</script> -->\n<script lang='tsx'>let y = <b/></script>",
                false,
                vec!["export const x = 1", "let y = <b/>"],
                true,
            ),
            (
                "<script type=\"application/ld+json\">{}</script><script src=\"a.js\" /><script lang=\"coffee\">x</script>",
                false,
                vec![],
                false,
            ),
            (
                "---\nimport Card from './Card.astro'\n---\n<Card title={\"é\"} />\n<script>\nhello()\n</script>",
                true,
                vec!["import Card from './Card.astro'\n", "\nhello()\n"],
                false,
            ),
            (
                "<template><p>日本語🚀</p></template>\n<SCRIPT LANG=\"TSX\">const s = \"é\"</SCRIPT>\n",
                false,
                vec!["const s = \"é\""],
                true,
            ),
            (
                "<template>\n  <p title=\"<!-- not a real comment\">hi</p>\n</template>\n<script lang=\"ts\">\nexport const ok = 1\n</script>\n",
                false,
                vec!["\nexport const ok = 1\n"],
                false,
            ),
            (
                "<template><p title='<!-- single quoted'>hi</p><p data-x=\"<script>fake()</script>\">ok</p></template>\n<script>\nconst real = 2\n</script>\n",
                false,
                vec!["\nconst real = 2\n"],
                false,
            ),
            (
                "<template><p>{a < b && \"don't\"}</p></template>\n<style>p::before { content: \"<!--\"; }</style>\n<script lang=\"ts\">\nconst afterStyle = 3\n</script>\n",
                false,
                vec!["\nconst afterStyle = 3\n"],
                false,
            ),
            (
                "<template>\n  <p>if x<y don't worry</p>\n</template>\n<script lang=\"ts\">\nconst msg = \"can't break\";\nexport const ok = 4\n</script>\n",
                false,
                vec!["\nconst msg = \"can't break\";\nexport const ok = 4\n"],
                false,
            ),
            (
                "<template><p>if x<y <span title = '<!-- not a comment'>hi</span></p></template>\n<script setup lang=\"ts\" generic=\"T extends Array<string>\">\nexport const ok = 5\n</script>\n",
                false,
                vec!["\nexport const ok = 5\n"],
                false,
            ),
            (
                "<template><p>if x<y <script lang=\"ts\">export const inlineOk = 6</script></p></template>\n",
                false,
                vec!["export const inlineOk = 6"],
                false,
            ),
        ] {
            let (masked, found_jsx) = scripts(source, frontmatter);
            assert_eq!(masked.len(), source.len(), "{source}");
            assert_eq!(masked.lines().count(), source.lines().count(), "{source}");
            for code in &kept {
                let at = source.find(code).unwrap();
                assert_eq!(&masked[at..at + code.len()], *code, "{source}");
            }
            let words: Vec<_> = kept
                .iter()
                .flat_map(|code| code.split_whitespace())
                .collect();
            assert_eq!(
                masked.split_whitespace().collect::<Vec<_>>(),
                words,
                "{source}"
            );
            assert_eq!(found_jsx, jsx, "{source}");
        }
    }
}
