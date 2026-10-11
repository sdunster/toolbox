//! The database abstraction: [`Handler`] trait, [`Error`], and the row/paging types
//! every backend and every later domain module builds on.
//!
//! Step 3 (auth) adds `user`, `login_code`, `user_token`, `webauthn_credential`
//! and `ephemeral_state` below; step 4 adds `instance`/`membership`; step 5 adds
//! `ticket`/`ticket_message`/`counter`. Each of those steps grows [`Handler`]
//! with the methods it needs, following the shape of
//! [`crate::dynamodb::Handler`]'s generic infrastructure and
//! [`crate::mockdb::Handler`]'s all-fail mock.

use std::future::Future;
use thiserror::Error;

/// These errors are separated into groups because callers want to handle them
/// differently (e.g. `NotFound` from a lookup is often fine to surface to the user;
/// `Infrastructure` usually isn't).
#[derive(Error, Debug)]
pub enum Error {
    /// Returned when a queried record does not exist.
    #[error("Record not found: {0}")]
    NotFound(String),
    /// Returned when a DB row cannot be deserialized into the expected type.
    #[error("Hydration error: {0}")]
    Hydration(String),
    /// An unexpected error, probably fine to ignore and retry.
    #[error("Infrastructure error: {0}")]
    Infrastructure(String),
    /// Returned when a row violates an expected data-integrity invariant.
    #[error("Data integrity error: {0}")]
    Integrity(String),
    /// Type conversion error, e.g. converting a string ID to an integer.
    #[error("Data type conversion error: {0}")]
    TypeConversion(String),
    #[error("Mutation disabled")]
    MutationDisabled,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Collapse the results of a lookup on an attribute that is *expected* to be unique
/// (but not enforced as unique by the data model) down to at most one row. Returns
/// an [`Error::Integrity`] if more than one row shares the attribute, so callers
/// that assume uniqueness fail loudly rather than silently picking an arbitrary
/// match.
pub fn at_most_one<T>(mut matches: Vec<T>, describe: impl FnOnce() -> String) -> Result<Option<T>> {
    if matches.len() > 1 {
        return Err(Error::Integrity(describe()));
    }
    Ok(matches.pop())
}

/// Implemented by every domain row type so generic helpers like
/// [`crate::dynamodb::Handler::get_records`] can index results by primary key.
pub trait HasID {
    fn id(&self) -> &str;
}

/// Where a table scan left off. Scanning the base table returns a
/// `LastEvaluatedKey` of just the primary key, and every scannable table is
/// hash-keyed on `id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanCursor {
    pub last_id: String,
}

/// One page of a table scan.
///
/// Rows are hydrated independently: an `Err` — always [`Error::Hydration`], naming
/// the offending row — is one bad record, not a failed page, so a scan can survey a
/// table end to end and report every problem it finds.
///
/// **`rows` being empty does not mean the scan is finished.** DynamoDB's `Limit`
/// counts items *examined*, and a page can come back empty while more remain. Only
/// `next == None` ends the walk.
#[derive(Debug)]
pub struct ScanPage<T> {
    pub rows: Vec<Result<T>>,
    pub next: Option<ScanCursor>,
}

/// A `user` row — the only durable identity Toolbox has. Membership (which
/// instances they belong to, in what role) is step 4's table; nothing here names
/// an instance.
#[derive(Clone, Debug, PartialEq)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: String,
    pub enabled: bool,
    pub created_at: u64,
    /// Absent until the user's first authenticated request. Touched at most once
    /// per minute (see `auth::fetch_update_user_auth_info`) to bound write volume.
    pub access_time: Option<u64>,
    /// Grants admin access to every instance's settings/membership/inbound
    /// addresses via the `Superuser`/`InstanceOwnerOrSuperuser` GraphQL guards —
    /// and, per the house rule in `CLAUDE.md`, **nothing else**: a superuser does
    /// not pass `Member`/`InstanceOwner` and has no implicit ticket access.
    /// Same omit-optional-attributes convention as `Instance::deleted`: only
    /// ever written as `true`; absent means `false`. Grantable only via
    /// `bin/cli.rs`'s `user set-superuser` — no GraphQL mutation constructs
    /// [`UserUpdateShape::SetSuperuser`].
    pub superuser: bool,
}

impl HasID for User {
    fn id(&self) -> &str {
        &self.id
    }
}

/// Update shapes for `user`, seslogin's convention (an enum of named shapes
/// rather than a struct of `Option<T>` fields) — each variant is exactly the
/// attributes one call site needs to touch, so a caller cannot accidentally
/// clobber a field it never meant to change.
#[derive(Clone, Debug, PartialEq)]
pub enum UserUpdateShape<'a> {
    Fields {
        name: &'a str,
        enabled: bool,
    },
    /// Throttled touch of `access_time` on a successful authenticated request;
    /// see [`crate::auth`].
    AccessTime,
    /// Grant (`true`) or revoke (`false`) superuser. `true` SETs the
    /// `superuser` attribute; `false` REMOVEs it — never written as
    /// `Bool(false)`, per the omit-optional-attributes house rule.
    /// **CLI-only**: `bin/cli.rs`'s `user set-superuser` is the only caller
    /// that constructs this — no GraphQL mutation may (see
    /// [`User::superuser`]'s doc comment).
    SetSuperuser(bool),
    /// Change a user's email. The taken-address pre-check (mirroring
    /// [`Handler::create_user`]'s caller-side check) is the caller's job —
    /// this variant does not re-check, the same division of labour as
    /// `create_instance`'s slug check.
    SetEmail {
        email: &'a str,
    },
}

/// A pending email login code. Hash key is `email` itself (see `SCHEMA.md`) — at
/// most one outstanding code per address, which is also what makes the 30s
/// resend rate limit a single-row read.
#[derive(Clone, Debug, PartialEq)]
pub struct LoginCode {
    pub email: String,
    pub code_hash: String,
    pub expires_at: u64,
    pub attempts: u64,
    pub last_sent_at: u64,
}

/// A `mtu_{id}.{secret}` session token. Only `token_hash` (sha256 of the full
/// token string) is ever stored — the token itself exists only at issuance, as
/// the string returned by `auth::issue_user_token`. Like [`ApiToken`], `id` is
/// embedded in the token, so [`crate::db::Handler::get_user_token`] is always a
/// strongly consistent `GetItem` by id, never a GSI lookup.
#[derive(Clone, Debug, PartialEq)]
pub struct UserToken {
    pub id: String,
    pub token_hash: String,
    pub user_id: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub last_used_at: Option<u64>,
}

impl HasID for UserToken {
    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserTokenUpdateShape {
    TouchLastUsed,
}

/// One authorized OAuth client for a user (the MCP interface). Holds both the
/// current access and refresh token hashes — a grant is the unit of revocation, so
/// rotating either token rewrites the same row rather than creating a new one.
/// `expires_at` is the absolute cap (also the DynamoDB TTL attribute):
/// `refresh_expires_at` slides forward on every rotation but never past it.
#[derive(Clone, Debug, PartialEq)]
pub struct OAuthGrant {
    pub id: String,
    pub user_id: String,
    pub client_id: String,
    pub client_name: String,
    pub redirect_uri: String,
    /// The audience (RFC 8707 `resource`) this grant's tokens are bound to, e.g.
    /// `<base>/mcp`. Checked on every use so a token minted for one audience can't
    /// authenticate against another.
    pub resource: String,
    pub scope: String,
    pub access_token_hash: String,
    pub access_expires_at: u64,
    pub refresh_token_hash: String,
    pub refresh_expires_at: u64,
    pub expires_at: u64,
    pub created_at: u64,
    pub last_used_at: Option<u64>,
}

impl HasID for OAuthGrant {
    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OAuthGrantUpdateShape {
    /// Refresh-token rotation: replace both secrets, sliding `refresh_expires_at`
    /// forward (capped by the grant's `expires_at` — enforced by the caller in
    /// `oauth.rs`, not here). Compare-and-swap on `expected_refresh_token_hash`, so
    /// of two concurrent refreshes with the same token only one wins; the loser gets
    /// [`Error::NotFound`], the same as a missing grant.
    Rotate {
        expected_refresh_token_hash: String,
        access_token_hash: String,
        access_expires_at: u64,
        refresh_token_hash: String,
        refresh_expires_at: u64,
    },
    TouchLastUsed,
}

/// An instance-scoped integration credential (`mta_{id}.{secret}`) — see
/// `auth::AuthInfo::ApiToken`'s doc comment for what it authorises (exactly
/// `submitVerifiedTicket`, for `instance_id`, and nothing else) and
/// `auth::issue_api_token`/`auth::verify_token`'s `mta_` branch for how it's
/// minted and checked. As with [`UserToken`], `id` is embedded in the token
/// string itself (the
/// house rule in `CLAUDE.md`'s "API tokens" entry: same id-in-token,
/// no-hash-GSI shape as the `+t{ticket_id}.{reply_token}` reply tag), so
/// [`crate::db::Handler::get_api_token`] is always a `GetItem` by id, never a
/// GSI lookup.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiToken {
    pub id: String,
    pub instance_id: String,
    pub name: String,
    /// sha256 of the *full* presented token string (`mta_{id}.{secret}`), not
    /// just the secret half — see `auth::verify_token`'s `mta_` branch. The
    /// secret itself is never stored; it exists in full only at issuance, as
    /// the string `auth::issue_api_token` returns.
    pub token_hash: String,
    /// Always written (never omitted) — unlike most bool flags in this
    /// project, a token's enabled state is a fact every row needs, not a
    /// sometimes-absent marker, so this does not follow the
    /// omit-optional-attributes convention `Instance::deleted`/
    /// `User::superuser` do.
    pub enabled: bool,
    pub created_at: u64,
    pub created_by_user_id: String,
    /// Absent until the token's first use, same throttled-touch convention
    /// as `UserToken::last_used_at`.
    pub last_used_at: Option<u64>,
}

impl HasID for ApiToken {
    fn id(&self) -> &str {
        &self.id
    }
}

/// Update shapes for `api_token` — `updateApiToken`'s write path
/// (`Fields`) and the throttled `last_used_at` touch on successful
/// verification (`TouchLastUsed`), mirroring [`UserTokenUpdateShape`].
#[derive(Clone, Debug, PartialEq)]
pub enum ApiTokenUpdateShape<'a> {
    Fields { name: &'a str, enabled: bool },
    TouchLastUsed,
}

/// A registered WebAuthn/passkey credential. `passkey_json` is the serialized
/// `webauthn_rs::prelude::Passkey` — opaque to everything except the
/// `webauthn-rs` crate, and the thing the serialized-fixture regression test in
/// `graphql::mutations` exists to protect.
#[derive(Clone, Debug, PartialEq)]
pub struct WebauthnCredential {
    /// Credential ID (base64url), also the DynamoDB hash key.
    pub id: String,
    pub user_id: String,
    /// User-supplied label, shown in the settings page.
    pub name: String,
    pub passkey_json: String,
    pub created_at: u64,
    pub last_used_at: Option<u64>,
}

impl HasID for WebauthnCredential {
    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebauthnCredentialUpdate {
    Rename(String),
    /// Always written on a successful login, even when the signature counter
    /// itself did not advance — see the doc comment at the `finish_passkey_login`
    /// call site for why gating this on `needs_update()` would leave
    /// `last_used_at` perpetually unset for most synced passkeys.
    TouchLastUsed {
        passkey_json: String,
    },
}

/// `instance.kind`: which of Toolbox's two separate functions an
/// instance is for. Set at creation (`create_instance`), **immutable after
/// creation** — `updateInstance`/`InstanceUpdateShape` has no way to change
/// it, and there is no CLI command that does either. Per the
/// omit-optional-attributes house rule, `kind` is written to the row only
/// for `Invoicing`; an absent attribute means `Support`, so every instance
/// created before invoicing existed needs no migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceKind {
    Support,
    Invoicing,
}

impl InstanceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Support => "support",
            Self::Invoicing => "invoicing",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "support" => Some(Self::Support),
            "invoicing" => Some(Self::Invoicing),
            _ => None,
        }
    }
}

/// An `instance` row — a tenant organisation. Owns zero or more
/// [`InboundAddress`]es and has zero or more [`Membership`]s.
#[derive(Clone, Debug, PartialEq)]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub slug: String,
    /// Immutable after creation — see [`InstanceKind`]'s doc comment.
    pub kind: InstanceKind,
    /// Opt-in, default `false`. Gates whether the instance appears in
    /// `publicInstances` and whether `requestSubmitCode` will issue a code for
    /// it. Per the omit-optional-attributes house rule, only ever written as
    /// `true`; `false` is the absence of the attribute. Support-only in
    /// practice (an invoicing instance is rejected by
    /// [`require_instance_kind`] everywhere this would matter), but the
    /// attribute itself is not kind-gated at the storage layer.
    pub public_submission_enabled: bool,
    pub from_name: String,
    pub signature: String,
    pub created_at: u64,
    /// Soft-delete marker. Same omit convention as
    /// `public_submission_enabled`: present (and `true`) means deleted, absent
    /// means active.
    pub deleted: bool,
    // ── Invoicing settings (invoicing instances only; see CLAUDE.md's
    // "Invoicing" house rule) — every field here is optional and follows the
    // omit-optional-attributes convention. A support instance never has any
    // of these set; nothing enforces that at the storage layer, but every
    // write path that could set them (`updateInvoicingSettings`) rejects a
    // support instance first via `require_instance_kind`.
    pub business_name: Option<String>,
    pub business_abn: Option<String>,
    pub business_address: Option<String>,
    pub business_phone: Option<String>,
    pub business_email: Option<String>,
    pub payment_details: Option<String>,
    /// Only ever written `true`; absent (`false`) is the default — an
    /// instance starts not registered for GST. See CLAUDE.md.
    pub gst_registered: bool,
    /// Absent means `"AUD"` — see [`Handler::update_instance`]'s
    /// `SetInvoicingSettings` doc comment and `validate_currency_code`.
    pub currency: Option<String>,
    /// Days after the issue date an invoice falls due, unless its project
    /// overrides it. Absent means [`DEFAULT_PAYMENT_TERMS_DAYS`] — see
    /// [`Instance::payment_terms_days_or_default`].
    pub payment_terms_days: Option<u32>,
}

impl Instance {
    /// This instance's currency code, defaulting to `"AUD"` when unset — the
    /// one place that default is decided, shared by the GraphQL resolver and
    /// anything else (the invoicing snapshot builder, in a later PR) that
    /// needs the effective code rather than the raw optional attribute.
    pub fn currency_or_default(&self) -> &str {
        self.currency.as_deref().unwrap_or("AUD")
    }

    /// This instance's default payment terms, in days — the one place the
    /// [`DEFAULT_PAYMENT_TERMS_DAYS`] fallback is applied.
    pub fn payment_terms_days_or_default(&self) -> u32 {
        self.payment_terms_days
            .unwrap_or(DEFAULT_PAYMENT_TERMS_DAYS)
    }
}

/// Payment terms an invoice gets when neither its project nor its instance
/// sets any.
pub const DEFAULT_PAYMENT_TERMS_DAYS: u32 = 14;

/// Longest payment terms either setting accepts.
pub const MAX_PAYMENT_TERMS_DAYS: u32 = 365;

impl HasID for Instance {
    fn id(&self) -> &str {
        &self.id
    }
}

/// Update shapes for `instance`. `Fields` never touches `deleted` (soft-delete
/// is its own variant, since it's a presence-marker attribute per the house
/// rule, not a plain field overwrite) or `kind` (immutable after creation —
/// see [`InstanceKind`]'s doc comment).
#[derive(Clone, Debug, PartialEq)]
pub enum InstanceUpdateShape<'a> {
    Fields {
        name: &'a str,
        from_name: &'a str,
        signature: &'a str,
        public_submission_enabled: bool,
    },
    /// Soft-delete (`true`) or restore (`false`) an instance. `true` sets the
    /// `deleted` attribute; `false` removes it — never written as `Bool(false)`,
    /// per the omit-optional-attributes house rule.
    SetDeleted(bool),
    /// Full-replace of every invoicing-settings attribute —
    /// `updateInvoicingSettings`'s write path. Each `Option<&str>` field is
    /// `Some` to `SET` (after the caller has trimmed and length-checked it)
    /// or `None` to `REMOVE` — the caller (`graphql::mutations::update_invoicing_settings`)
    /// turns an empty/blank input string into `None` before this is built, so
    /// this variant itself does no trimming. `gst_registered` is the one
    /// plain `bool`: `true` `SET`s the attribute, `false` `REMOVE`s it, per
    /// the omit-optional-attributes house rule (it is only ever written
    /// `true`). Never touches `kind`, `name`, or any other non-invoicing
    /// field.
    SetInvoicingSettings {
        business_name: Option<&'a str>,
        business_abn: Option<&'a str>,
        business_address: Option<&'a str>,
        business_phone: Option<&'a str>,
        business_email: Option<&'a str>,
        payment_details: Option<&'a str>,
        gst_registered: bool,
        currency: Option<&'a str>,
        /// `None` `REMOVE`s it (back to [`DEFAULT_PAYMENT_TERMS_DAYS`]).
        payment_terms_days: Option<u32>,
    },
}

/// Reject an instance whose `kind` isn't the one an operation requires — the
/// one place both directions of the kind-isolation house rule (CLAUDE.md) are
/// checked, so a support-only mutation and an invoicing-only one can't
/// independently drift on the wording or the check itself.
///
/// Callers that need "not found" semantics (a support-only operation reached
/// with an invoicing instance id, treated identically to a missing/deleted
/// instance — `addInboundAddress`, `createApiToken`, `submitTicket`/
/// `submitVerifiedTicket`, the requester submit-code flow) fold the kind
/// check into their own `.filter(...)`/`ok_or_else` chain (comparing
/// `instance.kind` directly) rather than calling this and mapping its error;
/// this function is for the other direction — an invoicing-only operation
/// (projects, `updateInvoicingSettings`) rejecting a support instance with a
/// plain validation-style error.
pub fn require_instance_kind(
    instance: &Instance,
    expected: InstanceKind,
) -> std::result::Result<(), String> {
    if instance.kind != expected {
        return Err(format!(
            "instance {} has kind {:?}, expected kind {:?}",
            instance.id,
            instance.kind.as_str(),
            expected.as_str()
        ));
    }
    Ok(())
}

/// Validate a currency code: exactly 3 uppercase ASCII letters (e.g. `AUD`,
/// `USD`) — enough to catch a typo without maintaining a real ISO 4217 list,
/// matching how little validation `db::validate_slug` does for the same
/// reason.
pub fn validate_currency_code(code: &str) -> std::result::Result<(), String> {
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(format!(
            "{code:?} is not a valid currency code (must be 3 uppercase letters, e.g. AUD)"
        ));
    }
    Ok(())
}

/// A `project` row — a client/job an invoicing instance bills against.
/// Invoicing-only: every write path that creates or updates one rejects a
/// support instance via [`require_instance_kind`].
#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub id: String,
    pub instance_id: String,
    pub name: String,
    pub client_name: String,
    pub client_abn: Option<String>,
    /// Multi-line (e.g. a street address across several lines).
    pub client_address: Option<String>,
    pub reference: Option<String>,
    /// Where `sendInvoice`/`sendCreditNote` mail by default.
    pub client_email: Option<String>,
    /// Overrides [`Instance::payment_terms_days`] for this project's invoices.
    pub payment_terms_days: Option<u32>,
    /// The GST-exclusive rate a new billable item on this project gets when
    /// it doesn't name one.
    pub default_unit_price_cents: Option<i64>,
    /// Only ever written `true`; absent (`false`) is the default — same omit
    /// convention as `Instance::deleted`/`Instance::gst_registered`. Archiving
    /// hides a project from the default project list without deleting its
    /// billing history; there is no delete in v1.
    pub archived: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Project {
    /// The payment terms this project's invoices get: its own override, else
    /// the instance's (which has its own default).
    pub fn payment_terms_days(&self, instance: &Instance) -> u32 {
        self.payment_terms_days
            .unwrap_or_else(|| instance.payment_terms_days_or_default())
    }
}

/// Every editable `project` attribute — `createProject`'s input and
/// `updateProject`'s full replace (which adds `archived`). Already trimmed
/// and validated; `None` means absent (create) or `REMOVE` (update).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ProjectFields {
    pub name: String,
    pub client_name: String,
    pub client_abn: Option<String>,
    pub client_address: Option<String>,
    pub client_email: Option<String>,
    pub reference: Option<String>,
    pub payment_terms_days: Option<u32>,
    pub default_unit_price_cents: Option<i64>,
}

impl HasID for Project {
    fn id(&self) -> &str {
        &self.id
    }
}

/// Update shapes for `project`. `Fields` is a full replace of every editable
/// attribute (`updateProject`'s write path, including `archived`) — there is
/// no delete in v1, so unlike `ticket`/`instance` there is no separate
/// soft-delete variant.
#[derive(Clone, Debug, PartialEq)]
pub enum ProjectUpdateShape<'a> {
    Fields {
        fields: &'a ProjectFields,
        archived: bool,
    },
}

/// A `billable_item` row — one line of work recorded against a [`Project`],
/// later collected onto an invoice. Money is integer throughout — see
/// `crate::invoicing::money`.
#[derive(Clone, Debug, PartialEq)]
pub struct BillableItem {
    pub id: String,
    /// Denormalised from the project so the instance-wide listing
    /// (`instance_id-date-index`) needs no join. Never changes: an item can't
    /// move between projects.
    pub instance_id: String,
    pub project_id: String,
    /// `YYYY-MM-DD`, always in exactly that form
    /// (`invoicing::validate_item_date`) — it is the listing GSIs' sort key,
    /// so the string order must be the date order.
    pub date: String,
    /// Trimmed, non-empty, ≤ 2000 chars, may be multi-line.
    pub description: String,
    /// Quantity × 100: `150` is 1.5. `0 < q ≤ 100_000_000`.
    pub quantity_hundredths: i64,
    /// GST-exclusive, `0 ≤ p ≤ 1_000_000_000`.
    pub unit_price_cents: i64,
    /// The invoice this item is on, if any — absent means unbilled. Nothing
    /// sets it yet: invoices (and the transactional attach/detach that writes
    /// this) arrive in the next PR of the invoicing stack. Until then every
    /// item is unbilled, and the update/delete paths already refuse an item
    /// that has one.
    pub invoice_id: Option<String>,
    /// No GST is charged on this line even when the instance is
    /// GST-registered. Only ever written `true`.
    pub gst_free: bool,
    /// The expense this item re-bills, when it was made by `rebillExpense`.
    /// The expense carries the matching `billable_item_id`; both are written
    /// (and, on delete, cleared) in one transaction.
    pub source_expense_id: Option<String>,
    pub created_by_user_id: String,
    pub created_at: u64,
    pub updated_at: u64,
}

impl HasID for BillableItem {
    fn id(&self) -> &str {
        &self.id
    }
}

/// Update shapes for `billable_item`. `Fields` is `updateBillableItem`'s
/// full replace of every editable attribute; `project_id`/`instance_id`/
/// `invoice_id` are never touched here.
#[derive(Clone, Debug, PartialEq)]
pub enum BillableItemUpdateShape<'a> {
    Fields {
        date: &'a str,
        description: &'a str,
        quantity_hundredths: i64,
        unit_price_cents: i64,
        gst_free: bool,
    },
}

/// A new billable item's fields — `create_billable_item`'s and
/// `rebill_expense`'s input.
#[derive(Clone, Debug, PartialEq)]
pub struct NewBillableItem<'a> {
    pub instance_id: &'a str,
    pub project_id: &'a str,
    pub date: &'a str,
    pub description: &'a str,
    pub quantity_hundredths: i64,
    pub unit_price_cents: i64,
    pub gst_free: bool,
    pub created_by_user_id: &'a str,
}

/// Which partition a billable-item listing reads: every item in an
/// instance (`instance_id-date-index`) or one project's
/// (`project_id-date-index`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BillableItemScope<'a> {
    Instance(&'a str),
    Project(&'a str),
}

/// The listing filter, applied as a DynamoDB `FilterExpression`:
/// `Unbilled` = `attribute_not_exists(invoice_id)`, `Billed` =
/// `attribute_exists(invoice_id)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BillableItemFilter {
    All,
    Unbilled,
    Billed,
}

/// Keyset cursor for a billable-item listing: `{date}:{id}`, shaped like
/// [`TicketCursor`]. Together with the listing's own scope (which supplies
/// the GSI hash key — `instance_id` or `project_id`), this is everything an
/// `ExclusiveStartKey` on either `*-date-index` needs: table key `id`, GSI
/// hash key, GSI sort key `date`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BillableItemCursor {
    pub date: String,
    pub id: String,
}

/// Forward-only (`first`/`after`), newest `date` first. `limit` is how many
/// matching rows to return at most — the resolver asks for one more than a
/// page to learn whether there's a next page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListBillableItemsPage {
    pub after: Option<BillableItemCursor>,
    pub limit: i32,
}

/// `expense.category`: one of a fixed list, built into the code rather than
/// configured per instance so totals stay comparable — see CLAUDE.md's
/// "Expenses" house rule. Stored as [`Self::as_str`]. [`Self::VehicleKm`]
/// is the one category with a different shape: a cents-per-km trip
/// ([`ExpenseDetail::VehicleKm`]) rather than a purchase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExpenseCategory {
    Materials,
    Subcontractors,
    ToolsEquipment,
    VehicleFuel,
    VehicleKm,
    Travel,
    MealsEntertainment,
    SoftwareSubscriptions,
    PhoneInternet,
    OfficeSupplies,
    ProfessionalFees,
    Insurance,
    RentUtilities,
    AdvertisingMarketing,
    BankFees,
    Training,
    LicencesMemberships,
    PostageFreight,
    Other,
}

impl ExpenseCategory {
    pub const ALL: [Self; 19] = [
        Self::Materials,
        Self::Subcontractors,
        Self::ToolsEquipment,
        Self::VehicleFuel,
        Self::VehicleKm,
        Self::Travel,
        Self::MealsEntertainment,
        Self::SoftwareSubscriptions,
        Self::PhoneInternet,
        Self::OfficeSupplies,
        Self::ProfessionalFees,
        Self::Insurance,
        Self::RentUtilities,
        Self::AdvertisingMarketing,
        Self::BankFees,
        Self::Training,
        Self::LicencesMemberships,
        Self::PostageFreight,
        Self::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Materials => "materials",
            Self::Subcontractors => "subcontractors",
            Self::ToolsEquipment => "tools_equipment",
            Self::VehicleFuel => "vehicle_fuel",
            Self::VehicleKm => "vehicle_km",
            Self::Travel => "travel",
            Self::MealsEntertainment => "meals_entertainment",
            Self::SoftwareSubscriptions => "software_subscriptions",
            Self::PhoneInternet => "phone_internet",
            Self::OfficeSupplies => "office_supplies",
            Self::ProfessionalFees => "professional_fees",
            Self::Insurance => "insurance",
            Self::RentUtilities => "rent_utilities",
            Self::AdvertisingMarketing => "advertising_marketing",
            Self::BankFees => "bank_fees",
            Self::Training => "training",
            Self::LicencesMemberships => "licences_memberships",
            Self::PostageFreight => "postage_freight",
            Self::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// What the CSV export and a re-billed line call it — the same labels
    /// as the web app's `lib/expenses.ts`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Materials => "Materials & supplies",
            Self::Subcontractors => "Subcontractors",
            Self::ToolsEquipment => "Tools & equipment",
            Self::VehicleFuel => "Vehicle & fuel",
            Self::VehicleKm => "Vehicle trip (cents per km)",
            Self::Travel => "Travel",
            Self::MealsEntertainment => "Meals & entertainment",
            Self::SoftwareSubscriptions => "Software & subscriptions",
            Self::PhoneInternet => "Phone & internet",
            Self::OfficeSupplies => "Office supplies",
            Self::ProfessionalFees => "Professional fees",
            Self::Insurance => "Insurance",
            Self::RentUtilities => "Rent & utilities",
            Self::AdvertisingMarketing => "Advertising & marketing",
            Self::BankFees => "Bank & merchant fees",
            Self::Training => "Training & education",
            Self::LicencesMemberships => "Licences & memberships",
            Self::PostageFreight => "Postage & freight",
            Self::Other => "Other",
        }
    }
}

/// The shape-specific half of an expense. Which variant a row has is
/// decided by its category — [`ExpenseCategory::VehicleKm`] is always a
/// `VehicleKm`, every other category a `Purchase` — enforced by
/// `invoicing::expense::validate_expense_input`, the one constructor every
/// write path goes through.
#[derive(Clone, Debug, PartialEq)]
pub enum ExpenseDetail {
    /// Something bought, from a receipt.
    Purchase {
        supplier: String,
        /// GST-inclusive — what was actually paid.
        amount_cents: i64,
        /// The GST included in `amount_cents`; absent for a GST-free purchase.
        gst_cents: Option<i64>,
    },
    /// A business trip claimed by the ATO's cents-per-km method. Carries no
    /// GST. The amount is derived, never stored —
    /// [`crate::invoicing::vehicle::trip_amount_cents`].
    VehicleKm {
        distance_tenths_km: i64,
        /// Looked up from the ATO table by the trip's date at write time
        /// and stored, so a later table change never alters a saved trip.
        rate_cents_per_km: i64,
    },
}

/// Every editable attribute of an expense — `createExpense`'s input and
/// `updateExpense`'s full replace.
#[derive(Clone, Debug, PartialEq)]
pub struct ExpenseFields {
    /// Optional — an expense need not belong to a project. Absent from the
    /// row (never `Null`) when unset, which is what keeps it out of the
    /// sparse `project_id-date-index`.
    pub project_id: Option<String>,
    /// `YYYY-MM-DD`, canonical — the listing GSIs' sort key, like
    /// `BillableItem::date`.
    pub date: String,
    pub category: ExpenseCategory,
    /// Optional for a purchase; required (the trip's business purpose) for
    /// a vehicle trip.
    pub description: Option<String>,
    pub detail: ExpenseDetail,
}

/// An `expense` row — money an invoicing instance spent, optionally against
/// one of its [`Project`]s. Never linked to an invoice.
#[derive(Clone, Debug, PartialEq)]
pub struct Expense {
    pub id: String,
    pub instance_id: String,
    pub fields: ExpenseFields,
    /// The billable item `rebillExpense` made from this expense, if any —
    /// set and cleared only together with that item's `source_expense_id`.
    /// An expense that has been re-billed can't be deleted.
    pub billable_item_id: Option<String>,
    /// The uploaded receipt, if any.
    pub receipt: Option<ExpenseReceipt>,
    /// Whoever logged it — for a vehicle trip, also whose 5,000 km running
    /// total it counts toward.
    pub created_by_user_id: String,
    pub created_at: u64,
    pub updated_at: u64,
}

/// A receipt file attached to an expense — stored under
/// `receipts/{instance_id}/{expense_id}/…` in the mail bucket.
#[derive(Clone, Debug, PartialEq)]
pub struct ExpenseReceipt {
    pub s3_key: String,
    pub filename: String,
    pub content_type: String,
    pub size: u64,
}

impl Expense {
    /// The GST included in what was paid: a purchase's `gst_cents`, or zero
    /// for a GST-free purchase and for every vehicle trip.
    pub fn gst_cents(&self) -> i64 {
        match &self.fields.detail {
            ExpenseDetail::Purchase { gst_cents, .. } => gst_cents.unwrap_or(0),
            ExpenseDetail::VehicleKm { .. } => 0,
        }
    }

    /// What the expense cost before GST.
    pub fn amount_ex_gst_cents(&self) -> i64 {
        self.amount_cents() - self.gst_cents()
    }

    /// What the expense cost, in cents: the GST-inclusive amount paid for a
    /// purchase, or distance × rate for a trip.
    pub fn amount_cents(&self) -> i64 {
        match &self.fields.detail {
            ExpenseDetail::Purchase { amount_cents, .. } => *amount_cents,
            ExpenseDetail::VehicleKm {
                distance_tenths_km,
                rate_cents_per_km,
            } => crate::invoicing::vehicle::trip_amount_cents(
                *distance_tenths_km,
                *rate_cents_per_km,
            ),
        }
    }
}

impl HasID for Expense {
    fn id(&self) -> &str {
        &self.id
    }
}

/// Which partition an expense listing reads — mirrors
/// [`BillableItemScope`]. `Project` reads the sparse
/// `project_id-date-index`, so an expense with no project never appears
/// in one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpenseScope<'a> {
    Instance(&'a str),
    Project(&'a str),
}

/// Keyset cursor for an expense listing: `{date}:{id}` — the same shape,
/// for the same reason, as [`BillableItemCursor`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpenseCursor {
    pub date: String,
    pub id: String,
}

/// Forward-only (`first`/`after`), newest `date` first — mirrors
/// [`ListBillableItemsPage`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListExpensesPage {
    pub after: Option<ExpenseCursor>,
    pub limit: i32,
}

/// `invoice.status`. **Strictly one-way**: `Finalized` never reverts to
/// `Draft` — no void, no un-finalize, per CLAUDE.md's "Invoicing" house
/// rule. The only thing that changes on a finalized invoice afterward is
/// `paid_date`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvoiceStatus {
    Draft,
    Finalized,
}

impl InvoiceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Finalized => "finalized",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "draft" => Some(Self::Draft),
            "finalized" => Some(Self::Finalized),
            _ => None,
        }
    }
}

/// An `invoice` row — a project's billable items collected for billing. See
/// CLAUDE.md's "Invoicing" house rule for the domain rules this shape
/// exists to support: `item_ids`/`version` back the transactional attach/
/// detach/finalize machinery in [`Handler`]'s `invoice` methods; everything
/// from `number` onward is `None` until [`Handler::finalize_invoice`] sets
/// it, once, forever.
#[derive(Clone, Debug, PartialEq)]
pub struct Invoice {
    pub id: String,
    pub instance_id: String,
    pub project_id: String,
    pub status: InvoiceStatus,
    /// Optimistic-concurrency counter. Bumped by every write that touches
    /// `item_ids` (`create_invoice`/`add_invoice_items`/
    /// `remove_invoice_items`) and by [`Handler::update_billable_item`] when
    /// editing an item that sits on this (draft) invoice — so a concurrent
    /// `finalize_invoice`, which conditions on the version it read, fails
    /// cleanly rather than freezing a snapshot that's already stale.
    pub version: u64,
    /// The authoritative set of items on this invoice. Absent (empty)
    /// attribute is a String Set's only way to be "empty" — see the
    /// omit-optional-attributes house rule.
    pub item_ids: Vec<String>,
    pub created_by_user_id: String,
    pub created_at: u64,
    pub updated_at: u64,
    /// Assigned once, at finalization, from the per-instance
    /// `next_invoice_number` counter. Absent for a draft. Displayed
    /// zero-padded to 3 digits (`db::Invoice::display_number`); it simply
    /// grows past 999.
    pub number: Option<u32>,
    /// `YYYY-MM-DD`. Absent for a draft; set once, at finalization, never
    /// changed afterward.
    pub issue_date: Option<String>,
    /// Frozen JSON (`invoicing::snapshot::InvoiceSnapshot`, `schema_version:
    /// 1`) of everything the invoice prints — set once, at finalization.
    /// Absent for a draft, whose printable content is instead built live
    /// (`invoicing::snapshot::build_snapshot`) from the project/instance/
    /// items as they stand right now.
    pub snapshot: Option<String>,
    /// Denormalised from the snapshot so listing/sorting never needs to
    /// parse JSON. Absent for a draft.
    pub total_cents: Option<i64>,
    pub finalized_at: Option<u64>,
    pub finalized_by_user_id: Option<String>,
    /// Absent means unpaid. Any member may set or clear this on a finalized
    /// invoice; it is never printed on the invoice itself.
    pub paid_date: Option<String>,
    /// The S3 key of this invoice's rendered PDF, once
    /// `downloadInvoicePdf` has rendered it for the first time. Absent for
    /// a draft, and absent for a finalized invoice that has never been
    /// downloaded. Set exactly once, by [`Handler::set_invoice_pdf_key`] —
    /// rendering is deterministic from the frozen `snapshot`, so a
    /// concurrent double-render just overwrites this with the same key and
    /// identical bytes, never a real race.
    pub pdf_s3_key: Option<String>,
    /// `YYYY-MM-DD`. Absent for a draft; set once, at finalization (from
    /// `finalizeInvoice`'s `dueDate`, else issue date + the project's or
    /// instance's payment terms), and frozen into the snapshot too.
    pub due_date: Option<String>,
    /// GST in `total_cents`, denormalised from the snapshot like
    /// `total_cents` so reports never parse JSON. Absent for a draft.
    pub gst_cents: Option<i64>,
    /// Payments recorded against this (finalized) invoice, oldest first.
    /// Stored as one JSON string attribute (`payments`), absent when empty.
    pub payments: Vec<InvoicePayment>,
    /// Sum of every credit note issued against this invoice (GST-inclusive),
    /// and the GST part of it. Absent (zero) until the first credit note;
    /// written only by [`Handler::create_credit_note`]'s transaction.
    pub credited_cents: i64,
    pub credited_gst_cents: i64,
    /// When `sendInvoice` last mailed it, and to whom.
    pub sent_at: Option<u64>,
    pub sent_to: Vec<String>,
}

impl HasID for Invoice {
    fn id(&self) -> &str {
        &self.id
    }
}

/// The `counter` row id that reserves invoice `number` in an instance:
/// `{instance_id}#invoice#{number}`. Written once, by the transaction that
/// finalizes the invoice taking that number, and never removed (finalized
/// invoices are permanent). Shares the table with the per-instance counter
/// rows (whose id is the bare instance id) because both are number
/// allocation; an instance id never contains `#`, so the two can't collide.
pub fn invoice_number_reservation_id(instance_id: &str, number: u32) -> String {
    format!("{instance_id}#invoice#{number}")
}

impl Invoice {
    /// Every payment recorded against this invoice, summed.
    pub fn paid_cents(&self) -> i64 {
        self.payments.iter().map(|p| p.amount_cents).sum()
    }

    /// What the client still owes: total − credited − paid. Negative when
    /// more has been paid and credited than was invoiced (a refund is due).
    /// Zero for a draft.
    pub fn balance_cents(&self) -> i64 {
        match self.total_cents {
            Some(total) => total - self.credited_cents - self.paid_cents(),
            None => 0,
        }
    }

    /// Unpaid (no `paid_date`), with a due date before `today`
    /// (`YYYY-MM-DD`). A draft is never overdue.
    pub fn is_overdue(&self, today: &str) -> bool {
        self.status == InvoiceStatus::Finalized
            && self.paid_date.is_none()
            && self.due_date.as_deref().is_some_and(|d| d < today)
    }
}

/// One payment received against an invoice. `id` is a nanoid, unique within
/// the invoice — what `deleteInvoicePayment` names.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InvoicePayment {
    pub id: String,
    /// `YYYY-MM-DD`.
    pub date: String,
    /// GST-inclusive cents received, > 0.
    pub amount_cents: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub recorded_by_user_id: String,
    pub recorded_at: u64,
}

/// A `credit_note` row — an adjustment note against one finalized invoice.
/// Like an invoice it is numbered (its own `next_credit_note_number`
/// sequence) and its printable content is frozen in `snapshot` at the moment
/// it is issued; unlike an invoice it has no draft stage and can never be
/// changed or deleted. See CLAUDE.md's "Credit notes" house rule.
#[derive(Clone, Debug, PartialEq)]
pub struct CreditNote {
    pub id: String,
    pub instance_id: String,
    pub invoice_id: String,
    /// Denormalised from the invoice so a project's figures need no join.
    pub project_id: String,
    pub number: u32,
    /// `YYYY-MM-DD`.
    pub issue_date: String,
    pub reason: String,
    /// Frozen `invoicing::snapshot::InvoiceSnapshot` JSON, with its
    /// `credit_note` block set.
    pub snapshot: String,
    pub subtotal_cents: i64,
    pub gst_cents: i64,
    pub total_cents: i64,
    pub created_by_user_id: String,
    pub created_at: u64,
    pub pdf_s3_key: Option<String>,
    pub sent_at: Option<u64>,
    pub sent_to: Vec<String>,
}

impl HasID for CreditNote {
    fn id(&self) -> &str {
        &self.id
    }
}

impl CreditNote {
    /// `CN-001` — zero-padded like an invoice number, prefixed so the two
    /// sequences can't be confused on paper.
    pub fn display_number(&self) -> String {
        credit_note_display_number(self.number)
    }
}

pub fn credit_note_display_number(number: u32) -> String {
    format!("CN-{number:03}")
}

/// What `finalize_invoice` freezes onto the row.
#[derive(Clone, Debug, PartialEq)]
pub struct FinalizeInvoice<'a> {
    pub number: u32,
    pub issue_date: &'a str,
    pub due_date: &'a str,
    pub snapshot_json: &'a str,
    pub total_cents: i64,
    pub gst_cents: i64,
    pub finalized_by_user_id: &'a str,
}

/// Which partition a credit-note listing reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreditNoteScope<'a> {
    Instance(&'a str),
    Invoice(&'a str),
}

/// Keyset cursor for a credit-note listing — same shape as [`InvoiceCursor`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreditNoteCursor {
    pub created_at: u64,
    pub id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListCreditNotesPage {
    pub after: Option<CreditNoteCursor>,
    pub limit: i32,
}

/// Which partition an invoice listing reads — mirrors
/// [`BillableItemScope`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvoiceScope<'a> {
    Instance(&'a str),
    Project(&'a str),
}

/// `invoices`' filter. `Unpaid`/`Paid`/`Overdue` all imply `Finalized` — a
/// draft has no paid status. `Overdue` is unpaid with a `due_date` before
/// `today` (`YYYY-MM-DD`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvoiceListFilter {
    All,
    Draft,
    Unpaid,
    Paid,
    Overdue { today: String },
}

/// Keyset cursor for an invoice listing: `{created_at}:{id}`, mirroring
/// [`BillableItemCursor`]/[`TicketCursor`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvoiceCursor {
    pub created_at: u64,
    pub id: String,
}

/// Forward-only (`first`/`after`), newest `created_at` first — mirrors
/// [`ListBillableItemsPage`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListInvoicesPage {
    pub after: Option<InvoiceCursor>,
    pub limit: i32,
}

/// `inbound_address.kind`: whether a row matches one exact address or every
/// address at a domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressKind {
    Exact,
    Wildcard,
}

impl AddressKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Wildcard => "wildcard",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "exact" => Some(Self::Exact),
            "wildcard" => Some(Self::Wildcard),
            _ => None,
        }
    }
}

/// An `inbound_address` row. Hash key is `address` itself (lowercased, or a
/// `*@domain` wildcard) — see `SCHEMA.md` for why: inbound-mail routing
/// (`crate::inbound::routing`) does a direct `GetItem` on the normalized
/// recipient, so making the address the key turns routing into a `GetItem`
/// instead of a `Query`.
#[derive(Clone, Debug, PartialEq)]
pub struct InboundAddress {
    pub address: String,
    pub instance_id: String,
    pub kind: AddressKind,
    pub created_at: u64,
}

/// `membership.role`: what a user may do in an instance. `Owner` implies every
/// `Agent` permission plus instance settings (inbound addresses, membership).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MembershipRole {
    Owner,
    Agent,
}

impl MembershipRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Agent => "agent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Self::Owner),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }
}

/// A `membership` row: one user's role in one instance.
#[derive(Clone, Debug, PartialEq)]
pub struct Membership {
    pub id: String,
    pub user_id: String,
    pub instance_id: String,
    pub role: MembershipRole,
    /// This member's staff-notification preferences for this instance — see
    /// [`NotificationSettings`]'s doc comment.
    pub notification_settings: NotificationSettings,
}

impl HasID for Membership {
    fn id(&self) -> &str {
        &self.id
    }
}

/// One member's staff-email-notification preferences for one instance —
/// whether `crate::staff_notify` mails them about a given kind of ticket
/// activity. Each field is stored as an *optional* Bool on the `membership`
/// row (see [`NotificationSettingsPatch`]) — per the omit-optional-attributes
/// house rule, an absent attribute means "use the default", not `false`, so
/// this type's [`Default`] impl is the single source of truth for what those
/// defaults are; `crate::dynamodb`'s hydration and this impl must never
/// disagree. Removing and re-adding a membership drops the row (and with it
/// every attribute here), so a re-added member always starts back at these
/// defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotificationSettings {
    /// A new ticket is opened in this instance (inbound email or the public
    /// submission form).
    pub new_ticket: bool,
    /// Someone else assigns a ticket to this member.
    pub assigned_to_me: bool,
    /// A ticket assigned to this member receives an "update" (a customer
    /// message, an agent reply, an internal note, or a close/reopen) —
    /// including being unassigned from/reassigned away from this member,
    /// which reuses this same setting (see `crate::staff_notify`'s doc
    /// comment on why assignment changes are covered by *this* setting, not
    /// `assigned_to_me`, for the outgoing member's side of the change).
    pub assigned_to_me_updated: bool,
    /// A ticket with no assignee receives an update.
    pub unassigned_updated: bool,
    /// A ticket assigned to someone else receives an update. Off by default —
    /// unlike the other four, opting into every other agent's ticket traffic
    /// is noisy enough that it should be a deliberate choice.
    pub assigned_to_others_updated: bool,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            new_ticket: true,
            assigned_to_me: true,
            assigned_to_me_updated: true,
            unassigned_updated: true,
            assigned_to_others_updated: false,
        }
    }
}

/// A patch to a subset of [`NotificationSettings`]' fields —
/// `updateNotificationSettings`'s argument shape and
/// [`Handler::update_membership_notification_settings`]'s input. `None`
/// means "leave this field alone"; `Some(v)` always writes an explicit
/// `true`/`false` (never a `REMOVE` back to the default — there is no GraphQL
/// path back to "unset", only removing the membership itself resets to
/// defaults, per [`NotificationSettings`]'s doc comment).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NotificationSettingsPatch {
    pub new_ticket: Option<bool>,
    pub assigned_to_me: Option<bool>,
    pub assigned_to_me_updated: Option<bool>,
    pub unassigned_updated: Option<bool>,
    pub assigned_to_others_updated: Option<bool>,
}

impl NotificationSettingsPatch {
    /// True when every field is `None` — the caller sent an update with
    /// nothing to change. [`crate::dynamodb::Handler::update_membership_notification_settings`]
    /// treats this as a no-op rather than sending DynamoDB an `UpdateItem`
    /// with an empty `SET` clause.
    pub fn is_empty(&self) -> bool {
        self.new_ticket.is_none()
            && self.assigned_to_me.is_none()
            && self.assigned_to_me_updated.is_none()
            && self.unassigned_updated.is_none()
            && self.assigned_to_others_updated.is_none()
    }

    /// Merge this patch onto `current`, returning the resulting settings —
    /// used to build the response `MembershipInfo` a mutation returns
    /// without a second read of the row it just wrote.
    pub fn merged_onto(&self, current: NotificationSettings) -> NotificationSettings {
        NotificationSettings {
            new_ticket: self.new_ticket.unwrap_or(current.new_ticket),
            assigned_to_me: self.assigned_to_me.unwrap_or(current.assigned_to_me),
            assigned_to_me_updated: self
                .assigned_to_me_updated
                .unwrap_or(current.assigned_to_me_updated),
            unassigned_updated: self
                .unassigned_updated
                .unwrap_or(current.unassigned_updated),
            assigned_to_others_updated: self
                .assigned_to_others_updated
                .unwrap_or(current.assigned_to_others_updated),
        }
    }
}

/// A row in the generic, TTL'd `ephemeral_state` table: a `kind`-namespaced
/// key/value capability store. Backs WebAuthn challenge state (`kind` "reg" /
/// "auth") in this step, and will back the requester submit-token flow's
/// capability token (`kind` "submit_token") in step 4 — the whole reason this is
/// written generically rather than as a WebAuthn-specific table. `payload` is
/// opaque JSON whose shape depends on `kind`.
#[derive(Clone, Debug, PartialEq)]
pub struct EphemeralState {
    pub id: String,
    pub kind: String,
    pub payload: String,
    pub expires_at: u64,
}

/// A WebAuthn registration/login challenge, as a typed view over an
/// `ephemeral_state` row (`kind` "reg" or "auth"). Not a separate table — see
/// [`Handler::put_webauthn_state`]'s doc comment for how `user_id` and
/// `state_json` fold into that row's opaque `payload`.
#[derive(Clone, Debug, PartialEq)]
pub struct WebauthnState {
    pub id: String,
    pub kind: String,
    /// Set for registration challenges (so `finishPasskeyRegistration` can check
    /// the challenge belongs to the caller); absent for login challenges, which
    /// are usernameless by design.
    pub user_id: Option<String>,
    /// JSON-serialized `PasskeyRegistration` or `DiscoverableAuthentication`.
    pub state_json: String,
    pub expires_at: u64,
}

/// `ticket.status` — the un-composited form of `instance_status`'s suffix. Kept
/// alongside the composite marker attributes because resolvers read `status`
/// directly far more often than they need the composite (see `SCHEMA.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TicketStatus {
    Open,
    Closed,
    Deleted,
}

impl TicketStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Deleted => "deleted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "closed" => Some(Self::Closed),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

/// The desired state of `ticket`'s three sparse composite marker attributes —
/// `instance_status`, `instance_visible`, `instance_assignee` — for a given
/// `(instance_id, status, assignee_user_id)`. `Some(value)` means the write
/// path must `SET` the attribute to `value`; `None` means it must `REMOVE` it
/// (never write `AttributeValue::Null` — see `CLAUDE.md`'s house rule and
/// `SCHEMA.md`'s "Known issues" callout on why a GSI hash key attribute must
/// be *absent*, not null, for a row to drop out of that index).
///
/// This is the single place that decides what the three markers *should* be.
/// [`crate::dynamodb::Handler::create_ticket`] and its `update_ticket`
/// (`TicketUpdateShape::SetStatusAndAssignee`) both build their `UpdateItem`/
/// `PutItem` calls from this function's output rather than each computing
/// their own — the bug this project is most worried about (per the build
/// plan) is exactly two write paths disagreeing about when a marker should be
/// present, and having only one function decide closes that off structurally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TicketMarkers {
    /// Always present: `"{instance_id}#{status}"`.
    pub instance_status: String,
    /// `Some("{instance_id}")` unless `status == Deleted`.
    pub instance_visible: Option<String>,
    /// `Some("{instance_id}#{assignee_user_id}")` only when assigned *and*
    /// `status != Deleted` — a deleted ticket carries no assignee marker
    /// regardless of whether `assignee_user_id` was passed, since "assigned
    /// to me" must never surface a deleted ticket.
    pub instance_assignee: Option<String>,
}

pub fn compute_ticket_markers(
    instance_id: &str,
    status: TicketStatus,
    assignee_user_id: Option<&str>,
) -> TicketMarkers {
    let instance_status = format!("{instance_id}#{}", status.as_str());
    if status == TicketStatus::Deleted {
        return TicketMarkers {
            instance_status,
            instance_visible: None,
            instance_assignee: None,
        };
    }
    TicketMarkers {
        instance_status,
        instance_visible: Some(instance_id.to_string()),
        instance_assignee: assignee_user_id.map(|uid| format!("{instance_id}#{uid}")),
    }
}

/// The `[#{slug}-{number}]` subject tag inbound-mail threading (step 7) looks
/// for, and outbound mail (step 6) stamps onto every `Subject:` header. Pure
/// and unit-tested here since both later steps depend on this exact format
/// staying stable.
pub fn ticket_subject_tag(slug: &str, number: u64) -> String {
    format!("[#{slug}-{number}]")
}

/// A `ticket` row. See `SCHEMA.md` for the full attribute-by-attribute
/// rationale, especially the three composite marker attributes
/// (`instance_status`/`instance_visible`/`instance_assignee`), which are
/// *not* fields of this struct — they exist purely as sparse GSI keys and are
/// recomputed from `status`/`assignee_user_id` by [`compute_ticket_markers`]
/// whenever a write needs them, never stored/read as ordinary data here.
#[derive(Clone, Debug, PartialEq)]
pub struct Ticket {
    pub id: String,
    pub instance_id: String,
    /// Per-instance sequential number, allocated once via
    /// [`Handler::increment_ticket_counter`]'s atomic `ADD` — never
    /// read-then-written. See `SCHEMA.md`'s "Known issues" register.
    pub number: u64,
    pub subject: String,
    pub status: TicketStatus,
    pub requester_emails: Vec<String>,
    pub cc_emails: Vec<String>,
    pub assignee_user_id: Option<String>,
    /// Opaque 16-char token embedded in outbound `Reply-To` as `+t{reply_token}`
    /// (step 6) and read back out of an inbound `+tag` recipient (step 7) for
    /// threading. Minted once at creation, never rotated.
    pub reply_token: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub last_activity_at: u64,
    /// Denormalised: true once any message on this ticket carries an
    /// attachment. Exists so a list row can show a paperclip without reading
    /// every message of every ticket on the page — which is one extra query
    /// per row, on the screen agents look at most. Never cleared: an
    /// attachment that existed is a fact about the thread's history.
    pub has_attachments: bool,
}

impl HasID for Ticket {
    fn id(&self) -> &str {
        &self.id
    }
}

/// Update shapes for `ticket`. **`SetStatusAndAssignee` is the only variant
/// allowed to touch any of the three composite marker attributes**, and it
/// always recomputes all three together (via [`compute_ticket_markers`]) in a
/// single `UpdateItem` — every call site that changes status (open/close/
/// delete/restore) or assignee (assign/unassign) goes through this one
/// variant, passing through whichever of the two it isn't changing. This is
/// deliberate: a write path that updated `instance_status` in one call and
/// `instance_assignee` in a separate one would reintroduce the exact race
/// `SCHEMA.md`'s "Known issues" register warns about (a crash between the two
/// calls leaving the markers inconsistent) — collapsing every marker-affecting
/// write into this single variant makes that structurally impossible.
///
/// The remaining variants never touch the markers, only ordinary fields, and
/// still bump `updated_at`/`last_activity_at` — any write to a ticket counts
/// as activity for the "newest activity first" ordering the listing GSIs use.
#[derive(Clone, Debug, PartialEq)]
pub enum TicketUpdateShape<'a> {
    SetStatusAndAssignee {
        instance_id: &'a str,
        status: TicketStatus,
        /// The *desired* assignee after this write — pass the ticket's
        /// current assignee unchanged for a pure status transition, or the
        /// current status unchanged for a pure assignment change.
        assignee_user_id: Option<&'a str>,
        now: u64,
    },
    AddRequester {
        email: &'a str,
        now: u64,
    },
    RemoveRequester {
        email: &'a str,
        now: u64,
    },
    AddCc {
        email: &'a str,
        now: u64,
    },
    RemoveCc {
        email: &'a str,
        now: u64,
    },
    /// Bump `updated_at`/`last_activity_at` only — a new message (reply/note)
    /// landed on the ticket with no status or assignee change.
    Touch {
        now: u64,
    },
    /// Record that this ticket has at least one attachment somewhere in its
    /// thread. Idempotent, and one-way: see [`Ticket::has_attachments`].
    MarkHasAttachments,
}

/// Keyset pagination cursor for a ticket listing: `{last_activity_at}:{id}`,
/// mirroring seslogin's `PeriodCursor`. `last_activity_at` first (the sort
/// key every listing GSI shares) so ties on it still resolve deterministically
/// via `id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TicketCursor {
    pub last_activity_at: u64,
    pub id: String,
}

/// Which of the three listing GSIs a `list_tickets` call queries, and any
/// extra non-key filter it needs. Maps directly to the four `TicketStatusFilter`
/// values the GraphQL layer exposes:
///
/// - `OPEN`/`CLOSED`/`DELETED` (owner-only, enforced in the resolver, not
///   here) → `Status(_)`, the `instance_status-last_activity_at-index`.
/// - `ALL` → `Visible`, the `instance_visible-last_activity_at-index` (every
///   non-deleted ticket).
/// - `assignedTo` set (regardless of `status`) → `AssignedTo`, the
///   `instance_assignee-last_activity_at-index`, which is itself sparse to
///   assigned + non-deleted tickets; an additional `status` narrows further
///   via a `FilterExpression` (not a key condition — DynamoDB allows only one
///   index per query).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TicketListFilter {
    Status(TicketStatus),
    Visible,
    AssignedTo {
        user_id: String,
        status: Option<TicketStatus>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListTicketsPage {
    pub after: Option<TicketCursor>,
    pub before: Option<TicketCursor>,
    pub limit: i32,
    /// `true` for the default "newest activity first" order.
    pub descending: bool,
}

/// `ticket_message.kind`. `Note` rows are internal-only: never emailed, and
/// filtered out of any requester-visible resolver (enforced in
/// `graphql::query::Ticket::messages`, not only in the web UI).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TicketMessageKind {
    Inbound,
    Reply,
    Note,
    System,
}

impl TicketMessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Reply => "reply",
            Self::Note => "note",
            Self::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "inbound" => Some(Self::Inbound),
            "reply" => Some(Self::Reply),
            "note" => Some(Self::Note),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// One `ticket_message.attachments` entry — populated by inbound-mail
/// storage and by `replyToTicket`'s `attachmentKeys` (both step 7), via
/// `TicketMessageUpdateShape::SetAttachments`. See `SCHEMA.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub s3_key: String,
    pub filename: String,
    pub content_type: String,
    pub size: u64,
}

/// A `ticket_message` row. See `SCHEMA.md` for the full rationale, especially
/// why `author_user_id`/`from_email` and `body_text`/`body_html` are each
/// mutually-optional-but-usually-one-present pairs rather than a single
/// tagged field.
#[derive(Clone, Debug, PartialEq)]
pub struct TicketMessage {
    pub id: String,
    pub ticket_id: String,
    pub kind: TicketMessageKind,
    /// Present for `reply`/`note`; absent for `inbound`/`system`.
    pub author_user_id: Option<String>,
    /// Present for `inbound`; absent otherwise.
    pub from_email: Option<String>,
    pub to_emails: Vec<String>,
    pub cc_emails: Vec<String>,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    /// SES's rewritten `Message-ID` once this message is actually sent (step
    /// 6) or the id an inbound message arrived with (step 7). Absent until
    /// [`Handler::update_ticket_message`]'s `SetRfcMessageId` lands it — the
    /// row is always created first (see `graphql::mutations::reply_to_ticket`),
    /// before the send even happens, so this is genuinely absent, not just
    /// unset-by-convention, for a reply that hasn't gone out yet or whose
    /// send failed.
    pub rfc_message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    /// See [`Attachment`]'s doc comment — always empty as of this step.
    pub attachments: Vec<Attachment>,
    /// Present only on `inbound` rows, pointing at the raw MIME in S3 (step 7).
    pub raw_s3_key: Option<String>,
    pub created_at: u64,
}

impl HasID for TicketMessage {
    fn id(&self) -> &str {
        &self.id
    }
}

/// Update shapes for `ticket_message`. Only `rfc_message_id` is ever
/// updated after creation — every other attribute is fixed at
/// [`Handler::create_ticket_message`] time — because it's the one thing
/// that can't be known until *after* the row exists: a reply is persisted
/// first, then sent, then (only on success) stamped with the id SES
/// returned. See `graphql::mutations::reply_to_ticket`'s doc comment for
/// why the row is never rolled back when the send itself fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TicketMessageUpdateShape<'a> {
    SetRfcMessageId {
        rfc_message_id: &'a str,
    },
    /// Stamp the raw MIME's S3 key onto an `inbound` row. Set after the row
    /// is created for the same reason `rfc_message_id` is: the pipeline
    /// creates the row, then stores the raw bytes (or, for the Lambda, has
    /// already relied on SES having stored them, and only needs to record
    /// the key), so this can't be known at `create_ticket_message` time
    /// without reordering the pipeline in a way that would leave a
    /// half-written row on a storage failure.
    SetRawS3Key {
        raw_s3_key: &'a str,
    },
    /// Stamp the final `{s3_key, filename, content_type, size}` list onto a
    /// message once every attachment has been uploaded (inbound) or moved
    /// out of `pending/` (`replyToTicket`) — see [`Attachment`]'s doc
    /// comment. An empty slice `REMOVE`s the attribute per the
    /// omit-optional-attributes house rule.
    SetAttachments {
        attachments: &'a [Attachment],
    },
}

/// `Sync` is required so a `&impl Handler` (including the erased handle returned by
/// [`crate::app::HasDb::db`]) can be held across `.await` inside the `Send` futures
/// the GraphQL/Poem stack builds. Both implementors ([`crate::dynamodb::Handler`],
/// [`crate::mockdb::Handler`]) are already `Sync`.
///
/// Every method here follows the RPITIT style already used by
/// [`crate::mail::Handler`]: `fn foo(&self, ...) -> impl Future<Output = Result<T>> + Send`,
/// not `async_trait`.
/// Resolve an `--instance` argument, which may be a slug or an id, to the
/// instance's **id**.
///
/// Every row that references an instance stores its id. Taking the argument at
/// face value silently writes the slug into `instance_id` instead, which does
/// not fail anywhere: the row is created, the CLI prints it back, and nothing
/// reads it until a resolver tries to load the instance by that id and finds
/// nothing. The visible symptom is an empty instance switcher for a user whose
/// membership plainly exists — a long way from the cause.
///
/// Slug is tried first because that is what an operator types. An argument that
/// matches no slug is treated as an id and checked, so a typo is rejected here
/// rather than persisted.
pub async fn resolve_instance_id(db: &impl Handler, slug_or_id: &str) -> Result<String> {
    if let Some(id) = db.get_instance_id_by_slug(slug_or_id).await? {
        return Ok(id);
    }
    let found = db.get_instances(&[slug_or_id]).await?;
    if found.into_iter().next().flatten().is_some() {
        return Ok(slug_or_id.to_string());
    }
    Err(Error::NotFound(format!(
        "no instance with slug or id {slug_or_id:?}"
    )))
}

/// Validate a candidate instance slug's *format*: lowercase ASCII letters,
/// digits and hyphens only, non-empty, no leading/trailing hyphen, capped at
/// a sane length (DNS-label-sized, since a slug also has to look reasonable
/// in a URL path segment and in a `[#{slug}-{number}]` subject tag).
///
/// This checks format only — whether the slug is already *taken* is a
/// separate, deliberately racy pre-check (`get_instance_id_by_slug`; see
/// [`Handler::create_instance`]'s doc comment for why). Shared by
/// `bin/cli.rs`'s `instance create` and the `createInstance` GraphQL
/// mutation so the two can never disagree about what counts as a valid
/// slug — the CLI had no format check before this function existed, so this
/// also tightens what `instance create` itself accepts.
pub fn validate_slug(slug: &str) -> std::result::Result<(), String> {
    const MAX_LEN: usize = 63;
    if slug.is_empty() {
        return Err("slug cannot be empty".to_string());
    }
    if slug.len() > MAX_LEN {
        return Err(format!("slug cannot be longer than {MAX_LEN} characters"));
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err("slug may only contain lowercase letters, digits, and hyphens".to_string());
    }
    if slug.starts_with('-') || slug.ends_with('-') {
        return Err("slug cannot start or end with a hyphen".to_string());
    }
    Ok(())
}

/// Trim, lowercase, and shape-check a user's email address (exactly one `@`,
/// non-empty local and domain parts). The one normalizer for every entry point
/// that writes or looks up a `user.email` or a `login_code` key — run it once at
/// the boundary and use the result for every read and write after, which is
/// what makes user-email matching case-insensitive. See `SCHEMA.md`'s `user`
/// table for the invariant. Kept separate from `normalize_ticket_email` in
/// `graphql/mutations.rs` so either can change without the other following.
pub fn normalize_user_email(raw: &str) -> std::result::Result<String, String> {
    let email = raw.trim().to_lowercase();
    let Some((local, domain)) = email.split_once('@') else {
        return Err(format!(
            "{raw:?} is not a valid email address (missing '@')"
        ));
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err(format!("{raw:?} is not a valid email address"));
    }
    Ok(email)
}

pub trait Handler: Sync {
    // ── instance ──────────────────────────────────────────────────────────
    fn get_instances<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> impl Future<Output = Result<Vec<Option<Instance>>>> + Send;
    /// Resolve a slug to its instance id via `slug-index`, collapsed through
    /// [`at_most_one`]. Slug uniqueness is enforced at the application layer
    /// (DynamoDB only enforces uniqueness on the primary key) — see
    /// [`Handler::create_instance`]'s doc comment and `SCHEMA.md`'s known-issues
    /// register for the race this leaves open.
    fn get_instance_id_by_slug(
        &self,
        slug: &str,
    ) -> impl Future<Output = Result<Option<String>>> + Send;
    /// Create an instance. Callers (the GraphQL layer, the CLI) are expected to
    /// call [`Handler::get_instance_id_by_slug`] first and reject a taken slug
    /// before calling this — this method itself does not re-check, so a
    /// concurrent pair of callers that both pass that pre-check can still both
    /// succeed here, leaving two instances that share a slug. That race is
    /// documented (not fixed) in `SCHEMA.md`'s known-issues register: instance
    /// creation is a rare, operator/admin-driven action (CLI bootstrap, or
    /// the superuser-only `createInstance` GraphQL mutation), not a
    /// high-concurrency user-facing path, so the explicit pre-check plus a
    /// documented race was chosen over a deterministic-id-from-slug scheme
    /// (which would break if a slug is ever renamed, since `id` is the stable
    /// foreign key every other table references).
    fn create_instance(
        &self,
        name: &str,
        slug: &str,
        from_name: &str,
        signature: &str,
        public_submission_enabled: bool,
        kind: InstanceKind,
    ) -> impl Future<Output = Result<Instance>> + Send;
    fn update_instance(
        &self,
        id: &str,
        change: InstanceUpdateShape<'_>,
    ) -> impl Future<Output = Result<()>> + Send;
    /// Every instance, active and deleted alike (callers filter as needed) — a
    /// base-table scan, not a query. Fine at this table's scale: instances are
    /// tenant organisations, created by an operator via the CLI, not a
    /// high-cardinality user-generated table.
    fn list_instances(&self) -> impl Future<Output = Result<Vec<Instance>>> + Send;

    // ── inbound_address ──────────────────────────────────────────────────────
    /// `address` must already be normalized (lowercased; `*@domain` for a
    /// wildcard) — see `crate::inbound::routing` for the normalization/
    /// classification logic callers are expected to run first.
    fn create_inbound_address(
        &self,
        address: &str,
        instance_id: &str,
        kind: AddressKind,
    ) -> impl Future<Output = Result<InboundAddress>> + Send;
    fn get_inbound_address(
        &self,
        address: &str,
    ) -> impl Future<Output = Result<Option<InboundAddress>>> + Send;
    fn delete_inbound_address(&self, address: &str) -> impl Future<Output = Result<()>> + Send;
    fn list_inbound_addresses_by_instance(
        &self,
        instance_id: &str,
    ) -> impl Future<Output = Result<Vec<InboundAddress>>> + Send;

    // ── membership ────────────────────────────────────────────────────────
    fn create_membership(
        &self,
        user_id: &str,
        instance_id: &str,
        role: MembershipRole,
    ) -> impl Future<Output = Result<Membership>> + Send;
    fn delete_membership(&self, id: &str) -> impl Future<Output = Result<()>> + Send;
    /// Change an existing membership's role in place. Added for the
    /// superuser-only `setMemberRole` GraphQL mutation, which needs to
    /// change a role without disturbing the membership's `id` (a
    /// delete-then-recreate would work too, but churns the row's identity
    /// for no reason and briefly leaves the user with no membership row at
    /// all if the create half failed).
    fn update_membership_role(
        &self,
        id: &str,
        role: MembershipRole,
    ) -> impl Future<Output = Result<()>> + Send;
    /// Apply a [`NotificationSettingsPatch`] to a membership row —
    /// `updateNotificationSettings`'s write path. Only the fields the patch
    /// sets are touched (`SET`, never `REMOVE`); an empty patch
    /// (`NotificationSettingsPatch::is_empty`) is a no-op that sends no
    /// `UpdateItem` at all, matching the house rule against a call with an
    /// empty update expression.
    fn update_membership_notification_settings(
        &self,
        id: &str,
        patch: &NotificationSettingsPatch,
    ) -> impl Future<Output = Result<()>> + Send;
    fn list_memberships_by_user(
        &self,
        user_id: &str,
    ) -> impl Future<Output = Result<Vec<Membership>>> + Send;
    fn list_memberships_by_instance(
        &self,
        instance_id: &str,
    ) -> impl Future<Output = Result<Vec<Membership>>> + Send;

    // ── user ──────────────────────────────────────────────────────────────
    fn get_users<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> impl Future<Output = Result<Vec<Option<User>>>> + Send;
    /// Resolve an email to its user id via `email-index`, collapsed through
    /// [`at_most_one`] — unlike seslogin's raw `Vec<String>`, callers here get a
    /// single answer directly, since every call site immediately wants "the one
    /// user with this email, if any" rather than the raw index hits.
    ///
    /// **Callers must pass an already-normalized email** ([`normalize_user_email`]
    /// — trimmed, lowercase). This method matches exactly what it's given; it
    /// does no normalization of its own, so a caller that skips
    /// `normalize_user_email` will silently fail to find a user whose email
    /// differs only in case or surrounding whitespace.
    fn get_user_id_by_email(
        &self,
        email: &str,
    ) -> impl Future<Output = Result<Option<String>>> + Send;
    fn create_user(&self, email: &str, name: &str) -> impl Future<Output = Result<User>> + Send;
    /// Every user — a base-table scan, not a query, for the same reason as
    /// [`Handler::list_instances`]: `user` has no listing GSI (only
    /// `email-index`, for the login path), and this table's cardinality is a
    /// handful of team members, not a user-generated table. Backs
    /// `bin/cli.rs`'s `user list`.
    fn list_users(&self) -> impl Future<Output = Result<Vec<User>>> + Send;
    fn update_user(
        &self,
        id: &str,
        change: UserUpdateShape<'_>,
    ) -> impl Future<Output = Result<()>> + Send;

    // ── login_code ────────────────────────────────────────────────────────
    fn put_login_code(
        &self,
        email: &str,
        code_hash: &str,
        expires_at: u64,
        now: u64,
    ) -> impl Future<Output = Result<()>> + Send;
    fn get_login_code(&self, email: &str)
    -> impl Future<Output = Result<Option<LoginCode>>> + Send;
    fn delete_login_code(&self, email: &str) -> impl Future<Output = Result<()>> + Send;
    fn increment_login_code_attempts(&self, email: &str)
    -> impl Future<Output = Result<()>> + Send;

    // ── user_token ────────────────────────────────────────────────────────
    fn create_user_token(
        &self,
        id: &str,
        token_hash: &str,
        user_id: &str,
        expires_at: u64,
    ) -> impl Future<Output = Result<UserToken>> + Send;
    fn get_user_token(&self, id: &str) -> impl Future<Output = Result<Option<UserToken>>> + Send;
    fn update_user_token(
        &self,
        id: &str,
        change: UserTokenUpdateShape,
    ) -> impl Future<Output = Result<()>> + Send;
    fn delete_user_token(&self, id: &str) -> impl Future<Output = Result<()>> + Send;

    // ── oauth_grant ───────────────────────────────────────────────────────
    /// Caller supplies the id (and every other field) — unlike
    /// `create_user_token`, the id is embedded in the tokens `oauth::mint_grant`
    /// builds, so it must exist before the row does.
    fn create_oauth_grant(&self, grant: &OAuthGrant) -> impl Future<Output = Result<()>> + Send;
    /// Strongly consistent `GetItem` — a just-minted token must authenticate on
    /// its first use, and a just-revoked one must stop working immediately.
    fn get_oauth_grant(&self, id: &str) -> impl Future<Output = Result<Option<OAuthGrant>>> + Send;
    fn update_oauth_grant(
        &self,
        id: &str,
        change: OAuthGrantUpdateShape,
    ) -> impl Future<Output = Result<()>> + Send;
    fn delete_oauth_grant(&self, id: &str) -> impl Future<Output = Result<()>> + Send;
    /// Backs the "connected apps" list: every grant a user has authorized.
    fn list_oauth_grants_by_user(
        &self,
        user_id: &str,
    ) -> impl Future<Output = Result<Vec<OAuthGrant>>> + Send;

    // ── api_token ─────────────────────────────────────────────────────────
    /// `id` is supplied by the caller (`auth::issue_api_token`), not
    /// generated here — unlike every other `create_*` method, which mints
    /// its own id, this one needs to mint the id *first* so it can be
    /// embedded in the returned token string before the row exists. The
    /// `attribute_not_exists(id)` condition guards against the same
    /// astronomically-unlikely nanoid collision `create_instance` documents,
    /// not against a real caller race.
    fn create_api_token(
        &self,
        id: &str,
        instance_id: &str,
        name: &str,
        token_hash: &str,
        created_by_user_id: &str,
    ) -> impl Future<Output = Result<ApiToken>> + Send;
    /// A consistent `GetItem` by id — see [`ApiToken`]'s doc comment for why
    /// this table has no `token_hash` GSI to look up by instead.
    fn get_api_token(&self, id: &str) -> impl Future<Output = Result<Option<ApiToken>>> + Send;
    /// The management list (`Instance.apiTokens`) — every token for one
    /// instance, via `instance_id-index`.
    fn list_api_tokens_by_instance(
        &self,
        instance_id: &str,
    ) -> impl Future<Output = Result<Vec<ApiToken>>> + Send;
    fn update_api_token(
        &self,
        id: &str,
        change: ApiTokenUpdateShape<'_>,
    ) -> impl Future<Output = Result<()>> + Send;
    /// Hard delete — nothing else in the schema references an `api_token`
    /// row, so unlike a ticket or an instance there is no soft-delete
    /// marker to set instead.
    fn delete_api_token(&self, id: &str) -> impl Future<Output = Result<()>> + Send;

    // ── project ───────────────────────────────────────────────────────────
    #[allow(clippy::too_many_arguments)]
    fn create_project(
        &self,
        instance_id: &str,
        fields: &ProjectFields,
    ) -> impl Future<Output = Result<Project>> + Send;
    fn get_projects<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> impl Future<Output = Result<Vec<Option<Project>>>> + Send;
    /// Every project for one instance, via `instance_id-index` — the
    /// projects list's data source. Unpaginated, matching
    /// `list_inbound_addresses_by_instance`: an invoicing instance's project
    /// list is small enough (clients/jobs, not a user-generated table) that
    /// a Relay connection would be overhead with no real page to turn.
    fn list_projects_by_instance(
        &self,
        instance_id: &str,
    ) -> impl Future<Output = Result<Vec<Project>>> + Send;
    fn update_project(
        &self,
        id: &str,
        change: ProjectUpdateShape<'_>,
    ) -> impl Future<Output = Result<()>> + Send;

    // ── billable_item ─────────────────────────────────────────────────────
    fn create_billable_item(
        &self,
        item: &NewBillableItem<'_>,
    ) -> impl Future<Output = Result<BillableItem>> + Send;
    /// `rebillExpense`'s write: `Put` a new billable item carrying
    /// `source_expense_id = expense_id`, and `SET billable_item_id` on the
    /// expense conditioned on `attribute_exists(id) AND
    /// attribute_not_exists(billable_item_id)`, in one transaction.
    /// `Ok(None)` (nothing written) when the expense is gone or was already
    /// re-billed.
    fn rebill_expense(
        &self,
        expense_id: &str,
        item: &NewBillableItem<'_>,
    ) -> impl Future<Output = Result<Option<BillableItem>>> + Send;
    fn get_billable_items<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> impl Future<Output = Result<Vec<Option<BillableItem>>>> + Send;
    /// Apply `change`. `invoice_id` must be the item's *current* `invoice_id`
    /// (the caller already has the row, from `require_billable_item_member`
    /// or equivalent) and decides which of two write shapes this uses:
    ///
    /// - `None` (unbilled item): a plain conditional `UpdateItem`
    ///   (`attribute_exists(id) AND attribute_not_exists(invoice_id)`) —
    ///   unchanged from before invoices existed.
    /// - `Some(invoice_id)` (item on a draft invoice — the caller is
    ///   expected to have already rejected a *finalized* one): a
    ///   `TransactWriteItems` that also bumps that invoice's `version`,
    ///   conditioned on the invoice still being `status = draft`. This is
    ///   what makes a concurrent `finalize_invoice` (which reads a `version`
    ///   before this call and conditions its write on it) fail cleanly
    ///   instead of freezing a snapshot mid-edit.
    ///
    /// Returns `Ok(false)` (nothing written) when either path's condition(s)
    /// fail — the row vanished, was put on an invoice between the caller's
    /// read and this write, or (the `Some` path) its invoice was finalized
    /// concurrently — so the caller can report a conflict instead of
    /// silently editing a billed/frozen line.
    fn update_billable_item(
        &self,
        id: &str,
        invoice_id: Option<&str>,
        change: BillableItemUpdateShape<'_>,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// Delete, with the same "exists and unbilled" condition and `Ok(false)`
    /// contract as [`Self::update_billable_item`]'s `None` path. Deleting an
    /// item on any invoice (draft or finalized) is refused — see
    /// CLAUDE.md's "Invoicing" house rule.
    ///
    /// `source_expense_id` must be the item's current one: when set, the
    /// delete is a transaction that also `REMOVE`s the expense's
    /// `billable_item_id` (conditioned on it still naming this item), so the
    /// expense can be re-billed again.
    fn delete_billable_item(
        &self,
        id: &str,
        source_expense_id: Option<&str>,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// One page of billable items, newest `date` first (ties in DynamoDB's
    /// own index order, i.e. by `id`), keyset-paginated. `filter` is applied
    /// as a `FilterExpression`, which DynamoDB evaluates *after* `Limit` — so
    /// an implementation must keep querying until it has `page.limit`
    /// matching rows or the index is exhausted, never stop at the first
    /// short page.
    fn list_billable_items(
        &self,
        scope: BillableItemScope<'_>,
        filter: BillableItemFilter,
        page: ListBillableItemsPage,
    ) -> impl Future<Output = Result<Vec<BillableItem>>> + Send;
    /// Same contract as [`Self::get_billable_items`], but a strongly
    /// consistent `BatchGetItem` — used only by `finalize_invoice`'s own
    /// step 2 (CLAUDE.md's "finalize algorithm"), which must see every
    /// item's *just-written* `invoice_id`, not a possibly-stale replica.
    fn get_billable_items_consistent<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> impl Future<Output = Result<Vec<Option<BillableItem>>>> + Send;

    // ── expense ───────────────────────────────────────────────────────────
    //
    // `date` is a DynamoDB reserved word, aliased `#d` as for `billable_item`.
    //
    fn create_expense(
        &self,
        instance_id: &str,
        fields: &ExpenseFields,
        created_by_user_id: &str,
    ) -> impl Future<Output = Result<Expense>> + Send;
    fn get_expenses<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> impl Future<Output = Result<Vec<Option<Expense>>>> + Send;
    /// Full-replace every editable attribute: present values are `SET`,
    /// absent optional ones `REMOVE`d (so switching a purchase to a trip
    /// drops `supplier`/`amount_cents`/`gst_cents`, and clearing a project
    /// drops the row out of `project_id-date-index`). `instance_id` and the
    /// `created_*` attributes never change. `Ok(false)` (nothing written)
    /// when the row no longer exists.
    fn update_expense(
        &self,
        id: &str,
        fields: &ExpenseFields,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// `Ok(false)` when the row no longer exists, or has been re-billed
    /// (`attribute_not_exists(billable_item_id)` is part of the condition).
    fn delete_expense(&self, id: &str) -> impl Future<Output = Result<bool>> + Send;
    /// Set (`Some`) or `REMOVE` (`None`) an expense's receipt attributes.
    /// `Ok(false)` when the row no longer exists.
    fn set_expense_receipt(
        &self,
        id: &str,
        receipt: Option<&ExpenseReceipt>,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// Every expense in `instance_id` dated `from..=to` (`YYYY-MM-DD`),
    /// oldest first — the reports' and CSV export's source.
    fn list_expenses_in_range(
        &self,
        instance_id: &str,
        from: &str,
        to: &str,
    ) -> impl Future<Output = Result<Vec<Expense>>> + Send;
    /// Every expense on one project (the sparse `project_id-date-index`),
    /// for the project's financial summary.
    fn list_all_expenses_by_project(
        &self,
        project_id: &str,
    ) -> impl Future<Output = Result<Vec<Expense>>> + Send;
    /// One page of expenses, newest `date` first, keyset-paginated —
    /// [`Self::list_billable_items`]'s contract exactly, including "keep
    /// querying until the page is full" when `category` adds a
    /// `FilterExpression`.
    fn list_expenses(
        &self,
        scope: ExpenseScope<'_>,
        category: Option<ExpenseCategory>,
        page: ListExpensesPage,
    ) -> impl Future<Output = Result<Vec<Expense>>> + Send;
    /// Total distance, in tenths of a km, of `user_id`'s vehicle trips in
    /// `instance_id` dated `from..=to` (`YYYY-MM-DD`) — the 5,000 km running
    /// total. Reads `instance_id-date-index` over that date range, filtered
    /// to `category = vehicle_km` and the user; bounded by one person's
    /// trips in one financial year.
    fn sum_vehicle_km_tenths(
        &self,
        instance_id: &str,
        user_id: &str,
        from: &str,
        to: &str,
    ) -> impl Future<Output = Result<i64>> + Send;

    // ── invoice ───────────────────────────────────────────────────────────
    //
    // `status`, `number`, `version`, and `snapshot` are DynamoDB reserved words: every
    // expression that names one aliases it (`#status`, `#num`, `#v`, `#snap`).
    //
    /// Batch-fetch by id, eventually consistent — the dataloader path
    /// (`Loader<InvoiceId>`, `BillableItem.invoice`/`.status`,
    /// `Invoice.project`-shaped lookups elsewhere) and `invoice(id)`'s data
    /// source. Use [`Self::get_invoice_consistent`] instead wherever the
    /// *current* `version`/`status`/`item_ids` drive a subsequent
    /// conditional write (every invoice mutation's authorization fetch).
    fn get_invoices<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> impl Future<Output = Result<Vec<Option<Invoice>>>> + Send;
    /// A strongly consistent `GetItem` by id — same reasoning as
    /// [`Self::get_billable_items_consistent`]: every invoice mutation reads
    /// the row, checks `status`/`version`, and conditions its write on what
    /// it just read, so a replica lagging behind the caller's own most
    /// recent write (e.g. two mutations back to back) must never be served
    /// here.
    fn get_invoice_consistent(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<Invoice>>> + Send;
    /// Create a draft invoice and attach `item_ids` to it in one
    /// `TransactWriteItems`: `Put` the invoice (`status = draft`, `version =
    /// 1`, `item_ids`) plus, for each item, `Update … SET invoice_id`
    /// conditioned on `attribute_not_exists(invoice_id) AND project_id =
    /// :project_id`. The invoice's own `Put` is conditioned on
    /// `attribute_not_exists(id)` (the same astronomically-unlikely nanoid
    /// guard every other `create_*` uses). `item_ids` must be non-empty and
    /// is **not re-validated here** — the caller (`createInvoice`) is
    /// expected to have already checked every id belongs to `project_id`
    /// and is currently unbilled, for a clear error message; this call
    /// re-checks it atomically via the transaction's own per-item
    /// condition. Returns `Ok(None)` (nothing written) if that condition
    /// failed for any item — a race between the caller's check and this
    /// write, since the pre-check already covers the ordinary case.
    fn create_invoice(
        &self,
        instance_id: &str,
        project_id: &str,
        item_ids: &[String],
        created_by_user_id: &str,
    ) -> impl Future<Output = Result<Option<Invoice>>> + Send;
    /// Add `item_ids` to a draft invoice: one `TransactWriteItems` that `ADD`s
    /// them to the invoice's `item_ids` set and bumps `version` (conditioned
    /// on `status = draft AND version = expected_version`), plus, for each
    /// item, `Update … SET invoice_id` conditioned on
    /// `attribute_not_exists(invoice_id) AND project_id = :project_id` — the
    /// same per-item condition as [`Self::create_invoice`]. `Ok(false)` on
    /// any condition failure (the draft changed concurrently, or an item is
    /// no longer eligible).
    fn add_invoice_items(
        &self,
        invoice_id: &str,
        project_id: &str,
        item_ids: &[String],
        expected_version: u64,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// Remove `item_ids` from a draft invoice: the mirror image of
    /// [`Self::add_invoice_items`] — `DELETE`s them from `item_ids` (same
    /// `status`/`version` condition and `version` bump) plus, for each item,
    /// `REMOVE invoice_id` conditioned on `invoice_id = :invoice_id`.
    /// Removing every item (an empty draft) is allowed; finalizing one is
    /// not (checked in the GraphQL layer, not here).
    fn remove_invoice_items(
        &self,
        invoice_id: &str,
        item_ids: &[String],
        expected_version: u64,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// Delete a draft invoice: `Delete` the invoice row (conditioned on
    /// `status = draft AND version = expected_version`) plus, for each of
    /// `item_ids` (the invoice's own, as the caller last read them),
    /// `REMOVE invoice_id` conditioned on `invoice_id = :invoice_id` — same
    /// per-item shape as [`Self::remove_invoice_items`].
    fn delete_invoice(
        &self,
        invoice_id: &str,
        item_ids: &[String],
        expected_version: u64,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// The finalize write — step 6 of CLAUDE.md's "finalize algorithm". A
    /// `TransactWriteItems` of two updates: the invoice itself, conditioned
    /// on `status = draft AND version = expected_version`, setting
    /// `status=finalized`, `number`, `issue_date`, `snapshot`,
    /// `total_cents`, `finalized_at`, `finalized_by_user_id`, and bumping
    /// `version`; and the number's reservation row in `counter`
    /// ([`invoice_number_reservation_id`]), created conditioned on
    /// `attribute_not_exists(id)` — what makes a duplicate number impossible
    /// now that `finalizeInvoice(number:)` may claim a number below the
    /// counter. (Nothing is written to the items: every item's `invoice_id`
    /// already points at this invoice from the moment it was attached.)
    /// Every other step of the algorithm (reading the draft and its items,
    /// allocating `number`, building the snapshot) happens in the caller
    /// (`graphql::mutations::finalize_invoice`) before this is called — a
    /// counter-allocated `number` having already been taken by the time this
    /// runs is exactly why a failed condition here leaves a *gap* in the
    /// numbering, never a duplicate (see `SCHEMA.md`'s "Known issues").
    /// `Ok(false)` if either condition fails — a concurrent edit, a second
    /// finalize racing this one, or the number already being used.
    ///
    /// Also writes `due_date` and `gst_cents` (denormalised from the
    /// snapshot, like `total_cents`).
    fn finalize_invoice(
        &self,
        instance_id: &str,
        invoice_id: &str,
        expected_version: u64,
        finalized: &FinalizeInvoice<'_>,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// Replace a finalized invoice's `payments` (JSON; `REMOVE`d when empty)
    /// and set (`Some`) or `REMOVE` (`None`) its `paid_date`, bumping
    /// `version`. Conditioned on `status = finalized AND version =
    /// expected_version`, so two people recording payments at once can't
    /// lose one. `Ok(false)` on a condition failure.
    fn set_invoice_payments(
        &self,
        invoice_id: &str,
        expected_version: u64,
        payments: &[InvoicePayment],
        paid_date: Option<&str>,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// Record that `sendInvoice` mailed this (finalized) invoice to `to`.
    fn set_invoice_sent(
        &self,
        invoice_id: &str,
        sent_at: u64,
        to: &[String],
    ) -> impl Future<Output = Result<bool>> + Send;
    /// Every finalized invoice in an instance or project, unpaged — the
    /// reports', CSV export's and project summary's source. Bounded by one
    /// business's invoice history.
    fn list_all_finalized_invoices(
        &self,
        scope: InvoiceScope<'_>,
    ) -> impl Future<Output = Result<Vec<Invoice>>> + Send;

    // ── credit_note ───────────────────────────────────────────────────────
    //
    // `number` and `snapshot` are reserved words, as on `invoice`.
    //
    /// Issue a credit note, in one `TransactWriteItems`: `Put` the note
    /// (`attribute_not_exists(id)`) and, on its invoice, `ADD
    /// credited_cents`/`credited_gst_cents` plus `version`, and `SET
    /// paid_date = settled_date` when `settled_date` is `Some` (the credit
    /// settles the invoice) — conditioned on `status = finalized AND version
    /// = invoice_expected_version`. `Ok(false)` (nothing written) on a
    /// condition failure; the credit-note number, allocated before this, is
    /// then a gap, like an invoice number.
    fn create_credit_note(
        &self,
        note: &CreditNote,
        invoice_expected_version: u64,
        settled_date: Option<&str>,
    ) -> impl Future<Output = Result<bool>> + Send;
    fn get_credit_notes<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> impl Future<Output = Result<Vec<Option<CreditNote>>>> + Send;
    fn get_credit_note_consistent(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<CreditNote>>> + Send;
    /// One page, newest `created_at` first — [`Self::list_invoices`]'s shape.
    fn list_credit_notes(
        &self,
        scope: CreditNoteScope<'_>,
        page: ListCreditNotesPage,
    ) -> impl Future<Output = Result<Vec<CreditNote>>> + Send;
    /// Every credit note in an instance or against one invoice, unpaged.
    fn list_all_credit_notes(
        &self,
        scope: CreditNoteScope<'_>,
    ) -> impl Future<Output = Result<Vec<CreditNote>>> + Send;
    /// `ADD next_credit_note_number 1` on the instance's `counter` row —
    /// the credit-note counterpart of [`Self::increment_invoice_counter`].
    fn increment_credit_note_counter(
        &self,
        instance_id: &str,
    ) -> impl Future<Output = Result<u64>> + Send;
    fn set_credit_note_pdf_key(
        &self,
        id: &str,
        key: &str,
    ) -> impl Future<Output = Result<bool>> + Send;
    fn set_credit_note_sent(
        &self,
        id: &str,
        sent_at: u64,
        to: &[String],
    ) -> impl Future<Output = Result<bool>> + Send;
    /// Cache a finalized invoice's rendered PDF key
    /// (`graphql::mutations::download_invoice_pdf`'s first render).
    /// Conditioned on `attribute_exists(id) AND status = finalized` — a
    /// draft has no PDF to cache a key for. `Ok(false)` if the row is
    /// missing or not finalized (shouldn't happen: the caller already read
    /// the row via `require_invoice_member` and rejected a draft before
    /// rendering). Rendering is deterministic from the frozen `snapshot`,
    /// so a second caller racing this one writes the same key — this is
    /// an unconditional `SET`, not a `attribute_not_exists` guard, on
    /// purpose.
    fn set_invoice_pdf_key(
        &self,
        invoice_id: &str,
        key: &str,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// Atomically allocate the next invoice number via `UpdateItem ADD` on
    /// `counter` (`id = instance_id`, attribute `next_invoice_number`) —
    /// the invoicing counterpart of [`Self::increment_ticket_counter`], same
    /// gap-not-duplicate reasoning.
    fn increment_invoice_counter(
        &self,
        instance_id: &str,
    ) -> impl Future<Output = Result<u64>> + Send;
    /// Read the counter's current value without incrementing it — `0` if
    /// the attribute is absent (no invoice finalized yet). Backs
    /// `InvoicingSettingsInfo.nextInvoiceNumber` (`= this + 1`), read lazily
    /// by that one field's resolver rather than on every `Instance` fetch.
    fn get_invoice_counter(&self, instance_id: &str) -> impl Future<Output = Result<u64>> + Send;
    /// `setNextInvoiceNumber`'s write path: set `next_invoice_number =
    /// new_value`, conditioned on `attribute_not_exists(next_invoice_number)
    /// OR next_invoice_number <= :new_value` — forward-only. `Ok(false)`
    /// when the condition fails ("can only move forward").
    fn set_next_invoice_number(
        &self,
        instance_id: &str,
        new_value: u64,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// Whether `number` is already used by an invoice in this instance — the
    /// check behind `finalizeInvoice(number:)`, which may claim any unused
    /// number, lower ones included. First a consistent `GetItem` of its
    /// reservation row ([`invoice_number_reservation_id`], written by every
    /// [`Self::finalize_invoice`]); then, for invoices finalized before
    /// reservation rows existed, a query of the instance's invoices for that
    /// `number`. The query is eventually consistent, which only matters for
    /// an invoice finalized in the last moment — and every such invoice has a
    /// reservation row. Advisory: the reservation's condition inside
    /// `finalize_invoice` is what actually prevents a duplicate.
    fn invoice_number_used(
        &self,
        instance_id: &str,
        number: u32,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// One page of an invoice listing, newest `created_at` first — mirrors
    /// [`Self::list_billable_items`] exactly, including the "no `Limit` on a
    /// filtered query" rule.
    fn list_invoices(
        &self,
        scope: InvoiceScope<'_>,
        filter: InvoiceListFilter,
        page: ListInvoicesPage,
    ) -> impl Future<Output = Result<Vec<Invoice>>> + Send;

    // ── ticket ────────────────────────────────────────────────────────────
    fn get_tickets<T: AsRef<str> + Sync>(
        &self,
        ids: &[T],
    ) -> impl Future<Output = Result<Vec<Option<Ticket>>>> + Send;
    /// Atomically allocate the next per-instance ticket number via `UpdateItem
    /// ADD` on `counter` (`id = instance_id`) — never a read-then-write. See
    /// `SCHEMA.md`'s "Known issues" register for the duplicate-number race
    /// this closes: a failure after this call succeeds but before the ticket
    /// row is written leaves a *gap*, never a *duplicate*, since the counter
    /// itself never moves backward.
    fn increment_ticket_counter(
        &self,
        instance_id: &str,
    ) -> impl Future<Output = Result<u64>> + Send;
    /// Create a new, always-`Open`, always-unassigned ticket. `number` must
    /// come from a prior [`Handler::increment_ticket_counter`] call —
    /// callers, not this method, own the counter/ticket-write sequencing.
    fn create_ticket(
        &self,
        instance_id: &str,
        number: u64,
        subject: &str,
        requester_emails: &[String],
        cc_emails: &[String],
    ) -> impl Future<Output = Result<Ticket>> + Send;
    fn update_ticket(
        &self,
        id: &str,
        change: TicketUpdateShape<'_>,
    ) -> impl Future<Output = Result<()>> + Send;
    /// One page of a ticket listing. See [`TicketListFilter`] for which GSI
    /// each variant queries and [`ListTicketsPage`]/[`TicketCursor`] for the
    /// keyset-pagination shape (mirrors seslogin's `list_periods_for_location`).
    fn list_tickets(
        &self,
        instance_id: &str,
        filter: TicketListFilter,
        page: ListTicketsPage,
    ) -> impl Future<Output = Result<Vec<Ticket>>> + Send;
    /// Resolve `{instance_id}#{number}` via `instance_number-index` to a
    /// ticket id — the `[#{slug}-{number}]` subject-tag threading fallback
    /// (step 7). `number` alone, paired with the *already-resolved*
    /// `instance_id` (not the subject's slug, which is cosmetic once an
    /// instance is known), is what the index is keyed on — see
    /// `inbound::resolution::ResolutionPlan::subject_number`'s doc comment
    /// for why this makes the result structurally incapable of resolving to
    /// another tenant's ticket. Collapsed through [`at_most_one`]: the
    /// composite key is meant to be unique by construction (one ticket per
    /// instance+number), so more than one hit is a data-integrity error, not
    /// a normal outcome. Callers still `GetItem` the returned id afterward
    /// (this is a `KEYS_ONLY` index) for a strongly consistent read.
    fn get_ticket_id_by_instance_number(
        &self,
        instance_id: &str,
        number: u64,
    ) -> impl Future<Output = Result<Option<String>>> + Send;

    // ── ticket_message ───────────────────────────────────────────────────
    /// `in_reply_to`/`references` are this new message's *own* threading
    /// headers — for a reply/notice/acknowledgement (step 6), computed by
    /// `outbound::threading_for` from the ticket's prior messages before
    /// this one is built; for an inbound message (step 7), copied from the
    /// arriving mail's own headers. Neither is the row's `rfc_message_id`
    /// (this message's *own* identity once sent) — that lands afterward via
    /// [`Handler::update_ticket_message`], since it isn't known until the
    /// send actually happens.
    #[allow(clippy::too_many_arguments)]
    fn create_ticket_message(
        &self,
        ticket_id: &str,
        kind: TicketMessageKind,
        author_user_id: Option<&str>,
        from_email: Option<&str>,
        to_emails: &[String],
        cc_emails: &[String],
        body_text: Option<&str>,
        body_html: Option<&str>,
        in_reply_to: Option<&str>,
        references: Option<&str>,
    ) -> impl Future<Output = Result<TicketMessage>> + Send;
    /// Every message for a ticket, oldest first (the GSI's natural ascending
    /// scan order) — the thread view's data source.
    fn list_ticket_messages(
        &self,
        ticket_id: &str,
    ) -> impl Future<Output = Result<Vec<TicketMessage>>> + Send;
    fn update_ticket_message(
        &self,
        id: &str,
        change: TicketMessageUpdateShape<'_>,
    ) -> impl Future<Output = Result<()>> + Send;
    /// Resolve an `In-Reply-To`/`References` id to the ticket it belongs to,
    /// via `rfc_message_id-index` (a `KEYS_ONLY` index projecting the
    /// `ticket_message`'s own id, not `ticket_id`) followed by a strongly
    /// consistent `GetItem` on that message to read `ticket_id` back out —
    /// the `In-Reply-To`/`References` threading fallback (step 7). `None`
    /// when no message carries that id. Collapsed through [`at_most_one`]:
    /// `rfc_message_id` is meant to be unique (it's the provider's own
    /// message id), so more than one hit is a data-integrity error.
    fn get_ticket_id_by_rfc_message_id(
        &self,
        rfc_message_id: &str,
    ) -> impl Future<Output = Result<Option<String>>> + Send;

    // ── processed_message ─────────────────────────────────────────────────
    /// Inbound-mail idempotency: a conditional `PutItem`
    /// (`attribute_not_exists(ses_message_id)`) written *before* any other
    /// processing of a given SES message id. Returns `true` when this call
    /// is the one that claimed it (processing should proceed); `false` when
    /// another call already claimed it (a duplicate SQS delivery — the
    /// caller exits successfully without reprocessing). See `SCHEMA.md`'s
    /// "Known issues" register for the window this does and does not close:
    /// marking *before* the work means a crash mid-processing drops the
    /// message rather than duplicating it, a deliberate trade (SES retries
    /// are common; double-posting a customer reply into a ticket is worse
    /// than a rare dropped message that stays in S3 and the DLQ).
    fn claim_processed_message(
        &self,
        ses_message_id: &str,
        now: u64,
        expires_at: u64,
    ) -> impl Future<Output = Result<bool>> + Send;

    // ── webauthn_credential ───────────────────────────────────────────────
    fn create_webauthn_credential(
        &self,
        id: &str,
        user_id: &str,
        name: &str,
        passkey_json: &str,
    ) -> impl Future<Output = Result<WebauthnCredential>> + Send;
    fn get_webauthn_credential(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<WebauthnCredential>>> + Send;
    fn list_webauthn_credentials_by_user(
        &self,
        user_id: &str,
    ) -> impl Future<Output = Result<Vec<WebauthnCredential>>> + Send;
    fn count_webauthn_credentials_by_user(
        &self,
        user_id: &str,
    ) -> impl Future<Output = Result<usize>> + Send;
    fn update_webauthn_credential(
        &self,
        id: &str,
        change: WebauthnCredentialUpdate,
    ) -> impl Future<Output = Result<()>> + Send;
    fn delete_webauthn_credential(&self, id: &str) -> impl Future<Output = Result<()>> + Send;

    // ── ephemeral_state (generic) ────────────────────────────────────────────
    /// Upsert a record into the `ephemeral_state` table (overwrites any existing
    /// item with the same `id`).
    fn put_ephemeral_state(
        &self,
        id: &str,
        kind: &str,
        payload: &str,
        expires_at: u64,
    ) -> impl Future<Output = Result<()>> + Send;
    fn get_ephemeral_state(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<EphemeralState>>> + Send;
    fn delete_ephemeral_state(&self, id: &str) -> impl Future<Output = Result<()>> + Send;

    // ── WebAuthn challenge state (a typed view over ephemeral_state) ─────────
    /// `kind` is `"reg"` or `"auth"`; `user_id` and `state_json` are folded into
    /// the `ephemeral_state` row's opaque JSON `payload` (see
    /// [`WebauthnState`]'s doc comment) rather than becoming attributes of their
    /// own — the table stays generic so step 4's requester submit token can reuse
    /// it without the schema accreting WebAuthn-specific columns.
    fn put_webauthn_state(
        &self,
        id: &str,
        kind: &str,
        user_id: Option<&str>,
        state_json: &str,
        expires_at: u64,
    ) -> impl Future<Output = Result<()>> + Send;
    fn get_webauthn_state(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<WebauthnState>>> + Send;
    fn delete_webauthn_state(&self, id: &str) -> impl Future<Output = Result<()>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_most_one_returns_none_for_no_matches() {
        assert_eq!(
            at_most_one::<i32>(vec![], || "none".to_string()).unwrap(),
            None
        );
    }

    #[test]
    fn at_most_one_returns_the_single_match() {
        assert_eq!(
            at_most_one(vec![42], || "one".to_string()).unwrap(),
            Some(42)
        );
    }

    #[test]
    fn at_most_one_errors_on_multiple_matches() {
        let err = at_most_one(vec![1, 2], || "dup email".to_string()).unwrap_err();
        match err {
            Error::Integrity(msg) => assert_eq!(msg, "dup email"),
            other => panic!("expected Integrity, got {other:?}"),
        }
    }

    #[test]
    fn ticket_status_as_str_parse_round_trips() {
        for status in [
            TicketStatus::Open,
            TicketStatus::Closed,
            TicketStatus::Deleted,
        ] {
            assert_eq!(TicketStatus::parse(status.as_str()), Some(status));
        }
    }

    #[test]
    fn ticket_status_parse_rejects_garbage() {
        assert_eq!(TicketStatus::parse("archived"), None);
        assert_eq!(TicketStatus::parse(""), None);
        assert_eq!(TicketStatus::parse("Open"), None);
    }

    #[test]
    fn ticket_message_kind_as_str_parse_round_trips() {
        for kind in [
            TicketMessageKind::Inbound,
            TicketMessageKind::Reply,
            TicketMessageKind::Note,
            TicketMessageKind::System,
        ] {
            assert_eq!(TicketMessageKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn ticket_message_kind_parse_rejects_garbage() {
        assert_eq!(TicketMessageKind::parse("bogus"), None);
    }

    #[test]
    fn ticket_subject_tag_format() {
        assert_eq!(ticket_subject_tag("acme", 42), "[#acme-42]");
        assert_eq!(ticket_subject_tag("ridgeline", 1), "[#ridgeline-1]");
    }

    /// The marker-attribute computation for every `(status, assignee)`
    /// combination — the exhaustive table this whole design lives or dies on.
    /// See `compute_ticket_markers`'s doc comment for why both write paths
    /// (`create_ticket`, `update_ticket`'s `SetStatusAndAssignee`) must go
    /// through this one function rather than each recomputing it.
    #[test]
    fn markers_open_unassigned() {
        let m = compute_ticket_markers("inst1", TicketStatus::Open, None);
        assert_eq!(m.instance_status, "inst1#open");
        assert_eq!(m.instance_visible.as_deref(), Some("inst1"));
        assert_eq!(m.instance_assignee, None);
    }

    #[test]
    fn markers_open_assigned() {
        let m = compute_ticket_markers("inst1", TicketStatus::Open, Some("user1"));
        assert_eq!(m.instance_status, "inst1#open");
        assert_eq!(m.instance_visible.as_deref(), Some("inst1"));
        assert_eq!(m.instance_assignee.as_deref(), Some("inst1#user1"));
    }

    #[test]
    fn markers_closed_unassigned() {
        let m = compute_ticket_markers("inst1", TicketStatus::Closed, None);
        assert_eq!(m.instance_status, "inst1#closed");
        assert_eq!(m.instance_visible.as_deref(), Some("inst1"));
        assert_eq!(m.instance_assignee, None);
    }

    #[test]
    fn markers_closed_assigned() {
        let m = compute_ticket_markers("inst1", TicketStatus::Closed, Some("user1"));
        assert_eq!(m.instance_status, "inst1#closed");
        assert_eq!(m.instance_visible.as_deref(), Some("inst1"));
        assert_eq!(m.instance_assignee.as_deref(), Some("inst1#user1"));
    }

    #[test]
    fn markers_deleted_unassigned() {
        let m = compute_ticket_markers("inst1", TicketStatus::Deleted, None);
        assert_eq!(m.instance_status, "inst1#deleted");
        assert_eq!(m.instance_visible, None);
        assert_eq!(m.instance_assignee, None);
    }

    #[test]
    fn validate_slug_accepts_a_normal_slug() {
        assert!(validate_slug("acme-support").is_ok());
        assert!(validate_slug("a1").is_ok());
    }

    #[test]
    fn validate_slug_rejects_empty() {
        assert!(validate_slug("").is_err());
    }

    #[test]
    fn validate_slug_rejects_uppercase() {
        assert!(validate_slug("Acme").is_err());
    }

    #[test]
    fn validate_slug_rejects_leading_or_trailing_hyphen() {
        assert!(validate_slug("-acme").is_err());
        assert!(validate_slug("acme-").is_err());
    }

    #[test]
    fn validate_slug_rejects_invalid_characters() {
        assert!(validate_slug("acme_support").is_err());
        assert!(validate_slug("acme.support").is_err());
        assert!(validate_slug("acme support").is_err());
    }

    #[test]
    fn validate_slug_rejects_too_long() {
        let slug = "a".repeat(64);
        assert!(validate_slug(&slug).is_err());
        let ok = "a".repeat(63);
        assert!(validate_slug(&ok).is_ok());
    }

    #[test]
    fn normalize_user_email_trims_and_lowercases() {
        assert_eq!(
            normalize_user_email("  Bob@Example.com ").unwrap(),
            "bob@example.com"
        );
    }

    #[test]
    fn normalize_user_email_rejects_malformed_addresses() {
        for bad in ["", "   ", "bob", "@example.com", "bob@", "a@b@c"] {
            assert!(
                normalize_user_email(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn notification_settings_defaults_match_the_documented_table() {
        let d = NotificationSettings::default();
        assert!(d.new_ticket);
        assert!(d.assigned_to_me);
        assert!(d.assigned_to_me_updated);
        assert!(d.unassigned_updated);
        assert!(
            !d.assigned_to_others_updated,
            "off by default — noisy otherwise"
        );
    }

    #[test]
    fn notification_settings_patch_empty_patch_changes_nothing() {
        let current = NotificationSettings::default();
        let patch = NotificationSettingsPatch::default();
        assert!(patch.is_empty());
        assert_eq!(patch.merged_onto(current), current);
    }

    #[test]
    fn notification_settings_patch_merges_only_set_fields() {
        let current = NotificationSettings::default();
        let patch = NotificationSettingsPatch {
            assigned_to_others_updated: Some(true),
            new_ticket: Some(false),
            ..Default::default()
        };
        assert!(!patch.is_empty());
        let merged = patch.merged_onto(current);
        assert!(!merged.new_ticket);
        assert!(merged.assigned_to_others_updated);
        // Untouched fields keep the current value.
        assert_eq!(merged.assigned_to_me, current.assigned_to_me);
        assert_eq!(
            merged.assigned_to_me_updated,
            current.assigned_to_me_updated
        );
        assert_eq!(merged.unassigned_updated, current.unassigned_updated);
    }

    #[test]
    fn instance_kind_as_str_parse_round_trips() {
        for kind in [InstanceKind::Support, InstanceKind::Invoicing] {
            assert_eq!(InstanceKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn instance_kind_parse_rejects_garbage() {
        assert_eq!(InstanceKind::parse("Support"), None);
        assert_eq!(InstanceKind::parse(""), None);
        assert_eq!(InstanceKind::parse("bogus"), None);
    }

    #[test]
    fn require_instance_kind_accepts_a_matching_kind() {
        let instance = Instance {
            id: "inst1".into(),
            name: "Test".into(),
            slug: "test".into(),
            kind: InstanceKind::Invoicing,
            public_submission_enabled: false,
            from_name: String::new(),
            signature: String::new(),
            created_at: 0,
            deleted: false,
            business_name: None,
            business_abn: None,
            business_address: None,
            business_phone: None,
            business_email: None,
            payment_details: None,
            gst_registered: false,
            currency: None,
            payment_terms_days: None,
        };
        assert!(require_instance_kind(&instance, InstanceKind::Invoicing).is_ok());
        assert!(require_instance_kind(&instance, InstanceKind::Support).is_err());
    }

    #[test]
    fn currency_or_default_falls_back_to_aud() {
        let mut instance = Instance {
            id: "inst1".into(),
            name: "Test".into(),
            slug: "test".into(),
            kind: InstanceKind::Invoicing,
            public_submission_enabled: false,
            from_name: String::new(),
            signature: String::new(),
            created_at: 0,
            deleted: false,
            business_name: None,
            business_abn: None,
            business_address: None,
            business_phone: None,
            business_email: None,
            payment_details: None,
            gst_registered: false,
            currency: None,
            payment_terms_days: None,
        };
        assert_eq!(instance.currency_or_default(), "AUD");
        instance.currency = Some("USD".to_string());
        assert_eq!(instance.currency_or_default(), "USD");
    }

    #[test]
    fn validate_currency_code_accepts_three_uppercase_letters() {
        assert!(validate_currency_code("AUD").is_ok());
        assert!(validate_currency_code("USD").is_ok());
    }

    #[test]
    fn validate_currency_code_rejects_wrong_length() {
        assert!(validate_currency_code("AU").is_err());
        assert!(validate_currency_code("AUDD").is_err());
        assert!(validate_currency_code("").is_err());
    }

    #[test]
    fn validate_currency_code_rejects_lowercase_or_non_letters() {
        assert!(validate_currency_code("aud").is_err());
        assert!(validate_currency_code("A1D").is_err());
        assert!(validate_currency_code("A$D").is_err());
    }

    /// The case that makes `Deleted` its own branch rather than falling out of
    /// the same logic as `Open`/`Closed`: a deleted-but-still-assigned ticket
    /// must drop `instance_assignee` too, not just `instance_visible` — "assigned
    /// to me" must never surface a deleted ticket.
    #[test]
    fn markers_deleted_assigned_still_drops_assignee_marker() {
        let m = compute_ticket_markers("inst1", TicketStatus::Deleted, Some("user1"));
        assert_eq!(m.instance_status, "inst1#deleted");
        assert_eq!(m.instance_visible, None);
        assert_eq!(
            m.instance_assignee, None,
            "a deleted ticket must never carry an instance_assignee marker, even if it has an assignee_user_id"
        );
    }
}
