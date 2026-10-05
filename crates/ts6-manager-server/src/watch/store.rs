//! In-memory watch sessions and playheads.
//!
//! The panel process is a single replica (the same assumption as
//! `AppState::setup_lock`). A restart drops every session and playhead.
//! Idle sessions expire so a closed tab does not hold the user's slot
//! until the next deploy.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// How long a session stays reserved after the last panel touch
/// (mint, ticket refresh, redeem, or playhead read/write).
pub(crate) const IDLE_SECS: i64 = 15 * 60;

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub broadcast: String,
    pub idle_deadline: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct Playhead {
    pub position_ms: u64,
    pub paused: bool,
    pub updated_at: i64,
    pub updated_by: i64,
}

pub(crate) enum Admit {
    Created(Session),
    /// The user already holds a live session. `replace: true` was not set.
    Active {
        session_id: String,
        broadcast: String,
    },
}

#[derive(Default)]
struct Inner {
    /// One live session per panel user id.
    sessions: HashMap<i64, Session>,
    /// Shared clock per broadcast name.
    playheads: HashMap<String, Playhead>,
}

/// Cheap to clone: handlers share one map via `Arc`.
#[derive(Clone, Default)]
pub struct Store {
    inner: Arc<Mutex<Inner>>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Reserve the user's single watch slot, or report the slot already
    /// in use. An idle-expired row is dropped and does not conflict.
    pub(crate) fn admit(&self, user_id: i64, broadcast: &str, replace: bool, now: i64) -> Admit {
        let mut guard = self.lock();
        if let Some(existing) = guard.sessions.get(&user_id).cloned() {
            if existing.idle_deadline > now && !replace {
                return Admit::Active {
                    session_id: existing.id,
                    broadcast: existing.broadcast,
                };
            }
            guard.sessions.remove(&user_id);
        }
        let session = Session {
            id: new_id(),
            broadcast: broadcast.to_string(),
            idle_deadline: now.saturating_add(IDLE_SECS),
        };
        guard.sessions.insert(user_id, session.clone());
        Admit::Created(session)
    }

    /// Drop the user's session when `session_id` is the live one.
    pub(crate) fn release(&self, user_id: i64, session_id: &str) -> bool {
        let mut guard = self.lock();
        match guard.sessions.get(&user_id) {
            Some(session) if session.id == session_id => {
                guard.sessions.remove(&user_id);
                true
            }
            _ => false,
        }
    }

    /// Live session for `user_id`, if any. Does not move the idle deadline.
    /// An expired row is removed.
    pub(crate) fn live_session(&self, user_id: i64, now: i64) -> Option<Session> {
        let mut guard = self.lock();
        let session = guard.sessions.get(&user_id)?.clone();
        if session.idle_deadline <= now {
            guard.sessions.remove(&user_id);
            return None;
        }
        Some(session)
    }

    /// Extend the idle deadline when `session_id` is still the user's
    /// live session. A stale id (replaced or released) returns `None`
    /// and does not touch the replacement.
    pub(crate) fn touch_session(
        &self,
        user_id: i64,
        session_id: &str,
        now: i64,
    ) -> Option<Session> {
        let mut guard = self.lock();
        let session = guard.sessions.get(&user_id)?.clone();
        if session.idle_deadline <= now {
            guard.sessions.remove(&user_id);
            return None;
        }
        if session.id != session_id {
            return None;
        }
        let mut session = session;
        session.idle_deadline = now.saturating_add(IDLE_SECS);
        guard.sessions.insert(user_id, session.clone());
        Some(session)
    }

    /// Extend the idle deadline of whatever session the user currently holds.
    pub(crate) fn touch_live(&self, user_id: i64, now: i64) -> Option<Session> {
        let mut guard = self.lock();
        let session = guard.sessions.get(&user_id)?.clone();
        if session.idle_deadline <= now {
            guard.sessions.remove(&user_id);
            return None;
        }
        let mut session = session;
        session.idle_deadline = now.saturating_add(IDLE_SECS);
        guard.sessions.insert(user_id, session.clone());
        Some(session)
    }

    pub(crate) fn playhead(&self, broadcast: &str) -> Option<Playhead> {
        self.lock().playheads.get(broadcast).copied()
    }

    pub(crate) fn put_playhead(
        &self,
        broadcast: &str,
        position_ms: u64,
        paused: bool,
        user_id: i64,
        now: i64,
    ) {
        self.lock().playheads.insert(
            broadcast.to_string(),
            Playhead {
                position_ms,
                paused,
                updated_at: now,
                updated_by: user_id,
            },
        );
    }
}

fn new_id() -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    hex::encode(bytes)
}
