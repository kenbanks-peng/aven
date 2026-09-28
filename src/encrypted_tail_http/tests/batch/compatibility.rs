use super::*;
use aven_core::sync::encrypted_tail::{Operation, Reply};
use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::StatusCode,
    response::Response,
};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};
use tokio::process::{Child, Command};

const PRE_BATCH_REVISION: &str = "a2b7af024cc357483dac0301c38d27297d24ba72";

#[tokio::test]
async fn candidate_endpoint_negotiates_the_advertised_batch_limits() {
    let f = fixture().await;
    converge(&f).await;
    let inputs = f.peer_store.tail_inputs(&f.peer, &f.origin).await.unwrap();

    let Reply::Features(features) = Client::new(&f.origin)
        .unwrap()
        .exchange(
            &inputs.authority.context,
            &inputs.bearer,
            Operation::Features,
        )
        .await
        .unwrap()
    else {
        panic!("candidate endpoint did not return batch features");
    };

    assert_eq!(features.count, tail::BATCH_COUNT);
    assert_eq!(features.bytes, tail::BATCH_BYTES);
}

#[tokio::test]
async fn ambiguous_features_refusal_does_not_authorize_singleton_fallback() {
    let mut f = fixture().await;
    converge(&f).await;
    let state = Arc::new(Traffic {
        feature_refusal: Some((StatusCode::BAD_REQUEST, "encrypted-tail-refused")),
        ..Default::default()
    });
    instrument(&mut f, state.clone()).await;
    edits(&f, 2).await;

    let error = Client::new(&f.origin)
        .unwrap()
        .round(&f.peer_store, &f.peer, &blobs(&f.peer))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("outcome-unknown"));
    let requests = state.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|(_, body)| body["operation"] == "Features")
            .count(),
        1
    );
    assert!(
        !requests
            .iter()
            .any(|(_, body)| body["operation"].get("Append").is_some())
    );
    drop(requests);
    assert_eq!(
        scalar(&f.peer, "SELECT count(*) FROM local_e2ee_outbox").await,
        0
    );
}

/// Requires the harness-built test binary from the pinned pre-batch revision.
/// Unlike the middleware-based unit cases, this starts the actual old endpoint
/// and invokes the actual old core HTTP client in separate test processes.
#[tokio::test]
#[ignore = "run scripts/test-batch-compatibility for old/new binary evidence"]
async fn pre_batch_endpoint_and_client_interoperate_with_candidate() {
    let baseline_client_binary = baseline_binary("AVEN_BATCH_PREBATCH_CLIENT_TEST_BIN");
    let baseline_server_binary = baseline_binary("AVEN_BATCH_PREBATCH_SERVER_TEST_BIN");
    assert_eq!(
        std::env::var("AVEN_BATCH_PREBATCH_REVISION").unwrap_or_default(),
        PRE_BATCH_REVISION,
        "the compatibility harness must pin its old test binary to {PRE_BATCH_REVISION}"
    );

    // First direction: the old core client's unchanged v1 Append must be
    // accepted by the candidate endpoint. The candidate then resolves and
    // accepts the same durable operation without appending it twice.
    let mut candidate = fixture().await;
    converge(&candidate).await;
    edits(&candidate, 1).await;
    let (inputs, frozen_records) = frozen(&candidate).await;
    assert_eq!(frozen_records.len(), 1);
    let traffic = Arc::new(Traffic::default());
    instrument(&mut candidate, traffic.clone()).await;
    let before_old_append = scalar(
        &candidate.server,
        "SELECT high_water FROM server_e2ee_allocator",
    )
    .await;
    let old_client_input = candidate.root.path().join("old-client-input.json");
    fs::write(
        &old_client_input,
        serde_json::to_vec(&serde_json::json!({
            "origin": candidate.origin,
            "context": inputs.authority.context,
            "bearer": inputs.bearer.expose(),
            "record": frozen_records[0].1,
        }))
        .unwrap(),
    )
    .unwrap();
    let old_client = run_old_worker(
        &baseline_client_binary,
        "encrypted_tail_http::tests::batch_compatibility_old_client_worker",
        &[("AVEN_BATCH_CLIENT_INPUT", old_client_input.as_os_str())],
    )
    .await;
    assert_worker_succeeded(old_client, "old client -> candidate endpoint");
    {
        let requests = traffic.requests.lock().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|(path, body)| {
                    path == PATH && body["operation"].get("Append").is_some()
                })
                .count(),
            1
        );
        assert!(
            !requests
                .iter()
                .any(|(path, _)| { path == aven_core::sync::client::tail::BATCH_PATH })
        );
        assert!(
            !requests
                .iter()
                .any(|(_, body)| body["operation"] == "Features")
        );
    }
    let accepted_high_water = scalar(
        &candidate.server,
        "SELECT high_water FROM server_e2ee_allocator",
    )
    .await;
    assert_eq!(accepted_high_water, before_old_append + 1);
    drain(
        &Client::new(&candidate.origin).unwrap(),
        &candidate.peer_store,
        &candidate.peer,
    )
    .await;
    assert_eq!(
        scalar(
            &candidate.server,
            "SELECT high_water FROM server_e2ee_allocator",
        )
        .await,
        accepted_high_water
    );
    assert_eq!(
        scalar(&candidate.peer, "SELECT count(*) FROM local_e2ee_outbox").await,
        0
    );

    // Opposite direction: freeze multiple operations, then run the candidate
    // client against the old server implementation. The old endpoint's real
    // serde refusal for Features selects singleton mode; all frozen IDs must
    // be looked up before the first v1 Append.
    let mut legacy = fixture().await;
    converge(&legacy).await;
    edits(&legacy, 3).await;
    let (_, frozen_records) = frozen(&legacy).await;
    assert!(frozen_records.len() > 1);
    let before = scalar(
        &legacy.server,
        "SELECT high_water FROM server_e2ee_allocator",
    )
    .await;
    legacy.task.abort();
    let _ = (&mut legacy.task).await;
    // The old migrator rejects schema versions it does not recognize. The
    // server uses the tail tables, not this client-local outbox state, so drop
    // only the unknown-version marker on the test fixture.
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 20260928163800")
        .execute(
            &mut *aven_core::test_support::acquire(&legacy.server)
                .await
                .unwrap(),
        )
        .await
        .unwrap();
    let origin = legacy.origin.clone();
    let upstream_origin = reserve_local_origin().await;
    let worker_log = legacy.root.path().join("pre-batch-server.log");
    let _ = fs::remove_file(legacy.root.path().join("tail-server-ready"));
    let mut old_server = spawn_old_server(
        &baseline_server_binary,
        legacy.root.path(),
        &upstream_origin,
        &worker_log,
    )
    .await;
    let (proxy_trace, mut proxy_task) = start_proxy(&origin, &upstream_origin).await;

    let client = Client::new(&origin).unwrap();
    let rounds = tokio::time::timeout(Duration::from_secs(90), async {
        let mut round_error = None;
        let mut caught_up = false;
        for _ in 0..100 {
            match client
                .round(&legacy.peer_store, &legacy.peer, &blobs(&legacy.peer))
                .await
            {
                Ok(round) if round.metadata_caught_up => {
                    caught_up = true;
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    round_error = Some(error.to_string());
                    break;
                }
            }
        }
        (caught_up, round_error)
    })
    .await;
    stop_child(&mut old_server).await;
    proxy_task.abort();
    let _ = (&mut proxy_task).await;
    let (caught_up, round_error) = rounds.expect("timed out during old/new rounds");
    assert!(
        caught_up,
        "candidate client against pre-batch endpoint failed: {round_error:?}; server log: {}",
        fs::read_to_string(&worker_log).unwrap_or_default()
    );

    let events = proxy_trace.requests.lock().unwrap().clone();
    let tail_events: Vec<_> = events
        .iter()
        .filter(|event| event["path"] == PATH)
        .collect();
    let feature_refusal = tail_events
        .iter()
        .find(|event| event["request"]["operation"] == "Features")
        .expect("pre-batch endpoint must receive the Features probe");
    assert_eq!(feature_refusal["status"], 400);
    assert_eq!(
        feature_refusal["response"]["error"],
        "encrypted-tail-malformed"
    );

    let first_append = tail_events
        .iter()
        .position(|event| event["request"]["operation"].get("Append").is_some())
        .expect("candidate must use legacy singleton Append");
    let lookups_before_append: Vec<_> = tail_events[..first_append]
        .iter()
        .filter_map(|event| {
            event["request"]["operation"]["Lookup"]["operation_id"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    let expected_ids: Vec<_> = frozen_records
        .iter()
        .map(|(operation_id, _)| operation_id.clone())
        .collect();
    assert_eq!(lookups_before_append, expected_ids);
    assert!(
        events
            .iter()
            .all(|event| event["path"] != aven_core::sync::client::tail::BATCH_PATH)
    );
    assert_eq!(
        tail_events
            .iter()
            .filter(|event| event["request"]["operation"].get("Append").is_some())
            .count(),
        frozen_records.len()
    );
    assert_eq!(
        scalar(
            &legacy.server,
            "SELECT high_water FROM server_e2ee_allocator"
        )
        .await,
        before + frozen_records.len() as i64
    );
    assert_eq!(
        scalar(&legacy.peer, "SELECT count(*) FROM local_e2ee_outbox").await,
        0
    );
}

#[derive(Default)]
struct ProxyTrace {
    requests: std::sync::Mutex<Vec<Value>>,
}

struct Proxy {
    upstream: String,
    http: reqwest::Client,
    trace: Arc<ProxyTrace>,
}

async fn proxy_request(State(proxy): State<Arc<Proxy>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_owned())
        .unwrap_or_else(|| "/".to_owned());
    let (parts, body) = request.into_parts();
    let request_bytes = to_bytes(body, 8 * 1024 * 1024).await.unwrap();
    let request_json = serde_json::from_slice::<Value>(&request_bytes).unwrap_or(Value::Null);
    let url = format!("{}{}", proxy.upstream, path_and_query);
    let mut outgoing = proxy.http.request(
        reqwest::Method::from_bytes(parts.method.as_str().as_bytes()).unwrap(),
        url,
    );
    for (name, value) in &parts.headers {
        if name == axum::http::header::HOST
            || name == axum::http::header::CONNECTION
            || name == axum::http::header::TRANSFER_ENCODING
        {
            continue;
        }
        if let Ok(value) = value.to_str() {
            outgoing = outgoing.header(name.as_str(), value);
        }
    }
    let response = match outgoing.body(request_bytes.to_vec()).send().await {
        Ok(response) => response,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(axum::body::Body::from("old endpoint unavailable"))
                .unwrap();
        }
    };
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
    let response_headers = response.headers().clone();
    let response_bytes = response.bytes().await.unwrap();
    let response_json = serde_json::from_slice::<Value>(&response_bytes).unwrap_or(Value::Null);
    proxy
        .trace
        .requests
        .lock()
        .unwrap()
        .push(serde_json::json!({
            "path": path,
            "request": request_json,
            "status": status.as_u16(),
            "response": response_json,
        }));

    let mut forwarded = Response::builder()
        .status(status)
        .body(axum::body::Body::from(response_bytes))
        .unwrap();
    for (name, value) in &response_headers {
        if name == axum::http::header::CONNECTION || name == axum::http::header::TRANSFER_ENCODING {
            continue;
        }
        forwarded.headers_mut().insert(name.clone(), value.clone());
    }
    forwarded
}

async fn start_proxy(
    origin: &str,
    upstream: &str,
) -> (Arc<ProxyTrace>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(origin.strip_prefix("http://").unwrap())
        .await
        .unwrap();
    let trace = Arc::new(ProxyTrace::default());
    let state = Arc::new(Proxy {
        upstream: upstream.to_owned(),
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .build()
            .unwrap(),
        trace: trace.clone(),
    });
    let app = axum::Router::new()
        .fallback(axum::routing::any(proxy_request))
        .with_state(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (trace, task)
}

async fn reserve_local_origin() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}")
}

fn baseline_binary(variable: &str) -> PathBuf {
    let path = std::env::var_os(variable)
        .unwrap_or_else(|| panic!("set {variable} via scripts/test-batch-compatibility"));
    let path = PathBuf::from(path);
    assert!(path.is_file(), "pre-batch test binary missing: {path:?}");
    path
}

async fn run_old_worker(
    binary: &Path,
    test_name: &str,
    env: &[(&str, &std::ffi::OsStr)],
) -> Output {
    let mut command = Command::new(binary);
    command.args(["--exact", test_name, "--ignored", "--nocapture"]);
    for (key, value) in env {
        command.env(key, value);
    }
    command.kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("timed out waiting for pre-batch client worker")
        .unwrap()
}

fn assert_worker_succeeded(output: Output, description: &str) {
    assert!(
        output.status.success(),
        "{description} worker failed ({}):\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn spawn_old_server(binary: &Path, root: &Path, origin: &str, worker_log: &Path) -> Child {
    let log = fs::File::create(worker_log).unwrap();
    let error_log = log.try_clone().unwrap();
    let mut child = Command::new(binary)
        .args([
            "--exact",
            "encrypted_tail_http::tests::server_worker",
            "--ignored",
            "--nocapture",
        ])
        .env("AVEN_TAIL_ROOT", root)
        .env("AVEN_TAIL_ORIGIN", origin)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let ready = root.join("tail-server-ready");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if ready.exists() {
                return;
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "pre-batch server worker exited before ready ({status}): {}",
                    fs::read_to_string(worker_log).unwrap_or_default()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("timed out waiting for pre-batch server worker");
    child
}

async fn stop_child(child: &mut Child) {
    if child.try_wait().unwrap().is_none() {
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
}
