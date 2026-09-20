#[cfg(not(test))]
use std::fs;
#[cfg(any(not(test), unix))]
use std::io;
use std::process::Command as ProcessCommand;
#[cfg(not(test))]
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[cfg(not(test))]
use super::terminal::{SystemTerminalTransition, run_while_terminal_suspended};

pub(crate) fn is_editor_prefix_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('x')
}

#[cfg(not(test))]
pub(crate) fn edit_text_externally(
    value: String,
    filename: &str,
    mouse_capture: bool,
) -> Result<String> {
    let path = temp_editor_path(filename)?;
    fs::write(&path, value)?;
    let result = run_external_editor(&path, mouse_capture)
        .and_then(|()| fs::read_to_string(&path).map_err(Into::into));
    let _ = fs::remove_file(&path);
    if let Some(parent) = path.parent() {
        let _ = fs::remove_dir(parent);
    }
    result
}

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_EDITOR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_external_editor() {
    FAIL_NEXT_EDITOR.set(true);
}

#[cfg(test)]
pub(crate) fn edit_text_externally(
    value: String,
    _filename: &str,
    _mouse_capture: bool,
) -> Result<String> {
    if FAIL_NEXT_EDITOR.replace(false) {
        anyhow::bail!("injected external editor failure");
    }
    Ok(format!("{value} from editor"))
}

#[cfg(not(test))]
fn temp_editor_path(filename: &str) -> io::Result<std::path::PathBuf> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("aven-tui-editor-{pid}-{millis}"));
    fs::create_dir(&dir)?;
    Ok(dir.join(filename))
}

#[cfg(not(test))]
fn run_external_editor(path: &std::path::Path, mouse_capture: bool) -> Result<()> {
    let mut transition = SystemTerminalTransition::new(mouse_capture);
    let status = run_while_terminal_suspended(&mut transition, || {
        external_editor_command(path).status().map_err(Into::into)
    })?;
    if !status.success() {
        anyhow::bail!("editor exited with {status}");
    }
    Ok(())
}

#[cfg(not(test))]
fn external_editor_command(path: &std::path::Path) -> ProcessCommand {
    let mut command = ProcessCommand::new("sh");
    command
        .arg("-c")
        .arg("exec ${VISUAL:-${EDITOR:-vi}} \"$1\"")
        .arg("sh")
        .arg(path);
    configure_terminal_child_signals(&mut command);
    command
}

#[cfg(unix)]
pub(crate) fn configure_terminal_child_signals(command: &mut ProcessCommand) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            let mut signals = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
            if libc::sigemptyset(signals.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            let result =
                libc::pthread_sigmask(libc::SIG_SETMASK, signals.as_ptr(), std::ptr::null_mut());
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(result))
            }
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn configure_terminal_child_signals(_command: &mut ProcessCommand) {}
