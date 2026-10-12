//! `MutationRoot`: email-code login, opaque token issuance/revocation,
//! passkeys, and instance-scoped API token management. Ported from
//! seslogin's `graphql/mutations.rs` (the auth block around its lines
//! 360-570, and the passkey block around 2146-2560), trimmed to Toolbox's
//! principals — no kiosk sessions. `createApiToken`/`updateApiToken`/
//! `deleteApiToken` below are a later addition (see `auth::AuthInfo::ApiToken`'s
//! doc comment) with no seslogin equivalent to port from.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use async_graphql::{Context, ID, Object, SimpleObject};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use tracing::{info, warn};

use crate::app::{App, HasDb, HasMail, HasStorage};
use crate::auth::{self, AuthInfo};
use crate::db;
use crate::db::Handler as _;
use crate::inbound::{attachments, routing};
use crate::invoicing;
use crate::mail::Handler as _;
use crate::outbound;
use crate::staff_notify::{self, Actor, StaffEvent};
use crate::storage::Handler as _;

use super::auth::{AuthGuard, AuthRequirement, is_member, is_owner};
use super::error::ApiError;
use super::finance::{CreditNote, CreditNoteInput, RecordPaymentInput, SendDocumentInput};
use super::query::{
    ApiTokenInfo, BillableItem, BillableItemInput, CreateProjectInput, CreatedApiToken, Expense,
    ExpenseInput, InboundAddressInfo, Instance, InstanceKindType, Invoice, InvoicingSettingsInput,
    MembershipInfo, MembershipRoleType, NotificationSettingsInput, PasskeyInfo, Project, Ticket,
    TicketMessage, TicketStatusType, UpdateProjectInput, User,
};

/// One code per address per this many seconds — cheap anti-spam for
/// `requestAuthCode`, independent of the code's own 10-minute validity.
const LOGIN_CODE_RATE_LIMIT_S: u64 = 30;
/// A code is burned (deleted, forcing a fresh `requestAuthCode`) after this many
/// wrong guesses.
const LOGIN_CODE_MAX_ATTEMPTS: u64 = 5;
/// Decimal digits in a login code.
const LOGIN_CODE_DIGITS: u32 = 6;
/// Passkeys a single user may register. Matches seslogin's cap; re-checked after
/// the WebAuthn ceremony completes (see `finish_passkey_registration`) to close
/// the race between the pre-check and the write.
const MAX_PASSKEYS_PER_USER: usize = 10;
/// `submitVerifiedTicket`'s combined `to`+`cc` cap, after normalization and
/// dedup. An integration token is long-lived and unattended — there's no
/// human in the loop to notice a misbehaving caller — so this is what stops
/// a leaked (or simply buggy) token from turning this mutation into a bulk
/// mail relay via one ticket's requester/CC lists.
const MAX_VERIFIED_TICKET_RECIPIENTS: usize = 20;
/// `createApiToken`/`updateApiToken`'s `name` length cap — generous for a
/// human-chosen label ("Zendesk sync", "Partner portal"), not meant to be a
/// meaningful constraint in practice.
const MAX_API_TOKEN_NAME_LEN: usize = 100;
/// Length cap for a short invoicing-settings/project field (name, ABN,
/// phone, email, reference) — see the build plan's validation-limits entry.
const MAX_INVOICING_FIELD_LEN: usize = 200;
/// Length cap for a long, multi-line invoicing-settings/project field
/// (address, payment details).
const MAX_INVOICING_LONG_FIELD_LEN: usize = 2000;
/// Alias of [`MAX_INVOICING_FIELD_LEN`] for project fields — same limit,
/// named for the call site it reads at.
const MAX_PROJECT_FIELD_LEN: usize = MAX_INVOICING_FIELD_LEN;
/// Alias of [`MAX_INVOICING_LONG_FIELD_LEN`] for project fields.
const MAX_PROJECT_LONG_FIELD_LEN: usize = MAX_INVOICING_LONG_FIELD_LEN;
/// Most items one invoice may carry — see CLAUDE.md's "Invoicing" house
/// rule ("Validation limits"): a `TransactWriteItems` call caps out at 100
/// actions, and one item is always taken by the invoice row itself.
const MAX_INVOICE_ITEMS: usize = 50;

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

/// The calling `AuthInfo::User`'s id, or a `Forbidden` error for anything else
/// (no credentials, or a `Requester` capability token — passkeys belong to
/// users, not to the public submit flow).
fn require_user_id(ctx: &Context<'_>) -> Result<String> {
    match ctx.data_opt::<AuthInfo>() {
        Some(AuthInfo::User { id, .. }) => Ok(id.clone()),
        _ => Err(ApiError::forbidden("Must be authenticated as a user").into()),
    }
}

/// The per-ticket authorization check every ticket mutation uses: fetch the
/// ticket by id, then verify the caller is a member of *its* `instance_id`.
///
/// This is deliberately not "check the caller is a member of an `instanceId`
/// argument" — these mutations take only `ticketId`, and a ticket's instance
/// is a fact of the record, not something the caller gets to assert. Checking
/// an `instanceId` argument instead (or trusting one, if a resolver even had
/// one to check) would let a member of instance A act on instance B's ticket
/// simply by passing B's ticket id; fetching the record and checking *its*
/// `instance_id` closes that off structurally. A ticket that doesn't exist,
/// and a ticket that exists but belongs to an instance the caller isn't a
/// member of, are reported identically — `NOT_FOUND` — so this can't be used
/// to probe which ticket ids exist in another instance.
async fn require_ticket_member<A: App + HasDb + HasMail + Send + Sync>(
    ctx: &Context<'_>,
    app: &A,
    ticket_id: &str,
) -> Result<db::Ticket> {
    let ticket = app
        .db()
        .get_tickets(&[ticket_id])
        .await?
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| ApiError::not_found("Ticket", ticket_id))?;
    let Some(AuthInfo::User { memberships, .. }) = ctx.data_opt::<AuthInfo>() else {
        return Err(ApiError::forbidden("Must be authenticated as a user").into());
    };
    if !is_member(memberships, &ticket.instance_id) {
        return Err(ApiError::not_found("Ticket", ticket_id).into());
    }
    Ok(ticket)
}

/// The per-record authorization check `updateApiToken`/`deleteApiToken` use —
/// they take only a token `id` (never an `instanceId`), so, like
/// [`require_ticket_member`], authorization has to happen inside the
/// resolver body after fetching the row, not in a static `#[graphql(guard)]`.
/// Both mutations are declared with **no** static guard at all (`Authenticated`
/// would admit a `Requester`, which must never manage tokens), so this is the
/// entire authorization story for both, checked in this order:
///
/// 1. The caller isn't an `AuthInfo::User` at all → `FORBIDDEN`, checked
///    *before* the fetch so a non-user caller learns nothing about whether
///    `id` exists.
/// 2. The row doesn't exist → `NOT_FOUND`.
/// 3. The caller is neither a superuser nor an owner of the row's
///    `instance_id` → `NOT_FOUND` — the same "missing and unauthorized look
///    identical" posture as [`require_ticket_member`], so this can't be used
///    to probe which token ids exist in another instance.
async fn require_api_token_manager<A: App + HasDb + Send + Sync>(
    ctx: &Context<'_>,
    app: &A,
    id: &str,
) -> Result<db::ApiToken> {
    let Some(AuthInfo::User {
        memberships,
        is_superuser,
        ..
    }) = ctx.data_opt::<AuthInfo>()
    else {
        return Err(ApiError::forbidden("Must be authenticated as a user").into());
    };
    let token = app
        .db()
        .get_api_token(id)
        .await?
        .ok_or_else(|| ApiError::not_found("ApiToken", id))?;
    if !*is_superuser && !is_owner(memberships, &token.instance_id) {
        return Err(ApiError::not_found("ApiToken", id).into());
    }
    Ok(token)
}

/// The per-record authorization check `updateProject` uses — it takes only a
/// project `id` (never an `instanceId`), so, like [`require_ticket_member`]/
/// [`require_api_token_manager`], authorization happens inside the resolver
/// body after fetching the row, not in a static `#[graphql(guard)]`. A
/// project that doesn't exist, and one that exists but belongs to an
/// instance the caller isn't a member of, are reported identically —
/// `NOT_FOUND` — so this can't be used to probe another instance's projects.
/// Superusers get **no** access here — they don't pass `is_member` either,
/// matching the existing superuser boundary (CLAUDE.md): a superuser has no
/// implicit access to any instance's projects/items/invoices.
async fn require_project_member<A: App + HasDb + Send + Sync>(
    ctx: &Context<'_>,
    app: &A,
    id: &str,
) -> Result<db::Project> {
    // Checked before the fetch, so a non-user caller learns nothing about
    // whether `id` exists.
    let Some(AuthInfo::User { memberships, .. }) = ctx.data_opt::<AuthInfo>() else {
        return Err(ApiError::forbidden("Must be authenticated as a user").into());
    };
    let project = app
        .db()
        .get_projects(&[id])
        .await?
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| ApiError::not_found("Project", id))?;
    if !is_member(memberships, &project.instance_id) {
        return Err(ApiError::not_found("Project", id).into());
    }
    Ok(project)
}

/// Trim and length-check a required project field (`name`/`clientName`) —
/// `createProject`/`updateProject`'s validation for the two non-optional
/// fields. Matches [`MAX_PROJECT_FIELD_LEN`].
fn require_project_name(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("name cannot be empty"));
    }
    if trimmed.chars().count() > MAX_PROJECT_FIELD_LEN {
        return Err(anyhow!(
            "name cannot be longer than {MAX_PROJECT_FIELD_LEN} characters"
        ));
    }
    Ok(trimmed.to_string())
}

fn require_project_client_name(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("clientName cannot be empty"));
    }
    if trimmed.chars().count() > MAX_PROJECT_FIELD_LEN {
        return Err(anyhow!(
            "clientName cannot be longer than {MAX_PROJECT_FIELD_LEN} characters"
        ));
    }
    Ok(trimmed.to_string())
}

/// Trim an optional project field (`clientAbn`/`clientAddress`/`reference`);
/// a blank result is `None` (the caller `REMOVE`s the attribute), matching
/// the omit-optional-attributes house rule. `field_name` names the argument
/// in the error message; `max_len` is [`MAX_PROJECT_FIELD_LEN`] or
/// [`MAX_PROJECT_LONG_FIELD_LEN`] depending on the field.
fn normalize_project_field(
    raw: Option<&str>,
    field_name: &str,
    max_len: usize,
) -> Result<Option<String>> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > max_len {
        return Err(anyhow!(
            "{field_name} cannot be longer than {max_len} characters"
        ));
    }
    Ok(Some(trimmed.to_string()))
}

/// Trim an invoicing-settings string field; a blank result is `None` (the
/// caller `REMOVE`s the attribute), matching the omit-optional-attributes
/// house rule — `updateInvoicingSettings` is a full replace, so a field the
/// caller leaves blank is explicitly cleared, not left alone. Capped at
/// [`MAX_INVOICING_FIELD_LEN`].
fn normalize_invoicing_field(raw: &str, field_name: &str) -> Result<Option<String>> {
    normalize_project_field(Some(raw), field_name, MAX_INVOICING_FIELD_LEN)
}

/// Same as [`normalize_invoicing_field`], for the two long/multi-line fields
/// (`businessAddress`/`paymentDetails`), capped at
/// [`MAX_INVOICING_LONG_FIELD_LEN`].
fn normalize_invoicing_long_field(raw: &str, field_name: &str) -> Result<Option<String>> {
    normalize_project_field(Some(raw), field_name, MAX_INVOICING_LONG_FIELD_LEN)
}

/// `updateBillableItem`/`deleteBillableItem`'s per-record authorization —
/// the same shape as [`require_project_member`]: user check before the
/// fetch, then `NOT_FOUND` for a missing item and for one in an instance the
/// caller isn't a member of alike. Superusers get no access (they don't
/// pass `is_member`).
async fn require_billable_item_member<A: App + HasDb + Send + Sync>(
    ctx: &Context<'_>,
    app: &A,
    id: &str,
) -> Result<db::BillableItem> {
    let Some(AuthInfo::User { memberships, .. }) = ctx.data_opt::<AuthInfo>() else {
        return Err(ApiError::forbidden("Must be authenticated as a user").into());
    };
    let item = app
        .db()
        .get_billable_items(&[id])
        .await?
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| ApiError::not_found("BillableItem", id))?;
    if !is_member(memberships, &item.instance_id) {
        return Err(ApiError::not_found("BillableItem", id).into());
    }
    Ok(item)
}

/// `updateExpense`/`deleteExpense`'s per-record authorization — the same
/// shape as [`require_billable_item_member`]: `NOT_FOUND` for a missing
/// expense and for one in an instance the caller isn't a member of alike.
/// Superusers get no access.
async fn require_expense_member<A: App + HasDb + Send + Sync>(
    ctx: &Context<'_>,
    app: &A,
    id: &str,
) -> Result<db::Expense> {
    let Some(AuthInfo::User { memberships, .. }) = ctx.data_opt::<AuthInfo>() else {
        return Err(ApiError::forbidden("Must be authenticated as a user").into());
    };
    let expense = app
        .db()
        .get_expenses(&[id])
        .await?
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| ApiError::not_found("Expense", id))?;
    if !is_member(memberships, &expense.instance_id) {
        return Err(ApiError::not_found("Expense", id).into());
    }
    Ok(expense)
}

/// Validate an [`ExpenseInput`] for `instance_id`. A `projectId` must be a
/// project in the same instance (`NOT_FOUND` otherwise, like
/// `billableItems`' filter), and an archived project takes no new expenses
/// — unless `current_project_id` (the expense's project before this
/// update) is that same project, so editing an expense on a since-archived
/// job still works.
async fn validate_expense<A: App + HasDb + Send + Sync>(
    app: &A,
    instance_id: &str,
    input: &ExpenseInput,
    current_project_id: Option<&str>,
) -> Result<db::ExpenseFields> {
    if let Some(project_id) = &input.project_id {
        let project = app
            .db()
            .get_projects(&[project_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .filter(|p| p.instance_id == instance_id)
            .ok_or_else(|| ApiError::not_found("Project", project_id.as_str()))?;
        if project.archived && current_project_id != Some(project.id.as_str()) {
            return Err(anyhow!(
                "Project is archived — un-archive it to add expenses"
            ));
        }
    }
    invoicing::expense::validate_expense_input(invoicing::expense::ExpenseInput {
        project_id: input.project_id.as_ref().map(|p| p.to_string()),
        date: &input.date,
        category: Some(input.category.into()),
        description: input.description.as_deref(),
        supplier: input.supplier.as_deref(),
        amount_cents: input.amount_cents,
        gst_cents: input.gst_cents,
        distance_km: input.distance_km.as_deref(),
    })
    .map_err(|e| anyhow!(e))
}

/// The per-record authorization check every invoice mutation uses — the
/// same shape as [`require_project_member`]/[`require_billable_item_member`]:
/// user check before the fetch, then `NOT_FOUND` for a missing invoice and
/// for one in an instance the caller isn't a member of alike. Superusers
/// get no access (they don't pass `is_member`). Uses
/// [`db::Handler::get_invoice_consistent`], not the (eventually consistent)
/// dataloader path — every caller conditions a subsequent write on the
/// `status`/`version` this returns.
async fn require_invoice_member<A: App + HasDb + Send + Sync>(
    ctx: &Context<'_>,
    app: &A,
    id: &str,
) -> Result<db::Invoice> {
    let Some(AuthInfo::User { memberships, .. }) = ctx.data_opt::<AuthInfo>() else {
        return Err(ApiError::forbidden("Must be authenticated as a user").into());
    };
    let invoice = app
        .db()
        .get_invoice_consistent(id)
        .await?
        .ok_or_else(|| ApiError::not_found("Invoice", id))?;
    if !is_member(memberships, &invoice.instance_id) {
        return Err(ApiError::not_found("Invoice", id).into());
    }
    Ok(invoice)
}

/// `finalizeInvoice`'s `CONFLICT` for a number another invoice in the
/// instance already has.
fn invoice_number_used(number: u32) -> ApiError {
    ApiError::conflict(format!(
        "Invoice number {number} is already used in this instance"
    ))
}

/// Validate a set of billable-item ids to add to an invoice
/// (`createInvoice`'s `itemIds`, or `addInvoiceItems`'): at least one, no
/// duplicates against each other or `existing` (the invoice's current
/// `item_ids` — empty for `createInvoice`), and the combined total capped at
/// [`MAX_INVOICE_ITEMS`].
fn validate_new_invoice_item_ids(ids: &[ID], existing: &[String]) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Err(anyhow!("At least one item id is required"));
    }
    if existing.len() + ids.len() > MAX_INVOICE_ITEMS {
        return Err(anyhow!(
            "An invoice can have at most {MAX_INVOICE_ITEMS} items"
        ));
    }
    let mut seen: HashSet<String> = existing.iter().cloned().collect();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let s = id.as_str().to_string();
        if !seen.insert(s.clone()) {
            return Err(anyhow!("Item {s} is duplicated"));
        }
        out.push(s);
    }
    Ok(out)
}

/// Validate a set of billable-item ids to remove from an invoice
/// (`removeInvoiceItems`): at least one, no duplicates, and every id must
/// currently be on `current` (the invoice's own `item_ids`) — a clearer
/// error than letting the transaction's own condition fail on an id that
/// was never on this invoice.
fn validate_remove_invoice_item_ids(ids: &[ID], current: &[String]) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Err(anyhow!("At least one item id is required"));
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let s = id.as_str().to_string();
        if !seen.insert(s.clone()) {
            return Err(anyhow!("Item {s} is duplicated"));
        }
        if !current.iter().any(|x| x == &s) {
            return Err(anyhow!("Item {s} is not on this invoice"));
        }
        out.push(s);
    }
    Ok(out)
}

/// Fetch and validate a set of billable items being attached to an invoice
/// (`createInvoice`/`addInvoiceItems`): every id must resolve to a row that
/// belongs to `project_id` and is currently unbilled. `NOT_FOUND` for a
/// missing id (it never existed, or belongs to another instance entirely);
/// `CONFLICT` for one that exists but isn't eligible (wrong project, or
/// already on an invoice) — this is a pre-check for a clear message, not
/// the sole guard: the transaction's own per-item condition re-checks both
/// atomically.
async fn fetch_eligible_items<A: App + HasDb + Send + Sync>(
    app: &A,
    project_id: &str,
    ids: &[String],
) -> Result<Vec<db::BillableItem>> {
    let fetched = app.db().get_billable_items(ids).await?;
    let mut items = Vec::with_capacity(ids.len());
    for (id, item) in ids.iter().zip(fetched) {
        let item = item.ok_or_else(|| ApiError::not_found("BillableItem", id))?;
        if item.project_id != project_id {
            return Err(ApiError::conflict(format!(
                "Billable item {id} does not belong to this invoice's project"
            ))
            .into());
        }
        if item.invoice_id.is_some() {
            return Err(
                ApiError::conflict(format!("Billable item {id} is already on an invoice")).into(),
            );
        }
        items.push(item);
    }
    Ok(items)
}

/// Reject a prospective invoice total (subtotal + GST, in cents) that would
/// exceed `Number.MAX_SAFE_INTEGER` — see CLAUDE.md's "Invoicing" house
/// rule ("Totals overflow") and `invoicing::validate_total_within_safe_integer`.
fn check_invoice_total_within_safe_integer(
    items: &[db::BillableItem],
    gst_registered: bool,
) -> Result<()> {
    let subtotal: i64 = items
        .iter()
        .map(|it| invoicing::money::line_amount_cents(it.quantity_hundredths, it.unit_price_cents))
        .sum();
    let gst = if gst_registered {
        invoicing::money::gst_cents(subtotal)
    } else {
        0
    };
    let total = subtotal.saturating_add(gst);
    invoicing::validate_total_within_safe_integer(total).map_err(|e| anyhow!(e))
}

/// A [`BillableItemInput`] after validation — canonical date, trimmed
/// description, quantity in hundredths.
struct ValidBillableItem {
    date: String,
    description: String,
    quantity_hundredths: i64,
    unit_price_cents: i64,
}

/// `unit_price_cents` is the price the caller resolved — the input's own,
/// else the project's default (create) or the item's current one (update).
fn validate_billable_item_input(
    input: &BillableItemInput,
    unit_price_cents: i64,
) -> Result<ValidBillableItem> {
    Ok(ValidBillableItem {
        date: invoicing::validate_item_date(&input.date).map_err(|e| anyhow!(e))?,
        description: invoicing::validate_description(&input.description).map_err(|e| anyhow!(e))?,
        quantity_hundredths: invoicing::money::parse_quantity(&input.quantity)
            .map_err(|e| anyhow!(e))?,
        unit_price_cents: invoicing::validate_unit_price_cents(unit_price_cents)
            .map_err(|e| anyhow!(e))?,
    })
}

/// `createProject`/`updateProject`'s raw fields, before validation.
struct ProjectFieldsInput<'a> {
    name: &'a str,
    client_name: &'a str,
    client_abn: Option<&'a str>,
    client_address: Option<&'a str>,
    client_email: Option<&'a str>,
    reference: Option<&'a str>,
    payment_terms_days: Option<i32>,
    default_unit_price_cents: Option<i64>,
}

/// Trim and validate every project field; blank optional strings become
/// `None` (absent / `REMOVE`d).
fn validate_project_fields(input: ProjectFieldsInput<'_>) -> Result<db::ProjectFields> {
    let client_email =
        normalize_project_field(input.client_email, "clientEmail", MAX_PROJECT_FIELD_LEN)?
            .map(|e| db::normalize_user_email(&e).map_err(|e| anyhow!(e)))
            .transpose()?;
    Ok(db::ProjectFields {
        name: require_project_name(input.name)?,
        client_name: require_project_client_name(input.client_name)?,
        client_abn: normalize_project_field(input.client_abn, "clientAbn", MAX_PROJECT_FIELD_LEN)?,
        client_address: normalize_project_field(
            input.client_address,
            "clientAddress",
            MAX_PROJECT_LONG_FIELD_LEN,
        )?,
        client_email,
        reference: normalize_project_field(input.reference, "reference", MAX_PROJECT_FIELD_LEN)?,
        payment_terms_days: input
            .payment_terms_days
            .map(invoicing::validate_payment_terms_days)
            .transpose()
            .map_err(|e| anyhow!(e))?,
        default_unit_price_cents: input
            .default_unit_price_cents
            .map(invoicing::validate_unit_price_cents)
            .transpose()
            .map_err(|e| anyhow!(e))?,
    })
}

/// Longest credit-note reason, in characters.
const MAX_CREDIT_NOTE_REASON_LEN: usize = 500;

/// Largest receipt `attachExpenseReceipt` accepts.
const MAX_RECEIPT_BYTES: u64 = 20 * 1024 * 1024;

/// A new payment row, stamped with who recorded it and when.
fn new_payment(
    date: &str,
    amount_cents: i64,
    note: Option<String>,
    user_id: &str,
) -> db::InvoicePayment {
    db::InvoicePayment {
        id: crate::dynamodb::new_id(),
        date: date.to_string(),
        amount_cents,
        note,
        recorded_by_user_id: user_id.to_string(),
        recorded_at: crate::clock::now_sec(),
    }
}

/// Write a finalized invoice's new `payments` list, recomputing `paid_date`
/// (`invoicing::ledger::settled_date`) in the same conditional write, and
/// return the updated invoice. An invoice that was already settled keeps
/// its `paid_date` if it still is; one newly settled is dated by its latest
/// payment. `CONFLICT` if it changed since `invoice` was read.
async fn write_payments<A: App + HasDb + Send + Sync>(
    app: &A,
    invoice: db::Invoice,
    payments: Vec<db::InvoicePayment>,
) -> Result<Invoice<A>> {
    let fallback = invoice.paid_date.clone().unwrap_or_default();
    let paid_date = invoicing::ledger::settled_date(
        invoice.total_cents.unwrap_or(0),
        invoice.credited_cents,
        &payments,
        &fallback,
    )
    .map(|d| match &invoice.paid_date {
        Some(existing) => existing.clone(),
        None => d,
    })
    // Settled by credits alone with nothing to date it by.
    .map(|d| {
        if d.is_empty() {
            invoicing::today_utc()
        } else {
            d
        }
    });
    let committed = app
        .db()
        .set_invoice_payments(
            &invoice.id,
            invoice.version,
            &payments,
            paid_date.as_deref(),
        )
        .await?;
    if !committed {
        return Err(
            ApiError::conflict("Invoice changed concurrently — reload and try again").into(),
        );
    }
    Ok(Invoice::new(db::Invoice {
        payments,
        paid_date,
        version: invoice.version + 1,
        updated_at: crate::clock::now_sec(),
        ..invoice
    }))
}

/// Resolve `sendInvoice`/`sendCreditNote`'s recipients (an empty `to`
/// means the project's client email) and covering message.
fn resolve_send_input(
    input: &SendDocumentInput,
    client_email: Option<&str>,
) -> Result<(Vec<String>, Vec<String>, Option<String>)> {
    let to = if input.to.is_empty() {
        vec![
            client_email
                .ok_or_else(|| {
                    anyhow!("This project has no client email — add one, or say who to send it to")
                })?
                .to_string(),
        ]
    } else {
        input.to.clone()
    };
    let (to, cc) = invoicing::send::normalize_recipients(&to, &input.cc).map_err(|e| anyhow!(e))?;
    let message =
        invoicing::send::validate_message(input.message.as_deref()).map_err(|e| anyhow!(e))?;
    Ok((to, cc, message))
}

/// A finalized invoice's frozen snapshot.
fn invoice_snapshot(invoice: &db::Invoice) -> Result<invoicing::snapshot::InvoiceSnapshot> {
    let json = invoice
        .snapshot
        .as_deref()
        .ok_or_else(|| anyhow!("Finalized invoice {} is missing its snapshot", invoice.id))?;
    serde_json::from_str(json)
        .map_err(|e| anyhow!("Invoice {} has a corrupt snapshot: {e}", invoice.id))
}

fn credit_note_snapshot(note: &db::CreditNote) -> Result<invoicing::snapshot::InvoiceSnapshot> {
    serde_json::from_str(&note.snapshot)
        .map_err(|e| anyhow!("Credit note {} has a corrupt snapshot: {e}", note.id))
}

/// `Invoice-008.pdf`.
fn invoice_pdf_filename(invoice: &db::Invoice) -> String {
    let display_number = invoice
        .number
        .map(|n| format!("{n:03}"))
        .unwrap_or_else(|| "unknown".to_string());
    format!("Invoice-{display_number}.pdf")
}

/// `Credit-Note-CN-001.pdf`.
fn credit_note_pdf_filename(note: &db::CreditNote) -> String {
    format!("Credit-Note-{}.pdf", note.display_number())
}

/// A frozen document's PDF: the cached object at `cached_key` when there is
/// one, else rendered from `snapshot` and stored at `new_key`. Returns the
/// key and the bytes; the caller records a newly stored key on its row.
async fn document_pdf<A: App + HasStorage + Send + Sync>(
    app: &A,
    cached_key: Option<&str>,
    snapshot: &invoicing::snapshot::InvoiceSnapshot,
    new_key: &str,
) -> Result<(String, Vec<u8>)> {
    if let Some(key) = cached_key {
        return Ok((key.to_string(), app.storage().get_bytes(key).await?));
    }
    let bytes = invoicing::pdf::render_invoice_pdf(snapshot)?;
    app.storage()
        .put_bytes(new_key, &bytes, "application/pdf")
        .await?;
    Ok((new_key.to_string(), bytes))
}

/// The credit-note counterpart of [`require_invoice_member`]: `NOT_FOUND`
/// for missing and not-yours alike; superusers get no access.
async fn require_credit_note_member<A: App + HasDb + Send + Sync>(
    ctx: &Context<'_>,
    app: &A,
    id: &str,
) -> Result<db::CreditNote> {
    let Some(AuthInfo::User { memberships, .. }) = ctx.data_opt::<AuthInfo>() else {
        return Err(ApiError::forbidden("Must be authenticated as a user").into());
    };
    let note = app
        .db()
        .get_credit_note_consistent(id)
        .await?
        .ok_or_else(|| ApiError::not_found("CreditNote", id))?;
    if !is_member(memberships, &note.instance_id) {
        return Err(ApiError::not_found("CreditNote", id).into());
    }
    Ok(note)
}

/// What a re-billed expense's line says by default: the category and
/// supplier (or a trip's distance), then its own description.
fn rebill_description(expense: &db::Expense) -> String {
    let head = match &expense.fields.detail {
        db::ExpenseDetail::Purchase { supplier, .. } => {
            format!("{}: {supplier}", expense.fields.category.label())
        }
        db::ExpenseDetail::VehicleKm {
            distance_tenths_km, ..
        } => format!(
            "Vehicle travel: {} km",
            invoicing::vehicle::format_distance_km(*distance_tenths_km)
        ),
    };
    match expense.fields.description.as_deref() {
        Some(d) if !d.is_empty() => format!("{head}\n{d}"),
        _ => head,
    }
}

/// `receipts/{instance_id}/{expense_id}/` — every receipt key for an
/// expense starts with this, which is what `attachExpenseReceipt` checks.
fn receipt_key_prefix(expense: &db::Expense) -> String {
    format!("receipts/{}/{}/", expense.instance_id, expense.id)
}

/// Move every validated `pending/…` key in `keys` into
/// `attachments/{ticket_id}/{message_id}/{n}/{filename}`, returning the
/// `db::Attachment` records to stamp onto the message row. A key that
/// doesn't belong to `ticket.instance_id`'s pending prefix is rejected
/// outright (`FORBIDDEN`) — see `reply_to_ticket`'s doc comment. A free
/// function, not a method on `MutationRoot`'s `#[Object]` impl — everything
/// in that impl block becomes a GraphQL field, which this must not.
async fn move_attachments<A: App + HasStorage + Send + Sync>(
    app: &A,
    ticket: &db::Ticket,
    message_id: &str,
    keys: Vec<String>,
) -> Result<Vec<db::Attachment>> {
    let mut stored = Vec::with_capacity(keys.len());
    for (index, key) in keys.iter().enumerate() {
        if !attachments::is_pending_key_for_instance(key, &ticket.instance_id) {
            return Err(ApiError::forbidden(format!(
                "attachment key {key:?} is not a pending upload for this ticket's instance"
            ))
            .into());
        }
        let filename = key.rsplit('/').next().unwrap_or("attachment").to_string();
        let content_type = attachments::guess_content_type(&filename).to_string();
        let final_key = attachments::attachment_key(&ticket.id, message_id, index, &filename);
        app.storage().move_object(key, &final_key).await?;
        let size = app.storage().object_size(&final_key).await.unwrap_or(0);
        stored.push(db::Attachment {
            s3_key: final_key,
            filename,
            content_type,
            size,
        });
    }
    Ok(stored)
}

/// Load an instance and its inbound addresses together — the "can't build
/// outbound mail without these" precondition every mail-sending mutation
/// needs — logging and returning `None` on any failure rather than
/// propagating. Only used by [`send_system_notification`]'s best-effort
/// sends (the acknowledgement, a status notice); `reply_to_ticket` does its
/// own non-swallowing lookup, because a reply that can't be built/sent must
/// fail loudly (see that mutation's doc comment), not silently skip.
async fn load_instance_and_addresses<A: App + HasDb + Send + Sync>(
    app: &A,
    instance_id: &str,
    context: &str,
) -> Option<(db::Instance, Vec<db::InboundAddress>)> {
    let instance = match app.db().get_instances(&[instance_id]).await {
        Ok(v) => v.into_iter().next().flatten(),
        Err(e) => {
            warn!("{context}: could not load instance {instance_id}: {e}");
            return None;
        }
    };
    let Some(instance) = instance else {
        warn!("{context}: instance {instance_id} missing");
        return None;
    };
    match app
        .db()
        .list_inbound_addresses_by_instance(&instance.id)
        .await
    {
        Ok(addresses) => Some((instance, addresses)),
        Err(e) => {
            warn!("{context}: could not load inbound addresses for instance {instance_id}: {e}");
            None
        }
    }
}

/// Build, send, and persist (as a `System` `ticket_message`) one
/// best-effort outbound notification: the `submitTicket` acknowledgement or
/// a `setTicketStatus` close/reopen notice. Every failure, at any stage, is
/// logged and swallowed rather than propagated — see those two mutations'
/// doc comments for why a notification failure must never fail a mutation
/// whose primary effect (the ticket exists; its status changed) has already
/// succeeded. Contrast `reply_to_ticket`, where a send failure *is*
/// surfaced: an agent's reply is primary content, not a secondary notice.
async fn send_system_notification<A: App + HasDb + HasMail + Send + Sync>(
    app: &A,
    ticket: &db::Ticket,
    requester_emails: &[String],
    cc_emails: &[String],
    body: &str,
    context: &str,
) {
    let Some((instance, addresses)) =
        load_instance_and_addresses(app, &ticket.instance_id, context).await
    else {
        return;
    };
    let built = match outbound::build_outbound(
        &instance,
        &addresses,
        ticket,
        requester_emails,
        cc_emails,
        body,
        &outbound::Threading::default(),
    ) {
        Ok(b) => b,
        Err(e) => {
            warn!(
                "{context}: could not build notification for ticket {}: {e}",
                ticket.id
            );
            return;
        }
    };
    let message_id = match app.mail().send_raw(&built.raw, &built.to, &built.cc).await {
        Ok(id) => id,
        Err(e) => {
            warn!(
                "{context}: failed to send notification for ticket {}: {e}",
                ticket.id
            );
            return;
        }
    };
    let msg = match app
        .db()
        .create_ticket_message(
            &ticket.id,
            db::TicketMessageKind::System,
            None,
            None,
            &built.to,
            &built.cc,
            Some(body),
            None,
            None,
            None,
        )
        .await
    {
        Ok(m) => m,
        Err(e) => {
            warn!(
                "{context}: notification sent for ticket {} but could not persist the system message: {e}",
                ticket.id
            );
            return;
        }
    };
    if let Err(e) = app
        .db()
        .update_ticket_message(
            &msg.id,
            db::TicketMessageUpdateShape::SetRfcMessageId {
                rfc_message_id: &message_id,
            },
        )
        .await
    {
        warn!(
            "{context}: could not stamp rfc_message_id on notification for ticket {}: {e}",
            ticket.id
        );
    }
}

/// Trim, lowercase, and sanity-check an email address supplied to
/// `addTicketRequester`/`addTicketCc` — a minimal shape check (one `@`,
/// non-empty local/domain parts), not full RFC 5321 validation, matching how
/// little validation `inbound::routing::classify_for_storage` does for the
/// same reason: perfect email validation is famously not worth attempting,
/// and DynamoDB/SES will reject anything that matters more than this does.
fn normalize_ticket_email(raw: &str) -> Result<String> {
    let lower = raw.trim().to_lowercase();
    let Some((local, domain)) = lower.split_once('@') else {
        return Err(anyhow!(
            "{raw:?} is not a valid email address (missing '@')"
        ));
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err(anyhow!("{raw:?} is not a valid email address"));
    }
    Ok(lower)
}

/// Normalize, dedupe, and cap `submitVerifiedTicket`'s `to`/`cc` lists — pure
/// (no DB, no instance), so it's testable on its own. Each address goes
/// through [`normalize_ticket_email`]; `to` keeps first-seen order with
/// duplicates dropped; `cc` does the same, *and* drops anything already
/// present in `to` (a requester doesn't also need to be a CC on their own
/// ticket). `to` must be non-empty after that — a ticket needs at least one
/// requester — and the combined, post-dedup length must not exceed
/// [`MAX_VERIFIED_TICKET_RECIPIENTS`].
///
/// **Does not check an instance's own inbound addresses** — that requires
/// the instance's `inbound_address` rows, which this function has no access
/// to; the caller (`submit_verified_ticket`) runs that check separately
/// after loading the instance, via `outbound::is_own_address`.
fn normalize_verified_recipients(
    to: &[String],
    cc: &[String],
) -> Result<(Vec<String>, Vec<String>)> {
    let mut to_norm = Vec::with_capacity(to.len());
    let mut seen: HashSet<String> = HashSet::new();
    for raw in to {
        let email = normalize_ticket_email(raw)?;
        if seen.insert(email.clone()) {
            to_norm.push(email);
        }
    }
    if to_norm.is_empty() {
        return Err(anyhow!("to must contain at least one address"));
    }

    let mut cc_norm = Vec::with_capacity(cc.len());
    for raw in cc {
        let email = normalize_ticket_email(raw)?;
        // `seen` already holds every `to` address, so this one check both
        // dedupes cc against itself and drops any cc that's also a to.
        if seen.insert(email.clone()) {
            cc_norm.push(email);
        }
    }

    if to_norm.len() + cc_norm.len() > MAX_VERIFIED_TICKET_RECIPIENTS {
        return Err(anyhow!(
            "too many recipients: {} exceeds the limit of {MAX_VERIFIED_TICKET_RECIPIENTS}",
            to_norm.len() + cc_norm.len()
        ));
    }

    Ok((to_norm, cc_norm))
}

/// The shared write path behind `submitTicket` and `submitVerifiedTicket`:
/// allocate the next per-instance ticket number, create the ticket, and
/// store the submitter's own message as the first `ticket_message` (`kind:
/// INBOUND`, `from_email` set, no `author_user_id` — no staff member wrote
/// it). `requester_emails[0]` is treated as the message's `from_email`
/// regardless of how many requesters there are — see
/// `submit_verified_ticket`'s doc comment for why that's the right choice
/// there too, not just for `submitTicket`'s always-exactly-one-requester
/// case this was originally written for.
///
/// Deliberately stops here: sending the acknowledgement and the staff
/// notice, and logging, differ enough between the two callers (a token id
/// to log for one, not the other; matching `context` strings for
/// `send_system_notification`/`notify_staff`) that folding them in here
/// would just move the divergence inside this function instead of removing
/// it.
async fn open_submitted_ticket<A: App + HasDb + Send + Sync>(
    app: &A,
    instance: &db::Instance,
    subject: &str,
    body: &str,
    requester_emails: &[String],
    cc_emails: &[String],
) -> Result<db::Ticket> {
    let number = app.db().increment_ticket_counter(&instance.id).await?;
    let ticket = app
        .db()
        .create_ticket(&instance.id, number, subject, requester_emails, cc_emails)
        .await?;

    app.db()
        .create_ticket_message(
            &ticket.id,
            db::TicketMessageKind::Inbound,
            None,
            Some(&requester_emails[0]),
            &[],
            &[],
            Some(body),
            None,
            None,
            None,
        )
        .await?;

    Ok(ticket)
}

pub struct MutationRoot<A: App + HasDb + HasMail + HasStorage + Send + Sync> {
    pub(super) app: Arc<A>,
}

#[derive(SimpleObject)]
struct PasskeyChallenge {
    challenge_id: String,
    options_json: String,
}

/// A presigned upload slot for `createAttachmentUpload`: `key` is what the
/// client later passes back in `replyToTicket(attachmentKeys:)`;
/// `uploadUrl` is a short-lived presigned PUT the client uploads the file's
/// bytes to directly (never through this API).
#[derive(SimpleObject)]
struct AttachmentUpload {
    key: String,
    upload_url: String,
}

/// `submitVerifiedTicket`'s result — deliberately narrow, not `Ticket<A>`:
/// an integration token has no business reading back ticket internals
/// (assignee, messages, internal notes) through nested fields just because
/// it opened the ticket. `subject_tag` is the `[#{slug}-{number}]` tag
/// `outbound::tag_subject` stamps on every outbound message for this
/// ticket, handed back so the caller can recognize its own ticket in a
/// downstream inbox without a second query.
#[derive(SimpleObject)]
struct SubmittedTicket {
    id: ID,
    number: i64,
    subject_tag: String,
}

#[Object]
impl<A: App + HasDb + HasMail + HasStorage + Send + Sync + 'static> MutationRoot<A> {
    /// Approve an OAuth authorization request: the user has seen the consent
    /// screen (`oauthAuthorizationRequest`) and clicked "Approve". Mints a
    /// single-use authorization code and returns the client's `redirect_uri`
    /// with `code` (and `state`, if given) appended — the caller just navigates
    /// there.
    ///
    /// `User` only (a web session, `mtu_`): an `mtoa_` OAuth token never reaches
    /// GraphQL at all, so an MCP client can't approve further grants for itself.
    /// Re-validates the client id and redirect URI itself rather than trusting
    /// whatever the consent page last read: nothing stops a client from calling
    /// this directly, so every check the query made is repeated here.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    #[allow(clippy::too_many_arguments)]
    async fn approve_oauth_authorization(
        &self,
        ctx: &Context<'_>,
        client_id: String,
        redirect_uri: String,
        code_challenge: String,
        code_challenge_method: String,
        scope: Option<String>,
        resource: Option<String>,
        state: Option<String>,
    ) -> Result<String> {
        let user_id = require_user_id(ctx)?;

        let registration = crate::oauth::client_id_key_from_env()
            .and_then(|key| crate::oauth::decode_client_id(&key, &client_id))
            .ok_or_else(|| anyhow!("Unknown or invalid client"))?;
        if !registration
            .redirect_uris
            .iter()
            .any(|u| u == &redirect_uri)
        {
            return Err(anyhow!("redirect_uri is not registered for this client"));
        }
        if code_challenge_method != "S256" {
            return Err(anyhow!("code_challenge_method must be S256"));
        }
        // RFC 7636 S256 challenges are exactly the base64url (no padding)
        // encoding of a 32-byte SHA-256 digest: always 43 characters.
        if code_challenge.len() != 43
            || !code_challenge
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(anyhow!(
                "code_challenge does not look like a valid S256 challenge"
            ));
        }
        if let Some(scope) = &scope
            && scope != crate::oauth::DEFAULT_SCOPE
        {
            return Err(anyhow!("Unsupported scope: {scope:?}"));
        }

        let now = crate::clock::now_sec();
        let code = crate::nonce::generate_nonce(32);
        let payload = crate::oauth_http::AuthCodePayload {
            user_id,
            client_id: client_id.clone(),
            client_name: registration.client_name,
            redirect_uri: redirect_uri.clone(),
            code_challenge,
            scope,
            resource,
        };
        let payload_json =
            serde_json::to_string(&payload).context("serializing authorization code payload")?;
        self.app
            .db()
            .put_ephemeral_state(
                &crate::oauth_http::oauth_code_state_id(&auth::hash_token(&code)),
                crate::oauth_http::OAUTH_CODE_STATE_KIND,
                &payload_json,
                now + crate::oauth_http::OAUTH_CODE_TTL_S,
            )
            .await?;

        let mut url = url::Url::parse(&redirect_uri)
            .map_err(|_| anyhow!("redirect_uri is not a valid URL"))?;
        url.query_pairs_mut().append_pair("code", &code);
        if let Some(state) = &state {
            url.query_pairs_mut().append_pair("state", state);
        }
        Ok(url.to_string())
    }

    /// Request an email login code. **Always returns `true`**, whether or not the
    /// address belongs to a real, enabled user — telling the caller otherwise
    /// would let anyone enumerate registered emails one guess at a time. Every
    /// early-return path below exists to preserve that: a bad Turnstile token, an
    /// unknown/disabled user, a malformed email, and a rate-limit hit are all
    /// indistinguishable from the outside. `email` is normalized
    /// ([`db::normalize_user_email`]) once, up front, and the normalized form is
    /// used for every lookup/write below (`get_user_id_by_email`, the
    /// `login_code` row, the mail send) — this is what makes login
    /// case-insensitive: `Bob@Example.com` finds the same user and `login_code`
    /// row as `bob@example.com`.
    async fn request_auth_code(
        &self,
        ctx: &Context<'_>,
        email: String,
        turnstile_token: Option<String>,
    ) -> bool {
        let email = match db::normalize_user_email(&email) {
            Ok(e) => e,
            Err(e) => {
                info!("request_auth_code: malformed email: {e}");
                return true;
            }
        };

        let remote_ip = ctx
            .data_opt::<super::ClientIp>()
            .and_then(|ip| ip.0.as_deref());
        match crate::turnstile::verify(turnstile_token.as_deref(), remote_ip).await {
            Ok(true) => {}
            Ok(false) => {
                info!("Turnstile challenge failed for request_auth_code");
                return true;
            }
            Err(e) => {
                warn!("Turnstile error in request_auth_code: {:#}", e);
                return true;
            }
        }

        let user_id = match self.app.db().get_user_id_by_email(&email).await {
            Ok(Some(id)) => id,
            Ok(None) => return true,
            Err(e) => {
                warn!("DB error looking up user in request_auth_code: {:#}", e);
                return true;
            }
        };

        match self.app.db().get_users(&[&user_id]).await {
            Ok(users) => match users.into_iter().next().flatten() {
                Some(user) if user.enabled => {}
                _ => {
                    info!("request_auth_code: user disabled or missing id={}", user_id);
                    return true;
                }
            },
            Err(e) => {
                warn!(
                    "DB error checking user enabled in request_auth_code: {:#}",
                    e
                );
                return true;
            }
        }

        let now = crate::clock::now_sec();

        // Rate limit: at most one code per LOGIN_CODE_RATE_LIMIT_S per email.
        if let Ok(Some(existing)) = self.app.db().get_login_code(&email).await
            && now < existing.last_sent_at + LOGIN_CODE_RATE_LIMIT_S
        {
            info!("Rate limit hit for request_auth_code email={}", email);
            return true;
        }

        let code = crate::nonce::generate_code(LOGIN_CODE_DIGITS);

        // Log only in debug builds, so a code never reaches a production log.
        #[cfg(debug_assertions)]
        info!("Email login code for email={}: {}", email, code);

        let code_hash = sha256_hex(&code);
        let expires_at = crate::expire::ExpirePolicy::LoginCode.expires_at(now);

        if let Err(e) = self
            .app
            .db()
            .put_login_code(&email, &code_hash, expires_at, now)
            .await
        {
            warn!("Failed to store login code: {:#}", e);
            return true;
        }

        let subject = "Your Toolbox login code";
        let body = format!(
            "Your login code is: {code}\n\n\
             This code expires in 10 minutes. Do not share it.\n\n\
             If you did not request this code, you can ignore this email."
        );

        info!(user_id = %user_id, "Sending login code to {}", email);
        if let Err(e) = self
            .app
            .mail()
            .send_plain_text(&email, subject, &body)
            .await
        {
            warn!("Failed to send login code email to {}: {:#}", email, e);
        }

        true
    }

    /// Verify an email login code and return an opaque `mtu_` token on success.
    /// **Returns `null` on every failure path** — expired, wrong code, too many
    /// attempts, unknown/disabled user, malformed email — so the response never
    /// tells the caller which step failed. `email` is normalized
    /// ([`db::normalize_user_email`]) once, up front, exactly like
    /// [`Self::request_auth_code`], so a differently-cased address still
    /// resolves to the `login_code` row `requestAuthCode` wrote.
    async fn verify_auth_code(&self, email: String, code: String) -> Option<String> {
        let email = match db::normalize_user_email(&email) {
            Ok(e) => e,
            Err(e) => {
                info!("verify_auth_code: malformed email: {e}");
                return None;
            }
        };
        let now = crate::clock::now_sec();

        let record = match self.app.db().get_login_code(&email).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                info!("verify_auth_code: no code for email={}", email);
                return None;
            }
            Err(e) => {
                warn!("DB error in verify_auth_code: {:#}", e);
                return None;
            }
        };

        if now >= record.expires_at {
            let _ = self.app.db().delete_login_code(&email).await;
            info!("verify_auth_code: expired code for email={}", email);
            return None;
        }

        if record.attempts >= LOGIN_CODE_MAX_ATTEMPTS {
            let _ = self.app.db().delete_login_code(&email).await;
            info!("verify_auth_code: too many attempts for email={}", email);
            return None;
        }

        // Count this attempt *before* comparing — a burst of guesses still burns
        // down toward LOGIN_CODE_MAX_ATTEMPTS even if every one of them errors out
        // some other way before reaching the comparison below.
        let _ = self.app.db().increment_login_code_attempts(&email).await;

        let expected_hash = sha256_hex(&code);
        if record.code_hash != expected_hash {
            info!("verify_auth_code: wrong code for email={}", email);
            return None;
        }

        let _ = self.app.db().delete_login_code(&email).await;

        let user_id = match self.app.db().get_user_id_by_email(&email).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                warn!("verify_auth_code: user not found for email={}", email);
                return None;
            }
            Err(e) => {
                warn!("DB error fetching user in verify_auth_code: {:#}", e);
                return None;
            }
        };

        match self.app.db().get_users(&[&user_id]).await {
            Ok(users) => match users.into_iter().next().flatten() {
                Some(user) if user.enabled => {}
                _ => {
                    info!("verify_auth_code: user disabled or missing id={}", user_id);
                    return None;
                }
            },
            Err(e) => {
                warn!(
                    "DB error checking user enabled in verify_auth_code: {:#}",
                    e
                );
                return None;
            }
        }

        match auth::issue_user_token(&*self.app, &user_id).await {
            Ok(token) => {
                info!("Issued user token for user_id={}", user_id);
                Some(token)
            }
            Err(e) => {
                warn!("Failed to issue user token: {:#}", e);
                None
            }
        }
    }

    /// Request a public-submit-form email verification code for `(slug,
    /// email)`. **Always returns `true`**, same anti-enumeration rationale as
    /// [`Self::request_auth_code`] — a bad Turnstile token, an unresolvable
    /// slug, and an instance with public submission disabled are all
    /// indistinguishable from the outside. The code itself is stored in
    /// `ephemeral_state` under `kind: "submit_code"` — never in `login_code`;
    /// see `auth::SUBMIT_CODE_STATE_KIND`'s doc comment for why that
    /// separation is load-bearing, not incidental.
    async fn request_submit_code(
        &self,
        ctx: &Context<'_>,
        slug: String,
        email: String,
        turnstile_token: Option<String>,
    ) -> bool {
        let remote_ip = ctx
            .data_opt::<super::ClientIp>()
            .and_then(|ip| ip.0.as_deref());
        match crate::turnstile::verify(turnstile_token.as_deref(), remote_ip).await {
            Ok(true) => {}
            Ok(false) => {
                info!("Turnstile challenge failed for request_submit_code");
                return true;
            }
            Err(e) => {
                warn!("Turnstile error in request_submit_code: {:#}", e);
                return true;
            }
        }

        let instance_id = match self.app.db().get_instance_id_by_slug(&slug).await {
            Ok(Some(id)) => id,
            Ok(None) => return true,
            Err(e) => {
                warn!("DB error resolving slug in request_submit_code: {:#}", e);
                return true;
            }
        };

        let instance = match self.app.db().get_instances(&[&instance_id]).await {
            Ok(instances) => match instances.into_iter().next().flatten() {
                Some(i)
                    if i.public_submission_enabled
                        && !i.deleted
                        && i.kind == db::InstanceKind::Support =>
                {
                    i
                }
                _ => {
                    info!(
                        "request_submit_code: instance not public or missing id={}",
                        instance_id
                    );
                    return true;
                }
            },
            Err(e) => {
                warn!("DB error fetching instance in request_submit_code: {:#}", e);
                return true;
            }
        };

        let now = crate::clock::now_sec();
        let state_id = auth::submit_code_state_id(&instance_id, &email);

        // Rate limit: at most one code per LOGIN_CODE_RATE_LIMIT_S per
        // (instance, email) — same window as the login-code flow.
        if let Ok(Some(existing)) = self.app.db().get_ephemeral_state(&state_id).await
            && existing.kind == auth::SUBMIT_CODE_STATE_KIND
            && let Ok(payload) = serde_json::from_str::<auth::SubmitCodePayload>(&existing.payload)
            && now < payload.last_sent_at + LOGIN_CODE_RATE_LIMIT_S
        {
            info!(
                "Rate limit hit for request_submit_code instance={} email={}",
                instance_id, email
            );
            return true;
        }

        let code = crate::nonce::generate_code(LOGIN_CODE_DIGITS);

        // Log only in debug builds, so a code never reaches a production log.
        #[cfg(debug_assertions)]
        info!(
            "Submit code for instance slug={} email={}: {}",
            slug, email, code
        );

        let payload = match serde_json::to_string(&auth::SubmitCodePayload {
            code_hash: sha256_hex(&code),
            attempts: 0,
            last_sent_at: now,
        }) {
            Ok(p) => p,
            Err(e) => {
                warn!("Failed to serialize submit code payload: {:#}", e);
                return true;
            }
        };
        let expires_at = crate::expire::ExpirePolicy::SubmitCode.expires_at(now);

        if let Err(e) = self
            .app
            .db()
            .put_ephemeral_state(
                &state_id,
                auth::SUBMIT_CODE_STATE_KIND,
                &payload,
                expires_at,
            )
            .await
        {
            warn!("Failed to store submit code: {:#}", e);
            return true;
        }

        let subject = format!("Your {} verification code", instance.name);
        let body = format!(
            "Your verification code is: {code}\n\n\
             This code expires in 10 minutes. Do not share it.\n\n\
             If you did not request this code, you can ignore this email."
        );

        info!(instance_id = %instance_id, "Sending submit code to {}", email);
        if let Err(e) = self
            .app
            .mail()
            .send_plain_text(&email, &subject, &body)
            .await
        {
            warn!("Failed to send submit code email to {}: {:#}", email, e);
        }

        true
    }

    /// Verify a public-submit-form code and return a short-lived (15 minute)
    /// capability token scoped to exactly `(email, instance_id)` on success —
    /// authorised for `submitTicket` alone, never anything a real user
    /// session can do. **Returns `null` on every failure path**, mirroring
    /// [`Self::verify_auth_code`]. Reads and writes `ephemeral_state` under
    /// `kind: "submit_code"` exclusively — never `login_code` — which is what
    /// makes it structurally impossible for a code minted by
    /// [`Self::request_auth_code`] to be accepted here, or vice versa; see
    /// `tests/submit_code_dynamodb_local.rs` for the regression tests.
    async fn verify_submit_code(
        &self,
        slug: String,
        email: String,
        code: String,
    ) -> Option<String> {
        let instance_id = match self.app.db().get_instance_id_by_slug(&slug).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                info!("verify_submit_code: unknown slug={}", slug);
                return None;
            }
            Err(e) => {
                warn!("DB error resolving slug in verify_submit_code: {:#}", e);
                return None;
            }
        };

        let now = crate::clock::now_sec();
        let state_id = auth::submit_code_state_id(&instance_id, &email);

        let state = match self.app.db().get_ephemeral_state(&state_id).await {
            Ok(Some(s)) if s.kind == auth::SUBMIT_CODE_STATE_KIND => s,
            Ok(_) => {
                info!(
                    "verify_submit_code: no submit code for instance={} email={}",
                    instance_id, email
                );
                return None;
            }
            Err(e) => {
                warn!("DB error in verify_submit_code: {:#}", e);
                return None;
            }
        };

        if now >= state.expires_at {
            let _ = self.app.db().delete_ephemeral_state(&state_id).await;
            info!("verify_submit_code: expired code for email={}", email);
            return None;
        }

        let Ok(mut payload) = serde_json::from_str::<auth::SubmitCodePayload>(&state.payload)
        else {
            warn!("verify_submit_code: corrupt payload for id={}", state_id);
            return None;
        };

        if payload.attempts >= LOGIN_CODE_MAX_ATTEMPTS {
            let _ = self.app.db().delete_ephemeral_state(&state_id).await;
            info!("verify_submit_code: too many attempts for email={}", email);
            return None;
        }

        // Count this attempt *before* comparing, mirroring verify_auth_code.
        payload.attempts += 1;
        if let Ok(updated) = serde_json::to_string(&payload) {
            let _ = self
                .app
                .db()
                .put_ephemeral_state(
                    &state_id,
                    auth::SUBMIT_CODE_STATE_KIND,
                    &updated,
                    state.expires_at,
                )
                .await;
        }

        if payload.code_hash != sha256_hex(&code) {
            info!("verify_submit_code: wrong code for email={}", email);
            return None;
        }

        let _ = self.app.db().delete_ephemeral_state(&state_id).await;

        match auth::issue_submit_token(&*self.app, &email, &instance_id).await {
            Ok(token) => {
                info!(
                    "Issued submit token for instance={} email={}",
                    instance_id, email
                );
                Some(token)
            }
            Err(e) => {
                warn!("Failed to issue submit token: {:#}", e);
                None
            }
        }
    }

    /// Add an inbound address to an instance. Owner-or-superuser — see
    /// `Instance::inbound_addresses`'s doc comment. `address` is normalized
    /// and classified (exact vs. `*@domain` wildcard) by
    /// `inbound::routing::classify_for_storage`, the same logic `bin/cli.rs`'s
    /// `address add` uses, so the two can never disagree about what counts as
    /// a valid address.
    #[graphql(
        guard = "AuthGuard::new(AuthRequirement::InstanceOwnerOrSuperuser(instance_id.to_string()))"
    )]
    async fn add_inbound_address(
        &self,
        instance_id: ID,
        address: String,
    ) -> Result<InboundAddressInfo> {
        // Support-only — see CLAUDE.md's kind-isolation house rule. An
        // invoicing instance is treated identically to "not found", never a
        // distinct error, so this can't be used to probe an instance's kind.
        self.app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .filter(|i| !i.deleted && i.kind == db::InstanceKind::Support)
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;
        let (normalized, kind) =
            routing::classify_for_storage(&address).map_err(ApiError::forbidden)?;
        if let Some(existing) = self.app.db().get_inbound_address(&normalized).await? {
            return Err(ApiError::conflict(format!(
                "{normalized} is already mapped to instance {}",
                existing.instance_id
            ))
            .into());
        }
        let created = self
            .app
            .db()
            .create_inbound_address(&normalized, instance_id.as_str(), kind)
            .await?;
        Ok(created.into())
    }

    /// Remove an inbound address from an instance. Owner-or-superuser — see
    /// `Instance::inbound_addresses`'s doc comment. An address that doesn't
    /// exist, or that belongs to a different instance, is reported the same
    /// "not found" either way — so this can't be used to probe another
    /// instance's addresses.
    #[graphql(
        guard = "AuthGuard::new(AuthRequirement::InstanceOwnerOrSuperuser(instance_id.to_string()))"
    )]
    async fn remove_inbound_address(&self, instance_id: ID, address: String) -> Result<bool> {
        let normalized = address.trim().to_lowercase();
        let existing = self
            .app
            .db()
            .get_inbound_address(&normalized)
            .await?
            .filter(|a| a.instance_id == instance_id.as_str())
            .ok_or_else(|| ApiError::not_found("InboundAddress", &normalized))?;
        self.app
            .db()
            .delete_inbound_address(&existing.address)
            .await?;
        Ok(true)
    }

    // ── Admin (superuser-only) mutations ─────────────────────────────────────
    //
    // Every mutation below is guarded on `AuthRequirement::Superuser` and
    // sends no mail — internal bookkeeping, per CLAUDE.md's "Outbound mail"
    // entry, same as `assignTicket`/the requester/CC edits. None of them
    // grant any ticket access: the superuser boundary (CLAUDE.md) is admin +
    // instance settings only. Granting superuser itself has no mutation at
    // all — see `db::User::superuser`'s doc comment: `bin/cli.rs`'s `user
    // set-superuser` is the only way.

    /// Create a new instance. Superuser-only — instance creation is now also
    /// a web action, not only `bin/cli.rs`'s `instance create`, but stays
    /// gated the same way: an operator-driven, low-frequency action, not a
    /// self-serve one. Slug format (`db::validate_slug`) and the taken-slug
    /// pre-check (`get_instance_id_by_slug`) mirror the CLI's `instance
    /// create` exactly, so the two entry points can never disagree about
    /// what counts as a valid or available slug — see `SCHEMA.md`'s
    /// slug-uniqueness race entry for the pre-check-then-write window this
    /// still leaves open, unchanged by this mutation existing. `fromName`
    /// defaults to `name`, matching the CLI.
    #[allow(clippy::too_many_arguments)]
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
    async fn create_instance(
        &self,
        name: String,
        slug: String,
        from_name: Option<String>,
        signature: Option<String>,
        public_submission_enabled: bool,
        #[graphql(default_with = "InstanceKindType::Support")] kind: InstanceKindType,
    ) -> Result<Instance<A>> {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("name cannot be empty"));
        }
        let slug = slug.trim().to_string();
        db::validate_slug(&slug).map_err(ApiError::forbidden)?;
        if self
            .app
            .db()
            .get_instance_id_by_slug(&slug)
            .await?
            .is_some()
        {
            return Err(ApiError::conflict(format!("slug {slug:?} is already taken")).into());
        }
        let from_name = from_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(name);
        let signature = signature.unwrap_or_default();
        let kind: db::InstanceKind = kind.into();
        if public_submission_enabled && kind != db::InstanceKind::Support {
            return Err(anyhow!(
                "publicSubmissionEnabled only applies to a support instance"
            ));
        }
        let created = self
            .app
            .db()
            .create_instance(
                name,
                &slug,
                from_name,
                &signature,
                public_submission_enabled,
                kind,
            )
            .await?;
        Ok(Instance::new(created))
    }

    /// Update an instance's name/from-name/signature/public-submission
    /// setting (`InstanceUpdateShape::Fields`). Superuser-only. There is no
    /// `slug` argument — slugs are immutable once created (a rename would
    /// break `[#{slug}-{number}]` subject tags already stamped on past
    /// tickets and emailed to requesters). Deleting/restoring is a separate
    /// mutation, [`Self::set_instance_deleted`] — matching
    /// `InstanceUpdateShape` itself treating `deleted` as its own variant.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
    async fn update_instance(
        &self,
        id: ID,
        name: String,
        from_name: String,
        signature: String,
        public_submission_enabled: bool,
    ) -> Result<Instance<A>> {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("name cannot be empty"));
        }
        let current = self
            .app
            .db()
            .get_instances(&[id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", id.as_str()))?;
        if public_submission_enabled && current.kind != db::InstanceKind::Support {
            return Err(anyhow!(
                "publicSubmissionEnabled only applies to a support instance"
            ));
        }
        self.app
            .db()
            .update_instance(
                id.as_str(),
                db::InstanceUpdateShape::Fields {
                    name,
                    from_name: &from_name,
                    signature: &signature,
                    public_submission_enabled,
                },
            )
            .await?;
        Ok(Instance::new(db::Instance {
            name: name.to_string(),
            from_name,
            signature,
            public_submission_enabled,
            ..current
        }))
    }

    /// Soft-delete (`deleted: true`) or restore (`deleted: false`) an
    /// instance — `db::InstanceUpdateShape::SetDeleted`. Superuser-only.
    /// See `SCHEMA.md`'s instance soft-delete section for what `deleted`
    /// now actually hides: the instance drops out of every member's
    /// `memberships` list, `instance(slug)` resolves to `null` for it (both
    /// identically to "not a member"/"no such slug", not an error — no
    /// probing), and inbound mail addressed to it is dropped the same way
    /// mail to no known instance is (logged, no ticket, no mail sent) rather
    /// than continuing to open tickets nobody will ever see. A no-op
    /// transition (already at the requested state) still succeeds, mirroring
    /// `setTicketStatus`'s idempotency.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
    async fn set_instance_deleted(&self, id: ID, deleted: bool) -> Result<Instance<A>> {
        let current = self
            .app
            .db()
            .get_instances(&[id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", id.as_str()))?;
        if current.deleted != deleted {
            self.app
                .db()
                .update_instance(id.as_str(), db::InstanceUpdateShape::SetDeleted(deleted))
                .await?;
        }
        Ok(Instance::new(db::Instance { deleted, ..current }))
    }

    /// Create a new user. Does not grant any membership — pair with
    /// [`Self::add_member`], mirroring `bin/cli.rs`'s `user create` +
    /// `member add`. Superuser-only. Rejects an email already in use, the
    /// same pre-check the CLI does. `email` is normalized
    /// ([`db::normalize_user_email`] — trimmed and lowercased) before the
    /// taken-email check and the write, the same normalizer every other
    /// entry point that writes or looks up a user email uses
    /// (`bin/cli.rs`'s `user create`, `request_auth_code`,
    /// `verify_auth_code`), so this can never disagree with any of them
    /// about what "the same address" means. This is what makes an
    /// admin-created account's email case-insensitive for login too.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
    async fn create_user(&self, email: String, name: String) -> Result<User<A>> {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("name cannot be empty"));
        }
        let email = db::normalize_user_email(&email).map_err(|e| anyhow!(e))?;
        let email = email.as_str();
        if self.app.db().get_user_id_by_email(email).await?.is_some() {
            return Err(
                ApiError::conflict(format!("a user with email {email} already exists")).into(),
            );
        }
        let created = self.app.db().create_user(email, name).await?;
        Ok(User::new(created))
    }

    /// Update a user's name, email, and enabled state
    /// (`UserUpdateShape::Fields` plus, when the email changed,
    /// `UserUpdateShape::SetEmail`). Superuser-only. `email` is normalized
    /// ([`db::normalize_user_email`]) before comparison and before the
    /// taken-email pre-check against `get_user_id_by_email`, exactly like
    /// [`Self::create_user`] — since `current.email` is itself always
    /// stored normalized, a request that only changes the case of an
    /// already-lowercase email is a no-op, not a conflict. Rejects
    /// `enabled: false` when `id` names the caller themselves —
    /// self-lockout: `Superuser` authorizes acting on users in general, not
    /// any particular one, so nothing else stops a superuser from disabling
    /// their own account and having no way back in short of another
    /// operator running the CLI.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
    async fn update_user(
        &self,
        ctx: &Context<'_>,
        id: ID,
        name: String,
        email: String,
        enabled: bool,
    ) -> Result<User<A>> {
        let caller_id = require_user_id(ctx)?;
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("name cannot be empty"));
        }
        let email = db::normalize_user_email(&email).map_err(|e| anyhow!(e))?;
        let current = self
            .app
            .db()
            .get_users(&[id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("User", id.as_str()))?;
        if !enabled && id.as_str() == caller_id {
            return Err(ApiError::forbidden("Cannot disable your own account").into());
        }
        if email != current.email && self.app.db().get_user_id_by_email(&email).await?.is_some() {
            return Err(
                ApiError::conflict(format!("a user with email {email} already exists")).into(),
            );
        }
        if email != current.email {
            self.app
                .db()
                .update_user(id.as_str(), db::UserUpdateShape::SetEmail { email: &email })
                .await?;
        }
        self.app
            .db()
            .update_user(id.as_str(), db::UserUpdateShape::Fields { name, enabled })
            .await?;
        Ok(User::new(db::User {
            name: name.to_string(),
            email,
            enabled,
            ..current
        }))
    }

    /// **Soft delete.** Disables the user (`enabled = false`, which already
    /// blocks login and every authenticated request — see
    /// `auth::fetch_update_user_auth_info`, which re-checks `enabled` on
    /// every request) and removes every membership they hold. Never a hard
    /// delete: `ticket_message.author_user_id`/`ticket.assignee_user_id`
    /// reference a user by id, so removing the row itself would leave those
    /// references dangling. Superuser-only. Rejects deleting the caller's
    /// own account — same self-lockout reasoning as
    /// [`Self::update_user`]'s doc comment. Written to converge on a retry
    /// after a partial failure: the user is disabled first, then
    /// memberships are deleted one at a time, so a retry that finds the
    /// user already disabled (a prior call's first write succeeded, its
    /// second didn't finish) just re-attempts whatever memberships remain,
    /// rather than failing on an "already disabled" precondition.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
    async fn delete_user(&self, ctx: &Context<'_>, id: ID) -> Result<User<A>> {
        let caller_id = require_user_id(ctx)?;
        if id.as_str() == caller_id {
            return Err(ApiError::forbidden("Cannot delete your own account").into());
        }
        let current = self
            .app
            .db()
            .get_users(&[id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("User", id.as_str()))?;
        self.app
            .db()
            .update_user(
                id.as_str(),
                db::UserUpdateShape::Fields {
                    name: &current.name,
                    enabled: false,
                },
            )
            .await?;
        let memberships = self.app.db().list_memberships_by_user(id.as_str()).await?;
        for m in &memberships {
            self.app.db().delete_membership(&m.id).await?;
        }
        Ok(User::new(db::User {
            enabled: false,
            ..current
        }))
    }

    /// Grant `userId` a role in `instanceId`. Superuser-only — the web
    /// counterpart to `bin/cli.rs`'s `member add`. Verifies the instance and
    /// the user both exist, and rejects if a membership already exists
    /// (mirroring the CLI's own pre-check) — use [`Self::set_member_role`]
    /// to change an existing membership's role instead.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
    async fn add_member(
        &self,
        instance_id: ID,
        user_id: ID,
        role: MembershipRoleType,
    ) -> Result<Instance<A>> {
        let instance = self
            .app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;
        let user_exists = self
            .app
            .db()
            .get_users(&[user_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .is_some();
        if !user_exists {
            return Err(ApiError::not_found("User", user_id.as_str()).into());
        }
        let existing = self
            .app
            .db()
            .list_memberships_by_user(user_id.as_str())
            .await?;
        if existing.iter().any(|m| m.instance_id == instance.id) {
            return Err(ApiError::conflict(format!(
                "user {} is already a member of instance {} — use setMemberRole to change the role",
                user_id.as_str(),
                instance_id.as_str()
            ))
            .into());
        }
        self.app
            .db()
            .create_membership(user_id.as_str(), &instance.id, role.into())
            .await?;
        Ok(Instance::new(instance))
    }

    /// Revoke `userId`'s membership in `instanceId`. Superuser-only. A
    /// membership that doesn't exist is reported `NOT_FOUND`, same as a
    /// missing instance — there's no cross-tenant probing concern to hide
    /// behind a uniform "not found" here the way ticket mutations do, since
    /// a superuser can already see every instance/user via `adminInstances`/
    /// `adminUsers`.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
    async fn remove_member(&self, instance_id: ID, user_id: ID) -> Result<Instance<A>> {
        let instance = self
            .app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;
        let membership = self
            .app
            .db()
            .list_memberships_by_user(user_id.as_str())
            .await?
            .into_iter()
            .find(|m| m.instance_id == instance.id)
            .ok_or_else(|| {
                ApiError::not_found(
                    "Membership",
                    format!("{}@{}", user_id.as_str(), instance_id.as_str()),
                )
            })?;
        self.app.db().delete_membership(&membership.id).await?;
        Ok(Instance::new(instance))
    }

    /// Change an existing membership's role in place
    /// (`db::Handler::update_membership_role`), rather than a
    /// remove-then-add — see that method's doc comment for why: it keeps
    /// the membership row's own id stable and never leaves a window with no
    /// membership row at all if a second write failed. Superuser-only.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
    async fn set_member_role(
        &self,
        instance_id: ID,
        user_id: ID,
        role: MembershipRoleType,
    ) -> Result<Instance<A>> {
        let instance = self
            .app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;
        let membership = self
            .app
            .db()
            .list_memberships_by_user(user_id.as_str())
            .await?
            .into_iter()
            .find(|m| m.instance_id == instance.id)
            .ok_or_else(|| {
                ApiError::not_found(
                    "Membership",
                    format!("{}@{}", user_id.as_str(), instance_id.as_str()),
                )
            })?;
        self.app
            .db()
            .update_membership_role(&membership.id, role.into())
            .await?;
        Ok(Instance::new(instance))
    }

    // ── API token management ─────────────────────────────────────────────────

    /// Mint a fresh `mta_` integration token for `instanceId`, scoped to
    /// exactly `submitVerifiedTicket` (see `auth::AuthInfo::ApiToken`'s doc
    /// comment). Owner-or-superuser, the same posture as `addInboundAddress` —
    /// this is instance settings, and the superuser boundary in `CLAUDE.md`
    /// explicitly allows a superuser to manage those without granting any
    /// ticket access.
    ///
    /// **`token` is the only time the full secret ever exists outside the
    /// caller's own storage** — only its sha256 hash is persisted (see
    /// `auth::issue_api_token`), mirroring `issue_user_token`'s "the caller
    /// keeps nothing else" contract. There is no way to recover a lost
    /// secret; the only remedy is deleting the row and minting a new one.
    #[graphql(
        guard = "AuthGuard::new(AuthRequirement::InstanceOwnerOrSuperuser(instance_id.to_string()))"
    )]
    async fn create_api_token(
        &self,
        ctx: &Context<'_>,
        instance_id: ID,
        name: String,
    ) -> Result<CreatedApiToken<A>> {
        let user_id = require_user_id(ctx)?;
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("name cannot be empty"));
        }
        if name.chars().count() > MAX_API_TOKEN_NAME_LEN {
            return Err(anyhow!(
                "name cannot be longer than {MAX_API_TOKEN_NAME_LEN} characters"
            ));
        }

        // Support-only — see CLAUDE.md's kind-isolation house rule. An
        // invoicing instance is treated identically to "not found".
        self.app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .filter(|i| !i.deleted && i.kind == db::InstanceKind::Support)
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;

        let (token, secret) =
            auth::issue_api_token(&*self.app, instance_id.as_str(), name, &user_id).await?;
        Ok(CreatedApiToken::new(secret, ApiTokenInfo::new(token)))
    }

    /// Rename and/or enable/disable an existing token. Takes only `id` — the
    /// instance it belongs to is a fact of the record, not a caller-supplied
    /// argument — so this mutation carries **no static guard**;
    /// `require_api_token_manager` is the entire authorization story (see
    /// its doc comment). Disabling takes effect on the token's very next
    /// request: `verify_token`'s `mta_` branch checks `enabled` on every
    /// call, and nothing caches the result.
    async fn update_api_token(
        &self,
        ctx: &Context<'_>,
        id: ID,
        name: String,
        enabled: bool,
    ) -> Result<ApiTokenInfo<A>> {
        let existing = require_api_token_manager(ctx, &*self.app, id.as_str()).await?;
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("name cannot be empty"));
        }
        if name.chars().count() > MAX_API_TOKEN_NAME_LEN {
            return Err(anyhow!(
                "name cannot be longer than {MAX_API_TOKEN_NAME_LEN} characters"
            ));
        }
        self.app
            .db()
            .update_api_token(
                &existing.id,
                db::ApiTokenUpdateShape::Fields { name, enabled },
            )
            .await?;
        Ok(ApiTokenInfo::new(db::ApiToken {
            name: name.to_string(),
            enabled,
            ..existing
        }))
    }

    /// Hard delete — see `db::Handler::delete_api_token`'s doc comment for
    /// why there's no soft-delete marker to set instead. Same no-static-guard,
    /// `require_api_token_manager`-only authorization as
    /// [`Self::update_api_token`].
    async fn delete_api_token(&self, ctx: &Context<'_>, id: ID) -> Result<bool> {
        let existing = require_api_token_manager(ctx, &*self.app, id.as_str()).await?;
        self.app.db().delete_api_token(&existing.id).await?;
        Ok(true)
    }

    // ── Ticket mutations ─────────────────────────────────────────────────────

    /// Open a new ticket from the public submit form. **Requester-token
    /// only** — the instance and the requester's email come from the token
    /// (`AuthInfo::Requester`), never from an argument, so this can't be used
    /// to open a ticket in someone else's name or in an instance the token
    /// wasn't scoped to. The requester's own submission becomes the ticket's
    /// first message, stored as `kind: INBOUND` with `fromEmail` set (never
    /// `authorUserId` — no staff member wrote it).
    ///
    /// Sends the requester a brief acknowledgement — with the `Reply-To`
    /// thread already wired up, so the web form's result lands in their
    /// inbox ready to reply to. **Best-effort**: see
    /// `send_system_notification`'s doc comment for why a failure here is
    /// logged, not surfaced — the ticket itself is the primary effect and
    /// already exists by the time this runs; failing the mutation over it
    /// would risk a duplicate submission on retry.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Requester)")]
    async fn submit_ticket(
        &self,
        ctx: &Context<'_>,
        subject: String,
        body: String,
    ) -> Result<Ticket<A>> {
        let Some(AuthInfo::Requester { email, instance_id }) = ctx.data_opt::<AuthInfo>() else {
            return Err(ApiError::forbidden("Must hold a requester submit token").into());
        };
        let subject = subject.trim();
        if subject.is_empty() {
            return Err(anyhow!("subject cannot be empty"));
        }
        let body = body.trim();
        if body.is_empty() {
            return Err(anyhow!("body cannot be empty"));
        }

        let instance = self
            .app
            .db()
            .get_instances(&[instance_id])
            .await?
            .into_iter()
            .next()
            .flatten()
            .filter(|i| !i.deleted && i.kind == db::InstanceKind::Support)
            .ok_or_else(|| ApiError::not_found("Instance", instance_id))?;

        let ticket = open_submitted_ticket(
            &*self.app,
            &instance,
            subject,
            body,
            std::slice::from_ref(email),
            &[],
        )
        .await?;

        send_system_notification(
            &*self.app,
            &ticket,
            std::slice::from_ref(email),
            &[],
            outbound::ACKNOWLEDGEMENT_BODY,
            "submit_ticket",
        )
        .await;

        // Best-effort staff notice — see `staff_notify`'s module doc. A new
        // ticket is always unassigned, so there is no assignee category to
        // pass through.
        staff_notify::notify_staff(
            &*self.app,
            &ticket,
            None,
            StaffEvent::NewTicket {
                requester_email: email,
            },
            Actor::Email(email),
            Some(body),
        )
        .await;

        info!(
            "Ticket submitted: instance={} ticket={} number={}",
            instance.id, ticket.id, ticket.number
        );
        Ok(Ticket::new(ticket))
    }

    /// Open a new ticket with a **caller-supplied, trusted** list of
    /// requester (`to`) and CC (`cc`) addresses — the integration-token
    /// counterpart to `submitTicket`. `AuthRequirement::ApiToken` means the
    /// instance comes from the token, never an argument (same reasoning as
    /// `submitTicket`'s `Requester` token), but unlike `submitTicket` the
    /// *addresses* are also taken on the caller's word rather than proven by
    /// an email-verification code — that is the entire point of this
    /// mutation, and why an `mta_` token is instance-owner-or-superuser
    /// minted rather than self-service: whoever holds one can open a ticket
    /// claiming to be any address they name. An external service that has
    /// already verified its own users' addresses (a customer portal behind
    /// its own login, say) is the intended caller.
    ///
    /// `to[0]` is treated as the submitter for the first message's
    /// `fromEmail` — with a caller-supplied list rather than
    /// `submitTicket`'s always-exactly-one-address token, some convention is
    /// needed, and "the primary requester is whichever address the caller
    /// listed first" is the least surprising one available. `to`/`cc` are
    /// normalized ([`normalize_ticket_email`]), deduped, and capped at
    /// [`MAX_VERIFIED_TICKET_RECIPIENTS`] by
    /// [`normalize_verified_recipients`] before anything is written; an
    /// address matching one of this instance's own inbound addresses is
    /// rejected outright (`outbound::is_own_address`) rather than silently
    /// dropped — silently dropping it (the way `build_outbound` treats
    /// `To`/`Cc`) could leave a ticket whose requester is itself, or whose
    /// acknowledgement has nowhere left to go.
    ///
    /// Sends the same best-effort acknowledgement `submitTicket` does (see
    /// `send_system_notification`'s doc comment for the best-effort
    /// reasoning) to every `to`/`cc` address, and the same best-effort
    /// `NewTicket` staff notice, attributed to `to[0]` — there is no user or
    /// requester identity to attribute it to otherwise.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::ApiToken)")]
    async fn submit_verified_ticket(
        &self,
        ctx: &Context<'_>,
        subject: String,
        body: String,
        to: Vec<String>,
        #[graphql(default_with = "Vec::new()")] cc: Vec<String>,
    ) -> Result<SubmittedTicket> {
        let Some(AuthInfo::ApiToken {
            token_id,
            instance_id,
        }) = ctx.data_opt::<AuthInfo>()
        else {
            return Err(ApiError::forbidden("Must present a valid API token").into());
        };
        let subject = subject.trim();
        if subject.is_empty() {
            return Err(anyhow!("subject cannot be empty"));
        }
        let body = body.trim();
        if body.is_empty() {
            return Err(anyhow!("body cannot be empty"));
        }
        let (to, cc) = normalize_verified_recipients(&to, &cc)?;

        let instance = self
            .app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .filter(|i| !i.deleted && i.kind == db::InstanceKind::Support)
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;

        let addresses = self
            .app
            .db()
            .list_inbound_addresses_by_instance(&instance.id)
            .await?;
        if let Some(own) = to
            .iter()
            .chain(cc.iter())
            .find(|addr| outbound::is_own_address(&addresses, addr))
        {
            return Err(anyhow!(
                "{own:?} is one of this instance's own inbound addresses and cannot be a requester or CC"
            ));
        }

        let ticket = open_submitted_ticket(&*self.app, &instance, subject, body, &to, &cc).await?;

        send_system_notification(
            &*self.app,
            &ticket,
            &to,
            &cc,
            outbound::ACKNOWLEDGEMENT_BODY,
            "submit_verified_ticket",
        )
        .await;

        // Best-effort staff notice — see `staff_notify`'s module doc, and
        // this mutation's own doc comment for why `to[0]` is the attributed
        // requester. A new ticket is always unassigned, so there is no
        // assignee category to pass through.
        staff_notify::notify_staff(
            &*self.app,
            &ticket,
            None,
            StaffEvent::NewTicket {
                requester_email: &to[0],
            },
            Actor::Email(&to[0]),
            Some(body),
        )
        .await;

        info!(
            "Verified ticket submitted: instance={} ticket={} number={} token={}",
            instance.id, ticket.id, ticket.number, token_id
        );
        Ok(SubmittedTicket {
            id: ID(ticket.id.clone()),
            number: ticket.number as i64,
            subject_tag: db::ticket_subject_tag(&instance.slug, ticket.number),
        })
    }

    /// Mint a presigned S3 PUT into `pending/{instance_id}/{nanoid}/{filename}`
    /// for an agent to upload an attachment to, ahead of attaching it to a
    /// reply with `replyToTicket(attachmentKeys:)`. Member-only, using the
    /// same per-ticket authorization as every other ticket mutation (the
    /// `ticketId` argument exists only to derive — and check membership of
    /// — the owning instance; the key itself is instance-scoped, not
    /// ticket-scoped, since the upload happens before any message row
    /// exists to scope it to). The upload is not attached to anything until
    /// `replyToTicket` moves it; an upload that's never attached expires out
    /// of `pending/` on its own (see the build plan's `s3_mail.tf` sketch —
    /// a 1-day lifecycle rule on that prefix).
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn create_attachment_upload(
        &self,
        ctx: &Context<'_>,
        ticket_id: ID,
        filename: String,
        content_type: String,
    ) -> Result<AttachmentUpload> {
        let ticket = require_ticket_member(ctx, &*self.app, ticket_id.as_str()).await?;
        let safe_name = attachments::sanitize_filename(Some(&filename), 0);
        let key =
            attachments::pending_key(&ticket.instance_id, &crate::dynamodb::new_id(), &safe_name);
        let upload_url = self.app.storage().presign_put(&key, &content_type).await?;
        Ok(AttachmentUpload { key, upload_url })
    }

    /// Reply to a ticket as a member (agent/owner): persists the reply
    /// (`kind: REPLY`), bumps the ticket's activity, builds the outbound
    /// RFC 5322 message (`crate::outbound`) and sends it via
    /// `crate::mail::Handler::send_raw`, then stamps the returned message id
    /// onto the row so a further reply — from either side — threads
    /// correctly.
    ///
    /// `attachmentKeys`, when given, are `pending/…` keys returned by prior
    /// `createAttachmentUpload` calls. Each is validated with
    /// `inbound::attachments::is_pending_key_for_instance` against *this*
    /// ticket's instance before being moved into place — a caller cannot
    /// attach an arbitrary S3 key, including another instance's pending
    /// upload, this way. Moved (and the message row's `attachments`
    /// stamped) *before* the send is attempted, so a send failure still
    /// leaves a fully-formed, attachment-bearing row behind — consistent
    /// with the "the row survives" reasoning below. **Not embedded in the
    /// outbound MIME message itself** — that would mean fetching every
    /// attachment's bytes back out of storage and re-encoding them into the
    /// raw message on every reply, which is a real feature but a separate
    /// one from wiring up storage/move/record; deferred rather than
    /// half-built here. The stored attachment is still visible (and
    /// downloadable) in the thread view either way.
    ///
    /// **On a send failure, this mutation returns `Err` — but the
    /// `ticket_message` row it already created is *not* rolled back.**
    /// What the agent typed is the durable record of what they tried to
    /// say; a transient SES/network failure shouldn't erase it and force a
    /// retype. But the caller must be told delivery didn't happen — a
    /// silent success here would let an agent believe a customer received a
    /// reply that never left the building — so the failure surfaces as a
    /// mutation error rather than a quietly-`null` `rfcMessageId` on an
    /// otherwise-successful response. The persisted-but-unsent row (no
    /// `rfcMessageId`) doubles as the audit trail an operator needs to spot
    /// and retry a failed send. Contrast `submit_ticket`'s acknowledgement
    /// and `set_ticket_status`'s notice, both best-effort: those mutations'
    /// primary effect already succeeded by the time mail is attempted, so a
    /// failure there is logged, not surfaced.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn reply_to_ticket(
        &self,
        ctx: &Context<'_>,
        ticket_id: ID,
        body: String,
        attachment_keys: Option<Vec<String>>,
    ) -> Result<TicketMessage<A>> {
        let ticket = require_ticket_member(ctx, &*self.app, ticket_id.as_str()).await?;
        let user_id = require_user_id(ctx)?;
        let body = body.trim();
        if body.is_empty() {
            return Err(anyhow!("body cannot be empty"));
        }

        // Threading is computed from the ticket's *existing* messages,
        // before this reply is created — see `outbound::threading_for`'s
        // doc comment for why it chains from the last message that
        // actually carries an `rfc_message_id`, not just the last row.
        let prior_messages = self.app.db().list_ticket_messages(&ticket.id).await?;
        let threading = outbound::threading_for(&prior_messages);

        let instance = self
            .app
            .db()
            .get_instances(&[ticket.instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", &ticket.instance_id))?;
        let addresses = self
            .app
            .db()
            .list_inbound_addresses_by_instance(&instance.id)
            .await?;

        // Built (and can fail on a configuration problem — no inbound
        // address, no recipients left once our own are excluded) *before*
        // anything is persisted, so a reply that can't possibly be sent
        // never creates a dangling message row in the first place. That's
        // a different failure mode from the send failure this method's doc
        // comment is about: this one means the reply was never attempted.
        let built = outbound::build_outbound(
            &instance,
            &addresses,
            &ticket,
            &ticket.requester_emails,
            &ticket.cc_emails,
            body,
            &threading,
        )?;

        let msg = self
            .app
            .db()
            .create_ticket_message(
                &ticket.id,
                db::TicketMessageKind::Reply,
                Some(&user_id),
                None,
                &built.to,
                &built.cc,
                Some(body),
                None,
                threading.in_reply_to.as_deref(),
                threading.references.as_deref(),
            )
            .await?;

        self.app
            .db()
            .update_ticket(
                &ticket.id,
                db::TicketUpdateShape::Touch {
                    now: crate::clock::now_sec(),
                },
            )
            .await?;

        let stored_attachments = move_attachments(
            &*self.app,
            &ticket,
            &msg.id,
            attachment_keys.unwrap_or_default(),
        )
        .await?;
        if !stored_attachments.is_empty() {
            self.app
                .db()
                .update_ticket_message(
                    &msg.id,
                    db::TicketMessageUpdateShape::SetAttachments {
                        attachments: &stored_attachments,
                    },
                )
                .await?;
            // Denormalise onto the ticket so a list row can show a paperclip
            // without reading every message of every ticket on the page.
            self.app
                .db()
                .update_ticket(&ticket.id, db::TicketUpdateShape::MarkHasAttachments)
                .await?;
        }

        // From here on, the row exists no matter what happens next — see
        // this method's doc comment.
        let message_id = self
            .app
            .mail()
            .send_raw(&built.raw, &built.to, &built.cc)
            .await
            .with_context(|| format!("sending reply for ticket {}", ticket.id))?;

        self.app
            .db()
            .update_ticket_message(
                &msg.id,
                db::TicketMessageUpdateShape::SetRfcMessageId {
                    rfc_message_id: &message_id,
                },
            )
            .await?;

        // Best-effort staff notice — only after the customer-facing send
        // above actually succeeded (this line is unreached otherwise, since
        // the `?` above returns early on failure), per CLAUDE.md.
        staff_notify::notify_staff(
            &*self.app,
            &ticket,
            ticket.assignee_user_id.as_deref(),
            StaffEvent::AgentReply,
            Actor::User { id: &user_id },
            Some(body),
        )
        .await;

        Ok(TicketMessage::new(db::TicketMessage {
            rfc_message_id: Some(message_id),
            attachments: stored_attachments,
            ..msg
        }))
    }

    /// Add an internal note to a ticket. Member-only, same per-ticket
    /// authorization as every other ticket mutation. Stored as `kind: NOTE`,
    /// which `Ticket.messages` filters out for anyone but a member of this
    /// ticket's instance — see that field's doc comment. **Never emailed,
    /// now or ever**: this mutation never calls `send_system_notification`
    /// or `mail::Handler::send_raw`, unlike `submit_ticket` (an
    /// acknowledgement), `set_ticket_status` (a close/reopen notice), and
    /// `reply_to_ticket` (the `REPLY` itself) — pinned by
    /// `tests/outbound_mail_dynamodb_local.rs`'s
    /// `internal_note_sends_no_mail`.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn add_internal_note(
        &self,
        ctx: &Context<'_>,
        ticket_id: ID,
        body: String,
    ) -> Result<TicketMessage<A>> {
        let ticket = require_ticket_member(ctx, &*self.app, ticket_id.as_str()).await?;
        let user_id = require_user_id(ctx)?;
        let body = body.trim();
        if body.is_empty() {
            return Err(anyhow!("body cannot be empty"));
        }

        let msg = self
            .app
            .db()
            .create_ticket_message(
                &ticket.id,
                db::TicketMessageKind::Note,
                Some(&user_id),
                None,
                &[],
                &[],
                Some(body),
                None,
                None,
                None,
            )
            .await?;

        self.app
            .db()
            .update_ticket(
                &ticket.id,
                db::TicketUpdateShape::Touch {
                    now: crate::clock::now_sec(),
                },
            )
            .await?;

        // Best-effort staff notice — an internal note never emails the
        // customer (see this mutation's own doc comment), but it is still
        // ticket activity staff members can opt into hearing about.
        staff_notify::notify_staff(
            &*self.app,
            &ticket,
            ticket.assignee_user_id.as_deref(),
            StaffEvent::InternalNote,
            Actor::User { id: &user_id },
            Some(body),
        )
        .await;

        Ok(TicketMessage::new(msg))
    }

    /// Change a ticket's status — `OPEN`/`CLOSED`/`DELETED`. Deleting and
    /// restoring a ticket are just this mutation with `DELETED`/`OPEN`: see
    /// `db::TicketUpdateShape::SetStatusAndAssignee`'s doc comment for why
    /// that one variant, not a bespoke delete/restore code path, is what
    /// keeps the three composite marker attributes consistent. A no-op
    /// transition (already at the requested status) still succeeds, returning
    /// the ticket unchanged, rather than erroring — idempotent by design
    /// (and, since it returns before reaching the mail step below, sends no
    /// notice either).
    ///
    /// A close or a reopen (including restoring a deleted ticket) mails
    /// requesters/CCs a brief notice — see `outbound::status_notice_body`.
    /// Deleting one does not: see that function's doc comment. The same
    /// "not into `DELETED`" carve-out applies to the best-effort staff
    /// notice below.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn set_ticket_status(
        &self,
        ctx: &Context<'_>,
        ticket_id: ID,
        status: TicketStatusType,
    ) -> Result<Ticket<A>> {
        let ticket = require_ticket_member(ctx, &*self.app, ticket_id.as_str()).await?;
        let user_id = require_user_id(ctx)?;
        let old_status = ticket.status;
        let new_status: db::TicketStatus = status.into();
        if new_status == old_status {
            return Ok(Ticket::new(ticket));
        }
        let now = crate::clock::now_sec();
        self.app
            .db()
            .update_ticket(
                &ticket.id,
                db::TicketUpdateShape::SetStatusAndAssignee {
                    instance_id: &ticket.instance_id,
                    status: new_status,
                    assignee_user_id: ticket.assignee_user_id.as_deref(),
                    now,
                },
            )
            .await?;

        // Best-effort: a close or reopen mails requesters/CCs a brief
        // notice; assignment and requester/CC edits do not (see
        // `outbound::status_notice_body`'s doc comment, and the "Outbound
        // mail" entry in `CLAUDE.md`, for the reasoning). Failure here is
        // logged, never surfaced — the status change is this mutation's
        // primary effect and has already succeeded.
        if let Some(notice) = outbound::status_notice_body(old_status, new_status) {
            send_system_notification(
                &*self.app,
                &ticket,
                &ticket.requester_emails,
                &ticket.cc_emails,
                notice,
                "set_ticket_status",
            )
            .await;
        }

        // Best-effort staff notice, for any status change except one *into*
        // `DELETED` — mirrors the customer-mail carve-out above (housekeeping,
        // not something a member needs to hear about). Passed a ticket
        // reflecting the *new* status, so `staff_notify`'s own deleted-ticket
        // skip (rule 4) sees the post-write state.
        if new_status != db::TicketStatus::Deleted {
            let ticket_for_notify = db::Ticket {
                status: new_status,
                ..ticket.clone()
            };
            staff_notify::notify_staff(
                &*self.app,
                &ticket_for_notify,
                ticket.assignee_user_id.as_deref(),
                StaffEvent::StatusChanged {
                    old: old_status,
                    new: new_status,
                },
                Actor::User { id: &user_id },
                None,
            )
            .await;
        }

        Ok(Ticket::new(db::Ticket {
            status: new_status,
            updated_at: now,
            last_activity_at: now,
            ..ticket
        }))
    }

    /// Assign (or, with `userId: null`, unassign) a ticket. The target user,
    /// when assigning, must themselves be a member of this ticket's
    /// instance — assigning to an outsider would be silently useless (they
    /// could never see it via the "assigned to me" filter, which is itself
    /// instance-scoped). **Sends no customer mail**: which agent owns a
    /// ticket is internal bookkeeping the requester has no reason to be
    /// notified about — see the "Outbound mail" entry in `CLAUDE.md`. It
    /// *does* send a best-effort staff notice, but only to the new and
    /// previous assignee (never the rest of the team) — see
    /// `staff_notify`'s module doc — and only when the assignee actually
    /// changes (a self-assign, or reassigning to the current assignee,
    /// notifies nobody).
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn assign_ticket(
        &self,
        ctx: &Context<'_>,
        ticket_id: ID,
        user_id: Option<ID>,
    ) -> Result<Ticket<A>> {
        let ticket = require_ticket_member(ctx, &*self.app, ticket_id.as_str()).await?;
        let actor_id = require_user_id(ctx)?;
        if let Some(uid) = &user_id {
            let target_memberships = self.app.db().list_memberships_by_user(uid.as_str()).await?;
            if !target_memberships
                .iter()
                .any(|m| m.instance_id == ticket.instance_id)
            {
                return Err(
                    ApiError::forbidden("Assignee must be a member of this instance").into(),
                );
            }
        }
        let now = crate::clock::now_sec();
        let old_assignee = ticket.assignee_user_id.clone();
        let assignee_user_id = user_id.as_ref().map(|id| id.as_str());
        self.app
            .db()
            .update_ticket(
                &ticket.id,
                db::TicketUpdateShape::SetStatusAndAssignee {
                    instance_id: &ticket.instance_id,
                    status: ticket.status,
                    assignee_user_id,
                    now,
                },
            )
            .await?;

        if old_assignee.as_deref() != assignee_user_id {
            staff_notify::notify_staff(
                &*self.app,
                &ticket,
                assignee_user_id,
                StaffEvent::Assigned {
                    old: old_assignee.as_deref(),
                    new: assignee_user_id,
                },
                Actor::User { id: &actor_id },
                None,
            )
            .await;
        }

        Ok(Ticket::new(db::Ticket {
            assignee_user_id: user_id.map(|id| id.to_string()),
            updated_at: now,
            last_activity_at: now,
            ..ticket
        }))
    }

    /// Add a requester (a "To" recipient) to a ticket. **Sends no mail at
    /// all** — customer or staff — see `CLAUDE.md`'s "Outbound mail" and
    /// "Staff notifications" entries; unlike `assignTicket` (which does send
    /// a staff notice), who's on a ticket's requester/CC list is internal
    /// bookkeeping nobody is notified about.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn add_ticket_requester(
        &self,
        ctx: &Context<'_>,
        ticket_id: ID,
        email: String,
    ) -> Result<Ticket<A>> {
        let ticket = require_ticket_member(ctx, &*self.app, ticket_id.as_str()).await?;
        let email = normalize_ticket_email(&email)?;
        let now = crate::clock::now_sec();
        self.app
            .db()
            .update_ticket(
                &ticket.id,
                db::TicketUpdateShape::AddRequester { email: &email, now },
            )
            .await?;
        let mut requester_emails = ticket.requester_emails.clone();
        if !requester_emails.contains(&email) {
            requester_emails.push(email);
        }
        Ok(Ticket::new(db::Ticket {
            requester_emails,
            updated_at: now,
            last_activity_at: now,
            ..ticket
        }))
    }

    /// Remove a requester from a ticket. Refuses to remove the last one — a
    /// ticket must always have someone to reply to. **Sends no mail at
    /// all** — see [`Self::add_ticket_requester`]'s doc comment.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn remove_ticket_requester(
        &self,
        ctx: &Context<'_>,
        ticket_id: ID,
        email: String,
    ) -> Result<Ticket<A>> {
        let ticket = require_ticket_member(ctx, &*self.app, ticket_id.as_str()).await?;
        let email = normalize_ticket_email(&email)?;
        if ticket.requester_emails.len() <= 1 && ticket.requester_emails.contains(&email) {
            return Err(ApiError::conflict("A ticket must keep at least one requester").into());
        }
        let now = crate::clock::now_sec();
        self.app
            .db()
            .update_ticket(
                &ticket.id,
                db::TicketUpdateShape::RemoveRequester { email: &email, now },
            )
            .await?;
        let requester_emails = ticket
            .requester_emails
            .iter()
            .filter(|e| **e != email)
            .cloned()
            .collect();
        Ok(Ticket::new(db::Ticket {
            requester_emails,
            updated_at: now,
            last_activity_at: now,
            ..ticket
        }))
    }

    /// Add a CC recipient to a ticket. **Sends no mail at all** — see
    /// [`Self::add_ticket_requester`]'s doc comment.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn add_ticket_cc(
        &self,
        ctx: &Context<'_>,
        ticket_id: ID,
        email: String,
    ) -> Result<Ticket<A>> {
        let ticket = require_ticket_member(ctx, &*self.app, ticket_id.as_str()).await?;
        let email = normalize_ticket_email(&email)?;
        let now = crate::clock::now_sec();
        self.app
            .db()
            .update_ticket(
                &ticket.id,
                db::TicketUpdateShape::AddCc { email: &email, now },
            )
            .await?;
        let mut cc_emails = ticket.cc_emails.clone();
        if !cc_emails.contains(&email) {
            cc_emails.push(email);
        }
        Ok(Ticket::new(db::Ticket {
            cc_emails,
            updated_at: now,
            last_activity_at: now,
            ..ticket
        }))
    }

    /// Remove a CC recipient from a ticket. Unlike requesters, CCs may go to
    /// zero — there is no "must keep at least one" rule for them.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn remove_ticket_cc(
        &self,
        ctx: &Context<'_>,
        ticket_id: ID,
        email: String,
    ) -> Result<Ticket<A>> {
        let ticket = require_ticket_member(ctx, &*self.app, ticket_id.as_str()).await?;
        let email = normalize_ticket_email(&email)?;
        let now = crate::clock::now_sec();
        self.app
            .db()
            .update_ticket(
                &ticket.id,
                db::TicketUpdateShape::RemoveCc { email: &email, now },
            )
            .await?;
        let cc_emails = ticket
            .cc_emails
            .iter()
            .filter(|e| **e != email)
            .cloned()
            .collect();
        Ok(Ticket::new(db::Ticket {
            cc_emails,
            updated_at: now,
            last_activity_at: now,
            ..ticket
        }))
    }

    /// Update the caller's own staff-notification preferences for
    /// `instanceId` (`db::NotificationSettingsPatch`, `SET`-only — see
    /// `CLAUDE.md`'s omit-optional-attributes house rule). Member-only,
    /// resolved via `list_memberships_by_user(caller)` filtered to
    /// `instanceId` — never an `instanceId` argument trusted on its own,
    /// the same "never trust an id argument" posture as
    /// `require_ticket_member` — so this can only ever touch the caller's
    /// *own* membership row, which is also what makes
    /// `MembershipInfo::notification_settings`'s self-only guard
    /// meaningful: there is no mutation that can set anyone else's.
    /// Returns the membership merged onto its current settings, without a
    /// second read of the row this just wrote.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Member(instance_id.to_string()))")]
    async fn update_notification_settings(
        &self,
        ctx: &Context<'_>,
        instance_id: ID,
        settings: NotificationSettingsInput,
    ) -> Result<MembershipInfo<A>> {
        let user_id = require_user_id(ctx)?;
        let membership = self
            .app
            .db()
            .list_memberships_by_user(&user_id)
            .await?
            .into_iter()
            .find(|m| m.instance_id == instance_id.as_str())
            .ok_or_else(|| ApiError::not_found("Membership", instance_id.as_str()))?;

        let patch: db::NotificationSettingsPatch = settings.into();
        if !patch.is_empty() {
            self.app
                .db()
                .update_membership_notification_settings(&membership.id, &patch)
                .await?;
        }
        let merged = patch.merged_onto(membership.notification_settings);

        let instance = self
            .app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;

        Ok(MembershipInfo::new(
            user_id,
            Instance::new(instance),
            membership.role.into(),
            merged,
        ))
    }

    /// Revoke the calling token, if there is one to revoke (a `Requester`
    /// capability token, or an impersonated dev-auth session, has none — this is
    /// still a harmless `true` for either).
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn logout(&self, ctx: &Context<'_>) -> Result<bool> {
        if let Some(AuthInfo::User {
            token_id: Some(token_id),
            ..
        }) = ctx.data_opt::<AuthInfo>()
        {
            self.app.db().delete_user_token(token_id).await?;
        }
        Ok(true)
    }

    // ── Passkey (WebAuthn) mutations ─────────────────────────────────────────

    /// Start passkey registration for the authenticated user. Returns a JSON
    /// challenge to pass to the browser's WebAuthn API.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn begin_passkey_registration(&self, ctx: &Context<'_>) -> Result<PasskeyChallenge> {
        use webauthn_rs::prelude::*;

        let user_id = require_user_id(ctx)?;

        let count = self
            .app
            .db()
            .count_webauthn_credentials_by_user(&user_id)
            .await?;
        if count >= MAX_PASSKEYS_PER_USER {
            return Err(anyhow!(
                "Maximum of {MAX_PASSKEYS_PER_USER} passkeys allowed"
            ));
        }

        let existing = self
            .app
            .db()
            .list_webauthn_credentials_by_user(&user_id)
            .await?;

        let webauthn = ctx.data_unchecked::<Arc<Webauthn>>();

        // The user handle stays tied to the (immutable) user id so a passkey
        // keeps working if the user's email changes. Only the display name —
        // what the OS/password manager shows — uses the email.
        let user_uuid = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, user_id.as_bytes());
        let display_name = self
            .app
            .db()
            .get_users(&[&user_id])
            .await?
            .into_iter()
            .next()
            .flatten()
            .map(|u| u.email)
            .unwrap_or_else(|| user_id.clone());

        let existing_cred_ids: Vec<CredentialID> = existing
            .iter()
            .filter_map(|c| {
                serde_json::from_str::<Passkey>(&c.passkey_json)
                    .ok()
                    .map(|pk| pk.cred_id().clone())
            })
            .collect();

        let exclude = if existing_cred_ids.is_empty() {
            None
        } else {
            Some(existing_cred_ids)
        };

        let (ccr, reg_state) = webauthn.start_passkey_registration(
            user_uuid,
            &display_name,
            &display_name,
            exclude,
        )?;

        // Force the credential to be discoverable (a resident key). webauthn-rs's
        // registration options don't expose a builder knob for the modern
        // `residentKey` field (only the legacy `requireResidentKey` boolean), so
        // inject `residentKey: "required"` into the options JSON before handing
        // it to the browser. A platform authenticator makes a credential
        // discoverable by default, but a security key may not unless asked — and
        // a non-discoverable credential can't be used by `beginPasskeyLogin`'s
        // usernameless flow. `finish_passkey_registration` doesn't validate
        // residency itself, so there's no verification mismatch from patching
        // the request but not the response.
        let mut options_value = serde_json::to_value(&ccr.public_key)
            .map_err(|e| anyhow!("Failed to serialize registration options: {e}"))?;
        if let Some(sel) = options_value
            .get_mut("authenticatorSelection")
            .and_then(|v| v.as_object_mut())
        {
            sel.insert("residentKey".to_string(), serde_json::json!("required"));
            sel.insert("requireResidentKey".to_string(), serde_json::json!(true));
        }
        let options_json = serde_json::to_string(&options_value)
            .map_err(|e| anyhow!("Failed to serialize registration options: {e}"))?;
        let state_json = serde_json::to_string(&reg_state)
            .map_err(|e| anyhow!("Failed to serialize registration state: {e}"))?;

        let challenge_id = nanoid::nanoid!(32);
        let expires_at = crate::expire::ExpirePolicy::WebauthnChallenge.from_now();

        self.app
            .db()
            .put_webauthn_state(
                &challenge_id,
                "reg",
                Some(&user_id),
                &state_json,
                expires_at,
            )
            .await?;

        Ok(PasskeyChallenge {
            challenge_id,
            options_json,
        })
    }

    /// Finish passkey registration: verify the browser response and store the
    /// credential.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn finish_passkey_registration(
        &self,
        ctx: &Context<'_>,
        challenge_id: String,
        credential_json: String,
        name: String,
    ) -> Result<PasskeyInfo> {
        use webauthn_rs::prelude::*;

        let user_id = require_user_id(ctx)?;

        let state_record = self
            .app
            .db()
            .get_webauthn_state(&challenge_id)
            .await?
            .ok_or_else(|| anyhow!("Registration challenge not found or expired"))?;

        if state_record.kind != "reg" {
            return Err(anyhow!("Invalid challenge kind"));
        }
        if state_record.user_id.as_deref() != Some(user_id.as_str()) {
            return Err(anyhow!("Challenge belongs to a different user"));
        }

        let now = crate::clock::now_sec();
        if now >= state_record.expires_at {
            let _ = self.app.db().delete_webauthn_state(&challenge_id).await;
            return Err(anyhow!("Registration challenge expired"));
        }

        let reg_state: PasskeyRegistration = serde_json::from_str(&state_record.state_json)
            .map_err(|e| anyhow!("Failed to deserialize registration state: {e}"))?;
        let reg_credential: RegisterPublicKeyCredential = serde_json::from_str(&credential_json)
            .map_err(|e| anyhow!("Failed to parse credential: {e}"))?;

        let webauthn = ctx.data_unchecked::<Arc<Webauthn>>();
        let passkey = webauthn
            .finish_passkey_registration(&reg_credential, &reg_state)
            .map_err(|e| anyhow!("Passkey registration failed: {e}"))?;

        // Re-check the cap to guard against a race with a second concurrent
        // registration from the same user.
        let count = self
            .app
            .db()
            .count_webauthn_credentials_by_user(&user_id)
            .await?;
        if count >= MAX_PASSKEYS_PER_USER {
            let _ = self.app.db().delete_webauthn_state(&challenge_id).await;
            return Err(anyhow!(
                "Maximum of {MAX_PASSKEYS_PER_USER} passkeys allowed"
            ));
        }

        let cred_id = URL_SAFE_NO_PAD.encode(passkey.cred_id().as_ref());
        let passkey_json = serde_json::to_string(&passkey)
            .map_err(|e| anyhow!("Failed to serialize passkey: {e}"))?;

        let cred = self
            .app
            .db()
            .create_webauthn_credential(&cred_id, &user_id, &name, &passkey_json)
            .await?;

        let _ = self.app.db().delete_webauthn_state(&challenge_id).await;

        info!(
            "Passkey registered for user_id={} cred_id={}",
            user_id, cred_id
        );

        Ok(cred.into())
    }

    /// Start a discoverable passkey login (no username required). Returns a JSON
    /// challenge to pass to the browser's WebAuthn API.
    async fn begin_passkey_login(&self, ctx: &Context<'_>) -> Result<PasskeyChallenge> {
        use webauthn_rs::prelude::*;

        let webauthn = ctx.data_unchecked::<Arc<Webauthn>>();
        let (rcr, auth_state) = webauthn
            .start_discoverable_authentication()
            .map_err(|e| anyhow!("Failed to start passkey login: {e}"))?;

        let options_json = serde_json::to_string(&rcr.public_key)
            .map_err(|e| anyhow!("Failed to serialize login options: {e}"))?;
        let state_json = serde_json::to_string(&auth_state)
            .map_err(|e| anyhow!("Failed to serialize auth state: {e}"))?;

        let challenge_id = nanoid::nanoid!(32);
        let expires_at = crate::expire::ExpirePolicy::WebauthnChallenge.from_now();

        self.app
            .db()
            .put_webauthn_state(&challenge_id, "auth", None, &state_json, expires_at)
            .await?;

        Ok(PasskeyChallenge {
            challenge_id,
            options_json,
        })
    }

    /// Finish passkey login: verify the browser response and return an opaque
    /// `mtu_` token. `Ok(None)` for anything that means "this credential doesn't
    /// authenticate" (expired challenge, unknown credential, disabled user) — an
    /// `Err` is reserved for the request itself being malformed.
    async fn finish_passkey_login(
        &self,
        ctx: &Context<'_>,
        challenge_id: String,
        credential_json: String,
    ) -> Result<Option<String>> {
        use webauthn_rs::prelude::*;

        let state_record = self
            .app
            .db()
            .get_webauthn_state(&challenge_id)
            .await?
            .ok_or_else(|| anyhow!("Login challenge not found or expired"))?;

        if state_record.kind != "auth" {
            return Err(anyhow!("Invalid challenge kind"));
        }

        let now = crate::clock::now_sec();
        if now >= state_record.expires_at {
            let _ = self.app.db().delete_webauthn_state(&challenge_id).await;
            return Ok(None);
        }

        let auth_state: DiscoverableAuthentication = serde_json::from_str(&state_record.state_json)
            .map_err(|e| anyhow!("Failed to deserialize auth state: {e}"))?;
        let auth_credential: PublicKeyCredential = serde_json::from_str(&credential_json)
            .map_err(|_| anyhow!("Failed to parse credential"))?;

        let webauthn = ctx.data_unchecked::<Arc<Webauthn>>();
        let (_user_handle, cred_id_bytes) = webauthn
            .identify_discoverable_authentication(&auth_credential)
            .map_err(|e| anyhow!("Failed to identify credential: {e}"))?;

        let cred_id_str = URL_SAFE_NO_PAD.encode(cred_id_bytes);
        let stored = match self.app.db().get_webauthn_credential(&cred_id_str).await? {
            Some(c) => c,
            None => {
                info!("finish_passkey_login: unknown credential {}", cred_id_str);
                let _ = self.app.db().delete_webauthn_state(&challenge_id).await;
                return Ok(None);
            }
        };

        let mut passkey: Passkey = serde_json::from_str(&stored.passkey_json)
            .map_err(|e| anyhow!("Failed to deserialize stored passkey: {e}"))?;

        let auth_result = webauthn
            .finish_discoverable_authentication(
                &auth_credential,
                auth_state,
                &[DiscoverableKey::from(&passkey)],
            )
            .map_err(|e| anyhow!("Passkey authentication failed: {e}"))?;

        // Always record last_used_at on a successful login. The counter bump is
        // conditional (needs_update() only fires when the signature counter
        // advanced), but most platform/synced passkeys keep the counter at 0 and
        // never report needs_update(), so gating the whole write on it would
        // leave last_used_at perpetually unset for the common case.
        if auth_result.needs_update() {
            passkey.update_credential(&auth_result);
        }
        let updated_json = serde_json::to_string(&passkey)
            .map_err(|e| anyhow!("Failed to serialize updated passkey: {e}"))?;
        let _ = self
            .app
            .db()
            .update_webauthn_credential(
                &cred_id_str,
                db::WebauthnCredentialUpdate::TouchLastUsed {
                    passkey_json: updated_json,
                },
            )
            .await;

        let _ = self.app.db().delete_webauthn_state(&challenge_id).await;

        match self
            .app
            .db()
            .get_users(&[&stored.user_id])
            .await?
            .into_iter()
            .next()
            .flatten()
        {
            Some(user) if user.enabled => {}
            _ => {
                info!(
                    "finish_passkey_login: user disabled or missing id={}",
                    stored.user_id
                );
                return Ok(None);
            }
        }

        let token = auth::issue_user_token(&*self.app, &stored.user_id).await?;
        info!(
            "Passkey login for user_id={} cred_id={}",
            stored.user_id, cred_id_str
        );
        Ok(Some(token))
    }

    /// Rename one of the authenticated user's passkeys. A credential belonging
    /// to someone else reports the same "not found" as a nonexistent one — never
    /// "belongs to another user" — so this can't be used to probe who owns a
    /// guessed credential id.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn rename_passkey(
        &self,
        ctx: &Context<'_>,
        id: String,
        name: String,
    ) -> Result<PasskeyInfo> {
        let user_id = require_user_id(ctx)?;

        let cred = self
            .app
            .db()
            .get_webauthn_credential(&id)
            .await?
            .ok_or_else(|| anyhow!("Passkey not found"))?;
        if cred.user_id != user_id {
            return Err(anyhow!("Passkey not found"));
        }

        let trimmed = name.trim().to_string();
        if trimmed.is_empty() {
            return Err(anyhow!("Name cannot be empty"));
        }

        self.app
            .db()
            .update_webauthn_credential(&id, db::WebauthnCredentialUpdate::Rename(trimmed.clone()))
            .await?;

        Ok(PasskeyInfo {
            id: cred.id,
            name: trimmed,
            created_at: cred.created_at as i64,
            last_used_at: cred.last_used_at.map(|t| t as i64),
        })
    }

    /// Delete one of the authenticated user's passkeys. Same ownership-hiding
    /// "not found" as [`Self::rename_passkey`].
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn delete_passkey(&self, ctx: &Context<'_>, id: String) -> Result<bool> {
        let user_id = require_user_id(ctx)?;

        let cred = self
            .app
            .db()
            .get_webauthn_credential(&id)
            .await?
            .ok_or_else(|| anyhow!("Passkey not found"))?;
        if cred.user_id != user_id {
            return Err(anyhow!("Passkey not found"));
        }

        self.app.db().delete_webauthn_credential(&id).await?;
        Ok(true)
    }

    /// Revoke one of the caller's own "connected AI apps". Same ownership-hiding
    /// "not found" as [`Self::delete_passkey`]: a grant that doesn't exist and
    /// one that belongs to someone else fail identically, so a caller can't
    /// probe for other users' grant ids. Deliberately **not** superuser-
    /// overridable — a superuser can't list another user's grants either (see
    /// `User.oauthGrants`), and disabling a user already stops every credential
    /// of theirs, this one included, via `fetch_update_user_auth_info`.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn revoke_oauth_grant(&self, ctx: &Context<'_>, id: ID) -> Result<bool> {
        let user_id = require_user_id(ctx)?;

        let grant = self
            .app
            .db()
            .get_oauth_grant(&id)
            .await?
            .ok_or_else(|| anyhow!("OAuth grant not found"))?;
        if grant.user_id != user_id {
            return Err(anyhow!("OAuth grant not found"));
        }

        self.app.db().delete_oauth_grant(&id).await?;
        Ok(true)
    }

    // ── Invoicing settings ────────────────────────────────────────────────────

    /// Full-replace an invoicing instance's seller settings/payment footer —
    /// `InstanceUpdateShape::SetInvoicingSettings`. Owner-or-superuser, the
    /// same posture as `addInboundAddress`/`createApiToken`: this is
    /// instance settings, not something a plain agent can change. Rejects a
    /// support instance (`db::require_instance_kind`) — see CLAUDE.md's
    /// "Invoicing" house rule.
    ///
    /// Every string field in `input` is trimmed; a blank result after
    /// trimming `REMOVE`s the corresponding attribute rather than writing an
    /// empty string, per the omit-optional-attributes house rule — this is a
    /// full replace, so a field the caller leaves blank is explicitly
    /// cleared, not left alone. `gstRegistered` is written `true` or
    /// `REMOVE`d — see `db::Instance::gst_registered`'s doc comment.
    /// `currency` follows the same blank-means-absent rule (absent defaults
    /// to `"AUD"` — `db::Instance::currency_or_default`); a non-blank value
    /// must be 3 uppercase ASCII letters (`db::validate_currency_code`).
    #[graphql(
        guard = "AuthGuard::new(AuthRequirement::InstanceOwnerOrSuperuser(instance_id.to_string()))"
    )]
    async fn update_invoicing_settings(
        &self,
        instance_id: ID,
        input: InvoicingSettingsInput,
    ) -> Result<Instance<A>> {
        let instance = self
            .app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;
        db::require_instance_kind(&instance, db::InstanceKind::Invoicing)
            .map_err(|e| anyhow!(e))?;

        let business_name = normalize_invoicing_field(&input.business_name, "businessName")?;
        let business_abn = normalize_invoicing_field(&input.business_abn, "businessAbn")?;
        let business_address =
            normalize_invoicing_long_field(&input.business_address, "businessAddress")?;
        let business_phone = normalize_invoicing_field(&input.business_phone, "businessPhone")?;
        let business_email = normalize_invoicing_field(&input.business_email, "businessEmail")?;
        let payment_details =
            normalize_invoicing_long_field(&input.payment_details, "paymentDetails")?;
        let currency = {
            let trimmed = input.currency.trim();
            if trimmed.is_empty() {
                None
            } else {
                db::validate_currency_code(trimmed).map_err(|e| anyhow!(e))?;
                Some(trimmed.to_string())
            }
        };
        let payment_terms_days = input
            .payment_terms_days
            .map(invoicing::validate_payment_terms_days)
            .transpose()
            .map_err(|e| anyhow!(e))?;

        self.app
            .db()
            .update_instance(
                instance_id.as_str(),
                db::InstanceUpdateShape::SetInvoicingSettings {
                    business_name: business_name.as_deref(),
                    business_abn: business_abn.as_deref(),
                    business_address: business_address.as_deref(),
                    business_phone: business_phone.as_deref(),
                    business_email: business_email.as_deref(),
                    payment_details: payment_details.as_deref(),
                    gst_registered: input.gst_registered,
                    currency: currency.as_deref(),
                    payment_terms_days,
                },
            )
            .await?;

        Ok(Instance::new(db::Instance {
            business_name,
            business_abn,
            business_address,
            business_phone,
            business_email,
            payment_details,
            gst_registered: input.gst_registered,
            currency,
            payment_terms_days,
            ..instance
        }))
    }

    // ── Projects ──────────────────────────────────────────────────────────────

    /// Create a project in an invoicing instance. Member (owner or agent) —
    /// managing projects/items/draft invoices is day-to-day work, not an
    /// owner-only setting, per CLAUDE.md's "Invoicing" house rule. Rejects a
    /// support instance.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Member(instance_id.to_string()))")]
    async fn create_project(
        &self,
        instance_id: ID,
        input: CreateProjectInput,
    ) -> Result<Project<A>> {
        let instance = self
            .app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;
        db::require_instance_kind(&instance, db::InstanceKind::Invoicing)
            .map_err(|e| anyhow!(e))?;

        let fields = validate_project_fields(ProjectFieldsInput {
            name: &input.name,
            client_name: &input.client_name,
            client_abn: input.client_abn.as_deref(),
            client_address: input.client_address.as_deref(),
            client_email: input.client_email.as_deref(),
            reference: input.reference.as_deref(),
            payment_terms_days: input.payment_terms_days,
            default_unit_price_cents: input.default_unit_price_cents,
        })?;
        let created = self
            .app
            .db()
            .create_project(instance_id.as_str(), &fields)
            .await?;
        Ok(Project::new(created))
    }

    /// Update a project — full replace, including `archived`. Takes only
    /// `id`; the instance it belongs to is a fact of the record, so
    /// authorization happens in the body after fetching the row (mirroring
    /// `require_ticket_member`) rather than a static `#[graphql(guard)]`. A
    /// project that doesn't exist, or whose instance the caller isn't a
    /// member of, is `NOT_FOUND` either way — no probing.
    async fn update_project(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: UpdateProjectInput,
    ) -> Result<Project<A>> {
        let project = require_project_member(ctx, &*self.app, id.as_str()).await?;
        let fields = validate_project_fields(ProjectFieldsInput {
            name: &input.name,
            client_name: &input.client_name,
            client_abn: input.client_abn.as_deref(),
            client_address: input.client_address.as_deref(),
            client_email: input.client_email.as_deref(),
            reference: input.reference.as_deref(),
            payment_terms_days: input.payment_terms_days,
            default_unit_price_cents: input.default_unit_price_cents,
        })?;
        self.app
            .db()
            .update_project(
                &project.id,
                db::ProjectUpdateShape::Fields {
                    fields: &fields,
                    archived: input.archived,
                },
            )
            .await?;

        Ok(Project::new(db::Project {
            name: fields.name,
            client_name: fields.client_name,
            client_abn: fields.client_abn,
            client_address: fields.client_address,
            client_email: fields.client_email,
            reference: fields.reference,
            payment_terms_days: fields.payment_terms_days,
            default_unit_price_cents: fields.default_unit_price_cents,
            archived: input.archived,
            updated_at: crate::clock::now_sec(),
            ..project
        }))
    }

    // ── Billable items ────────────────────────────────────────────────────────

    /// Record a billable item against a project. Any member of the project's
    /// (invoicing) instance — authorised through the project, like
    /// `updateProject`: `NOT_FOUND` for a missing project and a non-member
    /// alike, and a superuser without a membership is a non-member. An
    /// archived project takes no new items (un-archive it first). See
    /// `BillableItemInput` for the validation rules.
    async fn create_billable_item(
        &self,
        ctx: &Context<'_>,
        project_id: ID,
        input: BillableItemInput,
    ) -> Result<BillableItem<A>> {
        let project = require_project_member(ctx, &*self.app, project_id.as_str()).await?;
        let Some(AuthInfo::User { id: user_id, .. }) = ctx.data_opt::<AuthInfo>() else {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        };
        if project.archived {
            return Err(anyhow!(
                "Project is archived — un-archive it to add billable items"
            ));
        }
        let unit_price_cents = input
            .unit_price_cents
            .or(project.default_unit_price_cents)
            .ok_or_else(|| anyhow!("Unit price is required — this project has no default rate"))?;
        let fields = validate_billable_item_input(&input, unit_price_cents)?;
        let created = self
            .app
            .db()
            .create_billable_item(&db::NewBillableItem {
                instance_id: &project.instance_id,
                project_id: &project.id,
                date: &fields.date,
                description: &fields.description,
                quantity_hundredths: fields.quantity_hundredths,
                unit_price_cents: fields.unit_price_cents,
                gst_free: input.gst_free,
                created_by_user_id: user_id,
            })
            .await?;
        Ok(BillableItem::new(created))
    }

    /// Full-replace a billable item's date/description/quantity/unit price.
    /// Id-only, authorised like `updateProject` (`NOT_FOUND` for missing or
    /// not yours). An unbilled item is a plain conditional update, unchanged
    /// from before invoices existed. An item on a *draft* invoice stays
    /// editable — the write also bumps that invoice's `version` in the same
    /// transaction (`db::Handler::update_billable_item`), conditioned on it
    /// still being a draft. An item on a *finalized* invoice is refused —
    /// checked up front here for a clear message, and again atomically in
    /// the write's condition, so a concurrent finalize is never silently
    /// edited around.
    async fn update_billable_item(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: BillableItemInput,
    ) -> Result<BillableItem<A>> {
        let item = require_billable_item_member(ctx, &*self.app, id.as_str()).await?;
        // Kept (not just checked) when present, so the returned item below
        // can use it as its known parent invoice — see `BillableItem`'s
        // `parent_invoice` doc comment.
        let invoice = if let Some(invoice_id) = &item.invoice_id {
            let invoice = self
                .app
                .db()
                .get_invoice_consistent(invoice_id)
                .await?
                .ok_or_else(|| {
                    ApiError::conflict("Billable item's invoice is missing — reload and try again")
                })?;
            if invoice.status != db::InvoiceStatus::Draft {
                return Err(ApiError::conflict(
                    "Billable item is on a finalized invoice and can no longer be edited",
                )
                .into());
            }
            Some(invoice)
        } else {
            None
        };
        let fields = validate_billable_item_input(
            &input,
            input.unit_price_cents.unwrap_or(item.unit_price_cents),
        )?;
        let written = self
            .app
            .db()
            .update_billable_item(
                &item.id,
                item.invoice_id.as_deref(),
                db::BillableItemUpdateShape::Fields {
                    date: &fields.date,
                    description: &fields.description,
                    quantity_hundredths: fields.quantity_hundredths,
                    unit_price_cents: fields.unit_price_cents,
                    gst_free: input.gst_free,
                },
            )
            .await?;
        if !written {
            // Covers both write shapes' condition failures — the item was
            // deleted, attached to an invoice (unbilled path), or its
            // invoice was finalized concurrently (draft-item path) — so
            // this can't claim a specific cause it hasn't actually checked.
            return Err(ApiError::conflict(
                "Billable item changed concurrently — reload and try again",
            )
            .into());
        }
        let updated = db::BillableItem {
            date: fields.date,
            description: fields.description,
            quantity_hundredths: fields.quantity_hundredths,
            unit_price_cents: fields.unit_price_cents,
            gst_free: input.gst_free,
            updated_at: crate::clock::now_sec(),
            ..item
        };
        Ok(match invoice {
            Some(invoice) => BillableItem::with_parent_invoice(updated, invoice),
            None => BillableItem::new(updated),
        })
    }

    /// Delete a billable item, returning its id (for the client's store).
    /// Unbilled only, regardless of the item's invoice's own status — see
    /// CLAUDE.md's "Invoicing" house rule. Deleting an item `rebillExpense`
    /// made frees its expense to be re-billed again (same transaction).
    async fn delete_billable_item(&self, ctx: &Context<'_>, id: ID) -> Result<ID> {
        let item = require_billable_item_member(ctx, &*self.app, id.as_str()).await?;
        if item.invoice_id.is_some() {
            return Err(ApiError::conflict("Billable item is already on an invoice").into());
        }
        if !self
            .app
            .db()
            .delete_billable_item(&item.id, item.source_expense_id.as_deref())
            .await?
        {
            return Err(ApiError::conflict(
                "Billable item changed (deleted or put on an invoice) — reload and try again",
            )
            .into());
        }
        Ok(ID(item.id))
    }

    // ── Expenses ──────────────────────────────────────────────────────────────

    /// Record an expense in an invoicing instance, optionally against one
    /// of its projects. Any member (owner or agent); superusers without a
    /// membership get nothing. Rejects a support instance. See
    /// `ExpenseInput` for which fields a purchase and a vehicle trip take.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Member(instance_id.to_string()))")]
    async fn create_expense(
        &self,
        ctx: &Context<'_>,
        instance_id: ID,
        input: ExpenseInput,
    ) -> Result<Expense<A>> {
        let Some(AuthInfo::User { id: user_id, .. }) = ctx.data_opt::<AuthInfo>() else {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        };
        let instance = self
            .app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;
        db::require_instance_kind(&instance, db::InstanceKind::Invoicing)
            .map_err(|e| anyhow!(e))?;
        let fields = validate_expense(&*self.app, &instance.id, &input, None).await?;
        let created = self
            .app
            .db()
            .create_expense(&instance.id, &fields, user_id)
            .await?;
        Ok(Expense::new(created))
    }

    /// Full-replace an expense — including its category (a purchase can
    /// become a trip and vice versa) and its project (omit `projectId` to
    /// detach it). Id-only, `NOT_FOUND` for missing or not yours. A trip's
    /// rate is looked up again from its (possibly new) date.
    async fn update_expense(
        &self,
        ctx: &Context<'_>,
        id: ID,
        input: ExpenseInput,
    ) -> Result<Expense<A>> {
        let expense = require_expense_member(ctx, &*self.app, id.as_str()).await?;
        let fields = validate_expense(
            &*self.app,
            &expense.instance_id,
            &input,
            expense.fields.project_id.as_deref(),
        )
        .await?;
        if !self.app.db().update_expense(&expense.id, &fields).await? {
            return Err(ApiError::not_found("Expense", id.as_str()).into());
        }
        Ok(Expense::new(db::Expense {
            fields,
            updated_at: crate::clock::now_sec(),
            ..expense
        }))
    }

    /// Delete an expense, returning its id. Refused (`CONFLICT`) while it
    /// is re-billed — delete the billable item `rebillExpense` made first.
    async fn delete_expense(&self, ctx: &Context<'_>, id: ID) -> Result<ID> {
        let expense = require_expense_member(ctx, &*self.app, id.as_str()).await?;
        if expense.billable_item_id.is_some() {
            return Err(ApiError::conflict(
                "Expense has been re-billed — delete its billable item first",
            )
            .into());
        }
        if !self.app.db().delete_expense(&expense.id).await? {
            return Err(ApiError::conflict(
                "Expense changed (deleted or re-billed) — reload and try again",
            )
            .into());
        }
        Ok(ID(expense.id))
    }

    // ── Invoices ──────────────────────────────────────────────────────────────

    /// Create a draft invoice from a project's unbilled items, in one
    /// transaction (`db::Handler::create_invoice`). Authorised through the
    /// project like `createBillableItem` — `NOT_FOUND` for a missing
    /// project or a non-member alike; an archived project is allowed (you
    /// may be invoicing final work on a job you've since archived). Every
    /// id in `itemIds` must belong to this project and be currently
    /// unbilled (`CONFLICT`/`NOT_FOUND` otherwise), 1–50 of them
    /// (`MAX_INVOICE_ITEMS`), no duplicates.
    async fn create_invoice(
        &self,
        ctx: &Context<'_>,
        project_id: ID,
        item_ids: Vec<ID>,
    ) -> Result<Invoice<A>> {
        let project = require_project_member(ctx, &*self.app, project_id.as_str()).await?;
        let Some(AuthInfo::User { id: user_id, .. }) = ctx.data_opt::<AuthInfo>() else {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        };
        let ids = validate_new_invoice_item_ids(&item_ids, &[])?;
        let items = fetch_eligible_items(&*self.app, &project.id, &ids).await?;

        let instance = self
            .app
            .db()
            .get_instances(&[project.instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", project.instance_id.as_str()))?;
        check_invoice_total_within_safe_integer(&items, instance.gst_registered)?;

        let created = self
            .app
            .db()
            .create_invoice(&project.instance_id, &project.id, &ids, user_id)
            .await?
            .ok_or_else(|| {
                ApiError::conflict(
                    "One or more items changed (deleted or put on another invoice) — reload and try again",
                )
            })?;
        Ok(Invoice::new(created))
    }

    /// Add unbilled items to a draft invoice, in one transaction
    /// (`db::Handler::add_invoice_items`) that also bumps the invoice's
    /// `version`. `NOT_FOUND`/`CONFLICT` for the same reasons as
    /// `createInvoice`; `CONFLICT` if the invoice is already finalized, or
    /// changed concurrently (a stale `version`).
    async fn add_invoice_items(
        &self,
        ctx: &Context<'_>,
        invoice_id: ID,
        item_ids: Vec<ID>,
    ) -> Result<Invoice<A>> {
        let invoice = require_invoice_member(ctx, &*self.app, invoice_id.as_str()).await?;
        if invoice.status != db::InvoiceStatus::Draft {
            return Err(
                ApiError::conflict("Invoice is finalized and can no longer be edited").into(),
            );
        }
        let new_ids = validate_new_invoice_item_ids(&item_ids, &invoice.item_ids)?;
        let new_items = fetch_eligible_items(&*self.app, &invoice.project_id, &new_ids).await?;

        let existing_items: Vec<db::BillableItem> = if invoice.item_ids.is_empty() {
            vec![]
        } else {
            self.app
                .db()
                .get_billable_items(&invoice.item_ids)
                .await?
                .into_iter()
                .flatten()
                .collect()
        };
        let instance = self
            .app
            .db()
            .get_instances(&[invoice.instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", invoice.instance_id.as_str()))?;
        let mut all_items = existing_items;
        all_items.extend(new_items);
        check_invoice_total_within_safe_integer(&all_items, instance.gst_registered)?;

        let committed = self
            .app
            .db()
            .add_invoice_items(&invoice.id, &invoice.project_id, &new_ids, invoice.version)
            .await?;
        if !committed {
            return Err(
                ApiError::conflict("Invoice changed concurrently — reload and try again").into(),
            );
        }
        let mut item_ids_result = invoice.item_ids.clone();
        item_ids_result.extend(new_ids);
        Ok(Invoice::new(db::Invoice {
            item_ids: item_ids_result,
            version: invoice.version + 1,
            updated_at: crate::clock::now_sec(),
            ..invoice
        }))
    }

    /// Remove items from a draft invoice, in one transaction
    /// (`db::Handler::remove_invoice_items`) that also bumps the invoice's
    /// `version`. Removing every item (an empty draft) is allowed —
    /// finalizing one is not. `CONFLICT` if the invoice is finalized, an id
    /// isn't currently on it, or the invoice changed concurrently.
    async fn remove_invoice_items(
        &self,
        ctx: &Context<'_>,
        invoice_id: ID,
        item_ids: Vec<ID>,
    ) -> Result<Invoice<A>> {
        let invoice = require_invoice_member(ctx, &*self.app, invoice_id.as_str()).await?;
        if invoice.status != db::InvoiceStatus::Draft {
            return Err(
                ApiError::conflict("Invoice is finalized and can no longer be edited").into(),
            );
        }
        let remove_ids = validate_remove_invoice_item_ids(&item_ids, &invoice.item_ids)?;
        let committed = self
            .app
            .db()
            .remove_invoice_items(&invoice.id, &remove_ids, invoice.version)
            .await?;
        if !committed {
            return Err(
                ApiError::conflict("Invoice changed concurrently — reload and try again").into(),
            );
        }
        let item_ids_result: Vec<String> = invoice
            .item_ids
            .iter()
            .filter(|id| !remove_ids.contains(id))
            .cloned()
            .collect();
        Ok(Invoice::new(db::Invoice {
            item_ids: item_ids_result,
            version: invoice.version + 1,
            updated_at: crate::clock::now_sec(),
            ..invoice
        }))
    }

    /// Delete a draft invoice, returning its id. Draft only — a `Delete` on
    /// the invoice plus `REMOVE invoice_id` on each of its items, in one
    /// transaction (`db::Handler::delete_invoice`) conditioned on the
    /// `version` this resolver just read.
    async fn delete_invoice(&self, ctx: &Context<'_>, invoice_id: ID) -> Result<ID> {
        let invoice = require_invoice_member(ctx, &*self.app, invoice_id.as_str()).await?;
        if invoice.status != db::InvoiceStatus::Draft {
            return Err(ApiError::conflict("Only a draft invoice can be deleted").into());
        }
        let committed = self
            .app
            .db()
            .delete_invoice(&invoice.id, &invoice.item_ids, invoice.version)
            .await?;
        if !committed {
            return Err(
                ApiError::conflict("Invoice changed concurrently — reload and try again").into(),
            );
        }
        Ok(ID(invoice.id))
    }

    /// Finalize a draft invoice — CLAUDE.md's "finalize algorithm", steps
    /// 1–6: re-read the items this resolver's `require_invoice_member`
    /// fetch named (consistently, so this sees each item's just-attached
    /// `invoice_id`), read the project and instance (requiring
    /// `businessName` to be set), allocate the next number from the
    /// per-instance counter, build the frozen snapshot, and write it all in
    /// one conditional transaction with the number's reservation row
    /// (`db::Handler::finalize_invoice`). Strictly final — no void, no
    /// un-finalize; only `setInvoicePaid` may touch a finalized invoice
    /// afterward. `CONFLICT` if the draft changed concurrently (a stale
    /// `version`, checked both up front — via the consistent items read —
    /// and in the final write's own condition).
    ///
    /// `issueDate` may be any valid date, past or future — backdating is how
    /// an existing invoice is imported as it was issued. `number`, when given,
    /// replaces step 4's counter allocation with exactly that number — for
    /// importing an invoice under its original number. Owner-only, like
    /// `setNextInvoiceNumber`, since it can move the same counter. Any number
    /// no invoice in the instance already has is accepted, lower than the
    /// counter or not; a used one is `CONFLICT`, and the invoice stays a
    /// draft. A number above the counter moves the counter up to it first,
    /// so automatic numbering never runs into it. The guarantee against a
    /// duplicate is the number's reservation row, written in the same
    /// transaction as the finalize (`db::Handler::finalize_invoice`).
    ///
    /// `dueDate` (`YYYY-MM-DD`, on or after `issueDate`) defaults to
    /// `issueDate` + the project's payment terms (else the instance's, else
    /// 14 days); it is frozen onto the row and printed on the invoice.
    async fn finalize_invoice(
        &self,
        ctx: &Context<'_>,
        invoice_id: ID,
        issue_date: String,
        number: Option<i32>,
        due_date: Option<String>,
    ) -> Result<Invoice<A>> {
        let invoice = require_invoice_member(ctx, &*self.app, invoice_id.as_str()).await?;
        let Some(AuthInfo::User {
            id: user_id,
            memberships,
            ..
        }) = ctx.data_opt::<AuthInfo>()
        else {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        };
        let explicit_number = match number {
            None => None,
            Some(_) if !is_owner(memberships, &invoice.instance_id) => {
                return Err(ApiError::forbidden(
                    "Only an owner of this instance can choose an invoice number",
                )
                .into());
            }
            Some(n) if n < 1 => return Err(anyhow!("number must be at least 1")),
            Some(n) => Some(u32::try_from(n).unwrap_or(0)),
        };
        if invoice.status != db::InvoiceStatus::Draft {
            return Err(ApiError::conflict("Invoice is already finalized").into());
        }
        if invoice.item_ids.is_empty() {
            return Err(anyhow!("Add at least one item before finalizing"));
        }
        let issue_date = invoicing::validate_item_date(&issue_date).map_err(|e| anyhow!(e))?;

        // Step 2: a consistent BatchGetItem, verifying every item still
        // points back at this invoice — closes the race between this
        // resolver's own (consistent) invoice read and a concurrent detach.
        let fetched = self
            .app
            .db()
            .get_billable_items_consistent(&invoice.item_ids)
            .await?;
        let mut resolved_items = Vec::with_capacity(invoice.item_ids.len());
        for (id, item) in invoice.item_ids.iter().zip(fetched) {
            let item = item.ok_or_else(|| {
                ApiError::conflict(format!(
                    "Billable item {id} is missing — reload and try again"
                ))
            })?;
            if item.invoice_id.as_deref() != Some(invoice.id.as_str()) {
                return Err(ApiError::conflict(
                    "Invoice changed concurrently — reload and try again",
                )
                .into());
            }
            resolved_items.push(item);
        }

        // Step 3: project + instance, requiring business_name.
        let project = self
            .app
            .db()
            .get_projects(&[invoice.project_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Project", invoice.project_id.as_str()))?;
        let instance = self
            .app
            .db()
            .get_instances(&[invoice.instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", invoice.instance_id.as_str()))?;
        if instance
            .business_name
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            return Err(anyhow!("Complete the invoicing settings first"));
        }

        let due_date = match due_date.as_deref() {
            Some(d) => invoicing::validate_item_date(d).map_err(|e| anyhow!(e))?,
            None => invoicing::add_days(&issue_date, project.payment_terms_days(&instance))
                .map_err(|e| anyhow!(e))?,
        };
        if due_date < issue_date {
            return Err(anyhow!("The due date can't be before the issue date"));
        }

        // Step 4: allocate the number — the counter's next, or the caller's
        // explicit choice.
        let number = match explicit_number {
            None => {
                let n = self
                    .app
                    .db()
                    .increment_invoice_counter(&invoice.instance_id)
                    .await?;
                u32::try_from(n).unwrap_or(u32::MAX)
            }
            Some(n) => {
                if self
                    .app
                    .db()
                    .invoice_number_used(&invoice.instance_id, n)
                    .await?
                {
                    return Err(invoice_number_used(n).into());
                }
                // Forward-only, so a no-op for a number at or below the
                // counter; above it, keeps the automatic sequence clear of `n`.
                self.app
                    .db()
                    .set_next_invoice_number(&invoice.instance_id, u64::from(n))
                    .await?;
                n
            }
        };

        // Step 5: build the snapshot.
        let snapshot = invoicing::snapshot::build_snapshot(
            &instance,
            &project,
            &resolved_items,
            Some(number),
            Some(&issue_date),
            Some(&due_date),
        );
        invoicing::validate_total_within_safe_integer(snapshot.total_cents)
            .map_err(|e| anyhow!(e))?;
        let snapshot_json = serde_json::to_string(&snapshot)
            .map_err(|e| anyhow!("Failed to serialize invoice snapshot: {e}"))?;

        // Step 6: the conditional write, with the number's reservation. A
        // failure here leaves a counter-allocated `number` unused — a gap,
        // never a duplicate (see `SCHEMA.md`'s "Known issues"); an explicit
        // one is simply still free, so a retry can claim it.
        let committed = self
            .app
            .db()
            .finalize_invoice(
                &invoice.instance_id,
                &invoice.id,
                invoice.version,
                &db::FinalizeInvoice {
                    number,
                    issue_date: &issue_date,
                    due_date: &due_date,
                    snapshot_json: &snapshot_json,
                    total_cents: snapshot.total_cents,
                    gst_cents: snapshot.gst_cents,
                    finalized_by_user_id: user_id,
                },
            )
            .await?;
        if !committed {
            // Either condition can fail; say which, when it's the number.
            if self
                .app
                .db()
                .invoice_number_used(&invoice.instance_id, number)
                .await?
            {
                return Err(invoice_number_used(number).into());
            }
            return Err(
                ApiError::conflict("Invoice changed concurrently — reload and try again").into(),
            );
        }

        let now = crate::clock::now_sec();
        Ok(Invoice::new(db::Invoice {
            status: db::InvoiceStatus::Finalized,
            version: invoice.version + 1,
            number: Some(number),
            issue_date: Some(issue_date),
            due_date: Some(due_date),
            snapshot: Some(snapshot_json),
            total_cents: Some(snapshot.total_cents),
            gst_cents: Some(snapshot.gst_cents),
            finalized_at: Some(now),
            finalized_by_user_id: Some(user_id.clone()),
            updated_at: now,
            ..invoice
        }))
    }

    /// Mark a finalized invoice paid in full on `paidDate` — a shortcut for
    /// `recordInvoicePayment` of its whole remaining balance — or, with
    /// `null`, remove every payment recorded against it. `CONFLICT` on a
    /// draft, when `paidDate` is given but nothing is owed, or if the
    /// invoice changed concurrently.
    async fn set_invoice_paid(
        &self,
        ctx: &Context<'_>,
        invoice_id: ID,
        paid_date: Option<String>,
    ) -> Result<Invoice<A>> {
        let invoice = require_invoice_member(ctx, &*self.app, invoice_id.as_str()).await?;
        let user_id = require_user_id(ctx)?;
        if invoice.status != db::InvoiceStatus::Finalized {
            return Err(ApiError::conflict("Only a finalized invoice can be marked paid").into());
        }
        let payments = match &paid_date {
            Some(d) => {
                let date = invoicing::validate_item_date(d).map_err(|e| anyhow!(e))?;
                let balance = invoice.balance_cents();
                if balance <= 0 {
                    return Err(ApiError::conflict("Invoice is already paid").into());
                }
                let mut payments = invoice.payments.clone();
                payments.push(new_payment(&date, balance, None, &user_id));
                payments
            }
            None => Vec::new(),
        };
        write_payments(&*self.app, invoice, payments).await
    }

    /// Record a payment received against a finalized invoice: `amountCents`
    /// (GST-inclusive, > 0, no more than the balance owing) on `date`. When
    /// it settles the balance the invoice becomes PAID, dated by the latest
    /// payment. `CONFLICT` on a draft, an already-settled invoice, or a
    /// concurrent change.
    async fn record_invoice_payment(
        &self,
        ctx: &Context<'_>,
        invoice_id: ID,
        input: RecordPaymentInput,
    ) -> Result<Invoice<A>> {
        let invoice = require_invoice_member(ctx, &*self.app, invoice_id.as_str()).await?;
        let user_id = require_user_id(ctx)?;
        if invoice.status != db::InvoiceStatus::Finalized {
            return Err(ApiError::conflict("Only a finalized invoice can take payments").into());
        }
        let date = invoicing::validate_item_date(&input.date).map_err(|e| anyhow!(e))?;
        let note = invoicing::ledger::validate_payment_note(input.note.as_deref())
            .map_err(|e| anyhow!(e))?;
        let balance = invoice.balance_cents();
        if balance <= 0 {
            return Err(ApiError::conflict("Invoice is already paid").into());
        }
        if input.amount_cents <= 0 {
            return Err(anyhow!("Amount must be greater than zero"));
        }
        if input.amount_cents > balance {
            return Err(anyhow!(
                "Amount is more than the {} still owing",
                invoicing::money::format_cents(balance)
            ));
        }
        if invoice.payments.len() >= invoicing::ledger::MAX_PAYMENTS_PER_INVOICE {
            return Err(anyhow!(
                "An invoice can have at most {} payments",
                invoicing::ledger::MAX_PAYMENTS_PER_INVOICE
            ));
        }
        let mut payments = invoice.payments.clone();
        payments.push(new_payment(&date, input.amount_cents, note, &user_id));
        write_payments(&*self.app, invoice, payments).await
    }

    /// Remove one recorded payment (a mistake, a bounced transfer). The
    /// invoice goes back to UNPAID if that leaves anything owing.
    /// `NOT_FOUND` for a payment id not on this invoice.
    async fn delete_invoice_payment(
        &self,
        ctx: &Context<'_>,
        invoice_id: ID,
        payment_id: ID,
    ) -> Result<Invoice<A>> {
        let invoice = require_invoice_member(ctx, &*self.app, invoice_id.as_str()).await?;
        if !invoice.payments.iter().any(|p| p.id == payment_id.as_str()) {
            return Err(ApiError::not_found("Payment", payment_id.as_str()).into());
        }
        let payments = invoice
            .payments
            .iter()
            .filter(|p| p.id != payment_id.as_str())
            .cloned()
            .collect();
        write_payments(&*self.app, invoice, payments).await
    }

    /// **Sends email.** Mail a finalized invoice to the client with its PDF
    /// attached: to `input.to` (default: the project's client email) and
    /// `input.cc`, at most 10 in total, with an optional covering
    /// `message`. Sent from the system sender under the business name, with
    /// `Reply-To` the business email — see `invoicing::send`. Sending is
    /// this mutation's whole point, so a send failure fails it (nothing is
    /// recorded); on success `sentAt`/`sentTo` are updated. Can be sent
    /// again (a reminder). `CONFLICT` on a draft.
    async fn send_invoice(
        &self,
        ctx: &Context<'_>,
        invoice_id: ID,
        #[graphql(default)] input: SendDocumentInput,
    ) -> Result<Invoice<A>> {
        let invoice = require_invoice_member(ctx, &*self.app, invoice_id.as_str()).await?;
        if invoice.status != db::InvoiceStatus::Finalized {
            return Err(ApiError::conflict("Only a finalized invoice can be sent").into());
        }
        let project = self
            .app
            .db()
            .get_projects(&[invoice.project_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Project", invoice.project_id.as_str()))?;
        let (to, cc, message) = resolve_send_input(&input, project.client_email.as_deref())?;
        let snapshot = invoice_snapshot(&invoice)?;
        let filename = invoice_pdf_filename(&invoice);
        let (key, bytes) = document_pdf(
            &*self.app,
            invoice.pdf_s3_key.as_deref(),
            &snapshot,
            &format!("invoices/{}/{}/{filename}", invoice.instance_id, invoice.id),
        )
        .await?;
        if invoice.pdf_s3_key.is_none() {
            self.app.db().set_invoice_pdf_key(&invoice.id, &key).await?;
        }
        let built = invoicing::send::build(
            &snapshot,
            invoice.balance_cents(),
            &bytes,
            &filename,
            &to,
            &cc,
            message.as_deref(),
        )?;
        self.app
            .mail()
            .send_raw(&built.raw, &built.to, &built.cc)
            .await
            .map_err(|e| anyhow!("Sending the invoice failed: {e}"))?;
        let now = crate::clock::now_sec();
        let mut sent_to = to.clone();
        sent_to.extend(cc);
        self.app
            .db()
            .set_invoice_sent(&invoice.id, now, &sent_to)
            .await?;
        info!(invoice_id = %invoice.id, "sent invoice");
        Ok(Invoice::new(db::Invoice {
            sent_at: Some(now),
            sent_to,
            pdf_s3_key: Some(key),
            ..invoice
        }))
    }

    /// Download a finalized invoice's PDF: a presigned URL with
    /// `Content-Disposition: attachment; filename="Invoice-{displayNumber}.pdf"`
    /// (`storage::Handler::presign_get_download`, filename sanitised there).
    /// `CONFLICT` on a draft — finalization is what freezes the content a
    /// PDF prints (CLAUDE.md's "Invoicing" house rule), so there's nothing
    /// yet to render.
    ///
    /// This is a mutation, not a query, because the *first* call for a
    /// given invoice writes: it renders the frozen `snapshot` (the only input
    /// `invoicing::pdf::render_invoice_pdf` reads), stores it, and caches the
    /// key on the row (`db::Handler::set_invoice_pdf_key`). Every later call
    /// presigns the cached key. Rendering is deterministic from the snapshot,
    /// so two callers racing the first render each store a correct PDF of
    /// the same invoice — see `invoicing::pdf`'s doc comment.
    async fn download_invoice_pdf(&self, ctx: &Context<'_>, invoice_id: ID) -> Result<String> {
        let invoice = require_invoice_member(ctx, &*self.app, invoice_id.as_str()).await?;
        if invoice.status != db::InvoiceStatus::Finalized {
            return Err(ApiError::conflict("Only a finalized invoice can be downloaded").into());
        }
        let filename = invoice_pdf_filename(&invoice);
        let key = match invoice.pdf_s3_key.clone() {
            Some(key) => key,
            None => {
                let snapshot = invoice_snapshot(&invoice)?;
                let (key, _) = document_pdf(
                    &*self.app,
                    None,
                    &snapshot,
                    &format!("invoices/{}/{}/{filename}", invoice.instance_id, invoice.id),
                )
                .await?;
                self.app.db().set_invoice_pdf_key(&invoice.id, &key).await?;
                key
            }
        };
        self.app
            .storage()
            .presign_get_download(&key, &filename)
            .await
    }

    /// **Irreversible.** Issue a credit note (an adjustment note, when the
    /// invoice was a tax invoice) against a finalized invoice: numbered from
    /// its own `CN-` sequence, frozen at once, and never editable or
    /// deletable — the correction for a finalized invoice, which itself can
    /// never change. `input.lines` are GST-exclusive amounts to credit;
    /// omit them to credit the whole invoice (only while nothing has been
    /// credited against it). The credit note's total (with GST, at the
    /// invoice's own GST registration) can't exceed what's left
    /// uncredited. The invoice's balance drops by that total, and if that
    /// settles it, it becomes PAID as of the credit note's date. Any member;
    /// sends no email (see `sendCreditNote`). `CONFLICT` on a draft or a
    /// concurrent change.
    async fn issue_credit_note(
        &self,
        ctx: &Context<'_>,
        invoice_id: ID,
        input: CreditNoteInput,
    ) -> Result<CreditNote<A>> {
        let invoice = require_invoice_member(ctx, &*self.app, invoice_id.as_str()).await?;
        let user_id = require_user_id(ctx)?;
        if invoice.status != db::InvoiceStatus::Finalized {
            return Err(
                ApiError::conflict("A credit note can only adjust a finalized invoice").into(),
            );
        }
        let issue_date =
            invoicing::validate_item_date(&input.issue_date).map_err(|e| anyhow!(e))?;
        if invoice
            .issue_date
            .as_deref()
            .is_some_and(|d| issue_date.as_str() < d)
        {
            return Err(anyhow!(
                "A credit note can't be dated before the invoice it adjusts"
            ));
        }
        let reason = input.reason.trim().to_string();
        if reason.is_empty() {
            return Err(anyhow!("Give a reason for the credit note"));
        }
        if reason.chars().count() > MAX_CREDIT_NOTE_REASON_LEN {
            return Err(anyhow!(
                "Reason cannot be longer than {MAX_CREDIT_NOTE_REASON_LEN} characters"
            ));
        }
        let invoice_snap = invoice_snapshot(&invoice)?;
        let lines = match &input.lines {
            None => {
                if invoice.credited_cents > 0 {
                    return Err(anyhow!(
                        "This invoice already has a credit note — list the lines to credit"
                    ));
                }
                invoicing::snapshot::full_credit_lines(&invoice_snap)
            }
            Some(lines) => {
                if lines.is_empty() || lines.len() > MAX_INVOICE_ITEMS {
                    return Err(anyhow!(
                        "A credit note needs between 1 and {MAX_INVOICE_ITEMS} lines"
                    ));
                }
                lines
                    .iter()
                    .map(|l| {
                        if l.amount_cents <= 0 {
                            return Err(anyhow!("Each line's amount must be greater than zero"));
                        }
                        invoicing::validate_unit_price_cents(l.amount_cents)
                            .map_err(|e| anyhow!(e))?;
                        Ok(invoicing::snapshot::CreditLine {
                            description: invoicing::validate_description(&l.description)
                                .map_err(|e| anyhow!(e))?,
                            amount_cents: l.amount_cents,
                            gst_free: l.gst_free,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?
            }
        };
        // Check the total against what's left before spending a number on it.
        let trial = invoicing::snapshot::build_credit_note_snapshot(
            &invoice_snap,
            0,
            &issue_date,
            &reason,
            &lines,
        );
        let creditable = invoice.total_cents.unwrap_or(0) - invoice.credited_cents;
        if trial.total_cents > creditable {
            return Err(anyhow!(
                "That credits {} but only {} of this invoice is left to credit",
                invoicing::money::format_cents(trial.total_cents),
                invoicing::money::format_cents(creditable.max(0))
            ));
        }

        let number = self
            .app
            .db()
            .increment_credit_note_counter(&invoice.instance_id)
            .await?;
        let number = u32::try_from(number).unwrap_or(u32::MAX);
        let snapshot = invoicing::snapshot::build_credit_note_snapshot(
            &invoice_snap,
            number,
            &issue_date,
            &reason,
            &lines,
        );
        let note = db::CreditNote {
            id: crate::dynamodb::new_id(),
            instance_id: invoice.instance_id.clone(),
            invoice_id: invoice.id.clone(),
            project_id: invoice.project_id.clone(),
            number,
            issue_date: issue_date.clone(),
            reason,
            snapshot: serde_json::to_string(&snapshot)
                .map_err(|e| anyhow!("Failed to serialize credit note snapshot: {e}"))?,
            subtotal_cents: snapshot.subtotal_cents,
            gst_cents: snapshot.gst_cents,
            total_cents: snapshot.total_cents,
            created_by_user_id: user_id,
            created_at: crate::clock::now_sec(),
            pdf_s3_key: None,
            sent_at: None,
            sent_to: Vec::new(),
        };
        let settled = if invoice.paid_date.is_none() {
            invoicing::ledger::settled_date(
                invoice.total_cents.unwrap_or(0),
                invoice.credited_cents + note.total_cents,
                &invoice.payments,
                &issue_date,
            )
        } else {
            None
        };
        let committed = self
            .app
            .db()
            .create_credit_note(&note, invoice.version, settled.as_deref())
            .await?;
        if !committed {
            // The number is now a gap, never a duplicate — like an invoice
            // number lost to a failed finalize.
            return Err(
                ApiError::conflict("Invoice changed concurrently — reload and try again").into(),
            );
        }
        Ok(CreditNote::new(note))
    }

    /// Download a credit note's PDF — `downloadInvoicePdf`'s twin: rendered
    /// from the frozen snapshot on first call, cached after.
    async fn download_credit_note_pdf(
        &self,
        ctx: &Context<'_>,
        credit_note_id: ID,
    ) -> Result<String> {
        let note = require_credit_note_member(ctx, &*self.app, credit_note_id.as_str()).await?;
        let filename = credit_note_pdf_filename(&note);
        let key = match note.pdf_s3_key.clone() {
            Some(key) => key,
            None => {
                let snapshot = credit_note_snapshot(&note)?;
                let (key, _) = document_pdf(
                    &*self.app,
                    None,
                    &snapshot,
                    &format!("credit-notes/{}/{}/{filename}", note.instance_id, note.id),
                )
                .await?;
                self.app
                    .db()
                    .set_credit_note_pdf_key(&note.id, &key)
                    .await?;
                key
            }
        };
        self.app
            .storage()
            .presign_get_download(&key, &filename)
            .await
    }

    /// **Sends email.** `sendInvoice` for a credit note: mails it with its
    /// PDF to `input.to` (default: the project's client email) and `cc`.
    async fn send_credit_note(
        &self,
        ctx: &Context<'_>,
        credit_note_id: ID,
        #[graphql(default)] input: SendDocumentInput,
    ) -> Result<CreditNote<A>> {
        let note = require_credit_note_member(ctx, &*self.app, credit_note_id.as_str()).await?;
        let project = self
            .app
            .db()
            .get_projects(&[note.project_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Project", note.project_id.as_str()))?;
        let (to, cc, message) = resolve_send_input(&input, project.client_email.as_deref())?;
        let snapshot = credit_note_snapshot(&note)?;
        let filename = credit_note_pdf_filename(&note);
        let (key, bytes) = document_pdf(
            &*self.app,
            note.pdf_s3_key.as_deref(),
            &snapshot,
            &format!("credit-notes/{}/{}/{filename}", note.instance_id, note.id),
        )
        .await?;
        if note.pdf_s3_key.is_none() {
            self.app
                .db()
                .set_credit_note_pdf_key(&note.id, &key)
                .await?;
        }
        let built = invoicing::send::build(
            &snapshot,
            0,
            &bytes,
            &filename,
            &to,
            &cc,
            message.as_deref(),
        )?;
        self.app
            .mail()
            .send_raw(&built.raw, &built.to, &built.cc)
            .await
            .map_err(|e| anyhow!("Sending the credit note failed: {e}"))?;
        let now = crate::clock::now_sec();
        let mut sent_to = to.clone();
        sent_to.extend(cc);
        self.app
            .db()
            .set_credit_note_sent(&note.id, now, &sent_to)
            .await?;
        Ok(CreditNote::new(db::CreditNote {
            sent_at: Some(now),
            sent_to,
            pdf_s3_key: Some(key),
            ..note
        }))
    }

    /// Re-bill an expense to its project's client: create an UNBILLED
    /// billable item for its GST-exclusive cost plus `markupPercent`
    /// (default 0, at most 1000, 2 dp), dated like the expense, linked to it
    /// so it can't be billed twice. `description` defaults to the
    /// expense's category and supplier. `gstFree` (default `false`) marks
    /// the new line GST-free. Refused for an expense with no project, on an
    /// archived project, or already re-billed (`CONFLICT`). Deleting the
    /// item frees the expense to be re-billed again.
    async fn rebill_expense(
        &self,
        ctx: &Context<'_>,
        expense_id: ID,
        markup_percent: Option<String>,
        description: Option<String>,
        #[graphql(default)] gst_free: bool,
    ) -> Result<BillableItem<A>> {
        let expense = require_expense_member(ctx, &*self.app, expense_id.as_str()).await?;
        let user_id = require_user_id(ctx)?;
        if expense.billable_item_id.is_some() {
            return Err(ApiError::conflict("Expense has already been re-billed").into());
        }
        let project_id = expense
            .fields
            .project_id
            .clone()
            .ok_or_else(|| anyhow!("Put the expense on a project before re-billing it"))?;
        let project = self
            .app
            .db()
            .get_projects(&[project_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Project", project_id.as_str()))?;
        if project.archived {
            return Err(anyhow!(
                "Project is archived — un-archive it to add billable items"
            ));
        }
        let markup = invoicing::parse_markup_basis_points(markup_percent.as_deref().unwrap_or(""))
            .map_err(|e| anyhow!(e))?;
        let unit_price_cents =
            invoicing::rebill_unit_price_cents(expense.amount_ex_gst_cents(), markup);
        invoicing::validate_unit_price_cents(unit_price_cents).map_err(|e| anyhow!(e))?;
        let description = match description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            Some(d) => d.to_string(),
            None => rebill_description(&expense),
        };
        let description = invoicing::validate_description(&description).map_err(|e| anyhow!(e))?;
        let created = self
            .app
            .db()
            .rebill_expense(
                &expense.id,
                &db::NewBillableItem {
                    instance_id: &expense.instance_id,
                    project_id: &project.id,
                    date: &expense.fields.date,
                    description: &description,
                    quantity_hundredths: 100,
                    unit_price_cents,
                    gst_free,
                    created_by_user_id: &user_id,
                },
            )
            .await?
            .ok_or_else(|| {
                ApiError::conflict("Expense changed (deleted or re-billed) — reload and try again")
            })?;
        Ok(BillableItem::new(created))
    }

    /// Mint a presigned PUT for an expense's receipt, at
    /// `receipts/{instanceId}/{expenseId}/{random}/{filename}`. Upload the
    /// file there, then call `attachExpenseReceipt` with the returned key.
    async fn create_expense_receipt_upload(
        &self,
        ctx: &Context<'_>,
        expense_id: ID,
        filename: String,
        content_type: String,
    ) -> Result<AttachmentUpload> {
        let expense = require_expense_member(ctx, &*self.app, expense_id.as_str()).await?;
        let safe_name = attachments::sanitize_filename(Some(&filename), 0);
        let key = format!(
            "{}{}/{safe_name}",
            receipt_key_prefix(&expense),
            crate::dynamodb::new_id()
        );
        let upload_url = self.app.storage().presign_put(&key, &content_type).await?;
        Ok(AttachmentUpload { key, upload_url })
    }

    /// Attach an uploaded receipt (a key from `createExpenseReceiptUpload`
    /// for this same expense) to the expense, replacing any earlier one.
    /// Refused for a key belonging to another expense, a file that wasn't
    /// uploaded, or one over 20 MB.
    async fn attach_expense_receipt(
        &self,
        ctx: &Context<'_>,
        expense_id: ID,
        key: String,
        content_type: String,
    ) -> Result<Expense<A>> {
        let expense = require_expense_member(ctx, &*self.app, expense_id.as_str()).await?;
        let prefix = receipt_key_prefix(&expense);
        let filename = key
            .strip_prefix(&prefix)
            .and_then(|rest| rest.split_once('/'))
            .map(|(_, name)| name)
            .filter(|name| !name.is_empty() && !name.contains('/'))
            .ok_or_else(|| ApiError::forbidden("That upload doesn't belong to this expense"))?
            .to_string();
        let size = self
            .app
            .storage()
            .object_size(&key)
            .await
            .map_err(|_| anyhow!("The receipt hasn't finished uploading"))?;
        if size > MAX_RECEIPT_BYTES {
            return Err(anyhow!("A receipt can be at most 20 MB"));
        }
        let content_type = match content_type.trim() {
            "" => "application/octet-stream".to_string(),
            t => t.chars().take(100).collect(),
        };
        let receipt = db::ExpenseReceipt {
            s3_key: key,
            filename,
            content_type,
            size,
        };
        if !self
            .app
            .db()
            .set_expense_receipt(&expense.id, Some(&receipt))
            .await?
        {
            return Err(ApiError::not_found("Expense", expense_id.as_str()).into());
        }
        Ok(Expense::new(db::Expense {
            receipt: Some(receipt),
            updated_at: crate::clock::now_sec(),
            ..expense
        }))
    }

    /// Detach an expense's receipt.
    async fn remove_expense_receipt(
        &self,
        ctx: &Context<'_>,
        expense_id: ID,
    ) -> Result<Expense<A>> {
        let expense = require_expense_member(ctx, &*self.app, expense_id.as_str()).await?;
        if !self.app.db().set_expense_receipt(&expense.id, None).await? {
            return Err(ApiError::not_found("Expense", expense_id.as_str()).into());
        }
        Ok(Expense::new(db::Expense {
            receipt: None,
            updated_at: crate::clock::now_sec(),
            ..expense
        }))
    }

    /// A presigned download URL for an expense's receipt. `NOT_FOUND` when
    /// it has none.
    async fn download_expense_receipt(&self, ctx: &Context<'_>, expense_id: ID) -> Result<String> {
        let expense = require_expense_member(ctx, &*self.app, expense_id.as_str()).await?;
        let receipt = expense
            .receipt
            .as_ref()
            .ok_or_else(|| ApiError::not_found("Receipt", expense_id.as_str()))?;
        self.app
            .storage()
            .presign_get_download(&receipt.s3_key, &receipt.filename)
            .await
    }

    /// Owner-or-superuser: set the next invoice number to be assigned by
    /// `finalizeInvoice` (`db::Handler::set_next_invoice_number` writes
    /// `next - 1` to the counter). Forward-only — `CONFLICT` if `next`
    /// would move the counter backward. `next` must be at least 1.
    #[graphql(
        guard = "AuthGuard::new(AuthRequirement::InstanceOwnerOrSuperuser(instance_id.to_string()))"
    )]
    async fn set_next_invoice_number(&self, instance_id: ID, next: i32) -> Result<Instance<A>> {
        let instance = self
            .app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;
        db::require_instance_kind(&instance, db::InstanceKind::Invoicing)
            .map_err(|e| anyhow!(e))?;
        if next < 1 {
            return Err(anyhow!("next must be at least 1"));
        }
        let new_value = u64::try_from(next - 1).unwrap_or(0);
        let committed = self
            .app
            .db()
            .set_next_invoice_number(&instance.id, new_value)
            .await?;
        if !committed {
            return Err(ApiError::conflict("The next invoice number can only move forward").into());
        }
        Ok(Instance::new(instance))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanitized fixture, hand-constructed (not captured from a real device) to
    /// match `webauthn-rs` 0.5.5's `Passkey` serde shape exactly — every field
    /// name and enum variant string below was read straight out of that crate's
    /// source (`interface.rs`, `webauthn-rs-proto`), not guessed. Key bytes are
    /// zeroed. This test exists to catch a `webauthn-rs` serde format change
    /// during a future library upgrade — if deserialization breaks here, stored
    /// passkeys in DynamoDB are at risk of the same break.
    const PASSKEY_JSON_V0_5: &str = r#"{"cred":{"cred_id":"AAAAAAAAAAAAAAAAAAAAAAAAAAAA","cred":{"type_":"ES256","key":{"EC_EC2":{"curve":"SECP256R1","x":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","y":"BAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}},"counter":0,"transports":null,"user_verified":true,"backup_eligible":true,"backup_state":true,"registration_policy":"preferred","extensions":{"cred_protect":"NotRequested","hmac_create_secret":"NotRequested","appid":"NotRequested","cred_props":"Ignored"},"attestation":{"data":"None","metadata":"None"},"attestation_format":"none"}}"#;

    #[test]
    fn passkey_json_round_trips() {
        use webauthn_rs::prelude::Passkey;
        let passkey: Passkey = serde_json::from_str(PASSKEY_JSON_V0_5).expect(
            "stored passkey JSON must deserialize — format changed after a webauthn-rs upgrade?",
        );
        let reserialized =
            serde_json::to_string(&passkey).expect("passkey must reserialize to JSON");
        let reparsed: Passkey =
            serde_json::from_str(&reserialized).expect("reserialized passkey must round-trip");
        let rereserialized =
            serde_json::to_string(&reparsed).expect("reparsed passkey must reserialize");
        assert_eq!(
            reserialized, rereserialized,
            "passkey JSON must be stable across serde round trips"
        );
    }

    #[test]
    fn sha256_hex_is_stable_and_matches_length() {
        let h = sha256_hex("123456");
        assert_eq!(h, sha256_hex("123456"));
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(h, sha256_hex("654321"));
    }
}
