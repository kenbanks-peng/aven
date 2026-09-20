use std::io::{self, IsTerminal};

use crossterm::style::{Color, Stylize};

use super::SyncRunSummary;

pub(super) fn print_sync_result(summary: &SyncRunSummary) {
    let styled = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    print!("{}", format_sync_result(summary, styled));
}

fn format_sync_result(summary: &SyncRunSummary, styled: bool) -> String {
    let no_activity = summary.pushed == 0
        && summary.pulled == 0
        && summary.blob_uploaded == 0
        && summary.blob_downloaded == 0;
    let mut output = String::new();
    if no_activity && summary.complete {
        push_headline(&mut output, "Everything is up to date", true, styled);
        push_row(
            &mut output,
            "Cursor",
            &format_number(summary.cursor),
            false,
            styled,
        );
        return output;
    }

    push_headline(
        &mut output,
        if summary.complete {
            "Sync complete"
        } else {
            "Sync incomplete"
        },
        summary.complete,
        styled,
    );
    push_row(
        &mut output,
        "Changes",
        &format!("{} sent, {} received", summary.pushed, summary.pulled),
        false,
        styled,
    );
    if summary.blob_uploaded > 0 || summary.blob_downloaded > 0 {
        push_row(
            &mut output,
            "Attachments",
            &format!(
                "{} uploaded ({}), {} downloaded ({})",
                summary.blob_uploaded,
                format_bytes(summary.blob_uploaded_bytes),
                summary.blob_downloaded,
                format_bytes(summary.blob_downloaded_bytes),
            ),
            false,
            styled,
        );
    }
    if summary.blob_upload_remaining > 0
        || summary.blob_upload_remaining_bytes > 0
        || summary.blob_download_remaining > 0
        || summary.blob_download_remaining_bytes > 0
    {
        push_row(
            &mut output,
            "Remaining",
            &format!(
                "{}, {}",
                format_count_with_bytes(
                    summary.blob_upload_remaining,
                    summary.blob_upload_remaining_bytes,
                    "upload",
                    "uploads",
                ),
                format_count_with_bytes(
                    summary.blob_download_remaining,
                    summary.blob_download_remaining_bytes,
                    "download",
                    "downloads",
                ),
            ),
            true,
            styled,
        );
    }
    push_row(
        &mut output,
        "Cursor",
        &format_number(summary.cursor),
        false,
        styled,
    );
    if !summary.complete {
        output.push('\n');
        if styled {
            output.push_str(&format!(
                "{}\n",
                "Run `aven sync` again to continue.".with(Color::Rgb {
                    r: 150,
                    g: 150,
                    b: 150,
                })
            ));
        } else {
            output.push_str("Run `aven sync` again to continue.\n");
        }
    }
    output
}

fn push_headline(output: &mut String, text: &str, complete: bool, styled: bool) {
    let (marker, color) = if complete {
        ("✓", Color::Green)
    } else {
        ("!", Color::Yellow)
    };
    if styled {
        output.push_str(&format!(
            "{} {}\n",
            marker.with(color).bold(),
            text.with(color).bold()
        ));
    } else {
        output.push_str(&format!("{} {text}\n", if complete { "ok" } else { "!!" }));
    }
}

fn push_row(output: &mut String, label: &str, value: &str, warning: bool, styled: bool) {
    if styled {
        let value_color = if warning {
            Color::Yellow
        } else {
            Color::Rgb {
                r: 150,
                g: 150,
                b: 150,
            }
        };
        output.push_str(&format!(
            "  {}  {}\n",
            format!("{label:<11}").with(Color::Cyan),
            value.with(value_color)
        ));
    } else {
        output.push_str(&format!("   {label:<11}  {value}\n"));
    }
}

fn format_number(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(character);
    }
    if value < 0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

fn format_count_with_bytes(count: usize, bytes: u64, singular: &str, plural: &str) -> String {
    let noun = if count == 1 { singular } else { plural };
    format!("{count} {noun} ({})", format_bytes(bytes))
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} {}", if bytes == 1 { "byte" } else { "bytes" });
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_result_is_concise_when_up_to_date() {
        let summary = SyncRunSummary {
            cursor: 6_659,
            complete: true,
            ..SyncRunSummary::default()
        };

        assert_eq!(
            format_sync_result(&summary, false),
            "ok Everything is up to date\n   Cursor       6,659\n"
        );
    }

    #[test]
    fn terminal_result_uses_color() {
        let summary = SyncRunSummary {
            complete: true,
            ..SyncRunSummary::default()
        };

        let output = format_sync_result(&summary, true);
        assert!(output.contains("\u{1b}["));
        assert!(output.contains("Everything is up to date"));
    }

    #[test]
    fn human_result_reports_transfers_and_remaining_work() {
        let summary = SyncRunSummary {
            pushed: 3,
            pulled: 2,
            blob_uploaded: 1,
            blob_uploaded_bytes: 1_536,
            blob_downloaded: 2,
            blob_downloaded_bytes: 2_097_152,
            blob_upload_remaining: 1,
            blob_upload_remaining_bytes: 512,
            blob_download_remaining: 3,
            blob_download_remaining_bytes: 3_145_728,
            cursor: 42,
            complete: false,
            ..SyncRunSummary::default()
        };

        let output = format_sync_result(&summary, false);
        assert!(output.contains("!! Sync incomplete"));
        assert!(output.contains("3 sent, 2 received"));
        assert!(output.contains("1 uploaded (1.5 KiB), 2 downloaded (2.0 MiB)"));
        assert!(output.contains("1 upload (512 bytes), 3 downloads (3.0 MiB)"));
        assert!(output.contains("Cursor       42"));
        assert!(output.contains("Run `aven sync` again to continue."));
    }
}
