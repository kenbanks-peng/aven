use std::collections::HashMap;

use anyhow::{Result, bail};
use serde_json::json;
use sqlx::{QueryBuilder, Row, Sqlite, SqliteConnection};

use crate::db::{
    Database, conflict_exists, entity_conflict_exists, entity_field_version, field_version,
    insert_change, set_entity_field_version, set_field_version,
};
use crate::ids::{TaskId, WorkspaceId, now};
use crate::recurrence::RecurrenceSeriesId;
use crate::types::MutableEntityType;
use crate::workspaces::Workspace;

use super::fields::normalize_metadata_key;
use super::fields::{
    metadata_field_by_key, require_metadata_field, resolve_or_create_metadata_field,
};
use super::validation::validate_metadata_update;
use super::{MetadataField, ResolvedMetadataValue, TaskMetadataInput, TaskMetadataValue};

impl Database {
    pub async fn task_metadata(
        &self,
        workspace_id: &WorkspaceId,
        task_id: &TaskId,
    ) -> Result<Vec<TaskMetadataValue>> {
        let mut conn = self.acquire_reader().await?;
        task_metadata_in_workspace(&mut conn, workspace_id, task_id).await
    }
}

pub(crate) async fn insert_initial_task_metadata(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    task_id: &TaskId,
    inputs: &[TaskMetadataInput],
    timestamp: &str,
) -> Result<Vec<ResolvedMetadataValue>> {
    let values = resolve_metadata_inputs(conn, workspace, inputs).await?;
    for value in &values {
        sqlx::query(
            "INSERT INTO task_metadata(
                 workspace_id, task_id, field_id, value, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&workspace.id)
        .bind(task_id)
        .bind(&value.field_id)
        .bind(&value.value)
        .bind(timestamp)
        .bind(timestamp)
        .execute(&mut *conn)
        .await?;
    }
    Ok(values)
}

pub(crate) async fn set_task_metadata(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    task_id: &TaskId,
    input: &TaskMetadataInput,
) -> Result<bool> {
    let field = resolve_metadata_input_field(conn, workspace, input).await?;
    let identity = format!("metadata:{}", field.id);
    let current = sqlx::query_scalar::<_, String>(
        "SELECT value FROM task_metadata
         WHERE workspace_id = ? AND task_id = ? AND field_id = ?",
    )
    .bind(&workspace.id)
    .bind(task_id)
    .bind(&field.id)
    .fetch_optional(&mut *conn)
    .await?;
    if current.as_deref() == Some(input.value.as_str()) {
        return Ok(false);
    }
    if conflict_exists(conn, &workspace.id, task_id, &identity).await? {
        bail!("error conflicted-field ref={task_id} field={identity}");
    }
    let base = field_version(conn, task_id, &identity).await?;
    let timestamp = now();
    sqlx::query(
        "INSERT INTO task_metadata(
             workspace_id, task_id, field_id, value, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(workspace_id, task_id, field_id)
         DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(&workspace.id)
    .bind(task_id)
    .bind(&field.id)
    .bind(&input.value)
    .bind(&timestamp)
    .bind(&timestamp)
    .execute(&mut *conn)
    .await?;
    touch_task(conn, &workspace.id, task_id, &timestamp).await?;
    let change_id = insert_change(
        conn,
        "task",
        task_id,
        Some(&identity),
        "set_task_metadata",
        json!({
            "workspace_id": &workspace.id,
            "workspace_key": &workspace.key,
            "field_id": &field.id,
            "key": &field.key,
            "value": &input.value,
        }),
        base.as_deref(),
    )
    .await?;
    set_field_version(conn, task_id, &identity, &change_id).await?;
    Ok(true)
}

pub(crate) async fn remove_task_metadata(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    task_id: &TaskId,
    input: &str,
) -> Result<bool> {
    let key = normalize_metadata_key(input)?;
    let Some(field) = metadata_field_by_key(conn, &workspace.id, &key).await? else {
        bail!("error unknown-metadata-field");
    };
    let identity = format!("metadata:{}", field.id);
    if conflict_exists(conn, &workspace.id, task_id, &identity).await? {
        bail!("error conflicted-field ref={task_id} field={identity}");
    }
    let base = field_version(conn, task_id, &identity).await?;
    let removed_value = sqlx::query_scalar::<_, String>(
        "SELECT value FROM task_metadata
         WHERE workspace_id = ? AND task_id = ? AND field_id = ?",
    )
    .bind(&workspace.id)
    .bind(task_id)
    .bind(&field.id)
    .fetch_optional(&mut *conn)
    .await?;
    let deleted = sqlx::query(
        "DELETE FROM task_metadata
         WHERE workspace_id = ? AND task_id = ? AND field_id = ?",
    )
    .bind(&workspace.id)
    .bind(task_id)
    .bind(&field.id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Ok(false);
    }
    let timestamp = now();
    touch_task(conn, &workspace.id, task_id, &timestamp).await?;
    let change_id = insert_change(
        conn,
        "task",
        task_id,
        Some(&identity),
        "remove_task_metadata",
        json!({
            "workspace_id": &workspace.id,
            "workspace_key": &workspace.key,
            "field_id": &field.id,
            "key": &field.key,
            "value": removed_value,
        }),
        base.as_deref(),
    )
    .await?;
    set_field_version(conn, task_id, &identity, &change_id).await?;
    Ok(true)
}

pub(crate) async fn set_recurrence_metadata(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    series_id: &RecurrenceSeriesId,
    input: &TaskMetadataInput,
) -> Result<bool> {
    let field = resolve_metadata_input_field(conn, workspace, input).await?;
    let identity = format!("metadata:{}", field.id);
    let current = sqlx::query_scalar::<_, String>(
        "SELECT value FROM recurrence_series_metadata
         WHERE workspace_id = ? AND series_id = ? AND field_id = ?",
    )
    .bind(&workspace.id)
    .bind(series_id)
    .bind(&field.id)
    .fetch_optional(&mut *conn)
    .await?;
    if current.as_deref() == Some(input.value.as_str()) {
        return Ok(false);
    }
    if entity_conflict_exists(
        conn,
        &workspace.id,
        MutableEntityType::RecurrenceSeries,
        series_id.as_str(),
        &identity,
    )
    .await?
    {
        bail!("error conflicted-recurrence-field field={identity}");
    }
    let base = entity_field_version(
        conn,
        &workspace.id,
        MutableEntityType::RecurrenceSeries,
        series_id.as_str(),
        &identity,
    )
    .await?;
    let timestamp = now();
    sqlx::query(
        "INSERT INTO recurrence_series_metadata(
             workspace_id, series_id, field_id, value, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(workspace_id, series_id, field_id)
         DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(&workspace.id)
    .bind(series_id)
    .bind(&field.id)
    .bind(&input.value)
    .bind(&timestamp)
    .bind(&timestamp)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE recurrence_series SET updated_at = ? WHERE workspace_id = ? AND id = ?")
        .bind(&timestamp)
        .bind(&workspace.id)
        .bind(series_id)
        .execute(&mut *conn)
        .await?;
    let change_id = insert_change(
        conn,
        "recurrence_series",
        series_id.as_str(),
        Some(&identity),
        "set_recurrence_metadata",
        json!({
            "workspace_id": &workspace.id,
            "workspace_key": &workspace.key,
            "field_id": &field.id,
            "key": &field.key,
            "value": &input.value,
        }),
        base.as_deref(),
    )
    .await?;
    set_entity_field_version(
        conn,
        &workspace.id,
        MutableEntityType::RecurrenceSeries,
        series_id.as_str(),
        &identity,
        &change_id,
    )
    .await?;
    Ok(true)
}

pub(crate) async fn remove_recurrence_metadata(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    series_id: &RecurrenceSeriesId,
    key: &str,
) -> Result<bool> {
    let Some(field) =
        metadata_field_by_key(conn, &workspace.id, &normalize_metadata_key(key)?).await?
    else {
        return Ok(false);
    };
    let identity = format!("metadata:{}", field.id);
    if entity_conflict_exists(
        conn,
        &workspace.id,
        MutableEntityType::RecurrenceSeries,
        series_id.as_str(),
        &identity,
    )
    .await?
    {
        bail!("error conflicted-recurrence-field field={identity}");
    }
    let removed = sqlx::query(
        "DELETE FROM recurrence_series_metadata
         WHERE workspace_id = ? AND series_id = ? AND field_id = ?",
    )
    .bind(&workspace.id)
    .bind(series_id)
    .bind(&field.id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if removed == 0 {
        return Ok(false);
    }
    let base = entity_field_version(
        conn,
        &workspace.id,
        MutableEntityType::RecurrenceSeries,
        series_id.as_str(),
        &identity,
    )
    .await?;
    let timestamp = now();
    sqlx::query("UPDATE recurrence_series SET updated_at = ? WHERE workspace_id = ? AND id = ?")
        .bind(&timestamp)
        .bind(&workspace.id)
        .bind(series_id)
        .execute(&mut *conn)
        .await?;
    let change_id = insert_change(
        conn,
        "recurrence_series",
        series_id.as_str(),
        Some(&identity),
        "remove_recurrence_metadata",
        json!({
            "workspace_id": &workspace.id,
            "workspace_key": &workspace.key,
            "field_id": &field.id,
            "key": &field.key,
        }),
        base.as_deref(),
    )
    .await?;
    set_entity_field_version(
        conn,
        &workspace.id,
        MutableEntityType::RecurrenceSeries,
        series_id.as_str(),
        &identity,
        &change_id,
    )
    .await?;
    Ok(true)
}
async fn resolve_metadata_input_field(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    input: &TaskMetadataInput,
) -> Result<MetadataField> {
    match &input.expected_field_id {
        Some(id) => require_metadata_field(conn, &workspace.id, id, &input.key).await,
        None => resolve_or_create_metadata_field(conn, workspace, &input.key).await,
    }
}

pub(crate) async fn resolve_metadata_inputs(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    inputs: &[TaskMetadataInput],
) -> Result<Vec<ResolvedMetadataValue>> {
    validate_metadata_update(inputs, &[])?;
    let mut values = Vec::with_capacity(inputs.len());
    for input in inputs {
        let field = resolve_metadata_input_field(conn, workspace, input).await?;
        values.push(ResolvedMetadataValue {
            field_id: field.id,
            key: field.key,
            value: input.value.clone(),
        });
    }
    Ok(values)
}
async fn touch_task(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    task_id: &TaskId,
    timestamp: &str,
) -> Result<()> {
    let affected = sqlx::query("UPDATE tasks SET updated_at = ? WHERE workspace_id = ? AND id = ?")
        .bind(timestamp)
        .bind(workspace_id)
        .bind(task_id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    if affected != 1 {
        bail!("error task-not-found task_id={task_id}");
    }
    Ok(())
}

pub(crate) async fn task_metadata_in_workspace(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    task_id: &TaskId,
) -> Result<Vec<TaskMetadataValue>> {
    let rows = sqlx::query(
        "SELECT m.field_id, f.key, m.value
         FROM task_metadata m
         JOIN metadata_fields f ON f.workspace_id = m.workspace_id AND f.id = m.field_id
         WHERE m.workspace_id = ? AND m.task_id = ?
         ORDER BY f.key",
    )
    .bind(workspace_id)
    .bind(task_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| TaskMetadataValue {
            field_id: row.get("field_id"),
            key: row.get("key"),
            value: row.get("value"),
        })
        .collect())
}

pub(crate) async fn metadata_by_task_ids(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    task_ids: &[TaskId],
) -> Result<HashMap<TaskId, Vec<TaskMetadataValue>>> {
    let mut values = HashMap::new();
    for chunk in task_ids.chunks(900) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT m.task_id, m.field_id, f.key, m.value
             FROM task_metadata m
             JOIN metadata_fields f
               ON f.workspace_id = m.workspace_id AND f.id = m.field_id
             WHERE m.workspace_id = ",
        );
        query.push_bind(workspace_id);
        query.push(" AND m.task_id IN (");
        {
            let mut separated = query.separated(", ");
            for task_id in chunk {
                separated.push_bind(task_id);
            }
        }
        query.push(") ORDER BY m.task_id, f.key");
        for row in query.build().fetch_all(&mut *conn).await? {
            let task_id: TaskId = row.get("task_id");
            values
                .entry(task_id)
                .or_insert_with(Vec::new)
                .push(TaskMetadataValue {
                    field_id: row.get("field_id"),
                    key: row.get("key"),
                    value: row.get("value"),
                });
        }
    }
    Ok(values)
}
