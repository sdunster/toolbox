//! Background jobs for the worker Lambda — the one queue abstraction.
//!
//! The API (and the worker itself) *produce* [`Job`]s onto the worker SQS
//! queue (`infra/sqs.tf`); `bin/worker` consumes them alongside inbound mail,
//! which arrives on the same queue as SNS envelopes. A job body is plain JSON
//! tagged by a `job` field, which an SNS envelope never carries, so
//! [`Job::parse`] tells the two apart. See CLAUDE.md's "Worker jobs" entry.
//!
//! Implementations: `sqsqueue::Queue` (the real queue, `WORKER_QUEUE_URL`)
//! and `mockqueue::Queue` (in memory, for tests and `poem-local`).

use std::future::Future;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Longest delay SQS allows on a single message (15 minutes).
pub const MAX_DELAY_SECS: u32 = 900;

/// One unit of background work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum Job {
    /// Send the next batch of a broadcast's pending recipients
    /// (`broadcast_send::run_batch`). `seq` is the batch's position in the
    /// broadcast's chain — see `db::Broadcast::send_seq`.
    BroadcastBatch { broadcast_id: String, seq: u64 },
}

impl Job {
    pub fn to_body(&self) -> String {
        serde_json::to_string(self).expect("a Job always serializes")
    }

    /// `Ok(None)` when `body` is not a job at all (no top-level `job` field —
    /// e.g. the SNS envelope inbound mail arrives in); an error when it claims
    /// to be a job but isn't one this build knows, so it is retried and then
    /// dead-lettered rather than silently dropped.
    pub fn parse(body: &str) -> Result<Option<Job>> {
        let value: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if value.get("job").is_none() {
            return Ok(None);
        }
        serde_json::from_value(value)
            .map(Some)
            .context("parsing a worker job")
    }
}

pub trait Handler: Sync {
    /// Enqueue `job`, invisible to the worker for `delay_secs` (at most
    /// [`MAX_DELAY_SECS`]; larger values are clamped).
    fn send_job(&self, job: &Job, delay_secs: u32) -> impl Future<Output = Result<()>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_round_trips_through_its_body() {
        let job = Job::BroadcastBatch {
            broadcast_id: "b1".to_string(),
            seq: 3,
        };
        assert_eq!(
            job.to_body(),
            r#"{"job":"broadcast_batch","broadcast_id":"b1","seq":3}"#
        );
        assert_eq!(Job::parse(&job.to_body()).unwrap(), Some(job));
    }

    #[test]
    fn an_sns_envelope_is_not_a_job() {
        let envelope = r#"{"Type":"Notification","MessageId":"m","Message":"{}"}"#;
        assert_eq!(Job::parse(envelope).unwrap(), None);
        assert_eq!(Job::parse("not json").unwrap(), None);
    }

    #[test]
    fn an_unknown_job_is_an_error_not_a_skip() {
        assert!(Job::parse(r#"{"job":"something_new"}"#).is_err());
        assert!(Job::parse(r#"{"job":"broadcast_batch"}"#).is_err());
    }
}
