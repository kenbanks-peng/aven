use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection};

use crate::ids::{TaskId, WorkspaceId};
use crate::projects::resolve_existing_project_in_workspace;
use crate::recurrence::RecurrenceSeriesId;
use crate::workspaces::Workspace;

use super::{ConflictDetail, ConflictListItem};

pub(super) async fn list_conflicts(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    project_key: Option<&str>,
    field: Option<&str>,
) -> Result<Vec<ConflictListItem>> {
    let workspace_id = &workspace.id;
    let project_id = if let Some(project) = project_key {
        Some(
            resolve_existing_project_in_workspace(conn, workspace_id, project)
                .await?
                .id,
        )
    } else {
        None
    };
    let rows = sqlx::query(
        r#"SELECT c.entity_id AS task_id, c.entity_type, c.field, c.variant_a, c.variant_b,
                 t.title, p.prefix, p.key AS project_key
                 FROM conflicts c
                 JOIN tasks t ON c.entity_type = 'task' AND t.workspace_id = c.workspace_id AND t.id = c.entity_id
                 JOIN projects p ON p.workspace_id = t.workspace_id AND p.id = t.project_id
                 WHERE c.workspace_id = ? AND c.resolved = 0
                 AND (? IS NULL OR t.project_id = ?)
                 AND (? IS NULL OR c.field = ?)
                 UNION ALL
                 SELECT c.entity_id AS task_id, c.entity_type, c.field, c.variant_a, c.variant_b,
                 s.title, p.prefix, p.key AS project_key
                 FROM conflicts c
                 JOIN recurrence_series s ON c.entity_type = 'recurrence_series'
                    AND s.workspace_id = c.workspace_id AND s.id = c.entity_id
                 JOIN projects p ON p.workspace_id = s.workspace_id AND p.id = s.project_id
                 WHERE c.workspace_id = ? AND c.resolved = 0
                 AND (? IS NULL OR s.project_id = ?)
                 AND (? IS NULL OR c.field = ?)
                 ORDER BY field"#,
    )
    .bind(workspace_id)
    .bind(&project_id)
    .bind(&project_id)
    .bind(field)
    .bind(field)
    .bind(workspace_id)
    .bind(&project_id)
    .bind(&project_id)
    .bind(field)
    .bind(field)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ConflictListItem {
            task_id: row.get("task_id"),
            recurrence_series: row.get::<String, _>("entity_type") == "recurrence_series",
            title: row.get("title"),
            project_key: row.get("project_key"),
            project_prefix: row.get("prefix"),
            field: row.get("field"),
            variant_a: row.get("variant_a"),
            variant_b: row.get("variant_b"),
        })
        .collect())
}

pub(super) async fn task_conflicts(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    task_id: &crate::ids::TaskId,
    field: Option<&str>,
) -> Result<Vec<ConflictDetail>> {
    let workspace_id = &workspace.id;
    let rows = sqlx::query(
        r#"SELECT field, variant_a, local_value, variant_b, remote_value
         FROM conflicts
         WHERE workspace_id = ? AND task_id = ? AND resolved = 0 AND (? IS NULL OR field = ?)
         ORDER BY field, id"#,
    )
    .bind(workspace_id)
    .bind(task_id)
    .bind(field)
    .bind(field)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ConflictDetail {
            field: row.get("field"),
            variant_a: row.get("variant_a"),
            local_value: row.get("local_value"),
            variant_b: row.get("variant_b"),
            remote_value: row.get("remote_value"),
        })
        .collect())
}

const SQLITE_BIND_BUDGET: usize = 900;
const CONFLICT_CANDIDATE_BIND_COUNT: usize = 2;
pub(super) const CONFLICT_CANDIDATE_CHUNK_SIZE: usize =
    (SQLITE_BIND_BUDGET - 1) / CONFLICT_CANDIDATE_BIND_COUNT;

pub(super) async fn unresolved_task_conflict_fields(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    candidates: &[(TaskId, String)],
) -> Result<HashMap<TaskId, HashSet<String>>> {
    let mut unique_candidates = HashSet::new();
    let candidates = candidates
        .iter()
        .filter(|candidate| unique_candidates.insert((*candidate).clone()))
        .collect::<Vec<_>>();
    let mut fields_by_task = HashMap::new();
    for candidates in candidates.chunks(CONFLICT_CANDIDATE_CHUNK_SIZE) {
        let mut query = unresolved_task_conflict_query(workspace_id, candidates, false);
        for row in query.build().fetch_all(&mut *conn).await? {
            fields_by_task
                .entry(row.get("task_id"))
                .or_insert_with(HashSet::new)
                .insert(row.get("field"));
        }
    }
    Ok(fields_by_task)
}

pub(super) fn unresolved_task_conflict_query(
    workspace_id: &WorkspaceId,
    candidates: &[&(TaskId, String)],
    explain: bool,
) -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::<Sqlite>::new(if explain {
        "EXPLAIN QUERY PLAN WITH candidates(task_id, field) AS (VALUES "
    } else {
        "WITH candidates(task_id, field) AS (VALUES "
    });
    {
        let mut separated = query.separated(", ");
        for (task_id, field) in candidates {
            separated
                .push("(")
                .push_bind_unseparated(task_id)
                .push_unseparated(", ")
                .push_bind_unseparated(field)
                .push_unseparated(")");
        }
    }
    query.push(
        ") SELECT DISTINCT candidate.task_id, candidate.field
         FROM candidates candidate
         WHERE EXISTS (
             SELECT 1 FROM conflicts c INDEXED BY sqlite_autoindex_conflicts_1
             WHERE c.workspace_id = ",
    );
    query.push_bind(workspace_id);
    query.push(
        " AND c.entity_type = 'task'
           AND c.entity_id = candidate.task_id
           AND c.task_id = c.entity_id
           AND c.field = candidate.field
           AND c.resolved = 0
         )",
    );
    query
}

pub(super) async fn recurrence_series_conflicts(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    series_id: &RecurrenceSeriesId,
    field: Option<&str>,
) -> Result<Vec<ConflictDetail>> {
    let rows = sqlx::query(
        "SELECT field, variant_a, local_value, variant_b, remote_value FROM conflicts
         WHERE workspace_id = ? AND entity_type = 'recurrence_series' AND entity_id = ?
           AND resolved = 0 AND (? IS NULL OR field = ?) ORDER BY field, id",
    )
    .bind(&workspace.id)
    .bind(series_id)
    .bind(field)
    .bind(field)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ConflictDetail {
            field: row.get("field"),
            variant_a: row.get("variant_a"),
            local_value: row.get("local_value"),
            variant_b: row.get("variant_b"),
            remote_value: row.get("remote_value"),
        })
        .collect())
}

pub(super) async fn conflict_variant_value(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    task_id: &crate::ids::TaskId,
    field: &str,
    token: &str,
) -> Result<String> {
    for detail in task_conflicts(conn, workspace, task_id, Some(field)).await? {
        if token == detail.variant_a {
            return Ok(detail.local_value);
        }
        if token == detail.variant_b {
            return Ok(detail.remote_value);
        }
    }
    bail!("error unknown-variant token={token}")
}
