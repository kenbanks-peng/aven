use super::*;
use crate::sync::{LocalSharedStatePackageContext, LocalSharedStatePackageKey};

type Fixture = (
    tempfile::TempDir,
    Database,
    SeedSourceAuthority,
    SeedAuthority,
    LocalSharedStatePackageKey,
    String,
);

async fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let database = Database::open(&root.path().join("seed.sqlite"))
        .await
        .unwrap();
    freeze(root, database).await
}

async fn add_task(database: &Database, title: &str) {
    let workspace = database.list_workspaces().await.unwrap().remove(0);
    database
        .create_task(
            &workspace,
            crate::operations::TaskDraft {
                title: title.into(),
                description: String::new(),
                project: Some("app".into()),
                status: "todo".into(),
                priority: "none".into(),
                source: crate::choices::TaskSource::Cli,
                labels: vec![],
                metadata: vec![],
                available_at: None,
                due_on: None,
                is_epic: false,
            },
        )
        .await
        .unwrap();
}

/// A frozen capture with accepted, provenance-carrying and pending history.
async fn history_fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let database = Database::open(&root.path().join("seed.sqlite"))
        .await
        .unwrap();
    add_task(&database, "one").await;
    add_task(&database, "two").await;
    {
        let mut conn = database.acquire_writer().await.unwrap();
        sqlx::query("UPDATE changes SET server_seq = local_seq * 3 WHERE change_id IN (SELECT change_id FROM changes ORDER BY local_seq LIMIT 3)").execute(&mut *conn).await.unwrap();
        sqlx::query("INSERT INTO shared_history_provenance(change_id, source_server_seq, source_pending_rank) SELECT change_id, 999, NULL FROM changes ORDER BY local_seq LIMIT 1").execute(&mut *conn).await.unwrap();
    }
    freeze(root, database).await
}

async fn freeze(root: tempfile::TempDir, database: Database) -> Fixture {
    let key = LocalSharedStatePackageKey::new([7; 32]);
    let context = LocalSharedStatePackageContext {
        vault_id: [8; 32],
        generation_id: [9; 32],
    };
    let seed = SeedAuthority::generate(context, &key, [10; 32]).unwrap();
    database
        .pin_local_seed_genesis(seed.genesis())
        .await
        .unwrap();
    let source = SeedSourceAuthority::generate([11; 32], seed.genesis()).unwrap();
    {
        let installation = db::installation::InstallationGuard::acquire(database.path()).unwrap();
        database
            .bind_seed_source(&source, &installation)
            .await
            .unwrap();
    }
    let capture = database
        .capture_local_shared_state_never_dispatched()
        .await
        .unwrap();
    database
        .package_local_shared_state_never_dispatched(
            root.path(),
            context,
            &key,
            seed.genesis().commitment(),
        )
        .await
        .unwrap();
    (
        root,
        database,
        source,
        seed,
        key,
        capture.candidate_id().to_string(),
    )
}

#[tokio::test]
async fn history_validation_does_not_read_exported_domain_tables() {
    let (_root, database, source, seed, key, _candidate) = fixture().await;
    let mut conn = database.acquire_writer().await.unwrap();
    sqlx::query("ALTER TABLE projects RENAME TO projects_not_read_by_history_validation")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);

    database
        .prepare_seed_publication_intent(&source, &seed, &key)
        .await
        .unwrap();
}

#[tokio::test]
async fn committed_intent_blocks_actual_cancellation_across_pools() {
    let (_root, database, source, seed, key, candidate) = fixture().await;
    let other = Database::open(database.path()).await.unwrap();
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    *INTENT_BARRIER.lock().unwrap() = Some((candidate.clone(), entered, resume));
    let preparing = tokio::spawn(async move {
        database
            .prepare_seed_publication_intent(&source, &seed, &key)
            .await
    });
    waiting.await.unwrap();
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let cancel = tokio::spawn(async move {
        started.send(()).unwrap();
        other
            .cancel_local_shared_state_never_dispatched(&candidate)
            .await
    });
    started_rx.await.unwrap();
    release.send(()).unwrap();
    preparing.await.unwrap().unwrap();
    let error = cancel.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("intent-owned"));
}

#[tokio::test]
async fn cancellation_commit_prevents_intent_creation() {
    let (_root, database, source, seed, key, candidate) = fixture().await;
    let other = Database::open(database.path()).await.unwrap();
    assert!(
        other
            .cancel_local_shared_state_never_dispatched(&candidate)
            .await
            .unwrap()
    );
    assert!(
        database
            .prepare_seed_publication_intent(&source, &seed, &key)
            .await
            .is_err()
    );
    assert!(
        database
            .seed_publication_intent_bytes()
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn intent_sql_failure_preserves_capture_and_allows_local_cancel() {
    let (_root, database, source, seed, key, candidate) = fixture().await;
    let mut conn = database.acquire_writer().await.unwrap();
    sqlx::query("CREATE TRIGGER fail_intent BEFORE INSERT ON local_seed_publication_intent BEGIN SELECT RAISE(ABORT, 'injected'); END").execute(&mut *conn).await.unwrap();
    drop(conn);
    assert!(
        database
            .prepare_seed_publication_intent(&source, &seed, &key)
            .await
            .is_err()
    );
    assert!(
        database
            .seed_publication_intent_bytes()
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        database
            .cancel_local_shared_state_never_dispatched(&candidate)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn lost_intent_row_does_not_restore_local_cancellation_authority() {
    let (_root, database, source, seed, key, candidate) = fixture().await;
    database
        .prepare_seed_publication_intent(&source, &seed, &key)
        .await
        .unwrap();
    let mut conn = database.acquire_writer().await.unwrap();
    sqlx::query("DELETE FROM local_seed_publication_intent")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    assert!(
        database
            .cancel_local_shared_state_never_dispatched(&candidate)
            .await
            .is_err()
    );
    assert!(
        database
            .prepare_seed_publication_intent(&source, &seed, &key)
            .await
            .is_err()
    );
    assert!(
        database
            .resume_local_shared_state_never_dispatched()
            .await
            .is_err()
    );
    let mut conn = database.acquire_writer().await.unwrap();
    assert!(
        sqlx::query("DELETE FROM local_shared_capture_journal")
            .execute(&mut *conn)
            .await
            .is_err()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM local_shared_capture_journal")
            .fetch_one(&mut *conn)
            .await
            .unwrap(),
        1
    );
}

async fn sql(database: &Database, statement: &'static str) {
    let mut conn = database.acquire_writer().await.unwrap();
    sqlx::query(statement).execute(&mut *conn).await.unwrap();
}

async fn intent_error(fixture: &Fixture) -> Option<String> {
    let (_, database, source, seed, key, _) = fixture;
    database
        .prepare_seed_publication_intent(source, seed, key)
        .await
        .err()
        .map(|error| format!("{error:#}"))
}

#[tokio::test]
async fn history_validation_accepts_later_pending_edits() {
    let fixture = history_fixture().await;
    add_task(&fixture.1, "after capture").await;
    assert_eq!(intent_error(&fixture).await, None);
}

#[tokio::test]
async fn history_validation_compares_payloads_semantically() {
    let fixture = history_fixture().await;
    let mut conn = fixture.1.acquire_writer().await.unwrap();
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT change_id, payload FROM changes WHERE payload LIKE '{%'")
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    let mut reordered = 0;
    for (change_id, payload) in rows {
        let serde_json::Value::Object(map) = serde_json::from_str(&payload).unwrap() else {
            continue;
        };
        if map.len() < 2 {
            continue;
        }
        let entries = map
            .iter()
            .rev()
            .map(|(key, value)| format!("{} : {}", serde_json::to_string(key).unwrap(), value))
            .collect::<Vec<_>>();
        let rewritten = format!("{{ {} }}", entries.join(" ,\n "));
        assert_ne!(rewritten, payload);
        sqlx::query("UPDATE changes SET payload = ? WHERE change_id = ?")
            .bind(rewritten)
            .bind(change_id)
            .execute(&mut *conn)
            .await
            .unwrap();
        reordered += 1;
    }
    drop(conn);
    assert!(reordered > 0);
    assert_eq!(intent_error(&fixture).await, None);
}

#[tokio::test]
async fn history_validation_refuses_semantic_payload_change() {
    let fixture = history_fixture().await;
    sql(
        &fixture.1,
        "UPDATE changes SET payload = json_set(payload, '$.injected', 1)
         WHERE change_id = (SELECT change_id FROM changes WHERE payload LIKE '{%' LIMIT 1)",
    )
    .await;
    let error = intent_error(&fixture).await.unwrap();
    assert!(error.contains("seed-captured-history-changed"), "{error}");
}

#[tokio::test]
async fn history_validation_refuses_changed_original_sequence() {
    let fixture = history_fixture().await;
    sql(
        &fixture.1,
        "UPDATE changes SET server_seq = server_seq + 100000
         WHERE change_id = (SELECT change_id FROM changes WHERE server_seq IS NOT NULL LIMIT 1)",
    )
    .await;
    let error = intent_error(&fixture).await.unwrap();
    assert!(error.contains("seed-captured-history-changed"), "{error}");
}

#[tokio::test]
async fn history_validation_refuses_missing_captured_row() {
    let fixture = history_fixture().await;
    sql(
        &fixture.1,
        "DELETE FROM changes WHERE change_id = (SELECT max(change_id) FROM changes)",
    )
    .await;
    let error = intent_error(&fixture).await.unwrap();
    assert!(error.contains("seed-captured-history-changed"), "{error}");
}

#[tokio::test]
async fn history_validation_refuses_uncaptured_accepted_history() {
    let fixture = history_fixture().await;
    add_task(&fixture.1, "after capture").await;
    sql(
        &fixture.1,
        "UPDATE changes SET server_seq = 500000 WHERE change_id NOT IN
         (SELECT change_id FROM local_shared_capture_changes) AND change_id =
         (SELECT max(change_id) FROM changes WHERE change_id NOT IN
          (SELECT change_id FROM local_shared_capture_changes))",
    )
    .await;
    let error = intent_error(&fixture).await.unwrap();
    assert!(
        error.contains("seed-uncaptured-accepted-history"),
        "{error}"
    );
}

#[tokio::test]
async fn history_validation_refuses_changed_provenance() {
    for statement in [
        "UPDATE shared_history_provenance SET source_server_seq = 998",
        "DELETE FROM shared_history_provenance",
        "INSERT INTO shared_history_provenance(change_id, source_server_seq, source_pending_rank)
         SELECT change_id, NULL, 7 FROM changes WHERE change_id NOT IN
         (SELECT change_id FROM shared_history_provenance) LIMIT 1",
    ] {
        let fixture = history_fixture().await;
        sql(&fixture.1, statement).await;
        let error = intent_error(&fixture).await.unwrap();
        assert!(error.contains("seed-source-provenance-changed"), "{error}");
    }
}

#[tokio::test]
async fn history_validation_refuses_changed_capture_map() {
    let fixture = history_fixture().await;
    sql(
        &fixture.1,
        "DELETE FROM local_shared_capture_changes WHERE change_id =
         (SELECT max(change_id) FROM local_shared_capture_changes)",
    )
    .await;
    assert!(intent_error(&fixture).await.is_some());
}
