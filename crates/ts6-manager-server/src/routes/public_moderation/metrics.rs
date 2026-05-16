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

impl Counters {
    fn new() -> Self {
        Counters {
            cells: std::array::from_fn(|_| std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }
}

static COUNTERS: LazyLock<Counters> = LazyLock::new(Counters::new);

fn index(items: &[&str], value: &str) -> Option<usize> {
    items.iter().position(|item| *item == value)
}

/// Increment one `(kind, outcome)` cell of `counters`. An unknown label is
/// logged and dropped (see [`record`]). Split from [`record`] so tests can
/// drive a fresh, isolated [`Counters`] — the process-global counter
/// cannot be asserted on under parallel `cargo test`.
fn record_into(counters: &Counters, kind: &str, outcome: &str) {
    match (index(&KINDS, kind), index(&OUTCOMES, outcome)) {
        (Some(k), Some(o)) => {
            counters.cells[k][o].fetch_add(1, Ordering::Relaxed);
        }
        _ => tracing::error!(
            kind,
            outcome,
            "public moderation metric: unknown kind/outcome label",
        ),
    }
}

/// Increment `moderation_public_submissions_total{kind,outcome}`. An
/// unknown `kind` / `outcome` is a programming error — it is logged and
/// dropped rather than panicking a request thread.
pub fn record(kind: &str, outcome: &str) {
    record_into(&COUNTERS, kind, outcome);
}

/// Every `(kind, outcome, count)` cell — the full label cross-product,
/// so a never-incremented series still renders as `0` (Prometheus
/// scrapers prefer a stable series set).
pub fn snapshot() -> Vec<(&'static str, &'static str, u64)> {
    snapshot_of(&COUNTERS)
}

/// [`snapshot`] of an explicit [`Counters`] — lets a test read back a
/// fresh, isolated counter rather than the process-global one.
fn snapshot_of(counters: &Counters) -> Vec<(&'static str, &'static str, u64)> {
    let mut out = Vec::with_capacity(KINDS.len() * OUTCOMES.len());
    for (k, kind) in KINDS.iter().enumerate() {
        for (o, outcome) in OUTCOMES.iter().enumerate() {
            out.push((
                *kind,
                *outcome,
                counters.cells[k][o].load(Ordering::Relaxed),
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
        // A fresh, isolated counter — deterministic under parallel
        // `cargo test`, unlike the process-global `COUNTERS`.
        let counters = Counters::new();
        record_into(&counters, "appeal", "accepted");
        let cell = snapshot_of(&counters)
            .into_iter()
            .find(|(k, o, _)| *k == "appeal" && *o == "accepted")
            .map(|(_, _, n)| n)
            .unwrap();
        assert_eq!(cell, 1, "the addressed cell was incremented");
        let total: u64 = snapshot_of(&counters).iter().map(|(_, _, n)| n).sum();
        assert_eq!(total, 1, "only the addressed cell moved");
    }

    #[test]
    fn record_with_unknown_label_is_a_no_op() {
        // Fresh counter so the assertion is exact: an unknown label must
        // touch no cell at all (not just leave a racy global sum stable).
        let counters = Counters::new();
        record_into(&counters, "not-a-kind", "accepted");
        record_into(&counters, "report", "not-an-outcome");
        let total: u64 = snapshot_of(&counters).iter().map(|(_, _, n)| n).sum();
        assert_eq!(total, 0, "unknown labels must not touch any cell");
    }
}
