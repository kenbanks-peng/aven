use std::env;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::AppConfig;

const APP_DIR: &str = "aven";

pub fn expand_tilde(path: &Path) -> Result<PathBuf> {
    expand_tilde_from(path, dirs::home_dir().as_deref())
}

pub(crate) fn expand_tilde_from(path: &Path, home: Option<&Path>) -> Result<PathBuf> {
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(component)) if component == "~") {
        return Ok(path.to_path_buf());
    }
    let home = home.context("could not find home directory")?;
    Ok(home.join(components.as_path()))
}

pub fn config_dir_path() -> Result<PathBuf> {
    if let Ok(path) = env::var("AVEN_CONFIG_DIR") {
        return Ok(PathBuf::from(path));
    }
    let home = dirs::home_dir().context("could not find home directory")?;
    Ok(home.join(".config").join(APP_DIR))
}

pub fn config_file_path() -> Result<PathBuf> {
    let mut path = config_dir_path()?;
    path.push("config.yaml");
    Ok(path)
}

pub fn default_db_path() -> Result<PathBuf> {
    let mut dir = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
        .context("could not find state directory")?;
    dir.push("aven");
    dir.push("db.sqlite");
    Ok(dir)
}

pub fn resolve_db_path(flag: Option<PathBuf>, config: &AppConfig) -> Result<PathBuf> {
    resolve_db_path_from(
        flag,
        env::var_os("AVEN_DB").map(PathBuf::from),
        debug_db_path_from_env(),
        config,
        cfg!(debug_assertions),
    )
}

pub(super) fn resolve_db_path_from(
    flag: Option<PathBuf>,
    env_db: Option<PathBuf>,
    dev_db: Option<PathBuf>,
    config: &AppConfig,
    debug_build: bool,
) -> Result<PathBuf> {
    if let Some(path) = flag {
        return Ok(path);
    }
    if debug_build && let Some(path) = dev_db {
        return Ok(path);
    }
    if let Some(path) = env_db {
        return Ok(path);
    }
    if let Some(path) = &config.local.db_path {
        return expand_tilde(path);
    }
    if debug_build {
        bail!("error debug-database-required hint=\"set AVEN_DEV_DB, set AVEN_DB, or pass --db\"");
    }
    default_db_path()
}

pub fn debug_db_path_from_env() -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        env::var_os("AVEN_DEV_DB").map(PathBuf::from)
    } else {
        None
    }
}

#[allow(dead_code)]
pub fn resolve_blob_dir(db_path: &Path, config: &AppConfig) -> Result<PathBuf> {
    let base = db_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match &config.local.blob_dir {
        Some(path) if path.is_absolute() => Ok(path.clone()),
        Some(path) => Ok(base.join(path)),
        None => {
            let mut blob_dir = db_path.as_os_str().to_os_string();
            blob_dir.push(".blobs");
            Ok(PathBuf::from(blob_dir))
        }
    }
}

pub fn resolve_sync_server(flag: Option<&str>, config: &AppConfig) -> Result<String> {
    let environment = env::var("AVEN_SYNC_SERVER").ok();
    resolve_sync_server_from(flag, environment.as_deref(), config)
}

pub(crate) fn resolve_sync_server_from(
    flag: Option<&str>,
    environment: Option<&str>,
    config: &AppConfig,
) -> Result<String> {
    if let Some(server) = flag {
        return Ok(server.to_string());
    }
    if let Some(server) = environment {
        return Ok(server.to_string());
    }
    if let Some(server) = &config.sync.server_url {
        return Ok(server.clone());
    }
    bail!("error sync-server-required hint=\"pass --server or configure sync.server_url\"")
}
