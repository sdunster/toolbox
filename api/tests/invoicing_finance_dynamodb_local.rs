//! Integration tests for what happens to an invoice after it is issued —
//! due dates, payments, credit notes, emailing, and the reports built over
//! them — plus re-billing expenses, receipts, GST-free lines and project
//! default rates, against **DynamoDB Local**. Covers:
//!
//!   - a due date defaults to issue date + the project's (else the
//!     instance's) payment terms, can be overridden, and drives OVERDUE;
//!   - partial payments settle an invoice exactly when the balance reaches
//!     zero; deleting one un-settles it; overpaying is refused;
//!   - credit notes: numbered `CN-`, frozen, capped at what's left to
//!     credit, settle an invoice when they cover it, render a PDF, and are
//!     invisible to non-members;
//!   - `sendInvoice` mails the client with the PDF attached and records it;
//!   - `rebillExpense` makes a linked item once; deleting the item frees the
//!     expense; a re-billed expense can't be deleted;
//!   - expense receipts: upload key scoped to the expense, attach, download;
//!   - GST-free lines are left out of the GST; a project's default rate
//!     fills an omitted unit price;
//!   - `gstReport` (cash and accrual), `receivables`, `invoicingExport` and
//!     `Project.financials`.
//!
//! # Running this test
//!
//! ```sh
//! make local-up
//! make local-tables
//! cd api
//! set -a && . ../local/local.env && set +a
//! cargo test --test invoicing_finance_dynamodb_local
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
            "invoicing_finance_dynamodb_local: {endpoint} is configured but not reachable — \
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
                    "invoicing_finance_dynamodb_local: AWS_ENDPOINT_URL_DYNAMODB/DB_PREFIX not \
                     set to a reachable local DynamoDB — skipping. See this file's header."
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

fn member(user_id: &str, instance_id: &str) -> AuthInfo {
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

fn outsider(user_id: &str) -> AuthInfo {
    AuthInfo::User {
        id: user_id.to_string(),
        memberships: vec![],
        is_superuser: false,
        token_id: None,
        grant_id: None,
    }
}

struct World {
    s: TestSchema,
    instance_id: String,
    user_id: String,
    project_id: String,
}

impl World {
    fn auth(&self) -> AuthInfo {
        member(&self.user_id, &self.instance_id)
    }

    async fn run(&self, query: &str, vars: Value) -> Response {
        self.s.run(query, vars, self.auth()).await
    }

    fn db(&self) -> &dynamodb::Handler {
        &self.s.app.db
    }
}

/// A GST-registered invoicing instance with 30-day terms, one agent, and one
/// project with a client email and a default rate.
async fn setup(label: &str) -> Option<World> {
    let prefix = local_db_prefix().await?;
    let db = dynamodb::Handler::new(&prefix, false).await;
    let instance = db
        .create_instance(
            &unique(label),
            &unique(&format!("{label}-slug")),
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
            business_abn: None,
            business_address: None,
            business_phone: None,
            business_email: Some("accounts@fictional.example"),
            payment_details: Some("BSB 000-000 Acc 00000000"),
            gst_registered: true,
            currency: None,
            payment_terms_days: Some(30),
        },
    )
    .await
    .expect("settings");
    let user = db
        .create_user(&format!("{}@example.com", unique(label)), "Agent")
        .await
        .expect("create_user");
    db.create_membership(&user.id, &instance.id, db::MembershipRole::Agent)
        .await
        .expect("membership");
    let project = db
        .create_project(
            &instance.id,
            &db::ProjectFields {
                name: "Fictional Job".into(),
                client_name: "Fictional Client Pty Ltd".into(),
                client_email: Some("client@example.com".into()),
                default_unit_price_cents: Some(10_000),
                ..Default::default()
            },
        )
        .await
        .expect("create_project");
    let storage_dir = std::env::temp_dir().join(unique("finance-storage"));
    let my_app = Arc::new(app::new(
        db,
        mockmail::Handler::new(),
        mockstorage::Storage::with_dir(storage_dir),
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
        user_id: user.id,
        project_id: project.id,
    })
}

const INVOICE_FIELDS: &str = "id status displayNumber issueDate dueDate paidDate overdue \
     daysOverdue totalCents gstCents subtotalCents paidCents creditedCents balanceCents \
     sentAt sentTo payments { id date amountCents note } \
     lines { description amountCents gstFree } creditNotes { id displayNumber totalCents }";

/// Create an item on `project_id` (using the project's default rate when
/// `price` is `None`) and return its id.
async fn item(
    w: &World,
    project_id: &str,
    qty: &str,
    price: Option<i64>,
    gst_free: bool,
) -> String {
    let r = w
        .run(
            "mutation($p: ID!, $input: BillableItemInput!) {
                createBillableItem(projectId: $p, input: $input) { id unitPriceCents gstFree }
            }",
            json!({
                "p": project_id,
                "input": {
                    "date": "2026-08-01",
                    "description": "Work",
                    "quantity": qty,
                    "unitPriceCents": price,
                    "gstFree": gst_free,
                }
            }),
        )
        .await;
    data(&r, "createBillableItem.id")
        .as_str()
        .unwrap()
        .to_string()
}

/// Create a draft from `items` and finalize it on `issue` (with an
/// optional explicit due date), returning the invoice JSON.
async fn invoice(w: &World, items: &[String], issue: &str, due: Option<&str>) -> Value {
    let r = w
        .run(
            "mutation($p: ID!, $items: [ID!]!) { createInvoice(projectId: $p, itemIds: $items) { id } }",
            json!({ "p": w.project_id, "items": items }),
        )
        .await;
    let id = data(&r, "createInvoice.id");
    let r = w
        .run(
            &format!(
                "mutation($id: ID!, $issue: String!, $due: String) {{
                    finalizeInvoice(invoiceId: $id, issueDate: $issue, dueDate: $due) {{ {INVOICE_FIELDS} }}
                }}"
            ),
            json!({ "id": id, "issue": issue, "due": due }),
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

async fn pay(w: &World, id: &str, date: &str, cents: i64) -> Response {
    w.run(
        &format!(
            "mutation($id: ID!, $input: RecordPaymentInput!) {{
                recordInvoicePayment(invoiceId: $id, input: $input) {{ {INVOICE_FIELDS} }}
            }}"
        ),
        json!({ "id": id, "input": { "date": date, "amountCents": cents, "note": "EFT" } }),
    )
    .await
}

async fn credit(w: &World, id: &str, date: &str, lines: Option<Value>) -> Response {
    w.run(
        "mutation($id: ID!, $input: CreditNoteInput!) {
            issueCreditNote(invoiceId: $id, input: $input) {
                id displayNumber issueDate reason title totalCents gstCents subtotalCents
                invoice { id } lines { description amountCents }
            }
        }",
        json!({ "id": id, "input": { "issueDate": date, "reason": "Adjustment", "lines": lines } }),
    )
    .await
}

fn str_of(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn due_dates_follow_terms_and_drive_overdue() {
    let _ = require_local_db!();
    let Some(w) = setup("due").await else { return };

    // The project has no terms of its own: the instance's 30 days apply.
    let a = invoice(
        &w,
        &[item(&w, &w.project_id, "1", None, false).await],
        "2026-08-01",
        None,
    )
    .await;
    assert_eq!(str_of(&a, "dueDate"), "2026-08-31");

    // A project override wins.
    let r = w
        .run(
            "mutation($id: ID!, $input: UpdateProjectInput!) { updateProject(id: $id, input: $input) {
                paymentTermsDays effectivePaymentTermsDays clientEmail defaultUnitPriceCents } }",
            json!({ "id": w.project_id, "input": {
                "name": "Fictional Job", "clientName": "Fictional Client Pty Ltd",
                "clientEmail": "Client@Example.com", "paymentTermsDays": 7,
                "defaultUnitPriceCents": 10000, "archived": false } }),
        )
        .await;
    assert_eq!(
        data(&r, "updateProject.effectivePaymentTermsDays"),
        json!(7)
    );
    assert_eq!(
        data(&r, "updateProject.clientEmail"),
        json!("client@example.com")
    );
    let b = invoice(
        &w,
        &[item(&w, &w.project_id, "1", None, false).await],
        "2026-08-01",
        None,
    )
    .await;
    assert_eq!(str_of(&b, "dueDate"), "2026-08-08");

    // An explicit due date wins, but can't precede the issue date.
    let c = invoice(
        &w,
        &[item(&w, &w.project_id, "1", None, false).await],
        "2026-08-01",
        Some("2026-09-15"),
    )
    .await;
    assert_eq!(str_of(&c, "dueDate"), "2026-09-15");
    let draft = w
        .run(
            "mutation($p: ID!, $items: [ID!]!) { createInvoice(projectId: $p, itemIds: $items) { id } }",
            json!({ "p": w.project_id, "items": [item(&w, &w.project_id, "1", None, false).await] }),
        )
        .await;
    let draft_id = data(&draft, "createInvoice.id");
    let bad = w
        .run(
            "mutation($id: ID!) { finalizeInvoice(invoiceId: $id, issueDate: \"2026-08-10\", dueDate: \"2026-08-09\") { id } }",
            json!({ "id": draft_id }),
        )
        .await;
    assert!(error_message(&bad).contains("before the issue date"));

    // Long past due (by the real clock): overdue, and in the OVERDUE filter.
    assert_eq!(a["overdue"], json!(true));
    assert!(a["daysOverdue"].as_i64().unwrap() > 0);
    let r = w
        .run(
            "query($i: ID!) { invoices(instanceId: $i, filter: OVERDUE) { edges { node { id } } } }",
            json!({ "i": w.instance_id }),
        )
        .await;
    let ids: Vec<Value> = data(&r, "invoices.edges")
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["node"]["id"].clone())
        .collect();
    assert!(ids.contains(&a["id"]));

    // Paid in full: no longer overdue.
    let paid = pay(
        &w,
        a["id"].as_str().unwrap(),
        "2026-09-01",
        a["totalCents"].as_i64().unwrap(),
    )
    .await;
    let paid = data(&paid, "recordInvoicePayment");
    assert_eq!(paid["overdue"], json!(false));
    assert_eq!(str_of(&paid, "paidDate"), "2026-09-01");
}

#[tokio::test]
async fn partial_payments_settle_only_when_the_balance_is_zero() {
    let _ = require_local_db!();
    let Some(w) = setup("pay").await else { return };
    // 2 × 100.00 + 10% GST = 220.00
    let inv = invoice(
        &w,
        &[item(&w, &w.project_id, "2", None, false).await],
        "2026-08-01",
        None,
    )
    .await;
    let id = str_of(&inv, "id");
    assert_eq!(inv["totalCents"], json!(22_000));
    assert_eq!(inv["balanceCents"], json!(22_000));

    let r = pay(&w, &id, "2026-08-05", 10_000).await;
    let after_one = data(&r, "recordInvoicePayment");
    assert_eq!(after_one["balanceCents"], json!(12_000));
    assert_eq!(after_one["paidDate"], Value::Null);

    let too_much = pay(&w, &id, "2026-08-06", 12_001).await;
    assert!(error_message(&too_much).contains("more than"));

    let r = pay(&w, &id, "2026-08-20", 12_000).await;
    let settled = data(&r, "recordInvoicePayment");
    assert_eq!(settled["balanceCents"], json!(0));
    assert_eq!(str_of(&settled, "paidDate"), "2026-08-20");
    assert_eq!(settled["payments"].as_array().unwrap().len(), 2);

    let already = pay(&w, &id, "2026-08-21", 1).await;
    assert_eq!(error_code(&already), "CONFLICT");

    // Removing a payment un-settles it.
    let payment_id = settled["payments"][1]["id"].clone();
    let r = w
        .run(
            "mutation($id: ID!, $p: ID!) { deleteInvoicePayment(invoiceId: $id, paymentId: $p) { paidDate balanceCents } }",
            json!({ "id": id, "p": payment_id }),
        )
        .await;
    assert_eq!(data(&r, "deleteInvoicePayment.paidDate"), Value::Null);
    assert_eq!(data(&r, "deleteInvoicePayment.balanceCents"), json!(12_000));

    // setInvoicePaid pays the rest; null clears every payment.
    let r = w
        .run(
            "mutation($id: ID!) { setInvoicePaid(invoiceId: $id, paidDate: \"2026-08-30\") { paidDate paidCents } }",
            json!({ "id": id }),
        )
        .await;
    assert_eq!(data(&r, "setInvoicePaid.paidDate"), json!("2026-08-30"));
    assert_eq!(data(&r, "setInvoicePaid.paidCents"), json!(22_000));
    let r = w
        .run(
            "mutation($id: ID!) { setInvoicePaid(invoiceId: $id, paidDate: null) { paidDate paidCents } }",
            json!({ "id": id }),
        )
        .await;
    assert_eq!(data(&r, "setInvoicePaid.paidDate"), Value::Null);
    assert_eq!(data(&r, "setInvoicePaid.paidCents"), json!(0));
}

#[tokio::test]
async fn credit_notes_are_numbered_capped_and_settle_invoices() {
    let _ = require_local_db!();
    let Some(w) = setup("credit").await else {
        return;
    };
    let inv = invoice(
        &w,
        &[item(&w, &w.project_id, "2", None, false).await],
        "2026-08-01",
        None,
    )
    .await;
    let id = str_of(&inv, "id");

    // Can't predate the invoice.
    let early = credit(&w, &id, "2026-07-31", None).await;
    assert!(error_message(&early).contains("before the invoice"));

    // A partial credit: 50.00 + GST = 55.00.
    let r = credit(
        &w,
        &id,
        "2026-08-02",
        Some(json!([{ "description": "Discount", "amountCents": 5000 }])),
    )
    .await;
    let first = data(&r, "issueCreditNote");
    assert_eq!(first["displayNumber"], json!("CN-001"));
    assert_eq!(first["title"], json!("Adjustment Note"));
    assert_eq!(first["totalCents"], json!(5_500));
    assert_eq!(first["gstCents"], json!(500));

    // A full credit is refused once something has been credited.
    let full = credit(&w, &id, "2026-08-03", None).await;
    assert!(error_message(&full).contains("already has a credit note"));
    // More than is left is refused.
    let over = credit(
        &w,
        &id,
        "2026-08-03",
        Some(json!([{ "description": "Too much", "amountCents": 15001 }])),
    )
    .await;
    assert!(error_message(&over).contains("left to credit"));

    // The rest settles the invoice, dated by the credit note.
    let r = credit(
        &w,
        &id,
        "2026-08-04",
        Some(json!([{ "description": "Rest", "amountCents": 15000 }])),
    )
    .await;
    assert_eq!(data(&r, "issueCreditNote.displayNumber"), json!("CN-002"));
    let after = get_invoice(&w, &id).await;
    assert_eq!(after["creditedCents"], json!(22_000));
    assert_eq!(after["balanceCents"], json!(0));
    assert_eq!(str_of(&after, "paidDate"), "2026-08-04");
    assert_eq!(after["creditNotes"].as_array().unwrap().len(), 2);
    assert_eq!(after["creditNotes"][0]["displayNumber"], json!("CN-002"));

    // A full credit of a fresh invoice copies its lines.
    let inv2 = invoice(
        &w,
        &[item(&w, &w.project_id, "1", None, true).await],
        "2026-08-01",
        None,
    )
    .await;
    let r = credit(&w, &str_of(&inv2, "id"), "2026-08-05", None).await;
    let note = data(&r, "issueCreditNote");
    assert_eq!(note["totalCents"], inv2["totalCents"]);
    assert_eq!(note["gstCents"], json!(0)); // the line was GST-free

    // PDF, listing, and a non-member sees nothing.
    let note_id = note["id"].as_str().unwrap();
    let r = w
        .run(
            "mutation($id: ID!) { downloadCreditNotePdf(creditNoteId: $id) }",
            json!({ "id": note_id }),
        )
        .await;
    assert!(data(&r, "downloadCreditNotePdf").as_str().is_some());
    let r = w
        .run(
            "query($i: ID!) { creditNotes(instanceId: $i) { edges { node { displayNumber } } } }",
            json!({ "i": w.instance_id }),
        )
        .await;
    assert_eq!(data(&r, "creditNotes.edges").as_array().unwrap().len(), 3);
    let r =
        w.s.run(
            "query($id: ID!) { creditNote(id: $id) { id } }",
            json!({ "id": note_id }),
            outsider(&w.user_id),
        )
        .await;
    assert_eq!(data(&r, "creditNote"), Value::Null);
    let r = w
        .s
        .run(
            "mutation($id: ID!, $input: CreditNoteInput!) { issueCreditNote(invoiceId: $id, input: $input) { id } }",
            json!({ "id": str_of(&inv2, "id"), "input": { "issueDate": "2026-08-06", "reason": "x" } }),
            outsider(&w.user_id),
        )
        .await;
    assert_eq!(error_code(&r), "NOT_FOUND");
}

#[tokio::test]
async fn send_invoice_mails_the_client_with_the_pdf() {
    let _ = require_local_db!();
    let Some(w) = setup("send").await else { return };
    let inv = invoice(
        &w,
        &[item(&w, &w.project_id, "1", None, false).await],
        "2026-08-01",
        None,
    )
    .await;
    let id = str_of(&inv, "id");
    let r = w
        .run(
            "mutation($id: ID!, $input: SendDocumentInput) { sendInvoice(invoiceId: $id, input: $input) { sentAt sentTo } }",
            json!({ "id": id, "input": { "cc": ["boss@example.com"], "message": "Thanks!" } }),
        )
        .await;
    assert_eq!(
        data(&r, "sendInvoice.sentTo"),
        json!(["client@example.com", "boss@example.com"])
    );
    assert!(data(&r, "sendInvoice.sentAt").as_i64().is_some());
    let sent = w.s.app.mail.sent_raw();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, vec!["client@example.com".to_string()]);
    assert_eq!(sent[0].cc, vec!["boss@example.com".to_string()]);
    let parsed = mail_parser::MessageParser::default()
        .parse(&sent[0].raw)
        .expect("parses");
    assert_eq!(parsed.attachment_count(), 1);
    assert!(
        parsed
            .subject()
            .unwrap_or_default()
            .starts_with("Tax Invoice 0")
    );

    // A draft can't be sent; a project with no client email needs a `to`.
    let draft = w
        .run(
            "mutation($p: ID!, $items: [ID!]!) { createInvoice(projectId: $p, itemIds: $items) { id } }",
            json!({ "p": w.project_id, "items": [item(&w, &w.project_id, "1", None, false).await] }),
        )
        .await;
    let r = w
        .run(
            "mutation($id: ID!) { sendInvoice(invoiceId: $id) { id } }",
            json!({ "id": data(&draft, "createInvoice.id") }),
        )
        .await;
    assert_eq!(error_code(&r), "CONFLICT");
    let bare = w
        .db()
        .create_project(
            &w.instance_id,
            &db::ProjectFields {
                name: "No email".into(),
                client_name: "Client".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let bare_item = item(&w, &bare.id, "1", Some(100), false).await;
    let r = w
        .run(
            "mutation($p: ID!, $items: [ID!]!) { createInvoice(projectId: $p, itemIds: $items) { id } }",
            json!({ "p": bare.id, "items": [bare_item] }),
        )
        .await;
    let bare_id = data(&r, "createInvoice.id");
    w.run(
        "mutation($id: ID!) { finalizeInvoice(invoiceId: $id, issueDate: \"2026-08-01\") { id } }",
        json!({ "id": bare_id }),
    )
    .await;
    let r = w
        .run(
            "mutation($id: ID!) { sendInvoice(invoiceId: $id) { id } }",
            json!({ "id": bare_id }),
        )
        .await;
    assert!(error_message(&r).contains("no client email"));
}

async fn expense(w: &World, project_id: Option<&str>) -> String {
    let r = w
        .run(
            "mutation($i: ID!, $input: ExpenseInput!) { createExpense(instanceId: $i, input: $input) { id } }",
            json!({ "i": w.instance_id, "input": {
                "projectId": project_id, "date": "2026-08-03", "category": "MATERIALS",
                "supplier": "Hardware Co", "amountCents": 11000, "gstCents": 1000 } }),
        )
        .await;
    data(&r, "createExpense.id").as_str().unwrap().to_string()
}

#[tokio::test]
async fn rebilling_an_expense_links_one_item_at_a_time() {
    let _ = require_local_db!();
    let Some(w) = setup("rebill").await else {
        return;
    };
    let id = expense(&w, Some(&w.project_id)).await;
    let rebill = "mutation($id: ID!) { rebillExpense(expenseId: $id, markupPercent: \"10\") {
        id unitPriceCents description status sourceExpense { id } } }";
    let r = w.run(rebill, json!({ "id": id })).await;
    let created = data(&r, "rebillExpense");
    assert_eq!(created["unitPriceCents"], json!(11_000)); // 100.00 ex GST + 10%
    assert_eq!(
        created["description"],
        json!("Materials & supplies: Hardware Co")
    );
    assert_eq!(created["sourceExpense"]["id"], json!(id));

    let again = w.run(rebill, json!({ "id": id })).await;
    assert_eq!(error_code(&again), "CONFLICT");
    let del = w
        .run(
            "mutation($id: ID!) { deleteExpense(id: $id) }",
            json!({ "id": id }),
        )
        .await;
    assert_eq!(error_code(&del), "CONFLICT");

    let r = w
        .run(
            "query($i: ID!) { expenses(instanceId: $i) { edges { node { id rebilledItem { id } } } } }",
            json!({ "i": w.instance_id }),
        )
        .await;
    assert_eq!(
        data(&r, "expenses.edges")[0]["node"]["rebilledItem"]["id"],
        created["id"]
    );

    // Deleting the item frees the expense.
    let r = w
        .run(
            "mutation($id: ID!) { deleteBillableItem(id: $id) }",
            json!({ "id": created["id"] }),
        )
        .await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let r = w.run(rebill, json!({ "id": id })).await;
    assert!(r.errors.is_empty(), "{:?}", r.errors);

    // No project, no re-bill.
    let loose = expense(&w, None).await;
    let r = w.run(rebill, json!({ "id": loose })).await;
    assert!(error_message(&r).contains("on a project"));
}

#[tokio::test]
async fn receipts_attach_only_their_own_uploads() {
    let _ = require_local_db!();
    let Some(w) = setup("receipt").await else {
        return;
    };
    let id = expense(&w, None).await;
    let other = expense(&w, None).await;
    let r = w
        .run(
            "mutation($id: ID!) { createExpenseReceiptUpload(expenseId: $id, filename: \"receipt.pdf\", contentType: \"application/pdf\") { key uploadUrl } }",
            json!({ "id": id }),
        )
        .await;
    let key = data(&r, "createExpenseReceiptUpload.key")
        .as_str()
        .unwrap()
        .to_string();
    assert!(key.ends_with("/receipt.pdf"));

    // Not uploaded yet.
    let attach = "mutation($id: ID!, $key: String!) { attachExpenseReceipt(expenseId: $id, key: $key, contentType: \"application/pdf\") { receipt { filename size contentType } } }";
    let r = w.run(attach, json!({ "id": id, "key": key })).await;
    assert!(error_message(&r).contains("finished uploading"));

    w.s.app
        .storage
        .put_bytes(&key, b"%PDF-1.4 receipt", "application/pdf")
        .await
        .unwrap();
    // Someone else's expense can't claim it.
    let r = w.run(attach, json!({ "id": other, "key": key })).await;
    assert_eq!(error_code(&r), "FORBIDDEN");
    let r = w.run(attach, json!({ "id": id, "key": key })).await;
    assert_eq!(
        data(&r, "attachExpenseReceipt.receipt.filename"),
        json!("receipt.pdf")
    );
    assert_eq!(data(&r, "attachExpenseReceipt.receipt.size"), json!(16));

    let r = w
        .run(
            "mutation($id: ID!) { downloadExpenseReceipt(expenseId: $id) }",
            json!({ "id": id }),
        )
        .await;
    assert!(data(&r, "downloadExpenseReceipt").as_str().is_some());
    let r = w
        .run(
            "mutation($id: ID!) { removeExpenseReceipt(expenseId: $id) { receipt { filename } } }",
            json!({ "id": id }),
        )
        .await;
    assert_eq!(data(&r, "removeExpenseReceipt.receipt"), Value::Null);
}

#[tokio::test]
async fn gst_free_lines_and_default_rates() {
    let _ = require_local_db!();
    let Some(w) = setup("gstfree").await else {
        return;
    };
    let taxable = item(&w, &w.project_id, "1", None, false).await; // 100.00 default rate
    let free = item(&w, &w.project_id, "1", Some(5_000), true).await;
    let inv = invoice(&w, &[taxable, free], "2026-08-01", None).await;
    assert_eq!(inv["subtotalCents"], json!(15_000));
    assert_eq!(inv["gstCents"], json!(1_000));
    assert_eq!(inv["totalCents"], json!(16_000));
    let flags: Vec<bool> = inv["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["gstFree"].as_bool().unwrap())
        .collect();
    assert_eq!(flags.iter().filter(|f| **f).count(), 1);

    // No default rate and no price: refused.
    let bare = w
        .db()
        .create_project(
            &w.instance_id,
            &db::ProjectFields {
                name: "No rate".into(),
                client_name: "Client".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let r = w
        .run(
            "mutation($p: ID!) { createBillableItem(projectId: $p, input: { date: \"2026-08-01\", description: \"x\", quantity: \"1\" }) { id } }",
            json!({ "p": bare.id }),
        )
        .await;
    assert!(error_message(&r).contains("no default rate"));
}

#[tokio::test]
async fn reports_and_exports() {
    let _ = require_local_db!();
    let Some(w) = setup("reports").await else {
        return;
    };
    // Invoice A: 220.00 incl 20.00 GST, issued 2026-07-10, half paid in July
    // and half in October.
    let a = invoice(
        &w,
        &[item(&w, &w.project_id, "2", None, false).await],
        "2026-07-10",
        None,
    )
    .await;
    let a_id = str_of(&a, "id");
    data(
        &pay(&w, &a_id, "2026-07-20", 11_000).await,
        "recordInvoicePayment",
    );
    data(
        &pay(&w, &a_id, "2026-10-02", 11_000).await,
        "recordInvoicePayment",
    );
    // Invoice B: 110.00, issued 2026-08-01, credited 55.00 on 2026-08-15, unpaid.
    let b = invoice(
        &w,
        &[item(&w, &w.project_id, "1", None, false).await],
        "2026-08-01",
        None,
    )
    .await;
    let b_id = str_of(&b, "id");
    data(
        &credit(
            &w,
            &b_id,
            "2026-08-15",
            Some(json!([{ "description": "Discount", "amountCents": 5000 }])),
        )
        .await,
        "issueCreditNote",
    );
    // An expense on the project: 110.00 incl 10.00 GST.
    expense(&w, Some(&w.project_id)).await;

    let gst = "query($i: ID!, $basis: ReportBasisType!) { gstReport(instanceId: $i, from: \"2026-07-01\", to: \"2026-09-30\", basis: $basis) {
        salesCents gstOnSalesCents purchasesCents gstOnPurchasesCents netGstCents invoiceCount creditNoteCount paymentCount expenseCount } }";
    let r = w
        .run(gst, json!({ "i": w.instance_id, "basis": "ACCRUAL" }))
        .await;
    let accrual = data(&r, "gstReport");
    assert_eq!(accrual["salesCents"], json!(22_000 + 11_000 - 5_500));
    assert_eq!(accrual["gstOnSalesCents"], json!(2_000 + 1_000 - 500));
    assert_eq!(accrual["purchasesCents"], json!(11_000));
    assert_eq!(accrual["gstOnPurchasesCents"], json!(1_000));
    assert_eq!(accrual["netGstCents"], json!(1_500));
    assert_eq!(accrual["creditNoteCount"], json!(1));

    let r = w
        .run(gst, json!({ "i": w.instance_id, "basis": "CASH" }))
        .await;
    let cash = data(&r, "gstReport");
    assert_eq!(cash["salesCents"], json!(11_000));
    assert_eq!(cash["gstOnSalesCents"], json!(1_000));
    assert_eq!(cash["paymentCount"], json!(1));

    let r = w
        .run(
            "query($i: ID!) { receivables(instanceId: $i) { totalCents invoices { id balanceCents } } }",
            json!({ "i": w.instance_id }),
        )
        .await;
    assert_eq!(data(&r, "receivables.totalCents"), json!(5_500));
    assert_eq!(data(&r, "receivables.invoices")[0]["id"], json!(b_id));

    let export = "query($i: ID!, $kind: CsvExportType!) { invoicingExport(instanceId: $i, kind: $kind, from: \"2026-07-01\", to: \"2026-12-31\") }";
    for (kind, rows) in [
        ("INVOICES", 2),
        ("PAYMENTS", 2),
        ("CREDIT_NOTES", 1),
        ("EXPENSES", 1),
    ] {
        let r = w
            .run(export, json!({ "i": w.instance_id, "kind": kind }))
            .await;
        let csv = data(&r, "invoicingExport").as_str().unwrap().to_string();
        assert_eq!(
            csv.trim_end().split("\r\n").count(),
            rows + 1,
            "{kind}: {csv}"
        );
    }

    let r = w
        .run(
            "query($id: ID!) { project(id: $id) { financials { invoicedCents paidCents outstandingCents expensesCents profitCents unbilledCents } } }",
            json!({ "id": w.project_id }),
        )
        .await;
    let f = data(&r, "project.financials");
    assert_eq!(f["invoicedCents"], json!(20_000 + 10_000 - 5_000));
    assert_eq!(f["paidCents"], json!(22_000));
    assert_eq!(f["outstandingCents"], json!(5_500));
    assert_eq!(f["expensesCents"], json!(10_000));
    assert_eq!(f["profitCents"], json!(15_000));
    assert_eq!(f["unbilledCents"], json!(0));

    // A range longer than two years is refused.
    let r = w
        .run(
            "query($i: ID!) { invoicingExport(instanceId: $i, kind: INVOICES, from: \"2020-01-01\", to: \"2026-01-01\") }",
            json!({ "i": w.instance_id }),
        )
        .await;
    assert!(error_message(&r).contains("two years"));
}
