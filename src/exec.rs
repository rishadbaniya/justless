//! `justless exec`: run a short script over data too large to read, so that only what
//! the script prints reaches the agent. The streams go through the same capture as
//! `run`, which archives them under a run ID and bounds each preview.
//!
//! This isolates output, not code: scripts run with the caller's permissions.

use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Arg, ArgMatches};

use crate::capture::{self, Shape};
use crate::workspace::Workspace;

/// Largest script read from stdin. Data belongs in `--file`.
const STDIN_LIMIT: u64 = 1024 * 1024;
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
struct Shebang {
    program: String,
    options: Vec<String>,
    env: Vec<(String, String)>,
}

impl Shebang {
    /// Reads `#!/bin/sh -e`, `#!/usr/bin/env -S bash -eu` or
    /// `#!/usr/bin/env PYTHONUTF8=1 python3`. Options are split at whitespace, as
    /// macOS does; Linux would pass them as one argument.
    fn parse(code: &[u8]) -> Option<Self> {
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
            hook: true,
            family: Some(format!("exec {}", lang.label())),
            // Script output is mostly data, so a long stdout keeps whole lines and names
            // the ones left out, or is ranked against the intent.
            stdout_preview: Some(Box::new(move |stdout: &Path, id: &str| match intent {
                Some(intent) => crate::focus::preview(stdout, intent, max_bytes, id)
                    .context("could not rank stdout for --intent"),
                None => {
                    crate::focus::outline(stdout, max_bytes, id).context("could not outline stdout")
                }
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
        })
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
        self.send(signal)
    }

    fn occupied(&mut self) -> bool {
        // The answer is immediate. A watchdog that cannot answer cannot stop anything
        // either, so there is nothing to wait for.
        self.send("0")
            && self
                .answers
                .recv_timeout(Duration::from_secs(1))
                .unwrap_or(false)
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
    watchdog.send(&child.id().to_string());
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
                left_running = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn shebang(code: &str) -> Option<String> {
        Shebang::parse(code.as_bytes()).map(|line| line.program)
    }

    #[test]
    fn shebang_names_the_interpreter() {
        assert_eq!(shebang("#!/bin/sh\necho ok").as_deref(), Some("sh"));
        assert_eq!(
            shebang("#!/usr/bin/env python3\n").as_deref(),
            Some("python3")
        );
        assert_eq!(
            shebang("#!/usr/bin/env -S python3 -u\n").as_deref(),
            Some("python3")
        );
        assert_eq!(
            shebang("#!/usr/bin/env PYTHONUTF8=1 python3\n").as_deref(),
            Some("python3")
        );
        assert_eq!(
            shebang("#!/usr/bin/env -u PYTHONPATH python3\n").as_deref(),
            Some("python3")
        );
        assert_eq!(
            shebang("#! /usr/local/bin/python3.12\r\nprint(1)").as_deref(),
            Some("python3.12")
        );
        assert_eq!(shebang("#!/usr/bin/env node\r\n").as_deref(), Some("node"));
        assert_eq!(
            shebang("#!/usr/bin/env Rscript").as_deref(),
            Some("Rscript")
        );
        assert_eq!(shebang("#!/usr/bin/env\n"), None);
        assert_eq!(shebang("#!/usr/bin/env -S\n"), None);
        assert_eq!(shebang("#!\n"), None);
        assert_eq!(shebang("print(1)\n#!/bin/sh"), None);
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
        let words = |list: &[&str]| list.iter().map(|word| word.to_string()).collect::<Vec<_>>();
        assert_eq!(
            Lang::Deno.prefix(&[]),
            words(&["run", "--allow-all", "--quiet"])
        );
        assert_eq!(
            Lang::Deno.prefix(&words(&["run", "--allow-read=."])),
            words(&["run", "--allow-read=."])
        );
        assert_eq!(
            Lang::Deno.prefix(&words(&["--allow-net"])),
            words(&["run", "--allow-net"])
        );
        assert_eq!(Lang::Bun.prefix(&[]), words(&["run"]));
        assert_eq!(
            Lang::Bun.prefix(&words(&["--smol"])),
            words(&["run", "--smol"])
        );
        assert_eq!(Lang::Sh.prefix(&words(&["-e"])), words(&["-e"]));
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
}
