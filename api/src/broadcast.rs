//! Broadcast email — pure validation and MIME construction, no I/O.
//!
//! A broadcast is one announcement sent from a support instance to a list of
//! addresses, one copy per recipient (`sendBroadcast` in
//! `graphql::mutations` is the impure caller). See CLAUDE.md's "Broadcasts"
//! entry for the rules this module encodes; in short:
//!
//! - **A reply must open a new ticket, one per reply.** The inbound pipeline
//!   threads a message onto an existing ticket only by a
//!   `+t{ticket_id}.{reply_token}` tag, an `In-Reply-To`/`References` id
//!   found in `rfc_message_id-index`, or a `[#slug-n]` subject tag
//!   (`inbound::pipeline::resolve_ticket`). A broadcast carries none of the
//!   three: [`reply_to_address`] is a plain inbound address (no tag, and a
//!   `+` in the From address is refused outright), the broadcast's
//!   Message-ID is never stored on a `ticket_message` row, and a subject that
//!   parses as `[#slug-n]` is refused. Every reply therefore falls through to
//!   "new ticket" for its sender — and since each recipient gets their own
//!   copy, addressed to them alone, two people replying can never land on
//!   the same ticket.
//! - **Auto-responders are asked to stay quiet** ([`build_broadcast_message`]'s
//!   headers), and an auto-reply that ignores them is still dropped by
//!   `mailloop::check_structural` when it arrives marked as one.

use std::collections::HashSet;

use anyhow::{Context, Result, anyhow, bail};
use mail_builder::MessageBuilder;
use mail_builder::headers::raw::Raw;

use crate::db;
use crate::outbound::{self, BuiltMessage};

/// Most recipients one broadcast may have. Sending is inline in the API
/// Lambda (30 s timeout); 100 sends at [`SEND_CONCURRENCY`] in flight
/// finishes in a few seconds, with plenty of headroom.
pub const MAX_BROADCAST_RECIPIENTS: usize = 100;

/// How many sends `sendBroadcast` keeps in flight at once — enough to finish
/// [`MAX_BROADCAST_RECIPIENTS`] quickly, well under SES's default
/// per-second sending rate.
pub const SEND_CONCURRENCY: usize = 5;

const MAX_SUBJECT_CHARS: usize = 200;
const MAX_BODY_CHARS: usize = 50_000;
const MAX_FROM_NAME_CHARS: usize = 100;

/// SES message-tag names carried by every broadcast send and echoed back on
/// every delivery event — see `broadcast_events`.
pub const TAG_BROADCAST_ID: &str = "broadcast_id";
pub const TAG_BROADCAST_RECIPIENT: &str = "broadcast_recipient";

/// What the caller asked for, unvalidated.
#[derive(Debug, Clone, Copy)]
pub struct BroadcastInput<'a> {
    pub from_address: &'a str,
    pub from_name: Option<&'a str>,
    pub subject: &'a str,
    pub body: &'a str,
    pub recipients: &'a [String],
}

/// A broadcast that passed [`validate`] — everything needed to build and
/// send each copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidBroadcast {
    pub from_address: String,
    pub from_name: String,
    pub reply_to: String,
    pub subject: String,
    pub body: String,
    pub recipients: Vec<String>,
}

/// The domains this instance's inbound addresses live on, sorted and
/// deduplicated — the domains a broadcast may be sent from. An exact row
/// contributes its domain part; a `*@domain` wildcard its domain.
pub fn instance_mail_domains(addresses: &[db::InboundAddress]) -> Vec<String> {
    let mut domains: Vec<String> = addresses
        .iter()
        .filter_map(|a| a.address.rsplit_once('@').map(|(_, d)| d.to_string()))
        .filter(|d| !d.is_empty())
        .collect();
    domains.sort();
    domains.dedup();
    domains
}

/// Where a reply to a broadcast sent from `from` should go: an address that
/// routes back to this instance, so the reply becomes a ticket here.
///
/// `from` itself when it does (an exact inbound address, or any address on a
/// wildcard domain); otherwise the earliest-created exact inbound address on
/// the same domain (an instance with only `support@x` configured, sending as
/// `news@x`); otherwise the instance's primary inbound address. Never
/// carries a `+t…` reply tag — see this module's doc comment.
pub fn reply_to_address(addresses: &[db::InboundAddress], from: &str) -> Option<String> {
    if outbound::is_own_address(addresses, from) {
        return Some(from.to_string());
    }
    let domain = from.rsplit_once('@').map(|(_, d)| d)?;
    let same_domain = addresses
        .iter()
        .filter(|a| a.kind == db::AddressKind::Exact)
        .filter(|a| a.address.rsplit_once('@').map(|(_, d)| d) == Some(domain))
        .min_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.address.cmp(&b.address))
        });
    if let Some(a) = same_domain {
        return Some(a.address.clone());
    }
    outbound::primary_inbound_address(addresses)
}

/// Validate and normalize a broadcast request against the instance's
/// inbound addresses. Errors are user-facing messages.
pub fn validate(
    input: BroadcastInput<'_>,
    instance: &db::Instance,
    addresses: &[db::InboundAddress],
) -> Result<ValidBroadcast> {
    // From: a plain address on one of the instance's own domains.
    let from_address = crate::graphql::mutations::normalize_ticket_email(input.from_address)?;
    let (local, domain) = from_address
        .split_once('@')
        .expect("normalize_ticket_email guarantees an '@'");
    if local.contains('+') {
        // A `+tag` could be shaped like a ticket reply tag, threading every
        // reply onto that ticket instead of opening new ones.
        bail!("the From address cannot contain '+'");
    }
    if local == "*" || !local.chars().all(is_plain_local_char) {
        bail!("{from_address:?} is not a valid From address");
    }
    let domains = instance_mail_domains(addresses);
    if !domains.iter().any(|d| d == domain) {
        if domains.is_empty() {
            bail!("this instance has no inbound address, so it has no domain to send from");
        }
        bail!(
            "the From address must be on this instance's domain ({})",
            domains.join(", ")
        );
    }
    let reply_to = reply_to_address(addresses, &from_address)
        .ok_or_else(|| anyhow!("this instance has no inbound address for replies to go to"))?;

    let from_name = match input.from_name.map(str::trim) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => instance.from_name.trim().to_string(),
    };
    check_single_line("From name", &from_name, MAX_FROM_NAME_CHARS)?;

    let subject = input.subject.trim().to_string();
    if subject.is_empty() {
        bail!("subject cannot be empty");
    }
    check_single_line("subject", &subject, MAX_SUBJECT_CHARS)?;
    if outbound::parse_subject_tag(&subject).is_some() {
        // A reply would thread onto that ticket rather than open a new one.
        bail!("the subject cannot contain a ticket reference like [#slug-123]");
    }

    let body = input.body.trim().to_string();
    if body.is_empty() {
        bail!("body cannot be empty");
    }
    if body.chars().count() > MAX_BODY_CHARS {
        bail!("body is too long (at most {MAX_BODY_CHARS} characters)");
    }

    let recipients = normalize_recipients(input.recipients, addresses)?;

    Ok(ValidBroadcast {
        from_address,
        from_name,
        reply_to,
        subject,
        body,
        recipients,
    })
}

/// Normalize, dedupe (first-seen order), and cap the recipient list, and
/// refuse any of the instance's own inbound addresses — a broadcast mailing
/// its own support address would just open a ticket from itself.
pub fn normalize_recipients(
    raw: &[String],
    addresses: &[db::InboundAddress],
) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(raw.len());
    let mut seen: HashSet<String> = HashSet::new();
    for r in raw {
        if r.trim().is_empty() {
            continue;
        }
        let email = crate::graphql::mutations::normalize_ticket_email(r)?;
        if outbound::is_own_address(addresses, &email) {
            bail!("{email:?} is one of this instance's own inbound addresses");
        }
        if seen.insert(email.clone()) {
            out.push(email);
        }
    }
    if out.is_empty() {
        bail!("add at least one recipient");
    }
    if out.len() > MAX_BROADCAST_RECIPIENTS {
        bail!(
            "too many recipients: {} exceeds the limit of {MAX_BROADCAST_RECIPIENTS}",
            out.len()
        );
    }
    Ok(out)
}

fn is_plain_local_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

fn check_single_line(label: &str, value: &str, max_chars: usize) -> Result<()> {
    if value.chars().any(char::is_control) {
        bail!("{label} cannot contain line breaks or control characters");
    }
    if value.chars().count() > max_chars {
        bail!("{label} is too long (at most {max_chars} characters)");
    }
    Ok(())
}

/// Build one recipient's copy: `To:` that recipient only (no `Cc`, so a
/// reply-all can't pull the rest of the list in), `From` the chosen address
/// under the chosen name, `Reply-To` [`reply_to_address`], the subject
/// exactly as entered (no `[#…]` tag), and the instance signature appended.
///
/// Headers asking auto-responders not to answer: `Auto-Submitted:
/// auto-generated` (RFC 3834 — a compliant responder must not reply),
/// `X-Auto-Response-Suppress: OOF, AutoReply` (Exchange/Outlook), and
/// `Precedence: bulk` (older vacation programs). Plus `X-Toolbox-Loop`, like
/// every other message this system sends. A human reply doesn't copy any of
/// these, so it still passes `mailloop::check_structural`.
pub fn build_broadcast_message(
    broadcast: &ValidBroadcast,
    signature: &str,
    recipient: &str,
) -> Result<BuiltMessage> {
    let domain = broadcast
        .from_address
        .rsplit_once('@')
        .map(|(_, d)| d)
        .unwrap_or("toolbox.invalid");
    // Explicit, for the same reason as `outbound::build_outbound`: left
    // unset, mail-builder derives one from the local hostname.
    let message_id = format!("{}@{domain}", crate::nonce::generate_nonce(16));
    let raw = MessageBuilder::new()
        .from((
            broadcast.from_name.as_str(),
            broadcast.from_address.as_str(),
        ))
        .reply_to(broadcast.reply_to.as_str())
        .to(recipient)
        .subject(broadcast.subject.as_str())
        .message_id(message_id)
        .header("Auto-Submitted", Raw::new("auto-generated"))
        .header("X-Auto-Response-Suppress", Raw::new("OOF, AutoReply"))
        .header("Precedence", Raw::new("bulk"))
        .header("X-Toolbox-Loop", Raw::new("1"))
        .text_body(outbound::render_text(&broadcast.body, signature))
        .html_body(outbound::render_html(&broadcast.body, signature))
        .write_to_vec()
        .context("building broadcast MIME message")?;
    Ok(BuiltMessage {
        raw,
        to: vec![recipient.to_string()],
        cc: Vec::new(),
    })
}

/// The SES message tags for recipient `index` of broadcast `broadcast_id`.
pub fn send_tags(broadcast_id: &str, index: usize) -> Vec<(String, String)> {
    vec![
        (TAG_BROADCAST_ID.to_string(), broadcast_id.to_string()),
        (TAG_BROADCAST_RECIPIENT.to_string(), index.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbound::routing;

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
            name: "seslogin".to_string(),
            slug: "seslogin".to_string(),
            kind: db::InstanceKind::Support,
            public_submission_enabled: false,
            from_name: "seslogin Support".to_string(),
            signature: "Thanks,\nThe seslogin team".to_string(),
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

    fn exact_only() -> Vec<db::InboundAddress> {
        vec![addr("support@seslogin.test", db::AddressKind::Exact, 1)]
    }

    fn input<'a>(from: &'a str, recipients: &'a [String]) -> BroadcastInput<'a> {
        BroadcastInput {
            from_address: from,
            from_name: None,
            subject: "New in seslogin: location check-in",
            body: "Hello!\nWe've shipped something.",
            recipients,
        }
    }

    fn emails(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    // ── From / Reply-To ──────────────────────────────────────────────────

    #[test]
    fn mail_domains_cover_exact_and_wildcard_rows_deduplicated() {
        let addresses = vec![
            addr("support@a.test", db::AddressKind::Exact, 1),
            addr("billing@a.test", db::AddressKind::Exact, 2),
            addr("*@b.test", db::AddressKind::Wildcard, 3),
        ];
        assert_eq!(instance_mail_domains(&addresses), vec!["a.test", "b.test"]);
    }

    #[test]
    fn any_local_part_on_the_instance_domain_is_accepted() {
        let r = emails(&["a@example.com"]);
        let v = validate(input("News@SESLOGIN.test", &r), &instance(), &exact_only()).unwrap();
        assert_eq!(v.from_address, "news@seslogin.test");
    }

    #[test]
    fn a_foreign_domain_is_refused() {
        let r = emails(&["a@example.com"]);
        let err = validate(input("news@example.org", &r), &instance(), &exact_only()).unwrap_err();
        assert!(err.to_string().contains("seslogin.test"), "{err}");
    }

    #[test]
    fn a_plus_in_the_from_address_is_refused() {
        let r = emails(&["a@example.com"]);
        let err = validate(
            input("support+tABC.tok@seslogin.test", &r),
            &instance(),
            &exact_only(),
        )
        .unwrap_err();
        assert!(err.to_string().contains('+'), "{err}");
    }

    #[test]
    fn reply_to_is_the_from_address_when_it_routes_back() {
        assert_eq!(
            reply_to_address(&exact_only(), "support@seslogin.test").as_deref(),
            Some("support@seslogin.test")
        );
        let wildcard = vec![addr("*@seslogin.test", db::AddressKind::Wildcard, 1)];
        assert_eq!(
            reply_to_address(&wildcard, "news@seslogin.test").as_deref(),
            Some("news@seslogin.test")
        );
    }

    #[test]
    fn reply_to_falls_back_to_an_exact_address_on_the_same_domain() {
        let addresses = vec![
            addr("help@other.test", db::AddressKind::Exact, 1),
            addr("support@seslogin.test", db::AddressKind::Exact, 2),
        ];
        assert_eq!(
            reply_to_address(&addresses, "news@seslogin.test").as_deref(),
            Some("support@seslogin.test")
        );
    }

    #[test]
    fn reply_to_never_carries_a_tag_and_always_routes_to_the_instance() {
        let r = emails(&["a@example.com"]);
        let v = validate(input("news@seslogin.test", &r), &instance(), &exact_only()).unwrap();
        assert_eq!(v.reply_to, "support@seslogin.test");
        let n = routing::normalize_recipient(&v.reply_to);
        assert_eq!(n.tag, None);
        assert!(outbound::is_own_address(&exact_only(), &v.reply_to));
    }

    #[test]
    fn from_name_defaults_to_the_instance_from_name() {
        let r = emails(&["a@example.com"]);
        let v = validate(input("news@seslogin.test", &r), &instance(), &exact_only()).unwrap();
        assert_eq!(v.from_name, "seslogin Support");
        let mut i = input("news@seslogin.test", &r);
        i.from_name = Some("  The seslogin team ");
        let v = validate(i, &instance(), &exact_only()).unwrap();
        assert_eq!(v.from_name, "The seslogin team");
    }

    // ── subject / body ───────────────────────────────────────────────────

    #[test]
    fn a_subject_with_a_ticket_tag_is_refused() {
        let r = emails(&["a@example.com"]);
        let mut i = input("news@seslogin.test", &r);
        i.subject = "Update on [#seslogin-12]";
        assert!(validate(i, &instance(), &exact_only()).is_err());
    }

    #[test]
    fn a_subject_with_a_line_break_is_refused() {
        let r = emails(&["a@example.com"]);
        let mut i = input("news@seslogin.test", &r);
        i.subject = "Hello\r\nBcc: everyone@example.com";
        assert!(validate(i, &instance(), &exact_only()).is_err());
    }

    #[test]
    fn empty_subject_or_body_is_refused() {
        let r = emails(&["a@example.com"]);
        let mut i = input("news@seslogin.test", &r);
        i.subject = "   ";
        assert!(validate(i, &instance(), &exact_only()).is_err());
        let mut i = input("news@seslogin.test", &r);
        i.body = "\n";
        assert!(validate(i, &instance(), &exact_only()).is_err());
    }

    // ── recipients ───────────────────────────────────────────────────────

    #[test]
    fn recipients_are_normalized_and_deduplicated_in_order() {
        let r = emails(&["B@example.com", " a@example.com ", "b@EXAMPLE.com", ""]);
        assert_eq!(
            normalize_recipients(&r, &exact_only()).unwrap(),
            vec!["b@example.com", "a@example.com"]
        );
    }

    #[test]
    fn an_own_address_recipient_is_refused() {
        let r = emails(&["a@example.com", "Support@seslogin.test"]);
        assert!(normalize_recipients(&r, &exact_only()).is_err());
    }

    #[test]
    fn recipients_are_capped() {
        let ok: Vec<String> = (0..MAX_BROADCAST_RECIPIENTS)
            .map(|i| format!("r{i}@example.com"))
            .collect();
        assert_eq!(normalize_recipients(&ok, &exact_only()).unwrap().len(), 100);
        let too_many: Vec<String> = (0..=MAX_BROADCAST_RECIPIENTS)
            .map(|i| format!("r{i}@example.com"))
            .collect();
        assert!(normalize_recipients(&too_many, &exact_only()).is_err());
    }

    #[test]
    fn no_recipients_is_refused() {
        assert!(normalize_recipients(&emails(&["  "]), &exact_only()).is_err());
        assert!(normalize_recipients(&emails(&["not-an-email"]), &exact_only()).is_err());
    }

    // ── MIME ─────────────────────────────────────────────────────────────

    #[test]
    fn built_message_is_addressed_to_one_recipient_with_suppression_headers_and_no_tags() {
        let r = emails(&["one@example.com", "two@example.com"]);
        let v = validate(input("news@seslogin.test", &r), &instance(), &exact_only()).unwrap();
        let built = build_broadcast_message(&v, &instance().signature, "two@example.com").unwrap();
        assert_eq!(built.to, vec!["two@example.com"]);
        assert!(built.cc.is_empty());

        let msg = mail_parser::MessageParser::new().parse(&built.raw).unwrap();
        let to: Vec<_> = msg
            .to()
            .unwrap()
            .iter()
            .filter_map(|a| a.address())
            .collect();
        assert_eq!(
            to,
            vec!["two@example.com"],
            "only this recipient, never the list"
        );
        assert!(msg.cc().is_none());
        let from = msg.from().unwrap().first().unwrap();
        assert_eq!(from.address(), Some("news@seslogin.test"));
        assert_eq!(from.name(), Some("seslogin Support"));
        assert_eq!(
            msg.reply_to().unwrap().first().unwrap().address(),
            Some("support@seslogin.test")
        );
        assert_eq!(msg.subject(), Some("New in seslogin: location check-in"));
        assert_eq!(outbound::parse_subject_tag(msg.subject().unwrap()), None);
        assert_eq!(
            msg.header_raw("Auto-Submitted").map(str::trim),
            Some("auto-generated")
        );
        assert_eq!(
            msg.header_raw("X-Auto-Response-Suppress").map(str::trim),
            Some("OOF, AutoReply")
        );
        assert_eq!(msg.header_raw("Precedence").map(str::trim), Some("bulk"));
        assert!(msg.message_id().unwrap().ends_with("@seslogin.test"));
        assert!(msg.header_raw("In-Reply-To").is_none());
        assert!(msg.header_raw("References").is_none());

        let raw = String::from_utf8_lossy(&built.raw);
        assert!(!raw.contains("+t"), "no reply tag anywhere: {raw}");
        assert!(
            msg.body_text(0)
                .unwrap()
                .contains("We've shipped something.")
        );
        assert!(msg.body_text(0).unwrap().contains("The seslogin team"));
    }

    #[test]
    fn send_tags_carry_the_broadcast_and_recipient_index() {
        assert_eq!(
            send_tags("AbC123", 7),
            vec![
                ("broadcast_id".to_string(), "AbC123".to_string()),
                ("broadcast_recipient".to_string(), "7".to_string()),
            ]
        );
    }
}
