"""Fixed configuration for the agent benchmark. This is the only place arms differ."""

import hashlib
import json
import os
import random
from pathlib import Path

BENCH_DIR = Path(__file__).resolve().parent
REPO = BENCH_DIR.parents[1]
DATA = BENCH_DIR / "data"
PROMPTS = BENCH_DIR / "prompts"
PUBLIC_TASKS = BENCH_DIR / "tasks" / "public.json"

# Default locations, in a private folder outside the repository. Each can be overridden
# by environment variable or command-line flag. The store holds private tasks, truth,
# transcripts and the Claude Code capture; it is never committed.
SCRATCH = Path(os.environ.get(
    "AGENT_BENCH_SCRATCH",
    str(Path(os.path.expanduser("~")) / ".local" / "share" / "justless-bench")))
HOME = Path(os.environ.get("AGENT_BENCH_HOME", str(SCRATCH / "store")))
CORPUS = Path(os.environ.get("AGENT_BENCH_CORPUS", str(SCRATCH / "corpus")))
RUNROOT = Path(os.environ.get("AGENT_BENCH_RUNROOT", str(SCRATCH / "runs")))
USER_HOME = Path(os.path.expanduser("~"))
CARGO_HOME = Path(os.environ.get("CARGO_HOME", str(USER_HOME / ".cargo")))
RUSTUP_HOME = Path(os.environ.get("RUSTUP_HOME", str(USER_HOME / ".rustup")))
CC_BINARY = os.environ.get("AGENT_BENCH_CC_BINARY", str(USER_HOME / ".local/share/claude/versions/2.1.327"))
CC_VERSION = "2.1.327"

# The design pins PATH to ~/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin.
# ~/.cargo/bin holds an installed justless and /opt/homebrew/bin holds claude and codex,
# so each run gets a bin directory of symlinks to those two directories minus these names.
TOOL_DIRS = [CARGO_HOME / "bin", Path("/opt/homebrew/bin")]
TOOL_DENY_PREFIXES = ("justless", "claude", "codex", "aider", "gemini", "cursor", "opencode", "copilot")
SYSTEM_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"

SEED = 20261118
FIXED_DATE = "2026-11-18"
MTIME = 1767225600  # 2026-01-01T00:00:00Z
GIT_DATE = "2026-01-01T00:00:00Z"
GIT_AUTHOR = ("Bench", "bench@example.invalid")

API_BASE = "https://api.anthropic.com"
ANTHROPIC_VERSION = "2026-10-21"
BETAS = ["context-management-2025-06-27", "thinking-binding-controls-2026-08-01"]
MAX_OUTPUT_CAP = 32000

MODELS = {
    "claude-haiku-5-5": {"short": "haiku", "run_budget": 0.30, "api_default_effort": "medium"},
    "claude-sonnet-5-5": {"short": "sonnet", "run_budget": 3.00, "api_default_effort": "high"},
    "claude-opus-5-5": {"short": "opus", "run_budget": 6.00, "api_default_effort": "medium"},
}
EFFORTS = ["cc-default", "low", "medium", "high", "xhigh", "max"]

ARMS = {
    "a": {"name": "plain", "justless": False, "ce": False},
    "b": {"name": "justless", "justless": True, "ce": False},
    "c": {"name": "plain+ce", "justless": False, "ce": True},
    "d": {"name": "justless+ce", "justless": True, "ce": True},
}

# Runs stopped by a limit are still graded on their final state (run.py records stopped_by). The token limit
# is a placeholder until the pilot sets it (estimate prints the rule); the wall timeout is a hang guard and
# excludes API retry back-off.
LIMITS = {"turn_limit": 60, "token_limit": 3_000_000, "wall_timeout": 7200}
TOKEN_LIMIT_FLOOR = 3_000_000


def token_limit_rule(plain_cumulative_prompts):
    """At least 3x the plain arm's 95th-percentile cumulative prompt tokens per run, never below the floor, so
    the cap binds in about 1% of runs or fewer instead of turning higher usage into failures."""
    values = sorted(plain_cumulative_prompts)
    if not values:
        return TOKEN_LIMIT_FLOOR
    k = (len(values) - 1) * 0.95
    lo, hi = int(k), min(int(k) + 1, len(values) - 1)
    p95 = values[lo] + (values[hi] - values[lo]) * (k - lo)
    return max(TOKEN_LIMIT_FLOOR, int(-(-3 * p95 // 100000) * 100000))
CAMPAIGN_CAP_USD = 90.0
RESERVE_USD = 10.0
MAX_ATTEMPTS = 3
HOOK_TIMEOUT_DEFAULT = 600

# Context editing: the pilot validates shapes with 30,000; the main run uses the rule.
CE_PILOT_TRIGGER = 30000
CE_KEEP = 3
CE_CLEAR_AT_LEAST = 5000


def ce_trigger_rule(peak_prompts):
    """Median of plain-arm per-run peak prompt tokens, rounded up to 5,000, clamped to [30K, 100K]."""
    values = sorted(peak_prompts)
    if not values:
        return CE_PILOT_TRIGGER
    n = len(values)
    median = values[n // 2] if n % 2 else (values[n // 2 - 1] + values[n // 2]) / 2
    rounded = int(-(-median // 5000) * 5000)
    return max(30000, min(100000, rounded))


def context_management(arm, trigger):
    """The server-side context-management field for an arm, or None."""
    if not ARMS[arm]["ce"]:
        return None
    return {"edits": [{
        "type": "clear_tool_uses_20250919",
        "trigger": {"type": "input_tokens", "value": int(trigger)},
        "keep": {"type": "tool_uses", "value": CE_KEEP},
        "clear_at_least": {"type": "input_tokens", "value": CE_CLEAR_AT_LEAST},
    }]}


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_json(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def run_id(campaign, model, effort, arm, task_id, rep, manifest_hash, justless_sha, harness_hash, capture_hash):
    """harness_hash is the content hash of the harness tree (harness_tree_hash), so commits elsewhere in the
    repository do not re-key finished runs."""
    key = "|".join(str(x) for x in (campaign, model, effort, arm, task_id, rep, manifest_hash,
                                     justless_sha, harness_hash, capture_hash))
    return hashlib.sha256(key.encode()).hexdigest()[:16]


# A random salt per process: an attempt that crashed after billing requests has no result row, so its resume
# reuses the attempt number. The salt keeps that resume from reading the crashed attempt's cached prefix.
PROCESS_SALT = os.urandom(8).hex()


def nonce(run, attempt, salt=None):
    salt = PROCESS_SALT if salt is None else salt
    return hashlib.sha256(("%s|%d|%s" % (run, attempt, salt)).encode()).hexdigest()[:16]


HARNESS_SKIP_DIRS = {"__pycache__", "tests"}


def harness_tree_hash(root=BENCH_DIR):
    """Content hash of every file the harness runs with (code, data, prompts, public tasks), without Git.
    Tests and caches are excluded; any other edit, committed or not, changes the hash."""
    digest = hashlib.sha256()
    root = Path(root)
    for dirpath, dirnames, filenames in os.walk(str(root)):
        dirnames[:] = sorted(d for d in dirnames if d not in HARNESS_SKIP_DIRS and not d.startswith("."))
        for name in sorted(filenames):
            if name.endswith((".pyc", ".pyo")) or name.startswith("."):
                continue
            path = Path(dirpath) / name
            digest.update(str(path.relative_to(root)).encode() + b"\0")
            digest.update(path.read_bytes() + b"\0")
    return digest.hexdigest()[:16]


def arm_order(model, task_id, rep, arms, seed=SEED):
    """A seeded permutation of arms for one (model, task, rep) block."""
    seed = int(hashlib.sha256(("%d|%s|%s|%d" % (seed, model, task_id, rep)).encode()).hexdigest(), 16)
    order = sorted(arms)
    random.Random(seed).shuffle(order)
    return order


def pricing():
    with open(DATA / "pricing.json") as handle:
        return json.load(handle)


def parity():
    with open(DATA / "cc_parity.json") as handle:
        return json.load(handle)
