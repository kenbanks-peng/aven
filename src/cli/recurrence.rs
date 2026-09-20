use std::path::PathBuf;

use clap::{Args, Subcommand};

pub(super) const RECUR_HELP: &str = r#"Recurrence commands accept the stable RCR-... series ref printed by `aven add
--repeat` or a linked occurrence task ref. Complete or edit the projected task
with its ordinary task ref. Series template edits affect future occurrences."#;

pub(super) const RECUR_EDIT_HELP: &str = r#"Series edits affect future occurrences. Existing occurrence tasks retain their
stored fields. Supply at most one description source: --description,
--description-file, or --description-stdin."#;

#[derive(Args)]
pub(crate) struct RecurCommand {
    #[command(subcommand)]
    pub(crate) command: RecurSubcommand,
}

#[derive(Subcommand)]
pub(crate) enum RecurSubcommand {
    /// List recurring series
    List(RecurListArgs),
    /// Show a recurring series
    Show(RecurShowArgs),
    /// Show recurring series history
    History(RecurHistoryArgs),
    /// Edit the template used by future occurrences
    #[command(after_long_help = RECUR_EDIT_HELP)]
    Edit(Box<RecurEditArgs>),
    /// Skip the current occurrence
    Skip(RecurRefArgs),
    /// Pause a recurring series
    Pause(RecurRefArgs),
    /// Resume a paused recurring series
    Resume(RecurRefArgs),
    /// Stop future scheduling
    Stop(RecurStopArgs),
}

#[derive(Args)]
pub(crate) struct RecurListArgs {
    #[arg(long, help = "Print machine-readable JSON")]
    pub(crate) json: bool,
}

#[derive(Args)]
pub(crate) struct RecurShowArgs {
    /// Recurring-series ref or linked task ref; prefer a stable RCR-... ref
    pub(crate) series_ref: String,
    #[arg(long, help = "Print machine-readable JSON")]
    pub(crate) json: bool,
}

#[derive(Args)]
pub(crate) struct RecurHistoryArgs {
    /// Recurring-series ref or linked task ref; prefer a stable RCR-... ref
    pub(crate) series_ref: String,
    /// Skip this many newest history entries
    #[arg(long, default_value_t = 0)]
    pub(crate) offset: usize,
    #[arg(
        long,
        default_value_t = 100,
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..=500),
        help = "Maximum result count (1-500)"
    )]
    pub(crate) limit: usize,
    #[arg(long, help = "Print machine-readable JSON")]
    pub(crate) json: bool,
}

#[derive(Args)]
pub(crate) struct RecurEditArgs {
    /// Recurring-series ref or linked task ref; prefer a stable RCR-... ref
    pub(crate) series_ref: String,
    /// Set the title for future occurrences
    #[arg(long)]
    pub(crate) title: Option<String>,
    /// Set the future-occurrence description from this argument
    #[arg(long)]
    pub(crate) description: Option<String>,
    /// Read the future-occurrence description from a UTF-8 file
    #[arg(long)]
    pub(crate) description_file: Option<PathBuf>,
    /// Read the future-occurrence description from standard input
    #[arg(long)]
    pub(crate) description_stdin: bool,
    /// Assign future occurrences to a project by key or name
    #[arg(long)]
    pub(crate) project: Option<String>,
    /// Set future status: inbox, backlog, todo, or active
    #[arg(long)]
    pub(crate) status: Option<String>,
    /// Set future priority: none, low, medium, high, or urgent
    #[arg(long)]
    pub(crate) priority: Option<String>,
    /// Replace the future-occurrence label set; repeat for multiple labels
    #[arg(long, value_name = "LABEL")]
    pub(crate) label: Vec<String>,
    /// Set future-occurrence metadata; repeat for multiple fields
    #[arg(long, value_name = "KEY=VALUE")]
    pub(crate) metadata: Vec<String>,
    /// Remove future-occurrence metadata by key; repeat for multiple fields
    #[arg(long, value_name = "KEY")]
    pub(crate) remove_metadata: Vec<String>,
    /// Set the local availability time, or none for start-of-day availability
    #[arg(long, value_name = "HH:MM|none")]
    pub(crate) repeat_at: Option<String>,
    /// Give future occurrences a same-day deadline, or no deadline
    #[arg(long, value_name = "same-day|none")]
    pub(crate) repeat_due: Option<String>,
}

#[derive(Args)]
pub(crate) struct RecurRefArgs {
    /// Recurring-series ref or linked task ref; prefer a stable RCR-... ref
    pub(crate) series_ref: String,
}

#[derive(Args)]
pub(crate) struct RecurStopArgs {
    /// Recurring-series ref or linked task ref; prefer a stable RCR-... ref
    pub(crate) series_ref: String,
    /// Mark the current occurrence skipped while stopping the series
    #[arg(long)]
    pub(crate) skip_current: bool,
}
