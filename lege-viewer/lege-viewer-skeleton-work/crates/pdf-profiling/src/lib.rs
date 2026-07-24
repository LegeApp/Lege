//! Feature-gated, allocation-tolerant profiling primitives for the renderer.
//!
//! This crate deliberately has no renderer dependencies and no global state.
//! Callers own one [`ProfileReport`] per operation or worker, then merge the
//! reports at a synchronization point.  Renderer crates depend on it only
//! through their `profiling` feature, so production builds do not execute
//! timers or counter updates.

use std::collections::BTreeMap;
use std::time::Duration;

/// A stable, machine-readable profile payload for one operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileReport {
    /// Versioned independently so report readers can reject incompatible rows.
    pub schema_version: u32,
    /// Inclusive named stage durations. Repeated scopes are accumulated.
    pub durations: BTreeMap<&'static str, Duration>,
    /// Named event and work counters. Repeated updates are accumulated.
    pub counters: BTreeMap<&'static str, u64>,
    /// Renderer-owned bytes live when the operation ended.
    pub live_bytes: u64,
    /// Maximum renderer-owned bytes observed during the operation.
    pub peak_live_bytes: u64,
}

impl ProfileReport {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn new() -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            ..Self::default()
        }
    }

    pub fn add_duration(&mut self, name: &'static str, duration: Duration) {
        *self.durations.entry(name).or_default() += duration;
    }

    pub fn increment(&mut self, name: &'static str, amount: u64) {
        *self.counters.entry(name).or_default() += amount;
    }

    /// Account for a renderer-owned allocation. `release_bytes` must be
    /// called when it ceases to be live; this intentionally models only
    /// buffers the renderer can name, not the process allocator as a whole.
    pub fn allocate_bytes(&mut self, bytes: u64) {
        self.live_bytes = self.live_bytes.saturating_add(bytes);
        self.peak_live_bytes = self.peak_live_bytes.max(self.live_bytes);
    }

    pub fn release_bytes(&mut self, bytes: u64) {
        self.live_bytes = self.live_bytes.saturating_sub(bytes);
    }

    pub fn merge(&mut self, other: &Self) {
        self.schema_version = self.schema_version.max(other.schema_version);
        for (&name, &duration) in &other.durations {
            self.add_duration(name, duration);
        }
        for (&name, &count) in &other.counters {
            self.increment(name, count);
        }
        self.peak_live_bytes = self.peak_live_bytes.max(other.peak_live_bytes);
        self.live_bytes = self.live_bytes.saturating_add(other.live_bytes);
    }

    /// Render the report's flattened metrics as JSON object fields. Kept here
    /// to avoid making serde a production or toolchain dependency.
    pub fn write_json_fields(&self, out: &mut String) {
        use std::fmt::Write as _;
        let _ = write!(out, "\"profile_schema\":{}", self.schema_version);
        for (&name, duration) in &self.durations {
            let _ = write!(out, ",\"{}\":{}", name, duration.as_nanos());
        }
        for (&name, count) in &self.counters {
            let _ = write!(out, ",\"{}\":{}", name, count);
        }
        let _ = write!(
            out,
            ",\"live_bytes\":{},\"peak_live_bytes\":{}",
            self.live_bytes, self.peak_live_bytes
        );
    }
}

impl From<()> for ProfileReport {
    fn from(_: ()) -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn accumulates_and_merges() {
        let mut a = ProfileReport::new();
        a.add_duration("render.execute", Duration::from_nanos(3));
        a.increment("image.draws", 2);
        a.allocate_bytes(10);
        a.release_bytes(4);
        let mut b = ProfileReport::new();
        b.add_duration("render.execute", Duration::from_nanos(2));
        b.increment("image.draws", 1);
        b.allocate_bytes(20);
        a.merge(&b);
        assert_eq!(a.durations["render.execute"], Duration::from_nanos(5));
        assert_eq!(a.counters["image.draws"], 3);
        assert_eq!(a.peak_live_bytes, 20);
        assert_eq!(a.schema_version, ProfileReport::SCHEMA_VERSION);
    }
}
