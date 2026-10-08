//! Integration tests for the MCP ticket tools against **DynamoDB Local**, through
//! the real schema and `mcp::handle_post` with real `mtoa_` tokens:
//!
//!   - authorization is the site's own: a non-member (and a superuser without a
//!     membership) can't list or read tickets, and a ticket in someone else's
//!     instance is indistinguishable from a missing id;
//!   - `reply_to_ticket` and a close/reopen email the customer; notes, deletes,
//!     no-op status changes, assignment and recipient edits do not;
//!   - assignment and recipient rules (members only, a ticket keeps a requester);
//!   - `list_instance_members` is open to any member, owner or agent, and no one else.
//!
//! # Running this test
//!
//! ```sh
//! make local-up && make local-tables
//! cd api && set -a && . ../local/local.env && set +a
//! cargo test --test mcp_tickets_dynamodb_local
//! ```
//!
//! Every test **skips itself** when no reachable local DynamoDB is configured.

mod common;

use common::mcp::*;
use serde_json::{Value, json};
use toolbox::db;
use toolbox::db::Handler as _;

struct World {
    f: Fixture,
    instance: String,
    owner: String,
    agent: String,
    outsider: String,
    owner_token: String,
    agent_token: String,
    outsider_token: String,
    requester: String,
}

async fn world() -> Option<World> {
    let prefix = local_db_prefix().await?;
    let f = fixture(&prefix).await;
    let (instance, _) = make_instance(&f, "mcp-tix", db::InstanceKind::Support).await;
    let owner = make_user(&f, "mcp-owner", false).await;
    let agent = make_user(&f, "mcp-agent", false).await;
    let outsider = make_user(&f, "mcp-outsider", true).await;
    add_member(&f, &owner, &instance, true).await;
    add_member(&f, &agent, &instance, false).await;
    Some(World {
        owner_token: access_token_for(&f, &owner).await,
        agent_token: access_token_for(&f, &agent).await,
        outsider_token: access_token_for(&f, &outsider).await,
        requester: unique_email("customer"),
        f,
        instance,
        owner,
        agent,
        outsider,
    })
}

async fn make_ticket(w: &World, subject: &str) -> db::Ticket {
    let db = w.f.db();
    let number = db.increment_ticket_counter(&w.instance).await.unwrap();
    let ticket = db
        .create_ticket(
            &w.instance,
            number,
            subject,
            std::slice::from_ref(&w.requester),
            &[],
        )
        .await
        .unwrap();
    db.create_ticket_message(
        &ticket.id,
        db::TicketMessageKind::Inbound,
        None,
        Some(&w.requester),
        &[],
        &[],
        Some("Something is broken"),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    ticket
}

/// Emails sent to the customer — staff notices to members are a separate pipeline.
fn customer_mail(w: &World) -> usize {
    w.f.app
        .mail
        .sent_raw()
        .iter()
        .filter(|m| m.to.iter().chain(m.cc.iter()).any(|a| a == &w.requester))
        .count()
}

#[tokio::test]
async fn list_and_get_follow_membership() {
    let Some(w) = world().await else { return };
    let a = make_ticket(&w, "first").await;
    let b = make_ticket(&w, "second").await;

    let listed = call_tool(
        &w.f,
        &w.agent_token,
        "list_tickets",
        json!({"instanceId": w.instance}),
    )
    .await
    .unwrap();
    let ids: Vec<&str> = listed["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&a.id.as_str()) && ids.contains(&b.id.as_str()));

    // Paging: one at a time, following the cursor.
    let page1 = call_tool(
        &w.f,
        &w.agent_token,
        "list_tickets",
        json!({"instanceId": w.instance, "first": 1}),
    )
    .await
    .unwrap();
    assert_eq!(page1["tickets"].as_array().unwrap().len(), 1);
    assert_eq!(page1["hasNextPage"], true);
    let page2 = call_tool(
        &w.f,
        &w.agent_token,
        "list_tickets",
        json!({"instanceId": w.instance, "first": 1, "after": page1["endCursor"]}),
    )
    .await
    .unwrap();
    assert_ne!(page1["tickets"][0]["id"], page2["tickets"][0]["id"]);

    // The whole conversation, including the customer's message.
    let got = call_tool(
        &w.f,
        &w.agent_token,
        "get_ticket",
        json!({"ticketId": a.id}),
    )
    .await
    .unwrap();
    assert_eq!(got["ticket"]["subject"], "first");
    assert_eq!(got["messages"][0]["kind"], "INBOUND");
    assert_eq!(got["messages"][0]["bodyText"], "Something is broken");

    // A non-member — here a superuser, who gets no implicit ticket access.
    let denied = call_tool(
        &w.f,
        &w.outsider_token,
        "list_tickets",
        json!({"instanceId": w.instance}),
    )
    .await
    .unwrap_err();
    assert!(!denied.is_empty());
    let hidden = call_tool(
        &w.f,
        &w.outsider_token,
        "get_ticket",
        json!({"ticketId": a.id}),
    )
    .await
    .unwrap_err();
    let missing = call_tool(
        &w.f,
        &w.outsider_token,
        "get_ticket",
        json!({"ticketId": "no-such-id"}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        hidden, missing,
        "someone else's ticket must look exactly like a missing one"
    );
}

#[tokio::test]
async fn assigned_to_me_filters_the_queue() {
    let Some(w) = world().await else { return };
    let mine = make_ticket(&w, "mine").await;
    make_ticket(&w, "unassigned").await;
    call_tool(
        &w.f,
        &w.agent_token,
        "assign_ticket",
        json!({"ticketId": mine.id, "userId": w.agent}),
    )
    .await
    .unwrap();

    let queue = call_tool(
        &w.f,
        &w.agent_token,
        "list_tickets",
        json!({"instanceId": w.instance, "assignedToMe": true}),
    )
    .await
    .unwrap();
    let ids: Vec<&str> = queue["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![mine.id.as_str()]);
}

#[tokio::test]
async fn only_a_reply_and_a_close_or_reopen_email_the_customer() {
    let Some(w) = world().await else { return };
    let t = make_ticket(&w, "mail rules").await;
    let id = t.id.as_str();
    let before = customer_mail(&w);

    // Internal note: never customer mail.
    let note = call_tool(
        &w.f,
        &w.agent_token,
        "add_internal_note",
        json!({"ticketId": id, "body": "for staff"}),
    )
    .await
    .unwrap();
    assert_eq!(note["message"]["kind"], "NOTE");
    assert_eq!(customer_mail(&w), before);

    // Empty replies are refused before anything is sent.
    assert!(
        call_tool(
            &w.f,
            &w.agent_token,
            "reply_to_ticket",
            json!({"ticketId": id, "body": "  "})
        )
        .await
        .is_err()
    );
    assert_eq!(customer_mail(&w), before);

    // A reply is emailed.
    let reply = call_tool(
        &w.f,
        &w.agent_token,
        "reply_to_ticket",
        json!({"ticketId": id, "body": "On it"}),
    )
    .await
    .unwrap();
    assert_eq!(reply["message"]["kind"], "REPLY");
    assert_eq!(customer_mail(&w), before + 1);

    // Close and reopen each send a notice; a no-op sends nothing.
    let closed = call_tool(
        &w.f,
        &w.agent_token,
        "set_ticket_status",
        json!({"ticketId": id, "status": "CLOSED"}),
    )
    .await
    .unwrap();
    assert_eq!(closed["ticket"]["status"], "CLOSED");
    assert_eq!(customer_mail(&w), before + 2);
    call_tool(
        &w.f,
        &w.agent_token,
        "set_ticket_status",
        json!({"ticketId": id, "status": "CLOSED"}),
    )
    .await
    .unwrap();
    assert_eq!(customer_mail(&w), before + 2);
    call_tool(
        &w.f,
        &w.agent_token,
        "set_ticket_status",
        json!({"ticketId": id, "status": "OPEN"}),
    )
    .await
    .unwrap();
    assert_eq!(customer_mail(&w), before + 3);

    // Deleting is housekeeping: nothing to the customer.
    let deleted = call_tool(
        &w.f,
        &w.agent_token,
        "set_ticket_status",
        json!({"ticketId": id, "status": "DELETED"}),
    )
    .await
    .unwrap();
    assert_eq!(deleted["ticket"]["status"], "DELETED");
    assert_eq!(customer_mail(&w), before + 3);

    // The reply is in the thread the tool reads back; the note is visible to a member.
    let got = call_tool(&w.f, &w.owner_token, "get_ticket", json!({"ticketId": id}))
        .await
        .unwrap();
    let kinds: Vec<&str> = got["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"NOTE") && kinds.contains(&"REPLY"));
}

#[tokio::test]
async fn assignment_is_members_only_and_never_mails_the_customer() {
    let Some(w) = world().await else { return };
    let t = make_ticket(&w, "assign").await;
    let before = customer_mail(&w);

    let ok = call_tool(
        &w.f,
        &w.owner_token,
        "assign_ticket",
        json!({"ticketId": t.id, "userId": w.agent}),
    )
    .await
    .unwrap();
    assert_eq!(ok["ticket"]["assigneeUserId"], w.agent);

    // Not a member of the instance → refused.
    assert!(
        call_tool(
            &w.f,
            &w.owner_token,
            "assign_ticket",
            json!({"ticketId": t.id, "userId": w.outsider})
        )
        .await
        .is_err()
    );

    // Omitting userId unassigns.
    let un = call_tool(
        &w.f,
        &w.owner_token,
        "assign_ticket",
        json!({"ticketId": t.id}),
    )
    .await
    .unwrap();
    assert!(un["ticket"]["assigneeUserId"].is_null());
    assert_eq!(customer_mail(&w), before);
}

#[tokio::test]
async fn recipient_edits_keep_a_requester_and_send_nothing() {
    let Some(w) = world().await else { return };
    let t = make_ticket(&w, "recipients").await;
    let before = customer_mail(&w);
    let cc = unique_email("cc");

    let added = call_tool(
        &w.f,
        &w.agent_token,
        "update_ticket_recipients",
        json!({"ticketId": t.id, "list": "CC", "action": "ADD", "email": cc}),
    )
    .await
    .unwrap();
    assert!(
        added["ticket"]["ccEmails"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e == &json!(cc))
    );
    let removed = call_tool(
        &w.f,
        &w.agent_token,
        "update_ticket_recipients",
        json!({"ticketId": t.id, "list": "CC", "action": "REMOVE", "email": cc}),
    )
    .await
    .unwrap();
    assert_eq!(removed["ticket"]["ccEmails"], json!([]));

    // The only requester can't be removed.
    let last = call_tool(
        &w.f,
        &w.agent_token,
        "update_ticket_recipients",
        json!({"ticketId": t.id, "list": "REQUESTER", "action": "REMOVE", "email": w.requester}),
    )
    .await;
    assert!(last.is_err());
    // A bad list/action pair is a clear error, not a silent no-op.
    assert!(
        call_tool(
            &w.f,
            &w.agent_token,
            "update_ticket_recipients",
            json!({"ticketId": t.id, "list": "BCC", "action": "ADD", "email": cc}),
        )
        .await
        .is_err()
    );
    assert_eq!(customer_mail(&w), before);
}

#[tokio::test]
async fn member_listing_is_open_to_any_member_only() {
    let Some(w) = world().await else { return };
    for token in [&w.owner_token, &w.agent_token] {
        let view = call_tool(
            &w.f,
            token,
            "list_instance_members",
            json!({"instanceId": w.instance}),
        )
        .await
        .unwrap();
        let members: Vec<&str> = view["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["user"]["id"].as_str().unwrap())
            .collect();
        assert!(members.contains(&w.owner.as_str()) && members.contains(&w.agent.as_str()));
    }

    assert!(
        call_tool(
            &w.f,
            &w.outsider_token,
            "list_instance_members",
            json!({"instanceId": w.instance})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn ticket_tools_are_listed_with_schemas_and_missing_args_are_errors() {
    let Some(w) = world().await else { return };
    let list = rpc(&w.f, &w.agent_token, "tools/list", json!({})).await;
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "list_tickets",
        "get_ticket",
        "reply_to_ticket",
        "add_internal_note",
        "set_ticket_status",
        "assign_ticket",
        "update_ticket_recipients",
        "list_instance_members",
    ] {
        assert!(names.contains(&expected), "{expected}");
    }
    // The tools that email the customer say so in their descriptions.
    let text = |n: &str| -> String {
        list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == n)
            .unwrap()["description"]
            .as_str()
            .unwrap()
            .to_lowercase()
    };
    assert!(text("reply_to_ticket").contains("emails"));
    assert!(text("set_ticket_status").contains("emails"));
    assert!(
        call_tool(
            &w.f,
            &w.agent_token,
            "get_ticket",
            Value::Object(Default::default())
        )
        .await
        .is_err()
    );
}
