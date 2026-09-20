use crate::ids::{TaskId, WorkspaceId};
use crate::query::TaskNote;
use anyhow::Result;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection};
use std::collections::{HashMap, HashSet};

pub(super) async fn notes_for_tasks(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    task_ids: &[TaskId],
) -> Result<HashMap<TaskId, Vec<TaskNote>>> {
    let mut notes_by_task = HashMap::new();
    if task_ids.is_empty() {
        return Ok(notes_by_task);
    }
    for chunk in task_ids.chunks(super::SQLITE_BIND_CHUNK_SIZE) {
        if chunk.is_empty() {
            continue;
        }
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT task_id, id, body, created_at FROM notes WHERE workspace_id = ",
        );
        query.push_bind(workspace_id);
        query.push(" AND task_id IN (");
        {
            let mut separated = query.separated(", ");
            for task_id in chunk {
                separated.push_bind(task_id);
            }
        }
        query.push(") ORDER BY task_id, created_at DESC, id DESC");

        for row in query.build().fetch_all(&mut *conn).await? {
            let task_id: TaskId = row.get("task_id");
            let note = TaskNote {
                id: row.get("id"),
                body: row.get("body"),
                created_at: row.get("created_at"),
            };
            notes_by_task
                .entry(task_id)
                .or_insert_with(Vec::new)
                .push(note);
        }
    }
    Ok(notes_by_task)
}

pub(super) async fn task_ids_with_notes(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    task_ids: &[TaskId],
) -> Result<HashSet<TaskId>> {
    let mut task_ids_with_notes = HashSet::new();
    if task_ids.is_empty() {
        return Ok(task_ids_with_notes);
    }
    for chunk in task_ids.chunks(super::SQLITE_BIND_CHUNK_SIZE) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT DISTINCT task_id
             FROM notes WHERE workspace_id = ",
        );
        query.push_bind(workspace_id);
        query.push(" AND task_id IN (");
        {
            let mut separated = query.separated(", ");
            for task_id in chunk {
                separated.push_bind(task_id);
            }
        }
        query.push(")");

        for row in query.build().fetch_all(&mut *conn).await? {
            task_ids_with_notes.insert(row.get("task_id"));
        }
    }
    Ok(task_ids_with_notes)
}
