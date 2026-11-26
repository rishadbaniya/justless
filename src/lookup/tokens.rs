//! Identifier-aware tokens, shared by indexed text and questions so that
//! `parseHttpHeader`, `parse_http_header` and "parse the HTTP header" meet.

use std::ops::Range;

/// Question words that never name anything in a repository. Code verbs such as
/// get, find, use and handle stay searchable because they are common identifiers.
pub(crate) const STOPWORDS: [&str; 61] = [
    "a", "an", "and", "are", "as", "at", "be", "been", "by", "can", "did", "do", "does", "for",
    "from", "had", "has", "have", "how", "i", "if", "in", "into", "is", "it", "its", "me", "my",
    "of", "on", "or", "our", "so", "that", "the", "their", "them", "then", "there", "these",
    "this", "those", "to", "too", "was", "we", "were", "what", "when", "where", "which", "who",
    "why", "will", "with", "would", "you", "your", "should", "could", "please",
];

/// Query terms beyond this many are ignored.
pub(crate) const MAX_TERMS: usize = 64;

const MAX_WORD: usize = 256;
const MAX_WHOLE: usize = 64;

/// Calls `emit` with every token of `text` and the byte offset of the word it
/// came from. Words are runs of Unicode letters, digits and `_`.
#[cfg(test)]
pub(crate) fn tokens(text: &str, mut emit: impl FnMut(&str, usize)) {
    let mut tokenizer = Tokenizer::default();
    words(text, |offset, word| {
        tokenizer.word(word, |token| emit(token, offset))
    });
    qualified(text, |offset, _, token| emit(token, offset));
}

/// Calls `visit` with the byte offset and text of each word.
pub(crate) fn words(text: &str, mut visit: impl FnMut(usize, &str)) {
    if text.is_ascii() {
        let bytes = text.as_bytes();
        let mut start = None;
        for (index, &byte) in bytes.iter().enumerate() {
            if byte.is_ascii_alphanumeric() || byte == b'_' {
                start.get_or_insert(index);
            } else if let Some(begin) = start.take() {
                visit(begin, &text[begin..index]);
            }
        }
        if let Some(begin) = start {
            visit(begin, &text[begin..]);
        }
        return;
    }
    let mut start = None;
    for (index, character) in text.char_indices() {
        if character.is_alphanumeric() || character == '_' {
            start.get_or_insert(index);
        } else if let Some(begin) = start.take() {
            visit(begin, &text[begin..index]);
        }
    }
    if let Some(begin) = start {
        visit(begin, &text[begin..]);
    }
}

/// Calls `emit` with each adjacent `A::B` qualified identifier pair as a
/// compact lowercased `a::b` token, together with its `[start, end)` byte
/// span in `text`.
pub(crate) fn qualified(text: &str, mut emit: impl FnMut(usize, usize, &str)) {
    if !text.contains("::") {
        return;
    }
    let mut previous: Option<(usize, usize)> = None;
    let mut prev_norm = String::new();
    let mut norm = String::new();
    let mut buffer = String::new();
    words(text, |start, word| {
        let end = start + word.len();
        if let Some((prev_start, prev_end)) = previous
            && text.get(prev_end..start) == Some("::")
        {
            compact_ident_into(&text[prev_start..prev_end], &mut prev_norm);
            compact_ident_into(word, &mut norm);
            if prev_norm.chars().nth(1).is_some()
                && norm.chars().nth(1).is_some()
                && !(prev_norm.bytes().all(|byte| byte.is_ascii_digit())
                    && norm.bytes().all(|byte| byte.is_ascii_digit()))
                && prev_norm.len() + 2 + norm.len() <= MAX_WHOLE
            {
                buffer.clear();
                buffer.push_str(&prev_norm);
                buffer.push_str("::");
                buffer.push_str(&norm);
                emit(prev_start, end, &buffer);
            }
        }
        previous = Some((start, end));
    });
}

fn push_compact_ident(text: &str, out: &mut String) {
    if text.is_ascii() {
        out.reserve(text.len());
        for byte in text.bytes() {
            if byte.is_ascii_alphanumeric() {
                out.push(char::from(byte.to_ascii_lowercase()));
            }
        }
        return;
    }
    for character in text.chars() {
        if character.is_alphanumeric() {
            if character.is_ascii() {
                out.push(character.to_ascii_lowercase());
            } else {
                out.extend(character.to_lowercase());
            }
        }
    }
}

/// Writes the lowercased alphanumeric form of `text` (ignoring `_` and `-`)
/// into `out`, reusing its allocation.
pub(crate) fn compact_ident_into(text: &str, out: &mut String) {
    out.clear();
    push_compact_ident(text, out);
}

/// Lowercased alphanumeric form of an identifier, ignoring `_` and `-`, so
/// `parseHttpHeader`, `parse_http_header`, and `parse-http-header` match.
pub(crate) fn compact_ident(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    push_compact_ident(text, &mut out);
    out
}

/// Writes the normalized qualified symbol path (`Foo::bar_baz` or `Foo.barBaz`
/// -> `foo::barbaz`) into `out`, reusing its allocation.
pub(crate) fn normalize_qualified_into(text: &str, out: &mut String) {
    out.clear();
    for segment in text.split("::").flat_map(|part| part.split('.')) {
        let before = out.len();
        if !out.is_empty() {
            out.push_str("::");
        }
        let content_start = out.len();
        push_compact_ident(segment, out);
        if out.len() == content_start {
            out.truncate(before);
        }
    }
}

/// Normalizes a qualified symbol path (`Foo::bar_baz` or `Foo.barBaz`) to
/// `foo::barbaz` for exact/normalized symbol matching.
pub(crate) fn normalize_qualified(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    normalize_qualified_into(text, &mut out);
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Digit,
    Upper,
    Lower,
}

fn class_byte(byte: u8) -> Class {
    if byte.is_ascii_digit() {
        Class::Digit
    } else if byte.is_ascii_uppercase() {
        Class::Upper
    } else {
        Class::Lower
    }
}

fn class(character: char) -> Class {
    if character.is_ascii_digit() {
        Class::Digit
    } else if character.is_uppercase() {
        Class::Upper
    } else {
        Class::Lower
    }
}

/// Reusable buffers for splitting words.
#[derive(Default)]
pub(crate) struct Tokenizer {
    /// (piece, byte range in the word) of each camel-case part.
    parts: Vec<(usize, Range<usize>)>,
    characters: Vec<(usize, char)>,
    joined: String,
    lower: String,
}

impl Tokenizer {
    /// Emits the tokens of one word: the whole identifier without separators,
    /// each multi-part `_` piece, then each stemmed part.
    pub(crate) fn word(&mut self, word: &str, mut emit: impl FnMut(&str)) {
        if word.len() > MAX_WORD {
            return;
        }
        self.parts.clear();
        let mut pieces = 0;
        let mut offset = 0;
        for piece in word.split('_') {
            if !piece.is_empty() {
                self.camel(piece, offset, pieces);
                pieces += 1;
            }
            offset += piece.len() + 1;
        }
        if self.parts.len() >= 2 {
            self.joined.clear();
            for (_, range) in &self.parts {
                push_lower(&mut self.joined, &word[range.clone()]);
            }
            if (2..=MAX_WHOLE).contains(&self.joined.len()) {
                emit(&self.joined);
            }
        }
        if pieces >= 2 {
            let mut first = 0;
            while first < self.parts.len() {
                let piece = self.parts[first].0;
                let last = first
                    + self.parts[first..]
                        .iter()
                        .take_while(|(owner, _)| *owner == piece)
                        .count();
                if last - first >= 2 {
                    self.joined.clear();
                    for (_, range) in &self.parts[first..last] {
                        push_lower(&mut self.joined, &word[range.clone()]);
                    }
                    if (2..=MAX_WHOLE).contains(&self.joined.len()) {
                        emit(&self.joined);
                    }
                }
                first = last;
            }
        }
        // Emit letter+digit compounds (e.g. `v2`, `s3`, `utf8`, `http2`) and
        // short numeronyms (e.g. `k8s`, `i18n`, `e2e`) inside multi-part
        // camelCase pieces so `v2Api` and `s3Bucket` meet `v2_api` and `s3_bucket`.
        if self.parts.len() >= 3 {
            let mut first = 0;
            while first < self.parts.len() {
                let piece = self.parts[first].0;
                let last = first
                    + self.parts[first..]
                        .iter()
                        .take_while(|(owner, _)| *owner == piece)
                        .count();
                if last - first >= 3 {
                    let mut index = first;
                    while index + 1 < last {
                        let left = &word[self.parts[index].1.clone()];
                        let mid = &word[self.parts[index + 1].1.clone()];
                        let left_alpha = !left.is_empty() && left.chars().all(char::is_alphabetic);
                        let mid_digits = (1..=4).contains(&mid.len())
                            && mid.bytes().all(|byte| byte.is_ascii_digit());
                        if left_alpha && mid_digits {
                            let right = (index + 2 < last)
                                .then(|| &word[self.parts[index + 2].1.clone()])
                                .filter(|part| {
                                    left.len() == 1
                                        && part.len() == 1
                                        && part.bytes().all(|byte| byte.is_ascii_lowercase())
                                });
                            let span_len = if right.is_some() { 3 } else { 2 };
                            if span_len < last - first {
                                self.joined.clear();
                                push_lower(&mut self.joined, left);
                                self.joined.push_str(mid);
                                if let Some(right) = right {
                                    push_lower(&mut self.joined, right);
                                }
                                if (2..=MAX_WHOLE).contains(&self.joined.len()) {
                                    emit(&self.joined);
                                }
                            }
                            index += span_len;
                            continue;
                        }
                        index += 1;
                    }
                }
                first = last;
            }
        }
        let mut stem_buf = [0u8; MAX_WORD];
        for (_, range) in &self.parts {
            let part = &word[range.clone()];
            if part.chars().nth(1).is_none()
                || (part.len() > 6 && part.bytes().all(|byte| byte.is_ascii_digit()))
            {
                continue;
            }
            self.lower.clear();
            push_lower(&mut self.lower, part);
            let bytes = self.lower.as_bytes();
            if bytes.len() > 3
                && bytes.len() <= MAX_WORD
                && bytes.iter().all(u8::is_ascii_lowercase)
            {
                stem_buf[..bytes.len()].copy_from_slice(bytes);
                let new_len = stem_ascii_slice(&mut stem_buf[..bytes.len()]);
                emit(std::str::from_utf8(&stem_buf[..new_len]).expect("ASCII stem"));
            } else {
                emit(&stem(&self.lower));
            }
        }
    }

    /// Splits one piece at lower-to-upper humps, before the last capital of an
    /// acronym that runs into a lowercase word, and around digit runs.
    fn camel(&mut self, piece: &str, offset: usize, owner: usize) {
        if piece.is_ascii() {
            let bytes = piece.as_bytes();
            if bytes.is_empty() {
                return;
            }
            if bytes.iter().all(u8::is_ascii_lowercase) {
                self.parts.push((owner, offset..offset + bytes.len()));
                return;
            }
            let mut start = 0;
            for index in 1..bytes.len() {
                let previous = class_byte(bytes[index - 1]);
                let current = class_byte(bytes[index]);
                let next = bytes.get(index + 1).map(|&b| class_byte(b));
                let boundary = (previous == Class::Digit) != (current == Class::Digit)
                    || (previous == Class::Lower && current == Class::Upper)
                    || (previous == Class::Upper
                        && current == Class::Upper
                        && next == Some(Class::Lower));
                if boundary {
                    self.parts.push((owner, offset + start..offset + index));
                    start = index;
                }
            }
            self.parts
                .push((owner, offset + start..offset + bytes.len()));
            return;
        }
        self.characters.clear();
        self.characters.extend(piece.char_indices());
        let mut start = 0;
        for index in 1..self.characters.len() {
            let previous = class(self.characters[index - 1].1);
            let current = class(self.characters[index].1);
            let next = self
                .characters
                .get(index + 1)
                .map(|&(_, character)| class(character));
            let boundary = (previous == Class::Digit) != (current == Class::Digit)
                || (previous == Class::Lower && current == Class::Upper)
                || (previous == Class::Upper
                    && current == Class::Upper
                    && next == Some(Class::Lower));
            if boundary {
                let begin = self.characters[start].0;
                let end = self.characters[index].0;
                self.parts.push((owner, offset + begin..offset + end));
                start = index;
            }
        }
        if let Some(&(begin, _)) = self.characters.get(start) {
            self.parts
                .push((owner, offset + begin..offset + piece.len()));
        }
    }
}

fn push_lower(target: &mut String, text: &str) {
    if text.is_ascii() {
        target.extend(
            text.bytes()
                .map(|byte| char::from(byte.to_ascii_lowercase())),
        );
    } else {
        target.extend(text.chars().flat_map(char::to_lowercase));
    }
}

fn has_vowel(text: &[u8]) -> bool {
    text.iter().any(|byte| b"aeiouy".contains(byte))
}

/// Drops a doubled final consonant, as in "stopp" → "stop", but keeps
/// "ll", "ss" and "zz" so that "called" still meets "call".
fn undouble(text: &[u8]) -> &[u8] {
    match text {
        [.., a, b] if text.len() >= 3 && a == b && !b"lszaeiou".contains(b) => {
            &text[..text.len() - 1]
        }
        _ => text,
    }
}

/// Stems an ASCII-lowercase byte slice in place and returns its new length
/// (always `<= stem.len()`).
fn stem_ascii_slice(stem: &mut [u8]) -> usize {
    let mut len = stem.len();
    let mut stripped = false;
    if (stem[..len].ends_with(b"ies") || stem[..len].ends_with(b"ied")) && len > 4 {
        len -= 2;
        stem[len - 1] = b'y';
    } else if stem[..len].ends_with(b"ying") && len > 5 {
        len -= 3;
    } else if stem[..len].ends_with(b"ing") && len - 3 >= 3 && has_vowel(&stem[..len - 3]) {
        len = undouble(&stem[..len - 3]).len();
        stripped = true;
    } else if stem[..len].ends_with(b"eed") {
    } else if stem[..len].ends_with(b"ed") && len - 2 >= 3 && has_vowel(&stem[..len - 2]) {
        len = undouble(&stem[..len - 2]).len();
        stripped = true;
    } else if len > 4
        && [&b"ses"[..], b"xes", b"zes", b"ches", b"shes"]
            .iter()
            .any(|suffix| stem[..len].ends_with(suffix))
    {
        len -= 2;
    } else if stem[..len].ends_with(b"s")
        && !(stem[..len].ends_with(b"ss")
            || stem[..len].ends_with(b"us")
            || stem[..len].ends_with(b"is"))
    {
        len -= 1;
    }
    if stem[..len].ends_with(b"ation") && len - 5 >= 4 {
        len -= 5;
    } else if stem[..len].ends_with(b"ion") && len - 3 >= 4 && matches!(stem[len - 4], b't' | b's')
    {
        len -= 3;
    } else if stem[..len].ends_with(b"or") && len - 2 >= 4 && stem[len - 3] == b't' {
        len -= 2;
    } else if stem[..len].ends_with(b"ery") && len > 5 {
        len -= 1;
    } else if stem[..len].ends_with(b"ate") && len - 3 >= 4 {
        len -= 3;
    } else if stripped && stem[..len].ends_with(b"at") && len - 2 >= 4 {
        len -= 2;
    }
    if stem[..len].ends_with(b"y") && len > 3 {
        stem[len - 1] = b'i';
    } else if stem[..len].ends_with(b"e") && len >= 5 {
        len -= 1;
    }
    len
}

/// A light, deterministic suffix stemmer for ASCII words of four or more
/// letters, so that "retries", "retrying" and "retried" all become "retri".
pub(crate) fn stem(word: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    let bytes = word.as_bytes();
    if bytes.len() <= 3 || !bytes.iter().all(u8::is_ascii_lowercase) {
        return Cow::Borrowed(word);
    }
    if bytes.len() <= MAX_WORD {
        let mut buf = [0u8; MAX_WORD];
        buf[..bytes.len()].copy_from_slice(bytes);
        let new_len = stem_ascii_slice(&mut buf[..bytes.len()]);
        if buf[..new_len] == bytes[..new_len] {
            return Cow::Borrowed(&word[..new_len]);
        }
        return Cow::Owned(
            std::str::from_utf8(&buf[..new_len])
                .expect("ASCII stem")
                .to_owned(),
        );
    }
    let mut buf = bytes.to_vec();
    let new_len = stem_ascii_slice(&mut buf);
    buf.truncate(new_len);
    Cow::Owned(String::from_utf8(buf).expect("ASCII stem"))
}

/// Known source/config/doc file extensions used to recognize explicit file hints
/// in questions (like `retry.rs` or `app.toml`) without mistaking qualified
/// symbols (like `DownloadQueue.retry`) for paths.
const FILE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "rb", "java", "kt", "kts", "cs",
    "cpp", "cc", "cxx", "c", "h", "hpp", "md", "markdown", "mdx", "toml", "yaml", "yml", "json",
    "sql", "sh", "bash", "zsh", "txt", "ini", "cfg", "conf", "xml", "html", "css", "scss", "vue",
    "svelte", "php", "swift", "scala", "zig", "lua", "ex", "exs", "dart", "hcl", "tf", "ps1",
    "groovy", "csv", "tsv", "lock",
];

fn is_path_hint(token: &str) -> bool {
    if token.contains('/') && token.chars().any(char::is_alphanumeric) {
        return true;
    }
    if let Some((stem, ext)) = token.rsplit_once('.')
        && !stem.is_empty()
        && stem.chars().any(char::is_alphanumeric)
        && !ext.is_empty()
    {
        return FILE_EXTENSIONS
            .iter()
            .any(|candidate| ext.eq_ignore_ascii_case(candidate));
    }
    false
}

fn is_identifier_hint(token: &str) -> bool {
    if token.contains("::") {
        return token.split("::").filter(|part| !part.is_empty()).count() >= 2;
    }
    if token.contains('.') {
        let mut count = 0usize;
        let valid = token.split('.').all(|part| {
            count += 1;
            !part.is_empty() && part.chars().all(|c| c.is_alphanumeric() || c == '_')
        });
        return count >= 2 && valid && !is_path_hint(token);
    }
    if token.contains('_') && token.chars().any(char::is_alphanumeric) {
        return true;
    }
    if let Some((left, right)) = token.split_once('-')
        && !left.is_empty()
        && !right.is_empty()
        && left.chars().all(char::is_alphanumeric)
        && right
            .chars()
            .all(|character| character.is_alphanumeric() || character == '-')
    {
        return true;
    }
    // Mixed camelCase / PascalCase / SCREAMING acronym: an uppercase letter after
    // the first character (avoids boosting ordinary capitalized sentence starters).
    token.chars().skip(1).any(char::is_uppercase) && token.chars().all(char::is_alphanumeric)
}

fn trim_hint_token(token: &str) -> &str {
    let trimmed = token.trim_matches([
        '"', '\'', '`', '(', ')', '[', ']', '{', '}', ',', ';', '?', '!', ':',
    ]);
    if let Some((head, _)) = trimmed.split_once('(')
        && !head.is_empty()
    {
        return head;
    }
    trimmed
}

/// Strips a trailing `:LINE` or `:START-END` suffix from a path hint.
fn strip_line_suffix(token: &str) -> &str {
    if let Some((path, suffix)) = token.rsplit_once(':')
        && !path.is_empty()
        && !suffix.is_empty()
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
    {
        return path;
    }
    token
}

/// A question split into searchable words and the deduplicated terms they yield.
pub(crate) struct Question {
    /// Trimmed raw question without trailing punctuation, for exact symbol/heading match.
    pub(crate) raw: String,
    /// Identifier-shaped tokens in the question (`Foo::bar`, `snake_case`, `camelCase`).
    pub(crate) identifiers: Vec<String>,
    /// File path or filename hints in the question (`src/net/retry.rs`, `app.toml`).
    pub(crate) path_hints: Vec<String>,
    /// Searchable words as typed, lowercased, in question order and without repeats.
    pub(crate) words: Vec<Word>,
    /// Query terms in sorted order, at most [`MAX_TERMS`].
    pub(crate) terms: Vec<String>,
    /// The question had more than [`MAX_TERMS`] distinct terms.
    pub(crate) truncated: bool,
}

pub(crate) struct Word {
    pub(crate) text: String,
    /// Indexes into [`Question::terms`].
    pub(crate) terms: Vec<usize>,
}

impl Question {
    /// Returns `None` when no searchable word remains.
    pub(crate) fn parse(question: &str) -> Option<Self> {
        let mut tokenizer = Tokenizer::default();
        let mut candidates = Vec::<(String, Vec<String>)>::new();
        words(question, |_, word| {
            let mut found = Vec::new();
            tokenizer.word(word, |token| {
                if !found.iter().any(|seen| seen == token) {
                    found.push(token.to_owned());
                }
            });
            if !found.is_empty() {
                candidates.push((word.to_lowercase(), found));
            }
        });
        let searchable = candidates
            .iter()
            .any(|(word, _)| !STOPWORDS.contains(&word.as_str()));
        if searchable {
            candidates.retain(|(word, _)| !STOPWORDS.contains(&word.as_str()));
        }
        if candidates.is_empty() {
            return None;
        }
        let mut qualified_terms = Vec::<String>::new();
        qualified(question, |_, _, token| {
            if !qualified_terms.iter().any(|seen| seen == token) {
                qualified_terms.push(token.to_owned());
            }
        });
        let mut ordered = Vec::<String>::new();
        let mut truncated = false;
        let mut push_term = |token: &str| {
            if ordered.iter().any(|seen| seen == token) {
                return;
            }
            if ordered.len() == MAX_TERMS {
                truncated = true;
            } else {
                ordered.push(token.to_owned());
            }
        };
        for (_, found) in &candidates {
            for token in found {
                push_term(token);
            }
        }
        for token in &qualified_terms {
            push_term(token);
        }
        let mut terms = ordered;
        terms.sort();
        let mut words = Vec::<Word>::new();
        for (text, found) in candidates {
            if words.iter().any(|word| word.text == text) {
                continue;
            }
            let terms = found
                .iter()
                .filter_map(|token| terms.binary_search(token).ok())
                .collect();
            words.push(Word { text, terms });
        }
        let raw = question
            .trim()
            .trim_end_matches(['?', '!', '.', ','])
            .trim()
            .to_owned();
        let mut identifiers = Vec::new();
        let mut path_hints = Vec::new();
        for piece in question.split_whitespace() {
            let token = trim_hint_token(piece).trim_end_matches('.');
            if token.is_empty() {
                continue;
            }
            let path_candidate = strip_line_suffix(token);
            if is_path_hint(path_candidate) {
                let normalized = path_candidate.trim_start_matches("./");
                if !path_hints.iter().any(|seen| seen == normalized) {
                    path_hints.push(normalized.to_owned());
                }
            } else if is_identifier_hint(token) && !identifiers.iter().any(|seen| seen == token) {
                identifiers.push(token.to_owned());
            }
        }
        Some(Self {
            raw,
            identifiers,
            path_hints,
            words,
            terms,
            truncated,
        })
    }

    /// Whether the question asks about tests, which lifts the test prior.
    pub(crate) fn mentions_tests(&self) -> bool {
        self.terms
            .iter()
            .any(|term| term == "test" || term == "spec")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(text: &str) -> Vec<String> {
        let mut found = Vec::new();
        tokens(text, |token, _| found.push(token.to_owned()));
        found
    }

    #[test]
    fn identifiers_split_into_whole_and_stemmed_parts() {
        assert_eq!(
            all("parseHTTPResponse"),
            ["parsehttpresponse", "pars", "http", "respons"]
        );
        assert_eq!(
            all("parse_http_header"),
            ["parsehttpheader", "pars", "http", "header"]
        );
        assert_eq!(all("parseHttpHeader"), all("parse_http_header"));
        assert_eq!(all("PARSE_HTTP_HEADER"), all("parse_http_header"));
        assert_eq!(
            all("max_retry-count"),
            ["maxretry", "max", "retri", "count"]
        );
        assert_eq!(all("RetryPolicy"), ["retrypolicy", "retri", "polici"]);
        assert_eq!(all("DownloadQueue"), ["downloadqueue", "download", "queu"]);
        assert_eq!(
            all("utf8Decoder"),
            ["utf8decoder", "utf8", "utf", "decoder"]
        );
        assert_eq!(all("utf8Decoder"), all("utf8_decoder"));
        assert_eq!(all("v2_api"), ["v2api", "v2", "api"]);
        assert_eq!(all("v2Api"), all("v2_api"));
        assert_eq!(all("s3Bucket"), all("s3_bucket"));
        assert_eq!(all("http2Server"), all("http2_server"));
        assert_eq!(all("k8sClient"), all("k8s_client"));
        assert_eq!(all("e2eRunner"), all("e2e_runner"));
        assert_eq!(all("i18nLoader"), all("i18n_loader"));
        assert_eq!(all("__init__"), ["init"]);
        assert_eq!(all("déjà_vu"), ["déjàvu", "déjà", "vu"]);
        assert_eq!(all("IOError"), ["ioerror", "io", "error"]);
    }

    #[test]
    fn qualified_symbols_and_question_hints() {
        assert_eq!(all("Segment::open"), ["segment", "open", "segment::open"]);
        assert_eq!(
            all("RetryPolicy::backoff_ms"),
            [
                "retrypolicy",
                "retri",
                "polici",
                "backoffms",
                "backoff",
                "ms",
                "retrypolicy::backoffms"
            ]
        );
        assert_eq!(compact_ident("parse-http-header"), "parsehttpheader");
        assert_eq!(
            normalize_qualified("DownloadQueue.retry"),
            "downloadqueue::retry"
        );
        assert_eq!(
            normalize_qualified("RetryPolicy::backoff_ms"),
            "retrypolicy::backoffms"
        );

        let question =
            Question::parse("where is Segment::open(&path) called in src/lookup/store.rs:417?")
                .unwrap();
        assert_eq!(question.identifiers, ["Segment::open"]);
        assert_eq!(question.path_hints, ["src/lookup/store.rs"]);
        assert!(question.terms.contains(&"segment::open".to_owned()));

        let natural = Question::parse("where are retries configured?").unwrap();
        assert!(natural.identifiers.is_empty());
        assert!(natural.path_hints.is_empty());

        let dot = Question::parse("DownloadQueue.retry app.toml").unwrap();
        assert_eq!(dot.identifiers, ["DownloadQueue.retry"]);
        assert_eq!(dot.path_hints, ["app.toml"]);
    }

    #[test]
    fn digits_and_long_words() {
        assert_eq!(all("1234 1234567 x"), ["1234"]);
        assert!(all(&"a".repeat(257)).is_empty());
        assert_eq!(all(&"b".repeat(256)), ["b".repeat(256)]);
        assert_eq!(all("日本語のテキスト"), ["日本語のテキスト"]);
    }

    #[test]
    fn offsets_point_at_the_source_word() {
        let mut found = Vec::new();
        tokens("let retry_count = 1;", |token, at| {
            found.push(format!("{token}@{at}"))
        });
        assert_eq!(found, ["let@0", "retrycount@4", "retri@4", "count@4"]);
    }

    #[test]
    fn stemmer_vectors() {
        for (words, stemmed) in [
            ("retries retry retrying retried", "retri"),
            ("configured configuration configure", "configur"),
            ("redaction redactor redacted redacting redact", "redact"),
            (
                "compaction compactor compacted compacting compact",
                "compact",
            ),
            ("selection selector selected selecting select", "select"),
            ("discovery discover discovered discovering", "discover"),
            ("recovery recover recovered recovering", "recover"),
            ("editor edit edited editing", "edit"),
            ("reconnecting reconnects", "reconnect"),
            ("called calling", "call"),
            ("handled handle", "handl"),
            ("config", "config"),
            ("parse", "pars"),
            ("response", "respons"),
            ("stopped", "stop"),
            ("classes", "class"),
            ("status", "status"),
            ("agreed", "agreed"),
            ("use", "use"),
            ("Retries", "Retries"),
        ] {
            for word in words.split(' ') {
                assert_eq!(stem(word), stemmed, "{word}");
            }
        }
    }

    #[test]
    fn stopwords_are_dropped_from_questions() {
        assert_eq!(STOPWORDS.len(), 61);
        let question = Question::parse("where are errors handled").unwrap();
        let words = question
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(words, ["errors", "handled"]);
        assert_eq!(question.terms, ["error", "handl"]);
        let question = Question::parse("why did the upload fail").unwrap();
        assert_eq!(question.terms, ["fail", "upload"]);
        let question = Question::parse("where is it").unwrap();
        assert_eq!(question.terms, ["is", "it", "wher"]);
        assert!(Question::parse("   ").is_none());
        assert!(Question::parse("?!").is_none());
        let question = Question::parse("RetryPolicy retry").unwrap();
        assert_eq!(question.terms, ["polici", "retri", "retrypolicy"]);
        assert_eq!(question.words[1].terms, [1]);
        assert!(!question.mentions_tests());
        assert!(
            Question::parse("download retry tests")
                .unwrap()
                .mentions_tests()
        );
    }

    #[test]
    fn questions_keep_the_first_terms() {
        let long = (0..80)
            .map(|index| {
                format!("word{}", char::from(b'a' + (index % 26) as u8)).repeat(index / 26 + 1)
            })
            .collect::<Vec<_>>()
            .join(" ");
        let question = Question::parse(&long).unwrap();
        assert_eq!(question.terms.len(), MAX_TERMS);
        assert!(question.truncated);
    }
}
