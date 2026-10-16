//! Dependency-injection container: which concrete backends a binary compiles in.
//!
//! Every backend is a type parameter, so each binary compiles exactly the
//! implementations it uses: `bin/poem.rs` gets DynamoDB + SES, `bin/poem-local.rs`
//! gets DynamoDB + the mock mailer, and `bin/lambda` gets DynamoDB + SES. There is
//! deliberately no runtime switch — a server that could be talked into mocking its
//! own email by an environment variable is a worse thing to deploy than two
//! binaries. A cargo feature was considered and rejected for the same reason:
//! `make check` runs `cargo clippy --all-targets --all-features`, so a mock-mail
//! feature would be silently compiled into every lint pass (and could just as
//! easily be turned on in a real build); a second binary can't be turned on by
//! accident.
//!
//! **The queue** (`HasQueue`, [`crate::queue`]) carries background jobs for
//! the worker Lambda — today, the paced batches of a broadcast. Both the API
//! (`sendBroadcast` queues a broadcast's first batch) and the worker (each
//! batch queues the next) produce to it; the worker consumes it, alongside the
//! inbound mail that arrives on the same SQS queue. See CLAUDE.md's "Worker
//! jobs" entry.

use crate::db;
use crate::mail;
use crate::queue;
use crate::storage;
use webauthn_rs::prelude::{Webauthn, WebauthnBuilder};

pub trait App {
    /// Artificial delay (in ms) injected before handling every request. Dev-only,
    /// set by `--response-lag-ms` on `bin/poem`/`bin/poem-local`; always `0` on the
    /// Lambda binary.
    fn response_lag(&self) -> u64;
}

/// Build the WebAuthn relying-party instance from environment configuration.
///
/// `WEBAUTHN_RP_ID` is the relying-party ID (defaults to `localhost`).
/// `WEBAUTHN_RP_ORIGIN` is a comma-separated list of allowed origins; the first is
/// the primary (defaults to `http://localhost:5173`). Multiple origins let a single
/// deployment serve more than one frontend origin.
pub fn build_webauthn() -> anyhow::Result<Webauthn> {
    let rp_id = std::env::var("WEBAUTHN_RP_ID").unwrap_or_else(|_| "localhost".to_string());
    let origins_raw =
        std::env::var("WEBAUTHN_RP_ORIGIN").unwrap_or_else(|_| "http://localhost:5173".to_string());
    let mut origins = origins_raw
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(url::Url::parse)
        .collect::<Result<Vec<_>, _>>()?;
    if origins.is_empty() {
        return Err(anyhow::anyhow!(
            "WEBAUTHN_RP_ORIGIN must contain at least one origin"
        ));
    }
    let primary = origins.remove(0);
    // rp_name is the "human-palatable" label a browser/OS shows in its passkey
    // save/use UI. Using the rp_id (the actual domain) here rather than a fixed
    // brand string means what's displayed always matches the domain the passkey
    // is actually scoped to.
    let mut builder = WebauthnBuilder::new(&rp_id, &primary)?.rp_name(&rp_id);
    for extra in &origins {
        builder = builder.append_allowed_origin(extra);
    }
    Ok(builder.build()?)
}

pub trait HasDb {
    fn db(&self) -> &impl db::Handler;
}

pub trait HasMail {
    fn mail(&self) -> &impl mail::Handler;
}

/// Object storage (the mail bucket: raw MIME, attachments, presigned
/// upload/download) — step 7's addition alongside `HasDb`/`HasMail`.
pub trait HasStorage {
    fn storage(&self) -> &impl storage::Handler;
}

/// The worker job queue — see [`crate::queue`].
pub trait HasQueue {
    fn queue(&self) -> &impl queue::Handler;
}

/// Struct holding our global singletons. See the module doc for why every backend
/// is a type parameter.
pub struct MyApp<DBH: db::Handler, M: mail::Handler, S: storage::Handler, Q: queue::Handler> {
    pub db: DBH,
    pub mail: M,
    pub storage: S,
    pub queue: Q,
    pub response_lag: u64,
}

pub fn new<DBH: db::Handler, M: mail::Handler, S: storage::Handler, Q: queue::Handler>(
    db: DBH,
    mail: M,
    storage: S,
    queue: Q,
    response_lag: u64,
) -> MyApp<DBH, M, S, Q> {
    MyApp {
        db,
        mail,
        storage,
        queue,
        response_lag,
    }
}

impl<DBH: db::Handler, M: mail::Handler, S: storage::Handler, Q: queue::Handler> App
    for MyApp<DBH, M, S, Q>
{
    fn response_lag(&self) -> u64 {
        self.response_lag
    }
}

impl<DBH: db::Handler, M: mail::Handler, S: storage::Handler, Q: queue::Handler> HasDb
    for MyApp<DBH, M, S, Q>
{
    fn db(&self) -> &impl db::Handler {
        &self.db
    }
}

impl<DBH: db::Handler, M: mail::Handler, S: storage::Handler, Q: queue::Handler> HasMail
    for MyApp<DBH, M, S, Q>
{
    fn mail(&self) -> &impl mail::Handler {
        &self.mail
    }
}

impl<DBH: db::Handler, M: mail::Handler, S: storage::Handler, Q: queue::Handler> HasStorage
    for MyApp<DBH, M, S, Q>
{
    fn storage(&self) -> &impl storage::Handler {
        &self.storage
    }
}

impl<DBH: db::Handler, M: mail::Handler, S: storage::Handler, Q: queue::Handler> HasQueue
    for MyApp<DBH, M, S, Q>
{
    fn queue(&self) -> &impl queue::Handler {
        &self.queue
    }
}
