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
}

/// Calls `visit` with the byte offset and text of each word.
pub(crate) fn words(text: &str, mut visit: impl FnMut(usize, &str)) {
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Digit,
    Upper,
    Lower,
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
        for (_, range) in &self.parts {
            let part = &word[range.clone()];
            if part.chars().nth(1).is_none()
                || (part.len() > 6 && part.bytes().all(|byte| byte.is_ascii_digit()))
            {
                continue;
            }
            self.lower.clear();
            push_lower(&mut self.lower, part);
            emit(&stem(&self.lower));
        }
    }

    /// Splits one piece at lower-to-upper humps, before the last capital of an
    /// acronym that runs into a lowercase word, and around digit runs.
    fn camel(&mut self, piece: &str, offset: usize, owner: usize) {
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

/// A light, deterministic suffix stemmer for ASCII words of four or more
/// letters, so that "retries", "retrying" and "retried" all become "retri".
pub(crate) fn stem(word: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    let bytes = word.as_bytes();
    if bytes.len() <= 3 || !bytes.iter().all(u8::is_ascii_lowercase) {
        return Cow::Borrowed(word);
    }
    let mut stem = bytes.to_vec();
    let mut stripped = false;
    let length = stem.len();
    if (stem.ends_with(b"ies") || stem.ends_with(b"ied")) && length > 4 {
        stem.truncate(length - 3);
        stem.push(b'y');
    } else if stem.ends_with(b"ying") && length > 5 {
        stem.truncate(length - 3);
    } else if stem.ends_with(b"ing") && length - 3 >= 3 && has_vowel(&stem[..length - 3]) {
        let kept = undouble(&stem[..length - 3]).len();
        stem.truncate(kept);
        stripped = true;
    } else if stem.ends_with(b"eed") {
    } else if stem.ends_with(b"ed") && length - 2 >= 3 && has_vowel(&stem[..length - 2]) {
        let kept = undouble(&stem[..length - 2]).len();
        stem.truncate(kept);
        stripped = true;
    } else if length > 4
        && [&b"ses"[..], b"xes", b"zes", b"ches", b"shes"]
            .iter()
            .any(|suffix| stem.ends_with(suffix))
    {
        stem.truncate(length - 2);
    } else if stem.ends_with(b"s")
        && !(stem.ends_with(b"ss") || stem.ends_with(b"us") || stem.ends_with(b"is"))
    {
        stem.truncate(length - 1);
    }
    let length = stem.len();
    if stem.ends_with(b"ation") && length - 5 >= 4 {
        stem.truncate(length - 5);
    } else if stem.ends_with(b"ion") && length - 3 >= 4 && matches!(stem[length - 4], b't' | b's') {
        stem.truncate(length - 3);
    } else if stem.ends_with(b"or") && length - 2 >= 4 && stem[length - 3] == b't' {
        stem.truncate(length - 2);
    } else if stem.ends_with(b"ery") && length > 5 {
        stem.truncate(length - 1);
    } else if stem.ends_with(b"ate") && length - 3 >= 4 {
        stem.truncate(length - 3);
    } else if stripped && stem.ends_with(b"at") && length - 2 >= 4 {
        stem.truncate(length - 2);
    }
    let length = stem.len();
    if stem.ends_with(b"y") && length > 3 {
        stem[length - 1] = b'i';
    } else if stem.ends_with(b"e") && length >= 5 {
        stem.truncate(length - 1);
    }
    Cow::Owned(String::from_utf8(stem).expect("ASCII stem"))
}

/// A question split into searchable words and the deduplicated terms they yield.
pub(crate) struct Question {
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
        let mut ordered = Vec::<String>::new();
        let mut truncated = false;
        for (_, found) in &candidates {
            for token in found {
                if ordered.contains(token) {
                    continue;
                }
                if ordered.len() == MAX_TERMS {
                    truncated = true;
                } else {
                    ordered.push(token.clone());
                }
            }
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
        Some(Self {
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
        assert_eq!(all("utf8Decoder"), ["utf8decoder", "utf", "decoder"]);
        assert_eq!(all("v2_api"), ["v2api", "v2", "api"]);
        assert_eq!(all("__init__"), ["init"]);
        assert_eq!(all("déjà_vu"), ["déjàvu", "déjà", "vu"]);
        assert_eq!(all("IOError"), ["ioerror", "io", "error"]);
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
