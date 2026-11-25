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
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn tail(words: &[&str], n: usize) -> Option<Vec<String>> {
    (words.len() > n).then(|| words[n..].iter().map(|w| (*w).to_owned()).collect())
}

fn env_rest(words: &[&str]) -> Option<Vec<String>> {
    let mut i = 1;
    while let Some(word) = words.get(i) {
        match *word {
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
    tail(words, i)
}

/// Information about safe wrappers unwrapped by [`hook_effective_detail`] so
/// approval checks can enforce stricter rules on `env` flags and assignments,
/// and prevent auto-approval behind toolchain runners that may sync packages
/// or run environment hooks.
pub(crate) struct HookEffective {
    pub words: Vec<String>,
    /// Whether an `env` wrapper passed `-i`/`--ignore-environment` or `-u`/`--unset`.
    pub env_clears: bool,
    /// `NAME=VALUE` assignments carried by any unwrapped `env` wrapper.
    pub env_assignments: Vec<(String, String)>,
    /// Whether an unwrapped toolchain runner (`uv run`, `mise exec`/`x`,
    /// `direnv exec`, `pixi run`) may sync tools, evaluate `.envrc`/hooks, or
    /// run project tasks, so auto-approving clients must ask first.
    pub runner_unapprovable: bool,
}

fn program_name(first: &str) -> &str {
    if std::path::Path::new(first).is_absolute() || !first.contains('/') {
        std::path::Path::new(first)
            .file_name()
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
fn pixi_run_rest<'a, 'b>(mut args: &'a [&'b str]) -> Option<&'a [&'b str]> {
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

/// Strictly unwrap ONLY `timeout`, `nice`, `stdbuf`, `env`, and validated
/// toolchain runner wrappers (`uv run`, `mise exec`/`x`, `asdf exec`,
/// `direnv exec`, `pixi run`) for command-hook routing and profile selection.
/// Never unwraps `bash -c`, `sh -c`, `time`, `nohup`, `command`, or relative scripts.
pub(crate) fn hook_effective(words: &[&str]) -> Option<Vec<String>> {
    hook_effective_detail(words).map(|detail| detail.words)
}

/// Like [`hook_effective`], also reporting `env` options/assignments and
/// whether an unwrapped toolchain runner requires client approval so
/// `filters::approvable` can keep auto-approved clients strictly safe.
pub(crate) fn hook_effective_detail(words: &[&str]) -> Option<HookEffective> {
    let mut words: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
    let mut env_clears = false;
    let mut env_assignments = Vec::new();
    let mut runner_unapprovable = false;
    loop {
        let first = words.first()?.clone();
        let program = program_name(&first);
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
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
                tail(&refs, i + 1)?
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
                tail(&refs, i)?
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
                tail(&refs, i)?
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
                            env_assignments.push((name.to_owned(), value.to_owned()));
                            i += 1;
                        }
                        _ if word.starts_with('-') => return None,
                        _ => break,
                    }
                }
                tail(&refs, i)?
            }
            "uv" if refs.get(1) == Some(&"run") => {
                let inner = uv_run_rest(&refs[2..])?;
                runner_unapprovable = true;
                inner.iter().map(|w| (*w).to_owned()).collect()
            }
            "mise" if matches!(refs.get(1), Some(&"exec" | &"x")) => {
                // Only `mise exec -- <cmd>` / `mise x -- <cmd>` without tool@version
                // installs or directory-changing flags before `--`.
                if refs.get(2) != Some(&"--") || !refs.get(3).is_some_and(|w| !w.starts_with('-')) {
                    return None;
                }
                runner_unapprovable = true;
                tail(&refs, 3)?
            }
            "asdf" if refs.get(1) == Some(&"exec") => {
                if !refs
                    .get(2)
                    .is_some_and(|cmd| !cmd.starts_with('-') && !cmd.contains('/'))
                {
                    return None;
                }
                tail(&refs, 2)?
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
                tail(&refs, 3)?
            }
            "pixi" if refs.get(1) == Some(&"run") => {
                let inner = pixi_run_rest(&refs[2..])?;
                runner_unapprovable = true;
                inner.iter().map(|w| (*w).to_owned()).collect()
            }
            _ => break,
        };
        words = rest;
    }
    Some(HookEffective {
        words,
        env_clears,
        env_assignments,
        runner_unapprovable,
    })
}

/// The command a wrapper actually runs, or `None` for a wrapper option this
/// does not know.
pub(crate) fn effective(words: &[&str]) -> Option<Vec<String>> {
    let mut words: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
    loop {
        while words.first().is_some_and(|w| assignment(w)) {
            words.remove(0);
        }
        let first = words.first()?.clone();
        let program = if std::path::Path::new(&first).is_absolute() || !first.contains('/') {
            std::path::Path::new(&first)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(first.as_str())
        } else {
            first.as_str()
        };
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        let rest = match program {
            "env" => env_rest(&refs)?,
            "time" | "nohup" | "command" => match refs.get(1).copied() {
                Some("-p") | Some("--") => tail(&refs, 2)?,
                Some(word) if word.starts_with('-') => return None,
                _ => tail(&refs, 1)?,
            },
            "nice" => match refs.get(1).copied() {
                Some("-n") | Some("--adjustment") => tail(&refs, 3)?,
                Some(word) if word.starts_with("--adjustment=") => tail(&refs, 2)?,
                Some(word) if word.starts_with('-') => tail(&refs, 2)?,
                _ => tail(&refs, 1)?,
            },
            "timeout" => {
                let mut i = 1;
                while let Some(word) = refs.get(i) {
                    if *word == "-s"
                        || *word == "--signal"
                        || *word == "-k"
                        || *word == "--kill-after"
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
                if !duration.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    return None;
                }
                tail(&refs, i + 1)?
            }
            "stdbuf" => {
                let mut i = 1;
                while let Some(word) = refs.get(i) {
                    if *word == "--" {
                        i += 1;
                        break;
                    }
                    if matches!(
                        *word,
                        "-i" | "-o" | "-e" | "--input" | "--output" | "--error"
                    ) {
                        i += 2;
                    } else if word.starts_with('-') {
                        i += 1;
                    } else {
                        break;
                    }
                }
                tail(&refs, i)?
            }
            "bash" | "sh" | "zsh" | "dash" => {
                let mut i = 1;
                let mut found = false;
                while let Some(word) = refs.get(i) {
                    if *word == "-c" {
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
                effective(&inner_refs)?
            }
            "git" => {
                let mut i = 1;
                while let Some(word) = refs.get(i) {
                    match *word {
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
                let mut out = vec!["git".to_owned()];
                out.extend(refs[i..].iter().map(|w| (*w).to_owned()));
                return Some(out);
            }
            "uv" if refs.get(1) == Some(&"run") => uv_run_rest(&refs[2..])?
                .iter()
                .map(|w| (*w).to_owned())
                .collect(),
            "mise" if matches!(refs.get(1), Some(&"exec" | &"x")) => {
                if refs.get(2) != Some(&"--") || !refs.get(3).is_some_and(|w| !w.starts_with('-')) {
                    return None;
                }
                tail(&refs, 3)?
            }
            "asdf" if refs.get(1) == Some(&"exec") => {
                if !refs
                    .get(2)
                    .is_some_and(|cmd| !cmd.starts_with('-') && !cmd.contains('/'))
                {
                    return None;
                }
                tail(&refs, 2)?
            }
            "direnv" if refs.get(1) == Some(&"exec") => {
                if !refs
                    .get(2)
                    .is_some_and(|dir| crate::filters::is_local_dir(dir))
                    || !refs.get(3).is_some_and(|cmd| !cmd.starts_with('-'))
                {
                    return None;
                }
                tail(&refs, 3)?
            }
            "pixi" if refs.get(1) == Some(&"run") => pixi_run_rest(&refs[2..])?
                .iter()
                .map(|w| (*w).to_owned())
                .collect(),
            _ => break,
        };
        words = rest;
    }
    Some(words)
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
}
