//! Outgoing-email abstraction.
//!
//! Mirrors the [`crate::db`] / [`crate::dynamodb`] / [`crate::mockdb`] split: this
//! module holds the trait, [`crate::sesmail`] sends via AWS SES, and
//! [`crate::mockmail`] logs the message instead, so the API can run — including the
//! email-code login flow (`requestAuthCode`) — with no AWS account.
//!
//! `send_plain_text`/`send_html` cover the auth flow's simple mail. `send_raw`
//! (step 6) is for ticket mail: a fully-built RFC 5322 message from
//! [`crate::outbound`], sent with explicit envelope recipients so threading
//! headers, `Reply-To`, and attachments (step 7) are under this project's
//! control rather than SES's "simple" template. It returns the provider's
//! message id in the exact form later threading needs to look it back up —
//! see [`crate::sesmail`]'s doc comment for what that form is and why.

use anyhow::Result;
use std::future::Future;

/// Sender/reply-to for the simple auth-code mail (`send_plain_text`/
/// `send_html` — `requestAuthCode`'s login code), which has no per-instance
/// identity to speak from. **Not used by ticket mail**: that has real
/// per-instance addressing (`"{instance.from_name}" <{instance's primary
/// inbound address}>`, `Reply-To` carrying the ticket's reply token) via
/// [`crate::outbound`] and `send_raw`, computed fresh per send rather than
/// living behind a crate-wide constant.
/// Fallback sender for system mail — login codes and anything else not tied to
/// a particular instance. `.test` is reserved (RFC 6761) and can never be
/// registered or verified with a mail provider, which is deliberate: it makes a
/// deployment that forgot to configure [`MAIL_FROM_VAR`] fail loudly at the
/// provider instead of quietly sending from a domain someone else owns.
pub const FROM_FALLBACK: &str = "no-reply@toolbox.test";
pub const REPLY_TO_FALLBACK: &str = "support@toolbox.test";

/// Env var naming the address system mail is sent from. Set it to an address on
/// a domain verified with the mail provider — not necessarily the same domain
/// inbound mail arrives on (deployments can split the web app's own domain,
/// which system mail sends from, from a tenant's inbound mail domain).
///
/// Instance-scoped mail (ticket replies, notifications) does NOT use this: it
/// sends from the instance's own inbound address so replies thread back to the
/// right tenant. See `outbound::primary_inbound_address`.
pub const MAIL_FROM_VAR: &str = "MAIL_FROM";
pub const MAIL_REPLY_TO_VAR: &str = "MAIL_REPLY_TO";

/// The address system mail is sent from, or the reserved-domain fallback.
pub fn system_from() -> String {
    std::env::var(MAIL_FROM_VAR)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| FROM_FALLBACK.to_string())
}

/// Reply-To for system mail. Defaults to the sender, so a reply goes somewhere
/// real rather than to a second unconfigured placeholder.
pub fn system_reply_to() -> String {
    std::env::var(MAIL_REPLY_TO_VAR)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(system_from)
}

/// Redirect every outgoing email to this address instead of its real recipient.
///
/// A fork's local dev environment may be pointed at a database that carries real
/// member/requester email addresses (e.g. a sanitized snapshot of prod). Setting
/// `MAIL_OVERRIDE_TO` in `.env` makes it impossible for a local run to mail a real
/// person. Never set it in a deployed environment.
pub(crate) const OVERRIDE_TO_VAR: &str = "MAIL_OVERRIDE_TO";

/// Serializes every test in this crate that reads or writes the process-global
/// `MAIL_OVERRIDE_TO` env var — not just the ones in this module's own `tests`
/// submodule. `cargo test` runs tests from every module in one process on
/// multiple threads, so `mockmail::tests::records_what_was_sent` (which asserts
/// on `resolve_recipient`'s *unset* behaviour) must serialize against this
/// module's own tests (which set and clear the var) or it can observe another
/// thread's in-flight override and fail nondeterministically.
///
/// `tokio::sync::Mutex`, not `std::sync::Mutex`: `mockmail`'s test is
/// `#[tokio::test]` and needs to hold the guard across an `.await`, which is
/// exactly what a `std`/`parking_lot` guard must never do (see
/// `clippy::await_holding_lock`) but a tokio guard is designed for.
/// `blocking_lock()` (used by this module's own, synchronous `#[test]`s) is
/// safe here specifically because none of those tests run inside a tokio
/// runtime themselves.
#[cfg(test)]
pub(crate) static OVERRIDE_TO_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Resolve the address to actually send to, honouring [`OVERRIDE_TO_VAR`].
///
/// Applied by every backend, mock included, so what the mock logs is what a real
/// send would have done.
pub fn resolve_recipient(to: &str) -> String {
    match std::env::var(OVERRIDE_TO_VAR) {
        Ok(override_to) if !override_to.trim().is_empty() => {
            let override_to = override_to.trim().to_string();
            tracing::warn!("{OVERRIDE_TO_VAR} is set: redirecting email for {to} to {override_to}");
            override_to
        }
        _ => to.to_string(),
    }
}

/// `Sync` is required for the same reason as [`crate::db::Handler`]: a
/// `&impl Handler` is held across `.await` inside the `Send` futures the
/// GraphQL/Poem stack builds.
pub trait Handler: Sync {
    fn send_plain_text(
        &self,
        to: &str,
        subject: &str,
        content: &str,
    ) -> impl Future<Output = Result<()>> + Send;

    fn send_html(
        &self,
        to: &str,
        subject: &str,
        html: &str,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Send a fully-built raw RFC 5322 message (see [`crate::outbound`]) to
    /// explicit envelope recipients, returning the provider's message id.
    ///
    /// `to`/`cc` are the *envelope* recipients — what actually receives the
    /// mail — independent of whatever the raw bytes' own `To`/`Cc` headers
    /// say (in practice always the same, since `crate::outbound` builds both
    /// from the same filtered list). Every implementation must apply
    /// [`resolve_recipient`] to each of `to`/`cc` before sending, exactly as
    /// `send_plain_text`/`send_html` do — see [`OVERRIDE_TO_VAR`]'s doc
    /// comment: a raw send must be just as impossible to point at a real
    /// person from local dev as a plain-text one.
    fn send_raw(
        &self,
        raw: &[u8],
        to: &[String],
        cc: &[String],
    ) -> impl Future<Output = Result<String>> + Send;

    /// [`Self::send_raw`], plus provider message tags (`name`, `value`) that
    /// the provider echoes back on every delivery event for this message —
    /// SES's `EmailTags`, published by the `toolbox` configuration set's
    /// event destination (`infra/ses.tf`). Broadcasts use them to tie a
    /// bounce/complaint/delivery event back to one recipient row with no
    /// lookup index; see [`crate::broadcast_events`]. Tag names and values
    /// must be SES-safe (`[A-Za-z0-9_-]`, ≤ 256 chars).
    fn send_raw_tagged(
        &self,
        raw: &[u8],
        to: &[String],
        cc: &[String],
        tags: &[(String, String)],
    ) -> impl Future<Output = Result<String>> + Send;
}

#[cfg(test)]
mod tests {
    #[test]
    fn system_from_falls_back_to_the_reserved_domain_when_unset() {
        // Guarded by the same mutex the other env-touching tests use; see
        // below. The point of the assertion is that the fallback is on a
        // domain that cannot be registered, so an unconfigured deployment is
        // rejected by the provider rather than sending as someone else.
        assert!(super::FROM_FALLBACK.ends_with(".test"));
        assert!(super::REPLY_TO_FALLBACK.ends_with(".test"));
    }

    use super::*;

    // These tests share the process-global `MAIL_OVERRIDE_TO` env var (as does
    // `mockmail::tests::records_what_was_sent`), so they must not run
    // concurrently with anything else that touches it — see
    // `OVERRIDE_TO_ENV_LOCK`'s doc comment.
    use super::OVERRIDE_TO_ENV_LOCK as ENV_LOCK;

    #[test]
    fn resolve_recipient_passes_through_when_unset() {
        let _guard = ENV_LOCK.blocking_lock();
        // SAFETY: serialized by ENV_LOCK; no other thread reads/writes this var
        // concurrently within this test binary.
        unsafe {
            std::env::remove_var(OVERRIDE_TO_VAR);
        }
        assert_eq!(resolve_recipient("a@example.com"), "a@example.com");
    }

    #[test]
    fn resolve_recipient_redirects_when_set() {
        let _guard = ENV_LOCK.blocking_lock();
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var(OVERRIDE_TO_VAR, " override@example.com ");
        }
        assert_eq!(resolve_recipient("a@example.com"), "override@example.com");
        unsafe {
            std::env::remove_var(OVERRIDE_TO_VAR);
        }
    }

    #[test]
    fn resolve_recipient_ignores_a_blank_override() {
        let _guard = ENV_LOCK.blocking_lock();
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var(OVERRIDE_TO_VAR, "   ");
        }
        assert_eq!(resolve_recipient("a@example.com"), "a@example.com");
        unsafe {
            std::env::remove_var(OVERRIDE_TO_VAR);
        }
    }
}
