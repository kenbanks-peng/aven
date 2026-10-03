//! Logical snapshots for quiescent fixtures using isolated file-backed stores.
//! Assertion failures never print protected bytes or plaintext domain content.
use super::*;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub(crate) struct ProtectedState {
    tables: BTreeMap<String, [u8; 32]>,
    files: BTreeMap<PathBuf, [u8; 32]>,
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

impl ProtectedState {
    /// Includes all local tables, including membership/descriptor mirrors,
    /// active generation, cursors, watermarks, pending/frozen/accepted records,
    /// checkpoints and domain data. Protected files include keys and trust floors.
    /// SQLite page layout, WAL bytes and filesystem metadata are not state.
    pub(crate) async fn capture(db: &Database, isolated_keys: &Path) -> Self {
        let mut conn = aven_core::test_support::acquire(db).await.unwrap();
        sqlx::query("BEGIN").execute(&mut *conn).await.unwrap();
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'server_%' ORDER BY name",
        ).fetch_all(&mut *conn).await.unwrap();
        let mut tables = BTreeMap::new();
        // Dynamic SQL contains only double-quoted schema identifiers; values are never interpolated.
        for name in names {
            let columns: Vec<String> = sqlx::query(sqlx::AssertSqlSafe(format!(
                "PRAGMA table_info({})",
                quote(&name)
            )))
            .fetch_all(&mut *conn)
            .await
            .unwrap()
            .iter()
            .map(|row| row.get("name"))
            .collect();
            let select = columns
                .iter()
                .map(|c| {
                    let c = quote(c);
                    format!("typeof({c}) || '/' || CASE typeof({c}) WHEN 'text' THEN hex({c}) WHEN 'blob' THEN hex({c}) ELSE quote({c}) END")
                })
                .collect::<Vec<_>>()
                .join(",");
            let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT {select} FROM {}",
                quote(&name)
            )))
            .fetch_all(&mut *conn)
            .await
            .unwrap();
            let mut rows: Vec<Vec<String>> = rows
                .iter()
                .map(|row| (0..columns.len()).map(|i| row.get(i)).collect())
                .collect();
            rows.sort();
            tables.insert(
                name,
                Sha256::digest(serde_json::to_vec(&(columns, rows)).unwrap()).into(),
            );
        }
        sqlx::query("ROLLBACK").execute(&mut *conn).await.unwrap();
        let mut files = BTreeMap::new();
        fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, [u8; 32]>) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                if kind.is_dir() {
                    visit(root, &entry.path(), files);
                } else {
                    assert!(
                        kind.is_file(),
                        "isolated protected store must contain regular files"
                    );
                    let bytes = zeroize::Zeroizing::new(std::fs::read(entry.path()).unwrap());
                    files.insert(
                        entry.path().strip_prefix(root).unwrap().to_owned(),
                        Sha256::digest(&*bytes).into(),
                    );
                }
            }
        }
        visit(isolated_keys, isolated_keys, &mut files);
        assert!(!files.is_empty(), "protected snapshot must not be vacuous");
        Self { tables, files }
    }

    pub(crate) async fn assert_unchanged(&self, db: &Database, isolated_keys: &Path) {
        let after = Self::capture(db, isolated_keys).await;
        assert!(
            self.files == after.files,
            "protected store contents or file inventory changed on rejection"
        );
        assert_eq!(
            self.tables.keys().collect::<Vec<_>>(),
            after.tables.keys().collect::<Vec<_>>(),
            "local table inventory changed"
        );
        for (table, digest) in &self.tables {
            assert!(
                after.tables[table] == *digest,
                "local table {table} changed on rejection"
            );
        }
    }
}
