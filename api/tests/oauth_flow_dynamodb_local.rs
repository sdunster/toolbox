//! End-to-end tests for the OAuth authorization server against **DynamoDB
//! Local** — not the mocks: dynamic client registration → the consent step
//! (`oauthAuthorizationRequest`/`approveOauthAuthorization`, through a real
//! schema) → the token endpoint's `authorization_code` and `refresh_token`
//! grants → `oauth::verify_access_token`. Covers:
//!
//!   - the full happy path, including that the issued access token resolves to
//!     the approving user and that the site's own `verify_token` rejects it;
//!   - a code is single use (a replay fails even with a correct verifier), and a
//!     wrong PKCE verifier / redirect_uri / client_id fails;
//!   - refresh rotation, and OAuth 2.1 reuse detection: presenting an
//!     already-rotated refresh token revokes the whole grant;
//!   - a disabled user can't refresh;
//!   - `approveOauthAuthorization` refuses an unregistered redirect, a non-S256
//!     method, an unsupported scope and a `Requester` principal.
//!
//! The client-id signing key comes from the process environment, so every test
//! serializes on one `tokio::sync::Mutex` held across all `.await`s (see
//! CLAUDE.md's house rule on env-touching tests).
//!
//! # Running this test
//!
//! ```sh
//! make local-up && make local-tables
//! cd api && set -a && . ../local/local.env && set +a
//! cargo test --test oauth_flow_dynamodb_local
//! ```
//!
//! Every test **skips itself** when no reachable local DynamoDB is configured.

use std::sync::Arc;

use async_graphql::{Request, Variables};
use base64::Engine;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, MutexGuard};
use toolbox::app;
use toolbox::auth::{self, AuthError, AuthInfo};
use toolbox::db;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::graphql;
use toolbox::mockmail;
use toolbox::mockqueue;
use toolbox::mockstorage;
use toolbox::oauth;
use toolbox::oauth_http;

type TestApp =
    app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage, mockqueue::Queue>;
type TestSchema = graphql::ToolboxSchema<TestApp>;

static ENV_LOCK: Mutex<()> = Mutex::const_new(());

const TEST_SECRET: &str = "oauth-flow-test-secret";
const REDIRECT: &str = "https://client.example/callback";
const RESOURCE: &str = "https://toolbox.test/mcp";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

async fn local_db_prefix() -> Option<String> {
    toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;
    let client = toolbox::local_dev::dynamodb_client().await;
    client.list_tables().send().await.ok()?;
    Some(prefix)
}

macro_rules! require_local_db {
    () => {
        match local_db_prefix().await {
            Some(prefix) => prefix,
            None => {
                eprintln!("oauth_flow_dynamodb_local: no reachable local DynamoDB — skipping.");
                return;
            }
        }
    };
}

/// Holds the env lock and the signing secret for the life of one test.
struct Env(#[allow(dead_code)] MutexGuard<'static, ()>);

async fn env() -> Env {
    let guard = ENV_LOCK.lock().await;
    // SAFETY: every test in this file holds `ENV_LOCK` across its whole body.
    unsafe { std::env::set_var(oauth::CLIENT_ID_SECRET_ENV, TEST_SECRET) };
    Env(guard)
}

fn challenge() -> String {
    BASE64_URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER.as_bytes()))
}

struct Fixture {
    app: Arc<TestApp>,
    schema: TestSchema,
    user_id: String,
    client_id: String,
}

async fn fixture(prefix: &str) -> Fixture {
    let db = dynamodb::Handler::new(prefix, false).await;
    let email = format!("oauth-{}@example.com", nanoid::nanoid!(8)).to_lowercase();
    let user_id = db.create_user(&email, "OAuth Tester").await.unwrap().id;
    let app = Arc::new(app::new(
        db,
        mockmail::Handler::new(),
        mockstorage::Storage::new(),
        mockqueue::Queue::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(app.clone(), webauthn);

    let key = oauth::client_id_key_from_env();
    let reply = oauth_http::register(
        key.as_ref(),
        json!({"client_name": "Test Client", "redirect_uris": [REDIRECT]})
            .to_string()
            .as_bytes(),
    );
    assert_eq!(reply.status, 201, "{}", reply.body);
    let client_id = serde_json::from_str::<Value>(&reply.body).unwrap()["client_id"]
        .as_str()
        .unwrap()
        .to_string();
    Fixture {
        app,
        schema,
        user_id,
        client_id,
    }
}

fn user_auth(user_id: &str) -> AuthInfo {
    AuthInfo::User {
        id: user_id.to_string(),
        memberships: vec![],
        is_superuser: false,
        token_id: None,
        grant_id: None,
    }
}

const APPROVE: &str = r#"
    mutation($clientId: String!, $redirectUri: String!, $challenge: String!,
             $method: String!, $scope: String, $resource: String, $state: String) {
        approveOauthAuthorization(clientId: $clientId, redirectUri: $redirectUri,
            codeChallenge: $challenge, codeChallengeMethod: $method,
            scope: $scope, resource: $resource, state: $state)
    }
"#;

/// Run `approveOauthAuthorization` as `auth`; returns `(data, first error message)`.
async fn approve_with(
    f: &Fixture,
    auth: AuthInfo,
    redirect: &str,
    method: &str,
    scope: Option<&str>,
) -> (Option<String>, Option<String>) {
    let response = f
        .schema
        .execute(
            Request::new(APPROVE)
                .variables(Variables::from_json(json!({
                    "clientId": f.client_id, "redirectUri": redirect,
                    "challenge": challenge(), "method": method,
                    "scope": scope, "resource": RESOURCE, "state": "xyz",
                })))
                .data(auth),
        )
        .await;
    let data = serde_json::to_value(&response.data).unwrap();
    (
        data["approveOauthAuthorization"]
            .as_str()
            .map(str::to_string),
        response.errors.first().map(|e| e.message.clone()),
    )
}

/// Approve as the fixture's user and pull the `code` out of the redirect URL.
async fn approved_code(f: &Fixture) -> String {
    let (url, err) = approve_with(f, user_auth(&f.user_id), REDIRECT, "S256", None).await;
    let url = url::Url::parse(&url.unwrap_or_else(|| panic!("approve failed: {err:?}"))).unwrap();
    assert!(url.as_str().starts_with(REDIRECT));
    let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(pairs.get("state").map(String::as_str), Some("xyz"));
    pairs["code"].clone()
}

async fn token(f: &Fixture, form: &[(&str, &str)]) -> (u16, Value) {
    let body = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let key = oauth::client_id_key_from_env();
    let reply = oauth_http::token(&*f.app, key.as_ref(), None, body.as_bytes()).await;
    (reply.status, serde_json::from_str(&reply.body).unwrap())
}

async fn exchange(f: &Fixture, code: &str) -> (u16, Value) {
    token(
        f,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", &f.client_id),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
            ("resource", RESOURCE),
        ],
    )
    .await
}

async fn refresh(f: &Fixture, refresh_token: &str) -> (u16, Value) {
    token(
        f,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &f.client_id),
        ],
    )
    .await
}

#[tokio::test]
async fn full_flow_issues_tokens_that_resolve_to_the_approving_user() {
    let prefix = require_local_db!();
    let _env = env().await;
    let f = fixture(&prefix).await;

    let (status, body) = exchange(&f, &approved_code(&f).await).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["token_type"], "Bearer");
    let access = body["access_token"].as_str().unwrap();

    match oauth::verify_access_token(&*f.app, access, RESOURCE).await {
        Ok(AuthInfo::User { id, grant_id, .. }) => {
            assert_eq!(id, f.user_id);
            assert!(grant_id.is_some());
        }
        other => panic!("expected a User, got ok={}", other.is_ok()),
    }
    // The site's own auth path must not accept it.
    let via_graphql = auth::verify_token(&*f.app, access).await;
    assert!(matches!(via_graphql, Err(AuthError::Permanent(_))));
}

#[tokio::test]
async fn a_code_is_single_use_even_with_the_right_verifier() {
    let prefix = require_local_db!();
    let _env = env().await;
    let f = fixture(&prefix).await;
    let code = approved_code(&f).await;

    assert_eq!(exchange(&f, &code).await.0, 200);
    let (status, body) = exchange(&f, &code).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"], "invalid_grant");
}

#[tokio::test]
async fn pkce_redirect_and_client_mismatches_are_rejected() {
    let prefix = require_local_db!();
    let _env = env().await;
    let f = fixture(&prefix).await;

    // Each attempt burns its code, so mint a fresh one per case.
    let cases: [(&str, &str, &str); 3] = [
        ("code_verifier", "A".repeat(43).leak(), "invalid_grant"),
        ("redirect_uri", "https://evil.example/cb", "invalid_grant"),
        ("client_id", "not-our-client", "invalid_client"),
    ];
    for (field, value, want) in cases {
        let code = approved_code(&f).await;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", f.client_id.as_str()),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
        ];
        for pair in &mut form {
            if pair.0 == field {
                pair.1 = value;
            }
        }
        let (status, body) = token(&f, &form).await;
        assert!(status == 400 || status == 401, "{field}: {status}");
        assert_eq!(body["error"], want, "{field}: {body}");
    }
}

#[tokio::test]
async fn refresh_rotates_and_reuse_revokes_the_grant() {
    let prefix = require_local_db!();
    let _env = env().await;
    let f = fixture(&prefix).await;
    let (_, first) = exchange(&f, &approved_code(&f).await).await;
    let refresh1 = first["refresh_token"].as_str().unwrap().to_string();

    let (status, second) = refresh(&f, &refresh1).await;
    assert_eq!(status, 200, "{second}");
    let access2 = second["access_token"].as_str().unwrap();
    let refresh2 = second["refresh_token"].as_str().unwrap();
    assert_ne!(refresh2, refresh1);
    assert!(
        oauth::verify_access_token(&*f.app, access2, RESOURCE)
            .await
            .is_ok()
    );

    // Replaying the rotated-away token is treated as theft: the grant dies.
    let (status, body) = refresh(&f, &refresh1).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"], "invalid_grant");
    let after = oauth::verify_access_token(&*f.app, access2, RESOURCE).await;
    assert!(matches!(after, Err(AuthError::Permanent(_))));
    assert_eq!(refresh(&f, refresh2).await.0, 400);
}

#[tokio::test]
async fn a_disabled_user_cannot_refresh() {
    let prefix = require_local_db!();
    let _env = env().await;
    let f = fixture(&prefix).await;
    let (_, first) = exchange(&f, &approved_code(&f).await).await;
    f.app
        .db
        .update_user(
            &f.user_id,
            db::UserUpdateShape::Fields {
                name: "OAuth Tester",
                enabled: false,
            },
        )
        .await
        .unwrap();
    let (status, body) = refresh(&f, first["refresh_token"].as_str().unwrap()).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"], "invalid_grant");
}

#[tokio::test]
async fn approve_refuses_bad_requests_and_non_user_principals() {
    let prefix = require_local_db!();
    let _env = env().await;
    let f = fixture(&prefix).await;
    let user = || user_auth(&f.user_id);

    let (ok, err) = approve_with(&f, user(), "https://evil.example/cb", "S256", None).await;
    assert!(ok.is_none());
    assert!(err.unwrap().contains("redirect_uri is not registered"));

    let (ok, err) = approve_with(&f, user(), REDIRECT, "plain", None).await;
    assert!(ok.is_none());
    assert!(err.unwrap().contains("S256"));

    let (ok, err) = approve_with(&f, user(), REDIRECT, "S256", Some("admin")).await;
    assert!(ok.is_none());
    assert!(err.unwrap().contains("Unsupported scope"));

    let requester = AuthInfo::Requester {
        email: "someone@example.com".into(),
        instance_id: "inst".into(),
    };
    let (ok, err) = approve_with(&f, requester, REDIRECT, "S256", None).await;
    assert!(ok.is_none(), "a Requester must not be able to approve");
    assert!(err.is_some());

    let (ok, err) = approve_with(
        &f,
        AuthInfo::ApiToken {
            token_id: "t".into(),
            instance_id: "i".into(),
        },
        REDIRECT,
        "S256",
        None,
    )
    .await;
    assert!(ok.is_none(), "an ApiToken must not be able to approve");
    assert!(err.is_some());
}

#[tokio::test]
async fn consent_query_returns_only_the_name_and_redirect_host() {
    let prefix = require_local_db!();
    let _env = env().await;
    let f = fixture(&prefix).await;
    let run = |redirect: &str| {
        let req = Request::new(
            "query($c: String!, $r: String!) { oauthAuthorizationRequest(clientId: $c, redirectUri: $r) { clientName redirectHost } }",
        )
        .variables(Variables::from_json(json!({"c": f.client_id, "r": redirect})))
        .data(user_auth(&f.user_id));
        f.schema.execute(req)
    };
    let ok = run(REDIRECT).await;
    assert!(ok.errors.is_empty(), "{:?}", ok.errors);
    let data = serde_json::to_value(&ok.data).unwrap();
    assert_eq!(
        data["oauthAuthorizationRequest"],
        json!({"clientName": "Test Client", "redirectHost": "client.example"})
    );
    assert!(!run("https://evil.example/cb").await.errors.is_empty());
}
