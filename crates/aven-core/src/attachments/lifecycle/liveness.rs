use anyhow::Result;
use sqlx::SqliteConnection;

use crate::db::{begin_immediate, get_meta, set_meta};

use super::{Clock, timestamp};

const LIVENESS_CURSOR_META_KEY: &str = "attachment_liveness_cursor";

pub(super) fn live_blob_references_sql(sha256_expr: &str) -> String {
    format!(
        "EXISTS(
           SELECT 1 FROM task_attachments ta
           JOIN tasks t ON t.workspace_id = ta.workspace_id AND t.id = ta.task_id
           WHERE ta.sha256 = {sha256_expr} AND ta.deleted = 0 AND t.deleted = 0
         ) OR EXISTS(
           SELECT 1 FROM server_blob_references sbr
           LEFT JOIN server_task_tombstones st
             ON st.workspace_id = sbr.workspace_id AND st.task_id = sbr.task_id
           WHERE sbr.sha256 = {sha256_expr} AND sbr.deleted = 0
             AND COALESCE(st.deleted, 0) = 0
         )"
    )
}

pub async fn reconcile_liveness(conn: &mut SqliteConnection, clock: &dyn Clock) -> Result<()> {
    let mut tx = begin_immediate(conn).await?;
    reconcile_liveness_in_transaction(&mut tx, clock).await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn reconcile_liveness_in_transaction(
    conn: &mut SqliteConnection,
    clock: &dyn Clock,
) -> Result<()> {
    let now = timestamp(clock.now());
    sqlx::query("DELETE FROM blob_leases WHERE expires_at <= ?")
        .bind(&now)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM blob_upload_reservations WHERE expires_at <= ?")
        .bind(&now)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT OR IGNORE INTO blob_lifecycle(sha256, unreferenced_at)
         SELECT sha256, NULL FROM blob_inventory",
    )
    .execute(&mut *conn)
    .await?;
    let live_blob_references = live_blob_references_sql("blob_lifecycle.sha256");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE blob_lifecycle SET unreferenced_at = NULL
         WHERE unreferenced_at IS NOT NULL
           AND ({live_blob_references})"
    )))
    .execute(&mut *conn)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE blob_lifecycle SET unreferenced_at = ?
         WHERE unreferenced_at IS NULL
           AND NOT ({live_blob_references})"
    )))
    .bind(&now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub(crate) async fn reconcile_liveness_for_hashes_in_transaction(
    conn: &mut SqliteConnection,
    hashes: &[String],
    clock: &dyn Clock,
) -> Result<()> {
    if hashes.is_empty() {
        return Ok(());
    }
    let now = timestamp(clock.now());
    let hashes = serde_json::to_string(hashes)?;
    let live_inventory_references = live_blob_references_sql("bi.sha256");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT OR IGNORE INTO blob_lifecycle(sha256, unreferenced_at)
         SELECT bi.sha256,
                CASE WHEN ({live_inventory_references}) THEN NULL ELSE ? END
         FROM blob_inventory bi
         JOIN (SELECT DISTINCT value AS sha256 FROM json_each(?)) affected
           ON affected.sha256 = bi.sha256"
    )))
    .bind(&now)
    .bind(&hashes)
    .execute(&mut *conn)
    .await?;

    let live_blob_references = live_blob_references_sql("blob_lifecycle.sha256");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE blob_lifecycle SET unreferenced_at = NULL
         WHERE unreferenced_at IS NOT NULL
           AND sha256 IN (SELECT value FROM json_each(?))
           AND ({live_blob_references})"
    )))
    .bind(&hashes)
    .execute(&mut *conn)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE blob_lifecycle SET unreferenced_at = ?
         WHERE unreferenced_at IS NULL
           AND sha256 IN (SELECT value FROM json_each(?))
           AND NOT ({live_blob_references})"
    )))
    .bind(&now)
    .bind(&hashes)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub(super) async fn reconcile_liveness_bounded(
    conn: &mut SqliteConnection,
    limit: usize,
    clock: &dyn Clock,
) -> Result<()> {
    let mut tx = begin_immediate(conn).await?;
    let now = timestamp(clock.now());
    sqlx::query("DELETE FROM blob_leases WHERE expires_at <= ?")
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM blob_upload_reservations WHERE expires_at <= ?")
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    if limit == 0 {
        tx.commit().await?;
        return Ok(());
    }

    let cursor = get_meta(&mut tx, LIVENESS_CURSOR_META_KEY)
        .await?
        .unwrap_or_default();
    let hashes = sqlx::query_scalar::<_, String>(
        "SELECT sha256 FROM blob_inventory
         WHERE sha256 > ? ORDER BY sha256 LIMIT ?",
    )
    .bind(&cursor)
    .bind(i64::try_from(limit)?)
    .fetch_all(&mut *tx)
    .await?;
    reconcile_liveness_for_hashes_in_transaction(&mut tx, &hashes, clock).await?;
    set_meta(
        &mut tx,
        LIVENESS_CURSOR_META_KEY,
        if hashes.len() < limit {
            ""
        } else {
            hashes.last().map(String::as_str).unwrap_or_default()
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn is_protected(
    conn: &mut SqliteConnection,
    sha256: &str,
    now: &str,
) -> Result<bool> {
    let live_blob_references = live_blob_references_sql("?");
    Ok(sqlx::query_scalar::<_, bool>(sqlx::AssertSqlSafe(format!(
        "SELECT
           {live_blob_references} OR EXISTS(
             SELECT 1 FROM changes
             WHERE server_seq IS NULL AND op_type = 'attachment_add'
               AND json_extract(payload, '$.sha256') = ?
           ) OR EXISTS(
             SELECT 1 FROM blob_leases WHERE sha256 = ? AND expires_at > ?
           ) OR EXISTS(
             SELECT 1 FROM blob_upload_reservations WHERE sha256 = ? AND expires_at > ?
           )"
    )))
    .bind(sha256)
    .bind(sha256)
    .bind(sha256)
    .bind(sha256)
    .bind(now)
    .bind(sha256)
    .bind(now)
    .fetch_one(&mut *conn)
    .await?)
}
