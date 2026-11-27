# Fledgling

An infrastructure-as-code tool for managing database schemas with versioned
migrations.

## Installation

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/kmiller96/fledgling/main/install.sh | bash
```

The installer places `fledgling` in `~/.local/bin`. Set `FLEDGLING_INSTALL_DIR`
to choose another location. Windows users can download the `.zip` binary from
the [latest GitHub Release](https://github.com/kmiller96/fledgling/releases/latest).

Alternatively, install from crates.io:

```sh
cargo install fledgling
```

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
database. Migration IDs start at `1` and increment from the highest ID on
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

### Concurrent migrations

Run only one `fledgling migrate` or `fledgling rollback` at a time per database.
SQLite protects each migration transaction, but Fledgling does not coordinate
overlapping runs. For automated deployments, use a single migration job.

## Architecture

SQLite is the only supported database. `schema::Diff` is a vector of logical
object changes, not execution order. SQLite lowers those changes into ordered SQL.
`Change::invert` is a logical helper only; SQLite derives Down SQL from the
selected DDL.

The SQLite planner is offline and owns its internal schema model and additive
planner. Users write SQL, not a schema DSL. It parses SQL into that model,
renders its operations as SQLite DDL, and verifies Up and Down in SQLite.
Connection-bound execution commits each migration or rollback with its ledger
update in one transaction.

The Rust API exposes `pull`, `preview`, `generate`, `migrate`, `status`, and
`rollback`. Pass `Dialect::SQLite` and the paths required by the workflow.
Backend, schema, migration-file, and ledger types are internal. Applied
history is checked against the ordered migration IDs, not file contents.

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
