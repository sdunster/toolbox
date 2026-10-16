//! SES event publishing records, parsed once.
//!
//! The `toolbox` configuration set publishes `BOUNCE`, `COMPLAINT`,
//! `DELIVERY`, `DELIVERY_DELAY`, `REJECT`, `OPEN` and `CLICK` events for
//! every message sent through it to the `toolbox-mail-feedback` SNS topic
//! (`infra/ses_feedback.tf`), which invokes the inbound-mail Lambda directly.
//! That Lambda's SNS handler parses each message here, once, into a
//! [`SesEvent`], and hands the same value to every consumer:
//!
//! - `suppression::feedback_from` — permanent bounces and complaints add the
//!   address to `email_suppression`, whatever was sent;
//! - `broadcast_events::update_from` — the outcome, or the first open or
//!   click, for one broadcast recipient, found by the
//!   `broadcast_id`/`broadcast_recipient` tags;
//! - `ticket_delivery::updates_from` — the outcome for each recipient of one
//!   customer-facing ticket message, or its first open or click, found by
//!   the `ticket_id` tag and SES's message id.
//!
//! **Tags are the join key, never a lookup.** SES echoes a message's
//! `EmailTags` on every event about it, and only this system sets them, so a
//! consumer finds its row by primary key from the tags alone — no GSI, no
//! eventual-consistency window against the write that recorded the send.
//! An event with none of a consumer's tags is simply not that consumer's.
//!
//! Pure: no I/O. [`SesEvent::parse`] is the only fallible step (malformed
//! JSON); every field a given event type doesn't carry is just absent.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::db;

/// Longest detail string kept — SES diagnostic codes are normally one SMTP
/// line, but they are remote-controlled text.
pub const MAX_DETAIL_CHARS: usize = 500;

/// Which kind of event this is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventType {
    Send,
    Delivery,
    DeliveryDelay,
    Bounce,
    Complaint,
    Reject,
    Open,
    Click,
    /// Anything else SES may publish (`RenderingFailure`, `Subscription`,
    /// …) — no consumer acts on these.
    Other(String),
}

impl EventType {
    fn from_name(name: &str) -> Self {
        match name {
            "Send" => Self::Send,
            "Delivery" => Self::Delivery,
            "DeliveryDelay" => Self::DeliveryDelay,
            "Bounce" => Self::Bounce,
            "Complaint" => Self::Complaint,
            "Reject" => Self::Reject,
            "Open" => Self::Open,
            "Click" => Self::Click,
            other => Self::Other(other.to_string()),
        }
    }
}

/// One SES event, with the parts any consumer needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SesEvent {
    pub event_type: EventType,
    /// SES's own message id (bare, not the `<…@region.amazonses.com>` form).
    pub message_id: Option<String>,
    /// The message's `EmailTags`, plus SES's own `ses:*` tags.
    pub tags: HashMap<String, Vec<String>>,
    /// The original envelope recipients (`mail.destination`).
    pub destination: Vec<String>,
    /// When the event itself happened, else when the mail was sent (Unix
    /// seconds); `None` if SES gave neither in a parseable form.
    pub at: Option<u64>,
    pub bounce: Option<Bounce>,
    pub complaint: Option<Complaint>,
    pub delivery: Option<Delivery>,
    pub delivery_delay: Option<DeliveryDelay>,
    pub reject_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bounce {
    /// `Permanent`, `Transient` or `Undetermined`.
    pub bounce_type: String,
    pub bounce_sub_type: Option<String>,
    pub recipients: Vec<AffectedRecipient>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Complaint {
    pub feedback_type: Option<String>,
    pub sub_type: Option<String>,
    pub recipients: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub smtp_response: Option<String>,
    pub recipients: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryDelay {
    pub delay_type: Option<String>,
    pub recipients: Vec<AffectedRecipient>,
}

/// A bounced or delayed recipient, with SES's per-recipient diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AffectedRecipient {
    pub email: String,
    pub status: Option<String>,
    pub diagnostic_code: Option<String>,
}

/// What an event means for one recipient's delivery: the status to record
/// and a human-readable reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecipientOutcome {
    /// Lowercased.
    pub email: String,
    pub status: db::DeliveryStatus,
    pub detail: Option<String>,
}

// ── raw serde shapes ─────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEvent {
    /// Configuration-set event publishing (what `infra/ses_feedback.tf`
    /// sets up).
    event_type: Option<String>,
    /// Identity notifications use this key instead; accepted too.
    notification_type: Option<String>,
    #[serde(default)]
    mail: RawMail,
    bounce: Option<RawBounce>,
    complaint: Option<RawComplaint>,
    delivery: Option<RawDelivery>,
    delivery_delay: Option<RawDeliveryDelay>,
    reject: Option<RawReject>,
    open: Option<RawEngagement>,
    click: Option<RawEngagement>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawMail {
    message_id: Option<String>,
    timestamp: Option<String>,
    #[serde(default)]
    destination: Vec<String>,
    #[serde(default)]
    tags: HashMap<String, Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBounce {
    #[serde(default)]
    bounce_type: String,
    bounce_sub_type: Option<String>,
    #[serde(default)]
    bounced_recipients: Vec<RawAffected>,
    timestamp: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAffected {
    #[serde(default)]
    email_address: String,
    status: Option<String>,
    diagnostic_code: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawComplaint {
    #[serde(default)]
    complained_recipients: Vec<RawAddress>,
    complaint_feedback_type: Option<String>,
    complaint_sub_type: Option<String>,
    timestamp: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAddress {
    #[serde(default)]
    email_address: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDelivery {
    smtp_response: Option<String>,
    #[serde(default)]
    recipients: Vec<String>,
    timestamp: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDeliveryDelay {
    delay_type: Option<String>,
    #[serde(default)]
    delayed_recipients: Vec<RawAffected>,
    timestamp: Option<String>,
}

/// An `open` or `click` block — only its time is used. (A click's `link` is
/// deliberately not kept: it's whatever URL the message contained.)
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEngagement {
    timestamp: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReject {
    reason: Option<String>,
}

fn unix_seconds(ts: Option<&str>) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(ts?)
        .ok()
        .and_then(|t| u64::try_from(t.timestamp()).ok())
}

/// Lowercase and trim an address, unwrapping a `Name <addr>` form. `None` for
/// anything without an `@` with text on both sides.
pub fn normalize_address(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let inner = match (raw.rfind('<'), raw.rfind('>')) {
        (Some(start), Some(end)) if start < end => &raw[start + 1..end],
        _ => raw,
    };
    let addr = inner.trim().to_lowercase();
    let (local, domain) = addr.split_once('@')?;
    if local.is_empty() || domain.is_empty() {
        return None;
    }
    Some(addr)
}

/// Cap a detail string at [`MAX_DETAIL_CHARS`] characters, marking the cut.
pub fn truncate_detail(s: &str) -> String {
    if s.chars().count() <= MAX_DETAIL_CHARS {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX_DETAIL_CHARS).collect();
        out.push('…');
        out
    }
}

/// Join the non-empty parts with `sep`; `None` if there are none.
fn join(parts: &[Option<&str>], sep: &str) -> Option<String> {
    let joined = parts
        .iter()
        .flatten()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(sep);
    (!joined.is_empty()).then_some(joined)
}

impl SesEvent {
    /// Parse one SES event (an SNS message body). Only malformed JSON is an
    /// error.
    pub fn parse(message: &str) -> Result<Self> {
        let raw: RawEvent = serde_json::from_str(message).context("parsing SES event JSON")?;
        let name = raw.event_type.or(raw.notification_type).unwrap_or_default();
        let affected = |list: Vec<RawAffected>| -> Vec<AffectedRecipient> {
            list.into_iter()
                .map(|r| AffectedRecipient {
                    email: r.email_address,
                    status: r.status,
                    diagnostic_code: r.diagnostic_code,
                })
                .collect()
        };
        let event_ts = raw
            .bounce
            .as_ref()
            .and_then(|b| b.timestamp.clone())
            .or_else(|| raw.complaint.as_ref().and_then(|c| c.timestamp.clone()))
            .or_else(|| raw.delivery.as_ref().and_then(|d| d.timestamp.clone()))
            .or_else(|| {
                raw.delivery_delay
                    .as_ref()
                    .and_then(|d| d.timestamp.clone())
            })
            .or_else(|| raw.open.as_ref().and_then(|o| o.timestamp.clone()))
            .or_else(|| raw.click.as_ref().and_then(|c| c.timestamp.clone()));
        let at = unix_seconds(event_ts.as_deref())
            .or_else(|| unix_seconds(raw.mail.timestamp.as_deref()));
        Ok(SesEvent {
            event_type: EventType::from_name(&name),
            message_id: raw.mail.message_id,
            tags: raw.mail.tags,
            destination: raw.mail.destination,
            at,
            bounce: raw.bounce.map(|b| Bounce {
                bounce_type: b.bounce_type,
                bounce_sub_type: b.bounce_sub_type,
                recipients: affected(b.bounced_recipients),
            }),
            complaint: raw.complaint.map(|c| Complaint {
                feedback_type: c.complaint_feedback_type,
                sub_type: c.complaint_sub_type,
                recipients: c
                    .complained_recipients
                    .into_iter()
                    .map(|r| r.email_address)
                    .collect(),
            }),
            delivery: raw.delivery.map(|d| Delivery {
                smtp_response: d.smtp_response,
                recipients: d.recipients,
            }),
            delivery_delay: raw.delivery_delay.map(|d| DeliveryDelay {
                delay_type: d.delay_type,
                recipients: affected(d.delayed_recipients),
            }),
            reject_reason: raw.reject.and_then(|r| r.reason),
        })
    }

    /// The first value of message tag `name`, if the message carried it.
    pub fn tag(&self, name: &str) -> Option<&str> {
        self.tags.get(name)?.first().map(String::as_str)
    }

    /// Whether this is an engagement event (an open or a click) — about the
    /// message as a whole: SES names no recipient for one, since every
    /// recipient of one message shares its tracking pixel and links.
    pub fn engagement(&self) -> Option<db::Engagement> {
        match self.event_type {
            EventType::Open => Some(db::Engagement::Open),
            EventType::Click => Some(db::Engagement::Click),
            _ => None,
        }
    }

    /// What this event means for each recipient it concerns, in the order
    /// SES lists them. Empty for event types that carry no delivery outcome
    /// (`Send`, `Open`, `Click`, …). A `Reject` concerns every original recipient.
    ///
    /// The detail strings: a delivery's SMTP response; a bounce's
    /// `{type}/{subType}: {diagnosticCode}`; a delay's
    /// `{delayType}: {diagnosticCode}`; a complaint's
    /// `Marked as spam ({feedbackType})`; a reject's reason.
    pub fn recipient_outcomes(&self) -> Vec<RecipientOutcome> {
        use db::DeliveryStatus as S;
        let mut out = Vec::new();
        let mut push = |email: &str, status: S, detail: Option<String>| {
            if let Some(email) = normalize_address(email) {
                out.push(RecipientOutcome {
                    email,
                    status,
                    detail: detail.map(|d| truncate_detail(&d)),
                });
            }
        };
        match self.event_type {
            EventType::Delivery => {
                if let Some(d) = &self.delivery {
                    let recipients = if d.recipients.is_empty() {
                        &self.destination
                    } else {
                        &d.recipients
                    };
                    for r in recipients {
                        push(r, S::Delivered, d.smtp_response.clone());
                    }
                }
            }
            EventType::Bounce => {
                if let Some(b) = &self.bounce {
                    let kind = join(
                        &[Some(b.bounce_type.as_str()), b.bounce_sub_type.as_deref()],
                        "/",
                    );
                    for r in &b.recipients {
                        let detail = match (&kind, r.diagnostic_code.as_deref()) {
                            (Some(k), Some(code)) => Some(format!("{k}: {code}")),
                            (Some(k), None) => Some(k.clone()),
                            (None, code) => code.map(str::to_string),
                        };
                        push(&r.email, S::Bounced, detail);
                    }
                }
            }
            EventType::DeliveryDelay => {
                if let Some(d) = &self.delivery_delay {
                    for r in &d.recipients {
                        let detail = match (d.delay_type.as_deref(), r.diagnostic_code.as_deref()) {
                            (Some(t), Some(code)) => Some(format!("{t}: {code}")),
                            (t, code) => t.or(code).map(str::to_string),
                        };
                        push(&r.email, S::Delayed, detail);
                    }
                }
            }
            EventType::Complaint => {
                if let Some(c) = &self.complaint {
                    let detail = Some(match c.feedback_type.as_deref() {
                        Some(t) => format!("Marked as spam ({t})"),
                        None => "Marked as spam".to_string(),
                    });
                    let recipients = if c.recipients.is_empty() {
                        &self.destination
                    } else {
                        &c.recipients
                    };
                    for r in recipients {
                        push(r, S::Complained, detail.clone());
                    }
                }
            }
            EventType::Reject => {
                for r in &self.destination {
                    push(r, S::Rejected, self.reject_reason.clone());
                }
            }
            EventType::Send | EventType::Open | EventType::Click | EventType::Other(_) => {}
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(body: serde_json::Value) -> SesEvent {
        SesEvent::parse(&body.to_string()).unwrap()
    }

    #[test]
    fn parses_mail_fields_and_tags() {
        let e = event(serde_json::json!({
            "eventType": "Send",
            "mail": {
                "messageId": "0100-abc",
                "timestamp": "2026-10-13T01:00:00.000Z",
                "destination": ["a@example.com"],
                "tags": { "ticket_id": ["T1"], "ses:configuration-set": ["toolbox"] }
            },
            "send": {}
        }));
        assert_eq!(e.event_type, EventType::Send);
        assert_eq!(e.message_id.as_deref(), Some("0100-abc"));
        assert_eq!(e.tag("ticket_id"), Some("T1"));
        assert_eq!(e.tag("missing"), None);
        assert_eq!(e.destination, vec!["a@example.com"]);
        assert_eq!(e.at, Some(1_791_853_200));
        assert!(e.recipient_outcomes().is_empty());
    }

    #[test]
    fn bounce_outcome_per_recipient_with_its_own_diagnostic() {
        let e = event(serde_json::json!({
            "eventType": "Bounce",
            "mail": { "destination": ["A@example.com", "b@example.com"] },
            "bounce": {
                "bounceType": "Permanent",
                "bounceSubType": "General",
                "timestamp": "2026-10-13T01:00:05.000Z",
                "bouncedRecipients": [
                    { "emailAddress": "A@Example.com", "diagnosticCode": "550 5.1.1 unknown" },
                    { "emailAddress": "b@example.com" }
                ]
            }
        }));
        assert_eq!(e.at, Some(1_791_853_205), "the event's own timestamp wins");
        assert_eq!(
            e.recipient_outcomes(),
            vec![
                RecipientOutcome {
                    email: "a@example.com".into(),
                    status: db::DeliveryStatus::Bounced,
                    detail: Some("Permanent/General: 550 5.1.1 unknown".into()),
                },
                RecipientOutcome {
                    email: "b@example.com".into(),
                    status: db::DeliveryStatus::Bounced,
                    detail: Some("Permanent/General".into()),
                },
            ]
        );
    }

    #[test]
    fn delivery_complaint_delay_and_reject_outcomes() {
        let d = event(serde_json::json!({
            "eventType": "Delivery",
            "mail": {},
            "delivery": { "recipients": ["a@example.com"], "smtpResponse": "250 OK" }
        }));
        assert_eq!(
            d.recipient_outcomes()[0].status,
            db::DeliveryStatus::Delivered
        );
        assert_eq!(d.recipient_outcomes()[0].detail.as_deref(), Some("250 OK"));

        let c = event(serde_json::json!({
            "eventType": "Complaint",
            "mail": {},
            "complaint": {
                "complaintFeedbackType": "abuse",
                "complainedRecipients": [{ "emailAddress": "a@example.com" }]
            }
        }));
        assert_eq!(
            c.recipient_outcomes()[0].detail.as_deref(),
            Some("Marked as spam (abuse)")
        );

        let delay = event(serde_json::json!({
            "eventType": "DeliveryDelay",
            "mail": {},
            "deliveryDelay": {
                "delayType": "MailboxFull",
                "delayedRecipients": [{ "emailAddress": "a@example.com", "diagnosticCode": "452 full" }]
            }
        }));
        assert_eq!(
            delay.recipient_outcomes()[0].detail.as_deref(),
            Some("MailboxFull: 452 full")
        );

        let reject = event(serde_json::json!({
            "eventType": "Reject",
            "mail": { "destination": ["a@example.com", "b@example.com"] },
            "reject": { "reason": "Bad content" }
        }));
        let r = reject.recipient_outcomes();
        assert_eq!(r.len(), 2, "a reject concerns every original recipient");
        assert!(r.iter().all(|o| o.status == db::DeliveryStatus::Rejected));
    }

    #[test]
    fn identity_notification_shape_is_accepted() {
        let e = event(serde_json::json!({
            "notificationType": "Bounce",
            "mail": {},
            "bounce": { "bounceType": "Transient", "bouncedRecipients": [{ "emailAddress": "a@x.io" }] }
        }));
        assert_eq!(e.event_type, EventType::Bounce);
    }

    #[test]
    fn opens_and_clicks_are_engagement_not_delivery_outcomes() {
        let open = event(serde_json::json!({
            "eventType": "Open",
            "mail": { "timestamp": "2026-10-13T01:00:00.000Z", "destination": ["a@example.com"] },
            "open": { "timestamp": "2026-10-13T02:00:00.000Z", "userAgent": "Mozilla", "ipAddress": "192.0.2.1" }
        }));
        assert_eq!(open.event_type, EventType::Open);
        assert_eq!(open.engagement(), Some(db::Engagement::Open));
        assert_eq!(open.at, Some(1_791_856_800), "the open's own time wins");
        assert!(open.recipient_outcomes().is_empty());

        let click = event(serde_json::json!({
            "eventType": "Click",
            "mail": { "destination": ["a@example.com"] },
            "click": { "timestamp": "2026-10-13T02:00:05.000Z", "link": "https://example.com/" }
        }));
        assert_eq!(click.engagement(), Some(db::Engagement::Click));
        assert_eq!(click.at, Some(1_791_856_805));

        let delivery = event(serde_json::json!({ "eventType": "Delivery", "mail": {} }));
        assert_eq!(delivery.engagement(), None);
    }

    #[test]
    fn unknown_types_and_missing_blocks_yield_no_outcomes() {
        let e = event(serde_json::json!({ "eventType": "RenderingFailure", "mail": {} }));
        assert_eq!(e.event_type, EventType::Other("RenderingFailure".into()));
        assert_eq!(e.engagement(), None);
        assert!(e.recipient_outcomes().is_empty());
        let e = event(serde_json::json!({ "eventType": "Bounce", "mail": {} }));
        assert!(e.recipient_outcomes().is_empty());
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(SesEvent::parse("not json").is_err());
    }

    #[test]
    fn long_details_are_truncated() {
        let long = "x".repeat(MAX_DETAIL_CHARS * 2);
        let e = event(serde_json::json!({
            "eventType": "Reject",
            "mail": { "destination": ["a@example.com"] },
            "reject": { "reason": long }
        }));
        let d = e.recipient_outcomes().remove(0).detail.unwrap();
        assert_eq!(d.chars().count(), MAX_DETAIL_CHARS + 1);
    }

    #[test]
    fn normalize_address_unwraps_display_names_and_rejects_junk() {
        assert_eq!(
            normalize_address("  \"Jo\" <Jo@Example.COM> ").as_deref(),
            Some("jo@example.com")
        );
        assert_eq!(normalize_address("nobody"), None);
        assert_eq!(normalize_address("@example.com"), None);
    }
}
