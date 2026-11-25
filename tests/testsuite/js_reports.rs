//! End-to-end tests for structured JS/TS and Playwright test reports:
//! Jest, Vitest, Bun test, Node `node --test`, and Playwright test.
//!
//! Verifies passing-suite compaction, failing-suite root-cause grouping with
//! exact `file:line:col` and `[stream:A-B]` pointers, `Compared with run-ID:`
//! rerun diffs, child exit-status preservation, exact `justless log` replay,
//! console/warning preservation, and fail-closed fallback on unrecognized output.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

use crate::common::{BIN, executable, has, lacks, run_id, text};

struct Sandbox {
    dir: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        for sub in ["root", "bin", "home", "state"] {
            fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        fs::write(dir.path().join("root/.justless.toml"), "").unwrap();
        let stub = "#!/bin/sh\ncat \"$OUT\"\ncat \"$ERR\" >&2\nexit \"$CODE\"\n";
        for tool in ["jest", "vitest", "bun", "node", "playwright", "npm"] {
            executable(&dir.path().join("bin").join(tool), stub);
        }
        Self { dir }
    }

    fn justless(&self, args: &[&str], streams: (&Path, &Path), code: i32) -> Output {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![self.dir.path().join("bin")];
        dirs.extend(std::env::split_paths(&path));
        Command::new(BIN)
            .args(args)
            .current_dir(self.dir.path().join("root"))
            .env("PATH", std::env::join_paths(dirs).unwrap())
            .env("HOME", self.dir.path().join("home"))
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("OUT", streams.0)
            .env("ERR", streams.1)
            .env("CODE", code.to_string())
            .output()
            .unwrap()
    }

    fn file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::write(&path, content).unwrap();
        path
    }

    fn log(&self, output: &Output, stream: &str, range: Option<(usize, usize)>) -> String {
        let id = run_id(output);
        let empty = Path::new("/dev/null");
        let mut args = vec!["log".to_owned(), id, "--stream".into(), stream.into()];
        if let Some((from, to)) = range {
            args.push("--from".into());
            args.push(from.to_string());
            args.push("--to".into());
            args.push(to.to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let replay = self.justless(&arg_refs, (empty, empty), 0);
        assert!(replay.status.success(), "{replay:?}");
        text(if stream == "stdout" {
            &replay.stdout
        } else {
            &replay.stderr
        })
    }
}

#[cfg(unix)]
#[test]
fn jest_compacts_passes_groups_root_causes_diffs_reruns_and_replays_exact_stderr() {
    let sandbox = Sandbox::new();
    let empty = sandbox.file("empty", "");

    // 1. Huge passing Jest run on stderr compacts heavily and replays verbatim.
    let mut pass_err = String::new();
    for i in 1..=100 {
        pass_err.push_str(&format!("PASS src/features/mod_{i:03}.test.ts (0.14 s)\n"));
    }
    pass_err.push_str(
        "\nTest Suites: 100 passed, 100 total\n\
         Tests:       540 passed, 540 total\n\
         Snapshots:   8 total\n\
         Time:        4.82 s\n\
         Ran all test suites.\n",
    );
    let pass_file = sandbox.file("jest_pass.err", &pass_err);
    let pass = sandbox.justless(
        &["run", "--hook", "--session", "s1", "--", "jest"],
        (&empty, &pass_file),
        0,
    );
    assert_eq!(pass.status.code(), Some(0));
    let shown = text(&pass.stdout);
    has(&shown, "jest: 100 passed suite(s) [stderr:1-100]\n");
    has(&shown, "Test Suites: 100 passed, 100 total\n");
    has(&shown, "Tests:       540 passed, 540 total\n");
    has(&shown, "Snapshots:   8 total\n");
    has(&shown, "Time:        4.82 s\n");
    lacks(&shown, "mod_001.test.ts");
    assert!(
        shown.len() * 10 < pass_err.len(),
        "expected >90% reduction ({} vs {}):\n{shown}",
        shown.len(),
        pass_err.len()
    );
    assert_eq!(sandbox.log(&pass, "stderr", None), pass_err);

    // 2. Failing Jest run with shared root cause + distinct assertion + console.error on stdout.
    let fail1_out =
        "  console.error\n    Redis timeout on replica-2\n\n      at connect (src/cache.ts:17:9)\n";
    let mut fail1_err = String::new();
    for i in 1..=25 {
        fail1_err.push_str(&format!("PASS src/ok_{i:02}.test.ts\n"));
    }
    fail1_err.push_str(
        "FAIL src/billing.test.ts\n\
           ● Billing › calculates subtotal\n\n\
             TypeError: Cannot read properties of undefined (reading 'taxRate')\n\n\
               20 | export function tax(n: number) {\n\
             > 21 |   return n * cfg.taxRate;\n\
                  |                  ^\n\n\
               at tax (src/pricing.ts:21:18)\n\
               at Object.<anonymous> (src/billing.test.ts:12:15)\n\n\
           ● Billing › calculates grand total\n\n\
             TypeError: Cannot read properties of undefined (reading 'taxRate')\n\n\
               at tax (src/pricing.ts:21:18)\n\
               at Object.<anonymous> (src/billing.test.ts:20:15)\n\n\
           ● Billing › applies coupon\n\n\
             expect(received).toBe(expected) // Object.is equality\n\n\
             Expected: 80\n\
             Received: 90\n\n\
               at Object.toBe (src/billing.test.ts:34:25)\n\n\
         Test Suites: 1 failed, 25 passed, 26 total\n\
         Tests:       3 failed, 120 passed, 123 total\n\
         Time:        2.11 s\n\
         Ran all test suites.\n",
    );
    let fail1_out_file = sandbox.file("jest_fail1.out", fail1_out);
    let fail1_err_file = sandbox.file("jest_fail1.err", &fail1_err);
    let fail1 = sandbox.justless(
        &["run", "--hook", "--session", "loop", "--", "jest"],
        (&fail1_out_file, &fail1_err_file),
        1,
    );
    assert_eq!(fail1.status.code(), Some(1));
    let shown1 = text(&fail1.stdout);
    has(&shown1, "3 reported failing test(s)\n");
    has(
        &shown1,
        "2 test(s): TypeError: Cannot read properties of undefined (reading 'taxRate') · src/pricing.ts:21:18\n",
    );
    has(&shown1, "  Billing › calculates subtotal [stderr:27-36]\n");
    has(
        &shown1,
        "  Billing › calculates grand total [stderr:38-43]\n",
    );
    has(
        &shown1,
        "1 test(s): Billing › applies coupon · src/billing.test.ts:34:25 [stderr:45-52]\n",
    );
    has(&shown1, "  Expected: 80\n  Received: 90\n");
    has(&shown1, "Redis timeout on replica-2");
    has(&shown1, "jest: 25 passed suite(s) [stderr:1-25]\n");
    assert_eq!(sandbox.log(&fail1, "stderr", None), fail1_err);
    assert_eq!(sandbox.log(&fail1, "stdout", None), fail1_out);

    // 3. Rerun in same session after fixing `src/pricing.ts`: 2 fixed, 1 still failing.
    let mut fail2_err = String::new();
    for i in 1..=25 {
        fail2_err.push_str(&format!("PASS src/ok_{i:02}.test.ts\n"));
    }
    fail2_err.push_str(
        "FAIL src/billing.test.ts\n\
           ● Billing › applies coupon\n\n\
             expect(received).toBe(expected) // Object.is equality\n\n\
             Expected: 80\n\
             Received: 90\n\n\
               at Object.toBe (src/billing.test.ts:34:25)\n\n\
         Test Suites: 1 failed, 25 passed, 26 total\n\
         Tests:       1 failed, 122 passed, 123 total\n\
         Time:        1.98 s\n\
         Ran all test suites.\n",
    );
    let fail2_err_file = sandbox.file("jest_fail2.err", &fail2_err);
    let fail2 = sandbox.justless(
        &["run", "--hook", "--session", "loop", "--", "jest"],
        (&empty, &fail2_err_file),
        1,
    );
    assert_eq!(fail2.status.code(), Some(1));
    let shown2 = text(&fail2.stdout);
    let id1 = run_id(&fail1);
    has(&shown2, "1 reported failing test(s)\n");
    has(&shown2, &format!("Compared with {id1}:\n"));
    has(&shown2, "- Billing › calculates subtotal ");
    has(&shown2, "- Billing › calculates grand total ");
    has(&shown2, "[prior stderr:27-36]");
    has(&shown2, "Still failing (1):\n");
    has(&shown2, "  Billing › applies coupon");
    has(&shown2, "[stderr:27-34]");
    has(&shown2, "Tests:       1 failed, 122 passed, 123 total\n");

    // 4. Third run after fixing the last test: all pass!
    let pass_after = sandbox.justless(
        &["run", "--hook", "--session", "loop", "--", "jest"],
        (&empty, &pass_file),
        0,
    );
    assert_eq!(pass_after.status.code(), Some(0));
    let shown3 = text(&pass_after.stdout);
    let id2 = run_id(&fail2);
    has(&shown3, &format!("Compared with {id2}:\n"));
    has(&shown3, "- Billing › applies coupon ");
    has(&shown3, "Test Suites: 100 passed, 100 total\n");
}

#[cfg(unix)]
#[test]
fn vitest_bun_node_test_and_playwright_produce_compact_reports_with_replay() {
    let sandbox = Sandbox::new();
    let empty = sandbox.file("empty", "");

    // --- Vitest (split stdout passes/totals + stderr FAIL blocks) ---
    let mut vitest_out = String::from(" RUN  v2.1.8 /repo/web\n\n");
    for i in 1..=60 {
        vitest_out.push_str(&format!(" ✓ src/mod_{i:02}.test.ts (5 tests) 6ms\n"));
    }
    vitest_out.push_str(
        " ❯ src/cart.test.ts (4 tests | 1 failed) 15ms\n\n\
         Test Files  1 failed | 60 passed (61)\n\
              Tests  1 failed | 303 passed (304)\n\
           Start at  19:45:00\n\
           Duration  1.45s (transform 90ms, tests 310ms)\n",
    );
    let vitest_err = "\
⎯⎯⎯⎯⎯⎯ Failed Tests 1 ⎯⎯⎯⎯⎯⎯

 FAIL  src/cart.test.ts > Cart > removes item
AssertionError: expected 2 to be 1 // Object.is equality

- Expected
+ Received

- 1
+ 2

 ❯ src/cart.test.ts:42:21
     40|   test('removes item', () => {
     42|     expect(cart.count).toBe(1);
       |                        ^

⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯[1/1]⎯
";
    let vo = sandbox.file("vitest.out", &vitest_out);
    let ve = sandbox.file("vitest.err", vitest_err);
    let vitest = sandbox.justless(&["run", "--hook", "--", "vitest", "run"], (&vo, &ve), 1);
    assert_eq!(vitest.status.code(), Some(1));
    let vshown = text(&vitest.stdout);
    has(&vshown, "1 reported failing test(s)\n");
    has(
        &vshown,
        "1 test(s): src/cart.test.ts > Cart > removes item · src/cart.test.ts:42:21 [stderr:3-15]\n",
    );
    has(
        &vshown,
        "  AssertionError: expected 2 to be 1 // Object.is equality\n",
    );
    has(&vshown, "vitest: 60 passed test file(s) [stdout:3-62]\n");
    has(&vshown, "Test Files  1 failed | 60 passed (61)\n");
    assert!(
        vshown.len() * 4 < vitest_out.len() + vitest_err.len(),
        "{vshown}"
    );
    assert_eq!(sandbox.log(&vitest, "stdout", None), vitest_out);
    assert_eq!(sandbox.log(&vitest, "stderr", None), vitest_err);

    // --- Bun test (stderr output) ---
    let mut bun_err = String::from("bun test v1.1.38 (deadbeef)\n\nsrc/math.test.ts:\n");
    for i in 1..=80 {
        bun_err.push_str(&format!("(pass) Math > check {i:02} [0.09ms]\n"));
    }
    bun_err.push_str(
        "\nsrc/auth.test.ts:\n\
         18 |     expect(valid).toBe(true);\n\
                            ^\n\
         error: expect(received).toBe(expected)\n\n\
         Expected: true\n\
         Received: false\n\n\
               at <anonymous> (src/auth.test.ts:18:19)\n\
         (fail) Auth > validates jwt [0.88ms]\n\n\
          80 pass\n\
          1 fail\n\
          81 expect() calls\n\
         Ran 81 tests across 2 files. [31.20ms]\n",
    );
    let be = sandbox.file("bun.err", &bun_err);
    let bun = sandbox.justless(&["run", "--hook", "--", "bun", "test"], (&empty, &be), 1);
    assert_eq!(bun.status.code(), Some(1));
    let bshown = text(&bun.stdout);
    has(&bshown, "1 reported failing test(s)\n");
    has(
        &bshown,
        "src/auth.test.ts › Auth > validates jwt · src/auth.test.ts:18:19 [stderr:",
    );
    has(&bshown, "  Expected: true\n  Received: false\n");
    has(&bshown, "bun test: 80 passed test(s) [stderr:4-83]\n");
    has(&bshown, "Ran 81 tests across 2 files. [31.20ms]\n");
    assert!(bshown.len() * 6 < bun_err.len(), "{bshown}");
    assert_eq!(sandbox.log(&bun, "stderr", None), bun_err);

    // --- Node `--test` (TAP with YAML blocks suppressed on passes, preserved on failures) ---
    let mut node_out = String::from("TAP version 13\n# Subtest: Ledger\n");
    for i in 1..=60 {
        node_out.push_str(&format!(
            "    # Subtest: case {i:02}\n    ok {i} - case {i:02}\n      ---\n      duration_ms: 0.25\n      type: 'test'\n      ...\n"
        ));
    }
    node_out.push_str(concat!(
        "    # Subtest: rejects overdraft\n",
        "    not ok 61 - rejects overdraft\n",
        "      ---\n",
        "      duration_ms: 1.15\n",
        "      type: 'test'\n",
        "      location: '/repo/test/ledger.test.js:80:3'\n",
        "      failureType: 'testCodeFailure'\n",
        "      error: |-\n",
        "        Expected values to be strictly equal:\n",
        "\n",
        "        -10 !== 0\n",
        "      code: 'ERR_ASSERTION'\n",
        "      name: 'AssertionError'\n",
        "      expected: 0\n",
        "      actual: -10\n",
        "      stack: |-\n",
        "        TestContext.<anonymous> (/repo/test/ledger.test.js:82:10)\n",
        "      ...\n",
        "    1..61\n",
        "not ok 1 - Ledger\n",
        "  ---\n",
        "  duration_ms: 21.0\n",
        "  type: 'suite'\n",
        "  failureType: 'subtestsFailed'\n",
        "  error: '1 subtest failed'\n",
        "  ...\n",
        "1..1\n",
        "# tests 61\n",
        "# suites 1\n",
        "# pass 60\n",
        "# fail 1\n",
        "# cancelled 0\n",
        "# skipped 0\n",
        "# todo 0\n",
        "# duration_ms 34.50\n",
    ));
    let no = sandbox.file("node.out", &node_out);
    let node = sandbox.justless(&["run", "--hook", "--", "node", "--test"], (&no, &empty), 1);
    assert_eq!(node.status.code(), Some(1));
    let nshown = text(&node.stdout);
    has(&nshown, "1 reported failing test(s)\n");
    has(
        &nshown,
        "1 test(s): Ledger › rejects overdraft · /repo/test/ledger.test.js:82:10 [stdout:",
    );
    has(&nshown, "-10 !== 0");
    has(&nshown, "node --test: 60 passed test(s) [stdout:3-362]\n");
    has(&nshown, "# tests 61\n");
    has(&nshown, "# pass 60\n");
    has(&nshown, "# fail 1\n");
    lacks(&nshown, "duration_ms: 0.25");
    lacks(&nshown, "1 subtest failed");
    assert!(nshown.len() * 8 < node_out.len(), "{nshown}");
    assert_eq!(sandbox.log(&node, "stdout", None), node_out);

    // --- Playwright test (`playwright test` matched via summary::family) ---
    let mut pw_out = String::from("Running 52 tests using 4 workers\n\n");
    for i in 1..=50 {
        pw_out.push_str(&format!(
            "  ✓  {i} [chromium] › tests/ui.spec.ts:{i}:5 › UI › check {i} (150ms)\n"
        ));
    }
    pw_out.push_str(
        "  ✘  51 [chromium] › tests/login.spec.ts:18:5 › Login › shows error banner (5.1s)\n\
           ✘  52 [webkit] › tests/login.spec.ts:18:5 › Login › shows error banner (5.2s)\n\n\
           1) [chromium] › tests/login.spec.ts:18:5 › Login › shows error banner ───────────────\n\n\
             Error: expect(locator).toBeVisible() failed\n\n\
             Locator:  getByRole('alert')\n\
             Expected: visible\n\
             Received: hidden\n\
             Timeout:  5000ms\n\n\
               20 |     await expect(page.getByRole('alert')).toBeVisible();\n\
                    |                                           ^\n\n\
                 at /repo/tests/login.spec.ts:20:43\n\n\
           2) [webkit] › tests/login.spec.ts:18:5 › Login › shows error banner ─────────────────\n\n\
             Error: expect(locator).toBeVisible() failed\n\n\
             Locator:  getByRole('alert')\n\
             Expected: visible\n\
             Received: hidden\n\
             Timeout:  5000ms\n\n\
                 at /repo/tests/login.spec.ts:20:43\n\n\
           2 failed\n\
             [chromium] › tests/login.spec.ts:18:5 › Login › shows error banner ────────────────\n\
             [webkit] › tests/login.spec.ts:18:5 › Login › shows error banner ──────────────────\n\
           50 passed (21.3s)\n",
    );
    let pwo = sandbox.file("pw.out", &pw_out);
    let pw = sandbox.justless(
        &[
            "run",
            "--hook",
            "--session",
            "pw",
            "--",
            "playwright",
            "test",
        ],
        (&pwo, &empty),
        1,
    );
    assert_eq!(pw.status.code(), Some(1));
    let pshown = text(&pw.stdout);
    has(&pshown, "2 reported failing test(s)\n");
    has(
        &pshown,
        "2 test(s): Error: expect(locator).toBeVisible() failed · /repo/tests/login.spec.ts:20:43\n",
    );
    has(&pshown, "  Locator:  getByRole('alert')\n");
    has(
        &pshown,
        "  [chromium] › tests/login.spec.ts:18:5 › Login › shows error banner [stdout:",
    );
    has(
        &pshown,
        "  [webkit] › tests/login.spec.ts:18:5 › Login › shows error banner [stdout:",
    );
    has(&pshown, "playwright: 50 passed test(s) [stdout:3-52]\n");
    has(&pshown, "50 passed (21.3s)\n");
    assert!(pshown.len() * 5 < pw_out.len(), "{pshown}");
    assert_eq!(sandbox.log(&pw, "stdout", None), pw_out);

    // Playwright rerun after fixing the banner shows `Compared with run-ID:`.
    let mut pw_pass = String::from("Running 52 tests using 4 workers\n\n");
    for i in 1..=52 {
        pw_pass.push_str(&format!(
            "  ✓  {i} [chromium] › tests/ui.spec.ts:{i}:5 › UI › check {i} (140ms)\n"
        ));
    }
    pw_pass.push_str("\n  52 passed (16.1s)\n");
    let pwp = sandbox.file("pw_pass.out", &pw_pass);
    let pw2 = sandbox.justless(
        &[
            "run",
            "--hook",
            "--session",
            "pw",
            "--",
            "playwright",
            "test",
        ],
        (&pwp, &empty),
        0,
    );
    assert_eq!(pw2.status.code(), Some(0));
    let pshown2 = text(&pw2.stdout);
    has(&pshown2, &format!("Compared with {}:\n", run_id(&pw)));
    has(
        &pshown2,
        "- [chromium] › tests/login.spec.ts:18:5 › Login › shows error banner ",
    );
    has(&pshown2, "52 passed (16.1s)\n");
}

#[cfg(unix)]
#[test]
fn unrecognized_or_machine_formatted_output_fails_closed() {
    let sandbox = Sandbox::new();
    let empty = sandbox.file("empty", "");

    // 1. Pre-test crash on Jest keeps exact error verbatim and preserves exit status.
    let crash = "Error: Cannot find module './jest.config.js'\n    at Module._resolveFilename (node:internal/modules/cjs/loader:1145:15)\n";
    let crash_file = sandbox.file("crash.err", crash);
    let out = sandbox.justless(&["run", "--hook", "--", "jest"], (&empty, &crash_file), 1);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stdout), "");
    has(&text(&out.stderr), crash);

    // 2. Playwright `--reporter=json` stays verbatim JSON.
    let pw_json = "{\"config\":{\"version\":\"1.49.0\"},\"suites\":[{\"title\":\"a.spec.ts\"}]}\n";
    let json_file = sandbox.file("pw.json", pw_json);
    let out = sandbox.justless(
        &[
            "run",
            "--hook",
            "--",
            "playwright",
            "test",
            "--reporter=json",
        ],
        (&json_file, &empty),
        0,
    );
    assert_eq!(out.status.code(), Some(0));
    has(&text(&out.stdout), "\"version\":\"1.49.0\"");

    // 3. `npm test` running a custom reporter without recognized totals falls back cleanly.
    let custom = "custom-suite: running 3 checks\ncheck 1 ok\ncheck 2 ok\ncheck 3 ok\nALL DONE\n";
    let custom_file = sandbox.file("custom.out", custom);
    let out = sandbox.justless(
        &["run", "--hook", "--", "npm", "test"],
        (&custom_file, &empty),
        0,
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout), custom);

    // 4. `npm test` with many `✓ ` lines but no recognized totals falls back to LineFilter.
    let mut custom_passes = String::new();
    for i in 1..=60 {
        custom_passes.push_str(&format!("  ✓ custom check {i:02}\n"));
    }
    custom_passes.push_str("CUSTOM SUMMARY: 60 ok\n");
    let custom_pass_file = sandbox.file("custom_passes.out", &custom_passes);
    let out = sandbox.justless(
        &["run", "--hook", "--fresh", "--", "npm", "test"],
        (&custom_pass_file, &empty),
        0,
    );
    assert_eq!(out.status.code(), Some(0));
    let shown = text(&out.stdout);
    has(&shown, "60 filtered");
    has(&shown, "CUSTOM SUMMARY: 60 ok");

    // 5. Merged (`2>&1` / `--merge`) Jest run still produces the structured report with `[stdout:...]`.
    let mut merged_jest = String::new();
    for i in 1..=40 {
        merged_jest.push_str(&format!("PASS src/m_{i:02}.test.ts\n"));
    }
    merged_jest.push_str(
        "Test Suites: 40 passed, 40 total\n\
         Tests:       160 passed, 160 total\n\
         Time:        1.50 s\n\
         Ran all test suites.\n",
    );
    let merged_file = sandbox.file("jest_merged.out", &merged_jest);
    let out = sandbox.justless(
        &["run", "--hook", "--merge", "--fresh", "--", "jest"],
        (&merged_file, &empty),
        0,
    );
    assert_eq!(out.status.code(), Some(0));
    let shown = text(&out.stdout);
    has(&shown, "jest: 40 passed suite(s) [stdout:1-40]\n");
    has(&shown, "Tests:       160 passed, 160 total\n");

    // 6. Stderr warnings during `node --test` are preserved (either in notes or stderr preview).
    let mut node_pass = String::from("TAP version 13\n");
    for i in 1..=30 {
        node_pass.push_str(&format!(
            "# Subtest: t{i}\nok {i} - t{i}\n  ---\n  duration_ms: 0.2\n  type: 'test'\n  ...\n"
        ));
    }
    node_pass.push_str("1..30\n# tests 30\n# pass 30\n# fail 0\n");
    let node_warn_err =
        "(node:1234) [DEP0040] DeprecationWarning: The `punycode` module is deprecated.\n";
    let npo = sandbox.file("node_warn.out", &node_pass);
    let npe = sandbox.file("node_warn.err", node_warn_err);
    let out = sandbox.justless(
        &["run", "--hook", "--fresh", "--", "node", "--test"],
        (&npo, &npe),
        0,
    );
    assert_eq!(out.status.code(), Some(0));
    let shown = text(&out.stdout);
    has(&shown, "node --test: 30 passed test(s) [stdout:2-181]\n");
    has(
        &shown,
        "DeprecationWarning: The `punycode` module is deprecated.",
    );
    has(&shown, "[stderr:1-1]");
}
