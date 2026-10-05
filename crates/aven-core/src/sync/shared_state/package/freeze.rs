//! Bounded-image-memory package creation. Tentative ciphertext is spooled to an
//! anonymous temporary file; only a complete package is committed to SQLite.
use super::*;
use anyhow::Context;
use sqlx::Connection as _;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, LazyLock};
use tokio::sync::Semaphore;

const MAX_SPOOL_IO_WORKERS: usize = 2;
static SPOOL_IO_WORKERS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_SPOOL_IO_WORKERS)));

#[cfg(test)]
type ReadBarrier = (
    String,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
);
#[cfg(test)]
static READ_BARRIER: std::sync::Mutex<Vec<ReadBarrier>> = std::sync::Mutex::new(Vec::new());
#[cfg(test)]
type ReadbackBarrier = (
    String,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
#[cfg(test)]
static READBACK_BARRIER: std::sync::Mutex<Vec<ReadbackBarrier>> = std::sync::Mutex::new(Vec::new());

struct CiphertextSpool {
    file: Option<std::fs::File>,
    #[cfg(test)]
    candidate: String,
}

impl CiphertextSpool {
    async fn create(_candidate: &str) -> Result<Self> {
        let file = run_spool_io(move || Ok(tempfile::tempfile()?)).await?;
        Ok(Self {
            file: Some(file),
            #[cfg(test)]
            candidate: _candidate.to_string(),
        })
    }

    async fn append(&mut self, record: Vec<u8>) -> Result<(u64, usize)> {
        let file = self.file.take().context("package spool is unavailable")?;
        let (file, offset, length) = run_spool_io(move || {
            let mut file = file;
            let offset = file.seek(SeekFrom::End(0))?;
            let length = record.len();
            file.write_all(&record)?;
            Ok((file, offset, length))
        })
        .await?;
        self.file = Some(file);
        Ok((offset, length))
    }

    async fn read_at(&mut self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let file = self.file.take().context("package spool is unavailable")?;
        #[cfg(test)]
        let candidate = self.candidate.clone();
        let (file, bytes) = run_spool_io(move || {
            #[cfg(test)]
            let finished = wait_for_read(&candidate);
            let mut file = file;
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = vec![0; length];
            file.read_exact(&mut bytes)?;
            #[cfg(test)]
            if let Some(finished) = finished {
                let _ = finished.send(());
            }
            Ok((file, bytes))
        })
        .await?;
        self.file = Some(file);
        Ok(bytes)
    }
}

async fn run_spool_io<F, T>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let permit = SPOOL_IO_WORKERS
        .clone()
        .acquire_owned()
        .await
        .context("package spool worker pool closed")?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .context("package spool worker failed")?
}

struct SpoolRecord {
    object: [u8; 32],
    index: usize,
    offset: u64,
    length: usize,
    commitment: [u8; 32],
}

#[cfg(test)]
fn wait_for_read(candidate: &str) -> Option<tokio::sync::oneshot::Sender<()>> {
    let barrier = {
        let mut barriers = READ_BARRIER.lock().unwrap();
        barriers
            .iter()
            .position(|(expected, _, _, _)| expected == candidate)
            .map(|index| barriers.remove(index))
    };
    barrier.map(|(_, reached, resume, finished)| {
        let _ = reached.send(());
        let _ = resume.blocking_recv();
        finished
    })
}

#[cfg(test)]
async fn wait_before_readback(candidate: &str) {
    let barrier = {
        let mut barriers = READBACK_BARRIER.lock().unwrap();
        barriers
            .iter()
            .position(|(expected, _, _)| expected == candidate)
            .map(|index| barriers.remove(index))
    };
    if let Some((_, reached, resume)) = barrier {
        let _ = reached.send(());
        let _ = resume.await;
    }
}

pub(super) async fn freeze_and_validate(
    database: &Database,
    blob_dir: &Path,
    context: LocalSharedStatePackageContext,
    key: &LocalSharedStatePackageKey,
    membership: [u8; 32],
    mut supplied_capture: Option<NeverDispatchedLocalSharedCapture>,
) -> Result<ValidatedSeed> {
    loop {
        let capture = match supplied_capture.take() {
            Some(capture) => capture,
            None => database
                .resume_local_shared_state_never_dispatched()
                .await?
                .context("error local-shared-capture-missing")?,
        };
        let candidate = capture.candidate_id().to_string();
        let mut conn = database.acquire_reader().await?;
        let mut tx = conn.begin().await?;
        super::super::adoption::ensure_no_intent(&mut tx).await?;
        if frozen_exists(&mut tx, &candidate).await? {
            tx.commit().await?;
            drop(conn);
            return ValidatedSeed::load_expected(
                database,
                key,
                membership,
                Some(capture),
                Some(context),
            )
            .await;
        }
        let selected: Vec<(String, String)> = sqlx::query_as(
            "SELECT sha256, classification FROM local_shared_capture_images
             WHERE candidate_id = ? AND classification != 'unavailable'
             ORDER BY sha256",
        )
        .bind(&candidate)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        drop(conn);

        ensure!(
            selected.len() <= MAX_PACKAGE_IMAGE_COUNT,
            "error encrypted-local-shared-package-too-many-images"
        );
        let stream = decode_context_id(capture.stream_id(), "stream")?;
        let mut spool = CiphertextSpool::create(&candidate).await?;
        let mut summaries = Vec::with_capacity(selected.len());
        let mut records = Vec::new();
        let mut missing = Vec::new();
        let mut total = 0_usize;
        for selected_image in &selected {
            let (images, absent) = load_selected_image_plaintexts(
                blob_dir,
                std::slice::from_ref(selected_image),
                &capture.capture.snapshot,
            )
            .await?;
            missing.extend(absent);
            for image in images {
                total = total
                    .checked_add(image.bytes.len())
                    .context("image total overflow")?;
                ensure!(
                    total <= MAX_PACKAGE_IMAGE_PLAINTEXT_BYTES,
                    "error encrypted-local-shared-package-images-too-large"
                );
                let object = random_id()?;
                let image_key = derive_image_key(key, context, object)?;
                let encrypted = EncryptedImage {
                    sha256: image.source_sha256,
                    object_id: object,
                    artifact: encrypt_artifact(
                        &image.bytes,
                        context,
                        stream,
                        object,
                        IMAGE_FAMILY,
                        IMAGE_CLASS,
                        &image_key,
                    )?,
                };
                let summary =
                    publication::ImageSummary::authenticated(&encrypted, context, stream, key)?;
                for (index, chunk) in encrypted.artifact.chunks.into_iter().enumerate() {
                    let commitment = chunk.record_commitment;
                    let (offset, length) = spool.append(chunk.record.into_owned()).await?;
                    records.push(SpoolRecord {
                        object,
                        index,
                        offset,
                        length,
                        commitment,
                    });
                }
                summaries.push(summary);
            }
        }
        if !missing.is_empty() {
            mark_capture_images_unavailable(database, &candidate, &missing).await?;
            continue;
        }
        let (metadata, attachments) =
            publication::build_metadata(&capture, context, &summaries, key, membership)?;

        let mut conn = database.acquire_writer().await?;
        let mut tx = db::begin_immediate(&mut conn).await?;
        super::super::adoption::ensure_no_intent(&mut tx).await?;
        let active: Option<String> = sqlx::query_scalar(
            "SELECT candidate_id FROM local_shared_capture_journal WHERE singleton = 1",
        )
        .fetch_optional(&mut *tx)
        .await?;
        ensure!(
            active.as_ref() == Some(&candidate),
            "error local-shared-capture-changed-during-packaging"
        );
        super::super::validate_persisted_local_capture(
            &mut tx,
            &candidate,
            &capture.images,
            &capture.capture.snapshot,
        )
        .await?;
        if frozen_exists(&mut tx, &candidate).await? {
            tx.commit().await?;
            drop(conn);
            continue;
        }
        persist_metadata(&mut tx, &candidate, &metadata).await?;
        for record in records {
            let bytes = spool.read_at(record.offset, record.length).await?;
            ensure!(
                codec::hash(&bytes) == record.commitment,
                "error seed-package-chunk-mismatch"
            );
            sqlx::query(
                "INSERT INTO local_shared_capture_package_records(
                     candidate_id, component, object_id, chunk_index, record
                 ) VALUES (?, 'image', ?, ?, ?)",
            )
            .bind(&candidate)
            .bind(record.object.as_slice())
            .bind(i64::try_from(record.index)?)
            .bind(bytes)
            .execute(&mut *tx)
            .await?;
        }
        let updated = sqlx::query(
            "UPDATE local_shared_capture_journal SET frozen_descriptor_commitment = ?
             WHERE candidate_id = ? AND frozen_descriptor_commitment IS NULL",
        )
        .bind(codec::hash(&metadata.descriptor).as_slice())
        .bind(&candidate)
        .execute(&mut *tx)
        .await?;
        ensure!(
            updated.rows_affected() == 1,
            "error encrypted-local-shared-package-already-frozen"
        );
        super::super::validated::record_freeze(&mut tx, &capture, &attachments).await?;
        tx.commit().await?;
        drop(conn);
        #[cfg(test)]
        wait_before_readback(&candidate).await;

        // Verify the committed representation without extending the freeze
        // writer. Cancellation here leaves a complete freeze that a retry can
        // authenticate; no partial representation is visible.
        let mut conn = database.acquire_reader().await?;
        let mut tx = conn.begin().await?;
        let stored = upload::FrozenUpload::open(&mut tx, &candidate).await?;
        ensure!(
            stored.descriptor() == metadata.descriptor.as_slice()
                && stored.catalogs() == &metadata.catalogs,
            "error encrypted-local-shared-package-write-mismatch"
        );
        stored.verify_records(&mut tx).await?;
        let proof = ValidatedSeed::from_pass(
            &mut tx,
            capture,
            metadata.descriptor,
            metadata.catalogs,
            attachments,
        )
        .await?;
        tx.commit().await?;
        return Ok(proof);
    }
}

async fn persist_metadata(
    conn: &mut sqlx::SqliteConnection,
    candidate: &str,
    package: &publication::Package,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO local_shared_capture_publication(
             candidate_id, descriptor, data_catalog, prefix_catalog, image_catalog
         ) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(candidate)
    .bind(&package.descriptor)
    .bind(&package.catalogs[0])
    .bind(&package.catalogs[1])
    .bind(&package.catalogs[2])
    .execute(&mut *conn)
    .await?;
    for (component, records) in [
        (STATE_COMPONENT, &package.state),
        (MANIFEST_COMPONENT, &package.manifest),
    ] {
        for (index, record) in records.iter().enumerate() {
            sqlx::query(
                "INSERT INTO local_shared_capture_package_records(
                     candidate_id, component, object_id, chunk_index, record
                 ) VALUES (?, ?, x'', ?, ?)",
            )
            .bind(candidate)
            .bind(component)
            .bind(i64::try_from(index)?)
            .bind(record)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

pub(super) async fn frozen_exists(
    conn: &mut sqlx::SqliteConnection,
    candidate: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM local_shared_capture_publication WHERE candidate_id = ?
         ) OR EXISTS(
             SELECT 1 FROM local_shared_capture_journal
             WHERE candidate_id = ? AND frozen_descriptor_commitment IS NOT NULL
         )",
    )
    .bind(candidate)
    .bind(candidate)
    .fetch_one(conn)
    .await?)
}

#[cfg(test)]
#[path = "freeze_tests.rs"]
mod tests;
