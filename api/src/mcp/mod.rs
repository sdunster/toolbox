//! MCP ([Model Context Protocol](https://modelcontextprotocol.io)) endpoint:
//! `POST /mcp`, the "Streamable HTTP" transport, run stateless with plain JSON
//! responses (no `Mcp-Session-Id`, no SSE) — Lambda can't hold sessions.
//!
//! **Why this is a small hand-rolled JSON-RPC 2.0 dispatcher rather than the
//! official `rmcp` crate:** its session machinery is on by default and has to be
//! explicitly disabled (alongside `json_response`) to get the stateless/JSON
//! behaviour a Lambda needs; it still wants a `SessionManager`, a service
//! factory building a whole handler per request, origin allow-lists and SSE
//! fallbacks; handing our verified [`AuthInfo`] to a tool means threading it
//! through `http::request::Parts` extensions, while our bearer check is itself
//! an async DB-backed call that has to run *before* rmcp's dispatch anyway; and
//! bridging one `tower::Service` to both poem and `lambda_http` is two fresh
//! pieces of glue. For tools that each run one fixed GraphQL document, a
//! dispatcher shaped like the OAuth endpoints ([`crate::oauth_http`]'s
//! `HttpReply`) is less code and runs identically under poem and `lambda_http`.
//!
//! **Layout.** This module is the protocol (auth, JSON-RPC, the tool registry);
//! [`tool`] holds the shared plumbing every tool uses to run GraphQL as its
//! caller; each family of tools is its own module exposing `catalogue()` (its
//! `tools/list` entries) and `dispatch()` (`None` for a name that isn't its own).
//! Adding a family is one module plus one line in each of [`tool_catalogue`] and
//! [`dispatch_tool`].
//!
//! **Auth.** Only an OAuth `mtoa_` access token audience-bound to `<api
//! base>/mcp` is accepted here — see [`oauth::verify_access_token`]. Missing or
//! invalid tokens get a 401 carrying `WWW-Authenticate` pointing at the
//! protected-resource metadata (RFC 9728), per the MCP authorization spec.

pub mod invoicing;
pub mod tickets;
pub mod tool;
pub mod whoami;

use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};

use crate::app::{App, HasDb, HasMail, HasStorage};
use crate::auth::{self, AuthError, AuthInfo};
use crate::base_url::api_base_url;
use crate::graphql::{ClientIp, ToolboxSchema};
use crate::oauth;
use crate::oauth_http::HttpReply;
use crate::telemetry::RequestTelemetry;

use self::tool::{ToolContext, ToolOutcome};

/// MCP protocol versions we can speak, newest first. [`negotiate_protocol_version`]
/// echoes the client's choice when it's in this list, else falls back to
/// `SUPPORTED_PROTOCOL_VERSIONS[0]`.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

const JSONRPC_PARSE_ERROR: i64 = -32700;
const JSONRPC_INVALID_REQUEST: i64 = -32600;
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;

/// `GET /mcp` and `DELETE /mcp`: this transport is stateless and JSON-only, so
/// there is no server-initiated stream to open (`GET`) and no session to end
/// (`DELETE`).
pub fn method_not_allowed() -> HttpReply {
    HttpReply {
        status: 405,
        headers: vec![("Allow".to_string(), "POST".to_string())],
        body: String::new(),
    }
}

/// A 401 pointing the client at the protected-resource metadata (RFC 9728),
/// per the MCP authorization spec. `invalid_token` is added only once a
/// token was actually presented and rejected — its absence is what tells a
/// client "you haven't authenticated yet" versus "your credential is bad".
fn unauthorized(api_base: &str, invalid_token: bool) -> HttpReply {
    let mut value =
        format!("Bearer resource_metadata=\"{api_base}/.well-known/oauth-protected-resource/mcp\"");
    if invalid_token {
        value.push_str(", error=\"invalid_token\"");
    }
    HttpReply {
        status: 401,
        headers: vec![
            ("WWW-Authenticate".to_string(), value),
            ("Content-Type".to_string(), "application/json".to_string()),
        ],
        body: "{}".to_string(),
    }
}

fn service_unavailable() -> HttpReply {
    HttpReply {
        status: 503,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: r#"{"error":"Service temporarily unavailable"}"#.to_string(),
    }
}

fn json_reply(status: u16, value: &Value) -> HttpReply {
    HttpReply {
        status,
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: serde_json::to_string(value)
            .expect("mcp response bodies are plain serde_json::Value and never fail"),
    }
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Echo the client's `protocolVersion` if we speak it, else our latest —
/// matches the MCP spec's negotiation rule for `initialize`.
fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|v| SUPPORTED_PROTOCOL_VERSIONS.iter().find(|&&sv| sv == v))
        .copied()
        .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0])
}

fn initialize_result(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    json!({
        "protocolVersion": negotiate_protocol_version(requested),
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "toolbox",
            "version": crate::environment::GIT_REV,
        },
        "instructions": INSTRUCTIONS,
    })
}

/// Shown to the model at `initialize`. Grows one paragraph per tool family.
const INSTRUCTIONS: &str = "Toolbox is a multi-tenant support-ticket and invoicing system. \
    Every tool acts with the authenticated caller's own permissions: a member sees only the \
    instances they belong to, and a superuser gets admin functions but no implicit access to \
    any instance's tickets or invoices. Call `whoami` first to learn who you are acting as \
    and which instances (and kinds of instance) you can work in. Ticket tools apply to SUPPORT \
    instances only. `reply_to_ticket` and closing or reopening a ticket send email to the \
    customer and cannot be unsent; internal notes and assignment do not. Invoicing tools apply \
    to INVOICING instances only. `send_invoice` and `send_credit_note` email the client and \
    cannot be unsent; no other invoicing tool sends email. `finalize_invoice` and \
    `issue_credit_note` are irreversible: only call them when the user has explicitly asked \
    for that specific invoice or credit note.";

// ── Tool registry ───────────────────────────────────────────────────────────

/// Every tool's `tools/list` entry. Built fresh per request (cheap `json!`
/// values) rather than cached — `tools/list` is not a hot path, and this keeps
/// each schema next to the tool it describes.
fn tool_catalogue() -> Vec<Value> {
    let mut tools = whoami::catalogue();
    tools.extend(tickets::catalogue());
    tools.extend(invoicing::catalogue());
    tools
}

async fn dispatch_tool<A>(ctx: &ToolContext<'_, A>, name: &str, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    if let Some(outcome) = whoami::dispatch(ctx, name, arguments).await {
        return outcome;
    }
    if let Some(outcome) = tickets::dispatch(ctx, name, arguments).await {
        return outcome;
    }
    if let Some(outcome) = invoicing::dispatch(ctx, name, arguments).await {
        return outcome;
    }
    ToolOutcome::error(format!("Unknown tool \"{name}\""))
}

// ── HTTP entry point ────────────────────────────────────────────────────────

/// `POST /mcp`. Handles bearer auth (RFC 9728 challenge on failure) and the
/// whole JSON-RPC 2.0 dispatch, then emits [`RequestTelemetry`] for the
/// request — this endpoint owns its own telemetry (unlike `/oauth/token`,
/// which the two binaries emit around) since almost every branch here
/// (auth, parse errors, tool errors) needs a status/latency recorded, and
/// duplicating that dispatch in both `server.rs` and `bin/lambda/handler.rs`
/// would be all downside.
pub async fn handle_post<A>(
    app: &Arc<A>,
    schema: &ToolboxSchema<A>,
    host: Option<&str>,
    authorization: Option<&str>,
    client_ip: ClientIp,
    body: &[u8],
) -> HttpReply
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    let request_start = Instant::now();
    let api_base = api_base_url(host);
    let resource = format!("{api_base}/mcp");

    let emit = |status: u16, operation_name: &str, auth_info: Option<&AuthInfo>| {
        let (caller_type, caller_id) = auth::caller_info(auth_info);
        RequestTelemetry {
            status,
            operation_name,
            caller_type,
            caller_id: &caller_id,
            latency_ms: request_start.elapsed().as_secs_f64() * 1000.0,
            ..Default::default()
        }
        .emit();
    };

    let Some(token) = authorization.and_then(|h| h.strip_prefix("Bearer ")) else {
        let reply = unauthorized(&api_base, false);
        emit(reply.status, "mcp:auth", None);
        return reply;
    };

    let auth_info = match oauth::verify_access_token(&**app, token, &resource).await {
        Ok(info) => info,
        Err(AuthError::Permanent(msg)) => {
            tracing::info!("mcp: auth rejected: {msg}");
            let reply = unauthorized(&api_base, true);
            emit(reply.status, "mcp:auth", None);
            return reply;
        }
        Err(AuthError::Transient(msg)) => {
            tracing::error!("mcp: transient auth error: {msg}");
            let reply = service_unavailable();
            emit(reply.status, "mcp:auth", None);
            return reply;
        }
    };

    let raw: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            let reply = json_reply(
                400,
                &rpc_error(Value::Null, JSONRPC_PARSE_ERROR, "Parse error"),
            );
            emit(reply.status, "mcp:parse_error", Some(&auth_info));
            return reply;
        }
    };

    if raw.is_array() {
        let reply = json_reply(
            400,
            &rpc_error(
                Value::Null,
                JSONRPC_INVALID_REQUEST,
                "Batch requests are not supported",
            ),
        );
        emit(reply.status, "mcp:batch_rejected", Some(&auth_info));
        return reply;
    }

    let Some(obj) = raw.as_object() else {
        let reply = json_reply(
            400,
            &rpc_error(Value::Null, JSONRPC_INVALID_REQUEST, "Invalid Request"),
        );
        emit(reply.status, "mcp:invalid_request", Some(&auth_info));
        return reply;
    };

    let id = obj.get("id").cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        let reply = json_reply(
            400,
            &rpc_error(
                id.unwrap_or(Value::Null),
                JSONRPC_INVALID_REQUEST,
                "Invalid Request: missing \"method\"",
            ),
        );
        emit(reply.status, "mcp:invalid_request", Some(&auth_info));
        return reply;
    };
    let params = obj.get("params").cloned().unwrap_or_else(|| json!({}));

    // A JSON-RPC *notification* (no "id", or the MCP convention of a
    // `notifications/...` method name) gets no response at all.
    if id.is_none() || method.starts_with("notifications/") {
        emit(202, "mcp:notification", Some(&auth_info));
        return HttpReply {
            status: 202,
            headers: vec![],
            body: String::new(),
        };
    }
    let id = id.expect("checked above");

    let (operation_name, result) = match method {
        "initialize" => (
            "mcp:initialize".to_string(),
            rpc_result(id, initialize_result(&params)),
        ),
        "ping" => ("mcp:ping".to_string(), rpc_result(id, json!({}))),
        "tools/list" => (
            "mcp:tools/list".to_string(),
            rpc_result(id, json!({ "tools": tool_catalogue() })),
        ),
        "tools/call" => {
            let tool_name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let ctx = ToolContext {
                app,
                schema,
                auth_info: &auth_info,
                client_ip: &client_ip,
            };
            let outcome = dispatch_tool(&ctx, &tool_name, &arguments).await;
            // Only known tool names reach telemetry: the name is client-supplied.
            let known = tool_catalogue()
                .iter()
                .any(|t| t["name"].as_str() == Some(tool_name.as_str()));
            let label = if known { tool_name.as_str() } else { "unknown" };
            (
                format!("mcp:tools/call:{label}"),
                rpc_result(id, outcome.into_json()),
            )
        }
        _ => (
            "mcp:unknown_method".to_string(),
            rpc_error(id, JSONRPC_METHOD_NOT_FOUND, "Method not found"),
        ),
    };

    let reply = json_reply(200, &result);
    emit(reply.status, &operation_name, Some(&auth_info));
    reply
}
