//! Integration tests for PR 1 of the finance feature — instance `kind`,
//! kind isolation, finance settings, and the `project` table/CRUD —
//! against **DynamoDB Local**, following `api_token_dynamodb_local.rs`'s
//! helper pattern. Covers:
//!
//!   - a finance instance's owner and agent can create/update/list
//!     projects, sorted by name case-insensitively;
//!   - a non-member gets `NOT_FOUND` (mutations) or `null`/empty (queries);
//!   - a superuser with no membership cannot list/create projects — the
//!     existing superuser boundary (CLAUDE.md) extends unchanged;
//!   - project operations on a support instance, and support-only
//!     operations (`addInboundAddress`/`createApiToken`) on a finance
//!     instance, are both rejected;
//!   - `updateFinanceSettings`: owner OK, agent rejected, superuser OK,
//!     a blank field REMOVEs the attribute (verified via a direct db read),
//!     and a support instance is rejected;
//!   - `publicInstances` excludes finance instances.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test finance_dynamodb_local
//! ```
//!
//! Like the other `*_dynamodb_local.rs` files, every test here **skips
//! itself** when no reachable local DynamoDB is configured, so `cargo test`
//! and CI stay green with no local stack running. Every row is created fresh
//! per test run (nanoid'd ids/slugs/emails), so repeated runs never collide.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use toolbox::app;
use toolbox::app::HasDb as _;
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

/// See `tests/auth_dynamodb_local.rs`'s identically-named helper for the full
/// rationale.
async fn local_db_prefix() -> Option<String> {
    let endpoint = toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;

    let client = toolbox::local_dev::dynamodb_client().await;
    if client.list_tables().send().await.is_err() {
        eprintln!(
            "finance_dynamodb_local: {endpoint} is configured but not reachable — skipping. \
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
                    "finance_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set to a \
                     reachable local DynamoDB — skipping. See this file's header for how to run it."
                );
                return;
            }
        }
    };
}

fn unique_id(label: &str) -> String {
    let suffix: String = nanoid::nanoid!(16)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    format!("{label}-{suffix}").to_lowercase()
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

/// Mirrors `api_token_dynamodb_local.rs`'s identically-named helper.
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

fn superuser_auth(user_id: &str) -> AuthInfo {
    AuthInfo::User {
        id: user_id.to_string(),
        memberships: vec![],
        is_superuser: true,
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

/// Create a finance instance + one owner + one agent, returning
/// `(instance_id, owner_user_id, agent_user_id)`.
async fn setup_finance_instance(db: &dynamodb::Handler, label: &str) -> (String, String, String) {
    let instance = db
        .create_instance(
            &unique_id(label),
            &unique_id(&format!("{label}-slug")),
            label,
            "",
            false,
            db::InstanceKind::Finance,
        )
        .await
        .expect("create_instance");
    let owner = db
        .create_user(&unique_email(&format!("{label}-owner")), "Owner")
        .await
        .expect("create_user");
    db.create_membership(&owner.id, &instance.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership");
    let agent = db
        .create_user(&unique_email(&format!("{label}-agent")), "Agent")
        .await
        .expect("create_user");
    db.create_membership(&agent.id, &instance.id, db::MembershipRole::Agent)
        .await
        .expect("create_membership");
    (instance.id, owner.id, agent.id)
}

/// Create a plain support instance + one owner, returning
/// `(instance_id, owner_user_id)`.
async fn setup_support_instance(db: &dynamodb::Handler, label: &str) -> (String, String) {
    let instance = db
        .create_instance(
            &unique_id(label),
            &unique_id(&format!("{label}-slug")),
            label,
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    let owner = db
        .create_user(&unique_email(&format!("{label}-owner")), "Owner")
        .await
        .expect("create_user");
    db.create_membership(&owner.id, &instance.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership");
    (instance.id, owner.id)
}

const CREATE_PROJECT_MUTATION: &str = r#"
    mutation($instanceId: ID!, $input: CreateProjectInput!) {
        createProject(instanceId: $instanceId, input: $input) {
            id name clientName clientAbn clientAddress reference archived
        }
    }
"#;

const UPDATE_PROJECT_MUTATION: &str = r#"
    mutation($id: ID!, $input: UpdateProjectInput!) {
        updateProject(id: $id, input: $input) {
            id name clientName clientAbn clientAddress reference archived
        }
    }
"#;

const PROJECTS_QUERY: &str = r#"
    query($instanceId: ID!, $includeArchived: Boolean!) {
        projects(instanceId: $instanceId, includeArchived: $includeArchived) {
            id name archived
        }
    }
"#;

const PROJECT_QUERY: &str = r#"
    query($id: ID!) {
        project(id: $id) { id name }
    }
"#;

const UPDATE_FINANCE_SETTINGS_MUTATION: &str = r#"
    mutation($instanceId: ID!, $input: FinanceSettingsInput!) {
        updateFinanceSettings(instanceId: $instanceId, input: $input) {
            financeSettings {
                businessName businessAbn businessAddress businessPhone
                businessEmail paymentDetails gstRegistered currency
            }
        }
    }
"#;

const ADD_INBOUND_ADDRESS_MUTATION: &str = r#"
    mutation($instanceId: ID!, $address: String!) {
        addInboundAddress(instanceId: $instanceId, address: $address) { address }
    }
"#;

const CREATE_API_TOKEN_MUTATION: &str = r#"
    mutation($instanceId: ID!, $name: String!) {
        createApiToken(instanceId: $instanceId, name: $name) { token apiToken { id } }
    }
"#;

const PUBLIC_INSTANCES_QUERY: &str = "{ publicInstances { slug } }";

fn default_project_input(name: &str) -> Value {
    json!({
        "name": name,
        "clientName": "Fictional Client Pty Ltd",
        "clientAbn": "12 345 678 901",
        "clientAddress": "1 Example St\nSomewhere",
        "reference": "Site A",
    })
}

async fn create_project(
    schema: &TestSchema,
    instance_id: &str,
    name: &str,
    auth: AuthInfo,
) -> Response {
    schema
        .execute(
            Request::new(CREATE_PROJECT_MUTATION)
                .variables(Variables::from_json(json!({
                    "instanceId": instance_id,
                    "input": default_project_input(name),
                })))
                .data(auth),
        )
        .await
}

#[tokio::test]
async fn owner_and_agent_can_create_update_and_list_projects() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id, agent_id) = setup_finance_instance(&db, "ledger").await;
    let (_my_app, schema) = build_app_and_schema(db);

    let created = create_project(
        &schema,
        &instance_id,
        "Zeta Job",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    let created = expect_data(&created, "createProject");
    assert_eq!(created["name"], "Zeta Job");
    assert_eq!(created["clientName"], "Fictional Client Pty Ltd");
    assert_eq!(created["archived"], json!(false));
    let project_id = created["id"].as_str().unwrap().to_string();

    // The agent can also create one, and can update the owner's project.
    let created_by_agent = create_project(
        &schema,
        &instance_id,
        "Alpha Job",
        agent_auth(&agent_id, &instance_id),
    )
    .await;
    expect_data(&created_by_agent, "createProject");

    let update_response = schema
        .execute(
            Request::new(UPDATE_PROJECT_MUTATION)
                .variables(Variables::from_json(json!({
                    "id": project_id,
                    "input": {
                        "name": "Zeta Job (renamed)",
                        "clientName": "Fictional Client Pty Ltd",
                        "clientAbn": null,
                        "clientAddress": "1 Example St\nSomewhere",
                        "reference": null,
                        "archived": false,
                    }
                })))
                .data(agent_auth(&agent_id, &instance_id)),
        )
        .await;
    let updated = expect_data(&update_response, "updateProject");
    assert_eq!(updated["name"], "Zeta Job (renamed)");
    assert_eq!(updated["clientAbn"], Value::Null);
    assert_eq!(updated["reference"], Value::Null);

    // Sorted by name case-insensitively: "Alpha Job" before "Zeta Job (renamed)".
    let list_response = schema
        .execute(
            Request::new(PROJECTS_QUERY)
                .variables(Variables::from_json(
                    json!({ "instanceId": instance_id, "includeArchived": false }),
                ))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    let projects = expect_data(&list_response, "projects");
    let names: Vec<&str> = projects
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Alpha Job", "Zeta Job (renamed)"]);
}

#[tokio::test]
async fn archiving_hides_a_project_unless_include_archived_is_set() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id, _agent_id) = setup_finance_instance(&db, "archive").await;
    let (_my_app, schema) = build_app_and_schema(db);

    let created = create_project(
        &schema,
        &instance_id,
        "To Archive",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    let created = expect_data(&created, "createProject");
    let project_id = created["id"].as_str().unwrap().to_string();

    schema
        .execute(
            Request::new(UPDATE_PROJECT_MUTATION)
                .variables(Variables::from_json(json!({
                    "id": project_id,
                    "input": {
                        "name": "To Archive",
                        "clientName": "Fictional Client Pty Ltd",
                        "clientAbn": null,
                        "clientAddress": null,
                        "reference": null,
                        "archived": true,
                    }
                })))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;

    let hidden = schema
        .execute(
            Request::new(PROJECTS_QUERY)
                .variables(Variables::from_json(
                    json!({ "instanceId": instance_id, "includeArchived": false }),
                ))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    assert_eq!(
        expect_data(&hidden, "projects").as_array().unwrap().len(),
        0
    );

    let shown = schema
        .execute(
            Request::new(PROJECTS_QUERY)
                .variables(Variables::from_json(
                    json!({ "instanceId": instance_id, "includeArchived": true }),
                ))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    assert_eq!(expect_data(&shown, "projects").as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_non_member_cannot_reach_projects() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id, _agent_id) = setup_finance_instance(&db, "outsider").await;
    let outsider_id = unique_id("outsider-user");
    let (_my_app, schema) = build_app_and_schema(db);

    let created = create_project(
        &schema,
        &instance_id,
        "Owner's Job",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    let project_id = expect_data(&created, "createProject.id")
        .as_str()
        .unwrap()
        .to_string();

    // A user with no membership at all: `Member` guard rejects `createProject`.
    let outsider_create = create_project(
        &schema,
        &instance_id,
        "Should fail",
        AuthInfo::User {
            id: outsider_id.clone(),
            memberships: vec![],
            is_superuser: false,
            token_id: None,
            grant_id: None,
        },
    )
    .await;
    assert_eq!(expect_error_code(&outsider_create), "UNAUTHENTICATED");

    // `updateProject` is id-only: a non-member gets NOT_FOUND, not FORBIDDEN.
    let outsider_update = schema
        .execute(
            Request::new(UPDATE_PROJECT_MUTATION)
                .variables(Variables::from_json(json!({
                    "id": project_id,
                    "input": {
                        "name": "Stolen",
                        "clientName": "Fictional Client Pty Ltd",
                        "clientAbn": null,
                        "clientAddress": null,
                        "reference": null,
                        "archived": false,
                    }
                })))
                .data(AuthInfo::User {
                    id: outsider_id.clone(),
                    memberships: vec![],
                    is_superuser: false,
                    token_id: None,
                    grant_id: None,
                }),
        )
        .await;
    assert_eq!(expect_error_code(&outsider_update), "NOT_FOUND");

    // `project(id)` returns null, not an error, for a non-member.
    let outsider_read = schema
        .execute(
            Request::new(PROJECT_QUERY)
                .variables(Variables::from_json(json!({ "id": project_id })))
                .data(AuthInfo::User {
                    id: outsider_id,
                    memberships: vec![],
                    is_superuser: false,
                    token_id: None,
                    grant_id: None,
                }),
        )
        .await;
    assert_eq!(expect_data(&outsider_read, "project"), Value::Null);
}

#[tokio::test]
async fn a_superuser_with_no_membership_cannot_reach_projects() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id, _agent_id) = setup_finance_instance(&db, "suboundary").await;
    let superuser_id = unique_id("super");
    let (_my_app, schema) = build_app_and_schema(db);

    let created = create_project(
        &schema,
        &instance_id,
        "Owner's Job",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    let project_id = expect_data(&created, "createProject.id")
        .as_str()
        .unwrap()
        .to_string();

    // Superusers don't pass `Member` — the existing boundary (CLAUDE.md)
    // extends unchanged: no implicit access to any instance's projects.
    let list_response = schema
        .execute(
            Request::new(PROJECTS_QUERY)
                .variables(Variables::from_json(
                    json!({ "instanceId": instance_id, "includeArchived": false }),
                ))
                .data(superuser_auth(&superuser_id)),
        )
        .await;
    assert_eq!(expect_error_code(&list_response), "UNAUTHENTICATED");

    let create_response = create_project(
        &schema,
        &instance_id,
        "Should fail",
        superuser_auth(&superuser_id),
    )
    .await;
    assert_eq!(expect_error_code(&create_response), "UNAUTHENTICATED");

    let read_response = schema
        .execute(
            Request::new(PROJECT_QUERY)
                .variables(Variables::from_json(json!({ "id": project_id })))
                .data(superuser_auth(&superuser_id)),
        )
        .await;
    assert_eq!(expect_data(&read_response, "project"), Value::Null);
}

#[tokio::test]
async fn project_operations_on_a_support_instance_are_rejected() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id) = setup_support_instance(&db, "supportonly").await;
    let (_my_app, schema) = build_app_and_schema(db);

    let create_response = create_project(
        &schema,
        &instance_id,
        "Should fail",
        owner_auth(&owner_id, &instance_id),
    )
    .await;
    assert!(
        !create_response.errors.is_empty(),
        "createProject on a support instance must fail"
    );

    let list_response = schema
        .execute(
            Request::new(PROJECTS_QUERY)
                .variables(Variables::from_json(
                    json!({ "instanceId": instance_id, "includeArchived": false }),
                ))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    assert!(
        !list_response.errors.is_empty(),
        "projects on a support instance must fail"
    );
}

#[tokio::test]
async fn update_finance_settings_owner_ok_agent_and_support_rejected_blank_removes() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id, agent_id) = setup_finance_instance(&db, "settings").await;
    let (my_app, schema) = build_app_and_schema(db);

    let full_input = json!({
        "businessName": "Fictional Trading Co",
        "businessAbn": "98 765 432 100",
        "businessAddress": "42 Wallaby Way\nSydney",
        "businessPhone": "0400 000 000",
        "businessEmail": "billing@fictional-trading.example.com",
        "paymentDetails": "BSB 000-000 Acc 00000000",
        "gstRegistered": true,
        "currency": "AUD",
    });

    // Agent cannot update finance settings — owner-or-superuser only.
    let agent_response = schema
        .execute(
            Request::new(UPDATE_FINANCE_SETTINGS_MUTATION)
                .variables(Variables::from_json(json!({
                    "instanceId": instance_id,
                    "input": full_input,
                })))
                .data(agent_auth(&agent_id, &instance_id)),
        )
        .await;
    assert_eq!(expect_error_code(&agent_response), "UNAUTHENTICATED");

    // Owner can.
    let owner_response = schema
        .execute(
            Request::new(UPDATE_FINANCE_SETTINGS_MUTATION)
                .variables(Variables::from_json(json!({
                    "instanceId": instance_id,
                    "input": full_input,
                })))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    let settings = expect_data(&owner_response, "updateFinanceSettings.financeSettings");
    assert_eq!(settings["businessName"], "Fictional Trading Co");
    assert_eq!(settings["gstRegistered"], json!(true));
    assert_eq!(settings["currency"], "AUD");

    // Verify via a direct db read too, not just the mutation's own response.
    let stored = my_app
        .db()
        .get_instances(&[instance_id.as_str()])
        .await
        .expect("get_instances")
        .into_iter()
        .next()
        .flatten()
        .expect("instance exists");
    assert_eq!(
        stored.business_name.as_deref(),
        Some("Fictional Trading Co")
    );
    assert!(stored.gst_registered);
    assert_eq!(stored.currency.as_deref(), Some("AUD"));

    // A superuser can too.
    let superuser_id = unique_id("super");
    let mut blank_input = full_input.clone();
    blank_input["businessPhone"] = json!("");
    blank_input["currency"] = json!("");
    blank_input["gstRegistered"] = json!(false);
    let superuser_response = schema
        .execute(
            Request::new(UPDATE_FINANCE_SETTINGS_MUTATION)
                .variables(Variables::from_json(json!({
                    "instanceId": instance_id,
                    "input": blank_input,
                })))
                .data(superuser_auth(&superuser_id)),
        )
        .await;
    let settings = expect_data(&superuser_response, "updateFinanceSettings.financeSettings");
    // Blank businessPhone/currency -> REMOVEd; currency defaults back to AUD.
    assert_eq!(settings["businessPhone"], Value::Null);
    assert_eq!(settings["currency"], "AUD");
    assert_eq!(settings["gstRegistered"], json!(false));
    // businessName was resent non-blank, so it's still set.
    assert_eq!(settings["businessName"], "Fictional Trading Co");

    let stored_after_blank = my_app
        .db()
        .get_instances(&[instance_id.as_str()])
        .await
        .expect("get_instances")
        .into_iter()
        .next()
        .flatten()
        .expect("instance exists");
    assert_eq!(stored_after_blank.business_phone, None);
    assert_eq!(stored_after_blank.currency, None);
    assert!(!stored_after_blank.gst_registered);

    // Rejected on a support instance.
    let (support_instance_id, support_owner_id) =
        setup_support_instance(&my_app.db, "notfinance").await;
    let support_response = schema
        .execute(
            Request::new(UPDATE_FINANCE_SETTINGS_MUTATION)
                .variables(Variables::from_json(json!({
                    "instanceId": support_instance_id,
                    "input": full_input,
                })))
                .data(owner_auth(&support_owner_id, &support_instance_id)),
        )
        .await;
    assert!(
        !support_response.errors.is_empty(),
        "updateFinanceSettings on a support instance must fail"
    );
}

#[tokio::test]
async fn add_inbound_address_and_create_api_token_reject_an_finance_instance() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id, _agent_id) = setup_finance_instance(&db, "notsupport").await;
    let (_my_app, schema) = build_app_and_schema(db);

    let address_response = schema
        .execute(
            Request::new(ADD_INBOUND_ADDRESS_MUTATION)
                .variables(Variables::from_json(json!({
                    "instanceId": instance_id,
                    "address": "support@notsupport.example.com",
                })))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    assert_eq!(expect_error_code(&address_response), "NOT_FOUND");

    let token_response = schema
        .execute(
            Request::new(CREATE_API_TOKEN_MUTATION)
                .variables(Variables::from_json(json!({
                    "instanceId": instance_id,
                    "name": "Should fail",
                })))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    assert_eq!(expect_error_code(&token_response), "NOT_FOUND");
}

#[tokio::test]
async fn public_instances_excludes_finance_instances() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;

    // A support instance with public submission enabled...
    let support = db
        .create_instance(
            "Public Support Co",
            &unique_id("public-support"),
            "Public Support Co",
            "",
            true,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");

    // ...and a finance instance. `create_instance` itself doesn't reject
    // `public_submission_enabled: true` on a finance instance (only the
    // GraphQL mutation/CLI do), so this also proves `publicInstances` itself
    // enforces kind isolation, not just its callers.
    let finance = db
        .create_instance(
            "Ledger Co",
            &unique_id("ledger-co"),
            "Ledger Co",
            "",
            true,
            db::InstanceKind::Finance,
        )
        .await
        .expect("create_instance");

    let (_my_app, schema) = build_app_and_schema(db);
    let response = schema.execute(Request::new(PUBLIC_INSTANCES_QUERY)).await;
    let slugs: Vec<String> = expect_data(&response, "publicInstances")
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["slug"].as_str().unwrap().to_string())
        .collect();
    assert!(slugs.contains(&support.slug));
    assert!(!slugs.contains(&finance.slug));
}
