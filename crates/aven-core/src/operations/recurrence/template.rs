use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use sqlx::SqliteConnection;

use crate::change_log::{ChangeEntity, ChangePayload, append_change, op_type};
use crate::choices::{TaskPriority, TaskStatus};
use crate::db::{begin_immediate, entity_field_version, set_entity_field_version};
use crate::error::CoreError;
use crate::ids::{WorkspaceId, now};
use crate::labels::{resolve_labels_in_workspace, resolve_or_create_labels_in_workspace};
use crate::projects::resolve_or_create_project_in_workspace;
use crate::recurrence::RecurrenceSeriesId;
use crate::refs::get_task_in_workspace;
use crate::types::MutableEntityType;
use crate::workspaces::Workspace;

use super::{
    CreateRecurrenceSeriesParams, RecurrenceCreateOutcome, RecurrenceTemplateUpdateOutcome,
    SERIES_TEMPLATE_FIELDS, UpdateRecurrenceTemplateParams, ensure_no_series_conflict,
    format_local_time, format_utc, load_series, load_series_labels, materialize_occurrence,
    recurrence_series_ref, utc_now,
};

pub async fn create_recurrence_series(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    params: CreateRecurrenceSeriesParams,
) -> Result<RecurrenceCreateOutcome> {
    let CreateRecurrenceSeriesParams {
        draft,
        at,
        create_missing_labels,
    } = params;
    let at = at.map_or_else(utc_now, Ok)?;
    let priority = TaskPriority::parse(&draft.priority)?;
    let initial_status = TaskStatus::parse(&draft.initial_status)?;
    ensure!(
        initial_status.is_open(),
        CoreError::validation(format!(
            "error recurrence-initial-status-terminal status={}",
            initial_status.as_str()
        ))
    );
    let creation_date = at
        .with_timezone(&draft.schedule.timezone.timezone())
        .date_naive();
    let first_slot = draft
        .schedule
        .slots_on_or_after(draft.schedule.start_on.max(creation_date))
        .next()
        .context("recurrence schedule has no representable first slot")?;
    let series_id = RecurrenceSeriesId::new();
    let created_at = format_utc(at);

    let mut tx = begin_immediate(conn).await?;
    let project =
        resolve_or_create_project_in_workspace(&mut tx, &workspace.id, draft.project.as_str())
            .await?;
    let labels = if create_missing_labels {
        resolve_or_create_labels_in_workspace(&mut tx, workspace, &draft.labels)
            .await?
            .names
    } else {
        resolve_labels_in_workspace(&mut tx, &workspace.id, &draft.labels).await?
    };
    let series_metadata =
        crate::metadata::resolve_metadata_inputs(&mut tx, workspace, &draft.metadata).await?;
    sqlx::query(
        "INSERT INTO recurrence_series(
            workspace_id, id, title, description, project_id, priority, initial_status,
            frequency, interval, weekdays, timezone, start_on, available_local_time,
            due_policy, state, stopped_at, created_at, updated_at, deleted
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', '', ?, ?, 0)",
    )
    .bind(&workspace.id)
    .bind(&series_id)
    .bind(&draft.title)
    .bind(&draft.description)
    .bind(&project.id)
    .bind(priority.as_str())
    .bind(initial_status.as_str())
    .bind(draft.schedule.rule.frequency().as_str())
    .bind(i64::from(draft.schedule.rule.interval()))
    .bind(draft.schedule.rule.weekdays_set().to_string())
    .bind(draft.schedule.timezone.as_str())
    .bind(draft.schedule.start_on.format("%Y-%m-%d").to_string())
    .bind(format_local_time(draft.schedule.available_local_time))
    .bind(draft.schedule.due_policy.as_str())
    .bind(&created_at)
    .bind(&created_at)
    .execute(&mut *tx)
    .await?;
    for label in &labels {
        sqlx::query(
            "INSERT INTO recurrence_series_labels(workspace_id, series_id, label)
             VALUES (?, ?, ?)",
        )
        .bind(&workspace.id)
        .bind(&series_id)
        .bind(label)
        .execute(&mut *tx)
        .await?;
    }
    for value in &series_metadata {
        sqlx::query(
            "INSERT INTO recurrence_series_metadata(
                 workspace_id, series_id, field_id, value, created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&workspace.id)
        .bind(&series_id)
        .bind(&value.field_id)
        .bind(&value.value)
        .bind(&created_at)
        .bind(&created_at)
        .execute(&mut *tx)
        .await?;
    }
    let create_change_id = append_change(
        &mut tx,
        ChangeEntity::RecurrenceSeries,
        series_id.as_str(),
        None,
        op_type::CREATE_RECURRENCE_SERIES,
        ChangePayload::workspace(workspace)
            .set("series_id", series_id.as_str())
            .set("title", &draft.title)
            .set("description", &draft.description)
            .set("project_id", project.id.as_str())
            .set("project_key", &project.key)
            .set("project_name", &project.name)
            .set("project_prefix", &project.prefix)
            .set("priority", priority.as_str())
            .set("initial_status", initial_status.as_str())
            .set("frequency", draft.schedule.rule.frequency().as_str())
            .set("interval", draft.schedule.rule.interval())
            .set("weekdays", draft.schedule.rule.weekdays_set().to_string())
            .set("timezone", draft.schedule.timezone.as_str())
            .set(
                "start_on",
                draft.schedule.start_on.format("%Y-%m-%d").to_string(),
            )
            .set(
                "available_local_time",
                format_local_time(draft.schedule.available_local_time),
            )
            .set("due_policy", draft.schedule.due_policy.as_str())
            .set("labels", &labels)
            .set("metadata", &series_metadata)
            .set("state", "active")
            .set("stopped_at", "")
            .set("created_at", &created_at)
            .set("updated_at", &created_at),
    )
    .await?;
    for field in SERIES_TEMPLATE_FIELDS {
        set_entity_field_version(
            &mut tx,
            &workspace.id,
            MutableEntityType::RecurrenceSeries,
            series_id.as_str(),
            field,
            &create_change_id,
        )
        .await?;
    }
    for value in &series_metadata {
        set_entity_field_version(
            &mut tx,
            &workspace.id,
            MutableEntityType::RecurrenceSeries,
            series_id.as_str(),
            &format!("metadata:{}", value.field_id),
            &create_change_id,
        )
        .await?;
    }
    let series = load_series(&mut tx, &workspace.id, &series_id).await?;
    let occurrence =
        materialize_occurrence(&mut tx, workspace, &series, &labels, first_slot).await?;
    let task_id = occurrence
        .task_id
        .as_ref()
        .expect("materialized occurrence has a task")
        .clone();
    let task = get_task_in_workspace(&mut tx, workspace, &task_id).await?;
    let series_ref = recurrence_series_ref(&mut tx, &workspace.id, &series_id).await?;
    tx.commit().await?;
    Ok(RecurrenceCreateOutcome {
        series,
        series_ref,
        occurrence,
        task,
    })
}

pub async fn update_recurrence_template(
    conn: &mut SqliteConnection,
    workspace: &Workspace,
    series_id: &RecurrenceSeriesId,
    params: UpdateRecurrenceTemplateParams,
) -> Result<RecurrenceTemplateUpdateOutcome> {
    let UpdateRecurrenceTemplateParams {
        update,
        create_missing_labels,
    } = params;
    if let Some(priority) = update.priority.as_deref() {
        TaskPriority::parse(priority)?;
    }
    if let Some(status) = update.initial_status.as_deref() {
        let status = TaskStatus::parse(status)?;
        ensure!(
            status.is_open(),
            CoreError::validation(format!(
                "error recurrence-initial-status-terminal status={}",
                status.as_str()
            ))
        );
    }

    let mut tx = begin_immediate(conn).await?;
    let current = load_series(&mut tx, &workspace.id, series_id).await?;
    crate::metadata::validate_recurrence_metadata_result(
        &mut tx,
        &workspace.id,
        series_id,
        &update.set_metadata,
        &update.remove_metadata,
    )
    .await?;
    let mut values = Vec::<(&str, String)>::new();
    if let Some(title) = update.title
        && title != current.title
    {
        values.push(("title", title));
    }
    if let Some(description) = update.description
        && description != current.description
    {
        values.push(("description", description));
    }
    if let Some(priority) = update.priority
        && priority != current.priority.as_str()
    {
        values.push(("priority", priority));
    }
    if let Some(status) = update.initial_status
        && status != current.initial_status.as_str()
    {
        values.push(("initial_status", status));
    }
    if let Some(time) = update.available_local_time {
        let value = format_local_time(time);
        if value != format_local_time(current.available_local_time) {
            values.push(("available_local_time", value));
        }
    }
    if let Some(due_policy) = update.due_policy
        && due_policy != current.due_policy
    {
        values.push(("due_policy", due_policy.as_str().to_string()));
    }
    if let Some(project) = update.project {
        let project =
            resolve_or_create_project_in_workspace(&mut tx, &workspace.id, &project).await?;
        if project.id != current.project_id {
            values.push(("project", project.id.to_string()));
        }
    }

    let current_labels = load_series_labels(&mut tx, &workspace.id, series_id).await?;
    let target_labels = if let Some(labels) = update.labels {
        if create_missing_labels {
            resolve_or_create_labels_in_workspace(&mut tx, workspace, &labels)
                .await?
                .names
        } else {
            resolve_labels_in_workspace(&mut tx, &workspace.id, &labels).await?
        }
    } else {
        current_labels.clone()
    };
    let labels_changed = current_labels != target_labels;
    let mut metadata_changed = false;
    for input in &update.set_metadata {
        metadata_changed |=
            crate::metadata::set_recurrence_metadata(&mut tx, workspace, series_id, input).await?;
    }
    for key in &update.remove_metadata {
        metadata_changed |=
            crate::metadata::remove_recurrence_metadata(&mut tx, workspace, series_id, key).await?;
    }
    if values.is_empty() && !labels_changed && !metadata_changed {
        tx.commit().await?;
        return Ok(RecurrenceTemplateUpdateOutcome {
            series: current,
            changed: false,
        });
    }
    if values.is_empty() && !labels_changed {
        let series = load_series(&mut tx, &workspace.id, series_id).await?;
        tx.commit().await?;
        return Ok(RecurrenceTemplateUpdateOutcome {
            series,
            changed: true,
        });
    }

    for (field, _) in &values {
        ensure_no_series_conflict(&mut tx, &workspace.id, series_id, field).await?;
    }
    if labels_changed {
        ensure_no_series_conflict(&mut tx, &workspace.id, series_id, "labels").await?;
    }
    let mut base_versions = BTreeMap::new();
    for (field, _) in &values {
        base_versions.insert(
            (*field).to_string(),
            entity_field_version(
                &mut tx,
                &workspace.id,
                MutableEntityType::RecurrenceSeries,
                series_id.as_str(),
                field,
            )
            .await?,
        );
    }
    if labels_changed {
        base_versions.insert(
            "labels".to_string(),
            entity_field_version(
                &mut tx,
                &workspace.id,
                MutableEntityType::RecurrenceSeries,
                series_id.as_str(),
                "labels",
            )
            .await?,
        );
    }
    let updated_at = now();
    for (field, value) in &values {
        update_series_template_scalar(&mut tx, &workspace.id, series_id, field, value, &updated_at)
            .await?;
    }
    if labels_changed {
        sqlx::query(
            "DELETE FROM recurrence_series_labels WHERE workspace_id = ? AND series_id = ?",
        )
        .bind(&workspace.id)
        .bind(series_id)
        .execute(&mut *tx)
        .await?;
        for label in &target_labels {
            sqlx::query(
                "INSERT INTO recurrence_series_labels(workspace_id, series_id, label)
                 VALUES (?, ?, ?)",
            )
            .bind(&workspace.id)
            .bind(series_id)
            .bind(label)
            .execute(&mut *tx)
            .await?;
        }
    }
    let change_id = append_change(
        &mut tx,
        ChangeEntity::RecurrenceSeries,
        series_id.as_str(),
        None,
        op_type::UPDATE_RECURRENCE_TEMPLATE,
        ChangePayload::workspace(workspace)
            .set("fields", &values)
            .set("base_versions", &base_versions)
            .set("labels_changed", labels_changed)
            .set("labels", &target_labels)
            .set("updated_at", &updated_at),
    )
    .await?;
    for (field, _) in &values {
        set_entity_field_version(
            &mut tx,
            &workspace.id,
            MutableEntityType::RecurrenceSeries,
            series_id.as_str(),
            field,
            &change_id,
        )
        .await?;
    }
    if labels_changed {
        set_entity_field_version(
            &mut tx,
            &workspace.id,
            MutableEntityType::RecurrenceSeries,
            series_id.as_str(),
            "labels",
            &change_id,
        )
        .await?;
    }
    let series = load_series(&mut tx, &workspace.id, series_id).await?;
    tx.commit().await?;
    Ok(RecurrenceTemplateUpdateOutcome {
        series,
        changed: true,
    })
}
async fn update_series_template_scalar(
    conn: &mut SqliteConnection,
    workspace_id: &WorkspaceId,
    series_id: &RecurrenceSeriesId,
    field: &str,
    value: &str,
    updated_at: &str,
) -> Result<()> {
    let query = match field {
        "title" => {
            "UPDATE recurrence_series SET title = ?, updated_at = ? WHERE workspace_id = ? AND id = ?"
        }
        "description" => {
            "UPDATE recurrence_series SET description = ?, updated_at = ? WHERE workspace_id = ? AND id = ?"
        }
        "project" => {
            "UPDATE recurrence_series SET project_id = ?, updated_at = ? WHERE workspace_id = ? AND id = ?"
        }
        "priority" => {
            "UPDATE recurrence_series SET priority = ?, updated_at = ? WHERE workspace_id = ? AND id = ?"
        }
        "initial_status" => {
            "UPDATE recurrence_series SET initial_status = ?, updated_at = ? WHERE workspace_id = ? AND id = ?"
        }
        "available_local_time" => {
            "UPDATE recurrence_series SET available_local_time = ?, updated_at = ? WHERE workspace_id = ? AND id = ?"
        }
        "due_policy" => {
            "UPDATE recurrence_series SET due_policy = ?, updated_at = ? WHERE workspace_id = ? AND id = ?"
        }
        _ => bail!("invalid recurrence template field: {field}"),
    };
    sqlx::query(query)
        .bind(value)
        .bind(updated_at)
        .bind(workspace_id)
        .bind(series_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}
