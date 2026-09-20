use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use sqlx::SqliteConnection;

use super::Clock;

#[derive(Clone)]
pub(super) struct TestClock(Arc<Mutex<DateTime<Utc>>>);

impl TestClock {
    pub(super) fn at(value: &str) -> Self {
        Self(Arc::new(Mutex::new(
            DateTime::parse_from_rfc3339(value).unwrap().to_utc(),
        )))
    }

    pub(super) fn advance(&self, duration: chrono::Duration) {
        let mut now = self.0.lock().unwrap();
        *now += duration;
    }
}

impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

pub(super) async fn insert_task(conn: &mut SqliteConnection, task_id: &str) {
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

pub(super) async fn insert_attachment(
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
