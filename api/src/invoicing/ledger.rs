//! What a finalized invoice is owed and what has settled it: payments,
//! credit notes, and the derived `paid_date`. See CLAUDE.md's "Payments and
//! credit notes" house rule.
//!
//! An invoice's **balance** is `total − credited − paid`. It is *settled*
//! once that reaches zero or below, and `paid_date` (the attribute every
//! UNPAID/PAID/OVERDUE filter reads) is set exactly when it is settled — to
//! the date of whatever settled it. Every write that changes payments or
//! credits recomputes it with [`settled_date`] and writes the result in the
//! same conditional update, so the two can never disagree.

use crate::db;

/// Most payments one invoice may carry — far more than any real invoice
/// needs, and what keeps the `payments` JSON attribute well under
/// DynamoDB's 400 KB item limit.
pub const MAX_PAYMENTS_PER_INVOICE: usize = 100;

/// Longest payment note, in characters.
pub const MAX_PAYMENT_NOTE_LEN: usize = 200;

/// The `paid_date` an invoice should carry: `None` while anything is still
/// owed; once settled, the later of the latest payment date and `fallback`
/// — the date of the event doing the settling (a credit note's issue date,
/// a new payment's date), or the invoice's existing `paid_date` when a
/// change leaves it settled.
pub fn settled_date(
    total_cents: i64,
    credited_cents: i64,
    payments: &[db::InvoicePayment],
    fallback: &str,
) -> Option<String> {
    let paid: i64 = payments.iter().map(|p| p.amount_cents).sum();
    if total_cents - credited_cents - paid > 0 {
        return None;
    }
    let latest_payment = payments.iter().map(|p| p.date.as_str()).max();
    Some(match latest_payment {
        Some(d) if d > fallback => d.to_string(),
        _ => fallback.to_string(),
    })
}

/// Trim a payment note; blank is `None`.
pub fn validate_payment_note(raw: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > MAX_PAYMENT_NOTE_LEN {
        return Err(format!(
            "Note cannot be longer than {MAX_PAYMENT_NOTE_LEN} characters"
        ));
    }
    Ok(Some(trimmed.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payment(date: &str, amount_cents: i64) -> db::InvoicePayment {
        db::InvoicePayment {
            id: format!("p-{date}"),
            date: date.into(),
            amount_cents,
            note: None,
            recorded_by_user_id: "u".into(),
            recorded_at: 0,
        }
    }

    #[test]
    fn unsettled_while_anything_is_owed() {
        assert_eq!(settled_date(10_000, 0, &[], "2026-09-01"), None);
        assert_eq!(
            settled_date(10_000, 2_000, &[payment("2026-09-01", 7_999)], "2026-09-02"),
            None
        );
    }

    #[test]
    fn settled_by_payments_takes_the_latest_payment_date() {
        let payments = [payment("2026-09-03", 4_000), payment("2026-09-01", 6_000)];
        assert_eq!(
            settled_date(10_000, 0, &payments, "2026-09-01").as_deref(),
            Some("2026-09-03")
        );
    }

    #[test]
    fn settled_by_a_credit_note_takes_its_date_when_later() {
        assert_eq!(
            settled_date(10_000, 10_000, &[], "2026-09-05").as_deref(),
            Some("2026-09-05")
        );
        let payments = [payment("2026-09-01", 6_000)];
        assert_eq!(
            settled_date(10_000, 4_000, &payments, "2026-09-05").as_deref(),
            Some("2026-09-05")
        );
    }

    #[test]
    fn overpaid_is_still_settled() {
        assert!(
            settled_date(
                10_000,
                5_000,
                &[payment("2026-09-01", 10_000)],
                "2026-09-01"
            )
            .is_some()
        );
    }

    #[test]
    fn payment_note_is_trimmed_and_capped() {
        assert_eq!(
            validate_payment_note(Some("  EFT  ")),
            Ok(Some("EFT".into()))
        );
        assert_eq!(validate_payment_note(Some("   ")), Ok(None));
        assert_eq!(validate_payment_note(None), Ok(None));
        assert!(validate_payment_note(Some(&"x".repeat(MAX_PAYMENT_NOTE_LEN + 1))).is_err());
    }
}
