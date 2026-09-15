//! Injected LLM adapters land here in Task 5.4.

mod draft;
mod moderation;
mod selection;
pub mod transport;

pub use selection::{draft_engine_from_env, moderation_preflight_from_env};
