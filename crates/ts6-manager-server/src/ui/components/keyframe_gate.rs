//! Keyframe gate for the MoQ VP8 player.
//!
//! The sidecar writes one VP8 frame per moq-lite group (1 s GOP). A delta
//! decoded without the keyframe that precedes it paints as static. This
//! state machine starts each subscribe waiting for a keyframe and drops
//! every delta until that key arrives. The same wait applies after a gap
//! and on every resubscribe.
//!
//! Group drains run concurrently, so a later keyframe can be decided
//! before an earlier delta. A delta is accepted only when its group
//! sequence is at or after the keyframe that opened the current epoch.
//!
//! Gaps are: a skipped group sequence, an incomplete group
//! ([`KeyframeGate::note_gap`]), a decoder error, a reconnect, or the tab
//! returning from the background. The caller flushes or rebuilds the
//! `VideoDecoder` when [`GateAction::Feed`] says `flush_before`.

/// What to do with one VP8 frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAction {
    /// Hand the frame to the decoder. `flush_before` means the decoder
    /// still holds samples from before a gap and must be reset first.
    Feed { flush_before: bool },
    /// Discard. The frame is a delta with no decoded keyframe, or it
    /// belongs to a group that arrived after a newer one.
    Drop,
}

/// Whether the group sequence is still worth reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupAdmit {
    /// Sequence is new. Frames still pass through [`KeyframeGate::on_frame`].
    Ready,
    /// This sequence is older than a group already completed. Drop its frames.
    Late,
}

/// Pure accept/drop state for one playback session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyframeGate {
    awaiting_key: bool,
    needs_flush: bool,
    last_completed: Option<u64>,
    /// Group sequence of the keyframe that opened the current decode epoch.
    /// `None` while waiting. Deltas before this sequence stay dropped even
    /// after a newer key has already cleared [`Self::awaiting_key`].
    epoch_key: Option<u64>,
    /// [`Self::begin_subscribe`] has run. The next call is a resubscribe.
    subscribed: bool,
    frames_decoded: u64,
    frames_dropped: u64,
    keyframe_waits: u64,
}

impl Default for KeyframeGate {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyframeGate {
    /// Starts waiting for the first keyframe. That initial wait is counted
    /// so the debug readout is non-zero until a key arrives.
    pub fn new() -> Self {
        Self {
            awaiting_key: true,
            needs_flush: false,
            last_completed: None,
            epoch_key: None,
            subscribed: false,
            frames_decoded: 0,
            frames_dropped: 0,
            keyframe_waits: 1,
        }
    }

    pub fn frames_decoded(&self) -> u64 {
        self.frames_decoded
    }

    pub fn frames_dropped(&self) -> u64 {
        self.frames_dropped
    }

    pub fn keyframe_waits(&self) -> u64 {
        self.keyframe_waits
    }

    pub fn awaiting_key(&self) -> bool {
        self.awaiting_key
    }

    /// Arm a video SUBSCRIBE. The first call keeps the wait counted by
    /// [`Self::new`]. Every later call is a resubscribe: wait for a new
    /// keyframe, drop deltas until it arrives, and count another keyframe
    /// wait. Sequence numbers from the new subscription are not compared
    /// with groups from the previous one.
    pub fn begin_subscribe(&mut self) {
        let resubscribe = self.subscribed;
        self.subscribed = true;
        // Drop the previous epoch so an in-flight delta cannot be decoded
        // against a key from the subscription we just left.
        self.epoch_key = None;
        self.last_completed = None;
        self.awaiting_key = true;
        if resubscribe {
            self.needs_flush = self.needs_flush || self.frames_decoded > 0;
            self.keyframe_waits = self.keyframe_waits.saturating_add(1);
        }
    }

    /// Enter the wait-for-keyframe state. A second call while a wait is
    /// already in progress does not bump the counter again.
    ///
    /// Sets the flush flag so the next accepted keyframe resets the decoder.
    /// The previous epoch key is forgotten, so an in-flight delta cannot
    /// sneak through once a newer key clears the wait flag.
    pub fn note_gap(&mut self) {
        self.needs_flush = true;
        self.epoch_key = None;
        if !self.awaiting_key {
            self.awaiting_key = true;
            self.keyframe_waits = self.keyframe_waits.saturating_add(1);
        }
    }

    /// Inspect a moq group sequence before its frames are decoded.
    ///
    /// A jump of more than one past the last completed group is a gap.
    /// A sequence older than that group is late and must not be decoded.
    pub fn begin_group(&mut self, seq: u64) -> GroupAdmit {
        if let Some(last) = self.last_completed {
            if seq <= last {
                return GroupAdmit::Late;
            }
            if seq != last.saturating_add(1) {
                self.note_gap();
            }
        }
        GroupAdmit::Ready
    }

    /// Decide one frame in `group_seq`.
    ///
    /// `late_group` is the [`GroupAdmit::Late`] snapshot from
    /// [`Self::begin_group`], taken before the frame body is read. It is
    /// not recomputed here: a key admitted on time must still be fed if a
    /// later delta group happens to finish first.
    ///
    /// A non-key is fed only after a keyframe from this subscribe has been
    /// accepted in this group or an earlier one. A keyframe older than the
    /// one that opened the epoch is dropped: concurrent drains can observe
    /// the newer key first.
    pub fn on_frame(&mut self, is_key: bool, group_seq: u64, late_group: bool) -> GateAction {
        if late_group {
            self.frames_dropped = self.frames_dropped.saturating_add(1);
            return GateAction::Drop;
        }
        if !is_key {
            let in_epoch = matches!(
                self.epoch_key,
                Some(key_seq) if !self.awaiting_key && group_seq >= key_seq
            );
            if !in_epoch {
                self.frames_dropped = self.frames_dropped.saturating_add(1);
                return GateAction::Drop;
            }
            self.frames_decoded = self.frames_decoded.saturating_add(1);
            return GateAction::Feed {
                flush_before: false,
            };
        }
        if self.epoch_key.is_some_and(|key_seq| group_seq < key_seq) {
            self.frames_dropped = self.frames_dropped.saturating_add(1);
            return GateAction::Drop;
        }
        let flush_before = self.needs_flush;
        self.needs_flush = false;
        if self.awaiting_key || self.epoch_key.is_none() {
            self.epoch_key = Some(group_seq);
        }
        self.awaiting_key = false;
        self.frames_decoded = self.frames_decoded.saturating_add(1);
        GateAction::Feed { flush_before }
    }

    /// The group was read through to a clean end, including a group whose
    /// frames were all dropped. Sequence order advances so the next group
    /// is not treated as a fresh gap.
    pub fn complete_group(&mut self, seq: u64) {
        match self.last_completed {
            Some(prev) if seq <= prev => {}
            _ => self.last_completed = Some(seq),
        }
    }
}

/// VP8 uncompressed frame header: bit 0 of the first byte is 0 for a keyframe.
pub fn vp8_is_keyframe(data: &[u8]) -> bool {
    !data.is_empty() && (data[0] & 0x01) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vp8_keyframe_bit() {
        assert!(vp8_is_keyframe(&[0x00, 0x11]));
        assert!(vp8_is_keyframe(&[0x10]));
        assert!(!vp8_is_keyframe(&[0x01]));
        assert!(!vp8_is_keyframe(&[0x9d]));
        assert!(!vp8_is_keyframe(&[]));
    }

    #[test]
    fn deltas_wait_for_the_first_keyframe_without_a_flush() {
        let mut gate = KeyframeGate::new();
        assert_eq!(gate.keyframe_waits(), 1);
        assert_eq!(
            gate.begin_group(0),
            GroupAdmit::Ready,
            "the first group is not a gap"
        );
        assert_eq!(gate.on_frame(false, 0, false), GateAction::Drop);
        assert_eq!(gate.frames_dropped(), 1);
        gate.complete_group(0);

        assert_eq!(gate.begin_group(1), GroupAdmit::Ready);
        assert_eq!(
            gate.on_frame(true, 1, false),
            GateAction::Feed {
                flush_before: false
            }
        );
        gate.complete_group(1);
        assert_eq!(gate.frames_decoded(), 1);
        assert!(!gate.awaiting_key());
    }

    #[test]
    fn in_sync_group_decodes_a_key_and_its_deltas() {
        let mut gate = KeyframeGate::new();
        assert_eq!(gate.begin_group(4), GroupAdmit::Ready);
        assert_eq!(
            gate.on_frame(true, 4, false),
            GateAction::Feed {
                flush_before: false
            }
        );
        assert_eq!(
            gate.on_frame(false, 4, false),
            GateAction::Feed {
                flush_before: false
            }
        );
        assert_eq!(
            gate.on_frame(false, 4, false),
            GateAction::Feed {
                flush_before: false
            }
        );
        gate.complete_group(4);
        assert_eq!(gate.frames_decoded(), 3);
        assert_eq!(gate.frames_dropped(), 0);
    }

    #[test]
    fn skipped_group_drops_deltas_until_the_next_key_flushes() {
        let mut gate = KeyframeGate::new();
        assert_eq!(gate.begin_group(0), GroupAdmit::Ready);
        assert_eq!(
            gate.on_frame(true, 0, false),
            GateAction::Feed {
                flush_before: false
            }
        );
        gate.complete_group(0);
        assert_eq!(gate.begin_group(1), GroupAdmit::Ready);
        assert_eq!(
            gate.on_frame(false, 1, false),
            GateAction::Feed {
                flush_before: false
            }
        );
        gate.complete_group(1);

        // Group 2 never arrived.
        assert_eq!(gate.begin_group(3), GroupAdmit::Ready);
        assert!(gate.awaiting_key());
        assert_eq!(gate.keyframe_waits(), 2);
        assert_eq!(gate.on_frame(false, 3, false), GateAction::Drop);
        gate.complete_group(3);

        assert_eq!(gate.begin_group(4), GroupAdmit::Ready);
        assert_eq!(gate.on_frame(false, 4, false), GateAction::Drop);
        gate.complete_group(4);

        assert_eq!(gate.begin_group(5), GroupAdmit::Ready);
        assert_eq!(
            gate.on_frame(true, 5, false),
            GateAction::Feed { flush_before: true }
        );
        gate.complete_group(5);
        assert_eq!(gate.frames_decoded(), 3);
        assert!(!gate.awaiting_key());
        assert_eq!(gate.keyframe_waits(), 2);
    }

    #[test]
    fn note_gap_counts_once_until_a_key_resumes() {
        let mut gate = KeyframeGate::new();
        gate.begin_group(0);
        gate.on_frame(true, 0, false);
        gate.complete_group(0);

        gate.note_gap();
        assert_eq!(gate.keyframe_waits(), 2);
        gate.note_gap();
        assert_eq!(gate.keyframe_waits(), 2, "already waiting");
        assert_eq!(gate.on_frame(false, 1, false), GateAction::Drop);
        assert_eq!(
            gate.on_frame(true, 2, false),
            GateAction::Feed { flush_before: true }
        );
        assert_eq!(gate.keyframe_waits(), 2);
    }

    #[test]
    fn late_group_is_dropped_even_when_it_is_a_keyframe() {
        let mut gate = KeyframeGate::new();
        gate.begin_group(2);
        gate.on_frame(true, 2, false);
        gate.complete_group(2);

        assert_eq!(gate.begin_group(2), GroupAdmit::Late);
        assert_eq!(gate.on_frame(true, 2, true), GateAction::Drop);
        gate.complete_group(2);
        assert_eq!(gate.frames_decoded(), 1);
        assert_eq!(gate.frames_dropped(), 1);

        assert_eq!(gate.begin_group(3), GroupAdmit::Ready);
        assert_eq!(
            gate.on_frame(false, 3, false),
            GateAction::Feed {
                flush_before: false
            }
        );
    }

    #[test]
    fn incomplete_group_does_not_advance_the_sequence() {
        let mut gate = KeyframeGate::new();
        gate.begin_group(0);
        gate.on_frame(true, 0, false);
        gate.complete_group(0);

        gate.begin_group(1);
        gate.note_gap();
        // Caller must not complete_group after a short read.
        assert_eq!(gate.keyframe_waits(), 2);

        assert_eq!(gate.begin_group(2), GroupAdmit::Ready);
        assert_eq!(gate.on_frame(false, 2, false), GateAction::Drop);
        assert_eq!(
            gate.on_frame(true, 2, false),
            GateAction::Feed { flush_before: true }
        );
    }

    #[test]
    fn every_delta_before_the_first_key_is_dropped_and_the_wait_is_counted() {
        let mut gate = KeyframeGate::new();
        gate.begin_subscribe();
        assert!(gate.awaiting_key());
        assert_eq!(
            gate.keyframe_waits(),
            1,
            "initial subscribe keeps the first wait"
        );

        for seq in 0..5 {
            assert_eq!(gate.begin_group(seq), GroupAdmit::Ready);
            assert_eq!(gate.on_frame(false, seq, false), GateAction::Drop);
            gate.complete_group(seq);
        }
        assert_eq!(gate.frames_dropped(), 5);
        assert_eq!(gate.frames_decoded(), 0);
        assert_eq!(gate.keyframe_waits(), 1);

        assert_eq!(gate.begin_group(5), GroupAdmit::Ready);
        assert_eq!(
            gate.on_frame(true, 5, false),
            GateAction::Feed {
                flush_before: false
            }
        );
        assert!(!gate.awaiting_key());
    }

    #[test]
    fn earlier_delta_stays_dropped_after_a_later_key_opens_the_epoch() {
        let mut gate = KeyframeGate::new();
        gate.begin_subscribe();
        // Both groups are admitted before either frame is decided. That is
        // the concurrent drain: the key task can run first.
        assert_eq!(gate.begin_group(10), GroupAdmit::Ready);
        assert_eq!(gate.begin_group(40), GroupAdmit::Ready);

        assert_eq!(
            gate.on_frame(true, 40, false),
            GateAction::Feed {
                flush_before: false
            }
        );
        assert_eq!(gate.on_frame(false, 10, false), GateAction::Drop);
        assert_eq!(
            gate.on_frame(true, 10, false),
            GateAction::Drop,
            "a key older than the epoch key is not a resume point"
        );
        assert_eq!(
            gate.on_frame(false, 40, false),
            GateAction::Feed {
                flush_before: false
            },
            "deltas that share the key's group are in the epoch"
        );
        assert_eq!(
            gate.on_frame(false, 41, false),
            GateAction::Feed {
                flush_before: false
            }
        );
        assert_eq!(gate.frames_dropped(), 2);
        assert_eq!(gate.frames_decoded(), 3);
        assert_eq!(gate.keyframe_waits(), 1);
    }

    #[test]
    fn resubscribe_waits_for_a_key_again_and_counts_it() {
        let mut gate = KeyframeGate::new();
        gate.begin_subscribe();
        gate.begin_group(4);
        gate.on_frame(true, 4, false);
        gate.complete_group(4);
        gate.begin_group(5);
        assert_eq!(
            gate.on_frame(false, 5, false),
            GateAction::Feed {
                flush_before: false
            }
        );
        gate.complete_group(5);
        assert_eq!(gate.keyframe_waits(), 1);

        gate.begin_subscribe();
        assert!(gate.awaiting_key());
        assert_eq!(gate.keyframe_waits(), 2);

        // The new subscription's sequences are not late against the old one.
        assert_eq!(gate.begin_group(0), GroupAdmit::Ready);
        assert_eq!(gate.on_frame(false, 0, false), GateAction::Drop);
        assert_eq!(gate.on_frame(false, 1, false), GateAction::Drop);
        assert_eq!(
            gate.on_frame(true, 2, false),
            GateAction::Feed { flush_before: true }
        );
        assert!(!gate.awaiting_key());
        assert_eq!(gate.keyframe_waits(), 2);
        assert_eq!(
            gate.on_frame(false, 3, false),
            GateAction::Feed {
                flush_before: false
            }
        );
    }
}
