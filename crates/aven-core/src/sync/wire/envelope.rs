use super::*;
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};

pub(super) fn validate_blob_contracts(blobs: &[BlobUploadContract]) -> Result<()> {
    if blobs.len() > MAX_PUSH_BATCH {
        bail!(
            "error blob-batch-too-large limit={} got={}",
            MAX_PUSH_BATCH,
            blobs.len()
        );
    }
    let mut seen = HashSet::with_capacity(blobs.len());
    let mut hashes = HashSet::with_capacity(blobs.len());
    for blob in blobs {
        ensure_sync_id("workspace_id", &blob.workspace_id)?;
        validate_sha256_for_sync(&blob.sha256)?;
        validate_blob_size_for_sync(blob.byte_size)?;
        map_attachment_validation(validate_media_type(&blob.media_type))?;
        map_attachment_validation(validate_dimensions(Some(blob.width), Some(blob.height)))?;
        if !seen.insert((blob.workspace_id.as_str(), blob.sha256.as_str())) {
            bail!("error duplicate-blob-contract");
        }
        hashes.insert(blob.sha256.as_str());
    }
    if hashes.len() > MAX_BLOB_TRANSFER_OBJECTS {
        bail!(
            "error blob-batch-too-large limit={} got={}",
            MAX_BLOB_TRANSFER_OBJECTS,
            hashes.len()
        );
    }
    Ok(())
}

pub(super) fn validate_blob_hashes(hashes: &[String]) -> Result<()> {
    if hashes.len() > MAX_BLOB_TRANSFER_OBJECTS {
        bail!(
            "error blob-batch-too-large limit={} got={}",
            MAX_BLOB_TRANSFER_OBJECTS,
            hashes.len()
        );
    }
    let mut seen = HashSet::with_capacity(hashes.len());
    for hash in hashes {
        validate_sha256_for_sync(hash)?;
        if !seen.insert(hash.as_str()) {
            bail!("error duplicate-blob-hash");
        }
    }
    Ok(())
}

#[derive(Debug)]

struct SyncProtocolError {
    client: u32,
    server: u32,
}

impl std::fmt::Display for SyncProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "error sync-protocol-unsupported client={} server={}",
            self.client, self.server
        )
    }
}

impl std::error::Error for SyncProtocolError {}

fn sync_protocol_error(client: u32, server: u32) -> anyhow::Error {
    anyhow::Error::new(SyncProtocolError { client, server })
}

pub(super) fn validate_sync_protocol_version(client: u32, server: u32) -> Result<()> {
    if client != server {
        return Err(sync_protocol_error(client, server));
    }
    Ok(())
}

pub(super) fn validate_sync_request_protocol_version(client: Option<u32>) -> Result<()> {
    validate_sync_protocol_version(client.unwrap_or(0), SYNC_PROTOCOL_VERSION)
}

pub(super) fn request_pull_limit(requested: Option<u32>) -> Result<u32> {
    match requested {
        None => Ok(MAX_PULL_BATCH),
        Some(limit @ 1..=MAX_PULL_BATCH) => Ok(limit),
        Some(limit) => {
            bail!("error sync-pull-limit-out-of-range min=1 max={MAX_PULL_BATCH} got={limit}")
        }
    }
}

pub(super) fn validate_sync_request_envelope(
    request: &SyncRequest,
) -> Result<ValidatedSyncRequestEnvelope> {
    validate_request_at_protocol(request, SYNC_PROTOCOL_VERSION)
}

pub(super) fn validate_request_at_protocol(
    request: &SyncRequest,
    protocol: u32,
) -> Result<ValidatedSyncRequestEnvelope> {
    validate_sync_protocol_version(request.protocol_version.unwrap_or(0), protocol)?;
    validate_request_cursor(request.after)?;
    validate_push_batch_size(request.changes.len())?;
    Ok(ValidatedSyncRequestEnvelope {
        after: request.after,
        pull_limit: request_pull_limit(request.pull_limit)?,
        push_count: request.changes.len(),
    })
}

fn validate_request_cursor(after: i64) -> Result<()> {
    if after < 0 {
        bail!("error sync-after-out-of-range min=0 got={after}");
    }
    Ok(())
}

fn validate_push_batch_size(len: usize) -> Result<()> {
    if len > MAX_PUSH_BATCH {
        bail!("error sync-push-too-large limit={MAX_PUSH_BATCH} got={len}");
    }
    Ok(())
}

pub(super) fn validate_sync_response_for_request(
    after: i64,
    pull_limit: u32,
    request_change_ids: &[String],
    response: &SyncResponse,
) -> Result<()> {
    validate_response_at_protocol(
        SYNC_PROTOCOL_VERSION,
        after,
        pull_limit,
        request_change_ids,
        response,
    )
}

pub(super) fn validate_response_at_protocol(
    protocol: u32,
    after: i64,
    pull_limit: u32,
    request_change_ids: &[String],
    response: &SyncResponse,
) -> Result<()> {
    validate_sync_protocol_version(protocol, response.protocol_version)?;
    if response.changes.len() > pull_limit as usize {
        bail!(
            "error invalid-sync-response pull-too-large limit={} got={}",
            pull_limit,
            response.changes.len()
        );
    }
    if response.cursor < after {
        bail!(
            "error invalid-sync-response cursor-regressed after={} cursor={}",
            after,
            response.cursor
        );
    }
    validate_push_acks(request_change_ids, response)?;
    validate_pull_page(after, pull_limit, response)?;
    for change in &response.changes {
        super::super::protocol::validate_change(protocol, change)?;
    }
    validate_push_pull_overlap(response)?;
    Ok(())
}

fn validate_push_acks(request_change_ids: &[String], response: &SyncResponse) -> Result<()> {
    if response.push_acks.len() != request_change_ids.len() {
        bail!(
            "error invalid-sync-response push-ack-count expected={} got={}",
            request_change_ids.len(),
            response.push_acks.len()
        );
    }
    let expected = request_change_ids
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut seen = HashSet::with_capacity(response.push_acks.len());
    for ack in &response.push_acks {
        if !expected.contains(ack.change_id.as_str()) {
            bail!(
                "error invalid-sync-response unexpected-push-ack change_id={}",
                ack.change_id
            );
        }
        if ack.server_seq <= 0 {
            bail!(
                "error invalid-sync-response push-ack-server-seq change_id={} server_seq={}",
                ack.change_id,
                ack.server_seq
            );
        }
        if !seen.insert(ack.change_id.as_str()) {
            bail!(
                "error invalid-sync-response duplicate-push-ack change_id={}",
                ack.change_id
            );
        }
    }
    Ok(())
}

fn validate_pull_page(after: i64, pull_limit: u32, response: &SyncResponse) -> Result<()> {
    let mut previous = after;
    let mut change_ids = HashSet::with_capacity(response.changes.len());
    for change in &response.changes {
        validate_pulled_change(change)?;
        if !change_ids.insert(&change.change_id) {
            bail!(
                "error invalid-sync-response duplicate-pull-change change_id={}",
                change.change_id
            );
        }
        let server_seq = change.server_seq.with_context(|| {
            format!(
                "error invalid-sync-response missing-server-seq change_id={}",
                change.change_id
            )
        })?;
        if server_seq <= previous {
            bail!(
                "error invalid-sync-response server-seq-order previous={} server_seq={}",
                previous,
                server_seq
            );
        }
        previous = server_seq;
    }
    let expected_cursor = response
        .changes
        .last()
        .and_then(|change| change.server_seq)
        .unwrap_or(after);
    if response.cursor != expected_cursor {
        bail!(
            "error invalid-sync-response cursor-mismatch expected={} got={}",
            expected_cursor,
            response.cursor
        );
    }
    if response.has_more && response.changes.len() < pull_limit as usize {
        bail!(
            "error invalid-sync-response has-more-short-page returned={} limit={}",
            response.changes.len(),
            pull_limit
        );
    }
    Ok(())
}

fn validate_push_pull_overlap(response: &SyncResponse) -> Result<()> {
    let mut sequence_owners = HashMap::new();
    for (change_id, server_seq) in response
        .push_acks
        .iter()
        .map(|ack| (ack.change_id.as_str(), ack.server_seq))
        .chain(response.changes.iter().filter_map(|change| {
            change
                .server_seq
                .map(|seq| (change.change_id.as_str(), seq))
        }))
    {
        if let Some(owner) = sequence_owners.insert(server_seq, change_id)
            && owner != change_id
        {
            bail!(
                "error invalid-sync-response server-seq-owner-mismatch server_seq={} first={} second={}",
                server_seq,
                owner,
                change_id
            );
        }
    }
    let acked = response
        .push_acks
        .iter()
        .map(|ack| (ack.change_id.as_str(), ack.server_seq))
        .collect::<HashMap<_, _>>();
    for change in &response.changes {
        if let Some(acked_server_seq) = acked.get(change.change_id.as_str()) {
            let Some(pull_server_seq) = change.server_seq else {
                continue;
            };
            if *acked_server_seq != pull_server_seq {
                bail!(
                    "error invalid-sync-response push-pull-server-seq-mismatch change_id={} ack={} pull={}",
                    change.change_id,
                    acked_server_seq,
                    pull_server_seq
                );
            }
        }
    }
    Ok(())
}
