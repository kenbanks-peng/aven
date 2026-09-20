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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::test_support::TestClock;
    use super::super::*;
    use super::*;
    use crate::attachments::storage::{object_path, upsert_inventory_available};
    use crate::db::open_db;

    #[tokio::test]
    async fn lease_protects_expired_unreferenced_blob_until_release() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("test.sqlite");
        let pool = open_db(&db_path).await.unwrap();
        let mut conn = pool.acquire().await.unwrap();
        let blob_dir = temp.path().join("blobs");
        let hash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        upsert_inventory_available(&mut conn, hash, 4, "image/png")
            .await
            .unwrap();
        let path = object_path(&blob_dir, hash).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"blob").unwrap();
        let clock = TestClock::at("2026-07-10T00:00:00Z");
        reconcile_liveness(&mut conn, &clock).await.unwrap();
        clock.advance(chrono::Duration::days(8));
        let lease = acquire_lease(&mut conn, hash, "backup", &clock)
            .await
            .unwrap();
        let policy = LifecyclePolicy::default();
        let blocked = prune(&mut conn, &blob_dir, policy, true, &clock)
            .await
            .unwrap();
        assert_eq!(blocked.pruned.count, 0);
        assert!(path.exists());

        release_lease(&mut conn, &lease).await.unwrap();
        let pruned = prune(&mut conn, &blob_dir, policy, true, &clock)
            .await
            .unwrap();
        assert_eq!(pruned.pruned.count, 1);
        assert!(!path.exists());
    }
}
