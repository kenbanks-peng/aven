use crate::ids::{TaskId, WorkspaceId};
use crate::query::TaskDependencyLink;
use crate::query::fragments;
use crate::refs::DisplayRefContext;
use anyhow::Result;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection};
use std::collections::HashMap;

pub(super) async fn unresolved_blocker_counts_for_tasks(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    task_ids: &[TaskId],
) -> Result<HashMap<TaskId, i64>> {
    let mut counts = HashMap::new();
    if task_ids.is_empty() {
        return Ok(counts);
    }
    for chunk in task_ids.chunks(super::SQLITE_BIND_CHUNK_SIZE) {
        if chunk.is_empty() {
            continue;
        }
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT d.task_id, COUNT(*) AS blockers
             FROM task_dependencies d
             JOIN tasks blocker
              ON blocker.workspace_id = d.workspace_id AND blocker.id = d.depends_on_task_id
             WHERE d.workspace_id = ",
        );
        query.push_bind(workspace_id);
        query.push(" AND d.task_id IN (");
        {
            let mut separated = query.separated(", ");
            for task_id in chunk {
                separated.push_bind(task_id);
            }
        }
        query.push(format!(
            ") AND {} GROUP BY d.task_id",
            fragments::open_task_clause("blocker"),
        ));

        for row in query.build().fetch_all(&mut *conn).await? {
            counts.insert(row.get("task_id"), row.get::<i64, _>("blockers"));
        }
    }
    Ok(counts)
}

pub(super) async fn dependent_counts_for_tasks(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    task_ids: &[TaskId],
) -> Result<HashMap<TaskId, i64>> {
    let mut counts = HashMap::new();
    if task_ids.is_empty() {
        return Ok(counts);
    }
    for chunk in task_ids.chunks(super::SQLITE_BIND_CHUNK_SIZE) {
        if chunk.is_empty() {
            continue;
        }
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT d.depends_on_task_id, COUNT(*) AS dependents
             FROM task_dependencies d
             JOIN tasks blocker
              ON blocker.workspace_id = d.workspace_id AND blocker.id = d.depends_on_task_id
             JOIN tasks dependent
              ON dependent.workspace_id = d.workspace_id AND dependent.id = d.task_id
             WHERE d.workspace_id = ",
        );
        query.push_bind(workspace_id);
        query.push(" AND d.depends_on_task_id IN (");
        {
            let mut separated = query.separated(", ");
            for task_id in chunk {
                separated.push_bind(task_id);
            }
        }
        query.push(format!(
            ") AND {} AND {} GROUP BY d.depends_on_task_id",
            fragments::open_task_clause("blocker"),
            fragments::open_task_clause("dependent"),
        ));

        for row in query.build().fetch_all(&mut *conn).await? {
            counts.insert(
                row.get("depends_on_task_id"),
                row.get::<i64, _>("dependents"),
            );
        }
    }
    Ok(counts)
}

pub(super) async fn dependency_links_for_tasks(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    task_ids: &[TaskId],
    blocks_only: bool,
    display_refs: &DisplayRefContext,
) -> Result<HashMap<TaskId, Vec<TaskDependencyLink>>> {
    let mut links = HashMap::new();
    if task_ids.is_empty() {
        return Ok(links);
    }
    for chunk in task_ids.chunks(super::SQLITE_BIND_CHUNK_SIZE) {
        if chunk.is_empty() {
            continue;
        }
        let initial = if blocks_only {
            format!(
                "SELECT d.depends_on_task_id AS source_task_id,
                        t.id, t.title, t.status, t.priority, p.key AS project_key, p.prefix AS project_prefix,
                        d.created_at AS dependency_created_at,
                        CASE
                            WHEN {}
                             AND {}
                            THEN 1 ELSE 0
                        END AS unresolved
                 FROM task_dependencies d
                 JOIN tasks blocker
                  ON blocker.workspace_id = d.workspace_id AND blocker.id = d.depends_on_task_id
                 JOIN tasks t
                  ON t.workspace_id = d.workspace_id AND t.id = d.task_id
                 JOIN projects p
                  ON p.workspace_id = t.workspace_id AND p.id = t.project_id
                 WHERE d.workspace_id =",
                fragments::open_task_clause("blocker"),
                fragments::open_task_clause("t"),
            )
        } else {
            format!(
                "SELECT d.task_id AS source_task_id,
                        t.id, t.title, t.status, t.priority, p.key AS project_key, p.prefix AS project_prefix,
                        d.created_at AS dependency_created_at,
                        CASE
                            WHEN {}
                            THEN 1 ELSE 0
                        END AS unresolved
                 FROM task_dependencies d
                 JOIN tasks t
                  ON t.workspace_id = d.workspace_id AND t.id = d.depends_on_task_id
                 JOIN projects p
                  ON p.workspace_id = t.workspace_id AND p.id = t.project_id
                 WHERE d.workspace_id =",
                fragments::open_task_clause("t"),
            )
        };
        let mut query = QueryBuilder::<Sqlite>::new(&initial);
        query.push_bind(workspace_id);
        let source_column = if blocks_only {
            "d.depends_on_task_id"
        } else {
            "d.task_id"
        };
        query.push(" AND ");
        query.push(source_column);
        query.push(" IN (");
        {
            let mut separated = query.separated(", ");
            for task_id in chunk {
                separated.push_bind(task_id);
            }
        }
        query.push(") ORDER BY unresolved DESC, t.status, t.title, d.created_at, t.id");

        for row in query.build().fetch_all(&mut *conn).await? {
            let source_task_id: TaskId = row.get("source_task_id");
            links.entry(source_task_id).or_insert_with(Vec::new).push(
                super::dependency_link_from_row(&row, workspace_id, display_refs),
            );
        }
    }
    Ok(links)
}
