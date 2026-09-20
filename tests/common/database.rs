use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{Connection as _, SqliteConnection};

pub fn meta_value(db: &Path, key: &str) -> Option<String> {
    let runtime = test_runtime();
    runtime.block_on(async {
        let mut conn = open_test_db(db).await;
        sqlx::query_scalar::<_, String>("SELECT value FROM meta WHERE key = ?")
            .bind(key)
            .fetch_optional(&mut conn)
            .await
            .expect("read meta value")
    })
}

pub fn execute_sql(db: &Path, sql: &'static str) {
    let runtime = test_runtime();
    runtime.block_on(async {
        let mut conn = open_test_db(db).await;
        sqlx::raw_sql(sql)
            .execute(&mut conn)
            .await
            .expect("execute test sql");
    });
}

pub fn scalar_i64(db: &Path, sql: &'static str) -> i64 {
    let runtime = test_runtime();
    runtime.block_on(async {
        let mut conn = open_test_db(db).await;
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(&mut conn)
            .await
            .expect("read scalar value")
    })
}

fn test_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create tokio runtime")
}

pub async fn insert_task_fixtures(pool: &sqlx::SqlitePool, fixtures: &[(&str, &str, &str)]) {
    for (id, title, project_key) in fixtures {
        sqlx::query(
            "INSERT INTO tasks(id,title,description,project_id,status,priority,created_at,updated_at)
             VALUES (?, ?, '', (SELECT id FROM projects WHERE key = ?), 'inbox', 'none', 't', 't')",
        )
        .bind(id)
        .bind(title)
        .bind(project_key)
        .execute(pool)
        .await
        .expect("insert task fixture");
    }
}

async fn open_test_db(db: &Path) -> SqliteConnection {
    let options = SqliteConnectOptions::new()
        .filename(db)
        .create_if_missing(false)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5));
    SqliteConnection::connect_with(&options)
        .await
        .expect("open sqlite db")
}
