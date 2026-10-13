//! Bounce and complaint handling for outbound mail (issue #6).
//!
//! Two halves, joined by the `email_suppression` table:
//!
//! 1. **Recording.** The SES configuration set (`infra/ses.tf`) publishes
//!    `BOUNCE` and `COMPLAINT` events to an SNS topic that invokes the
//!    inbound-mail Lambda directly (`bin/inbound-mail`). That Lambda hands
//!    each SNS message to [`record_feedback`], which [`parse_feedback`]s it
//!    and upserts one row per affected address. Only a **permanent** bounce
//!    or a complaint suppresses an address; a transient bounce (mailbox
//!    full, greylisting) is ignored, and so is a complaint whose feedback
//!    type is `not-spam`.
//!
//! 2. **Skipping.** Every instance-mail send — ticket replies and notices,
//!    staff notifications, invoices and credit notes — consults the table
//!    first ([`filter_recipients`]/[`find_suppressed`]) and leaves a
//!    suppressed address off the envelope *and* off the stored message's
//!    `to`/`cc`, so the thread shows who was actually mailed. System mail
//!    (login codes, the public form's verification code) deliberately does
//!    not: locking someone out of signing in on the strength of an old
//!    bounce is worse than one more bounce, and SES's own account-level
//!    suppression list still applies to it either way.
//!
//! **Lookups fail open.** If the table can't be read, the send goes ahead to
//! every recipient (with a `warn!`) — exactly what happened before this
//! module existed, and SES's account-level list is still the backstop. A
//! suppression check must never be the reason a customer reply fails.
//!
//! Suppression is global, not per instance: a dead mailbox is dead for every
//! tenant, and the account-level list SES keeps is global too. Removing an
//! address (`cli suppression remove`) is an operator action.

use std::collections::HashSet;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use crate::app::HasDb;
use crate::db::{self, Handler as _};

/// Longest `detail` stored — SES diagnostic codes are normally one SMTP line,
/// but they are remote-controlled text.
const MAX_DETAIL_LEN: usize = 500;

/// One address to suppress, parsed out of an SES event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feedback {
    pub email: String,
    pub reason: db::SuppressionReason,
    pub detail: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SesEvent {
    /// Configuration-set event publishing (what `infra/ses.tf` sets up).
    event_type: Option<String>,
    /// Identity notifications use this key instead; accepted too, so the
    /// parser doesn't care which of the two SES features delivered it.
    notification_type: Option<String>,
    bounce: Option<Bounce>,
    complaint: Option<Complaint>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bounce {
    bounce_type: String,
    bounce_sub_type: Option<String>,
    #[serde(default)]
    bounced_recipients: Vec<BouncedRecipient>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BouncedRecipient {
    email_address: String,
    status: Option<String>,
    diagnostic_code: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Complaint {
    #[serde(default)]
    complained_recipients: Vec<ComplainedRecipient>,
    complaint_feedback_type: Option<String>,
    complaint_sub_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComplainedRecipient {
    email_address: String,
}

/// Lowercase and trim an address, unwrapping a `Name <addr>` form. `None` for
/// anything without an `@`.
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

fn truncate(s: String) -> String {
    if s.len() <= MAX_DETAIL_LEN {
        return s;
    }
    let mut end = MAX_DETAIL_LEN;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn join_detail(parts: &[Option<&str>]) -> Option<String> {
    let joined = parts
        .iter()
        .flatten()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then(|| truncate(joined))
}

/// Parse one SES event (the SNS message body) into the addresses it
/// suppresses. Pure. An event of any other type, a transient bounce, or a
/// `not-spam` complaint yields an empty list — not an error. Malformed JSON
/// is an error.
pub fn parse_feedback(message: &str) -> Result<Vec<Feedback>> {
    let event: SesEvent = serde_json::from_str(message).context("parsing SES event JSON")?;
    let kind = event
        .event_type
        .or(event.notification_type)
        .unwrap_or_default();
    let mut out = Vec::new();
    match kind.as_str() {
        "Bounce" => {
            let Some(bounce) = event.bounce else {
                return Ok(out);
            };
            if bounce.bounce_type != "Permanent" {
                return Ok(out);
            }
            for r in bounce.bounced_recipients {
                let Some(email) = normalize_address(&r.email_address) else {
                    continue;
                };
                out.push(Feedback {
                    email,
                    reason: db::SuppressionReason::HardBounce,
                    detail: join_detail(&[
                        bounce.bounce_sub_type.as_deref(),
                        r.status.as_deref(),
                        r.diagnostic_code.as_deref(),
                    ]),
                });
            }
        }
        "Complaint" => {
            let Some(complaint) = event.complaint else {
                return Ok(out);
            };
            if complaint.complaint_feedback_type.as_deref() == Some("not-spam") {
                return Ok(out);
            }
            for r in complaint.complained_recipients {
                let Some(email) = normalize_address(&r.email_address) else {
                    continue;
                };
                out.push(Feedback {
                    email,
                    reason: db::SuppressionReason::Complaint,
                    detail: join_detail(&[
                        complaint.complaint_feedback_type.as_deref(),
                        complaint.complaint_sub_type.as_deref(),
                    ]),
                });
            }
        }
        _ => {}
    }
    Ok(out)
}

/// Parse an SES event and record a suppression for every address it names.
/// Returns how many addresses were recorded. Any write failure is returned,
/// so the Lambda invocation fails and SNS/Lambda retry it — every write is
/// an idempotent upsert, so a replay of a half-applied event is harmless.
pub async fn record_feedback<A: HasDb + Sync>(app: &A, message: &str) -> Result<usize> {
    let feedback = parse_feedback(message)?;
    let now = crate::clock::now_sec();
    for f in &feedback {
        app.db()
            .record_email_suppression(&f.email, f.reason, f.detail.as_deref(), now)
            .await
            .with_context(|| format!("recording suppression for {}", f.email))?;
        info!(
            email = %f.email,
            reason = f.reason.as_str(),
            detail = f.detail.as_deref().unwrap_or_default(),
            "address suppressed"
        );
    }
    Ok(feedback.len())
}

/// The suppression rows for whichever of `emails` are suppressed, in
/// `emails`' order (case-insensitive, duplicates collapsed). Fails open: a
/// read error is logged under `context` and treated as "none suppressed" —
/// see this module's doc comment.
pub async fn find_suppressed<A: HasDb + Sync>(
    app: &A,
    emails: &[String],
    context: &str,
) -> Vec<db::EmailSuppression> {
    let mut seen = HashSet::new();
    let keys: Vec<String> = emails
        .iter()
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty() && seen.insert(e.clone()))
        .collect();
    if keys.is_empty() {
        return vec![];
    }
    match app.db().get_email_suppressions(&keys).await {
        Ok(rows) => rows.into_iter().flatten().collect(),
        Err(e) => {
            warn!("{context}: could not read the email suppression list, sending anyway: {e}");
            vec![]
        }
    }
}

/// Envelope recipients with every suppressed address removed, plus the
/// suppressions that removed them.
#[derive(Debug, Clone, PartialEq)]
pub struct Filtered {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub suppressed: Vec<db::EmailSuppression>,
}

/// Drop suppressed addresses from `to`/`cc`, keeping order. See
/// [`find_suppressed`] for the fail-open behaviour.
pub async fn filter_recipients<A: HasDb + Sync>(
    app: &A,
    to: &[String],
    cc: &[String],
    context: &str,
) -> Filtered {
    let all: Vec<String> = to.iter().chain(cc).cloned().collect();
    let suppressed = find_suppressed(app, &all, context).await;
    let blocked: HashSet<&str> = suppressed.iter().map(|s| s.email.as_str()).collect();
    let keep = |list: &[String]| -> Vec<String> {
        list.iter()
            .filter(|e| !blocked.contains(e.trim().to_lowercase().as_str()))
            .cloned()
            .collect()
    };
    let filtered = Filtered {
        to: keep(to),
        cc: keep(cc),
        suppressed,
    };
    if !filtered.suppressed.is_empty() {
        info!(
            skipped = %describe_all(&filtered.suppressed),
            "{context}: not mailing suppressed address(es)"
        );
    }
    filtered
}

/// `"a@example.com (hard bounce)"` — for log lines and error messages.
pub fn describe(s: &db::EmailSuppression) -> String {
    let why = match s.reason {
        db::SuppressionReason::HardBounce => "hard bounce",
        db::SuppressionReason::Complaint => "spam complaint",
    };
    format!("{} ({why})", s.email)
}

/// [`describe`] for a list, comma-separated.
pub fn describe_all(list: &[db::EmailSuppression]) -> String {
    list.iter().map(describe).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from the SES developer guide's event-publishing examples
    /// ("Examples of event data that Amazon SES publishes to Amazon SNS").
    const PERMANENT_BOUNCE: &str = r#"{
      "eventType": "Bounce",
      "bounce": {
        "bounceType": "Permanent",
        "bounceSubType": "General",
        "bouncedRecipients": [
          {
            "emailAddress": "Recipient@Example.com",
            "action": "failed",
            "status": "5.1.1",
            "diagnosticCode": "smtp; 550 5.1.1 user unknown"
          }
        ],
        "timestamp": "2017-08-05T00:41:02.669Z",
        "feedbackId": "01000157c44f053b-61b59c11-9236-11e6-8f96-7be8aexample-000000",
        "reportingMTA": "dsn; mta.example.com"
      },
      "mail": {
        "timestamp": "2017-08-05T00:40:02.012Z",
        "source": "Sender Name <sender@example.com>",
        "messageId": "EXAMPLE7c191be45-e9aedb9a-02f9-4d12-a87d-dd0099a07f8a-000000",
        "destination": ["recipient@example.com"]
      }
    }"#;

    const COMPLAINT: &str = r#"{
      "eventType": "Complaint",
      "complaint": {
        "feedbackId": "0100015e1a3bd9ea-9fb98a44-7b3b-4a4b-9b6c-0bc7b1d9c6b1-000000",
        "complaintSubType": null,
        "complainedRecipients": [{ "emailAddress": "recipient@example.com" }],
        "timestamp": "2017-08-05T00:41:02.669Z",
        "userAgent": "Mozilla/5.0",
        "complaintFeedbackType": "abuse",
        "arrivalDate": "2017-08-05T00:41:02.669Z"
      },
      "mail": { "messageId": "EXAMPLE", "destination": ["recipient@example.com"] }
    }"#;

    #[test]
    fn permanent_bounce_suppresses_each_recipient_lowercased() {
        let got = parse_feedback(PERMANENT_BOUNCE).unwrap();
        assert_eq!(
            got,
            vec![Feedback {
                email: "recipient@example.com".into(),
                reason: db::SuppressionReason::HardBounce,
                detail: Some("General 5.1.1 smtp; 550 5.1.1 user unknown".into()),
            }]
        );
    }

    #[test]
    fn transient_bounce_suppresses_nothing() {
        let msg = PERMANENT_BOUNCE.replace("\"Permanent\"", "\"Transient\"");
        assert!(parse_feedback(&msg).unwrap().is_empty());
    }

    #[test]
    fn complaint_suppresses_the_complainer() {
        let got = parse_feedback(COMPLAINT).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].email, "recipient@example.com");
        assert_eq!(got[0].reason, db::SuppressionReason::Complaint);
        assert_eq!(got[0].detail.as_deref(), Some("abuse"));
    }

    #[test]
    fn not_spam_complaint_suppresses_nothing() {
        let msg = COMPLAINT.replace("\"abuse\"", "\"not-spam\"");
        assert!(parse_feedback(&msg).unwrap().is_empty());
    }

    #[test]
    fn identity_notification_shape_is_accepted_too() {
        let msg = PERMANENT_BOUNCE.replace("\"eventType\"", "\"notificationType\"");
        assert_eq!(parse_feedback(&msg).unwrap().len(), 1);
    }

    #[test]
    fn other_event_types_suppress_nothing() {
        let msg =
            r#"{"eventType":"Delivery","delivery":{"recipients":["a@example.com"]},"mail":{}}"#;
        assert!(parse_feedback(msg).unwrap().is_empty());
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_feedback("not json").is_err());
    }

    #[test]
    fn normalize_address_unwraps_display_names_and_rejects_junk() {
        assert_eq!(
            normalize_address("  \"Jo Bloggs\" <Jo@Example.COM> ").as_deref(),
            Some("jo@example.com")
        );
        assert_eq!(normalize_address("a@b").as_deref(), Some("a@b"));
        assert_eq!(normalize_address("nobody"), None);
        assert_eq!(normalize_address("@example.com"), None);
    }

    #[test]
    fn detail_is_capped() {
        let long = "x".repeat(MAX_DETAIL_LEN * 2);
        let msg = PERMANENT_BOUNCE.replace("smtp; 550 5.1.1 user unknown", &long);
        let got = parse_feedback(&msg).unwrap();
        assert_eq!(got[0].detail.as_ref().unwrap().len(), MAX_DETAIL_LEN);
    }
}
