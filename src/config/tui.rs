use std::collections::BTreeSet;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::custom_commands::CustomTuiCommandConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    #[serde(default)]
    pub table: TaskTableConfig,
    #[serde(default)]
    pub sidebar: SidebarConfig,
    #[serde(default = "default_task_columns")]
    pub columns: Vec<TaskColumnConfig>,
    #[serde(default)]
    pub commands: Vec<CustomTuiCommandConfig>,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            table: TaskTableConfig::default(),
            sidebar: SidebarConfig::default(),
            columns: default_task_columns(),
            commands: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskTableConfig {
    #[serde(default = "default_table_columns")]
    pub columns: Vec<TableColumn>,
    #[serde(default)]
    pub compact_status: bool,
}

impl Default for TaskTableConfig {
    fn default() -> Self {
        Self {
            columns: default_table_columns(),
            compact_status: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableColumn {
    Ref,
    Title,
    Labels,
    Metadata,
    Project,
    Status,
    Priority,
    Time,
    Due,
}

impl TableColumn {
    /// Every column, in semantic index order. Indexes geometry and cell arrays.
    pub const ALL: [Self; 9] = [
        Self::Ref,
        Self::Title,
        Self::Labels,
        Self::Metadata,
        Self::Project,
        Self::Status,
        Self::Priority,
        Self::Time,
        Self::Due,
    ];

    /// Columns shown when `tui.table.columns` is omitted, in display order.
    pub const DEFAULT: [Self; 8] = [
        Self::Ref,
        Self::Title,
        Self::Labels,
        Self::Metadata,
        Self::Project,
        Self::Status,
        Self::Priority,
        Self::Time,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Ref => "ref",
            Self::Title => "title",
            Self::Labels => "labels",
            Self::Metadata => "metadata",
            Self::Project => "project",
            Self::Status => "status",
            Self::Priority => "priority",
            Self::Time => "time",
            Self::Due => "due",
        }
    }
}

pub(super) fn default_table_columns() -> Vec<TableColumn> {
    TableColumn::DEFAULT.to_vec()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarConfig {
    #[serde(default = "default_sidebar_views")]
    pub views: Vec<SidebarView>,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            views: default_sidebar_views(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarView {
    Queue,
    Ready,
    Blocked,
    Overdue,
    All,
    Open,
    Inbox,
    Active,
    Backlog,
    Todo,
    Upcoming,
    Done,
    Conflicts,
    Epics,
    Recurring,
    RecentActions,
    Search,
}

fn default_sidebar_views() -> Vec<SidebarView> {
    vec![
        SidebarView::Queue,
        SidebarView::Ready,
        SidebarView::Blocked,
        SidebarView::Overdue,
        SidebarView::All,
        SidebarView::Open,
        SidebarView::Inbox,
        SidebarView::Active,
        SidebarView::Backlog,
        SidebarView::Todo,
        SidebarView::Upcoming,
        SidebarView::Done,
        SidebarView::Conflicts,
        SidebarView::Epics,
        SidebarView::Recurring,
        SidebarView::RecentActions,
        SidebarView::Search,
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskColumnConfig {
    pub name: String,
    pub statuses: Vec<String>,
}

impl TaskColumnConfig {
    fn new(name: &str, statuses: &[&str]) -> Self {
        Self {
            name: name.to_string(),
            statuses: statuses.iter().map(|status| status.to_string()).collect(),
        }
    }
}

pub(super) fn default_task_columns() -> Vec<TaskColumnConfig> {
    vec![
        TaskColumnConfig::new("Inbox", &["inbox"]),
        TaskColumnConfig::new("Backlog", &["backlog"]),
        TaskColumnConfig::new("Todo", &["todo"]),
        TaskColumnConfig::new("Active", &["active"]),
        TaskColumnConfig::new("Done", &["done", "canceled"]),
    ]
}

pub(super) fn validate(config: &TuiConfig) -> Result<()> {
    if config.table.columns.is_empty() {
        bail!("tui.table.columns must include at least one column");
    }
    let mut table_columns = BTreeSet::new();
    for column in &config.table.columns {
        if !table_columns.insert(*column) {
            bail!(
                "tui.table.columns contains duplicate column {}",
                column.name()
            );
        }
    }

    let mut sidebar_views = BTreeSet::new();
    for view in &config.sidebar.views {
        if !sidebar_views.insert(view) {
            bail!("tui.sidebar.views contains duplicate view {view:?}");
        }
    }

    if config.columns.is_empty() {
        bail!("column view requires at least one column");
    }
    let valid = crate::choices::STATUSES
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut assigned = BTreeSet::new();
    for column in &config.columns {
        if column.name.trim().is_empty() {
            bail!("column name must not be blank");
        }
        if column.statuses.is_empty() {
            bail!("column {} must include at least one status", column.name);
        }
        for status in &column.statuses {
            if !valid.contains(status.as_str()) {
                bail!("unknown column status {status}");
            }
            if !assigned.insert(status.as_str()) {
                bail!("duplicate column status {status}");
            }
        }
    }
    let missing = valid.difference(&assigned).copied().collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!("missing column statuses {}", missing.join(","));
    }

    Ok(())
}
