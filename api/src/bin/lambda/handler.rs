use std::fmt::Display;
use std::sync::Arc;
use std::time::Instant;

use async_graphql::{
    Request as GraphQlRequest, Response as GraphQlResponse, ServerError as GraphQlError,
};
use http::{HeaderMap, Method, StatusCode};
use lambda_http::request::RequestContext;
use lambda_http::{Body, Error, Request, RequestExt, Response};
use sha2::{Digest, Sha256};

use toolbox::app;
use toolbox::auth::{self, AuthInfo};
use toolbox::db;
use toolbox::graphql;
use toolbox::mail;
use toolbox::queue;
use toolbox::request_metrics::{self, RequestMetrics};
use toolbox::storage;
use toolbox::telemetry::{self, RequestTelemetry};

use crate::errors::{ClientError, ServerError};

type GraphQlSchema<H, M, S, Q> = graphql::ToolboxSchema<app::MyApp<H, M, S, Q>>;

pub struct Handler<
    H: db::Handler + Send + Sync,
    M: mail::Handler + Send + Sync,
    S: storage::Handler + Send + Sync,
    Q: queue::Handler + Send + Sync,
> {
    app: Arc<app::MyApp<H, M, S, Q>>,
    schema: GraphQlSchema<H, M, S, Q>,
}

impl<
    H: db::Handler + Send + Sync + 'static,
    M: mail::Handler + Send + Sync + 'static,
    S: storage::Handler + Send + Sync + 'static,
    Q: queue::Handler + Send + Sync + 'static,
> Handler<H, M, S, Q>
{
    pub fn new(app: Arc<app::MyApp<H, M, S, Q>>, schema: GraphQlSchema<H, M, S, Q>) -> Self {
        Self { app, schema }
    }

    pub async fn handle_request(&self, request: Request) -> Result<Response<Body>, Error> {
        let request_start = Instant::now();
        let headers = request.headers().clone();

        // Client IP, forwarded to Cloudflare as `remoteip` during Turnstile
        // verification. Behind CloudFront the Function URL's source IP is an
        // edge server, so `CloudFront-Viewer-Address` wins when present.
        // CloudFront overwrites that header, but a caller hitting the Function
        // URL directly can still set it — acceptable while the URL is public,
        // since `remoteip` is only a hint to Turnstile. Otherwise the request
        // context's source IP (not spoofable), then the first X-Forwarded-For
        // hop.
        let viewer_ip = graphql::ClientIp::from_cloudfront_viewer_address(
            headers
                .get("cloudfront-viewer-address")
                .and_then(|v| v.to_str().ok()),
        );
        let client_ip = match request.request_context_ref() {
            _ if viewer_ip.0.is_some() => viewer_ip,
            Some(RequestContext::ApiGatewayV2(ctx)) => {
                graphql::ClientIp(ctx.http.source_ip.clone())
            }
            _ => graphql::ClientIp::from_forwarded_for(
                headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
            ),
        };

        // OAuth authorization-server routes are handled entirely outside GraphQL —
        // see `oauth_http.rs`. Everything else keeps behaving exactly as before;
        // this dispatch only ever *adds* paths, it never changes how an existing
        // one is handled.
        let host = headers.get("host").and_then(|v| v.to_str().ok());
        match (request.method().clone(), request.uri().path()) {
            (Method::OPTIONS, path) if toolbox::oauth_http::is_cors_preflight_path(path) => {
                return oauth_reply_response(toolbox::oauth_http::cors_preflight());
            }
            (Method::GET, "/.well-known/oauth-authorization-server") => {
                return oauth_reply_response(toolbox::oauth_http::metadata(host));
            }
            (Method::POST, "/oauth/register") => {
                let body = body_bytes(request);
                return oauth_reply_response(toolbox::oauth_http::register(
                    toolbox::oauth::client_id_key_from_env().as_ref(),
                    &body,
                ));
            }
            (Method::POST, "/oauth/token") => {
                let body = body_bytes(request);
                let reply = toolbox::oauth_http::token(
                    &*self.app,
                    toolbox::oauth::client_id_key_from_env().as_ref(),
                    host,
                    &body,
                )
                .await;
                RequestTelemetry {
                    status: reply.status,
                    operation_name: "oauth:token",
                    latency_ms: request_start.elapsed().as_secs_f64() * 1000.0,
                    ..Default::default()
                }
                .emit();
                return oauth_reply_response(reply);
            }
            (Method::GET, "/.well-known/oauth-protected-resource/mcp")
            | (Method::GET, "/.well-known/oauth-protected-resource") => {
                return oauth_reply_response(toolbox::oauth_http::protected_resource_metadata(
                    host,
                ));
            }
            (Method::GET, "/mcp") | (Method::DELETE, "/mcp") => {
                return oauth_reply_response(toolbox::mcp::method_not_allowed());
            }
            (Method::POST, "/mcp") => {
                let authorization = headers.get("Authorization").and_then(|v| v.to_str().ok());
                let body = body_bytes(request);
                let reply = toolbox::mcp::handle_post(
                    &self.app,
                    &self.schema,
                    host,
                    authorization,
                    client_ip,
                    &body,
                )
                .await;
                return oauth_reply_response(reply);
            }
            _ => {}
        }

        let query = if request.method() == Method::POST {
            self.graphql_request_from_post(request)
        } else if request.method() == Method::GET {
            return graphiql_for_request();
        } else {
            Err(ClientError::MethodNotAllowed)
        };
        let mut query = match query {
            Err(e) => return error_response(StatusCode::BAD_REQUEST, graphql_error(e)),
            Ok(q) => q,
        };

        let auth_opt = match self.try_auth(&headers).await {
            Err(auth::AuthError::Permanent(ref msg)) => {
                emit_auth_failure_telemetry(401, request_start, msg);
                return error_response(
                    StatusCode::UNAUTHORIZED,
                    graphql_error(format!("Authentication error: {}", msg)),
                );
            }
            Err(auth::AuthError::Transient(ref msg)) => {
                tracing::error!("Transient auth error: {}", msg);
                emit_auth_failure_telemetry(503, request_start, msg);
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    graphql_error("Service temporarily unavailable"),
                );
            }
            Ok(opt) => opt,
        };
        let (caller_type, caller_id) = auth::caller_info(auth_opt.as_ref());
        if let Some(auth) = auth_opt {
            query = query.data(auth);
        }

        query = query
            .data(self.app.clone())
            .data(client_ip)
            .data(graphql::get_dataloader(self.app.clone()));

        let operation_context = telemetry::extract_operation_context(&mut query);
        let metrics = Arc::new(RequestMetrics::default());
        let gql_response = request_metrics::METRICS
            .scope(metrics.clone(), self.schema.execute(query))
            .await;
        let gql_error_count = gql_response.errors.len();

        // Serialize first so the emitted telemetry carries the real final HTTP status.
        let result = serde_json::to_string(&gql_response);
        let status: u16 = if result.is_ok() { 200 } else { 500 };

        RequestTelemetry {
            status,
            operation_type: operation_context.operation_type,
            operation_name: operation_context.operation_name(),
            caller_type,
            caller_id: &caller_id,
            latency_ms: request_start.elapsed().as_secs_f64() * 1000.0,
            graphql_error_count: gql_error_count,
            ..Default::default()
        }
        .with_metrics(&metrics)
        .emit();

        match result {
            Ok(response_body) => Response::builder()
                .status(200)
                .body(Body::Text(response_body))
                .map_err(ServerError::from)
                .map_err(Error::from),
            Err(e) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                graphql_error(ServerError::from(e)),
            ),
        }
    }

    /// Parse a POST body into a GraphQL request. Also computes the request body's
    /// hex SHA-256 for the debug log — there is no signed-header scheme in
    /// Toolbox to bind it into (unlike seslogin's kiosk `SLKey` auth), but
    /// logging it costs nothing and gives an audit trail for "was this exact body
    /// received".
    fn graphql_request_from_post(&self, request: Request) -> Result<GraphQlRequest, ClientError> {
        match request.into_body() {
            Body::Text(text) => {
                tracing::debug!(body_sha256 = %body_sha256_hex(text.as_bytes()), "received request body");
                serde_json::from_str::<GraphQlRequest>(&text).map_err(ClientError::from)
            }
            Body::Binary(binary) => {
                tracing::debug!(body_sha256 = %body_sha256_hex(&binary), "received request body");
                serde_json::from_slice::<GraphQlRequest>(&binary).map_err(ClientError::from)
            }
            _ => Err(ClientError::EmptyBody),
        }
    }

    async fn try_auth(&self, headers: &HeaderMap) -> Result<Option<AuthInfo>, auth::AuthError> {
        let auth_header = headers
            .get("Authorization")
            .and_then(|value| value.to_str().ok());
        match auth::verify_authorization_header(&*self.app, auth_header).await {
            Some(res) => res.map(Some),
            None => Ok(None),
        }
    }
}

fn body_sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Emits request telemetry for an auth failure that short-circuits before GraphQL
/// execution. `status` is 401 (permanent / unauthenticated) or 503 (transient
/// backend error). `auth_error` is the reason auth failed, recorded in the
/// `api_request` log.
fn emit_auth_failure_telemetry(status: u16, request_start: Instant, auth_error: &str) {
    RequestTelemetry {
        status,
        latency_ms: request_start.elapsed().as_secs_f64() * 1000.0,
        auth_error,
        ..Default::default()
    }
    .emit();
}

fn graphql_error(message: impl Display) -> String {
    let message = message.to_string();
    let response = GraphQlResponse::from_errors(vec![GraphQlError::new(message, None)]);
    serde_json::to_string(&response).expect("Valid response should never fail to serialize")
}

fn error_response(status: StatusCode, body: String) -> Result<Response<Body>, Error> {
    Ok(Response::builder().status(status).body(Body::Text(body))?)
}

/// Extract raw body bytes from a Lambda request, for the two OAuth routes that
/// take their own body format (JSON, form-urlencoded) rather than GraphQL's.
/// An empty/missing body becomes an empty slice — the callers all treat that as
/// "no parameters", which is the right failure mode for a malformed or absent body.
fn body_bytes(request: Request) -> Vec<u8> {
    match request.into_body() {
        Body::Text(text) => text.into_bytes(),
        Body::Binary(binary) => binary,
        _ => Vec::new(),
    }
}

/// Turn a framework-agnostic [`toolbox::oauth_http::HttpReply`] into a Lambda response.
fn oauth_reply_response(reply: toolbox::oauth_http::HttpReply) -> Result<Response<Body>, Error> {
    let mut builder = Response::builder().status(reply.status);
    for (name, value) in &reply.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
        .body(Body::Text(reply.body))
        .map_err(ServerError::from)
        .map_err(Error::from)
}

fn graphiql_for_request() -> Result<Response<Body>, Error> {
    // GraphiQL resolves its endpoint against the page's origin, not its path, so
    // name the path explicitly: behind CloudFront the API lives at `/graphql`,
    // and `/` is the web app. The raw Function URL ignores the path either way.
    let html = async_graphql::http::GraphiQLSource::build()
        .endpoint("/graphql")
        .finish();
    Response::builder()
        .status(200)
        .header("Content-Type", "text/html")
        .body(Body::Text(html))
        .map_err(Error::from)
}
