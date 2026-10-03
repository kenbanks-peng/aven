use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use sqlx::SqliteConnection;

use crate::ids::{MetadataFieldId, TaskId, WorkspaceId};
use crate::sync::wire::{ChangeWire, MoveTasksPayload};
use crate::types::MutableEntityType;

pub(crate) async fn apply_move_tasks(
    conn: &mut SqliteConnection,
    change: &ChangeWire,
) -> Result<Vec<TaskId>> {
    apply_move(conn, change, true).await
}

pub(crate) async fn replay_move_tasks(
    conn: &mut SqliteConnection,
    change: &ChangeWire,
) -> Result<Vec<TaskId>> {
    apply_move(conn, change, false).await
}

async fn apply_move(
    conn: &mut SqliteConnection,
    change: &ChangeWire,
    graphs: bool,
) -> Result<Vec<TaskId>> {
    let payload = MoveTasksPayload::from_change(change)?;
    ensure_workspace(conn, &payload.target_workspace_id).await?;
    let project_id = super::project::ensure_project_for_payload(
        conn,
        &payload.target_workspace_id,
        &payload.project_id,
        change,
    )
    .await?;
    let members = payload
        .tasks
        .iter()
        .map(|task| task.task_id.clone())
        .collect::<HashSet<_>>();
    let target_fields = target_metadata_fields(conn, change, &payload).await?;

    for task in &payload.tasks {
        relocate_task(conn, change, &payload, &project_id, task, &target_fields).await?;
    }
    replace_relationships(conn, &payload, &members, graphs).await?;
    Ok(payload.tasks.into_iter().map(|task| task.task_id).collect())
}

async fn ensure_workspace(conn: &mut SqliteConnection, workspace_id: &WorkspaceId) -> Result<()> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspaces WHERE id = ?)")
        .bind(workspace_id)
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        bail!("error unknown-workspace-id id={workspace_id}");
    }
    Ok(())
}

async fn target_metadata_fields(
    conn: &mut SqliteConnection,
    change: &ChangeWire,
    payload: &MoveTasksPayload,
) -> Result<HashMap<String, MetadataFieldId>> {
    let mut fields = HashMap::new();
    for metadata in payload.tasks.iter().flat_map(|task| &task.metadata) {
        if fields.contains_key(&metadata.key) {
            continue;
        }
        let field = super::metadata::ensure_remote_field(
            conn,
            &payload.target_workspace_id,
            &metadata.field_id,
            &metadata.key,
            &change.created_at,
        )
        .await?;
        fields.insert(metadata.key.clone(), field.id);
    }
    for task in &payload.tasks {
        let context: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT m.id, m.key FROM metadata_fields m JOIN tasks t ON t.workspace_id = m.workspace_id
             WHERE t.id = ? AND EXISTS(SELECT 1 FROM conflicts c WHERE c.entity_type = 'task' AND c.entity_id = t.id AND c.field = 'metadata:' || m.id)")
            .bind(&task.task_id).fetch_all(&mut *conn).await?;
        for (id, key) in context {
            if fields.contains_key(&key) {
                continue;
            }
            let field = super::metadata::ensure_remote_field(
                conn,
                &payload.target_workspace_id,
                &id.parse()?,
                &key,
                &change.created_at,
            )
            .await?;
            fields.insert(key, field.id);
        }
    }
    Ok(fields)
}

async fn relocate_task(
    conn: &mut SqliteConnection,
    change: &ChangeWire,
    payload: &MoveTasksPayload,
    project_id: &crate::ids::ProjectId,
    snapshot: &crate::sync::wire::MoveTaskSnapshot,
    target_fields: &HashMap<String, MetadataFieldId>,
) -> Result<()> {
    let current_workspace: WorkspaceId =
        sqlx::query_scalar("SELECT workspace_id FROM tasks WHERE id = ?")
            .bind(&snapshot.task_id)
            .fetch_optional(&mut *conn)
            .await?
            .with_context(|| format!("error move-task-missing task_id={}", snapshot.task_id))?;
    if current_workspace != payload.source_workspace_id
        && current_workspace != payload.target_workspace_id
        && !workspace_is_historical(conn, &snapshot.task_id, &payload.source_workspace_id).await?
    {
        bail!(
            "error invalid-task-workspace task_id={} workspace_id={} task_workspace_id={}",
            snapshot.task_id,
            payload.source_workspace_id,
            current_workspace
        );
    }

    let conflicts: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, field FROM conflicts WHERE entity_type = 'task' AND entity_id = ?",
    )
    .bind(&snapshot.task_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut conflict_fields = Vec::with_capacity(conflicts.len());
    for (id, field) in conflicts {
        conflict_fields.push((
            id,
            remap_metadata_identity(conn, &current_workspace, &field, target_fields).await?,
        ));
    }

    for workspace_id in [
        &payload.source_workspace_id,
        &current_workspace,
        &payload.target_workspace_id,
    ] {
        sqlx::query(
            "INSERT OR IGNORE INTO task_workspace_history(task_id, workspace_id)
             VALUES (?, ?)",
        )
        .bind(&snapshot.task_id)
        .bind(workspace_id)
        .execute(&mut *conn)
        .await?;
    }

    sqlx::query("UPDATE local_e2ee_image_references SET workspace = ? WHERE parent = ?")
        .bind(&payload.target_workspace_id)
        .bind(&snapshot.task_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM meta WHERE key GLOB ?")
        .bind(format!("epic_membership_baseline:*:{}", snapshot.task_id))
        .execute(&mut *conn)
        .await?;

    sqlx::query("UPDATE notes SET workspace_id = ? WHERE task_id = ?")
        .bind(&payload.target_workspace_id)
        .bind(&snapshot.task_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE task_attachments SET workspace_id = ? WHERE task_id = ?")
        .bind(&payload.target_workspace_id)
        .bind(&snapshot.task_id)
        .execute(&mut *conn)
        .await?;

    sqlx::query("DELETE FROM task_labels WHERE task_id = ?")
        .bind(&snapshot.task_id)
        .execute(&mut *conn)
        .await?;
    for label in &snapshot.labels {
        super::label::create_or_update_task_label(
            conn,
            &payload.target_workspace_id,
            &snapshot.task_id,
            label,
            &change.created_at,
        )
        .await?;
    }

    sqlx::query("DELETE FROM task_metadata WHERE task_id = ?")
        .bind(&snapshot.task_id)
        .execute(&mut *conn)
        .await?;
    for metadata in &snapshot.metadata {
        let Some(value) = &metadata.value else {
            continue;
        };
        let field_id = target_fields
            .get(&metadata.key)
            .context("error move-target-metadata-field-missing")?;
        sqlx::query(
            "INSERT INTO task_metadata(workspace_id, task_id, field_id, value, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&payload.target_workspace_id)
        .bind(&snapshot.task_id)
        .bind(field_id)
        .bind(value)
        .bind(&change.created_at)
        .bind(&change.created_at)
        .execute(&mut *conn)
        .await?;
    }

    sqlx::query(
        "DELETE FROM field_versions WHERE entity_type = 'task' AND entity_id = ?
        AND (workspace_id != ? OR field GLOB 'metadata:*')",
    )
    .bind(&snapshot.task_id)
    .bind(&current_workspace)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE field_versions SET workspace_id = ? WHERE entity_type = 'task' AND entity_id = ?",
    )
    .bind(&payload.target_workspace_id)
    .bind(&snapshot.task_id)
    .execute(&mut *conn)
    .await?;
    for metadata in &snapshot.metadata {
        let field = format!("metadata:{}", target_fields[&metadata.key]);
        if let Some(version) = &metadata.version {
            crate::db::set_entity_field_version(
                conn,
                &payload.target_workspace_id,
                MutableEntityType::Task,
                snapshot.task_id.as_str(),
                &field,
                version,
            )
            .await?;
        } else {
            sqlx::query("DELETE FROM field_versions WHERE entity_type = 'task' AND entity_id = ? AND field = ?")
                .bind(&snapshot.task_id)
                .bind(&field)
                .execute(&mut *conn)
                .await?;
        }
    }
    crate::db::set_entity_field_version(
        conn,
        &payload.target_workspace_id,
        MutableEntityType::Task,
        snapshot.task_id.as_str(),
        "project",
        &change.change_id,
    )
    .await?;

    for (id, field) in conflict_fields {
        if let Some(field) = field {
            sqlx::query("UPDATE conflicts SET workspace_id = ?, field = ? WHERE id = ?")
                .bind(&payload.target_workspace_id)
                .bind(field)
                .bind(id)
                .execute(&mut *conn)
                .await?;
        } else {
            sqlx::query("DELETE FROM conflicts WHERE id = ?")
                .bind(id)
                .execute(&mut *conn)
                .await?;
        }
    }

    sqlx::query("UPDATE tasks SET workspace_id = ?, project_id = ?, updated_at = MAX(updated_at, ?) WHERE id = ?")
        .bind(&payload.target_workspace_id)
        .bind(project_id)
        .bind(&change.created_at)
        .bind(&snapshot.task_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn workspace_is_historical(
    conn: &mut SqliteConnection,
    task_id: &TaskId,
    workspace_id: &WorkspaceId,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM task_workspace_history WHERE task_id = ? AND workspace_id = ?)",
    )
    .bind(task_id)
    .bind(workspace_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(Into::into)
}

async fn remap_metadata_identity(
    conn: &mut SqliteConnection,
    source_workspace_id: &WorkspaceId,
    field: &str,
    target_fields: &HashMap<String, MetadataFieldId>,
) -> Result<Option<String>> {
    let Some(source_field_id) = field.strip_prefix("metadata:") else {
        return Ok(Some(field.to_string()));
    };
    let key: Option<String> =
        sqlx::query_scalar("SELECT key FROM metadata_fields WHERE workspace_id = ? AND id = ?")
            .bind(source_workspace_id)
            .bind(source_field_id)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(key
        .and_then(|key| target_fields.get(&key))
        .map(|id| format!("metadata:{id}")))
}

async fn replace_relationships(
    conn: &mut SqliteConnection,
    payload: &MoveTasksPayload,
    members: &HashSet<TaskId>,
    graphs: bool,
) -> Result<()> {
    for task_id in members {
        if graphs {
            sqlx::query(
                "DELETE FROM task_dependencies WHERE task_id = ? OR depends_on_task_id = ?",
            )
            .bind(task_id)
            .bind(task_id)
            .execute(&mut *conn)
            .await?;
        }
        sqlx::query("DELETE FROM task_related_links WHERE task_a_id = ? OR task_b_id = ?")
            .bind(task_id)
            .bind(task_id)
            .execute(&mut *conn)
            .await?;
        if graphs {
            sqlx::query("DELETE FROM task_epic_links WHERE child_task_id = ? OR epic_task_id = ?")
                .bind(task_id)
                .bind(task_id)
                .execute(&mut *conn)
                .await?;
        }
    }
    for dependency in payload.dependencies.iter().filter(|_| graphs) {
        sqlx::query(
            "INSERT INTO task_dependencies(workspace_id, task_id, depends_on_task_id, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&payload.target_workspace_id)
        .bind(&dependency.task_id)
        .bind(&dependency.depends_on_task_id)
        .bind(&dependency.created_at)
        .execute(&mut *conn)
        .await?;
    }
    for related in &payload.related {
        sqlx::query(
            "INSERT INTO task_related_links(workspace_id, task_a_id, task_b_id, linked, last_change_id)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&payload.target_workspace_id)
        .bind(&related.task_a_id)
        .bind(&related.task_b_id)
        .bind(i64::from(related.linked))
        .bind(&related.last_change_id)
        .execute(&mut *conn)
        .await?;
    }
    for epic in payload.epics.iter().filter(|_| graphs) {
        sqlx::query(
            "INSERT INTO task_epic_links(workspace_id, epic_task_id, child_task_id, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(&payload.target_workspace_id)
        .bind(&epic.epic_task_id)
        .bind(&epic.child_task_id)
        .bind(&epic.created_at)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Reconstructs historical placement without replaying incorporated domain state.
pub(crate) async fn rebuild_task_placements(conn: &mut SqliteConnection) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO task_workspace_history(task_id, workspace_id) SELECT id, workspace_id FROM tasks")
        .execute(&mut *conn).await?;
    let payloads: Vec<String> =
        sqlx::query_scalar("SELECT payload FROM changes WHERE op_type = 'move_tasks'")
            .fetch_all(&mut *conn)
            .await?;
    for json in payloads {
        let payload: MoveTasksPayload = serde_json::from_str(&json)?;
        for task in payload.tasks {
            for workspace in [&payload.source_workspace_id, &payload.target_workspace_id] {
                sqlx::query("INSERT OR IGNORE INTO task_workspace_history(task_id, workspace_id) VALUES (?, ?)")
                    .bind(&task.task_id).bind(workspace).execute(&mut *conn).await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
