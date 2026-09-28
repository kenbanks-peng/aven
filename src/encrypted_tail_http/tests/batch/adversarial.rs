use super::*;
use axum::{
    body::Body,
    extract::{Request, State},
};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, PartialEq, Eq)]
struct ProjectionRows {
    parents: Vec<(String, String, bool, bool, Option<String>)>,
    images: Vec<(
        Vec<u8>,
        Option<Vec<u8>>,
        i64,
        Option<i64>,
        Vec<u8>,
        Option<String>,
        bool,
    )>,
    references: Vec<(String, String, String, bool, Option<Vec<u8>>)>,
}

#[derive(Debug, PartialEq, Eq)]
struct ServerRows {
    high_water: i64,
    tail: Vec<(String, i64, Vec<u8>, Vec<u8>)>,
    projection: ProjectionRows,
}

async fn server_rows(db: &Database) -> ServerRows {
    let mut conn = aven_core::test_support::acquire(db).await.unwrap();
    let high_water = sqlx::query_scalar("SELECT high_water FROM server_e2ee_allocator")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let tail = sqlx::query_as(
        "SELECT operation_id,sequence,commitment,record FROM server_e2ee_tail ORDER BY sequence",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let parents = sqlx::query_as(
        "SELECT workspace,parent,deleted,protected,version FROM server_e2ee_image_parents ORDER BY workspace,parent",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let images = sqlx::query_as(
        "SELECT object,bootstrap,byte_size,unreferenced_at,descriptor,origin,complete FROM server_e2ee_images ORDER BY object",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let references = sqlx::query_as(
        "SELECT workspace,reference,parent,deleted,object FROM server_e2ee_image_references ORDER BY workspace,reference",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    ServerRows {
        high_water,
        tail,
        projection: ProjectionRows {
            parents,
            images,
            references,
        },
    }
}

async fn projection_failure_fixture() -> (Fixture, TailSnapshot, Vec<(String, Vec<u8>)>) {
    let f = fixture().await;
    converge(&f).await;
    edits(&f, 3).await;
    drain(&Client::new(&f.origin).unwrap(), &f.peer_store, &f.peer).await;
    let mut conn = aven_core::test_support::acquire(&f.peer).await.unwrap();
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM tasks WHERE title LIKE 'batch task %' ORDER BY title")
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    drop(conn);
    assert_eq!(ids.len(), 3);
    let workspace = f.peer.list_workspaces().await.unwrap().remove(0);
    for id in ids {
        f.peer
            .update_task(
                &workspace,
                &id.parse().unwrap(),
                TaskUpdate {
                    deleted: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let (inputs, records) = frozen(&f).await;
    assert_eq!(records.len(), 3);
    (f, inputs, records)
}

#[tokio::test]
async fn late_sql_failure_restores_existing_parent_and_image_projection_rows_exactly() {
    let (f, inputs, records) = projection_failure_fixture().await;
    let before = server_rows(&f.server).await;
    assert!(!before.projection.parents.is_empty());
    {
        let mut conn = aven_core::test_support::acquire(&f.server).await.unwrap();
        sqlx::query("INSERT INTO meta(key,value) VALUES ('batch_failure_id', ?)")
            .bind(&records[2].0)
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER fail_batch_parent BEFORE INSERT ON server_e2ee_tail WHEN NEW.operation_id=(SELECT value FROM meta WHERE key='batch_failure_id') BEGIN SELECT RAISE(ABORT, 'injected'); END")
            .execute(&mut *conn)
            .await
            .unwrap();
    }
    assert!(append(&f, &inputs, &records).await.is_err());
    assert_eq!(server_rows(&f.server).await, before);
    {
        let mut conn = aven_core::test_support::acquire(&f.server).await.unwrap();
        sqlx::query("DROP TRIGGER fail_batch_parent")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("DELETE FROM meta WHERE key='batch_failure_id'")
            .execute(&mut *conn)
            .await
            .unwrap();
    }
    // A known final member is rejected before either new member is inserted.
    // Compare all server rows so the insert-only refusal cannot partially apply.
    f.server
        .encrypted_tail_exchange(
            &inputs.authority.context,
            &inputs.bearer,
            Operation::Append {
                record: records[2].1.clone(),
                ticket: None,
            },
        )
        .await
        .unwrap();
    let after_single = server_rows(&f.server).await;
    let error = append(&f, &inputs, &records)
        .await
        .err()
        .expect("known batch member must be refused");
    assert!(error.to_string().contains("encrypted-tail-batch-known"));
    assert_eq!(server_rows(&f.server).await, after_single);
}

#[derive(Clone, Copy)]
enum ResponseFault {
    Reordered,
    ResolvedAbsent,
}

struct FaultState {
    fault: AtomicUsize,
}

impl FaultState {
    fn new(fault: ResponseFault) -> Self {
        Self {
            fault: AtomicUsize::new(fault as usize + 1),
        }
    }

    fn take(&self, fault: ResponseFault) -> bool {
        self.fault
            .compare_exchange(fault as usize + 1, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

async fn corrupt_response(
    State(state): State<Arc<FaultState>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let request_bytes = axum::body::to_bytes(body, tail::BATCH_APPEND_LIMIT)
        .await
        .unwrap();
    let request_value: Value = serde_json::from_slice(&request_bytes).unwrap();
    let path = parts.uri.path().to_owned();
    let response = next
        .run(Request::from_parts(parts, Body::from(request_bytes)))
        .await;
    if path != aven_core::sync::client::tail::BATCH_PATH || !response.status().is_success() {
        return response;
    }
    let is_resolve = request_value["operation"].get("Resolve").is_some();
    let fault = match (is_resolve, state.fault.load(Ordering::SeqCst)) {
        (false, n) if n == ResponseFault::Reordered as usize + 1 => ResponseFault::Reordered,
        (true, n) if n == ResponseFault::ResolvedAbsent as usize + 1 => {
            ResponseFault::ResolvedAbsent
        }
        _ => return response,
    };
    if !state.take(fault) {
        return response;
    }
    let (mut response_parts, response_body) = response.into_parts();
    let bytes = axum::body::to_bytes(response_body, tail::BATCH_CONTROL_LIMIT)
        .await
        .unwrap();
    let mut value: Value = serde_json::from_slice(&bytes).unwrap();
    if matches!(fault, ResponseFault::ResolvedAbsent) {
        let ids = request_value["operation"]["Resolve"]["operation_ids"]
            .as_array()
            .unwrap();
        value["operation"]["Resolved"].as_array_mut().unwrap()[0] = serde_json::json!({
            "Absent": { "operation_id": ids[0] }
        });
    } else {
        value["operation"]["Appended"]
            .as_array_mut()
            .unwrap()
            .swap(0, 1);
    }
    response_parts
        .headers
        .remove(reqwest::header::CONTENT_LENGTH);
    Response::from_parts(
        response_parts,
        Body::from(serde_json::to_vec(&value).unwrap()),
    )
}

async fn instrument_fault(f: &mut Fixture, fault: ResponseFault) {
    f.task.abort();
    let _ = (&mut f.task).await;
    let state = Arc::new(FaultState::new(fault));
    let app = crate::peer_enrollment_http::router(f.server.clone())
        .merge(router(f.server.clone(), Default::default()))
        .layer(axum::middleware::from_fn_with_state(
            state,
            corrupt_response,
        ));
    (_, f.task) = e2ee_http::serve(app, f.origin.strip_prefix("http://").unwrap()).await;
}

#[tokio::test]
async fn reordered_mapping_never_partially_accepts() {
    let mut f = fixture().await;
    converge(&f).await;
    edits(&f, 3).await;
    let (inputs, records) = frozen(&f).await;
    let before_high = server_rows(&f.server).await.high_water;
    instrument_fault(&mut f, ResponseFault::Reordered).await;
    let client = Client::new(&f.origin).unwrap();
    assert!(
        client
            .round(&f.peer_store, &f.peer, &blobs(&f.peer))
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&f.peer, "SELECT count(*) FROM local_e2ee_outbox").await,
        records.len() as i64
    );
    assert_eq!(
        server_rows(&f.server).await.high_water,
        before_high + records.len() as i64
    );
    assert_eq!(
        f.peer
            .encrypted_tail_frozen_records(&inputs.authority)
            .await
            .unwrap(),
        records
    );
    drain(&client, &f.peer_store, &f.peer).await;
    assert_eq!(
        scalar(&f.peer, "SELECT count(*) FROM local_e2ee_outbox").await,
        0
    );
    assert_eq!(server_rows(&f.server).await.high_water, before_high + 3);
}

#[tokio::test]
async fn observed_operation_cannot_be_reconciled_from_a_later_absent_response() {
    let mut f = fixture().await;
    converge(&f).await;
    let accepted_before = scalar(&f.peer, "SELECT count(*) FROM local_e2ee_accepted").await;
    edits(&f, 3).await;
    let (inputs, records) = frozen(&f).await;
    let BatchReply::Appended(mappings) = append(&f, &inputs, &records).await.unwrap() else {
        panic!("expected append mappings");
    };
    f.peer
        .observe_encrypted_tail(&inputs.authority, &mappings[0].clone().into())
        .await
        .unwrap();
    let before = server_rows(&f.server).await;
    instrument_fault(&mut f, ResponseFault::ResolvedAbsent).await;
    let client = Client::new(&f.origin).unwrap();
    let error = client
        .round(&f.peer_store, &f.peer, &blobs(&f.peer))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("encrypted-tail-invalid"));
    assert_eq!(server_rows(&f.server).await, before);
    let observed: Vec<(String, Option<i64>, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT operation_id,observed_sequence,observed_commitment FROM local_e2ee_outbox ORDER BY position",
    )
    .fetch_all(&mut *aven_core::test_support::acquire(&f.peer).await.unwrap())
    .await
    .unwrap();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].0, records[0].0);
    assert_eq!(observed[0].1, Some(mappings[0].sequence));
    assert_eq!(
        observed[0].2.as_deref(),
        Some(mappings[0].commitment.as_slice())
    );
    assert_eq!(
        scalar(&f.peer, "SELECT count(*) FROM local_e2ee_accepted").await,
        accepted_before + 2
    );
    for mapping in &mappings[1..] {
        let accepted: Option<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT sequence,commitment FROM local_e2ee_accepted WHERE operation_id=?",
        )
        .bind(&mapping.operation_id)
        .fetch_optional(&mut *aven_core::test_support::acquire(&f.peer).await.unwrap())
        .await
        .unwrap();
        assert_eq!(
            accepted,
            Some((mapping.sequence, mapping.commitment.to_vec()))
        );
    }
    assert_eq!(
        scalar(&f.peer, "SELECT count(*) FROM local_e2ee_outbox").await,
        1
    );
}
