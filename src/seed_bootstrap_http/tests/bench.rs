//! Run each ignored sample in a fresh release-test process. The server is a
//! separate process; body counts include transmitted batch envelopes and
//! record bytes, not merely progress counters.
//!
//! `cargo test --release --lib seed_bootstrap_http::tests::bench::large_image_setup -- --ignored --exact --nocapture`
//! `AVEN_SETUP_MIB` defaults to 255; the aggregate image plaintext cap is
//! 256 MiB. Peak RSS is the client's lifetime high-water mark.
use super::*;
use aven_core::{
    operations::{AttachmentAddInput, TaskDraft},
    sync::{
        bootstrap_staging::{self, batch},
        client::{
            ClientHost, SetupInvitation,
            engine::{self, Amount, Progress, Stage},
            keys::{FileProtectedStorage, ProtectedStorage, StoreResult},
        },
        seed_claim::Secret,
    },
    test_support::writer_timing,
};
use axum::{
    body::{Body, to_bytes},
    extract::Request,
    http::header,
    middleware::Next,
};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const IMAGE_PLAINTEXT_CAP_BYTES: u64 = 256 * 1_048_576;
const MIB: u64 = 1_048_576;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Default)]
struct Traffic {
    requests: AtomicU64,
    request_body_bytes: AtomicU64,
    batch_body_bytes: AtomicU64,
    batch_record_bytes: AtomicU64,
    response_body_bytes: AtomicU64,
    trace: Mutex<Vec<String>>,
}

async fn count_traffic(
    traffic: Arc<Traffic>,
    metrics_path: PathBuf,
    trace_path: PathBuf,
    request: Request,
    next: Next,
) -> axum::response::Response {
    let path = request.uri().path().to_string();
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let is_batch = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        == Some(batch::CONTENT_TYPE);
    let (parts, body) = request.into_parts();
    let request_body = to_bytes(body, batch::MAX_BYTES).await.unwrap();
    let request_len = request_body.len() as u64;
    let batch_record_len = if is_batch {
        batch::decode(&request_body)
            .unwrap()
            .records
            .iter()
            .map(|record| record.len() as u64)
            .sum()
    } else {
        0
    };
    let request = Request::from_parts(parts, Body::from(request_body));
    let response = next.run(request).await;
    let (parts, body) = response.into_parts();
    let status = parts.status;
    let response_body = to_bytes(body, crate::seed_bootstrap_http::RESPONSE_LIMIT)
        .await
        .unwrap();
    let response_len = response_body.len() as u64;
    let response = axum::response::Response::from_parts(parts, Body::from(response_body));

    traffic.requests.fetch_add(1, Ordering::Relaxed);
    traffic
        .request_body_bytes
        .fetch_add(request_len, Ordering::Relaxed);
    if is_batch {
        traffic
            .batch_body_bytes
            .fetch_add(request_len, Ordering::Relaxed);
        traffic
            .batch_record_bytes
            .fetch_add(batch_record_len, Ordering::Relaxed);
    }
    traffic
        .response_body_bytes
        .fetch_add(response_len, Ordering::Relaxed);
    traffic.trace.lock().unwrap().push(format!(
        "path={path} content_type={content_type} status={} request_body_bytes={request_len} response_body_bytes={response_len}",
        status.as_u16(),
    ));
    std::fs::write(
        metrics_path,
        format!(
            "requests={}\nrequest_body_bytes={}\nbatch_body_bytes={}\nbatch_record_bytes={}\nresponse_body_bytes={}\n",
            traffic.requests.load(Ordering::Relaxed),
            traffic.request_body_bytes.load(Ordering::Relaxed),
            traffic.batch_body_bytes.load(Ordering::Relaxed),
            traffic.batch_record_bytes.load(Ordering::Relaxed),
            traffic.response_body_bytes.load(Ordering::Relaxed),
        ),
    )
    .unwrap();
    std::fs::write(trace_path, traffic.trace.lock().unwrap().join("\n")).unwrap();
    response
}

struct BenchHost {
    keys: PathBuf,
    blobs: PathBuf,
}

impl ClientHost for BenchHost {
    fn ensure_sync_allowed(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn protected_storage(&self) -> StoreResult<Arc<dyn ProtectedStorage>> {
        Ok(Arc::new(FileProtectedStorage::new(self.keys.clone())))
    }

    fn blob_dir(&self, _database: &Database) -> anyhow::Result<PathBuf> {
        Ok(self.blobs.clone())
    }

    fn device_label(&self) -> Option<String> {
        None
    }
}

fn peak_rss_bytes() -> u64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    let rss = unsafe { usage.assume_init() }.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        rss
    } else {
        rss * 1024
    }
}

fn value(metrics: &str, name: &str) -> u64 {
    metrics
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key == name).then(|| value.parse().unwrap())
        })
        .unwrap()
}

fn duration_total(samples: &[Duration]) -> Duration {
    samples.iter().sum()
}

fn duration_max(samples: &[Duration]) -> Duration {
    samples.iter().max().copied().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "large-image release resource baseline"]
async fn large_image_setup() {
    if let Ok(root) = std::env::var("AVEN_SETUP_SERVER_ROOT") {
        let root = Path::new(&root);
        let db = Database::open(&root.join("server.sqlite")).await.unwrap();
        crate::test_support::e2ee_http::issue_setup(&db).await;
        let traffic = Arc::new(Traffic::default());
        let metrics_path = root.join("traffic.txt");
        let trace_path = root.join("traffic-requests.txt");
        let app = crate::seed_bootstrap_http::router(db.clone(), Default::default())
            .merge(crate::peer_enrollment_http::router(db.clone()))
            .merge(crate::encrypted_tail_http::router(
                db,
                aven_core::attachments::LifecyclePolicy::default(),
            ))
            .layer(axum::middleware::from_fn({
                let traffic = traffic.clone();
                let metrics_path = metrics_path.clone();
                let trace_path = trace_path.clone();
                move |request, next| {
                    count_traffic(
                        traffic.clone(),
                        metrics_path.clone(),
                        trace_path.clone(),
                        request,
                        next,
                    )
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::fs::write(
            root.join("origin"),
            format!("http://{}", listener.local_addr().unwrap()),
        )
        .unwrap();
        axum::serve(listener, app).await.unwrap();
        return;
    }

    assert!(!cfg!(debug_assertions), "use --release for measurements");
    let mib: u64 = std::env::var("AVEN_SETUP_MIB")
        .map(|value| value.parse().unwrap())
        .unwrap_or(255);
    assert!((1..=255).contains(&mib));
    let root = tempfile::tempdir().unwrap();
    let db = Database::open(&root.path().join("client.sqlite"))
        .await
        .unwrap();
    let workspace = db.list_workspaces().await.unwrap().remove(0);
    let task = db
        .create_task(
            &workspace,
            TaskDraft {
                title: "setup memory benchmark".into(),
                description: String::new(),
                project: Some("app".into()),
                status: "todo".into(),
                priority: "none".into(),
                source: aven_core::choices::TaskSource::Cli,
                labels: vec![],
                metadata: vec![],
                available_at: None,
                due_on: None,
                is_epic: false,
            },
        )
        .await
        .unwrap()
        .task;

    let target = mib * MIB;
    let mut image_bytes = 0_u64;
    let mut image_count = 0_u64;
    let mut random = 1_u32;
    while image_bytes < target {
        // Incompressible pixels; PNG adds only its small framing overhead.
        let height = [100, 256, 512, 768, 1020][image_count as usize % 5];
        let mut image = image::RgbaImage::new(512, height);
        for byte in image.as_mut() {
            random ^= random << 13;
            random ^= random >> 17;
            random ^= random << 5;
            *byte = random as u8;
        }
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let bytes = encoded.into_inner();
        if image_bytes + bytes.len() as u64 > IMAGE_PLAINTEXT_CAP_BYTES {
            break;
        }
        image_bytes += bytes.len() as u64;
        db.add_task_attachment(
            &workspace,
            root.path(),
            Default::default(),
            &task.id,
            AttachmentAddInput {
                filename: Some(format!("image-{image_count}.png")),
                alt_text: None,
                declared_media_type: None,
                bytes,
                optimization_policy: aven_core::attachments::ImageOptimizationPolicy::Preserve,
                dedupe_existing: false,
            },
        )
        .await
        .unwrap();
        image_count += 1;
    }

    let server_log = std::fs::File::create(root.path().join("server.log")).unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "seed_bootstrap_http::tests::bench::large_image_setup",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("AVEN_SETUP_SERVER_ROOT", root.path())
        .stdout(Stdio::from(server_log.try_clone().unwrap()))
        .stderr(Stdio::from(server_log))
        .spawn()
        .unwrap();
    let mut server = Server(child);
    let origin_path = root.path().join("origin");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !origin_path.exists() {
        if let Some(status) = server.0.try_wait().unwrap() {
            let log = std::fs::read_to_string(root.path().join("server.log")).unwrap_or_default();
            panic!("server worker exited with {status}:\n{log}");
        }
        assert!(Instant::now() < deadline, "server startup timeout");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let origin = std::fs::read_to_string(origin_path).unwrap();
    let host = BenchHost {
        keys: root.path().join("keys"),
        blobs: root.path().to_path_buf(),
    };
    let invitation = SetupInvitation {
        server: origin,
        setup_id: [9; 32],
        secret: Secret::new([7; 32]),
    };
    let driver = crate::sync_http::HttpDriver::new().unwrap();
    println!(
        "setup_path=engine::run_setup proof_reuse=package_seed_capture_validated_to_resume_validated server_process=separate"
    );
    let elapsed_start = Instant::now();
    let stages = Arc::new(Mutex::new(Vec::<(Stage, Duration)>::new()));
    let latest_bytes = Arc::new(AtomicU64::new(0));
    let total_bytes = Arc::new(AtomicU64::new(0));
    let (outcome, writer) = writer_timing::measure(async {
        driver
            .run(|link| async {
                let started = Instant::now();
                let stages = stages.clone();
                let latest_bytes = latest_bytes.clone();
                let total_bytes = total_bytes.clone();
                let progress = move |progress: Progress| {
                    let mut stages = stages.lock().unwrap();
                    if stages
                        .last()
                        .is_none_or(|(stage, _)| *stage != progress.stage)
                    {
                        stages.push((progress.stage, started.elapsed()));
                    }
                    if let Some(Amount::Bytes {
                        done,
                        total: Some(total),
                    }) = progress.amount
                    {
                        latest_bytes.store(done, Ordering::Relaxed);
                        total_bytes.store(total, Ordering::Relaxed);
                    }
                };
                engine::run_setup(link, &db, &host, &invitation, &progress).await
            })
            .await
    })
    .await;
    let wall = elapsed_start.elapsed();
    if let Err(error) = outcome {
        let traffic = std::fs::read_to_string(root.path().join("traffic.txt"))
            .unwrap_or_else(|_| "no HTTP requests recorded".into());
        let trace = std::fs::read_to_string(root.path().join("traffic-requests.txt"))
            .unwrap_or_else(|_| "no request trace".into());
        let server_log = std::fs::read_to_string(root.path().join("server.log"))
            .unwrap_or_else(|_| "no server log".into());
        panic!(
            "setup failed: {error:#}; traffic:\n{traffic}; requests:\n{trace}; server log:\n{server_log}"
        );
    }
    let total = total_bytes.load(Ordering::Relaxed);
    let uploaded = latest_bytes.load(Ordering::Relaxed);
    assert!(total > 0, "setup did not report its byte budget");
    assert_eq!(uploaded, total, "setup did not upload every frozen record");

    let metrics = std::fs::read_to_string(root.path().join("traffic.txt")).unwrap();
    let request_bytes = value(&metrics, "request_body_bytes");
    let batch_bytes = value(&metrics, "batch_body_bytes");
    let batch_record_bytes = value(&metrics, "batch_record_bytes");
    let response_bytes = value(&metrics, "response_body_bytes");
    assert_eq!(
        batch_record_bytes, total,
        "HTTP batches did not carry the reported record bytes"
    );
    println!(
        "fixture images={image_count} image_plaintext_bytes={image_bytes} image_cap_bytes={IMAGE_PLAINTEXT_CAP_BYTES} staging_cap_bytes={} staging_chunk_cap={} record_cap_bytes={} batch_payload_cap_bytes={} batch_body_cap_bytes={} batch_record_cap={}",
        bootstrap_staging::MAX_STORAGE_BYTES,
        bootstrap_staging::MAX_CHUNKS,
        bootstrap_staging::MAX_REQUEST_BYTES,
        batch::MAX_PAYLOAD,
        batch::MAX_BYTES,
        batch::MAX_RECORDS,
    );
    println!(
        "sample wall_s={:.3} peak_rss_bytes={} progress_record_bytes={} http_request_body_bytes={} http_batch_body_bytes={} http_batch_record_bytes={} http_batch_framing_bytes={} http_response_body_bytes={} http_total_body_bytes={} http_requests={}",
        wall.as_secs_f64(),
        peak_rss_bytes(),
        total,
        request_bytes,
        batch_bytes,
        batch_record_bytes,
        batch_bytes - batch_record_bytes,
        response_bytes,
        request_bytes + response_bytes,
        value(&metrics, "requests"),
    );
    println!(
        "writer_gate_holds={} writer_gate_hold_total_s={:.6} writer_gate_hold_max_s={:.6} writer_gate_acquisitions={} writer_gate_wait_total_s={:.6} writer_gate_wait_max_s={:.6}",
        writer.gate_holds.len(),
        duration_total(&writer.gate_holds).as_secs_f64(),
        duration_max(&writer.gate_holds).as_secs_f64(),
        writer.gate_waits.len(),
        duration_total(&writer.gate_waits).as_secs_f64(),
        duration_max(&writer.gate_waits).as_secs_f64(),
    );
    println!(
        "writer_transaction_commit_completions={} begin_to_commit_total_s={:.6} begin_to_commit_max_s={:.6}",
        writer.transaction_commits.len(),
        duration_total(&writer.transaction_commits).as_secs_f64(),
        duration_max(&writer.transaction_commits).as_secs_f64(),
    );
    println!(
        "writer_transaction_explicit_rollback_completions={} begin_to_rollback_total_s={:.6} begin_to_rollback_max_s={:.6}",
        writer.transaction_rollbacks.len(),
        duration_total(&writer.transaction_rollbacks).as_secs_f64(),
        duration_max(&writer.transaction_rollbacks).as_secs_f64(),
    );
    println!(
        "writer_transactions_dropped={} begin_to_rollback_dispatch_total_s={:.6} begin_to_rollback_dispatch_max_s={:.6} rollback_completion_measured_for_drops=false",
        writer.transaction_drop_dispatches.len(),
        duration_total(&writer.transaction_drop_dispatches).as_secs_f64(),
        duration_max(&writer.transaction_drop_dispatches).as_secs_f64(),
    );
    let mut stages = stages.lock().unwrap().clone();
    stages.push((Stage::FinishingSetup, wall));
    for pair in stages.windows(2) {
        println!(
            "stage={:?} wall_s={:.3}",
            pair[0].0,
            pair[1].1.saturating_sub(pair[0].1).as_secs_f64(),
        );
    }
}
