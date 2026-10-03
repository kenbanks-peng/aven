use super::*;
use crate::test_support::e2ee_http::protected_state::ProtectedState;

struct Splice {
    delivered: AtomicUsize,
    record: (String, Vec<u8>),
}

/// Replaces the first record of every successful Page reply with `record`,
/// recomputing the relay-controlled commitment so only authentication refuses it.
async fn splice_page(
    State(attack): State<Arc<Splice>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let response = next.run(request).await;
    if response.status() != StatusCode::OK {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let bytes = to_bytes(body, tail::RESPONSE_LIMIT).await.unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    if value["operation"].get("Page").is_some() {
        let mut envelope: Envelope<Reply> = serde_json::from_value(value.clone()).unwrap();
        let Reply::Page(ref mut page) = envelope.operation else {
            unreachable!()
        };
        assert!(!page.records.is_empty(), "attack must deliver a record");
        let accepted = &mut page.records[0];
        let (id, record) = &attack.record;
        accepted.record.clone_from(record);
        accepted.mapping.operation_id.clone_from(id);
        accepted.mapping.commitment = sha2::Sha256::digest(&accepted.record).into();
        value = serde_json::to_value(envelope).unwrap();
        attack.delivered.fetch_add(1, Ordering::SeqCst);
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(
        parts,
        axum::body::Body::from(serde_json::to_vec(&value).unwrap()),
    )
}

#[tokio::test]
async fn valid_foreign_vault_record_leaves_protected_state_unchanged() {
    let mut f = fixture().await;
    // Freeze and append a real valid record without acknowledging it to its author.
    let (_, _, record) = accepted_task(&f, "hostile page must not apply").await;
    let workspace = f.seed.list_workspaces().await.unwrap().remove(0);
    f.seed
        .create_task(&workspace, draft("pending protected local edit"))
        .await
        .unwrap();
    let inputs = f.seed_store.tail_inputs(&f.seed, &f.origin).await.unwrap();
    let frozen_local = head_record(&f.seed, &inputs.authority).await;
    assert_eq!(frozen(&f.seed).await, frozen_local);
    let after = f
        .seed
        .encrypted_round_state(&inputs.authority)
        .await
        .unwrap()
        .cursor;
    let client = Client::new(&f.origin).unwrap();
    let Reply::Page(control) = client
        .exchange(
            &inputs.authority.context,
            &inputs.bearer,
            Operation::Pull {
                after,
                limit: 1,
                watermark: None,
            },
        )
        .await
        .unwrap()
    else {
        panic!("control page")
    };
    assert_eq!(
        control.records[0].record, record,
        "honest response must contain our valid record"
    );
    drop(inputs);
    let other = fixture().await;
    let (id, _, foreign) = accepted_task(&other, "valid foreign vault record").await;
    let attack = Arc::new(Splice {
        delivered: AtomicUsize::new(0),
        record: (id, foreign),
    });
    f.task.abort();
    let _ = (&mut f.task).await;
    let app = peer_enrollment_http::router(f.server.clone())
        .merge(router(
            f.server.clone(),
            crate::config::AttachmentLifecycleConfig::default().server_policy(),
        ))
        .layer(axum::middleware::from_fn_with_state(
            attack.clone(),
            splice_page,
        ));
    (_, f.task) = e2ee_http::serve(app, f.origin.strip_prefix("http://").unwrap()).await;
    let keys = f.root.path().join("keys");
    let before = ProtectedState::capture(&f.seed, &keys).await;
    let error = client
        .pull_only_round(&f.seed_store, &f.seed)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("encrypted-tail"), "{error:#}");
    assert_eq!(attack.delivered.load(Ordering::SeqCst), 1);
    before.assert_unchanged(&f.seed, &keys).await;
    assert_eq!(frozen(&f.seed).await, frozen_local);
    // Restore honest service and prove the same valid page still applies.
    restart_server(&mut f, no_fault()).await;
    client
        .pull_only_round(&f.seed_store, &f.seed)
        .await
        .unwrap();
    assert_eq!(
        scalar(
            &f.seed,
            "SELECT count(*) FROM tasks WHERE title='hostile page must not apply'"
        )
        .await,
        1
    );
}
