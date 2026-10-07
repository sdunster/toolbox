//! Authentication principals and the `Authorization` header dispatcher.
//!
//! Ported from seslogin's `auth.rs`, minus the kiosk/session principal
//! Toolbox has no equivalent of. There are three principals: an
//! authenticated [`AuthInfo::User`] (a member, possibly of several instances),
//! an anonymous [`AuthInfo::Requester`] holding a short-lived, single-purpose
//! capability token scoped to one instance (the public submit form's flow — the
//! same pattern as seslogin's `period_link.rs`), and [`AuthInfo::ApiToken`], a
//! long-lived instance-scoped integration credential that authorises exactly
//! `submitVerifiedTicket` and nothing else — see that variant's doc comment.

use thiserror::Error;
use tracing::warn;

use crate::app::{App, HasDb};
use crate::db;
use crate::db::Handler as _;

#[derive(Debug, Error)]
pub enum AuthError {
    /// Token is definitively invalid — bad token, expired, record not found, etc.
    /// Surfaced as 401.
    #[error("{0}")]
    Permanent(String),
    /// Infrastructure failure during verification — DB down, network error, etc.
    /// Surfaced as 503, since retrying may succeed.
    #[error("{0}")]
    Transient(String),
}

/// Classify a `db::Error` encountered while verifying a token: a definitive
/// "doesn't exist" is a bad credential (401); anything else might succeed on
/// retry (503), so it must not be treated the same as an invalid token.
pub(crate) fn classify_db_err(msg: &str, e: db::Error) -> AuthError {
    match e {
        db::Error::NotFound(_) => AuthError::Permanent(format!("{msg}: {e:#}")),
        _ => AuthError::Transient(format!("{msg}: {e:#}")),
    }
}

/// One membership of an [`AuthInfo::User`] in an instance.
///
/// A thin, auth-time-only shape — not the `membership` table's row type, which
/// step 4 defines in `db.rs` alongside the rest of the instance/membership domain
/// and which this will eventually be built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    pub instance_id: String,
    pub is_owner: bool,
}

#[derive(Clone)]
pub enum AuthInfo {
    /// An authenticated member, holding every instance they belong to (so a guard
    /// can check `Member(instance)`/`InstanceOwner(instance)` without a DB round
    /// trip per field).
    User {
        id: String,
        /// Always empty until step 4 adds the `membership` table and a real
        /// lookup (`get_memberships_by_user` or similar) here. Left as a real
        /// field now — rather than invented later — so every guard and resolver
        /// that reads it is already written against its final shape; an empty
        /// vec just means `Member`/`InstanceOwner` guards correctly reject
        /// everyone, since there is nothing to be a member of yet.
        memberships: Vec<Membership>,
        /// Mirrors `db::User::superuser`, populated in
        /// [`fetch_update_user_auth_info`] from the same row fetch that builds
        /// `memberships` (no extra round trip). Grants the `Superuser`/
        /// `InstanceOwnerOrSuperuser` guards and **nothing else** — see
        /// `db::User::superuser`'s doc comment and `CLAUDE.md`'s superuser
        /// boundary house rule: a superuser must never pass `Member`/
        /// `InstanceOwner`.
        is_superuser: bool,
        /// Set only when authenticated via an opaque `mtu_` token (always true
        /// today — there is no other way to authenticate as a `User` yet), so
        /// `logout` can revoke exactly the token that was presented.
        token_id: Option<String>,
        /// Set only when authenticated via an OAuth access token (`mtoa_`);
        /// identifies which grant so resolvers/telemetry can tell OAuth callers
        /// apart. `None` everywhere else.
        grant_id: Option<String>,
    },
    /// The public submit form's short-lived capability token: authorises exactly
    /// `submitTicket` for one `(email, instance_id)` pair, and nothing else.
    /// Unreachable until step 4.
    Requester { email: String, instance_id: String },
    /// An instance-scoped integration token (`mta_`): authorises exactly
    /// `submitVerifiedTicket` for `instance_id`, and nothing else — never a
    /// `User`, never a `Requester`, never passes any other
    /// [`crate::graphql::auth::AuthRequirement`] guard (including
    /// `Authenticated` — see that guard's doc comment for why widening it to
    /// admit this variant would be a mistake). Minted only by an instance
    /// owner or superuser via `issue_api_token`; long-lived (no expiry — see
    /// that function's doc comment), so revocation is disabling or deleting
    /// the row, not waiting it out.
    ApiToken {
        token_id: String,
        instance_id: String,
    },
}

/// Prefix of an opaque user session token, stored in `user_token`. Format:
/// `mtu_{id}.{secret}` — the same id-in-token shape as [`API_TOKEN_PREFIX`],
/// so verification is a strongly consistent `GetItem` on `id` (no `token_hash`
/// GSI, no eventual-consistency window for a just-issued token) followed by a
/// constant-time compare of the full token's sha256 against the stored
/// `token_hash`. No JWTs in this project — every credential is an opaque secret
/// whose hash is checked in DynamoDB, per `CLAUDE.md`/the build plan.
pub const USER_TOKEN_PREFIX: &str = "mtu_";

/// Prefix of the public submit form's opaque capability token, sha256-hashed
/// and stored in `ephemeral_state` under `kind: "submit_token"`. Deliberately
/// a different prefix from [`USER_TOKEN_PREFIX`] so [`verify_token`]'s
/// dispatch can never confuse the two, however either is stored.
pub const REQUESTER_TOKEN_PREFIX: &str = "mts_";

/// Prefix of an instance-scoped integration token, stored in the durable
/// `api_token` table. Format: `mta_{id}.{secret}` — see [`issue_api_token`]'s
/// doc comment for why the row's own id is embedded in the token rather than
/// looked up via a `token_hash` GSI (the same reasoning, and the same shape,
/// as the `+t{ticket_id}.{reply_token}` reply tag — see `CLAUDE.md`).
/// Deliberately a fourth-letter suffix distinct from [`USER_TOKEN_PREFIX`]
/// (`mtu_`) and [`REQUESTER_TOKEN_PREFIX`] (`mts_`), so [`verify_token`]'s
/// dispatch never has to guess which scheme a bearer token belongs to.
pub const API_TOKEN_PREFIX: &str = "mta_";

/// `ephemeral_state` `kind` for the requester submit-token flow's short-lived
/// capability token (post-code-verification). Distinct from
/// [`SUBMIT_CODE_STATE_KIND`] — the token and the code that unlocks it are two
/// different secrets with two different lifetimes, stored under two different
/// kinds.
const SUBMIT_TOKEN_STATE_KIND: &str = "submit_token";

/// `ephemeral_state` `kind` for the public submit form's 6-digit
/// email-verification code.
///
/// **Critically not `login_code`.** If the submit-code flow reused the
/// `login_code` table (or otherwise shared storage with the user login-code
/// flow), a code written by `requestSubmitCode` could be presented to
/// `verifyAuthCode` — or a code written by `requestAuthCode` presented to
/// `verifySubmitCode` — and, for an email that happens to resolve on both
/// sides, cross-authenticate into the wrong flow's outcome (a full user
/// session from a public, unauthenticated submit-code request). Storing
/// submit codes in `ephemeral_state` under this `kind`, and login codes in the
/// dedicated `login_code` table, makes that structurally impossible: the two
/// lookups touch different tables. See
/// `tests/submit_code_dynamodb_local.rs` for the regression tests covering
/// both directions.
pub const SUBMIT_CODE_STATE_KIND: &str = "submit_code";

/// Deterministic `ephemeral_state` id for a submit code, scoped to
/// `(instance_id, email)` — not just `email` the way `login_code` is keyed,
/// because a requester may hold an outstanding code for more than one public
/// instance at once. A second request for the same `(instance_id, email)`
/// pair reuses (overwrites) this same slot, which is what makes the 30s
/// resend rate limit a single-row read, mirroring `login_code`.
pub fn submit_code_state_id(instance_id: &str, email: &str) -> String {
    format!(
        "{SUBMIT_CODE_STATE_KIND}_{}",
        hash_token(&format!("{instance_id}:{email}"))
    )
}

/// JSON payload of a `kind: "submit_code"` `ephemeral_state` row. Mirrors
/// `login_code`'s columns (`code_hash`/`attempts`/`last_sent_at`), folded into
/// one opaque payload since `ephemeral_state` has no schema of its own.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SubmitCodePayload {
    pub code_hash: String,
    pub attempts: u64,
    pub last_sent_at: u64,
}

/// JSON payload of a `kind: "submit_token"` `ephemeral_state` row: the two
/// pieces of authority the requester submit token carries and nothing else —
/// it cannot read or touch any ticket other than one it creates for this
/// exact `(email, instance_id)` pair.
#[derive(serde::Serialize, serde::Deserialize)]
struct SubmitTokenPayload {
    email: String,
    instance_id: String,
}

fn submit_token_state_id(token_hash: &str) -> String {
    format!("{SUBMIT_TOKEN_STATE_KIND}_{token_hash}")
}

/// Issue a fresh opaque `mts_` capability token scoped to exactly
/// `(email, instance_id)`. Only its sha256 hash is stored; the secret returned
/// here is the only time it exists in full. Mirrors [`issue_user_token`]'s
/// shape, but stored under `ephemeral_state`/`submit_token` rather than
/// `user_token` — this token authorises `submitTicket` alone, never a `me`
/// query or anything else a real user session can do.
pub async fn issue_submit_token<A: App + HasDb>(
    app: &A,
    email: &str,
    instance_id: &str,
) -> anyhow::Result<String> {
    let secret = format!(
        "{}{}",
        REQUESTER_TOKEN_PREFIX,
        crate::nonce::generate_nonce(16)
    );
    let hash = hash_token(&secret);
    let expires_at = crate::expire::ExpirePolicy::RequesterSubmitToken.from_now();
    let payload = serde_json::to_string(&SubmitTokenPayload {
        email: email.to_string(),
        instance_id: instance_id.to_string(),
    })?;
    app.db()
        .put_ephemeral_state(
            &submit_token_state_id(&hash),
            SUBMIT_TOKEN_STATE_KIND,
            &payload,
            expires_at,
        )
        .await?;
    Ok(secret)
}

/// Dev-only auth override configured via a CLI flag on the poem server. When set,
/// the server bypasses token verification entirely and treats every request as
/// the configured caller. Intended for local UI testing/screenshots only — never
/// enable in a deployed environment. Absent from the Lambda binary: `bin/lambda`
/// never constructs a `DevAuthConfig` in the first place, since it has no CLI to
/// read a flag from.
///
/// Only a `User` variant exists (unlike seslogin's session/user split) — there is
/// no kiosk-equivalent principal to impersonate.
pub enum DevAuthConfig {
    User { id_or_email: String },
}

/// Resolve a [`DevAuthConfig`] into an [`AuthInfo`] without any token check.
///
/// Impersonation keeps the impersonated caller's *real* permissions — it resolves
/// the same [`AuthInfo::User`] a normal login would produce (memberships
/// included, once step 4 populates them), rather than granting some synthetic
/// elevated principal. `token_id` is `None`: there is no real token to revoke, so
/// `logout` is a no-op for an impersonated session.
pub async fn resolve_dev_auth<A: App + HasDb>(
    app: &A,
    config: &DevAuthConfig,
) -> Result<AuthInfo, AuthError> {
    match config {
        DevAuthConfig::User { id_or_email } => {
            let user_id = if id_or_email.contains('@') {
                let email = db::normalize_user_email(id_or_email).map_err(|e| {
                    AuthError::Permanent(format!("Dev auth: invalid email {id_or_email:?}: {e}"))
                })?;
                app.db()
                    .get_user_id_by_email(&email)
                    .await
                    .map_err(|e| classify_db_err("dev auth: fetch user by email", e))?
                    .ok_or_else(|| {
                        AuthError::Permanent(format!("Dev auth user not found: {email}"))
                    })?
            } else {
                id_or_email.clone()
            };
            fetch_update_user_auth_info(app, user_id).await
        }
    }
}

/// What kind of caller made a request, used as a telemetry/logging dimension.
///
/// The string forms are a stable log contract: CloudWatch Logs Insights queries and
/// metric filters match on them, so renaming a variant's string changes what those
/// queries return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CallerType {
    User,
    Requester,
    ApiToken,
    #[default]
    Unauthenticated,
}

impl CallerType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Requester => "requester",
            Self::ApiToken => "api_token",
            Self::Unauthenticated => "unauthenticated",
        }
    }
}

impl std::fmt::Display for CallerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Maps an optional [`AuthInfo`] to `(caller_type, caller_id)` for telemetry/logging.
pub fn caller_info(auth: Option<&AuthInfo>) -> (CallerType, String) {
    match auth {
        None => (CallerType::Unauthenticated, "unknown".to_owned()),
        Some(AuthInfo::User { id, .. }) => (CallerType::User, id.clone()),
        Some(AuthInfo::Requester { email, .. }) => (CallerType::Requester, email.clone()),
        Some(AuthInfo::ApiToken { token_id, .. }) => (CallerType::ApiToken, token_id.clone()),
    }
}

/// Hex SHA-256 of a token secret. The stored row is keyed by this, never the raw
/// token — see the house rule against storing secrets in `CLAUDE.md`'s spirit
/// (not written down there specifically, but the same principle: a DB/PITR leak
/// must not expose a usable credential).
pub(crate) fn hash_token(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

/// Issue a fresh `mtu_{id}.{secret}` session token for `user_id`. Only the
/// sha256 hash of the full token is stored; the string returned here is the
/// only time it ever exists in full — callers (`verifyAuthCode`,
/// `finishPasskeyLogin`) hand it straight back to the client and keep nothing.
/// The id is minted here, before the row is written, so it can be embedded in
/// the token — see [`USER_TOKEN_PREFIX`] and [`issue_api_token`].
pub async fn issue_user_token<A: App + HasDb>(app: &A, user_id: &str) -> anyhow::Result<String> {
    let id = crate::dynamodb::new_id();
    let full_token = format!(
        "{USER_TOKEN_PREFIX}{id}.{}",
        crate::nonce::generate_nonce(32)
    );
    let hash = hash_token(&full_token);
    let expires_at = crate::expire::ExpirePolicy::UserToken.from_now();
    app.db()
        .create_user_token(&id, &hash, user_id, expires_at)
        .await?;
    Ok(full_token)
}

/// Mint a fresh `mta_{id}.{secret}` integration token for `instance_id` and
/// persist its row, returning both the created [`db::ApiToken`] and the full
/// secret — the only time it ever exists in full. `createApiToken`
/// (`graphql::mutations`) hands the secret straight back to the caller and
/// keeps nothing.
///
/// The id is minted *here*, before the row is written, specifically so it
/// can be embedded in the returned token — see [`API_TOKEN_PREFIX`]'s doc
/// comment for why: verification then becomes a `GetItem` on `id` (no GSI,
/// no eventual-consistency window), with `secret` compared
/// constant-time against the stored `token_hash`, exactly mirroring the
/// `+t{ticket_id}.{reply_token}` reply tag's reasoning in `CLAUDE.md`.
///
/// No expiry: unlike [`issue_user_token`]/[`issue_submit_token`], this is a
/// long-lived integration credential — an external service configures it
/// once and keeps using it, so there is no session to time out. Revocation
/// is `updateApiToken(enabled: false)` or `deleteApiToken`, not waiting for
/// an expiry.
pub async fn issue_api_token<A: App + HasDb>(
    app: &A,
    instance_id: &str,
    name: &str,
    created_by_user_id: &str,
) -> anyhow::Result<(db::ApiToken, String)> {
    let id = crate::dynamodb::new_id();
    // The full token string, prefix and id included — `token_hash` below is
    // sha256 of *this*, not just the random suffix, since verification
    // (`verify_token_with_api_token`) hashes whatever the caller presents in
    // the `Authorization` header.
    let full_token = format!(
        "{API_TOKEN_PREFIX}{id}.{}",
        crate::nonce::generate_nonce(32)
    );
    let hash = hash_token(&full_token);
    let token = app
        .db()
        .create_api_token(&id, instance_id, name, &hash, created_by_user_id)
        .await?;
    Ok((token, full_token))
}

/// Fetch a user's DB record, reject if disabled, and throttle-touch
/// `access_time`. Shared by every path that resolves to an [`AuthInfo::User`]
/// (opaque-token verification and `--dev-auth-user`), so "what does it take for
/// a user id to become a valid principal" is defined in exactly one place.
/// Always returns `token_id: None`; callers that authenticated via a token fill
/// it in themselves afterward.
pub(crate) async fn fetch_update_user_auth_info<A: App + HasDb>(
    app: &A,
    user_id: String,
) -> Result<AuthInfo, AuthError> {
    let users = app
        .db()
        .get_users(&[&user_id])
        .await
        .map_err(|e| classify_db_err("fetch user from db", e))?;
    let user = users
        .into_iter()
        .next()
        .flatten()
        .ok_or_else(|| AuthError::Permanent("User not found".into()))?;

    if !user.enabled {
        return Err(AuthError::Permanent("User account is disabled".into()));
    }

    // Throttled: only write access_time if it's stale by more than a minute, to
    // reduce DB write load on the hot path of every authenticated request.
    let now = crate::clock::now_sec();
    if user.access_time.is_none_or(|t| now > t + 60) {
        match app
            .db()
            .update_user(&user_id, db::UserUpdateShape::AccessTime)
            .await
        {
            Ok(_) => {}
            Err(db::Error::MutationDisabled) => {
                warn!("update_user skipped: mutations disabled");
            }
            Err(e) => return Err(AuthError::Transient(e.to_string())),
        }
    }

    // One query against membership.user_id-index — every instance this user
    // belongs to, in one round trip, so guards can check Member/InstanceOwner
    // without a further DB call per field.
    let memberships = app
        .db()
        .list_memberships_by_user(&user_id)
        .await
        .map_err(|e| classify_db_err("fetch user memberships", e))?
        .into_iter()
        .map(|m| Membership {
            instance_id: m.instance_id,
            is_owner: m.role == db::MembershipRole::Owner,
        })
        .collect();

    Ok(AuthInfo::User {
        id: user_id,
        memberships,
        is_superuser: user.superuser,
        token_id: None,
        grant_id: None,
    })
}

async fn verify_token_with_user_token<A: App + HasDb>(
    app: &A,
    token: &str,
) -> Result<AuthInfo, AuthError> {
    let Some((id, _secret)) = parse_id_token(USER_TOKEN_PREFIX, token) else {
        return Err(AuthError::Permanent("Invalid user token".into()));
    };
    let user_token = app
        .db()
        .get_user_token(id)
        .await
        .map_err(|e| classify_db_err("fetch user token", e))?
        .ok_or_else(|| AuthError::Permanent("Invalid user token".into()))?;

    // Same message as a missing row — see `verify_token_with_api_token`.
    let hash = hash_token(token);
    if !crate::inbound::resolution::constant_time_eq(
        hash.as_bytes(),
        user_token.token_hash.as_bytes(),
    ) {
        return Err(AuthError::Permanent("Invalid user token".into()));
    }

    let now = crate::clock::now_sec();
    if now >= user_token.expires_at {
        return Err(AuthError::Permanent("User token has expired".into()));
    }

    let token_id = user_token.id.clone();

    // Throttled touch, same 60s window as access_time above.
    if user_token.last_used_at.is_none_or(|t| now > t + 60) {
        match app
            .db()
            .update_user_token(&user_token.id, db::UserTokenUpdateShape::TouchLastUsed)
            .await
        {
            Ok(_) => {}
            Err(db::Error::MutationDisabled) => {
                warn!("update_user_token skipped: mutations disabled");
            }
            Err(e) => return Err(AuthError::Transient(e.to_string())),
        }
    }

    match fetch_update_user_auth_info(app, user_token.user_id).await? {
        AuthInfo::User {
            id,
            memberships,
            is_superuser,
            ..
        } => Ok(AuthInfo::User {
            id,
            memberships,
            is_superuser,
            token_id: Some(token_id),
            grant_id: None,
        }),
        other => Ok(other),
    }
}

async fn verify_token_with_requester_token<A: App + HasDb>(
    app: &A,
    token: &str,
) -> Result<AuthInfo, AuthError> {
    let hash = hash_token(token);
    let state = app
        .db()
        .get_ephemeral_state(&submit_token_state_id(&hash))
        .await
        .map_err(|e| classify_db_err("fetch submit token", e))?
        .filter(|s| s.kind == SUBMIT_TOKEN_STATE_KIND)
        .ok_or_else(|| AuthError::Permanent("Invalid submit token".into()))?;

    let now = crate::clock::now_sec();
    if now >= state.expires_at {
        return Err(AuthError::Permanent("Submit token has expired".into()));
    }

    let payload: SubmitTokenPayload = serde_json::from_str(&state.payload)
        .map_err(|e| AuthError::Permanent(format!("Corrupt submit token payload: {e}")))?;

    Ok(AuthInfo::Requester {
        email: payload.email,
        instance_id: payload.instance_id,
    })
}

/// Split a presented `{prefix}{id}.{secret}` token (`mtu_` or `mta_`) into
/// `(id, secret)`, or `None` if it isn't shaped like one — the prefix is
/// missing, there's no `.` separator, or either half is empty. Splitting on
/// the *first* `.` is unambiguous because neither the nanoid alphabet (`id`)
/// nor the base64url alphabet ([`crate::nonce::generate_nonce`], `secret`)
/// contains `.` — see [`API_TOKEN_PREFIX`]'s doc comment. A free function,
/// not inlined into the verifiers, so the malformed-input cases are
/// unit-testable without a database.
fn parse_id_token<'a>(prefix: &str, token: &'a str) -> Option<(&'a str, &'a str)> {
    let rest = token.strip_prefix(prefix)?;
    let (id, secret) = rest.split_once('.')?;
    if id.is_empty() || secret.is_empty() {
        return None;
    }
    Some((id, secret))
}

/// Verify an `mta_` integration token: parse it, `GetItem` the row by the id
/// it carries, compare the *full* presented token against the stored
/// `token_hash` in constant time, and check `enabled` — see
/// [`issue_api_token`]'s doc comment for the id-in-token design this mirrors
/// from the reply tag. A missing row and a hash mismatch are reported with
/// the **same message** ("Invalid API token"), deliberately: distinguishing
/// them would tell a caller with a wrong secret whether the id half of their
/// guess happened to be real.
async fn verify_token_with_api_token<A: App + HasDb>(
    app: &A,
    token: &str,
) -> Result<AuthInfo, AuthError> {
    let Some((id, _secret)) = parse_id_token(API_TOKEN_PREFIX, token) else {
        return Err(AuthError::Permanent("Invalid API token".into()));
    };
    let api_token = app
        .db()
        .get_api_token(id)
        .await
        .map_err(|e| classify_db_err("fetch api token", e))?
        .ok_or_else(|| AuthError::Permanent("Invalid API token".into()))?;

    let hash = hash_token(token);
    if !crate::inbound::resolution::constant_time_eq(
        hash.as_bytes(),
        api_token.token_hash.as_bytes(),
    ) {
        return Err(AuthError::Permanent("Invalid API token".into()));
    }

    if !api_token.enabled {
        return Err(AuthError::Permanent("API token is disabled".into()));
    }

    // Throttled touch, same 60s window as access_time/user_token above.
    let now = crate::clock::now_sec();
    if api_token.last_used_at.is_none_or(|t| now > t + 60) {
        match app
            .db()
            .update_api_token(&api_token.id, db::ApiTokenUpdateShape::TouchLastUsed)
            .await
        {
            Ok(_) => {}
            Err(db::Error::MutationDisabled) => {
                warn!("update_api_token skipped: mutations disabled");
            }
            Err(e) => return Err(AuthError::Transient(e.to_string())),
        }
    }

    Ok(AuthInfo::ApiToken {
        token_id: api_token.id,
        instance_id: api_token.instance_id,
    })
}

/// Dispatch an opaque token to the right verifier by its prefix: `mtu_` for a
/// user session, `mts_` for a requester submit capability, `mta_` for an
/// instance-scoped integration token. Anything else is a
/// definitively bad credential, not a "try the next scheme" fallthrough.
pub async fn verify_token<A: App + HasDb>(app: &A, token: &str) -> Result<AuthInfo, AuthError> {
    if token.starts_with(USER_TOKEN_PREFIX) {
        return verify_token_with_user_token(app, token).await;
    }
    if token.starts_with(REQUESTER_TOKEN_PREFIX) {
        return verify_token_with_requester_token(app, token).await;
    }
    if token.starts_with(API_TOKEN_PREFIX) {
        return verify_token_with_api_token(app, token).await;
    }
    Err(AuthError::Permanent("Unrecognized token".into()))
}

/// Dispatch an `Authorization` header value: `Bearer <token>` is the only scheme
/// Toolbox has (no cookies, no signed-kiosk-key scheme like seslogin's `SLKey`).
/// Returns `None` when there is no recognized header, so the request proceeds
/// unauthenticated and the GraphQL guards (`AuthRequirement`) reject anything that
/// requires a principal. A `Bearer` header that *is* present but doesn't verify
/// always yields `Some(Err(..))` — a bad credential is not silently treated as no
/// credential.
pub async fn verify_authorization_header<A: App + HasDb>(
    app: &A,
    auth_header: Option<&str>,
) -> Option<Result<AuthInfo, AuthError>> {
    let auth_header = auth_header?;
    let token = auth_header.strip_prefix("Bearer ")?;
    Some(verify_token(app, token).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_info_for_no_auth() {
        let (kind, id) = caller_info(None);
        assert_eq!(kind, CallerType::Unauthenticated);
        assert_eq!(id, "unknown");
    }

    #[test]
    fn caller_info_for_user() {
        let auth = AuthInfo::User {
            id: "u1".into(),
            memberships: vec![],
            is_superuser: false,
            token_id: Some("tok1".into()),
            grant_id: None,
        };
        let (kind, id) = caller_info(Some(&auth));
        assert_eq!(kind, CallerType::User);
        assert_eq!(id, "u1");
    }

    #[test]
    fn caller_info_for_requester() {
        let auth = AuthInfo::Requester {
            email: "r@example.com".into(),
            instance_id: "inst1".into(),
        };
        let (kind, id) = caller_info(Some(&auth));
        assert_eq!(kind, CallerType::Requester);
        assert_eq!(id, "r@example.com");
    }

    #[test]
    fn caller_info_for_api_token() {
        let auth = AuthInfo::ApiToken {
            token_id: "tok1".into(),
            instance_id: "inst1".into(),
        };
        let (kind, id) = caller_info(Some(&auth));
        assert_eq!(kind, CallerType::ApiToken);
        assert_eq!(id, "tok1");
    }

    #[test]
    fn caller_type_strings_are_stable() {
        assert_eq!(CallerType::User.as_str(), "user");
        assert_eq!(CallerType::Requester.as_str(), "requester");
        assert_eq!(CallerType::ApiToken.as_str(), "api_token");
        assert_eq!(CallerType::Unauthenticated.as_str(), "unauthenticated");
    }

    #[test]
    fn parse_id_token_accepts_a_well_formed_token() {
        assert_eq!(
            parse_id_token(API_TOKEN_PREFIX, "mta_abc123.def456"),
            Some(("abc123", "def456"))
        );
    }

    #[test]
    fn parse_id_token_rejects_missing_separator() {
        assert_eq!(parse_id_token(API_TOKEN_PREFIX, "mta_abc123def456"), None);
    }

    #[test]
    fn parse_id_token_rejects_empty_id() {
        assert_eq!(parse_id_token(API_TOKEN_PREFIX, "mta_.def456"), None);
    }

    #[test]
    fn parse_id_token_rejects_empty_secret() {
        assert_eq!(parse_id_token(API_TOKEN_PREFIX, "mta_abc123."), None);
    }

    #[test]
    fn parse_id_token_rejects_the_wrong_prefix() {
        assert_eq!(parse_id_token(API_TOKEN_PREFIX, "mtu_abc123.def456"), None);
        assert_eq!(parse_id_token(USER_TOKEN_PREFIX, "mta_abc123.def456"), None);
    }

    #[test]
    fn parse_id_token_accepts_a_user_token() {
        assert_eq!(
            parse_id_token(USER_TOKEN_PREFIX, "mtu_abc123.def456"),
            Some(("abc123", "def456"))
        );
    }

    #[tokio::test]
    async fn verify_token_rejects_a_malformed_user_token() {
        use crate::app;
        use crate::mockdb;
        use crate::mockmail;
        use crate::mockstorage;

        let my_app = app::new(
            mockdb::Handler::new(),
            mockmail::Handler::new(),
            mockstorage::Storage::new(),
            0,
        );
        // `mtu_opaquesecret` is the old, pre-id format: rejected outright, not
        // looked up.
        for bad in ["mtu_opaquesecret", "mtu_.secret", "mtu_id.", "mtu_."] {
            let result = verify_token(&my_app, bad).await;
            assert!(
                matches!(result, Err(AuthError::Permanent(_))),
                "{bad:?} should be rejected as a malformed user token"
            );
        }
    }

    #[tokio::test]
    async fn verify_token_rejects_a_malformed_api_token() {
        use crate::app;
        use crate::mockdb;
        use crate::mockmail;
        use crate::mockstorage;

        let my_app = app::new(
            mockdb::Handler::new(),
            mockmail::Handler::new(),
            mockstorage::Storage::new(),
            0,
        );
        for bad in ["mta_no_dot_here", "mta_.secret", "mta_id.", "mta_."] {
            let result = verify_token(&my_app, bad).await;
            assert!(
                matches!(result, Err(AuthError::Permanent(_))),
                "{bad:?} should be rejected as a malformed api token"
            );
        }
    }

    #[test]
    fn hash_token_is_stable_hex_sha256() {
        let h = hash_token("mtu_abc");
        assert_eq!(h, hash_token("mtu_abc"));
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(h, hash_token("mtu_xyz"));
    }

    #[test]
    fn classify_db_err_maps_not_found_to_permanent() {
        let e = classify_db_err("x", db::Error::NotFound("missing".into()));
        assert!(matches!(e, AuthError::Permanent(_)));
    }

    #[test]
    fn classify_db_err_maps_everything_else_to_transient() {
        for e in [
            db::Error::Infrastructure("boom".into()),
            db::Error::Hydration("boom".into()),
            db::Error::Integrity("boom".into()),
            db::Error::TypeConversion("boom".into()),
            db::Error::MutationDisabled,
        ] {
            assert!(matches!(classify_db_err("x", e), AuthError::Transient(_)));
        }
    }

    /// `verify_token` must keep rejecting `mtoa_`/`mtor_` OAuth tokens: they match
    /// no prefix it recognizes, so they fail as unrecognized without ever reaching
    /// the DB (mockdb fails every call, so reaching it would surface as
    /// `Transient`). `oauth::verify_access_token` is the only accepted entry point.
    #[tokio::test]
    async fn verify_token_rejects_oauth_tokens() {
        use crate::app;
        use crate::mockdb;
        use crate::mockmail;
        use crate::mockstorage;

        let my_app = app::new(
            mockdb::Handler::new(),
            mockmail::Handler::new(),
            mockstorage::Storage::new(),
            0,
        );
        for token in ["mtoa_abc123.somesecret", "mtor_abc123.somesecret"] {
            let result = verify_token(&my_app, token).await;
            assert!(matches!(result, Err(AuthError::Permanent(_))), "{token}");
        }
    }

    #[tokio::test]
    async fn verify_token_rejects_an_unrecognized_prefix() {
        use crate::app;
        use crate::mockdb;
        use crate::mockmail;
        use crate::mockstorage;

        let my_app = app::new(
            mockdb::Handler::new(),
            mockmail::Handler::new(),
            mockstorage::Storage::new(),
            0,
        );
        let result = verify_token(&my_app, "slu_not_our_scheme").await;
        assert!(matches!(result, Err(AuthError::Permanent(_))));
    }

    #[tokio::test]
    async fn verify_authorization_header_ignores_a_missing_header() {
        use crate::app;
        use crate::mockdb;
        use crate::mockmail;
        use crate::mockstorage;

        let my_app = app::new(
            mockdb::Handler::new(),
            mockmail::Handler::new(),
            mockstorage::Storage::new(),
            0,
        );
        assert!(verify_authorization_header(&my_app, None).await.is_none());
    }

    #[tokio::test]
    async fn verify_authorization_header_ignores_a_non_bearer_scheme() {
        use crate::app;
        use crate::mockdb;
        use crate::mockmail;
        use crate::mockstorage;

        let my_app = app::new(
            mockdb::Handler::new(),
            mockmail::Handler::new(),
            mockstorage::Storage::new(),
            0,
        );
        assert!(
            verify_authorization_header(&my_app, Some("Basic dXNlcjpwYXNz"))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn verify_authorization_header_surfaces_a_bad_bearer_token_as_an_error() {
        use crate::app;
        use crate::mockdb;
        use crate::mockmail;
        use crate::mockstorage;

        let my_app = app::new(
            mockdb::Handler::new(),
            mockmail::Handler::new(),
            mockstorage::Storage::new(),
            0,
        );
        let result = verify_authorization_header(&my_app, Some("Bearer garbage"))
            .await
            .expect("a Bearer header must always be checked, never silently ignored");
        assert!(result.is_err());
    }
}
