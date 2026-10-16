//! The `whoami` tool: who the MCP client is acting as, and what they can reach.

use serde_json::{Value, json};

use crate::app::{App, HasDb, HasMail, HasQueue, HasStorage};

use super::tool::{ToolContext, ToolOutcome};

/// `me`'s fields, as one literal. `role` is `OWNER`/`AGENT`; an instance's
/// `kind` (`SUPPORT`/`INVOICING`) is what tells a client which tool families
/// apply there. Deliberately excludes `notificationSettings` (not needed to
/// act) and `passkeys`/`oauthGrants` (never something an AI client needs).
const WHOAMI_QUERY: &str = "query McpWhoami { me { id email name enabled isSuperuser \
     memberships { role instance { id name slug kind } } } }";

pub fn catalogue() -> Vec<Value> {
    vec![json!({
        "name": "whoami",
        "title": "Who am I",
        "description": "Look up the Toolbox user you are acting as: their id, email, name, \
            whether they are a superuser, and every instance they belong to with their role \
            (OWNER or AGENT) and the instance's kind (SUPPORT for a ticket queue, INVOICING \
            for projects and invoices). Use this first to learn who you are acting as and \
            which instances you can work in. A superuser without a membership has no access \
            to any instance's tickets or invoices — only to admin functions.",
        "inputSchema": { "type": "object", "properties": {} },
        "outputSchema": {
            "type": "object",
            "properties": {
                "user": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "email": { "type": "string" },
                        "name": { "type": "string" },
                        "enabled": { "type": "boolean" },
                        "isSuperuser": { "type": "boolean" },
                        "memberships": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "role": { "type": "string", "enum": ["OWNER", "AGENT"] },
                                    "instance": {
                                        "type": "object",
                                        "properties": {
                                            "id": { "type": "string" },
                                            "name": { "type": "string" },
                                            "slug": { "type": "string" },
                                            "kind": { "type": "string", "enum": ["SUPPORT", "INVOICING"] },
                                        },
                                    },
                                },
                            },
                        },
                    },
                },
            },
            "required": ["user"],
        },
        "annotations": { "title": "Who am I", "readOnlyHint": true, "idempotentHint": true },
    })]
}

/// `None` when `name` isn't one of this module's tools.
pub async fn dispatch<A>(
    ctx: &ToolContext<'_, A>,
    name: &str,
    _arguments: &Value,
) -> Option<ToolOutcome>
where
    A: App + HasDb + HasMail + HasStorage + HasQueue + Send + Sync + 'static,
{
    if name != "whoami" {
        return None;
    }
    // `me` is the root field; MCP structured content wants one object-rooted
    // key, so hand it back under `user`, matching every other single-user shape.
    let result = ctx
        .run(WHOAMI_QUERY, json!({}))
        .await
        .map(|mut data| json!({ "user": data["me"].take() }));
    Some(result.into())
}
