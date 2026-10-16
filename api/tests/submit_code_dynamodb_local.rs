//! **Critical security regression test** against DynamoDB Local: the public
//! submit-form's 6-digit code (`requestSubmitCode`/`verifySubmitCode`) and the
//! user login code (`requestAuthCode`/`verifyAuthCode`) must be stored and
//! checked completely independently.
//!
//! If the two ever shared storage (e.g. `requestSubmitCode` wrote to the same
//! `login_code` table `requestAuthCode` uses), an attacker could request a
//! submit code for their own address and present it to `verifyAuthCode` to
//! obtain a full user session — see `auth::SUBMIT_CODE_STATE_KIND`'s doc
//! comment. This file proves both directions are rejected, and that the
//! legitimate path on each side still works.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test submit_code_dynamodb_local
//! ```
//!
//! Skips itself (prints a message, returns) when no reachable local DynamoDB
//! is configured — see `tests/auth_dynamodb_local.rs`'s identically-named
//! helper for the full rationale.

use std::sync::Arc;

use async_graphql::{Request, Variables};
use serde_json::json;
use toolbox::app;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::graphql;
use toolbox::mockmail;
use toolbox::mockqueue;
use toolbox::mockstorage;

async fn local_db_prefix() -> Option<String> {
    let endpoint = toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;

    let client = toolbox::local_dev::dynamodb_client().await;
    if client.list_tables().send().await.is_err() {
        eprintln!(
            "submit_code_dynamodb_local: {endpoint} is configured but not reachable — skipping. \
             Run `make local-up && make local-tables` first."
        );
        return None;
    }

    Some(prefix)
}

macro_rules! require_local_db {
    () => {
        match local_db_prefix().await {
            Some(prefix) => prefix,
            None => {
                eprintln!(
                    "submit_code_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set to \
                     a reachable local DynamoDB — skipping. See this file's header for how to \
                     run it."
                );
                return;
            }
        }
    };
}

/// Lowercased deliberately: some tests below create a user directly via
/// `db.create_user` (which stores exactly what it's given) and then log in
/// through `requestAuthCode`/`verifyAuthCode`, which normalize the `email`
/// argument (`db::normalize_user_email`) — a mixed-case nanoid here would
/// only be asserting against that normalization.
fn unique_email(label: &str) -> String {
    format!("{label}-{}@toolbox.test", nanoid::nanoid!(10)).to_lowercase()
}

/// Pull a 6-digit code out of the mock mailer's most recent message to `to`
/// whose body contains `marker`, exactly as a real user would read one out of
/// their inbox — never by reaching into the stored hash.
fn extract_code(mail: &mockmail::Handler, to: &str, marker: &str) -> String {
    let sent = mail.sent();
    let email = sent
        .iter()
        .rev()
        .find(|e| e.to == to && e.body.contains(marker))
        .unwrap_or_else(|| {
            panic!("no email to {to} with marker {marker:?} was sent; sent: {sent:?}")
        });
    let start = email.body.find(marker).unwrap() + marker.len();
    let rest = &email.body[start..];
    let end = rest.find('\n').unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

fn expect_data(response: &async_graphql::Response, path: &str) -> serde_json::Value {
    assert!(
        response.errors.is_empty(),
        "unexpected GraphQL errors on {path}: {:?}",
        response.errors
    );
    let data = serde_json::to_value(&response.data).expect("response data serializes");
    let mut cur = &data;
    for segment in path.split('.') {
        cur = cur
            .get(segment)
            .unwrap_or_else(|| panic!("response data missing `{segment}` (path {path}): {data}"));
    }
    cur.clone()
}

const REQUEST_AUTH_CODE: &str = r#"
    mutation($email: String!) { requestAuthCode(email: $email) }
"#;
const VERIFY_AUTH_CODE: &str = r#"
    mutation($email: String!, $code: String!) { verifyAuthCode(email: $email, code: $code) }
"#;
const REQUEST_SUBMIT_CODE: &str = r#"
    mutation($slug: String!, $email: String!) { requestSubmitCode(slug: $slug, email: $email) }
"#;
const VERIFY_SUBMIT_CODE: &str = r#"
    mutation($slug: String!, $email: String!, $code: String!) {
        verifySubmitCode(slug: $slug, email: $email, code: $code)
    }
"#;

#[tokio::test]
async fn verify_auth_code_rejects_a_code_minted_by_request_submit_code() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let mail = mockmail::Handler::new();

    let email = unique_email("cross-a");
    db.create_user(&email, "Cross Test A")
        .await
        .expect("create_user");
    let instance = db
        .create_instance(
            "Cross Co",
            &format!("cross-a-{}", nanoid::nanoid!(8)),
            "Cross Co",
            "",
            true,
            toolbox::db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");

    let my_app = Arc::new(app::new(
        db,
        mail,
        mockstorage::Storage::new(),
        mockqueue::Queue::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(my_app.clone(), webauthn);

    // Get a real submit code via the legitimate public flow.
    let response = schema
        .execute(
            Request::new(REQUEST_SUBMIT_CODE).variables(Variables::from_json(json!({
                "slug": instance.slug,
                "email": email,
            }))),
        )
        .await;
    assert_eq!(expect_data(&response, "requestSubmitCode"), json!(true));
    let submit_code = extract_code(&my_app.mail, &email, "Your verification code is: ");

    // Presenting it to verifyAuthCode must fail — there is no login_code row
    // for this email at all (requestAuthCode was never called), so this also
    // proves requestSubmitCode did not write one.
    let response = schema
        .execute(
            Request::new(VERIFY_AUTH_CODE).variables(Variables::from_json(json!({
                "email": email,
                "code": submit_code,
            }))),
        )
        .await;
    assert_eq!(
        expect_data(&response, "verifyAuthCode"),
        json!(null),
        "verifyAuthCode must never accept a submit code"
    );

    // The legitimate use of that same code must still work, proving the code
    // itself was valid and the rejection above was about the *table*, not a
    // broken code.
    let response = schema
        .execute(
            Request::new(VERIFY_SUBMIT_CODE).variables(Variables::from_json(json!({
                "slug": instance.slug,
                "email": email,
                "code": submit_code,
            }))),
        )
        .await;
    let token = expect_data(&response, "verifySubmitCode");
    let token = token.as_str().expect("verifySubmitCode returns a token");
    assert!(
        token.starts_with(toolbox::auth::REQUESTER_TOKEN_PREFIX),
        "expected an mts_ submit token, got {token}"
    );
}

#[tokio::test]
async fn verify_submit_code_rejects_a_code_minted_by_request_auth_code() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let mail = mockmail::Handler::new();

    let email = unique_email("cross-b");
    db.create_user(&email, "Cross Test B")
        .await
        .expect("create_user");
    let instance = db
        .create_instance(
            "Cross Co 2",
            &format!("cross-b-{}", nanoid::nanoid!(8)),
            "Cross Co 2",
            "",
            true,
            toolbox::db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");

    let my_app = Arc::new(app::new(
        db,
        mail,
        mockstorage::Storage::new(),
        mockqueue::Queue::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(my_app.clone(), webauthn);

    // Get a real login code via the legitimate user flow.
    let response = schema
        .execute(
            Request::new(REQUEST_AUTH_CODE)
                .variables(Variables::from_json(json!({ "email": email }))),
        )
        .await;
    assert_eq!(expect_data(&response, "requestAuthCode"), json!(true));
    let login_code = extract_code(&my_app.mail, &email, "Your login code is: ");

    // Presenting it to verifySubmitCode must fail — there is no submit_code
    // ephemeral_state row for this (instance, email) pair at all
    // (requestSubmitCode was never called), so this also proves
    // requestAuthCode did not write one.
    let response = schema
        .execute(
            Request::new(VERIFY_SUBMIT_CODE).variables(Variables::from_json(json!({
                "slug": instance.slug,
                "email": email,
                "code": login_code,
            }))),
        )
        .await;
    assert_eq!(
        expect_data(&response, "verifySubmitCode"),
        json!(null),
        "verifySubmitCode must never accept a login code"
    );

    // The legitimate use of that same code must still work.
    let response = schema
        .execute(
            Request::new(VERIFY_AUTH_CODE).variables(Variables::from_json(json!({
                "email": email,
                "code": login_code,
            }))),
        )
        .await;
    let token = expect_data(&response, "verifyAuthCode");
    let token = token.as_str().expect("verifyAuthCode returns a token");
    assert!(
        token.starts_with(toolbox::auth::USER_TOKEN_PREFIX),
        "expected an mtu_ user token, got {token}"
    );
}

#[tokio::test]
async fn request_submit_code_is_a_noop_for_an_instance_without_public_submission() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let mail = mockmail::Handler::new();

    let email = unique_email("private");
    let instance = db
        .create_instance(
            "Private Co",
            &format!("private-{}", nanoid::nanoid!(8)),
            "Private Co",
            "",
            /* public_submission_enabled */ false,
            toolbox::db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");

    let my_app = Arc::new(app::new(
        db,
        mail,
        mockstorage::Storage::new(),
        mockqueue::Queue::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(my_app.clone(), webauthn);

    let response = schema
        .execute(
            Request::new(REQUEST_SUBMIT_CODE).variables(Variables::from_json(json!({
                "slug": instance.slug,
                "email": email,
            }))),
        )
        .await;
    // Still true — anti-enumeration.
    assert_eq!(expect_data(&response, "requestSubmitCode"), json!(true));
    assert!(
        my_app.mail.sent().iter().all(|e| e.to != email),
        "no code should be sent for an instance without public submission enabled"
    );
}
