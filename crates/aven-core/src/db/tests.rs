use super::rows::optional_task_date;
use super::*;
use crate::recurrence::{RecurrenceOutcome, RecurrenceProjectionState};
use crate::types::MutableEntityType;

const PRIVATE_IN_MEMORY_INPUTS: &[&str] = &[
    ":memory:",
    "sqlite::memory:",
    "sqlite://:memory:",
    "file::memory:",
    "sqlite:file::memory:",
    "sqlite://file::memory:",
    "%3Amemory%3A",
    "sqlite:%3Amemory%3A",
    "sqlite://%3Amemory%3A",
    "file:%3Amemory%3A",
    "sqlite:file:%3Amemory%3A",
    "sqlite://file:%3Amemory%3A",
    "file::memory:?cache=private",
    "sqlite://?mode=memory&cache=private",
    "named?mode=memory&cache=private",
    "sqlite://named?mode=memory&cache=private",
    "sqlite://named?cache=private&mode=memory",
    "sqlite://named?mode=mem%6Fry&cache=private",
];

#[tokio::test]
async fn wal_readers_progress_while_writes_remain_serialized() {
    let temp = tempfile::tempdir().unwrap();
    let database = Database::open(&temp.path().join("concurrency.sqlite"))
        .await
        .unwrap();
    let mut writer = database.acquire_writer().await.unwrap();
    let mut tx = begin_immediate(&mut writer).await.unwrap();
    set_meta(&mut tx, "local_seq", "1").await.unwrap();

    let reader_database = database.clone();
    let reader = tokio::spawn(async move {
        let mut conn = reader_database.acquire_reader().await.unwrap();
        get_meta(&mut conn, "local_seq").await.unwrap()
    });
    let observed = tokio::time::timeout(Duration::from_secs(1), reader)
        .await
        .expect("reader should not wait for the writer")
        .unwrap();
    assert_eq!(observed.as_deref(), Some("0"));

    let second_writer_database = database.clone();
    let second_writer = tokio::spawn(async move {
        let mut conn = second_writer_database.acquire_writer().await.unwrap();
        let mut tx = begin_immediate(&mut conn).await.unwrap();
        set_meta(&mut tx, "local_seq", "2").await.unwrap();
        tx.commit().await.unwrap();
    });
    tokio::task::yield_now().await;
    assert!(!second_writer.is_finished());

    tx.commit().await.unwrap();
    drop(writer);
    tokio::time::timeout(Duration::from_secs(1), second_writer)
        .await
        .expect("second writer should proceed after the first commits")
        .unwrap();
    assert_eq!(
        database.meta("local_seq").await.unwrap().as_deref(),
        Some("2")
    );
}

#[test]
fn database_storage_follows_sqlx_connection_input_semantics() {
    for &input in PRIVATE_IN_MEMORY_INPUTS {
        let options = SqliteConnectOptions::from_str(input).unwrap();
        assert_eq!(
            database_storage(input, &options),
            DatabaseStorage::InMemory,
            "{input}"
        );
    }

    for input in [
        "mode=memory.sqlite",
        "ordinary-file::memory:.sqlite",
        "sqlite::memory:.sqlite",
        "/tmp/directory-mode=memory/database.sqlite",
    ] {
        let options = SqliteConnectOptions::from_str(input).unwrap();
        assert_eq!(
            database_storage(input, &options),
            DatabaseStorage::File,
            "{input}"
        );
    }
}

#[tokio::test]
async fn database_retains_canonical_file_identity() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("identity.sqlite");
    let database = Database::open(&path).await.unwrap();

    assert_eq!(
        database.file_identity(),
        Some(fs::canonicalize(&path).unwrap().as_path())
    );
}

#[tokio::test]
async fn in_memory_databases_have_no_file_identity() {
    for &input in PRIVATE_IN_MEMORY_INPUTS {
        let database = Database::open(Path::new(input)).await.unwrap();
        assert_eq!(database.file_identity(), None, "{input}");
    }
}

#[tokio::test]
async fn private_in_memory_inputs_keep_one_connection_and_one_schema() {
    for &input in PRIVATE_IN_MEMORY_INPUTS {
        let pool = open_db(Path::new(input)).await.unwrap();
        let pool_options = pool.options();
        assert_eq!(pool_options.get_min_connections(), 1, "{input}");
        assert_eq!(pool_options.get_max_connections(), 1, "{input}");
        assert_eq!(pool_options.get_idle_timeout(), None, "{input}");
        assert_eq!(pool_options.get_max_lifetime(), None, "{input}");
        assert_eq!(pool.size(), 1, "{input}");
        let mut first = pool.acquire().await.unwrap();
        let second_pool = pool.clone();
        let mut second = tokio::spawn(async move {
            let mut connection = second_pool.acquire().await.unwrap();
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM sqlite_schema WHERE name = 'acquisition_visibility'",
            )
            .fetch_one(&mut *connection)
            .await
            .unwrap()
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut second)
                .await
                .is_err(),
            "{input} opened another connection"
        );

        sqlx::query("CREATE TABLE acquisition_visibility(id INTEGER PRIMARY KEY)")
            .execute(&mut *first)
            .await
            .unwrap();
        drop(first);

        let visible = tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(visible, 1, "{input}");
    }
}

#[tokio::test]
async fn filesystem_lookalike_keeps_wal_and_concurrent_connections() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp
        .path()
        .join("ordinary-file::memory:-mode=memory.sqlite");
    let pool = open_db(&path).await.unwrap();
    let first = pool.acquire().await.unwrap();
    let mut second = tokio::time::timeout(Duration::from_secs(1), pool.acquire())
        .await
        .expect("file pool should allow a concurrent acquisition")
        .unwrap();
    let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&mut *second)
        .await
        .unwrap();

    assert_eq!(
        pool.options().get_max_connections(),
        FILE_DATABASE_CONNECTIONS
    );
    assert_eq!(journal_mode, "wal");
    drop(first);
    drop(second);
    pool.close().await;
}

#[tokio::test]
async fn in_process_backup_captures_wal_and_replaces_destination() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.sqlite");
    let backup = temp.path().join("backup.sqlite");
    let database = Database::open(&source).await.unwrap();
    let mut writer = database.acquire_writer().await.unwrap();
    let mut tx = begin_immediate(&mut writer).await.unwrap();
    set_meta(&mut tx, "backup-test", "first").await.unwrap();
    tx.commit().await.unwrap();
    drop(writer);
    assert!(wal_path(&source).exists());

    fs::write(&backup, b"existing destination").unwrap();
    backup_database(&source, &backup).await.unwrap();
    let mut backup_conn = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&backup)
            .read_only(true),
    )
    .await
    .unwrap();
    assert_eq!(
        get_meta(&mut backup_conn, "backup-test")
            .await
            .unwrap()
            .as_deref(),
        Some("first")
    );
    drop(backup_conn);

    let mut writer = database.acquire_writer().await.unwrap();
    set_meta(&mut writer, "backup-test", "second")
        .await
        .unwrap();
    drop(writer);
    backup_database(&source, &backup).await.unwrap();
    let mut backup_conn = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&backup)
            .read_only(true),
    )
    .await
    .unwrap();
    assert_eq!(
        get_meta(&mut backup_conn, "backup-test")
            .await
            .unwrap()
            .as_deref(),
        Some("second")
    );
}

#[tokio::test]
async fn in_process_backup_rejects_missing_source() {
    let temp = tempfile::tempdir().unwrap();
    let backup = temp.path().join("backup.sqlite");
    let error = backup_database(&temp.path().join("missing.sqlite"), &backup)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("could not open source"));
    assert!(!backup.exists());
}

#[tokio::test]
async fn restore_replaces_sidecars_and_preserves_safety_copy() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target.sqlite");
    let source = temp.path().join("source.sqlite");
    let target_database = Database::open(&target).await.unwrap();
    let source_database = Database::open(&source).await.unwrap();
    let mut target_writer = target_database.acquire_writer().await.unwrap();
    set_meta(&mut target_writer, "restore-test", "target")
        .await
        .unwrap();
    drop(target_writer);
    let mut source_writer = source_database.acquire_writer().await.unwrap();
    set_meta(&mut source_writer, "restore-test", "source")
        .await
        .unwrap();
    drop(source_writer);
    target_database.pool.close().await;
    source_database.pool.close().await;
    fs::write(wal_path(&target), b"stale wal").unwrap();
    fs::write(shm_path(&target), b"stale shm").unwrap();

    let safety = restore_database_file(&target, &source).await.unwrap();
    assert!(!wal_path(&target).exists());
    assert!(!shm_path(&target).exists());

    let mut restored = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&target)
            .read_only(true),
    )
    .await
    .unwrap();
    assert_eq!(
        get_meta(&mut restored, "restore-test")
            .await
            .unwrap()
            .as_deref(),
        Some("source")
    );
    let mut preserved = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&safety)
            .read_only(true),
    )
    .await
    .unwrap();
    assert_eq!(
        get_meta(&mut preserved, "restore-test")
            .await
            .unwrap()
            .as_deref(),
        Some("target")
    );
}

#[tokio::test]
async fn task_from_row_maps_empty_dates_to_absence() {
    let mut conn = SqliteConnection::connect(":memory:")
        .await
        .expect("open db");
    let row = sqlx::query(
        "SELECT 'TASK000000000001' AS id,
                    '0000000000000000' AS workspace_id,
                    'optional dates' AS title,
                    '' AS description,
                    '0000000000000001' AS project_id,
                    'app' AS project_key,
                    'APP' AS project_prefix,
                    'todo' AS status,
                    'none' AS priority,
                    'unknown' AS source,
                    't' AS created_at,
                    't' AS updated_at,
                    't' AS queue_activity_at,
                    '' AS available_at,
                    '' AS due_on,
                    0 AS deleted,
                    0 AS is_epic",
    )
    .fetch_one(&mut conn)
    .await
    .expect("row");

    let task = task_from_row(&row).unwrap();

    assert_eq!(task.available_at, None);
    assert_eq!(task.due_on, None);
}

#[test]
fn task_date_boundary_preserves_present_values() {
    assert_eq!(
        optional_task_date("2099-01-01T00:00:00Z".to_string()).as_deref(),
        Some("2099-01-01T00:00:00Z")
    );
    assert_eq!(
        optional_task_date("2099-01-01".to_string()).as_deref(),
        Some("2099-01-01")
    );
}

#[tokio::test]
async fn task_from_row_rejects_invalid_status_and_priority() {
    let mut conn = SqliteConnection::connect(":memory:")
        .await
        .expect("open db");
    let row = sqlx::query(
        "SELECT 'TASK000000000001' AS id,
                    '0000000000000000' AS workspace_id,
                    'bad status' AS title,
                    '' AS description,
                    '0000000000000001' AS project_id,
                    'app' AS project_key,
                    'APP' AS project_prefix,
                    'blocked' AS status,
                    'none' AS priority,
                    'unknown' AS source,
                    't' AS created_at,
                    't' AS updated_at,
                    't' AS queue_activity_at,
                    0 AS deleted,
                    0 AS is_epic",
    )
    .fetch_one(&mut conn)
    .await
    .expect("row");
    assert_eq!(
        task_from_row(&row).unwrap_err().to_string(),
        "error invalid-status input=blocked choices=inbox,backlog,todo,active,done,canceled"
    );

    let row = sqlx::query(
        "SELECT 'TASK000000000001' AS id,
                    '0000000000000000' AS workspace_id,
                    'bad priority' AS title,
                    '' AS description,
                    '0000000000000001' AS project_id,
                    'app' AS project_key,
                    'APP' AS project_prefix,
                    'inbox' AS status,
                    'soon' AS priority,
                    't' AS created_at,
                    't' AS updated_at,
                    't' AS queue_activity_at,
                    0 AS deleted,
                    0 AS is_epic",
    )
    .fetch_one(&mut conn)
    .await
    .expect("row");
    assert_eq!(
        task_from_row(&row).unwrap_err().to_string(),
        "error invalid-priority input=soon choices=none,low,medium,high,urgent"
    );
}

#[tokio::test]
async fn recurrence_migration_enforces_schedule_immutability_and_task_conflict_compatibility() {
    let (_temp, mut conn) = crate::test_support::test_conn().await;
    sqlx::query(
        "INSERT INTO recurrence_series(
                workspace_id, id, title, description, project_id, priority, initial_status,
                frequency, interval, weekdays, timezone, start_on, available_local_time,
                due_policy, state, created_at, updated_at
             ) VALUES (
                '0000000000000000', '7KQ9A1X4MV2P8D6R', 'journal', '',
                '7KQ9A1X4MV2P8D6S', 'none', 'todo', 'daily', 1, '', 'UTC',
                '2026-07-20', '09:00:00', 'same_day', 'active', 't', 't'
             )",
    )
    .execute(&mut *conn)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO recurrence_series(
                workspace_id, id, title, description, project_id, priority, initial_status,
                frequency, interval, weekdays, timezone, start_on, available_local_time,
                due_policy, state, created_at, updated_at
             ) VALUES
                ('0000000000000000', '7KQ9A1X4MV2P8D7A', 'days', '',
                 '7KQ9A1X4MV2P8D6S', 'none', 'todo', 'daily', 3, '', 'UTC',
                 '2026-07-20', '', 'same_day', 'active', 't', 't'),
                ('0000000000000000', '7KQ9A1X4MV2P8D7B', 'months', '',
                 '7KQ9A1X4MV2P8D6S', 'none', 'todo', 'monthly', 3, '', 'UTC',
                 '2026-07-20', '', 'same_day', 'active', 't', 't'),
                ('0000000000000000', '7KQ9A1X4MV2P8D7C', 'years', '',
                 '7KQ9A1X4MV2P8D6S', 'none', 'todo', 'yearly', 2, '', 'UTC',
                 '2026-07-20', '', 'same_day', 'active', 't', 't')",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    for invalid in [
        "INSERT INTO recurrence_series(workspace_id,id,title,description,project_id,priority,initial_status,frequency,interval,weekdays,timezone,start_on,available_local_time,due_policy,state,created_at,updated_at) VALUES ('0000000000000000','7KQ9A1X4MV2P8D7D','bad','','7KQ9A1X4MV2P8D6S','none','todo','daily',0,'','UTC','2026-07-20','','same_day','active','t','t')",
        "INSERT INTO recurrence_series(workspace_id,id,title,description,project_id,priority,initial_status,frequency,interval,weekdays,timezone,start_on,available_local_time,due_policy,state,created_at,updated_at) VALUES ('0000000000000000','7KQ9A1X4MV2P8D7E','bad','','7KQ9A1X4MV2P8D6S','none','todo','monthly',2,'mon','UTC','2026-07-20','','same_day','active','t','t')",
        "INSERT INTO recurrence_series(workspace_id,id,title,description,project_id,priority,initial_status,frequency,interval,weekdays,timezone,start_on,available_local_time,due_policy,state,created_at,updated_at) VALUES ('0000000000000000','7KQ9A1X4MV2P8D7F','bad','','7KQ9A1X4MV2P8D6S','none','todo','weekly',2,'','UTC','2026-07-20','','same_day','active','t','t')",
    ] {
        assert!(sqlx::query(invalid).execute(&mut *conn).await.is_err());
    }

    sqlx::query(
        "UPDATE recurrence_series SET title = 'future journal' WHERE id = '7KQ9A1X4MV2P8D6R'",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    let error = sqlx::query(
            "UPDATE recurrence_series SET frequency = 'weekly', weekdays = 'mon' WHERE id = '7KQ9A1X4MV2P8D6R'",
        )
        .execute(&mut *conn)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("recurrence schedule is immutable")
    );

    sqlx::query(
            "INSERT INTO conflicts(task_id, field, local_value, remote_value, remote_change_id, variant_a, variant_b, created_at)
             VALUES ('7KQ9A1X4MV2P8D6T', 'title', 'a', 'b', 'remote', 'a', 'b', 't')",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
    let identity: (String, String) = sqlx::query_as(
        "SELECT entity_type, entity_id FROM conflicts WHERE task_id = '7KQ9A1X4MV2P8D6T'",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        identity,
        ("task".to_string(), "7KQ9A1X4MV2P8D6T".to_string())
    );
}

#[tokio::test]
async fn recurrence_rows_map_through_validated_domain_types() {
    let mut conn = SqliteConnection::connect(":memory:").await.unwrap();
    let series_row = sqlx::query(
        "SELECT '0000000000000000' AS workspace_id,
                    '7KQ9A1X4MV2P8D6R' AS id, 'journal' AS title, '' AS description,
                    '7KQ9A1X4MV2P8D6S' AS project_id, 'high' AS priority,
                    'todo' AS initial_status, 'weekly' AS frequency, 2 AS interval,
                    'mon,fri' AS weekdays, 'Europe/Stockholm' AS timezone,
                    '2026-07-20' AS start_on, '09:30:00' AS available_local_time,
                    'same_day' AS due_policy, 'active' AS state, '' AS stopped_at,
                    'created' AS created_at, 'updated' AS updated_at, 0 AS deleted",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    let series = recurrence_series_from_row(&series_row).unwrap();
    assert_eq!(series.id.as_str(), "7KQ9A1X4MV2P8D6R");
    assert_eq!(series.rule.interval(), 2);
    assert_eq!(series.available_local_time.unwrap().to_string(), "09:30:00");

    let occurrence_row = sqlx::query(
        "SELECT '0000000000000000' AS workspace_id,
                    '7KQ9A1X4MV2P8D6R' AS series_id, '2026-07-20' AS slot_on,
                    '7KQ9A1X4MV2P8D6T' AS task_id, 'completed' AS outcome,
                    'resolved' AS resolved_at, 'change' AS outcome_change_id,
                    'resolved' AS projection_state, '' AS archived_at",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    let occurrence = recurrence_occurrence_from_row(&occurrence_row).unwrap();
    assert_eq!(
        occurrence.task_id.as_ref().map(|task_id| task_id.as_str()),
        Some("7KQ9A1X4MV2P8D6T")
    );
    assert_eq!(occurrence.outcome, Some(RecurrenceOutcome::Completed));
    assert_eq!(
        occurrence.projection_state,
        RecurrenceProjectionState::Resolved
    );

    let invalid_row = sqlx::query(
        "SELECT '0000000000000000' AS workspace_id,
                    '7KQ9A1X4MV2P8D6R' AS id, 'journal' AS title, '' AS description,
                    '7KQ9A1X4MV2P8D6S' AS project_id, 'none' AS priority,
                    'todo' AS initial_status, 'weekly' AS frequency, 1 AS interval,
                    'fri,mon' AS weekdays, 'UTC' AS timezone, '2026-07-20' AS start_on,
                    '' AS available_local_time, 'none' AS due_policy, 'active' AS state,
                    '' AS stopped_at, 't' AS created_at, 't' AS updated_at, 0 AS deleted",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert!(recurrence_series_from_row(&invalid_row).is_err());
}

#[tokio::test]
async fn field_versions_support_task_and_recurrence_series_identity() {
    let (_temp, mut conn) = crate::test_support::test_conn().await;
    sqlx::query(
            "INSERT INTO tasks(id, title, description, project_id, status, priority, created_at, updated_at)
             VALUES ('7KQ9A1X4MV2P8D6T', 'task', '', '7KQ9A1X4MV2P8D6S', 'todo', 'none', 't', 't')",
        )
        .execute(&mut *conn)
        .await
        .unwrap();
    set_field_version(&mut conn, "7KQ9A1X4MV2P8D6T", "title", "task-version")
        .await
        .unwrap();
    set_entity_field_version(
        &mut conn,
        &crate::workspaces::default_workspace_id(),
        MutableEntityType::RecurrenceSeries,
        "7KQ9A1X4MV2P8D6R",
        "title",
        "series-version",
    )
    .await
    .unwrap();

    assert_eq!(
        field_version(&mut conn, "7KQ9A1X4MV2P8D6T", "title")
            .await
            .unwrap()
            .as_deref(),
        Some("task-version")
    );
    assert_eq!(
        entity_field_version(
            &mut conn,
            &crate::workspaces::default_workspace_id(),
            MutableEntityType::RecurrenceSeries,
            "7KQ9A1X4MV2P8D6R",
            "title",
        )
        .await
        .unwrap()
        .as_deref(),
        Some("series-version")
    );
}
