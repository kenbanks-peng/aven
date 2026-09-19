mod common;

use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::json;

use common::{
    TestEnv, TestProcess, TestServer, contains_all, contains_none, extract_attachment_id,
    extract_ref, fail, ok, png_bytes, scalar_i64,
};

const MAX_PUSH_BATCH: usize = 256;
const DAEMON_SYNC_PAGE_BUDGET: usize = 8;
const DEFAULT_WORKSPACE_ID: &str = "0000000000000000";
const APP_PROJECT_ID: &str = "APP0000000000000";

fn exec_sql(db: &Path, sql: &str) {
    let output = Command::new("sqlite3")
        .arg(db)
        .arg(sql)
        .output()
        .expect("run sqlite");
    assert!(
        output.status.success(),
        "sqlite failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn seed_budgeted_local_backlog(db: &Path, count: usize) {
    exec_sql(
        db,
        &format!(
            "INSERT OR IGNORE INTO projects(id, workspace_id, key, name, prefix, created_at, updated_at)
             VALUES ('{APP_PROJECT_ID}', '{DEFAULT_WORKSPACE_ID}', 'app', 'app', 'APP',
                     '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"
        ),
    );

    for start in (1..=count).step_by(128) {
        let end = (start + 127).min(count);
        let task_values = (start..=end)
            .map(|seq| {
                format!(
                    "('{DEFAULT_WORKSPACE_ID}', 'TSK{seq:013}', 'budgeted daemon task {seq}', '', '{APP_PROJECT_ID}', 'inbox', 'none', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')"
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let change_values = (start..=end)
            .map(|seq| {
                let payload = json!({
                    "workspace_id": DEFAULT_WORKSPACE_ID,
                    "workspace_key": "default",
                    "title": format!("budgeted daemon task {seq}"),
                    "description": "",
                    "project_id": APP_PROJECT_ID,
                    "project_key": "app",
                    "project_name": "app",
                    "project_prefix": "APP",
                    "status": "inbox",
                    "priority": "none",
                    "created_at": "2026-01-01T00:00:00Z"
                })
                .to_string()
                .replace('\'', "''");
                format!(
                    "('CHG{seq:013}', 'client-a', {seq}, 'task', 'TSK{seq:013}', NULL, 'create_task', '{payload}', NULL, '2026-01-01T00:00:00Z', NULL)"
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        exec_sql(
            db,
            &format!(
                "BEGIN;
                 INSERT INTO tasks(workspace_id, id, title, description, project_id, status, priority, created_at, updated_at, queue_activity_at) VALUES {task_values};
                 INSERT INTO changes(change_id, client_id, local_seq, entity_type, entity_id, field, op_type, payload, base_version, created_at, server_seq) VALUES {change_values};
                 COMMIT;"
            ),
        );
    }
}

#[test]
fn daemon_reports_startup_configuration_errors() {
    let env = TestEnv::new();
    env.write_config(
        r#"
sync:
  enabled: false
  server_url: "http://127.0.0.1:9"
"#,
    );
    let db = env.db("daemon.sqlite");
    contains_all(&fail(env.aven(&db, ["daemon"])), &["error sync-disabled"]);

    let env = TestEnv::new();
    env.write_config(
        r#"
sync:
  enabled: true
"#,
    );
    let db = env.db("daemon.sqlite");
    contains_all(
        &fail(env.aven(&db, ["daemon"])),
        &["error sync-server-required"],
    );

    let env = TestEnv::new();
    env.write_config(
        r#"
sync:
  enabled: true
  server_url: "http://127.0.0.1:9"

daemon:
  wake_addr: "not-an-address"
"#,
    );
    let db = env.db("daemon.sqlite");
    contains_all(
        &fail(env.aven(&db, ["daemon"])),
        &["invalid daemon wake address"],
    );

    let env = TestEnv::new();
    env.write_config(
        r#"
sync:
  enabled: true
  server_url: "http://127.0.0.1:9"

daemon:
  wake_addr: "0.0.0.0:47631"
"#,
    );
    let db = env.db("daemon.sqlite");
    contains_all(
        &fail(env.aven(&db, ["daemon"])),
        &["error daemon-wake-requires-loopback"],
    );
}

#[test]
fn daemon_refuses_wake_port_that_is_already_bound() {
    let env = TestEnv::new();
    let db = env.db("daemon.sqlite");
    let wake_addr = env.free_loopback_addr();
    let _socket = UdpSocket::bind(&wake_addr).expect("bind wake addr");
    env.write_config(&format!(
        r#"
local:
  db_path: "{}"

sync:
  enabled: true
  server_url: "http://127.0.0.1:9"

daemon:
  wake_addr: "{}"
"#,
        db.display(),
        wake_addr
    ));

    let error = fail(env.aven_config(["daemon"]));
    contains_all(
        &error,
        &[
            "could not bind daemon wake address",
            "is another daemon running",
        ],
    );
}

#[test]
fn daemon_wake_syncs_representative_mutations() {
    let env = TestEnv::new();
    let server = TestServer::start(&env);
    let client_a = env.db("client-a.sqlite");
    let client_b = env.db("client-b.sqlite");
    let wake_addr = env.free_loopback_addr();
    env.write_daemon_config(&client_a, &server, &wake_addr, 3600);

    let daemon = TestProcess::start_daemon(&env);
    daemon.wait_for_log("daemon-synced", Duration::from_secs(5));

    let mark = daemon.log_mark();
    ok(env.aven_config(["label", "create", "sync"]));
    let task_ref = extract_ref(&ok(env.aven_config([
        "add",
        "wake synced task",
        "--project",
        "app",
        "--label",
        "sync",
    ])));
    daemon.wait_for_log_after(mark, "daemon-synced", Duration::from_secs(5));

    let mark = daemon.log_mark();
    ok(env.aven_config(["edit", &task_ref, "--status", "active"]));
    ok(env.aven_config_stdin(["note", &task_ref, "--stdin"], "wake note\n"));
    ok(env.aven_config(["delete", &task_ref]));
    ok(env.aven_config(["restore", &task_ref]));
    daemon.wait_for_log_after(mark, "daemon-synced", Duration::from_secs(5));

    ok(env.aven(&client_b, ["sync", "--server", &server.url]));
    let shown = ok(env.aven(&client_b, ["show", &task_ref, "--full"]));
    contains_all(
        &shown,
        &[&task_ref, "wake synced task", "status=active", "wake note"],
    );
    contains_none(&shown, &["deleted=yes"]);
}

#[test]
fn daemon_attachment_maintenance_prunes_grace_expired_attachment() {
    let env = TestEnv::new();
    let server = TestServer::start(&env);
    let db = env.db("daemon-prune.sqlite");
    let wake_addr = env.free_loopback_addr();
    let task_ref = extract_ref(&ok(
        env.aven(&db, ["add", "daemon prune", "--project", "app"])
    ));
    let image = env.path("daemon-prune.png");
    std::fs::write(&image, png_bytes(2, 1)).unwrap();
    let attachment_id = extract_attachment_id(&ok(env.aven(
        &db,
        ["attachment", "add", &task_ref, image.to_str().unwrap()],
    )));
    let listed = ok(env.aven(&db, ["attachment", "list", &task_ref, "--json"]));
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    let sha256 = listed[0]["sha256"].as_str().unwrap();
    let mut blob_root = db.as_os_str().to_os_string();
    blob_root.push(".blobs");
    let blob_path = PathBuf::from(blob_root)
        .join("objects")
        .join("sha256")
        .join(sha256);
    assert!(blob_path.exists());
    ok(env.aven(&db, ["sync", "--server", &server.url]));
    ok(env.aven(&db, ["attachment", "delete", &attachment_id]));
    env.write_config(&format!(
        r#"
local:
  db_path: "{}"
  attachment_lifecycle:
    grace_days: 0
    maintenance_limit: 16

sync:
  enabled: true
  server_url: "{}"

daemon:
  wake_addr: "{}"
"#,
        db.display(),
        server.url,
        wake_addr
    ));

    let daemon = TestProcess::start_daemon(&env);
    daemon.wait_for_log("daemon-maintained", Duration::from_secs(5));

    let mark = daemon.log_mark();
    ok(env.aven(
        &db,
        ["add", "daemon maintenance follow-up", "--project", "app"],
    ));
    daemon.wait_for_log_after(mark, "daemon-synced", Duration::from_secs(5));
    let output = daemon.output();
    assert_eq!(output.matches("daemon-maintained").count(), 1, "{output}");
    contains_none(&output, &["daemon sync failed"]);
    assert!(!blob_path.exists());
}

#[test]
fn daemon_periodic_syncs_without_wake() {
    let env = TestEnv::new();
    let server = TestServer::start(&env);
    let client_a = env.db("client-a.sqlite");
    let client_b = env.db("client-b.sqlite");
    let wake_addr = env.free_loopback_addr();
    env.write_daemon_config(&client_a, &server, &wake_addr, 1);

    let daemon = TestProcess::start_daemon(&env);
    daemon.wait_for_log("daemon-synced", Duration::from_secs(5));
    let mark = daemon.log_mark();
    let task_ref = extract_ref(&ok(env.aven(
        &client_a,
        ["add", "periodic synced task", "--project", "app"],
    )));

    daemon.wait_for_log_after(mark, "daemon-synced", Duration::from_secs(5));
    ok(env.aven(&client_b, ["sync", "--server", &server.url]));
    let list = ok(env.aven(&client_b, ["list", "--all"]));
    contains_all(&list, &[&task_ref, "periodic synced task"]);
}

#[test]
fn daemon_syncs_large_backlog_across_budgeted_rounds() {
    let env = TestEnv::new();
    let server = TestServer::start(&env);
    let client_a = env.db("client-a.sqlite");
    let client_b = env.db("client-b.sqlite");
    let wake_addr = env.free_loopback_addr();
    env.write_daemon_config(&client_a, &server, &wake_addr, 3600);

    ok(env.aven_config(["list", "--all"]));
    let task_count = MAX_PUSH_BATCH * DAEMON_SYNC_PAGE_BUDGET + 1;
    seed_budgeted_local_backlog(&client_a, task_count);

    let daemon = TestProcess::start_daemon_with_env(&env, [("AVEN_LOG", "aven=debug")]);
    let first_pushed = MAX_PUSH_BATCH * DAEMON_SYNC_PAGE_BUDGET;
    let incomplete = format!(
        "daemon-synced pushed={first_pushed} pulled=0 blob_uploaded=0 blob_uploaded_bytes=0 blob_downloaded=0 blob_downloaded_bytes=0 blob_upload_remaining=0 blob_upload_remaining_bytes=0 blob_download_remaining=0 blob_download_remaining_bytes=0 cursor={first_pushed} complete=false pages={DAEMON_SYNC_PAGE_BUDGET}"
    );
    let complete = format!(
        "daemon-synced pushed=1 pulled=0 blob_uploaded=0 blob_uploaded_bytes=0 blob_downloaded=0 blob_downloaded_bytes=0 blob_upload_remaining=0 blob_upload_remaining_bytes=0 blob_download_remaining=0 blob_download_remaining_bytes=0 cursor={task_count} complete=true pages=1"
    );

    daemon.wait_for_log(&incomplete, Duration::from_secs(30));
    daemon.wait_for_log(&complete, Duration::from_secs(30));
    let output = daemon.output();
    assert!(
        output
            .find(&incomplete)
            .expect("incomplete daemon sync marker")
            < output.find(&complete).expect("complete daemon sync marker"),
        "daemon output should report incomplete work before completion\n{output}"
    );
    assert_eq!(
        scalar_i64(
            &client_a,
            "SELECT count(*) FROM changes WHERE server_seq IS NULL"
        ),
        0
    );

    ok(env.aven(&client_b, ["sync", "--server", &server.url]));
    assert_eq!(
        scalar_i64(
            &client_b,
            "SELECT count(*) FROM tasks WHERE title LIKE 'budgeted daemon task %'",
        ),
        task_count as i64
    );

    // Assert the daemon reused the same HTTP client across wake rounds
    let client_ids: Vec<&str> = output
        .lines()
        .filter(|line| line.contains("sync client starting") && line.contains("http_client_id="))
        .filter_map(|line| line.split("http_client_id=").nth(1))
        .filter_map(|value| {
            value
                .split_whitespace()
                .next()
                .or_else(|| value.split(',').next())
        })
        .collect();
    assert!(
        client_ids.len() >= 2,
        "expected at least 2 sync client starting events, got {}",
        client_ids.len()
    );
    let first = client_ids[0];
    for (i, id) in client_ids.iter().enumerate() {
        assert_eq!(
            *id, first,
            "http_client_id mismatch at index {i}: expected {first}, got {id}"
        );
    }
}

#[test]
fn daemon_wakes_respect_failure_deadlines_and_recover() {
    use aven_core::sync::wire::SYNC_PROTOCOL_VERSION;
    use axum::{Router, body::Bytes, http::StatusCode, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicU16, AtomicUsize, Ordering},
    };

    for initial_status in [400, 401] {
        let env = TestEnv::new();
        let server = TestServer::start(&env);
        let db = env.db("retry-deadline.sqlite");
        ok(env.aven(&db, ["add", "pending during failure", "--project", "app"]));
        let pending = scalar_i64(&db, "SELECT count(*) FROM changes WHERE server_seq IS NULL");
        let wake_addr = env.free_loopback_addr();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let status = Arc::new(AtomicU16::new(initial_status));
        let requests = Arc::new(AtomicUsize::new(0));
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let proxy_status = Arc::clone(&status);
        let proxy_requests = Arc::clone(&requests);
        let upstream = server.url.clone();
        let proxy = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let client = reqwest::Client::new();
                let app = Router::new().route(
                    "/sync",
                    post(move |headers: axum::http::HeaderMap, body: Bytes| {
                        let status = Arc::clone(&proxy_status);
                        let requests = Arc::clone(&proxy_requests);
                        let upstream = upstream.clone();
                        let client = client.clone();
                        async move {
                            requests.fetch_add(1, Ordering::SeqCst);
                            match status.load(Ordering::SeqCst) {
                                400 => (
                                    StatusCode::BAD_REQUEST,
                                    format!(
                                        "error sync-protocol-unsupported client={} server={}",
                                        SYNC_PROTOCOL_VERSION,
                                        SYNC_PROTOCOL_VERSION + 1
                                    ),
                                ),
                                401 => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
                                _ => {
                                    let mut request = client
                                        .post(format!("{upstream}/sync"))
                                        .header("content-type", "application/json");
                                    if let Some(encoding) = headers.get("content-encoding") {
                                        request = request.header("content-encoding", encoding);
                                    }
                                    let response = request.body(body).send().await.unwrap();
                                    (response.status(), response.text().await.unwrap())
                                }
                            }
                        }
                    }),
                );
                axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), app)
                    .with_graceful_shutdown(async {
                        let _ = stop_rx.await;
                    })
                    .await
                    .unwrap();
            });
        });
        // Dropping the sender also ends the proxy if a test assertion panics.
        let interval = if initial_status == 400 { 3 } else { 3600 };
        env.write_config(&format!(
            "local:\n  db_path: '{}'\nsync:\n  enabled: true\n  server_url: '{}'\n  interval_seconds: {}\ndaemon:\n  wake_addr: '{}'\n",
            db.display(), url, interval, wake_addr
        ));
        let daemon = TestProcess::start_daemon(&env);
        daemon.wait_for_log("daemon sync failed", Duration::from_secs(5));
        if initial_status == 401 {
            let mark = daemon.log_mark();
            daemon.wait_for_log_after(mark, "daemon sync failed", Duration::from_secs(5));
        }
        let attempts = requests.load(Ordering::SeqCst);
        assert_eq!(attempts, if initial_status == 400 { 1 } else { 2 });
        let wake = UdpSocket::bind("127.0.0.1:0").unwrap();
        for _ in 0..20 {
            wake.send_to(b"1", &wake_addr).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            attempts,
            "wake bypassed retry deadline"
        );
        assert_eq!(
            scalar_i64(&db, "SELECT count(*) FROM changes WHERE server_seq IS NULL"),
            pending
        );
        assert_eq!(common::meta_value(&db, "sync_cursor").as_deref(), Some("0"));
        if initial_status == 400 {
            assert_eq!(
                common::meta_value(&db, "sync_blocked_protocol"),
                Some((SYNC_PROTOCOL_VERSION + 1).to_string())
            );
        }
        status.store(0, Ordering::SeqCst);
        daemon.wait_for_log("daemon-synced", Duration::from_secs(8));
        assert_eq!(
            scalar_i64(&db, "SELECT count(*) FROM changes WHERE server_seq IS NULL"),
            0
        );
        assert_eq!(requests.load(Ordering::SeqCst), attempts + 2);
        assert_eq!(
            common::meta_value(&db, "sync_blocked_protocol").as_deref(),
            Some("")
        );
        if initial_status == 401 {
            let mark = daemon.log_mark();
            ok(env.aven_config(["add", "wake after recovery", "--project", "app"]));
            daemon.wait_for_log_after(mark, "daemon-synced", Duration::from_secs(5));
            assert_eq!(
                scalar_i64(&db, "SELECT count(*) FROM changes WHERE server_seq IS NULL"),
                0
            );
        }
        drop(daemon);
        let _ = stop_tx.send(());
        proxy.join().unwrap();
    }
}
