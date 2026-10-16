//! Integration tests for outbound mail (step 6) against **DynamoDB Local**
//! and the **mock** mailer — not real SES. Proves the full shape end to end,
//! through the real GraphQL mutations, in a way none of the pure unit tests
//! in `src/outbound.rs` can: that `replyToTicket` actually calls
//! `mail::Handler::send_raw` exactly once, addressed correctly, carrying the
//! `+t{token}` reply address and the tagged subject; that `addInternalNote`
//! calls it zero times; that `setTicketStatus` sends a notice on close; and
//! that `submitTicket` sends an acknowledgement.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test outbound_mail_dynamodb_local
//! ```
//!
//! Like the other `*_dynamodb_local.rs` files, every test here **skips
//! itself** when no reachable local DynamoDB is configured, so `cargo test`
//! and CI stay green with no local stack running.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::json;
use toolbox::app;
use toolbox::auth::{AuthInfo, Membership};
use toolbox::db;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::graphql;
use toolbox::mockmail;
use toolbox::mockqueue;
use toolbox::mockstorage;

type TestApp =
    app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage, mockqueue::Queue>;
type TestSchema = graphql::ToolboxSchema<TestApp>;

/// See `tests/auth_dynamodb_local.rs`'s identically-named helper for the
/// full rationale.
async fn local_db_prefix() -> Option<String> {
    let endpoint = toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;

    let client = toolbox::local_dev::dynamodb_client().await;
    if client.list_tables().send().await.is_err() {
        eprintln!(
            "outbound_mail_dynamodb_local: {endpoint} is configured but not reachable — skipping. \
             Run `make local-up && make local-tables` first."
        );
        return None;
    }

    Some(prefix)
}

macro_rules! require_local_db {
    () => {
        match local_db_prefix().await {
            Some(prefix) => prefix,
            None => {
                eprintln!(
                    "outbound_mail_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set to \
                     a reachable local DynamoDB — skipping. See this file's header for how to run it."
                );
                return;
            }
        }
    };
}

fn unique_id(label: &str) -> String {
    format!("{label}-{}", nanoid::nanoid!(8))
}

fn unique_email(label: &str) -> String {
    format!("{label}-{}@example.com", nanoid::nanoid!(8))
}

fn expect_data(response: &Response, path: &str) -> serde_json::Value {
    assert!(
        response.errors.is_empty(),
        "unexpected GraphQL errors on {path}: {:?}",
        response.errors
    );
    let data = serde_json::to_value(&response.data).expect("response data serializes");
    let mut cur = &data;
    for segment in path.split('.') {
        cur = cur
            .get(segment)
            .unwrap_or_else(|| panic!("response data missing `{segment}` (path {path}): {data}"));
    }
    cur.clone()
}

fn user_auth(user_id: &str, instance_id: &str, is_owner: bool) -> AuthInfo {
    AuthInfo::User {
        id: user_id.to_string(),
        memberships: vec![Membership {
            instance_id: instance_id.to_string(),
            is_owner,
        }],
        is_superuser: false,
        token_id: None,
        grant_id: None,
    }
}

fn requester_auth(email: &str, instance_id: &str) -> AuthInfo {
    AuthInfo::Requester {
        email: email.to_string(),
        instance_id: instance_id.to_string(),
    }
}

/// An instance with one exact inbound address, one owner user, and a
/// membership — every test's starting point. Returns
/// `(instance, owner_user_id, inbound_address)`.
async fn setup_instance(db: &dynamodb::Handler, label: &str) -> (db::Instance, String, String) {
    let instance = db
        .create_instance(
            &format!("{label} instance"),
            &unique_id(&format!("{label}-slug")),
            &format!("{label} Support"),
            "Thanks,\nThe Support Team",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    let address = format!("support-{}@{label}.toolbox.test", nanoid::nanoid!(6));
    db.create_inbound_address(&address, &instance.id, db::AddressKind::Exact)
        .await
        .expect("create_inbound_address");
    let owner = db
        .create_user(&unique_email(&format!("{label}-owner")), "Owner")
        .await
        .expect("create_user");
    db.create_membership(&owner.id, &instance.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership");
    (instance, owner.id, address)
}

/// Create a ticket directly through the DB layer, with a requester and one
/// CC, returning the created `db::Ticket`.
async fn make_ticket(
    db: &dynamodb::Handler,
    instance_id: &str,
    requester_email: &str,
    cc_email: &str,
) -> db::Ticket {
    let number = db
        .increment_ticket_counter(instance_id)
        .await
        .expect("increment_ticket_counter");
    db.create_ticket(
        instance_id,
        number,
        "Where is my order?",
        std::slice::from_ref(&requester_email.to_string()),
        std::slice::from_ref(&cc_email.to_string()),
    )
    .await
    .expect("create_ticket")
}

fn build_app_and_schema(db: dynamodb::Handler) -> (Arc<TestApp>, TestSchema) {
    let my_app = Arc::new(app::new(
        db,
        mockmail::Handler::new(),
        mockstorage::Storage::new(),
        mockqueue::Queue::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(my_app.clone(), webauthn);
    (my_app, schema)
}

const REPLY_MUTATION: &str = r#"
    mutation($ticketId: ID!, $body: String!) {
        replyToTicket(ticketId: $ticketId, body: $body) { id kind rfcMessageId }
    }
"#;

const ADD_NOTE_MUTATION: &str = r#"
    mutation($ticketId: ID!, $body: String!) {
        addInternalNote(ticketId: $ticketId, body: $body) { id kind }
    }
"#;

const SET_STATUS_MUTATION: &str = r#"
    mutation($ticketId: ID!, $status: TicketStatusType!) {
        setTicketStatus(ticketId: $ticketId, status: $status) { id status }
    }
"#;

const SUBMIT_TICKET_MUTATION: &str = r#"
    mutation($subject: String!, $body: String!) {
        submitTicket(subject: $subject, body: $body) { id number }
    }
"#;

#[tokio::test]
async fn reply_sends_exactly_one_message_addressed_and_tagged_correctly() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id, _address) = setup_instance(&db, "reply").await;
    let requester_email = unique_email("requester");
    let cc_email = unique_email("cc");
    let ticket = make_ticket(&db, &instance.id, &requester_email, &cc_email).await;

    let (my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(REPLY_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "body": "We're looking into it.",
                })))
                .data(user_auth(&owner_id, &instance.id, true)),
        )
        .await;
    let rfc_message_id = expect_data(&response, "replyToTicket.rfcMessageId");
    assert!(
        rfc_message_id.as_str().is_some_and(|s| !s.is_empty()),
        "the reply must be stamped with an rfcMessageId once sent: {rfc_message_id:?}"
    );

    let sent = my_app.mail.sent_raw();
    assert_eq!(sent.len(), 1, "exactly one message must be sent: {sent:?}");
    let msg = &sent[0];
    assert_eq!(msg.to, vec![requester_email.clone()]);
    assert_eq!(msg.cc, vec![cc_email.clone()]);

    let raw = String::from_utf8_lossy(&msg.raw);
    let expected_tag = format!("[#{}-{}]", instance.slug, ticket.number);
    assert!(
        raw.contains(&expected_tag),
        "subject must carry {expected_tag:?}: {raw}"
    );
    assert!(
        raw.contains(&format!("+t{}.{}@", ticket.id, ticket.reply_token)),
        "Reply-To must carry the +t{{ticket_id}}.{{token}} address (see outbound::reply_to_address): {raw}"
    );
    assert!(raw.contains("X-Toolbox-Loop: 1"), "{raw}");
}

#[tokio::test]
async fn internal_note_sends_no_mail() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id, _address) = setup_instance(&db, "note").await;
    let requester_email = unique_email("requester");
    let cc_email = unique_email("cc");
    let ticket = make_ticket(&db, &instance.id, &requester_email, &cc_email).await;

    let (my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(ADD_NOTE_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "body": "Internal only: waiting on vendor.",
                })))
                .data(user_auth(&owner_id, &instance.id, true)),
        )
        .await;
    assert_eq!(
        expect_data(&response, "addInternalNote.kind"),
        json!("NOTE")
    );

    assert_eq!(
        my_app.mail.sent_raw().len(),
        0,
        "an internal note must never send mail"
    );
    assert_eq!(
        my_app.mail.sent().len(),
        0,
        "an internal note must never send mail (plain/HTML path either)"
    );
}

#[tokio::test]
async fn closing_a_ticket_sends_exactly_one_notice() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id, _address) = setup_instance(&db, "close").await;
    let requester_email = unique_email("requester");
    let cc_email = unique_email("cc");
    let ticket = make_ticket(&db, &instance.id, &requester_email, &cc_email).await;

    let (my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(SET_STATUS_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "status": "CLOSED",
                })))
                .data(user_auth(&owner_id, &instance.id, true)),
        )
        .await;
    assert_eq!(
        expect_data(&response, "setTicketStatus.status"),
        json!("CLOSED")
    );

    let sent = my_app.mail.sent_raw();
    assert_eq!(
        sent.len(),
        1,
        "closing must send exactly one notice: {sent:?}"
    );
    assert_eq!(sent[0].to, vec![requester_email]);
    assert_eq!(sent[0].cc, vec![cc_email]);
}

#[tokio::test]
async fn closing_a_ticket_quietly_sends_no_customer_notice() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id, _address) = setup_instance(&db, "closeq").await;
    let requester_email = unique_email("requester");
    let cc_email = unique_email("cc");
    let ticket = make_ticket(&db, &instance.id, &requester_email, &cc_email).await;

    let (my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(
                r#"
                mutation($ticketId: ID!, $status: TicketStatusType!) {
                    setTicketStatus(ticketId: $ticketId, status: $status, quiet: true) { id status }
                }
                "#,
            )
            .variables(Variables::from_json(json!({
                "ticketId": ticket.id,
                "status": "CLOSED",
            })))
            .data(user_auth(&owner_id, &instance.id, true)),
        )
        .await;
    assert_eq!(
        expect_data(&response, "setTicketStatus.status"),
        json!("CLOSED")
    );

    // `setup_instance` has only the one member (the actor), so no staff
    // notice either — any mail at all here would be the customer notice.
    let sent = my_app.mail.sent_raw();
    assert!(sent.is_empty(), "a quiet close must send nothing: {sent:?}");
}

#[tokio::test]
async fn self_assigning_a_ticket_sends_no_mail_at_all() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id, _address) = setup_instance(&db, "assign").await;
    let requester_email = unique_email("requester");
    let cc_email = unique_email("cc");
    let ticket = make_ticket(&db, &instance.id, &requester_email, &cc_email).await;

    let (my_app, schema) = build_app_and_schema(db);

    const ASSIGN_MUTATION: &str = r#"
        mutation($ticketId: ID!, $userId: ID) {
            assignTicket(ticketId: $ticketId, userId: $userId) { id assigneeUserId }
        }
    "#;
    let response = schema
        .execute(
            Request::new(ASSIGN_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "userId": owner_id,
                })))
                .data(user_auth(&owner_id, &instance.id, true)),
        )
        .await;
    assert_eq!(
        expect_data(&response, "assignTicket.assigneeUserId"),
        json!(owner_id)
    );

    // No customer mail (internal bookkeeping) *and* no staff notice either —
    // the actor is their own new assignee, and `staff_notify` always
    // excludes the actor. `setup_instance` has only the one member, so this
    // also stands in for "nobody else exists to notify"; the next test
    // covers assigning to someone else.
    assert_eq!(
        my_app.mail.sent_raw().len(),
        0,
        "self-assignment must never send mail, customer or staff"
    );
}

/// Assigning to a *different* member sends no customer mail, but does send
/// the new assignee a best-effort staff notice (default `assignedToMe`
/// setting) — and nothing to anyone else. See `staff_notify`'s module doc.
#[tokio::test]
async fn assigning_a_ticket_to_another_member_sends_no_customer_mail_but_notifies_the_assignee() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id, _address) = setup_instance(&db, "assign2").await;
    let agent = db
        .create_user(&unique_email("assign2-agent"), "Agent")
        .await
        .expect("create_user");
    db.create_membership(&agent.id, &instance.id, db::MembershipRole::Agent)
        .await
        .expect("create_membership");
    let requester_email = unique_email("requester");
    let cc_email = unique_email("cc");
    let ticket = make_ticket(&db, &instance.id, &requester_email, &cc_email).await;

    let (my_app, schema) = build_app_and_schema(db);

    const ASSIGN_MUTATION: &str = r#"
        mutation($ticketId: ID!, $userId: ID) {
            assignTicket(ticketId: $ticketId, userId: $userId) { id assigneeUserId }
        }
    "#;
    let response = schema
        .execute(
            Request::new(ASSIGN_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "userId": agent.id,
                })))
                .data(user_auth(&owner_id, &instance.id, true)),
        )
        .await;
    assert_eq!(
        expect_data(&response, "assignTicket.assigneeUserId"),
        json!(agent.id)
    );

    let sent = my_app.mail.sent_raw();
    assert_eq!(
        sent.len(),
        1,
        "exactly one staff notice, to the new assignee, and no customer mail: {sent:?}"
    );
    assert_eq!(sent[0].to, vec![agent.email.clone()]);
    let raw = String::from_utf8_lossy(&sent[0].raw);
    assert!(
        !raw.contains("+t"),
        "a staff notice must never carry a +t… reply tag: {raw}"
    );
}

#[tokio::test]
async fn submitting_a_ticket_sends_an_acknowledgement() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id, _address) = setup_instance(&db, "submit").await;
    let owner_email = db
        .get_users(&[&owner_id])
        .await
        .expect("get_users")
        .into_iter()
        .next()
        .flatten()
        .expect("owner exists")
        .email;
    let requester_email = unique_email("requester");

    let (my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(SUBMIT_TICKET_MUTATION)
                .variables(Variables::from_json(json!({
                    "subject": "Help please",
                    "body": "Something is broken.",
                })))
                .data(requester_auth(&requester_email, &instance.id)),
        )
        .await;
    assert!(
        expect_data(&response, "submitTicket.id").as_str().is_some(),
        "submitTicket must succeed even though mail is best-effort"
    );

    // Two messages now: the customer-facing acknowledgement, and a
    // best-effort staff notice to the instance's owner — who is *not* the
    // actor here (submitTicket's actor is the requester's own address, per
    // `staff_notify`), so the default `newTicket` setting notifies them.
    let sent = my_app.mail.sent_raw();
    assert_eq!(
        sent.len(),
        2,
        "expected the requester's acknowledgement plus one staff notice: {sent:?}"
    );

    let ack = sent
        .iter()
        .find(|m| m.to == vec![requester_email.clone()])
        .expect("no acknowledgement addressed to the requester");
    assert!(
        String::from_utf8_lossy(&ack.raw).contains("Thanks for reaching out"),
        "the requester's message must be the acknowledgement"
    );

    let staff_notice = sent
        .iter()
        .find(|m| m.to == vec![owner_email.clone()])
        .expect("no staff notice addressed to the owner");
    let raw = String::from_utf8_lossy(&staff_notice.raw);
    assert!(raw.contains(&requester_email), "{raw}");
    assert!(
        !raw.contains("+t"),
        "a staff notice must never carry a +t… reply tag: {raw}"
    );
}

// ── staff notifications ─────────────────────────────────────────────────────
//
// Everything above exercises customer-facing mail; these exercise
// `staff_notify` end to end, through the same real GraphQL mutations,
// against a *second* member so there is always someone other than the actor
// around to (not) notify.

/// `label`'s instance, its owner, and a second member (an agent) added to
/// it — `staff_notify`'s recipient rules need someone who isn't the actor.
async fn setup_instance_with_agent(
    db: &dynamodb::Handler,
    label: &str,
) -> (db::Instance, String, db::User, String) {
    let (instance, owner_id, address) = setup_instance(db, label).await;
    let agent = db
        .create_user(&unique_email(&format!("{label}-agent")), "Agent")
        .await
        .expect("create_user");
    db.create_membership(&agent.id, &instance.id, db::MembershipRole::Agent)
        .await
        .expect("create_membership");
    (instance, owner_id, agent, address)
}

#[tokio::test]
async fn an_agent_reply_notifies_a_fellow_member_by_default_but_never_the_actor() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id, agent, _address) = setup_instance_with_agent(&db, "staffreply").await;
    let requester_email = unique_email("requester");
    let cc_email = unique_email("cc");
    let ticket = make_ticket(&db, &instance.id, &requester_email, &cc_email).await;

    let (my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(REPLY_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "body": "We're on it.",
                })))
                .data(user_auth(&owner_id, &instance.id, true)),
        )
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);

    let sent = my_app.mail.sent_raw();
    // The customer reply, plus a staff notice to the agent (unassigned
    // ticket, `unassignedUpdated` defaults true) — never to the owner, who
    // is the actor.
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(
        sent.iter().any(|m| m.to == vec![requester_email.clone()]),
        "the customer reply is still sent: {sent:?}"
    );
    let staff_notice = sent
        .iter()
        .find(|m| m.to == vec![agent.email.clone()])
        .expect("no staff notice addressed to the agent");
    let raw = String::from_utf8_lossy(&staff_notice.raw);
    assert!(raw.contains("replied to the customer"), "{raw}");
    assert!(
        !raw.contains("+t"),
        "a staff notice must never carry a +t… reply tag: {raw}"
    );
}

#[tokio::test]
async fn an_internal_note_notifies_a_fellow_member_by_default() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id, agent, _address) = setup_instance_with_agent(&db, "staffnote").await;
    let requester_email = unique_email("requester");
    let cc_email = unique_email("cc");
    let ticket = make_ticket(&db, &instance.id, &requester_email, &cc_email).await;

    let (my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(ADD_NOTE_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "body": "Waiting on vendor.",
                })))
                .data(user_auth(&owner_id, &instance.id, true)),
        )
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);

    // No customer mail at all (an internal note never sends any — see
    // CLAUDE.md), but the agent gets a staff notice.
    let sent = my_app.mail.sent_raw();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].to, vec![agent.email.clone()]);
    assert!(String::from_utf8_lossy(&sent[0].raw).contains("added an internal note"));
}

#[tokio::test]
async fn closing_a_ticket_notifies_a_fellow_member_alongside_the_customer_notice() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id, agent, _address) = setup_instance_with_agent(&db, "staffclose").await;
    let requester_email = unique_email("requester");
    let cc_email = unique_email("cc");
    let ticket = make_ticket(&db, &instance.id, &requester_email, &cc_email).await;

    let (my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(SET_STATUS_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "status": "CLOSED",
                })))
                .data(user_auth(&owner_id, &instance.id, true)),
        )
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);

    let sent = my_app.mail.sent_raw();
    assert_eq!(sent.len(), 2, "customer notice + staff notice: {sent:?}");
    assert!(sent.iter().any(|m| m.to == vec![requester_email.clone()]));
    let staff_notice = sent
        .iter()
        .find(|m| m.to == vec![agent.email.clone()])
        .expect("no staff notice addressed to the agent");
    assert!(String::from_utf8_lossy(&staff_notice.raw).contains("closed this ticket"));
}

#[tokio::test]
async fn a_reply_on_a_ticket_assigned_to_someone_else_does_not_notify_by_default() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id, agent, _address) = setup_instance_with_agent(&db, "staffothers").await;
    let requester_email = unique_email("requester");
    let cc_email = unique_email("cc");
    let ticket = make_ticket(&db, &instance.id, &requester_email, &cc_email).await;
    // Assign to the owner (self-assign, sends no mail — see the assignment
    // tests above) so the ticket is "assigned to someone else" from the
    // agent's point of view.
    db.update_ticket(
        &ticket.id,
        db::TicketUpdateShape::SetStatusAndAssignee {
            instance_id: &instance.id,
            status: ticket.status,
            assignee_user_id: Some(&owner_id),
            now: ticket.updated_at,
        },
    )
    .await
    .expect("assign ticket to owner directly");

    let (my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(REPLY_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "body": "Working on it.",
                })))
                .data(user_auth(&owner_id, &instance.id, true)),
        )
        .await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);

    // Only the customer-facing reply — `assignedToOthersUpdated` defaults
    // to off, so the agent hears nothing about the owner's ticket.
    let sent = my_app.mail.sent_raw();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].to, vec![requester_email]);

    // Enabling the setting via the mutation makes the very next reply
    // notify the agent — proving the mutation's write actually takes
    // effect, not just that the default is off.
    const UPDATE_SETTINGS_MUTATION: &str = r#"
        mutation($instanceId: ID!, $settings: NotificationSettingsInput!) {
            updateNotificationSettings(instanceId: $instanceId, settings: $settings) {
                notificationSettings { assignedToOthersUpdated }
            }
        }
    "#;
    let update_response = schema
        .execute(
            Request::new(UPDATE_SETTINGS_MUTATION)
                .variables(Variables::from_json(json!({
                    "instanceId": instance.id,
                    "settings": { "assignedToOthersUpdated": true },
                })))
                .data(user_auth(&agent.id, &instance.id, false)),
        )
        .await;
    assert_eq!(
        expect_data(
            &update_response,
            "updateNotificationSettings.notificationSettings.assignedToOthersUpdated"
        ),
        json!(true)
    );

    let response2 = schema
        .execute(
            Request::new(REPLY_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "body": "Still working on it.",
                })))
                .data(user_auth(&owner_id, &instance.id, true)),
        )
        .await;
    assert!(response2.errors.is_empty(), "{:?}", response2.errors);

    let sent2 = my_app.mail.sent_raw();
    assert_eq!(
        sent2.len(),
        3,
        "prior 1 + this reply's customer mail + the now-enabled staff notice: {sent2:?}"
    );
    assert!(
        sent2.iter().any(|m| m.to == vec![agent.email.clone()]),
        "the agent should now be notified: {sent2:?}"
    );
}

fn superuser_auth(user_id: &str) -> AuthInfo {
    AuthInfo::User {
        id: user_id.to_string(),
        memberships: vec![],
        is_superuser: true,
        token_id: None,
        grant_id: None,
    }
}

/// `MembershipInfo.notificationSettings` is only reachable off a `User`
/// object — `me` always returns the caller's own, so the one real path to
/// *someone else's* is a superuser's `adminUser`. This is that path,
/// end to end: it must be rejected `FORBIDDEN`, not silently return the
/// agent's real settings or a misleadingly empty/default-looking response.
#[tokio::test]
async fn notification_settings_is_forbidden_via_admin_user() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (_instance, _owner_id, agent, _address) =
        setup_instance_with_agent(&db, "staffguard").await;
    let superuser = db
        .create_user(&unique_email("staffguard-super"), "Super")
        .await
        .expect("create_user");

    let (_my_app, schema) = build_app_and_schema(db);

    const ADMIN_USER_SETTINGS_QUERY: &str = r#"
        query($id: ID!) {
            adminUser(id: $id) {
                memberships {
                    instance { id }
                    notificationSettings { newTicket }
                }
            }
        }
    "#;
    let response = schema
        .execute(
            Request::new(ADMIN_USER_SETTINGS_QUERY)
                .variables(Variables::from_json(json!({ "id": agent.id })))
                .data(superuser_auth(&superuser.id)),
        )
        .await;
    assert!(
        !response.errors.is_empty(),
        "a superuser must not be able to read another user's notification settings via adminUser"
    );
    assert!(
        response.errors.iter().any(|e| e
            .message
            .contains("Cannot view another member's notification settings")),
        "{:?}",
        response.errors
    );
}

/// Customer-facing ticket mail carries the `ticket_id` SES tag, and SES
/// events for it land on `TicketMessage.deliveries`, per recipient, moving
/// only forward — and opens/clicks on `openedAt`/`clickedAt`, per message,
/// keeping the earliest.
#[tokio::test]
async fn ticket_mail_is_tagged_and_its_delivery_events_show_on_the_message() {
    use toolbox::ses_events::SesEvent;
    use toolbox::ticket_delivery;

    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id, _address) = setup_instance(&db, "track").await;
    let requester_email = unique_email("requester").to_lowercase();
    let cc_email = unique_email("cc").to_lowercase();
    let ticket = make_ticket(&db, &instance.id, &requester_email, &cc_email).await;
    let (my_app, schema) = build_app_and_schema(db.clone());

    let response = schema
        .execute(
            Request::new(REPLY_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "body": "We're looking into it.",
                })))
                .data(user_auth(&owner_id, &instance.id, true)),
        )
        .await;
    expect_data(&response, "replyToTicket.id");

    // A close notice is customer mail too.
    let response = schema
        .execute(
            Request::new(SET_STATUS_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "status": "CLOSED",
                })))
                .data(user_auth(&owner_id, &instance.id, true)),
        )
        .await;
    expect_data(&response, "setTicketStatus.id");

    let sent = my_app.mail.sent_raw();
    assert_eq!(sent.len(), 2, "{sent:?}");
    for msg in &sent {
        assert_eq!(
            msg.tags,
            vec![("ticket_id".to_string(), ticket.id.clone())],
            "every customer-facing ticket message carries the ticket tag"
        );
    }
    let reply_ses_id = db::ses_message_id_from_rfc(&sent[0].message_id)
        .expect("mock ids have the SES shape")
        .to_string();

    let ev = |event_type: &str, key: &str, body: serde_json::Value| {
        let mut v = json!({
            "eventType": event_type,
            "mail": {
                "messageId": reply_ses_id,
                "timestamp": "2026-10-13T01:00:00.000Z",
                "destination": [requester_email, cc_email],
                "tags": { "ticket_id": [ticket.id] },
            },
        });
        v[key] = body;
        SesEvent::parse(&v.to_string()).unwrap()
    };
    for e in [
        ev(
            "Delivery",
            "delivery",
            json!({ "recipients": [requester_email], "smtpResponse": "250 OK" }),
        ),
        ev(
            "Bounce",
            "bounce",
            json!({
                "bounceType": "Permanent",
                "bounceSubType": "General",
                "bouncedRecipients": [{ "emailAddress": cc_email, "diagnosticCode": "550 unknown" }],
            }),
        ),
        // Late and less final: must not hide the bounce.
        ev(
            "DeliveryDelay",
            "deliveryDelay",
            json!({ "delayType": "MailboxFull", "delayedRecipients": [{ "emailAddress": cc_email }] }),
        ),
        ev(
            "Open",
            "open",
            json!({ "timestamp": "2026-10-13T03:00:00.000Z" }),
        ),
        // An earlier open arriving late wins; a replay changes nothing.
        ev(
            "Open",
            "open",
            json!({ "timestamp": "2026-10-13T02:00:00.000Z" }),
        ),
        ev(
            "Open",
            "open",
            json!({ "timestamp": "2026-10-13T03:00:00.000Z" }),
        ),
        ev(
            "Click",
            "click",
            json!({ "timestamp": "2026-10-13T02:00:30.000Z" }),
        ),
    ] {
        ticket_delivery::apply(&db, &e).await.expect("apply");
    }

    let response = schema
        .execute(
            Request::new(
                r#"query($id: ID!) { ticket(id: $id) { messages { kind rfcMessageId deliveries { email status detail } openedAt clickedAt } } }"#,
            )
            .variables(Variables::from_json(json!({ "id": ticket.id })))
            .data(user_auth(&owner_id, &instance.id, true))
            .data(graphql::get_dataloader(my_app.clone())),
        )
        .await;
    let messages = expect_data(&response, "ticket.messages");
    let reply = messages
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["kind"] == "REPLY")
        .expect("the reply");
    assert_eq!(reply["openedAt"], 1_791_856_800);
    assert_eq!(reply["clickedAt"], 1_791_856_830);
    // The engagement row is the message's, never listed as a recipient.
    assert_eq!(reply["deliveries"].as_array().unwrap().len(), 2, "{reply}");
    let mut deliveries = reply["deliveries"].as_array().unwrap().clone();
    deliveries.sort_by_key(|d| d["email"].as_str().unwrap().to_string());
    let by_email = |e: &str| {
        deliveries
            .iter()
            .find(|d| d["email"] == e)
            .unwrap_or_else(|| panic!("no delivery for {e}: {deliveries:?}"))
            .clone()
    };
    assert_eq!(by_email(&requester_email)["status"], "DELIVERED");
    assert_eq!(by_email(&cc_email)["status"], "BOUNCED");
    assert_eq!(
        by_email(&cc_email)["detail"],
        "Permanent/General: 550 unknown"
    );
    // The close notice has no events yet.
    let system = messages
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["kind"] == "SYSTEM")
        .expect("the close notice");
    assert_eq!(system["deliveries"], json!([]));
    assert_eq!(system["openedAt"], json!(null));
}
