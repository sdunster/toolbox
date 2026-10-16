//! Shared harness for the MCP integration tests (`tests/mcp_*_dynamodb_local.rs`),
//! all of which run against **DynamoDB Local** — not the mocks — through the real
//! schema and the real `mcp::handle_post`, authenticated with a real `mtoa_`
//! access token minted the same way the token endpoint mints one.
//!
//! Included with `mod common;` + `use common::mcp::*;`. `#![allow(dead_code)]`
//! because each test binary uses a different subset.

#![allow(dead_code)]

use std::sync::Arc;

use serde_json::{Value, json};
use toolbox::app;
use toolbox::app::HasDb as _;
use toolbox::base_url::api_base_url;
use toolbox::db;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::graphql::{self, ClientIp};
use toolbox::mcp;
use toolbox::mockmail;
use toolbox::mockqueue;
use toolbox::mockstorage;
use toolbox::oauth;

pub type TestApp =
    app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage, mockqueue::Queue>;
pub type TestSchema = graphql::ToolboxSchema<TestApp>;

/// The `Host` the tests present. `api_base_url` derives `https://toolbox.test`
/// from it unless `API_BASE_URL` is set, so tests compute the resource through
/// the same function rather than assuming either.
pub const HOST: &str = "toolbox.test";

pub fn resource() -> String {
    format!("{}/mcp", api_base_url(Some(HOST)))
}

/// `Some(DB_PREFIX)` when a local DynamoDB is reachable, else `None` (the test
/// then skips itself, like every other `*_dynamodb_local.rs`).
pub async fn local_db_prefix() -> Option<String> {
    toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;
    let client = toolbox::local_dev::dynamodb_client().await;
    client.list_tables().send().await.ok()?;
    Some(prefix)
}

pub fn unique(label: &str) -> String {
    format!("{label}-{}", nanoid::nanoid!(8))
}

pub fn unique_email(label: &str) -> String {
    format!("{label}-{}@example.com", nanoid::nanoid!(8)).to_lowercase()
}

pub struct Fixture {
    pub app: Arc<TestApp>,
    pub schema: TestSchema,
}

impl Fixture {
    pub fn db(&self) -> &impl db::Handler {
        self.app.db()
    }
}

pub async fn fixture(prefix: &str) -> Fixture {
    let app = Arc::new(app::new(
        dynamodb::Handler::new(prefix, false).await,
        mockmail::Handler::new(),
        mockstorage::Storage::new(),
        mockqueue::Queue::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(app.clone(), webauthn);
    Fixture { app, schema }
}

/// Create an enabled user (optionally a superuser).
pub async fn make_user(f: &Fixture, label: &str, superuser: bool) -> String {
    let user = f
        .db()
        .create_user(&unique_email(label), label)
        .await
        .expect("create_user");
    if superuser {
        f.db()
            .update_user(&user.id, db::UserUpdateShape::SetSuperuser(true))
            .await
            .expect("set superuser");
    }
    user.id
}

/// Create an instance of `kind` with one inbound address (support only), and
/// return `(instance_id, slug)`.
pub async fn make_instance(f: &Fixture, label: &str, kind: db::InstanceKind) -> (String, String) {
    let slug = unique(&format!("{label}-slug"));
    let instance = f
        .db()
        .create_instance(&unique(label), &slug, label, "", false, kind)
        .await
        .expect("create_instance");
    if kind == db::InstanceKind::Support {
        f.db()
            .create_inbound_address(
                &format!("{}@{label}.example.com", nanoid::nanoid!(6).to_lowercase()),
                &instance.id,
                db::AddressKind::Exact,
            )
            .await
            .expect("create_inbound_address");
    }
    (instance.id, slug)
}

pub async fn add_member(f: &Fixture, user_id: &str, instance_id: &str, owner: bool) {
    let role = if owner {
        db::MembershipRole::Owner
    } else {
        db::MembershipRole::Agent
    };
    f.db()
        .create_membership(user_id, instance_id, role)
        .await
        .expect("create_membership");
}

/// Mint + store a real grant for `user_id` and return its access token — exactly
/// what the token endpoint's `authorization_code` grant ends up with.
pub async fn access_token_for(f: &Fixture, user_id: &str) -> String {
    let (grant, access, _refresh) = oauth::mint_grant(
        user_id,
        "test-client",
        "Test Client",
        "https://client.example/cb",
        &resource(),
        oauth::DEFAULT_SCOPE,
        toolbox::clock::now_sec(),
    );
    f.db()
        .create_oauth_grant(&grant)
        .await
        .expect("create grant");
    access
}

/// POST a raw JSON-RPC body to the MCP endpoint as `token`.
pub async fn post_raw(
    f: &Fixture,
    token: Option<&str>,
    body: &str,
) -> toolbox::oauth_http::HttpReply {
    let header = token.map(|t| format!("Bearer {t}"));
    mcp::handle_post(
        &f.app,
        &f.schema,
        Some(HOST),
        header.as_deref(),
        ClientIp(Some("198.51.100.7".into())),
        body.as_bytes(),
    )
    .await
}

/// A JSON-RPC call; returns the parsed response body.
pub async fn rpc(f: &Fixture, token: &str, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let reply = post_raw(f, Some(token), &body.to_string()).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    serde_json::from_str(&reply.body).expect("JSON reply")
}

/// Call a tool. Returns `Ok(structuredContent)` or `Err(error text)`.
pub async fn call_tool(
    f: &Fixture,
    token: &str,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let response = rpc(
        f,
        token,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    )
    .await;
    let result = &response["result"];
    if result["isError"].as_bool() == Some(true) {
        Err(result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    } else {
        Ok(result["structuredContent"].clone())
    }
}
