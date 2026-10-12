//! Emailing a finalized invoice or credit note to the client, with its PDF
//! attached — `sendInvoice`/`sendCreditNote`. See CLAUDE.md's "Outbound
//! mail" and "Sending invoices" house rules.
//!
//! An invoicing instance has no inbound address of its own (inbound mail is
//! a support-instance feature), so this mail is sent from the system sender
//! (`mail::system_from()`) under the business's name, with `Reply-To` set to
//! the business's own email when it has one — a client's reply goes to the
//! business, never into Toolbox's inbound pipeline. The subject never
//! carries a `[#slug-n]` ticket tag, so even a reply that did reach the
//! support domain couldn't be threaded onto a ticket.

use anyhow::{Context as _, Result};
use mail_builder::MessageBuilder;
use std::collections::HashSet;

use crate::db;
use crate::invoicing::money;
use crate::invoicing::snapshot::InvoiceSnapshot;
use crate::mail;
use crate::outbound::BuiltMessage;

/// Combined `to` + `cc` cap, after normalization and dedup.
pub const MAX_RECIPIENTS: usize = 10;

/// Longest covering message, in characters.
pub const MAX_MESSAGE_LEN: usize = 5000;

/// Normalize, dedupe and cap a send's recipients. `to` must end up
/// non-empty; a `cc` already in `to` is dropped.
pub fn normalize_recipients(
    to: &[String],
    cc: &[String],
) -> Result<(Vec<String>, Vec<String>), String> {
    let mut seen = HashSet::new();
    let mut norm = |list: &[String]| -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for raw in list {
            let email = db::normalize_user_email(raw)?;
            if seen.insert(email.clone()) {
                out.push(email);
            }
        }
        Ok(out)
    };
    let to = norm(to)?;
    let cc = norm(cc)?;
    if to.is_empty() {
        return Err("Add at least one recipient".to_string());
    }
    if to.len() + cc.len() > MAX_RECIPIENTS {
        return Err(format!(
            "Too many recipients: at most {MAX_RECIPIENTS} in total"
        ));
    }
    Ok((to, cc))
}

/// Trim the optional covering message; blank is `None`.
pub fn validate_message(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > MAX_MESSAGE_LEN {
        return Err(format!(
            "Message cannot be longer than {MAX_MESSAGE_LEN} characters"
        ));
    }
    Ok(Some(trimmed.to_string()))
}

fn ddmmyyyy(iso: &str) -> String {
    match iso.split('-').collect::<Vec<_>>().as_slice() {
        [y, m, d] => format!("{d}/{m}/{y}"),
        _ => iso.to_string(),
    }
}

/// `"Tax Invoice 008 from Fictional Trades"` — what the subject says.
pub fn subject(snapshot: &InvoiceSnapshot) -> String {
    let number = snapshot.display_number.as_deref().unwrap_or("");
    match snapshot.seller.name.as_deref().filter(|n| !n.is_empty()) {
        Some(name) => format!("{} {number} from {name}", snapshot.title),
        None => format!("{} {number}", snapshot.title),
    }
}

/// The plain-text body: the sender's message (if any), then what's
/// attached and what it means for the client — the amount due and due date
/// for an invoice, the amount credited for a credit note — then the payment
/// details for an invoice.
pub fn body(snapshot: &InvoiceSnapshot, balance_cents: i64, message: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(message) = message {
        out.push_str(message);
        out.push_str("\n\n");
    }
    let number = snapshot.display_number.as_deref().unwrap_or("");
    out.push_str(&format!(
        "Please find attached {} {number}.\n\n",
        snapshot.title.to_lowercase()
    ));
    match &snapshot.credit_note {
        Some(info) => {
            out.push_str(&format!(
                "It credits {} {} against invoice {}.\n",
                snapshot.currency,
                money::format_cents(snapshot.total_cents),
                info.invoice_display_number
            ));
        }
        None => {
            let due = snapshot
                .due_date
                .as_deref()
                .map(|d| format!(", due by {}", ddmmyyyy(d)))
                .unwrap_or_default();
            out.push_str(&format!(
                "Amount due: {} {}{due}.\n",
                snapshot.currency,
                money::format_cents(balance_cents.max(0)),
            ));
            if let Some(details) = snapshot
                .payment_details
                .as_deref()
                .filter(|d| !d.is_empty())
            {
                out.push('\n');
                out.push_str(details);
                out.push('\n');
            }
        }
    }
    out
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Build the message: `From` the system sender under the seller's name,
/// `Reply-To` the seller's email (else the system reply-to), the body above
/// as text and HTML, and the PDF attached as `filename`.
pub fn build(
    snapshot: &InvoiceSnapshot,
    balance_cents: i64,
    pdf: &[u8],
    filename: &str,
    to: &[String],
    cc: &[String],
    message: Option<&str>,
) -> Result<BuiltMessage> {
    let from = mail::system_from();
    let from_domain = from
        .rsplit_once('@')
        .map(|(_, d)| d)
        .unwrap_or("toolbox.invalid")
        .to_string();
    let from_name = snapshot
        .seller
        .name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Toolbox".to_string());
    let reply_to = snapshot
        .seller
        .email
        .clone()
        .filter(|e| !e.is_empty())
        .unwrap_or_else(mail::system_reply_to);
    let text = body(snapshot, balance_cents, message);
    let html = format!(
        "<!DOCTYPE html>\n<html><body style=\"font-family:Arial,Helvetica,sans-serif;color:#222\">\n<div>{}</div>\n</body></html>",
        escape_html(&text).replace('\n', "<br>\n")
    );
    let message_id = format!("{}@{from_domain}", crate::nonce::generate_nonce(16));
    let mut builder = MessageBuilder::new()
        .from((from_name.as_str(), from.as_str()))
        .reply_to(reply_to.as_str())
        .to(to.to_vec())
        .subject(subject(snapshot))
        .message_id(message_id)
        .text_body(text)
        .html_body(html)
        .attachment("application/pdf", filename.to_string(), pdf.to_vec());
    if !cc.is_empty() {
        builder = builder.cc(cc.to_vec());
    }
    let raw = builder.write_to_vec().context("building invoice email")?;
    Ok(BuiltMessage {
        raw,
        to: to.to_vec(),
        cc: cc.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invoicing::snapshot::{InvoiceSnapshotBillTo, InvoiceSnapshotSeller};
    use mail_parser::MimeHeaders as _;

    fn snapshot() -> InvoiceSnapshot {
        InvoiceSnapshot {
            schema_version: 2,
            title: "Tax Invoice".into(),
            display_number: Some("008".into()),
            issue_date: Some("2026-08-19".into()),
            seller: InvoiceSnapshotSeller {
                name: Some("Fictional Trades".into()),
                abn: None,
                address: None,
                phone: None,
                email: Some("accounts@fictional.example".into()),
            },
            bill_to: InvoiceSnapshotBillTo {
                name: "Client".into(),
                abn: None,
                address: None,
            },
            reference: None,
            currency: "AUD".into(),
            gst_registered: true,
            lines: vec![],
            subtotal_cents: 10_000,
            gst_cents: 1_000,
            total_cents: 11_000,
            payment_details: Some("BSB 000-000 Acc 0000".into()),
            due_date: Some("2026-09-02".into()),
            credit_note: None,
            no_gst_note: false,
        }
    }

    #[test]
    fn recipients_are_normalized_deduped_and_capped() {
        let (to, cc) = normalize_recipients(
            &[" A@Example.com ".into(), "a@example.com".into()],
            &["a@example.com".into(), "b@example.com".into()],
        )
        .unwrap();
        assert_eq!(to, vec!["a@example.com"]);
        assert_eq!(cc, vec!["b@example.com"]);
        assert!(normalize_recipients(&[], &["b@example.com".into()]).is_err());
        assert!(normalize_recipients(&["nope".into()], &[]).is_err());
        let many: Vec<String> = (0..11).map(|i| format!("x{i}@example.com")).collect();
        assert!(normalize_recipients(&many, &[]).is_err());
    }

    #[test]
    fn body_states_the_balance_due_and_payment_details() {
        let text = body(&snapshot(), 6_000, Some("Thanks for your business."));
        assert!(text.starts_with("Thanks for your business.\n\n"));
        assert!(text.contains("Amount due: AUD 60.00, due by 02/09/2026."));
        assert!(text.contains("BSB 000-000"));
    }

    #[test]
    fn the_message_attaches_the_pdf_and_replies_to_the_business() {
        let built = build(
            &snapshot(),
            11_000,
            b"%PDF-1.3 fake",
            "Invoice-008.pdf",
            &["client@example.com".into()],
            &[],
            None,
        )
        .unwrap();
        let parsed = mail_parser::MessageParser::default()
            .parse(&built.raw)
            .expect("parses");
        assert_eq!(
            parsed.subject(),
            Some("Tax Invoice 008 from Fictional Trades")
        );
        assert_eq!(
            parsed
                .reply_to()
                .and_then(|r| r.first())
                .and_then(|a| a.address()),
            Some("accounts@fictional.example")
        );
        assert_eq!(parsed.attachment_count(), 1);
        assert_eq!(
            parsed.attachment(0).and_then(|a| a.attachment_name()),
            Some("Invoice-008.pdf")
        );
        assert!(outbound_tag_free(parsed.subject().unwrap_or("")));
    }

    fn outbound_tag_free(subject: &str) -> bool {
        crate::outbound::parse_subject_tag(subject).is_none()
    }
}
