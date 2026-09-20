use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CustomTuiCommandConfig {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub description: String,
    pub program: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_keys: Option<Vec<String>>,
    pub target: CustomTuiCommandTarget,
    #[serde(default)]
    pub execution: CustomTuiCommandExecution,
    #[serde(default)]
    pub on_success: CustomTuiCommandSuccess,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomTuiCommandConfigInput {
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    description: String,
    program: PathBuf,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    keys: Vec<String>,
    #[serde(default)]
    detail_keys: Option<Vec<String>>,
    target: Option<CustomTuiCommandTarget>,
    requires: Option<CustomTuiCommandRequirement>,
    #[serde(default)]
    execution: CustomTuiCommandExecution,
    #[serde(default)]
    on_success: CustomTuiCommandSuccess,
}

impl<'de> Deserialize<'de> for CustomTuiCommandConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let input = CustomTuiCommandConfigInput::deserialize(deserializer)?;
        if input.target.is_some() && input.requires.is_some() {
            return Err(serde::de::Error::custom(
                "custom command cannot supply both target and requires",
            ));
        }
        let target = input.target.unwrap_or_else(|| {
            input
                .requires
                .map(CustomTuiCommandRequirement::target)
                .unwrap_or_default()
        });
        Ok(Self {
            name: input.name,
            aliases: input.aliases,
            description: input.description,
            program: input.program,
            cwd: input.cwd,
            env: input.env,
            timeout_seconds: input.timeout_seconds,
            args: input.args,
            keys: input.keys,
            detail_keys: input.detail_keys,
            target,
            execution: input.execution,
            on_success: input.on_success,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CustomTuiCommandTarget {
    None,
    #[default]
    Focused,
    Marked,
    MarkedOrFocused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum CustomTuiCommandRequirement {
    None,
    SelectedTask,
}

impl CustomTuiCommandRequirement {
    fn target(self) -> CustomTuiCommandTarget {
        match self {
            Self::None => CustomTuiCommandTarget::None,
            Self::SelectedTask => CustomTuiCommandTarget::Focused,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CustomTuiCommandExecution {
    Background,
    #[default]
    Wait,
    Terminal,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CustomTuiCommandSuccess {
    #[default]
    Stay,
    Refresh,
    Quit,
    RefreshAndQuit,
}

pub(super) fn validate(commands: &[CustomTuiCommandConfig]) -> Result<()> {
    let mut command_names = crate::tui::built_in_command_names()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    for command in commands {
        if command.description.trim().is_empty() {
            bail!(
                "custom command {} description must not be blank",
                command.name
            );
        }
        if command.program.as_os_str().is_empty()
            || command.program.to_string_lossy().trim().is_empty()
        {
            bail!("custom command {} program must not be blank", command.name);
        }
        if command
            .cwd
            .as_ref()
            .is_some_and(|cwd| cwd.as_os_str().is_empty())
        {
            bail!("custom command {} cwd must not be blank", command.name);
        }
        for (name, value) in &command.env {
            if name.is_empty() || name.contains('=') || name.contains('\0') {
                bail!(
                    "custom command {} has invalid environment variable name {name:?}",
                    command.name
                );
            }
            if value.contains('\0') {
                bail!(
                    "custom command {} environment variable {name:?} contains a NUL byte",
                    command.name
                );
            }
        }
        if let Some(timeout_seconds) = command.timeout_seconds
            && !(1..=super::MAX_CUSTOM_COMMAND_TIMEOUT_SECONDS).contains(&timeout_seconds)
        {
            bail!(
                "custom command {} timeout_seconds must be between 1 and {}",
                command.name,
                super::MAX_CUSTOM_COMMAND_TIMEOUT_SECONDS
            );
        }
        if command.execution == CustomTuiCommandExecution::Background
            && command.on_success != CustomTuiCommandSuccess::Stay
        {
            bail!(
                "custom command {} background execution requires on_success: stay",
                command.name
            );
        }
        for name in
            std::iter::once(command.name.as_str()).chain(command.aliases.iter().map(String::as_str))
        {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
            {
                bail!("invalid custom command name {name:?}");
            }
            if !command_names.insert(name.to_string()) {
                bail!("duplicate or built-in custom command name {name}");
            }
        }
    }
    crate::tui::validate_custom_command_keys(commands)?;
    Ok(())
}
