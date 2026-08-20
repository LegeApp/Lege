//! Progress fan-out.
//!
//! `lege::progress` publishes every task's updates onto one process-wide
//! `flume` channel. That channel is MPMC, not broadcast: two consumers would
//! *split* the stream rather than each see all of it, so this module holds a
//! single long-lived receiver and demultiplexes by task id itself.
//!
//! Updates for a task nobody is currently polling are parked rather than
//! dropped, so a second job queued behind the first does not lose its early
//! events while the host is still draining the first.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use lege::progress::{ProgressUpdate, get_progress_manager};

/// The one subscription to the global progress channel.
fn receiver() -> &'static flume::Receiver<ProgressUpdate> {
    static RECEIVER: OnceLock<flume::Receiver<ProgressUpdate>> = OnceLock::new();
    RECEIVER.get_or_init(|| get_progress_manager().subscribe())
}

/// Updates pulled off the channel that belong to a task other than the one
/// currently being polled.
fn parked() -> &'static Mutex<HashMap<u64, VecDeque<ProgressUpdate>>> {
    static PARKED: OnceLock<Mutex<HashMap<u64, VecDeque<ProgressUpdate>>>> = OnceLock::new();
    PARKED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Bound on parked updates per task, so a task the host never polls cannot
/// grow without limit. Progress updates are advisory and the newest matter
/// most, so the oldest is discarded on overflow.
const MAX_PARKED_PER_TASK: usize = 256;

fn take_parked(task_id: u64) -> Option<ProgressUpdate> {
    let mut parked = parked().lock().ok()?;
    let queue = parked.get_mut(&task_id)?;
    let update = queue.pop_front();
    if queue.is_empty() {
        parked.remove(&task_id);
    }
    update
}

fn park(update: ProgressUpdate) {
    let task_id = match &update {
        ProgressUpdate::Status { task_id, .. }
        | ProgressUpdate::Completed { task_id, .. }
        | ProgressUpdate::Error { task_id, .. } => *task_id,
    };

    let Ok(mut parked) = parked().lock() else {
        return;
    };
    let queue = parked.entry(task_id).or_default();
    if queue.len() >= MAX_PARKED_PER_TASK {
        queue.pop_front();
    }
    queue.push_back(update);
}

/// Forget anything parked for a task. Called when a task reaches a terminal
/// state so a long-lived process does not accumulate dead entries.
pub(crate) fn forget(task_id: u64) {
    if let Ok(mut parked) = parked().lock() {
        parked.remove(&task_id);
    }
}

/// Establish the subscription before a job is enqueued.
///
/// Not required for correctness: `ProgressManager` holds a receiver for the
/// lifetime of the process, so the unbounded channel buffers from creation and
/// a late `subscribe()` still sees everything already queued. This exists so
/// the one-time setup happens at a predictable point rather than inside
/// whichever `poll` call happens to run first.
pub(crate) fn prime() {
    let _ = receiver();
}

/// Wait up to `timeout` for the next update belonging to `task_id`.
///
/// Returns `None` on timeout — that is a normal result, not an error; the host
/// polls in a loop and a quiet interval simply means no new progress.
pub(crate) fn poll(task_id: u64, timeout: Duration) -> Option<ProgressUpdate> {
    if let Some(update) = take_parked(task_id) {
        return Some(update);
    }

    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }

        match receiver().recv_timeout(remaining) {
            Ok(update) => {
                let matches = match &update {
                    ProgressUpdate::Status { task_id: id, .. }
                    | ProgressUpdate::Completed { task_id: id, .. }
                    | ProgressUpdate::Error { task_id: id, .. } => *id == task_id,
                };
                if matches {
                    return Some(update);
                }
                park(update);
            }
            // Timed out, or the sender side is gone. Either way there is
            // nothing further to hand back for this call.
            Err(_) => return None,
        }
    }
}

/// True when this update is the last one a task will produce.
pub(crate) fn is_terminal(update: &ProgressUpdate) -> bool {
    matches!(
        update,
        ProgressUpdate::Completed { .. } | ProgressUpdate::Error { .. }
    )
}
