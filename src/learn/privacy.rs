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

/// Whether an option or build-arg name looks like it carries a credential or secret.
pub fn is_secret_name(name: &str) -> bool {
    keyword(name)
}

fn contains_ascii_ignore_case(haystack: &str, needle: &[u8]) -> bool {
    let bytes = haystack.as_bytes();
    if needle.is_empty() {
        return true;
    }
    bytes
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn is_shell_op(word: &str) -> bool {
    matches!(word, "&&" | "||" | ";" | "|")
}

/// Split `raw` on unquoted shell control operators (`&&`, `||`, `|&`, `;`, `|`, `\n`)
/// before shell word-splitting each piece so unspaced chains like `true&&terraform login`
/// and `sh -c 'true&&kubectl get secrets'` still produce separate command words.
fn split_shell_words(raw: &str) -> Vec<String> {
    let mut words = Vec::new();
    let bytes = raw.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut seg_start = 0;
    let mut i = 0;
    let push_segment = |segment: &str, out: &mut Vec<String>| {
        if segment.trim().is_empty() {
            return;
        }
        let parsed = shlex::split(segment)
            .unwrap_or_else(|| segment.split_whitespace().map(str::to_owned).collect());
        out.extend(parsed);
    };
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if !in_single && b == b'\\' {
            escaped = true;
            i += 1;
            continue;
        }
        if !in_double && b == b'\'' {
            in_single = !in_single;
            i += 1;
            continue;
        }
        if !in_single && b == b'"' {
            in_double = !in_double;
            i += 1;
            continue;
        }
        if !in_single && !in_double {
            let op = if bytes.get(i..i + 2) == Some(b"&&") {
                Some(("&&", 2))
            } else if bytes.get(i..i + 2) == Some(b"||") {
                Some(("||", 2))
            } else if bytes.get(i..i + 2) == Some(b"|&") {
                Some(("|", 2))
            } else if b == b';' || b == b'\n' {
                Some((";", 1))
            } else if b == b'|' {
                Some(("|", 1))
            } else {
                None
            };
            if let Some((tok, len)) = op {
                push_segment(&raw[seg_start..i], &mut words);
                words.push(tok.to_owned());
                i += len;
                seg_start = i;
                continue;
            }
        }
        i += 1;
    }
    push_segment(&raw[seg_start..], &mut words);
    words
}

fn screen(raw: &str, env: &[String], root: Option<&Path>) -> bool {
    if TOKENS.is_match(raw) || raw.contains("://") || address(raw) {
        return true;
    }
    if contains_ascii_ignore_case(raw, b"authorization:")
        || contains_ascii_ignore_case(raw, b"bearer ")
    {
        return true;
    }
    let words = split_shell_words(raw);
    if env.iter().chain(&words).any(|word| secret_assignment(word)) {
        return true;
    }
    // A script passed as one word, as in `sh -c '...'`, is screened like a command.
    // Each level is shorter than the last, so this ends.
    if words.iter().any(|word| {
        word.len() < raw.len()
            && (word.contains(char::is_whitespace)
                || word.contains("&&")
                || word.contains("||")
                || word.contains([';', '|']))
            && screen(word, &[], root)
    }) {
        return true;
    }
    let root = root.map(Path::to_string_lossy);
    for (index, word) in words.iter().enumerate() {
        let next = words.get(index + 1).map(String::as_str);
        let next2 = words.get(index + 2).map(String::as_str);
        let non_op_next = next.filter(|value| !value.is_empty() && !is_shell_op(value));

        // Catch spaced `=` assignments such as `db_password = hunter2`,
        // `-var=db_password = hunter2`, `db_password= hunter2`, or `db_password =hunter2`.
        let lhs = word
            .strip_prefix('-')
            .and_then(|rest| rest.split_once('=').map(|(_, v)| v))
            .unwrap_or(word.as_str());
        if next == Some("=")
            && let Some(val) = next2.filter(|v| !v.is_empty() && !is_shell_op(v))
            && secret_assignment(&format!("{lhs}={val}"))
        {
            return true;
        }
        if lhs.ends_with('=')
            && let Some(val) = non_op_next
            && secret_assignment(&format!("{lhs}{val}"))
        {
            return true;
        }
        if let Some(val) = non_op_next.and_then(|v| v.strip_prefix('='))
            && !val.is_empty()
            && secret_assignment(&format!("{lhs}={val}"))
        {
            return true;
        }

        if let Some(name) = word.strip_prefix('-') {
            let (name, value) = match name.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => {
                    if keyword(name) {
                        // A secret-taking flag followed by ANY non-empty value token
                        // carries a secret, even when that secret starts with `-`.
                        (name, non_op_next)
                    } else {
                        (name, non_op_next.filter(|value| !value.starts_with('-')))
                    }
                }
            };
            if value.is_some_and(|value| !value.trim().is_empty()) && keyword(name) {
                return true;
            }
            // Catch `--build-arg=API_TOKEN=val`, `--set=a=1,db.password=val`, `--env=DB_PASSWORD=val`.
            if value.is_some_and(secret_assignment) {
                return true;
            }
            if matches!(word.as_str(), "-H" | "--header")
                && non_op_next.is_some_and(|value| {
                    HEADERS
                        .iter()
                        .any(|header| contains_ascii_ignore_case(value, header.as_bytes()))
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
                        && part != root.as_ref()
                        && part
                            .strip_prefix(root.as_ref())
                            .is_none_or(|rest| !rest.starts_with('/'))
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
    KEYWORDS
        .iter()
        .any(|keyword| contains_ascii_ignore_case(name, keyword.as_bytes()))
}

fn is_secret_kv(segment: &str) -> bool {
    segment.split_once('=').is_some_and(|(name, value)| {
        let name = name.trim();
        let value = value.trim();
        !name.is_empty()
            && !name.starts_with('-')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '[' | ']'))
            && !value.is_empty()
            && keyword(name)
    })
}

/// `NAME=value`, including dotted configuration keys such as git's `-c a.b=value`,
/// comma-separated lists such as helm's `--set a=1,db.password=hunter2`, and
/// whitespace around `=`.
fn secret_assignment(word: &str) -> bool {
    is_secret_kv(word) || word.split(',').any(is_secret_kv)
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

/// First subcommand after leading global flags (and their values) for programs
/// whose auth/login subcommands may be preceded by options such as `-n prod`,
/// `--kube-context prod`, `-chdir=infra`, or `--context prod`.
fn first_subcommand<'a>(prog: &str, args: &'a [String]) -> Option<(&'a str, usize)> {
    let takes_value = |flag: &str| -> bool {
        match prog {
            "kubectl" => matches!(
                flag,
                "-n" | "--namespace"
                    | "--context"
                    | "--kubeconfig"
                    | "--cluster"
                    | "--user"
                    | "-s"
                    | "--server"
                    | "--as"
                    | "--as-group"
                    | "--as-uid"
                    | "--certificate-authority"
                    | "--client-certificate"
                    | "--client-key"
                    | "--token"
                    | "--username"
                    | "--password"
                    | "--cache-dir"
                    | "--request-timeout"
                    | "--tls-server-name"
                    | "--profile"
                    | "--profile-output"
                    | "-v"
                    | "--v"
                    | "--vmodule"
                    | "--log-dir"
                    | "--log-file"
            ),
            "helm" => matches!(
                flag,
                "-n" | "--namespace"
                    | "--kube-context"
                    | "--kubeconfig"
                    | "--kube-apiserver"
                    | "--kube-as-user"
                    | "--kube-as-group"
                    | "--kube-ca-file"
                    | "--kube-tls-server-name"
                    | "--kube-token"
                    | "--registry-config"
                    | "--repository-cache"
                    | "--repository-config"
                    | "--burst-limit"
                    | "--qps"
            ),
            "terraform" | "tofu" | "terragrunt" => matches!(
                flag,
                "-chdir"
                    | "--chdir"
                    | "--terragrunt-working-dir"
                    | "--working-dir"
                    | "--terragrunt-config"
                    | "--config"
                    | "--terragrunt-iam-role"
                    | "--iam-role"
                    | "--terragrunt-download-dir"
                    | "--download-dir"
                    | "--terragrunt-tfpath"
                    | "--tf-path"
                    | "--log-level"
                    | "--log-format"
            ),
            "docker" | "podman" => matches!(
                flag,
                "-c" | "--context"
                    | "-H"
                    | "--host"
                    | "--config"
                    | "-l"
                    | "--log-level"
                    | "--tlscacert"
                    | "--tlscert"
                    | "--tlskey"
                    | "--connection"
                    | "--identity"
                    | "--url"
                    | "--root"
                    | "--runroot"
                    | "--tmpdir"
                    | "--storage-driver"
                    | "--storage-opt"
                    | "--runtime"
                    | "--network-cmd-path"
                    | "--events-backend"
                    | "--cgroup-manager"
                    | "--namespace"
                    | "--volumepath"
            ),
            "npm" => matches!(
                flag,
                "-C" | "--prefix"
                    | "--userconfig"
                    | "--globalconfig"
                    | "--registry"
                    | "--cache"
                    | "--loglevel"
            ),
            _ => false,
        }
    };
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let s = arg.as_str();
        if s == "--" {
            return args.get(i + 1).map(|w| (w.as_str(), i + 1));
        }
        if s.starts_with('-') {
            i += 1;
            if !s.contains('=') && takes_value(s) {
                i += 1;
            }
            continue;
        }
        return Some((s, i));
    }
    None
}

fn is_kubectl_secret_arg(arg: &str) -> bool {
    if arg.starts_with('-') {
        return false;
    }
    arg.split(',').any(|item| {
        let resource = item.split(['/', '.']).next().unwrap_or("");
        resource.eq_ignore_ascii_case("secret") || resource.eq_ignore_ascii_case("secrets")
    })
}

fn skip_wrapper_flags(wrapper: &str, words: &[String], mut index: usize) -> usize {
    while let Some(arg) = words.get(index).map(String::as_str) {
        if is_shell_op(arg) {
            break;
        }
        if arg == "--" {
            index += 1;
            break;
        }
        if wrapper == "env" && !arg.starts_with('-') && arg.contains('=') {
            index += 1;
            continue;
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        index += 1;
        let takes_val = match wrapper {
            "env" => matches!(
                arg,
                "-u" | "--unset" | "-C" | "--chdir" | "-S" | "--split-string"
            ),
            "sudo" | "doas" => matches!(arg, "-u" | "-g" | "-C" | "-h" | "-p" | "-r" | "-t"),
            "exec" => arg == "-a",
            _ => false,
        };
        if takes_val && words.get(index).is_some_and(|next| !is_shell_op(next)) {
            index += 1;
        }
    }
    index
}

/// A network or credential program at the start of any piece of the command.
fn network(words: &[String]) -> bool {
    let mut start = true;
    let mut index = 0;
    while index < words.len() {
        let word = words[index].as_str();
        if is_shell_op(word) {
            start = true;
        } else if start {
            let trimmed = word.trim_start_matches(['(', '{']);
            if trimmed.is_empty() || trimmed == "!" {
                index += 1;
                continue;
            }
            let name = trimmed
                .rsplit('/')
                .next()
                .unwrap_or(trimmed)
                .trim_end_matches([')', '}']);
            if WRAPPERS.contains(&name) {
                index = skip_wrapper_flags(name, words, index + 1);
                continue;
            }
            if secret_assignment(trimmed) || trimmed.contains('=') {
                index += 1;
                continue;
            }
            let mut end = index + 1;
            while end < words.len() && !is_shell_op(words[end].as_str()) {
                end += 1;
            }
            let args = &words[index + 1..end];
            let sub = first_subcommand(name, args);
            if NETWORK.contains(&name)
                || matches!(
                    (name, sub.map(|(s, _)| s)),
                    ("docker" | "podman", Some("login"))
                        | ("gh", Some("auth"))
                        | ("npm", Some("login"))
                        | ("helm", Some("login" | "registry"))
                        | ("terraform" | "tofu" | "terragrunt", Some("login"))
                        | ("kubectl", Some("auth"))
                )
            {
                return true;
            }
            // Drop kubectl invocations that read, write or configure secrets/credentials
            // or dump raw kubeconfig credentials via `kubectl config view --raw`.
            if name == "kubectl" {
                if let Some(("config", sub_idx)) = sub {
                    let rest = &args[sub_idx + 1..];
                    let has_view = rest
                        .iter()
                        .any(|a| !a.starts_with('-') && a.eq_ignore_ascii_case("view"));
                    let has_raw = rest.iter().any(|a| {
                        a == "--raw"
                            || (a.starts_with("--raw=") && !a.eq_ignore_ascii_case("--raw=false"))
                    });
                    if has_view && has_raw {
                        return true;
                    }
                }
                if args.iter().any(|arg| {
                    arg.eq_ignore_ascii_case("set-credentials") || is_kubectl_secret_arg(arg)
                }) {
                    return true;
                }
            }
            start = false;
            index = end;
            continue;
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
            "kubectl get secrets -n prod",
            "kubectl create secret generic app --from-literal=a=b",
            "kubectl auth can-i get pods",
            "kubectl -n prod auth can-i get pods",
            "kubectl --namespace=prod auth whoami",
            "kubectl get secret/db-creds",
            "kubectl get secrets/tls-cert",
            "kubectl get secret,configmap",
            "kubectl get pods,secrets,configmaps",
            "kubectl get secret.v1/db-creds",
            "kubectl config set-credentials admin",
            "kubectl config view --raw",
            "kubectl --context prod config view --raw",
            "helm registry login registry.example",
            "helm --kube-context prod registry login registry.example",
            "helm -n prod --debug registry login",
            "helm upgrade app ./chart --set a=1,db.password=hunter2",
            "helm upgrade app ./chart --set=a=1,db.password=hunter2",
            "helm upgrade app ./chart --set 'a=1, db.password = hunter2'",
            "terraform login",
            "terraform -chdir=infra login",
            "tofu -chdir=infra login",
            "terragrunt --working-dir infra login",
            "terraform apply -var 'db_password = hunter2'",
            "terraform apply -var=\"db_password = hunter2\"",
            "terraform apply -var=db_password = hunter2",
            "terraform apply -var db_password = hunter2",
            "terraform apply -var db_password= hunter2",
            "terraform apply -var db_password =hunter2",
            "docker --context prod login",
            "docker -c prod -D login registry.example",
            "docker build --build-arg=API_TOKEN=hunter2 .",
            "cargo publish --token -hunter2",
            "./deploy --password -hunter2",
            "./cli --secret -hunter2",
            "./cli --api-key -hunter2",
            "./cli --passphrase -hunter2",
            "sh -c 'true&&kubectl get secrets'",
            "true&&terraform login&&terraform init",
            "echo ok;ssh host",
            "true||docker login",
            "env -u FOO terraform login",
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
            "echo 'a&&b' && make test",
            "git rev-parse 3f9a1c2e7b40d3c1a8e9f0b2c4d6e8f0a1b3c5d7",
            "kubectl -n staging get pods",
            "kubectl get secretstore",
            "kubectl config view",
            "helm dependency update ./charts/app",
            "helm upgrade app ./chart --set a=1,replicas=3",
            "terraform -chdir=infra plan",
            "docker build --build-arg APP_ENV=staging .",
            "docker --context prod build --build-arg APP_ENV=staging .",
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
