//! The GraphQL API server, against real AWS: DynamoDB and SES.
//!
//! For an AWS-free local run, see `poem-local.rs` — a separate binary rather than a
//! flag on this one, so this server has no code path that can be talked into
//! mocking anything.

use std::error::Error;

use toolbox::dynamodb;
use toolbox::s3storage;
use toolbox::server;
use toolbox::sesmail;
use toolbox::sqsqueue;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let startup = server::init()?;
    let db = dynamodb::Handler::new(&startup.db_prefix, !startup.cli.enable_mutations).await;
    let mailer = sesmail::Mailer::new().await;
    let storage = s3storage::Storage::new().await?;
    // Jobs go to the real worker queue (`WORKER_QUEUE_URL`) and are run by
    // the deployed worker, never by this process — a dev server pointed at
    // a real prefix must not start sending broadcasts itself.
    let queue = sqsqueue::Queue::new().await;
    server::run(startup, db, mailer, storage, queue, |_| {}).await
}
