//! Original-installation adoption. Never installs the captured domain snapshot.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;

use super::validated::{FreezeBinding, ProofCache, VALIDATION_VERSION, ValidatedSeed};
use super::{NeverDispatchedLocalSharedCapture, package};
use crate::db::{self, Database};
use crate::sync::LocalSharedStatePackageKey;
use crate::sync::seed_claim::{Genesis, Publication, PublicationOutcome, SeedAuthority};

/// Host-persisted source identity, independent of replaceable SQLite state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedSourceAuthority(Vec<u8>);

impl SeedSourceAuthority {
    pub fn generate(account: [u8; 32], genesis: &Genesis) -> Result<Self> {
        let mut bytes = b"AVENSRC1".to_vec();
        bytes.extend(account);
        let mut incarnation = [0; 32];
        getrandom::fill(&mut incarnation).context("error seed-source-entropy")?;
        bytes.extend(incarnation);
        bytes.extend(genesis.commitment());
        Ok(Self(bytes))
    }

    pub fn from_protected_storage(
        bytes: &[u8],
        account: [u8; 32],
        genesis: &Genesis,
    ) -> Result<Self> {
        ensure!(
            bytes.len() == 104
                && &bytes[..8] == b"AVENSRC1"
                && bytes[8..40] == account
                && bytes[72..] == genesis.commitment(),
            "error seed-source-authority-mismatch"
        );
        Ok(Self(bytes.to_vec()))
    }

    pub fn protected_storage_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct IntentData {
    source: Vec<u8>,
    client: String,
    generation: i64,
    candidate: String,
    descriptor: Vec<u8>,
    publication: Vec<u8>,
    history: [u8; 32],
    /// Absent only in intents written before freezes were bound; those fail
    /// closed rather than being trusted or re-signed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    freeze: Option<FreezeBinding>,
}

/// Exact local intent, not permission to dispatch or evidence of server acceptance.
#[derive(Clone, Debug)]
pub struct SeedPublicationIntent {
    bytes: Vec<u8>,
    data: IntentData,
}

impl SeedPublicationIntent {
    pub fn from_protected_storage(
        bytes: &[u8],
        source: &SeedSourceAuthority,
        genesis: &Genesis,
    ) -> Result<Self> {
        ensure!(bytes.len() <= 65536, "error seed-intent-too-large");
        let data: IntentData =
            serde_json::from_slice(bytes).context("error seed-intent-corrupt")?;
        ensure!(
            source.0[72..] == genesis.commitment()
                && data.source == source.0
                && serde_json::to_vec(&data)? == bytes,
            "error seed-intent-source-mismatch"
        );
        let publication = Publication::from_record(genesis, &data.descriptor, &data.publication)?;
        ensure!(
            hex::encode(publication.binding().bootstrap_id) == data.candidate,
            "error seed-intent-candidate-mismatch"
        );
        Ok(Self {
            bytes: bytes.to_vec(),
            data,
        })
    }

    pub fn protected_storage_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub(crate) fn freeze_binding(&self) -> Option<FreezeBinding> {
        self.data.freeze
    }

    /// Fails unless `proof` is the proof this intent's binding records.
    fn check_proof(&self, proof: &ValidatedSeed) -> Result<()> {
        let binding = self.data.freeze.context("error seed-freeze-unsupported")?;
        ensure!(
            binding.validation_version == VALIDATION_VERSION,
            "error seed-intent-receipt-unsupported"
        );
        ensure!(
            proof.identity().candidate() == self.data.candidate
                && proof.identity().history() == self.data.history
                && proof.descriptor() == self.data.descriptor
                && *proof.binding() == binding,
            "error seed-package-mismatch"
        );
        Ok(())
    }

    pub fn descriptor(&self) -> &[u8] {
        &self.data.descriptor
    }

    pub fn publication(&self, genesis: &Genesis) -> Result<Publication> {
        Publication::from_record(genesis, &self.data.descriptor, &self.data.publication)
    }
}

pub(crate) async fn ensure_unbound(conn: &mut SqliteConnection) -> Result<()> {
    let bound: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM local_seed_source) OR EXISTS(SELECT 1 FROM local_seed_publication_intent)").fetch_one(&mut *conn).await?;
    let peer_bound: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM local_peer_enrollment)")
        .fetch_one(&mut *conn)
        .await?;
    ensure!(
        !bound && !peer_bound,
        "error e2ee-installation-fenced encrypted-tail-and-replacement-unavailable"
    );
    Ok(())
}

pub(super) async fn ensure_no_intent(conn: &mut SqliteConnection) -> Result<()> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM local_seed_publication_intent) OR EXISTS(SELECT 1 FROM local_shared_capture_journal WHERE publication_owned = 1)")
            .fetch_one(conn)
            .await?;
    ensure!(
        !exists,
        "error seed-publication-intent-owned local-cancellation-unavailable"
    );
    Ok(())
}

async fn source_matches(
    conn: &mut SqliteConnection,
    source: &SeedSourceAuthority,
) -> Result<String> {
    let (bytes, client): (Vec<u8>, String) =
        sqlx::query_as("SELECT authority, client_id FROM local_seed_source WHERE singleton = 1")
            .fetch_optional(&mut *conn)
            .await?
            .context("error seed-source-missing")?;
    ensure!(
        bytes == source.0 && db::get_meta(conn, "client_id").await?.as_deref() == Some(&client),
        "error seed-source-mismatch"
    );
    Ok(client)
}

async fn generation(conn: &mut SqliteConnection) -> Result<i64> {
    db::get_meta(conn, "sync_generation")
        .await?
        .context("error seed-generation-missing")?
        .parse()
        .context("error seed-generation-invalid")
}

// Typed values serialize with sorted object keys; payloads are parsed before
// comparison so historical meaning does not depend on JSON whitespace.
pub(super) fn history_bytes(
    rows: &[crate::data_safety::export_types::ChangeRow],
) -> Result<String> {
    let mut values = rows
        .iter()
        .map(|row| {
            let mut value = serde_json::to_value(row)?;
            value["payload"] = serde_json::from_str(&row.payload)?;
            Ok(value)
        })
        .collect::<Result<Vec<serde_json::Value>>>()?;
    values.sort_by(|a, b| a["change_id"].as_str().cmp(&b["change_id"].as_str()));
    Ok(serde_json::to_string(&values)?)
}

/// One row of the stored source history map, borrowing its payload text.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceChange<'a> {
    change_id: String,
    client_id: String,
    local_seq: i64,
    entity_type: String,
    entity_id: String,
    field: Option<String>,
    op_type: String,
    #[serde(borrow)]
    payload: &'a serde_json::value::RawValue,
    base_version: Option<String>,
    created_at: String,
    server_seq: Option<i64>,
}

impl SourceChange<'_> {
    /// Compares every field, the original server sequence only when asked.
    /// Payloads match byte-for-byte first and semantically otherwise.
    fn matches(&self, row: &crate::data_safety::ChangeRow, with_server_seq: bool) -> Result<bool> {
        if self.change_id != row.change_id
            || self.client_id != row.client_id
            || self.local_seq != row.local_seq
            || self.entity_type != row.entity_type
            || self.entity_id != row.entity_id
            || self.field != row.field
            || self.op_type != row.op_type
            || self.base_version != row.base_version
            || self.created_at != row.created_at
            || (with_server_seq && self.server_seq != row.server_seq)
        {
            return Ok(false);
        }
        let source = self.payload.get();
        if source == row.payload {
            return Ok(true);
        }
        Ok(serde_json::from_str::<serde_json::Value>(source)?
            == serde_json::from_str::<serde_json::Value>(&row.payload)?)
    }
}

const HISTORY_PAGE: i64 = 1024;

async fn validate_history(
    conn: &mut SqliteConnection,
    candidate: &str,
    source: &SeedSourceAuthority,
) -> Result<[u8; 32]> {
    let (capture_source, history, stored_provenance, captured_generation): (
        Option<Vec<u8>>,
        Option<String>,
        Option<String>,
        i64,
    ) = sqlx::query_as(
        "SELECT source_authority, source_history, source_provenance, sync_generation
         FROM local_shared_capture_journal WHERE candidate_id = ?",
    )
    .bind(candidate)
    .fetch_optional(&mut *conn)
    .await?
    .context("error seed-capture-missing")?;
    ensure!(
        capture_source.as_deref() == Some(source.0.as_slice())
            && captured_generation == generation(conn).await?,
        "error seed-capture-source-changed"
    );
    let history = history.context("error seed-capture-incompatible recapture-never-dispatched")?;
    let stored_provenance =
        stored_provenance.context("error seed-capture-incompatible recapture-never-dispatched")?;
    let expected: Vec<SourceChange<'_>> =
        serde_json::from_str(&history).context("error seed-history-invalid")?;
    ensure!(
        expected
            .windows(2)
            .all(|pair| pair[0].change_id < pair[1].change_id),
        "error seed-history-invalid"
    );

    let captured: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM local_shared_capture_changes WHERE candidate_id = ?",
    )
    .bind(candidate)
    .fetch_one(&mut *conn)
    .await?;
    ensure!(
        usize::try_from(captured)? == expected.len(),
        "error seed-captured-history-map-mismatch"
    );
    // Captured IDs are exactly the source IDs, so an ordered walk of the live
    // rows joined to them must meet every source row once, in the same order.
    let mut pending = expected.iter();
    let mut after = String::new();
    loop {
        let page: Vec<crate::data_safety::ChangeRow> = sqlx::query_as(
            "SELECT ch.change_id, ch.client_id, ch.local_seq, ch.entity_type, ch.entity_id,
                    ch.field, ch.op_type, ch.payload, ch.base_version, ch.created_at,
                    ch.server_seq
             FROM local_shared_capture_changes c
             JOIN changes ch ON ch.change_id = c.change_id
             WHERE c.candidate_id = ? AND c.change_id > ?
             ORDER BY c.change_id LIMIT ?",
        )
        .bind(candidate)
        .bind(&after)
        .bind(HISTORY_PAGE)
        .fetch_all(&mut *conn)
        .await?;
        let Some(last) = page.last() else {
            break;
        };
        after = last.change_id.clone();
        for row in &page {
            let source_row = pending
                .next()
                .context("error seed-captured-history-changed")?;
            ensure!(
                source_row.matches(row, true)?,
                "error seed-captured-history-changed"
            );
        }
    }
    ensure!(
        pending.next().is_none(),
        "error seed-captured-history-changed"
    );

    let provenance_changed: bool = sqlx::query_scalar(
        "WITH source(change_id, server_seq, pending_rank) AS (
             SELECT json_extract(value, '$.change_id'),
                    json_extract(value, '$.source_server_seq'),
                    json_extract(value, '$.source_pending_rank')
             FROM json_each(?2)
         )
         SELECT EXISTS(
             SELECT 1 FROM shared_history_provenance p
             JOIN local_shared_capture_changes c
               ON c.candidate_id = ?1 AND c.change_id = p.change_id
             LEFT JOIN source s ON s.change_id = p.change_id
             WHERE s.change_id IS NULL
                OR s.server_seq IS NOT p.source_server_seq
                OR s.pending_rank IS NOT p.source_pending_rank
         ) OR EXISTS(
             SELECT 1 FROM source s
             LEFT JOIN shared_history_provenance p ON p.change_id = s.change_id
             LEFT JOIN local_shared_capture_changes c
               ON c.candidate_id = ?1 AND c.change_id = s.change_id
             WHERE p.change_id IS NULL OR c.change_id IS NULL
                OR s.server_seq IS NOT p.source_server_seq
                OR s.pending_rank IS NOT p.source_pending_rank
         )",
    )
    .bind(candidate)
    .bind(&stored_provenance)
    .fetch_one(&mut *conn)
    .await?;
    ensure!(!provenance_changed, "error seed-source-provenance-changed");

    let uncaptured: bool = sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM changes ch
             WHERE ch.server_seq IS NOT NULL
               AND NOT EXISTS(SELECT 1 FROM local_shared_capture_changes c
                              WHERE c.candidate_id = ? AND c.change_id = ch.change_id)
         )",
    )
    .bind(candidate)
    .fetch_one(&mut *conn)
    .await?;
    ensure!(!uncaptured, "error seed-uncaptured-accepted-history");

    Ok(history_digest(&history, &stored_provenance))
}

pub(super) fn history_digest(history: &str, provenance: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"aven-local-source-history-v1");
    digest.update((history.len() as u64).to_be_bytes());
    digest.update(history.as_bytes());
    digest.update(provenance.as_bytes());
    digest.finalize().into()
}

/// Checks the stored source history map against the frozen capture, ignoring
/// original server sequences, and returns the history commitment.
pub(super) async fn check_frozen_history(
    conn: &mut SqliteConnection,
    capture: &NeverDispatchedLocalSharedCapture,
) -> Result<[u8; 32]> {
    let (history, provenance): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT source_history, source_provenance
         FROM local_shared_capture_journal WHERE candidate_id = ?",
    )
    .bind(capture.candidate_id())
    .fetch_optional(&mut *conn)
    .await?
    .context("error seed-capture-missing")?;
    let (Some(history), Some(provenance)) = (history, provenance) else {
        anyhow::bail!("error seed-capture-incompatible recapture-never-dispatched");
    };
    let expected: Vec<SourceChange<'_>> =
        serde_json::from_str(&history).context("error seed-history-invalid")?;
    let mut frozen = capture
        .capture
        .snapshot
        .tables
        .changes
        .iter()
        .collect::<Vec<_>>();
    frozen.sort_by(|a, b| a.change_id.cmp(&b.change_id));
    ensure!(
        frozen.len() == expected.len(),
        "error seed-captured-history-map-mismatch"
    );
    for (source_row, frozen_row) in expected.iter().zip(&frozen) {
        ensure!(
            source_row.matches(frozen_row, false)?,
            "error seed-captured-history-map-mismatch"
        );
    }
    Ok(history_digest(&history, &provenance))
}

/// Records captured provenance and moves captured rows to their prefix
/// ranks. Rows outside the capture keep their state.
async fn adopt_captured_history(
    conn: &mut SqliteConnection,
    candidate: &str,
    prefix_count: u64,
) -> Result<()> {
    let captured: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM local_shared_capture_changes WHERE candidate_id = ?",
    )
    .bind(candidate)
    .fetch_one(&mut *conn)
    .await?;
    ensure!(
        u64::try_from(captured)? == prefix_count,
        "error seed-prefix-count-mismatch"
    );
    let provenance_conflict: bool = sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM local_shared_capture_changes c
             JOIN shared_history_provenance p USING (change_id)
             WHERE c.candidate_id = ?
               AND (c.source_server_seq IS NOT p.source_server_seq
                    OR c.source_pending_rank IS NOT p.source_pending_rank)
         )",
    )
    .bind(candidate)
    .fetch_one(&mut *conn)
    .await?;
    ensure!(!provenance_conflict, "error seed-provenance-changed");
    sqlx::query(
        "INSERT OR IGNORE INTO shared_history_provenance(
             change_id, source_server_seq, source_pending_rank
         )
         SELECT change_id, source_server_seq, source_pending_rank
         FROM local_shared_capture_changes WHERE candidate_id = ?",
    )
    .bind(candidate)
    .execute(&mut *conn)
    .await?;
    // The unique server_seq index requires clearing old ranks before
    // assigning the new ones.
    sqlx::query(
        "UPDATE changes SET server_seq = NULL WHERE change_id IN
         (SELECT change_id FROM local_shared_capture_changes WHERE candidate_id = ?)",
    )
    .bind(candidate)
    .execute(&mut *conn)
    .await?;
    let ranked = sqlx::query(
        "UPDATE changes SET server_seq = (
             SELECT prefix_rank FROM local_shared_capture_changes c
             WHERE c.candidate_id = ?1 AND c.change_id = changes.change_id
         )
         WHERE change_id IN
         (SELECT change_id FROM local_shared_capture_changes WHERE candidate_id = ?1)",
    )
    .bind(candidate)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    ensure!(
        i64::try_from(ranked)? == captured,
        "error seed-history-coverage-changed"
    );
    Ok(())
}

impl Database {
    /// Loads exact upload bytes only for a sealed, protected publication intent.
    /// Adopted installations need only their retained intent and remote outcome.
    pub(crate) async fn seed_publication_upload(
        &self,
        source: &SeedSourceAuthority,
        intent: &SeedPublicationIntent,
        seed: &SeedAuthority,
        key: &LocalSharedStatePackageKey,
        proofs: &mut ProofCache,
    ) -> Result<Option<package::upload::FrozenUpload>> {
        let proof = match self.seed_publication_intent_bytes().await? {
            Some((_, state)) if state == "adopted" => None,
            // Failures surface only where the writer would have validated.
            _ => Some(proofs.get(self, key, seed.genesis().commitment()).await),
        };
        let mut conn = self.acquire_writer().await?;
        let mut tx = db::begin_immediate(&mut conn).await?;
        source_matches(&mut tx, source).await?;
        let (stored, state): (Vec<u8>, String) = sqlx::query_as(
            "SELECT intent, state FROM local_seed_publication_intent WHERE singleton = 1",
        )
        .fetch_optional(&mut *tx)
        .await?
        .context("error seed-intent-missing")?;
        ensure!(stored == intent.bytes, "error seed-intent-mismatch");
        if state == "adopted" {
            return Ok(None);
        }
        ensure!(
            state == "sealed" && intent.data.generation == generation(&mut tx).await?,
            "error seed-intent-not-sealed"
        );
        ensure!(
            validate_history(&mut tx, &intent.data.candidate, source).await? == intent.data.history,
            "error seed-history-commitment-mismatch"
        );
        let proof = proof.context("error seed-intent-changed")??;
        proof.identity().assert_matches(&mut tx).await?;
        intent.check_proof(proof)?;
        tx.commit().await?;
        // Records stay in SQLite; each is verified against these catalogs as
        // it is read for upload.
        Ok(Some(package::upload::FrozenUpload::new(
            intent.data.candidate.clone(),
            proof.descriptor().to_vec(),
            proof.metadata().catalogs.clone(),
        )?))
    }

    pub async fn seed_source_pin(&self) -> Result<Option<Vec<u8>>> {
        let mut conn = self.acquire_reader().await?;
        Ok(
            sqlx::query_scalar("SELECT authority FROM local_seed_source WHERE singleton = 1")
                .fetch_optional(&mut *conn)
                .await?,
        )
    }

    /// Called under the installation interlock after protected persistence.
    pub async fn bind_seed_source(
        &self,
        source: &SeedSourceAuthority,
        installation: &db::installation::InstallationGuard,
    ) -> Result<()> {
        ensure!(
            self.file_identity() == Some(installation.identity()),
            "error seed-source-installation-mismatch"
        );
        installation.fence()?;
        let mut conn = self.acquire_writer().await?;
        let mut tx = db::begin_immediate(&mut conn).await?;
        let existing: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT authority FROM local_seed_source WHERE singleton = 1")
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(existing) = existing {
            ensure!(existing == source.0, "error seed-source-mismatch");
            source_matches(&mut tx, source).await?;
        } else {
            super::ensure_no_active_local_shared_capture(&mut tx).await?;
            let client = db::get_meta(&mut tx, "client_id")
                .await?
                .context("missing client identity")?;
            sqlx::query("INSERT INTO local_seed_source VALUES (1, ?, ?)")
                .bind(&source.0)
                .bind(client)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn seed_publication_intent_bytes(&self) -> Result<Option<(Vec<u8>, String)>> {
        let mut conn = self.acquire_reader().await?;
        Ok(sqlx::query_as(
            "SELECT intent, state FROM local_seed_publication_intent WHERE singleton = 1",
        )
        .fetch_optional(&mut *conn)
        .await?)
    }

    /// Commits cancellation exclusion before any protected intent is exposed.
    /// Retrying returns exact persisted bytes, never a newly signed context.
    pub async fn prepare_seed_publication_intent(
        &self,
        source: &SeedSourceAuthority,
        seed: &SeedAuthority,
        key: &LocalSharedStatePackageKey,
    ) -> Result<SeedPublicationIntent> {
        self.prepare_seed_publication_intent_with(source, seed, key, &mut ProofCache::default())
            .await
    }

    pub(crate) async fn prepare_seed_publication_intent_with(
        &self,
        source: &SeedSourceAuthority,
        seed: &SeedAuthority,
        key: &LocalSharedStatePackageKey,
        proofs: &mut ProofCache,
    ) -> Result<SeedPublicationIntent> {
        let proof = match self.seed_publication_intent_bytes().await? {
            Some((_, state)) if state == "adopted" => None,
            // Failures surface only where the writer would have validated.
            _ => Some(proofs.get(self, key, seed.genesis().commitment()).await),
        };
        let mut conn = self.acquire_writer().await?;
        let mut tx = db::begin_immediate(&mut conn).await?;
        let client = source_matches(&mut tx, source).await?;
        let pin: Vec<u8> =
            sqlx::query_scalar("SELECT commitment FROM local_seed_genesis_pin WHERE singleton = 1")
                .fetch_optional(&mut *tx)
                .await?
                .context("error seed-genesis-pin-missing")?;
        ensure!(
            pin == seed.genesis().commitment(),
            "error seed-genesis-pin-mismatch"
        );
        let existing: Option<(Vec<u8>, String)> = sqlx::query_as(
            "SELECT intent, state FROM local_seed_publication_intent WHERE singleton = 1",
        )
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((bytes, state)) = existing {
            let intent =
                SeedPublicationIntent::from_protected_storage(&bytes, source, seed.genesis())?;
            if state != "adopted" {
                ensure!(
                    intent.data.generation == generation(&mut tx).await?
                        && intent.data.history
                            == validate_history(&mut tx, &intent.data.candidate, source).await?,
                    "error seed-intent-source-changed"
                );
                let proof = proof.context("error seed-intent-changed")??;
                proof.identity().assert_matches(&mut tx).await?;
                intent.check_proof(proof)?;
                tx.commit().await?;
            }
            return Ok(intent);
        }
        ensure_no_intent(&mut tx).await?;
        let proof = proof.context("error seed-intent-changed")??;
        let candidate = proof.identity().candidate().to_string();
        let history = validate_history(&mut tx, &candidate, source).await?;
        proof.identity().assert_matches(&mut tx).await?;
        ensure!(
            history == proof.identity().history(),
            "error seed-capture-changed"
        );
        // The package was authenticated under `key` against the capture.
        let publication = seed.sign_authenticated_publication(proof.descriptor(), key)?;
        let data = IntentData {
            source: source.0.clone(),
            client,
            generation: generation(&mut tx).await?,
            candidate,
            descriptor: proof.descriptor().to_vec(),
            publication: publication.record().to_vec(),
            history,
            freeze: Some(*proof.binding()),
        };
        let bytes = serde_json::to_vec(&data)?;
        sqlx::query("INSERT INTO local_seed_publication_intent(singleton, candidate_id, intent, state) VALUES (1, ?, ?, 'preparing')").bind(&data.candidate).bind(&bytes).execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE local_shared_capture_journal SET publication_owned = 1 WHERE candidate_id = ?",
        )
        .bind(&data.candidate)
        .execute(&mut *tx)
        .await?;
        #[cfg(test)]
        wait_intent_boundary(&data.candidate).await;
        tx.commit().await?;
        SeedPublicationIntent::from_protected_storage(&bytes, source, seed.genesis())
    }

    /// Hosts supply an intent reloaded from protected storage, never a boolean.
    pub async fn seal_seed_publication_intent(
        &self,
        source: &SeedSourceAuthority,
        intent: &SeedPublicationIntent,
    ) -> Result<()> {
        let mut conn = self.acquire_writer().await?;
        let mut tx = db::begin_immediate(&mut conn).await?;
        source_matches(&mut tx, source).await?;
        ensure!(
            intent.data.source == source.0
                && intent.data.generation == generation(&mut tx).await?
                && intent.data.history
                    == validate_history(&mut tx, &intent.data.candidate, source).await?,
            "error seed-intent-source-changed"
        );
        let commitment: Vec<u8> = sqlx::query_scalar("SELECT frozen_descriptor_commitment FROM local_shared_capture_journal WHERE candidate_id = ?").bind(&intent.data.candidate).fetch_one(&mut *tx).await?;
        ensure!(
            commitment == Sha256::digest(&intent.data.descriptor).as_slice(),
            "error seed-frozen-descriptor-changed"
        );
        let changed = sqlx::query("UPDATE local_seed_publication_intent SET state = 'sealed' WHERE singleton = 1 AND intent = ? AND state IN ('preparing', 'sealed')").bind(&intent.bytes).execute(&mut *tx).await?.rows_affected();
        ensure!(changed == 1, "error seed-intent-cas-failed");
        tx.commit().await?;
        Ok(())
    }

    /// Atomically adopts the original seed's history, never its old domain image.
    /// Returns false for an already committed matching adoption without rewinding.
    pub(crate) async fn adopt_seed_publication(
        &self,
        source: &SeedSourceAuthority,
        intent: &SeedPublicationIntent,
        seed: &SeedAuthority,
        key: &LocalSharedStatePackageKey,
        outcome: &PublicationOutcome,
        proofs: &mut ProofCache,
    ) -> Result<bool> {
        let verified =
            SeedPublicationIntent::from_protected_storage(&intent.bytes, source, seed.genesis())?;
        outcome.validate_expected(seed.genesis(), &verified.data.descriptor)?;
        ensure!(
            outcome.publication().record().as_slice() == verified.data.publication,
            "error seed-publication-outcome-mismatch"
        );
        let binding = outcome.publication().binding();
        let association = format!(
            "{}:{}:{}",
            hex::encode(binding.vault_id),
            hex::encode(binding.stream_id),
            hex::encode(binding.bootstrap_id)
        );
        let validated = match self.seed_publication_intent_bytes().await? {
            Some((_, state)) if state == "adopted" => None,
            // Failures surface only where the writer would have validated.
            _ => Some(proofs.get(self, key, seed.genesis().commitment()).await),
        };
        let mut conn = self.acquire_writer().await?;
        let mut tx = db::begin_immediate(&mut conn).await?;
        let pin: Vec<u8> =
            sqlx::query_scalar("SELECT commitment FROM local_seed_genesis_pin WHERE singleton = 1")
                .fetch_optional(&mut *tx)
                .await?
                .context("error seed-genesis-pin-missing")?;
        ensure!(
            pin == seed.genesis().commitment(),
            "error seed-genesis-pin-mismatch"
        );
        ensure!(
            source_matches(&mut tx, source).await? == intent.data.client,
            "error seed-source-client-changed"
        );
        let (stored, state, adopted_generation): (Vec<u8>, String, Option<i64>) = sqlx::query_as("SELECT intent, state, association_generation FROM local_seed_publication_intent WHERE singleton = 1").fetch_optional(&mut *tx).await?.context("error seed-intent-missing")?;
        ensure!(stored == intent.bytes, "error seed-intent-mismatch");
        if state == "adopted" {
            ensure!(
                db::get_meta(&mut tx, "e2ee_association").await?.as_deref() == Some(&association)
                    && adopted_generation == Some(generation(&mut tx).await?)
                    && db::get_meta(&mut tx, "sync_cursor")
                        .await?
                        .context("missing cursor")?
                        .parse::<u64>()?
                        >= binding.prefix_count,
                "error seed-adopted-association-changed"
            );
            crate::sync::encrypted_tail::dependencies::validate(
                &mut tx,
                &association,
                i64::try_from(binding.prefix_count)?,
            )
            .await?;
            crate::sync::encrypted_tail::attachments::client::validate(
                &mut tx,
                &association,
                i64::try_from(binding.prefix_count)?,
                &binding.descriptor_commitment,
            )
            .await?;
            tx.commit().await?;
            return Ok(false);
        }
        ensure!(
            state == "sealed" && intent.data.generation == generation(&mut tx).await?,
            "error seed-intent-not-sealed"
        );
        ensure!(
            validate_history(&mut tx, &intent.data.candidate, source).await? == intent.data.history,
            "error seed-history-commitment-mismatch"
        );
        let proof = validated.context("error seed-intent-changed")??;
        proof.identity().assert_matches(&mut tx).await?;
        intent.check_proof(proof)?;
        adopt_captured_history(&mut tx, &intent.data.candidate, binding.prefix_count).await?;
        crate::epic_membership::recover(&mut tx, true).await?;
        let next = intent
            .data
            .generation
            .checked_add(1)
            .context("sync generation overflow")?;
        crate::sync::encrypted_tail::dependencies::initialize(
            &mut tx,
            &association,
            next,
            i64::try_from(binding.prefix_count)?,
            &proof.capture().capture.snapshot.tables.task_dependencies,
        )
        .await?;
        crate::sync::encrypted_tail::attachments::client::initialize_index(
            &mut tx,
            &association,
            next,
            i64::try_from(binding.prefix_count)?,
            &binding.descriptor_commitment,
            proof.attachments().clone(),
            true,
        )
        .await?;
        db::set_meta(&mut tx, "sync_generation", &next.to_string()).await?;
        db::set_meta(&mut tx, "sync_cursor", &binding.prefix_count.to_string()).await?;
        db::set_meta(&mut tx, "e2ee_association", &association).await?;
        sqlx::query("DELETE FROM meta WHERE key = 'sync_server_url'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE local_seed_publication_intent SET state = 'adopted', association_generation = ?, association = ? WHERE singleton = 1").bind(next).bind(&association).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Separate post-commit cleanup. Durable authority/receipt never cascades away.
    pub async fn cleanup_adopted_seed_capture(
        &self,
        source: &SeedSourceAuthority,
        intent: &SeedPublicationIntent,
    ) -> Result<()> {
        let mut conn = self.acquire_writer().await?;
        let mut tx = db::begin_immediate(&mut conn).await?;
        source_matches(&mut tx, source).await?;
        let ready: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM local_seed_publication_intent WHERE intent = ? AND state = 'adopted' AND association = (SELECT value FROM meta WHERE key = 'e2ee_association') AND association_generation = CAST((SELECT value FROM meta WHERE key = 'sync_generation') AS INTEGER))").bind(&intent.bytes).fetch_one(&mut *tx).await?;
        ensure!(ready, "error seed-adoption-cleanup-not-authorized");
        sqlx::query("DELETE FROM local_shared_capture_journal WHERE candidate_id = ?")
            .bind(&intent.data.candidate)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
type IntentBarrier = (
    String,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
#[cfg(test)]
static INTENT_BARRIER: std::sync::Mutex<Option<IntentBarrier>> = std::sync::Mutex::new(None);
#[cfg(test)]
async fn wait_intent_boundary(candidate: &str) {
    let barrier = {
        let mut slot = INTENT_BARRIER.lock().unwrap();
        if slot.as_ref().is_some_and(|(id, _, _)| id == candidate) {
            slot.take()
        } else {
            None
        }
    };
    if let Some((_, entered, resume)) = barrier {
        let _ = entered.send(());
        let _ = resume.await;
    }
}
#[cfg(test)]
mod tests;
