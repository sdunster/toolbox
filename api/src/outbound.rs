//! Outbound MIME construction — pure, no I/O, no AWS.
//!
//! Mirrors [`crate::inbound::routing`]'s split: this module decides *what*
//! goes on the wire (headers, body, envelope recipients), and
//! [`crate::graphql::mutations`] is the only caller that touches the
//! database or the [`crate::mail::Handler`] trait. [`sesmail`](crate::sesmail)
//! and [`mockmail`](crate::mockmail) each get handed a [`BuiltMessage`] and
//! never construct MIME themselves.
//!
//! Every *customer-facing* outbound email in this project — an agent's
//! reply, a status-change notice, a submission acknowledgement — goes
//! through the single [`build_outbound`] function, so there is exactly one
//! place that can get a required header wrong. Staff (member) email
//! notifications are a deliberately separate pipeline,
//! [`crate::staff_notify`] — see that module's doc comment for why a staff
//! notice can't just be another `build_outbound` call (in short: it must
//! never carry the `+t{ticket_id}.{reply_token}` reply tag this module
//! stamps on every message below, or a staff member's own reply could be
//! mistaken for a customer message).
//!
//! **The step 6/7 contract**: [`reply_to_address`] builds the `Reply-To`
//! this module stamps on every outbound message, and it must parse back to
//! the same instance, ticket, and reply token through
//! `inbound::routing`'s `+tag` stripping and `inbound::resolution`'s tag
//! parsing (step 7). [`tests::reply_to_address_round_trips_through_inbound_routing`]
//! is that promise, pinned. If it ever breaks, a reply silently becomes a
//! new ticket instead of threading onto the existing one.
//!
//! **Tag format: `+t{ticket_id}.{reply_token}`, not the originally-planned
//! `+t{reply_token}`.** The build plan specified the reply tag as bare
//! `+t{reply_token}`, looked up by scanning for a ticket with that token —
//! but `reply_token` has no GSI, and adding one would make resolution depend
//! on an *eventually consistent* index: an auto-reply arriving a second
//! after a ticket is created could race the index and wrongly open a second
//! ticket instead of threading onto the first. Carrying `ticket_id` in the
//! tag turns resolution into a strongly-consistent `GetItem` on the ticket's
//! primary key, with no GSI and no consistency window, followed by a
//! constant-time comparison of `reply_token` (`inbound::resolution::constant_time_eq`)
//! — which remains the actual security boundary: without it, anyone able to
//! guess or enumerate a 12-char ticket id could email into someone else's
//! ticket. See `CLAUDE.md` and `SCHEMA.md` for the same note.

use anyhow::{Context, Result, anyhow, bail};
use mail_builder::MessageBuilder;
use mail_builder::headers::raw::Raw;

use crate::db;
use crate::inbound::routing;

/// A built outbound message: the raw RFC 5322 bytes, plus the envelope
/// recipients it must be sent to.
///
/// `to`/`cc` are not re-derived from the raw bytes by
/// [`crate::mail::Handler::send_raw`] implementations — they're handed over
/// explicitly so `sesmail`/`mockmail` never need a MIME parser just to know
/// who to give SES's `Destination` to, and so `MAIL_OVERRIDE_TO` (applied by
/// the backend to these fields, not to the raw bytes) redirects delivery
/// without having to rewrite the message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltMessage {
    pub raw: Vec<u8>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
}

/// `In-Reply-To`/`References` for a new outbound message on a ticket,
/// derived from the last message that actually carries an `rfc_message_id`.
///
/// Not literally "the last row by `created_at`": an internal note never has
/// one (notes aren't emailed), and neither does a reply whose send failed
/// (see `graphql::mutations::reply_to_ticket`'s doc comment for why that row
/// is still persisted). Skipping those and chaining from the last message
/// that *is* actually part of the RFC 5322 thread is what keeps the chain
/// unbroken across either case.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Threading {
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
}

/// Compute [`Threading`] from a ticket's messages, oldest-first (as
/// [`crate::db::Handler::list_ticket_messages`] returns them).
pub fn threading_for(messages: &[db::TicketMessage]) -> Threading {
    let Some(last) = messages.iter().rev().find(|m| m.rfc_message_id.is_some()) else {
        return Threading::default();
    };
    let rid = last
        .rfc_message_id
        .clone()
        .expect("checked present by find() above");
    let references = match last.references.as_deref() {
        Some(existing) if !existing.trim().is_empty() => format!("{existing} {rid}"),
        _ => rid.clone(),
    };
    Threading {
        in_reply_to: Some(rid),
        references: Some(references),
    }
}

/// Choose the concrete address this instance's outbound mail is sent from,
/// given all of its `inbound_address` rows.
///
/// An **exact** address is preferred — deterministically the
/// earliest-created, ties broken alphabetically by address, so this is
/// stable regardless of what order `list_inbound_addresses_by_instance`
/// happens to return rows in. If the instance has only wildcard addresses
/// (`*@domain`), there is no literal deliverable address configured for it
/// at all; by convention this synthesizes `support@{domain}` on the
/// earliest-created wildcard's domain. That is not an arbitrary made-up
/// address: `support+t{ticket_id}.{token}@{domain}` still round-trips correctly through
/// `inbound::routing` (no exact match, falls through to the `*@domain`
/// wildcard row), so a reply sent from it threads back exactly as if a real
/// `support@{domain}` row existed. An instance with no inbound addresses at
/// all has nothing to send from, so this returns `None`.
pub fn primary_inbound_address(addresses: &[db::InboundAddress]) -> Option<String> {
    let earliest = |kind: db::AddressKind| {
        addresses.iter().filter(|a| a.kind == kind).min_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.address.cmp(&b.address))
        })
    };
    if let Some(exact) = earliest(db::AddressKind::Exact) {
        return Some(exact.address.clone());
    }
    let wildcard = earliest(db::AddressKind::Wildcard)?;
    let domain = wildcard.address.strip_prefix("*@")?;
    Some(format!("support@{domain}"))
}

/// `{local}+t{ticket_id}.{reply_token}@{domain}`, built from a concrete
/// `from_address` (as returned by [`primary_inbound_address`]) — the
/// `Reply-To` every outbound message carries, and the step 6/7 contract this
/// module's doc comment describes.
pub fn reply_to_address(from_address: &str, ticket_id: &str, reply_token: &str) -> Result<String> {
    let (local, domain) = from_address
        .split_once('@')
        .ok_or_else(|| anyhow!("{from_address:?} is not a valid address (missing '@')"))?;
    Ok(format!("{local}+t{ticket_id}.{reply_token}@{domain}"))
}

/// Whether `candidate` is one of this instance's own inbound addresses —
/// an exact match, or the same domain as one of its wildcards. Used to
/// exclude our own addresses from `To`/`Cc` so a ticket can never mail
/// itself into a loop, however it ended up on the requester/CC list.
///
/// `pub(crate)`, not private: `graphql::mutations::submit_verified_ticket`
/// reuses this to reject a caller-supplied `to`/`cc` address that is itself
/// one of the instance's own inbound addresses — see that mutation's doc
/// comment for why silently filtering it (the way [`build_outbound`] does
/// for `To`/`Cc`) would be the wrong behaviour there.
pub(crate) fn is_own_address(addresses: &[db::InboundAddress], candidate: &str) -> bool {
    let n = routing::normalize_recipient(candidate);
    addresses.iter().any(|a| match a.kind {
        db::AddressKind::Exact => a.address == n.address,
        db::AddressKind::Wildcard => a.address.strip_prefix("*@") == n.domain.as_deref(),
    })
}

/// `[#{slug}-{number}]` parsed back out of the front of a subject line, if
/// present — the pure counterpart to [`tag_subject`], and the threading
/// fallback step 7 needs when `In-Reply-To`/`References` are both missing.
///
/// The slug/number split happens at the tag's *last* `-`, so a hyphenated
/// slug (`[#ridge-line-42]`) still parses as slug `ridge-line`, number `42`
/// rather than failing or splitting in the wrong place.
pub fn parse_subject_tag(subject: &str) -> Option<(String, u64)> {
    // The tag is searched for *anywhere* in the subject, not just at the
    // start. A customer's reply almost never keeps it leading: mail clients
    // prepend `Re:`, `Fwd:`, `AW:`, `Re[2]:` and worse, and a human editing a
    // subject line will happily leave the tag mid-string. Anchoring this to
    // the start would mean the subject-tag fallback never fired for the most
    // common shape of reply there is, silently, with replies opening
    // duplicate tickets instead of threading.
    //
    // A false positive (a subject that merely happens to contain something
    // shaped like a tag) is bounded: the number still has to match a real
    // ticket in the resolved instance, and a tag naming another tenant's
    // ticket is rejected by the caller.
    let mut rest = subject;
    while let Some(start) = rest.find("[#") {
        let inner = &rest[start + 2..];
        if let Some((tag, _)) = inner.split_once(']')
            && let Some((slug, number)) = tag.rsplit_once('-')
            && !slug.is_empty()
            && let Ok(number) = number.parse()
        {
            return Some((slug.to_string(), number));
        }
        rest = inner;
    }
    None
}

/// `Subject: [#{slug}-{number}] {subject}` — without double-tagging. If
/// `subject` already begins with a parseable tag (the customer replied with
/// it still in place, or an agent left a previous tag in a follow-up
/// subject), it's returned unchanged rather than gaining a second tag.
pub fn tag_subject(slug: &str, number: u64, subject: &str) -> String {
    if parse_subject_tag(subject).is_some() {
        return subject.to_string();
    }
    format!("{} {}", db::ticket_subject_tag(slug, number), subject)
}

/// Body text for the acknowledgement `submitTicket` sends the requester —
/// see `graphql::mutations::submit_ticket`. A constant, not a template: the
/// copy is deliberately short and generic (no per-instance customization in
/// v1), so there is exactly one string to keep in sync with what the public
/// submit form itself tells the requester happens next.
pub const ACKNOWLEDGEMENT_BODY: &str =
    "Thanks for reaching out. We've received your message and will follow up here.";

/// Body text for the notice `setTicketStatus` sends requesters/CCs on a
/// close or reopen, or `None` when `new_status` shouldn't be mailed at all.
///
/// **Only close/reopen mail — assignment, requester/CC edits, and deletion
/// do not.** This is a documented product decision (see `CLAUDE.md`'s
/// "Outbound mail: which ticket updates email" entry): assignment and
/// requester/CC edits are internal bookkeeping that a customer has no
/// reason to be notified about, and deleting a ticket is the same kind of
/// admin housekeeping (spam cleanup, a mistakenly-opened ticket) — not a
/// resolution a customer is waiting on, so `Deleted` is excluded in either
/// direction (closing *into* delete, or restoring *out of* it) even though
/// both go through this same mutation.
pub fn status_notice_body(
    old_status: db::TicketStatus,
    new_status: db::TicketStatus,
) -> Option<&'static str> {
    use db::TicketStatus::{Closed, Open};
    match new_status {
        Closed => Some(
            "This ticket has been marked closed. Reply to this email if you need to reopen it.",
        ),
        Open if old_status != Open => Some("This ticket has been reopened — we're back on it."),
        _ => None,
    }
}

/// Minimal HTML escaping — the four characters that matter inside `<div>`
/// text content. Matches `seslogin`'s `activity_summary::escape_html`.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Hand-built HTML body — no template engine, matching seslogin's
/// `activity_summary::build_summary_html` pattern. `body` is escaped and its
/// newlines converted to `<br>`; the instance's signature, if set, is
/// escaped the same way and appended in a visually distinct block below a
/// divider.
fn render_html(body: &str, signature: &str) -> String {
    let mut html = String::from(
        "<!DOCTYPE html>\n<html><body style=\"font-family:Arial,Helvetica,sans-serif;color:#222\">\n",
    );
    html.push_str(&format!(
        "<div>{}</div>\n",
        escape_html(body).replace('\n', "<br>\n")
    ));
    if !signature.trim().is_empty() {
        html.push_str(&format!(
            "<div style=\"margin-top:1em;padding-top:0.5em;border-top:1px solid #e5e7eb;color:#555\">{}</div>\n",
            escape_html(signature).replace('\n', "<br>\n")
        ));
    }
    html.push_str("</body></html>");
    html
}

/// Plain-text counterpart to [`render_html`]: the body, with the instance's
/// signature (if set) appended after a blank line — no escaping needed for
/// plain text.
fn render_text(body: &str, signature: &str) -> String {
    if signature.trim().is_empty() {
        body.to_string()
    } else {
        format!("{body}\n\n{signature}")
    }
}

/// Build one outbound message on `ticket`: `To: requester_emails`,
/// `Cc: cc_emails`, with this instance's own inbound addresses excluded
/// from both (see [`is_own_address`]); `From` the instance's primary
/// inbound address under its `from_name`; `Reply-To` carrying the `+t{token}`
/// threading address; `Subject` tagged `[#{slug}-{number}]`;
/// `X-Toolbox-Loop: 1`; `In-Reply-To`/`References` from `threading`; and
/// a `multipart/alternative` plain-text + HTML body with the instance's
/// signature appended.
#[allow(clippy::too_many_arguments)]
pub fn build_outbound(
    instance: &db::Instance,
    addresses: &[db::InboundAddress],
    ticket: &db::Ticket,
    requester_emails: &[String],
    cc_emails: &[String],
    body: &str,
    threading: &Threading,
) -> Result<BuiltMessage> {
    let from_address = primary_inbound_address(addresses).ok_or_else(|| {
        anyhow!(
            "instance {:?} has no inbound address configured to send outbound mail from",
            instance.id
        )
    })?;
    let reply_to = reply_to_address(&from_address, &ticket.id, &ticket.reply_token)?;

    let to: Vec<String> = requester_emails
        .iter()
        .filter(|e| !is_own_address(addresses, e))
        .cloned()
        .collect();
    let cc: Vec<String> = cc_emails
        .iter()
        .filter(|e| !is_own_address(addresses, e))
        .cloned()
        .collect();
    if to.is_empty() && cc.is_empty() {
        bail!(
            "ticket {:?} has no recipients left once this instance's own inbound addresses are excluded",
            ticket.id
        );
    }

    let subject = tag_subject(&instance.slug, ticket.number, &ticket.subject);
    let html = render_html(body, &instance.signature);
    let text = render_text(body, &instance.signature);

    // Set Message-ID explicitly. Left unset, mail-builder synthesises one from the
    // local hostname — which puts the sending machine's private hostname into a
    // header on every outbound message. SES overwrites Message-ID on send anyway
    // (which is why `sesmail` returns the rewritten form for us to store), so this
    // value is only what a local `MOCK_MAIL_DIR` message carries; it should still
    // leak nothing. The instance's own mail domain is the honest choice.
    let from_domain = from_address
        .rsplit_once('@')
        .map(|(_, d)| d)
        .unwrap_or("toolbox.invalid");
    let message_id = format!("{}@{}", crate::nonce::generate_nonce(16), from_domain);

    let mut builder = MessageBuilder::new()
        .from((instance.from_name.as_str(), from_address.as_str()))
        .reply_to(reply_to.as_str())
        .subject(subject)
        .message_id(message_id)
        .header("X-Toolbox-Loop", Raw::new("1"))
        .text_body(text)
        .html_body(html);
    if !to.is_empty() {
        builder = builder.to(to.clone());
    }
    if !cc.is_empty() {
        builder = builder.cc(cc.clone());
    }
    // Written with the generic `.header()`/`Raw` escape hatch, not the typed
    // `.in_reply_to()`/`.references()` helpers: those wrap their value in a
    // fresh pair of `<...>`, but the ids stored on `ticket_message` (and
    // therefore in `threading`) already carry the brackets SES's rewritten
    // Message-ID uses (see `sesmail`'s doc comment) — passing them through
    // `.in_reply_to()` would double-wrap them into `<<...>>`.
    if let Some(irt) = &threading.in_reply_to {
        builder = builder.header("In-Reply-To", Raw::new(irt.clone()));
    }
    if let Some(refs) = &threading.references {
        builder = builder.header("References", Raw::new(refs.clone()));
    }

    let raw = builder
        .write_to_vec()
        .context("building outbound MIME message")?;

    Ok(BuiltMessage { raw, to, cc })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(address: &str, kind: db::AddressKind, created_at: u64) -> db::InboundAddress {
        db::InboundAddress {
            address: address.to_string(),
            instance_id: "inst1".to_string(),
            kind,
            created_at,
        }
    }

    fn instance() -> db::Instance {
        db::Instance {
            id: "inst1".to_string(),
            name: "Acme".to_string(),
            slug: "acme".to_string(),
            kind: db::InstanceKind::Support,
            public_submission_enabled: false,
            from_name: "Acme Support".to_string(),
            signature: "Thanks,\nAcme Support".to_string(),
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

    fn ticket() -> db::Ticket {
        db::Ticket {
            id: "tick1".to_string(),
            instance_id: "inst1".to_string(),
            number: 42,
            subject: "Where is my order?".to_string(),
            status: db::TicketStatus::Open,
            requester_emails: vec!["requester@example.com".to_string()],
            cc_emails: vec!["cc@example.com".to_string()],
            assignee_user_id: None,
            reply_token: "abc123token".to_string(),
            created_at: 1_000,
            updated_at: 1_000,
            last_activity_at: 1_000,
            has_attachments: false,
        }
    }

    fn message(rfc_message_id: Option<&str>, references: Option<&str>) -> db::TicketMessage {
        db::TicketMessage {
            id: "msg1".to_string(),
            ticket_id: "tick1".to_string(),
            kind: db::TicketMessageKind::Inbound,
            author_user_id: None,
            from_email: Some("requester@example.com".to_string()),
            to_emails: vec![],
            cc_emails: vec![],
            body_text: Some("hello".to_string()),
            body_html: None,
            rfc_message_id: rfc_message_id.map(String::from),
            in_reply_to: None,
            references: references.map(String::from),
            attachments: vec![],
            raw_s3_key: None,
            created_at: 1_000,
        }
    }

    // ── the step 6/7 contract ────────────────────────────────────────────

    /// The one test this module's doc comment calls out by name: the
    /// `Reply-To` this module builds must parse back to the same instance
    /// (via its lookup key) and the same reply token through
    /// `inbound::routing`'s own `+tag` stripping. If this ever fails, a
    /// customer's reply silently opens a new ticket instead of threading
    /// onto the one it replied to.
    #[test]
    fn reply_to_address_round_trips_through_inbound_routing() {
        use crate::inbound::resolution;

        let from = "support@acme.toolbox.test";
        let reply_to = reply_to_address(from, "TickET12345A", "abc123TOKEN").unwrap();
        assert_eq!(
            reply_to,
            "support+tTickET12345A.abc123TOKEN@acme.toolbox.test"
        );

        let n = routing::normalize_recipient(&reply_to);
        assert_eq!(
            n.address, from,
            "must strip back to the exact inbound address"
        );
        assert_eq!(
            n.tag.as_deref(),
            Some("tTickET12345A.abc123TOKEN"),
            "the tag must carry the ticket id and reply token behind a 't' marker, case intact"
        );
        let parsed = resolution::parse_reply_tag(n.tag.as_deref().unwrap())
            .expect("tag must parse as a reply tag");
        assert_eq!(parsed.ticket_id, "TickET12345A");
        assert_eq!(parsed.reply_token, "abc123TOKEN");

        // And it must resolve through the real routing table, not just
        // parse cleanly: build a matcher over exactly the address we sent
        // from, and confirm `resolve` finds it.
        let known: std::collections::HashSet<&str> = [from].into_iter().collect();
        let hit = routing::resolve(&[reply_to.as_str()], |k| known.contains(k));
        assert_eq!(hit.as_deref(), Some(from));
    }

    /// Same contract, for an instance whose only inbound address is a
    /// wildcard — the case `primary_inbound_address`'s doc comment explains.
    #[test]
    fn reply_to_address_round_trips_for_a_wildcard_only_instance() {
        let addresses = [addr(
            "*@ridgeline.toolbox.test",
            db::AddressKind::Wildcard,
            1,
        )];
        let from = primary_inbound_address(&addresses).unwrap();
        assert_eq!(from, "support@ridgeline.toolbox.test");

        let reply_to = reply_to_address(&from, "tick1", "tok").unwrap();
        let known: std::collections::HashSet<&str> =
            ["*@ridgeline.toolbox.test"].into_iter().collect();
        let hit = routing::resolve(&[reply_to.as_str()], |k| known.contains(k));
        assert_eq!(hit.as_deref(), Some("*@ridgeline.toolbox.test"));
    }

    // ── primary_inbound_address ──────────────────────────────────────────

    #[test]
    fn primary_prefers_exact_over_wildcard() {
        let addresses = [
            addr("*@acme.toolbox.test", db::AddressKind::Wildcard, 1),
            addr("support@acme.toolbox.test", db::AddressKind::Exact, 2),
        ];
        assert_eq!(
            primary_inbound_address(&addresses).as_deref(),
            Some("support@acme.toolbox.test")
        );
    }

    #[test]
    fn primary_is_deterministic_among_several_exact_addresses() {
        let addresses = [
            addr("billing@acme.toolbox.test", db::AddressKind::Exact, 5),
            addr("support@acme.toolbox.test", db::AddressKind::Exact, 5),
        ];
        // Same `created_at`: tie-broken alphabetically, and stable
        // regardless of input order.
        let a = primary_inbound_address(&addresses);
        let b = primary_inbound_address(&[addresses[1].clone(), addresses[0].clone()]);
        assert_eq!(a, b);
        assert_eq!(a.as_deref(), Some("billing@acme.toolbox.test"));
    }

    #[test]
    fn primary_synthesizes_support_at_domain_for_a_wildcard_only_instance() {
        let addresses = [addr(
            "*@ridgeline.toolbox.test",
            db::AddressKind::Wildcard,
            1,
        )];
        assert_eq!(
            primary_inbound_address(&addresses).as_deref(),
            Some("support@ridgeline.toolbox.test")
        );
    }

    #[test]
    fn primary_is_none_with_no_addresses_at_all() {
        assert_eq!(primary_inbound_address(&[]), None);
    }

    // ── subject tag round trip ───────────────────────────────────────────

    #[test]
    fn subject_tag_is_found_after_a_reply_prefix() {
        // The shape that actually arrives from a customer's mail client. This
        // is the regression test for the fallback having been anchored to the
        // start of the subject, where it never matched a real reply.
        for subject in [
            "Re: [#acme-42] Printer on fire",
            "RE: [#acme-42] Printer on fire",
            "Fwd: [#acme-42] Printer on fire",
            "Re[2]: [#acme-42] Printer on fire",
            "AW: Re: [#acme-42] Printer on fire",
            "   Re: [#acme-42] Printer on fire",
        ] {
            assert_eq!(
                parse_subject_tag(subject),
                Some(("acme".to_string(), 42)),
                "failed to find the tag in {subject:?}"
            );
        }
    }

    #[test]
    fn subject_tag_skips_a_malformed_candidate_and_finds_a_later_one() {
        assert_eq!(
            parse_subject_tag("Re: [#nonsense] and [#acme-42] Printer on fire"),
            Some(("acme".to_string(), 42))
        );
    }

    #[test]
    fn subject_with_no_tag_anywhere_is_none() {
        assert_eq!(parse_subject_tag("Re: Printer on fire [not a tag]"), None);
    }

    #[test]
    fn tag_subject_does_not_double_tag_a_reply_that_kept_its_tag() {
        let already = "Re: [#acme-42] Printer on fire";
        assert_eq!(tag_subject("acme", 42, already), already);
    }

    #[test]
    fn subject_tag_round_trips() {
        let tagged = tag_subject("acme", 42, "Where is my order?");
        assert_eq!(tagged, "[#acme-42] Where is my order?");
        assert_eq!(parse_subject_tag(&tagged), Some(("acme".to_string(), 42)));
    }

    #[test]
    fn subject_tag_round_trips_with_a_hyphenated_slug() {
        let tagged = tag_subject("ridge-line", 7, "Help");
        assert_eq!(tagged, "[#ridge-line-7] Help");
        assert_eq!(
            parse_subject_tag(&tagged),
            Some(("ridge-line".to_string(), 7))
        );
    }

    #[test]
    fn tag_subject_does_not_double_tag_an_already_tagged_subject() {
        let once = tag_subject("acme", 42, "Where is my order?");
        let twice = tag_subject("acme", 42, &once);
        assert_eq!(
            once, twice,
            "re-tagging an already-tagged subject must be a no-op"
        );

        // Even a *different* ticket's tag already present must not gain a
        // second one — any parseable tag up front is left alone.
        let other_tag_present = "[#other-9] some subject".to_string();
        assert_eq!(
            tag_subject("acme", 42, &other_tag_present),
            other_tag_present
        );
    }

    #[test]
    fn subject_containing_bracket_characters_still_gets_tagged() {
        // `[urgent]` is not `[#...]`, so it isn't mistaken for a tag.
        let subject = "Re: [urgent] server down";
        let tagged = tag_subject("acme", 3, subject);
        assert_eq!(tagged, "[#acme-3] Re: [urgent] server down");
        assert_eq!(parse_subject_tag(subject), None);
    }

    #[test]
    fn malformed_tags_do_not_parse() {
        assert_eq!(parse_subject_tag("[#acme] missing a number"), None);
        assert_eq!(parse_subject_tag("[#acme-notanumber] subject"), None);
        assert_eq!(parse_subject_tag("no tag here at all"), None);
        assert_eq!(parse_subject_tag("[#-42] empty slug"), None);
    }

    #[test]
    fn subject_with_leading_whitespace_still_parses() {
        assert_eq!(
            parse_subject_tag("   [#acme-1] padded"),
            Some(("acme".to_string(), 1))
        );
    }

    // ── threading ─────────────────────────────────────────────────────────

    #[test]
    fn threading_is_empty_with_no_messages() {
        assert_eq!(threading_for(&[]), Threading::default());
    }

    #[test]
    fn threading_chains_from_the_last_message_with_an_rfc_message_id() {
        let messages = vec![
            message(Some("<first@ap-southeast-2.amazonses.com>"), None),
            message(
                Some("<second@ap-southeast-2.amazonses.com>"),
                Some("<first@ap-southeast-2.amazonses.com>"),
            ),
        ];
        let t = threading_for(&messages);
        assert_eq!(
            t.in_reply_to.as_deref(),
            Some("<second@ap-southeast-2.amazonses.com>")
        );
        assert_eq!(
            t.references.as_deref(),
            Some("<first@ap-southeast-2.amazonses.com> <second@ap-southeast-2.amazonses.com>")
        );
    }

    #[test]
    fn threading_skips_a_trailing_message_with_no_rfc_message_id() {
        // e.g. an internal note, or a reply whose send failed — neither is
        // part of the RFC 5322 thread, so threading must chain from the
        // last message that actually is.
        let messages = vec![
            message(Some("<first@ap-southeast-2.amazonses.com>"), None),
            message(None, None),
        ];
        let t = threading_for(&messages);
        assert_eq!(
            t.in_reply_to.as_deref(),
            Some("<first@ap-southeast-2.amazonses.com>")
        );
        assert_eq!(
            t.references.as_deref(),
            Some("<first@ap-southeast-2.amazonses.com>")
        );
    }

    // ── status_notice_body ───────────────────────────────────────────────

    #[test]
    fn status_notice_mails_on_close() {
        assert!(status_notice_body(db::TicketStatus::Open, db::TicketStatus::Closed).is_some());
    }

    #[test]
    fn status_notice_mails_on_reopen() {
        assert!(status_notice_body(db::TicketStatus::Closed, db::TicketStatus::Open).is_some());
        assert!(
            status_notice_body(db::TicketStatus::Deleted, db::TicketStatus::Open).is_some(),
            "restoring a deleted ticket counts as a reopen"
        );
    }

    #[test]
    fn status_notice_is_silent_on_delete_in_either_direction() {
        assert_eq!(
            status_notice_body(db::TicketStatus::Open, db::TicketStatus::Deleted),
            None
        );
        assert_eq!(
            status_notice_body(db::TicketStatus::Closed, db::TicketStatus::Deleted),
            None
        );
    }

    // ── HTML escaping ────────────────────────────────────────────────────

    #[test]
    fn render_html_escapes_a_script_tag_and_ampersand() {
        let html = render_html("<script>alert(1)</script> & friends", "");
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("&amp; friends"), "{html}");
    }

    #[test]
    fn render_html_appends_the_signature_when_set() {
        let html = render_html("body", "Thanks,\nAcme Support");
        assert!(html.contains("Thanks,"), "{html}");
        assert!(html.contains("Acme Support"), "{html}");
    }

    #[test]
    fn render_html_omits_the_signature_block_when_blank() {
        let html = render_html("body", "");
        assert_eq!(html.matches("<div").count(), 1, "{html}");
    }

    // ── build_outbound: full message shape ──────────────────────────────

    fn parse(raw: &[u8]) -> mail_parser::Message<'_> {
        mail_parser::MessageParser::new()
            .parse(raw)
            .expect("built message must parse")
    }

    #[test]
    fn build_outbound_sets_every_required_header() {
        let instance = instance();
        let addresses = [addr("support@acme.toolbox.test", db::AddressKind::Exact, 1)];
        let ticket = ticket();
        let threading = Threading {
            in_reply_to: Some("<prev@ap-southeast-2.amazonses.com>".to_string()),
            references: Some("<prev@ap-southeast-2.amazonses.com>".to_string()),
        };
        let built = build_outbound(
            &instance,
            &addresses,
            &ticket,
            &ticket.requester_emails,
            &ticket.cc_emails,
            "We're looking into it.",
            &threading,
        )
        .unwrap();

        assert_eq!(built.to, vec!["requester@example.com".to_string()]);
        assert_eq!(built.cc, vec!["cc@example.com".to_string()]);

        let msg = parse(&built.raw);

        let from = msg.from().unwrap().first().unwrap();
        assert_eq!(from.name(), Some("Acme Support"));
        assert_eq!(from.address(), Some("support@acme.toolbox.test"));

        let reply_to = msg.reply_to().unwrap().first().unwrap();
        assert_eq!(
            reply_to.address(),
            Some("support+ttick1.abc123token@acme.toolbox.test")
        );

        let to = msg.to().unwrap().first().unwrap();
        assert_eq!(to.address(), Some("requester@example.com"));
        let cc = msg.cc().unwrap().first().unwrap();
        assert_eq!(cc.address(), Some("cc@example.com"));

        assert_eq!(msg.subject(), Some("[#acme-42] Where is my order?"));

        // Message-ID must be on the instance's own mail domain. Left unset,
        // mail-builder derives one from the local hostname, putting the sending
        // machine's private hostname on every outbound message — so this pins
        // that the explicit id is still being set.
        let message_id = msg.message_id().expect("Message-ID header");
        assert!(
            message_id.ends_with("@acme.toolbox.test"),
            "Message-ID {message_id:?} should be on the instance mail domain, \
             not a hostname-derived default"
        );

        assert_eq!(msg.header_raw("X-Toolbox-Loop").map(str::trim), Some("1"));

        let in_reply_to = msg.header_raw("In-Reply-To").expect("In-Reply-To present");
        assert!(
            in_reply_to.contains("<prev@ap-southeast-2.amazonses.com>"),
            "{in_reply_to:?}"
        );
        let references = msg.header_raw("References").expect("References present");
        assert!(
            references.contains("<prev@ap-southeast-2.amazonses.com>"),
            "{references:?}"
        );

        assert_eq!(msg.text_body_count(), 1);
        assert_eq!(msg.html_body_count(), 1);
        let text = msg.body_text(0).expect("text body");
        assert!(text.contains("We're looking into it."));
        let html = msg.body_html(0).expect("html body");
        assert!(
            html.contains("We&#39;re looking into it.") || html.contains("We're looking into it.")
        );
    }

    #[test]
    fn build_outbound_excludes_our_own_addresses_from_to_and_cc() {
        let instance = instance();
        let addresses = [addr("support@acme.toolbox.test", db::AddressKind::Exact, 1)];
        let mut t = ticket();
        // Our own address snuck onto the CC list somehow (e.g. a requester
        // added it by hand) — it must never come back out on the wire.
        t.cc_emails = vec![
            "cc@example.com".to_string(),
            "support@acme.toolbox.test".to_string(),
        ];

        let built = build_outbound(
            &instance,
            &addresses,
            &t,
            &t.requester_emails,
            &t.cc_emails,
            "hello",
            &Threading::default(),
        )
        .unwrap();

        assert_eq!(built.cc, vec!["cc@example.com".to_string()]);
        assert!(!built.to.contains(&"support@acme.toolbox.test".to_string()));
        assert!(!built.cc.contains(&"support@acme.toolbox.test".to_string()));

        let raw_str = String::from_utf8_lossy(&built.raw);
        assert!(
            !raw_str.contains("support@acme.toolbox.test\n")
                && !raw_str.contains("<support@acme.toolbox.test>,")
                || raw_str.matches("support@acme.toolbox.test").count() == 1,
            "our own address must appear at most once (the From header), never in To/Cc: {raw_str}"
        );
    }

    #[test]
    fn build_outbound_excludes_a_wildcard_domain_match_from_recipients() {
        let mut instance = instance();
        instance.slug = "ridgeline".to_string();
        let addresses = [addr(
            "*@ridgeline.toolbox.test",
            db::AddressKind::Wildcard,
            1,
        )];
        let mut t = ticket();
        t.requester_emails = vec!["customer@example.com".to_string()];
        t.cc_emails = vec!["anything@ridgeline.toolbox.test".to_string()];

        let built = build_outbound(
            &instance,
            &addresses,
            &t,
            &t.requester_emails,
            &t.cc_emails,
            "hello",
            &Threading::default(),
        )
        .unwrap();

        assert_eq!(built.to, vec!["customer@example.com".to_string()]);
        assert!(built.cc.is_empty(), "wildcard-domain CC must be excluded");
    }

    #[test]
    fn build_outbound_errors_when_the_instance_has_no_inbound_address() {
        let instance = instance();
        let t = ticket();
        let err = build_outbound(
            &instance,
            &[],
            &t,
            &t.requester_emails,
            &t.cc_emails,
            "hello",
            &Threading::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no inbound address"), "{err}");
    }

    #[test]
    fn build_outbound_errors_when_no_recipients_remain() {
        let instance = instance();
        let addresses = [addr("support@acme.toolbox.test", db::AddressKind::Exact, 1)];
        let mut t = ticket();
        t.requester_emails = vec!["support@acme.toolbox.test".to_string()];
        t.cc_emails = vec![];
        let err = build_outbound(
            &instance,
            &addresses,
            &t,
            &t.requester_emails,
            &t.cc_emails,
            "hello",
            &Threading::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("no recipients"), "{err}");
    }
}
