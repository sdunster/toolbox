//! Delivery tracking for customer-facing ticket mail.
//!
//! Every message a ticket sends its requesters and CCs — an agent's reply,
//! the `submitTicket` acknowledgement, a close/reopen notice, the inbound
//! relay to other participants — goes out with one SES message tag,
//! `ticket_id` ([`send_tags`]). SES echoes it on every event about that
//! message, so [`apply`] can record each recipient's outcome (delivered,
//! delayed, bounced with the remote server's reason, rejected, marked as
//! spam) in `message_delivery`, keyed `{ses_message_id}#{email}`: a plain
//! upsert by primary key, with no lookup to find the `ticket_message` row and
//! so no race against the write that stamps its `rfc_message_id`. The thread
//! view joins the two on the SES id inside `rfc_message_id`
//! (`db::ses_message_id_from_rfc`).
//!
//! Opens and clicks ([`db::Engagement`]) are recorded per *message*, not per
//! recipient, in one row keyed by the bare `{ses_message_id}`
//! (`db::MessageEngagement`): a ticket message goes out as one SES message to
//! every To/Cc address, so they share one tracking pixel and SES can't say
//! which of them opened it. Only the first time of each is kept.
//!
//! Staff notifications are not tagged: they go to members, not to the people
//! a ticket is for. Statuses only move forward (`db::DeliveryStatus::rank`),
//! exactly as for broadcasts. One consumer of [`crate::ses_events`].

use anyhow::Result;

use crate::db;
use crate::ses_events::{RecipientOutcome, SesEvent};

/// The SES message-tag name carrying the ticket id.
pub const TAG_TICKET_ID: &str = "ticket_id";

/// The SES message tags for a customer-facing message on `ticket_id`.
pub fn send_tags(ticket_id: &str) -> Vec<(String, String)> {
    vec![(TAG_TICKET_ID.to_string(), ticket_id.to_string())]
}

/// What `event` means for a ticket message: its ticket, SES's message id,
/// and each affected recipient's outcome. `None` when the event isn't about
/// ticket mail, has no message id, or carries no delivery outcome.
pub fn updates_from(event: &SesEvent) -> Option<(String, String, Vec<RecipientOutcome>)> {
    let ticket_id = event.tag(TAG_TICKET_ID)?;
    let ses_message_id = event.message_id.as_deref()?;
    let outcomes = event.recipient_outcomes();
    if outcomes.is_empty() {
        return None;
    }
    Some((ticket_id.to_string(), ses_message_id.to_string(), outcomes))
}

/// Record `event` against the ticket message it names. Returns a short
/// description for logging. Not ticket mail, or a stale out-of-order event,
/// is not an error; only a database failure is (so the Lambda retries, and
/// every write is idempotent).
pub async fn apply<H: db::Handler>(db: &H, event: &SesEvent) -> Result<String> {
    if let Some(engagement) = event.engagement() {
        let (Some(ticket_id), Some(ses_message_id)) =
            (event.tag(TAG_TICKET_ID), event.message_id.as_deref())
        else {
            return Ok("not a tracked ticket message".to_string());
        };
        let at = event.at.unwrap_or_else(crate::clock::now_sec);
        let recorded = db
            .record_message_engagement(ticket_id, ses_message_id, engagement, at)
            .await?;
        return Ok(format!(
            "ticket {ticket_id} message {ses_message_id}: {engagement:?} {}",
            if recorded {
                "recorded"
            } else {
                "not recorded (an earlier one is already stored)"
            }
        ));
    }
    let Some((ticket_id, ses_message_id, outcomes)) = updates_from(event) else {
        return Ok("not a tracked ticket message".to_string());
    };
    let at = event.at.unwrap_or_else(crate::clock::now_sec);
    let mut recorded = 0;
    for o in &outcomes {
        let update = db::DeliveryUpdate {
            status: o.status,
            detail: o.detail.clone(),
            at,
        };
        if db
            .record_message_delivery(&ticket_id, &ses_message_id, &o.email, &update)
            .await?
        {
            recorded += 1;
        }
    }
    Ok(format!(
        "ticket {ticket_id} message {ses_message_id}: recorded {recorded} of {} recipient status(es)",
        outcomes.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(json: serde_json::Value) -> SesEvent {
        SesEvent::parse(&json.to_string()).unwrap()
    }

    #[test]
    fn a_tagged_bounce_yields_one_update_per_recipient() {
        let e = event(serde_json::json!({
            "eventType": "Bounce",
            "mail": { "messageId": "0100-x", "tags": { "ticket_id": ["T1"] } },
            "bounce": {
                "bounceType": "Permanent",
                "bouncedRecipients": [
                    { "emailAddress": "a@example.com" },
                    { "emailAddress": "b@example.com" }
                ]
            }
        }));
        let (ticket, mid, outcomes) = updates_from(&e).unwrap();
        assert_eq!(ticket, "T1");
        assert_eq!(mid, "0100-x");
        assert_eq!(outcomes.len(), 2);
    }

    #[test]
    fn untagged_or_outcome_free_events_are_not_ticket_updates() {
        let untagged = event(serde_json::json!({
            "eventType": "Delivery",
            "mail": { "messageId": "m", "tags": { "broadcast_id": ["b"] } },
            "delivery": { "recipients": ["a@example.com"] }
        }));
        assert!(updates_from(&untagged).is_none());
        let send = event(serde_json::json!({
            "eventType": "Send",
            "mail": { "messageId": "m", "tags": { "ticket_id": ["T1"] } }
        }));
        assert!(updates_from(&send).is_none());
    }

    #[test]
    fn an_open_is_engagement_not_a_recipient_update() {
        let open = event(serde_json::json!({
            "eventType": "Open",
            "mail": { "messageId": "m", "destination": ["a@example.com", "b@example.com"], "tags": { "ticket_id": ["T1"] } },
            "open": { "timestamp": "2026-10-13T02:00:00.000Z" }
        }));
        assert!(updates_from(&open).is_none());
        assert_eq!(open.engagement(), Some(db::Engagement::Open));
    }

    #[test]
    fn send_tags_name_the_ticket() {
        assert_eq!(
            send_tags("AbC"),
            vec![("ticket_id".to_string(), "AbC".to_string())]
        );
    }
}
