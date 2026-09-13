ALTER TABLE tasks
ADD COLUMN source_with_ios TEXT NOT NULL DEFAULT 'unknown'
CHECK (source_with_ios IN ('cli', 'tui', 'api', 'ios', 'unknown'));

UPDATE tasks SET source_with_ios = source;
ALTER TABLE tasks DROP COLUMN source;
ALTER TABLE tasks RENAME COLUMN source_with_ios TO source;
