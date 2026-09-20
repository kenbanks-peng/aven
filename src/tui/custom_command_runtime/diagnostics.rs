use unicode_width::UnicodeWidthChar;

const DIAGNOSTIC_LINE_LIMIT: usize = 2;
const DIAGNOSTIC_CHAR_LIMIT: usize = 512;
const DIAGNOSTIC_WIDTH_LIMIT: usize = 512;

pub(super) fn diagnostic_excerpt(output: &super::CommandOutput) -> Option<String> {
    let selected = if sanitized_lines(&output.stderr.bytes).is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    let mut lines = sanitized_lines(&selected.bytes);
    if selected.truncated
        && !selected.bytes.starts_with(b"\n")
        && !selected.bytes.starts_with(b"\r")
        && lines.len() > 1
    {
        lines.remove(0);
    }
    if lines.is_empty() {
        return None;
    }
    let joined = lines
        .into_iter()
        .take(DIAGNOSTIC_LINE_LIMIT)
        .collect::<Vec<_>>()
        .join(" | ");
    let prefix = if selected.truncated { "… " } else { "" };
    Some(bound_diagnostic(
        &format!("{prefix}{joined}"),
        DIAGNOSTIC_CHAR_LIMIT,
        DIAGNOSTIC_WIDTH_LIMIT,
    ))
}

fn sanitized_lines(bytes: &[u8]) -> Vec<String> {
    strip_terminal_sequences(&String::from_utf8_lossy(bytes))
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect()
}

fn bound_diagnostic(value: &str, max_chars: usize, max_width: usize) -> String {
    let char_count = value.chars().count();
    let display_width = value
        .chars()
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum::<usize>();
    if char_count <= max_chars && display_width <= max_width {
        return value.to_string();
    }
    let content_chars = max_chars.saturating_sub(1);
    let content_width = max_width.saturating_sub(1);
    let mut bounded = String::new();
    let mut width = 0_usize;
    for (chars, character) in value.chars().enumerate() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if chars == content_chars || width.saturating_add(character_width) > content_width {
            break;
        }
        bounded.push(character);
        width += character_width;
    }
    if max_chars > 0 && max_width > 0 {
        bounded.push('…');
    }
    bounded
}

fn strip_terminal_sequences(value: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Text,
        Escape,
        Csi,
        Osc,
        StringEscape,
        EscapeIntermediate,
    }

    let mut output = String::new();
    let mut state = State::Text;
    for character in value.chars() {
        state = match state {
            State::Text => match character {
                '\u{1b}' => State::Escape,
                '\n' => {
                    output.push('\n');
                    State::Text
                }
                '\t' => {
                    output.push(' ');
                    State::Text
                }
                character if character.is_control() => State::Text,
                character => {
                    output.push(character);
                    State::Text
                }
            },
            State::Escape => match character {
                '[' => State::Csi,
                ']' | 'P' | 'X' | '^' | '_' => State::Osc,
                '\u{20}'..='\u{2f}' => State::EscapeIntermediate,
                _ => State::Text,
            },
            State::Csi => {
                if ('\u{40}'..='\u{7e}').contains(&character) {
                    State::Text
                } else {
                    State::Csi
                }
            }
            State::Osc => match character {
                '\u{7}' => State::Text,
                '\u{1b}' => State::StringEscape,
                _ => State::Osc,
            },
            State::StringEscape => {
                if character == '\\' {
                    State::Text
                } else {
                    State::Osc
                }
            }
            State::EscapeIntermediate => {
                if ('\u{30}'..='\u{7e}').contains(&character) {
                    State::Text
                } else {
                    State::EscapeIntermediate
                }
            }
        };
    }
    output
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::{CapturedOutput, CommandOutput};
    use super::*;
    use unicode_width::UnicodeWidthChar;

    #[test]
    fn diagnostics_render_invalid_utf8_lossily() {
        let output = CommandOutput {
            stdout: CapturedOutput {
                bytes: Vec::new(),
                truncated: false,
            },
            stderr: CapturedOutput {
                bytes: b"invalid byte: \xff reason".to_vec(),
                truncated: false,
            },
        };

        assert_eq!(
            diagnostic_excerpt(&output).as_deref(),
            Some("invalid byte: � reason")
        );
    }

    #[test]
    fn diagnostics_remove_ansi_and_unsafe_controls() {
        let output = CommandOutput {
            stdout: CapturedOutput {
                bytes: Vec::new(),
                truncated: false,
            },
            stderr: CapturedOutput {
                bytes: b"\x1b[31mred\x1b[0m\x07\r\n\x1b]0;secret title\x07safe\n".to_vec(),
                truncated: false,
            },
        };

        assert_eq!(diagnostic_excerpt(&output).as_deref(), Some("red | safe"));
    }

    #[test]
    fn diagnostics_bound_characters_and_display_width() {
        let output = CommandOutput {
            stdout: CapturedOutput {
                bytes: Vec::new(),
                truncated: false,
            },
            stderr: CapturedOutput {
                bytes: "界".repeat(600).into_bytes(),
                truncated: false,
            },
        };

        let excerpt = diagnostic_excerpt(&output).unwrap();
        assert!(excerpt.chars().count() <= DIAGNOSTIC_CHAR_LIMIT);
        assert!(
            excerpt
                .chars()
                .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
                .sum::<usize>()
                <= DIAGNOSTIC_WIDTH_LIMIT
        );
        assert!(excerpt.ends_with('…'));
    }
}
