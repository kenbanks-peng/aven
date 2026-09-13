use sqlx::{Connection, SqliteConnection};

#[tokio::test]
async fn ios_source_migration_preserves_existing_sources_and_schema_objects() {
    let mut connection = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    let migrator = sqlx::migrate!("./migrations");
    let migration = migrator
        .iter()
        .find(|migration| migration.version == 20260913060740)
        .unwrap();
    for previous in migrator
        .iter()
        .filter(|previous| previous.version < migration.version)
    {
        sqlx::raw_sql(previous.sql.clone())
            .execute(&mut connection)
            .await
            .unwrap();
    }
    for (index, source) in ["cli", "tui", "api", "unknown"].iter().enumerate() {
        sqlx::query(
            "INSERT INTO tasks(id, title, description, project_id, status, priority,
             created_at, updated_at, source)
             VALUES (?, 'existing task', '', '0000000000000000', 'inbox', 'none',
             '2026-09-01T00:00:00Z', '2026-09-01T00:00:00Z', ?)",
        )
        .bind(format!("{index:016}"))
        .bind(source)
        .execute(&mut connection)
        .await
        .unwrap();
    }
    let objects_sql = "SELECT name, sql FROM sqlite_schema
                       WHERE type IN ('index', 'trigger') ORDER BY name";
    let before: Vec<(String, Option<String>)> = sqlx::query_as(objects_sql)
        .fetch_all(&mut connection)
        .await
        .unwrap();
    sqlx::raw_sql(migration.sql.clone())
        .execute(&mut connection)
        .await
        .unwrap();
    let after: Vec<(String, Option<String>)> = sqlx::query_as(objects_sql)
        .fetch_all(&mut connection)
        .await
        .unwrap();
    assert_eq!(before, after);
    let sources: Vec<String> = sqlx::query_scalar("SELECT source FROM tasks ORDER BY id")
        .fetch_all(&mut connection)
        .await
        .unwrap();
    assert_eq!(sources, ["cli", "tui", "api", "unknown"]);
    sqlx::query("UPDATE tasks SET source = 'ios' WHERE source = 'unknown'")
        .execute(&mut connection)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE tasks SET source = 'invalid'")
            .execute(&mut connection)
            .await
            .is_err()
    );
}

#[test]
fn ios_source_protocol_rejects_older_peers_and_source_mutations() {
    use aven_core::sync::wire::{
        SYNC_PROTOCOL_VERSION, validate_sync_protocol_version,
        validate_sync_request_protocol_version,
    };

    assert!(validate_sync_request_protocol_version(Some(16)).is_err());
    assert!(validate_sync_protocol_version(SYNC_PROTOCOL_VERSION, 16).is_err());
    assert!(validate_sync_request_protocol_version(Some(SYNC_PROTOCOL_VERSION)).is_ok());
    assert!(aven_core::task_fields::TaskField::parse("source").is_none());
}
