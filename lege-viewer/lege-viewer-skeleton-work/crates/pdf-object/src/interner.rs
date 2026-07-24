//! Per-document name interning.
//!
//! Scaling rationale (blueprint §4.1): a large document resolves millions of
//! dictionary keys; interning turns every key comparison into a `u32`
//! compare and removes per-key heap traffic from the hot resolution path.
//!
//! Concurrency model: interning is heaviest during the single-threaded
//! document-open phase; afterwards workers mostly *look up* names that
//! already exist. A `RwLock` around the map gives uncontended reads for that
//! pattern; ids are dense indices into an append-only table. If contention
//! ever shows up in profiles, swap the interior for sharded maps without
//! changing `NameId` or the public API.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Interned name id. Dense, per-document, 4 bytes. Never compare ids from
/// different documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NameId(u32);

impl NameId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug, Default)]
struct Inner {
    map: HashMap<Arc<[u8]>, NameId>,
    table: Vec<Arc<[u8]>>,
}

/// Thread-safe append-only name interner.
#[derive(Debug, Default)]
pub struct NameInterner {
    inner: RwLock<Inner>,
}

impl NameInterner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `name`, returning its stable id. Read-lock fast path.
    pub fn intern(&self, name: &[u8]) -> NameId {
        if let Some(id) = self.lookup(name) {
            return id;
        }
        // Poison recovery is sound: the map/table pair is consistent after
        // every completed statement, so a panicked writer leaves valid data.
        let mut inner = self.inner.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Re-check: another writer may have inserted between our read and
        // write acquisition.
        if let Some(id) = inner.map.get(name) {
            return *id;
        }
        // Saturate on the (physically unreachable — 2^32 names would need
        // hundreds of GB) table overflow rather than panicking a page.
        let id = NameId(u32::try_from(inner.table.len()).unwrap_or(u32::MAX));
        let key: Arc<[u8]> = Arc::from(name);
        inner.table.push(Arc::clone(&key));
        inner.map.insert(key, id);
        id
    }

    /// Look up without inserting.
    pub fn lookup(&self, name: &[u8]) -> Option<NameId> {
        // See `intern` for why poison recovery is sound here.
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map
            .get(name)
            .copied()
    }

    /// Resolve an id back to its bytes. A foreign/corrupt id resolves to the
    /// empty name rather than panicking (never-panic invariant).
    pub fn resolve(&self, id: NameId) -> Arc<[u8]> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .table
            .get(id.index())
            .cloned()
            .unwrap_or_else(|| Arc::from(&b""[..]))
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap_or_else(std::sync::PoisonError::into_inner).table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::thread;

    #[test]
    fn intern_is_idempotent() {
        let i = NameInterner::new();
        let a = i.intern(b"Type");
        let b = i.intern(b"Type");
        assert_eq!(a, b);
        assert_eq!(i.resolve(a).as_ref(), b"Type");
        assert_eq!(i.len(), 1);
    }

    #[test]
    fn concurrent_interning_converges() {
        let i = Arc::new(NameInterner::new());
        let names: Vec<Vec<u8>> = (0..64).map(|n| format!("Name{}", n % 16).into_bytes()).collect();
        thread::scope(|s| {
            for _ in 0..8 {
                let i = Arc::clone(&i);
                let names = names.clone();
                s.spawn(move || {
                    for n in &names {
                        let id = i.intern(n);
                        assert_eq!(i.resolve(id).as_ref(), &n[..]);
                    }
                });
            }
        });
        assert_eq!(i.len(), 16);
    }
}
