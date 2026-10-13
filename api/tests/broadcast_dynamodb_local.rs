//! Integration tests for broadcast email against **DynamoDB Local** and the
//! **mock** mailer: `sendBroadcast` through the real GraphQL schema, replies
//! through the real inbound pipeline, and SES delivery events through
//! `broadcast_events::apply`. Proves the three promises CLAUDE.md's
//! "Broadcasts" entry makes: one copy per recipient, a reply from each
//! recipient opens its *own* new ticket, and bounces/deliveries land on the
//! right recipient row.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test broadcast_dynamodb_local
//! ```
//!
//! Like the other `*_dynamodb_local.rs` files, every test here **skips
//! itself** when no reachable local DynamoDB is configured.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::json;
use toolbox::app;
use toolbox::auth::{AuthInfo, Membership};
use toolbox::broadcast_events;
use toolbox::db;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::graphql;
use toolbox::inbound::pipeline;
use toolbox::mockmail;
use toolbox::mockstorage;

type TestApp = app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage>;
type TestSchema = graphql::ToolboxSchema<TestApp>;

async fn local_db_prefix() -> Option<String> {
    let endpoint = toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;

    let client = toolbox::local_dev::dynamodb_client().await;
    if client.list_tables().send().await.is_err() {
        eprintln!(
            "broadcast_dynamodb_local: {endpoint} is configured but not reachable — skipping. \
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
                    "broadcast_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set to \
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

/// Lowercase, as `sendBroadcast` normalizes every recipient.
fn unique_email(label: &str) -> String {
    format!("{label}-{}@example.com", nanoid::nanoid!(8)).to_lowercase()
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

fn expect_error_code(response: &Response) -> String {
    let err = response
        .errors
        .first()
        .unwrap_or_else(|| panic!("expected a GraphQL error, got none: {response:?}"));
    match err.extensions.as_ref().and_then(|e| e.get("code")) {
        Some(async_graphql::Value::String(s)) => s.clone(),
        other => panic!("error had no string extensions.code: {other:?} ({err:?})"),
    }
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

/// A support instance with one exact inbound address (`support@{domain}`)
/// and an owner. Returns `(instance, owner_user_id, domain)`.
async fn setup_instance(
    db: &dynamodb::Handler,
    label: &str,
    kind: db::InstanceKind,
) -> (db::Instance, String, String) {
    let instance = db
        .create_instance(
            &format!("{label} instance"),
            &unique_id(&format!("{label}-slug")),
            &format!("{label} Support"),
            "Thanks,\nThe team",
            false,
            kind,
        )
        .await
        .expect("create_instance");
    let domain = format!("{}.toolbox.test", unique_id(label).to_lowercase());
    if kind == db::InstanceKind::Support {
        db.create_inbound_address(
            &format!("support@{domain}"),
            &instance.id,
            db::AddressKind::Exact,
        )
        .await
        .expect("create_inbound_address");
    }
    let owner = db
        .create_user(&unique_email(&format!("{label}-owner")), "Owner")
        .await
        .expect("create_user");
    db.create_membership(&owner.id, &instance.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership");
    (instance, owner.id, domain)
}

fn build_app_and_schema(db: dynamodb::Handler) -> (Arc<TestApp>, TestSchema) {
    let my_app = Arc::new(app::new(
        db,
        mockmail::Handler::new(),
        mockstorage::Storage::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(my_app.clone(), webauthn);
    (my_app, schema)
}

const SEND_BROADCAST: &str = r#"
    mutation($instanceId: ID!, $input: SendBroadcastInput!) {
        sendBroadcast(instanceId: $instanceId, input: $input) {
            id fromAddress fromName replyTo subject recipientCount sentCount failedCount
            recipients { email sendStatus error deliveryStatus deliveryDetail }
        }
    }
"#;

const LIST_BROADCASTS: &str = r#"
    query($slug: String!) {
        instance(slug: $slug) {
            broadcastFromDomains
            broadcasts { id subject deliveredCount bouncedCount complainedCount
                recipients { email sendStatus deliveryStatus deliveryDetail } }
        }
    }
"#;

async fn send(
    schema: &TestSchema,
    auth: AuthInfo,
    instance_id: &str,
    from: &str,
    recipients: &[String],
) -> Response {
    schema
        .execute(
            Request::new(SEND_BROADCAST)
                .variables(Variables::from_json(json!({
                    "instanceId": instance_id,
                    "input": {
                        "fromAddress": from,
                        "subject": "New in seslogin: location check-in",
                        "body": "We've shipped location check-in.\nReply with any questions.",
                        "recipients": recipients,
                    },
                })))
                .data(auth),
        )
        .await
}

#[tokio::test]
async fn sends_one_copy_per_recipient_and_records_each_result() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) = setup_instance(&db, "bcast", db::InstanceKind::Support).await;
    let (app, schema) = build_app_and_schema(db.clone());

    let recipients = vec![
        unique_email("alice"),
        unique_email("bob"),
        unique_email("carol"),
    ];
    let resp = send(
        &schema,
        user_auth(&owner, &instance.id, true),
        &instance.id,
        &format!("news@{domain}"),
        &recipients,
    )
    .await;
    let b = expect_data(&resp, "sendBroadcast");
    assert_eq!(b["fromAddress"], format!("news@{domain}"));
    assert_eq!(b["fromName"], "bcast Support");
    assert_eq!(b["replyTo"], format!("support@{domain}"));
    assert_eq!(b["recipientCount"], 3);
    assert_eq!(b["sentCount"], 3);
    assert_eq!(b["failedCount"], 0);

    let sent = app.mail.sent_raw();
    assert_eq!(sent.len(), 3);
    let broadcast_id = b["id"].as_str().unwrap().to_string();
    for msg in &sent {
        assert_eq!(msg.to.len(), 1, "one recipient per copy");
        assert!(msg.cc.is_empty());
        let index: usize = recipients.iter().position(|r| *r == msg.to[0]).unwrap();
        assert_eq!(
            msg.tags,
            vec![
                ("broadcast_id".to_string(), broadcast_id.clone()),
                ("broadcast_recipient".to_string(), index.to_string()),
            ]
        );
        let raw = String::from_utf8_lossy(&msg.raw);
        assert!(raw.contains("Auto-Submitted: auto-generated"), "{raw}");
        assert!(!raw.contains("[#"), "no ticket tag in the subject: {raw}");
    }

    let stored = db
        .get_broadcast_consistent(&broadcast_id)
        .await
        .unwrap()
        .expect("broadcast row");
    assert_eq!(stored.recipients.len(), 3);
    for (r, email) in stored.recipients.iter().zip(&recipients) {
        assert_eq!(&r.email, email, "recipient order is preserved");
        assert_eq!(r.send_status, db::BroadcastSendStatus::Sent);
        assert!(r.provider_message_id.is_some());
    }
}

#[tokio::test]
async fn replies_from_different_recipients_open_separate_new_tickets() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) = setup_instance(&db, "reply", db::InstanceKind::Support).await;
    let (app, schema) = build_app_and_schema(db.clone());

    let alice = unique_email("alice");
    let bob = unique_email("bob");
    let resp = send(
        &schema,
        user_auth(&owner, &instance.id, true),
        &instance.id,
        &format!("news@{domain}"),
        &[alice.clone(), bob.clone()],
    )
    .await;
    let b = expect_data(&resp, "sendBroadcast");
    let reply_to = b["replyTo"].as_str().unwrap().to_string();
    let sent = app.mail.sent_raw();

    let mut ticket_ids = Vec::new();
    for who in [&alice, &bob] {
        let copy = sent.iter().find(|m| m.to[0] == *who).unwrap();
        // A real client threads the reply on the provider's Message-ID.
        let raw = format!(
            "From: {who}\r\nTo: {reply_to}\r\nIn-Reply-To: {mid}\r\nReferences: {mid}\r\n\
             Message-ID: <{id}@example.com>\r\n\
             Subject: Re: New in seslogin: location check-in\r\nMIME-Version: 1.0\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\r\nLooks great — question from {who}.",
            mid = copy.message_id,
            id = nanoid::nanoid!(12),
        );
        let outcome =
            pipeline::process_raw_message(&*app, &nanoid::nanoid!(16), raw.as_bytes(), None)
                .await
                .expect("process_raw_message");
        assert!(outcome.dropped_reason.is_none(), "{outcome:?}");
        assert!(outcome.created_new_ticket, "a reply opens a new ticket");
        let ticket_id = outcome.ticket_id.unwrap();
        let ticket = db
            .get_tickets(&[ticket_id.as_str()])
            .await
            .unwrap()
            .into_iter()
            .next()
            .flatten()
            .unwrap();
        assert_eq!(ticket.instance_id, instance.id);
        assert_eq!(ticket.requester_emails, vec![who.clone()]);
        assert!(ticket.cc_emails.is_empty(), "{:?}", ticket.cc_emails);
        ticket_ids.push(ticket_id);
    }
    assert_ne!(ticket_ids[0], ticket_ids[1], "two people, two tickets");

    // An out-of-office reply that marks itself as automatic is dropped.
    let copy = sent.iter().find(|m| m.to[0] == alice).unwrap();
    let ooo = format!(
        "From: {alice}\r\nTo: {reply_to}\r\nIn-Reply-To: {mid}\r\nAuto-Submitted: auto-replied\r\n\
         Subject: Out of office\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nAway.",
        mid = copy.message_id
    );
    let outcome = pipeline::process_raw_message(&*app, &nanoid::nanoid!(16), ooo.as_bytes(), None)
        .await
        .unwrap();
    assert!(outcome.dropped_reason.is_some(), "{outcome:?}");
}

#[tokio::test]
async fn only_an_owner_of_a_support_instance_can_send() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, _owner, domain) = setup_instance(&db, "perm", db::InstanceKind::Support).await;
    let (app, schema) = build_app_and_schema(db.clone());
    let to = vec![unique_email("x")];
    let from = format!("news@{domain}");

    let agent = db
        .create_user(&unique_email("agent"), "Agent")
        .await
        .unwrap();
    let resp = send(
        &schema,
        user_auth(&agent.id, &instance.id, false),
        &instance.id,
        &from,
        &to,
    )
    .await;
    assert_eq!(expect_error_code(&resp), "UNAUTHENTICATED");

    let superuser = AuthInfo::User {
        id: agent.id.clone(),
        memberships: vec![],
        is_superuser: true,
        token_id: None,
        grant_id: None,
    };
    let resp = send(&schema, superuser, &instance.id, &from, &to).await;
    assert_eq!(expect_error_code(&resp), "UNAUTHENTICATED");

    let (invoicing, inv_owner, _) = setup_instance(&db, "permi", db::InstanceKind::Invoicing).await;
    let resp = send(
        &schema,
        user_auth(&inv_owner, &invoicing.id, true),
        &invoicing.id,
        &from,
        &to,
    )
    .await;
    assert_eq!(expect_error_code(&resp), "NOT_FOUND");

    assert!(app.mail.sent_raw().is_empty(), "nothing was sent");
}

#[tokio::test]
async fn a_from_address_off_the_instance_domain_is_refused_before_sending() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, _domain) = setup_instance(&db, "dom", db::InstanceKind::Support).await;
    let (app, schema) = build_app_and_schema(db.clone());
    let resp = send(
        &schema,
        user_auth(&owner, &instance.id, true),
        &instance.id,
        "news@example.org",
        &[unique_email("x")],
    )
    .await;
    assert!(!resp.errors.is_empty());
    assert!(
        resp.errors[0].message.contains("domain"),
        "{:?}",
        resp.errors
    );
    assert!(app.mail.sent_raw().is_empty());
}

fn ses_event(
    event_type: &str,
    broadcast_id: &str,
    index: usize,
    body_key: &str,
    body: serde_json::Value,
) -> String {
    let mut v = json!({
        "eventType": event_type,
        "mail": {
            "timestamp": "2026-10-13T01:00:00.000Z",
            "messageId": "0100018f-test",
            "tags": {
                "ses:configuration-set": ["toolbox"],
                "broadcast_id": [broadcast_id],
                "broadcast_recipient": [index.to_string()],
            },
        },
    });
    v[body_key] = body;
    v.to_string()
}

#[tokio::test]
async fn delivery_events_update_the_right_recipient_and_only_move_forward() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) = setup_instance(&db, "events", db::InstanceKind::Support).await;
    let (_app, schema) = build_app_and_schema(db.clone());
    let recipients = vec![
        unique_email("ok"),
        unique_email("gone"),
        unique_email("angry"),
    ];
    let resp = send(
        &schema,
        user_auth(&owner, &instance.id, true),
        &instance.id,
        &format!("news@{domain}"),
        &recipients,
    )
    .await;
    let id = expect_data(&resp, "sendBroadcast")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let delivery = json!({ "timestamp": "2026-10-13T01:00:01.000Z", "smtpResponse": "250 OK" });
    let bounce = json!({
        "bounceType": "Permanent",
        "bounceSubType": "General",
        "bouncedRecipients": [{ "diagnosticCode": "smtp; 550 5.1.1 user unknown" }],
        "timestamp": "2026-10-13T01:00:02.000Z",
    });
    let delay = json!({ "delayType": "MailboxFull", "timestamp": "2026-10-13T01:00:03.000Z" });
    for msg in [
        ses_event("Delivery", &id, 0, "delivery", delivery.clone()),
        ses_event("Bounce", &id, 1, "bounce", bounce),
        // A late, less-final event must not hide the bounce.
        ses_event("DeliveryDelay", &id, 1, "deliveryDelay", delay),
        ses_event("Delivery", &id, 2, "delivery", delivery.clone()),
        ses_event(
            "Complaint",
            &id,
            2,
            "complaint",
            json!({ "complaintFeedbackType": "abuse" }),
        ),
        // Redelivery of the same event is harmless.
        ses_event("Delivery", &id, 0, "delivery", delivery.clone()),
        // Unknown broadcast and out-of-range index are no-ops, not errors.
        ses_event("Delivery", "nonexistent0", 0, "delivery", delivery.clone()),
        ses_event("Delivery", &id, 99, "delivery", delivery.clone()),
    ] {
        broadcast_events::apply(&db, &msg).await.expect("apply");
    }
    // Ticket mail carries no tags: ignored.
    let untagged =
        json!({ "eventType": "Bounce", "mail": { "tags": {} }, "bounce": {} }).to_string();
    broadcast_events::apply(&db, &untagged).await.unwrap();

    let stored = db.get_broadcast_consistent(&id).await.unwrap().unwrap();
    assert_eq!(stored.recipients.len(), 3);
    let r = &stored.recipients;
    assert_eq!(
        r[0].delivery_status,
        Some(db::BroadcastDeliveryStatus::Delivered)
    );
    assert_eq!(r[0].delivery_detail.as_deref(), Some("250 OK"));
    assert_eq!(
        r[1].delivery_status,
        Some(db::BroadcastDeliveryStatus::Bounced)
    );
    assert_eq!(
        r[1].delivery_detail.as_deref(),
        Some("Permanent/General: smtp; 550 5.1.1 user unknown")
    );
    assert_eq!(
        r[2].delivery_status,
        Some(db::BroadcastDeliveryStatus::Complained)
    );
    // The send-time fields are untouched by events.
    assert!(
        r.iter()
            .all(|x| x.send_status == db::BroadcastSendStatus::Sent)
    );

    // And the owner sees it all in the listing.
    let resp = schema
        .execute(
            Request::new(LIST_BROADCASTS)
                .variables(Variables::from_json(json!({ "slug": instance.slug })))
                .data(user_auth(&owner, &instance.id, true)),
        )
        .await;
    let inst = expect_data(&resp, "instance");
    assert_eq!(inst["broadcastFromDomains"], json!([domain]));
    let listed = &inst["broadcasts"][0];
    assert_eq!(listed["id"], id);
    assert_eq!(listed["deliveredCount"], 1);
    assert_eq!(listed["bouncedCount"], 1);
    assert_eq!(listed["complainedCount"], 1);
    assert_eq!(listed["recipients"][1]["deliveryStatus"], "BOUNCED");
}

#[tokio::test]
async fn a_suppressed_address_is_recorded_and_never_sent() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) = setup_instance(&db, "supp", db::InstanceKind::Support).await;
    let (app, schema) = build_app_and_schema(db.clone());

    let gone = unique_email("gone");
    let ok = unique_email("ok");
    db.record_email_suppression(
        &gone,
        db::SuppressionReason::HardBounce,
        Some("5.1.1 user unknown"),
        1,
    )
    .await
    .unwrap();

    let resp = send(
        &schema,
        user_auth(&owner, &instance.id, true),
        &instance.id,
        &format!("news@{domain}"),
        &[gone.clone(), ok.clone()],
    )
    .await;
    let b = expect_data(&resp, "sendBroadcast");
    assert_eq!(b["sentCount"], 1);
    assert_eq!(b["recipients"][0]["sendStatus"], "SUPPRESSED");
    assert!(
        b["recipients"][0]["error"]
            .as_str()
            .unwrap()
            .contains("hard bounce"),
        "{b}"
    );
    assert_eq!(b["recipients"][1]["sendStatus"], "SENT");

    let sent = app.mail.sent_raw();
    assert_eq!(sent.len(), 1, "only the unsuppressed address is mailed");
    assert_eq!(sent[0].to, vec![ok]);
    // Its tag still names its original index, so delivery events land on it.
    assert_eq!(
        sent[0].tags[1],
        ("broadcast_recipient".to_string(), "1".to_string())
    );

    let stored = db
        .get_broadcast_consistent(b["id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.recipients[0].send_status,
        db::BroadcastSendStatus::Suppressed
    );
}
