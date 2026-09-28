use super::*;
use crate::sync::shared_state::package::test_support::*;
use std::time::Duration;

struct ReleaseOnDrop(Option<tokio::sync::oneshot::Sender<()>>);

impl ReleaseOnDrop {
    fn release(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.release();
    }
}

#[tokio::test]
async fn postcommit_substitution_cannot_replace_authenticated_metadata() {
    let (dir, database, _) = source_with_history().await;
    let capture = database
        .capture_local_shared_state_never_dispatched()
        .await
        .unwrap();
    let candidate = capture.candidate_id().to_string();
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    READBACK_BARRIER
        .lock()
        .unwrap()
        .push((candidate.clone(), reached_tx, resume_rx));

    let worker_database = database.clone();
    let blob_dir = dir.path().to_path_buf();
    let freezing = tokio::spawn(async move {
        worker_database
            .package_and_validate(&blob_dir, package_context(), &package_key(), [7; 32], None)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("freeze should commit before readback")
        .unwrap();

    let mut conn = database.acquire_writer().await.unwrap();
    let (descriptor, data, prefix, images): (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) = sqlx::query_as(
        "SELECT descriptor, data_catalog, prefix_catalog, image_catalog
             FROM local_shared_capture_publication WHERE candidate_id = ?",
    )
    .bind(&candidate)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    let original_descriptor = descriptor.clone();
    let mut replacement = publication::Package {
        descriptor,
        catalogs: [data, prefix, images],
        state: Vec::new(),
        manifest: Vec::new(),
        images: Vec::new(),
    };
    let prefix_count = publication::staging::DeclarationView::decode(&replacement.descriptor)
        .unwrap()
        .prefix_count();
    publication::replace_prefix_catalog(&mut replacement, prefix_count);
    assert_ne!(replacement.descriptor, original_descriptor);
    sqlx::query(
        "UPDATE local_shared_capture_publication
         SET descriptor = ?, prefix_catalog = ? WHERE candidate_id = ?",
    )
    .bind(&replacement.descriptor)
    .bind(&replacement.catalogs[1])
    .bind(&candidate)
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE local_shared_capture_journal SET frozen_descriptor_commitment = ?
         WHERE candidate_id = ?",
    )
    .bind(codec::hash(&replacement.descriptor).as_slice())
    .bind(&candidate)
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    resume_tx.send(()).unwrap();

    let error = match freezing.await.unwrap() {
        Ok(_) => panic!("substituted metadata unexpectedly produced a proof"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "error encrypted-local-shared-package-write-mismatch"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_spool_read_rolls_back_every_freeze_row() {
    let (dir, database, task) = source_with_history().await;
    add_selected_images(dir.path(), &database, &task).await;
    let capture = database
        .capture_local_shared_state_never_dispatched()
        .await
        .unwrap();
    let candidate = capture.candidate_id().to_string();
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    READ_BARRIER
        .lock()
        .unwrap()
        .push((candidate.clone(), reached_tx, resume_rx, finished_tx));
    let mut release = ReleaseOnDrop(Some(resume_tx));

    let worker_database = database.clone();
    let blob_dir = dir.path().to_path_buf();
    let freezing = tokio::spawn(async move {
        worker_database
            .package_and_validate(&blob_dir, package_context(), &package_key(), [7; 32], None)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("spool read should reach its blocking worker")
        .unwrap();

    freezing.abort();
    assert!(matches!(freezing.await, Err(error) if error.is_cancelled()));
    release.release();
    tokio::time::timeout(Duration::from_secs(10), finished_rx)
        .await
        .expect("detached spool read should finish after release")
        .unwrap();

    let mut conn = database.acquire_reader().await.unwrap();
    let (publications, records, frozen_descriptor, frozen_capture, objects, pins): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = sqlx::query_as(
        "SELECT
             (SELECT count(*) FROM local_shared_capture_publication),
             (SELECT count(*) FROM local_shared_capture_package_records),
             (SELECT count(*) FROM local_shared_capture_journal
              WHERE frozen_descriptor_commitment IS NOT NULL),
             (SELECT count(*) FROM local_shared_capture_journal
              WHERE frozen_capture_commitment IS NOT NULL),
             (SELECT count(*) FROM local_shared_capture_images
              WHERE object_id IS NOT NULL),
             (SELECT count(*) FROM local_shared_capture_pins)",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        (
            publications,
            records,
            frozen_descriptor,
            frozen_capture,
            objects,
            pins,
        ),
        (0, 0, 0, 0, 0, 2)
    );
    drop(conn);

    let proof = database
        .package_and_validate(dir.path(), package_context(), &package_key(), [7; 32], None)
        .await
        .unwrap();
    assert_eq!(proof.identity().candidate(), candidate);
}
