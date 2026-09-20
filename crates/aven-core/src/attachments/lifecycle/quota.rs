use std::path::Path;

use anyhow::{Result, bail};
use sqlx::SqliteConnection;

use crate::attachments::validation::validate_sha256;
use crate::db::begin_immediate;
use crate::ids::new_id;

use super::{Clock, LEASE_TTL, LifecyclePolicy, timestamp};

pub async fn reserve_upload(
    conn: &mut SqliteConnection,
    workspace_id: &str,
    sha256: &str,
    byte_size: i64,
    quota_bytes: i64,
    clock: &dyn Clock,
) -> Result<Option<String>> {
    validate_sha256(sha256)?;
    let mut tx = begin_immediate(conn).await?;
    let existing: bool = sqlx::query_scalar(
        "SELECT EXISTS(
           SELECT 1 FROM server_blob_references sbr
           LEFT JOIN server_task_tombstones st
             ON st.workspace_id = sbr.workspace_id AND st.task_id = sbr.task_id
           WHERE sbr.workspace_id = ? AND sbr.sha256 = ? AND sbr.deleted = 0
             AND COALESCE(st.deleted, 0) = 0
         )",
    )
    .bind(workspace_id)
    .bind(sha256)
    .fetch_one(&mut *tx)
    .await?;
    if existing {
        tx.commit().await?;
        return Ok(None);
    }
    let used: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(byte_size), 0) FROM (
           SELECT sbr.sha256, MAX(sbr.byte_size) AS byte_size
           FROM server_blob_references sbr
           LEFT JOIN server_task_tombstones st
             ON st.workspace_id = sbr.workspace_id AND st.task_id = sbr.task_id
           WHERE sbr.workspace_id = ? AND sbr.deleted = 0 AND COALESCE(st.deleted, 0) = 0
           GROUP BY sbr.sha256
         )",
    )
    .bind(workspace_id)
    .fetch_one(&mut *tx)
    .await?;
    let reserved: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(byte_size), 0) FROM blob_upload_reservations
         WHERE workspace_id = ? AND sha256 != ? AND expires_at > ?",
    )
    .bind(workspace_id)
    .bind(sha256)
    .bind(timestamp(clock.now()))
    .fetch_one(&mut *tx)
    .await?;
    if used.saturating_add(reserved).saturating_add(byte_size) > quota_bytes {
        tx.rollback().await?;
        bail!("error attachment-quota-exceeded");
    }
    let reservation_id = new_id();
    let now = clock.now();
    let expires = now + chrono::Duration::from_std(LEASE_TTL)?;
    sqlx::query(
        "INSERT INTO blob_upload_reservations(
           reservation_id, workspace_id, sha256, byte_size, created_at, expires_at
         ) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(workspace_id, sha256) DO UPDATE SET
           reservation_id = excluded.reservation_id,
           byte_size = excluded.byte_size,
           created_at = excluded.created_at,
           expires_at = excluded.expires_at",
    )
    .bind(&reservation_id)
    .bind(workspace_id)
    .bind(sha256)
    .bind(byte_size)
    .bind(timestamp(now))
    .bind(timestamp(expires))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(reservation_id))
}

pub async fn release_reservation(conn: &mut SqliteConnection, reservation_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM blob_upload_reservations WHERE reservation_id = ?")
        .bind(reservation_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub async fn local_unique_bytes(conn: &mut SqliteConnection) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(SUM(byte_size), 0) FROM blob_inventory WHERE available = 1",
    )
    .fetch_one(&mut *conn)
    .await?)
}

pub async fn ensure_local_capacity(
    conn: &mut SqliteConnection,
    blob_dir: &Path,
    sha256: &str,
    byte_size: i64,
    policy: LifecyclePolicy,
    clock: &dyn Clock,
) -> Result<Option<String>> {
    let used = local_unique_bytes(conn).await?;
    if used.saturating_add(byte_size) > policy.quota_bytes {
        super::maintenance::prune(conn, blob_dir, policy, true, clock).await?;
    }
    let now = clock.now();
    let mut tx = begin_immediate(conn).await?;
    let existing: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM blob_inventory WHERE sha256 = ? AND available = 1)",
    )
    .bind(sha256)
    .fetch_one(&mut *tx)
    .await?;
    if existing {
        tx.commit().await?;
        return Ok(None);
    }
    let used: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(byte_size), 0) FROM blob_inventory WHERE available = 1",
    )
    .fetch_one(&mut *tx)
    .await?;
    let reserved: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(byte_size), 0) FROM blob_upload_reservations
         WHERE workspace_id = '__local__' AND sha256 != ? AND expires_at > ?",
    )
    .bind(sha256)
    .bind(timestamp(now))
    .fetch_one(&mut *tx)
    .await?;
    if used.saturating_add(reserved).saturating_add(byte_size) > policy.quota_bytes {
        tx.rollback().await?;
        bail!("error attachment-quota-exceeded");
    }
    let reservation_id = new_id();
    let expires = now + chrono::Duration::from_std(LEASE_TTL)?;
    sqlx::query(
        "INSERT INTO blob_upload_reservations(
           reservation_id, workspace_id, sha256, byte_size, created_at, expires_at
         ) VALUES (?, '__local__', ?, ?, ?, ?)
         ON CONFLICT(workspace_id, sha256) DO UPDATE SET
           reservation_id = excluded.reservation_id,
           byte_size = excluded.byte_size,
           created_at = excluded.created_at,
           expires_at = excluded.expires_at",
    )
    .bind(&reservation_id)
    .bind(sha256)
    .bind(byte_size)
    .bind(timestamp(now))
    .bind(timestamp(expires))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(reservation_id))
}
