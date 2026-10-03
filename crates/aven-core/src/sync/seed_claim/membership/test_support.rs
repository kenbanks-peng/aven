use super::super::*;
use super::{
    Declaration, Evidence, EvidenceRecord, Invitation, Joiner, Membership, RotationMaterial,
    VerifiedKeys, encoding,
};
use crate::{
    db::Database,
    sync::{bootstrap_format, bootstrap_staging as staging},
};

pub(crate) struct Fixture {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) db: Database,
    pub(crate) seed: SeedAuthority,
    pub(crate) key: LocalSharedStatePackageKey,
    pub(crate) package: bootstrap_format::Package,
    pub(crate) publication: Publication,
}
impl Fixture {
    pub(crate) async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = Database::open(&dir.path().join("source.db")).await.unwrap();
        let blob_dir = dir.path().to_path_buf();
        Self::publishing(dir, &source, &blob_dir).await
    }

    /// Publishes `source`'s current shared state as the bootstrap. The
    /// source's durable capture is released once packaged.
    pub(crate) async fn publishing(
        dir: tempfile::TempDir,
        source: &Database,
        blob_dir: &std::path::Path,
    ) -> Self {
        let context = LocalSharedStatePackageContext {
            vault_id: [1; 32],
            generation_id: [2; 32],
        };
        let key = LocalSharedStatePackageKey::new([3; 32]);
        let seed = SeedAuthority::generate(context, &key, [4; 32]).unwrap();
        source
            .capture_local_shared_state_never_dispatched()
            .await
            .unwrap();
        let package = source
            .package_local_shared_state_never_dispatched(
                blob_dir,
                context,
                &key,
                seed.genesis().commitment(),
            )
            .await
            .unwrap()
            .upload_package();
        let candidate = source
            .resume_local_shared_state_never_dispatched()
            .await
            .unwrap()
            .unwrap();
        source
            .cancel_local_shared_state_never_dispatched(candidate.candidate_id())
            .await
            .unwrap();
        let p = seed.prepare_bootstrap_publication(&package, &key).unwrap();
        let db = Database::open(&dir.path().join("server.db")).await.unwrap();
        let setup_secret = Secret::new([5; 32]);
        db.issue_e2ee_server_setup(&setup_secret, [4; 32], u64::MAX)
            .await
            .unwrap();
        db.admit_seed_claim(
            &seed.genesis().claim_bytes(),
            ClaimAuthentication::SetupSecret(&setup_secret),
        )
        .await
        .unwrap();
        let auth = staging::Authentication {
            vault_id: context.vault_id,
            genesis_commitment: seed.genesis().commitment(),
            bearer: seed.bearer(),
        };
        let mut components = Vec::new();
        for (i, c) in [
            staging::Component::DataCatalog,
            staging::Component::PrefixCatalog,
            staging::Component::ImageCatalog,
        ]
        .into_iter()
        .enumerate()
        {
            components.push((c, package.catalogs[i].chunks(1048576).collect::<Vec<_>>()));
        }
        components.push((
            staging::Component::Manifest,
            package.manifest.iter().map(Vec::as_slice).collect(),
        ));
        components.push((
            staging::Component::State,
            package.state.iter().map(Vec::as_slice).collect(),
        ));
        let budget = staging::Budget {
            bytes: components
                .iter()
                .flat_map(|(_, v)| v)
                .map(|v| v.len() as u64)
                .sum(),
            chunks: components.iter().map(|(_, v)| v.len() as u64).sum(),
        };
        db.declare_bootstrap_staging(&auth, &package.descriptor, budget)
            .await
            .unwrap();
        for (component, records) in components {
            for (index, bytes) in records.iter().enumerate() {
                db.put_bootstrap_chunk(
                    &auth,
                    staging::PutChunk {
                        bootstrap_id: p.binding().bootstrap_id,
                        descriptor_commitment: p.binding().descriptor_commitment,
                        component,
                        index: index as u64,
                        bytes,
                    },
                )
                .await
                .unwrap();
            }
        }
        db.publish_bootstrap(
            &auth,
            staging::PublishBootstrap {
                bootstrap_id: p.binding().bootstrap_id,
                descriptor_commitment: p.binding().descriptor_commitment,
                record: p.record(),
            },
            Default::default(),
        )
        .await
        .unwrap();
        Self {
            dir,
            db,
            seed,
            key,
            package,
            publication: p,
        }
    }
}

struct Signed {
    seed: SeedAuthority,
    descriptor: Vec<u8>,
    record: Vec<u8>,
    membership: Membership,
    key: LocalSharedStatePackageKey,
}

fn signed_publication() -> Signed {
    let seed = publication::fixture::seed();
    let descriptor = publication::fixture::descriptor(seed.genesis());
    let binding = PublicationBinding::from_descriptor(seed.genesis(), &descriptor).unwrap();
    let (core, state, attachments) = publication::components(seed.genesis(), &binding);
    let record = publication::fixture::signed(&seed, &core, &state, &attachments);
    let membership = Membership::from_publication(seed.genesis(), &descriptor, &record).unwrap();
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/genesis.json")).unwrap();
    let key = LocalSharedStatePackageKey::new(
        hex::decode(fixture["generation_secret"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap(),
    );
    Signed {
        seed,
        descriptor,
        record,
        membership,
        key,
    }
}

/// The fixed genesis seed, its signed publication membership and initial key.
pub(crate) fn publication() -> (SeedAuthority, Membership, LocalSharedStatePackageKey) {
    let s = signed_publication();
    (s.seed, s.membership, s.key)
}

pub(crate) fn content_authority() -> (Membership, VerifiedKeys) {
    let (_, membership, key) = publication();
    let keys = membership.verify_initial_key(&key).unwrap();
    (membership, keys)
}

/// Fixture membership before and after the seed admits the first peer, whose
/// Ed25519 seed is repeated 33.
struct Bases {
    signed: Signed,
    admitted: Membership,
    admission: [Vec<u8>; 3],
}

fn bases() -> Bases {
    let signed = signed_publication();
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/membership.json")).unwrap();
    let field = |name: &str| hex::decode(fixture[name].as_str().unwrap()).unwrap();
    let admission = [field("declaration"), field("request"), field("admission")];
    let admitted = signed
        .membership
        .append(&admission[0], &admission[1], &admission[2])
        .unwrap();
    Bases {
        signed,
        admitted,
        admission,
    }
}

thread_local! {
    static BASES: Bases = bases();
}

pub(crate) fn fuzz(data: &[u8]) {
    use crate::sync::fuzz::Input;
    BASES.with(|b| {
        let mut input = Input(data);
        let selector = input.byte();
        let base = if input.byte() % 2 == 0 {
            &b.signed.membership
        } else {
            &b.admitted
        };
        match selector % 6 {
            0 => {
                if let Ok((evidence, membership)) = Evidence::decode(input.0) {
                    let again = Evidence::decode(&serde_json::to_vec(&evidence).unwrap())
                        .expect("verified evidence re-encodes");
                    assert_eq!(again.1.head(), membership.head());
                }
            }
            1 => {
                if let Ok(genesis) = Genesis::from_record(input.0) {
                    assert_eq!(genesis.record().as_slice(), input.0);
                }
                let _ = Genesis::from_claim(input.0);
            }
            2 => {
                let descriptor = input.part();
                let genesis = b.signed.seed.genesis();
                if let Ok(m) = Membership::from_publication(genesis, descriptor, input.0) {
                    assert_eq!(m.sequence(), 1);
                }
            }
            3 => {
                let declaration = input.part();
                let request = input.part();
                if let Ok(next) = base.append(declaration, request, input.0) {
                    assert!(next.extends(base) && next.sequence() == base.sequence() + 1);
                }
            }
            4 => {
                // Re-sign fuzzed components so validation runs past the signature.
                let signer = if input.byte() % 2 == 0 {
                    *b.signed.seed.signing.expose()
                } else {
                    [33; 32]
                };
                let declaration = input.part();
                let request = input.part();
                let mut core = input.part().to_vec();
                let state = input.part();
                let attachments = input.0;
                if core.len() >= 32 {
                    let at = core.len() - 32;
                    core[at..].copy_from_slice(&codec::state_hash(state));
                }
                let signature = SigningKey::from_bytes(&signer)
                    .sign(&cce(MEMBERSHIP_SIGN, &[&core, attachments]));
                let mut record = vec![1];
                for part in [&core[..], state, attachments, &signature.to_bytes()] {
                    bytes(&mut record, part);
                }
                if let Ok(next) = base.append(declaration, request, &record) {
                    assert!(next.extends(base) && next.sequence() == base.sequence() + 1);
                    let _ = next.validate_key(&b.signed.key);
                }
            }
            _ => {
                let raw = input.0;
                let _ = Declaration::from_record(base, raw);
                let _ = VerifiedKeys::from_protected_storage(base, raw);
                let _ = Invitation::from_protected_storage(raw);
                let _ = Joiner::from_protected_storage(raw);
                let _ = RotationMaterial::from_protected_storage(raw);
                let _ = SeedAuthority::from_protected_storage(
                    raw,
                    b.signed.seed.genesis().context(),
                    &b.signed.key,
                );
            }
        }
    })
}

pub(crate) fn fuzz_seeds() -> Vec<(&'static str, Vec<u8>)> {
    use crate::sync::fuzz::frame;
    let b = bases();
    let s = &b.signed;
    let [declaration, request, admission] = &b.admission;
    let evidence = Evidence {
        genesis: s.seed.genesis().record().to_vec(),
        publication: s.record.clone(),
        descriptor: s.descriptor.clone(),
        transitions: vec![EvidenceRecord {
            declaration: declaration.clone(),
            request: request.clone(),
            record: admission.clone(),
        }],
    };
    let evidence = serde_json::to_vec(&evidence).unwrap();
    Evidence::decode(&evidence).unwrap();
    let (core, state, attachments, _) = encoding::components(admission).unwrap();
    let genesis = s.seed.genesis().record();
    vec![
        ("membership", frame(&[0, 0], &[&evidence])),
        ("membership", frame(&[1, 0], &[genesis])),
        ("membership", frame(&[2, 0], &[&s.descriptor, &s.record])),
        (
            "membership",
            frame(&[3, 0], &[declaration, request, admission]),
        ),
        (
            "membership",
            frame(
                &[4, 0, 0],
                &[declaration, request, core, state, attachments],
            ),
        ),
        ("membership", frame(&[5, 1], &[declaration])),
        ("wire", frame(&[4], &[&evidence])),
    ]
}
