//! Vue, Svelte and Astro components keep their code in `<script>` elements, and
//! Astro also in a `---` frontmatter fence at the top of the file. Blanking the
//! markup and styles around that code, byte for byte, lets the TypeScript
//! grammar parse it with every offset and line number unchanged.

use std::ops::Range;

/// `source` with everything outside its scripts (and, with `frontmatter`, its
/// Astro frontmatter) replaced by spaces, keeping line breaks, and whether any
/// script is JSX or TSX.
pub(crate) fn scripts(source: &str, frontmatter: bool) -> (String, bool) {
    let lower = source.to_ascii_lowercase();
    let mut keep = Vec::<Range<usize>>::new();
    let mut jsx = false;
    let mut at = 0;
    if frontmatter && let Some(fence) = fence(source) {
        at = fence.end;
        keep.push(fence);
    }
    while let Some(offset) = lower[at..].find('<') {
        let start = at + offset;
        let rest = &lower[start..];
        if rest.starts_with("<!--") {
            at = rest.find("-->").map_or(source.len(), |end| start + end + 3);
            continue;
        }
        let opens = rest.strip_prefix("<script").is_some_and(|after| {
            after.is_empty()
                || after.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/')
        });
        if !opens {
            at = start + 1;
            continue;
        }
        let Some(tag_end) = tag_end(source, start) else {
            break;
        };
        let tag = &source[start + "<script".len()..tag_end - 1];
        if tag.trim_end().ends_with('/') {
            at = tag_end;
            continue;
        }
        let close = lower[tag_end..]
            .find("</script")
            .map_or(source.len(), |offset| tag_end + offset);
        if let Some(dialect_jsx) = dialect(tag) {
            jsx |= dialect_jsx;
            keep.push(tag_end..close);
        }
        at = close;
    }
    let mut masked = String::with_capacity(source.len());
    let mut kept = keep.iter().peekable();
    for (offset, character) in source.char_indices() {
        while kept.next_if(|range| range.end <= offset).is_some() {}
        if kept.peek().is_some_and(|range| range.contains(&offset))
            || matches!(character, '\n' | '\r')
        {
            masked.push(character);
        } else {
            masked.extend(std::iter::repeat_n(' ', character.len_utf8()));
        }
    }
    (masked, jsx)
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
/// skipping quoted attribute values.
fn tag_end(source: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, byte) in source.bytes().enumerate().skip(start) {
        match (quote, byte) {
            (Some(open), _) if byte == open => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(byte),
            (None, b'>') => return Some(offset + 1),
            _ => {}
        }
    }
    None
}

/// Whether a script with these attributes is JavaScript or TypeScript, and if
/// so whether it is JSX. JSON, templates and other languages are not.
fn dialect(tag: &str) -> Option<bool> {
    let mut jsx = false;
    for (name, value) in attributes(tag) {
        let value = value.to_ascii_lowercase();
        match name.as_str() {
            "lang" => match value.as_str() {
                "js" | "javascript" | "ts" | "typescript" => {}
                "jsx" | "tsx" => jsx = true,
                _ => return None,
            },
            "type" => match value.as_str() {
                ""
                | "module"
                | "text/javascript"
                | "application/javascript"
                | "text/typescript"
                | "application/typescript" => {}
                _ => return None,
            },
            _ => {}
        }
    }
    Some(jsx)
}

/// The attributes of a tag, without the tag name, as lowercase names and raw
/// values. An attribute without a value has an empty one.
fn attributes(tag: &str) -> Vec<(String, &str)> {
    let mut found = Vec::new();
    let mut rest = tag;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
        if rest.is_empty() {
            return found;
        }
        let name_end = rest
            .find(|c: char| c.is_whitespace() || c == '=' || c == '/')
            .unwrap_or(rest.len());
        let name = rest[..name_end].to_ascii_lowercase();
        rest = rest[name_end..].trim_start();
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
        found.push((name, value));
    }
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
