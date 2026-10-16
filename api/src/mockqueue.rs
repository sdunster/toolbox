//! In-memory [`crate::queue::Handler`]: jobs are recorded, never sent.
//!
//! Tests [`Queue::drain`] it and run each job themselves; `bin/poem-local`
//! drains it on a timer and runs jobs in-process (delays ignored), so local
//! dev sees a broadcast go out within seconds.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};

use crate::queue::{self, Job};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedJob {
    pub job: Job,
    pub delay_secs: u32,
}

#[derive(Default)]
pub struct Queue {
    jobs: Mutex<Vec<QueuedJob>>,
    failing: AtomicBool,
}

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every job queued so far, oldest first, leaving the queue empty.
    pub fn drain(&self) -> Vec<QueuedJob> {
        std::mem::take(&mut *self.jobs.lock().expect("mock queue lock"))
    }

    /// Make every later `send_job` fail (tests of the enqueue-failure path).
    pub fn set_failing(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }
}

impl queue::Handler for Queue {
    async fn send_job(&self, job: &Job, delay_secs: u32) -> Result<()> {
        if self.failing.load(Ordering::SeqCst) {
            bail!("mock queue is set to fail");
        }
        tracing::info!(?job, delay_secs, "mockqueue: job queued");
        self.jobs.lock().expect("mock queue lock").push(QueuedJob {
            job: job.clone(),
            delay_secs: delay_secs.min(queue::MAX_DELAY_SECS),
        });
        Ok(())
    }
}
