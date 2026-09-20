use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use sqlx::SqliteConnection;

use super::super::wire::{AttachmentAddPayload, ChangeWire};
use crate::change_log::op_type;

pub(super) async fn apply_server_blob_reference(
    conn: &mut SqliteConnection,
    change: &ChangeWire,
    affected_attachment_hashes: &mut HashSet<String>,
) -> Result<()> {
    match change.op_type.as_str() {
        op_type::ATTACHMENT_ADD => {
            let payload = AttachmentAddPayload::from_change(change)?;
            let previous_sha256: Option<String> = sqlx::query_scalar(
                "SELECT sha256 FROM server_blob_references
                 WHERE workspace_id = ? AND attachment_id = ?",
            )
            .bind(&payload.workspace_id)
            .bind(&payload.attachment_id)
            .fetch_optional(&mut *conn)
            .await?;
            if let Some(previous_sha256) = previous_sha256 {
                affected_attachment_hashes.insert(previous_sha256);
            }
            affected_attachment_hashes.insert(payload.sha256.clone());
            sqlx::query(
                "INSERT INTO server_blob_references(
                   workspace_id, attachment_id, task_id, sha256, byte_size, deleted
                 ) VALUES (?, ?, ?, ?, ?, 0)
                 ON CONFLICT(workspace_id, attachment_id) DO UPDATE SET
                   task_id = excluded.task_id, sha256 = excluded.sha256,
                   byte_size = excluded.byte_size, deleted = 0",
            )
            .bind(&payload.workspace_id)
            .bind(&payload.attachment_id)
            .bind(&change.entity_id)
            .bind(&payload.sha256)
            .bind(payload.byte_size)
            .execute(&mut *conn)
            .await?;
            sqlx::query(
                "DELETE FROM blob_upload_reservations WHERE workspace_id = ? AND sha256 = ?",
            )
            .bind(&payload.workspace_id)
            .bind(&payload.sha256)
            .execute(&mut *conn)
            .await?;
        }
        op_type::ATTACHMENT_DELETE => {
            let workspace_id = change.payload["workspace_id"]
                .as_str()
                .context("payload missing workspace_id")?;
            let attachment_id = change.payload["attachment_id"]
                .as_str()
                .context("payload missing attachment_id")?;
            let sha256: Option<String> = sqlx::query_scalar(
                "SELECT sha256 FROM server_blob_references
                 WHERE workspace_id = ? AND attachment_id = ?",
            )
            .bind(workspace_id)
            .bind(attachment_id)
            .fetch_optional(&mut *conn)
            .await?;
            if let Some(sha256) = sha256 {
                affected_attachment_hashes.insert(sha256);
            }
            sqlx::query(
                "UPDATE server_blob_references SET deleted = 1
                 WHERE workspace_id = ? AND attachment_id = ?",
            )
            .bind(workspace_id)
            .bind(attachment_id)
            .execute(&mut *conn)
            .await?;
        }
        op_type::SET_FIELD | op_type::RESOLVE_FIELD
            if change.field.as_deref() == Some("deleted") =>
        {
            let workspace_id = change.payload["workspace_id"]
                .as_str()
                .context("payload missing workspace_id")?;
            let hashes: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT sha256 FROM server_blob_references
                 WHERE workspace_id = ? AND task_id = ?",
            )
            .bind(workspace_id)
            .bind(&change.entity_id)
            .fetch_all(&mut *conn)
            .await?;
            affected_attachment_hashes.extend(hashes);
            let deleted = change.payload["value"]
                .as_str()
                .is_some_and(|value| value == "1");
            sqlx::query(
                "INSERT INTO server_task_tombstones(workspace_id, task_id, deleted)
                 VALUES (?, ?, ?)
                 ON CONFLICT(workspace_id, task_id) DO UPDATE SET deleted = excluded.deleted",
            )
            .bind(workspace_id)
            .bind(&change.entity_id)
            .bind(i64::from(deleted))
            .execute(&mut *conn)
            .await?;
        }
        _ => {}
    }
    Ok(())
}

pub(super) async fn collect_attachment_liveness_hashes(
    conn: &mut SqliteConnection,
    change: &ChangeWire,
    affected_attachment_hashes: &mut HashSet<String>,
) -> Result<()> {
    match change.op_type.as_str() {
        op_type::ATTACHMENT_ADD => {
            let payload = AttachmentAddPayload::from_change(change)?;
            affected_attachment_hashes.insert(payload.sha256);
        }
        op_type::ATTACHMENT_DELETE => {
            let workspace_id = change.payload["workspace_id"]
                .as_str()
                .context("payload missing workspace_id")?;
            let attachment_id = change.payload["attachment_id"]
                .as_str()
                .context("payload missing attachment_id")?;
            let sha256: Option<String> = sqlx::query_scalar(
                "SELECT sha256 FROM task_attachments
                 WHERE workspace_id = ? AND attachment_id = ?",
            )
            .bind(workspace_id)
            .bind(attachment_id)
            .fetch_optional(&mut *conn)
            .await?;
            if let Some(sha256) = sha256 {
                affected_attachment_hashes.insert(sha256);
            }
        }
        op_type::SET_FIELD | op_type::RESOLVE_FIELD
            if change.field.as_deref() == Some("deleted") =>
        {
            let workspace_id = change.payload["workspace_id"]
                .as_str()
                .context("payload missing workspace_id")?;
            let hashes: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT sha256 FROM task_attachments
                 WHERE workspace_id = ? AND task_id = ?",
            )
            .bind(workspace_id)
            .bind(&change.entity_id)
            .fetch_all(&mut *conn)
            .await?;
            affected_attachment_hashes.extend(hashes);
        }
        _ => {}
    }
    Ok(())
}

pub(super) async fn prepare_server_blobs(
    conn: &mut SqliteConnection,
    blob_dir: &Path,
    changes: &[ChangeWire],
) -> Result<()> {
    let assigned_change_ids = super::load_assigned_change_ids(conn, changes).await?;
    let contracts = changes
        .iter()
        .filter(|change| {
            change.op_type == op_type::ATTACHMENT_ADD
                && !assigned_change_ids.contains(&change.change_id)
        })
        .map(|change| {
            super::super::blob::attachment_blob_contract(change)?
                .context("error attachment-blob-missing")
        })
        .collect::<Result<Vec<_>>>()?;
    for contract in super::super::blob::unique_blob_content_contracts(&contracts)? {
        validate_server_blob_before_writer(conn, blob_dir, &contract).await?;
    }
    Ok(())
}

async fn validate_server_blob_before_writer(
    conn: &mut SqliteConnection,
    blob_dir: &Path,
    contract: &super::super::wire::BlobUploadContract,
) -> Result<()> {
    let Some(row) = crate::attachments::storage::blob_inventory_row(conn, &contract.sha256).await?
    else {
        bail!("error attachment-blob-missing");
    };
    validate_server_blob_inventory(&row, contract)?;
    let path = crate::attachments::object_path(blob_dir, &contract.sha256)?;
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("error attachment-blob-missing")
        }
        Err(error) => return Err(error.into()),
    };
    if i64::try_from(bytes.len()).ok() != Some(contract.byte_size)
        || crate::attachments::storage::sha256_hex(&bytes) != contract.sha256
    {
        bail!("error attachment-blob-content-mismatch");
    }
    let validated =
        crate::attachments::decode::validate_image(bytes, Some(contract.media_type.clone()))
            .await?;
    if (validated.facts.width, validated.facts.height) != (contract.width, contract.height) {
        bail!("error blob-inventory-metadata-mismatch");
    }
    Ok(())
}

fn validate_server_blob_inventory(
    row: &crate::types::BlobInventoryRow,
    contract: &super::super::wire::BlobUploadContract,
) -> Result<()> {
    if !row.available {
        bail!("error attachment-blob-missing");
    }
    if row.byte_size != contract.byte_size || row.media_type != contract.media_type {
        bail!("error blob-inventory-metadata-mismatch");
    }
    Ok(())
}

pub(super) async fn ensure_attachment_blobs_admitted(
    conn: &mut SqliteConnection,
    blob_dir: &Path,
    changes: &[ChangeWire],
) -> Result<()> {
    let contracts = super::super::blob::attachment_blob_contracts(changes)?;
    for contract in super::super::blob::unique_blob_content_contracts(&contracts)? {
        let Some(row) =
            crate::attachments::storage::blob_inventory_row(conn, &contract.sha256).await?
        else {
            bail!("error attachment-blob-missing");
        };
        validate_server_blob_inventory(&row, &contract)?;
        if !crate::attachments::object_path(blob_dir, &contract.sha256)?.exists() {
            bail!("error attachment-blob-missing");
        }
    }
    for contract in super::super::blob::unique_blob_admission_contracts(&contracts) {
        let admitted: bool = sqlx::query_scalar(
            "SELECT EXISTS(
               SELECT 1 FROM server_blob_references sbr
               LEFT JOIN server_task_tombstones st
                 ON st.workspace_id = sbr.workspace_id AND st.task_id = sbr.task_id
               WHERE sbr.workspace_id = ? AND sbr.sha256 = ? AND sbr.deleted = 0
                 AND COALESCE(st.deleted, 0) = 0
             ) OR EXISTS(
               SELECT 1 FROM blob_upload_reservations
               WHERE workspace_id = ? AND sha256 = ? AND byte_size = ? AND expires_at > ?
             )",
        )
        .bind(&contract.workspace_id)
        .bind(&contract.sha256)
        .bind(&contract.workspace_id)
        .bind(&contract.sha256)
        .bind(contract.byte_size)
        .bind(crate::ids::now())
        .fetch_one(&mut *conn)
        .await?;
        if !admitted {
            bail!("error attachment-blob-unreserved");
        }
    }
    Ok(())
}
