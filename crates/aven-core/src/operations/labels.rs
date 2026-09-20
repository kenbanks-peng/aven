use crate::ids::WorkspaceId;

use anyhow::{Result, bail};
use sqlx::SqliteConnection;
use tracing::info;

use crate::change_log::{ChangeEntity, ChangePayload, append_change, op_type};
use crate::db::{Database, begin_immediate};
use crate::ids::now;
use crate::labels::normalize_label;
use crate::undo::{UndoCommand, UndoPayload, record_tui_undo};
use crate::workspaces::Workspace;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelDeleteOutcome {
    pub name: String,
    pub changed: bool,
    pub task_count: usize,
    pub series_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelRenameOutcome {
    pub previous_name: String,
    pub name: String,
    pub changed: bool,
    pub task_count: usize,
    pub series_count: usize,
}

pub struct LabelOutcome {
    pub name: String,
    pub created: bool,
    pub change_id: Option<String>,
}

impl Database {
    pub async fn create_label(&self, workspace: &Workspace, name: &str) -> Result<LabelOutcome> {
        let mut conn = self.acquire_writer().await?;
        let mut tx = begin_immediate(&mut conn).await?;
        let outcome = match create_label_operation(&mut tx, workspace, name).await {
            Ok(outcome) => outcome,
            Err(error) => {
                tx.rollback().await?;
                return Err(error);
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    pub async fn create_label_with_tui_undo(
        &self,
        workspace: &Workspace,
        name: &str,
    ) -> Result<LabelOutcome> {
        let mut conn = self.acquire_writer().await?;
        let mut tx = begin_immediate(&mut conn).await?;
        let outcome = create_label_operation(&mut tx, workspace, name).await?;
        if outcome.created {
            record_tui_undo(
                &mut tx,
                &workspace.id,
                &format!("label {}", outcome.name),
                UndoPayload {
                    commands: vec![UndoCommand::DeleteCreatedLabel {
                        label: outcome.name.clone(),
                        create_change_id: outcome.change_id.clone().unwrap_or_default(),
                    }],
                },
            )
            .await?;
        }
        tx.commit().await?;
        Ok(outcome)
    }

    pub async fn delete_label(
        &self,
        workspace: &Workspace,
        name: &str,
    ) -> Result<LabelDeleteOutcome> {
        let mut conn = self.acquire_writer().await?;
        delete_label_operation(&mut conn, workspace, name, false).await
    }

    pub async fn delete_label_with_tui_undo(
        &self,
        workspace: &Workspace,
        name: &str,
    ) -> Result<LabelDeleteOutcome> {
        let mut conn = self.acquire_writer().await?;
        delete_label_operation(&mut conn, workspace, name, true).await
    }

    pub async fn rename_label_with_tui_undo(
        &self,
        workspace: &Workspace,
        name: &str,
        new_name: &str,
    ) -> Result<LabelRenameOutcome> {
        let mut conn = self.acquire_writer().await?;
        rename_label_operation(&mut conn, workspace, name, new_name, true).await
    }
}

pub(crate) async fn create_label_operation(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    name: &str,
) -> Result<LabelOutcome> {
    let name = normalize_label(name);
    if name.is_empty() {
        bail!("error invalid-label");
    }
    let existed = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM labels WHERE workspace_id = ? AND name = ?",
    )
    .bind(&workspace.id)
    .bind(&name)
    .fetch_one(&mut *conn)
    .await?
        > 0;
    let created_at = now();
    sqlx::query("INSERT OR IGNORE INTO labels(workspace_id, name, created_at) VALUES (?, ?, ?)")
        .bind(&workspace.id)
        .bind(&name)
        .bind(&created_at)
        .execute(&mut *conn)
        .await?;
    let created = !existed;
    let change_id = if created {
        Some(
            append_change(
                conn,
                ChangeEntity::Label,
                &name,
                None,
                op_type::CREATE_LABEL,
                ChangePayload::workspace(workspace)
                    .set("name", name.clone())
                    .set("created_at", created_at),
            )
            .await?,
        )
    } else {
        None
    };
    if created {
        info!("label created");
    }
    Ok(LabelOutcome {
        name,
        created,
        change_id,
    })
}

pub async fn rename_label_operation(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    name: &str,
    new_name: &str,
    tui_undo: bool,
) -> Result<LabelRenameOutcome> {
    let name = normalize_label(name);
    let new_name = normalize_label(new_name);
    if name.is_empty() || new_name.is_empty() {
        bail!("error invalid-label");
    }
    let mut tx = begin_immediate(conn).await?;
    let snapshot = label_snapshot(&mut tx, &workspace.id, &name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("error label-not-found label={name}"))?;
    if name == new_name {
        tx.commit().await?;
        return Ok(LabelRenameOutcome {
            previous_name: name.clone(),
            name,
            changed: false,
            task_count: snapshot.task_ids.len(),
            series_count: snapshot.series_ids.len(),
        });
    }
    if label_snapshot(&mut tx, &workspace.id, &new_name)
        .await?
        .is_some()
    {
        bail!("error label-exists label={new_name}");
    }
    set_label_name(&mut tx, workspace, &name, &new_name).await?;
    if tui_undo {
        record_tui_undo(
            &mut tx,
            &workspace.id,
            &format!("label {new_name}"),
            UndoPayload {
                commands: vec![UndoCommand::SetLabelName {
                    before: name.clone(),
                    after: new_name.clone(),
                }],
            },
        )
        .await?;
    }
    tx.commit().await?;
    info!("label renamed");
    Ok(LabelRenameOutcome {
        previous_name: name,
        name: new_name,
        changed: true,
        task_count: snapshot.task_ids.len(),
        series_count: snapshot.series_ids.len(),
    })
}

pub async fn delete_label_operation(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    name: &str,
    tui_undo: bool,
) -> Result<LabelDeleteOutcome> {
    let name = normalize_label(name);
    if name.is_empty() {
        bail!("error invalid-label");
    }
    let mut tx = begin_immediate(conn).await?;
    let Some(snapshot) = label_snapshot(&mut tx, &workspace.id, &name).await? else {
        tx.commit().await?;
        return Ok(LabelDeleteOutcome {
            name,
            changed: false,
            task_count: 0,
            series_count: 0,
        });
    };
    sqlx::query("DELETE FROM task_labels WHERE workspace_id = ? AND label = ?")
        .bind(&workspace.id)
        .bind(&name)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM recurrence_series_labels WHERE workspace_id = ? AND label = ?")
        .bind(&workspace.id)
        .bind(&name)
        .execute(&mut *tx)
        .await?;
    let labels = sqlx::query("DELETE FROM labels WHERE workspace_id = ? AND name = ?")
        .bind(&workspace.id)
        .bind(&name)
        .execute(&mut *tx)
        .await?;
    if labels.rows_affected() != 1 {
        bail!("error label-delete-race label={name}");
    }
    append_change(
        &mut tx,
        ChangeEntity::Label,
        &name,
        None,
        op_type::LABEL_DELETE,
        ChangePayload::workspace(workspace)
            .set("name", name.clone())
            .set("deleted_at", now()),
    )
    .await?;
    if tui_undo {
        record_tui_undo(
            &mut tx,
            &workspace.id,
            &format!("label {name}"),
            UndoPayload {
                commands: vec![UndoCommand::RestoreDeletedLabel {
                    name: name.clone(),
                    created_at: snapshot.created_at.clone(),
                    task_ids: snapshot.task_ids.clone(),
                    series_ids: snapshot.series_ids.clone(),
                }],
            },
        )
        .await?;
    }
    tx.commit().await?;
    info!("label deleted");
    Ok(LabelDeleteOutcome {
        name,
        changed: true,
        task_count: snapshot.task_ids.len(),
        series_count: snapshot.series_ids.len(),
    })
}

#[derive(Debug)]
struct LabelSnapshot {
    created_at: String,
    task_ids: Vec<crate::ids::TaskId>,
    series_ids: Vec<crate::recurrence::RecurrenceSeriesId>,
}

async fn label_snapshot(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    name: &str,
) -> Result<Option<LabelSnapshot>> {
    let Some(created_at) = sqlx::query_scalar::<_, String>(
        "SELECT created_at FROM labels WHERE workspace_id = ? AND name = ?",
    )
    .bind(workspace_id)
    .bind(name)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    let task_ids = sqlx::query_scalar(
        "SELECT task_id FROM task_labels WHERE workspace_id = ? AND label = ? ORDER BY task_id",
    )
    .bind(workspace_id)
    .bind(name)
    .fetch_all(&mut *conn)
    .await?;
    let series_ids = sqlx::query_scalar(
        "SELECT series_id FROM recurrence_series_labels WHERE workspace_id = ? AND label = ? ORDER BY series_id",
    )
    .bind(workspace_id)
    .bind(name)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Some(LabelSnapshot {
        created_at,
        task_ids,
        series_ids,
    }))
}

pub(crate) async fn set_label_name(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    name: &str,
    new_name: &str,
) -> Result<()> {
    sqlx::query("UPDATE labels SET name = ? WHERE workspace_id = ? AND name = ?")
        .bind(new_name)
        .bind(&workspace.id)
        .bind(name)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE task_labels SET label = ? WHERE workspace_id = ? AND label = ?")
        .bind(new_name)
        .bind(&workspace.id)
        .bind(name)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE recurrence_series_labels SET label = ? WHERE workspace_id = ? AND label = ?",
    )
    .bind(new_name)
    .bind(&workspace.id)
    .bind(name)
    .execute(&mut *conn)
    .await?;
    append_change(
        conn,
        ChangeEntity::Label,
        name,
        None,
        op_type::SET_LABEL_NAME,
        ChangePayload::workspace(workspace)
            .set("name", name)
            .set("new_name", new_name)
            .set("renamed_at", now()),
    )
    .await?;
    Ok(())
}
