//! Bounded snapshot cache for `lege-pdf serve`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use pdf_document::DocumentSnapshot;

use crate::open::{self, DocumentIdentity};

#[derive(Debug)]
struct Entry {
    identity: DocumentIdentity,
    snapshot: Arc<DocumentSnapshot>,
    last_used: Instant,
    password: Option<String>,
}

/// Whether a snapshot opened with `stored` may be reused to answer a later
/// request that supplied `requested`. Asymmetric by design: a password-less
/// request must never be handed a snapshot that required a password to
/// open — otherwise one client's password would silently unlock the
/// document for every subsequent password-less request against the same
/// path for the rest of the cache entry's `idle_timeout`.
fn password_matches(requested: Option<&str>, stored: Option<&str>) -> bool {
    match (requested, stored) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b,
        (None, Some(_)) => false,
        (Some(_), None) => false,
    }
}

/// LRU cache of open document snapshots keyed by path identity.
#[derive(Debug)]
pub struct SnapshotCache {
    max_open: usize,
    idle_timeout: Option<Duration>,
    entries: HashMap<String, Entry>,
}

impl SnapshotCache {
    pub fn new(max_open: usize, idle_timeout_secs: u64) -> Self {
        Self {
            max_open: max_open.max(1),
            idle_timeout: if idle_timeout_secs == 0 {
                None
            } else {
                Some(Duration::from_secs(idle_timeout_secs))
            },
            entries: HashMap::new(),
        }
    }

    pub fn get_or_open(
        &mut self,
        path: &Path,
        password: Option<&str>,
    ) -> Result<(DocumentIdentity, Arc<DocumentSnapshot>)> {
        self.evict_idle();
        let identity = DocumentIdentity::from_path(path)?;
        let key = identity.display_path();

        if let Some(entry) = self.entries.get_mut(&key) {
            let password_ok = password_matches(password, entry.password.as_deref());
            if entry.identity == identity && password_ok {
                entry.last_used = Instant::now();
                return Ok((entry.identity.clone(), Arc::clone(&entry.snapshot)));
            }
            // File changed or password mismatch — drop and reopen.
            self.entries.remove(&key);
        }

        self.evict_if_full();
        let snapshot = open::open_with_identity(&identity, password)?;
        let snapshot = Arc::new(snapshot);
        self.entries.insert(
            key,
            Entry {
                identity: identity.clone(),
                snapshot: Arc::clone(&snapshot),
                last_used: Instant::now(),
                password: password.map(str::to_owned),
            },
        );
        Ok((identity, snapshot))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn evict_idle(&mut self) {
        let Some(timeout) = self.idle_timeout else {
            return;
        };
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.last_used) <= timeout);
    }

    fn evict_if_full(&mut self) {
        while self.entries.len() >= self.max_open {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone());
            let Some(key) = oldest else {
                break;
            };
            self.entries.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_matches_never_serves_a_protected_snapshot_password_less() {
        assert!(password_matches(None, None));
        assert!(password_matches(Some("secret"), Some("secret")));
        assert!(!password_matches(Some("wrong"), Some("secret")));
        // The bug: a snapshot opened with a password must not be reused for
        // a later request that supplied none.
        assert!(!password_matches(None, Some("secret")));
        // Symmetric case, already correct before the fix: a request that
        // supplies a password must not reuse a password-less snapshot.
        assert!(!password_matches(Some("secret"), None));
    }
}
