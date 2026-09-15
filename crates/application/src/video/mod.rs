//! Artifact job use cases land here in Task 5.2.

pub mod artifacts;
pub mod attach_ready;
pub mod jobs;
pub mod share_card;
pub mod worker;

pub use worker::worker_tick;
// Worker 5.2's use cases are reachable by full path; pub mod is the frozen contract
// (coordinator re-manifest during the wave — mirrors content/mod.rs).
