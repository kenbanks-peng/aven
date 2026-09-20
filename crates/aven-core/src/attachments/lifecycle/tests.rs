use std::fs;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::Connection as _;
use sqlx::SqliteConnection;
use sqlx::sqlite::SqliteConnectOptions;

use crate::attachments::storage::{object_path, upsert_inventory_available};
use crate::db::{begin_immediate, open_db};

use super::filesystem::{
    DIRECTORY_CURSORS, FileMove, move_file_without_replacing_with, scan_directory_page,
    scan_directory_page_with,
};
use super::*;
use super::{staging_dir, trash_dir};

#[derive(Clone)]
struct TestClock(Arc<Mutex<DateTime<Utc>>>);

impl TestClock {
    fn at(value: &str) -> Self {
        Self(Arc::new(Mutex::new(
            DateTime::parse_from_rfc3339(value).unwrap().to_utc(),
        )))
    }

    fn advance(&self, duration: chrono::Duration) {
        let mut now = self.0.lock().unwrap();
        *now += duration;
    }
}

impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

async fn insert_task(conn: &mut SqliteConnection, task_id: &str) {
    sqlx::query(
        "INSERT INTO tasks(
               workspace_id, id, title, description, project_id, status, priority,
               created_at, updated_at, queue_activity_at
             ) VALUES ('0000000000000000', ?, 'task', '', 'project', 'inbox', 'none',
                       '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
    )
    .bind(task_id)
    .execute(conn)
    .await
    .unwrap();
}

async fn insert_attachment(
    conn: &mut SqliteConnection,
    attachment_id: &str,
    task_id: &str,
    sha256: &str,
    deleted: bool,
) {
    sqlx::query(
        "INSERT INTO task_attachments(
               workspace_id, attachment_id, task_id, sha256, byte_size, media_type, width, height,
               created_at, deleted, deleted_at
             ) VALUES ('0000000000000000', ?, ?, ?, 4, 'image/png', 1, 1,
                       '2026-01-01T00:00:00Z', ?, ?)",
    )
    .bind(attachment_id)
    .bind(task_id)
    .bind(sha256)
    .bind(i64::from(deleted))
    .bind(deleted.then_some("2026-01-01T00:00:00Z"))
    .execute(conn)
    .await
    .unwrap();
}

#[tokio::test]
async fn final_live_reference_starts_grace_once_and_restore_clears_it() {
    let temp = tempfile::tempdir().unwrap();
    let pool = open_db(&temp.path().join("test.sqlite")).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    upsert_inventory_available(&mut conn, hash, 4, "image/png")
        .await
        .unwrap();
    insert_task(&mut conn, "0000000000000001").await;
    insert_task(&mut conn, "0000000000000002").await;
    insert_attachment(
        &mut conn,
        "0000000000000011",
        "0000000000000001",
        hash,
        false,
    )
    .await;
    insert_attachment(
        &mut conn,
        "0000000000000012",
        "0000000000000002",
        hash,
        false,
    )
    .await;
    let clock = TestClock::at("2026-07-01T00:00:00Z");

    reconcile_liveness(&mut conn, &clock).await.unwrap();
    sqlx::query("UPDATE task_attachments SET deleted = 1, deleted_at = 'x' WHERE attachment_id = '0000000000000011'")
            .execute(&mut *conn).await.unwrap();
    reconcile_liveness(&mut conn, &clock).await.unwrap();
    let value: Option<String> =
        sqlx::query_scalar("SELECT unreferenced_at FROM blob_lifecycle WHERE sha256 = ?")
            .bind(hash)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(value, None, "one live reference keeps the hash live");

    sqlx::query("UPDATE task_attachments SET deleted = 1, deleted_at = 'x' WHERE attachment_id = '0000000000000012'")
            .execute(&mut *conn).await.unwrap();
    reconcile_liveness(&mut conn, &clock).await.unwrap();
    let first: String =
        sqlx::query_scalar("SELECT unreferenced_at FROM blob_lifecycle WHERE sha256 = ?")
            .bind(hash)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    clock.advance(chrono::Duration::days(1));
    reconcile_liveness(&mut conn, &clock).await.unwrap();
    let second: String =
        sqlx::query_scalar("SELECT unreferenced_at FROM blob_lifecycle WHERE sha256 = ?")
            .bind(hash)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(first, second, "grace starts exactly once");

    sqlx::query("UPDATE task_attachments SET deleted = 0, deleted_at = NULL WHERE attachment_id = '0000000000000012'")
            .execute(&mut *conn).await.unwrap();
    reconcile_liveness(&mut conn, &clock).await.unwrap();
    let restored: Option<String> =
        sqlx::query_scalar("SELECT unreferenced_at FROM blob_lifecycle WHERE sha256 = ?")
            .bind(hash)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(restored, None);
}

#[tokio::test]
async fn affected_liveness_reconciliation_is_scoped_and_write_minimal() {
    let temp = tempfile::tempdir().unwrap();
    let pool = open_db(&temp.path().join("test.sqlite")).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let affected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
    let unrelated = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
    let clock = TestClock::at("2026-07-01T00:00:00Z");
    for hash in [&affected, &unrelated] {
        upsert_inventory_available(&mut conn, hash, 4, "image/png")
            .await
            .unwrap();
    }
    insert_task(&mut conn, "0000000000000001").await;
    insert_attachment(
        &mut conn,
        "0000000000000011",
        "0000000000000001",
        &affected,
        false,
    )
    .await;
    for hash in [&affected, &unrelated] {
        sqlx::query("INSERT INTO blob_lifecycle(sha256, unreferenced_at) VALUES (?, ?)")
            .bind(hash)
            .bind("2026-06-01T00:00:00Z")
            .execute(&mut *conn)
            .await
            .unwrap();
    }
    sqlx::query(
        "CREATE TABLE lifecycle_updates(count INTEGER NOT NULL DEFAULT 0);
             CREATE TRIGGER count_lifecycle_updates
             AFTER UPDATE OF unreferenced_at ON blob_lifecycle
             BEGIN
                 INSERT INTO lifecycle_updates(count) VALUES (1);
             END",
    )
    .execute(&mut *conn)
    .await
    .unwrap();

    reconcile_liveness_for_hashes_in_transaction(
        &mut conn,
        std::slice::from_ref(&affected),
        &clock,
    )
    .await
    .unwrap();
    let unrelated_at: String =
        sqlx::query_scalar("SELECT unreferenced_at FROM blob_lifecycle WHERE sha256 = ?")
            .bind(&unrelated)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(unrelated_at, "2026-06-01T00:00:00Z");
    let updates: i64 = sqlx::query_scalar("SELECT count(*) FROM lifecycle_updates")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(updates, 1, "the live affected row changes once");

    reconcile_liveness_for_hashes_in_transaction(
        &mut conn,
        std::slice::from_ref(&affected),
        &clock,
    )
    .await
    .unwrap();
    let repeated_updates: i64 = sqlx::query_scalar("SELECT count(*) FROM lifecycle_updates")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(repeated_updates, 1, "an already-live row is not rewritten");

    sqlx::query(
        "UPDATE task_attachments SET deleted = 1, deleted_at = 'x'
             WHERE attachment_id = '0000000000000011'",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    reconcile_liveness_for_hashes_in_transaction(
        &mut conn,
        std::slice::from_ref(&affected),
        &clock,
    )
    .await
    .unwrap();
    let first_unreferenced: String =
        sqlx::query_scalar("SELECT unreferenced_at FROM blob_lifecycle WHERE sha256 = ?")
            .bind(&affected)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(first_unreferenced, "2026-07-01T00:00:00Z");
    clock.advance(chrono::Duration::days(1));
    reconcile_liveness_for_hashes_in_transaction(
        &mut conn,
        std::slice::from_ref(&affected),
        &clock,
    )
    .await
    .unwrap();
    let second_unreferenced: String =
        sqlx::query_scalar("SELECT unreferenced_at FROM blob_lifecycle WHERE sha256 = ?")
            .bind(&affected)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(first_unreferenced, second_unreferenced);

    sqlx::query(
        "UPDATE task_attachments SET deleted = 0, deleted_at = NULL
             WHERE attachment_id = '0000000000000011'",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    reconcile_liveness(&mut conn, &clock).await.unwrap();
    let restored: Option<String> =
        sqlx::query_scalar("SELECT unreferenced_at FROM blob_lifecycle WHERE sha256 = ?")
            .bind(&affected)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(restored, None);
    let full_updates: i64 = sqlx::query_scalar("SELECT count(*) FROM lifecycle_updates")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(full_updates, 3, "the full reset writes only the transition");
}

#[tokio::test]
async fn lease_protects_expired_unreferenced_blob_until_release() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.sqlite");
    let pool = open_db(&db_path).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let blob_dir = temp.path().join("blobs");
    let hash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    upsert_inventory_available(&mut conn, hash, 4, "image/png")
        .await
        .unwrap();
    let path = object_path(&blob_dir, hash).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"blob").unwrap();
    let clock = TestClock::at("2026-07-10T00:00:00Z");
    reconcile_liveness(&mut conn, &clock).await.unwrap();
    clock.advance(chrono::Duration::days(8));
    let lease = acquire_lease(&mut conn, hash, "backup", &clock)
        .await
        .unwrap();
    let policy = LifecyclePolicy::default();
    let blocked = prune(&mut conn, &blob_dir, policy, true, &clock)
        .await
        .unwrap();
    assert_eq!(blocked.pruned.count, 0);
    assert!(path.exists());

    release_lease(&mut conn, &lease).await.unwrap();
    let pruned = prune(&mut conn, &blob_dir, policy, true, &clock)
        .await
        .unwrap();
    assert_eq!(pruned.pruned.count, 1);
    assert!(!path.exists());
}

#[tokio::test]
async fn quota_is_unique_by_hash_and_reservations_are_workspace_scoped() {
    let temp = tempfile::tempdir().unwrap();
    let pool = open_db(&temp.path().join("test.sqlite")).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let hash = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    let clock = TestClock::at("2026-07-01T00:00:00Z");
    let first = reserve_upload(&mut conn, "workspace-a", hash, 8, 8, &clock)
        .await
        .unwrap();
    assert!(first.is_some());
    let replacement = reserve_upload(&mut conn, "workspace-a", hash, 8, 8, &clock)
        .await
        .unwrap();
    assert!(replacement.is_some());
    let other = reserve_upload(&mut conn, "workspace-b", hash, 8, 8, &clock)
        .await
        .unwrap();
    assert!(other.is_some());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM blob_upload_reservations")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn concurrent_attach_wins_prune_recheck() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.sqlite");
    let pool = open_db(&db_path).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let blob_dir = temp.path().join("blobs");
    let hash = "abababababababababababababababababababababababababababababababab";
    upsert_inventory_available(&mut conn, hash, 4, "image/png")
        .await
        .unwrap();
    insert_task(&mut conn, "0000000000000003").await;
    let path = object_path(&blob_dir, hash).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"blob").unwrap();
    let clock = TestClock::at("2026-07-01T00:00:00Z");
    reconcile_liveness(&mut conn, &clock).await.unwrap();
    clock.advance(chrono::Duration::days(8));
    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .busy_timeout(Duration::from_secs(5));
    let prune_conn = SqliteConnection::connect_with(&options).await.unwrap();

    let mut tx = begin_immediate(&mut conn).await.unwrap();
    insert_attachment(&mut tx, "0000000000000013", "0000000000000003", hash, false).await;
    let prune_dir = blob_dir.clone();
    let prune_clock = clock.clone();
    let pruning = tokio::spawn(async move {
        let mut prune_conn = prune_conn;
        prune(
            &mut prune_conn,
            &prune_dir,
            LifecyclePolicy::default(),
            true,
            &prune_clock,
        )
        .await
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.commit().await.unwrap();

    let summary = pruning.await.unwrap();
    assert_eq!(summary.pruned.count, 0);
    assert!(path.exists());
}

#[tokio::test]
async fn accepted_server_reference_keeps_blob_live() {
    let temp = tempfile::tempdir().unwrap();
    let pool = open_db(&temp.path().join("test.sqlite")).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let hash = "acacacacacacacacacacacacacacacacacacacacacacacacacacacacacacacac";
    upsert_inventory_available(&mut conn, hash, 4, "image/png")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO server_blob_references(
               workspace_id, attachment_id, task_id, sha256, byte_size
             ) VALUES ('workspace', '0000000000000042', '0000000000000004', ?, 4)",
    )
    .bind(hash)
    .execute(&mut *conn)
    .await
    .unwrap();
    let clock = TestClock::at("2026-07-01T00:00:00Z");

    reconcile_liveness(&mut conn, &clock).await.unwrap();
    let unreferenced_at: Option<String> =
        sqlx::query_scalar("SELECT unreferenced_at FROM blob_lifecycle WHERE sha256 = ?")
            .bind(hash)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(unreferenced_at, None);
}

#[tokio::test]
async fn local_quota_boundary_is_hash_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.sqlite");
    let pool = open_db(&db_path).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let blob_dir = temp.path().join("blobs");
    let existing = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let new_hash = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    upsert_inventory_available(&mut conn, existing, 8, "image/png")
        .await
        .unwrap();
    let clock = TestClock::at("2026-07-01T00:00:00Z");
    let policy = LifecyclePolicy {
        quota_bytes: 8,
        ..LifecyclePolicy::default()
    };

    ensure_local_capacity(&mut conn, &blob_dir, existing, 8, policy, &clock)
        .await
        .unwrap();
    let error = ensure_local_capacity(&mut conn, &blob_dir, new_hash, 1, policy, &clock)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "error attachment-quota-exceeded");
}

#[tokio::test]
async fn unavailable_directory_releases_cursor_owned_by_lexical_path() {
    let temp = tempfile::tempdir().unwrap();
    let scan_root = temp.path().join("scan");
    let lexical_root = temp.path().join("anchor").join("..").join("scan");
    fs::create_dir_all(temp.path().join("anchor")).unwrap();
    fs::create_dir_all(&scan_root).unwrap();
    fs::write(scan_root.join("first"), b"first").unwrap();
    fs::write(scan_root.join("second"), b"second").unwrap();
    let canonical_root = fs::canonicalize(&lexical_root).unwrap();

    scan_directory_page(lexical_root.clone(), 1).await.unwrap();
    assert!(
        DIRECTORY_CURSORS
            .lock()
            .unwrap()
            .contains_key(&lexical_root)
    );

    fs::remove_dir_all(&scan_root).unwrap();
    let _ = scan_directory_page(lexical_root.clone(), 1).await;

    let cursors = DIRECTORY_CURSORS.lock().unwrap();
    assert!(!cursors.contains_key(&lexical_root));
    assert!(!cursors.contains_key(&canonical_root));
}

#[tokio::test]
async fn directory_iteration_error_releases_cursor_state() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("scan");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("first"), b"first").unwrap();
    fs::write(dir.join("second"), b"second").unwrap();

    scan_directory_page(dir.clone(), 1).await.unwrap();
    assert!(DIRECTORY_CURSORS.lock().unwrap().contains_key(&dir));

    let result = scan_directory_page_with(dir.clone(), 1, |_| {
        Err(io::Error::other("injected directory iteration failure"))
    })
    .await;
    assert!(result.as_ref().is_err_and(|error| {
        error
            .to_string()
            .contains("injected directory iteration failure")
    }));
    assert!(!DIRECTORY_CURSORS.lock().unwrap().contains_key(&dir));
}

#[test]
fn file_move_copies_atomically_when_hard_links_are_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    fs::write(&source, b"attachment").unwrap();

    let moved = move_file_without_replacing_with(&source, &target, |_, _| {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "injected hard-link failure",
        ))
    })
    .unwrap();

    assert_eq!(moved, FileMove::Moved);
    assert!(!source.exists());
    assert_eq!(fs::read(target).unwrap(), b"attachment");
}

#[test]
fn file_move_copy_fallback_does_not_replace_target() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    fs::write(&source, b"source").unwrap();
    fs::write(&target, b"target").unwrap();

    let moved = move_file_without_replacing_with(&source, &target, |_, _| {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "injected hard-link failure",
        ))
    })
    .unwrap();

    assert_eq!(moved, FileMove::TargetExists);
    assert_eq!(fs::read(source).unwrap(), b"source");
    assert_eq!(fs::read(target).unwrap(), b"target");
}

#[tokio::test]
async fn interrupted_atomic_create_object_is_reconciled_after_grace() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.sqlite");
    let pool = open_db(&db_path).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let blob_dir = temp.path().join("blobs");
    let hash = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
    let path = object_path(&blob_dir, hash).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"orphan").unwrap();
    let clock = TestClock::at("2030-07-01T00:00:00Z");
    let policy = LifecyclePolicy {
        grace: Duration::ZERO,
        ..LifecyclePolicy::default()
    };

    prune(&mut conn, &blob_dir, policy, true, &clock)
        .await
        .unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn orphan_traversal_obeys_limit_and_resumes_between_steps() {
    let temp = tempfile::tempdir().unwrap();
    let pool = open_db(&temp.path().join("test.sqlite")).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let blob_dir = temp.path().join("blobs");
    for digit in ['6', '7', '8', '9', 'a'] {
        let hash = digit.to_string().repeat(64);
        let path = object_path(&blob_dir, &hash).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"orphan").unwrap();
    }
    let clock = TestClock::at("2030-07-01T00:00:00Z");
    let policy = LifecyclePolicy {
        grace: Duration::ZERO,
        maintenance_limit: 2,
        ..LifecyclePolicy::default()
    };

    prune(&mut conn, &blob_dir, policy, true, &clock)
        .await
        .unwrap();
    let after_one = fs::read_dir(staging_dir(&blob_dir)).unwrap().count();
    assert!(after_one >= 3);

    for _ in 0..4 {
        prune(&mut conn, &blob_dir, policy, true, &clock)
            .await
            .unwrap();
    }
    assert_eq!(fs::read_dir(staging_dir(&blob_dir)).unwrap().count(), 0);
}

#[tokio::test]
async fn interrupted_trash_move_restores_available_object() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("test.sqlite");
    let pool = open_db(&db_path).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let blob_dir = temp.path().join("blobs");
    let hash = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    upsert_inventory_available(&mut conn, hash, 4, "image/png")
        .await
        .unwrap();
    let source = object_path(&blob_dir, hash).unwrap();
    let trash = trash_dir(&blob_dir).join(hash);
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::create_dir_all(trash.parent().unwrap()).unwrap();
    fs::write(&source, b"blob").unwrap();
    fs::rename(&source, &trash).unwrap();

    reconcile_trash(&mut conn, &blob_dir).await.unwrap();
    assert!(source.exists());
    assert!(!trash.exists());
}
