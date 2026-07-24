//! In-order emission over out-of-order completion.
//!
//! Pages finish in any order; downstream stages (encoders, sequential
//! writers) want them in page order. The buffer holds completed items until
//! their turn.

use std::collections::BTreeMap;

/// Reorders `(sequence, item)` insertions into contiguous in-order drains.
#[derive(Debug)]
pub struct ReorderBuffer<T> {
    next: u64,
    pending: BTreeMap<u64, T>,
}

impl<T> ReorderBuffer<T> {
    /// `first` is the sequence number expected first.
    pub fn new(first: u64) -> Self {
        Self { next: first, pending: BTreeMap::new() }
    }

    pub fn insert(&mut self, seq: u64, item: T) {
        debug_assert!(seq >= self.next, "sequence {seq} already emitted (next={})", self.next);
        self.pending.insert(seq, item);
    }

    /// Remove and return all items that are now contiguous from the head.
    pub fn drain_ready(&mut self) -> impl Iterator<Item = (u64, T)> {
        let mut ready = Vec::new();
        while let Some(entry) = self.pending.first_entry() {
            if *entry.key() == self.next {
                ready.push(entry.remove_entry());
                self.next += 1;
            } else {
                break;
            }
        }
        ready.into_iter()
    }

    /// Items completed but still waiting on earlier sequences.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn next_expected(&self) -> u64 {
        self.next
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn emits_in_order_despite_ooo_completion() {
        let mut buf = ReorderBuffer::new(0);
        buf.insert(2, "c");
        buf.insert(0, "a");
        let first: Vec<_> = buf.drain_ready().collect();
        assert_eq!(first, vec![(0, "a")]);
        assert_eq!(buf.pending_len(), 1);
        buf.insert(1, "b");
        let rest: Vec<_> = buf.drain_ready().collect();
        assert_eq!(rest, vec![(1, "b"), (2, "c")]);
        assert_eq!(buf.next_expected(), 3);
    }

    #[test]
    fn nonzero_start() {
        let mut buf = ReorderBuffer::new(10);
        buf.insert(10, "x");
        assert_eq!(buf.drain_ready().count(), 1);
    }
}
