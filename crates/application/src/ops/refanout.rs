//! D26 — re-run-fanout: the notification-fanout twin of
//! [`super::replay_job`]. Authorization is one atomic (command + audit)
//! transaction; the shared leased runner delivers by emitting
//! `RefanoutRequested`, which the notifier consumes downstream.

use crate::model::AdminContext;
use crate::ports::{Clock, OpsJobCommand, Store};

use super::audit::OpsError;
use super::replay_job::{authorize, OpsJobCmd, KIND_REFANOUT};

/// Authorizes a durable re-fanout command for one subject.
///
/// # Errors
/// As [`super::replay_job::authorize`].
pub async fn authorize_refanout<S: Store, C: Clock>(
    store: &S,
    clock: &C,
    actor: &AdminContext,
    cmd: OpsJobCmd,
) -> Result<OpsJobCommand, OpsError> {
    authorize(store, clock, actor, KIND_REFANOUT, cmd).await
}
