use std::io::Write;
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result};

pub(crate) fn create_secret_gist(
    markdown: &str,
    filename: &str,
    description: &str,
) -> Result<String> {
    let mut child = ProcessCommand::new("gh")
        .args([
            "gist",
            "create",
            "--filename",
            filename,
            "--desc",
            description,
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("could not run gh; install and authenticate GitHub CLI")?;
    child
        .stdin
        .take()
        .context("gh standard input is unavailable")?
        .write_all(markdown.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(if message.is_empty() {
            format!("gh gist create exited with {}", output.status)
        } else {
            message
        });
    }
    let url = String::from_utf8(output.stdout)
        .context("gh gist create returned a non-UTF-8 URL")?
        .trim()
        .to_string();
    if url.is_empty() {
        anyhow::bail!("gh gist create returned no URL");
    }
    Ok(url)
}
