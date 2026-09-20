use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use anyhow::Result;

pub(super) struct ScannedFile {
    pub(super) path: PathBuf,
    pub(super) name: String,
    pub(super) len: u64,
    pub(super) modified: SystemTime,
}

#[derive(Default)]
pub(super) struct DirectoryCursor {
    entries: Option<fs::ReadDir>,
}

pub(super) static DIRECTORY_CURSORS: LazyLock<Mutex<HashMap<PathBuf, DirectoryCursor>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) async fn scan_directory_all(dir: PathBuf) -> Result<Vec<ScannedFile>> {
    crate::attachments::blocking::run(move || {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let metadata = entry.metadata()?;
            files.push(ScannedFile {
                path: entry.path(),
                name: entry.file_name().to_string_lossy().into_owned(),
                len: metadata.len(),
                modified: metadata.modified()?,
            });
        }
        Ok(files)
    })
    .await
}

pub(super) async fn scan_directory_page(dir: PathBuf, limit: usize) -> Result<Vec<ScannedFile>> {
    scan_directory_page_with(dir, limit, |entries| entries.next().transpose()).await
}

pub(super) async fn scan_directory_page_with<F>(
    dir: PathBuf,
    limit: usize,
    mut next_entry: F,
) -> Result<Vec<ScannedFile>>
where
    F: FnMut(&mut fs::ReadDir) -> io::Result<Option<fs::DirEntry>> + Send + 'static,
{
    if limit == 0 {
        return Ok(Vec::new());
    }
    crate::attachments::blocking::run(move || {
        let key = dir.clone();
        let mut cursors = DIRECTORY_CURSORS
            .lock()
            .map_err(|_| anyhow::anyhow!("attachment directory cursor poisoned"))?;
        let mut cursor = match cursors.remove(&key) {
            Some(cursor) => cursor,
            None => {
                let entries = match fs::read_dir(&dir) {
                    Ok(entries) => entries,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Ok(Vec::new());
                    }
                    Err(error) => return Err(error.into()),
                };
                DirectoryCursor {
                    entries: Some(entries),
                }
            }
        };
        let mut files = Vec::new();
        let mut exhausted = false;
        for _ in 0..limit {
            let Some(entry) = next_entry(
                cursor
                    .entries
                    .as_mut()
                    .expect("attachment directory cursor should be open"),
            )?
            else {
                exhausted = true;
                break;
            };
            if !entry.file_type()?.is_file() {
                continue;
            }
            let metadata = entry.metadata()?;
            files.push(ScannedFile {
                path: entry.path(),
                name: entry.file_name().to_string_lossy().into_owned(),
                len: metadata.len(),
                modified: metadata.modified()?,
            });
        }
        if !exhausted {
            cursors.insert(key, cursor);
        }
        Ok(files)
    })
    .await
}

pub(super) async fn remove_files(files: Vec<PathBuf>) -> Result<()> {
    crate::attachments::blocking::run(move || {
        for path in files {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileMove {
    Moved,
    SourceMissing,
    TargetExists,
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("attachment move target has no parent"))?;
    fs::File::open(parent)?.sync_all()
}

fn finish_file_move(source: &Path, target: &Path) -> Result<FileMove> {
    if let Err(error) = sync_parent(target) {
        let _ = fs::remove_file(target);
        return Err(error.into());
    }
    if let Err(error) = fs::remove_file(source) {
        let _ = fs::remove_file(target);
        let _ = sync_parent(target);
        return Err(error.into());
    }
    Ok(FileMove::Moved)
}

fn copy_file_without_replacing(source: &Path, target: &Path) -> Result<FileMove> {
    let mut source_file = match fs::File::open(source) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(FileMove::SourceMissing);
        }
        Err(error) => return Err(error.into()),
    };
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("attachment move target has no parent"))?;
    let mut staging = tempfile::Builder::new()
        .prefix(".aven-move-")
        .tempfile_in(parent)?;
    io::copy(&mut source_file, &mut staging)?;
    staging.as_file().sync_all()?;
    match staging.persist_noclobber(target) {
        Ok(_) => finish_file_move(source, target),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            Ok(FileMove::TargetExists)
        }
        Err(error) => Err(error.error.into()),
    }
}

pub(super) fn move_file_without_replacing(source: &Path, target: &Path) -> Result<FileMove> {
    move_file_without_replacing_with(source, target, |source, target| {
        fs::hard_link(source, target)
    })
}

pub(super) fn move_file_without_replacing_with<F>(
    source: &Path,
    target: &Path,
    hard_link: F,
) -> Result<FileMove>
where
    F: FnOnce(&Path, &Path) -> io::Result<()>,
{
    match hard_link(source, target) {
        Ok(()) => finish_file_move(source, target),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(FileMove::TargetExists),
        Err(error) if error.kind() == io::ErrorKind::NotFound && !source.exists() => {
            Ok(FileMove::SourceMissing)
        }
        Err(_) => copy_file_without_replacing(source, target),
    }
}

pub(super) async fn restore_trashed_files(files: Vec<(PathBuf, PathBuf)>) {
    let _ = crate::attachments::blocking::run(move || {
        for (source, target) in files {
            if !target.exists() {
                continue;
            }
            if source.exists() {
                fs::remove_file(target)?;
            } else {
                let _ = move_file_without_replacing(&target, &source)?;
            }
        }
        Ok(())
    })
    .await;
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    use std::io;

    #[tokio::test]
    async fn unavailable_directory_releases_cursor_owned_by_lexical_path() {
        let temp = tempfile::tempdir().unwrap();
        let scan_root = temp.path().join("scan");
        let lexical_root = temp.path().join("anchor").join("..").join("scan");
        fs::create_dir_all(temp.path().join("anchor")).unwrap();
        fs::create_dir_all(&scan_root).unwrap();
        fs::write(scan_root.join("first"), b"first").unwrap();
        fs::write(scan_root.join("second"), b"second").unwrap();
        let canonical_root = fs::canonicalize(&lexical_root).unwrap();

        scan_directory_page(lexical_root.clone(), 1).await.unwrap();
        assert!(
            DIRECTORY_CURSORS
                .lock()
                .unwrap()
                .contains_key(&lexical_root)
        );

        fs::remove_dir_all(&scan_root).unwrap();
        let _ = scan_directory_page(lexical_root.clone(), 1).await;

        let cursors = DIRECTORY_CURSORS.lock().unwrap();
        assert!(!cursors.contains_key(&lexical_root));
        assert!(!cursors.contains_key(&canonical_root));
    }

    #[tokio::test]
    async fn directory_iteration_error_releases_cursor_state() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("scan");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("first"), b"first").unwrap();
        fs::write(dir.join("second"), b"second").unwrap();

        scan_directory_page(dir.clone(), 1).await.unwrap();
        assert!(DIRECTORY_CURSORS.lock().unwrap().contains_key(&dir));

        let result = scan_directory_page_with(dir.clone(), 1, |_| {
            Err(io::Error::other("injected directory iteration failure"))
        })
        .await;
        assert!(result.as_ref().is_err_and(|error| {
            error
                .to_string()
                .contains("injected directory iteration failure")
        }));
        assert!(!DIRECTORY_CURSORS.lock().unwrap().contains_key(&dir));
    }

    #[test]
    fn file_move_copies_atomically_when_hard_links_are_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::write(&source, b"attachment").unwrap();

        let moved = move_file_without_replacing_with(&source, &target, |_, _| {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "injected hard-link failure",
            ))
        })
        .unwrap();

        assert_eq!(moved, FileMove::Moved);
        assert!(!source.exists());
        assert_eq!(fs::read(target).unwrap(), b"attachment");
    }

    #[test]
    fn file_move_copy_fallback_does_not_replace_target() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::write(&source, b"source").unwrap();
        fs::write(&target, b"target").unwrap();

        let moved = move_file_without_replacing_with(&source, &target, |_, _| {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "injected hard-link failure",
            ))
        })
        .unwrap();

        assert_eq!(moved, FileMove::TargetExists);
        assert_eq!(fs::read(source).unwrap(), b"source");
        assert_eq!(fs::read(target).unwrap(), b"target");
    }
}
