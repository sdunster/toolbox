//! Integration tests for PR 3 of the invoicing feature — the `invoice`
//! table, its transactional attach/detach/finalize machinery, and its
//! GraphQL surface — against **DynamoDB Local**, following
//! `billable_items_dynamodb_local.rs`'s helper pattern. Covers:
//!
//!   - an agent can create/add/remove/delete/finalize an invoice;
//!   - a non-member gets `NOT_FOUND`; a superuser with no membership gets
//!     nothing; a support instance is rejected;
//!   - an item from another project, or already on an invoice, is a
//!     `CONFLICT`/`NOT_FOUND` on attach;
//!   - finalize assigns sequential numbers and the snapshot is frozen — a
//!     project/settings edit after finalize never changes it;
//!   - a finalized invoice rejects add/remove/delete/finalize; an item on
//!     one rejects update/delete;
//!   - an item on a *draft* invoice can be updated, and that bumps the
//!     invoice's `version` (checked at the `db::Handler` level, where a
//!     stale `expected_version` is directly observable);
//!   - `setInvoicePaid` set/clear, rejected on a draft;
//!   - `setNextInvoiceNumber` is forward-only;
//!   - `finalizeInvoice` requires `businessName` to be set first;
//!   - `BillableItem.status` transitions UNBILLED -> DRAFT -> INVOICED;
//!   - `invoices` list filters (ALL/DRAFT/UNPAID/PAID) and pagination.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test invoices_dynamodb_local
//! ```
//!
//! Skips itself when no reachable local DynamoDB is configured, like every
//! other `*_dynamodb_local.rs` file.

use std::sync::Arc;

use async_graphql::{Request, Response, Variables};
use serde_json::{Value, json};
use toolbox::app;
use toolbox::auth::{AuthInfo, Membership};
use toolbox::db;
use toolbox::db::Handler as _;
use toolbox::dynamodb;
use toolbox::graphql;
use toolbox::mockmail;
use toolbox::mockstorage;
use toolbox::storage::Handler as _;

type TestApp = app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage>;

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
            "invoices_dynamodb_local: {endpoint} is configured but not reachable — \
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
                    "invoices_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set \
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

/// An invoicing instance, with `businessName` already set (so
/// `finalizeInvoice` doesn't need its own setup in every test), one owner,
/// one agent, and one project. Returns `(instance_id, owner_id, agent_id,
/// project_id)`.
async fn setup(db: &dynamodb::Handler, label: &str) -> (String, String, String, String) {
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
    db.update_instance(
        &instance.id,
        db::InstanceUpdateShape::SetInvoicingSettings {
            business_name: Some("Fictional Trades Pty Ltd"),
            business_abn: Some("11 222 333 444"),
            business_address: None,
            business_phone: None,
            business_email: None,
            payment_details: Some("BSB 000-000 Acc 00000000"),
            gst_registered: false,
            currency: None,
        },
    )
    .await
    .expect("set invoicing settings");
    let owner = db
        .create_user(&unique_email(&format!("{label}-owner")), "Owner")
        .await
        .expect("create_user owner");
    db.create_membership(&owner.id, &instance.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership owner");
    let agent = db
        .create_user(&unique_email(&format!("{label}-agent")), "Agent")
        .await
        .expect("create_user agent");
    db.create_membership(&agent.id, &instance.id, db::MembershipRole::Agent)
        .await
        .expect("create_membership agent");
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
    (instance.id, owner.id, agent.id, project.id)
}

async fn create_items(
    db: &dynamodb::Handler,
    instance_id: &str,
    project_id: &str,
    user_id: &str,
    count: usize,
) -> Vec<String> {
    let mut ids = Vec::with_capacity(count);
    for n in 0..count {
        let item = db
            .create_billable_item(
                instance_id,
                project_id,
                "2026-08-19",
                &format!("Line {n}"),
                200,
                10_000,
                user_id,
            )
            .await
            .expect("create_billable_item");
        ids.push(item.id);
    }
    ids
}

const INVOICE_FIELDS: &str = "id status number displayNumber issueDate paidDate currency \
     gstRegistered subtotalCents gstCents totalCents title reference paymentDetails \
     createdAt finalizedAt billTo { name } seller { name } lines { description amountCents } \
     project { id } items { id status }";

fn create_invoice_mutation() -> String {
    format!(
        "mutation($projectId: ID!, $itemIds: [ID!]!) {{
            createInvoice(projectId: $projectId, itemIds: $itemIds) {{ {INVOICE_FIELDS} }}
        }}"
    )
}

fn add_invoice_items_mutation() -> String {
    format!(
        "mutation($invoiceId: ID!, $itemIds: [ID!]!) {{
            addInvoiceItems(invoiceId: $invoiceId, itemIds: $itemIds) {{ {INVOICE_FIELDS} }}
        }}"
    )
}

fn remove_invoice_items_mutation() -> String {
    format!(
        "mutation($invoiceId: ID!, $itemIds: [ID!]!) {{
            removeInvoiceItems(invoiceId: $invoiceId, itemIds: $itemIds) {{ {INVOICE_FIELDS} }}
        }}"
    )
}

const DELETE_INVOICE_MUTATION: &str =
    "mutation($invoiceId: ID!) { deleteInvoice(invoiceId: $invoiceId) }";

fn finalize_invoice_mutation() -> String {
    format!(
        "mutation($invoiceId: ID!, $issueDate: String!, $number: Int) {{
            finalizeInvoice(invoiceId: $invoiceId, issueDate: $issueDate, number: $number) {{
                {INVOICE_FIELDS}
            }}
        }}"
    )
}

fn set_invoice_paid_mutation() -> String {
    format!(
        "mutation($invoiceId: ID!, $paidDate: String) {{
            setInvoicePaid(invoiceId: $invoiceId, paidDate: $paidDate) {{ {INVOICE_FIELDS} }}
        }}"
    )
}

const DOWNLOAD_INVOICE_PDF_MUTATION: &str =
    "mutation($invoiceId: ID!) { downloadInvoicePdf(invoiceId: $invoiceId) }";

async fn download_invoice_pdf(schema: &TestSchema, invoice_id: &str, auth: AuthInfo) -> Response {
    schema
        .execute(
            Request::new(DOWNLOAD_INVOICE_PDF_MUTATION)
                .variables(Variables::from_json(json!({ "invoiceId": invoice_id })))
                .data(auth),
        )
        .await
}

const SET_NEXT_INVOICE_NUMBER_MUTATION: &str = "mutation($instanceId: ID!, $next: Int!) {
    setNextInvoiceNumber(instanceId: $instanceId, next: $next) {
        invoicingSettings { nextInvoiceNumber }
    }
}";

fn invoice_query() -> String {
    format!("query($id: ID!) {{ invoice(id: $id) {{ {INVOICE_FIELDS} }} }}")
}

const LIST_INVOICES_QUERY: &str = "
    query($instanceId: ID!, $projectId: ID, $filter: InvoiceFilterType!, $first: Int, $after: String) {
        invoices(instanceId: $instanceId, projectId: $projectId, filter: $filter, first: $first, after: $after) {
            edges { cursor node { id status } }
            pageInfo { hasNextPage endCursor }
        }
    }
";

const UPDATE_PROJECT_MUTATION: &str = "
    mutation($id: ID!, $input: UpdateProjectInput!) {
        updateProject(id: $id, input: $input) { id clientName }
    }
";

const UPDATE_INVOICING_SETTINGS_MUTATION: &str = "
    mutation($instanceId: ID!, $input: InvoicingSettingsInput!) {
        updateInvoicingSettings(instanceId: $instanceId, input: $input) {
            invoicingSettings { businessName }
        }
    }
";

const UPDATE_BILLABLE_ITEM_MUTATION: &str = "
    mutation($id: ID!, $input: BillableItemInput!) {
        updateBillableItem(id: $id, input: $input) { id status description }
    }
";

const DELETE_BILLABLE_ITEM_MUTATION: &str = "mutation($id: ID!) { deleteBillableItem(id: $id) }";

async fn create_invoice(
    schema: &TestSchema,
    project_id: &str,
    item_ids: &[String],
    auth: AuthInfo,
) -> Response {
    schema
        .execute(
            Request::new(create_invoice_mutation())
                .variables(Variables::from_json(
                    json!({ "projectId": project_id, "itemIds": item_ids }),
                ))
                .data(auth),
        )
        .await
}

async fn finalize(
    schema: &TestSchema,
    invoice_id: &str,
    issue_date: &str,
    auth: AuthInfo,
) -> Response {
    schema
        .execute(
            Request::new(finalize_invoice_mutation())
                .variables(Variables::from_json(
                    json!({ "invoiceId": invoice_id, "issueDate": issue_date }),
                ))
                .data(auth),
        )
        .await
}

/// Happy path: create, add, remove, delete a draft; then create+finalize a
/// second one and confirm its computed totals.
#[tokio::test]
async fn agent_can_create_add_remove_delete_and_finalize() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "flow").await;
    let schema = build_schema(db.clone());
    let auth = || member_auth(&agent_id, &instance_id);

    let items = create_items(&db, &instance_id, &project_id, &agent_id, 3).await;

    // create with 2 items
    let created = create_invoice(&schema, &project_id, &items[0..2], auth()).await;
    let invoice = expect_data(&created, "createInvoice");
    assert_eq!(invoice["status"], "DRAFT");
    assert_eq!(invoice["number"], Value::Null);
    let invoice_id = invoice["id"].as_str().unwrap().to_string();
    assert_eq!(invoice["items"].as_array().unwrap().len(), 2);

    // add the third
    let added = schema
        .execute(
            Request::new(add_invoice_items_mutation())
                .variables(Variables::from_json(
                    json!({ "invoiceId": invoice_id, "itemIds": [items[2].clone()] }),
                ))
                .data(auth()),
        )
        .await;
    let invoice = expect_data(&added, "addInvoiceItems");
    assert_eq!(invoice["items"].as_array().unwrap().len(), 3);

    // remove one
    let removed = schema
        .execute(
            Request::new(remove_invoice_items_mutation())
                .variables(Variables::from_json(
                    json!({ "invoiceId": invoice_id, "itemIds": [items[0].clone()] }),
                ))
                .data(auth()),
        )
        .await;
    let invoice = expect_data(&removed, "removeInvoiceItems");
    assert_eq!(invoice["items"].as_array().unwrap().len(), 2);

    // delete the draft
    let deleted = schema
        .execute(
            Request::new(DELETE_INVOICE_MUTATION)
                .variables(Variables::from_json(json!({ "invoiceId": invoice_id })))
                .data(auth()),
        )
        .await;
    assert_eq!(
        expect_data(&deleted, "deleteInvoice").as_str().unwrap(),
        invoice_id
    );
    // the items are unbilled again
    for id in &items {
        let rec = db
            .get_billable_items(&[id.as_str()])
            .await
            .unwrap()
            .into_iter()
            .next()
            .flatten()
            .unwrap();
        assert!(
            rec.invoice_id.is_none(),
            "item {id} should be unbilled again"
        );
    }

    // second invoice: create + finalize, check totals (2 x $2.00 x 100.00 = ...)
    let items2 = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
    let created2 = create_invoice(&schema, &project_id, &items2, auth()).await;
    let invoice2 = expect_data(&created2, "createInvoice");
    let invoice2_id = invoice2["id"].as_str().unwrap().to_string();
    let finalized = finalize(&schema, &invoice2_id, "2026-08-19", auth()).await;
    let invoice2 = expect_data(&finalized, "finalizeInvoice");
    assert_eq!(invoice2["status"], "FINALIZED");
    assert_eq!(invoice2["number"], 1);
    assert_eq!(invoice2["displayNumber"], "001");
    assert_eq!(invoice2["issueDate"], "2026-08-19");
    // 2.00 x 100.00 = 200.00
    assert_eq!(invoice2["subtotalCents"], 20_000);
    assert_eq!(invoice2["gstCents"], 0);
    assert_eq!(invoice2["totalCents"], 20_000);
    assert_eq!(invoice2["title"], "Invoice");
}

#[tokio::test]
async fn non_member_gets_not_found_everywhere() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "nonmember").await;
    let schema = build_schema(db.clone());
    let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
    let created = create_invoice(
        &schema,
        &project_id,
        &items,
        member_auth(&agent_id, &instance_id),
    )
    .await;
    let invoice_id = expect_data(&created, "createInvoice")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let outsider = || outsider_auth("stranger");
    // createInvoice, authorised through the project
    let attempt = create_invoice(&schema, &project_id, &items, outsider()).await;
    assert_eq!(expect_error_code(&attempt), "NOT_FOUND");

    let add = schema
        .execute(
            Request::new(add_invoice_items_mutation())
                .variables(Variables::from_json(
                    json!({ "invoiceId": invoice_id, "itemIds": Vec::<String>::new() }),
                ))
                .data(outsider()),
        )
        .await;
    assert_eq!(expect_error_code(&add), "NOT_FOUND");

    let remove = schema
        .execute(
            Request::new(remove_invoice_items_mutation())
                .variables(Variables::from_json(
                    json!({ "invoiceId": invoice_id, "itemIds": Vec::<String>::new() }),
                ))
                .data(outsider()),
        )
        .await;
    assert_eq!(expect_error_code(&remove), "NOT_FOUND");

    let delete = schema
        .execute(
            Request::new(DELETE_INVOICE_MUTATION)
                .variables(Variables::from_json(json!({ "invoiceId": invoice_id })))
                .data(outsider()),
        )
        .await;
    assert_eq!(expect_error_code(&delete), "NOT_FOUND");

    let fin = finalize(&schema, &invoice_id, "2026-08-19", outsider()).await;
    assert_eq!(expect_error_code(&fin), "NOT_FOUND");

    let paid = schema
        .execute(
            Request::new(set_invoice_paid_mutation())
                .variables(Variables::from_json(
                    json!({ "invoiceId": invoice_id, "paidDate": "2026-08-19" }),
                ))
                .data(outsider()),
        )
        .await;
    assert_eq!(expect_error_code(&paid), "NOT_FOUND");

    let listed = schema
        .execute(
            Request::new(LIST_INVOICES_QUERY)
                .variables(Variables::from_json(json!({
                    "instanceId": instance_id, "projectId": null, "filter": "ALL", "first": 5, "after": null
                })))
                .data(outsider()),
        )
        .await;
    assert_eq!(expect_error_code(&listed), "UNAUTHENTICATED");
}

#[tokio::test]
async fn superuser_with_no_membership_gets_nothing() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "superuser").await;
    let schema = build_schema(db.clone());
    let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
    let created = create_invoice(
        &schema,
        &project_id,
        &items,
        member_auth(&agent_id, &instance_id),
    )
    .await;
    let invoice_id = expect_data(&created, "createInvoice")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let su = || superuser_auth("root");
    let attempt = create_invoice(&schema, &project_id, &items, su()).await;
    assert_eq!(expect_error_code(&attempt), "NOT_FOUND");

    let fin = finalize(&schema, &invoice_id, "2026-08-19", su()).await;
    assert_eq!(expect_error_code(&fin), "NOT_FOUND");

    let listed = schema
        .execute(
            Request::new(LIST_INVOICES_QUERY)
                .variables(Variables::from_json(json!({
                    "instanceId": instance_id, "projectId": null, "filter": "ALL", "first": 5, "after": null
                })))
                .data(su()),
        )
        .await;
    assert_eq!(expect_error_code(&listed), "UNAUTHENTICATED");

    // setNextInvoiceNumber IS reachable by a superuser (InstanceOwnerOrSuperuser)
    let set_next = schema
        .execute(
            Request::new(SET_NEXT_INVOICE_NUMBER_MUTATION)
                .variables(Variables::from_json(
                    json!({ "instanceId": instance_id, "next": 9 }),
                ))
                .data(su()),
        )
        .await;
    assert!(
        set_next.errors.is_empty(),
        "superuser should be able to set the next invoice number: {:?}",
        set_next.errors
    );
}

#[tokio::test]
async fn support_instance_is_rejected() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let instance = db
        .create_instance(
            &unique_id("support"),
            &unique_id("support-slug"),
            "support",
            "",
            false,
            db::InstanceKind::Support,
        )
        .await
        .expect("create_instance");
    let owner = db
        .create_user(&unique_email("support-owner"), "Owner")
        .await
        .expect("create_user");
    db.create_membership(&owner.id, &instance.id, db::MembershipRole::Owner)
        .await
        .expect("create_membership");
    let schema = build_schema(db.clone());
    let auth = || owner_auth(&owner.id, &instance.id);

    let listed = schema
        .execute(
            Request::new(LIST_INVOICES_QUERY)
                .variables(Variables::from_json(json!({
                    "instanceId": instance.id, "projectId": null, "filter": "ALL", "first": 5, "after": null
                })))
                .data(auth()),
        )
        .await;
    assert!(expect_error_message(&listed).contains("expected kind"));

    let set_next = schema
        .execute(
            Request::new(SET_NEXT_INVOICE_NUMBER_MUTATION)
                .variables(Variables::from_json(
                    json!({ "instanceId": instance.id, "next": 1 }),
                ))
                .data(auth()),
        )
        .await;
    assert!(expect_error_message(&set_next).contains("expected kind"));
}

#[tokio::test]
async fn item_from_another_project_or_already_billed_is_a_conflict() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _owner_id, agent_id, project_a) = setup(&db, "wrongproj").await;
    let project_b = db
        .create_project(&instance_id, "Other Job", "Other Client", None, None, None)
        .await
        .expect("create_project b");
    let schema = build_schema(db.clone());
    let auth = || member_auth(&agent_id, &instance_id);

    let items_a = create_items(&db, &instance_id, &project_a, &agent_id, 1).await;
    let items_b = create_items(&db, &instance_id, &project_b.id, &agent_id, 1).await;

    // item from project B, invoice against project A
    let attempt = create_invoice(&schema, &project_a, &items_b, auth()).await;
    assert_eq!(expect_error_code(&attempt), "CONFLICT");

    // now bill items_a properly, then try to double-bill one of them
    let created = create_invoice(&schema, &project_a, &items_a, auth()).await;
    let invoice_id = expect_data(&created, "createInvoice")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let double = create_invoice(&schema, &project_a, &items_a, auth()).await;
    assert_eq!(expect_error_code(&double), "CONFLICT");

    // Re-adding items that are already on *this* invoice is a duplicate
    // input, not a state conflict — validate_new_invoice_item_ids rejects
    // it before any DB call.
    let add_double = schema
        .execute(
            Request::new(add_invoice_items_mutation())
                .variables(Variables::from_json(
                    json!({ "invoiceId": invoice_id, "itemIds": items_a }),
                ))
                .data(auth()),
        )
        .await;
    assert!(
        expect_error_message(&add_double).contains("duplicated"),
        "{:?}",
        add_double.errors
    );

    // But adding an item that's on a *different* invoice is a CONFLICT.
    let other_items = create_items(&db, &instance_id, &project_a, &agent_id, 1).await;
    let other_invoice = create_invoice(&schema, &project_a, &other_items, auth()).await;
    let other_invoice_id = expect_data(&other_invoice, "createInvoice")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let cross_add = schema
        .execute(
            Request::new(add_invoice_items_mutation())
                .variables(Variables::from_json(
                    json!({ "invoiceId": other_invoice_id, "itemIds": items_a }),
                ))
                .data(auth()),
        )
        .await;
    assert_eq!(expect_error_code(&cross_add), "CONFLICT");
}

#[tokio::test]
async fn finalize_assigns_sequential_numbers_and_freezes_the_snapshot() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id, agent_id, project_id) = setup(&db, "sequential").await;
    let schema = build_schema(db.clone());
    let auth = || member_auth(&agent_id, &instance_id);

    let mut numbers = Vec::new();
    let mut first_invoice_id = String::new();
    for n in 0..3 {
        let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
        let created = create_invoice(&schema, &project_id, &items, auth()).await;
        let invoice_id = expect_data(&created, "createInvoice")["id"]
            .as_str()
            .unwrap()
            .to_string();
        if n == 0 {
            first_invoice_id = invoice_id.clone();
        }
        let finalized = finalize(&schema, &invoice_id, "2026-08-19", auth()).await;
        let invoice = expect_data(&finalized, "finalizeInvoice");
        numbers.push(invoice["number"].as_i64().unwrap());
    }
    assert_eq!(numbers, vec![1, 2, 3]);

    // Read the first invoice's frozen fields before mutating the project/settings.
    let before = expect_data(
        &schema
            .execute(
                Request::new(invoice_query())
                    .variables(Variables::from_json(json!({ "id": first_invoice_id })))
                    .data(auth()),
            )
            .await,
        "invoice",
    );

    // Edit the project's client name and the instance's business name.
    let update_project = schema
        .execute(
            Request::new(UPDATE_PROJECT_MUTATION)
                .variables(Variables::from_json(json!({
                    "id": project_id,
                    "input": {
                        "name": "Fictional Job",
                        "clientName": "Renamed Client Pty Ltd",
                        "clientAbn": null,
                        "clientAddress": null,
                        "reference": null,
                        "archived": false,
                    }
                })))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    assert!(
        update_project.errors.is_empty(),
        "{:?}",
        update_project.errors
    );

    let update_settings = schema
        .execute(
            Request::new(UPDATE_INVOICING_SETTINGS_MUTATION)
                .variables(Variables::from_json(json!({
                    "instanceId": instance_id,
                    "input": {
                        "businessName": "Renamed Trades Pty Ltd",
                        "businessAbn": "",
                        "businessAddress": "",
                        "businessPhone": "",
                        "businessEmail": "",
                        "paymentDetails": "",
                        "gstRegistered": true,
                        "currency": "",
                    }
                })))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    assert!(
        update_settings.errors.is_empty(),
        "{:?}",
        update_settings.errors
    );

    let after = expect_data(
        &schema
            .execute(
                Request::new(invoice_query())
                    .variables(Variables::from_json(json!({ "id": first_invoice_id })))
                    .data(auth()),
            )
            .await,
        "invoice",
    );
    assert_eq!(before["billTo"], after["billTo"]);
    assert_eq!(before["seller"], after["seller"]);
    assert_eq!(before["title"], after["title"]);
    assert_eq!(before["totalCents"], after["totalCents"]);
    assert_eq!(after["billTo"]["name"], "Fictional Client Pty Ltd");
    assert_eq!(after["seller"]["name"], "Fictional Trades Pty Ltd");
    assert_eq!(after["title"], "Invoice");
}

#[tokio::test]
async fn finalized_invoice_rejects_add_remove_delete_and_finalize_again() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "finalfrozen").await;
    let schema = build_schema(db.clone());
    let auth = || member_auth(&agent_id, &instance_id);

    let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
    let extra_items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
    let created = create_invoice(&schema, &project_id, &items, auth()).await;
    let invoice_id = expect_data(&created, "createInvoice")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let finalized = finalize(&schema, &invoice_id, "2026-08-19", auth()).await;
    assert!(finalized.errors.is_empty(), "{:?}", finalized.errors);

    let add = schema
        .execute(
            Request::new(add_invoice_items_mutation())
                .variables(Variables::from_json(
                    json!({ "invoiceId": invoice_id, "itemIds": extra_items }),
                ))
                .data(auth()),
        )
        .await;
    assert_eq!(expect_error_code(&add), "CONFLICT");

    let remove = schema
        .execute(
            Request::new(remove_invoice_items_mutation())
                .variables(Variables::from_json(
                    json!({ "invoiceId": invoice_id, "itemIds": items }),
                ))
                .data(auth()),
        )
        .await;
    assert_eq!(expect_error_code(&remove), "CONFLICT");

    let delete = schema
        .execute(
            Request::new(DELETE_INVOICE_MUTATION)
                .variables(Variables::from_json(json!({ "invoiceId": invoice_id })))
                .data(auth()),
        )
        .await;
    assert_eq!(expect_error_code(&delete), "CONFLICT");

    let refinalize = finalize(&schema, &invoice_id, "2026-08-20", auth()).await;
    assert_eq!(expect_error_code(&refinalize), "CONFLICT");
}

#[tokio::test]
async fn item_on_finalized_invoice_rejects_update_and_delete() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "itemfrozen").await;
    let schema = build_schema(db.clone());
    let auth = || member_auth(&agent_id, &instance_id);

    let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
    let created = create_invoice(&schema, &project_id, &items, auth()).await;
    let invoice_id = expect_data(&created, "createInvoice")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let finalized = finalize(&schema, &invoice_id, "2026-08-19", auth()).await;
    assert!(finalized.errors.is_empty(), "{:?}", finalized.errors);

    let update = schema
        .execute(
            Request::new(UPDATE_BILLABLE_ITEM_MUTATION)
                .variables(Variables::from_json(json!({
                    "id": items[0],
                    "input": { "date": "2026-08-19", "description": "Changed", "quantity": "1", "unitPriceCents": 100 },
                })))
                .data(auth()),
        )
        .await;
    assert_eq!(expect_error_code(&update), "CONFLICT");

    let delete = schema
        .execute(
            Request::new(DELETE_BILLABLE_ITEM_MUTATION)
                .variables(Variables::from_json(json!({ "id": items[0] })))
                .data(auth()),
        )
        .await;
    assert_eq!(expect_error_code(&delete), "CONFLICT");
}

#[tokio::test]
async fn billable_item_status_transitions_unbilled_draft_invoiced() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "statustrans").await;
    let schema = build_schema(db.clone());
    let auth = || member_auth(&agent_id, &instance_id);

    let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;

    let created = create_invoice(&schema, &project_id, &items, auth()).await;
    let invoice = expect_data(&created, "createInvoice");
    let invoice_id = invoice["id"].as_str().unwrap().to_string();
    assert_eq!(invoice["items"][0]["status"], "DRAFT");

    let finalized = finalize(&schema, &invoice_id, "2026-08-19", auth()).await;
    let invoice = expect_data(&finalized, "finalizeInvoice");
    assert_eq!(invoice["items"][0]["status"], "INVOICED");
}

/// `createInvoice`'s own response must already show every attached item as
/// `DRAFT` with `invoice.id` set to the invoice just created — not
/// `UNBILLED`/`null`, which is what an eventually consistent read (the
/// dataloader's `get_invoices`, or a plain `get_billable_items`) could still
/// serve immediately after the same request's write. See `Invoice::items`
/// and `BillableItem`'s `parent_invoice` doc comments.
#[tokio::test]
async fn create_invoice_returns_items_already_reporting_draft_and_their_invoice() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "consistency").await;
    let schema = build_schema(db.clone());
    let auth = || member_auth(&agent_id, &instance_id);

    let items = create_items(&db, &instance_id, &project_id, &agent_id, 3).await;

    let created = schema
        .execute(
            Request::new(
                "mutation($projectId: ID!, $itemIds: [ID!]!) {
                    createInvoice(projectId: $projectId, itemIds: $itemIds) {
                        id
                        items { id status invoice { id } }
                    }
                }",
            )
            .variables(Variables::from_json(
                json!({ "projectId": project_id, "itemIds": items }),
            ))
            .data(auth()),
        )
        .await;
    let invoice = expect_data(&created, "createInvoice");
    let invoice_id = invoice["id"].as_str().unwrap().to_string();
    let returned_items = invoice["items"].as_array().unwrap();
    assert_eq!(returned_items.len(), 3);
    for item in returned_items {
        assert_eq!(item["status"], "DRAFT");
        assert_eq!(item["invoice"]["id"].as_str().unwrap(), invoice_id);
    }
}

#[tokio::test]
async fn set_invoice_paid_sets_and_clears_and_is_rejected_on_a_draft() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "paid").await;
    let schema = build_schema(db.clone());
    let auth = || member_auth(&agent_id, &instance_id);

    let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
    let created = create_invoice(&schema, &project_id, &items, auth()).await;
    let invoice_id = expect_data(&created, "createInvoice")["id"]
        .as_str()
        .unwrap()
        .to_string();

    // rejected on a draft
    let attempt = schema
        .execute(
            Request::new(set_invoice_paid_mutation())
                .variables(Variables::from_json(
                    json!({ "invoiceId": invoice_id, "paidDate": "2026-08-19" }),
                ))
                .data(auth()),
        )
        .await;
    assert_eq!(expect_error_code(&attempt), "CONFLICT");

    finalize(&schema, &invoice_id, "2026-08-19", auth()).await;

    let set = schema
        .execute(
            Request::new(set_invoice_paid_mutation())
                .variables(Variables::from_json(
                    json!({ "invoiceId": invoice_id, "paidDate": "2026-08-20" }),
                ))
                .data(auth()),
        )
        .await;
    let invoice = expect_data(&set, "setInvoicePaid");
    assert_eq!(invoice["paidDate"], "2026-08-20");
    assert_eq!(invoice["status"], "FINALIZED");

    let cleared = schema
        .execute(
            Request::new(set_invoice_paid_mutation())
                .variables(Variables::from_json(
                    json!({ "invoiceId": invoice_id, "paidDate": null }),
                ))
                .data(auth()),
        )
        .await;
    let invoice = expect_data(&cleared, "setInvoicePaid");
    assert_eq!(invoice["paidDate"], Value::Null);
}

#[tokio::test]
async fn next_invoice_number_is_forward_only() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id, agent_id, project_id) = setup(&db, "nextnum").await;
    let schema = build_schema(db.clone());

    let set9 = schema
        .execute(
            Request::new(SET_NEXT_INVOICE_NUMBER_MUTATION)
                .variables(Variables::from_json(
                    json!({ "instanceId": instance_id, "next": 9 }),
                ))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    let next = expect_data(&set9, "setNextInvoiceNumber");
    assert_eq!(next["invoicingSettings"]["nextInvoiceNumber"], 9);

    let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
    let created = create_invoice(
        &schema,
        &project_id,
        &items,
        member_auth(&agent_id, &instance_id),
    )
    .await;
    let invoice_id = expect_data(&created, "createInvoice")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let finalized = finalize(
        &schema,
        &invoice_id,
        "2026-08-19",
        member_auth(&agent_id, &instance_id),
    )
    .await;
    let invoice = expect_data(&finalized, "finalizeInvoice");
    assert_eq!(invoice["number"], 9);

    let set5 = schema
        .execute(
            Request::new(SET_NEXT_INVOICE_NUMBER_MUTATION)
                .variables(Variables::from_json(
                    json!({ "instanceId": instance_id, "next": 5 }),
                ))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    assert_eq!(expect_error_code(&set5), "CONFLICT");

    // moving forward again still works
    let set10 = schema
        .execute(
            Request::new(SET_NEXT_INVOICE_NUMBER_MUTATION)
                .variables(Variables::from_json(
                    json!({ "instanceId": instance_id, "next": 10 }),
                ))
                .data(owner_auth(&owner_id, &instance_id)),
        )
        .await;
    assert!(set10.errors.is_empty(), "{:?}", set10.errors);
}

/// The ids `setup` returns, for the explicit-number test's helpers.
struct ClaimWorld {
    db: dynamodb::Handler,
    schema: TestSchema,
    instance_id: String,
    owner_id: String,
    agent_id: String,
    project_id: String,
}

impl ClaimWorld {
    async fn new_draft(&self) -> String {
        let items = create_items(
            &self.db,
            &self.instance_id,
            &self.project_id,
            &self.agent_id,
            1,
        )
        .await;
        let created = create_invoice(
            &self.schema,
            &self.project_id,
            &items,
            member_auth(&self.agent_id, &self.instance_id),
        )
        .await;
        expect_data(&created, "createInvoice")["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn finalize_numbered(&self, invoice_id: &str, number: i32, auth: AuthInfo) -> Response {
        self.schema
            .execute(
                Request::new(finalize_invoice_mutation())
                    .variables(Variables::from_json(json!({
                        "invoiceId": invoice_id,
                        "issueDate": "2019-03-04",
                        "number": number,
                    })))
                    .data(auth),
            )
            .await
    }

    fn owner(&self) -> AuthInfo {
        owner_auth(&self.owner_id, &self.instance_id)
    }

    /// Finalize a fresh draft as the owner with `number`, expecting it to
    /// be refused as used and the draft left a draft.
    async fn assert_number_refused(&self, number: i32) {
        let draft = self.new_draft().await;
        let refused = self.finalize_numbered(&draft, number, self.owner()).await;
        assert_eq!(expect_error_code(&refused), "CONFLICT", "{number}");
        assert!(
            expect_error_message(&refused).contains("already used"),
            "{refused:?}"
        );
        let still = self
            .schema
            .execute(
                Request::new(invoice_query())
                    .variables(Variables::from_json(json!({ "id": draft })))
                    .data(member_auth(&self.agent_id, &self.instance_id)),
            )
            .await;
        assert_eq!(expect_data(&still, "invoice")["status"], "DRAFT");
    }
}

/// Importing an existing invoice: `finalizeInvoice(number:)` takes exactly
/// that number with a backdated issue date — owner-only; any number no
/// invoice already has, lower than the counter included; a used one (claimed,
/// counter-assigned, or finalized before reservation rows existed) is refused
/// and leaves the draft untouched; and the counter skips past a claim above it.
#[tokio::test]
async fn finalize_with_explicit_number_takes_any_unused_number() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, owner_id, agent_id, project_id) = setup(&db, "claimnum").await;
    let w = ClaimWorld {
        schema: build_schema(db.clone()),
        db,
        instance_id,
        owner_id,
        agent_id,
        project_id,
    };

    // A plain agent can't choose a number; nor can anyone choose zero.
    let first = w.new_draft().await;
    let by_agent = w
        .finalize_numbered(&first, 42, member_auth(&w.agent_id, &w.instance_id))
        .await;
    assert_eq!(expect_error_code(&by_agent), "FORBIDDEN");
    let zero = w.finalize_numbered(&first, 0, w.owner()).await;
    assert!(!zero.errors.is_empty());

    // Above the counter: taken, and the counter moves up to it.
    let imported = w.finalize_numbered(&first, 42, w.owner()).await;
    let invoice = expect_data(&imported, "finalizeInvoice");
    assert_eq!(invoice["number"], 42);
    assert_eq!(invoice["displayNumber"], "042");
    assert_eq!(invoice["issueDate"], "2019-03-04");
    w.assert_number_refused(42).await;

    // Below the counter: fine while unused.
    let lower = w
        .finalize_numbered(&w.new_draft().await, 7, w.owner())
        .await;
    assert_eq!(expect_data(&lower, "finalizeInvoice")["number"], 7);
    w.assert_number_refused(7).await;

    // The automatic sequence carries on after the highest claim, and a
    // counter-assigned number is just as used.
    let auto = finalize(
        &w.schema,
        &w.new_draft().await,
        "2026-08-19",
        member_auth(&w.agent_id, &w.instance_id),
    )
    .await;
    assert_eq!(expect_data(&auto, "finalizeInvoice")["number"], 43);
    w.assert_number_refused(43).await;

    // An invoice finalized before reservation rows existed has none; its
    // number is found on the invoice itself.
    let legacy = w
        .finalize_numbered(&w.new_draft().await, 5, w.owner())
        .await;
    assert_eq!(expect_data(&legacy, "finalizeInvoice")["number"], 5);
    toolbox::local_dev::dynamodb_client()
        .await
        .delete_item()
        .table_name(format!("{prefix}_counter"))
        .key(
            "id",
            aws_sdk_dynamodb::types::AttributeValue::S(db::invoice_number_reservation_id(
                &w.instance_id,
                5,
            )),
        )
        .send()
        .await
        .unwrap();
    w.assert_number_refused(5).await;
}

#[tokio::test]
async fn finalize_requires_business_name_to_be_set_first() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let instance = db
        .create_instance(
            &unique_id("nobiz"),
            &unique_id("nobiz-slug"),
            "nobiz",
            "",
            false,
            db::InstanceKind::Invoicing,
        )
        .await
        .expect("create_instance");
    // deliberately no updateInstance call: business_name stays absent
    let agent = db
        .create_user(&unique_email("nobiz-agent"), "Agent")
        .await
        .expect("create_user");
    db.create_membership(&agent.id, &instance.id, db::MembershipRole::Agent)
        .await
        .expect("create_membership");
    let project = db
        .create_project(&instance.id, "Job", "Client", None, None, None)
        .await
        .expect("create_project");
    let schema = build_schema(db.clone());
    let auth = || member_auth(&agent.id, &instance.id);

    let items = create_items(&db, &instance.id, &project.id, &agent.id, 1).await;
    let created = create_invoice(&schema, &project.id, &items, auth()).await;
    let invoice_id = expect_data(&created, "createInvoice")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let finalized = finalize(&schema, &invoice_id, "2026-08-19", auth()).await;
    assert!(
        expect_error_message(&finalized).contains("Complete the invoicing settings first"),
        "{:?}",
        finalized.errors
    );
}

#[tokio::test]
async fn invoice_list_filters_and_paginates() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "listing").await;

    // 2 draft, 2 finalized-unpaid, 1 finalized-paid = 5 total
    let mut draft_ids = Vec::new();
    let mut finalized_ids = Vec::new();
    for _ in 0..2 {
        let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
        let inv = db
            .create_invoice(&instance_id, &project_id, &items, &agent_id)
            .await
            .expect("create_invoice")
            .expect("committed");
        draft_ids.push(inv.id);
    }
    for _ in 0..3 {
        let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
        let inv = db
            .create_invoice(&instance_id, &project_id, &items, &agent_id)
            .await
            .expect("create_invoice")
            .expect("committed");
        let number = db.increment_invoice_counter(&instance_id).await.unwrap();
        let ok = db
            .finalize_invoice(
                &instance_id,
                &inv.id,
                inv.version,
                number as u32,
                "2026-08-19",
                "{}",
                0,
                &agent_id,
            )
            .await
            .expect("finalize_invoice");
        assert!(ok);
        finalized_ids.push(inv.id);
    }
    // mark the first finalized invoice paid
    assert!(
        db.set_invoice_paid(&finalized_ids[0], Some("2026-08-20"))
            .await
            .expect("set_invoice_paid")
    );

    let schema = build_schema(db.clone());
    let auth = || member_auth(&agent_id, &instance_id);

    async fn collect_all(
        schema: &TestSchema,
        instance_id: &str,
        filter: &str,
        first: i32,
        auth: &dyn Fn() -> AuthInfo,
    ) -> Vec<String> {
        let mut out = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..100 {
            let response = schema
                .execute(
                    Request::new(LIST_INVOICES_QUERY)
                        .variables(Variables::from_json(json!({
                            "instanceId": instance_id,
                            "projectId": null,
                            "filter": filter,
                            "first": first,
                            "after": after,
                        })))
                        .data(auth()),
                )
                .await;
            let conn = expect_data(&response, "invoices");
            let edges = conn["edges"].as_array().unwrap();
            assert!(edges.len() <= first as usize, "page larger than `first`");
            for e in edges {
                out.push(e["node"]["id"].as_str().unwrap().to_string());
            }
            if conn["pageInfo"]["hasNextPage"] != json!(true) {
                return out;
            }
            assert_eq!(
                edges.len(),
                first as usize,
                "a full page must report hasNextPage correctly"
            );
            after = Some(conn["pageInfo"]["endCursor"].as_str().unwrap().to_string());
        }
        panic!("pagination did not terminate");
    }

    let all = collect_all(&schema, &instance_id, "ALL", 2, &auth).await;
    assert_eq!(all.len(), 5, "{all:?}");
    assert_eq!(
        all.iter().collect::<std::collections::HashSet<_>>().len(),
        5,
        "no duplicates"
    );

    let draft = collect_all(&schema, &instance_id, "DRAFT", 1, &auth).await;
    assert_eq!(draft.len(), 2);
    assert!(draft.iter().all(|id| draft_ids.contains(id)));

    let unpaid = collect_all(&schema, &instance_id, "UNPAID", 5, &auth).await;
    assert_eq!(unpaid.len(), 2);
    assert!(!unpaid.contains(&finalized_ids[0]));

    let paid = collect_all(&schema, &instance_id, "PAID", 5, &auth).await;
    assert_eq!(paid, vec![finalized_ids[0].clone()]);
}

/// `downloadInvoicePdf`: the first call renders from the frozen snapshot,
/// stores it, and caches `pdf_s3_key`; the second call just re-presigns the
/// same cached key. Both calls return a non-empty URL, and the stored
/// object is a real (if mock-storage-backed) PDF.
#[tokio::test]
async fn download_invoice_pdf_renders_caches_and_presigns_twice() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "pdf").await;
    let schema = build_schema(db.clone());
    let auth = || member_auth(&agent_id, &instance_id);

    let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
    let created = create_invoice(&schema, &project_id, &items, auth()).await;
    let invoice_id = expect_data(&created, "createInvoice")["id"]
        .as_str()
        .unwrap()
        .to_string();
    finalize(&schema, &invoice_id, "2026-08-19", auth()).await;

    // Before the first download, no key is cached yet.
    let before = db
        .get_invoice_consistent(&invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.pdf_s3_key, None);

    let first = download_invoice_pdf(&schema, &invoice_id, auth()).await;
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    let first_url = expect_data(&first, "downloadInvoicePdf")
        .as_str()
        .unwrap()
        .to_string();
    assert!(!first_url.is_empty());

    let after_first = db
        .get_invoice_consistent(&invoice_id)
        .await
        .unwrap()
        .unwrap();
    let key = after_first
        .pdf_s3_key
        .clone()
        .expect("pdf_s3_key set after first download");
    assert_eq!(
        key,
        format!("invoices/{instance_id}/{invoice_id}/Invoice-001.pdf")
    );

    // The mock storage's stored object is a real PDF.
    let storage = mockstorage::Storage::new();
    let bytes = storage.get_bytes(&key).await.expect("stored PDF bytes");
    assert!(bytes.starts_with(b"%PDF"), "expected a PDF, got {bytes:?}");
    assert!(bytes.len() > 500);

    // The second call reuses the cached key rather than re-rendering — the
    // key (and the underlying bytes) are unchanged.
    let second = download_invoice_pdf(&schema, &invoice_id, auth()).await;
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    let second_url = expect_data(&second, "downloadInvoicePdf")
        .as_str()
        .unwrap()
        .to_string();
    assert!(!second_url.is_empty());

    let after_second = db
        .get_invoice_consistent(&invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after_second.pdf_s3_key, Some(key));
}

#[tokio::test]
async fn download_invoice_pdf_rejects_a_draft() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "pdfdraft").await;
    let schema = build_schema(db.clone());
    let auth = || member_auth(&agent_id, &instance_id);

    let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
    let created = create_invoice(&schema, &project_id, &items, auth()).await;
    let invoice_id = expect_data(&created, "createInvoice")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let attempt = download_invoice_pdf(&schema, &invoice_id, auth()).await;
    assert_eq!(expect_error_code(&attempt), "CONFLICT");
}

#[tokio::test]
async fn download_invoice_pdf_non_member_gets_not_found() {
    let prefix = require_local_db!();
    let db = dynamodb::Handler::new(&prefix, false).await;
    let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "pdfoutsider").await;
    let schema = build_schema(db.clone());

    let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
    let created = create_invoice(
        &schema,
        &project_id,
        &items,
        member_auth(&agent_id, &instance_id),
    )
    .await;
    let invoice_id = expect_data(&created, "createInvoice")["id"]
        .as_str()
        .unwrap()
        .to_string();
    finalize(
        &schema,
        &invoice_id,
        "2026-08-19",
        member_auth(&agent_id, &instance_id),
    )
    .await;

    let attempt =
        download_invoice_pdf(&schema, &invoice_id, outsider_auth(&unique_id("outsider"))).await;
    assert_eq!(expect_error_code(&attempt), "NOT_FOUND");
}

/// `db::Handler`-level checks of the optimistic-concurrency machinery: a
/// stale `expected_version` is rejected cleanly (`Ok(false)`), never
/// panicking or silently succeeding. This is the "concurrent edit" case
/// CLAUDE.md's finalize algorithm documents — editing an item on a draft
/// invoice bumps its `version`, so a finalize that read the invoice before
/// that edit must fail when it finally writes.
mod db_level_conflicts {
    use super::*;

    #[tokio::test]
    async fn editing_an_item_on_a_draft_bumps_version_and_a_stale_finalize_conflicts() {
        let prefix = require_local_db!();
        let db = dynamodb::Handler::new(&prefix, false).await;
        let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "staleversion").await;
        let items = create_items(&db, &instance_id, &project_id, &agent_id, 1).await;
        let invoice = db
            .create_invoice(&instance_id, &project_id, &items, &agent_id)
            .await
            .expect("create_invoice")
            .expect("committed");
        assert_eq!(invoice.version, 1);

        // Edit the item while it's on the draft — this must succeed and
        // bump the invoice's version.
        let item = db
            .get_billable_items(&[items[0].as_str()])
            .await
            .unwrap()
            .into_iter()
            .next()
            .flatten()
            .unwrap();
        let written = db
            .update_billable_item(
                &item.id,
                item.invoice_id.as_deref(),
                db::BillableItemUpdateShape::Fields {
                    date: "2026-08-19",
                    description: "Edited while draft",
                    quantity_hundredths: 300,
                    unit_price_cents: 5_000,
                },
            )
            .await
            .expect("update_billable_item");
        assert!(written);

        // A finalize using the *stale* (pre-edit) version must be refused.
        let stale = db
            .finalize_invoice(
                &instance_id,
                &invoice.id,
                invoice.version,
                1,
                "2026-08-19",
                "{}",
                0,
                &agent_id,
            )
            .await
            .expect("finalize_invoice");
        assert!(!stale, "a stale version must not be allowed to finalize");

        // The correct (current) version does succeed.
        let current = db
            .get_invoice_consistent(&invoice.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.version, invoice.version + 1);
        let ok = db
            .finalize_invoice(
                &instance_id,
                &invoice.id,
                current.version,
                1,
                "2026-08-19",
                "{}",
                0,
                &agent_id,
            )
            .await
            .expect("finalize_invoice");
        assert!(ok);
    }

    #[tokio::test]
    async fn add_remove_delete_finalize_all_reject_a_stale_version() {
        let prefix = require_local_db!();
        let db = dynamodb::Handler::new(&prefix, false).await;
        let (instance_id, _owner_id, agent_id, project_id) = setup(&db, "staleversion2").await;
        let items = create_items(&db, &instance_id, &project_id, &agent_id, 2).await;
        let invoice = db
            .create_invoice(&instance_id, &project_id, &[items[0].clone()], &agent_id)
            .await
            .expect("create_invoice")
            .expect("committed");
        let wrong_version = invoice.version + 41;

        assert!(
            !db.add_invoice_items(&invoice.id, &project_id, &[items[1].clone()], wrong_version)
                .await
                .expect("add_invoice_items")
        );
        assert!(
            !db.remove_invoice_items(&invoice.id, &[items[0].clone()], wrong_version)
                .await
                .expect("remove_invoice_items")
        );
        assert!(
            !db.delete_invoice(&invoice.id, &[items[0].clone()], wrong_version)
                .await
                .expect("delete_invoice")
        );
        assert!(
            !db.finalize_invoice(
                &instance_id,
                &invoice.id,
                wrong_version,
                1,
                "2026-08-19",
                "{}",
                0,
                &agent_id
            )
            .await
            .expect("finalize_invoice")
        );

        // the invoice is untouched — still a draft, version unchanged, item still attached
        let still = db
            .get_invoice_consistent(&invoice.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still.version, invoice.version);
        assert_eq!(still.status, db::InvoiceStatus::Draft);
        assert_eq!(still.item_ids, vec![items[0].clone()]);
    }
}
