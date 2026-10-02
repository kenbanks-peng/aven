use super::*;
use crate::db::{self, Database, begin_immediate};
use crate::sync::{persistence, wire::ChangeWire};
use anyhow::Context as _;
use sqlx::{Row, SqliteConnection};
use std::collections::HashSet;

pub(super) const INITIAL_IMAGE_WATERMARK: &str = "e2ee_initial_image_watermark";

// Missing catch-up state never proves that initial image demand is safe.
pub(super) async fn initial_image_watermark(conn: &mut SqliteConnection) -> Result<String> {
    let state = db::get_meta(conn, INITIAL_IMAGE_WATERMARK)
        .await?
        .context("error encrypted-image-initial-catch-up")?;
    valid(state == "pending" || state == "ready" || state.parse::<i64>().is_ok_and(|n| n >= 0))?;
    Ok(state)
}

pub(super) async fn validate_binding_and_cursor(
    conn: &mut SqliteConnection,
    authority: &Authority,
) -> Result<i64> {
    authority.validate()?;
    valid(crate::sync::protocol::replica_protocol(conn).await? <= 18)?;
    valid(
        db::get_meta(conn, "e2ee_association").await?.as_deref()
            == Some(authority.association.as_str()),
    )?;
    valid(
        db::get_meta(conn, "sync_generation").await?.as_deref()
            == Some(authority.sync_generation.to_string().as_str()),
    )?;
    let cursor = db::get_meta(conn, "sync_cursor")
        .await?
        .context("error encrypted-tail-cursor")?
        .parse::<i64>()?;
    valid(cursor >= authority.prefix)?;
    super::dependencies::validate(conn, &authority.association, authority.prefix).await?;
    super::attachments::client::validate(
        conn,
        &authority.association,
        authority.prefix,
        &authority.context.descriptor,
    )
    .await?;
    Ok(cursor)
}

pub(super) async fn load_change(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<Option<ChangeWire>> {
    let row = sqlx::query("SELECT * FROM changes WHERE change_id = ?")
        .bind(id)
        .fetch_optional(conn)
        .await?;
    row.map(|row| {
        Ok(ChangeWire {
            change_id: row.try_get("change_id")?,
            client_id: row.try_get("client_id")?,
            local_seq: row.try_get("local_seq")?,
            entity_type: row.try_get("entity_type")?,
            entity_id: row.try_get("entity_id")?,
            field: row.try_get("field")?,
            op_type: row.try_get("op_type")?,
            payload: domain::strict_value(row.try_get::<String, _>("payload")?.as_bytes())?,
            base_version: row.try_get("base_version")?,
            created_at: row.try_get("created_at")?,
            server_seq: row.try_get("server_seq")?,
        })
    })
    .transpose()
}

/// Ephemeral validation coverage for a serialized push run.
/// The mutation sequence invalidates it after local edits; the ordering endpoint
/// prevents reuse beyond the checked prefix, including after pending-row deletion.
#[derive(Clone)]
pub struct Preflight {
    local_seq: i64,
    through: (i64, String, String),
}

impl Preflight {
    fn covers(&self, local_seq: i64, change: &ChangeWire) -> bool {
        self.local_seq == local_seq
            && (
                change.local_seq,
                change.created_at.as_str(),
                change.change_id.as_str(),
            ) <= (
                self.through.0,
                self.through.1.as_str(),
                self.through.2.as_str(),
            )
    }
}

/// Validates one bounded ordered pending window before any of it is frozen.
async fn preflight(
    conn: &mut SqliteConnection,
    local_seq: i64,
) -> Result<Option<(ChangeWire, Preflight)>> {
    let pending_ids: Vec<String> = sqlx::query_scalar(
        "SELECT change_id FROM changes WHERE server_seq IS NULL
         ORDER BY local_seq, created_at, change_id LIMIT 4096",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut preflight_bytes = 0;
    let mut head = None;
    let mut through = None;
    for id in pending_ids {
        let change = load_change(&mut *conn, &id)
            .await?
            .context("error encrypted-tail-history-lost")?;
        let bytes = serde_json::to_vec(&change)?.len();
        if head.is_some() && preflight_bytes + bytes > 16 * 1048576 {
            break;
        }
        domain::validate(&change)?;
        preflight_bytes += bytes;
        through = Some((
            change.local_seq,
            change.created_at.clone(),
            change.change_id.clone(),
        ));
        if head.is_none() {
            head = Some(change);
        }
    }
    Ok(head
        .zip(through)
        .map(|(head, through)| (head, Preflight { local_seq, through })))
}

fn without_server_sequence(change: &ChangeWire) -> ChangeWire {
    let mut change = change.clone();
    change.server_seq = None;
    change
}

fn require_canonical_equality(local: &ChangeWire, incoming: &ChangeWire) -> Result<()> {
    domain::validate(&without_server_sequence(local))?;
    domain::validate(&without_server_sequence(incoming))?;
    ensure!(
        persistence::changes::canonical_equal(local, incoming),
        "error encrypted-tail-same-id-divergence"
    );
    Ok(())
}

async fn validate_mapping(
    conn: &mut SqliteConnection,
    mapping: &Mapping,
    cursor: i64,
) -> Result<()> {
    let rows: Vec<(String, i64, Vec<u8>)> = sqlx::query_as(
        "SELECT operation_id, sequence, commitment FROM local_e2ee_accepted
         WHERE operation_id = ? OR sequence = ?",
    )
    .bind(&mapping.operation_id)
    .bind(mapping.sequence)
    .fetch_all(&mut *conn)
    .await?;
    for (operation_id, sequence, commitment) in &rows {
        valid(
            operation_id == &mapping.operation_id
                && *sequence == mapping.sequence
                && commitment.as_slice() == mapping.commitment,
        )?;
    }
    valid(mapping.sequence > cursor || !rows.is_empty())?;
    if let Some(change) = load_change(conn, &mapping.operation_id).await? {
        valid(change.server_seq.is_none_or(|s| s == mapping.sequence))?;
    }
    let collision: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM changes WHERE server_seq = ? AND change_id != ?)",
    )
    .bind(mapping.sequence)
    .bind(&mapping.operation_id)
    .fetch_one(conn)
    .await?;
    valid(!collision)
}

async fn record_acceptance_and_clear_outbox(
    conn: &mut SqliteConnection,
    record: &Accepted,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO local_e2ee_accepted(operation_id, sequence, commitment, record)
         VALUES (?, ?, ?, ?) ON CONFLICT(operation_id) DO NOTHING",
    )
    .bind(&record.mapping.operation_id)
    .bind(record.mapping.sequence)
    .bind(record.mapping.commitment.as_slice())
    .bind(&record.record)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM local_e2ee_outbox WHERE operation_id = ?")
        .bind(&record.mapping.operation_id)
        .execute(conn)
        .await?;
    Ok(())
}

fn open_accepted_change(authority: &Authority, accepted: &Accepted) -> Result<ChangeWire> {
    valid(
        accepted.mapping.sequence > authority.prefix
            && hash(&accepted.record) == accepted.mapping.commitment,
    )?;
    let e = codec::parse(&accepted.record)?;
    valid(
        authority
            .membership
            .generation_allows(e.generation, u64::try_from(accepted.mapping.sequence)?),
    )?;
    let change = codec::open(authority, &accepted.record)?;
    valid(change.change_id == accepted.mapping.operation_id)?;
    Ok(change)
}

async fn load_observed_outcome(
    conn: &mut SqliteConnection,
    operation_id: &str,
) -> Result<Option<(Option<i64>, Option<Vec<u8>>)>> {
    Ok(sqlx::query_as(
        "SELECT observed_sequence, observed_commitment FROM local_e2ee_outbox
         WHERE operation_id = ?",
    )
    .bind(operation_id)
    .fetch_optional(conn)
    .await?)
}

async fn task_exists(conn: &mut SqliteConnection, task_id: &str) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?)")
            .bind(task_id)
            .fetch_one(conn)
            .await?,
    )
}

async fn apply_new_remote_change(
    conn: &mut SqliteConnection,
    change: &ChangeWire,
    attachment_hashes: &mut HashSet<String>,
) -> Result<()> {
    persistence::collect_attachment_liveness_hashes(conn, change, attachment_hashes).await?;
    if persistence::is_epic_change(change) {
        crate::epic_membership::capture_snapshot_baseline(
            conn,
            persistence::epic_change_workspace(change)?,
            &change.entity_id,
        )
        .await?;
    }
    let related = matches!(change.op_type.as_str(), "related_add" | "related_remove");
    // Related links reference their establishing change through a foreign key,
    // so their history row must exist before applying the presence register.
    if related {
        persistence::insert_wire_change(conn, change).await?;
    }
    crate::sync::apply::apply_remote_change(conn, change)
        .await
        .map_err(|_| anyhow::anyhow!("error encrypted-tail-apply"))?;
    if !related {
        persistence::insert_wire_change(conn, change).await?;
    }
    persistence::reconcile_epic_change(conn, change).await?;
    Ok(())
}

async fn accept_local_change(
    conn: &mut SqliteConnection,
    authority: &Authority,
    accepted: &Accepted,
    change: &ChangeWire,
) -> Result<()> {
    persistence::update_change_server_seq(
        &mut *conn,
        &change.change_id,
        Some(accepted.mapping.sequence),
    )
    .await?;
    persistence::reconcile_epic_change(&mut *conn, change).await?;
    super::notes::reconcile(&mut *conn, authority.prefix, change).await?;
    super::labels::reconcile(&mut *conn, authority.prefix, change).await?;
    if let Some(workspace) = super::dependencies::affected_workspace(change)? {
        super::dependencies::reconcile(&mut *conn, authority.prefix, workspace).await?;
    }
    super::attachments::client::accept(&mut *conn, accepted, change).await?;
    record_acceptance_and_clear_outbox(&mut *conn, accepted).await?;
    Ok(())
}

impl Database {
    /// Validates attachment coverage before a drain can push or apply records.
    /// Transaction-local checks continue to validate the active association.
    pub(crate) async fn validate_encrypted_attachment_integrity(
        &self,
        authority: &Authority,
    ) -> Result<()> {
        let mut conn = self.acquire_reader().await?;
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        validate_binding_and_cursor(&mut tx, authority).await?;
        super::attachments::client::validate_integrity(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// One validated read of local round state. Observation never freezes work
    /// or advances the download selector.
    pub async fn encrypted_round_state(&self, authority: &Authority) -> Result<RoundState> {
        let mut conn = self.acquire_reader().await?;
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        let cursor = validate_binding_and_cursor(&mut tx, authority).await?;
        let initial = initial_image_watermark(&mut tx).await?;
        let (idle, upload_pending): (bool, bool) = sqlx::query_as(
            "SELECT NOT EXISTS(SELECT 1 FROM local_e2ee_outbox)
                    AND NOT EXISTS(SELECT 1 FROM changes WHERE server_seq IS NULL),
                    EXISTS(SELECT 1 FROM changes
                           WHERE server_seq IS NULL AND op_type = 'attachment_add')",
        )
        .fetch_one(&mut *tx)
        .await?;
        let downloads = if initial == "ready" {
            Some(super::attachments::client::downloads(&mut tx).await?)
        } else {
            None
        };
        tx.commit().await?;
        Ok(RoundState {
            cursor,
            initial_watermark: initial.parse().ok(),
            idle,
            upload_pending,
            downloads,
        })
    }
    /// Returns the frozen head, or freezes the next ordered pending change.
    /// Image heads also return exact staged ciphertext for upload.
    pub async fn prepare_encrypted_push(
        &self,
        authority: &Authority,
        blob_dir: &std::path::Path,
    ) -> Result<Option<Push>> {
        Ok(self
            .prepare_encrypted_push_inner(authority, blob_dir, None, 1)
            .await?
            .0)
    }
    /// Freezes the next ordered pending group in one serialized push run.
    /// Reuses only the checked window while the local mutation sequence is unchanged.
    pub async fn prepare_encrypted_batch_in_run(
        &self,
        authority: &Authority,
        blob_dir: &std::path::Path,
        preflight: Option<Preflight>,
        count: usize,
    ) -> Result<(Option<Push>, Option<Preflight>)> {
        self.prepare_encrypted_push_inner(authority, blob_dir, preflight, count)
            .await
    }
    async fn prepare_encrypted_push_inner(
        &self,
        authority: &Authority,
        blob_dir: &std::path::Path,
        preflight: Option<Preflight>,
        count: usize,
    ) -> Result<(Option<Push>, Option<Preflight>)> {
        valid((1..=BATCH_COUNT).contains(&count))?;
        let mut conn = self.acquire_writer().await?;
        let mut tx = begin_immediate(&mut conn).await?;
        validate_binding_and_cursor(&mut tx, authority).await?;
        if authority.rotation_pending() {
            tx.commit().await?;
            return Ok((None, preflight));
        }
        let frozen: Option<(String, i64, Vec<u8>, bool)> = sqlx::query_as(
            "SELECT association, sync_generation, record, blocked
             FROM local_e2ee_outbox ORDER BY position LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((association, generation, record, blocked)) = frozen {
            valid(association == authority.association && generation == authority.sync_generation)?;
            ensure!(!blocked, "error encrypted-tail-integrity-blocked");
            ensure!(
                !authority.record_is_closed(&record)?,
                "error encrypted-tail-outcome-required"
            );
            let change = codec::open(authority, &record)?;
            require_canonical_equality(
                &change,
                &load_change(&mut tx, &change.change_id)
                    .await?
                    .context("error encrypted-tail-history-lost")?,
            )?;
            let upload = if change.op_type == "attachment_add" {
                Some(
                    super::attachments::client::frozen_upload(
                        &mut tx, authority, &change, &record, blob_dir,
                    )
                    .await?,
                )
            } else {
                None
            };
            tx.commit().await?;
            return Ok((Some(Push { record, upload }), preflight));
        }
        let local_seq = db::get_meta(&mut tx, "local_seq")
            .await?
            .context("error encrypted-tail-local-sequence")?
            .parse::<i64>()?;
        let id: Option<String> = sqlx::query_scalar(
            "SELECT change_id FROM changes WHERE server_seq IS NULL
             ORDER BY local_seq, created_at, change_id LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else {
            tx.commit().await?;
            return Ok((None, preflight));
        };
        let mut change = load_change(&mut tx, &id)
            .await?
            .context("error encrypted-tail-history-lost")?;
        let preflight = match preflight {
            Some(coverage) if coverage.covers(local_seq, &change) => coverage,
            _ => {
                let (head, coverage) = self::preflight(&mut tx, local_seq)
                    .await?
                    .context("error encrypted-tail-history-lost")?;
                change = head;
                coverage
            }
        };
        let (projection, upload) = if change.op_type == "attachment_add" {
            let (projection, upload) =
                super::attachments::client::stage(&mut tx, authority, &change, blob_dir).await?;
            (projection, Some(upload))
        } else {
            (domain::validate(&change)?, None)
        };
        let record = codec::seal_projection(authority, &change, &projection)?;
        // Check the full decoder contract before committing dispatch authority.
        require_canonical_equality(&change, &codec::open(authority, &record)?)?;
        sqlx::query(
            "INSERT INTO local_e2ee_outbox(
                 position, operation_id, association, sync_generation, record
             ) VALUES (1, ?, ?, ?, ?)",
        )
        .bind(&change.change_id)
        .bind(&authority.association)
        .bind(authority.sync_generation)
        .bind(&record)
        .execute(&mut *tx)
        .await?;
        let mut frozen_count = 1;
        if upload.is_none() && count > 1 {
            let ids: Vec<String> = sqlx::query_scalar(
                "SELECT change_id FROM changes WHERE server_seq IS NULL AND change_id != ?
                 ORDER BY local_seq, created_at, change_id LIMIT ?",
            )
            .bind(&change.change_id)
            .bind((count - 1) as i64)
            .fetch_all(&mut *tx)
            .await?;
            let mut bytes = record.len();
            for (index, id) in ids.iter().enumerate() {
                let next = load_change(&mut tx, id)
                    .await?
                    .context("error encrypted-tail-history-lost")?;
                if !preflight.covers(local_seq, &next) || next.op_type == "attachment_add" {
                    break;
                }
                let projection = domain::validate(&next)?;
                let next_record = codec::seal_projection(authority, &next, &projection)?;
                if bytes + next_record.len() > BATCH_BYTES {
                    break;
                }
                require_canonical_equality(&next, &codec::open(authority, &next_record)?)?;
                bytes += next_record.len();
                sqlx::query("INSERT INTO local_e2ee_outbox(position, operation_id, association, sync_generation, record) VALUES (?, ?, ?, ?, ?)")
                    .bind((index + 2) as i64).bind(id).bind(&authority.association)
                    .bind(authority.sync_generation).bind(next_record).execute(&mut *tx).await?;
                frozen_count += 1;
            }
        }
        tx.commit().await?;
        if frozen_count > 1 {
            crate::sync::crash::Crash::Tail.at("batch-frozen");
        }
        crate::sync::crash::Crash::Tail.at(if upload.is_some() {
            "image-frozen"
        } else {
            "frozen"
        });
        Ok((Some(Push { record, upload }), Some(preflight)))
    }
    /// Closed-generation absence and replacement commit under one outbox/history owner.
    /// A failed transaction retains old bytes and requires a new lookup on retry.
    pub async fn reconcile_encrypted_tail_absence(
        &self,
        authority: &Authority,
        absence: &AbsentOperation,
        blob_dir: &std::path::Path,
    ) -> Result<bool> {
        self.reconcile_encrypted_tail_absences(authority, std::slice::from_ref(absence), blob_dir)
            .await
    }
    pub async fn reconcile_encrypted_tail_absences(
        &self,
        authority: &Authority,
        absences: &[AbsentOperation],
        blob_dir: &std::path::Path,
    ) -> Result<bool> {
        valid(absences.len() <= BATCH_COUNT)?;
        let mut conn = self.acquire_writer().await?;
        let mut tx = begin_immediate(&mut conn).await?;
        validate_binding_and_cursor(&mut tx, authority).await?;
        let mut changed = false;
        for absence in absences {
            valid(absence.context == authority.context)?;
            let (record, association, generation, observed, blocked): (Vec<u8>, String, i64, bool, bool) = sqlx::query_as("SELECT record,association,sync_generation,observed_sequence IS NOT NULL OR observed_commitment IS NOT NULL,blocked FROM local_e2ee_outbox WHERE record=?").bind(&absence.record).fetch_one(&mut *tx).await?;
            valid(
                record == absence.record
                    && association == authority.association
                    && generation == authority.sync_generation
                    && !observed
                    && !blocked,
            )?;
            let change = codec::open(authority, &record)?;
            let local = load_change(&mut tx, &change.change_id)
                .await?
                .context("error encrypted-tail-history-lost")?;
            require_canonical_equality(&local, &change)?;
            let accepted: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM local_e2ee_accepted WHERE operation_id=?)",
            )
            .bind(&change.change_id)
            .fetch_one(&mut *tx)
            .await?;
            valid(local.server_seq.is_none() && !accepted)?;
            if authority.rotation_pending() {
                tx.commit().await?;
                return Ok(false);
            }
            if !authority.record_is_closed(&record)? {
                continue;
            }
            let mut projection = codec::parse(&record)?.projection;
            if change.op_type == "attachment_add" {
                projection = super::attachments::client::supersede(
                    &mut tx, authority, &change, projection, blob_dir,
                )
                .await?;
            }
            let replacement = codec::seal_projection(authority, &change, &projection)?;
            require_canonical_equality(&change, &codec::open(authority, &replacement)?)?;
            let n = sqlx::query("UPDATE local_e2ee_outbox SET record=? WHERE record=? AND observed_sequence IS NULL AND observed_commitment IS NULL AND blocked=0")
            .bind(&replacement).bind(&record).execute(&mut *tx).await?.rows_affected();
            valid(n == 1)?;
            changed = true;
        }
        if changed {
            crate::sync::crash::Crash::Tail.at("before-supersede-commit");
        }
        tx.commit().await?;
        if changed {
            crate::sync::crash::Crash::Tail.at("after-supersede-commit");
        }
        Ok(true)
    }
    /// Retain an observed immutable outcome before fetching a different representation.
    pub async fn observe_encrypted_tail(
        &self,
        authority: &Authority,
        mapping: &Mapping,
    ) -> Result<Vec<u8>> {
        let mut conn = self.acquire_writer().await?;
        let mut tx = begin_immediate(&mut conn).await?;
        let cursor = validate_binding_and_cursor(&mut tx, authority).await?;
        validate_mapping(&mut tx, mapping, cursor).await?;
        valid(mapping.sequence > authority.prefix)?;
        let (operation_id, record, observed_sequence, observed_commitment): (
            String,
            Vec<u8>,
            Option<i64>,
            Option<Vec<u8>>,
        ) = sqlx::query_as(
            "SELECT operation_id, record, observed_sequence, observed_commitment
             FROM local_e2ee_outbox WHERE operation_id = ?",
        )
        .bind(&mapping.operation_id)
        .fetch_one(&mut *tx)
        .await?;
        valid(
            operation_id == mapping.operation_id
                && observed_sequence.is_none_or(|sequence| sequence == mapping.sequence)
                && observed_commitment
                    .as_ref()
                    .is_none_or(|h| h.as_slice() == mapping.commitment),
        )?;
        if hash(&record) == mapping.commitment {
            let envelope = codec::parse(&record)?;
            valid(
                authority
                    .membership
                    .generation_allows(envelope.generation, u64::try_from(mapping.sequence)?),
            )?;
        }
        sqlx::query(
            "UPDATE local_e2ee_outbox SET observed_sequence = ?, observed_commitment = ?
             WHERE operation_id = ?",
        )
        .bind(mapping.sequence)
        .bind(mapping.commitment.as_slice())
        .bind(&mapping.operation_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(record)
    }
    pub async fn verify_encrypted_tail_outcome(
        &self,
        authority: &Authority,
        accepted: &Accepted,
    ) -> Result<()> {
        let result = self
            .commit_encrypted_tail_outcome(authority, accepted)
            .await;
        if result.as_ref().err().is_some_and(|error| {
            crate::sync::client::errors::has_code(error, "encrypted-tail-same-id-divergence")
        }) {
            let mut conn = self.acquire_writer().await?;
            sqlx::query("UPDATE local_e2ee_outbox SET blocked = 1 WHERE operation_id = ?")
                .bind(&accepted.mapping.operation_id)
                .execute(&mut *conn)
                .await?;
        }
        result
    }
    async fn commit_encrypted_tail_outcome(
        &self,
        authority: &Authority,
        accepted: &Accepted,
    ) -> Result<()> {
        let change = open_accepted_change(authority, accepted)?;
        let mut conn = self.acquire_writer().await?;
        let mut tx = begin_immediate(&mut conn).await?;
        let cursor = validate_binding_and_cursor(&mut tx, authority).await?;
        validate_mapping(&mut tx, &accepted.mapping, cursor).await?;
        let observed = load_observed_outcome(&mut tx, &change.change_id).await?;
        if let Some((sequence, commitment)) = observed {
            valid(
                sequence == Some(accepted.mapping.sequence)
                    && commitment.as_deref() == Some(accepted.mapping.commitment.as_slice()),
            )?;
        }
        let local = load_change(&mut tx, &change.change_id)
            .await?
            .context("error encrypted-tail-local-missing")?;
        require_canonical_equality(&local, &change)?;
        accept_local_change(&mut tx, authority, accepted, &change).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn encrypted_tail_has_batch_work(&self, authority: &Authority) -> Result<bool> {
        let mut conn = self.acquire_reader().await?;
        validate_binding_and_cursor(&mut conn, authority).await?;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM local_e2ee_outbox")
            .fetch_one(&mut *conn)
            .await?;
        if count > 1 {
            return Ok(true);
        }
        if count == 1 {
            return Ok(false);
        }
        let ops: Vec<String> = sqlx::query_scalar("SELECT op_type FROM changes WHERE server_seq IS NULL ORDER BY local_seq,created_at,change_id LIMIT 2").fetch_all(&mut *conn).await?;
        Ok(ops.len() == 2 && ops.iter().all(|op| op != "attachment_add"))
    }
    /// Returns the immutable unresolved inventory in dispatch order.
    pub async fn encrypted_tail_frozen_records(
        &self,
        authority: &Authority,
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let mut conn = self.acquire_reader().await?;
        let mut tx = sqlx::Connection::begin(&mut *conn).await?;
        validate_binding_and_cursor(&mut tx, authority).await?;
        let rows: Vec<(String, Vec<u8>, String, i64, bool)> = sqlx::query_as(
            "SELECT operation_id,record,association,sync_generation,blocked FROM local_e2ee_outbox ORDER BY position LIMIT 129"
        ).fetch_all(&mut *tx).await?;
        valid(rows.len() <= BATCH_COUNT)?;
        let mut result = Vec::with_capacity(rows.len());
        for (id, record, association, generation, blocked) in rows {
            valid(association == authority.association && generation == authority.sync_generation)?;
            ensure!(!blocked, "error encrypted-tail-integrity-blocked");
            let change = codec::open(authority, &record)?;
            valid(change.change_id == id)?;
            result.push((id, record));
        }
        tx.commit().await?;
        Ok(result)
    }

    /// Accepts exact frozen ciphertext together with every local domain effect.
    pub async fn accept_encrypted_tail_batch(
        &self,
        authority: &Authority,
        mappings: &[Mapping],
    ) -> Result<()> {
        valid(!mappings.is_empty() && mappings.len() <= BATCH_COUNT)?;
        let mut conn = self.acquire_writer().await?;
        let mut tx = begin_immediate(&mut conn).await?;
        let cursor = validate_binding_and_cursor(&mut tx, authority).await?;
        let mut accepted_records = Vec::new();
        let mut ids = HashSet::new();
        let mut sequences = HashSet::new();
        for mapping in mappings {
            valid(ids.insert(&mapping.operation_id) && sequences.insert(mapping.sequence))?;
            validate_mapping(&mut tx, mapping, cursor).await?;
            let (record, sequence, commitment): (Vec<u8>, Option<i64>, Option<Vec<u8>>) = sqlx::query_as(
                "SELECT record,observed_sequence,observed_commitment FROM local_e2ee_outbox WHERE operation_id=?"
            ).bind(&mapping.operation_id).fetch_one(&mut *tx).await?;
            valid(
                sequence.is_none_or(|n| n == mapping.sequence)
                    && commitment
                        .as_deref()
                        .is_none_or(|h| h == mapping.commitment),
            )?;
            let accepted = Accepted {
                mapping: mapping.clone(),
                record,
            };
            let change = open_accepted_change(authority, &accepted)?;
            let local = load_change(&mut tx, &change.change_id)
                .await?
                .context("error encrypted-tail-local-missing")?;
            require_canonical_equality(&local, &change)?;
            accepted_records.push((accepted, change));
        }
        for (accepted, change) in &accepted_records {
            accept_local_change(&mut tx, authority, accepted, change).await?;
        }
        crate::sync::crash::Crash::Tail.at("before-batch-commit");
        tx.commit().await?;
        crate::sync::crash::Crash::Tail.at("after-batch-commit");
        Ok(())
    }

    pub async fn apply_encrypted_tail_page(
        &self,
        authority: &Authority,
        page: &Page,
    ) -> Result<()> {
        valid(
            page.records.len() <= PAGE_COUNT
                && page.after >= authority.prefix
                && page.watermark >= page.after
                && page
                    .records
                    .iter()
                    .map(|accepted| accepted.record.len())
                    .sum::<usize>()
                    <= PAGE_BYTES,
        )?;
        let mut operation_ids = HashSet::new();
        let mut previous_sequence = page.after;
        let mut changes = Vec::new();
        for accepted in &page.records {
            valid(
                accepted.mapping.sequence > previous_sequence
                    && accepted.mapping.sequence <= page.watermark
                    && operation_ids.insert(&accepted.mapping.operation_id),
            )?;
            changes.push(open_accepted_change(authority, accepted)?);
            previous_sequence = accepted.mapping.sequence;
        }
        valid(
            page.cursor == previous_sequence && (!page.has_more || previous_sequence > page.after),
        )?;
        let mut conn = self.acquire_writer().await?;
        let mut tx = begin_immediate(&mut conn).await?;
        valid(validate_binding_and_cursor(&mut tx, authority).await? == page.after)?;
        let initial = initial_image_watermark(&mut tx).await?;
        let target = if initial == "pending" {
            Some(page.watermark)
        } else {
            initial.parse::<i64>().ok()
        };
        if let Some(target) = target {
            valid(page.watermark == target && page.after <= target)?;
        }
        let mut attachment_hashes = HashSet::new();
        let mut dependency_workspaces = HashSet::new();
        // Validate every mapping and local comparison before any domain effects.
        let mut local_presence = Vec::with_capacity(changes.len());
        for (accepted, change) in page.records.iter().zip(&changes) {
            validate_mapping(&mut tx, &accepted.mapping, page.after).await?;
            let local = load_change(&mut tx, &change.change_id).await?;
            if let Some(local) = &local {
                require_canonical_equality(local, change)?;
            }
            let observed = load_observed_outcome(&mut tx, &change.change_id).await?;
            if let Some((Some(sequence), Some(commitment))) = observed {
                valid(
                    sequence == accepted.mapping.sequence
                        && commitment.as_slice() == accepted.mapping.commitment,
                )?;
            }
            local_presence.push(local.is_some());
        }
        // All local ranks precede incoming relation application. Page IDs are unique,
        // and ranking does not insert history, so validated presence remains stable.
        for ((accepted, change), is_local) in page.records.iter().zip(&changes).zip(&local_presence)
        {
            if *is_local {
                persistence::update_change_server_seq(
                    &mut tx,
                    &change.change_id,
                    Some(accepted.mapping.sequence),
                )
                .await?;
                persistence::reconcile_epic_change(&mut tx, change).await?;
            }
        }
        for ((accepted, mut change), is_local) in
            page.records.iter().zip(changes).zip(local_presence)
        {
            if !is_local {
                // Applying lifecycle resolution can materialize deterministic history
                // needed by a later record in this page. It is not an identity-only echo.
                if let Some(generated) = load_change(&mut tx, &change.change_id).await? {
                    valid(super::recurrence::is_deterministic(&change))?;
                    require_canonical_equality(&generated, &change)?;
                    valid(generated.server_seq.is_none())?;
                    persistence::update_change_server_seq(
                        &mut tx,
                        &change.change_id,
                        Some(accepted.mapping.sequence),
                    )
                    .await?;
                } else {
                    change.server_seq = Some(accepted.mapping.sequence);
                    let generated = change.op_type == "create_task"
                        && super::recurrence::is_deterministic(&change)
                        && task_exists(&mut tx, &change.entity_id).await?;
                    apply_new_remote_change(&mut tx, &change, &mut attachment_hashes).await?;
                    if generated {
                        super::recurrence::adopt_earlier_generation(
                            &mut tx,
                            authority.prefix,
                            &change,
                        )
                        .await?;
                    }
                }
            }
            if let Some(workspace) = super::dependencies::affected_workspace(&change)? {
                dependency_workspaces.insert(workspace.to_owned());
            }
            super::notes::reconcile(&mut tx, authority.prefix, &change).await?;
            super::labels::reconcile(&mut tx, authority.prefix, &change).await?;
            super::attachments::client::accept(&mut tx, accepted, &change).await?;
            record_acceptance_and_clear_outbox(&mut tx, accepted).await?;
        }
        for workspace in dependency_workspaces {
            super::dependencies::reconcile(&mut tx, authority.prefix, &workspace).await?;
        }
        crate::attachments::lifecycle::reconcile_liveness_for_hashes_in_transaction(
            &mut tx,
            &attachment_hashes.into_iter().collect::<Vec<_>>(),
            &crate::attachments::lifecycle::SystemClock,
        )
        .await?;
        crate::sync::protocol::establish_protocol(&mut tx, 18).await?;
        db::set_meta(&mut tx, "sync_cursor", &page.cursor.to_string()).await?;
        if let Some(target) = target {
            let state = if page.cursor >= target {
                "ready".into()
            } else {
                target.to_string()
            };
            db::set_meta(&mut tx, INITIAL_IMAGE_WATERMARK, &state).await?;
        }
        crate::sync::crash::Crash::Tail.at("before-page-commit");
        tx.commit().await?;
        crate::sync::crash::Crash::Tail.at("after-page-commit");
        Ok(())
    }
}
