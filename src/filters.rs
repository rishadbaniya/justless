//! Command selection and line filters shared by explicit runs and agent hooks.
use std::ffi::OsString;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use regex::Regex;
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Containers,
    Pods,
    Deployments,
    StatefulSets,
    Nodes,
    JsTests,
    Logs,
}

impl Profile {
    pub fn label(self) -> &'static str {
        match self {
            Self::Containers => "docker ps",
            Self::Pods => "kubectl get pods",
            Self::Deployments => "kubectl get deployments",
            Self::StatefulSets => "kubectl get statefulsets",
            Self::Nodes => "kubectl get nodes",
            Self::JsTests => "JavaScript tests",
            Self::Logs => "container logs",
        }
    }

    pub fn inventory(self) -> bool {
        !matches!(self, Self::JsTests | Self::Logs)
    }
}

// Accept display-oriented options explicitly for resource commands. A new flag or
// custom output format should reach the original command until we understand it.
fn display_options(mut args: &[&str], flags: &[&str], valued: &[&str], names: bool) -> bool {
    while let Some((&arg, rest)) = args.split_first() {
        args = rest;
        let (key, inline) = arg
            .split_once('=')
            .map_or((arg, None), |(k, v)| (k, Some(v)));
        if valued.contains(&key) {
            if inline.is_none() {
                let Some((&v, rest)) = args.split_first() else {
                    return false;
                };
                if v.starts_with('-') {
                    return false;
                }
                args = rest;
            }
        } else if !(flags.contains(&arg) || names && !arg.starts_with('-')) {
            return false;
        }
    }
    true
}

pub fn profile(words: &[&str]) -> Option<Profile> {
    let (&program, mut args) = words.split_first()?;
    if program == "kubectl" {
        // kubectl permits a namespace or context before or after the verb.
        while let Some((&arg, rest)) = args.split_first() {
            if ["--namespace", "-n", "--context"].contains(&arg)
                && rest.first().is_some_and(|v| !v.starts_with('-'))
            {
                args = &rest[1..];
            } else if arg.starts_with("--namespace=") || arg.starts_with("--context=") {
                args = rest;
            } else {
                break;
            }
        }
    }
    match (program, args) {
        ("docker", ["ps", tail @ ..] | ["container", "ls" | "list" | "ps", tail @ ..])
            if display_options(
                tail,
                &["-a", "--all", "--no-trunc", "--latest", "-l"],
                &["-f", "--filter", "-n", "--last"],
                false,
            ) =>
        {
            Some(Profile::Containers)
        }
        ("kubectl", ["get", resource, tail @ ..])
            if display_options(
                tail,
                &["-A", "--all-namespaces"],
                &[
                    "-n",
                    "--namespace",
                    "--context",
                    "-l",
                    "--selector",
                    "--field-selector",
                    "--sort-by",
                ],
                true,
            ) =>
        {
            match *resource {
                "po" | "pod" | "pods" => Some(Profile::Pods),
                "deploy" | "deployment" | "deployments" | "deployments.apps" => {
                    Some(Profile::Deployments)
                }
                "sts" | "statefulset" | "statefulsets" | "statefulsets.apps" => {
                    Some(Profile::StatefulSets)
                }
                "no" | "node" | "nodes" => Some(Profile::Nodes),
                _ => None,
            }
        }
        ("docker", ["logs", tail @ ..] | ["container", "logs", tail @ ..])
            if display_options(
                tail,
                &["-t", "--timestamps", "--details"],
                &["--since", "--until", "--tail", "-n"],
                true,
            ) =>
        {
            Some(Profile::Logs)
        }
        ("kubectl", ["logs", tail @ ..])
            if display_options(
                tail,
                &[
                    "-p",
                    "--previous",
                    "--timestamps",
                    "--all-containers",
                    "--prefix",
                ],
                &[
                    "--since",
                    "--since-time",
                    "--tail",
                    "-c",
                    "--container",
                    "-n",
                    "--namespace",
                    "--context",
                    "-l",
                    "--selector",
                ],
                true,
            ) =>
        {
            Some(Profile::Logs)
        }
        ("pnpm" | "npm" | "yarn" | "bun", ["test", ..] | ["run", "test", ..])
        | ("jest", _)
        | ("vitest", ["run", ..])
        | ("node", ["--test", ..])
            if js_display(args) =>
        {
            Some(Profile::JsTests)
        }
        _ => None,
    }
}

fn js_display(args: &[&str]) -> bool {
    !args.iter().any(|a| {
        let flag = a.split('=').next().unwrap_or(a);
        matches!(
            flag,
            "--watch"
                | "--watchAll"
                | "--watch-all"
                | "-w"
                | "--ui"
                | "--interactive"
                | "--inspect"
                | "--inspect-brk"
                | "--debug"
                | "--json"
                | "--reporter"
                | "--reporters"
                | "--test-reporter"
                | "--test-reporter-destination"
                | "--outputFile"
                | "--output-file"
                | "--listTests"
                | "--showConfig"
                | "--updateSnapshot"
                | "-u"
                | "--coverage"
                | "--help"
                | "--version"
        )
    })
}

pub fn legacy_hook(words: &[&str]) -> bool {
    matches!(
        words,
        ["cargo", "build" | "check" | "clippy" | "test", ..]
            | ["git", "diff" | "show" | "status" | "log", ..]
            | ["go", "test" | "build", ..]
            | ["python" | "python3", "-m", "pytest", ..]
            | ["pytest" | "rg", ..]
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub command: Vec<String>,
    pub omit: String,
}

impl Rule {
    pub fn validate(&self) -> Result<Regex> {
        ensure!(
            !self.command.is_empty() && self.command.iter().all(|s| !s.is_empty()),
            "filters.command needs a nonempty argv prefix"
        );
        let regex = Regex::new(&self.omit).context("invalid filters.omit regex")?;
        ensure!(
            !regex.is_match(""),
            "filters.omit must not match empty text"
        );
        Ok(regex)
    }
}

pub struct Selection {
    pub profile: Option<Profile>,
    pub omit: Option<Regex>,
}

impl Selection {
    pub fn for_run(args: &[OsString], rules: &[Rule]) -> Result<Self> {
        let mut words: Vec<_> = args.iter().map(|a| a.to_str().unwrap_or("")).collect();
        if let Some(program) = words.first_mut() {
            *program = Path::new(*program)
                .file_name()
                .and_then(|p| p.to_str())
                .unwrap_or("");
        }
        let rule = rules
            .iter()
            .filter(|r| {
                words.starts_with(&r.command.iter().map(String::as_str).collect::<Vec<_>>())
            })
            .max_by_key(|r| r.command.len());
        Ok(Self {
            profile: profile(&words),
            omit: rule.map(Rule::validate).transpose()?,
        })
    }
}

// Keep unrecognized evidence. For very large streams retain the first diagnostic and
// its following context independently of the head/tail preview.
pub struct LineFilter<'a> {
    config: &'a Selection,
    limit: usize,
    prefix: String,
    evidence: String,
    tail: std::collections::VecDeque<String>,
    tail_bytes: usize,
    retained: usize,
    omitted: usize,
    duplicate: usize,
    last: String,
    evidence_until: usize,
    clipped: bool,
    invalid: bool,
}

impl<'a> LineFilter<'a> {
    pub fn new(config: &'a Selection, limit: usize) -> Self {
        Self {
            config,
            limit,
            prefix: String::new(),
            evidence: String::new(),
            tail: Default::default(),
            tail_bytes: 0,
            retained: 0,
            omitted: 0,
            duplicate: 0,
            last: String::new(),
            evidence_until: 0,
            clipped: false,
            invalid: false,
        }
    }

    pub fn observe(&mut self, text: &str, n: usize) {
        if self.invalid
            || (self.config.omit.is_none()
                && !matches!(self.config.profile, Some(Profile::JsTests | Profile::Logs)))
        {
            return;
        }
        if text.len() >= 256 * 1024 || text.contains(['\0', '\u{fffd}']) {
            self.invalid = true;
            return;
        }
        let line = text.trim();
        let js = self.config.profile == Some(Profile::JsTests);
        let error = diagnostic(line);
        let context = n <= self.evidence_until;
        if error {
            self.evidence_until = n.saturating_add(4);
        }
        if !error && !context {
            let success = js
                && (line.starts_with("PASS ")
                    || line.starts_with("✓ ")
                    || line.starts_with("✔ ")
                    || line.starts_with("√ ")
                    || line.starts_with("ok ")
                        && line[3..].starts_with(|c: char| c.is_ascii_digit()));
            if success || self.config.omit.as_ref().is_some_and(|r| r.is_match(line)) {
                self.omitted += 1;
                self.last.clear();
                return;
            }
            if self.config.profile == Some(Profile::Logs) && !line.is_empty() && text == self.last {
                self.duplicate += 1;
                return;
            }
        }
        if self.config.profile == Some(Profile::Logs) {
            self.last = text.to_owned();
        }
        self.retained += 1;
        if !self.clipped && self.prefix.len() + text.len() <= self.limit {
            self.prefix.push_str(text);
        } else {
            self.clipped = true;
        }
        if error || context {
            let detail = format!("L{n} {}\n", line);
            append(&mut self.evidence, &detail, self.limit / 2);
        }
        let mut end = String::new();
        append(&mut end, text, self.limit / 4);
        self.tail_bytes += end.len();
        self.tail.push_back(end);
        while self.tail_bytes > self.limit / 4 || self.tail.len() > 32 {
            self.tail_bytes -= self.tail.pop_front().unwrap().len();
        }
    }

    pub fn finish(self) -> Option<Vec<u8>> {
        if self.invalid || self.omitted + self.duplicate == 0 {
            return None;
        }
        let mut report = format!(
            "[{} retained lines, {} filtered, {} repeats]\n",
            self.retained, self.omitted, self.duplicate
        );
        if self.clipped || report.len() + self.prefix.len() > self.limit {
            if !self.evidence.is_empty() {
                report.push_str(&self.evidence);
            }
            append(&mut report, &self.prefix, self.limit * 2 / 3);
            report.push_str("\n[… full stream in log …]\n");
            for line in self.tail {
                append(&mut report, &line, self.limit);
            }
        } else {
            report.push_str(&self.prefix);
        }
        Some(report.into_bytes())
    }
}

fn diagnostic(line: &str) -> bool {
    let text = line.to_ascii_lowercase();
    [
        "fail",
        "error",
        "panic",
        "warn",
        "exception",
        "expected:",
        "received:",
        "not ok",
        "assert",
        "fatal",
        "✗",
        "×",
        "❯",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn append(out: &mut String, text: &str, budget: usize) {
    let available = budget.saturating_sub(out.len()).min(text.len());
    out.push_str(&text[..text.floor_char_boundary(available)]);
}
