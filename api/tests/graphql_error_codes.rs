//! Exercises the error-classification extension (`graphql::RequestMetricsExt`) end
//! to end, over a real `async_graphql::Schema` built with the crate's mocks
//! (`mockdb` + `mockmail`):
//!
//! - a guard failure must carry `extensions.code = "UNAUTHENTICATED"`
//! - a plain `db::Error` bubbling out of a resolver, with no `ApiError` wrapping,
//!   must default to `extensions.code = "INTERNAL"`
//!
//! `guard_failure_is_unauthenticated`/`plain_db_error_is_internal` use a small ad
//! hoc schema rather than the crate's real `QueryRoot`, so they can exercise both
//! guard and non-guard failure paths without polluting the committed
//! `schema.graphql` with test-only fields. `real_schema_guarded_field_with_no_credentials_is_unauthenticated`
//! covers the same guard-failure shape but against the crate's actual production
//! schema (`me`), so a regression in how `graphql::build_schema` wires the guard
//! extension wouldn't be masked by the ad hoc schema alone.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Object, Schema, Value};
use toolbox::app;
use toolbox::db;
use toolbox::graphql::RequestMetricsExt;
use toolbox::graphql::auth::{AuthGuard, AuthRequirement};
use toolbox::mockdb;
use toolbox::mockmail;
use toolbox::mockqueue;
use toolbox::mockstorage;

struct TestQuery;

#[Object]
impl TestQuery {
    /// Requires authentication. No `AuthInfo` is ever injected into these tests, so
    /// this always fails its guard.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn guarded(&self) -> &'static str {
        "unreachable"
    }

    /// Returns a plain `db::Error`, with no `ApiError` wrapping, so it must
    /// classify as `INTERNAL` by default.
    async fn boom(&self) -> anyhow::Result<&'static str> {
        Err(db::Error::Infrastructure("simulated failure".into()).into())
    }
}

fn build_test_schema() -> Schema<TestQuery, EmptyMutation, EmptySubscription> {
    let my_app = Arc::new(app::new(
        mockdb::Handler::new(),
        mockmail::Handler::new(),
        mockstorage::Storage::new(),
        mockqueue::Queue::new(),
        0,
    ));
    Schema::build(TestQuery, EmptyMutation, EmptySubscription)
        .data(my_app)
        .extension(RequestMetricsExt)
        .finish()
}

fn error_code(response: &async_graphql::Response) -> Option<String> {
    let extensions = response.errors.first()?.extensions.as_ref()?;
    match extensions.get("code")? {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

#[tokio::test]
async fn guard_failure_is_unauthenticated() {
    let schema = build_test_schema();
    let response = schema.execute("{ guarded }").await;
    assert!(!response.errors.is_empty(), "expected an error");
    assert_eq!(error_code(&response).as_deref(), Some("UNAUTHENTICATED"));
}

#[tokio::test]
async fn plain_db_error_is_internal() {
    let schema = build_test_schema();
    let response = schema.execute("{ boom }").await;
    assert!(!response.errors.is_empty(), "expected an error");
    assert_eq!(error_code(&response).as_deref(), Some("INTERNAL"));
}

#[tokio::test]
async fn real_schema_guarded_field_with_no_credentials_is_unauthenticated() {
    let my_app = Arc::new(app::new(
        mockdb::Handler::new(),
        mockmail::Handler::new(),
        mockstorage::Storage::new(),
        mockqueue::Queue::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = toolbox::graphql::build_schema(my_app, webauthn);

    let response = schema.execute("{ me { id } }").await;
    assert!(!response.errors.is_empty(), "expected an error");
    assert_eq!(error_code(&response).as_deref(), Some("UNAUTHENTICATED"));
}
