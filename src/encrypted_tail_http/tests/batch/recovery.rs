use super::*;
use aven_core::sync::{
    client::tail::BATCH_PATH,
    encrypted_tail::BatchRecord,
    seed_claim::membership::{Joiner, Membership, VerifiedKeys},
};
use std::path::Path;

async fn outbox(f: &Fixture) -> Vec<(String, Vec<u8>)> {
    let mut conn = aven_core::test_support::acquire(&f.peer).await.unwrap();
    sqlx::query_as("SELECT operation_id, record FROM local_e2ee_outbox ORDER BY position")
        .fetch_all(&mut *conn)
        .await
        .unwrap()
}

async fn server_records(f: &Fixture) -> Vec<(String, Vec<u8>)> {
    let mut conn = aven_core::test_support::acquire(&f.server).await.unwrap();
    sqlx::query_as("SELECT operation_id, record FROM server_e2ee_tail ORDER BY sequence")
        .fetch_all(&mut *conn)
        .await
        .unwrap()
}

async fn outbox_records(db: &Database) -> Vec<(String, Vec<u8>)> {
    let mut conn = aven_core::test_support::acquire(db).await.unwrap();
    sqlx::query_as("SELECT operation_id, record FROM local_e2ee_outbox ORDER BY position")
        .fetch_all(&mut *conn)
        .await
        .unwrap()
}

fn batch_requests(state: &Traffic) -> Vec<(String, serde_json::Value)> {
    state
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(path, _)| path == BATCH_PATH)
        .cloned()
        .collect()
}

fn append_records(value: &serde_json::Value) -> Vec<Vec<u8>> {
    serde_json::from_value::<Vec<BatchRecord>>(value["operation"]["Append"]["records"].clone())
        .unwrap()
        .into_iter()
        .map(|record| record.0)
        .collect()
}

async fn begin_rotation(
    f: &Fixture,
    driver: &super::super::membership::Joined,
    targets: &[[u8; 32]],
) -> (Joiner, Membership, VerifiedKeys, u64) {
    let enrollment = crate::peer_enrollment_http::Client::new(&f.origin).unwrap();
    enrollment.refresh(&driver.store, &driver.db).await.unwrap();
    let signer = driver
        .store
        .prepare_peer(&driver.db, &f.origin, None)
        .await
        .unwrap();
    let inputs = driver
        .store
        .active_inputs(&driver.db, &f.origin)
        .await
        .unwrap();
    let membership = inputs.membership.clone();
    let keys = VerifiedKeys::from_protected_storage(
        &membership,
        &inputs.generation_keys().protected_storage_bytes(),
    )
    .unwrap();
    drop(inputs);

    let revoke = signer
        .authority()
        .prepare_revoke(&membership, targets)
        .unwrap();
    f.server
        .apply_membership_management(&auth(&membership, &signer), &revoke)
        .await
        .unwrap();
    let membership = membership.append(&[], &[], &revoke).unwrap();
    let high_water = f
        .server
        .prepare_membership_management(&auth(&membership, &signer))
        .await
        .unwrap()
        .high_water;
    (signer, membership, keys, high_water)
}

async fn finish_rotation(
    f: &Fixture,
    signer: &Joiner,
    membership: &Membership,
    keys: &VerifiedKeys,
    high_water: u64,
) {
    let rotation = signer
        .authority()
        .prepare_rotation(membership, keys, high_water)
        .unwrap();
    f.server
        .apply_membership_management(&auth(membership, signer), &rotation)
        .await
        .unwrap();
}

fn auth<'a>(
    membership: &aven_core::sync::seed_claim::membership::Membership,
    signer: &'a aven_core::sync::seed_claim::membership::Joiner,
) -> aven_core::sync::seed_claim::peer::Authentication<'a> {
    aven_core::sync::seed_claim::peer::Authentication {
        vault: membership.genesis().context().vault_id,
        genesis: membership.genesis().commitment(),
        head: membership.head(),
        device: signer.device(),
        bearer: signer.bearer(),
    }
}

async fn crash_supersession_worker(root: &Path, origin: &str, stage: &str) {
    let output =
        e2ee_http::worker("encrypted_tail_http::tests::membership::rotation::supersession_worker")
            .env("AVEN_SUPER_ROOT", root)
            .env("AVEN_SUPER_ORIGIN", origin)
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
}

#[tokio::test]
async fn partial_batch_outcome_across_rotation_resolves_every_id_before_resend() {
    let mut f = fixture().await;
    converge(&f).await;
    edits(&f, 5).await;
    let (inputs, original) = frozen(&f).await;
    let prefix_len = server_records(&f).await.len();
    append(&f, &inputs, &original[..2]).await.unwrap();
    drop(inputs);
    let third = super::super::membership::join(&f, "third", &f.seed, &f.seed_store).await;
    let seed_device = f
        .seed_store
        .active_inputs(&f.seed, &f.origin)
        .await
        .unwrap()
        .device();
    let (signer, membership, keys, high_water) = begin_rotation(&f, &third, &[seed_device]).await;
    assert!(membership.rotation_pending());
    let state = Arc::new(Traffic::default());
    instrument(&mut f, state.clone()).await;

    crate::peer_enrollment_http::Client::new(&f.origin)
        .unwrap()
        .refresh(&f.peer_store, &f.peer)
        .await
        .unwrap();
    let pending_inputs = f.peer_store.tail_inputs(&f.peer, &f.origin).await.unwrap();
    assert!(pending_inputs.authority.rotation_pending());
    assert!(matches!(
        Client::new(&f.origin)
            .unwrap()
            .push(&pending_inputs, &f.peer, &blobs(&f.peer))
            .await
            .unwrap(),
        PushStep::Empty
    ));
    drop(pending_inputs);
    let while_pending = outbox(&f).await;
    let expected_pending = original[2..]
        .iter()
        .map(|(id, record)| (id.clone(), record.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        while_pending.iter().map(|(id, _)| id).collect::<Vec<_>>(),
        expected_pending
            .iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        "pending outbox IDs after rotation lookups; accepted server IDs: {:?}",
        server_records(&f)
            .await
            .iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        while_pending, expected_pending,
        "closed-generation absence must retain exact bytes while rotation is pending"
    );
    let pending_requests = state.requests.lock().unwrap().clone();
    let lookups = pending_requests
        .iter()
        .filter(|(path, value)| path == PATH && value["operation"].get("Lookup").is_some())
        .map(|(_, value)| {
            match serde_json::from_value::<Operation>(value["operation"].clone()).unwrap() {
                Operation::Lookup { operation_id, .. } => operation_id,
                _ => panic!("pending batch resolution must use operation lookups"),
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lookups,
        original
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        !pending_requests
            .iter()
            .any(|(_, value)| value["operation"].get("Append").is_some()),
        "publishing must wait for the pending membership rotation"
    );

    finish_rotation(&f, &signer, &membership, &keys, high_water).await;
    state.requests.lock().unwrap().clear();
    crash_supersession_worker(f.root.path(), &f.origin, "after-supersede-commit").await;

    let observed = batch_requests(&state);
    let first_resolve = observed
        .iter()
        .position(|(_, value)| value["operation"].get("Resolve").is_some())
        .unwrap();
    assert!(
        !observed
            .iter()
            .any(|(_, value)| value["operation"].get("Append").is_some()),
        "mixed Found/Absent recovery must resolve every surviving ID before append"
    );
    let BatchOperation::Resolve { operation_ids } =
        serde_json::from_value(observed[first_resolve].1["operation"].clone()).unwrap()
    else {
        panic!("all original IDs must be reconciled")
    };
    assert_eq!(
        operation_ids,
        original[2..]
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    );

    let replacement = outbox(&f).await;
    assert_eq!(
        replacement.iter().map(|(id, _)| id).collect::<Vec<_>>(),
        original[2..].iter().map(|(id, _)| id).collect::<Vec<_>>()
    );
    assert!(
        replacement
            .iter()
            .zip(&original[2..])
            .all(|((_, new), (_, old))| new != old),
        "absent records from the closed generation must be re-enveloped"
    );
    let accepted_group_ids: i64 = {
        let mut conn = aven_core::test_support::acquire(&f.peer).await.unwrap();
        sqlx::query_scalar("SELECT count(*) FROM local_e2ee_accepted WHERE operation_id IN (?, ?)")
            .bind(&original[0].0)
            .bind(&original[1].0)
            .fetch_one(&mut *conn)
            .await
            .unwrap()
    };
    assert_eq!(accepted_group_ids, 2);
    let accepted_before_retry = server_records(&f).await;
    assert_eq!(accepted_before_retry.len(), prefix_len + 2);
    assert_eq!(accepted_before_retry[prefix_len..], original[..2]);

    state.requests.lock().unwrap().clear();
    let reopened = Database::open(f.peer.path()).await.unwrap();
    let store = isolated_store(&reopened, &f.root.path().join("peer-keys")).await;
    Client::new(&f.origin)
        .unwrap()
        .round(&store, &reopened, &blobs(&reopened))
        .await
        .unwrap();
    let requests = batch_requests(&state);
    let resolve_index = requests
        .iter()
        .position(|(_, value)| value["operation"].get("Resolve").is_some())
        .unwrap();
    let append_index = requests
        .iter()
        .position(|(_, value)| value["operation"].get("Append").is_some())
        .unwrap();
    assert!(resolve_index < append_index);
    let BatchOperation::Resolve { operation_ids } =
        serde_json::from_value(requests[resolve_index].1["operation"].clone()).unwrap()
    else {
        panic!("restart must resolve all surviving IDs")
    };
    assert_eq!(
        operation_ids,
        replacement
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        append_records(&requests[append_index].1),
        replacement
            .iter()
            .map(|(_, record)| record.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(outbox_records(&reopened).await, Vec::new());
    let accepted = server_records(&f).await;
    assert_eq!(accepted.len(), prefix_len + original.len());
    assert_eq!(accepted[prefix_len..prefix_len + 2], original[..2]);
    assert_eq!(accepted[prefix_len + 2..], replacement);
}

async fn make_shared_update(f: &Fixture, divergent: bool) -> String {
    let workspace = f.seed.list_workspaces().await.unwrap().remove(0);
    let task = f
        .seed
        .create_task(&workspace, draft("batch shared identity"))
        .await
        .unwrap()
        .task;
    converge(f).await;
    let seed_title = if divergent {
        "canonical seed meaning"
    } else {
        "canonical equal meaning"
    };
    let peer_title = if divergent {
        "canonical peer divergence"
    } else {
        "canonical equal meaning"
    };
    for (db, title) in [(&f.seed, seed_title), (&f.peer, peer_title)] {
        db.update_task(
            &workspace,
            &task.id,
            TaskUpdate {
                title: Some(title.into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    let (operation_id, created_at): (String, String) = {
        let mut conn = aven_core::test_support::acquire(&f.seed).await.unwrap();
        sqlx::query_as("SELECT change_id, created_at FROM changes WHERE server_seq IS NULL")
            .fetch_one(&mut *conn)
            .await
            .unwrap()
    };
    {
        let mut conn = aven_core::test_support::acquire(&f.peer).await.unwrap();
        let local_id: String =
            sqlx::query_scalar("SELECT change_id FROM changes WHERE server_seq IS NULL")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        sqlx::query("UPDATE changes SET change_id=?, created_at=? WHERE change_id=?")
            .bind(&operation_id)
            .bind(&created_at)
            .bind(&local_id)
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("UPDATE field_versions SET version=? WHERE version=?")
            .bind(&operation_id)
            .bind(&local_id)
            .execute(&mut *conn)
            .await
            .unwrap();
    }
    edits(f, 1).await;
    operation_id
}

#[tokio::test]
async fn alternate_batch_ciphertext_requires_equal_canonical_meaning_and_blocks_divergence() {
    for divergent in [false, true] {
        let mut f = fixture().await;
        let duplicate_id = make_shared_update(&f, divergent).await;
        let pending_ids: Vec<String> = {
            let mut conn = aven_core::test_support::acquire(&f.peer).await.unwrap();
            sqlx::query_scalar(
                "SELECT change_id FROM changes WHERE server_seq IS NULL
                 ORDER BY local_seq, created_at, change_id",
            )
            .fetch_all(&mut *conn)
            .await
            .unwrap()
        };
        assert_eq!(pending_ids.len(), 2);
        assert_eq!(pending_ids[0], duplicate_id);
        let before = scalar(&f.server, "SELECT high_water FROM server_e2ee_allocator").await;
        let server_before = server_records(&f).await.len();
        drain(&Client::new(&f.origin).unwrap(), &f.seed_store, &f.seed).await;
        let remote_record: Vec<u8> = {
            let mut conn = aven_core::test_support::acquire(&f.server).await.unwrap();
            sqlx::query_scalar("SELECT record FROM server_e2ee_tail WHERE operation_id=?")
                .bind(&duplicate_id)
                .fetch_one(&mut *conn)
                .await
                .unwrap()
        };
        let state = Arc::new(Traffic::default());
        instrument(&mut f, state.clone()).await;
        let client = Client::new(&f.origin).unwrap();

        // The batch is refused atomically at the known ID, then retried only
        // after the client has resolved the complete frozen group.
        let cursor_before = f.peer.meta("sync_cursor").await.unwrap();
        let first_round = client.round(&f.peer_store, &f.peer, &blobs(&f.peer)).await;
        let requests = batch_requests(&state);
        let first_append = requests
            .iter()
            .position(|(_, value)| value["operation"].get("Append").is_some())
            .unwrap();
        let original_records = append_records(&requests[first_append].1);
        assert_eq!(original_records.len(), pending_ids.len());
        assert_ne!(original_records[0], remote_record);
        let resolve = requests
            .iter()
            .position(|(_, value)| value["operation"].get("Resolve").is_some())
            .unwrap();
        assert!(first_append < resolve);
        let BatchOperation::Resolve { operation_ids } =
            serde_json::from_value(requests[resolve].1["operation"].clone()).unwrap()
        else {
            panic!("the full group must be reconciled after known-ID refusal")
        };
        assert_eq!(operation_ids, pending_ids);
        assert!(
            state.requests.lock().unwrap().iter().any(|(path, value)| {
                path == PATH && value["operation"].get("Lookup").is_some()
            }),
            "a different commitment must be fetched before canonical comparison"
        );

        let all_requests = state.requests.lock().unwrap().clone();
        let resolve_global = all_requests
            .iter()
            .position(|(path, value)| {
                path == BATCH_PATH && value["operation"].get("Resolve").is_some()
            })
            .unwrap();
        if divergent {
            assert_eq!(
                scalar(&f.server, "SELECT high_water FROM server_e2ee_allocator").await,
                before + 1
            );
            let error = first_round.unwrap_err();
            assert!(
                error.to_string().contains("same-id-divergence"),
                "{error:#}"
            );
            assert_eq!(f.peer.meta("sync_cursor").await.unwrap(), cursor_before);
            let original = outbox(&f).await;
            assert_eq!(
                original
                    .iter()
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>(),
                pending_ids
            );
            assert_eq!(
                original
                    .iter()
                    .map(|(_, record)| record.clone())
                    .collect::<Vec<_>>(),
                original_records
            );
            assert_eq!(
                scalar(
                    &f.peer,
                    "SELECT count(*) FROM local_e2ee_outbox WHERE blocked=1"
                )
                .await,
                1
            );
            let observed: (Option<i64>, Option<Vec<u8>>) = {
                let mut conn = aven_core::test_support::acquire(&f.peer).await.unwrap();
                sqlx::query_as(
                    "SELECT observed_sequence, observed_commitment
                     FROM local_e2ee_outbox WHERE operation_id=?",
                )
                .bind(&duplicate_id)
                .fetch_one(&mut *conn)
                .await
                .unwrap()
            };
            let remote_mapping: (i64, Vec<u8>) = {
                let mut conn = aven_core::test_support::acquire(&f.server).await.unwrap();
                sqlx::query_as(
                    "SELECT sequence, commitment FROM server_e2ee_tail WHERE operation_id=?",
                )
                .bind(&duplicate_id)
                .fetch_one(&mut *conn)
                .await
                .unwrap()
            };
            assert_eq!(observed, (Some(remote_mapping.0), Some(remote_mapping.1)));
            assert!(
                !all_requests[resolve_global + 1..]
                    .iter()
                    .any(|(_, value)| value["operation"].get("Append").is_some()),
                "divergent canonical history must not advance to the absent neighbor"
            );
            edits(&f, 1).await;
            state.requests.lock().unwrap().clear();
            let high = scalar(&f.server, "SELECT high_water FROM server_e2ee_allocator").await;
            assert!(
                client
                    .round(&f.peer_store, &f.peer, &blobs(&f.peer))
                    .await
                    .is_err()
            );
            assert_eq!(
                scalar(&f.server, "SELECT high_water FROM server_e2ee_allocator").await,
                high
            );
            assert_eq!(outbox(&f).await, original);
            let accepted_neighbor: i64 = {
                let mut conn = aven_core::test_support::acquire(&f.peer).await.unwrap();
                sqlx::query_scalar("SELECT count(*) FROM local_e2ee_accepted WHERE operation_id=?")
                    .bind(&pending_ids[1])
                    .fetch_one(&mut *conn)
                    .await
                    .unwrap()
            };
            assert_eq!(accepted_neighbor, 0);
            assert_eq!(f.peer.meta("sync_cursor").await.unwrap(), cursor_before);
            assert!(
                !state
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(_, value)| value["operation"].get("Append").is_some()),
                "newer local work must stay behind the unresolved divergent operation"
            );
        } else {
            first_round.unwrap();
            assert_eq!(
                scalar(&f.server, "SELECT high_water FROM server_e2ee_allocator").await,
                before + 2
            );
            let append_after_resolve = all_requests[resolve_global + 1..]
                .iter()
                .find(|(path, value)| path == PATH && value["operation"].get("Append").is_some())
                .unwrap();
            let Operation::Append { record, ticket } =
                serde_json::from_value(append_after_resolve.1["operation"].clone()).unwrap()
            else {
                panic!("the remaining singleton must use the existing append contract")
            };
            assert!(ticket.is_none());
            assert_eq!(record, original_records[1]);
            assert_eq!(outbox(&f).await, Vec::new());
            let accepted: Vec<u8> = {
                let mut conn = aven_core::test_support::acquire(&f.peer).await.unwrap();
                sqlx::query_scalar("SELECT record FROM local_e2ee_accepted WHERE operation_id=?")
                    .bind(&duplicate_id)
                    .fetch_one(&mut *conn)
                    .await
                    .unwrap()
            };
            assert_eq!(accepted, remote_record);
            assert_eq!(server_records(&f).await.len(), server_before + 2);
        }
    }
}
