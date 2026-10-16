//! Integration test for the real email-code login flow against **DynamoDB
//! Local** — not the mocks. Exercises `auth::verify_token`/`verify_authorization_header`
//! and the `requestAuthCode` / `verifyAuthCode` / `me` / `logout` GraphQL
//! mutations wired through a real `dynamodb::Handler`, end to end:
//!
//!   create a user → requestAuthCode → verifyAuthCode → authenticated `me` →
//!   `logout` → the revoked token is rejected.
//!
//! # Running this test
//!
//! Needs a local DynamoDB (DynamoDB Local, not real AWS) with the schema
//! already created:
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test auth_dynamodb_local
//! ```
//!
//! `local/local.env` sets `AWS_ENDPOINT_URL_DYNAMODB=http://localhost:8100` and
//! `DB_PREFIX=local`, matching what `make local-tables` creates. Without that
//! endpoint configured and reachable, every test in this file **skips itself**
//! (prints a message and returns) rather than failing — so `cargo test` (and
//! CI, which never brings up DynamoDB Local for the `api-check` job) stays
//! green with no local stack running. Mail is mocked (`mockmail`), not real
//! SES, since this test only needs to read back the login code that was
//! "sent" — see `extract_login_code`.
//!
//! Uses the real `local` prefix's tables directly rather than standing up a
//! throwaway prefix — DynamoDB Local has no cost concern, and every row this
//! test creates uses a nanoid'd unique email, so repeated runs never collide
//! with each other or with anything `make local-seed` writes later. Rows are
//! left behind (no `delete_user`/`delete_webauthn_credential`-by-user exists
//! yet) — harmless in a throwaway local database.

use std::sync::Arc;

use async_graphql::{Request, Variables};
use serde_json::json;
use toolbox::app;
use toolbox::app::HasDb as _;
use toolbox::auth;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::graphql;
use toolbox::mockmail;
use toolbox::mockqueue;
use toolbox::mockstorage;

/// `Some(prefix)` when a local DynamoDB is configured *and* actually
/// reachable; `None` means every test below should skip itself. Checked fresh
/// per test (rather than once, cached) since there's no shared test harness
/// setup hook here — `#[tokio::test]` functions are independent.
async fn local_db_prefix() -> Option<String> {
    let endpoint = toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;

    // A configured-but-unreachable endpoint (DynamoDB Local not actually
    // running) must skip too, not fail with a connection error — probe with a
    // cheap, harmless call.
    let client = toolbox::local_dev::dynamodb_client().await;
    if client.list_tables().send().await.is_err() {
        eprintln!(
            "auth_dynamodb_local: {endpoint} is configured but not reachable — skipping. \
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
                    "auth_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set to a \
                     reachable local DynamoDB — skipping. See this file's header for how to run it."
                );
                return;
            }
        }
    };
}

/// Pull the 6-digit code back out of the mock mailer's recorded outbox. Mirrors
/// exactly what a real user would read out of their inbox — this test never
/// reaches into `login_code`'s stored hash.
fn extract_login_code(mail: &mockmail::Handler, to: &str) -> String {
    let sent = mail.sent();
    let email = sent
        .iter()
        .rev()
        .find(|e| e.to == to)
        .unwrap_or_else(|| panic!("no login-code email was sent to {to}; sent: {sent:?}"));
    let marker = "Your login code is: ";
    let start = email
        .body
        .find(marker)
        .unwrap_or_else(|| panic!("login-code email body had no code marker: {}", email.body))
        + marker.len();
    let rest = &email.body[start..];
    let end = rest.find('\n').unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

/// Lowercased deliberately: `requestAuthCode`/`verifyAuthCode` normalize the
/// `email` argument (`db::normalize_user_email`), so a mixed-case nanoid
/// here would only be asserting against that normalization, since the user
/// below is created directly via `db.create_user` (which stores exactly
/// what it's given) and then looked up through the normalized GraphQL path.
fn unique_email() -> String {
    format!("integration-{}@toolbox.test", nanoid::nanoid!(10)).to_lowercase()
}

/// Read `data.<path>` out of a GraphQL response as JSON, panicking with the
/// response's errors (if any) on failure — every assertion helper below wants
/// "give me the value or tell me why it's missing", not a raw `Response`.
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

#[tokio::test]
async fn email_code_login_flow_end_to_end() {
    let prefix = require_local_db!();

    let db = dynamodb::Handler::new(&prefix, /* read_only */ false).await;
    let mail = mockmail::Handler::new();

    let email = unique_email();
    let user = db
        .create_user(&email, "Integration Test User")
        .await
        .expect("create_user against DynamoDB Local");
    assert!(user.enabled);
    assert_eq!(user.email, email);

    let my_app = Arc::new(app::new(
        db,
        mail,
        mockstorage::Storage::new(),
        mockqueue::Queue::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(my_app.clone(), webauthn);

    // ── requestAuthCode ──────────────────────────────────────────────────
    let request_code_query = r#"
        mutation($email: String!) {
            requestAuthCode(email: $email)
        }
    "#;
    let response = schema
        .execute(
            Request::new(request_code_query)
                .variables(Variables::from_json(json!({ "email": email }))),
        )
        .await;
    assert_eq!(expect_data(&response, "requestAuthCode"), json!(true));

    let code = extract_login_code(&my_app.mail, &email);
    assert_eq!(code.len(), 6, "login code should be 6 digits: {code:?}");
    assert!(code.chars().all(|c| c.is_ascii_digit()));

    // ── verifyAuthCode ───────────────────────────────────────────────────
    let verify_code_query = r#"
        mutation($email: String!, $code: String!) {
            verifyAuthCode(email: $email, code: $code)
        }
    "#;
    let response = schema
        .execute(
            Request::new(verify_code_query).variables(Variables::from_json(
                json!({ "email": email, "code": code }),
            )),
        )
        .await;
    let token_value = expect_data(&response, "verifyAuthCode");
    let token = token_value
        .as_str()
        .unwrap_or_else(|| panic!("verifyAuthCode returned non-string/null: {token_value}"))
        .to_string();
    assert!(
        token.starts_with(auth::USER_TOKEN_PREFIX),
        "token should carry the mtu_ prefix: {token}"
    );

    // A wrong code, on a *fresh* request (the real code is already consumed —
    // verifyAuthCode deletes it on success), must fail closed rather than
    // panic on a missing row.
    let response = schema
        .execute(
            Request::new(verify_code_query).variables(Variables::from_json(json!({
                "email": email,
                "code": "000000",
            }))),
        )
        .await;
    assert_eq!(expect_data(&response, "verifyAuthCode"), json!(null));

    // ── authenticated `me` ───────────────────────────────────────────────
    let auth_info = auth::verify_authorization_header(&*my_app, Some(&format!("Bearer {token}")))
        .await
        .expect("Bearer header must be recognized")
        .expect("token must verify");

    let me_query = "{ me { id email name enabled } }";
    let response = schema.execute(Request::new(me_query).data(auth_info)).await;
    assert_eq!(expect_data(&response, "me.email"), json!(email));
    assert_eq!(expect_data(&response, "me.id"), json!(user.id));
    assert_eq!(expect_data(&response, "me.enabled"), json!(true));

    // A request with no credentials at all must still be rejected — this
    // isn't "logged in as nobody caches to logged in as this user".
    let response = schema.execute(Request::new(me_query)).await;
    assert!(!response.errors.is_empty());

    // ── logout ───────────────────────────────────────────────────────────
    let auth_info = auth::verify_authorization_header(&*my_app, Some(&format!("Bearer {token}")))
        .await
        .expect("Bearer header must be recognized")
        .expect("token must still verify before logout");
    let logout_query = "mutation { logout }";
    let response = schema
        .execute(Request::new(logout_query).data(auth_info))
        .await;
    assert_eq!(expect_data(&response, "logout"), json!(true));

    // ── the revoked token is rejected ───────────────────────────────────
    let result = auth::verify_token(&*my_app, &token).await;
    assert!(
        matches!(result, Err(auth::AuthError::Permanent(_))),
        "a logged-out token must fail permanently, not just error transiently"
    );
}

#[tokio::test]
async fn disabled_user_cannot_log_in() {
    let prefix = require_local_db!();

    let db = dynamodb::Handler::new(&prefix, false).await;
    let mail = mockmail::Handler::new();

    let email = unique_email();
    let user = db
        .create_user(&email, "Disabled Integration User")
        .await
        .expect("create_user against DynamoDB Local");
    db.update_user(
        &user.id,
        toolbox::db::UserUpdateShape::Fields {
            name: "Disabled Integration User",
            enabled: false,
        },
    )
    .await
    .expect("update_user against DynamoDB Local");

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
            Request::new(r#"mutation($email: String!) { requestAuthCode(email: $email) }"#)
                .variables(Variables::from_json(json!({ "email": email }))),
        )
        .await;
    // Still true — a disabled user's requestAuthCode is indistinguishable
    // from an unknown one, by design (anti-enumeration).
    assert_eq!(expect_data(&response, "requestAuthCode"), json!(true));

    // But no email actually went out, and no code was ever stored, so there
    // is nothing a disabled user could verify their way past.
    assert!(
        my_app.mail.sent().iter().all(|e| e.to != email),
        "a disabled user must never receive a login code"
    );
    assert!(
        my_app
            .db()
            .get_login_code(&email)
            .await
            .expect("get_login_code against DynamoDB Local")
            .is_none(),
        "no login_code row should exist for a disabled user"
    );
}

/// Login is case-insensitive end to end: `requestAuthCode` with a
/// mixed-case, space-padded form of a user's (lowercase-stored) email
/// stores the login code under the normalized key, and `verifyAuthCode`
/// with a *different* casing of that same address and the right code still
/// succeeds — see `db::normalize_user_email`.
#[tokio::test]
async fn login_is_case_insensitive() {
    let prefix = require_local_db!();

    let db = dynamodb::Handler::new(&prefix, false).await;
    let mail = mockmail::Handler::new();

    let stored_email = unique_email();
    assert_eq!(
        stored_email,
        stored_email.trim().to_lowercase(),
        "unique_email() fixture must already be normalized"
    );
    db.create_user(&stored_email, "Case Insensitive User")
        .await
        .expect("create_user against DynamoDB Local");

    let my_app = Arc::new(app::new(
        db,
        mail,
        mockstorage::Storage::new(),
        mockqueue::Queue::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(my_app.clone(), webauthn);

    // requestAuthCode with a mixed-case, space-padded variant of the stored
    // email.
    let request_variant = format!("  {} ", stored_email.to_uppercase());
    let response = schema
        .execute(
            Request::new(r#"mutation($email: String!) { requestAuthCode(email: $email) }"#)
                .variables(Variables::from_json(json!({ "email": request_variant }))),
        )
        .await;
    assert_eq!(expect_data(&response, "requestAuthCode"), json!(true));

    // The login code was actually sent (to the real, normalized address) and
    // stored under the normalized `login_code` key — not under the
    // mixed-case/padded form the caller sent.
    let code = extract_login_code(&my_app.mail, &stored_email);
    assert!(
        my_app
            .db()
            .get_login_code(&stored_email)
            .await
            .expect("get_login_code against DynamoDB Local")
            .is_some(),
        "login_code must be stored under the normalized (lowercase) email"
    );

    // verifyAuthCode with yet another form of the same address (uppercase,
    // no surrounding whitespace this time — distinct from the padded form
    // requestAuthCode was called with) and the right code must still
    // succeed.
    let verify_variant = stored_email.to_uppercase();
    let response = schema
        .execute(
            Request::new(
                r#"mutation($email: String!, $code: String!) { verifyAuthCode(email: $email, code: $code) }"#,
            )
            .variables(Variables::from_json(json!({
                "email": verify_variant,
                "code": code,
            }))),
        )
        .await;
    let token = expect_data(&response, "verifyAuthCode");
    assert!(
        token
            .as_str()
            .is_some_and(|t| t.starts_with(auth::USER_TOKEN_PREFIX)),
        "verifyAuthCode with a differently-cased email must still succeed: {token:?}"
    );
}
