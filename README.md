# Fledgling

An infrastructure-as-code tool for managing database schemas with versioned
migrations.

## Installation

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/kmiller96/fledgling/master/install.sh | bash
```

The installer places `fledgling` in `~/.local/bin`. Set `FLEDGLING_INSTALL_DIR`
to choose another location. Windows users can download the `.zip` binary from
the [latest GitHub Release](https://github.com/kmiller96/fledgling/releases/latest).

Alternatively, install from crates.io:

```sh
cargo install fledgling
```

Building from source requires a C compiler and libclang for the PostgreSQL SQL
parser. Linux also requires OpenSSL development libraries.

## Quickstart

You can quickly scaffold all the files you need by running the `init` command:

```sh
fledgling init
```

This will run an interactive installer to setup fledgling in your project.

Next, define your database schema in `schema.sql`. For example:

```sql
CREATE TABLE books (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL
);
```

### Migration workflow

Fledgling compares your target schema with the schema from your migration
history, not your live database.

- `fledgling generate` - this generates the database migration files for you.
- `fledgling migrate` - this applies the database migrations against the target.

Try it yourself!

```sh
fledgling generate  # Writes the migration to `./migrations/` by default
fledgling migrate
```

`generate` is an offline, checked additive-schema workflow: it replays the
migrations on disk and publishes a new Up/Down pair without connecting to the
target database. Migration IDs start at `1` and increment from the highest ID on
disk. The filename stem is the database's numeric ID padded to at least six
digits: `000001.up.sql` with an optional `000001.down.sql`, then `000002.up.sql`,
and so on. `fledgling status` displays IDs with the same padding.

Migration filenames must use positive numeric IDs. Older timestamp/name files
are not renumbered automatically. For an existing applied history, changing
the files alone does not change the IDs recorded in the database.

Generated migrations include both files. `fledgling rollback` runs the latest
applied migration's Down SQL and requires that file to exist.

Now, let's add a `author` column to `schema.sql`:

```sql
CREATE TABLE books (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    author TEXT
);
```

Preview the next migration without connecting to the database or writing files:

```bash
fledgling preview
```

`preview` prints the same Up and Down SQL that `generate` would write:

```sql
-- Up
ALTER TABLE "books" ADD COLUMN "author" TEXT;

-- Down
ALTER TABLE "books" DROP COLUMN "author";
```

### PostgreSQL

Select PostgreSQL in `fledgling init` or set it in `fledgling.toml`:

```toml
[fledgling]
dialect = "postgres"
database_url = "postgresql://localhost/my_app"
migrations = "migrations"
schema = "schema.sql"
```

Create the database before running `pull`, `status`, `migrate`, or `rollback`.
Override the URL with `--database` if needed. Keep config files containing
passwords out of version control.

`preview` and `generate` replay a PostgreSQL model offline. They support new
ordinary tables, named indexes, views, and appending one plain nullable column
per existing table. Unqualified names resolve to `public`; qualify objects in
other pre-existing schemas.
Unsupported DDL, including schema creation and changes to existing column
types, stops offline planning rather than being ignored. The planner also
rejects adding a column to a table with existing dependent views. The offline check
compares models in both directions; only the server can validate SQL semantics.
Handwritten migrations can run any server-supported SQL that fits in a
transaction and leaves the reserved ledger intact. Only run trusted SQL.

`pull` emits DDL for supported schemas, sequences, tables, indexes, and views.
It is not a full dump: it does not export data, roles, extensions, or other
prerequisites such as custom types and functions. Its output can include
statements outside the narrower offline planner subset.

For local verification, see the [PostgreSQL integration tests](tests/postgres/scenarios/README.md).

### Concurrent migrations

Run only one `fledgling migrate` or `fledgling rollback` at a time per database.
Each migration and its ledger update commit in one transaction, but Fledgling
does not coordinate overlapping runs. Use a single migration job per database.

## Architecture

`schema` models backend-defined objects. `planner::ChangeSet` describes logical
changes, not execution order. `Change::invert` does not guarantee reversibility.
Each backend lowers changes into ordered `migrations::Operation`s and renders
them as SQL.

`migrations` defines the Up/Down file boundary and the independent `Ledger`
contract. `driver::Driver` executes migration commands without
owning applied history. A database-backed ledger can share a transaction with
execution; an external ledger needs a separate coordination and recovery policy.

SQLite verifies both directions in a disposable SQLite database. PostgreSQL
verifies both directions in its offline model. The backends share the numeric
migration-file format, but each owns its driver, ledger, and workflows.

## Motivation

I have been a big fan for a long time of the 
[goose](https://github.com/pressly/goose) tool. Expressing your database 
migrations in plain SQL is a fantastic developer experience, especially for
developers comfortable in raw SQL like I am.

For my web project, however, I have enjoyed using tools like 
[drizzle](https://orm.drizzle.team/). Being able to define your database 
declaratively and having a program figure out the migrations makes for a very
comfortable developer experience. Having to figure out how to migrate your 
tables every time you make a change is a pain, and having drizzle do this for me
is very nice. 

I wanted to come up with a program which has that same drizzle developer 
experience but relies on plain SQL to get this done. Not every project of mine
uses TypeScript, so I want to use a tool better suited to my needs!

## Releases

See [release instructions](docs/releases.md).
