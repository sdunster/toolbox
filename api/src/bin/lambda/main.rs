//! The GraphQL API Lambda (`api-lambda`), served over a Function URL.
//!
//! Schema and app are built once here, outside the handler closure, so a warm
//! Lambda instance reuses them across invocations instead of rebuilding the schema
//! (and re-resolving WebAuthn config) on every request.

mod errors;
mod handler;

use lambda_http::{Error, run, service_fn, tracing};
use std::env;
use std::sync::Arc;
use toolbox::app;
use toolbox::dynamodb;
use toolbox::graphql;
use toolbox::s3storage;
use toolbox::sesmail;
use toolbox::sqsqueue;

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();

    // Toolbox has no JWTs — every credential is an opaque secret whose
    // sha256 hash is looked up in DynamoDB, so there is no signing secret to
    // read from SSM at cold start the way seslogin reads a JWT secret.
    toolbox::turnstile::log_startup_state();

    let mailer = sesmail::Mailer::new().await;
    let storage = s3storage::Storage::new()
        .await
        .expect("MAIL_BUCKET must be set for the API lambda's attachment upload/download");

    let read_only = env::var("READ_ONLY")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    let db_prefix = env::var("DB_PREFIX").expect("DB_PREFIX must be set for dynamodb backend");
    let db = dynamodb::Handler::new(&db_prefix, read_only).await;
    // `sendBroadcast` queues a broadcast's first batch for the worker.
    let queue = sqsqueue::Queue::new().await;
    let app = Arc::new(app::new(db, mailer, storage, queue, 0));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(app.clone(), webauthn);
    let handler = handler::Handler::new(app, schema);
    run(service_fn(|req| handler.handle_request(req))).await
}
