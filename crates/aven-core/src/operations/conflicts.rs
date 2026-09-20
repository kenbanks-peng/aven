use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use sqlx::SqliteConnection;
#[cfg(test)]
use sqlx::{QueryBuilder, Sqlite};

use crate::db::Database;
use crate::ids::{TaskId, WorkspaceId};
use crate::recurrence::RecurrenceSeriesId;
use crate::types::Task;
use crate::workspaces::Workspace;

mod metadata;
mod reads;
mod recurrence;
mod task;

pub(crate) use recurrence::cleanup_recurrence_projections;
use recurrence::resolve_recurrence_conflict;
use task::resolve_conflict_value;

impl Database {
    pub async fn list_conflicts(
        &self,
        workspace: &Workspace,
        project_key: Option<&str>,
        field: Option<&str>,
    ) -> Result<Vec<ConflictListItem>> {
        let mut conn = self.acquire_reader().await?;
        list_conflicts(&mut conn, workspace, project_key, field).await
    }

    pub async fn task_conflicts(
        &self,
        workspace: &Workspace,
        task_id: &TaskId,
        field: Option<&str>,
    ) -> Result<Vec<ConflictDetail>> {
        let mut conn = self.acquire_reader().await?;
        task_conflicts(&mut conn, workspace, task_id, field).await
    }

    pub async fn unresolved_task_conflict_fields(
        &self,
        workspace_id: &WorkspaceId,
        candidates: &[(TaskId, String)],
    ) -> Result<HashMap<TaskId, HashSet<String>>> {
        let mut conn = self.acquire_reader().await?;
        unresolved_task_conflict_fields(&mut conn, workspace_id, candidates).await
    }

    pub async fn conflict_variant_value(
        &self,
        workspace: &Workspace,
        task_id: &TaskId,
        field: &str,
        token: &str,
    ) -> Result<String> {
        let mut conn = self.acquire_reader().await?;
        conflict_variant_value(&mut conn, workspace, task_id, field, token).await
    }

    pub async fn resolve_conflict_with_tui_undo(
        &self,
        workspace: &Workspace,
        task_id: &TaskId,
        field: &str,
        value: &str,
        summary: &str,
    ) -> Result<ConflictResolutionOutcome> {
        let mut conn = self.acquire_writer().await?;
        resolve_conflict_value(
            &mut conn,
            workspace,
            task_id,
            field,
            ConflictResolutionValue::Explicit(value),
            None,
            Some(summary),
        )
        .await
    }

    pub async fn resolve_conflict(
        &self,
        workspace: &Workspace,
        task_id: &TaskId,
        field: &str,
        value: &str,
    ) -> Result<ConflictOutcome> {
        let mut conn = self.acquire_writer().await?;
        resolve_conflict(&mut conn, workspace, task_id, field, value).await
    }

    pub async fn recurrence_series_conflicts(
        &self,
        workspace: &Workspace,
        series_id: &RecurrenceSeriesId,
        field: Option<&str>,
    ) -> Result<Vec<ConflictDetail>> {
        let mut conn = self.acquire_reader().await?;
        recurrence_series_conflicts(&mut conn, workspace, series_id, field).await
    }

    pub async fn recurrence_conflict_variant_value(
        &self,
        workspace: &Workspace,
        series_id: &RecurrenceSeriesId,
        field: &str,
        token: &str,
    ) -> Result<String> {
        let mut conn = self.acquire_reader().await?;
        for detail in
            recurrence_series_conflicts(&mut conn, workspace, series_id, Some(field)).await?
        {
            if token == detail.variant_a {
                return Ok(detail.local_value);
            }
            if token == detail.variant_b {
                return Ok(detail.remote_value);
            }
        }
        bail!("error unknown-variant token={token}")
    }

    pub async fn resolve_recurrence_conflict(
        &self,
        workspace: &Workspace,
        series_id: &RecurrenceSeriesId,
        field: &str,
        value: &str,
    ) -> Result<String> {
        let mut conn = self.acquire_writer().await?;
        resolve_recurrence_conflict(&mut conn, workspace, series_id, field, value).await
    }
}

pub struct ConflictListItem {
    pub task_id: TaskId,
    pub recurrence_series: bool,
    pub title: String,
    pub project_key: String,
    pub project_prefix: String,
    pub field: String,
    pub variant_a: String,
    pub variant_b: String,
}

pub struct ConflictDetail {
    pub field: String,
    pub variant_a: String,
    pub local_value: String,
    pub variant_b: String,
    pub remote_value: String,
}

pub struct ConflictOutcome {
    pub task: Task,
    pub field: String,
}

pub struct ConflictResolutionOutcome {
    pub outcome: ConflictOutcome,
    pub before: String,
    pub after: String,
    pub conflict_id: i64,
}

pub async fn list_conflicts(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    project_key: Option<&str>,
    field: Option<&str>,
) -> Result<Vec<ConflictListItem>> {
    reads::list_conflicts(conn, workspace, project_key, field).await
}

pub async fn task_conflicts(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    task_id: &TaskId,
    field: Option<&str>,
) -> Result<Vec<ConflictDetail>> {
    reads::task_conflicts(conn, workspace, task_id, field).await
}

#[cfg(test)]
const CONFLICT_CANDIDATE_CHUNK_SIZE: usize = reads::CONFLICT_CANDIDATE_CHUNK_SIZE;

async fn unresolved_task_conflict_fields(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    candidates: &[(TaskId, String)],
) -> Result<HashMap<TaskId, HashSet<String>>> {
    reads::unresolved_task_conflict_fields(conn, workspace_id, candidates).await
}

#[cfg(test)]
fn unresolved_task_conflict_query(
    workspace_id: &WorkspaceId,
    candidates: &[&(TaskId, String)],
    explain: bool,
) -> QueryBuilder<Sqlite> {
    reads::unresolved_task_conflict_query(workspace_id, candidates, explain)
}

pub async fn recurrence_series_conflicts(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    series_id: &RecurrenceSeriesId,
    field: Option<&str>,
) -> Result<Vec<ConflictDetail>> {
    reads::recurrence_series_conflicts(conn, workspace, series_id, field).await
}

pub async fn conflict_variant_value(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    task_id: &TaskId,
    field: &str,
    token: &str,
) -> Result<String> {
    reads::conflict_variant_value(conn, workspace, task_id, field, token).await
}

pub(crate) enum ConflictResolutionValue<'a> {
    Local,
    Remote,
    Explicit(&'a str),
}

pub(crate) struct ExpectedConflictIdentity<'a> {
    pub variant_a: &'a str,
    pub variant_b: &'a str,
}

pub async fn resolve_conflict(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    task_id: &crate::ids::TaskId,
    field: &str,
    value: &str,
) -> Result<ConflictOutcome> {
    Ok(resolve_conflict_value(
        conn,
        workspace,
        task_id,
        field,
        ConflictResolutionValue::Explicit(value),
        None,
        None,
    )
    .await?
    .outcome)
}

pub(crate) async fn resolve_conflict_transaction(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    task_id: &crate::ids::TaskId,
    field: &str,
    expected: ExpectedConflictIdentity<'_>,
    resolution: ConflictResolutionValue<'_>,
) -> Result<ConflictOutcome> {
    Ok(resolve_conflict_value(
        conn,
        workspace,
        task_id,
        field,
        resolution,
        Some(expected),
        None,
    )
    .await?
    .outcome)
}

#[cfg(test)]
mod tests {
    use sqlx::Row;

    use super::{
        CONFLICT_CANDIDATE_CHUNK_SIZE, TaskId, WorkspaceId, unresolved_task_conflict_fields,
        unresolved_task_conflict_query,
    };

    async fn insert_task_conflict(
        conn: &mut sqlx::SqliteConnection,
        workspace_id: &WorkspaceId,
        task_id: &TaskId,
        field: &str,
        remote_change_id: &str,
        resolved: bool,
    ) {
        sqlx::query(
            "INSERT INTO conflicts(
                 workspace_id, entity_type, entity_id, task_id, field,
                 local_value, remote_value, remote_change_id, variant_a,
                 variant_b, created_at, resolved
             ) VALUES (?, 'task', ?, ?, ?, 'local', 'remote', ?, 'a', 'b', 't', ?)",
        )
        .bind(workspace_id)
        .bind(task_id)
        .bind(task_id)
        .bind(field)
        .bind(remote_change_id)
        .bind(resolved)
        .execute(conn)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn candidate_conflicts_filter_by_field_and_workspace() {
        let (_temp, mut conn) = crate::test_support::test_conn().await;
        let workspace_id: WorkspaceId = "0000000000000000".parse().unwrap();
        let other_workspace_id: WorkspaceId = "0000000000000001".parse().unwrap();
        let task_id: TaskId = "0000000000001000".parse().unwrap();

        insert_task_conflict(
            &mut conn,
            &workspace_id,
            &task_id,
            "priority",
            "priority-a",
            false,
        )
        .await;
        insert_task_conflict(
            &mut conn,
            &workspace_id,
            &task_id,
            "labels",
            "resolved-labels",
            true,
        )
        .await;
        insert_task_conflict(
            &mut conn,
            &other_workspace_id,
            &task_id,
            "status",
            "other-workspace",
            false,
        )
        .await;
        sqlx::query(
            "WITH RECURSIVE seq(i) AS (
                 VALUES(0) UNION ALL SELECT i + 1 FROM seq WHERE i < 1999
             )
             INSERT INTO conflicts(
                 workspace_id, entity_type, entity_id, task_id, field,
                 local_value, remote_value, remote_change_id, variant_a,
                 variant_b, created_at
             )
             SELECT ?, 'task', ?, ?, printf('metadata:irrelevant-%04d', i),
                    'local', 'remote', printf('irrelevant-%04d', i), 'a', 'b', 't'
             FROM seq",
        )
        .bind(&workspace_id)
        .bind(&task_id)
        .bind(&task_id)
        .execute(&mut *conn)
        .await
        .unwrap();

        let candidates = vec![
            (task_id.clone(), "priority".to_string()),
            (task_id.clone(), "labels".to_string()),
            (task_id.clone(), "status".to_string()),
        ];
        let conflicts = unresolved_task_conflict_fields(&mut conn, &workspace_id, &candidates)
            .await
            .unwrap();

        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts.get(&task_id).unwrap(),
            &std::collections::HashSet::from(["priority".to_string()])
        );
    }

    #[tokio::test]
    async fn candidate_conflicts_chunk_below_the_sqlite_bind_limit() {
        let (_temp, mut conn) = crate::test_support::test_conn().await;
        let workspace_id: WorkspaceId = "0000000000000000".parse().unwrap();
        let candidates = (0..=CONFLICT_CANDIDATE_CHUNK_SIZE)
            .map(|index| {
                (
                    format!("{index:016X}").parse::<TaskId>().unwrap(),
                    "priority".to_string(),
                )
            })
            .collect::<Vec<_>>();
        for index in [0, candidates.len() - 1] {
            insert_task_conflict(
                &mut conn,
                &workspace_id,
                &candidates[index].0,
                "priority",
                &format!("chunk-{index}"),
                false,
            )
            .await;
        }

        let conflicts = unresolved_task_conflict_fields(&mut conn, &workspace_id, &candidates)
            .await
            .unwrap();

        assert_eq!(conflicts.len(), 2);
        assert!(conflicts.contains_key(&candidates[0].0));
        assert!(conflicts.contains_key(&candidates.last().unwrap().0));
    }

    #[tokio::test]
    async fn candidate_conflict_plan_uses_exact_field_identity_lookups() {
        let (_temp, mut conn) = crate::test_support::test_conn().await;
        let workspace_id: WorkspaceId = "0000000000000000".parse().unwrap();
        let candidates = (0..64)
            .map(|index| {
                (
                    format!("{index:016X}").parse::<TaskId>().unwrap(),
                    "priority".to_string(),
                )
            })
            .collect::<Vec<_>>();
        let candidate_refs = candidates.iter().collect::<Vec<_>>();
        let mut query = unresolved_task_conflict_query(&workspace_id, &candidate_refs, true);
        let plan = query
            .build()
            .fetch_all(&mut *conn)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            plan.contains("sqlite_autoindex_conflicts_1")
                && plan.contains("workspace_id=?")
                && plan.contains("entity_type=?")
                && plan.contains("entity_id=?")
                && plan.contains("field=?"),
            "unexpected candidate conflict plan:\n{plan}"
        );
    }
}
