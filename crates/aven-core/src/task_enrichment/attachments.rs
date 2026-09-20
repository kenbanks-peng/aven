use crate::attachments::AttachmentBytesState;
use crate::ids::{TaskId, WorkspaceId};
use crate::query::AttachmentMetadata;
use anyhow::Result;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection};
use std::collections::HashMap;

pub(super) async fn live_attachment_counts_for_tasks(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    task_ids: &[TaskId],
) -> Result<HashMap<TaskId, u32>> {
    let mut counts = HashMap::new();
    for chunk in task_ids.chunks(super::SQLITE_BIND_CHUNK_SIZE) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT task_id, count(*) AS attachment_count FROM task_attachments
             WHERE deleted = 0 AND workspace_id = ",
        );
        query.push_bind(workspace_id);
        query.push(" AND task_id IN (");
        let mut separated = query.separated(", ");
        for task_id in chunk {
            separated.push_bind(task_id);
        }
        query.push(") GROUP BY task_id");
        for row in query.build().fetch_all(&mut *conn).await? {
            let count = row.get::<i64, _>("attachment_count");
            counts.insert(
                row.get("task_id"),
                count.clamp(0, i64::from(u32::MAX)) as u32,
            );
        }
    }
    Ok(counts)
}

pub(super) async fn attachments_for_tasks(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    task_ids: &[TaskId],
) -> Result<HashMap<TaskId, Vec<AttachmentMetadata>>> {
    let mut attachments_by_task = HashMap::new();
    if task_ids.is_empty() {
        return Ok(attachments_by_task);
    }
    for chunk in task_ids.chunks(super::SQLITE_BIND_CHUNK_SIZE) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT ta.attachment_id, ta.task_id, ta.sha256, ta.media_type, ta.byte_size,
                    ta.filename, ta.alt_text, ta.width, ta.height, ta.created_at,
                    ta.deleted, ta.deleted_at,
                    CASE WHEN bi.sha256 IS NULL THEN 0 ELSE 1 END AS has_inventory,
                    COALESCE(bi.available, 0) AS has_blob
             FROM task_attachments ta
             LEFT JOIN blob_inventory bi ON bi.sha256 = ta.sha256
             WHERE ta.workspace_id =",
        );
        query.push_bind(workspace_id);
        query.push(" AND ta.task_id IN (");
        {
            let mut separated = query.separated(", ");
            for task_id in chunk {
                separated.push_bind(task_id);
            }
        }
        query.push(") AND ta.deleted = 0 ORDER BY ta.task_id, ta.created_at, ta.attachment_id");

        for row in query.build().fetch_all(&mut *conn).await? {
            let task_id: TaskId = row.get("task_id");
            let has_blob = row.get::<i64, _>("has_blob") != 0;
            let bytes_state = if has_blob {
                AttachmentBytesState::Present
            } else if row.get::<i64, _>("has_inventory") != 0 {
                AttachmentBytesState::Unavailable
            } else {
                AttachmentBytesState::PendingDownload
            };
            attachments_by_task
                .entry(task_id.clone())
                .or_insert_with(Vec::new)
                .push(AttachmentMetadata {
                    attachment_id: row.get("attachment_id"),
                    task_id: task_id.to_string(),
                    sha256: row.get("sha256"),
                    media_type: row.get("media_type"),
                    byte_size: row.get("byte_size"),
                    filename: row.get("filename"),
                    alt_text: row.get("alt_text"),
                    width: row.get("width"),
                    height: row.get("height"),
                    created_at: row.get("created_at"),
                    deleted: row.get::<i64, _>("deleted") != 0,
                    deleted_at: row.get("deleted_at"),
                    bytes_state,
                    has_blob,
                });
        }
    }
    Ok(attachments_by_task)
}
