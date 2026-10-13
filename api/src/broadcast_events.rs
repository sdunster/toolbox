//! SES delivery events → broadcast recipient status.
//!
//! The `toolbox` SES configuration set publishes bounce/complaint/delivery/
//! delay/reject events for every message sent through it to the
//! `toolbox-mail-feedback` SNS topic (`infra/ses_feedback.tf`), which invokes
//! the inbound-mail Lambda directly; its SNS handler passes each message
//! both here and to `suppression::record_feedback`.
//!
//! Bounces can't be picked up any other way: SES sends from a custom MAIL
//! FROM domain (`mail.{domain}`), so the bounce message itself goes to SES,
//! never to our inbound pipeline.
//!
//! **Matching an event to a recipient needs no lookup index.** Every
//! broadcast send carries two SES message tags, `broadcast_id` and
//! `broadcast_recipient` (the recipient's index in `broadcast.recipients`,
//! see [`crate::broadcast::send_tags`]), and SES echoes a message's tags in
//! every event about it — so an event names its row and list position
//! directly, a strongly consistent update by primary key with no GSI and no
//! race against the send-result write. Only this system sets tags; a
//! recipient can't forge them. Events for anything else (ticket mail, which
//! carries no tags) are ignored.
//!
//! Parsing ([`parse_event`]) is pure and unit-tested against SES's documented
//! event shapes; [`apply`] is the one function that writes.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::broadcast::{TAG_BROADCAST_ID, TAG_BROADCAST_RECIPIENT};
use crate::db;

/// What one SES event means for a broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedEvent {
    /// Nothing to record — not a broadcast message, or an event type that
    /// carries no delivery outcome (`Send`, `Open`, `Click`, …).
    Ignored(String),
    Update {
        broadcast_id: String,
        index: usize,
        update: db::BroadcastDeliveryUpdate,
    },
}

/// Parse one SES event-publishing record (the SNS `Message` body). Only
/// malformed JSON is an error; everything else that isn't a recordable
/// broadcast outcome is [`ParsedEvent::Ignored`]. `now` is the fallback
/// timestamp when the event carries none.
pub fn parse_event(message: &str, now: u64) -> Result<ParsedEvent> {
    let v: Value = serde_json::from_str(message).context("parsing SES event JSON")?;
    let event_type = v
        .get("eventType")
        .or_else(|| v.get("notificationType"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let tags = v.pointer("/mail/tags");
    let first_tag = |name: &str| -> Option<String> {
        tags?
            .get(name)?
            .as_array()?
            .first()?
            .as_str()
            .map(str::to_string)
    };
    let Some(broadcast_id) = first_tag(TAG_BROADCAST_ID) else {
        return Ok(ParsedEvent::Ignored(format!(
            "{event_type} event for a message that isn't a broadcast"
        )));
    };
    let Some(index) = first_tag(TAG_BROADCAST_RECIPIENT).and_then(|s| s.parse::<usize>().ok())
    else {
        return Ok(ParsedEvent::Ignored(format!(
            "{event_type} event for broadcast {broadcast_id} has no usable recipient tag"
        )));
    };

    let str_at = |ptr: &str| v.pointer(ptr).and_then(Value::as_str).map(str::to_string);
    let first_diagnostic = |list_ptr: &str| -> Option<String> {
        v.pointer(list_ptr)?
            .as_array()?
            .iter()
            .find_map(|r| r.get("diagnosticCode").and_then(Value::as_str))
            .map(str::to_string)
    };
    let (status, detail, ts) = match event_type.as_str() {
        "Delivery" => (
            db::BroadcastDeliveryStatus::Delivered,
            str_at("/delivery/smtpResponse"),
            str_at("/delivery/timestamp"),
        ),
        "Bounce" => {
            let kind = [
                str_at("/bounce/bounceType"),
                str_at("/bounce/bounceSubType"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("/");
            let detail = match first_diagnostic("/bounce/bouncedRecipients") {
                Some(code) if !kind.is_empty() => Some(format!("{kind}: {code}")),
                Some(code) => Some(code),
                None if !kind.is_empty() => Some(kind),
                None => None,
            };
            (
                db::BroadcastDeliveryStatus::Bounced,
                detail,
                str_at("/bounce/timestamp"),
            )
        }
        "Complaint" => (
            db::BroadcastDeliveryStatus::Complained,
            Some(match str_at("/complaint/complaintFeedbackType") {
                Some(t) => format!("Marked as spam ({t})"),
                None => "Marked as spam".to_string(),
            }),
            str_at("/complaint/timestamp"),
        ),
        "Reject" => (
            db::BroadcastDeliveryStatus::Rejected,
            str_at("/reject/reason"),
            None,
        ),
        "DeliveryDelay" => {
            let delay = str_at("/deliveryDelay/delayType");
            let detail = match (delay, first_diagnostic("/deliveryDelay/delayedRecipients")) {
                (Some(d), Some(code)) => Some(format!("{d}: {code}")),
                (d, code) => d.or(code),
            };
            (
                db::BroadcastDeliveryStatus::Delayed,
                detail,
                str_at("/deliveryDelay/timestamp"),
            )
        }
        other => {
            return Ok(ParsedEvent::Ignored(format!(
                "{other} event for broadcast {broadcast_id} carries no delivery outcome"
            )));
        }
    };
    let at = ts
        .or_else(|| str_at("/mail/timestamp"))
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
        .and_then(|t| u64::try_from(t.timestamp()).ok())
        .unwrap_or(now);
    Ok(ParsedEvent::Update {
        broadcast_id,
        index,
        update: db::BroadcastDeliveryUpdate {
            status,
            detail: detail.map(|d| truncate(&d, 500)),
            at,
        },
    })
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

/// Parse and record one SES event. Returns a short description of what
/// happened, for logging. An event that matches nothing (not a broadcast,
/// an unknown broadcast or index, or a stale out-of-order event) is not an
/// error — retrying it would never succeed — so only malformed JSON and
/// database failures are.
pub async fn apply<H: db::Handler>(db: &H, message: &str) -> Result<String> {
    let now = crate::clock::now_sec();
    match parse_event(message, now)? {
        ParsedEvent::Ignored(reason) => Ok(reason),
        ParsedEvent::Update {
            broadcast_id,
            index,
            update,
        } => {
            let applied = db
                .set_broadcast_delivery(&broadcast_id, index, &update)
                .await?;
            Ok(if applied {
                format!(
                    "broadcast {broadcast_id} recipient {index}: {}",
                    update.status.as_str()
                )
            } else {
                format!(
                    "broadcast {broadcast_id} recipient {index}: {} not recorded \
                     (unknown recipient, or a later status is already stored)",
                    update.status.as_str()
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tagged_mail() -> Value {
        serde_json::json!({
            "timestamp": "2026-10-13T01:00:00.000Z",
            "source": "news@seslogin.test",
            "messageId": "0100018f-abc",
            "destination": ["a@example.com"],
            "tags": {
                "ses:configuration-set": ["toolbox"],
                "broadcast_id": ["AbC123xyz789"],
                "broadcast_recipient": ["2"]
            }
        })
    }

    fn event(event_type: &str, key: &str, body: Value) -> String {
        let mut v = serde_json::json!({ "eventType": event_type, "mail": tagged_mail() });
        v[key] = body;
        v.to_string()
    }

    fn update(message: &str) -> (String, usize, db::BroadcastDeliveryUpdate) {
        match parse_event(message, 42).unwrap() {
            ParsedEvent::Update {
                broadcast_id,
                index,
                update,
            } => (broadcast_id, index, update),
            other => panic!("expected an update, got {other:?}"),
        }
    }

    #[test]
    fn a_permanent_bounce_records_its_type_and_diagnostic() {
        let msg = event(
            "Bounce",
            "bounce",
            serde_json::json!({
                "bounceType": "Permanent",
                "bounceSubType": "General",
                "bouncedRecipients": [{
                    "emailAddress": "a@example.com",
                    "action": "failed",
                    "status": "5.1.1",
                    "diagnosticCode": "smtp; 550 5.1.1 user unknown"
                }],
                "timestamp": "2026-10-13T01:00:05.000Z"
            }),
        );
        let (id, index, u) = update(&msg);
        assert_eq!(id, "AbC123xyz789");
        assert_eq!(index, 2);
        assert_eq!(u.status, db::BroadcastDeliveryStatus::Bounced);
        assert_eq!(
            u.detail.as_deref(),
            Some("Permanent/General: smtp; 550 5.1.1 user unknown")
        );
        assert_eq!(u.at, 1_791_853_205);
    }

    #[test]
    fn a_suppression_list_bounce_without_a_diagnostic_still_says_why() {
        let msg = event(
            "Bounce",
            "bounce",
            serde_json::json!({
                "bounceType": "Permanent",
                "bounceSubType": "OnAccountSuppressionList",
                "bouncedRecipients": [{ "emailAddress": "a@example.com" }]
            }),
        );
        let (_, _, u) = update(&msg);
        assert_eq!(
            u.detail.as_deref(),
            Some("Permanent/OnAccountSuppressionList")
        );
        // No bounce timestamp: falls back to the mail's own.
        assert_eq!(u.at, 1_791_853_200);
    }

    #[test]
    fn a_delivery_records_the_smtp_response() {
        let msg = event(
            "Delivery",
            "delivery",
            serde_json::json!({
                "timestamp": "2026-10-13T01:00:01.000Z",
                "recipients": ["a@example.com"],
                "smtpResponse": "250 2.6.0 Message received"
            }),
        );
        let (_, _, u) = update(&msg);
        assert_eq!(u.status, db::BroadcastDeliveryStatus::Delivered);
        assert_eq!(u.detail.as_deref(), Some("250 2.6.0 Message received"));
    }

    #[test]
    fn a_complaint_reject_and_delay_each_map_to_their_status() {
        let (_, _, u) = update(&event(
            "Complaint",
            "complaint",
            serde_json::json!({ "complaintFeedbackType": "abuse" }),
        ));
        assert_eq!(u.status, db::BroadcastDeliveryStatus::Complained);
        assert_eq!(u.detail.as_deref(), Some("Marked as spam (abuse)"));

        let (_, _, u) = update(&event(
            "Reject",
            "reject",
            serde_json::json!({ "reason": "Bad content" }),
        ));
        assert_eq!(u.status, db::BroadcastDeliveryStatus::Rejected);
        assert_eq!(u.detail.as_deref(), Some("Bad content"));

        let (_, _, u) = update(&event(
            "DeliveryDelay",
            "deliveryDelay",
            serde_json::json!({
                "delayType": "MailboxFull",
                "delayedRecipients": [{ "diagnosticCode": "452 4.2.2 mailbox full" }]
            }),
        ));
        assert_eq!(u.status, db::BroadcastDeliveryStatus::Delayed);
        assert_eq!(
            u.detail.as_deref(),
            Some("MailboxFull: 452 4.2.2 mailbox full")
        );
    }

    #[test]
    fn a_send_event_is_ignored() {
        let msg = event("Send", "send", serde_json::json!({}));
        assert!(matches!(
            parse_event(&msg, 0).unwrap(),
            ParsedEvent::Ignored(_)
        ));
    }

    #[test]
    fn an_untagged_event_is_ignored() {
        let msg = serde_json::json!({
            "eventType": "Bounce",
            "mail": { "messageId": "x", "tags": { "ses:configuration-set": ["toolbox"] } },
            "bounce": { "bounceType": "Permanent" }
        })
        .to_string();
        assert!(matches!(
            parse_event(&msg, 0).unwrap(),
            ParsedEvent::Ignored(_)
        ));
    }

    #[test]
    fn a_non_numeric_recipient_tag_is_ignored() {
        let msg = serde_json::json!({
            "eventType": "Delivery",
            "mail": { "tags": { "broadcast_id": ["b"], "broadcast_recipient": ["two"] } },
            "delivery": {}
        })
        .to_string();
        assert!(matches!(
            parse_event(&msg, 0).unwrap(),
            ParsedEvent::Ignored(_)
        ));
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_event("{not json", 0).is_err());
    }

    #[test]
    fn long_detail_is_truncated() {
        let long = "x".repeat(600);
        let (_, _, u) = update(&event(
            "Reject",
            "reject",
            serde_json::json!({ "reason": long }),
        ));
        assert_eq!(u.detail.unwrap().chars().count(), 501);
    }

    #[test]
    fn delivery_status_ranks_only_move_forward_sensibly() {
        use db::BroadcastDeliveryStatus::*;
        assert!(Delayed.rank() < Delivered.rank());
        assert!(Delivered.rank() < Bounced.rank());
        assert!(Delivered.rank() < Complained.rank());
        assert_eq!(Bounced.rank(), Rejected.rank());
    }
}
