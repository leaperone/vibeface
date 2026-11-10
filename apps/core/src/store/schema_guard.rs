use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension as _};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ColumnShape {
    type_name: String,
    notnull: bool,
    default_value: Option<String>,
    pk: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IndexShape {
    unique: bool,
    origin: String,
    partial: bool,
    columns: Vec<IndexColumn>,
    definition: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IndexColumn {
    kind: i64,
    name: Option<String>,
    descending: bool,
    collation: String,
    key: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ForeignKeyColumn {
    sequence: u32,
    table: String,
    from: String,
    to: Option<String>,
    on_update: String,
    on_delete: String,
    matching: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TableShape {
    columns: BTreeMap<String, ColumnShape>,
    indexes: BTreeMap<String, IndexShape>,
    foreign_keys: Vec<Vec<ForeignKeyColumn>>,
    checks: Vec<Vec<String>>,
    deferred_foreign_keys: bool,
}
type ApplicationShape = BTreeMap<String, TableShape>;

static EXPECTED_SHAPE: OnceLock<ApplicationShape> = OnceLock::new();

// These objects contain only derived query data. Missing objects may be rebuilt after
// validating every other application constraint; conflicting definitions are refused.
const DERIVED_TABLES: &[&str] = &[
    "conversation_setting_writes",
    "memory_history_operations",
    "memory_winner_chain",
    "conversation_history_revisions",
    "workspace_tasks_lookup",
    "workspace_assignments_lookup",
];
const DERIVED_INDEXES: &[(&str, &str)] = &[
    ("work_runs", "work_runs_reconcile"),
    ("messages", "messages_history"),
    ("messages", "messages_replies_status"),
    ("messages", "messages_bot_sender"),
    ("works", "works_history"),
    ("bot_changes", "bot_changes_history"),
    ("questions", "questions_history"),
    ("session_owners", "session_owners_history"),
    ("scheduler_jobs", "scheduler_jobs_history"),
    ("scheduler_jobs", "scheduler_jobs_main_history"),
    ("scheduler_jobs", "scheduler_jobs_hold"),
    ("scheduler_jobs", "scheduler_jobs_context"),
    ("pending_checks", "pending_checks_history"),
    ("pending_checks", "pending_checks_scope"),
    ("scheduler_sessions", "scheduler_sessions_active"),
    ("work_stops", "work_stops_conversation"),
    ("work_dispatches", "work_dispatches_conversation"),
    ("conversation_setting_writes", "conversation_setting_observed"),
    ("memory_winner_chain", "memory_winner_depth"),
    ("memory_history_operations", "memory_history_conflicts"),
    ("schedules", "schedules_history"),
    ("memories", "memories_history"),
    ("skills", "skills_history"),
    ("workspace_assignments_lookup", "workspace_assignments_work"),
];

// The new covering indexes or existing primary keys already serve these prefixes.
// Only remove the exact former definitions; preserve any user-modified index.
const SUPERSEDED_INDEXES: &[(&str, &str)] = &[
    ("messages_order", "CREATE INDEX messages_order ON messages(network_id,conversation_id,rank)"),
    ("memories_conversation", "CREATE INDEX memories_conversation ON memories(network_id,conversation_id)"),
    ("bot_changes_order", "CREATE INDEX bot_changes_order ON bot_changes(network_id,conversation_id,rank)"),
    ("questions_conversation", "CREATE INDEX questions_conversation ON questions(network_id,conversation_id,rank)"),
];

fn normalize_type_name(raw: &str) -> String {
    raw.trim().to_ascii_uppercase()
}

fn normalize_default(raw: Option<String>) -> Option<String> {
    raw.map(|value| value.trim().to_string())
}

// Tokenize metadata SQL so formatting and identifier quoting do not change the signature.
// String literals retain their exact bytes because they affect CHECK and partial predicates.
fn sql_tokens(sql: &str) -> Vec<String> {
    let chars: Vec<_> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() || c == ';' {
            i += 1;
            continue;
        }
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            continue;
        }
        if matches!(c, '\'' | '"' | '`' | '[') {
            let end = if c == '[' { ']' } else { c };
            let mut value = String::new();
            i += 1;
            while i < chars.len() {
                if chars[i] == end {
                    i += 1;
                    if c != '[' && chars.get(i) == Some(&end) {
                        value.push(end);
                        i += 1;
                        continue;
                    }
                    break;
                }
                value.push(chars[i]);
                i += 1;
            }
            tokens.push(if c == '\'' {
                format!("'{}'", value.replace('\'', "''"))
            } else {
                value.to_ascii_lowercase()
            });
        } else if c.is_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            tokens.push(
                chars[start..i]
                    .iter()
                    .collect::<String>()
                    .to_ascii_lowercase(),
            );
        } else {
            tokens.push(c.to_string());
            i += 1;
        }
    }
    tokens
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
        let mut columns = BTreeMap::new();
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
        let sql: String = connection.query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
            [&table],
            |row| row.get(0),
        )?;
        let tokens = sql_tokens(&sql);
        let mut checks = Vec::new();
        for (offset, token) in tokens.iter().enumerate() {
            if token != "check" || tokens.get(offset + 1).map(String::as_str) != Some("(") {
                continue;
            }
            let mut depth = 0;
            let mut expression = Vec::new();
            for token in &tokens[offset + 1..] {
                if token == "(" {
                    depth += 1;
                }
                if token == ")" {
                    depth -= 1;
                }
                expression.push(token.clone());
                if depth == 0 {
                    break;
                }
            }
            checks.push(expression);
        }
        checks.sort();
        let mut foreign_keys = BTreeMap::<i64, Vec<ForeignKeyColumn>>::new();
        let mut stmt = connection.prepare(
            "SELECT id, seq, \"table\", \"from\", \"to\", on_update, on_delete, \"match\"
             FROM pragma_foreign_key_list(?1) ORDER BY id, seq",
        )?;
        let rows = stmt.query_map([&table], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                ForeignKeyColumn {
                    sequence: row.get(1)?,
                    table: row.get(2)?,
                    from: row.get(3)?,
                    to: row.get(4)?,
                    on_update: row.get(5)?,
                    on_delete: row.get(6)?,
                    matching: row.get(7)?,
                },
            ))
        })?;
        for row in rows {
            let (id, column) = row?;
            foreign_keys.entry(id).or_default().push(column);
        }
        let mut foreign_keys: Vec<_> = foreign_keys.into_values().collect();
        foreign_keys.sort();
        let mut indexes = BTreeMap::new();
        let mut stmt = connection
            .prepare("SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1)")?;
        let rows = stmt.query_map([&table], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, bool>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?;
        for row in rows {
            let (name, unique, origin, partial) = row?;
            let mut detail = connection.prepare(
                "SELECT CASE WHEN cid >= 0 THEN 0 ELSE cid END, name, desc, coll, key
                 FROM pragma_index_xinfo(?1) ORDER BY seqno",
            )?;
            let columns = detail
                .query_map([&name], |row| {
                    Ok(IndexColumn {
                        kind: row.get(0)?,
                        name: row.get(1)?,
                        descending: row.get(2)?,
                        collation: row.get(3)?,
                        key: row.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let sql: Option<String> = connection.query_row(
                "SELECT sql FROM sqlite_schema WHERE type='index' AND name=?1",
                [&name],
                |row| row.get(0),
            )?;
            let definition = sql.map(|sql| {
                let tokens = sql_tokens(&sql);
                // The index name is not part of its expressions or partial predicate.
                tokens
                    .iter()
                    .position(|token| token == "on")
                    .map(|offset| tokens[offset + 1..].to_vec())
                    .unwrap_or(tokens)
            });
            indexes.insert(
                name,
                IndexShape {
                    unique,
                    origin,
                    partial,
                    columns,
                    definition,
                },
            );
        }
        shape.insert(
            table,
            TableShape {
                columns,
                indexes,
                foreign_keys,
                checks,
                deferred_foreign_keys: tokens.iter().any(|token| token == "deferrable"),
            },
        );
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

/// Checks required columns, indexes and constraints before the runtime can write.
/// Extra user tables and indexes are ignored. Column order is ignored.
pub(crate) fn validate_application_schema(connection: &Connection) -> Result<()> {
    validate_schema(connection, false)
}

pub(crate) fn validate_readonly_schema(connection: &Connection) -> Result<()> {
    validate_schema(connection, true)
}

pub(crate) fn install_derived_objects(connection: &Connection) -> Result<()> {
    validate_schema(connection, true)?;
    let mut superseded = Vec::new();
    for &(name, expected) in SUPERSEDED_INDEXES {
        let sql: Option<String> = connection.query_row(
            "SELECT sql FROM sqlite_schema WHERE type='index' AND name=?", [name], |row| row.get(0),
        ).optional()?;
        if sql.is_some_and(|sql| sql_tokens(&sql) == sql_tokens(expected)) {
            superseded.push(name);
        }
    }
    let present = |kind: &str, name: &str| -> Result<bool> {
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type=? AND name=?)",
            [kind, name], |row| row.get(0),
        )?)
    };
    let mut missing = Vec::new();
    for &table in DERIVED_TABLES {
        if !present("table", table)? { missing.push(("table", table)); }
    }
    let rebuild_settings = missing.iter().any(|(_, name)| matches!(*name,
        "conversation_setting_writes" | "memory_history_operations" | "memory_winner_chain"));
    let rebuild_workspace = missing.iter().any(|(_, name)| matches!(*name,
        "workspace_tasks_lookup" | "workspace_assignments_lookup"));
    let initialize_history = missing.contains(&("table", "conversation_history_revisions"));
    for &(_, index) in DERIVED_INDEXES {
        if !present("index", index)? { missing.push(("index", index)); }
    }
    if missing.is_empty() && superseded.is_empty() { return Ok(()); }

    // Execute the authoritative definitions, rather than a second hand-maintained DDL.
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(super::INITIAL_SCHEMA)?;
    let tx = connection.unchecked_transaction()?;
    for (kind, name) in missing {
        let sql: String = reference.query_row(
            "SELECT sql FROM sqlite_schema WHERE type=? AND name=?",
            [kind, name], |row| row.get(0),
        )?;
        tx.execute_batch(&sql)?;
    }
    for name in superseded {
        tx.execute_batch(&format!("DROP INDEX {name}"))?;
    }
    validate_application_schema(&tx)?;
    if rebuild_settings || rebuild_workspace || initialize_history {
        let mut statement = tx.prepare("SELECT network_id FROM networks ORDER BY network_id")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let network: String = row.get(0)?;
            if rebuild_settings {
                super::conversation_settings_projection::rebuild_projection(&tx, &network)?;
            }
            if rebuild_workspace {
                super::workspace::rebuild_lookup_projection(&tx, &network)?;
            }
            if initialize_history {
                // This is local cache metadata, not part of the signed shared state.
                // A new random starting epoch cannot reuse a prior zero cursor after repair.
                tx.execute(
                    "INSERT INTO conversation_history_revisions(network_id,conversation_id,revision) VALUES(?,'',(random() & 0x3fffffffffffffff)+1)",
                    [&network],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

fn validate_schema(connection: &Connection, allow_missing_derived: bool) -> Result<()> {
    let expected = expected_application_shape()?;
    let actual = application_shape(connection)?;
    let mut mismatches = Vec::new();

    for (table, expected_table) in expected {
        let Some(actual_table) = actual.get(table) else {
            if allow_missing_derived && DERIVED_TABLES.contains(&table.as_str()) {
                continue;
            }
            mismatches.push(format!("{table}: missing table"));
            continue;
        };
        for (index, expected_index) in &expected_table.indexes {
            if allow_missing_derived
                && DERIVED_INDEXES.contains(&(table.as_str(), index.as_str()))
                && !actual_table.indexes.contains_key(index)
            {
                continue;
            }
            if actual_table.indexes.get(index) != Some(expected_index) {
                mismatches.push(format!(
                    "{table}.{index}: required index metadata does not match this Core"
                ));
            }
        }
        if actual_table.foreign_keys != expected_table.foreign_keys
            || actual_table.deferred_foreign_keys != expected_table.deferred_foreign_keys
        {
            mismatches.push(format!(
                "{table}: foreign key constraints do not match this Core"
            ));
        }
        if actual_table.checks != expected_table.checks {
            mismatches.push(format!("{table}: CHECK constraints do not match this Core"));
        }
        for (column, expected_column) in &expected_table.columns {
            let Some(actual_column) = actual_table.columns.get(column) else {
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
        "data_schema_mismatch: database user_version matches the current data epoch but schema metadata does not match this Core build. \
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
                 ALTER TABLE scheduler_jobs RENAME COLUMN scope TO missing_scope;",
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
        assert_eq!(std::fs::read(&path)?, before);

        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    fn assert_schema_refused(schema: &str, expected_diagnostic: &str) -> Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "vibeface-schema-integrity-{}",
            uuid::Uuid::new_v4()
        ));
        crate::config::private_dir(&dir)?;
        let path = dir.join("node.sqlite3");
        {
            let db = Connection::open(&path)?;
            db.execute_batch(schema)?;
            db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            db.execute(
                "INSERT INTO identity_meta(key,value) VALUES('device','preserved-device')",
                [],
            )?;
            let actual = application_shape(&db)?;
            for (table, shape) in expected_application_shape()? {
                assert_eq!(
                    actual[table].columns, shape.columns,
                    "{table} columns must be unchanged in the fixture"
                );
            }
        }
        let before = std::fs::read(&path)?;
        let error = match Store::open(&path) {
            Ok(_) => panic!("incompatible schema was accepted"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("data_schema_mismatch"), "{error}");
        assert!(error.contains(expected_diagnostic), "{error}");
        assert!(
            error.contains("backup") && error.contains("offline repair"),
            "{error}"
        );
        assert_eq!(std::fs::read(&path)?, before);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn missing_or_changed_integrity_indexes_are_refused_without_writes() -> Result<()> {
        for statement in [
            "DROP INDEX events_main;",
            "DROP INDEX invites_invite_hash;",
            "DROP INDEX events_main; CREATE INDEX events_main ON events(network_id,author,seq) WHERE main=1;",
            "DROP INDEX events_main; CREATE UNIQUE INDEX events_main ON events(network_id,author,seq) WHERE main=0;",
            "DROP INDEX events_main; CREATE UNIQUE INDEX events_main ON events(network_id,author,seq);",
            "DROP INDEX events_main; CREATE UNIQUE INDEX events_main ON events(network_id,author,hash) WHERE main=1;",
            "DROP INDEX events_main; CREATE UNIQUE INDEX events_main ON events(network_id,author COLLATE NOCASE,seq) WHERE main=1;",
            "DROP INDEX triage_queue_claim; CREATE INDEX triage_queue_claim ON triage_queue(json_extract(event,'$.wrong'),json_extract(event,'$._triage.attempt'),json_extract(event,'$._triage.state'),ts_ms,event_id);",
        ] {
            let schema = format!("{INITIAL_SCHEMA}\n{statement}");
            let index = if statement.contains("invites_invite_hash") {
                "invites_invite_hash"
            } else if statement.contains("triage_queue_claim") {
                "triage_queue_claim"
            } else {
                "events_main"
            };
            assert_schema_refused(&schema, index)?;
        }
        Ok(())
    }

    #[test]
    fn foreign_key_and_check_changes_with_identical_columns_are_refused_without_writes()
    -> Result<()> {
        let foreign_key = ", CONSTRAINT `0` FOREIGN KEY (`network_id`) REFERENCES `networks` (`network_id`) ON UPDATE NO ACTION ON DELETE NO ACTION";
        assert!(INITIAL_SCHEMA.contains(foreign_key));
        assert_schema_refused(
            &INITIAL_SCHEMA.replace(foreign_key, ""),
            "events: foreign key",
        )?;
        assert_schema_refused(
            &INITIAL_SCHEMA.replace("ON DELETE NO ACTION", "ON DELETE CASCADE"),
            "events: foreign key",
        )?;
        assert_schema_refused(
            &INITIAL_SCHEMA.replace(
                "ON DELETE NO ACTION",
                "ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED",
            ),
            "events: foreign key",
        )?;
        assert_schema_refused(
            &INITIAL_SCHEMA.replace(
                "`main` integer NOT NULL",
                "`main` integer NOT NULL CHECK(main IN (0,1))",
            ),
            "events: CHECK",
        )?;
        Ok(())
    }

    #[test]
    fn equivalent_index_formatting_and_quoted_identifiers_are_accepted() -> Result<()> {
        let db = Connection::open_in_memory()?;
        init_current_db(&db)?;
        db.execute_batch("DROP INDEX events_main; CREATE UNIQUE INDEX events_main ON events (network_id, author, seq) /* local formatting */ WHERE MAIN = 1;")?;
        validate_application_schema(&db)
    }

    #[test]
    fn missing_derived_objects_are_installed_without_changing_existing_rows() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("vibeface-derived-index-{}", uuid::Uuid::new_v4()));
        crate::config::private_dir(&dir)?;
        let path = dir.join("node.sqlite3");
        {
            let db = Connection::open(&path)?;
            init_current_db(&db)?;
            for &(_, sql) in SUPERSEDED_INDEXES {
                db.execute_batch(sql)?;
            }
            for &(_, index) in DERIVED_INDEXES {
                db.execute_batch(&format!("DROP INDEX {index};"))?;
            }
            for &table in DERIVED_TABLES {
                db.execute_batch(&format!("DROP TABLE {table};"))?;
            }
            db.execute(
                "INSERT INTO identity_meta(key,value) VALUES('device','original-device')",
                [],
            )?;
        }
        let before = std::fs::read(&path)?;
        drop(Store::open_readonly(&path)?);
        assert_eq!(std::fs::read(&path)?, before);
        let store = Store::open(&path)?;
        let value: String = store.connection.query_row(
            "SELECT value FROM identity_meta WHERE key='device'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(value, "original-device");
        validate_application_schema(&store.connection)?;
        for &(name, _) in SUPERSEDED_INDEXES {
            let exists: bool = store.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='index' AND name=?)",
                [name], |row| row.get(0),
            )?;
            assert!(!exists, "redundant index {name} was retained");
        }
        drop(store);
        drop(Store::open(&path)?);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn invalid_schema_is_not_modified_while_a_derived_index_is_missing() -> Result<()> {
        assert_schema_refused(
            &format!("{INITIAL_SCHEMA}\nDROP INDEX work_runs_reconcile; DROP INDEX events_main;"),
            "events_main",
        )?;
        assert_schema_refused(
            &format!(
                "{INITIAL_SCHEMA}\nDROP INDEX work_runs_reconcile; CREATE INDEX work_runs_reconcile ON work_runs(work_id);"
            ),
            "work_runs_reconcile",
        )?;
        assert_schema_refused(
            &format!("{INITIAL_SCHEMA}\nDROP TABLE memory_winner_chain; DROP INDEX events_main;"),
            "events_main",
        )?;
        assert_schema_refused(
            &INITIAL_SCHEMA.replace("`winning` integer NOT NULL DEFAULT 0", "`winning` text NOT NULL DEFAULT 0"),
            "memory_history_operations.winning",
        )
    }

    #[test]
    fn customized_index_is_preserved_when_redundant_former_indexes_are_removed() -> Result<()> {
        let db = Connection::open_in_memory()?;
        init_current_db(&db)?;
        for &(_, sql) in SUPERSEDED_INDEXES { db.execute_batch(sql)?; }
        db.execute_batch("DROP INDEX messages_order; CREATE INDEX messages_order ON messages(role)")?;
        install_derived_objects(&db)?;
        let sql: String = db.query_row(
            "SELECT sql FROM sqlite_schema WHERE name='messages_order'", [], |row| row.get(0),
        )?;
        assert_eq!(sql_tokens(&sql), sql_tokens("CREATE INDEX messages_order ON messages(role)"));
        assert_schema_refused(
            &format!("{INITIAL_SCHEMA}\n{}; DROP INDEX events_main;", SUPERSEDED_INDEXES[0].1),
            "events_main",
        )?;
        Ok(())
    }

    #[test]
    fn failed_derived_reconstruction_rolls_back_every_created_object() -> Result<()> {
        let db = Connection::open_in_memory()?;
        init_current_db(&db)?;
        for &table in DERIVED_TABLES {
            db.execute_batch(&format!("DROP TABLE {table};"))?;
        }
        db.execute_batch(
            "INSERT INTO networks(network_id,bundle) VALUES('network','{}');
             INSERT INTO events(network_id,author,seq,hash,scope_id,lamport,created_at_ms,seen_at_ms,envelope,main,effective,kind)
             VALUES('network','author',1,'event','conversation',1,1,1,'invalid signed envelope',1,1,'bot_changed');",
        )?;
        assert!(install_derived_objects(&db).is_err());
        for &table in DERIVED_TABLES {
            let exists: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?)",
                [table], |row| row.get(0),
            )?;
            assert!(!exists, "failed reconstruction retained {table}");
        }
        let envelope: String = db.query_row("SELECT envelope FROM events", [], |row| row.get(0))?;
        assert_eq!(envelope, "invalid signed envelope");
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
            db.execute("CREATE INDEX user_event_note ON events(subject)", [])?;
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
