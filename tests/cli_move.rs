mod common;

use common::{TestEnv, contains_all, extract_ref, fail, ok};

#[test]
fn move_preserves_task_identity_and_associated_data() {
    let env = TestEnv::new();
    let db = env.db("move.sqlite");
    ok(env.aven(&db, ["workspace", "create", "Source"]));
    ok(env.aven(&db, ["workspace", "create", "Target"]));
    ok(env.aven(
        &db,
        [
            "--workspace",
            "source",
            "project",
            "create",
            "Source Project",
        ],
    ));
    ok(env.aven(
        &db,
        [
            "--workspace",
            "target",
            "project",
            "create",
            "Target Project",
        ],
    ));
    ok(env.aven(
        &db,
        ["--workspace", "source", "label", "create", "important"],
    ));
    let task_ref = extract_ref(&ok(env.aven(
        &db,
        [
            "--workspace",
            "source",
            "add",
            "Move me",
            "--project",
            "Source Project",
            "--label",
            "important",
            "--metadata",
            "owner=Raine",
        ],
    )));
    ok(env.aven(
        &db,
        ["--workspace", "source", "note", &task_ref, "Keep this note"],
    ));

    let moved = ok(env.aven(
        &db,
        [
            "--workspace",
            "source",
            "move",
            &task_ref,
            "--to-workspace",
            "target",
            "--project",
            "Target Project",
        ],
    ));
    contains_all(
        &moved,
        &[
            "moved ",
            "workspace=target",
            "project=target-project",
            "moved=1 from=source to=target",
        ],
    );

    let detail = ok(env.aven(
        &db,
        [
            "--workspace",
            "target",
            "show",
            &task_ref,
            "--full",
            "--json",
        ],
    ));
    contains_all(
        &detail,
        &[
            "Move me",
            "target-project",
            "important",
            "owner",
            "Raine",
            "Keep this note",
        ],
    );
    let source = ok(env.aven(&db, ["--workspace", "source", "list", "--all"]));
    assert!(!source.contains("Move me"), "{source}");
}

#[test]
fn missing_target_project_rolls_back_without_moving_the_task() {
    let env = TestEnv::new();
    let db = env.db("move-rollback.sqlite");
    ok(env.aven(&db, ["workspace", "create", "Source"]));
    ok(env.aven(&db, ["workspace", "create", "Target"]));
    ok(env.aven(
        &db,
        [
            "--workspace",
            "source",
            "project",
            "create",
            "Source Project",
        ],
    ));
    let task_ref = extract_ref(&ok(env.aven(
        &db,
        [
            "--workspace",
            "source",
            "add",
            "Stay put",
            "--project",
            "Source Project",
        ],
    )));

    let error = fail(env.aven(
        &db,
        [
            "--workspace",
            "source",
            "move",
            &task_ref,
            "--to-workspace",
            "target",
            "--project",
            "Missing",
        ],
    ));
    contains_all(&error, &["error unknown-project", "input=Missing"]);
    let source = ok(env.aven(&db, ["--workspace", "source", "show", &task_ref]));
    contains_all(&source, &["Stay put"]);
}
