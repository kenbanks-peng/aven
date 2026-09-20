use super::ChangeWire;
use crate::workspaces::Workspace;

pub(super) fn test_workspace() -> Workspace {
    Workspace {
        id: "0000000000000000".parse().unwrap(),
        key: "default".to_string(),
        name: "default".to_string(),
    }
}

pub(super) fn make_change_wire(
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
