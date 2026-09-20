use super::*;
use crate::change_log::op_type;
use crate::choices::TaskSource;
use crate::recurrence::RecurrenceSeriesId;
use crate::task_fields::TaskField;
use anyhow::{Context, Result, bail};
use serde_json::Value;

pub(super) fn validate_create_workspace(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "workspace")?;
    ensure_sync_id("entity_id", &change.entity_id)?;
    required_string_payload("key", &change.payload)?;
    required_string_payload("name", &change.payload)?;
    required_string_payload("created_at", &change.payload)?;

    Ok(())
}

pub(super) fn validate_set_workspace_field(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "workspace")?;
    ensure_sync_id("entity_id", &change.entity_id)?;
    let field = change
        .field
        .as_deref()
        .filter(|field| !field.trim().is_empty())
        .context("error invalid-sync-change field missing")?;
    if !matches!(field, "name" | "key") {
        bail!("error invalid-sync-change field={field}");
    }
    required_string_payload("value", &change.payload)?;

    Ok(())
}

pub(super) fn validate_create_project(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "project")?;
    ensure_project_id("entity_id", &change.entity_id)?;
    optional_workspace_payload(&change.payload)?;
    required_string_payload("key", &change.payload)?;
    required_string_payload("name", &change.payload)?;
    required_string_payload("prefix", &change.payload)?;
    required_string_payload("created_at", &change.payload)?;

    Ok(())
}

pub(super) fn validate_set_project_metadata(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "project")?;
    ensure_project_id("entity_id", &change.entity_id)?;
    optional_workspace_payload(&change.payload)?;
    required_string_payload("key", &change.payload)?;
    required_string_payload("name", &change.payload)?;
    required_string_payload("prefix", &change.payload)?;
    required_string_payload("updated_at", &change.payload)?;

    Ok(())
}

pub(super) fn validate_create_label(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "label")?;
    optional_workspace_payload(&change.payload)?;
    required_string_payload("name", &change.payload)?;
    required_string_payload("created_at", &change.payload)?;

    Ok(())
}

pub(super) fn validate_create_metadata_field(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "metadata_field")?;
    ensure_metadata_field_id("entity_id", &change.entity_id)?;
    required_workspace_payload(&change.payload)?;
    validate_metadata_key_payload("key", &change.payload)?;
    required_timestamp_payload("created_at", &change.payload)?;

    Ok(())
}

pub(super) fn validate_set_metadata_field(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "metadata_field")?;
    ensure_metadata_field_id("entity_id", &change.entity_id)?;
    required_workspace_payload(&change.payload)?;
    if change.field.as_deref() != Some("key") {
        bail!("error invalid-sync-change field=key");
    }
    validate_metadata_key_payload("key", &change.payload)?;
    optional_bool_payload("conflict_resolution", &change.payload)?;

    Ok(())
}

pub(super) fn validate_task_metadata(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "task")?;
    ensure_sync_id("entity_id", &change.entity_id)?;
    required_workspace_payload(&change.payload)?;
    let field_id = required_string_payload("field_id", &change.payload)?;
    ensure_metadata_field_id("field_id", &field_id)?;
    let expected_field = format!("metadata:{field_id}");
    if change.field.as_deref() != Some(expected_field.as_str()) {
        bail!("error invalid-sync-change metadata-field-mismatch");
    }
    validate_metadata_key_payload("key", &change.payload)?;
    optional_bool_payload("conflict_resolution", &change.payload)?;
    if change.op_type == op_type::SET_TASK_METADATA {
        validate_metadata_value_payload("value", &change.payload)?;
    }

    Ok(())
}

pub(super) fn validate_create_task(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "task")?;
    ensure_sync_id("entity_id", &change.entity_id)?;
    optional_workspace_payload(&change.payload)?;
    required_string_payload("title", &change.payload)?;
    let project_id = required_string_payload("project_id", &change.payload)?;
    ensure_project_id("project_id", &project_id)?;
    optional_string_payload("description", &change.payload)?;
    let recurrence_task = change
        .payload
        .get("series_id")
        .and_then(Value::as_str)
        .is_some();
    if recurrence_task {
        let series_id = required_string_payload("series_id", &change.payload)?;
        series_id
            .parse::<RecurrenceSeriesId>()
            .map_err(|_| anyhow::anyhow!("error invalid-sync-change series_id invalid-id"))?;
        super::recurrence::validate_recurrence_task(change)?;
    } else {
        required_string_payload("project_key", &change.payload)?;
        required_string_payload("project_name", &change.payload)?;
        required_string_payload("project_prefix", &change.payload)?;
    }
    if let Some(status) = optional_string_payload("status", &change.payload)? {
        validate_sync_task_field_value(TaskField::Status, &status)?;
    }
    if let Some(priority) = optional_string_payload("priority", &change.payload)? {
        validate_sync_task_field_value(TaskField::Priority, &priority)?;
    }
    if let Some(source) = optional_string_payload("source", &change.payload)? {
        TaskSource::parse(&source).context("error invalid-sync-change invalid-task-source")?;
    }
    if let Some(available_at) = optional_string_payload("available_at", &change.payload)? {
        validate_sync_task_field_value(TaskField::AvailableAt, &available_at)?;
    }
    if let Some(due_on) = optional_string_payload("due_on", &change.payload)? {
        validate_sync_task_field_value(TaskField::DueOn, &due_on)?;
    }
    if let Some(is_epic) = optional_string_payload("is_epic", &change.payload)? {
        validate_sync_task_field_value(TaskField::IsEpic, &is_epic)?;
    }
    optional_string_array_payload("labels", &change.payload)?;
    validate_metadata_array_payload(&change.payload)?;
    optional_string_payload("created_at", &change.payload)?;

    Ok(())
}

pub(super) fn validate_task_field(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "task")?;
    ensure_sync_id("entity_id", &change.entity_id)?;
    optional_workspace_payload(&change.payload)?;
    let field = change
        .field
        .as_deref()
        .filter(|field| !field.trim().is_empty())
        .context("error invalid-sync-change field missing")?;
    let task_field = TaskField::parse_for_sync(field)?;
    let value = required_string_payload("value", &change.payload)?;
    validate_sync_task_field_value(task_field, &value)?;
    if task_field == TaskField::Project {
        let project_id = required_string_payload("project_id", &change.payload)?;
        ensure_project_id("project_id", &project_id)?;
        if value != project_id {
            bail!("error invalid-sync-change project-value-mismatch");
        }
        required_string_payload("project_key", &change.payload)?;
        required_string_payload("project_name", &change.payload)?;
        required_string_payload("project_prefix", &change.payload)?;
    }

    Ok(())
}

pub(super) fn validate_label_change(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "task")?;
    ensure_sync_id("entity_id", &change.entity_id)?;
    optional_workspace_payload(&change.payload)?;
    required_string_payload("label", &change.payload)?;

    Ok(())
}

pub(super) fn validate_note_add(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "task")?;
    ensure_sync_id("entity_id", &change.entity_id)?;
    optional_workspace_payload(&change.payload)?;
    let note_id = required_string_payload("note_id", &change.payload)?;
    ensure_sync_id("note_id", &note_id)?;
    required_string_payload("body", &change.payload)?;
    required_string_payload("created_at", &change.payload)?;

    Ok(())
}

pub(super) fn validate_dependency_change(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "task")?;
    ensure_sync_id("entity_id", &change.entity_id)?;
    required_workspace_payload(&change.payload)?;
    let depends_on_task_id = required_string_payload("depends_on_task_id", &change.payload)?;
    ensure_sync_id("depends_on_task_id", &depends_on_task_id)?;
    if change.entity_id == depends_on_task_id {
        bail!("error invalid-sync-change dependency-self");
    }

    Ok(())
}

pub(super) fn validate_related_change(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "task")?;
    ensure_sync_id("entity_id", &change.entity_id)?;
    required_workspace_payload(&change.payload)?;
    if change.field.as_deref() != Some("related") {
        bail!("error invalid-sync-change field=related");
    }
    let related_task_id = required_string_payload("related_task_id", &change.payload)?;
    ensure_sync_id("related_task_id", &related_task_id)?;
    if change.entity_id == related_task_id {
        bail!("error invalid-sync-change related-self");
    }

    Ok(())
}

pub(super) fn validate_epic_link_change(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "task")?;
    ensure_sync_id("entity_id", &change.entity_id)?;
    required_workspace_payload(&change.payload)?;
    let epic_task_id = required_string_payload("epic_task_id", &change.payload)?;
    ensure_sync_id("epic_task_id", &epic_task_id)?;
    if change.entity_id == epic_task_id {
        bail!("error invalid-sync-change epic-self");
    }
    if change.op_type == op_type::EPIC_LINK_ADD {
        required_timestamp_payload("created_at", &change.payload)?;
    }

    Ok(())
}

pub(super) fn validate_project_delete(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "project")?;
    ensure_project_id("entity_id", &change.entity_id)?;
    required_workspace_payload(&change.payload)?;
    required_timestamp_payload("deleted_at", &change.payload)?;

    Ok(())
}

pub(super) fn validate_set_label_name(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "label")?;
    required_workspace_payload(&change.payload)?;
    let name = required_string_payload("name", &change.payload)?;
    if name != change.entity_id {
        bail!("error invalid-sync-change label-value-mismatch");
    }
    required_string_payload("new_name", &change.payload)?;
    required_timestamp_payload("renamed_at", &change.payload)?;

    Ok(())
}

pub(super) fn validate_label_delete(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "label")?;
    required_workspace_payload(&change.payload)?;
    let name = required_string_payload("name", &change.payload)?;
    if name != change.entity_id {
        bail!("error invalid-sync-change label-value-mismatch");
    }
    required_timestamp_payload("deleted_at", &change.payload)?;

    Ok(())
}

pub(super) fn validate_note_edit(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "task")?;
    ensure_sync_id("entity_id", &change.entity_id)?;
    if change.field.as_deref() != Some("notes") {
        bail!("error invalid-sync-change field=notes");
    }
    required_workspace_payload(&change.payload)?;
    let note_id = required_string_payload("note_id", &change.payload)?;
    ensure_sync_id("note_id", &note_id)?;
    required_string_payload("body", &change.payload)?;
    required_timestamp_payload("edited_at", &change.payload)?;

    Ok(())
}

pub(super) fn validate_label_restore(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "label")?;
    required_workspace_payload(&change.payload)?;
    let name = required_string_payload("name", &change.payload)?;
    if name != change.entity_id {
        bail!("error invalid-sync-change label-value-mismatch");
    }
    required_timestamp_payload("created_at", &change.payload)?;
    required_timestamp_payload("restored_at", &change.payload)?;
    for key in ["task_ids", "series_ids"] {
        optional_string_array_payload(key, &change.payload)?;
        if change.payload.get(key).is_none() {
            bail!("error invalid-sync-change payload.{key} missing");
        }
    }
    for task_id in string_array_payload("task_ids", &change.payload)? {
        ensure_sync_id("task_ids", &task_id)?;
    }
    for series_id in string_array_payload("series_ids", &change.payload)? {
        ensure_sync_id("series_ids", &series_id)?;
    }

    Ok(())
}

pub(super) fn validate_note_delete(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "task")?;
    ensure_sync_id("entity_id", &change.entity_id)?;
    if change.field.as_deref() != Some("notes") {
        bail!("error invalid-sync-change field=notes");
    }
    required_workspace_payload(&change.payload)?;
    let note_id = required_string_payload("note_id", &change.payload)?;
    ensure_sync_id("note_id", &note_id)?;
    required_timestamp_payload("deleted_at", &change.payload)?;

    Ok(())
}

pub(super) fn validate_recurrence_metadata(change: &ChangeWire) -> Result<()> {
    ensure_entity_type(change, "recurrence_series")?;
    change
        .entity_id
        .parse::<RecurrenceSeriesId>()
        .map_err(|_| anyhow::anyhow!("error invalid-sync-change entity_id invalid-id"))?;
    required_workspace_payload(&change.payload)?;
    let field_id = required_string_payload("field_id", &change.payload)?;
    ensure_metadata_field_id("field_id", &field_id)?;
    let expected_field = format!("metadata:{field_id}");
    if change.field.as_deref() != Some(expected_field.as_str()) {
        bail!("error invalid-sync-change metadata-field-mismatch");
    }
    validate_metadata_key_payload("key", &change.payload)?;
    optional_bool_payload("conflict_resolution", &change.payload)?;
    if change.op_type == op_type::SET_RECURRENCE_METADATA {
        validate_metadata_value_payload("value", &change.payload)?;
    }

    Ok(())
}
