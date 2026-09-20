use super::*;
use crate::change_log::{ChangePayload, op_type};
use crate::recurrence::{
    RecurrenceDuePolicy, RecurrenceRule, RecurrenceSchedule, RecurrenceSeriesId,
    derive_occurrence_identity,
};
use crate::workspaces::Workspace;
use chrono::NaiveDate;

fn test_workspace() -> Workspace {
    Workspace {
        id: "0000000000000000".parse().unwrap(),
        key: "default".to_string(),
        name: "default".to_string(),
    }
}

fn make_change_wire(
    op_type: &str,
    entity_type: &str,
    entity_id: &str,
    payload: serde_json::Value,
) -> ChangeWire {
    ChangeWire {
        change_id: "AAAAAAAAAAAAAAA0".to_string(),
        client_id: "client".to_string(),
        local_seq: 1,
        entity_type: entity_type.to_string(),
        entity_id: entity_id.to_string(),
        field: None,
        op_type: op_type.to_string(),
        payload,
        base_version: None,
        created_at: "2026-06-01T00:00:00Z".to_string(),
        server_seq: None,
    }
}

#[test]
fn sync_timestamps_accept_fractional_utc_precision() {
    validate_timestamp_value("created_at", "2026-07-28T06:41:44Z").unwrap();
    validate_timestamp_value("created_at", "2026-07-28T06:41:44.000000Z").unwrap();
    validate_timestamp_value("created_at", "2026-07-28T06:41:44.123456789Z").unwrap();
}

#[test]
fn sync_timestamps_reject_offsets_and_invalid_calendar_values() {
    for value in ["2026-07-28T08:41:44+02:00", "2026-13-28T06:41:44Z", "today"] {
        assert!(validate_timestamp_value("created_at", value).is_err());
    }
}

#[test]
fn recurrence_projection_rejects_nondeterministic_change_timestamp() {
    let workspace = test_workspace();
    let series_id: RecurrenceSeriesId = "AAAAAAAAAAAAAAAA".parse().unwrap();
    let schedule = RecurrenceSchedule::new(
        RecurrenceRule::daily(),
        "UTC".parse().unwrap(),
        "2026-07-20".parse().unwrap(),
        None,
        RecurrenceDuePolicy::SameDay,
    );
    let slot_on: NaiveDate = "2026-07-20".parse().unwrap();
    let identity =
        derive_occurrence_identity(&workspace.id, &series_id, &schedule, slot_on).unwrap();
    let payload = ChangePayload::workspace(&workspace)
        .set("series_id", series_id.as_str())
        .set("slot_on", slot_on.to_string())
        .set("task_id", identity.task_id.as_str())
        .set("projected_at", &identity.occurrence_link.projected_at)
        .set("task_change_id", &identity.task_change_id)
        .set("occurrence_change_id", &identity.occurrence_change_id)
        .set(
            "task_field_version_seed",
            &identity.field_version_seeds.task,
        )
        .set(
            "occurrence_field_version_seed",
            &identity.field_version_seeds.occurrence,
        )
        .set("frequency", "daily")
        .set("interval", 1)
        .set("weekdays", "")
        .set("timezone", "UTC")
        .set("start_on", "2026-07-20")
        .set("available_local_time", "")
        .set("due_policy", "same_day")
        .into_value();
    let mut change = make_change_wire(
        op_type::PROJECT_RECURRENCE_OCCURRENCE,
        "recurrence_series",
        series_id.as_str(),
        payload,
    );
    change.change_id = identity.occurrence_change_id;
    change.field = Some("projection".to_string());
    change.created_at = identity.occurrence_link.projected_at;
    validate_pushed_change(&change).unwrap();

    change.created_at = "2026-07-20T00:00:01Z".to_string();
    assert!(
        validate_pushed_change(&change)
            .unwrap_err()
            .to_string()
            .contains("recurrence-deterministic-mismatch")
    );
}

#[test]
fn request_pull_limit_has_default_and_bounds() {
    assert_eq!(request_pull_limit(None).unwrap(), MAX_PULL_BATCH);
    assert!(request_pull_limit(Some(MAX_PULL_BATCH)).is_ok());
    assert_eq!(
        request_pull_limit(Some(0)).unwrap_err().to_string(),
        "error sync-pull-limit-out-of-range min=1 max=512 got=0"
    );
    assert_eq!(
        request_pull_limit(Some(MAX_PULL_BATCH + 1))
            .unwrap_err()
            .to_string(),
        "error sync-pull-limit-out-of-range min=1 max=512 got=513"
    );
}

#[test]
fn request_envelope_rejects_negative_cursor_and_oversized_push_batch() {
    let request = SyncRequest {
        protocol_version: Some(SYNC_PROTOCOL_VERSION),
        client_id: "test-client".to_string(),
        after: -1,
        pull_limit: Some(MAX_PULL_BATCH),
        changes: Vec::new(),
    };
    assert_eq!(
        validate_sync_request_envelope(&request)
            .unwrap_err()
            .to_string(),
        "error sync-after-out-of-range min=0 got=-1"
    );

    let request = SyncRequest {
        protocol_version: Some(SYNC_PROTOCOL_VERSION),
        client_id: "test-client".to_string(),
        after: 0,
        pull_limit: Some(MAX_PULL_BATCH),
        changes: vec![
            ChangeWire {
                change_id: "AAAAAAAAAAAAAAA0".to_string(),
                client_id: "client".to_string(),
                local_seq: 1,
                entity_type: "task".to_string(),
                entity_id: "BBBBBBBBBBBBBBBB".to_string(),
                field: None,
                op_type: "create_task".to_string(),
                payload: serde_json::json!({"title":"oops","project_id":"0000000000000000","project_key":"app","project_name":"app","project_prefix":"APP","workspace_id":"0000000000000000","workspace_key":"default","created_at":"2026-01-01T00:00:00Z"}),
                base_version: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                server_seq: None,
            };
            MAX_PUSH_BATCH + 1
        ],
    };
    assert_eq!(
        validate_sync_request_envelope(&request)
            .unwrap_err()
            .to_string(),
        "error sync-push-too-large limit=256 got=257"
    );
}

#[test]
fn response_validation_respects_request_pull_limit() {
    let response = SyncResponse {
        protocol_version: SYNC_PROTOCOL_VERSION,
        cursor: 1,
        has_more: false,
        push_acks: vec![],
        changes: vec![
            ChangeWire {
                change_id: "AAAAAAAAAAAAAAA1".to_string(),
                client_id: "client".to_string(),
                local_seq: 1,
                entity_type: "task".to_string(),
                entity_id: "BBBBBBBBBBBBBBBB".to_string(),
                field: None,
                op_type: "create_task".to_string(),
                payload: serde_json::json!({
                    "title":"one",
                    "project_id":"0000000000000000",
                    "project_key":"app",
                    "project_name":"app",
                    "project_prefix":"APP",
                }),
                base_version: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                server_seq: Some(1),
            };
            MAX_PULL_BATCH as usize + 1
        ],
    };
    assert_eq!(
        validate_sync_response_for_request(0, MAX_PULL_BATCH, &[], &response)
            .unwrap_err()
            .to_string(),
        "error invalid-sync-response pull-too-large limit=512 got=513"
    );
}

#[test]
fn constructed_create_task_payload_passes_wire_validation() {
    let ws = test_workspace();
    let payload = ChangePayload::workspace(&ws)
        .set("title", "test task")
        .set("description", "a description")
        .set("project_id", "1111111111111111")
        .set("project_key", "app")
        .set("project_name", "App")
        .set("project_prefix", "APP")
        .set("status", "inbox")
        .set("priority", "none")
        .set("created_at", "2026-06-01T00:00:00Z")
        .into_value();
    let change = make_change_wire(op_type::CREATE_TASK, "task", "BBBBBBBBBBBBBBBB", payload);
    validate_pushed_change(&change)
        .expect("create_task payload built with ChangePayload should be wire-valid");
}

#[test]
fn constructed_create_project_payload_passes_wire_validation() {
    let ws = test_workspace();
    let payload = ChangePayload::workspace(&ws)
        .set("key", "app")
        .set("name", "App")
        .set("prefix", "APP")
        .set("created_at", "2026-06-01T00:00:00Z")
        .into_value();
    let change = make_change_wire(
        op_type::CREATE_PROJECT,
        "project",
        "1111111111111111",
        payload,
    );
    validate_pushed_change(&change)
        .expect("create_project payload built with ChangePayload should be wire-valid");
}

#[test]
fn project_changes_reject_invalid_project_ids() {
    let ws = test_workspace();
    let project_payload = ChangePayload::workspace(&ws)
        .set("key", "app")
        .set("name", "App")
        .set("prefix", "APP")
        .set("created_at", "2026-06-01T00:00:00Z")
        .into_value();
    let create_project = make_change_wire(
        op_type::CREATE_PROJECT,
        "project",
        "invalid",
        project_payload,
    );
    assert_eq!(
        validate_pushed_change(&create_project)
            .unwrap_err()
            .to_string(),
        "error invalid-sync-change entity_id invalid-id"
    );

    let task_payload = ChangePayload::workspace(&ws)
        .set("title", "test task")
        .set("project_id", "invalid")
        .set("project_key", "app")
        .set("project_name", "App")
        .set("project_prefix", "APP")
        .set("created_at", "2026-06-01T00:00:00Z")
        .into_value();
    let create_task = make_change_wire(
        op_type::CREATE_TASK,
        "task",
        "BBBBBBBBBBBBBBBB",
        task_payload,
    );
    assert_eq!(
        validate_pushed_change(&create_task)
            .unwrap_err()
            .to_string(),
        "error invalid-sync-change project_id invalid-id"
    );

    let field_payload = ChangePayload::workspace(&ws)
        .set("value", "invalid")
        .set("project_id", "invalid")
        .set("project_key", "app")
        .set("project_name", "App")
        .set("project_prefix", "APP")
        .into_value();
    let mut set_field = make_change_wire(
        op_type::SET_FIELD,
        "task",
        "BBBBBBBBBBBBBBBB",
        field_payload,
    );
    set_field.field = Some("project".to_string());
    assert_eq!(
        validate_pushed_change(&set_field).unwrap_err().to_string(),
        "error invalid-sync-change project_id invalid-id"
    );
}

#[test]
fn constructed_label_add_payload_passes_wire_validation() {
    let ws = test_workspace();
    let payload = ChangePayload::workspace(&ws)
        .set("label", "bug")
        .into_value();
    let mut change = make_change_wire(op_type::LABEL_ADD, "task", "BBBBBBBBBBBBBBBB", payload);
    change.field = Some("labels".to_string());
    validate_pushed_change(&change)
        .expect("label_add payload built with ChangePayload should be wire-valid");
}

#[test]
fn constructed_label_administration_payloads_pass_wire_validation() {
    let ws = test_workspace();
    let rename = make_change_wire(
        op_type::SET_LABEL_NAME,
        "label",
        "old",
        ChangePayload::workspace(&ws)
            .set("name", "old")
            .set("new_name", "new")
            .set("renamed_at", "2026-06-01T00:00:00Z")
            .into_value(),
    );
    validate_pushed_change(&rename).unwrap();

    let restore = make_change_wire(
        op_type::LABEL_RESTORE,
        "label",
        "new",
        ChangePayload::workspace(&ws)
            .set("name", "new")
            .set("created_at", "2026-06-01T00:00:00Z")
            .set("task_ids", ["BBBBBBBBBBBBBBBB"])
            .set("series_ids", ["CCCCCCCCCCCCCCCC"])
            .set("restored_at", "2026-06-01T00:00:01Z")
            .into_value(),
    );
    validate_pushed_change(&restore).unwrap();
}

#[test]
fn constructed_dependency_add_payload_passes_wire_validation() {
    let ws = test_workspace();
    let payload = ChangePayload::workspace(&ws)
        .set("depends_on_task_id", "CCCCCCCCCCCCCCCC")
        .into_value();
    let mut change = make_change_wire(op_type::DEPENDENCY_ADD, "task", "BBBBBBBBBBBBBBBB", payload);
    change.field = Some("dependencies".to_string());
    validate_pushed_change(&change)
        .expect("dependency_add payload built with ChangePayload should be wire-valid");
}

#[test]
fn related_payload_validation_requires_distinct_task_endpoints() {
    let ws = test_workspace();
    let payload = ChangePayload::workspace(&ws)
        .set("related_task_id", "CCCCCCCCCCCCCCCC")
        .into_value();
    let mut change = make_change_wire(op_type::RELATED_ADD, "task", "BBBBBBBBBBBBBBBB", payload);
    change.field = Some("related".to_string());
    validate_pushed_change(&change)
        .expect("related_add payload built with ChangePayload should be wire-valid");

    change.payload["related_task_id"] = serde_json::json!("BBBBBBBBBBBBBBBB");
    assert_eq!(
        validate_pushed_change(&change).unwrap_err().to_string(),
        "error invalid-sync-change related-self"
    );
}

#[test]
fn constructed_note_edit_payload_passes_wire_validation() {
    let ws = test_workspace();
    let payload = ChangePayload::workspace(&ws)
        .set("note_id", "DDDDDDDDDDDDDDDD")
        .set("body", "corrected note body")
        .set("edited_at", "2026-06-01T01:00:00Z")
        .into_value();
    let mut change = make_change_wire(op_type::NOTE_EDIT, "task", "BBBBBBBBBBBBBBBB", payload);
    change.field = Some("notes".to_string());
    validate_pushed_change(&change)
        .expect("note_edit payload built with ChangePayload should be wire-valid");
}

#[test]
fn constructed_note_add_payload_passes_wire_validation() {
    let ws = test_workspace();
    let payload = ChangePayload::workspace(&ws)
        .set("note_id", "DDDDDDDDDDDDDDDD")
        .set("body", "note body")
        .set("created_at", "2026-06-01T00:00:00Z")
        .into_value();
    let mut change = make_change_wire(op_type::NOTE_ADD, "task", "BBBBBBBBBBBBBBBB", payload);
    change.field = Some("notes".to_string());
    validate_pushed_change(&change)
        .expect("note_add payload built with ChangePayload should be wire-valid");
}

fn attachment_add_payload() -> Value {
    ChangePayload::workspace(&test_workspace())
        .set("attachment_id", "7KQ9A1X4MV2P8D6R")
        .set(
            "sha256",
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        )
        .set("byte_size", 12_i64)
        .set("media_type", "image/png")
        .set("filename", None::<&str>)
        .set("alt_text", None::<&str>)
        .set("width", 320_i64)
        .set("height", 240_i64)
        .set("created_at", "2026-06-01T00:00:00Z")
        .into_value()
}

fn attachment_change(op_type: &str, payload: Value) -> ChangeWire {
    let mut change = make_change_wire(op_type, "task", "BBBBBBBBBBBBBBBB", payload);
    change.field = Some("attachments".to_string());
    change
}

#[test]
fn constructed_attachment_payloads_parse_without_json_or_protocol_changes() {
    let add_value = attachment_add_payload();
    let add = AttachmentAddPayload::from_change(&attachment_change(
        op_type::ATTACHMENT_ADD,
        add_value.clone(),
    ))
    .unwrap();
    assert_eq!(serde_json::to_value(add).unwrap(), add_value);

    let delete_value = ChangePayload::workspace(&test_workspace())
        .set("attachment_id", "7KQ9A1X4MV2P8D6R")
        .set("deleted_at", "2026-06-01T00:00:00Z")
        .into_value();
    let delete = AttachmentDeletePayload::from_change(&attachment_change(
        op_type::ATTACHMENT_DELETE,
        delete_value.clone(),
    ))
    .unwrap();
    assert_eq!(serde_json::to_value(delete).unwrap(), delete_value);
    assert_eq!(SYNC_PROTOCOL_VERSION, 18);
}

#[test]
fn attachment_payloads_accept_extensions_and_missing_optional_text() {
    let mut payload = attachment_add_payload();
    let object = payload.as_object_mut().unwrap();
    object.remove("filename");
    object.remove("alt_text");
    object.insert("extension".to_string(), serde_json::json!({ "version": 1 }));

    AttachmentAddPayload::from_change(&attachment_change(op_type::ATTACHMENT_ADD, payload))
        .unwrap();
}

#[test]
fn attachment_payload_authority_rejects_malformed_fields() {
    let cases = [
        ("workspace_id", Value::Null, "workspace_id"),
        (
            "attachment_id",
            Value::String("short".to_string()),
            "attachment_id",
        ),
        (
            "sha256",
            Value::String("short".to_string()),
            "invalid-sha256",
        ),
        ("byte_size", Value::String("12".to_string()), "byte_size"),
        ("byte_size", Value::from(-1), "invalid-attachment-size"),
        (
            "media_type",
            Value::String("image/svg+xml".to_string()),
            "unsupported-attachment-media-type",
        ),
        (
            "filename",
            Value::String("bad/name.png".to_string()),
            "invalid-attachment-filename",
        ),
        (
            "alt_text",
            Value::String("bad\nalt".to_string()),
            "invalid-attachment-alt-text",
        ),
        ("height", Value::Null, "invalid-attachment-dimensions"),
        ("width", Value::from(0), "invalid-attachment-dimensions"),
        (
            "created_at",
            Value::String("today".to_string()),
            "invalid-timestamp",
        ),
    ];
    for (key, value, expected) in cases {
        let mut payload = attachment_add_payload();
        payload[key] = value;
        let error =
            AttachmentAddPayload::from_change(&attachment_change(op_type::ATTACHMENT_ADD, payload))
                .unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "{key} error did not contain {expected}: {error}"
        );
    }

    let mut wrong_field = attachment_change(op_type::ATTACHMENT_ADD, attachment_add_payload());
    wrong_field.field = Some("description".to_string());
    assert!(
        AttachmentAddPayload::from_change(&wrong_field)
            .unwrap_err()
            .to_string()
            .contains("field=attachments")
    );

    let mut invalid_entity = attachment_change(op_type::ATTACHMENT_ADD, attachment_add_payload());
    invalid_entity.entity_id = "invalid".to_string();
    assert!(
        AttachmentAddPayload::from_change(&invalid_entity)
            .unwrap_err()
            .to_string()
            .contains("entity_id")
    );
}
