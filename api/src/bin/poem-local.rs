//! The GraphQL API server with email mocked out: mail is logged rather than sent.
//! Started by `make dev-local`; see DEVELOPMENT.md, "Running without AWS".
//!
//! Deliberately a separate binary from `poem.rs`, not a flag or a cargo feature on
//! it. A feature would be switched on by `--all-features`, which `make check`
//! passes to clippy — the real SES path would then stop being linted, and any
//! `--all-features` build would quietly produce a mocked server.
//!
//! DynamoDB is *not* mocked: `DB_PREFIX` and `AWS_ENDPOINT_URL_DYNAMODB` still
//! decide which database this talks to, so it can be pointed at DynamoDB Local or
//! at a real table. That combination — a real database that provably cannot send
//! email — is the point.

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use toolbox::app::MyApp;
use toolbox::dynamodb;
use toolbox::mockmail;
use toolbox::mockqueue;
use toolbox::mockstorage;
use toolbox::server;
use toolbox::worker;

type LocalApp = MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage, mockqueue::Queue>;

/// How often queued worker jobs are run in-process. Their SQS delay is
/// ignored, so a broadcast's batches go out seconds apart, not minutes.
const JOB_POLL: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let startup = server::init()?;
    tracing::warn!("poem-local: SES is mocked. Email will be logged instead of sent.");
    let db = dynamodb::Handler::new(&startup.db_prefix, !startup.cli.enable_mutations).await;
    let run_jobs = startup.cli.enable_mutations;
    server::run(
        startup,
        db,
        mockmail::Handler::from_env(),
        mockstorage::Storage::from_env(),
        mockqueue::Queue::new(),
        move |app| {
            if run_jobs {
                tokio::spawn(run_queued_jobs(app));
            }
        },
    )
    .await
}

/// Stand-in for the deployed worker: drain the mock queue and run each job
/// through the same [`worker::run_job`] the worker Lambda uses.
async fn run_queued_jobs(app: Arc<LocalApp>) {
    loop {
        tokio::time::sleep(JOB_POLL).await;
        for queued in app.queue.drain() {
            if let Err(e) = worker::run_job(&*app, &queued.job).await {
                tracing::error!(job = ?queued.job, "worker job failed: {e:#}");
            }
        }
    }
}
