//! v1.1 flow engine — PURA-241.
//!
//! Wire-format spec: `docs/flows/architecture.md` + `docs/flows/http-api.md`.
//! This module owns the engine surface (per-flow run dispatch, action
//! execution, persistence) and the trigger sources (cron, manualFire,
//! ts6ClientJoined). It does NOT mount any HTTP routes — that is the
//! F-impl-routes child's responsibility — and it stops short of wiring
//! the production [`ActionDispatcher`] into [`crate::app_state::AppState`];
//! the routes child swaps the basic dispatcher for the real one when it
//! plumbs `AppState`.
//!
//! Concurrency posture (brief §6.3):
//!   - **Per-flow serial** with drop-on-busy. While a flow's slot is
//!     occupied, additional triggers for the same flow are dropped and
//!     counted; the engine does NOT queue.
//!   - **Cross-flow parallel** up to a global semaphore of
//!     `max(4, num_cpus)`. Excess waits.
//!   - **Boot-time invariant**: any `bot_flow_run` left in `in_flight`
//!     at startup is rewritten to `interrupted` before the trigger bus
//!     opens, so the persistence layer matches reality.
//!
//! Failure model (brief §6.4):
//!   - Actions error → action row carries `errored`, subsequent rows
//!     `skipped`, run row `errored`. The flow stays enabled.
//!   - Engine task panics are caught at the per-run boundary; the engine
//!     itself does not die.

// Consumed by F-impl-routes (PURA-241 sibling) — the engine surface is
// complete here but no in-crate caller mounts it yet, so the re-exports
// and several handle methods read as dead until that child lands.
#![allow(dead_code, unused_imports)]

pub mod dispatch;
pub mod engine;
pub mod routes;
pub mod trigger;

#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod routes_tests;

pub use engine::{
    ActionContext, ActionDispatcher, ActionOutcome, BasicDispatcher, EngineDeps, FireError,
    FlowEngine, FlowEngineHandle,
};
pub use trigger::{FloodRegistry, FloodSpec, ParsedTrigger, TriggerEvent};
