use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection as _, SqliteConnection, SqlitePool};

use super::MIGRATOR;

const MIGRATION_BACKUP_KEEP: usize = 20;

pub(super) async fn backup_before_pending_migrations(
    path: &Path,
    existed_before_open: bool,
    pool: &SqlitePool,
) -> Result<()> {
    if !migration_backups_enabled() || !existed_before_open || !has_pending_migrations(pool).await?
    {
        return Ok(());
    }
    let backup_path = migration_backup_path(path)?;
    let mut conn = pool.acquire().await?;
    backup_database_with_connection(&mut conn, &backup_path).await?;
    prune_migration_backups(path)?;
    Ok(())
}

fn migration_backups_enabled() -> bool {
    std::env::var_os("AVEN_DEV_MIGRATION_BACKUPS").is_some()
}

async fn has_pending_migrations(pool: &SqlitePool) -> Result<bool> {
    let applied_versions =
        match sqlx::query_scalar::<_, i64>("SELECT version FROM _sqlx_migrations")
            .fetch_all(pool)
            .await
        {
            Ok(versions) => versions,
            Err(error) => {
                let Some(db_error) = error.as_database_error() else {
                    return Err(error.into());
                };
                if db_error.code().as_deref() == Some("1") {
                    return Ok(MIGRATOR.iter().next().is_some());
                }
                return Err(error.into());
            }
        };
    Ok(MIGRATOR
        .iter()
        .any(|migration| !applied_versions.contains(&migration.version)))
}

fn migration_backup_path(path: &Path) -> Result<PathBuf> {
    default_sqlite_backup_path(path, "before-migrate")
}

pub fn default_backup_path(path: &Path, reason: &str) -> Result<PathBuf> {
    backup_path_with_extension(path, reason, "aven-backup.tar.zst")
}

pub fn default_sqlite_backup_path(path: &Path, reason: &str) -> Result<PathBuf> {
    backup_path_with_extension(path, reason, "sqlite")
}

fn backup_path_with_extension(path: &Path, reason: &str, extension: &str) -> Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let backup_dir = parent.join("backups");
    fs::create_dir_all(&backup_dir)
        .with_context(|| format!("could not create {}", backup_dir.display()))?;
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("db.sqlite");
    Ok(backup_dir.join(format!(
        "{stem}.{reason}-{}.{}",
        backup_timestamp()?,
        extension
    )))
}

pub async fn backup_database(source: &Path, backup: &Path) -> Result<()> {
    if !source.is_file() {
        bail!("could not open source {}", source.display());
    }
    if let Some(parent) = backup.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let mut conn = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(source)
            .read_only(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5)),
    )
    .await
    .with_context(|| format!("could not open source {}", source.display()))?;
    backup_database_with_connection(&mut conn, backup).await
}

pub fn wal_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", path.display()))
}

pub fn shm_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-shm", path.display()))
}

pub async fn restore_database_file(target: &Path, source: &Path) -> Result<PathBuf> {
    validate_sqlite_source(source).await?;
    let safety = create_restore_safety_backup(target).await?;
    let staging = target.with_extension("restore-staging");
    if staging.exists() {
        fs::remove_file(&staging)
            .with_context(|| format!("could not remove {}", staging.display()))?;
    }
    fs::copy(source, &staging).with_context(|| {
        format!(
            "could not copy {} -> {}",
            source.display(),
            staging.display()
        )
    })?;
    for sidecar in [wal_path(target), shm_path(target)] {
        if sidecar.exists() {
            fs::remove_file(&sidecar)
                .with_context(|| format!("could not remove {}", sidecar.display()))?;
        }
    }
    fs::rename(&staging, target)
        .with_context(|| format!("could not replace {}", target.display()))?;
    Ok(safety)
}

pub(crate) async fn create_restore_safety_backup(target: &Path) -> Result<PathBuf> {
    let safety = default_sqlite_backup_path(target, "before-restore")?;
    if target.exists() {
        backup_database(target, &safety).await?;
    } else {
        SqliteConnection::connect_with(
            &SqliteConnectOptions::new()
                .filename(&safety)
                .create_if_missing(true),
        )
        .await
        .with_context(|| format!("could not create {}", safety.display()))?
        .close()
        .await?;
    }
    Ok(safety)
}

async fn validate_sqlite_source(source: &Path) -> Result<()> {
    let mut conn = sqlx::SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(source)
            .read_only(true)
            .foreign_keys(true),
    )
    .await
    .with_context(|| format!("could not open source {}", source.display()))?;
    let quick_check: String = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_one(&mut conn)
        .await?;
    if quick_check != "ok" {
        bail!("error backup-source-corrupt quick_check={quick_check}");
    }
    Ok(())
}

fn backup_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?
        .as_secs())
}

pub(crate) async fn backup_database_with_connection(
    conn: &mut SqliteConnection,
    backup: &Path,
) -> Result<()> {
    let parent = backup.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let staging_dir = tempfile::Builder::new()
        .prefix(".aven-sqlite-backup-")
        .tempdir_in(parent)
        .with_context(|| format!("could not create backup staging in {}", parent.display()))?;
    let staging = staging_dir.path().join("database.sqlite");
    sqlx::query("VACUUM INTO ?")
        .bind(staging.display().to_string())
        .execute(&mut *conn)
        .await
        .with_context(|| format!("could not back up database to {}", backup.display()))?;
    fs::rename(&staging, backup)
        .with_context(|| format!("could not replace {}", backup.display()))?;
    Ok(())
}

fn prune_migration_backups(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let backup_dir = parent.join("backups");
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let prefix = format!("{file_name}.before-migrate-");
    let mut backups = fs::read_dir(&backup_dir)
        .with_context(|| format!("could not read {}", backup_dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".sqlite"))
        })
        .collect::<Vec<_>>();
    backups.sort_by_key(|entry| entry.file_name());
    let remove_count = backups.len().saturating_sub(MIGRATION_BACKUP_KEEP);
    for entry in backups.into_iter().take(remove_count) {
        let path = entry.path();
        fs::remove_file(&path).with_context(|| format!("could not remove {}", path.display()))?;
    }
    Ok(())
}
