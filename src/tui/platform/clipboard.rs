#[cfg(target_os = "macos")]
use std::fs;
#[cfg(any(target_os = "linux", test))]
use std::io;
#[cfg(all(not(test), any(target_os = "macos", target_os = "linux")))]
use std::io::Write;
#[cfg(all(target_os = "linux", not(test)))]
use std::io::{Read, Seek};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Command as ProcessCommand;
#[cfg(any(target_os = "linux", test))]
use std::process::Output;

use anyhow::Result;

#[derive(Debug)]
pub(crate) struct ClipboardImage {
    pub(crate) filename: String,
    pub(crate) bytes: Vec<u8>,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LinuxClipboardBackend {
    Wayland,
    X11,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ClipboardCommand {
    pub(super) program: &'static str,
    pub(super) args: Vec<std::ffi::OsString>,
}

#[cfg(any(target_os = "linux", test))]
pub(super) struct ClipboardCommandOutput {
    pub(super) success: bool,
    pub(super) status: String,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

#[cfg(any(target_os = "linux", test))]
impl From<Output> for ClipboardCommandOutput {
    fn from(output: Output) -> Self {
        Self {
            success: output.status.success(),
            status: output.status.to_string(),
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy)]
pub(super) struct ClipboardImageFormat {
    pub(super) mime: &'static str,
    pub(super) extension: &'static str,
}

#[cfg(any(target_os = "linux", test))]
const CLIPBOARD_IMAGE_FORMATS: [ClipboardImageFormat; 5] = [
    ClipboardImageFormat {
        mime: "image/png",
        extension: "png",
    },
    ClipboardImageFormat {
        mime: "image/jpeg",
        extension: "jpg",
    },
    ClipboardImageFormat {
        mime: "image/jpg",
        extension: "jpg",
    },
    ClipboardImageFormat {
        mime: "image/gif",
        extension: "gif",
    },
    ClipboardImageFormat {
        mime: "image/webp",
        extension: "webp",
    },
];

#[cfg(any(target_os = "linux", test))]
pub(super) fn linux_clipboard_backend_order(
    wayland_display: Option<&std::ffi::OsStr>,
    xdg_session_type: Option<&std::ffi::OsStr>,
) -> [LinuxClipboardBackend; 2] {
    let wayland_session = wayland_display.is_some_and(|value| !value.is_empty())
        || xdg_session_type.is_some_and(|value| {
            value
                .to_str()
                .is_some_and(|value| value.eq_ignore_ascii_case("wayland"))
        });
    if wayland_session {
        [LinuxClipboardBackend::Wayland, LinuxClipboardBackend::X11]
    } else {
        [LinuxClipboardBackend::X11, LinuxClipboardBackend::Wayland]
    }
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn clipboard_list_command(backend: LinuxClipboardBackend) -> ClipboardCommand {
    match backend {
        LinuxClipboardBackend::Wayland => ClipboardCommand {
            program: "wl-paste",
            args: vec!["-l".into()],
        },
        LinuxClipboardBackend::X11 => ClipboardCommand {
            program: "xclip",
            args: vec![
                "-selection".into(),
                "clipboard".into(),
                "-t".into(),
                "TARGETS".into(),
                "-o".into(),
            ],
        },
    }
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn clipboard_read_command(
    backend: LinuxClipboardBackend,
    format: ClipboardImageFormat,
) -> ClipboardCommand {
    match backend {
        LinuxClipboardBackend::Wayland => ClipboardCommand {
            program: "wl-paste",
            args: vec!["--type".into(), format.mime.into()],
        },
        LinuxClipboardBackend::X11 => ClipboardCommand {
            program: "xclip",
            args: vec![
                "-selection".into(),
                "clipboard".into(),
                "-t".into(),
                format.mime.into(),
                "-o".into(),
            ],
        },
    }
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn clipboard_write_command(backend: LinuxClipboardBackend) -> ClipboardCommand {
    match backend {
        LinuxClipboardBackend::Wayland => ClipboardCommand {
            program: "wl-copy",
            args: Vec::new(),
        },
        LinuxClipboardBackend::X11 => ClipboardCommand {
            program: "xclip",
            args: vec!["-selection".into(), "clipboard".into(), "-in".into()],
        },
    }
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn advertised_clipboard_image_format(output: &[u8]) -> Option<ClipboardImageFormat> {
    let advertised = String::from_utf8_lossy(output);
    CLIPBOARD_IMAGE_FORMATS.iter().copied().find(|format| {
        advertised
            .lines()
            .any(|line| line.trim().eq_ignore_ascii_case(format.mime))
    })
}

#[cfg(any(target_os = "linux", test))]
enum ClipboardBackendResult {
    Image(ClipboardImage),
    NoImage,
    Unavailable,
    Failed(String),
}

#[cfg(any(target_os = "linux", test))]
fn clipboard_command_reports_no_content(output: &ClipboardCommandOutput) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.contains("Nothing is copied")
        || stderr.contains("There is no owner for the CLIPBOARD selection")
}

#[cfg(any(target_os = "linux", test))]
fn clipboard_command_failure(
    command: &ClipboardCommand,
    output: &ClipboardCommandOutput,
) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("{} exited with {}", command.program, output.status)
    } else {
        format!(
            "{} exited with {}: {stderr}",
            command.program, output.status
        )
    }
}

#[cfg(any(target_os = "linux", test))]
fn read_linux_clipboard_backend(
    backend: LinuxClipboardBackend,
    run: &mut impl FnMut(&ClipboardCommand) -> io::Result<ClipboardCommandOutput>,
) -> ClipboardBackendResult {
    let list_command = clipboard_list_command(backend);
    let list_output = match run(&list_command) {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ClipboardBackendResult::Unavailable;
        }
        Err(error) => {
            return ClipboardBackendResult::Failed(format!(
                "could not run {}: {error}",
                list_command.program
            ));
        }
    };
    if !list_output.success {
        if clipboard_command_reports_no_content(&list_output) {
            return ClipboardBackendResult::NoImage;
        }
        return ClipboardBackendResult::Failed(clipboard_command_failure(
            &list_command,
            &list_output,
        ));
    }
    let Some(format) = advertised_clipboard_image_format(&list_output.stdout) else {
        return ClipboardBackendResult::NoImage;
    };

    let read_command = clipboard_read_command(backend, format);
    let image_output = match run(&read_command) {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ClipboardBackendResult::Unavailable;
        }
        Err(error) => {
            return ClipboardBackendResult::Failed(format!(
                "could not run {}: {error}",
                read_command.program
            ));
        }
    };
    if !image_output.success {
        if clipboard_command_reports_no_content(&image_output) {
            return ClipboardBackendResult::NoImage;
        }
        return ClipboardBackendResult::Failed(clipboard_command_failure(
            &read_command,
            &image_output,
        ));
    }
    if image_output.stdout.is_empty() {
        return ClipboardBackendResult::NoImage;
    }
    ClipboardBackendResult::Image(ClipboardImage {
        filename: format!("pasted-image.{}", format.extension),
        bytes: image_output.stdout,
    })
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn read_linux_clipboard_image_with(
    backends: [LinuxClipboardBackend; 2],
    mut run: impl FnMut(&ClipboardCommand) -> io::Result<ClipboardCommandOutput>,
) -> Result<Option<ClipboardImage>> {
    let mut saw_no_image = false;
    let mut failures = Vec::new();
    for backend in backends {
        match read_linux_clipboard_backend(backend, &mut run) {
            ClipboardBackendResult::Image(image) => return Ok(Some(image)),
            ClipboardBackendResult::NoImage => saw_no_image = true,
            ClipboardBackendResult::Unavailable => {}
            ClipboardBackendResult::Failed(error) => failures.push(error),
        }
    }
    if saw_no_image {
        return Ok(None);
    }
    if !failures.is_empty() {
        anyhow::bail!(failures.join("; "));
    }
    anyhow::bail!("Linux clipboard image paste requires wl-paste or xclip")
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn copy_linux_clipboard_with(
    backends: [LinuxClipboardBackend; 2],
    value: &str,
    mut run: impl FnMut(&ClipboardCommand, &[u8]) -> io::Result<ClipboardCommandOutput>,
) -> Result<()> {
    let mut failures = Vec::new();
    for backend in backends {
        let command = clipboard_write_command(backend);
        match run(&command, value.as_bytes()) {
            Ok(output) if output.success => return Ok(()),
            Ok(output) => failures.push(clipboard_command_failure(&command, &output)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("could not run {}: {error}", command.program)),
        }
    }
    if failures.is_empty() {
        anyhow::bail!("Linux clipboard copy requires wl-copy or xclip");
    }
    anyhow::bail!(failures.join("; "))
}

#[cfg(all(target_os = "linux", not(test)))]
fn run_clipboard_write_command(
    command: &ClipboardCommand,
    value: &[u8],
) -> io::Result<ClipboardCommandOutput> {
    let mut stderr = tempfile::tempfile()?;
    let mut child = ProcessCommand::new(command.program)
        .args(&command.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(stderr.try_clone()?)
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("clipboard command standard input is unavailable"))?;
    let write_result = stdin.write_all(value);
    drop(stdin);
    let status = child.wait()?;
    write_result?;
    stderr.rewind()?;
    let mut stderr_output = Vec::new();
    stderr.read_to_end(&mut stderr_output)?;
    Ok(ClipboardCommandOutput {
        success: status.success(),
        status: status.to_string(),
        stdout: Vec::new(),
        stderr: stderr_output,
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn read_clipboard_image() -> Result<Option<ClipboardImage>> {
    let temp = tempfile::Builder::new()
        .prefix("aven-clipboard-image-")
        .suffix(".png")
        .tempfile()?;
    let path = temp.path().to_path_buf();
    let script = r#"
set outPath to POSIX file (system attribute "AVEN_CLIPBOARD_IMAGE_PATH")
try
    set imageData to the clipboard as «class PNGf»
on error
    return "no-image"
end try
set fileRef to open for access outPath with write permission
try
    set eof of fileRef to 0
    write imageData to fileRef
    close access fileRef
on error errText
    try
        close access fileRef
    end try
    error errText
end try
return "ok"
"#;
    let output = ProcessCommand::new("osascript")
        .arg("-e")
        .arg(script)
        .env("AVEN_CLIPBOARD_IMAGE_PATH", &path)
        .output()?;
    if !output.status.success() {
        anyhow::bail!("osascript exited with {}", output.status);
    }
    if String::from_utf8_lossy(&output.stdout).trim() == "no-image" {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    Ok(Some(ClipboardImage {
        filename: "pasted-image.png".to_string(),
        bytes,
    }))
}

#[cfg(target_os = "linux")]
pub(crate) fn read_clipboard_image() -> Result<Option<ClipboardImage>> {
    let backends = linux_clipboard_backend_order(
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("XDG_SESSION_TYPE").as_deref(),
    );
    read_linux_clipboard_image_with(backends, |command| {
        ProcessCommand::new(command.program)
            .args(&command.args)
            .output()
            .map(Into::into)
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn read_clipboard_image() -> Result<Option<ClipboardImage>> {
    Ok(None)
}

#[cfg(target_os = "macos")]
pub(crate) fn read_clipboard_text() -> Result<Option<String>> {
    let output = ProcessCommand::new("pbpaste").output()?;
    if !output.status.success() {
        anyhow::bail!("pbpaste exited with {}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    Ok((!text.trim().is_empty()).then_some(text))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn read_clipboard_text() -> Result<Option<String>> {
    Ok(None)
}

#[cfg(all(not(test), target_os = "macos"))]
pub(crate) fn copy_to_clipboard(value: &str) -> Result<()> {
    let mut child = ProcessCommand::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(value.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("pbcopy exited with {status}");
    }
    Ok(())
}

#[cfg(all(not(test), target_os = "linux"))]
pub(crate) fn copy_to_clipboard(value: &str) -> Result<()> {
    let backends = linux_clipboard_backend_order(
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("XDG_SESSION_TYPE").as_deref(),
    );
    copy_linux_clipboard_with(backends, value, run_clipboard_write_command)
}

#[cfg(all(not(test), not(any(target_os = "macos", target_os = "linux"))))]
pub(crate) fn copy_to_clipboard(_value: &str) -> Result<()> {
    anyhow::bail!("clipboard copy is unsupported on this platform")
}

#[cfg(test)]
thread_local! {
    static TEST_CLIPBOARD: std::cell::RefCell<Option<String>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
pub(crate) fn copy_to_clipboard(value: &str) -> Result<()> {
    TEST_CLIPBOARD.with(|clipboard| clipboard.replace(Some(value.to_string())));
    Ok(())
}

#[cfg(test)]
pub(crate) fn clipboard_text_for_test() -> Option<String> {
    TEST_CLIPBOARD.with(|clipboard| clipboard.borrow().clone())
}
