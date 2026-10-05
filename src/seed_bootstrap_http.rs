//! Server side of seed bootstrap: claiming a server and publishing the seed's
//! frozen snapshot. The wire framing is documented in
//! `aven_core::sync::client::bootstrap`. One active request per router bounds
//! concurrent core materialization. Batch bodies are framed and bounded
//! before a request waits for that permit.

use crate::http_admission;
#[cfg(test)]
use crate::{protected_local_keys::ProtectedLocalKeyStore, sync_http::HttpDriver};
use anyhow::{Result, ensure};
pub(crate) use aven_core::sync::client::bootstrap::{
    Envelope, Operation, PATH, REQUEST_LIMIT, RESPONSE_LIMIT, Reply,
};
use aven_core::{
    db::Database,
    sync::{
        bootstrap_staging::{self as staging, batch},
        seed_claim::{ClaimAuthentication, ClaimRefusal, Genesis, Secret},
    },
};
use axum::{
    Router,
    body::Bytes,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
    routing::post,
};
use std::{sync::Arc, time::Duration};

const TIMEOUT: Duration = Duration::from_secs(30);
/// Slowest upload rate a request body is given time for.
const MIN_UPLOAD_BYTES_PER_SECOND: u64 = 256 * 1024;
/// Bodies are collected up to the larger of the two framings; each framing
/// then enforces its own limit.
const COLLECT_LIMIT: usize = if REQUEST_LIMIT > batch::MAX_BYTES {
    REQUEST_LIMIT
} else {
    batch::MAX_BYTES
};
const CODES: http_admission::Codes = http_admission::codes!("bootstrap");

struct Server {
    database: Database,
    admission: http_admission::Admission,
    publication_policy: staging::PublicationPolicy,
}

/// A router serving only the bootstrap route.
/// Claims use the storage's unexpired issued setup verifier.
/// Bind loopback, a trusted VPN interface, or a TLS-protected private hop.
pub fn router(database: Database, publication_policy: staging::PublicationPolicy) -> Router {
    Router::new()
        .route(PATH, post(handle))
        .fallback(|| async { http_admission::refusal(StatusCode::NOT_FOUND, "not-found") })
        .method_not_allowed_fallback(|| async {
            http_admission::refusal(StatusCode::METHOD_NOT_ALLOWED, "method-not-allowed")
        })
        .with_state(Arc::new(Server {
            database,
            admission: http_admission::Admission::new(1),
            publication_policy,
        }))
}

async fn handle(State(server): State<Arc<Server>>, request: Request) -> Response {
    let server = &*server;
    // Time to receive the declared body at the slowest supported rate.
    let transfer = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
        .map_or(Duration::ZERO, |length| {
            Duration::from_millis(
                length.min(COLLECT_LIMIT as u64) * 1000 / MIN_UPLOAD_BYTES_PER_SECOND,
            )
        });
    let outcome = http_admission::dispatch_with(
        &server.admission,
        TIMEOUT + transfer,
        http_admission::BODY_TIMEOUT + transfer,
        request,
        COLLECT_LIMIT,
        |headers, bytes| handle_bounded(server, headers, bytes),
    )
    .await;
    http_admission::respond(&CODES, outcome)
}

/// A request whose framing was checked before it waited for a permit.
enum Framed {
    Json(Envelope),
    Batch(Bytes),
}

fn is_batch(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        == Some(batch::CONTENT_TYPE)
        && !headers.contains_key(header::CONTENT_ENCODING)
}

fn framing(
    headers: &HeaderMap,
    bytes: Option<Bytes>,
) -> Result<(Secret, Framed), http_admission::Refusal> {
    if is_batch(headers) {
        let bytes = bytes.ok_or_else(|| CODES.too_large())?;
        let secret = CODES.bearer(headers)?;
        batch::decode(&bytes)
            .map_err(|_| http_admission::Refusal::new(StatusCode::BAD_REQUEST, CODES.malformed))?;
        return Ok((secret, Framed::Batch(bytes)));
    }
    let bytes = CODES.json_body(headers, bytes)?;
    if bytes.len() > REQUEST_LIMIT {
        return Err(CODES.too_large());
    }
    let secret = CODES.bearer(headers)?;
    Ok((secret, Framed::Json(CODES.parse(&bytes)?)))
}

async fn handle_bounded(server: &Server, headers: HeaderMap, bytes: Option<Bytes>) -> Response {
    let (secret, framed) = match framing(&headers, bytes) {
        Ok(framed) => framed,
        Err(refusal) => return http_admission::operation_refusal(&CODES, &refusal.into()),
    };
    let claim = matches!(
        &framed,
        Framed::Json(Envelope {
            operation: Operation::ClaimSetup { .. } | Operation::ClaimBearer { .. },
            ..
        })
    );
    let result = match framed {
        Framed::Json(envelope) => dispatch(server, &secret, envelope).await,
        Framed::Batch(bytes) => store_batch(server, &secret, &bytes).await,
    };
    let reply = match result {
        Ok(reply) => reply,
        // Only a refusal decided inside the claim transaction is definite; any
        // other claim error, such as a storage timeout, leaves the outcome
        // unknown so the claimant keeps its authority and retries.
        Err(error) if claim => {
            return match error.downcast_ref::<ClaimRefusal>() {
                Some(ClaimRefusal::Unauthorized { claimed: false }) => http_admission::refusal(
                    StatusCode::FORBIDDEN,
                    "bootstrap-setup-invitation-rejected",
                ),
                Some(ClaimRefusal::Expired) => http_admission::refusal(
                    StatusCode::FORBIDDEN,
                    "bootstrap-setup-invitation-expired",
                ),
                Some(_) => claimed(),
                None => http_admission::operation_refusal(&CODES, &error),
            };
        }
        Err(error) if error.downcast_ref::<staging::Unauthorized>().is_some() => {
            return match server.database.e2ee_server_is_claimed().await {
                Ok(true) => claimed(),
                Ok(false) => http_admission::refusal(
                    StatusCode::FORBIDDEN,
                    "bootstrap-setup-invitation-rejected",
                ),
                Err(error) => http_admission::operation_refusal(&CODES, &error),
            };
        }
        Err(error) => return http_admission::operation_refusal(&CODES, &error),
    };
    http_admission::reply(&CODES, &reply, RESPONSE_LIMIT)
}

fn claimed() -> Response {
    http_admission::refusal(StatusCode::CONFLICT, "bootstrap-storage-already-claimed")
}

async fn dispatch(server: &Server, secret: &Secret, e: Envelope) -> Result<Reply> {
    let db = &server.database;
    let auth = staging::Authentication {
        vault_id: e.vault,
        genesis_commitment: e.genesis,
        bearer: secret,
    };
    Ok(match e.operation {
        Operation::ClaimSetup { ref bytes } | Operation::ClaimBearer { ref bytes } => {
            // Context mismatch must fail before the claim transaction can mutate.
            ensure!(
                bytes.len() == aven_core::sync::seed_claim::CLAIM_BYTES,
                "invalid claim"
            );
            let genesis = Genesis::from_claim(bytes)?;
            ensure!(
                genesis.context().vault_id == e.vault && genesis.commitment() == e.genesis,
                "invalid context"
            );
            let authentication = if matches!(e.operation, Operation::ClaimSetup { .. }) {
                ClaimAuthentication::SetupSecret(secret)
            } else {
                ClaimAuthentication::SeedBearer(secret)
            };
            let result = db.admit_seed_claim(bytes, authentication).await?;
            Reply::Claimed {
                vault: result.vault_id,
                claim: result.claim_id,
                genesis: result.genesis_commitment,
            }
        }
        Operation::Declare { descriptor, budget } => Reply::Staging(
            db.declare_bootstrap_staging(&auth, &descriptor, budget)
                .await?,
        ),
        Operation::Status { bootstrap } => {
            db.bootstrap_staging_status(&auth, bootstrap).await?.into()
        }
        Operation::Ensure {
            bootstrap,
            commitment,
        } => Reply::Staging(
            db.ensure_bootstrap_staging(&auth, bootstrap, commitment)
                .await?,
        ),
        Operation::Cancel { bootstrap } => {
            db.cancel_bootstrap_staging(&auth, bootstrap).await?.into()
        }
        Operation::Publish {
            bootstrap,
            commitment,
            record,
        } => Reply::Published(
            db.publish_bootstrap(
                &auth,
                staging::PublishBootstrap {
                    bootstrap_id: bootstrap,
                    descriptor_commitment: commitment,
                    record: &record,
                },
                server.publication_policy,
            )
            .await?
            .publication()
            .record()
            .to_vec(),
        ),
    })
}

async fn store_batch(server: &Server, secret: &Secret, bytes: &[u8]) -> Result<Reply> {
    let batch::Batch { header, records } = batch::decode(bytes)?;
    server
        .database
        .put_bootstrap_batch(
            &staging::Authentication {
                vault_id: header.vault,
                genesis_commitment: header.genesis,
                bearer: secret,
            },
            staging::PutBatch {
                bootstrap_id: header.bootstrap,
                descriptor_commitment: header.commitment,
                records: header
                    .records
                    .iter()
                    .zip(records)
                    .map(|(slot, bytes)| (slot.component, slot.index, bytes))
                    .collect(),
            },
        )
        .await?;
    Ok(Reply::Stored)
}

#[cfg(test)]
mod client;
#[cfg(test)]
pub(crate) use client::Client;

#[cfg(test)]
pub(crate) mod tests;
