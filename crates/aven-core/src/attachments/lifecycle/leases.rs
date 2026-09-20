use anyhow::{Result, bail};
use sqlx::SqliteConnection;

use crate::attachments::validation::validate_sha256;
use crate::ids::new_id;

use super::{Clock, LEASE_TTL, timestamp};

pub async fn acquire_lease(
    conn: &mut SqliteConnection,
    sha256: &str,
    kind: &str,
    clock: &dyn Clock,
) -> Result<String> {
    validate_sha256(sha256)?;
    if !matches!(kind, "staging" | "read" | "backup" | "transfer") {
        bail!("error attachment-lease-kind-invalid");
    }
    let lease_id = new_id();
    let now = clock.now();
    let expires = now + chrono::Duration::from_std(LEASE_TTL)?;
    sqlx::query(
        "INSERT INTO blob_leases(lease_id, sha256, kind, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&lease_id)
    .bind(sha256)
    .bind(kind)
    .bind(timestamp(now))
    .bind(timestamp(expires))
    .execute(&mut *conn)
    .await?;
    Ok(lease_id)
}

pub async fn release_lease(conn: &mut SqliteConnection, lease_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM blob_leases WHERE lease_id = ?")
        .bind(lease_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}
