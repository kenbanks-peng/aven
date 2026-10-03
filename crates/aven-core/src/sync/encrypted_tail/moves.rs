use anyhow::{Context, Result};
use sqlx::SqliteConnection;

use crate::change_log::op_type;
use crate::sync::wire::{ChangeWire, MoveTasksPayload};

pub(super) async fn is_moved(conn: &mut SqliteConnection, prefix: i64, task: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM changes c, json_each(c.payload, '$.tasks') t
         WHERE c.op_type = 'move_tasks' AND (c.server_seq > ? OR c.server_seq IS NULL)
           AND json_extract(t.value, '$.task_id') = ?)",
    )
    .bind(prefix)
    .bind(task)
    .fetch_one(conn)
    .await?)
}

/// Move snapshots establish placement-sensitive state; only their subsequent
/// commands replay, in accepted order followed by the optimistic pending tail.
pub(super) async fn reconcile(conn: &mut SqliteConnection, prefix: i64) -> Result<()> {
    let any: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM changes WHERE op_type='move_tasks' AND (server_seq > ? OR server_seq IS NULL))")
        .bind(prefix).fetch_one(&mut *conn).await?;
    if !any {
        return Ok(());
    }
    let ids: Vec<String> = sqlx::query_scalar(
        "WITH tail AS (
             SELECT change_id, op_type, field, ROW_NUMBER() OVER (ORDER BY server_seq IS NULL, server_seq, local_seq, created_at, change_id) AS position
             FROM changes WHERE server_seq > ? OR server_seq IS NULL
         ) SELECT change_id FROM tail
           WHERE position >= (SELECT MIN(position) FROM tail WHERE op_type='move_tasks')
             AND (op_type IN ('move_tasks','label_add','label_remove','create_label','set_label_name','label_delete','label_restore','set_task_metadata','remove_task_metadata','related_add','related_remove','create_project','set_project_metadata','project_delete')
                  OR (op_type IN ('set_field','resolve_field') AND field='project'))
           ORDER BY position",
    ).bind(prefix).fetch_all(&mut *conn).await?;
    // Tail metadata conflicts are derived again from snapshot value/version pairs.
    // Conflicts from the incorporated prefix remain authoritative.
    sqlx::query("DELETE FROM conflicts WHERE entity_type='task' AND field LIKE 'metadata:%'
        AND remote_change_id IN (SELECT change_id FROM changes WHERE (server_seq > ? OR server_seq IS NULL) AND op_type IN ('set_task_metadata','remove_task_metadata'))
        AND entity_id IN (SELECT json_extract(t.value, '$.task_id') FROM changes c, json_each(c.payload, '$.tasks') t WHERE c.op_type='move_tasks' AND (c.server_seq > ? OR c.server_seq IS NULL))")
        .bind(prefix).bind(prefix).execute(&mut *conn).await?;
    let mut moved = std::collections::HashSet::new();
    let mut projects = std::collections::HashSet::new();
    for id in ids {
        let change = super::client::load_change(conn, &id)
            .await?
            .context("error encrypted-move-history")?;
        if change.op_type == op_type::MOVE_TASKS {
            let payload = MoveTasksPayload::from_change(&change)?;
            moved.extend(payload.tasks.iter().map(|t| t.task_id.to_string()));
            projects.insert(payload.project_id.to_string());
            crate::sync::apply::replay_move_tasks(conn, &change).await?;
            let project: String = sqlx::query_scalar("SELECT project_id FROM tasks WHERE id=?")
                .bind(&payload.tasks[0].task_id)
                .fetch_one(&mut *conn)
                .await?;
            projects.insert(project);
        } else if !moved.is_empty() && change.entity_type == "label" {
            apply_label_effect(conn, &change, &moved).await?;
        } else if change.entity_type == "project" && projects.contains(&change.entity_id) {
            crate::sync::apply::apply_remote_change(conn, &change).await?;
        } else if !moved.is_empty()
            && needs_reconcile(&change)
            && (moved.contains(&change.entity_id)
                || ["depends_on_task_id", "related_task_id", "epic_task_id"]
                    .iter()
                    .any(|key| {
                        change
                            .payload
                            .get(key)
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|id| moved.contains(id))
                    }))
        {
            if matches!(
                change.op_type.as_str(),
                op_type::RELATED_ADD | op_type::RELATED_REMOVE
            ) {
                crate::sync::apply::replay_related(conn, &change).await?;
            } else {
                crate::sync::apply::apply_remote_change(conn, &change).await?;
            }
        }
    }
    Ok(())
}

async fn apply_label_effect(
    conn: &mut SqliteConnection,
    change: &ChangeWire,
    moved: &std::collections::HashSet<String>,
) -> Result<()> {
    let workspace = change.payload["workspace_id"]
        .as_str()
        .context("error encrypted-move-label")?;
    let name = change.payload["name"]
        .as_str()
        .context("error encrypted-move-label")?;
    if matches!(
        change.op_type.as_str(),
        op_type::CREATE_LABEL | op_type::LABEL_RESTORE | op_type::SET_LABEL_NAME
    ) {
        let new_name = if change.op_type == op_type::SET_LABEL_NAME {
            change.payload["new_name"]
                .as_str()
                .context("error encrypted-move-label")?
        } else {
            name
        };
        sqlx::query(
            "INSERT OR IGNORE INTO labels(workspace_id, name, created_at) VALUES (?, ?, ?)",
        )
        .bind(workspace)
        .bind(new_name)
        .bind(
            change.payload["created_at"]
                .as_str()
                .unwrap_or(&change.created_at),
        )
        .execute(&mut *conn)
        .await?;
    }
    for task in moved {
        if change.op_type == op_type::SET_LABEL_NAME {
            let new_name = change.payload["new_name"]
                .as_str()
                .context("error encrypted-move-label")?;
            sqlx::query("INSERT OR IGNORE INTO task_labels(workspace_id, task_id, label) SELECT workspace_id, task_id, ? FROM task_labels WHERE workspace_id=? AND task_id=? AND label=?")
                .bind(new_name).bind(workspace).bind(task).bind(name).execute(&mut *conn).await?;
        }
        if matches!(
            change.op_type.as_str(),
            op_type::LABEL_DELETE | op_type::SET_LABEL_NAME
        ) {
            sqlx::query("DELETE FROM task_labels WHERE workspace_id=? AND task_id=? AND label=?")
                .bind(workspace)
                .bind(task)
                .bind(name)
                .execute(&mut *conn)
                .await?;
        } else if change.op_type == op_type::LABEL_RESTORE
            && change.payload["task_ids"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(task)))
        {
            sqlx::query("INSERT OR IGNORE INTO task_labels(workspace_id, task_id, label) SELECT ?, id, ? FROM tasks WHERE workspace_id=? AND id=?")
                .bind(workspace).bind(name).bind(workspace).bind(task).execute(&mut *conn).await?;
        }
    }
    if matches!(
        change.op_type.as_str(),
        op_type::LABEL_DELETE | op_type::SET_LABEL_NAME
    ) {
        sqlx::query("DELETE FROM labels WHERE workspace_id=? AND name=?
            AND NOT EXISTS(SELECT 1 FROM task_labels WHERE workspace_id=? AND label=?)
            AND NOT EXISTS(SELECT 1 FROM recurrence_series_labels WHERE workspace_id=? AND label=?)")
            .bind(workspace).bind(name).bind(workspace).bind(name).bind(workspace).bind(name).execute(&mut *conn).await?;
    }
    Ok(())
}

pub(super) fn is_metadata(change: &ChangeWire) -> bool {
    matches!(
        change.op_type.as_str(),
        op_type::SET_TASK_METADATA | op_type::REMOVE_TASK_METADATA
    )
}

pub(super) fn needs_reconcile(change: &ChangeWire) -> bool {
    matches!(
        change.op_type.as_str(),
        op_type::MOVE_TASKS
            | op_type::CREATE_PROJECT
            | op_type::SET_PROJECT_METADATA
            | op_type::PROJECT_DELETE
            | op_type::LABEL_ADD
            | op_type::LABEL_REMOVE
            | op_type::CREATE_LABEL
            | op_type::SET_LABEL_NAME
            | op_type::LABEL_DELETE
            | op_type::LABEL_RESTORE
            | op_type::SET_TASK_METADATA
            | op_type::REMOVE_TASK_METADATA
            | op_type::RELATED_ADD
            | op_type::RELATED_REMOVE
    ) || matches!(
        change.op_type.as_str(),
        op_type::SET_FIELD | op_type::RESOLVE_FIELD
    ) && change.field.as_deref() == Some("project")
}

pub(super) async fn server_workspace(
    conn: &mut SqliteConnection,
    task: &str,
    authored: &str,
) -> Result<String> {
    let current: Option<String> = sqlx::query_scalar(
        "SELECT workspace_id FROM server_e2ee_task_placements WHERE task_id = ?",
    )
    .bind(task)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(current) = current else {
        // Published catalogs contain current parent placement, not decrypted history.
        let parents: Vec<String> =
            sqlx::query_scalar("SELECT workspace FROM server_e2ee_image_parents WHERE parent = ?")
                .bind(task)
                .fetch_all(&mut *conn)
                .await?;
        super::valid(parents.len() <= 1)?;
        return Ok(parents
            .into_iter()
            .next()
            .unwrap_or_else(|| authored.into()));
    };
    if current != authored {
        super::valid(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM server_e2ee_task_workspace_history WHERE task_id = ? AND workspace_id = ?)")
            .bind(task).bind(authored).fetch_one(&mut *conn).await?)?;
    }
    Ok(current)
}

pub(super) async fn admit(
    conn: &mut SqliteConnection,
    source: &str,
    target: &str,
    tasks: &[String],
) -> Result<()> {
    for task in tasks {
        let current = server_workspace(conn, task, source).await?;
        sqlx::query("INSERT OR IGNORE INTO server_e2ee_task_workspace_history(task_id, workspace_id) VALUES (?, ?), (?, ?), (?, ?)")
            .bind(task).bind(source).bind(task).bind(&current).bind(task).bind(target).execute(&mut *conn).await?;
        sqlx::query("INSERT INTO server_e2ee_task_placements(task_id, workspace_id) VALUES (?, ?) ON CONFLICT(task_id) DO UPDATE SET workspace_id = excluded.workspace_id")
            .bind(task).bind(target).execute(&mut *conn).await?;
        sqlx::query("INSERT INTO server_e2ee_image_parents(workspace, parent, version, deleted, protected)
            SELECT ?, parent, version, deleted, protected FROM server_e2ee_image_parents WHERE workspace = ? AND parent = ?
            ON CONFLICT(workspace, parent) DO UPDATE SET version = excluded.version, deleted = excluded.deleted, protected = MAX(protected, excluded.protected)")
            .bind(target).bind(&current).bind(task).execute(&mut *conn).await?;
        sqlx::query("INSERT OR IGNORE INTO server_e2ee_image_scopes(object, workspace)
            SELECT object, ? FROM server_e2ee_image_references WHERE parent = ? AND object IS NOT NULL")
            .bind(target).bind(task).execute(&mut *conn).await?;
        sqlx::query("UPDATE server_e2ee_image_references SET workspace = ? WHERE parent = ?")
            .bind(target)
            .bind(task)
            .execute(&mut *conn)
            .await?;
        sqlx::query("DELETE FROM server_e2ee_image_parents WHERE parent=? AND workspace != ? AND NOT EXISTS(SELECT 1 FROM server_e2ee_image_references r WHERE r.workspace=server_e2ee_image_parents.workspace AND r.parent=server_e2ee_image_parents.parent)")
            .bind(task).bind(target).execute(&mut *conn).await?;
        super::attachments::server::retain_parent(conn, target, task).await?;
    }
    for table in [
        "server_e2ee_task_placements",
        "server_e2ee_task_workspace_history",
        "server_e2ee_image_parents",
        "server_e2ee_image_scopes",
    ] {
        let count: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&mut *conn)
                .await?;
        super::valid(count <= 262144)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn move_admission_refuses_structural_overflow_atomically() {
        let (_dir, mut conn) = crate::test_support::test_conn().await;
        sqlx::query("WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<262144)
            INSERT INTO server_e2ee_task_placements(task_id, workspace_id) SELECT printf('%016X', n), '0000000000000000' FROM ids")
            .execute(&mut *conn).await.unwrap();
        let mut tx = crate::db::begin_immediate(&mut conn).await.unwrap();
        assert!(
            admit(
                &mut tx,
                "0000000000000000",
                "1111111111111111",
                &["ZZZZZZZZZZZZZZZZ".into()]
            )
            .await
            .is_err()
        );
        tx.rollback().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM server_e2ee_task_placements")
                .fetch_one(&mut *conn)
                .await
                .unwrap(),
            262144
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM server_e2ee_task_workspace_history")
                .fetch_one(&mut *conn)
                .await
                .unwrap(),
            0
        );
    }
}
