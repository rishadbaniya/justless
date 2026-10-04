//! What a learned rule may contain. A command that looks like it carries a credential
//! drops its whole correction; nothing is masked. Rule slots are allowlisted tokens.
//!
//! The token formats are adapted from gitleaks (MIT License, Copyright (c) 2019
//! Zachary Rice): <https://github.com/gitleaks/gitleaks/blob/master/config/gitleaks.toml>.
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

static TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?:A3T[A-Z0-9]|AKIA|ASIA|ABIA|ACCA)[A-Z2-7]{16}\b",
        r"|\bgh[pousr]_[0-9A-Za-z]{36}\b",
        r"|github_pat_\w{22,}",
        r"|glpat-[\w-]{20}",
        r"|xox[abposr]-[0-9A-Za-z-]{10,}",
        r"|xapp-\d-",
        r"|\bsk-(?:ant-|proj-|svcacct-|admin-)?[A-Za-z0-9_-]{20,}",
        r"|\b[sr]k_(?:live|test|prod)_[A-Za-z0-9]{10,}",
        r"|\bnpm_[A-Za-z0-9]{36}\b",
        r"|pypi-AgEIcHlwaS5vcmc",
        r"|\bAIza[\w-]{35}\b",
        r"|\bey[A-Za-z0-9_-]{10,}\.ey[A-Za-z0-9_-]{10,}\.",
        r"|-----BEGIN [A-Z ]*PRIVATE KEY",
        r"|hooks\.slack\.com/",
    ))
    .expect("token patterns")
});
static ADDRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        // user@host and e-mail addresses; a version such as pkg@1.2 is not a host.
        r"[A-Za-z0-9._%+-]@[A-Za-z][A-Za-z0-9-]*",
        r"|\b(?:\d{1,3}\.){3}\d{1,3}\b",
    ))
    .expect("address patterns")
});
/// npm dist-tags, as in `left-pad@latest`, which name a release rather than a host.
const DIST_TAGS: &[&str] = &[
    "latest", "next", "beta", "alpha", "canary", "rc", "stable", "lts", "nightly",
];

fn address(text: &str) -> bool {
    ADDRESS.find_iter(text).any(|found| {
        let Some((_, host)) = found.as_str().split_once('@') else {
            return true;
        };
        !DIST_TAGS.contains(&host) || text[found.end()..].starts_with('.')
    })
}

static SCOPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@[a-z0-9][a-z0-9._-]*/[a-z0-9][a-z0-9._-]*$").expect("npm scope pattern")
});
static TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_@%+=:,./-]{1,64}$").expect("slot pattern"));

const KEYWORDS: &[&str] = &[
    "pass",
    "pwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "access_key",
    "private_key",
    "credential",
    "creds",
    "auth",
    "cookie",
    "session_id",
    "bearer",
    "header",
];
const HEADERS: &[&str] = &["authorization", "cookie", "token", "api-key"];
const NETWORK: &[&str] = &[
    "ssh",
    "scp",
    "sftp",
    "rsync",
    "curl",
    "wget",
    "http",
    "xh",
    "nc",
    "telnet",
    "ftp",
    "mysql",
    "psql",
    "mongo",
    "mongosh",
    "redis-cli",
    "aws",
    "gcloud",
    "az",
    "kubectl",
    "helm",
    "doctl",
    "flyctl",
    "heroku",
    "vercel",
    "netlify",
    "vault",
    "op",
    "gpg",
    "pass",
    "security",
    "openssl",
    "ssh-add",
    "ssh-keygen",
];
const WRAPPERS: &[&str] = &["sudo", "doas", "env", "time", "nohup", "command", "exec"];

/// Whether a raw command, with its stripped `NAME=value` words, may carry a credential,
/// an address, a path outside the workspace or a network tool.
pub fn unsafe_command(raw: &str, env: &[String], root: &Path) -> bool {
    screen(raw, env, Some(root))
}

/// Whether a command may carry a credential or an address, or runs a network tool,
/// whatever its paths. A zsh glob error is the local shell's wherever it ran.
pub fn carries_credential(raw: &str, env: &[String]) -> bool {
    screen(raw, env, None)
}

fn screen(raw: &str, env: &[String], root: Option<&Path>) -> bool {
    if TOKENS.is_match(raw) || raw.contains("://") || address(raw) {
        return true;
    }
    let lower = raw.to_ascii_lowercase();
    if lower.contains("authorization:") || lower.contains("bearer ") {
        return true;
    }
    let words =
        shlex::split(raw).unwrap_or_else(|| raw.split_whitespace().map(str::to_owned).collect());
    if env.iter().chain(&words).any(|word| secret_assignment(word)) {
        return true;
    }
    // A script passed as one word, as in `sh -c '...'`, is screened like a command.
    // Each level is shorter than the last, so this ends.
    if words.iter().any(|word| {
        word.len() < raw.len() && word.contains(char::is_whitespace) && screen(word, &[], root)
    }) {
        return true;
    }
    let root = root.map(Path::to_string_lossy);
    for (index, word) in words.iter().enumerate() {
        let next = words.get(index + 1).map(String::as_str);
        if let Some(name) = word.strip_prefix('-') {
            let (name, value) = match name.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (name, next.filter(|value| !value.starts_with('-'))),
            };
            if value.is_some_and(|value| !value.is_empty()) && keyword(name) {
                return true;
            }
            if matches!(word.as_str(), "-H" | "--header")
                && next.is_some_and(|value| {
                    let value = value.to_ascii_lowercase();
                    HEADERS.iter().any(|header| value.contains(header))
                })
            {
                return true;
            }
        }
        for part in std::iter::once(word.as_str()).chain(word.split_once('=').map(|(_, v)| v)) {
            if random(part) {
                return true;
            }
            if let Some(root) = &root
                && (part.starts_with('~')
                    || part.starts_with('/')
                        && part != root
                        && !part.starts_with(&format!("{root}/"))
                        && !part.starts_with("/dev/null"))
            {
                return true;
            }
        }
    }
    network(&words)
}

/// Whether a value may be substituted into a rule's text.
pub fn safe_token(token: &str) -> bool {
    TOKEN.is_match(token)
        && !token.starts_with('/')
        && !token.split('/').any(|part| part == "..")
        && !token.contains("://")
        && (!token.contains('@') || SCOPE.is_match(token))
        && !random(token)
}

/// Whether a finished rule is one short line that cannot open or close markup.
pub fn safe_rule_text(text: &str) -> bool {
    text.len() <= 160
        && !text.contains(['\n', '\r'])
        && !text.contains("<!--")
        && !text.contains("-->")
        && text.matches('`').count().is_multiple_of(2)
}

fn keyword(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    KEYWORDS.iter().any(|keyword| name.contains(keyword))
}

/// `NAME=value`, including dotted configuration keys such as git's `-c a.b=value`.
fn secret_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, value)| {
        !name.is_empty()
            && !name.starts_with('-')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            && !value.is_empty()
            && keyword(name)
    })
}

/// A long word of key-like characters with letters, digits and high entropy. Hex
/// hashes stay under 4.0 bits per character.
fn random(word: &str) -> bool {
    word.len() >= 20
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+/=_-".contains(&b))
        && word.bytes().any(|b| b.is_ascii_alphabetic())
        && word.bytes().any(|b| b.is_ascii_digit())
        && entropy(word) > 4.0
}

fn entropy(word: &str) -> f64 {
    let mut counts = [0u32; 256];
    for byte in word.bytes() {
        counts[usize::from(byte)] += 1;
    }
    let length = word.len() as f64;
    counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let p = f64::from(count) / length;
            -p * p.log2()
        })
        .sum()
}

/// A network or credential program at the start of any piece of the command.
fn network(words: &[String]) -> bool {
    let mut start = true;
    let mut index = 0;
    while index < words.len() {
        let word = words[index].as_str();
        if matches!(word, "&&" | "||" | ";" | "|") {
            start = true;
        } else if start {
            let name = word.rsplit('/').next().unwrap_or(word);
            let next = words.get(index + 1).map(String::as_str);
            if WRAPPERS.contains(&name) || secret_assignment(word) || word.contains('=') {
                index += 1;
                continue;
            }
            if NETWORK.contains(&name)
                || matches!(
                    (name, next),
                    ("docker", Some("login")) | ("gh", Some("auth")) | ("npm", Some("login"))
                )
            {
                return true;
            }
            start = false;
        }
        index += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_commands_with_credentials() {
        let root = Path::new("/work/repo");
        for raw in [
            "API_TOKEN=abc123 make test",
            "./deploy --password hunter2",
            "git clone https://u:p@example.invalid/x",
            &format!(
                "gh pr list --repo {}",
                "ghp_".to_owned() + &"a1B2".repeat(9)
            ),
            "make test ARG=Zx8Qk2Lp9Vr4Tn7Wm3Yb6Hc1",
            "cat /home/someone/x",
            "ssh host make",
            "curl -H 'Authorization: x' localhost",
            "ping 10.0.0.1",
            "git push git@github.com",
            "ls ~/secrets",
            "git -c http.extraHeader=X-Thing:abc --version",
            "git -c 'http.extraHeader=Authorization: Bearer abc' fetch",
            "cargo publish --token abc",
            "mail me@latest.example",
            "sh -c 'true && API_TOKEN=x make'",
            "bash -c \"sh -c 'make --password hunter2'\"",
            "sh -c 'cat /home/someone/x'",
        ] {
            assert!(unsafe_command(raw, &[], root), "{raw}");
        }
        for raw in [
            "make test",
            "cargo test -p core token_parser",
            "cd /work/repo/server && make",
            "git log --oneline -n 5 2>/dev/null",
            "npm install left-pad@1.3.0",
            "npm install left-pad@latest @types/node@next",
            "sh -c 'cd server && make test'",
            "git rev-parse 3f9a1c2e7b40d3c1a8e9f0b2c4d6e8f0a1b3c5d7",
        ] {
            assert!(!unsafe_command(raw, &[], root), "{raw}");
        }
        assert!(unsafe_command("make", &["DB_PASSWORD=x".into()], root));
        // Paths outside the workspace count only for unsafe_command.
        assert!(!carries_credential("cd /srv/other && ls *.log", &[]));
        assert!(carries_credential("ssh host ls *.log", &[]));
        assert!(carries_credential("TOKEN=x ls *.log", &[]));
    }

    #[test]
    fn allowlists_rule_slots_and_text() {
        for token in [
            "make",
            "test:unit",
            "@scope/pkg",
            "server/api",
            "--include",
            "-p",
        ] {
            assert!(safe_token(token), "{token}");
        }
        for token in [
            "",
            "/etc",
            "../x",
            "x-->y",
            "a`b",
            "user@host",
            "http://x",
            "Zx8Qk2Lp9Vr4Tn7Wm3Yb6Hc1",
        ] {
            assert!(!safe_token(token), "{token}");
        }
        assert!(safe_rule_text("Run `make` from `server/`."));
        assert!(!safe_rule_text("Run `make from `server/`."));
        assert!(!safe_rule_text("a <!-- b"));
        assert!(!safe_rule_text(&"x".repeat(161)));
    }
}
