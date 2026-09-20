use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use sqlx::SqliteConnection;

use super::super::apply::apply_remote_change;
use super::{ApplySyncPage, ClientSyncPage};
use crate::change_log::op_type;
use crate::db::{Database, begin_immediate, get_meta, set_meta};
use crate::sync::wire::{
    ChangeRow, ChangeWire, MAX_PUSH_BATCH, MAX_SYNC_REQUEST_BYTES, SyncRequest,
};

impl Database {
    pub(in crate::sync) async fn pending_sync_changes_exist(&self) -> Result<bool> {
        let mut conn = self.acquire_reader().await?;
        Ok(
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM changes WHERE server_seq IS NULL)")
                .fetch_one(&mut *conn)
                .await?,
        )
    }

    pub(in crate::sync) async fn pending_blob_counts(
        &self,
        known_server_blobs: &HashSet<String>,
    ) -> Result<crate::attachments::lifecycle::ByteCount> {
        let mut conn = self.acquire_reader().await?;
        let known = serde_json::to_string(known_server_blobs)?;
        let (count, bytes): (i64, i64) = sqlx::query_as(
            "WITH known(sha256) AS (
               SELECT value FROM json_each(?)
             ), pending AS (
               SELECT json_extract(payload, '$.workspace_id') AS workspace_id,
                      json_extract(payload, '$.sha256') AS sha256,
                      MAX(CAST(json_extract(payload, '$.byte_size') AS INTEGER)) AS byte_size
               FROM changes
               WHERE server_seq IS NULL AND op_type = 'attachment_add'
               GROUP BY workspace_id, sha256
             )
             SELECT COUNT(*), COALESCE(SUM(byte_size), 0)
             FROM pending LEFT JOIN known USING (sha256)
             WHERE known.sha256 IS NULL",
        )
        .bind(known)
        .fetch_one(&mut *conn)
        .await?;
        Ok(crate::attachments::lifecycle::ByteCount {
            count: u64::try_from(count)?,
            bytes: u64::try_from(bytes)?,
        })
    }

    pub(in crate::sync) async fn replica_sync_protocol(&self) -> Result<u32> {
        let mut conn = self.acquire_reader().await?;
        super::super::protocol::replica_protocol(&mut conn).await
    }

    pub(in crate::sync) async fn prepare_sync_discovery(&self, server: &str) -> Result<String> {
        let mut conn = self.acquire_writer().await?;
        validate_sync_server(&mut conn, server).await?;
        super::super::protocol::replica_protocol(&mut conn).await?;
        get_meta(&mut conn, "client_id")
            .await?
            .context("missing client id")
    }

    pub(in crate::sync) async fn block_sync_protocol(&self, protocol: u32) -> Result<()> {
        let mut conn = self.acquire_writer().await?;
        set_meta(&mut conn, "sync_blocked_protocol", &protocol.to_string()).await
    }

    pub async fn prepare_client_sync_page(
        &self,
        server: String,
        push_limit: usize,
        pull_limit: u32,
    ) -> Result<ClientSyncPage> {
        self.prepare_client_sync_page_at_protocol(server, push_limit, pull_limit, None)
            .await
    }

    pub(in crate::sync) async fn prepare_client_sync_page_at_protocol(
        &self,
        server: String,
        push_limit: usize,
        pull_limit: u32,
        protocol: Option<u32>,
    ) -> Result<ClientSyncPage> {
        let mut conn = self.acquire_writer().await?;
        validate_sync_server(&mut conn, &server).await?;
        let behavior_protocol = super::super::protocol::replica_protocol(&mut conn).await?;
        let protocol = protocol.unwrap_or(behavior_protocol);
        super::super::protocol::validate_behavior_protocol(protocol)?;
        if protocol < behavior_protocol {
            return Err(super::super::protocol::SyncCompatibilityError {
                server_protocol: protocol,
                client_protocol: behavior_protocol,
            }
            .into());
        }
        let client_id = get_meta(&mut conn, "client_id")
            .await?
            .context("missing client id")?;
        let after = sync_cursor(&mut conn).await?;
        let changes = load_unsynced_changes(&mut conn, push_limit.min(MAX_PUSH_BATCH)).await?;
        let request = bound_push_request(
            SyncRequest {
                protocol_version: Some(protocol),
                client_id,
                after,
                pull_limit: Some(pull_limit),
                changes,
            },
            MAX_SYNC_REQUEST_BYTES,
        )?;
        for change in &request.changes {
            super::super::protocol::validate_change(protocol, change)?;
        }
        Ok(ClientSyncPage {
            behavior_protocol,
            pending: request.changes.len(),
            request,
        })
    }

    pub async fn apply_client_sync_page(&self, page: ApplySyncPage) -> Result<usize> {
        self.apply_client_sync_page_with_context(page, None).await
    }

    pub(in crate::sync) async fn apply_client_sync_page_with_context(
        &self,
        page: ApplySyncPage,
        expected_behavior: Option<u32>,
    ) -> Result<usize> {
        let protocol = page
            .request
            .protocol_version
            .context("missing selected protocol")?;
        super::super::protocol::validate_behavior_protocol(protocol)?;
        let envelope = super::super::wire::validate_request_at_protocol(&page.request, protocol)?;
        let request_change_ids = page
            .request
            .changes
            .iter()
            .map(|change| change.change_id.clone())
            .collect::<Vec<_>>();
        super::super::wire::validate_response_at_protocol(
            protocol,
            envelope.after,
            envelope.pull_limit,
            &request_change_ids,
            &page.response,
        )?;
        let mut conn = self.acquire_writer().await?;
        apply_sync_response(&mut conn, page, expected_behavior).await
    }
}

async fn sync_cursor(conn: &mut SqliteConnection) -> Result<i64> {
    Ok(crate::db::get_meta(conn, "sync_cursor")
        .await?
        .unwrap_or_else(|| "0".to_string())
        .parse::<i64>()?)
}

async fn validate_sync_server(conn: &mut SqliteConnection, server: &str) -> Result<()> {
    let normalized = server.trim_end_matches('/');
    if let Some(existing) = get_meta(conn, "sync_server_url").await? {
        if existing != normalized {
            bail!(
                "error sync-server-changed existing={} requested={} hint=\"use a fresh database for a different sync server\"",
                existing,
                normalized
            );
        }
    } else {
        set_meta(conn, "sync_server_url", normalized).await?;
    }
    Ok(())
}

pub(super) fn bound_push_request(
    mut request: SyncRequest,
    byte_limit: usize,
) -> Result<SyncRequest> {
    let changes = std::mem::take(&mut request.changes);
    // The empty array already accounts for brackets and the complete request envelope.
    let mut bytes = serde_json::to_vec(&request)?.len();
    if bytes > byte_limit {
        bail!("error sync-request-envelope-too-large limit={byte_limit}");
    }
    for change in changes.into_iter().take(MAX_PUSH_BATCH) {
        let change_bytes = serde_json::to_vec(&change)?.len();
        let separator = usize::from(!request.changes.is_empty());
        if change_bytes + separator > byte_limit - bytes {
            if request.changes.is_empty() {
                bail!(
                    "error sync-change-exceeds-request-budget local_seq={} limit={byte_limit} hint=repair-pending-change",
                    change.local_seq
                );
            }
            break;
        }
        bytes += change_bytes + separator;
        request.changes.push(change);
    }
    Ok(request)
}

async fn load_unsynced_changes(
    conn: &mut SqliteConnection,
    limit: usize,
) -> Result<Vec<ChangeWire>> {
    let limit = limit as i64;
    let rows = sqlx::query_as!(
        ChangeRow,
        r#"SELECT change_id AS "change_id!: String", client_id AS "client_id!: String",
         local_seq AS "local_seq!: i64", entity_type AS "entity_type!: String",
         entity_id AS "entity_id!: String", field, op_type AS "op_type!: String",
         payload AS "payload!: String", base_version, created_at AS "created_at!: String",
         server_seq
         FROM changes WHERE server_seq IS NULL ORDER BY local_seq, created_at LIMIT ?"#,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(ChangeRow::into_wire).collect())
}

pub(super) async fn apply_sync_response(
    conn: &mut SqliteConnection,
    page: ApplySyncPage,
    expected_behavior: Option<u32>,
) -> Result<usize> {
    let mut applied = 0;
    let mut tx = begin_immediate(conn).await?;
    let current_behavior = super::super::protocol::replica_protocol(&mut tx).await?;
    if expected_behavior.is_some_and(|expected| expected != current_behavior) {
        bail!("error stale-sync-page replica-protocol-changed");
    }
    let selected = page
        .request
        .protocol_version
        .context("missing selected protocol")?;
    if selected < current_behavior {
        bail!("error stale-sync-page replica-protocol-regressed");
    }
    super::super::protocol::establish_protocol(&mut tx, selected).await?;
    let current_cursor = sync_cursor(&mut tx).await?;
    if current_cursor != page.request.after {
        bail!(
            "error stale-sync-page expected_cursor={} request_cursor={}",
            current_cursor,
            page.request.after
        );
    }
    super::update_change_server_seqs_if_missing(&mut tx, &page.response.push_acks).await?;
    super::reconcile_acknowledged_epic_memberships(&mut tx, &page.response.push_acks).await?;
    let existing_change_ids =
        super::load_existing_change_ids(&mut tx, &page.response.changes).await?;
    let mut affected_series = HashSet::new();
    let mut affected_attachment_hashes = HashSet::new();
    for change in &page.response.changes {
        if existing_change_ids.contains(change.change_id.as_str()) {
            super::verify_existing_change(&mut tx, change).await?;
            super::update_change_server_seq(&mut tx, &change.change_id, change.server_seq).await?;
            super::reconcile_epic_change(&mut tx, change).await?;
            continue;
        }
        super::collect_attachment_liveness_hashes(&mut tx, change, &mut affected_attachment_hashes)
            .await?;
        if super::is_epic_change(change) {
            let workspace_id = super::epic_change_workspace(change)?;
            crate::epic_membership::capture_snapshot_baseline(
                &mut tx,
                workspace_id,
                &change.entity_id,
            )
            .await?;
        }
        let related_mutation = matches!(
            change.op_type.as_str(),
            op_type::RELATED_ADD | op_type::RELATED_REMOVE
        );
        if related_mutation {
            super::insert_wire_change(&mut tx, change).await?;
        }
        apply_remote_change(&mut tx, change).await?;
        if change.entity_type == "recurrence_series" {
            let workspace_id = change
                .payload
                .get("workspace_id")
                .and_then(serde_json::Value::as_str)
                .context("recurrence change missing workspace_id")?;
            affected_series.insert((workspace_id.to_string(), change.entity_id.clone()));
        } else if let Some(series_id) = change
            .payload
            .get("series_id")
            .and_then(serde_json::Value::as_str)
        {
            let workspace_id = change
                .payload
                .get("workspace_id")
                .and_then(serde_json::Value::as_str)
                .context("recurrence task change missing workspace_id")?;
            affected_series.insert((workspace_id.to_string(), series_id.to_string()));
        }
        if !related_mutation {
            super::insert_wire_change(&mut tx, change).await?;
        }
        super::reconcile_epic_change(&mut tx, change).await?;
        applied += 1;
    }
    for (workspace_id, series_id) in affected_series {
        let workspace_id: crate::ids::WorkspaceId = workspace_id.parse()?;
        let series_id: crate::recurrence::RecurrenceSeriesId = series_id.parse()?;
        let workspace = crate::workspaces::workspace_for_id(&mut tx, &workspace_id).await?;
        let at =
            chrono::DateTime::parse_from_rfc3339(&page.attempted_at)?.with_timezone(&chrono::Utc);
        crate::operations::recurrence::reconcile_recurrence_series_in_transaction(
            &mut tx, &workspace, &series_id, at,
        )
        .await?;
    }
    let affected_attachment_hashes = affected_attachment_hashes.into_iter().collect::<Vec<_>>();
    crate::attachments::lifecycle::reconcile_liveness_for_hashes_in_transaction(
        &mut tx,
        &affected_attachment_hashes,
        &crate::attachments::lifecycle::SystemClock,
    )
    .await?;
    let pushed = page.previous_pushed + page.response.push_acks.len() as i64;
    let pulled = page.previous_pulled + applied;
    set_meta(&mut tx, "sync_cursor", &page.response.cursor.to_string()).await?;
    set_meta(&mut tx, "sync_last_success_at", &page.attempted_at).await?;
    let pending: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM changes WHERE server_seq IS NULL)")
            .fetch_one(&mut *tx)
            .await?;
    let caught_up = !page.response.has_more && !pending;
    set_meta(
        &mut tx,
        "sync_metadata_caught_up",
        if caught_up { "1" } else { "0" },
    )
    .await?;
    if caught_up {
        set_meta(&mut tx, "sync_metadata_confirmed_at", &crate::ids::now()).await?;
    }

    set_meta(&mut tx, "sync_last_error", "").await?;
    set_meta(&mut tx, "sync_blocked_protocol", "").await?;
    set_meta(&mut tx, "sync_last_pushed", &pushed.to_string()).await?;
    set_meta(&mut tx, "sync_last_pulled", &pulled.to_string()).await?;
    set_meta(
        &mut tx,
        "sync_last_cursor",
        &page.response.cursor.to_string(),
    )
    .await?;
    tx.commit().await?;
    Ok(applied)
}
