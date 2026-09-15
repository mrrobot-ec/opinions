//! Completion + attachment ride ONE transaction (codex B5): the token-fenced
//! CAS to `ready` and the market-locked column write + `VideoAttached` event
//! commit together, so a crash never strands an unattached ready artifact and
//! a stale attempt can never attach.

use time::OffsetDateTime;

use crate::error::StoreError;
use crate::model::JobId;
use crate::ports::VideoTx;

/// Applies a successful render: CAS to `ready`, then attach under the market
/// row lock. Returns `false` (dropping the result silently) when a newer
/// attempt owns the job.
///
/// # Errors
/// Backend failures only; a lost CAS is `Ok(false)`, not an error.
pub async fn complete_and_attach(
    tx: &mut (dyn VideoTx + '_),
    job: JobId,
    token: uuid::Uuid,
    asset_url: &str,
    now: OffsetDateTime,
) -> Result<bool, StoreError> {
    if !tx.complete_ready(job, token, asset_url, now).await? {
        return Ok(false);
    }
    if !tx.attach_ready(job).await? {
        return Err(StoreError::Invariant("completed job did not attach"));
    }
    Ok(true)
}
