//! Sending a queued broadcast, a paced batch at a time — the worker's half
//! of `sendBroadcast`.
//!
//! `sendBroadcast` validates a broadcast, writes its row with every recipient
//! `pending`, and queues [`queue::Job::BroadcastBatch`] `seq 0` ([`start`]).
//! The worker runs [`run_batch`] for it, which sends the next
//! [`broadcast::BATCH_SIZE`] pending recipients and queues `seq + 1` with a
//! [`broadcast::BATCH_INTERVAL_SECS`] delay, until nobody is left. See
//! CLAUDE.md's "Broadcasts" entry.
//!
//! **At most once per recipient, whatever SQS does.** SQS delivers a job at
//! least once, and a failed job is retried, so a batch may run twice. Every
//! recipient is therefore *claimed* (`pending` → `sending`, conditionally)
//! before its copy is built and sent; a recipient someone else claimed is
//! skipped. A crash between claim and result leaves it `sending` for good —
//! never retried, since the mail may already be out.
//!
//! **One chain, not two.** Each batch queues its successor *before*
//! recording its own progress (`send_seq = seq + 1`), so a failure anywhere
//! leaves `send_seq` at `seq` and SQS's retry picks up where it left off. A
//! job whose `seq` is below the stored `send_seq` is a late duplicate and is
//! dropped. The one window left — a duplicate running at the same moment as
//! the original, or a retry after the successor was queued but before the
//! progress write landed — can queue two successors, and the two chains may
//! then run side by side, doubling the pace for the rest of that broadcast.
//! The claims still keep every copy single; only the pacing suffers, and only
//! after an SQS duplicate or a failed DynamoDB write.

use std::collections::HashMap;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::app::{HasDb, HasMail, HasQueue};
use crate::broadcast;
use crate::db::{self, Handler as _};
use crate::mail::Handler as _;
use crate::queue::{Handler as _, Job};
use crate::suppression;

/// What one [`run_batch`] did, for the worker's log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOutcome {
    /// No such broadcast.
    Missing,
    /// `seq` is behind the stored `send_seq`: a duplicate delivery.
    Stale,
    /// The instance is gone (deleted, or no longer a support instance):
    /// every pending recipient was recorded `failed` instead of sent.
    InstanceGone { failed: usize },
    /// A batch went out. `remaining` pending recipients are left for the
    /// next batch (queued when non-zero).
    Sent {
        sent: usize,
        failed: usize,
        suppressed: usize,
        remaining: usize,
    },
}

/// Queue a newly written broadcast's first batch, to run straight away.
pub async fn start<A: HasQueue + Sync>(app: &A, broadcast_id: &str) -> Result<()> {
    app.queue()
        .send_job(&batch_job(broadcast_id, 0), 0)
        .await
        .context("queueing the broadcast's first batch")
}

fn batch_job(broadcast_id: &str, seq: u64) -> Job {
    Job::BroadcastBatch {
        broadcast_id: broadcast_id.to_string(),
        seq,
    }
}

/// Send batch `seq` of broadcast `broadcast_id` — see this module's doc
/// comment. An `Err` means the job should be retried (SQS redelivers it);
/// a single recipient's send failure is not one — it is recorded `failed`.
pub async fn run_batch<A: HasDb + HasMail + HasQueue + Sync>(
    app: &A,
    broadcast_id: &str,
    seq: u64,
) -> Result<BatchOutcome> {
    let Some(rec) = app.db().get_broadcast_consistent(broadcast_id).await? else {
        warn!(broadcast_id, seq, "broadcast batch for a missing broadcast");
        return Ok(BatchOutcome::Missing);
    };
    if rec.draft {
        // Nothing queues a draft's batches; a job naming one is a bug.
        warn!(broadcast_id, seq, "broadcast batch for a draft; dropping");
        return Ok(BatchOutcome::Missing);
    }
    if rec.send_seq > seq {
        info!(
            broadcast_id,
            seq,
            send_seq = rec.send_seq,
            "stale broadcast batch; dropping"
        );
        return Ok(BatchOutcome::Stale);
    }

    let instance = app
        .db()
        .get_instances(&[rec.instance_id.as_str()])
        .await?
        .into_iter()
        .next()
        .flatten()
        .filter(|i| !i.deleted && i.kind == db::InstanceKind::Support);
    let Some(instance) = instance else {
        let failed = fail_pending(app, &rec, "the instance was deleted before this was sent").await;
        app.db()
            .set_broadcast_progress(broadcast_id, seq, None)
            .await?;
        return Ok(BatchOutcome::InstanceGone { failed });
    };

    let batch: Vec<usize> = pending_indices(&rec).take(broadcast::BATCH_SIZE).collect();
    let valid = broadcast::ValidBroadcast::from_record(&rec);

    // Checked again at send time, not just when the broadcast was queued: a
    // batch can run hours later, and an address that hard-bounced on some
    // other mail in between must not be mailed. Fails open, like every
    // other instance-mail send (see `crate::suppression`).
    let batch_emails: Vec<String> = batch
        .iter()
        .map(|&i| rec.recipients[i].email.clone())
        .collect();
    let suppressed: HashMap<String, String> =
        suppression::find_suppressed(app, &batch_emails, "broadcast batch")
            .await
            .into_iter()
            .map(|s| (s.email.clone(), suppression::describe(&s)))
            .collect();

    let (mut sent, mut failed, mut skipped) = (0, 0, 0);
    for index in batch.iter().copied() {
        if !app
            .db()
            .claim_broadcast_recipient(broadcast_id, index)
            .await?
        {
            // Another run of this job got there first.
            continue;
        }
        let email = &rec.recipients[index].email;
        let result = match suppressed.get(email) {
            Some(why) => db::BroadcastSendResult {
                status: db::BroadcastSendStatus::Suppressed,
                provider_message_id: None,
                error: Some(why.clone()),
            },
            None => send_one(app, &valid, &instance.signature, broadcast_id, index, email).await,
        };
        match result.status {
            db::BroadcastSendStatus::Sent => sent += 1,
            db::BroadcastSendStatus::Suppressed => skipped += 1,
            _ => failed += 1,
        }
        if let Err(e) = app
            .db()
            .set_broadcast_send_result(broadcast_id, index, &result)
            .await
        {
            // The mail is already out (or not); a lost status write only
            // leaves this recipient showing as `sending`.
            warn!(
                broadcast_id,
                index, "could not record broadcast send result: {e:#}"
            );
        }
    }

    // Recount from the row as it was read, less this batch: anything not in
    // it that was pending still is (only this chain sends).
    let remaining = pending_indices(&rec).filter(|i| !batch.contains(i)).count();
    let next_batch_at = if remaining > 0 {
        app.queue()
            .send_job(
                &batch_job(broadcast_id, seq + 1),
                broadcast::BATCH_INTERVAL_SECS,
            )
            .await
            .context("queueing the broadcast's next batch")?;
        Some(crate::clock::now_sec() + u64::from(broadcast::BATCH_INTERVAL_SECS))
    } else {
        None
    };
    if !app
        .db()
        .set_broadcast_progress(broadcast_id, seq, next_batch_at)
        .await?
    {
        info!(broadcast_id, seq, "a later batch already recorded progress");
    }
    info!(
        broadcast_id,
        seq,
        sent,
        failed,
        suppressed = skipped,
        remaining,
        "broadcast batch sent"
    );
    Ok(BatchOutcome::Sent {
        sent,
        failed,
        suppressed: skipped,
        remaining,
    })
}

fn pending_indices(rec: &db::Broadcast) -> impl Iterator<Item = usize> + '_ {
    rec.recipients
        .iter()
        .enumerate()
        .filter(|(_, r)| r.send_status == db::BroadcastSendStatus::Pending)
        .map(|(i, _)| i)
}

async fn send_one<A: HasMail + Sync>(
    app: &A,
    valid: &broadcast::ValidBroadcast,
    signature: &str,
    broadcast_id: &str,
    index: usize,
    email: &str,
) -> db::BroadcastSendResult {
    let sent = async {
        let msg = broadcast::build_broadcast_message(valid, signature, email)?;
        let tags = broadcast::send_tags(broadcast_id, index);
        app.mail()
            .send_raw_tagged(&msg.raw, &msg.to, &msg.cc, &tags)
            .await
    }
    .await;
    match sent {
        Ok(message_id) => db::BroadcastSendResult {
            status: db::BroadcastSendStatus::Sent,
            provider_message_id: Some(message_id),
            error: None,
        },
        Err(e) => {
            warn!(broadcast_id, index, "broadcast send failed: {e:#}");
            db::BroadcastSendResult {
                status: db::BroadcastSendStatus::Failed,
                provider_message_id: None,
                error: Some(format!("{e:#}")),
            }
        }
    }
}

/// Record every still-`pending` recipient of `rec` as `failed` with
/// `reason` — claimed first, so one a concurrent batch is sending is left
/// alone. Best-effort: a write failure is logged and skipped. Returns how
/// many were recorded.
pub async fn fail_pending<A: HasDb + Sync>(app: &A, rec: &db::Broadcast, reason: &str) -> usize {
    let result = db::BroadcastSendResult {
        status: db::BroadcastSendStatus::Failed,
        provider_message_id: None,
        error: Some(reason.to_string()),
    };
    let mut failed = 0;
    for index in pending_indices(rec) {
        let recorded = async {
            Ok::<bool, db::Error>(
                app.db().claim_broadcast_recipient(&rec.id, index).await?
                    && app
                        .db()
                        .set_broadcast_send_result(&rec.id, index, &result)
                        .await?,
            )
        }
        .await;
        match recorded {
            Ok(true) => failed += 1,
            Ok(false) => {}
            Err(e) => warn!(
                broadcast_id = rec.id,
                index, "could not fail recipient: {e:#}"
            ),
        }
    }
    failed
}

/// How long after the next batch starts the last of `pending` recipients
/// goes out: `ceil(pending / BATCH_SIZE) - 1` gaps of `BATCH_INTERVAL_SECS`.
pub fn estimated_duration_secs(pending: usize) -> u64 {
    let batches = pending.div_ceil(broadcast::BATCH_SIZE) as u64;
    batches.saturating_sub(1) * u64::from(broadcast::BATCH_INTERVAL_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_counts_the_gaps_between_batches() {
        assert_eq!(estimated_duration_secs(0), 0);
        assert_eq!(estimated_duration_secs(1), 0);
        assert_eq!(estimated_duration_secs(broadcast::BATCH_SIZE), 0);
        assert_eq!(
            estimated_duration_secs(broadcast::BATCH_SIZE + 1),
            u64::from(broadcast::BATCH_INTERVAL_SECS)
        );
        // 117 recipients at 5 every 5 minutes: 24 batches, 23 gaps.
        assert_eq!(estimated_duration_secs(117), 23 * 300);
    }

    #[test]
    fn the_first_job_is_seq_zero() {
        assert_eq!(
            batch_job("b1", 0),
            Job::BroadcastBatch {
                broadcast_id: "b1".to_string(),
                seq: 0
            }
        );
    }
}
