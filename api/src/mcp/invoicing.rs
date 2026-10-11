//! MCP tools for invoicing instances: projects, billable items, invoices and
//! expenses.
//! Every tool runs one fixed GraphQL document as the caller (see
//! [`super::tool`]), so authorization is the site's: only a real member of an
//! **invoicing** instance can use them, a superuser without a membership sees
//! nothing (the same boundary as tickets), and a support instance is refused by
//! the resolver's own kind check.
//!
//! **Only `send_invoice` and `send_credit_note` send email** (to the client,
//! with the PDF attached), and their descriptions say so. The other things to
//! be careful with are `finalize_invoice` and `issue_credit_note`: both are
//! strictly one-way (no void, no un-finalize, no deleting a credit note),
//! assign the next number in their sequence, and freeze what they print, so
//! their descriptions say so and they take an explicit date with no default.
//!
//! **Importing existing invoices** needs nothing extra: billable-item dates,
//! `issueDate` and `paidDate` all accept any past date, and `finalize_invoice`
//! takes an optional owner-only `number` — any number not already used in the
//! instance — so an imported invoice keeps its original number and date in the
//! same irreversible call.
//!
//! **Deliberately not exposed:** `updateInvoicingSettings` and
//! `setNextInvoiceNumber` (owner-level instance settings — change them in the
//! web app; `finalize_invoice`'s `number` covers what an import needs) and the
//! raw invoice snapshot.
//!
//! Money is integers: `unitPriceCents` is GST-exclusive cents, and a quantity is
//! a decimal **string** with at most 2 decimal places (`"1.5"`) that only the
//! server parses — a JSON number is accepted and converted, never computed with.

use serde_json::{Value, json};

use crate::app::{App, HasDb, HasMail, HasStorage};

use super::tool::{ToolContext, ToolOutcome, missing_argument};

const PROJECT_FIELDS: &str = "id name clientName clientAbn clientAddress clientEmail reference \
     paymentTermsDays effectivePaymentTermsDays defaultUnitPriceCents archived createdAt updatedAt";

const ITEM_FIELDS: &str = "id date description quantity unitPriceCents amountCents gstFree status \
     project { id name }";

const EXPENSE_FIELDS: &str = "id date category description supplier amountCents gstCents \
     distanceKm rateCentsPerKm project { id name } rebilledItem { id status } \
     receipt { filename contentType size }";

const CREDIT_NOTE_FIELDS: &str = "id displayNumber issueDate reason title subtotalCents gstCents \
     totalCents currency createdAt sentAt sentTo invoice { id displayNumber } \
     lines { description amountCents gstFree }";

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
     invoiced; one on a project can be passed on to the client with `rebill_expense`. \
     Sends no email.";

/// What a list of invoices shows. `get_invoice` adds the frozen-or-live detail.
const INVOICE_SUMMARY_FIELDS: &str = "id status number displayNumber issueDate dueDate paidDate \
     overdue daysOverdue title reference subtotalCents gstCents totalCents paidCents \
     creditedCents balanceCents currency createdAt finalizedAt sentAt \
     project { id name clientName }";

const INVOICE_DETAIL_FIELDS: &str = "id status number displayNumber issueDate dueDate paidDate \
     overdue daysOverdue title reference subtotalCents gstCents totalCents paidCents \
     creditedCents balanceCents currency gstRegistered paymentDetails createdAt finalizedAt \
     sentAt sentTo project { id name clientName clientEmail } \
     billTo { name abn address } seller { name abn address phone email } \
     lines { date description quantity unitPriceCents amountCents gstFree } \
     items { id date description quantity unitPriceCents amountCents gstFree status } \
     payments { id date amountCents note } \
     creditNotes { id displayNumber issueDate reason totalCents }";

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
            "clientEmail": { "type": ["string", "null"], "description": "Where send_invoice mails by default." },
            "paymentTermsDays": { "type": ["integer", "null"], "description": "This project's own payment terms; null uses the instance's." },
            "effectivePaymentTermsDays": { "type": "integer", "description": "The terms its invoices actually get." },
            "defaultUnitPriceCents": { "type": ["integer", "null"], "description": "GST-exclusive rate a new item gets when it names no price." },
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
            "gstFree": { "type": "boolean", "description": "No GST on this line." },
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
            "dueDate": { "type": ["string", "null"], "description": "YYYY-MM-DD; null for a draft." },
            "paidDate": { "type": ["string", "null"], "description": "YYYY-MM-DD the balance reached zero (payments and credit notes); null while anything is owed." },
            "overdue": { "type": "boolean" },
            "daysOverdue": { "type": ["integer", "null"] },
            "paidCents": { "type": "integer" },
            "creditedCents": { "type": "integer", "description": "Sum of its credit notes, GST-inclusive." },
            "balanceCents": { "type": "integer", "description": "total - credited - paid." },
            "sentAt": { "type": ["integer", "null"], "description": "Unix seconds send_invoice last mailed it." },
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
        "unitPriceCents": { "type": "integer", "description": "GST-exclusive price in cents, 0 to 1,000,000,000. Optional: a new item takes its project's defaultUnitPriceCents; an update keeps the current price." },
        "gstFree": { "type": "boolean", "default": false, "description": "No GST on this line even when the business is GST-registered." },
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
                    "clientEmail": { "type": "string", "description": "Where send_invoice mails by default." },
                    "reference": { "type": "string", "description": "e.g. a client PO number; prints on the invoice." },
                    "paymentTermsDays": { "type": "integer", "description": "0-365. Overrides the instance's terms for this project's invoices." },
                    "defaultUnitPriceCents": { "type": "integer", "description": "GST-exclusive rate new items get when they name no price." },
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
                    "clientEmail": { "type": "string", "description": "Empty string clears it." },
                    "paymentTermsDays": { "type": ["integer", "null"], "description": "null clears it (the instance's terms apply)." },
                    "defaultUnitPriceCents": { "type": ["integer", "null"], "description": "null clears it." },
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
                    "gstFree": item_input["gstFree"].clone(),
                },
                "required": ["projectId", "date", "description", "quantity"],
            },
            "outputSchema": wrap("item", item_schema()),
            "annotations": { "title": "Create billable item", "idempotentHint": false },
        }),
        json!({
            "name": "update_billable_item",
            "title": "Update billable item",
            "description": "Replace a billable item's date, description, quantity, unit price and \
                GST-free flag (a full replace: an omitted gstFree becomes false; an omitted \
                unitPriceCents keeps the current price). Refused if the item is on a FINALIZED \
                invoice; an item on a draft invoice can still be edited and the draft updates.",
            "inputSchema": {
                "type": "object",
                "properties": update_item_props,
                "required": ["id", "date", "description", "quantity"],
            },
            "outputSchema": wrap("item", item_schema()),
            "annotations": { "title": "Update billable item", "idempotentHint": true },
        }),
        json!({
            "name": "delete_billable_item",
            "title": "Delete billable item",
            "description": "Permanently delete a billable item. Refused if it is on any invoice, \
                draft or finalized — remove it from the draft first. Deleting an item made by \
                rebill_expense frees that expense to be re-billed.",
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
            "description": "Permanently delete an expense. Refused while it is re-billed (delete \
                its billable item first).",
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
                one project. `filter`: DRAFT, UNPAID (finalized, something still owed), OVERDUE \
                (unpaid and past its due date), PAID or ALL. \
                Summaries only — use `get_invoice` for the lines and seller/bill-to detail. \
                Results are paged: pass `endCursor` as `after` for the next page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instanceId": { "type": "string" },
                    "projectId": { "type": "string" },
                    "filter": { "type": "string", "enum": ["ALL", "DRAFT", "UNPAID", "OVERDUE", "PAID"], "default": "ALL" },
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
            "description": "Fetch one invoice in full: totals, due date, payments, credit notes, \
                balance, the printed lines, seller and bill-to blocks, payment details, and its \
                current items. A FINALIZED invoice \
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
                asked to finalize this specific invoice; check it first with `get_invoice`.\n\n\
                Importing an existing invoice: give its original `issueDate` (past dates are \
                fine) and its original `number`. `number` is owner-only and may be any number \
                no invoice in the instance already has, in any order; a used one is refused and \
                leaves the invoice a draft. Automatic numbering carries on after the highest \
                number used. Without `number`, the next number in sequence is used. The seller \
                details and payment text printed are the instance's CURRENT invoicing settings. \
                `dueDate` defaults to issueDate + the project's (else the instance's) payment \
                terms. A mistake is corrected afterwards with issue_credit_note. Sends no email.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "invoiceId": { "type": "string" },
                    "issueDate": { "type": "string", "description": "YYYY-MM-DD; may be in the past." },
                    "number": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Owner-only: use exactly this invoice number (e.g. 8 for \"008\") \
                            instead of the next in sequence. For importing existing invoices; omit otherwise.",
                    },
                    "dueDate": { "type": "string", "description": "YYYY-MM-DD, on or after issueDate. Optional." },
                },
                "required": ["invoiceId", "issueDate"],
            },
            "outputSchema": wrap("invoice", invoice_schema()),
            "annotations": { "title": "Finalize invoice", "destructiveHint": true, "idempotentHint": false },
        }),
        json!({
            "name": "set_invoice_paid",
            "title": "Set invoice paid date",
            "description": "Mark a FINALIZED invoice paid in full on `paidDate` (YYYY-MM-DD) — \
                records one payment of its whole remaining balance — or remove every recorded \
                payment by omitting `paidDate`. For part-payments use record_invoice_payment. \
                Refused on a draft.",
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
        json!({
            "name": "record_invoice_payment",
            "title": "Record invoice payment",
            "description": "Record money received against a FINALIZED invoice: `amountCents` \
                (GST-inclusive, no more than the balance owing) on `date` (YYYY-MM-DD), with an \
                optional `note`. The invoice becomes PAID when its balance reaches zero. Sends no email.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "invoiceId": { "type": "string" },
                    "date": { "type": "string" },
                    "amountCents": { "type": "integer" },
                    "note": { "type": "string" },
                },
                "required": ["invoiceId", "date", "amountCents"],
            },
            "outputSchema": wrap("invoice", invoice_schema()),
            "annotations": { "title": "Record invoice payment", "idempotentHint": false },
        }),
        json!({
            "name": "delete_invoice_payment",
            "title": "Delete invoice payment",
            "description": "Remove one recorded payment (get its id from get_invoice's \
                `payments`). The invoice goes back to UNPAID if anything is then owed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "invoiceId": { "type": "string" },
                    "paymentId": { "type": "string" },
                },
                "required": ["invoiceId", "paymentId"],
            },
            "outputSchema": wrap("invoice", invoice_schema()),
            "annotations": { "title": "Delete invoice payment", "destructiveHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "send_invoice",
            "title": "Email invoice to client",
            "description": "**SENDS EMAIL to the client, which cannot be unsent.** Email a \
                FINALIZED invoice with its PDF attached: to `to` (default: the project's client \
                email) and `cc`, at most 10 addresses, with an optional covering `message`. It \
                comes from the business name with replies going to the business email. Can be \
                sent again as a reminder. Only do this when the user has asked to send this \
                specific invoice, and confirm the recipients with them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "invoiceId": { "type": "string" },
                    "to": { "type": "array", "items": { "type": "string" } },
                    "cc": { "type": "array", "items": { "type": "string" } },
                    "message": { "type": "string" },
                },
                "required": ["invoiceId"],
            },
            "outputSchema": wrap("invoice", invoice_schema()),
            "annotations": { "title": "Email invoice to client", "destructiveHint": false, "idempotentHint": false, "openWorldHint": true },
        }),
        json!({
            "name": "issue_credit_note",
            "title": "Issue credit note",
            "description": "**IRREVERSIBLE.** Issue a credit note (an \"Adjustment Note\" for a \
                tax invoice) against a FINALIZED invoice — the only way to correct one. It gets \
                the next CN- number and can never be edited or deleted. `lines` are GST-exclusive \
                amounts to credit ({description, amountCents, gstFree}); omit them to credit the \
                whole invoice (only while nothing has been credited yet). GST is added at the \
                invoice's own rate. The credit can't exceed what's left uncredited; it reduces \
                the invoice's balance and marks it PAID if that reaches zero. `issueDate` and \
                `reason` are required: confirm them with the user. Sends no email.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "invoiceId": { "type": "string" },
                    "issueDate": { "type": "string", "description": "YYYY-MM-DD, not before the invoice's issue date." },
                    "reason": { "type": "string", "description": "Printed on the credit note." },
                    "lines": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "description": { "type": "string" },
                                "amountCents": { "type": "integer" },
                                "gstFree": { "type": "boolean" },
                            },
                            "required": ["description", "amountCents"],
                        },
                    },
                },
                "required": ["invoiceId", "issueDate", "reason"],
            },
            "outputSchema": wrap("creditNote", credit_note_schema()),
            "annotations": { "title": "Issue credit note", "destructiveHint": true, "idempotentHint": false },
        }),
        json!({
            "name": "list_credit_notes",
            "title": "List credit notes",
            "description": "List credit notes in an INVOICING instance, newest first, optionally \
                for one invoice. Paged: pass `endCursor` as `after` for the next page.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instanceId": { "type": "string" },
                    "invoiceId": { "type": "string" },
                    "first": paging_props()["first"].clone(),
                    "after": paging_props()["after"].clone(),
                },
                "required": ["instanceId"],
            },
            "outputSchema": listing_schema("creditNotes", credit_note_schema()),
            "annotations": { "title": "List credit notes", "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "get_credit_note_pdf_url",
            "title": "Get credit note PDF link",
            "description": "Get a time-limited download link for a credit note's PDF. Hand it to \
                the user; don't fetch it.",
            "inputSchema": {
                "type": "object",
                "properties": { "creditNoteId": { "type": "string" } },
                "required": ["creditNoteId"],
            },
            "outputSchema": wrap("url", json!({ "type": "string" })),
            "annotations": { "title": "Get credit note PDF link", "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "send_credit_note",
            "title": "Email credit note to client",
            "description": "**SENDS EMAIL to the client, which cannot be unsent.** send_invoice \
                for a credit note: emails it with its PDF to `to` (default: the project's client \
                email) and `cc`. Only when the user has asked; confirm the recipients.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "creditNoteId": { "type": "string" },
                    "to": { "type": "array", "items": { "type": "string" } },
                    "cc": { "type": "array", "items": { "type": "string" } },
                    "message": { "type": "string" },
                },
                "required": ["creditNoteId"],
            },
            "outputSchema": wrap("creditNote", credit_note_schema()),
            "annotations": { "title": "Email credit note to client", "destructiveHint": false, "idempotentHint": false, "openWorldHint": true },
        }),
        json!({
            "name": "rebill_expense",
            "title": "Re-bill expense to client",
            "description": "Pass an expense on to its project's client: creates an UNBILLED \
                billable item for the expense's GST-exclusive cost plus `markupPercent` (default \
                0), dated like the expense and linked to it so it can't be billed twice. \
                `description` defaults to the category and supplier. Refused for an expense \
                with no project or one already re-billed. Sends no email.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "expenseId": { "type": "string" },
                    "markupPercent": { "type": "string", "description": "e.g. \"10\" or \"12.5\"; 0-1000." },
                    "description": { "type": "string" },
                    "gstFree": { "type": "boolean", "default": false },
                },
                "required": ["expenseId"],
            },
            "outputSchema": wrap("item", item_schema()),
            "annotations": { "title": "Re-bill expense to client", "idempotentHint": false },
        }),
        json!({
            "name": "get_gst_report",
            "title": "GST / BAS report",
            "description": "The BAS figures for an INVOICING instance over `from`..`to` \
                (YYYY-MM-DD, at most two years): G1 total sales and 1A GST on sales (from \
                finalized invoices and credit notes by issue date for ACCRUAL, or from payments \
                received for CASH, with GST apportioned per payment), and purchases and 1B GST \
                on purchases from expenses by date. Figures are in cents. Informational — check \
                with an accountant before lodging.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instanceId": { "type": "string" },
                    "from": { "type": "string" },
                    "to": { "type": "string" },
                    "basis": { "type": "string", "enum": ["CASH", "ACCRUAL"] },
                },
                "required": ["instanceId", "from", "to", "basis"],
            },
            "outputSchema": wrap("report", json!({ "type": "object" })),
            "annotations": { "title": "GST / BAS report", "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "get_receivables",
            "title": "Aged receivables",
            "description": "What clients owe an INVOICING instance: the outstanding total, \
                bucketed by days past due (current, 1-30, 31-60, 61-90, over 90), and every \
                invoice with a balance, most overdue first.",
            "inputSchema": {
                "type": "object",
                "properties": { "instanceId": { "type": "string" } },
                "required": ["instanceId"],
            },
            "outputSchema": wrap("receivables", json!({ "type": "object" })),
            "annotations": { "title": "Aged receivables", "readOnlyHint": true, "idempotentHint": true },
        }),
        json!({
            "name": "export_csv",
            "title": "Export CSV",
            "description": "A CSV export for an accountant over `from`..`to` (YYYY-MM-DD, at \
                most two years): INVOICES (by issue date), CREDIT_NOTES, PAYMENTS (by payment \
                date) or EXPENSES (by date). Returns the CSV text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "instanceId": { "type": "string" },
                    "kind": { "type": "string", "enum": ["INVOICES", "CREDIT_NOTES", "PAYMENTS", "EXPENSES"] },
                    "from": { "type": "string" },
                    "to": { "type": "string" },
                },
                "required": ["instanceId", "kind", "from", "to"],
            },
            "outputSchema": wrap("csv", json!({ "type": "string" })),
            "annotations": { "title": "Export CSV", "readOnlyHint": true, "idempotentHint": true },
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
            let number = match arguments.get("number") {
                None | Some(Value::Null) => None,
                Some(n) => match n.as_i64().and_then(|n| i32::try_from(n).ok()) {
                    Some(n) => Some(n),
                    None => return Some(ToolOutcome::error("number must be a whole number")),
                },
            };
            let doc = format!(
                "mutation McpFinalize($id: ID!, $date: String!, $number: Int, $due: String) {{ \
                 finalizeInvoice(invoiceId: $id, issueDate: $date, number: $number, dueDate: $due) \
                 {{ {INVOICE_DETAIL_FIELDS} }} }}"
            );
            rename(
                ctx.run(
                    &doc,
                    json!({
                        "id": id,
                        "date": date,
                        "number": number,
                        "due": string_arg(arguments, "dueDate"),
                    }),
                )
                .await,
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
        "record_invoice_payment" => {
            let (Some(id), Some(date), Some(amount)) = (
                string_arg(arguments, "invoiceId"),
                string_arg(arguments, "date"),
                arguments.get("amountCents").and_then(Value::as_i64),
            ) else {
                return Some(missing_argument("invoiceId/date/amountCents"));
            };
            let doc = format!(
                "mutation McpPay($id: ID!, $input: RecordPaymentInput!) {{ \
                 recordInvoicePayment(invoiceId: $id, input: $input) {{ {INVOICE_DETAIL_FIELDS} }} }}"
            );
            let input = json!({ "date": date, "amountCents": amount, "note": string_arg(arguments, "note") });
            rename(
                ctx.run(&doc, json!({ "id": id, "input": input })).await,
                "recordInvoicePayment",
                "invoice",
            )
        }
        "delete_invoice_payment" => {
            let (Some(id), Some(payment_id)) = (
                string_arg(arguments, "invoiceId"),
                string_arg(arguments, "paymentId"),
            ) else {
                return Some(missing_argument("invoiceId/paymentId"));
            };
            let doc = format!(
                "mutation McpUnpay($id: ID!, $p: ID!) {{ \
                 deleteInvoicePayment(invoiceId: $id, paymentId: $p) {{ {INVOICE_DETAIL_FIELDS} }} }}"
            );
            rename(
                ctx.run(&doc, json!({ "id": id, "p": payment_id })).await,
                "deleteInvoicePayment",
                "invoice",
            )
        }
        "send_invoice" | "send_credit_note" => {
            let (arg, field, fields, out) = if name == "send_invoice" {
                (
                    "invoiceId",
                    "sendInvoice",
                    INVOICE_SUMMARY_FIELDS,
                    "invoice",
                )
            } else {
                (
                    "creditNoteId",
                    "sendCreditNote",
                    CREDIT_NOTE_FIELDS,
                    "creditNote",
                )
            };
            let Some(id) = string_arg(arguments, arg) else {
                return Some(missing_argument(arg));
            };
            let input = json!({
                "to": id_list(arguments, "to").unwrap_or_default(),
                "cc": id_list(arguments, "cc").unwrap_or_default(),
                "message": string_arg(arguments, "message"),
            });
            let doc = format!(
                "mutation McpSend($id: ID!, $input: SendDocumentInput) {{ \
                 {field}({arg}: $id, input: $input) {{ {fields} }} }}"
            );
            rename(
                ctx.run(&doc, json!({ "id": id, "input": input })).await,
                field,
                out,
            )
        }
        "issue_credit_note" => {
            let (Some(id), Some(date), Some(reason)) = (
                string_arg(arguments, "invoiceId"),
                string_arg(arguments, "issueDate"),
                string_arg(arguments, "reason"),
            ) else {
                return Some(missing_argument("invoiceId/issueDate/reason"));
            };
            let doc = format!(
                "mutation McpCredit($id: ID!, $input: CreditNoteInput!) {{ \
                 issueCreditNote(invoiceId: $id, input: $input) {{ {CREDIT_NOTE_FIELDS} }} }}"
            );
            let lines = arguments.get("lines").filter(|l| l.is_array()).cloned();
            let input = json!({ "issueDate": date, "reason": reason, "lines": lines });
            rename(
                ctx.run(&doc, json!({ "id": id, "input": input })).await,
                "issueCreditNote",
                "creditNote",
            )
        }
        "list_credit_notes" => {
            let Some(instance_id) = string_arg(arguments, "instanceId") else {
                return Some(missing_argument("instanceId"));
            };
            let doc = format!(
                "query McpCreditNotes($instanceId: ID!, $invoiceId: ID, $first: Int!, $after: String) {{ \
                 creditNotes(instanceId: $instanceId, invoiceId: $invoiceId, first: $first, after: $after) {{ \
                 edges {{ node {{ {CREDIT_NOTE_FIELDS} }} }} pageInfo {{ hasNextPage endCursor }} }} }}"
            );
            unwrap_connection(
                ctx.run(
                    &doc,
                    json!({
                        "instanceId": instance_id,
                        "invoiceId": string_arg(arguments, "invoiceId"),
                        "first": page_size(arguments),
                        "after": string_arg(arguments, "after"),
                    }),
                )
                .await,
                "creditNotes",
                "creditNotes",
            )
        }
        "get_credit_note_pdf_url" => {
            let Some(id) = string_arg(arguments, "creditNoteId") else {
                return Some(missing_argument("creditNoteId"));
            };
            rename(
                ctx.run(
                    "mutation McpCreditPdf($id: ID!) { downloadCreditNotePdf(creditNoteId: $id) }",
                    json!({ "id": id }),
                )
                .await,
                "downloadCreditNotePdf",
                "url",
            )
        }
        "rebill_expense" => {
            let Some(id) = string_arg(arguments, "expenseId") else {
                return Some(missing_argument("expenseId"));
            };
            let markup = match arguments.get("markupPercent") {
                Some(Value::String(s)) => Some(s.clone()),
                Some(Value::Number(n)) => Some(n.to_string()),
                _ => None,
            };
            let doc = format!(
                "mutation McpRebill($id: ID!, $markup: String, $description: String, $gstFree: Boolean!) {{ \
                 rebillExpense(expenseId: $id, markupPercent: $markup, description: $description, \
                 gstFree: $gstFree) {{ {ITEM_FIELDS} }} }}"
            );
            rename(
                ctx.run(
                    &doc,
                    json!({
                        "id": id,
                        "markup": markup,
                        "description": string_arg(arguments, "description"),
                        "gstFree": arguments.get("gstFree").and_then(Value::as_bool).unwrap_or(false),
                    }),
                )
                .await,
                "rebillExpense",
                "item",
            )
        }
        "get_gst_report" => {
            let (Some(instance_id), Some(from), Some(to), Some(basis)) = (
                string_arg(arguments, "instanceId"),
                string_arg(arguments, "from"),
                string_arg(arguments, "to"),
                string_arg(arguments, "basis"),
            ) else {
                return Some(missing_argument("instanceId/from/to/basis"));
            };
            rename(
                ctx.run(
                    "query McpGst($i: ID!, $from: String!, $to: String!, $basis: ReportBasisType!) { \
                     gstReport(instanceId: $i, from: $from, to: $to, basis: $basis) { from to basis \
                     gstRegistered currency salesCents gstOnSalesCents purchasesCents \
                     gstOnPurchasesCents netGstCents invoiceCount creditNoteCount paymentCount \
                     expenseCount } }",
                    json!({ "i": instance_id, "from": from, "to": to, "basis": basis }),
                )
                .await,
                "gstReport",
                "report",
            )
        }
        "get_receivables" => {
            let Some(instance_id) = string_arg(arguments, "instanceId") else {
                return Some(missing_argument("instanceId"));
            };
            rename(
                ctx.run(
                    "query McpReceivables($i: ID!) { receivables(instanceId: $i) { asOf currency \
                     totalCents currentCents days1To30Cents days31To60Cents days61To90Cents \
                     daysOver90Cents invoices { id displayNumber issueDate dueDate daysOverdue \
                     totalCents balanceCents project { id name clientName } } } }",
                    json!({ "i": instance_id }),
                )
                .await,
                "receivables",
                "receivables",
            )
        }
        "export_csv" => {
            let (Some(instance_id), Some(kind), Some(from), Some(to)) = (
                string_arg(arguments, "instanceId"),
                string_arg(arguments, "kind"),
                string_arg(arguments, "from"),
                string_arg(arguments, "to"),
            ) else {
                return Some(missing_argument("instanceId/kind/from/to"));
            };
            rename(
                ctx.run(
                    "query McpExport($i: ID!, $kind: CsvExportType!, $from: String!, $to: String!) { \
                     invoicingExport(instanceId: $i, kind: $kind, from: $from, to: $to) }",
                    json!({ "i": instance_id, "kind": kind, "from": from, "to": to }),
                )
                .await,
                "invoicingExport",
                "csv",
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
        "clientEmail": string_arg(arguments, "clientEmail"),
        "reference": string_arg(arguments, "reference"),
        "paymentTermsDays": arguments.get("paymentTermsDays").and_then(Value::as_i64),
        "defaultUnitPriceCents": arguments.get("defaultUnitPriceCents").and_then(Value::as_i64),
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
    // A number field: an explicit `null` clears it, absent keeps it.
    let pick_number = |field: &str| -> Value {
        match arguments.get(field) {
            Some(v) if v.is_null() || v.is_i64() => v.clone(),
            _ => current[field].clone(),
        }
    };
    let input = json!({
        "name": arguments.get("name").and_then(Value::as_str).map(Value::from).unwrap_or_else(|| current["name"].clone()),
        "clientName": arguments.get("clientName").and_then(Value::as_str).map(Value::from).unwrap_or_else(|| current["clientName"].clone()),
        "clientAbn": pick("clientAbn"),
        "clientAddress": pick("clientAddress"),
        "reference": pick("reference"),
        "clientEmail": pick("clientEmail"),
        "paymentTermsDays": pick_number("paymentTermsDays"),
        "defaultUnitPriceCents": pick_number("defaultUnitPriceCents"),
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
    let (Some(project_id), Some(date), Some(description), Some(quantity)) = (
        string_arg(arguments, "projectId"),
        string_arg(arguments, "date"),
        string_arg(arguments, "description"),
        quantity_arg(arguments),
    ) else {
        return missing_argument("projectId/date/description/quantity");
    };
    let price = arguments.get("unitPriceCents").and_then(Value::as_i64);
    let doc = format!(
        "mutation McpCreateItem($projectId: ID!, $input: BillableItemInput!) {{ \
         createBillableItem(projectId: $projectId, input: $input) {{ {ITEM_FIELDS} }} }}"
    );
    let input = json!({
        "date": date, "description": description, "quantity": quantity, "unitPriceCents": price,
        "gstFree": arguments.get("gstFree").and_then(Value::as_bool).unwrap_or(false),
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
    let (Some(id), Some(date), Some(description), Some(quantity)) = (
        string_arg(arguments, "id"),
        string_arg(arguments, "date"),
        string_arg(arguments, "description"),
        quantity_arg(arguments),
    ) else {
        return missing_argument("id/date/description/quantity");
    };
    let price = arguments.get("unitPriceCents").and_then(Value::as_i64);
    let doc = format!(
        "mutation McpUpdateItem($id: ID!, $input: BillableItemInput!) {{ \
         updateBillableItem(id: $id, input: $input) {{ {ITEM_FIELDS} }} }}"
    );
    let input = json!({
        "date": date, "description": description, "quantity": quantity, "unitPriceCents": price,
        "gstFree": arguments.get("gstFree").and_then(Value::as_bool).unwrap_or(false),
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
            "rebilledItem": { "type": ["object", "null"], "description": "The billable item rebill_expense made from it." },
            "receipt": { "type": ["object", "null"], "description": "The uploaded receipt's filename/contentType/size." },
        },
    })
}

fn credit_note_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "displayNumber": { "type": "string", "description": "e.g. \"CN-001\"." },
            "issueDate": { "type": "string" },
            "reason": { "type": "string" },
            "title": { "type": "string", "description": "\"Adjustment Note\" or \"Credit Note\"." },
            "subtotalCents": { "type": "integer" },
            "gstCents": { "type": "integer" },
            "totalCents": { "type": "integer", "description": "GST-inclusive amount taken off the invoice's balance." },
            "currency": { "type": "string" },
            "invoice": { "type": "object" },
            "lines": { "type": "array" },
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
