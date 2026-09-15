//! Axum HTTP adapter: DTOs, error envelope, routes, OpenAPI.

#![allow(
    clippy::wildcard_imports,
    clippy::enum_glob_use,
    clippy::module_name_repetitions
)]

pub mod dto;
pub mod error;
pub mod middleware;
pub mod routes;
pub mod ws;

pub use routes::{router, AppState, AppStateInner, Phase5Services, VoteMetadataConfig};
