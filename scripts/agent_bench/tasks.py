"""Task schema, loading, mutation application and the frozen manifest."""

import hashlib
import json
import random
from pathlib import Path

import config

FAMILIES = ["bugfix", "lint", "location", "impact"]
REQUIRED = ["id", "family", "repo", "language", "prompt", "setup", "oracle"]


class TaskError(Exception):
    pass


def validate(task):
    missing = [k for k in REQUIRED if k not in task]
    if missing:
        raise TaskError("%s: missing %s" % (task.get("id"), ", ".join(missing)))
    if task["family"] not in FAMILIES:
        raise TaskError("%s: unknown family %s" % (task["id"], task["family"]))
    if not task["id"].startswith(task["family"] + "-"):
        raise TaskError("%s: id must start with the family" % task["id"])
    return task


def load_public():
    with open(config.PUBLIC_TASKS) as handle:
        data = json.load(handle)
    return [validate(t) for t in data["tasks"]]


def private_dir(home):
    return Path(home) / "tasks"


def load_private(home):
    directory = private_dir(home)
    if not directory.is_dir():
        return []
    out = []
    for path in sorted(directory.glob("*.json")):
        if path.name.startswith("_"):
            continue
        with open(path) as handle:
            out.append(validate(json.load(handle)))
    return out


def save_private(home, task):
    directory = private_dir(home)
    directory.mkdir(parents=True, exist_ok=True)
    with open(directory / ("%s.json" % task["id"]), "w") as handle:
        json.dump(task, handle, indent=1, sort_keys=True)


def manifest_hash(tasks):
    digest = hashlib.sha256()
    for task in sorted(tasks, key=lambda t: t["id"]):
        frozen = {k: v for k, v in task.items() if k not in ("review", "generator_log")}
        digest.update(json.dumps(frozen, sort_keys=True, separators=(",", ":")).encode())
    return digest.hexdigest()[:16]


def ordered(tasks, seed=config.SEED):
    """The fixed model-subset order: sorted IDs shuffled with the global seed."""
    ids = sorted(t["id"] for t in tasks)
    random.Random(seed).shuffle(ids)
    index = {tid: i for i, tid in enumerate(ids)}
    return sorted(tasks, key=lambda t: index[t["id"]])


def first_per_family(tasks, k):
    out = []
    counts = {}
    for task in ordered(tasks):
        family = task["family"]
        if counts.get(family, 0) < k:
            out.append(task)
            counts[family] = counts.get(family, 0) + 1
    return out


def apply_edits(root, edits, reverse=False):
    """Apply [{file, offset, old, new}] edits; verifies the old text at each offset."""
    by_file = {}
    for edit in edits:
        by_file.setdefault(edit["file"], []).append(edit)
    for rel, file_edits in by_file.items():
        path = Path(root) / rel
        data = path.read_text()
        for edit in sorted(file_edits, key=lambda e: e["offset"], reverse=True):
            old, new = (edit["new"], edit["old"]) if reverse else (edit["old"], edit["new"])
            offset = edit["offset"] if not reverse else edit.get("new_offset", edit["offset"])
            if data[offset:offset + len(old)] != old:
                found = data.find(old)
                if found < 0 or data.count(old) != 1:
                    raise TaskError("edit for %s at %d does not match" % (rel, offset))
                offset = found
            data = data[:offset] + new + data[offset + len(old):]
        path.write_text(data)


def with_new_offsets(edits):
    """Record where each replacement lands after earlier edits in the same file shift it."""
    out = []
    by_file = {}
    for edit in edits:
        by_file.setdefault(edit["file"], []).append(dict(edit))
    for rel, file_edits in by_file.items():
        shift = 0
        for edit in sorted(file_edits, key=lambda e: e["offset"]):
            edit["new_offset"] = edit["offset"] + shift
            shift += len(edit["new"]) - len(edit["old"])
            out.append(edit)
    return out
