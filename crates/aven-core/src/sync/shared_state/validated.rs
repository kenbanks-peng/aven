//! Reusable proof that the frozen package authenticates against the capture.
//!
//! One keyed pass establishes the proof. Writers never repeat it; they check
//! that the stored bytes the proof was made from are still the stored bytes.
use anyhow::{Context, Result, ensure};
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

    pub(crate) fn history(&self) -> [u8; 32] {
        self.history
    }
}

/// Commits to the capture's rank table and image classifications, which
/// adoption and packaging read directly.
async fn tables_digest(conn: &mut SqliteConnection, candidate: &str) -> Result<[u8; 32]> {
    let changes: Vec<(String, i64, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT change_id, prefix_rank, source_server_seq, source_pending_rank
         FROM local_shared_capture_changes WHERE candidate_id = ? ORDER BY change_id",
    )
    .bind(candidate)
    .fetch_all(&mut *conn)
    .await?;
    let images: Vec<(String, String)> = sqlx::query_as(
        "SELECT sha256, classification FROM local_shared_capture_images
         WHERE candidate_id = ? ORDER BY sha256",
    )
    .bind(candidate)
    .fetch_all(&mut *conn)
    .await?;
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
    digest.update((images.len() as u64).to_be_bytes());
    for (sha256, classification) in &images {
        text(&mut digest, sha256);
        text(&mut digest, classification);
    }
    Ok(digest.finalize().into())
}

/// A frozen package authenticated under the key against its capture.
pub(crate) struct ValidatedSeed {
    capture: NeverDispatchedLocalSharedCapture,
    metadata: PackageMetadata,
    identity: FrozenIdentity,
    attachments: publication::AttachmentIndex,
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
        Ok(Self {
            capture,
            metadata: PackageMetadata {
                descriptor: upload.descriptor.clone(),
                catalogs: upload.catalogs.clone(),
            },
            identity,
            attachments,
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
        tx.commit().await?;
        drop(conn);
        let upload = package.into_upload();
        ensure!(
            identity.descriptor_commitment == crate::sync::codec::hash(&upload.descriptor),
            "error seed-capture-changed"
        );
        let attachments = publication::authenticate_capture(&upload, &capture, key, membership)?;
        Ok(Self {
            capture,
            metadata: PackageMetadata {
                descriptor: upload.descriptor,
                catalogs: upload.catalogs,
            },
            identity,
            attachments,
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
}

/// Holds at most one proof across setup stages, loading it only when needed.
#[derive(Default)]
pub(crate) struct ProofCache {
    seed: Option<ValidatedSeed>,
}

impl ProofCache {
    pub(crate) fn new(seed: Option<ValidatedSeed>) -> Self {
        Self { seed }
    }

    pub(crate) async fn get(
        &mut self,
        database: &Database,
        key: &LocalSharedStatePackageKey,
        membership: [u8; 32],
    ) -> Result<&ValidatedSeed> {
        if self.seed.is_none() {
            self.seed = Some(ValidatedSeed::load(database, key, membership).await?);
        }
        Ok(self.seed.as_ref().expect("proof was just loaded"))
    }
}
