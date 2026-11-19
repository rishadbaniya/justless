"""Run directories: snapshot clones, warm targets, arm install, baseline commit, environment."""

import os
import re
import shutil
import subprocess
from pathlib import Path

import config

SKIP_COPY = {".git", "target", ".justless", "node_modules", "__pycache__", ".venv", "venv"}

# Pre-flight secret patterns. A hit outside the reviewed allowlist refuses paid runs.
SECRET_PATTERNS = [
    ("anthropic_key", re.compile(r"sk-ant-[A-Za-z0-9_\-]{20,}")),
    ("openai_key", re.compile(r"sk-(?:proj-)?[A-Za-z0-9]{32,}")),
    ("aws_access_key", re.compile(r"AKIA[0-9A-Z]{16}")),
    ("github_token", re.compile(r"gh[pousr]_[A-Za-z0-9]{36,}")),
    ("slack_token", re.compile(r"xox[baprs]-[A-Za-z0-9-]{10,}")),
    ("private_key", re.compile(r"-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----")),
    ("eth_private_key", re.compile(r"(?i)(?:private[_ ]?key|secret)[\"'\s:=]+(?:0x)?[0-9a-f]{64}\b")),
    ("generic_secret", re.compile(r"(?i)(?:api[_-]?key|secret|passphrase|password)\s*[:=]\s*[\"'][^\"'\s]{16,}[\"']")),
]


def git_env(root, home=None):
    name, email = config.GIT_AUTHOR
    env = {
        "PATH": config.SYSTEM_PATH, "HOME": str(home or Path(root).parent / "home"), "LANG": "en_US.UTF-8",
        "GIT_AUTHOR_NAME": name, "GIT_AUTHOR_EMAIL": email, "GIT_AUTHOR_DATE": config.GIT_DATE,
        "GIT_COMMITTER_NAME": name, "GIT_COMMITTER_EMAIL": email, "GIT_COMMITTER_DATE": config.GIT_DATE,
        "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
    }
    return env


def _git(root, args, env):
    return subprocess.run(["git"] + args, cwd=str(root), env=env, capture_output=True, text=True, check=True)


def git_baseline(root, env=None):
    """Replace any .git with one fixed-date commit of the current tree."""
    root = Path(root)
    env = env or git_env(root)
    shutil.rmtree(root / ".git", ignore_errors=True)
    _git(root, ["init", "-q", "-b", "main"], env)
    _git(root, ["add", "-A"], env)
    _git(root, ["commit", "-q", "--allow-empty", "-m", "baseline"], env)
    normalize_mtimes(root / ".git")


def git_commit_all(root, message, env=None):
    env = env or git_env(root)
    _git(root, ["add", "-A"], env)
    _git(root, ["commit", "-q", "--allow-empty", "-m", message], env)


def git_task_history(root, task, env=None):
    """The baseline commit, plus for a task whose setup is a history (a regression hidden in a cleanup commit)
    a second fixed-date commit holding that setup. The snapshot holds the final tree, so the setup edits are
    undone for the first commit and redone for the second. Every file keeps the fixed mtime."""
    import tasks as tasks_module
    root = Path(root)
    setup = task.get("setup") or {}
    history = setup.get("kind") == "history"
    if history:
        tasks_module.apply_edits(root, setup["edits"], reverse=True)
        normalize_mtimes(root)
    git_baseline(root, env=env)
    if history:
        tasks_module.apply_edits(root, setup["edits"])
        normalize_mtimes(root)
        git_commit_all(root, setup["commit_message"], env=env or git_env(root))
        normalize_mtimes(root / ".git")


def normalize_mtimes(root, stamp=None):
    stamp = config.MTIME if stamp is None else stamp
    root = Path(root)
    for dirpath, dirnames, filenames in os.walk(str(root), topdown=False):
        for name in filenames + dirnames:
            try:
                os.utime(os.path.join(dirpath, name), (stamp, stamp), follow_symlinks=False)
            except (OSError, NotImplementedError):
                pass
    os.utime(str(root), (stamp, stamp))


def newest_mtime(root, skip=(".git",)):
    newest = 0
    for dirpath, dirnames, filenames in os.walk(str(root)):
        dirnames[:] = [d for d in dirnames if d not in skip]
        for name in filenames:
            try:
                newest = max(newest, os.lstat(os.path.join(dirpath, name)).st_mtime)
            except OSError:
                pass
    return newest


def clone_tree(src, dst):
    """APFS clonefile copy (cp -cRp), falling back to a plain copy."""
    dst = Path(dst)
    dst.parent.mkdir(parents=True, exist_ok=True)
    proc = subprocess.run(["/bin/cp", "-cRp", str(src), str(dst)], capture_output=True)
    if proc.returncode == 0:
        return "clone"
    shutil.rmtree(dst, ignore_errors=True)
    subprocess.run(["/bin/cp", "-Rp", str(src), str(dst)], check=True, capture_output=True)
    return "copy"


def copy_source(src, dst):
    """Copy a corpus repo without .git, build output or caches. The corpus is only read."""
    src = Path(src)
    dst = Path(dst)
    if dst.exists():
        shutil.rmtree(dst)

    def ignore(directory, names):
        return [n for n in names if n in SKIP_COPY]

    shutil.copytree(str(src), str(dst), symlinks=True, ignore=ignore)


def tree_hash(root, skip=(".git", "target", ".justless")):
    """sha256 over sorted (relative path, mode bit, content) for every file."""
    import hashlib
    digest = hashlib.sha256()
    root = Path(root)
    for dirpath, dirnames, filenames in os.walk(str(root)):
        dirnames[:] = sorted(d for d in dirnames if d not in skip)
        for name in sorted(filenames):
            path = Path(dirpath) / name
            rel = path.relative_to(root).as_posix()
            digest.update(rel.encode() + b"\0")
            try:
                if path.is_symlink():
                    digest.update(b"L" + os.readlink(str(path)).encode())
                else:
                    with open(str(path), "rb") as handle:
                        digest.update(b"F" + handle.read())
            except OSError:
                digest.update(b"E")
            digest.update(b"\0")
    return digest.hexdigest()


def tree_files(root, skip=(".git", "target", ".justless")):
    """Map relative path -> bytes for every regular file."""
    out = {}
    root = Path(root)
    for dirpath, dirnames, filenames in os.walk(str(root)):
        dirnames[:] = [d for d in dirnames if d not in skip]
        for name in filenames:
            path = Path(dirpath) / name
            if path.is_symlink() or not path.is_file():
                continue
            try:
                out[path.relative_to(root).as_posix()] = path.read_bytes()
            except OSError:
                pass
    return out


def make_toolbin(bindir, justless_binary=None):
    """A PATH directory of symlinks to the pinned tool directories, minus agent and Justless binaries."""
    bindir = Path(bindir)
    bindir.mkdir(parents=True, exist_ok=True)
    for directory in config.TOOL_DIRS:
        if not directory.is_dir():
            continue
        for entry in sorted(os.listdir(str(directory))):
            if entry.lower().startswith(config.TOOL_DENY_PREFIXES):
                continue
            target = bindir / entry
            if not os.path.lexists(str(target)):
                os.symlink(str(directory / entry), str(target))
    if justless_binary:
        install_binary(justless_binary, bindir / "justless")
    return bindir


def install_binary(binary, dest):
    dest = Path(dest)
    dest.parent.mkdir(parents=True, exist_ok=True)
    if dest.exists():
        dest.unlink()
    proc = subprocess.run(["/bin/cp", "-c", str(binary), str(dest)], capture_output=True)
    if proc.returncode != 0:
        shutil.copy2(str(binary), str(dest))
    os.chmod(str(dest), 0o755)
    return dest


def tool_env(root, home, bindir, target, justless_dir=None, tmp=None):
    """The allowlisted environment for tools, hooks and oracles. No host variable passes through."""
    path = ([str(justless_dir)] if justless_dir else []) + [str(bindir), config.SYSTEM_PATH]
    tmp = Path(tmp or Path(home).parent / "tmp")
    tmp.mkdir(parents=True, exist_ok=True)
    Path(home).mkdir(parents=True, exist_ok=True)
    dead = "http://127.0.0.1:9"
    return {
        "PATH": ":".join(path),
        "HOME": str(home),
        "TMPDIR": str(tmp) + "/",
        "LANG": "en_US.UTF-8",
        "TERM": "dumb",
        "SHELL": "/bin/bash",
        "USER": "bench",
        "CARGO_HOME": str(config.CARGO_HOME),
        "RUSTUP_HOME": str(config.RUSTUP_HOME),
        "CARGO_TARGET_DIR": str(target),
        "CARGO_NET_OFFLINE": "true",
        "CARGO_TERM_COLOR": "never",
        "http_proxy": dead, "https_proxy": dead, "HTTP_PROXY": dead, "HTTPS_PROXY": dead,
        "ALL_PROXY": dead, "all_proxy": dead, "NO_PROXY": "", "no_proxy": "",
        "GIT_CONFIG_NOSYSTEM": "1",
        "CLAUDE_PROJECT_DIR": str(root),
        "CLAUDECODE": "1",
        "PYTHONDONTWRITEBYTECODE": "1",
    }


def local_packages(root):
    """Package names declared by Cargo.toml files inside the repository."""
    names = set()
    for dirpath, dirnames, filenames in os.walk(str(root)):
        dirnames[:] = [d for d in dirnames if d not in SKIP_COPY]
        if "Cargo.toml" in filenames:
            text = (Path(dirpath) / "Cargo.toml").read_text(errors="replace")
            m = re.search(r"\[package\][^\[]*?\bname\s*=\s*\"([^\"]+)\"", text, re.S)
            if m:
                names.add(m.group(1))
    return names


def invalidate_local_fingerprints(root, target):
    """Make Cargo rebuild the repository's own crates in a cloned warm target.

    Crates compiled in the snapshot directory embed its absolute path (env!("CARGO_MANIFEST_DIR"),
    file!()), and Cargo's fingerprints for path packages do not include that path, so a cloned target
    would run test binaries that read fixtures from the snapshot. Dependencies stay warm."""
    names = local_packages(root)
    removed = 0
    for fingerprint in Path(target).glob("*/.fingerprint"):
        for entry in fingerprint.iterdir():
            package = entry.name.rsplit("-", 1)[0]
            if package in names or package.replace("_", "-") in names or package.replace("-", "_") in names:
                shutil.rmtree(str(entry), ignore_errors=True)
                removed += 1
    return removed


def install_arm(root, arm, justless, env):
    """Install Justless in a Justless arm. Returns the list of changed paths."""
    if not config.ARMS[arm]["justless"]:
        return []
    before = tree_files(root)
    subprocess.run([str(justless), "init", "--hooks", "--agent", "claude"], cwd=str(root), env=env,
                   capture_output=True, check=True)
    after = tree_files(root)
    return sorted(p for p in set(before) | set(after) if before.get(p) != after.get(p))


def secret_scan(root, allowlist=()):
    """Return [(path, line, kind)] for secret-looking strings outside the allowlist."""
    hits = []
    allowed = set(allowlist)
    for rel, data in sorted(tree_files(root).items()):
        if b"\0" in data[:4096]:
            continue
        text = data.decode("utf-8", "replace")
        for lineno, line in enumerate(text.splitlines(), 1):
            for kind, pattern in SECRET_PATTERNS:
                if pattern.search(line):
                    key = "%s:%d:%s" % (rel, lineno, kind)
                    if key not in allowed:
                        hits.append((rel, lineno, kind))
    return hits


def allowed_secret_texts(root, allowlist=()):
    """The matched strings on reviewed allowlist lines, which the in-flight filter then ignores."""
    allowed = set(allowlist)
    out = set()
    files = tree_files(root)
    for entry in allowed:
        rel, _, rest = entry.partition(":")
        lineno, _, kind = rest.partition(":")
        data = files.get(rel)
        if data is None or not lineno.isdigit():
            continue
        lines = data.decode("utf-8", "replace").splitlines()
        if int(lineno) - 1 >= len(lines):
            continue
        for name, pattern in SECRET_PATTERNS:
            if name == kind:
                out.update(m.group(0) for m in pattern.finditer(lines[int(lineno) - 1]))
    return out


def remove(path):
    shutil.rmtree(str(path), ignore_errors=True)
