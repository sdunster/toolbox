//! Shared plumbing for MCP tools: how a tool runs a GraphQL document as its
//! caller, and how its result is rendered for the protocol.
//!
//! **Same permissions, by construction.** No tool touches the database
//! directly. Each one runs a fixed GraphQL document against the in-process
//! schema with the caller's [`AuthInfo`] attached exactly as the GraphQL
//! endpoint builds its own request (`server.rs::index` /
//! `bin/lambda/handler.rs`) — `AuthInfo`, the app, `ClientIp`, the dataloader —
//! so every existing authorization guard (membership checks, the superuser
//! boundary, `NOT_FOUND` for a ticket you can't see, …) applies unchanged. A tool
//! never widens what its caller could already do over GraphQL; it can only
//! narrow it.

use std::sync::Arc;

use async_graphql::Variables;
use serde_json::{Value, json};

use crate::app::{App, HasDb, HasMail, HasQueue, HasStorage};
use crate::auth::AuthInfo;
use crate::graphql::{self, ClientIp, ToolboxSchema};

/// Everything a tool needs to run GraphQL as the authenticated caller.
pub struct ToolContext<'a, A: App + HasDb + HasMail + HasStorage + HasQueue + Send + Sync + 'static>
{
    pub app: &'a Arc<A>,
    pub schema: &'a ToolboxSchema<A>,
    pub auth_info: &'a AuthInfo,
    pub client_ip: &'a ClientIp,
}

impl<A> ToolContext<'_, A>
where
    A: App + HasDb + HasMail + HasStorage + HasQueue + Send + Sync + 'static,
{
    /// Run a fixed GraphQL document as the caller (see the module doc). Returns
    /// the response's `data` as plain JSON on success, or every error message on
    /// failure (a guard rejection, a not-found, a validation failure — whatever
    /// GraphQL raised).
    pub async fn run(&self, document: &str, variables: Value) -> Result<Value, Vec<String>> {
        let auth_info: AuthInfo = (*self.auth_info).clone();
        let app: Arc<A> = Arc::clone(self.app);
        let client_ip: ClientIp = (*self.client_ip).clone();
        let request = async_graphql::Request::new(document.to_string())
            .variables(Variables::from_json(variables))
            .data(auth_info)
            .data(graphql::get_dataloader(app.clone()))
            .data(app)
            .data(client_ip);
        let response = self.schema.execute(request).await;
        if !response.errors.is_empty() {
            return Err(response.errors.iter().map(|e| e.message.clone()).collect());
        }
        Ok(response.data.into_json().unwrap_or(Value::Null))
    }
}

/// The result of running a tool: either its data (rendered as both
/// `structuredContent` and a pretty-printed text block) or a list of error
/// messages (rendered as `isError: true` text) — see [`Self::into_json`].
pub enum ToolOutcome {
    Ok(Value),
    Error(Vec<String>),
}

impl ToolOutcome {
    pub fn into_json(self) -> Value {
        match self {
            ToolOutcome::Ok(data) => {
                let text = serde_json::to_string_pretty(&data).unwrap_or_else(|_| data.to_string());
                json!({
                    "content": [{ "type": "text", "text": text }],
                    "structuredContent": data,
                    "isError": false,
                })
            }
            ToolOutcome::Error(messages) => json!({
                "content": [{ "type": "text", "text": messages.join("\n") }],
                "isError": true,
            }),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        ToolOutcome::Error(vec![message.into()])
    }
}

impl From<Result<Value, Vec<String>>> for ToolOutcome {
    fn from(result: Result<Value, Vec<String>>) -> Self {
        match result {
            Ok(v) => ToolOutcome::Ok(v),
            Err(e) => ToolOutcome::Error(e),
        }
    }
}

pub fn missing_argument(name: &str) -> ToolOutcome {
    ToolOutcome::error(format!("Missing or invalid \"{name}\" argument"))
}
