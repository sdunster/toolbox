//! Integration tests for bounce/complaint suppression (issue #6) against
//! **DynamoDB Local** and the **mock** mailer: that an SES feedback event
//! lands in `email_suppression` (idempotently, first reason winning), that
//! `replyToTicket` and the close notice skip a suppressed address — on the
//! wire and on the stored row — that a reply with nobody left to mail is
//! refused before anything is persisted, and that `Ticket.undeliverableEmails`
//! reports it to members only.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test suppression_dynamodb_local
//! ```
//!
//! Like the other `*_dynamodb_local.rs` files, every test here **skips
//! itself** when no reachable local DynamoDB is configured.

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
use toolbox::suppression;

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
            "suppression_dynamodb_local: {endpoint} is configured but not reachable — skipping. \
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
                    "suppression_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set to \
                     a reachable local DynamoDB — skipping. See this file's header for how to run it."
                );
                return;
            }
        }
    };
}

/// Lowercase, like every address the ticket mutations and the inbound
/// pipeline store.
fn unique_email(label: &str) -> String {
    format!("{label}-{}@example.com", nanoid::nanoid!(8)).to_lowercase()
}

fn bounce_event(email: &str) -> String {
    json!({
        "eventType": "Bounce",
        "bounce": {
            "bounceType": "Permanent",
            "bounceSubType": "General",
            "bouncedRecipients": [{
                "emailAddress": email,
                "status": "5.1.1",
                "diagnosticCode": "smtp; 550 5.1.1 user unknown"
            }]
        },
        "mail": {}
    })
    .to_string()
}

fn complaint_event(email: &str) -> String {
    json!({
        "eventType": "Complaint",
        "complaint": {
            "complainedRecipients": [{ "emailAddress": email }],
            "complaintFeedbackType": "abuse"
        },
        "mail": {}
    })
    .to_string()
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

fn error_code(response: &Response) -> Option<String> {
    response.errors.first().and_then(|e| {
        e.extensions
            .as_ref()
            .and_then(|ext| ext.get("code"))
            .map(|v| v.to_string().trim_matches('"').to_string())
    })
}

fn user_auth(user_id: &str, instance_id: &str) -> AuthInfo {
    AuthInfo::User {
        id: user_id.to_string(),
        memberships: vec![Membership {
            instance_id: instance_id.to_string(),
            is_owner: true,
        }],
        is_superuser: false,
        token_id: None,
        grant_id: None,
    }
}

/// An instance with one inbound address and an owner; returns
/// `(instance, owner_user_id)`.
async fn setup_instance(db: &dynamodb::Handler, label: &str) -> (db::Instance, String) {
    let instance = db
        .create_instance(
            &format!("{label} instance"),
            &format!("{label}-{}", nanoid::nanoid!(8).to_lowercase()),
            &format!("{label} Support"),
            "Thanks",
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
    (instance, owner.id)
}

async fn make_ticket(
    db: &dynamodb::Handler,
    instance_id: &str,
    requester: &str,
    cc: &str,
) -> db::Ticket {
    let number = db
        .increment_ticket_counter(instance_id)
        .await
        .expect("increment_ticket_counter");
    db.create_ticket(
        instance_id,
        number,
        "Where is my order?",
        &[requester.to_string()],
        &[cc.to_string()],
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
        replyToTicket(ticketId: $ticketId, body: $body) { id toEmails ccEmails }
    }
"#;

const UNDELIVERABLE_QUERY: &str = r#"
    query($id: ID!) {
        ticket(id: $id) { undeliverableEmails { email reason suppressedAt } }
    }
"#;

#[tokio::test]
async fn feedback_is_recorded_idempotently_and_the_first_reason_wins() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (my_app, _schema) = build_app_and_schema(db);
    let email = unique_email("complainer");

    // Mixed case on the wire; stored lowercased.
    let n = suppression::record_feedback(&*my_app, &complaint_event(&email.to_uppercase()))
        .await
        .expect("record complaint");
    assert_eq!(n, 1);
    // SES then bounces the address as suppressed: same row, reason kept.
    suppression::record_feedback(&*my_app, &bounce_event(&email))
        .await
        .expect("record bounce");

    let row = my_app
        .db
        .get_email_suppressions(&[email.as_str()])
        .await
        .expect("get")
        .into_iter()
        .next()
        .flatten()
        .expect("row exists");
    assert_eq!(row.email, email);
    assert_eq!(row.reason, db::SuppressionReason::Complaint);
    assert_eq!(row.detail.as_deref(), Some("abuse"));
    assert_eq!(row.event_count, 2);

    // A transient bounce records nothing.
    let other = unique_email("full-mailbox");
    let transient = bounce_event(&other).replace("Permanent", "Transient");
    assert_eq!(
        suppression::record_feedback(&*my_app, &transient)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        my_app
            .db
            .get_email_suppressions(&[other.as_str()])
            .await
            .unwrap(),
        vec![None]
    );

    assert!(my_app.db.delete_email_suppression(&email).await.unwrap());
    assert!(!my_app.db.delete_email_suppression(&email).await.unwrap());
}

#[tokio::test]
async fn reply_and_close_notice_skip_a_suppressed_cc() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id) = setup_instance(&db, "supp").await;
    let requester = unique_email("requester");
    let dead_cc = unique_email("dead-cc");
    let ticket = make_ticket(&db, &instance.id, &requester, &dead_cc).await;
    let (my_app, schema) = build_app_and_schema(db);

    suppression::record_feedback(&*my_app, &bounce_event(&dead_cc))
        .await
        .unwrap();

    let response = schema
        .execute(
            Request::new(REPLY_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "body": "On its way.",
                })))
                .data(user_auth(&owner_id, &instance.id)),
        )
        .await;
    // The stored row says who was actually mailed.
    assert_eq!(
        expect_data(&response, "replyToTicket.toEmails"),
        json!([requester])
    );
    assert_eq!(expect_data(&response, "replyToTicket.ccEmails"), json!([]));

    let sent = my_app.mail.sent_raw();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].to, vec![requester.clone()]);
    assert!(sent[0].cc.is_empty(), "suppressed CC must not be mailed");

    // The close notice skips it too.
    my_app.mail.clear();
    let response = schema
        .execute(
            Request::new(
                r#"mutation($id: ID!) { setTicketStatus(ticketId: $id, status: CLOSED) { id } }"#,
            )
            .variables(Variables::from_json(json!({ "id": ticket.id })))
            .data(user_auth(&owner_id, &instance.id)),
        )
        .await;
    expect_data(&response, "setTicketStatus.id");
    let sent = my_app.mail.sent_raw();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].cc.is_empty(), "{sent:?}");

    // Members see why.
    let response = schema
        .execute(
            Request::new(UNDELIVERABLE_QUERY)
                .variables(Variables::from_json(json!({ "id": ticket.id })))
                .data(user_auth(&owner_id, &instance.id)),
        )
        .await;
    let list = expect_data(&response, "ticket.undeliverableEmails");
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["email"], json!(dead_cc));
    assert_eq!(list[0]["reason"], json!("HARD_BOUNCE"));
}

#[tokio::test]
async fn reply_with_every_recipient_suppressed_is_refused_before_persisting() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, owner_id) = setup_instance(&db, "suppall").await;
    let requester = unique_email("gone");
    let cc = unique_email("spam-reporter");
    let ticket = make_ticket(&db, &instance.id, &requester, &cc).await;
    let (my_app, schema) = build_app_and_schema(db);

    suppression::record_feedback(&*my_app, &bounce_event(&requester))
        .await
        .unwrap();
    suppression::record_feedback(&*my_app, &complaint_event(&cc))
        .await
        .unwrap();

    let response = schema
        .execute(
            Request::new(REPLY_MUTATION)
                .variables(Variables::from_json(json!({
                    "ticketId": ticket.id,
                    "body": "Hello?",
                })))
                .data(user_auth(&owner_id, &instance.id)),
        )
        .await;
    assert_eq!(
        error_code(&response).as_deref(),
        Some("CONFLICT"),
        "{response:?}"
    );
    let message = response.errors[0].message.clone();
    assert!(message.contains(&requester), "{message}");
    assert!(message.contains(&cc), "{message}");
    assert!(my_app.mail.sent_raw().is_empty());
    let messages = my_app.db.list_ticket_messages(&ticket.id).await.unwrap();
    assert!(
        messages
            .iter()
            .all(|m| m.kind != db::TicketMessageKind::Reply),
        "no reply row may be persisted: {messages:?}"
    );
}

#[tokio::test]
async fn undeliverable_emails_is_members_only() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance, _owner_id) = setup_instance(&db, "suppauth").await;
    let requester = unique_email("requester");
    let ticket = make_ticket(&db, &instance.id, &requester, &unique_email("cc")).await;
    let (_my_app, schema) = build_app_and_schema(db);

    let response = schema
        .execute(
            Request::new(UNDELIVERABLE_QUERY)
                .variables(Variables::from_json(json!({ "id": ticket.id })))
                .data(AuthInfo::Requester {
                    email: requester.clone(),
                    instance_id: instance.id.clone(),
                }),
        )
        .await;
    assert_eq!(
        error_code(&response).as_deref(),
        Some("FORBIDDEN"),
        "{response:?}"
    );
}
