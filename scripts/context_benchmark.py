#!/usr/bin/env python3
"""Measure source and transcript bytes on a verified local repair."""

import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile


def compact(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()


def measure(binary, work):
    def cli(*args, data=b""):
        return subprocess.run([binary, *args], cwd=work, input=data,
                              stdout=subprocess.PIPE, check=True).stdout

    file = "pricing.py"
    old = "def checkout_total(subtotal, fee):\n    return subtotal - fee"
    replacement = old.replace(" - ", " + ")
    helpers = "".join(f"def rate_{i}(amount):\n    return amount * {i}\n\n" for i in range(450))
    original = (helpers + old + "\n").encode()
    (work / file).write_bytes(original)
    page = cli("read", file)
    selected = cli("read", file, "--match", "checkout_total", "--json")
    outline = json.loads(selected)
    anchor = f"{outline['revision']}#{outline['entries'][0]['id']}"
    body = cli("expand", file, anchor)
    assert body == old.encode()
    cli("edit", file, anchor, data=replacement.encode())
    assert (work / file).read_bytes() == original.replace(old.encode(), replacement.encode())
    subprocess.run([sys.executable, "-B", "-c",
                    "from pricing import checkout_total; assert checkout_total(100, 7) == 107"],
                   cwd=work, check=True)

    messages = []
    for i in range(6):
        call_id = f"build_{i}"
        messages.append({"role": "assistant", "content": [
            {"type": "tool_use", "id": call_id, "name": "build", "input": {}}]})
        messages.append({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": call_id,
             "content": f"build {i} checked a source file\n" * 200}]})
    before = compact({"messages": messages})
    after = cli("mask", data=before)
    assert json.loads(cli("mask", "--restore", data=after)) == json.loads(before)
    return {
        "full_file_bytes": len(original), "first_outline_bytes": len(page),
        "filtered_outline_bytes": len(selected), "expanded_symbol_bytes": len(body),
        "edit_reference_and_replacement_bytes": len(anchor.encode()) + len(replacement.encode()),
        "request_before_bytes": len(before), "request_after_bytes": len(after),
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path,
                        default=Path(__file__).resolve().parents[1] / "target/release/justless")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as directory:
        print(json.dumps(measure(args.binary.resolve(), Path(directory)), indent=2))
