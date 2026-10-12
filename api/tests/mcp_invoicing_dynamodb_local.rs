//! Integration tests for the MCP invoicing tools against **DynamoDB Local**,
//! through the real schema and `mcp::handle_post` with real `mtoa_` tokens:
//!
//!   - the whole lifecycle: project -> billable items -> draft invoice -> finalize
//!     (needs a business name; strictly final afterwards) -> paid -> PDF link;
//!   - draft editing (add/remove/delete) and `update_project`'s merge semantics;
//!   - expenses: a purchase and a cents-per-km trip, update/delete, and the
//!     caller's financial-year km summary;
//!   - the boundaries: a superuser without a membership sees nothing (and
//!     someone else's invoice looks exactly like a missing one), and a support
//!     instance is refused by the resolver's own kind check.
//!
//! # Running this test
//!
//! ```sh
//! make local-up && make local-tables
//! cd api && set -a && . ../local/local.env && set +a
//! cargo test --test mcp_invoicing_dynamodb_local
//! ```
//!
//! Every test **skips itself** when no reachable local DynamoDB is configured.

mod common;

use common::mcp::*;
use serde_json::{Value, json};
use toolbox::db;
use toolbox::db::Handler as _;

struct World {
    f: Fixture,
    instance: String,
    member_token: String,
    outsider_token: String,
    support_token: String,
    support_instance: String,
}

async fn world() -> Option<World> {
    let prefix = local_db_prefix().await?;
    let f = fixture(&prefix).await;
    let (instance, _) = make_instance(&f, "mcp-inv", db::InstanceKind::Invoicing).await;
    let (support_instance, _) =
        make_instance(&f, "mcp-inv-support", db::InstanceKind::Support).await;
    let member = make_user(&f, "mcp-inv-member", false).await;
    let outsider = make_user(&f, "mcp-inv-outsider", true).await;
    add_member(&f, &member, &instance, false).await;
    add_member(&f, &member, &support_instance, false).await;
    Some(World {
        member_token: access_token_for(&f, &member).await,
        outsider_token: access_token_for(&f, &outsider).await,
        support_token: access_token_for(&f, &member).await,
        f,
        instance,
        support_instance,
    })
}

async fn set_business_name(w: &World) {
    w.f.db()
        .update_instance(
            &w.instance,
            db::InstanceUpdateShape::SetInvoicingSettings {
                business_name: Some("Fictional Trading Co"),
                business_abn: None,
                business_address: None,
                business_phone: None,
                business_email: None,
                payment_details: Some("BSB 000-000 Acct 12345678"),
                gst_registered: true,
                currency: None,
                payment_terms_days: None,
            },
        )
        .await
        .unwrap();
}

async fn project(w: &World, name: &str) -> String {
    let p = call_tool(
        &w.f,
        &w.member_token,
        "create_project",
        json!({"instanceId": w.instance, "name": name, "clientName": "Acme Pty Ltd",
               "clientAbn": "12 345 678 901", "reference": "PO-1"}),
    )
    .await
    .unwrap();
    p["project"]["id"].as_str().unwrap().to_string()
}

async fn item(w: &World, project_id: &str, quantity: Value, price: i64) -> Value {
    call_tool(
        &w.f,
        &w.member_token,
        "create_billable_item",
        json!({"projectId": project_id, "date": "2026-09-01", "description": "Consulting",
               "quantity": quantity, "unitPriceCents": price}),
    )
    .await
    .unwrap()["item"]
        .clone()
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn full_lifecycle_from_project_to_pdf() {
    let Some(w) = world().await else { return };
    let t = &w.member_token;
    let pid = project(&w, "Website").await;

    // Money: quantity is a string (a JSON number is accepted too); amount is server-computed.
    let a = item(&w, &pid, json!("1.5"), 10_000).await;
    assert_eq!(a["amountCents"], 15_000);
    assert_eq!(a["status"], "UNBILLED");
    let b = item(&w, &pid, json!(2), 5_050).await;
    assert_eq!(b["quantity"], "2");
    assert_eq!(b["amountCents"], 10_100);

    let unbilled = call_tool(
        &w.f,
        t,
        "list_billable_items",
        json!({"instanceId": w.instance, "projectId": pid, "filter": "UNBILLED"}),
    )
    .await
    .unwrap();
    assert_eq!(unbilled["items"].as_array().unwrap().len(), 2);

    // Draft: no number yet, items become DRAFT.
    let inv = call_tool(
        &w.f,
        t,
        "create_invoice",
        json!({"projectId": pid, "itemIds": [id(&a), id(&b)]}),
    )
    .await
    .unwrap()["invoice"]
        .clone();
    assert_eq!(inv["status"], "DRAFT");
    assert!(inv["number"].is_null() && inv["displayNumber"].is_null());
    let inv_id = id(&inv);
    assert!(
        call_tool(&w.f, t, "get_invoice_pdf_url", json!({"invoiceId": inv_id}))
            .await
            .is_err(),
        "no PDF for a draft"
    );
    let billed = call_tool(
        &w.f,
        t,
        "list_billable_items",
        json!({"instanceId": w.instance, "filter": "BILLED"}),
    )
    .await
    .unwrap();
    assert!(
        billed["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["status"] == "DRAFT")
    );

    // A draft's item can still be edited, and the draft follows.
    let edited = call_tool(
        &w.f,
        t,
        "update_billable_item",
        json!({"id": id(&a), "date": "2026-09-01", "description": "Consulting (revised)", "quantity": "2", "unitPriceCents": 10_000}),
    )
    .await
    .unwrap();
    assert_eq!(edited["item"]["amountCents"], 20_000);

    // Finalizing needs a business name...
    let no_name = call_tool(
        &w.f,
        t,
        "finalize_invoice",
        json!({"invoiceId": inv_id, "issueDate": "2026-09-29"}),
    )
    .await
    .unwrap_err();
    assert!(no_name.to_lowercase().contains("settings"), "{no_name}");
    set_business_name(&w).await;

    // ...and is then final: numbered, frozen, locked.
    let fin = call_tool(
        &w.f,
        t,
        "finalize_invoice",
        json!({"invoiceId": inv_id, "issueDate": "2026-09-29"}),
    )
    .await
    .unwrap()["invoice"]
        .clone();
    assert_eq!(fin["status"], "FINALIZED");
    assert_eq!(fin["issueDate"], "2026-09-29");
    assert!(fin["displayNumber"].as_str().unwrap().len() >= 3);
    assert_eq!(fin["seller"]["name"], "Fictional Trading Co");
    assert_eq!(fin["billTo"]["name"], "Acme Pty Ltd");
    assert_eq!(
        fin["totalCents"].as_i64().unwrap(),
        fin["subtotalCents"].as_i64().unwrap() + fin["gstCents"].as_i64().unwrap()
    );

    assert!(
        call_tool(
            &w.f,
            t,
            "finalize_invoice",
            json!({"invoiceId": inv_id, "issueDate": "2026-09-30"})
        )
        .await
        .is_err()
    );
    assert!(
        call_tool(&w.f, t, "update_billable_item",
            json!({"id": id(&a), "date": "2026-09-01", "description": "x", "quantity": "1", "unitPriceCents": 1}))
            .await
            .is_err(),
        "items on a finalized invoice are locked"
    );
    assert!(
        call_tool(&w.f, t, "delete_billable_item", json!({"id": id(&a)}))
            .await
            .is_err()
    );
    assert!(
        call_tool(&w.f, t, "delete_invoice", json!({"invoiceId": inv_id}))
            .await
            .is_err()
    );
    assert!(
        call_tool(
            &w.f,
            t,
            "remove_invoice_items",
            json!({"invoiceId": inv_id, "itemIds": [id(&a)]})
        )
        .await
        .is_err()
    );

    // Paid, then unpaid again.
    let paid = call_tool(
        &w.f,
        t,
        "set_invoice_paid",
        json!({"invoiceId": inv_id, "paidDate": "2026-10-05"}),
    )
    .await
    .unwrap();
    assert_eq!(paid["invoice"]["paidDate"], "2026-10-05");
    let unpaid = call_tool(
        &w.f,
        t,
        "list_invoices",
        json!({"instanceId": w.instance, "filter": "UNPAID"}),
    )
    .await
    .unwrap();
    assert!(
        unpaid["invoices"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["id"] != json!(inv_id))
    );
    let cleared = call_tool(&w.f, t, "set_invoice_paid", json!({"invoiceId": inv_id}))
        .await
        .unwrap();
    assert!(cleared["invoice"]["paidDate"].is_null());

    // The link, and the read-back.
    let pdf = call_tool(&w.f, t, "get_invoice_pdf_url", json!({"invoiceId": inv_id}))
        .await
        .unwrap();
    assert!(!pdf["url"].as_str().unwrap().is_empty());
    let got = call_tool(&w.f, t, "get_invoice", json!({"invoiceId": inv_id}))
        .await
        .unwrap();
    assert_eq!(got["invoice"]["lines"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn draft_invoices_can_be_reshaped_and_deleted() {
    let Some(w) = world().await else { return };
    let t = &w.member_token;
    let pid = project(&w, "Draft work").await;
    let (a, b) = (
        item(&w, &pid, json!("1"), 1_000).await,
        item(&w, &pid, json!("1"), 2_000).await,
    );

    let inv = call_tool(
        &w.f,
        t,
        "create_invoice",
        json!({"projectId": pid, "itemIds": [id(&a)]}),
    )
    .await
    .unwrap()["invoice"]
        .clone();
    let inv_id = id(&inv);
    let added = call_tool(
        &w.f,
        t,
        "add_invoice_items",
        json!({"invoiceId": inv_id, "itemIds": [id(&b)]}),
    )
    .await
    .unwrap();
    assert_eq!(added["invoice"]["items"].as_array().unwrap().len(), 2);
    let removed = call_tool(
        &w.f,
        t,
        "remove_invoice_items",
        json!({"invoiceId": inv_id, "itemIds": [id(&a)]}),
    )
    .await
    .unwrap();
    assert_eq!(removed["invoice"]["items"].as_array().unwrap().len(), 1);

    // An empty draft exists but can't be finalized.
    call_tool(
        &w.f,
        t,
        "remove_invoice_items",
        json!({"invoiceId": inv_id, "itemIds": [id(&b)]}),
    )
    .await
    .unwrap();
    set_business_name(&w).await;
    assert!(
        call_tool(
            &w.f,
            t,
            "finalize_invoice",
            json!({"invoiceId": inv_id, "issueDate": "2026-09-29"})
        )
        .await
        .is_err()
    );

    // Deleting the draft frees its items again.
    assert_eq!(
        call_tool(&w.f, t, "delete_invoice", json!({"invoiceId": inv_id}))
            .await
            .unwrap()["deletedId"],
        json!(inv_id)
    );
    let unbilled = call_tool(
        &w.f,
        t,
        "list_billable_items",
        json!({"instanceId": w.instance, "projectId": pid, "filter": "UNBILLED"}),
    )
    .await
    .unwrap();
    assert_eq!(unbilled["items"].as_array().unwrap().len(), 2);
    // ...and an unbilled item can be deleted.
    call_tool(&w.f, t, "delete_billable_item", json!({"id": id(&a)}))
        .await
        .unwrap();
}

#[tokio::test]
async fn update_project_merges_and_archiving_blocks_new_items() {
    let Some(w) = world().await else { return };
    let t = &w.member_token;
    let pid = project(&w, "Merge me").await;

    // Only `reference` changes; everything else keeps its value.
    let p = call_tool(
        &w.f,
        t,
        "update_project",
        json!({"id": pid, "reference": "PO-2"}),
    )
    .await
    .unwrap();
    assert_eq!(p["project"]["reference"], "PO-2");
    assert_eq!(p["project"]["clientName"], "Acme Pty Ltd");
    assert_eq!(p["project"]["clientAbn"], "12 345 678 901");
    assert_eq!(p["project"]["archived"], false);

    // An empty string clears an optional field.
    let cleared = call_tool(
        &w.f,
        t,
        "update_project",
        json!({"id": pid, "clientAbn": ""}),
    )
    .await
    .unwrap();
    assert!(cleared["project"]["clientAbn"].is_null());
    assert_eq!(cleared["project"]["reference"], "PO-2");

    // Archived: hidden by default, and takes no new items.
    call_tool(
        &w.f,
        t,
        "update_project",
        json!({"id": pid, "archived": true}),
    )
    .await
    .unwrap();
    let visible = call_tool(&w.f, t, "list_projects", json!({"instanceId": w.instance}))
        .await
        .unwrap();
    assert!(
        visible["projects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["id"] != json!(pid))
    );
    let all = call_tool(
        &w.f,
        t,
        "list_projects",
        json!({"instanceId": w.instance, "includeArchived": true}),
    )
    .await
    .unwrap();
    assert!(
        all["projects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == json!(pid))
    );
    assert!(
        call_tool(&w.f, t, "create_billable_item",
            json!({"projectId": pid, "date": "2026-09-01", "description": "x", "quantity": "1", "unitPriceCents": 1}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn boundaries_match_the_site() {
    let Some(w) = world().await else { return };
    let pid = project(&w, "Private").await;
    let it = item(&w, &pid, json!("1"), 1_000).await;
    let inv = call_tool(
        &w.f,
        &w.member_token,
        "create_invoice",
        json!({"projectId": pid, "itemIds": [id(&it)]}),
    )
    .await
    .unwrap()["invoice"]
        .clone();

    // A superuser without a membership gets no invoicing access...
    let o = &w.outsider_token;
    assert!(
        call_tool(&w.f, o, "list_projects", json!({"instanceId": w.instance}))
            .await
            .is_err()
    );
    assert!(
        call_tool(&w.f, o, "list_invoices", json!({"instanceId": w.instance}))
            .await
            .is_err()
    );
    assert!(
        call_tool(
            &w.f,
            o,
            "create_project",
            json!({"instanceId": w.instance, "name": "x", "clientName": "y"})
        )
        .await
        .is_err()
    );
    // ...and someone else's invoice or project reads exactly like a missing one.
    let hidden = call_tool(&w.f, o, "get_invoice", json!({"invoiceId": id(&inv)}))
        .await
        .unwrap_err();
    let missing = call_tool(
        &w.f,
        o,
        "get_invoice",
        json!({"invoiceId": "no-such-invoice"}),
    )
    .await
    .unwrap_err();
    assert_eq!(hidden, missing);
    let hidden = call_tool(
        &w.f,
        o,
        "update_project",
        json!({"id": pid, "name": "hijack"}),
    )
    .await
    .unwrap_err();
    let missing = call_tool(
        &w.f,
        o,
        "update_project",
        json!({"id": "no-such-project", "name": "hijack"}),
    )
    .await
    .unwrap_err();
    assert_eq!(hidden, missing);
    assert!(
        call_tool(
            &w.f,
            o,
            "finalize_invoice",
            json!({"invoiceId": id(&inv), "issueDate": "2026-09-29"})
        )
        .await
        .is_err()
    );

    // A member of a *support* instance can't use invoicing tools on it.
    assert!(
        call_tool(
            &w.f,
            &w.support_token,
            "list_projects",
            json!({"instanceId": w.support_instance})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn invoicing_tools_are_listed_and_finalize_warns() {
    let Some(w) = world().await else { return };
    let list = rpc(&w.f, &w.member_token, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    for expected in [
        "list_projects",
        "create_project",
        "update_project",
        "list_billable_items",
        "create_billable_item",
        "update_billable_item",
        "delete_billable_item",
        "list_invoices",
        "get_invoice",
        "create_invoice",
        "add_invoice_items",
        "remove_invoice_items",
        "delete_invoice",
        "finalize_invoice",
        "set_invoice_paid",
        "get_invoice_pdf_url",
        "list_expenses",
        "create_expense",
        "update_expense",
        "delete_expense",
        "get_vehicle_km_summary",
    ] {
        assert!(tools.iter().any(|t| t["name"] == expected), "{expected}");
    }
    let finalize = tools
        .iter()
        .find(|t| t["name"] == "finalize_invoice")
        .unwrap();
    assert!(
        finalize["description"]
            .as_str()
            .unwrap()
            .contains("IRREVERSIBLE")
    );
    assert_eq!(finalize["annotations"]["destructiveHint"], true);
    assert!(finalize["inputSchema"]["properties"]["number"].is_object());
    // Owner-level settings are deliberately not tools.
    assert!(
        !tools
            .iter()
            .any(|t| t["name"].as_str().unwrap().contains("settings"))
    );
    assert!(
        call_tool(
            &w.f,
            &w.member_token,
            "finalize_invoice",
            json!({"invoiceId": "x"})
        )
        .await
        .is_err(),
        "issueDate is required"
    );
}

#[tokio::test]
async fn expenses_and_vehicle_km() {
    let Some(w) = world().await else { return };
    let t = &w.member_token;
    let project_id = project(&w, "Expenses job").await;

    let bought = call_tool(
        &w.f,
        t,
        "create_expense",
        json!({"instanceId": w.instance, "projectId": project_id, "date": "2026-09-02",
               "category": "MATERIALS", "supplier": "Fictional Hardware",
               "amountCents": 5_500, "gstCents": 500}),
    )
    .await
    .unwrap()["expense"]
        .clone();
    assert_eq!(bought["amountCents"], 5_500);
    assert_eq!(bought["project"]["id"], json!(project_id));

    // A JSON number distance is accepted; 10 km on 2 Sep 2026 at 91c.
    let trip = call_tool(
        &w.f,
        t,
        "create_expense",
        json!({"instanceId": w.instance, "date": "2026-09-03", "category": "VEHICLE_KM",
               "description": "Client meeting", "distanceKm": 10}),
    )
    .await
    .unwrap()["expense"]
        .clone();
    assert_eq!(trip["distanceKm"], "10");
    assert_eq!(trip["rateCentsPerKm"], 91);
    assert_eq!(trip["amountCents"], 910);

    let refused = call_tool(
        &w.f,
        t,
        "create_expense",
        json!({"instanceId": w.instance, "date": "2026-09-03", "category": "VEHICLE_KM",
               "description": "Fuel", "distanceKm": "5", "supplier": "Fictional Fuel"}),
    )
    .await;
    assert!(refused.is_err(), "{refused:?}");

    let updated = call_tool(
        &w.f,
        t,
        "update_expense",
        json!({"id": id(&trip), "date": "2026-09-03", "category": "VEHICLE_KM",
               "description": "Client meeting", "distanceKm": "25.5"}),
    )
    .await
    .unwrap();
    assert_eq!(updated["expense"]["distanceKm"], "25.5");

    let summary = call_tool(
        &w.f,
        t,
        "get_vehicle_km_summary",
        json!({"instanceId": w.instance, "financialYear": 2026}),
    )
    .await
    .unwrap();
    assert_eq!(summary["summary"]["totalKm"], "25.5");
    assert_eq!(summary["summary"]["capKm"], 5000);

    let trips = call_tool(
        &w.f,
        t,
        "list_expenses",
        json!({"instanceId": w.instance, "category": "VEHICLE_KM"}),
    )
    .await
    .unwrap();
    let trips = trips["expenses"].as_array().unwrap();
    assert_eq!(trips.len(), 1);
    assert_eq!(trips[0]["id"], json!(id(&trip)));

    let deleted = call_tool(&w.f, t, "delete_expense", json!({"id": id(&bought)}))
        .await
        .unwrap();
    assert_eq!(deleted["deletedId"], json!(id(&bought)));

    // A superuser without a membership sees nothing; a support instance is refused.
    assert!(
        call_tool(
            &w.f,
            &w.outsider_token,
            "list_expenses",
            json!({"instanceId": w.instance}),
        )
        .await
        .is_err()
    );
    assert!(
        call_tool(
            &w.f,
            &w.support_token,
            "list_expenses",
            json!({"instanceId": w.support_instance}),
        )
        .await
        .is_err()
    );
}

/// Importing an existing invoice exactly as issued: a backdated item, the
/// original number and issue date (owner-only), and a backdated paid date.
#[tokio::test]
async fn owner_can_import_an_invoice_with_its_original_number_and_dates() {
    let Some(w) = world().await else { return };
    set_business_name(&w).await;
    let owner = make_user(&w.f, "mcp-inv-owner", false).await;
    add_member(&w.f, &owner, &w.instance, true).await;
    let owner_token = access_token_for(&w.f, &owner).await;

    let pid = project(&w, "Imported").await;
    let it = call_tool(
        &w.f,
        &owner_token,
        "create_billable_item",
        json!({"projectId": pid, "date": "2019-02-11", "description": "Old work",
               "quantity": "3", "unitPriceCents": 5_000}),
    )
    .await
    .unwrap()["item"]
        .clone();
    let inv = call_tool(
        &w.f,
        &owner_token,
        "create_invoice",
        json!({"projectId": pid, "itemIds": [id(&it)]}),
    )
    .await
    .unwrap()["invoice"]
        .clone();

    // A plain member can't choose a number.
    let denied = call_tool(
        &w.f,
        &w.member_token,
        "finalize_invoice",
        json!({"invoiceId": id(&inv), "issueDate": "2019-02-28", "number": 1017}),
    )
    .await
    .unwrap_err();
    assert!(denied.contains("owner"), "{denied}");

    let fin = call_tool(
        &w.f,
        &owner_token,
        "finalize_invoice",
        json!({"invoiceId": id(&inv), "issueDate": "2019-02-28", "number": 1017}),
    )
    .await
    .unwrap()["invoice"]
        .clone();
    assert_eq!(fin["number"], 1017);
    assert_eq!(fin["displayNumber"], "1017");
    assert_eq!(fin["issueDate"], "2019-02-28");
    assert_eq!(fin["lines"][0]["date"], "2019-02-11");

    // A lower number is fine while unused; a used one is refused.
    let older = call_tool(
        &w.f,
        &owner_token,
        "create_billable_item",
        json!({"projectId": pid, "date": "2018-06-01", "description": "Older work",
               "quantity": "1", "unitPriceCents": 9_900}),
    )
    .await
    .unwrap()["item"]
        .clone();
    let older_inv = call_tool(
        &w.f,
        &owner_token,
        "create_invoice",
        json!({"projectId": pid, "itemIds": [id(&older)]}),
    )
    .await
    .unwrap()["invoice"]
        .clone();
    let reused = call_tool(
        &w.f,
        &owner_token,
        "finalize_invoice",
        json!({"invoiceId": id(&older_inv), "issueDate": "2018-06-30", "number": 1017}),
    )
    .await
    .unwrap_err();
    assert!(reused.contains("already used"), "{reused}");
    let older_fin = call_tool(
        &w.f,
        &owner_token,
        "finalize_invoice",
        json!({"invoiceId": id(&older_inv), "issueDate": "2018-06-30", "number": 998}),
    )
    .await
    .unwrap()["invoice"]
        .clone();
    assert_eq!(older_fin["displayNumber"], "998");

    let paid = call_tool(
        &w.f,
        &owner_token,
        "set_invoice_paid",
        json!({"invoiceId": id(&inv), "paidDate": "2019-03-15"}),
    )
    .await
    .unwrap();
    assert_eq!(paid["invoice"]["paidDate"], "2019-03-15");

    assert!(
        call_tool(
            &w.f,
            &owner_token,
            "finalize_invoice",
            json!({"invoiceId": "x", "issueDate": "2019-02-28", "number": "seven"}),
        )
        .await
        .unwrap_err()
        .contains("whole number")
    );
}

#[tokio::test]
async fn payments_credit_notes_sending_and_reports() {
    let Some(w) = world().await else { return };
    let t = &w.member_token;
    set_business_name(&w).await;
    let pid = call_tool(
        &w.f,
        t,
        "create_project",
        json!({"instanceId": w.instance, "name": "Fitout", "clientName": "Acme Pty Ltd",
               "clientEmail": "accounts@acme.example", "paymentTermsDays": 7,
               "defaultUnitPriceCents": 10_000}),
    )
    .await
    .unwrap()["project"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    // No price: the project's default rate applies.
    let it = call_tool(
        &w.f,
        t,
        "create_billable_item",
        json!({"projectId": pid, "date": "2026-09-01", "description": "Labour", "quantity": "2"}),
    )
    .await
    .unwrap()["item"]
        .clone();
    assert_eq!(it["unitPriceCents"], 10_000);
    let inv = call_tool(
        &w.f,
        t,
        "create_invoice",
        json!({"projectId": pid, "itemIds": [id(&it)]}),
    )
    .await
    .unwrap()["invoice"]
        .clone();
    let fin = call_tool(
        &w.f,
        t,
        "finalize_invoice",
        json!({"invoiceId": id(&inv), "issueDate": "2026-09-01"}),
    )
    .await
    .unwrap()["invoice"]
        .clone();
    assert_eq!(fin["dueDate"], "2026-09-08");
    assert_eq!(fin["totalCents"], 22_000); // GST-registered via set_business_name

    let paid = call_tool(
        &w.f,
        t,
        "record_invoice_payment",
        json!({"invoiceId": id(&fin), "date": "2026-09-05", "amountCents": 11_000}),
    )
    .await
    .unwrap()["invoice"]
        .clone();
    assert_eq!(paid["balanceCents"], 11_000);
    assert!(paid["paidDate"].is_null());

    let note = call_tool(
        &w.f,
        t,
        "issue_credit_note",
        json!({"invoiceId": id(&fin), "issueDate": "2026-09-06", "reason": "Discount",
               "lines": [{"description": "Discount", "amountCents": 10_000}]}),
    )
    .await
    .unwrap()["creditNote"]
        .clone();
    assert_eq!(note["totalCents"], 11_000);
    assert!(note["displayNumber"].as_str().unwrap().starts_with("CN-"));
    let after = call_tool(&w.f, t, "get_invoice", json!({"invoiceId": id(&fin)}))
        .await
        .unwrap()["invoice"]
        .clone();
    assert_eq!(after["balanceCents"], 0);
    assert_eq!(after["paidDate"], "2026-09-06");

    let sent = call_tool(&w.f, t, "send_invoice", json!({"invoiceId": id(&fin)}))
        .await
        .unwrap()["invoice"]
        .clone();
    assert!(sent["sentAt"].is_i64());
    assert_eq!(
        w.f.app.mail.sent_raw().last().unwrap().to,
        vec!["accounts@acme.example"]
    );

    let report = call_tool(
        &w.f,
        t,
        "get_gst_report",
        json!({"instanceId": w.instance, "from": "2026-07-01", "to": "2026-09-30", "basis": "ACCRUAL"}),
    )
    .await
    .unwrap()["report"]
        .clone();
    assert_eq!(report["salesCents"], 11_000);
    let csv = call_tool(
        &w.f,
        t,
        "export_csv",
        json!({"instanceId": w.instance, "kind": "PAYMENTS", "from": "2026-09-01", "to": "2026-09-30"}),
    )
    .await
    .unwrap()["csv"]
        .clone();
    assert!(csv.as_str().unwrap().contains("110.00"));
    let receivables = call_tool(
        &w.f,
        t,
        "get_receivables",
        json!({"instanceId": w.instance}),
    )
    .await
    .unwrap();
    assert_eq!(receivables["receivables"]["totalCents"], 0);

    // An outsider can't credit someone else's invoice.
    assert!(
        call_tool(
            &w.f,
            &w.outsider_token,
            "issue_credit_note",
            json!({"invoiceId": id(&fin), "issueDate": "2026-09-07", "reason": "x"}),
        )
        .await
        .is_err()
    );
}
