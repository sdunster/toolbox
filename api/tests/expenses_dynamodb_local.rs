//! Integration tests for expenses — the `expense` table and its GraphQL
//! surface — against **DynamoDB Local**, following
//! `billable_items_dynamodb_local.rs`'s helper pattern. Covers:
//!
//!   - an agent can create/update/list/delete a purchase and a vehicle trip;
//!     a trip's rate comes from its date's financial year and its amount is
//!     derived;
//!   - an update is a full replace that `REMOVE`s cleared optional
//!     attributes (project, GST, a purchase's fields when it becomes a trip),
//!     never writing `Null`;
//!   - an expense without a project is absent from a project listing;
//!   - a non-member and a superuser without a membership get nothing; a
//!     `projectId` from another instance is `NOT_FOUND`; an archived project
//!     takes no new expenses;
//!   - keyset pagination with the category filter applied over sparse rows;
//!   - `vehicleKmSummary` sums only the caller's own trips in the financial
//!     year;
//!   - a support instance is rejected; field rules per category.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test expenses_dynamodb_local
//! ```
//!
//! Skips itself when no reachable local DynamoDB is configured, like every
//! other `*_dynamodb_local.rs` file.

use std::collections::HashSet;
use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use aws_sdk_dynamodb::types::AttributeValue;
use serde_json::{Value, json};
use toolbox::app::{self, HasDb as _};
use toolbox::auth::{AuthInfo, Membership};
use toolbox::db;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::graphql;
use toolbox::mockmail;
use toolbox::mockstorage;

type TestApp = app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage>;

/// The schema plus the per-request dataloader the server attaches
/// (`server.rs`) — `Expense.project` and `Expense.createdBy` resolve through it.
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
            "expenses_dynamodb_local: {endpoint} is configured but not reachable — \
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
                    "expenses_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set \
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
            "Fictional Job",
            "Fictional Client Pty Ltd",
            None,
            None,
            None,
        )
        .await
        .expect("create_project");
    (instance.id, agent.id, project.id)
}

/// The raw row, for asserting which attributes are present or absent.
async fn raw_row(prefix: &str, id: &str) -> std::collections::HashMap<String, AttributeValue> {
    toolbox::local_dev::dynamodb_client()
        .await
        .get_item()
        .table_name(format!("{prefix}_expense"))
        .key("id", AttributeValue::S(id.to_string()))
        .consistent_read(true)
        .send()
        .await
        .expect("get_item")
        .item
        .expect("row exists")
}

const EXPENSE_FIELDS: &str = "id date category description supplier amountCents gstCents \
                              distanceKm rateCentsPerKm createdAt updatedAt \
                              project { id } createdBy { id }";

fn create_mutation() -> String {
    format!(
        "mutation($instanceId: ID!, $input: ExpenseInput!) {{
            createExpense(instanceId: $instanceId, input: $input) {{ {EXPENSE_FIELDS} }}
        }}"
    )
}

fn update_mutation() -> String {
    format!(
        "mutation($id: ID!, $input: ExpenseInput!) {{
            updateExpense(id: $id, input: $input) {{ {EXPENSE_FIELDS} }}
        }}"
    )
}

const DELETE_MUTATION: &str = "mutation($id: ID!) { deleteExpense(id: $id) }";

const LIST_QUERY: &str = r#"
    query($instanceId: ID!, $projectId: ID, $category: ExpenseCategoryType, $first: Int, $after: String) {
        expenses(instanceId: $instanceId, projectId: $projectId, category: $category, first: $first, after: $after) {
            edges { cursor node { id date category } }
            pageInfo { hasNextPage endCursor }
        }
    }
"#;

const SUMMARY_QUERY: &str = r#"
    query($instanceId: ID!, $financialYear: Int) {
        vehicleKmSummary(instanceId: $instanceId, financialYear: $financialYear) {
            financialYear financialYearLabel totalKm capKm rateCentsPerKm
        }
    }
"#;

fn purchase(date: &str, project_id: Option<&str>) -> Value {
    json!({
        "projectId": project_id,
        "date": date,
        "category": "MATERIALS",
        "supplier": "Fictional Hardware",
        "amountCents": 11_000,
        "gstCents": 1_000,
    })
}

fn trip(date: &str, distance_km: &str) -> Value {
    json!({
        "date": date,
        "category": "VEHICLE_KM",
        "description": "Site visit",
        "distanceKm": distance_km,
    })
}

async fn run(schema: &TestSchema, query: &str, vars: Value, auth: AuthInfo) -> Response {
    schema
        .execute(
            Request::new(query)
                .variables(Variables::from_json(vars))
                .data(auth),
        )
        .await
}

async fn create(schema: &TestSchema, instance_id: &str, input: Value, auth: AuthInfo) -> Response {
    run(
        schema,
        &create_mutation(),
        json!({ "instanceId": instance_id, "input": input }),
        auth,
    )
    .await
}

async fn create_id(schema: &TestSchema, instance_id: &str, input: Value, auth: AuthInfo) -> String {
    let response = create(schema, instance_id, input, auth).await;
    expect_data(&response, "createExpense.id")
        .as_str()
        .unwrap()
        .to_string()
}

/// Walk every page of a listing, returning ids in order.
async fn collect_all(
    schema: &TestSchema,
    vars: Value,
    first: i32,
    auth: &dyn Fn() -> AuthInfo,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..100 {
        let mut page_vars = vars.clone();
        page_vars["first"] = json!(first);
        page_vars["after"] = json!(after);
        let response = run(schema, LIST_QUERY, page_vars, auth()).await;
        let conn = expect_data(&response, "expenses");
        for edge in conn["edges"].as_array().unwrap() {
            out.push(edge["node"]["id"].as_str().unwrap().to_string());
        }
        if conn["pageInfo"]["hasNextPage"] != json!(true) {
            return out;
        }
        after = conn["pageInfo"]["endCursor"].as_str().map(str::to_string);
    }
    panic!("pagination did not terminate");
}

#[tokio::test]
async fn an_agent_can_create_update_list_and_delete_expenses() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, project_id) = setup(&db, "exp-crud").await;
    let schema = build_schema(db);
    let auth = || member_auth(&agent_id, &instance_id);

    let created = create(
        &schema,
        &instance_id,
        purchase("2026-08-19", Some(&project_id)),
        auth(),
    )
    .await;
    let created = expect_data(&created, "createExpense");
    assert_eq!(created["category"], "MATERIALS");
    assert_eq!(created["supplier"], "Fictional Hardware");
    assert_eq!(created["amountCents"], 11_000);
    assert_eq!(created["gstCents"], 1_000);
    assert_eq!(created["distanceKm"], Value::Null);
    assert_eq!(created["project"]["id"], json!(project_id));
    assert_eq!(created["createdBy"]["id"], json!(agent_id));
    let purchase_id = created["id"].as_str().unwrap().to_string();

    // A trip on 1 July 2026 is FY 2026–27: 91c/km. 12.5 km → $11.38 (1137.5c
    // rounded half-up).
    let trip_created = create(&schema, &instance_id, trip("2026-07-01", "12.5"), auth()).await;
    let trip_created = expect_data(&trip_created, "createExpense");
    assert_eq!(trip_created["category"], "VEHICLE_KM");
    assert_eq!(trip_created["distanceKm"], "12.5");
    assert_eq!(trip_created["rateCentsPerKm"], 91);
    assert_eq!(trip_created["amountCents"], 1_138);
    assert_eq!(trip_created["supplier"], Value::Null);
    assert_eq!(trip_created["gstCents"], Value::Null);
    assert_eq!(trip_created["project"], Value::Null);
    let trip_id = trip_created["id"].as_str().unwrap().to_string();
    let row = raw_row(&prefix, &trip_id).await;
    for absent in ["project_id", "supplier", "amount_cents", "gst_cents"] {
        assert!(
            !row.contains_key(absent),
            "{absent} should be absent: {row:?}"
        );
    }

    // Moving the trip back a day re-derives the rate from FY 2025–26 (88c).
    let updated = run(
        &schema,
        &update_mutation(),
        json!({ "id": trip_id, "input": trip("2026-06-30", "12.5") }),
        auth(),
    )
    .await;
    let updated = expect_data(&updated, "updateExpense");
    assert_eq!(updated["rateCentsPerKm"], 88);
    assert_eq!(updated["amountCents"], 1_100);

    let listed = run(
        &schema,
        LIST_QUERY,
        json!({ "instanceId": instance_id }),
        auth(),
    )
    .await;
    let edges = expect_data(&listed, "expenses.edges");
    let ids: Vec<&str> = edges
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["node"]["id"].as_str().unwrap())
        .collect();
    // Newest date first.
    assert_eq!(ids, vec![purchase_id.as_str(), trip_id.as_str()]);

    let deleted = run(&schema, DELETE_MUTATION, json!({ "id": trip_id }), auth()).await;
    assert_eq!(expect_data(&deleted, "deleteExpense"), json!(trip_id));
    let again = run(&schema, DELETE_MUTATION, json!({ "id": trip_id }), auth()).await;
    assert_eq!(expect_error_code(&again), "NOT_FOUND");
}

#[tokio::test]
async fn an_update_removes_cleared_attributes_instead_of_writing_null() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, project_id) = setup(&db, "exp-remove").await;
    let schema = build_schema(db);
    let auth = || member_auth(&agent_id, &instance_id);

    let mut input = purchase("2026-08-19", Some(&project_id));
    input["description"] = json!("Timber");
    let id = create_id(&schema, &instance_id, input, auth()).await;
    let row = raw_row(&prefix, &id).await;
    for present in ["project_id", "gst_cents", "description", "supplier"] {
        assert!(
            row.contains_key(present),
            "{present} should be set: {row:?}"
        );
    }

    // Detach from the project, GST-free, no description.
    let mut cleared = purchase("2026-08-19", None);
    cleared["gstCents"] = Value::Null;
    let response = run(
        &schema,
        &update_mutation(),
        json!({ "id": id, "input": cleared }),
        auth(),
    )
    .await;
    let updated = expect_data(&response, "updateExpense");
    assert_eq!(updated["project"], Value::Null);
    assert_eq!(updated["gstCents"], Value::Null);
    let row = raw_row(&prefix, &id).await;
    for absent in ["project_id", "gst_cents", "description"] {
        assert!(
            !row.contains_key(absent),
            "{absent} should be absent: {row:?}"
        );
    }
    assert!(
        !row.values().any(|v| v.is_null()),
        "no Null attributes: {row:?}"
    );

    // A purchase turned into a trip drops every purchase attribute.
    let response = run(
        &schema,
        &update_mutation(),
        json!({ "id": id, "input": trip("2026-08-19", "3") }),
        auth(),
    )
    .await;
    assert_eq!(
        expect_data(&response, "updateExpense.category"),
        "VEHICLE_KM"
    );
    let row = raw_row(&prefix, &id).await;
    for absent in ["supplier", "amount_cents", "gst_cents"] {
        assert!(
            !row.contains_key(absent),
            "{absent} should be absent: {row:?}"
        );
    }
    assert!(row.contains_key("distance_tenths_km"));
    assert!(row.contains_key("rate_cents_per_km"));
}

#[tokio::test]
async fn an_expense_without_a_project_is_not_in_a_project_listing() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, project_id) = setup(&db, "exp-sparse").await;
    let schema = build_schema(db);
    let auth = || member_auth(&agent_id, &instance_id);

    let on_project = create_id(
        &schema,
        &instance_id,
        purchase("2026-08-19", Some(&project_id)),
        auth(),
    )
    .await;
    let off_project = create_id(&schema, &instance_id, purchase("2026-08-20", None), auth()).await;

    let project_ids = collect_all(
        &schema,
        json!({ "instanceId": instance_id, "projectId": project_id }),
        10,
        &auth,
    )
    .await;
    assert_eq!(project_ids, vec![on_project.clone()]);
    let all_ids = collect_all(&schema, json!({ "instanceId": instance_id }), 10, &auth).await;
    assert_eq!(all_ids, vec![off_project, on_project]);
}

#[tokio::test]
async fn a_non_member_and_a_superuser_get_nothing() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, _) = setup(&db, "exp-outsider").await;
    let schema = build_schema(db);
    let id = create_id(
        &schema,
        &instance_id,
        purchase("2026-08-19", None),
        member_auth(&agent_id, &instance_id),
    )
    .await;

    for auth in [
        outsider_auth(&unique_id("outsider")),
        superuser_auth(&unique_id("super")),
    ] {
        let created = create(
            &schema,
            &instance_id,
            purchase("2026-08-19", None),
            auth.clone(),
        )
        .await;
        assert_eq!(
            expect_error_code(&created),
            "UNAUTHENTICATED",
            "{created:?}"
        );
        let listed = run(
            &schema,
            LIST_QUERY,
            json!({ "instanceId": instance_id }),
            auth.clone(),
        )
        .await;
        assert_eq!(expect_error_code(&listed), "UNAUTHENTICATED", "{listed:?}");
        let summary = run(
            &schema,
            SUMMARY_QUERY,
            json!({ "instanceId": instance_id }),
            auth.clone(),
        )
        .await;
        assert_eq!(
            expect_error_code(&summary),
            "UNAUTHENTICATED",
            "{summary:?}"
        );
        let updated = run(
            &schema,
            &update_mutation(),
            json!({ "id": id, "input": purchase("2026-08-19", None) }),
            auth.clone(),
        )
        .await;
        assert_eq!(expect_error_code(&updated), "NOT_FOUND", "{updated:?}");
        let deleted = run(&schema, DELETE_MUTATION, json!({ "id": id }), auth).await;
        assert_eq!(expect_error_code(&deleted), "NOT_FOUND", "{deleted:?}");
    }
}

#[tokio::test]
async fn a_foreign_or_archived_project_is_refused() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, project_id) = setup(&db, "exp-proj").await;
    let (_, _, foreign_project_id) = setup(&db, "exp-proj-foreign").await;
    let schema = build_schema(db);
    let auth = || member_auth(&agent_id, &instance_id);

    let foreign = create(
        &schema,
        &instance_id,
        purchase("2026-08-19", Some(&foreign_project_id)),
        auth(),
    )
    .await;
    assert_eq!(expect_error_code(&foreign), "NOT_FOUND");

    // An expense already on a project stays editable after it's archived.
    let existing = create_id(
        &schema,
        &instance_id,
        purchase("2026-08-19", Some(&project_id)),
        auth(),
    )
    .await;
    schema
        .app
        .db()
        .update_project(
            &project_id,
            db::ProjectUpdateShape::Fields {
                name: "Fictional Job",
                client_name: "Fictional Client Pty Ltd",
                client_abn: None,
                client_address: None,
                reference: None,
                archived: true,
            },
        )
        .await
        .expect("archive");

    let refused = create(
        &schema,
        &instance_id,
        purchase("2026-08-20", Some(&project_id)),
        auth(),
    )
    .await;
    assert!(
        expect_error_message(&refused).contains("archived"),
        "{refused:?}"
    );
    let edited = run(
        &schema,
        &update_mutation(),
        json!({ "id": existing, "input": purchase("2026-08-21", Some(&project_id)) }),
        auth(),
    )
    .await;
    assert_eq!(expect_data(&edited, "updateExpense.date"), "2026-08-21");
}

#[tokio::test]
async fn pagination_with_a_sparse_category_filter() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, _) = setup(&db, "exp-page").await;
    let schema = build_schema(db);
    let auth = || member_auth(&agent_id, &instance_id);

    // 15 purchases with 5 trips scattered among them, on distinct dates.
    let mut trips = Vec::new();
    for day in 1..=20 {
        let date = format!("2026-08-{day:02}");
        if day % 4 == 0 {
            trips.push(create_id(&schema, &instance_id, trip(&date, "1"), auth()).await);
        } else {
            create_id(&schema, &instance_id, purchase(&date, None), auth()).await;
        }
    }
    trips.reverse(); // newest first

    let got = collect_all(
        &schema,
        json!({ "instanceId": instance_id, "category": "VEHICLE_KM" }),
        2,
        &auth,
    )
    .await;
    assert_eq!(got, trips);

    let all = collect_all(&schema, json!({ "instanceId": instance_id }), 3, &auth).await;
    assert_eq!(all.len(), 20);
    assert_eq!(all.iter().collect::<HashSet<_>>().len(), 20);
}

#[tokio::test]
async fn vehicle_km_summary_counts_only_the_callers_trips_in_the_year() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, _) = setup(&db, "exp-summary").await;
    let colleague = db
        .create_user(&unique_email("exp-summary-colleague"), "Colleague")
        .await
        .expect("create_user");
    db.create_membership(&colleague.id, &instance_id, db::MembershipRole::Agent)
        .await
        .expect("create_membership");
    let schema = build_schema(db);
    let auth = || member_auth(&agent_id, &instance_id);

    // FY 2025–26: 30 June 2026 counts; 1 July 2026 is the next year.
    create_id(&schema, &instance_id, trip("2025-07-01", "100"), auth()).await;
    create_id(&schema, &instance_id, trip("2026-06-30", "20.5"), auth()).await;
    create_id(&schema, &instance_id, trip("2026-07-01", "999"), auth()).await;
    create_id(&schema, &instance_id, purchase("2026-01-10", None), auth()).await;
    create_id(
        &schema,
        &instance_id,
        trip("2026-01-10", "500"),
        member_auth(&colleague.id, &instance_id),
    )
    .await;

    let summary = run(
        &schema,
        SUMMARY_QUERY,
        json!({ "instanceId": instance_id, "financialYear": 2025 }),
        auth(),
    )
    .await;
    let summary = expect_data(&summary, "vehicleKmSummary");
    assert_eq!(
        summary,
        json!({
            "financialYear": 2025,
            "financialYearLabel": "2025–26",
            "totalKm": "120.5",
            "capKm": 5000,
            "rateCentsPerKm": 88,
        })
    );
}

#[tokio::test]
async fn a_support_instance_is_rejected() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let instance = db
        .create_instance(
            &unique_id("exp-support"),
            &unique_id("exp-support-slug"),
            "Support",
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    let user_id = unique_id("exp-support-agent");
    let schema = build_schema(db);
    let auth = || member_auth(&user_id, &instance.id);

    let created = create(&schema, &instance.id, purchase("2026-08-19", None), auth()).await;
    assert!(
        expect_error_message(&created).contains("expected kind"),
        "{created:?}"
    );
    let listed = run(
        &schema,
        LIST_QUERY,
        json!({ "instanceId": instance.id }),
        auth(),
    )
    .await;
    assert!(
        expect_error_message(&listed).contains("expected kind"),
        "{listed:?}"
    );
}

#[tokio::test]
async fn field_rules_follow_the_category() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, agent_id, _) = setup(&db, "exp-invalid").await;
    let schema = build_schema(db);
    let auth = || member_auth(&agent_id, &instance_id);

    let mut trip_with_supplier = trip("2026-08-19", "5");
    trip_with_supplier["supplier"] = json!("Fictional Fuel");
    let mut purchase_with_distance = purchase("2026-08-19", None);
    purchase_with_distance["distanceKm"] = json!("5");
    let mut gst_over_amount = purchase("2026-08-19", None);
    gst_over_amount["gstCents"] = json!(20_000);
    let mut no_purpose = trip("2026-08-19", "5");
    no_purpose["description"] = json!("  ");

    for (input, needle) in [
        (trip_with_supplier, "not a supplier"),
        (purchase_with_distance, "takes a distance"),
        (gst_over_amount, "GST cannot be more"),
        (no_purpose, "business purpose"),
        (trip("2030-01-01", "5"), "No ATO cents-per-km rate"),
        (trip("2026-08-19", "5000.1"), "cannot be more than"),
    ] {
        let response = create(&schema, &instance_id, input, auth()).await;
        assert!(
            expect_error_message(&response).contains(needle),
            "expected {needle:?}: {response:?}"
        );
    }
}
