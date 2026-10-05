//! Fuzz hooks for publication packages and their decrypted domain state.
use super::*;
use crate::sync::fuzz::{Input, frame};

struct Slice<'a>(&'a [u8]);

impl domain::In for Slice<'_> {
    fn remaining(&self) -> u64 {
        self.0.len() as u64
    }
    fn read(&mut self, into: &mut [u8]) -> Result<()> {
        valid(into.len() <= self.0.len())?;
        let (head, rest) = self.0.split_at(into.len());
        into.copy_from_slice(head);
        self.0 = rest;
        Ok(())
    }
    fn skip(&mut self, n: u64) -> Result<()> {
        valid(n <= self.remaining())?;
        self.0 = &self.0[size(n)?..];
        Ok(())
    }
}

/// Fields after the descriptor and catalogs are records whose first byte
/// selects state (0), manifest (1) or an image whose ID is the next 32 bytes.
fn package(input: &mut Input<'_>) -> Package {
    let mut package = Package {
        descriptor: input.part().to_vec(),
        catalogs: [
            input.part().to_vec(),
            input.part().to_vec(),
            input.part().to_vec(),
        ],
        state: Vec::new(),
        manifest: Vec::new(),
        images: Vec::new(),
    };
    while !input.is_empty() {
        let mut record = Input(input.part());
        match record.byte() {
            0 => package.state.push(record.0.to_vec()),
            1 => package.manifest.push(record.0.to_vec()),
            _ => {
                let Ok(object_id) = record.0.get(..32).unwrap_or_default().try_into() else {
                    continue;
                };
                let records = vec![record.0[32..].to_vec()];
                match package.images.last_mut() {
                    Some(last) if last.object_id == object_id => last.records.extend(records),
                    _ => package.images.push(ImageRecords { object_id, records }),
                }
            }
        }
    }
    package
}

pub(crate) fn bootstrap(data: &[u8]) {
    let mut input = Input(data);
    match input.byte() % 5 {
        0 => {
            if let Ok(d) = Descriptor::decode(input.0) {
                assert_eq!(d.encode().unwrap(), input.0);
            }
        }
        1 => {
            let expected = u64::from(input.byte());
            let raw = input.0;
            let _ = decode_state_catalog(raw);
            if let Ok(images) = Images::decode(raw) {
                assert_eq!(images.encode().unwrap(), raw);
            }
            if let Ok(rows) = catalog::prefix_decode(raw, expected) {
                assert_eq!(catalog::prefix_encode(&rows).unwrap(), raw);
            }
        }
        2 => {
            let raw = input.0;
            let mappings = domain::decode_mappings(&mut Slice(raw));
            if let Ok((tables, decoded, stats)) = domain::decode(raw) {
                assert!(mappings.unwrap() == decoded);
                let (encoded, again) = domain::encode(&tables, &decoded).unwrap();
                assert_eq!(encoded, raw);
                assert_eq!(again, stats);
                let _ = projection::prefix(&tables);
                let empty = Images {
                    objects: Vec::new(),
                    parents: Vec::new(),
                    references: Vec::new(),
                };
                let _ = projection::images(&tables, &decoded, empty);
                let _ = capture(tables).validate();
            }
        }
        3 => {
            if let Ok(batch) = crate::sync::bootstrap_staging::batch::decode(input.0) {
                assert_eq!(batch.records.len(), batch.header.records.len());
            }
        }
        _ => {
            let package = package(&mut input);
            if validate_keyless(&package).is_ok() {
                let _ = decrypt_capture(&package, &key());
            }
        }
    }
}

fn key() -> LocalSharedStatePackageKey {
    LocalSharedStatePackageKey::new([3; 32])
}

pub(crate) fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    let package = crate::sync::fuzz::with_history(async |db| {
        let dir = tempfile::tempdir().unwrap();
        let context = crypto::LocalSharedStatePackageContext {
            vault_id: [1; 32],
            generation_id: [2; 32],
        };
        db.capture_local_shared_state_never_dispatched()
            .await
            .unwrap();
        db.package_local_shared_state_never_dispatched(dir.path(), context, &key(), [4; 32])
            .await
            .unwrap()
            .upload_package()
    });
    let d = Descriptor::decode(&package.descriptor).unwrap();
    let state = decode_state_catalog(&package.catalogs[0]).unwrap();
    let state_key =
        crypto::derive_bootstrap_class_key(&key(), d.context(), d.stream, d.bootstrap, 1).unwrap();
    let plaintext = crypto::decrypt_artifact(
        &state.encrypted(&package.state),
        d.context(),
        d.stream,
        d.bootstrap,
        2,
        1,
        &state_key,
        size(STATE_LIMIT).unwrap(),
    )
    .unwrap();
    domain::decode(&plaintext).unwrap();
    decrypt_capture(&package, &key()).unwrap();
    let prefix = catalog::prefix_decode(&package.catalogs[1], d.prefix)
        .unwrap()
        .len();
    let records: Vec<Vec<u8>> = [(0, &package.state), (1, &package.manifest)]
        .into_iter()
        .flat_map(|(kind, records)| records.iter().map(move |r| [&[kind][..], r].concat()))
        .collect();
    let mut parts = vec![&package.descriptor[..]];
    parts.extend(package.catalogs.iter().map(Vec::as_slice));
    parts.extend(records.iter().map(Vec::as_slice));
    parts.push(&[]);
    let mut seeds = vec![
        ("bootstrap", frame(&[0], &[&package.descriptor])),
        ("bootstrap", frame(&[1, 1], &[&package.catalogs[0]])),
        (
            "bootstrap",
            frame(&[1, prefix as u8], &[&package.catalogs[1]]),
        ),
        ("bootstrap", frame(&[1, 0], &[&package.catalogs[2]])),
        ("bootstrap", frame(&[2], &[&plaintext])),
        ("bootstrap", frame(&[4], &parts)),
    ];
    let batch = crate::sync::bootstrap_staging::batch::encode(
        &crate::sync::bootstrap_staging::batch::Header {
            vault: [1; 32],
            genesis: [5; 32],
            bootstrap: d.bootstrap,
            commitment: [6; 32],
            records: vec![crate::sync::bootstrap_staging::batch::Slot {
                component: crate::sync::bootstrap_staging::Component::State,
                index: 0,
                len: package.state[0].len() as u64,
            }],
        },
        &[&package.state[0]],
    )
    .unwrap();
    seeds.push(("bootstrap", frame(&[3], &[&batch])));
    seeds
}
