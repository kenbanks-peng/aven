use super::*;

impl Fixture {
    fn batch<'a>(&self, records: Vec<(Component, u64, &'a [u8])>) -> PutBatch<'a> {
        PutBatch {
            bootstrap_id: self.id,
            descriptor_commitment: self.commitment(),
            records,
        }
    }

    /// Every slot of `catalogs` (true) or of the records they describe.
    fn slots(&self, catalogs: bool) -> Vec<(Component, u64, &[u8])> {
        self.components()
            .into_iter()
            .filter(|(component, _)| component.is_catalog() == catalogs)
            .flat_map(|(component, records)| {
                records
                    .into_iter()
                    .enumerate()
                    .map(move |(index, bytes)| (component, index as u64, bytes))
            })
            .collect()
    }

    async fn stored(&self) -> Vec<(Vec<u8>, i64, Vec<u8>)> {
        let mut conn = self.server.acquire_reader().await.unwrap();
        sqlx::query_as(
            "SELECT component, chunk_index, bytes FROM server_bootstrap_chunks
             ORDER BY component, chunk_index",
        )
        .fetch_all(&mut *conn)
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn batch_stores_every_record_and_an_exact_retry_is_stored_again() {
    let f = Fixture::new().await;
    f.declare().await;
    f.server
        .put_bootstrap_batch(&f.auth(), f.batch(f.slots(true)))
        .await
        .unwrap();
    let data = f.slots(false);
    assert!(data.len() > 1 && data.len() <= crate::sync::bootstrap_staging::batch::MAX_RECORDS);
    f.server
        .put_bootstrap_batch(&f.auth(), f.batch(data.clone()))
        .await
        .unwrap();
    let full = f.status().await;
    assert!(
        full.components
            .iter()
            .all(|c| c.chunks.iter().all(|p| *p == Presence::Verified))
    );
    let before = f.stored().await;
    f.server
        .put_bootstrap_batch(&f.auth(), f.batch(data))
        .await
        .unwrap();
    assert_eq!(f.stored().await, before);
}

#[tokio::test]
async fn one_conflicting_record_rolls_back_the_whole_batch() {
    let f = Fixture::new().await;
    f.declare().await;
    f.server
        .put_bootstrap_batch(&f.auth(), f.batch(f.slots(true)))
        .await
        .unwrap();
    let manifest = f.package.manifest[0].as_slice();
    f.server
        .put_bootstrap_batch(&f.auth(), f.batch(vec![(Component::Manifest, 0, manifest)]))
        .await
        .unwrap();
    let before = f.stored().await;
    let mut changed = manifest.to_vec();
    *changed.last_mut().unwrap() ^= 1;
    let mut records: Vec<_> = f
        .slots(false)
        .into_iter()
        .filter(|(component, _, _)| *component != Component::Manifest)
        .collect();
    records.push((Component::Manifest, 0, &changed));
    let error = f
        .server
        .put_bootstrap_batch(&f.auth(), f.batch(records))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "error bootstrap-chunk-conflict");
    assert_eq!(f.stored().await, before);
}

#[tokio::test]
async fn batch_mixing_a_catalog_with_data_is_refused() {
    let f = Fixture::new().await;
    f.declare().await;
    let mut records = f.slots(true);
    records.push((Component::Manifest, 0, f.package.manifest[0].as_slice()));
    let error = f
        .server
        .put_bootstrap_batch(&f.auth(), f.batch(records))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "error bootstrap-batch-mixed");
    assert!(f.stored().await.is_empty());
}

#[tokio::test]
async fn batch_with_a_wrong_slot_length_stores_nothing() {
    let f = Fixture::new().await;
    f.declare().await;
    let mut records = f.slots(true);
    let short = &records[0].2[..records[0].2.len() - 1];
    records[0].2 = short;
    let error = f
        .server
        .put_bootstrap_batch(&f.auth(), f.batch(records))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "error bootstrap-chunk-shape");
    assert!(f.stored().await.is_empty());
}

#[tokio::test]
async fn expired_staging_refuses_batches_until_ensured_again() {
    let f = Fixture::new().await;
    f.declare().await;
    let mut conn = f.server.acquire_writer().await.unwrap();
    sqlx::query("UPDATE server_bootstrap_candidates SET expires_at = 0")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let error = f
        .server
        .put_bootstrap_batch(&f.auth(), f.batch(f.slots(true)))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "error bootstrap-staging-expired");
    assert!(f.stored().await.is_empty());
    f.server
        .ensure_bootstrap_staging(&f.auth(), f.id, f.commitment())
        .await
        .unwrap();
    f.server
        .put_bootstrap_batch(&f.auth(), f.batch(f.slots(true)))
        .await
        .unwrap();
}
