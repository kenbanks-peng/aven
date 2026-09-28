//! Reusable proof that the frozen package authenticates against the capture.
//!
//! One keyed pass establishes the proof. Writers never repeat it; they check
//! that the stored bytes the proof was made from are still the stored bytes.
//! A protected intent carries a [`FreezeBinding`] to the proof's inputs, so a
//! later process rebuilds the proof from hashes of the stored bytes alone.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Connection, SqliteConnection};

use super::package::{self, EncryptedLocalSharedStatePackage, publication};
use super::{NeverDispatchedLocalSharedCapture, adoption, load_persisted_local_capture};
use crate::db::Database;
use crate::sync::LocalSharedStatePackageKey;

/// Clear package metadata: descriptor and catalogs, in data, prefix, image order.
pub(crate) struct PackageMetadata {
    pub(crate) descriptor: Vec<u8>,
    pub(crate) catalogs: [Vec<u8>; 3],
}

/// The only [`FreezeBinding::validation_version`] this build understands.
pub(crate) const VALIDATION_VERSION: u32 = 1;

/// What a keyed pass established, bound into the protected intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FreezeBinding {
    pub(crate) validation_version: u32,
    /// [`FrozenIdentity::digest`] of the authenticated capture.
    pub(crate) capture_digest: [u8; 32],
    /// [`mapping_digest`] of the authenticated image mappings.
    pub(crate) image_mapping_digest: [u8; 32],
}

/// One stored image row: sha256, classification and package object ID.
type MappingRow = (String, String, Option<Vec<u8>>);

/// The stored commitments a freeze writes with its package.
struct FreezeRecord {
    commitment: Option<Vec<u8>>,
    mappings: Vec<MappingRow>,
}

impl FreezeRecord {
    async fn read(conn: &mut SqliteConnection, candidate: &str) -> Result<Self> {
        let commitment: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT frozen_capture_commitment FROM local_shared_capture_journal
             WHERE candidate_id = ?",
        )
        .bind(candidate)
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
        Ok(Self {
            commitment,
            mappings: mapping_rows(conn, candidate).await?,
        })
    }

    /// The binding for a keyed pass that produced `attachments` from the
    /// capture `identity` describes. The stored object IDs must be exactly
    /// the authenticated mappings.
    fn binding(
        &self,
        identity: &FrozenIdentity,
        attachments: &publication::AttachmentIndex,
    ) -> Result<FreezeBinding> {
        let capture_digest = identity.digest();
        ensure!(
            self.commitment.as_deref() == Some(capture_digest.as_slice()),
            "error seed-freeze-unsupported"
        );
        let recorded = recorded_objects(&self.mappings)?;
        ensure!(
            recorded.len() == attachments.objects.len()
                && attachments.objects.iter().all(|(image, sha256)| {
                    recorded
                        .iter()
                        .any(|(id, recorded)| *id == image.id && recorded == sha256)
                }),
            "error seed-freeze-unsupported"
        );
        Ok(FreezeBinding {
            validation_version: VALIDATION_VERSION,
            capture_digest,
            image_mapping_digest: mapping_digest(&self.mappings),
        })
    }
}

async fn mapping_rows(conn: &mut SqliteConnection, candidate: &str) -> Result<Vec<MappingRow>> {
    Ok(sqlx::query_as(
        "SELECT sha256, classification, object_id FROM local_shared_capture_images
         WHERE candidate_id = ? ORDER BY sha256",
    )
    .bind(candidate)
    .fetch_all(&mut *conn)
    .await?)
}

/// The `(object ID, sha256)` pairs of rows that carry an object.
fn recorded_objects(rows: &[MappingRow]) -> Result<Vec<([u8; 32], String)>> {
    rows.iter()
        .filter_map(|(sha256, _, object)| object.as_ref().map(|object| (sha256, object)))
        .map(|(sha256, object)| {
            let id = <[u8; 32]>::try_from(object.as_slice())
                .map_err(|_| anyhow::anyhow!("error seed-capture-changed"))?;
            Ok((id, sha256.clone()))
        })
        .collect()
}

/// A framed, domain-labelled digest of every image row in sha256 order.
fn mapping_digest(rows: &[MappingRow]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"aven-seed-image-mappings-v1");
    digest.update((rows.len() as u64).to_be_bytes());
    for (sha256, classification, object) in rows {
        frame(&mut digest, sha256.as_bytes());
        frame(&mut digest, classification.as_bytes());
        match object {
            Some(object) => {
                digest.update([1]);
                frame(&mut digest, object);
            }
            None => digest.update([0]),
        }
    }
    digest.finalize().into()
}

fn frame(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

/// Writes the commitments of a package just persisted under `conn`: each
/// selected image's object ID, then the digest of the frozen capture.
pub(super) async fn record_freeze(
    conn: &mut SqliteConnection,
    capture: &NeverDispatchedLocalSharedCapture,
    attachments: &publication::AttachmentIndex,
) -> Result<()> {
    for (image, sha256) in &attachments.objects {
        let updated = sqlx::query(
            "UPDATE local_shared_capture_images SET object_id = ?
             WHERE candidate_id = ? AND sha256 = ? AND object_id IS NULL",
        )
        .bind(image.id.as_slice())
        .bind(capture.candidate_id())
        .bind(sha256)
        .execute(&mut *conn)
        .await?;
        ensure!(
            updated.rows_affected() == 1,
            "error encrypted-local-shared-package-write-mismatch"
        );
    }
    let identity = FrozenIdentity::establish(conn, capture).await?;
    let updated = sqlx::query(
        "UPDATE local_shared_capture_journal SET frozen_capture_commitment = ?
         WHERE candidate_id = ? AND frozen_capture_commitment IS NULL",
    )
    .bind(identity.digest().as_slice())
    .bind(capture.candidate_id())
    .execute(&mut *conn)
    .await?;
    ensure!(
        updated.rows_affected() == 1,
        "error encrypted-local-shared-package-already-frozen"
    );
    Ok(())
}

/// Commitments to every stored input the proof depends on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FrozenIdentity {
    candidate: String,
    stream: String,
    descriptor_commitment: [u8; 32],
    snapshot: [u8; 32],
    generation: i64,
    history: [u8; 32],
    tables: [u8; 32],
}

impl FrozenIdentity {
    /// Reads the identity of `capture`, which must have been decoded from the
    /// currently stored snapshot text, and checks its source history map.
    async fn establish(
        conn: &mut SqliteConnection,
        capture: &NeverDispatchedLocalSharedCapture,
    ) -> Result<Self> {
        let identity = Self::read(conn).await?;
        ensure!(
            identity.candidate == capture.candidate_id
                && identity.stream == capture.stream_id
                && identity.snapshot == capture.snapshot_digest,
            "error seed-capture-changed"
        );
        ensure!(
            adoption::check_frozen_history(conn, capture).await? == identity.history,
            "error seed-capture-changed"
        );
        Ok(identity)
    }

    async fn read(conn: &mut SqliteConnection) -> Result<Self> {
        type Row = (
            String,
            String,
            Option<Vec<u8>>,
            String,
            i64,
            Option<String>,
            Option<String>,
        );
        let (candidate, stream, commitment, snapshot, generation, history, provenance): Row =
            sqlx::query_as(
                "SELECT candidate_id, stream_id, frozen_descriptor_commitment, snapshot_json,
                        sync_generation, source_history, source_provenance
                 FROM local_shared_capture_journal WHERE singleton = 1",
            )
            .fetch_optional(&mut *conn)
            .await?
            .context("error seed-capture-missing")?;
        let descriptor_commitment = commitment
            .and_then(|c| <[u8; 32]>::try_from(c).ok())
            .context("error seed-package-missing")?;
        let (Some(history), Some(provenance)) = (history, provenance) else {
            anyhow::bail!("error seed-capture-incompatible recapture-never-dispatched");
        };
        let tables = tables_digest(conn, &candidate).await?;
        Ok(Self {
            snapshot: crate::sync::codec::hash(snapshot.as_bytes()),
            history: adoption::history_digest(&history, &provenance),
            candidate,
            stream,
            descriptor_commitment,
            generation,
            tables,
        })
    }

    /// Fails unless the stored capture, history map, rank and image tables and
    /// frozen descriptor are exactly those the proof was made from.
    pub(crate) async fn assert_matches(&self, conn: &mut SqliteConnection) -> Result<()> {
        ensure!(
            Self::read(conn).await? == *self,
            "error seed-capture-changed"
        );
        Ok(())
    }

    pub(crate) fn candidate(&self) -> &str {
        &self.candidate
    }

    /// A framed, domain-labelled digest of every commitment.
    pub(crate) fn digest(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(b"aven-seed-frozen-capture-v1");
        frame(&mut digest, self.candidate.as_bytes());
        frame(&mut digest, self.stream.as_bytes());
        digest.update(self.descriptor_commitment);
        digest.update(self.snapshot);
        digest.update(self.generation.to_be_bytes());
        digest.update(self.history);
        digest.update(self.tables);
        digest.finalize().into()
    }

    pub(crate) fn history(&self) -> [u8; 32] {
        self.history
    }
}

/// Commits to the capture's rank table and image rows, which adoption and
/// packaging read directly.
async fn tables_digest(conn: &mut SqliteConnection, candidate: &str) -> Result<[u8; 32]> {
    let changes: Vec<(String, i64, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT change_id, prefix_rank, source_server_seq, source_pending_rank
         FROM local_shared_capture_changes WHERE candidate_id = ? ORDER BY change_id",
    )
    .bind(candidate)
    .fetch_all(&mut *conn)
    .await?;
    let images = mapping_rows(conn, candidate).await?;
    let mut digest = Sha256::new();
    digest.update(b"aven-local-capture-tables-v1");
    let text = |digest: &mut Sha256, value: &str| {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    };
    let number = |digest: &mut Sha256, value: Option<i64>| match value {
        Some(value) => {
            digest.update([1]);
            digest.update(value.to_be_bytes());
        }
        None => digest.update([0]),
    };
    digest.update((changes.len() as u64).to_be_bytes());
    for (change_id, rank, server_seq, pending_rank) in &changes {
        text(&mut digest, change_id);
        digest.update(rank.to_be_bytes());
        number(&mut digest, *server_seq);
        number(&mut digest, *pending_rank);
    }
    digest.update(mapping_digest(&images));
    Ok(digest.finalize().into())
}

/// A frozen package authenticated under the key against its capture.
pub(crate) struct ValidatedSeed {
    capture: NeverDispatchedLocalSharedCapture,
    metadata: PackageMetadata,
    identity: FrozenIdentity,
    attachments: publication::AttachmentIndex,
    binding: FreezeBinding,
}

impl ValidatedSeed {
    /// Wraps a pass the caller just made. `conn` must see the package's
    /// frozen commitment and the capture's snapshot text.
    pub(super) async fn from_pass(
        conn: &mut SqliteConnection,
        capture: NeverDispatchedLocalSharedCapture,
        package: &EncryptedLocalSharedStatePackage,
        attachments: publication::AttachmentIndex,
    ) -> Result<Self> {
        let identity = FrozenIdentity::establish(conn, &capture).await?;
        let upload = package.upload();
        ensure!(
            identity.descriptor_commitment == crate::sync::codec::hash(&upload.descriptor),
            "error seed-capture-changed"
        );
        let binding = FreezeRecord::read(conn, &identity.candidate)
            .await?
            .binding(&identity, &attachments)?;
        Ok(Self {
            capture,
            metadata: PackageMetadata {
                descriptor: upload.descriptor.clone(),
                catalogs: upload.catalogs.clone(),
            },
            identity,
            attachments,
            binding,
        })
    }

    /// Reads capture and package in one read transaction, then makes the one
    /// keyed pass without holding any connection.
    pub(crate) async fn load(
        database: &Database,
        key: &LocalSharedStatePackageKey,
        membership: [u8; 32],
    ) -> Result<Self> {
        let mut conn = database.acquire_reader().await?;
        let mut tx = conn.begin().await?;
        let capture = load_persisted_local_capture(&mut tx)
            .await?
            .context("error seed-capture-missing")?;
        let package = package::load_package(&mut tx, capture.candidate_id())
            .await?
            .context("error seed-package-missing")?;
        let identity = FrozenIdentity::establish(&mut tx, &capture).await?;
        let record = FreezeRecord::read(&mut tx, &identity.candidate).await?;
        tx.commit().await?;
        drop(conn);
        let upload = package.into_upload();
        ensure!(
            identity.descriptor_commitment == crate::sync::codec::hash(&upload.descriptor),
            "error seed-capture-changed"
        );
        let attachments = publication::authenticate_capture(&upload, &capture, key, membership)?;
        let binding = record.binding(&identity, &attachments)?;
        Ok(Self {
            capture,
            metadata: PackageMetadata {
                descriptor: upload.descriptor,
                catalogs: upload.catalogs,
            },
            identity,
            attachments,
            binding,
        })
    }

    /// Rebuilds the proof a protected intent's `binding` records, from hashes
    /// of the stored bytes and without any decryption. Each hash is compared
    /// with the binding, never with another stored column.
    pub(crate) async fn load_from_intent(
        database: &Database,
        binding: FreezeBinding,
    ) -> Result<Self> {
        ensure!(
            binding.validation_version == VALIDATION_VERSION,
            "error seed-intent-receipt-unsupported"
        );
        let mut conn = database.acquire_reader().await?;
        let mut tx = conn.begin().await?;
        let capture = load_persisted_local_capture(&mut tx)
            .await?
            .context("error seed-capture-missing")?;
        let identity = FrozenIdentity::establish(&mut tx, &capture).await?;
        let mappings = mapping_rows(&mut tx, &identity.candidate).await?;
        let (descriptor, data, prefix, images): (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) =
            sqlx::query_as(
                "SELECT descriptor, data_catalog, prefix_catalog, image_catalog
                 FROM local_shared_capture_publication WHERE candidate_id = ?",
            )
            .bind(&identity.candidate)
            .fetch_optional(&mut *tx)
            .await?
            .context("error seed-package-missing")?;
        tx.commit().await?;
        drop(conn);
        ensure!(
            identity.digest() == binding.capture_digest
                && mapping_digest(&mappings) == binding.image_mapping_digest
                && identity.descriptor_commitment == crate::sync::codec::hash(&descriptor),
            "error seed-capture-changed"
        );
        let catalogs = [data, prefix, images];
        let attachments =
            publication::index_from_objects(&descriptor, &catalogs, &recorded_objects(&mappings)?)?;
        Ok(Self {
            capture,
            metadata: PackageMetadata {
                descriptor,
                catalogs,
            },
            identity,
            attachments,
            binding,
        })
    }

    pub(crate) fn identity(&self) -> &FrozenIdentity {
        &self.identity
    }

    pub(crate) fn metadata(&self) -> &PackageMetadata {
        &self.metadata
    }

    pub(crate) fn descriptor(&self) -> &[u8] {
        &self.metadata.descriptor
    }

    pub(crate) fn capture(&self) -> &NeverDispatchedLocalSharedCapture {
        &self.capture
    }

    pub(crate) fn attachments(&self) -> &publication::AttachmentIndex {
        &self.attachments
    }

    pub(crate) fn binding(&self) -> &FreezeBinding {
        &self.binding
    }
}

/// Holds at most one proof across setup stages, loading it only when needed:
/// from hashes when a protected intent binds it, otherwise with a keyed pass.
#[derive(Default)]
pub(crate) struct ProofCache {
    seed: Option<ValidatedSeed>,
    binding: Option<FreezeBinding>,
}

impl ProofCache {
    pub(crate) fn new(seed: Option<ValidatedSeed>) -> Self {
        Self {
            seed,
            binding: None,
        }
    }

    /// Lets a later load trust `intent`'s binding. Callers pass only an
    /// intent read back from protected storage; an intent in SQLite alone
    /// may never have been authenticated.
    pub(crate) fn trust_protected(&mut self, intent: &super::adoption::SeedPublicationIntent) {
        self.binding = intent.freeze_binding();
    }

    pub(crate) async fn get(
        &mut self,
        database: &Database,
        key: &LocalSharedStatePackageKey,
        membership: [u8; 32],
    ) -> Result<&ValidatedSeed> {
        if self.seed.is_none() {
            self.seed = Some(match self.binding {
                Some(binding) => ValidatedSeed::load_from_intent(database, binding).await?,
                None => ValidatedSeed::load(database, key, membership).await?,
            });
        }
        Ok(self.seed.as_ref().expect("proof was just loaded"))
    }
}
