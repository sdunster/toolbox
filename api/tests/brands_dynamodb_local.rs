//! Integration tests for brands — the other names one finance instance
//! issues under — against **DynamoDB Local**. Covers:
//!
//!   - `addBrand`/`updateBrand`: owner-only, names unique, the default brand
//!     can't be edited here, a support instance is refused, and a stale
//!     read-modify-write of the list is refused at the `db::Handler` level;
//!   - a project's brand flows to its drafts, is frozen into an invoice at
//!     finalization (seller printed "{legal} trading as {brand}"), and is
//!     copied onto credit notes — later project or brand edits never change
//!     a finalized invoice;
//!   - an archived brand can't be chosen for a new project or expense, but
//!     a row already on it keeps it through an edit;
//!   - the `brandId` filter on `invoices`, `creditNotes`, `expenses`,
//!     `gstReport` and `receivables` (a draft moving with its project), and
//!     an unknown id is refused.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test brands_dynamodb_local
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
use toolbox::mockqueue;
use toolbox::mockstorage;

type TestApp =
    app::MyApp<dynamodb::Handler, mockmail::Handler, mockstorage::Storage, mockqueue::Queue>;

struct TestSchema {
    app: Arc<TestApp>,
    schema: graphql::ToolboxSchema<TestApp>,
}

impl TestSchema {
    async fn run(&self, query: &str, vars: Value, auth: AuthInfo) -> Response {
        self.schema
            .execute(
                Request::new(query)
                    .variables(Variables::from_json(vars))
                    .data(auth)
                    .data(graphql::get_dataloader(self.app.clone())),
            )
            .await
    }
}

async fn local_db_prefix() -> Option<String> {
    let endpoint = toolbox::local_dev::require_local_dynamodb_endpoint().ok()?;
    let prefix = std::env::var("DB_PREFIX").ok()?;
    let client = toolbox::local_dev::dynamodb_client().await;
    if client.list_tables().send().await.is_err() {
        eprintln!(
            "brands_dynamodb_local: {endpoint} is configured but not reachable — \
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
                    "brands_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not set to a \
                     reachable local DynamoDB — skipping. See this file's header."
                );
                return;
            }
        }
    };
}

fn unique(label: &str) -> String {
    let suffix: String = nanoid::nanoid!(16)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    format!("{label}-{suffix}").to_lowercase()
}

fn data(response: &Response, path: &str) -> Value {
    assert!(
        response.errors.is_empty(),
        "unexpected GraphQL errors on {path}: {:?}",
        response.errors
    );
    let all = serde_json::to_value(&response.data).expect("response data serializes");
    let mut cur = &all;
    for segment in path.split('.') {
        cur = cur
            .get(segment)
            .unwrap_or_else(|| panic!("response data missing `{segment}` (path {path}): {all}"));
    }
    cur.clone()
}

fn error_message(response: &Response) -> String {
    response
        .errors
        .first()
        .unwrap_or_else(|| panic!("expected a GraphQL error, got none: {response:?}"))
        .message
        .clone()
}

fn error_code(response: &Response) -> String {
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

struct World {
    s: TestSchema,
    instance_id: String,
    slug: String,
    owner_id: String,
    agent_id: String,
    /// The client every project here bills.
    client_id: String,
}

impl World {
    fn owner(&self) -> AuthInfo {
        user_auth(&self.owner_id, &self.instance_id, true)
    }

    fn agent(&self) -> AuthInfo {
        user_auth(&self.agent_id, &self.instance_id, false)
    }

    /// Run as the agent — day-to-day finance work.
    async fn run(&self, query: &str, vars: Value) -> Response {
        self.s.run(query, vars, self.agent()).await
    }

    fn db(&self) -> &dynamodb::Handler {
        &self.s.app.db
    }
}

/// An instance of `kind` named `label`, with finance settings whose
/// legal name is "Samuel James Dunster", one owner and one agent.
async fn setup_kind(label: &str, kind: db::InstanceKind) -> Option<World> {
    let prefix = local_db_prefix().await?;
    let db = dynamodb::Handler::new(&prefix, false).await;
    let instance = db
        .create_instance(
            &unique(label),
            &unique(&format!("{label}-slug")),
            label,
            "",
            false,
            kind,
        )
        .await
        .expect("create_instance");
    if kind == db::InstanceKind::Finance {
        db.update_instance(
            &instance.id,
            db::InstanceUpdateShape::SetFinanceSettings {
                business_name: Some("Samuel James Dunster"),
                business_abn: Some("11 222 333 444"),
                business_address: None,
                business_phone: None,
                business_email: Some("sam@example.com"),
                payment_details: Some("BSB 000-000 Acc 00000000"),
                gst_registered: true,
                currency: None,
                payment_terms_days: None,
            },
        )
        .await
        .expect("settings");
    }
    let owner = db
        .create_user(&format!("{}@example.com", unique("owner")), "Owner")
        .await
        .expect("create_user owner");
    db.create_membership(&owner.id, &instance.id, db::MembershipRole::Owner)
        .await
        .expect("membership owner");
    let agent = db
        .create_user(&format!("{}@example.com", unique("agent")), "Agent")
        .await
        .expect("create_user agent");
    db.create_membership(&agent.id, &instance.id, db::MembershipRole::Agent)
        .await
        .expect("membership agent");
    let client = db
        .create_client(
            &instance.id,
            &db::ClientFields {
                name: "Fictional Client Pty Ltd".into(),
                ..Default::default()
            },
        )
        .await
        .expect("create_client");
    let my_app = Arc::new(app::new(
        db,
        mockmail::Handler::new(),
        mockstorage::Storage::with_dir(std::env::temp_dir().join(unique("brands-storage"))),
        mockqueue::Queue::new(),
        0,
    ));
    let webauthn = Arc::new(app::build_webauthn().expect("WebAuthn build failed"));
    let schema = graphql::build_schema(my_app.clone(), webauthn);
    Some(World {
        s: TestSchema {
            app: my_app,
            schema,
        },
        instance_id: instance.id,
        slug: instance.slug,
        owner_id: owner.id,
        agent_id: agent.id,
        client_id: client.id,
    })
}

async fn setup(label: &str) -> Option<World> {
    setup_kind(label, db::InstanceKind::Finance).await
}

const BRAND_FIELDS: &str = "brandId name email phone address paymentDetails archived isDefault";

fn add_brand_mutation() -> String {
    format!(
        "mutation($i: ID!, $input: BrandInput!) {{
            addBrand(instanceId: $i, input: $input) {{ brands {{ {BRAND_FIELDS} }} }}
        }}"
    )
}

fn update_brand_mutation() -> String {
    format!(
        "mutation($i: ID!, $b: ID!, $input: BrandInput!, $archived: Boolean!) {{
            updateBrand(instanceId: $i, brandId: $b, input: $input, archived: $archived) {{
                brands {{ {BRAND_FIELDS} }}
            }}
        }}"
    )
}

/// Add a brand as the owner and return its id.
async fn add_brand(w: &World, input: Value) -> String {
    let r =
        w.s.run(
            &add_brand_mutation(),
            json!({ "i": w.instance_id, "input": input }),
            w.owner(),
        )
        .await;
    let brands = data(&r, "addBrand.brands");
    let last = brands.as_array().unwrap().last().unwrap();
    last["brandId"].as_str().unwrap().to_string()
}

async fn update_brand(w: &World, brand_id: &str, input: Value, archived: bool) -> Response {
    w.s.run(
        &update_brand_mutation(),
        json!({ "i": w.instance_id, "b": brand_id, "input": input, "archived": archived }),
        w.owner(),
    )
    .await
}

async fn create_project(w: &World, name: &str, brand_id: Option<&str>) -> Response {
    w.run(
        "mutation($i: ID!, $input: CreateProjectInput!) {
            createProject(instanceId: $i, input: $input) { id brand { brandId name isDefault } }
        }",
        json!({ "i": w.instance_id, "input": {
            "name": name, "clientId": w.client_id, "brandId": brand_id,
        } }),
    )
    .await
}

async fn project(w: &World, name: &str, brand_id: Option<&str>) -> String {
    let r = create_project(w, name, brand_id).await;
    data(&r, "createProject.id").as_str().unwrap().to_string()
}

async fn set_project_brand(w: &World, project_id: &str, name: &str, brand_id: Option<&str>) {
    let r = w
        .run(
            "mutation($id: ID!, $input: UpdateProjectInput!) {
                updateProject(id: $id, input: $input) { id }
            }",
            json!({ "id": project_id, "input": {
                "name": name, "clientId": w.client_id, "brandId": brand_id,
                "archived": false,
            } }),
        )
        .await;
    data(&r, "updateProject.id");
}

const INVOICE_FIELDS: &str = "id status totalCents paymentDetails \
     brand { brandId name isDefault } seller { name legalName email }";

/// A draft invoice of one $100 item on `project_id`.
async fn draft(w: &World, project_id: &str) -> Value {
    let r = w
        .run(
            "mutation($p: ID!, $input: BillableItemInput!) {
                createBillableItem(projectId: $p, input: $input) { id }
            }",
            json!({ "p": project_id, "input": {
                "date": "2026-08-01", "description": "Work", "quantity": "1",
                "unitPriceCents": 10_000,
            } }),
        )
        .await;
    let item = data(&r, "createBillableItem.id");
    let r = w
        .run(
            &format!(
                "mutation($p: ID!, $items: [ID!]!) {{
                    createInvoice(projectId: $p, itemIds: $items) {{ {INVOICE_FIELDS} }}
                }}"
            ),
            json!({ "p": project_id, "items": [item] }),
        )
        .await;
    data(&r, "createInvoice")
}

async fn finalize(w: &World, invoice_id: &str) -> Value {
    let r = w
        .run(
            &format!(
                "mutation($id: ID!) {{
                    finalizeInvoice(invoiceId: $id, issueDate: \"2026-08-01\") {{ {INVOICE_FIELDS} }}
                }}"
            ),
            json!({ "id": invoice_id }),
        )
        .await;
    data(&r, "finalizeInvoice")
}

async fn get_invoice(w: &World, id: &str) -> Value {
    let r = w
        .run(
            &format!("query($id: ID!) {{ invoice(id: $id) {{ {INVOICE_FIELDS} }} }}"),
            json!({ "id": id }),
        )
        .await;
    data(&r, "invoice")
}

async fn create_expense(w: &World, brand_id: Option<&str>) -> Response {
    w.run(
        "mutation($i: ID!, $input: ExpenseInput!) {
            createExpense(instanceId: $i, input: $input) { id brand { brandId } }
        }",
        json!({ "i": w.instance_id, "input": {
            "date": "2026-08-03", "category": "MATERIALS", "supplier": "Hardware Co",
            "amountCents": 11_000, "gstCents": 1_000, "brandId": brand_id,
        } }),
    )
    .await
}

fn ids(connection: &Value) -> Vec<String> {
    let mut ids: Vec<String> = connection["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["node"]["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

#[tokio::test]
async fn owner_manages_brands_and_the_default_comes_first() {
    let _ = require_local_db!();
    let Some(w) = setup("brandmgmt").await else {
        return;
    };

    // Before any brand, the list is just the default — the settings.
    let r = w
        .run(
            &format!(
                "query($s: String!) {{ instance(slug: $s) {{ brands {{ {BRAND_FIELDS} }} }} }}"
            ),
            json!({ "s": w.slug }),
        )
        .await;
    let brands = &data(&r, "instance.brands");
    assert_eq!(
        brands,
        &json!([{
            "brandId": "default", "name": "Samuel James Dunster", "email": "sam@example.com",
            "phone": null, "address": null, "paymentDetails": "BSB 000-000 Acc 00000000",
            "archived": false, "isDefault": true,
        }])
    );

    // Blank optional fields are stored as absent and read back as null.
    let id = add_brand(
        &w,
        json!({ "name": " Positive Rate ", "email": "hello@positiverate.example", "phone": "  " }),
    )
    .await;
    let r = update_brand(
        &w,
        &id,
        json!({ "name": "Positive Rate", "email": "hello@positiverate.example",
                "address": "1 Brand St" }),
        false,
    )
    .await;
    let brands = data(&r, "updateBrand.brands");
    assert_eq!(brands.as_array().unwrap().len(), 2);
    assert_eq!(brands[0]["isDefault"], json!(true));
    assert_eq!(
        brands[1],
        json!({
            "brandId": id, "name": "Positive Rate", "email": "hello@positiverate.example",
            "phone": null, "address": "1 Brand St", "paymentDetails": null,
            "archived": false, "isDefault": false,
        })
    );

    // Names are unique, case-insensitively.
    let r =
        w.s.run(
            &add_brand_mutation(),
            json!({ "i": w.instance_id, "input": { "name": "positive rate" } }),
            w.owner(),
        )
        .await;
    assert!(error_message(&r).contains("already a brand"), "{r:?}");

    // An agent can't add or edit one.
    let r = w
        .run(
            &add_brand_mutation(),
            json!({ "i": w.instance_id, "input": { "name": "Agent Brand" } }),
        )
        .await;
    assert_eq!(error_code(&r), "UNAUTHENTICATED");

    // The default brand is edited through the settings, and an unknown id
    // is NOT_FOUND.
    let r = update_brand(&w, "default", json!({ "name": "x" }), false).await;
    assert!(error_message(&r).contains("finance settings"), "{r:?}");
    let r = update_brand(&w, "nope", json!({ "name": "x" }), false).await;
    assert_eq!(error_code(&r), "NOT_FOUND");

    // A stale read-modify-write of the list is refused, not merged.
    let instance = w
        .db()
        .get_instances(&[w.instance_id.as_str()])
        .await
        .unwrap()
        .remove(0)
        .unwrap();
    assert_eq!(instance.brands.len(), 1);
    assert!(
        !w.db()
            .set_brands(&w.instance_id, &[], &instance.brands)
            .await
            .unwrap(),
        "a write over a list that has since changed must not commit"
    );
}

#[tokio::test]
async fn a_support_instance_has_no_brands() {
    let _ = require_local_db!();
    let Some(w) = setup_kind("brandsupport", db::InstanceKind::Support).await else {
        return;
    };
    let r =
        w.s.run(
            &add_brand_mutation(),
            json!({ "i": w.instance_id, "input": { "name": "Nope" } }),
            w.owner(),
        )
        .await;
    assert!(!r.errors.is_empty(), "support instance took a brand: {r:?}");
}

#[tokio::test]
async fn a_brand_flows_from_project_to_invoice_and_freezes_at_finalize() {
    let _ = require_local_db!();
    let Some(w) = setup("brandflow").await else {
        return;
    };
    let brand = add_brand(
        &w,
        json!({ "name": "Positive Rate", "email": "hello@positiverate.example" }),
    )
    .await;
    let r = create_project(&w, "Branded Job", Some(&brand)).await;
    assert_eq!(
        data(&r, "createProject.brand"),
        json!({ "brandId": brand, "name": "Positive Rate", "isDefault": false })
    );
    let project_id = data(&r, "createProject.id").as_str().unwrap().to_string();

    // A draft follows its project, both ways.
    let d = draft(&w, &project_id).await;
    let invoice_id = d["id"].as_str().unwrap().to_string();
    assert_eq!(d["brand"]["brandId"], json!(brand));
    assert_eq!(d["seller"]["name"], json!("Positive Rate"));
    assert_eq!(d["seller"]["legalName"], json!("Samuel James Dunster"));
    assert_eq!(d["seller"]["email"], json!("hello@positiverate.example"));
    // Unset on the brand: falls back to the settings.
    assert_eq!(d["paymentDetails"], json!("BSB 000-000 Acc 00000000"));
    set_project_brand(&w, &project_id, "Branded Job", None).await;
    let d = get_invoice(&w, &invoice_id).await;
    assert_eq!(d["brand"]["isDefault"], json!(true));
    assert_eq!(d["seller"]["name"], json!("Samuel James Dunster"));
    assert_eq!(d["seller"]["legalName"], json!(null));
    set_project_brand(&w, &project_id, "Branded Job", Some(&brand)).await;

    // Finalized: the brand and the seller it printed are fixed.
    let f = finalize(&w, &invoice_id).await;
    assert_eq!(f["brand"]["brandId"], json!(brand));
    assert_eq!(f["seller"]["name"], json!("Positive Rate"));
    assert_eq!(f["seller"]["legalName"], json!("Samuel James Dunster"));
    set_project_brand(&w, &project_id, "Branded Job", None).await;
    let r = update_brand(&w, &brand, json!({ "name": "Renamed Rate" }), false).await;
    data(&r, "updateBrand.brands");
    let after = get_invoice(&w, &invoice_id).await;
    assert_eq!(after["brand"]["brandId"], json!(brand));
    assert_eq!(
        after["seller"], f["seller"],
        "a finalized invoice never changes"
    );
    assert_eq!(after["paymentDetails"], f["paymentDetails"]);

    // The stored row carries the brand it was finalized under.
    let row = w
        .db()
        .get_invoice_consistent(&invoice_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.brand_id.as_deref(), Some(brand.as_str()));

    // A credit note copies the invoice's brand, not the project's.
    let r = w
        .run(
            "mutation($id: ID!, $input: CreditNoteInput!) {
                issueCreditNote(invoiceId: $id, input: $input) { id brand { brandId } }
            }",
            json!({ "id": invoice_id, "input": { "issueDate": "2026-08-02", "reason": "Refund" } }),
        )
        .await;
    assert_eq!(data(&r, "issueCreditNote.brand.brandId"), json!(brand));

    // The PDF renders from the branded snapshot.
    let r = w
        .run(
            "mutation($id: ID!) { downloadInvoicePdf(invoiceId: $id) }",
            json!({ "id": invoice_id }),
        )
        .await;
    data(&r, "downloadInvoicePdf");
}

#[tokio::test]
async fn an_archived_brand_takes_no_new_rows_but_keeps_its_old_ones() {
    let _ = require_local_db!();
    let Some(w) = setup("brandarchive").await else {
        return;
    };
    let brand = add_brand(&w, json!({ "name": "Old Name" })).await;
    let project_id = project(&w, "Old Job", Some(&brand)).await;
    let r = create_expense(&w, Some(&brand)).await;
    let expense_id = data(&r, "createExpense.id").as_str().unwrap().to_string();

    let r = update_brand(&w, &brand, json!({ "name": "Old Name" }), true).await;
    assert_eq!(data(&r, "updateBrand.brands")[1]["archived"], json!(true));

    // Refused for a new project or expense...
    let r = create_project(&w, "New Job", Some(&brand)).await;
    assert!(error_message(&r).contains("archived"), "{r:?}");
    let r = create_expense(&w, Some(&brand)).await;
    assert!(error_message(&r).contains("archived"), "{r:?}");
    // ...and an unknown id is refused outright.
    let r = create_project(&w, "New Job", Some("nope")).await;
    assert!(error_message(&r).contains("does not exist"), "{r:?}");

    // ...but rows already on it keep it through an edit.
    set_project_brand(&w, &project_id, "Old Job renamed", Some(&brand)).await;
    let r = w
        .run(
            "mutation($id: ID!, $input: ExpenseInput!) {
                updateExpense(id: $id, input: $input) { id brand { brandId archived } }
            }",
            json!({ "id": expense_id, "input": {
                "date": "2026-08-04", "category": "MATERIALS", "supplier": "Hardware Co",
                "amountCents": 22_000, "gstCents": 2_000, "brandId": brand,
            } }),
        )
        .await;
    assert_eq!(
        data(&r, "updateExpense.brand"),
        json!({ "brandId": brand, "archived": true })
    );
}

#[tokio::test]
async fn brand_filters_narrow_lists_and_reports() {
    let _ = require_local_db!();
    let Some(w) = setup("brandfilter").await else {
        return;
    };
    let brand = add_brand(&w, json!({ "name": "Positive Rate" })).await;
    let plain_project = project(&w, "Plain Job", None).await;
    let branded_project = project(&w, "Branded Job", Some(&brand)).await;

    let plain = draft(&w, &plain_project).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    finalize(&w, &plain).await;
    let branded = draft(&w, &branded_project).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    finalize(&w, &branded).await;
    // A branded draft is filtered by its project's brand.
    let branded_draft = draft(&w, &branded_project).await["id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = w
        .run(
            "mutation($id: ID!, $input: CreditNoteInput!) { issueCreditNote(invoiceId: $id, input: $input) { id } }",
            json!({ "id": branded, "input": { "issueDate": "2026-08-02", "reason": "Refund",
                "lines": [{ "description": "Part", "amountCents": 1_000 }] } }),
        )
        .await;
    let credit_note = data(&r, "issueCreditNote.id").as_str().unwrap().to_string();

    let plain_expense = data(&create_expense(&w, None).await, "createExpense.id")
        .as_str()
        .unwrap()
        .to_string();
    let branded_expense = data(&create_expense(&w, Some(&brand)).await, "createExpense.id")
        .as_str()
        .unwrap()
        .to_string();

    let invoices = "query($i: ID!, $b: ID) { invoices(instanceId: $i, brandId: $b) {
        edges { node { id } } } }";
    let credit_notes = "query($i: ID!, $b: ID) { creditNotes(instanceId: $i, brandId: $b) {
        edges { node { id } } } }";
    let expenses = "query($i: ID!, $b: ID) { expenses(instanceId: $i, brandId: $b) {
        edges { node { id } } } }";
    let gst = "query($i: ID!, $b: ID) { gstReport(instanceId: $i, from: \"2026-07-01\",
        to: \"2026-09-30\", basis: ACCRUAL, brandId: $b) {
        invoiceCount creditNoteCount expenseCount salesCents } }";
    let receivables =
        "query($i: ID!, $b: ID) { receivables(instanceId: $i, brandId: $b) { invoices { id } } }";

    let q =
        |query: &'static str, b: Option<&str>| w.run(query, json!({ "i": w.instance_id, "b": b }));

    // No filter: everything.
    assert_eq!(
        ids(&data(&q(invoices, None).await, "invoices")),
        sorted(vec![plain.clone(), branded.clone(), branded_draft.clone()])
    );
    // The default brand: rows with no brand.
    assert_eq!(
        ids(&data(&q(invoices, Some("default")).await, "invoices")),
        vec![plain.clone()]
    );
    assert_eq!(
        ids(&data(&q(invoices, Some(&brand)).await, "invoices")),
        sorted(vec![branded.clone(), branded_draft.clone()])
    );
    assert_eq!(
        ids(&data(&q(credit_notes, Some(&brand)).await, "creditNotes")),
        vec![credit_note.clone()]
    );
    assert!(
        ids(&data(
            &q(credit_notes, Some("default")).await,
            "creditNotes"
        ))
        .is_empty()
    );
    assert_eq!(
        ids(&data(&q(expenses, Some("default")).await, "expenses")),
        vec![plain_expense.clone()]
    );
    assert_eq!(
        ids(&data(&q(expenses, Some(&brand)).await, "expenses")),
        vec![branded_expense.clone()]
    );

    let report = data(&q(gst, Some(&brand)).await, "gstReport");
    assert_eq!(report["invoiceCount"], json!(1));
    assert_eq!(report["creditNoteCount"], json!(1));
    assert_eq!(report["expenseCount"], json!(1));
    let report = data(&q(gst, Some("default")).await, "gstReport");
    assert_eq!(report["invoiceCount"], json!(1));
    assert_eq!(report["creditNoteCount"], json!(0));
    assert_eq!(report["expenseCount"], json!(1));
    let all = data(&q(gst, None).await, "gstReport");
    assert_eq!(all["invoiceCount"], json!(2));

    let only = |v: Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        only(data(
            &q(receivables, Some(&brand)).await,
            "receivables.invoices"
        )),
        vec![branded.clone()]
    );
    assert_eq!(
        only(data(
            &q(receivables, Some("default")).await,
            "receivables.invoices"
        )),
        vec![plain.clone()]
    );

    // Moving the project to the default brand moves its draft in the
    // filter too (it shows the project's brand), never its finalized one.
    set_project_brand(&w, &branded_project, "Branded Job", None).await;
    assert_eq!(
        ids(&data(&q(invoices, Some("default")).await, "invoices")),
        sorted(vec![plain.clone(), branded_draft.clone()])
    );
    assert_eq!(
        ids(&data(&q(invoices, Some(&brand)).await, "invoices")),
        vec![branded.clone()]
    );
    assert_eq!(
        get_invoice(&w, &branded_draft).await["brand"]["isDefault"],
        json!(true)
    );

    // An id the instance doesn't have is refused, not an empty result.
    for query in [invoices, credit_notes, expenses, gst, receivables] {
        let r = q(query, Some("nope")).await;
        assert!(
            error_message(&r).contains("does not exist"),
            "{query}: {r:?}"
        );
    }
}
