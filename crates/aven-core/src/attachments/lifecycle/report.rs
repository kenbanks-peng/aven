use std::fs;
use std::path::Path;

use anyhow::Result;
use sqlx::{Row, SqliteConnection};

use crate::attachments::validation::validate_sha256;

use super::liveness::{is_protected, live_blob_references_sql};
use super::{
    Clock, LifecyclePolicy, LifecycleReport, cutoff, reconcile_liveness, staging_dir, timestamp,
    trash_dir,
};

pub async fn lifecycle_report(
    conn: &mut SqliteConnection,
    blob_dir: &Path,
    policy: LifecyclePolicy,
    clock: &dyn Clock,
) -> Result<LifecycleReport> {
    reconcile_liveness(conn, clock).await?;
    let now = timestamp(clock.now());
    let cutoff = cutoff(clock.now(), policy.grace)?;
    let live_blob_references = live_blob_references_sql("bi.sha256");
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT bi.sha256, bi.byte_size, bi.available, bl.unreferenced_at,
           {live_blob_references} AS referenced
         FROM blob_inventory bi LEFT JOIN blob_lifecycle bl ON bl.sha256 = bi.sha256"
    )))
    .fetch_all(&mut *conn)
    .await?;
    let mut report = LifecycleReport::default();
    for row in rows {
        let sha256: String = row.get("sha256");
        let bytes = u64::try_from(row.get::<i64, _>("byte_size"))?;
        let available = row.get::<i64, _>("available") != 0;
        let referenced = row.get::<i64, _>("referenced") != 0;
        let unreferenced_at: Option<String> = row.get("unreferenced_at");
        if available {
            report.quota.count += 1;
            report.quota.bytes += bytes;
        }
        if referenced {
            report.referenced.count += 1;
            report.referenced.bytes += bytes;
        } else if is_protected(conn, &sha256, &now).await? {
            report.protected.count += 1;
            report.protected.bytes += bytes;
        } else if unreferenced_at
            .as_deref()
            .is_some_and(|at| at <= cutoff.as_str())
        {
            report.eligible.count += 1;
            report.eligible.bytes += bytes;
        } else {
            report.grace_period.count += 1;
            report.grace_period.bytes += bytes;
        }
        if referenced && unreferenced_at.is_some() || !referenced && unreferenced_at.is_none() {
            report.inconsistencies.count += 1;
            report.inconsistencies.bytes += bytes;
        }
    }
    let (reservation_count, reservation_bytes): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(byte_size), 0)
         FROM blob_upload_reservations WHERE expires_at > ?",
    )
    .bind(&now)
    .fetch_one(&mut *conn)
    .await?;
    report.reservations = super::ByteCount {
        count: u64::try_from(reservation_count)?,
        bytes: u64::try_from(reservation_bytes)?,
    };
    for entry in fs::read_dir(staging_dir(blob_dir))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = entry.metadata()?;
        if name.starts_with(".aven-stage-") {
            report.staging.count += 1;
            report.staging.bytes += metadata.len();
        } else if validate_sha256(&name).is_ok() {
            let tracked: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM blob_inventory WHERE sha256 = ?)")
                    .bind(&name)
                    .fetch_one(&mut *conn)
                    .await?;
            if !tracked {
                report.staging.count += 1;
                report.staging.bytes += metadata.len();
                report.inconsistencies.count += 1;
                report.inconsistencies.bytes += metadata.len();
            }
        }
    }
    for entry in fs::read_dir(trash_dir(blob_dir))
        .into_iter()
        .flatten()
        .flatten()
    {
        if entry.file_type()?.is_file() {
            report.trash.count += 1;
            report.trash.bytes += entry.metadata()?.len();
        }
    }
    Ok(report)
}
