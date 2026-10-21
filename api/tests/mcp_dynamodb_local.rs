//! Integration tests for the MCP endpoint's protocol layer and `whoami`, against
//! **DynamoDB Local** — through the real schema and the real `mcp::handle_post`,
//! authenticated with a real `mtoa_` access token:
//!
//!   - auth: no token / a bad token / a token minted for another audience all get
//!     a 401 carrying the RFC 9728 `WWW-Authenticate` challenge (and only a
//!     *presented* token adds `error="invalid_token"`); a disabled user's token is
//!     rejected; a normal site `mtu_` token is not an MCP credential either;
//!   - protocol: `initialize` negotiates a version, `ping`, `tools/list`,
//!     notifications get a 202, batches/garbage/unknown methods are refused;
//!   - `whoami` returns the caller's memberships (role + instance kind), and for
//!     a superuser *without* a membership returns none — the superuser boundary
//!     is unchanged by reaching the API through MCP.
//!
//! # Running this test
//!
//! ```sh
//! make local-up && make local-tables
//! cd api && set -a && . ../local/local.env && set +a
//! cargo test --test mcp_dynamodb_local
//! ```
//!
//! Every test **skips itself** when no reachable local DynamoDB is configured.

mod common;

use common::mcp::*;
use serde_json::{Value, json};
use toolbox::db;
use toolbox::db::Handler as _;

fn header<'a>(reply: &'a toolbox::oauth_http::HttpReply, name: &str) -> Option<&'a str> {
    reply
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn missing_token_gets_the_protected_resource_challenge() {
    let Some(prefix) = local_db_prefix().await else {
        return;
    };
    let f = fixture(&prefix).await;
    let reply = post_raw(&f, None, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).await;
    assert_eq!(reply.status, 401);
    let challenge = header(&reply, "WWW-Authenticate").unwrap();
    assert!(
        challenge.starts_with("Bearer resource_metadata=\""),
        "{challenge}"
    );
    assert!(challenge.contains("/.well-known/oauth-protected-resource/mcp"));
    assert!(!challenge.contains("invalid_token"), "{challenge}");
}

#[tokio::test]
async fn a_bad_or_foreign_token_is_invalid_token() {
    let Some(prefix) = local_db_prefix().await else {
        return;
    };
    let f = fixture(&prefix).await;
    let user = make_user(&f, "mcp-bad", false).await;
    let good = access_token_for(&f, &user).await;

    // Right shape, wrong secret.
    let (grant_id, _) = toolbox::oauth::parse_access_token(&good).unwrap();
    let forged = format!("mtoa_{grant_id}.not-the-secret");

    // A site session token is not an MCP credential.
    let site = toolbox::auth::issue_user_token(&*f.app, &user)
        .await
        .unwrap();

    // A grant minted for a different audience.
    let (other, other_access, _) = toolbox::oauth::mint_grant(
        &user,
        "c",
        "C",
        "https://client.example/cb",
        "https://elsewhere.example/mcp",
        toolbox::oauth::DEFAULT_SCOPE,
        toolbox::clock::now_sec(),
    );
    f.db().create_oauth_grant(&other).await.unwrap();

    for token in [forged, site, other_access, "garbage".to_string()] {
        let reply = post_raw(
            &f,
            Some(&token),
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        )
        .await;
        assert_eq!(reply.status, 401, "{token}");
        assert!(
            header(&reply, "WWW-Authenticate")
                .unwrap()
                .contains("error=\"invalid_token\""),
            "{token}"
        );
    }
    // ...while the good one works.
    assert_eq!(rpc(&f, &good, "ping", json!({})).await["result"], json!({}));
}

#[tokio::test]
async fn a_disabled_users_token_stops_working() {
    let Some(prefix) = local_db_prefix().await else {
        return;
    };
    let f = fixture(&prefix).await;
    let user = make_user(&f, "mcp-disabled", false).await;
    let token = access_token_for(&f, &user).await;
    f.db()
        .update_user(
            &user,
            db::UserUpdateShape::Fields {
                name: "mcp-disabled",
                enabled: false,
            },
        )
        .await
        .unwrap();
    let reply = post_raw(
        &f,
        Some(&token),
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
    )
    .await;
    assert_eq!(reply.status, 401);
}

#[tokio::test]
async fn protocol_basics() {
    let Some(prefix) = local_db_prefix().await else {
        return;
    };
    let f = fixture(&prefix).await;
    let user = make_user(&f, "mcp-proto", false).await;
    let token = access_token_for(&f, &user).await;

    // initialize echoes a version we speak, and falls back to our newest otherwise.
    let init = rpc(
        &f,
        &token,
        "initialize",
        json!({"protocolVersion": "2025-06-18"}),
    )
    .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "toolbox");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    let init = rpc(
        &f,
        &token,
        "initialize",
        json!({"protocolVersion": "1999-01-01"}),
    )
    .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");

    // tools/list: every tool has an object-rooted output schema.
    let list = rpc(&f, &token, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t["name"] == "whoami"));
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert_eq!(tool["outputSchema"]["type"], "object", "{tool}");
        assert!(tool["description"].as_str().unwrap().len() > 20, "{tool}");
    }

    // Unknown method → JSON-RPC error; unknown tool → isError, not a protocol error.
    let unknown = rpc(&f, &token, "resources/list", json!({})).await;
    assert_eq!(unknown["error"]["code"], -32601);
    let err = call_tool(&f, &token, "no_such_tool", json!({}))
        .await
        .unwrap_err();
    assert!(err.contains("Unknown tool"), "{err}");

    // Notifications get a bare 202; batches, garbage and non-objects are refused.
    let note = post_raw(
        &f,
        Some(&token),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert_eq!(note.status, 202);
    assert!(note.body.is_empty());
    for body in ["[]", "not json", "42", r#"{"jsonrpc":"2.0","id":1}"#] {
        let reply = post_raw(&f, Some(&token), body).await;
        assert_eq!(reply.status, 400, "{body}");
    }
    assert_eq!(toolbox::mcp::method_not_allowed().status, 405);
}

#[tokio::test]
async fn whoami_reports_memberships_with_role_and_kind() {
    let Some(prefix) = local_db_prefix().await else {
        return;
    };
    let f = fixture(&prefix).await;
    let user = make_user(&f, "mcp-who", false).await;
    let (support, _) = make_instance(&f, "mcp-support", db::InstanceKind::Support).await;
    let (finance, _) = make_instance(&f, "mcp-finance", db::InstanceKind::Finance).await;
    add_member(&f, &user, &support, true).await;
    add_member(&f, &user, &finance, false).await;
    let token = access_token_for(&f, &user).await;

    let who = call_tool(&f, &token, "whoami", json!({})).await.unwrap();
    let me = &who["user"];
    assert_eq!(me["id"], user);
    assert_eq!(me["isSuperuser"], false);
    let by_instance: std::collections::HashMap<&str, (&str, &str)> = me["memberships"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["instance"]["id"].as_str().unwrap(),
                (
                    m["role"].as_str().unwrap(),
                    m["instance"]["kind"].as_str().unwrap(),
                ),
            )
        })
        .collect();
    assert_eq!(by_instance[support.as_str()], ("OWNER", "SUPPORT"));
    assert_eq!(by_instance[finance.as_str()], ("AGENT", "FINANCE"));
    // Nothing that belongs to the site session leaks into the tool output.
    let text = who.to_string();
    for forbidden in [
        "passkey",
        "oauthGrants",
        "notificationSettings",
        "tokenHash",
    ] {
        assert!(!text.contains(forbidden), "{forbidden} in {text}");
    }
}

#[tokio::test]
async fn a_superuser_without_a_membership_sees_none() {
    let Some(prefix) = local_db_prefix().await else {
        return;
    };
    let f = fixture(&prefix).await;
    let admin = make_user(&f, "mcp-super", true).await;
    make_instance(&f, "mcp-elsewhere", db::InstanceKind::Support).await;
    let token = access_token_for(&f, &admin).await;

    let who: Value = call_tool(&f, &token, "whoami", json!({})).await.unwrap();
    assert_eq!(who["user"]["isSuperuser"], true);
    assert_eq!(who["user"]["memberships"], json!([]));
}
