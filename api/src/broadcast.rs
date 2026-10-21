//! Broadcast email — pure validation and MIME construction, no I/O.
//!
//! A broadcast is one announcement sent from a support instance to a list of
//! addresses, one copy per recipient. `sendBroadcast` in `graphql::mutations`
//! validates and queues it; the worker sends it in paced batches
//! (`broadcast_send`). See CLAUDE.md's "Broadcasts" entry for the rules this
//! module encodes; in short:
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

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use mail_builder::MessageBuilder;
use mail_builder::headers::raw::Raw;

use crate::broadcast_html;
use crate::db;
use crate::outbound::{self, BuiltMessage};

/// Most recipients one broadcast may have. Sending happens in the worker, a
/// batch at a time, so this is bounded by the `broadcast` row instead: every
/// recipient's status lives in one DynamoDB item (400 KB at most), and a
/// recipient map with a bounce diagnostic can reach ~500 bytes, next to a
/// body of up to 50,000 characters.
pub const MAX_BROADCAST_RECIPIENTS: usize = 500;

/// Recipients sent per batch — one worker job, one at a time.
pub const BATCH_SIZE: usize = 5;

/// Gap between batches: each batch queues the next with this SQS delay
/// (must be at most `queue::MAX_DELAY_SECS`). Slow on purpose — a steady
/// trickle is kinder to the sending domain's reputation than a burst. At
/// [`BATCH_SIZE`] every five minutes, 100 recipients take ~1h40m.
pub const BATCH_INTERVAL_SECS: u32 = 300;

// One SQS message can only be delayed so long, and an empty batch would
// never finish a broadcast.
const _: () = assert!(BATCH_INTERVAL_SECS <= crate::queue::MAX_DELAY_SECS && BATCH_SIZE > 0);

/// Most saved senders one instance may have — a short pick-list, stored as
/// one JSON attribute on the instance row.
pub const MAX_BROADCAST_SENDERS: usize = 20;

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
    /// The plain-text body — for an HTML broadcast, the optional `text/plain`
    /// alternative (blank derives one from the HTML).
    pub body: &'a str,
    /// An HTML body (`broadcast_html::process` runs on it), or `None` for a
    /// plain-text broadcast.
    pub html: Option<&'a str>,
    /// Uploaded images for the HTML's relative references: relative path →
    /// public URL (`createBroadcastAssetUpload`).
    pub asset_urls: &'a HashMap<String, String>,
    pub recipients: &'a [String],
}

/// What a broadcast is being validated for — how complete it must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Saving a draft: the recipient list may still be empty, and images may
    /// still be missing (each one is reported, not refused).
    Draft,
    /// A test copy to the owner's own addresses: the recipient list is not
    /// used, but every image must be uploaded.
    Test,
    /// The real thing: at least one recipient and no missing image.
    Send,
}

/// A broadcast that passed [`validate`] — everything needed to build and
/// send each copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidBroadcast {
    pub from_address: String,
    pub from_name: String,
    pub reply_to: String,
    pub subject: String,
    /// Plain-text body, or an HTML broadcast's own `text/plain` alternative
    /// (empty when it has none — [`build_broadcast_message`] derives one).
    pub body: String,
    /// The processed HTML body, if this is an HTML broadcast.
    pub html: Option<String>,
    /// Relative images in `html` with no uploaded file — only ever non-empty
    /// for [`Purpose::Draft`].
    pub missing_assets: Vec<String>,
    pub recipients: Vec<String>,
}

impl ValidBroadcast {
    /// The validated broadcast a stored row was created from — what the
    /// worker builds each copy with. `validate` ran before the row was
    /// written, so its fields need no re-checking.
    pub fn from_record(rec: &db::Broadcast) -> Self {
        Self {
            from_address: rec.from_address.clone(),
            from_name: rec.from_name.clone(),
            reply_to: rec.reply_to.clone(),
            subject: rec.subject.clone(),
            body: rec.body_text.clone(),
            html: rec.body_html.clone(),
            missing_assets: Vec::new(),
            recipients: rec.recipients.iter().map(|r| r.email.clone()).collect(),
        }
    }
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
/// inbound addresses, as complete as `purpose` needs it. Errors are
/// user-facing messages.
pub fn validate(
    input: BroadcastInput<'_>,
    purpose: Purpose,
    instance: &db::Instance,
    addresses: &[db::InboundAddress],
) -> Result<ValidBroadcast> {
    let from_address = validate_from_address(input.from_address, addresses)?;
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
    let (html, missing_assets) = match input.html {
        None => {
            if body.is_empty() {
                bail!("body cannot be empty");
            }
            (None, Vec::new())
        }
        Some(html) => {
            let processed = broadcast_html::process(html, input.asset_urls)?;
            if body.len() + processed.html.len() > broadcast_html::MAX_HTML_BYTES {
                bail!(
                    "the HTML and its plain-text version are too large together (at most {} KB)",
                    broadcast_html::MAX_HTML_BYTES / 1000
                );
            }
            (Some(processed.html), processed.missing_assets)
        }
    };
    if body.chars().count() > MAX_BODY_CHARS {
        bail!("body is too long (at most {MAX_BODY_CHARS} characters)");
    }
    if purpose != Purpose::Draft && !missing_assets.is_empty() {
        bail!(
            "upload the missing images first: {}",
            missing_assets.join(", ")
        );
    }

    let recipients = match purpose {
        Purpose::Send => normalize_recipients(input.recipients, addresses)?,
        Purpose::Draft | Purpose::Test => {
            if input.recipients.iter().all(|r| r.trim().is_empty()) {
                Vec::new()
            } else {
                normalize_recipients(input.recipients, addresses)?
            }
        }
    };

    Ok(ValidBroadcast {
        from_address,
        from_name,
        reply_to,
        subject,
        body,
        html,
        missing_assets,
        recipients,
    })
}

/// Normalize a From address and check it's one a broadcast may be sent
/// from: a plain local part (no `+`, no wildcard) on one of the instance's
/// own inbound-address domains. Shared by [`validate`] and
/// [`validate_sender`], so a saved sender can never hold an address
/// `sendBroadcast` would refuse at the time it was saved.
pub fn validate_from_address(raw: &str, addresses: &[db::InboundAddress]) -> Result<String> {
    let from_address = crate::graphql::mutations::normalize_ticket_email(raw)?;
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
    Ok(from_address)
}

/// Validate a new saved sender (`addBroadcastSender`) against the
/// instance's inbound addresses and the senders it already has, returning
/// the normalized `(name, address)`. The name is required here — a sender
/// is a name and an address — and an identical pair can't be saved twice.
pub fn validate_sender(
    name: &str,
    address: &str,
    addresses: &[db::InboundAddress],
    existing: &[db::BroadcastSender],
) -> Result<(String, String)> {
    let name = name.trim().to_string();
    if name.is_empty() {
        bail!("sender name cannot be empty");
    }
    check_single_line("sender name", &name, MAX_FROM_NAME_CHARS)?;
    let address = validate_from_address(address, addresses)?;
    if existing
        .iter()
        .any(|s| s.name == name && s.address == address)
    {
        bail!("{name} <{address}> is already a saved sender");
    }
    if existing.len() >= MAX_BROADCAST_SENDERS {
        bail!("at most {MAX_BROADCAST_SENDERS} saved senders per instance");
    }
    Ok((name, address))
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
        .text_body(text_part(broadcast, signature))
        .html_body(html_part(broadcast, signature))
        .write_to_vec()
        .context("building broadcast MIME message")?;
    Ok(BuiltMessage {
        raw,
        to: vec![recipient.to_string()],
        cc: Vec::new(),
    })
}

/// The `text/plain` part. A plain-text broadcast gets the instance signature
/// appended, like every other instance mail. An HTML broadcast gets the
/// owner's own alternative, or one derived from the HTML — and no signature
/// in either part: the design is sent as authored.
fn text_part(broadcast: &ValidBroadcast, signature: &str) -> String {
    match &broadcast.html {
        None => outbound::render_text(&broadcast.body, signature),
        Some(_) if !broadcast.body.is_empty() => broadcast.body.clone(),
        Some(html) => broadcast_html::html_to_text(html),
    }
}

fn html_part(broadcast: &ValidBroadcast, signature: &str) -> String {
    match &broadcast.html {
        None => outbound::render_html(&broadcast.body, signature),
        Some(html) => html.clone(),
    }
}

/// A test copy of `broadcast` for `to` (`sendBroadcastTest`): built exactly
/// like a real one, so it shows what recipients will get, with the subject
/// prefixed `[TEST]` so it can't be mistaken for the real send.
pub fn build_test_message(
    broadcast: &ValidBroadcast,
    signature: &str,
    to: &str,
) -> Result<BuiltMessage> {
    let mut test = broadcast.clone();
    test.subject = format!("[TEST] {}", broadcast.subject);
    build_broadcast_message(&test, signature, to)
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
            broadcast_senders: Vec::new(),
            brands: Vec::new(),
        }
    }

    fn exact_only() -> Vec<db::InboundAddress> {
        vec![addr("support@seslogin.test", db::AddressKind::Exact, 1)]
    }

    static NO_ASSETS: std::sync::LazyLock<HashMap<String, String>> =
        std::sync::LazyLock::new(HashMap::new);

    fn input<'a>(from: &'a str, recipients: &'a [String]) -> BroadcastInput<'a> {
        BroadcastInput {
            from_address: from,
            from_name: None,
            subject: "New in seslogin: location check-in",
            body: "Hello!\nWe've shipped something.",
            html: None,
            asset_urls: &NO_ASSETS,
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
        let v = validate(
            input("News@SESLOGIN.test", &r),
            Purpose::Send,
            &instance(),
            &exact_only(),
        )
        .unwrap();
        assert_eq!(v.from_address, "news@seslogin.test");
    }

    #[test]
    fn a_foreign_domain_is_refused() {
        let r = emails(&["a@example.com"]);
        let err = validate(
            input("news@example.org", &r),
            Purpose::Send,
            &instance(),
            &exact_only(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("seslogin.test"), "{err}");
    }

    #[test]
    fn a_plus_in_the_from_address_is_refused() {
        let r = emails(&["a@example.com"]);
        let err = validate(
            input("support+tABC.tok@seslogin.test", &r),
            Purpose::Send,
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
        let v = validate(
            input("news@seslogin.test", &r),
            Purpose::Send,
            &instance(),
            &exact_only(),
        )
        .unwrap();
        assert_eq!(v.reply_to, "support@seslogin.test");
        let n = routing::normalize_recipient(&v.reply_to);
        assert_eq!(n.tag, None);
        assert!(outbound::is_own_address(&exact_only(), &v.reply_to));
    }

    #[test]
    fn from_name_defaults_to_the_instance_from_name() {
        let r = emails(&["a@example.com"]);
        let v = validate(
            input("news@seslogin.test", &r),
            Purpose::Send,
            &instance(),
            &exact_only(),
        )
        .unwrap();
        assert_eq!(v.from_name, "seslogin Support");
        let mut i = input("news@seslogin.test", &r);
        i.from_name = Some("  The seslogin team ");
        let v = validate(i, Purpose::Send, &instance(), &exact_only()).unwrap();
        assert_eq!(v.from_name, "The seslogin team");
    }

    // ── saved senders ────────────────────────────────────────────────────

    fn sender(name: &str, address: &str) -> db::BroadcastSender {
        db::BroadcastSender {
            id: "s1".to_string(),
            name: name.to_string(),
            address: address.to_string(),
        }
    }

    #[test]
    fn a_sender_is_normalized_and_must_be_on_the_instance_domain() {
        assert_eq!(
            validate_sender(
                "  SES Activity Updates ",
                "Updates@SESLOGIN.test",
                &exact_only(),
                &[]
            )
            .unwrap(),
            (
                "SES Activity Updates".to_string(),
                "updates@seslogin.test".to_string()
            )
        );
        assert!(validate_sender("News", "news@example.org", &exact_only(), &[]).is_err());
        assert!(validate_sender("News", "news+t1.x@seslogin.test", &exact_only(), &[]).is_err());
        assert!(validate_sender("News", "*@seslogin.test", &exact_only(), &[]).is_err());
    }

    #[test]
    fn a_sender_needs_a_single_line_name() {
        assert!(validate_sender("  ", "news@seslogin.test", &exact_only(), &[]).is_err());
        assert!(validate_sender("A\r\nBcc: x", "news@seslogin.test", &exact_only(), &[]).is_err());
    }

    #[test]
    fn an_identical_sender_is_refused_but_another_name_on_the_same_address_is_not() {
        let existing = vec![sender("Updates", "updates@seslogin.test")];
        assert!(
            validate_sender("Updates", "UPDATES@seslogin.test", &exact_only(), &existing).is_err()
        );
        assert!(
            validate_sender(
                "Product news",
                "updates@seslogin.test",
                &exact_only(),
                &existing
            )
            .is_ok()
        );
    }

    #[test]
    fn saved_senders_are_capped() {
        let existing: Vec<_> = (0..MAX_BROADCAST_SENDERS)
            .map(|i| sender(&format!("S{i}"), "news@seslogin.test"))
            .collect();
        assert!(
            validate_sender("One more", "news@seslogin.test", &exact_only(), &existing).is_err()
        );
    }

    // ── subject / body ───────────────────────────────────────────────────

    #[test]
    fn a_subject_with_a_ticket_tag_is_refused() {
        let r = emails(&["a@example.com"]);
        let mut i = input("news@seslogin.test", &r);
        i.subject = "Update on [#seslogin-12]";
        assert!(validate(i, Purpose::Send, &instance(), &exact_only()).is_err());
    }

    #[test]
    fn a_subject_with_a_line_break_is_refused() {
        let r = emails(&["a@example.com"]);
        let mut i = input("news@seslogin.test", &r);
        i.subject = "Hello\r\nBcc: everyone@example.com";
        assert!(validate(i, Purpose::Send, &instance(), &exact_only()).is_err());
    }

    #[test]
    fn empty_subject_or_body_is_refused() {
        let r = emails(&["a@example.com"]);
        let mut i = input("news@seslogin.test", &r);
        i.subject = "   ";
        assert!(validate(i, Purpose::Send, &instance(), &exact_only()).is_err());
        let mut i = input("news@seslogin.test", &r);
        i.body = "\n";
        assert!(validate(i, Purpose::Send, &instance(), &exact_only()).is_err());
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
        assert_eq!(
            normalize_recipients(&ok, &exact_only()).unwrap().len(),
            MAX_BROADCAST_RECIPIENTS
        );
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
        let v = validate(
            input("news@seslogin.test", &r),
            Purpose::Send,
            &instance(),
            &exact_only(),
        )
        .unwrap();
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
    fn from_record_rebuilds_what_validate_produced() {
        let r = emails(&["one@example.com", "two@example.com"]);
        let v = validate(
            input("news@seslogin.test", &r),
            Purpose::Send,
            &instance(),
            &exact_only(),
        )
        .unwrap();
        let rec = db::Broadcast {
            id: "b1".to_string(),
            instance_id: "inst1".to_string(),
            created_by_user_id: "u1".to_string(),
            created_at: 1,
            from_address: v.from_address.clone(),
            from_name: v.from_name.clone(),
            reply_to: v.reply_to.clone(),
            subject: v.subject.clone(),
            body_text: v.body.clone(),
            body_html: v.html.clone(),
            draft: false,
            last_test_sent_at: None,
            recipients: v
                .recipients
                .iter()
                .map(db::BroadcastRecipient::pending)
                .collect(),
            send_seq: 0,
            next_batch_at: Some(1),
        };
        assert_eq!(ValidBroadcast::from_record(&rec), v);
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

    // ── HTML ─────────────────────────────────────────────────────────────

    const HTML: &str = r#"<html><body><h1>Big news</h1><p>Shipped.</p>
        <img src="images/logo.png"></body></html>"#;

    fn html_input<'a>(
        recipients: &'a [String],
        assets: &'a HashMap<String, String>,
    ) -> BroadcastInput<'a> {
        BroadcastInput {
            body: "",
            html: Some(HTML),
            asset_urls: assets,
            ..input("news@seslogin.test", recipients)
        }
    }

    fn logo_hosted() -> HashMap<String, String> {
        HashMap::from([(
            "images/logo.png".to_string(),
            "https://app.example.com/email-assets/inst1/x/logo.png".to_string(),
        )])
    }

    #[test]
    fn a_draft_may_have_no_recipients_and_missing_images() {
        let v = validate(
            html_input(&[], &NO_ASSETS),
            Purpose::Draft,
            &instance(),
            &exact_only(),
        )
        .unwrap();
        assert!(v.recipients.is_empty());
        assert_eq!(v.missing_assets, vec!["images/logo.png"]);
    }

    #[test]
    fn a_test_or_real_send_refuses_missing_images() {
        let r = emails(&["one@example.com"]);
        for purpose in [Purpose::Test, Purpose::Send] {
            let err = validate(
                html_input(&r, &NO_ASSETS),
                purpose,
                &instance(),
                &exact_only(),
            )
            .unwrap_err();
            assert!(err.to_string().contains("images/logo.png"), "{err}");
        }
    }

    #[test]
    fn a_real_send_needs_recipients_but_a_test_does_not() {
        let assets = logo_hosted();
        assert!(
            validate(
                html_input(&[], &assets),
                Purpose::Send,
                &instance(),
                &exact_only()
            )
            .is_err()
        );
        assert!(
            validate(
                html_input(&[], &assets),
                Purpose::Test,
                &instance(),
                &exact_only()
            )
            .is_ok()
        );
    }

    #[test]
    fn an_html_broadcast_is_sent_as_authored_with_a_derived_text_part_and_no_signature() {
        let r = emails(&["one@example.com"]);
        let assets = logo_hosted();
        let v = validate(
            html_input(&r, &assets),
            Purpose::Send,
            &instance(),
            &exact_only(),
        )
        .unwrap();
        let built = build_broadcast_message(&v, &instance().signature, "one@example.com").unwrap();
        let msg = mail_parser::MessageParser::new().parse(&built.raw).unwrap();
        let html = msg.body_html(0).unwrap();
        assert!(html.contains("<h1>Big news</h1>"), "{html}");
        assert!(
            html.contains("https://app.example.com/email-assets/inst1/x/logo.png"),
            "{html}"
        );
        assert!(!html.contains("The seslogin team"), "{html}");
        let text = msg.body_text(0).unwrap().replace("\r\n", "\n");
        assert_eq!(text.trim(), "Big news\n\nShipped.");
        // The broadcast rules still hold: no reply tag, no ticket subject.
        assert!(!String::from_utf8_lossy(&built.raw).contains("+t"));
        assert_eq!(outbound::parse_subject_tag(msg.subject().unwrap()), None);
    }

    #[test]
    fn an_html_broadcast_uses_its_own_text_part_when_given() {
        let r = emails(&["one@example.com"]);
        let assets = logo_hosted();
        let i = BroadcastInput {
            body: "Plain version.",
            ..html_input(&r, &assets)
        };
        let v = validate(i, Purpose::Send, &instance(), &exact_only()).unwrap();
        let built = build_broadcast_message(&v, &instance().signature, "one@example.com").unwrap();
        let msg = mail_parser::MessageParser::new().parse(&built.raw).unwrap();
        assert_eq!(msg.body_text(0).unwrap().trim(), "Plain version.");
    }

    #[test]
    fn a_test_copy_is_built_like_the_real_one_with_a_test_subject() {
        let r = emails(&["one@example.com"]);
        let v = validate(
            input("news@seslogin.test", &r),
            Purpose::Send,
            &instance(),
            &exact_only(),
        )
        .unwrap();
        let built = build_test_message(&v, &instance().signature, "me@example.com").unwrap();
        assert_eq!(built.to, vec!["me@example.com"]);
        let msg = mail_parser::MessageParser::new().parse(&built.raw).unwrap();
        assert_eq!(
            msg.subject(),
            Some("[TEST] New in seslogin: location check-in")
        );
        assert_eq!(msg.header_raw("Precedence").map(str::trim), Some("bulk"));
    }
}
