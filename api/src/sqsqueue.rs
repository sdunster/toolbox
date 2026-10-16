//! [`crate::queue::Handler`] backed by the worker SQS queue.
//!
//! The queue URL comes from `WORKER_QUEUE_URL` (set on both Lambdas by
//! Terraform). It is read once at construction but only *required* when a
//! job is sent: a dev server (`bin/poem`) without it still starts, and only
//! `sendBroadcast` fails, with a message naming the variable.

use anyhow::{Result, anyhow};
use aws_sdk_sqs::Client;

use crate::queue::{self, Job};

pub const QUEUE_URL_VAR: &str = "WORKER_QUEUE_URL";

pub struct Queue {
    client: Client,
    url: Option<String>,
}

impl Queue {
    pub async fn new() -> Self {
        let config = crate::aws_config_loader().load().await;
        let url = std::env::var(QUEUE_URL_VAR)
            .ok()
            .filter(|u| !u.trim().is_empty());
        if url.is_none() {
            tracing::warn!("{QUEUE_URL_VAR} is not set; background jobs cannot be queued");
        }
        Self {
            client: Client::new(&config),
            url,
        }
    }
}

impl queue::Handler for Queue {
    async fn send_job(&self, job: &Job, delay_secs: u32) -> Result<()> {
        let url = self
            .url
            .as_deref()
            .ok_or_else(|| anyhow!("{QUEUE_URL_VAR} is not set, so the job can't be queued"))?;
        self.client
            .send_message()
            .queue_url(url)
            .message_body(job.to_body())
            .delay_seconds(delay_secs.min(queue::MAX_DELAY_SECS) as i32)
            .send()
            .await
            .map_err(|e| {
                anyhow!(
                    "SQS SendMessage failed: {}",
                    aws_sdk_sqs::error::DisplayErrorContext(e)
                )
            })?;
        Ok(())
    }
}
