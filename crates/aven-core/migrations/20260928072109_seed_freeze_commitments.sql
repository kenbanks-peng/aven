-- Digest of the frozen capture's stored inputs, written with the package.
ALTER TABLE local_shared_capture_journal ADD COLUMN frozen_capture_commitment BLOB;

-- The package object that carries each selected image, written with the package.
ALTER TABLE local_shared_capture_images ADD COLUMN object_id BLOB;
