//! PostgreSQL implementation of the application ports.
#![allow(
    clippy::doc_markdown,
    clippy::needless_pass_by_value,
    clippy::missing_errors_doc
)]

mod advance_tx;
mod alert_tx;
mod comment_tx;
mod compliance_tx;
mod content_tx;
mod credit_tx;
mod deposit_tx;
mod economy_tx;
mod integrity_tx;
mod invariant_read_tx;
mod notification_tx;
mod ops_audit_tx;
mod ops_config_tx;
mod outbound_tx;
mod resolve_tx;
mod rows;
mod store;
mod trade_tx;
mod unwind_tx;
mod video_tx;
mod vote_tx;
mod withdraw_tx;

pub use alert_tx::PgAlertStore;
pub use compliance_tx::PgComplianceStore;
pub use store::PgStore;
pub use withdraw_tx::PgWithdrawStore;
