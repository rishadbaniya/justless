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
                while refs
                    .get(i)
                    .is_some_and(|w| w.starts_with('-') && *w != "--")
                {
                    i += 1;
                }
                if refs.get(i) == Some(&"--") {
                    i += 1;
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
}
