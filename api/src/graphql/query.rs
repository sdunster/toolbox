//! `QueryRoot` and the `User` GraphQL object.
//!
//! Split into its own module the way seslogin splits `query.rs` from
//! `mutations.rs` — `graphql::mod` just wires the two together into a schema.

use std::marker::PhantomData;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_graphql::connection::{Connection, EmptyFields};
use async_graphql::dataloader::DataLoader;
use async_graphql::{Context, Enum, ID, InputObject, Object, SimpleObject};

use crate::app::{App, HasDb, HasStorage};
use crate::auth::AuthInfo;
use crate::db;
use crate::db::Handler as _;
use crate::storage::Handler as _;

use super::auth::{AuthGuard, AuthRequirement, is_member, is_owner};
use super::dataloader::DatabaseLoader;
use super::error::ApiError;
use super::pagination::{build_connection, pagination_args};
use super::{InstanceId, InvoiceId, ProjectId, UserId};
use crate::invoicing::{self, money, vehicle};

/// Metadata for a stored passkey credential — never the credential itself (no
/// private key material, no raw `passkey_json`).
#[derive(SimpleObject, Clone, Debug)]
pub struct PasskeyInfo {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl From<db::WebauthnCredential> for PasskeyInfo {
    fn from(c: db::WebauthnCredential) -> Self {
        Self {
            id: c.id,
            name: c.name,
            created_at: c.created_at as i64,
            last_used_at: c.last_used_at.map(|t| t as i64),
        }
    }
}

/// `instance.kind`, exposed over GraphQL. Immutable after creation — see
/// `db::InstanceKind`'s doc comment.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum InstanceKindType {
    Support,
    Invoicing,
}

impl From<db::InstanceKind> for InstanceKindType {
    fn from(kind: db::InstanceKind) -> Self {
        match kind {
            db::InstanceKind::Support => Self::Support,
            db::InstanceKind::Invoicing => Self::Invoicing,
        }
    }
}

impl From<InstanceKindType> for db::InstanceKind {
    fn from(kind: InstanceKindType) -> Self {
        match kind {
            InstanceKindType::Support => Self::Support,
            InstanceKindType::Invoicing => Self::Invoicing,
        }
    }
}

/// `membership.role`, exposed over GraphQL.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum MembershipRoleType {
    Owner,
    Agent,
}

impl From<db::MembershipRole> for MembershipRoleType {
    fn from(role: db::MembershipRole) -> Self {
        match role {
            db::MembershipRole::Owner => Self::Owner,
            db::MembershipRole::Agent => Self::Agent,
        }
    }
}

impl From<MembershipRoleType> for db::MembershipRole {
    fn from(role: MembershipRoleType) -> Self {
        match role {
            MembershipRoleType::Owner => Self::Owner,
            MembershipRoleType::Agent => Self::Agent,
        }
    }
}

/// A member's five staff-notification preferences, exposed over GraphQL as
/// `NotificationSettings` — see `db::NotificationSettings`'s doc comment for
/// what each field means and what it defaults to when never explicitly set.
#[derive(SimpleObject, Clone, Copy, Debug, PartialEq, Eq)]
#[graphql(name = "NotificationSettings")]
pub struct NotificationSettingsInfo {
    pub new_ticket: bool,
    pub assigned_to_me: bool,
    pub assigned_to_me_updated: bool,
    pub unassigned_updated: bool,
    pub assigned_to_others_updated: bool,
}

impl From<db::NotificationSettings> for NotificationSettingsInfo {
    fn from(s: db::NotificationSettings) -> Self {
        Self {
            new_ticket: s.new_ticket,
            assigned_to_me: s.assigned_to_me,
            assigned_to_me_updated: s.assigned_to_me_updated,
            unassigned_updated: s.unassigned_updated,
            assigned_to_others_updated: s.assigned_to_others_updated,
        }
    }
}

/// `updateNotificationSettings`'s argument — a patch over
/// [`db::NotificationSettingsPatch`]: every field optional, `null`/omitted
/// meaning "leave this one alone". See that type's doc comment.
#[derive(InputObject, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NotificationSettingsInput {
    pub new_ticket: Option<bool>,
    pub assigned_to_me: Option<bool>,
    pub assigned_to_me_updated: Option<bool>,
    pub unassigned_updated: Option<bool>,
    pub assigned_to_others_updated: Option<bool>,
}

impl From<NotificationSettingsInput> for db::NotificationSettingsPatch {
    fn from(i: NotificationSettingsInput) -> Self {
        Self {
            new_ticket: i.new_ticket,
            assigned_to_me: i.assigned_to_me,
            assigned_to_me_updated: i.assigned_to_me_updated,
            unassigned_updated: i.unassigned_updated,
            assigned_to_others_updated: i.assigned_to_others_updated,
        }
    }
}

/// `inbound_address.kind`, exposed over GraphQL.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum AddressKindType {
    Exact,
    Wildcard,
}

impl From<db::AddressKind> for AddressKindType {
    fn from(kind: db::AddressKind) -> Self {
        match kind {
            db::AddressKind::Exact => Self::Exact,
            db::AddressKind::Wildcard => Self::Wildcard,
        }
    }
}

/// An `inbound_address` row, exposed over GraphQL. Only reachable through
/// `Instance.inboundAddresses`, which is owner-guarded — see that field's doc
/// comment.
#[derive(SimpleObject, Clone, Debug)]
pub struct InboundAddressInfo {
    pub address: String,
    pub kind: AddressKindType,
    pub created_at: i64,
}

impl From<db::InboundAddress> for InboundAddressInfo {
    fn from(a: db::InboundAddress) -> Self {
        Self {
            address: a.address,
            kind: a.kind.into(),
            created_at: a.created_at as i64,
        }
    }
}

/// One instance, exposed publicly and unauthenticated via `publicInstances` —
/// **only** the name and slug, so nothing else about a tenant (its inbound
/// addresses, its members, whether it even has any) leaks to an anonymous
/// caller browsing `/submit`.
#[derive(SimpleObject, Clone, Debug)]
pub struct PublicInstance {
    pub name: String,
    pub slug: String,
}

impl From<db::Instance> for PublicInstance {
    fn from(i: db::Instance) -> Self {
        Self {
            name: i.name,
            slug: i.slug,
        }
    }
}

/// An invoicing instance's seller settings and payment footer — the printed
/// "Invoice to"/seller block and payment text, per CLAUDE.md's "Invoicing"
/// house rule. `null` for a support instance (see
/// [`Instance::invoicing_settings`]). Owner-or-superuser to *write*
/// (`updateInvoicingSettings`/`setNextInvoiceNumber`), but readable by any
/// member — the same posture as every other plain `Instance` field.
///
/// Generic over `A` (unlike a plain `SimpleObject`) so
/// [`Self::next_invoice_number`] can read the `counter` row lazily, through
/// `ctx` — a DB read this field alone needs, so it must not run on every
/// plain `Instance`/`invoicingSettings` fetch that doesn't ask for it.
#[derive(Debug, PartialEq)]
pub struct InvoicingSettingsInfo<A: App + HasDb + Send + Sync> {
    _marker: PhantomData<A>,
    rec: db::Instance,
}

impl<A: App + HasDb + Send + Sync> InvoicingSettingsInfo<A> {
    pub fn new(rec: db::Instance) -> Self {
        Self {
            _marker: PhantomData,
            rec,
        }
    }
}

impl<A: App + HasDb + Send + Sync> Clone for InvoicingSettingsInfo<A> {
    fn clone(&self) -> Self {
        Self {
            _marker: PhantomData,
            rec: self.rec.clone(),
        }
    }
}

#[Object]
impl<A: App + HasDb + Send + Sync + 'static> InvoicingSettingsInfo<A> {
    async fn business_name(&self) -> Option<&str> {
        self.rec.business_name.as_deref()
    }
    async fn business_abn(&self) -> Option<&str> {
        self.rec.business_abn.as_deref()
    }
    async fn business_address(&self) -> Option<&str> {
        self.rec.business_address.as_deref()
    }
    async fn business_phone(&self) -> Option<&str> {
        self.rec.business_phone.as_deref()
    }
    async fn business_email(&self) -> Option<&str> {
        self.rec.business_email.as_deref()
    }
    async fn payment_details(&self) -> Option<&str> {
        self.rec.payment_details.as_deref()
    }
    async fn gst_registered(&self) -> bool {
        self.rec.gst_registered
    }
    /// Defaults to `"AUD"` — see `db::Instance::currency_or_default`.
    async fn currency(&self) -> String {
        self.rec.currency_or_default().to_string()
    }
    /// `(counter value, or 0 if never finalized) + 1` — the number
    /// `finalizeInvoice` will assign next, and `setNextInvoiceNumber`'s
    /// argument shape. Read lazily here (a `counter` `GetItem`), not
    /// carried on `db::Instance`, so a plain `Instance`/`invoicingSettings`
    /// fetch that doesn't ask for this field costs nothing extra.
    async fn next_invoice_number(&self, ctx: &Context<'_>) -> Result<i32> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let counter = app.db().get_invoice_counter(&self.rec.id).await?;
        Ok(i32::try_from(counter.saturating_add(1)).unwrap_or(i32::MAX))
    }
}

/// `updateInvoicingSettings`'s argument — full-replace, unlike
/// [`NotificationSettingsInput`]'s per-field patch: every call writes every
/// field. Every string field is required by the schema but treated as
/// "unset" when blank after trimming — see
/// `graphql::mutations::update_invoicing_settings`'s doc comment for the
/// REMOVE-on-blank rule this input drives.
#[derive(InputObject, Clone, Debug)]
pub struct InvoicingSettingsInput {
    pub business_name: String,
    pub business_abn: String,
    pub business_address: String,
    pub business_phone: String,
    pub business_email: String,
    pub payment_details: String,
    pub gst_registered: bool,
    pub currency: String,
}

/// `createProject`'s argument.
#[derive(InputObject, Clone, Debug)]
pub struct CreateProjectInput {
    pub name: String,
    pub client_name: String,
    pub client_abn: Option<String>,
    pub client_address: Option<String>,
    pub reference: Option<String>,
}

/// `updateProject`'s argument — full-replace, including `archived`.
#[derive(InputObject, Clone, Debug)]
pub struct UpdateProjectInput {
    pub name: String,
    pub client_name: String,
    pub client_abn: Option<String>,
    pub client_address: Option<String>,
    pub reference: Option<String>,
    pub archived: bool,
}

/// An `instance` row, exposed over GraphQL to its members. Generic over `A`
/// for the same reason as [`User`] — see that type's doc comment.
#[derive(Debug, PartialEq)]
pub struct Instance<A: App + HasDb + Send + Sync> {
    _marker: PhantomData<A>,
    rec: db::Instance,
}

impl<A: App + HasDb + Send + Sync> Instance<A> {
    pub fn new(rec: db::Instance) -> Self {
        Self {
            _marker: PhantomData,
            rec,
        }
    }
}

impl<A: App + HasDb + Send + Sync> Clone for Instance<A> {
    fn clone(&self) -> Self {
        Self {
            _marker: PhantomData,
            rec: self.rec.clone(),
        }
    }
}

#[Object]
impl<A: App + HasDb + Send + Sync + 'static> Instance<A> {
    async fn id(&self) -> ID {
        ID(self.rec.id.clone())
    }
    async fn name(&self) -> &str {
        &self.rec.name
    }
    async fn slug(&self) -> &str {
        &self.rec.slug
    }
    /// Immutable after creation — see `db::InstanceKind`'s doc comment.
    async fn kind(&self) -> InstanceKindType {
        self.rec.kind.into()
    }
    /// This instance's invoicing settings, or `null` for a support instance.
    /// See [`InvoicingSettingsInfo`]'s doc comment.
    async fn invoicing_settings(&self) -> Option<InvoicingSettingsInfo<A>> {
        if self.rec.kind != db::InstanceKind::Invoicing {
            return None;
        }
        Some(InvoicingSettingsInfo::new(self.rec.clone()))
    }
    async fn public_submission_enabled(&self) -> bool {
        self.rec.public_submission_enabled
    }
    // Named `from_display_name`, not `from_name`, purely to dodge an
    // async-graphql-derive quirk: the `#[Object]` macro generates an internal
    // `__FieldIdent` enum whose variants are the *literal* resolver method
    // identifiers (no case conversion) alongside its own associated function
    // `__FieldIdent::from_name(...)` — a resolver actually named `from_name`
    // collides with that generated function name and fails to compile. The
    // GraphQL field name is unaffected (`#[graphql(name = ...)]` below).
    #[graphql(name = "fromName")]
    async fn from_display_name(&self) -> &str {
        &self.rec.from_name
    }
    async fn signature(&self) -> &str {
        &self.rec.signature
    }
    async fn created_at(&self) -> i64 {
        self.rec.created_at as i64
    }
    /// Soft-delete marker — see `SCHEMA.md`'s instance soft-delete section.
    /// Reachable here (unlike most tenant data) because the admin
    /// (`adminInstances`/`adminInstance`) queries deliberately surface
    /// deleted instances too, so the web admin UI can show and restore
    /// them; a plain member-facing path never reaches a deleted instance in
    /// the first place (`instance(slug)` returns `null` for one, and
    /// `User.memberships` filters them out), so this field is harmless
    /// there too.
    async fn deleted(&self) -> bool {
        self.rec.deleted
    }

    /// Every inbound address mapped to this instance. Owner-or-superuser:
    /// per the build plan, member invites are deferred to the CLI, and
    /// inbound addressing is treated with the same "owner, not just any
    /// member" sensitivity — an agent can work tickets without being able
    /// to reconfigure where mail routes. A superuser can reach this too
    /// (admin/support), but — per the superuser boundary in `CLAUDE.md` —
    /// that is instance *settings*, not ticket access; nothing ticket-facing
    /// grants a superuser anything here.
    #[graphql(
        guard = "AuthGuard::new(AuthRequirement::InstanceOwnerOrSuperuser(self.rec.id.clone()))"
    )]
    async fn inbound_addresses(&self, ctx: &Context<'_>) -> Result<Vec<InboundAddressInfo>> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let addrs = app
            .db()
            .list_inbound_addresses_by_instance(&self.rec.id)
            .await?;
        Ok(addrs.into_iter().map(InboundAddressInfo::from).collect())
    }

    /// Every member of this instance and their role. Readable by any member
    /// (owner or agent) — agents hand tickets to each other, so the
    /// assignee picker needs the team list, and colleague names/emails are
    /// not sensitive within an instance — and by a superuser, whose admin UI
    /// manages membership through `addMember`/`removeMember`/`setMemberRole`
    /// (superuser-only, `graphql::mutations`); `bin/cli.rs`'s `member add`
    /// remains available for operators working outside the web UI. A
    /// colleague's `User` reached through here can't be used to see which
    /// *other* instances they belong to: `User.memberships` is
    /// self-or-superuser.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::MemberOrSuperuser(self.rec.id.clone()))")]
    async fn members(&self, ctx: &Context<'_>) -> Result<Vec<MemberInfo<A>>> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let memberships = app.db().list_memberships_by_instance(&self.rec.id).await?;
        if memberships.is_empty() {
            return Ok(vec![]);
        }
        let user_ids: Vec<&str> = memberships.iter().map(|m| m.user_id.as_str()).collect();
        let users = app.db().get_users(&user_ids).await?;
        Ok(memberships
            .into_iter()
            .zip(users)
            .filter_map(|(m, user)| {
                user.map(|u| MemberInfo {
                    user: User::new(u),
                    role: m.role.into(),
                })
            })
            .collect())
    }

    /// Every `api_token` row for this instance, oldest first — the token
    /// management page's data source. Owner-or-superuser, same posture (and
    /// same doc-comment reasoning) as [`Self::inbound_addresses`]/
    /// [`Self::members`]: an integration credential is instance *settings*,
    /// not something a plain agent needs to see or a superuser is barred
    /// from managing on an owner's behalf. The secret itself never appears
    /// here — see `createApiToken`'s doc comment for the only time it does.
    #[graphql(
        guard = "AuthGuard::new(AuthRequirement::InstanceOwnerOrSuperuser(self.rec.id.clone()))"
    )]
    async fn api_tokens(&self, ctx: &Context<'_>) -> Result<Vec<ApiTokenInfo<A>>> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let mut tokens = app.db().list_api_tokens_by_instance(&self.rec.id).await?;
        tokens.sort_by_key(|t| t.created_at);
        Ok(tokens.into_iter().map(ApiTokenInfo::new).collect())
    }
}

/// One member of an instance: the user, and their role in this instance
/// specifically (a user's role can differ across the instances they belong
/// to).
#[derive(Debug, PartialEq)]
pub struct MemberInfo<A: App + HasDb + Send + Sync> {
    user: User<A>,
    role: MembershipRoleType,
}

impl<A: App + HasDb + Send + Sync> Clone for MemberInfo<A> {
    fn clone(&self) -> Self {
        Self {
            user: self.user.clone(),
            role: self.role,
        }
    }
}

#[Object]
impl<A: App + HasDb + Send + Sync + 'static> MemberInfo<A> {
    async fn user(&self) -> &User<A> {
        &self.user
    }
    async fn role(&self) -> MembershipRoleType {
        self.role
    }
}

/// An `api_token` row, exposed over GraphQL. Reachable only through
/// `Instance.apiTokens` (owner-or-superuser — see that field's doc comment)
/// and as the payload of `createApiToken`/`updateApiToken`. **Never carries
/// the secret or its hash** — `createApiToken`'s `CreatedApiToken.token` is
/// the one and only place the full secret is ever returned.
#[derive(Debug, PartialEq)]
pub struct ApiTokenInfo<A: App + HasDb + Send + Sync> {
    _marker: PhantomData<A>,
    rec: db::ApiToken,
}

impl<A: App + HasDb + Send + Sync> ApiTokenInfo<A> {
    pub fn new(rec: db::ApiToken) -> Self {
        Self {
            _marker: PhantomData,
            rec,
        }
    }
}

impl<A: App + HasDb + Send + Sync> Clone for ApiTokenInfo<A> {
    fn clone(&self) -> Self {
        Self {
            _marker: PhantomData,
            rec: self.rec.clone(),
        }
    }
}

#[Object]
impl<A: App + HasDb + Send + Sync + 'static> ApiTokenInfo<A> {
    async fn id(&self) -> ID {
        ID(self.rec.id.clone())
    }
    async fn name(&self) -> &str {
        &self.rec.name
    }
    async fn enabled(&self) -> bool {
        self.rec.enabled
    }
    async fn created_at(&self) -> i64 {
        self.rec.created_at as i64
    }
    async fn last_used_at(&self) -> Option<i64> {
        self.rec.last_used_at.map(|t| t as i64)
    }
    /// Dataloaded — see [`TicketMessage::author`]'s doc comment.
    async fn created_by(&self, ctx: &Context<'_>) -> Result<Option<User<A>>> {
        let loader = ctx.data_unchecked::<DataLoader<DatabaseLoader<A>>>();
        let rec = loader
            .load_one(UserId(ID(self.rec.created_by_user_id.clone())))
            .await
            .map_err(|e| anyhow!("Failed to load createdBy via DataLoader: {}", e))?;
        Ok(rec.map(User::new))
    }
}

/// `createApiToken`'s result: the freshly minted [`ApiTokenInfo`] alongside
/// `token`, the full secret — see that mutation's doc comment for why this
/// is the one and only place it appears. Generic over `A` for the same
/// reason as [`Instance`]/[`ApiTokenInfo`] themselves (composes with
/// `build_schema`'s per-binary `A`); not a `#[derive(SimpleObject)]` like
/// [`super::mutations::AttachmentUpload`] because `ApiTokenInfo<A>` is itself
/// a `#[Object]` type carrying `A`'s bounds, not a plain value type.
#[derive(Debug, PartialEq)]
pub struct CreatedApiToken<A: App + HasDb + Send + Sync> {
    token: String,
    api_token: ApiTokenInfo<A>,
}

impl<A: App + HasDb + Send + Sync> CreatedApiToken<A> {
    pub fn new(token: String, api_token: ApiTokenInfo<A>) -> Self {
        Self { token, api_token }
    }
}

#[Object]
impl<A: App + HasDb + Send + Sync + 'static> CreatedApiToken<A> {
    async fn token(&self) -> &str {
        &self.token
    }
    async fn api_token(&self) -> &ApiTokenInfo<A> {
        &self.api_token
    }
}

/// One of the caller's own memberships: which instance, and their role in it.
/// Backs `User.memberships` (i.e. `me.memberships`), which `adminUser` also
/// reuses for a superuser looking up someone else's memberships — see that
/// field's doc comment. `user_id` is carried alongside `instance`/`role`
/// purely so [`Self::notification_settings`] can enforce its self-only
/// guard without a second DB round trip; `settings` is loaded once here
/// (from the same `db::Membership` row `role` already came from) rather
/// than re-fetched by that resolver.
#[derive(Debug, PartialEq)]
pub struct MembershipInfo<A: App + HasDb + Send + Sync> {
    user_id: String,
    instance: Instance<A>,
    role: MembershipRoleType,
    settings: db::NotificationSettings,
}

impl<A: App + HasDb + Send + Sync> MembershipInfo<A> {
    /// Construct directly from already-loaded parts — used by
    /// `graphql::mutations::update_notification_settings`, which builds its
    /// response from the membership row it just patched rather than a fresh
    /// `User.memberships` read.
    pub fn new(
        user_id: String,
        instance: Instance<A>,
        role: MembershipRoleType,
        settings: db::NotificationSettings,
    ) -> Self {
        Self {
            user_id,
            instance,
            role,
            settings,
        }
    }
}

impl<A: App + HasDb + Send + Sync> Clone for MembershipInfo<A> {
    fn clone(&self) -> Self {
        Self {
            user_id: self.user_id.clone(),
            instance: self.instance.clone(),
            role: self.role,
            settings: self.settings,
        }
    }
}

#[Object]
impl<A: App + HasDb + Send + Sync + 'static> MembershipInfo<A> {
    async fn instance(&self) -> &Instance<A> {
        &self.instance
    }
    async fn role(&self) -> MembershipRoleType {
        self.role
    }

    /// This member's own staff-notification preferences for this instance.
    /// **Self only** — the same defence-in-depth reasoning as
    /// `User::passkeys`: `adminUser` lets a superuser reach another user's
    /// `memberships`, and notification preferences are exactly the kind of
    /// per-person setting that shouldn't leak to a lookup, even though
    /// (unlike passkeys) there's no credential material involved.
    /// `FORBIDDEN`, not the defaults, for anyone but the member themselves —
    /// an innocuous-looking default response would be the wrong failure
    /// mode here too, for the same reason `User::passkeys` doesn't return an
    /// empty list.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn notification_settings(&self, ctx: &Context<'_>) -> Result<NotificationSettingsInfo> {
        let Some(AuthInfo::User { id, .. }) = ctx.data_opt::<AuthInfo>() else {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        };
        if id != &self.user_id {
            return Err(
                ApiError::forbidden("Cannot view another member's notification settings").into(),
            );
        }
        Ok(self.settings.into())
    }
}

/// A `user` row, exposed over GraphQL. Generic over `A` (rather than holding a
/// concrete app type) so it composes with `build_schema`'s per-binary `A`, the
/// same reason `MutationRoot<A>` is generic — see `graphql::mod`'s doc comment.
#[derive(Debug, PartialEq)]
pub struct User<A: App + HasDb + Send + Sync> {
    _marker: PhantomData<A>,
    rec: db::User,
}

impl<A: App + HasDb + Send + Sync> User<A> {
    pub fn new(rec: db::User) -> Self {
        Self {
            _marker: PhantomData,
            rec,
        }
    }
}

impl<A: App + HasDb + Send + Sync> Clone for User<A> {
    fn clone(&self) -> Self {
        Self {
            _marker: PhantomData,
            rec: self.rec.clone(),
        }
    }
}

#[Object]
impl<A: App + HasDb + Send + Sync + 'static> User<A> {
    async fn id(&self) -> ID {
        ID(self.rec.id.clone())
    }
    async fn email(&self) -> &str {
        &self.rec.email
    }
    async fn name(&self) -> &str {
        &self.rec.name
    }
    async fn enabled(&self) -> bool {
        self.rec.enabled
    }
    async fn created_at(&self) -> i64 {
        self.rec.created_at as i64
    }
    async fn access_time(&self) -> Option<i64> {
        self.rec.access_time.map(|t| t as i64)
    }
    /// Mirrors `db::User::superuser`. Readable on any `User` object (not
    /// self-guarded like [`Self::passkeys`]) — knowing *whether* someone is
    /// a superuser is not itself sensitive the way passkey credential
    /// metadata is, and the admin user list needs to show it for every row.
    /// It cannot be *set* over GraphQL either way — see
    /// `db::User::superuser`'s doc comment.
    #[graphql(name = "isSuperuser")]
    async fn is_superuser(&self) -> bool {
        self.rec.superuser
    }

    /// The caller's own registered passkeys. **Self only**: now that
    /// `adminUser` lets a superuser look up any user's record, this can no
    /// longer rely on the only path to a `User` object being `me` — a
    /// superuser fetching someone else's passkey *metadata* (registration
    /// time, credential id) would still be a real information leak, even
    /// though it carries no private key material. `FORBIDDEN`, not an empty
    /// list, for anyone but the user themselves (including a superuser) —
    /// an empty list would be indistinguishable from "this user has no
    /// passkeys" and invite exactly the kind of admin-lookup probing this
    /// guard exists to prevent.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn passkeys(&self, ctx: &Context<'_>) -> Result<Vec<PasskeyInfo>> {
        let Some(AuthInfo::User { id, .. }) = ctx.data_opt::<AuthInfo>() else {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        };
        if id != &self.rec.id {
            return Err(ApiError::forbidden("Cannot view another user's passkeys").into());
        }
        let app = ctx.data_unchecked::<Arc<A>>();
        let creds = app
            .db()
            .list_webauthn_credentials_by_user(&self.rec.id)
            .await?;
        Ok(creds.into_iter().map(PasskeyInfo::from).collect())
    }

    /// The caller's own "connected AI apps" (OAuth grants), newest first.
    /// **Self only**, exactly like [`Self::passkeys`] and for the same reason:
    /// `adminUser` lets a superuser look up any user's record, and which AI
    /// clients someone has authorized is theirs to see — `FORBIDDEN`, not an
    /// empty list, for anyone but the user themselves (superusers included).
    /// Filters out anything already past its TTL, since DynamoDB's deletion
    /// lags real expiry.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn oauth_grants(&self, ctx: &Context<'_>) -> Result<Vec<OAuthGrant>> {
        let Some(AuthInfo::User { id, .. }) = ctx.data_opt::<AuthInfo>() else {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        };
        if id != &self.rec.id {
            return Err(ApiError::forbidden("Cannot view another user's connected apps").into());
        }
        let app = ctx.data_unchecked::<Arc<A>>();
        let now = crate::clock::now_sec();
        let mut grants = app
            .db()
            .list_oauth_grants_by_user(&self.rec.id)
            .await?
            .into_iter()
            .filter(|g| g.expires_at > now && g.refresh_expires_at > now)
            .collect::<Vec<_>>();
        grants.sort_by_key(|g| std::cmp::Reverse(g.created_at));
        Ok(grants.into_iter().map(OAuthGrant::new).collect())
    }

    /// Every (non-deleted) instance this user belongs to, and their role in
    /// each — the instance switcher's data source, and also what the admin
    /// user page uses to show a looked-up user's memberships (see this
    /// field's use from `adminUser`: reusing it here, rather than adding a
    /// superuser-only duplicate, keeps "which instances is this user a
    /// member of" defined in exactly one resolver). Deleted instances are
    /// filtered out — see `SCHEMA.md`'s instance soft-delete section for why
    /// this, not a DB-level filter, is where that happens. Same
    /// defence-in-depth guard as [`Self::passkeys`] used to be — unlike
    /// passkeys, a superuser may read it too: reading who belongs to which
    /// instance (not the credentials themselves) is the thing a superuser
    /// legitimately needs `adminUser` for. Anyone else gets `FORBIDDEN` —
    /// `Instance.members` and `Ticket.assignee` hand any member a colleague's
    /// `User`, and which *other* instances (other tenants) that colleague
    /// belongs to is none of their business.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn memberships(&self, ctx: &Context<'_>) -> Result<Vec<MembershipInfo<A>>> {
        let Some(AuthInfo::User {
            id, is_superuser, ..
        }) = ctx.data_opt::<AuthInfo>()
        else {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        };
        if id != &self.rec.id && !*is_superuser {
            return Err(ApiError::forbidden("Cannot view another user's memberships").into());
        }
        let app = ctx.data_unchecked::<Arc<A>>();
        let memberships = app.db().list_memberships_by_user(&self.rec.id).await?;
        if memberships.is_empty() {
            return Ok(vec![]);
        }
        let instance_ids: Vec<&str> = memberships.iter().map(|m| m.instance_id.as_str()).collect();
        let instances = app.db().get_instances(&instance_ids).await?;
        Ok(memberships
            .into_iter()
            .zip(instances)
            .filter_map(|(m, inst)| {
                inst.filter(|i| !i.deleted).map(|i| MembershipInfo {
                    user_id: m.user_id.clone(),
                    instance: Instance::new(i),
                    role: m.role.into(),
                    settings: m.notification_settings,
                })
            })
            .collect())
    }
}

// ── Tickets ──────────────────────────────────────────────────────────────────

/// `ticket.status`, exposed over GraphQL. Also `setTicketStatus`'s argument
/// type — deleting/restoring a ticket is just `setTicketStatus(DELETED)` /
/// `setTicketStatus(OPEN)`, not a separate mutation, since the three states
/// are exactly `TicketStatus`'s three variants.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum TicketStatusType {
    Open,
    Closed,
    Deleted,
}

impl From<db::TicketStatus> for TicketStatusType {
    fn from(s: db::TicketStatus) -> Self {
        match s {
            db::TicketStatus::Open => Self::Open,
            db::TicketStatus::Closed => Self::Closed,
            db::TicketStatus::Deleted => Self::Deleted,
        }
    }
}

impl From<TicketStatusType> for db::TicketStatus {
    fn from(s: TicketStatusType) -> Self {
        match s {
            TicketStatusType::Open => Self::Open,
            TicketStatusType::Closed => Self::Closed,
            TicketStatusType::Deleted => Self::Deleted,
        }
    }
}

/// `tickets(status:)`'s filter — a superset of [`TicketStatusType`] with
/// `ALL` (every non-deleted ticket, the `instance_visible` GSI). `DELETED` is
/// the explicit, owner-only way to reach deleted tickets — enforced in the
/// `tickets` resolver, not by the schema.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum TicketStatusFilterType {
    Open,
    Closed,
    All,
    Deleted,
}

/// `ticket_message.kind`, exposed over GraphQL.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum TicketMessageKindType {
    Inbound,
    Reply,
    Note,
    System,
}

impl From<db::TicketMessageKind> for TicketMessageKindType {
    fn from(k: db::TicketMessageKind) -> Self {
        match k {
            db::TicketMessageKind::Inbound => Self::Inbound,
            db::TicketMessageKind::Reply => Self::Reply,
            db::TicketMessageKind::Note => Self::Note,
            db::TicketMessageKind::System => Self::System,
        }
    }
}

/// One `ticket_message.attachments` entry, exposed over GraphQL. Generic
/// over `A` for the same reason as [`User`]/[`Ticket`] — `downloadUrl`
/// needs `ctx.data_unchecked::<Arc<A>>().storage()` to mint a presigned GET.
///
/// `downloadUrl` is a short-lived presigned S3 GET — resolved fresh on every
/// read, per [`crate::storage::PRESIGN_EXPIRY`], rather than stored, so a
/// link handed to a client is never valid longer than that window.
#[derive(Debug, PartialEq)]
pub struct Attachment<A: App + HasStorage + Send + Sync> {
    _marker: PhantomData<A>,
    rec: db::Attachment,
}

impl<A: App + HasStorage + Send + Sync> Attachment<A> {
    pub fn new(rec: db::Attachment) -> Self {
        Self {
            _marker: PhantomData,
            rec,
        }
    }
}

impl<A: App + HasStorage + Send + Sync> Clone for Attachment<A> {
    fn clone(&self) -> Self {
        Self {
            _marker: PhantomData,
            rec: self.rec.clone(),
        }
    }
}

#[Object]
impl<A: App + HasStorage + Send + Sync + 'static> Attachment<A> {
    async fn filename(&self) -> &str {
        &self.rec.filename
    }
    async fn content_type(&self) -> &str {
        &self.rec.content_type
    }
    async fn size(&self) -> i64 {
        self.rec.size as i64
    }
    async fn download_url(&self, ctx: &Context<'_>) -> Result<String> {
        let app = ctx.data_unchecked::<Arc<A>>();
        app.storage().presign_get(&self.rec.s3_key).await
    }
}

/// A `ticket_message` row, exposed over GraphQL. Reachable only through
/// [`Ticket::messages`] (never listed independently), which is where the
/// internal-note visibility rule is enforced.
#[derive(Debug, PartialEq)]
pub struct TicketMessage<A: App + HasDb + Send + Sync> {
    _marker: PhantomData<A>,
    rec: db::TicketMessage,
}

impl<A: App + HasDb + Send + Sync> TicketMessage<A> {
    pub fn new(rec: db::TicketMessage) -> Self {
        Self {
            _marker: PhantomData,
            rec,
        }
    }
}

impl<A: App + HasDb + Send + Sync> Clone for TicketMessage<A> {
    fn clone(&self) -> Self {
        Self {
            _marker: PhantomData,
            rec: self.rec.clone(),
        }
    }
}

#[Object]
impl<A: App + HasDb + HasStorage + Send + Sync + 'static> TicketMessage<A> {
    async fn id(&self) -> ID {
        ID(self.rec.id.clone())
    }
    async fn ticket_id(&self) -> ID {
        ID(self.rec.ticket_id.clone())
    }
    async fn kind(&self) -> TicketMessageKindType {
        self.rec.kind.into()
    }
    async fn author_user_id(&self) -> Option<ID> {
        self.rec.author_user_id.clone().map(ID)
    }
    /// The message's author, for `reply`/`note` messages — `null` for
    /// `inbound`/`system`. Dataloaded (see `graphql::dataloader`): a thread
    /// with several staff replies resolves every author in one `BatchGetItem`,
    /// not one per message.
    async fn author(&self, ctx: &Context<'_>) -> Result<Option<User<A>>> {
        let Some(uid) = &self.rec.author_user_id else {
            return Ok(None);
        };
        let loader = ctx.data_unchecked::<DataLoader<DatabaseLoader<A>>>();
        let rec = loader
            .load_one(UserId(ID(uid.clone())))
            .await
            .map_err(|e| anyhow!("Failed to load author via DataLoader: {}", e))?;
        Ok(rec.map(User::new))
    }
    async fn from_email(&self) -> Option<&str> {
        self.rec.from_email.as_deref()
    }
    async fn to_emails(&self) -> &[String] {
        &self.rec.to_emails
    }
    async fn cc_emails(&self) -> &[String] {
        &self.rec.cc_emails
    }
    async fn body_text(&self) -> Option<&str> {
        self.rec.body_text.as_deref()
    }
    async fn body_html(&self) -> Option<&str> {
        self.rec.body_html.as_deref()
    }
    async fn rfc_message_id(&self) -> Option<&str> {
        self.rec.rfc_message_id.as_deref()
    }
    async fn in_reply_to(&self) -> Option<&str> {
        self.rec.in_reply_to.as_deref()
    }
    async fn created_at(&self) -> i64 {
        self.rec.created_at as i64
    }
    async fn attachments(&self) -> Vec<Attachment<A>> {
        self.rec
            .attachments
            .iter()
            .cloned()
            .map(Attachment::new)
            .collect()
    }
}

/// A `ticket` row, exposed over GraphQL to its instance's members — and, for
/// exactly the fields resolvable off `self.rec` plus [`Ticket::messages`]'s
/// filtered view, to the requester who opened it (see [`QueryRoot::ticket`]'s
/// doc comment for why a `Requester` principal can reach a `Ticket` at all,
/// given the build plan's "a Requester token can only call submitTicket,
/// never read anything" — this is the one documented exception, needed to
/// give a requester any way to see their own ticket's thread, and it never
/// weakens the internal-note rule).
#[derive(Debug, PartialEq)]
pub struct Ticket<A: App + HasDb + Send + Sync> {
    _marker: PhantomData<A>,
    rec: db::Ticket,
}

impl<A: App + HasDb + Send + Sync> Ticket<A> {
    pub fn new(rec: db::Ticket) -> Self {
        Self {
            _marker: PhantomData,
            rec,
        }
    }
}

impl<A: App + HasDb + Send + Sync> Clone for Ticket<A> {
    fn clone(&self) -> Self {
        Self {
            _marker: PhantomData,
            rec: self.rec.clone(),
        }
    }
}

#[Object]
impl<A: App + HasDb + HasStorage + Send + Sync + 'static> Ticket<A> {
    async fn id(&self) -> ID {
        ID(self.rec.id.clone())
    }
    async fn instance_id(&self) -> ID {
        ID(self.rec.instance_id.clone())
    }
    /// Dataloaded — see [`TicketMessage::author`]'s doc comment for why this
    /// matters on a list page (a page of tickets across several instances,
    /// once cross-instance views exist, would otherwise N+1).
    async fn instance(&self, ctx: &Context<'_>) -> Result<Instance<A>> {
        let loader = ctx.data_unchecked::<DataLoader<DatabaseLoader<A>>>();
        loader
            .load_one(InstanceId(ID(self.rec.instance_id.clone())))
            .await
            .map_err(|e| anyhow!("Failed to load instance via DataLoader: {}", e))?
            .map(Instance::new)
            .ok_or_else(|| anyhow!("Instance with ID {} missing", self.rec.instance_id))
    }
    async fn number(&self) -> i64 {
        self.rec.number as i64
    }
    async fn subject(&self) -> &str {
        &self.rec.subject
    }
    async fn status(&self) -> TicketStatusType {
        self.rec.status.into()
    }
    async fn requester_emails(&self) -> &[String] {
        &self.rec.requester_emails
    }
    async fn cc_emails(&self) -> &[String] {
        &self.rec.cc_emails
    }
    async fn assignee_user_id(&self) -> Option<ID> {
        self.rec.assignee_user_id.clone().map(ID)
    }
    /// Dataloaded — see [`TicketMessage::author`]'s doc comment.
    async fn assignee(&self, ctx: &Context<'_>) -> Result<Option<User<A>>> {
        let Some(uid) = &self.rec.assignee_user_id else {
            return Ok(None);
        };
        let loader = ctx.data_unchecked::<DataLoader<DatabaseLoader<A>>>();
        let rec = loader
            .load_one(UserId(ID(uid.clone())))
            .await
            .map_err(|e| anyhow!("Failed to load assignee via DataLoader: {}", e))?;
        Ok(rec.map(User::new))
    }
    async fn created_at(&self) -> i64 {
        self.rec.created_at as i64
    }
    async fn updated_at(&self) -> i64 {
        self.rec.updated_at as i64
    }
    async fn last_activity_at(&self) -> i64 {
        self.rec.last_activity_at as i64
    }

    /// Whether any message in this ticket's thread carries an attachment.
    ///
    /// Denormalised onto the ticket precisely so a list row can show a
    /// paperclip without fetching `messages { attachments }` for every row on
    /// the page — that is one extra query per ticket, on the screen agents
    /// spend their day on.
    async fn has_attachments(&self) -> bool {
        self.rec.has_attachments
    }

    /// Every message on this ticket, oldest first — the thread view's data
    /// source. **Internal notes (`kind: NOTE`) are filtered out for anyone
    /// but a member of this ticket's instance** — enforced here, not only in
    /// the web UI, per the build plan's hard requirement that a note must
    /// never be visible to a requester. A caller who is neither a member of
    /// this instance nor the requester who opened this specific ticket gets
    /// `FORBIDDEN`, not a filtered (possibly empty) list — this field must
    /// not become a way to probe whether a ticket id exists.
    async fn messages(&self, ctx: &Context<'_>) -> Result<Vec<TicketMessage<A>>> {
        let show_notes = match ctx.data_opt::<AuthInfo>() {
            Some(AuthInfo::User { memberships, .. }) => {
                if !is_member(memberships, &self.rec.instance_id) {
                    return Err(
                        ApiError::forbidden("Not a member of this ticket's instance").into(),
                    );
                }
                true
            }
            Some(AuthInfo::Requester { email, instance_id }) => {
                if instance_id != &self.rec.instance_id
                    || !self.rec.requester_emails.iter().any(|e| e == email)
                {
                    return Err(ApiError::forbidden("Not authorized to view this ticket").into());
                }
                false
            }
            // An integration token authorises `submitVerifiedTicket` alone —
            // it has no business reading a thread back, notes or otherwise.
            Some(AuthInfo::ApiToken { .. }) => {
                return Err(ApiError::forbidden("Must be authenticated").into());
            }
            None => return Err(ApiError::forbidden("Must be authenticated").into()),
        };
        let app = ctx.data_unchecked::<Arc<A>>();
        let msgs = app.db().list_ticket_messages(&self.rec.id).await?;
        Ok(msgs
            .into_iter()
            .filter(|m| show_notes || m.kind != db::TicketMessageKind::Note)
            .map(TicketMessage::new)
            .collect())
    }
}

/// A `project` row, exposed over GraphQL to its invoicing instance's
/// members. Invoicing-only — see `db::Project`'s doc comment.
#[derive(Debug, PartialEq)]
pub struct Project<A: App + HasDb + Send + Sync> {
    _marker: PhantomData<A>,
    rec: db::Project,
}

impl<A: App + HasDb + Send + Sync> Project<A> {
    pub fn new(rec: db::Project) -> Self {
        Self {
            _marker: PhantomData,
            rec,
        }
    }
}

impl<A: App + HasDb + Send + Sync> Clone for Project<A> {
    fn clone(&self) -> Self {
        Self {
            _marker: PhantomData,
            rec: self.rec.clone(),
        }
    }
}

#[Object]
impl<A: App + HasDb + Send + Sync + 'static> Project<A> {
    async fn id(&self) -> ID {
        ID(self.rec.id.clone())
    }
    /// Dataloaded — see [`TicketMessage::author`]'s doc comment.
    async fn instance(&self, ctx: &Context<'_>) -> Result<Instance<A>> {
        let loader = ctx.data_unchecked::<DataLoader<DatabaseLoader<A>>>();
        loader
            .load_one(InstanceId(ID(self.rec.instance_id.clone())))
            .await
            .map_err(|e| anyhow!("Failed to load instance via DataLoader: {}", e))?
            .map(Instance::new)
            .ok_or_else(|| anyhow!("Instance with ID {} missing", self.rec.instance_id))
    }
    async fn name(&self) -> &str {
        &self.rec.name
    }
    async fn client_name(&self) -> &str {
        &self.rec.client_name
    }
    async fn client_abn(&self) -> Option<&str> {
        self.rec.client_abn.as_deref()
    }
    async fn client_address(&self) -> Option<&str> {
        self.rec.client_address.as_deref()
    }
    async fn reference(&self) -> Option<&str> {
        self.rec.reference.as_deref()
    }
    async fn archived(&self) -> bool {
        self.rec.archived
    }
    async fn created_at(&self) -> i64 {
        self.rec.created_at as i64
    }
    async fn updated_at(&self) -> i64 {
        self.rec.updated_at as i64
    }
}

/// Where a billable item stands relative to invoicing. Every item is
/// `UNBILLED` until invoices exist (the next PR of the invoicing stack); from
/// then on, an item on a draft invoice is `DRAFT` and one on a finalized
/// invoice is `INVOICED`. Defined in full now so the web client's handling
/// doesn't have to change shape when the other two start appearing.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum BillableItemStatusType {
    Unbilled,
    Draft,
    Invoiced,
}

/// `billableItems`' filter. `BILLED` means "on any invoice, draft or
/// finalized" — `attribute_exists(invoice_id)`.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum BillableItemFilterType {
    All,
    Unbilled,
    Billed,
}

impl From<BillableItemFilterType> for db::BillableItemFilter {
    fn from(f: BillableItemFilterType) -> Self {
        match f {
            BillableItemFilterType::All => Self::All,
            BillableItemFilterType::Unbilled => Self::Unbilled,
            BillableItemFilterType::Billed => Self::Billed,
        }
    }
}

/// `createBillableItem`/`updateBillableItem`'s argument — the same full set
/// of editable fields for both (update is a full replace).
///
/// - `date`: `YYYY-MM-DD`, a real calendar day.
/// - `description`: trimmed, non-empty, ≤ 2000 chars, multi-line allowed.
/// - `quantity`: a decimal **string** with at most 2 dp, `0 < q ≤ 1,000,000`
///   (`"2"`, `"1.5"`, `"0.25"`) — a string so no client float ever gets
///   near it; the server is the one parser (`invoicing::money::parse_quantity`).
/// - `unitPriceCents`: integer cents, GST-exclusive, `0 ≤ p ≤ 1,000,000,000`.
#[derive(InputObject, Clone, Debug)]
pub struct BillableItemInput {
    pub date: String,
    pub description: String,
    pub quantity: String,
    pub unit_price_cents: i64,
}

/// A `billable_item` row, exposed over GraphQL to its invoicing instance's
/// members. See `db::BillableItem`.
#[derive(Debug, PartialEq)]
pub struct BillableItem<A: App + HasDb + Send + Sync> {
    _marker: PhantomData<A>,
    rec: db::BillableItem,
    /// Set when this item was resolved *through* an invoice that's already
    /// been loaded (currently only [`Invoice::items`] and mutations that
    /// already hold the invoice they just wrote) — `status`/`invoice` use it
    /// directly, once it's confirmed to actually be this item's invoice,
    /// instead of visiting `Loader<InvoiceId>`. That loader batches an
    /// eventually consistent `get_invoices`, so right after a same-request
    /// write it can serve a replica that hasn't caught up yet; the parent
    /// invoice here is whatever this resolution just wrote or strongly-read,
    /// so it's never stale.
    parent_invoice: Option<db::Invoice>,
}

impl<A: App + HasDb + Send + Sync> BillableItem<A> {
    pub fn new(rec: db::BillableItem) -> Self {
        Self {
            _marker: PhantomData,
            rec,
            parent_invoice: None,
        }
    }

    /// Same as [`Self::new`], but with `parent` — an invoice already known
    /// (not necessarily loaded via the dataloader) — on hand for
    /// `status`/`invoice` to use directly. Only actually used when
    /// `rec.invoice_id` matches `parent.id`; otherwise those fields fall
    /// back to the loader exactly as [`Self::new`] would.
    pub fn with_parent_invoice(rec: db::BillableItem, parent: db::Invoice) -> Self {
        Self {
            _marker: PhantomData,
            rec,
            parent_invoice: Some(parent),
        }
    }
}

impl<A: App + HasDb + Send + Sync> Clone for BillableItem<A> {
    fn clone(&self) -> Self {
        Self {
            _marker: PhantomData,
            rec: self.rec.clone(),
            parent_invoice: self.parent_invoice.clone(),
        }
    }
}

#[Object]
impl<A: App + HasDb + Send + Sync + 'static> BillableItem<A> {
    async fn id(&self) -> ID {
        ID(self.rec.id.clone())
    }
    /// Dataloaded — a page of items resolves this once per node.
    async fn project(&self, ctx: &Context<'_>) -> Result<Project<A>> {
        let loader = ctx.data_unchecked::<DataLoader<DatabaseLoader<A>>>();
        loader
            .load_one(ProjectId(ID(self.rec.project_id.clone())))
            .await
            .map_err(|e| anyhow!("Failed to load project via DataLoader: {}", e))?
            .map(Project::new)
            .ok_or_else(|| anyhow!("Project with ID {} missing", self.rec.project_id))
    }
    /// `YYYY-MM-DD`.
    async fn date(&self) -> &str {
        &self.rec.date
    }
    /// Multi-line; lines starting `* ` or `- ` render as bullets on the
    /// invoice.
    async fn description(&self) -> &str {
        &self.rec.description
    }
    /// The quantity as its shortest decimal string (`"2"`, `"1.5"`,
    /// `"0.25"`) — the same form `BillableItemInput.quantity` accepts, so an
    /// edit form can round-trip it unchanged.
    async fn quantity(&self) -> String {
        money::format_quantity(self.rec.quantity_hundredths)
    }
    /// GST-exclusive, in cents.
    async fn unit_price_cents(&self) -> i64 {
        self.rec.unit_price_cents
    }
    /// round-half-up(quantity × unit price), in cents — computed here, never
    /// stored, so it can't drift from the two values it's derived from.
    /// Can exceed 2^31 at the validation bounds (up to 10^15); it is
    /// serialized as a JSON number, which a JS client holds exactly (it's
    /// under 2^53).
    async fn amount_cents(&self) -> i64 {
        money::line_amount_cents(self.rec.quantity_hundredths, self.rec.unit_price_cents)
    }
    /// `UNBILLED` when not on an invoice; otherwise `DRAFT`/`INVOICED`
    /// according to that invoice's own status, dataloaded — a page of items
    /// resolves this once per node, same as [`Self::project`]. Falls back to
    /// `INVOICED` if the invoice row is somehow missing (data-integrity
    /// problem, not a normal outcome) — the conservative reading, since an
    /// `invoice_id` being set at all means this item is *not* plainly
    /// unbilled.
    async fn status(&self, ctx: &Context<'_>) -> Result<BillableItemStatusType> {
        let Some(invoice_id) = &self.rec.invoice_id else {
            return Ok(BillableItemStatusType::Unbilled);
        };
        if let Some(parent) = &self.parent_invoice
            && &parent.id == invoice_id
        {
            return Ok(match parent.status {
                db::InvoiceStatus::Draft => BillableItemStatusType::Draft,
                db::InvoiceStatus::Finalized => BillableItemStatusType::Invoiced,
            });
        }
        let loader = ctx.data_unchecked::<DataLoader<DatabaseLoader<A>>>();
        let invoice = loader
            .load_one(InvoiceId(ID(invoice_id.clone())))
            .await
            .map_err(|e| anyhow!("Failed to load invoice via DataLoader: {}", e))?;
        Ok(match invoice.map(|i| i.status) {
            Some(db::InvoiceStatus::Draft) => BillableItemStatusType::Draft,
            Some(db::InvoiceStatus::Finalized) | None => BillableItemStatusType::Invoiced,
        })
    }
    /// The invoice this item is on, or `null` if unbilled. Dataloaded, same
    /// batching as [`Self::status`] — except when a known parent invoice
    /// already names it, per `parent_invoice`'s doc comment.
    async fn invoice(&self, ctx: &Context<'_>) -> Result<Option<Invoice<A>>> {
        let Some(invoice_id) = &self.rec.invoice_id else {
            return Ok(None);
        };
        if let Some(parent) = &self.parent_invoice
            && &parent.id == invoice_id
        {
            return Ok(Some(Invoice::new(parent.clone())));
        }
        let loader = ctx.data_unchecked::<DataLoader<DatabaseLoader<A>>>();
        let invoice = loader
            .load_one(InvoiceId(ID(invoice_id.clone())))
            .await
            .map_err(|e| anyhow!("Failed to load invoice via DataLoader: {}", e))?;
        Ok(invoice.map(Invoice::new))
    }
    async fn created_at(&self) -> i64 {
        self.rec.created_at as i64
    }
    async fn updated_at(&self) -> i64 {
        self.rec.updated_at as i64
    }
}

/// `invoice.status`, over GraphQL.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum InvoiceStatusType {
    Draft,
    Finalized,
}

impl From<db::InvoiceStatus> for InvoiceStatusType {
    fn from(s: db::InvoiceStatus) -> Self {
        match s {
            db::InvoiceStatus::Draft => Self::Draft,
            db::InvoiceStatus::Finalized => Self::Finalized,
        }
    }
}

/// `invoices`' filter. `UNPAID`/`PAID` both imply finalized — a draft has no
/// paid status.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum InvoiceFilterType {
    All,
    Draft,
    Unpaid,
    Paid,
}

impl From<InvoiceFilterType> for db::InvoiceListFilter {
    fn from(f: InvoiceFilterType) -> Self {
        match f {
            InvoiceFilterType::All => Self::All,
            InvoiceFilterType::Draft => Self::Draft,
            InvoiceFilterType::Unpaid => Self::Unpaid,
            InvoiceFilterType::Paid => Self::Paid,
        }
    }
}

/// The printed "Invoice to" block — from the project's client fields, per
/// `invoicing::snapshot::InvoiceSnapshotBillTo`.
#[derive(SimpleObject, Clone, Debug)]
pub struct InvoiceBillToInfo {
    pub name: String,
    pub abn: Option<String>,
    pub address: Option<String>,
}

impl From<invoicing::snapshot::InvoiceSnapshotBillTo> for InvoiceBillToInfo {
    fn from(b: invoicing::snapshot::InvoiceSnapshotBillTo) -> Self {
        Self {
            name: b.name,
            abn: b.abn,
            address: b.address,
        }
    }
}

/// The printed seller block — from the instance's business settings, per
/// `invoicing::snapshot::InvoiceSnapshotSeller`.
#[derive(SimpleObject, Clone, Debug)]
pub struct InvoiceSellerInfo {
    pub name: Option<String>,
    pub abn: Option<String>,
    pub address: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
}

impl From<invoicing::snapshot::InvoiceSnapshotSeller> for InvoiceSellerInfo {
    fn from(s: invoicing::snapshot::InvoiceSnapshotSeller) -> Self {
        Self {
            name: s.name,
            abn: s.abn,
            address: s.address,
            phone: s.phone,
            email: s.email,
        }
    }
}

/// One printed line — per `invoicing::snapshot::InvoiceSnapshotLine`.
#[derive(SimpleObject, Clone, Debug)]
pub struct InvoiceLineInfo {
    pub date: String,
    pub description: String,
    pub quantity: String,
    pub unit_price_cents: i64,
    pub amount_cents: i64,
}

impl From<invoicing::snapshot::InvoiceSnapshotLine> for InvoiceLineInfo {
    fn from(l: invoicing::snapshot::InvoiceSnapshotLine) -> Self {
        Self {
            date: l.date,
            description: l.description,
            quantity: l.quantity,
            unit_price_cents: l.unit_price_cents,
            amount_cents: l.amount_cents,
        }
    }
}

/// An `invoice` row, exposed over GraphQL to its invoicing instance's
/// members. Every printable field (`title`, `billTo`, `seller`, `lines`,
/// the totals, `currency`, `gstRegistered`, `paymentDetails`) reads from the
/// frozen `snapshot` when [`db::Invoice::status`] is `Finalized`, or builds
/// one live (`invoicing::snapshot::build_snapshot`, `number: None`) when
/// it's a `Draft` — see [`Self::effective`].
#[derive(Debug, PartialEq)]
pub struct Invoice<A: App + HasDb + Send + Sync> {
    _marker: PhantomData<A>,
    rec: db::Invoice,
}

impl<A: App + HasDb + Send + Sync> Invoice<A> {
    pub fn new(rec: db::Invoice) -> Self {
        Self {
            _marker: PhantomData,
            rec,
        }
    }
}

impl<A: App + HasDb + Send + Sync> Clone for Invoice<A> {
    fn clone(&self) -> Self {
        Self {
            _marker: PhantomData,
            rec: self.rec.clone(),
        }
    }
}

impl<A: App + HasDb + Send + Sync + 'static> Invoice<A> {
    /// This invoice's printable content: the frozen snapshot for a
    /// finalized invoice (a JSON parse, no I/O), or a live
    /// `invoicing::snapshot::build_snapshot` for a draft (fetches the
    /// project, instance, and current items). Called independently by each
    /// content-bearing field below — for a draft, that means each such
    /// field re-fetches/rebuilds rather than sharing one result, which is
    /// simple and correct but not the cheapest possible query plan; fine at
    /// this feature's v1 scale (see CLAUDE.md's "Invoicing" house rule).
    async fn effective(&self, ctx: &Context<'_>) -> Result<invoicing::snapshot::InvoiceSnapshot> {
        match self.rec.status {
            db::InvoiceStatus::Finalized => {
                let json = self.rec.snapshot.as_deref().ok_or_else(|| {
                    anyhow!("Finalized invoice {} is missing its snapshot", self.rec.id)
                })?;
                serde_json::from_str(json)
                    .map_err(|e| anyhow!("Invoice {} has a corrupt snapshot: {e}", self.rec.id))
            }
            db::InvoiceStatus::Draft => {
                let app = ctx.data_unchecked::<Arc<A>>();
                let project = app
                    .db()
                    .get_projects(&[self.rec.project_id.as_str()])
                    .await?
                    .into_iter()
                    .next()
                    .flatten()
                    .ok_or_else(|| anyhow!("Project with ID {} missing", self.rec.project_id))?;
                let instance = app
                    .db()
                    .get_instances(&[self.rec.instance_id.as_str()])
                    .await?
                    .into_iter()
                    .next()
                    .flatten()
                    .ok_or_else(|| anyhow!("Instance with ID {} missing", self.rec.instance_id))?;
                let items: Vec<db::BillableItem> = if self.rec.item_ids.is_empty() {
                    vec![]
                } else {
                    app.db()
                        .get_billable_items(&self.rec.item_ids)
                        .await?
                        .into_iter()
                        .flatten()
                        .collect()
                };
                Ok(invoicing::snapshot::build_snapshot(
                    &instance, &project, &items, None, None,
                ))
            }
        }
    }
}

#[Object]
impl<A: App + HasDb + Send + Sync + 'static> Invoice<A> {
    async fn id(&self) -> ID {
        ID(self.rec.id.clone())
    }
    async fn status(&self) -> InvoiceStatusType {
        self.rec.status.into()
    }
    async fn number(&self) -> Option<i32> {
        self.rec.number.map(|n| n as i32)
    }
    /// Zero-padded to 3 digits (`"008"`); `null` for a draft. Mirrors
    /// `invoicing::snapshot::InvoiceSnapshot::display_number` exactly.
    async fn display_number(&self) -> Option<String> {
        self.rec.number.map(|n| format!("{n:03}"))
    }
    /// `YYYY-MM-DD`; `null` for a draft.
    async fn issue_date(&self) -> Option<&str> {
        self.rec.issue_date.as_deref()
    }
    /// `YYYY-MM-DD`; `null` means unpaid. Never printed on the invoice.
    async fn paid_date(&self) -> Option<&str> {
        self.rec.paid_date.as_deref()
    }
    /// Dataloaded — see `TicketMessage::author`'s doc comment.
    async fn project(&self, ctx: &Context<'_>) -> Result<Project<A>> {
        let loader = ctx.data_unchecked::<DataLoader<DatabaseLoader<A>>>();
        loader
            .load_one(ProjectId(ID(self.rec.project_id.clone())))
            .await
            .map_err(|e| anyhow!("Failed to load project via DataLoader: {}", e))?
            .map(Project::new)
            .ok_or_else(|| anyhow!("Project with ID {} missing", self.rec.project_id))
    }
    /// This invoice's current items — live rows fetched by `item_ids`, for
    /// both a draft and a finalized invoice (never from the snapshot, which
    /// carries frozen `InvoiceLine`s, not `BillableItem`s). A strongly
    /// consistent `BatchGetItem` (`get_billable_items_consistent`), not the
    /// (eventually consistent) `get_billable_items` — a mutation that just
    /// attached these items (`createInvoice`/`addInvoiceItems`) returns this
    /// field in the same response, and each item is handed this invoice as
    /// its known parent, so `status`/`invoice` resolve from it directly
    /// rather than the (also eventually consistent) dataloader path.
    async fn items(&self, ctx: &Context<'_>) -> Result<Vec<BillableItem<A>>> {
        if self.rec.item_ids.is_empty() {
            return Ok(vec![]);
        }
        let app = ctx.data_unchecked::<Arc<A>>();
        let items = app
            .db()
            .get_billable_items_consistent(&self.rec.item_ids)
            .await?;
        Ok(items
            .into_iter()
            .flatten()
            .map(|item| BillableItem::with_parent_invoice(item, self.rec.clone()))
            .collect())
    }
    /// "Tax Invoice" or "Invoice" — see [`Self::effective`].
    async fn title(&self, ctx: &Context<'_>) -> Result<String> {
        Ok(self.effective(ctx).await?.title)
    }
    async fn bill_to(&self, ctx: &Context<'_>) -> Result<InvoiceBillToInfo> {
        Ok(self.effective(ctx).await?.bill_to.into())
    }
    async fn reference(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        Ok(self.effective(ctx).await?.reference)
    }
    async fn seller(&self, ctx: &Context<'_>) -> Result<InvoiceSellerInfo> {
        Ok(self.effective(ctx).await?.seller.into())
    }
    async fn lines(&self, ctx: &Context<'_>) -> Result<Vec<InvoiceLineInfo>> {
        Ok(self
            .effective(ctx)
            .await?
            .lines
            .into_iter()
            .map(InvoiceLineInfo::from)
            .collect())
    }
    async fn subtotal_cents(&self, ctx: &Context<'_>) -> Result<i64> {
        Ok(self.effective(ctx).await?.subtotal_cents)
    }
    async fn gst_cents(&self, ctx: &Context<'_>) -> Result<i64> {
        Ok(self.effective(ctx).await?.gst_cents)
    }
    async fn total_cents(&self, ctx: &Context<'_>) -> Result<i64> {
        Ok(self.effective(ctx).await?.total_cents)
    }
    async fn currency(&self, ctx: &Context<'_>) -> Result<String> {
        Ok(self.effective(ctx).await?.currency)
    }
    async fn gst_registered(&self, ctx: &Context<'_>) -> Result<bool> {
        Ok(self.effective(ctx).await?.gst_registered)
    }
    async fn payment_details(&self, ctx: &Context<'_>) -> Result<Option<String>> {
        Ok(self.effective(ctx).await?.payment_details)
    }
    async fn created_at(&self) -> i64 {
        self.rec.created_at as i64
    }
    async fn finalized_at(&self) -> Option<i64> {
        self.rec.finalized_at.map(|t| t as i64)
    }
}

/// Default/max page size for `billableItems` — the same numbers as
/// `tickets`.
const DEFAULT_BILLABLE_ITEM_PAGE_SIZE: usize = 25;
const MAX_BILLABLE_ITEM_PAGE_SIZE: usize = 100;

/// `{date}:{id}` — see `db::BillableItemCursor`'s doc comment.
fn encode_billable_item_cursor(i: &db::BillableItem) -> String {
    format!("{}:{}", i.date, i.id)
}

fn decode_billable_item_cursor(cursor: &str) -> Result<db::BillableItemCursor> {
    let (date, id) = cursor
        .split_once(':')
        .ok_or_else(|| anyhow!("Invalid cursor"))?;
    let date = invoicing::validate_item_date(date).map_err(|_| anyhow!("Invalid cursor"))?;
    if id.is_empty() {
        return Err(anyhow!("Invalid cursor"));
    }
    Ok(db::BillableItemCursor {
        date,
        id: id.to_string(),
    })
}

/// `expense.category` — the fixed list (CLAUDE.md's "Expenses" house rule).
/// `VEHICLE_KM` is a cents-per-km trip; every other value is a purchase.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum ExpenseCategoryType {
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

impl From<ExpenseCategoryType> for db::ExpenseCategory {
    fn from(c: ExpenseCategoryType) -> Self {
        use ExpenseCategoryType as T;
        match c {
            T::Materials => Self::Materials,
            T::Subcontractors => Self::Subcontractors,
            T::ToolsEquipment => Self::ToolsEquipment,
            T::VehicleFuel => Self::VehicleFuel,
            T::VehicleKm => Self::VehicleKm,
            T::Travel => Self::Travel,
            T::MealsEntertainment => Self::MealsEntertainment,
            T::SoftwareSubscriptions => Self::SoftwareSubscriptions,
            T::PhoneInternet => Self::PhoneInternet,
            T::OfficeSupplies => Self::OfficeSupplies,
            T::ProfessionalFees => Self::ProfessionalFees,
            T::Insurance => Self::Insurance,
            T::RentUtilities => Self::RentUtilities,
            T::AdvertisingMarketing => Self::AdvertisingMarketing,
            T::BankFees => Self::BankFees,
            T::Training => Self::Training,
            T::LicencesMemberships => Self::LicencesMemberships,
            T::PostageFreight => Self::PostageFreight,
            T::Other => Self::Other,
        }
    }
}

impl From<db::ExpenseCategory> for ExpenseCategoryType {
    fn from(c: db::ExpenseCategory) -> Self {
        use db::ExpenseCategory as C;
        match c {
            C::Materials => Self::Materials,
            C::Subcontractors => Self::Subcontractors,
            C::ToolsEquipment => Self::ToolsEquipment,
            C::VehicleFuel => Self::VehicleFuel,
            C::VehicleKm => Self::VehicleKm,
            C::Travel => Self::Travel,
            C::MealsEntertainment => Self::MealsEntertainment,
            C::SoftwareSubscriptions => Self::SoftwareSubscriptions,
            C::PhoneInternet => Self::PhoneInternet,
            C::OfficeSupplies => Self::OfficeSupplies,
            C::ProfessionalFees => Self::ProfessionalFees,
            C::Insurance => Self::Insurance,
            C::RentUtilities => Self::RentUtilities,
            C::AdvertisingMarketing => Self::AdvertisingMarketing,
            C::BankFees => Self::BankFees,
            C::Training => Self::Training,
            C::LicencesMemberships => Self::LicencesMemberships,
            C::PostageFreight => Self::PostageFreight,
            C::Other => Self::Other,
        }
    }
}

/// `createExpense`/`updateExpense`'s argument (update is a full replace).
/// Which fields apply depends on `category` — see
/// `invoicing::expense::validate_expense_input`:
///
/// - A purchase (every category but `VEHICLE_KM`): `supplier` and
///   `amountCents` (integer cents, **GST-inclusive** — what was paid) are
///   required; `gstCents` (the GST included in that amount) is optional —
///   omit it for a GST-free purchase; `distanceKm` is refused.
/// - A vehicle trip (`VEHICLE_KM`): `distanceKm` (a decimal string, ≤ 1 dp,
///   `0 < d ≤ 5000`) and `description` (the trip's business purpose) are
///   required; `supplier`/`amountCents`/`gstCents` are refused — the
///   amount is distance × the ATO cents-per-km rate for the date's
///   financial year.
/// - `projectId` is optional; omit it for an expense that isn't for a
///   particular job.
#[derive(InputObject, Clone, Debug)]
pub struct ExpenseInput {
    pub project_id: Option<ID>,
    pub date: String,
    pub category: ExpenseCategoryType,
    pub description: Option<String>,
    pub supplier: Option<String>,
    pub amount_cents: Option<i64>,
    pub gst_cents: Option<i64>,
    pub distance_km: Option<String>,
}

/// An `expense` row, exposed to its invoicing instance's members. See
/// `db::Expense`.
#[derive(Debug, PartialEq)]
pub struct Expense<A: App + HasDb + Send + Sync> {
    _marker: PhantomData<A>,
    rec: db::Expense,
}

impl<A: App + HasDb + Send + Sync> Expense<A> {
    pub fn new(rec: db::Expense) -> Self {
        Self {
            _marker: PhantomData,
            rec,
        }
    }
}

impl<A: App + HasDb + Send + Sync> Clone for Expense<A> {
    fn clone(&self) -> Self {
        Self::new(self.rec.clone())
    }
}

#[Object]
impl<A: App + HasDb + Send + Sync + 'static> Expense<A> {
    async fn id(&self) -> ID {
        ID(self.rec.id.clone())
    }
    /// The project this expense is for, or `null`. Dataloaded.
    async fn project(&self, ctx: &Context<'_>) -> Result<Option<Project<A>>> {
        let Some(project_id) = &self.rec.fields.project_id else {
            return Ok(None);
        };
        let loader = ctx.data_unchecked::<DataLoader<DatabaseLoader<A>>>();
        Ok(loader
            .load_one(ProjectId(ID(project_id.clone())))
            .await
            .map_err(|e| anyhow!("Failed to load project via DataLoader: {}", e))?
            .map(Project::new))
    }
    /// `YYYY-MM-DD`.
    async fn date(&self) -> &str {
        &self.rec.fields.date
    }
    async fn category(&self) -> ExpenseCategoryType {
        self.rec.fields.category.into()
    }
    /// Optional for a purchase; a trip's business purpose for `VEHICLE_KM`.
    async fn description(&self) -> Option<&str> {
        self.rec.fields.description.as_deref()
    }
    /// `null` for a vehicle trip.
    async fn supplier(&self) -> Option<&str> {
        match &self.rec.fields.detail {
            db::ExpenseDetail::Purchase { supplier, .. } => Some(supplier),
            db::ExpenseDetail::VehicleKm { .. } => None,
        }
    }
    /// What it cost, in cents: the GST-inclusive amount paid for a purchase;
    /// round-half-up(distance × rate) for a trip, computed here, never
    /// stored.
    async fn amount_cents(&self) -> i64 {
        self.rec.amount_cents()
    }
    /// The GST included in `amountCents`; `null` when GST-free, and always
    /// for a trip.
    async fn gst_cents(&self) -> Option<i64> {
        match &self.rec.fields.detail {
            db::ExpenseDetail::Purchase { gst_cents, .. } => *gst_cents,
            db::ExpenseDetail::VehicleKm { .. } => None,
        }
    }
    /// A trip's distance, as its shortest decimal string (`"12.5"`) —
    /// the form `ExpenseInput.distanceKm` accepts. `null` for a purchase.
    async fn distance_km(&self) -> Option<String> {
        match &self.rec.fields.detail {
            db::ExpenseDetail::VehicleKm {
                distance_tenths_km, ..
            } => Some(vehicle::format_distance_km(*distance_tenths_km)),
            db::ExpenseDetail::Purchase { .. } => None,
        }
    }
    /// The ATO rate this trip was claimed at, stored when it was written.
    /// `null` for a purchase.
    async fn rate_cents_per_km(&self) -> Option<i64> {
        match &self.rec.fields.detail {
            db::ExpenseDetail::VehicleKm {
                rate_cents_per_km, ..
            } => Some(*rate_cents_per_km),
            db::ExpenseDetail::Purchase { .. } => None,
        }
    }
    /// Dataloaded — see [`TicketMessage::author`]'s doc comment.
    async fn created_by(&self, ctx: &Context<'_>) -> Result<Option<User<A>>> {
        let loader = ctx.data_unchecked::<DataLoader<DatabaseLoader<A>>>();
        let rec = loader
            .load_one(UserId(ID(self.rec.created_by_user_id.clone())))
            .await
            .map_err(|e| anyhow!("Failed to load createdBy via DataLoader: {}", e))?;
        Ok(rec.map(User::new))
    }
    async fn created_at(&self) -> i64 {
        self.rec.created_at as i64
    }
    async fn updated_at(&self) -> i64 {
        self.rec.updated_at as i64
    }
}

/// `vehicleKmSummary`: the caller's own cents-per-km trips in one financial
/// year, against the ATO's 5,000 km cap. Informational — nothing enforces
/// the cap.
#[derive(SimpleObject, Clone, Debug)]
pub struct VehicleKmSummary {
    /// The calendar year the financial year starts in (`2026` = FY
    /// 2026–27, 1 July 2026 – 30 June 2027).
    pub financial_year: i32,
    /// `"2026–27"`.
    pub financial_year_label: String,
    /// Total distance, as a decimal string (`"1240.5"`).
    pub total_km: String,
    /// The ATO cap: 5,000 km per car per financial year.
    pub cap_km: i64,
    /// This financial year's ATO rate, or `null` if the built-in table has
    /// none for it yet.
    pub rate_cents_per_km: Option<i64>,
}

/// Default/max page size for `expenses` — the same numbers as
/// `billableItems`.
const DEFAULT_EXPENSE_PAGE_SIZE: usize = 25;
const MAX_EXPENSE_PAGE_SIZE: usize = 100;

/// `{date}:{id}` — see `db::ExpenseCursor`'s doc comment.
fn encode_expense_cursor(e: &db::Expense) -> String {
    format!("{}:{}", e.fields.date, e.id)
}

fn decode_expense_cursor(cursor: &str) -> Result<db::ExpenseCursor> {
    let c = decode_billable_item_cursor(cursor)?;
    Ok(db::ExpenseCursor {
        date: c.date,
        id: c.id,
    })
}

/// Default/max page size for `invoices` — the same numbers as
/// `billableItems`/`tickets`.
const DEFAULT_INVOICE_PAGE_SIZE: usize = 25;
const MAX_INVOICE_PAGE_SIZE: usize = 100;

/// `{created_at}:{id}` — see `db::InvoiceCursor`'s doc comment.
fn encode_invoice_cursor(i: &db::Invoice) -> String {
    format!("{}:{}", i.created_at, i.id)
}

fn decode_invoice_cursor(cursor: &str) -> Result<db::InvoiceCursor> {
    let mut parts = cursor.splitn(2, ':');
    let created_at = parts
        .next()
        .ok_or_else(|| anyhow!("Invalid cursor"))?
        .parse::<u64>()
        .map_err(|_| anyhow!("Invalid cursor"))?;
    let id = parts.next().ok_or_else(|| anyhow!("Invalid cursor"))?;
    if id.is_empty() {
        return Err(anyhow!("Invalid cursor"));
    }
    Ok(db::InvoiceCursor {
        created_at,
        id: id.to_string(),
    })
}

/// Default/max page size for `tickets`, matching seslogin's periods
/// convention (small default so a queue page render stays cheap; a generous
/// but bounded max so a client can't force an unbounded scan).
const DEFAULT_TICKET_PAGE_SIZE: usize = 25;
const MAX_TICKET_PAGE_SIZE: usize = 100;

/// `{last_activity_at}:{id}` — see `db::TicketCursor`'s doc comment. Mirrors
/// seslogin's `encode_period_cursor`.
fn encode_ticket_cursor(t: &db::Ticket) -> String {
    format!("{}:{}", t.last_activity_at, t.id)
}

fn decode_ticket_cursor(cursor: &str) -> Result<db::TicketCursor> {
    let mut parts = cursor.splitn(2, ':');
    let last_activity_at = parts
        .next()
        .ok_or_else(|| anyhow!("Invalid cursor"))?
        .parse::<u64>()
        .map_err(|_| anyhow!("Invalid cursor"))?;
    let id = parts.next().ok_or_else(|| anyhow!("Invalid cursor"))?;
    if id.is_empty() {
        return Err(anyhow!("Invalid cursor"));
    }
    Ok(db::TicketCursor {
        last_activity_at,
        id: id.to_string(),
    })
}

/// A user's "connected AI app" — an authorized OAuth client. Never exposes the
/// token hashes or the (self-claimed, DCR-issued) `client_id`; the redirect
/// host is what actually identifies the client to a viewer, same reasoning as
/// `OAuthAuthorizationRequest` on the consent screen.
#[derive(Debug, Clone, PartialEq)]
pub struct OAuthGrant {
    rec: db::OAuthGrant,
}

impl OAuthGrant {
    pub(crate) fn new(rec: db::OAuthGrant) -> Self {
        Self { rec }
    }
}

#[Object]
impl OAuthGrant {
    async fn id(&self) -> ID {
        ID(self.rec.id.clone())
    }
    async fn client_name(&self) -> &str {
        &self.rec.client_name
    }
    /// Host the client's tokens redirect back to. Shown alongside the
    /// (unauthenticated, self-claimed) client name, same as on the consent
    /// screen.
    async fn redirect_host(&self) -> String {
        url::Url::parse(&self.rec.redirect_uri)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| self.rec.redirect_uri.clone())
    }
    async fn scope(&self) -> &str {
        &self.rec.scope
    }
    async fn created_at(&self) -> i64 {
        self.rec.created_at as i64
    }
    async fn last_used_at(&self) -> Option<i64> {
        self.rec.last_used_at.map(|t| t as i64)
    }
    /// When this grant's refresh token stops working if it's never used
    /// again. Sliding: it moves out on every refresh, up to the grant's
    /// absolute cap (not exposed — a grant in active use never approaches it).
    async fn refresh_expires_at(&self) -> i64 {
        self.rec.refresh_expires_at as i64
    }
}

/// What the OAuth consent screen needs to show before a user approves or denies
/// an authorization request: the client's self-claimed name, and — the one
/// value that actually matters, since dynamic client registration is
/// unauthenticated — the host it will redirect back to.
#[derive(SimpleObject, Clone, Debug)]
pub struct OAuthAuthorizationRequest {
    pub client_name: String,
    pub redirect_host: String,
}

pub struct QueryRoot<A: App + HasDb + Send + Sync> {
    _marker: PhantomData<A>,
}

impl<A: App + HasDb + Send + Sync> QueryRoot<A> {
    pub fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

impl<A: App + HasDb + Send + Sync> Default for QueryRoot<A> {
    fn default() -> Self {
        Self::new()
    }
}

#[Object]
impl<A: App + HasDb + HasStorage + Send + Sync + 'static> QueryRoot<A> {
    /// Look up an OAuth client + redirect URI before showing the consent
    /// screen. Deliberately returns nothing about the client beyond its
    /// (self-claimed, since registration is unauthenticated) name and the
    /// redirect host — the consent page shows the host prominently precisely
    /// because the name alone can't be trusted. A signed-in `User` only: a
    /// `Requester` capability token is not someone who can grant access.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn oauth_authorization_request(
        &self,
        ctx: &Context<'_>,
        client_id: String,
        redirect_uri: String,
    ) -> Result<OAuthAuthorizationRequest> {
        if !matches!(ctx.data_opt::<AuthInfo>(), Some(AuthInfo::User { .. })) {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        }
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
        let redirect_host = url::Url::parse(&redirect_uri)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .ok_or_else(|| anyhow!("redirect_uri has no host"))?;
        Ok(OAuthAuthorizationRequest {
            client_name: registration.client_name,
            redirect_host,
        })
    }

    /// API build version — the git commit this server was built from.
    async fn version(&self) -> String {
        crate::environment::GIT_REV.to_string()
    }

    /// The authenticated caller's own user record. Guarded on `Authenticated`
    /// (the only requirement Toolbox's guard enum offers that fits a "must
    /// have *some* credential" query), but only a `User` principal actually has
    /// a record to return — a `Requester` capability token hits the explicit
    /// error below rather than some confusing "not found".
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn me(&self, ctx: &Context<'_>) -> Result<User<A>> {
        let Some(AuthInfo::User { id, .. }) = ctx.data_opt::<AuthInfo>() else {
            return Err(anyhow!("Only an authenticated user has a `me` record"));
        };
        let app = ctx.data_unchecked::<Arc<A>>();
        let rec = app
            .db()
            .get_users(&[id])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| anyhow!("User with ID {id} missing"))?;
        Ok(User::new(rec))
    }

    /// Resolve a slug (from a URL: `/app/:slug`) to the full instance record.
    /// Requires *some* authenticated user (checked here, not via a static
    /// `#[graphql(guard)]`, since the guard's `Member(instance_id)` requirement
    /// needs an instance id this resolver only learns after resolving the
    /// slug) and, once resolved, that the caller is actually a member of that
    /// instance. A slug that doesn't resolve, or that resolves to an instance
    /// the caller isn't a member of, is reported identically — `None` — so a
    /// non-member can't use this to probe which slugs exist.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn instance(&self, ctx: &Context<'_>, slug: String) -> Result<Option<Instance<A>>> {
        let Some(AuthInfo::User { memberships, .. }) = ctx.data_opt::<AuthInfo>() else {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        };
        let app = ctx.data_unchecked::<Arc<A>>();
        let Some(instance_id) = app.db().get_instance_id_by_slug(&slug).await? else {
            return Ok(None);
        };
        if !memberships.iter().any(|m| m.instance_id == instance_id) {
            return Ok(None);
        }
        let Some(rec) = app
            .db()
            .get_instances(&[&instance_id])
            .await?
            .into_iter()
            .next()
            .flatten()
            .filter(|i| !i.deleted)
        else {
            // A deleted instance is reported identically to "no such
            // instance"/"not a member" — `Ok(None)`, never an error — for
            // the same no-probing reason as the membership check above. A
            // former member's slug-resolve for an instance an operator just
            // soft-deleted should look exactly like it stopped existing.
            return Ok(None);
        };
        Ok(Some(Instance::new(rec)))
    }

    /// Every instance with public ticket submission enabled — the bare
    /// `/submit` list. Unauthenticated by design (this is how an anonymous
    /// visitor finds which organisations accept public tickets), and
    /// deliberately exposes only `name`/`slug`: see [`PublicInstance`]'s doc
    /// comment for why nothing else about a tenant may leak here.
    async fn public_instances(&self, ctx: &Context<'_>) -> Result<Vec<PublicInstance>> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let instances = app.db().list_instances().await?;
        Ok(instances
            .into_iter()
            .filter(|i| {
                i.public_submission_enabled && !i.deleted && i.kind == db::InstanceKind::Support
            })
            .map(PublicInstance::from)
            .collect())
    }

    /// One of the four ticket listing views, as a Relay connection ordered
    /// newest-activity-first. `status` defaults to `OPEN`; `DELETED` is
    /// owner-only (the explicit way to reach a soft-deleted ticket) —
    /// checked here, not by a static guard, since it depends on the `status`
    /// argument's value. `assignedTo`, when given, takes priority over
    /// `status`'s usual GSI choice and queries `instance_assignee` instead,
    /// narrowed by `status` via a `FilterExpression` when it isn't `ALL` (see
    /// `db::TicketListFilter`'s doc comment).
    ///
    /// **No `search` argument.** The build plan's schema sketch shows one,
    /// but DynamoDB cannot do a text search without a table scan, and a
    /// bounded post-filter (fetch a page, then filter in memory) would look
    /// like search while silently missing matches outside that page — worse
    /// than not having it. Left out of the schema; see `SCHEMA.md` for the
    /// same note next to the `ticket` table.
    #[allow(clippy::too_many_arguments)]
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Member(instance_id.to_string()))")]
    async fn tickets(
        &self,
        ctx: &Context<'_>,
        instance_id: ID,
        #[graphql(default_with = "TicketStatusFilterType::Open")] status: TicketStatusFilterType,
        assigned_to: Option<ID>,
        after: Option<String>,
        before: Option<String>,
        first: Option<i32>,
        last: Option<i32>,
    ) -> Result<Connection<String, Ticket<A>, EmptyFields, EmptyFields>> {
        let Some(AuthInfo::User { memberships, .. }) = ctx.data_opt::<AuthInfo>() else {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        };
        if status == TicketStatusFilterType::Deleted && !is_owner(memberships, instance_id.as_str())
        {
            return Err(ApiError::forbidden("Only an owner can list deleted tickets").into());
        }

        let after_cursor = after.as_deref().map(decode_ticket_cursor).transpose()?;
        let before_cursor = before.as_deref().map(decode_ticket_cursor).transpose()?;
        let has_after = after_cursor.is_some();
        let has_before = before_cursor.is_some();
        let (page_size, is_last_mode) =
            pagination_args(first, last, DEFAULT_TICKET_PAGE_SIZE, MAX_TICKET_PAGE_SIZE)?;
        let fetch_limit = i32::try_from(page_size.saturating_add(1))
            .map_err(|_| anyhow!("Requested page is too large"))?;

        let filter = match &assigned_to {
            Some(uid) => db::TicketListFilter::AssignedTo {
                user_id: uid.to_string(),
                status: match status {
                    TicketStatusFilterType::Open => Some(db::TicketStatus::Open),
                    TicketStatusFilterType::Closed => Some(db::TicketStatus::Closed),
                    TicketStatusFilterType::All => None,
                    TicketStatusFilterType::Deleted => Some(db::TicketStatus::Deleted),
                },
            },
            None => match status {
                TicketStatusFilterType::Open => {
                    db::TicketListFilter::Status(db::TicketStatus::Open)
                }
                TicketStatusFilterType::Closed => {
                    db::TicketListFilter::Status(db::TicketStatus::Closed)
                }
                TicketStatusFilterType::All => db::TicketListFilter::Visible,
                TicketStatusFilterType::Deleted => {
                    db::TicketListFilter::Status(db::TicketStatus::Deleted)
                }
            },
        };

        let app = ctx.data_unchecked::<Arc<A>>();
        let items = app
            .db()
            .list_tickets(
                instance_id.as_str(),
                filter,
                db::ListTicketsPage {
                    after: after_cursor,
                    before: before_cursor,
                    limit: fetch_limit,
                    descending: !is_last_mode,
                },
            )
            .await?;

        Ok(build_connection(
            items,
            page_size,
            is_last_mode,
            has_after,
            has_before,
            |t| (encode_ticket_cursor(t), Ticket::new(t.clone())),
        ))
    }

    /// Fetch a single ticket by id. Reachable by a member of its instance, or
    /// by the `Requester` who opened it (matching `instance_id` *and*
    /// `requester_emails`) — see [`Ticket`]'s doc comment for why a
    /// `Requester` can reach this at all. Anyone else — including a member of
    /// a *different* instance, or a requester who isn't on this ticket —
    /// gets `null`, identically to a nonexistent id, so this can't be used to
    /// probe which ticket ids exist in another instance.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn ticket(&self, ctx: &Context<'_>, id: ID) -> Result<Option<Ticket<A>>> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let Some(rec) = app
            .db()
            .get_tickets(&[id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
        else {
            return Ok(None);
        };
        let visible = match ctx.data_opt::<AuthInfo>() {
            Some(AuthInfo::User { memberships, .. }) => is_member(memberships, &rec.instance_id),
            Some(AuthInfo::Requester { email, instance_id }) => {
                instance_id == &rec.instance_id && rec.requester_emails.iter().any(|e| e == email)
            }
            // Never visible to an integration token — see `messages`'s
            // matching arm above.
            Some(AuthInfo::ApiToken { .. }) => false,
            None => false,
        };
        if !visible {
            return Ok(None);
        }
        Ok(Some(Ticket::new(rec)))
    }

    /// Every project for an invoicing instance, sorted by name
    /// case-insensitively — the projects list page's data source.
    /// Unpaginated, like `Instance.inboundAddresses` (see
    /// `db::Handler::list_projects_by_instance`'s doc comment for why).
    /// Rejects a support instance with a plain validation error — the caller
    /// is a real member of a real instance, just the wrong kind, unlike
    /// `addInboundAddress`'s "treat as not found" posture for the opposite
    /// direction.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Member(instance_id.to_string()))")]
    async fn projects(
        &self,
        ctx: &Context<'_>,
        instance_id: ID,
        #[graphql(default = false)] include_archived: bool,
    ) -> Result<Vec<Project<A>>> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let instance = app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;
        db::require_instance_kind(&instance, db::InstanceKind::Invoicing)
            .map_err(|e| anyhow!(e))?;
        let mut projects = app
            .db()
            .list_projects_by_instance(instance_id.as_str())
            .await?;
        if !include_archived {
            projects.retain(|p| !p.archived);
        }
        projects.sort_by_key(|p| p.name.to_lowercase());
        Ok(projects.into_iter().map(Project::new).collect())
    }

    /// Fetch a single project by id. Reachable only by a member of its
    /// (invoicing) instance — a project that doesn't exist, belongs to an
    /// instance the caller isn't a member of, or belongs to a support
    /// instance, is reported identically — `null` — mirroring
    /// [`Self::ticket`]'s no-probing posture.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn project(&self, ctx: &Context<'_>, id: ID) -> Result<Option<Project<A>>> {
        let Some(AuthInfo::User { memberships, .. }) = ctx.data_opt::<AuthInfo>() else {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        };
        let app = ctx.data_unchecked::<Arc<A>>();
        let Some(rec) = app
            .db()
            .get_projects(&[id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
        else {
            return Ok(None);
        };
        if !is_member(memberships, &rec.instance_id) {
            return Ok(None);
        }
        let is_invoicing = app
            .db()
            .get_instances(&[rec.instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .is_some_and(|i| i.kind == db::InstanceKind::Invoicing);
        if !is_invoicing {
            return Ok(None);
        }
        Ok(Some(Project::new(rec)))
    }

    /// Billable items in an invoicing instance — every project's, or one
    /// project's when `projectId` is given — as a Relay connection, newest
    /// `date` first (see `db::Handler::list_billable_items`). Forward-only
    /// (`first`/`after`). Member of `instanceId`; a support instance is
    /// rejected with a plain validation error (like `projects`), and a
    /// `projectId` that doesn't exist or belongs to another instance is
    /// `NOT_FOUND` either way. Superusers get nothing — they don't pass
    /// `Member`.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Member(instance_id.to_string()))")]
    async fn billable_items(
        &self,
        ctx: &Context<'_>,
        instance_id: ID,
        project_id: Option<ID>,
        #[graphql(default_with = "BillableItemFilterType::All")] filter: BillableItemFilterType,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<Connection<String, BillableItem<A>, EmptyFields, EmptyFields>> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let instance = app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;
        db::require_instance_kind(&instance, db::InstanceKind::Invoicing)
            .map_err(|e| anyhow!(e))?;
        if let Some(project_id) = &project_id {
            app.db()
                .get_projects(&[project_id.as_str()])
                .await?
                .into_iter()
                .next()
                .flatten()
                .filter(|p| p.instance_id == instance.id)
                .ok_or_else(|| ApiError::not_found("Project", project_id.as_str()))?;
        }

        let after_cursor = after
            .as_deref()
            .map(decode_billable_item_cursor)
            .transpose()?;
        let has_after = after_cursor.is_some();
        let (page_size, _) = pagination_args(
            first,
            None,
            DEFAULT_BILLABLE_ITEM_PAGE_SIZE,
            MAX_BILLABLE_ITEM_PAGE_SIZE,
        )?;
        let fetch_limit = i32::try_from(page_size.saturating_add(1))
            .map_err(|_| anyhow!("Requested page is too large"))?;
        let scope = match &project_id {
            Some(p) => db::BillableItemScope::Project(p.as_str()),
            None => db::BillableItemScope::Instance(instance_id.as_str()),
        };
        let items = app
            .db()
            .list_billable_items(
                scope,
                filter.into(),
                db::ListBillableItemsPage {
                    after: after_cursor,
                    limit: fetch_limit,
                },
            )
            .await?;

        Ok(build_connection(
            items,
            page_size,
            false,
            has_after,
            false,
            |i| (encode_billable_item_cursor(i), BillableItem::new(i.clone())),
        ))
    }

    /// Expenses in an invoicing instance — all of them, or one project's
    /// when `projectId` is given — as a Relay connection, newest `date`
    /// first. Same posture as [`Self::billable_items`]: member of
    /// `instanceId`, support instance rejected with a plain validation
    /// error, `projectId` from another instance is `NOT_FOUND`, superusers
    /// get nothing. `category` narrows to one category.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Member(instance_id.to_string()))")]
    async fn expenses(
        &self,
        ctx: &Context<'_>,
        instance_id: ID,
        project_id: Option<ID>,
        category: Option<ExpenseCategoryType>,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<Connection<String, Expense<A>, EmptyFields, EmptyFields>> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let instance = app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;
        db::require_instance_kind(&instance, db::InstanceKind::Invoicing)
            .map_err(|e| anyhow!(e))?;
        if let Some(project_id) = &project_id {
            app.db()
                .get_projects(&[project_id.as_str()])
                .await?
                .into_iter()
                .next()
                .flatten()
                .filter(|p| p.instance_id == instance.id)
                .ok_or_else(|| ApiError::not_found("Project", project_id.as_str()))?;
        }

        let after_cursor = after.as_deref().map(decode_expense_cursor).transpose()?;
        let has_after = after_cursor.is_some();
        let (page_size, _) = pagination_args(
            first,
            None,
            DEFAULT_EXPENSE_PAGE_SIZE,
            MAX_EXPENSE_PAGE_SIZE,
        )?;
        let fetch_limit = i32::try_from(page_size.saturating_add(1))
            .map_err(|_| anyhow!("Requested page is too large"))?;
        let scope = match &project_id {
            Some(p) => db::ExpenseScope::Project(p.as_str()),
            None => db::ExpenseScope::Instance(instance_id.as_str()),
        };
        let expenses = app
            .db()
            .list_expenses(
                scope,
                category.map(Into::into),
                db::ListExpensesPage {
                    after: after_cursor,
                    limit: fetch_limit,
                },
            )
            .await?;

        Ok(build_connection(
            expenses,
            page_size,
            false,
            has_after,
            false,
            |e| (encode_expense_cursor(e), Expense::new(e.clone())),
        ))
    }

    /// The caller's own cents-per-km trips in `instanceId` for one
    /// financial year (`financialYear` is the year it starts in; defaults to
    /// the current one, by UTC date) against the ATO's 5,000 km cap.
    /// Self-only by construction — it only ever sums the caller's trips,
    /// since the cap applies per person. Member; support instance rejected
    /// like `expenses`.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Member(instance_id.to_string()))")]
    async fn vehicle_km_summary(
        &self,
        ctx: &Context<'_>,
        instance_id: ID,
        financial_year: Option<i32>,
    ) -> Result<VehicleKmSummary> {
        let Some(AuthInfo::User { id: user_id, .. }) = ctx.data_opt::<AuthInfo>() else {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        };
        let app = ctx.data_unchecked::<Arc<A>>();
        let instance = app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;
        db::require_instance_kind(&instance, db::InstanceKind::Invoicing)
            .map_err(|e| anyhow!(e))?;
        let financial_year = match financial_year {
            Some(y) if (2000..=9998).contains(&y) => y,
            Some(_) => return Err(anyhow!("financialYear is out of range")),
            None => {
                let today = chrono::DateTime::from_timestamp(crate::clock::now_sec() as i64, 0)
                    .ok_or_else(|| anyhow!("System clock is out of range"))?
                    .date_naive();
                vehicle::financial_year_of(today)
            }
        };
        let (from, to) = vehicle::financial_year_bounds(financial_year);
        let tenths = app
            .db()
            .sum_vehicle_km_tenths(&instance.id, user_id, &from, &to)
            .await?;
        Ok(VehicleKmSummary {
            financial_year,
            financial_year_label: vehicle::format_financial_year(financial_year),
            total_km: vehicle::format_distance_km(tenths),
            cap_km: vehicle::ANNUAL_CAP_KM,
            rate_cents_per_km: vehicle::rate_for_financial_year(financial_year).ok(),
        })
    }

    /// Invoices in an invoicing instance — every project's, or one
    /// project's when `projectId` is given — as a Relay connection, newest
    /// `createdAt` first (see `db::Handler::list_invoices`). Same posture as
    /// [`Self::billable_items`]: member of `instanceId`, support instance
    /// rejected with a plain validation error, `projectId` from another
    /// instance is `NOT_FOUND`, superusers get nothing.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Member(instance_id.to_string()))")]
    async fn invoices(
        &self,
        ctx: &Context<'_>,
        instance_id: ID,
        project_id: Option<ID>,
        #[graphql(default_with = "InvoiceFilterType::All")] filter: InvoiceFilterType,
        first: Option<i32>,
        after: Option<String>,
    ) -> Result<Connection<String, Invoice<A>, EmptyFields, EmptyFields>> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let instance = app
            .db()
            .get_instances(&[instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| ApiError::not_found("Instance", instance_id.as_str()))?;
        db::require_instance_kind(&instance, db::InstanceKind::Invoicing)
            .map_err(|e| anyhow!(e))?;
        if let Some(project_id) = &project_id {
            app.db()
                .get_projects(&[project_id.as_str()])
                .await?
                .into_iter()
                .next()
                .flatten()
                .filter(|p| p.instance_id == instance.id)
                .ok_or_else(|| ApiError::not_found("Project", project_id.as_str()))?;
        }

        let after_cursor = after.as_deref().map(decode_invoice_cursor).transpose()?;
        let has_after = after_cursor.is_some();
        let (page_size, _) = pagination_args(
            first,
            None,
            DEFAULT_INVOICE_PAGE_SIZE,
            MAX_INVOICE_PAGE_SIZE,
        )?;
        let fetch_limit = i32::try_from(page_size.saturating_add(1))
            .map_err(|_| anyhow!("Requested page is too large"))?;
        let scope = match &project_id {
            Some(p) => db::InvoiceScope::Project(p.as_str()),
            None => db::InvoiceScope::Instance(instance_id.as_str()),
        };
        let invoices = app
            .db()
            .list_invoices(
                scope,
                filter.into(),
                db::ListInvoicesPage {
                    after: after_cursor,
                    limit: fetch_limit,
                },
            )
            .await?;

        Ok(build_connection(
            invoices,
            page_size,
            false,
            has_after,
            false,
            |i| (encode_invoice_cursor(i), Invoice::new(i.clone())),
        ))
    }

    /// Fetch a single invoice by id. Reachable only by a member of its
    /// (invoicing) instance — missing, wrong instance, and belonging to a
    /// support instance are all reported identically — `null` — mirroring
    /// [`Self::project`]'s no-probing posture. A strongly consistent
    /// `GetItem` (`get_invoice_consistent`), not the dataloader path: the web
    /// app navigates straight here right after `createInvoice`/
    /// `finalizeInvoice`, so an eventually consistent read could still show
    /// "not found" for an invoice this same request just created.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Authenticated)")]
    async fn invoice(&self, ctx: &Context<'_>, id: ID) -> Result<Option<Invoice<A>>> {
        let Some(AuthInfo::User { memberships, .. }) = ctx.data_opt::<AuthInfo>() else {
            return Err(ApiError::forbidden("Must be authenticated as a user").into());
        };
        let app = ctx.data_unchecked::<Arc<A>>();
        let Some(rec) = app.db().get_invoice_consistent(id.as_str()).await? else {
            return Ok(None);
        };
        if !is_member(memberships, &rec.instance_id) {
            return Ok(None);
        }
        let is_invoicing = app
            .db()
            .get_instances(&[rec.instance_id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .is_some_and(|i| i.kind == db::InstanceKind::Invoicing);
        if !is_invoicing {
            return Ok(None);
        }
        Ok(Some(Invoice::new(rec)))
    }

    // ── Admin (superuser-only) ──────────────────────────────────────────────

    /// Every instance, **including soft-deleted ones**, sorted by name — the
    /// admin instance list. Superuser-only; unlike every other way an
    /// `Instance` is reached, this deliberately does not filter `deleted`
    /// out, since restoring a deleted instance is exactly what this list is
    /// for. A superuser reaching an instance this way still gets no ticket
    /// access to it — see `CLAUDE.md`'s superuser boundary house rule; this
    /// query and `adminInstance` expose settings-shaped data only
    /// (`Instance`'s own fields, `inboundAddresses`, `members`), never a
    /// `tickets`/`ticket` path, because `Instance` has none.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
    async fn admin_instances(&self, ctx: &Context<'_>) -> Result<Vec<Instance<A>>> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let mut instances = app.db().list_instances().await?;
        instances.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(instances.into_iter().map(Instance::new).collect())
    }

    /// One instance by id, including a soft-deleted one; `null` if no
    /// instance has that id at all. Superuser-only — see
    /// [`Self::admin_instances`]'s doc comment.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
    async fn admin_instance(&self, ctx: &Context<'_>, id: ID) -> Result<Option<Instance<A>>> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let rec = app
            .db()
            .get_instances(&[id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten();
        Ok(rec.map(Instance::new))
    }

    /// Every user, sorted by email — the admin user list. Superuser-only.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
    async fn admin_users(&self, ctx: &Context<'_>) -> Result<Vec<User<A>>> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let mut users = app.db().list_users().await?;
        users.sort_by(|a, b| a.email.cmp(&b.email));
        Ok(users.into_iter().map(User::new).collect())
    }

    /// One user by id; `null` if no user has that id. Superuser-only. The
    /// returned `User`'s `memberships` field is how the admin user page
    /// shows which instances this user belongs to and in what role — see
    /// [`User::memberships`]'s doc comment for why that field, not a
    /// separate superuser-only one, is reused here. `passkeys` stays
    /// self-only even when reached this way — see that field's doc comment.
    #[graphql(guard = "AuthGuard::new(AuthRequirement::Superuser)")]
    async fn admin_user(&self, ctx: &Context<'_>, id: ID) -> Result<Option<User<A>>> {
        let app = ctx.data_unchecked::<Arc<A>>();
        let rec = app
            .db()
            .get_users(&[id.as_str()])
            .await?
            .into_iter()
            .next()
            .flatten();
        Ok(rec.map(User::new))
    }
}

#[cfg(test)]
mod ticket_cursor_tests {
    use super::*;

    #[test]
    fn ticket_cursor_round_trips() {
        let t = db::Ticket {
            id: "tick0000001".into(),
            instance_id: "inst1".into(),
            number: 7,
            subject: "Help".into(),
            status: db::TicketStatus::Open,
            requester_emails: vec!["a@example.com".into()],
            cc_emails: vec![],
            assignee_user_id: None,
            reply_token: "0123456789abcdef".into(),
            created_at: 1_000,
            updated_at: 1_000,
            last_activity_at: 1_234_567,
            has_attachments: false,
        };
        let cursor = encode_ticket_cursor(&t);
        assert_eq!(cursor, "1234567:tick0000001");
        let decoded = decode_ticket_cursor(&cursor).unwrap();
        assert_eq!(decoded.last_activity_at, 1_234_567);
        assert_eq!(decoded.id, "tick0000001");
    }

    #[test]
    fn decode_ticket_cursor_rejects_missing_separator() {
        assert!(decode_ticket_cursor("nosep").is_err());
    }

    #[test]
    fn decode_ticket_cursor_rejects_non_numeric_timestamp() {
        assert!(decode_ticket_cursor("notanumber:tick1").is_err());
    }

    #[test]
    fn decode_ticket_cursor_rejects_empty_id() {
        assert!(decode_ticket_cursor("1234567:").is_err());
    }

    #[test]
    fn decode_ticket_cursor_rejects_empty_string() {
        assert!(decode_ticket_cursor("").is_err());
    }

    #[test]
    fn decode_ticket_cursor_accepts_an_id_containing_a_colon() {
        // splitn(2, ':') means only the first colon splits — an id (unlikely
        // in practice, since ids are nanoids, but not schema-forbidden)
        // containing a colon must not truncate.
        let decoded = decode_ticket_cursor("42:abc:def").unwrap();
        assert_eq!(decoded.last_activity_at, 42);
        assert_eq!(decoded.id, "abc:def");
    }
}
