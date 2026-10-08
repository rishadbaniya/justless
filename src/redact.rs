//! Secret redaction for captured command output.
//!
//! Bytes from a process Justless ran reach the agent only through Justless, so every
//! view of a run reads them through [`Reader`] or [`Redactor::bytes`]. Each detected
//! secret becomes a marker such as `[redacted:github-token:4f1c]` and line breaks are
//! never added or removed, so line numbers match the archive. The archive keeps the
//! original bytes, and `log --unredacted` replays them. Source views stay exact: edits
//! verify revisions over exact bytes, and the agent can open those files itself.
//!
//! Detection is layered for speed. A literal prefilter runs over newline-aligned
//! blocks, and only the lines it hits go through a `RegexSet` that selects the rules
//! to run. Clean bytes pass through untouched.
//!
//! Invariants the rules keep, which the long-line windows rely on:
//! - every rule's context before the secret is under 200 bytes and holds no newline;
//! - no real secret format exceeds 8 KiB (`HOLD`);
//! - no secret class contains a newline, and none of the token rules contains a quote.
//!
//! Token formats follow the gitleaks default rules (MIT licensed,
//! <https://github.com/gitleaks/gitleaks>), with a prefix plus a minimum length rather
//! than exact vendor lengths.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::hash::{BuildHasher, Hasher, RandomState};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use anyhow::{Result, anyhow, ensure};
use clap::Arg;
use regex::bytes::{Regex, RegexSet};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::workspace::Workspace;

/// Bump whenever a built-in rule changes, so archives scanned clean are scanned again.
const RULES_VERSION: &str = "1";
const READ_SIZE: usize = 64 * 1024;
/// Lines longer than this are redacted in windows instead of being held whole.
const WINDOW: usize = 1024 * 1024;
/// Bytes at the end of each window that are scanned again with the next one.
const HOLD: usize = 8 * 1024;
/// A private key block longer than this many lines ends the block.
const PEM_LINES: usize = 1024;

const TRIGGERS: &[&str] = &[
    "AKIA",
    "ASIA",
    "ABIA",
    "ACCA",
    "A3T",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "gldt-",
    "glrt-",
    "glptt-",
    "xox",
    "xapp-",
    "hooks.slack.com",
    "sk_live_",
    "rk_live_",
    "sk-",
    "AIza",
    "npm_",
    "hf_",
    "pypi-Ag",
    "eyJ",
    "://",
    "-----BEGIN",
    "LS0tLS1CRUdJTi",
];
const KEYWORDS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "api-key",
    "api.key",
    "apikey",
    "access_key",
    "access-key",
    "accesskey",
    "private_key",
    "private-key",
    "privatekey",
    "credential",
    "authorization",
    "bearer",
];

#[derive(Clone, Copy)]
enum Check {
    None,
    /// Minimum Shannon entropy in bits per byte.
    Entropy(f64),
    /// Entropy, at least one letter and one digit, and not shaped like a name or path.
    Generic(f64),
    /// At least 32 base64 bytes of key material.
    PemBody,
    /// Base64 whose decoded start names a private key.
    PemBase64,
}

struct Spec {
    kind: &'static str,
    pattern: &'static str,
    check: Check,
    /// The secret's byte class, used to absorb a value that runs past a window.
    tail: &'static str,
}

const SPECS: &[Spec] = &[
    Spec {
        kind: "private-key",
        pattern: r"(?-u)-----BEGIN[ A-Z0-9_-]{0,40}PRIVATE KEY(?: BLOCK)?-----(?P<s>(?:\\[nr]|[A-Za-z0-9+/=\s])+?)-----END[ A-Z0-9_-]{0,40}PRIVATE KEY(?: BLOCK)?-----",
        check: Check::PemBody,
        tail: r"[A-Za-z0-9+/=\\]",
    },
    // A header followed by key material on the same line with no END after it.
    Spec {
        kind: "private-key",
        pattern: r"(?-u)-----BEGIN[ A-Z0-9_-]{0,40}PRIVATE KEY(?: BLOCK)?-----(?P<s>(?:\\[nr]|[A-Za-z0-9+/=])+)",
        check: Check::PemBody,
        tail: r"[A-Za-z0-9+/=\\]",
    },
    Spec {
        kind: "private-key",
        pattern: r"(?-u)(?P<s>LS0tLS1CRUdJTi[A-Za-z0-9+/]{40,}={0,2})",
        check: Check::PemBase64,
        tail: r"[A-Za-z0-9+/=]",
    },
    Spec {
        kind: "aws-access-key",
        pattern: r"(?-u)\b(?P<s>(?:A3T[A-Z0-9]|AKIA|ASIA|ABIA|ACCA)[A-Z2-7]{16})\b",
        check: Check::Entropy(3.0),
        tail: r"[A-Z2-7]",
    },
    Spec {
        kind: "github-token",
        pattern: r"(?-u)\b(?P<s>(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{82,})",
        check: Check::Entropy(3.0),
        tail: r"[A-Za-z0-9_]",
    },
    Spec {
        kind: "gitlab-token",
        pattern: r"(?-u)\b(?P<s>gl(?:pat|dt|rt|ptt)-[A-Za-z0-9_-]{20,})",
        check: Check::Entropy(3.0),
        tail: r"[A-Za-z0-9_-]",
    },
    Spec {
        kind: "slack-token",
        pattern: r"(?-u)\b(?P<s>xox[abposre]-[A-Za-z0-9-]{10,}|xapp-[0-9]-[A-Za-z0-9-]{10,})",
        check: Check::Entropy(3.0),
        tail: r"[A-Za-z0-9-]",
    },
    Spec {
        kind: "slack-webhook",
        pattern: r"(?-u)hooks\.slack\.com/(?:services|workflows|triggers)/(?P<s>[A-Za-z0-9+/_-]{20,})",
        check: Check::None,
        tail: r"[A-Za-z0-9+/_-]",
    },
    Spec {
        kind: "stripe-key",
        pattern: r"(?-u)\b(?P<s>[rs]k_live_[A-Za-z0-9]{20,})",
        check: Check::Entropy(3.0),
        tail: r"[A-Za-z0-9]",
    },
    Spec {
        kind: "anthropic-key",
        pattern: r"(?-u)\b(?P<s>sk-ant-[a-z]{2,8}[0-9]{2}-[A-Za-z0-9_-]{20,})",
        check: Check::Entropy(3.0),
        tail: r"[A-Za-z0-9_-]",
    },
    Spec {
        kind: "openai-key",
        pattern: r"(?-u)\b(?P<s>sk-(?:proj|svcacct|admin)-[A-Za-z0-9_-]{20,}|sk-[A-Za-z0-9]{20}T3BlbkFJ[A-Za-z0-9]{20})",
        check: Check::Entropy(3.0),
        tail: r"[A-Za-z0-9_-]",
    },
    Spec {
        kind: "google-api-key",
        pattern: r"(?-u)(?P<s>AIza[A-Za-z0-9_-]{35})",
        check: Check::Entropy(3.0),
        tail: r"[A-Za-z0-9_-]",
    },
    Spec {
        kind: "npm-token",
        pattern: r"(?-u)\b(?P<s>npm_[A-Za-z0-9]{36})",
        check: Check::Entropy(3.0),
        tail: r"[A-Za-z0-9]",
    },
    Spec {
        kind: "pypi-token",
        pattern: r"(?-u)(?P<s>pypi-AgEIcHlwaS5vcmc[A-Za-z0-9_-]{50,})",
        check: Check::None,
        tail: r"[A-Za-z0-9_-]",
    },
    Spec {
        kind: "huggingface-token",
        pattern: r"(?-u)\b(?P<s>hf_[A-Za-z]{34})",
        check: Check::Entropy(3.0),
        tail: r"[A-Za-z]",
    },
    Spec {
        kind: "jwt",
        pattern: r"(?-u)\b(?P<s>eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,})",
        check: Check::Entropy(3.0),
        tail: r"[A-Za-z0-9_-]",
    },
    Spec {
        kind: "url-password",
        pattern: r#"(?i-u)\b[a-z][a-z0-9+.-]{1,20}://[^\s:/?#@'"<>]{0,128}:(?P<s>[^\s/?#@'"<>]{4,256})@"#,
        check: Check::None,
        tail: r#"[^\s/?#@'"<>]"#,
    },
    Spec {
        kind: "authorization",
        pattern: r#"(?i-u)\b(?:proxy-)?authorization(?:\\?["'])?\s{0,3}[:=]\s{0,3}(?:\\?["'])?(?:bearer|token|basic|digest)\s{1,3}(?P<s>[A-Za-z0-9._~+/=-]{8,})"#,
        check: Check::None,
        tail: r"[A-Za-z0-9._~+/=-]",
    },
    Spec {
        kind: "authorization",
        pattern: r"(?i-u)\bbearer\s{1,3}(?P<s>[A-Za-z0-9._~+/-]{16,}=*)",
        check: Check::Generic(3.0),
        tail: r"[A-Za-z0-9._~+/=-]",
    },
    Spec {
        kind: "secret",
        pattern: r#"(?i-u)(?:password|passwd|secret|token|api[_.-]?key|apikey|access[_.-]?key|private[_.-]?key|credentials?)[A-Za-z0-9_.-]{0,20}(?:\\?["'])?\s{0,3}(?::=|=>|=|:)\s{0,3}(?:\\?["'`])?(?P<s>[A-Za-z0-9+/_.~=!#%^&*-]{12,})"#,
        check: Check::Generic(3.5),
        tail: r"[A-Za-z0-9+/_.~=!#%^&*-]",
    },
    Spec {
        kind: "secret",
        pattern: r#"(?i-u)--[a-z0-9-]{0,30}(?:password|secret|token|api-key)[a-z0-9-]{0,20}\s{1,3}["']?(?P<s>[A-Za-z0-9+/_.~=!#%^&*-]{12,})"#,
        check: Check::Generic(3.5),
        tail: r"[A-Za-z0-9+/_.~=!#%^&*-]",
    },
];

struct Rule {
    kind: &'static str,
    re: Regex,
    check: Check,
    tail: [bool; 256],
}

struct Rules {
    set: RegexSet,
    list: Vec<Rule>,
}

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|error| panic!("built-in pattern {pattern}: {error}"))
}

/// Compiled on the first line the prefilter hits, so clean output never pays for it.
static RULES: LazyLock<Rules> = LazyLock::new(|| Rules {
    set: RegexSet::new(SPECS.iter().map(|spec| spec.pattern)).expect("rule set"),
    list: SPECS
        .iter()
        .map(|spec| {
            let class = compile(&format!("(?-u)^{}$", spec.tail));
            Rule {
                kind: spec.kind,
                re: compile(spec.pattern),
                check: spec.check,
                tail: std::array::from_fn(|byte| class.is_match(&[byte as u8])),
            }
        })
        .collect(),
});

static PREFILTER: LazyLock<Regex> = LazyLock::new(|| {
    let literals: Vec<_> = TRIGGERS.iter().map(|t| regex::escape(t)).collect();
    let words: Vec<_> = KEYWORDS.iter().map(|t| regex::escape(t)).collect();
    compile(&format!(
        "(?-u:{}|(?i:{}))",
        literals.join("|"),
        words.join("|")
    ))
});

static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?i-u)example|placeholder|changeme|dummy|redacted|your[_-]|x{6,}|\*{3,}|0{8,}|1234567890|abcdefghij|^\$\{?[A-Za-z_][A-Za-z0-9_]*\}?$|^%[A-Za-z_]+%$|^(?:password|passwd|pass|secret|token|pwd)$",
    )
});
/// Words that make an identifier a name rather than a value.
static NAMED: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?i-u)key|token|secret|private|public|pass|auth|hash|digest|sign|cert|cipher|crypt|seed|salt|nonce|param|config|option|flag|type|buffer|handle|error|result|value|data|info|account|request|response|store",
    )
});
static ANSI: LazyLock<Regex> = LazyLock::new(|| compile(r"(?-u)\x1b\[[0-?]*[ -/]*[@-~]"));
static PEM_BEGIN: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?-u)-----BEGIN[ A-Z0-9_-]{0,40}PRIVATE KEY(?: BLOCK)?-----"));
static PEM_END: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?-u)-----END[ A-Z0-9_-]{0,40}PRIVATE KEY(?: BLOCK)?-----"));
/// What may follow a header that opens a block on the next lines: string syntax only.
static PEM_TAIL: LazyLock<Regex> =
    LazyLock::new(|| compile(r#"(?-u)^(?:\s|\\[nr]|\\|["'`,+;().])*$"#));
/// A body line: optional string syntax around base64 or an RFC 1421 header.
static PEM_BODY: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?-u)^(\s*["'`]?)([A-Za-z0-9+/=]{1,128}|(?:Proc-Type|DEK-Info|Comment|Version|Hash):[^"'`\\]*|)((?:\\[nr])*\\?["'`]?\s*[,+;)]*\s*\\?)\s*$"#,
    )
});

/// How many secrets a run's output held, under which rules and allowlist.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Found {
    pub rules: String,
    pub count: u64,
}

impl Found {
    pub fn new(rules: &str) -> Self {
        Self {
            rules: rules.to_owned(),
            count: 0,
        }
    }
}

enum KeySource {
    #[cfg(test)]
    Fixed(Option<[u8; 32]>),
    File(PathBuf),
}

/// Compiled allowlist and the project key that makes marker hashes unguessable.
pub struct Redactor {
    allow: Vec<Regex>,
    fingerprint: String,
    source: KeySource,
    key: OnceLock<Option<[u8; 32]>>,
}

impl Redactor {
    fn build(allow: &[String], source: KeySource) -> Result<Self> {
        let mut compiled = Vec::with_capacity(allow.len());
        let mut fingerprint = Sha256::new();
        fingerprint.update(RULES_VERSION);
        for pattern in allow {
            let re = Regex::new(pattern).map_err(|error| {
                anyhow!(".justless.toml: redact_allow: invalid pattern '{pattern}': {error}")
            })?;
            ensure!(
                !re.is_match(b""),
                ".justless.toml: redact_allow: pattern '{pattern}' matches empty text"
            );
            compiled.push(re);
            fingerprint.update(pattern);
            fingerprint.update([0]);
        }
        let digest = fingerprint.finalize();
        Ok(Self {
            allow: compiled,
            fingerprint: format!(
                "r{RULES_VERSION}-{:02x}{:02x}{:02x}{:02x}",
                digest[0], digest[1], digest[2], digest[3]
            ),
            source,
            key: OnceLock::new(),
        })
    }

    /// Identifies the rules and allowlist, so a clean archive is known to stay clean.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Redacts a whole slice, borrowing it when nothing was found.
    pub fn bytes<'a>(&self, input: &'a [u8]) -> Cow<'a, [u8]> {
        if !PREFILTER.is_match(input) {
            return Cow::Borrowed(input);
        }
        let mut engine = Engine::new(self, WINDOW, HOLD);
        let mut output = Vec::with_capacity(input.len() + 64);
        for chunk in input.chunks(READ_SIZE) {
            engine.feed(chunk, &mut output);
        }
        engine.finish(&mut output);
        if engine.count == 0 {
            Cow::Borrowed(input)
        } else {
            Cow::Owned(output)
        }
    }

    pub fn text<'a>(&self, input: &'a str) -> Cow<'a, str> {
        match self.bytes(input.as_bytes()) {
            Cow::Borrowed(_) => Cow::Borrowed(input),
            Cow::Owned(bytes) => Cow::Owned(
                String::from_utf8(bytes)
                    .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()),
            ),
        }
    }

    /// Checks shared by every rule: markers, placeholders and the allowlist.
    fn shared(&self, value: &[u8]) -> bool {
        !value.starts_with(b"[redacted:")
            && !PLACEHOLDER.is_match(value)
            && !self.allow.iter().any(|allow| allow.is_match(value))
    }

    fn accepts(&self, check: Check, value: &[u8]) -> bool {
        self.shared(value)
            && match check {
                Check::None => true,
                Check::Entropy(minimum) => entropy(value) >= minimum,
                Check::Generic(minimum) => {
                    entropy(value) >= minimum
                        && value.iter().any(u8::is_ascii_alphabetic)
                        && value.iter().any(u8::is_ascii_digit)
                        && !name_shaped(value)
                }
                Check::PemBody => base64_bytes(value) >= 32,
                Check::PemBase64 => {
                    let start = decode_base64(&value[..value.len().min(64)]);
                    start.windows(11).any(|w| w == b"PRIVATE KEY")
                }
            }
    }

    fn key(&self) -> Option<&[u8; 32]> {
        self.key
            .get_or_init(|| match &self.source {
                #[cfg(test)]
                KeySource::Fixed(key) => *key,
                KeySource::File(path) => {
                    let key = load_key(path);
                    if key.is_none() {
                        note(&KEY_NOTE, || {
                            "no readable .justless/redact.key; markers omit their hash".into()
                        });
                    }
                    key
                }
            })
            .as_ref()
    }

    /// A hasher already holding the key, or `None` when markers omit their hash.
    fn seeded(&self) -> Option<Sha256> {
        self.key().map(|key| {
            let mut hasher = Sha256::new();
            hasher.update(key);
            hasher.update([0]);
            hasher
        })
    }

    fn marker(&self, kind: &str, secret: &[u8]) -> Vec<u8> {
        finish_marker(
            kind,
            self.seeded().map(|mut hasher| {
                hasher.update(secret);
                hasher
            }),
        )
    }
}

fn finish_marker(kind: &str, hasher: Option<Sha256>) -> Vec<u8> {
    match hasher {
        Some(hasher) => {
            let digest = hasher.finalize();
            format!("[redacted:{kind}:{:02x}{:02x}]", digest[0], digest[1]).into_bytes()
        }
        None => format!("[redacted:{kind}]").into_bytes(),
    }
}

fn entropy(value: &[u8]) -> f64 {
    let mut counts = [0u32; 256];
    for &byte in value {
        counts[usize::from(byte)] += 1;
    }
    let total = value.len() as f64;
    counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let p = f64::from(count) / total;
            -p * p.log2()
        })
        .sum()
}

/// Paths; identifier chains such as `ML_KEM_512_SHARED_SECRET_LENGTH` or
/// `child1_token.child_token`, where every segment is letters followed by digits; and
/// type names such as `Ed25519PrivateKey` that say what they hold. A random value
/// contains one of those words about once in 2,000 tries.
fn name_shaped(value: &[u8]) -> bool {
    if [&b"/"[..], b"./", b"../", b"~/"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
    {
        return true;
    }
    // A reference such as `&X25519SharedSecret` is still a type.
    let name = value.strip_prefix(b"&").unwrap_or(value);
    if name.first().is_some_and(u8::is_ascii_uppercase)
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        && NAMED.is_match(name)
    {
        return true;
    }
    let separator = |byte: &u8| matches!(byte, b'_' | b'.' | b'-');
    value.iter().any(separator)
        && value.split(separator).all(|segment| {
            let letters = segment
                .iter()
                .take_while(|byte| byte.is_ascii_alphabetic())
                .count();
            segment[letters..].iter().all(u8::is_ascii_digit)
        })
}

/// Base64 bytes in a value, not counting `\n` and `\r` escapes.
fn base64_bytes(value: &[u8]) -> usize {
    let mut count = 0;
    let mut index = 0;
    while index < value.len() {
        if value[index] == b'\\' && matches!(value.get(index + 1), Some(b'n' | b'r')) {
            index += 2;
            continue;
        }
        count +=
            usize::from(value[index].is_ascii_alphanumeric() || b"+/=".contains(&value[index]));
        index += 1;
    }
    count
}

fn decode_base64(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &byte in input {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => break,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    output
}

/// The length of a line without its terminator.
fn content_len(line: &[u8]) -> usize {
    let mut end = line.len();
    if line.last() == Some(&b'\n') {
        end -= 1;
    }
    while end > 0 && line[end - 1] == b'\r' {
        end -= 1;
    }
    end
}

/// A private key header with nothing but string syntax after it, so the key follows
/// on the next lines.
fn pem_opens(text: &[u8]) -> bool {
    let Some(begin) = PEM_BEGIN.find_iter(text).last() else {
        return false;
    };
    let tail = &text[begin.end()..];
    !PEM_END.is_match(tail) && PEM_TAIL.is_match(tail)
}

struct Hit {
    start: usize,
    end: usize,
    secret: Range<usize>,
    rule: usize,
    ok: bool,
}

/// A secret that reached the end of a window: its bytes in the next window are part
/// of the same marker.
struct Continuation {
    kind: &'static str,
    tail: &'static [bool; 256],
    hasher: Option<Sha256>,
    last: u8,
}

#[derive(Default)]
struct Pem {
    /// Each buffered line, its material or header range, and whether it is key material.
    lines: Vec<(Vec<u8>, Range<usize>, bool)>,
    material: Vec<u8>,
}

/// Push-based redaction state for one stream.
struct Engine<'r> {
    redactor: &'r Redactor,
    window: usize,
    hold: usize,
    /// Bytes of the current line not yet emitted. When `context` is 1, the first byte
    /// was emitted and is kept so `\b` sees the byte before a window.
    line: Vec<u8>,
    context: usize,
    /// Part of the current line was emitted by an earlier window.
    partial: bool,
    continuation: Option<Continuation>,
    pem: Option<Pem>,
    count: u64,
}

impl<'r> Engine<'r> {
    fn new(redactor: &'r Redactor, window: usize, hold: usize) -> Self {
        LazyLock::force(&PREFILTER);
        debug_assert!(hold < window);
        Self {
            redactor,
            window,
            hold,
            line: Vec::new(),
            context: 0,
            partial: false,
            continuation: None,
            pem: None,
            count: 0,
        }
    }

    fn feed(&mut self, mut data: &[u8], out: &mut Vec<u8>) {
        if !self.line.is_empty() || self.continuation.is_some() {
            match data.iter().position(|&byte| byte == b'\n') {
                None => return self.extend_line(data, out),
                Some(newline) => {
                    self.extend_line(&data[..=newline], out);
                    self.end_line(out);
                    data = &data[newline + 1..];
                }
            }
        }
        match data.iter().rposition(|&byte| byte == b'\n') {
            None => self.extend_line(data, out),
            Some(newline) => {
                self.block(&data[..=newline], out);
                self.extend_line(&data[newline + 1..], out);
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<u8>) {
        if self.continuation.is_some() {
            self.close_continuation(out);
        }
        if !self.line.is_empty() {
            self.end_line(out);
        }
        self.flush_pem(out);
    }

    /// Complete lines. Bytes before the line holding the first trigger pass untouched.
    fn block(&mut self, mut data: &[u8], out: &mut Vec<u8>) {
        while !data.is_empty() {
            if self.pem.is_none() {
                let Some(hit) = PREFILTER.find(data) else {
                    out.extend_from_slice(data);
                    return;
                };
                let start = data[..hit.start()]
                    .iter()
                    .rposition(|&byte| byte == b'\n')
                    .map_or(0, |index| index + 1);
                out.extend_from_slice(&data[..start]);
                data = &data[start..];
                let end = line_end(data);
                self.candidate(&data[..end], out);
                data = &data[end..];
            } else {
                let end = line_end(data);
                self.process_line(&data[..end], out);
                data = &data[end..];
            }
        }
    }

    /// Appends bytes of an unfinished line, redacting full windows as they fill.
    fn extend_line(&mut self, mut data: &[u8], out: &mut Vec<u8>) {
        loop {
            if let Some(continuation) = self.continuation.as_mut() {
                let run = data
                    .iter()
                    .take_while(|&&byte| continuation.tail[usize::from(byte)])
                    .count();
                if let Some(hasher) = continuation.hasher.as_mut() {
                    hasher.update(&data[..run]);
                }
                if run > 0 {
                    continuation.last = data[run - 1];
                }
                data = &data[run..];
                if data.is_empty() {
                    return;
                }
                self.close_continuation(out);
            }
            let pending = self.line.len() - self.context;
            let (now, later) = data.split_at(data.len().min(self.window - pending));
            // Grow by doubling but never past one window, so memory stays bounded.
            let needed = self.line.len() + now.len();
            if needed > self.line.capacity() {
                let target = (needed * 2).min(self.context + self.window).max(needed);
                self.line.reserve_exact(target - self.line.len());
            }
            self.line.extend_from_slice(now);
            if self.line.len() - self.context < self.window {
                return;
            }
            data = later;
            self.window_line(out);
        }
    }

    fn close_continuation(&mut self, out: &mut Vec<u8>) {
        let continuation = self.continuation.take().expect("continuation");
        out.extend_from_slice(&finish_marker(continuation.kind, continuation.hasher));
        self.line.clear();
        self.line.push(continuation.last);
        self.context = 1;
    }

    /// Redacts a full window of a long line and keeps `hold` bytes for the next one.
    fn window_line(&mut self, out: &mut Vec<u8>) {
        // A line this long is never part of a key block.
        self.flush_pem(out);
        let mut line = std::mem::take(&mut self.line);
        let (emitted, continuation) = self.redact(&line, self.context, Some(self.hold), out);
        self.partial = true;
        if let Some(continuation) = continuation {
            self.continuation = Some(continuation);
            line.clear();
            self.context = 0;
        } else {
            line.drain(..emitted - 1);
            self.context = 1;
        }
        self.line = line;
    }

    /// The current line is complete, with its terminator unless the stream ended.
    fn end_line(&mut self, out: &mut Vec<u8>) {
        let line = std::mem::take(&mut self.line);
        if self.partial {
            let end = content_len(&line).max(self.context);
            let start = out.len();
            self.redact(&line[..end], self.context, None, out);
            if pem_opens(&out[start..]) {
                self.pem = Some(Pem::default());
            }
            out.extend_from_slice(&line[end..]);
        } else {
            self.process_line(&line, out);
        }
        self.partial = false;
        self.context = 0;
        self.line = line;
        self.line.clear();
    }

    fn process_line(&mut self, line: &[u8], out: &mut Vec<u8>) {
        if self.pem.is_some() && self.pem_line(line, out) {
            return;
        }
        if PREFILTER.is_match(line) {
            self.candidate(line, out);
        } else {
            out.extend_from_slice(line);
        }
    }

    /// A whole line the prefilter hit.
    fn candidate(&mut self, line: &[u8], out: &mut Vec<u8>) {
        let (body, terminator) = line.split_at(content_len(line));
        let start = out.len();
        if body.contains(&0x1b) {
            // Colors can split a keyword from its value. Match without them, and keep
            // the original bytes unless something was redacted.
            let stripped = ANSI.replace_all(body, &b""[..]);
            let before = self.count;
            let mut redacted = Vec::with_capacity(stripped.len());
            self.redact(&stripped, 0, None, &mut redacted);
            out.extend_from_slice(if self.count > before { &redacted } else { body });
        } else {
            self.redact(body, 0, None, out);
        }
        if pem_opens(&out[start..]) {
            self.pem = Some(Pem::default());
        }
        out.extend_from_slice(terminator);
    }

    /// Buffers a line inside a key block. Returns false when the line ends the block
    /// without belonging to it.
    fn pem_line(&mut self, line: &[u8], out: &mut Vec<u8>) -> bool {
        let body = &line[..content_len(line)];
        if PEM_END.is_match(body) {
            let pem = self.pem.as_mut().expect("key block");
            pem.lines.push((line.to_vec(), 0..0, false));
            self.flush_pem(out);
            return true;
        }
        let pem = self.pem.as_mut().expect("key block");
        if pem.lines.len() < PEM_LINES
            && let Some(captures) = PEM_BODY.captures(body)
        {
            let range = captures.get(2).expect("material group").range();
            let value = &body[range.clone()];
            let material = !value.is_empty() && !value.contains(&b':');
            if material {
                if pem.material.is_empty() && value.len() < 16 {
                    self.flush_pem(out);
                    return false;
                }
                pem.material.extend_from_slice(value);
            }
            pem.lines.push((line.to_vec(), range, material));
            return true;
        }
        self.flush_pem(out);
        false
    }

    /// Emits a buffered key block: the first material line becomes one marker, the
    /// other material and header values are removed, and every line break stays.
    fn flush_pem(&mut self, out: &mut Vec<u8>) {
        let Some(pem) = self.pem.take() else {
            return;
        };
        if pem.material.is_empty() || !self.redactor.shared(&pem.material) {
            for (line, ..) in pem.lines {
                out.extend_from_slice(&line);
            }
            return;
        }
        self.count += 1;
        let mut marker = Some(self.redactor.marker("private-key", &pem.material));
        for (line, range, material) in pem.lines {
            out.extend_from_slice(&line[..range.start]);
            if material && let Some(marker) = marker.take() {
                out.extend_from_slice(&marker);
            }
            out.extend_from_slice(&line[range.end..]);
        }
    }

    /// Redacts `hay[from..]` into `out`. With `hold`, the last `hold` bytes are kept
    /// back for the next window unless a match that starts earlier runs into them.
    /// Returns where emission stopped and, when a secret reaches the end of the
    /// window, its continuation.
    fn redact(
        &mut self,
        hay: &[u8],
        from: usize,
        hold: Option<usize>,
        out: &mut Vec<u8>,
    ) -> (usize, Option<Continuation>) {
        let end = hay.len();
        let hits = if from < end && PREFILTER.is_match_at(hay, from) {
            hits(self.redactor, hay, from)
        } else {
            Vec::new()
        };
        let mut emit = hold.map_or(end, |hold| end.saturating_sub(hold).max(from));
        if hold.is_some() {
            // A match that straddles the cut moves it, so nothing is applied twice.
            while let Some(next) = hits
                .iter()
                .filter(|hit| hit.start < emit && hit.end > emit && hit.end < end)
                .map(|hit| hit.end)
                .max()
            {
                emit = next;
            }
        }
        let mut spans = resolve(&hits, emit);
        let mut continuation = None;
        if hold.is_some() && spans.iter().any(|(secret, _)| secret.end == end) {
            // A value longer than the hold runs on into the next window.
            emit = end;
            spans = resolve(&hits, end);
            if let Some((secret, rule)) = spans.iter().find(|(secret, _)| secret.end == end) {
                let rule = &RULES.list[*rule];
                continuation = Some(Continuation {
                    kind: rule.kind,
                    tail: &rule.tail,
                    hasher: self.redactor.seeded().map(|mut hasher| {
                        hasher.update(&hay[secret.clone()]);
                        hasher
                    }),
                    last: hay[end - 1],
                });
            }
        }
        let mut at = from;
        for (secret, rule) in &spans {
            out.extend_from_slice(&hay[at..secret.start]);
            if secret.end != end || continuation.is_none() {
                let kind = RULES.list[*rule].kind;
                out.extend_from_slice(&self.redactor.marker(kind, &hay[secret.clone()]));
            }
            self.count += 1;
            at = secret.end;
        }
        out.extend_from_slice(&hay[at..emit]);
        (emit, continuation)
    }
}

fn line_end(data: &[u8]) -> usize {
    data.iter()
        .position(|&byte| byte == b'\n')
        .map_or(data.len(), |index| index + 1)
}

/// Every rule match from `from`, accepted or not, in rule order.
fn hits(redactor: &Redactor, hay: &[u8], from: usize) -> Vec<Hit> {
    let rules = &*RULES;
    let mut hits = Vec::new();
    for index in rules.set.matches_at(hay, from).iter() {
        let rule = &rules.list[index];
        let mut at = from;
        while at <= hay.len()
            && let Some(captures) = rule.re.captures_at(hay, at)
        {
            let whole = captures.get(0).expect("match");
            let secret = captures.name("s").expect("secret group");
            hits.push(Hit {
                start: whole.start(),
                end: whole.end(),
                secret: secret.range(),
                rule: index,
                ok: redactor.accepts(rule.check, secret.as_bytes()),
            });
            at = whole.end().max(whole.start() + 1);
        }
    }
    hits
}

/// Accepted secrets of matches that start before `limit`. On overlap the earlier
/// rule wins.
fn resolve(hits: &[Hit], limit: usize) -> Vec<(Range<usize>, usize)> {
    let mut taken: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    for hit in hits.iter().filter(|hit| hit.ok && hit.start < limit) {
        let overlaps = taken
            .range(..hit.secret.end)
            .next_back()
            .is_some_and(|(_, (end, _))| *end > hit.secret.start);
        if !overlaps {
            taken.insert(hit.secret.start, (hit.secret.end, hit.rule));
        }
    }
    taken
        .into_iter()
        .map(|(start, (end, rule))| (start..end, rule))
        .collect()
}

/// A buffered reader that redacts secrets on the way through. Without a redactor it
/// passes bytes through unchanged.
pub struct Reader<'r, R> {
    inner: R,
    engine: Option<Engine<'r>>,
    input: Vec<u8>,
    output: Vec<u8>,
    position: usize,
    length: usize,
    done: bool,
}

impl<'r, R: Read> Reader<'r, R> {
    pub fn new(inner: R, redactor: Option<&'r Redactor>) -> Self {
        Self {
            inner,
            engine: redactor.map(|redactor| Engine::new(redactor, WINDOW, HOLD)),
            input: vec![0; READ_SIZE],
            output: Vec::new(),
            position: 0,
            length: 0,
            done: false,
        }
    }

    #[cfg(test)]
    fn with_window(inner: R, redactor: &'r Redactor, window: usize, hold: usize) -> Self {
        let mut reader = Self::new(inner, Some(redactor));
        reader.engine = Some(Engine::new(redactor, window, hold));
        reader
    }

    /// Secrets redacted so far, under this redactor's rules.
    pub fn found(&self) -> Found {
        self.engine
            .as_ref()
            .map_or_else(Found::default, |engine| Found {
                rules: engine.redactor.fingerprint.clone(),
                count: engine.count,
            })
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: Read> BufRead for Reader<'_, R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        while self.position == self.length && !self.done {
            let read = loop {
                match self.inner.read(&mut self.input) {
                    Ok(read) => break read,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            };
            self.position = 0;
            match self.engine.as_mut() {
                None => {
                    self.length = read;
                    self.done = read == 0;
                }
                Some(engine) => {
                    self.output.clear();
                    if read == 0 {
                        engine.finish(&mut self.output);
                        self.done = true;
                    } else {
                        engine.feed(&self.input[..read], &mut self.output);
                    }
                    self.length = self.output.len();
                }
            }
        }
        let buffer = if self.engine.is_some() {
            &self.output
        } else {
            &self.input
        };
        Ok(&buffer[self.position..self.length])
    }

    fn consume(&mut self, amount: usize) {
        self.position = (self.position + amount).min(self.length);
    }
}

impl<R: Read> Read for Reader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let count = available.len().min(buffer.len());
        buffer[..count].copy_from_slice(&available[..count]);
        self.consume(count);
        Ok(count)
    }
}

static ACTIVE: OnceLock<Option<Redactor>> = OnceLock::new();
static BYPASS: AtomicBool = AtomicBool::new(false);
static CONFIG_NOTE: AtomicBool = AtomicBool::new(false);
static KEY_NOTE: AtomicBool = AtomicBool::new(false);
/// Whether each archive is known to hold no secret under the current rules.
static CLEAN: LazyLock<Mutex<HashMap<PathBuf, bool>>> = LazyLock::new(Default::default);

fn note(flag: &AtomicBool, message: impl FnOnce() -> String) {
    if !flag.swap(true, Ordering::Relaxed) {
        eprintln!("justless: {}", message());
    }
}

/// The redactor for a new capture. A bad configuration stops the run before the
/// command starts.
pub fn for_run(workspace: &Workspace) -> Result<Option<&'static Redactor>> {
    if let Some(active) = ACTIVE.get() {
        return Ok(active.as_ref());
    }
    let settings = workspace.settings()?;
    let redactor = if settings.redact {
        Some(Redactor::build(
            &settings.redact_allow,
            KeySource::File(workspace.state.join("redact.key")),
        )?)
    } else {
        None
    };
    Ok(ACTIVE.get_or_init(|| redactor).as_ref())
}

/// The redactor for views of earlier runs. It never fails: a configuration that
/// cannot be read falls back to the built-in rules, so redaction is never lost.
pub fn for_workspace(workspace: &Workspace) -> Option<&'static Redactor> {
    lenient(&workspace.root, &workspace.state)
}

/// The redactor for replaying one archived run, or `None` for an exact copy: after
/// `bypass`, with redaction off, or when the run was recorded clean under the
/// current rules.
pub fn for_archive(directory: &Path) -> Option<&'static Redactor> {
    if BYPASS.load(Ordering::Relaxed) {
        return None;
    }
    // Archives live at ROOT/.justless/runs/RUN.
    let state = directory
        .parent()
        .and_then(Path::parent)
        .unwrap_or(directory);
    let root = state.parent().unwrap_or(state);
    let redactor = lenient(root, state)?;
    let mut clean = CLEAN.lock().unwrap_or_else(|poison| poison.into_inner());
    let clean = *clean
        .entry(directory.to_path_buf())
        .or_insert_with(|| recorded_clean(directory, redactor.fingerprint()));
    (!clean).then_some(redactor)
}

/// Replay originals for the rest of this process. Only `--unredacted` calls it.
pub fn bypass() {
    BYPASS.store(true, Ordering::Relaxed);
}

pub fn unredacted() -> Arg<'static> {
    Arg::new("unredacted")
        .long("unredacted")
        .help("Replay the archived bytes without redacting secrets")
}

fn lenient(root: &Path, state: &Path) -> Option<&'static Redactor> {
    ACTIVE
        .get_or_init(|| {
            let workspace = Workspace {
                root: root.to_path_buf(),
                cwd: root.to_path_buf(),
                state: state.to_path_buf(),
            };
            let key = || KeySource::File(state.join("redact.key"));
            let configured = workspace.settings().and_then(|settings| {
                if settings.redact {
                    Redactor::build(&settings.redact_allow, key()).map(Some)
                } else {
                    Ok(None)
                }
            });
            match configured {
                Ok(redactor) => redactor,
                Err(error) => {
                    note(&CONFIG_NOTE, || {
                        format!("{error:#}; redacting with built-in rules only")
                    });
                    Some(Redactor::build(&[], key()).expect("built-in rules"))
                }
            }
        })
        .as_ref()
}

fn recorded_clean(directory: &Path, fingerprint: &str) -> bool {
    #[derive(Deserialize)]
    struct Record {
        #[serde(default)]
        redaction: Option<Found>,
    }
    File::open(directory.join("record.json"))
        .ok()
        .and_then(|file| serde_json::from_reader::<_, Record>(BufReader::new(file)).ok())
        .and_then(|record| record.redaction)
        .is_some_and(|found| found.count == 0 && found.rules == fingerprint)
}

fn load_key(path: &Path) -> Option<[u8; 32]> {
    match fs::read(path) {
        Ok(bytes) => bytes.try_into().ok(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_key(path),
        Err(_) => None,
    }
}

/// Writes a new random key without replacing one a concurrent process created.
fn create_key(path: &Path) -> Option<[u8; 32]> {
    let directory = path.parent()?;
    if !directory.is_dir() {
        return None;
    }
    let state = RandomState::new();
    let mut seed = Sha256::new();
    for round in 0..4u64 {
        let mut hasher = state.build_hasher();
        hasher.write_u64(round);
        seed.update(hasher.finish().to_le_bytes());
    }
    seed.update(std::process::id().to_le_bytes());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    seed.update(now.as_nanos().to_le_bytes());
    let key: [u8; 32] = seed.finalize().into();
    let mut temporary = tempfile::NamedTempFile::new_in(directory).ok()?;
    temporary.write_all(&key).ok()?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Some(key),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            fs::read(path).ok()?.try_into().ok()
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];

    fn redactor() -> Redactor {
        Redactor::build(&[], KeySource::Fixed(Some(KEY))).unwrap()
    }

    fn redact(redactor: &Redactor, input: &[u8]) -> (Vec<u8>, u64) {
        let mut engine = Engine::new(redactor, WINDOW, HOLD);
        let mut output = Vec::new();
        for chunk in input.chunks(READ_SIZE) {
            engine.feed(chunk, &mut output);
        }
        engine.finish(&mut output);
        (output, engine.count)
    }

    fn text(input: &str) -> String {
        String::from_utf8(redact(&redactor(), input.as_bytes()).0).unwrap()
    }

    /// Deterministic base62 text, so no literal token lives in the repository.
    fn base62(seed: &str, length: usize) -> String {
        const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        let mut output = String::new();
        let mut block = Sha256::digest(seed.as_bytes());
        while output.len() < length {
            for byte in block {
                if output.len() < length {
                    output.push(ALPHABET[usize::from(byte) % 62] as char);
                }
            }
            block = Sha256::digest(block);
        }
        output
    }

    fn upper(seed: &str, length: usize) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        Sha256::digest(seed.as_bytes())
            .iter()
            .take(length)
            .map(|byte| ALPHABET[usize::from(*byte) % 32] as char)
            .collect()
    }

    fn letters(seed: &str, length: usize) -> String {
        base62(seed, length * 2)
            .chars()
            .filter(char::is_ascii_alphabetic)
            .chain(std::iter::repeat('q'))
            .take(length)
            .collect()
    }

    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut output = String::new();
        for chunk in bytes.chunks(3) {
            let value = chunk
                .iter()
                .enumerate()
                .fold(0u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));
            for i in 0..4 {
                if i <= chunk.len() {
                    output.push(ALPHABET[(value >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    output.push('=');
                }
            }
        }
        output
    }

    /// A PEM body of `lines` lines of 64 base64 characters.
    fn pem_body(seed: &str, lines: usize) -> Vec<String> {
        let body = base64(base62(seed, lines * 48).as_bytes());
        body.as_bytes()
            .chunks(64)
            .take(lines)
            .map(|chunk| String::from_utf8(chunk.to_vec()).unwrap())
            .collect()
    }

    fn header(kind: &str) -> String {
        format!("{}BEGIN {kind}PRIVATE KEY-----", "-----")
    }

    fn footer(kind: &str) -> String {
        format!("{}END {kind}PRIVATE KEY-----", "-----")
    }

    fn jwt(seed: &str) -> String {
        let head = base64(br#"{"alg":"HS256","typ":"JWT"}"#).replace('=', "");
        let claims =
            base64(format!(r#"{{"sub":"{}"}}"#, base62(seed, 12)).as_bytes()).replace('=', "");
        format!("{head}.{claims}.{}", base62(&format!("{seed}s"), 43))
    }

    /// One positive sample per kind, built at runtime: (kind, secret, line).
    fn samples() -> Vec<(&'static str, String, String)> {
        let github = format!("{}{}", "gh", "p_") + &base62("github", 36);
        let aws = format!("{}{}", "AK", "IA") + &upper("aws", 16);
        let gitlab = format!("{}{}", "glp", "at-") + &base62("gitlab", 20);
        let slack = format!("{}{}", "xo", "xb-") + &base62("slack", 24);
        let webhook = base62("webhook", 44);
        let stripe = format!("{}{}", "sk_l", "ive_") + &base62("stripe", 24);
        let anthropic = format!("{}{}", "sk-a", "nt-api03-") + &base62("anthropic", 40);
        let openai = format!("{}{}", "sk-p", "roj-") + &base62("openai", 40);
        let google = format!("{}{}", "AI", "za") + &base62("google", 35);
        let npm = format!("{}{}", "np", "m_") + &base62("npm", 36);
        let pypi = format!("{}{}", "pypi-", "AgEIcHlwaS5vcmc") + &base62("pypi", 60);
        let huggingface = format!("{}{}", "h", "f_") + &letters("hf", 34);
        let token = jwt("jwt");
        let password = base62("url", 10);
        let bearer = base62("bearer", 32);
        let generic = base62("generic", 24);
        let scheme = "postgres";
        vec![
            (
                "github-token",
                github.clone(),
                format!("GITHUB_TOKEN={github}"),
            ),
            (
                "aws-access-key",
                aws.clone(),
                format!("  \"AccessKeyId\": \"{aws}\","),
            ),
            ("gitlab-token", gitlab.clone(), format!("token: {gitlab}")),
            (
                "slack-token",
                slack.clone(),
                format!("export SLACK_BOT={slack}"),
            ),
            (
                "slack-webhook",
                webhook.clone(),
                format!("url = https://hooks.slack.com/services/{webhook}"),
            ),
            ("stripe-key", stripe.clone(), format!("STRIPE={stripe}")),
            (
                "anthropic-key",
                anthropic.clone(),
                format!("key {anthropic} loaded"),
            ),
            (
                "openai-key",
                openai.clone(),
                format!("{{\"key\": \"{openai}\"}}"),
            ),
            (
                "google-api-key",
                google.clone(),
                format!("maps?key={google}&q=1"),
            ),
            (
                "npm-token",
                npm.clone(),
                format!("//registry.npmjs.org/:_authToken={npm}"),
            ),
            ("pypi-token", pypi.clone(), format!("password = {pypi}")),
            (
                "huggingface-token",
                huggingface.clone(),
                format!("HF {huggingface}"),
            ),
            (
                "jwt",
                token.clone(),
                format!("cookie: session={token}; Path=/"),
            ),
            (
                "url-password",
                password.clone(),
                format!("{scheme}://app:{password}@db.internal:5432/app"),
            ),
            (
                "authorization",
                bearer.clone(),
                format!("> {}: Bearer {bearer}", "Authorization"),
            ),
            ("secret", generic.clone(), format!("API_KEY=\"{generic}\"")),
        ]
    }

    #[test]
    fn each_kind_replaces_its_whole_secret_and_nothing_else() {
        let redactor = redactor();
        for (kind, secret, line) in samples() {
            let (output, count) = redact(&redactor, format!("{line}\n").as_bytes());
            let output = String::from_utf8(output).unwrap();
            assert_eq!(count, 1, "{kind}: {output}");
            let marker = String::from_utf8(redactor.marker(kind, secret.as_bytes())).unwrap();
            assert_eq!(
                output,
                format!("{}\n", line.replace(&secret, &marker)),
                "{kind}"
            );
            assert!(!output.contains(&secret[secret.len() - 8..]), "{kind}");
        }
    }

    #[test]
    fn secrets_are_found_in_common_contexts() {
        let github = format!("{}{}", "gh", "o_") + &base62("contexts", 36);
        let generic = base62("value", 20);
        let scheme = "https";
        let cases = [
            format!("[remote \"origin\"]\n\turl = {scheme}://bot:{github}@github.com/o/r.git"),
            format!("data:\n  api_key: {generic}\n"),
            format!("\x1b[32mtoken\x1b[0m={generic}"),
            format!(r#"{{"rendered":"let token = \"{generic}\";"}}"#),
            format!(r#"{{"headers": {{"Authorization": "Bearer {generic}"}}}}"#),
            format!(r#"{{"Authorization\":\"Bearer {generic}\""}}"#),
            format!("mysql --password {generic} -h db"),
            format!("{}{}", "AWS_SECRET_ACCESS_KEY=", base62("aws-secret", 40)),
        ];
        let redactor = redactor();
        for case in cases {
            let (output, count) = redact(&redactor, case.as_bytes());
            let output = String::from_utf8(output).unwrap();
            assert_eq!(count, 1, "{case:?} -> {output}");
            assert!(
                !output.contains(&generic) && !output.contains(&github),
                "{output}"
            );
            assert!(output.contains("[redacted:"), "{output}");
        }
    }

    #[test]
    fn ordinary_code_and_logs_stay_byte_identical() {
        let placeholder = format!("{}{}", "gh", "p_") + &"x".repeat(36);
        let scheme = "postgres";
        let negatives = [
            "token = generate_token(rng)".to_owned(),
            r#"let password = std::env::var("DB_PASSWORD")?;"#.to_owned(),
            "secret = hashlib.sha256(x)".to_owned(),
            format!("aws_access_key_id = {}{}", "AKIAIOSFODNN7", "EXAMPLE"),
            "export GITHUB_TOKEN=$GITHUB_TOKEN".to_owned(),
            "curl -H \"Authorization: Bearer ${TOKEN}\" https://api".to_owned(),
            format!("{scheme}://user:password@localhost/db"),
            format!("{scheme}://user:****@github.com/x"),
            "max_tokens = 4096".to_owned(),
            r#""token_count": 267459,"#.to_owned(),
            "error: unexpected token `}` in expression".to_owned(),
            r#"tokenizers = "0.15.2""#.to_owned(),
            r#""js-tokens": "^4.0.0","#.to_owned(),
            r#"password: "changeme123456""#.to_owned(),
            format!("GITHUB_TOKEN={placeholder}"),
            "Author: Jane Doe <jane@example.org>".to_owned(),
            "credential.helper=osxkeychain".to_owned(),
            "PWD=/Users/x/dev/justless".to_owned(),
            "apiVersion: rbac.authorization.k8s.io/v1".to_owned(),
            "kube-apiserver --authorization-mode=Node,RBAC".to_owned(),
            "shared_secret_size: ML_KEM_512_SHARED_SECRET_LENGTH".to_owned(),
            "let t = child1_token.child_token;".to_owned(),
            r#""secret_name": "app-config-123456""#.to_owned(),
            format!("The `{}` header opens a key.", header("")),
            format!("assert!(pem.starts_with(\"{}\"));", header("")),
            "-----BEGIN CERTIFICATE-----".to_owned(),
            base64(base62("certificate", 48).as_bytes()),
            "-----END CERTIFICATE-----".to_owned(),
            r#"checksum = "8f9e1f2b6a1c5ed7c3a1e4f0b2d6c8e9a7b5d3f1e2c4a6b8d0f1e3c5a7b9d1f3""#
                .to_owned(),
            r#""integrity": "sha512-abcDEF0123456789+/abcDEF0123456789==","#.to_owned(),
            "feat(secret-redaction): keep secrets out of previews".to_owned(),
            "fix(token): count tokens in streaming responses".to_owned(),
            "    Finished `test` profile [unoptimized + debuginfo] target(s)".to_owned(),
            "warning: unused variable: `token`".to_owned(),
            "POSTGRES_PASSWORD=postgres".to_owned(),
            "password=hunter2".to_owned(),
            // Type annotations seen in crate sources.
            "    let der_private_key_buffer: EcPrivateKeyRfc5915Der = key.as_der()?;".to_owned(),
            "pub const PrivateKeyAttestMask: X509CertificateTemplatePrivateKeyFlag = X(1);"
                .to_owned(),
            "    pub PresentHistoryToken: D3DKMT_PRESENTHISTORYTOKEN,".to_owned(),
            "    private_key: Ed25519PrivateKey".to_owned(),
            "fn derive(secret: &X25519SharedSecret, params: Argon2Params)".to_owned(),
            "    let raw_private_key: Curve25519SeedBin<'_> = private_key".to_owned(),
        ];
        let redactor = redactor();
        for line in negatives {
            let input = format!("{line}\n");
            let (output, count) = redact(&redactor, input.as_bytes());
            assert_eq!(count, 0, "{line}");
            assert_eq!(output, input.as_bytes(), "{line}");
            assert!(matches!(redactor.bytes(input.as_bytes()), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn redaction_is_idempotent() {
        let redactor = redactor();
        let mut lines: Vec<String> = samples().into_iter().map(|(_, _, line)| line).collect();
        lines.push(private_key_block("idempotent", "RSA ", 4).join("\n"));
        lines.push(encrypted_block("idem-enc").join("\n"));
        for line in lines {
            let once = redactor.bytes(line.as_bytes()).into_owned();
            assert_ne!(once, line.as_bytes());
            assert_eq!(redactor.bytes(&once), Cow::Borrowed(&once[..]), "{line}");
        }
        let without = Redactor::build(&[], KeySource::Fixed(None)).unwrap();
        let scheme = "https";
        let line = format!("{scheme}://u:{}@h", base62("nohash", 12));
        let once = without.bytes(line.as_bytes()).into_owned();
        assert_eq!(
            once,
            format!("{scheme}://u:[redacted:url-password]@h").as_bytes()
        );
        assert_eq!(without.bytes(&once), Cow::Borrowed(&once[..]));
    }

    #[test]
    fn markers_are_keyed_and_stable() {
        let redactor = redactor();
        let secret = format!("{}{}", "gh", "p_") + &base62("stable", 36);
        let input = format!("a={secret}\nb={secret}\n");
        let output = String::from_utf8(redact(&redactor, input.as_bytes()).0).unwrap();
        let markers: Vec<_> = output.lines().map(|l| &l[2..]).collect();
        assert_eq!(markers[0], markers[1]);
        let again = redact(&super::tests::redactor(), input.as_bytes()).0;
        assert_eq!(again, output.as_bytes());
        let other = Redactor::build(&[], KeySource::Fixed(Some([8; 32]))).unwrap();
        assert_ne!(redact(&other, input.as_bytes()).0, output.as_bytes());
        // SHA-256([7; 32] || 0x00 || "abc") starts 0fa0, computed independently.
        assert_eq!(redactor.marker("secret", b"abc"), b"[redacted:secret:0fa0]");
        let none = Redactor::build(&[], KeySource::Fixed(None)).unwrap();
        assert_eq!(none.marker("jwt", b"abc"), b"[redacted:jwt]");
    }

    #[test]
    fn terminators_bytes_and_colors_are_preserved() {
        let redactor = redactor();
        let secret = base62("terminators", 20);
        let input = format!("a\r\ntoken={secret}\r\n\nlast token={secret}");
        let (output, count) = redact(&redactor, input.as_bytes());
        assert_eq!(count, 2);
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches('\n').count(), 3);
        assert_eq!(output.matches("\r\n").count(), 2);
        assert!(!output.ends_with('\n') && !output.contains(&secret));

        let mut invalid = b"\xff\xfe password=".to_vec();
        invalid.extend_from_slice(secret.as_bytes());
        invalid.extend_from_slice(b" \xc3\x28\n");
        let (output, count) = redact(&redactor, &invalid);
        assert_eq!(count, 1);
        assert!(output.starts_with(b"\xff\xfe password=[redacted:secret:"));
        assert!(output.ends_with(b"] \xc3\x28\n"));

        let colored = b"\x1b[1mtoken\x1b[0m count: 3\n";
        assert_eq!(redact(&redactor, colored).0, colored);
        let colored = format!("\x1b[32mtoken\x1b[0m={secret}\n");
        let output = String::from_utf8(redact(&redactor, colored.as_bytes()).0).unwrap();
        assert!(output.starts_with("token=[redacted:secret:"), "{output}");

        let mut binary = Vec::new();
        let mut block = Sha256::digest(b"binary");
        for _ in 0..4096 {
            binary.extend_from_slice(&block);
            block = Sha256::digest(block);
        }
        let (output, count) = redact(&redactor, &binary);
        assert_eq!((output, count), (binary.clone(), 0));
    }

    fn private_key_block(seed: &str, kind: &str, lines: usize) -> Vec<String> {
        let mut block = vec![header(kind)];
        block.extend(pem_body(seed, lines));
        block.push(footer(kind));
        block
    }

    fn encrypted_block(seed: &str) -> Vec<String> {
        let mut block = vec![
            header("RSA "),
            "Proc-Type: 4,ENCRYPTED".to_owned(),
            "DEK-Info: AES-128-CBC,0123456789ABCDEF0123456789ABCDEF".to_owned(),
            String::new(),
        ];
        block.extend(pem_body(seed, 3));
        block.push(footer("RSA "));
        block
    }

    #[test]
    fn private_key_blocks_keep_their_lines() {
        let redactor = redactor();
        for kind in ["RSA ", "EC ", "OPENSSH ", "ENCRYPTED ", ""] {
            let block = private_key_block(kind, kind, 5);
            let input = format!("before\n{}\nafter\n", block.join("\n"));
            let (output, count) = redact(&redactor, input.as_bytes());
            assert_eq!(count, 1, "{kind}");
            let output = String::from_utf8(output).unwrap();
            let lines: Vec<_> = output.lines().collect();
            assert_eq!(lines.len(), 9);
            assert_eq!(lines[1], header(kind));
            assert!(lines[2].starts_with("[redacted:private-key:"), "{output}");
            assert!(lines[3..6].iter().all(|line| line.is_empty()), "{output}");
            assert_eq!(lines[7], footer(kind));
            assert_eq!(lines[8], "after");
        }
        let (output, count) = redact(&redactor, encrypted_block("enc").join("\n").as_bytes());
        assert_eq!(count, 1);
        let output = String::from_utf8(output).unwrap();
        let lines: Vec<_> = output.lines().collect();
        assert_eq!(lines.len(), 8);
        assert!(lines[1..4].iter().all(|line| line.is_empty()), "{output}");
        assert!(lines[4].starts_with("[redacted:private-key:"), "{output}");
    }

    #[test]
    fn key_blocks_hash_every_material_line() {
        // Unencrypted OpenSSH keys of one type share their first body line.
        let redactor = redactor();
        let first = pem_body("shared", 1).remove(0);
        let block = |seed: &str| {
            let mut lines = vec![header("OPENSSH "), first.clone()];
            lines.extend(pem_body(seed, 3));
            lines.push(footer("OPENSSH "));
            lines.join("\n")
        };
        let one = redact(&redactor, block("one").as_bytes()).0;
        let two = redact(&redactor, block("two").as_bytes()).0;
        assert_ne!(one, two);
    }

    #[test]
    fn keys_inside_string_literals_and_json() {
        let redactor = redactor();
        let body = pem_body("literal", 3);
        let literal = format!(
            "const KEY: &str = \"{}\\n\\\n{}\\n\\\n{}\\n\\\n{}\\n\\\n{}\\n\";\n",
            header(""),
            body[0],
            body[1],
            body[2],
            footer("")
        );
        let (output, count) = redact(&redactor, literal.as_bytes());
        let output = String::from_utf8(output).unwrap();
        assert_eq!(count, 1, "{output}");
        assert_eq!(output.lines().count(), 5);
        let lines: Vec<_> = output.lines().collect();
        assert!(lines[1].starts_with("[redacted:private-key:") && lines[1].ends_with("\\n\\"));
        assert_eq!(lines[2], "\\n\\");

        let json = format!(
            r#"{{"private_key": "{}\n{}\n{}\n{}\n"}}"#,
            header(""),
            body[0],
            body[1],
            footer("")
        );
        let output = String::from_utf8(redact(&redactor, json.as_bytes()).0).unwrap();
        assert!(!output.contains(&body[0][..20]) && !output.contains(&body[1][..20]));
        assert!(
            serde_json::from_str::<serde_json::Value>(&output).is_ok(),
            "{output}"
        );

        let truncated = format!(r#""{}\n{}\n{}"#, header(""), body[0], body[1]);
        let output = String::from_utf8(redact(&redactor, truncated.as_bytes()).0).unwrap();
        assert!(!output.contains(&body[0][..20]), "{output}");
    }

    #[test]
    fn unterminated_and_short_blocks() {
        let redactor = redactor();
        let body = pem_body("open", 3);
        let input = format!(
            "{}\n{}\n{}\nnot a body line: done\n",
            header(""),
            body[0],
            body[1]
        );
        let (output, count) = redact(&redactor, input.as_bytes());
        let output = String::from_utf8(output).unwrap();
        assert_eq!(count, 1);
        assert!(!output.contains(&body[0]) && output.ends_with("not a body line: done\n"));
        assert_eq!(output.lines().count(), 4);

        let at_end = format!("{}\n{}\n{}", header(""), body[0], body[1]);
        let (output, count) = redact(&redactor, at_end.as_bytes());
        assert_eq!(count, 1);
        assert!(!String::from_utf8(output).unwrap().contains(&body[1]));

        let short = format!("{}\nabc\nxyz\n{}\n", header(""), footer(""));
        assert_eq!(redact(&redactor, short.as_bytes()), (short.into_bytes(), 0));
        let empty = format!("{}\n{}\n", header(""), footer(""));
        assert_eq!(redact(&redactor, empty.as_bytes()), (empty.into_bytes(), 0));

        let long = private_key_block("long", "", PEM_LINES + 10).join("\n");
        let (output, count) = redact(&redactor, long.as_bytes());
        let output = String::from_utf8(output).unwrap();
        assert_eq!(count, 1);
        assert_eq!(output.lines().count(), PEM_LINES + 12);
        assert!(
            output
                .lines()
                .skip(2)
                .take(PEM_LINES - 1)
                .all(str::is_empty)
        );
    }

    #[test]
    fn base64_encoded_private_keys_but_not_certificates() {
        let redactor = redactor();
        let key = private_key_block("k8s", "", 4).join("\n");
        let encoded = base64(key.as_bytes());
        let line = format!("  tls.key: {encoded}\n");
        let (output, count) = redact(&redactor, line.as_bytes());
        assert_eq!(count, 1);
        assert!(
            String::from_utf8(output)
                .unwrap()
                .starts_with("  tls.key: [redacted:private-key:")
        );
        let certificate = format!(
            "{}BEGIN CERTIFICATE-----\n{}\n",
            "-----",
            pem_body("cert", 4).join("\n")
        );
        let line = format!("  tls.crt: {}\n", base64(certificate.as_bytes()));
        assert_eq!(redact(&redactor, line.as_bytes()).1, 0);
    }

    #[test]
    fn single_line_keys() {
        let redactor = redactor();
        let body = pem_body("single", 2).join("");
        for line in [
            format!("{}{}{}", header("RSA "), body, footer("RSA ")),
            format!("KEY=\"{}\\n{}\\n{}\"", header(""), body, footer("")),
            format!("KEY=\"{}\\n{}", header(""), body),
        ] {
            let (output, count) = redact(&redactor, line.as_bytes());
            assert_eq!(count, 1, "{line}");
            assert!(!String::from_utf8(output).unwrap().contains(&body[..32]));
        }
    }

    #[test]
    fn chunked_input_matches_whole_input() {
        let redactor = redactor();
        let mut input = String::new();
        for (_, _, line) in samples() {
            input.push_str(&line);
            input.push_str("\r\n");
        }
        input.push_str(&private_key_block("chunks", "EC ", 3).join("\n"));
        input.push_str("\n\x1b[31mpassword\x1b[0m: ");
        input.push_str(&base62("ansi", 16));
        let whole = redactor.bytes(input.as_bytes()).into_owned();
        for size in [1, 7, 4096] {
            let mut engine = Engine::new(&redactor, WINDOW, HOLD);
            let mut output = Vec::new();
            for chunk in input.as_bytes().chunks(size) {
                engine.feed(chunk, &mut output);
            }
            engine.finish(&mut output);
            assert_eq!(output, whole, "chunk size {size}");
        }
        let mut reader = Reader::new(input.as_bytes(), Some(&redactor));
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, whole);
        assert_eq!(reader.found().count, 18);
        assert_eq!(reader.found().rules, redactor.fingerprint());
    }

    fn windowed(redactor: &Redactor, input: &[u8], window: usize, hold: usize) -> Vec<u8> {
        let mut engine = Engine::new(redactor, window, hold);
        let mut output = Vec::new();
        for chunk in input.chunks(97) {
            engine.feed(chunk, &mut output);
        }
        engine.finish(&mut output);
        output
    }

    #[test]
    fn long_lines_match_whole_line_redaction_wherever_windows_fall() {
        let redactor = redactor();
        let (window, hold) = (4096, 512);
        let filler = base62("filler", 3 * window);
        let secret = format!("{}{}", "gh", "p_") + &base62("window", 36);
        let generic = base62("generic-window", 30);
        // Sweep across both the hold-back cut and the window end.
        for offset in (window - hold - 300..window + 300).step_by(7) {
            for (value, prefix) in [(&secret, " "), (&generic, " api_key=")] {
                let mut line = filler[..offset].to_owned();
                line.push_str(prefix);
                line.push_str(value);
                line.push(' ');
                line.push_str(&filler[offset..]);
                line.push('\n');
                let expected = redact(&redactor, line.as_bytes()).0;
                assert_ne!(expected, line.as_bytes());
                assert_eq!(
                    windowed(&redactor, line.as_bytes(), window, hold),
                    expected,
                    "offset {offset}"
                );
            }
        }
    }

    #[test]
    fn long_runs_without_secrets_pass_unchanged() {
        let redactor = redactor();
        let run = base64(base62("unbroken", 3 * 1024 * 1024).as_bytes());
        let input = format!("{run}\nnext\n");
        let mut reader = Reader::new(input.as_bytes(), Some(&redactor));
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(reader.found().count, 0);
        assert!(output == input.as_bytes());
        let engine = reader.engine.as_ref().unwrap();
        assert!(engine.line.capacity() <= WINDOW + HOLD + READ_SIZE);
    }

    #[test]
    fn long_secrets_crossing_a_window_are_fully_masked() {
        let redactor = redactor();
        let value = base62("very long value", 20 * 1024);
        let line = format!("{} token={value} tail\n", "-".repeat(3000));
        let output = windowed(&redactor, line.as_bytes(), 8192, 1024);
        let text = String::from_utf8(output).unwrap();
        assert!(text.ends_with(" tail\n") && text.contains("token=[redacted:secret:"));
        assert!(
            value
                .as_bytes()
                .windows(8)
                .step_by(5)
                .all(|w| !text.contains(std::str::from_utf8(w).unwrap())),
        );
        assert_eq!(
            text,
            String::from_utf8(redact(&redactor, line.as_bytes()).0).unwrap()
        );
    }

    #[test]
    fn word_boundaries_hold_at_window_starts() {
        let redactor = redactor();
        let (window, hold) = (4096, 512);
        let secret = format!("{}{}", "gh", "p_") + &base62("boundary", 36);
        // The token starts inside an alphanumeric run, so `\b` must not match even
        // where a window happens to begin.
        for offset in (window - hold - 40..window - hold + 40).step_by(3) {
            let mut line = "a".repeat(offset);
            line.push_str(&secret);
            line.push('\n');
            assert_eq!(redact(&redactor, line.as_bytes()).1, 0);
            assert_eq!(
                windowed(&redactor, line.as_bytes(), window, hold),
                line.as_bytes()
            );
        }
    }

    #[test]
    fn real_size_window_with_a_secret_at_the_boundary() {
        let redactor = redactor();
        let secret = base62("real-size", 24);
        let mut line = "z".repeat(WINDOW - HOLD - 5);
        line.push_str(" password=");
        line.push_str(&secret);
        line.push(' ');
        line.push_str(&"y".repeat(2 * WINDOW));
        line.push('\n');
        let mut reader = Reader::with_window(line.as_bytes(), &redactor, WINDOW, HOLD);
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(reader.found().count, 1);
        assert_eq!(output.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(output, redact(&redactor, line.as_bytes()).0);
        assert!(reader.engine.as_ref().unwrap().line.capacity() < WINDOW + HOLD + READ_SIZE + 64);
    }

    #[test]
    fn allowlist_and_fingerprint() {
        let value = base62("allowed", 24);
        let line = format!("password={value}EXAMPLEX\n");
        assert_eq!(text(&line), line);
        let line = format!("password={value}\n");
        let allow = vec![format!("^{}", &value[..6])];
        let allowed = Redactor::build(&allow, KeySource::Fixed(None)).unwrap();
        assert_eq!(redact(&allowed, line.as_bytes()).1, 0);
        assert_eq!(redact(&redactor(), line.as_bytes()).1, 1);
        assert_ne!(allowed.fingerprint(), redactor().fingerprint());
        assert!(redactor().fingerprint().starts_with("r1-"));

        let error = Redactor::build(&["[".to_owned()], KeySource::Fixed(None))
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("redact_allow: invalid pattern '['"),
            "{error}"
        );
        let error = Redactor::build(&["x*".to_owned()], KeySource::Fixed(None))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("matches empty text"), "{error}");
    }

    #[test]
    fn every_positive_holds_a_trigger() {
        for (kind, _, line) in samples() {
            assert!(PREFILTER.is_match(line.as_bytes()), "{kind}");
        }
        for spec in SPECS {
            // Each rule names its trigger literally, so the prefilter cannot miss it.
            let lowered = spec.pattern.to_ascii_lowercase();
            assert!(
                TRIGGERS
                    .iter()
                    .any(|t| spec.pattern.contains(&t.replace('.', r"\.")))
                    || KEYWORDS.iter().any(|k| lowered.contains(k))
                    || spec.pattern.contains("(?:A3T[A-Z0-9]|AKIA")
                    || spec.pattern.contains("gl(?:pat")
                    || spec.pattern.contains("xox[")
                    || spec.pattern.contains("(?:ghp|gho")
                    || spec.pattern.contains("[rs]k_live_")
                    || spec.pattern.contains("api[_.-]?key"),
                "{}",
                spec.kind
            );
        }
    }

    #[test]
    fn decoder_and_helpers() {
        assert_eq!(
            decode_base64(base64(b"hello world!").as_bytes()),
            b"hello world!"
        );
        assert!(name_shaped(b"ML_KEM_512_SHARED_SECRET_LENGTH"));
        assert!(name_shaped(b"password.encode_utf16"));
        assert!(name_shaped(b"./target/debug"));
        assert!(!name_shaped(b"a8F3kQ9zR2"));
        assert_eq!(base64_bytes(br"ab\ncd\r=="), 6);
        assert!(entropy(b"aaaaaaaaaaaa") < 0.1);
    }
}
