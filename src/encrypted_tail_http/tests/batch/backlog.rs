use super::*;

async fn pending(db: &Database) -> i64 {
    scalar(db, "SELECT count(*) FROM changes WHERE server_seq IS NULL").await
}

async fn large_notes(f: &Fixture) {
    let workspace = f.peer.list_workspaces().await.unwrap().remove(0);
    let task_id: aven_core::ids::TaskId = sqlx::query_scalar("SELECT id FROM tasks LIMIT 1")
        .fetch_one(&mut *aven_core::test_support::acquire(&f.peer).await.unwrap())
        .await
        .unwrap();
    for index in 0..300 {
        f.peer
            .add_note(
                &workspace,
                &task_id,
                format!("{index:03}-{}", "x".repeat(60_000)),
            )
            .await
            .unwrap();
    }
    assert_eq!(pending(&f.peer).await, 300);
    assert!(
        scalar(
            &f.peer,
            "SELECT sum(length(payload)) FROM changes WHERE server_seq IS NULL"
        )
        .await
            > 16 * 1048576
    );
}

#[tokio::test]
async fn offline_backlog_above_count_limit_drains_after_lost_reply_and_reopen() {
    let mut f = fixture().await;
    converge(&f).await;
    let workspace = f.seed.list_workspaces().await.unwrap().remove(0);
    f.seed
        .create_task(&workspace, draft("remote while backlogged"))
        .await
        .unwrap();
    let client = Client::new(&f.origin).unwrap();
    drain(&client, &f.seed_store, &f.seed).await;
    edits(&f, 4100).await;
    assert_eq!(pending(&f.peer).await, 4100);
    let state = Arc::new(Traffic {
        lose: AtomicBool::new(true),
        ..Default::default()
    });
    instrument(&mut f, state).await;
    assert!(
        client
            .round(&f.peer_store, &f.peer, &blobs(&f.peer))
            .await
            .is_err()
    );
    let inputs = f.peer_store.tail_inputs(&f.peer, &f.origin).await.unwrap();
    let frozen = f
        .peer
        .encrypted_tail_frozen_records(&inputs.authority)
        .await
        .unwrap();
    assert_eq!(frozen.len(), tail::BATCH_COUNT);
    let reopened = Database::open(f.peer.path()).await.unwrap();
    assert_eq!(
        reopened
            .encrypted_tail_frozen_records(&inputs.authority)
            .await
            .unwrap(),
        frozen
    );
    let round = client
        .round(&f.peer_store, &reopened, &blobs(&f.peer))
        .await
        .unwrap();
    assert!(round.sent_changes <= 2048);
    assert!(pending(&reopened).await > 0 && pending(&reopened).await < 4100);
    assert_eq!(
        scalar(
            &reopened,
            "SELECT count(*) FROM tasks WHERE title='remote while backlogged'"
        )
        .await,
        1
    );
    drain(&client, &f.peer_store, &reopened).await;
    drain(&client, &f.seed_store, &f.seed).await;
    assert_eq!(pending(&reopened).await, 0);
    assert_eq!(
        scalar(&reopened, "SELECT count(*) FROM local_e2ee_outbox").await,
        0
    );
    assert_eq!(
        scalar(
            &f.seed,
            "SELECT count(*) FROM tasks WHERE title LIKE 'batch task %'"
        )
        .await,
        4100
    );
    for (id, bytes) in frozen {
        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT record FROM server_e2ee_tail WHERE operation_id=?")
                .bind(id)
                .fetch_one(&mut *aven_core::test_support::acquire(&f.server).await.unwrap())
                .await
                .unwrap();
        assert_eq!(stored, bytes);
    }
}

#[tokio::test]
async fn offline_backlog_above_byte_limit_drains_in_batch_and_singleton_modes() {
    for legacy in [false, true] {
        let mut f = fixture().await;
        converge(&f).await;
        large_notes(&f).await;
        let state = Arc::new(Traffic {
            legacy,
            ..Default::default()
        });
        instrument(&mut f, state).await;
        let client = Client::new(&f.origin).unwrap();
        drain(&client, &f.peer_store, &f.peer).await;
        drain(&client, &f.seed_store, &f.seed).await;
        assert_eq!(pending(&f.peer).await, 0);
        assert_eq!(
            scalar(&f.peer, "SELECT count(*) FROM local_e2ee_outbox").await,
            0
        );
        let local: Vec<(String, String)> = sqlx::query_as("SELECT id, body FROM notes ORDER BY id")
            .fetch_all(&mut *aven_core::test_support::acquire(&f.peer).await.unwrap())
            .await
            .unwrap();
        let remote: Vec<(String, String)> =
            sqlx::query_as("SELECT id, body FROM notes ORDER BY id")
                .fetch_all(&mut *aven_core::test_support::acquire(&f.seed).await.unwrap())
                .await
                .unwrap();
        assert_eq!(local.len(), 300);
        assert_eq!(local, remote);
    }
}

#[tokio::test]
async fn removed_pending_rows_do_not_extend_cached_validation_into_an_invalid_window() {
    let f = fixture().await;
    converge(&f).await;
    large_notes(&f).await;
    let inputs = f.peer_store.tail_inputs(&f.peer, &f.origin).await.unwrap();
    let (_, coverage) = f
        .peer
        .prepare_encrypted_batch_in_run(&inputs.authority, &blobs(&f.peer), None, 1)
        .await
        .unwrap();
    let client = Client::new(&f.origin).unwrap();
    client
        .push(&inputs, &f.peer, &blobs(&f.peer))
        .await
        .unwrap();
    // Undo can remove pending rows without advancing the local mutation sequence.
    // Retain only the last two rows, beyond the byte-bounded validation window.
    let mut conn = aven_core::test_support::acquire(&f.peer).await.unwrap();
    sqlx::query("DELETE FROM changes WHERE server_seq IS NULL AND change_id NOT IN (SELECT change_id FROM changes WHERE server_seq IS NULL ORDER BY local_seq DESC, created_at DESC, change_id DESC LIMIT 2)")
        .execute(&mut *conn).await.unwrap();
    sqlx::query("UPDATE changes SET op_type='unsupported' WHERE change_id=(SELECT change_id FROM changes WHERE server_seq IS NULL ORDER BY local_seq DESC, created_at DESC, change_id DESC LIMIT 1)")
        .execute(&mut *conn).await.unwrap();
    drop(conn);
    let error = f
        .peer
        .prepare_encrypted_batch_in_run(&inputs.authority, &blobs(&f.peer), coverage, 1)
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("operation-unsupported"));
    assert_eq!(pending(&f.peer).await, 2);
    assert_eq!(
        scalar(&f.peer, "SELECT count(*) FROM local_e2ee_outbox").await,
        0
    );
}
