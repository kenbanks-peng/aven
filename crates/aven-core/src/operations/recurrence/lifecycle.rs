use anyhow::{Context, Result, ensure};
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::{Row, SqliteConnection};

use crate::change_log::{ChangeEntity, ChangePayload, append_change, op_type};
use crate::db::{begin_immediate, entity_field_version, insert_change, set_entity_field_version};
use crate::error::CoreError;
use crate::ids::{TaskId, new_id};
use crate::recurrence::{
    RecurrenceOutcome, RecurrenceSeriesId, RecurrenceSeriesState, next_slot_after,
    projection_slot_at, slot_cutoff, slot_values,
};
use crate::types::MutableEntityType;
use crate::workspaces::Workspace;

use super::{
    RecurrenceStateOutcome, ensure_no_series_conflict, format_utc, load_projected_occurrence,
    load_series, load_series_labels, materialize_occurrence,
    reconcile_recurrence_series_in_transaction, resolve_recurrence_occurrence_in_transaction,
};

pub async fn pause_recurrence_series(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    series_id: &RecurrenceSeriesId,
    paused_at: &str,
) -> Result<RecurrenceStateOutcome> {
    let paused_at_utc = DateTime::parse_from_rfc3339(paused_at)
        .context("invalid recurrence pause time")?
        .with_timezone(&Utc);
    let mut tx = begin_immediate(conn).await?;
    let series = load_series(&mut tx, &workspace.id, series_id).await?;
    ensure!(
        matches!(series.state, RecurrenceSeriesState::Active),
        CoreError::validation(format!(
            "error recurrence-pause-invalid-state state={}",
            series.state.as_str()
        ))
    );
    ensure_no_series_conflict(&mut tx, &workspace.id, series_id, "state").await?;
    reconcile_recurrence_series_in_transaction(&mut tx, workspace, series_id, paused_at_utc)
        .await?;
    let projected = load_projected_occurrence(&mut tx, &workspace.id, series_id).await?;
    let change_id = set_series_state(
        &mut tx,
        workspace,
        series_id,
        RecurrenceSeriesState::Paused,
        None,
        paused_at,
        op_type::SET_RECURRENCE_STATE,
    )
    .await?;
    let interval_id = new_id();
    sqlx::query(
        "INSERT INTO recurrence_pause_intervals(
            workspace_id, id, series_id, paused_at, resumed_at, suspended_slot_on,
            suspended_task_id, created_by_change_id, resolved_by_change_id
         ) VALUES (?, ?, ?, ?, '', ?, ?, ?, '')",
    )
    .bind(&workspace.id)
    .bind(&interval_id)
    .bind(series_id)
    .bind(paused_at)
    .bind(
        projected
            .as_ref()
            .map(|occurrence| occurrence.slot_on.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
    )
    .bind(
        projected
            .as_ref()
            .and_then(|occurrence| occurrence.task_id.as_ref())
            .map(TaskId::as_str)
            .unwrap_or(""),
    )
    .bind(&change_id)
    .execute(&mut *tx)
    .await?;
    append_change(
        &mut tx,
        ChangeEntity::RecurrenceSeries,
        series_id.as_str(),
        Some("pause"),
        op_type::OPEN_RECURRENCE_PAUSE,
        ChangePayload::workspace(workspace)
            .set("interval_id", &interval_id)
            .set("paused_at", paused_at)
            .set(
                "suspended_slot_on",
                projected
                    .as_ref()
                    .map(|occurrence| occurrence.slot_on.format("%Y-%m-%d").to_string())
                    .unwrap_or_default(),
            )
            .set(
                "suspended_task_id",
                projected
                    .as_ref()
                    .and_then(|occurrence| occurrence.task_id.as_ref())
                    .map(TaskId::as_str)
                    .unwrap_or(""),
            ),
    )
    .await?;
    let series = load_series(&mut tx, &workspace.id, series_id).await?;
    tx.commit().await?;
    Ok(RecurrenceStateOutcome {
        series,
        occurrence: projected,
    })
}

pub async fn resume_recurrence_series(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    series_id: &RecurrenceSeriesId,
    at: DateTime<Utc>,
) -> Result<RecurrenceStateOutcome> {
    let mut resumed_at = format_utc(at);
    let mut tx = begin_immediate(conn).await?;
    let series = load_series(&mut tx, &workspace.id, series_id).await?;
    ensure!(
        matches!(series.state, RecurrenceSeriesState::Paused),
        CoreError::validation(format!(
            "error recurrence-resume-invalid-state state={}",
            series.state.as_str()
        ))
    );
    ensure_no_series_conflict(&mut tx, &workspace.id, series_id, "state").await?;
    let interval = sqlx::query(
        "SELECT id, paused_at, suspended_slot_on, suspended_task_id
         FROM recurrence_pause_intervals
         WHERE workspace_id = ? AND series_id = ? AND resumed_at = ''",
    )
    .bind(&workspace.id)
    .bind(series_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| CoreError::validation("error recurrence-open-pause-missing"))?;
    let interval_id: String = interval.get("id");
    let paused_at: String = interval.get("paused_at");
    resumed_at = timestamp_strictly_after(&paused_at, &resumed_at)?;
    let suspended_slot_text: String = interval.get("suspended_slot_on");
    let suspended_slot = (!suspended_slot_text.is_empty())
        .then(|| suspended_slot_text.parse::<NaiveDate>())
        .transpose()?;

    let close_change_id = append_change(
        &mut tx,
        ChangeEntity::RecurrenceSeries,
        series_id.as_str(),
        Some("pause"),
        op_type::CLOSE_RECURRENCE_PAUSE,
        ChangePayload::workspace(workspace)
            .set("interval_id", &interval_id)
            .set("paused_at", &paused_at)
            .set("resumed_at", &resumed_at),
    )
    .await?;
    sqlx::query(
        "UPDATE recurrence_pause_intervals SET resumed_at = ?, resolved_by_change_id = ?
         WHERE workspace_id = ? AND id = ? AND resumed_at = ''",
    )
    .bind(&resumed_at)
    .bind(&close_change_id)
    .bind(&workspace.id)
    .bind(&interval_id)
    .execute(&mut *tx)
    .await?;
    set_series_state(
        &mut tx,
        workspace,
        series_id,
        RecurrenceSeriesState::Active,
        None,
        &resumed_at,
        op_type::SET_RECURRENCE_STATE,
    )
    .await?;

    let schedule = series.schedule();
    let mut occurrence = load_projected_occurrence(&mut tx, &workspace.id, series_id).await?;
    let suspended_still_live = if let Some(suspended_slot) = suspended_slot {
        occurrence
            .as_ref()
            .is_some_and(|value| value.slot_on == suspended_slot)
            && at < slot_cutoff(&schedule, suspended_slot)?
    } else {
        false
    };
    if occurrence.is_some() && !suspended_still_live {
        let slot = occurrence.as_ref().expect("checked occurrence").slot_on;
        sqlx::query(
            "UPDATE recurrence_occurrences
             SET projection_state = 'archived', archived_at = ?
             WHERE workspace_id = ? AND series_id = ? AND slot_on = ?
             AND projection_state = 'projected'",
        )
        .bind(&resumed_at)
        .bind(&workspace.id)
        .bind(series_id)
        .bind(slot.format("%Y-%m-%d").to_string())
        .execute(&mut *tx)
        .await?;
        occurrence = None;
    }
    if occurrence.is_none() {
        let mut target = projection_slot_at(&schedule, at)?;
        let boundary = slot_values(&schedule, target)?.boundary_at;
        if boundary >= paused_at && boundary < resumed_at {
            target = next_slot_after(&series.rule, series.start_on, target)
                .context("recurrence schedule has no representable resumed slot")?;
        }
        let labels = load_series_labels(&mut tx, &workspace.id, series_id).await?;
        occurrence =
            Some(materialize_occurrence(&mut tx, workspace, &series, &labels, target).await?);
    }
    let series = load_series(&mut tx, &workspace.id, series_id).await?;
    tx.commit().await?;
    Ok(RecurrenceStateOutcome { series, occurrence })
}

pub async fn stop_recurrence_series(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    series_id: &RecurrenceSeriesId,
    skip_current: bool,
    stopped_at: &str,
) -> Result<RecurrenceStateOutcome> {
    let mut tx = begin_immediate(conn).await?;
    let outcome = stop_recurrence_series_in_transaction(
        &mut tx,
        workspace,
        series_id,
        skip_current,
        stopped_at,
        true,
    )
    .await?;
    tx.commit().await?;
    Ok(outcome)
}

pub(crate) async fn stop_recurrence_series_for_project_delete_in_transaction(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    series_id: &RecurrenceSeriesId,
    stopped_at: &str,
) -> Result<RecurrenceStateOutcome> {
    stop_recurrence_series_in_transaction(conn, workspace, series_id, false, stopped_at, false)
        .await
}

async fn stop_recurrence_series_in_transaction(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    series_id: &RecurrenceSeriesId,
    skip_current: bool,
    stopped_at: &str,
    reconcile: bool,
) -> Result<RecurrenceStateOutcome> {
    let mut stopped_at = stopped_at.to_string();
    let series = load_series(conn, &workspace.id, series_id).await?;
    ensure!(
        !matches!(series.state, RecurrenceSeriesState::Stopped),
        CoreError::validation("error recurrence-already-stopped")
    );
    ensure_no_series_conflict(conn, &workspace.id, series_id, "state").await?;
    let open_pause = if matches!(series.state, RecurrenceSeriesState::Paused) {
        let interval = sqlx::query(
            "SELECT id, paused_at FROM recurrence_pause_intervals
             WHERE workspace_id = ? AND series_id = ? AND resumed_at = ''",
        )
        .bind(&workspace.id)
        .bind(series_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| CoreError::validation("error recurrence-open-pause-missing"))?;
        let interval_id: String = interval.get("id");
        let paused_at: String = interval.get("paused_at");
        stopped_at = timestamp_strictly_after(&paused_at, &stopped_at)?;
        Some((interval_id, paused_at))
    } else {
        None
    };
    let stopped_at_utc = DateTime::parse_from_rfc3339(&stopped_at)
        .context("invalid recurrence stop time")?
        .with_timezone(&Utc);
    if reconcile {
        reconcile_recurrence_series_in_transaction(conn, workspace, series_id, stopped_at_utc)
            .await?;
    }
    if let Some((interval_id, paused_at)) = open_pause {
        let close_change_id = append_change(
            conn,
            ChangeEntity::RecurrenceSeries,
            series_id.as_str(),
            Some("pause"),
            op_type::CLOSE_RECURRENCE_PAUSE,
            ChangePayload::workspace(workspace)
                .set("interval_id", &interval_id)
                .set("paused_at", &paused_at)
                .set("resumed_at", &stopped_at),
        )
        .await?;
        sqlx::query(
            "UPDATE recurrence_pause_intervals SET resumed_at = ?, resolved_by_change_id = ?
             WHERE workspace_id = ? AND id = ? AND resumed_at = ''",
        )
        .bind(&stopped_at)
        .bind(&close_change_id)
        .bind(&workspace.id)
        .bind(&interval_id)
        .execute(&mut *conn)
        .await?;
    }
    set_series_state(
        conn,
        workspace,
        series_id,
        RecurrenceSeriesState::Stopped,
        Some(&stopped_at),
        &stopped_at,
        op_type::STOP_RECURRENCE_SERIES,
    )
    .await?;
    let projected = load_projected_occurrence(conn, &workspace.id, series_id).await?;
    let occurrence = if skip_current {
        let projected = projected
            .ok_or_else(|| CoreError::validation("error recurrence-current-occurrence-missing"))?;
        let task_id = projected
            .task_id
            .as_ref()
            .expect("projected occurrence has a task");
        Some(
            resolve_recurrence_occurrence_in_transaction(
                conn,
                workspace,
                task_id,
                RecurrenceOutcome::Skipped,
                &stopped_at,
            )
            .await?
            .resolved,
        )
    } else {
        projected
    };
    let series = load_series(conn, &workspace.id, series_id).await?;
    Ok(RecurrenceStateOutcome { series, occurrence })
}
async fn set_series_state(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    series_id: &RecurrenceSeriesId,
    state: RecurrenceSeriesState,
    stopped_at: Option<&str>,
    changed_at: &str,
    operation: &'static str,
) -> Result<String> {
    let base = entity_field_version(
        conn,
        &workspace.id,
        MutableEntityType::RecurrenceSeries,
        series_id.as_str(),
        "state",
    )
    .await?;
    sqlx::query(
        "UPDATE recurrence_series SET state = ?, stopped_at = ?, updated_at = ?
         WHERE workspace_id = ? AND id = ?",
    )
    .bind(state.as_str())
    .bind(stopped_at.unwrap_or(""))
    .bind(changed_at)
    .bind(&workspace.id)
    .bind(series_id)
    .execute(&mut *conn)
    .await?;
    let change_id = insert_change(
        conn,
        "recurrence_series",
        series_id.as_str(),
        Some("state"),
        operation,
        ChangePayload::workspace(workspace)
            .set("state", state.as_str())
            .set("stopped_at", stopped_at.unwrap_or(""))
            .set("changed_at", changed_at)
            .into_value(),
        base.as_deref(),
    )
    .await?;
    set_entity_field_version(
        conn,
        &workspace.id,
        MutableEntityType::RecurrenceSeries,
        series_id.as_str(),
        "state",
        &change_id,
    )
    .await?;
    if matches!(state, RecurrenceSeriesState::Stopped) {
        set_entity_field_version(
            conn,
            &workspace.id,
            MutableEntityType::RecurrenceSeries,
            series_id.as_str(),
            "stopped_at",
            &change_id,
        )
        .await?;
    }
    Ok(change_id)
}
pub(crate) fn timestamp_strictly_after(earlier: &str, candidate: &str) -> Result<String> {
    if candidate > earlier {
        return Ok(candidate.to_string());
    }
    let earlier = DateTime::parse_from_rfc3339(earlier)?.with_timezone(&Utc);
    let adjusted = earlier
        .checked_add_signed(chrono::Duration::seconds(1))
        .context("recurrence lifecycle timestamp is out of range")?;
    Ok(format_utc(adjusted))
}
