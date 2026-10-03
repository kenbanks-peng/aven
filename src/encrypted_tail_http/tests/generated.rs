//! Generated multi-device interleavings over the real HTTP transport, checked
//! against membership and content invariants after every step.
//!
//! `AVEN_GENERATED_CASES` raises the case count for long exploration runs and
//! `AVEN_GENERATED_TRACE=1` prints each step's outcome.
use super::*;
use crate::peer_enrollment_http;
use crate::test_support::e2ee_http::{expiry, protected_state::ProtectedState};
use aven_core::sync::seed_claim::membership::Membership;
use proptest::prelude::*;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

const MAX_DEVICES: usize = 4;

#[derive(Clone, Copy, Debug)]
enum Kind {
    Append,
    Put,
    Complete,
    Revoke,
    Rotate,
}

/// Device and task operands are reduced modulo the current population, so
/// every generated sequence stays meaningful while it shrinks. Local writes
/// may target removed devices, which keep their plaintext and local access.
#[derive(Clone, Debug)]
enum Action {
    Create(usize),
    Edit(usize, usize),
    Delete(usize, usize),
    Image(usize, usize),
    Sync(usize),
    Join(usize),
    Remove(usize, usize),
    Lose(Kind),
    RestartDevice(usize),
    RestartServer,
    StaleRetry(usize),
}

fn action() -> impl Strategy<Value = Action> {
    let kind = prop_oneof![
        Just(Kind::Append),
        Just(Kind::Put),
        Just(Kind::Complete),
        Just(Kind::Revoke),
        Just(Kind::Rotate),
    ];
    prop_oneof![
        3 => (0..4usize).prop_map(Action::Create),
        2 => (0..4usize, 0..8usize).prop_map(|(d, t)| Action::Edit(d, t)),
        1 => (0..4usize, 0..8usize).prop_map(|(d, t)| Action::Delete(d, t)),
        1 => (0..4usize, 0..8usize).prop_map(|(d, t)| Action::Image(d, t)),
        5 => (0..4usize).prop_map(Action::Sync),
        1 => (0..4usize).prop_map(Action::Join),
        2 => (0..4usize, 0..4usize).prop_map(|(d, t)| Action::Remove(d, t)),
        2 => kind.prop_map(Action::Lose),
        1 => (0..4usize).prop_map(Action::RestartDevice),
        1 => Just(Action::RestartServer),
        1 => (0..4usize).prop_map(Action::StaleRetry),
    ]
}

/// Loses the OK reply of one upcoming request of an armed kind after the
/// server commits it, and counts handler panics.
#[derive(Default)]
struct Faults {
    armed: Mutex<Option<(&'static str, usize)>>,
    panics: AtomicUsize,
}

async fn fault_layer(
    State(faults): State<Arc<Faults>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 64 * 1024 * 1024).await.unwrap();
    let name = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| {
            let value = value.get("operation").cloned().unwrap_or(value);
            value.as_object()?.keys().next().cloned()
        });
    let lose = {
        let mut armed = faults.armed.lock().unwrap();
        match &mut *armed {
            Some((kind, skip)) if name.as_deref() == Some(*kind) => {
                if *skip == 0 {
                    *armed = None;
                    true
                } else {
                    *skip -= 1;
                    false
                }
            }
            _ => false,
        }
    };
    let response =
        match tokio::spawn(next.run(Request::from_parts(parts, axum::body::Body::from(bytes))))
            .await
        {
            Ok(response) => response,
            Err(_) => {
                faults.panics.fetch_add(1, Ordering::SeqCst);
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
    if lose && response.status() == StatusCode::OK {
        (StatusCode::BAD_GATEWAY, "generated-lost-reply").into_response()
    } else {
        response
    }
}

async fn serve_faulty(f: &mut Fixture, faults: Arc<Faults>, reopen: bool) {
    f.task.abort();
    let _ = (&mut f.task).await;
    if reopen {
        f.server = Database::open(&f.root.path().join("server.sqlite"))
            .await
            .unwrap();
        f.server.verify_membership_history().await.unwrap();
    }
    let app = e2ee_http::router(f.server.clone())
        .await
        .layer(axum::middleware::from_fn_with_state(faults, fault_layer));
    (_, f.task) = e2ee_http::serve(app, f.origin.strip_prefix("http://").unwrap()).await;
}

struct Removal {
    step: usize,
    /// The target's own view when removal was requested.
    floor: Membership,
    confirmed: bool,
}

struct Device {
    keys: PathBuf,
    db: Database,
    store: ProtectedLocalKeyStore,
    id: [u8; 32],
    floor: Membership,
    removed: Option<Removal>,
}

impl Device {
    async fn open(path: &Path, keys: PathBuf, origin: &str) -> Self {
        let db = Database::open(path).await.unwrap();
        let store = isolated_store(&db, &keys).await;
        let inputs = store.active_inputs(&db, origin).await.unwrap();
        let (id, floor) = (inputs.device(), inputs.membership.clone());
        drop(inputs);
        Self {
            keys,
            db,
            store,
            id,
            floor,
            removed: None,
        }
    }
    async fn reopen(&mut self) {
        self.db = Database::open(self.db.path()).await.unwrap();
        self.store = isolated_store(&self.db, &self.keys).await;
    }
    async fn tasks(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT id FROM tasks WHERE deleted=0 ORDER BY id")
            .fetch_all(&mut *aven_core::test_support::acquire(&self.db).await.unwrap())
            .await
            .unwrap()
    }
}

struct Model {
    origin: String,
    devices: Vec<Device>,
    /// `(device, step, title)` for every title a device wrote locally.
    writes: Vec<(usize, usize, String)>,
    counter: usize,
    trace: bool,
}

impl Model {
    fn live(&self) -> Vec<usize> {
        (0..self.devices.len())
            .filter(|&i| self.devices[i].removed.is_none())
            .collect()
    }
    fn pick_live(&self, i: usize) -> usize {
        let live = self.live();
        live[i % live.len()]
    }
    fn token(&mut self) -> String {
        self.counter += 1;
        format!("generated-{}", self.counter)
    }
    /// Fault outcomes surface as stable `error <code>` messages, never as
    /// untyped transport or decoding failures.
    fn outcome<T: std::fmt::Debug>(&self, step: usize, what: String, result: &anyhow::Result<T>) {
        if let Err(error) = result {
            assert!(
                error.to_string().starts_with("error "),
                "step {step}: {what} failed untyped: {error:#}"
            );
        }
        self.log(step, || format!("{what}: {result:?}"));
    }
    fn log(&self, step: usize, message: impl FnOnce() -> String) {
        if self.trace {
            eprintln!("step {step}: {}", message());
        }
    }

    async fn apply(&mut self, f: &mut Fixture, faults: &Arc<Faults>, step: usize, action: &Action) {
        let client = Client::new(&f.origin).unwrap();
        match *action {
            Action::Create(d) => {
                let d = d % self.devices.len();
                let title = self.token();
                let db = &self.devices[d].db;
                let w = db.list_workspaces().await.unwrap().remove(0);
                db.create_task(&w, draft(&title)).await.unwrap();
                self.writes.push((d, step, title));
            }
            Action::Edit(d, t) | Action::Delete(d, t) => {
                let d = d % self.devices.len();
                let tasks = self.devices[d].tasks().await;
                if tasks.is_empty() {
                    return;
                }
                let task: aven_core::ids::TaskId = tasks[t % tasks.len()].parse().unwrap();
                let update = if matches!(action, Action::Edit(..)) {
                    let title = self.token();
                    self.writes.push((d, step, title.clone()));
                    TaskUpdate {
                        title: Some(title),
                        ..Default::default()
                    }
                } else {
                    TaskUpdate {
                        deleted: Some(true),
                        ..Default::default()
                    }
                };
                let db = &self.devices[d].db;
                let w = db.list_workspaces().await.unwrap().remove(0);
                if let Err(error) = db.update_task(&w, &task, update).await {
                    // Open conflicts refuse further edits until resolved.
                    assert!(
                        error.to_string().starts_with("error conflicted-field "),
                        "{error:#}"
                    );
                }
            }
            Action::Image(d, t) => {
                let d = d % self.devices.len();
                let tasks = self.devices[d].tasks().await;
                if tasks.is_empty() {
                    return;
                }
                let task: aven_core::ids::TaskId = tasks[t % tasks.len()].parse().unwrap();
                self.counter += 1;
                let mut bytes = std::io::Cursor::new(Vec::new());
                ::image::DynamicImage::new_rgb8(1 + self.counter as u32, 2)
                    .write_to(&mut bytes, ::image::ImageFormat::Png)
                    .unwrap();
                let db = &self.devices[d].db;
                let w = db.list_workspaces().await.unwrap().remove(0);
                db.add_task_attachment(
                    &w,
                    &blobs(db),
                    Default::default(),
                    &task,
                    aven_core::operations::AttachmentAddInput {
                        filename: None,
                        alt_text: None,
                        declared_media_type: None,
                        bytes: bytes.into_inner(),
                        optimization_policy:
                            aven_core::attachments::ImageOptimizationPolicy::Preserve,
                        dedupe_existing: false,
                    },
                )
                .await
                .unwrap();
            }
            Action::Sync(d) => {
                let d = self.pick_live(d);
                let device = &self.devices[d];
                let result = client
                    .round(&device.store, &device.db, &blobs(&device.db))
                    .await;
                self.outcome(step, format!("sync {d}"), &result);
            }
            Action::Join(inviter) => {
                if self.devices.len() >= MAX_DEVICES {
                    return;
                }
                let inviter = self.pick_live(inviter);
                let name = format!("joined-{step}");
                let result = self.join(f, inviter, &name).await;
                self.outcome(step, format!("join via {inviter}"), &result);
                if result.is_ok() {
                    let device = Device::open(
                        &f.root.path().join(format!("{name}.sqlite")),
                        f.root.path().join(format!("{name}-keys")),
                        &f.origin,
                    )
                    .await;
                    self.devices.push(device);
                }
            }
            Action::Remove(driver, target) => {
                let driver = self.pick_live(driver);
                let others: Vec<usize> = self.live().into_iter().filter(|&i| i != driver).collect();
                if others.is_empty() {
                    return;
                }
                let target = others[target % others.len()];
                let id = self.devices[target].id;
                let floor = self.devices[target].floor.clone();
                let device = &self.devices[driver];
                let result = peer_enrollment_http::Client::new(&f.origin)
                    .unwrap()
                    .remove_device(&device.store, &device.db, id)
                    .await;
                self.outcome(step, format!("remove {target} by {driver}"), &result);
                self.devices[target].removed = Some(Removal {
                    step,
                    floor,
                    confirmed: false,
                });
            }
            Action::Lose(kind) => {
                *faults.armed.lock().unwrap() = Some(match kind {
                    Kind::Append => ("Append", 0),
                    Kind::Put => ("Put", 0),
                    Kind::Complete => ("Complete", 0),
                    Kind::Revoke => ("Manage", 0),
                    Kind::Rotate => ("Manage", 1),
                });
            }
            Action::RestartDevice(d) => {
                let d = d % self.devices.len();
                self.devices[d].reopen().await;
            }
            Action::RestartServer => serve_faulty(f, faults.clone(), true).await,
            Action::StaleRetry(d) => {
                let removed: Vec<usize> = (0..self.devices.len())
                    .filter(|&i| self.devices[i].removed.is_some())
                    .collect();
                if removed.is_empty() {
                    return;
                }
                let d = removed[d % removed.len()];
                let device = &self.devices[d];
                let result = client
                    .round(&device.store, &device.db, &blobs(&device.db))
                    .await;
                self.outcome(step, format!("stale retry {d}"), &result);
                if !device.removed.as_ref().unwrap().confirmed {
                    if result.is_ok() {
                        // Still a member: everything written so far may be accepted.
                        self.devices[d].removed.as_mut().unwrap().step = step;
                    }
                    return;
                }
                let error = result.expect_err("removed credentials were accepted");
                assert!(
                    !error.is::<aven_core::sync::seed_claim::membership::StaleContext>(),
                    "removed credentials received stale context: {error:#}"
                );
                // A refused round may freeze its pending change locally, but a
                // repeated refusal changes nothing.
                let before = ProtectedState::capture(&device.db, &device.keys).await;
                assert!(
                    client
                        .round(&device.store, &device.db, &blobs(&device.db))
                        .await
                        .is_err()
                );
                before.assert_unchanged(&device.db, &device.keys).await;
            }
        }
    }

    /// The real enrollment sequence, stopping at the first refused or lost
    /// exchange instead of asserting success.
    async fn join(&self, f: &Fixture, inviter: usize, name: &str) -> anyhow::Result<()> {
        let inviter = &self.devices[inviter];
        let client = peer_enrollment_http::Client::new(&f.origin)?;
        let db = Database::open(&f.root.path().join(format!("{name}.sqlite"))).await?;
        let store = isolated_store(&db, &f.root.path().join(format!("{name}-keys"))).await;
        let invitation = client.invite(&inviter.store, &inviter.db, expiry()).await?;
        client.request(&store, &db, Some(invitation)).await?;
        anyhow::ensure!(
            client.admit(&inviter.store, &inviter.db).await?,
            "not admitted"
        );
        anyhow::ensure!(client.complete(&store, &db).await?, "not complete");
        client.install(&store, &db).await.map(drop)
    }

    async fn check(&mut self, step: usize) {
        let live = self.live();
        for i in 0..self.devices.len() {
            let device = &self.devices[i];
            let report = device.db.database_integrity_report().await.unwrap();
            assert!(
                report.quick_check_ok && report.checks.iter().all(|c| c.ok),
                "device {i} {:?} integrity: {:?} {:?}", device.db.path(), report.checks.iter().filter(|c| !c.ok).collect::<Vec<_>>(), sqlx::query_as::<_, (String, i64, Option<i64>, String)>("SELECT client_id, local_seq, server_seq, op_type FROM changes ORDER BY local_seq").fetch_all(&mut *aven_core::test_support::acquire(&device.db).await.unwrap()).await.unwrap()
            );
            let inputs = match device.store.active_inputs(&device.db, &self.origin).await {
                Ok(inputs) => inputs,
                Err(error) => {
                    assert!(
                        device.removed.is_some(),
                        "step {step}: live device {i} lost its inputs: {error:#}"
                    );
                    continue;
                }
            };
            let m = inputs.membership.clone();
            inputs.generation_keys().validate(&m).unwrap();
            drop(inputs);
            assert!(
                m.extends(&device.floor),
                "step {step}: device {i} membership moved off its floor ({} -> {})",
                device.floor.sequence(),
                m.sequence()
            );
            if let Some(removal) = &device.removed
                && removal.confirmed
            {
                assert_eq!(
                    m.generations(),
                    removal.floor.generations(),
                    "step {step}: removed device {i} gained a generation key"
                );
            }
            self.devices[i].floor = m;
        }
        for &s in &live {
            for x in 0..self.devices.len() {
                let view = &self.devices[s].floor;
                let target = &self.devices[x];
                if let Some(removal) = &target.removed
                    && !removal.confirmed
                    && view.contains_head(&removal.floor.head())
                    && !view.has_device(target.id)
                {
                    self.devices[x].removed.as_mut().unwrap().confirmed = true;
                }
            }
        }
        for &s in &live {
            let titles: Vec<String> = sqlx::query_scalar("SELECT title FROM tasks")
                .fetch_all(
                    &mut *aven_core::test_support::acquire(&self.devices[s].db)
                        .await
                        .unwrap(),
                )
                .await
                .unwrap();
            for (writer, written, title) in &self.writes {
                if let Some(removal) = &self.devices[*writer].removed
                    && removal.confirmed
                    && *written > removal.step
                {
                    assert!(
                        !titles.contains(title),
                        "step {step}: survivor {s} applied removed device {writer}'s write \
                         from step {written}"
                    );
                }
            }
        }
    }

    async fn drain(&mut self) {
        let client = Client::new(&self.origin).unwrap();
        let live = self.live();
        for _ in 0..3 {
            for &d in &live {
                drain(&client, &self.devices[d].store, &self.devices[d].db).await;
            }
        }
        self.check(usize::MAX).await;
        let reference = self.devices[live[0]].floor.clone();
        let mut states = Vec::new();
        for &d in &live {
            let device = &self.devices[d];
            assert!(
                device.floor.head() == reference.head(),
                "survivor {d} head diverged"
            );
            assert!(
                !device.floor.rotation_pending(),
                "survivor {d} rotation pending"
            );
            let round = client
                .round(&device.store, &device.db, &blobs(&device.db))
                .await
                .unwrap();
            assert_eq!(round.images, ImageTransfer::Complete, "survivor {d} images");
            assert!(!round.publishing_blocked, "survivor {d} publishing blocked");
            let mut c = aven_core::test_support::acquire(&device.db).await.unwrap();
            // Open conflicts keep each device's displayed value until the user
            // resolves them, so only the set of conflicted fields must agree.
            let conflicts: Vec<(String, String)> = sqlx::query_as(
                "SELECT DISTINCT entity_id,field FROM conflicts WHERE resolved=0 ORDER BY 1,2",
            )
            .fetch_all(&mut *c)
            .await
            .unwrap();
            let tasks: Vec<(String, String, String, String)> = sqlx::query_as(
                "SELECT id,
                     CASE WHEN EXISTS(SELECT 1 FROM conflicts WHERE resolved=0 AND entity_id=tasks.id AND field='title') THEN '' ELSE title END,
                     CASE WHEN EXISTS(SELECT 1 FROM conflicts WHERE resolved=0 AND entity_id=tasks.id AND field='status') THEN '' ELSE status END,
                     CASE WHEN EXISTS(SELECT 1 FROM conflicts WHERE resolved=0 AND entity_id=tasks.id AND field='deleted') THEN '' ELSE CAST(deleted AS TEXT) END
                 FROM tasks ORDER BY id",
            )
            .fetch_all(&mut *c)
            .await
            .unwrap();
            let images: Vec<(String, i64)> = sqlx::query_as(
                "SELECT attachment_id,deleted FROM task_attachments ORDER BY attachment_id",
            )
            .fetch_all(&mut *c)
            .await
            .unwrap();
            states.push((conflicts, tasks, images));
        }
        assert_quiescent(
            &live
                .iter()
                .map(|&d| &self.devices[d].db)
                .collect::<Vec<_>>(),
        )
        .await;
        for state in &states[1..] {
            assert_eq!(state, &states[0], "survivors diverged after drain");
        }
        for x in 0..self.devices.len() {
            let Some(removal) = &self.devices[x].removed else {
                continue;
            };
            assert!(removal.confirmed, "removal of device {x} never took effect");
            assert!(!reference.has_device(self.devices[x].id));
            let device = &self.devices[x];
            assert!(
                client
                    .round(&device.store, &device.db, &blobs(&device.db))
                    .await
                    .is_err(),
                "removed device {x} still syncs"
            );
            assert!(
                !device
                    .floor
                    .generations()
                    .contains(reference.current_generation()),
                "removed device {x} holds the current generation"
            );
        }
    }
}

async fn run(actions: Vec<Action>) {
    let mut f = fixture().await;
    let faults = Arc::new(Faults::default());
    serve_faulty(&mut f, faults.clone(), false).await;
    let root = f.root.path().to_owned();
    let mut model = Model {
        origin: f.origin.clone(),
        devices: vec![
            Device::open(f.seed.path(), root.join("keys"), &f.origin).await,
            Device::open(f.peer.path(), root.join("peer-keys"), &f.origin).await,
        ],
        writes: Vec::new(),
        counter: 0,
        trace: std::env::var_os("AVEN_GENERATED_TRACE").is_some(),
    };
    for (step, action) in actions.iter().enumerate() {
        model.log(step, || format!("{action:?}"));
        model.apply(&mut f, &faults, step, action).await;
        model.check(step).await;
        assert_eq!(
            faults.panics.load(Ordering::SeqCst),
            0,
            "server handler panicked"
        );
    }
    *faults.armed.lock().unwrap() = None;
    model.drain().await;
    assert_eq!(
        faults.panics.load(Ordering::SeqCst),
        0,
        "server handler panicked"
    );
}

fn cases() -> u32 {
    std::env::var("AVEN_GENERATED_CASES")
        .ok()
        .map_or(8, |cases| cases.parse().unwrap())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases(),
        failure_persistence: None,
        max_shrink_iters: 256,
        ..ProptestConfig::default()
    })]
    #[test]
    fn generated_multi_device_sequences_keep_membership_and_content_invariants(
        actions in proptest::collection::vec(action(), 1..=16)
    ) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(actions));
    }
}
