//! simswarm — the Phase 6 external test harness (plan D28).
//!
//! Drives the Opinions system EXCLUSIVELY through the public REST/WS API.
//! No internal crate dependencies: wire DTOs live in [`wire`], and drift
//! against the generated `openapi.json` is caught by a contract test, not by
//! importing adapter types.
//!
//! Module contract (frozen at Task 6.0a; internals belong to the wave):
//! - [`domain`]  (W3): personas, rings, Poisson schedules, decision fns — pure.
//! - [`engine`]  (W3): per-agent Tokio tasks over the [`ports`] role traits.
//! - [`manifest`] / [`trace`] (W3): versioned run manifest + PlannedOpportunity
//!   / DecidedAction artifacts (dry-run and replay contracts).
//! - [`transport`] (W3): the ONLY module allowed to construct an HTTP client
//!   (enforced by `just deps-check`, same rule as `adapters/src/llm/transport.rs`).
//! - [`invariants_client`] (W3): calls `GET /admin/invariants` in quiet segments.
//! - [`chaos`] (W4): process kill / webhook storm / crash-barrier controller.

pub mod chaos;
pub mod domain;
pub mod engine;
pub mod invariants_client;
pub mod manifest;
pub mod ports;
pub mod trace;
pub mod transport;
pub mod wire;
