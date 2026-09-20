use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Subcommand};

pub(super) const CONFLICT_EXAMPLES: &str = r#"Examples:
  aven conflict show APP-7KQ9
  aven conflict diff APP-7KQ9 description
  aven conflict resolve APP-7KQ9 description --use VARIANT_TOKEN
  aven conflict resolve APP-7KQ9 description --value-file resolved.md

Inspect both variants before resolving. Variant tokens come from `conflict show`.
--use takes precedence over explicit values. Without --use, supply exactly one
of --value, --value-file, or --value-stdin."#;

pub(super) const SERVER_HELP: &str = r#"Loopback binds may run without authentication. Private and public binds require
sync.auth_token in the configuration file. Public binds also require
--unsafe-public-bind. Aven does not provide TLS termination."#;

pub(super) const SYNC_HELP: &str = r#"The server URL comes from --server, AVEN_SYNC_SERVER, or sync.server_url, in
that order. Authentication and other sync settings live in the configuration
file. Run `aven config show` to inspect the active file and `aven doctor` to
diagnose routing and sync configuration."#;

pub(super) const PAIR_HELP: &str = r#"Pairing reads configuration and produces an invitation without opening a task
database or contacting the sync server. The invitation requires a nonempty
sync.auth_token and a phone-reachable HTTP or HTTPS server URL. Use --server
when the configured URL is loopback or available only from the desktop.

Use --copy on the local desktop to put the invitation on the clipboard instead
of displaying a QR code. The invitation contains credentials; clipboard history
and sharing services may retain it. SSH clipboard copying is not supported."#;

#[derive(Args)]
pub(crate) struct ConflictCommand {
    #[command(subcommand)]
    pub(crate) command: ConflictSubcommand,
}

#[derive(Subcommand)]
pub(crate) enum ConflictSubcommand {
    /// List unresolved sync conflicts
    List {
        /// Restrict conflicts to a project by key or name
        #[arg(long)]
        project: Option<String>,
        /// Restrict conflicts to a field name
        #[arg(long)]
        field: Option<String>,
        #[arg(
            long,
            value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..),
            help = "Maximum result count (must be at least 1)"
        )]
        limit: Option<usize>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    /// Show a conflict as a text diff
    Diff {
        /// Task ref with the conflict
        task_ref: String,
        /// Conflicted field name
        field: String,
    },
    /// Export conflicting values to files
    Export {
        /// Task ref with the conflict
        task_ref: String,
        /// Conflicted field name
        field: String,
        /// Directory to receive one file per variant
        #[arg(long)]
        dir: PathBuf,
    },
    /// Show conflict details for a task
    Show {
        /// Task or recurring-series ref with conflicts
        task_ref: String,
        /// Restrict output to one field
        #[arg(long)]
        field: Option<String>,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    /// Resolve a sync conflict
    #[command(after_long_help = CONFLICT_EXAMPLES)]
    Resolve {
        /// Task or recurring-series ref with the conflict
        task_ref: String,
        /// Conflicted field name
        field: String,
        /// Select an exact variant token printed by `conflict show`
        #[arg(long = "use")]
        use_variant: Option<String>,
        /// Resolve with this explicit value
        #[arg(long)]
        value: Option<String>,
        /// Read the explicit resolution value from a UTF-8 file
        #[arg(long)]
        value_file: Option<PathBuf>,
        /// Read the explicit resolution value from standard input
        #[arg(long)]
        value_stdin: bool,
    },
}

#[derive(Args)]
pub(crate) struct DaemonArgs {
    #[command(subcommand)]
    pub(crate) command: Option<DaemonSubcommand>,
}

#[derive(Subcommand)]
pub(crate) enum DaemonSubcommand {
    /// Report daemon installation and runtime health without changing it
    Status(StatusArgs),
    /// Install the background daemon
    Install(DaemonInstallArgs),
    /// Uninstall the background daemon
    Uninstall,
    /// Restart the background daemon
    Restart,
    /// Repair the background daemon installation
    Repair(DaemonRepairArgs),
}

#[derive(Args)]
pub(crate) struct DaemonInstallArgs {
    #[arg(
        long,
        value_name = "PATH",
        help = "Write this executable path into the LaunchAgent"
    )]
    pub(crate) program: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct DaemonRepairArgs {
    #[arg(long, help = "Succeed without changes when the LaunchAgent is absent")]
    pub(crate) if_installed: bool,
    #[arg(
        long,
        value_name = "PATH",
        help = "Write this executable path into the LaunchAgent"
    )]
    pub(crate) program: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct ServerArgs {
    /// Listen address; port 0 asks the OS to choose a free port
    #[arg(long, default_value = "127.0.0.1:0")]
    pub(crate) bind: SocketAddr,
    /// SQLite path; blobs use local.blob_dir or a path derived from this path
    #[arg(long)]
    pub(crate) data: PathBuf,
    /// Confirm an authenticated public bind without built-in TLS
    #[arg(long)]
    pub(crate) unsafe_public_bind: bool,
}

#[derive(Args)]
pub(crate) struct SyncArgs {
    #[command(subcommand)]
    pub(crate) command: Option<SyncSubcommand>,
    /// Override the configured sync server URL
    #[arg(long)]
    pub(crate) server: Option<String>,
    /// Emit the versioned sync result as JSON
    #[arg(long)]
    pub(crate) json: bool,
}

#[derive(Subcommand)]
pub(crate) enum SyncSubcommand {
    /// Produce a pairing invitation for Aven iOS onboarding
    #[command(after_long_help = PAIR_HELP)]
    Pair(PairArgs),
    /// Report sync configuration, health, progress, and pending work
    Status(StatusArgs),
}

#[derive(Args)]
pub(crate) struct PairArgs {
    /// Use a phone-reachable server URL for this invitation
    #[arg(long)]
    pub(crate) server: Option<String>,
    /// Copy the invitation to the local clipboard instead of displaying a QR code
    #[arg(long)]
    pub(crate) copy: bool,
}

#[derive(Args)]
pub(crate) struct StatusArgs {
    /// Emit the versioned status report as JSON
    #[arg(long)]
    pub(crate) json: bool,
}
