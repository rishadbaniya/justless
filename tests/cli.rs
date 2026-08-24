use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::process::{Command, Output};

use tempfile::TempDir;

fn fixture() -> TempDir {
    let target = TempDir::new().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/failing");
    for file in [
        "Cargo.toml",
        ".gitignore",
        "src/lib.rs",
        "src/pricing.rs",
        "src/obsolete.rs",
        "tests/checkout.rs",
    ] {
        let path = target.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::copy(source.join(file), path).unwrap();
    }
    target
}

fn mixed_fixture() -> TempDir {
    let target = TempDir::new().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/mixed");
    for file in [
        "frontend/orders.ts",
        "frontend/format.ts",
        "backend/orders.py",
        "db/schema.sql",
        "docs/operations.md",
        "backend/settings.custom",
    ] {
        let path = target.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::copy(source.join(file), path).unwrap();
    }
    target
}

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_justless"))
        .args(args)
        .current_dir(root)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .output()
        .unwrap()
}

fn success(root: &Path, args: &[&str]) -> String {
    let output = cli(root, args);
    assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    stdout(&output)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn run_id(output: &Output) -> String {
    stderr(output)
        .split_once("[justless ")
        .unwrap()
        .1
        .split_once(']')
        .unwrap()
        .0
        .to_owned()
}

#[test]
fn failing_cargo_test_leads_to_source_and_exact_log_slice() {
    let root = fixture();
    let path = root.path();
    let output = cli(
        path,
        &[
            "run",
            "--max-bytes",
            "512",
            "--",
            "cargo",
            "test",
            "--offline",
        ],
    );
    assert_eq!(output.status.code(), Some(101), "{:?}", output);
    let preview = stdout(&output);
    assert!(preview.len() <= 512, "{preview}");
    assert!(preview.contains("total_includes_fee"), "{preview}");
    assert!(preview.contains("tests/checkout.rs:11"), "{preview}");
    assert!(preview.contains("assertion"), "{preview}");
    assert!(!preview.contains("only-in-the-full-log"), "{preview}");
    let id = run_id(&output);
    let context = success(path, &["context", "--run", &id]);
    assert!(context.contains("tests/checkout.rs:11"), "{context}");
    assert!(
        context.contains("assert_eq!(total(100, 5), 105);"),
        "{context}"
    );
    assert!(
        context.contains("checkout_fixture::pricing::checkout_total"),
        "{context}"
    );
    let code = success(path, &["context", "--symbol", "checkout_total"]);
    assert!(code.contains("src/pricing.rs:1-3"), "{code}");
    assert!(code.contains("apply_fee(subtotal, fee)"), "{code}");
    let full = success(path, &["log", &id, "--stream", "stdout"]);
    assert!(full.len() > 15_000, "{}", full.len());
    let matches = success(path, &["search", &id, "only-in-the-full-log"]);
    let number = matches.split(':').nth(1).unwrap();
    let part = success(
        path,
        &[
            "log", &id, "--stream", "stdout", "--from", number, "--to", number,
        ],
    );
    assert_eq!(part, "billing-trace probe=only-in-the-full-log\n");
    assert_eq!(
        full.lines().nth(number.parse::<usize>().unwrap() - 1),
        Some(part.trim_end())
    );
    let bounded = success(path, &["context", "--run", &id, "--budget", "240"]);
    assert!(bounded.len() <= 240, "{bounded}");
    assert!(bounded.contains("[context budget reached]"), "{bounded}");
    fs::write(path.join("tests/checkout.rs"), "fn new_source() {}\n").unwrap();
    let historical = success(path, &["context", "--run", &id]);
    assert!(
        historical.contains("Source changed or disappeared"),
        "{historical}"
    );
}

#[test]
fn index_tracks_relationships_and_reuses_unchanged_files() {
    let root = fixture();
    let path = root.path();
    assert!(success(path, &["index"]).contains("4 parsed"));
    assert!(success(path, &["index"]).contains("0 parsed · 4 reused"));
    let tests = success(path, &["query", "apply_fee", "--view", "tests"]);
    assert!(tests.contains("related test (2 call hops)"), "{tests}");
    assert!(tests.contains("total_includes_fee"), "{tests}");
    let callers = success(path, &["query", "checkout_total", "--view", "callers"]);
    assert!(
        callers.contains(
            "call from checkout_fixture_test_checkout::total_includes_fee at tests/checkout.rs:11"
        ),
        "{callers}"
    );
    let references = success(path, &["query", "checkout_total", "--view", "references"]);
    assert!(
        references.contains("import from file scope at tests/checkout.rs:1"),
        "{references}"
    );
    fs::write(
        path.join("src/pricing.rs"),
        "pub fn replacement_total() -> u32 { 105 }\n",
    )
    .unwrap();
    fs::remove_file(path.join("src/obsolete.rs")).unwrap();
    let changed = success(path, &["index"]);
    assert!(
        changed.contains("1 parsed · 2 reused · 1 removed"),
        "{changed}"
    );
    assert!(
        success(
            path,
            &["query", "replacement_total", "--view", "definition"]
        )
        .contains("src/pricing.rs:1")
    );
    for stale in ["checkout_total", "apply_fee", "obsolete_helper"] {
        assert!(
            stderr(&cli(path, &["query", stale])).contains("symbol not found"),
            "{stale}"
        );
    }
}

#[test]
fn git_changes_and_codex_setup_work_in_a_repository() {
    let root = fixture();
    let path = root.path();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?} {}", stderr(&output));
    };
    git(&["init", "-q"]);
    fs::write(path.join("AGENTS.md"), "# Existing instructions\n").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Justless Test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "-qm",
        "fixture",
    ]);
    success(path, &["init-codex"]);
    success(path, &["init-codex"]);
    let agents = fs::read_to_string(path.join("AGENTS.md")).unwrap();
    assert!(agents.starts_with("# Existing instructions\n"));
    assert_eq!(agents.matches("<!-- justless:begin -->").count(), 1);
    git(&["check-ignore", ".justless/index.json"]);
    let pricing = path.join("src/pricing.rs");
    let previous = fs::read_to_string(&pricing).unwrap();
    fs::write(&pricing, previous.replace("fee + 1", "fee + 2")).unwrap();
    let changes = success(path, &["context", "--changes"]);
    assert!(changes.contains("Changed src/pricing.rs:6-6"), "{changes}");
    assert!(changes.contains("apply_fee [function]"), "{changes}");
    assert!(changes.contains("total_includes_fee"), "{changes}");
    git(&["add", "src/pricing.rs"]);
    assert!(success(path, &["context", "--changes", "--staged"]).contains("apply_fee"));
    fs::write(&pricing, &previous).unwrap();
    let staged = success(path, &["context", "--changes", "--staged"]);
    assert!(
        staged.contains("working tree differs from staged source"),
        "{staged}"
    );
    let output = cli(path, &["run", "--", "git", "diff", "--", "src/pricing.rs"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let id = run_id(&output);
    assert!(success(path, &["log", &id, "--stream", "stdout"]).contains("fee + 1"));
}

#[test]
fn log_replay_preserves_binary_streams_and_command_status() {
    let root = fixture();
    let path = root.path();
    let command = [
        "sh",
        "-c",
        "printf '\\001\\000hello\\377'; printf 'oops\\n' >&2; exit 23",
    ];
    let output = cli(path, &["run", "--", command[0], command[1], command[2]]);
    assert_eq!(output.status.code(), Some(23));
    let id = run_id(&output);
    let log = cli(path, &["log", &id]);
    assert_eq!(log.stdout, b"\x01\x00hello\xff");
    assert_eq!(log.stderr, b"oops\n");
    let raw = cli(
        path,
        &["run", "--raw", "--", command[0], command[1], command[2]],
    );
    assert_eq!(raw.status.code(), Some(23));
    assert_eq!(raw.stdout, log.stdout);
    assert_eq!(raw.stderr, log.stderr);
    assert!(!stderr(&cli(path, &["log", "../not-a-run"])).is_empty());
    assert!(!stderr(&cli(path, &["code", "../Cargo.toml", "--to", "1"])).is_empty());
    let state = PathBuf::from(path).join(".justless/runs");
    assert_eq!(fs::read_dir(state).unwrap().count(), 1);
}

#[test]
fn compiler_json_errors_link_to_source_and_the_original_message() {
    let root = fixture();
    let path = root.path();
    fs::write(
        path.join("src/pricing.rs"),
        "pub fn checkout_total(subtotal: u32, fee: u32) -> u32 {\n    subtotal + fee + \"wrong\"\n}\n",
    ).unwrap();
    let output = cli(
        path,
        &[
            "run",
            "--max-bytes",
            "512",
            "--",
            "cargo",
            "check",
            "--offline",
            "--message-format=json",
        ],
    );
    assert_eq!(output.status.code(), Some(101), "{}", stderr(&output));
    let preview = stdout(&output);
    assert!(preview.contains("error[E0277]"), "{preview}");
    assert!(preview.contains("src/pricing.rs:2"), "{preview}");
    let id = run_id(&output);
    let source = success(path, &["context", "--run", &id]);
    assert!(source.contains("src/pricing.rs:2"), "{source}");
    assert!(source.contains("subtotal + fee + \"wrong\""), "{source}");
    let full = success(path, &["log", &id, "--stream", "stdout"]);
    assert!(full.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .is_ok_and(|json| json["reason"] == "compiler-message")
    }));
}

#[test]
fn ambiguous_names_and_invalid_edits_cannot_return_stale_definitions() {
    let root = fixture();
    let path = root.path();
    let source = path.join("src/obsolete.rs");
    fs::write(&source, "pub fn apply_fee() {}\n").unwrap();
    let ambiguous = cli(path, &["query", "apply_fee"]);
    assert_ne!(ambiguous.status.code(), Some(0));
    let message = stderr(&ambiguous);
    assert!(message.contains("ambiguous symbol"), "{message}");
    assert!(message.contains("src/obsolete.rs:1"), "{message}");
    assert!(message.contains("src/pricing.rs:5-7"), "{message}");
    assert!(success(path, &["query", "pricing::apply_fee"]).contains("src/pricing.rs:5-7"));
    fs::write(&source, "pub fn apply_fee(\n").unwrap();
    let errors = cli(path, &["index"]);
    assert!(
        stderr(&errors).contains("src/obsolete.rs: line"),
        "{}",
        stderr(&errors)
    );
    assert!(success(path, &["query", "apply_fee"]).contains("src/pricing.rs:5-7"));
}

#[test]
fn mixed_repo_paths_and_exact_text_refresh_after_edits() {
    let repo = mixed_fixture();
    let root = repo.path();
    let index = cli(root, &["index"]);
    assert!(index.status.success(), "{}", stderr(&index));
    assert!(index.stderr.is_empty(), "{}", stderr(&index));
    assert!(
        stdout(&index).contains("6 text files"),
        "{}",
        stdout(&index)
    );
    let route = success(root, &["path", "fetchOrders", "purchase_orders"]);
    assert!(
        route.contains("HTTP route at frontend/orders.ts:4"),
        "{route}"
    );
    assert!(
        route.contains("list_orders [function] at backend/orders.py:7-8"),
        "{route}"
    );
    assert!(
        route.contains("SQL relation at backend/orders.py:8"),
        "{route}"
    );
    assert!(
        route.contains("purchase_orders [table] at db/schema.sql:1-3"),
        "{route}"
    );
    let imports = success(
        root,
        &["query", "frontend/orders.ts", "--view", "dependencies"],
    );
    assert!(
        imports.contains("formatOrder [function] at frontend/format.ts:1-3"),
        "{imports}"
    );
    let hits = success(root, &["find", "-i", "CANARY-CHECKOUT"]);
    assert!(hits.contains("docs/operations.md:3"), "{hits}");
    assert!(hits.contains("backend/settings.custom:1"), "{hits}");
    fs::write(root.join(".justlessignore"), "*.custom\n").unwrap();
    let hits = success(root, &["find", "canary-checkout"]);
    assert!(!hits.contains("settings.custom"), "{hits}");
    fs::write(
        root.join("frontend/orders.ts"),
        "export function replacementOrder() {}\n",
    )
    .unwrap();
    fs::remove_file(root.join("db/schema.sql")).unwrap();
    for symbol in ["fetchOrders", "purchase_orders"] {
        assert!(
            stderr(&cli(root, &["query", symbol])).contains("symbol not found"),
            "{symbol}"
        );
    }
    assert!(
        success(root, &["query", "replacementOrder", "--view", "definition"])
            .contains("frontend/orders.ts:1")
    );
}

#[test]
fn structural_parsers_cover_common_non_rust_languages() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    for (file, source, name) in [
        (
            "program.js",
            "export function jsThing() { return 1; }\n",
            "jsThing",
        ),
        (
            "view.tsx",
            "export function TsView() { return <div />; }\n",
            "TsView",
        ),
        (
            "main.go",
            "package main\nfunc GoThing() int { return 1 }\n",
            "GoThing",
        ),
        (
            "Demo.java",
            "class JavaThing { int javaMethod() { return 1; } }\n",
            "JavaThing",
        ),
        ("main.c", "int c_thing(void) { return 1; }\n", "c_thing"),
        ("main.cpp", "int cpp_thing() { return 1; }\n", "cpp_thing"),
        (
            "Demo.cs",
            "class CSharpThing { int Run() { return 1; } }\n",
            "CSharpThing",
        ),
        (
            "demo.rb",
            "class RubyThing\n  def run\n    1\n  end\nend\n",
            "RubyThing",
        ),
        (
            "demo.php",
            "<?php function php_thing() { return 1; }\n",
            "php_thing",
        ),
        (
            "demo.sh",
            "bash_thing() { printf 'hello\\n'; }\n",
            "bash_thing",
        ),
    ] {
        fs::write(root.join(file), source).unwrap();
        let output = success(root, &["query", name, "--view", "definition"]);
        assert!(output.contains(&format!("{file}:")), "{output}");
    }
}

fn language_fixture(language: &str) -> TempDir {
    let target = TempDir::new().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/languages")
        .join(language);
    for entry in fs::read_dir(source).unwrap() {
        let path = entry.unwrap().path();
        fs::copy(&path, target.path().join(path.file_name().unwrap())).unwrap();
    }
    target
}

struct LanguageCase<'a> {
    language: &'a str,
    file: &'a str,
    rows: &'a [&'a str],
    expand: (&'a str, &'a str),
    caller: (&'a str, &'a str),
    test: (&'a str, &'a str),
}

fn check_language(case: LanguageCase<'_>) -> TempDir {
    let repo = language_fixture(case.language);
    let root = repo.path();
    let outline = success(root, &["read", case.file, "--outline"]);
    for row in case.rows {
        assert!(outline.contains(&format!("{row}\n")), "{row}\n{outline}");
    }
    let (symbol, content) = case.expand;
    assert_eq!(success(root, &["expand", case.file, symbol]), content);
    let (symbol, caller) = case.caller;
    let callers = success(root, &["query", symbol, "--view", "callers"]);
    assert!(callers.contains(caller), "{callers}");
    let found = success(root, &["find", symbol]);
    assert!(found.contains(&format!("{}:", case.file)), "{found}");
    let (symbol, test) = case.test;
    let tests = success(root, &["query", symbol, "--view", "tests"]);
    assert!(tests.contains(test), "{tests}");
    repo
}

#[test]
fn kotlin_outlines_calls_and_junit_tests_share_the_jvm_index() {
    let repo = check_language(LanguageCase {
        language: "kotlin",
        file: "Billing.kt",
        rows: &[
            "#3 L7-9 interface Ledger  interface Ledger",
            "#5 L11-23 class InvoiceService  class InvoiceService(private val ledger: Ledger) : Ledger by ledger",
            "#6 L12-12 property InvoiceService::currency  val currency = \"EUR\"",
            "#7 L14-18 method InvoiceService::total  fun total(items: List<Cents>): Cents",
            "#8 L21-21 method InvoiceService::create  fun create(ledger: Ledger): InvoiceService",
            "#9 L25-25 function applyTax  fun applyTax(amount: Cents): Cents",
        ],
        expand: (
            "applyTax",
            "fun applyTax(amount: Cents): Cents = amount * 2",
        ),
        caller: (
            "applyTax",
            "call from Billing.kt::InvoiceService::total at Billing.kt:17",
        ),
        test: (
            "total",
            "related test (1 call hops): BillingTest.kt::BillingTest::totalsItems [test]",
        ),
    });
    let root = repo.path();
    // Kotlin and Java resolve calls to each other, and JUnit marks Java tests too.
    fs::write(
        root.join("TaxTest.java"),
        "class TaxTest {\n  @org.junit.Test\n  void doubles() { BillingKt.applyTax(2); }\n}\n",
    )
    .unwrap();
    let tests = success(root, &["query", "applyTax", "--view", "tests"]);
    assert!(
        tests.contains("related test (1 call hops): TaxTest.java::TaxTest::doubles [test]"),
        "{tests}"
    );
}

#[test]
fn swift_extensions_extend_their_type_and_xctest_methods_are_tests() {
    let repo = check_language(LanguageCase {
        language: "swift",
        file: "Billing.swift",
        rows: &[
            "#3 L5-7 protocol Ledger  protocol Ledger",
            "#5 L9-12 struct Invoice  struct Invoice",
            "#8 L14-26 class InvoiceService  final class InvoiceService",
            "#10 L17-19 constructor InvoiceService::init  init(ledger: Ledger)",
            "#11 L21-25 method InvoiceService::total  func total(_ items: [Cents]) -> Cents",
            "#12 L29-29 method InvoiceService::reset  func reset()",
            "#13 L32-34 function applyTax  func applyTax(_ amount: Cents) -> Cents",
        ],
        expand: ("InvoiceService::reset", "func reset() {}"),
        caller: (
            "applyTax",
            "call from Billing.swift::InvoiceService::total at Billing.swift:24",
        ),
        test: (
            "total",
            "related test (1 call hops): BillingTests.swift::BillingTests::testTotalsItems [test]",
        ),
    });
    let definition = success(
        repo.path(),
        &["query", "InvoiceService", "--view", "definition"],
    );
    assert_eq!(
        definition,
        "Billing.swift::InvoiceService [class] at Billing.swift:14-26\n"
    );
}

#[test]
fn scala_companions_and_test_dsl_calls_are_indexed() {
    let repo = check_language(LanguageCase {
        language: "scala",
        file: "Billing.scala",
        rows: &[
            "#3 L7-9 trait Ledger  trait Ledger",
            "#5 L11-11 class Invoice  case class Invoice(id: String, total: Cents)",
            "#6 L13-21 class InvoiceService  class InvoiceService(ledger: Ledger)",
            "#8 L16-20 method InvoiceService::total  def total(items: List[Cents]): Cents =",
            "#9 L24-24 method InvoiceService::create  def create(ledger: Ledger): InvoiceService =",
            "#10 L27-27 function applyTax  def applyTax(amount: Cents): Cents =",
        ],
        expand: (
            "InvoiceService::create",
            "def create(ledger: Ledger): InvoiceService = new InvoiceService(ledger)",
        ),
        caller: (
            "applyTax",
            "call from Billing.scala::InvoiceService::total at Billing.scala:19",
        ),
        test: (
            "create",
            "related test (1 call hops): BillingSuite.scala::BillingSuite::\"totals items\" [test]",
        ),
    });
    let suite = success(repo.path(), &["read", "BillingSuite.scala", "--outline"]);
    assert!(
        suite.contains("#3 L6-9 test BillingSuite::\"totals items\"  test(\"totals items\") {\n"),
        "{suite}"
    );
}

#[test]
fn dart_constructors_and_test_calls_are_indexed() {
    let repo = check_language(LanguageCase {
        language: "dart",
        file: "billing.dart",
        rows: &[
            "#3 L5-7 class Ledger  abstract class Ledger",
            "#4 L6-6 method Ledger::record  void record(Cents amount);",
            "#6 L10-10 field InvoiceService::ledger  final Ledger ledger;",
            "#7 L12-12 constructor InvoiceService::new  InvoiceService(this.ledger);",
            "#9 L16-20 method InvoiceService::total  Cents total(List<Cents> items)",
            "#10 L23-25 function applyTax  Cents applyTax(Cents amount)",
        ],
        expand: (
            "InvoiceService::standard",
            "InvoiceService.standard() : ledger = MemoryLedger();",
        ),
        caller: (
            "applyTax",
            "call from billing.dart::InvoiceService::total at billing.dart:19",
        ),
        test: (
            "total",
            "related test (1 call hops): billing_test.dart::main::\"totals items\" [test]",
        ),
    });
    let definition = success(
        repo.path(),
        &["query", "InvoiceService", "--view", "definition"],
    );
    assert_eq!(
        definition,
        "billing.dart::InvoiceService [class] at billing.dart:9-21\n"
    );
    // A test described by a function name stays distinct from that function.
    let tests = success(repo.path(), &["query", "applyTax", "--view", "tests"]);
    assert!(
        tests.starts_with("billing.dart::applyTax [function]")
            && tests.contains(
                "related test (1 call hops): billing_test.dart::main::\"applyTax\" [test]"
            ),
        "{tests}"
    );
}

#[test]
fn lua_and_zig_outlines_link_calls_and_tests() {
    check_language(LanguageCase {
        language: "lua",
        file: "billing.lua",
        rows: &[
            "#1 L3-5 function apply_tax  local function apply_tax(amount)",
            "#2 L7-13 function total  function M.total(items)",
            "#3 L15-17 function format_total  local format_total = function(amount)",
            "#4 L19-21 method describe  function M:describe()",
        ],
        expand: (
            "format_total",
            "local format_total = function(amount)\n  return string.format(\"%d\", amount)\nend",
        ),
        caller: (
            "apply_tax",
            "call from billing.lua::total at billing.lua:12",
        ),
        test: (
            "total",
            "related test (1 call hops): billing_spec.lua::\"totals items\" [test]",
        ),
    });
    let repo = check_language(LanguageCase {
        language: "zig",
        file: "billing.zig",
        rows: &[
            "#2 L3-9 struct Invoice  pub const Invoice = struct",
            "#3 L6-8 function Invoice::add  pub fn add(self: *Invoice, amount: u64) void",
            "#4 L11-13 function applyTax  fn applyTax(amount: u64) u64",
            "#5 L15-19 test \"adds tax\"  test \"adds tax\"",
            "#6 L21-23 test \"applyTax\"  test applyTax",
        ],
        expand: (
            "Invoice::add",
            "pub fn add(self: *Invoice, amount: u64) void {\n        self.total += applyTax(amount);\n    }",
        ),
        caller: (
            "applyTax",
            "call from billing.zig::Invoice::add at billing.zig:7",
        ),
        test: (
            "add",
            "related test (1 call hops): billing.zig::\"adds tax\" [test]",
        ),
    });
    // `test applyTax` documents the function without shadowing it.
    let callers = success(repo.path(), &["query", "applyTax", "--view", "callers"]);
    assert!(
        callers.contains("call from billing.zig::\"applyTax\" at billing.zig:22"),
        "{callers}"
    );
}

#[test]
fn terraform_blocks_use_addresses_and_link_references() {
    let repo = language_fixture("terraform");
    let root = repo.path();
    let outline = success(root, &["read", "main.tf", "--outline"]);
    for row in [
        "#1 L1-3 terraform terraform  terraform\n",
        "#2 L5-7 provider provider.aws  provider \"aws\"\n",
        "#3 L10-10 local local.bucket_name  bucket_name = \"${var.prefix}-logs\"\n",
        "#4 L13-15 resource aws_s3_bucket.logs  resource \"aws_s3_bucket\" \"logs\"\n",
        "#5 L17-21 data data.aws_iam_policy_document.read  data \"aws_iam_policy_document\" \"read\"\n",
        "#6 L23-26 module module.vpc  module \"vpc\"\n",
    ] {
        assert!(outline.contains(row), "{row}{outline}");
    }
    let variables = success(root, &["read", "variables.tf", "--outline"]);
    assert!(
        variables.contains("#1 L1-4 variable var.region  variable \"region\"\n"),
        "{variables}"
    );
    assert_eq!(
        success(root, &["expand", "main.tf", "aws_s3_bucket.logs"]),
        "resource \"aws_s3_bucket\" \"logs\" {\n  bucket = local.bucket_name\n}"
    );
    for (symbol, expected) in [
        (
            "aws_s3_bucket.logs",
            "  reference from main.tf::data.aws_iam_policy_document.read at main.tf:19\n",
        ),
        (
            "aws_s3_bucket.logs",
            "  possible reference from outputs.tf::output.bucket_arn at outputs.tf:2\n",
        ),
        (
            "local.bucket_name",
            "  reference from main.tf::aws_s3_bucket.logs at main.tf:14\n",
        ),
        (
            "var.region",
            "  possible reference from main.tf::provider.aws at main.tf:6\n",
        ),
        (
            "data.aws_iam_policy_document.read",
            "  possible reference from outputs.tf::output.read_policy at outputs.tf:6\n",
        ),
        (
            "module.vpc",
            "  possible reference from outputs.tf::output.read_policy at outputs.tf:7\n",
        ),
    ] {
        let references = success(root, &["query", symbol, "--view", "references"]);
        assert!(references.contains(expected), "{symbol}: {references}");
    }
    let found = success(root, &["find", "aws_s3_bucket.logs"]);
    assert!(found.contains("outputs.tf:2:"), "{found}");
}

fn hook(root: &Path, mode: &str, input: serde_json::Value) -> serde_json::Value {
    agent_hook(root, "hook-codex", mode, input)
}

fn agent_hook(
    root: &Path,
    handler: &str,
    mode: &str,
    input: serde_json::Value,
) -> serde_json::Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
        .args([handler, mode])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn codex_hooks_merge_and_use_the_documented_tool_contract() {
    let repo = mixed_fixture();
    let root = repo.path();
    fs::create_dir(root.join(".codex")).unwrap();
    let config = root.join(".codex/hooks.json");
    fs::write(&config, r#"{"description":"preserve","hooks":{"PreToolUse":[{"matcher":"^Other$","hooks":[{"type":"command","command":"echo independent"}]}]}}"#).unwrap();
    success(root, &["init", "--hooks"]);
    let installed = fs::read_to_string(&config).unwrap();
    success(root, &["init-codex", "--hooks"]);
    assert_eq!(installed, fs::read_to_string(&config).unwrap());
    let parsed: serde_json::Value = serde_json::from_str(&installed).unwrap();
    assert_eq!(parsed["description"], "preserve");
    assert_eq!(parsed["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    assert_eq!(
        parsed["hooks"]["SessionStart"][0]["hooks"][0]["async"],
        true
    );
    assert_eq!(
        parsed["hooks"]["PreToolUse"][1]["hooks"][0]["command"],
        "justless hook-codex before-command"
    );

    let start = hook(
        root,
        "session-start",
        serde_json::json!({"hook_event_name":"SessionStart"}),
    );
    assert!(
        start["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("Justless indexed")
    );
    assert!(root.join(".justless/index.json").is_file());
    let call = |command: &str| {
        serde_json::json!({
            "hook_event_name":"PreToolUse", "tool_name":"Bash", "tool_cwd": root,
            "tool_input":{"command":command,"workdir":root}
        })
    };
    let rewritten = hook(
        root,
        "before-command",
        call("rg 'fetch|list_orders' frontend"),
    );
    assert_eq!(
        rewritten["hookSpecificOutput"]["permissionDecision"],
        "allow"
    );
    assert_eq!(
        rewritten["hookSpecificOutput"]["updatedInput"]["command"],
        "justless run --hook -- rg 'fetch|list_orders' frontend"
    );
    assert_eq!(
        rewritten["hookSpecificOutput"]["updatedInput"]["workdir"],
        root.to_str().unwrap()
    );
    assert!(
        rewritten["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("justless query")
    );
    for command in [
        "cargo test && git push",
        "git push origin main",
        "rg $(echo hello)",
        "rg --json fetch frontend",
        "git status --porcelain=v1",
        "rg fetch > matches.txt",
        "npm test -- --watch",
        "justless run --hook -- rg fetch",
        "rg fetch frontend/*.ts",
    ] {
        assert_eq!(
            hook(root, "before-command", call(command)),
            serde_json::json!({}),
            "{command}"
        );
    }
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(git.status.success(), "{}", stderr(&git));
    let rewritten = hook(root, "before-command", call("git status --short"));
    let command = rewritten["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_justless"));
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(binary.parent().unwrap().to_path_buf())
            .chain(std::env::split_paths(&inherited)),
    )
    .unwrap();
    let output = Command::new("sh")
        .args(["-c", command])
        .env("PATH", path)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("frontend/"), "{}", stdout(&output));
    // A hook-routed run that reached the agent verbatim prints no footer, but it is
    // still archived and listed.
    assert!(
        !stderr(&output).contains("[justless"),
        "{}",
        stderr(&output)
    );
    let latest = success(root, &["history"]);
    let id = latest.split_whitespace().next().unwrap();
    assert!(
        root.join(".justless/runs")
            .join(id)
            .join("stdout")
            .is_file()
    );
}

#[test]
fn install_and_run_claude_and_agy_hooks() {
    use serde_json::{Value, json};
    let repo = mixed_fixture();
    let root = repo.path().canonicalize().unwrap();
    fs::create_dir(root.join(".claude")).unwrap();
    let claude = root.join(".claude/settings.json");
    let agy = root.join(".agents/hooks.json");
    fs::create_dir(root.join(".agents")).unwrap();
    fs::write(
        &claude,
        r#"{"permissions":{"defaultMode":"default"},"hooks":{"SessionStart":[]}}"#,
    )
    .unwrap();
    fs::write(
        &agy,
        r#"{"other":{"enabled":false},"justless":{"PreToolUse":"invalid"}}"#,
    )
    .unwrap();
    let setup = ["init", "--hooks", "--agent", "claude", "--agent", "agy"];
    assert!(!cli(&root, &setup).status.success());
    assert!(!root.join("CLAUDE.md").exists());
    fs::write(&agy, r#"{"other":{"enabled":false}}"#).unwrap();
    fs::write(root.join("CLAUDE.md"), "# Team rules\n").unwrap();
    success(&root, &setup);
    let configs = || {
        [
            fs::read_to_string(&claude).unwrap(),
            fs::read_to_string(&agy).unwrap(),
        ]
    };
    let installed = configs();
    success(&root, &setup);
    assert_eq!(installed, configs());
    assert!(!root.join(".codex").exists());
    assert!(root.join("AGENTS.md").is_file());
    let instructions = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
    assert!(instructions.starts_with("# Team rules\n"));
    assert_eq!(instructions.matches("<!-- justless:begin -->").count(), 1);
    let [claude, agy] = installed.map(|s| serde_json::from_str::<Value>(&s).unwrap());
    assert_eq!(claude["permissions"]["defaultMode"], "default");
    assert_eq!(
        claude["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "justless hook-claude before-command"
    );
    assert_eq!(agy["other"]["enabled"], false);
    assert_eq!(
        agy["justless"]["SessionStart"][0]["command"],
        "justless hook-agy session-start"
    );
    assert_eq!(agy["justless"]["PreToolUse"][0]["matcher"], "^run_command$");
    let claude_input = json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":root,"tool_input":{"command":"git diff","description":"show changes"}});
    let output = agent_hook(&root, "hook-claude", "before-command", claude_input.clone());
    let hook = &output["hookSpecificOutput"];
    assert_eq!(
        hook["updatedInput"],
        json!({"command":"justless run --hook -- git diff","description":"show changes"})
    );
    assert!(hook.get("permissionDecision").is_none());
    let mut plan = claude_input;
    plan["permission_mode"] = json!("plan");
    assert_eq!(
        agent_hook(&root, "hook-claude", "before-command", plan),
        json!({})
    );
    let elsewhere = TempDir::new().unwrap();
    let agy_input = |command: &str, cwd: &Path| json!({"workspacePaths":[root],"toolCall":{"name":"run_command","args":{"CommandLine":command,"Cwd":cwd,"WaitMsBeforeAsync":10}}});
    let dispatch = |command: &str, cwd: &Path| {
        agent_hook(
            elsewhere.path(),
            "hook-agy",
            "before-command",
            agy_input(command, cwd),
        )
    };
    assert_eq!(
        dispatch("git diff", &root),
        json!({"decision":"allow","overwrite":{"CommandLine":"justless run --hook -- git diff"}})
    );
    assert_eq!(
        dispatch("git diff", elsewhere.path()),
        json!({"decision":"allow"})
    );
    assert_eq!(
        dispatch("git diff | cat", &root),
        json!({"decision":"allow"})
    );
    let mut multiple = agy_input("git diff", &root);
    multiple["workspacePaths"] = json!([elsewhere.path(), root]);
    let result = agent_hook(
        elsewhere.path(),
        "hook-agy",
        "before-command",
        multiple.clone(),
    );
    assert_eq!(
        result["overwrite"]["CommandLine"],
        "justless run --hook -- git diff"
    );
    multiple["toolCall"]["args"]["RunPersistent"] = json!(true);
    assert_eq!(
        agent_hook(&root, "hook-agy", "before-command", multiple),
        json!({"decision":"allow"})
    );
    let startup = agent_hook(
        elsewhere.path(),
        "hook-agy",
        "session-start",
        json!({"workspacePaths":[root]}),
    );
    assert!(
        startup["injectSteps"][0]["ephemeralMessage"]
            .as_str()
            .unwrap()
            .contains("Justless indexed")
    );
    assert!(root.join(".justless/index.json").is_file());
    assert!(!elsewhere.path().join(".justless").exists());
}

#[test]
fn non_rust_diagnostics_and_project_budgets_work() {
    let repo = mixed_fixture();
    let root = repo.path();
    fs::write(
        root.join(".justless.toml"),
        "context_budget = 320\nmax_output_bytes = 512\nmax_file_bytes = 1000\n",
    )
    .unwrap();
    let output = cli(
        root,
        &[
            "run",
            "--",
            "sh",
            "-c",
            r#"printf 'Traceback (most recent call last):\n  File "backend/orders.py", line 8, in list_orders\nRuntimeError: synthetic\n' >&2; exit 7"#,
        ],
    );
    assert_eq!(output.status.code(), Some(7), "{}", stderr(&output));
    let id = run_id(&output);
    let compact = success(root, &["context", "--run", &id]);
    assert!(compact.len() <= 320, "{compact}");
    let full_context = success(root, &["context", "--run", &id, "--budget", "2000"]);
    assert!(
        full_context.contains("backend/orders.py:8"),
        "{full_context}"
    );
    assert!(
        full_context.contains("purchase_orders [table]"),
        "{full_context}"
    );
    let docs = root.join("docs/operations.md");
    fs::write(&docs, "old marker").unwrap();
    assert!(success(root, &["find", "old marker"]).contains("docs/operations.md"));
    fs::write(&docs, "new marker").unwrap();
    assert!(success(root, &["find", "new marker"]).contains("docs/operations.md"));
    assert!(!success(root, &["find", "old marker"]).contains("docs/operations.md"));
    fs::write(&docs, "new marker".repeat(120)).unwrap();
    assert!(!success(root, &["find", "new marker"]).contains("docs/operations.md"));
}

#[test]
fn diff_sections_delta_sessions_and_exact_recall() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let git = |args: &[&str]| {
        let result = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(result.status.success(), "{}", stderr(&result));
        result
    };
    git(&["init", "-q"]);
    for file in ["first.txt", "second.txt"] {
        fs::write(root.join(file), "old\n").unwrap();
        git(&["add", file]);
        fs::write(root.join(file), "a long new line of evidence\n".repeat(90)).unwrap();
    }
    let args = [
        "run",
        "--session",
        "one",
        "--max-bytes",
        "512",
        "--",
        "git",
        "diff",
    ];
    let first = cli(root, &args);
    let id = run_id(&first);
    assert!(stdout(&first).contains("first.txt") && stdout(&first).contains("second.txt"));
    let same = success(root, &args);
    assert!(same.contains(&format!("unchanged since {id}")));
    let another = success(
        root,
        &[
            "run",
            "--session",
            "two",
            "--max-bytes",
            "512",
            "--",
            "git",
            "diff",
        ],
    );
    assert!(another.contains("second.txt") && !another.contains("since"));
    fs::write(root.join("first.txt"), "different hunk\n".repeat(93)).unwrap();
    let delta = cli(root, &args);
    assert!(stdout(&delta).contains("+ first.txt") && !stdout(&delta).contains("second.txt"));
    let changed = run_id(&delta);
    let exact = cli(root, &["log", &changed, "--stream", "stdout"]);
    assert_eq!(exact.stdout, git(&["diff"]).stdout);
    let oldest = success(
        root,
        &["history", "a long new line", "--oldest", "--limit", "1"],
    );
    assert!(oldest.contains(&id) && oldest.contains("stdout:"));
    let stats = success(root, &["stats"]);
    assert!(
        stats.contains("1/4 reopened in full") && stats.contains("1 unchanged"),
        "{stats}"
    );
    assert!(
        success(
            root,
            &[
                "run",
                "--session",
                "one",
                "--fresh",
                "--max-bytes",
                "512",
                "--",
                "git",
                "diff"
            ]
        )
        .contains("second.txt")
    );
    fs::write(root.join("first.txt"), "old\n").unwrap();
    fs::write(root.join("second.txt"), "old\n").unwrap();
    let removed = success(root, &args);
    assert!(removed.contains("- first.txt") && removed.contains("- second.txt"));
}

#[test]
fn task_packs_and_handoffs_link_live_and_historical_evidence() {
    let mixed = mixed_fixture();
    let two_seeds = success(
        mixed.path(),
        &["pack", "trace fetchOrders to purchase_orders"],
    );
    assert!(
        two_seeds.contains("HTTP route") && two_seeds.contains("SQL relation"),
        "{two_seeds}"
    );
    let pack = success(
        mixed.path(),
        &["pack", "What breaks if I rename purchase_orders?"],
    );
    for term in [
        "schema.sql",
        "list_orders",
        "fetchOrders",
        "possible HTTP",
        "SELECT order_id",
    ] {
        assert!(pack.contains(term), "{pack}");
    }
    assert!(
        success(
            mixed.path(),
            &["pack", "purchase_orders", "--budget", "128"]
        )
        .len()
            <= 128
    );
    assert!(success(mixed.path(), &["pack", "canary-checkout"]).contains("docs/operations.md"));
    let repo = fixture();
    let root = repo.path();
    let args = [
        "run",
        "--max-bytes",
        "512",
        "--",
        "cargo",
        "test",
        "--offline",
    ];
    let failed = cli(root, &args);
    assert_eq!(failed.status.code(), Some(101));
    let id = run_id(&failed);
    let evidence = success(root, &["pack", "diagnose the failing checkout test"]);
    for term in [
        &id,
        "tests/checkout.rs:11",
        "apply_fee",
        "subtotal + fee + 1",
    ] {
        assert!(evidence.contains(term), "{evidence}");
    }
    let slice = ["code", "src/pricing.rs", "--from", "5", "--to", "7"];
    success(root, &slice);
    success(root, &slice);
    let source = root.join("src/pricing.rs");
    let text = fs::read_to_string(&source).unwrap();
    fs::write(source, text.replace("subtotal + fee + 1", "subtotal + fee")).unwrap();
    let handoff = success(root, &["resume"]);
    assert!(
        handoff.contains(&id) && handoff.contains("src/pricing.rs:5-7 [source changed]"),
        "{handoff}"
    );
    let fixed = cli(root, &args);
    assert_eq!(fixed.status.code(), Some(0));
    assert!(
        stdout(&fixed).contains("- total_includes_fee"),
        "{}",
        stdout(&fixed)
    );
    assert!(
        !success(root, &["pack", "why did checkout fail"]).contains("Recorded cargo test failure")
    );
    assert!(success(root, &["history", "assertion", "--oldest", "--limit", "1"]).contains(&id));
    assert!(success(root, &["stats"]).contains("1 repeated source-range read(s)"));
}

#[test]
fn older_runs_are_searchable_and_hook_statistics_do_not_capture_arguments() {
    let repo = mixed_fixture();
    let root = repo.path();
    let run = root.join(".justless/runs/run-older");
    fs::create_dir_all(&run).unwrap();
    fs::write(run.join("record.json"),r#"{"id":"run-older","status":1,"raw_bytes":8,"preview_bytes":8,"diagnostics":[],"source_digests":{}}"#).unwrap();
    fs::write(run.join("stdout"), "old log\n").unwrap();
    fs::write(run.join("stderr"), "").unwrap();
    assert!(success(root, &["history", "old log"]).contains("run-older legacy stdout:1"));
    assert!(success(root, &["resume"]).contains("run-older · legacy"));
    let call = |command: &str| serde_json::json!({"hook_event_name":"PreToolUse","tool_name":"Bash","session_id":"odd ' input","tool_input":{"command":command}});
    hook(
        root,
        "before-command",
        call("git status --porcelain secret-value"),
    );
    let result = hook(root, "before-command", call("git status"));
    let wrapped = result["hookSpecificOutput"]["updatedInput"]["command"]
        .as_str()
        .unwrap();
    assert!(
        wrapped.starts_with("justless run --hook --session ")
            && wrapped.ends_with(" -- git status")
            && !wrapped.contains("odd")
    );
    assert!(success(root, &["stats"]).contains("Hooks passed through: git ×1."));
    assert!(
        !fs::read_to_string(root.join(".justless/activity.json"))
            .unwrap()
            .contains("secret-value")
    );
}

#[cfg(unix)]
#[test]
fn pytest_summaries_group_shared_diagnostics_and_preserve_failure_exit() {
    use std::os::unix::fs::PermissionsExt;
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let exe = root.join("pytest");
    fs::write(&exe, "#!/bin/sh\ncat results\nexit 1\n").unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
    let filler = "progress\n".repeat(100);
    fs::write(root.join("results"), format!("{filler}FAILED orders.py::one - TimeoutError\nFAILED orders.py::two - TimeoutError\nFAILED orders.py::three - ValueError\n3 failed in 0.1s\n")).unwrap();
    let args = ["run", "--max-bytes", "512", "--", exe.to_str().unwrap()];
    let failed = cli(root, &args);
    let preview = stdout(&failed);
    assert_eq!(failed.status.code(), Some(1));
    assert!(
        preview.contains("3 reported failing")
            && preview.contains("2 test(s)")
            && preview.matches("TimeoutError").count() == 1,
        "{preview}"
    );
    fs::write(
        root.join("results"),
        format!("{filler}FAILED orders.py::two - TimeoutError\n1 failed in 0.1s\n"),
    )
    .unwrap();
    let next = cli(root, &args);
    assert!(
        stdout(&next).contains("- orders.py::one") && !stdout(&next).contains("- orders.py::two")
    );
    fs::write(root.join("results"), "unrecognized fatal runner output\n").unwrap();
    let unknown = cli(root, &args);
    assert_eq!(unknown.status.code(), Some(1));
    assert_eq!(stdout(&unknown), "unrecognized fatal runner output\n");
}

#[test]
fn history_finds_an_exact_match_across_large_line_windows() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let text = format!(
        "{}split-match{}\nnext\n",
        "x".repeat(256 * 1024 - 3),
        "y".repeat(500_000)
    );
    fs::write(root.join("big.txt"), text).unwrap();
    let run = cli(root, &["run", "--", "cat", "big.txt"]);
    let id = run_id(&run);
    assert!(success(root, &["search", &id, "split-match"]).contains("stdout:1:"));
    assert!(success(root, &["history", "split-match"]).contains("split-match"));
    assert!(success(root, &["history", "next"]).contains("stdout:2:"));
}

#[test]
fn concurrent_runs_publish_complete_archives_and_keep_workdirs_separate() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
    let a = root.join("a");
    let b = root.join("b");
    for folder in [&a, &b] {
        fs::create_dir(folder).unwrap();
        fs::write(folder.join("report"), "evidence\n".repeat(80)).unwrap();
    }
    let args = ["run", "--", "cat", "report"];
    let runs = std::thread::scope(|scope| {
        let tasks: Vec<_> = (0..4).map(|_| scope.spawn(|| cli(&a, &args))).collect();
        tasks
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(
        runs.iter()
            .filter(|r| stdout(r).contains("Output unchanged"))
            .count(),
        3
    );
    for run in &runs {
        assert!(run.status.success());
        assert_eq!(
            success(root, &["log", &run_id(run), "--stream", "stdout"]),
            "evidence\n".repeat(80)
        );
    }
    assert_eq!(success(&b, &args), "evidence\n".repeat(80));
    assert_eq!(success(root, &["history"]).lines().count(), 5);
}

#[cfg(unix)]
fn fake_program(dir: &Path, name: &str, script: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let exe = bin.join(name);
    fs::write(&exe, format!("#!/bin/sh\n{script}")).unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
    exe
}

#[cfg(unix)]
#[test]
fn container_and_cluster_tables_list_only_rows_that_need_attention() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    fs::write(
        root.join("ps.txt"),
        concat!(
            "CONTAINER ID   IMAGE         COMMAND   CREATED       STATUS                       PORTS     NAMES\n",
            "a1b2c3d4e5f6   nginx:1.27    \"nginx\"   2 hours ago   Up 2 hours (healthy)         80/tcp    web\n",
            "b2c3d4e5f6a1   postgres:16   \"pg\"      2 hours ago   Up 2 hours (unhealthy)                 db\n",
            "c3d4e5f6a1b2   redis:7       \"redis\"   3 hours ago   Exited (137) 5 minutes ago             cache\n",
            "d4e5f6a1b2c3   busybox       \"sh\"      3 hours ago   Up 3 hours                             worker\n",
        ),
    )
    .unwrap();
    fs::write(
        root.join("pods.txt"),
        concat!(
            "NAMESPACE   NAME          READY   STATUS             RESTARTS   AGE\n",
            "prod        api-abcde     1/1     Running            0          3d\n",
            "prod        api-fghij     0/1     CrashLoopBackOff   12         3d\n",
            "jobs        migrate-xyz   0/1     Completed          0          1d\n",
        ),
    )
    .unwrap();
    let docker = fake_program(root, "docker", "cat ps.txt\n");
    let kubectl = fake_program(root, "kubectl", "cat pods.txt\nexit ${FAIL:-0}\n");

    let ps = success(root, &["run", "--", docker.to_str().unwrap(), "ps", "-a"]);
    assert!(
        ps.starts_with("docker ps: 4 listed, 2 need attention\n")
            && ps.contains("db unhealthy [stdout:3-3]")
            && ps.contains("cache Exited (137) [stdout:4-4]")
            && ps.contains("Other examples: web, worker")
            && !ps.contains("nginx:1.27"),
        "{ps}"
    );
    let again = success(root, &["run", "--", docker.to_str().unwrap(), "ps", "-a"]);
    assert!(again.starts_with("Output unchanged since run-"), "{again}");

    let get = ["run", "--", kubectl.to_str().unwrap(), "get", "pods", "-A"];
    let pods = success(root, &get);
    assert!(
        pods.contains("kubectl get pods: 3 listed, 1 need attention")
            && pods.contains("prod/api-fghij CrashLoopBackOff ready=0/1 restarts=12")
            && !pods.contains("prod/api-abcde Running"),
        "{pods}"
    );

    // Formats Justless does not understand keep the ordinary preview.
    let wide = success(
        root,
        &[
            "run",
            "--",
            kubectl.to_str().unwrap(),
            "get",
            "pods",
            "-o",
            "wide",
        ],
    );
    assert_eq!(wide, fs::read_to_string(root.join("pods.txt")).unwrap());
    let failed = Command::new(env!("CARGO_BIN_EXE_justless"))
        .args(get)
        .current_dir(root)
        .env("FAIL", "1")
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        stdout(&failed),
        fs::read_to_string(root.join("pods.txt")).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn js_tests_logs_and_project_filters_keep_failure_evidence() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let npm = fake_program(
        root,
        "npm",
        concat!(
            "i=0; while [ $i -lt 40 ]; do echo \"PASS src/m$i.test.js\"; i=$((i+1)); done\n",
            "echo 'FAIL src/cart.test.js'\n",
            "echo '  Expected: 30'\n",
            "echo '  Received: 27'\n",
            "echo 'Tests: 1 failed, 40 passed, 41 total'\n",
            "exit 1\n",
        ),
    );
    let tests = cli(root, &["run", "--", npm.to_str().unwrap(), "test"]);
    let preview = stdout(&tests);
    assert_eq!(tests.status.code(), Some(1));
    assert!(
        preview.starts_with("[4 retained lines, 40 filtered, 0 repeats]\n")
            && preview.contains("FAIL src/cart.test.js\n  Expected: 30\n  Received: 27\n")
            && !preview.contains("PASS "),
        "{preview}"
    );
    let id = run_id(&tests);
    assert_eq!(
        success(
            root,
            &["log", &id, "--stream", "stdout", "--from", "1", "--to", "1"]
        ),
        "PASS src/m0.test.js\n"
    );

    let docker = fake_program(
        root,
        "docker",
        "for i in 1 2 3 4 5; do echo 'GET /health 200'; done\necho 'ERROR db refused'\n",
    );
    let logs = success(
        root,
        &["run", "--", docker.to_str().unwrap(), "logs", "api"],
    );
    assert_eq!(
        logs,
        "[2 retained lines, 0 filtered, 4 repeats]\nGET /health 200\nERROR db refused\n"
    );

    let make = fake_program(
        root,
        "make",
        concat!(
            "for i in 1 2 3; do echo \"make[1]: Entering directory '/w/$i'\"; echo \"cc -c f$i.c\"; done\n",
            "echo \"f3.c:10: error: expected ';'\"\n",
            "exit 2\n",
        ),
    );
    fs::write(
        root.join(".justless.toml"),
        "[[filters]]\ncommand = [\"make\"]\nomit = '^make\\[\\d+\\]: Entering'\n",
    )
    .unwrap();
    let built = cli(root, &["run", "--", make.to_str().unwrap()]);
    assert_eq!(built.status.code(), Some(2));
    assert_eq!(
        stdout(&built),
        "[4 retained lines, 3 filtered, 0 repeats]\ncc -c f1.c\ncc -c f2.c\ncc -c f3.c\nf3.c:10: error: expected ';'\n"
    );

    for (rule, message) in [
        ("command = []\nomit = 'x'", "nonempty argv prefix"),
        (
            "command = ['make']\nomit = '.*'",
            "must not match empty text",
        ),
        (
            "command = ['make']\nomit = '('",
            "invalid filters.omit regex",
        ),
    ] {
        fs::write(
            root.join(".justless.toml"),
            format!("[[filters]]\n{rule}\n"),
        )
        .unwrap();
        let rejected = cli(root, &["run", "--", make.to_str().unwrap()]);
        assert!(
            !rejected.status.success() && stderr(&rejected).contains(message),
            "{rule}: {}",
            stderr(&rejected)
        );
    }
}

#[test]
fn hooks_route_display_commands_and_leave_machine_formats_alone() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let call = |command: &str| serde_json::json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":root,"tool_input":{"command":command}});
    for command in [
        "docker ps -a",
        "docker container ls --filter status=exited",
        "kubectl get pods -A",
        "kubectl -n prod get deploy",
        "kubectl logs api-1 --tail 50",
        "docker logs --since 10m api",
        "npm test",
        "pnpm run test",
        "vitest run",
        "node --test",
    ] {
        let output = agent_hook(root, "hook-claude", "before-command", call(command));
        assert_eq!(
            output["hookSpecificOutput"]["updatedInput"]["command"],
            format!("justless run --hook -- {command}"),
            "{command}"
        );
    }
    for command in [
        "docker ps --format json",
        "docker ps -q",
        "kubectl get svc",
        "kubectl get pods -o yaml",
        "kubectl logs -f api-1",
        "npm test -- --watch",
        "vitest",
        "jest --json",
        "docker ps | grep db",
    ] {
        assert_eq!(
            agent_hook(root, "hook-claude", "before-command", call(command)),
            serde_json::json!({}),
            "{command}"
        );
    }
}

#[test]
fn compound_commands_wrap_each_supported_segment_in_place() {
    let repo = TempDir::new().unwrap();
    let root = repo.path().canonicalize().unwrap();
    fs::create_dir(root.join("web")).unwrap();
    let call = |command: &str| serde_json::json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":root,"tool_input":{"command":command}});
    let rewrite = |agent: &str, command: &str| {
        agent_hook(&root, agent, "before-command", call(command))["hookSpecificOutput"]
            ["updatedInput"]["command"]
            .as_str()
            .map(str::to_owned)
    };
    for (command, expected) in [
        (
            "git status; git diff",
            "justless run --hook -- git status; justless run --hook -- git diff",
        ),
        (
            "cd web && npm test",
            "cd web && justless run --hook -- npm test",
        ),
        (
            "git log --oneline -5  &&  git diff --stat",
            "justless run --hook -- git log --oneline -5  &&  justless run --hook -- git diff --stat",
        ),
    ] {
        for agent in ["hook-claude", "hook-codex"] {
            assert_eq!(
                rewrite(agent, command).as_deref(),
                Some(expected),
                "{agent} {command}"
            );
        }
    }
    // Claude Code checks permissions for each rewritten segment. Codex auto-approves
    // a rewrite, so it only rewrites chains made entirely of supported commands.
    for (command, expected) in [
        (
            "cargo test && git push",
            "justless run --hook -- cargo test && git push",
        ),
        (
            "cargo test || true",
            "justless run --hook -- cargo test || true",
        ),
        (
            "echo \"a && b\" && git status",
            "echo \"a && b\" && justless run --hook -- git status",
        ),
    ] {
        assert_eq!(rewrite("hook-claude", command).as_deref(), Some(expected));
        assert_eq!(rewrite("hook-codex", command), None, "{command}");
    }
    for command in [
        "cd ../elsewhere && cargo test",
        "cd /tmp && cargo test",
        "cargo test 2>&1 | tail -20",
        "cargo test &",
        "cargo test && $(touch x)",
        "cargo test &&",
        "; cargo test",
        "(cd web && npm test)",
        "cargo test # && rm",
        "git status\ngit diff",
        "git push && echo done",
    ] {
        assert_eq!(rewrite("hook-claude", command), None, "{command:?}");
    }
}

#[cfg(unix)]
fn lint_fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/lint")
        .join(name);
    fs::read_to_string(path).unwrap()
}

#[cfg(unix)]
#[test]
fn linter_findings_group_by_rule_with_locations() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    // clippy.txt is real `cargo clippy` output. The other fixtures follow each
    // tool's documented default format.
    for (program, args, fixture, stream, expected) in [
        (
            "cargo",
            &["clippy"][..],
            "clippy.txt",
            "2",
            &[
                "cargo clippy: 11 finding(s), 0 error(s), 11 warning(s) · 4 rule(s) · 1 file(s)",
                "warning clippy::needless_return ×5  unneeded `return` statement",
                "  src/lib.rs:1:49, 2:50, 3:50, 3:72 +1 more [stderr:15-27]",
                "warning unused_variables ×2  unused variable: `unused`",
            ][..],
        ),
        (
            "mypy",
            &["app.py"],
            "mypy.txt",
            "1",
            &[
                "error return-value ×2  Incompatible return value type (got \"int\", expected \"str\")",
                "  app.py:7, 14 [stdout:1-1]",
                "error attr-defined ×1",
            ],
        ),
        (
            "ruff",
            &["check", "."],
            "ruff.txt",
            "1",
            &[
                "warning F401 ×3",
                "  app.py:1:8, 2:8, 3:8",
                "warning F841 ×2",
            ],
        ),
        (
            "ruff",
            &["check", "."],
            "ruff_full.txt",
            "1",
            &[
                "warning F401 ×2  `os` imported but unused",
                "  app.py:1:8, 2:8 [stdout:1-7]",
            ],
        ),
        (
            "tsc",
            &["--noEmit"],
            "tsc.txt",
            "1",
            &[
                "error TS2322 ×3",
                "  src/a.ts:1:7, 2:7, src/b.ts:3:33 [stdout:1-1]",
            ],
        ),
        (
            "shellcheck",
            &["s.sh"],
            "shellcheck.txt",
            "1",
            &["warning SC2086 ×2", "  s.sh:2, 3", "error SC2045 ×1"],
        ),
        (
            "rubocop",
            &[],
            "rubocop.txt",
            "1",
            &[
                "warning Style/FrozenStringLiteralComment ×2",
                "  app.rb:1:1, lib.rb:1:1 [stdout:6-6]",
            ],
        ),
    ] {
        fs::write(root.join("output.txt"), lint_fixture(fixture)).unwrap();
        let redirect = if stream == "2" { " >&2" } else { "" };
        let exe = fake_program(
            root,
            program,
            &format!("cat output.txt{redirect}\nexit 1\n"),
        );
        let mut command = vec!["run", "--fresh", "--", exe.to_str().unwrap()];
        command.extend(args);
        let output = cli(root, &command);
        assert_eq!(output.status.code(), Some(1), "{fixture}");
        let preview = stdout(&output);
        for line in expected {
            assert!(
                preview.contains(line),
                "{fixture}: missing {line:?}\n{preview}"
            );
        }
        assert!(
            stderr(&output).trim_start().starts_with("[justless run-"),
            "{fixture}"
        );
    }

    // A repeated ESLint report collapses to one line per rule and one outcome line.
    fs::write(
        root.join("output.txt"),
        lint_fixture("eslint.txt").repeat(4),
    )
    .unwrap();
    let eslint = fake_program(root, "eslint", "cat output.txt\nexit 1\n");
    let report = stdout(&cli(root, &["run", "--", eslint.to_str().unwrap(), "."]));
    assert!(
        report.starts_with("eslint: 20 finding(s), 16 error(s), 4 warning(s) · 3 rule(s) · 2 file(s)\n")
            && report.contains("error no-undef ×12  'foo' is not defined\n  /work/web/b.js:3:15, 4:25, /work/web/c.js:1:1 [stdout:4-4]")
            && report.matches("✖ 5 problems").count() == 1,
        "{report}"
    );

    // A rerun reports which rules appeared, changed or are no longer reported.
    fs::write(
        root.join("output.txt"),
        "/work/web/b.js\n  3:15  error  'foo' is not defined  no-undef\n  9:1  error  'x' is assigned a value but never used  no-unused-vars\n".repeat(3),
    )
    .unwrap();
    let rerun = stdout(&cli(root, &["run", "--", eslint.to_str().unwrap(), "."]));
    assert!(
        rerun.contains("Compared with run-")
            && rerun.contains("~ error no-undef ×12 → ×3")
            && rerun.contains("~ error no-unused-vars ×4 → ×3")
            && rerun.contains("- warning eqeqeq ×4 (no longer reported)"),
        "{rerun}"
    );

    // Output with no recognized finding keeps the ordinary preview.
    fs::write(root.join("output.txt"), "All checks passed!\n").unwrap();
    let ruff = fake_program(root, "ruff", "cat output.txt\n");
    assert_eq!(
        success(root, &["run", "--", ruff.to_str().unwrap(), "check"]),
        "All checks passed!\n"
    );
}

#[test]
fn hooks_route_linters_build_tools_and_package_runners() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let call = |command: &str| serde_json::json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":root,"tool_input":{"command":command}});
    for command in [
        "cargo clippy --all-targets -- -D warnings",
        "npx eslint src",
        "npx -y tsc --noEmit",
        "pnpm exec vue-tsc --noEmit",
        "uv run ruff check .",
        "poetry run mypy app",
        "python -m mypy app",
        "bundle exec rubocop",
        "golangci-lint run ./...",
        "shellcheck -f gcc install.sh",
        "go vet ./...",
        "uv run pytest -q",
        "make",
        "./gradlew build",
        "mvn -q test",
        "dotnet build",
        "swift test",
        "cmake --build build",
    ] {
        let output = agent_hook(root, "hook-claude", "before-command", call(command));
        assert_eq!(
            output["hookSpecificOutput"]["updatedInput"]["command"],
            format!("justless run --hook -- {command}"),
            "{command}"
        );
    }
    for command in [
        "eslint --format json src",
        "ruff check --output-format=json .",
        "npx tsc --watch",
        "mypy --pretty app",
        "shellcheck -f json install.sh",
        "npx",
        "uv run",
        "cmake -S . -B build",
    ] {
        assert_eq!(
            agent_hook(root, "hook-claude", "before-command", call(command)),
            serde_json::json!({}),
            "{command}"
        );
    }
}

#[cfg(unix)]
#[test]
fn compaction_hooks_restore_recent_runs_and_guide_the_summary() {
    use serde_json::{Value, json};
    let repo = TempDir::new().unwrap();
    let root = repo.path().canonicalize().unwrap();
    let raw_hook = |agent: &str, mode: &str, input: Value| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
            .args([agent, mode])
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        stdout(&output)
    };
    let compact = json!({"hook_event_name":"SessionStart","source":"compact","cwd":root});
    let pre = json!({"hook_event_name":"PreCompact","trigger":"auto","cwd":root});

    // With no recorded activity both hooks stay silent.
    success(
        &root,
        &[
            "init", "--hooks", "--agent", "claude", "--agent", "codex", "--agent", "agy",
        ],
    );
    assert_eq!(
        raw_hook("hook-claude", "after-compact", compact.clone()).trim(),
        "{}"
    );
    assert_eq!(raw_hook("hook-claude", "before-compact", pre.clone()), "");

    let claude: Value =
        serde_json::from_str(&fs::read_to_string(root.join(".claude/settings.json")).unwrap())
            .unwrap();
    let after = &claude["hooks"]["SessionStart"][1];
    assert_eq!(after["matcher"], "^compact$");
    assert_eq!(
        after["hooks"][0]["command"],
        "justless hook-claude after-compact"
    );
    assert!(after["hooks"][0].get("async").is_none());
    assert_eq!(
        claude["hooks"]["PreCompact"][0]["hooks"][0]["command"],
        "justless hook-claude before-compact"
    );
    let codex: Value =
        serde_json::from_str(&fs::read_to_string(root.join(".codex/hooks.json")).unwrap()).unwrap();
    assert_eq!(codex["hooks"]["SessionStart"][1]["matcher"], "^compact$");
    assert!(codex["hooks"].get("PreCompact").is_none());
    assert!(
        !fs::read_to_string(root.join(".agents/hooks.json"))
            .unwrap()
            .contains("compact")
    );

    fs::write(root.join("output.txt"), lint_fixture("mypy.txt")).unwrap();
    let mypy = fake_program(&root, "mypy", "cat output.txt\nexit 1\n");
    let run = cli(&root, &["run", "--", mypy.to_str().unwrap(), "app.py"]);
    let id = run_id(&run);

    let restored: Value =
        serde_json::from_str(&raw_hook("hook-claude", "after-compact", compact.clone())).unwrap();
    let context = restored["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert_eq!(
        restored["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );
    assert!(
        context.contains(&format!(
            "{id} · mypy · exit 1 · 3 finding(s) in 2 rule(s), first return-value ×2"
        )) && context.contains("justless log RUN_ID")
            && context.len() < 2400,
        "{context}"
    );
    let codex_output: Value =
        serde_json::from_str(&raw_hook("hook-codex", "after-compact", compact)).unwrap();
    assert_eq!(codex_output, restored);
    let startup = json!({"hook_event_name":"SessionStart","source":"startup","cwd":root});
    assert_eq!(
        raw_hook("hook-codex", "after-compact", startup).trim(),
        "{}"
    );

    let instructions = raw_hook("hook-claude", "before-compact", pre);
    assert!(
        instructions.starts_with("Keep these Justless references verbatim in the summary.")
            && instructions.contains(&id),
        "{instructions}"
    );
}

#[cfg(unix)]
#[test]
fn stats_report_savings_by_command_with_token_estimates() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    assert!(success(root, &["stats"]).starts_with("No captured runs yet."));
    fs::write(root.join("output.txt"), lint_fixture("mypy.txt").repeat(20)).unwrap();
    let mypy = fake_program(root, "mypy", "cat output.txt\nexit 1\n");
    cli(root, &["run", "--", mypy.to_str().unwrap(), "app.py"]);
    cli(root, &["run", "--", "sh", "-c", "echo small"]);
    let report: serde_json::Value =
        serde_json::from_str(&success(root, &["stats", "--json"])).unwrap();
    let raw = report["raw_bytes"].as_u64().unwrap();
    let preview = report["preview_bytes"].as_u64().unwrap();
    let saved = report["saved_bytes"].as_u64().unwrap();
    assert_eq!(report["runs"], 2);
    assert_eq!(saved, raw - preview);
    assert_eq!(report["estimated_tokens_saved"], saved.div_ceil(4));
    assert_eq!(report["token_estimate"], "UTF-8 bytes / 4");
    assert_eq!(report["commands"][0]["family"], "mypy");
    assert_eq!(report["days"].as_array().unwrap().len(), 1);
    let text = success(root, &["stats"]);
    assert!(
        text.contains("tokens saved (estimated as bytes ÷ 4)")
            && text.contains("By command, most saved first\n  mypy"),
        "{text}"
    );
}

#[test]
fn discover_classifies_claude_code_shell_commands() {
    let repo = TempDir::new().unwrap();
    let root = repo.path().join("work");
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let config = repo.path().join("claude");
    let slug: String = root
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let transcripts = config.join("projects").join(slug);
    fs::create_dir_all(&transcripts).unwrap();
    let mut lines = Vec::new();
    for (id, command, output, cwd) in [
        ("t1", "cargo test", "x".repeat(4000), root.clone()),
        (
            "t2",
            "cd web && npm test",
            "ok\n[justless run-abc] exit 0".into(),
            root.join("web"),
        ),
        ("t3", "ls", "y".repeat(100), root.clone()),
        (
            "t4",
            "cargo test",
            "z".repeat(900),
            PathBuf::from("/elsewhere"),
        ),
    ] {
        lines.push(serde_json::json!({"type":"assistant","cwd":cwd,"timestamp":"2999-01-01T00:00:00Z",
            "message":{"content":[{"type":"tool_use","id":id,"name":"Bash","input":{"command":command}}]}}));
        lines.push(
            serde_json::json!({"type":"user","cwd":cwd,"timestamp":"2999-01-01T00:00:01Z",
            "message":{"content":[{"type":"tool_result","tool_use_id":id,"content":output}]}}),
        );
    }
    let body: Vec<_> = lines.iter().map(|l| l.to_string()).collect();
    fs::write(transcripts.join("session.jsonl"), body.join("\n")).unwrap();
    let discover = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_justless"))
            .arg("discover")
            .args(args)
            .current_dir(&root)
            .env("CLAUDE_CONFIG_DIR", &config)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        stdout(&output)
    };
    let report: serde_json::Value = serde_json::from_str(&discover(&["--json"])).unwrap();
    assert_eq!(report["commands"], 3);
    assert_eq!(report["sessions"], 1);
    let groups = report["groups"].as_array().unwrap();
    let route = |name: &str| groups.iter().find(|g| g["route"] == name).unwrap();
    assert_eq!(route("through_justless")["labels"][0]["label"], "npm test");
    assert_eq!(route("supported_not_routed")["output_bytes"], 4000);
    assert_eq!(route("supported_not_routed")["estimated_tokens"], 1000);
    assert_eq!(route("not_supported")["labels"][0]["label"], "ls");
    let text = discover(&[]);
    assert!(
        text.starts_with("Claude Code ran 3 shell command(s) here in the last 30 day(s), across 1 session(s)")
            && text.contains("Supported, not routed: 1 command(s) · 3.9 KB output · ≈1000 tokens\n  cargo test ×1 (3.9 KB)")
            && text.contains("justless init --hooks --agent claude")
            && !text.contains("x".repeat(20).as_str()),
        "{text}"
    );
}

#[test]
fn cursor_gemini_and_copilot_hooks_merge_and_follow_their_contracts() {
    use serde_json::{Value, json};
    let repo = mixed_fixture();
    let root = repo.path().canonicalize().unwrap();
    let cursor = root.join(".cursor/hooks.json");
    let gemini = root.join(".gemini/settings.json");
    let copilot = root.join(".github/hooks/justless.json");
    fs::create_dir(root.join(".cursor")).unwrap();
    fs::create_dir(root.join(".gemini")).unwrap();
    fs::write(&cursor, r#"{"hooks":[]}"#).unwrap();
    fs::write(
        &gemini,
        r#"{"mcpServers":{"other":{"command":"other"}},"hooks":{"BeforeTool":[{"matcher":"write_file","hooks":[{"type":"command","command":"scan"}]}]}}"#,
    )
    .unwrap();
    let setup = [
        "init", "--hooks", "--agent", "copilot", "--agent", "gemini", "--agent", "cursor",
    ];
    // An invalid file stops setup before anything is written.
    assert!(!cli(&root, &setup).status.success());
    assert!(!root.join("GEMINI.md").exists() && !root.join(".github").exists());
    fs::write(
        &cursor,
        r#"{"version":1,"hooks":{"afterFileEdit":[{"command":"fmt"}],"preToolUse":[{"command":"audit","matcher":"Read"}]}}"#,
    )
    .unwrap();
    fs::write(root.join("GEMINI.md"), "# Team rules\n").unwrap();
    success(&root, &setup);
    let configs = || [&cursor, &gemini, &copilot].map(|path| fs::read_to_string(path).unwrap());
    let installed = configs();
    success(&root, &setup);
    assert_eq!(installed, configs());
    assert!(root.join("AGENTS.md").is_file());
    let instructions = fs::read_to_string(root.join("GEMINI.md")).unwrap();
    assert!(instructions.starts_with("# Team rules\n"));
    assert_eq!(instructions.matches("<!-- justless:begin -->").count(), 1);
    let [cursor, gemini, copilot] = installed.map(|s| serde_json::from_str::<Value>(&s).unwrap());

    assert_eq!(cursor["version"], 1);
    assert_eq!(cursor["hooks"]["afterFileEdit"][0]["command"], "fmt");
    assert_eq!(
        cursor["hooks"]["preToolUse"],
        json!([
            {"command":"audit","matcher":"Read"},
            {"command":"justless hook-cursor before-command","matcher":"Shell","timeout":5}
        ])
    );
    assert_eq!(
        cursor["hooks"]["sessionStart"],
        json!([{"command":"justless hook-cursor session-start","timeout":60}])
    );
    assert_eq!(gemini["mcpServers"]["other"]["command"], "other");
    assert!(gemini.get("version").is_none());
    assert_eq!(gemini["hooks"]["BeforeTool"][0]["matcher"], "write_file");
    assert_eq!(
        gemini["hooks"]["BeforeTool"][1],
        json!({"matcher":"^run_shell_command$","hooks":[{"type":"command",
            "name":"justless-before-command","command":"justless hook-gemini before-command",
            "timeout":5000}]})
    );
    assert_eq!(
        gemini["hooks"]["SessionStart"],
        json!([{"hooks":[{"type":"command","name":"justless-session-start",
            "command":"justless hook-gemini session-start","timeout":60000}]}])
    );
    assert_eq!(
        copilot,
        json!({"version":1,"hooks":{
            "sessionStart":[{"type":"command",
                "bash":"justless hook-copilot session-start || true","timeoutSec":60}],
            "preToolUse":[{"type":"command",
                "bash":"justless hook-copilot before-command || true","matcher":"bash",
                "timeoutSec":5}]
        }})
    );

    let run = |args: &[&str], stdin: Option<&str>| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_justless"))
            .args(args)
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        if let Some(text) = stdin {
            input.write_all(text.as_bytes()).unwrap();
        }
        drop(input);
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        stdout(&output)
    };
    let hook = |handler: &str, mode: &str, input: Value| {
        let output = run(&[handler, mode], Some(&input.to_string()));
        serde_json::from_str::<Value>(&output).unwrap()
    };

    // Session context in each client's documented field.
    let context = |output: &Value, pointer: &str| {
        output
            .pointer(pointer)
            .and_then(Value::as_str)
            .is_some_and(|text| text.starts_with("Justless indexed"))
    };
    let start = hook(
        "hook-cursor",
        "session-start",
        json!({"hook_event_name":"sessionStart","session_id":"s","workspace_roots":[root]}),
    );
    assert!(context(&start, "/additional_context"), "{start}");
    assert_eq!(start.as_object().unwrap().len(), 1);
    let start = hook(
        "hook-gemini",
        "session-start",
        json!({"hook_event_name":"SessionStart","source":"startup","cwd":root}),
    );
    assert_eq!(start["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(
        context(&start, "/hookSpecificOutput/additionalContext"),
        "{start}"
    );
    let start = hook(
        "hook-copilot",
        "session-start",
        json!({"sessionId":"s","timestamp":1,"cwd":root,"source":"startup"}),
    );
    assert!(context(&start, "/additionalContext"), "{start}");
    assert!(root.join(".justless/index.json").is_file());
    assert_eq!(
        hook(
            "hook-gemini",
            "session-start",
            json!({"hook_event_name":"BeforeTool"})
        ),
        json!({})
    );

    // Cursor returns `permission: "allow"` with a rewrite, so a list that contains
    // an unsupported command passes through, as for Codex.
    let cursor = |command: &str| {
        hook(
            "hook-cursor",
            "before-command",
            json!({"hook_event_name":"preToolUse","tool_name":"Shell","cwd":root,
                "tool_input":{"command":command,"working_directory":root}}),
        )
    };
    assert_eq!(
        cursor("git diff"),
        json!({"permission":"allow","updated_input":{
            "command":"justless run --hook -- git diff","working_directory":root}})
    );
    assert_eq!(
        cursor("git status && git diff")["updated_input"]["command"],
        "justless run --hook -- git status && justless run --hook -- git diff"
    );
    for command in ["git push", "cargo test && git push", "git diff | cat"] {
        assert_eq!(cursor(command), json!({}), "{command}");
    }
    let mut other = json!({"hook_event_name":"preToolUse","tool_name":"Read",
        "cwd":root,"tool_input":{"command":"git diff"}});
    assert_eq!(
        hook("hook-cursor", "before-command", other.clone()),
        json!({})
    );
    other["tool_name"] = json!("Shell");
    other["conversation_id"] = json!("conversation");
    let rewritten = hook("hook-cursor", "before-command", other);
    let command = rewritten["updated_input"]["command"].as_str().unwrap();
    assert!(
        command.starts_with("justless run --hook --session ") && command.ends_with(" -- git diff"),
        "{command}"
    );

    // Gemini CLI checks its policy against the rewritten command, so a mixed list is
    // wrapped in place, as in Claude Code.
    let gemini = |args: Value| {
        hook(
            "hook-gemini",
            "before-command",
            json!({"hook_event_name":"BeforeTool","tool_name":"run_shell_command",
                "cwd":root,"tool_input":args}),
        )
    };
    assert_eq!(
        gemini(
            json!({"command":"cargo test && git push","description":"test","dir_path":"frontend"})
        ),
        json!({"hookSpecificOutput":{"hookEventName":"BeforeTool","tool_input":{
            "command":"justless run --hook -- cargo test && git push","description":"test",
            "dir_path":"frontend"}}})
    );
    for args in [
        json!({"command":"git push"}),
        json!({"command":"git diff","is_background":true}),
        json!({"command":"git diff","dir_path":"/"}),
    ] {
        assert_eq!(gemini(args.clone()), json!({}), "{args}");
    }

    // Copilot sends camelCase input with `toolArgs` as a JSON string or an object,
    // and gets `modifiedArgs` without a permission decision.
    let copilot = |args: Value| {
        hook(
            "hook-copilot",
            "before-command",
            json!({"sessionId":"","timestamp":1,"cwd":root,"toolName":"bash","toolArgs":args}),
        )
    };
    let expected = json!({"modifiedArgs":{"command":"justless run --hook -- git diff",
        "description":"show changes"}});
    let args = json!({"command":"git diff","description":"show changes"});
    assert_eq!(copilot(json!(args.to_string())), expected);
    assert_eq!(copilot(args), expected);
    for command in ["git push", "cargo test && git push"] {
        assert_eq!(copilot(json!({"command":command})), json!({}), "{command}");
    }

    // Every failure answers with the no-op and a zero exit status.
    for handler in ["hook-cursor", "hook-gemini", "hook-copilot"] {
        for (args, stdin) in [
            (vec![handler, "before-command"], Some("not json")),
            (
                vec!["--root", "/nonexistent/justless", handler, "before-command"],
                Some("{}"),
            ),
            (vec![handler, "unknown-mode"], Some("{}")),
            (vec![handler], None),
        ] {
            assert_eq!(run(&args, stdin), "{}\n", "{args:?}");
        }
    }
}

fn has_line(output: &str, expected: &str) {
    assert!(
        output.lines().any(|line| line == expected),
        "missing {expected:?} in\n{output}"
    );
}

#[test]
fn map_orients_with_located_areas_hubs_links_and_entry_points() {
    let repo = mixed_fixture();
    let root = repo.path();
    let map = success(root, &["map"]);
    assert_eq!(map, success(root, &["map"]), "map must be deterministic");
    for expected in [
        "Map · 6 files · 5 symbols · 4 links (2 possible) · TypeScript 2 · Python 1 · SQL 1",
        "  frontend/ · 2 files · 2 symbols · TypeScript · hub formatOrder frontend/format.ts:1-3",
        "  docs/ · 1 file · 0 symbols · text · docs/operations.md:1",
        "  list_orders [function] backend/orders.py:7-8 · in 1 (1 possible) · out 1 (1 possible)",
        "  frontend/format.ts +1 file · 2 symbols · formatOrder frontend/format.ts:1-3, fetchOrders frontend/orders.ts:3-7",
        "  frontend/ → backend/ · TypeScript → Python · possible HTTP route 1 · fetchOrders frontend/orders.ts:4 → list_orders backend/orders.py:7-8",
        "  backend/ → db/ · Python → SQL · possible SQL relation 1 · list_orders backend/orders.py:8 → purchase_orders db/schema.sql:1-3",
        "  list_orders [function] backend/orders.py:7-8 · HTTP route /api/orders",
        "Tests · none detected",
    ] {
        has_line(&map, expected);
    }
    let small = success(root, &["map", "--budget", "300"]);
    assert!(small.len() <= 300, "{} bytes\n{small}", small.len());
    assert!(small.contains("Not shown: "), "{small}");
    let json: serde_json::Value = serde_json::from_str(&success(root, &["map", "--json"])).unwrap();
    assert_eq!(json["symbols"], 5);
    assert_eq!(json["possible_links"], 2);
    assert_eq!(
        json["cross_area_links"][0]["example"]["at"],
        "backend/orders.py:8"
    );
    assert_eq!(json["cross_area_links"][0]["example"]["possible"], true);
    assert_eq!(json["entry_points"][0]["reason"], "HTTP route /api/orders");
    assert_eq!(json["capped"], false);

    let repo = fixture();
    let path = repo.path();
    fs::create_dir_all(path.join("src/bin")).unwrap();
    fs::write(
        path.join("src/bin/quote.rs"),
        "fn main() {\n    println!(\"{}\", checkout_fixture::checkout_total(1, 2));\n}\n",
    )
    .unwrap();
    let map = success(path, &["map"]);
    for expected in [
        "  pricing::checkout_total [function] src/pricing.rs:1-3 · in 4 · out 1",
        "  src/bin/ → src/pricing.rs · call 1 · main src/bin/quote.rs:2 → pricing::checkout_total src/pricing.rs:1-3",
        "  main [function] src/bin/quote.rs:1-3 · main",
        "  tests/ · 1 file · 1 test · e.g. total_includes_fee [test] tests/checkout.rs:3-12",
    ] {
        has_line(&map, expected);
    }
}

#[test]
fn affected_walks_dependents_by_distance_to_the_tests_that_reach_them() {
    let repo = fixture();
    let path = repo.path();
    let impact = success(path, &["affected", "apply_fee"]);
    assert_eq!(impact, success(path, &["affected", "apply_fee"]));
    for expected in [
        "Affected by pricing::apply_fee [function] src/pricing.rs:5-7 · 4 dependents within 3 hops",
        "Distance 1 · 1",
        "  pricing::checkout_total [function] src/pricing.rs:1-3 · call at src/pricing.rs:2",
        "Distance 2 · 3",
        "  total_includes_fee [test] tests/checkout.rs:3-12 · call at tests/checkout.rs:11 → pricing::checkout_total",
        "  file scope · import at src/lib.rs:4 → pricing::checkout_total",
        "  file scope · import at tests/checkout.rs:1 → pricing::checkout_total",
        "  total_includes_fee [test] tests/checkout.rs:3-12 · 2 hops",
        "  cargo test --test checkout total_includes_fee",
    ] {
        has_line(&impact, expected);
    }
    // Tests are found past the walk's depth, and the report says it stopped early.
    let shallow = success(path, &["affected", "apply_fee", "--depth", "1"]);
    assert!(!shallow.contains("Distance 2"), "{shallow}");
    has_line(&shallow, "  cargo test --test checkout total_includes_fee");
    has_line(
        &shallow,
        "Depth limit: 3 more dependents past 1 hop. Raise --depth to follow them.",
    );
    let file = success(path, &["affected", "src/pricing.rs"]);
    has_line(
        &file,
        "Affected by src/pricing.rs (2 symbols) · 3 dependents within 3 hops",
    );
    let small = success(path, &["affected", "apply_fee", "--budget", "300"]);
    assert!(small.len() <= 300, "{} bytes\n{small}", small.len());
    has_line(&small, "  cargo test --test checkout total_includes_fee");
    let json: serde_json::Value =
        serde_json::from_str(&success(path, &["affected", "apply_fee", "--json"])).unwrap();
    assert_eq!(json["reached"], 4);
    assert_eq!(json["dependents"].as_array().unwrap().len(), 4);
    assert_eq!(
        json["dependents"][0]["dependent"]["name"],
        "pricing::checkout_total"
    );
    assert_eq!(json["tests"][0]["hops"], 2);
    assert_eq!(
        json["commands"],
        serde_json::json!(["cargo test --test checkout total_includes_fee"])
    );
    assert!(
        stderr(&cli(path, &["affected", "src/missing.rs"]))
            .contains("src/missing.rs is not an indexed file or symbol")
    );
    // Without a repository there are no changes to start from.
    assert!(stderr(&cli(path, &["affected", "--changes"])).contains("git diff failed"));

    // A cycle ends once every symbol on it has been reached.
    fs::write(
        path.join("src/cycle.rs"),
        "pub fn ping(n: u32) -> u32 {\n    if n == 0 { 0 } else { pong(n - 1) }\n}\n\npub fn pong(n: u32) -> u32 {\n    ping(n)\n}\n",
    )
    .unwrap();
    let mut lib = fs::OpenOptions::new()
        .append(true)
        .open(path.join("src/lib.rs"))
        .unwrap();
    writeln!(lib, "pub mod cycle;").unwrap();
    let cycle = success(path, &["affected", "ping", "--depth", "6"]);
    has_line(
        &cycle,
        "Affected by cycle::ping [function] src/cycle.rs:1-3 · 1 dependent within 6 hops",
    );
    has_line(
        &cycle,
        "  cycle::pong [function] src/cycle.rs:5-7 · call at src/cycle.rs:6",
    );

    // TypeScript → Python → SQL, with pytest node IDs for the tests that reach it.
    let repo = mixed_fixture();
    let root = repo.path();
    fs::create_dir(root.join("tests")).unwrap();
    fs::write(
        root.join("tests/test_orders.py"),
        "from backend.orders import list_orders\n\n\ndef test_list_orders():\n    assert list_orders() is not None\n\n\nclass TestOrders:\n    def test_again(self):\n        list_orders()\n",
    )
    .unwrap();
    let impact = success(root, &["affected", "purchase_orders"]);
    for expected in [
        "  list_orders [function] backend/orders.py:7-8 · possible SQL relation at backend/orders.py:8",
        "  fetchOrders [function] frontend/orders.ts:3-7 · possible HTTP route at frontend/orders.ts:4 → list_orders",
        "  test_list_orders [test] tests/test_orders.py:4-5 · possible · 2 hops",
        "  pytest tests/test_orders.py::test_list_orders tests/test_orders.py::TestOrders::test_again",
    ] {
        has_line(&impact, expected);
    }
}

#[test]
fn affected_caps_fan_out_deterministically() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let mut source = String::from("def hot():\n    return 1\n");
    for number in 0..300 {
        source.push_str(&format!(
            "\n\ndef caller_{number:03}():\n    return hot()\n"
        ));
    }
    fs::write(root.join("hot.py"), source).unwrap();
    let args = ["affected", "hot", "--budget", "100000"];
    let impact = success(root, &args);
    assert_eq!(impact, success(root, &args));
    has_line(
        &impact,
        "Fan-out cap: followed 256 of 300 dependents of hot [function] hot.py:1-2.",
    );
    has_line(&impact, "Distance 1 · 256");
    assert!(impact.contains("  caller_255 [function]"), "{impact}");
    assert!(!impact.contains("caller_256"), "{impact}");
    let json: serde_json::Value = serde_json::from_str(&success(
        root,
        &["affected", "hot", "--budget", "100000", "--json"],
    ))
    .unwrap();
    assert_eq!(json["capped"], true);
    assert_eq!(json["dependents"].as_array().unwrap().len(), 256);
    assert_eq!(json["commands"], serde_json::json!([]));
}

#[test]
fn affected_changes_start_from_the_changed_symbols() {
    let repo = fixture();
    let path = repo.path();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?} {}", stderr(&output));
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Justless Test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "-qm",
        "fixture",
    ]);
    assert_eq!(
        success(path, &["affected", "--changes"]),
        "No changed files.\n"
    );
    let clean: serde_json::Value =
        serde_json::from_str(&success(path, &["affected", "--changes", "--json"])).unwrap();
    assert_eq!(clean["changed"], serde_json::json!([]));
    assert_eq!(clean["reached"], 0);
    let pricing = path.join("src/pricing.rs");
    let source = fs::read_to_string(&pricing).unwrap();
    fs::write(&pricing, source.replace("fee + 1", "fee + 2")).unwrap();
    let impact = success(path, &["affected", "--changes"]);
    for expected in [
        "Affected by changes (1 changed symbol) · 4 dependents within 3 hops",
        "  pricing::apply_fee [function] src/pricing.rs:5-7 · changed src/pricing.rs:6-6",
        "  pricing::checkout_total [function] src/pricing.rs:1-3 · call at src/pricing.rs:2",
        "  cargo test --test checkout total_includes_fee",
    ] {
        has_line(&impact, expected);
    }
}

/// A repository with one file full of matches, many lightly matching files and the
/// dependency and build trees agents usually do not want listed.
fn search_repo() -> TempDir {
    let repo = TempDir::new().unwrap();
    let write = |path: &str, text: &str| {
        let path = repo.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    let big: String = (0..150)
        .map(|i| format!("let needle_{i} = load(needle, {i});\n"))
        .collect();
    write("src/big.rs", &big);
    for dir in ["api", "core", "db"] {
        for f in 0..10 {
            let text: String = (0..40)
                .map(|i| {
                    if (i + f) % 9 == 0 {
                        format!("    call(needle, {i}); // {dir} {f}\n")
                    } else {
                        format!("    other({i});\n")
                    }
                })
                .collect();
            write(&format!("src/{dir}/f{f}.rs"), &text);
        }
    }
    for p in 0..60 {
        write(
            &format!("node_modules/pkg{p}/index.js"),
            "module.exports = needle;\n",
        );
    }
    for t in 0..80 {
        write(&format!("target/debug/deps/lib{t}.rlib"), "bin");
    }
    write("docs/rare.md", "rare_token appears once\n");
    repo
}

fn log_range(line: &str) -> (String, String) {
    let range = line
        .rsplit_once("[stdout:")
        .unwrap()
        .1
        .trim_end_matches(']');
    let (from, to) = range.split_once('-').unwrap_or((range, range));
    (from.to_owned(), to.to_owned())
}

#[cfg(unix)]
#[test]
fn search_output_groups_matches_by_file_with_exact_lines() {
    let repo = search_repo();
    let root = repo.path();
    let direct = |args: &[&str]| {
        let output = Command::new("grep")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        stdout(&output)
    };
    let raw = direct(&["-rn", "needle", "."]);
    let total = raw.lines().count();
    let files = raw
        .lines()
        .map(|l| l.split(':').next().unwrap())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    assert!(raw.len() > 4096);

    let output = cli(root, &["run", "--", "grep", "-rn", "needle", "."]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = stdout(&output);
    assert!(
        report.len() <= 4096 && report.len() * 3 < raw.len(),
        "{report}"
    );
    assert!(
        report.starts_with(&format!("grep: {total} matches in {files} files\n")),
        "{report}"
    );
    // The busiest file comes first and links to exactly its original lines.
    let group = report.lines().nth(1).unwrap();
    assert!(group.starts_with("./src/big.rs ×150 [stdout:"), "{report}");
    let (from, to) = log_range(group);
    let id = run_id(&output);
    let slice = success(
        root,
        &[
            "log", &id, "--stream", "stdout", "--from", &from, "--to", &to,
        ],
    );
    assert_eq!(slice.lines().count(), 150);
    assert!(slice.lines().all(|l| l.starts_with("./src/big.rs:")));
    let first = raw
        .lines()
        .find(|l| l.starts_with("./src/big.rs:"))
        .unwrap();
    assert_eq!(
        report.lines().nth(2).unwrap(),
        format!("  {}", &first["./src/big.rs:".len()..])
    );
    assert!(report.contains(" more in file\n"), "{report}");
    // Dependency trees collapse into one line that still links to their matches.
    assert!(
        report.contains("\n./node_modules/ 60 matches in 60 files collapsed [stdout:"),
        "{report}"
    );

    // Archived logs, including the stdout of the run itself, are never searched,
    // even when the pattern names the state directory.
    fs::write(root.join("docs/state.md"), "keep .justless/ out of Git\n").unwrap();
    let state = cli(root, &["run", "--", "grep", "-rn", ".justless", "."]);
    assert_eq!(
        stdout(&state),
        "./docs/state.md:1:keep .justless/ out of Git\n"
    );
    let again = cli(
        root,
        &["run", "--fresh", "--", "grep", "-rn", "needle", "."],
    );
    assert!(stdout(&again).starts_with(&format!("grep: {total} matches")));
    let source = fs::read_to_string(root.join("src/big.rs")).unwrap();
    fs::write(root.join("src/big.rs"), format!("{source}needle again\n")).unwrap();
    let rerun = stdout(&cli(root, &["run", "--", "grep", "-rn", "needle", "."]));
    assert!(
        rerun.contains(&format!("Compared with {}:\n", run_id(&again)))
            && rerun.contains("~ ./src/big.rs ×150 → ×151"),
        "{rerun}"
    );

    // No match keeps grep's exit status 1 and prints nothing.
    let missing = cli(root, &["run", "--", "grep", "-rn", "absent_token", "."]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(stdout(&missing).is_empty());
    // Small output stays verbatim.
    let small = cli(root, &["run", "--", "grep", "-rn", "rare_token", "."]);
    let expected = direct(&["-rn", "--exclude-dir=.justless", "rare_token", "."]);
    assert_eq!(stdout(&small), expected);
    // Context lines extend each file's range but are not counted as matches.
    let context = stdout(&cli(
        root,
        &["run", "--", "grep", "-rn", "-A", "1", "needle", "src"],
    ));
    let matches = direct(&["-rn", "needle", "src"]).lines().count();
    assert!(
        context.starts_with(&format!("grep: {matches} matches in "))
            && context.lines().next().unwrap().ends_with(" context lines"),
        "{context}"
    );
}

#[cfg(unix)]
#[test]
fn listings_group_by_directory_and_collapse_dependency_trees() {
    let repo = search_repo();
    let root = repo.path();
    let output = cli(root, &["run", "--", "find", "."]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = stdout(&output);
    let raw = success(root, &["log", &run_id(&output), "--stream", "stdout"]);
    assert!(raw.len() > 4096 && report.len() * 3 < raw.len(), "{report}");
    let total = raw.lines().count();
    assert!(
        report.starts_with(&format!("find: {total} entries in ")),
        "{report}"
    );
    let modules = raw
        .lines()
        .filter(|l| l.starts_with("./node_modules/"))
        .count();
    let line = report
        .lines()
        .find(|l| l.starts_with("node_modules/ "))
        .unwrap();
    assert!(
        line.starts_with(&format!(
            "node_modules/ {modules} entries collapsed [stdout:"
        )),
        "{report}"
    );
    assert!(report.contains("\nsrc/api/ 10: f"), "{report}");
    assert!(
        report.contains("\nsrc/ 4: ") && !report.contains("pkg1"),
        "{report}"
    );

    let report = stdout(&cli(root, &["run", "--", "ls", "-laR"]));
    assert!(
        report.starts_with("ls: ") && report.contains("\ntarget/ ") && report.len() < 4096,
        "{report}"
    );
    assert!(report.contains("\nsrc/db/ 10: f0.rs, f1.rs"), "{report}");

    // tree output, with its non-breaking-space indentation and count line.
    let mut tree = String::from(".\n");
    for p in 0..60 {
        tree.push_str(&format!(
            "├── pkg{p}\n│\u{a0}\u{a0} ├── index.js\n│\u{a0}\u{a0} └── package.json\n"
        ));
    }
    tree.push_str("└── src\n    └── main.rs\n\n61 directories, 121 files\n");
    fs::write(root.join("tree.txt"), &tree).unwrap();
    let script = format!("cat '{}'\n", root.join("tree.txt").display());
    let program = fake_program(root, "tree", &script);
    let report = stdout(&cli(
        root,
        &["run", "--", program.to_str().unwrap(), "-L", "2"],
    ));
    assert!(
        report.starts_with("tree: 182 entries in 62 directories · 61 directories, 121 files\n")
            && report.contains("\nsrc/ 1: main.rs [stdout:"),
        "{report}"
    );
}

#[test]
fn hooks_route_searches_and_listings_but_not_machine_output() {
    let repo = mixed_fixture();
    let root = repo.path();
    let call = |command: &str| serde_json::json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":root,"tool_input":{"command":command}});
    for command in [
        "grep -rn fetch .",
        "grep -n fetch frontend/orders.ts frontend/format.ts",
        "grep -rl fetch frontend",
        "git grep -n fetch",
        "rg -l fetch",
        "rg --files",
        "rg -n -C 2 fetch frontend",
        "rg -c fetch",
        "ag fetch",
        "find . -name '*.ts' -type f",
        "find frontend -maxdepth 2",
        "ls -la",
        "ls -R frontend",
        "tree -L 2",
        "fd orders",
        "git ls-files",
    ] {
        // Claude Code keeps its own grep (see claude_code_keeps_its_own_grep_...).
        let agents: &[&str] = if command.starts_with("grep ") {
            &["hook-codex"]
        } else {
            &["hook-claude", "hook-codex"]
        };
        for agent in agents {
            let output = agent_hook(root, agent, "before-command", call(command));
            assert_eq!(
                output["hookSpecificOutput"]["updatedInput"]["command"],
                format!("justless run --hook -- {command}"),
                "{agent} {command}"
            );
        }
    }
    for command in [
        "grep fetch frontend/orders.ts",
        "grep -rn fetch . | head",
        "grep -rc fetch .",
        "grep -rh fetch .",
        "grep -rZ fetch .",
        "git grep --no-index fetch",
        "rg --json fetch",
        "find . -print0",
        "find . -name '*.tmp' -delete",
        "find . -ls",
        "ls",
        "ls -la > listing.txt",
        "tree -J",
        "fd -0 orders",
        "git ls-files -z",
    ] {
        assert_eq!(
            agent_hook(root, "hook-claude", "before-command", call(command)),
            serde_json::json!({}),
            "{command}"
        );
    }
}

#[test]
fn claude_grep_and_glob_results_are_grouped_and_recoverable() {
    use serde_json::{Value, json};
    let repo = search_repo();
    let root = repo.path().canonicalize().unwrap();
    let raw = Command::new("grep")
        .args(["-rn", "needle", "src"])
        .current_dir(&root)
        .output()
        .unwrap();
    let content = stdout(&raw).trim_end().to_owned();
    let lines = content.lines().count();
    let grep = |content: &str, mode: Value| {
        json!({"hook_event_name":"PostToolUse","tool_name":"Grep","cwd":root,
            "tool_input":{"pattern":"needle","output_mode":"content","-n":true},
            "tool_response":{"mode":mode,"numFiles":0,"filenames":[],"content":content,
                "numLines":content.lines().count(),"appliedLimit":250}})
    };
    let event = grep(&content, json!("content"));
    let output = agent_hook(&root, "hook-claude", "after-search", event);
    let hook = &output["hookSpecificOutput"];
    let updated = &hook["updatedToolOutput"];
    let text = updated["content"].as_str().unwrap();
    assert!(text.len() * 3 < content.len(), "{text}");
    assert!(
        text.contains(&format!(
            "\nGrep: {lines} matches in 31 files\nsrc/big.rs ×150\n  1:let needle_0"
        )),
        "{text}"
    );
    assert_eq!(updated["numLines"], text.lines().count());
    assert_eq!(updated["appliedLimit"], 250);
    assert_eq!(updated["mode"], "content");
    let id = text
        .strip_prefix("[justless recall ")
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    assert!(hook["additionalContext"].as_str().unwrap().contains(id));
    assert_eq!(success(&root, &["recall", id]), content);

    // Results without paths, counts and small results reach Claude unchanged.
    let single: String = (1..300)
        .map(|i| format!("{i}:let needle_{i} = load(needle);\n"))
        .collect();
    for event in [
        grep(&single, json!("content")),
        grep(&content, json!("count")),
        grep("src/big.rs:1:needle", json!("content")),
    ] {
        let output = agent_hook(&root, "hook-claude", "after-search", event);
        assert_eq!(output, json!({}));
    }

    let mut files: Vec<String> = Vec::new();
    for dir in ["api", "core", "db"] {
        for f in 0..10 {
            let path = root.join(format!("src/{dir}/f{f}.rs"));
            files.push(path.display().to_string());
        }
    }
    for p in 0..60 {
        let path = root.join(format!("node_modules/pkg{p}/index.js"));
        files.push(path.display().to_string());
    }
    let glob = json!({"hook_event_name":"PostToolUse","tool_name":"Glob","cwd":root,
        "tool_input":{"pattern":"**/*.*"},
        "tool_response":{"durationMs":4,"numFiles":files.len(),"filenames":files,"truncated":true,
            "totalMatches":250,"countIsComplete":true}});
    let output = agent_hook(&root, "hook-claude", "after-search", glob);
    let updated = &output["hookSpecificOutput"]["updatedToolOutput"];
    let names: Vec<_> = updated["filenames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap())
        .collect();
    assert_eq!(updated["numFiles"], 90);
    assert_eq!(updated["truncated"], false);
    assert!(
        names[1].starts_with(&format!("Glob in {}: 90 entries", root.display()))
            && names.contains(&"node_modules/ 60 entries collapsed")
            && names.contains(&"src/api/ 10: f0.rs, f1.rs, f2.rs, f3.rs, f4.rs, f5.rs +4 more")
            && names
                .last()
                .unwrap()
                .starts_with("(Showing 90 of 250 matching files; 160 more"),
        "{names:?}"
    );
    let id = names[0]
        .strip_prefix("[justless recall ")
        .unwrap()
        .split(']')
        .next()
        .unwrap();
    assert_eq!(success(&root, &["recall", id]), files.join("\n"));
}

#[test]
fn claude_code_keeps_its_own_grep_and_destructive_finds_pass_through() {
    let repo = TempDir::new().unwrap();
    let root = repo.path().canonicalize().unwrap();
    let call = |command: &str| serde_json::json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":root,"tool_input":{"command":command}});
    let rewrite = |agent: &str, command: &str| {
        agent_hook(&root, agent, "before-command", call(command))["hookSpecificOutput"]
            ["updatedInput"]["command"]
            .as_str()
            .map(str::to_owned)
    };
    // Claude Code's shell replaces grep with a bundled ugrep that honors .gitignore,
    // so its hook leaves grep alone. Other clients run the system grep either way.
    for command in ["grep -rn Workspace src", "cd src && grep -rn foo ."] {
        assert_eq!(rewrite("hook-claude", command), None, "{command}");
        assert!(rewrite("hook-codex", command).is_some(), "{command}");
    }
    for command in [
        "find . -name '*.rs'",
        "rg Workspace",
        "git grep -n Workspace",
        "ls -la src",
    ] {
        assert_eq!(
            rewrite("hook-claude", command),
            Some(format!("justless run --hook -- {command}")),
            "{command}"
        );
    }
    for command in [
        "find . -name '*.tmp' -delete",
        "find . -exec rm {} +",
        "find . -name x -execdir cat {} +",
        "find . -ok rm {} +",
    ] {
        for agent in ["hook-claude", "hook-codex"] {
            assert_eq!(rewrite(agent, command), None, "{agent} {command}");
        }
    }
}

#[test]
fn hook_routed_runs_print_a_footer_only_when_it_adds_information() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    fs::write(root.join("notes.txt"), "alpha\nbeta\n").unwrap();
    let hooked = |args: &[&str]| {
        let mut all = vec!["run", "--hook", "--"];
        all.extend(args);
        cli(root, &all)
    };

    // Verbatim success: the agent already has every byte, so nothing is added.
    let ok = hooked(&["sh", "-c", "echo done"]);
    assert_eq!(
        (ok.status.code(), stdout(&ok), stderr(&ok)),
        (Some(0), "done\n".into(), String::new())
    );

    // A search that matches nothing keeps its exit status 1 and prints nothing.
    let miss = hooked(&["grep", "-rn", "zzz-no-match", "."]);
    assert_eq!(
        (miss.status.code(), stdout(&miss), stderr(&miss)),
        (Some(1), String::new(), String::new())
    );

    // A real failure keeps its message and gains one short marker with the exit status.
    let error = hooked(&["grep", "-rn", "zzz", "/nonexistent/justless-path"]);
    assert_eq!(error.status.code(), Some(2));
    let marker = stderr(&error);
    assert!(
        marker.lines().last().unwrap().starts_with("[justless run-")
            && marker.trim_end().ends_with(" · exit 2]"),
        "{marker}"
    );
    assert!(marker.contains("nonexistent"), "{marker}");

    // Clipped output says how much was shown, and the full stream replays exactly.
    let long = hooked(&["sh", "-c", "seq 1 20000"]);
    assert_eq!(long.status.code(), Some(0));
    let marker = stderr(&long);
    assert!(
        marker.contains(" of ") && marker.trim_end().ends_with(" shown]"),
        "{marker}"
    );
    let id = marker
        .trim()
        .trim_start_matches('[')
        .split(' ')
        .nth(1)
        .unwrap();
    let replay = success(root, &["log", id, "--stream", "stdout"]);
    let expected: String = (1..=20000).map(|n| format!("{n}\n")).collect();
    assert_eq!(replay, expected);

    // A repeated run collapses and needs no marker of its own.
    let again = hooked(&["sh", "-c", "seq 1 20000"]);
    assert!(
        stdout(&again).starts_with("Output unchanged since run-"),
        "{}",
        stdout(&again)
    );
    assert_eq!(stderr(&again), "");

    // Output without a final newline still puts the marker on its own line.
    let partial = hooked(&["sh", "-c", "printf partial; exit 4"]);
    assert_eq!(
        (partial.status.code(), stdout(&partial)),
        (Some(4), "partial".into())
    );
    assert!(
        stderr(&partial).starts_with("\n[justless run-"),
        "{:?}",
        stderr(&partial)
    );

    // A command typed by a person keeps the full footer that scripts parse.
    let typed = cli(root, &["run", "--", "sh", "-c", "echo typed"]);
    assert!(stderr(&typed).contains("] exit 0 · 6 of 6 output bytes · justless log run-"));

    // Stats count the footers the agent received and flag runs that grew.
    let report: serde_json::Value =
        serde_json::from_str(&success(root, &["stats", "--json"])).unwrap();
    let grown: u64 = report["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["grown_runs"].as_u64().unwrap())
        .sum();
    assert!(grown >= 2, "{report:#}");
    assert!(success(root, &["stats"]).contains("grew"));
}

#[test]
fn search_groups_files_whose_names_share_a_prefix() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    for name in ["settings", "settings-prod"] {
        fs::write(
            root.join(name),
            "needle in this configuration value\n".repeat(100),
        )
        .unwrap();
    }
    // `settings-prod:1:…` must not be read as a context line of `settings`.
    let output = cli(
        root,
        &[
            "run",
            "--",
            "grep",
            "-n",
            "needle",
            "settings",
            "settings-prod",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let preview = stdout(&output);
    assert!(
        preview.starts_with("grep: 200 matches in 2 files\n") && !preview.contains("context lines"),
        "{preview}"
    );
    // Real context lines (`settings-2-…`) are still recognized as context.
    fs::write(
        root.join("settings"),
        "needle one\nplain line\n".repeat(300),
    )
    .unwrap();
    let context = cli(
        root,
        &[
            "run",
            "--",
            "grep",
            "-n",
            "-A",
            "1",
            "needle",
            "settings",
            "settings-prod",
        ],
    );
    let preview = stdout(&context);
    assert!(
        preview.starts_with("grep: 400 matches in 2 files, 300 context lines"),
        "{preview}"
    );
}

#[test]
fn failed_run_context_keeps_every_diagnostic_at_a_shared_location() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    fs::write(root.join("price.py"), "def price():\n    return 1\n").unwrap();
    // Progress on stdout and the real error on stderr both name price.py:2.
    let output = cli(
        root,
        &[
            "run",
            "--",
            "sh",
            "-c",
            "echo 'price.py:2: progress'; echo 'price.py:2: error: EXPECTED_FAILURE' >&2; exit 1",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let id = run_id(&output);
    let context = success(root, &["context", "--run", &id, "--budget", "12000"]);
    let error = context.find("EXPECTED_FAILURE").expect(&context);
    let progress = context.find("progress").expect(&context);
    // Both survive, and the failure's own line comes first.
    assert!(error < progress, "{context}");
    assert!(
        context.contains("price.py:2 · log stderr:1")
            && context.contains("price.py:2 · log stdout:1"),
        "{context}"
    );
}

#[test]
fn deleting_a_function_points_affected_at_its_callers_not_its_neighbor() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=f@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
    };
    git(&["init", "-q"]);
    fs::write(root.join(".gitignore"), ".justless/\n").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"probe\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    let original = "pub fn kept() -> u8 { 1 }\npub fn removed() -> u8 { 2 }\npub fn consumer() -> u8 { removed() }\n\
#[test]\nfn checks_removed() { assert_eq!(consumer(), 2); }\n#[test]\nfn checks_kept() { assert_eq!(kept(), 1); }\n\
pub fn long() -> u8 {\n    let a = 1;\n    let b = 2;\n    a + b\n}\n";
    let source = root.join("src/lib.rs");
    fs::write(&source, original).unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "fixture"]);

    fs::write(
        &source,
        original.replace("pub fn removed() -> u8 { 2 }\n", ""),
    )
    .unwrap();
    for staged in [false, true] {
        if staged {
            git(&["add", "src/lib.rs"]);
        }
        let mut args = vec!["affected", "--changes"];
        if staged {
            args.push("--staged");
        }
        let impact = success(root, &args);
        assert!(
            impact.contains("removed [function] · deleted from src/lib.rs (committed lines 2-2)")
                && impact.contains(
                    "consumer [function] src/lib.rs:2 · still references deleted removed"
                )
                && impact.contains("cargo test checks_removed")
                && !impact.contains("kept [function]")
                && !impact.contains("checks_kept"),
            "staged={staged}\n{impact}"
        );
    }
    assert!(
        success(root, &["context", "--changes"]).contains(
            "src/lib.rs · deleted after line 1 (committed lines 2-2) · removed [function]"
        )
    );

    // Lines removed inside a surviving function change that function.
    git(&["commit", "-qm", "remove"]);
    let current = fs::read_to_string(&source).unwrap();
    fs::write(&source, current.replace("    let b = 2;\n", "")).unwrap();
    let impact = success(root, &["affected", "--changes"]);
    assert!(
        impact.contains("long [function]") && impact.contains("lines removed after src/lib.rs:"),
        "{impact}"
    );
    assert!(!impact.contains("checks_kept"), "{impact}");
}

#[test]
fn revisions_with_a_tilde_are_routed_but_home_paths_are_not() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let call = |command: &str| serde_json::json!({"hook_event_name":"PreToolUse","tool_name":"Bash","cwd":root,"tool_input":{"command":command}});
    // `~` inside a word is literal to the shell, so these are ordinary Git revisions.
    for command in [
        "git diff HEAD~3",
        "git log main~5..HEAD",
        "git show HEAD~1 --stat",
    ] {
        let output = agent_hook(root, "hook-claude", "before-command", call(command));
        assert_eq!(
            output["hookSpecificOutput"]["updatedInput"]["command"],
            format!("justless run --hook -- {command}"),
            "{command}"
        );
    }
    // At the start of a word, or after `=` or `:`, the shell expands it: pass through.
    for command in [
        "ls ~/x",
        "cat ~/.ssh/config",
        "git diff --output=~/x",
        "git log a:~b",
    ] {
        assert_eq!(
            agent_hook(root, "hook-claude", "before-command", call(command)),
            serde_json::json!({}),
            "{command}"
        );
    }
}

/// Run a fake linter that prints `out` on stdout and `err` on stderr, and return the
/// preview. A `--fresh` first argument goes to Justless rather than the linter.
#[cfg(unix)]
fn lint_preview(root: &Path, program: &str, args: &[&str], out: &str, err: &str) -> String {
    fs::write(root.join("out.txt"), out).unwrap();
    fs::write(root.join("err.txt"), err).unwrap();
    let exe = fake_program(root, program, "cat out.txt\ncat err.txt >&2\nexit 1\n");
    let (fresh, args) = match args.split_first() {
        Some((&"--fresh", rest)) => (true, rest),
        _ => (false, args),
    };
    let mut command = vec!["run"];
    if fresh {
        command.push("--fresh");
    }
    command.extend(["--", exe.to_str().unwrap()]);
    command.extend(args);
    let output = cli(root, &command);
    assert_eq!(output.status.code(), Some(1), "{program}");
    stdout(&output)
}

#[cfg(unix)]
#[test]
fn lint_reports_keep_findings_under_unusual_paths() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let fresh = |program: &str, args: &[&str], out: &str| {
        let mut args = args.to_vec();
        args.insert(0, "--fresh");
        lint_preview(root, program, &args, out, "")
    };

    // ESLint file names may contain spaces, and rows never inherit an earlier file.
    let mut out = String::from("\n/work/web/src/a.js\n");
    for i in 1..=30 {
        out.push_str(&format!("  {i}:5  error  'foo' is not defined  no-undef\n"));
    }
    out.push_str("\n/work/web/src/My Components/Button.jsx\n  10:1   error  'React' is not defined  no-undef\n  12:7   error  'unusedProp' is assigned a value but never used  no-unused-vars\n\n✖ 32 problems (32 errors, 0 warnings)\n");
    let report = fresh("eslint", &["."], &out);
    assert!(
        report.starts_with(
            "eslint: 32 finding(s), 32 error(s), 0 warning(s) · 2 rule(s) · 2 file(s)\n"
        ) && report.contains("  /work/web/src/My Components/Button.jsx:12:7 [stdout:")
            && report
                .contains("/work/web/src/My Components/Button.jsx:10:1, /work/web/src/a.js:1:5"),
        "{report}"
    );
    let out = out.replace("/work/web/src/a.js", "/work/My App/src/a.js");
    let report = fresh("eslint", &["."], &out);
    assert!(
        report.contains("error no-undef ×31") && report.contains("/work/My App/src/a.js:1:5"),
        "{report}"
    );

    // Windows paths keep their drive letter, count as separate files and sort per file.
    let mut out = String::new();
    for (file, line) in [(r"C:\proj\src\b.ts", 3), (r"C:\proj\src\a.ts", 5)] {
        out.push_str(&format!(
            "{file}({line},1): error TS2304: Cannot find name 'x'.\n"
        ));
    }
    for i in 1..=10 {
        out.push_str(&format!(
            "C:\\proj\\src\\c.ts({i},1): error TS2304: Cannot find name 'q'.\n"
        ));
    }
    let report = fresh("tsc", &[], &out);
    assert!(
        report.contains("· 3 file(s)")
            && report.contains(
                r"  C:\proj\src\a.ts:5:1, C:\proj\src\b.ts:3:1, C:\proj\src\c.ts:1:1, 2:1 +8 more"
            ),
        "{report}"
    );
    let mut out = String::from("\nC:\\proj\\src\\b.js\n");
    for i in 1..=10 {
        out.push_str(&format!("  {i}:5  error  'foo' is not defined  no-undef\n"));
    }
    out.push_str("\nC:\\proj\\src\\a.js\n  3:1  error  'bar' is not defined  no-undef\n\nD:\\other\\c.js\n  1:1  error  'baz' is not defined  no-undef\n");
    let report = fresh("eslint", &["."], &out);
    assert!(
        report.contains("· 3 file(s)")
            && report.contains(r"  C:\proj\src\a.js:3:1, C:\proj\src\b.js:1:5, 2:5, 3:5 +8 more"),
        "{report}"
    );

    // TypeScript paths may contain parentheses, and elaboration lines stay with their error.
    let mut out = "app/(auth)/login/page.tsx(12,7): error TS2322: Type 'string' is not assignable to type 'number'.\n\
app/(auth)/login/page.tsx(20,3): error TS2304: Cannot find name 'useSession'.\n\
src/lib/api.ts(4,10): error TS2322: Type '{ id: string; extra: number; }' is not assignable to type 'User'.\n  \
Object literal may only specify known properties, and 'extra' does not exist in type 'User'.\n\
src/lib/api.ts(9,1): error TS2554: Expected 1 arguments, but got 2.\n"
        .to_owned();
    for i in 21..=26 {
        out.push_str(&format!(
            "src/lib/api.ts({i},1): error TS2554: Expected 1 arguments, but got 2.\n"
        ));
    }
    let report = fresh("tsc", &["--noEmit"], &out);
    assert!(
        report
            .starts_with("tsc: 10 finding(s), 10 error(s), 0 warning(s) · 3 rule(s) · 2 file(s)\n")
            && report
                .contains("  app/(auth)/login/page.tsx:12:7, src/lib/api.ts:4:10 [stdout:1-1]")
            && report.contains("  app/(auth)/login/page.tsx:20:3 [stdout:2-2]")
            && report.contains("      Object literal may only specify known properties"),
        "{report}"
    );

    // One-line findings accept spaces and a colon in the path. The tool's own total stays.
    let mut out = String::new();
    for i in 1..=8 {
        out.push_str(&format!(
            "src/app.py:{i}:1: F401 [*] `os{i}` imported but unused\n"
        ));
    }
    out.push_str("src/my mod.py:2:1: F821 Undefined name `x`\nsrc/my mod.py:3:1: F821 Undefined name `y`\nFound 10 errors.\n");
    let report = fresh("ruff", &["check", "."], &out);
    assert!(
        report.starts_with("ruff check: 10 finding(s)")
            && report.contains("warning F821 ×2  Undefined name `x`\n  src/my mod.py:2:1, 3:1")
            && report.contains("Found 10 errors. [stdout:11]"),
        "{report}"
    );
    let mut out = String::new();
    for i in 1..=7 {
        out.push_str(&format!(
            "pkg/a.py:{i}: error: Name \"v{i}\" is not defined  [name-defined]\n"
        ));
    }
    out.push_str("pkg/a:b.py:9: error: Name \"q\" is not defined  [name-defined]\nFound 8 errors in 2 files (checked 2 source files)\n");
    let report = fresh("mypy", &["pkg"], &out);
    assert!(
        report.contains("error name-defined ×8")
            && report.contains("· 2 file(s)")
            && report.contains("Found 8 errors in 2 files"),
        "{report}"
    );
}

#[cfg(unix)]
#[test]
fn lint_reports_name_rustc_lints_and_short_format_errors() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    // Real `cargo check --message-format=short`: the error code is part of the level.
    let report = lint_preview(
        root,
        "cargo",
        &["--fresh", "check"],
        "",
        &lint_fixture("cargo_short.txt"),
    );
    assert!(
        report.starts_with("cargo check: 4 finding(s), 1 error(s), 3 warning(s) · 2 rule(s)")
            && report.contains("error E0308 ×1  mismatched types: expected `u32`, found `&str`\n")
            && report.contains("warning unused_variables ×3  unused variable: `x`\n"),
        "{report}"
    );
    // A parallel build's progress line is not detail of the error before it.
    let interleaved = lint_fixture("cargo_short.txt").replace(
        "found `&str`\n",
        "found `&str`\n    Checking c2 v0.1.0 (/work/c2)\n",
    );
    let report = lint_preview(root, "cargo", &["--fresh", "check"], "", &interleaved);
    assert!(
        report.contains("  src/lib.rs:4:21 [stderr:2-2]\n") && !report.contains("Checking c2"),
        "{report}"
    );
    // rustc names dead_code and unused_imports once, with wording that varies by item.
    let report = lint_preview(
        root,
        "cargo",
        &["--fresh", "check"],
        "",
        &lint_fixture("cargo_dead_code.txt"),
    );
    assert!(
        report.starts_with("cargo check: 8 finding(s), 0 error(s), 8 warning(s) · 2 rule(s)")
            && report.contains("warning dead_code ×6")
            && report.contains("warning unused_imports ×2"),
        "{report}"
    );
    // `-D warnings` and a lint group on an item still name each lint.
    let report = lint_preview(
        root,
        "cargo",
        &["--fresh", "clippy"],
        "",
        &lint_fixture("clippy_deny_warnings.txt"),
    );
    assert!(
        report.contains("error dead_code ×2") && report.contains("error unused_variables ×2"),
        "{report}"
    );
    let report = lint_preview(
        root,
        "cargo",
        &["--fresh", "check"],
        "",
        &lint_fixture("cargo_deny_group.txt"),
    );
    assert!(report.contains("error unused_imports ×2"), "{report}");
    // Real `cargo clippy --message-format=short` names no lint. Common rustc lints are
    // recognized by their wording, a repeated label is dropped, and Clippy's own
    // findings stay grouped by message.
    let report = lint_preview(
        root,
        "cargo",
        &["--fresh", "clippy", "--message-format=short"],
        "",
        &lint_fixture("clippy_short_lints.txt"),
    );
    assert!(
        report.starts_with("cargo clippy: 7 finding(s), 0 error(s), 7 warning(s) · 3 rule(s)")
            && report.contains(
                "warning non_snake_case ×3  function `Foo` should have a snake case name\n"
            )
            && report.contains("warning unreachable_code ×2  unreachable statement\n")
            && report.contains("warning unneeded … statement ×2  unneeded `return` statement\n"),
        "{report}"
    );
}

#[cfg(unix)]
#[test]
fn lint_reports_keep_failures_outside_the_findings_visible() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    // A failed build script has no source location. Its cause stays in the preview.
    let report = lint_preview(
        root,
        "cargo",
        &["--fresh", "check"],
        "",
        &lint_fixture("cargo_build_script.txt"),
    );
    assert!(
        report.contains("· 1 error(s) without a location\n")
            && report.contains("error: failed to run custom build command for `r05 v0.1.0 (/work/r05)` [stderr:11]\n    Caused by:")
            && report.contains("      QA_BUILD_FAILURE: libfoo not found; set FOO_DIR to the install prefix\n"),
        "{report}"
    );

    // A stream with no finding, such as a crash on stderr, is summarized by its range.
    let mut out = String::from("************* Module app\n");
    for i in 1..=8 {
        out.push_str(&format!(
            "app.py:{}:0: C0116: Missing function or method docstring (missing-function-docstring)\n",
            i * 10
        ));
    }
    let err = "Exception on node <Call l.42> in file '/work/app/models.py'\nTraceback (most recent call last):\n  File \"typecheck.py\", line 1394, in visit_call\n    called = safe_infer(node.func)\nRecursionError: maximum recursion depth exceeded while calling a Python object\n";
    let report = lint_preview(root, "pylint", &["--fresh", "app.py"], &out, err);
    assert!(
        report.contains("warning C0116 ×8")
            && report
                .contains("stderr: 5 other line(s) [stderr:1-5]\n  Exception on node <Call l.42>")
            && report.contains("  RecursionError: maximum recursion depth exceeded"),
        "{report}"
    );

    // `could not compile` lines collapse into one that names each package and target,
    // and further outcomes are counted.
    let report = lint_preview(
        root,
        "cargo",
        &["--fresh", "check"],
        "",
        &lint_fixture("cargo_workspace.txt"),
    );
    assert!(
        report.contains(
            "error: could not compile 6 packages: `c1` (lib), `c6` (lib), `c4` (lib), `c5` (lib), `c3` (lib), `c2` (lib) [stderr:11]\n"
        ),
        "{report}"
    );
    // `--all-targets` fails the library and its test build separately.
    let report = lint_preview(
        root,
        "cargo",
        &["--fresh", "clippy", "--all-targets", "--keep-going"],
        "",
        &lint_fixture("cargo_targets.txt"),
    );
    assert!(
        report.contains("error: could not compile `rt` (lib, lib test) [stderr:11]\n"),
        "{report}"
    );
    let report = lint_preview(
        root,
        "cargo",
        &["--fresh", "clippy", "--all-targets", "--keep-going"],
        "",
        &lint_fixture("cargo_workspace_targets.txt"),
    );
    assert!(
        report.contains(
            "error: could not compile 2 packages: `a` (lib test, lib), `b` (lib, lib test) [stderr:11]\n"
        ),
        "{report}"
    );
    // Many targets of one package are counted, and a line without a target, as older
    // Cargo prints, names the package alone.
    let mut err = String::from("error: could not compile `old` due to previous error\n");
    for target in [
        "lib",
        "lib test",
        "test \"a\"",
        "test \"b\"",
        "test \"c\"",
        "bin \"d\"",
    ] {
        err.push_str(&format!(
            "error: could not compile `rt` ({target}) due to 1 previous error\n"
        ));
    }
    let report = lint_preview(
        root,
        "cargo",
        &["--fresh", "check"],
        "",
        &(lint_fixture("cargo_short.txt") + &err),
    );
    assert!(
        report.contains(
            "error: could not compile 3 packages: `r04` (lib), `old`, `rt` (lib, lib test, test \"a\", test \"b\" +2 more) [stderr:7]\n"
        ),
        "{report}"
    );
    let err: String = (1..=6)
        .map(|i| format!("Error: worker {i} failed\n"))
        .collect();
    let report = lint_preview(
        root,
        "eslint",
        &["--fresh", "."],
        &lint_fixture("eslint.txt").repeat(2),
        &err,
    );
    assert!(
        report.contains("✖ 5 problems (4 errors, 1 warning) [stdout:11]\n")
            && report.contains("Error: worker 3 failed [stderr:3]\n")
            && !report.contains("worker 4")
            && report.contains("+3 more outcome line(s) in full log\n"),
        "{report}"
    );
}

#[cfg(unix)]
#[test]
fn lint_reruns_compare_severity_and_findings_within_the_budget() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let cargo = |fixture: &str| lint_preview(root, "cargo", &["check"], "", &lint_fixture(fixture));

    // Errors and warnings of one rule are compared separately.
    cargo("cargo_severity_before.txt");
    let rerun = cargo("cargo_severity_after.txt");
    assert!(
        rerun.contains("Compared with run-")
            && rerun.contains("- error unused_variables ×2 (no longer reported)\n")
            && !rerun.contains("Same findings"),
        "{rerun}"
    );

    // A fixed finding replaced by a new one at the same count is not the same result.
    cargo("cargo_moved_before.txt");
    let rerun = cargo("cargo_moved_after.txt");
    assert!(
        rerun.contains("~ warning unused_variables ×2 with different findings\n"),
        "{rerun}"
    );
    // The same findings on shifted lines say so.
    let moved = lint_fixture("cargo_moved_after.txt")
        .replace("src/lib.rs:2:19", "src/lib.rs:4:19")
        .replace("src/lib.rs:3:23", "src/lib.rs:5:23");
    let rerun = lint_preview(root, "cargo", &["check"], "", &moved);
    assert!(
        rerun.contains("Same findings as run-") && rerun.contains(", some on other lines.\n"),
        "{rerun}"
    );

    // An over-budget rerun keeps every rule, the comparison and the outcome, and drops
    // error excerpts first.
    let first = cargo("cargo_many_before.txt");
    assert!(first.len() <= 4096, "{}", first.len());
    let rerun = cargo("cargo_many_after.txt");
    assert!(
        rerun.len() <= 4096
            && rerun
                .starts_with("cargo check: 17 finding(s), 16 error(s), 1 warning(s) · 17 rule(s)")
            && ["E0599", "E0603", "E0608", "E0614", "E0616", "E0618"]
                .iter()
                .all(|code| rerun.contains(&format!("error {code} ×1")))
            && rerun.contains("warning unused_assignments ×1")
            && rerun.contains("Compared with run-")
            && rerun.contains("- error E0308 ×1 (no longer reported)\n")
            && rerun.contains("error: could not compile `r08` (lib) due to 16 previous errors"),
        "{rerun}"
    );
    // A smaller budget drops whole groups with a count rather than cutting a line.
    let exe = root.join("bin/cargo");
    let output = cli(
        root,
        &[
            "run",
            "--max-bytes",
            "1024",
            "--",
            exe.to_str().unwrap(),
            "check",
        ],
    );
    let small = stdout(&output);
    assert!(
        small.len() <= 1024
            && small.contains("more rule(s) with ")
            && small.contains("error: could not compile `r08`"),
        "{small}"
    );

    // A baseline recorded without severities compares by rule alone.
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    let forget_severities = || {
        for entry in fs::read_dir(root.join(".justless/runs")).unwrap() {
            let path = entry.unwrap().path().join("record.json");
            let mut record: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            record["summary"]["outcomes"] = serde_json::json!([]);
            fs::write(&path, record.to_string()).unwrap();
        }
    };
    let eslint = lint_fixture("eslint.txt");
    lint_preview(root, "eslint", &["."], &eslint, "");
    forget_severities();
    let without_eqeqeq: String = eslint
        .lines()
        .filter(|line| !line.contains("eqeqeq"))
        .map(|line| format!("{line}\n"))
        .collect();
    let rerun = lint_preview(root, "eslint", &["."], &without_eqeqeq, "");
    assert!(
        rerun.contains("Compared with run-")
            && rerun.contains("- eqeqeq ×1 (no longer reported)\n")
            && !rerun.contains("+ "),
        "{rerun}"
    );
    forget_severities();
    let rerun = lint_preview(root, "eslint", &["."], &format!("{without_eqeqeq}\n"), "");
    assert!(rerun.contains("Same rule counts as run-"), "{rerun}");
}

#[cfg(unix)]
#[test]
fn lint_reports_read_pylint_categories_pyright_hadolint_and_staticcheck() {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    // pylint's E (error) and F (fatal) categories are errors, listed before conventions.
    let mut out = String::from(
        "************* Module app\n\
app.py:1:0: C0114: Missing module docstring (missing-module-docstring)\n\
app.py:3:4: E0602: Undefined variable 'foo' (undefined-variable)\n\
app.py:9:0: F0001: No module named nope (fatal)\n",
    );
    for i in 10..=30 {
        out.push_str(&format!(
            "app.py:{i}:0: C0116: Missing function or method docstring (missing-function-docstring)\n"
        ));
    }
    let report = lint_preview(root, "pylint", &["--fresh", "app.py"], &out, "");
    assert!(
        report.starts_with("pylint: 24 finding(s), 2 error(s), 22 warning(s)")
            && report.contains("\nerror E0602 ×1")
            && report.contains("\nerror F0001 ×1"),
        "{report}"
    );

    // These samples follow each tool's documented default output.
    let report = lint_preview(
        root,
        "pyright",
        &["--fresh"],
        &lint_fixture("pyright.txt"),
        "",
    );
    assert!(
        report.starts_with("pyright: 5 finding(s), 4 error(s), 1 warning(s) · 3 rule(s) · 2 file(s)\n")
            && report.contains("error reportMissingImports ×3  Import \"requestz\" could not be resolved\n  /work/app/main.py:1:8, 2:8, /work/app/util.py:3:8")
            && report.contains("        \"Literal['x']\" is not assignable to \"int\" (reportArgumentType)\n  /work/app/main.py:9:12 [stdout:4-5]")
            && report.contains("4 errors, 1 warning, 0 informations [stdout:9]"),
        "{report}"
    );
    let report = lint_preview(
        root,
        "hadolint",
        &["--fresh", "Dockerfile"],
        &lint_fixture("hadolint.txt").repeat(3),
        "",
    );
    assert!(
        report.starts_with(
            "hadolint: 21 finding(s), 3 error(s), 18 warning(s) · 6 rule(s) · 2 file(s)\n"
        ) && report.contains("error DL3020 ×3  Use COPY instead of ADD for files and folders\n")
            && report.contains("warning DL3008 ×6")
            && report.contains("  Dockerfile:3, 5 [stdout:1-1]"),
        "{report}"
    );
    let report = lint_preview(
        root,
        "staticcheck",
        &["--fresh", "./..."],
        &lint_fixture("staticcheck.txt").repeat(3),
        "",
    );
    assert!(
        report.contains(
            "warning S1028 ×6  should use fmt.Errorf(...) instead of errors.New(fmt.Sprintf(...))\n"
        ) && report.contains("warning SA1019 ×3")
            && report.contains("warning U1000 ×3"),
        "{report}"
    );
}

/// Writes Claude Code transcripts for `root` under `config`. Each file holds
/// Bash calls as (command, output) pairs.
fn write_transcripts(root: &Path, config: &Path, files: &[(&str, &[(&str, &str)])]) {
    let slug: String = root
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let base = config.join("projects").join(slug);
    for (file, calls) in files {
        let mut lines = Vec::new();
        for (i, (command, output)) in calls.iter().enumerate() {
            let id = format!("{file}-{i}");
            lines.push(serde_json::json!({"cwd":root,"timestamp":"2999-01-01T00:00:00Z",
                "message":{"content":[{"type":"tool_use","id":id,"name":"Bash","input":{"command":command}}]}}).to_string());
            lines.push(
                serde_json::json!({"cwd":root,"timestamp":"2999-01-01T00:00:01Z",
                "message":{"content":[{"type":"tool_result","tool_use_id":id,"content":output}]}})
                .to_string(),
            );
        }
        let path = base.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, lines.join("\n")).unwrap();
    }
}

#[test]
fn discover_reads_subagent_transcripts_and_labels_commands_without_arguments() {
    let repo = TempDir::new().unwrap();
    let root = repo.path().join("work");
    fs::create_dir(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let config = repo.path().join("claude");
    let footer = "ok\n[justless run-0lF7HYV5JgQV] exit 0 · 3 of 3 output bytes · justless log run-0lF7HYV5JgQV";
    let outline = "revision 3aa7da2e4ea6fb73b0c21cd8 · 200 lines\n#1 L1-1 function f";
    write_transcripts(
        &root,
        &config,
        &[
            ("S.jsonl", &[("cargo test", "test result: ok")]),
            (
                "S/subagents/agent-a.jsonl",
                &[("cd web && npm test", footer)],
            ),
            (
                "S/subagents/workflows/wf_1/agent-b.jsonl",
                &[
                    ("./target/debug/justless query f", "src/lib.rs:1"),
                    ("cat src/lib.rs", outline),
                ],
            ),
            (
                "T.jsonl",
                &[
                    ("cargo test 2>&1 | tail -20", "test result: FAILED"),
                    ("n=$(grep -n foo src/lib.rs); echo $n", ""),
                    ("mysql secretdb", "x"),
                    ("ssh prod-host uptime", "x"),
                    // Justless's own source mentions the footer text mid-line.
                    (
                        "sed -n 160,200p src/discover.rs",
                        "let route = if text_contains(content, \"[justless run-\")",
                    ),
                    ("cat notes.txt", "revision is not an outline"),
                ],
            ),
        ],
    );
    let output = Command::new(env!("CARGO_BIN_EXE_justless"))
        .args(["discover", "--json"])
        .current_dir(&root)
        .env("CLAUDE_CONFIG_DIR", &config)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["sessions"], 2);
    assert_eq!(report["commands"], 10);
    let labels = |route: &str| -> Vec<String> {
        report["groups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["route"] == route)
            .map(|g| {
                g["labels"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|l| format!("{} ×{}", l["label"].as_str().unwrap(), l["commands"]))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert_eq!(
        labels("through_justless"),
        ["cat ×1", "justless query ×1", "npm test ×1"]
    );
    assert_eq!(labels("supported_not_routed"), ["cargo test ×1"]);
    assert_eq!(labels("passed_through"), ["cargo test ×1"]);
    assert_eq!(
        labels("not_supported"),
        ["cat ×1", "mysql ×1", "other ×1", "sed ×1", "ssh ×1"]
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains("secretdb") && !text.contains("prod-host"));
}

#[test]
fn history_orders_by_completion_and_skips_unreadable_records() {
    let repo = mixed_fixture();
    let root = repo.path();
    let mut ids = Vec::new();
    for n in 1..=6 {
        let script = format!("echo cmd {n}: TimeoutError");
        ids.push(run_id(&cli(root, &["run", "--", "sh", "-c", &script])));
    }
    // A copy can reorder modification times, so the newest run gets the oldest.
    for (k, id) in ids.iter().enumerate() {
        let record = fs::File::options()
            .write(true)
            .open(root.join(".justless/runs").join(id).join("record.json"))
            .unwrap();
        let age = std::time::Duration::from_secs(1000 + k as u64 * 100);
        record
            .set_modified(std::time::SystemTime::now() - age)
            .unwrap();
    }
    let oldest = success(
        root,
        &["history", "TimeoutError", "--oldest", "--limit", "1"],
    );
    assert!(
        oldest.contains(&ids[0]) && oldest.contains("cmd 1: TimeoutError"),
        "{oldest}"
    );
    let listed: Vec<String> = success(root, &["history"])
        .lines()
        .map(|line| line.split(' ').next().unwrap().to_owned())
        .collect();
    assert_eq!(listed, ids.iter().rev().cloned().collect::<Vec<_>>());

    // An empty record and one missing required fields, sorted newest.
    for (id, record) in [
        ("run-empty", String::new()),
        (
            "run-partial",
            r#"{"id":"run-partial","status":0,"completed_ms":99999999999999}"#.to_owned(),
        ),
    ] {
        let directory = root.join(".justless/runs").join(id);
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("record.json"), record).unwrap();
    }
    for args in [
        &["history"][..],
        &["history", "TimeoutError"],
        &["stats"],
        &["resume"],
        &["pack", "where is TimeoutError raised"],
    ] {
        let output = cli(root, args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        let warnings = stderr(&output);
        assert!(
            warnings.contains("justless: skipped run run-empty: decode run record")
                && warnings.contains("justless: skipped run run-partial: decode run record"),
            "{args:?}: {warnings}"
        );
    }
    let resume = success(root, &["resume"]);
    let runs: Vec<_> = resume.lines().filter(|l| l.starts_with("run-")).collect();
    assert_eq!(runs.len(), 5, "{resume}");
    assert!(runs[0].starts_with(&ids[5]), "{resume}");
    let handoff = hook(
        root,
        "after-compact",
        serde_json::json!({"hook_event_name":"SessionStart","source":"compact"}),
    );
    assert!(
        handoff["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains(&ids[5])
    );
}
