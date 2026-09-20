use clap::{Args, Subcommand};

#[derive(Args)]
pub(crate) struct DepCommand {
    #[command(subcommand)]
    pub(crate) command: DepSubcommand,
}

#[derive(Subcommand)]
pub(crate) enum DepSubcommand {
    /// Add a dependency to a task
    Add(DepAddArgs),
    /// Remove a dependency from a task
    Remove(DepRemoveArgs),
    /// List a task's dependencies
    List(DepListArgs),
}

#[derive(Args)]
pub(crate) struct DepAddArgs {
    /// Blocked task ref
    pub(crate) task_ref: String,
    /// Blocker task ref
    pub(crate) depends_on_ref: String,
}

#[derive(Args)]
pub(crate) struct DepRemoveArgs {
    /// Blocked task ref
    pub(crate) task_ref: String,
    /// Blocker task ref
    pub(crate) depends_on_ref: String,
}

#[derive(Args)]
pub(crate) struct DepListArgs {
    /// Task whose blockers and dependents to list
    pub(crate) task_ref: String,
    #[arg(long, help = "Print machine-readable JSON")]
    pub(crate) json: bool,
}

#[derive(Args)]
pub(crate) struct RelatedCommand {
    #[command(subcommand)]
    pub(crate) command: RelatedSubcommand,
}

#[derive(Subcommand)]
pub(crate) enum RelatedSubcommand {
    /// Link two related tasks
    Add(RelatedMutationArgs),
    /// Unlink two related tasks
    Remove(RelatedMutationArgs),
    /// List a task's related tasks
    List(RelatedListArgs),
}

#[derive(Args)]
pub(crate) struct RelatedMutationArgs {
    /// First task ref
    pub(crate) task_ref: String,
    /// Other task ref in the symmetric link
    pub(crate) related_ref: String,
}

#[derive(Args)]
pub(crate) struct RelatedListArgs {
    /// Task whose related links to list
    pub(crate) task_ref: String,
    #[arg(long, help = "Print machine-readable JSON")]
    pub(crate) json: bool,
}

#[derive(Args)]
pub(crate) struct EpicCommand {
    #[command(subcommand)]
    pub(crate) command: EpicSubcommand,
}

#[derive(Subcommand)]
pub(crate) enum EpicSubcommand {
    /// Add a task to an epic
    Add(EpicAddArgs),
    /// Remove a task from an epic
    Remove(EpicRemoveArgs),
    /// List an epic's child tasks
    List(EpicListArgs),
}

#[derive(Args)]
pub(crate) struct EpicAddArgs {
    /// Child task ref
    pub(crate) child_ref: String,
    /// Epic task ref
    pub(crate) epic_ref: String,
}

#[derive(Args)]
pub(crate) struct EpicRemoveArgs {
    /// Child task ref
    pub(crate) child_ref: String,
    /// Epic task ref
    pub(crate) epic_ref: String,
}

#[derive(Args)]
pub(crate) struct EpicListArgs {
    /// Epic task ref whose children to list
    pub(crate) epic_ref: String,
    #[arg(long, help = "Print machine-readable JSON")]
    pub(crate) json: bool,
}
