//! Ignored sync throughput benchmark. Counts rounds, HTTP requests, HTTP body
//! bytes and protected backend loads (one Keychain lookup each on macOS) for a
//! seed that pushes many changes and images, and a peer that pulls them.
//!
//! cargo test --lib encrypted_tail_http::tests::bench -- --ignored --nocapture
//! Throughput sizes: AVEN_BENCH_TASKS (default 2000), AVEN_BENCH_IMAGES
//! (default 500), AVEN_BENCH_IMAGE_BYTES (default tiny fixture), and
//! AVEN_BENCH_LATENCY_MS (default 0 per request). Integrity-scan sizes:
//! AVEN_BENCH_EDITS (default 2000), AVEN_BENCH_ATTACHMENTS (default 500).
use super::*;
use crate::protected_local_keys::tests::BACKEND_LOADS;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

static HTTP_REQUESTS: AtomicU64 = AtomicU64::new(0);
static HTTP_REQUEST_BYTES: AtomicU64 = AtomicU64::new(0);
static HTTP_RESPONSE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Middleware for `FixtureOptions::count_http`.
pub(super) async fn count_http(
    request: Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::body::HttpBody;
    HTTP_REQUESTS.fetch_add(1, Relaxed);
    HTTP_REQUEST_BYTES.fetch_add(request.body().size_hint().lower(), Relaxed);
    let latency_ms = size("AVEN_BENCH_LATENCY_MS", 0);
    if latency_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(latency_ms as u64)).await;
    }
    let response = next.run(request).await;
    HTTP_RESPONSE_BYTES.fetch_add(response.body().size_hint().lower(), Relaxed);
    response
}

fn size(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn distinct_png(index: usize, approximate_bytes: usize) -> Vec<u8> {
    let (width, height) = if approximate_bytes == 0 {
        (11, 7)
    } else {
        let width = 512;
        let pixels = approximate_bytes.div_ceil(4);
        (
            width,
            u32::try_from(pixels.div_ceil(width as usize)).unwrap(),
        )
    };
    let mut image = ::image::RgbaImage::new(width, height);
    let mut random = (index as u64).wrapping_add(1);
    for byte in image.as_mut() {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        *byte = random as u8;
    }
    image.as_mut()[..8].copy_from_slice(&(index as u64).to_be_bytes());
    let mut bytes = std::io::Cursor::new(Vec::new());
    ::image::DynamicImage::ImageRgba8(image)
        .write_to(&mut bytes, ::image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

struct Measured {
    rounds: usize,
    requests: u64,
    request_bytes: u64,
    response_bytes: u64,
    loads: u64,
    scan_calls: u64,
    scan_seconds: f64,
    seconds: f64,
}

impl Measured {
    fn print(&self, label: &str) {
        println!(
            "{label}: rounds={} requests={} request_bytes={} response_bytes={} protected_loads={} loads_per_round={:.1} attachment_scan_calls={} attachment_scan={:.6}s attachment_scan_share={:.2}% elapsed={:.2}s",
            self.rounds,
            self.requests,
            self.request_bytes,
            self.response_bytes,
            self.loads,
            self.loads as f64 / self.rounds.max(1) as f64,
            self.scan_calls,
            self.scan_seconds,
            100.0 * self.scan_seconds / self.seconds,
            self.seconds
        );
    }
}

async fn measure_drain(client: &Client, store: &ProtectedLocalKeyStore, db: &Database) -> Measured {
    let scan = tail::attachment_integrity_scan_metrics();
    let (requests, request_bytes, response_bytes, loads, start) = (
        HTTP_REQUESTS.load(Relaxed),
        HTTP_REQUEST_BYTES.load(Relaxed),
        HTTP_RESPONSE_BYTES.load(Relaxed),
        BACKEND_LOADS.load(Relaxed),
        Instant::now(),
    );
    let mut rounds = 0;
    let mut drain = client.start_drain(store, db).await.unwrap();
    loop {
        let round = client
            .round_in_drain(store, db, &blobs(db), &mut drain)
            .await
            .unwrap();
        rounds += 1;
        assert_ne!(round.images, ImageTransfer::Failed);
        if round.metadata_caught_up && round.images == ImageTransfer::Complete {
            break;
        }
        assert!(rounds < 100_000, "round budget");
    }
    let elapsed = start.elapsed().as_secs_f64();
    let scan_after = tail::attachment_integrity_scan_metrics();
    Measured {
        rounds,
        requests: HTTP_REQUESTS.load(Relaxed) - requests,
        request_bytes: HTTP_REQUEST_BYTES.load(Relaxed) - request_bytes,
        response_bytes: HTTP_RESPONSE_BYTES.load(Relaxed) - response_bytes,
        loads: BACKEND_LOADS.load(Relaxed) - loads,
        scan_calls: scan_after.0 - scan.0,
        scan_seconds: (scan_after.1 - scan.1) as f64 / 1_000_000_000.0,
        seconds: elapsed,
    }
}

async fn print_attachment_integrity_plan(db: &Database) {
    let mut conn = aven_core::test_support::acquire(db).await.unwrap();
    let rows: Vec<(i64, i64, i64, String)> = sqlx::query_as(
        "EXPLAIN QUERY PLAN SELECT EXISTS(SELECT 1 FROM task_attachments a
         WHERE NOT EXISTS(SELECT 1 FROM local_e2ee_image_references r
                          WHERE r.workspace=a.workspace_id AND r.reference=a.attachment_id)
           AND NOT EXISTS(SELECT 1 FROM changes c
                          WHERE c.change_id=a.created_by_change_id AND c.server_seq IS NULL))",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    for (_, _, _, detail) in rows {
        println!("attachment integrity query plan: {detail}");
    }
}

async fn inflate_attachment_rows(db: &Database, rows: usize, base_reference: &str) {
    let existing = scalar(db, "SELECT count(*) FROM task_attachments").await;
    let additions = i64::try_from(rows).unwrap() - existing;
    assert!(additions > 0);
    let mut conn = aven_core::test_support::acquire(db).await.unwrap();
    let mut tx = sqlx::Connection::begin(&mut *conn).await.unwrap();
    sqlx::query(
        "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i < ?)
         INSERT INTO task_attachments(
           workspace_id,attachment_id,task_id,sha256,byte_size,media_type,
           filename,alt_text,width,height,created_at,created_by_change_id,
           deleted,deleted_at,deleted_by_change_id)
         SELECT workspace_id,printf('%016X',n.i),task_id,sha256,byte_size,media_type,
           filename,alt_text,width,height,created_at,created_by_change_id,
           deleted,deleted_at,deleted_by_change_id
         FROM task_attachments CROSS JOIN n WHERE attachment_id=?",
    )
    .bind(additions)
    .bind(base_reference)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i < ?)
         INSERT INTO local_e2ee_image_references(
           workspace,reference,parent,object,origin,deleted)
         SELECT workspace,printf('%016X',n.i),parent,object,origin,deleted
         FROM local_e2ee_image_references CROSS JOIN n WHERE reference=?",
    )
    .bind(additions)
    .bind(base_reference)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        scalar(db, "SELECT count(*) FROM task_attachments").await,
        i64::try_from(rows).unwrap()
    );
}

#[tokio::test]
async fn idle_drain_makes_only_the_tail_request() {
    let f = fixture_with(FixtureOptions {
        count_http: true,
        ..Default::default()
    })
    .await;
    converge(&f).await;
    let before = HTTP_REQUESTS.load(Relaxed);

    let round = Client::new(&f.origin)
        .unwrap()
        .round(&f.seed_store, &f.seed, f.root.path())
        .await
        .unwrap();

    assert!(round.metadata_caught_up);
    assert_eq!(HTTP_REQUESTS.load(Relaxed) - before, 1);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn bench_push_and_pull_many_changes_and_images() {
    let (tasks, images, image_bytes) = (
        size("AVEN_BENCH_TASKS", 2000),
        size("AVEN_BENCH_IMAGES", 500),
        size("AVEN_BENCH_IMAGE_BYTES", 0),
    );
    let f = fixture_with(FixtureOptions {
        count_http: true,
        ..Default::default()
    })
    .await;
    converge(&f).await;
    let w = f.seed.list_workspaces().await.unwrap().remove(0);
    let before = scalar(&f.seed, "SELECT count(*) FROM changes").await;
    let mut ids = Vec::new();
    for index in 0..tasks {
        let task = f
            .seed
            .create_task(&w, draft(&format!("bench task {index}")))
            .await
            .unwrap()
            .task;
        ids.push(task.id);
    }
    for index in 0..images {
        f.seed
            .add_task_attachment(
                &w,
                &blobs(&f.seed),
                Default::default(),
                &ids[index % ids.len()],
                aven_core::operations::AttachmentAddInput {
                    filename: Some(format!("bench-{index}.png")),
                    alt_text: None,
                    declared_media_type: None,
                    bytes: distinct_png(index, image_bytes),
                    optimization_policy: aven_core::attachments::ImageOptimizationPolicy::Preserve,
                    dedupe_existing: false,
                },
            )
            .await
            .unwrap();
    }
    let changes = scalar(&f.seed, "SELECT count(*) FROM changes").await - before;
    println!(
        "workload: tasks={tasks} images={images} image_bytes={image_bytes} changes={changes} latency_ms={}",
        size("AVEN_BENCH_LATENCY_MS", 0)
    );
    let client = Client::new(&f.origin).unwrap();
    measure_drain(&client, &f.seed_store, &f.seed)
        .await
        .print("seed push");
    measure_drain(&client, &f.peer_store, &f.peer)
        .await
        .print("peer pull");
    assert_eq!(
        scalar(
            &f.peer,
            "SELECT count(*) FROM task_attachments WHERE deleted=0"
        )
        .await,
        scalar(
            &f.seed,
            "SELECT count(*) FROM task_attachments WHERE deleted=0"
        )
        .await
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark"]
async fn bench_attachment_integrity_scan() {
    let (edits, attachments) = (
        size("AVEN_BENCH_EDITS", 2000),
        size("AVEN_BENCH_ATTACHMENTS", 500),
    );
    let f = fixture_with(FixtureOptions {
        count_http: true,
        ..Default::default()
    })
    .await;
    converge(&f).await;
    let workspace = f.seed.list_workspaces().await.unwrap().remove(0);
    let base_task = f
        .seed
        .create_task(&workspace, draft("attachment scan base"))
        .await
        .unwrap()
        .task;
    let base_attachment = f
        .seed
        .add_task_attachment(
            &workspace,
            &blobs(&f.seed),
            Default::default(),
            &base_task.id,
            aven_core::operations::AttachmentAddInput {
                filename: Some("attachment-scan.png".into()),
                alt_text: None,
                declared_media_type: None,
                bytes: distinct_png(0, 0),
                optimization_policy: aven_core::attachments::ImageOptimizationPolicy::Preserve,
                dedupe_existing: false,
            },
        )
        .await
        .unwrap()
        .outcome
        .attachment
        .attachment_id;
    converge(&f).await;
    inflate_attachment_rows(&f.seed, attachments, &base_attachment).await;
    inflate_attachment_rows(&f.peer, attachments, &base_attachment).await;
    print_attachment_integrity_plan(&f.seed).await;
    let before = scalar(&f.seed, "SELECT count(*) FROM changes").await;
    for index in 0..edits {
        f.seed
            .create_task(&workspace, draft(&format!("scan edit {index}")))
            .await
            .unwrap();
    }
    let changes = scalar(&f.seed, "SELECT count(*) FROM changes").await - before;
    println!(
        "attachment integrity workload: attachments={attachments} edits={edits} changes={changes}"
    );
    let client = Client::new(&f.origin).unwrap();
    measure_drain(&client, &f.seed_store, &f.seed)
        .await
        .print("seed push");
    measure_drain(&client, &f.peer_store, &f.peer)
        .await
        .print("peer pull");
}
