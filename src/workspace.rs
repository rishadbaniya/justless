use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Location {
    pub file: String,
    pub start: usize,
    pub end: usize,
}

impl std::fmt::Display for Location {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let file = display_path(&self.file);
        if self.start == self.end {
            write!(formatter, "{file}:{}", self.start)
        } else {
            write!(formatter, "{file}:{}-{}", self.start, self.end)
        }
    }
}

/// Quotes a name that contains control characters, such as a newline, so one
/// result stays on one line. Other names are returned unchanged.
pub fn display_path(name: &str) -> std::borrow::Cow<'_, str> {
    if name.chars().any(char::is_control) {
        format!("{name:?}").into()
    } else {
        name.into()
    }
}

#[derive(Clone)]
pub struct Workspace {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub state: PathBuf,
    /// The directory the command started in, which can lie outside `root`. A
    /// command printed for the agent to run next is run from here. Empty when
    /// that directory is unknown, as for the MCP server's commands.
    pub origin: PathBuf,
}

/// Set for a command whose output reaches an agent whose shell may work anywhere,
/// such as the MCP server's child: every command it prints names `--root`.
pub const ORIGIN_UNKNOWN: &str = "JUSTLESS_ORIGIN_UNKNOWN";

/// How a `justless` command printed for the agent to run next starts, so that
/// run from the directory the printing command started in it reaches the same
/// workspace. Each is `justless`, or `justless --root ROOT` with the absolute root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    /// For commands that name runs, recall IDs, symbols or workspace-relative
    /// paths: `--root` when a plain `justless` started there finds another root.
    pub command: String,
    /// For `read`, `expand` and `edit`, whose bare paths resolve from the current
    /// directory first unless `--root` is given: `--root` also whenever that
    /// directory is not the root.
    pub files: String,
}

#[cfg(test)]
impl Invocation {
    pub fn plain() -> Self {
        Self {
            command: "justless".into(),
            files: "justless".into(),
        }
    }
}

/// A path quoted for a POSIX shell when it needs quoting.
pub fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    shlex::try_quote(&text).map_or_else(|_| text.to_string(), |quoted| quoted.into_owned())
}

/// A relative file path as a printed command's argument: quoted for a POSIX
/// shell, and starting with `./` when it would otherwise start with `-`, so the
/// command reads it as a path rather than an option (`-release.md`).
pub fn shell_path(file: &str) -> String {
    if file.starts_with('-') {
        shell_quote(Path::new(&format!("./{file}")))
    } else {
        shell_quote(Path::new(file))
    }
}

/// Text as a printed command's last positional argument: quoted, and after
/// `--` when it starts with `-`, so the command reads it as text, not an option.
pub fn shell_text(text: &str) -> String {
    let quoted = shell_quote(Path::new(text));
    if text.starts_with('-') {
        format!("-- {quoted}")
    } else {
        quoted
    }
}

pub(crate) const PROJECT_MARKERS: &[&str] = &[
    ".justless.toml",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "pom.xml",
    "composer.json",
    "Gemfile",
    "CMakeLists.txt",
];

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub filters: Vec<crate::filters::Rule>,
    pub context_budget: usize,
    pub max_output_bytes: usize,
    pub max_file_bytes: u64,
    /// The most captured runs kept under `.justless/runs/`, newest first.
    pub max_runs: usize,
    /// The most bytes those runs may hold. The newest run is always kept.
    pub max_archive_bytes: u64,
    /// The most bytes one run archives from each stream. Output past it is read
    /// and counted but not stored, so one command can never fill the disk.
    pub max_run_bytes: u64,
    /// Replace secrets in captured command output with markers.
    pub redact: bool,
    /// Regular expressions for detected values to leave visible.
    pub redact_allow: Vec<String>,
    /// `[test]`: which changes run the full suite or have no test impact.
    pub test: crate::selection::TestSettings,
    /// Adapt each command family's preview budget to how often agents reopened
    /// its clipped logs in full. See `adaptive.rs`.
    pub adaptive_previews: bool,
    /// The highest adapted budget. Defaults to four times `max_output_bytes`.
    pub adaptive_cap_bytes: Option<usize>,
    /// The lowest adapted budget. Defaults to half of `max_output_bytes`, at least 512.
    pub adaptive_floor_bytes: Option<usize>,
    /// Show JSON tables with consecutive rows that share a field's value under
    /// one `@ VALUE` line. Off by default: plain rows are easier to count and quote.
    pub json_grouping: bool,
    /// Commands every hook leaves alone: the first words of a command, or a
    /// regular expression when the entry starts with `^`. See `crate::rewrite`.
    pub exclude_commands: Vec<String>,
    /// Whether the user-level hooks `init --global` installs work in this project.
    pub global_hooks: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            filters: Vec::new(),
            context_budget: 2400,
            max_output_bytes: 4096,
            max_file_bytes: 2 * 1024 * 1024,
            max_runs: 200,
            max_archive_bytes: 256 * 1024 * 1024,
            max_run_bytes: 256 * 1024 * 1024,
            redact: true,
            redact_allow: Vec::new(),
            test: crate::selection::TestSettings::default(),
            adaptive_previews: true,
            adaptive_cap_bytes: None,
            adaptive_floor_bytes: None,
            json_grouping: false,
            exclude_commands: Vec::new(),
            global_hooks: true,
        }
    }
}

/// The marked root of a workspace entered at `cwd` when no `--root` is given: the
/// nearest Git checkout, then the nearest Justless state or settings, then the
/// nearest project manifest. Without one, `cwd` itself is the root.
pub fn marked_root(cwd: &Path) -> Option<&Path> {
    cwd.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .or_else(|| {
            cwd.ancestors().find(|ancestor| {
                ancestor.join(".justless").is_dir() || ancestor.join(".justless.toml").is_file()
            })
        })
        .or_else(|| {
            cwd.ancestors().find(|ancestor| {
                PROJECT_MARKERS
                    .iter()
                    .any(|name| ancestor.join(name).is_file())
            })
        })
}

/// The root a plain `justless` started in `start` takes: its marked root, or
/// `start` itself.
pub fn found_root(start: &Path) -> &Path {
    marked_root(start).unwrap_or(start)
}

impl Workspace {
    pub fn discover(root: Option<&Path>) -> Result<Self> {
        Self::locate(root, None)
    }

    /// Searches upward from `start` instead of the working directory, the way
    /// a command run from `start` would find its project.
    pub fn discover_from(start: &Path) -> Result<Self> {
        Self::locate(None, Some(start))
    }

    fn locate(root: Option<&Path>, start: Option<&Path>) -> Result<Self> {
        let cwd = std::env::current_dir()?.canonicalize()?;
        let start = match start {
            Some(start) => start
                .canonicalize()
                .with_context(|| format!("open {}", start.display()))?,
            None => cwd.clone(),
        };
        let root = match root {
            Some(root) => root
                .canonicalize()
                .with_context(|| format!("open {}", root.display()))?,
            None => found_root(&start).to_path_buf(),
        };
        ensure!(root.is_dir(), "{} is not a directory", root.display());
        let origin = if std::env::var_os(ORIGIN_UNKNOWN).is_some() {
            PathBuf::new()
        } else {
            cwd.clone()
        };
        let cwd = if cwd.starts_with(&root) {
            cwd
        } else {
            root.clone()
        };
        let state = root.join(".justless");
        Ok(Self {
            root,
            cwd,
            state,
            origin,
        })
    }

    /// The same workspace for a command printed for an agent working in `dir`,
    /// such as the directory a hook reports. A relative `dir` starts at `origin`.
    pub fn working_in(&self, dir: &Path) -> Self {
        Self {
            root: self.root.clone(),
            cwd: self.cwd.clone(),
            state: self.state.clone(),
            origin: self.origin.join(dir),
        }
    }

    /// How a printed command starts, for an agent working where this one started.
    pub fn invocation(&self) -> Invocation {
        // A directory that cannot be resolved gets `--root`, which works from anywhere.
        let origin = self.origin.canonicalize().ok();
        let found = origin
            .as_deref()
            .is_some_and(|origin| found_root(origin) == self.root);
        let at_root = origin.as_deref() == Some(self.root.as_path());
        let rooted = || format!("justless --root {}", shell_quote(&self.root));
        let plain = || "justless".to_owned();
        Invocation {
            command: if found { plain() } else { rooted() },
            files: if found && at_root { plain() } else { rooted() },
        }
    }

    /// The start of a printed command that names runs, recall IDs, symbols or
    /// workspace-relative paths. See [`Invocation::command`].
    pub fn justless(&self) -> String {
        self.invocation().command
    }

    pub fn git(&self, args: &[&str]) -> Result<Output> {
        Command::new("git")
            .arg("--literal-pathspecs")
            .args(args)
            .current_dir(&self.root)
            .output()
            .context("run git")
    }

    pub fn source_path(&self, file: &str) -> Result<PathBuf> {
        let relative = Path::new(file);
        ensure!(
            relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
            "expected a workspace-relative file path"
        );
        let path = self.root.join(relative);
        let canonical = path
            .canonicalize()
            .with_context(|| format!("open {file}"))?;
        ensure!(
            (canonical == path || self.respelled(&path, &canonical)) && canonical.is_file(),
            "expected a workspace file: {file}"
        );
        Ok(canonical)
    }

    /// macOS file systems return the stored Unicode normalization of a name, so
    /// the precomposed name Git prints can canonicalize to decomposed bytes.
    /// Accept that difference when no component is a symlink and only accented
    /// or other non-ASCII characters differ, so ASCII case still has to match.
    fn respelled(&self, path: &Path, canonical: &Path) -> bool {
        // Drop non-ASCII characters and any base letter a combining mark follows.
        let ascii = |path: &Path| {
            let text: Vec<char> = path.to_str()?.chars().collect();
            Some(
                text.iter()
                    .enumerate()
                    .filter(|&(at, c)| {
                        c.is_ascii() && !text.get(at + 1).is_some_and(|&next| combining(next))
                    })
                    .map(|(_, c)| *c)
                    .collect::<String>(),
            )
        };
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        let mut current = self.root.clone();
        canonical.starts_with(&self.root)
            && ascii(path).is_some()
            && ascii(path) == ascii(canonical)
            && relative.components().all(|part| {
                current.push(part);
                fs::symlink_metadata(&current).is_ok_and(|meta| !meta.file_type().is_symlink())
            })
    }

    pub fn relative_file(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(&self.root).ok()?;
        let name = relative.to_str()?;
        self.source_path(name).ok()?;
        Some(name.to_owned())
    }

    pub fn prepare_state(&self) -> Result<()> {
        private_dir(&self.state)?;
        // A directory whose own .gitignore says `*` is invisible to Git, so archived
        // output never shows up in `git status`, diffs or `git add -A`, including in
        // nested repositories and checkouts that do not ignore `.justless/`.
        let ignore = self.state.join(".gitignore");
        if fs::symlink_metadata(&ignore).is_err() {
            let _ = File::options()
                .write(true)
                .create_new(true)
                .open(&ignore)
                .and_then(|mut file| file.write_all(b"*\n"));
        }
        crate::registry::touch(self);
        Ok(())
    }

    pub fn settings(&self) -> Result<Settings> {
        let path = self.root.join(".justless.toml");
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(error) => return Err(error).context("read .justless.toml metadata"),
        };
        ensure!(metadata.is_file(), ".justless.toml must be a regular file");
        let text = fs::read_to_string(&path)?;
        let settings = toml_edit::de::from_str::<Settings>(&text).context("read .justless.toml")?;
        ensure!(
            settings.context_budget >= 128,
            "context_budget must be at least 128"
        );
        ensure!(
            settings.max_output_bytes >= 512,
            "max_output_bytes must be at least 512"
        );
        ensure!(
            settings.max_file_bytes > 0,
            "max_file_bytes must be positive"
        );
        ensure!(settings.max_runs > 0, "max_runs must be positive");
        ensure!(settings.max_run_bytes > 0, "max_run_bytes must be positive");
        ensure!(
            settings
                .adaptive_cap_bytes
                .is_none_or(|cap| cap >= settings.max_output_bytes),
            "adaptive_cap_bytes must be at least max_output_bytes"
        );
        ensure!(
            settings
                .adaptive_floor_bytes
                .is_none_or(|floor| (512..=settings.max_output_bytes).contains(&floor)),
            "adaptive_floor_bytes must be at least 512 and at most max_output_bytes"
        );
        for entry in &settings.exclude_commands {
            crate::rewrite::check_exclusion(entry)?;
        }
        Ok(settings)
    }
}

fn combining(c: char) -> bool {
    matches!(
        u32::from(c),
        0x300..=0x36F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
    )
}

pub fn private_dir(path: &Path) -> Result<()> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path)?;
            ensure!(metadata.is_dir(), "{} must be a directory", path.display());
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("create {}", path.display())),
    }
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut temporary = NamedTempFile::new_in(path.parent().context("output directory")?)?;
    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        serde_json::to_writer(&mut writer, value)?;
        writer.flush()?;
    }
    temporary.persist(path)?;
    Ok(())
}

pub fn write_text(path: &Path, value: &str) -> Result<()> {
    let mut temporary = NamedTempFile::new_in(path.parent().context("output directory")?)?;
    if let Ok(metadata) = fs::metadata(path) {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())?;
    }
    temporary.write_all(value.as_bytes())?;
    temporary.persist(path)?;
    Ok(())
}

pub fn lock_state(path: &Path) -> Result<File> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let started = std::time::Instant::now();
    let timeout = std::env::var("JUSTLESS_LOCK_TIMEOUT")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map_or(std::time::Duration::from_secs(30), |seconds| {
            std::time::Duration::from_secs_f64(seconds)
        });
    let mut announced = false;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("lock {}", path.display()));
            }
            Err(fs::TryLockError::WouldBlock) => {}
        }
        let waited = started.elapsed();
        ensure!(
            waited < timeout,
            "timed out after {:.0?} waiting for {} (held by another justless process)",
            waited,
            path.display()
        );
        if !announced && waited >= std::time::Duration::from_secs(1) {
            eprintln!(
                "justless: waiting for {} (held by another justless process)",
                path.display()
            );
            announced = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printed_arguments_never_read_as_options() {
        assert_eq!(shell_path("-release.md"), "./-release.md");
        assert_eq!(shell_path("docs/-v.md"), "docs/-v.md");
        assert_eq!(shell_path("my notes.md"), "'my notes.md'");
        assert_eq!(shell_path("-a b.md"), "'./-a b.md'");
        assert_eq!(shell_text("-release.md"), "-- -release.md");
        assert_eq!(shell_text("src/a.rs"), "src/a.rs");
    }

    fn workspace(root: &Path, origin: &Path) -> Workspace {
        Workspace {
            root: root.to_path_buf(),
            cwd: root.to_path_buf(),
            state: root.join(".justless"),
            origin: origin.to_path_buf(),
        }
    }

    fn rooted(root: &Path) -> String {
        format!("justless --root {}", shell_quote(root))
    }

    #[test]
    fn printed_commands_name_the_root_only_where_a_plain_one_would_miss_it() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().canonicalize().unwrap();
        for sub in [
            ".git",
            "billing",
            "docs",
            "vendor/lib/src",
            "vendor/lib/.git",
        ] {
            fs::create_dir_all(top.join(sub)).unwrap();
        }
        let plain = || "justless".to_owned();

        // Started at the root: nothing to add.
        let at_root = workspace(&top, &top).invocation();
        assert_eq!(at_root.command, plain());
        assert_eq!(at_root.files, plain());
        // A subdirectory finds the same root, but bare paths for `read`, `expand`
        // and `edit` would start there.
        let below = workspace(&top, &top.join("docs")).invocation();
        assert_eq!(below.command, plain());
        assert_eq!(below.files, rooted(&top));
        // `--root billing` from the top, or from inside billing, which has no marker.
        let billing = top.join("billing");
        for origin in [&top, &billing] {
            let scoped = workspace(&billing, origin).invocation();
            assert_eq!(scoped.command, rooted(&billing), "{}", origin.display());
            assert_eq!(scoped.files, rooted(&billing), "{}", origin.display());
        }
        // A hook-routed run inside a nested repository records in the outer one.
        let nested = workspace(&top, &top.join("vendor/lib/src")).invocation();
        assert_eq!(nested.command, rooted(&top));
        assert_eq!(nested.files, rooted(&top));
        // A directory that no longer exists cannot be checked, so the root is named.
        let gone = workspace(&top, &top.join("removed")).invocation();
        assert_eq!(gone.command, rooted(&top));
        // A hook's reported directory replaces where the hook itself started.
        let hook = workspace(&top, &top).working_in(Path::new("vendor/lib"));
        assert_eq!(hook.justless(), rooted(&top));
    }

    #[test]
    fn a_printed_root_is_quoted_for_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().canonicalize().unwrap();
        let spaced = top.join("my project");
        fs::create_dir(&spaced).unwrap();
        let command = workspace(&spaced, &top).justless();
        assert_eq!(
            shlex::split(&command).unwrap(),
            ["justless", "--root", spaced.to_str().unwrap()]
        );
    }
}
