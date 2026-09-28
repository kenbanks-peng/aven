use super::*;
use crate::{
    protected_local_keys::tests::isolated_store,
    test_support::e2ee_http::{self, fixture},
};
use aven_core::sync::bootstrap_format::Package;
use aven_core::sync::client::bootstrap::components;
use axum::{body::to_bytes, http::header};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

#[cfg(unix)]
mod bench;

#[tokio::test]
async fn client_keeps_attachment_quota_refusal_code() {
    use axum::{Router, routing::post};

    let root = tempfile::tempdir().unwrap();
    let (_db, _store, seed, _package) = fixture(root.path()).await;
    let app = Router::new().route(
        PATH,
        post(|| async {
            http_admission::refusal(StatusCode::PAYLOAD_TOO_LARGE, "attachment-quota-exceeded")
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let result = client
        .exchange(
            seed.genesis(),
            seed.bearer(),
            Operation::Status { bootstrap: [0; 32] },
        )
        .await;
    let Err(error) = result else {
        panic!("quota refusal was accepted")
    };
    assert_eq!(error.to_string(), "error attachment-quota-exceeded");
    task.abort();
}

#[tokio::test]
async fn client_retries_retryable_busy_response() {
    use axum::{Router, http::header, response::IntoResponse, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    let root = tempfile::tempdir().unwrap();
    let (_db, _store, seed, _package) = fixture(root.path()).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let state = attempts.clone();
    let app = Router::new().route(
        PATH,
        post(move || {
            let state = state.clone();
            async move {
                if state.fetch_add(1, Ordering::SeqCst) == 0 {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [
                            (header::RETRY_AFTER, "0"),
                            (header::CONTENT_TYPE, "application/json"),
                        ],
                        "{\"error\":\"bootstrap-busy\"}",
                    )
                        .into_response()
                } else {
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        serde_json::to_vec(&Reply::Missing).unwrap(),
                    )
                        .into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = Client::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    assert!(matches!(
        client
            .exchange(
                seed.genesis(),
                seed.bearer(),
                Operation::Status { bootstrap: [0; 32] }
            )
            .await
            .unwrap(),
        Reply::Missing
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    task.abort();
}

async fn serve(db: Database) -> (Client, tokio::task::JoinHandle<()>) {
    e2ee_http::issue_setup(&db).await;
    let app = router(db, Default::default());
    let (origin, task) = e2ee_http::serve(app, "127.0.0.1:0").await;
    (Client::new(&origin).unwrap(), task)
}

fn budget(package: &Package) -> staging::Budget {
    let chunks = components(package);
    staging::Budget {
        bytes: chunks
            .iter()
            .flat_map(|(_, c)| c)
            .map(|c| c.len() as u64)
            .sum(),
        chunks: chunks.iter().map(|(_, c)| c.len() as u64).sum(),
    }
}

#[tokio::test]
async fn claim_storage_error_after_commit_stays_ambiguous_and_exact_retry_succeeds() {
    let root = tempfile::tempdir().unwrap();
    let (_db, _store, seed, _package) = fixture(root.path()).await;
    let server = Database::open(&root.path().join("server.sqlite"))
        .await
        .unwrap();
    let (http, task) = serve(server.clone()).await;
    let setup_secret = Secret::new([7; 32]);
    http.claim(
        seed.genesis(),
        ClaimAuthentication::SetupSecret(&setup_secret),
    )
    .await
    .unwrap();
    // The reply to the committed claim was lost and the retry meets a locked
    // server database.
    let mut lock = aven_core::test_support::acquire(&server).await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *lock)
        .await
        .unwrap();
    for authentication in [
        ClaimAuthentication::SetupSecret(&setup_secret),
        ClaimAuthentication::SeedBearer(seed.bearer()),
    ] {
        let error = http
            .claim(seed.genesis(), authentication)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "error bootstrap-refused outcome-unknown");
    }
    sqlx::query("ROLLBACK").execute(&mut *lock).await.unwrap();
    drop(lock);
    http.claim(
        seed.genesis(),
        ClaimAuthentication::SeedBearer(seed.bearer()),
    )
    .await
    .unwrap();
    task.abort();
}

#[tokio::test]
async fn loopback_rejects_bad_authority_context_and_bytes_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    let (db, store, seed, package) = fixture(root.path()).await;
    let server = Database::open(&root.path().join("server.sqlite"))
        .await
        .unwrap();
    let (http, task) = serve(server.clone()).await;
    let rejected = http
        .claim(
            seed.genesis(),
            ClaimAuthentication::SetupSecret(&Secret::new([0; 32])),
        )
        .await
        .unwrap_err();
    assert_eq!(
        rejected.to_string(),
        "error bootstrap-setup-invitation-rejected"
    );
    // An otherwise valid setup credential with wrong context cannot claim.
    let bad = Envelope {
        vault: [0; 32],
        genesis: seed.genesis().commitment(),
        operation: Operation::ClaimSetup {
            bytes: seed.genesis().claim_bytes(),
        },
    };
    let response = http
        .http
        .post(http.endpoint.clone())
        .header(header::CONTENT_TYPE, "application/json")
        .bearer_auth(hex::encode([7; 32]))
        .body(serde_json::to_vec(&bad).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let mut conn = aven_core::test_support::acquire(&server).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM server_seed_claim")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(count, 0);
    drop(conn);
    assert!(
        http.claim(
            seed.genesis(),
            ClaimAuthentication::SeedBearer(seed.bearer())
        )
        .await
        .is_err()
    );
    http.claim(
        seed.genesis(),
        ClaimAuthentication::SetupSecret(&Secret::new([7; 32])),
    )
    .await
    .unwrap();
    http.claim(
        seed.genesis(),
        ClaimAuthentication::SetupSecret(&Secret::new([7; 32])),
    )
    .await
    .unwrap();
    http.claim(
        seed.genesis(),
        ClaimAuthentication::SeedBearer(seed.bearer()),
    )
    .await
    .unwrap();
    let intent = store.prepare_seed_adoption_intent(&db).await.unwrap();
    let signed = intent.publication(seed.genesis()).unwrap();
    let b = signed.binding();
    for secret in [Secret::new([0; 32]), Secret::new([7; 32])] {
        assert!(
            http.exchange(
                seed.genesis(),
                &secret,
                Operation::Declare {
                    descriptor: package.descriptor.clone(),
                    budget: budget(&package)
                }
            )
            .await
            .is_err()
        );
        assert!(
            http.exchange(
                seed.genesis(),
                &secret,
                Operation::Status {
                    bootstrap: b.bootstrap_id
                }
            )
            .await
            .is_err()
        );
    }
    assert!(matches!(
        http.exchange(
            seed.genesis(),
            seed.bearer(),
            Operation::Status {
                bootstrap: b.bootstrap_id
            }
        )
        .await
        .unwrap(),
        Reply::Missing
    ));
    let Reply::Staging(_) = http
        .exchange(
            seed.genesis(),
            seed.bearer(),
            Operation::Declare {
                descriptor: package.descriptor.clone(),
                budget: budget(&package),
            },
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    let status = || Operation::Status {
        bootstrap: b.bootstrap_id,
    };
    let before = serde_json::to_vec(
        &http
            .exchange(seed.genesis(), seed.bearer(), status())
            .await
            .unwrap(),
    )
    .unwrap();
    let publish = || Operation::Publish {
        bootstrap: b.bootstrap_id,
        commitment: b.descriptor_commitment,
        record: signed.record().to_vec(),
    };
    assert!(
        http.exchange(seed.genesis(), seed.bearer(), publish())
            .await
            .is_err()
    ); // incomplete catalogs/artifacts
    let put = |bytes: Vec<u8>| {
        let header = batch::Header {
            vault: seed.genesis().context().vault_id,
            genesis: seed.genesis().commitment(),
            bootstrap: b.bootstrap_id,
            commitment: b.descriptor_commitment,
            records: vec![batch::Slot {
                component: staging::Component::DataCatalog,
                index: 0,
                len: bytes.len() as u64,
            }],
        };
        (header, bytes)
    };
    let mut changed = package.descriptor.clone();
    changed[0] ^= 1;
    assert!(
        http.exchange(
            seed.genesis(),
            seed.bearer(),
            Operation::Declare {
                descriptor: changed,
                budget: budget(&package)
            }
        )
        .await
        .is_err()
    );
    for (bytes, status, code) in [
        (
            b"{\"invalid\":true}".to_vec(),
            StatusCode::BAD_REQUEST,
            "bootstrap-malformed",
        ),
        (
            vec![b' '; REQUEST_LIMIT + 1],
            StatusCode::PAYLOAD_TOO_LARGE,
            "bootstrap-limit",
        ),
    ] {
        let response = http
            .http
            .post(http.endpoint.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .bearer_auth(hex::encode(seed.bearer().expose()))
            .body(bytes)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(
            response.text().await.unwrap(),
            format!("{{\"error\":\"{code}\"}}")
        );
    }
    assert_eq!(
        before,
        serde_json::to_vec(
            &http
                .exchange(seed.genesis(), seed.bearer(), status())
                .await
                .unwrap()
        )
        .unwrap()
    );
    let (header, bytes) = put(package.catalogs[0].clone());
    http.put_batch(seed.bearer(), &header, &[&bytes])
        .await
        .unwrap();
    let before = serde_json::to_vec(
        &http
            .exchange(seed.genesis(), seed.bearer(), status())
            .await
            .unwrap(),
    )
    .unwrap();
    let mut changed = package.catalogs[0].clone();
    changed[0] ^= 1;
    let (header, changed) = put(changed);
    assert!(
        http.put_batch(seed.bearer(), &header, &[&changed])
            .await
            .is_err()
    );
    assert_eq!(
        before,
        serde_json::to_vec(
            &http
                .exchange(seed.genesis(), seed.bearer(), status())
                .await
                .unwrap()
        )
        .unwrap()
    );
    assert!(http.resume(&store, &db).await.unwrap());
    assert!(!http.resume(&store, &db).await.unwrap());
    let claimed = http
        .claim(
            seed.genesis(),
            ClaimAuthentication::SetupSecret(&Secret::new([7; 32])),
        )
        .await
        .unwrap_err();
    assert_eq!(
        claimed.to_string(),
        "error bootstrap-storage-already-claimed"
    );
    let Reply::Published(retried) = http
        .exchange(seed.genesis(), seed.bearer(), publish())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(retried, signed.record().as_slice());
    let Reply::Published(canceled) = http
        .exchange(
            seed.genesis(),
            seed.bearer(),
            Operation::Cancel {
                bootstrap: b.bootstrap_id,
            },
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(retried, canceled);
    assert!(
        http.exchange(seed.genesis(), &Secret::new([7; 32]), status())
            .await
            .is_err()
    );
    assert_eq!(
        http.http
            .post(http.endpoint.join("/sync").unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    task.abort();
}

struct ProcessServer {
    child: Child,
    origin: String,
}
impl Drop for ProcessServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn process_server(root: &Path, fault: &str) -> ProcessServer {
    let ready = root.join("ready");
    let _ = std::fs::remove_file(&ready);
    let log = std::fs::File::create(root.join(format!("server-{fault}.log"))).unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "seed_bootstrap_http::tests::server_worker",
            "--nocapture",
        ])
        .env("AVEN_HTTP_TEST_ROOT", root)
        .env("AVEN_HTTP_TEST_FAULT", fault)
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let mut server = ProcessServer {
        child,
        origin: String::new(),
    };
    for _ in 0..500 {
        if let Ok(origin) = std::fs::read_to_string(&ready) {
            server.origin = origin;
            return server;
        }
        assert!(
            server.child.try_wait().unwrap().is_none(),
            "server exited before listening"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not listen");
}

fn process_client(root: &Path, origin: &str, expected: &str) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "seed_bootstrap_http::tests::client_worker",
            "--nocapture",
        ])
        .env("AVEN_HTTP_TEST_ROOT", root)
        .env("AVEN_HTTP_TEST_ORIGIN", origin)
        .env("AVEN_HTTP_TEST_EXPECTED", expected)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn loopback_process_restart_recovers_exact_upload_and_remote_commit_before_adoption() {
    let root = tempfile::tempdir().unwrap();
    let (db, store, seed, package) = fixture(root.path()).await;
    let server = process_server(root.path(), "put").await;
    let http = Client::new(&server.origin).unwrap();
    http.claim(
        seed.genesis(),
        ClaimAuthentication::SetupSecret(&Secret::new([7; 32])),
    )
    .await
    .unwrap();
    drop(http);
    drop(store);
    drop(db);
    // Server exits after the second binary batch commits, before its response.
    process_client(root.path(), &server.origin, "uncertain");
    drop(server);
    let db = Database::open(&root.path().join("client.sqlite"))
        .await
        .unwrap();
    let store = isolated_store(&db, &root.path().join("keys")).await;
    let intent = store.prepare_seed_adoption_intent(&db).await.unwrap();
    let exact_intent = intent.protected_storage_bytes().to_vec();
    let signed = intent.publication(seed.genesis()).unwrap();
    let b = signed.binding();
    assert!(
        db.cancel_local_shared_state_never_dispatched(&hex::encode(b.bootstrap_id))
            .await
            .is_err()
    );
    let (_, _, reloaded) = store.seed_http_inputs(&db).await.unwrap();
    assert!(reloaded.unwrap() == package);
    drop(store);
    drop(db);
    let server_db = Database::open(&root.path().join("server.sqlite"))
        .await
        .unwrap();
    let mut conn = aven_core::test_support::acquire(&server_db).await.unwrap();
    let staged: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM server_bootstrap_chunks")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(staged, package.catalogs[0]);
    drop(conn);
    drop(server_db);
    // Both client and server are new processes. Already committed upload bytes
    // are retried exactly; server now exits after publication before response.
    let server = process_server(root.path(), "publish").await;
    process_client(root.path(), &server.origin, "uncertain");
    drop(server);
    let server_db = Database::open(&root.path().join("server.sqlite"))
        .await
        .unwrap();
    let mut conn = aven_core::test_support::acquire(&server_db).await.unwrap();
    let remote: Vec<u8> =
        sqlx::query_scalar("SELECT signed_record FROM server_bootstrap_publication")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(remote, signed.record().as_slice());
    let candidates: i64 = sqlx::query_scalar("SELECT count(*) FROM server_bootstrap_candidates")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(candidates, 1);
    drop(conn);
    drop(server_db);
    let db = Database::open(&root.path().join("client.sqlite"))
        .await
        .unwrap();
    let store = isolated_store(&db, &root.path().join("keys")).await;
    assert_eq!(
        store
            .prepare_seed_adoption_intent(&db)
            .await
            .unwrap()
            .protected_storage_bytes(),
        exact_intent
    );
    let before = db.export_data("fixed".into()).await.unwrap();
    assert!(before.tables.changes.iter().all(|c| c.server_seq.is_none()));
    let pending: Vec<_> = before
        .tables
        .changes
        .iter()
        .filter(|c| c.payload.contains("AFTER-CAPTURE"))
        .map(|c| c.change_id.clone())
        .collect();
    assert!(!pending.is_empty());
    drop(store);
    drop(db);
    let server = process_server(root.path(), "none").await;
    process_client(root.path(), &server.origin, "adopted");
    process_client(root.path(), &server.origin, "already-adopted");
    let db = Database::open(&root.path().join("client.sqlite"))
        .await
        .unwrap();
    let after = db.export_data("fixed".into()).await.unwrap();
    assert_eq!(
        serde_json::to_value(&before.tables.tasks).unwrap(),
        serde_json::to_value(&after.tables.tasks).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&before.tables.task_attachments).unwrap(),
        serde_json::to_value(&after.tables.task_attachments).unwrap()
    );
    assert!(
        after
            .tables
            .changes
            .iter()
            .filter(|c| pending.contains(&c.change_id))
            .all(|c| c.server_seq.is_none())
    );
    assert_eq!(
        after
            .tables
            .changes
            .iter()
            .filter(|c| c.server_seq.is_some())
            .count() as u64,
        b.prefix_count
    );
    let mut conn = aven_core::test_support::acquire(&db).await.unwrap();
    let (state, generation): (String, i64) =
        sqlx::query_as("SELECT state, association_generation FROM local_seed_publication_intent")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(state, "adopted");
    assert!(generation > 0);
    let pins: i64 = sqlx::query_scalar("SELECT count(*) FROM local_shared_capture_journal")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(pins, 0);
    drop(conn);
    let server_db = Database::open(&root.path().join("server.sqlite"))
        .await
        .unwrap();
    let mut conn = aven_core::test_support::acquire(&server_db).await.unwrap();
    let records: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT signed_record FROM server_bootstrap_publication")
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    assert_eq!(records, vec![signed.record().to_vec()]);
    let high: i64 = sqlx::query_scalar("SELECT high_water FROM server_e2ee_allocator")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(high as u64, b.prefix_count);
    let image: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM server_e2ee_image_chunks")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(image, package.images[0].records[0]);
    drop(conn);
    let http = Client::new(&server.origin).unwrap();
    let Reply::Published(record) = http
        .exchange(
            seed.genesis(),
            seed.bearer(),
            Operation::Publish {
                bootstrap: b.bootstrap_id,
                commitment: b.descriptor_commitment,
                record: signed.record().to_vec(),
            },
        )
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(record, signed.record().as_slice());
}

#[tokio::test]
#[ignore = "subprocess worker invoked by loopback restart test"]
async fn server_worker() {
    let root = PathBuf::from(std::env::var_os("AVEN_HTTP_TEST_ROOT").unwrap());
    let fault = std::env::var("AVEN_HTTP_TEST_FAULT").unwrap();
    let database = Database::open(&root.join("server.sqlite")).await.unwrap();
    e2ee_http::issue_setup(&database).await;
    let batches = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = router(database, Default::default()).layer(axum::middleware::from_fn(
        move |request: Request, next: axum::middleware::Next| {
            let fault = fault.clone();
            let batches = batches.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = to_bytes(body, batch::MAX_BYTES).await.unwrap();
                let should_exit = match serde_json::from_slice::<Envelope>(&bytes) {
                    Ok(envelope) => {
                        fault == "publish"
                            && matches!(envelope.operation, Operation::Publish { .. })
                    }
                    Err(_) => {
                        fault == "put"
                            && batches.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1
                    }
                };
                let response = next
                    .run(Request::from_parts(parts, axum::body::Body::from(bytes)))
                    .await;
                if should_exit && response.status() == StatusCode::OK {
                    std::process::exit(0);
                }
                response
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ready_tmp = root.join("ready.tmp");
    std::fs::write(
        &ready_tmp,
        format!("http://{}", listener.local_addr().unwrap()),
    )
    .unwrap();
    // The parent treats any readable ready file as a complete origin.
    std::fs::rename(ready_tmp, root.join("ready")).unwrap();
    axum::serve(listener, app).await.unwrap();
}

#[tokio::test]
#[ignore = "subprocess worker invoked by loopback restart test"]
async fn client_worker() {
    let root = PathBuf::from(std::env::var_os("AVEN_HTTP_TEST_ROOT").unwrap());
    let db = Database::open(&root.join("client.sqlite")).await.unwrap();
    let store = isolated_store(&db, &root.join("keys")).await;
    let result = Client::new(&std::env::var("AVEN_HTTP_TEST_ORIGIN").unwrap())
        .unwrap()
        .resume(&store, &db)
        .await;
    match std::env::var("AVEN_HTTP_TEST_EXPECTED").unwrap().as_str() {
        "uncertain" => assert!(result.is_err()),
        "adopted" => assert!(result.unwrap()),
        "already-adopted" => assert!(!result.unwrap()),
        _ => panic!(),
    }
}

#[tokio::test]
async fn invalid_http_outcome_preserves_sealed_intent_and_capture_until_verified_retry() {
    let root = tempfile::tempdir().unwrap();
    let (db, store, seed, _) = fixture(root.path()).await;
    let server = Database::open(&root.path().join("server.sqlite"))
        .await
        .unwrap();
    e2ee_http::issue_setup(&server).await;
    let app = router(server.clone(), Default::default()).layer(axum::middleware::from_fn(
        |request: Request, next: axum::middleware::Next| async move {
            let response = next.run(request).await;
            let (parts, body) = response.into_parts();
            let bytes = to_bytes(body, RESPONSE_LIMIT).await.unwrap();
            let replacement = match serde_json::from_slice::<Reply>(&bytes) {
                Ok(Reply::Published(mut record)) => {
                    *record.last_mut().unwrap() ^= 1;
                    serde_json::to_vec(&Reply::Published(record)).unwrap()
                }
                _ => bytes.to_vec(),
            };
            Response::from_parts(parts, axum::body::Body::from(replacement))
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Client::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    http.claim(
        seed.genesis(),
        ClaimAuthentication::SetupSecret(&Secret::new([7; 32])),
    )
    .await
    .unwrap();
    assert!(http.resume(&store, &db).await.is_err());
    let mut conn = aven_core::test_support::acquire(&db).await.unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM local_seed_publication_intent")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(state, "sealed");
    let captured: i64 = sqlx::query_scalar("SELECT count(*) FROM local_shared_capture_journal")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(captured, 1);
    drop(conn);
    assert!(
        db.export_data("fixed".into())
            .await
            .unwrap()
            .tables
            .changes
            .iter()
            .all(|c| c.server_seq.is_none())
    );
    let intent = store
        .prepare_seed_adoption_intent(&db)
        .await
        .unwrap()
        .protected_storage_bytes()
        .to_vec();
    task.abort();
    let (http, task) = serve(server).await;
    assert!(http.resume(&store, &db).await.unwrap());
    assert_eq!(
        store
            .prepare_seed_adoption_intent(&db)
            .await
            .unwrap()
            .protected_storage_bytes(),
        intent
    );
    task.abort();
}

#[tokio::test]
async fn unpublished_resume_replays_all_verified_slots_in_bounded_batches() {
    let root = tempfile::tempdir().unwrap();
    let (db, store, seed, package) = fixture(root.path()).await;
    let expected_bytes = budget(&package).bytes;
    let expected_chunks = budget(&package).chunks;
    let intent = store.prepare_seed_adoption_intent(&db).await.unwrap();
    let binding = *intent.publication(seed.genesis()).unwrap().binding();
    let expected: Vec<_> = components(&package)
        .into_iter()
        .flat_map(|(component, records)| {
            records.into_iter().enumerate().map(move |(index, record)| {
                (
                    batch::Slot {
                        component,
                        index: index as u64,
                        len: record.len() as u64,
                    },
                    record.to_vec(),
                )
            })
        })
        .collect();
    let server = Database::open(&root.path().join("server.sqlite"))
        .await
        .unwrap();
    e2ee_http::issue_setup(&server).await;
    let app = router(server.clone(), Default::default()).layer(axum::middleware::from_fn(
        |request: Request, next: axum::middleware::Next| async move {
            let (parts, body) = request.into_parts();
            let bytes = to_bytes(body, batch::MAX_BYTES).await.unwrap();
            if serde_json::from_slice::<Envelope>(&bytes)
                .is_ok_and(|envelope| matches!(envelope.operation, Operation::Publish { .. }))
            {
                return axum::response::IntoResponse::into_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                );
            }
            next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
                .await
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let first_http = Client::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let first_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    first_http
        .claim(
            seed.genesis(),
            ClaimAuthentication::SetupSecret(&Secret::new([7; 32])),
        )
        .await
        .unwrap();
    let first_progress = std::sync::Mutex::new(Vec::new());
    let record_first = |done, total| first_progress.lock().unwrap().push((done, total));
    assert!(
        first_http
            .resume_reporting(&store, &db, &record_first)
            .await
            .is_err()
    );
    let first_progress = first_progress.into_inner().unwrap();
    assert_eq!(first_progress.first(), Some(&(0, expected_bytes)));
    assert_eq!(
        first_progress.last(),
        Some(&(expected_bytes, expected_bytes))
    );

    let mut conn = aven_core::test_support::acquire(&server).await.unwrap();
    let (held_bytes, held_chunks): (i64, i64) = sqlx::query_as(
        "SELECT coalesce(sum(length(bytes)), 0), count(*) FROM server_bootstrap_chunks",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    let publications: i64 = sqlx::query_scalar("SELECT count(*) FROM server_bootstrap_publication")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    assert_eq!(held_bytes as u64, expected_bytes);
    assert_eq!(held_chunks as u64, expected_chunks);
    assert_eq!(publications, 0);
    let Reply::Staging(status) = first_http
        .exchange(
            seed.genesis(),
            seed.bearer(),
            Operation::Status {
                bootstrap: binding.bootstrap_id,
            },
        )
        .await
        .unwrap()
    else {
        panic!("unpublished staging should remain resumable");
    };
    assert!(status.components.iter().all(|component| {
        component
            .chunks
            .iter()
            .all(|presence| *presence == staging::Presence::Verified)
    }));
    first_task.abort();

    let expected_vault = seed.genesis().context().vault_id;
    let expected_genesis = seed.genesis().commitment();
    let batches = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = batches.clone();
    let app = router(server, Default::default()).layer(axum::middleware::from_fn(
        move |request: Request, next: axum::middleware::Next| {
            let seen = seen.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = to_bytes(body, batch::MAX_BYTES).await.unwrap();
                let is_batch = parts
                    .headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    == Some(batch::CONTENT_TYPE);
                if !is_batch {
                    return next
                        .run(Request::from_parts(parts, axum::body::Body::from(bytes)))
                        .await;
                }
                let decoded = batch::decode(&bytes).expect("upload must remain binary batches");
                assert!(decoded.header.records.len() <= batch::MAX_RECORDS);
                assert!(
                    decoded
                        .header
                        .records
                        .iter()
                        .map(|slot| slot.len)
                        .sum::<u64>()
                        <= batch::MAX_PAYLOAD as u64
                );
                let catalog = decoded.header.records[0].component.is_catalog();
                assert!(
                    decoded
                        .header
                        .records
                        .iter()
                        .all(|slot| slot.component.is_catalog() == catalog)
                );
                assert_eq!(decoded.header.vault, expected_vault);
                assert_eq!(decoded.header.genesis, expected_genesis);
                assert_eq!(decoded.header.bootstrap, binding.bootstrap_id);
                assert_eq!(decoded.header.commitment, binding.descriptor_commitment);
                seen.lock().unwrap().push(
                    decoded
                        .header
                        .records
                        .iter()
                        .copied()
                        .zip(decoded.records.iter().map(|record| record.to_vec()))
                        .collect::<Vec<_>>(),
                );
                next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
                    .await
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Client::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let progress = std::sync::Mutex::new(Vec::new());
    let record = |done, total| progress.lock().unwrap().push((done, total));
    assert!(http.resume_reporting(&store, &db, &record).await.unwrap());
    task.abort();

    let observed_batches = std::mem::take(&mut *batches.lock().unwrap());
    let observed: Vec<_> = observed_batches.iter().flatten().cloned().collect();
    assert_eq!(observed, expected);
    assert_eq!(
        observed.iter().map(|(slot, _)| slot.len).sum::<u64>(),
        expected_bytes
    );
    assert_eq!(observed.len() as u64, expected_chunks);
    let mut dependent_records_started = false;
    for (slot, _) in &observed {
        if slot.component.is_catalog() {
            assert!(
                !dependent_records_started,
                "catalogs must be uploaded first"
            );
        } else {
            dependent_records_started = true;
        }
    }

    let progress = progress.into_inner().unwrap();
    assert_eq!(progress.len(), observed_batches.len() + 1);
    assert_eq!(progress.first(), Some(&(0, expected_bytes)));
    assert_eq!(progress.last(), Some(&(expected_bytes, expected_bytes)));
    for (reports, batch) in progress.windows(2).zip(&observed_batches) {
        let payload: u64 = batch.iter().map(|(slot, _)| slot.len).sum();
        assert_eq!(reports[1].0 - reports[0].0, payload);
        assert_eq!(reports[1].1, expected_bytes);
    }
}

#[tokio::test]
async fn inconsistent_staging_responses_fail_before_upload() {
    // Cover the descriptor-bearing Declare response, the initial Status, and
    // both identity and component-shape checks on Ensure.
    for fault in 0..4 {
        let root = tempfile::tempdir().unwrap();
        let (db, store, seed, package) = fixture(root.path()).await;
        let server = Database::open(&root.path().join("server.sqlite"))
            .await
            .unwrap();
        let (initial, initial_task) = serve(server.clone()).await;
        initial
            .claim(
                seed.genesis(),
                ClaimAuthentication::SetupSecret(&Secret::new([7; 32])),
            )
            .await
            .unwrap();
        if fault != 0 {
            initial
                .exchange(
                    seed.genesis(),
                    seed.bearer(),
                    Operation::Declare {
                        descriptor: package.descriptor.clone(),
                        budget: budget(&package),
                    },
                )
                .await
                .unwrap();
        }
        initial_task.abort();

        let app = router(server.clone(), Default::default()).layer(axum::middleware::from_fn(
            move |request: Request, next: axum::middleware::Next| async move {
                let (parts, body) = request.into_parts();
                let bytes = to_bytes(body, batch::MAX_BYTES).await.unwrap();
                let target =
                    serde_json::from_slice::<Envelope>(&bytes)
                        .ok()
                        .is_some_and(|envelope| match fault {
                            0 => matches!(&envelope.operation, Operation::Declare { .. }),
                            1 => matches!(&envelope.operation, Operation::Status { .. }),
                            2 | 3 => matches!(&envelope.operation, Operation::Ensure { .. }),
                            _ => unreachable!(),
                        });
                let response = next
                    .run(Request::from_parts(parts, axum::body::Body::from(bytes)))
                    .await;
                if !target {
                    return response;
                }
                let (mut parts, body) = response.into_parts();
                let bytes = to_bytes(body, RESPONSE_LIMIT).await.unwrap();
                let mut reply: Reply = serde_json::from_slice(&bytes).unwrap();
                let Reply::Staging(status) = &mut reply else {
                    panic!("staging operation must return a staging reply");
                };
                match fault {
                    0 => status.descriptor_commitment[0] ^= 1,
                    1 => status.stream_id[0] ^= 1,
                    2 => status.budget.bytes += 1,
                    3 => {
                        status.components.remove(0);
                    }
                    _ => unreachable!(),
                }
                parts.headers.remove(header::CONTENT_LENGTH);
                axum::response::Response::from_parts(
                    parts,
                    axum::body::Body::from(serde_json::to_vec(&reply).unwrap()),
                )
            },
        ));
        let (origin, task) = e2ee_http::serve(app, "127.0.0.1:0").await;
        let hostile = Client::new(&origin).unwrap();
        assert_eq!(
            hostile.resume(&store, &db).await.unwrap_err().to_string(),
            "error bootstrap-status-mismatch"
        );
        let mut conn = aven_core::test_support::acquire(&server).await.unwrap();
        let chunks: i64 = sqlx::query_scalar("SELECT count(*) FROM server_bootstrap_chunks")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(chunks, 0);
        drop(conn);
        task.abort();

        let (honest, task) = serve(server).await;
        assert!(honest.resume(&store, &db).await.unwrap());
        task.abort();
    }
}

#[tokio::test]
async fn corrupt_later_batch_keeps_partial_staging_unpublished_and_retryable() {
    let root = tempfile::tempdir().unwrap();
    let (db, store, seed, package) = fixture(root.path()).await;
    let (object, image_records) = components(&package)
        .into_iter()
        .find_map(|(component, records)| match component {
            staging::Component::Image(object) => Some((
                object,
                records.into_iter().map(<[u8]>::to_vec).collect::<Vec<_>>(),
            )),
            _ => None,
        })
        .unwrap();
    assert!(!image_records.is_empty());
    let intent = store.prepare_seed_adoption_intent(&db).await.unwrap();
    let sealed_intent = intent.protected_storage_bytes().to_vec();
    let server = Database::open(&root.path().join("server.sqlite"))
        .await
        .unwrap();
    let (initial, initial_task) = serve(server.clone()).await;
    initial
        .claim(
            seed.genesis(),
            ClaimAuthentication::SetupSecret(&Secret::new([7; 32])),
        )
        .await
        .unwrap();
    initial_task.abort();

    let corrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let did_corrupt = corrupted.clone();
    let local = db.clone();
    let app = router(server.clone(), Default::default()).layer(axum::middleware::from_fn(
        move |request: Request, next: axum::middleware::Next| {
            let did_corrupt = did_corrupt.clone();
            let local = local.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = to_bytes(body, batch::MAX_BYTES).await.unwrap();
                let catalogs = batch::decode(&bytes).ok().is_some_and(|batch| {
                    batch
                        .header
                        .records
                        .iter()
                        .all(|slot| slot.component.is_catalog())
                });
                let response = next
                    .run(Request::from_parts(parts, axum::body::Body::from(bytes)))
                    .await;
                if catalogs
                    && response.status() == StatusCode::OK
                    && !did_corrupt.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    let mut conn = aven_core::test_support::acquire(&local).await.unwrap();
                    sqlx::query(
                        "UPDATE local_shared_capture_package_records
                         SET record = zeroblob(length(record))
                         WHERE component = 'image' AND object_id = ? AND chunk_index = 0",
                    )
                    .bind(object.as_slice())
                    .execute(&mut *conn)
                    .await
                    .unwrap();
                }
                response
            }
        },
    ));
    let (origin, task) = e2ee_http::serve(app, "127.0.0.1:0").await;
    let http = Client::new(&origin).unwrap();
    let error = http.resume(&store, &db).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("seed-package-mismatch"),
        "{error:#}"
    );
    assert!(corrupted.load(std::sync::atomic::Ordering::SeqCst));
    task.abort();

    let (intent_bytes, state) = db.seed_publication_intent_bytes().await.unwrap().unwrap();
    assert_eq!(intent_bytes, sealed_intent);
    assert_eq!(state, "sealed");
    let mut conn = aven_core::test_support::acquire(&db).await.unwrap();
    let captures: i64 = sqlx::query_scalar("SELECT count(*) FROM local_shared_capture_journal")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let associated: i64 =
        sqlx::query_scalar("SELECT count(*) FROM changes WHERE server_seq IS NOT NULL")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(captures, 1);
    assert_eq!(associated, 0);
    drop(conn);

    let component = std::iter::once(5).chain(object).collect::<Vec<_>>();
    let mut conn = aven_core::test_support::acquire(&server).await.unwrap();
    let (staged, publications): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM server_bootstrap_chunks),
                (SELECT count(*) FROM server_bootstrap_publication)",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    let target_staged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM server_bootstrap_chunks
         WHERE component = ? AND chunk_index = 0",
    )
    .bind(&component)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert!(staged > 0, "earlier catalog batch should remain staged");
    assert_eq!(publications, 0, "corruption must prevent publication");
    assert_eq!(target_staged, 0, "corrupt record must not be sent");
    drop(conn);

    let mut conn = aven_core::test_support::acquire(&db).await.unwrap();
    sqlx::query(
        "UPDATE local_shared_capture_package_records SET record = ?
         WHERE component = 'image' AND object_id = ? AND chunk_index = 0",
    )
    .bind(&image_records[0])
    .bind(object.as_slice())
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let (retry, retry_task) = serve(server).await;
    assert!(retry.resume(&store, &db).await.unwrap());
    retry_task.abort();
}

#[tokio::test]
async fn client_bounds_responses_and_rejects_redirects_and_unsafe_origins() {
    assert!(Client::new("http://100.100.20.30:3746").is_ok());
    assert!(Client::new("http://sync.private.example:3746").is_ok());
    for origin in [
        "ftp://remote.example",
        "https://user:secret@example.com",
        "https://example.com/private",
        "https://example.com/?token=secret",
    ] {
        assert_eq!(
            Client::new(origin).err().unwrap().to_string(),
            "error bootstrap-origin"
        );
    }
    let root = tempfile::tempdir().unwrap();
    let (_, _, seed, _) = fixture(root.path()).await;
    for (status, body, message) in [
        (
            StatusCode::OK,
            vec![b' '; RESPONSE_LIMIT + 1],
            "error bootstrap-response-limit",
        ),
        (
            StatusCode::OK,
            b"PRIVATE-NOT-JSON".to_vec(),
            "error bootstrap-response",
        ),
        (
            StatusCode::TEMPORARY_REDIRECT,
            Vec::new(),
            "error bootstrap-refused outcome-unknown",
        ),
    ] {
        let app = Router::new().route(
            PATH,
            post(move || async move {
                (
                    status,
                    [
                        (header::CONTENT_TYPE, "application/json"),
                        (header::LOCATION, "http://127.0.0.1:1/secret"),
                    ],
                    body,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Client::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let error = http
            .claim(
                seed.genesis(),
                ClaimAuthentication::SetupSecret(&Secret::new([7; 32])),
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), message);
        task.abort();
    }
}
