use anyhow::Result;
use sqlx::{SqliteConnection, query_scalar};
use std::path::Path;

use super::IntegrityCheck;

mod attachments;
mod recurrence;

pub(crate) async fn recurrence_integrity_checks(
    conn: &mut SqliteConnection,
) -> Result<Vec<IntegrityCheck>> {
    recurrence::recurrence_integrity_checks(conn).await
}

pub(crate) async fn attachment_integrity_checks(
    conn: &mut SqliteConnection,
    blob_dir: &Path,
    deep: bool,
) -> Result<Vec<IntegrityCheck>> {
    attachments::attachment_integrity_checks(conn, blob_dir, deep).await
}

async fn count_check(
    conn: &mut SqliteConnection,
    label: &'static str,
    query: &'static str,
) -> Result<IntegrityCheck> {
    let count: i64 = query_scalar(query).fetch_one(&mut *conn).await?;
    Ok(IntegrityCheck {
        label,
        ok: count == 0,
        value: format!("{count} orphaned"),
    })
}
