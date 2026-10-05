CREATE TABLE task_workspace_history (
    task_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    PRIMARY KEY (task_id, workspace_id)
);

INSERT INTO task_workspace_history(task_id, workspace_id)
SELECT id, workspace_id FROM tasks;

DROP TRIGGER IF EXISTS tasks_au;
CREATE TRIGGER tasks_au AFTER UPDATE OF workspace_id, title, description, project_id, status, priority, deleted ON tasks BEGIN
    DELETE FROM task_search_documents WHERE workspace_id = old.workspace_id AND task_id = old.id;
    INSERT INTO task_search_documents(workspace_id, task_id, workspace_token, title, description, labels, notes, project_key, project_name, project_prefix, status, priority)
    VALUES (new.workspace_id, new.id, new.workspace_id, new.title, new.description,
        COALESCE((SELECT group_concat(label, ' ') FROM (SELECT label FROM task_labels tl WHERE tl.workspace_id = new.workspace_id AND tl.task_id = new.id ORDER BY tl.label)), ''),
        COALESCE((SELECT group_concat(body, ' ') FROM (SELECT body FROM notes n WHERE n.workspace_id = new.workspace_id AND n.task_id = new.id ORDER BY n.created_at DESC, n.id DESC)), ''),
        IFNULL((SELECT key FROM projects WHERE workspace_id = new.workspace_id AND id = new.project_id), ''),
        IFNULL((SELECT name FROM projects WHERE workspace_id = new.workspace_id AND id = new.project_id), ''),
        IFNULL((SELECT prefix FROM projects WHERE workspace_id = new.workspace_id AND id = new.project_id), ''),
        new.status, new.priority);
END;

CREATE TABLE server_e2ee_task_placements (task_id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL);
CREATE TABLE server_e2ee_task_workspace_history (task_id TEXT NOT NULL, workspace_id TEXT NOT NULL, PRIMARY KEY(task_id, workspace_id));

DROP TRIGGER local_e2ee_image_reference_update;
CREATE TRIGGER local_e2ee_image_reference_update BEFORE UPDATE ON local_e2ee_image_references
WHEN NEW.reference IS NOT OLD.reference OR NEW.parent IS NOT OLD.parent OR NEW.object IS NOT OLD.object
 OR NEW.origin IS NOT OLD.origin OR NEW.deleted < OLD.deleted
BEGIN SELECT RAISE(ABORT, 'authenticated reference identity is immutable'); END;

DROP TRIGGER server_e2ee_image_reference_update;
CREATE TRIGGER server_e2ee_image_reference_update BEFORE UPDATE ON server_e2ee_image_references
WHEN NEW.reference IS NOT OLD.reference OR NEW.parent IS NOT OLD.parent OR NEW.object IS NOT OLD.object
 OR NEW.deleted < OLD.deleted
BEGIN SELECT RAISE(ABORT, 'opaque reference identity is immutable'); END;

CREATE TABLE local_e2ee_epic_edges (
    workspace_id TEXT NOT NULL, child_task_id TEXT NOT NULL, epic_task_id TEXT NOT NULL,
    created_at TEXT NOT NULL, PRIMARY KEY(workspace_id, child_task_id)
);
