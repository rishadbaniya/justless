# Justless examples

Every example here runs on a fixture or on this repository, so you can reproduce it from a checkout. The output was captured at commit `5e96988` on Linux x86_64 with a release build (Rust 1.99.0-nightly, ripgrep 13.0.0). Run IDs and thread IDs differ on every run. Revisions are hashes of the file path and content, so yours will match. Byte counts for runs that print a `Compiling` line depend on the length of the directory path, and absolute paths are shortened to `…`.

Build the checkout binary and put it on your `PATH` first:

```sh
cargo build --release --locked
export PATH="$(pwd)/target/release:$PATH"
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
test result: FAILED. 0 passed; 1 failed; 0 ignored · 2 test binaries
  0 passed, 1 failed  tests/checkout.rs [stdout:7-375]
  1 ran no tests: unittests src/lib.rs
stderr: 3 cargo status line(s) [stderr:1-3]
stderr: 1 other line(s) [stderr:4-4]
  error: test failed, to rerun pass `--test checkout`

[justless run-XgBduwQ5zt7I] exit 101 · 451 of 18038 output bytes · justless log run-XgBduwQ5zt7I
```

The command still exits with status 101. Cargo printed 18,038 bytes. Justless showed 451 of them (517 bytes including the footer): the failure grouped by its diagnostic, the per-binary result with empty `unittests src/lib.rs` collapsed, and Cargo's status lines folded to a range. `[stdout:365-368]` is the failure's range in the archived log.

`context --run` goes from the failure to the source. It shows the log lines, the failing test, the function the assertion calls and the source around the assertion.

```sh
justless context --run run-XgBduwQ5zt7I
```

```text
Run run-XgBduwQ5zt7I · 1 source locations
tests/checkout.rs:11 · log stdout:365
  Replay: justless log run-XgBduwQ5zt7I --stream stdout --from 363 --to 367
  stdout:363: billing-trace step=349 customer=demo state=ready
  stdout:364: 
  stdout:365: thread 'total_includes_fee' (3779206) panicked at tests/checkout.rs:11:5:
  stdout:366: assertion `left == right` failed
  stdout:367:   left: 106
checkout_fixture_test_checkout::total_includes_fee [test] at tests/checkout.rs:3-12
  calls checkout_fixture::pricing::checkout_total [function] at src/pricing.rs:1-3 (from :11)
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
Compared with run-XgBduwQ5zt7I:
- total_includes_fee tests/checkout.rs:11:5: assertion `left == right` failed left: 106 right: 105 [prior stdout:365-368]
test result: ok. 1 passed; 0 failed; 0 ignored · 3 test binaries
  1 passed  tests/checkout.rs [stdout:7-10]
  2 ran no tests: unittests src/lib.rs, doc-tests checkout_fixture
stderr: 5 cargo status line(s) [stderr:1-5]

[justless run-7ycmenYFytou] exit 0 · 375 of 710 output bytes · justless log run-7ycmenYFytou
```

A line removed from a report is marked with `-`. Empty test targets (`unittests src/lib.rs`, `doc-tests checkout_fixture`) and Cargo's status lines collapse, so even this small passing rerun shrinks from 710 to 375 bytes.

## Recover exact bytes from an old run

The first run's full log is still archived. One line in the middle of the trace, `billing-trace probe=only-in-the-full-log`, never appeared in any summary. `search` finds it, and `log` replays it exactly.

```sh
justless search run-XgBduwQ5zt7I 'probe=' --stream stdout
justless log run-XgBduwQ5zt7I --stream stdout --from 189 --to 189
```

```text
stdout:189: billing-trace probe=only-in-the-full-log
billing-trace probe=only-in-the-full-log
```

`justless history TEXT` searches every archived run in the project, and `justless stats` adds up what stayed out of context. The first figure is what the previews kept out. The net figure also subtracts the 491 bytes that `context --run`, `search` and `log` printed from the first run afterwards:

```text
Kept 17.3 KB of 18.3 KB command output out of context at first preview (94.5%) across 2 run(s)
Later replays and lookups printed 491 B of run output again, so net saved: 16.8 KB (91.9%)
≈8034 tokens saved at first preview, ≈7811 net (estimated at 2.21 bytes per token for logs)

Last 7 day(s), UTC, with retrievals on the day their run completed
  2026-11-25     2 run(s)    18.3 KB → 1022 B     94.5%  net   91.9%  ████████████████████

By command, most net savings first
  cargo test     2 run(s)    18.3 KB → 1022 B     94.5%  net   91.9%  · 3 targeted lookup(s) · 491 B retrieved later

Recovery: 0 run(s) reopened in full · 3 targeted lookup(s) · 0 repeated source-range read(s)
Source outlines: 1 view(s) printed 205 B instead of 160 B (+208 B expanded), net -253 B kept out (≈-99 tokens, estimated at 2.57 bytes per token for code)
```

Replaying the whole first log with `justless log run-XgBduwQ5zt7I` would print its 18,038 bytes again and take the net figure below zero: the preview, the lookups and the replay together would put more of that run in context than running `cargo test` directly.

## What a hook-routed command prints

A command you type ends with the full footer shown above, which scripts can parse. Hooks run commands as `justless run --hook`, which adds a footer only when it tells the agent something. You can type `justless run --hook -- COMMAND` to see exactly what an agent would receive. These are from a fresh copy of `fixtures/failing`, before the fix:

| Situation | Command | What follows the output |
| --- | --- | --- |
| Succeeded, shown in full | `rg -n checkout_total src` | Nothing. The two matching lines arrive as `rg` printed them. |
| Search matched nothing | `rg -n no_such_symbol src` | Nothing, and the exit status stays 1. |
| Failed, shown in full | `rg -n total missing` | `[justless run-eDm6wne8yVYy · exit 2]` |
| Shortened and failed | `cargo test` | `[justless run-YAIUcTaR5J2m · 451 B of 17.6 KB shown · exit 101]` |
| Shortened | `rg -n unwrap src tests` in this repository | `[justless run-Coh5niYHtwAE · 3.9 KB of 407.2 KB shown]` |

Every run is archived either way. `justless history` lists it, and the run ID in a marker works with `log`, `search` and `context --run`. When a rerun prints byte-for-byte the same output with the same exit status, the preview can collapse to `Output unchanged since run-…`.

## Let preview budgets follow what agents reopen

Without `--max-bytes`, each command family's preview budget moves with the project's own history. In a checkout of this repository, seven hook-routed searches ran with `justless run --hook -- rg -n PATTERN` for `unwrap`, `expect`, `clone`, `pub fn`, `impl `, `fn ` and `Result<`. The second and fourth were replayed with `justless log RUN_ID`, which replays both streams. Each grouped report had filled most of the budget, so a larger budget would have shown more of it. Five clipped searches with two replays is more than a quarter, so the sixth search ran at 8 KB and showed 7.0 KB of 70.1 KB, where the first had shown 3.2 KB of 68.2 KB. Then 21 runs of `git log --stat -n N`, for N from 41 to 61, were never replayed in full, though three had a five-line `log` lookup. After 20 clipped runs, the 21st ran at 2 KB and showed 1.0 KB of 53.0 KB. `justless stats` ends with the reasons:

```text
Preview budgets per stream (default 4.0 KB, adapting from 2.0 KB to 16.0 KB)
  git log    2.0 KB · lowered from 4.0 KB: none of the last 20 clipped runs was reopened in full · since then 0 of 1 clipped run reopened in full, 5 needed
  rg         8.0 KB · raised from 4.0 KB: 2 of the last 5 clipped runs were reopened in full · since then 0 of 2 clipped runs reopened in full, 5 needed
```

The `git log` budget is keyed by its command label because the `git` family covers every Git subcommand. Each later step is judged only by runs made at the new budget. Replaying only a summarized stream would not have counted: `justless log RUN_ID --stream stdout` on a failing `cargo test`, whose stdout was summarized while its compiler warnings were clipped on stderr, leaves the budget alone and `stats` says so. `adaptive_previews = false` in `.justless.toml` turns this off.

## Group search results by file

When a search's stdout is larger than the preview budget, 4,096 bytes by default, Justless groups it by file. Files with the most matches come first. Each group shows its first matching lines — folding identical lines across multiple line numbers as `L10,28,380: …` — and the range of its lines in the full log. Overflow files collapse by directory. This search over this repository's own source printed 416,963 bytes (407.2 KB). The agent received 3,987 bytes (4,043 bytes with the footer), abridged here.

```sh
justless run --hook -- rg -n unwrap src tests
```

```text
rg: 5249 matches in 129 files
tests/cli.rs ×1247 [stdout:648-1894]
  L10,28,380: let target = TempDir::new().unwrap();
  L21,39,1057,2889,3378,4547,4984: fs::create_dir_all(path.parent().unwrap()).unwrap();
  +1237 more in file
tests/progressive.rs ×197 [stdout:3592-3788]
  12:        .args(["--root", root.to_str().unwrap()])
  L18,275,654,733,1050,1068,1080,1095,1113,1208,1222,1234,1246,1260,1272 (+10 more): .unwrap();
  +171 more in file
tests/proxy.rs ×149 [stdout:3789-3937]
  53:        self.first.split(' ').nth(1).unwrap_or_default()
  57:        self.first.split(' ').nth(1).unwrap().parse().unwrap()
  +147 more in file
src/lint.rs ×139 [stdout:2087-2225]
  127:    LazyLock::new(|| Regex::new(r"^(error|warning)(?:\[([A-Za-z0-9_/-]+)\])?: (.+)$").unwrap());
  129:    LazyLock::new(|| Regex::new(r"^(TS\d+)\s+\[(ERROR|WARNING)\]:\s+(.+)$").unwrap());
  +137 more in file
[… 39 more files …]
+86 more files with 1404 matches in src/ 42 files ×725, tests/testsuite/ 21 files ×459, src/lookup/ 8 files ×87, src/proxy/ 6 files ×72, src/learn/ 4 files ×42, src/batch/ 4 files ×15, … [stdout:1-5249]
[justless run-Coh5niYHtwAE · 3.9 KB of 407.2 KB shown]
```

`justless log run-Coh5niYHtwAE --stream stdout --from 3592 --to 3788` returns every match in `tests/progressive.rs`. File listings from `find`, `fd`, `tree`, `git ls-files`, `ls -l` and `ls -R` are grouped by directory the same way. Dependency and build directories such as `node_modules` and `target` collapse into a count unless the command names them.

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

`map` gives a first look at a repository from the index alone. It shows the areas, the most connected symbols (`Hubs`), consolidated communities of linked symbols (and cross-community `Bridges` on larger repositories), links across areas and languages, and entry points. Every row has a file and line.

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
Hubs · 4 · in/out
  purchase_orders [table] db/schema.sql:1-3 · in 1 (1 possible) · out 0
  list_orders [function] backend/orders.py:6-8 · in 1 (1 possible) · out 1 (1 possible)
  formatOrder [function] frontend/format.ts:1-3 · in 1 · out 0
  fetchOrders [function] frontend/orders.ts:3-7 · in 0 · out 1 (1 possible)
Communities · 1 · 3 isolated
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

`find` searches for literal text in every indexed file, including the runbook and the config file that no parser recognizes. When a file has three or more matches, `find` automatically groups them under the filename to avoid repeating paths:

```sh
justless find canary-checkout
justless find order
```

```text
backend/settings.custom:1: deployment=canary-checkout
docs/operations.md:3: The deployment follows the canary-checkout procedure.

backend/orders.py:
  6: @router.get("/api/orders")
  7: def list_orders():
  8: return database.execute("SELECT order_id FROM purchase_orders")
db/schema.sql:1: CREATE TABLE purchase_orders (
db/schema.sql:2: order_id INTEGER PRIMARY KEY
frontend/orders.ts:
  4: const response = await fetch('/api/orders');
  5: const orders = await response.json() as string[];
  6: return orders.map(formatOrder);
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
Affected tests · working tree vs HEAD 6022e54 · 1 changed file · 1 starting symbol
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
Affected tests · working tree vs HEAD 6022e54 · 1 changed file · 1 starting symbol
cargo · subset · 7 of 20 indexed tests in 4 of 7 test targets
Run: cargo test --offline -p api -p ledger-core --lib -- <4 test names> --exact
1 reported failing test(s)
1 test(s): tests::invoice_total_adds_vat api/src/lib.rs:18:9: assertion `left == right` failed left: "12.01" right: "12.00" [stdout:9-12]
test result: FAILED. 0 passed; 1 failed; 0 ignored; 1 filtered out · 1 test binaries
  0 passed, 1 failed  unittests src/lib.rs [stdout:2-19]
stderr: 4 cargo status line(s) [stderr:1-4]
stderr: 1 other line(s) [stderr:5-5]
  error: test failed, to rerun pass `-p api --lib`

[justless run-rPvjYRpSg6Yu] exit 101 · 440 of 833 output bytes · justless log run-rPvjYRpSg6Yu
Run: cargo test --offline -p api -p ledger-core --test cli --test totals
…
Run: cargo test --offline -p ledger-core --doc -- tax::vat
…
Subset failed (cargo): 3 of 3 tests that ran failed, exit 101 (7 of 20 indexed tests selected, 13 not run); the full suite was not run (0.9 s).
  ledger/src/tax.rs - tax::vat (line 3) · exact · documentation example on a reached item
  tests::invoice_total_adds_vat · possible · call at api/src/lib.rs:18 → invoice_total
  prints_the_total
Full suite: justless test -- cargo test --offline · Why: justless test --why run-rPvjYRpSg6Yu
```

The exit status is Cargo's 101. Cargo stops at the first failing test binary in each step, as `cargo test` does, so `ledger-core`'s own unit tests did not get to run; add `--no-fail-fast` to the command to see every failure. `justless test --why` shows why a test was chosen, from the record archived with the run:

```sh
justless test --why run-rPvjYRpSg6Yu --test vat_rounds_down
```

```text
run-rPvjYRpSg6Yu · cargo test (affected subset) · exit 101 · subset · HEAD 6022e54
…
Changed ledger/src/tax.rs · modified
Start tax::vat [function] ledger/src/tax.rs:1-8 · changed ledger/src/tax.rs:7-7
Step filtered · run-rPvjYRpSg6Yu · exit 101 · 1 ran · cargo test --offline -p api -p ledger-core --lib -- money::tests::adds_vat tax::tests::vat_is_a_fifth tax::tests::vat_rounds_down tests::invoice_total_adds_vat --exact
Step unfiltered · run-xPuVS7mvpE5Y · exit 101 · 1 ran · cargo test --offline -p api -p ledger-core --test cli --test totals
Step doctests · run-dxRaUuaZp2KH · exit 101 · 1 ran · cargo test --offline -p ledger-core --doc -- tax::vat
  tax::tests::vat_rounds_down · ledger-core --lib · exact · call at ledger/src/tax.rs:28 → tax::vat
Cost: 0.9 s · 1582 of 2309 output bytes shown
```

After a fix, the same command compares each step with its previous run and still labels the result a subset:

```sh
perl -pi -e 's{cents \* RATE / 100 \+ 1$}{(cents * RATE) / 100}' ledger/src/tax.rs
justless test --affected -- cargo test --offline
```

```text
…
Compared with run-rPvjYRpSg6Yu:
- tests::invoice_total_adds_vat api/src/lib.rs:18:9: assertion `left == right` failed left: "12.01" right: "12.00" [prior stdout:9-12]
…
Subset passed (cargo): ran 8 tests reachable from your changes (7 of 20 indexed tests selected, 13 not run); the full suite was not run (1.0 s).
Full suite: justless test -- cargo test --offline · Why: justless test --why run-m5aZm0tBZsnN
```

Before saying the tests pass, run the full suite:

```sh
justless test -- cargo test --offline
```

```text
…
Full suite (cargo): exit 0, 21 tests ran, 0 failed (0.7 s).
```

A change no test reaches runs the full suite and says why, and `--no-fallback` keeps a labeled low-confidence subset instead:

```sh
git checkout -q -- .
perl -pi -e 's/\{name\},\{cents\}/{name};{cents}/' ledger/src/report.rs
justless test --affected --dry-run | head -4
```

```text
Affected tests · working tree vs HEAD 6022e54 · 1 changed file · 1 starting symbol
Changed ledger/src/report.rs · modified
Start report::csv [function] ledger/src/report.rs:1-6 · changed ledger/src/report.rs:4-4
cargo · full suite · no test reaches report::csv (ledger/src/report.rs:1-6) through indexed links
```

## Explain a symbol's architectural role

On a larger repository, `justless map` groups symbols into consolidated `Communities` and surfaces cross-community `Bridges` (in this repository at `5e96988`, 236 communities and 186 bridges across 9,092 symbols). `justless map --explain NAME` drills into a single symbol to report its architectural role, PageRank-style centrality rank, blast radius, community peers, cross-community bridges, callers, dependencies and reachable tests in one 2.3 KB answer:

```sh
justless map --explain would_route
```

```text
Explain · justless_bin::agents::would_route [function] src/agents.rs:4382-4405 · role: bridge
Centrality · rank #1,183 of 9,092 · in 5 · out 8 (2 possible) · blast: 5 dependents (1-hop), 7 dependents (3-hop), 3 tests
Community · src/discover.rs +9 files · 92 symbols, 10 files · peers: history::human_bytes, rewrite::Exclusions, history::label
Cross-community bridges · 5
  → batch::Batch::parse_list [method] src/batch.rs:160-162 · call at src/agents.rs:4390 · src/batch/parse.rs +4 files
  → agents::plan [function] src/agents.rs:3648-3813 · call at src/agents.rs:4402 · src/agents.rs +4 files
  → batch::Batch [struct] src/batch.rs:20 · reference at src/agents.rs:4390 · src/batch/parse.rs +4 files
  → agents::Rules [struct] src/agents.rs:2771-2797 · reference at src/agents.rs:4394 · src/agents.rs +4 files
  … +1 more
Callers · 5
  discover::discover [function] src/discover.rs:471-786 · reference at src/discover.rs:604
  agents::tests [module] src/agents.rs:4792-5221 · import at src/agents.rs:4796
  agents::tests::mixed_chains_with_vars_and_redirects_route_for_mixed_clients_only [test] src/agents.rs:4950-5049 · call at src/agents.rs:4964 ×2
  discover::tests::wave2_routing_rules_classify_accurately_across_claude_codex_and [test] src/discover.rs:1200-1325 · reference at src/discover.rs:1206 ×2
  … +1 more
Dependencies · 8
  batch::Batch::parse_list [method] src/batch.rs:160-162 · call at src/agents.rs:4390
  rewrite::Exclusions::line [method] src/rewrite.rs:122-127 · call at src/agents.rs:4388
  agents::plan [function] src/agents.rs:3648-3813 · call at src/agents.rs:4402
  rewrite::Exclusions [struct] src/rewrite.rs:76-78 · reference at src/agents.rs:4386 ×2
  batch::Batch [struct] src/batch.rs:20 · reference at src/agents.rs:4390
  … +3 more
Reachable tests (≤3 hops) · 3
  agents::tests::mixed_chains_with_vars_and_redirects_route_for_mixed_clients_only [test] src/agents.rs:4950-5049 · 1 hop
  discover::tests::wave2_routing_rules_classify_accurately_across_claude_codex_and [test] src/discover.rs:1200-1325 · 1 hop
  discover::tests::mixed_var_chains_classify_as_supported_not_routed_for_claude_an [test] src/discover.rs:1424-1470 · 1 hop
Next: justless expand src/agents.rs L4382-4405, query agents::would_route or affected agents::would_route
```

## Trace a natural-language question across languages

When you do not know exact symbol names yet, `justless lookup` ranks matching symbols and sections across languages and — when a question asks how components connect — traverses the code graph to surface multi-hop paths automatically. Here it traces a TypeScript caller through a Python route handler to the SQL table in `fixtures/mixed`:

```sh
justless --root fixtures/mixed lookup "how does orders connect to purchase_orders?"
```

```text
db/schema.sql:1-3 table purchase_orders
  1 | CREATE TABLE purchase_orders (
  2 |   order_id INTEGER PRIMARY KEY
backend/orders.py:6-8 function list_orders
  6 | @router.get("/api/orders")
  8 |     return database.execute("SELECT order_id FROM purchase_orders")
frontend/orders.ts:3-7 function fetchOrders
  3 | export async function fetchOrders() {
  4 |   const response = await fetch('/api/orders');
frontend/format.ts:1-3 function formatOrder
  1 | export function formatOrder(value: string): string {
docs/operations.md:1-3 section Order service runbook
  1 | # Order service runbook
backend/orders.py:3 constant router
  3 | router = APIRouter()
Graph paths · 1
  fetchOrders [function · TypeScript] frontend/orders.ts:3-7 (2 hops)
    → possible HTTP route at frontend/orders.ts:4 → list_orders [function · Python] backend/orders.py:6-8
    → possible SQL relation at backend/orders.py:8 → purchase_orders [table · SQL] db/schema.sql:1-3
6 of 6 ranked · matched orders, purchase_orders · no match connect
Next: justless expand db/schema.sql L1-3
```

## Outline Markdown by headings, tables and code blocks, then expand a range

`read --outline` parses Markdown into numbered `h1`/`h2`/`h3` headings, `table` anchors (with column headers and row counts), and fenced `code` blocks. Outlining this repository's 273,594-byte (1,142-line) `docs/reference.md` returns 1,598 bytes on the first page, and `expand` accepts `#A..#B` entry ranges to pull back only the exact sections you need:

```sh
justless read docs/reference.md --outline
```

```text
revision 5ed9fdb08d39e4ae9bf0c355 · 1142 lines
#1 L1-1142 h1 Justless reference
#2 L3-53 h2 Commands
#3 L5-50 table Command | Result (44 rows)
#4 L54-77 h2 Indexing and languages
#5 L60-77 h3 Narrow and page `find`
#6 L78-101 h2 Reading a symbol again
#7 L102-137 h2 Repository map and change impact
#8 L138-263 h2 Affected tests
#9 L154-191 h3 How tests are chosen
#10 L163-174 table Runner | Found from | Unit | Command (10 rows)
#11 L192-215 h3 When the full suite runs instead
#12 L194-212 table Reason | When (17 rows)
#13 L216-234 h3 Output, exit status and records
#14 L235-263 h3 Failures compared with a base
#15 L264-299 h2 Progressive reads and verified edits
#16 L300-325 h2 Recoverable tool history
#17 L308-322 code python
#18 L326-404 h2 Task packs and run history
#19 L405-464 h2 Configuration
#20 L409-427 code toml
[… #21 through #37 …]
#38 L797-800 h3 Codex
#39 L801-815 h3 Thinking and history edits
#40 L816-834 h3 Stats
… 14 more entries (40 of 54)
next --after 5ed9fdb08d39e4ae9bf0c355#40
```

Now expand just `#38` through `#39` (1,667 exact UTF-8 bytes out of 273,594):

```sh
justless expand docs/reference.md '5ed9fdb08d39e4ae9bf0c355#38..#39'
```

```text
### Codex

Codex is not supported. It uses the OpenAI Responses API, including a WebSocket transport, and manages its own context. That API has no equivalent request parameter for the proxy to add.

### Thinking and history edits

`justless mask` and the OpenCode plugin rewrite earlier tool results. On Claude models that tie thinking to the conversation, that invalidates every later thinking block, so they now return those requests byte for byte:

| Request | Model | `mask` |
| --- | --- | --- |
| Messages | Claude 3 and 4 models, Opus, Sonnet, Fable and Mythos 5, Mythos Preview | Retires results as before |
| Messages | Every other model, including Opus, Sonnet and Haiku 5.5, Fable and Mythos 5.1 and later, and unknown names | Unchanged |
| Chat Completions, Responses or OpenCode | A Claude model | The same rule as Messages |
| Chat Completions, Responses or OpenCode | Another model | Retires results as before |
| OpenCode | None given with `--model` | Unchanged |
| Messages, Chat Completions or Responses | None | Retires results, unless the request carries thinking or reasoning items |

The model is `--model ID` when given, else the request's `model`. Provider prefixes and suffixes such as `openrouter/anthropic/`, `us.anthropic.`, `databricks-`, `anthropic:`, `@20251101`, `-v1:0` and `[1m]` are ignored, and any other name that mentions Claude counts as a Claude model. `--stats` reports why a request was held, and `--restore` always expands references. The OpenCode plugin passes the session's selected model. Run `justless init --hooks --agent opencode` again to update an installed plugin, which leaves every request unchanged until then.
```
