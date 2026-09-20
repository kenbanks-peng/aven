use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, NaiveTime};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::choices::{TaskPriority, TaskSource, TaskStatus};
use crate::recurrence::{
    RecurrenceDuePolicy, RecurrenceFrequency, RecurrenceOutcome, RecurrenceProjectionState,
    RecurrenceRule, RecurrenceSeriesState, TimeZoneId, WeekdaySet,
};
use crate::types::{
    RecurrenceOccurrence, RecurrencePauseInterval, RecurrenceSeries, RecurrenceSeriesLabel, Task,
};

pub(super) fn optional_task_date(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

pub(crate) fn task_from_row(row: &SqliteRow) -> Result<Task> {
    Ok(Task {
        id: row.try_get("id")?,
        workspace_id: row.try_get("workspace_id")?,
        title: row.try_get("title")?,
        description: row.try_get("description")?,
        project_id: row.try_get("project_id")?,
        project_key: row.try_get("project_key")?,
        project_prefix: row.try_get("project_prefix")?,
        status: TaskStatus::parse(row.try_get::<String, _>("status")?.as_str())?,
        priority: TaskPriority::parse(row.try_get::<String, _>("priority")?.as_str())?,
        source: TaskSource::parse(row.try_get::<String, _>("source")?.as_str())?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        queue_activity_at: row.try_get("queue_activity_at")?,
        available_at: optional_task_date(row.try_get("available_at")?),
        due_on: optional_task_date(row.try_get("due_on")?),
        deleted: row.try_get::<i64, _>("deleted")? != 0,
        is_epic: row.try_get::<i64, _>("is_epic")? != 0,
    })
}

pub(crate) fn recurrence_series_from_row(row: &SqliteRow) -> Result<RecurrenceSeries> {
    let frequency = RecurrenceFrequency::parse(&row.try_get::<String, _>("frequency")?)?;
    let interval = u32::try_from(row.try_get::<i64, _>("interval")?)
        .context("recurrence interval must fit u32")?;
    let weekdays = row
        .try_get::<String, _>("weekdays")?
        .parse::<WeekdaySet>()
        .map_err(anyhow::Error::msg)?;
    let rule = RecurrenceRule::new(frequency, interval, weekdays)?;
    let start_on = row
        .try_get::<String, _>("start_on")?
        .parse::<NaiveDate>()
        .context("invalid recurrence start date")?;
    let available_local_time = optional_text(row.try_get("available_local_time")?)
        .map(|value| {
            value
                .parse::<NaiveTime>()
                .context("invalid recurrence availability time")
        })
        .transpose()?;
    let initial_status = TaskStatus::parse(&row.try_get::<String, _>("initial_status")?)?;
    if !initial_status.is_open() {
        bail!("recurrence initial status must be open");
    }
    let state = RecurrenceSeriesState::parse(&row.try_get::<String, _>("state")?)?;
    let stopped_at = optional_text(row.try_get("stopped_at")?);
    if matches!(state, RecurrenceSeriesState::Stopped) != stopped_at.is_some() {
        bail!("recurrence stopped state and stop time must agree");
    }
    let deleted = row.try_get::<i64, _>("deleted")?;
    if !matches!(deleted, 0 | 1) {
        bail!("recurrence deleted value must be zero or one");
    }
    Ok(RecurrenceSeries {
        workspace_id: row.try_get("workspace_id")?,
        id: row.try_get::<String, _>("id")?.parse()?,
        title: row.try_get("title")?,
        description: row.try_get("description")?,
        project_id: row.try_get("project_id")?,
        priority: TaskPriority::parse(&row.try_get::<String, _>("priority")?)?,
        initial_status,
        rule,
        timezone: row
            .try_get::<String, _>("timezone")?
            .parse::<TimeZoneId>()?,
        start_on,
        available_local_time,
        due_policy: RecurrenceDuePolicy::parse(&row.try_get::<String, _>("due_policy")?)?,
        state,
        stopped_at,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        deleted: deleted != 0,
    })
}

pub(crate) fn recurrence_series_label_from_row(row: &SqliteRow) -> Result<RecurrenceSeriesLabel> {
    Ok(RecurrenceSeriesLabel {
        workspace_id: row.try_get("workspace_id")?,
        series_id: row.try_get::<String, _>("series_id")?.parse()?,
        label: row.try_get("label")?,
    })
}

pub(crate) fn recurrence_occurrence_from_row(row: &SqliteRow) -> Result<RecurrenceOccurrence> {
    let task_id = optional_text(row.try_get("task_id")?)
        .map(|value| value.parse())
        .transpose()?;
    let outcome = optional_text(row.try_get("outcome")?)
        .map(|value| RecurrenceOutcome::parse(&value))
        .transpose()?;
    let resolved_at = optional_text(row.try_get("resolved_at")?);
    let outcome_change_id = optional_text(row.try_get("outcome_change_id")?);
    let projection_state =
        RecurrenceProjectionState::parse(&row.try_get::<String, _>("projection_state")?)?;
    let archived_at = optional_text(row.try_get("archived_at")?);
    let valid_shape = match projection_state {
        RecurrenceProjectionState::Projected => {
            task_id.is_some()
                && outcome.is_none()
                && resolved_at.is_none()
                && outcome_change_id.is_none()
                && archived_at.is_none()
        }
        RecurrenceProjectionState::Resolved => {
            task_id.is_some()
                && outcome.is_some()
                && resolved_at.is_some()
                && outcome_change_id.is_some()
                && archived_at.is_none()
        }
        RecurrenceProjectionState::Archived => {
            task_id.is_some()
                && outcome.is_none()
                && resolved_at.is_none()
                && outcome_change_id.is_none()
                && archived_at.is_some()
        }
    };
    if !valid_shape {
        bail!("recurrence occurrence fields do not match projection state");
    }
    Ok(RecurrenceOccurrence {
        workspace_id: row.try_get("workspace_id")?,
        series_id: row.try_get::<String, _>("series_id")?.parse()?,
        slot_on: row
            .try_get::<String, _>("slot_on")?
            .parse::<NaiveDate>()
            .context("invalid recurrence slot date")?,
        task_id,
        outcome,
        resolved_at,
        outcome_change_id,
        projection_state,
        archived_at,
    })
}

pub(crate) fn recurrence_pause_interval_from_row(
    row: &SqliteRow,
) -> Result<RecurrencePauseInterval> {
    let suspended_slot_on = optional_text(row.try_get("suspended_slot_on")?)
        .map(|value| {
            value
                .parse::<NaiveDate>()
                .context("invalid suspended recurrence slot date")
        })
        .transpose()?;
    let suspended_task_id = optional_text(row.try_get("suspended_task_id")?)
        .map(|value| value.parse())
        .transpose()?;
    let resumed_at = optional_text(row.try_get("resumed_at")?);
    let resolved_by_change_id = optional_text(row.try_get("resolved_by_change_id")?);
    if resumed_at.is_some() != resolved_by_change_id.is_some() {
        bail!("recurrence pause resume time and change must agree");
    }
    if suspended_slot_on.is_some() != suspended_task_id.is_some() {
        bail!("recurrence suspended slot and task must agree");
    }
    Ok(RecurrencePauseInterval {
        workspace_id: row.try_get("workspace_id")?,
        id: row.try_get("id")?,
        series_id: row.try_get::<String, _>("series_id")?.parse()?,
        paused_at: row.try_get("paused_at")?,
        resumed_at,
        suspended_slot_on,
        suspended_task_id,
        created_by_change_id: row.try_get("created_by_change_id")?,
        resolved_by_change_id,
    })
}

fn optional_text(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}
