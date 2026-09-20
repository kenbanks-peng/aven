use anyhow::Result;
use sqlx::SqliteConnection;

use crate::ids::WorkspaceId;
use crate::types::MutableEntityType;

pub(crate) async fn entity_field_version(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    entity_type: MutableEntityType,
    entity_id: &str,
    field: &str,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT version FROM field_versions
         WHERE workspace_id = ? AND entity_type = ? AND entity_id = ? AND field = ?",
    )
    .bind(workspace_id)
    .bind(entity_type.as_str())
    .bind(entity_id)
    .bind(field)
    .fetch_optional(&mut *conn)
    .await?)
}

pub(crate) async fn set_entity_field_version(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    entity_type: MutableEntityType,
    entity_id: &str,
    field: &str,
    version: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO field_versions(workspace_id, entity_type, entity_id, field, version)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(workspace_id, entity_type, entity_id, field)
         DO UPDATE SET version = excluded.version",
    )
    .bind(workspace_id)
    .bind(entity_type.as_str())
    .bind(entity_id)
    .bind(field)
    .bind(version)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub(crate) async fn field_version(
    conn: &mut SqliteConnection,
    entity_id: &str,
    field: &str,
) -> Result<Option<String>> {
    let workspace_id =
        sqlx::query_scalar::<_, WorkspaceId>("SELECT workspace_id FROM tasks WHERE id = ? LIMIT 1")
            .bind(entity_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some(workspace_id) = workspace_id else {
        return Ok(None);
    };
    entity_field_version(
        conn,
        &workspace_id,
        MutableEntityType::Task,
        entity_id,
        field,
    )
    .await
}

pub(crate) async fn set_field_version(
    conn: &mut SqliteConnection,
    entity_id: &str,
    field: &str,
    version: &str,
) -> Result<()> {
    let workspace_id =
        sqlx::query_scalar::<_, WorkspaceId>("SELECT workspace_id FROM tasks WHERE id = ? LIMIT 1")
            .bind(entity_id)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some(workspace_id) = workspace_id {
        set_entity_field_version(
            conn,
            &workspace_id,
            MutableEntityType::Task,
            entity_id,
            field,
            version,
        )
        .await?;
    }
    Ok(())
}

pub(crate) async fn entity_conflict_exists(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    entity_type: MutableEntityType,
    entity_id: &str,
    field: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM conflicts
         WHERE workspace_id = ? AND entity_type = ? AND entity_id = ?
         AND field = ? AND resolved = 0 LIMIT 1",
    )
    .bind(workspace_id)
    .bind(entity_type.as_str())
    .bind(entity_id)
    .bind(field)
    .fetch_one(&mut *conn)
    .await?
        > 0)
}

pub(crate) async fn conflict_exists(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    task_id: &crate::ids::TaskId,
    field: &str,
) -> Result<bool> {
    entity_conflict_exists(
        conn,
        workspace_id,
        MutableEntityType::Task,
        task_id.as_str(),
        field,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn field_versions_support_task_and_recurrence_series_identity() {
        let (_temp, mut conn) = crate::test_support::test_conn().await;
        sqlx::query(
                "INSERT INTO tasks(id, title, description, project_id, status, priority, created_at, updated_at)
                 VALUES ('7KQ9A1X4MV2P8D6T', 'task', '', '7KQ9A1X4MV2P8D6S', 'todo', 'none', 't', 't')",
            )
            .execute(&mut *conn)
            .await
            .unwrap();
        set_field_version(&mut conn, "7KQ9A1X4MV2P8D6T", "title", "task-version")
            .await
            .unwrap();
        set_entity_field_version(
            &mut conn,
            &crate::workspaces::default_workspace_id(),
            MutableEntityType::RecurrenceSeries,
            "7KQ9A1X4MV2P8D6R",
            "title",
            "series-version",
        )
        .await
        .unwrap();

        assert_eq!(
            field_version(&mut conn, "7KQ9A1X4MV2P8D6T", "title")
                .await
                .unwrap()
                .as_deref(),
            Some("task-version")
        );
        assert_eq!(
            entity_field_version(
                &mut conn,
                &crate::workspaces::default_workspace_id(),
                MutableEntityType::RecurrenceSeries,
                "7KQ9A1X4MV2P8D6R",
                "title",
            )
            .await
            .unwrap()
            .as_deref(),
            Some("series-version")
        );
    }
}
