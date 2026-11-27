//! Applied history stored in the PostgreSQL database.

use std::cell::RefCell;

use postgres::GenericClient;

use super::PostgresError;
use crate::migrations::Ledger;

pub(super) const LEDGER_NAME: &str = "_fledgling_migrations";
const LEDGER: &str = "public._fledgling_migrations";

/// Ledger bound to a PostgreSQL client or transaction.
pub struct PostgresLedger<'a, C>(RefCell<&'a mut C>);

impl<'a, C: GenericClient> PostgresLedger<'a, C> {
    pub(super) fn new(client: &'a mut C) -> Self {
        Self(RefCell::new(client))
    }

    fn oid(&self) -> Result<Option<u32>, PostgresError> {
        let Some(row) = self.0.borrow_mut().query_opt(
            "SELECT c.oid, c.relkind = 'r' AND c.relpersistence = 'p' AND NOT c.relispartition,
                    ARRAY(SELECT a.attname::text || ':' ||
                                 pg_catalog.format_type(a.atttypid, a.atttypmod) || ':' ||
                                 a.attidentity::text || ':' || a.attnotnull::text
                          FROM pg_catalog.pg_attribute a
                          WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
                          ORDER BY a.attnum)
             FROM pg_catalog.pg_class c
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = 'public' AND c.relname = $1",
            &[&LEDGER_NAME],
        )?
        else {
            return Ok(None);
        };
        if !row.get::<_, bool>(1)
            || row.get::<_, Vec<String>>(2) != ["position:bigint:a:true", "id:numeric(20,0)::true"]
        {
            return Err(PostgresError::Invalid(format!(
                "incompatible {LEDGER_NAME} ledger format"
            )));
        }
        Ok(Some(row.get(0)))
    }

    pub(crate) fn ensure(&mut self) -> Result<(), PostgresError> {
        if self.oid()?.is_none() {
            self.0.get_mut().batch_execute(&format!(
                "CREATE TABLE {LEDGER} (
                    position BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                    id NUMERIC(20,0) NOT NULL UNIQUE CHECK (id BETWEEN 1 AND 18446744073709551615)
                )"
            ))?;
        }
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> Result<Option<(u32, Vec<u64>)>, PostgresError> {
        let Some(oid) = self.oid()? else {
            return Ok(None);
        };
        let ids = self
            .0
            .borrow_mut()
            .query(
                &format!("SELECT id::text FROM {LEDGER} ORDER BY position"),
                &[],
            )?
            .into_iter()
            .map(|row| {
                row.get::<_, &str>(0)
                    .parse::<u64>()
                    .ok()
                    .filter(|id| *id > 0)
                    .ok_or_else(|| {
                        PostgresError::Invalid(format!("invalid migration ID in {LEDGER_NAME}"))
                    })
            })
            .collect::<Result<_, _>>()?;
        Ok(Some((oid, ids)))
    }
}

impl<C: GenericClient> Ledger for PostgresLedger<'_, C> {
    type Error = PostgresError;

    fn list(&self) -> Result<Vec<u64>, Self::Error> {
        Ok(self.snapshot()?.map_or_else(Vec::new, |(_, ids)| ids))
    }

    fn commit(&mut self, id: u64) -> Result<(), Self::Error> {
        self.ensure()?;
        self.0.get_mut().execute(
            &format!("INSERT INTO {LEDGER} (id) VALUES ($1::text::numeric)"),
            &[&id.to_string()],
        )?;
        Ok(())
    }

    fn uncommit(&mut self, id: u64) -> Result<(), Self::Error> {
        if self.oid()?.is_some()
            && self.0.get_mut().execute(
                &format!(
                    "DELETE FROM {LEDGER}
                      WHERE position = (SELECT max(position) FROM {LEDGER})
                      AND id = $1::text::numeric"
                ),
                &[&id.to_string()],
            )? == 1
        {
            Ok(())
        } else {
            Err(PostgresError::Invalid(format!(
                "migration {id} is not the latest applied"
            )))
        }
    }
}
