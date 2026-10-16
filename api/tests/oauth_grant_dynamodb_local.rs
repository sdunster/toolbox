//! Integration tests for the `oauth_grant` table against **DynamoDB Local** —
//! not the mocks. The compare-and-swap in refresh rotation, the strongly
//! consistent `GetItem`, and the `user_id-index` GSI only show up against a real
//! table:
//!
//!   - a minted grant round-trips through `create_oauth_grant`/`get_oauth_grant`
//!     and its access token verifies through `oauth::verify_access_token` to an
//!     `AuthInfo::User` with `grant_id` set;
//!   - a wrong resource, a tampered secret, and a disabled user all fail;
//!   - `verify_token` (the GraphQL auth path) still rejects the same token;
//!   - rotation swaps both secrets, and a second rotation with the *stale*
//!     refresh hash loses (`NotFound`) — two concurrent refreshes can't both win;
//!   - `list_oauth_grants_by_user` returns only that user's grants;
//!   - an absent `last_used_at` is omitted, never written as `Null`.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test oauth_grant_dynamodb_local
//! ```
//!
//! Like the other `*_dynamodb_local.rs` files, every test **skips itself** when
//! no reachable local DynamoDB is configured.

use toolbox::app;
use toolbox::auth::{self, AuthError, AuthInfo};
use toolbox::db;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::mockmail;
use toolbox::mockqueue;
use toolbox::mockstorage;
use toolbox::oauth;

type TestApp =
    app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage, mockqueue::Queue>;

async fn local_db_prefix() -> Option<String> {
    toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;
    let client = toolbox::local_dev::dynamodb_client().await;
    if client.list_tables().send().await.is_err() {
        eprintln!("oauth_grant_dynamodb_local: DynamoDB not reachable — skipping.");
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
                    "oauth_grant_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set to a \
                     reachable local DynamoDB — skipping. See this file's header."
                );
                return;
            }
        }
    };
}

const RESOURCE: &str = "https://toolbox.test/mcp";

fn test_app(db: dynamodb::Handler) -> TestApp {
    app::new(
        db,
        mockmail::Handler::new(),
        mockstorage::Storage::new(),
        mockqueue::Queue::new(),
        0,
    )
}

async fn make_user(db: &dynamodb::Handler) -> String {
    let email = format!("oauth-{}@example.com", nanoid::nanoid!(8)).to_lowercase();
    db.create_user(&email, "OAuth Tester")
        .await
        .expect("create user")
        .id
}

fn mint(user_id: &str) -> (db::OAuthGrant, String, String) {
    oauth::mint_grant(
        user_id,
        "client-1",
        "Test Client",
        "https://client.example/cb",
        RESOURCE,
        oauth::DEFAULT_SCOPE,
        toolbox::clock::now_sec(),
    )
}

#[tokio::test]
async fn grant_round_trips_and_its_access_token_verifies() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let user_id = make_user(&db).await;
    let (grant, access, _refresh) = mint(&user_id);
    db.create_oauth_grant(&grant).await.expect("create grant");

    assert_eq!(
        db.get_oauth_grant(&grant.id).await.unwrap(),
        Some(grant.clone())
    );

    let app = test_app(db);
    match oauth::verify_access_token(&app, &access, RESOURCE).await {
        Ok(AuthInfo::User { id, grant_id, .. }) => {
            assert_eq!(id, user_id);
            assert_eq!(grant_id.as_deref(), Some(grant.id.as_str()));
        }
        other => panic!("expected AuthInfo::User, got {:?}", other.is_ok()),
    }

    // The same token must not authenticate against another audience...
    let wrong = oauth::verify_access_token(&app, &access, "https://toolbox.test/other").await;
    assert!(matches!(wrong, Err(AuthError::Permanent(_))));
    // ...nor with the wrong secret half...
    let (grant_id, _) = oauth::parse_access_token(&access).unwrap();
    let forged = format!("{}{grant_id}.not-the-secret", oauth::ACCESS_TOKEN_PREFIX);
    let bad = oauth::verify_access_token(&app, &forged, RESOURCE).await;
    assert!(matches!(bad, Err(AuthError::Permanent(_))));
    // ...nor through the GraphQL auth path.
    let via_graphql = auth::verify_token(&app, &access).await;
    assert!(matches!(via_graphql, Err(AuthError::Permanent(_))));
}

#[tokio::test]
async fn a_disabled_user_cannot_use_their_grant() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let user_id = make_user(&db).await;
    let (grant, access, _) = mint(&user_id);
    db.create_oauth_grant(&grant).await.unwrap();
    db.update_user(
        &user_id,
        db::UserUpdateShape::Fields {
            name: "OAuth Tester",
            enabled: false,
        },
    )
    .await
    .expect("disable user");

    let app = test_app(db);
    let result = oauth::verify_access_token(&app, &access, RESOURCE).await;
    assert!(matches!(result, Err(AuthError::Permanent(_))));
}

#[tokio::test]
async fn rotation_is_compare_and_swap_on_the_refresh_hash() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let user_id = make_user(&db).await;
    let (grant, old_access, _old_refresh) = mint(&user_id);
    db.create_oauth_grant(&grant).await.unwrap();

    let now = toolbox::clock::now_sec();
    let (shape, new_access, _new_refresh) = oauth::rotate_grant(&grant, now);
    db.update_oauth_grant(&grant.id, shape.clone())
        .await
        .expect("first rotation wins");

    // A second refresh presenting the same (now stale) refresh token loses.
    let second = db.update_oauth_grant(&grant.id, shape).await;
    assert!(matches!(second, Err(db::Error::NotFound(_))), "{second:?}");

    let app = test_app(db);
    assert!(
        oauth::verify_access_token(&app, &new_access, RESOURCE)
            .await
            .is_ok()
    );
    let stale = oauth::verify_access_token(&app, &old_access, RESOURCE).await;
    assert!(matches!(stale, Err(AuthError::Permanent(_))));
}

#[tokio::test]
async fn list_by_user_is_scoped_and_delete_revokes() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (user_a, user_b) = (make_user(&db).await, make_user(&db).await);
    let (a1, _, _) = mint(&user_a);
    let (a2, _, _) = mint(&user_a);
    let (b1, _, _) = mint(&user_b);
    for g in [&a1, &a2, &b1] {
        db.create_oauth_grant(g).await.unwrap();
    }

    let mut ids: Vec<String> = db
        .list_oauth_grants_by_user(&user_a)
        .await
        .unwrap()
        .into_iter()
        .map(|g| g.id)
        .collect();
    ids.sort();
    let mut want = vec![a1.id.clone(), a2.id.clone()];
    want.sort();
    assert_eq!(ids, want);

    db.delete_oauth_grant(&a1.id).await.unwrap();
    assert_eq!(db.get_oauth_grant(&a1.id).await.unwrap(), None);
}

#[tokio::test]
async fn last_used_at_is_omitted_until_first_use_never_null() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let user_id = make_user(&db).await;
    let (grant, access, _) = mint(&user_id);
    db.create_oauth_grant(&grant).await.unwrap();

    let client = toolbox::local_dev::dynamodb_client().await;
    let raw = client
        .get_item()
        .table_name(format!("{prefix}_oauth_grant"))
        .key(
            "id",
            aws_sdk_dynamodb::types::AttributeValue::S(grant.id.clone()),
        )
        .send()
        .await
        .unwrap();
    assert!(!raw.item().unwrap().contains_key("last_used_at"));

    let app = test_app(db);
    oauth::verify_access_token(&app, &access, RESOURCE)
        .await
        .expect("verifies");
    let touched = app.db.get_oauth_grant(&grant.id).await.unwrap().unwrap();
    assert!(touched.last_used_at.is_some());
}
