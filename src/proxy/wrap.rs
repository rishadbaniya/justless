//! `justless wrap claude`: Claude Code through the local proxy.

// On Windows, `wrap` is refused and only `justless proxy` runs.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};
use clap::ArgMatches;
use serde_json::Value;

use super::ledger::{Totals, short, thousands};
use super::{Proxy, Settings, default_upstream};
use crate::history::human_bytes;
use crate::workspace::Workspace;

/// Variables that send Claude Code to a cloud provider instead of the Claude API.
const PROVIDERS: [&str; 5] = [
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
];

/// Claude Code turns these direct-connection defaults off behind a custom base URL.
/// Setting them keeps a wrapped session's requests like an unwrapped one's, and the
/// hint headers tell the proxy each request's class.
pub const PARITY: [&str; 2] = [
    "CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING",
    "CLAUDE_CODE_GATEWAY_HINT_HEADERS",
];

/// Claude Code sends its requests through `HTTPS_PROXY` or `HTTP_PROXY` when one is
/// set, including requests for 127.0.0.1, unless `NO_PROXY` exempts them. Returns
/// the list with loopback added, or `None` when no proxy is set or loopback is
/// already exempt.
fn no_proxy() -> Option<String> {
    let set = |name: &str| env::var_os(name).is_some_and(|value| !value.is_empty());
    let proxied = [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .iter()
    .any(|name| set(name));
    if !proxied {
        return None;
    }
    let mut hosts: Vec<String> = Vec::new();
    for name in ["NO_PROXY", "no_proxy"] {
        for host in env::var(name).unwrap_or_default().split(',') {
            let host = host.trim();
            if !host.is_empty() && !hosts.iter().any(|h| h == host) {
                hosts.push(host.to_owned());
            }
        }
    }
    if hosts.iter().any(|h| h == "*") {
        return None;
    }
    let before = hosts.len();
    for host in ["127.0.0.1", "localhost"] {
        if !hosts.iter().any(|h| h == host) {
            hosts.push(host.to_owned());
        }
    }
    (hosts.len() > before).then(|| hosts.join(","))
}

fn truthy(name: &str) -> bool {
    env::var(name).is_ok_and(|value| {
        let value = value.trim().to_ascii_lowercase();
        !value.is_empty() && !matches!(value.as_str(), "0" | "false" | "no" | "off")
    })
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| executable(path))
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

/// Settings files that can route Claude Code around the proxy.
fn settings_warnings(cwd: &Path) -> Vec<String> {
    let user = match env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            Some((
                dir.join("settings.json"),
                format!("{}/settings.json", dir.display()),
            ))
        }
        None => env::var_os("HOME").map(|home| {
            (
                PathBuf::from(home).join(".claude/settings.json"),
                "~/.claude/settings.json".to_owned(),
            )
        }),
    };
    let project = [".claude/settings.json", ".claude/settings.local.json"]
        .map(|name| (cwd.join(name), name.to_owned()));
    user.into_iter()
        .chain(project)
        .filter_map(|(path, label)| {
            let value: Value = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
            let names: Vec<&str> = value["env"]
                .as_object()?
                .keys()
                .map(String::as_str)
                .filter(|key| *key == "ANTHROPIC_BASE_URL" || key.starts_with("CLAUDE_CODE_USE_"))
                .collect();
            (!names.is_empty()).then(|| {
                format!(
                    "{label} sets {}; Claude Code may bypass the proxy.",
                    names.join(" and ")
                )
            })
        })
        .collect()
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", thousands(n), if n == 1 { one } else { many })
}

fn summary(totals: &Totals, archive: Option<&Path>) -> String {
    if totals.seen == 0 {
        return "justless: the proxy received no requests. A settings file or managed policy may route Claude Code elsewhere.".into();
    }
    let mut parts = vec![
        plural(totals.requests, "request", "requests"),
        format!(
            "{} ({} on an expired cache)",
            plural(totals.moves, "clear", "clears"),
            thousands(totals.cold_moves)
        ),
        plural(
            totals.cleared_results,
            "old tool result marked for clearing",
            "old tool results marked for clearing",
        ),
        format!(
            "{} input tokens not sent to the model",
            short(totals.tokens_not_sent)
        ),
    ];
    if totals.mismatches > 0 {
        parts.push(format!(
            "the API cleared a different number of calls on {}",
            plural(totals.mismatches, "request", "requests")
        ));
    }
    if let Some(archive) = archive {
        parts.push(format!(
            "{} in {}",
            plural(
                totals.archived,
                "tool result archived",
                "tool results archived"
            ),
            archive.display()
        ));
    }
    if totals.client_managed > 0 {
        parts.push(format!(
            "Claude Code cleared tool results itself in {}",
            plural(totals.client_managed, "conversation", "conversations")
        ));
    }
    if totals.live_compacted > 0 {
        parts.push(format!(
            "{} in the live zone ({} saved)",
            plural(
                totals.live_compacted,
                "newest tool result compacted",
                "newest tool results compacted"
            ),
            human_bytes(totals.live_saved_bytes)
        ));
    }
    format!("justless: {}", parts.join(" · "))
}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

/// Claude Code handles Ctrl-C itself, and the terminal sends it to the whole
/// process group. The wrapper ignores it so the proxy outlives the session. It is
/// set after the spawn, because an ignored signal is inherited across exec.
#[cfg(unix)]
fn ignore_terminal_signals() {
    // SAFETY: setting a signal disposition to SIG_IGN has no other effect.
    unsafe {
        for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGHUP] {
            libc::signal(signal, libc::SIG_IGN);
        }
    }
}

/// Every argument after `claude`, exactly as given.
fn claude_args(args: &ArgMatches) -> Result<Vec<OsString>> {
    match args.subcommand() {
        Some(("claude", sub)) => Ok(sub
            .values_of_os("")
            .map(|values| values.map(OsString::from).collect())
            .unwrap_or_default()),
        Some((other, _)) => bail!(
            "wrap launches Claude Code: justless wrap [OPTIONS] claude [CLAUDE_ARGS]... (not {other})"
        ),
        None => bail!("usage: justless wrap [OPTIONS] claude [CLAUDE_ARGS]..."),
    }
}

#[cfg(not(unix))]
pub fn run(_ws: &Workspace, _args: &ArgMatches, _settings: Settings) -> Result<i32> {
    bail!(
        "wrap is not supported on Windows. Run `justless proxy` and set ANTHROPIC_BASE_URL for Claude Code to the address it prints."
    )
}

#[cfg(unix)]
pub fn run(ws: &Workspace, args: &ArgMatches, settings: Settings) -> Result<i32> {
    let claude_args = claude_args(args)?;
    if env::var_os("JUSTLESS_PROXY").is_some() {
        bail!("this session already runs through a Justless proxy (JUSTLESS_PROXY is set)");
    }
    if let Some(name) = PROVIDERS.iter().find(|name| truthy(name)) {
        bail!("wrap needs the Claude API, but {name} is set. Run claude directly.");
    }
    let Some(claude) = find_on_path("claude") else {
        bail!(
            "claude is not on PATH. Install Claude Code, or point another client at `justless proxy`."
        );
    };
    let mut options = settings.options;
    let betas_off = truthy("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
    if betas_off {
        options.clear = false;
    }
    for warning in settings_warnings(&ws.cwd) {
        eprintln!("justless: {warning}");
    }
    let upstream = default_upstream();
    let clearing = if betas_off {
        "archive only, because CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS is set".to_owned()
    } else if !options.clear {
        "archive only (--no-clear)".to_owned()
    } else {
        format!(
            "old tool results cleared from {} tokens",
            short(options.floor)
        )
    };
    let live = if options.compact_live && settings.archive {
        " · live-zone compaction on"
    } else {
        ""
    };
    let proxy = Proxy::start(ws, 0, &upstream, options, settings.archive, false)?;
    let addr = proxy.addr();
    let archive = if settings.archive {
        format!("archive {}", ws.state.display())
    } else {
        "archive off".to_owned()
    };
    eprintln!(
        "justless: Claude Code → {addr} → {} · {clearing}{live} · {archive} · {} stats shows what changed",
        proxy.upstream(),
        ws.justless()
    );
    let mut command = Command::new(&claude);
    command
        .args(&claude_args)
        .env("ANTHROPIC_BASE_URL", format!("http://{addr}"))
        .env("JUSTLESS_PROXY", "1");
    for name in PARITY {
        if env::var_os(name).is_none() {
            command.env(name, "1");
        }
    }
    if let Some(hosts) = no_proxy() {
        command.env("NO_PROXY", &hosts).env("no_proxy", &hosts);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            proxy.stop();
            bail!("start {}: {error}", claude.display());
        }
    };
    ignore_terminal_signals();
    let status = child.wait();
    let totals = proxy.stop();
    eprintln!(
        "{}",
        summary(&totals, settings.archive.then_some(ws.state.as_path()))
    );
    Ok(exit_code(status?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_a_session() {
        let mut totals = Totals {
            seen: 230,
            requests: 214,
            moves: 4,
            cold_moves: 3,
            cleared_results: 412,
            tokens_not_sent: 2_910_000,
            mismatches: 0,
            archived: 512,
            client_managed: 2,
            live_compacted: 0,
            live_saved_bytes: 0,
        };
        assert_eq!(
            summary(&totals, Some(Path::new("/repo/.justless"))),
            "justless: 214 requests · 4 clears (3 on an expired cache) · 412 old tool results marked for clearing · 2.91M input tokens not sent to the model · 512 tool results archived in /repo/.justless · Claude Code cleared tool results itself in 2 conversations"
        );
        totals.mismatches = 3;
        assert!(summary(&totals, None).contains(
            "not sent to the model · the API cleared a different number of calls on 3 requests"
        ));
        assert!(summary(&Totals::default(), None).contains("received no requests"));
        // Probes alone show that Claude Code reached the proxy.
        let probes = Totals {
            seen: 2,
            ..Totals::default()
        };
        assert!(summary(&probes, None).starts_with("justless: 0 requests"));
        totals.live_compacted = 3;
        totals.live_saved_bytes = 48_000;
        assert!(
            summary(&totals, None).contains("3 newest tool results compacted in the live zone"),
            "{}",
            summary(&totals, None)
        );
    }
}
