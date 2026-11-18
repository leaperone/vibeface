use super::super::*;

#[test]
fn first_release_schema_reopens_and_rejects_old_databases_without_modifying_them() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("vibeface-schema-{}", uuid::Uuid::new_v4()));
    crate::config::private_dir(&dir)?;
    let path = dir.join("node.sqlite3");
    let store = Store::open(&path)?;
    let version: u32 = store
        .connection
        .pragma_query_value(None, "user_version", |row| row.get(0))?;
    assert_eq!(version, SCHEMA_VERSION);
    store.connection.execute(
        "INSERT INTO identity_meta(key,value) VALUES('retained','value')",
        [],
    )?;
    drop(store);
    let store = Store::open(&path)?;
    let value: String = store.connection.query_row(
        "SELECT value FROM identity_meta WHERE key='retained'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(value, "value");
    drop(store);
    for version in [0, 2, 12] {
        let db = Connection::open(&path)?;
        db.pragma_update(None, "user_version", version)?;
        drop(db);
        let before = std::fs::read(&path)?;
        assert!(Store::open(&path).is_err());
        assert!(Store::open_readonly(&path).is_err());
        assert_eq!(std::fs::read(&path)?, before);
    }
    let view_path = dir.join("view.sqlite3");
    let db = Connection::open(&view_path)?;
    db.execute_batch("CREATE VIEW old_view AS SELECT 1;")?;
    drop(db);
    let before = std::fs::read(&view_path)?;
    assert!(Store::open(&view_path).is_err());
    assert_eq!(std::fs::read(&view_path)?, before);
    std::fs::remove_dir_all(dir)?;
    Ok(())
}
