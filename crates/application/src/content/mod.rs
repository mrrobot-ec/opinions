//! Content publication use cases land here in Task 5.1.
//! (Coordinator re-manifest during the 5.x wave: the module surface below is the
//! frozen contract; the submodule internals belong to worker 5.1.)

pub mod create_draft;
pub mod expiry;
pub mod publish_draft;
pub mod publish_now_command;
pub mod publisher;
pub mod review_draft;
pub mod template_engine;

pub use publisher::publisher_tick;
pub use template_engine::draft_engine;
// Worker 5.1's use cases are reachable by full path (application::content::create_draft::…);
// the pub mod declarations above are the frozen contract — internals belong to 5.1.
