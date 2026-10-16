//! Running one [`queue::Job`] — shared by the worker Lambda (`bin/worker`)
//! and `bin/poem-local`'s in-process stand-in for it.

use anyhow::Result;
use tracing::info;

use crate::app::{HasDb, HasMail, HasQueue};
use crate::broadcast_send;
use crate::queue::Job;

/// Run `job`. An `Err` fails the SQS message so it is retried (and, after
/// the queue's `maxReceiveCount`, dead-lettered).
pub async fn run_job<A: HasDb + HasMail + HasQueue + Sync>(app: &A, job: &Job) -> Result<()> {
    match job {
        Job::BroadcastBatch { broadcast_id, seq } => {
            let outcome = broadcast_send::run_batch(app, broadcast_id, *seq).await?;
            info!(broadcast_id, seq, ?outcome, "broadcast batch job done");
        }
    }
    Ok(())
}
