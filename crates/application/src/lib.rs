//! Application layer: use cases over ports (hexagonal core). Dependencies
//! point inward — this crate depends on `domain` only; no SQL and no
//! SQL-/HTTP-framework types anywhere (machine-enforced by `just deps-check`).

pub mod advance_due;
pub mod advance_market;
pub mod cast_vote;
pub mod comments;
pub mod content;
pub mod contract;
pub mod create_user;
pub mod credit_deposit;
pub mod ensure_genesis;
pub mod error;
pub mod fakes;
pub mod integrity;
pub mod model;
pub mod moderation_escalate;
pub mod money;
pub mod notify;
pub mod ops;
pub mod place_trade;
pub mod ports;
pub mod preview_trade;
pub mod resolve_market;
pub mod seed_market;
pub mod video;
