//! The frozen package as an upload source: clear metadata held in memory and
//! records read from SQLite one slot at a time.
use anyhow::{Context, Result, ensure};
use sqlx::{Connection, SqliteConnection};

use super::publication::staging::{ArtifactView, DeclarationView};
use super::{IMAGE_COMPONENT, MANIFEST_COMPONENT, STATE_COMPONENT, publication};
use crate::db::Database;
use crate::sync::LocalSharedStatePackageKey;
use crate::sync::bootstrap_staging::{Budget, Component, batch::Slot};
use crate::sync::shared_state::NeverDispatchedLocalSharedCapture;

const MAX_CATALOG_BYTES: i64 = 16 * 1024 * 1024;
const CATALOGS: [Component; 3] = [
    Component::DataCatalog,
    Component::PrefixCatalog,
    Component::ImageCatalog,
];

/// Every slot of a frozen package, in upload order, and the recipe each record
/// is verified against when it is read back.
pub struct FrozenUpload {
    candidate: String,
    descriptor: Vec<u8>,
    catalogs: [Vec<u8>; 3],
    declaration: DeclarationView,
    artifacts: Vec<ArtifactView>,
    slots: Vec<Slot>,
}

impl FrozenUpload {
    /// Opens bounded metadata and checks exact row identity and length coverage.
    /// Blob lengths are constrained before SQLite returns their contents.
    pub(crate) async fn open(conn: &mut SqliteConnection, candidate: &str) -> Result<Self> {
        let upload = Self::load_metadata(conn, candidate).await?;
        upload.check_rows(conn).await?;
        Ok(upload)
    }

    /// Loads bounded descriptor and catalog metadata without requiring records.
    /// Proof reconstruction and adoption do not consume ciphertext rows.
    pub(crate) async fn load_metadata(
        conn: &mut SqliteConnection,
        candidate: &str,
    ) -> Result<Self> {
        let descriptor: Vec<u8> = sqlx::query_scalar(
            "SELECT descriptor FROM local_shared_capture_publication
             WHERE candidate_id = ? AND length(descriptor) <= ?",
        )
        .bind(candidate)
        .bind(i64::try_from(publication::MAX_DESCRIPTOR_BYTES)?)
        .fetch_optional(&mut *conn)
        .await?
        .context("error seed-package-missing")?;
        let declaration = DeclarationView::decode(&descriptor)?;
        let mut expected = [0_u64; 3];
        for (class, length) in expected.iter_mut().enumerate() {
            *length = declaration
                .catalog_lengths(class)?
                .into_iter()
                .try_fold(0_u64, u64::checked_add)
                .context("error seed-package-mismatch")?;
        }
        ensure!(
            expected
                .iter()
                .all(|length| *length <= MAX_CATALOG_BYTES as u64),
            "error seed-package-mismatch"
        );
        let (data, prefix, images): (Vec<u8>, Vec<u8>, Vec<u8>) = sqlx::query_as(
            "SELECT data_catalog, prefix_catalog, image_catalog
             FROM local_shared_capture_publication
             WHERE candidate_id = ? AND length(data_catalog) = ?
               AND length(prefix_catalog) = ? AND length(image_catalog) = ?",
        )
        .bind(candidate)
        .bind(i64::try_from(expected[0])?)
        .bind(i64::try_from(expected[1])?)
        .bind(i64::try_from(expected[2])?)
        .fetch_optional(&mut *conn)
        .await?
        .context("error seed-package-missing")?;
        let frozen: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT frozen_descriptor_commitment FROM local_shared_capture_journal
             WHERE candidate_id = ?",
        )
        .bind(candidate)
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
        ensure!(
            frozen.as_deref() == Some(crate::sync::codec::hash(&descriptor).as_slice()),
            "error encrypted-local-shared-package-frozen-descriptor-mismatch"
        );
        Self::new(candidate.to_string(), descriptor, [data, prefix, images])
    }

    /// Checks catalogs against the descriptor, which commits every slot.
    pub(crate) fn new(
        candidate: String,
        descriptor: Vec<u8>,
        catalogs: [Vec<u8>; 3],
    ) -> Result<Self> {
        let declaration = DeclarationView::decode(&descriptor)?;
        let mut artifacts = declaration.artifacts(None);
        let mut slots = Vec::new();
        for (class, component) in CATALOGS.into_iter().enumerate() {
            let catalog = declaration.catalog(class, &catalogs[class])?;
            artifacts.extend(declaration.artifacts(Some(&catalog)));
            slots.extend(slots_of(component, &declaration.catalog_lengths(class)?));
        }
        // Upload order: manifest, state, then images.
        artifacts.sort_by_key(|artifact| match artifact.component {
            Component::Manifest => 0,
            Component::State => 1,
            _ => 2,
        });
        for artifact in &artifacts {
            slots.extend(slots_of(artifact.component, &artifact.lengths()));
        }
        Ok(Self {
            candidate,
            descriptor,
            catalogs,
            declaration,
            artifacts,
            slots,
        })
    }

    async fn check_rows(&self, conn: &mut SqliteConnection) -> Result<()> {
        let mut expected = self
            .slots
            .iter()
            .filter(|slot| !slot.component.is_catalog())
            .map(|slot| {
                let (component, object_id) = record_identity(slot.component);
                Ok((
                    component.to_string(),
                    object_id,
                    i64::try_from(slot.index)?,
                    i64::try_from(slot.len)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        expected.sort();
        let actual: Vec<(String, Vec<u8>, i64, i64)> = sqlx::query_as(
            "SELECT component, object_id, chunk_index, length(record)
             FROM local_shared_capture_package_records WHERE candidate_id = ?
             ORDER BY component, object_id, chunk_index LIMIT ?",
        )
        .bind(&self.candidate)
        .bind(i64::try_from(expected.len() + 1)?)
        .fetch_all(&mut *conn)
        .await?;
        ensure!(
            actual == expected,
            "error encrypted-local-shared-package-frozen-records-invalid"
        );
        Ok(())
    }

    /// Verifies every persisted record without retaining the package.
    pub(super) async fn verify_records(&self, conn: &mut SqliteConnection) -> Result<()> {
        for slot in self
            .slots
            .iter()
            .filter(|slot| !slot.component.is_catalog())
        {
            self.read_record(conn, slot).await?;
        }
        Ok(())
    }

    pub fn descriptor(&self) -> &[u8] {
        &self.descriptor
    }

    pub(crate) fn catalogs(&self) -> &[Vec<u8>; 3] {
        &self.catalogs
    }

    pub(crate) fn context_and_membership(
        &self,
    ) -> Result<(super::LocalSharedStatePackageContext, [u8; 32])> {
        Ok(publication::context_and_membership(&self.descriptor)?)
    }

    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    pub fn budget(&self) -> Budget {
        Budget {
            bytes: self.slots.iter().map(|slot| slot.len).sum(),
            chunks: self.slots.len() as u64,
        }
    }

    fn catalog_slice(&self, class: usize, index: u64) -> Result<&[u8]> {
        let lengths = self.declaration.catalog_lengths(class)?;
        let index = usize::try_from(index)?;
        ensure!(index < lengths.len(), "error seed-package-mismatch");
        let start: u64 = lengths[..index].iter().sum();
        let bytes = self.catalogs[class]
            .get(usize::try_from(start)?..usize::try_from(start + lengths[index])?)
            .context("error seed-package-mismatch")?;
        self.declaration.verify_slice(class, index, bytes)?;
        Ok(bytes)
    }

    async fn read_record(&self, conn: &mut SqliteConnection, slot: &Slot) -> Result<Vec<u8>> {
        let (component, object_id) = record_identity(slot.component);
        let record: Vec<u8> = sqlx::query_scalar(
            "SELECT record FROM local_shared_capture_package_records
             WHERE candidate_id = ? AND component = ? AND object_id = ?
               AND chunk_index = ? AND length(record) = ?",
        )
        .bind(&self.candidate)
        .bind(component)
        .bind(object_id)
        .bind(i64::try_from(slot.index)?)
        .bind(i64::try_from(slot.len)?)
        .fetch_optional(&mut *conn)
        .await?
        .context("error seed-package-missing")?;
        self.artifacts
            .iter()
            .find(|artifact| artifact.component == slot.component)
            .context("error seed-package-mismatch")?
            .verify_chunk(usize::try_from(slot.index)?, &record)
            .context("error seed-package-mismatch")?;
        Ok(record)
    }

    /// Reads records in separate short transactions and verifies each against
    /// its descriptor-bound catalog recipe before returning it to transport.
    pub async fn read(&self, database: &Database, slots: &[Slot]) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::with_capacity(slots.len());
        for slot in slots {
            let record = match slot.component {
                Component::DataCatalog => self.catalog_slice(0, slot.index)?.to_vec(),
                Component::PrefixCatalog => self.catalog_slice(1, slot.index)?.to_vec(),
                Component::ImageCatalog => self.catalog_slice(2, slot.index)?.to_vec(),
                _ => {
                    let mut conn = database.acquire_reader().await?;
                    let mut tx = conn.begin().await?;
                    let record = self.read_record(&mut tx, slot).await?;
                    tx.commit().await?;
                    record
                }
            };
            ensure!(
                record.len() as u64 == slot.len,
                "error seed-package-mismatch"
            );
            out.push(record);
        }
        Ok(out)
    }

    /// Authenticates the package and capture from one coherent reader snapshot.
    /// Image ciphertext is retained for at most one image at a time.
    pub(crate) async fn authenticate(
        &self,
        conn: &mut SqliteConnection,
        capture: &NeverDispatchedLocalSharedCapture,
        key: &LocalSharedStatePackageKey,
        membership: [u8; 32],
    ) -> Result<publication::AttachmentIndex> {
        let mut metadata = publication::Package {
            descriptor: self.descriptor.clone(),
            catalogs: self.catalogs.clone(),
            state: Vec::new(),
            manifest: Vec::new(),
            images: Vec::new(),
        };
        for slot in self
            .slots
            .iter()
            .filter(|slot| matches!(slot.component, Component::State | Component::Manifest))
        {
            let record = self.read_record(conn, slot).await?;
            match slot.component {
                Component::State => metadata.state.push(record),
                Component::Manifest => metadata.manifest.push(record),
                _ => unreachable!(),
            }
        }
        let index =
            publication::authenticate_metadata_capture(&metadata, capture, key, membership)?;
        for (object, sha256) in &index.objects {
            let mut records = Vec::new();
            for slot in self
                .slots
                .iter()
                .filter(|slot| slot.component == Component::Image(object.id))
            {
                records.push(self.read_record(conn, slot).await?);
            }
            publication::authenticate_indexed_image(&index, object.id, sha256, &records, key)?;
        }
        Ok(index)
    }

    /// Reads every record into one package for the established public adapter.
    pub async fn package(&self, database: &Database) -> Result<publication::Package> {
        let records = self.read(database, &self.slots).await?;
        let mut package = publication::Package {
            descriptor: self.descriptor.clone(),
            catalogs: self.catalogs.clone(),
            state: Vec::new(),
            manifest: Vec::new(),
            images: Vec::new(),
        };
        for (slot, record) in self.slots.iter().zip(records) {
            match slot.component {
                Component::State => package.state.push(record),
                Component::Manifest => package.manifest.push(record),
                Component::Image(object_id) => {
                    if package
                        .images
                        .last()
                        .is_none_or(|image| image.object_id != object_id)
                    {
                        package.images.push(publication::ImageRecords {
                            object_id,
                            records: Vec::new(),
                        });
                    }
                    package.images.last_mut().unwrap().records.push(record);
                }
                _ => {}
            }
        }
        package.images.sort_by_key(|image| image.object_id);
        Ok(package)
    }
}

fn record_identity(component: Component) -> (&'static str, Vec<u8>) {
    match component {
        Component::Manifest => (MANIFEST_COMPONENT, Vec::new()),
        Component::State => (STATE_COMPONENT, Vec::new()),
        Component::Image(id) => (IMAGE_COMPONENT, id.to_vec()),
        _ => unreachable!("catalogs are not records"),
    }
}

fn slots_of(component: Component, lengths: &[u64]) -> impl Iterator<Item = Slot> + '_ {
    lengths.iter().enumerate().map(move |(index, len)| Slot {
        component,
        index: index as u64,
        len: *len,
    })
}
