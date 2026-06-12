use anyhow::anyhow;
use chrono::Local;
use flume;
use std::io::{self, Write};
use std::time::{Duration, Instant};

fn timestamp() -> String {
    Local::now().format("[%H:%M:%S]").to_string()
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let millis = duration.subsec_millis();
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    if hours > 0 {
        format!("{}h {:02}m {:02}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{:02}m {:02}s", minutes, seconds)
    } else if millis > 0 {
        format!("{:02}.{:03}s", seconds, millis)
    } else {
        format!("{:02}s", seconds)
    }
}

/// CLI 3-line renderer that redraws in place without scrolling.
/// Listens to ProgressUpdate and displays a smooth, compact 3-line status block.
/// Only prints significant changes to avoid spamming the console.
pub async fn run_cli_three_line(
    rx: flume::Receiver<crate::progress::ProgressUpdate>,
) -> anyhow::Result<()> {
    // Hide cursor; ensure we unhide on drop
    struct CursorGuard;
    impl Drop for CursorGuard {
        fn drop(&mut self) {
            print!("\x1b[?25h"); // show cursor
            let _ = io::stdout().flush();
        }
    }
    let _cg = CursorGuard {};
    print!("\x1b[?25l"); // hide cursor
    println!();
    println!();
    println!(); // reserve 3 lines
    let mut last_lines = ("".to_string(), "".to_string(), "".to_string());
    let start_time = Instant::now();
    let mut completed_duration: Option<Duration> = None;
    let mut progress_error: Option<String> = None;

    // Track the last progress state to only update on significant changes
    let mut last_rendered = 0usize;
    let mut last_detected = 0usize;
    let mut last_encoded = 0usize;
    let mut update_counter = 0u32;

    loop {
        match rx.recv_async().await {
            Ok(crate::progress::ProgressUpdate::Status { status, .. }) => {
                let (l0, l1, l2) = status.to_display_lines();

                // Determine if this is a significant change
                let mut is_significant = false;

                match &status {
                    crate::progress::ProcessingStatus::LayoutProgress {
                        rendered,
                        detected,
                        encoded,
                        ..
                    } => {
                        // Only update every 10 pages or when a stage meaningfully advances
                        if *rendered != last_rendered
                            && (*rendered % 10 == 0 || *rendered > last_rendered)
                        {
                            is_significant = true;
                        }
                        if *detected != last_detected
                            && (*detected % 10 == 0 || *detected > last_detected)
                        {
                            is_significant = true;
                        }
                        if *encoded != last_encoded
                            && (*encoded % 10 == 0 || *encoded > last_encoded)
                        {
                            is_significant = true;
                        }
                        last_rendered = *rendered;
                        last_detected = *detected;
                        last_encoded = *encoded;
                    }
                    crate::progress::ProcessingStatus::NoLayoutProgress { encoded, .. } => {
                        // Update every 10 pages
                        if *encoded % 10 == 0 || *encoded > last_encoded {
                            is_significant = true;
                        }
                        last_encoded = *encoded;
                    }
                    crate::progress::ProcessingStatus::MarginProgress {
                        pass1_rendered,
                        pass2_processed,
                        ..
                    } => {
                        // Update every 10 pages
                        if *pass1_rendered % 10 == 0 || *pass2_processed % 10 == 0 {
                            is_significant = true;
                        }
                        if *pass2_processed > last_encoded {
                            is_significant = true;
                        }
                        last_rendered = *pass1_rendered;
                        last_encoded = *pass2_processed;
                    }
                    crate::progress::ProcessingStatus::ReflowProgress { current, .. } => {
                        if *current % 10 == 0 || *current > last_encoded {
                            is_significant = true;
                        }
                        last_encoded = *current;
                    }
                    crate::progress::ProcessingStatus::EpubProgress {
                        rendered,
                        detected,
                        ocr,
                        ..
                    } => {
                        if *rendered > last_rendered
                            || *detected > last_detected
                            || *ocr > last_encoded
                        {
                            is_significant = true;
                        }
                        last_rendered = *rendered;
                        last_detected = *detected;
                        last_encoded = *ocr;
                    }
                    // Always show these status types
                    crate::progress::ProcessingStatus::Initializing
                    | crate::progress::ProcessingStatus::AssemblingOutput
                    | crate::progress::ProcessingStatus::Complete { .. }
                    | crate::progress::ProcessingStatus::Error { .. }
                    | crate::progress::ProcessingStatus::FootnotesDetected { .. }
                    | crate::progress::ProcessingStatus::OcrLayerDetected { .. }
                    | crate::progress::ProcessingStatus::MarginPass1Analyzing
                    | crate::progress::ProcessingStatus::MarginAnalysisSummary { .. }
                    | crate::progress::ProcessingStatus::PipelineMessage { .. }
                    | crate::progress::ProcessingStatus::PdfAppend { .. }
                    | crate::progress::ProcessingStatus::PdfAppendMargin { .. } => {
                        is_significant = true;
                    }
                }

                // Force update every 50 status messages to ensure we're not completely stuck
                update_counter += 1;
                if update_counter >= 50 {
                    is_significant = true;
                    update_counter = 0;
                }

                // Only redraw if something significant changed
                if is_significant
                    && (l0.as_str(), l1.as_str(), l2.as_str())
                        != (
                            last_lines.0.as_str(),
                            last_lines.1.as_str(),
                            last_lines.2.as_str(),
                        )
                {
                    // Move cursor up 3 lines, clear the block, rewrite
                    print!("\x1b[3F\x1b[0J");

                    // Colorize based on status type
                    let (status_color, detail_color) = match &status {
                        crate::progress::ProcessingStatus::LayoutProgress { .. } => {
                            ("\x1b[2;34m", "\x1b[96m") // Dim blue status, bright cyan detail
                        }
                        crate::progress::ProcessingStatus::NoLayoutProgress { .. } => {
                            ("\x1b[2;34m", "\x1b[96m")
                        }
                        crate::progress::ProcessingStatus::MarginProgress { .. } => {
                            ("\x1b[2;34m", "\x1b[96m")
                        }
                        crate::progress::ProcessingStatus::ReflowProgress { .. } => {
                            ("\x1b[2;34m", "\x1b[96m")
                        }
                        crate::progress::ProcessingStatus::EpubProgress { .. } => {
                            ("\x1b[2;34m", "\x1b[96m")
                        }
                        crate::progress::ProcessingStatus::PdfAppend { .. }
                        | crate::progress::ProcessingStatus::PdfAppendMargin { .. } => {
                            ("\x1b[2;34m", "\x1b[92m") // Page complete gets bright green
                        }
                        crate::progress::ProcessingStatus::Initializing => {
                            ("\x1b[2;34m", "\x1b[94m") // Soft blue for starting
                        }
                        crate::progress::ProcessingStatus::AssemblingOutput => {
                            ("\x1b[2;34m", "\x1b[95m") // Soft magenta for assembling
                        }
                        _ => ("\x1b[2;34m", "\x1b[2;36m"), // Default dim colors
                    };

                    println!("{} {}Status\x1b[0m {}", timestamp(), status_color, l0);
                    println!(
                        "{} {}Detail\x1b[0m {}{}{}",
                        timestamp(),
                        detail_color,
                        detail_color,
                        l1,
                        "\x1b[0m"
                    );
                    if !l2.is_empty() {
                        println!(
                            "{} \x1b[2;36mDetail\x1b[0m \x1b[2;36m{}\x1b[0m",
                            timestamp(),
                            l2
                        );
                    } else {
                        println!();
                    }
                    io::stdout().flush()?;
                    last_lines = (l0, l1, l2);
                }
            }
            Ok(crate::progress::ProgressUpdate::Completed { message, .. }) => {
                print!("\x1b[3F\x1b[0J");
                println!("{} \x1b[92mStatus\x1b[0m [Complete]", timestamp());
                println!("{} \x1b[92mDetail\x1b[0m {}", timestamp(), message);
                println!("{} \x1b[92mDetail\x1b[0m Ready for next task.", timestamp());
                io::stdout().flush()?;
                completed_duration = Some(start_time.elapsed());
                break;
            }
            Ok(crate::progress::ProgressUpdate::Error { error, .. }) => {
                print!("\x1b[3F\x1b[0J");
                println!("{} \x1b[2;34mStatus\x1b[0m [Error]", timestamp());
                println!("{} \x1b[2;36mDetail\x1b[0m An error occurred.", timestamp());
                println!("{} \x1b[2;36mDetail\x1b[0m {}", timestamp(), error);
                io::stdout().flush()?;
                progress_error = Some(error);
                break;
            }
            Err(flume::RecvError::Disconnected) => break,
        }
    }

    if let Some(duration) = completed_duration {
        println!();
        println!(
            "{} \x1b[92mProcessing completed in {}.\x1b[0m",
            timestamp(),
            format_duration(duration)
        );
        io::stdout().flush()?;
    }

    if let Some(err) = progress_error {
        return Err(anyhow!(err));
    }

    Ok(())
}
