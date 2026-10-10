//! MCP tools for invoicing instances: projects, billable items, invoices and
//! expenses.
//! Every tool runs one fixed GraphQL document as the caller (see
//! [`super::tool`]), so authorization is the site's: only a real member of an
//! **invoicing** instance can use them, a superuser without a membership sees
//! nothing (the same boundary as tickets), and a support instance is refused by
//! the resolver's own kind check.
//!
//! **No invoicing tool sends email.** The one thing to be careful with is
//! `finalize_invoice`: it is strictly one-way (no void, no un-finalize), assigns
//! the next invoice number, and freezes what the invoice prints, so its
//! description says so and it takes an explicit `issueDate` with no default.
//!
//! **Deliberately not exposed:** `updateInvoicingSettings` and
//! `setNextInvoiceNumber` (owner-level instance settings — change them in the
//! web app) and the raw invoice snapshot.
//!
//! Money is integers: `unitPriceCents` is GST-exclusive cents, and a quantity is
//! a decimal **string** with at most 2 decimal places (`"1.5"`) that only the
//! server parses — a JSON number is accepted and converted, never computed with.

use serde_json::{Value, json};

use crate::app::{App, HasDb, HasMail, HasStorage};

use super::tool::{ToolContext, ToolOutcome, missing_argument};

const PROJECT_FIELDS: &str = "id name clientName clientAbn clientAddress reference archived \
     createdAt updatedAt";

const ITEM_FIELDS: &str = "id date description quantity unitPriceCents amountCents status \
     project { id name }";

const EXPENSE_FIELDS: &str = "id date category description supplier amountCents gstCents \
     distanceKm rateCentsPerKm project { id name }";

/// `ExpenseCategoryType`'s values, for the tools' input schemas.
const EXPENSE_CATEGORIES: [&str; 19] = [
    "MATERIALS",
    "SUBCONTRACTORS",
    "TOOLS_EQUIPMENT",
    "VEHICLE_FUEL",
    "VEHICLE_KM",
    "TRAVEL",
    "MEALS_ENTERTAINMENT",
    "SOFTWARE_SUBSCRIPTIONS",
    "PHONE_INTERNET",
    "OFFICE_SUPPLIES",
    "PROFESSIONAL_FEES",
    "INSURANCE",
    "RENT_UTILITIES",
    "ADVERTISING_MARKETING",
    "BANK_FEES",
    "TRAINING",
    "LICENCES_MEMBERSHIPS",
    "POSTAGE_FREIGHT",
    "OTHER",
];

const EXPENSE_WRITE_DESCRIPTION: &str = "Record an expense in an INVOICING instance, \
     optionally against one of its projects. Two shapes, chosen by `category`: a PURCHASE \
     (every category except VEHICLE_KM) needs `supplier` and `amountCents` (GST-inclusive — \
     what was paid) plus optional `gstCents` (the GST included; omit for a GST-free \
     purchase); a VEHICLE_KM trip needs `distanceKm` and a `description` of its business \
     purpose, and must not have supplier/amount/GST — its amount is distance x the ATO \
     cents-per-km rate for the trip date's financial year, looked up for you. A car claimed \
     by cents per km can't also claim its fuel (VEHICLE_FUEL). Expenses are never \
     invoiced and send no email.";

/// What a list of invoices shows. `get_invoice` adds the frozen-or-live detail.
const INVOICE_SUMMARY_FIELDS: &str = "id status number displayNumber issueDate paidDate title \
     reference subtotalCents gstCents totalCents currency createdAt finalizedAt \
     project { id name clientName }";

const INVOICE_DETAIL_FIELDS: &str = "id status number displayNumber issueDate paidDate title \
     reference subtotalCents gstCents totalCents currency gstRegistered paymentDetails \
     createdAt finalizedAt project { id name clientName } \
     billTo { name abn address } seller { name abn address phone email } \
     lines { date description quantity unitPriceCents amountCents } \
     items { id date description quantity unitPriceCents amountCents status }";

const DEFAULT_PAGE_SIZE: i64 = 20;
const MAX_PAGE_SIZE: i64 = 50;

fn project_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "name": { "type": "string" },
            "clientName": { "type": "string" },
            "clientAbn": { "type": ["string", "null"] },
            "clientAddress": { "type": ["string", "null"] },
            "reference": { "type": ["string", "null"] },
            "archived": { "type": "boolean" },
            "createdAt": { "type": "integer", "description": "Unix seconds." },
            "updatedAt": { "type": "integer", "description": "Unix seconds." },
        },
    })
}

fn item_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "date": { "type": "string", "description": "YYYY-MM-DD." },
            "description": { "type": "string" },
            "quantity": { "type": "string", "description": "Decimal string, e.g. \"1.5\"." },
            "unitPriceCents": { "type": "integer", "description": "GST-exclusive, in cents." },
            "amountCents": { "type": "integer", "description": "round-half-up(quantity x unit price), in cents." },
            "status": { "type": "string", "enum": ["UNBILLED", "DRAFT", "INVOICED"] },
            "project": { "type": "object", "properties": { "id": { "type": "string" }, "name": { "type": "string" } } },
        },
    })
}

fn invoice_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "status": { "type": "string", "enum": ["DRAFT", "FINALIZED"] },
            "number": { "type": ["integer", "null"], "description": "Null for a draft." },
            "displayNumber": { "type": ["string", "null"], "description": "Zero-padded, e.g. \"008\"; null for a draft." },
            "issueDate": { "type": ["string", "null"], "description": "YYYY-MM-DD; null for a draft." },
            "paidDate": { "type": ["string", "null"], "description": "YYYY-MM-DD; null means unpaid." },
            "title": { "type": "string" },
            "reference": { "type": ["string", "null"] },
            "subtotalCents": { "type": "integer" },
            "gstCents": { "type": "integer" },
            "totalCents": { "type": "integer" },
            "currency": { "type": "string" },
            "createdAt": { "type": "integer" },
            "finalizedAt": { "type": ["integer", "null"] },
            "project": { "type": "object" },
        },
    })
}

fn listing_schema(key: &str, item: Value) -> Value {
    json!({
        "type": "object",
        "properties": {
            key: { "type": "array", "items": item },
            "hasNextPage": { "type": "boolean" },
            "endCursor": { "type": ["string", "null"] },
        },
        "required": [key, "hasNextPage"],
    })
}

fn wrap(key: &str, value: Value) -> Value {
    json!({ "type": "object", "properties": { key: value }, "required": [key] })
}

fn paging_props() -> Value {
    json!({
        "first": { "type": "integer", "minimum": 1, "maximum": MAX_PAGE_SIZE, "default": DEFAULT_PAGE_SIZE },
        "after": { "type": "string", "description": "`endCursor` from the previous page." },
    })
}

pub fn catalogue() -> Vec<Value> {
    let item_input = json!({
        "date": { "type": "string", "description": "YYYY-MM-DD." },
        "description": { "type": "string", "description": "Multi-line; lines starting `* ` or `- ` print as bullets." },
        "quantity": { "type": "string", "description": "Decimal, at most 2 decimal places, > 0 and <= 1,000,000. e.g. \"1.5\"." },
        "unitPriceCents": { "type": "integer", "description": "GST-exclusive price in cents, 0 to 1,000,000,000." },
    });
    let mut update_item_props = item_input.clone();
    update_item_props["id"] = json!({ "type": "string" });
    let expense_props = json!({
        "projectId": { "type": "string", "description": "Optional — omit for an expense not tied to a project." },
        "date": { "type": "string", "description": "YYYY-MM-DD." },
        "category": { "type": "string", "enum": EXPENSE_CATEGORIES },
        "description": { "type": "string", "description": "Optional for a purchase; the business purpose of a VEHICLE_KM trip (required there)." },
        "supplier": { "type": "string", "description": "Purchases only (required there)." },
        "amountCents": { "type": "integer", "description": "Purchases only: GST-inclusive cents paid, 1 to 1,000,000,000." },
        "gstCents": { "type": "integer", "description": "Purchases only: GST included in amountCents (usually amount / 11). Omit if GST-free." },
        "distanceKm": { "type": "string", "description": "VEHICLE_KM only: km, at most 1 decimal place, up to 5000. e.g. \"12.5\"." },
    });
    let mut create_expense_props = expense_props.clone();
    create_expense_props["instanceId"] = json!({ "type": "string" });
    let mut update_expense_props = expense_props;
    update_expense_props["id"] = json!({ "type": "string" });

    vec![
        json!({
            "name": "list_projects",
            "title": "List projects",
            "description": "List the projects (a client's billing identity) in an INVOICING \
                instance you are a member of, sorted by name. Archived projects are hidden \
                unless `includeArchived` is true. Get `instanceId` from `whoami` (only \
                instances of kind INVOICING apply). Invoicing tools send no email.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instanceId": { "type": "string" },
                    "includeArchived": { "type": "boolean", "default": false },
                },
                "required": ["instanceId"],
            },
            "outputSchema": wrap("projects", json!({ "type": "array", "items": project_schema() })),
            "annotations": { "title": "List projects", "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "create_project",
            "title": "Create project",
            "description": "Create a project in an INVOICING instance: the client/job that billable \
                items and invoices are grouped under. `name` and `clientName` are required; \
                the rest print in the invoice's \"Invoice to\" block.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instanceId": { "type": "string" },
                    "name": { "type": "string" },
                    "clientName": { "type": "string" },
                    "clientAbn": { "type": "string" },
                    "clientAddress": { "type": "string" },
                    "reference": { "type": "string", "description": "e.g. a client PO number; prints on the invoice." },
                },
                "required": ["instanceId", "name", "clientName"],
            },
            "outputSchema": wrap("project", project_schema()),
            "annotations": { "title": "Create project", "idempotentHint": false },
        }),
        json!({
            "name": "update_project",
            "title": "Update project",
            "description": "Change a project. Only the fields you pass change; the rest keep \
                their current value. Set `archived` to true to retire a project (an archived \
                project takes no new billable items), false to restore it. Already-finalized \
                invoices are unaffected: they were frozen when finalized.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "name": { "type": "string" },
                    "clientName": { "type": "string" },
                    "clientAbn": { "type": "string", "description": "Empty string clears it." },
                    "clientAddress": { "type": "string", "description": "Empty string clears it." },
                    "reference": { "type": "string", "description": "Empty string clears it." },
                    "archived": { "type": "boolean" },
                },
                "required": ["id"],
            },
            "outputSchema": wrap("project", project_schema()),
            "annotations": { "title": "Update project", "idempotentHint": true },
        }),
        json!({
            "name": "list_billable_items",
            "title": "List billable items",
            "description": "List billable items (work to be invoiced) in an INVOICING instance, \
                newest first, optionally for one project. `filter`: UNBILLED (not on any \
                invoice yet — what you can put on a new invoice), BILLED (on a draft or \
                finalized invoice) or ALL. Each item's `status` is UNBILLED, DRAFT or INVOICED. \
                Results are paged: pass `endCursor` as `after` for the next page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instanceId": { "type": "string" },
                    "projectId": { "type": "string" },
                    "filter": { "type": "string", "enum": ["ALL", "UNBILLED", "BILLED"], "default": "ALL" },
                    "first": paging_props()["first"].clone(),
                    "after": paging_props()["after"].clone(),
                },
                "required": ["instanceId"],
            },
            "outputSchema": listing_schema("items", item_schema()),
            "annotations": { "title": "List billable items", "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "create_billable_item",
            "title": "Create billable item",
            "description": "Record a unit of billable work against a project (it stays UNBILLED \
                until put on an invoice). The amount is computed for you: quantity x unit price, \
                rounded half-up to the cent. Refused for an archived project.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "projectId": { "type": "string" },
                    "date": item_input["date"].clone(),
                    "description": item_input["description"].clone(),
                    "quantity": item_input["quantity"].clone(),
                    "unitPriceCents": item_input["unitPriceCents"].clone(),
                },
                "required": ["projectId", "date", "description", "quantity", "unitPriceCents"],
            },
            "outputSchema": wrap("item", item_schema()),
            "annotations": { "title": "Create billable item", "idempotentHint": false },
        }),
        json!({
            "name": "update_billable_item",
            "title": "Update billable item",
            "description": "Replace a billable item's date, description, quantity and unit price \
                (pass all four — this is a full replace). Refused if the item is on a FINALIZED \
                invoice; an item on a draft invoice can still be edited and the draft updates.",
            "inputSchema": {
                "type": "object",
                "properties": update_item_props,
                "required": ["id", "date", "description", "quantity", "unitPriceCents"],
            },
            "outputSchema": wrap("item", item_schema()),
            "annotations": { "title": "Update billable item", "idempotentHint": true },
        }),
        json!({
            "name": "delete_billable_item",
            "title": "Delete billable item",
            "description": "Permanently delete a billable item. Refused if it is on any invoice, \
                draft or finalized — remove it from the draft first.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
            },
            "outputSchema": wrap("deletedId", json!({ "type": "string" })),
            "annotations": { "title": "Delete billable item", "destructiveHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "list_expenses",
            "title": "List expenses",
            "description": "List expenses (money the business spent, and cents-per-km vehicle \
                trips) in an INVOICING instance, newest first, optionally for one project and/or \
                one `category`. Results are paged: pass `endCursor` as `after` for the next page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instanceId": { "type": "string" },
                    "projectId": { "type": "string" },
                    "category": { "type": "string", "enum": EXPENSE_CATEGORIES },
                    "first": paging_props()["first"].clone(),
                    "after": paging_props()["after"].clone(),
                },
                "required": ["instanceId"],
            },
            "outputSchema": listing_schema("expenses", expense_schema()),
            "annotations": { "title": "List expenses", "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "create_expense",
            "title": "Create expense",
            "description": EXPENSE_WRITE_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": create_expense_props,
                "required": ["instanceId", "date", "category"],
            },
            "outputSchema": wrap("expense", expense_schema()),
            "annotations": { "title": "Create expense", "idempotentHint": false },
        }),
        json!({
            "name": "update_expense",
            "title": "Update expense",
            "description": format!(
                "Replace an expense (a full replace: pass every field it should keep — an \
                 omitted projectId, gstCents or description is cleared). {EXPENSE_WRITE_DESCRIPTION}"
            ),
            "inputSchema": {
                "type": "object",
                "properties": update_expense_props,
                "required": ["id", "date", "category"],
            },
            "outputSchema": wrap("expense", expense_schema()),
            "annotations": { "title": "Update expense", "idempotentHint": true },
        }),
        json!({
            "name": "delete_expense",
            "title": "Delete expense",
            "description": "Permanently delete an expense.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
            },
            "outputSchema": wrap("deletedId", json!({ "type": "string" })),
            "annotations": { "title": "Delete expense", "destructiveHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "get_vehicle_km_summary",
            "title": "Vehicle km this financial year",
            "description": "Your own cents-per-km vehicle trips (VEHICLE_KM expenses) in an \
                INVOICING instance for one Australian financial year, against the ATO cap of \
                5,000 business km per car. `financialYear` is the year it starts in (2026 = \
                1 July 2026 to 30 June 2027); defaults to the current one. Informational: \
                nothing stops a trip past the cap.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instanceId": { "type": "string" },
                    "financialYear": { "type": "integer" },
                },
                "required": ["instanceId"],
            },
            "outputSchema": wrap("summary", json!({
                "type": "object",
                "properties": {
                    "financialYear": { "type": "integer" },
                    "financialYearLabel": { "type": "string", "description": "e.g. \"2026–27\"." },
                    "totalKm": { "type": "string" },
                    "capKm": { "type": "integer" },
                    "rateCentsPerKm": { "type": ["integer", "null"] },
                },
            })),
            "annotations": { "title": "Vehicle km this financial year", "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "list_invoices",
            "title": "List invoices",
            "description": "List invoices in an INVOICING instance, newest first, optionally for \
                one project. `filter`: DRAFT, UNPAID (finalized, not yet paid), PAID or ALL. \
                Summaries only — use `get_invoice` for the lines and seller/bill-to detail. \
                Results are paged: pass `endCursor` as `after` for the next page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instanceId": { "type": "string" },
                    "projectId": { "type": "string" },
                    "filter": { "type": "string", "enum": ["ALL", "DRAFT", "UNPAID", "PAID"], "default": "ALL" },
                    "first": paging_props()["first"].clone(),
                    "after": paging_props()["after"].clone(),
                },
                "required": ["instanceId"],
            },
            "outputSchema": listing_schema("invoices", invoice_schema()),
            "annotations": { "title": "List invoices", "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "get_invoice",
            "title": "Get invoice",
            "description": "Fetch one invoice in full: totals, the printed lines, seller and \
                bill-to blocks, payment details, and its current items. A FINALIZED invoice \
                shows exactly what was frozen when it was finalized; a DRAFT shows a live \
                preview that changes as items and settings change.",
            "inputSchema": {
                "type": "object",
                "properties": { "invoiceId": { "type": "string" } },
                "required": ["invoiceId"],
            },
            "outputSchema": wrap("invoice", invoice_schema()),
            "annotations": { "title": "Get invoice", "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "create_invoice",
            "title": "Create draft invoice",
            "description": "Start a DRAFT invoice for a project from at least one of that \
                project's UNBILLED items (see `list_billable_items`). A draft has no number \
                and can be freely changed or deleted; nothing is final until `finalize_invoice`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "projectId": { "type": "string" },
                    "itemIds": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                },
                "required": ["projectId", "itemIds"],
            },
            "outputSchema": wrap("invoice", invoice_schema()),
            "annotations": { "title": "Create draft invoice", "idempotentHint": false },
        }),
        json!({
            "name": "add_invoice_items",
            "title": "Add items to a draft invoice",
            "description": "Attach more of the project's UNBILLED items to a DRAFT invoice. \
                Refused on a finalized invoice.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "invoiceId": { "type": "string" },
                    "itemIds": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                },
                "required": ["invoiceId", "itemIds"],
            },
            "outputSchema": wrap("invoice", invoice_schema()),
            "annotations": { "title": "Add items to a draft invoice", "idempotentHint": true },
        }),
        json!({
            "name": "remove_invoice_items",
            "title": "Remove items from a draft invoice",
            "description": "Detach items from a DRAFT invoice; they go back to UNBILLED. Removing \
                every item leaves an empty draft, which can't be finalized. Refused on a \
                finalized invoice.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "invoiceId": { "type": "string" },
                    "itemIds": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                },
                "required": ["invoiceId", "itemIds"],
            },
            "outputSchema": wrap("invoice", invoice_schema()),
            "annotations": { "title": "Remove items from a draft invoice", "idempotentHint": true },
        }),
        json!({
            "name": "delete_invoice",
            "title": "Delete draft invoice",
            "description": "Delete a DRAFT invoice outright; its items go back to UNBILLED. A \
                finalized invoice can never be deleted.",
            "inputSchema": {
                "type": "object",
                "properties": { "invoiceId": { "type": "string" } },
                "required": ["invoiceId"],
            },
            "outputSchema": wrap("deletedId", json!({ "type": "string" })),
            "annotations": { "title": "Delete draft invoice", "destructiveHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "finalize_invoice",
            "title": "Finalize invoice",
            "description": "**IRREVERSIBLE.** Finalize a DRAFT invoice: assigns the next invoice \
                number, freezes everything it prints (seller, bill-to, lines, totals, currency, \
                payment details) and locks its items. There is no void and no un-finalize, and \
                the number cannot be reused. Requires the instance's invoicing settings to \
                already have a business name. `issueDate` (YYYY-MM-DD) is required: confirm it \
                with the user rather than guessing. Only do this when the user has explicitly \
                asked to finalize this specific invoice; check it first with `get_invoice`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "invoiceId": { "type": "string" },
                    "issueDate": { "type": "string", "description": "YYYY-MM-DD." },
                },
                "required": ["invoiceId", "issueDate"],
            },
            "outputSchema": wrap("invoice", invoice_schema()),
            "annotations": { "title": "Finalize invoice", "destructiveHint": true, "idempotentHint": false },
        }),
        json!({
            "name": "set_invoice_paid",
            "title": "Set invoice paid date",
            "description": "Mark a FINALIZED invoice paid on `paidDate` (YYYY-MM-DD), or clear it \
                (mark unpaid) by omitting `paidDate`. Not printed on the invoice. Refused on a \
                draft.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "invoiceId": { "type": "string" },
                    "paidDate": { "type": "string", "description": "YYYY-MM-DD; omit to mark unpaid." },
                },
                "required": ["invoiceId"],
            },
            "outputSchema": wrap("invoice", invoice_schema()),
            "annotations": { "title": "Set invoice paid date", "idempotentHint": true },
        }),
        json!({
            "name": "get_invoice_pdf_url",
            "title": "Get invoice PDF link",
            "description": "Get a time-limited download link for a FINALIZED invoice's PDF, \
                rendered from exactly what was frozen at finalization. Refused on a draft. \
                The link is a presigned URL: hand it to the user, don't try to fetch it.",
            "inputSchema": {
                "type": "object",
                "properties": { "invoiceId": { "type": "string" } },
                "required": ["invoiceId"],
            },
            "outputSchema": wrap("url", json!({ "type": "string" })),
            "annotations": { "title": "Get invoice PDF link", "readOnlyHint": true, "idempotentHint": true },
        }),
    ]
}

fn string_arg<'a>(arguments: &'a Value, name: &str) -> Option<&'a str> {
    arguments.get(name).and_then(Value::as_str)
}

/// A decimal quantity, accepted as a string (preferred) or a JSON number. Only
/// the text is forwarded: the server does all the parsing and rounding.
fn quantity_arg(arguments: &Value) -> Option<String> {
    match arguments.get("quantity")? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn id_list(arguments: &Value, name: &str) -> Option<Vec<String>> {
    let items: Vec<String> = arguments
        .get(name)?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    (!items.is_empty()).then_some(items)
}

/// Move a mutation's root field to the tool's documented output key.
fn rename(result: Result<Value, Vec<String>>, from: &str, to: &str) -> ToolOutcome {
    result
        .map(|mut data| json!({ to: data[from].take() }))
        .into()
}

/// A connection's nodes + paging info, under `key`.
fn unwrap_connection(result: Result<Value, Vec<String>>, from: &str, key: &str) -> ToolOutcome {
    result
        .map(|data| {
            let conn = &data[from];
            let nodes: Vec<Value> = conn["edges"]
                .as_array()
                .map(|edges| edges.iter().map(|e| e["node"].clone()).collect())
                .unwrap_or_default();
            json!({
                key: nodes,
                "hasNextPage": conn["pageInfo"]["hasNextPage"],
                "endCursor": conn["pageInfo"]["endCursor"],
            })
        })
        .into()
}

fn page_size(arguments: &Value) -> i64 {
    arguments
        .get("first")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE)
}

/// `None` when `name` isn't one of this module's tools.
pub async fn dispatch<A>(
    ctx: &ToolContext<'_, A>,
    name: &str,
    arguments: &Value,
) -> Option<ToolOutcome>
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    Some(match name {
        "list_projects" => list_projects(ctx, arguments).await,
        "create_project" => create_project(ctx, arguments).await,
        "update_project" => update_project(ctx, arguments).await,
        "list_billable_items" => list_items(ctx, arguments).await,
        "create_billable_item" => create_item(ctx, arguments).await,
        "update_billable_item" => update_item(ctx, arguments).await,
        "delete_billable_item" => {
            let Some(id) = string_arg(arguments, "id") else {
                return Some(missing_argument("id"));
            };
            rename(
                ctx.run(
                    "mutation McpDeleteItem($id: ID!) { deleteBillableItem(id: $id) }",
                    json!({ "id": id }),
                )
                .await,
                "deleteBillableItem",
                "deletedId",
            )
        }
        "list_expenses" => list_expenses(ctx, arguments).await,
        "create_expense" | "update_expense" => write_expense(ctx, name, arguments).await,
        "delete_expense" => {
            let Some(id) = string_arg(arguments, "id") else {
                return Some(missing_argument("id"));
            };
            rename(
                ctx.run(
                    "mutation McpDeleteExpense($id: ID!) { deleteExpense(id: $id) }",
                    json!({ "id": id }),
                )
                .await,
                "deleteExpense",
                "deletedId",
            )
        }
        "get_vehicle_km_summary" => {
            let Some(instance_id) = string_arg(arguments, "instanceId") else {
                return Some(missing_argument("instanceId"));
            };
            rename(
                ctx.run(
                    "query McpVehicleKm($instanceId: ID!, $financialYear: Int) { \
                     vehicleKmSummary(instanceId: $instanceId, financialYear: $financialYear) { \
                     financialYear financialYearLabel totalKm capKm rateCentsPerKm } }",
                    json!({
                        "instanceId": instance_id,
                        "financialYear": arguments.get("financialYear").and_then(Value::as_i64),
                    }),
                )
                .await,
                "vehicleKmSummary",
                "summary",
            )
        }
        "list_invoices" => list_invoices(ctx, arguments).await,
        "get_invoice" => {
            let Some(id) = string_arg(arguments, "invoiceId") else {
                return Some(missing_argument("invoiceId"));
            };
            let doc = format!(
                "query McpInvoice($id: ID!) {{ invoice(id: $id) {{ {INVOICE_DETAIL_FIELDS} }} }}"
            );
            match ctx.run(&doc, json!({ "id": id })).await {
                Err(e) => ToolOutcome::Error(e),
                // `null` for a missing id and a non-member alike.
                Ok(data) if data["invoice"].is_null() => ToolOutcome::error(
                    "Invoice not found (or you are not a member of its instance)",
                ),
                Ok(mut data) => ToolOutcome::Ok(json!({ "invoice": data["invoice"].take() })),
            }
        }
        "create_invoice" => {
            let (Some(project_id), Some(item_ids)) = (
                string_arg(arguments, "projectId"),
                id_list(arguments, "itemIds"),
            ) else {
                return Some(missing_argument("projectId/itemIds"));
            };
            let doc = format!(
                "mutation McpCreateInvoice($projectId: ID!, $itemIds: [ID!]!) {{ \
                 createInvoice(projectId: $projectId, itemIds: $itemIds) {{ {INVOICE_DETAIL_FIELDS} }} }}"
            );
            rename(
                ctx.run(
                    &doc,
                    json!({ "projectId": project_id, "itemIds": item_ids }),
                )
                .await,
                "createInvoice",
                "invoice",
            )
        }
        "add_invoice_items" | "remove_invoice_items" => {
            let (Some(invoice_id), Some(item_ids)) = (
                string_arg(arguments, "invoiceId"),
                id_list(arguments, "itemIds"),
            ) else {
                return Some(missing_argument("invoiceId/itemIds"));
            };
            let field = if name == "add_invoice_items" {
                "addInvoiceItems"
            } else {
                "removeInvoiceItems"
            };
            let doc = format!(
                "mutation McpInvoiceItems($invoiceId: ID!, $itemIds: [ID!]!) {{ \
                 {field}(invoiceId: $invoiceId, itemIds: $itemIds) {{ {INVOICE_DETAIL_FIELDS} }} }}"
            );
            rename(
                ctx.run(
                    &doc,
                    json!({ "invoiceId": invoice_id, "itemIds": item_ids }),
                )
                .await,
                field,
                "invoice",
            )
        }
        "delete_invoice" => {
            let Some(id) = string_arg(arguments, "invoiceId") else {
                return Some(missing_argument("invoiceId"));
            };
            rename(
                ctx.run(
                    "mutation McpDeleteInvoice($id: ID!) { deleteInvoice(invoiceId: $id) }",
                    json!({ "id": id }),
                )
                .await,
                "deleteInvoice",
                "deletedId",
            )
        }
        "finalize_invoice" => {
            let (Some(id), Some(date)) = (
                string_arg(arguments, "invoiceId"),
                string_arg(arguments, "issueDate"),
            ) else {
                return Some(missing_argument("invoiceId/issueDate"));
            };
            let doc = format!(
                "mutation McpFinalize($id: ID!, $date: String!) {{ \
                 finalizeInvoice(invoiceId: $id, issueDate: $date) {{ {INVOICE_DETAIL_FIELDS} }} }}"
            );
            rename(
                ctx.run(&doc, json!({ "id": id, "date": date })).await,
                "finalizeInvoice",
                "invoice",
            )
        }
        "set_invoice_paid" => {
            let Some(id) = string_arg(arguments, "invoiceId") else {
                return Some(missing_argument("invoiceId"));
            };
            let doc = format!(
                "mutation McpPaid($id: ID!, $paid: String) {{ \
                 setInvoicePaid(invoiceId: $id, paidDate: $paid) {{ {INVOICE_SUMMARY_FIELDS} }} }}"
            );
            rename(
                ctx.run(
                    &doc,
                    json!({ "id": id, "paid": string_arg(arguments, "paidDate") }),
                )
                .await,
                "setInvoicePaid",
                "invoice",
            )
        }
        "get_invoice_pdf_url" => {
            let Some(id) = string_arg(arguments, "invoiceId") else {
                return Some(missing_argument("invoiceId"));
            };
            rename(
                ctx.run(
                    "mutation McpPdf($id: ID!) { downloadInvoicePdf(invoiceId: $id) }",
                    json!({ "id": id }),
                )
                .await,
                "downloadInvoicePdf",
                "url",
            )
        }
        _ => return None,
    })
}

async fn list_projects<A>(ctx: &ToolContext<'_, A>, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    let Some(instance_id) = string_arg(arguments, "instanceId") else {
        return missing_argument("instanceId");
    };
    let include_archived = arguments
        .get("includeArchived")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let doc = format!(
        "query McpProjects($instanceId: ID!, $archived: Boolean!) {{ \
         projects(instanceId: $instanceId, includeArchived: $archived) {{ {PROJECT_FIELDS} }} }}"
    );
    rename(
        ctx.run(
            &doc,
            json!({ "instanceId": instance_id, "archived": include_archived }),
        )
        .await,
        "projects",
        "projects",
    )
}

async fn create_project<A>(ctx: &ToolContext<'_, A>, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    let (Some(instance_id), Some(name), Some(client_name)) = (
        string_arg(arguments, "instanceId"),
        string_arg(arguments, "name"),
        string_arg(arguments, "clientName"),
    ) else {
        return missing_argument("instanceId/name/clientName");
    };
    let doc = format!(
        "mutation McpCreateProject($instanceId: ID!, $input: CreateProjectInput!) {{ \
         createProject(instanceId: $instanceId, input: $input) {{ {PROJECT_FIELDS} }} }}"
    );
    let input = json!({
        "name": name,
        "clientName": client_name,
        "clientAbn": string_arg(arguments, "clientAbn"),
        "clientAddress": string_arg(arguments, "clientAddress"),
        "reference": string_arg(arguments, "reference"),
    });
    rename(
        ctx.run(&doc, json!({ "instanceId": instance_id, "input": input }))
            .await,
        "createProject",
        "project",
    )
}

/// `updateProject` is a full replace, so read the current record (as the caller —
/// a non-member finds nothing, exactly as over GraphQL) and merge the passed
/// fields over it. An empty string clears an optional field.
async fn update_project<A>(ctx: &ToolContext<'_, A>, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    let Some(id) = string_arg(arguments, "id") else {
        return missing_argument("id");
    };
    let read = format!("query McpProject($id: ID!) {{ project(id: $id) {{ {PROJECT_FIELDS} }} }}");
    let current = match ctx.run(&read, json!({ "id": id })).await {
        Err(e) => return ToolOutcome::Error(e),
        Ok(data) if data["project"].is_null() => {
            return ToolOutcome::error(
                "Project not found (or you are not a member of its instance)",
            );
        }
        Ok(data) => data["project"].clone(),
    };
    let pick = |field: &str| -> Value {
        match arguments.get(field).and_then(Value::as_str) {
            Some("") => Value::Null,
            Some(s) => json!(s),
            None => current[field].clone(),
        }
    };
    let input = json!({
        "name": arguments.get("name").and_then(Value::as_str).map(Value::from).unwrap_or_else(|| current["name"].clone()),
        "clientName": arguments.get("clientName").and_then(Value::as_str).map(Value::from).unwrap_or_else(|| current["clientName"].clone()),
        "clientAbn": pick("clientAbn"),
        "clientAddress": pick("clientAddress"),
        "reference": pick("reference"),
        "archived": arguments.get("archived").and_then(Value::as_bool).unwrap_or_else(|| current["archived"].as_bool().unwrap_or(false)),
    });
    let doc = format!(
        "mutation McpUpdateProject($id: ID!, $input: UpdateProjectInput!) {{ \
         updateProject(id: $id, input: $input) {{ {PROJECT_FIELDS} }} }}"
    );
    rename(
        ctx.run(&doc, json!({ "id": id, "input": input })).await,
        "updateProject",
        "project",
    )
}

async fn list_items<A>(ctx: &ToolContext<'_, A>, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    let Some(instance_id) = string_arg(arguments, "instanceId") else {
        return missing_argument("instanceId");
    };
    let doc = format!(
        "query McpItems($instanceId: ID!, $projectId: ID, $filter: BillableItemFilterType!, \
         $first: Int!, $after: String) {{ billableItems(instanceId: $instanceId, \
         projectId: $projectId, filter: $filter, first: $first, after: $after) {{ \
         edges {{ node {{ {ITEM_FIELDS} }} }} pageInfo {{ hasNextPage endCursor }} }} }}"
    );
    unwrap_connection(
        ctx.run(
            &doc,
            json!({
                "instanceId": instance_id,
                "projectId": string_arg(arguments, "projectId"),
                "filter": string_arg(arguments, "filter").unwrap_or("ALL"),
                "first": page_size(arguments),
                "after": string_arg(arguments, "after"),
            }),
        )
        .await,
        "billableItems",
        "items",
    )
}

async fn create_item<A>(ctx: &ToolContext<'_, A>, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    let (Some(project_id), Some(date), Some(description), Some(quantity), Some(price)) = (
        string_arg(arguments, "projectId"),
        string_arg(arguments, "date"),
        string_arg(arguments, "description"),
        quantity_arg(arguments),
        arguments.get("unitPriceCents").and_then(Value::as_i64),
    ) else {
        return missing_argument("projectId/date/description/quantity/unitPriceCents");
    };
    let doc = format!(
        "mutation McpCreateItem($projectId: ID!, $input: BillableItemInput!) {{ \
         createBillableItem(projectId: $projectId, input: $input) {{ {ITEM_FIELDS} }} }}"
    );
    let input = json!({
        "date": date, "description": description, "quantity": quantity, "unitPriceCents": price,
    });
    rename(
        ctx.run(&doc, json!({ "projectId": project_id, "input": input }))
            .await,
        "createBillableItem",
        "item",
    )
}

async fn update_item<A>(ctx: &ToolContext<'_, A>, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    let (Some(id), Some(date), Some(description), Some(quantity), Some(price)) = (
        string_arg(arguments, "id"),
        string_arg(arguments, "date"),
        string_arg(arguments, "description"),
        quantity_arg(arguments),
        arguments.get("unitPriceCents").and_then(Value::as_i64),
    ) else {
        return missing_argument("id/date/description/quantity/unitPriceCents");
    };
    let doc = format!(
        "mutation McpUpdateItem($id: ID!, $input: BillableItemInput!) {{ \
         updateBillableItem(id: $id, input: $input) {{ {ITEM_FIELDS} }} }}"
    );
    let input = json!({
        "date": date, "description": description, "quantity": quantity, "unitPriceCents": price,
    });
    rename(
        ctx.run(&doc, json!({ "id": id, "input": input })).await,
        "updateBillableItem",
        "item",
    )
}

async fn list_invoices<A>(ctx: &ToolContext<'_, A>, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    let Some(instance_id) = string_arg(arguments, "instanceId") else {
        return missing_argument("instanceId");
    };
    let doc = format!(
        "query McpInvoices($instanceId: ID!, $projectId: ID, $filter: InvoiceFilterType!, \
         $first: Int!, $after: String) {{ invoices(instanceId: $instanceId, \
         projectId: $projectId, filter: $filter, first: $first, after: $after) {{ \
         edges {{ node {{ {INVOICE_SUMMARY_FIELDS} }} }} pageInfo {{ hasNextPage endCursor }} }} }}"
    );
    unwrap_connection(
        ctx.run(
            &doc,
            json!({
                "instanceId": instance_id,
                "projectId": string_arg(arguments, "projectId"),
                "filter": string_arg(arguments, "filter").unwrap_or("ALL"),
                "first": page_size(arguments),
                "after": string_arg(arguments, "after"),
            }),
        )
        .await,
        "invoices",
        "invoices",
    )
}

fn expense_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "date": { "type": "string", "description": "YYYY-MM-DD." },
            "category": { "type": "string", "enum": EXPENSE_CATEGORIES },
            "description": { "type": ["string", "null"] },
            "supplier": { "type": ["string", "null"], "description": "Null for a vehicle trip." },
            "amountCents": { "type": "integer", "description": "GST-inclusive cents paid; for a trip, distance x rate." },
            "gstCents": { "type": ["integer", "null"], "description": "GST included in amountCents; null when GST-free or a trip." },
            "distanceKm": { "type": ["string", "null"], "description": "A trip's km, e.g. \"12.5\"; null for a purchase." },
            "rateCentsPerKm": { "type": ["integer", "null"], "description": "The ATO rate the trip was claimed at." },
            "project": { "type": ["object", "null"], "properties": { "id": { "type": "string" }, "name": { "type": "string" } } },
        },
    })
}

/// The `ExpenseInput` fields, forwarded as given — the server does all the
/// validation. `distanceKm` may be a string (preferred) or a JSON number.
fn expense_input(arguments: &Value) -> Value {
    let distance_km = match arguments.get("distanceKm") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    json!({
        "projectId": string_arg(arguments, "projectId"),
        "date": string_arg(arguments, "date"),
        "category": string_arg(arguments, "category"),
        "description": string_arg(arguments, "description"),
        "supplier": string_arg(arguments, "supplier"),
        "amountCents": arguments.get("amountCents").and_then(Value::as_i64),
        "gstCents": arguments.get("gstCents").and_then(Value::as_i64),
        "distanceKm": distance_km,
    })
}

async fn list_expenses<A>(ctx: &ToolContext<'_, A>, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    let Some(instance_id) = string_arg(arguments, "instanceId") else {
        return missing_argument("instanceId");
    };
    let doc = format!(
        "query McpExpenses($instanceId: ID!, $projectId: ID, $category: ExpenseCategoryType, \
         $first: Int!, $after: String) {{ expenses(instanceId: $instanceId, \
         projectId: $projectId, category: $category, first: $first, after: $after) {{ \
         edges {{ node {{ {EXPENSE_FIELDS} }} }} pageInfo {{ hasNextPage endCursor }} }} }}"
    );
    unwrap_connection(
        ctx.run(
            &doc,
            json!({
                "instanceId": instance_id,
                "projectId": string_arg(arguments, "projectId"),
                "category": string_arg(arguments, "category"),
                "first": page_size(arguments),
                "after": string_arg(arguments, "after"),
            }),
        )
        .await,
        "expenses",
        "expenses",
    )
}

async fn write_expense<A>(ctx: &ToolContext<'_, A>, name: &str, arguments: &Value) -> ToolOutcome
where
    A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static,
{
    if string_arg(arguments, "date").is_none() || string_arg(arguments, "category").is_none() {
        return missing_argument("date/category");
    }
    let input = expense_input(arguments);
    let (doc, field, vars) = if name == "create_expense" {
        let Some(instance_id) = string_arg(arguments, "instanceId") else {
            return missing_argument("instanceId");
        };
        (
            format!(
                "mutation McpCreateExpense($instanceId: ID!, $input: ExpenseInput!) {{ \
                 createExpense(instanceId: $instanceId, input: $input) {{ {EXPENSE_FIELDS} }} }}"
            ),
            "createExpense",
            json!({ "instanceId": instance_id, "input": input }),
        )
    } else {
        let Some(id) = string_arg(arguments, "id") else {
            return missing_argument("id");
        };
        (
            format!(
                "mutation McpUpdateExpense($id: ID!, $input: ExpenseInput!) {{ \
                 updateExpense(id: $id, input: $input) {{ {EXPENSE_FIELDS} }} }}"
            ),
            "updateExpense",
            json!({ "id": id, "input": input }),
        )
    };
    rename(ctx.run(&doc, vars).await, field, "expense")
}
