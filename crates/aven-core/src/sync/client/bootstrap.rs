//! Seed bootstrap: claiming a server for a new sync and publishing the
//! seed's frozen snapshot.
//!
//! HTTP framing: POST /e2ee/bootstrap/v1. Control operations are
//! application/json, one externally tagged operation in a context envelope.
//! Package records travel only in binary batches (see
//! [`staging::batch`]), each carrying its context in the batch header; the
//! server answers a stored batch with `Stored`. Base64 strings and batch
//! records carry exact existing codec bytes, never a second encrypted package
//! representation. Setup and device credentials use Authorization: Bearer <64
//! lowercase hex digits>; only ClaimSetup uses setup authority. IDs and
//! payloads never enter URLs. JSON requests are bounded at the base64 length
//! of one chunk plus 4096 bytes of framing, batches at
//! [`staging::batch::MAX_BYTES`]. Status responses are bounded at 1 MiB. Busy
//! callers retry a bounded number of times.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use super::exchange::{self, Link};
use super::keys::ProtectedLocalKeyStore;
use crate::db::Database;
use crate::sync::shared_state::validated::{ProofCache, ValidatedSeed};
use crate::sync::{
    base64_bytes, bootstrap_staging as staging,
    seed_claim::{ClaimAuthentication, ClaimResult, Genesis, PublicationOutcome, Secret},
};

/// Logs one setup stage's duration at debug level when dropped.
pub(crate) struct StageTimer {
    stage: &'static str,
    started: std::time::Instant,
}

impl StageTimer {
    pub(crate) fn start(stage: &'static str) -> Self {
        Self {
            stage,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for StageTimer {
    fn drop(&mut self) {
        tracing::debug!(
            stage = self.stage,
            elapsed_ms = self.started.elapsed().as_secs_f64() * 1000.0,
            "setup stage finished"
        );
    }
}

pub const PATH: &str = "/e2ee/bootstrap/v1";
pub const REQUEST_LIMIT: usize = base64_bytes::encoded_len(staging::MAX_REQUEST_BYTES) + 4096;
pub const RESPONSE_LIMIT: usize = 1_048_576;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub vault: [u8; 32],
    pub genesis: [u8; 32],
    pub operation: Operation,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Operation {
    ClaimSetup {
        #[serde(with = "crate::sync::base64_bytes")]
        bytes: Vec<u8>,
    },
    ClaimBearer {
        #[serde(with = "crate::sync::base64_bytes")]
        bytes: Vec<u8>,
    },
    Declare {
        #[serde(with = "crate::sync::base64_bytes")]
        descriptor: Vec<u8>,
        budget: staging::Budget,
    },
    Status {
        bootstrap: [u8; 32],
    },
    Ensure {
        bootstrap: [u8; 32],
        commitment: [u8; 32],
    },
    Cancel {
        bootstrap: [u8; 32],
    },
    Publish {
        bootstrap: [u8; 32],
        commitment: [u8; 32],
        #[serde(with = "crate::sync::base64_bytes")]
        record: Vec<u8>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Reply {
    Claimed {
        vault: [u8; 32],
        claim: [u8; 32],
        genesis: [u8; 32],
    },
    Missing,
    Canceled,
    Staging(staging::StagingStatus),
    Stored,
    Published(#[serde(with = "crate::sync::base64_bytes")] Vec<u8>),
}

impl From<staging::Status> for Reply {
    fn from(value: staging::Status) -> Self {
        match value {
            staging::Status::Missing => Self::Missing,
            staging::Status::Canceled => Self::Canceled,
            staging::Status::Staging(status) => Self::Staging(status),
            staging::Status::Published(outcome) => {
                Self::Published(outcome.publication().record().to_vec())
            }
        }
    }
}

/// Bounded seed bootstrap exchanges with one server.
pub struct Client {
    link: Link,
    endpoint: url::Url,
}

impl Client {
    pub fn new(origin: &str, link: Link) -> Result<Self> {
        Ok(Self {
            link,
            endpoint: super::origin::endpoint(origin, PATH)?,
        })
    }

    pub async fn exchange(
        &self,
        genesis: &Genesis,
        secret: &Secret,
        operation: Operation,
    ) -> Result<Reply> {
        let claim = matches!(
            &operation,
            Operation::ClaimSetup { .. } | Operation::ClaimBearer { .. }
        );
        let bytes = serde_json::to_vec(&Envelope {
            vault: genesis.context().vault_id,
            genesis: genesis.commitment(),
            operation,
        })
        .map_err(|_| anyhow::anyhow!("error bootstrap-request"))?;
        ensure!(
            bytes.len() <= REQUEST_LIMIT,
            "error bootstrap-request-limit"
        );
        self.post(secret, exchange::json_content(), bytes, claim)
            .await
    }

    /// Stores one batch of package records. Every record is sent exactly as
    /// frozen; the server checks each against its descriptor slot.
    pub async fn put_batch(
        &self,
        secret: &Secret,
        header: &staging::batch::Header,
        records: &[&[u8]],
    ) -> Result<Reply> {
        let bytes = staging::batch::encode(header, records)?;
        let content_type = exchange::HttpHeader {
            name: "content-type".into(),
            value: staging::batch::CONTENT_TYPE.into(),
        };
        self.post(secret, content_type, bytes, false).await
    }

    async fn post(
        &self,
        secret: &Secret,
        content_type: exchange::HttpHeader,
        bytes: Vec<u8>,
        claim: bool,
    ) -> Result<Reply> {
        let bytes = exchange::post(
            &self.link,
            &self.endpoint,
            Some(secret),
            content_type,
            bytes,
            RESPONSE_LIMIT,
        )
        .await
        .map_err(|failure| match failure {
            exchange::Failure::Network => {
                anyhow::anyhow!("error bootstrap-network outcome-unknown")
            }
            exchange::Failure::SecureTransport => {
                anyhow::anyhow!("error bootstrap-tls outcome-unknown")
            }
            exchange::Failure::Malformed => anyhow::anyhow!("error bootstrap-response"),
            exchange::Failure::TooLarge => anyhow::anyhow!("error bootstrap-response-limit"),
            exchange::Failure::RequestBodyLimit => {
                anyhow::anyhow!("error sync-request-body-limit")
            }
            exchange::Failure::Refused { code, .. } => match code.as_deref() {
                Some("bootstrap-storage-already-claimed") => {
                    anyhow::anyhow!("error bootstrap-storage-already-claimed")
                }
                Some("bootstrap-setup-invitation-rejected") if claim => {
                    anyhow::anyhow!("error bootstrap-setup-invitation-rejected")
                }
                Some("bootstrap-setup-invitation-expired") if claim => {
                    anyhow::anyhow!("error bootstrap-setup-invitation-expired")
                }
                Some("attachment-quota-exceeded") => {
                    anyhow::anyhow!("error attachment-quota-exceeded")
                }
                _ => anyhow::anyhow!("error bootstrap-refused outcome-unknown"),
            },
        })?;
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("error bootstrap-response"))
    }

    /// Initial setup claim or exact bearer-authorized claim resumption.
    /// Authority must already be durably protected locally. Publication retires
    /// claim resumption; use the protected-intent resume path after dispatch.
    pub async fn claim(
        &self,
        genesis: &Genesis,
        authentication: ClaimAuthentication<'_>,
    ) -> Result<()> {
        let (secret, operation) = match authentication {
            ClaimAuthentication::SetupSecret(secret) => (
                secret,
                Operation::ClaimSetup {
                    bytes: genesis.claim_bytes(),
                },
            ),
            ClaimAuthentication::SeedBearer(secret) => (
                secret,
                Operation::ClaimBearer {
                    bytes: genesis.claim_bytes(),
                },
            ),
        };
        match self.exchange(genesis, secret, operation).await? {
            Reply::Claimed {
                vault,
                claim,
                genesis: commitment,
            } => ClaimResult {
                vault_id: vault,
                claim_id: claim,
                genesis_commitment: commitment,
            }
            .validate_pinned(genesis),
            _ => anyhow::bail!("error bootstrap-response"),
        }
    }

    /// Resume one frozen candidate, then validate outcome, adopt and clean up.
    /// Call after claim, source preparation, capture and packaging. This never
    /// creates authority, recaptures, cancels, or enables ordinary encrypted sync.
    pub async fn resume(
        &self,
        store: &ProtectedLocalKeyStore,
        database: &Database,
    ) -> Result<bool> {
        self.resume_reporting(store, database, &|_, _| {}).await
    }

    /// [`Self::resume`], reporting bytes the server holds of the package's
    /// exact total: first what it already held, then again after each
    /// stored batch. Only slots the server reports missing are sent.
    pub async fn resume_reporting(
        &self,
        store: &ProtectedLocalKeyStore,
        database: &Database,
        uploaded: &(dyn Fn(u64, u64) + Sync),
    ) -> Result<bool> {
        self.resume_validated(store, database, None, uploaded).await
    }

    /// [`Self::resume_reporting`], reusing a proof the caller already made
    /// so the frozen package is not authenticated again.
    pub(crate) async fn resume_validated(
        &self,
        store: &ProtectedLocalKeyStore,
        database: &Database,
        proof: Option<ValidatedSeed>,
        uploaded: &(dyn Fn(u64, u64) + Sync),
    ) -> Result<bool> {
        let mut proofs = ProofCache::new(proof);
        let (seed, intent) = store.seed_resume_intent(database, &mut proofs).await?;
        let publication = intent.publication(seed.genesis())?;
        let binding = publication.binding();
        let status = self
            .exchange(
                seed.genesis(),
                seed.bearer(),
                Operation::Status {
                    bootstrap: binding.bootstrap_id,
                },
            )
            .await?;
        let record = match status {
            Reply::Published(record) => record,
            Reply::Missing | Reply::Staging(_) => {
                let upload = store
                    .seed_upload(database, &intent, &mut proofs)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("error bootstrap-outcome-missing"))?;
                let _timer = StageTimer::start("upload");
                let budget = upload.budget();
                let staging = match status {
                    Reply::Missing => {
                        self.exchange(
                            seed.genesis(),
                            seed.bearer(),
                            Operation::Declare {
                                descriptor: upload.descriptor().to_vec(),
                                budget,
                            },
                        )
                        .await?
                    }
                    Reply::Staging(ref s) => {
                        ensure!(
                            s.descriptor_commitment == binding.descriptor_commitment
                                && s.stream_id == binding.stream_id
                                && s.budget == budget,
                            "error bootstrap-status-mismatch"
                        );
                        self.exchange(
                            seed.genesis(),
                            seed.bearer(),
                            Operation::Ensure {
                                bootstrap: binding.bootstrap_id,
                                commitment: binding.descriptor_commitment,
                            },
                        )
                        .await?
                    }
                    _ => unreachable!(),
                };
                let header = |records| staging::batch::Header {
                    vault: seed.genesis().context().vault_id,
                    genesis: seed.genesis().commitment(),
                    bootstrap: binding.bootstrap_id,
                    commitment: binding.descriptor_commitment,
                    records,
                };
                let mut staging = staging;
                let mut reported = false;
                // Catalog slices must verify before the server lists the
                // records they describe, so the second round sends those.
                for _ in 0..3 {
                    let Reply::Staging(status) = staging else {
                        anyhow::bail!("error bootstrap-response");
                    };
                    ensure!(
                        status.descriptor_commitment == binding.descriptor_commitment
                            && status.stream_id == binding.stream_id
                            && status.budget == budget,
                        "error bootstrap-status-mismatch"
                    );
                    let missing = missing_slots(upload.slots(), &status)?;
                    let listed = status.components.len();
                    let mut held = budget.bytes
                        - missing.iter().map(|slot| slot.len).sum::<u64>()
                        - unlisted_bytes(upload.slots(), &status);
                    if !reported {
                        tracing::debug!(held, total = budget.bytes, "server holds staged bytes");
                        uploaded(held, budget.bytes);
                        reported = true;
                    }
                    let mut sent = 0_u64;
                    for batch in pack(&missing, header) {
                        let records = upload.read(database, &batch).await?;
                        let refs: Vec<&[u8]> = records.iter().map(Vec::as_slice).collect();
                        let reply = self.put_batch(seed.bearer(), &header(batch), &refs).await?;
                        ensure!(
                            matches!(reply, Reply::Stored),
                            "error bootstrap-upload-refused"
                        );
                        let length: u64 = records.iter().map(|r| r.len() as u64).sum();
                        sent += length;
                        held += length;
                        uploaded(held, budget.bytes);
                    }
                    tracing::debug!(sent, "uploaded missing staged bytes");
                    // Every component was listed, so every missing slot was sent.
                    if listed == component_count(upload.slots()) {
                        break;
                    }
                    staging = self
                        .exchange(
                            seed.genesis(),
                            seed.bearer(),
                            Operation::Status {
                                bootstrap: binding.bootstrap_id,
                            },
                        )
                        .await?;
                }
                match self
                    .exchange(
                        seed.genesis(),
                        seed.bearer(),
                        Operation::Publish {
                            bootstrap: binding.bootstrap_id,
                            commitment: binding.descriptor_commitment,
                            record: publication.record().to_vec(),
                        },
                    )
                    .await?
                {
                    Reply::Published(record) => record,
                    _ => anyhow::bail!("error bootstrap-response"),
                }
            }
            _ => anyhow::bail!("error bootstrap-candidate-unavailable"),
        };
        let outcome =
            PublicationOutcome::from_response(seed.genesis(), intent.descriptor(), &record)?;
        ensure!(
            outcome.publication() == &publication,
            "error bootstrap-outcome-mismatch"
        );
        let _timer = StageTimer::start("adopt");
        store
            .adopt_seed_publication_with(database, &outcome, &mut proofs)
            .await
    }
}

/// Slots of `slots` the server lists as missing, in upload order.
fn missing_slots(
    slots: &[staging::batch::Slot],
    status: &staging::StagingStatus,
) -> Result<Vec<staging::batch::Slot>> {
    let mut missing = Vec::new();
    for listed in &status.components {
        let expected = slots
            .iter()
            .filter(|slot| slot.component == listed.component)
            .count();
        ensure!(
            listed.chunks.len() == expected,
            "error bootstrap-status-mismatch"
        );
    }
    for slot in slots {
        let listed = status
            .components
            .iter()
            .find(|listed| listed.component == slot.component);
        if let Some(listed) = listed
            && listed.chunks[slot.index as usize] == staging::Presence::Missing
        {
            missing.push(*slot);
        }
    }
    Ok(missing)
}

/// Bytes of components the server does not list yet because their
/// describing catalog has not verified.
fn unlisted_bytes(slots: &[staging::batch::Slot], status: &staging::StagingStatus) -> u64 {
    slots
        .iter()
        .filter(|slot| {
            !status
                .components
                .iter()
                .any(|listed| listed.component == slot.component)
        })
        .map(|slot| slot.len)
        .sum()
}

fn component_count(slots: &[staging::batch::Slot]) -> usize {
    let mut components: Vec<staging::Component> = Vec::new();
    for slot in slots {
        if !components.contains(&slot.component) {
            components.push(slot.component);
        }
    }
    components.len()
}

/// Splits `slots` into batches within every batch limit, keeping catalog
/// slices apart from the records catalogs describe.
fn pack(
    slots: &[staging::batch::Slot],
    header: impl Fn(Vec<staging::batch::Slot>) -> staging::batch::Header,
) -> Vec<Vec<staging::batch::Slot>> {
    let mut batches: Vec<Vec<staging::batch::Slot>> = Vec::new();
    let mut current: Vec<staging::batch::Slot> = Vec::new();
    let mut payload = 0_u64;
    for slot in slots {
        let fits = !current.is_empty()
            && current.len() < staging::batch::MAX_RECORDS
            && payload + slot.len <= staging::batch::MAX_PAYLOAD as u64
            && current[0].component.is_catalog() == slot.component.is_catalog()
            && staging::batch::header_len(&header(
                current.iter().copied().chain([*slot]).collect(),
            ))
            .is_some();
        if !fits && !current.is_empty() {
            batches.push(std::mem::take(&mut current));
            payload = 0;
        }
        current.push(*slot);
        payload += slot.len;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// A package's chunks by component, in upload order.
#[cfg(any(test, feature = "test-support"))]
pub fn components(
    package: &crate::sync::bootstrap_format::Package,
) -> Vec<(staging::Component, Vec<&[u8]>)> {
    let mut out = Vec::new();
    for (index, component) in [
        staging::Component::DataCatalog,
        staging::Component::PrefixCatalog,
        staging::Component::ImageCatalog,
    ]
    .into_iter()
    .enumerate()
    {
        out.push((
            component,
            package.catalogs[index].chunks(1_048_576).collect(),
        ));
    }
    out.push((
        staging::Component::Manifest,
        package.manifest.iter().map(Vec::as_slice).collect(),
    ));
    out.push((
        staging::Component::State,
        package.state.iter().map(Vec::as_slice).collect(),
    ));
    for image in &package.images {
        out.push((
            staging::Component::Image(image.object_id),
            image.records.iter().map(Vec::as_slice).collect(),
        ));
    }
    out
}
