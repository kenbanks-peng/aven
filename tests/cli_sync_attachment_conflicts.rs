mod common;

use common::{TestEnv, TestServer, execute_sql, extract_ref, ok, png_bytes, scalar_i64};

#[test]
fn stale_task_delete_keeps_server_image_available_after_prune_and_restart() {
    let env = TestEnv::new();
    env.write_config("local:\n  attachment_lifecycle:\n    grace_days: 0\n");
    let server = TestServer::start(&env);
    let a = env.db("a.sqlite");
    let b = env.db("b.sqlite");
    let server_db = env.db("server.sqlite");
    let task = extract_ref(&ok(env.aven(&a, ["add", "image owner", "--project", "app"])));
    let image = env.path("image.png");
    std::fs::write(&image, png_bytes(2, 2)).unwrap();
    let added = ok(env.aven(&a, ["attachment", "add", &task, image.to_str().unwrap()]));
    let attachment = added
        .split_whitespace()
        .find_map(|word| word.strip_prefix("attachment_id="))
        .unwrap();
    env.sync_ok(&a, &server.url);
    env.sync_ok(&b, &server.url);
    ok(env.aven(&b, ["delete", &task]));
    ok(env.aven(&a, ["delete", &task]));
    ok(env.aven(&a, ["restore", &task]));
    env.sync_ok(&a, &server.url);
    env.sync_ok(&b, &server.url);
    env.sync_ok(&a, &server.url);
    assert_eq!(scalar_i64(&a, "SELECT deleted FROM tasks"), 0);
    assert_eq!(
        scalar_i64(&a, "SELECT count(*) FROM conflicts WHERE resolved = 0"),
        1
    );
    assert_eq!(
        scalar_i64(&server_db, "SELECT deleted FROM server_task_tombstones"),
        0
    );
    ok(env.aven(&server_db, ["attachment", "prune", "--apply"]));
    assert_eq!(
        scalar_i64(&server_db, "SELECT available FROM blob_inventory"),
        1
    );

    // An older server may have persisted the requested deletion instead of retention.
    drop(server);
    execute_sql(
        &server_db,
        "UPDATE server_task_tombstones SET deleted = 1;
         UPDATE blob_lifecycle SET unreferenced_at = '2000-01-01T00:00:00Z';",
    );
    let server = TestServer::start(&env);
    assert_eq!(
        scalar_i64(&server_db, "SELECT deleted FROM server_task_tombstones"),
        0
    );
    ok(env.aven(&server_db, ["attachment", "prune", "--apply"]));
    let c = env.db("fresh.sqlite");
    env.sync_ok(&c, &server.url);
    assert_eq!(scalar_i64(&c, "SELECT deleted FROM tasks"), 0);
    assert_eq!(scalar_i64(&c, "SELECT available FROM blob_inventory"), 1);

    // Explicit attachment deletion releases retention even for contested parents.
    ok(env.aven(&c, ["attachment", "delete", attachment]));
    env.sync_ok(&c, &server.url);
    ok(env.aven(&server_db, ["attachment", "prune", "--apply"]));
    assert_eq!(
        scalar_i64(&server_db, "SELECT available FROM blob_inventory"),
        0
    );
}

#[test]
fn nonconflicting_task_delete_still_releases_server_image() {
    let env = TestEnv::new();
    env.write_config("local:\n  attachment_lifecycle:\n    grace_days: 0\n");
    let server = TestServer::start(&env);
    let a = env.db("a.sqlite");
    let task = extract_ref(&ok(
        env.aven(&a, ["add", "ordinary delete", "--project", "app"])
    ));
    let image = env.path("image.png");
    std::fs::write(&image, png_bytes(2, 2)).unwrap();
    ok(env.aven(&a, ["attachment", "add", &task, image.to_str().unwrap()]));
    env.sync_ok(&a, &server.url);
    ok(env.aven(&a, ["delete", &task]));
    env.sync_ok(&a, &server.url);
    let server_db = env.db("server.sqlite");
    assert_eq!(
        scalar_i64(&server_db, "SELECT deleted FROM server_task_tombstones"),
        1
    );
    ok(env.aven(&server_db, ["attachment", "prune", "--apply"]));
    assert_eq!(
        scalar_i64(&server_db, "SELECT available FROM blob_inventory"),
        0
    );
}
