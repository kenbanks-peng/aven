use std::fs;
use std::path::PathBuf;

use serde_yaml::Value;

use crate::config::{self as app_config, AppConfig};

pub(super) struct ConfigBootstrap {
    pub(super) path: Option<PathBuf>,
    pub(super) config: Option<AppConfig>,
    pub(super) loose_db_path: Option<PathBuf>,
    pub(super) failure: Option<String>,
}

pub(super) fn inspect_config() -> ConfigBootstrap {
    let path = match app_config::config_file_path() {
        Ok(path) => path,
        Err(_) => {
            return ConfigBootstrap {
                path: None,
                config: None,
                loose_db_path: None,
                failure: Some(
                    "configuration path is unavailable; set AVEN_CONFIG_DIR or HOME".to_string(),
                ),
            };
        }
    };
    if !path.exists() {
        return ConfigBootstrap {
            config: AppConfig::load_from_path(&path).ok(),
            path: Some(path),
            loose_db_path: None,
            failure: None,
        };
    }
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => {
            return ConfigBootstrap {
                path: Some(path),
                config: None,
                loose_db_path: None,
                failure: Some(
                    "configuration file is unreadable; check file ownership and permissions"
                        .to_string(),
                ),
            };
        }
    };
    let loose = serde_yaml::from_str::<Value>(&text).ok();
    let loose_db_path = loose.as_ref().and_then(configured_database_path);
    match AppConfig::load_from_path(&path) {
        Ok(config) => ConfigBootstrap {
            path: Some(path),
            config: Some(config),
            loose_db_path,
            failure: None,
        },
        Err(_) => {
            let location = serde_yaml::from_str::<AppConfig>(&text)
                .err()
                .and_then(|error| error.location())
                .map(|location| {
                    format!(" at line {}, column {}", location.line(), location.column())
                })
                .unwrap_or_default();
            ConfigBootstrap {
                path: Some(path),
                config: None,
                loose_db_path,
                failure: Some(format!(
                    "configuration is invalid{location}; fix the YAML or invalid value, then rerun `aven doctor`"
                )),
            }
        }
    }
}

fn configured_database_path(value: &Value) -> Option<PathBuf> {
    value
        .get("local")?
        .get("db_path")?
        .as_str()
        .map(PathBuf::from)
}

pub(super) fn resolve_doctor_db_path(
    flag: Option<PathBuf>,
    bootstrap: &ConfigBootstrap,
) -> (Option<PathBuf>, &'static str, Option<String>) {
    if let Some(path) = flag {
        return (Some(path), "--db", None);
    }
    if let Some(path) = app_config::debug_db_path_from_env() {
        return (Some(path), "AVEN_DEV_DB", None);
    }
    if let Some(path) = std::env::var_os("AVEN_DB") {
        return (Some(PathBuf::from(path)), "AVEN_DB", None);
    }
    let configured = bootstrap
        .config
        .as_ref()
        .and_then(|config| config.local.db_path.clone())
        .or_else(|| bootstrap.loose_db_path.clone());
    if let Some(path) = configured {
        return match app_config::expand_tilde(&path) {
            Ok(path) => (Some(path), "config local.db_path", None),
            Err(_) => (
                None,
                "config local.db_path",
                Some("database path could not expand `~`; set HOME or pass --db".to_string()),
            ),
        };
    }
    if cfg!(debug_assertions) {
        return (
            None,
            "unresolved",
            Some("debug builds require --db, AVEN_DEV_DB, AVEN_DB, or local.db_path".to_string()),
        );
    }
    match app_config::default_db_path() {
        Ok(path) => (Some(path), "default", None),
        Err(_) => (
            None,
            "default",
            Some("platform database path is unavailable; set XDG_STATE_HOME or HOME".to_string()),
        ),
    }
}
