# Justless examples

Every example here runs on a fixture or on this repository, so you can reproduce it from a checkout. The output was captured at commit `0725fb0` on macOS with Rust 1.93.0. Run IDs, revisions and thread IDs differ on every machine, and absolute paths are shortened to `…`.

Build the checkout binary and put it on your `PATH` first:

```sh
cargo build --locked
export PATH="$(pwd)/target/debug:$PATH"
```

## Diagnose and fix a failing test

`fixtures/failing` has a test that logs 350 trace lines and then fails one assertion. Copy it to a temporary directory and run the tests through Justless. The output below was captured after a first build, so it has no `Compiling` line.

```sh
demo_root=$(mktemp -d)
cp -R fixtures/failing/. "$demo_root/"
cd "$demo_root"
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

A command you type ends with the full footer shown above, which scripts can parse. Hooks run commands as `justless run --hook`, which adds a footer only when it tells the agent something. You can type `justless run --hook -- COMMAND` to see exactly what an agent would receive. These are from the same fixture:

| Situation | Command | What follows the output |
| --- | --- | --- |
| Succeeded, shown in full | `rg -n checkout_total src` | Nothing. The two matching lines arrive as `rg` printed them. |
| Search matched nothing | `rg -n no_such_symbol src` | Nothing, and the exit status stays 1. |
| Failed, shown in full | `rg -n total missing` | `[justless run-FsqZyd9YoBZn · exit 2]` |
| Shortened and failed | `cargo test` | `[justless run-Rh1dgeGqKvnq · 634 B of 17.6 KB shown · exit 101]` |
| Shortened | `rg -n unwrap src tests` in this repository | `[justless run-av6t8NyOdXQ6 · 2.9 KB of 39.3 KB shown]` |

Every run is archived either way. `justless history` lists it, and the run ID in a marker works with `log`, `search` and `context --run`. When a rerun prints byte-for-byte the same output with the same exit status, the preview can collapse to `Output unchanged since run-…`.

## Group search results by file

When a search's stdout is larger than the preview budget, 4,096 bytes by default, Justless groups it by file. Files with the most matches come first. Each group shows its first matching lines exactly as printed and the range of its lines in the full log. This search over this repository's own source printed 40,279 bytes. The agent received 3,049 bytes, abridged here.

```sh
justless run --hook -- rg -n unwrap src tests
```

```text
rg: 558 matches in 25 files
tests/cli.rs ×254 [stdout:6-259]
  10:    let target = TempDir::new().unwrap();
  +253 more in file
tests/progressive.rs ×62 [stdout:321-382]
  12:        .args(["--root", root.to_str().unwrap()])
  +61 more in file
tests/mcp.rs ×47 [stdout:270-316]
  11:    let target = TempDir::new().unwrap();
  +46 more in file
src/lint.rs ×29 [stdout:514-542]
  28:    LazyLock::new(|| Regex::new(r"^(error|warning)(?:\[([A-Za-z0-9_]+)\])?: (.+)$").unwrap());
  +28 more in file
[… 20 more files, one group each …]
src/workspace.rs ×1 [stdout:469]
  88:                .unwrap_or(&cwd)
[justless run-av6t8NyOdXQ6 · 2.9 KB of 39.3 KB shown]
```

`justless log run-av6t8NyOdXQ6 --stream stdout --from 321 --to 382` returns every match in `tests/progressive.rs`. File listings from `find`, `fd`, `tree`, `git ls-files`, `ls -l` and `ls -R` are grouped by directory the same way. Dependency and build directories such as `node_modules` and `target` collapse into a count unless the command names them.

## Group lint findings by rule

Compiler and linter output collapses to one line per rule, with a count, the first message and up to four locations. This repository passes Clippy's default lints. With the pedantic group turned on, Clippy printed 71,858 bytes for 127 warnings. The agent received 3,968 bytes, abridged here.

```sh
justless run --hook -- cargo clippy -- -W clippy::pedantic
```

```text
cargo clippy: 127 finding(s), 0 error(s), 127 warning(s) · 25 rule(s) · 21 file(s)
warning clippy::format_push_string ×36  `format!(..)` appended to existing `String`
  src/impact.rs:761:9, 769:9, src/lint.rs:361:13, 369:21 +32 more [stderr:571-583]
warning clippy::too_many_lines ×21  this function has too many lines (105/100)
  src/agents.rs:146:1, src/capture.rs:136:1, 455:1, src/context.rs:598:1 +17 more [stderr:98-106]
warning clippy::semicolon_if_nothing_returned ×17  consider adding a `;` to the last statement for consistent formatting
  src/lint.rs:385:25, src/map.rs:161:13, 227:9, src/rust_syntax.rs:266:17 +13 more [stderr:806-814]
warning clippy::map_unwrap_or ×10  called `map(<f>).unwrap_or_else(<g>)` on an `Option` value
  src/agents.rs:553:9, 575:9, 717:15, 920:31 +6 more [stderr:131-145]
[… 20 more rules …]
warning clippy::used_underscore_binding ×1  used underscore-prefixed binding
  src/index.rs:243:14 [stderr:602-615]
[justless run-Dx2yJZKpa3RB · 3.8 KB of 70.2 KB shown]
```

Errors also keep the detail lines of their first occurrence, such as the expected and found types. A rerun lists the rules that appeared, changed count or are no longer reported. When the grouped report is larger than the ordinary preview, Justless shows the preview instead. That happened with `--all-targets` added here: 142 warnings over 26 rules. The [reference](reference.md#task-packs-and-run-history) lists the recognized tools.

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
  backend/ · 2 files · 2 symbols · Python · hub list_orders backend/orders.py:7-8
  frontend/ · 2 files · 2 symbols · TypeScript · hub formatOrder frontend/format.ts:1-3
  db/ · 1 file · 1 symbol · SQL · hub purchase_orders db/schema.sql:1-3
  docs/ · 1 file · 0 symbols · text · docs/operations.md:1
Hubs · in = dependents, out = dependencies
  formatOrder [function] frontend/format.ts:1-3 · in 1 · out 0
  list_orders [function] backend/orders.py:7-8 · in 1 (1 possible) · out 1 (1 possible)
  purchase_orders [table] db/schema.sql:1-3 · in 1 (1 possible) · out 0
  fetchOrders [function] frontend/orders.ts:3-7 · in 0 · out 1 (1 possible)
Communities · 2 groups of linked symbols
  backend/orders.py · 2 symbols · list_orders backend/orders.py:7-8, router backend/orders.py:3
  frontend/format.ts +1 file · 2 symbols · formatOrder frontend/format.ts:1-3, fetchOrders frontend/orders.ts:3-7
Cross-area links · 2
  backend/ → db/ · Python → SQL · possible SQL relation 1 · list_orders backend/orders.py:8 → purchase_orders db/schema.sql:1-3
  frontend/ → backend/ · TypeScript → Python · possible HTTP route 1 · fetchOrders frontend/orders.ts:4 → list_orders backend/orders.py:7-8
Entry points · 1
  list_orders [function] backend/orders.py:7-8 · HTTP route /api/orders
Tests · none detected
Next: justless expand FILE NAME, query NAME or affected NAME
```

`path` follows the literal `/api/orders` from TypeScript to a Python route decorator. It then follows `purchase_orders` from an embedded SQL statement to the schema. Both links are inferred from source literals, so they are labeled possible.

```sh
justless path fetchOrders purchase_orders
```

```text
frontend/orders.ts::fetchOrders [function] at frontend/orders.ts:3-7
  → possible HTTP route at frontend/orders.ts:4 → backend/orders.py::list_orders [function] at backend/orders.py:7-8
  → possible SQL relation at backend/orders.py:8 → db/schema.sql::purchase_orders [table] at db/schema.sql:1-3
```

`affected` walks the same links in reverse. A change to the table can reach the TypeScript client two hops away.

```sh
justless affected purchase_orders
```

```text
Affected by purchase_orders [table] db/schema.sql:1-3 · 2 dependents within 3 hops
Distance 1 · 1
  list_orders [function] backend/orders.py:7-8 · possible SQL relation at backend/orders.py:8
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
