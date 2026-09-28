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

async fn post_batch_body(f: &Fixture, inputs: &TailSnapshot, body: Vec<u8>) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}{}",
            f.origin,
            aven_core::sync::client::tail::BATCH_PATH
        ))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .bearer_auth(hex::encode(inputs.bearer.expose()))
        .body(body)
        .send()
        .await
        .unwrap()
}

fn append_body(inputs: &TailSnapshot, records: &[(String, Vec<u8>)]) -> Vec<u8> {
    serde_json::to_vec(&tail::batch::Envelope {
        context: inputs.authority.context.clone(),
        correlation: [7; 32],
        operation: BatchOperation::Append {
            records: records
                .iter()
                .map(|(_, record)| BatchRecord(record.clone()))
                .collect(),
        },
    })
    .unwrap()
}

fn resize_body(mut body: Vec<u8>, limit: usize) -> Vec<u8> {
    assert!(
        body.len() <= limit,
        "body exceeds configured limit before padding"
    );
    body.resize(limit, b' ');
    body
}

#[tokio::test]
async fn http_and_decoded_append_bounds_accept_exact_limit_and_reject_limit_plus_one() {
    // Record framing and encryption overhead are fixed for these records. A
    // calibration group lets descriptions fill the decoded budget exactly.
    let calibration = fixture().await;
    edits(&calibration, tail::BATCH_COUNT).await;
    let (_, base_records) = frozen(&calibration).await;
    assert_eq!(base_records.len(), tail::BATCH_COUNT);
    let decoded_base = base_records
        .iter()
        .map(|(_, record)| record.len())
        .sum::<usize>();
    let mut extra = tail::BATCH_BYTES - decoded_base;
    let descriptions = (0..tail::BATCH_COUNT)
        .map(|index| {
            let left = tail::BATCH_COUNT - index;
            let size = extra / left;
            extra -= size;
            size
        })
        .collect::<Vec<_>>();
    drop(calibration);

    let f = fixture().await;
    let workspace = f.peer.list_workspaces().await.unwrap().remove(0);
    for (index, description_len) in descriptions.into_iter().enumerate() {
        let mut task = draft(&format!("batch task {index}"));
        task.description = "x".repeat(description_len);
        f.peer.create_task(&workspace, task).await.unwrap();
    }
    let (inputs, records) = frozen(&f).await;
    assert_eq!(records.len(), tail::BATCH_COUNT);
    let decoded = records
        .iter()
        .map(|(_, record)| record.len())
        .sum::<usize>();
    assert_eq!(decoded, tail::BATCH_BYTES);
    assert!(
        records
            .iter()
            .all(|(_, record)| record.len() <= tail::RECORD_LIMIT)
    );

    // The serialized body ceiling has framing slack. JSON whitespace reaches
    // its exact edge without changing the decoded operation.
    let exact = resize_body(append_body(&inputs, &records), tail::BATCH_APPEND_LIMIT);
    let before = server_rows(&f.server).await;
    let mut too_large = exact.clone();
    too_large.push(b' ');
    assert!(
        !post_batch_body(&f, &inputs, too_large)
            .await
            .status()
            .is_success()
    );
    assert_eq!(server_rows(&f.server).await, before);

    // One decoded byte over the limit is refused even when the serialized body
    // itself is exactly at the allowed HTTP size.
    let mut over_decoded = records.clone();
    over_decoded.last_mut().unwrap().1.push(0);
    assert_eq!(
        over_decoded
            .iter()
            .map(|(_, record)| record.len())
            .sum::<usize>(),
        tail::BATCH_BYTES + 1
    );
    let over_body = resize_body(
        append_body(&inputs, &over_decoded),
        tail::BATCH_APPEND_LIMIT,
    );
    assert!(
        !post_batch_body(&f, &inputs, over_body)
            .await
            .status()
            .is_success()
    );
    assert_eq!(server_rows(&f.server).await, before);

    let response = post_batch_body(&f, &inputs, exact).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let reply: tail::batch::Envelope<BatchReply> =
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    let BatchReply::Appended(mappings) = reply.operation else {
        panic!("expected append mappings");
    };
    assert_eq!(mappings.len(), tail::BATCH_COUNT);
    let after = server_rows(&f.server).await;
    assert_eq!(
        after.high_water - before.high_water,
        tail::BATCH_COUNT as i64
    );
    assert_eq!(after.tail.len() - before.tail.len(), tail::BATCH_COUNT);

    // The custom visitor rejects the 129th tiny item before a caller can use
    // an unbounded collection. Both request variants share this count ceiling.
    assert!(
        serde_json::from_value::<BatchOperation>(serde_json::json!({
            "Append": { "records": vec![""; tail::BATCH_COUNT + 1] }
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<BatchOperation>(serde_json::json!({
            "Resolve": { "operation_ids": vec!["id"; tail::BATCH_COUNT + 1] }
        }))
        .is_err()
    );
}

#[tokio::test]
async fn resolve_http_control_limit_is_exact_and_limit_plus_one_is_refused() {
    let f = fixture().await;
    let inputs = f.peer_store.tail_inputs(&f.peer, &f.origin).await.unwrap();
    let operation_ids = (0..tail::BATCH_COUNT)
        .map(|index| format!("{index:03}{}", "\u{1}".repeat(253)))
        .collect::<Vec<_>>();
    assert!(operation_ids.iter().all(|id| id.len() == 256));
    let body = serde_json::to_vec(&tail::batch::Envelope {
        context: inputs.authority.context.clone(),
        correlation: [8; 32],
        operation: BatchOperation::Resolve {
            operation_ids: operation_ids.clone(),
        },
    })
    .unwrap();
    let exact = resize_body(body, tail::BATCH_CONTROL_LIMIT);
    let mut over = exact.clone();
    over.push(b' ');
    assert!(
        !post_batch_body(&f, &inputs, over)
            .await
            .status()
            .is_success()
    );
    let response = post_batch_body(&f, &inputs, exact).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let bytes = response.bytes().await.unwrap();
    assert!(bytes.len() <= tail::BATCH_CONTROL_LIMIT);
    let reply: tail::batch::Envelope<BatchReply> = serde_json::from_slice(&bytes).unwrap();
    let BatchReply::Resolved(resolutions) = reply.operation else {
        panic!("expected resolve mappings");
    };
    assert_eq!(resolutions.len(), tail::BATCH_COUNT);
}

fn replace_header_id(record: &mut [u8], new_id: &str) {
    let header_len = u32::from_be_bytes(record[..4].try_into().unwrap()) as usize;
    assert!(header_len + 4 <= record.len());
    let mut cursor = 4 + 8;
    for _ in 0..4 {
        let size = u32::from_be_bytes(record[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4 + size;
    }
    let id_len = u32::from_be_bytes(record[cursor..cursor + 4].try_into().unwrap()) as usize;
    assert_eq!(id_len, new_id.len());
    record[cursor + 4..cursor + 4 + id_len].copy_from_slice(new_id.as_bytes());
}

fn replace_generation(record: &mut [u8], generation: [u8; 32]) {
    let header_len = u32::from_be_bytes(record[..4].try_into().unwrap()) as usize;
    assert!(header_len + 4 <= record.len());
    let mut cursor = 4 + 8;
    for _ in 0..2 {
        let size = u32::from_be_bytes(record[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4 + size;
    }
    let generation_len = u32::from_be_bytes(record[cursor..cursor + 4].try_into().unwrap());
    assert_eq!(generation_len, 32);
    cursor += 4;
    record[cursor..cursor + 32].copy_from_slice(&generation);
}

#[tokio::test]
async fn prevalidation_and_allocator_failures_leave_all_server_rows_unchanged() {
    let f = fixture().await;
    converge(&f).await;
    edits(&f, 3).await;
    let (inputs, records) = frozen(&f).await;
    assert_eq!(records.len(), 3);

    let before = server_rows(&f.server).await;
    let mut invalid_last = records.clone();
    invalid_last[2].1 = vec![0];
    assert!(append(&f, &inputs, &invalid_last).await.is_err());
    assert_eq!(server_rows(&f.server).await, before);

    let mut wrong_generation = records.clone();
    replace_generation(&mut wrong_generation[2].1, [0xa5; 32]);
    assert!(append(&f, &inputs, &wrong_generation).await.is_err());
    assert_eq!(server_rows(&f.server).await, before);

    let prefix_id: String = sqlx::query_scalar(
        "SELECT operation_id FROM server_bootstrap_prefix ORDER BY rank LIMIT 1",
    )
    .fetch_one(&mut *aven_core::test_support::acquire(&f.server).await.unwrap())
    .await
    .unwrap();
    let mut prefix = records.clone();
    replace_header_id(&mut prefix[2].1, &prefix_id);
    let error = append(&f, &inputs, &prefix)
        .await
        .err()
        .expect("prefix identity must be refused");
    assert!(error.to_string().contains("prefix-identity-collision"));
    assert_eq!(server_rows(&f.server).await, before);

    // Existing identity on the final member is discovered before any earlier
    // member is inserted or its parent projection is updated.
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

    // Exhaustion is checked before the first insert; allocator state itself is
    // preserved at its maximum value and projection tables remain byte-exact.
    let mut conn = aven_core::test_support::acquire(&f.server).await.unwrap();
    sqlx::query("UPDATE server_e2ee_allocator SET high_water=? WHERE singleton=1")
        .bind(i64::MAX)
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let exhausted = server_rows(&f.server).await;
    assert_eq!(exhausted.high_water, i64::MAX);
    assert!(append(&f, &inputs, &records[..2]).await.is_err());
    assert_eq!(server_rows(&f.server).await, exhausted);
    let mut conn = aven_core::test_support::acquire(&f.server).await.unwrap();
    sqlx::query("UPDATE server_e2ee_allocator SET high_water=? WHERE singleton=1")
        .bind(after_single.high_water)
        .execute(&mut *conn)
        .await
        .unwrap();
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
    append(&f, &inputs, &records).await.unwrap();
    assert_ne!(
        server_rows(&f.server).await.projection.parents,
        before.projection.parents
    );
}

#[derive(Clone, Copy)]
enum ResponseFault {
    Malformed,
    Reordered,
    Duplicate,
    Truncated,
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
        (false, n) if n == ResponseFault::Malformed as usize + 1 => ResponseFault::Malformed,
        (false, n) if n == ResponseFault::Reordered as usize + 1 => ResponseFault::Reordered,
        (false, n) if n == ResponseFault::Duplicate as usize + 1 => ResponseFault::Duplicate,
        (false, n) if n == ResponseFault::Truncated as usize + 1 => ResponseFault::Truncated,
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
        let mappings = value["operation"]["Appended"].as_array_mut().unwrap();
        match fault {
            ResponseFault::Malformed => mappings[0]["commitment"] = Value::Null,
            ResponseFault::Reordered => mappings.swap(0, 1),
            ResponseFault::Duplicate => mappings[1] = mappings[0].clone(),
            ResponseFault::Truncated => {
                mappings.pop();
            }
            ResponseFault::ResolvedAbsent => unreachable!(),
        }
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
async fn malformed_reordered_duplicate_and_truncated_mappings_never_partially_accept() {
    for fault in [
        ResponseFault::Malformed,
        ResponseFault::Reordered,
        ResponseFault::Duplicate,
        ResponseFault::Truncated,
    ] {
        let mut f = fixture().await;
        converge(&f).await;
        edits(&f, 3).await;
        let (inputs, records) = frozen(&f).await;
        let before_high = server_rows(&f.server).await.high_water;
        instrument_fault(&mut f, fault).await;
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
        let server_ids: Vec<String> = sqlx::query_scalar(
            "SELECT operation_id FROM server_e2ee_tail ORDER BY sequence DESC LIMIT 3",
        )
        .fetch_all(&mut *aven_core::test_support::acquire(&f.server).await.unwrap())
        .await
        .unwrap();
        assert_eq!(server_ids.len(), records.len());
        assert!(records.iter().all(|(id, _)| server_ids.contains(id)));
        let frozen_after = f
            .peer
            .encrypted_tail_frozen_records(&inputs.authority)
            .await
            .unwrap();
        assert_eq!(frozen_after, records);
        drain(&client, &f.peer_store, &f.peer).await;
        assert_eq!(
            scalar(&f.peer, "SELECT count(*) FROM local_e2ee_outbox").await,
            0
        );
        assert_eq!(server_rows(&f.server).await.high_water, before_high + 3);
    }
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

async fn create_device_tasks(db: &Database, prefix: &str, count: usize) -> Vec<String> {
    let workspace = db.list_workspaces().await.unwrap().remove(0);
    let mut titles = Vec::with_capacity(count);
    for index in 0..count {
        let name = format!("{prefix}-{index}");
        db.create_task(&workspace, draft(&name)).await.unwrap();
        titles.push(name);
    }
    titles
}

async fn frozen_for(
    store: &ProtectedLocalKeyStore,
    db: &Database,
    origin: &str,
) -> (TailSnapshot, Vec<(String, Vec<u8>)>) {
    let inputs = store.tail_inputs(db, origin).await.unwrap();
    db.prepare_encrypted_batch_in_run(&inputs.authority, &blobs(db), None, tail::BATCH_COUNT)
        .await
        .unwrap();
    let records = db
        .encrypted_tail_frozen_records(&inputs.authority)
        .await
        .unwrap();
    (inputs, records)
}

#[tokio::test]
async fn overlapping_device_batches_and_concurrent_lost_replies_converge_without_identity_damage() {
    let mut f = fixture().await;
    converge(&f).await;
    let initial_rows = server_rows(&f.server).await;
    let before_ids: Vec<String> = initial_rows.tail.iter().map(|row| row.0.clone()).collect();

    create_device_tasks(&f.seed, "overlap-seed", 2).await;
    create_device_tasks(&f.peer, "overlap-peer", 2).await;
    let (seed_inputs, seed_records) = frozen_for(&f.seed_store, &f.seed, &f.origin).await;
    let (peer_inputs, peer_records) = frozen_for(&f.peer_store, &f.peer, &f.origin).await;
    assert_eq!(seed_records.len(), 2);
    assert_eq!(peer_records.len(), 2);
    let seed_batch = vec![seed_records[0].clone(), seed_records[1].clone()];
    let peer_batch = vec![seed_records[0].clone(), peer_records[0].clone()];
    let (seed_result, peer_result) = tokio::join!(
        append(&f, &seed_inputs, &seed_batch),
        append(&f, &peer_inputs, &peer_batch),
    );
    assert_ne!(seed_result.is_ok(), peer_result.is_ok());
    assert!(seed_result.is_err() || peer_result.is_err());
    assert_eq!(
        server_rows(&f.server).await.high_water,
        initial_rows.high_water + 2
    );

    let seed_client = Client::new(&f.origin).unwrap();
    let peer_client = Client::new(&f.origin).unwrap();
    let ((), ()) = tokio::join!(
        drain(&seed_client, &f.seed_store, &f.seed),
        drain(&peer_client, &f.peer_store, &f.peer),
    );
    drain(&seed_client, &f.seed_store, &f.seed).await;
    drain(&peer_client, &f.peer_store, &f.peer).await;

    create_device_tasks(&f.seed, "fault-seed", 3).await;
    create_device_tasks(&f.peer, "fault-peer", 3).await;
    let (_, seed_fault_records) = frozen_for(&f.seed_store, &f.seed, &f.origin).await;
    let (_, peer_fault_records) = frozen_for(&f.peer_store, &f.peer, &f.origin).await;
    let concurrent_records = seed_fault_records
        .iter()
        .chain(peer_fault_records.iter())
        .cloned()
        .collect::<Vec<_>>();
    let concurrent_ids = concurrent_records
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    assert_eq!(concurrent_ids.len(), 6);
    let before_fault = server_rows(&f.server).await;
    let state = Arc::new(Traffic {
        lose: std::sync::atomic::AtomicBool::new(true),
        ..Default::default()
    });
    instrument(&mut f, state.clone()).await;
    let seed_client = Client::new(&f.origin).unwrap();
    let peer_client = Client::new(&f.origin).unwrap();
    let seed_blobs = blobs(&f.seed);
    let peer_blobs = blobs(&f.peer);
    let (seed_round, peer_round) = tokio::join!(
        seed_client.round(&f.seed_store, &f.seed, &seed_blobs),
        peer_client.round(&f.peer_store, &f.peer, &peer_blobs),
    );
    assert_ne!(seed_round.is_err(), peer_round.is_err());
    assert_eq!(
        server_rows(&f.server).await.high_water,
        before_fault.high_water + 6
    );

    let ((), ()) = tokio::join!(
        drain(&seed_client, &f.seed_store, &f.seed),
        drain(&peer_client, &f.peer_store, &f.peer),
    );
    drain(&seed_client, &f.seed_store, &f.seed).await;
    drain(&peer_client, &f.peer_store, &f.peer).await;
    assert_quiescent(&[&f.seed, &f.peer]).await;

    let final_rows = server_rows(&f.server).await;
    assert_eq!(final_rows.high_water, initial_rows.high_water + 10);
    assert_eq!(final_rows.tail.len(), initial_rows.tail.len() + 10);
    let mut expected_records: std::collections::HashMap<String, Vec<u8>> = seed_records
        .iter()
        .chain(peer_records.iter())
        .chain(concurrent_records.iter())
        .map(|(id, record)| (id.clone(), record.clone()))
        .collect();
    assert_eq!(expected_records.len(), 10);
    let actual_ids: Vec<String> = final_rows.tail.iter().map(|row| row.0.clone()).collect();
    for id in expected_records.keys() {
        assert!(actual_ids.contains(id), "missing server operation {id}");
    }
    for id in before_ids {
        assert!(
            actual_ids.contains(&id),
            "existing operation disappeared: {id}"
        );
    }
    use sha2::{Digest, Sha256};
    for (id, _, commitment, record) in &final_rows.tail {
        if let Some(expected_record) = expected_records.remove(id) {
            assert_eq!(record, &expected_record, "ciphertext changed for {id}");
            assert_eq!(commitment, Sha256::digest(record).as_slice());
        }
    }
    assert!(expected_records.is_empty());
    let sequences: Vec<i64> = final_rows.tail.iter().map(|row| row.1).collect();
    let unique_sequences: std::collections::HashSet<i64> = sequences.iter().copied().collect();
    assert_eq!(unique_sequences.len(), sequences.len());
    assert_eq!(sequences.iter().max().copied(), Some(final_rows.high_water));
    for title in [
        "overlap-seed-0",
        "overlap-seed-1",
        "overlap-peer-0",
        "overlap-peer-1",
        "fault-seed-0",
        "fault-seed-1",
        "fault-seed-2",
        "fault-peer-0",
        "fault-peer-1",
        "fault-peer-2",
    ] {
        let seed_has: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE title=?)")
            .bind(title)
            .fetch_one(&mut *aven_core::test_support::acquire(&f.seed).await.unwrap())
            .await
            .unwrap();
        let peer_has: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE title=?)")
            .bind(title)
            .fetch_one(&mut *aven_core::test_support::acquire(&f.peer).await.unwrap())
            .await
            .unwrap();
        assert!(
            seed_has && peer_has,
            "task did not converge on both devices: {title}"
        );
    }
}
