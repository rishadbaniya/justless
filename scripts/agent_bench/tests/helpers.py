"""Shared test helpers. Tests run offline: python3 -m unittest discover -s scripts/agent_bench/tests -v"""

import os
import shutil
import sys
import tempfile
from pathlib import Path

BENCH = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(BENCH))

import config  # noqa: E402

BINARY = Path(os.environ.get("JUSTLESS_BINARY", str(config.REPO / "target" / "release" / "justless")))
if not BINARY.exists():
    BINARY = config.REPO / "target" / "debug" / "justless"


def tempdir(test):
    path = Path(tempfile.mkdtemp(prefix="ab-test-"))
    test.addCleanup(shutil.rmtree, str(path), True)
    return Path(os.path.realpath(str(path)))


def have_cargo():
    return shutil.which("cargo", path=str(config.CARGO_HOME / "bin") + ":" + config.SYSTEM_PATH) is not None


def cargo_env(root, target):
    import workspace
    home = Path(root).parent / "home"
    bindir = Path(root).parent / "bin"
    workspace.make_toolbin(bindir)
    return workspace.tool_env(root, home, bindir, target)


CRATE_LIB = '''pub fn clamp_add(a: u32, b: u32, max: u32) -> u32 {
    let sum = a + b;
    if sum > max {
        max
    } else {
        sum
    }
}

pub fn double(x: u32) -> u32 {
    x * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps() {
        assert_eq!(clamp_add(5, 6, 10), 10);
        assert_eq!(clamp_add(2, 3, 10), 5);
    }

    #[test]
    fn doubles() {
        assert_eq!(double(4), 8);
    }
}
'''

CRATE_TEST = '''#[test]
fn integration_clamp() {
    assert_eq!(demo_crate::clamp_add(1, 1, 1), 1);
}
'''


def make_crate(root):
    root = Path(root)
    (root / "src").mkdir(parents=True)
    (root / "tests").mkdir()
    (root / "Cargo.toml").write_text('[package]\nname = "demo-crate"\nversion = "0.1.0"\nedition = "2021"\n')
    (root / "src" / "lib.rs").write_text(CRATE_LIB)
    (root / "tests" / "integration.rs").write_text(CRATE_TEST)
    return root
