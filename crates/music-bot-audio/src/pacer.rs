//! Wall-clock 20 ms pacer.
//!
//! The pacer's only job is to assign each frame an `Instant` such that
//! `frame[N].scheduled_at == start + N * 20ms` — drift-free.
//!
//! WS-1 (bot lifecycle) is responsible for *waiting* until that `Instant`
//! before sending the frame on the wire. Why split it: the pacer crate has
//! no opinion about back-pressure or transmission, and the test harness can
//! re-time frames against an arbitrary clock.

use std::time::{Duration, Instant};

use crate::types::frame_duration;

#[derive(Debug, Clone)]
pub struct WallClockPacer {
    start: Instant,
    period: Duration,
    next_index: u64,
}

impl WallClockPacer {
    pub fn new(start: Instant) -> Self {
        Self {
            start,
            period: frame_duration(),
            next_index: 0,
        }
    }

    pub fn start(&self) -> Instant {
        self.start
    }

    /// Returns `(index, scheduled_at)` for the next paced frame and advances
    /// the cursor. `scheduled_at = start + index * 20ms`, never `now`.
    pub fn tick(&mut self) -> (u64, Instant) {
        let i = self.next_index;
        self.next_index += 1;
        (i, self.start + self.period * i as u32)
    }

    /// Total paced frames emitted so far.
    pub fn count(&self) -> u64 {
        self.next_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_free_schedule() {
        let start = Instant::now();
        let mut p = WallClockPacer::new(start);
        for expected in 0..50_u64 {
            let (i, at) = p.tick();
            assert_eq!(i, expected);
            let delta = at.duration_since(start);
            assert_eq!(delta, Duration::from_millis(20) * expected as u32);
        }
    }
}
