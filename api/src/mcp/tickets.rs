//! MCP tools for support tickets. Every tool runs one fixed GraphQL document as
//! the caller (see [`super::tool`]), so ticket authorization is exactly the
//! site's: a ticket in an instance you don't belong to is `null`/not found —
//! identically to a nonexistent id — and a superuser without a membership sees
//! no tickets at all. Nothing here reimplements a check.
//!
//! **Which of these send email?** Exactly what the same mutation sends over
//! GraphQL (see CLAUDE.md's "Outbound mail" house rule) — the tool descriptions
//! say so, because an AI client should know before it acts:
//!   - `reply_to_ticket` emails the requesters and CCs, always;
//!   - `set_ticket_status` emails them a brief notice on a close or a reopen
//!     (not on a delete);
//!   - notes, assignment and recipient edits send no customer mail.
//!
//! Staff notifications (`staff_notify`) fire exactly as they do for the same
//! action taken in the web app, with the MCP caller as the actor.
//!
//! **Not exposed:** attachments (an MCP client has no way to upload one, and
//! `downloadUrl` is a presigned link — filenames and sizes are listed so a client
//! can tell an attachment exists, but the content stays in the web app), the raw
//! HTML body (plain text only), and hard-to-undo admin functions.

use serde_json::{Value, json};

use crate::app::{App, HasDb, HasMail, HasStorage};

use super::tool::{ToolContext, ToolOutcome, missing_argument};

const TICKET_FIELDS: &str = "id instanceId number subject status requesterEmails ccEmails \
     assigneeUserId createdAt updatedAt lastActivityAt hasAttachments";

const MESSAGE_FIELDS: &str = "id kind authorUserId fromEmail toEmails ccEmails bodyText \
     createdAt attachments { filename contentType size }";

const DEFAULT_PAGE_SIZE: i64 = 20;
const MAX_PAGE_SIZE: i64 = 50;

fn ticket_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "instanceId": { "type": "string" },
            "number": { "type": "integer", "description": "The per-instance ticket number shown to people, e.g. 42 in [#acme-42]." },
            "subject": { "type": "string" },
            "status": { "type": "string", "enum": ["OPEN", "CLOSED", "DELETED"] },
            "requesterEmails": { "type": "array", "items": { "type": "string" } },
            "ccEmails": { "type": "array", "items": { "type": "string" } },
            "assigneeUserId": { "type": ["string", "null"] },
            "createdAt": { "type": "integer", "description": "Unix seconds." },
            "updatedAt": { "type": "integer", "description": "Unix seconds." },
            "lastActivityAt": { "type": "integer", "description": "Unix seconds." },
            "hasAttachments": { "type": "boolean" },
        },
    })
}

fn message_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "kind": {
                "type": "string",
                "enum": ["INBOUND", "REPLY", "NOTE", "SYSTEM"],
                "description": "INBOUND: a customer's email. REPLY: an agent's reply, emailed to the customer. NOTE: internal, never shown to the customer. SYSTEM: an automatic notice.",
            },
            "authorUserId": { "type": ["string", "null"] },
            "fromEmail": { "type": ["string", "null"] },
            "toEmails": { "type": "array", "items": { "type": "string" } },
            "ccEmails": { "type": "array", "items": { "type": "string" } },
            "bodyText": { "type": ["string", "null"] },
            "createdAt": { "type": "integer", "description": "Unix seconds." },
            "attachments": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "filename": { "type": "string" },
                        "contentType": { "type": "string" },
                        "size": { "type": "integer" },
                    },
                },
            },
        },
    })
}

fn ticket_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "ticket": ticket_schema() },
        "required": ["ticket"],
    })
}

fn ticket_id_input() -> Value {
    json!({
        "type": "object",
        "properties": { "ticketId": { "type": "string", "description": "The ticket's id (not its number)." } },
        "required": ["ticketId"],
    })
}

pub fn catalogue() -> Vec<Value> {
    vec![
        json!({
            "name": "list_tickets",
            "title": "List tickets",
            "description": "List tickets in a support instance you are a member of, newest \
                activity first. Defaults to OPEN. Use `assignedToMe` for your own queue, or \
                `assignedTo` with another member's user id. Results are paged: pass the \
                returned `endCursor` as `after` for the next page. DELETED is owner-only. \
                Get `instanceId` from `whoami` (only instances of kind SUPPORT have tickets).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instanceId": { "type": "string" },
                    "status": { "type": "string", "enum": ["OPEN", "CLOSED", "ALL", "DELETED"], "default": "OPEN" },
                    "assignedToMe": { "type": "boolean", "description": "Only tickets assigned to you." },
                    "assignedTo": { "type": "string", "description": "Only tickets assigned to this user id. Ignored if assignedToMe is true." },
                    "first": { "type": "integer", "minimum": 1, "maximum": MAX_PAGE_SIZE, "default": DEFAULT_PAGE_SIZE },
                    "after": { "type": "string", "description": "`endCursor` from the previous page." },
                },
                "required": ["instanceId"],
            },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "tickets": { "type": "array", "items": ticket_schema() },
                    "hasNextPage": { "type": "boolean" },
                    "endCursor": { "type": ["string", "null"] },
                },
                "required": ["tickets", "hasNextPage"],
            },
            "annotations": { "title": "List tickets", "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "get_ticket",
            "title": "Get ticket",
            "description": "Fetch one ticket with its whole conversation, oldest message first \
                (customer emails, agent replies, internal notes and system notices — each \
                labelled by `kind`). Returns an error if the ticket doesn't exist or you \
                aren't a member of its instance (the two are indistinguishable).",
            "inputSchema": ticket_id_input(),
            "outputSchema": {
                "type": "object",
                "properties": {
                    "ticket": ticket_schema(),
                    "messages": { "type": "array", "items": message_schema() },
                },
                "required": ["ticket", "messages"],
            },
            "annotations": { "title": "Get ticket", "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "reply_to_ticket",
            "title": "Reply to ticket",
            "description": "Send a reply on a ticket. **This emails the ticket's requesters and \
                CCs** with your message as written, so it is customer-facing and cannot be \
                unsent — use `add_internal_note` for anything internal. Plain text. Fails, \
                without recording anything as delivered, if the email can't be sent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ticketId": { "type": "string" },
                    "body": { "type": "string", "description": "The reply, plain text." },
                },
                "required": ["ticketId", "body"],
            },
            "outputSchema": { "type": "object", "properties": { "message": message_schema() }, "required": ["message"] },
            "annotations": { "title": "Reply to ticket", "destructiveHint": false, "idempotentHint": false, "openWorldHint": true },
        }),
        json!({
            "name": "add_internal_note",
            "title": "Add internal note",
            "description": "Add an internal note to a ticket. Notes are visible to members of the \
                instance only — never emailed to, or shown to, the customer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ticketId": { "type": "string" },
                    "body": { "type": "string" },
                },
                "required": ["ticketId", "body"],
            },
            "outputSchema": { "type": "object", "properties": { "message": message_schema() }, "required": ["message"] },
            "annotations": { "title": "Add internal note", "idempotentHint": false },
        }),
        json!({
            "name": "set_ticket_status",
            "title": "Set ticket status",
            "description": "Close, reopen or delete a ticket. **Closing or reopening emails the \
                requesters and CCs a brief notice**; deleting (a soft delete, spam cleanup) \
                sends nothing, and restoring a deleted ticket to OPEN counts as a reopen. \
                Setting the status a ticket already has changes nothing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ticketId": { "type": "string" },
                    "status": { "type": "string", "enum": ["OPEN", "CLOSED", "DELETED"] },
                },
                "required": ["ticketId", "status"],
            },
            "outputSchema": ticket_output_schema(),
            "annotations": { "title": "Set ticket status", "idempotentHint": true, "openWorldHint": true },
        }),
        json!({
            "name": "assign_ticket",
            "title": "Assign ticket",
            "description": "Assign a ticket to a member of its instance, or unassign it by \
                omitting `userId`. Sends no customer email; the new (and any previous) \
                assignee may get a staff notification per their settings. To assign to \
                yourself, use your own id from `whoami`; `list_instance_members` finds \
                your colleagues.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ticketId": { "type": "string" },
                    "userId": { "type": "string", "description": "Omit to unassign." },
                },
                "required": ["ticketId"],
            },
            "outputSchema": ticket_output_schema(),
            "annotations": { "title": "Assign ticket", "idempotentHint": true },
        }),
        json!({
            "name": "update_ticket_recipients",
            "title": "Add or remove a ticket requester or CC",
            "description": "Add or remove one email address on a ticket's requester list or CC \
                list. Requesters and CCs receive every future reply, so double-check the \
                address. A ticket must keep at least one requester. Sends no email itself.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ticketId": { "type": "string" },
                    "list": { "type": "string", "enum": ["REQUESTER", "CC"] },
                    "action": { "type": "string", "enum": ["ADD", "REMOVE"] },
                    "email": { "type": "string" },
                },
                "required": ["ticketId", "list", "action", "email"],
            },
            "outputSchema": ticket_output_schema(),
            "annotations": { "title": "Update ticket recipients", "idempotentHint": true },
        }),
        json!({
            "name": "list_instance_members",
            "title": "List instance members",
            "description": "List the members of an instance you belong to (id, email, name, \
                role) — for finding a user id to assign a ticket to. Any member, owner or \
                agent, can list their own instance's members.",
            "inputSchema": {
                "type": "object",
                "properties": { "instanceId": { "type": "string" } },
                "required": ["instanceId"],
            },
            "outputSchema": {
                "type": "object",
                "properties": {
                    "members": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "role": { "type": "string", "enum": ["OWNER", "AGENT"] },
                                "user": {
                                    "type": "object",
                                    "properties": {
                                        "id": { "type": "string" },
                                        "email": { "type": "string" },
                                        "name": { "type": "string" },
                                    },
                                },
                            },
                        },
                    },
                },
                "required": ["members"],
            },
            "annotations": { "title": "List instance members", "readOnlyHint": true, "idempotentHint": true },
        }),
    ]
}

fn string_arg<'a>(arguments: &'a Value, name: &str) -> Option<&'a str> {
    arguments.get(name).and_then(Value::as_str)
}

/// `None` when `name` isn't one of this module's tools.
pub async fn dispatch<A>(
    ctx: &ToolContext<'_, A>,
    name: &str,
    arguments: &Value,
) -> Option<ToolOutcome>
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    Some(match name {
        "list_tickets" => list_tickets(ctx, arguments).await,
        "get_ticket" => get_ticket(ctx, arguments).await,
        "reply_to_ticket" => {
            let (Some(id), Some(body)) = (
                string_arg(arguments, "ticketId"),
                string_arg(arguments, "body"),
            ) else {
                return Some(missing_argument("ticketId/body"));
            };
            let doc = format!(
                "mutation McpReply($ticketId: ID!, $body: String!) {{ \
                 replyToTicket(ticketId: $ticketId, body: $body) {{ {MESSAGE_FIELDS} }} }}"
            );
            rename(
                ctx.run(&doc, json!({"ticketId": id, "body": body})).await,
                "replyToTicket",
                "message",
            )
        }
        "add_internal_note" => {
            let (Some(id), Some(body)) = (
                string_arg(arguments, "ticketId"),
                string_arg(arguments, "body"),
            ) else {
                return Some(missing_argument("ticketId/body"));
            };
            let doc = format!(
                "mutation McpNote($ticketId: ID!, $body: String!) {{ \
                 addInternalNote(ticketId: $ticketId, body: $body) {{ {MESSAGE_FIELDS} }} }}"
            );
            rename(
                ctx.run(&doc, json!({"ticketId": id, "body": body})).await,
                "addInternalNote",
                "message",
            )
        }
        "set_ticket_status" => {
            let (Some(id), Some(status)) = (
                string_arg(arguments, "ticketId"),
                string_arg(arguments, "status"),
            ) else {
                return Some(missing_argument("ticketId/status"));
            };
            let doc = format!(
                "mutation McpStatus($ticketId: ID!, $status: TicketStatusType!) {{ \
                 setTicketStatus(ticketId: $ticketId, status: $status) {{ {TICKET_FIELDS} }} }}"
            );
            rename(
                ctx.run(&doc, json!({"ticketId": id, "status": status}))
                    .await,
                "setTicketStatus",
                "ticket",
            )
        }
        "assign_ticket" => {
            let Some(id) = string_arg(arguments, "ticketId") else {
                return Some(missing_argument("ticketId"));
            };
            let doc = format!(
                "mutation McpAssign($ticketId: ID!, $userId: ID) {{ \
                 assignTicket(ticketId: $ticketId, userId: $userId) {{ {TICKET_FIELDS} }} }}"
            );
            let user_id = string_arg(arguments, "userId");
            rename(
                ctx.run(&doc, json!({"ticketId": id, "userId": user_id}))
                    .await,
                "assignTicket",
                "ticket",
            )
        }
        "update_ticket_recipients" => update_recipients(ctx, arguments).await,
        "list_instance_members" => list_instance_members(ctx, arguments).await,
        _ => return None,
    })
}

/// Move a mutation's root field to the tool's documented output key, so every
/// tool's `structuredContent` matches its `outputSchema`.
fn rename(result: Result<Value, Vec<String>>, from: &str, to: &str) -> ToolOutcome {
    result
        .map(|mut data| json!({ to: data[from].take() }))
        .into()
}

async fn list_tickets<A>(ctx: &ToolContext<'_, A>, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    let Some(instance_id) = string_arg(arguments, "instanceId") else {
        return missing_argument("instanceId");
    };
    let status = string_arg(arguments, "status").unwrap_or("OPEN");
    let first = arguments
        .get("first")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);

    let assigned_to: Option<String> =
        if arguments.get("assignedToMe").and_then(Value::as_bool) == Some(true) {
            match ctx.run("query McpMe { me { id } }", json!({})).await {
                Ok(data) => data["me"]["id"].as_str().map(str::to_string),
                Err(e) => return ToolOutcome::Error(e),
            }
        } else {
            string_arg(arguments, "assignedTo").map(str::to_string)
        };

    let doc = format!(
        "query McpTickets($instanceId: ID!, $status: TicketStatusFilterType!, $assignedTo: ID, \
         $first: Int!, $after: String) {{ tickets(instanceId: $instanceId, status: $status, \
         assignedTo: $assignedTo, first: $first, after: $after) {{ \
         edges {{ node {{ {TICKET_FIELDS} }} }} pageInfo {{ hasNextPage endCursor }} }} }}"
    );
    let result = ctx
        .run(
            &doc,
            json!({
                "instanceId": instance_id, "status": status, "assignedTo": assigned_to,
                "first": first, "after": string_arg(arguments, "after"),
            }),
        )
        .await;
    result
        .map(|data| {
            let conn = &data["tickets"];
            let tickets: Vec<Value> = conn["edges"]
                .as_array()
                .map(|edges| edges.iter().map(|e| e["node"].clone()).collect())
                .unwrap_or_default();
            json!({
                "tickets": tickets,
                "hasNextPage": conn["pageInfo"]["hasNextPage"],
                "endCursor": conn["pageInfo"]["endCursor"],
            })
        })
        .into()
}

async fn get_ticket<A>(ctx: &ToolContext<'_, A>, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    let Some(id) = string_arg(arguments, "ticketId") else {
        return missing_argument("ticketId");
    };
    let doc = format!(
        "query McpTicket($id: ID!) {{ ticket(id: $id) {{ {TICKET_FIELDS} \
         messages {{ {MESSAGE_FIELDS} }} }} }}"
    );
    match ctx.run(&doc, json!({"id": id})).await {
        Err(e) => ToolOutcome::Error(e),
        // `ticket` is `null` for a missing id and a non-member alike.
        Ok(data) if data["ticket"].is_null() => {
            ToolOutcome::error("Ticket not found (or you are not a member of its instance)")
        }
        Ok(mut data) => {
            let mut ticket = data["ticket"].take();
            let messages = ticket["messages"].take();
            ToolOutcome::Ok(json!({ "ticket": ticket, "messages": messages }))
        }
    }
}

async fn update_recipients<A>(ctx: &ToolContext<'_, A>, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    let (Some(id), Some(list), Some(action), Some(email)) = (
        string_arg(arguments, "ticketId"),
        string_arg(arguments, "list"),
        string_arg(arguments, "action"),
        string_arg(arguments, "email"),
    ) else {
        return missing_argument("ticketId/list/action/email");
    };
    let field = match (list, action) {
        ("REQUESTER", "ADD") => "addTicketRequester",
        ("REQUESTER", "REMOVE") => "removeTicketRequester",
        ("CC", "ADD") => "addTicketCc",
        ("CC", "REMOVE") => "removeTicketCc",
        _ => {
            return ToolOutcome::error("`list` must be REQUESTER or CC and `action` ADD or REMOVE");
        }
    };
    let doc = format!(
        "mutation McpRecipients($ticketId: ID!, $email: String!) {{ \
         {field}(ticketId: $ticketId, email: $email) {{ {TICKET_FIELDS} }} }}"
    );
    rename(
        ctx.run(&doc, json!({"ticketId": id, "email": email})).await,
        field,
        "ticket",
    )
}

async fn list_instance_members<A>(ctx: &ToolContext<'_, A>, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    let Some(instance_id) = string_arg(arguments, "instanceId") else {
        return missing_argument("instanceId");
    };
    // `instance(slug:)` is the member-facing lookup, so resolve the slug from the
    // caller's own memberships first — an id for an instance they don't belong to
    // finds nothing, the same as it would over GraphQL.
    let memberships = match ctx
        .run(
            "query McpSlug { me { memberships { instance { id slug } } } }",
            json!({}),
        )
        .await
    {
        Ok(data) => data,
        Err(e) => return ToolOutcome::Error(e),
    };
    let slug = memberships["me"]["memberships"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["instance"]["id"].as_str() == Some(instance_id))
        .and_then(|m| m["instance"]["slug"].as_str());
    let Some(slug) = slug else {
        return ToolOutcome::error("Instance not found (or you are not a member of it)");
    };
    let result = ctx
        .run(
            "query McpMembers($slug: String!) { instance(slug: $slug) { \
             members { role user { id email name } } } }",
            json!({"slug": slug}),
        )
        .await;
    match result {
        Err(e) => ToolOutcome::Error(e),
        Ok(mut data) => ToolOutcome::Ok(json!({ "members": data["instance"]["members"].take() })),
    }
}
