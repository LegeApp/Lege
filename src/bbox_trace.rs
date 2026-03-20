//! Opt-in stderr tracing for layout bboxes, encodes, and PDF placement.
//! Enable with `LEGE_BBOX_TRACE=1` (also accepts `true`, `yes`, `on`).

use std::sync::OnceLock;

pub fn enabled() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var("LEGE_BBOX_TRACE")
            .map(|v| {
                let s = v.to_ascii_lowercase();
                matches!(s.as_str(), "1" | "true" | "yes" | "on")
            })
            .unwrap_or(false)
    })
}
