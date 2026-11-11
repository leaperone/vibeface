use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ColumnShape {
    type_name: String,
    notnull: bool,
    default_value: Option<String>,
    pk: u32,
}

type TableShape = BTreeMap<String, ColumnShape>;
type ApplicationShape = BTreeMap<String, TableShape>;

static EXPECTED_SHAPE: OnceLock<ApplicationShape> = OnceLock::new();

fn normalize_type_name(raw: &str) -> String {
    raw.trim().to_ascii_uppercase()
}

fn normalize_default(raw: Option<String>) -> Option<String> {
    raw.map(|value| value.trim().to_string())
}

fn application_shape(connection: &Connection) -> Result<ApplicationShape> {
    let mut tables = BTreeSet::new();
    let mut stmt = connection.prepare(
        "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let names = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for name in names {
        tables.insert(name?);
    }

    let mut shape = ApplicationShape::new();
    for table in tables {
        let mut columns = TableShape::new();
        let pragma = format!("PRAGMA table_info(\"{}\")", table.replace('"', "\"\""));
        let mut stmt = connection.prepare(&pragma)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, u32>(5)?,
            ))
        })?;
        for row in rows {
            let (name, type_name, notnull, default_value, pk) = row?;
            columns.insert(
                name,
                ColumnShape {
                    type_name: normalize_type_name(&type_name),
                    notnull: notnull != 0,
                    default_value: normalize_default(default_value),
                    pk,
                },
            );
        }
        shape.insert(table, columns);
    }
    Ok(shape)
}

fn expected_application_shape() -> Result<&'static ApplicationShape> {
    if let Some(shape) = EXPECTED_SHAPE.get() {
        return Ok(shape);
    }
    let connection = Connection::open_in_memory().context("build expected schema")?;
    connection
        .execute_batch(super::INITIAL_SCHEMA)
        .context("apply authoritative initial schema")?;
    let shape = application_shape(&connection)?;
    let _ = EXPECTED_SHAPE.set(shape);
    Ok(EXPECTED_SHAPE.get().expect("expected schema cache"))
}

/// Ensures every table/column required by the current runtime exists with compatible metadata.
/// Extra user tables are ignored. Column order is ignored.
pub(crate) fn validate_application_schema(connection: &Connection) -> Result<()> {
    let expected = expected_application_shape()?;
    let actual = application_shape(connection)?;
    let mut mismatches = Vec::new();

    for (table, expected_columns) in expected {
        let Some(actual_columns) = actual.get(table) else {
            mismatches.push(format!("{table}: missing table"));
            continue;
        };
        for (column, expected_column) in expected_columns {
            let Some(actual_column) = actual_columns.get(column) else {
                mismatches.push(format!("{table}.{column}: missing column"));
                continue;
            };
            if actual_column != expected_column {
                mismatches.push(format!(
                    "{table}.{column}: metadata does not match this Core"
                ));
            }
        }
    }

    if mismatches.is_empty() {
        return Ok(());
    }
    mismatches.sort();
    bail!(
        "data_schema_mismatch: database user_version matches the current data epoch but table/column metadata does not match this Core build. \
Keep a backup of the Node data directory and its identity. This database needs offline repair before this Core can start; do not wipe identity or change the data epoch.\n{}",
        mismatches.join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{INITIAL_SCHEMA, SCHEMA_VERSION, Store};

    fn init_current_db(connection: &Connection) -> rusqlite::Result<()> {
        connection.execute_batch(INITIAL_SCHEMA)?;
        connection.pragma_update(None, "user_version", SCHEMA_VERSION)
    }

    #[test]
    fn missing_current_columns_are_refused_without_mutating_database_bytes() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("vibeface-schema-guard-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let path = dir.join("node.sqlite3");
        {
            let db = Connection::open(&path)?;
            init_current_db(&db)?;
            db.execute_batch(
                "ALTER TABLE messages RENAME COLUMN target TO missing_target;
                 ALTER TABLE scheduler_jobs RENAME COLUMN scope TO missing_scope;
                 ALTER TABLE events RENAME COLUMN causal_clock TO missing_clock;",
            )?;
            db.execute(
                "INSERT INTO identity_meta(key,value) VALUES('device','test-device')",
                [],
            )?;
        }
        let before = std::fs::read(&path)?;

        let open_result = Store::open(&path);
        assert!(open_result.is_err());
        let message = open_result.err().expect("open error").to_string();
        assert!(message.contains("data_schema_mismatch"));
        assert!(message.contains("messages.target"));
        assert!(message.contains("scheduler_jobs.scope"));
        assert!(message.contains("events.causal_clock"));
        assert_eq!(std::fs::read(&path)?, before);

        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn current_schema_allows_extra_user_tables() -> Result<()> {
        let store = Store::open(":memory:")?;
        drop(store);

        let dir = std::env::temp_dir().join(format!(
            "vibeface-schema-guard-current-{}",
            uuid::Uuid::new_v4()
        ));
        crate::config::private_dir(&dir)?;
        let path = dir.join("node.sqlite3");
        {
            let db = Connection::open(&path)?;
            init_current_db(&db)?;
            db.execute("CREATE TABLE \"用户备注-1\" (note text NOT NULL)", [])?;
        }
        let store = Store::open(&path)?;
        let _version: u32 = store
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        drop(store);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
