//! Fixed vectors for the freeze-digest encodings.
//!
//! Protected seed publication intents store a [`FreezeBinding`] whose digests
//! were computed with these encodings. Changing any expected value here
//! invalidates every stored intent: bump [`VALIDATION_VERSION`] alongside the
//! encoding change so old bindings fail closed instead of mismatching.
use sqlx::{Connection, SqliteConnection};

use super::*;

const CANDIDATE: &str = "candidate-1";

fn image_rows() -> Vec<MappingRow> {
    vec![
        (
            "00".repeat(32),
            "current_selected".into(),
            Some(vec![0x11; 32]),
        ),
        (
            "aa".repeat(32),
            "extra_selected".into(),
            Some(vec![0x22; 32]),
        ),
        ("ff".repeat(32), "unavailable".into(), None),
    ]
}

/// The two tables [`tables_digest`] reads, holding the vector rows out of
/// sort order plus another candidate's rows that must not contribute.
async fn capture_tables() -> SqliteConnection {
    let mut conn = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    sqlx::raw_sql(
        "CREATE TABLE local_shared_capture_changes (
             candidate_id TEXT NOT NULL,
             change_id TEXT NOT NULL,
             prefix_rank INTEGER NOT NULL,
             source_server_seq INTEGER,
             source_pending_rank INTEGER
         );
         CREATE TABLE local_shared_capture_images (
             candidate_id TEXT NOT NULL,
             sha256 TEXT NOT NULL,
             classification TEXT NOT NULL,
             object_id BLOB
         );",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    let changes: [(&str, super::super::RankRow<'_>); 4] = [
        (CANDIDATE, ("z9".into(), 2, None, Some(3))),
        (CANDIDATE, ("a".into(), 1, Some(4), None)),
        (CANDIDATE, ("m".into(), 3, Some(7), None)),
        ("other", ("b".into(), 1, Some(1), None)),
    ];
    for (candidate, (change, rank, server_seq, pending_rank)) in changes {
        sqlx::query("INSERT INTO local_shared_capture_changes VALUES (?, ?, ?, ?, ?)")
            .bind(candidate)
            .bind(change.into_owned())
            .bind(rank)
            .bind(server_seq)
            .bind(pending_rank)
            .execute(&mut conn)
            .await
            .unwrap();
    }
    let mut rows = image_rows();
    rows.reverse();
    let other: MappingRow = ("11".repeat(32), "unavailable".into(), None);
    for (candidate, (sha256, classification, object)) in rows
        .into_iter()
        .map(|row| (CANDIDATE, row))
        .chain([("other", other)])
    {
        sqlx::query("INSERT INTO local_shared_capture_images VALUES (?, ?, ?, ?)")
            .bind(candidate)
            .bind(sha256)
            .bind(classification)
            .bind(object)
            .execute(&mut conn)
            .await
            .unwrap();
    }
    conn
}

#[test]
fn image_mapping_digest_matches_version_one_vectors() {
    assert_eq!(
        hex::encode(mapping_digest(&[])),
        "b6b9432f7647fb2c098d476f95b6b5996ae43b42389cc04d680e873632887ca8"
    );
    assert_eq!(
        hex::encode(mapping_digest(&image_rows())),
        "2ace75554428ba03e80d1233ddc586d9111c5df1634b2ae987c5cd7e6f246a75"
    );
}

#[test]
fn history_digest_matches_version_one_vector() {
    assert_eq!(
        hex::encode(adoption::history_digest(
            b"[\"history\"]",
            b"{\"provenance\":1}"
        )),
        "bc58071aeb586b9cb4cc448bce3abc1632e4c9aeff70e1810d6e426f816aee43"
    );
}

#[tokio::test]
async fn tables_digest_matches_version_one_vectors() {
    let mut conn = capture_tables().await;
    assert_eq!(
        hex::encode(tables_digest(&mut conn, CANDIDATE).await.unwrap()),
        "b0c992d85b3355d66f36a2a93518e0becb66bf71f36df3d87de24064b50cc884"
    );
    assert_eq!(
        hex::encode(tables_digest(&mut conn, "absent").await.unwrap()),
        "ec6ab579774c7e593bbce4136ff5555c7ff49d0022c1e7ee8032a55836fdc613"
    );
}

#[tokio::test]
async fn capture_digest_matches_version_one_vector() {
    let mut conn = capture_tables().await;
    let identity = FrozenIdentity {
        candidate: CANDIDATE.into(),
        stream: "stream-1".into(),
        descriptor_commitment: [0x33; 32],
        snapshot: crate::sync::codec::hash(b"{\"a\":1}\n"),
        generation: 5,
        history: adoption::history_digest(b"[\"history\"]", b"{\"provenance\":1}"),
        tables: tables_digest(&mut conn, CANDIDATE).await.unwrap(),
    };
    assert_eq!(
        hex::encode(identity.digest()),
        "059c4e3ac3413369625dbb63b9a6fb3bd50f6d32daf02a614cf49cfb3e2c7895"
    );
}

#[test]
fn freeze_binding_serialization_matches_version_one_vector() {
    let binding = FreezeBinding {
        validation_version: VALIDATION_VERSION,
        capture_digest: [0x44; 32],
        image_mapping_digest: [0x55; 32],
    };
    let expected = concat!(
        r#"{"validation_version":1,"#,
        r#""capture_digest":[68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68,68],"#,
        r#""image_mapping_digest":[85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85,85]}"#
    );
    assert_eq!(serde_json::to_string(&binding).unwrap(), expected);
    assert_eq!(
        serde_json::from_str::<FreezeBinding>(expected).unwrap(),
        binding
    );
}
