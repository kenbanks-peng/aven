#[cfg(target_os = "macos")]
use std::process::Command;

use anyhow::Result;

#[cfg(target_os = "macos")]
pub(crate) fn read_text() -> Result<Option<String>> {
    let output = Command::new("pbpaste").output()?;
    if !output.status.success() {
        anyhow::bail!("pbpaste exited with {}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    Ok((!text.trim().is_empty()).then_some(text))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn read_text() -> Result<Option<String>> {
    Ok(None)
}
