// These tests pin the current unquarantined behavior of authenticated member
// inbound records (AVN-P42B finding W3). A fix must update them to assert the
// new quarantine or skip behavior rather than delete them.
use super::*;
use aven_core::ids::TaskId;

async fn cursor(db: &Database) -> String {
    db.meta("sync_cursor").await.unwrap().unwrap()
}

#[derive(Default)]
struct RequestTrace {
    requests: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}

async fn trace_requests(
    axum::extract::State(trace): axum::extract::State<Arc<RequestTrace>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_owned();
    let bytes = to_bytes(body, 8 * 1024 * 1024).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap();
    trace.requests.lock().unwrap().push((path, value));
    next.run(axum::extract::Request::from_parts(
        parts,
        axum::body::Body::from(bytes),
    ))
    .await
}

async fn trace_server_requests(f: &mut Fixture, trace: Arc<RequestTrace>) {
    f.task.abort();
    let _ = (&mut f.task).await;
    let app = crate::peer_enrollment_http::router(f.server.clone())
        .merge(crate::encrypted_tail_http::router(
            f.server.clone(),
            Default::default(),
        ))
        .layer(axum::middleware::from_fn_with_state(trace, trace_requests));
    (_, f.task) = e2ee_http::serve(app, f.origin.strip_prefix("http://").unwrap()).await;
}

#[tokio::test]
async fn invalid_project_metadata_wedges_peers_but_origin_advances_via_batch() {
    let mut f = fixture().await;
    let client = Client::new(&f.origin).unwrap();
    converge(&f).await;
    assert_quiescent(&[&f.seed, &f.peer]).await;
    let victim_cursor = cursor(&f.peer).await;
    assert_eq!(cursor(&f.seed).await, victim_cursor);

    let trace = Arc::new(RequestTrace::default());
    trace_server_requests(&mut f, trace.clone()).await;
    let workspace = f.seed.list_workspaces().await.unwrap().remove(0);
    let project = f
        .seed
        .rename_project(&workspace, "app", "Project", Some("APP"))
        .await
        .unwrap()
        .project;
    let metadata_change: String = sqlx::query_scalar(
        "SELECT change_id FROM changes
         WHERE server_seq IS NULL AND op_type='set_project_metadata'
         ORDER BY local_seq DESC LIMIT 1",
    )
    .fetch_one(&mut *aven_core::test_support::acquire(&f.seed).await.unwrap())
    .await
    .unwrap();
    let payload: String = sqlx::query_scalar("SELECT payload FROM changes WHERE change_id=?")
        .bind(&metadata_change)
        .fetch_one(&mut *aven_core::test_support::acquire(&f.seed).await.unwrap())
        .await
        .unwrap();
    let mut payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    payload["key"] = "wrong".into();
    sqlx::query("UPDATE changes SET payload=? WHERE change_id=?")
        .bind(payload.to_string())
        .bind(&metadata_change)
        .execute(&mut *aven_core::test_support::acquire(&f.seed).await.unwrap())
        .await
        .unwrap();
    let metadata_shape: (String, String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT entity_type, entity_id, field, base_version FROM changes WHERE change_id=?",
    )
    .bind(&metadata_change)
    .fetch_one(&mut *aven_core::test_support::acquire(&f.seed).await.unwrap())
    .await
    .unwrap();
    assert_eq!(metadata_shape.0, "project");
    assert_eq!(metadata_shape.1, project.id.as_str());
    assert_eq!(metadata_shape.2, None);
    assert_eq!(metadata_shape.3, None);
    assert_eq!(payload["key"], "wrong");
    assert_eq!(payload["name"], "Project");
    assert_eq!(payload["prefix"], "APP");

    let existing_task: TaskId =
        sqlx::query_scalar("SELECT id FROM tasks WHERE title='AFTER-CAPTURE' LIMIT 1")
            .fetch_one(&mut *aven_core::test_support::acquire(&f.seed).await.unwrap())
            .await
            .unwrap();
    f.seed
        .update_task(
            &workspace,
            &existing_task,
            TaskUpdate {
                title: Some("batch companion change".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Two validly shaped local rows make the normal client negotiate batch append.
    drain(&client, &f.seed_store, &f.seed).await;
    let requests = trace.requests.lock().unwrap().clone();
    let batch = requests
        .iter()
        .find(|(path, value)| {
            path == aven_core::sync::client::tail::BATCH_PATH
                && value["operation"].get("Append").is_some()
        })
        .expect("attacker should append both changes over the batch HTTP route");
    assert_eq!(
        batch.1["operation"]["Append"]["records"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let (metadata_sequence, metadata_record): (i64, Vec<u8>) =
        sqlx::query_as("SELECT sequence, record FROM server_e2ee_tail WHERE operation_id=?")
            .bind(&metadata_change)
            .fetch_one(&mut *aven_core::test_support::acquire(&f.server).await.unwrap())
            .await
            .unwrap();
    assert!(!metadata_record.is_empty());
    assert!(metadata_sequence > victim_cursor.parse::<i64>().unwrap());
    assert!(
        cursor(&f.seed).await.parse::<i64>().unwrap() > metadata_sequence,
        "the origin must also advance across the companion record"
    );
    assert_eq!(cursor(&f.peer).await, victim_cursor);
    let local_project: String = sqlx::query_scalar("SELECT key FROM projects WHERE id=?")
        .bind(project.id.as_str())
        .fetch_one(&mut *aven_core::test_support::acquire(&f.seed).await.unwrap())
        .await
        .unwrap();
    assert_eq!(local_project, "project");

    // The origin recognizes its own change ID and advances without applying it.
    // A peer has no matching local row and fails while applying that same page.
    for _ in 0..2 {
        let error = client
            .round(&f.peer_store, &f.peer, &blobs(&f.peer))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "error encrypted-tail-apply");
        assert_eq!(cursor(&f.peer).await, victim_cursor);
        assert_eq!(title(&f.peer, &existing_task).await, "AFTER-CAPTURE");
        f.peer.export_data("fixed".into()).await.unwrap();
    }
}

#[tokio::test]
async fn absent_task_set_field_wedge_repeats_without_advancing_cursor() {
    let f = fixture().await;
    let client = Client::new(&f.origin).unwrap();
    converge(&f).await;
    assert_quiescent(&[&f.seed, &f.peer]).await;
    let converged_cursor = cursor(&f.peer).await;
    assert_eq!(cursor(&f.seed).await, converged_cursor);

    let workspace = f.seed.list_workspaces().await.unwrap().remove(0);
    let existing_task: TaskId =
        sqlx::query_scalar("SELECT id FROM tasks WHERE title='AFTER-CAPTURE' LIMIT 1")
            .fetch_one(&mut *aven_core::test_support::acquire(&f.seed).await.unwrap())
            .await
            .unwrap();

    // An ordinary field update on an existing shared task is accepted first.
    f.seed
        .update_task(
            &workspace,
            &existing_task,
            TaskUpdate {
                title: Some("control update applied".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    drain(&client, &f.seed_store, &f.seed).await;
    drain(&client, &f.peer_store, &f.peer).await;
    assert_eq!(
        title(&f.peer, &existing_task).await,
        "control update applied"
    );
    let control_cursor = cursor(&f.peer).await;
    assert_eq!(cursor(&f.seed).await, control_cursor);

    // Keep the normal local change shape, but address a valid absent task ID.
    f.seed
        .update_task(
            &workspace,
            &existing_task,
            TaskUpdate {
                title: Some("unapplied absent-task update".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let change_id: String = sqlx::query_scalar(
        "SELECT change_id FROM changes
         WHERE server_seq IS NULL AND op_type='set_field' AND field='title'
         ORDER BY local_seq DESC LIMIT 1",
    )
    .fetch_one(&mut *aven_core::test_support::acquire(&f.seed).await.unwrap())
    .await
    .unwrap();
    let absent_task = TaskId::new();
    let absent_task_id = absent_task.as_str();
    let exists_on_attacker: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?)")
            .bind(absent_task_id)
            .fetch_one(&mut *aven_core::test_support::acquire(&f.seed).await.unwrap())
            .await
            .unwrap();
    let exists_on_victim: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?)")
            .bind(absent_task_id)
            .fetch_one(&mut *aven_core::test_support::acquire(&f.peer).await.unwrap())
            .await
            .unwrap();
    assert!(!exists_on_attacker && !exists_on_victim);
    sqlx::query("UPDATE changes SET entity_id=?, base_version=NULL WHERE change_id=?")
        .bind(absent_task_id)
        .bind(&change_id)
        .execute(&mut *aven_core::test_support::acquire(&f.seed).await.unwrap())
        .await
        .unwrap();

    let pending: (
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        String,
    ) = sqlx::query_as(
        "SELECT entity_type, entity_id, field, op_type, base_version, payload
             FROM changes WHERE change_id=?",
    )
    .bind(&change_id)
    .fetch_one(&mut *aven_core::test_support::acquire(&f.seed).await.unwrap())
    .await
    .unwrap();
    assert_eq!(pending.0, "task");
    assert_eq!(pending.1, absent_task_id);
    assert_eq!(pending.2.as_deref(), Some("title"));
    assert_eq!(pending.3, "set_field");
    assert_eq!(pending.4, None);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&pending.5).unwrap()["value"],
        "unapplied absent-task update"
    );

    // The normal HTTP round performs preflight, freeze, sealing, and append.
    drain(&client, &f.seed_store, &f.seed).await;
    let (sequence, commitment, record): (i64, Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT sequence, commitment, record FROM server_e2ee_tail WHERE operation_id=?",
    )
    .bind(&change_id)
    .fetch_one(&mut *aven_core::test_support::acquire(&f.server).await.unwrap())
    .await
    .unwrap();
    assert!(sequence > control_cursor.parse::<i64>().unwrap());
    assert!(!commitment.is_empty());
    assert!(!record.is_empty());
    assert_eq!(cursor(&f.seed).await, sequence.to_string());

    let victim_cursor = cursor(&f.peer).await;
    assert_eq!(victim_cursor, control_cursor);
    for _ in 0..2 {
        let error = client
            .round(&f.peer_store, &f.peer, &blobs(&f.peer))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "error encrypted-tail-apply");
        assert_eq!(cursor(&f.peer).await, victim_cursor);
        assert_eq!(
            title(&f.peer, &existing_task).await,
            "control update applied"
        );
        f.peer.export_data("fixed".into()).await.unwrap();
    }
}
