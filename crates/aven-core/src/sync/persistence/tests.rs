use std::collections::HashSet;

use crate::change_log::op_type;
use crate::db::{Database, set_meta};

use std::io::Cursor;
use std::time::Duration;

use image::{DynamicImage, ImageFormat};
use serde_json::json;

use super::*;
use crate::attachments::storage::{object_path, sha256_hex, upsert_inventory_available};
use crate::sync::wire::{
    ChangeWire, MAX_PUSH_BATCH, MAX_SYNC_REQUEST_BYTES, SYNC_PROTOCOL_VERSION, SyncRequest,
};

fn budget_request() -> SyncRequest {
    SyncRequest {
        protocol_version: Some(SYNC_PROTOCOL_VERSION),
        client_id: "budget-client\"\n".to_string(),
        after: i64::MAX,
        pull_limit: Some(super::super::wire::MAX_PULL_BATCH),
        changes: (0..3)
            .map(|index| ChangeWire {
                change_id: format!("AAAAAAAAAAAAAAA{index}"),
                client_id: "budget-client".to_string(),
                local_seq: index + 1,
                entity_type: "task".to_string(),
                entity_id: "BBBBBBBBBBBBBBB0".to_string(),
                field: Some("description".to_string()),
                op_type: op_type::SET_FIELD.to_string(),
                payload: json!({
                    "workspace_id": "0000000000000000",
                    "workspace_key": "default",
                    "value": "quoted \"text\"\n雪".repeat(8),
                }),
                base_version: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                server_seq: None,
            })
            .collect(),
    }
}

#[test]
fn push_byte_budget_selects_exact_ordered_prefix() {
    let request = budget_request();
    for change in &request.changes {
        super::super::wire::validate_pushed_change(change).unwrap();
    }
    let mut prefix = request.clone();
    prefix.changes.truncate(2);
    let limit = serde_json::to_vec(&prefix).unwrap().len();
    let selected = bound_push_request(request.clone(), limit).unwrap();
    assert_eq!(
        serde_json::to_vec(&selected).unwrap(),
        serde_json::to_vec(&prefix).unwrap()
    );
    let selected = bound_push_request(request, limit - 1).unwrap();
    assert_eq!(selected.changes.len(), 1);
    assert_eq!(selected.changes[0].local_seq, 1);
    assert!(serde_json::to_vec(&selected).unwrap().len() < limit);
}

#[test]
fn push_byte_budget_rejects_unfit_first_change_and_envelope() {
    let mut request = budget_request();
    request.changes.truncate(1);
    let limit = serde_json::to_vec(&request).unwrap().len();
    assert_eq!(
        bound_push_request(request.clone(), limit)
            .unwrap()
            .changes
            .len(),
        1
    );
    assert!(
        bound_push_request(request.clone(), limit - 1)
            .unwrap_err()
            .to_string()
            .contains("sync-change-exceeds-request-budget")
    );
    request.changes.clear();
    let limit = serde_json::to_vec(&request).unwrap().len();
    assert!(
        bound_push_request(request.clone(), limit)
            .unwrap()
            .changes
            .is_empty()
    );
    assert!(
        bound_push_request(request, limit - 1)
            .unwrap_err()
            .to_string()
            .contains("sync-request-envelope-too-large")
    );
}

#[test]
fn push_byte_budget_does_not_skip_a_change_that_does_not_fit() {
    let mut request = budget_request();
    let mut prefix = request.clone();
    prefix.changes.truncate(2);
    let limit = serde_json::to_vec(&prefix).unwrap().len();
    request.changes[1].payload["value"] = json!("middle".repeat(limit));
    let selected = bound_push_request(request, limit).unwrap();
    assert_eq!(selected.changes.len(), 1);
    assert_eq!(selected.changes[0].local_seq, 1);
}

#[test]
fn push_byte_budget_preserves_count_bound() {
    let mut request = budget_request();
    request.changes = vec![request.changes[0].clone(); MAX_PUSH_BATCH + 1];
    let selected = bound_push_request(request, MAX_SYNC_REQUEST_BYTES).unwrap();
    assert_eq!(selected.changes.len(), MAX_PUSH_BATCH);
}

#[tokio::test]
async fn server_task_deletion_operations_reconcile_attachment_liveness() {
    let (_temp, mut conn) = crate::test_support::test_conn().await;
    for (index, operation) in [op_type::SET_FIELD, op_type::RESOLVE_FIELD]
        .into_iter()
        .enumerate()
    {
        let task_id = format!("BBBBBBBBBBBBBBB{index}");
        let attachment_id = format!("CCCCCCCCCCCCCCC{index}");
        let sha256 = format!("{index:064x}");
        upsert_inventory_available(&mut conn, &sha256, 1, "image/png")
            .await
            .unwrap();
        sqlx::query("INSERT INTO blob_lifecycle(sha256, unreferenced_at) VALUES (?, NULL)")
            .bind(&sha256)
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO server_blob_references(
               workspace_id, attachment_id, task_id, sha256, byte_size, deleted
             ) VALUES ('0000000000000000', ?, ?, ?, 1, 0)",
        )
        .bind(attachment_id)
        .bind(&task_id)
        .bind(&sha256)
        .execute(&mut *conn)
        .await
        .unwrap();

        let deletion_change = |value: &str| ChangeWire {
            change_id: format!("AAAAAAAAAAAAAA{index}{value}"),
            client_id: "client".to_string(),
            local_seq: 1,
            entity_type: "task".to_string(),
            entity_id: task_id.clone(),
            field: Some("deleted".to_string()),
            op_type: operation.to_string(),
            payload: json!({
                "workspace_id": "0000000000000000",
                "workspace_key": "default",
                "value": value,
            }),
            base_version: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            server_seq: None,
        };

        let mut affected_hashes = HashSet::new();
        apply_server_blob_reference(&mut conn, &deletion_change("1"), &mut affected_hashes)
            .await
            .unwrap();
        let affected_hashes = affected_hashes.into_iter().collect::<Vec<_>>();
        crate::attachments::lifecycle::reconcile_liveness_for_hashes_in_transaction(
            &mut conn,
            &affected_hashes,
            &crate::attachments::lifecycle::SystemClock,
        )
        .await
        .unwrap();
        let deleted: bool = sqlx::query_scalar(
            "SELECT deleted FROM server_task_tombstones
             WHERE workspace_id = '0000000000000000' AND task_id = ?",
        )
        .bind(&task_id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        let unreferenced_at: Option<String> =
            sqlx::query_scalar("SELECT unreferenced_at FROM blob_lifecycle WHERE sha256 = ?")
                .bind(&sha256)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert!(deleted, "{operation} must apply live-to-deleted state");
        assert!(unreferenced_at.is_some());

        let mut affected_hashes = HashSet::new();
        apply_server_blob_reference(&mut conn, &deletion_change("0"), &mut affected_hashes)
            .await
            .unwrap();
        let affected_hashes = affected_hashes.into_iter().collect::<Vec<_>>();
        crate::attachments::lifecycle::reconcile_liveness_for_hashes_in_transaction(
            &mut conn,
            &affected_hashes,
            &crate::attachments::lifecycle::SystemClock,
        )
        .await
        .unwrap();
        let deleted: bool = sqlx::query_scalar(
            "SELECT deleted FROM server_task_tombstones
             WHERE workspace_id = '0000000000000000' AND task_id = ?",
        )
        .bind(&task_id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        let unreferenced_at: Option<String> =
            sqlx::query_scalar("SELECT unreferenced_at FROM blob_lifecycle WHERE sha256 = ?")
                .bind(&sha256)
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert!(!deleted, "{operation} must apply deleted-to-live state");
        assert_eq!(unreferenced_at, None);
    }
}

#[tokio::test]
async fn related_comparison_observes_push_acknowledgement_first() {
    let (_temp, mut conn) = crate::test_support::test_conn().await;
    let workspace = crate::workspaces::Workspace::default();
    let project = crate::projects::create_project(&mut conn, &workspace, "related-ack")
        .await
        .unwrap();
    let task_id: crate::ids::TaskId = "AAAA000000000001".parse().unwrap();
    let related_task_id: crate::ids::TaskId = "BBBB000000000002".parse().unwrap();
    for id in [&task_id, &related_task_id] {
        sqlx::query(
            "INSERT INTO tasks(id, workspace_id, title, description, project_id, status, priority, created_at, updated_at)
             VALUES (?, ?, 'task', '', ?, 'todo', 'none', 't', 't')",
        )
        .bind(id)
        .bind(&workspace.id)
        .bind(&project.id)
        .execute(&mut *conn)
        .await
        .unwrap();
    }
    let local = crate::operations::set_task_related_link_in_transaction(
        &mut conn,
        &workspace,
        &task_id,
        &related_task_id,
        true,
    )
    .await
    .unwrap();
    let local_change_id = local.change_id.unwrap();
    set_meta(&mut conn, "sync_cursor", "3").await.unwrap();

    let remote = ChangeWire {
        change_id: "CCCC000000000003".to_string(),
        client_id: "remote".to_string(),
        local_seq: 1,
        entity_type: "task".to_string(),
        entity_id: task_id.to_string(),
        field: Some("related".to_string()),
        op_type: op_type::RELATED_REMOVE.to_string(),
        payload: json!({
            "workspace_id": workspace.id,
            "workspace_key": workspace.key,
            "related_task_id": related_task_id,
        }),
        base_version: None,
        created_at: "2026-08-22T00:00:00Z".to_string(),
        server_seq: Some(4),
    };
    let page = ApplySyncPage {
        request: SyncRequest {
            protocol_version: Some(SYNC_PROTOCOL_VERSION),
            client_id: "local".to_string(),
            after: 3,
            pull_limit: Some(100),
            changes: Vec::new(),
        },
        response: SyncResponse {
            protocol_version: SYNC_PROTOCOL_VERSION,
            cursor: 4,
            has_more: false,
            push_acks: vec![PushAck {
                change_id: local_change_id.clone(),
                server_seq: 5,
            }],
            changes: vec![remote],
        },
        attempted_at: "2026-08-22T00:00:01Z".to_string(),
        previous_pushed: 0,
        previous_pulled: 0,
    };

    apply_sync_response(&mut conn, page, None).await.unwrap();

    let state: (i64, String) = sqlx::query_as(
        "SELECT linked, last_change_id FROM task_related_links
         WHERE workspace_id = ? AND task_a_id = ? AND task_b_id = ?",
    )
    .bind(&workspace.id)
    .bind(&task_id)
    .bind(&related_task_id)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(state, (1, local_change_id.clone()));
    let acknowledged: Option<i64> =
        sqlx::query_scalar("SELECT server_seq FROM changes WHERE change_id = ?")
            .bind(local_change_id)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(acknowledged, Some(5));
}

async fn corrupt_attachment_page() -> (
    tempfile::TempDir,
    Database,
    std::path::PathBuf,
    ServerSyncPage,
) {
    let temp = tempfile::tempdir().unwrap();
    let database = Database::open(&temp.path().join("server.sqlite"))
        .await
        .unwrap();
    let blob_dir = temp.path().join("blobs");
    let expected = b"expected";
    let sha256 = sha256_hex(expected);
    {
        let mut conn = database.acquire_writer().await.unwrap();
        upsert_inventory_available(&mut conn, &sha256, expected.len() as i64, "image/png")
            .await
            .unwrap();
    }
    let path = object_path(&blob_dir, &sha256).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"corrupt!").unwrap();
    let change = ChangeWire {
        change_id: "0123456789ABCDEF".to_string(),
        client_id: "client-a".to_string(),
        local_seq: 1,
        entity_type: "task".to_string(),
        entity_id: "0123456789ABCDE0".to_string(),
        field: Some("attachments".to_string()),
        op_type: op_type::ATTACHMENT_ADD.to_string(),
        payload: json!({
            "workspace_id": "0000000000000000",
            "workspace_key": "default",
            "attachment_id": "7KQ9A1X4MV2P8D6R",
            "sha256": sha256,
            "byte_size": expected.len(),
            "media_type": "image/png",
            "filename": "photo.png",
            "alt_text": "photo",
            "width": 1,
            "height": 1,
            "created_at": "2026-01-01T00:00:00Z"
        }),
        base_version: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        server_seq: None,
    };
    let page = ServerSyncPage {
        request: SyncRequest {
            protocol_version: Some(SYNC_PROTOCOL_VERSION),
            client_id: "client-a".to_string(),
            after: 0,
            pull_limit: Some(100),
            changes: vec![change],
        },
    };
    (temp, database, blob_dir, page)
}

fn page_for_contract(contract: &super::super::wire::BlobUploadContract) -> ServerSyncPage {
    let change = ChangeWire {
        change_id: "0123456789ABCDEF".to_string(),
        client_id: "client-a".to_string(),
        local_seq: 1,
        entity_type: "task".to_string(),
        entity_id: "0123456789ABCDE0".to_string(),
        field: Some("attachments".to_string()),
        op_type: op_type::ATTACHMENT_ADD.to_string(),
        payload: json!({
            "workspace_id": contract.workspace_id,
            "workspace_key": "default",
            "attachment_id": "7KQ9A1X4MV2P8D6R",
            "sha256": contract.sha256,
            "byte_size": contract.byte_size,
            "media_type": contract.media_type,
            "filename": "photo.png",
            "alt_text": "photo",
            "width": contract.width,
            "height": contract.height,
            "created_at": "2026-01-01T00:00:00Z"
        }),
        base_version: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        server_seq: None,
    };
    ServerSyncPage {
        request: SyncRequest {
            protocol_version: Some(SYNC_PROTOCOL_VERSION),
            client_id: "client-a".to_string(),
            after: 0,
            pull_limit: Some(100),
            changes: vec![change],
        },
    }
}

#[tokio::test]
async fn corrupt_server_blob_is_rejected_before_writer_acquisition() {
    let (_temp, database, blob_dir, page) = corrupt_attachment_page().await;
    let writer = database.acquire_writer().await.unwrap();
    let task = tokio::spawn({
        let database = database.clone();
        async move {
            database
                .persist_server_sync_page_with_blobs(page, &blob_dir)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        task.is_finished(),
        "corrupt content validation should not wait for the writer gate"
    );
    drop(writer);
    let error = task.await.unwrap().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("attachment-blob-content-mismatch")
    );
}

#[tokio::test]
async fn reservation_is_rechecked_after_blob_preparation() {
    let temp = tempfile::tempdir().unwrap();
    let database = Database::open(&temp.path().join("server.sqlite"))
        .await
        .unwrap();
    let blob_dir = temp.path().join("blobs");
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::new_rgba8(1, 1)
        .write_to(&mut encoded, ImageFormat::Png)
        .unwrap();
    let bytes = encoded.into_inner();
    let contract = super::super::wire::BlobUploadContract {
        workspace_id: "0000000000000000".to_string(),
        sha256: sha256_hex(&bytes),
        byte_size: bytes.len() as i64,
        media_type: "image/png".to_string(),
        width: 1,
        height: 1,
    };
    database
        .store_server_blob(
            &blob_dir,
            crate::attachments::lifecycle::LifecyclePolicy::default(),
            &contract,
            bytes,
        )
        .await
        .unwrap();
    let page = page_for_contract(&contract);
    {
        let mut reader = database.acquire_reader().await.unwrap();
        prepare_server_blobs(&mut reader, &blob_dir, &page.request.changes)
            .await
            .unwrap();
    }
    {
        let mut writer = database.acquire_writer().await.unwrap();
        sqlx::query("DELETE FROM blob_upload_reservations")
            .execute(&mut *writer)
            .await
            .unwrap();
    }
    let mut writer = database.acquire_writer().await.unwrap();
    let error = assign_server_sequences(&mut writer, page.request.changes, Some(&blob_dir))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("attachment-blob-unreserved"));
    let accepted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM changes")
        .fetch_one(&mut *writer)
        .await
        .unwrap();
    assert_eq!(accepted, 0);
}
