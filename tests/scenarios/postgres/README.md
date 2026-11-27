# PostgreSQL acceptance test fixture

Requires Docker Compose and Cargo. Starts a disposable PostgreSQL 17 container.
Each live scenario creates and drops its own test databases. The supplied test
role must have `CREATEDB`.

```sh
docker compose -f tests/scenarios/postgres/compose.yaml up -d --wait
FLEDGLING_TEST_POSTGRES_URL=postgresql://fledgling:fledgling@127.0.0.1:55432/fledgling_test \
cargo test --test postgres -- --nocapture
docker compose -f tests/scenarios/postgres/compose.yaml down
```

The fixture uses tmpfs, so stopping/removing its container discards its data.
Without `FLEDGLING_TEST_POSTGRES_URL`, only the offline planning checks run.

To add a scenario, create a folder under `tests/scenarios/postgres/` with
`target.schema.sql`, optional `current.schema.sql` (omit to start empty), and
`expected.up.sql`/`expected.down.sql` (both empty for a no-op). For a rejected
plan, replace the expected SQL pair with `error.txt`. Runner rejection fixtures
live under `_runner/` and supply `current.schema.sql`, one `invalid.up.sql` or
`invalid.down.sql`, and `error.txt`. No Rust registration is required.
Pull fixtures live under `_pull/` and supply `current.schema.sql` and optionally
either `error.txt` or a boolean `assert.sql` checked before and after replay.
