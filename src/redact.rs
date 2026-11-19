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
const RULES_VERSION: &str = "2";
const READ_SIZE: usize = 64 * 1024;
/// Lines longer than this are redacted in windows instead of being held whole.
const WINDOW: usize = 1024 * 1024;
/// Bytes at the end of each window that are scanned again with the next one.
const HOLD: usize = 8 * 1024;
/// A private key block longer than this many lines ends the block.
const PEM_LINES: usize = 1024;
/// A private key block holding more than this many bytes ends, so memory stays bounded.
const PEM_BYTES: usize = 256 * 1024;
/// A key body line with more base64 than this is not key material.
const PEM_LINE_BYTES: usize = 16 * 1024;
/// Wrapped base64 is at most this wide; a longer first line starts no block.
const WRAP_WIDTH: usize = 128;
/// Text before a key header longer than this is not treated as a line prefix.
const PREFIX_BYTES: usize = 256;

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
    "passphrase",
    "pass_phrase",
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
    // An image reference such as `docker://ubuntu:22.04@sha256:…` has a tag, not a
    // password, before its digest.
    Spec {
        kind: "url-password",
        pattern: r#"(?i-u)\b[a-z][a-z0-9+.-]{1,20}://[^\s:/?#@'"<>]{0,128}:(?P<s>[^\s/?#@'"<>]{4,256})@(?P<x>sha(?:256|384|512):)?"#,
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
    // The value runs to whitespace, a quote or a backslash, so symbols such as `$`,
    // `@` or `(` stay inside it. `n` is the rest of the name, `sep` the separator and
    // `q` an opening quote; `Check::Generic` reads them.
    Spec {
        kind: "secret",
        pattern: r#"(?i-u)(?:password|passwd|pass[_.-]?phrase|secret|token|api[_.-]?key|apikey|access[_.-]?key|private[_.-]?key|credentials?)(?P<n>[A-Za-z0-9_.-]{0,20})(?:\\?["'])?\s{0,3}(?P<sep>:=|=>|=|:)\s{0,3}(?P<q>\\?["'`])?(?P<s>[^\s"'`\\]{12,})"#,
        check: Check::Generic(3.5),
        tail: r#"[^\s"'`\\]"#,
    },
    Spec {
        kind: "secret",
        pattern: r#"(?i-u)--[a-z0-9-]{0,30}(?:password|pass-?phrase|secret|token|api-key)[a-z0-9-]{0,20}\s{1,3}(?P<q>["'])?(?P<s>[^\s"'`\\]{12,})"#,
        check: Check::Generic(3.5),
        tail: r#"[^\s"'`\\]"#,
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
/// A body line: optional string syntax around base64 or an RFC 1421 header. The
/// length of the base64 is checked in code, so an unwrapped body still matches.
static PEM_BODY: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?-u)^(\s*["'`]?)([A-Za-z0-9+/=]+|(?:Proc-Type|DEK-Info|Comment|Version|Hash):[^"'`\\]*|)((?:\\[nr])*\\?["'`]?\s*[,+;)]*\s*\\?)\s*$"#,
    )
});
/// A line of wrapped base64, such as the output of `base64` or `openssl base64`.
static ENCODED_LINE: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?-u)^\s*([A-Za-z0-9+/]+={0,2})\s*$"));
/// An unquoted value that is code: a call, macro, index, tuple or generic type, such
/// as `hashlib.sha256(x)`, `Token![=](span),`, `buf.read::<u32>()?;`,
/// `&self.0[i2];` or `Option<Ed25519PrivateKey>,`, or a path such as
/// `&secp256k1::SecretKey`. What follows the bracket ends in closing syntax or holds
/// only identifier characters, as when a space cut it off; after a method such as
/// `path.match(` it can be anything. Symbols that code rarely holds, such as `@`, `%`
/// or a `$` outside a macro variable, mark a generated password.
static CODE: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?-u)^[&*!]?(?:::)?(?:(?:[A-Za-z_][A-Za-z0-9_]*(?:(?:\.|::|->)[A-Za-z0-9_]+)*(?:::)?!?)?[(\[<](?:(?:[^@$%^]|\$[A-Za-z_(])*[)\]>,;?{|]|[A-Za-z0-9_.:&*!<(\[]*)|[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+|[A-Za-z_][A-Za-z0-9_]*(?:\.|->)[A-Za-z_][A-Za-z0-9_]*(?:(?:\.|::|->)[A-Za-z0-9_]+)*[(\[].*)$"#,
    )
});
/// Template and placeholder syntax such as `{token2}`, `${{ secrets.X }}`, `%(pw)s`,
/// `$(cat f)` or `<api-key-1>`, an interpolation such as `${config.url}/token`, or
/// a character class such as `[A-Za-z0-9._~+/-]+`.
static TEMPLATE: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?-u)^(?:\$?\{.*\}|%\([A-Za-z_][A-Za-z0-9_]*\)[sdr]|\$\(.*\)|<.*>)$|\$\{[A-Za-z_][A-Za-z0-9_.]*\}|a-z|A-Z|0-9",
    )
});
/// A checksum labelled with its algorithm, such as `sha256=4f9c…` or `sha512-…`.
static DIGEST: LazyLock<Regex> = LazyLock::new(|| {
    compile(r"(?i-u)^(?:sha1|sha224|sha256|sha384|sha512|md5|blake2b?|blake3)[-=:]")
});
/// A file extension ending a name, such as the `.rs` of `token.rs`.
static EXTENSION: LazyLock<Regex> = LazyLock::new(|| compile(r"(?-u)\.[A-Za-z0-9]{1,8}$"));
/// A source file name ending a key, such as `src/to_tokens.rs` in a checksum map.
/// Files that could hold a secret themselves, such as `.txt` or `.env`, are not here.
static SOURCE_FILE: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?i-u)\.(?:rs|js|mjs|cjs|ts|tsx|jsx|css|scss|py|go|rb|java|kt|swift|c|h|cc|cpp|hpp|md|html|toml|lock|map|svg|png|xml|proto|wasm)$",
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

    /// Whether redacting `input` gives exactly `expected`. Reading stops as soon as
    /// the redacted text differs, so a large stream costs little.
    pub fn matches(&self, input: impl Read, expected: &[u8]) -> io::Result<bool> {
        let mut reader = Reader::new(input, Some(self));
        let mut at = 0;
        loop {
            let chunk = reader.fill_buf()?;
            if chunk.is_empty() {
                return Ok(at == expected.len());
            }
            let length = chunk.len();
            if expected.get(at..at + length) != Some(chunk) {
                return Ok(false);
            }
            at += length;
            reader.consume(length);
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

    fn accepts(&self, check: Check, value: &[u8], context: Context) -> bool {
        self.shared(value)
            && match check {
                Check::None => true,
                Check::Entropy(minimum) => entropy(value) >= minimum,
                Check::Generic(minimum) => generic(value, minimum, context),
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

/// What surrounds a value matched after a keyword.
#[derive(Clone, Copy)]
struct Context<'a> {
    /// The value follows an opening quote, so it is a string rather than code.
    quoted: bool,
    /// The separator was a bare `:`, as in a type annotation.
    colon: bool,
    /// The value before closing punctuation was trimmed from it.
    untrimmed: &'a [u8],
}

/// A value after a keyword: high entropy with a letter and a digit, two symbols such
/// as `@` or `$` as in a generated password, or a long run of mixed-case letters that
/// is not CamelCase, and not code, a template, a checksum, a URL, a path or a name.
fn generic(value: &[u8], minimum: f64, context: Context) -> bool {
    let symbols = value
        .iter()
        .filter(|byte| b"!@#$%^*?~+=|".contains(byte))
        .count();
    let entropy = entropy(value);
    let random_letters = || {
        value.len() >= 20
            && entropy >= 4.0
            && value.iter().any(u8::is_ascii_uppercase)
            && value.iter().any(u8::is_ascii_lowercase)
            && !value
                .split(|byte| !byte.is_ascii_alphanumeric())
                .all(word_like)
    };
    value.len() >= 12
        && entropy >= minimum
        && value.iter().any(u8::is_ascii_alphabetic)
        && (symbols >= 2 || value.iter().any(u8::is_ascii_digit) || random_letters())
        // `token::Kind` is a path, not `token: :Kind`.
        && !(context.colon && !context.quoted && value.starts_with(b":"))
        && !value.windows(3).any(|window| window == b"://")
        && !DIGEST.is_match(value)
        // A public 20-byte account address such as a token contract's; private keys
        // are 32 bytes.
        && !(value.len() == 42
            && value.starts_with(b"0x")
            && value[2..].iter().all(u8::is_ascii_hexdigit))
        && !TEMPLATE.is_match(value)
        && !TEMPLATE.is_match(context.untrimmed)
        && !path_shaped(value)
        && !identifier_chain(value)
        && (context.quoted || !CODE.is_match(context.untrimmed) && !CODE.is_match(value))
        && !(context.colon && !context.quoted && type_name(value))
}

/// A path such as `config/secrets/prod-2024`, `~/keys/ci_ed25519.pem` or
/// `/Users/Me/MyProject`: it has a slash and no `+`, and every part between `/ . _ -`
/// is a lowercase, uppercase or capitalised word with digits. A random base64 value
/// with a slash has a long mixed-case part, so it is still treated as a secret.
fn path_shaped(value: &[u8]) -> bool {
    value.contains(&b'/')
        && !value.contains(&b'+')
        && value.split(|byte| b"/._-~@:".contains(byte)).all(word_like)
}

fn word_like(part: &[u8]) -> bool {
    if !part.iter().all(u8::is_ascii_alphanumeric) {
        return false;
    }
    if !part.iter().any(u8::is_ascii_uppercase) || !part.iter().any(u8::is_ascii_lowercase) {
        return true;
    }
    // CamelCase: each capital starts a word of lowercase letters, then digits.
    let mut index = part
        .iter()
        .take_while(|byte| !byte.is_ascii_uppercase())
        .count();
    while index < part.len() {
        index += 1;
        let letters = part[index..]
            .iter()
            .take_while(|byte| byte.is_ascii_lowercase())
            .count();
        if letters == 0 {
            return false;
        }
        index += letters;
        index += part[index..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
    }
    true
}

/// Identifier chains and simple expressions such as `ML_KEM_512_SHARED_SECRET_LENGTH`,
/// `child1_token.child_token`, `&p256::PRIVATE_KEY_OPS`, `s->s3.mac_secret_size` or
/// `params.secret_len*2`, where every segment is letters followed by digits.
fn identifier_chain(value: &[u8]) -> bool {
    let start = value
        .iter()
        .take_while(|byte| b"&*/".contains(byte))
        .count();
    let value = &value[start..];
    let separator = |byte: &u8| matches!(byte, b'_' | b'.' | b'-' | b':' | b'*' | b'>' | b',');
    value.iter().any(separator)
        && value.split(separator).all(|segment| {
            let letters = segment
                .iter()
                .take_while(|byte| byte.is_ascii_alphabetic())
                .count();
            segment[letters..].iter().all(u8::is_ascii_digit)
        })
}

/// A type name such as `Ed25519PrivateKey` in `private_key: Ed25519PrivateKey`:
/// capitalised and alphanumeric, naming what it holds, with at most two runs of
/// digits. Only a value after a bare `:` is checked for it, so `PASSWORD=MyPass2024`
/// is still redacted.
fn type_name(value: &[u8]) -> bool {
    // A reference such as `&X25519SharedSecret` is still a type.
    let name = value.strip_prefix(b"&").unwrap_or(value);
    let digit_runs = name
        .iter()
        .enumerate()
        .filter(|&(index, byte)| {
            byte.is_ascii_digit() && (index == 0 || !name[index - 1].is_ascii_digit())
        })
        .count();
    name.first().is_some_and(u8::is_ascii_uppercase)
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        && NAMED.is_match(name)
        && digit_runs <= 2
}

/// Hex of a checksum's length or longer, as in a checksum map.
fn hex_digest(value: &[u8]) -> bool {
    value.len() >= 32 && value.iter().all(u8::is_ascii_hexdigit)
}

/// The keyword is part of a file name or path, such as `src/token.rs` or
/// `secrets/db_password`, as a search prints before each line.
fn file_name(hay: &[u8], start: usize, name: &[u8]) -> bool {
    let floor = start.saturating_sub(PREFIX_BYTES);
    let word = hay[floor..start]
        .iter()
        .rposition(|byte| !(byte.is_ascii_alphanumeric() || b"_.-/".contains(byte)))
        .map_or(floor, |index| floor + index + 1);
    hay[word..start].contains(&b'/') || EXTENSION.is_match(name)
}

/// The length of a leading line number such as `107:` or `12-`.
fn line_number(value: &[u8]) -> usize {
    let digits = value
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    match value.get(digits) {
        Some(b':' | b'-') if digits > 0 => digits + 1,
        _ => 0,
    }
}

/// Whether `at` falls inside a marker, such as the `password` in
/// `[redacted:url-password:1b03]`, so redacting twice changes nothing.
fn inside_marker(hay: &[u8], at: usize) -> bool {
    let floor = at.saturating_sub(40);
    hay[floor..at]
        .iter()
        .rposition(|&byte| byte == b'[' || byte == b']')
        .is_some_and(|index| hay[floor + index..].starts_with(b"[redacted:"))
}

/// Drops punctuation that ends an unquoted value rather than belonging to it: a
/// trailing comma or colon, or a bracket the value did not open.
fn trim(hay: &[u8], mut range: Range<usize>, quoted: bool) -> Range<usize> {
    const PAIRS: [(u8, u8); 4] = [(b'(', b')'), (b'[', b']'), (b'{', b'}'), (b'<', b'>')];
    // Opened minus closed, per bracket pair.
    let mut open = [0isize; 4];
    for byte in &hay[range.clone()] {
        for (index, (opener, closer)) in PAIRS.iter().enumerate() {
            open[index] += isize::from(byte == opener) - isize::from(byte == closer);
        }
    }
    while range.end > range.start {
        let last = hay[range.end - 1];
        let pair = PAIRS.iter().position(|(_, closer)| *closer == last);
        let drop = !quoted
            && match pair {
                Some(index) => open[index] < 0,
                None => b",;:.".contains(&last),
            };
        if !drop {
            break;
        }
        if let Some(index) = pair {
            open[index] += 1;
        }
        range.end -= 1;
    }
    range
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
/// on the next lines. Returns the shape of the text before the header, which each
/// body line repeats when a tool prefixes lines, as `rg -n`, `git diff` or a
/// timestamped log do.
fn pem_opens(text: &[u8]) -> Option<Prefix> {
    let text = without_colors(text);
    let begin = PEM_BEGIN.find_iter(&text).last()?;
    let tail = &text[begin.end()..];
    (!PEM_END.is_match(tail) && PEM_TAIL.is_match(tail))
        .then(|| Prefix::new(&text[..begin.start()]))
}

fn without_colors(text: &[u8]) -> Cow<'_, [u8]> {
    if text.contains(&0x1b) {
        ANSI.replace_all(text, &b""[..])
    } else {
        Cow::Borrowed(text)
    }
}

#[derive(Clone, Copy)]
enum Token {
    /// A diff marker: `+`, `-` or a space.
    Diff,
    /// One or more digits, such as a line number.
    Digits,
    /// Spaces or tabs, possibly none.
    Space,
    /// `:` or `-`, which search tools use for matches and context lines.
    Separator,
    Byte(u8),
}

/// The shape of the text before a key header, such as `+`, `12:`, `id_rsa-3-`,
/// `     4\t` or `2026-11-18T10:00:00Z `. Body lines carry the same shape with their
/// own line numbers, times or diff markers.
#[derive(Default)]
struct Prefix(Vec<Token>);

impl Prefix {
    fn new(text: &[u8]) -> Self {
        if text.len() > PREFIX_BYTES {
            return Self::default();
        }
        let mut tokens = Vec::new();
        let mut index = 0;
        while index < text.len() {
            let byte = text[index];
            let run = |test: fn(&u8) -> bool| text[index..].iter().take_while(|b| test(b)).count();
            let (token, length) = if index == 0 && (byte == b'+' || byte == b'-' || text == b" ") {
                (Token::Diff, 1)
            } else if byte.is_ascii_digit() {
                (Token::Digits, run(u8::is_ascii_digit))
            } else if byte == b' ' || byte == b'\t' {
                (Token::Space, run(|b| *b == b' ' || *b == b'\t'))
            } else if byte == b':' || byte == b'-' {
                (Token::Separator, 1)
            } else {
                (Token::Byte(byte), 1)
            };
            tokens.push(token);
            index += length;
        }
        Self(tokens)
    }

    /// The length of this shape at the start of `line`, if the line has it.
    fn find(&self, line: &[u8]) -> Option<usize> {
        let mut at = 0;
        for token in &self.0 {
            let rest = &line[at..];
            let run = |test: fn(&u8) -> bool| rest.iter().take_while(|b| test(b)).count();
            let length = match token {
                Token::Diff => usize::from(matches!(rest.first(), Some(b'+' | b'-' | b' '))),
                Token::Digits => run(u8::is_ascii_digit),
                Token::Space => {
                    at += run(|b| *b == b' ' || *b == b'\t');
                    continue;
                }
                Token::Separator => usize::from(matches!(rest.first(), Some(b':' | b'-'))),
                Token::Byte(byte) => usize::from(rest.first() == Some(byte)),
            };
            if length == 0 {
                return None;
            }
            at += length;
        }
        Some(at)
    }
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

/// A line buffered inside a key block.
struct PemLine {
    original: Vec<u8>,
    /// The line without terminal colors, when it had any. Ranges index this text.
    plain: Option<Vec<u8>>,
    /// Key material or an RFC 1421 header value, removed when the block is redacted.
    range: Range<usize>,
    material: bool,
}

/// A private key block being buffered, so its whole body gets one marker.
#[derive(Default)]
struct Pem {
    prefix: Prefix,
    lines: Vec<PemLine>,
    material: Vec<u8>,
    bytes: usize,
    /// Wrapped base64 of a whole key, with the width of its first line, which was
    /// already redacted and counted.
    encoded: Option<usize>,
}

impl Pem {
    fn push(&mut self, line: PemLine) {
        self.bytes += line.original.len();
        if line.material {
            let text = line.plain.as_ref().unwrap_or(&line.original);
            self.material.extend_from_slice(&text[line.range.clone()]);
        }
        self.lines.push(line);
    }

    fn full(&self, line: &[u8]) -> bool {
        self.lines.len() >= PEM_LINES || self.bytes + line.len() > PEM_BYTES
    }
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
    /// Markers the last `redact` call wrote: where in its output, the rule, and the
    /// secret's range in its input.
    spans: Vec<(Range<usize>, usize, Range<usize>)>,
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
            spans: Vec::new(),
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
            if let Some(prefix) = pem_opens(&out[start..]) {
                self.pem = Some(Pem {
                    prefix,
                    ..Pem::default()
                });
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
        let stripped;
        let hay = if body.contains(&0x1b) {
            // Colors can split a keyword from its value. Match without them, and keep
            // the original bytes unless something was redacted.
            stripped = ANSI.replace_all(body, &b""[..]);
            let before = self.count;
            let mut redacted = Vec::with_capacity(stripped.len());
            self.redact(&stripped, 0, None, &mut redacted);
            if self.count > before {
                out.extend_from_slice(&redacted);
                &stripped[..]
            } else {
                out.extend_from_slice(body);
                body
            }
        } else {
            self.redact(body, 0, None, out);
            body
        };
        if let Some(prefix) = pem_opens(&out[start..]) {
            self.pem = Some(Pem {
                prefix,
                ..Pem::default()
            });
        } else if let Some((marker, secret)) =
            self.spans.iter().find_map(|(marker, rule, secret)| {
                // Base64 of a whole key that `base64` or `openssl base64` wrapped: the
                // next lines hold the rest of it.
                (matches!(RULES.list[*rule].check, Check::PemBase64)
                    && secret.len() <= WRAP_WIDTH
                    && hay[secret.end..].iter().all(u8::is_ascii_whitespace))
                .then(|| (marker.clone(), hay[secret.clone()].to_vec()))
            })
        {
            let mut first = out.split_off(start);
            let prefix = Prefix::new(&first[..marker.start]);
            first.extend_from_slice(terminator);
            self.pem = Some(Pem {
                prefix,
                bytes: first.len(),
                lines: vec![PemLine {
                    original: first,
                    plain: None,
                    range: marker,
                    material: true,
                }],
                encoded: Some(secret.len()),
                material: secret,
            });
            return;
        }
        out.extend_from_slice(terminator);
    }

    /// Buffers a line inside a key block. Returns false when the line ends the block
    /// without belonging to it.
    fn pem_line(&mut self, line: &[u8], out: &mut Vec<u8>) -> bool {
        let pem = self.pem.as_ref().expect("key block");
        let body = &line[..content_len(line)];
        let text = without_colors(body);
        let plain =
            matches!(text, Cow::Owned(_)).then(|| [&text[..], &line[body.len()..]].concat());
        if pem.encoded.is_some() {
            return self.encoded_line(line, &text, plain, out);
        }
        if PEM_END.is_match(&text) {
            let line = PemLine {
                original: line.to_vec(),
                plain: None,
                range: 0..0,
                material: false,
            };
            self.pem.as_mut().expect("key block").push(line);
            self.flush_pem(out);
            return true;
        }
        let found = (!pem.full(line))
            .then(|| {
                // Prefer the header's prefix, such as `12-` or `+`; string syntax and
                // indentation are part of the body pattern.
                pem.prefix
                    .find(&text)
                    .and_then(|start| {
                        pem_body(&text[start..])
                            .map(|(range, m)| (range.start + start..range.end + start, m))
                    })
                    .or_else(|| pem_body(&text))
            })
            .flatten();
        let Some((range, material)) = found else {
            self.flush_pem(out);
            return false;
        };
        if material && pem.material.is_empty() && range.len() < 16 {
            self.flush_pem(out);
            return false;
        }
        let line = PemLine {
            original: line.to_vec(),
            plain,
            range,
            material,
        };
        self.pem.as_mut().expect("key block").push(line);
        true
    }

    /// Buffers a line of wrapped base64 after the first. Lines as wide as the first
    /// continue the block; a shorter line ends it when the decoded text then ends a key.
    fn encoded_line(
        &mut self,
        line: &[u8],
        text: &[u8],
        plain: Option<Vec<u8>>,
        out: &mut Vec<u8>,
    ) -> bool {
        let pem = self.pem.as_ref().expect("key block");
        let width = pem.encoded.expect("encoded block");
        let found = pem
            .prefix
            .find(text)
            .and_then(|start| {
                encoded_body(&text[start..]).map(|range| range.start + start..range.end + start)
            })
            .or_else(|| encoded_body(text))
            .filter(|range| range.len() <= width && !pem.full(line));
        let last = found.as_ref().is_some_and(|range| range.len() < width);
        let accepted = found.filter(|range| {
            !last || {
                let mut material = pem.material.clone();
                material.extend_from_slice(&text[range.clone()]);
                material.len().is_multiple_of(4) && ends_key(&material)
            }
        });
        let Some(range) = accepted else {
            self.flush_pem(out);
            return false;
        };
        let line = PemLine {
            original: line.to_vec(),
            plain,
            range,
            material: true,
        };
        self.pem.as_mut().expect("key block").push(line);
        if last {
            self.flush_pem(out);
        }
        true
    }

    /// Emits a buffered key block: the first material line becomes one marker, the
    /// other material and header values are removed, and every line break stays.
    fn flush_pem(&mut self, out: &mut Vec<u8>) {
        let Some(pem) = self.pem.take() else {
            return;
        };
        let encoded = pem.encoded.is_some();
        if !encoded && (pem.material.is_empty() || !self.redactor.shared(&pem.material)) {
            for line in pem.lines {
                out.extend_from_slice(&line.original);
            }
            return;
        }
        // The first line of encoded base64 was already counted when it was redacted.
        if !encoded {
            self.count += 1;
        }
        let mut marker = Some(self.redactor.marker("private-key", &pem.material));
        for line in pem.lines {
            let text = match &line.plain {
                Some(plain) if !line.range.is_empty() => plain,
                _ => &line.original,
            };
            out.extend_from_slice(&text[..line.range.start]);
            if line.material
                && let Some(marker) = marker.take()
            {
                out.extend_from_slice(&marker);
            }
            out.extend_from_slice(&text[line.range.end..]);
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
            hits(self.redactor, hay, from, hold.is_some())
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
        // A secret that ends before its match, as in `user:pw@` at the window's end,
        // is emitted whole rather than cut.
        if let Some((secret, _)) = spans.last() {
            emit = emit.max(secret.end);
        }
        let base = out.len();
        self.spans.clear();
        let mut at = from;
        for (secret, rule) in spans {
            out.extend_from_slice(&hay[at..secret.start]);
            if secret.end != end || continuation.is_none() {
                let kind = RULES.list[rule].kind;
                let start = out.len() - base;
                out.extend_from_slice(&self.redactor.marker(kind, &hay[secret.clone()]));
                self.spans
                    .push((start..out.len() - base, rule, secret.clone()));
            }
            self.count += 1;
            at = secret.end;
        }
        out.extend_from_slice(&hay[at..emit]);
        (emit, continuation)
    }
}

/// The material or header range of a key body line, and whether it is material.
fn pem_body(text: &[u8]) -> Option<(Range<usize>, bool)> {
    let captures = PEM_BODY.captures(text)?;
    let range = captures.get(2).expect("material group").range();
    let value = &text[range.clone()];
    let material = !value.is_empty() && !value.contains(&b':');
    (!material || value.len() <= PEM_LINE_BYTES).then_some((range, material))
}

/// The base64 of a line of wrapped base64.
fn encoded_body(text: &[u8]) -> Option<Range<usize>> {
    Some(
        ENCODED_LINE
            .captures(text)?
            .get(1)
            .expect("base64 group")
            .range(),
    )
}

/// Whether complete base64 decodes to text ending in a PEM footer's dashes.
fn ends_key(material: &[u8]) -> bool {
    let decoded = decode_base64(&material[material.len().saturating_sub(64)..]);
    let end = decoded
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |index| index + 1);
    decoded[..end].ends_with(b"-----")
}

fn line_end(data: &[u8]) -> usize {
    data.iter()
        .position(|&byte| byte == b'\n')
        .map_or(data.len(), |index| index + 1)
}

/// Every rule match from `from`, accepted or not, in rule order. With `open_end`, the
/// line goes on past `hay`, so a value that reaches its end is kept whole.
fn hits(redactor: &Redactor, hay: &[u8], from: usize, open_end: bool) -> Vec<Hit> {
    let rules = &*RULES;
    let mut hits = Vec::new();
    let mut rescan = 2 * (hay.len() - from) + 4096;
    for index in rules.set.matches_at(hay, from).iter() {
        let rule = &rules.list[index];
        let mut at = from;
        while at <= hay.len()
            && let Some(captures) = rule.re.captures_at(hay, at)
        {
            let whole = captures.get(0).expect("match");
            let found = captures.name("s").expect("secret group");
            let quoted = captures.name("q").is_some();
            let generic = matches!(rule.check, Check::Generic(_));
            let name = captures.name("n").map_or(&b""[..], |name| name.as_bytes());
            let colon = captures
                .name("sep")
                .is_some_and(|sep| sep.as_bytes() == b":");
            let mut start = found.start();
            if generic && colon && !quoted && file_name(hay, whole.start(), name) {
                // After a file name, as in `src/token.rs:107:` from a search, the value
                // is the line's text, so its line number is not part of it.
                start += line_number(&hay[start..found.end()]);
            }
            let secret = if generic && !(open_end && found.end() == hay.len()) {
                trim(hay, start..found.end(), quoted)
            } else {
                start..found.end()
            };
            let context = Context {
                quoted,
                colon,
                untrimmed: &hay[start..found.end()],
            };
            let value = &hay[secret.clone()];
            // An `x` group marks a match to leave alone, such as an image digest.
            let ok = captures.name("x").is_none()
                && redactor.accepts(rule.check, value, context)
                && !inside_marker(hay, whole.start())
                // A checksum after a source file name such as `src/to_tokens.rs`.
                && !(generic && hex_digest(value) && SOURCE_FILE.is_match(name));
            hits.push(Hit {
                start: whole.start(),
                end: whole.end(),
                secret,
                rule: index,
                ok,
            });
            // A rejected value can hold another assignment, as in
            // `token_url=https://h/?token=…`, so look inside it, within a budget
            // that keeps a long run of keywords linear.
            let next = if generic && !ok && found.len() <= rescan {
                rescan -= found.len();
                found.start()
            } else {
                whole.end()
            };
            at = next.max(whole.start() + 1);
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
                origin: root.to_path_buf(),
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

/// Whether a run's record says its archive holds no secret under these rules. A
/// run whose streams a background process could still extend after the scan is
/// never clean, and neither is one Justless did not finish recording.
fn recorded_clean(directory: &Path, fingerprint: &str) -> bool {
    #[derive(Deserialize)]
    struct Record {
        #[serde(default)]
        redaction: Option<Found>,
        #[serde(default)]
        drained: bool,
        #[serde(default)]
        unfinished: bool,
    }
    File::open(directory.join("record.json"))
        .ok()
        .and_then(|file| serde_json::from_reader::<_, Record>(BufReader::new(file)).ok())
        .filter(|record| !record.drained && !record.unfinished)
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
    /// One positive sample per kind, built at runtime: (kind, secret, line),
    /// where the line is the template with `$` replaced by the secret.
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
        let url = format!("{}://app:$@db.internal:5432/app", "postgres");
        let authorization = format!("> {}: Bearer $", "Authorization");
        [
            ("github-token", github, "GITHUB_TOKEN=$"),
            ("aws-access-key", aws, "  \"AccessKeyId\": \"$\","),
            ("gitlab-token", gitlab, "token: $"),
            ("slack-token", slack, "export SLACK_BOT=$"),
            (
                "slack-webhook",
                webhook,
                "url = https://hooks.slack.com/services/$",
            ),
            ("stripe-key", stripe, "STRIPE=$"),
            ("anthropic-key", anthropic, "key $ loaded"),
            ("openai-key", openai, "{\"key\": \"$\"}"),
            ("google-api-key", google, "maps?key=$&q=1"),
            ("npm-token", npm, "//registry.npmjs.org/:_authToken=$"),
            ("pypi-token", pypi, "password = $"),
            ("huggingface-token", huggingface, "HF $"),
            ("jwt", token, "cookie: session=$; Path=/"),
            ("url-password", password, url.as_str()),
            ("authorization", bearer, authorization.as_str()),
            ("secret", generic, "API_KEY=\"$\""),
        ]
        .into_iter()
        .map(|(kind, secret, template)| (kind, secret.clone(), template.replace('$', &secret)))
        .collect()
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

    /// A key block whose lines carry `prefix(line number)`, with a trailing newline.
    fn prefixed(seed: &str, prefix: impl Fn(usize) -> String) -> String {
        private_key_block(seed, "", 6)
            .iter()
            .enumerate()
            .map(|(index, line)| format!("{}{line}\n", prefix(index + 1)))
            .collect()
    }

    /// A key block base64-encoded and wrapped at `width`, with a trailing newline.
    fn wrapped(seed: &str, width: usize) -> String {
        let block = private_key_block(seed, "", 6).join("\n") + "\n";
        base64(block.as_bytes())
            .as_bytes()
            .chunks(width)
            .map(|line| format!("{}\n", std::str::from_utf8(line).unwrap()))
            .collect()
    }

    fn key_marker(output: &str) -> &str {
        let start = output.find("[redacted:private-key:").expect("marker");
        &output[start..start + 27]
    }

    #[test]
    fn prefixed_colored_and_wrapped_blocks() {
        let redactor = redactor();
        let body = pem_body("", 6);
        let plain = text(&prefixed("", |_| String::new()));
        let expected = key_marker(&plain).to_owned();
        let shapes: [&dyn Fn(usize) -> String; 7] = [
            &|n| format!("{n}:"),
            &|n| {
                format!(
                    "id_rsa{}{n}{}",
                    if n == 1 { ":" } else { "-" },
                    if n == 1 { ":" } else { "-" }
                )
            },
            &|n| format!("{n:>6}\t"),
            &|n| if n % 2 == 0 { "-".into() } else { "+".into() },
            &|n| format!("2026-11-18T10:00:{n:02}.{}Z ", "5".repeat(n % 4 + 1)),
            &|_| "\x1b[32m+\x1b[m\x1b[32m".into(),
            &|_| "    ".into(),
        ];
        for shape in shapes {
            let input = prefixed("", shape);
            let (output, count) = redact(&redactor, input.as_bytes());
            let output = String::from_utf8(output).unwrap();
            assert_eq!(count, 1, "{input}");
            assert_eq!(key_marker(&output), expected, "{output}");
            assert_eq!(output.lines().count(), input.lines().count());
            assert!(
                body.iter().all(|line| !output.contains(&line[..32])),
                "{output}"
            );
            // The prefix of the first body line stays.
            let second = input.lines().nth(1).unwrap();
            let prefix = &second[..second.find(&body[0]).unwrap()];
            let prefix = ANSI.replace_all(prefix.as_bytes(), &b""[..]);
            assert!(
                output
                    .lines()
                    .nth(1)
                    .unwrap()
                    .as_bytes()
                    .starts_with(&prefix)
            );
            assert_eq!(
                redactor.bytes(output.as_bytes()),
                Cow::Borrowed(output.as_bytes())
            );
        }

        let one = text(&wrapped("wrapped", 64));
        let two = text(&wrapped("wrapped", 76));
        assert_eq!(key_marker(&one), key_marker(&two));
        assert!(one.lines().skip(1).all(str::is_empty), "{one}");
        assert_ne!(key_marker(&one), key_marker(&text(&wrapped("other", 64))));
        // A short line that does not end the key is not part of it.
        let mut cut = wrapped("cut", 64);
        cut.truncate(cut.rfind('\n').unwrap());
        cut.truncate(cut.rfind('\n').unwrap() + 1);
        let output = text(&format!("{cut}done\n"));
        assert!(output.ends_with("\ndone\n"), "{output}");
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
        input.push('\n');
        input.push_str(&prefixed("chunks-diff", |n| format!("\x1b[32m{n}:\x1b[m")));
        input.push_str(&wrapped("chunks-b64", 64));
        input.push_str("\x1b[31mpassword\x1b[0m: ");
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
        assert_eq!(reader.found().count, 20);
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
        let symbols = format!("{}$(x)@y,", base62("symbols-window", 20));
        let url = format!("redis://u:{}@h", base62("url-window", 12));
        // Sweep across both the hold-back cut and the window end.
        for offset in (window - hold - 300..window + 300).step_by(7) {
            for (value, prefix) in [
                (&secret, " "),
                (&generic, " api_key="),
                (&symbols, " password="),
                (&url, " "),
            ] {
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
        // Punctuation after a value that runs past a window is absorbed with it,
        // wherever the window falls.
        for offset in (0..64).step_by(3) {
            let line = format!("{} token={value}, tail\n", "-".repeat(3000 + offset));
            let text = String::from_utf8(windowed(&redactor, line.as_bytes(), 8192, 1024)).unwrap();
            assert!(text.ends_with(" tail\n"), "{offset}");
            assert!(!text.contains(&value[value.len() - 16..]), "{offset}");
        }
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
        assert!(redactor().fingerprint().starts_with("r2-"));

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
        assert!(identifier_chain(b"ML_KEM_512_SHARED_SECRET_LENGTH"));
        assert!(identifier_chain(b"password.encode_utf16"));
        assert!(!identifier_chain(b"a8F3kQ9zR2"));
        for path in [
            &b"./target/debug"[..],
            b"/Users/Alice/MyProject/.secrets/creds-2024.json",
            b"models/llama-3.1-8b/tokenizer.model",
            b"C:/Users/Bob/AppData/Local/token.txt",
        ] {
            assert!(path_shaped(path), "{}", String::from_utf8_lossy(path));
        }
        assert!(!path_shaped(b"/x+049UNErhjZhmElZuZ1Y9tDRd6GtHUlsTizT2D"));
        assert!(!path_shaped(b"/xQ049UNErhjZhmElZuZ1Y9tDRd6GtHUlsTizT2D"));
        assert!(type_name(b"Ed25519PrivateKey") && type_name(b"&X25519SharedSecret"));
        assert!(!type_name(b"KeyLm2Qp7Rt4Vw8ZnB3c"));
        let line = b"f(token=abc123)), x";
        assert_eq!(trim(line, 8..15, false), 8..14);
        assert_eq!(trim(b"(a1)", 0..4, false), 0..4);
        assert_eq!(trim(b"abc),", 0..5, true), 0..5);
        assert_eq!(Prefix::new(b"id_rsa:12:").find(b"id_rsa-13-MIIE"), Some(10));
        assert_eq!(Prefix::new(b"     9\t").find(b"    10\tMIIE"), Some(7));
        assert_eq!(Prefix::new(b"+").find(b"-MIIE"), Some(1));
        assert_eq!(Prefix::new(b"12:").find(b"MIIE"), None);
        assert!(ends_key(
            base64(b"...\n-----END PRIVATE KEY-----\n").as_bytes()
        ));
        assert!(!ends_key(format!("{}done", base64(b"-----\n")).as_bytes()));
        assert_eq!(base64_bytes(br"ab\ncd\r=="), 6);
        assert!(entropy(b"aaaaaaaaaaaa") < 0.1);
    }
}
