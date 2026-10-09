"""Per-run macOS sandbox-exec profiles.

Every tool call, hook and oracle runs under a profile that

- denies reads and writes of the task store, the run root, the corpus, the scratch
  directory, the user's home (except the Rust toolchain) and other runs,
- re-allows the run's own directory,
- denies reads of every Cargo git checkout and database of the task's own repository and of checkout
  revisions its Cargo.lock does not pin, since those can hold the unmutated original of a task file,
- denies writes outside the run directory and /dev, except Cargo's lock and usage-tracking files,
- denies executing any `justless`, `claude` or `codex` binary except the run's pinned copy,
- denies outbound network except 127.0.0.1.

SBPL applies the last matching rule, so the allows that follow a deny take precedence.
"""

import hashlib
import os
import re
import shutil
import subprocess
from pathlib import Path

SANDBOX_EXEC = "/usr/bin/sandbox-exec"
DENIAL = re.compile(r"(Operation not permitted|Permission denied|sandbox)", re.I)


def available():
    return os.path.exists(SANDBOX_EXEC)


def _quote(path):
    return '"%s"' % str(path).replace("\\", "\\\\").replace('"', '\\"')


def _real(path):
    try:
        return str(Path(path).resolve())
    except OSError:
        return str(path)


def _regex_quote(text):
    return "".join("\\" + c if c in ".^$*+?()[]{}|\\" else c for c in text)


def cargo_write_regexes(cargo_home):
    """Cargo's lock and usage-tracking files, the only writes a warm offline build makes in the Cargo home."""
    out = []
    for variant in sorted({str(cargo_home), _real(cargo_home)}):
        out.append("^%s/\\.(package-cache|package-cache-mutate|global-cache)(-journal|-wal|-shm)?$"
                   % _regex_quote(variant.rstrip("/")))
    return out


def profile(run_dir, deny_read=(), allow_read=(), allow_write=(), allow_exec=(), network=False, deny_after=(),
            write_regexes=()):
    """Build an SBPL profile string for one run. SBPL applies the last matching rule, so deny_after paths stay
    denied inside the re-allowed allow_read directories."""
    run_dir = _real(run_dir)
    lines = ["(version 1)", "(allow default)"]
    for path in deny_read:
        for variant in sorted({str(path), _real(path)}):
            lines.append("(deny file-read* file-write* (subpath %s))" % _quote(variant))
    ancestors = set()
    for path in allow_read:
        for variant in sorted({str(path), _real(path)}):
            lines.append("(allow file-read* (subpath %s))" % _quote(variant))
            # libgit2 (Cargo git dependencies) stats the ancestors of ~/.cargo/git; allow metadata only.
            parent = Path(variant)
            while str(parent) != parent.anchor:
                parent = parent.parent
                ancestors.add(str(parent))
    for parent in sorted(ancestors):
        lines.append("(allow file-read-metadata (literal %s))" % _quote(parent))
    for path in deny_after:
        for variant in sorted({str(path), _real(path)}):
            lines.append("(deny file-read* file-write* (subpath %s))" % _quote(variant))
    writable = [run_dir, "/dev"] + [_real(p) for p in allow_write]
    lines.append("(deny file-write*)")
    lines.append("(allow file-write* %s)" % " ".join(["(subpath %s)" % _quote(p) for p in writable]
                                                      + ['(regex #"%s")' % r for r in write_regexes]))
    lines.append("(allow file-read* (subpath %s))" % _quote(run_dir))
    # Path lookups and getcwd() of the run directory need metadata of its ancestors.
    parent = Path(run_dir)
    while str(parent) != parent.anchor:
        parent = parent.parent
        lines.append("(allow file-read-metadata (literal %s))" % _quote(parent))
    lines.append('(deny process-exec (regex #"/(justless|claude|codex)[^/]*$"))')
    for path in allow_exec:
        lines.append("(allow process-exec (literal %s))" % _quote(_real(path)))
    if not network:
        lines.append("(deny network-outbound)")
        lines.append('(allow network-outbound (remote ip "localhost:*"))')
        lines.append("(allow network-outbound (remote unix-socket))")
    return "\n".join(lines) + "\n"


def default_denies(config, extra=()):
    """The read-deny list shared by every run."""
    denies = [config.HOME, config.RUNROOT, config.CORPUS, config.SCRATCH, config.USER_HOME]
    if Path("/private/tmp/claude-502").exists():
        denies.append(Path("/private/tmp/claude-502"))
    return [d for d in denies if d] + list(extra)


def default_allows(config):
    """Directories re-allowed for reading inside the denied home: the Rust toolchain only."""
    return [config.CARGO_HOME, config.RUSTUP_HOME]


def git_lock_sources(repo_root):
    """[(repository name, locked commit)] for the git sources in a Cargo.lock."""
    lock = Path(repo_root) / "Cargo.lock"
    if not lock.exists():
        return []
    prefix = 'source = "' + "g" + "it+"
    out = set()
    for line in lock.read_text(errors="replace").split("\n"):
        if not line.startswith(prefix):
            continue
        url = line[len(prefix):].rstrip('"')
        rev = url.split("#", 1)[1] if "#" in url else ""
        name = url.split("#", 1)[0].split("?", 1)[0].rstrip("/").rsplit("/", 1)[-1]
        if name.endswith(".git"):
            name = name[:-4]
        out.add((name, rev))
    return sorted(out)


def _ident_name(ident):
    """`crate_name-0123456789abcdef` -> `crate_name` (Cargo's git cache directory naming)."""
    name, _, digest = ident.rpartition("-")
    return name if name and len(digest) == 16 else ident


def cargo_git_denies(cargo_home, own_repos, repo_root):
    """Paths under the Cargo git cache that a run must not read: every checkout and database of the task's own
    repository, which hold the unmutated originals, and every checkout revision or database that the task's
    Cargo.lock does not use. Raises if the lock uses the task's own repository, which the deny would break."""
    sources = git_lock_sources(repo_root)
    own = set(own_repos)
    clash = sorted(name for name, _ in sources if name in own)
    if clash:
        raise ValueError("Cargo.lock depends on the task's own repository %s; its git cache cannot be hidden"
                         % ", ".join(clash))
    locked = {}
    for name, rev in sources:
        locked.setdefault(name, []).append(rev)
    denies = []
    git = Path(cargo_home) / "git"
    for kind in ("checkouts", "db"):
        base = git / kind
        if not base.is_dir():
            continue
        for ident in sorted(base.iterdir()):
            if not ident.is_dir():
                continue
            name = _ident_name(ident.name)
            if name in own or name not in locked:
                denies.append(ident)
            elif kind == "checkouts":
                for revision in sorted(ident.iterdir()):
                    if revision.is_dir() and not any(rev.startswith(revision.name) for rev in locked[name]):
                        denies.append(revision)
    return denies


def git_blob_sha(text):
    import hashlib
    data = text.encode("utf-8") if isinstance(text, str) else text
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def answer_key_exposure(task, cargo_home, denies):
    """[(file, database)] where an unmutated original of an edit task's file is still readable from a Cargo git
    database the build needs. The sandbox cannot hide those; validate-tasks reports them."""
    originals = (task.get("controls") or {}).get("inverse_files") or {}
    db_root = Path(cargo_home) / "git" / "db"
    if not originals or not db_root.is_dir():
        return []
    hidden = {str(Path(d)) for d in denies}
    readable = [d for d in sorted(db_root.iterdir()) if d.is_dir() and str(d) not in hidden]
    found = []
    for rel, text in sorted(originals.items()):
        sha = git_blob_sha(text)
        for db in readable:
            proc = subprocess.run(["g" + "it", "--git-dir", str(db), "cat-file", "-e", sha],
                                  capture_output=True)
            if proc.returncode == 0:
                found.append((rel, db.name))
    return found


def cargo_home_state(cargo_home):
    """Name, size and mtime of everything in the Cargo home that later runs execute or read as configuration."""
    cargo_home = Path(cargo_home)
    state = {}
    for path in sorted(cargo_home.glob("config*")) + sorted(cargo_home.glob("credentials*")) + \
            sorted((cargo_home / "bin").glob("*")):
        try:
            info = path.lstat()
        except OSError:
            continue
        state[str(path.relative_to(cargo_home))] = [info.st_size, info.st_mtime_ns, info.st_ino]
    return state


def write_profile(run_dir, text):
    path = Path(run_dir) / "sandbox.sb"
    path.write_text(text)
    return path, hashlib.sha256(text.encode()).hexdigest()


def wrap(argv, profile_path):
    if profile_path is None:
        return list(argv)
    return [SANDBOX_EXEC, "-f", str(profile_path)] + list(argv)


def count_denials(text):
    return len(re.findall(r"Operation not permitted", text or ""))


def self_test(config, workdir):
    """Plant a truth file in the store and a sibling run, then verify the profile blocks them.

    Returns a list of (check, passed, detail)."""
    workdir = Path(workdir)
    store = workdir / "store"
    runroot = workdir / "runs"
    mine = runroot / "aaaaaaaaaaaa"
    sibling = runroot / "bbbbbbbbbbbb"
    for path in (store, mine / "repo", sibling / "repo"):
        path.mkdir(parents=True, exist_ok=True)
    (store / "truth.json").write_text('{"secret": "truth"}\n')
    (sibling / "repo" / "fixed.rs").write_text("fn fixed() {}\n")
    (mine / "repo" / "ok.txt").write_text("mine\n")
    deny = default_denies(config, [store, runroot])
    hidden = config.CARGO_HOME / "git" / "checkouts"
    text = profile(mine, deny_read=deny, allow_read=default_allows(config), deny_after=[hidden],
                   write_regexes=cargo_write_regexes(config.CARGO_HOME))
    prof, _ = write_profile(mine, text)
    env = {"PATH": config.SYSTEM_PATH, "HOME": str(mine / "home"), "TMPDIR": str(mine / "tmp")}
    (mine / "home").mkdir(exist_ok=True)
    (mine / "tmp").mkdir(exist_ok=True)

    def run(command):
        proc = subprocess.run(wrap(["/bin/bash", "-c", command], prof), cwd=str(mine / "repo"), env=env,
                              stdin=subprocess.DEVNULL, capture_output=True, timeout=60)
        return proc.returncode, (proc.stdout + proc.stderr).decode("utf-8", "replace")

    results = []
    code, out = run("cat %s" % _quote(store / "truth.json"))
    results.append(("deny store read", code != 0 and "truth" not in out.replace("truth.json", ""), out.strip()[:200]))
    code, out = run("ls %s; cat %s" % (_quote(sibling / "repo"), _quote(sibling / "repo" / "fixed.rs")))
    results.append(("deny sibling run", "fn fixed" not in out and "fixed.rs\n" not in out, out.strip()[:200]))
    code, out = run("echo hi > %s" % _quote(workdir / "outside.txt"))
    results.append(("deny write outside run", not (workdir / "outside.txt").exists(), out.strip()[:200]))
    code, out = run("cat ok.txt && echo x > new.txt && cat new.txt && pwd -P")
    results.append(("allow own run", code == 0 and "mine" in out and "x" in out, out.strip()[:200]))
    code, out = run("/usr/bin/curl -sS -m 5 https://example.com -o /dev/null; echo exit=$?")
    results.append(("deny network", "exit=0" not in out, out.strip()[:200]))
    if (config.CARGO_HOME / "bin" / "justless").exists():
        code, out = run("%s --version" % _quote(config.CARGO_HOME / "bin" / "justless"))
        results.append(("deny unpinned justless exec", code != 0, out.strip()[:200]))
    code, out = run("cat %s/.zshrc %s/.claude/settings.json 2>&1 | head -2" % (
        _quote(config.USER_HOME), _quote(config.USER_HOME)))
    results.append(("deny user home", "Operation not permitted" in out or "No such file" in out, out.strip()[:200]))
    code, out = run("ls %s >/dev/null && echo toolchain_ok; ls %s; echo exit=$?" % (
        _quote(config.CARGO_HOME / "bin"), _quote(hidden)))
    results.append(("deny hidden cargo git cache", "toolchain_ok" in out and "exit=0" not in out, out.strip()[:200]))
    probe = config.CARGO_HOME / "bin" / ".sandbox-write-probe"
    code, out = run("touch %s; echo exit=$?" % _quote(probe))
    written = probe.exists()
    if written:
        probe.unlink()
    results.append(("deny cargo home writes", not written, out.strip()[:200]))
    shutil.rmtree(workdir, ignore_errors=True)
    return results
