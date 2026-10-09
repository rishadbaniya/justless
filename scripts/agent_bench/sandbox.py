"""Per-run macOS sandbox-exec profiles.

Every tool call, hook and oracle runs under a profile that

- denies reads and writes of the task store, the run root, the corpus, the scratch
  directory, the user's home (except the Rust toolchain) and other runs,
- re-allows the run's own directory,
- denies writes outside the run directory, the Cargo home and /dev,
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


def profile(run_dir, deny_read=(), allow_read=(), allow_write=(), allow_exec=(), network=False):
    """Build an SBPL profile string for one run."""
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
    writable = [run_dir, "/dev"] + [_real(p) for p in allow_write]
    lines.append("(deny file-write*)")
    lines.append("(allow file-write* %s)" % " ".join("(subpath %s)" % _quote(p) for p in writable))
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
    text = profile(mine, deny_read=deny, allow_read=default_allows(config), allow_write=[config.CARGO_HOME])
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
    shutil.rmtree(workdir, ignore_errors=True)
    return results
