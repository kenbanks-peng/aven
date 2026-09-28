//! The frozen package as an upload source: clear metadata held in memory and
//! records read from SQLite a batch at a time.
use anyhow::{Context, Result, ensure};
use sqlx::Connection;

use super::publication::staging::{ArtifactView, DeclarationView};
use super::{IMAGE_COMPONENT, MANIFEST_COMPONENT, STATE_COMPONENT};
use crate::db::Database;
use crate::sync::bootstrap_staging::{Budget, Component, batch::Slot};

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
    /// Checks the catalogs against the descriptor, which the caller has
    /// already matched with the frozen commitment.
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

    pub fn descriptor(&self) -> &[u8] {
        &self.descriptor
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

    /// Reads the records for `slots` in one read transaction and verifies each
    /// against its catalog recipe, so bytes changed since freezing never leave.
    pub async fn read(&self, database: &Database, slots: &[Slot]) -> Result<Vec<Vec<u8>>> {
        let mut conn = database.acquire_reader().await?;
        let mut tx = conn.begin().await?;
        let mut out = Vec::with_capacity(slots.len());
        for slot in slots {
            let record = match slot.component {
                Component::DataCatalog => self.catalog_slice(0, slot.index)?.to_vec(),
                Component::PrefixCatalog => self.catalog_slice(1, slot.index)?.to_vec(),
                Component::ImageCatalog => self.catalog_slice(2, slot.index)?.to_vec(),
                component => {
                    let (name, object_id) = match component {
                        Component::State => (STATE_COMPONENT, Vec::new()),
                        Component::Manifest => (MANIFEST_COMPONENT, Vec::new()),
                        Component::Image(id) => (IMAGE_COMPONENT, id.to_vec()),
                        _ => unreachable!("catalogs are handled above"),
                    };
                    let record: Vec<u8> = sqlx::query_scalar(
                        "SELECT record FROM local_shared_capture_package_records
                         WHERE candidate_id = ? AND component = ? AND object_id = ?
                           AND chunk_index = ?",
                    )
                    .bind(&self.candidate)
                    .bind(name)
                    .bind(object_id)
                    .bind(i64::try_from(slot.index)?)
                    .fetch_optional(&mut *tx)
                    .await?
                    .context("error seed-package-missing")?;
                    self.artifacts
                        .iter()
                        .find(|artifact| artifact.component == component)
                        .context("error seed-package-mismatch")?
                        .verify_chunk(usize::try_from(slot.index)?, &record)
                        .context("error seed-package-mismatch")?;
                    record
                }
            };
            ensure!(
                record.len() as u64 == slot.len,
                "error seed-package-mismatch"
            );
            out.push(record);
        }
        tx.commit().await?;
        Ok(out)
    }

    /// Reads every record into one package.
    pub async fn package(&self, database: &Database) -> Result<super::publication::Package> {
        let records = self.read(database, &self.slots).await?;
        let mut package = super::publication::Package {
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
                        package.images.push(super::publication::ImageRecords {
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

fn slots_of(component: Component, lengths: &[u64]) -> impl Iterator<Item = Slot> + '_ {
    lengths.iter().enumerate().map(move |(index, len)| Slot {
        component,
        index: index as u64,
        len: *len,
    })
}
