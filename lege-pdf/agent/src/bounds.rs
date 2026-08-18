//! Safe defaults for agent-facing expansive operations.

/// Shared resource bounds applied to every expansive command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Maximum pages processed per invocation (0 = unlimited).
    pub max_pages: u32,
    /// Maximum inventory / op / match items emitted per page or command.
    pub max_items: u32,
    /// Soft cap on serialized payload bytes for a single record.
    pub max_bytes: u64,
    /// Wall-clock timeout hint in seconds (0 = none).
    pub timeout_secs: u64,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            max_pages: 50,
            max_items: 10_000,
            max_bytes: 8 * 1024 * 1024,
            timeout_secs: 60,
        }
    }
}

impl Bounds {
    pub fn truncate_items<T>(
        &self,
        mut items: Vec<T>,
        warnings: &mut Vec<String>,
        label: &str,
    ) -> Vec<T> {
        if self.max_items > 0 && items.len() as u32 > self.max_items {
            warnings.push(format!(
                "{label} truncated from {} to max-items={}",
                items.len(),
                self.max_items
            ));
            items.truncate(self.max_items as usize);
        }
        items
    }

    /// Returns true when `serialized_len` exceeds the soft payload cap.
    pub fn exceeds_bytes(&self, serialized_len: usize) -> bool {
        self.max_bytes > 0 && serialized_len as u64 > self.max_bytes
    }
}
