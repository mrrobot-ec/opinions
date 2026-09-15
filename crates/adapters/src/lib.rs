//! Outward adapters for the opinion-market application core.
//!
//! - [`http`]: Axum routes + OpenAPI (Task 1.4)
//! - [`pg`]: SQLx Postgres ports (Task 1.3)

// Pedantic doc style is enforced at the application/domain layer; adapter
// bindings use many framework names (Axum, SQLx, OpenAPI) that trip
// clippy::doc_markdown without improving clarity.
#![allow(
    clippy::doc_markdown,
    clippy::needless_raw_string_hashes,
    clippy::wildcard_imports,
    clippy::enum_glob_use,
    clippy::module_name_repetitions,
    clippy::needless_pass_by_value,
    clippy::missing_errors_doc,
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::redundant_closure_for_method_calls
)]

pub mod compliance;
pub mod http;
pub mod inbox;
pub mod llm;
pub mod money_ports;
pub mod notifier;
pub mod pg;
pub mod phone;
pub mod rails;
pub mod relay;
pub mod render;
