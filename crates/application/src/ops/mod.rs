//! Phase 6 ops control plane (D24-D30). Wave W1: the typed config catalog,
//! generation-serialized `SetConfig`, durable two-phase proposals, and the
//! watch-snapshot reconciler. Wave W2: audit plumbing, dual-controlled
//! unwind + receivables + remedial credit, and durable manual-ops commands.

pub mod alerts;
pub mod audit;
pub mod chain_reconcile;
pub mod config;
pub mod fee_override;
pub mod proposals;
pub mod receivable_collection;
pub mod reconciler;
pub mod refanout;
pub mod remedial_credit;
pub mod replay_job;
pub mod set_config;
pub mod unwind_market;
