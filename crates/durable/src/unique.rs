//! Unique name suffixes.

use std::sync::atomic::{AtomicU64, Ordering};

/// Maximum attempts to find a free temp or trash name.
pub(crate) const MAX_ATTEMPTS: u32 = 16;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// `<pid>-<seq>`: unique within the process by the counter, across processes by the pid.
pub(crate) fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}
