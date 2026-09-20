use anyhow::{Result, bail};
use serde_json::json;
use sqlx::{Row, SqliteConnection};

use crate::db::{
    Database, begin_immediate, entity_conflict_exists, entity_field_version, insert_change,
    set_entity_field_version,
};
use crate::ids::{MetadataFieldId, WorkspaceId, now};
use crate::types::MutableEntityType;
use crate::workspaces::Workspace;

use super::{MetadataField, MetadataFieldUsage};

impl Database {
    pub async fn list_metadata_fields(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<MetadataFieldUsage>> {
        let mut conn = self.acquire_reader().await?;
        list_metadata_fields_in_workspace(&mut conn, workspace_id).await
    }

    pub async fn find_metadata_field(
        &self,
        workspace_id: &WorkspaceId,
        key: &str,
    ) -> Result<Option<MetadataField>> {
        let mut conn = self.acquire_reader().await?;
        find_metadata_field_in_workspace(&mut conn, workspace_id, key).await
    }

    pub async fn rename_metadata_field(
        &self,
        workspace: &Workspace,
        key: &str,
        new_key: &str,
    ) -> Result<MetadataField> {
        let mut conn = self.acquire_writer().await?;
        let mut tx = begin_immediate(&mut conn).await?;
        let field = rename_metadata_field(&mut tx, workspace, key, new_key).await?;
        tx.commit().await?;
        Ok(field)
    }
}
pub fn normalize_metadata_key(input: &str) -> Result<String> {
    let key = input.trim().to_ascii_lowercase();
    let mut bytes = key.bytes();
    let Some(first) = bytes.next() else {
        bail!("error invalid-metadata-key");
    };
    if !first.is_ascii_lowercase()
        || key.len() > 64
        || !bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
        || key.starts_with("aven.")
    {
        bail!("error invalid-metadata-key");
    }
    Ok(key)
}

pub(crate) async fn find_metadata_field_in_workspace(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    input: &str,
) -> Result<Option<MetadataField>> {
    let key = normalize_metadata_key(input)?;
    metadata_field_by_key(conn, workspace_id, &key).await
}

pub(crate) async fn metadata_field_by_key(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    key: &str,
) -> Result<Option<MetadataField>> {
    let row = sqlx::query(
        "SELECT id, workspace_id, key, created_at, updated_at
         FROM metadata_fields WHERE workspace_id = ? AND key = ?",
    )
    .bind(workspace_id)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(metadata_field_from_row))
}

pub(crate) async fn metadata_field_by_id(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    field_id: &MetadataFieldId,
) -> Result<Option<MetadataField>> {
    let row = sqlx::query(
        "SELECT id, workspace_id, key, created_at, updated_at
         FROM metadata_fields WHERE workspace_id = ? AND id = ?",
    )
    .bind(workspace_id)
    .bind(field_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(metadata_field_from_row))
}

pub(crate) async fn resolve_or_create_metadata_field(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    input: &str,
) -> Result<MetadataField> {
    let key = normalize_metadata_key(input)?;
    if let Some(field) = metadata_field_by_key(conn, &workspace.id, &key).await? {
        return Ok(field);
    }

    let id = MetadataFieldId::new();
    let timestamp = now();
    sqlx::query(
        "INSERT INTO metadata_fields(id, workspace_id, key, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&workspace.id)
    .bind(&key)
    .bind(&timestamp)
    .bind(&timestamp)
    .execute(&mut *conn)
    .await?;
    let change_id = insert_change(
        conn,
        "metadata_field",
        id.as_str(),
        None,
        "create_metadata_field",
        json!({
            "workspace_id": &workspace.id,
            "workspace_key": &workspace.key,
            "key": &key,
            "created_at": &timestamp,
        }),
        None,
    )
    .await?;
    set_entity_field_version(
        conn,
        &workspace.id,
        MutableEntityType::MetadataField,
        id.as_str(),
        "key",
        &change_id,
    )
    .await?;
    Ok(MetadataField {
        id,
        workspace_id: workspace.id.clone(),
        key,
        created_at: timestamp.clone(),
        updated_at: timestamp,
    })
}

pub(crate) async fn rename_metadata_field(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    input: &str,
    new_input: &str,
) -> Result<MetadataField> {
    let key = normalize_metadata_key(input)?;
    let new_key = normalize_metadata_key(new_input)?;
    let mut field = metadata_field_by_key(conn, &workspace.id, &key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("error unknown-metadata-field"))?;
    let resolving = entity_conflict_exists(
        conn,
        &workspace.id,
        MutableEntityType::MetadataField,
        field.id.as_str(),
        "key",
    )
    .await?;
    if key == new_key && !resolving {
        return Ok(field);
    }
    if key != new_key
        && metadata_field_by_key(conn, &workspace.id, &new_key)
            .await?
            .is_some()
    {
        bail!("error metadata-field-exists");
    }
    let base = if resolving {
        None
    } else {
        entity_field_version(
            conn,
            &workspace.id,
            MutableEntityType::MetadataField,
            field.id.as_str(),
            "key",
        )
        .await?
    };
    let timestamp = now();
    sqlx::query(
        "UPDATE metadata_fields SET key = ?, updated_at = ?
         WHERE workspace_id = ? AND id = ?",
    )
    .bind(&new_key)
    .bind(&timestamp)
    .bind(&workspace.id)
    .bind(&field.id)
    .execute(&mut *conn)
    .await?;
    let change_id = insert_change(
        conn,
        "metadata_field",
        field.id.as_str(),
        Some("key"),
        "set_metadata_field",
        json!({
            "workspace_id": &workspace.id,
            "workspace_key": &workspace.key,
            "key": &new_key,
            "conflict_resolution": resolving,
        }),
        base.as_deref(),
    )
    .await?;
    set_entity_field_version(
        conn,
        &workspace.id,
        MutableEntityType::MetadataField,
        field.id.as_str(),
        "key",
        &change_id,
    )
    .await?;
    if resolving {
        sqlx::query(
            "UPDATE conflicts SET resolved = 1
             WHERE workspace_id = ? AND entity_type = 'metadata_field'
               AND entity_id = ? AND field = 'key' AND resolved = 0",
        )
        .bind(&workspace.id)
        .bind(&field.id)
        .execute(&mut *conn)
        .await?;
    }
    field.key = new_key;
    field.updated_at = timestamp;
    Ok(field)
}
pub(crate) async fn require_metadata_field(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    field_id: &MetadataFieldId,
    key: &str,
) -> Result<MetadataField> {
    let field = metadata_field_by_id(conn, workspace_id, field_id).await?;
    match field {
        Some(field) if field.key == key => Ok(field),
        _ => bail!("error metadata-field-changed"),
    }
}
async fn list_metadata_fields_in_workspace(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
) -> Result<Vec<MetadataFieldUsage>> {
    let rows = sqlx::query(
        "SELECT f.id, f.workspace_id, f.key, f.created_at, f.updated_at,
                (SELECT count(*) FROM task_metadata m
                 WHERE m.workspace_id = f.workspace_id AND m.field_id = f.id) AS task_count,
                (SELECT count(*) FROM recurrence_series_metadata m
                 WHERE m.workspace_id = f.workspace_id AND m.field_id = f.id) AS series_count
         FROM metadata_fields f
         WHERE f.workspace_id = ?
         ORDER BY f.key",
    )
    .bind(workspace_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| MetadataFieldUsage {
            field: MetadataField {
                id: row.get("id"),
                workspace_id: row.get("workspace_id"),
                key: row.get("key"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            },
            task_count: usize::try_from(row.get::<i64, _>("task_count")).unwrap_or(usize::MAX),
            series_count: usize::try_from(row.get::<i64, _>("series_count")).unwrap_or(usize::MAX),
        })
        .collect())
}

fn metadata_field_from_row(row: sqlx::sqlite::SqliteRow) -> MetadataField {
    MetadataField {
        id: row.get("id"),
        workspace_id: row.get("workspace_id"),
        key: row.get("key"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}
