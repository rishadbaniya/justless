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
        if self.start == self.end {
            write!(formatter, "{}:{}", self.file, self.start)
        } else {
            write!(formatter, "{}:{}-{}", self.file, self.start, self.end)
        }
    }
}

pub struct Workspace {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub state: PathBuf,
}

const PROJECT_MARKERS: &[&str] = &[
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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            filters: Vec::new(),
            context_budget: 2400,
            max_output_bytes: 4096,
            max_file_bytes: 2 * 1024 * 1024,
        }
    }
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
            None => start
                .ancestors()
                .find(|ancestor| ancestor.join(".git").exists())
                .or_else(|| {
                    start.ancestors().find(|ancestor| {
                        ancestor.join(".justless").is_dir()
                            || ancestor.join(".justless.toml").is_file()
                    })
                })
                .or_else(|| {
                    start.ancestors().find(|ancestor| {
                        PROJECT_MARKERS
                            .iter()
                            .any(|name| ancestor.join(name).is_file())
                    })
                })
                .unwrap_or(&start)
                .to_path_buf(),
        };
        ensure!(root.is_dir(), "{} is not a directory", root.display());
        let cwd = if cwd.starts_with(&root) {
            cwd
        } else {
            root.clone()
        };
        let state = root.join(".justless");
        Ok(Self { root, cwd, state })
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
            canonical == path && canonical.is_file(),
            "expected a workspace file: {file}"
        );
        Ok(canonical)
    }

    pub fn relative_file(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(&self.root).ok()?;
        let name = relative.to_str()?;
        self.source_path(name).ok()?;
        Some(name.to_owned())
    }

    pub fn prepare_state(&self) -> Result<()> {
        private_dir(&self.state)
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
        Ok(settings)
    }
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
        .open(path)?;
    file.lock()?;
    Ok(file)
}
