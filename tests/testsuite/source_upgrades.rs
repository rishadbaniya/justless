//! End-to-end tests for `#A..#B` range expand, overlap coalescing, cross-file
//! batch expand, constant folding, Rust signature slimming, and `--session`
//! repeat-read delta dedup on `read` and `expand`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

use crate::common::{BIN, has, lacks, piped, text};

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(["--root", root.to_str().unwrap()])
        .args(args)
        .current_dir(root)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("JUSTLESS_SESSION")
        .output()
        .unwrap()
}

fn ok(root: &Path, args: &[&str]) -> String {
    let out = cli(root, args);
    assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn revision_of(outline: &str) -> String {
    outline
        .lines()
        .next()
        .unwrap()
        .strip_prefix("revision ")
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}

#[test]
fn range_expand_expands_folded_imports_constants_and_multi_selectors() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let source = concat!(
        "use std::collections::BTreeMap;\n",
        "use std::fmt;\n",
        "use std::fs;\n",
        "use std::io;\n",
        "use std::path::Path;\n",
        "use std::path::PathBuf;\n",
        "\n",
        "const C_1: usize = 1;\n",
        "const C_2: usize = 2;\n",
        "const C_3: usize = 3;\n",
        "static C_4: usize = 4;\n",
        "const C_5: usize = 5;\n",
        "const C_6: usize = 6;\n",
        "\n",
        "const MULTI: &[&str] = &[\n",
        "    \"alpha\",\n",
        "    \"beta\",\n",
        "];\n",
        "\n",
        "const S_1: usize = 10;\n",
        "const S_2: usize = 20;\n",
        "const S_3: usize = 30;\n",
        "const S_4: usize = 40;\n",
        "\n",
        "pub fn alpha() -> usize {\n",
        "    C_1 + S_1\n",
        "}\n",
        "\n",
        "pub fn beta() -> usize {\n",
        "    C_6 + S_4\n",
        "}\n",
    );
    fs::write(root.join("lib.rs"), source).unwrap();

    let outline = ok(root, &["read", "lib.rs", "--outline"]);
    let rev = revision_of(&outline);

    // 6 contiguous imports fold, 6 contiguous single-line const/static fold,
    // multi-line const and 4-item const run stay unfolded.
    has(&outline, "#1..#6 L1-6 imports (6 items)\n");
    has(&outline, "#7..#12 L8-13 constants (6 items)\n");
    has(
        &outline,
        "#13 L15-18 constant MULTI  const MULTI: &[&str]\n",
    );
    has(&outline, "#14 L20-20 constant S_1");
    has(&outline, "#17 L23-23 constant S_4");
    has(&outline, "#18 L25-27 function alpha");
    has(&outline, "#19 L29-31 function beta");

    // `--match` bypasses constant folding and shows individual entries.
    let matched = ok(root, &["read", "lib.rs", "--outline", "--match", "C_3"]);
    has(&matched, "#9 L10-10 constant C_3  const C_3: usize = 3;\n");
    lacks(&matched, "constants (");

    // Single-selector `#A..#B` returns 100% raw exact bytes (zero headers).
    let expected_imports = concat!(
        "use std::collections::BTreeMap;\n",
        "use std::fmt;\n",
        "use std::fs;\n",
        "use std::io;\n",
        "use std::path::Path;\n",
        "use std::path::PathBuf;",
    );
    assert_eq!(
        ok(root, &["expand", "lib.rs", &format!("{rev}#1..#6")]),
        expected_imports
    );

    let expected_consts = concat!(
        "const C_1: usize = 1;\n",
        "const C_2: usize = 2;\n",
        "const C_3: usize = 3;\n",
        "static C_4: usize = 4;\n",
        "const C_5: usize = 5;\n",
        "const C_6: usize = 6;",
    );
    assert_eq!(
        ok(root, &["expand", "lib.rs", &format!("{rev}#7..#12")]),
        expected_consts
    );
    // Shorthand `#7..#12` and `REV#7..12` also resolve exact bytes.
    assert_eq!(ok(root, &["expand", "lib.rs", "#7..#12"]), expected_consts);
    assert_eq!(
        ok(root, &["expand", "lib.rs", &format!("{rev}#7..12")]),
        expected_consts
    );

    // `--json` preserves range anchor and exact content.
    let range_json: Value = serde_json::from_str(&ok(
        root,
        &["expand", "lib.rs", &format!("{rev}#7..#12"), "--json"],
    ))
    .unwrap();
    assert_eq!(range_json["selector"], format!("{rev}#7..#12"));
    assert_eq!(range_json["first"], 8);
    assert_eq!(range_json["last"], 13);
    assert_eq!(range_json["content"], expected_consts);

    // Multi-selector combining range `#1..#6` + single entry `#18` inherits revision.
    let multi = ok(root, &["expand", "lib.rs", &format!("{rev}#1..#6,#18")]);
    has(
        &multi,
        &format!("--- #1..#6 L1-6 ---\n{expected_imports}\n"),
    );
    has(
        &multi,
        "--- #18 L25-27 ---\npub fn alpha() -> usize {\n    C_1 + S_1\n}\n",
    );

    // Invalid ranges and stale revisions fail strictly.
    for bad in [
        "000000000000000000000000#1..#6".to_owned(),
        format!("{rev}#0..#3"),
        format!("{rev}#6..#1"),
        format!("{rev}#1..#99"),
    ] {
        let out = cli(root, &["expand", "lib.rs", &bad]);
        assert!(!out.status.success(), "expected failure for {bad}");
        assert!(out.stdout.is_empty(), "expected empty stdout for {bad}");
    }

    // Verified `edit` works on `#A..#B` range selectors too.
    let mut cmd = Command::new(BIN);
    cmd.args([
        "--root",
        root.to_str().unwrap(),
        "edit",
        "lib.rs",
        &format!("{rev}#7..#12"),
    ])
    .current_dir(root);
    let edited = piped(&mut cmd, b"const CONSOLIDATED: usize = 21;");
    assert!(edited.status.success(), "{}", text(&edited.stderr));
    let on_disk = fs::read_to_string(root.join("lib.rs")).unwrap();
    has(&on_disk, "const CONSOLIDATED: usize = 21;\n\nconst MULTI");
    lacks(&on_disk, "const C_1");
}

#[test]
fn overlap_coalescing_merges_contained_and_overlapping_spans() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let source = concat!(
        "pub mod engine {\n",
        "    pub fn step_one(x: u32) -> u32 {\n",
        "        x + 1\n",
        "    }\n",
        "\n",
        "    pub fn step_two(x: u32) -> u32 {\n",
        "        x + 2\n",
        "    }\n",
        "}\n",
        "\n",
        "pub fn standalone() -> u32 {\n",
        "    42\n",
        "}\n",
    );
    fs::write(root.join("engine.rs"), source).unwrap();

    let outline = ok(root, &["read", "engine.rs", "--outline"]);
    let rev = revision_of(&outline);
    // #1 is mod engine (L1-9), #2 is step_one (L2-4), #3 is step_two (L6-8), #4 is standalone (L11-13).
    has(&outline, "#1 L1-9 module engine");
    has(&outline, "#2 L2-4 function engine::step_one");
    has(&outline, "#3 L6-8 function engine::step_two");

    // Parent + child coalesces into one span and prints the child body only once.
    let contained = ok(root, &["expand", "engine.rs", &format!("{rev}#1,#2")]);
    has(&contained, "--- #1 L1-9 (includes #2) ---\n");
    assert_eq!(
        contained.matches("pub fn step_one").count(),
        1,
        "{contained}"
    );

    // Reversed order (`#2,#1`) also coalesces under the enclosing `#1`.
    let reversed = ok(root, &["expand", "engine.rs", &format!("{rev}#2,#1")]);
    has(&reversed, "--- #1 L1-9 (includes #2) ---\n");
    assert_eq!(reversed.matches("pub fn step_one").count(), 1, "{reversed}");

    // Parent + two children lists both included IDs cleanly.
    let two_children = ok(root, &["expand", "engine.rs", &format!("{rev}#1,#2,#3")]);
    has(&two_children, "--- #1 L1-9 (includes #2, #3) ---\n");
    assert_eq!(
        two_children.matches("pub fn step_two").count(),
        1,
        "{two_children}"
    );

    // Duplicate selector (`#1,#1`) coalesces without redundant `(includes #1)`.
    let dup = ok(root, &["expand", "engine.rs", &format!("{rev}#1,#1")]);
    has(&dup, "--- #1 L1-9 ---\n");
    lacks(&dup, "(includes");
    assert_eq!(dup.matches("pub mod engine").count(), 1, "{dup}");

    // Overlapping line ranges merge into one exact span.
    let lines = ok(root, &["expand", "engine.rs", "L1-6,L4-11"]);
    has(&lines, "--- L1-11 ---\n");
    assert_eq!(lines.matches("pub fn step_two").count(), 1, "{lines}");
    has(&lines, "pub fn standalone() -> u32 {\n");

    // `--json` multi-selector also emits the coalesced span once.
    let coalesced_json: Value = serde_json::from_str(&ok(
        root,
        &["expand", "engine.rs", &format!("{rev}#1,#2"), "--json"],
    ))
    .unwrap();
    let spans = coalesced_json["spans"].as_array().unwrap();
    assert_eq!(spans.len(), 1, "{coalesced_json}");
    assert_eq!(spans[0]["first"], 1);
    assert_eq!(spans[0]["last"], 9);

    // Single-selector `expand FILE SEL` stays 100% raw exact bytes with zero headers.
    let single = ok(root, &["expand", "engine.rs", &format!("{rev}#2")]);
    assert_eq!(
        single,
        "pub fn step_one(x: u32) -> u32 {\n        x + 1\n    }"
    );
    lacks(&single, "---");
}

#[test]
fn cross_file_batch_expand_verifies_revisions_and_prints_exact_headers() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/a.rs"),
        "pub struct Billing;\n\nimpl Billing {\n    pub fn total(&self) -> u32 {\n        10\n    }\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/b.rs"),
        "use std::fs;\nuse std::io;\nuse std::fmt;\n\npub fn run_b() -> u32 {\n    20\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\n\n[[bin]]\nname = \"worker\"\npath = \"src/a.rs\"\n",
    )
    .unwrap();

    let rev_b = revision_of(&ok(root, &["read", "src/b.rs", "--outline"]));

    // 2-arg cross-file batch with `::` symbol path + `#A..#B` entry range.
    let batch2 = ok(
        root,
        &[
            "expand",
            "src/a.rs:Billing::total",
            &format!("src/b.rs:{rev_b}#1..#3"),
        ],
    );
    has(
        &batch2,
        "=== src/a.rs ===\npub fn total(&self) -> u32 {\n        10\n    }\n",
    );
    has(
        &batch2,
        "=== src/b.rs ===\nuse std::fs;\nuse std::io;\nuse std::fmt;\n",
    );

    // 3-arg cross-file batch including a TOML selector with `:` (`bin:worker`).
    let batch3 = ok(
        root,
        &[
            "expand",
            "src/a.rs:Billing::total",
            &format!("src/b.rs:{rev_b}#1..#3,#4"),
            "Cargo.toml:bin:worker",
        ],
    );
    has(&batch3, "=== src/a.rs ===\n");
    has(&batch3, "=== src/b.rs ===\n--- #1..#3 L1-3 ---\n");
    has(
        &batch3,
        "--- #4 L5-7 ---\npub fn run_b() -> u32 {\n    20\n}\n",
    );
    has(
        &batch3,
        "=== Cargo.toml ===\n[[bin]]\nname = \"worker\"\npath = \"src/a.rs\"\n",
    );

    // Cross-file batch `--json` returns structured per-file entries.
    let batch_json: Value = serde_json::from_str(&ok(
        root,
        &[
            "expand",
            "src/a.rs:Billing::total",
            &format!("src/b.rs:{rev_b}#4"),
            "--json",
        ],
    ))
    .unwrap();
    let files = batch_json["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "{batch_json}");
    assert_eq!(files[0]["file"], "src/a.rs");
    assert_eq!(files[1]["file"], "src/b.rs");
    assert_eq!(files[1]["content"], "pub fn run_b() -> u32 {\n    20\n}");

    // Stale revision on any file rejects the whole batch with empty stdout.
    let stale = cli(
        root,
        &[
            "expand",
            "src/a.rs:Billing::total",
            "src/b.rs:000000000000000000000000#1..#3",
        ],
    );
    assert!(!stale.status.success());
    assert!(stale.stdout.is_empty(), "{}", text(&stale.stdout));
    has(&text(&stale.stderr), "stale revision");

    // `--revision` is rejected in cross-file batch mode.
    let bad_rev = cli(
        root,
        &[
            "expand",
            "src/a.rs:L1-2",
            "src/b.rs:L1-2",
            "--revision",
            &rev_b,
        ],
    );
    assert!(!bad_rev.status.success());
    has(
        &text(&bad_rev.stderr),
        "--revision applies to a single file",
    );

    // Standard 2-arg `expand FILE SEL` stays raw even when SEL contains `:` or `::`.
    assert_eq!(
        ok(root, &["expand", "Cargo.toml", "bin:worker"]),
        "[[bin]]\nname = \"worker\"\npath = \"src/a.rs\"\n"
    );
    assert_eq!(
        ok(root, &["expand", "src/a.rs", "Billing::total"]),
        "pub fn total(&self) -> u32 {\n        10\n    }"
    );
}

#[test]
fn session_dedup_handles_repeat_outlines_ranges_edits_reverts_fresh_and_compaction() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let mut big = String::new();
    for i in 1..=45 {
        big.push_str(&format!(
            "pub fn compute_{i:02}(input: u64) -> u64 {{\n    input.wrapping_add({i})\n}}\n\n"
        ));
    }
    fs::write(root.join("big.rs"), &big).unwrap();

    let small: String = (1..=30)
        .map(|i| format!("let setting_{i:02} = configure_value({i});\n"))
        .collect();
    fs::write(root.join("small.txt"), &small).unwrap();

    // Without `--session`, repeated `read --outline` and `expand` are 100% byte-identical.
    let plain_outline = ok(root, &["read", "big.rs", "--outline"]);
    assert_eq!(ok(root, &["read", "big.rs", "--outline"]), plain_outline);
    let plain_range = ok(root, &["expand", "big.rs", "L1-80"]);
    assert_eq!(ok(root, &["expand", "big.rs", "L1-80"]), plain_range);

    // With `--session s1`, first outline matches plain; second returns 1-line unchanged notice.
    let first_outline = ok(root, &["read", "big.rs", "--outline", "--session", "s1"]);
    assert_eq!(first_outline, plain_outline);
    let second_outline = ok(root, &["read", "big.rs", "--outline", "--session", "s1"]);
    assert_eq!(second_outline.lines().count(), 1, "{second_outline}");
    has(&second_outline, "big.rs · outline unchanged at ");
    assert!(second_outline.len() < plain_outline.len());

    // `--fresh` forces full outline; another session also prints full outline.
    assert_eq!(
        ok(
            root,
            &["read", "big.rs", "--outline", "--session", "s1", "--fresh"]
        ),
        plain_outline
    );
    assert_eq!(
        ok(root, &["read", "big.rs", "--outline", "--session", "s2"]),
        plain_outline
    );

    // With `--session s1`, first `expand L1-80` returns exact raw bytes; second returns 1-line notice.
    let first_range = ok(root, &["expand", "big.rs", "L1-80", "--session", "s1"]);
    assert_eq!(first_range, plain_range);
    let second_range = ok(root, &["expand", "big.rs", "L1-80", "--session", "s1"]);
    assert_eq!(second_range.lines().count(), 1, "{second_range}");
    has(
        &second_range,
        "big.rs:1-80 · unchanged since you read it at ",
    );
    assert!(second_range.len() < plain_range.len());

    // `--fresh` returns exact raw bytes again.
    assert_eq!(
        ok(
            root,
            &["expand", "big.rs", "L1-80", "--session", "s1", "--fresh"]
        ),
        plain_range
    );

    // Edit one line inside L1-80: next `expand L1-80 --session s1` emits a compact line delta.
    let edited = big.replace("wrapping_add(10)", "wrapping_add(1000)");
    fs::write(root.join("big.rs"), &edited).unwrap();
    let delta = ok(root, &["expand", "big.rs", "L1-80", "--session", "s1"]);
    has(&delta, "big.rs:1-80 · 79 of 80 lines unchanged");
    has(&delta, "+ 38 |     input.wrapping_add(1000)\n");
    has(&delta, "-    |     input.wrapping_add(10)\n");
    lacks(&delta, "compute_02");
    assert!(delta.len() < plain_range.len(), "{delta}");

    // Immediately re-reading L1-80 after the delta returns a 1-line unchanged notice.
    let after_delta = ok(root, &["expand", "big.rs", "L1-80", "--session", "s1"]);
    assert_eq!(after_delta.lines().count(), 1, "{after_delta}");
    has(&after_delta, "unchanged since you read it at ");

    // Partial view + edit + revert safety: edit both line 2 (compute_01) and line 70 (compute_18),
    // view only L1-20, revert line 2, then expand L1-80 — both line 2 and line 70 must be shown!
    let two_edits = edited
        .replace("wrapping_add(1)", "wrapping_add(111)")
        .replace("wrapping_add(18)", "wrapping_add(1888)");
    fs::write(root.join("big.rs"), &two_edits).unwrap();
    let partial = ok(root, &["expand", "big.rs", "L1-20", "--session", "s1"]);
    has(&partial, "wrapping_add(111)");
    lacks(&partial, "wrapping_add(1888)");

    let reverted = two_edits.replace("wrapping_add(111)", "wrapping_add(1)");
    fs::write(root.join("big.rs"), &reverted).unwrap();
    let after_revert = ok(root, &["expand", "big.rs", "L1-80", "--session", "s1"]);
    has(&after_revert, "wrapping_add(1)");
    has(&after_revert, "wrapping_add(1888)");
    lacks(&after_revert, "· unchanged since you read it");

    // Small whole-file `read small.txt --session s1` deduplicates and diffs edits too.
    assert_eq!(ok(root, &["read", "small.txt", "--session", "s1"]), small);
    let small_again = ok(root, &["read", "small.txt", "--session", "s1"]);
    assert_eq!(small_again.lines().count(), 1, "{small_again}");
    has(
        &small_again,
        "small.txt:1-30 · unchanged since you read it at ",
    );
    fs::write(
        root.join("small.txt"),
        small.replace("configure_value(15)", "configure_value(150)"),
    )
    .unwrap();
    let small_delta = ok(root, &["read", "small.txt", "--session", "s1"]);
    has(
        &small_delta,
        "+ 15 | let setting_15 = configure_value(150);\n",
    );
    has(
        &small_delta,
        "-    | let setting_15 = configure_value(15);\n",
    );

    // `stats --json` records the delta savings from `read`/`expand --session`.
    let stats: Value = serde_json::from_str(&ok(root, &["stats", "--json"])).unwrap();
    assert!(
        stats["changed_since_read"]["views"].as_u64().unwrap() >= 4,
        "{stats}"
    );

    // Compaction clears session file views so the next read prints whole again.
    let mut compact = Command::new(BIN);
    compact
        .args([
            "--root",
            root.to_str().unwrap(),
            "hook-claude",
            "before-compact",
        ])
        .current_dir(root);
    let hook_out = piped(
        &mut compact,
        json!({"hook_event_name":"PreCompact","trigger":"auto","cwd":root})
            .to_string()
            .as_bytes(),
    );
    assert!(hook_out.status.success());
    let current_outline = ok(root, &["read", "big.rs", "--outline"]);
    assert_eq!(
        ok(root, &["read", "big.rs", "--outline", "--session", "s1"]),
        current_outline
    );
    let current_range = ok(root, &["expand", "big.rs", "L1-80"]);
    assert_eq!(
        ok(root, &["expand", "big.rs", "L1-80", "--session", "s1"]),
        current_range
    );

    // Same-line declarations (`fn one() {} fn two() {}`) must not falsely mark
    // sibling declarations on the same line as seen under `--session`.
    let same_line = format!(
        "fn one() {{ /* {} */ }} fn two() {{ /* {} */ }}\n",
        "a".repeat(120),
        "b".repeat(120)
    );
    fs::write(root.join("same.rs"), &same_line).unwrap();
    let one_out = ok(root, &["expand", "same.rs", "one", "--session", "s1"]);
    assert!(one_out.starts_with("fn one()"), "{one_out}");
    let two_out = ok(root, &["expand", "same.rs", "two", "--session", "s1"]);
    assert!(two_out.starts_with("fn two()"), "{two_out}");
    lacks(&two_out, "unchanged since you read it");

    // Cross-file batch expand also respects `--session` dedup.
    let batch_first = ok(
        root,
        &[
            "expand",
            "big.rs:L1-80",
            "small.txt:L1-30",
            "--session",
            "s-batch",
        ],
    );
    has(&batch_first, "=== big.rs ===\npub fn compute_01");
    has(&batch_first, "=== small.txt ===\nlet setting_01");
    let batch_second = ok(
        root,
        &[
            "expand",
            "big.rs:L1-80",
            "small.txt:L1-30",
            "--session",
            "s-batch",
        ],
    );
    has(
        &batch_second,
        "=== big.rs ===\nbig.rs:1-80 · unchanged since you read it",
    );
    has(
        &batch_second,
        "=== small.txt ===\nsmall.txt:1-30 · unchanged since you read it",
    );
    assert!(batch_second.len() < batch_first.len());
}

#[test]
fn rust_redundant_signature_slimming_keeps_visibility_and_generics() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let source = concat!(
        "struct Plain {\n    x: u32,\n}\n\n",
        "enum Choice {\n    A,\n    B,\n}\n\n",
        "trait Marker {}\n\n",
        "pub struct Exported {\n    pub x: u32,\n}\n\n",
        "struct Generic<T>(T);\n",
    );
    fs::write(root.join("types.rs"), source).unwrap();

    let outline = ok(root, &["read", "types.rs", "--outline"]);
    has(&outline, "#1 L1-3 struct Plain\n");
    has(&outline, "#2 L5-8 enum Choice\n");
    has(&outline, "#3 L10-10 trait Marker\n");
    has(&outline, "#4 L12-14 struct Exported  pub struct Exported\n");
    has(
        &outline,
        "#5 L16-16 struct Generic  struct Generic<T>(T);\n",
    );

    // `--match` still matches on `struct Plain` even though the redundant display signature is omitted.
    let matched = ok(
        root,
        &["read", "types.rs", "--outline", "--match", "struct Plain"],
    );
    has(&matched, "#1 L1-3 struct Plain\n");
    lacks(&matched, "Choice");
}
