-- A capture's large JSON documents, written once as zstd frames. Kept apart
-- from the journal row so the journal's later small updates never rewrite them.
-- Documents moved here from older journals stay plain JSON text.
CREATE TABLE local_shared_capture_documents (
    candidate_id TEXT PRIMARY KEY,
    snapshot BLOB NOT NULL,
    source_history BLOB,
    source_provenance BLOB,
    FOREIGN KEY (candidate_id) REFERENCES local_shared_capture_journal(candidate_id)
        ON DELETE CASCADE
);

INSERT INTO local_shared_capture_documents(
    candidate_id, snapshot, source_history, source_provenance
)
SELECT candidate_id, snapshot_json, source_history, source_provenance
FROM local_shared_capture_journal;

ALTER TABLE local_shared_capture_journal DROP COLUMN snapshot_json;
ALTER TABLE local_shared_capture_journal DROP COLUMN source_history;
ALTER TABLE local_shared_capture_journal DROP COLUMN source_provenance;
