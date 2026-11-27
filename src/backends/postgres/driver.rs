use crate::{
    driver::Driver,
    migrations::{AppliedMigration, Ledger, Migration},
};
use native_tls::TlsConnector;
use postgres::{Client, Transaction};
use postgres_native_tls::MakeTlsConnector;
use std::cell::RefCell;
use std::{error::Error, fmt};

const LEDGER: &str = "_fledgling_migrations";

#[derive(Debug)]
pub enum PostgresError {
    Database(postgres::Error),
    Invalid(String),
}
impl fmt::Display for PostgresError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(e) => e.fmt(f),
            Self::Invalid(e) => f.write_str(e),
        }
    }
}
impl Error for PostgresError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        if let Self::Database(e) = self {
            Some(e)
        } else {
            None
        }
    }
}
impl From<postgres::Error> for PostgresError {
    fn from(e: postgres::Error) -> Self {
        Self::Database(e)
    }
}

pub struct PostgresDriver {
    client: RefCell<Client>,
    read_only: bool,
}
impl PostgresDriver {
    pub fn connect(url: &str) -> Result<Self, PostgresError> {
        Self::open(url, false)
    }
    pub fn open_read_only(url: &str) -> Result<Self, PostgresError> {
        Self::open(url, true)
    }
    fn open(url: &str, read_only: bool) -> Result<Self, PostgresError> {
        let tls = TlsConnector::builder()
            .build()
            .map_err(|e| PostgresError::Invalid(format!("TLS setup: {e}")))?;
        let mut client =
            Client::connect(url, MakeTlsConnector::new(tls)).map_err(PostgresError::Database)?;
        if read_only {
            client.batch_execute("SET default_transaction_read_only = on")?;
        }
        Ok(Self {
            client: RefCell::new(client),
            read_only,
        })
    }
    pub fn pull(&self) -> Result<String, PostgresError> {
        let mut client = self.client.borrow_mut();
        let mut tx = client
            .build_transaction()
            .isolation_level(postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()?;
        tx.batch_execute("SET LOCAL search_path = pg_catalog")?;
        let objects = snapshot(&mut tx)?;
        tx.rollback()?;
        Ok(render_objects(&objects))
    }

    fn run(&mut self, migration: &Migration, up: bool) -> Result<(), PostgresError> {
        if self.read_only {
            return Err(PostgresError::Invalid(
                "database connection is read-only".into(),
            ));
        }
        let artifact = if up {
            &migration.up
        } else {
            migration
                .down
                .as_ref()
                .ok_or_else(|| PostgresError::Invalid("migration has no Down SQL".into()))?
        };
        validate_batch(&artifact.sql)?;
        let mut client = self.client.borrow_mut();
        let mut tx = client.transaction()?;
        if up {
            ensure(&mut tx)?;
            check_unapplied(&mut tx, migration.id)?;
        } else {
            check_latest(&mut tx, migration.id)?;
        }
        tx.batch_execute(&artifact.sql)?;
        if up {
            append(&mut tx, migration.id)?;
        } else {
            remove(&mut tx, migration.id)?;
        }
        tx.commit()?;
        Ok(())
    }
}

type Objects = std::collections::BTreeMap<(String, String), String>;
fn snapshot(tx: &mut Transaction<'_>) -> Result<Objects, PostgresError> {
    let mut out = std::collections::BTreeMap::new();
    if let Some(row) = tx.query_opt(
        "SELECT format('%I.%I', n.nspname, c.relname)
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'
           AND (c.relkind NOT IN ('r','i','v','S')
                OR c.relispartition OR c.relhassubclass OR c.relpersistence <> 'p'
                OR c.relrowsecurity OR c.relforcerowsecurity
                OR EXISTS (SELECT 1 FROM pg_trigger t WHERE t.tgrelid = c.oid AND NOT t.tgisinternal)
                OR EXISTS (SELECT 1 FROM pg_attribute a
                           WHERE a.attrelid = c.oid AND NOT a.attisdropped AND a.attgenerated <> ''))
         ORDER BY n.nspname,c.relname LIMIT 1",
        &[],
    )? {
        return Err(PostgresError::Invalid(format!(
            "pull cannot represent PostgreSQL relation {}",
            row.get::<_, String>(0)
        )));
    }
    let schemas = tx.query(
        "SELECT nspname FROM pg_namespace WHERE nspname NOT IN ('public','pg_catalog','information_schema') AND nspname NOT LIKE 'pg\\_%' ORDER BY nspname",
        &[],
    )?;
    for row in schemas {
        let schema: String = row.get(0);
        out.insert(
            ("schema".into(), schema.clone()),
            format!("CREATE SCHEMA {};", quote(&schema)),
        );
    }
    // Sequences must exist before table defaults that call nextval(); ownership
    // is restored after the owning table has been created.
    let sequences = tx.query(
        "SELECT n.nspname,c.relname,format_type(s.seqtypid,NULL),s.seqstart,s.seqincrement,s.seqmin,s.seqmax,s.seqcache,s.seqcycle,\
         (SELECT format('%I.%I.%I', tn.nspname, tc.relname, a.attname) FROM pg_depend d JOIN pg_class tc ON tc.oid=d.refobjid JOIN pg_namespace tn ON tn.oid=tc.relnamespace JOIN pg_attribute a ON a.attrelid=tc.oid AND a.attnum=d.refobjsubid WHERE d.classid='pg_class'::regclass AND d.refclassid='pg_class'::regclass AND d.objid=c.oid AND d.deptype='a' LIMIT 1) \
         FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_sequence s ON s.seqrelid=c.oid \
         WHERE c.relkind='S' AND n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%' \
         AND NOT EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid='pg_class'::regclass AND d.objid=c.oid AND d.deptype='i') ORDER BY n.nspname,c.relname",
        &[],
    )?;
    for r in sequences {
        let schema: String = r.get(0);
        let name: String = r.get(1);
        let seqtype: String = r.get(2);
        let increment: i64 = r.get(4);
        let min: i64 = r.get(5);
        let max: i64 = r.get(6);
        let cache: i64 = r.get(7);
        let cycle: bool = r.get(8);
        let start: i64 = r.get(3);
        let owner: Option<String> = r.get(9);
        out.insert(("sequence".into(), format!("{schema}.{name}")),
            format!("CREATE SEQUENCE {}.{} AS {seqtype} INCREMENT BY {increment} MINVALUE {min} MAXVALUE {max} START WITH {start} CACHE {cache} {};",
                quote(&schema), quote(&name), if cycle { "CYCLE" } else { "NO CYCLE" }));
        if let Some(owner) = owner {
            out.insert(
                ("ownership".into(), format!("{schema}.{name}")),
                format!(
                    "ALTER SEQUENCE {}.{} OWNED BY {owner};",
                    quote(&schema),
                    quote(&name)
                ),
            );
        }
    }
    let tables = tx.query("SELECT n.nspname,c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg_toast%' AND c.relkind IN ('r','p') AND c.relname <> $1 ORDER BY n.nspname,c.relname",&[&LEDGER])?;
    for row in tables {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let cols=tx.query("SELECT a.attname,format_type(a.atttypid,a.atttypmod),a.attnotnull,pg_get_expr(d.adbin,d.adrelid),a.attidentity::text FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace LEFT JOIN pg_attrdef d ON d.adrelid=c.oid AND d.adnum=a.attnum WHERE n.nspname=$1 AND c.relname=$2 AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum",&[&schema,&table])?;
        let mut defs = Vec::new();
        for c in cols {
            let mut text = format!(
                "{} {}",
                quote(&c.get::<_, String>(0)),
                c.get::<_, String>(1)
            );
            let identity: String = c.get(4);
            if !identity.is_empty() {
                text.push_str(if identity == "a" {
                    " GENERATED ALWAYS AS IDENTITY"
                } else {
                    " GENERATED BY DEFAULT AS IDENTITY"
                });
            }
            if let Some(default) = c.get::<_, Option<String>>(3)
                && identity.is_empty()
            {
                text.push_str(&format!(" DEFAULT {default}"));
            }
            if c.get::<_, bool>(2) {
                text.push_str(" NOT NULL");
            }
            defs.push(text);
        }
        let cons=tx.query("SELECT conname, pg_get_constraintdef(oid), contype::text FROM pg_constraint WHERE conrelid=format('%I.%I',$1::text,$2::text)::regclass ORDER BY contype,conname",&[&schema,&table])?;
        for c in cons {
            let definition = format!(
                "CONSTRAINT {} {}",
                quote(&c.get::<_, String>(0)),
                c.get::<_, String>(1)
            );
            if c.get::<_, String>(2) == "f" {
                out.insert(
                    (
                        "foreign key".into(),
                        definition.clone() + &quote(&schema) + &quote(&table),
                    ),
                    format!(
                        "ALTER TABLE {}.{} ADD {definition};",
                        quote(&schema),
                        quote(&table)
                    ),
                );
            } else {
                defs.push(definition);
            }
        }
        out.insert(
            ("table".into(), format!("{schema}.{table}")),
            format!(
                "CREATE TABLE {}.{} ({});",
                quote(&schema),
                quote(&table),
                defs.join(", ")
            ),
        );
    }
    let indexes=tx.query("SELECT n.nspname,c.relname,pg_get_indexdef(c.oid) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_index i ON i.indexrelid=c.oid JOIN pg_class tbl ON tbl.oid=i.indrelid WHERE c.relkind='i' AND n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg\\_%' AND tbl.relname <> $1 AND NOT EXISTS (SELECT 1 FROM pg_constraint con WHERE con.conindid=c.oid AND con.conrelid=i.indrelid) ORDER BY n.nspname,c.relname",&[&LEDGER])?;
    for r in indexes {
        out.insert(
            (
                "index".into(),
                format!("{}.{}", r.get::<_, String>(0), r.get::<_, String>(1)),
            ),
            format!("{};", r.get::<_, String>(2)),
        );
    }
    let views = tx.query(
        "WITH RECURSIVE views AS (
            SELECT c.oid,n.nspname,c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE c.relkind='v' AND n.nspname <> 'information_schema' AND n.nspname NOT LIKE 'pg\\_%'
         ), deps AS (
            SELECT DISTINCT v.oid AS source, d.refobjid AS target FROM views v
            JOIN pg_rewrite r ON r.ev_class=v.oid
            JOIN pg_depend d ON d.classid='pg_rewrite'::regclass AND d.objid=r.oid
                AND d.refclassid='pg_class'::regclass
            JOIN views target ON target.oid=d.refobjid WHERE target.oid<>v.oid
         ), walk(source,target,depth) AS (
            SELECT oid,oid,0 FROM views
            UNION
            SELECT walk.source,deps.target,walk.depth+1 FROM walk JOIN deps ON deps.source=walk.target
            WHERE walk.depth < (SELECT count(*) FROM views)
         )
         SELECT v.nspname,v.relname,pg_get_viewdef(v.oid),max(w.depth) >= (SELECT count(*) FROM views)
         FROM views v JOIN walk w ON w.source=v.oid GROUP BY v.oid,v.nspname,v.relname
         ORDER BY max(w.depth),v.nspname,v.relname",
        &[],
    )?;
    for (position, r) in views.into_iter().enumerate() {
        let s: String = r.get(0);
        let n: String = r.get(1);
        let def: String = r.get(2);
        if r.get::<_, bool>(3) {
            return Err(PostgresError::Invalid(
                "pull cannot order cyclic PostgreSQL views".into(),
            ));
        }
        out.insert(
            ("view".into(), format!("{position:020}")),
            format!(
                "CREATE VIEW {}.{} AS {};",
                quote(&s),
                quote(&n),
                def.trim_end_matches(';')
            ),
        );
    }
    Ok(out)
}
fn render_objects(objects: &Objects) -> String {
    let mut values = objects.iter().collect::<Vec<_>>();
    values.sort_by_key(|((kind, _), _)| match kind.as_str() {
        "schema" => 0,
        "sequence" => 1,
        "table" => 2,
        "ownership" => 3,
        "index" => 4,
        "foreign key" => 5,
        "view" => 6,
        _ => 4,
    });
    values
        .into_iter()
        .map(|(_, ddl)| format!("{ddl}\n"))
        .collect()
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
fn validate_batch(sql: &str) -> Result<(), PostgresError> {
    use pg_query::{NodeEnum, NodeRef};
    let parsed = pg_query::parse(sql)
        .map_err(|e| PostgresError::Invalid(format!("invalid migration SQL: {e}")))?;
    for (node, ..) in parsed.protobuf.nodes() {
        if matches!(node, NodeRef::TransactionStmt(_)) {
            return Err(PostgresError::Invalid(
                "explicit transaction control is not allowed in migrations".into(),
            ));
        }
        let reserved = match node {
            NodeRef::RangeVar(r) => r.relname == LEDGER,
            NodeRef::RenameStmt(r) => r.newname == LEDGER || r.subname == LEDGER,
            NodeRef::DropStmt(s) => s.objects.iter().any(|object| match object.node.as_ref() {
                Some(NodeEnum::String(name)) => name.sval == LEDGER,
                Some(NodeEnum::List(list)) => list.items.iter().any(|item| {
                    matches!(item.node.as_ref(), Some(NodeEnum::String(name)) if name.sval == LEDGER)
                }),
                _ => false,
            }),
            _ => false,
        };
        if reserved {
            return Err(PostgresError::Invalid(
                "migration may not modify the reserved _fledgling_migrations ledger".into(),
            ));
        }
    }
    Ok(())
}
fn relation_exists(tx: &mut Transaction<'_>) -> Result<bool, PostgresError> {
    Ok(tx
        .query_one(
            "SELECT to_regclass($1) IS NOT NULL",
            &[&format!("public.{LEDGER}")],
        )?
        .get(0))
}
fn ensure(tx: &mut Transaction<'_>) -> Result<(), PostgresError> {
    if !relation_exists(tx)? {
        tx.batch_execute(&format!("CREATE TABLE public.{} (position BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, id NUMERIC(20,0) NOT NULL UNIQUE CHECK (id > 0 AND id <= 18446744073709551615))", quote(LEDGER)))?;
    } else {
        check_format(tx)?;
    }
    Ok(())
}
fn check_format(tx: &mut Transaction<'_>) -> Result<(), PostgresError> {
    let relkind: Option<String> = tx
        .query_opt(
            "SELECT c.relkind::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relname=$1",
            &[&LEDGER],
        )?
        .map(|r| r.get(0));
    let rows=tx.query("SELECT column_name,data_type,is_identity FROM information_schema.columns WHERE table_schema='public' AND table_name=$1 ORDER BY ordinal_position",&[&LEDGER])?;
    let cols: Vec<(String, String, String)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    if relkind.as_deref() != Some("r")
        || cols
            != [
                ("position".into(), "bigint".into(), "YES".into()),
                ("id".into(), "numeric".into(), "NO".into()),
            ]
    {
        return Err(PostgresError::Invalid(
            "incompatible _fledgling_migrations ledger format".into(),
        ));
    }
    Ok(())
}
fn check_unapplied(tx: &mut Transaction<'_>, id: u64) -> Result<(), PostgresError> {
    if tx
        .query_opt(
            &format!(
                "SELECT 1 FROM public.{} WHERE id=$1::text::numeric",
                quote(LEDGER)
            ),
            &[&id.to_string()],
        )?
        .is_some()
    {
        return Err(PostgresError::Invalid("migration already applied".into()));
    }
    Ok(())
}
fn check_latest(tx: &mut Transaction<'_>, id: u64) -> Result<(), PostgresError> {
    let latest = if relation_exists(tx)? {
        check_format(tx)?;
        tx.query_opt(
            &format!(
                "SELECT id::text FROM public.{} ORDER BY position DESC LIMIT 1 FOR UPDATE",
                quote(LEDGER)
            ),
            &[],
        )?
        .map(|r| r.get::<_, String>(0))
    } else {
        None
    };
    if latest.as_deref() != Some(&id.to_string()) {
        return Err(PostgresError::Invalid(
            "migration is not the latest applied".into(),
        ));
    }
    Ok(())
}
fn append(tx: &mut Transaction<'_>, id: u64) -> Result<(), PostgresError> {
    tx.execute(
        &format!(
            "INSERT INTO public.{} (id) VALUES ($1::text::numeric)",
            quote(LEDGER)
        ),
        &[&id.to_string()],
    )?;
    Ok(())
}
fn remove(tx: &mut Transaction<'_>, id: u64) -> Result<(), PostgresError> {
    tx.execute(
        &format!(
            "DELETE FROM public.{} WHERE id=$1::text::numeric",
            quote(LEDGER)
        ),
        &[&id.to_string()],
    )?;
    Ok(())
}
impl Ledger for PostgresDriver {
    type Error = PostgresError;
    fn list(&self) -> Result<Vec<AppliedMigration>, Self::Error> {
        let mut client = self.client.borrow_mut();
        let mut tx = client.transaction()?;
        if !relation_exists(&mut tx)? {
            tx.rollback()?;
            return Ok(vec![]);
        }
        check_format(&mut tx)?;
        let rows = tx.query(
            &format!(
                "SELECT id::text FROM public.{} ORDER BY position",
                quote(LEDGER)
            ),
            &[],
        )?;
        let records = rows
            .into_iter()
            .map(|r| {
                let s: String = r.get(0);
                let id = s
                    .parse::<u64>()
                    .map_err(|_| PostgresError::Invalid("invalid ID in migration ledger".into()))?;
                if id.to_string() != s {
                    return Err(PostgresError::Invalid(
                        "invalid ID in migration ledger".into(),
                    ));
                }
                Ok(AppliedMigration { id })
            })
            .collect::<Result<Vec<_>, _>>()?;
        tx.commit()?;
        Ok(records)
    }
    fn commit(&mut self, m: AppliedMigration) -> Result<(), Self::Error> {
        let mut tx = self.client.get_mut().transaction()?;
        ensure(&mut tx)?;
        check_unapplied(&mut tx, m.id)?;
        append(&mut tx, m.id)?;
        tx.commit()?;
        Ok(())
    }
    fn revert(&mut self, id: u64) -> Result<(), Self::Error> {
        let mut tx = self.client.get_mut().transaction()?;
        check_latest(&mut tx, id)?;
        remove(&mut tx, id)?;
        tx.commit()?;
        Ok(())
    }
}
impl Driver for PostgresDriver {
    fn apply(&mut self, m: &Migration) -> Result<(), Self::Error> {
        self.run(m, true)
    }
    fn revert(&mut self, m: &Migration) -> Result<(), Self::Error> {
        self.run(m, false)
    }
}

#[cfg(test)]
mod tests {
    use super::validate_batch;

    #[test]
    fn transaction_control_is_rejected_by_the_postgresql_parser() {
        for sql in [
            "CREATE TABLE x (id int); COMMIT",
            "/* statement */ END",
            "BEGIN TRANSACTION",
            "ROLLBACK TO SAVEPOINT nested",
            "SAVEPOINT nested",
        ] {
            assert!(validate_batch(sql).is_err(), "{sql}");
        }
        assert!(validate_batch("SELECT 'COMMIT'; -- END").is_ok());
        assert!(validate_batch("CREATE TABLE x (id int)").is_ok());
    }

    #[test]
    fn direct_ledger_mutations_are_rejected() {
        for sql in [
            "DROP TABLE _fledgling_migrations",
            "ALTER TABLE public._fledgling_migrations ADD COLUMN x int",
            "DELETE FROM public._fledgling_migrations",
            "TRUNCATE TABLE public._fledgling_migrations",
            "ALTER TABLE x RENAME TO _fledgling_migrations",
        ] {
            assert!(validate_batch(sql).is_err(), "{sql}");
        }
    }
}
