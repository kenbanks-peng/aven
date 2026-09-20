use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::{Row, SqliteConnection};

use crate::attachments::storage::object_path;
use crate::attachments::validation::validate_sha256;
use crate::db::begin_immediate;

use super::filesystem::{
    FileMove, move_file_without_replacing, remove_files, restore_trashed_files, scan_directory_all,
    scan_directory_page,
};
use super::liveness::{is_protected, live_blob_references_sql, reconcile_liveness_bounded};
use super::{
    ByteCount, Clock, LEASE_TTL, LifecyclePolicy, PruneSummary, cutoff, staging_dir, timestamp,
    trash_dir,
};

async fn reconcile_trash_files(
    conn: &mut SqliteConnection,
    blob_dir: &Path,
    files: Vec<super::filesystem::ScannedFile>,
) -> Result<()> {
    let hashes = files
        .iter()
        .filter(|file| validate_sha256(&file.name).is_ok())
        .map(|file| file.name.clone())
        .collect::<Vec<_>>();
    let available = if hashes.is_empty() {
        HashSet::new()
    } else {
        sqlx::query_scalar::<_, String>(
            "SELECT sha256 FROM blob_inventory
             WHERE available = 1 AND sha256 IN (SELECT value FROM json_each(?))",
        )
        .bind(serde_json::to_string(&hashes)?)
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .collect()
    };
    let blob_dir = blob_dir.to_path_buf();
    crate::attachments::blocking::run(move || {
        for file in files {
            if validate_sha256(&file.name).is_err() {
                continue;
            }
            let target = object_path(&blob_dir, &file.name)?;
            if available.contains(&file.name) {
                if target.exists() {
                    fs::remove_file(file.path)?;
                } else {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    match move_file_without_replacing(&file.path, &target)? {
                        FileMove::Moved | FileMove::SourceMissing | FileMove::TargetExists => {}
                    }
                }
            } else {
                match fs::remove_file(file.path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    })
    .await
}

pub async fn reconcile_trash(conn: &mut SqliteConnection, blob_dir: &Path) -> Result<()> {
    reconcile_trash_files(
        conn,
        blob_dir,
        scan_directory_all(trash_dir(blob_dir)).await?,
    )
    .await
}

async fn reconcile_trash_page(
    conn: &mut SqliteConnection,
    blob_dir: &Path,
    limit: usize,
) -> Result<()> {
    reconcile_trash_files(
        conn,
        blob_dir,
        scan_directory_page(trash_dir(blob_dir), limit).await?,
    )
    .await
}

pub async fn reconcile_staging(blob_dir: &Path) -> Result<ByteCount> {
    let files = scan_directory_all(staging_dir(blob_dir)).await?;
    let stale = files
        .into_iter()
        .filter(|file| {
            file.name.starts_with(".aven-stage-")
                && file.modified.elapsed().is_ok_and(|age| age >= LEASE_TTL)
        })
        .collect::<Vec<_>>();
    let removed = ByteCount {
        count: u64::try_from(stale.len())?,
        bytes: stale.iter().map(|file| file.len).sum(),
    };
    remove_files(stale.into_iter().map(|file| file.path).collect()).await?;
    Ok(removed)
}

pub async fn reconcile_missing_objects(
    conn: &mut SqliteConnection,
    blob_dir: &Path,
    clock: &dyn Clock,
) -> Result<ByteCount> {
    let rows = sqlx::query(
        "SELECT sha256, byte_size FROM blob_inventory
         WHERE available = 1 ORDER BY sha256",
    )
    .fetch_all(&mut *conn)
    .await?;
    let verified_at = timestamp(clock.now());
    let mut missing = ByteCount::default();
    for row in rows {
        let sha256: String = row.get("sha256");
        if object_path(blob_dir, &sha256)?.exists() {
            continue;
        }
        sqlx::query(
            "UPDATE blob_inventory SET available = 0, last_verified_at = ?
             WHERE sha256 = ? AND available = 1",
        )
        .bind(&verified_at)
        .bind(&sha256)
        .execute(&mut *conn)
        .await?;
        missing.count += 1;
        missing.bytes += u64::try_from(row.get::<i64, _>("byte_size"))?;
    }
    Ok(missing)
}

async fn reconcile_object_directory(
    conn: &mut SqliteConnection,
    blob_dir: &Path,
    grace: Duration,
    limit: usize,
    clock: &dyn Clock,
) -> Result<ByteCount> {
    let files = scan_directory_page(staging_dir(blob_dir), limit).await?;
    let stale_staging = files
        .iter()
        .filter(|file| {
            file.name.starts_with(".aven-stage-")
                && file.modified.elapsed().is_ok_and(|age| age >= LEASE_TTL)
        })
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    remove_files(stale_staging).await?;

    let cutoff = clock.now() - chrono::Duration::from_std(grace)?;
    let canonical = files
        .iter()
        .filter(|file| {
            validate_sha256(&file.name).is_ok() && DateTime::<Utc>::from(file.modified) <= cutoff
        })
        .collect::<Vec<_>>();
    if canonical.is_empty() {
        return Ok(ByteCount::default());
    }
    let hashes = canonical
        .iter()
        .map(|file| file.name.clone())
        .collect::<Vec<_>>();
    let candidates = serde_json::to_string(&hashes)?;
    let now = timestamp(clock.now());
    let live_blob_references = live_blob_references_sql("candidate.value");
    let protected = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
        "SELECT candidate.value FROM json_each(?) candidate
         WHERE EXISTS(SELECT 1 FROM blob_inventory bi WHERE bi.sha256 = candidate.value)
            OR {live_blob_references}
            OR EXISTS(
              SELECT 1 FROM changes
              WHERE server_seq IS NULL AND op_type = 'attachment_add'
                AND json_extract(payload, '$.sha256') = candidate.value
            )
            OR EXISTS(
              SELECT 1 FROM blob_leases
              WHERE sha256 = candidate.value AND expires_at > ?
            )
            OR EXISTS(
              SELECT 1 FROM blob_upload_reservations
              WHERE sha256 = candidate.value AND expires_at > ?
            )"
    )))
    .bind(candidates)
    .bind(&now)
    .bind(&now)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect::<HashSet<_>>();
    let removable = canonical
        .into_iter()
        .filter(|file| !protected.contains(&file.name))
        .map(|file| (file.path.clone(), file.name.clone(), file.len))
        .collect::<Vec<_>>();
    if removable.is_empty() {
        return Ok(ByteCount::default());
    }
    let blob_dir = blob_dir.to_path_buf();
    crate::attachments::blocking::run(move || {
        let trash = trash_dir(&blob_dir);
        fs::create_dir_all(&trash)?;
        let mut removed = ByteCount::default();
        for (source, name, len) in removable {
            let target = trash.join(name);
            if move_file_without_replacing(&source, &target)? == FileMove::Moved {
                fs::remove_file(target)?;
                removed.count += 1;
                removed.bytes += len;
            }
        }
        Ok(removed)
    })
    .await
}

pub async fn reconcile_orphan_objects(
    conn: &mut SqliteConnection,
    blob_dir: &Path,
    grace: Duration,
    clock: &dyn Clock,
) -> Result<ByteCount> {
    reconcile_object_directory(conn, blob_dir, grace, usize::MAX, clock).await
}

pub async fn prune(
    conn: &mut SqliteConnection,
    blob_dir: &Path,
    policy: LifecyclePolicy,
    apply: bool,
    clock: &dyn Clock,
) -> Result<PruneSummary> {
    if apply {
        reconcile_trash_page(conn, blob_dir, policy.maintenance_limit).await?;
        reconcile_object_directory(
            conn,
            blob_dir,
            policy.grace,
            policy.maintenance_limit,
            clock,
        )
        .await?;
        reconcile_missing_objects(conn, blob_dir, clock).await?;
    }
    reconcile_liveness_bounded(conn, policy.maintenance_limit, clock).await?;
    let now = timestamp(clock.now());
    let cutoff = cutoff(clock.now(), policy.grace)?;
    let rows = sqlx::query(
        "SELECT bi.sha256, bi.byte_size
         FROM blob_inventory bi
         JOIN blob_lifecycle bl ON bl.sha256 = bi.sha256
         WHERE bi.available = 1 AND bl.unreferenced_at IS NOT NULL
           AND bl.unreferenced_at <= ?
         ORDER BY bl.unreferenced_at, bi.sha256 LIMIT ?",
    )
    .bind(&cutoff)
    .bind(i64::try_from(policy.maintenance_limit)?)
    .fetch_all(&mut *conn)
    .await?;
    let mut summary = PruneSummary::default();
    for row in rows {
        let sha256: String = row.get("sha256");
        let byte_size: i64 = row.get("byte_size");
        if is_protected(conn, &sha256, &now).await? {
            continue;
        }
        summary.eligible.count += 1;
        summary.eligible.bytes += u64::try_from(byte_size)?;
        if !apply {
            continue;
        }
        let mut tx = begin_immediate(conn).await?;
        let still_eligible: bool = sqlx::query_scalar(
            "SELECT EXISTS(
               SELECT 1 FROM blob_inventory bi
               JOIN blob_lifecycle bl ON bl.sha256 = bi.sha256
               WHERE bi.sha256 = ? AND bi.available = 1
                 AND bl.unreferenced_at IS NOT NULL AND bl.unreferenced_at <= ?
             )",
        )
        .bind(&sha256)
        .bind(&cutoff)
        .fetch_one(&mut *tx)
        .await?;
        if !still_eligible || is_protected(&mut tx, &sha256, &now).await? {
            tx.rollback().await?;
            continue;
        }
        let source = object_path(blob_dir, &sha256)?;
        let trash = trash_dir(blob_dir);
        let trashed = trash.join(&sha256);
        let source_for_move = source.clone();
        let trashed_for_move = trashed.clone();
        let moved = crate::attachments::blocking::run(move || {
            fs::create_dir_all(&trash)?;
            move_file_without_replacing(&source_for_move, &trashed_for_move)
        })
        .await?;
        if moved == FileMove::TargetExists {
            tx.rollback().await?;
            continue;
        }
        if let Err(error) = sqlx::query(
            "UPDATE blob_inventory SET available = 0, last_verified_at = ? WHERE sha256 = ?",
        )
        .bind(&now)
        .bind(&sha256)
        .execute(&mut *tx)
        .await
        {
            let _ = tx.rollback().await;
            if moved == FileMove::Moved {
                restore_trashed_files(vec![(source, trashed)]).await;
            }
            return Err(error.into());
        }
        if let Err(error) = tx.commit().await {
            if moved == FileMove::Moved {
                restore_trashed_files(vec![(source, trashed)]).await;
            }
            return Err(error.into());
        }
        if moved == FileMove::Moved {
            remove_files(vec![trashed]).await?;
        }
        summary.pruned.count += 1;
        summary.pruned.bytes += u64::try_from(byte_size)?;
    }
    let blob_dir = blob_dir.to_path_buf();
    crate::attachments::blocking::run(move || {
        prune_preview_cache(&blob_dir, policy.preview_quota_bytes)
    })
    .await?;
    Ok(summary)
}

pub fn prune_preview_cache(blob_dir: &Path, quota: u64) -> Result<ByteCount> {
    let root = blob_dir.join("cache").join("previews");
    if !root.exists() {
        return Ok(ByteCount::default());
    }
    let mut files = Vec::new();
    let mut dirs = vec![root];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                dirs.push(entry.path());
            } else if entry.file_type()?.is_file() {
                let metadata = entry.metadata()?;
                files.push((metadata.modified()?, metadata.len(), entry.path()));
            }
        }
    }
    let mut total: u64 = files.iter().map(|(_, size, _)| *size).sum();
    files.sort_by_key(|(modified, _, path)| (*modified, path.clone()));
    let mut removed = ByteCount::default();
    for (_, size, path) in files {
        if total <= quota {
            break;
        }
        fs::remove_file(path)?;
        total -= size;
        removed.count += 1;
        removed.bytes += size;
    }
    Ok(removed)
}
