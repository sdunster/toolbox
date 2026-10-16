//! Integration tests for the "connected AI apps" list and revoke
//! (`User.oauthGrants`, `revokeOauthGrant`) against **DynamoDB Local**:
//!
//!   - `me { oauthGrants }` lists the caller's own grants, newest first, and
//!     omits anything past its expiry (DynamoDB's TTL deletion lags);
//!   - another user's grants are `FORBIDDEN` — a superuser looking them up via
//!     `adminUser` included, same posture as `User.passkeys`;
//!   - token hashes and the client id are not part of the schema at all;
//!   - `revokeOauthGrant` deletes the caller's own grant (its access token stops
//!     verifying at once), and someone else's grant fails *identically* to a
//!     nonexistent one, so ids can't be probed. A superuser gets no override.
//!
//! # Running this test
//!
//! ```sh
//! make local-up && make local-tables
//! cd api && set -a && . ../local/local.env && set +a
//! cargo test --test oauth_grants_dynamodb_local
//! ```
//!
//! Every test **skips itself** when no reachable local DynamoDB is configured.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use toolbox::app;
use toolbox::auth::{AuthError, AuthInfo};
use toolbox::db;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::graphql;
use toolbox::mockmail;
use toolbox::mockqueue;
use toolbox::mockstorage;
use toolbox::oauth;

type TestApp =
    app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage, mockqueue::Queue>;
type TestSchema = graphql::ToolboxSchema<TestApp>;

const RESOURCE: &str = "https://toolbox.test/mcp";

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
                eprintln!("oauth_grants_dynamodb_local: no reachable local DynamoDB — skipping.");
                return;
            }
        }
    };
}

struct Fixture {
    app: Arc<TestApp>,
    schema: TestSchema,
}

async fn fixture(prefix: &str) -> Fixture {
    let db = dynamodb::Handler::new(prefix, false).await;
    let app = Arc::new(app::new(
        db,
        mockmail::Handler::new(),
        mockstorage::Storage::new(),
        mockqueue::Queue::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(app.clone(), webauthn);
    Fixture { app, schema }
}

async fn make_user(f: &Fixture) -> String {
    let email = format!("grants-{}@example.com", nanoid::nanoid!(8)).to_lowercase();
    f.app
        .db
        .create_user(&email, "Grants Tester")
        .await
        .unwrap()
        .id
}

fn auth(user_id: &str, superuser: bool) -> AuthInfo {
    AuthInfo::User {
        id: user_id.to_string(),
        memberships: vec![],
        is_superuser: superuser,
        token_id: None,
        grant_id: None,
    }
}

/// Mint + store a grant for `user_id`, `age_s` old, optionally already expired.
async fn store_grant(
    f: &Fixture,
    user_id: &str,
    name: &str,
    age_s: u64,
    expired: bool,
) -> (db::OAuthGrant, String) {
    let now = toolbox::clock::now_sec();
    let (mut grant, access, _) = oauth::mint_grant(
        user_id,
        "client-id-secret-value",
        name,
        "https://client.example/cb",
        RESOURCE,
        oauth::DEFAULT_SCOPE,
        now - age_s,
    );
    if expired {
        grant.refresh_expires_at = now - 1;
    }
    f.app.db.create_oauth_grant(&grant).await.unwrap();
    (grant, access)
}

fn code_of(response: &Response) -> String {
    let err = response.errors.first().expect("expected an error");
    match err.extensions.as_ref().and_then(|e| e.get("code")) {
        Some(async_graphql::Value::String(s)) => s.clone(),
        // Plain `anyhow!` errors carry no code; the classifier defaults them.
        _ => "NO_CODE".to_string(),
    }
}

async fn run(f: &Fixture, query: &str, vars: Value, who: AuthInfo) -> Response {
    f.schema
        .execute(
            Request::new(query)
                .variables(Variables::from_json(vars))
                .data(who),
        )
        .await
}

const LIST_ME: &str = "{ me { oauthGrants { id clientName redirectHost scope createdAt lastUsedAt refreshExpiresAt } } }";
const REVOKE: &str = "mutation($id: ID!) { revokeOauthGrant(id: $id) }";

#[tokio::test]
async fn lists_own_grants_newest_first_and_omits_expired() {
    let prefix = require_local_db!();
    let f = fixture(&prefix).await;
    let user = make_user(&f).await;
    let (old, _) = store_grant(&f, &user, "Older", 5_000, false).await;
    let (new, _) = store_grant(&f, &user, "Newer", 100, false).await;
    store_grant(&f, &user, "Expired", 9_000, true).await;

    let response = run(&f, LIST_ME, json!({}), auth(&user, false)).await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    let data = serde_json::to_value(&response.data).unwrap();
    let grants = data["me"]["oauthGrants"].as_array().unwrap();
    let ids: Vec<&str> = grants.iter().map(|g| g["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![new.id.as_str(), old.id.as_str()]);
    assert_eq!(grants[0]["clientName"], "Newer");
    assert_eq!(grants[0]["redirectHost"], "client.example");
    assert!(grants[0]["lastUsedAt"].is_null());
}

#[tokio::test]
async fn hashes_and_client_id_are_not_in_the_schema() {
    let prefix = require_local_db!();
    let f = fixture(&prefix).await;
    let user = make_user(&f).await;
    for field in [
        "clientId",
        "accessTokenHash",
        "refreshTokenHash",
        "redirectUri",
    ] {
        let q = format!("{{ me {{ oauthGrants {{ {field} }} }} }}");
        let response = run(&f, &q, json!({}), auth(&user, false)).await;
        assert!(
            !response.errors.is_empty(),
            "{field} should not be queryable"
        );
    }
}

#[tokio::test]
async fn other_users_grants_are_forbidden_even_for_a_superuser() {
    let prefix = require_local_db!();
    let f = fixture(&prefix).await;
    let (owner, admin) = (make_user(&f).await, make_user(&f).await);
    store_grant(&f, &owner, "Mine", 10, false).await;

    let query = "query($id: ID!) { adminUser(id: $id) { oauthGrants { id } } }";
    let response = run(&f, query, json!({"id": owner}), auth(&admin, true)).await;
    assert_eq!(code_of(&response), "FORBIDDEN", "{:?}", response.errors);
}

#[tokio::test]
async fn revoke_deletes_the_callers_own_grant_and_kills_its_token() {
    let prefix = require_local_db!();
    let f = fixture(&prefix).await;
    let user = make_user(&f).await;
    let (grant, access) = store_grant(&f, &user, "Mine", 10, false).await;
    assert!(
        oauth::verify_access_token(&*f.app, &access, RESOURCE)
            .await
            .is_ok()
    );

    let response = run(&f, REVOKE, json!({"id": grant.id}), auth(&user, false)).await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    assert_eq!(f.app.db.get_oauth_grant(&grant.id).await.unwrap(), None);
    let after = oauth::verify_access_token(&*f.app, &access, RESOURCE).await;
    assert!(matches!(after, Err(AuthError::Permanent(_))));
}

#[tokio::test]
async fn revoking_someone_elses_grant_looks_exactly_like_a_missing_one() {
    let prefix = require_local_db!();
    let f = fixture(&prefix).await;
    let (owner, other, admin) = (
        make_user(&f).await,
        make_user(&f).await,
        make_user(&f).await,
    );
    let (grant, _) = store_grant(&f, &owner, "Mine", 10, false).await;

    let missing = run(
        &f,
        REVOKE,
        json!({"id": "no-such-grant"}),
        auth(&other, false),
    )
    .await;
    for who in [auth(&other, false), auth(&admin, true)] {
        let theirs = run(&f, REVOKE, json!({"id": grant.id}), who).await;
        assert_eq!(
            theirs.errors[0].message, missing.errors[0].message,
            "must not distinguish 'not yours' from 'missing'"
        );
    }
    // ...and it survived both attempts.
    assert!(f.app.db.get_oauth_grant(&grant.id).await.unwrap().is_some());
}

#[tokio::test]
async fn a_requester_cannot_list_or_revoke() {
    let prefix = require_local_db!();
    let f = fixture(&prefix).await;
    let user = make_user(&f).await;
    let (grant, _) = store_grant(&f, &user, "Mine", 10, false).await;
    let requester = || AuthInfo::Requester {
        email: "x@example.com".into(),
        instance_id: "i".into(),
    };
    assert!(
        !run(&f, LIST_ME, json!({}), requester())
            .await
            .errors
            .is_empty()
    );
    let response = run(&f, REVOKE, json!({"id": grant.id}), requester()).await;
    assert!(!response.errors.is_empty());
    assert!(f.app.db.get_oauth_grant(&grant.id).await.unwrap().is_some());
}
