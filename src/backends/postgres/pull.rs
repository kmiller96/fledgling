//! Render the supported PostgreSQL catalog objects in dependency order.

use postgres::Transaction;

use super::{PostgresError, ledger::LEDGER_NAME, objects::invalid, quote};

pub(super) fn schema_sql(tx: &mut Transaction<'_>) -> Result<String, PostgresError> {
    let mut schemas = Vec::new();
    let mut sequences = Vec::new();
    let mut tables = Vec::new();
    let mut ownership = Vec::new();
    let mut foreign_keys = Vec::new();
    let mut indexes = Vec::new();
    let mut views = Vec::new();

    for row in tx.query(
        "SELECT nspname::text FROM pg_namespace
         WHERE nspname !~ '^pg_' AND nspname NOT IN ('public', 'information_schema')
         ORDER BY nspname",
        &[],
    )? {
        schemas.push(format!("CREATE SCHEMA {};", quote(row.get(0))));
    }

    // Identity sequences are rendered as part of their columns. Ledger
    // relation-owned indexes and sequences are omitted along with the ledger.
    for row in tx.query(
        "WITH ledger AS (SELECT to_regclass($1)::oid AS oid)
         SELECT c.oid, format('%I.%I', n.nspname, c.relname), c.relkind::text,
                c.relpersistence = 'p' AND NOT (
                    c.relispartition OR c.relhassubclass OR c.relrowsecurity OR c.relforcerowsecurity
                    OR c.reloptions IS NOT NULL OR c.reltablespace <> 0 AND c.relkind = 'r'
                    OR EXISTS (SELECT FROM pg_inherits WHERE inhrelid = c.oid)
                    OR EXISTS (SELECT FROM pg_trigger WHERE tgrelid = c.oid AND NOT tgisinternal)
                    OR EXISTS (SELECT FROM pg_rewrite WHERE ev_class = c.oid AND rulename <> '_RETURN')
                )
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         CROSS JOIN ledger
         WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema'
           AND c.oid IS DISTINCT FROM ledger.oid
           AND NOT EXISTS (SELECT FROM pg_index WHERE indexrelid = c.oid AND indrelid = ledger.oid)
           AND NOT EXISTS (
               SELECT FROM pg_depend d WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid
               AND d.refclassid = 'pg_class'::regclass AND d.deptype IN ('a', 'i')
               AND c.relkind = 'S' AND (d.deptype = 'i' OR d.refobjid = ledger.oid))
         ORDER BY n.nspname, c.relname",
        &[&format!("public.{LEDGER_NAME}")],
    )? {
        let oid: u32 = row.get(0);
        let name: &str = row.get(1);
        let kind: &str = row.get(2);
        let supported: bool = row.get(3);
        if !supported {
            return Err(invalid(format!("pull cannot represent PostgreSQL relation {name}")));
        }
        match kind {
            "r" => {
                let (table, keys) = table_sql(tx, oid, name)?;
                tables.push(table);
                foreign_keys.extend(keys);
            }
            "i" => {
                if let Some(row) = tx.query_opt(
                    "SELECT pg_get_indexdef($1) WHERE NOT EXISTS (
                         SELECT FROM pg_constraint WHERE conindid = $1 AND contype IN ('p', 'u', 'x')
                     )", &[&oid],
                )? {
                    indexes.push(format!("{};", row.get::<_, &str>(0)));
                }
            }
            "S" => {
                let row = tx.query_one(
                    "SELECT format_type(s.seqtypid, NULL),
                            format('INCREMENT BY %s MINVALUE %s MAXVALUE %s START WITH %s CACHE %s %s',
                                s.seqincrement, s.seqmin, s.seqmax, s.seqstart, s.seqcache,
                                CASE WHEN s.seqcycle THEN 'CYCLE' ELSE 'NO CYCLE' END),
                            (SELECT format('%I.%I.%I', n.nspname, c.relname, a.attname)
                             FROM pg_depend d JOIN pg_class c ON c.oid = d.refobjid
                             JOIN pg_namespace n ON n.oid = c.relnamespace
                             JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = d.refobjsubid
                             WHERE d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass
                               AND d.objid = s.seqrelid AND d.deptype = 'a')
                     FROM pg_sequence s WHERE s.seqrelid = $1",
                    &[&oid],
                )?;
                sequences.push(format!("CREATE SEQUENCE {name} AS {} {};",
                    row.get::<_, &str>(0), row.get::<_, &str>(1)));
                if let Some(owner) = row.get::<_, Option<&str>>(2) {
                    ownership.push(format!("ALTER SEQUENCE {name} OWNED BY {owner};"));
                }
            }
            "v" => {
                let row = tx.query_one(
                    "SELECT pg_get_viewdef($1::oid), ARRAY(
                         SELECT DISTINCT d.refobjid FROM pg_depend d
                         JOIN pg_rewrite r ON r.oid = d.objid
                         JOIN pg_class c ON c.oid = d.refobjid
                         WHERE r.ev_class = $1 AND c.relkind = 'v' AND d.refobjid <> $1
                         AND d.classid = 'pg_rewrite'::regclass AND d.refclassid = 'pg_class'::regclass
                     )", &[&oid],
                )?;
                views.push((oid, row.get::<_, Vec<u32>>(1), format!("CREATE VIEW {name} AS {};",
                    row.get::<_, &str>(0).trim_end().trim_end_matches(';'))));
            }
            _ => return Err(invalid(format!("pull cannot represent PostgreSQL relation {name}"))),
        }
    }

    let mut ordered_views = Vec::new();
    while !views.is_empty() {
        let pos = views
            .iter()
            .position(|(_, deps, _)| {
                !deps
                    .iter()
                    .any(|dep| views.iter().any(|(oid, _, _)| oid == dep))
            })
            .ok_or_else(|| invalid("pull cannot order cyclic PostgreSQL views"))?;
        ordered_views.push(views.remove(pos).2);
    }
    Ok(schemas
        .into_iter()
        .chain(sequences)
        .chain(tables)
        .chain(ownership)
        .chain(indexes)
        .chain(foreign_keys)
        .chain(ordered_views)
        .map(|sql| format!("{sql}\n"))
        .collect())
}

fn table_sql(
    tx: &mut Transaction<'_>,
    oid: u32,
    name: &str,
) -> Result<(String, Vec<String>), PostgresError> {
    let mut columns = Vec::new();
    for row in tx.query(
        "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), a.attnotnull,
                pg_get_expr(d.adbin, d.adrelid), a.attidentity::text, a.attgenerated::text,
                CASE WHEN a.attcollation <> t.typcollation THEN format('%I.%I', n.nspname, c.collname) END,
                (SELECT format('SEQUENCE NAME %I.%I INCREMENT BY %s MINVALUE %s MAXVALUE %s START WITH %s CACHE %s %s',
                     sn.nspname, sc.relname, s.seqincrement, s.seqmin, s.seqmax, s.seqstart, s.seqcache,
                     CASE WHEN s.seqcycle THEN 'CYCLE' ELSE 'NO CYCLE' END)
                 FROM pg_depend dep JOIN pg_sequence s ON s.seqrelid = dep.objid
                 JOIN pg_class sc ON sc.oid = s.seqrelid JOIN pg_namespace sn ON sn.oid = sc.relnamespace
                 WHERE dep.classid = 'pg_class'::regclass AND dep.refclassid = 'pg_class'::regclass
                   AND dep.refobjid = a.attrelid AND dep.refobjsubid = a.attnum AND dep.deptype = 'i')
         FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
         LEFT JOIN pg_collation c ON c.oid = a.attcollation LEFT JOIN pg_namespace n ON n.oid = c.collnamespace
         WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
        &[&oid],
    )? {
        let column: &str = row.get(0);
        if !row.get::<_, &str>(5).is_empty() {
            return Err(invalid(format!("pull cannot represent generated column {name}.{}", quote(column))));
        }
        let mut sql = format!("{} {}", quote(column), row.get::<_, &str>(1));
        if let Some(collation) = row.get::<_, Option<&str>>(6) {
            sql.push_str(&format!(" COLLATE {collation}"));
        }
        match row.get::<_, &str>(4) {
            "" => {
                if let Some(default) = row.get::<_, Option<&str>>(3) {
                    sql.push_str(&format!(" DEFAULT {default}"));
                }
            }
            identity => {
                let mode = if identity == "a" { "ALWAYS" } else { "BY DEFAULT" };
                let options = row.get::<_, Option<&str>>(7)
                    .ok_or_else(|| invalid(format!("missing identity sequence for {name}.{}", quote(column))))?;
                sql.push_str(&format!(" GENERATED {mode} AS IDENTITY ({options})"));
            }
        }
        if row.get::<_, bool>(2) {
            sql.push_str(" NOT NULL");
        }
        columns.push(sql);
    }
    let mut foreign_keys = Vec::new();
    for row in tx.query(
        "SELECT conname::text, contype::text, pg_get_constraintdef(oid) FROM pg_constraint
         WHERE conrelid = $1 ORDER BY conname",
        &[&oid],
    )? {
        let kind: &str = row.get(1);
        // PostgreSQL 18 exposes NOT NULL constraints in pg_constraint. The
        // column definitions above already account for ordinary NOT NULL.
        if kind == "n" {
            continue;
        }
        let constraint = format!("CONSTRAINT {} {}", quote(row.get(0)), row.get::<_, &str>(2));
        if kind == "f" {
            foreign_keys.push(format!("ALTER TABLE {name} ADD {constraint};"));
        } else {
            columns.push(constraint);
        }
    }
    Ok((
        format!("CREATE TABLE {name} (\n    {}\n);", columns.join(",\n    ")),
        foreign_keys,
    ))
}
