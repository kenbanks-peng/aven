use std::io::{self, Write};
use std::path::Path;

use anyhow::Result;

use super::clipboard::{
    ClipboardCommand, ClipboardCommandOutput, LinuxClipboardBackend,
    advertised_clipboard_image_format, clipboard_list_command, clipboard_read_command,
    clipboard_write_command, copy_linux_clipboard_with, linux_clipboard_backend_order,
    read_linux_clipboard_image_with,
};
use super::terminal::{
    KeyboardEnhancementMode, KeyboardEnhancementState, TerminalTransition,
    run_while_terminal_suspended,
};
use super::viewer::{
    OperatingSystem, ViewerCommand, default_browser_command, default_image_viewer_command,
};

#[derive(Default)]
struct FakeEditorTransition {
    suspended: usize,
    restored: usize,
}

impl TerminalTransition for FakeEditorTransition {
    fn suspend(&mut self) -> Result<()> {
        self.suspended += 1;
        Ok(())
    }

    fn restore(&mut self) -> Result<()> {
        self.restored += 1;
        Ok(())
    }
}

#[test]
fn external_editor_operation_restores_terminal_after_success_and_failure() {
    for result in [Ok("edited"), Err(anyhow::anyhow!("editor failed"))] {
        let mut transition = FakeEditorTransition::default();
        let failed = result.is_err();
        let actual = run_while_terminal_suspended(&mut transition, || result);

        assert_eq!(actual.is_err(), failed);
        assert_eq!(transition.suspended, 1);
        assert_eq!(transition.restored, 1);
    }
}

#[test]
fn selects_platform_default_image_viewer_commands() {
    let path = Path::new("/tmp/attachment image.png");
    assert_eq!(
        default_image_viewer_command(OperatingSystem::Macos, path),
        ViewerCommand {
            program: "open",
            args: vec![path.as_os_str().to_owned()],
        }
    );
    assert_eq!(
        default_image_viewer_command(OperatingSystem::Linux, path),
        ViewerCommand {
            program: "xdg-open",
            args: vec![path.as_os_str().to_owned()],
        }
    );
    assert_eq!(
        default_image_viewer_command(OperatingSystem::Windows, path),
        ViewerCommand {
            program: "rundll32.exe",
            args: vec![
                "url.dll,FileProtocolHandler".into(),
                path.as_os_str().to_owned(),
            ],
        }
    );
}

#[test]
fn selects_platform_default_browser_commands() {
    let url = "https://aven.raine.dev/recurring-tasks/";
    assert_eq!(
        default_browser_command(OperatingSystem::Macos, url),
        ViewerCommand {
            program: "open",
            args: vec![url.into()],
        }
    );
    assert_eq!(
        default_browser_command(OperatingSystem::Linux, url),
        ViewerCommand {
            program: "xdg-open",
            args: vec![url.into()],
        }
    );
    assert_eq!(
        default_browser_command(OperatingSystem::Windows, url),
        ViewerCommand {
            program: "rundll32.exe",
            args: vec!["url.dll,FileProtocolHandler".into(), url.into()],
        }
    );
}

#[test]
fn selects_linux_clipboard_backend_from_session_environment() {
    use std::ffi::OsStr;

    assert_eq!(
        linux_clipboard_backend_order(Some(OsStr::new("wayland-0")), None),
        [LinuxClipboardBackend::Wayland, LinuxClipboardBackend::X11]
    );
    assert_eq!(
        linux_clipboard_backend_order(None, Some(OsStr::new("WAYLAND"))),
        [LinuxClipboardBackend::Wayland, LinuxClipboardBackend::X11]
    );
    assert_eq!(
        linux_clipboard_backend_order(None, Some(OsStr::new("x11"))),
        [LinuxClipboardBackend::X11, LinuxClipboardBackend::Wayland]
    );
}

#[test]
fn selects_linux_clipboard_read_and_write_commands() {
    let format =
        advertised_clipboard_image_format(b"image/webp\nimage/gif\nimage/jpeg\nimage/png\n")
            .unwrap();

    assert_eq!(format.mime, "image/png");
    assert_eq!(format.extension, "png");
    assert_eq!(
        clipboard_list_command(LinuxClipboardBackend::Wayland),
        ClipboardCommand {
            program: "wl-paste",
            args: vec!["-l".into()],
        }
    );
    assert_eq!(
        clipboard_read_command(LinuxClipboardBackend::X11, format),
        ClipboardCommand {
            program: "xclip",
            args: vec![
                "-selection".into(),
                "clipboard".into(),
                "-t".into(),
                "image/png".into(),
                "-o".into(),
            ],
        }
    );
    assert_eq!(
        clipboard_write_command(LinuxClipboardBackend::Wayland),
        ClipboardCommand {
            program: "wl-copy",
            args: Vec::new(),
        }
    );
    assert_eq!(
        clipboard_write_command(LinuxClipboardBackend::X11),
        ClipboardCommand {
            program: "xclip",
            args: vec!["-selection".into(), "clipboard".into(), "-in".into()],
        }
    );
}

#[test]
fn falls_back_from_missing_wayland_tool_to_x11() {
    use std::collections::VecDeque;

    let mut responses = VecDeque::from([
        Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
        Ok(clipboard_test_output(true, b"image/webp\n", b"")),
        Ok(clipboard_test_output(true, b"webp bytes", b"")),
    ]);
    let mut commands = Vec::new();

    let image = read_linux_clipboard_image_with(
        [LinuxClipboardBackend::Wayland, LinuxClipboardBackend::X11],
        |command| {
            commands.push(command.clone());
            responses.pop_front().unwrap()
        },
    )
    .unwrap()
    .unwrap();

    assert_eq!(image.filename, "pasted-image.webp");
    assert_eq!(image.bytes, b"webp bytes");
    assert_eq!(commands[0].program, "wl-paste");
    assert_eq!(commands[1].program, "xclip");
    assert_eq!(commands[2].args[3], "image/webp");
}

#[test]
fn checks_x11_when_wayland_clipboard_has_no_image() {
    use std::collections::VecDeque;

    let mut responses = VecDeque::from([
        Ok(clipboard_test_output(true, b"text/plain\n", b"")),
        Ok(clipboard_test_output(true, b"image/gif\n", b"")),
        Ok(clipboard_test_output(true, b"gif bytes", b"")),
    ]);

    let image = read_linux_clipboard_image_with(
        [LinuxClipboardBackend::Wayland, LinuxClipboardBackend::X11],
        |_| responses.pop_front().unwrap(),
    )
    .unwrap()
    .unwrap();

    assert_eq!(image.filename, "pasted-image.gif");
    assert_eq!(image.bytes, b"gif bytes");
}

#[test]
fn reports_non_image_content_when_an_available_backend_has_no_image() {
    use std::collections::VecDeque;

    let mut responses = VecDeque::from([
        Ok(clipboard_test_output(true, b"text/plain\n", b"")),
        Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
    ]);

    let image = read_linux_clipboard_image_with(
        [LinuxClipboardBackend::Wayland, LinuxClipboardBackend::X11],
        |_| responses.pop_front().unwrap(),
    )
    .unwrap();

    assert!(image.is_none());

    let mut responses = VecDeque::from([
        Ok(clipboard_test_output(false, b"", b"Nothing is copied")),
        Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
    ]);
    let empty = read_linux_clipboard_image_with(
        [LinuxClipboardBackend::Wayland, LinuxClipboardBackend::X11],
        |_| responses.pop_front().unwrap(),
    )
    .unwrap();

    assert!(empty.is_none());
}

#[test]
fn distinguishes_missing_tools_from_clipboard_command_failures() {
    use std::collections::VecDeque;

    let missing = read_linux_clipboard_image_with(
        [LinuxClipboardBackend::X11, LinuxClipboardBackend::Wayland],
        |_| Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
    )
    .unwrap_err();
    assert_eq!(
        missing.to_string(),
        "Linux clipboard image paste requires wl-paste or xclip"
    );

    let mut responses = VecDeque::from([
        Ok(clipboard_test_output(false, b"", b"cannot open display")),
        Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
    ]);
    let failed = read_linux_clipboard_image_with(
        [LinuxClipboardBackend::X11, LinuxClipboardBackend::Wayland],
        |_| responses.pop_front().unwrap(),
    )
    .unwrap_err();
    assert_eq!(
        failed.to_string(),
        "xclip exited with exit status: 1: cannot open display"
    );
}

#[test]
fn writes_linux_clipboard_payload_with_backend_fallback() {
    use std::collections::VecDeque;

    let mut responses = VecDeque::from([
        Ok(clipboard_test_output(
            false,
            b"",
            b"cannot connect to wayland display",
        )),
        Ok(clipboard_test_output(true, b"", b"")),
    ]);
    let mut commands = Vec::new();
    let mut payloads = Vec::new();

    let value = "task title\nsecond line";
    copy_linux_clipboard_with(
        [LinuxClipboardBackend::Wayland, LinuxClipboardBackend::X11],
        value,
        |command, payload| {
            commands.push(command.clone());
            payloads.push(payload.to_vec());
            responses.pop_front().unwrap()
        },
    )
    .unwrap();

    assert_eq!(commands[0].program, "wl-copy");
    assert_eq!(commands[1].program, "xclip");
    assert_eq!(payloads, vec![value.as_bytes().to_vec(); 2]);
}

#[test]
fn reports_missing_linux_clipboard_tools() {
    let error = copy_linux_clipboard_with(
        [LinuxClipboardBackend::X11, LinuxClipboardBackend::Wayland],
        "task title",
        |_, _| Err(io::Error::new(io::ErrorKind::NotFound, "missing")),
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "Linux clipboard copy requires wl-copy or xclip"
    );
}

#[test]
fn reports_linux_clipboard_command_failures() {
    use std::collections::VecDeque;

    let mut responses = VecDeque::from([
        Ok(clipboard_test_output(
            false,
            b"",
            b"cannot connect to wayland display",
        )),
        Ok(clipboard_test_output(false, b"", b"cannot open display")),
    ]);

    let error = copy_linux_clipboard_with(
        [LinuxClipboardBackend::Wayland, LinuxClipboardBackend::X11],
        "task title",
        |_, _| responses.pop_front().unwrap(),
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "wl-copy exited with exit status: 1: cannot connect to wayland display; \
             xclip exited with exit status: 1: cannot open display"
    );
}

fn clipboard_test_output(success: bool, stdout: &[u8], stderr: &[u8]) -> ClipboardCommandOutput {
    ClipboardCommandOutput {
        success,
        status: if success {
            "exit status: 0".to_string()
        } else {
            "exit status: 1".to_string()
        },
        stdout: stdout.to_vec(),
        stderr: stderr.to_vec(),
    }
}

#[test]
fn kitty_keyboard_enhancement_pushes_and_pops_state() {
    let mut state = KeyboardEnhancementState::default();
    let mut output = Vec::new();

    state
        .enable(KeyboardEnhancementMode::Kitty, &mut output)
        .unwrap();
    assert_eq!(state.mode, Some(KeyboardEnhancementMode::Kitty));
    state.disable(&mut output).unwrap();

    assert_eq!(output, b"\x1b[>1u\x1b[<1u");
    assert_eq!(state.mode, None);
}

#[test]
fn failed_restore_keeps_keyboard_state_available_for_retry() {
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut state = KeyboardEnhancementState::default();
    let mut output = Vec::new();
    state
        .enable(KeyboardEnhancementMode::Kitty, &mut output)
        .unwrap();

    assert!(state.disable(&mut FailingWriter).is_err());
    assert_eq!(state.mode, Some(KeyboardEnhancementMode::Kitty));
    state.disable(&mut output).unwrap();

    assert_eq!(output, b"\x1b[>1u\x1b[<1u");
    assert_eq!(state.mode, None);
}

#[test]
fn modify_other_keys_enhancement_restores_terminal_mode() {
    let mut state = KeyboardEnhancementState::default();
    let mut output = Vec::new();

    state
        .enable(KeyboardEnhancementMode::ModifyOtherKeys, &mut output)
        .unwrap();
    assert_eq!(state.mode, Some(KeyboardEnhancementMode::ModifyOtherKeys));
    state.disable(&mut output).unwrap();
    state.disable(&mut output).unwrap();

    assert_eq!(output, b"\x1b[>4;2m\x1b[>4m");
    assert_eq!(state.mode, None);
}
