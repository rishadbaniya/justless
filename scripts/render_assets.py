#!/usr/bin/env python3
"""Generate all 8 Justless dark/light SVG pairs and looping demo-dark.gif.

All terminal outputs, byte counts, ratios, and replays are captured live from
the target `justless` binary in an isolated temporary workspace (never touching
the repository's own `.justless/` directory) plus the structured JSON report
emitted by `scripts/suite_benchmark.py`.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import math
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from typing import Any
from xml.sax.saxutils import escape as _xml_escape


REPO_ROOT = Path(__file__).resolve().parents[1]
RESVG_BIN = Path("/usr/bin/resvg")
FFMPEG_BIN = Path("/usr/bin/ffmpeg")

SANS_FONT = "-apple-system, BlinkMacSystemFont, 'Segoe UI', 'Liberation Sans', 'DejaVu Sans', Helvetica, Arial, sans-serif"
MONO_FONT = "ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, 'Liberation Mono', 'DejaVu Sans Mono', monospace"

RUN_ID_RE = re.compile(r"\[justless (run-[A-Za-z0-9]+)")
RANGE_RE = re.compile(r"\[(stdout|stderr):(\d+)(?:-(\d+))?\]")

THEMES: dict[str, dict[str, str]] = {
    "dark": {
        "canvas_bg": "#0d1117",
        "card_bg": "#161b22",
        "titlebar_bg": "#1c2129",
        "recessed_bg": "#0d1117",
        "border": "#30363d",
        "axis_zero": "#484f58",
        "grid": "#21262d",
        "dot": "#3d444d",
        "dot_demo": "#30363d",
        "fg": "#e6edf3",
        "muted": "#9198a1",
        "raw_bar": "#6e7681",
        "accent": "#9b7bff",
        "accent_bar": "#9977ff",
        "pill_bg": "#2a2150",
        "replay_bg": "#1f1a36",
    },
    "light": {
        "canvas_bg": "#ffffff",
        "card_bg": "#f6f8fa",
        "titlebar_bg": "#eef1f4",
        "recessed_bg": "#ffffff",
        "border": "#d0d7de",
        "axis_zero": "#afb8c1",
        "grid": "#eaeef2",
        "dot": "#d0d7de",
        "dot_demo": "#d0d7de",
        "fg": "#1f2328",
        "muted": "#59636e",
        "raw_bar": "#8c959f",
        "accent": "#7c4dff",
        "accent_bar": "#7c4dff",
        "pill_bg": "#ede7ff",
        "replay_bg": "#f1ecff",
    },
}


def esc(text: str) -> str:
    return _xml_escape(text, {'"': "&quot;"})


def clean_env(home: Path) -> dict[str, str]:
    env = dict(os.environ)
    orig_home = Path(env.get("HOME", str(Path.home())))
    env.setdefault("RUSTUP_HOME", str(orig_home / ".rustup"))
    env.setdefault("CARGO_HOME", str(orig_home / ".cargo"))
    for key in (
        "JUSTLESS_DISABLE",
        "JUSTLESS_SESSION",
        "CLAUDE_SESSION_ID",
        "JUSTLESS_MAX_BYTES",
    ):
        env.pop(key, None)
    env["HOME"] = str(home)
    env["XDG_STATE_HOME"] = str(home / ".local/state")
    env["CLAUDE_CONFIG_DIR"] = str(home / ".claude")
    env["CODEX_HOME"] = str(home / ".codex")
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    env["LC_ALL"] = "C"
    return env


def run_cmd(argv: list[str], cwd: Path, env: dict[str, str]) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        argv,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


def fmt_kb(n: int) -> str:
    if n >= 1024 * 100:
        return f"{n / 1024:,.1f} KB"
    if n >= 1024:
        return f"{n / 1024:.1f} KB"
    return f"{n:,} B"


def fmt_kb_header(n: int) -> str:
    if n >= 10240:
        return f"{n / 1024:,.1f} KB"
    if n >= 1024:
        return f"{n:,} B"
    return f"{n} B"


def fmt_ratio(raw: int, compact: int) -> str:
    ratio = raw / max(compact, 1)
    if ratio >= 10.0:
        return f"{ratio:.0f}×"
    return f"{ratio:.1f}×"


def wrap_mono(line: str, max_cols: int, cont_indent: int = 2) -> list[tuple[int, str]]:
    """Wrap a single monospace line into (indent_cols, text) segments.

    Guarantees indent_cols + len(text) <= max_cols for every returned row.
    """
    text = line.rstrip("\r\n")
    if not text:
        return [(0, "")]
    if len(text) <= max_cols:
        return [(0, text)]

    lead_len = len(text) - len(text.lstrip(" "))
    cont_cols = min(lead_len + cont_indent, max(2, max_cols // 4))
    out: list[tuple[int, str]] = []
    remaining = text
    first = True

    while remaining:
        indent = 0 if first else cont_cols
        avail = max(1, max_cols - indent)
        if len(remaining) <= avail:
            out.append((indent, remaining))
            break
        # Prefer splitting after a space within [avail // 2, avail].
        split_at = remaining.rfind(" ", max(1, avail // 2), avail + 1)
        if split_at > 0:
            chunk = remaining[: split_at + 1]
            rest = remaining[split_at + 1 :]
        else:
            chunk = remaining[:avail]
            rest = remaining[avail:]
        out.append((indent, chunk))
        remaining = rest.lstrip(" ") if first and lead_len == 0 else rest
        first = False

    return out


def elision_divider(
    x_left: float,
    x_right: float,
    y_center: float,
    label: str,
    t: dict[str, str],
    char_w: float = 7.2,
) -> list[str]:
    pill_w = len(label) * char_w + 26.0
    cx = (x_left + x_right) / 2.0
    px = cx - pill_w / 2.0
    return [
        f'<line x1="{x_left:.1f}" y1="{y_center:.1f}" x2="{px:.1f}" y2="{y_center:.1f}" stroke="{t["border"]}" stroke-dasharray="4 4"/>',
        f'<line x1="{px + pill_w:.1f}" y1="{y_center:.1f}" x2="{x_right:.1f}" y2="{y_center:.1f}" stroke="{t["border"]}" stroke-dasharray="4 4"/>',
        f'<rect x="{px:.1f}" y="{y_center - 11:.1f}" width="{pill_w:.1f}" height="22" rx="11" fill="{t["pill_bg"]}"/>',
        f'<text x="{cx:.1f}" y="{y_center + 4.5:.1f}" class="m" text-anchor="middle" fill="{t["fg"]}" font-weight="600">{esc(label)}</text>',
    ]


def card_chrome(
    x: float,
    y: float,
    w: float,
    h: float,
    title_bold: str,
    title_meta: str,
    t: dict[str, str],
    accent_border: bool = False,
) -> list[str]:
    stroke = t["accent"] if accent_border else t["border"]
    sw = "1.5" if accent_border else "1"
    title_fill = t["accent"] if accent_border else t["fg"]
    return [
        f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="12" fill="{t["card_bg"]}" stroke="{stroke}" stroke-width="{sw}"/>',
        f'<circle cx="{x + 20}" cy="{y + 20}" r="5.5" fill="{t["dot"]}"/>',
        f'<circle cx="{x + 36}" cy="{y + 20}" r="5.5" fill="{t["dot"]}"/>',
        f'<circle cx="{x + 52}" cy="{y + 20}" r="5.5" fill="{t["dot"]}"/>',
        f'<text x="{x + 76}" y="{y + 25}" class="s"><tspan font-weight="600" fill="{title_fill}">{esc(title_bold)}</tspan><tspan fill="{t["muted"]}">{esc(title_meta)}</tspan></text>',
        f'<line x1="{x}" y1="{y + 40}" x2="{x + w}" y2="{y + 40}" stroke="{t["border"]}"/>',
    ]


# ---------------------------------------------------------------------------
# Live capture helpers (run once in an isolated tempdir)
# ---------------------------------------------------------------------------
def capture_all(binary: Path) -> dict[str, Any]:
    binary = binary.resolve()
    if not binary.is_file():
        raise FileNotFoundError(f"justless binary not found: {binary}")

    with tempfile.TemporaryDirectory(prefix="justless-assets-") as tmp:
        work = Path(tmp)
        home = work / "home"
        home.mkdir()
        env = clean_env(home)

        # 1. Failing cargo test on disposable copy of fixtures/failing
        failing_dir = work / "failing"
        shutil.copytree(REPO_ROOT / "fixtures/failing", failing_dir)
        (failing_dir / ".justless.toml").write_text("", encoding="utf-8")
        warm = run_cmd(["cargo", "test", "--no-run"], failing_dir, env)
        if warm.returncode != 0:
            raise RuntimeError(f"cargo test --no-run failed:\n{warm.stderr.decode('utf-8', 'replace')}")

        raw_ct = run_cmd(["cargo", "test"], failing_dir, env)
        jl_ct = run_cmd([str(binary), "run", "--hook", "--fresh", "--", "cargo", "test"], failing_dir, env)
        ct_run_id = RUN_ID_RE.search(jl_ct.stderr.decode("utf-8", "replace"))
        if not ct_run_id:
            raise RuntimeError(f"missing run ID from cargo test:\n{jl_ct.stderr.decode('utf-8', 'replace')}")
        run_id = ct_run_id.group(1)

        jl_ct_stdout = jl_ct.stdout.decode("utf-8", "replace")
        jl_ct_footer = jl_ct.stderr.decode("utf-8", "replace").strip()
        # Locate [stdout:A-B] on the failing test line
        fail_line = next(
            (ln for ln in jl_ct_stdout.splitlines() if "total_includes_fee" in ln and "[stdout:" in ln),
            "",
        )
        rng = RANGE_RE.search(fail_line)
        if not rng:
            raise RuntimeError(f"no [stdout:A-B] in failing cargo test output:\n{jl_ct_stdout}")
        ct_from, ct_to = int(rng.group(2)), int(rng.group(3) or rng.group(2))
        ct_replay_proc = run_cmd(
            [str(binary), "log", run_id, "--stream", "stdout", "--from", str(ct_from), "--to", str(ct_to)],
            failing_dir,
            env,
        )
        if ct_replay_proc.returncode != 0:
            raise RuntimeError("justless log replay failed for cargo test")
        ct_replay_lines = ct_replay_proc.stdout.decode("utf-8", "replace").splitlines()

        # Reconstruct combined raw terminal stream from archived stdout+stderr of the exact same run
        arch_out = run_cmd([str(binary), "log", run_id, "--stream", "stdout"], failing_dir, env).stdout.decode("utf-8", "replace")
        arch_err = run_cmd([str(binary), "log", run_id, "--stream", "stderr"], failing_dir, env).stderr.decode("utf-8", "replace")
        err_lines = arch_err.splitlines()
        out_lines = arch_out.splitlines()
        # Natural terminal order: Finished + Running unittests, unittest output, Running tests/checkout.rs, checkout output, error line
        raw_ct_combined: list[str] = []
        if len(err_lines) >= 4:
            # Find where second test binary begins in stdout ("running 1 test")
            split_idx = next((i for i, l in enumerate(out_lines) if l.strip() == "running 1 test"), 6)
            raw_ct_combined.extend(err_lines[:2])
            raw_ct_combined.extend(out_lines[: max(0, split_idx - 1)])
            raw_ct_combined.append(err_lines[2])
            raw_ct_combined.extend(out_lines[max(0, split_idx - 1) :])
            raw_ct_combined.extend(err_lines[3:])
        else:
            raw_ct_combined = err_lines + out_lines

        failing_cap = {
            "run_id": run_id,
            "raw_bytes": len(raw_ct.stdout) + len(raw_ct.stderr),
            "raw_lines": raw_ct_combined,
            "raw_line_count": len(out_lines) + len(err_lines),
            "jl_stdout_bytes": len(jl_ct.stdout),
            "jl_footer_bytes": len(jl_ct.stderr),
            "jl_total_bytes": len(jl_ct.stdout) + len(jl_ct.stderr),
            "jl_lines": jl_ct_stdout.splitlines(),
            "jl_footer": jl_ct_footer,
            "replay_from": ct_from,
            "replay_to": ct_to,
            "replay_lines": ct_replay_lines,
        }

        # 2. Isolated copy of repo source + docs/reference.md
        repo_copy = work / "repo_copy"
        repo_copy.mkdir()
        (repo_copy / ".justless.toml").write_text("", encoding="utf-8")
        shutil.copy2(REPO_ROOT / "Cargo.toml", repo_copy / "Cargo.toml")
        shutil.copytree(REPO_ROOT / "src", repo_copy / "src")
        (repo_copy / "docs").mkdir()
        shutil.copy2(REPO_ROOT / "docs/reference.md", repo_copy / "docs/reference.md")

        # 2a. Outline + expand on docs/reference.md
        ref_raw_bytes = (repo_copy / "docs/reference.md").read_bytes()
        ref_raw_lines = ref_raw_bytes.decode("utf-8").splitlines()
        out_proc = run_cmd([str(binary), "read", "docs/reference.md", "--outline"], repo_copy, env)
        if out_proc.returncode != 0:
            raise RuntimeError(f"read --outline failed: {out_proc.stderr.decode('utf-8', 'replace')}")
        outline_text = out_proc.stdout.decode("utf-8")
        rev_m = re.search(r"^revision ([0-9a-f]+)", outline_text, re.MULTILINE)
        fail_m = re.search(r"^#(\d+) L(\d+)-(\d+) h3 Failures$", outline_text, re.MULTILINE)
        if not rev_m or not fail_m:
            raise RuntimeError(f"could not locate Failures anchor in outline:\n{outline_text}")
        anchor = f"{rev_m.group(1)}#{fail_m.group(1)}"
        fail_start, fail_end = int(fail_m.group(2)), int(fail_m.group(3))
        exp_proc = run_cmd([str(binary), "expand", "docs/reference.md", anchor], repo_copy, env)
        if exp_proc.returncode != 0:
            raise RuntimeError(f"expand failed: {exp_proc.stderr.decode('utf-8', 'replace')}")

        outline_cap = {
            "raw_bytes": len(ref_raw_bytes),
            "raw_lines": ref_raw_lines,
            "outline_bytes": len(out_proc.stdout),
            "outline_lines": outline_text.splitlines(),
            "anchor": anchor,
            "fail_entry_idx": next(
                i for i, l in enumerate(outline_text.splitlines()) if l.startswith(f"#{fail_m.group(1)} ")
            ),
            "fail_start": fail_start,
            "fail_end": fail_end,
            "expand_bytes": len(exp_proc.stdout),
            "expand_lines": exp_proc.stdout.decode("utf-8").splitlines(),
        }

        # 2b. Live rg -n "fn " src (single run so raw log & grouped report share exact line numbers!)
        rg_jl = run_cmd([str(binary), "run", "--fresh", "--", "rg", "-n", "fn ", "src"], repo_copy, env)
        rg_rid_m = RUN_ID_RE.search(rg_jl.stderr.decode("utf-8", "replace"))
        if not rg_rid_m:
            raise RuntimeError(f"missing run ID from rg:\n{rg_jl.stderr.decode('utf-8', 'replace')}")
        rg_rid = rg_rid_m.group(1)
        rg_raw_text = run_cmd(
            [str(binary), "log", rg_rid, "--stream", "stdout"], repo_copy, env
        ).stdout.decode("utf-8", "replace")
        rg_jl_text = rg_jl.stdout.decode("utf-8", "replace")
        rg_jl_footer = rg_jl.stderr.decode("utf-8", "replace").strip()

        # Find first file group's [stdout:A-B] range and replay 3 lines (A..A+2)
        first_group_m = RANGE_RE.search(rg_jl_text)
        if not first_group_m:
            raise RuntimeError(f"no [stdout:A-B] in rg summary:\n{rg_jl_text}")
        rg_from = int(first_group_m.group(2))
        rg_to = min(int(first_group_m.group(3) or rg_from), rg_from + 2)
        rg_replay = run_cmd(
            [str(binary), "log", rg_rid, "--stream", "stdout", "--from", str(rg_from), "--to", str(rg_to)],
            repo_copy,
            env,
        ).stdout.decode("utf-8", "replace")

        search_cap = {
            "run_id": rg_rid,
            "raw_bytes": len(rg_raw_text.encode("utf-8")),
            "raw_lines": rg_raw_text.splitlines(),
            "jl_bytes": len(rg_jl.stdout) + len(rg_jl.stderr),
            "jl_stdout_bytes": len(rg_jl.stdout),
            "jl_footer_bytes": len(rg_jl.stderr),
            "jl_lines": rg_jl_text.splitlines(),
            "jl_footer": rg_jl_footer,
            "replay_from": rg_from,
            "replay_to": rg_to,
            "replay_lines": rg_replay.splitlines(),
        }

        # 2c. Live query would_route vs rg -n -w would_route src + cat src/agents.rs
        q_rg = run_cmd(["rg", "-n", "-w", "would_route", "src"], repo_copy, env)
        agents_raw = (repo_copy / "src/agents.rs").read_bytes()
        agents_lines = agents_raw.decode("utf-8").splitlines()
        q_jl = run_cmd([str(binary), "query", "would_route"], repo_copy, env)
        if q_jl.returncode != 0:
            raise RuntimeError(f"query would_route failed: {q_jl.stderr.decode('utf-8', 'replace')}")
        q_jl_text = q_jl.stdout.decode("utf-8")
        def_m = re.search(r"at src/agents\.rs:(\d+)-(\d+)", q_jl_text)
        if not def_m:
            raise RuntimeError(f"could not parse definition span from query:\n{q_jl_text}")
        def_start, def_end = int(def_m.group(1)), int(def_m.group(2))

        query_cap = {
            "rg_lines": q_rg.stdout.decode("utf-8", "replace").splitlines(),
            "rg_bytes": len(q_rg.stdout),
            "agents_bytes": len(agents_raw),
            "agents_lines": agents_lines,
            "raw_bytes": len(q_rg.stdout) + len(agents_raw),
            "raw_line_count": len(q_rg.stdout.decode("utf-8", "replace").splitlines()) + len(agents_lines),
            "jl_bytes": len(q_jl.stdout),
            "jl_lines": q_jl_text.splitlines(),
            "def_start": def_start,
            "def_end": def_end,
        }

    return {
        "failing": failing_cap,
        "outline": outline_cap,
        "search": search_cap,
        "query": query_cap,
    }


# ---------------------------------------------------------------------------
# 1. demo-dark.svg, demo-light.svg, and static-frame renderer for demo-dark.gif
# ---------------------------------------------------------------------------
def _build_demo_layout(cap: dict[str, Any]) -> dict[str, Any]:
    """Build wrapped display lines & exact timeline timestamps for demo SVG/GIF."""
    cmd1 = "cargo test"
    cmd2 = (
        f"justless log {cap['run_id']} --stream stdout "
        f"--from {cap['replay_from']} --to {cap['replay_to']}"
    )

    # Prepare output lines for cmd1: split [stdout:A-B] badge onto its own highlighted line
    # and convert leading spaces into exact column indents so SVG preserves alignment cleanly.
    display_rows: list[dict[str, Any]] = []
    for raw_line in cap["jl_lines"]:
        if "total_includes_fee" in raw_line and "[stdout:" in raw_line:
            m = RANGE_RE.search(raw_line)
            if m:
                prefix = raw_line[: m.start()].rstrip()
                badge = m.group(0)
                for indent, seg in wrap_mono(prefix, 108):
                    lead = len(seg) - len(seg.lstrip(" "))
                    display_rows.append({"kind": "text", "indent": indent + lead, "text": seg.lstrip(" ")})
                display_rows.append({"kind": "badge", "indent": 2, "text": badge})
                continue
        for indent, seg in wrap_mono(raw_line, 108):
            lead = len(seg) - len(seg.lstrip(" "))
            display_rows.append({"kind": "text", "indent": indent + lead, "text": seg.lstrip(" ")})
    display_rows.append({"kind": "footer", "indent": 0, "text": cap["jl_footer"]})

    # Exact vertical spacing inside 900x426
    y_cmd1 = 58.0
    step_out = 18.0
    y_out_start = 76.0
    for idx, row in enumerate(display_rows):
        row["y"] = y_out_start + idx * step_out
    y_footer = display_rows[-1]["y"]
    y_cmd2 = y_footer + 27.0
    y_replay_start = y_cmd2 + 21.0
    step_replay = 18.5
    replay_rows = []
    for idx, line in enumerate(cap["replay_lines"][:4]):
        lead = len(line) - len(line.lstrip(" "))
        stripped = line.lstrip(" ")
        if len(line) > 106:
            stripped = stripped[: 106 - lead]
        replay_rows.append(
            {"indent": lead, "text": stripped, "raw": line, "y": y_replay_start + idx * step_replay}
        )
    y_prompt = y_replay_start + len(replay_rows) * step_replay + 16.0

    # Timeline (12.0s loop)
    duration_ms = 12000
    t_cmd1_start = 300
    t_cmd1_end = 900
    t_pill1 = 1050
    t_out_start = 1350
    t_out_step = 95
    t_out_last = t_out_start + (len(display_rows) - 1) * t_out_step
    t_pill2 = t_out_last + 350
    t_cmd2_show = t_pill2 + 750
    t_cmd2_type_start = t_cmd2_show + 250
    t_cmd2_type_end = t_cmd2_type_start + 1700
    t_pulse_peak = t_cmd2_type_end
    t_replay_start = t_cmd2_type_end + 350
    t_replay_step = 110
    t_pill3 = t_replay_start + len(replay_rows) * t_replay_step + 150
    t_prompt = t_pill3 + 350
    t_fade_start = 11250
    t_fade_end = 11700

    return {
        "cmd1": cmd1,
        "cmd2": cmd2,
        "display_rows": display_rows,
        "replay_rows": replay_rows,
        "y_cmd1": y_cmd1,
        "y_footer": y_footer,
        "y_cmd2": y_cmd2,
        "y_replay_start": y_replay_start,
        "step_replay": step_replay,
        "y_prompt": y_prompt,
        "duration_ms": duration_ms,
        "t_cmd1_start": t_cmd1_start,
        "t_cmd1_end": t_cmd1_end,
        "t_pill1": t_pill1,
        "t_out_start": t_out_start,
        "t_out_step": t_out_step,
        "t_pill2": t_pill2,
        "t_cmd2_show": t_cmd2_show,
        "t_cmd2_type_start": t_cmd2_type_start,
        "t_cmd2_type_end": t_cmd2_type_end,
        "t_pulse_peak": t_pulse_peak,
        "t_replay_start": t_replay_start,
        "t_replay_step": t_replay_step,
        "t_pill3": t_pill3,
        "t_prompt": t_prompt,
        "t_fade_start": t_fade_start,
        "t_fade_end": t_fade_end,
    }


def _pct(ms: float, dur: float) -> str:
    return f"{(ms * 100.0 / dur):.3f}%"


def render_demo_svg(cap: dict[str, Any], theme_name: str, static_ms: int | None = None) -> str:
    """Render animated demo-{dark,light}.svg, or an exact static frame at static_ms for GIF."""
    t = THEMES[theme_name]
    lay = _build_demo_layout(cap)
    cw = 7.8  # 13px monospace advance width
    x0 = 22.0
    x_cmd = x0 + 2 * cw  # 37.6
    dur = float(lay["duration_ms"])
    dur_s = dur / 1000.0

    cmd1 = lay["cmd1"]
    cmd2 = lay["cmd2"]
    w_cmd1 = len(cmd1) * cw
    w_cmd2 = len(cmd2) * cw

    jl_shown_lines = len(cap["jl_lines"]) + 1
    pill1_text = "hook routes it through justless run"
    pill2_text = f"{cap['raw_line_count']} lines → {jl_shown_lines} lines to the agent"
    pill3_text = f"{len(lay['replay_rows'])} lines back, byte for byte"

    desc_lines = [
        f"The agent runs cargo test; the hook routes it through justless run, which returns {jl_shown_lines} lines instead of {cap['raw_line_count']} and names the run.",
        f"The agent then replays lines {cap['replay_from']} to {cap['replay_to']} of the archived stdout. Captured output:",
        f"$ {cmd1}",
        *cap["jl_lines"],
        cap["jl_footer"],
        f"$ {cmd2}",
        *(r["raw"] for r in lay["replay_rows"]),
    ]

    parts: list[str] = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 900 426" width="900" height="426" role="img" aria-labelledby="t d">',
        "<title id=\"t\">justless demo: a failing cargo test, then an exact log replay</title>",
        f"<desc id=\"d\">{esc(chr(10).join(desc_lines))}\n</desc>",
    ]

    # Styles (animated CSS when static_ms is None; clean static font style when rendering GIF frames)
    style_lines = [
        f"text{{font-family:{MONO_FONT};font-size:13px}}",
        f".s{{font-family:{SANS_FONT};font-size:12px}}",
    ]
    if static_ms is None:
        def add_appear(cls: str, ms: float) -> None:
            p0 = _pct(max(0.0, ms - 1.0), dur)
            p1 = _pct(ms, dur)
            style_lines.append(f"@keyframes {cls}{{0%,{p0}{{opacity:0}}{p1},100%{{opacity:1}}}}")
            style_lines.append(f".{cls}{{animation:{cls} {dur_s:.1f}s linear infinite}}")

        add_appear("a_pill1", lay["t_pill1"])
        for i in range(len(lay["display_rows"])):
            add_appear(f"a_out{i}", lay["t_out_start"] + i * lay["t_out_step"])
        add_appear("a_pill2", lay["t_pill2"])
        add_appear("a_cmd2", lay["t_cmd2_show"])
        for i in range(len(lay["replay_rows"])):
            add_appear(f"a_rep{i}", lay["t_replay_start"] + i * lay["t_replay_step"])
        add_appear("a_pill3", lay["t_pill3"])
        add_appear("a_prompt", lay["t_prompt"])

        # Pulse highlight on [stdout:A-B]
        p_ps = _pct(lay["t_pulse_peak"] - 500, dur)
        p_pk = _pct(lay["t_pulse_peak"], dur)
        p_pe = _pct(lay["t_pulse_peak"] + 1100, dur)
        style_lines.append(f"@keyframes pulse{{0%,{p_ps}{{opacity:0}}{p_pk}{{opacity:1}}{p_pe},100%{{opacity:0}}}}")
        style_lines.append(f".pulse{{opacity:0;animation:pulse {dur_s:.1f}s ease-out infinite}}")

        # Typing reveal for cmd1
        c1s = _pct(lay["t_cmd1_start"], dur)
        c1e = _pct(lay["t_cmd1_end"], dur)
        c1h = _pct(lay["t_cmd1_end"] + 10, dur)
        c1c = _pct(lay["t_pill1"], dur)
        style_lines.append(
            f"@keyframes type_cmd1{{0%,{c1s}{{transform:translateX(0);opacity:1;animation-timing-function:steps({len(cmd1)},end)}}"
            f"{c1e}{{transform:translateX({w_cmd1:.1f}px);opacity:1}}"
            f"{c1h},100%{{transform:translateX({w_cmd1:.1f}px);opacity:0}}}}"
        )
        style_lines.append(f".type_cmd1{{opacity:0;animation:type_cmd1 {dur_s:.1f}s linear infinite}}")
        style_lines.append(
            f"@keyframes cur_cmd1{{0%,{c1s}{{opacity:1;transform:translateX(0);animation-timing-function:steps({len(cmd1)},end)}}"
            f"{c1e},{c1c}{{opacity:1;transform:translateX({w_cmd1:.1f}px)}}"
            f"{_pct(lay['t_pill1'] + 10, dur)},100%{{opacity:0;transform:translateX({w_cmd1:.1f}px)}}}}"
        )
        style_lines.append(f".cur_cmd1{{opacity:0;animation:cur_cmd1 {dur_s:.1f}s linear infinite}}")

        # Typing reveal for cmd2
        c2show = _pct(lay["t_cmd2_show"], dur)
        c2s = _pct(lay["t_cmd2_type_start"], dur)
        c2e = _pct(lay["t_cmd2_type_end"], dur)
        c2h = _pct(lay["t_cmd2_type_end"] + 10, dur)
        c2c = _pct(lay["t_replay_start"] - 100, dur)
        style_lines.append(
            f"@keyframes type_cmd2{{0%,{c2s}{{transform:translateX(0);opacity:1;animation-timing-function:steps({len(cmd2)},end)}}"
            f"{c2e}{{transform:translateX({w_cmd2:.1f}px);opacity:1}}"
            f"{c2h},100%{{transform:translateX({w_cmd2:.1f}px);opacity:0}}}}"
        )
        style_lines.append(f".type_cmd2{{opacity:0;animation:type_cmd2 {dur_s:.1f}s linear infinite}}")
        style_lines.append(
            f"@keyframes cur_cmd2{{0%,{_pct(lay['t_cmd2_show'] - 1, dur)}{{opacity:0;transform:translateX(0)}}"
            f"{c2show},{c2s}{{opacity:1;transform:translateX(0);animation-timing-function:steps({len(cmd2)},end)}}"
            f"{c2e},{c2c}{{opacity:1;transform:translateX({w_cmd2:.1f}px)}}"
            f"{_pct(lay['t_replay_start'] - 90, dur)},100%{{opacity:0;transform:translateX({w_cmd2:.1f}px)}}}}"
        )
        style_lines.append(f".cur_cmd2{{opacity:0;animation:cur_cmd2 {dur_s:.1f}s linear infinite}}")

        style_lines.append("@keyframes blink{0%,49.9%{opacity:1}50%,100%{opacity:0}}")
        style_lines.append(".blink{animation:blink 1s steps(1,end) infinite}")
        style_lines.append(
            f"@keyframes screen{{0%,{_pct(lay['t_fade_start'], dur)}{{opacity:1}}"
            f"{_pct(lay['t_fade_end'], dur)},100%{{opacity:0}}}}"
        )
        style_lines.append(f".screen{{animation:screen {dur_s:.1f}s ease-in infinite}}")
        style_lines.append("@media (prefers-reduced-motion: reduce){*{animation:none!important}}")

    parts.append("<style>\n" + "\n".join(style_lines) + "\n</style>")

    # Window frame + titlebar + prism logo
    parts.extend([
        f'<rect x="0.75" y="0.75" width="898.5" height="424.5" rx="12" fill="{t["card_bg"]}" stroke="{t["border"]}" stroke-width="1.5"/>',
        f'<path d="M0.75 34 V12.75 a12 12 0 0 1 12 -12 H887.25 a12 12 0 0 1 12 12 V34 Z" fill="{t["titlebar_bg"]}"/>',
        f'<line x1="0.75" y1="34" x2="899.25" y2="34" stroke="{t["border"]}"/>',
        f'<circle cx="22" cy="17.0" r="5.5" fill="{t["dot_demo"]}"/>',
        f'<circle cx="40" cy="17.0" r="5.5" fill="{t["dot_demo"]}"/>',
        f'<circle cx="58" cy="17.0" r="5.5" fill="{t["dot_demo"]}"/>',
        f'<text x="450.0" y="21.5" class="s" font-size="12.5" fill="{t["muted"]}" text-anchor="middle">agent shell · copy of fixtures/failing</text>',
        f'<g transform="translate(802 10.0)">'
        f'<path d="M0 3 L7 0 L14 3 L7 6 Z" fill="{t["fg"]}" opacity="0.9"/>'
        f'<path d="M0 6.5 L7 9.5 L14 6.5" fill="none" stroke="{t["raw_bar"]}" stroke-width="1.6" stroke-linejoin="round"/>'
        f'<path d="M3.5 11 L7 14.5 L10.5 11 Z" fill="{t["accent"]}"/></g>',
        f'<text x="822" y="21.5" font-size="13" font-weight="700" fill="{t["fg"]}">justless</text>',
    ])

    def pill_svg(text: str, y_baseline: float, cls: str, show: bool) -> str:
        if not show:
            return ""
        pw = max(168.0, len(text) * 6.8 + 24.0)
        px = 884.0 - pw
        g_open = f'<g class="{cls}">' if static_ms is None else "<g>"
        return (
            f"{g_open}"
            f'<rect x="{px:.1f}" y="{y_baseline - 14.5:.1f}" width="{pw:.1f}" height="21" rx="10.5" fill="{t["pill_bg"]}"/>'
            f'<text x="{px + pw / 2.0:.1f}" y="{y_baseline - 0.5:.1f}" class="s" font-size="12" font-weight="600" fill="{t["fg"]}" text-anchor="middle">{esc(text)}</text>'
            "</g>"
        )

    if static_ms is None:
        # Animated CSS SVG branch
        parts.extend([
            f'<clipPath id="clip_cmd1"><rect x="{x_cmd - 0.5:.1f}" y="{lay["y_cmd1"] - 14:.1f}" width="{w_cmd1 + 1.0:.1f}" height="19"/></clipPath>',
            f'<clipPath id="clip_cmd2"><rect x="{x_cmd - 0.5:.1f}" y="{lay["y_cmd2"] - 14:.1f}" width="{w_cmd2 + 1.0:.1f}" height="19"/></clipPath>',
            '<g class="screen">',
        ])
        # Growing replay backdrop
        rep_top = lay["y_replay_start"] - 14.0
        for i in range(len(lay["replay_rows"])):
            h_box = 4.0 + (i + 1) * lay["step_replay"]
            parts.append(
                f'<g class="a_rep{i}">'
                f'<rect x="12" y="{rep_top:.1f}" width="876" height="{h_box:.1f}" rx="6" fill="{t["replay_bg"]}"/>'
                f'<rect x="12" y="{rep_top:.1f}" width="3" height="{h_box:.1f}" rx="1.5" fill="{t["accent"]}"/>'
                "</g>"
            )
        # Cmd 1
        parts.append(
            f'<text x="{x0:.1f}" y="{lay["y_cmd1"]:.1f}" fill="{t["accent"]}" font-weight="700" textLength="{cw:.1f}" lengthAdjust="spacing">$</text>'
            f'<text x="{x_cmd:.1f}" y="{lay["y_cmd1"]:.1f}" fill="{t["fg"]}" font-weight="600" textLength="{w_cmd1:.1f}" lengthAdjust="spacing">{esc(cmd1)}</text>'
        )
        # Output lines
        for i, row in enumerate(lay["display_rows"]):
            rx = x0 + row["indent"] * cw
            rw = len(row["text"]) * cw
            if row["kind"] == "badge":
                parts.append(
                    f'<g class="a_out{i}">'
                    f'<rect x="{rx - 3.0:.1f}" y="{row["y"] - 13.5:.1f}" width="{rw + 6.0:.1f}" height="17.5" rx="4" fill="{t["pill_bg"]}"/>'
                    f'<rect class="pulse" x="{rx - 5.0:.1f}" y="{row["y"] - 15.5:.1f}" width="{rw + 10.0:.1f}" height="21.5" rx="6" fill="none" stroke="{t["accent"]}" stroke-width="1.5"/>'
                    f'<text x="{rx:.1f}" y="{row["y"]:.1f}" fill="{t["accent"]}" font-weight="700" textLength="{rw:.1f}" lengthAdjust="spacing">{esc(row["text"])}</text>'
                    "</g>"
                )
            else:
                fill = t["muted"] if row["kind"] == "footer" else t["fg"]
                parts.append(
                    f'<g class="a_out{i}"><text x="{rx:.1f}" y="{row["y"]:.1f}" fill="{fill}" textLength="{rw:.1f}" lengthAdjust="spacing">{esc(row["text"])}</text></g>'
                )
        # Cmd 2
        parts.append(
            f'<g class="a_cmd2">'
            f'<text x="{x0:.1f}" y="{lay["y_cmd2"]:.1f}" fill="{t["accent"]}" font-weight="700" textLength="{cw:.1f}" lengthAdjust="spacing">$</text>'
            f'<text x="{x_cmd:.1f}" y="{lay["y_cmd2"]:.1f}" fill="{t["fg"]}" font-weight="600" textLength="{w_cmd2:.1f}" lengthAdjust="spacing">{esc(cmd2)}</text>'
            "</g>"
        )
        # Replay lines
        for i, row in enumerate(lay["replay_rows"]):
            rx = x0 + row["indent"] * cw
            rw = len(row["text"]) * cw
            parts.append(
                f'<g class="a_rep{i}"><text x="{rx:.1f}" y="{row["y"]:.1f}" fill="{t["fg"]}" textLength="{rw:.1f}" lengthAdjust="spacing">{esc(row["text"])}</text></g>'
            )
        # Prompt + typing masks + cursors + callout pills
        parts.extend([
            f'<g class="a_prompt"><text x="{x0:.1f}" y="{lay["y_prompt"]:.1f}" fill="{t["accent"]}" font-weight="700" textLength="{cw:.1f}" lengthAdjust="spacing">$</text></g>',
            f'<g clip-path="url(#clip_cmd1)"><rect class="type_cmd1" x="{x_cmd - 0.5:.1f}" y="{lay["y_cmd1"] - 14:.1f}" width="{w_cmd1 + 1.0:.1f}" height="19" fill="{t["card_bg"]}"/></g>',
            f'<rect class="cur_cmd1" x="{x_cmd:.1f}" y="{lay["y_cmd1"] - 12.5:.1f}" width="{cw:.1f}" height="15.5" rx="1" fill="{t["accent"]}" opacity="0.85"/>',
            f'<g clip-path="url(#clip_cmd2)"><rect class="type_cmd2" x="{x_cmd - 0.5:.1f}" y="{lay["y_cmd2"] - 14:.1f}" width="{w_cmd2 + 1.0:.1f}" height="19" fill="{t["card_bg"]}"/></g>',
            f'<rect class="cur_cmd2" x="{x_cmd:.1f}" y="{lay["y_cmd2"] - 12.5:.1f}" width="{cw:.1f}" height="15.5" rx="1" fill="{t["accent"]}" opacity="0.85"/>',
            f'<g class="a_prompt"><rect class="blink" x="{x_cmd:.1f}" y="{lay["y_prompt"] - 12.5:.1f}" width="{cw:.1f}" height="15.5" rx="1" fill="{t["accent"]}" opacity="0.85"/></g>',
            pill_svg(pill1_text, lay["y_cmd1"], "a_pill1", True),
            pill_svg(pill2_text, lay["y_footer"], "a_pill2", True),
            pill_svg(pill3_text, lay["y_cmd2"], "a_pill3", True),
            "</g>",
        ])
    else:
        # Static-frame renderer for GIF at exact timestamp static_ms.
        # Avoid full-canvas alpha fade in GIF mode so paletteuse diff_mode=rectangle
        # keeps file size crisp and tiny (~100-200 KB) with razor-sharp text.
        ms = static_ms
        parts.append("<g>")

        # Replay highlight backdrop up to revealed count
        revealed_rep = sum(
            1 for i in range(len(lay["replay_rows"])) if ms >= lay["t_replay_start"] + i * lay["t_replay_step"]
        )
        if revealed_rep > 0:
            rep_top = lay["y_replay_start"] - 14.0
            h_box = 4.0 + revealed_rep * lay["step_replay"]
            parts.append(
                f'<rect x="12" y="{rep_top:.1f}" width="876" height="{h_box:.1f}" rx="6" fill="{t["replay_bg"]}"/>'
                f'<rect x="12" y="{rep_top:.1f}" width="3" height="{h_box:.1f}" rx="1.5" fill="{t["accent"]}"/>'
            )

        # Cmd 1 partial typing
        if ms < lay["t_cmd1_start"]:
            n1 = 0
        elif ms >= lay["t_cmd1_end"]:
            n1 = len(cmd1)
        else:
            frac = (ms - lay["t_cmd1_start"]) / (lay["t_cmd1_end"] - lay["t_cmd1_start"])
            n1 = min(len(cmd1), int(frac * len(cmd1)))
        typed1 = cmd1[:n1]
        parts.append(
            f'<text x="{x0:.1f}" y="{lay["y_cmd1"]:.1f}" fill="{t["accent"]}" font-weight="700">$</text>'
        )
        if typed1:
            parts.append(
                f'<text x="{x_cmd:.1f}" y="{lay["y_cmd1"]:.1f}" fill="{t["fg"]}" font-weight="600" textLength="{len(typed1) * cw:.1f}" lengthAdjust="spacing">{esc(typed1)}</text>'
            )
        if ms < lay["t_pill1"]:
            cx1 = x_cmd + n1 * cw
            parts.append(
                f'<rect x="{cx1:.1f}" y="{lay["y_cmd1"] - 12.5:.1f}" width="{cw:.1f}" height="15.5" rx="1" fill="{t["accent"]}" opacity="0.85"/>'
            )

        # Output lines revealed sequentially
        for i, row in enumerate(lay["display_rows"]):
            if ms < lay["t_out_start"] + i * lay["t_out_step"]:
                continue
            rx = x0 + row["indent"] * cw
            rw = len(row["text"]) * cw
            if row["kind"] == "badge":
                pulse_op = 0.0
                if lay["t_pulse_peak"] - 500 <= ms <= lay["t_pulse_peak"] + 1100:
                    if ms <= lay["t_pulse_peak"]:
                        pulse_op = (ms - (lay["t_pulse_peak"] - 500)) / 500.0
                    else:
                        pulse_op = 1.0 - (ms - lay["t_pulse_peak"]) / 1100.0
                parts.append(
                    f'<rect x="{rx - 3.0:.1f}" y="{row["y"] - 13.5:.1f}" width="{rw + 6.0:.1f}" height="17.5" rx="4" fill="{t["pill_bg"]}"/>'
                )
                if pulse_op > 0.02:
                    parts.append(
                        f'<rect x="{rx - 5.0:.1f}" y="{row["y"] - 15.5:.1f}" width="{rw + 10.0:.1f}" height="21.5" rx="6" fill="none" stroke="{t["accent"]}" stroke-width="1.5" opacity="{pulse_op:.2f}"/>'
                    )
                parts.append(
                    f'<text x="{rx:.1f}" y="{row["y"]:.1f}" fill="{t["accent"]}" font-weight="700" textLength="{rw:.1f}" lengthAdjust="spacing">{esc(row["text"])}</text>'
                )
            else:
                fill = t["muted"] if row["kind"] == "footer" else t["fg"]
                parts.append(
                    f'<text x="{rx:.1f}" y="{row["y"]:.1f}" fill="{fill}" textLength="{rw:.1f}" lengthAdjust="spacing">{esc(row["text"])}</text>'
                )

        # Cmd 2 partial typing
        if ms >= lay["t_cmd2_show"]:
            if ms < lay["t_cmd2_type_start"]:
                n2 = 0
            elif ms >= lay["t_cmd2_type_end"]:
                n2 = len(cmd2)
            else:
                frac = (ms - lay["t_cmd2_type_start"]) / (lay["t_cmd2_type_end"] - lay["t_cmd2_type_start"])
                n2 = min(len(cmd2), int(frac * len(cmd2)))
            typed2 = cmd2[:n2]
            parts.append(
                f'<text x="{x0:.1f}" y="{lay["y_cmd2"]:.1f}" fill="{t["accent"]}" font-weight="700">$</text>'
            )
            if typed2:
                parts.append(
                    f'<text x="{x_cmd:.1f}" y="{lay["y_cmd2"]:.1f}" fill="{t["fg"]}" font-weight="600" textLength="{len(typed2) * cw:.1f}" lengthAdjust="spacing">{esc(typed2)}</text>'
                )
            if ms < lay["t_replay_start"] - 100:
                cx2 = x_cmd + n2 * cw
                parts.append(
                    f'<rect x="{cx2:.1f}" y="{lay["y_cmd2"] - 12.5:.1f}" width="{cw:.1f}" height="15.5" rx="1" fill="{t["accent"]}" opacity="0.85"/>'
                )

        # Replay lines
        for i in range(revealed_rep):
            row = lay["replay_rows"][i]
            rx = x0 + row["indent"] * cw
            rw = len(row["text"]) * cw
            parts.append(
                f'<text x="{rx:.1f}" y="{row["y"]:.1f}" fill="{t["fg"]}" textLength="{rw:.1f}" lengthAdjust="spacing">{esc(row["text"])}</text>'
            )

        # Final prompt + blinking cursor
        if ms >= lay["t_prompt"]:
            parts.append(
                f'<text x="{x0:.1f}" y="{lay["y_prompt"]:.1f}" fill="{t["accent"]}" font-weight="700">$</text>'
            )
            if ((ms - lay["t_prompt"]) // 500) % 2 == 0:
                parts.append(
                    f'<rect x="{x_cmd:.1f}" y="{lay["y_prompt"] - 12.5:.1f}" width="{cw:.1f}" height="15.5" rx="1" fill="{t["accent"]}" opacity="0.85"/>'
                )

        # Callout pills
        parts.append(pill_svg(pill1_text, lay["y_cmd1"], "", ms >= lay["t_pill1"]))
        parts.append(pill_svg(pill2_text, lay["y_footer"], "", ms >= lay["t_pill2"]))
        parts.append(pill_svg(pill3_text, lay["y_cmd2"], "", ms >= lay["t_pill3"]))
        parts.append("</g>")

    parts.append("</svg>\n")
    return "\n".join(p for p in parts if p)


def render_demo_gif(cap: dict[str, Any], out_gif: Path) -> None:
    """Render crisp 900x426 looping demo-dark.gif at 10 fps via resvg + two-pass ffmpeg."""
    if not RESVG_BIN.is_file():
        raise FileNotFoundError(f"resvg not found at {RESVG_BIN}")
    if not FFMPEG_BIN.is_file():
        raise FileNotFoundError(f"ffmpeg not found at {FFMPEG_BIN}")

    lay = _build_demo_layout(cap)
    fps = 10
    step_ms = 1000 // fps
    frame_times = list(range(0, lay["duration_ms"], step_ms))

    with tempfile.TemporaryDirectory(prefix="justless-gif-") as tmp:
        td = Path(tmp)

        def build_frame(args: tuple[int, int]) -> None:
            idx, ms = args
            svg_path = td / f"frame_{idx:04d}.svg"
            png_path = td / f"frame_{idx:04d}.png"
            svg_path.write_text(render_demo_svg(cap, "dark", static_ms=ms), encoding="utf-8")
            proc = subprocess.run(
                [str(RESVG_BIN), "--background", THEMES["dark"]["canvas_bg"], str(svg_path), str(png_path)],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
            )
            if proc.returncode != 0:
                raise RuntimeError(
                    f"resvg failed on frame {idx}: {proc.stderr.decode('utf-8', 'replace')}"
                )

        max_workers = min(8, os.cpu_count() or 4)
        with ThreadPoolExecutor(max_workers=max_workers) as pool:
            list(pool.map(build_frame, enumerate(frame_times)))

        palette = td / "palette.png"
        out_gif.parent.mkdir(parents=True, exist_ok=True)
        pass1 = subprocess.run(
            [
                str(FFMPEG_BIN),
                "-y",
                "-framerate",
                str(fps),
                "-i",
                str(td / "frame_%04d.png"),
                "-vf",
                "palettegen=max_colors=64:stats_mode=diff",
                str(palette),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if pass1.returncode != 0:
            raise RuntimeError(f"ffmpeg palettegen failed:\n{pass1.stderr.decode('utf-8', 'replace')}")

        pass2 = subprocess.run(
            [
                str(FFMPEG_BIN),
                "-y",
                "-framerate",
                str(fps),
                "-i",
                str(td / "frame_%04d.png"),
                "-i",
                str(palette),
                "-lavfi",
                "paletteuse=dither=none",
                "-loop",
                "0",
                str(out_gif),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if pass2.returncode != 0:
            raise RuntimeError(f"ffmpeg paletteuse failed:\n{pass2.stderr.decode('utf-8', 'replace')}")


# ---------------------------------------------------------------------------
# 2. before-after-dark.svg & before-after-light.svg (1000x716)
# ---------------------------------------------------------------------------
def render_before_after_svg(
    cap: dict[str, Any], suite: dict[str, Any], theme_name: str
) -> str:
    t = THEMES[theme_name]
    cw = 7.2
    lh = 16.0
    left_cols = 60
    right_cols = 63

    raw_lines = cap["raw_lines"]
    # Find trace start ("---- total_includes_fee stdout ----") and panic start ("thread 'total_includes_fee'")
    head_end = next((i + 1 for i, l in enumerate(raw_lines) if "---- total_includes_fee stdout ----" in l), 15)
    panic_idx = next((i for i, l in enumerate(raw_lines) if "panicked at tests/checkout.rs" in l), len(raw_lines) - 12)
    tail_start = max(head_end, panic_idx - 2)
    elided_count = max(0, tail_start - head_end)

    left_head = raw_lines[:head_end]
    left_tail = raw_lines[tail_start:]

    jl_all_lines = cap["jl_lines"] + [cap["jl_footer"]]
    jl_line_count = len(jl_all_lines)
    cut_pct = (1.0 - cap["jl_total_bytes"] / cap["raw_bytes"]) * 100.0
    ratio_s = fmt_ratio(cap["raw_bytes"], cap["jl_total_bytes"])

    aria = (
        f"The same failing cargo test run twice. Without justless the agent receives "
        f"{cap['raw_line_count']} lines ({cap['raw_bytes']:,} bytes). With justless, "
        f"routed by the hook, it receives {jl_line_count} lines ({cap['jl_total_bytes']:,} bytes, "
        f"{cut_pct:.0f}% smaller): the failing test, its assertion, collapsed empty target, "
        f"exit code 101 and run ID {cap['run_id']}. justless log then replays the original assertion lines exactly."
    )

    parts: list[str] = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1000 716" width="1000" height="716" role="img" aria-label="{esc(aria)}">',
        "<title>Before and after justless: cargo test</title>",
        f"<style>.s{{font-family:{SANS_FONT};font-size:13.5px}}.m{{font-family:{MONO_FONT};font-size:12px;white-space:pre}}</style>",
        '<g xml:space="preserve">',
    ]

    # Left card: Without justless (472 x 708)
    parts.extend(
        card_chrome(
            4,
            4,
            472,
            708,
            "Without justless",
            f" · {cap['raw_line_count']:,} lines · {fmt_kb(cap['raw_bytes'])}",
            t,
        )
    )

    # Pre-compute left rows to position panic highlight rectangle cleanly
    left_render: list[tuple[str, float, str, bool]] = []
    y = 90.0
    for line in left_head:
        for indent, seg in wrap_mono(line, left_cols):
            if y <= 378.0:
                left_render.append(("text", y, seg, False) if indent == 0 else ("cont", y, seg, False))
                y += lh
    elide_y = y + 2.0
    y = elide_y + 26.0
    panic_y_start: float | None = None
    panic_y_end: float | None = None
    for idx_t, line in enumerate(left_tail):
        abs_idx = tail_start + idx_t
        is_panic = panic_idx <= abs_idx <= panic_idx + 3
        for indent, seg in wrap_mono(line, left_cols):
            if y <= 696.0:
                if is_panic:
                    if panic_y_start is None:
                        panic_y_start = y - 11.0
                    panic_y_end = y + 5.0
                left_render.append((f"x:{indent}", y, seg, is_panic))
                y += lh

    if panic_y_start is not None and panic_y_end is not None:
        parts.append(
            f'<rect x="12" y="{panic_y_start:.1f}" width="456" height="{panic_y_end - panic_y_start:.1f}" rx="4" fill="{t["pill_bg"]}"/>'
        )
    parts.append(
        f'<text x="20" y="70" class="m"><tspan fill="{t["accent"]}" font-weight="700">$ </tspan><tspan fill="{t["fg"]}" font-weight="600">cargo test</tspan></text>'
    )
    # Render left head lines
    for kind, ry, seg, _ in left_render:
        if ry < elide_y:
            rx = 20.0 if kind == "text" else 20.0 + 2 * cw
            parts.append(f'<text x="{rx:.1f}" y="{ry:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
    parts.extend(elision_divider(20, 460, elide_y, f"… {elided_count} more lines", t))
    for kind, ry, seg, _ in left_render:
        if ry > elide_y:
            indent = int(kind.split(":")[1]) if kind.startswith("x:") else 0
            rx = 20.0 + indent * cw
            parts.append(f'<text x="{rx:.1f}" y="{ry:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')

    # Right top card: With justless
    right_wrapped: list[tuple[int, str, bool, bool]] = []
    for line in jl_all_lines:
        is_fail = "total_includes_fee" in line
        is_footer = line.startswith("[justless ")
        for indent, seg in wrap_mono(line, right_cols):
            right_wrapped.append((indent, seg, is_fail, is_footer))

    right_h = int(90 + len(right_wrapped) * lh + 12)
    parts.extend(
        card_chrome(
            496,
            4,
            500,
            right_h,
            "With justless",
            f" · {jl_line_count} lines · {cap['jl_total_bytes']:,} B",
            t,
            accent_border=True,
        )
    )
    # Highlight failing line rows
    ry_cur = 90.0
    hl_top: float | None = None
    hl_bot: float | None = None
    for _, _, is_fail, _ in right_wrapped:
        if is_fail:
            if hl_top is None:
                hl_top = ry_cur - 11.0
            hl_bot = ry_cur + 5.0
        ry_cur += lh
    if hl_top is not None and hl_bot is not None:
        parts.append(
            f'<rect x="504" y="{hl_top:.1f}" width="484" height="{hl_bot - hl_top:.1f}" rx="4" fill="{t["pill_bg"]}"/>'
        )
    parts.append(
        f'<text x="512" y="70" class="m"><tspan fill="{t["accent"]}" font-weight="700">$ </tspan><tspan fill="{t["fg"]}" font-weight="600">cargo test</tspan></text>'
    )
    parts.append(f'<rect x="718" y="57" width="268" height="19" rx="9.5" fill="{t["pill_bg"]}"/>')
    parts.append(
        f'<text x="852" y="70.5" class="s" text-anchor="middle" style="font-size:12px" font-weight="600" fill="{t["fg"]}">routed by the hook through justless run</text>'
    )
    ry_cur = 90.0
    for indent, seg, _, is_footer in right_wrapped:
        fill = t["muted"] if is_footer else t["fg"]
        parts.append(
            f'<text x="{512 + indent * cw:.1f}" y="{ry_cur:.1f}" class="m" fill="{fill}">{esc(seg)}</text>'
        )
        ry_cur += lh

    # Right dashed card: Exact on demand
    dash_y = 4 + right_h + 18
    log_cmd = (
        f"justless log {cap['run_id']} --stream stdout "
        f"--from {cap['replay_from']} --to {cap['replay_to']}"
    )
    cmd_segs = wrap_mono(log_cmd, right_cols - 2)
    rep_segs: list[tuple[int, str]] = []
    for ln in cap["replay_lines"]:
        rep_segs.extend(wrap_mono(ln, right_cols))
    dash_h = int(46 + len(cmd_segs) * lh + 8 + len(rep_segs) * lh + 14)

    parts.append(
        f'<rect x="496" y="{dash_y}" width="500" height="{dash_h}" rx="12" fill="{t["recessed_bg"]}" stroke="{t["border"]}" stroke-width="1" stroke-dasharray="5 4"/>'
    )
    parts.append(
        f'<text x="512" y="{dash_y + 23}" class="s"><tspan font-weight="600" fill="{t["accent"]}">Exact on demand</tspan><tspan fill="{t["muted"]}"> · all {fmt_kb(cap["raw_bytes"])} archived in .justless/</tspan></text>'
    )
    cy = dash_y + 46.0
    for idx_c, (indent, seg) in enumerate(cmd_segs):
        if idx_c == 0:
            parts.append(
                f'<text x="512" y="{cy:.1f}" class="m"><tspan fill="{t["accent"]}" font-weight="700">$ </tspan><tspan fill="{t["fg"]}" font-weight="600">{esc(seg)}</tspan></text>'
            )
        else:
            parts.append(
                f'<text x="{512 + (indent + 2) * cw:.1f}" y="{cy:.1f}" class="m" fill="{t["fg"]}" font-weight="600">{esc(seg)}</text>'
            )
        cy += lh
    cy += 4.0
    for indent, seg in rep_segs:
        parts.append(
            f'<text x="{512 + indent * cw:.1f}" y="{cy:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>'
        )
        cy += lh

    # Summary footnotes on bottom right
    ny = dash_y + dash_h + 34
    tot = suite["totals"]
    parts.extend([
        f'<text x="512" y="{ny}" class="s" style="font-size:15px" font-weight="600" fill="{t["fg"]}">In this example: {cap["raw_line_count"]} → {jl_line_count} lines, {cap["raw_bytes"]:,} → {cap["jl_total_bytes"]:,} bytes ({ratio_s})</text>',
        f'<text x="512" y="{ny + 22}" class="s" style="font-size:13px" fill="{t["muted"]}">Same exit code 101 · same assertion · full output kept</text>',
        f'<line x1="512" y1="{ny + 36}" x2="980" y2="{ny + 36}" stroke="{t["border"]}"/>',
        f'<text x="512" y="{ny + 58}" class="s" style="font-size:13px" fill="{t["muted"]}">Fixture: test prints 351 trace lines, then fails; empty unit binary folds.</text>',
        f'<text x="512" y="{ny + 78}" class="s" style="font-size:13px" fill="{t["muted"]}">Across all {tot["count"]} suite workloads: {fmt_kb(tot["raw_bytes"])} → {fmt_kb(tot["justless_bytes"])} ({tot["cut_pct"]:.1f}% · {tot["ratio"]:.1f}×).</text>',
        f'<text x="512" y="{ny + 98}" class="s" style="font-size:13px" fill="{t["muted"]}">The {cap["jl_total_bytes"]:,} bytes include the {cap["jl_footer_bytes"]}-byte footer. Long lines wrap for display.</text>',
    ])

    parts.extend(["</g>", "</svg>\n"])
    return "\n".join(parts)


# ---------------------------------------------------------------------------
# 3. savings-dark.svg & savings-light.svg (900x780, 100% from --suite-json)
# ---------------------------------------------------------------------------
def _bar_path(x0: float, y0: float, w: float, h: float, r: float = 3.5) -> str:
    w = max(w, 2.5)
    rr = min(r, w / 2.0, h / 2.0)
    x1 = x0 + w
    y1 = y0 + h
    return (
        f"M{x0:.2f} {y0:.2f}"
        f"H{x1 - rr:.2f}"
        f"A{rr:.2f} {rr:.2f} 0 0 1 {x1:.2f} {y0 + rr:.2f}"
        f"V{y1 - rr:.2f}"
        f"A{rr:.2f} {rr:.2f} 0 0 1 {x1 - rr:.2f} {y1:.2f}"
        f"H{x0:.2f}z"
    )


def render_savings_svg(suite: dict[str, Any], theme_name: str) -> str:
    t = THEMES[theme_name]
    all_cases = suite["cases"]
    top14 = sorted(all_cases, key=lambda c: c["raw_bytes"], reverse=True)[:14]
    tot = suite["totals"]

    max_kb = max(c["raw_bytes"] / 1024.0 for c in top14)
    # Clean axis step & ceiling
    if max_kb <= 100:
        tick_step = 20
    elif max_kb <= 250:
        tick_step = 50
    elif max_kb <= 450:
        tick_step = 100
    else:
        tick_step = 200
    axis_max_kb = int(math.ceil(max_kb / tick_step) * tick_step)
    ticks = list(range(0, axis_max_kb + 1, tick_step))

    x_axis = 216.0
    x_bar_max = 678.0
    bar_span = x_bar_max - x_axis  # 462px; raw label max +70px -> 748 < pill_x (762)
    pill_x = 762.0
    pill_w = 112.0

    y_top = 122.0
    row_h = 42.0
    y_axis_bot = y_top + len(top14) * row_h

    tot_raw_s = fmt_kb(tot["raw_bytes"])
    tot_jl_s = fmt_kb(tot["justless_bytes"])
    headline = f"{tot_raw_s} → {tot_jl_s} ({tot['cut_pct']:.0f}% smaller · {tot['ratio']:.1f}×)"

    desc_items = "; ".join(
        f"{c['label']}: {fmt_kb(c['raw_bytes'])} raw, {fmt_kb(c['justless_bytes'])} with justless ({c['cut_pct']:.0f}%, {c['ratio']:.1f}x)"
        for c in top14
    )

    parts: list[str] = [
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 900 780" width="900" height="780" role="img" aria-labelledby="t d">',
        f"<title id=\"t\">Bytes an agent received: {esc(tot_raw_s)} raw versus {esc(tot_jl_s)} with justless ({tot['cut_pct']:.1f}% smaller, {tot['ratio']:.1f}x) across {tot['count']} reproducible workloads.</title>",
        f"<desc id=\"d\">Paired horizontal bars for the 14 largest workloads of {tot['count']} reproducible Wave 1-6 benchmarks on a linear 0 to {axis_max_kb} KB axis. {esc(desc_items)}.</desc>",
        f'<rect x="0.5" y="0.5" width="899" height="779" rx="14" fill="{t["canvas_bg"]}" stroke="{t["border"]}"/>',
        f'<text x="28" y="48" font-family="{SANS_FONT}" font-size="25" font-weight="700" fill="{t["fg"]}">{esc(headline)}</text>',
        f'<text x="28" y="74" font-family="{SANS_FONT}" font-size="14.5" fill="{t["muted"]}">Measured output bytes across {tot["count"]} reproducible Wave 1–6 workloads — raw vs. Justless</text>',
        f'<rect x="28" y="92" width="14" height="11" rx="2" fill="{t["raw_bar"]}"/>',
        f'<text x="49" y="102" font-family="{SANS_FONT}" font-size="13" fill="{t["muted"]}">Raw output</text>',
        f'<rect x="142" y="92" width="14" height="11" rx="2" fill="{t["accent_bar"]}"/>',
        f'<text x="163" y="102" font-family="{SANS_FONT}" font-size="13" fill="{t["muted"]}">With justless</text>',
        f'<rect x="268" y="90" width="54" height="15" rx="7.5" fill="{t["pill_bg"]}"/>',
        f'<text x="295" y="101.5" font-family="{SANS_FONT}" font-size="11" font-weight="700" fill="{t["accent"]}" text-anchor="middle">cut · ×</text>',
        f'<text x="330" y="102" font-family="{SANS_FONT}" font-size="13" fill="{t["muted"]}">Reduction &amp; compression ratio</text>',
    ]

    # Grid lines + x-axis labels
    for tick in ticks:
        tx = x_axis + (tick / axis_max_kb) * bar_span
        stroke = t["axis_zero"] if tick == 0 else t["grid"]
        lbl = f"{tick} KB" if tick == axis_max_kb else str(tick)
        parts.append(
            f'<line x1="{tx:.1f}" y1="{y_top:.1f}" x2="{tx:.1f}" y2="{y_axis_bot:.1f}" stroke="{stroke}" stroke-width="1"/>'
        )
        parts.append(
            f'<text x="{tx:.1f}" y="{y_axis_bot + 19:.1f}" font-family="{SANS_FONT}" font-size="12" fill="{t["muted"]}" text-anchor="middle" style="font-variant-numeric:tabular-nums">{lbl}</text>'
        )

    # 14 workload rows
    for idx, c in enumerate(top14):
        ry = y_top + idx * row_h
        raw_kb = c["raw_bytes"] / 1024.0
        jl_kb = c["justless_bytes"] / 1024.0
        w_raw = (raw_kb / axis_max_kb) * bar_span
        w_jl = (jl_kb / axis_max_kb) * bar_span

        # Left label (strictly fits in x=28..208 -> 180px)
        label_main = c["label"]
        if len(label_main) > 22:
            label_main = label_main[:21] + "…"
        sub = c.get("subtitle", "")
        if len(sub) > 24:
            sub = sub[:23] + "…"
        parts.append(
            f'<text x="28" y="{ry + 18:.1f}" font-family="{MONO_FONT}" font-size="12.5" font-weight="600" fill="{t["fg"]}">{esc(label_main)}</text>'
        )
        if sub:
            parts.append(
                f'<text x="28" y="{ry + 33:.1f}" font-family="{SANS_FONT}" font-size="11.5" fill="{t["muted"]}">{esc(sub)}</text>'
            )

        # Paired bars
        parts.append(f'<path d="{_bar_path(x_axis, ry + 6.0, w_raw, 13.0)}" fill="{t["raw_bar"]}"/>')
        parts.append(f'<path d="{_bar_path(x_axis, ry + 21.0, w_jl, 13.0)}" fill="{t["accent_bar"]}"/>')

        # Raw & compacted size labels
        raw_lbl_x = x_axis + max(w_raw, 2.5) + 7.0
        jl_lbl_x = x_axis + max(w_jl, 2.5) + 7.0
        parts.append(
            f'<text x="{raw_lbl_x:.1f}" y="{ry + 16.5:.1f}" font-family="{SANS_FONT}" font-size="12.5" fill="{t["muted"]}" style="font-variant-numeric:tabular-nums">{esc(fmt_kb(c["raw_bytes"]))}</text>'
        )
        parts.append(
            f'<text x="{jl_lbl_x:.1f}" y="{ry + 32.0:.1f}" font-family="{SANS_FONT}" font-size="12.5" font-weight="600" fill="{t["fg"]}" style="font-variant-numeric:tabular-nums">{esc(fmt_kb(c["justless_bytes"]))}</text>'
        )

        # Bold purple multiplier pill
        ratio_str = fmt_ratio(c["raw_bytes"], c["justless_bytes"])
        pill_txt = f"{c['cut_pct']:.0f}% · {ratio_str}"
        parts.append(
            f'<rect x="{pill_x:.1f}" y="{ry + 9.0:.1f}" width="{pill_w:.1f}" height="24" rx="12" fill="{t["pill_bg"]}"/>'
        )
        parts.append(
            f'<text x="{pill_x + pill_w / 2.0:.1f}" y="{ry + 25.0:.1f}" font-family="{SANS_FONT}" font-size="12" font-weight="700" fill="{t["accent"]}" text-anchor="middle" style="font-variant-numeric:tabular-nums">{esc(pill_txt)}</text>'
        )

    parts.append(
        f'<text x="28" y="758" font-family="{SANS_FONT}" font-size="12.5" fill="{t["muted"]}">Top 14 of {tot["count"]} reproducible workloads from scripts/suite_benchmark.py · totals cover all {tot["count"]} · sorted by raw size.</text>'
    )
    parts.append("</svg>\n")
    return "\n".join(parts)


# ---------------------------------------------------------------------------
# 4. outline-dark.svg & outline-light.svg (1000xH)
# ---------------------------------------------------------------------------
def render_outline_svg(cap: dict[str, Any], theme_name: str) -> str:
    t = THEMES[theme_name]
    cw = 7.2
    lh = 16.0
    left_cols = 58
    right_cols = 62

    out_lines = cap["outline_lines"]
    fail_idx = cap["fail_entry_idx"]
    # Show top rows (revision + #1..#10 covering h1, h2, table, h3) and a contiguous window
    # starting at a `code` entry (if present near Failures) through Failures + next --after.
    head_cut = min(11, max(8, fail_idx - 8))
    code_idx = next(
        (i for i in range(fail_idx - 1, head_cut - 1, -1) if " code " in out_lines[i]),
        max(head_cut, fail_idx - 2),
    )
    tail_from = max(head_cut, min(fail_idx - 2, code_idx))
    omitted_right = max(0, tail_from - head_cut)

    right_head_segs: list[tuple[int, str]] = []
    for ln in out_lines[:head_cut]:
        right_head_segs.extend(wrap_mono(ln, right_cols))

    right_tail_segs: list[tuple[int, str, bool]] = []
    # Include rows from tail_from through Failures + 2 context lines, plus final next --after line
    tail_end = min(len(out_lines), fail_idx + 3)
    tail_slice = out_lines[tail_from:tail_end]
    if out_lines[-1] not in tail_slice:
        tail_slice = tail_slice + [out_lines[-1]]
    for ln in tail_slice:
        is_hl = ln == out_lines[fail_idx]
        for indent, seg in wrap_mono(ln, right_cols):
            right_tail_segs.append((indent, seg, is_hl))

    # Right top card height
    y_r_after_head = 90.0 + len(right_head_segs) * lh
    y_r_elide = y_r_after_head + 6.0 if omitted_right > 0 else y_r_after_head
    y_r_tail_start = (y_r_elide + 26.0) if omitted_right > 0 else y_r_after_head
    right_top_h = int(y_r_tail_start + len(right_tail_segs) * lh + 10)

    # Right dashed expand card
    dash_y = 4 + right_top_h + 18
    exp_cmd = f"justless expand docs/reference.md '{cap['anchor']}'"
    exp_cmd_segs = wrap_mono(exp_cmd, right_cols - 2)
    exp_body_segs: list[tuple[int, str]] = []
    for ln in cap["expand_lines"]:
        exp_body_segs.extend(wrap_mono(ln, right_cols))
    dash_h = int(46 + len(exp_cmd_segs) * lh + 8 + len(exp_body_segs) * lh + 14)

    notes_y = dash_y + dash_h + 36
    total_h = max(920, notes_y + 92)
    left_h = total_h - 16

    # Left card content: top lines of docs/reference.md + elision + highlighted Failures section + trailing elision
    raw_lines = cap["raw_lines"]
    fail_s = cap["fail_start"] - 1
    fail_e = cap["fail_end"]
    fail_block_segs: list[tuple[int, str]] = []
    for ln in raw_lines[fail_s:fail_e]:
        fail_block_segs.extend(wrap_mono(ln, left_cols))

    # Compute how many head rows fit nicely above Failures + 2 elision pills
    avail_left_head_rows = max(8, int((left_h - 120 - 64 - len(fail_block_segs) * lh) / lh))
    left_head_segs: list[tuple[int, str]] = []
    raw_head_lines_used = 0
    for ln in raw_lines[:fail_s]:
        wrapped = wrap_mono(ln, left_cols)
        if len(left_head_segs) + len(wrapped) > avail_left_head_rows:
            break
        left_head_segs.extend(wrapped)
        raw_head_lines_used += 1

    elide_mid = max(0, fail_s - raw_head_lines_used)
    elide_end = max(0, len(raw_lines) - fail_e)

    ratio_s = fmt_ratio(cap["raw_bytes"], cap["outline_bytes"])
    aria = (
        f"Reading a big file. Without justless, cat docs/reference.md prints all {len(raw_lines):,} lines "
        f"({cap['raw_bytes']:,} bytes). With justless, justless read docs/reference.md --outline returns "
        f"{len(out_lines)} lines ({cap['outline_bytes']:,} bytes) using compact h1, h2, h3, table and code anchors. "
        f"justless expand docs/reference.md '{cap['anchor']}' then returns the Failures section, "
        f"lines {cap['fail_start']} to {cap['fail_end']}, byte for byte ({cap['expand_bytes']:,} bytes). "
        f"The outline is {ratio_s} smaller than the file."
    )

    parts: list[str] = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1000 {total_h}" width="1000" height="{total_h}" role="img" aria-label="{esc(aria)}">',
        "<title>Before and after justless: reading a big file</title>",
        f"<style>.s{{font-family:{SANS_FONT};font-size:13.5px}}.m,.r text{{font-family:{MONO_FONT};font-size:12px;white-space:pre}}</style>",
        '<g xml:space="preserve">',
    ]

    # Left card
    parts.extend(
        card_chrome(
            4,
            4,
            472,
            left_h,
            "Without justless",
            f" · {len(raw_lines):,} lines · {fmt_kb(cap['raw_bytes'])}",
            t,
        )
    )
    parts.append(
        f'<text x="20" y="70" class="m"><tspan fill="{t["accent"]}" font-weight="700">$ </tspan><tspan fill="{t["fg"]}" font-weight="600">cat docs/reference.md</tspan></text>'
    )
    ly = 90.0
    for indent, seg in left_head_segs:
        parts.append(f'<text x="{20 + indent * cw:.1f}" y="{ly:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
        ly += lh
    ly += 4.0
    parts.extend(elision_divider(20, 460, ly, f"… {elide_mid:,} more lines", t))
    ly += 26.0
    fb_top = ly - 11.0
    fb_h = len(fail_block_segs) * lh + 4.0
    parts.append(f'<rect x="12" y="{fb_top:.1f}" width="456" height="{fb_h:.1f}" rx="4" fill="{t["pill_bg"]}"/>')
    for indent, seg in fail_block_segs:
        parts.append(f'<text x="{20 + indent * cw:.1f}" y="{ly:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
        ly += lh
    ly += 6.0
    parts.extend(elision_divider(20, 460, ly, f"… {elide_end:,} more lines", t))

    # Right top card: Outline
    parts.extend(
        card_chrome(
            496,
            4,
            500,
            right_top_h,
            "With justless",
            f" · {len(out_lines)} lines · {fmt_kb_header(cap['outline_bytes'])}",
            t,
            accent_border=True,
        )
    )
    parts.append(
        f'<text x="512" y="70" class="m"><tspan fill="{t["accent"]}" font-weight="700">$ </tspan><tspan fill="{t["fg"]}" font-weight="600">justless read docs/reference.md --outline</tspan></text>'
    )
    ry = 90.0
    for indent, seg in right_head_segs:
        parts.append(f'<text x="{512 + indent * cw:.1f}" y="{ry:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
        ry += lh
    if omitted_right > 0:
        parts.extend(elision_divider(512, 980, y_r_elide, f"… {omitted_right} more lines", t))
        ry = y_r_tail_start

    # Highlight Failures row
    hl_top: float | None = None
    hl_bot: float | None = None
    tmp_y = ry
    for _, _, is_hl in right_tail_segs:
        if is_hl:
            if hl_top is None:
                hl_top = tmp_y - 11.0
            hl_bot = tmp_y + 5.0
        tmp_y += lh
    if hl_top is not None and hl_bot is not None:
        parts.append(
            f'<rect x="504" y="{hl_top:.1f}" width="484" height="{hl_bot - hl_top:.1f}" rx="4" fill="{t["pill_bg"]}"/>'
        )
    for indent, seg, _ in right_tail_segs:
        parts.append(f'<text x="{512 + indent * cw:.1f}" y="{ry:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
        ry += lh

    # Right dashed expand card
    parts.append(
        f'<rect x="496" y="{dash_y}" width="500" height="{dash_h}" rx="12" fill="{t["recessed_bg"]}" stroke="{t["border"]}" stroke-width="1" stroke-dasharray="5 4"/>'
    )
    parts.append(
        f'<text x="512" y="{dash_y + 23}" class="s"><tspan font-weight="600" fill="{t["accent"]}">Exact on demand</tspan><tspan fill="{t["muted"]}"> · one section, byte for byte</tspan></text>'
    )
    ey = dash_y + 46.0
    for idx_c, (indent, seg) in enumerate(exp_cmd_segs):
        if idx_c == 0:
            parts.append(
                f'<text x="512" y="{ey:.1f}" class="m"><tspan fill="{t["accent"]}" font-weight="700">$ </tspan><tspan fill="{t["fg"]}" font-weight="600">{esc(seg)}</tspan></text>'
            )
        else:
            parts.append(
                f'<text x="{512 + (indent + 2) * cw:.1f}" y="{ey:.1f}" class="m" fill="{t["fg"]}" font-weight="600">{esc(seg)}</text>'
            )
        ey += lh
    ey += 4.0
    for indent, seg in exp_body_segs:
        parts.append(f'<text x="{512 + indent * cw:.1f}" y="{ey:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
        ey += lh

    # Summary notes
    parts.extend([
        f'<text x="512" y="{notes_y}" class="s" style="font-size:15px" font-weight="600" fill="{t["fg"]}">In this example: {len(raw_lines):,} → {len(out_lines)} lines, {cap["raw_bytes"]:,} → {cap["outline_bytes"]:,} bytes ({ratio_s})</text>',
        f'<text x="512" y="{notes_y + 22}" class="s" style="font-size:13px" fill="{t["muted"]}">Then one section, lines {cap["fail_start"]}–{cap["fail_end"]}: {cap["expand_bytes"]:,} bytes, exactly as in the file</text>',
        f'<line x1="512" y1="{notes_y + 36}" x2="980" y2="{notes_y + 36}" stroke="{t["border"]}"/>',
        f'<text x="512" y="{notes_y + 58}" class="s" style="font-size:13px" fill="{t["muted"]}">Structured anchors for h1–h6 headings, tables and fenced code blocks.</text>',
        f'<text x="512" y="{notes_y + 78}" class="s" style="font-size:13px" fill="{t["muted"]}">Gaps mark lines left out of this picture. Long lines wrap for display.</text>',
    ])

    parts.extend(["</g>", "</svg>\n"])
    return "\n".join(parts)


# ---------------------------------------------------------------------------
# 5. search-dark.svg & search-light.svg (1000xH)
# ---------------------------------------------------------------------------
def render_search_svg(cap: dict[str, Any], theme_name: str) -> str:
    t = THEMES[theme_name]
    cw = 7.2
    lh = 16.0
    left_cols = 58
    right_cols = 62

    jl_lines = cap["jl_lines"]
    # Show header + top 3 file blocks (typically 1 + 3*4 = 13 lines), elision, then overflow summary + footer
    head_count = min(13, max(5, len(jl_lines) - 2))
    overflow_line = jl_lines[-1] if len(jl_lines) > head_count else ""
    omitted_right = max(0, len(jl_lines) - head_count - (1 if overflow_line else 0))

    r_head_segs: list[tuple[int, str, bool]] = []
    # Highlight top file block (lines 1..4)
    for idx, ln in enumerate(jl_lines[:head_count]):
        is_top = 1 <= idx <= 4
        for indent, seg in wrap_mono(ln, right_cols):
            r_head_segs.append((indent, seg, is_top))

    r_tail_segs: list[tuple[int, str, bool]] = []
    if overflow_line:
        for indent, seg in wrap_mono(overflow_line, right_cols):
            r_tail_segs.append((indent, seg, False))
    r_tail_segs.append((0, "", False))
    for indent, seg in wrap_mono(cap["jl_footer"], right_cols):
        r_tail_segs.append((indent, seg, True))

    y_r_elide = 90.0 + len(r_head_segs) * lh + 6.0
    y_r_tail = y_r_elide + 26.0 if omitted_right > 0 else 90.0 + len(r_head_segs) * lh
    right_top_h = int(y_r_tail + len(r_tail_segs) * lh + 10)

    # Dashed replay card
    dash_y = 4 + right_top_h + 18
    log_cmd = (
        f"justless log {cap['run_id']} --stream stdout "
        f"--from {cap['replay_from']} --to {cap['replay_to']}"
    )
    cmd_segs = wrap_mono(log_cmd, right_cols - 2)
    rep_segs: list[tuple[int, str]] = []
    for ln in cap["replay_lines"]:
        rep_segs.extend(wrap_mono(ln, right_cols))
    dash_h = int(46 + len(cmd_segs) * lh + 8 + len(rep_segs) * lh + 14)

    notes_y = dash_y + dash_h + 36
    total_h = max(780, notes_y + 114)
    left_h = total_h - 16

    # Left card: head lines, elision, highlighted replayed lines at their exact index, elision, tail
    raw_lines = cap["raw_lines"]
    r_from0 = cap["replay_from"] - 1
    r_to0 = cap["replay_to"]

    mid_segs: list[tuple[int, str]] = []
    for ln in raw_lines[r_from0:r_to0]:
        mid_segs.extend(wrap_mono(ln, left_cols))
    tail_raw = raw_lines[-3:] if len(raw_lines) > r_to0 + 3 else []
    tail_segs: list[tuple[int, str]] = []
    for ln in tail_raw:
        tail_segs.extend(wrap_mono(ln, left_cols))

    avail_head_rows = max(
        6,
        int((left_h - 106 - 68 - len(mid_segs) * lh - len(tail_segs) * lh) / lh),
    )
    head_segs: list[tuple[int, str]] = []
    head_raw_used = 0
    for ln in raw_lines[:r_from0]:
        w = wrap_mono(ln, left_cols)
        if len(head_segs) + len(w) > avail_head_rows:
            break
        head_segs.extend(w)
        head_raw_used += 1

    elide_1 = max(0, r_from0 - head_raw_used)
    elide_2 = max(0, len(raw_lines) - r_to0 - len(tail_raw))

    jl_total_lines = len(jl_lines) + 2
    ratio_s = fmt_ratio(cap["raw_bytes"], cap["jl_bytes"])
    aria = (
        f"A search with lots of output. Without justless, rg -n \"fn \" src prints {len(raw_lines):,} lines "
        f"({cap['raw_bytes']:,} bytes). With justless run, the agent receives {jl_total_lines} lines "
        f"({cap['jl_bytes']:,} bytes): grouped by file with match counts, sample lines and archived line ranges. "
        f"justless log {cap['run_id']} --stream stdout --from {cap['replay_from']} --to {cap['replay_to']} "
        f"then returns those lines verbatim. {ratio_s} fewer bytes."
    )

    parts: list[str] = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1000 {total_h}" width="1000" height="{total_h}" role="img" aria-label="{esc(aria)}">',
        "<title>Before and after justless: a search with lots of output</title>",
        f"<style>.s{{font-family:{SANS_FONT};font-size:13.5px}}.m,.r text{{font-family:{MONO_FONT};font-size:12px;white-space:pre}}</style>",
        '<g xml:space="preserve">',
    ]

    # Left card
    parts.extend(
        card_chrome(
            4,
            4,
            472,
            left_h,
            "Without justless",
            f" · {len(raw_lines):,} lines · {fmt_kb(cap['raw_bytes'])}",
            t,
        )
    )
    parts.append(
        f'<text x="20" y="70" class="m"><tspan fill="{t["accent"]}" font-weight="700">$ </tspan><tspan fill="{t["fg"]}" font-weight="600">rg -n &quot;fn &quot; src</tspan></text>'
    )
    ly = 90.0
    for indent, seg in head_segs:
        parts.append(f'<text x="{20 + indent * cw:.1f}" y="{ly:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
        ly += lh
    if elide_1 > 0:
        ly += 4.0
        parts.extend(elision_divider(20, 460, ly, f"… {elide_1:,} more lines", t))
        ly += 26.0
    m_top = ly - 11.0
    m_h = len(mid_segs) * lh + 4.0
    parts.append(f'<rect x="12" y="{m_top:.1f}" width="456" height="{m_h:.1f}" rx="4" fill="{t["pill_bg"]}"/>')
    for indent, seg in mid_segs:
        parts.append(f'<text x="{20 + indent * cw:.1f}" y="{ly:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
        ly += lh
    if elide_2 > 0:
        ly += 6.0
        parts.extend(elision_divider(20, 460, ly, f"… {elide_2:,} more lines", t))
        ly += 26.0
    for indent, seg in tail_segs:
        if ly <= left_h - 8:
            parts.append(f'<text x="{20 + indent * cw:.1f}" y="{ly:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
            ly += lh

    # Right top card
    parts.extend(
        card_chrome(
            496,
            4,
            500,
            right_top_h,
            "With justless",
            f" · {jl_total_lines} lines · {fmt_kb_header(cap['jl_bytes'])}",
            t,
            accent_border=True,
        )
    )
    parts.append(
        f'<text x="512" y="70" class="m"><tspan fill="{t["accent"]}" font-weight="700">$ </tspan><tspan fill="{t["fg"]}" font-weight="600">justless run -- rg -n &quot;fn &quot; src</tspan></text>'
    )
    ry = 90.0
    hl_top: float | None = None
    hl_bot: float | None = None
    for _, _, is_top in r_head_segs:
        if is_top:
            if hl_top is None:
                hl_top = ry - 11.0
            hl_bot = ry + 5.0
        ry += lh
    if hl_top is not None and hl_bot is not None:
        parts.append(
            f'<rect x="504" y="{hl_top:.1f}" width="484" height="{hl_bot - hl_top:.1f}" rx="4" fill="{t["pill_bg"]}"/>'
        )
    ry = 90.0
    for indent, seg, _ in r_head_segs:
        parts.append(f'<text x="{512 + indent * cw:.1f}" y="{ry:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
        ry += lh
    if omitted_right > 0:
        parts.extend(elision_divider(512, 980, y_r_elide, f"… {omitted_right} more lines", t))
        ry = y_r_tail
    for indent, seg, is_footer in r_tail_segs:
        fill = t["muted"] if is_footer else t["fg"]
        if seg:
            parts.append(f'<text x="{512 + indent * cw:.1f}" y="{ry:.1f}" class="m" fill="{fill}">{esc(seg)}</text>')
        ry += lh

    # Right dashed replay card
    parts.append(
        f'<rect x="496" y="{dash_y}" width="500" height="{dash_h}" rx="12" fill="{t["recessed_bg"]}" stroke="{t["border"]}" stroke-width="1" stroke-dasharray="5 4"/>'
    )
    parts.append(
        f'<text x="512" y="{dash_y + 23}" class="s"><tspan font-weight="600" fill="{t["accent"]}">Exact on demand</tspan><tspan fill="{t["muted"]}"> · all {fmt_kb(cap["raw_bytes"])} archived in .justless/</tspan></text>'
    )
    cy = dash_y + 46.0
    for idx_c, (indent, seg) in enumerate(cmd_segs):
        if idx_c == 0:
            parts.append(
                f'<text x="512" y="{cy:.1f}" class="m"><tspan fill="{t["accent"]}" font-weight="700">$ </tspan><tspan fill="{t["fg"]}" font-weight="600">{esc(seg)}</tspan></text>'
            )
        else:
            parts.append(
                f'<text x="{512 + (indent + 2) * cw:.1f}" y="{cy:.1f}" class="m" fill="{t["fg"]}" font-weight="600">{esc(seg)}</text>'
            )
        cy += lh
    cy += 4.0
    for indent, seg in rep_segs:
        parts.append(f'<text x="{512 + indent * cw:.1f}" y="{cy:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
        cy += lh

    parts.extend([
        f'<text x="512" y="{notes_y}" class="s" style="font-size:15px" font-weight="600" fill="{t["fg"]}">In this example: {len(raw_lines):,} → {jl_total_lines} lines, {cap["raw_bytes"]:,} → {cap["jl_bytes"]:,} bytes ({ratio_s})</text>',
        f'<text x="512" y="{notes_y + 22}" class="s" style="font-size:13px" fill="{t["muted"]}">Grouped by file; every group points at its archived lines</text>',
        f'<line x1="512" y1="{notes_y + 36}" x2="980" y2="{notes_y + 36}" stroke="{t["border"]}"/>',
        f'<text x="512" y="{notes_y + 58}" class="s" style="font-size:13px" fill="{t["muted"]}">Line {cap["replay_to"]} was not in the report; justless log returns it verbatim.</text>',
        f'<text x="512" y="{notes_y + 78}" class="s" style="font-size:13px" fill="{t["muted"]}">Gaps mark lines left out of this picture, {omitted_right} on the right.</text>',
        f'<text x="512" y="{notes_y + 98}" class="s" style="font-size:13px" fill="{t["muted"]}">The {cap["jl_bytes"]:,} bytes include the {cap["jl_footer_bytes"]}-byte footer. Long lines wrap for display.</text>',
    ])

    parts.extend(["</g>", "</svg>\n"])
    return "\n".join(parts)


# ---------------------------------------------------------------------------
# 6. query-dark.svg & query-light.svg (1000xH)
# ---------------------------------------------------------------------------
def render_query_svg(cap: dict[str, Any], theme_name: str) -> str:
    t = THEMES[theme_name]
    cw = 7.2
    lh = 16.0
    left_cols = 58
    right_cols = 62

    # Wrap right side (`justless query would_route`)
    r_segs: list[tuple[int, str, bool]] = []
    for idx, ln in enumerate(cap["jl_lines"]):
        # Highlight definition + callers block
        is_hl = idx == 0 or ln.lstrip().startswith(("import from", "call from", "reference from"))
        for indent, seg in wrap_mono(ln, right_cols):
            r_segs.append((indent, seg, is_hl))

    right_h = int(90 + len(r_segs) * lh + 14)

    # Wrap left side: rg -n -w would_route src (up to 6 matches) + cat src/agents.rs head + highlighted def
    rg_segs: list[tuple[int, str]] = []
    for ln in cap["rg_lines"][:5]:
        rg_segs.extend(wrap_mono(ln, left_cols))
    rg_extra = max(0, len(cap["rg_lines"]) - 5)

    agents_lines = cap["agents_lines"]
    def_s = cap["def_start"] - 1
    def_e = cap["def_end"]
    head_lines = agents_lines[:10]
    cat_head_segs: list[tuple[int, str]] = []
    for ln in head_lines:
        cat_head_segs.extend(wrap_mono(ln, left_cols))

    def_segs: list[tuple[int, str]] = []
    for ln in agents_lines[def_s:def_e]:
        def_segs.extend(wrap_mono(ln, left_cols))

    elide_before = max(0, def_s - len(head_lines))
    elide_after = max(0, len(agents_lines) - def_e)

    # Compute exact left height
    left_y_calc = 90.0 + len(rg_segs) * lh
    if rg_extra > 0:
        left_y_calc += lh
    left_y_calc += 26.0  # `$ cat src/agents.rs`
    left_y_calc += len(cat_head_segs) * lh + 30.0  # elision 1
    left_y_calc += len(def_segs) * lh + 32.0  # elision 2
    left_h = int(left_y_calc + 16)

    notes_y = 4 + right_h + 42
    total_h = max(left_h + 12, notes_y + 116)
    left_h = total_h - 12

    ratio_s = fmt_ratio(cap["raw_bytes"], cap["jl_bytes"])
    aria = (
        f"Where is would_route defined and what calls it? Without justless, the agent runs "
        f"rg -n -w would_route src and reads src/agents.rs: {cap['raw_line_count']:,} lines and "
        f"{cap['raw_bytes']:,} bytes in all. With justless, justless query would_route prints "
        f"{len(cap['jl_lines'])} lines ({cap['jl_bytes']:,} bytes): definition at "
        f"src/agents.rs:{cap['def_start']}-{cap['def_end']}, grouped callers, deduplicated "
        f"outgoing dependencies with (from :LINE), and related tests. {ratio_s} fewer bytes."
    )

    parts: list[str] = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1000 {total_h}" width="1000" height="{total_h}" role="img" aria-label="{esc(aria)}">',
        "<title>Before and after justless: where is a function and what calls it</title>",
        f"<style>.s{{font-family:{SANS_FONT};font-size:13.5px}}.m,.r text{{font-family:{MONO_FONT};font-size:12px;white-space:pre}}</style>",
        '<g xml:space="preserve">',
    ]

    # Left card
    parts.extend(
        card_chrome(
            4,
            4,
            472,
            left_h,
            "Without justless",
            f" · {cap['raw_line_count']:,} lines · {fmt_kb(cap['raw_bytes'])}",
            t,
        )
    )
    parts.append(
        f'<text x="20" y="70" class="m"><tspan fill="{t["accent"]}" font-weight="700">$ </tspan><tspan fill="{t["fg"]}" font-weight="600">rg -n -w would_route src</tspan></text>'
    )
    ly = 90.0
    for indent, seg in rg_segs:
        parts.append(f'<text x="{20 + indent * cw:.1f}" y="{ly:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
        ly += lh
    if rg_extra > 0:
        parts.append(f'<text x="20" y="{ly:.1f}" class="m" fill="{t["muted"]}">… +{rg_extra} more matching lines</text>')
        ly += lh

    ly += 10.0
    parts.append(
        f'<text x="20" y="{ly:.1f}" class="m"><tspan fill="{t["accent"]}" font-weight="700">$ </tspan><tspan fill="{t["fg"]}" font-weight="600">cat src/agents.rs</tspan></text>'
    )
    ly += 20.0
    for indent, seg in cat_head_segs:
        parts.append(f'<text x="{20 + indent * cw:.1f}" y="{ly:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
        ly += lh
    ly += 4.0
    parts.extend(elision_divider(20, 460, ly, f"… {elide_before:,} more lines", t))
    ly += 26.0
    def_top = ly - 11.0
    def_h = len(def_segs) * lh + 4.0
    parts.append(f'<rect x="12" y="{def_top:.1f}" width="456" height="{def_h:.1f}" rx="4" fill="{t["pill_bg"]}"/>')
    for indent, seg in def_segs:
        parts.append(f'<text x="{20 + indent * cw:.1f}" y="{ly:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
        ly += lh
    ly += 6.0
    parts.extend(elision_divider(20, 460, ly, f"… {elide_after:,} more lines", t))

    # Right card
    parts.extend(
        card_chrome(
            496,
            4,
            500,
            right_h,
            "With justless",
            f" · {len(cap['jl_lines'])} lines · {fmt_kb_header(cap['jl_bytes'])}",
            t,
            accent_border=True,
        )
    )
    parts.append(
        f'<text x="512" y="70" class="m"><tspan fill="{t["accent"]}" font-weight="700">$ </tspan><tspan fill="{t["fg"]}" font-weight="600">justless query would_route</tspan></text>'
    )
    ry = 90.0
    hl_top: float | None = None
    hl_bot: float | None = None
    for _, _, is_hl in r_segs:
        if is_hl:
            if hl_top is None:
                hl_top = ry - 11.0
            hl_bot = ry + 5.0
        ry += lh
    if hl_top is not None and hl_bot is not None:
        parts.append(
            f'<rect x="504" y="{hl_top:.1f}" width="484" height="{hl_bot - hl_top:.1f}" rx="4" fill="{t["pill_bg"]}"/>'
        )
    ry = 90.0
    for indent, seg, _ in r_segs:
        parts.append(f'<text x="{512 + indent * cw:.1f}" y="{ry:.1f}" class="m" fill="{t["fg"]}">{esc(seg)}</text>')
        ry += lh

    parts.extend([
        f'<text x="512" y="{notes_y}" class="s" style="font-size:15px" font-weight="600" fill="{t["fg"]}">In this example: {cap["raw_line_count"]:,} → {len(cap["jl_lines"])} lines, {cap["raw_bytes"]:,} → {cap["jl_bytes"]:,} bytes ({ratio_s})</text>',
        f'<text x="512" y="{notes_y + 22}" class="s" style="font-size:13px" fill="{t["muted"]}">Definition, grouped callers, deduplicated (from :LINE) edges and tests</text>',
        f'<line x1="512" y1="{notes_y + 36}" x2="980" y2="{notes_y + 36}" stroke="{t["border"]}"/>',
        f'<text x="512" y="{notes_y + 58}" class="s" style="font-size:13px" fill="{t["muted"]}">Left: rg -n -w would_route src, then reading src/agents.rs in full.</text>',
        f'<text x="512" y="{notes_y + 78}" class="s" style="font-size:13px" fill="{t["muted"]}">Right: answered from the local code graph in .justless/ (zero reads).</text>',
        f'<text x="512" y="{notes_y + 98}" class="s" style="font-size:13px" fill="{t["muted"]}">Long lines wrap for display.</text>',
    ])

    parts.extend(["</g>", "</svg>\n"])
    return "\n".join(parts)


# ---------------------------------------------------------------------------
# 7. statusline-dark.svg & statusline-light.svg (1000x132)
# ---------------------------------------------------------------------------
def render_statusline_svg(theme_name: str) -> str:
    t = THEMES[theme_name]
    aria = (
        "Claude Code's status line in a real session, with justless installed: "
        "justless, initially about 2.5 million tokens saved, net about 1.8 million "
        "tokens saved (72%) this session, 519 runs, 29 reads."
    )
    return "\n".join([
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1000 132" width="1000" height="132" role="img" aria-label="{esc(aria)}">',
        "<title>justless in Claude Code's status line</title>",
        f"<style>.s{{font-family:{SANS_FONT};font-size:13.5px}}.m{{font-family:{MONO_FONT};font-size:13px;white-space:pre}}</style>",
        '<g xml:space="preserve">',
        f'<rect x="4" y="4" width="992" height="124" rx="12" fill="{t["card_bg"]}" stroke="{t["border"]}" stroke-width="1"/>',
        f'<circle cx="24" cy="24" r="5.5" fill="{t["dot"]}"/>',
        f'<circle cx="40" cy="24" r="5.5" fill="{t["dot"]}"/>',
        f'<circle cx="56" cy="24" r="5.5" fill="{t["dot"]}"/>',
        f'<text x="80" y="29" class="s"><tspan font-weight="600" fill="{t["fg"]}">Claude Code</tspan><tspan fill="{t["muted"]}"> · status line with justless init --statusline</tspan></text>',
        f'<line x1="4" y1="44" x2="996" y2="44" stroke="{t["border"]}"/>',
        f'<text x="20" y="72" class="m"><tspan fill="{t["accent"]}" font-weight="700">&gt; </tspan></text>',
        f'<rect x="38" y="60" width="8" height="16" fill="{t["accent"]}" opacity="0.7"/>',
        f'<line x1="12" y1="88" x2="988" y2="88" stroke="{t["border"]}"/>',
        f'<text x="20" y="112" class="m"><tspan fill="{t["accent"]}" font-weight="700">justless</tspan>'
        f'<tspan fill="{t["muted"]}"> · initially </tspan>'
        f'<tspan fill="{t["fg"]}" font-weight="600">≈2.5M tokens saved</tspan>'
        f'<tspan fill="{t["muted"]}"> · net </tspan>'
        f'<tspan fill="{t["fg"]}" font-weight="600">≈1.8M tokens saved </tspan>'
        f'<tspan fill="{t["accent"]}" font-weight="700">(72%)</tspan>'
        f'<tspan fill="{t["muted"]}"> this session · 519 runs · 29 reads</tspan></text>',
        "</g>",
        "</svg>\n",
    ])


# ---------------------------------------------------------------------------
# 8. how-it-works-dark.svg & how-it-works-light.svg (1200x550)
# ---------------------------------------------------------------------------
def render_how_it_works_svg(theme_name: str) -> str:
    t = THEMES[theme_name]
    agents_col1 = [
        "Claude Code",
        "Codex",
        "Antigravity",
        "Cursor",
        "Gemini CLI",
        "Copilot CLI",
    ]
    agents_col2 = [
        "OpenCode",
        "Windsurf",
        "Cline",
        "Roo Code",
        "Kiro CLI",
        "Kilo Code",
    ]
    agent_els = [
        f'<text x="38" y="{126 + i * 39}" font-family="{SANS_FONT}" font-size="12.5" fill="{t["muted"]}" text-anchor="start">{name}</text>'
        for i, name in enumerate(agents_col1)
    ] + [
        f'<text x="142" y="{126 + i * 39}" font-family="{SANS_FONT}" font-size="12.5" fill="{t["muted"]}" text-anchor="start">{name}</text>'
        for i, name in enumerate(agents_col2)
    ]
    desc = (
        "How Justless works: 12 supported coding agents (Claude Code, Codex, Antigravity, "
        "Cursor, Gemini CLI, Copilot CLI, OpenCode, Windsurf, Cline, Roo Code, Kiro CLI, "
        "and Kilo Code) connect via hooks, MCP, or CLI to the local offline justless binary, "
        "which compacts commands (tests, lint, BuildKit, K8s/Cloud, CSV, diffs, JSON), code "
        "(26 graph langs, 48+ outline formats, map --html/--explain, lookup --graph, AST rename), "
        "and history (duplicates, semantic cues, multi-provider cache-safe --compact-live proxy "
        "with prefix-drift detection). Full output is archived locally in .justless/ (runs/, "
        "index.bin, observations/) and replayable byte-for-byte on demand via log, expand, and recall."
    )
    return "\n".join([
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 550" width="1200" height="550" role="img" aria-labelledby="t d">',
        '<title id="t">How Justless works</title>',
        f'<desc id="d">{esc(desc)}</desc>',
        "<defs>",
        f'  <marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0 0 L10 5 L0 10 z" fill="{t["raw_bar"]}"/></marker>',
        f'  <marker id="arrow-accent" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0 0 L10 5 L0 10 z" fill="{t["accent"]}"/></marker>',
        "</defs>",
        # Left card: Your agent (12 supported agents in a clean 2x6 grid)
        f'<rect x="20" y="40" width="224" height="340" rx="14" fill="{t["card_bg"]}" stroke="{t["border"]}"/>',
        f'<text x="38" y="77" font-family="{SANS_FONT}" font-size="16" font-weight="600" fill="{t["fg"]}" text-anchor="start">Your agent</text>',
        f'<rect x="164" y="61" width="64" height="22" rx="11" fill="{t["pill_bg"]}"/>',
        f'<text x="196" y="76" font-family="{SANS_FONT}" font-size="11" font-weight="600" fill="{t["accent"]}" text-anchor="middle">12 agents</text>',
        f'<line x1="36" y1="96" x2="228" y2="96" stroke="{t["border"]}"/>',
        *agent_els,
        # Middle card: justless
        f'<rect x="338" y="40" width="570" height="340" rx="16" fill="{t["card_bg"]}" stroke="{t["accent"]}" stroke-width="1.5"/>',
        f'<text x="362" y="78" font-family="{MONO_FONT}" font-size="20" font-weight="700" fill="{t["fg"]}" text-anchor="start">justless</text>',
        f'<text x="884" y="77" font-family="{SANS_FONT}" font-size="12" fill="{t["muted"]}" text-anchor="end">local · offline · one binary</text>',
        # Commands row
        f'<rect x="362" y="128" width="96" height="28" rx="14" fill="{t["pill_bg"]}"/>',
        f'<text x="410" y="147" font-family="{SANS_FONT}" font-size="13" font-weight="600" fill="{t["accent"]}" text-anchor="middle">Commands</text>',
        f'<text x="474" y="140" font-family="{SANS_FONT}" font-size="13.5" font-weight="500" fill="{t["fg"]}" text-anchor="start">tests · lint · BuildKit · K8s/Cloud · CSV · diffs · JSON</text>',
        f'<text x="474" y="162" font-family="{SANS_FONT}" font-size="12.5" fill="{t["muted"]}" text-anchor="start">grouped by failure, rule, hunk, schema and log template</text>',
        f'<line x1="358" y1="194" x2="888" y2="194" stroke="{t["border"]}"/>',
        # Code row
        f'<rect x="362" y="218" width="96" height="28" rx="14" fill="{t["pill_bg"]}"/>',
        f'<text x="410" y="237" font-family="{SANS_FONT}" font-size="13" font-weight="600" fill="{t["accent"]}" text-anchor="middle">Code</text>',
        f'<text x="474" y="230" font-family="{SANS_FONT}" font-size="13.5" font-weight="500" fill="{t["fg"]}" text-anchor="start">26 graph langs · 48+ outline formats · AST rename</text>',
        f'<text x="474" y="252" font-family="{SANS_FONT}" font-size="12.5" fill="{t["muted"]}" text-anchor="start">map --html/--explain · lookup --graph · query · expand</text>',
        f'<line x1="358" y1="284" x2="888" y2="284" stroke="{t["border"]}"/>',
        # History row
        f'<rect x="362" y="308" width="96" height="28" rx="14" fill="{t["pill_bg"]}"/>',
        f'<text x="410" y="327" font-family="{SANS_FONT}" font-size="13" font-weight="600" fill="{t["accent"]}" text-anchor="middle">History</text>',
        f'<text x="474" y="320" font-family="{SANS_FONT}" font-size="13.5" font-weight="500" fill="{t["fg"]}" text-anchor="start">duplicates · semantic cues · multi-provider proxy</text>',
        f'<text x="474" y="342" font-family="{SANS_FONT}" font-size="12.5" fill="{t["muted"]}" text-anchor="start">cache-safe --compact-live · prefix-drift · recall by ID</text>',
        # Right card: Context window
        f'<rect x="1004" y="40" width="176" height="340" rx="14" fill="{t["card_bg"]}" stroke="{t["border"]}"/>',
        f'<text x="1024" y="78" font-family="{SANS_FONT}" font-size="16" font-weight="600" fill="{t["fg"]}" text-anchor="start">Context window</text>',
        f'<rect x="1020" y="124" width="144" height="50" rx="8" fill="{t["recessed_bg"]}" stroke="{t["border"]}"/>',
        f'<text x="1032" y="145" font-family="{MONO_FONT}" font-size="12" fill="{t["fg"]}" text-anchor="start">3 failing tests</text>',
        f'<text x="1032" y="162" font-family="{MONO_FONT}" font-size="11" fill="{t["muted"]}" text-anchor="start">run-7Gq2 · exit 1</text>',
        f'<path d="M914 149 L996 149" stroke="{t["accent"]}" stroke-width="1.8" fill="none" marker-end="url(#arrow-accent)"/>',
        f'<rect x="1020" y="214" width="144" height="50" rx="8" fill="{t["recessed_bg"]}" stroke="{t["border"]}"/>',
        f'<text x="1032" y="235" font-family="{MONO_FONT}" font-size="12" fill="{t["fg"]}" text-anchor="start">outline</text>',
        f'<text x="1032" y="252" font-family="{MONO_FONT}" font-size="11" fill="{t["muted"]}" text-anchor="start">40 rows · rev#12</text>',
        f'<path d="M914 239 L996 239" stroke="{t["accent"]}" stroke-width="1.8" fill="none" marker-end="url(#arrow-accent)"/>',
        f'<rect x="1020" y="304" width="144" height="50" rx="8" fill="{t["recessed_bg"]}" stroke="{t["border"]}"/>',
        f'<text x="1032" y="325" font-family="{MONO_FONT}" font-size="12" fill="{t["fg"]}" text-anchor="start">recall 9f3c…</text>',
        f'<text x="1032" y="342" font-family="{MONO_FONT}" font-size="11" fill="{t["muted"]}" text-anchor="start">archived result</text>',
        f'<path d="M914 329 L996 329" stroke="{t["accent"]}" stroke-width="1.8" fill="none" marker-end="url(#arrow-accent)"/>',
        # Connectors & labels
        f'<path d="M250 210 L330 210" stroke="{t["raw_bar"]}" stroke-width="1.6" fill="none" marker-end="url(#arrow)"/>',
        f'<text x="291" y="198" font-family="{SANS_FONT}" font-size="12" fill="{t["muted"]}" text-anchor="middle">hooks · MCP</text>',
        f'<text x="291" y="232" font-family="{SANS_FONT}" font-size="12" fill="{t["muted"]}" text-anchor="middle">CLI</text>',
        f'<text x="956" y="119" font-family="{SANS_FONT}" font-size="12" font-weight="600" fill="{t["accent"]}" text-anchor="middle">compact</text>',
        f'<text x="956" y="135" font-family="{SANS_FONT}" font-size="12" font-weight="600" fill="{t["accent"]}" text-anchor="middle">by default</text>',
        f'<path d="M623 386 L623 422" stroke="{t["raw_bar"]}" stroke-width="1.6" fill="none" marker-end="url(#arrow)"/>',
        f'<text x="636" y="410" font-family="{SANS_FONT}" font-size="12" fill="{t["muted"]}" text-anchor="start">full output, archived</text>',
        # Bottom archive box: .justless/
        f'<rect x="338" y="430" width="570" height="90" rx="14" fill="{t["recessed_bg"]}" stroke="{t["border"]}" stroke-dasharray="5 4"/>',
        f'<text x="362" y="466" font-family="{MONO_FONT}" font-size="15" font-weight="600" fill="{t["fg"]}" text-anchor="start">.justless/</text>',
        f'<text x="470" y="466" font-family="{MONO_FONT}" font-size="13" fill="{t["muted"]}" text-anchor="start">runs/ · index.bin · observations/</text>',
        f'<text x="362" y="494" font-family="{SANS_FONT}" font-size="13" fill="{t["muted"]}" text-anchor="start">Every byte kept locally, replayable exactly.</text>',
        f'<path d="M914 475 L1080 475 Q1092 475 1092 463 L1092 388" stroke="{t["accent"]}" stroke-width="1.6" stroke-dasharray="6 5" fill="none" marker-end="url(#arrow-accent)"/>',
        f'<text x="932" y="465" font-family="{SANS_FONT}" font-size="12" font-weight="600" fill="{t["accent"]}" text-anchor="start">exact on demand</text>',
        f'<text x="932" y="495" font-family="{MONO_FONT}" font-size="12" fill="{t["muted"]}" text-anchor="start">log · expand · recall</text>',
        "</svg>\n",
    ])


def rasterize_qa(svg_paths: list[Path], qa_dir: Path) -> None:
    if not RESVG_BIN.is_file():
        raise FileNotFoundError(f"resvg not found at {RESVG_BIN}")
    qa_dir.mkdir(parents=True, exist_ok=True)

    def render_one(svg_path: Path) -> None:
        png_path = qa_dir / f"{svg_path.stem}.png"
        proc = subprocess.run(
            [str(RESVG_BIN), str(svg_path), str(png_path)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if proc.returncode != 0:
            raise RuntimeError(
                f"resvg failed for {svg_path.name}:\n{proc.stderr.decode('utf-8', 'replace')}"
            )

    with ThreadPoolExecutor(max_workers=min(8, os.cpu_count() or 4)) as pool:
        list(pool.map(render_one, svg_paths))


def generate_assets(
    binary: Path,
    suite_json: Path,
    out_dir: Path,
    make_gif: bool = False,
    qa_dir: Path | None = None,
) -> list[Path]:
    suite = json.loads(suite_json.read_text(encoding="utf-8"))
    caps = capture_all(binary)
    out_dir.mkdir(parents=True, exist_ok=True)

    written_svgs: list[Path] = []
    for theme in ("dark", "light"):
        outputs = {
            f"demo-{theme}.svg": render_demo_svg(caps["failing"], theme, static_ms=None),
            f"before-after-{theme}.svg": render_before_after_svg(caps["failing"], suite, theme),
            f"savings-{theme}.svg": render_savings_svg(suite, theme),
            f"outline-{theme}.svg": render_outline_svg(caps["outline"], theme),
            f"search-{theme}.svg": render_search_svg(caps["search"], theme),
            f"query-{theme}.svg": render_query_svg(caps["query"], theme),
            f"statusline-{theme}.svg": render_statusline_svg(theme),
            f"how-it-works-{theme}.svg": render_how_it_works_svg(theme),
        }
        for name, svg_text in outputs.items():
            path = out_dir / name
            path.write_text(svg_text, encoding="utf-8")
            written_svgs.append(path)

    written: list[Path] = list(written_svgs)
    if make_gif:
        gif_path = out_dir / "demo-dark.gif"
        render_demo_gif(caps["failing"], gif_path)
        written.append(gif_path)

    if qa_dir is not None:
        rasterize_qa(written_svgs, qa_dir)

    return written


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        type=Path,
        default=REPO_ROOT / "target/release/justless",
        help="Path to the compiled justless binary (default: ./target/release/justless)",
    )
    parser.add_argument(
        "--suite-json",
        type=Path,
        required=True,
        help="Path to the JSON emitted by scripts/suite_benchmark.py",
    )
    parser.add_argument(
        "--out-dir",
        type=Path,
        default=REPO_ROOT / "assets",
        help="Output directory for generated SVGs and GIF (default: assets)",
    )
    parser.add_argument(
        "--gif",
        action="store_true",
        help="Also render looping 900x426 demo-dark.gif via resvg + two-pass ffmpeg",
    )
    parser.add_argument(
        "--qa-dir",
        type=Path,
        default=None,
        help="Optional directory to rasterize every generated SVG to PNG via resvg for QA",
    )
    args = parser.parse_args(argv)

    written = generate_assets(
        binary=args.binary,
        suite_json=args.suite_json,
        out_dir=args.out_dir,
        make_gif=args.gif,
        qa_dir=args.qa_dir,
    )
    for p in written:
        sys.stdout.write(f"wrote {p} ({p.stat().st_size:,} B)\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
