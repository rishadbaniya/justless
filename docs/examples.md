# Justless examples

## Trace a request from TypeScript to SQL

Install Justless, then run these commands from the root of the Justless checkout.

```sh
mixed_repo=$(mktemp -d)
cp -R fixtures/mixed/. "$mixed_repo/"
cd "$mixed_repo"
justless index
justless path fetchOrders purchase_orders
justless query frontend/orders.ts --view dependencies
justless find canary-checkout
```

The path follows the literal `/api/orders` from TypeScript to a Python route decorator, then follows `purchase_orders` from an embedded SQL statement to the schema. Each relationship includes its source line. The two cross-language relationships are labeled possible because they are inferred from source literals. The text lookup finds the runbook and an unrecognized config extension.

## Reproduce the failing test demo

From the Justless checkout, copy the fixture to a temporary directory and run the test through Justless. The intentional failure leaves the exit status at `101`.

```sh
cargo build --locked
export PATH="$(pwd)/target/debug:$PATH"
demo_root=$(mktemp -d)
cp -R fixtures/failing/. "$demo_root/"
cd "$demo_root"
justless init-codex
demo_status=0
result=$(justless run --max-bytes 512 -- cargo test --offline 2>&1) || demo_status=$?
printf '%s\n' "$result"
test "$demo_status" -eq 101

run_id=$(printf '%s\n' "$result" | sed -n 's/^\[justless \(run-[[:alnum:]]*\)\].*/\1/p')
justless context --run "$run_id" --budget 2400
justless context --symbol checkout_total
justless query apply_fee --view tests

match=$(justless search "$run_id" 'probe=' --stream stdout)
line=$(printf '%s\n' "$match" | cut -d: -f2)
justless log "$run_id" --stream stdout --from "$line" --to "$line"
```

The run links to `tests/checkout.rs:11`, where `total(100, 5)` returns `106` instead of the expected `105`. The call resolves through an import alias and a re-export to `src/pricing.rs:1-3`. The last command replays the exact middle line `billing-trace probe=only-in-the-full-log`.
