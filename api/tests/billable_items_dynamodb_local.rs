//! Integration tests for PR 2 of the invoicing feature — the `billable_item`
//! table and its GraphQL surface — against **DynamoDB Local**, following
//! `invoicing_dynamodb_local.rs`'s helper pattern. Covers:
//!
//!   - an agent can create/update/list/delete items; amounts and quantity
//!     formatting come back as the API computes them;
//!   - a non-member gets `NOT_FOUND` (id-only mutations) or is rejected by the
//!     `Member` guard (the listing), and a `projectId` from another instance is
//!     `NOT_FOUND`;
//!   - a superuser with no membership gets nothing;
//!   - an archived project takes no new items;
//!   - keyset pagination, newest date first, across page boundaries with the
//!     UNBILLED/BILLED filter applied — no duplicates, nothing missing, even
//!     when matching rows are sparse (DynamoDB's `Limit` applies before the
//!     filter);
//!   - an item with an `invoice_id` refuses update/delete with `CONFLICT`;
//!   - project-scoped listing; a support instance is rejected;
//!   - input validation.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test billable_items_dynamodb_local
//! ```
//!
//! Skips itself when no reachable local DynamoDB is configured, like every
//! other `*_dynamodb_local.rs` file.

use std::collections::HashSet;
use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use aws_sdk_dynamodb::types::AttributeValue;
use serde_json::{Value, json};
use toolbox::app;
use toolbox::auth::{AuthInfo, Membership};
use toolbox::db;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::graphql;
use toolbox::mockmail;
use toolbox::mockstorage;

type TestApp = app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage>;

/// The schema plus the per-request dataloader the server attaches
/// (`server.rs`) — `BillableItem.project` resolves through it.
struct TestSchema {
    app: Arc<TestApp>,
    schema: graphql::ToolboxSchema<TestApp>,
}

impl TestSchema {
    async fn execute(&self, request: Request) -> Response {
        self.schema
            .execute(request.data(graphql::get_dataloader(self.app.clone())))
            .await
    }
}

/// See `tests/auth_dynamodb_local.rs`'s identically-named helper.
async fn local_db_prefix() -> Option<String> {
    let endpoint = toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;
    let client = toolbox::local_dev::dynamodb_client().await;
    if client.list_tables().send().await.is_err() {
        eprintln!(
            "billable_items_dynamodb_local: {endpoint} is configured but not reachable — \
             skipping. Run `make local-up && make local-tables` first."
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
                    "billable_items_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set \
                     to a reachable local DynamoDB — skipping. See this file's header."
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

fn expect_error_message(response: &Response) -> String {
    response
        .errors
        .first()
        .unwrap_or_else(|| panic!("expected a GraphQL error, got none: {response:?}"))
        .message
        .clone()
}

fn member_auth(user_id: &str, instance_id: &str) -> AuthInfo {
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

fn outsider_auth(user_id: &str) -> AuthInfo {
    AuthInfo::User {
        id: user_id.to_string(),
        memberships: vec![],
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

fn build_schema(db: dynamodb::Handler) -> TestSchema {
    let my_app = Arc::new(app::new(
        db,
        mockmail::Handler::new(),
        mockstorage::Storage::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(my_app.clone(), webauthn);
    TestSchema {
        app: my_app,
        schema,
    }
}

/// An invoicing instance with one agent and one project, returning
/// `(instance_id, agent_user_id, project_id)`.
async fn setup(db: &dynamodb::Handler, label: &str) -> (String, String, String) {
    let instance = db
        .create_instance(
            &unique_id(label),
            &unique_id(&format!("{label}-slug")),
            label,
            "",
            false,
            db::InstanceKind::Invoicing,
        )
        .await
        .expect("create_instance");
    let agent = db
        .create_user(&unique_email(&format!("{label}-agent")), "Agent")
        .await
        .expect("create_user");
    db.create_membership(&agent.id, &instance.id, db::MembershipRole::Agent)
        .await
        .expect("create_membership");
    let project = db
        .create_project(
            &instance.id,
            &db::ProjectFields {
                name: "Fictional Job".into(),
                client_name: "Fictional Client Pty Ltd".into(),
                ..Default::default()
            },
        )
        .await
        .expect("create_project");
    (instance.id, agent.id, project.id)
}

/// Put an item on a (fictional) invoice by writing `invoice_id` straight to
/// the row — nothing in this PR's API can, since invoices arrive next.
async fn mark_billed(prefix: &str, item_id: &str) {
    let client = toolbox::local_dev::dynamodb_client().await;
    client
        .update_item()
        .table_name(format!("{prefix}_billable_item"))
        .key("id", AttributeValue::S(item_id.to_string()))
        .update_expression("SET invoice_id = :inv")
        .expression_attribute_values(":inv", AttributeValue::S("FakeInvoice1".into()))
        .send()
        .await
        .expect("mark billed");
}

const ITEM_FIELDS: &str = "id date description quantity unitPriceCents amountCents status \
                           createdAt updatedAt project { id name }";

fn create_mutation() -> String {
    format!(
        "mutation($projectId: ID!, $input: BillableItemInput!) {{
            createBillableItem(projectId: $projectId, input: $input) {{ {ITEM_FIELDS} }}
        }}"
    )
}

fn update_mutation() -> String {
    format!(
        "mutation($id: ID!, $input: BillableItemInput!) {{
            updateBillableItem(id: $id, input: $input) {{ {ITEM_FIELDS} }}
        }}"
    )
}

const DELETE_MUTATION: &str = "mutation($id: ID!) { deleteBillableItem(id: $id) }";

const LIST_QUERY: &str = r#"
    query($instanceId: ID!, $projectId: ID, $filter: BillableItemFilterType!, $first: Int, $after: String) {
        billableItems(instanceId: $instanceId, projectId: $projectId, filter: $filter, first: $first, after: $after) {
            edges { cursor node { id date description status project { id } } }
            pageInfo { hasNextPage endCursor }
        }
    }
"#;

fn item_input(date: &str, description: &str, quantity: &str, unit_price_cents: i64) -> Value {
    json!({
        "date": date,
        "description": description,
        "quantity": quantity,
        "unitPriceCents": unit_price_cents,
    })
}

async fn create_item(
    schema: &TestSchema,
    project_id: &str,
    input: Value,
    auth: AuthInfo,
) -> Response {
    schema
        .execute(
            Request::new(create_mutation())
                .variables(Variables::from_json(
                    json!({ "projectId": project_id, "input": input }),
                ))
                .data(auth),
        )
        .await
}

async fn list(schema: &TestSchema, vars: Value, auth: AuthInfo) -> Response {
    schema
        .execute(
            Request::new(LIST_QUERY)
                .variables(Variables::from_json(vars))
                .data(auth),
        )
        .await
}

/// Walk every page of a listing, returning `(id, date)` in order.
async fn collect_all(
    schema: &TestSchema,
    instance_id: &str,
    project_id: Option<&str>,
    filter: &str,
    first: i32,
    auth: &dyn Fn() -> AuthInfo,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..100 {
        let response = list(
            schema,
            json!({
                "instanceId": instance_id,
                "projectId": project_id,
                "filter": filter,
                "first": first,
                "after": after,
            }),
            auth(),
        )
        .await;
        let conn = expect_data(&response, "billableItems");
        let edges = conn["edges"].as_array().unwrap();
        assert!(edges.len() <= first as usize, "page larger than `first`");
        for e in edges {
            out.push((
                e["node"]["id"].as_str().unwrap().to_string(),
                e["node"]["date"].as_str().unwrap().to_string(),
            ));
        }
        if conn["pageInfo"]["hasNextPage"] != json!(true) {
            return out;
        }
        assert_eq!(
            edges.len(),
            first as usize,
            "a page with a next page must be full — the filter loop stopped early"
        );
        after = Some(conn["pageInfo"]["endCursor"].as_str().unwrap().to_string());
    }
    panic!("pagination did not terminate");
}

#[tokio::test]
async fn an_agent_can_create_update_list_and_delete_items() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, project_id) = setup(&db, "crud").await;
    let schema = build_schema(db);
    let auth = || member_auth(&agent_id, &instance_id);

    let created = create_item(
        &schema,
        &project_id,
        item_input(
            "2026-08-19",
            "  Site visit\n* measure up\n* photos  ",
            "1.5",
            333,
        ),
        auth(),
    )
    .await;
    let created = expect_data(&created, "createBillableItem");
    assert_eq!(created["date"], "2026-08-19");
    assert_eq!(created["description"], "Site visit\n* measure up\n* photos");
    assert_eq!(created["quantity"], "1.5");
    assert_eq!(created["unitPriceCents"], 333);
    // 1.5 × 3.33 = 4.995 → 5.00, rounded half-up.
    assert_eq!(created["amountCents"], 500);
    assert_eq!(created["status"], "UNBILLED");
    assert_eq!(created["project"]["id"], json!(project_id));
    let item_id = created["id"].as_str().unwrap().to_string();

    let updated = schema
        .execute(
            Request::new(update_mutation())
                .variables(Variables::from_json(json!({
                    "id": item_id,
                    "input": item_input("2026-08-20", "Labour", "2.00", 40_000),
                })))
                .data(auth()),
        )
        .await;
    let updated = expect_data(&updated, "updateBillableItem");
    assert_eq!(updated["date"], "2026-08-20");
    assert_eq!(updated["description"], "Labour");
    assert_eq!(updated["quantity"], "2");
    assert_eq!(updated["amountCents"], 80_000);

    let listed = list(
        &schema,
        json!({ "instanceId": instance_id, "filter": "ALL" }),
        auth(),
    )
    .await;
    let edges = expect_data(&listed, "billableItems.edges");
    let edges = edges.as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["node"]["id"], json!(item_id));
    assert_eq!(edges[0]["node"]["date"], "2026-08-20");
    assert_eq!(edges[0]["cursor"], json!(format!("2026-08-20:{item_id}")));

    let deleted = schema
        .execute(
            Request::new(DELETE_MUTATION)
                .variables(Variables::from_json(json!({ "id": item_id })))
                .data(auth()),
        )
        .await;
    assert_eq!(expect_data(&deleted, "deleteBillableItem"), json!(item_id));

    let listed = list(
        &schema,
        json!({ "instanceId": instance_id, "filter": "ALL" }),
        auth(),
    )
    .await;
    assert_eq!(
        expect_data(&listed, "billableItems.edges")
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // Deleting again: gone, so NOT_FOUND.
    let again = schema
        .execute(
            Request::new(DELETE_MUTATION)
                .variables(Variables::from_json(json!({ "id": item_id })))
                .data(auth()),
        )
        .await;
    assert_eq!(expect_error_code(&again), "NOT_FOUND");
}

#[tokio::test]
async fn a_non_member_and_a_superuser_get_nothing() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, project_id) = setup(&db, "outsider").await;
    let schema = build_schema(db);

    let created = create_item(
        &schema,
        &project_id,
        item_input("2026-08-19", "Work", "1", 100),
        member_auth(&agent_id, &instance_id),
    )
    .await;
    let item_id = expect_data(&created, "createBillableItem.id")
        .as_str()
        .unwrap()
        .to_string();

    let outsider_id = unique_id("outsider");
    let superuser_id = unique_id("super");
    let callers: [(&str, &dyn Fn() -> AuthInfo); 2] = [
        ("outsider", &|| outsider_auth(&outsider_id)),
        ("superuser", &|| superuser_auth(&superuser_id)),
    ];
    for (label, auth) in callers {
        let create = create_item(
            &schema,
            &project_id,
            item_input("2026-08-19", "Nope", "1", 100),
            auth(),
        )
        .await;
        assert_eq!(expect_error_code(&create), "NOT_FOUND", "{label} create");

        let update = schema
            .execute(
                Request::new(update_mutation())
                    .variables(Variables::from_json(json!({
                        "id": item_id,
                        "input": item_input("2026-08-19", "Stolen", "1", 100),
                    })))
                    .data(auth()),
            )
            .await;
        assert_eq!(expect_error_code(&update), "NOT_FOUND", "{label} update");

        let delete = schema
            .execute(
                Request::new(DELETE_MUTATION)
                    .variables(Variables::from_json(json!({ "id": item_id })))
                    .data(auth()),
            )
            .await;
        assert_eq!(expect_error_code(&delete), "NOT_FOUND", "{label} delete");

        let listed = list(
            &schema,
            json!({ "instanceId": instance_id, "filter": "ALL" }),
            auth(),
        )
        .await;
        assert_eq!(
            expect_error_code(&listed),
            "UNAUTHENTICATED",
            "{label} list"
        );
    }
}

#[tokio::test]
async fn a_project_from_another_instance_is_not_found() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_a, agent_a, _project_a) = setup(&db, "scope-a").await;
    let (_instance_b, _agent_b, project_b) = setup(&db, "scope-b").await;
    let schema = build_schema(db);

    let listed = list(
        &schema,
        json!({ "instanceId": instance_a, "projectId": project_b, "filter": "ALL" }),
        member_auth(&agent_a, &instance_a),
    )
    .await;
    assert_eq!(expect_error_code(&listed), "NOT_FOUND");
}

#[tokio::test]
async fn an_archived_project_takes_no_new_items() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, project_id) = setup(&db, "archived").await;
    db.update_project(
        &project_id,
        db::ProjectUpdateShape::Fields {
            fields: &db::ProjectFields {
                name: "Fictional Job".into(),
                client_name: "Fictional Client Pty Ltd".into(),
                ..Default::default()
            },
            archived: true,
        },
    )
    .await
    .expect("archive");
    let schema = build_schema(db);

    let created = create_item(
        &schema,
        &project_id,
        item_input("2026-08-19", "Work", "1", 100),
        member_auth(&agent_id, &instance_id),
    )
    .await;
    assert!(
        expect_error_message(&created).contains("archived"),
        "{created:?}"
    );
}

#[tokio::test]
async fn a_billed_item_refuses_update_and_delete() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, project_id) = setup(&db, "billed").await;
    let schema = build_schema(db);
    let auth = || member_auth(&agent_id, &instance_id);

    let created = create_item(
        &schema,
        &project_id,
        item_input("2026-08-19", "Work", "1", 100),
        auth(),
    )
    .await;
    let item_id = expect_data(&created, "createBillableItem.id")
        .as_str()
        .unwrap()
        .to_string();
    mark_billed(&prefix, &item_id).await;

    let update = schema
        .execute(
            Request::new(update_mutation())
                .variables(Variables::from_json(json!({
                    "id": item_id,
                    "input": item_input("2026-08-19", "Edited", "1", 100),
                })))
                .data(auth()),
        )
        .await;
    assert_eq!(expect_error_code(&update), "CONFLICT");

    let delete = schema
        .execute(
            Request::new(DELETE_MUTATION)
                .variables(Variables::from_json(json!({ "id": item_id })))
                .data(auth()),
        )
        .await;
    assert_eq!(expect_error_code(&delete), "CONFLICT");

    let listed = list(
        &schema,
        json!({ "instanceId": instance_id, "filter": "BILLED" }),
        auth(),
    )
    .await;
    let edges = expect_data(&listed, "billableItems.edges");
    assert_eq!(edges[0]["node"]["status"], "INVOICED");
    assert_eq!(edges[0]["node"]["description"], "Work");
}

/// The case the filter loop exists for: 17 items across 7 dates (with
/// same-date ties), only some billed, read back in pages of 2 and 3 under
/// every filter. Each walk must yield exactly the matching set, in
/// non-increasing date order, with no duplicates — and every page that
/// claims a next page must be full.
#[tokio::test]
async fn pagination_across_pages_with_the_filter_applied() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, project_id) = setup(&db, "paging").await;
    let other_project = db
        .create_project(
            &instance_id,
            &db::ProjectFields {
                name: "Other Job".into(),
                client_name: "Other Client".into(),
                ..Default::default()
            },
        )
        .await
        .expect("create_project");
    let schema = build_schema(db);
    let auth = || member_auth(&agent_id, &instance_id);

    let dates = [
        "2026-01-05",
        "2026-03-01",
        "2026-03-01",
        "2026-03-01",
        "2025-12-31",
        "2026-02-14",
        "2026-02-14",
        "2026-07-04",
        "2026-01-05",
        "2026-03-01",
        "2026-07-04",
        "2025-12-31",
        "2026-02-14",
        "2026-01-05",
        "2026-06-30",
        "2026-06-30",
        "2026-03-01",
    ];
    let mut all = Vec::new();
    let mut billed = HashSet::new();
    let mut in_project = HashSet::new();
    for (i, date) in dates.iter().enumerate() {
        let project = if i % 4 == 3 {
            &other_project.id
        } else {
            &project_id
        };
        let created = create_item(
            &schema,
            project,
            item_input(date, &format!("Item {i}"), "1", 100),
            auth(),
        )
        .await;
        let id = expect_data(&created, "createBillableItem.id")
            .as_str()
            .unwrap()
            .to_string();
        // Billed rows are sparse and bunched, so an unfiltered `Limit`-sized
        // read often has zero or one match.
        if i % 5 == 0 || i == 7 {
            mark_billed(&prefix, &id).await;
            billed.insert(id.clone());
        }
        if project == &project_id {
            in_project.insert(id.clone());
        }
        all.push(id);
    }
    let all_set: HashSet<String> = all.iter().cloned().collect();

    let cases: [(&str, Option<&str>, HashSet<String>); 5] = [
        ("ALL", None, all_set.clone()),
        ("BILLED", None, billed.clone()),
        (
            "UNBILLED",
            None,
            all_set.difference(&billed).cloned().collect(),
        ),
        ("ALL", Some(project_id.as_str()), in_project.clone()),
        (
            "UNBILLED",
            Some(project_id.as_str()),
            in_project.difference(&billed).cloned().collect(),
        ),
    ];
    for (filter, project, expected) in cases {
        for first in [2, 3] {
            let rows = collect_all(&schema, &instance_id, project, filter, first, &auth).await;
            let ids: Vec<&String> = rows.iter().map(|(id, _)| id).collect();
            let unique: HashSet<String> = ids.iter().map(|s| (*s).clone()).collect();
            assert_eq!(
                unique.len(),
                ids.len(),
                "{filter}/{project:?}/first={first}: duplicates in {ids:?}"
            );
            assert_eq!(
                unique, expected,
                "{filter}/{project:?}/first={first}: wrong set"
            );
            assert!(
                rows.windows(2).all(|w| w[0].1 >= w[1].1),
                "{filter}/{project:?}/first={first}: not newest-date-first: {rows:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_support_instance_is_rejected() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let instance = db
        .create_instance(
            &unique_id("support"),
            &unique_id("support-slug"),
            "Support",
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    let user_id = unique_id("support-agent");
    let schema = build_schema(db);

    let listed = list(
        &schema,
        json!({ "instanceId": instance.id, "filter": "ALL" }),
        member_auth(&user_id, &instance.id),
    )
    .await;
    assert!(
        expect_error_message(&listed).contains("expected kind"),
        "{listed:?}"
    );
}

#[tokio::test]
async fn invalid_input_is_rejected() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, project_id) = setup(&db, "validate").await;
    let schema = build_schema(db);
    let auth = || member_auth(&agent_id, &instance_id);

    let too_long = "x".repeat(2001);
    let bad_inputs = [
        ("bad date", item_input("19/08/2026", "Work", "1", 100)),
        (
            "impossible date",
            item_input("2026-02-30", "Work", "1", 100),
        ),
        ("unpadded date", item_input("2026-8-1", "Work", "1", 100)),
        (
            "blank description",
            item_input("2026-08-19", "   ", "1", 100),
        ),
        (
            "long description",
            item_input("2026-08-19", &too_long, "1", 100),
        ),
        (
            "3dp quantity",
            item_input("2026-08-19", "Work", "1.234", 100),
        ),
        ("zero quantity", item_input("2026-08-19", "Work", "0", 100)),
        (
            "negative quantity",
            item_input("2026-08-19", "Work", "-1", 100),
        ),
        (
            "huge quantity",
            item_input("2026-08-19", "Work", "1000000.01", 100),
        ),
        (
            "garbage quantity",
            item_input("2026-08-19", "Work", "two", 100),
        ),
        ("negative price", item_input("2026-08-19", "Work", "1", -1)),
        (
            "huge price",
            item_input("2026-08-19", "Work", "1", 1_000_000_001),
        ),
    ];
    for (label, input) in bad_inputs {
        let response = create_item(&schema, &project_id, input, auth()).await;
        assert!(!response.errors.is_empty(), "{label} should be rejected");
    }

    // The bounds themselves are fine: max quantity × max price.
    let max = create_item(
        &schema,
        &project_id,
        item_input("2026-08-19", "Big", "1000000", 1_000_000_000),
        auth(),
    )
    .await;
    assert_eq!(
        expect_data(&max, "createBillableItem.amountCents"),
        json!(1_000_000_000_000_000_i64)
    );

    // A malformed cursor is a plain error, not a panic or a silent restart.
    let bad_cursor = list(
        &schema,
        json!({ "instanceId": instance_id, "filter": "ALL", "after": "nonsense" }),
        member_auth(&agent_id, &instance_id),
    )
    .await;
    assert!(!bad_cursor.errors.is_empty());
}
