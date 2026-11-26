//! `justless exec`: run a short script over data too large to read, so that only what
//! the script prints reaches the agent. The streams go through the same capture as
//! `run`, which archives them under a run ID and bounds each preview.
//!
//! This isolates output, not code: scripts run with the caller's permissions.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Arg, ArgMatches};
use regex::Regex;

use crate::capture::{self, Shape};
use crate::workspace::Workspace;

/// Largest script read from stdin. Data belongs in `--file`.
const STDIN_LIMIT: u64 = 1024 * 1024;
/// Largest stdout read into memory for `logview` / `stacktrace` shaping.
const TEXT_SHAPE_LIMIT: u64 = 2 * 1024 * 1024;
/// Time between SIGINT and SIGKILL once the limit passes.
const GRACE: Duration = Duration::from_secs(2);
/// How long processes the script left running, such as the reader of `> >(sort)`, may
/// keep writing after it exits.
const DRAIN: Duration = Duration::from_secs(2);
const LANGUAGES: &str = "sh, bash, python3, node, ruby, perl, deno, bun";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lang {
    Sh,
    Bash,
    Python3,
    Node,
    Ruby,
    Perl,
    Deno,
    Bun,
}

impl Lang {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "sh" => Self::Sh,
            "bash" => Self::Bash,
            "python3" | "python" => Self::Python3,
            "node" | "js" => Self::Node,
            "ruby" => Self::Ruby,
            "perl" => Self::Perl,
            "deno" => Self::Deno,
            "bun" => Self::Bun,
            _ => return None,
        })
    }

    /// The language of an interpreter named on a `#!` line.
    fn from_interpreter(name: &str) -> Option<Self> {
        Some(match name {
            "sh" | "dash" | "ash" => Self::Sh,
            "bash" => Self::Bash,
            "python" | "python3" => Self::Python3,
            _ if name.strip_prefix("python3.").is_some_and(|minor| {
                !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit())
            }) =>
            {
                Self::Python3
            }
            "node" | "nodejs" => Self::Node,
            "ruby" => Self::Ruby,
            "perl" => Self::Perl,
            "deno" => Self::Deno,
            "bun" => Self::Bun,
            _ => return None,
        })
    }

    fn from_extension(path: &Path) -> Option<Self> {
        Some(match path.extension()?.to_str()? {
            "sh" => Self::Sh,
            "bash" => Self::Bash,
            "py" => Self::Python3,
            "js" | "mjs" | "cjs" => Self::Node,
            "rb" => Self::Ruby,
            "pl" => Self::Perl,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Self::Sh => "sh",
            Self::Bash => "bash",
            Self::Python3 => "python3",
            Self::Node => "node",
            Self::Ruby => "ruby",
            Self::Perl => "perl",
            Self::Deno => "deno",
            Self::Bun => "bun",
        }
    }

    /// The extension of an inline script saved in the run directory.
    fn extension(self) -> &'static str {
        match self {
            Self::Sh | Self::Bash => "sh",
            Self::Python3 => "py",
            Self::Node => "js",
            Self::Ruby => "rb",
            Self::Perl => "pl",
            Self::Deno | Self::Bun => "ts",
        }
    }

    /// Programs to look for on `PATH`, in order.
    fn candidates(self) -> &'static [&'static str] {
        match self {
            Self::Sh => &["sh"],
            Self::Bash => &["bash"],
            Self::Python3 => &["python3", "python"],
            Self::Node => &["node"],
            Self::Ruby => &["ruby"],
            Self::Perl => &["perl"],
            Self::Deno => &["deno"],
            Self::Bun => &["bun"],
        }
    }

    /// Arguments placed before the script path: the options on the script's `#!`
    /// line, which for Deno and Bun replace the default `run` options.
    fn prefix(self, options: &[String]) -> Vec<String> {
        match self {
            Self::Deno | Self::Bun if options.first().is_some_and(|word| word == "run") => {
                options.to_vec()
            }
            Self::Deno if options.is_empty() => ["run", "--allow-all", "--quiet"]
                .map(str::to_owned)
                .to_vec(),
            Self::Deno | Self::Bun => std::iter::once("run".to_owned())
                .chain(options.iter().cloned())
                .collect(),
            _ => options.to_vec(),
        }
    }
}

/// A `#!` first line: the interpreter it names, the options it passes, and the
/// variables an `env` form sets.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Shebang {
    pub(crate) program: String,
    options: Vec<String>,
    env: Vec<(String, String)>,
}

impl Shebang {
    /// Reads `#!/bin/sh -e`, `#!/usr/bin/env -S bash -eu` or
    /// `#!/usr/bin/env PYTHONUTF8=1 python3`. Options are split at whitespace, as
    /// macOS does; Linux would pass them as one argument.
    pub(crate) fn parse(code: &[u8]) -> Option<Self> {
        let line = code.strip_prefix(b"#!")?;
        let line = &line[..line.iter().position(|&b| b == b'\n').unwrap_or(line.len())];
        let line = String::from_utf8_lossy(line);
        let mut words = line.trim_end_matches('\r').split_whitespace();
        let basename = |word: &str| word.rsplit('/').next().unwrap_or(word).to_owned();
        let mut program = basename(words.next()?);
        let mut env = Vec::new();
        if program == "env" {
            program = loop {
                let word = words.next()?;
                if matches!(word, "-u" | "-C" | "-P" | "--unset" | "--chdir") {
                    // The next word is this option's value.
                    words.next();
                } else if word.starts_with('-') {
                    // `-S` and other options of `env` itself.
                } else if let Some((name, value)) = word.split_once('=') {
                    env.push((name.to_owned(), value.to_owned()));
                } else {
                    break basename(word);
                }
            };
        }
        Some(Self {
            program,
            options: words.map(str::to_owned).collect(),
            env,
        })
    }
}

pub fn command() -> clap::Command<'static> {
    clap::Command::new("exec")
        .about("Run a short script over large data and keep only what it prints")
        .arg(
            Arg::new("code")
                .value_name("CODE")
                .allow_invalid_utf8(true)
                .conflicts_with("script")
                .help("Script text, or - to read it from stdin"),
        )
        .arg(
            Arg::new("script")
                .long("script")
                .value_name("FILE")
                .allow_invalid_utf8(true)
                .help("Run an existing script file in place"),
        )
        .arg(
            Arg::new("lang")
                .long("lang")
                .value_name("LANG")
                .possible_values([
                    "sh", "bash", "python3", "python", "node", "js", "ruby", "perl", "deno", "bun",
                ])
                .help("Interpreter; otherwise taken from a #! line or the --script extension"),
        )
        .arg(
            Arg::new("file")
                .long("file")
                .value_name("PATH")
                .multiple_occurrences(true)
                .allow_invalid_utf8(true)
                .help("Hand an input to the script by path (repeatable); the first is also JUSTLESS_FILE"),
        )
        .arg(
            Arg::new("intent")
                .long("intent")
                .value_name("TEXT")
                .help("Show the stdout lines most relevant to TEXT when output exceeds the preview; print one record per line"),
        )
        .arg(
            Arg::new("timeout")
                .long("timeout")
                .value_name("SECONDS")
                .default_value("60")
                .help("Stop the script and everything it started after this many seconds (0: no limit)"),
        )
        .arg(
            Arg::new("max-bytes")
                .long("max-bytes")
                .value_name("N")
                .help("Maximum preview bytes per stream (default 4096)"),
        )
        .arg(
            Arg::new("args")
                .value_name("ARGS")
                .multiple_values(true)
                .last(true)
                .allow_invalid_utf8(true)
                .help("Arguments for the script, after the --file paths"),
        )
}

/// Where the script comes from.
enum Script {
    /// Code from the command line or stdin, saved in the run directory before it runs.
    Inline(Vec<u8>),
    /// An existing file, run in place, with its first bytes for a `#!` line.
    File { path: PathBuf, head: Vec<u8> },
}

pub fn run(workspace: &Workspace, matches: &ArgMatches) -> Result<i32> {
    let max_bytes = if matches.is_present("max-bytes") {
        crate::read_num(matches, "max-bytes", 512)?
    } else {
        workspace.settings()?.max_output_bytes
    };
    let seconds: u64 = matches
        .value_of("timeout")
        .unwrap_or("60")
        .parse()
        .map_err(|_| anyhow!("--timeout expects a whole number of seconds"))?;
    let timeout = (seconds > 0).then(|| Duration::from_secs(seconds));
    let intent = matches.value_of("intent");
    if let Some(intent) = intent {
        ensure!(
            !crate::focus::terms(intent).is_empty(),
            "--intent needs a word other than common words such as \"the\" or \"what\""
        );
    }

    let (script, code_on_stdin) = load(matches)?;
    let shebang = Shebang::parse(match &script {
        Script::Inline(code) => code,
        Script::File { head, .. } => head,
    });
    let lang = language(matches, shebang.as_ref(), &script)?;
    // The options and variables on a `#!` line apply when it names the language that
    // runs, as they would if the file were run directly.
    let shebang = shebang
        .filter(|line| Lang::from_interpreter(&line.program) == Some(lang))
        .unwrap_or_default();
    let interpreter = find_on_path(lang.candidates()).with_context(|| {
        let candidates = lang.candidates();
        if candidates.len() > 1 {
            format!(
                "{} is not on PATH (tried {})",
                lang.label(),
                candidates.join(", ")
            )
        } else {
            format!("{} is not on PATH", lang.label())
        }
    })?;
    let mut files = Vec::new();
    for given in matches.values_of_os("file").into_iter().flatten() {
        let path = std::path::absolute(given)
            .and_then(|path| fs::metadata(&path).map(|_| path))
            .with_context(|| format!("--file {}", Path::new(given).display()))?;
        files.push((given, path));
    }
    let extra: Vec<&OsStr> = matches.values_of_os("args").into_iter().flatten().collect();

    // Runs over the same inputs share a key, so a later success supersedes the failed
    // attempts before it. The code and the intent are left out for that reason.
    let mut key: Vec<OsString> = vec!["exec".into(), lang.label().into()];
    key.extend(files.iter().map(|(given, _)| given.to_os_string()));
    key.push("--".into());
    key.extend(extra.iter().map(|arg| arg.to_os_string()));

    // A script that did not come from stdin can read piped input, as in
    // `curl … | justless exec …`. A terminal is never handed over: the script runs
    // outside the terminal's foreground process group.
    let pass_stdin = !code_on_stdin && !io::stdin().is_terminal();
    let timed_out = Cell::new(false);
    let left_running = Cell::new(false);
    let run_id = RefCell::new(String::new());
    let spawn = |directory: &Path, stdout: Stdio, stderr: Stdio| -> Result<i32> {
        if let Some(name) = directory.file_name() {
            *run_id.borrow_mut() = name.to_string_lossy().into_owned();
        }
        let path = match &script {
            Script::Inline(code) => {
                let path = directory.join(format!("script.{}", lang.extension()));
                write_private(&path, code).context("save the script")?;
                if lang == Lang::Node {
                    // Without this, a root package.json declaring "type": "module" would
                    // make `require` unavailable to the saved script.
                    write_private(&directory.join("package.json"), b"{}\n")
                        .context("save the script")?;
                }
                // Every traceback frame repeats this path, so it is kept short.
                relative(&path, &workspace.cwd, &workspace.root)
            }
            Script::File { path, .. } => path.clone(),
        };
        let mut command = Command::new(&interpreter);
        command
            .args(lang.prefix(&shebang.options))
            .arg(&path)
            .args(files.iter().map(|(_, path)| path))
            .args(&extra)
            .current_dir(&workspace.cwd)
            .stdin(if pass_stdin {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .stdout(stdout)
            .stderr(stderr);
        if std::env::var_os("NO_COLOR").is_none() {
            command.env("NO_COLOR", "1");
        }
        command.envs(shebang.env.iter().map(|(name, value)| (name, value)));
        match files.first() {
            Some((_, path)) => command.env("JUSTLESS_FILE", path),
            None => command.env_remove("JUSTLESS_FILE"),
        };
        let outcome =
            supervise(command, timeout).with_context(|| format!("start {}", lang.label()))?;
        timed_out.set(outcome.expired);
        left_running.set(outcome.left_running);
        Ok(if outcome.expired {
            124
        } else {
            capture::exit_code(outcome.status)
        })
    };
    let session = std::env::var("JUSTLESS_SESSION").unwrap_or_else(|_| "terminal".into());
    let justless = workspace.justless();
    let workspace_root = workspace.root.clone();
    let settings = workspace.settings()?;
    let json_grouping = settings.json_grouping;
    let lossless_only = settings.lossless_only;
    let intent_owned = intent.map(str::to_owned);
    let status = capture::execute_shaped(
        workspace,
        &key,
        Shape {
            max_bytes,
            raw: false,
            session: &session,
            // The output is the answer the script was written for, so it is never
            // collapsed into a notice that it matches an earlier run.
            fresh: true,
            window: false,
            // Hooks pass `exec` through, so its runs are not hook-routed, but the agent
            // reads its output the way it reads a hook-routed run's.
            hook: false,
            agent_footer: true,
            family: Some(format!("exec {}", lang.label())),
            merge: false,
            // Shape long redacted stdout via lossless deep-schema `jsonview`,
            // workspace-aware `stacktrace` collapsing, structured `logview` folding,
            // dedup-aware `--intent` ranking, or whole-line `focus::outline`.
            stdout_preview: Some(Box::new(move |stdout: &Path, id: &str| {
                shape_stdout_with_mode(
                    stdout,
                    id,
                    max_bytes,
                    intent_owned.as_deref(),
                    &workspace_root,
                    &justless,
                    json_grouping,
                    lossless_only,
                )
            })),
            spawn: Some(Box::new(spawn)),
        },
    )?;
    if timed_out.get() {
        eprintln!("[justless] stopped at --timeout {seconds}s");
    } else if left_running.get() {
        // A verbatim success prints no footer, so the notice names the run itself.
        eprintln!(
            "[justless {}] stopped processes the script left running; output they had not written is missing",
            run_id.borrow()
        );
    }
    Ok(status)
}

/// Reads the script and whether it came from stdin. Stdin is read only for an
/// explicit `-`, so piped data is never run as code.
fn load(matches: &ArgMatches) -> Result<(Script, bool)> {
    if let Some(file) = matches.value_of_os("script") {
        let path = std::path::absolute(file)
            .with_context(|| format!("--script {}", Path::new(file).display()))?;
        let metadata = fs::metadata(&path)
            .with_context(|| format!("--script {}", Path::new(file).display()))?;
        ensure!(
            !metadata.is_dir(),
            "--script {} is a directory",
            Path::new(file).display()
        );
        let mut head = Vec::new();
        File::open(&path)
            .and_then(|file| file.take(256).read_to_end(&mut head))
            .with_context(|| format!("--script {}", Path::new(file).display()))?;
        return Ok((Script::File { path, head }, false));
    }
    let Some(code) = matches.value_of_os("code") else {
        bail!("pass the script as an argument, as - to read it from stdin, or with --script FILE");
    };
    if code == "-" {
        let mut code = Vec::new();
        io::stdin()
            .lock()
            .take(STDIN_LIMIT + 1)
            .read_to_end(&mut code)
            .context("read the script from stdin")?;
        ensure!(!code.is_empty(), "the script on stdin is empty");
        ensure!(
            code.len() as u64 <= STDIN_LIMIT,
            "the script on stdin exceeds 1 MiB; hand data to it with --file"
        );
        return Ok((Script::Inline(code), true));
    }
    let bytes = code.as_encoded_bytes();
    ensure!(!bytes.is_empty(), "the script is empty");
    // `justless exec --lang python3 analyze.py` would otherwise run the file name as code.
    if !bytes.iter().any(u8::is_ascii_whitespace) && Path::new(code).is_file() {
        let name = code.to_string_lossy();
        bail!("{name} is a file; run it with --script {name}");
    }
    Ok((Script::Inline(bytes.to_vec()), false))
}

/// `--lang`, then a `#!` line naming a supported interpreter, then the extension of a
/// `--script` file. Justless never guesses from the code itself.
fn language(matches: &ArgMatches, shebang: Option<&Shebang>, script: &Script) -> Result<Lang> {
    if let Some(name) = matches.value_of("lang") {
        return Lang::parse(name).with_context(|| format!("unknown language {name}"));
    }
    if let Some(line) = shebang {
        return Lang::from_interpreter(&line.program)
            .with_context(|| format!("the #! line names {}; pass --lang", line.program));
    }
    if let Script::File { path, .. } = script
        && let Some(lang) = Lang::from_extension(path)
    {
        return Ok(lang);
    }
    bail!("name the language with --lang ({LANGUAGES}) or start the script with #!")
}

/// `path` relative to the working directory when both lie under the root, as in
/// `.justless/runs/run-…/script.py` or `../.justless/…`. Otherwise `path` itself.
fn relative(path: &Path, cwd: &Path, root: &Path) -> PathBuf {
    let (Ok(inside), Ok(depth)) = (path.strip_prefix(root), cwd.strip_prefix(root)) else {
        return path.to_owned();
    };
    let up: PathBuf = depth.components().map(|_| Component::ParentDir).collect();
    up.join(inside)
}

/// Looks up an interpreter the way a shell does, returning the absolute path to run.
fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    names.iter().find_map(|name| {
        std::env::split_paths(&path).find_map(|directory| {
            let directory = if directory.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                directory
            };
            let mut candidates = vec![directory.join(name)];
            if cfg!(windows) {
                candidates.push(directory.join(format!("{name}.exe")));
            }
            candidates
                .into_iter()
                .find(|candidate| executable(candidate))
                .and_then(|candidate| std::path::absolute(candidate).ok())
        })
    })
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// The processes a script started, as far as Justless can reach them.
trait Group {
    /// Sends `signal` to every process in the group; false when it cannot be sent.
    fn signal(&mut self, signal: &str) -> bool;
    /// Whether any process is left in the group.
    fn occupied(&mut self) -> bool;
    /// Whether a process that has not exited is left. Linux counts a zombie as a
    /// member, and an init that does not reap orphans leaves them in the group.
    fn running(&mut self) -> bool;
}

/// No group, as on Windows or without `/bin/sh`: only the interpreter can be stopped.
struct Alone;

impl Group for Alone {
    fn signal(&mut self, _: &str) -> bool {
        false
    }

    fn occupied(&mut self) -> bool {
        false
    }

    fn running(&mut self) -> bool {
        false
    }
}

/// Whether `ps` lists a process of `group` that has not exited, or `ps` cannot tell.
#[cfg(unix)]
fn running_in(group: u32) -> bool {
    let Ok(listing) = Command::new("ps")
        .args(["-A", "-o", "pgid=", "-o", "stat="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return true;
    };
    let group = group.to_string();
    !listing.status.success()
        || String::from_utf8_lossy(&listing.stdout)
            .lines()
            .any(|line| {
                let mut fields = line.split_whitespace();
                fields.next() == Some(group.as_str())
                    && fields.next().is_some_and(|stat| !stat.starts_with('Z'))
            })
}

/// The script leads its own process group, and the watchdog leads another, so no
/// signal the script sends its group reaches the watchdog. The first input line names
/// the script's group. Each later line names a signal for that group, or is `0` to ask
/// whether any member is left, answered with 1 or 0. End of input, including when
/// Justless dies from any signal, kills the group.
#[cfg(unix)]
const WATCHDOG: &str = "trap '' INT TERM HUP QUIT PIPE; read -r g || exit; \
    while read -r s; do if [ \"$s\" = 0 ]; then kill -s 0 -- \"-$g\" 2>/dev/null && echo 1 || echo 0; \
    else kill -s \"$s\" -- \"-$g\" 2>/dev/null; fi; done; kill -s KILL -- \"-$g\" 2>/dev/null";

#[cfg(unix)]
struct Watchdog {
    process: Child,
    input: std::process::ChildStdin,
    answers: std::sync::mpsc::Receiver<bool>,
    /// The script's process group, once the watchdog has it.
    group: Option<u32>,
}

#[cfg(unix)]
impl Watchdog {
    fn start() -> io::Result<Self> {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;

        let mut process = Command::new("/bin/sh")
            .args(["-c", WATCHDOG])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let input = process.stdin.take().expect("piped stdin");
        let output = process.stdout.take().expect("piped stdout");
        let (sender, answers) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in io::BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                if sender.send(line == "1").is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            process,
            input,
            answers,
            group: None,
        })
    }

    /// Hands the watchdog the script's process group.
    fn watch(&mut self, group: u32) {
        if self.send(&group.to_string()) {
            self.group = Some(group);
        }
    }

    /// A write to a watchdog that already exited fails with EPIPE, since Rust ignores
    /// SIGPIPE.
    fn send(&mut self, line: &str) -> bool {
        self.input.write_all(format!("{line}\n").as_bytes()).is_ok()
    }

    /// Kills what is left of the group and waits for the watchdog to exit.
    fn finish(self) {
        let Self {
            mut process, input, ..
        } = self;
        drop(input);
        let _ = process.wait();
    }
}

#[cfg(unix)]
impl Group for Watchdog {
    fn signal(&mut self, signal: &str) -> bool {
        self.group.is_some() && self.send(signal)
    }

    fn occupied(&mut self) -> bool {
        // The answer is immediate. A watchdog that cannot answer cannot stop anything
        // either, so there is nothing to wait for.
        self.group.is_some()
            && self.send("0")
            && self
                .answers
                .recv_timeout(Duration::from_secs(1))
                .unwrap_or(false)
    }

    fn running(&mut self) -> bool {
        self.group.is_some_and(running_in)
    }
}

/// How the script ended.
struct Outcome {
    status: ExitStatus,
    /// The time limit passed and the script was stopped.
    expired: bool,
    /// Processes it left running were still there `DRAIN` after it exited, and were
    /// stopped.
    left_running: bool,
}

/// Runs the script and waits until it and everything it started have stopped.
#[cfg(unix)]
fn supervise(mut command: Command, timeout: Option<Duration>) -> Result<Outcome> {
    use std::os::unix::process::CommandExt;

    // Without a watchdog, as in a sandbox lacking /bin/sh, the script stays in
    // Justless's process group and the time limit stops only the interpreter.
    let Ok(mut watchdog) = Watchdog::start() else {
        let mut child = command.spawn()?;
        return Ok(wait(&mut child, timeout, &mut Alone)?);
    };
    let mut child = match command.process_group(0).spawn() {
        Ok(child) => child,
        Err(error) => {
            watchdog.finish();
            return Err(error.into());
        }
    };
    watchdog.watch(child.id());
    let outcome = wait(&mut child, timeout, &mut watchdog);
    // Closing its input kills what is left of the group, so the streams are final
    // before they are read.
    watchdog.finish();
    Ok(outcome?)
}

#[cfg(not(unix))]
fn supervise(mut command: Command, timeout: Option<Duration>) -> Result<Outcome> {
    let mut child = command.spawn()?;
    Ok(wait(&mut child, timeout, &mut Alone)?)
}

/// Polls the interpreter until it exits. At the deadline the group receives SIGINT, and
/// SIGKILL after the grace period. SIGKILL also goes to the interpreter itself, which
/// may have left the group. After a normal exit, processes left in the group get up to
/// `DRAIN`, within the deadline, to finish writing.
fn wait(
    child: &mut Child,
    timeout: Option<Duration>,
    group: &mut dyn Group,
) -> io::Result<Outcome> {
    let deadline = timeout.and_then(|timeout| Instant::now().checked_add(timeout));
    let mut grace = None;
    let mut expired = false;
    let mut pause = Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let now = Instant::now();
        if !expired && deadline.is_some_and(|deadline| now >= deadline) {
            expired = true;
            if !group.signal("INT") {
                let _ = child.kill();
            }
            grace = Some(now + GRACE);
        } else if grace.is_some_and(|grace| now >= grace) {
            group.signal("KILL");
            let _ = child.kill();
            grace = None;
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(20));
    };
    let mut left_running = false;
    if !expired {
        let exited = Instant::now();
        let until = deadline.map_or(exited + DRAIN, |deadline| deadline.min(exited + DRAIN));
        let mut pause = Duration::from_millis(5);
        while group.occupied() {
            if Instant::now() >= until {
                // Only zombies that nothing reaps lose no output.
                left_running = group.running();
                break;
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(50));
        }
    }
    Ok(Outcome {
        status,
        expired,
        left_running,
    })
}

/// Leading ISO-8601 / RFC3339 timestamp, optionally bracketed.
static LOG_ISO_TS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\[?\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d+)?(?:Z|[+-]\d{2}:?\d{2})?\]?(?:\s+|$)",
    )
    .expect("iso log timestamp pattern")
});
/// Leading syslog / journalctl timestamp (`Mar 10 12:34:56 `).
static LOG_SYSLOG_TS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)\s+\d{1,2}\s+\d{2}:\d{2}:\d{2}\s+",
    )
    .expect("syslog timestamp pattern")
});
/// Leading clock timestamp (`12:34:56.789 ` or `[12:34:56] `).
static LOG_CLOCK_TS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[?\d{2}:\d{2}:\d{2}(?:[.,]\d+)?\]?\s+").expect("clock timestamp pattern")
});
/// Leading klog/glog header (`I1125 10:00:01.123456 1 `).
static LOG_KLOG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[IWEF]\d{4}\s+\d{2}:\d{2}:\d{2}\.\d+\s+\d+\s+").expect("klog pattern")
});
/// Explicit log level prefix (`[INFO]`, `ERROR:`, `WARN `, etc.).
static LOG_LEVEL_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:\[(?i:trace|trc|debug|dbg|info|inf|notice|warn|warning|wrn|error|err|erro|fatal|fata|crit|critical|severe)\]|(?:TRACE|TRC|DEBUG|DBG|INFO|INF|NOTICE|WARN|WARNING|WRN|ERROR|ERR|ERRO|FATAL|FATA|CRIT|CRITICAL|SEVERE)(?:\s*[:|\-]\s*|\s+))",
    )
    .expect("log level prefix pattern")
});
/// Logfmt severity field (`level=info`, `lvl=warn`, `severity=ERROR`).
static LOG_LOGFMT_LEVEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|\s)(?:level|lvl|severity)=[A-Za-z]+").expect("logfmt level pattern")
});
/// Kubernetes `--prefix` pod/container prefix (`[pod/container] ...`).
static LOG_K8S_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[([A-Za-z0-9._-]+/[A-Za-z0-9._-]+)\]\s+(.*)$").expect("k8s prefix pattern")
});
/// Docker Compose prefix (`service-1 | ...`).
static LOG_COMPOSE_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([A-Za-z0-9._-]{1,48})\s*\|\s+(.*)$").expect("compose prefix pattern")
});
/// Ruby backtrace continuation frame (`\tfrom path:line:in \`method'`).
static RUBY_FRAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([ \t]+)from ([^\r\n]+?):(\d+):in [`'].*").expect("ruby frame pattern")
});

/// Outcome of shaping a captured text stream for `justless exec`.
enum ShapedText {
    /// Compacted preview ready to display (guaranteed `<= max_bytes`).
    Ready(Vec<u8>),
    /// Native `crate::stacktrace` collapsed frames but the result still exceeds
    /// `max_bytes`, so `capture`'s stacktrace-collapsed `head + [diagnostics] + tail`
    /// preview should be kept instead of overwriting with a plain outline.
    UseCaptureStacktrace,
    /// Ruby stacktrace collapsed frames, still exceeding `max_bytes`, ready to outline.
    OutlineCollapsed(String),
    /// Output is not JSON, structured logs, or a collapsible stack trace.
    Fallback,
}

/// Shapes a long redacted stdout archive for `justless exec`.
///
/// With `--intent`, ranks matching lines and windows with boilerplate and window
/// deduplication. Without `--intent`, tries lossless deep-schema `jsonview` (routing
/// NDJSON structured logs to `logview` first), structured `logview` folding, and
/// workspace-aware `stacktrace` collapsing before falling back to `focus::outline`.
#[cfg(test)]
fn shape_stdout(
    stdout: &Path,
    run_id: &str,
    max_bytes: usize,
    intent: Option<&str>,
    workspace_root: &Path,
    justless: &str,
    json_grouping: bool,
) -> Result<Option<Vec<u8>>> {
    shape_stdout_with_mode(
        stdout,
        run_id,
        max_bytes,
        intent,
        workspace_root,
        justless,
        json_grouping,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn shape_stdout_with_mode(
    stdout: &Path,
    run_id: &str,
    max_bytes: usize,
    intent: Option<&str>,
    workspace_root: &Path,
    justless: &str,
    json_grouping: bool,
    lossless_only: bool,
) -> Result<Option<Vec<u8>>> {
    if !lossless_only && let Some(intent) = intent {
        return intent_preview(stdout, intent, max_bytes, justless, run_id)
            .map(Some)
            .context("could not rank stdout for --intent");
    }

    let size = fs::metadata(stdout)?.len();
    if size > 0 && size <= crate::jsonview::LIMIT as u64 {
        let head = read_prefix(stdout, 64)?;
        let can_read = size <= TEXT_SHAPE_LIMIT || could_be_json_prefix(&head);
        if can_read {
            let bytes = fs::read(stdout)?;
            if let Ok(text) = String::from_utf8(bytes) {
                match shape_text_preview_with_mode(
                    &text,
                    max_bytes,
                    workspace_root,
                    "stdout",
                    json_grouping,
                    None,
                    lossless_only,
                ) {
                    ShapedText::Ready(preview)
                        if preview.len() <= max_bytes
                            || (lossless_only && preview.len() < text.len()) =>
                    {
                        return Ok(Some(preview));
                    }
                    ShapedText::UseCaptureStacktrace if !lossless_only => return Ok(None),
                    ShapedText::OutlineCollapsed(collapsed) if !lossless_only => {
                        if let Some(outlined) =
                            outline_collapsed_text(&collapsed, max_bytes, justless, run_id)
                        {
                            return Ok(Some(outlined));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if lossless_only {
        return Ok(None);
    }

    crate::focus::outline(stdout, max_bytes, justless, run_id).context("could not outline stdout")
}

/// Shapes UTF-8 stream text using the Wave-1/Wave-2 engines (`jsonview`,
/// `tableview`, `csvview`, `logview`, `stacktrace`), applying `redactor` first
/// whenever one is provided.
#[cfg(test)]
fn shape_text_preview(
    raw: &str,
    max_bytes: usize,
    workspace_root: &Path,
    stream: &str,
    json_grouping: bool,
    redactor: Option<&crate::redact::Redactor>,
) -> ShapedText {
    shape_text_preview_with_mode(
        raw,
        max_bytes,
        workspace_root,
        stream,
        json_grouping,
        redactor,
        false,
    )
}

fn shape_text_preview_with_mode(
    raw: &str,
    max_bytes: usize,
    workspace_root: &Path,
    stream: &str,
    json_grouping: bool,
    redactor: Option<&crate::redact::Redactor>,
    lossless_only: bool,
) -> ShapedText {
    let text = match redactor {
        Some(redactor) => redactor.text(raw),
        None => Cow::Borrowed(raw),
    };
    let text = text.as_ref();
    if text.is_empty() || text.contains('\0') {
        return ShapedText::Fallback;
    }

    let json_candidate =
        could_be_json_prefix(text.as_bytes()) && !text.contains("\"compiler-message\"");
    if json_candidate {
        // NDJSON structured logs fold routine lines into templates and keep 100% of
        // errors/warnings visible instead of clipping mid-table.
        if !lossless_only
            && text.len() <= TEXT_SHAPE_LIMIT as usize
            && looks_like_json_logs(text)
            && let Some(folded) = format_logview(stream, text, max_bytes)
        {
            return ShapedText::Ready(folded.into_bytes());
        }
        let view = if lossless_only {
            crate::jsonview::views(text, json_grouping)
                .into_iter()
                .next()
                .map(String::into_bytes)
        } else {
            shape_json_view(text, max_bytes, json_grouping)
        };
        if let Some(view) = view {
            return ShapedText::Ready(view);
        }
    }

    let view_budget = if lossless_only { usize::MAX } else { max_bytes };
    if stream == "stdout"
        && text.len() <= crate::tableview::LIMIT
        && let Some(view) = crate::tableview::format(text, view_budget)
    {
        return ShapedText::Ready(view.into_bytes());
    }

    if stream == "stdout"
        && text.len() <= crate::csvview::LIMIT
        && let Some(view) = crate::csvview::format(text, view_budget)
    {
        return ShapedText::Ready(view.into_bytes());
    }

    if lossless_only || text.len() > TEXT_SHAPE_LIMIT as usize {
        return ShapedText::Fallback;
    }

    if looks_like_structured_logs(text)
        && let Some(folded) = format_logview(stream, text, max_bytes)
    {
        return ShapedText::Ready(folded.into_bytes());
    }

    if let Some((collapsed, native)) = collapse_stacktrace(text, Some(workspace_root), Some(stream))
    {
        if collapsed.len() <= max_bytes {
            return ShapedText::Ready(collapsed.into_bytes());
        }
        return if native {
            ShapedText::UseCaptureStacktrace
        } else {
            ShapedText::OutlineCollapsed(collapsed)
        };
    }

    ShapedText::Fallback
}

fn read_prefix(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut buf = Vec::with_capacity(limit);
    file.take(limit as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Cheap prefix guard so non-JSON text such as `[2026-01-01] INFO` or `[pod/web]`
/// never triggers a full multi-megabyte JSON parse.
fn could_be_json_prefix(bytes: &[u8]) -> bool {
    let trimmed = bytes.trim_ascii_start();
    let Some((&first, rest)) = trimmed.split_first() else {
        return false;
    };
    let next = rest.trim_ascii_start();
    match first {
        b'{' => matches!(next.first(), Some(b'"' | b'}') | None),
        b'[' => match next.first() {
            Some(b'{' | b'[' | b'"' | b't' | b'f' | b'n' | b']') | None => true,
            Some(b'-' | b'0'..=b'9') => {
                let after_digits = next
                    .iter()
                    .skip(usize::from(next[0] == b'-'))
                    .skip_while(|b| b.is_ascii_digit())
                    .copied()
                    .next();
                !matches!(after_digits, Some(b'-' | b':'))
            }
            Some(_) => false,
        },
        _ => false,
    }
}

/// Lossless deep-schema `jsonview` (table or compact JSON), clipped at whole lines
/// when larger than `max_bytes`, only when strictly smaller than `text`.
fn shape_json_view(text: &str, max_bytes: usize, group: bool) -> Option<Vec<u8>> {
    crate::jsonview::views(text, group)
        .into_iter()
        .find_map(|view| clip_json_view(view, max_bytes))
        .filter(|view| view.len() < text.len() && view.len() <= max_bytes)
}

fn strip_ansi(text: &str) -> Cow<'_, str> {
    if !text.contains('\x1b') {
        return Cow::Borrowed(text);
    }
    crate::capture::ANSI.replace_all(text, "")
}

fn decimal_digits(mut n: usize) -> usize {
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

/// Clips a `jsonview` rendering at whole lines to `max_bytes`, keeping the legend,
/// header, and whole rows unless too little would fit.
fn clip_json_view(view: String, max_bytes: usize) -> Option<Vec<u8>> {
    if view.len() <= max_bytes {
        return Some(view.into_bytes());
    }
    const MARKER_PREFIX: &str = "[";
    const MARKER_SUFFIX: &str = " more lines of this view in the full log]\n";
    let lines = view.split_inclusive('\n').count();
    let mut shown = String::with_capacity(max_bytes);
    for (index, line) in view.split_inclusive('\n').enumerate() {
        let remaining = lines - index;
        let marker_len = MARKER_PREFIX.len() + decimal_digits(remaining) + MARKER_SUFFIX.len();
        if shown.len() + line.len() + marker_len > max_bytes {
            if index < 3 || shown.len() < max_bytes / 2 {
                return None;
            }
            use std::fmt::Write as _;
            let _ = write!(shown, "{MARKER_PREFIX}{remaining}{MARKER_SUFFIX}");
            break;
        }
        shown.push_str(line);
    }
    (shown.len() <= max_bytes).then(|| shown.into_bytes())
}

fn format_logview(stream: &str, text: &str, max_bytes: usize) -> Option<String> {
    let folded = if stream == "stderr" {
        crate::logview::format("", text, max_bytes)?
    } else {
        crate::logview::format(text, "", max_bytes)?
    };
    (folded.len() <= max_bytes && folded.len() < text.len()).then_some(folded)
}

fn has_json_key(line: &str, quoted_key: &str) -> bool {
    let mut offset = 0;
    while let Some(pos) = line[offset..].find(quoted_key) {
        let after = &line[offset + pos + quoted_key.len()..];
        if after.trim_start().starts_with(':') {
            return true;
        }
        offset += pos + quoted_key.len();
    }
    false
}

fn is_json_log_line(line: &str) -> bool {
    const LEVEL_KEYS: &[&str] = &[
        "\"level\"",
        "\"lvl\"",
        "\"severity\"",
        "\"levelname\"",
        "\"log.level\"",
        "\"priority\"",
        "\"PRIORITY\"",
    ];
    const TS_KEYS: &[&str] = &[
        "\"ts\"",
        "\"timestamp\"",
        "\"time\"",
        "\"@timestamp\"",
        "\"datetime\"",
        "\"t\"",
    ];
    const MSG_KEYS: &[&str] = &[
        "\"msg\"",
        "\"message\"",
        "\"event\"",
        "\"error\"",
        "\"err\"",
    ];

    let trimmed = line.trim();
    let candidate = if trimmed.starts_with('{') && trimmed.ends_with('}') {
        trimmed
    } else {
        let rest = strip_leading_timestamp(trimmed).trim();
        if rest.starts_with('{') && rest.ends_with('}') {
            rest
        } else {
            return false;
        }
    };
    if LEVEL_KEYS.iter().any(|k| has_json_key(candidate, k)) {
        return true;
    }
    TS_KEYS.iter().any(|k| has_json_key(candidate, k))
        && MSG_KEYS.iter().any(|k| has_json_key(candidate, k))
}

/// Whether `text` is NDJSON structured logs (at least 8 non-empty lines, mostly JSON
/// objects carrying log keys such as `level`/`ts`/`msg`) rather than structured data.
fn looks_like_json_logs(text: &str) -> bool {
    let has_ansi = text.contains('\x1b');
    let mut sampled = 0usize;
    let mut json_objects = 0usize;
    let mut json_logs = 0usize;
    for raw in text.lines() {
        let clean = if has_ansi {
            strip_ansi(raw)
        } else {
            Cow::Borrowed(raw)
        };
        let line = clean.trim();
        if line.is_empty() {
            continue;
        }
        sampled += 1;
        let body = strip_leading_timestamp(line).trim();
        if body.starts_with('{') && body.ends_with('}') {
            json_objects += 1;
            if is_json_log_line(line) {
                json_logs += 1;
            }
        }
        if sampled >= 64 {
            break;
        }
    }
    sampled >= 8 && json_objects * 4 >= sampled * 3 && json_logs * 2 >= json_objects
}

fn is_level_word_upper(word: &str) -> bool {
    const LEVEL_WORDS: &[&str] = &[
        "TRACE", "TRC", "DEBUG", "DBG", "INFO", "INF", "LOG", "NOTICE", "WARN", "WARNING", "WRN",
        "ERROR", "ERR", "ERRO", "FATAL", "FATA", "CRIT", "CRITICAL", "PANIC", "SEVERE",
    ];
    LEVEL_WORDS
        .iter()
        .any(|level| word.eq_ignore_ascii_case(level))
}

/// Whether `text` looks like repetitive timestamped, JSON, level-prefixed, or
/// container-prefixed structured logs eligible for `logview` folding. Plain numbered
/// lines, CSV rows, and data exports return `false`.
fn looks_like_structured_logs(text: &str) -> bool {
    let has_ansi = text.contains('\x1b');
    let mut non_empty = 0usize;
    let mut sampled = 0usize;
    let mut structured = 0usize;
    for raw in text.lines() {
        let clean = if has_ansi {
            strip_ansi(raw)
        } else {
            Cow::Borrowed(raw)
        };
        let line = clean.trim();
        if line.is_empty() {
            continue;
        }
        non_empty += 1;
        if sampled < 64 {
            sampled += 1;
            let body = if let Some(caps) = LOG_K8S_PREFIX.captures(line) {
                caps.get(2).map_or(line, |m| m.as_str()).trim_start()
            } else if let Some(caps) = LOG_COMPOSE_PREFIX.captures(line) {
                let svc = caps.get(1).map_or("", |m| m.as_str());
                if svc.bytes().any(|b| b.is_ascii_alphabetic()) && !is_level_word_upper(svc) {
                    caps.get(2).map_or(line, |m| m.as_str()).trim_start()
                } else {
                    line
                }
            } else {
                line
            };
            let is_structured = is_json_log_line(body)
                || LOG_KLOG.is_match(body)
                || LOG_ISO_TS.is_match(body)
                || LOG_SYSLOG_TS.is_match(body)
                || LOG_CLOCK_TS.is_match(body)
                || LOG_LEVEL_PREFIX.is_match(body)
                || LOG_LOGFMT_LEVEL.is_match(body);
            if is_structured {
                structured += 1;
            }
        }
        if non_empty >= 8 && sampled >= 64 {
            break;
        }
    }
    non_empty >= 8 && structured >= 4 && structured * 5 >= sampled * 2
}

/// Collapses external runtime/package stack frames across Python, Node/V8/Deno/Bun,
/// Java/Kotlin/Scala, Go, Rust (via `crate::stacktrace`), and Ruby, keeping 100% of
/// error headers, throw-site frames, workspace/script frames, and redacted frames.
/// Returns `(collapsed, used_native_stacktrace)`.
fn collapse_stacktrace(
    text: &str,
    workspace_root: Option<&Path>,
    stream: Option<&str>,
) -> Option<(String, bool)> {
    if let Some(collapsed) = crate::stacktrace::collapse(text, workspace_root, stream) {
        return Some((collapsed, true));
    }
    collapse_ruby_stacktrace(text, workspace_root, stream).map(|collapsed| (collapsed, false))
}

fn ruby_workspace_roots(workspace_root: Option<&Path>) -> Vec<String> {
    let mut roots = Vec::new();
    if let Some(root) = workspace_root {
        let norm = root.to_string_lossy().replace('\\', "/");
        if !norm.is_empty() {
            roots.push(norm);
        }
        if let Ok(canon) = root.canonicalize() {
            let canon_norm = canon.to_string_lossy().replace('\\', "/");
            if !canon_norm.is_empty() && !roots.contains(&canon_norm) {
                roots.push(canon_norm);
            }
        }
    }
    roots
}

fn classify_ruby_frame(raw_path: &str, roots: &[String]) -> (bool, String) {
    let trimmed = raw_path.trim();
    let path_cow = if trimmed.contains('\\') {
        Cow::Owned(trimmed.replace('\\', "/"))
    } else {
        Cow::Borrowed(trimmed)
    };
    let path = path_cow.as_ref();
    if path.starts_with("<internal:") || path.starts_with("(eval") {
        return (false, "ruby:internal".to_owned());
    }
    if let Some(after) = path.rsplit_once("/gems/").map(|(_, rest)| rest) {
        let seg = after.split('/').next().unwrap_or("ruby");
        let pkg = strip_gem_version(seg);
        return (false, pkg.to_owned());
    }
    let external_markers = [
        "/ruby/",
        "/site_ruby/",
        "/vendor_ruby/",
        "/vendor/bundle/",
        "/bundler/",
        "/.rbenv/",
        "/.rvm/",
        "/.asdf/",
        "/.mise/",
    ];
    if external_markers.iter().any(|m| path.contains(m)) {
        return (false, "ruby".to_owned());
    }
    let is_abs = path.starts_with('/')
        || (path.len() >= 3
            && path.as_bytes()[0].is_ascii_alphabetic()
            && path.as_bytes()[1] == b':'
            && path.as_bytes()[2] == b'/');
    if is_abs {
        for root in roots {
            let root = root.trim_end_matches('/');
            if let Some(rest) = path.strip_prefix(root)
                && let Some(rel) = rest.strip_prefix('/')
            {
                if rel.starts_with("vendor/") || rel.contains("/vendor/bundle/") {
                    return (false, "ruby".to_owned());
                }
                return (true, String::new());
            }
        }
        return (false, "ruby".to_owned());
    }
    if path.starts_with("vendor/") {
        return (false, "ruby".to_owned());
    }
    (true, String::new())
}

fn strip_gem_version(segment: &str) -> &str {
    for (idx, _) in segment.match_indices('-') {
        if segment[idx + 1..]
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_digit())
        {
            return &segment[..idx];
        }
    }
    segment
}

fn collapse_ruby_stacktrace(
    text: &str,
    workspace_root: Option<&Path>,
    stream: Option<&str>,
) -> Option<String> {
    if !text.contains(":in ") {
        return None;
    }
    let has_ansi = text.contains('\x1b');
    let roots = ruby_workspace_roots(workspace_root);
    let mut out = String::with_capacity(text.len());
    let mut run: Vec<(usize, &str, String, String)> = Vec::new();
    let mut collapsed_any = false;

    let flush_run =
        |run: &mut Vec<(usize, &str, String, String)>, out: &mut String, collapsed: &mut bool| {
            if run.is_empty() {
                return;
            }
            if run.len() >= crate::stacktrace::MIN_COLLAPSE_FRAMES {
                let first_line = run.first().map_or(0, |r| r.0);
                let last_line = run.last().map_or(0, |r| r.0);
                let indent = run.first().map_or("", |r| r.2.as_str());
                let orig_bytes: usize = run.iter().map(|r| r.1.len()).sum();
                let mut counts: HashMap<&str, usize> = HashMap::new();
                for (_, _, _, pkg) in run.iter() {
                    if !pkg.is_empty() {
                        *counts.entry(pkg.as_str()).or_insert(0) += 1;
                    }
                }
                let mut pkgs: Vec<(&str, usize)> = counts.into_iter().collect();
                pkgs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
                let pkg_list: Vec<&str> = pkgs.into_iter().take(2).map(|(p, _)| p).collect();
                let pkg_part = if pkg_list.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", pkg_list.join(", "))
                };
                let loc_part = match stream {
                    Some(stream) => format!(" in {stream}:{first_line}-{last_line}"),
                    None => String::new(),
                };
                let summary = format!(
                    "{indent}[… {} runtime/dependency frames{pkg_part}{loc_part}]\n",
                    run.len()
                );
                if summary.len() < orig_bytes {
                    out.push_str(&summary);
                    *collapsed = true;
                    run.clear();
                    return;
                }
            }
            for (_, raw, _, _) in run.drain(..) {
                out.push_str(raw);
            }
        };

    for (idx, raw) in text.split_inclusive('\n').enumerate() {
        let line_no = idx + 1;
        let stripped_nl = raw.strip_suffix('\n').unwrap_or(raw);
        let plain_cow = if has_ansi {
            strip_ansi(stripped_nl)
        } else {
            Cow::Borrowed(stripped_nl)
        };
        let plain = plain_cow.trim_end_matches('\r');

        if plain.starts_with([' ', '\t'])
            && plain.contains(":in ")
            && let Some(caps) = RUBY_FRAME.captures(plain)
        {
            let indent = caps.get(1).map_or("", |m| m.as_str()).to_owned();
            let path = caps.get(2).map_or("", |m| m.as_str());
            let has_redacted = plain.contains("[redacted:");
            let (is_workspace, pkg) = classify_ruby_frame(path, &roots);
            if !is_workspace && !has_redacted {
                run.push((line_no, raw, indent, pkg));
                continue;
            }
        }
        flush_run(&mut run, &mut out, &mut collapsed_any);
        out.push_str(raw);
    }
    flush_run(&mut run, &mut out, &mut collapsed_any);

    (collapsed_any && out.len() < text.len()).then_some(out)
}

/// Outlines already-collapsed text (such as a very large Ruby backtrace) to fit
/// within `max_bytes`, keeping whole lines from both ends and a pointer to the log.
fn outline_collapsed_text(
    text: &str,
    max_bytes: usize,
    justless: &str,
    run_id: &str,
) -> Option<Vec<u8>> {
    let marker =
        format!("[justless] stdout trace clipped · {justless} log {run_id} --stream stdout\n");
    let body = max_bytes.saturating_sub(marker.len());
    if body < 64 {
        return None;
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut head = String::with_capacity(max_bytes);
    let mut head_count = 0usize;
    for &line in &lines {
        if head.len() + line.len() > body / 2 {
            break;
        }
        head.push_str(line);
        head_count += 1;
    }
    let tail_budget = body - head.len();
    let mut tail_lines = Vec::new();
    let mut tail_bytes = 0usize;
    for &line in lines[head_count..].iter().rev() {
        if tail_bytes + line.len() > tail_budget {
            break;
        }
        tail_bytes += line.len();
        tail_lines.push(line);
    }
    tail_lines.reverse();
    if head.is_empty() && tail_lines.is_empty() {
        return None;
    }
    let mut out = head;
    out.push_str(&marker);
    for line in tail_lines {
        out.push_str(line);
    }
    (out.len() <= max_bytes).then(|| out.into_bytes())
}

// ---------------------------------------------------------------------------
// Dedup-aware `--intent` preview for `justless exec`
// ---------------------------------------------------------------------------

const INTENT_LINE_LIMIT: usize = 4 * 1024 * 1024;
const INTENT_LINE_LIMIT_SHOWN: &str = "4 MiB";
const INTENT_MAX_TERMS: usize = 16;
const INTENT_SPAN_LIMIT: usize = 4096;
const INTENT_RANGE_LIMIT: usize = 1024;
const INTENT_RANGES_SHOWN: usize = 3;
const INTENT_NEARBY: usize = 8;
const INTENT_EXCERPT: usize = 240;
const INTENT_ANCHOR: usize = 160;
const INTENT_LEAD: usize = 80;
const INTENT_ROOMY: usize = 200;
const INTENT_SHOWN: usize = 60;
const INTENT_MAX_DEDUP_KEYS: usize = 8192;
const INTENT_K1: f64 = 1.2;
const INTENT_B: f64 = 0.75;
const INTENT_STOPWORDS: &[&str] = &[
    "a", "an", "the", "of", "to", "in", "for", "and", "or", "is", "are", "was", "were", "be",
    "been", "what", "which", "who", "how", "many", "much", "show", "me", "find", "all", "any",
    "each", "with", "by", "on", "at", "from", "that", "this", "these", "those", "it", "its", "i",
    "my", "we", "our", "where", "why", "when", "does", "do", "did", "get", "give", "list", "per",
    "please", "there", "their",
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct IntentTerm {
    text: String,
    phrase: bool,
}

fn intent_terms(intent: &str) -> Vec<IntentTerm> {
    let mut terms = Vec::new();
    let mut add = |term: IntentTerm| {
        if terms.len() < INTENT_MAX_TERMS && !terms.contains(&term) {
            terms.push(term);
        }
    };
    let parts: Vec<&str> = intent.split('"').collect();
    let unclosed = parts.len().is_multiple_of(2);
    for (index, part) in parts.iter().enumerate() {
        let quoted = index % 2 == 1 && !(unclosed && index == parts.len() - 1);
        if quoted {
            let phrase = part.trim();
            if !phrase.is_empty() {
                add(IntentTerm {
                    text: phrase.to_ascii_lowercase(),
                    phrase: true,
                });
            }
            continue;
        }
        intent_tokens(part, |_, token| {
            let mut word = token.to_lowercase();
            let single = word.chars().count() == 1 && !word.chars().all(|c| c.is_ascii_digit());
            if !single && !INTENT_STOPWORDS.contains(&word.as_str()) {
                intent_stem_in_place(&mut word);
                add(IntentTerm {
                    text: word,
                    phrase: false,
                });
            }
        });
    }
    terms
}

fn intent_tokens(text: &str, mut emit: impl FnMut(usize, &str)) {
    let mut start = None;
    let mut lower = false;
    for (index, c) in text.char_indices() {
        if c.is_alphanumeric() {
            match start {
                None => start = Some(index),
                Some(first) if lower && c.is_uppercase() => {
                    emit(first, &text[first..index]);
                    start = Some(index);
                }
                Some(_) => {}
            }
            lower = c.is_lowercase();
        } else {
            if let Some(first) = start.take() {
                emit(first, &text[first..index]);
            }
            lower = false;
        }
    }
    if let Some(first) = start {
        emit(first, &text[first..]);
    }
}

fn intent_stem_in_place(word: &mut String) {
    if word.bytes().any(|b| b.is_ascii_digit()) {
        return;
    }
    if word.len() > 4 && word.ends_with("ies") {
        word.truncate(word.len() - 3);
        word.push('y');
    } else if ["sses", "ches", "shes", "xes", "zes"]
        .iter()
        .any(|suffix| word.ends_with(suffix))
    {
        word.truncate(word.len() - 2);
    } else if word.len() > 3
        && word.ends_with('s')
        && !["ss", "us", "is"]
            .iter()
            .any(|suffix| word.ends_with(suffix))
    {
        word.pop();
    }
    if word.len() >= 6 && word.ends_with("ing") {
        word.truncate(word.len() - 3);
    } else if word.len() >= 5 && word.ends_with("ed") {
        word.truncate(word.len() - 2);
    }
    if word.len() > 3 && word.ends_with('e') {
        word.pop();
    }
}

#[derive(Clone, Copy, Debug)]
struct IntentSpan {
    start: usize,
    len: usize,
    term: usize,
}

struct IntentAnalysis {
    counts: Vec<u32>,
    spans: Vec<IntentSpan>,
    words: usize,
    shortest: usize,
    longest: usize,
    scratch: String,
}

impl IntentAnalysis {
    fn new(terms: &[IntentTerm]) -> Self {
        let words = terms.iter().filter(|term| !term.phrase);
        Self {
            counts: vec![0; terms.len()],
            spans: Vec::new(),
            words: 0,
            shortest: words.clone().map(|term| term.text.len()).min().unwrap_or(0),
            longest: words.map(|term| term.text.len() + 5).max().unwrap_or(0),
            scratch: String::with_capacity(64),
        }
    }

    fn run(&mut self, line: &str, terms: &[IntentTerm]) {
        self.counts.fill(0);
        self.spans.clear();
        self.words = 0;
        let has_words = terms.iter().any(|term| !term.phrase);
        let (shortest, longest) = (self.shortest, self.longest);
        intent_tokens(line, |start, token| {
            self.words += 1;
            if !has_words || token.is_ascii() && !(shortest..=longest).contains(&token.len()) {
                return;
            }
            self.scratch.clear();
            if token.is_ascii() {
                self.scratch.push_str(token);
                self.scratch.make_ascii_lowercase();
            } else {
                for ch in token.chars() {
                    for lower in ch.to_lowercase() {
                        self.scratch.push(lower);
                    }
                }
            }
            intent_stem_in_place(&mut self.scratch);
            for (index, term) in terms.iter().enumerate() {
                if !term.phrase && term.text == self.scratch {
                    self.counts[index] += 1;
                    if self.spans.len() < INTENT_SPAN_LIMIT {
                        self.spans.push(IntentSpan {
                            start,
                            len: token.len(),
                            term: index,
                        });
                    }
                }
            }
        });
        if terms.iter().any(|term| term.phrase) {
            let lower = line.to_ascii_lowercase();
            for (index, term) in terms.iter().enumerate() {
                if term.phrase {
                    for (start, _) in lower.match_indices(&term.text) {
                        self.counts[index] += 1;
                        if self.spans.len() < INTENT_SPAN_LIMIT {
                            self.spans.push(IntentSpan {
                                start,
                                len: term.text.len(),
                                term: index,
                            });
                        }
                    }
                }
            }
            self.spans.sort_by_key(|span| span.start);
        }
    }

    fn distinct(&self) -> usize {
        self.counts.iter().filter(|&&count| count > 0).count()
    }

    fn mask(&self) -> u32 {
        self.counts
            .iter()
            .enumerate()
            .filter(|&(_, &count)| count > 0)
            .fold(0, |mask, (index, _)| mask | 1 << index)
    }

    fn matches(&self) -> usize {
        self.counts.iter().map(|&count| count as usize).sum()
    }
}

#[derive(Clone)]
struct IntentExcerpt {
    text: String,
    focus: usize,
    before: bool,
    after: bool,
}

impl IntentExcerpt {
    fn render(&self, cap: usize) -> (String, usize, usize) {
        if !self.before && !self.after && self.text.len() <= cap {
            return (self.text.clone(), 0, self.text.len());
        }
        let room = cap.saturating_sub(2 * '…'.len_utf8());
        let text = &self.text;
        let lead = INTENT_LEAD.min(room / 3);
        let start = text.floor_char_boundary(
            self.focus
                .saturating_sub(lead)
                .min(text.len().saturating_sub(room)),
        );
        let end = text.floor_char_boundary((start + room).min(text.len()));
        let mut excerpt = String::with_capacity(cap.min(text.len() + 2 * '…'.len_utf8()));
        if self.before || start > 0 {
            excerpt.push('…');
        }
        excerpt.push_str(&text[start..end]);
        if self.after || end < text.len() {
            excerpt.push('…');
        }
        (excerpt, start, end)
    }
}

#[derive(Clone)]
struct IntentWindow {
    start: usize,
    end: usize,
    excerpt: IntentExcerpt,
    spans: Vec<(usize, usize)>,
}

impl IntentWindow {
    fn at(line: &str, start: usize, spans: &[IntentSpan]) -> Self {
        let end = line.floor_char_boundary((start + INTENT_EXCERPT).min(line.len()));
        let inside: Vec<(usize, usize)> = spans
            .iter()
            .filter(|span| span.start >= start && span.start + span.len <= end)
            .map(|span| (span.start - start, span.len))
            .collect();
        Self {
            start,
            end,
            excerpt: IntentExcerpt {
                text: line[start..end].to_owned(),
                focus: inside.first().map_or(0, |&(offset, _)| offset),
                before: start > 0,
                after: end < line.len(),
            },
            spans: inside,
        }
    }
}

fn strip_leading_timestamp(line: &str) -> &str {
    for re in [&*LOG_ISO_TS, &*LOG_SYSLOG_TS, &*LOG_CLOCK_TS] {
        if let Some(m) = re.find(line) {
            return &line[m.end()..];
        }
    }
    line
}

/// Normalizes a line or window excerpt for boilerplate deduplication: strips window
/// ellipses, leading timestamps, and redundant ASCII whitespace while preserving IDs,
/// numbers, and payload values so distinct records never collapse together.
fn intent_dedup_key(text: &str) -> String {
    let trimmed = text.trim_matches(|c: char| c == '…' || c.is_whitespace());
    let without_ts = strip_leading_timestamp(trimmed).trim();
    let mut out = String::with_capacity(without_ts.len());
    let mut prev_ws = false;
    for ch in without_ts.chars() {
        if ch.is_ascii_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

/// Trims incomplete partial fragments at cut (`…`) window boundaries so periodic
/// boilerplate comparisons ignore mid-token slices at the left/right edges.
fn trim_window_edges(key: &str, before: bool, after: bool) -> &str {
    let mut slice = key;
    if before && slice.len() > 64 {
        let probe = &slice[..slice.floor_char_boundary(32)];
        if let Some(pos) = probe.find(|c: char| c.is_ascii_whitespace() || c == ',' || c == '{') {
            let cut = if slice.as_bytes()[pos] == b'{' {
                pos
            } else {
                pos + 1
            };
            slice = slice[cut..].trim_start();
        }
    }
    if after && slice.len() > 64 {
        let tail_start = slice.ceil_char_boundary(slice.len().saturating_sub(32));
        let probe = &slice[tail_start..];
        if let Some(pos) = probe.rfind(|c: char| c.is_ascii_whitespace() || c == ',' || c == '}') {
            let cut = tail_start
                + if probe.as_bytes()[pos] == b'}' {
                    pos + 1
                } else {
                    pos
                };
            slice = slice[..cut].trim_end();
        }
    }
    slice
}

/// If `s` is strictly periodic with unit length `p` in `16..=s.len()/2`, returns that
/// repeating unit so phase-shifted boilerplate windows on the same line deduplicate
/// without ever colliding with distinct records.
fn periodic_unit(s: &str) -> Option<&str> {
    if s.len() < 48 {
        return None;
    }
    let anchor_end = s.floor_char_boundary(16);
    if anchor_end < 8 {
        return None;
    }
    let anchor = &s[..anchor_end];
    let mut search_from = anchor_end;
    while let Some(rel) = s[search_from..].find(anchor) {
        let period = search_from + rel;
        if period * 2 > s.len() {
            break;
        }
        if s.as_bytes()[period..] == s.as_bytes()[..s.len() - period] {
            return Some(&s[..period]);
        }
        search_from = period + 1;
        while search_from < s.len() && !s.is_char_boundary(search_from) {
            search_from += 1;
        }
    }
    None
}

fn is_redundant_window(
    key: &str,
    inner: &str,
    seen_keys: &HashSet<String>,
    seen_windows: &[(String, Option<String>)],
) -> bool {
    if seen_keys.contains(key) {
        return true;
    }
    if inner.len() < 40 {
        return false;
    }
    for (prev_inner, unit) in seen_windows {
        if prev_inner.contains(inner) {
            return true;
        }
        if let Some(unit) = unit {
            let repeats = inner.len() / unit.len() + 2;
            let repeated = unit.repeat(repeats);
            if repeated.contains(inner) {
                return true;
            }
        }
    }
    false
}

/// Up to `limit` non-overlapping, deduplicated windows of `line`, ranked best first.
fn intent_windows(
    line: &str,
    spans: &[IntentSpan],
    idf: &[f64],
    limit: usize,
) -> Vec<IntentWindow> {
    if line.len() <= INTENT_EXCERPT || spans.is_empty() {
        return vec![IntentWindow::at(line, 0, spans)];
    }
    let last = line.len() - INTENT_EXCERPT;
    // `(window_start, origin_span_start, distinct_terms, weight, tightest, inside_len)`
    let mut scored: Vec<(usize, usize, u32, f64, usize, usize)> = Vec::new();
    for span in spans {
        let start = line.floor_char_boundary(span.start.saturating_sub(INTENT_LEAD).min(last));
        if start < last
            && scored
                .last()
                .is_some_and(|&(previous, ..)| previous == start)
        {
            continue;
        }
        let end = line.floor_char_boundary((start + INTENT_EXCERPT).min(line.len()));
        let first = spans.partition_point(|inner| inner.start < start);
        let inside: Vec<IntentSpan> = spans[first..]
            .iter()
            .take_while(|inner| inner.start < end)
            .filter(|inner| inner.start + inner.len <= end)
            .copied()
            .collect();
        let (mut seen, mut weight) = (0u32, 0.0);
        for inner in &inside {
            if seen & (1 << inner.term) == 0 {
                seen |= 1 << inner.term;
                weight += idf[inner.term];
            }
        }
        scored.push((
            start,
            span.start,
            seen.count_ones(),
            weight,
            intent_tightest(&inside, seen),
            inside.len(),
        ));
    }
    scored.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then(b.3.total_cmp(&a.3))
            .then(a.4.cmp(&b.4))
            .then(b.5.cmp(&a.5))
            .then(a.0.cmp(&b.0))
    });
    let mut chosen: Vec<IntentWindow> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut seen_windows: Vec<(String, Option<String>)> = Vec::new();
    for (mut start, origin_span, ..) in scored {
        if chosen.len() == limit {
            break;
        }
        // If `start` dipped back into an already-chosen window (for example due to
        // `LEAD` or `.min(last)` near the end of the line) while `origin_span` itself
        // is uncovered, advance `start` to the end of that window instead of dropping
        // the uncovered tail match.
        if let Some(prev_end) = chosen
            .iter()
            .filter(|other| start < other.end && other.start <= origin_span)
            .map(|other| other.end)
            .max()
        {
            if origin_span < prev_end {
                continue;
            }
            start = line.ceil_char_boundary(prev_end).min(line.len());
        }
        // When an earlier window already captured a periodic boilerplate unit, skip
        // past any repeated boilerplate prefix at the start of this window so novel
        // content gets the full excerpt budget.
        for (_, unit) in &seen_windows {
            let Some(unit) = unit.as_deref() else {
                continue;
            };
            if let Some(first_hit) = line[start..].find(unit)
                && first_hit < unit.len()
            {
                let mut cursor = start + first_hit;
                let mut skipped = 0usize;
                while line[cursor..].starts_with(unit) {
                    cursor += unit.len();
                    skipped += 1;
                }
                if skipped >= 1 && cursor < line.len() {
                    start = line.ceil_char_boundary(cursor).min(line.len());
                }
            }
        }
        if start >= line.len() {
            continue;
        }
        let candidate = IntentWindow::at(line, start, spans);
        if candidate.spans.is_empty() {
            continue;
        }
        if chosen
            .iter()
            .any(|other| candidate.start < other.end && other.start < candidate.end)
        {
            continue;
        }
        let key = intent_dedup_key(&candidate.excerpt.text);
        let inner = trim_window_edges(&key, candidate.excerpt.before, candidate.excerpt.after);
        if is_redundant_window(&key, inner, &seen_keys, &seen_windows) {
            continue;
        }
        let unit = periodic_unit(inner).map(str::to_owned);
        let inner_owned = inner.to_owned();
        seen_keys.insert(key);
        seen_windows.push((inner_owned, unit));
        chosen.push(candidate);
    }
    if chosen.is_empty() {
        chosen.push(IntentWindow::at(line, 0, spans));
    }
    chosen
}

fn intent_tightest(spans: &[IntentSpan], terms: u32) -> usize {
    let mut counts = [0u32; INTENT_MAX_TERMS];
    let mut covered = 0u32;
    let mut best = usize::MAX;
    let mut left = 0;
    for right in spans {
        counts[right.term] += 1;
        covered |= 1 << right.term;
        while covered == terms && left < spans.len() {
            let first = spans[left];
            best = best.min((right.start + right.len).saturating_sub(first.start));
            counts[first.term] -= 1;
            if counts[first.term] == 0 {
                covered &= !(1 << first.term);
            }
            left += 1;
        }
    }
    best
}

struct IntentCandidate {
    line: usize,
    distinct: usize,
    score: f64,
    matches: usize,
    first: IntentWindow,
}

fn intent_rank(
    (distinct, score, line): (usize, f64, usize),
    (other_distinct, other_score, other_line): (usize, f64, usize),
) -> Ordering {
    distinct
        .cmp(&other_distinct)
        .then(score.total_cmp(&other_score))
        .then(other_line.cmp(&line))
}

impl IntentCandidate {
    fn key(&self) -> (usize, f64, usize) {
        (self.distinct, self.score, self.line)
    }
}

impl PartialEq for IntentCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for IntentCandidate {}
impl PartialOrd for IntentCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for IntentCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        intent_rank(self.key(), other.key())
    }
}

#[derive(Default)]
struct IntentRuns(Vec<(usize, usize)>);

impl IntentRuns {
    fn add(&mut self, line: usize) {
        if let Some((_, end)) = self.0.last_mut()
            && *end + 1 == line
        {
            *end = line;
        } else if self.0.len() < INTENT_RANGE_LIMIT {
            self.0.push((line, line));
        }
    }

    fn without(&self, shown: &[usize], wanted: usize) -> Vec<(usize, usize)> {
        let mut left = Vec::new();
        for &(first, last) in &self.0 {
            let mut start = first;
            let from = shown.partition_point(|&line| line < first);
            for &line in shown[from..].iter().take_while(|&&line| line <= last) {
                if line > start {
                    left.push((start, line - 1));
                }
                start = line + 1;
            }
            if start <= last {
                left.push((start, last));
            }
            if left.len() >= wanted {
                left.truncate(wanted);
                break;
            }
        }
        left
    }
}

struct IntentShown {
    line: usize,
    start: usize,
    end: usize,
    distinct: usize,
    matches: usize,
    entry: String,
}

struct IntentLayout {
    body: usize,
    used: usize,
    shown: Vec<IntentShown>,
    shown_keys: HashSet<String>,
}

impl IntentLayout {
    fn has(&self, line: usize) -> bool {
        self.shown.iter().any(|shown| shown.line == line)
    }

    fn place(
        &mut self,
        line: usize,
        distinct: usize,
        window: &IntentWindow,
        limit: usize,
        anchor: bool,
    ) -> bool {
        if self.shown.iter().any(|shown| {
            shown.line == line && (anchor || (window.start < shown.end && shown.start < window.end))
        }) {
            return false;
        }
        let marker = format!("stdout:{line}: ");
        let cap = limit.min(self.body.saturating_sub(marker.len() + 1));
        if cap == 0 {
            return false;
        }
        let (text, from, to) = window.excerpt.render(cap);
        let dedup_key = (distinct > 0).then(|| intent_dedup_key(&text));
        if let Some(ref key) = dedup_key
            && self.shown_keys.contains(key)
        {
            return false;
        }
        let entry = format!("{marker}{text}\n");
        if self.used + entry.len() > self.body {
            return false;
        }
        self.used += entry.len();
        if let Some(key) = dedup_key {
            self.shown_keys.insert(key);
        }
        let matches = window
            .spans
            .iter()
            .filter(|&&(start, len)| start >= from && start + len <= to)
            .count();
        self.shown.push(IntentShown {
            line,
            start: window.start,
            end: window.end,
            distinct,
            matches,
            entry,
        });
        true
    }
}

struct IntentTrailer<'a> {
    several: bool,
    every: usize,
    some: usize,
    duplicates: usize,
    inside: usize,
    cut: bool,
    runs: &'a str,
    log: &'a str,
}

impl IntentTrailer<'_> {
    fn render(&self) -> String {
        let mut text = String::from("[focus] ");
        if self.every == 0 && self.some == 0 {
            if self.cut {
                // A cut line may match past the part that was read.
            } else if self.inside == 0 {
                return "[focus] every match is shown\n".to_owned();
            } else {
                text.push_str("every matching line is shown");
            }
        } else if self.several {
            let (every_runs, some_runs) = if self.every > 0 {
                (self.runs, "")
            } else {
                ("", self.runs)
            };
            text.push_str(&format!(
                "{} more match every term{every_runs}, {} more match some{some_runs}",
                self.every, self.some
            ));
        } else {
            text.push_str(&format!("{} more match{}", self.every, self.runs));
        }
        let mut clauses = Vec::new();
        if self.duplicates > 0 && (self.every > 0 || self.some > 0) {
            let plural = if self.duplicates == 1 { "" } else { "s" };
            clauses.push(format!("{} duplicate{plural} collapsed", self.duplicates));
        }
        if self.inside > 0 {
            clauses.push(format!(
                "{} more matches inside shown lines; print one record per line to rank them",
                self.inside
            ));
        }
        if self.cut {
            clauses.push(format!(
                "lines over {INTENT_LINE_LIMIT_SHOWN} were ranked on their first {INTENT_LINE_LIMIT_SHOWN}"
            ));
        }
        for clause in clauses {
            if !text.ends_with(' ') {
                text.push_str("; ");
            }
            text.push_str(&clause);
        }
        text.push_str(self.log);
        text.push('\n');
        text
    }
}

fn intent_log_clause(justless: &str, run_id: &str, (first, last): (usize, usize)) -> String {
    format!(" · {justless} log {run_id} --stream stdout --from {first} --to {last}")
}

fn intent_show(intent: &str) -> String {
    let flat: String = intent
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let clipped = if flat.len() <= INTENT_SHOWN {
        flat
    } else {
        format!("{}…", &flat[..flat.floor_char_boundary(INTENT_SHOWN)])
    };
    if clipped.contains('"') {
        clipped
    } else {
        format!("\"{clipped}\"")
    }
}

fn intent_each_line(path: &Path, mut visit: impl FnMut(usize, &str, bool)) -> io::Result<()> {
    let mut file = BufReader::new(File::open(path)?);
    let mut raw = Vec::new();
    let mut number = 0;
    while let Some(cut) = intent_read_line(&mut file, &mut raw)? {
        number += 1;
        let text = String::from_utf8_lossy(&raw);
        let text = crate::capture::ANSI.replace_all(&text, "");
        visit(number, text.trim_end_matches(['\n', '\r']), cut);
        raw.clear();
    }
    Ok(())
}

fn intent_read_line(file: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<Option<bool>> {
    let mut read = false;
    let mut cut = false;
    loop {
        let chunk = file.fill_buf()?;
        if chunk.is_empty() {
            return Ok(read.then_some(cut));
        }
        read = true;
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let length = newline.map_or(chunk.len(), |index| index + 1);
        let kept = length.min(INTENT_LINE_LIMIT.saturating_sub(line.len()));
        cut |= chunk[..length][kept..].iter().any(|&byte| byte != b'\n');
        line.extend_from_slice(&chunk[..kept]);
        file.consume(length);
        if newline.is_some() {
            return Ok(Some(cut));
        }
    }
}

/// Ranks stdout lines and windows against `intent` within `max_bytes`, collapsing
/// duplicate boilerplate lines and redundant/overlapping windows so distinct matches
/// maximize the preview budget.
fn intent_preview(
    stdout: &Path,
    intent: &str,
    max_bytes: usize,
    justless: &str,
    run_id: &str,
) -> Result<Vec<u8>> {
    let terms = intent_terms(intent);
    ensure!(!terms.is_empty(), "the intent has no search terms");
    let mut analysis = IntentAnalysis::new(&terms);

    let mut lines = 0usize;
    let mut words = 0usize;
    let mut cut = false;
    let mut frequency = vec![0usize; terms.len()];
    intent_each_line(stdout, |_, line, clipped| {
        analysis.run(line, &terms);
        lines += 1;
        words += analysis.words;
        cut |= clipped;
        for (count, &n) in frequency.iter_mut().zip(&analysis.counts) {
            *count += usize::from(n > 0);
        }
    })?;
    if frequency.iter().all(|&count| count == 0) {
        return crate::focus::preview(stdout, intent, max_bytes, justless, run_id);
    }

    let shown_intent = intent_show(intent);
    let average = if words == 0 {
        1.0
    } else {
        words as f64 / lines as f64
    };
    let idf: Vec<f64> = frequency
        .iter()
        .map(|&df| (1.0 + (lines as f64 - df as f64 + 0.5) / (df as f64 + 0.5)).ln())
        .collect();
    let all = (1u32 << terms.len()) - 1;
    let several = terms.len() > 1;
    let keep = max_bytes / 24 + 1;
    let mut key_best: HashMap<String, IntentCandidate> = HashMap::new();
    let mut overflow_best: BinaryHeap<Reverse<IntentCandidate>> = BinaryHeap::new();
    let (mut every, mut some, mut total) = (0usize, 0usize, 0usize);
    let mut duplicate_matches = 0usize;
    let (mut every_runs, mut some_runs) = (IntentRuns::default(), IntentRuns::default());
    let (mut distinct_every_runs, mut distinct_some_runs) =
        (IntentRuns::default(), IntentRuns::default());
    let mut recent: VecDeque<(usize, u32)> = VecDeque::new();
    let mut nearby = false;
    let mut anchors: Vec<(usize, usize, usize, IntentWindow)> = Vec::new();

    intent_each_line(stdout, |number, line, _| {
        analysis.run(line, &terms);
        let distinct = analysis.distinct();
        let anchor = (number == 1 || number == lines) && !line.trim().is_empty();
        if distinct == 0 {
            if anchor {
                anchors.push((number, 0, 0, IntentWindow::at(line, 0, &[])));
            }
            return;
        }
        let matches = analysis.matches();
        total += matches;
        if distinct == terms.len() {
            every += 1;
            every_runs.add(number);
        } else {
            some += 1;
            some_runs.add(number);
            let mask = analysis.mask();
            while recent
                .front()
                .is_some_and(|&(line_no, _)| line_no + INTENT_NEARBY <= number)
            {
                recent.pop_front();
            }
            nearby |= recent.iter().fold(mask, |union, &(_, other)| union | other) == all;
            recent.push_back((number, mask));
        }
        let length = analysis.words as f64 / average;
        let score: f64 = analysis
            .counts
            .iter()
            .zip(&idf)
            .filter(|&(&count, _)| count > 0)
            .map(|(&count, idf)| {
                let tf = f64::from(count);
                idf * tf * (INTENT_K1 + 1.0)
                    / (tf + INTENT_K1 * (1.0 - INTENT_B + INTENT_B * length))
            })
            .sum();

        let (key, computed_first) = if line.len() <= INTENT_EXCERPT {
            (intent_dedup_key(line), None)
        } else {
            let first = intent_windows(line, &analysis.spans, &idf, 1).remove(0);
            (intent_dedup_key(&first.excerpt.text), Some(first))
        };

        if let Some(existing) = key_best.get_mut(&key) {
            duplicate_matches += 1;
            if intent_rank((distinct, score, number), existing.key()).is_gt() {
                let first = computed_first
                    .unwrap_or_else(|| intent_windows(line, &analysis.spans, &idf, 1).remove(0));
                *existing = IntentCandidate {
                    line: number,
                    distinct,
                    score,
                    matches,
                    first,
                };
            }
            return;
        }

        if distinct == terms.len() {
            distinct_every_runs.add(number);
        } else {
            distinct_some_runs.add(number);
        }

        if key_best.len() >= INTENT_MAX_DEDUP_KEYS {
            let dropped = overflow_best.len() >= keep
                && overflow_best.peek().is_some_and(|Reverse(worst)| {
                    intent_rank((distinct, score, number), worst.key()).is_le()
                });
            if dropped && !anchor {
                return;
            }
            let first = computed_first
                .unwrap_or_else(|| intent_windows(line, &analysis.spans, &idf, 1).remove(0));
            if anchor {
                anchors.push((number, distinct, matches, first.clone()));
            }
            if !dropped {
                overflow_best.push(Reverse(IntentCandidate {
                    line: number,
                    distinct,
                    score,
                    matches,
                    first,
                }));
                if overflow_best.len() > keep {
                    overflow_best.pop();
                }
            }
            return;
        }

        let first = computed_first
            .unwrap_or_else(|| intent_windows(line, &analysis.spans, &idf, 1).remove(0));
        if anchor {
            anchors.push((number, distinct, matches, first.clone()));
        }
        key_best.insert(
            key,
            IntentCandidate {
                line: number,
                distinct,
                score,
                matches,
                first,
            },
        );
    })?;

    let mut ranked: Vec<IntentCandidate> = key_best
        .into_values()
        .chain(overflow_best.into_iter().map(|Reverse(c)| c))
        .collect();
    ranked.sort_by(|a, b| b.cmp(a));
    ranked.truncate(keep);

    let mut header = if several {
        format!(
            "[focus] {shown_intent}: {every} of {lines} lines match every term, {some} match some\n"
        )
    } else {
        format!("[focus] {shown_intent}: {every} of {lines} lines match\n")
    };
    if several && every == 0 && nearby {
        header.push_str(
            "[focus] no line has every term, but neighboring lines do; print one record per line\n",
        );
    }

    let widest = "9".repeat(lines.to_string().len());
    let widest_runs = format!(" ({widest}-{widest}, {widest}-{widest}, {widest}-{widest}, …)");
    let widest_log = intent_log_clause(justless, run_id, (lines, lines));
    let reserve = |runs: &str, log: &str, duplicates: usize| {
        [(every, some), (0, 0)]
            .into_iter()
            .map(|(every, some)| {
                IntentTrailer {
                    several,
                    every,
                    some,
                    duplicates,
                    inside: total,
                    cut,
                    runs,
                    log,
                }
                .render()
                .len()
            })
            .max()
            .unwrap_or(0)
    };
    let full = max_bytes
        .saturating_sub(header.len() + reserve(&widest_runs, &widest_log, duplicate_matches));
    let roomy = full >= INTENT_ROOMY;
    let body = if roomy {
        full
    } else {
        max_bytes.saturating_sub(header.len() + reserve("", "", 0))
    };

    let mut layout = IntentLayout {
        body,
        used: 0,
        shown: Vec::new(),
        shown_keys: HashSet::new(),
    };
    if let Some(top) = ranked.first() {
        layout.place(top.line, top.distinct, &top.first, INTENT_EXCERPT, false);
    }
    for (line, distinct, _, window) in anchors.iter().rev() {
        layout.place(*line, *distinct, window, INTENT_ANCHOR, true);
    }
    for candidate in ranked.iter().skip(1) {
        layout.place(
            candidate.line,
            candidate.distinct,
            &candidate.first,
            INTENT_EXCERPT,
            false,
        );
    }

    let room = layout.body.saturating_sub(layout.used) / INTENT_EXCERPT;
    let mut longer: Vec<&IntentCandidate> = ranked
        .iter()
        .filter(|candidate| {
            candidate.matches > candidate.first.spans.len() && layout.has(candidate.line)
        })
        .collect();
    if room > 0 && !longer.is_empty() {
        let each = room / longer.len() + 2;
        let mut wanted: Vec<usize> = longer.iter().map(|candidate| candidate.line).collect();
        wanted.sort_unstable();
        let mut found: Vec<(usize, Vec<IntentWindow>)> = Vec::new();
        intent_each_line(stdout, |number, line, _| {
            if wanted.binary_search(&number).is_ok() {
                analysis.run(line, &terms);
                found.push((number, intent_windows(line, &analysis.spans, &idf, each)));
            }
        })?;
        for index in 1..each {
            let mut placed = false;
            longer.retain(|candidate| {
                let window = found
                    .iter()
                    .find(|(line, _)| *line == candidate.line)
                    .and_then(|(_, windows)| windows.get(index));
                let Some(window) = window else {
                    return false;
                };
                placed |= layout.place(
                    candidate.line,
                    candidate.distinct,
                    window,
                    INTENT_EXCERPT,
                    false,
                );
                true
            });
            if !placed {
                break;
            }
        }
    }

    let mut shown = layout.shown;
    shown.sort_by_key(|shown| (shown.line, shown.start));

    let mut lines_shown: Vec<(usize, usize)> = Vec::new();
    let mut inside = 0usize;
    for entry in &shown {
        if lines_shown
            .last()
            .is_none_or(|&(line, _)| line != entry.line)
        {
            lines_shown.push((entry.line, entry.distinct));
            let matches = ranked
                .iter()
                .find(|candidate| candidate.line == entry.line)
                .map(|candidate| candidate.matches)
                .or_else(|| {
                    anchors
                        .iter()
                        .find(|anchor| anchor.0 == entry.line)
                        .map(|anchor| anchor.2)
                })
                .unwrap_or(0);
            let visible: usize = shown
                .iter()
                .filter(|other| other.line == entry.line)
                .map(|other| other.matches)
                .sum();
            inside += matches.saturating_sub(visible);
        }
    }
    let shown_every = lines_shown
        .iter()
        .filter(|&&(_, distinct)| distinct == terms.len())
        .count();
    let shown_some = lines_shown
        .iter()
        .filter(|&&(_, distinct)| distinct > 0 && distinct < terms.len())
        .count();
    let (every_left, some_left) = (every - shown_every, some - shown_some);

    let numbers: Vec<usize> = lines_shown.iter().map(|&(line, _)| line).collect();
    let (primary_runs, fallback_runs, left) = if every_left > 0 {
        (&distinct_every_runs, &every_runs, every_left)
    } else {
        (&distinct_some_runs, &some_runs, some_left)
    };
    let next = if roomy {
        let distinct_next = primary_runs.without(&numbers, INTENT_RANGES_SHOWN);
        if distinct_next.is_empty() {
            fallback_runs.without(&numbers, INTENT_RANGES_SHOWN)
        } else {
            distinct_next
        }
    } else {
        Vec::new()
    };
    let listed: usize = next.iter().map(|&(first, last)| last - first + 1).sum();
    let runs_text = if next.is_empty() {
        String::new()
    } else {
        let mut parts: Vec<String> = next
            .iter()
            .map(|&(first, last)| {
                if first == last {
                    first.to_string()
                } else {
                    format!("{first}-{last}")
                }
            })
            .collect();
        if listed < left {
            parts.push("…".to_owned());
        }
        format!(" ({})", parts.join(", "))
    };
    let log = next
        .first()
        .map(|&range| intent_log_clause(justless, run_id, range))
        .unwrap_or_default();

    let mut output = String::with_capacity(max_bytes);
    output.push_str(&header);
    for shown in &shown {
        output.push_str(&shown.entry);
    }
    let shown_duplicates = if roomy {
        duplicate_matches.min(every_left + some_left)
    } else {
        0
    };
    output.push_str(
        &IntentTrailer {
            several,
            every: every_left,
            some: some_left,
            duplicates: shown_duplicates,
            inside,
            cut,
            runs: &runs_text,
            log: &log,
        }
        .render(),
    );
    ensure!(
        output.len() <= max_bytes,
        "the focused preview needs more than {max_bytes} bytes"
    );
    Ok(output.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shebang(code: &str) -> Option<String> {
        Shebang::parse(code.as_bytes()).map(|line| line.program)
    }

    #[test]
    fn shebang_names_the_interpreter() {
        for (code, program) in [
            ("#!/bin/sh\necho ok", Some("sh")),
            ("#!/usr/bin/env python3\n", Some("python3")),
            ("#!/usr/bin/env -S python3 -u\n", Some("python3")),
            ("#!/usr/bin/env PYTHONUTF8=1 python3\n", Some("python3")),
            ("#!/usr/bin/env -u PYTHONPATH python3\n", Some("python3")),
            (
                "#! /usr/local/bin/python3.12\r\nprint(1)",
                Some("python3.12"),
            ),
            ("#!/usr/bin/env node\r\n", Some("node")),
            ("#!/usr/bin/env Rscript", Some("Rscript")),
            ("#!/usr/bin/env\n", None),
            ("#!/usr/bin/env -S\n", None),
            ("#!\n", None),
            ("print(1)\n#!/bin/sh", None),
        ] {
            assert_eq!(shebang(code).as_deref(), program, "{code:?}");
        }
    }

    #[test]
    fn shebang_keeps_options_and_variables() {
        let parse = |code: &str| Shebang::parse(code.as_bytes()).unwrap();
        let options = |code: &str| parse(code).options;
        assert_eq!(options("#!/bin/sh -e\nfalse"), ["-e"]);
        assert_eq!(options("#!/usr/bin/env -S bash -eu\r\n"), ["-eu"]);
        assert_eq!(options("#!/usr/bin/perl -w -T\n"), ["-w", "-T"]);
        assert_eq!(options("#!/usr/bin/env python3\n"), Vec::<String>::new());
        assert_eq!(
            options("#!/usr/bin/env -S deno run --allow-read=./data\n"),
            ["run", "--allow-read=./data"]
        );
        assert_eq!(
            parse("#!/usr/bin/env -S PYTHONUTF8=1 A=b=c python3 -X dev\n"),
            Shebang {
                program: "python3".into(),
                options: vec!["-X".into(), "dev".into()],
                env: vec![
                    ("PYTHONUTF8".into(), "1".into()),
                    ("A".into(), "b=c".into())
                ],
            }
        );
    }

    #[test]
    fn deno_and_bun_options_replace_the_default_run() {
        let prefix = |lang: Lang, options: &str| {
            let options: Vec<String> = options.split_whitespace().map(String::from).collect();
            let prefix = lang.prefix(&options);
            assert!(prefix.iter().all(|word| !word.contains(' ')), "{prefix:?}");
            prefix.join(" ")
        };
        assert_eq!(prefix(Lang::Deno, ""), "run --allow-all --quiet");
        assert_eq!(
            prefix(Lang::Deno, "run --allow-read=."),
            "run --allow-read=."
        );
        assert_eq!(prefix(Lang::Deno, "--allow-net"), "run --allow-net");
        assert_eq!(prefix(Lang::Bun, ""), "run");
        assert_eq!(prefix(Lang::Bun, "--smol"), "run --smol");
        assert_eq!(prefix(Lang::Sh, "-e"), "-e");
        assert!(Lang::Python3.prefix(&[]).is_empty());
    }

    #[test]
    fn saved_scripts_are_named_relative_to_the_working_directory() {
        let root = Path::new("/w");
        let script = Path::new("/w/.justless/runs/run-1/script.py");
        assert_eq!(
            relative(script, root, root),
            Path::new(".justless/runs/run-1/script.py")
        );
        assert_eq!(
            relative(script, Path::new("/w/src/app"), root),
            Path::new("../../.justless/runs/run-1/script.py")
        );
        assert_eq!(relative(script, Path::new("/elsewhere"), root), script);
    }

    /// A zombie left in the group, as under an init that does not reap, is not running.
    #[cfg(unix)]
    #[test]
    fn ps_tells_running_members_from_zombies() {
        use std::os::unix::process::CommandExt;
        if Command::new("ps").arg("-A").output().is_err() {
            return;
        }
        let mut sleeper = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let group = sleeper.id();
        assert!(running_in(group));
        sleeper.kill().unwrap();
        // Unreaped, the sleeper stays a zombie member of its group.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!running_in(group));
        sleeper.wait().unwrap();
        assert!(!running_in(group));
    }

    #[test]
    fn interpreters_map_to_supported_languages() {
        for (name, lang) in [
            ("sh", Lang::Sh),
            ("dash", Lang::Sh),
            ("ash", Lang::Sh),
            ("bash", Lang::Bash),
            ("python", Lang::Python3),
            ("python3", Lang::Python3),
            ("python3.12", Lang::Python3),
            ("node", Lang::Node),
            ("nodejs", Lang::Node),
            ("ruby", Lang::Ruby),
            ("perl", Lang::Perl),
            ("deno", Lang::Deno),
            ("bun", Lang::Bun),
        ] {
            assert_eq!(Lang::from_interpreter(name), Some(lang), "{name}");
        }
        for name in ["Rscript", "python2", "python3.", "python3.x", "zsh", "env"] {
            assert_eq!(Lang::from_interpreter(name), None, "{name}");
        }
    }

    #[test]
    fn script_extensions_map_to_languages() {
        for (file, lang) in [
            ("tools/x.sh", Some(Lang::Sh)),
            ("x.bash", Some(Lang::Bash)),
            ("x.py", Some(Lang::Python3)),
            ("x.js", Some(Lang::Node)),
            ("x.mjs", Some(Lang::Node)),
            ("x.cjs", Some(Lang::Node)),
            ("x.rb", Some(Lang::Ruby)),
            ("x.pl", Some(Lang::Perl)),
            ("x.ts", None),
            ("Makefile", None),
        ] {
            assert_eq!(Lang::from_extension(Path::new(file)), lang, "{file}");
        }
        assert_eq!(Lang::parse("python"), Some(Lang::Python3));
        assert_eq!(Lang::parse("js"), Some(Lang::Node));
        assert_eq!(Lang::Bun.extension(), "ts");
        assert_eq!(Lang::Python3.candidates(), ["python3", "python"]);
    }

    #[test]
    fn json_exec_uses_deep_schema_jsonview_and_saves_bytes() {
        let root = Path::new("/workspace");
        let rows: Vec<String> = (1..=120)
            .map(|i| {
                format!(
                    r#"  {{"id": {i}, "service": "payments", "user": {{"name": "user{i}", "tier": "pro"}}, "metrics": {{"latency_ms": {}, "retries": {}}}, "status": "ok"}}"#,
                    10 + (i % 50),
                    i % 3
                )
            })
            .collect();
        let raw = format!("[\n{}\n]\n", rows.join(",\n"));
        assert!(raw.len() > 15_000, "raw={}", raw.len());

        let shaped = match shape_text_preview(&raw, 4096, root, "stdout", false, None) {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected deep-schema jsonview for large JSON array"),
        };
        assert!(shaped.len() <= 4096, "shaped={}", shaped.len());
        assert!(
            shaped.len() * 3 < raw.len(),
            "expected >66% reduction: raw={}, shaped={}",
            raw.len(),
            shaped.len()
        );
        assert!(shaped.starts_with("[JSON array as a table:"), "{shaped}");
        assert!(shaped.contains("user{name,tier=\"pro\"}"), "{shaped}");
        assert!(shaped.contains("metrics{latency_ms,retries}"), "{shaped}");
        assert!(shaped.contains("user120"), "{shaped}");
    }

    fn test_redactor() -> &'static crate::redact::Redactor {
        let dir = tempfile::TempDir::new().unwrap();
        let ws = Workspace::discover(Some(dir.path())).unwrap();
        crate::redact::for_workspace(&ws).expect("default redactor enabled")
    }

    #[test]
    fn json_exec_redacts_secrets_before_jsonview() {
        let root = Path::new("/workspace");
        let redactor = test_redactor();
        let secret = format!("ghp_{}", "aB3dE6gH9jK2mN5pQ8rS1tU4vW7xY0zC3eF6");
        let rows: Vec<String> = (1..=80)
            .map(|i| {
                format!(
                    r#"  {{"id": {i}, "token": "{secret}", "user": {{"name": "u{i}", "role": "svc"}}, "ok": true}}"#
                )
            })
            .collect();
        let raw = format!("[\n{}\n]\n", rows.join(",\n"));
        assert!(raw.len() > 8_000);

        let shaped = match shape_text_preview(&raw, 4096, root, "stdout", false, Some(redactor)) {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected redacted jsonview"),
        };
        assert!(shaped.len() <= 4096, "shaped={}", shaped.len());
        assert!(shaped.len() < raw.len());
        assert!(!shaped.contains(&secret), "secret leaked:\n{shaped}");
        assert!(shaped.contains("[redacted:"), "{shaped}");
    }

    #[test]
    fn ndjson_data_uses_jsonview_while_ndjson_logs_use_logview() {
        let root = Path::new("/workspace");

        // Pure NDJSON data records -> lossless jsonview.
        let data_ndjson: String = (1..=80)
            .map(|i| {
                format!(
                    r#"{{"id":{i},"sku":"SKU-{i:04}","warehouse":{{"region":"us-east","bin":{}}},"qty":{}}}"#,
                    i % 10,
                    i * 2
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert!(data_ndjson.len() > 4096);
        assert!(!looks_like_json_logs(&data_ndjson));
        let data_shaped = match shape_text_preview(&data_ndjson, 4096, root, "stdout", false, None)
        {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected jsonview for NDJSON data"),
        };
        assert!(
            data_shaped.starts_with("[JSON lines as a table:"),
            "{data_shaped}"
        );
        assert!(data_shaped.len() <= 4096);
        assert!(data_shaped.len() < data_ndjson.len() / 2);

        // NDJSON structured logs with late errors -> logview folds chatter & keeps errors.
        let mut log_lines = Vec::new();
        for i in 1..=200 {
            log_lines.push(format!(
                r#"{{"ts":"2026-04-01T10:00:{:02}.{:03}Z","level":"INFO","service":"api","msg":"GET /health 200 ok req_id={i}"}}"#,
                i % 60,
                i % 1000
            ));
        }
        log_lines.push(
            r#"{"ts":"2026-04-01T10:04:01.000Z","level":"ERROR","service":"api","msg":"database pool exhausted on shard-7"}"#
                .to_owned(),
        );
        log_lines.push(
            r#"{"ts":"2026-04-01T10:04:02.000Z","level":"WARN","service":"api","msg":"circuit breaker opened for payments"}"#
                .to_owned(),
        );
        let log_ndjson = log_lines.join("\n") + "\n";
        assert!(log_ndjson.len() > 18_000);
        assert!(looks_like_json_logs(&log_ndjson));

        let log_shaped = match shape_text_preview(&log_ndjson, 4096, root, "stdout", false, None) {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected logview for NDJSON structured logs"),
        };
        assert!(log_shaped.len() <= 4096);
        assert!(
            log_shaped.len() * 10 < log_ndjson.len(),
            "expected >90% reduction on NDJSON logs: raw={}, shaped={}",
            log_ndjson.len(),
            log_shaped.len()
        );
        assert!(
            log_shaped.contains("database pool exhausted on shard-7"),
            "{log_shaped}"
        );
        assert!(
            log_shaped.contains("circuit breaker opened for payments"),
            "{log_shaped}"
        );
        assert!(log_shaped.contains("[stdout:201]"), "{log_shaped}");
    }

    #[test]
    fn stacktrace_collapsing_preserves_workspace_and_redacted_frames() {
        let root = Path::new("/workspace");

        // 1. Python traceback (> 4096 raw) collapsing to fit in budget.
        let mut py = String::from("Traceback (most recent call last):\n");
        py.push_str(
            "  File \".justless/runs/run-1/script.py\", line 14, in main\n    fetch_orders()\n",
        );
        py.push_str(
            "  File \"/workspace/src/worker.py\", line 88, in fetch_orders\n    client.get(url)\n",
        );
        for i in 0..45 {
            py.push_str(&format!(
                "  File \"/usr/lib/python3.12/site-packages/urllib3/connectionpool.py\", line {}, in urlopen\n    response = self._make_request(conn, method, url, timeout=timeout, chunked=chunked)\n",
                100 + i
            ));
        }
        py.push_str("  File \"/workspace/src/retry.py\", line 22, in on_error\n    raise RuntimeError('upstream 503')\n");
        py.push_str("RuntimeError: upstream 503\n");
        assert!(py.len() > 4096, "py.len()={}", py.len());

        let py_shaped = match shape_text_preview(&py, 4096, root, "stdout", false, None) {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected collapsed Python stacktrace"),
        };
        assert!(py_shaped.len() <= 4096);
        assert!(
            py_shaped.len() * 4 < py.len(),
            "expected >75% reduction on Python trace: raw={}, shaped={}",
            py.len(),
            py_shaped.len()
        );
        assert!(
            py_shaped.contains(".justless/runs/run-1/script.py"),
            "{py_shaped}"
        );
        assert!(
            py_shaped.contains("/workspace/src/worker.py"),
            "{py_shaped}"
        );
        assert!(py_shaped.contains("/workspace/src/retry.py"), "{py_shaped}");
        assert!(
            py_shaped.contains("RuntimeError: upstream 503"),
            "{py_shaped}"
        );
        assert!(
            py_shaped.contains("runtime/dependency frames"),
            "{py_shaped}"
        );
        assert!(py_shaped.contains("in stdout:"), "{py_shaped}");

        // 2. Node.js stack trace (> 4096 raw).
        let mut node = String::from("Error: ECONNREFUSED 127.0.0.1:5432\n");
        node.push_str("    at connect (/workspace/src/db.js:42:11)\n");
        for i in 0..55 {
            node.push_str(&format!(
                "    at Layer.handle (/workspace/node_modules/express/lib/router/layer.js:{}:5)\n",
                90 + i
            ));
        }
        for i in 0..20 {
            node.push_str(&format!(
                "    at processTicksAndRejections (node:internal/process/task_queues:{}:5)\n",
                50 + i
            ));
        }
        node.push_str("    at Object.<anonymous> (.justless/runs/run-2/script.js:8:1)\n");
        assert!(node.len() > 4096, "node.len()={}", node.len());

        let node_shaped = match shape_text_preview(&node, 4096, root, "stdout", false, None) {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected collapsed Node stacktrace"),
        };
        assert!(node_shaped.len() <= 4096);
        assert!(
            node_shaped.len() * 5 < node.len(),
            "expected >80% reduction on Node trace: raw={}, shaped={}",
            node.len(),
            node_shaped.len()
        );
        assert!(
            node_shaped.contains("/workspace/src/db.js:42:11"),
            "{node_shaped}"
        );
        assert!(
            node_shaped.contains(".justless/runs/run-2/script.js:8:1"),
            "{node_shaped}"
        );
        assert!(node_shaped.contains("in stdout:"), "{node_shaped}");

        // 3. Ruby backtrace (> 4096 raw).
        let mut ruby = String::from(
            ".justless/runs/run-3/script.rb:12:in `process': connection reset (RuntimeError)\n",
        );
        ruby.push_str("\tfrom /workspace/lib/client.rb:44:in `fetch'\n");
        for i in 0..60 {
            ruby.push_str(&format!(
                "\tfrom /usr/lib/ruby/gems/3.2.0/gems/activerecord-7.1.3/lib/active_record/connection_adapters/abstract_adapter.rb:{}:in `exec_query'\n",
                200 + i
            ));
        }
        ruby.push_str("\tfrom .justless/runs/run-3/script.rb:20:in `<main>'\n");
        assert!(ruby.len() > 4096, "ruby.len()={}", ruby.len());

        let ruby_shaped = match shape_text_preview(&ruby, 4096, root, "stdout", false, None) {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected collapsed Ruby stacktrace"),
        };
        assert!(ruby_shaped.len() <= 4096);
        assert!(
            ruby_shaped.len() * 10 < ruby.len(),
            "expected >90% reduction on Ruby trace: raw={}, shaped={}",
            ruby.len(),
            ruby_shaped.len()
        );
        assert!(
            ruby_shaped.contains("/workspace/lib/client.rb:44"),
            "{ruby_shaped}"
        );
        assert!(
            ruby_shaped.contains(".justless/runs/run-3/script.rb:20"),
            "{ruby_shaped}"
        );
        assert!(ruby_shaped.contains("activerecord"), "{ruby_shaped}");
        assert!(ruby_shaped.contains("in stdout:3-62"), "{ruby_shaped}");

        // 4. Go, Rust, and Java stack traces collapse external frames while keeping workspace frames.
        let mut go = String::from("panic: nil pointer dereference\n\ngoroutine 1 [running]:\n");
        go.push_str("main.handleOrder(0x0)\n\t/workspace/cmd/server/main.go:42 +0x1a\n");
        for i in 0..35 {
            go.push_str(&format!(
                "net/http.(*conn).serve(0xc00012a000)\n\t/usr/local/go/src/net/http/server.go:{} +0x3c\n",
                1900 + i
            ));
        }
        go.push_str("main.main()\n\t/workspace/cmd/server/main.go:12 +0x20\n");
        let go_shaped = match shape_text_preview(&go, 4096, root, "stdout", false, None) {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected collapsed Go stacktrace"),
        };
        assert!(
            go_shaped.len() * 4 < go.len(),
            "raw={}, shaped={}",
            go.len(),
            go_shaped.len()
        );
        assert!(
            go_shaped.contains("/workspace/cmd/server/main.go:42"),
            "{go_shaped}"
        );
        assert!(go_shaped.contains("in stdout:"), "{go_shaped}");

        let mut rust =
            String::from("thread 'main' panicked at src/worker.rs:19:5:\nboom\nstack backtrace:\n");
        rust.push_str("   0: my_app::worker::run\n             at /workspace/src/worker.rs:19:5\n");
        for i in 1..=35 {
            rust.push_str(&format!(
                "  {i:2}: std::rt::lang_start_internal\n             at /rustc/abcdef/library/std/src/rt.rs:{}:13\n",
                120 + i
            ));
        }
        rust.push_str("  36: my_app::main\n             at /workspace/src/main.rs:7:5\n");
        let rust_shaped = match shape_text_preview(&rust, 4096, root, "stdout", false, None) {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected collapsed Rust backtrace"),
        };
        assert!(
            rust_shaped.len() * 4 < rust.len(),
            "raw={}, shaped={}",
            rust.len(),
            rust_shaped.len()
        );
        assert!(
            rust_shaped.contains("/workspace/src/worker.rs:19:5"),
            "{rust_shaped}"
        );
        assert!(
            rust_shaped.contains("/workspace/src/main.rs:7:5"),
            "{rust_shaped}"
        );
        assert!(rust_shaped.contains("in stdout:"), "{rust_shaped}");

        let mut java = String::from("java.lang.IllegalStateException: ledger locked\n");
        java.push_str("\tat com.acme.billing.Ledger.commit(Ledger.java:88)\n");
        for i in 0..55 {
            java.push_str(&format!(
                "\tat org.springframework.web.filter.OncePerRequestFilter.doFilter(OncePerRequestFilter.java:{})\n",
                100 + i
            ));
        }
        java.push_str("\tat com.acme.billing.App.main(App.java:14)\n");
        let java_shaped = match shape_text_preview(&java, 4096, root, "stdout", false, None) {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected collapsed Java stacktrace"),
        };
        assert!(
            java_shaped.len() * 5 < java.len(),
            "raw={}, shaped={}",
            java.len(),
            java_shaped.len()
        );
        assert!(
            java_shaped.contains("com.acme.billing.Ledger.commit"),
            "{java_shaped}"
        );
        assert!(
            java_shaped.contains("com.acme.billing.App.main"),
            "{java_shaped}"
        );
        assert!(java_shaped.contains("in stdout:"), "{java_shaped}");

        // 5. Redaction safety: a secret in a stack trace is redacted before collapsing and never leaks.
        let redactor = test_redactor();
        let secret = format!("ghp_{}", "zY9xW8vU7tS6rQ5pO4nM3lK2jI1hG0fE9dC8");
        let ruby_secret = ruby.replace("connection reset", &format!("token={secret}"));
        let redacted_trace =
            match shape_text_preview(&ruby_secret, 4096, root, "stdout", false, Some(redactor)) {
                ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
                _ => panic!("expected collapsed redacted Ruby trace"),
            };
        assert!(!redacted_trace.contains(&secret), "{redacted_trace}");
        assert!(redacted_trace.contains("[redacted:"), "{redacted_trace}");
    }

    #[test]
    fn structured_logview_folds_repetitive_chatter_and_skips_plain_exports() {
        let root = Path::new("/workspace");

        // Plain numbered output must NOT be routed to logview.
        let numbered: String = (1..=500).map(|i| format!("line {i} ok\n")).collect();
        assert!(!looks_like_structured_logs(&numbered));
        assert!(matches!(
            shape_text_preview(&numbered, 1024, root, "stdout", false, None),
            ShapedText::Fallback
        ));

        // Timestamped / level-prefixed repetitive logs with embedded warnings/errors -> logview.
        let mut logs = String::new();
        for i in 1..=300 {
            logs.push_str(&format!(
                "2026-04-01T12:00:{:02}.{:03}Z INFO worker polling queue batch={} latency={}ms\n",
                i % 60,
                i % 1000,
                i,
                3 + (i % 5)
            ));
            if i == 150 {
                logs.push_str(
                    "2026-04-01T12:02:30.000Z ERROR worker failed to commit order_id=ORD-991: deadlock detected\n",
                );
            }
            if i == 250 {
                logs.push_str(
                    "2026-04-01T12:04:10.000Z WARN worker retry queue depth exceeded 500\n",
                );
            }
        }
        assert!(logs.len() > 20_000, "logs.len()={}", logs.len());
        assert!(looks_like_structured_logs(&logs));

        let folded = match shape_text_preview(&logs, 4096, root, "stdout", false, None) {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected logview folding for repetitive timestamped logs"),
        };
        assert!(folded.len() <= 4096);
        assert!(
            folded.len() * 10 < logs.len(),
            "expected >90% reduction on timestamped logs: raw={}, folded={}",
            logs.len(),
            folded.len()
        );
        assert!(folded.contains("deadlock detected"), "{folded}");
        assert!(
            folded.contains("retry queue depth exceeded 500"),
            "{folded}"
        );
        assert!(folded.contains("[stdout:151]"), "{folded}");

        // Secrets in structured logs are redacted before folding and never leak.
        let redactor = test_redactor();
        let secret = format!("ghp_{}", "mN1oP2qR3sT4uV5wX6yZ7aB8cD9eF0gH1iJ2");
        let logs_with_secret = logs.replace("deadlock detected", &format!("auth={secret}"));
        let safe = match shape_text_preview(
            &logs_with_secret,
            4096,
            root,
            "stdout",
            false,
            Some(redactor),
        ) {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected redacted logview"),
        };
        assert!(!safe.contains(&secret), "{safe}");
        assert!(safe.contains("[redacted:"), "{safe}");
    }

    #[test]
    fn intent_dedup_surfaces_distinct_matches_past_boilerplate_and_periodic_windows() {
        let dir = tempfile::TempDir::new().unwrap();

        // Case 1: 250 timestamp-varying boilerplate matches before 10 distinct matches.
        let mut lines = String::new();
        for i in 1..=250 {
            lines.push_str(&format!(
                "2026-04-01T10:{:02}:{:02}Z WARN payments timeout retrying\n",
                (i / 60) % 60,
                i % 60
            ));
        }
        let distinct_reasons = [
            "stripe_eu_504",
            "adyen_tls_reset",
            "card_vault_eof",
            "ledger_deadlock",
            "fx_quote_stale",
            "fraud_rpc_timeout",
            "bank_ach_rejected",
            "webhook_dns_nxdomain",
            "kms_sign_throttled",
            "redis_cluster_failover",
        ];
        for reason in distinct_reasons {
            lines.push_str(&format!(
                "2026-04-01T10:15:00Z ERROR payments timeout fatal cause={reason} detail=checkout_pipeline_v2\n"
            ));
        }
        let path = dir.path().join("boilerplate.log");
        fs::write(&path, &lines).unwrap();

        // Baseline focus::preview gets swamped by boilerplate and misses the distinct causes.
        let baseline = String::from_utf8(
            crate::focus::preview(&path, "payments timeout", 2048, "justless", "run-1").unwrap(),
        )
        .unwrap();
        let baseline_distinct = distinct_reasons
            .iter()
            .filter(|&&r| baseline.contains(r))
            .count();

        let deduped = String::from_utf8(
            intent_preview(&path, "payments timeout", 2048, "justless", "run-1").unwrap(),
        )
        .unwrap();
        assert!(deduped.len() <= 2048, "len={}", deduped.len());
        let deduped_distinct = distinct_reasons
            .iter()
            .filter(|&&r| deduped.contains(r))
            .count();
        assert_eq!(
            deduped_distinct,
            distinct_reasons.len(),
            "expected all {} distinct causes (baseline got {baseline_distinct}):\n{deduped}",
            distinct_reasons.len()
        );
        assert!(deduped.contains("duplicates collapsed"), "{deduped}");
        assert!(deduped.len() < lines.len() / 8);

        // Case 2: One long line with periodic boilerplate chunks followed by distinct records.
        let mut long_line =
            r#"{"service":"payments","status":"timeout","reason":"retry"}, "#.repeat(90);
        for tag in [
            "vault_unreachable",
            "stripe_504",
            "ledger_deadlock",
            "kms_eof",
        ] {
            long_line.push_str(&format!(
                r#"{{"service":"payments","status":"timeout","reason":"{tag}","unique":true}}, "#
            ));
        }
        long_line.push('\n');
        let long_path = dir.path().join("long.log");
        fs::write(&long_path, &long_line).unwrap();

        let long_preview = String::from_utf8(
            intent_preview(&long_path, "payments timeout", 2048, "justless", "run-2").unwrap(),
        )
        .unwrap();
        assert!(long_preview.len() <= 2048, "len={}", long_preview.len());
        for tag in [
            "vault_unreachable",
            "stripe_504",
            "ledger_deadlock",
            "kms_eof",
        ] {
            assert!(
                long_preview.contains(tag),
                "missing distinct window {tag} in:\n{long_preview}"
            );
        }
        // Only one boilerplate window should appear, not a flood of identical repeats.
        let retry_windows = long_preview
            .lines()
            .filter(|l| {
                l.starts_with("stdout:1:")
                    && l.contains("\"reason\":\"retry\"")
                    && !l.contains("vault_unreachable")
            })
            .count();
        assert!(
            retry_windows <= 1,
            "expected at most 1 boilerplate window, got {retry_windows}:\n{long_preview}"
        );
    }

    #[test]
    fn exec_run_end_to_end_shapes_json_logs_and_stacktraces_with_exact_archive() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join(".justless.toml"), "").unwrap();
        let ws = Workspace::discover(Some(dir.path())).unwrap();

        let run_case = |args: &[&str]| -> (i32, serde_json::Value, PathBuf) {
            let before: HashSet<String> = fs::read_dir(ws.state.join("runs"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            let argv: Vec<&str> = std::iter::once("exec")
                .chain(args.iter().copied())
                .collect();
            let matches = command().try_get_matches_from(argv).unwrap();
            let status = run(&ws, &matches).unwrap();
            let new_dir = fs::read_dir(ws.state.join("runs"))
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .find(|p| {
                    p.join("record.json").is_file()
                        && !before.contains(&p.file_name().unwrap().to_string_lossy().into_owned())
                })
                .expect("new run directory");
            let record: serde_json::Value =
                serde_json::from_slice(&fs::read(new_dir.join("record.json")).unwrap()).unwrap();
            (status, record, new_dir)
        };

        // 1. JSON-emitting script: shrinks dramatically and archives exact raw JSON.
        let json_script = r#"
printf '[\n'
i=1
while [ $i -le 120 ]; do
  sep=","
  if [ $i -eq 120 ]; then sep=""; fi
  printf '  {"id": %d, "service": "orders", "user": {"name": "user%d", "tier": "pro"}, "ok": true}%s\n' "$i" "$i" "$sep"
  i=$((i + 1))
done
printf ']\n'
"#;
        let (status, rec, run_dir) = run_case(&["--lang", "sh", json_script]);
        assert_eq!(status, 0);
        let raw_json = rec["raw_bytes"].as_u64().unwrap();
        let preview_json = rec["preview_bytes"].as_u64().unwrap();
        assert!(raw_json > 10_000, "raw_json={raw_json}");
        assert!(
            preview_json <= 4096 && preview_json * 3 < raw_json,
            "expected deep-schema jsonview shrinkage: raw={raw_json}, preview={preview_json}"
        );
        let archived_json = fs::read_to_string(run_dir.join("stdout")).unwrap();
        assert_eq!(archived_json.len() as u64, raw_json);
        let parsed: serde_json::Value = serde_json::from_str(&archived_json).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 120);

        // 2. Failing script with deep Python traceback on stdout (exit 7 preserved + archived exact).
        let trace_script = r#"
echo "Traceback (most recent call last):"
echo '  File ".justless/runs/run-x/script.py", line 9, in main'
echo "    sync()"
i=1
while [ $i -le 45 ]; do
  printf '  File "/usr/lib/python3.12/site-packages/urllib3/connectionpool.py", line %d, in urlopen\n' "$((100 + i))"
  echo "    response = self._make_request(conn, method, url, timeout=timeout, chunked=chunked)"
  i=$((i + 1))
done
echo '  File "src/sync.py", line 31, in sync'
echo "    raise RuntimeError('upstream 503')"
echo "RuntimeError: upstream 503"
exit 7
"#;
        let (status, rec, run_dir) = run_case(&["--lang", "sh", trace_script]);
        assert_eq!(status, 7);
        let raw_trace = rec["raw_bytes"].as_u64().unwrap();
        let preview_trace = rec["preview_bytes"].as_u64().unwrap();
        assert!(raw_trace > 4096, "raw_trace={raw_trace}");
        assert!(
            preview_trace * 4 < raw_trace,
            "expected >75% trace reduction: raw={raw_trace}, preview={preview_trace}"
        );
        assert_eq!(
            fs::metadata(run_dir.join("stdout")).unwrap().len(),
            raw_trace
        );

        // 3. Repetitive structured-log script (> 15 KB -> folded under 1 KB).
        let log_script = r#"
i=1
while [ $i -le 260 ]; do
  printf '2026-04-01T12:00:%02d.%03dZ INFO worker polling batch=%d latency=4ms\n' "$((i % 60))" "$((i % 1000))" "$i"
  if [ $i -eq 130 ]; then
    echo "2026-04-01T12:02:10.000Z ERROR worker commit failed: deadlock on orders"
  fi
  i=$((i + 1))
done
"#;
        let (status, rec, run_dir) = run_case(&["--lang", "sh", log_script]);
        assert_eq!(status, 0);
        let raw_logs = rec["raw_bytes"].as_u64().unwrap();
        let preview_logs = rec["preview_bytes"].as_u64().unwrap();
        assert!(raw_logs > 15_000, "raw_logs={raw_logs}");
        assert!(
            preview_logs * 10 < raw_logs,
            "expected >90% logview reduction: raw={raw_logs}, preview={preview_logs}"
        );
        assert_eq!(
            fs::metadata(run_dir.join("stdout")).unwrap().len(),
            raw_logs
        );
    }

    #[test]
    fn csvview_exec_redacts_before_formatting_and_replays_exact_original_csv() {
        let root = Path::new("/workspace");
        let redactor = test_redactor();
        let secret = format!("ghp_{}", "qR7sT8uV9wX0yZ1aB2cD3eF4gH5iJ6kL7mN8");

        // 1. Redaction MUST run before csvview formatting (Tier 1 & Tier 2),
        // and stderr must never route through csvview.
        let mut tier1_csv =
            String::from("\"id\",\"service\",\"env\",\"token\",\"status\",\"amount\"\n");
        for i in 1..=50 {
            let status = if i % 20 == 0 { "err" } else { "ok" };
            tier1_csv.push_str(&format!(
                "\"{i}\",\"billing\",\"production\",\"{secret}\",\"{status}\",\"{:.2}\"\n",
                10.0 + (i as f64) * 1.25
            ));
        }
        assert!(matches!(
            shape_text_preview(&tier1_csv, 4096, root, "stderr", false, Some(redactor)),
            ShapedText::Fallback
        ));
        let shaped_t1 =
            match shape_text_preview(&tier1_csv, 4096, root, "stdout", false, Some(redactor)) {
                ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
                _ => panic!("expected redacted Tier 1 csvview"),
            };
        assert!(
            shaped_t1.starts_with("[CSV 50 rows × 6 cols"),
            "{shaped_t1}"
        );
        assert!(shaped_t1.len() <= 4096);
        assert!(shaped_t1.len() * 2 < tier1_csv.len());
        assert!(
            !shaped_t1.contains(&secret),
            "secret leaked into Tier 1 csvview:\n{shaped_t1}"
        );
        assert!(shaped_t1.contains("[redacted:"), "{shaped_t1}");

        let mut tier2_csv = String::from(
            "id,timestamp,service,env,region,status,api_token,latency_ms,amount_usd,note\n",
        );
        let regions = ["us-east-1", "eu-west-1", "us-west-2", "ap-south-1"];
        for i in 1..=1000 {
            let status = if i % 25 == 0 { "timeout" } else { "ok" };
            let tok = if i == 1 || i == 500 || i == 1000 {
                secret.as_str()
            } else {
                "public_key"
            };
            tier2_csv.push_str(&format!(
                "{i},2026-04-01T{:02}:{:02}:00Z,checkout,prod,{},{status},{tok},{},{:.2},trace_span_{i:06}\n",
                (i / 60) % 24,
                i % 60,
                regions[i % 4],
                12 + (i * 13) % 900,
                1.50 + (i as f64) * 0.75
            ));
        }
        assert!(tier2_csv.len() > 80_000, "raw={}", tier2_csv.len());
        let shaped_t2 =
            match shape_text_preview(&tier2_csv, 4096, root, "stdout", false, Some(redactor)) {
                ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
                _ => panic!("expected redacted Tier 2 csvview"),
            };
        assert!(
            shaped_t2.starts_with("[CSV 1000 rows × 10 cols · exact data in full log]\n"),
            "{shaped_t2}"
        );
        assert!(shaped_t2.len() <= 4096);
        assert!(
            shaped_t2.len() * 20 < tier2_csv.len(),
            "expected >95% reduction: raw={}, shaped={}",
            tier2_csv.len(),
            shaped_t2.len()
        );
        assert!(
            !shaped_t2.contains(&secret),
            "secret leaked into Tier 2 csvview:\n{shaped_t2}"
        );
        assert!(shaped_t2.contains("[redacted:"), "{shaped_t2}");
        assert!(shaped_t2.contains("[stdout:10-997]"), "{shaped_t2}");

        // 2. End-to-end `exec::run` emitting a 1,000-row ~100 KB CSV: preview shrinks >95%
        // via csvview while the archive and `capture::log` replay the exact original CSV.
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join(".justless.toml"), "").unwrap();
        let ws = Workspace::discover(Some(dir.path())).unwrap();

        let csv_script = r#"
printf 'id,timestamp,service,env,region,status,active,latency_ms,amount_usd,request_note\n'
i=1
while [ $i -le 1000 ]; do
  case $((i % 4)) in
    0) region=us-east-1 ;;
    1) region=eu-west-1 ;;
    2) region=us-west-2 ;;
    *) region=ap-south-1 ;;
  esac
  if [ $((i % 25)) -eq 0 ]; then
    status=timeout
  elif [ $((i % 50)) -eq 1 ]; then
    status=err
  else
    status=ok
  fi
  if [ $((i % 3)) -eq 0 ]; then active=false; else active=true; fi
  if [ $((i % 100)) -eq 0 ]; then latency=""; else latency=$((4 + (i * 19) % 1847)); fi
  printf '%d,2026-01-01T%02d:%02d:00Z,checkout,prod,%s,%s,%s,%s,%d.50,checkout_trace_span_%06d_shard_%d\n' \
    "$i" "$(((i / 60) % 24))" "$((i % 60))" "$region" "$status" "$active" "$latency" "$((1 + (i % 400)))" "$i" "$((i % 16))"
  i=$((i + 1))
done
"#;
        let matches = command()
            .try_get_matches_from(["exec", "--lang", "sh", csv_script])
            .unwrap();
        let status = run(&ws, &matches).unwrap();
        assert_eq!(status, 0);

        let run_dir = fs::read_dir(ws.state.join("runs"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.join("record.json").is_file())
            .expect("recorded CSV run directory");
        let rec: serde_json::Value =
            serde_json::from_slice(&fs::read(run_dir.join("record.json")).unwrap()).unwrap();
        let raw_bytes = rec["raw_bytes"].as_u64().unwrap();
        let preview_bytes = rec["preview_bytes"].as_u64().unwrap();
        assert!(raw_bytes >= 95_000, "raw_bytes={raw_bytes}");
        assert!(
            preview_bytes <= 4096 && preview_bytes * 20 < raw_bytes,
            "expected >95% csvview reduction: raw={raw_bytes}, preview={preview_bytes}"
        );

        let stdout_path = run_dir.join("stdout");
        let archived = fs::read_to_string(&stdout_path).unwrap();
        assert_eq!(archived.len() as u64, raw_bytes);
        let archived_lines: Vec<&str> = archived.lines().collect();
        assert_eq!(archived_lines.len(), 1001);
        assert_eq!(
            archived_lines[0],
            "id,timestamp,service,env,region,status,active,latency_ms,amount_usd,request_note"
        );
        assert!(
            archived_lines[1].starts_with("1,2026-01-01T00:01:00Z,checkout,prod,eu-west-1,err,")
        );
        assert!(
            archived_lines[1000]
                .starts_with("1000,2026-01-01T16:40:00Z,checkout,prod,us-east-1,timeout,")
        );

        // Verify `shape_stdout` produces the Tier 2 CSV schema preview pointing at exact lines.
        let run_id = run_dir.file_name().unwrap().to_string_lossy().into_owned();
        let preview = String::from_utf8(
            shape_stdout(
                &stdout_path,
                &run_id,
                4096,
                None,
                &ws.root,
                "justless",
                false,
            )
            .unwrap()
            .expect("csvview preview"),
        )
        .unwrap();
        assert!(
            preview.starts_with("[CSV 1000 rows × 10 cols · exact data in full log]\n"),
            "{preview}"
        );
        assert!(
            preview.contains("... 988 rows omitted [stdout:10-997] ..."),
            "{preview}"
        );
        assert!(archived_lines[9].starts_with("9,"));
        assert!(archived_lines[996].starts_with("996,"));

        // Verify `justless log` (`capture::log`) replays exact original bytes and exact omitted range.
        let replayed_all =
            crate::capture::log(&run_dir, &[crate::capture::Stream::Stdout], 1, usize::MAX)
                .unwrap();
        assert_eq!(replayed_all, raw_bytes);
        let expected_omitted_bytes: usize =
            archived_lines[9..=996].iter().map(|l| l.len() + 1).sum();
        let replayed_slice =
            crate::capture::log(&run_dir, &[crate::capture::Stream::Stdout], 10, 997).unwrap();
        assert_eq!(replayed_slice as usize, expected_omitted_bytes);
    }

    #[test]
    fn lossless_only_mode_skips_lossy_logs_and_outline_while_keeping_bijective_views() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // 1. Structured logs are lossy -> Fallback in lossless_only mode, and shape_stdout returns None.
        let mut logs = String::new();
        for i in 0..80 {
            logs.push_str(&format!(
                "2026-11-26T10:00:{:02}Z INFO service=api req={i} ok\n",
                i % 60
            ));
        }
        assert!(matches!(
            shape_text_preview_with_mode(&logs, 1024, root, "stdout", false, None, true),
            ShapedText::Fallback
        ));
        let log_file = root.join("logs.out");
        fs::write(&log_file, &logs).unwrap();
        assert!(
            shape_stdout_with_mode(
                &log_file, "run-1", 1024, None, root, "justless", false, true
            )
            .unwrap()
            .is_none()
        );

        // 2. Bijective JSON compacts without clipping even when budget is tiny.
        let mut rows = String::from("[");
        for i in 0..60 {
            if i > 0 {
                rows.push(',');
            }
            rows.push_str(&format!(
                "{{\"id\":{i},\"service\":\"svc-{i}\",\"env\":\"prod\",\"region\":\"us-east-1\"}}"
            ));
        }
        rows.push(']');
        let json_shaped =
            match shape_text_preview_with_mode(&rows, 512, root, "stdout", false, None, true) {
                ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
                _ => panic!("expected bijective jsonview in lossless_only mode"),
            };
        assert!(json_shaped.len() < rows.len());
        assert!(!json_shaped.contains("more lines of this view"));
        assert!(json_shaped.contains("svc-0") && json_shaped.contains("svc-59"));
    }

    #[test]
    fn ansi_fast_path_and_wave8_to_11_compatibility_in_exec_shaper() {
        let root = Path::new("/workspace");

        // 1. Zero-alloc ANSI fast path borrows plain strings and strips colored ones.
        assert!(matches!(strip_ansi("plain log line"), Cow::Borrowed(_)));
        assert_eq!(strip_ansi("\x1b[31merror\x1b[0m"), "error");

        // 2. UTF-8 BOM RFC 4180 CSV compacts in both default and lossless_only modes.
        let mut bom_csv =
            String::from("\u{feff}\"id\",\"service\",\"env\",\"status\",\"amount\"\n");
        for i in 1..=60 {
            let status = if i % 20 == 0 { "err" } else { "ok" };
            bom_csv.push_str(&format!(
                "\"{i}\",\"checkout\",\"prod\",\"{status}\",\"{:.2}\"\n",
                10.0 + (i as f64) * 0.5
            ));
        }
        let csv_lossless =
            match shape_text_preview_with_mode(&bom_csv, 512, root, "stdout", false, None, true) {
                ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
                _ => panic!("expected bijective csvview for UTF-8 BOM CSV in lossless_only mode"),
            };
        assert!(
            csv_lossless.starts_with("[CSV 60 rows × 5 cols"),
            "{csv_lossless}"
        );
        assert!(!csv_lossless.contains("rows omitted"), "{csv_lossless}");
        assert!(csv_lossless.contains("service=checkout"), "{csv_lossless}");
        assert!(csv_lossless.len() < bom_csv.len());

        // 3. Escaped-pipe Markdown table compacts cleanly in lossless_only and bounded modes.
        let mut md_table =
            String::from("| id | env | expr | status |\n| --- | --- | --- | --- |\n");
        for i in 1..=40 {
            let status = if i == 17 { "fail" } else { "pass" };
            md_table.push_str(&format!("| {i:03} | prod | a \\| b_{i:02} | {status} |\n"));
        }
        let table_lossless =
            match shape_text_preview_with_mode(&md_table, 512, root, "stdout", false, None, true) {
                ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
                _ => panic!("expected bijective tableview for escaped-pipe table"),
            };
        assert!(
            table_lossless.starts_with("[table: 40 rows, TSV;"),
            "{table_lossless}"
        );
        assert!(
            table_lossless.contains(r"a \\| b_17") || table_lossless.contains("a | b_17"),
            "{table_lossless}"
        );
        assert!(table_lossless.len() < md_table.len());

        // 4. Zap/Pino/K8s JSON & plain klog bursts route to logview, and respect lossless_only.
        let mut json_burst = String::new();
        for i in 1..=40 {
            json_burst.push_str(&format!(
                r#"{{"time":{},"level":30,"service":"gateway","msg":"ok /orders/{i} 200 {}ms"}}"#,
                1_712_001_700_000u64 + (i as u64) * 1000,
                i + 2
            ));
            json_burst.push('\n');
        }
        json_burst.push_str(
            r#"{"time":1712001750000,"level":50,"service":"gateway","msg":"upstream timeout","route":"/checkout","req_id":"req-42"}"#,
        );
        json_burst.push('\n');
        let folded_json = match shape_text_preview(&json_burst, 2048, root, "stdout", false, None) {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected logview folding for Pino JSON burst"),
        };
        assert!(folded_json.contains("×40"), "{folded_json}");
        assert!(
            folded_json.contains("upstream timeout") && folded_json.contains("req_id=req-42"),
            "{folded_json}"
        );

        let mut klog_burst = String::new();
        for i in 1..=40 {
            klog_burst.push_str(&format!(
                "I1125 10:00:{:02}.123456       1 sync.go:88] Synced pod default/web-{i:04}\n",
                i % 60
            ));
        }
        klog_burst.push_str(
            "E1125 10:01:00.654321       1 sync.go:104] Failed to bind volume pvc-orders\n",
        );
        let folded_klog = match shape_text_preview(&klog_burst, 2048, root, "stdout", false, None) {
            ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
            _ => panic!("expected logview folding for klog burst"),
        };
        assert!(folded_klog.contains("×40"), "{folded_klog}");
        assert!(
            folded_klog.contains("Failed to bind volume pvc-orders"),
            "{folded_klog}"
        );
        assert!(matches!(
            shape_text_preview_with_mode(&klog_burst, 2048, root, "stdout", false, None, true),
            ShapedText::Fallback
        ));

        // 5. Rust and Dart/Flutter stacktraces collapse external frames while lossless_only skips them.
        let mut rust_trace = String::from(
            "thread 'main' panicked at src/billing.rs:42:5:\noverflow\nstack backtrace:\n",
        );
        rust_trace.push_str("   0: billing::charge\n             at ./src/billing.rs:42:5\n");
        for i in 1..=12 {
            rust_trace.push_str(&format!(
                "   {i}: std::rt::lang_start_internal\n             at /rustc/abcdef/library/std/src/rt.rs:{}\n",
                100 + i
            ));
        }
        let rust_collapsed =
            match shape_text_preview(&rust_trace, 4096, root, "stderr", false, None) {
                ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
                _ => panic!("expected Rust stacktrace collapse"),
            };
        assert!(
            rust_collapsed.contains("src/billing.rs:42:5"),
            "{rust_collapsed}"
        );
        assert!(
            rust_collapsed.contains("runtime/dependency frames"),
            "{rust_collapsed}"
        );
        assert!(matches!(
            shape_text_preview_with_mode(&rust_trace, 4096, root, "stderr", false, None, true),
            ShapedText::Fallback
        ));

        let mut dart_trace = String::from("Unhandled exception:\nStateError: bad state\n");
        dart_trace.push_str("#0      CheckoutBloc.submit (package:app/checkout.dart:88:7)\n");
        for i in 1..=12 {
            dart_trace.push_str(&format!(
                "#{i:<6} _rootRunUnary (dart:async/zone.dart:{}:47)\n",
                1400 + i
            ));
        }
        let dart_collapsed =
            match shape_text_preview(&dart_trace, 4096, root, "stderr", false, None) {
                ShapedText::Ready(bytes) => String::from_utf8(bytes).unwrap(),
                _ => panic!("expected Dart stacktrace collapse"),
            };
        assert!(
            dart_collapsed.contains("package:app/checkout.dart:88:7"),
            "{dart_collapsed}"
        );
        assert!(
            dart_collapsed.contains("runtime/dependency frames"),
            "{dart_collapsed}"
        );
        assert!(matches!(
            shape_text_preview_with_mode(&dart_trace, 4096, root, "stderr", false, None, true),
            ShapedText::Fallback
        ));
    }
}
