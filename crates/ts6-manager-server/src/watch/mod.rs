//! v1.7 MoQ watch-together spike.
//!
//! Process-local session and playhead state for the panel API. The MoQ
//! relay itself stays on the sidecar's anonymous `/anon` listener; this
//! module is the panel-side authorization and sync surface the watch
//! page calls before it opens WebTransport.
//!
//! One active watch session per panel user. Many users may watch the
//! same broadcast and share one playhead keyed by that broadcast name.

mod store;
pub(crate) mod ticket;

pub(crate) use store::Admit;
pub use store::Store;

use std::time::{SystemTime, UNIX_EPOCH};

/// Unix seconds. `0` when the clock is before the epoch; ticket minting
/// treats that as a failure instead of issuing a born-expired token.
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Broadcast names accepted on the watch surface.
///
/// A name is one or more segments separated by `/` (moq-lite namespaces
/// such as `pura-spike/0`). Each segment is ASCII alphanumeric plus
/// `_`, `.`, and `-`. `.` and `..` segments are rejected.
pub(crate) fn valid_broadcast(name: &str) -> bool {
    if name.is_empty() || name.len() > 128 {
        return false;
    }
    name.split('/').all(|seg| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    })
}

#[cfg(test)]
mod tests {
    use super::valid_broadcast;

    #[test]
    fn broadcast_names() {
        assert!(valid_broadcast("lavfi-spike"));
        assert!(valid_broadcast("room/demo"));
        assert!(valid_broadcast("pura-spike/0"));
        assert!(valid_broadcast("cam_1.main"));
        assert!(!valid_broadcast(""));
        assert!(!valid_broadcast("../x"));
        assert!(!valid_broadcast("a/../b"));
        assert!(!valid_broadcast("/abs"));
        assert!(!valid_broadcast("a/"));
        assert!(!valid_broadcast("has space"));
        assert!(!valid_broadcast("semi;colon"));
        assert!(!valid_broadcast(&"a".repeat(129)));
    }
}
