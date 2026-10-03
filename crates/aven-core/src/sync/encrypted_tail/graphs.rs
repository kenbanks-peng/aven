use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};
use sqlx::SqliteConnection;

use crate::change_log::op_type;
use crate::data_safety::TaskEpicLinkRow;
use crate::ids::{TaskId, WorkspaceId};
use crate::sync::wire::MoveTasksPayload;

pub(crate) async fn initialize(
    conn: &mut SqliteConnection,
    edges: &[TaskEpicLinkRow],
) -> Result<()> {
    for edge in edges {
        sqlx::query("INSERT INTO local_e2ee_epic_edges(workspace_id, child_task_id, epic_task_id, created_at) VALUES (?, ?, ?, ?)")
            .bind(&edge.workspace_id).bind(&edge.child_task_id).bind(&edge.epic_task_id).bind(&edge.created_at).execute(&mut *conn).await?;
    }
    Ok(())
}

pub(super) fn needs_reconcile(change: &crate::sync::wire::ChangeWire) -> bool {
    matches!(
        change.op_type.as_str(),
        op_type::MOVE_TASKS
            | op_type::DEPENDENCY_ADD
            | op_type::DEPENDENCY_REMOVE
            | op_type::EPIC_LINK_ADD
            | op_type::EPIC_LINK_REMOVE
    ) || matches!(
        change.op_type.as_str(),
        op_type::SET_FIELD | op_type::RESOLVE_FIELD
    ) && change.field.as_deref() == Some("is_epic")
}

pub(crate) async fn owns_graphs(conn: &mut SqliteConnection) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM changes WHERE op_type='move_tasks')")
            .fetch_one(conn)
            .await?,
    )
}

/// Both graph domains share the published baseline and one accepted/pending
/// order. Partitioning edges by moved endpoints would change cycle decisions.
pub(super) async fn reconcile(conn: &mut SqliteConnection, prefix: i64) -> Result<()> {
    if !owns_graphs(conn).await? {
        return Ok(());
    }
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT change_id FROM changes WHERE (server_seq > ? OR server_seq IS NULL)
           AND op_type IN ('move_tasks','dependency_add','dependency_remove','epic_link_add','epic_link_remove')
         ORDER BY server_seq IS NULL, server_seq, local_seq, created_at, change_id",
    ).bind(prefix).fetch_all(&mut *conn).await?;
    let mut changes = Vec::with_capacity(ids.len());
    let mut placements: HashMap<TaskId, WorkspaceId> =
        sqlx::query_as::<_, (TaskId, WorkspaceId)>("SELECT id, workspace_id FROM tasks")
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect();
    let mut first_move = HashSet::new();
    for id in ids {
        let change = super::client::load_change(conn, &id)
            .await?
            .context("error encrypted-graph-history")?;
        if change.op_type == op_type::MOVE_TASKS {
            let payload = MoveTasksPayload::from_change(&change)?;
            for task in payload.tasks {
                if first_move.insert(task.task_id.clone()) {
                    placements.insert(task.task_id, payload.source_workspace_id.clone());
                }
            }
        }
        changes.push(change);
    }
    let baseline: Vec<(WorkspaceId, TaskId, TaskId, String)> = sqlx::query_as(
        "SELECT workspace_id, task_id, depends_on_task_id, created_at FROM local_e2ee_dependency_edges",
    ).fetch_all(&mut *conn).await?;
    let epics: Vec<TaskEpicLinkRow> = sqlx::query_as(
        "SELECT workspace_id, child_task_id, epic_task_id, created_at FROM local_e2ee_epic_edges",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut membership: HashMap<TaskId, TaskEpicLinkRow> = epics
        .into_iter()
        .map(|edge| (edge.child_task_id.clone(), edge))
        .collect();
    sqlx::query("DELETE FROM task_dependencies")
        .execute(&mut *conn)
        .await?;
    for (workspace, task, other, created_at) in baseline {
        ensure!(
            placements.get(&task) == Some(&workspace) && placements.get(&other) == Some(&workspace),
            "error encrypted-dependency-baseline-reinitialization-required"
        );
        sqlx::query("INSERT INTO task_dependencies(workspace_id, task_id, depends_on_task_id, created_at) VALUES (?, ?, ?, ?)")
            .bind(workspace).bind(task).bind(other).bind(created_at).execute(&mut *conn).await?;
    }
    for change in changes {
        if change.op_type == op_type::MOVE_TASKS {
            let payload = MoveTasksPayload::from_change(&change)?;
            let members = payload
                .tasks
                .iter()
                .map(|task| task.task_id.clone())
                .collect::<HashSet<_>>();
            membership.retain(|child, edge| {
                !members.contains(child) && !members.contains(&edge.epic_task_id)
            });
            for task in members {
                placements.insert(task.clone(), payload.target_workspace_id.clone());
                sqlx::query(
                    "DELETE FROM task_dependencies WHERE task_id=? OR depends_on_task_id=?",
                )
                .bind(&task)
                .bind(&task)
                .execute(&mut *conn)
                .await?;
            }
            for edge in payload.dependencies {
                sqlx::query("INSERT INTO task_dependencies(workspace_id, task_id, depends_on_task_id, created_at) VALUES (?, ?, ?, ?)")
                    .bind(&payload.target_workspace_id).bind(edge.task_id).bind(edge.depends_on_task_id).bind(edge.created_at).execute(&mut *conn).await?;
            }
            for edge in payload.epics {
                membership.insert(
                    edge.child_task_id.clone(),
                    TaskEpicLinkRow {
                        workspace_id: payload.target_workspace_id.clone(),
                        child_task_id: edge.child_task_id,
                        epic_task_id: edge.epic_task_id,
                        created_at: edge.created_at,
                    },
                );
            }
            continue;
        }
        let task: TaskId = change.entity_id.parse()?;
        let workspace = placements
            .get(&task)
            .context("error encrypted-graph-task-missing")?;
        let dependency = matches!(
            change.op_type.as_str(),
            op_type::DEPENDENCY_ADD | op_type::DEPENDENCY_REMOVE
        );
        let key = if dependency {
            "depends_on_task_id"
        } else {
            "epic_task_id"
        };
        let other: TaskId = change.payload[key]
            .as_str()
            .context("error encrypted-graph-endpoint")?
            .parse()?;
        let other_workspace = placements
            .get(&other)
            .context("error encrypted-graph-task-missing")?;
        if workspace != other_workspace {
            continue;
        }
        if dependency {
            if change.op_type == op_type::DEPENDENCY_ADD {
                crate::sync::apply::apply_graph_add(conn, workspace, &task, &other, &change)
                    .await?;
            } else {
                sqlx::query("DELETE FROM task_dependencies WHERE workspace_id=? AND task_id=? AND depends_on_task_id=?")
                    .bind(workspace).bind(&task).bind(&other).execute(&mut *conn).await?;
            }
        } else if change.op_type == op_type::EPIC_LINK_ADD {
            if membership
                .get(&task)
                .is_none_or(|edge| other < edge.epic_task_id)
            {
                membership.insert(
                    task.clone(),
                    TaskEpicLinkRow {
                        workspace_id: workspace.clone(),
                        child_task_id: task,
                        epic_task_id: other,
                        created_at: change.payload["created_at"]
                            .as_str()
                            .unwrap_or(&change.created_at)
                            .into(),
                    },
                );
            }
        } else if membership
            .get(&task)
            .is_some_and(|edge| edge.epic_task_id == other)
        {
            membership.remove(&task);
        }
    }
    sqlx::query("DELETE FROM task_epic_links")
        .execute(&mut *conn)
        .await?;
    for edge in membership.into_values() {
        sqlx::query("INSERT INTO task_epic_links(workspace_id, child_task_id, epic_task_id, created_at) VALUES (?, ?, ?, ?)")
            .bind(&edge.workspace_id).bind(&edge.child_task_id).bind(&edge.epic_task_id).bind(edge.created_at).execute(&mut *conn).await?;
        sqlx::query("UPDATE tasks SET is_epic=1 WHERE id=? AND workspace_id=?")
            .bind(edge.epic_task_id)
            .bind(edge.workspace_id)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}
