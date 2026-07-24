use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::text::TextSubstrate;

use super::{MemoryLease, PageIndex, PageStructure, TileKey};

pub trait WakeSink: Send + Sync + std::fmt::Debug {
    fn wake(&self);
}

/// A thin UI-facing projection of compile output. The conductor retains the
/// heavyweight semantic page and compiled display-list IR. The UI receives
/// only the document substrate it directly consumes, plus a shared memory
/// lease that remains alive for as long as any UI cache retains the text.
#[derive(Debug, Clone)]
pub struct PageArtifactUpdate {
    pub page: PageIndex,
    pub generation: u64,
    pub text: Arc<TextSubstrate>,
    pub structure: PageStructure,
    pub operation_count: usize,
    pub lowering_degraded: bool,
    pub memory_lease: MemoryLease,
}

#[derive(Debug, Clone)]
pub enum SessionUpdate {
    PageCompiled(PageArtifactUpdate),
    TileReady {
        key: TileKey,
        generation: u64,
    },
    PageError {
        page: PageIndex,
        message: String,
        quarantined: bool,
    },
    QueueDepths {
        compile_pending: usize,
        raster_pending: usize,
        in_flight: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateKey {
    Page(PageIndex),
    Tile(TileKey),
    Error(PageIndex),
    QueueDepths,
}

impl SessionUpdate {
    fn key(&self) -> UpdateKey {
        match self {
            SessionUpdate::PageCompiled(update) => UpdateKey::Page(update.page),
            SessionUpdate::TileReady { key, .. } => UpdateKey::Tile(*key),
            SessionUpdate::PageError { page, .. } => UpdateKey::Error(*page),
            SessionUpdate::QueueDepths { .. } => UpdateKey::QueueDepths,
        }
    }

    fn low_priority(&self) -> bool {
        match self {
            SessionUpdate::QueueDepths { .. } => true,
            SessionUpdate::TileReady { key, .. } => key.tier == super::TileTier::Draft,
            SessionUpdate::PageCompiled(_) | SessionUpdate::PageError { .. } => false,
        }
    }
}

#[derive(Debug)]
pub struct UpdateQueue {
    capacity: usize,
    pending: Mutex<VecDeque<SessionUpdate>>,
    wake_pending: AtomicBool,
    wake: Arc<dyn WakeSink>,
}

impl UpdateQueue {
    pub fn new(capacity: usize, wake: Arc<dyn WakeSink>) -> Arc<Self> {
        Arc::new(Self {
            capacity: capacity.max(8),
            pending: Mutex::new(VecDeque::with_capacity(capacity.max(8))),
            wake_pending: AtomicBool::new(false),
            wake,
        })
    }

    pub fn push(&self, update: SessionUpdate) {
        {
            let mut pending = self.pending.lock().expect("update queue poisoned");
            let key = update.key();
            if let Some(existing) = pending.iter_mut().find(|candidate| candidate.key() == key) {
                *existing = update;
            } else {
                if pending.len() >= self.capacity {
                    if let Some(index) = pending.iter().position(SessionUpdate::low_priority) {
                        pending.remove(index);
                    } else {
                        pending.pop_front();
                    }
                }
                pending.push_back(update);
            }
        }
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            self.wake.wake();
        }
    }

    pub fn drain(&self) -> Vec<SessionUpdate> {
        let updates = {
            let mut pending = self.pending.lock().expect("update queue poisoned");
            pending.drain(..).collect::<Vec<_>>()
        };
        self.wake_pending.store(false, Ordering::Release);
        // Close the reset race: a producer may have queued after the drain but
        // before the flag reset. Re-arm one wake in that case.
        let has_pending = !self
            .pending
            .lock()
            .expect("update queue poisoned")
            .is_empty();
        if has_pending && !self.wake_pending.swap(true, Ordering::AcqRel) {
            self.wake.wake();
        }
        updates
    }
}
