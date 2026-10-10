//! GraphQL schema root and cross-cutting infrastructure: schema construction, the
//! per-request DataLoader, and the always-on extension that classifies every
//! resolver error into a machine-readable `extensions.code`.
//!
//! `QueryRoot<A>` and `MutationRoot<A>` (in `query.rs`/`mutations.rs`) are generic
//! over the `App` type, mirroring seslogin: each resolver either reaches the app
//! through `ctx.data_unchecked::<Arc<A>>()` (`QueryRoot`, which holds no field of
//! its own) or through a stored `app: Arc<A>` field (`MutationRoot`, matching
//! seslogin's `self.app.db()` idiom). `A` is fixed to a concrete `MyApp<H, M>` at
//! the two places a schema actually gets built: `build_schema` below, and the
//! Lambda handler's `GraphQlSchema<H, M>` alias.

use async_graphql::dataloader::DataLoader;
use async_graphql::extensions::{
    Extension, ExtensionContext, ExtensionFactory, NextResolve, ResolveInfo,
};
use async_graphql::{EmptySubscription, Schema, ServerError, ServerResult, Value};
use std::sync::Arc;

use crate::app::{App, HasDb, HasMail, HasStorage};
use crate::auth::AuthInfo;
use crate::request_metrics;
use crate::telemetry::{self, OperationKind};

pub mod auth;
pub mod dataloader;
pub mod error;
pub mod finance;
pub mod mutations;
pub mod pagination;
pub mod query;

pub use mutations::MutationRoot;
pub use query::{OAuthAuthorizationRequest, OAuthGrant, PasskeyInfo, QueryRoot, User};

use self::dataloader::DatabaseLoader;

/// DataLoader keys. `Ticket.assignee`/`Ticket.instance` are the first fields
/// that need batching — a page of tickets resolving both per-node would
/// otherwise N+1 on `user`/`instance` `GetItem`s. Named/shaped exactly like
/// seslogin's `graphql/mod.rs` (`UserId(pub ID)`, etc.) — see
/// `graphql::dataloader`'s module doc.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UserId(pub async_graphql::ID);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InstanceId(pub async_graphql::ID);

/// `BillableItem.project` — a page of items resolves its project per node.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProjectId(pub async_graphql::ID);

/// `BillableItem.status`/`BillableItem.invoice` — telling `DRAFT` from
/// `INVOICED` (and resolving `invoice` itself) needs the invoice's current
/// `status`, batched the same way.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InvoiceId(pub async_graphql::ID);

/// Client IP for the current request, threaded from the HTTP layer so resolvers
/// (e.g. a future Turnstile verification) can forward it to external services.
/// `None` when the transport didn't supply one.
#[derive(Clone, Debug, Default)]
pub struct ClientIp(pub Option<String>);

impl ClientIp {
    /// Extract the client IP from an `X-Forwarded-For` header value: the first
    /// comma-separated entry, trimmed. Returns `None` for a missing/empty header.
    pub fn from_forwarded_for(value: Option<&str>) -> Self {
        Self(
            value
                .and_then(|v| v.split(',').next())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        )
    }

    /// Extract the client IP from a `CloudFront-Viewer-Address` header value,
    /// which CloudFront sets (overwriting any viewer-supplied copy) to
    /// `<ip>:<port>` — IPv6 addresses are not bracketed. Returns `None` for a
    /// missing header or anything that doesn't parse as an IP address.
    pub fn from_cloudfront_viewer_address(value: Option<&str>) -> Self {
        Self(
            value
                .and_then(|v| v.trim().rsplit_once(':'))
                .map(|(ip, _port)| ip.trim_start_matches('[').trim_end_matches(']'))
                .filter(|ip| ip.parse::<std::net::IpAddr>().is_ok())
                .map(str::to_owned),
        )
    }
}

/// The schema type for a given `App` implementation. Every binary that builds a
/// schema (`bin/poem`, `bin/poem-local`, `bin/lambda`, `bin/export-schema`) picks
/// a concrete `A = MyApp<H, M>` and gets a concrete `ToolboxSchema<MyApp<H, M>>`.
pub type ToolboxSchema<A> = Schema<QueryRoot<A>, MutationRoot<A>, EmptySubscription>;

/// Always-on extension that records top-level query/mutation field failures. On
/// each error it bumps the per-request failure counter (consumed by the slim EMF
/// metrics) and emits a structured `graphql_error` log line for CloudWatch Logs
/// Insights. It never alters resolver behaviour.
///
/// `pub` (unlike seslogin's `pub(crate)`) so `tests/graphql_error_codes.rs` — an
/// integration test, and therefore a separate crate — can build its own ad hoc
/// schema wired with this same extension, without needing every test to route
/// through the crate's real, still-empty `QueryRoot`.
pub struct RequestMetricsExt;

impl ExtensionFactory for RequestMetricsExt {
    fn create(&self) -> Arc<dyn Extension> {
        Arc::new(RequestMetricsExtImpl)
    }
}

struct RequestMetricsExtImpl;

/// Read back the `code` extension an error already carries (set by `AuthGuard` for
/// its guard-failure arms), if any.
fn existing_code(err: &ServerError) -> Option<String> {
    match err.extensions.as_ref()?.get("code")? {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

#[async_graphql::async_trait::async_trait]
impl Extension for RequestMetricsExtImpl {
    async fn resolve(
        &self,
        ctx: &ExtensionContext<'_>,
        info: ResolveInfo<'_>,
        next: NextResolve<'_>,
    ) -> ServerResult<Option<Value>> {
        let parent_type = info.parent_type;
        let operation_type = match parent_type {
            "QueryRoot" => Some(OperationKind::Query),
            "MutationRoot" => Some(OperationKind::Mutation),
            _ => None,
        };
        let field = info.name;
        let mut res = next.run(ctx, info).await;

        let Err(err) = &mut res else {
            return res;
        };

        // Classify and stamp `extensions.code`, at every depth — not just root
        // fields. A code already set by AuthGuard is left as-is; anything else
        // defaults to INTERNAL.
        let code = existing_code(err).unwrap_or_else(|| {
            let code = error::classify(err).as_str();
            err.extensions
                .get_or_insert_with(Default::default)
                .set("code", code);
            code.to_string()
        });

        // Only observe (metrics + structured log) top-level query/mutation fields,
        // not nested object fields.
        if let Some(operation_type) = operation_type {
            let _ = request_metrics::METRICS.try_with(|m| match operation_type {
                OperationKind::Mutation => m.incr_mutation_failure(),
                _ => m.incr_query_failure(),
            });
            let (caller_type, caller_id) = crate::auth::caller_info(ctx.data_opt::<AuthInfo>());
            telemetry::GraphQlFieldError {
                operation_type,
                field,
                parent_type,
                caller_type,
                caller_id: &caller_id,
                error: &err.message,
                code: &code,
            }
            .emit();
        }
        res
    }
}

pub fn build_schema<A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static>(
    app: Arc<A>,
    webauthn: Arc<webauthn_rs::prelude::Webauthn>,
) -> ToolboxSchema<A> {
    Schema::build(
        QueryRoot::new(),
        MutationRoot { app: app.clone() },
        EmptySubscription,
    )
    .data(app)
    .data(webauthn)
    .extension(RequestMetricsExt)
    .finish()
}

pub fn get_dataloader<A: App + HasDb + Send + Sync + 'static>(
    app: Arc<A>,
) -> DataLoader<DatabaseLoader<A>> {
    DataLoader::new(DatabaseLoader::new(app), request_metrics::metrics_spawner)
}

#[cfg(test)]
mod tests {
    use super::ClientIp;

    fn viewer(value: Option<&str>) -> Option<String> {
        ClientIp::from_cloudfront_viewer_address(value).0
    }

    #[test]
    fn cloudfront_viewer_address_strips_port() {
        assert_eq!(
            viewer(Some("198.51.100.10:46532")).as_deref(),
            Some("198.51.100.10")
        );
        assert_eq!(
            viewer(Some("2001:db8:85a3::8a2e:370:7334:46532")).as_deref(),
            Some("2001:db8:85a3::8a2e:370:7334")
        );
        assert_eq!(
            viewer(Some("[2001:db8::1]:443")).as_deref(),
            Some("2001:db8::1")
        );
    }

    #[test]
    fn cloudfront_viewer_address_rejects_junk() {
        assert_eq!(viewer(None), None);
        assert_eq!(viewer(Some("")), None);
        assert_eq!(viewer(Some("198.51.100.10")), None);
        assert_eq!(viewer(Some("not-an-ip:443")), None);
    }
}
