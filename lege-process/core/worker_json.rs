/// JSON-newline event renderer for --gui-worker mode.
///
/// Reads ProgressUpdate events from the receiver and writes one JSON object
/// per line to stdout. All human-readable output must go to stderr before
/// calling this function. stdout is owned exclusively by the protocol.
use std::io::Write;

use crate::progress::ProgressUpdate;

pub fn run_json_worker_with_terminal_hook<F>(
    rx: &flume::Receiver<ProgressUpdate>,
    task_id: u64,
    mut before_terminal: F,
) -> Option<ProgressUpdate>
where
    F: FnMut(&ProgressUpdate),
{
    loop {
        match rx.recv() {
            Ok(update) => {
                let our_task = match &update {
                    ProgressUpdate::Status { task_id: id, .. } => *id == task_id,
                    ProgressUpdate::Completed { task_id: id, .. } => *id == task_id,
                    ProgressUpdate::Error { task_id: id, .. } => *id == task_id,
                };
                if !our_task {
                    continue;
                }
                let is_terminal = matches!(
                    &update,
                    ProgressUpdate::Completed { .. } | ProgressUpdate::Error { .. }
                );
                if is_terminal {
                    before_terminal(&update);
                }
                match serde_json::to_string(&update) {
                    Ok(json) => {
                        println!("{}", json);
                        let _ = std::io::stdout().flush();
                    }
                    Err(e) => {
                        eprintln!("[worker-json] serialization error: {}", e);
                    }
                }
                if is_terminal {
                    return Some(update);
                }
            }
            Err(_) => break,
        }
    }
    None
}
