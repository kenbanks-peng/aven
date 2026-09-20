use std::path::PathBuf;

use clap::{Args, Subcommand};

pub(super) const BACKUP_HELP: &str = r#"Examples:
  aven backup --output backup.aven-backup.tar.zst
  aven backup restore backup.aven-backup.tar.zst --yes

Backup archives include the SQLite database and attachment objects available on
this device. Sync first when remote attachment objects may be missing. Restore
replaces local data, creates a safety backup, and requires --yes."#;

pub(super) const EXPORT_HELP: &str = r#"Portable JSON contains task data but no attachment bytes. Use `aven backup` for
attachment objects available on this device, and sync first when remote objects
may be missing."#;

pub(super) const IMPORT_HELP: &str = r#"Import validates portable JSON before replacing local data, creates a safety
backup, and requires --yes. Portable imports do not contain attachment bytes."#;

#[derive(Args)]
pub(crate) struct BackupCommand {
    #[command(subcommand)]
    pub(crate) command: Option<BackupSubcommand>,
    /// Write the backup archive to this path instead of the generated path
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,
}

#[derive(Subcommand)]
pub(crate) enum BackupSubcommand {
    /// Restore the database from a backup
    #[command(after_long_help = BACKUP_HELP)]
    Restore(BackupRestoreArgs),
}

#[derive(Args)]
pub(crate) struct BackupRestoreArgs {
    /// Backup archive or SQLite database file to restore
    pub(crate) path: PathBuf,
    /// Confirm replacement of local data
    #[arg(long)]
    pub(crate) yes: bool,
}

#[derive(Args)]
pub(crate) struct ExportArgs {
    /// Destination for portable JSON without attachment bytes
    #[arg(long)]
    pub(crate) output: PathBuf,
}

#[derive(Args)]
pub(crate) struct ImportArgs {
    /// Portable JSON export to validate and import
    pub(crate) path: PathBuf,
    /// Confirm replacement of local data
    #[arg(long)]
    pub(crate) yes: bool,
}

#[derive(Args)]
pub(crate) struct DoctorArgs {
    /// Run deeper read-only SQLite, relationship, and attachment checks
    #[arg(long)]
    pub(crate) integrity: bool,
    /// Print machine-readable JSON
    #[arg(long)]
    pub(crate) json: bool,
    /// Exit nonzero when the report contains error-level findings
    #[arg(long)]
    pub(crate) fail_on_error: bool,
}

#[derive(Args)]
pub(crate) struct AttachmentCommand {
    #[command(subcommand)]
    pub(crate) command: AttachmentSubcommand,
}

#[derive(Subcommand)]
pub(crate) enum AttachmentSubcommand {
    /// Attach a file to a task
    Add(AttachmentAddArgs),
    /// List attachments for a task
    List(AttachmentListArgs),
    /// Get attachment metadata and optionally write bytes
    Get(AttachmentGetArgs),
    /// Delete (tombstone) an attachment
    Delete(AttachmentDeleteArgs),
    /// Inspect or prune eligible attachment blobs
    Prune(AttachmentPruneArgs),
}

#[derive(Args)]
pub(crate) struct AttachmentAddArgs {
    /// Task ref to receive the attachment
    pub(crate) task_ref: String,
    /// Image file to attach
    pub(crate) path: PathBuf,
    /// Alternative text for the image
    #[arg(long)]
    pub(crate) alt: Option<String>,
    /// Override the filename stored in metadata
    #[arg(long)]
    pub(crate) filename: Option<String>,
    /// Declared media type, checked against the image bytes
    #[arg(long = "media-type")]
    pub(crate) media_type: Option<String>,
    /// Optimize supported image formats before storing bytes
    #[arg(long, conflicts_with = "no_optimize")]
    pub(crate) optimize: bool,
    /// Preserve attachment bytes exactly
    #[arg(long)]
    pub(crate) no_optimize: bool,
    /// Print machine-readable JSON
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Args)]
pub(crate) struct AttachmentListArgs {
    /// Task ref whose attachments to list
    pub(crate) task_ref: String,
    /// Include deleted (tombstoned) attachments
    #[arg(long)]
    pub(crate) all: bool,
    /// Print machine-readable JSON
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Args)]
pub(crate) struct AttachmentGetArgs {
    /// Exact attachment ID printed by `attachment add` or `attachment list`
    pub(crate) attachment_id: String,
    /// Write bytes to this path
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,
    /// Include deleted attachments
    #[arg(long)]
    pub(crate) all: bool,
    /// Print machine-readable JSON
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Args)]
pub(crate) struct AttachmentDeleteArgs {
    /// Exact attachment ID to tombstone
    pub(crate) attachment_id: String,
    /// Print machine-readable JSON
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Args)]
pub(crate) struct AttachmentPruneArgs {
    /// Apply deletion. The default is a dry run.
    #[arg(long, conflicts_with = "dry_run")]
    pub(crate) apply: bool,
    /// Inspect eligible blobs without deleting them
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Print machine-readable JSON
    #[arg(long)]
    pub(crate) json: bool,
}

impl AttachmentSubcommand {
    pub(crate) fn wakes_daemon(&self) -> bool {
        matches!(self, Self::Add(_) | Self::Delete(_))
    }
}
