pub mod app;
pub mod auth;
pub mod base_url;
pub mod broadcast;
pub mod broadcast_events;
pub mod clock;
pub mod db;
pub mod dynamodb;
pub mod environment;
pub mod expire;
pub mod graphql;
pub mod inbound;
pub mod invoicing;
pub mod local_dev;
pub mod mail;
pub mod mailloop;
pub mod mcp;
pub mod mockdb;
pub mod mockmail;
pub mod mockstorage;
pub mod nonce;
pub mod oauth;
pub mod oauth_http;
pub mod outbound;
pub mod request_metrics;
pub mod s3storage;
pub mod server;
pub mod sesmail;
pub mod staff_notify;
pub mod storage;
pub mod suppression;
pub mod telemetry;
pub mod turnstile;

/// Load local `.env`/`.env.secret` for CLI / dev binaries. The Lambda binaries
/// don't call this. AWS profile defaulting is handled natively by
/// [`aws_config_loader`], not by mutating the environment.
pub fn load_cli_env() {
    dotenvy::from_filename(".env").ok();
    dotenvy::from_filename(".env.secret").ok();
}

/// AWS SDK config loader with the project's default profile fallback applied.
/// Reads `AWS_PROFILE` (and any credentials already present in the environment)
/// first, so an explicit `AWS_PROFILE`, the Lambda execution role, and CI's OIDC
/// credentials all take precedence over anything this picks as a fallback. When
/// neither is set, no profile is forced — unlike seslogin, this project has no
/// single hardcoded SSO profile name to fall back to; callers relying on a
/// profile locally should set `AWS_PROFILE` themselves. Callers `.load().await`,
/// optionally adding region/credential overrides first.
pub fn aws_config_loader() -> aws_config::ConfigLoader {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
}
