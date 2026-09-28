use super::*;
use aven_core::sync::encrypted_tail::{BatchOperation, BatchRecord, BatchReply};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

mod adversarial;
mod compatibility;

#[derive(Default)]
struct Traffic {
    legacy: bool,
    lose: AtomicBool,
    fail_lookup: AtomicUsize,
    feature_refusal: Option<(StatusCode, &'static str)>,
    requests: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}
async fn intercept(
    State(state): State<Arc<Traffic>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, tail::BATCH_APPEND_LIMIT).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let path = parts.uri.path().to_owned();
    state
        .requests
        .lock()
        .unwrap()
        .push((path.clone(), value.clone()));
    if value["operation"].get("Lookup").is_some()
        && state
            .fail_lookup
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .ok()
            == Some(1)
    {
        return http_admission::refusal(StatusCode::REQUEST_TIMEOUT, "encrypted-tail-timeout");
    }
    if value["operation"] == "Features" {
        if let Some((status, code)) = state.feature_refusal {
            return http_admission::refusal(status, code);
        }
        if state.legacy {
            return http_admission::refusal(StatusCode::BAD_REQUEST, "encrypted-tail-malformed");
        }
    }
    if state.legacy && path == aven_core::sync::client::tail::BATCH_PATH {
        panic!("legacy server received a batch request");
    }
    let append = path == aven_core::sync::client::tail::BATCH_PATH
        && value["operation"].get("Append").is_some();
    let response = next
        .run(Request::from_parts(parts, axum::body::Body::from(bytes)))
        .await;
    if append && state.lose.swap(false, Ordering::SeqCst) && response.status() == StatusCode::OK {
        // A gateway can lose the reply after commit; Retry-After is not absence evidence.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "0")],
            "gateway lost reply",
        )
            .into_response();
    }
    response
}
async fn instrument(f: &mut Fixture, state: Arc<Traffic>) {
    f.task.abort();
    let _ = (&mut f.task).await;
    let app = crate::peer_enrollment_http::router(f.server.clone())
        .merge(router(f.server.clone(), Default::default()))
        .layer(axum::middleware::from_fn_with_state(state, intercept));
    (_, f.task) = e2ee_http::serve(app, f.origin.strip_prefix("http://").unwrap()).await;
}
async fn edits(f: &Fixture, count: usize) {
    let w = f.peer.list_workspaces().await.unwrap().remove(0);
    for n in 0..count {
        f.peer
            .create_task(&w, draft(&format!("batch task {n}")))
            .await
            .unwrap();
    }
}
async fn frozen(f: &Fixture) -> (TailSnapshot, Vec<(String, Vec<u8>)>) {
    let inputs = f.peer_store.tail_inputs(&f.peer, &f.origin).await.unwrap();
    f.peer
        .prepare_encrypted_batch_in_run(&inputs.authority, &blobs(&f.peer), None, tail::BATCH_COUNT)
        .await
        .unwrap();
    let records = f
        .peer
        .encrypted_tail_frozen_records(&inputs.authority)
        .await
        .unwrap();
    (inputs, records)
}
async fn append(
    f: &Fixture,
    inputs: &TailSnapshot,
    records: &[(String, Vec<u8>)],
) -> Result<BatchReply> {
    f.server
        .encrypted_tail_batch_exchange(
            &inputs.authority.context,
            &inputs.bearer,
            BatchOperation::Append {
                records: records
                    .iter()
                    .map(|(_, r)| BatchRecord(r.clone()))
                    .collect(),
            },
        )
        .await
}

#[tokio::test]
async fn negotiated_batches_and_legacy_fallback_keep_single_append_working() {
    for legacy in [false, true] {
        let mut f = fixture().await;
        converge(&f).await;
        let state = Arc::new(Traffic {
            legacy,
            ..Default::default()
        });
        instrument(&mut f, state.clone()).await;
        edits(&f, tail::BATCH_COUNT + 3).await;
        drain(&Client::new(&f.origin).unwrap(), &f.peer_store, &f.peer).await;
        let traffic = state.requests.lock().unwrap();
        assert_eq!(
            traffic
                .iter()
                .filter(|(_, v)| v["operation"] == "Features")
                .count(),
            1
        );
        let batches = traffic
            .iter()
            .filter(|(p, v)| {
                p == aven_core::sync::client::tail::BATCH_PATH
                    && v["operation"].get("Append").is_some()
            })
            .count();
        assert_eq!(batches, if legacy { 0 } else { 2 });
        let singles = traffic
            .iter()
            .filter(|(p, v)| p == PATH && v["operation"].get("Append").is_some())
            .count();
        assert_eq!(singles, if legacy { tail::BATCH_COUNT + 3 } else { 0 });
        assert_eq!(
            scalar(&f.peer, "SELECT count(*) FROM local_e2ee_outbox").await,
            0
        );
    }
}

#[tokio::test]
async fn invalid_last_record_and_known_id_roll_back_entire_batch() {
    let f = fixture().await;
    converge(&f).await;
    edits(&f, 3).await;
    let (inputs, records) = frozen(&f).await;
    let before = scalar(&f.server, "SELECT high_water FROM server_e2ee_allocator").await;
    let parents = scalar(&f.server, "SELECT count(*) FROM server_e2ee_image_parents").await;
    let mut invalid = records.clone();
    invalid[2].1 = vec![0];
    assert!(append(&f, &inputs, &invalid).await.is_err());
    let mut duplicate = records.clone();
    duplicate[2] = duplicate[0].clone();
    assert!(append(&f, &inputs, &duplicate).await.is_err());
    assert_eq!(
        scalar(&f.server, "SELECT high_water FROM server_e2ee_allocator").await,
        before
    );
    assert_eq!(
        scalar(&f.server, "SELECT count(*) FROM server_e2ee_image_parents").await,
        parents
    );
    // Failure after earlier inserts and side effects must roll the transaction back too.
    {
        let mut conn = aven_core::test_support::acquire(&f.server).await.unwrap();
        sqlx::query("INSERT INTO meta(key,value) VALUES ('batch_failure_id', ?)")
            .bind(&records[2].0)
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER fail_batch BEFORE INSERT ON server_e2ee_tail WHEN NEW.operation_id=(SELECT value FROM meta WHERE key='batch_failure_id') BEGIN SELECT RAISE(ABORT, 'injected'); END").execute(&mut *conn).await.unwrap();
    }
    assert!(append(&f, &inputs, &records).await.is_err());
    assert_eq!(
        scalar(&f.server, "SELECT high_water FROM server_e2ee_allocator").await,
        before
    );
    assert_eq!(
        scalar(&f.server, "SELECT count(*) FROM server_e2ee_image_parents").await,
        parents
    );
    {
        let mut conn = aven_core::test_support::acquire(&f.server).await.unwrap();
        sqlx::query("DROP TRIGGER fail_batch")
            .execute(&mut *conn)
            .await
            .unwrap();
    }
    f.server
        .encrypted_tail_exchange(
            &inputs.authority.context,
            &inputs.bearer,
            Operation::Append {
                record: records[1].1.clone(),
                ticket: None,
            },
        )
        .await
        .unwrap();
    let error = append(&f, &inputs, &records).await.err().unwrap();
    assert!(error.to_string().contains("encrypted-tail-batch-known"));
    assert_eq!(
        scalar(&f.server, "SELECT high_water FROM server_e2ee_allocator").await,
        before + 1
    );
    drain(&Client::new(&f.origin).unwrap(), &f.peer_store, &f.peer).await;
    assert_eq!(
        scalar(&f.server, "SELECT high_water FROM server_e2ee_allocator").await,
        before + 3
    );
}

#[tokio::test]
async fn lost_batch_response_resolves_every_id_without_resending() {
    let mut f = fixture().await;
    converge(&f).await;
    edits(&f, 5).await;
    let state = Arc::new(Traffic {
        lose: AtomicBool::new(true),
        ..Default::default()
    });
    instrument(&mut f, state.clone()).await;
    let client = Client::new(&f.origin).unwrap();
    assert!(
        client
            .round(&f.peer_store, &f.peer, &blobs(&f.peer))
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&f.peer, "SELECT count(*) FROM local_e2ee_outbox").await,
        5
    );
    let reopened = Database::open(f.peer.path()).await.unwrap();
    drain(&client, &f.peer_store, &reopened).await;
    let requests = state.requests.lock().unwrap();
    let batches: Vec<_> = requests
        .iter()
        .filter(|(p, _)| p == aven_core::sync::client::tail::BATCH_PATH)
        .collect();
    assert_eq!(batches.len(), 2);
    assert!(batches[0].1["operation"].get("Append").is_some());
    assert_eq!(
        batches[1].1["operation"]["Resolve"]["operation_ids"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    assert_eq!(
        scalar(&reopened, "SELECT count(*) FROM local_e2ee_outbox").await,
        0
    );
}

#[tokio::test]
async fn process_crashes_preserve_batch_freeze_acceptance_and_partial_observation() {
    for stage in [
        "batch-frozen",
        "after-batch-append",
        "before-batch-commit",
        "after-batch-commit",
        "batch-observed",
    ] {
        let f = fixture().await;
        converge(&f).await;
        edits(&f, 3).await;
        if stage == "batch-observed" {
            let (inputs, records) = frozen(&f).await;
            append(&f, &inputs, &records).await.unwrap();
        }
        let output = e2ee_http::worker("encrypted_tail_http::tests::process_worker")
            .env("AVEN_TAIL_ROOT", f.root.path())
            .env("AVEN_TAIL_ORIGIN", &f.origin)
            .env("AVEN_TAIL_CRASH", stage)
            .output()
            .await
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(84),
            "{stage}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if stage == "batch-observed" {
            assert_eq!(
                scalar(
                    &f.peer,
                    "SELECT count(*) FROM local_e2ee_outbox WHERE observed_sequence IS NOT NULL"
                )
                .await,
                1
            );
        }
        let reopened = Database::open(f.peer.path()).await.unwrap();
        let store = isolated_store(&reopened, &f.root.path().join("peer-keys")).await;
        drain(&Client::new(&f.origin).unwrap(), &store, &reopened).await;
        assert_eq!(
            scalar(
                &reopened,
                "SELECT count(*) FROM changes WHERE server_seq IS NULL"
            )
            .await,
            0
        );
        assert_eq!(
            scalar(&reopened, "SELECT count(*) FROM local_e2ee_outbox").await,
            0
        );
    }
}

#[test]
fn compact_batch_limits_cover_maximal_framing_without_changing_singletons() {
    let context = Context {
        vault: [255; 32],
        genesis: [255; 32],
        device: [255; 32],
        head: [255; 32],
        stream: [255; 32],
        descriptor: [255; 32],
    };
    let records = (0..tail::BATCH_COUNT)
        .map(|_| BatchRecord(vec![255; tail::BATCH_BYTES / tail::BATCH_COUNT]))
        .collect();
    let bytes = serde_json::to_vec(&tail::batch::Envelope {
        context: context.clone(),
        correlation: [255; 32],
        operation: BatchOperation::Append { records },
    })
    .unwrap();
    assert!(bytes.len() <= tail::BATCH_APPEND_LIMIT);
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(value["context"]["vault"].is_string());
    let mappings = (0..tail::BATCH_COUNT)
        .map(|_| tail::batch::CompactMapping {
            operation_id: "\u{1}".repeat(256),
            sequence: i64::MAX,
            commitment: [255; 32],
        })
        .collect();
    let response = serde_json::to_vec(&tail::batch::Envelope {
        context,
        correlation: [255; 32],
        operation: BatchReply::Appended(mappings),
    })
    .unwrap();
    assert!(response.len() <= tail::BATCH_CONTROL_LIMIT);
    assert!(
        serde_json::from_value::<BatchOperation>(
            serde_json::json!({"Append":{"records": vec![""; tail::BATCH_COUNT+1]}})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<BatchOperation>(
            serde_json::json!({"Resolve":{"operation_ids": vec!["id"; tail::BATCH_COUNT+1]}})
        )
        .is_err()
    );
}

#[tokio::test]
async fn downgrade_reconciles_the_whole_group_before_any_single_resend() {
    let mut f = fixture().await;
    converge(&f).await;
    edits(&f, 4).await;
    let (_, original) = frozen(&f).await;
    let state = Arc::new(Traffic {
        legacy: true,
        fail_lookup: AtomicUsize::new(4),
        ..Default::default()
    });
    instrument(&mut f, state.clone()).await;
    let client = Client::new(&f.origin).unwrap();
    assert!(
        client
            .round(&f.peer_store, &f.peer, &blobs(&f.peer))
            .await
            .is_err()
    );
    {
        let requests = state.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|(_, v)| v["operation"].get("Lookup").is_some())
                .count(),
            4
        );
        assert!(
            !requests
                .iter()
                .any(|(_, v)| v["operation"].get("Append").is_some())
        );
    }
    let inputs = f.peer_store.tail_inputs(&f.peer, &f.origin).await.unwrap();
    assert_eq!(
        f.peer
            .encrypted_tail_frozen_records(&inputs.authority)
            .await
            .unwrap(),
        original
    );
    state.requests.lock().unwrap().clear();
    drain(&client, &f.peer_store, &f.peer).await;
    let requests = state.requests.lock().unwrap();
    let first_append = requests
        .iter()
        .position(|(_, v)| v["operation"].get("Append").is_some())
        .unwrap();
    assert_eq!(
        requests[..first_append]
            .iter()
            .filter(|(_, v)| v["operation"].get("Lookup").is_some())
            .count(),
        4
    );
}

#[tokio::test]
async fn frozen_batch_stops_at_record_and_byte_budgets_without_absorbing_new_work() {
    let f = fixture().await;
    converge(&f).await;
    edits(&f, 20).await;
    let w = f.peer.list_workspaces().await.unwrap().remove(0);
    let mut conn = aven_core::test_support::acquire(&f.peer).await.unwrap();
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT entity_id FROM changes WHERE server_seq IS NULL ORDER BY local_seq",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    // Large descriptions create a byte-limited group well below the count ceiling.
    for id in ids {
        f.peer
            .update_task(
                &w,
                &id.parse().unwrap(),
                TaskUpdate {
                    description: Some("x".repeat(60_000)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let (inputs, records) = frozen(&f).await;
    assert!(records.len() < 40 && records.len() > 20);
    assert!(records.iter().map(|(_, r)| r.len()).sum::<usize>() <= tail::BATCH_BYTES);
    edits(&f, 1).await;
    f.peer
        .prepare_encrypted_batch_in_run(&inputs.authority, &blobs(&f.peer), None, tail::BATCH_COUNT)
        .await
        .unwrap();
    assert_eq!(
        f.peer
            .encrypted_tail_frozen_records(&inputs.authority)
            .await
            .unwrap(),
        records
    );
}

#[tokio::test]
async fn transport_disconnect_after_batch_acceptance_requires_resolution() {
    use aven_core::sync::client::{Session, Step};
    let f = fixture().await;
    converge(&f).await;
    edits(&f, 3).await;
    let driver = crate::sync_http::HttpDriver::new().unwrap();
    let mut session = Session::new(|link| async {
        aven_core::sync::client::tail::Client::new(&f.origin, link)?
            .round(&f.peer_store, &f.peer, &blobs(&f.peer))
            .await
    });
    let mut interrupted = false;
    loop {
        match session.next().await {
            Ok(Step::Request(request))
                if request
                    .url
                    .ends_with(aven_core::sync::client::tail::BATCH_PATH) =>
            {
                assert!(!interrupted);
                let mut outgoing = driver.http.post(request.url.as_str()).body(request.body);
                for header in request.headers {
                    outgoing = outgoing.header(header.name, header.value);
                }
                let response = outgoing.send().await.unwrap();
                assert_eq!(response.status(), reqwest::StatusCode::OK);
                response.bytes().await.unwrap();
                session.register_transport_failure(request.context).unwrap();
                interrupted = true;
            }
            Ok(Step::Request(request)) => driver.answer(&mut session, request).await.unwrap(),
            Ok(Step::Wait(delay)) => tokio::time::sleep(delay).await,
            Err(error) => {
                assert!(interrupted && error.to_string().contains("outcome-unknown"));
                break;
            }
            Ok(Step::Done(_)) => panic!("disconnect must leave an unknown outcome"),
        }
    }
    drop(session);
    assert_eq!(
        scalar(&f.peer, "SELECT count(*) FROM local_e2ee_outbox").await,
        3
    );
    let high = scalar(&f.server, "SELECT high_water FROM server_e2ee_allocator").await;
    drain(&Client::new(&f.origin).unwrap(), &f.peer_store, &f.peer).await;
    assert_eq!(
        scalar(&f.server, "SELECT high_water FROM server_e2ee_allocator").await,
        high
    );
}
