DROP TRIGGER e2ee_history_delete;
DROP TRIGGER e2ee_history_update;

CREATE TABLE local_e2ee_outbox_next (
    position INTEGER PRIMARY KEY CHECK(position >= 1),
    operation_id TEXT NOT NULL UNIQUE REFERENCES changes(change_id),
    association TEXT NOT NULL,
    sync_generation INTEGER NOT NULL,
    record BLOB NOT NULL CHECK(length(record) <= 135640),
    observed_sequence INTEGER CHECK(observed_sequence IS NULL OR observed_sequence > 0),
    observed_commitment BLOB CHECK(observed_commitment IS NULL OR length(observed_commitment) = 32),
    blocked INTEGER NOT NULL DEFAULT 0 CHECK(blocked IN (0,1)),
    CHECK((observed_sequence IS NULL) = (observed_commitment IS NULL))
);
INSERT INTO local_e2ee_outbox_next(
    position, operation_id, association, sync_generation, record,
    observed_sequence, observed_commitment, blocked
)
SELECT 1, operation_id, association, sync_generation, record,
       observed_sequence, observed_commitment, blocked
FROM local_e2ee_outbox;
DROP TABLE local_e2ee_outbox;
ALTER TABLE local_e2ee_outbox_next RENAME TO local_e2ee_outbox;

CREATE TABLE local_e2ee_accepted_next (
    operation_id TEXT PRIMARY KEY REFERENCES changes(change_id),
    sequence INTEGER NOT NULL UNIQUE CHECK(sequence > 0),
    commitment BLOB NOT NULL CHECK(length(commitment) = 32),
    record BLOB NOT NULL CHECK(length(record) <= 135640)
);
INSERT INTO local_e2ee_accepted_next(operation_id, sequence, commitment, record)
SELECT operation_id, sequence, commitment, record FROM local_e2ee_accepted;
DROP TABLE local_e2ee_accepted;
ALTER TABLE local_e2ee_accepted_next RENAME TO local_e2ee_accepted;

CREATE TRIGGER e2ee_history_delete BEFORE DELETE ON changes
WHEN EXISTS(SELECT 1 FROM local_e2ee_outbox WHERE operation_id=OLD.change_id)
  OR EXISTS(SELECT 1 FROM local_e2ee_accepted WHERE operation_id=OLD.change_id)
BEGIN SELECT RAISE(ABORT, 'error encrypted-history-owned'); END;
CREATE TRIGGER e2ee_history_update BEFORE UPDATE ON changes
WHEN (EXISTS(SELECT 1 FROM local_e2ee_outbox WHERE operation_id=OLD.change_id)
  OR EXISTS(SELECT 1 FROM local_e2ee_accepted WHERE operation_id=OLD.change_id))
 AND (NEW.change_id IS NOT OLD.change_id OR NEW.client_id IS NOT OLD.client_id
  OR NEW.local_seq IS NOT OLD.local_seq OR NEW.entity_type IS NOT OLD.entity_type
  OR NEW.entity_id IS NOT OLD.entity_id OR NEW.field IS NOT OLD.field
  OR NEW.op_type IS NOT OLD.op_type OR NEW.payload IS NOT OLD.payload
  OR NEW.base_version IS NOT OLD.base_version OR NEW.created_at IS NOT OLD.created_at)
BEGIN SELECT RAISE(ABORT, 'error encrypted-history-owned'); END;
