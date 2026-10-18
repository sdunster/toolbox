//! Integration tests for broadcast email against **DynamoDB Local** and the
//! **mock** mailer and queue: `sendBroadcast` through the real GraphQL schema,
//! its queued batches through the worker's own job runner, replies
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
use toolbox::broadcast;
use toolbox::broadcast_events;
use toolbox::db;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::graphql;
use toolbox::inbound::pipeline;
use toolbox::mockmail;
use toolbox::mockqueue;
use toolbox::mockstorage;
use toolbox::ses_events::SesEvent;
use toolbox::worker;

type TestApp =
    app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage, mockqueue::Queue>;
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
        mockqueue::Queue::new(),
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
            inProgress pendingCount nextBatchAt estimatedFinishAt
            recipients { email sendStatus error deliveryStatus deliveryDetail }
        }
    }
"#;

const LIST_BROADCASTS: &str = r#"
    query($slug: String!) {
        instance(slug: $slug) {
            broadcastFromDomains
            broadcasts { id subject deliveredCount bouncedCount complainedCount
                openedCount clickedCount
                recipients { email sendStatus deliveryStatus deliveryDetail openedAt clickedAt } }
        }
    }
"#;

/// Stand in for the worker: run every queued job, and every job those
/// queue, until the queue is empty (delays ignored). Returns the jobs run.
async fn run_queued_jobs(app: &TestApp) -> Vec<mockqueue::QueuedJob> {
    let mut ran = Vec::new();
    loop {
        let jobs = app.queue.drain();
        if jobs.is_empty() {
            return ran;
        }
        for queued in jobs {
            worker::run_job(app, &queued.job).await.expect("run_job");
            ran.push(queued);
        }
    }
}

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
    // Queued, not sent: the worker sends it.
    assert_eq!(b["sentCount"], 0);
    assert_eq!(b["pendingCount"], 3);
    assert_eq!(b["inProgress"], true);
    assert!(b["nextBatchAt"].is_i64(), "{b}");
    assert!(app.mail.sent_raw().is_empty(), "nothing is sent inline");

    let ran = run_queued_jobs(&app).await;
    assert_eq!(ran.len(), 1, "three recipients fit in one batch");
    assert_eq!(ran[0].delay_secs, 0, "the first batch runs straight away");
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
    assert!(!stored.in_progress(), "done: no next batch");
    assert_eq!(stored.send_seq, 1);
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
    run_queued_jobs(&app).await;
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

const ADD_SENDER: &str = r#"
    mutation($instanceId: ID!, $name: String!, $address: String!) {
        addBroadcastSender(instanceId: $instanceId, name: $name, address: $address) {
            broadcastSenders { id name address }
        }
    }
"#;

const DELETE_SENDER: &str = r#"
    mutation($instanceId: ID!, $senderId: ID!) {
        deleteBroadcastSender(instanceId: $instanceId, senderId: $senderId) {
            broadcastSenders { id name address }
        }
    }
"#;

async fn add_sender(
    schema: &TestSchema,
    auth: AuthInfo,
    instance_id: &str,
    name: &str,
    address: &str,
) -> Response {
    schema
        .execute(
            Request::new(ADD_SENDER)
                .variables(Variables::from_json(json!({
                    "instanceId": instance_id,
                    "name": name,
                    "address": address,
                })))
                .data(auth),
        )
        .await
}

#[tokio::test]
async fn saved_senders_are_added_listed_and_deleted_by_an_owner_only() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) = setup_instance(&db, "snd", db::InstanceKind::Support).await;
    let (app, schema) = build_app_and_schema(db.clone());
    let owner_auth = || user_auth(&owner, &instance.id, true);

    let resp = add_sender(
        &schema,
        owner_auth(),
        &instance.id,
        " SES Activity Updates ",
        &format!("Updates@{domain}"),
    )
    .await;
    let list = expect_data(&resp, "addBroadcastSender.broadcastSenders");
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["name"], "SES Activity Updates");
    assert_eq!(list[0]["address"], format!("updates@{domain}"));
    let first_id = list[0]["id"].as_str().unwrap().to_string();

    let resp = add_sender(
        &schema,
        owner_auth(),
        &instance.id,
        "Product news",
        &format!("news@{domain}"),
    )
    .await;
    let list = expect_data(&resp, "addBroadcastSender.broadcastSenders");
    assert_eq!(list.as_array().unwrap().len(), 2, "appended in order");
    assert_eq!(list[1]["name"], "Product news");

    // Off-domain and duplicate senders are refused; nothing is stored.
    let resp = add_sender(&schema, owner_auth(), &instance.id, "X", "x@example.org").await;
    assert!(!resp.errors.is_empty());
    let resp = add_sender(
        &schema,
        owner_auth(),
        &instance.id,
        "Product news",
        &format!("news@{domain}"),
    )
    .await;
    assert!(!resp.errors.is_empty());

    // An agent can neither read nor change them.
    let agent = db
        .create_user(&unique_email("agent"), "Agent")
        .await
        .unwrap();
    let resp = add_sender(
        &schema,
        user_auth(&agent.id, &instance.id, false),
        &instance.id,
        "Agent",
        &format!("agent@{domain}"),
    )
    .await;
    assert_eq!(expect_error_code(&resp), "UNAUTHENTICATED");

    let resp = schema
        .execute(
            Request::new(DELETE_SENDER)
                .variables(Variables::from_json(json!({
                    "instanceId": instance.id,
                    "senderId": first_id,
                })))
                .data(owner_auth()),
        )
        .await;
    let list = expect_data(&resp, "deleteBroadcastSender.broadcastSenders");
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["name"], "Product news");

    // Deleting it again is NOT_FOUND.
    let resp = schema
        .execute(
            Request::new(DELETE_SENDER)
                .variables(Variables::from_json(json!({
                    "instanceId": instance.id,
                    "senderId": first_id,
                })))
                .data(owner_auth()),
        )
        .await;
    assert_eq!(expect_error_code(&resp), "NOT_FOUND");

    // The stored list survives a fresh read.
    let stored = db
        .get_instances(&[instance.id.as_str()])
        .await
        .unwrap()
        .remove(0)
        .unwrap();
    assert_eq!(stored.broadcast_senders.len(), 1);
    assert_eq!(
        stored.broadcast_senders[0].address,
        format!("news@{domain}")
    );

    // A write based on a stale read is refused, not merged.
    assert!(
        !db.set_broadcast_senders(&instance.id, &[], &[])
            .await
            .unwrap(),
        "the list changed since this (empty) read"
    );
    assert!(
        app.mail.sent_raw().is_empty(),
        "saving a sender sends nothing"
    );
}

#[tokio::test]
async fn saved_senders_are_not_found_on_an_invoicing_instance() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (invoicing, owner, _) = setup_instance(&db, "sndi", db::InstanceKind::Invoicing).await;
    let (_app, schema) = build_app_and_schema(db.clone());
    let resp = add_sender(
        &schema,
        user_auth(&owner, &invoicing.id, true),
        &invoicing.id,
        "News",
        "news@example.org",
    )
    .await;
    assert_eq!(expect_error_code(&resp), "NOT_FOUND");
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
            "destination": ["recipient@example.com"],
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
    let (app, schema) = build_app_and_schema(db.clone());
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
    run_queued_jobs(&app).await;

    let delivery = json!({ "timestamp": "2026-10-13T01:00:01.000Z", "smtpResponse": "250 OK" });
    let bounce = json!({
        "bounceType": "Permanent",
        "bounceSubType": "General",
        "bouncedRecipients": [{
            "emailAddress": "recipient@example.com",
            "diagnosticCode": "smtp; 550 5.1.1 user unknown"
        }],
        "timestamp": "2026-10-13T01:00:02.000Z",
    });
    let delay = json!({
        "delayType": "MailboxFull",
        "delayedRecipients": [{ "emailAddress": "recipient@example.com" }],
        "timestamp": "2026-10-13T01:00:03.000Z",
    });
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
        broadcast_events::apply(&db, &SesEvent::parse(&msg).unwrap())
            .await
            .expect("apply");
    }
    // Ticket mail carries no tags: ignored.
    let untagged =
        json!({ "eventType": "Bounce", "mail": { "tags": {} }, "bounce": {} }).to_string();
    broadcast_events::apply(&db, &SesEvent::parse(&untagged).unwrap())
        .await
        .unwrap();

    let stored = db.get_broadcast_consistent(&id).await.unwrap().unwrap();
    assert_eq!(stored.recipients.len(), 3);
    let r = &stored.recipients;
    assert_eq!(r[0].delivery_status, Some(db::DeliveryStatus::Delivered));
    assert_eq!(r[0].delivery_detail.as_deref(), Some("250 OK"));
    assert_eq!(r[1].delivery_status, Some(db::DeliveryStatus::Bounced));
    assert_eq!(
        r[1].delivery_detail.as_deref(),
        Some("Permanent/General: smtp; 550 5.1.1 user unknown")
    );
    assert_eq!(r[2].delivery_status, Some(db::DeliveryStatus::Complained));
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
async fn opens_and_clicks_keep_the_earliest_time_on_the_right_recipient() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) =
        setup_instance(&db, "engagement", db::InstanceKind::Support).await;
    let (_app, schema) = build_app_and_schema(db.clone());
    let recipients = vec![unique_email("reader"), unique_email("ignorer")];
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

    let at = |t: &str| json!({ "timestamp": t });
    for msg in [
        ses_event(
            "Delivery",
            &id,
            0,
            "delivery",
            json!({ "smtpResponse": "250 OK" }),
        ),
        ses_event("Open", &id, 0, "open", at("2026-10-13T03:00:00.000Z")),
        // An earlier open arriving late wins; a later one changes nothing.
        ses_event("Open", &id, 0, "open", at("2026-10-13T02:00:00.000Z")),
        ses_event("Open", &id, 0, "open", at("2026-10-13T04:00:00.000Z")),
        ses_event("Click", &id, 0, "click", at("2026-10-13T02:00:30.000Z")),
        // Out-of-range index: a no-op, not an error.
        ses_event("Open", &id, 99, "open", at("2026-10-13T02:00:00.000Z")),
    ] {
        broadcast_events::apply(&db, &SesEvent::parse(&msg).unwrap())
            .await
            .expect("apply");
    }

    let stored = db.get_broadcast_consistent(&id).await.unwrap().unwrap();
    let r = &stored.recipients;
    assert_eq!(r[0].first_opened_at, Some(1_791_856_800));
    assert_eq!(r[0].first_clicked_at, Some(1_791_856_830));
    // Engagement never touches delivery status.
    assert_eq!(r[0].delivery_status, Some(db::DeliveryStatus::Delivered));
    assert_eq!(r[1].first_opened_at, None);
    assert_eq!(r[1].first_clicked_at, None);

    let resp = schema
        .execute(
            Request::new(LIST_BROADCASTS)
                .variables(Variables::from_json(json!({ "slug": instance.slug })))
                .data(user_auth(&owner, &instance.id, true)),
        )
        .await;
    let listed = &expect_data(&resp, "instance")["broadcasts"][0];
    assert_eq!(listed["openedCount"], 1);
    assert_eq!(listed["clickedCount"], 1);
    assert_eq!(listed["recipients"][0]["openedAt"], 1_791_856_800);
    assert_eq!(listed["recipients"][1]["openedAt"], json!(null));
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
    // Known suppressed at queue time, so recorded up front.
    assert_eq!(b["pendingCount"], 1);
    assert_eq!(b["recipients"][0]["sendStatus"], "SUPPRESSED");
    assert!(
        b["recipients"][0]["error"]
            .as_str()
            .unwrap()
            .contains("hard bounce"),
        "{b}"
    );
    assert_eq!(b["recipients"][1]["sendStatus"], "PENDING");

    run_queued_jobs(&app).await;
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

// ── background sending ───────────────────────────────────────────────────

/// Queue a broadcast to `count` fresh recipients; returns its id and them.
async fn queue_broadcast(
    schema: &TestSchema,
    instance: &db::Instance,
    owner: &str,
    domain: &str,
    count: usize,
) -> (String, Vec<String>) {
    let recipients: Vec<String> = (0..count).map(|i| unique_email(&format!("r{i}"))).collect();
    let resp = send(
        schema,
        user_auth(owner, &instance.id, true),
        &instance.id,
        &format!("news@{domain}"),
        &recipients,
    )
    .await;
    let id = expect_data(&resp, "sendBroadcast")["id"]
        .as_str()
        .unwrap()
        .to_string();
    (id, recipients)
}

fn batch_job(id: &str, seq: u64) -> toolbox::queue::Job {
    toolbox::queue::Job::BroadcastBatch {
        broadcast_id: id.to_string(),
        seq,
    }
}

#[tokio::test]
async fn sends_a_batch_at_a_time_each_queueing_the_next_after_a_delay() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) = setup_instance(&db, "pace", db::InstanceKind::Support).await;
    let (app, schema) = build_app_and_schema(db.clone());
    let n = broadcast::BATCH_SIZE * 2 + 2;
    let (id, recipients) = queue_broadcast(&schema, &instance, &owner, &domain, n).await;

    let first = app.queue.drain();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].job, batch_job(&id, 0));
    worker::run_job(&*app, &first[0].job).await.unwrap();

    // Exactly one batch's worth went out, in order, and the next is queued
    // behind the pacing delay.
    let sent = app.mail.sent_raw();
    assert_eq!(sent.len(), broadcast::BATCH_SIZE);
    for (msg, email) in sent.iter().zip(&recipients) {
        assert_eq!(&msg.to[0], email);
    }
    let next = app.queue.drain();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].job, batch_job(&id, 1));
    assert_eq!(next[0].delay_secs, broadcast::BATCH_INTERVAL_SECS);
    let stored = db.get_broadcast_consistent(&id).await.unwrap().unwrap();
    assert_eq!(stored.send_seq, 1);
    assert!(stored.in_progress());

    // The rest follow, one batch per job, until nothing is queued.
    worker::run_job(&*app, &next[0].job).await.unwrap();
    let ran = run_queued_jobs(&app).await;
    assert_eq!(ran.len(), 1, "a third and final batch");
    assert_eq!(app.mail.sent_raw().len(), n);
    let stored = db.get_broadcast_consistent(&id).await.unwrap().unwrap();
    assert_eq!(stored.send_seq, 3);
    assert!(!stored.in_progress());
    assert!(
        stored
            .recipients
            .iter()
            .all(|r| r.send_status == db::BroadcastSendStatus::Sent)
    );
}

#[tokio::test]
async fn a_list_over_the_old_limit_of_100_is_accepted_and_all_sent() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) = setup_instance(&db, "big", db::InstanceKind::Support).await;
    let (app, schema) = build_app_and_schema(db.clone());

    let (id, _) = queue_broadcast(&schema, &instance, &owner, &domain, 117).await;
    let ran = run_queued_jobs(&app).await;
    assert_eq!(ran.len(), 117usize.div_ceil(broadcast::BATCH_SIZE));
    let sent = app.mail.sent_raw();
    assert_eq!(sent.len(), 117);
    let unique: std::collections::HashSet<_> = sent.iter().map(|m| m.to[0].clone()).collect();
    assert_eq!(unique.len(), 117, "nobody mailed twice");
    let stored = db.get_broadcast_consistent(&id).await.unwrap().unwrap();
    assert!(!stored.in_progress());

    // Over the cap is refused outright, before anything is queued.
    let too_many: Vec<String> = (0..=broadcast::MAX_BROADCAST_RECIPIENTS)
        .map(|i| format!("r{i}@example.com"))
        .collect();
    let resp = send(
        &schema,
        user_auth(&owner, &instance.id, true),
        &instance.id,
        &format!("news@{domain}"),
        &too_many,
    )
    .await;
    assert!(!resp.errors.is_empty());
    assert!(app.queue.drain().is_empty());
}

#[tokio::test]
async fn a_duplicate_or_stale_job_never_sends_anyone_twice() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) = setup_instance(&db, "dupe", db::InstanceKind::Support).await;
    let (app, schema) = build_app_and_schema(db.clone());
    let n = broadcast::BATCH_SIZE + 1;
    let (id, _) = queue_broadcast(&schema, &instance, &owner, &domain, n).await;
    app.queue.drain();

    // SQS delivers seq 0 twice: the second copy is stale once the first has
    // recorded its progress.
    worker::run_job(&*app, &batch_job(&id, 0)).await.unwrap();
    let outcome = toolbox::broadcast_send::run_batch(&*app, &id, 0)
        .await
        .unwrap();
    assert_eq!(outcome, toolbox::broadcast_send::BatchOutcome::Stale);
    assert_eq!(app.mail.sent_raw().len(), broadcast::BATCH_SIZE);

    // A recipient already claimed by a concurrent run is skipped, and left
    // `sending` — never retried, since it may have gone out.
    assert!(
        db.claim_broadcast_recipient(&id, broadcast::BATCH_SIZE)
            .await
            .unwrap()
    );
    let next = app.queue.drain();
    worker::run_job(&*app, &next[0].job).await.unwrap();
    assert_eq!(app.mail.sent_raw().len(), broadcast::BATCH_SIZE);
    let stored = db.get_broadcast_consistent(&id).await.unwrap().unwrap();
    assert_eq!(
        stored.recipients[broadcast::BATCH_SIZE].send_status,
        db::BroadcastSendStatus::Sending
    );
    assert!(!stored.in_progress(), "nothing left pending");
    assert!(app.queue.drain().is_empty());

    // A job for a broadcast that doesn't exist is a harmless no-op.
    assert_eq!(
        toolbox::broadcast_send::run_batch(&*app, "nonexistent0", 0)
            .await
            .unwrap(),
        toolbox::broadcast_send::BatchOutcome::Missing
    );
}

#[tokio::test]
async fn an_instance_deleted_mid_broadcast_fails_the_rest_and_stops() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) = setup_instance(&db, "gone", db::InstanceKind::Support).await;
    let (app, schema) = build_app_and_schema(db.clone());
    let n = broadcast::BATCH_SIZE + 2;
    let (id, _) = queue_broadcast(&schema, &instance, &owner, &domain, n).await;
    let first = app.queue.drain();
    worker::run_job(&*app, &first[0].job).await.unwrap();

    db.update_instance(&instance.id, db::InstanceUpdateShape::SetDeleted(true))
        .await
        .unwrap();
    let ran = run_queued_jobs(&app).await;
    assert_eq!(ran.len(), 1);
    assert_eq!(
        app.mail.sent_raw().len(),
        broadcast::BATCH_SIZE,
        "no more sent"
    );
    let stored = db.get_broadcast_consistent(&id).await.unwrap().unwrap();
    assert!(!stored.in_progress());
    for r in &stored.recipients[broadcast::BATCH_SIZE..] {
        assert_eq!(r.send_status, db::BroadcastSendStatus::Failed);
        assert!(r.error.as_deref().unwrap().contains("deleted"), "{r:?}");
    }
}

#[tokio::test]
async fn an_address_suppressed_after_queueing_is_skipped_at_send_time() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) = setup_instance(&db, "late", db::InstanceKind::Support).await;
    let (app, schema) = build_app_and_schema(db.clone());
    let (id, recipients) = queue_broadcast(&schema, &instance, &owner, &domain, 2).await;

    db.record_email_suppression(&recipients[0], db::SuppressionReason::Complaint, None, 1)
        .await
        .unwrap();
    run_queued_jobs(&app).await;

    let sent = app.mail.sent_raw();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, vec![recipients[1].clone()]);
    let stored = db.get_broadcast_consistent(&id).await.unwrap().unwrap();
    assert_eq!(
        stored.recipients[0].send_status,
        db::BroadcastSendStatus::Suppressed
    );
}

#[tokio::test]
async fn a_broadcast_that_cannot_be_queued_fails_and_sends_nothing() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) = setup_instance(&db, "noq", db::InstanceKind::Support).await;
    let (app, schema) = build_app_and_schema(db.clone());
    app.queue.set_failing(true);

    let resp = send(
        &schema,
        user_auth(&owner, &instance.id, true),
        &instance.id,
        &format!("news@{domain}"),
        &[unique_email("a"), unique_email("b")],
    )
    .await;
    assert!(!resp.errors.is_empty(), "{resp:?}");
    assert!(app.mail.sent_raw().is_empty());

    let rows = db.list_broadcasts(&instance.id, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].in_progress());
    assert!(
        rows[0]
            .recipients
            .iter()
            .all(|r| r.send_status == db::BroadcastSendStatus::Failed)
    );
}

// ── drafts, test sends and HTML ──────────────────────────────────────────

const SAVE_DRAFT: &str = r#"
    mutation($instanceId: ID!, $input: SaveBroadcastDraftInput!) {
        saveBroadcastDraft(instanceId: $instanceId, input: $input) {
            id isDraft subject body html missingAssets lastTestSentAt inProgress
            recipients { email sendStatus }
        }
    }
"#;

const SEND_TEST: &str = r#"
    mutation($instanceId: ID!, $broadcastId: ID!, $to: [String!]!) {
        sendBroadcastTest(instanceId: $instanceId, broadcastId: $broadcastId, to: $to) {
            id isDraft lastTestSentAt
        }
    }
"#;

const SEND_DRAFT: &str = r#"
    mutation($instanceId: ID!, $broadcastId: ID!) {
        sendBroadcastDraft(instanceId: $instanceId, broadcastId: $broadcastId) {
            id isDraft inProgress pendingCount recipientCount
        }
    }
"#;

const DELETE_DRAFT: &str = r#"
    mutation($instanceId: ID!, $broadcastId: ID!) {
        deleteBroadcastDraft(instanceId: $instanceId, broadcastId: $broadcastId)
    }
"#;

const ASSET_UPLOAD: &str = r#"
    mutation($instanceId: ID!, $filename: String!, $contentType: String!, $size: Int!) {
        createBroadcastAssetUpload(
            instanceId: $instanceId, filename: $filename, contentType: $contentType, size: $size
        ) { uploadUrl url }
    }
"#;

const DRAFT_HTML: &str = r#"<!DOCTYPE html><html><head><style>h1{color:#c00}</style></head>
<body><h1>Big news</h1><p>We shipped it.</p><img src="images/logo.png" alt="Logo">
<script>alert(1)</script></body></html>"#;

async fn run(
    schema: &TestSchema,
    auth: &AuthInfo,
    query: &str,
    vars: serde_json::Value,
) -> Response {
    schema
        .execute(
            Request::new(query)
                .variables(Variables::from_json(vars))
                .data(auth.clone()),
        )
        .await
}

#[tokio::test]
async fn an_html_draft_is_built_up_test_sent_and_then_sent_for_real() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) = setup_instance(&db, "draft", db::InstanceKind::Support).await;
    let (app, schema) = build_app_and_schema(db.clone());
    let auth = user_auth(&owner, &instance.id, true);
    let recipients = vec![unique_email("alice"), unique_email("bob")];
    let draft_input = |id: Option<&str>, assets: serde_json::Value| {
        json!({
            "instanceId": instance.id,
            "input": {
                "id": id,
                "fromAddress": format!("news@{domain}"),
                "subject": "Big news",
                "html": DRAFT_HTML,
                "assetUrls": assets,
                "recipients": recipients,
            },
        })
    };

    // Saved with its image not yet uploaded: reported, not refused.
    let resp = run(&schema, &auth, SAVE_DRAFT, draft_input(None, json!([]))).await;
    let d = expect_data(&resp, "saveBroadcastDraft");
    let id = d["id"].as_str().unwrap().to_string();
    assert_eq!(d["isDraft"], true);
    assert_eq!(d["inProgress"], false);
    assert_eq!(d["missingAssets"], json!(["images/logo.png"]));
    assert!(!d["html"].as_str().unwrap().contains("<script"), "{d}");
    assert!(app.queue.drain().is_empty(), "a draft queues nothing");

    // A test send needs every image.
    let me = unique_email("owner-test");
    let test_vars = json!({ "instanceId": instance.id, "broadcastId": id, "to": [me] });
    let resp = run(&schema, &auth, SEND_TEST, test_vars.clone()).await;
    assert!(
        format!("{:?}", resp.errors).contains("images/logo.png"),
        "{resp:?}"
    );

    // Upload it, then save with its URL.
    let resp = run(
        &schema,
        &auth,
        ASSET_UPLOAD,
        json!({ "instanceId": instance.id, "filename": "images/logo.png",
                "contentType": "image/png", "size": 1234 }),
    )
    .await;
    let up = expect_data(&resp, "createBroadcastAssetUpload");
    let url = up["url"].as_str().unwrap().to_string();
    assert!(
        url.contains(&format!("/email-assets/{}/", instance.id)) && url.ends_with("/logo.png"),
        "{url}"
    );
    let resp = run(
        &schema,
        &auth,
        SAVE_DRAFT,
        draft_input(
            Some(&id),
            json!([{ "path": "images/logo.png", "url": url }]),
        ),
    )
    .await;
    let d = expect_data(&resp, "saveBroadcastDraft");
    assert_eq!(d["id"], id.as_str(), "the same draft, updated");
    assert_eq!(d["missingAssets"], json!([]));
    assert!(d["html"].as_str().unwrap().contains(&url), "{d}");

    // The test goes to `to` only: untagged, [TEST] subject, nothing queued.
    let resp = run(&schema, &auth, SEND_TEST, test_vars).await;
    let t = expect_data(&resp, "sendBroadcastTest");
    assert!(t["lastTestSentAt"].is_i64(), "{t}");
    let sent = app.mail.sent_raw();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, vec![me.clone()]);
    assert!(sent[0].tags.is_empty(), "a test is not tracked");
    let raw = String::from_utf8_lossy(&sent[0].raw);
    assert!(raw.contains("Subject: [TEST] Big news"), "{raw}");
    assert!(app.queue.drain().is_empty(), "a test queues nothing");
    let stored = db.get_broadcast_consistent(&id).await.unwrap().unwrap();
    assert!(stored.draft);
    assert!(
        stored
            .recipients
            .iter()
            .all(|r| r.send_status == db::BroadcastSendStatus::Pending),
        "the test address is never a recipient"
    );
    app.mail.clear();

    // Send it for real: the usual paced batches, HTML and all.
    let send_vars = json!({ "instanceId": instance.id, "broadcastId": id });
    let resp = run(&schema, &auth, SEND_DRAFT, send_vars.clone()).await;
    let b = expect_data(&resp, "sendBroadcastDraft");
    assert_eq!(b["isDraft"], false);
    assert_eq!(b["inProgress"], true);
    assert_eq!(b["pendingCount"], 2);
    run_queued_jobs(&app).await;
    let sent = app.mail.sent_raw();
    assert_eq!(sent.len(), 2);
    for msg in &sent {
        let parsed = mail_parser::MessageParser::new().parse(&msg.raw).unwrap();
        assert_eq!(parsed.subject(), Some("Big news"));
        let html = parsed.body_html(0).unwrap();
        assert!(html.contains(&url), "{html}");
        assert!(
            !html.contains("Thanks,"),
            "no signature on an HTML broadcast"
        );
        assert_eq!(msg.tags.len(), 2, "tracked like any broadcast copy");
    }

    // Sent once only, and no longer editable or deletable.
    let resp = run(&schema, &auth, SEND_DRAFT, send_vars.clone()).await;
    assert_eq!(expect_error_code(&resp), "CONFLICT");
    let resp = run(
        &schema,
        &auth,
        SAVE_DRAFT,
        draft_input(Some(&id), json!([])),
    )
    .await;
    assert_eq!(expect_error_code(&resp), "CONFLICT");
    let resp = run(&schema, &auth, DELETE_DRAFT, send_vars).await;
    assert_eq!(expect_error_code(&resp), "CONFLICT");
}

#[tokio::test]
async fn saving_a_draft_again_clears_its_test_and_a_draft_can_be_deleted() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) =
        setup_instance(&db, "draftdel", db::InstanceKind::Support).await;
    let (_app, schema) = build_app_and_schema(db.clone());
    let auth = user_auth(&owner, &instance.id, true);
    let save = |id: Option<String>| {
        json!({
            "instanceId": instance.id,
            "input": {
                "id": id,
                "fromAddress": format!("news@{domain}"),
                "subject": "Plain draft",
                "body": "Hello",
            },
        })
    };
    let resp = run(&schema, &auth, SAVE_DRAFT, save(None)).await;
    let id = expect_data(&resp, "saveBroadcastDraft.id")
        .as_str()
        .unwrap()
        .to_string();
    let resp = run(
        &schema,
        &auth,
        SEND_TEST,
        json!({ "instanceId": instance.id, "broadcastId": id, "to": [unique_email("me")] }),
    )
    .await;
    assert!(expect_data(&resp, "sendBroadcastTest.lastTestSentAt").is_i64());

    let resp = run(&schema, &auth, SAVE_DRAFT, save(Some(id.clone()))).await;
    assert_eq!(
        expect_data(&resp, "saveBroadcastDraft.lastTestSentAt"),
        serde_json::Value::Null
    );

    // A draft with no recipients can't be sent.
    let vars = json!({ "instanceId": instance.id, "broadcastId": id });
    let resp = run(&schema, &auth, SEND_DRAFT, vars.clone()).await;
    assert!(
        format!("{:?}", resp.errors).contains("at least one recipient"),
        "{resp:?}"
    );

    let resp = run(&schema, &auth, DELETE_DRAFT, vars.clone()).await;
    assert_eq!(expect_data(&resp, "deleteBroadcastDraft"), id.as_str());
    assert!(db.get_broadcast_consistent(&id).await.unwrap().is_none());
    let resp = run(&schema, &auth, DELETE_DRAFT, vars).await;
    assert_eq!(expect_error_code(&resp), "NOT_FOUND");
}

#[tokio::test]
async fn another_instances_owner_cannot_touch_a_draft() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, domain) =
        setup_instance(&db, "draftiso", db::InstanceKind::Support).await;
    let (other, other_owner, _) = setup_instance(&db, "draftiso2", db::InstanceKind::Support).await;
    let (_app, schema) = build_app_and_schema(db.clone());
    let resp = run(
        &schema,
        &user_auth(&owner, &instance.id, true),
        SAVE_DRAFT,
        json!({
            "instanceId": instance.id,
            "input": { "fromAddress": format!("news@{domain}"), "subject": "S", "body": "B" },
        }),
    )
    .await;
    let id = expect_data(&resp, "saveBroadcastDraft.id")
        .as_str()
        .unwrap()
        .to_string();

    // Through their own instance id (which their guard passes): not found.
    let auth = user_auth(&other_owner, &other.id, true);
    let vars = json!({ "instanceId": other.id, "broadcastId": id });
    for query in [SEND_DRAFT, DELETE_DRAFT] {
        let resp = run(&schema, &auth, query, vars.clone()).await;
        assert_eq!(expect_error_code(&resp), "NOT_FOUND");
    }
    let resp = run(
        &schema,
        &auth,
        SEND_TEST,
        json!({ "instanceId": other.id, "broadcastId": id, "to": ["x@example.com"] }),
    )
    .await;
    assert_eq!(expect_error_code(&resp), "NOT_FOUND");
}

#[tokio::test]
async fn asset_uploads_take_only_small_raster_images() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner, _) = setup_instance(&db, "asset", db::InstanceKind::Support).await;
    let (_app, schema) = build_app_and_schema(db.clone());
    let auth = user_auth(&owner, &instance.id, true);
    for (ct, size) in [
        ("image/svg+xml", 100),
        ("text/html", 100),
        ("image/png", 6_000_000),
    ] {
        let resp = run(
            &schema,
            &auth,
            ASSET_UPLOAD,
            json!({ "instanceId": instance.id, "filename": "a", "contentType": ct, "size": size }),
        )
        .await;
        assert!(!resp.errors.is_empty(), "{ct} {size} should be refused");
    }
}
