//! Integration tests for instance-scoped API tokens
//! (`createApiToken`/`updateApiToken`/`deleteApiToken`/`Instance.apiTokens`)
//! and `submitVerifiedTicket` against **DynamoDB Local** — not the mocks.
//! `auth::verify_token`'s `mta_` branch, the `api_token` table's
//! `instance_id-index`, and `submit_verified_ticket`'s recipient validation
//! only show up against a real table and a real end-to-end token round trip,
//! so this is the one place that can prove:
//!
//!   - an owner mints a token, it's listed, and the returned secret
//!     authenticates through `auth::verify_token` to `AuthInfo::ApiToken`;
//!   - `submitVerifiedTicket` creates a ticket with the right requesters/CCs,
//!     the right first message, and sends the acknowledgement (mocked mail);
//!   - a disabled token, a deleted token, and a wrong secret against a valid
//!     id all fail auth;
//!   - a plain agent (non-owner, non-superuser) cannot list/create/update/
//!     delete tokens, and an owner of instance A cannot touch instance B's
//!     token;
//!   - an `ApiToken` principal cannot call `submitTicket`,
//!     `createAttachmentUpload`, or query `me` — the guard truth table in
//!     `src/graphql/auth.rs` proven end to end, not just in the pure unit test;
//!   - a deleted instance, an own-inbound-address recipient, and more than 20
//!     combined recipients are all rejected.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test api_token_dynamodb_local
//! ```
//!
//! Like the other `*_dynamodb_local.rs` files, every test here **skips
//! itself** when no reachable local DynamoDB is configured, so `cargo test`
//! and CI stay green with no local stack running. Every row is created fresh
//! per test run (nanoid'd ids/emails), so repeated runs never collide.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use toolbox::app;
use toolbox::app::HasDb as _;
use toolbox::auth::{self, AuthInfo, Membership};
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

/// See `tests/auth_dynamodb_local.rs`'s identically-named helper for the full
/// rationale.
async fn local_db_prefix() -> Option<String> {
    let endpoint = toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;

    let client = toolbox::local_dev::dynamodb_client().await;
    if client.list_tables().send().await.is_err() {
        eprintln!(
            "api_token_dynamodb_local: {endpoint} is configured but not reachable — skipping. \
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
                    "api_token_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set to a \
                     reachable local DynamoDB — skipping. See this file's header for how to run it."
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
    format!("{label}-{}@example.com", nanoid::nanoid!(8)).to_lowercase()
}

fn expect_data(response: &Response, path: &str) -> Value {
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

/// Mirrors `tickets_dynamodb_local.rs`'s identically-named helper.
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

fn owner_auth(user_id: &str, instance_id: &str) -> AuthInfo {
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

fn agent_auth(user_id: &str, instance_id: &str) -> AuthInfo {
    AuthInfo::User {
        id: user_id.to_string(),
        memberships: vec![Membership {
            instance_id: instance_id.to_string(),
            is_owner: false,
        }],
        is_superuser: false,
        token_id: None,
        grant_id: None,
    }
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

/// Create an instance + one owner user + membership + one inbound address,
/// returning `(instance_id, instance_slug, owner_user_id)`. Every test
/// starts from this — the inbound address exists so `submitVerifiedTicket`
/// has somewhere to send the acknowledgement from, and so the
/// own-address-rejection test has a real address to reuse.
async fn setup_instance(db: &dynamodb::Handler, label: &str) -> (String, String, String) {
    let slug = unique_id(&format!("{label}-slug"));
    let instance = db
        .create_instance(
            &unique_id(label),
            &slug,
            label,
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    db.create_inbound_address(
        &format!("support@{label}.example.com"),
        &instance.id,
        db::AddressKind::Exact,
    )
    .await
    .expect("create_inbound_address");
    let owner = db
        .create_user(&unique_email(&format!("{label}-owner")), "Owner")
        .await
        .expect("create_user");
    db.create_membership(&owner.id, &instance.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership");
    (instance.id, instance.slug, owner.id)
}

const CREATE_TOKEN_MUTATION: &str = r#"
    mutation($instanceId: ID!, $name: String!) {
        createApiToken(instanceId: $instanceId, name: $name) {
            token
            apiToken { id name enabled createdAt lastUsedAt }
        }
    }
"#;

const UPDATE_TOKEN_MUTATION: &str = r#"
    mutation($id: ID!, $name: String!, $enabled: Boolean!) {
        updateApiToken(id: $id, name: $name, enabled: $enabled) { id name enabled }
    }
"#;

const DELETE_TOKEN_MUTATION: &str = r#"
    mutation($id: ID!) {
        deleteApiToken(id: $id)
    }
"#;

// `instance(slug:)`, not `adminInstance(id:)` — the member-facing query, so
// this exercises an owner reaching `apiTokens` through the same path the web
// admin UI does, not a superuser-only entry point layered on top.
const LIST_TOKENS_QUERY: &str = r#"
    query($slug: String!) {
        instance(slug: $slug) { apiTokens { id name enabled } }
    }
"#;

const SUBMIT_VERIFIED_TICKET_MUTATION: &str = r#"
    mutation($subject: String!, $body: String!, $to: [String!]!, $cc: [String!]!) {
        submitVerifiedTicket(subject: $subject, body: $body, to: $to, cc: $cc) {
            id number subjectTag
        }
    }
"#;

async fn create_token(
    schema: &TestSchema,
    instance_id: &str,
    name: &str,
    auth: AuthInfo,
) -> Response {
    schema
        .execute(
            Request::new(CREATE_TOKEN_MUTATION)
                .variables(Variables::from_json(
                    json!({ "instanceId": instance_id, "name": name }),
                ))
                .data(auth),
        )
        .await
}

#[tokio::test]
async fn owner_mints_a_token_it_authenticates_and_lists() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, slug, owner_id) = setup_instance(&db, "mint").await;
    let (my_app, schema) = build_app_and_schema(db);

    let response = create_token(
        &schema,
        &instance_id,
        "Partner portal",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    let created = expect_data(&response, "createApiToken");
    let secret = created["token"]
        .as_str()
        .expect("token is a string")
        .to_string();
    assert!(secret.starts_with(auth::API_TOKEN_PREFIX));
    assert_eq!(created["apiToken"]["name"], "Partner portal");
    assert_eq!(created["apiToken"]["enabled"], json!(true));
    assert_eq!(created["apiToken"]["lastUsedAt"], Value::Null);

    // The returned secret authenticates through `verify_token` to a
    // correctly-scoped `AuthInfo::ApiToken`.
    let resolved = auth::verify_token(&*my_app, &secret)
        .await
        .expect("freshly minted token must verify");
    match resolved {
        AuthInfo::ApiToken {
            instance_id: resolved_instance,
            ..
        } => assert_eq!(resolved_instance, instance_id),
        _ => panic!("expected AuthInfo::ApiToken"),
    }

    // Listed via Instance.apiTokens (owner-or-superuser), reached as a plain
    // member/owner through `instance(slug:)`.
    let list_response = schema
        .execute(
            Request::new(LIST_TOKENS_QUERY)
                .variables(Variables::from_json(json!({ "slug": slug })))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    let tokens = expect_data(&list_response, "instance.apiTokens");
    let tokens = tokens.as_array().expect("apiTokens array");
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0]["name"], "Partner portal");
}

#[tokio::test]
async fn a_disabled_token_and_a_wrong_secret_fail_auth() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _slug, owner_id) = setup_instance(&db, "baddisable").await;
    let (my_app, schema) = build_app_and_schema(db);

    let response = create_token(
        &schema,
        &instance_id,
        "To disable",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    let created = expect_data(&response, "createApiToken");
    let secret = created["token"].as_str().unwrap().to_string();
    let id = created["apiToken"]["id"].as_str().unwrap().to_string();

    // A wrong secret against a real id fails permanently — never a mutation
    // error, since verify_token is called outside the schema.
    let (prefix_part, _) = secret.split_once('.').unwrap();
    let wrong = format!("{prefix_part}.not-the-real-secret-at-all");
    let result = auth::verify_token(&*my_app, &wrong).await;
    assert!(
        matches!(result, Err(auth::AuthError::Permanent(_))),
        "a wrong secret against a valid id must fail permanently"
    );

    // Still valid before disabling.
    assert!(auth::verify_token(&*my_app, &secret).await.is_ok());

    let disable_response = schema
        .execute(
            Request::new(UPDATE_TOKEN_MUTATION)
                .variables(Variables::from_json(
                    json!({ "id": id, "name": "To disable", "enabled": false }),
                ))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    assert_eq!(
        expect_data(&disable_response, "updateApiToken.enabled"),
        json!(false)
    );

    let result = auth::verify_token(&*my_app, &secret).await;
    assert!(
        matches!(result, Err(auth::AuthError::Permanent(_))),
        "a disabled token must fail permanently"
    );
}

#[tokio::test]
async fn a_deleted_token_fails_auth() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _slug, owner_id) = setup_instance(&db, "deltoken").await;
    let (my_app, schema) = build_app_and_schema(db);

    let response = create_token(
        &schema,
        &instance_id,
        "To delete",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    let created = expect_data(&response, "createApiToken");
    let secret = created["token"].as_str().unwrap().to_string();
    let id = created["apiToken"]["id"].as_str().unwrap().to_string();

    let delete_response = schema
        .execute(
            Request::new(DELETE_TOKEN_MUTATION)
                .variables(Variables::from_json(json!({ "id": id })))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    assert_eq!(expect_data(&delete_response, "deleteApiToken"), json!(true));

    let result = auth::verify_token(&*my_app, &secret).await;
    assert!(
        matches!(result, Err(auth::AuthError::Permanent(_))),
        "a deleted token must fail permanently"
    );
}

#[tokio::test]
async fn a_plain_agent_cannot_manage_tokens() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, slug, owner_id) = setup_instance(&db, "agentcant").await;
    let agent = db
        .create_user(&unique_email("agentcant-agent"), "Agent")
        .await
        .expect("create_user");
    db.create_membership(&agent.id, &instance_id, db::MembershipRole::Agent)
        .await
        .expect("create_membership");
    let (_my_app, schema) = build_app_and_schema(db);

    // Cannot create.
    let create_response = create_token(
        &schema,
        &instance_id,
        "Should fail",
        agent_auth(&agent.id, &instance_id),
    )
    .await;
    assert_eq!(expect_error_code(&create_response), "UNAUTHENTICATED");

    // Cannot list (Instance.apiTokens is owner-or-superuser guarded, even
    // though the agent is a real member and `instance(slug:)` itself
    // resolves fine for them).
    let list_response = schema
        .execute(
            Request::new(LIST_TOKENS_QUERY)
                .variables(Variables::from_json(json!({ "slug": slug })))
                .data(agent_auth(&agent.id, &instance_id)),
        )
        .await;
    assert_eq!(expect_error_code(&list_response), "UNAUTHENTICATED");

    // An owner mints one so update/delete-by-agent has a real id to probe.
    let owner_created = create_token(
        &schema,
        &instance_id,
        "Owned by owner",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    let id = expect_data(&owner_created, "createApiToken.apiToken.id")
        .as_str()
        .unwrap()
        .to_string();

    let update_response = schema
        .execute(
            Request::new(UPDATE_TOKEN_MUTATION)
                .variables(Variables::from_json(
                    json!({ "id": id, "name": "Renamed by agent", "enabled": true }),
                ))
                .data(agent_auth(&agent.id, &instance_id)),
        )
        .await;
    assert_eq!(expect_error_code(&update_response), "NOT_FOUND");

    let delete_response = schema
        .execute(
            Request::new(DELETE_TOKEN_MUTATION)
                .variables(Variables::from_json(json!({ "id": id })))
                .data(agent_auth(&agent.id, &instance_id)),
        )
        .await;
    assert_eq!(expect_error_code(&delete_response), "NOT_FOUND");
}

#[tokio::test]
async fn an_owner_of_one_instance_cannot_touch_another_instances_token() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_a, _slug_a, owner_a) = setup_instance(&db, "crossa").await;
    let (instance_b, _slug_b, owner_b) = setup_instance(&db, "crossb").await;
    let (_my_app, schema) = build_app_and_schema(db);

    let created = create_token(
        &schema,
        &instance_a,
        "Instance A's token",
        owner_auth(&owner_a, &instance_a),
    )
    .await;
    let id = expect_data(&created, "createApiToken.apiToken.id")
        .as_str()
        .unwrap()
        .to_string();

    // owner_b is a real owner — just of a different instance — so this
    // proves the per-record check, not just "must be a User".
    let update_response = schema
        .execute(
            Request::new(UPDATE_TOKEN_MUTATION)
                .variables(Variables::from_json(
                    json!({ "id": id, "name": "Stolen", "enabled": true }),
                ))
                .data(owner_auth(&owner_b, &instance_b)),
        )
        .await;
    assert_eq!(expect_error_code(&update_response), "NOT_FOUND");

    let delete_response = schema
        .execute(
            Request::new(DELETE_TOKEN_MUTATION)
                .variables(Variables::from_json(json!({ "id": id })))
                .data(owner_auth(&owner_b, &instance_b)),
        )
        .await;
    assert_eq!(expect_error_code(&delete_response), "NOT_FOUND");
}

#[tokio::test]
async fn submit_verified_ticket_creates_a_ticket_with_the_right_recipients() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, slug, owner_id) = setup_instance(&db, "submitok").await;
    let (my_app, schema) = build_app_and_schema(db);

    let created = create_token(
        &schema,
        &instance_id,
        "Verified submitter",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    let secret = expect_data(&created, "createApiToken.token")
        .as_str()
        .unwrap()
        .to_string();
    let auth_info = auth::verify_token(&*my_app, &secret)
        .await
        .expect("token must verify");

    let to_email = unique_email("requester");
    let cc_email = unique_email("cc");
    let response = schema
        .execute(
            Request::new(SUBMIT_VERIFIED_TICKET_MUTATION)
                .variables(Variables::from_json(json!({
                    "subject": "Trouble logging in",
                    "body": "I can't log in to my account.",
                    "to": [to_email.clone(), to_email.clone()], // dup, must collapse to one
                    "cc": [cc_email.clone(), to_email.clone()], // cc dup of to, must drop
                })))
                .data(auth_info),
        )
        .await;
    let submitted = expect_data(&response, "submitVerifiedTicket");
    let ticket_id = submitted["id"].as_str().unwrap().to_string();
    assert_eq!(
        submitted["subjectTag"],
        json!(db::ticket_subject_tag(
            &slug,
            submitted["number"].as_u64().unwrap()
        ))
    );

    let ticket = my_app
        .db()
        .get_tickets(&[ticket_id.as_str()])
        .await
        .expect("get_tickets")
        .into_iter()
        .next()
        .flatten()
        .expect("ticket exists");
    assert_eq!(ticket.requester_emails, vec![to_email.clone()]);
    assert_eq!(ticket.cc_emails, vec![cc_email.clone()]);

    // Two messages: the submitter's own (kind: INBOUND, from open_submitted_ticket)
    // plus the best-effort acknowledgement send_system_notification persists
    // (kind: SYSTEM) — mirroring submitTicket's own shape.
    let messages = my_app
        .db()
        .list_ticket_messages(&ticket_id)
        .await
        .expect("list_ticket_messages");
    assert_eq!(messages.len(), 2);
    let inbound = messages
        .iter()
        .find(|m| m.kind == db::TicketMessageKind::Inbound)
        .expect("an inbound message must exist");
    assert_eq!(inbound.from_email.as_deref(), Some(to_email.as_str()));
    assert_eq!(inbound.author_user_id, None);
    assert!(
        messages
            .iter()
            .any(|m| m.kind == db::TicketMessageKind::System),
        "the best-effort acknowledgement should be persisted as a system message"
    );

    // Best-effort acknowledgement sent to both to and cc — `send_raw` (ticket
    // mail, unlike the login-code path) is recorded in `sent_raw`, not
    // `sent`.
    let sent_raw = my_app.mail.sent_raw();
    assert!(
        sent_raw
            .iter()
            .any(|e| e.to.contains(&to_email) && e.cc.contains(&cc_email)),
        "acknowledgement should have been sent to the requester and cc: {sent_raw:?}"
    );
}

#[tokio::test]
async fn submit_verified_ticket_rejects_an_own_inbound_address_recipient() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _slug, owner_id) = setup_instance(&db, "ownaddr").await;
    let (my_app, schema) = build_app_and_schema(db);

    let created = create_token(
        &schema,
        &instance_id,
        "Own address rejecter",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    let secret = expect_data(&created, "createApiToken.token")
        .as_str()
        .unwrap()
        .to_string();
    let auth_info = auth::verify_token(&*my_app, &secret)
        .await
        .expect("token must verify");

    let response = schema
        .execute(
            Request::new(SUBMIT_VERIFIED_TICKET_MUTATION)
                .variables(Variables::from_json(json!({
                    "subject": "Should fail",
                    "body": "Own address as requester",
                    "to": ["support@ownaddr.example.com"],
                    "cc": Vec::<String>::new(),
                })))
                .data(auth_info),
        )
        .await;
    assert!(
        !response.errors.is_empty(),
        "expected an error rejecting the instance's own inbound address"
    );
}

#[tokio::test]
async fn submit_verified_ticket_rejects_more_than_twenty_recipients() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _slug, owner_id) = setup_instance(&db, "toomany").await;
    let (my_app, schema) = build_app_and_schema(db);

    let created = create_token(
        &schema,
        &instance_id,
        "Too many recipients",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    let secret = expect_data(&created, "createApiToken.token")
        .as_str()
        .unwrap()
        .to_string();
    let auth_info = auth::verify_token(&*my_app, &secret)
        .await
        .expect("token must verify");

    let to: Vec<String> = (0..21).map(|i| unique_email(&format!("bulk{i}"))).collect();
    let response = schema
        .execute(
            Request::new(SUBMIT_VERIFIED_TICKET_MUTATION)
                .variables(Variables::from_json(json!({
                    "subject": "Bulk",
                    "body": "Too many",
                    "to": to,
                    "cc": Vec::<String>::new(),
                })))
                .data(auth_info),
        )
        .await;
    assert!(
        !response.errors.is_empty(),
        "more than 20 combined recipients must be rejected"
    );
}

#[tokio::test]
async fn submit_verified_ticket_rejects_a_deleted_instance() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _slug, owner_id) = setup_instance(&db, "deletedinst").await;
    let (my_app, schema) = build_app_and_schema(db);

    let created = create_token(
        &schema,
        &instance_id,
        "Soon deleted instance",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    let secret = expect_data(&created, "createApiToken.token")
        .as_str()
        .unwrap()
        .to_string();
    let auth_info = auth::verify_token(&*my_app, &secret)
        .await
        .expect("token must verify");

    my_app
        .db()
        .update_instance(&instance_id, db::InstanceUpdateShape::SetDeleted(true))
        .await
        .expect("soft-delete the instance");

    let response = schema
        .execute(
            Request::new(SUBMIT_VERIFIED_TICKET_MUTATION)
                .variables(Variables::from_json(json!({
                    "subject": "Should fail",
                    "body": "Instance is deleted",
                    "to": [unique_email("requester")],
                    "cc": Vec::<String>::new(),
                })))
                .data(auth_info),
        )
        .await;
    assert_eq!(expect_error_code(&response), "NOT_FOUND");
}

#[tokio::test]
async fn an_api_token_principal_cannot_reach_anything_else() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _slug, owner_id) = setup_instance(&db, "narrowscope").await;
    let (my_app, schema) = build_app_and_schema(db);

    let created = create_token(
        &schema,
        &instance_id,
        "Narrow scope",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    let secret = expect_data(&created, "createApiToken.token")
        .as_str()
        .unwrap()
        .to_string();
    let auth_info = auth::verify_token(&*my_app, &secret)
        .await
        .expect("token must verify");

    // Cannot call submitTicket (Requester-only guard).
    let submit_ticket_response = schema
        .execute(
            Request::new(r#"mutation { submitTicket(subject: "x", body: "y") { id } }"#)
                .data(clone_auth(&auth_info)),
        )
        .await;
    assert_eq!(
        expect_error_code(&submit_ticket_response),
        "UNAUTHENTICATED"
    );

    // Cannot call createAttachmentUpload (Authenticated-only guard).
    let upload_response = schema
        .execute(
            Request::new(
                r#"mutation($ticketId: ID!) {
                    createAttachmentUpload(ticketId: $ticketId, filename: "a.txt", contentType: "text/plain") { key }
                }"#,
            )
            .variables(Variables::from_json(json!({ "ticketId": "does-not-matter" })))
            .data(clone_auth(&auth_info)),
        )
        .await;
    assert_eq!(expect_error_code(&upload_response), "UNAUTHENTICATED");

    // Cannot query `me` (Authenticated-only guard).
    let me_response = schema
        .execute(Request::new("{ me { id } }").data(clone_auth(&auth_info)))
        .await;
    assert_eq!(expect_error_code(&me_response), "UNAUTHENTICATED");
}

/// `AuthInfo` isn't `Clone` — this test needs the same principal on three
/// separate `Request`s, so it's reconstructed field-by-field instead.
fn clone_auth(auth: &AuthInfo) -> AuthInfo {
    match auth {
        AuthInfo::ApiToken {
            token_id,
            instance_id,
        } => AuthInfo::ApiToken {
            token_id: token_id.clone(),
            instance_id: instance_id.clone(),
        },
        AuthInfo::User {
            id,
            memberships,
            is_superuser,
            token_id,
            grant_id,
        } => AuthInfo::User {
            id: id.clone(),
            memberships: memberships.clone(),
            is_superuser: *is_superuser,
            token_id: token_id.clone(),
            grant_id: grant_id.clone(),
        },
        AuthInfo::Requester { email, instance_id } => AuthInfo::Requester {
            email: email.clone(),
            instance_id: instance_id.clone(),
        },
    }
}
