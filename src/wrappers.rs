//! Command wrappers that stand in front of a tool call.
//!
//! `effective` removes assignments, `env`/`time`/`nohup`/`command`/`nice`/
//! `timeout`/`stdbuf`, `bash -c` and git's global options so output can be
//! classified. It is never used to replay a command: the recorded words keep
//! the wrappers.

/// A shell variable assignment such as `NAME=value`.
pub(crate) fn assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let bytes = name.as_bytes();
    bytes
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn slice_tail<'a, 'b>(words: &'a [&'b str], n: usize) -> Option<&'a [&'b str]> {
    (words.len() > n).then(|| &words[n..])
}

fn env_rest_slice<'a, 'b>(words: &'a [&'b str]) -> Option<&'a [&'b str]> {
    let mut i = 1;
    while let Some(&word) = words.get(i) {
        match word {
            "-i" | "--ignore-environment" | "-0" | "--null" => i += 1,
            "-u" | "--unset" => i += 2,
            "--" => {
                i += 1;
                break;
            }
            word if word.starts_with("--unset=") => i += 1,
            word if assignment(word) => i += 1,
            _ => break,
        }
    }
    slice_tail(words, i)
}

/// Information about safe wrappers unwrapped by [`hook_effective_detail`] so
/// approval checks can enforce stricter rules on `env` flags and assignments,
/// and prevent auto-approval behind toolchain runners that may sync packages
/// or run environment hooks.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct HookEffective {
    pub words: Vec<String>,
    /// Whether an `env` wrapper passed `-i`/`--ignore-environment` or `-u`/`--unset`.
    pub env_clears: bool,
    /// `NAME=VALUE` assignments carried by any unwrapped `env` wrapper.
    pub env_assignments: Vec<(String, String)>,
    /// Whether an unwrapped toolchain runner (`uv run`, `mise exec`/`x`,
    /// `direnv exec`, `pixi run`, `fvm`, `volta run`, `fnm exec`,
    /// `pyenv`/`rbenv exec`, `pdm run`, `corepack`) may sync tools, evaluate
    /// `.envrc`/hooks, or switch toolchains, so auto-approving clients must ask first.
    pub runner_unapprovable: bool,
}

/// Zero-allocation borrowed counterpart of [`HookEffective`] for hot-path hook
/// checks (`filters::approvable`, `filters::profile`, `shell_form::Form::approvable`).
pub(crate) struct HookEffectiveRef<'a, 'b> {
    pub words: &'a [&'b str],
    pub env_clears: bool,
    pub env_assignments: Vec<(&'b str, &'b str)>,
    pub runner_unapprovable: bool,
}

/// Borrowed-or-owned result of [`effective_words`], avoiding `Vec<String>` clones
/// on the common path where no `git` flag stripping or `bash -c` splitting occurs.
pub(crate) enum EffectiveWords<'a, 'b> {
    Borrowed(&'a [&'b str]),
    Owned(Vec<String>),
}

impl EffectiveWords<'_, '_> {
    fn into_owned(self) -> Vec<String> {
        match self {
            Self::Borrowed(slice) => slice.iter().map(|w| (*w).to_owned()).collect(),
            Self::Owned(vec) => vec,
        }
    }
}

fn program_name(first: &str) -> &str {
    if !first.as_bytes().iter().any(|&b| b == b'/' || b == b'\\') {
        return first;
    }
    let path = std::path::Path::new(first);
    if path.is_absolute() {
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(first)
    } else {
        first
    }
}

fn is_timeout_duration(text: &str) -> bool {
    let num = text.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(text);
    if num.is_empty() {
        return false;
    }
    let (mut digits, mut dots) = (0usize, 0usize);
    for byte in num.bytes() {
        if byte.is_ascii_digit() {
            digits += 1;
        } else if byte == b'.' {
            dots += 1;
        } else {
            return false;
        }
    }
    digits > 0 && dots <= 1
}

fn is_timeout_signal(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('-')
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn is_signed_int(text: &str) -> bool {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_stdbuf_mode(text: &str) -> bool {
    if matches!(text, "0" | "L") {
        return true;
    }
    let split = text
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(text.len());
    if split == 0 {
        return false;
    }
    matches!(
        &text[split..],
        "" | "K"
            | "M"
            | "G"
            | "T"
            | "P"
            | "E"
            | "k"
            | "m"
            | "g"
            | "KB"
            | "MB"
            | "GB"
            | "TB"
            | "KiB"
            | "MiB"
            | "GiB"
            | "TiB"
    )
}

fn is_safe_runner_name(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.contains("..")
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Remaining command words after `uv run` and any validated read-only/project-local
/// flags. Rejects package-fetching/execution flags such as `--with`,
/// `--with-requirements`, `--from`, `--python-fetch`, `--script`, and non-local
/// `--directory`/`--project` paths.
pub(crate) fn uv_run_rest<'a, 'b>(mut args: &'a [&'b str]) -> Option<&'a [&'b str]> {
    while let Some((&word, rest)) = args.split_first() {
        match word {
            "--" => {
                args = rest;
                break;
            }
            "--frozen" | "--offline" | "--no-sync" | "--locked" | "--no-dev" | "--dev"
            | "--all-extras" | "-q" | "--quiet" => args = rest,
            "--group" | "--extra" => {
                let (&value, tail) = rest.split_first()?;
                if !is_safe_runner_name(value) {
                    return None;
                }
                args = tail;
            }
            "--directory" | "--project" => {
                let (&value, tail) = rest.split_first()?;
                if !crate::filters::is_local_dir(value) {
                    return None;
                }
                args = tail;
            }
            _ if word.starts_with("--group=") || word.starts_with("--extra=") => {
                let (_, value) = word.split_once('=')?;
                if !is_safe_runner_name(value) {
                    return None;
                }
                args = rest;
            }
            _ if word.starts_with("--directory=") || word.starts_with("--project=") => {
                let (_, value) = word.split_once('=')?;
                if !crate::filters::is_local_dir(value) {
                    return None;
                }
                args = rest;
            }
            _ if word.starts_with('-') => return None,
            _ => break,
        }
    }
    (!args.is_empty() && !args[0].starts_with('-')).then_some(args)
}

/// Remaining command words after `pixi run` and safe read-only/env-selection flags.
pub(crate) fn pixi_run_rest<'a, 'b>(mut args: &'a [&'b str]) -> Option<&'a [&'b str]> {
    while let Some((&word, rest)) = args.split_first() {
        match word {
            "--" => {
                args = rest;
                break;
            }
            "--frozen" | "--locked" | "--no-install" | "--clean-env" | "-q" | "--quiet" => {
                args = rest;
            }
            "-e" | "--environment" => {
                let (&value, tail) = rest.split_first()?;
                if !is_safe_runner_name(value) {
                    return None;
                }
                args = tail;
            }
            _ if word.starts_with("--environment=") => {
                let (_, value) = word.split_once('=')?;
                if !is_safe_runner_name(value) {
                    return None;
                }
                args = rest;
            }
            _ if word.starts_with('-') => return None,
            _ => break,
        }
    }
    (!args.is_empty() && !args[0].starts_with('-')).then_some(args)
}

/// Remaining command words after `fvm` (`fvm flutter ...` / `fvm dart ...`),
/// keeping `"flutter"` or `"dart"` as the unwrapped program and rejecting bare
/// or `--help`/`-h`/`--version`/`-V` invocations.
pub(crate) fn fvm_rest<'a, 'b>(words: &'a [&'b str]) -> Option<&'a [&'b str]> {
    match words {
        [_, "flutter" | "dart", arg, ..]
            if !matches!(*arg, "-h" | "--help" | "-V" | "--version") =>
        {
            Some(&words[1..])
        }
        _ => None,
    }
}

/// Remaining command words after `corepack` (`corepack pnpm|yarn|npm ...`),
/// keeping the package manager as the unwrapped program and rejecting bare or
/// `--help`/`-h`/`--version`/`-V` invocations.
pub(crate) fn corepack_rest<'a, 'b>(words: &'a [&'b str]) -> Option<&'a [&'b str]> {
    match words {
        [_, "pnpm" | "yarn" | "npm", arg, ..]
            if !matches!(*arg, "-h" | "--help" | "-V" | "--version") =>
        {
            Some(&words[1..])
        }
        _ => None,
    }
}

/// Remaining command words after `volta run` and safe toolchain version selectors
/// (`--node`, `--npm`, `--yarn`, `--pnpm`, `--bundled-npm`, `--no-yarn`, `--quiet`,
/// `--verbose`). Rejects `--env`, `--help`/`-h`, and unknown flags.
pub(crate) fn volta_run_rest<'a, 'b>(mut args: &'a [&'b str]) -> Option<&'a [&'b str]> {
    while let Some((&word, rest)) = args.split_first() {
        match word {
            "--" => {
                args = rest;
                break;
            }
            "--bundled-npm" | "--no-yarn" | "--quiet" | "--verbose" => args = rest,
            "--node" | "--npm" | "--yarn" | "--pnpm" => {
                let (&value, tail) = rest.split_first()?;
                if !is_safe_runner_name(value) {
                    return None;
                }
                args = tail;
            }
            _ if word.starts_with("--node=")
                || word.starts_with("--npm=")
                || word.starts_with("--yarn=")
                || word.starts_with("--pnpm=") =>
            {
                let (_, value) = word.split_once('=')?;
                if !is_safe_runner_name(value) {
                    return None;
                }
                args = rest;
            }
            _ if word.starts_with('-') => return None,
            _ => break,
        }
    }
    (!args.is_empty() && !args[0].starts_with('-')).then_some(args)
}

/// Remaining command words after `fnm exec [--using=X | --using X] -- CMD...`.
/// Requires `--` before the command and rejects `--help`/`-h` and unknown flags.
pub(crate) fn fnm_exec_rest<'a, 'b>(mut args: &'a [&'b str]) -> Option<&'a [&'b str]> {
    let mut saw_sep = false;
    while let Some((&word, rest)) = args.split_first() {
        match word {
            "--" => {
                args = rest;
                saw_sep = true;
                break;
            }
            "--using" => {
                let (&value, tail) = rest.split_first()?;
                if !is_safe_runner_name(value) {
                    return None;
                }
                args = tail;
            }
            _ if word.starts_with("--using=") => {
                let (_, value) = word.split_once('=')?;
                if !is_safe_runner_name(value) {
                    return None;
                }
                args = rest;
            }
            _ => return None,
        }
    }
    (saw_sep && !args.is_empty() && !args[0].starts_with('-')).then_some(args)
}

/// Remaining command words after shim-exec wrappers (`asdf exec`, `pyenv exec`,
/// `rbenv exec`), requiring a bare command name with no leading `-` or `/`.
pub(crate) fn shim_exec_rest<'a, 'b>(args: &'a [&'b str]) -> Option<&'a [&'b str]> {
    args.first()
        .is_some_and(|cmd| !cmd.starts_with('-') && !cmd.contains('/'))
        .then_some(args)
}

/// Remaining command words after `pdm run` and safe quiet/local-project/venv flags.
/// Rejects `--list`/`-l`, `--help`/`-h`, non-local `-p`/`--project`, and unknown flags.
pub(crate) fn pdm_run_rest<'a, 'b>(mut args: &'a [&'b str]) -> Option<&'a [&'b str]> {
    while let Some((&word, rest)) = args.split_first() {
        match word {
            "--" => {
                args = rest;
                break;
            }
            "-q" | "--quiet" => args = rest,
            "-p" | "--project" => {
                let (&value, tail) = rest.split_first()?;
                if !crate::filters::is_local_dir(value) {
                    return None;
                }
                args = tail;
            }
            "--venv" => {
                let (&value, tail) = rest.split_first()?;
                if !is_safe_runner_name(value) {
                    return None;
                }
                args = tail;
            }
            _ if word.starts_with("--project=") => {
                let (_, value) = word.split_once('=')?;
                if !crate::filters::is_local_dir(value) {
                    return None;
                }
                args = rest;
            }
            _ if word.starts_with("--venv=") => {
                let (_, value) = word.split_once('=')?;
                if !is_safe_runner_name(value) {
                    return None;
                }
                args = rest;
            }
            _ if word.starts_with('-') => return None,
            _ => break,
        }
    }
    (!args.is_empty() && !args[0].starts_with('-')).then_some(args)
}

/// Remaining command words after `poetry run` or `pipenv run`, skipping only
/// `-q`/`--quiet` and `--` while rejecting `-h`/`--help` and unknown flags.
pub(crate) fn poetry_run_rest<'a, 'b>(mut args: &'a [&'b str]) -> Option<&'a [&'b str]> {
    while let Some((&word, rest)) = args.split_first() {
        match word {
            "--" => {
                args = rest;
                break;
            }
            "-q" | "--quiet" => args = rest,
            _ if word.starts_with('-') => return None,
            _ => break,
        }
    }
    (!args.is_empty() && !args[0].starts_with('-')).then_some(args)
}

/// Remaining command words after `pipenv run`, skipping only `-q`/`--quiet`
/// and `--` while rejecting `-h`/`--help` and unknown flags.
pub(crate) fn pipenv_run_rest<'a, 'b>(args: &'a [&'b str]) -> Option<&'a [&'b str]> {
    poetry_run_rest(args)
}

/// Strictly unwrap ONLY `timeout`, `nice`, `stdbuf`, `env`, and validated
/// toolchain runner wrappers (`uv run`, `mise exec`/`x`, `asdf exec`,
/// `direnv exec`, `pixi run`, `fvm flutter`/`dart`, `volta run`, `fnm exec`,
/// `pyenv`/`rbenv exec`, `pdm run`, `poetry run`, `pipenv run`, `corepack`)
/// for command-hook routing and profile selection.
/// Never unwraps `bash -c`, `sh -c`, `time`, `nohup`, `command`, `sudo`, `su`,
/// `eval`, `exec`, `make`, `just`, `task`, or relative scripts.
pub(crate) fn hook_effective(words: &[&str]) -> Option<Vec<String>> {
    hook_effective_slice(words).map(|slice| slice.iter().map(|w| (*w).to_owned()).collect())
}

/// Zero-allocation borrowed counterpart of [`hook_effective`].
pub(crate) fn hook_effective_slice<'a, 'b>(words: &'a [&'b str]) -> Option<&'a [&'b str]> {
    unwrap_hook_slice(words, false).map(|detail| detail.words)
}

/// Zero-allocation borrowed counterpart of [`hook_effective_detail`].
pub(crate) fn hook_effective_ref<'a, 'b>(words: &'a [&'b str]) -> Option<HookEffectiveRef<'a, 'b>> {
    unwrap_hook_slice(words, true)
}

/// Like [`hook_effective`], also reporting `env` options/assignments and
/// whether an unwrapped toolchain runner requires client approval so
/// `filters::approvable` can keep auto-approved clients strictly safe.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn hook_effective_detail(words: &[&str]) -> Option<HookEffective> {
    let detail = hook_effective_ref(words)?;
    Some(HookEffective {
        words: detail.words.iter().map(|w| (*w).to_owned()).collect(),
        env_clears: detail.env_clears,
        env_assignments: detail
            .env_assignments
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        runner_unapprovable: detail.runner_unapprovable,
    })
}

fn unwrap_hook_slice<'a, 'b>(
    mut refs: &'a [&'b str],
    collect_env: bool,
) -> Option<HookEffectiveRef<'a, 'b>> {
    let mut env_clears = false;
    let mut env_assignments = Vec::new();
    let mut runner_unapprovable = false;
    loop {
        let &first = refs.first()?;
        let program = program_name(first);
        let rest = match program {
            "timeout" => {
                let mut i = 1;
                while let Some(&word) = refs.get(i) {
                    match word {
                        "--" => {
                            i += 1;
                            break;
                        }
                        "-s" | "--signal" => {
                            if !refs.get(i + 1).is_some_and(|sig| is_timeout_signal(sig)) {
                                return None;
                            }
                            i += 2;
                        }
                        "-k" | "--kill-after" => {
                            if !refs.get(i + 1).is_some_and(|dur| is_timeout_duration(dur)) {
                                return None;
                            }
                            i += 2;
                        }
                        "--preserve-status" | "--foreground" | "-v" | "--verbose" => i += 1,
                        _ if word.starts_with("--signal=") => {
                            if !is_timeout_signal(&word["--signal=".len()..]) {
                                return None;
                            }
                            i += 1;
                        }
                        _ if word.starts_with("--kill-after=") => {
                            if !is_timeout_duration(&word["--kill-after=".len()..]) {
                                return None;
                            }
                            i += 1;
                        }
                        _ if word.starts_with("-s") && word.len() > 2 => {
                            if !is_timeout_signal(&word[2..]) {
                                return None;
                            }
                            i += 1;
                        }
                        _ if word.starts_with("-k") && word.len() > 2 => {
                            if !is_timeout_duration(&word[2..]) {
                                return None;
                            }
                            i += 1;
                        }
                        _ if word.starts_with('-') => return None,
                        _ => break,
                    }
                }
                let duration = refs.get(i)?;
                if !is_timeout_duration(duration) {
                    return None;
                }
                slice_tail(refs, i + 1)?
            }
            "nice" => {
                let mut i = 1;
                while let Some(&word) = refs.get(i) {
                    match word {
                        "--" => {
                            i += 1;
                            break;
                        }
                        "-n" | "--adjustment" => {
                            if !refs.get(i + 1).is_some_and(|n| is_signed_int(n)) {
                                return None;
                            }
                            i += 2;
                        }
                        _ if word.starts_with("--adjustment=") => {
                            if !is_signed_int(&word["--adjustment=".len()..]) {
                                return None;
                            }
                            i += 1;
                        }
                        _ if word.starts_with("-n") && word.len() > 2 => {
                            if !is_signed_int(&word[2..]) {
                                return None;
                            }
                            i += 1;
                        }
                        _ if word.strip_prefix('-').is_some_and(|n| {
                            !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                        }) =>
                        {
                            i += 1;
                        }
                        _ if word.starts_with('-') => return None,
                        _ => break,
                    }
                }
                slice_tail(refs, i)?
            }
            "stdbuf" => {
                let mut i = 1;
                let mut saw_mode = false;
                while let Some(&word) = refs.get(i) {
                    match word {
                        "--" => {
                            i += 1;
                            break;
                        }
                        "-i" | "-o" | "-e" | "--input" | "--output" | "--error" => {
                            if !refs.get(i + 1).is_some_and(|mode| is_stdbuf_mode(mode)) {
                                return None;
                            }
                            saw_mode = true;
                            i += 2;
                        }
                        _ if word
                            .strip_prefix("--input=")
                            .or_else(|| word.strip_prefix("--output="))
                            .or_else(|| word.strip_prefix("--error="))
                            .is_some_and(is_stdbuf_mode) =>
                        {
                            saw_mode = true;
                            i += 1;
                        }
                        _ if word
                            .strip_prefix("-i")
                            .or_else(|| word.strip_prefix("-o"))
                            .or_else(|| word.strip_prefix("-e"))
                            .filter(|rest| !rest.is_empty())
                            .is_some_and(is_stdbuf_mode) =>
                        {
                            saw_mode = true;
                            i += 1;
                        }
                        _ if word.starts_with('-') => return None,
                        _ => break,
                    }
                }
                if !saw_mode {
                    return None;
                }
                slice_tail(refs, i)?
            }
            "env" => {
                let mut i = 1;
                while let Some(&word) = refs.get(i) {
                    match word {
                        "--" => {
                            i += 1;
                            break;
                        }
                        "-i" | "--ignore-environment" => {
                            env_clears = true;
                            i += 1;
                        }
                        "-u" | "--unset" => {
                            let &var = refs.get(i + 1)?;
                            if var.is_empty()
                                || var.starts_with('-')
                                || crate::shell_form::steers(var)
                            {
                                return None;
                            }
                            env_clears = true;
                            i += 2;
                        }
                        _ if word.starts_with("--unset=") => {
                            let var = &word["--unset=".len()..];
                            if var.is_empty()
                                || var.starts_with('-')
                                || crate::shell_form::steers(var)
                            {
                                return None;
                            }
                            env_clears = true;
                            i += 1;
                        }
                        _ if word.starts_with("-u") && word.len() > 2 => {
                            let var = &word[2..];
                            if var.starts_with('-') || crate::shell_form::steers(var) {
                                return None;
                            }
                            env_clears = true;
                            i += 1;
                        }
                        _ if assignment(word) => {
                            let (name, value) = word.split_once('=')?;
                            if crate::shell_form::steers(name) {
                                return None;
                            }
                            if collect_env {
                                env_assignments.push((name, value));
                            }
                            i += 1;
                        }
                        _ if word.starts_with('-') => return None,
                        _ => break,
                    }
                }
                slice_tail(refs, i)?
            }
            "uv" if refs.get(1) == Some(&"run") => {
                runner_unapprovable = true;
                uv_run_rest(&refs[2..])?
            }
            "mise" if matches!(refs.get(1), Some(&"exec" | &"x")) => {
                // Only `mise exec -- <cmd>` / `mise x -- <cmd>` without tool@version
                // installs or directory-changing flags before `--`.
                if refs.get(2) != Some(&"--") || !refs.get(3).is_some_and(|w| !w.starts_with('-')) {
                    return None;
                }
                runner_unapprovable = true;
                slice_tail(refs, 3)?
            }
            "asdf" if refs.get(1) == Some(&"exec") => shim_exec_rest(&refs[2..])?,
            "pyenv" | "rbenv" if refs.get(1) == Some(&"exec") => {
                runner_unapprovable = true;
                shim_exec_rest(&refs[2..])?
            }
            "direnv" if refs.get(1) == Some(&"exec") => {
                if !refs
                    .get(2)
                    .is_some_and(|dir| crate::filters::is_local_dir(dir))
                    || !refs.get(3).is_some_and(|cmd| !cmd.starts_with('-'))
                {
                    return None;
                }
                runner_unapprovable = true;
                slice_tail(refs, 3)?
            }
            "pixi" if refs.get(1) == Some(&"run") => {
                runner_unapprovable = true;
                pixi_run_rest(&refs[2..])?
            }
            "fvm" if matches!(refs.get(1), Some(&"flutter" | &"dart")) => {
                runner_unapprovable = true;
                fvm_rest(refs)?
            }
            "volta" if refs.get(1) == Some(&"run") => {
                runner_unapprovable = true;
                volta_run_rest(&refs[2..])?
            }
            "fnm" if refs.get(1) == Some(&"exec") => {
                runner_unapprovable = true;
                fnm_exec_rest(&refs[2..])?
            }
            "pdm" if refs.get(1) == Some(&"run") => {
                runner_unapprovable = true;
                pdm_run_rest(&refs[2..])?
            }
            "poetry" if refs.get(1) == Some(&"run") => poetry_run_rest(&refs[2..])?,
            "pipenv" if refs.get(1) == Some(&"run") => pipenv_run_rest(&refs[2..])?,
            "corepack" if matches!(refs.get(1), Some(&"pnpm" | &"yarn" | &"npm")) => {
                runner_unapprovable = true;
                corepack_rest(refs)?
            }
            _ => break,
        };
        refs = rest;
    }
    Some(HookEffectiveRef {
        words: refs,
        env_clears,
        env_assignments,
        runner_unapprovable,
    })
}

/// The command a wrapper actually runs, or `None` for a wrapper option this
/// does not know.
pub(crate) fn effective(words: &[&str]) -> Option<Vec<String>> {
    effective_words(words).map(EffectiveWords::into_owned)
}

/// Zero-allocation fast-path counterpart of [`effective`], returning a borrowed
/// subslice whenever no `git` flag stripping or `bash -c` splitting occurs.
pub(crate) fn effective_words<'a, 'b>(mut refs: &'a [&'b str]) -> Option<EffectiveWords<'a, 'b>> {
    loop {
        while let Some((&first, rest)) = refs.split_first() {
            if assignment(first) {
                refs = rest;
            } else {
                break;
            }
        }
        let &first = refs.first()?;
        let program = program_name(first);
        let rest = match program {
            "env" => env_rest_slice(refs)?,
            "time" | "nohup" | "command" => match refs.get(1).copied() {
                Some("-p") | Some("--") => slice_tail(refs, 2)?,
                Some(word) if word.starts_with('-') => return None,
                _ => slice_tail(refs, 1)?,
            },
            "nice" => match refs.get(1).copied() {
                Some("-n") | Some("--adjustment") => slice_tail(refs, 3)?,
                Some(word) if word.starts_with("--adjustment=") => slice_tail(refs, 2)?,
                Some(word) if word.starts_with('-') => slice_tail(refs, 2)?,
                _ => slice_tail(refs, 1)?,
            },
            "timeout" => {
                let mut i = 1;
                while let Some(&word) = refs.get(i) {
                    if word == "-s" || word == "--signal" || word == "-k" || word == "--kill-after"
                    {
                        i += 2;
                    } else if word.starts_with("--signal=")
                        || word.starts_with("--kill-after=")
                        || (word.starts_with('-') && word.len() > 1)
                    {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let duration = refs.get(i)?;
                if !duration.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                    return None;
                }
                slice_tail(refs, i + 1)?
            }
            "stdbuf" => {
                let mut i = 1;
                while let Some(&word) = refs.get(i) {
                    if word == "--" {
                        i += 1;
                        break;
                    }
                    if matches!(
                        word,
                        "-i" | "-o" | "-e" | "--input" | "--output" | "--error"
                    ) {
                        i += 2;
                    } else if word.starts_with('-') {
                        i += 1;
                    } else {
                        break;
                    }
                }
                slice_tail(refs, i)?
            }
            "bash" | "sh" | "zsh" | "dash" => {
                let mut i = 1;
                let mut found = false;
                while let Some(&word) = refs.get(i) {
                    if word == "-c" {
                        found = true;
                        i += 1;
                        break;
                    }
                    if word
                        .strip_prefix('-')
                        .is_some_and(|flags| !word.starts_with("--") && flags.contains('c'))
                    {
                        found = true;
                        i += 1;
                        break;
                    }
                    if word.starts_with("--") {
                        return None;
                    }
                    i += 1;
                }
                if !found || refs.len() != i + 1 {
                    return None;
                }
                let inner = shlex::split(refs[i])?;
                if inner
                    .iter()
                    .any(|w| w.contains([';', '|', '&', '>', '<', '(', ')']))
                {
                    return None;
                }
                let inner_refs: Vec<&str> = inner.iter().map(String::as_str).collect();
                return Some(EffectiveWords::Owned(effective(&inner_refs)?));
            }
            "git" => {
                let mut i = 1;
                while let Some(&word) = refs.get(i) {
                    match word {
                        "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
                        | "--exec-path" | "--config-env" => i += 2,
                        "--no-pager"
                        | "-P"
                        | "--paginate"
                        | "-p"
                        | "--bare"
                        | "--literal-pathspecs"
                        | "--glob-pathspecs"
                        | "--noglob-pathspecs"
                        | "--icase-pathspecs"
                        | "--no-replace-objects"
                        | "--no-optional-locks" => i += 1,
                        word if word.starts_with("--git-dir=")
                            || word.starts_with("--work-tree=")
                            || word.starts_with("--namespace=")
                            || word.starts_with("--exec-path=")
                            || word.starts_with("--config-env=") =>
                        {
                            i += 1;
                        }
                        word if word.starts_with("-C") && word.len() > 2 => i += 1,
                        word if word.starts_with('-') => return None,
                        _ => break,
                    }
                }
                if i == 1 && first == "git" {
                    return Some(EffectiveWords::Borrowed(refs));
                }
                let mut out = Vec::with_capacity(1 + refs.len().saturating_sub(i));
                out.push("git".to_owned());
                out.extend(refs[i..].iter().map(|w| (*w).to_owned()));
                return Some(EffectiveWords::Owned(out));
            }
            "uv" if refs.get(1) == Some(&"run") => uv_run_rest(&refs[2..])?,
            "mise" if matches!(refs.get(1), Some(&"exec" | &"x")) => {
                if refs.get(2) != Some(&"--") || !refs.get(3).is_some_and(|w| !w.starts_with('-')) {
                    return None;
                }
                slice_tail(refs, 3)?
            }
            "asdf" | "pyenv" | "rbenv" if refs.get(1) == Some(&"exec") => {
                shim_exec_rest(&refs[2..])?
            }
            "direnv" if refs.get(1) == Some(&"exec") => {
                if !refs
                    .get(2)
                    .is_some_and(|dir| crate::filters::is_local_dir(dir))
                    || !refs.get(3).is_some_and(|cmd| !cmd.starts_with('-'))
                {
                    return None;
                }
                slice_tail(refs, 3)?
            }
            "pixi" if refs.get(1) == Some(&"run") => pixi_run_rest(&refs[2..])?,
            "fvm" if matches!(refs.get(1), Some(&"flutter" | &"dart")) => fvm_rest(refs)?,
            "volta" if refs.get(1) == Some(&"run") => volta_run_rest(&refs[2..])?,
            "fnm" if refs.get(1) == Some(&"exec") => fnm_exec_rest(&refs[2..])?,
            "pdm" if refs.get(1) == Some(&"run") => pdm_run_rest(&refs[2..])?,
            "poetry" if refs.get(1) == Some(&"run") => poetry_run_rest(&refs[2..])?,
            "pipenv" if refs.get(1) == Some(&"run") => pipenv_run_rest(&refs[2..])?,
            "corepack" if matches!(refs.get(1), Some(&"pnpm" | &"yarn" | &"npm")) => {
                corepack_rest(refs)?
            }
            _ => break,
        };
        refs = rest;
    }
    Some(EffectiveWords::Borrowed(refs))
}

#[cfg(test)]
mod tests {
    use super::effective;

    fn effective_words(command: &str) -> Option<Vec<String>> {
        let words = shlex::split(command)?;
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        effective(&refs)
    }

    #[test]
    fn wrappers_are_looked_through() {
        assert_eq!(
            effective_words("timeout 5 sed -n 1,2p f").unwrap(),
            ["sed", "-n", "1,2p", "f"]
        );
        assert_eq!(effective_words("env -u HOME cat f").unwrap(), ["cat", "f"]);
        assert_eq!(effective_words("nice -n 5 cat f").unwrap(), ["cat", "f"]);
        assert_eq!(
            effective_words("bash -c 'sed -n 1,2p f'").unwrap(),
            ["sed", "-n", "1,2p", "f"]
        );
        assert_eq!(effective_words("git -C . diff").unwrap(), ["git", "diff"]);
    }

    #[test]
    fn wrapper_options_are_looked_through() {
        assert_eq!(
            effective_words("timeout -s KILL 5 cat f").unwrap(),
            ["cat", "f"]
        );
        assert_eq!(
            effective_words("timeout --signal=KILL --kill-after=1 5 cat f").unwrap(),
            ["cat", "f"]
        );
        assert_eq!(effective_words("env -i cat f").unwrap(), ["cat", "f"]);
        assert_eq!(effective_words("command -p cat f").unwrap(), ["cat", "f"]);
        assert_eq!(effective_words("time -p cat f").unwrap(), ["cat", "f"]);
        assert_eq!(effective_words("stdbuf -oL cat f").unwrap(), ["cat", "f"]);
        assert_eq!(
            effective_words("stdbuf -o L -e 0 cat f").unwrap(),
            ["cat", "f"]
        );
        assert_eq!(
            effective_words("stdbuf --output L -- cat f").unwrap(),
            ["cat", "f"]
        );
        assert_eq!(effective_words("bash -lc 'cat f'").unwrap(), ["cat", "f"]);
        assert_eq!(
            effective_words("git --no-pager -C . diff").unwrap(),
            ["git", "diff"]
        );
        assert_eq!(
            effective_words("/usr/bin/git --no-pager diff").unwrap(),
            ["git", "diff"]
        );
        assert_eq!(
            effective_words("/usr/bin/env -u HOME /usr/bin/timeout 5 /usr/bin/git -P show")
                .unwrap(),
            ["git", "show"]
        );
        // Relative scripts with wrapper names are never unwrapped.
        assert_eq!(
            effective_words("./git --no-pager diff").unwrap(),
            ["./git", "--no-pager", "diff"]
        );
        assert_eq!(
            effective_words("scripts/env cat f").unwrap(),
            ["scripts/env", "cat", "f"]
        );
        assert_eq!(effective_words("LC_ALL=C cat f").unwrap(), ["cat", "f"]);
    }

    #[test]
    fn unwrapped_commands_are_unchanged() {
        assert_eq!(effective_words("cat f").unwrap(), ["cat", "f"]);
        assert_eq!(
            effective_words("rg -n needle src").unwrap(),
            ["rg", "-n", "needle", "src"]
        );
    }

    #[test]
    fn unsafe_wrappers_are_not_unwrapped() {
        assert_eq!(effective_words("command -v cat"), None);
        assert_eq!(effective_words("time -v cat"), None);
        assert_eq!(effective_words("bash -c 'cargo test; rm -rf /'"), None);
    }

    fn hook_effective_words(command: &str) -> Option<Vec<String>> {
        let words = shlex::split(command)?;
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        super::hook_effective(&refs)
    }

    #[test]
    fn hook_effective_unwraps_only_safe_validated_wrappers() {
        assert_eq!(
            hook_effective_words("timeout 60 cargo test").unwrap(),
            ["cargo", "test"]
        );
        assert_eq!(
            hook_effective_words(
                "timeout -s KILL -k 5s --preserve-status --foreground -v 1.5m cargo test"
            )
            .unwrap(),
            ["cargo", "test"]
        );
        assert_eq!(
            hook_effective_words("timeout --signal=TERM --kill-after=10 -- 30s pytest").unwrap(),
            ["pytest"]
        );
        assert_eq!(
            hook_effective_words("/usr/bin/timeout 60 /usr/bin/cargo test").unwrap(),
            ["/usr/bin/cargo", "test"]
        );
        assert_eq!(
            hook_effective_words("nice cargo test").unwrap(),
            ["cargo", "test"]
        );
        assert_eq!(
            hook_effective_words("nice -n 10 cargo test").unwrap(),
            ["cargo", "test"]
        );
        assert_eq!(
            hook_effective_words("nice -n-5 cargo test").unwrap(),
            ["cargo", "test"]
        );
        assert_eq!(
            hook_effective_words("nice --adjustment=+5 cargo test").unwrap(),
            ["cargo", "test"]
        );
        assert_eq!(
            hook_effective_words("nice -10 cargo test").unwrap(),
            ["cargo", "test"]
        );
        assert_eq!(
            hook_effective_words("stdbuf -oL cargo test").unwrap(),
            ["cargo", "test"]
        );
        assert_eq!(
            hook_effective_words("stdbuf -i 0 -o L -e 4K cargo test").unwrap(),
            ["cargo", "test"]
        );
        assert_eq!(
            hook_effective_words("stdbuf --output=1MiB -- cargo test").unwrap(),
            ["cargo", "test"]
        );
        assert_eq!(
            hook_effective_words("env RUST_BACKTRACE=1 cargo test").unwrap(),
            ["cargo", "test"]
        );
        assert_eq!(
            hook_effective_words("env -i -u HOME FOO=bar -- cargo test").unwrap(),
            ["cargo", "test"]
        );
        assert_eq!(
            hook_effective_words("timeout 60 nice -n 5 stdbuf -oL env LC_ALL=C cargo clippy")
                .unwrap(),
            ["cargo", "clippy"]
        );

        // `bash -c`, `sh -c`, `time`, `nohup`, `command`, relative scripts stay untouched.
        for untouched in [
            "bash -c 'cargo test'",
            "sh -c 'cargo test'",
            "time cargo test",
            "nohup cargo test",
            "command cargo test",
            "./timeout 60 cargo test",
            "scripts/env RUST_LOG=1 cargo test",
        ] {
            let words = shlex::split(untouched).unwrap();
            assert_eq!(hook_effective_words(untouched), Some(words), "{untouched}");
        }

        // Unsafe or unknown wrapper flags and steering variables fail with None.
        for rejected in [
            "env -S 'cargo test'",
            "env --split-string='cargo test'",
            "env -C /tmp cargo test",
            "env --chdir=/tmp cargo test",
            "env --argv0=evil cargo test",
            "env PATH=/tmp/bin cargo test",
            "env path=/tmp/bin cargo test",
            "env LD_PRELOAD=./x.so cargo test",
            "env DYLD_INSERT_LIBRARIES=./x.dylib cargo test",
            "env JUSTLESS_DISABLE=1 cargo test",
            "env -u PATH cargo test",
            "env --unset=LD_LIBRARY_PATH cargo test",
            "env -uJUSTLESS_SESSION cargo test",
            "timeout --unknown 60 cargo test",
            "timeout -x 60 cargo test",
            "timeout abc cargo test",
            "timeout -k nope 60 cargo test",
            "timeout -s -bad 60 cargo test",
            "timeout 60",
            "nice -x cargo test",
            "nice -n bad cargo test",
            "stdbuf cargo test",
            "stdbuf -oX cargo test",
            "stdbuf -z cargo test",
            "uv run",
            "uv run --frozen",
            "uv run --with evil pytest",
            "uv run --with=evil pytest",
            "uv run -w evil pytest",
            "uv run --with-requirements req.txt pytest",
            "uv run --from evil pytest",
            "uv run --python-fetch automatic pytest",
            "uv run --directory /tmp pytest",
            "uv run --directory ../outside pytest",
            "uv run --project ~/app pytest",
            "uv run --group ../bad pytest",
            "mise exec python@3.12 -- pytest",
            "mise x -C /tmp -- pytest",
            "mise exec cargo test",
            "mise exec --",
            "asdf exec",
            "asdf exec --bad cargo",
            "asdf exec ./evil.sh",
            "direnv exec /tmp pytest",
            "direnv exec ../outside pytest",
            "direnv exec ~/app pytest",
            "direnv exec .",
            "pixi run",
            "pixi run --manifest-path /tmp/pixi.toml pytest",
        ] {
            assert_eq!(hook_effective_words(rejected), None, "{rejected}");
        }

        // Validated toolchain runner wrappers unwrap with expected approval flags.
        for (command, expected, unapprovable) in [
            ("uv run pytest -q", &["pytest", "-q"][..], true),
            (
                "uv run --frozen --offline --no-sync pytest",
                &["pytest"][..],
                true,
            ),
            (
                "uv run --locked --no-dev --group lint --extra test --directory packages/api ruff check .",
                &["ruff", "check", "."][..],
                true,
            ),
            (
                "uv run --dev --all-extras --project=apps/web -- mypy src",
                &["mypy", "src"][..],
                true,
            ),
            ("mise exec -- cargo test", &["cargo", "test"][..], true),
            ("mise x -- pytest -q", &["pytest", "-q"][..], true),
            ("asdf exec cargo test", &["cargo", "test"][..], false),
            ("direnv exec . pytest", &["pytest"][..], true),
            (
                "direnv exec packages/api cargo test",
                &["cargo", "test"][..],
                true,
            ),
            ("pixi run pytest", &["pytest"][..], true),
            (
                "pixi run --frozen -e test ruff check .",
                &["ruff", "check", "."][..],
                true,
            ),
            ("timeout 60 uv run --frozen pytest", &["pytest"][..], true),
        ] {
            let words = shlex::split(command).unwrap();
            let refs: Vec<&str> = words.iter().map(String::as_str).collect();
            let detail = super::hook_effective_detail(&refs).expect(command);
            assert_eq!(detail.words, expected, "{command}");
            assert_eq!(detail.runner_unapprovable, unapprovable, "{command}");
            assert_eq!(effective_words(command).unwrap(), expected, "{command}");
        }

        // Non-runner subcommands and relative scripts stay untouched.
        for untouched in [
            "uv sync",
            "mise ls",
            "asdf list",
            "direnv status",
            "pixi install",
            "./uv run pytest",
            "./mise exec -- cargo test",
            "./asdf exec cargo test",
            "./direnv exec . pytest",
            "./pixi run pytest",
        ] {
            let words = shlex::split(untouched).unwrap();
            assert_eq!(hook_effective_words(untouched), Some(words), "{untouched}");
        }
    }

    #[test]
    fn modern_toolchain_wrappers_unwrap_safely_and_reject_unsafe_wrappers() {
        for (command, expected, unapprovable) in [
            ("fvm flutter test", &["flutter", "test"][..], true),
            (
                "fvm dart pub deps --no-dev",
                &["dart", "pub", "deps", "--no-dev"][..],
                true,
            ),
            (
                "fvm dart analyze lib/",
                &["dart", "analyze", "lib/"][..],
                true,
            ),
            ("volta run jest", &["jest"][..], true),
            (
                "volta run --node 20 --npm 10 --yarn=4 --pnpm=9 npm test",
                &["npm", "test"][..],
                true,
            ),
            (
                "volta run --bundled-npm --no-yarn -- vitest run",
                &["vitest", "run"][..],
                true,
            ),
            ("fnm exec -- npm test", &["npm", "test"][..], true),
            ("fnm exec --using=20 -- jest", &["jest"][..], true),
            (
                "fnm exec --using v20.11.0 -- pnpm test",
                &["pnpm", "test"][..],
                true,
            ),
            ("pyenv exec pytest -q", &["pytest", "-q"][..], true),
            ("rbenv exec rspec", &["rspec"][..], true),
            (
                "rbenv exec bundle exec rubocop",
                &["bundle", "exec", "rubocop"][..],
                true,
            ),
            ("pdm run pytest", &["pytest"][..], true),
            (
                "pdm run -q -p packages/api --venv test ruff check .",
                &["ruff", "check", "."][..],
                true,
            ),
            ("poetry run pytest", &["pytest"][..], false),
            ("poetry run -q -- mypy src", &["mypy", "src"][..], false),
            ("pipenv run pytest", &["pytest"][..], false),
            ("corepack pnpm test", &["pnpm", "test"][..], true),
            ("corepack yarn build", &["yarn", "build"][..], true),
            ("corepack npm test", &["npm", "test"][..], true),
            (
                "timeout 60 volta run --node 20 corepack pnpm test",
                &["pnpm", "test"][..],
                true,
            ),
        ] {
            let words = shlex::split(command).unwrap();
            let refs: Vec<&str> = words.iter().map(String::as_str).collect();
            let detail = super::hook_effective_detail(&refs).expect(command);
            assert_eq!(detail.words, expected, "{command}");
            assert_eq!(detail.runner_unapprovable, unapprovable, "{command}");
            assert_eq!(effective_words(command).unwrap(), expected, "{command}");
        }

        let env_detail = super::hook_effective_detail(&[
            "env",
            "-i",
            "RUST_BACKTRACE=1",
            "volta",
            "run",
            "jest",
        ])
        .expect("env + volta");
        assert!(env_detail.env_clears);
        assert_eq!(
            env_detail.env_assignments,
            vec![("RUST_BACKTRACE".to_owned(), "1".to_owned())]
        );
        assert_eq!(env_detail.words, ["jest"]);
        assert!(env_detail.runner_unapprovable);

        // Wrapper --help/-h, invalid flags, missing targets, and escaping paths fail closed.
        for rejected in [
            "fvm flutter",
            "fvm flutter --help",
            "fvm dart -h",
            "fvm flutter --version",
            "volta run",
            "volta run --help",
            "volta run -h",
            "volta run --env PATH=/tmp jest",
            "volta run --node ../bad jest",
            "fnm exec npm test",
            "fnm exec --help",
            "fnm exec --using=../bad -- npm test",
            "fnm exec --",
            "pyenv exec",
            "pyenv exec --help",
            "pyenv exec ./evil.sh",
            "rbenv exec -h",
            "rbenv exec /tmp/evil",
            "pdm run",
            "pdm run --help",
            "pdm run --list",
            "pdm run -p ../outside pytest",
            "pdm run --project=/tmp pytest",
            "poetry run",
            "poetry run --help",
            "poetry run --unknown pytest",
            "pipenv run -h",
            "corepack pnpm",
            "corepack pnpm --help",
            "corepack npm -V",
        ] {
            assert_eq!(hook_effective_words(rejected), None, "{rejected}");
            assert_eq!(effective_words(rejected), None, "effective: {rejected}");
        }

        // Unsafe eval/escalation wrappers, task runners, non-wrapper subcommands, and
        // relative paths are never unwrapped in hook mode.
        for untouched in [
            "sudo cargo test",
            "su -c 'cargo test'",
            "eval 'cargo test'",
            "exec cargo test",
            "bash -c 'cargo test'",
            "sh -c 'cargo test'",
            "make test",
            "just test",
            "task build",
            "fvm list",
            "fvm --help",
            "volta install node",
            "fnm use 20",
            "pyenv versions",
            "rbenv versions",
            "pdm install",
            "poetry install",
            "pipenv lock",
            "corepack enable",
            "./fvm flutter test",
            "./volta run jest",
            "./fnm exec -- npm test",
            "./pyenv exec pytest",
            "./corepack pnpm test",
        ] {
            let words = shlex::split(untouched).unwrap();
            assert_eq!(hook_effective_words(untouched), Some(words), "{untouched}");
        }
        // `sudo`, `eval`, `exec`, `make`, `just`, `task` also stay untouched in `effective`.
        for untouched in [
            "sudo cargo test",
            "eval 'cargo test'",
            "exec cargo test",
            "make test",
            "just test",
            "task build",
        ] {
            let words = shlex::split(untouched).unwrap();
            assert_eq!(effective_words(untouched), Some(words), "{untouched}");
        }
    }
}
