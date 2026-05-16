//! `moderation_public_submissions_total{kind,outcome}` — abuse telemetry
//! for the public moderation surface (PURA-307, brief §4.8).
//!
//! Every public submission emits a structured log line *and* bumps one of
//! these counters, so an operator sees an abuse spike on a dashboard
//! before it becomes a support ticket (engineering doctrine: telemetry
//! over guessing).
//!
//! The counter set is a process-wide [`LazyLock`] rather than a field on
//! `AppState` — it is a monotonic process counter, mirroring how the WS
//! hub's metrics are global, and avoids threading a handle through every
//! `AppState` literal. [`crate::routes::metrics`] reads [`snapshot`] to
//! render the Prometheus exposition line.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// `kind` label values — which public flow produced the submission.
pub const KINDS: [&str; 3] = ["report", "appeal", "report_link"];
/// `outcome` label values. `accepted` — the row was stored / the poke was
/// delivered. `rejected` — a client-side refusal (validation, bad token,
/// rate limit, conflict). `error` — a server-side failure.
pub const OUTCOMES: [&str; 3] = ["accepted", "rejected", "error"];

/// One `AtomicU64` per `(kind, outcome)` cell.
struct Counters {
    cells: [[AtomicU64; OUTCOMES.len()]; KINDS.len()],
}

static COUNTERS: LazyLock<Counters> = LazyLock::new(|| Counters {
    cells: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
});

fn index(items: &[&str], value: &str) -> Option<usize> {
    items.iter().position(|item| *item == value)
}

/// Increment `moderation_public_submissions_total{kind,outcome}`. An
/// unknown `kind` / `outcome` is a programming error — it is logged and
/// dropped rather than panicking a request thread.
pub fn record(kind: &str, outcome: &str) {
    match (index(&KINDS, kind), index(&OUTCOMES, outcome)) {
        (Some(k), Some(o)) => {
            COUNTERS.cells[k][o].fetch_add(1, Ordering::Relaxed);
        }
        _ => tracing::error!(
            kind,
            outcome,
            "public moderation metric: unknown kind/outcome label",
        ),
    }
}

/// Every `(kind, outcome, count)` cell — the full label cross-product,
/// so a never-incremented series still renders as `0` (Prometheus
/// scrapers prefer a stable series set).
pub fn snapshot() -> Vec<(&'static str, &'static str, u64)> {
    let mut out = Vec::with_capacity(KINDS.len() * OUTCOMES.len());
    for (k, kind) in KINDS.iter().enumerate() {
        for (o, outcome) in OUTCOMES.iter().enumerate() {
            out.push((
                *kind,
                *outcome,
                COUNTERS.cells[k][o].load(Ordering::Relaxed),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_covers_the_full_label_cross_product() {
        let snap = snapshot();
        assert_eq!(snap.len(), KINDS.len() * OUTCOMES.len());
        for kind in KINDS {
            for outcome in OUTCOMES {
                assert!(
                    snap.iter().any(|(k, o, _)| *k == kind && *o == outcome),
                    "missing series {kind}/{outcome}",
                );
            }
        }
    }

    #[test]
    fn record_increments_the_addressed_cell() {
        // Process-global counter — assert on a delta, never an absolute.
        let before = snapshot()
            .into_iter()
            .find(|(k, o, _)| *k == "appeal" && *o == "accepted")
            .map(|(_, _, n)| n)
            .unwrap();
        record("appeal", "accepted");
        let after = snapshot()
            .into_iter()
            .find(|(k, o, _)| *k == "appeal" && *o == "accepted")
            .map(|(_, _, n)| n)
            .unwrap();
        assert_eq!(after, before + 1);
    }

    #[test]
    fn record_with_unknown_label_is_a_no_op() {
        // Must not panic; the totals are unchanged for valid series.
        let before: u64 = snapshot().iter().map(|(_, _, n)| n).sum();
        record("not-a-kind", "accepted");
        record("report", "not-an-outcome");
        let after: u64 = snapshot().iter().map(|(_, _, n)| n).sum();
        assert_eq!(after, before, "unknown labels must not touch any cell");
    }
}
