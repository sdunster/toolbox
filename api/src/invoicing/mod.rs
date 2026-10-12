//! Invoicing domain logic that doesn't belong to any one storage backend or
//! GraphQL resolver — see CLAUDE.md's "Invoicing" house rule.
//!
//! - [`money`]: quantity/cents parsing, formatting, line amounts and GST.
//! - [`snapshot`]: the frozen `InvoiceSnapshot` shape and `build_snapshot`,
//!   the one function that produces both a finalized invoice's frozen
//!   content and a draft's live preview.
//! - [`pdf`]: rendering a finalized invoice's snapshot to a PDF.
//! - The billable-item input validators below, shared by
//!   `createBillableItem`/`updateBillableItem`.
//! - [`validate_total_within_safe_integer`], shared by every mutation that
//!   changes an invoice's item set.
//! - [`expense`]: expense input validation (`createExpense`/`updateExpense`).
//! - [`vehicle`]: the ATO cents-per-km rate table and trip arithmetic.
//! - [`ledger`]: payments, credits and the derived `paid_date`.
//! - [`report`]: the GST/BAS summary, aged receivables, project figures.
//! - [`csv`]: the accountant's CSV exports.
//! - [`send`]: emailing an invoice or credit note with its PDF attached.
//! - [`today_utc`]/[`add_days`]: the date arithmetic due dates and overdue
//!   checks need.

pub mod csv;
pub mod expense;
pub mod ledger;
pub mod money;
pub mod pdf;
pub mod report;
pub mod send;
pub mod snapshot;
pub mod vehicle;

use chrono::NaiveDate;

/// Longest billable-item description, in characters (multi-line allowed).
pub const MAX_DESCRIPTION_LEN: usize = 2000;

/// Validate a `YYYY-MM-DD` calendar date — strictly that shape (zero-padded,
/// no time, no timezone) and a real day (`2026-02-30` is rejected). Returns
/// the canonical string, which is what's stored: a `date` sort key only
/// orders correctly if every value is written in exactly this form.
pub fn validate_item_date(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    let parsed = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| format!("{s:?} is not a valid date (expected YYYY-MM-DD)"))?;
    let canonical = parsed.format("%Y-%m-%d").to_string();
    // chrono accepts unpadded fields ("2026-8-1"); the sort key doesn't.
    if canonical != s {
        return Err(format!("{s:?} is not a valid date (expected YYYY-MM-DD)"));
    }
    Ok(canonical)
}

/// Trim and length-check a billable item's description. Multi-line is fine
/// (lines starting `* `/`- ` become bullets on the invoice); only leading/
/// trailing whitespace of the whole string is trimmed, so the line structure
/// survives.
pub fn validate_description(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Description cannot be empty".to_string());
    }
    if trimmed.chars().count() > MAX_DESCRIPTION_LEN {
        return Err(format!(
            "Description cannot be longer than {MAX_DESCRIPTION_LEN} characters"
        ));
    }
    Ok(trimmed.to_string())
}

/// Range-check a unit price in cents: `0 ≤ p ≤` [`money::MAX_UNIT_PRICE_CENTS`].
/// Zero is allowed (a no-charge line); negatives are not (discounts/credit
/// lines are out of scope for v1).
pub fn validate_unit_price_cents(cents: i64) -> Result<i64, String> {
    if cents < 0 {
        return Err("Unit price cannot be negative".to_string());
    }
    if cents > money::MAX_UNIT_PRICE_CENTS {
        return Err("Unit price cannot be more than 10,000,000.00".to_string());
    }
    Ok(cents)
}

/// Reject a would-be invoice total (subtotal + GST, in cents) that exceeds
/// `Number.MAX_SAFE_INTEGER` — see CLAUDE.md's "Invoicing" house rule
/// ("Totals overflow"). `amountCents` can individually reach 10^15 within a
/// single line's own bounds, but a JS client must be able to hold an
/// invoice's *total* exactly, so every mutation that changes an invoice's
/// item set (`createInvoice`, `addInvoiceItems`, `finalizeInvoice`) checks
/// the prospective total here before writing. Practically unreachable at
/// the 50-item-per-invoice cap, but cheap to check.
pub fn validate_total_within_safe_integer(total_cents: i64) -> Result<(), String> {
    if total_cents > money::MAX_SAFE_TOTAL_CENTS {
        return Err(
            "Invoice total would exceed the maximum value this application supports".to_string(),
        );
    }
    Ok(())
}

/// Today's date, `YYYY-MM-DD`, by the UTC clock (`crate::clock`) — what
/// "overdue" and the aged-receivables report measure against. UTC, not the
/// business's local time: an invoice becomes overdue at UTC midnight after
/// its due date, up to a day later than local midnight in Australia.
pub fn today_utc() -> String {
    chrono::DateTime::from_timestamp(crate::clock::now_sec() as i64, 0)
        .map(|t| t.date_naive().format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".to_string())
}

/// `date` (`YYYY-MM-DD`, already validated) plus `days`.
pub fn add_days(date: &str, days: u32) -> Result<String, String> {
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| format!("{date:?} is not a valid date"))?;
    parsed
        .checked_add_days(chrono::Days::new(u64::from(days)))
        .map(|d| d.format("%Y-%m-%d").to_string())
        .ok_or_else(|| "Date is out of range".to_string())
}

/// Validate payment terms in days: `0..=`[`crate::db::MAX_PAYMENT_TERMS_DAYS`].
pub fn validate_payment_terms_days(days: i32) -> Result<u32, String> {
    u32::try_from(days)
        .ok()
        .filter(|d| *d <= crate::db::MAX_PAYMENT_TERMS_DAYS)
        .ok_or_else(|| {
            format!(
                "Payment terms must be between 0 and {} days",
                crate::db::MAX_PAYMENT_TERMS_DAYS
            )
        })
}

/// Parse a markup percentage (`"10"`, `"12.5"`, `"0"`) into basis points
/// (hundredths of a percent): at most 2 dp, `0 ≤ markup ≤ 1000%`. Reuses the
/// quantity parser's decimal rules.
pub fn parse_markup_basis_points(raw: &str) -> Result<i64, String> {
    let s = raw.trim();
    // `parse_quantity` rejects zero; "0", "0.0" and "" all mean no markup.
    let is_zero = s.bytes().any(|b| b == b'0')
        && s.bytes().all(|b| b == b'0' || b == b'.')
        && s.bytes().filter(|b| *b == b'.').count() <= 1;
    if s.is_empty() || is_zero {
        return Ok(0);
    }
    let bp = money::parse_quantity(s)
        .map_err(|_| format!("{s:?} is not a valid markup (use a percentage like 10 or 12.5)"))?;
    if bp > 100_000 {
        return Err("Markup cannot be more than 1000%".to_string());
    }
    Ok(bp)
}

/// What re-billing an expense charges, GST-exclusive: its own ex-GST cost
/// plus `markup_basis_points`, rounded half-up to the cent.
pub fn rebill_unit_price_cents(expense_ex_gst_cents: i64, markup_basis_points: i64) -> i64 {
    report::apportion(expense_ex_gst_cents, 10_000 + markup_basis_points, 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_days_crosses_months_and_years() {
        assert_eq!(add_days("2026-08-19", 14).unwrap(), "2026-09-02");
        assert_eq!(add_days("2026-12-25", 30).unwrap(), "2027-01-24");
        assert_eq!(add_days("2026-08-19", 0).unwrap(), "2026-08-19");
    }

    #[test]
    fn payment_terms_are_bounded() {
        assert_eq!(validate_payment_terms_days(0), Ok(0));
        assert_eq!(validate_payment_terms_days(365), Ok(365));
        assert!(validate_payment_terms_days(-1).is_err());
        assert!(validate_payment_terms_days(366).is_err());
    }

    #[test]
    fn markup_parses_to_basis_points() {
        assert_eq!(parse_markup_basis_points("0"), Ok(0));
        assert_eq!(parse_markup_basis_points(""), Ok(0));
        assert_eq!(parse_markup_basis_points("10"), Ok(1_000));
        assert_eq!(parse_markup_basis_points("12.5"), Ok(1_250));
        assert!(parse_markup_basis_points("-5").is_err());
        assert!(parse_markup_basis_points("1001").is_err());
        assert!(parse_markup_basis_points("abc").is_err());
    }

    #[test]
    fn rebill_price_applies_the_markup() {
        assert_eq!(rebill_unit_price_cents(10_000, 0), 10_000);
        assert_eq!(rebill_unit_price_cents(10_000, 1_000), 11_000);
        assert_eq!(rebill_unit_price_cents(333, 1_250), 375); // 374.625 -> 375
    }

    #[test]
    fn validate_item_date_accepts_a_real_padded_date() {
        assert_eq!(validate_item_date("2026-08-19").unwrap(), "2026-08-19");
        assert_eq!(validate_item_date(" 2024-02-29 ").unwrap(), "2024-02-29");
    }

    #[test]
    fn validate_item_date_rejects_bad_shapes_and_impossible_days() {
        for bad in [
            "",
            "2026-8-19",
            "2026-08-9",
            "19/08/2026",
            "2026-02-30",
            "2025-02-29",
            "2026-13-01",
            "2026-08-19T00:00:00",
            "20260819",
            "garbage",
        ] {
            assert!(validate_item_date(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn validate_description_trims_keeps_lines_and_caps_length() {
        assert_eq!(
            validate_description("  Labour\n* site visit\n* report  ").unwrap(),
            "Labour\n* site visit\n* report"
        );
        assert!(validate_description("   \n  ").is_err());
        assert!(validate_description(&"x".repeat(MAX_DESCRIPTION_LEN)).is_ok());
        assert!(validate_description(&"x".repeat(MAX_DESCRIPTION_LEN + 1)).is_err());
    }

    #[test]
    fn validate_total_within_safe_integer_accepts_up_to_the_limit() {
        assert!(validate_total_within_safe_integer(0).is_ok());
        assert!(validate_total_within_safe_integer(money::MAX_SAFE_TOTAL_CENTS).is_ok());
        assert!(validate_total_within_safe_integer(money::MAX_SAFE_TOTAL_CENTS + 1).is_err());
    }

    #[test]
    fn validate_unit_price_cents_bounds() {
        assert_eq!(validate_unit_price_cents(0), Ok(0));
        assert_eq!(
            validate_unit_price_cents(money::MAX_UNIT_PRICE_CENTS),
            Ok(money::MAX_UNIT_PRICE_CENTS)
        );
        assert!(validate_unit_price_cents(-1).is_err());
        assert!(validate_unit_price_cents(money::MAX_UNIT_PRICE_CENTS + 1).is_err());
    }
}
