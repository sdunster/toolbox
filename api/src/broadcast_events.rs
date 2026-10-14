//! SES delivery events → broadcast recipient status.
//!
//! One consumer of the shared SES event stream — see [`crate::ses_events`]
//! for how events arrive and are parsed (once, for every consumer).
//!
//! **Matching an event to a recipient needs no lookup index.** Every
//! broadcast send carries two SES message tags, `broadcast_id` and
//! `broadcast_recipient` (the recipient's index in `broadcast.recipients`,
//! see [`crate::broadcast::send_tags`]), and SES echoes a message's tags in
//! every event about it — so an event names its row and list position
//! directly, a strongly consistent update by primary key with no GSI and no
//! race against the send-result write. Only this system sets tags; a
//! recipient can't forge them. Events without them are not this module's.

use anyhow::Result;

use crate::broadcast::{TAG_BROADCAST_ID, TAG_BROADCAST_RECIPIENT};
use crate::db;
use crate::ses_events::SesEvent;

/// What one SES event means for a broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedEvent {
    /// Nothing to record — not a broadcast message, or an event type that
    /// carries no delivery outcome (`Send`, …).
    Ignored(String),
    /// The recipient's copy was opened or clicked. A broadcast copy has
    /// exactly one recipient, so — unlike ticket mail — this is exact about
    /// who.
    Engagement {
        broadcast_id: String,
        index: usize,
        engagement: db::Engagement,
        at: u64,
    },
    Update {
        broadcast_id: String,
        index: usize,
        update: db::DeliveryUpdate,
    },
}

/// What `event` means for a broadcast recipient. A broadcast copy has
/// exactly one recipient, so the first outcome SES reports is the one.
/// `now` is the fallback timestamp when the event carries none.
pub fn update_from(event: &SesEvent, now: u64) -> ParsedEvent {
    let kind = format!("{:?}", event.event_type);
    let Some(broadcast_id) = event.tag(TAG_BROADCAST_ID) else {
        return ParsedEvent::Ignored(format!("{kind} event for a message that isn't a broadcast"));
    };
    let Some(index) = event
        .tag(TAG_BROADCAST_RECIPIENT)
        .and_then(|s| s.parse::<usize>().ok())
    else {
        return ParsedEvent::Ignored(format!(
            "{kind} event for broadcast {broadcast_id} has no usable recipient tag"
        ));
    };
    if let Some(engagement) = event.engagement() {
        return ParsedEvent::Engagement {
            broadcast_id: broadcast_id.to_string(),
            index,
            engagement,
            at: event.at.unwrap_or(now),
        };
    }
    let Some(outcome) = event.recipient_outcomes().into_iter().next() else {
        return ParsedEvent::Ignored(format!(
            "{kind} event for broadcast {broadcast_id} carries no delivery outcome"
        ));
    };
    ParsedEvent::Update {
        broadcast_id: broadcast_id.to_string(),
        index,
        update: db::DeliveryUpdate {
            status: outcome.status,
            detail: outcome.detail,
            at: event.at.unwrap_or(now),
        },
    }
}

/// [`SesEvent::parse`] then [`update_from`] — for tests and callers holding
/// the raw message.
pub fn parse_event(message: &str, now: u64) -> Result<ParsedEvent> {
    Ok(update_from(&SesEvent::parse(message)?, now))
}

/// Record one SES event against the broadcast recipient it names. Returns a
/// short description of what happened, for logging. An event that matches
/// nothing (not a broadcast, an unknown broadcast or index, or a stale
/// out-of-order event) is not an error — retrying it would never succeed —
/// so only database failures are.
pub async fn apply<H: db::Handler>(db: &H, event: &SesEvent) -> Result<String> {
    let now = crate::clock::now_sec();
    match update_from(event, now) {
        ParsedEvent::Ignored(reason) => Ok(reason),
        ParsedEvent::Engagement {
            broadcast_id,
            index,
            engagement,
            at,
        } => {
            let applied = db
                .set_broadcast_engagement(&broadcast_id, index, engagement, at)
                .await?;
            Ok(format!(
                "broadcast {broadcast_id} recipient {index}: {engagement:?} {}",
                if applied {
                    "recorded"
                } else {
                    "not recorded (unknown recipient, or an earlier one is already stored)"
                }
            ))
        }
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
    use serde_json::Value;

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

    fn update(message: &str) -> (String, usize, db::DeliveryUpdate) {
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
        assert_eq!(u.status, db::DeliveryStatus::Bounced);
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
        assert_eq!(u.status, db::DeliveryStatus::Delivered);
        assert_eq!(u.detail.as_deref(), Some("250 2.6.0 Message received"));
    }

    #[test]
    fn a_complaint_reject_and_delay_each_map_to_their_status() {
        let (_, _, u) = update(&event(
            "Complaint",
            "complaint",
            serde_json::json!({ "complaintFeedbackType": "abuse" }),
        ));
        assert_eq!(u.status, db::DeliveryStatus::Complained);
        assert_eq!(u.detail.as_deref(), Some("Marked as spam (abuse)"));

        let (_, _, u) = update(&event(
            "Reject",
            "reject",
            serde_json::json!({ "reason": "Bad content" }),
        ));
        assert_eq!(u.status, db::DeliveryStatus::Rejected);
        assert_eq!(u.detail.as_deref(), Some("Bad content"));

        let (_, _, u) = update(&event(
            "DeliveryDelay",
            "deliveryDelay",
            serde_json::json!({
                "delayType": "MailboxFull",
                "delayedRecipients": [{
                    "emailAddress": "a@example.com",
                    "diagnosticCode": "452 4.2.2 mailbox full"
                }]
            }),
        ));
        assert_eq!(u.status, db::DeliveryStatus::Delayed);
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
    fn an_open_or_click_is_an_engagement_for_its_recipient() {
        let msg = event(
            "Open",
            "open",
            serde_json::json!({ "timestamp": "2026-10-13T02:00:00.000Z" }),
        );
        assert_eq!(
            parse_event(&msg, 0).unwrap(),
            ParsedEvent::Engagement {
                broadcast_id: "AbC123xyz789".into(),
                index: 2,
                engagement: db::Engagement::Open,
                at: 1_791_856_800,
            }
        );
        let msg = event(
            "Click",
            "click",
            serde_json::json!({ "link": "https://example.com/" }),
        );
        assert!(matches!(
            parse_event(&msg, 0).unwrap(),
            ParsedEvent::Engagement {
                engagement: db::Engagement::Click,
                at: 1_791_853_200,
                ..
            }
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
        use db::DeliveryStatus::*;
        assert!(Delayed.rank() < Delivered.rank());
        assert!(Delivered.rank() < Bounced.rank());
        assert!(Delivered.rank() < Complained.rank());
        assert_eq!(Bounced.rank(), Rejected.rank());
    }
}
