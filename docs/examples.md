# Justless examples

Every example here runs on a fixture or on this repository, so you can reproduce it from a checkout. The output was captured at commit `bbe3d44` on macOS with Rust 1.93.0 and ripgrep 15.2.0. Run IDs and thread IDs differ on every run. Revisions are hashes of the file path and content, so yours will match. Byte counts for runs that print a `Compiling` line depend on the length of the directory path, and absolute paths are shortened to `…`.

Build the checkout binary and put it on your `PATH` first:

```sh
cargo build --locked
export PATH="$(pwd)/target/debug:$PATH"
```

## Diagnose and fix a failing test

`fixtures/failing` has a test that logs 351 trace lines and then fails one assertion. Copy it to a temporary directory and run the tests through Justless. The first command builds the tests, so the output has no `Compiling` line.

```sh
demo_root=$(mktemp -d)
cp -R fixtures/failing/. "$demo_root/"
cd "$demo_root"
cargo test --no-run   # build first so the output below has no Compiling line
justless run -- cargo test
```

```text
1 reported failing test(s)
1 test(s): total_includes_fee tests/checkout.rs:11:5: assertion `left == right` failed left: 106 right: 105 [stdout:365-368]
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.00s
     Running unittests src/lib.rs (target/debug/deps/checkout_fixture-4b58a8d3c9315df3)
     Running tests/checkout.rs (target/debug/deps/checkout-08b118bf0c4a4cc2)
error: test failed, to rerun pass `--test checkout`

[justless run-yKGf8E4eZx1V] exit 101 · 634 of 18038 output bytes · justless log run-yKGf8E4eZx1V
```

The command still exits with status 101. Cargo printed 18,038 bytes. Justless showed 634 of them: the failure, grouped by its diagnostic, and the result lines. `[stdout:365-368]` is the failure's range in the archived log.

`context --run` goes from the failure to the source. It shows the log lines, the failing test, the function the assertion calls and the source around the assertion.

```sh
justless context --run run-yKGf8E4eZx1V
```

```text
Run run-yKGf8E4eZx1V · 1 source locations
tests/checkout.rs:11 · log stdout:365
  Replay: justless log run-yKGf8E4eZx1V --stream stdout --from 363 --to 367
  stdout:363: billing-trace step=349 customer=demo state=ready
  stdout:364: 
  stdout:365: thread 'total_includes_fee' (2709939) panicked at tests/checkout.rs:11:5:
  stdout:366: assertion `left == right` failed
  stdout:367:   left: 106
checkout_fixture_test_checkout::total_includes_fee [test] at tests/checkout.rs:3-12
  calls checkout_fixture::pricing::checkout_total [function] at src/pricing.rs:1-3 (from tests/checkout.rs:11)
  8 |             println!("billing-trace probe=only-in-the-full-log");
  9 |         }
  10 |     }
  11 |     assert_eq!(total(100, 5), 105);
  12 | }
  Declaration starts at tests/checkout.rs:4
```

The call goes through an import alias and a re-export to `checkout_total`, which calls `apply_fee`. Before you change `apply_fee`, `affected` lists what depends on it, hop by hop. It also lists the tests that reach it and a command that runs only those tests.

```sh
justless affected apply_fee
```

```text
Affected by pricing::apply_fee [function] src/pricing.rs:5-7 · 4 dependents within 3 hops
Distance 1 · 1
  pricing::checkout_total [function] src/pricing.rs:1-3 · call at src/pricing.rs:2
Distance 2 · 3
  total_includes_fee [test] tests/checkout.rs:3-12 · call at tests/checkout.rs:11 → pricing::checkout_total
  file scope · import at src/lib.rs:4 → pricing::checkout_total
  file scope · import at tests/checkout.rs:1 → pricing::checkout_total
Tests · 1
  total_includes_fee [test] tests/checkout.rs:3-12 · 2 hops
Run
  cargo test --test checkout total_includes_fee
```

Read the file's outline to get a revision and numbered entries, then expand one entry to its exact bytes.

```sh
justless read src/pricing.rs --outline
justless expand src/pricing.rs apply_fee --json
```

```text
revision 15b9420c8f03a7aab4b400b6 · 7 lines
#1 L1-3 function checkout_total  pub fn checkout_total(subtotal: u32, fee: u32) -> u32
#2 L5-7 function apply_fee  fn apply_fee(subtotal: u32, fee: u32) -> u32
{"file":"src/pricing.rs","revision":"15b9420c8f03a7aab4b400b6","selector":"15b9420c8f03a7aab4b400b6#2","first":5,"last":7,"content":"fn apply_fee(subtotal: u32, fee: u32) -> u32 {\n    subtotal + fee + 1\n}"}
```

Send only the replacement for that entry. The edit applies because the file still matches the revision. The same edit sent again is rejected, because the file has changed since that revision. Add `--dry-run` to see a diff without writing.

```sh
rev=15b9420c8f03a7aab4b400b6   # the revision from your outline
printf 'fn apply_fee(subtotal: u32, fee: u32) -> u32 {\n    subtotal + fee\n}' | justless edit src/pricing.rs "${rev}#2"
printf 'fn apply_fee(subtotal: u32, fee: u32) -> u32 {\n    subtotal + fee\n}' | justless edit src/pricing.rs "${rev}#2"
```

```text
1 edits · revision a3937a19c37ceb2b33df929e
justless: stale revision for this file; read a fresh outline
```

The second command exits with status 2. Run the tests again. The rerun is compared with the previous run of the same command, so the agent sees that the failure is gone.

```sh
justless run -- cargo test
```

```text
0 reported failing test(s)
Compared with run-yKGf8E4eZx1V:
- total_includes_fee tests/checkout.rs:11:5: assertion `left == right` failed left: 106 right: 105 [prior stdout:365-368]
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
   Compiling checkout-fixture v0.1.0 (…)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.03s
     Running unittests src/lib.rs (target/debug/deps/checkout_fixture-4b58a8d3c9315df3)
     Running tests/checkout.rs (target/debug/deps/checkout-08b118bf0c4a4cc2)
   Doc-tests checkout_fixture

[justless run-fWJvr2leK2Qi] exit 0 · 876 of 782 output bytes · justless log run-fWJvr2leK2Qi
```

A line removed from a report is marked with `-`. Here, the summary was larger than the output it summarized, and the footer reports it. `justless stats` counts such runs as having grown.

## Recover exact bytes from an old run

The first run's full log is still archived. One line in the middle of the trace, `billing-trace probe=only-in-the-full-log`, never appeared in any summary. `search` finds it, and `log` replays it exactly.

```sh
justless search run-yKGf8E4eZx1V 'probe=' --stream stdout
justless log run-yKGf8E4eZx1V --stream stdout --from 189 --to 189
```

```text
stdout:189: billing-trace probe=only-in-the-full-log
billing-trace probe=only-in-the-full-log
```

`justless history TEXT` searches every archived run in the project, and `justless stats` adds up what stayed out of context:

```text
Kept 16.7 KB of 18.4 KB command output out of context (90.9%) across 2 run(s)
≈4279 tokens saved (estimated as bytes ÷ 4)

Last 7 day(s), UTC
  2026-11-18     2 run(s)    18.4 KB → 1.7 KB     90.9%  ████████████████████

By command, most saved first
  cargo test     2 run(s)    18.4 KB → 1.7 KB     90.9%  · 3 targeted lookup(s) · 1 grew

Recovery: 0 run(s) reopened in full · 3 targeted lookup(s) · 0 repeated source-range read(s)
```

## What a hook-routed command prints

A command you type ends with the full footer shown above, which scripts can parse. Hooks run commands as `justless run --hook`, which adds a footer only when it tells the agent something. You can type `justless run --hook -- COMMAND` to see exactly what an agent would receive. These are from a fresh copy of `fixtures/failing`, before the fix:

| Situation | Command | What follows the output |
| --- | --- | --- |
| Succeeded, shown in full | `rg -n checkout_total src` | Nothing. The two matching lines arrive as `rg` printed them. |
| Search matched nothing | `rg -n no_such_symbol src` | Nothing, and the exit status stays 1. |
| Failed, shown in full | `rg -n total missing` | `[justless run-FsqZyd9YoBZn · exit 2]` |
| Shortened and failed | `cargo test` | `[justless run-Rh1dgeGqKvnq · 634 B of 17.6 KB shown · exit 101]` |
| Shortened | `rg -n unwrap src tests` in this repository | `[justless run-yUmvc2zHMpO9 · 2.9 KB of 57.3 KB shown]` |

Every run is archived either way. `justless history` lists it, and the run ID in a marker works with `log`, `search` and `context --run`. When a rerun prints byte-for-byte the same output with the same exit status, the preview can collapse to `Output unchanged since run-…`.

## Let preview budgets follow what agents reopen

Without `--max-bytes`, each command family's preview budget moves with the project's own history. In a copy of this repository, seven hook-routed searches ran with `justless run --hook -- rg -n PATTERN` for `unwrap`, `expect`, `clone`, `pub fn`, `impl `, `fn ` and `Result<`. The second and fourth were replayed with `justless log RUN_ID`. Five clipped searches with two replays is more than a quarter, so the sixth search ran at 8 KB and showed 7.0 KB of 66.9 KB, where the first had shown 3.2 KB of 62.9 KB. Then 21 runs of `git log --stat -n N`, for N from 41 to 61, were never replayed in full, though three had a five-line `log` lookup. After 20 clipped runs, the 21st ran at 2 KB and showed 1012 B of 50.9 KB. `justless stats` ends with the reasons:

```text
Preview budgets per stream (default 4.0 KB, adapting from 2.0 KB to 16.0 KB)
  git log    2.0 KB · lowered from 4.0 KB: none of the last 20 clipped runs was reopened in full · since then 0 of 1 reopened, 5 needed
  rg         8.0 KB · raised from 4.0 KB: 2 of the last 5 clipped runs were reopened in full · since then 0 of 2 reopened, 5 needed
```

The `git log` budget is keyed by its command label because the `git` family covers every Git subcommand. Each later step is judged only by runs made at the new budget, and `adaptive_previews = false` in `.justless.toml` turns this off.

## Group search results by file

When a search's stdout is larger than the preview budget, 4,096 bytes by default, Justless groups it by file. Files with the most matches come first. Each group shows its first matching lines exactly as printed and the range of its lines in the full log. This search over this repository's own source printed 58,723 bytes. The agent received 3,058 bytes, abridged here.

```sh
justless run --hook -- rg -n unwrap src tests
```

```text
rg: 817 matches in 25 files
tests/cli.rs ×356 [stdout:6-361]
  10:    let target = TempDir::new().unwrap();
  +355 more in file
tests/progressive.rs ×153 [stdout:469-621]
  12:        .args(["--root", root.to_str().unwrap()])
  +152 more in file
tests/mcp.rs ×82 [stdout:372-453]
  11:    let target = TempDir::new().unwrap();
  +81 more in file
src/lint.rs ×37 [stdout:744-780]
  39:    LazyLock::new(|| Regex::new(r"^(error|warning)(?:\[([A-Za-z0-9_]+)\])?: (.+)$").unwrap());
  +36 more in file
[… 20 more files, one group each …]
src/workspace.rs ×1 [stdout:699]
  115:                .unwrap_or(&start)
[justless run-yUmvc2zHMpO9 · 2.9 KB of 57.3 KB shown]
```

`justless log run-yUmvc2zHMpO9 --stream stdout --from 469 --to 621` returns every match in `tests/progressive.rs`. File listings from `find`, `fd`, `tree`, `git ls-files`, `ls -l` and `ls -R` are grouped by directory the same way. Dependency and build directories such as `node_modules` and `target` collapse into a count unless the command names them.

## Group lint findings by rule

Compiler and linter output collapses to one group per rule, with a count, the first message and up to four locations. This repository passes Clippy's default lints. With the pedantic group turned on, Clippy printed 79,563 bytes for 142 warnings. The agent received 3,916 bytes, abridged here.

```sh
justless run --hook -- cargo clippy -- -W clippy::pedantic
```

```text
cargo clippy: 142 finding(s), 0 error(s), 142 warning(s) · 24 rule(s) · 21 file(s)
warning clippy::format_push_string ×41  `format!(..)` appended to existing `String`
  src/impact.rs:761:9, 769:9, src/lint.rs:600:13, 629:21 +37 more [stderr:585-597]
warning clippy::too_many_lines ×23  this function has too many lines (103/100)
  src/agents.rs:208:1, src/capture.rs:139:1, 460:1, src/context.rs:818:1 +19 more [stderr:111-119]
warning clippy::semicolon_if_nothing_returned ×20  consider adding a `;` to the last statement for consistent formatting
  src/context.rs:688:17, src/lint.rs:815:17, 818:17, src/main.rs:333:13 +16 more [stderr:352-360]
warning clippy::doc_markdown ×10  item in documentation is missing backticks
  src/lint.rs:6:7, 8:53, 9:28, 9:37 +6 more [stderr:699-712]
[… 19 more rules …]
warning clippy::unnested_or_patterns ×1  unnested or-patterns
  src/filters.rs:336:9 [stderr:1-25]
[justless run-RoAAVolMv61r · 3.8 KB of 77.7 KB shown]
```

Errors also keep the detail lines of their first occurrence, such as the expected and found types. A rerun lists the rules that appeared, changed count, changed findings at the same count or are no longer reported, and says when the same findings only moved to other lines. When the grouped report is not smaller than the ordinary preview, Justless shows the preview instead. That happened with `--all-targets` added here, which also lints the tests: Clippy printed 102,700 bytes, and the agent received a 4,028-byte preview. The [reference](reference.md#task-packs-and-run-history) lists the recognized tools.

## Orient in a repository and trace a request from TypeScript to SQL

`fixtures/mixed` has a TypeScript client, a Python handler, a SQL schema, a runbook and a config file with an unfamiliar extension. Copy it and build the index:

```sh
mixed_repo=$(mktemp -d)
cp -R fixtures/mixed/. "$mixed_repo/"
cd "$mixed_repo"
justless index
```

```text
6 text files · 9 symbols · 4 relationships · 4 parsed · 0 reused · 0 removed
```

`map` gives a first look at a repository from the index alone. It shows the areas, the most connected symbols, groups of linked symbols, links across areas and languages, and entry points. Every row has a file and line.

```sh
justless map
```

```text
Map · 6 files · 5 symbols · 4 links (2 possible) · TypeScript 2 · Python 1 · SQL 1
Areas · 4
  backend/ · 2 files · 2 symbols · Python · hub list_orders backend/orders.py:6-8
  frontend/ · 2 files · 2 symbols · TypeScript · hub formatOrder frontend/format.ts:1-3
  db/ · 1 file · 1 symbol · SQL · hub purchase_orders db/schema.sql:1-3
  docs/ · 1 file · 0 symbols · text · docs/operations.md:1
Hubs · in = dependents, out = dependencies
  formatOrder [function] frontend/format.ts:1-3 · in 1 · out 0
  list_orders [function] backend/orders.py:6-8 · in 1 (1 possible) · out 1 (1 possible)
  purchase_orders [table] db/schema.sql:1-3 · in 1 (1 possible) · out 0
  fetchOrders [function] frontend/orders.ts:3-7 · in 0 · out 1 (1 possible)
Communities · 2 groups of linked symbols
  backend/orders.py · 2 symbols · list_orders backend/orders.py:6-8, router backend/orders.py:3
  frontend/format.ts +1 file · 2 symbols · formatOrder frontend/format.ts:1-3, fetchOrders frontend/orders.ts:3-7
Cross-area links · 2
  backend/ → db/ · Python → SQL · possible SQL relation 1 · list_orders backend/orders.py:8 → purchase_orders db/schema.sql:1-3
  frontend/ → backend/ · TypeScript → Python · possible HTTP route 1 · fetchOrders frontend/orders.ts:4 → list_orders backend/orders.py:6-8
Entry points · 1
  list_orders [function] backend/orders.py:6-8 · HTTP route /api/orders
Tests · none detected
Next: justless expand FILE NAME, query NAME or affected NAME
```

`path` follows the literal `/api/orders` from TypeScript to a Python route decorator. It then follows `purchase_orders` from an embedded SQL statement to the schema. Both links are inferred from source literals, so they are labeled possible.

```sh
justless path fetchOrders purchase_orders
```

```text
frontend/orders.ts::fetchOrders [function] at frontend/orders.ts:3-7
  → possible HTTP route at frontend/orders.ts:4 → backend/orders.py::list_orders [function] at backend/orders.py:6-8
  → possible SQL relation at backend/orders.py:8 → db/schema.sql::purchase_orders [table] at db/schema.sql:1-3
```

`affected` walks the same links in reverse. A change to the table can reach the TypeScript client two hops away.

```sh
justless affected purchase_orders
```

```text
Affected by purchase_orders [table] db/schema.sql:1-3 · 2 dependents within 3 hops
Distance 1 · 1
  list_orders [function] backend/orders.py:6-8 · possible SQL relation at backend/orders.py:8
Distance 2 · 1
  fetchOrders [function] frontend/orders.ts:3-7 · possible HTTP route at frontend/orders.ts:4 → list_orders
Tests · none reach the target through indexed links. Search with justless find.
```

`find` searches for literal text in every indexed file, including the runbook and the config file that no parser recognizes:

```sh
justless find canary-checkout
```

```text
backend/settings.custom:1: deployment=canary-checkout
docs/operations.md:3: The deployment follows the canary-checkout procedure.
```

In a Git checkout, `justless affected --changes` starts the same walk from your uncommitted changes, and `justless query frontend/orders.ts --view dependencies` lists a file's imports.

## Run only the tests a change reaches

`fixtures/affected` is a Cargo workspace with three packages: `ledger-core`, `api`, which depends on it and has a binary with a black-box CLI test, and the independent `reports`. Copy it, commit it, and build its tests once:

```sh
affected_repo=$(mktemp -d)
cp -R fixtures/affected/. "$affected_repo/"
cd "$affected_repo"
git init -q && git add -A && git commit -qm fixture
cargo test --offline --no-run
```

Make value-added tax one cent too high, then ask what would run. The revision after `HEAD` is your fixture commit and differs from the one below.

```sh
perl -pi -e 's{cents \* RATE / 100$}{cents * RATE / 100 + 1}' ledger/src/tax.rs
justless test --affected --dry-run
```

```text
Affected tests · working tree vs HEAD ae17dc5 · 1 changed file · 1 starting symbol
Changed ledger/src/tax.rs · modified
Start tax::vat [function] ledger/src/tax.rs:1-8 · changed ledger/src/tax.rs:7-7
cargo · subset · 7 of 20 indexed tests in 4 of 7 test targets
  Run (selected tests): cargo test -p api -p ledger-core --lib -- money::tests::adds_vat tax::tests::vat_is_a_fifth tax::tests::vat_rounds_down tests::invoice_total_adds_vat --exact
  Run (whole targets): cargo test -p api -p ledger-core --test cli --test totals
  Run (doctests): cargo test -p ledger-core --doc -- tax::vat
  tax::vat (doctest) · --doc · exact · documentation example on a reached item
  tests::invoice_total_adds_vat · api --lib · possible · call at api/src/lib.rs:18 → invoice_total · possible call at api/src/lib.rs:5 → money::Money::with_vat · call at ledger/src/money.rs:7 → tax::vat
  api/tests/cli.rs (whole target) · api --test cli · scope · api/tests/cli.rs runs the api binary, which the change reaches
  money::tests::adds_vat · ledger-core --lib · possible · possible call at ledger/src/money.rs:28 → money::Money::with_vat · call at ledger/src/money.rs:7 → tax::vat
  tax::tests::vat_is_a_fifth · ledger-core --lib · exact · call at ledger/src/tax.rs:23 → tax::vat
  tax::tests::vat_rounds_down · ledger-core --lib · exact · call at ledger/src/tax.rs:28 → tax::vat
  invoice_includes_vat · ledger-core --test totals · possible · names vat at ledger/tests/totals.rs:11
  ledger/tests/totals.rs (whole target) · ledger-core --test totals · exact · import at ledger/tests/totals.rs:1 → tax::vat
  zero_has_no_vat · ledger-core --test totals · possible · names vat at ledger/tests/totals.rs:16
Nothing ran (dry run).
```

The `reports` package and the tests of `net`, `Display` and `greeting` are left out. `tests/totals.rs` runs whole because its tests call `vat` inside a local `check!` macro the index does not expand; the name match finds them too. `api/tests/cli.rs` runs whole because it runs the `api` binary. `tests/needs.rs` requires a feature the command does not enable, so it never appears, just as `cargo test` skips it. Run it with the command you would normally use:

```sh
justless test --affected -- cargo test --offline
```

```text
Affected tests · working tree vs HEAD ae17dc5 · 1 changed file · 1 starting symbol
cargo · subset · 7 of 20 indexed tests in 4 of 7 test targets
Run: cargo test --offline -p api -p ledger-core --lib -- <4 test names> --exact
1 reported failing test(s)
1 test(s): tests::invoice_total_adds_vat api/src/lib.rs:18:9: assertion `left == right` failed left: "12.01" right: "12.00" [stdout:9-12]
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s
   Compiling ledger-core v0.1.0 (…/ledger)
   Compiling api v0.1.0 (…/api)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.34s
     Running unittests src/lib.rs (target/debug/deps/api-aca6214acba402d6)
error: test failed, to rerun pass `-p api --lib`

[justless run-ewOytdva2e6P] exit 101 · 745 of 975 output bytes · justless log run-ewOytdva2e6P
Run: cargo test --offline -p api -p ledger-core --test cli --test totals
…
Run: cargo test --offline -p ledger-core --doc -- tax::vat
…
Subset failed (cargo): 3 of 3 tests that ran failed, exit 101 (7 of 20 indexed tests selected, 13 not run); the full suite was not run (2.7 s).
  ledger/src/tax.rs - tax::vat (line 3) · exact · documentation example on a reached item
  tests::invoice_total_adds_vat · possible · call at api/src/lib.rs:18 → invoice_total
  prints_the_total
Full suite: justless test · Why: justless test --why run-ewOytdva2e6P
```

The exit status is Cargo's 101. Cargo stops at the first failing test binary in each step, as `cargo test` does, so `ledger-core`'s own unit tests did not get to run; add `--no-fail-fast` to the command to see every failure. `justless test --why` shows why a test was chosen, from the record archived with the run:

```sh
justless test --why run-ewOytdva2e6P --test vat_rounds_down
```

```text
run-ewOytdva2e6P · cargo test (affected subset) · exit 101 · subset · HEAD ae17dc5
…
Changed ledger/src/tax.rs · modified
Start tax::vat [function] ledger/src/tax.rs:1-8 · changed ledger/src/tax.rs:7-7
Step filtered · run-ewOytdva2e6P · exit 101 · 1 ran · cargo test --offline -p api -p ledger-core --lib -- money::tests::adds_vat tax::tests::vat_is_a_fifth tax::tests::vat_rounds_down tests::invoice_total_adds_vat --exact
Step unfiltered · run-fPb8byuARoFI · exit 101 · 1 ran · cargo test --offline -p api -p ledger-core --test cli --test totals
Step doctests · run-4gvQHsYTYblg · exit 101 · 1 ran · cargo test --offline -p ledger-core --doc -- tax::vat
  tax::tests::vat_rounds_down · ledger-core --lib · exact · call at ledger/src/tax.rs:28 → tax::vat
Cost: 2.7 s · 2193 of 2593 output bytes shown
```

After a fix, the same command compares each step with its previous run and still labels the result a subset:

```sh
perl -pi -e 's{cents \* RATE / 100 \+ 1$}{(cents * RATE) / 100}' ledger/src/tax.rs
justless test --affected -- cargo test --offline
```

```text
…
Compared with run-ewOytdva2e6P:
- tests::invoice_total_adds_vat api/src/lib.rs:18:9: assertion `left == right` failed left: "12.01" right: "12.00" [prior stdout:9-12]
…
Subset passed (cargo): ran 8 tests reachable from your changes (7 of 20 indexed tests selected, 13 not run); the full suite was not run (3.5 s).
Full suite: justless test · Why: justless test --why run-EXOQcFyIjl9N
```

Before saying the tests pass, run the full suite:

```sh
justless test -- cargo test --offline
```

```text
…
Full suite (cargo): exit 0, 21 tests ran, 0 failed (5.0 s).
```

A change no test reaches runs the full suite and says why, and `--no-fallback` keeps a labeled low-confidence subset instead:

```sh
git checkout -q -- .
perl -pi -e 's/\{name\},\{cents\}/{name};{cents}/' ledger/src/report.rs
justless test --affected --dry-run | head -4
```

```text
Affected tests · working tree vs HEAD ae17dc5 · 1 changed file · 1 starting symbol
Changed ledger/src/report.rs · modified
Start report::csv [function] ledger/src/report.rs:1-6 · changed ledger/src/report.rs:4-4
cargo · full suite · no test reaches report::csv (ledger/src/report.rs:1-6) through indexed links
```
