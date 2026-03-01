use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::io::{BufRead, BufReader};

use serde::Deserialize;
use clap::Parser;
use colored::*;

/// A small tool to count and group cargo check errors by file.
#[derive(Parser)]
struct Args {
    /// Output file to write results (optional)
    #[arg(short, long)]
    output: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "reason")]
enum CargoMessage {
    CompilerMessage { message: Diagnostic },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct Diagnostic {
    level: String,
    spans: Vec<Span>,
    message: String,
}

#[derive(Debug, Deserialize)]
struct Span {
    file_name: String,
    line_start: usize,
    is_primary: bool,
}

fn main() {
    let args = Args::parse();

    // Map from file → Vec<"error text (line N)">
    let mut file_error_map: HashMap<String, Vec<String>> = HashMap::new();

    let mut child = Command::new("cargo")
        .args(&["check", "--message-format=json"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("Failed to run cargo check");

    let stdout = BufReader::new(child.stdout.take().unwrap());
    for line in stdout.lines().flatten() {
        if let Ok(msg) = serde_json::from_str::<CargoMessage>(&line) {
            if let CargoMessage::CompilerMessage { message } = msg {
                if message.level == "error" {
                    for span in &message.spans {
                        if span.is_primary {
                            file_error_map
                                .entry(span.file_name.clone())
                                .or_insert_with(Vec::new)
                                .push(format!(
                                    "{} (line {})",
                                    message.message,
                                    span.line_start
                                ));
                        }
                    }
                }
            }
        }
    }

    // Sort files by number of errors descending
    let mut file_errors: Vec<_> = file_error_map.into_iter().collect();
    file_errors.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    // Print & build output
    let mut output = String::new();
    output.push_str("=== Errors grouped by file ===\n\n");

    for (file, messages) in &file_errors {
        println!("{}: {}", file.blue(), messages.len().to_string().red());
        for msg in messages {
            println!("    - {}", msg.yellow());
            output.push_str(&format!("{}:{} {}\n", file, msg, "\n"));
        }
    }

    // Write to file if requested
    if let Some(path) = args.output {
        std::fs::write(&path, output).expect("Failed to write to file");
        println!("Results written to {path}");
    }
}