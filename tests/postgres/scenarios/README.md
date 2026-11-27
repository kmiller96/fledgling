# PostgreSQL acceptance test fixture

Run offline planner scenarios with `cargo test --test postgres`. For the full
suite, start a disposable PostgreSQL 17 instance with Docker Compose:

```sh
docker compose -f tests/postgres/scenarios/compose.yaml up -d --wait
FLEDGLING_TEST_POSTGRES_URL=postgresql://fledgling:fledgling@127.0.0.1:55432/fledgling_test \
cargo test --test postgres -- --include-ignored
docker compose -f tests/postgres/scenarios/compose.yaml down
```

The fixture uses tmpfs, so stopping/removing its container discards its data.
For an existing test server, point `FLEDGLING_TEST_POSTGRES_URL` at a maintenance
database using a role with `CREATEDB`. Live tests create and drop their own
databases; they do not execute migration SQL against the maintenance database.
Live tests are marked ignored by default so missing configuration cannot look
like a passing live test. CI supplies its own PostgreSQL 17 instance.

To add a scenario, create a folder here with
`target.schema.sql`, optional `current.schema.sql` (omit to start empty), and
`expected.up.sql`/`expected.down.sql` (both empty for a no-op). For a rejected
plan, replace the expected SQL pair with `error.txt`. Runner rejection fixtures
live under `_runner/` and supply `current.schema.sql`, one `invalid.up.sql` or
`invalid.down.sql`, and `error.txt`. No Rust registration is required.
Pull fixtures live under `_pull/` and supply `current.schema.sql` and optionally
either `error.txt` or a boolean `assert.sql` checked before and after replay.
