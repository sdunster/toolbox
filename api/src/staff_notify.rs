//! Best-effort staff (member) email notifications — deliberately a separate
//! pipeline from [`crate::outbound`], which builds customer-facing mail.
//!
//! **Why separate, not a variant of `outbound::build_outbound`:** a staff
//! notice must never be able to land in the customer thread. `outbound`'s
//! `Reply-To` carries the `+t{ticket_id}.{reply_token}` tag precisely so a
//! customer's reply threads back onto the ticket — but a staff member's own
//! mail client offering "Reply" on a notice built that way would relay
//! straight back into the ticket as if the *customer* had written it (see
//! `crate::inbound::pipeline`'s "From is one of our own inbound addresses"
//! loop guard, which that tag would otherwise sail past). So every notice
//! this module builds:
//! - is `From` [`mail::system_from`] (the system sender), not the instance's
//!   inbound address, with `Reply-To` [`mail::system_reply_to`];
//! - carries no `+t…` tag anywhere;
//! - has a `Subject` that does not parse as `[#{slug}-{number}]`
//!   (`outbound::parse_subject_tag`) — SES receives every address on the
//!   support domain, so a tag-shaped subject on a reply to this notice would
//!   otherwise be a second, wrong way back into the thread;
//! - is one message per recipient (`To:` that person alone, no `Cc:`), so
//!   the footer can say *why* that specific person got it without exposing
//!   the rest of the team;
//! - is never persisted as a `ticket_message` row — it is not part of the
//!   customer-visible thread.
//!
//! The pure parts — which settings default to what, who gets mailed and why
//! ([`select_recipients`]), and the MIME itself ([`build_staff_mime`]) — are
//! unit-tested here with no I/O. The single impure entry point,
//! [`notify_staff`], is **best-effort**: every failure (loading members,
//! building, sending) is `warn!`-logged and swallowed, exactly like
//! `graphql::mutations::send_system_notification` and
//! `inbound::pipeline::notify_others` — by the time this runs, the
//! mutation's/message's primary effect has already happened, so a failure
//! here must never turn into a duplicate ticket or a failed mutation on
//! retry. See `CLAUDE.md`'s "Staff notifications" house rule for the event
//! list and the recipient rules this implements.

use anyhow::{Context, Result};
use mail_builder::MessageBuilder;
use mail_builder::headers::raw::Raw;
use tracing::warn;

use crate::app::{App, HasDb, HasMail};
use crate::db;
use crate::db::Handler as _;
use crate::mail;
use crate::mail::Handler as _;

/// Env var naming this deployment's web app origin — used to build the
/// `View ticket`/`Change your notification settings` links a staff notice
/// carries. Trimmed, with a trailing `/` stripped, so every caller can write
/// `{base}/app/...` without checking for a double slash.
pub const APP_BASE_URL_VAR: &str = "APP_BASE_URL";
const APP_BASE_URL_FALLBACK: &str = "http://localhost:5173";

/// The pure half of [`app_base_url`] — normalizes an already-read value (or
/// `None`, for "the env var isn't set") without touching the environment
/// itself, so this is testable with no `tokio::sync::Mutex` env-serialization
/// dance (see `CLAUDE.md`'s house rule on env-touching tests): nothing here
/// ever reads or writes a process-global.
fn normalize_base_url(raw: Option<&str>) -> String {
    raw.map(str::trim)
        .map(|s| s.trim_end_matches('/'))
        .filter(|s| !s.is_empty())
        .unwrap_or(APP_BASE_URL_FALLBACK)
        .to_string()
}

/// This deployment's web app origin, or the localhost fallback used by `make
/// dev`/`make dev-local`. The one impure caller of [`normalize_base_url`].
pub fn app_base_url() -> String {
    normalize_base_url(std::env::var(APP_BASE_URL_VAR).ok().as_deref())
}

/// Who performed the action a staff notice is about — used both to exclude
/// the actor from their own notification (nobody is mailed about their own
/// action) and to build a human-readable name for the message body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor<'a> {
    /// A signed-in member — every `graphql::mutations` call site.
    User { id: &'a str },
    /// The sender of an arriving customer email — already normalized (see
    /// `inbound::routing::normalize_recipient`), matched directly against
    /// `db::User::email` (itself always stored normalized), and also used
    /// for `submitTicket`, whose "actor" is the requester's own address.
    Email(&'a str),
}

/// Why a particular staff member received a notice — the footer text
/// `notify_staff` stamps as "You're receiving this because {reason}." Named
/// after the [`db::NotificationSettings`] field that authorized the send, so
/// the two can never drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    NewTicket,
    AssignedToMe,
    AssignedToMeUpdated,
    UnassignedUpdated,
    AssignedToOthersUpdated,
}

impl Reason {
    fn footer_text(self) -> &'static str {
        match self {
            Reason::NewTicket => {
                "a new ticket was opened and you have notifications enabled for new tickets"
            }
            Reason::AssignedToMe => "this ticket was assigned to you",
            Reason::AssignedToMeUpdated => {
                "this ticket is (or was) assigned to you and you have notifications enabled for updates to your tickets"
            }
            Reason::UnassignedUpdated => {
                "this ticket has no assignee and you have notifications enabled for updates to unassigned tickets"
            }
            Reason::AssignedToOthersUpdated => {
                "you have notifications enabled for updates to tickets assigned to others"
            }
        }
    }
}

/// One staff member who should be notified, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    pub user_id: String,
    pub email: String,
    pub name: String,
    pub reason: Reason,
}

/// What happened on a ticket — everything [`headline`] needs to render the
/// notice's one-line summary. Deliberately carries no message body of its
/// own; the excerpt is a separate parameter to [`notify_staff`], since it's
/// the same text for every recipient of a given event while the headline can
/// differ per recipient (see [`headline`]'s doc comment on the assignment
/// case).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaffEvent<'a> {
    /// A new ticket was opened — inbound mail or `submitTicket`.
    NewTicket { requester_email: &'a str },
    /// A customer sent a message on an existing ticket (inbound mail).
    CustomerMessage { from_email: &'a str, reopened: bool },
    /// A member replied (`replyToTicket`) — notified only after the
    /// customer-facing send actually succeeded, see `CLAUDE.md`.
    AgentReply,
    /// A member added an internal note (`addInternalNote`).
    InternalNote,
    /// `setTicketStatus` changed the ticket's status. Never constructed for
    /// a transition *into* `Deleted` — callers skip `notify_staff`
    /// entirely for that case, mirroring the customer-mail rule
    /// (`outbound::status_notice_body`); [`headline`] has no sensible
    /// wording for it because it is structurally never asked to render one.
    StatusChanged {
        old: db::TicketStatus,
        new: db::TicketStatus,
    },
    /// `assignTicket` changed the assignee — only ever constructed when
    /// `old != new`.
    Assigned {
        old: Option<&'a str>,
        new: Option<&'a str>,
    },
}

/// Recipient selection — pure, and the whole of `CLAUDE.md`'s "Staff
/// notifications" recipient-rules table. `candidates` is every membership of
/// the ticket's instance joined with its `db::User` (dropping any row whose
/// user is missing, which `notify_staff` already filters out before calling
/// this). `assignee_user_id` is the ticket's assignee *unchanged by this
/// event* — for every event except [`StaffEvent::Assigned`], the write that
/// triggered this notification never touches who the ticket is assigned to,
/// so this is simply `ticket.assignee_user_id` as it already stood.
pub fn select_recipients(
    candidates: &[(db::Membership, db::User)],
    ticket_status: db::TicketStatus,
    assignee_user_id: Option<&str>,
    actor: Actor<'_>,
    event: &StaffEvent<'_>,
) -> Vec<Recipient> {
    // Rule 4: a deleted ticket mails nobody, regardless of event or
    // settings — e.g. inbound mail threading onto a ticket that was deleted
    // between the message arriving and this being computed.
    if ticket_status == db::TicketStatus::Deleted {
        return Vec::new();
    }

    candidates
        .iter()
        .filter(|(_, user)| user.enabled)
        .filter(|(_, user)| !is_actor(user, actor))
        .filter_map(|(membership, user)| {
            reason_for(membership, user, assignee_user_id, event).map(|reason| Recipient {
                user_id: user.id.clone(),
                email: user.email.clone(),
                name: user.name.clone(),
                reason,
            })
        })
        .collect()
}

/// Whether `user` is the one who performed the action — see [`Actor`]'s doc
/// comment. Both sides of the `Email` comparison are expected already
/// normalized (trimmed, lowercased), so this is a plain equality check, not
/// a case-insensitive one — normalizing here too would silently paper over a
/// caller that forgot to normalize its own side.
fn is_actor(user: &db::User, actor: Actor<'_>) -> bool {
    match actor {
        Actor::User { id } => user.id == id,
        Actor::Email(email) => user.email == email,
    }
}

/// The [`Reason`] `user` should be notified for this event, or `None` if
/// their settings (or, for [`StaffEvent::Assigned`], their relationship to
/// the assignment) don't call for it.
fn reason_for(
    membership: &db::Membership,
    user: &db::User,
    assignee_user_id: Option<&str>,
    event: &StaffEvent<'_>,
) -> Option<Reason> {
    let settings = &membership.notification_settings;
    match event {
        StaffEvent::NewTicket { .. } => settings.new_ticket.then_some(Reason::NewTicket),
        // Assignment doesn't fan out to the rest of the team — only the
        // person handed the ticket and the person it was taken from. Both
        // checked against *this event's* old/new, never the ticket's
        // current `assignee_user_id` (which callers pass as the *new*
        // value anyway, since the write already happened by the time this
        // runs — see `graphql::mutations::assign_ticket`).
        StaffEvent::Assigned { old, new } => {
            if Some(user.id.as_str()) == *new {
                settings.assigned_to_me.then_some(Reason::AssignedToMe)
            } else if Some(user.id.as_str()) == *old {
                settings
                    .assigned_to_me_updated
                    .then_some(Reason::AssignedToMeUpdated)
            } else {
                None
            }
        }
        // Every other event is an "update": category is the ticket's
        // (event-unrelated) assignee.
        StaffEvent::CustomerMessage { .. }
        | StaffEvent::AgentReply
        | StaffEvent::InternalNote
        | StaffEvent::StatusChanged { .. } => match assignee_user_id {
            Some(assignee) if assignee == user.id => settings
                .assigned_to_me_updated
                .then_some(Reason::AssignedToMeUpdated),
            None => settings
                .unassigned_updated
                .then_some(Reason::UnassignedUpdated),
            Some(_) => settings
                .assigned_to_others_updated
                .then_some(Reason::AssignedToOthersUpdated),
        },
    }
}

/// Longest an excerpt is allowed to be, in `char`s (not bytes — a
/// multi-byte character is never split). Long enough to show real context,
/// short enough that a giant inbound message doesn't blow out a notice.
const MAX_EXCERPT_CHARS: usize = 4000;

/// Truncate `s` to at most [`MAX_EXCERPT_CHARS`] characters, appending `…`
/// when it was cut. Splits on a `char` boundary, never a byte boundary —
/// `s.chars().take(n)` can't produce invalid UTF-8 the way `&s[..n]` could.
fn truncate_excerpt(s: &str) -> String {
    if s.chars().count() <= MAX_EXCERPT_CHARS {
        return s.to_string();
    }
    let truncated: String = s.chars().take(MAX_EXCERPT_CHARS).collect();
    format!("{truncated}…")
}

/// The excerpt line for a notice's body, or `None` for an event with no
/// message content ([`StaffEvent::StatusChanged`]/[`StaffEvent::Assigned`] —
/// callers pass `message_body: None` for those). `Some("")`/whitespace-only
/// (an inbound message with no text part) becomes the placeholder rather
/// than an empty excerpt — "for inbound prefer text body, fall back to '(no
/// message body)'" per `CLAUDE.md`.
fn excerpt_line(message_body: Option<&str>) -> Option<String> {
    message_body.map(|body| {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            "(no message body)".to_string()
        } else {
            truncate_excerpt(trimmed)
        }
    })
}

/// The notice's one-line headline. `actor_name` is already resolved by the
/// caller (the member's `name`, falling back to `email`, or the customer's
/// email for an `Actor::Email` actor) — this function is pure text
/// formatting, not a lookup.
///
/// **The assignment case is the one place a single event produces two
/// different headlines**, one per recipient: the new assignee (always
/// `Reason::AssignedToMe`, since that's the only reason `select_recipients`
/// ever hands out for them) sees "assigned to you"; the outgoing assignee
/// (always `Reason::AssignedToMeUpdated` here — see `reason_for`) sees
/// either "unassigned from you" (no replacement) or "reassigned to {name}"
/// (there is one, named by `new_assignee_name`). `reason` is what
/// disambiguates which recipient this rendering is for.
fn headline(
    event: &StaffEvent<'_>,
    actor_name: &str,
    reason: Reason,
    new_assignee_name: Option<&str>,
) -> String {
    match event {
        StaffEvent::NewTicket { requester_email } => format!("New ticket from {requester_email}"),
        StaffEvent::CustomerMessage {
            from_email,
            reopened: false,
        } => format!("{from_email} replied"),
        StaffEvent::CustomerMessage {
            from_email,
            reopened: true,
        } => format!("{from_email} replied and reopened the ticket"),
        StaffEvent::AgentReply => format!("{actor_name} replied to the customer"),
        StaffEvent::InternalNote => format!("{actor_name} added an internal note"),
        StaffEvent::StatusChanged { old, new } => {
            use db::TicketStatus::{Closed, Deleted, Open};
            match new {
                Closed => format!("{actor_name} closed this ticket"),
                Open if *old == Deleted => format!("{actor_name} restored this ticket"),
                Open => format!("{actor_name} reopened this ticket"),
                // Never actually constructed — see `StaffEvent::StatusChanged`'s
                // doc comment — but a sane fallback beats a panic if that
                // invariant is ever violated.
                Deleted => format!("{actor_name} deleted this ticket"),
            }
        }
        StaffEvent::Assigned { .. } => match reason {
            Reason::AssignedToMe => format!("{actor_name} assigned this ticket to you"),
            _ => match new_assignee_name {
                Some(name) => format!("{actor_name} reassigned this ticket to {name}"),
                None => format!("{actor_name} unassigned this ticket from you"),
            },
        },
    }
}

/// Minimal HTML escaping — matches `outbound::escape_html`/seslogin's
/// `activity_summary::escape_html`. Duplicated in miniature rather than made
/// `pub(crate)` across the module boundary, the same call `inbound::pipeline`
/// makes for `strip_own_addresses`/`outbound::is_own_address`.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Hand-built HTML body from the same plain-text `body` [`build_staff_mime`]
/// sends as the `text/plain` part — no template engine, no separate content,
/// just escaped text with newlines turned into `<br>`, matching
/// `outbound::render_html`'s approach.
fn render_html(body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html><body style=\"font-family:Arial,Helvetica,sans-serif;color:#222\">\n<div>{}</div>\n</body></html>",
        escape_html(body).replace('\n', "<br>\n")
    )
}

/// One rendered staff notice, ready to send. The MIME-building analog of
/// `outbound::BuiltMessage`, but not that type: a staff notice always has
/// exactly one recipient and no `Cc`, so a `to: Vec<String>` would carry a
/// single-element list for no reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltStaffMessage {
    pub raw: Vec<u8>,
    pub to: String,
}

/// Build one recipient's notice. `headline`/`excerpt` are already rendered
/// text (see [`headline`]/[`excerpt_line`]); this function's own job is
/// purely the MIME shape described in this module's doc comment: `From` the
/// system sender under the instance's display name, `Reply-To` the system
/// reply-to, no `+t…` tag, a subject that can't parse as a customer-mail
/// tag, and the loop/threading headers.
fn build_staff_mime(
    instance: &db::Instance,
    ticket: &db::Ticket,
    recipient: &Recipient,
    headline: &str,
    excerpt: Option<&str>,
    base_url: &str,
) -> Result<BuiltStaffMessage> {
    let from = mail::system_from();
    let from_domain = from
        .rsplit_once('@')
        .map(|(_, d)| d)
        .unwrap_or("toolbox.invalid");

    // `[{instance.name} #{number}] {subject}` — deliberately not
    // `[#{slug}-{number}]`: that shape is exactly what
    // `outbound::parse_subject_tag` looks for, and a staff notice must never
    // produce a subject an inbound reply could thread onto. See this
    // module's doc comment and `subject_never_parses_as_a_customer_mail_tag`
    // below, which pins it.
    let subject = format!("[{} #{}] {}", instance.name, ticket.number, ticket.subject);

    let view_url = format!("{base_url}/app/tickets/{}", ticket.id);
    let settings_url = format!("{base_url}/app/settings");

    let mut body = String::new();
    body.push_str(headline);
    body.push('\n');
    if let Some(excerpt) = excerpt {
        body.push('\n');
        body.push_str(excerpt);
        body.push('\n');
    }
    body.push_str(&format!("\nView ticket: {view_url}\n\n"));
    body.push_str(&format!(
        "You're receiving this because {}. Change your notification settings: {settings_url}\n\
         Replies to this email are not delivered to the customer.",
        recipient.reason.footer_text()
    ));

    let html = render_html(&body);
    let message_id = format!("{}@{}", crate::nonce::generate_nonce(16), from_domain);
    // A single, fixed reference per ticket (not chained per-message the way
    // `outbound::Threading` is) — enough for a mail client to group every
    // notice about one ticket into a thread, and never stored on any
    // `ticket_message` row, so it can never be mistaken for real inbound
    // threading state.
    let references = format!("<ticket-{}@{}>", ticket.id, from_domain);

    let raw = MessageBuilder::new()
        .from((instance.name.as_str(), from.as_str()))
        .reply_to(mail::system_reply_to())
        .to(recipient.email.as_str())
        .subject(subject)
        .message_id(message_id)
        .header("Auto-Submitted", Raw::new("auto-generated"))
        .header("X-Toolbox-Loop", Raw::new("1"))
        .header("References", Raw::new(references))
        .text_body(body)
        .html_body(html)
        .write_to_vec()
        .context("building staff notification MIME message")?;

    Ok(BuiltStaffMessage {
        raw,
        to: recipient.email.clone(),
    })
}

/// Resolve a display name for `actor` out of the already-loaded
/// `candidates` — the member's `name`, falling back to `email` when blank,
/// or (for `Actor::User { id }` naming someone who isn't in `candidates` —
/// shouldn't happen, since every mutation's actor is a member of this
/// ticket's instance, but this must not panic if it somehow did) the bare
/// id. An `Actor::Email` actor (a customer) has no membership row to look
/// up; its address *is* the display name.
fn actor_display_name(candidates: &[(db::Membership, db::User)], actor: Actor<'_>) -> String {
    match actor {
        Actor::Email(email) => email.to_string(),
        Actor::User { id } => candidates
            .iter()
            .find(|(_, u)| u.id == id)
            .map(|(_, u)| display_name(u))
            .unwrap_or_else(|| id.to_string()),
    }
}

fn display_name(user: &db::User) -> String {
    if user.name.trim().is_empty() {
        user.email.clone()
    } else {
        user.name.clone()
    }
}

/// Notify every instance member whose settings call for it about one ticket
/// event — the single impure entry point this module exists to provide. See
/// this module's doc comment for the MIME shape and the best-effort
/// failure handling; see [`select_recipients`] for who gets mailed.
///
/// `assignee_user_id` is the category [`select_recipients`] needs for an
/// "update" event — pass `ticket.assignee_user_id.as_deref()` for every
/// event except [`StaffEvent::Assigned`], where it's ignored (the event
/// itself carries old/new). `message_body` is the raw message text for an
/// event with content (`NewTicket`/`CustomerMessage`/`AgentReply`/
/// `InternalNote`) — `None` for `StatusChanged`/`Assigned`, which have none.
#[allow(clippy::too_many_arguments)]
pub async fn notify_staff<A: App + HasDb + HasMail + Send + Sync>(
    app: &A,
    ticket: &db::Ticket,
    assignee_user_id: Option<&str>,
    event: StaffEvent<'_>,
    actor: Actor<'_>,
    message_body: Option<&str>,
) {
    // Cheapest possible short-circuit for rule 4 — skip every DB call
    // entirely rather than loading members just to filter them all out in
    // `select_recipients` (which enforces the same rule again, so this is
    // pure optimization, not the only place it's checked).
    if ticket.status == db::TicketStatus::Deleted {
        return;
    }

    let context = "staff_notify";

    let instance = match app.db().get_instances(&[ticket.instance_id.as_str()]).await {
        Ok(v) => v.into_iter().next().flatten(),
        Err(e) => {
            warn!(
                "{context}: could not load instance {}: {e}",
                ticket.instance_id
            );
            return;
        }
    };
    let Some(instance) = instance else {
        warn!("{context}: instance {} missing", ticket.instance_id);
        return;
    };

    let memberships = match app
        .db()
        .list_memberships_by_instance(&ticket.instance_id)
        .await
    {
        Ok(m) => m,
        Err(e) => {
            warn!(
                "{context}: could not load memberships for instance {}: {e}",
                ticket.instance_id
            );
            return;
        }
    };
    if memberships.is_empty() {
        return;
    }
    let user_ids: Vec<&str> = memberships.iter().map(|m| m.user_id.as_str()).collect();
    let users = match app.db().get_users(&user_ids).await {
        Ok(u) => u,
        Err(e) => {
            warn!(
                "{context}: could not load users for instance {}: {e}",
                ticket.instance_id
            );
            return;
        }
    };
    let candidates: Vec<(db::Membership, db::User)> = memberships
        .into_iter()
        .zip(users)
        .filter_map(|(m, u)| u.map(|u| (m, u)))
        .collect();

    let recipients = select_recipients(&candidates, ticket.status, assignee_user_id, actor, &event);
    if recipients.is_empty() {
        return;
    }

    let actor_name = actor_display_name(&candidates, actor);
    let new_assignee_name = match &event {
        StaffEvent::Assigned { new: Some(id), .. } => candidates
            .iter()
            .find(|(_, u)| u.id == *id)
            .map(|(_, u)| display_name(u)),
        _ => None,
    };
    let excerpt = excerpt_line(message_body);
    let base_url = app_base_url();

    for recipient in &recipients {
        let headline_text = headline(
            &event,
            &actor_name,
            recipient.reason,
            new_assignee_name.as_deref(),
        );
        let built = match build_staff_mime(
            &instance,
            ticket,
            recipient,
            &headline_text,
            excerpt.as_deref(),
            &base_url,
        ) {
            Ok(b) => b,
            Err(e) => {
                warn!(
                    "{context}: could not build notification for ticket {} recipient {}: {e}",
                    ticket.id, recipient.user_id
                );
                continue;
            }
        };
        if let Err(e) = app
            .mail()
            .send_raw(&built.raw, std::slice::from_ref(&built.to), &[])
            .await
        {
            warn!(
                "{context}: failed to send notification for ticket {} recipient {}: {e}",
                ticket.id, recipient.user_id
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str, email: &str, enabled: bool) -> db::User {
        db::User {
            id: id.to_string(),
            email: email.to_string(),
            name: String::new(),
            enabled,
            created_at: 1_000,
            access_time: None,
            superuser: false,
        }
    }

    fn membership(user_id: &str, settings: db::NotificationSettings) -> db::Membership {
        db::Membership {
            id: format!("m-{user_id}"),
            user_id: user_id.to_string(),
            instance_id: "inst1".to_string(),
            role: db::MembershipRole::Agent,
            notification_settings: settings,
        }
    }

    fn ticket(status: db::TicketStatus, assignee: Option<&str>) -> db::Ticket {
        db::Ticket {
            id: "tick1".to_string(),
            instance_id: "inst1".to_string(),
            number: 7,
            subject: "Printer on fire".to_string(),
            status,
            requester_emails: vec!["req@example.com".to_string()],
            cc_emails: vec![],
            assignee_user_id: assignee.map(str::to_string),
            reply_token: "tok".to_string(),
            created_at: 1_000,
            updated_at: 1_000,
            last_activity_at: 1_000,
            has_attachments: false,
        }
    }

    // ── app_base_url normalization ───────────────────────────────────────

    #[test]
    fn base_url_falls_back_when_unset() {
        assert_eq!(normalize_base_url(None), APP_BASE_URL_FALLBACK);
        assert_eq!(normalize_base_url(Some("")), APP_BASE_URL_FALLBACK);
        assert_eq!(normalize_base_url(Some("   ")), APP_BASE_URL_FALLBACK);
    }

    #[test]
    fn base_url_trims_and_strips_trailing_slash() {
        assert_eq!(
            normalize_base_url(Some("  https://support.example.com/  ")),
            "https://support.example.com"
        );
        assert_eq!(
            normalize_base_url(Some("https://support.example.com")),
            "https://support.example.com"
        );
    }

    // ── truncate_excerpt / excerpt_line ──────────────────────────────────

    #[test]
    fn truncate_excerpt_leaves_short_text_alone() {
        assert_eq!(truncate_excerpt("hello"), "hello");
    }

    #[test]
    fn truncate_excerpt_cuts_on_a_char_boundary_and_marks_it() {
        let long = "é".repeat(MAX_EXCERPT_CHARS + 10);
        let truncated = truncate_excerpt(&long);
        assert_eq!(truncated.chars().count(), MAX_EXCERPT_CHARS + 1); // +1 for '…'
        assert!(truncated.ends_with('…'));
        assert!(String::from_utf8(truncated.into_bytes()).is_ok());
    }

    #[test]
    fn excerpt_line_is_none_for_no_message_event() {
        assert_eq!(excerpt_line(None), None);
    }

    #[test]
    fn excerpt_line_uses_placeholder_for_blank_body() {
        assert_eq!(
            excerpt_line(Some("   ")),
            Some("(no message body)".to_string())
        );
        assert_eq!(
            excerpt_line(Some("")),
            Some("(no message body)".to_string())
        );
    }

    #[test]
    fn excerpt_line_trims_and_truncates_real_content() {
        assert_eq!(
            excerpt_line(Some("  hi there  ")),
            Some("hi there".to_string())
        );
    }

    // ── select_recipients: settings matrix on "update" events ───────────

    #[test]
    fn defaults_notify_an_agent_on_an_update_to_an_unassigned_ticket() {
        let candidates = vec![(
            membership("u2", db::NotificationSettings::default()),
            user("u2", "agent@example.com", true),
        )];
        let recipients = select_recipients(
            &candidates,
            db::TicketStatus::Open,
            None,
            Actor::User { id: "u1" },
            &StaffEvent::InternalNote,
        );
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].reason, Reason::UnassignedUpdated);
    }

    #[test]
    fn defaults_notify_the_assignee_on_an_update_to_their_ticket() {
        let candidates = vec![(
            membership("u2", db::NotificationSettings::default()),
            user("u2", "agent@example.com", true),
        )];
        let recipients = select_recipients(
            &candidates,
            db::TicketStatus::Open,
            Some("u2"),
            Actor::User { id: "u1" },
            &StaffEvent::AgentReply,
        );
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].reason, Reason::AssignedToMeUpdated);
    }

    #[test]
    fn defaults_do_not_notify_a_non_assignee_of_an_update_to_someone_elses_ticket() {
        let candidates = vec![(
            membership("u2", db::NotificationSettings::default()),
            user("u2", "agent@example.com", true),
        )];
        let recipients = select_recipients(
            &candidates,
            db::TicketStatus::Open,
            Some("u3"),
            Actor::User { id: "u1" },
            &StaffEvent::AgentReply,
        );
        assert!(
            recipients.is_empty(),
            "assigned_to_others_updated is off by default"
        );
    }

    #[test]
    fn enabling_assigned_to_others_updated_notifies_on_someone_elses_ticket() {
        let settings = db::NotificationSettings {
            assigned_to_others_updated: true,
            ..db::NotificationSettings::default()
        };
        let candidates = vec![(
            membership("u2", settings),
            user("u2", "agent@example.com", true),
        )];
        let recipients = select_recipients(
            &candidates,
            db::TicketStatus::Open,
            Some("u3"),
            Actor::User { id: "u1" },
            &StaffEvent::AgentReply,
        );
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].reason, Reason::AssignedToOthersUpdated);
    }

    #[test]
    fn every_setting_off_notifies_nobody_on_an_update() {
        let settings = db::NotificationSettings {
            new_ticket: false,
            assigned_to_me: false,
            assigned_to_me_updated: false,
            unassigned_updated: false,
            assigned_to_others_updated: false,
        };
        let candidates = vec![(
            membership("u2", settings),
            user("u2", "agent@example.com", true),
        )];
        for assignee in [None, Some("u2"), Some("u3")] {
            let recipients = select_recipients(
                &candidates,
                db::TicketStatus::Open,
                assignee,
                Actor::User { id: "u1" },
                &StaffEvent::InternalNote,
            );
            assert!(recipients.is_empty(), "assignee={assignee:?}");
        }
    }

    // ── actor exclusion ───────────────────────────────────────────────────

    #[test]
    fn actor_is_excluded_by_user_id() {
        let candidates = vec![(
            membership("u1", db::NotificationSettings::default()),
            user("u1", "actor@example.com", true),
        )];
        let recipients = select_recipients(
            &candidates,
            db::TicketStatus::Open,
            None,
            Actor::User { id: "u1" },
            &StaffEvent::InternalNote,
        );
        assert!(
            recipients.is_empty(),
            "the actor must never be mailed about their own action"
        );
    }

    #[test]
    fn actor_is_excluded_by_normalized_email_for_inbound_events() {
        let candidates = vec![(
            membership("u1", db::NotificationSettings::default()),
            user("u1", "agent@example.com", true),
        )];
        let recipients = select_recipients(
            &candidates,
            db::TicketStatus::Open,
            None,
            Actor::Email("agent@example.com"),
            &StaffEvent::CustomerMessage {
                from_email: "agent@example.com",
                reopened: false,
            },
        );
        assert!(
            recipients.is_empty(),
            "an agent emailing in must not be mailed about their own message"
        );
    }

    #[test]
    fn disabled_users_are_never_recipients() {
        let candidates = vec![(
            membership("u2", db::NotificationSettings::default()),
            user("u2", "agent@example.com", false),
        )];
        let recipients = select_recipients(
            &candidates,
            db::TicketStatus::Open,
            None,
            Actor::User { id: "u1" },
            &StaffEvent::InternalNote,
        );
        assert!(recipients.is_empty());
    }

    // ── deleted-status skip ───────────────────────────────────────────────

    #[test]
    fn a_deleted_ticket_notifies_nobody_regardless_of_event_or_settings() {
        let candidates = vec![(
            membership("u2", db::NotificationSettings::default()),
            user("u2", "agent@example.com", true),
        )];
        let recipients = select_recipients(
            &candidates,
            db::TicketStatus::Deleted,
            None,
            Actor::User { id: "u1" },
            &StaffEvent::NewTicket {
                requester_email: "r@example.com",
            },
        );
        assert!(recipients.is_empty());
    }

    // ── new ticket ────────────────────────────────────────────────────────

    #[test]
    fn new_ticket_notifies_every_enabled_non_actor_member_by_default() {
        let candidates = vec![
            (
                membership("u1", db::NotificationSettings::default()),
                user("u1", "owner@example.com", true),
            ),
            (
                membership("u2", db::NotificationSettings::default()),
                user("u2", "agent@example.com", true),
            ),
        ];
        let recipients = select_recipients(
            &candidates,
            db::TicketStatus::Open,
            None,
            Actor::Email("customer@example.com"),
            &StaffEvent::NewTicket {
                requester_email: "customer@example.com",
            },
        );
        assert_eq!(recipients.len(), 2);
        assert!(recipients.iter().all(|r| r.reason == Reason::NewTicket));
    }

    // ── assignment ────────────────────────────────────────────────────────

    #[test]
    fn assignment_notifies_only_the_new_and_old_assignee() {
        let candidates = vec![
            (
                membership("u1", db::NotificationSettings::default()),
                user("u1", "old@example.com", true),
            ),
            (
                membership("u2", db::NotificationSettings::default()),
                user("u2", "new@example.com", true),
            ),
            (
                membership("u3", db::NotificationSettings::default()),
                user("u3", "bystander@example.com", true),
            ),
        ];
        let recipients = select_recipients(
            &candidates,
            db::TicketStatus::Open,
            None,
            Actor::User { id: "u4" },
            &StaffEvent::Assigned {
                old: Some("u1"),
                new: Some("u2"),
            },
        );
        assert_eq!(recipients.len(), 2);
        let by_id = |id: &str| recipients.iter().find(|r| r.user_id == id).unwrap();
        assert_eq!(by_id("u1").reason, Reason::AssignedToMeUpdated);
        assert_eq!(by_id("u2").reason, Reason::AssignedToMe);
        assert!(!recipients.iter().any(|r| r.user_id == "u3"));
    }

    #[test]
    fn self_assignment_notifies_nobody() {
        let candidates = vec![(
            membership("u1", db::NotificationSettings::default()),
            user("u1", "solo@example.com", true),
        )];
        let recipients = select_recipients(
            &candidates,
            db::TicketStatus::Open,
            None,
            Actor::User { id: "u1" },
            &StaffEvent::Assigned {
                old: None,
                new: Some("u1"),
            },
        );
        assert!(
            recipients.is_empty(),
            "the actor is excluded even as their own new assignee"
        );
    }

    #[test]
    fn unassigning_notifies_only_the_former_assignee() {
        let candidates = vec![(
            membership("u1", db::NotificationSettings::default()),
            user("u1", "old@example.com", true),
        )];
        let recipients = select_recipients(
            &candidates,
            db::TicketStatus::Open,
            None,
            Actor::User { id: "u9" },
            &StaffEvent::Assigned {
                old: Some("u1"),
                new: None,
            },
        );
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0].reason, Reason::AssignedToMeUpdated);
    }

    // ── headline ──────────────────────────────────────────────────────────

    #[test]
    fn headline_text_matches_each_event() {
        assert_eq!(
            headline(
                &StaffEvent::NewTicket {
                    requester_email: "r@example.com"
                },
                "Actor",
                Reason::NewTicket,
                None
            ),
            "New ticket from r@example.com"
        );
        assert_eq!(
            headline(
                &StaffEvent::CustomerMessage {
                    from_email: "r@example.com",
                    reopened: false
                },
                "Actor",
                Reason::AssignedToMeUpdated,
                None
            ),
            "r@example.com replied"
        );
        assert_eq!(
            headline(
                &StaffEvent::CustomerMessage {
                    from_email: "r@example.com",
                    reopened: true
                },
                "Actor",
                Reason::AssignedToMeUpdated,
                None
            ),
            "r@example.com replied and reopened the ticket"
        );
        assert_eq!(
            headline(
                &StaffEvent::AgentReply,
                "Alice",
                Reason::UnassignedUpdated,
                None
            ),
            "Alice replied to the customer"
        );
        assert_eq!(
            headline(
                &StaffEvent::InternalNote,
                "Alice",
                Reason::UnassignedUpdated,
                None
            ),
            "Alice added an internal note"
        );
        assert_eq!(
            headline(
                &StaffEvent::StatusChanged {
                    old: db::TicketStatus::Open,
                    new: db::TicketStatus::Closed
                },
                "Alice",
                Reason::UnassignedUpdated,
                None
            ),
            "Alice closed this ticket"
        );
        assert_eq!(
            headline(
                &StaffEvent::StatusChanged {
                    old: db::TicketStatus::Closed,
                    new: db::TicketStatus::Open
                },
                "Alice",
                Reason::UnassignedUpdated,
                None
            ),
            "Alice reopened this ticket"
        );
        assert_eq!(
            headline(
                &StaffEvent::StatusChanged {
                    old: db::TicketStatus::Deleted,
                    new: db::TicketStatus::Open
                },
                "Alice",
                Reason::UnassignedUpdated,
                None
            ),
            "Alice restored this ticket"
        );
    }

    #[test]
    fn assignment_headlines_differ_for_the_new_vs_old_assignee() {
        let event = StaffEvent::Assigned {
            old: Some("u1"),
            new: Some("u2"),
        };
        assert_eq!(
            headline(&event, "Alice", Reason::AssignedToMe, None),
            "Alice assigned this ticket to you"
        );
        assert_eq!(
            headline(&event, "Alice", Reason::AssignedToMeUpdated, Some("Bob")),
            "Alice reassigned this ticket to Bob"
        );
        assert_eq!(
            headline(&event, "Alice", Reason::AssignedToMeUpdated, None),
            "Alice unassigned this ticket from you"
        );
    }

    // ── build_staff_mime: shape ──────────────────────────────────────────

    fn instance() -> db::Instance {
        db::Instance {
            id: "inst1".to_string(),
            name: "Acme".to_string(),
            slug: "acme".to_string(),
            kind: db::InstanceKind::Support,
            public_submission_enabled: false,
            from_name: "Acme Support".to_string(),
            signature: String::new(),
            created_at: 1_000,
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
        }
    }

    fn recipient() -> Recipient {
        Recipient {
            user_id: "u2".to_string(),
            email: "agent@example.com".to_string(),
            name: "Agent".to_string(),
            reason: Reason::UnassignedUpdated,
        }
    }

    fn parse(raw: &[u8]) -> mail_parser::Message<'_> {
        mail_parser::MessageParser::new()
            .parse(raw)
            .expect("built message must parse")
    }

    #[test]
    fn built_message_addresses_only_the_recipient_with_no_cc() {
        let built = build_staff_mime(
            &instance(),
            &ticket(db::TicketStatus::Open, None),
            &recipient(),
            "Someone replied",
            Some("hello there"),
            "http://localhost:5173",
        )
        .unwrap();
        assert_eq!(built.to, "agent@example.com");

        let msg = parse(&built.raw);
        let to = msg.to().unwrap().first().unwrap();
        assert_eq!(to.address(), Some("agent@example.com"));
        assert!(msg.cc().is_none(), "no Cc ever");
    }

    #[test]
    fn built_message_is_from_the_system_sender_under_the_instance_name() {
        let built = build_staff_mime(
            &instance(),
            &ticket(db::TicketStatus::Open, None),
            &recipient(),
            "headline",
            None,
            "http://localhost:5173",
        )
        .unwrap();
        let msg = parse(&built.raw);
        let from = msg.from().unwrap().first().unwrap();
        assert_eq!(from.name(), Some("Acme"));
        assert_eq!(from.address(), Some(mail::system_from().as_str()));
        let reply_to = msg.reply_to().unwrap().first().unwrap();
        assert_eq!(reply_to.address(), Some(mail::system_reply_to().as_str()));
    }

    #[test]
    fn built_message_reply_to_carries_no_reply_tag() {
        let built = build_staff_mime(
            &instance(),
            &ticket(db::TicketStatus::Open, None),
            &recipient(),
            "headline",
            None,
            "http://localhost:5173",
        )
        .unwrap();
        let msg = parse(&built.raw);
        let reply_to = msg.reply_to().unwrap().first().unwrap();
        assert!(
            !reply_to.address().unwrap_or_default().contains("+t"),
            "a staff notice's Reply-To must never carry a +t… thread tag"
        );
    }

    /// This module's central safety property, pinned: a staff notice's
    /// subject must never parse as the `[#{slug}-{number}]` tag inbound
    /// threading looks for, or a reply to it would be mistaken for a
    /// customer message. See this module's doc comment.
    #[test]
    fn subject_never_parses_as_a_customer_mail_tag() {
        let built = build_staff_mime(
            &instance(),
            &ticket(db::TicketStatus::Open, None),
            &recipient(),
            "headline",
            None,
            "http://localhost:5173",
        )
        .unwrap();
        let msg = parse(&built.raw);
        let subject = msg.subject().unwrap();
        assert_eq!(subject, "[Acme #7] Printer on fire");
        assert_eq!(
            crate::outbound::parse_subject_tag(subject),
            None,
            "subject {subject:?} must not parse as a [#slug-N] tag"
        );
    }

    #[test]
    fn built_message_carries_loop_and_auto_submitted_headers() {
        let built = build_staff_mime(
            &instance(),
            &ticket(db::TicketStatus::Open, None),
            &recipient(),
            "headline",
            None,
            "http://localhost:5173",
        )
        .unwrap();
        let msg = parse(&built.raw);
        assert_eq!(msg.header_raw("X-Toolbox-Loop").map(str::trim), Some("1"));
        assert_eq!(
            msg.header_raw("Auto-Submitted").map(str::trim),
            Some("auto-generated")
        );
    }

    #[test]
    fn built_message_body_carries_the_link_settings_url_and_reason() {
        let built = build_staff_mime(
            &instance(),
            &ticket(db::TicketStatus::Open, None),
            &recipient(),
            "Someone replied",
            Some("the excerpt text"),
            "https://acme.toolbox.test",
        )
        .unwrap();
        let msg = parse(&built.raw);
        let text = msg.body_text(0).unwrap().into_owned();
        assert!(text.contains("Someone replied"));
        assert!(text.contains("the excerpt text"));
        assert!(text.contains("https://acme.toolbox.test/app/tickets/tick1"));
        assert!(text.contains("https://acme.toolbox.test/app/settings"));
        assert!(text.contains(recipient().reason.footer_text()));
        assert!(text.contains("not delivered to the customer"));
    }

    #[test]
    fn built_message_html_body_escapes_content() {
        let mut r = recipient();
        r.reason = Reason::UnassignedUpdated;
        let built = build_staff_mime(
            &instance(),
            &ticket(db::TicketStatus::Open, None),
            &r,
            "<script>alert(1)</script>",
            None,
            "http://localhost:5173",
        )
        .unwrap();
        let msg = parse(&built.raw);
        let html = msg.body_html(0).unwrap().into_owned();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn built_message_id_is_on_the_system_sender_domain() {
        let built = build_staff_mime(
            &instance(),
            &ticket(db::TicketStatus::Open, None),
            &recipient(),
            "headline",
            None,
            "http://localhost:5173",
        )
        .unwrap();
        let msg = parse(&built.raw);
        let message_id = msg.message_id().expect("Message-ID present");
        let system_from = mail::system_from();
        let expected_domain = system_from.rsplit_once('@').map(|(_, d)| d).unwrap();
        assert!(message_id.ends_with(expected_domain), "{message_id}");
    }
}
