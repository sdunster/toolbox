//! Integer money arithmetic for finance — see CLAUDE.md's "Finance"
//! house rule. Nothing here ever touches a float:
//!
//! - **Quantity** is a decimal with at most 2 dp, stored as integer
//!   *hundredths* (`"1.5"` → `150`), `0 < q ≤ 1,000,000`.
//! - **Unit price** is integer *cents*, `0 ≤ p ≤ 1,000,000,000`
//!   (10,000,000.00). GST-exclusive.
//! - **Line amount** = round-half-up(`quantity_hundredths × unit_price_cents
//!   / 100`), in cents.
//! - **GST** = round-half-up(`subtotal / 10`), in cents (PR 3/4 use it).
//!
//! The bounds are chosen so every product fits comfortably in an `i64`
//! (`100_000_000 × 1_000_000_000 = 10^17 < 9.2 × 10^18`); the arithmetic is
//! still done in `i128` so a caller passing something out of range gets a
//! wrong-but-defined answer rather than a panic or wrap.

/// Largest quantity, in hundredths: 1,000,000.00.
pub const MAX_QUANTITY_HUNDREDTHS: i64 = 100_000_000;

/// Largest unit price, in cents: 10,000,000.00.
pub const MAX_UNIT_PRICE_CENTS: i64 = 1_000_000_000;

/// `Number.MAX_SAFE_INTEGER` (2^53 − 1) — the largest invoice *total*
/// (subtotal + GST, in cents) a JS client can hold exactly. See
/// `finance::validate_total_within_safe_integer`.
pub const MAX_SAFE_TOTAL_CENTS: i64 = 9_007_199_254_740_991;

/// Parse a user-typed quantity (`"2"`, `"1.5"`, `"0.25"`, `".5"`) into
/// integer hundredths. Rejects blank input, anything but ASCII digits and at
/// most one `.`, more than 2 decimal places, zero, negatives, and anything
/// over [`MAX_QUANTITY_HUNDREDTHS`]. The error is a complete sentence fit to
/// show the user.
pub fn parse_quantity(raw: &str) -> Result<i64, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("Quantity is required".to_string());
    }
    if s.starts_with('-') {
        return Err("Quantity must be greater than zero".to_string());
    }
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    let well_formed = !(whole.is_empty() && frac.is_empty())
        && whole.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
        // "1." (a trailing point with no digits) is a typo, not a number.
        && !(s.ends_with('.'));
    if !well_formed {
        return Err(format!(
            "{s:?} is not a valid quantity (use a number like 2, 1.5 or 0.25)"
        ));
    }
    if frac.len() > 2 {
        return Err("Quantity can have at most 2 decimal places".to_string());
    }
    // Strip leading zeros before the length check, so "0001" is fine but a
    // 40-digit string can't overflow the parse below.
    let whole_trimmed = whole.trim_start_matches('0');
    if whole_trimmed.len() > 7 {
        return Err("Quantity cannot be more than 1,000,000".to_string());
    }
    let whole_value: i64 = if whole_trimmed.is_empty() {
        0
    } else {
        whole_trimmed
            .parse()
            .map_err(|_| format!("{s:?} is not a valid quantity"))?
    };
    let frac_value: i64 = match frac.len() {
        0 => 0,
        1 => i64::from(frac.as_bytes()[0] - b'0') * 10,
        _ => frac
            .parse()
            .map_err(|_| format!("{s:?} is not a valid quantity"))?,
    };
    let hundredths = whole_value * 100 + frac_value;
    if hundredths <= 0 {
        return Err("Quantity must be greater than zero".to_string());
    }
    if hundredths > MAX_QUANTITY_HUNDREDTHS {
        return Err("Quantity cannot be more than 1,000,000".to_string());
    }
    Ok(hundredths)
}

/// Format integer hundredths as the shortest decimal string that
/// round-trips through [`parse_quantity`]: `200` → `"2"`, `150` → `"1.5"`,
/// `25` → `"0.25"`. No thousands separators — this is the value the API
/// hands back to an edit form, not only a display string.
pub fn format_quantity(hundredths: i64) -> String {
    let sign = if hundredths < 0 { "-" } else { "" };
    let abs = hundredths.unsigned_abs();
    let whole = abs / 100;
    let frac = abs % 100;
    match frac {
        0 => format!("{sign}{whole}"),
        f if f % 10 == 0 => format!("{sign}{whole}.{}", f / 10),
        f => format!("{sign}{whole}.{f:02}"),
    }
}

/// Divide `numerator` by a positive `denominator`, rounding half away from
/// zero — "round half up" for the non-negative values finance deals in.
fn div_round_half_up(numerator: i128, denominator: i128) -> i128 {
    debug_assert!(denominator > 0);
    let half = denominator / 2;
    if numerator >= 0 {
        (numerator + half) / denominator
    } else {
        -((-numerator + half) / denominator)
    }
}

/// One line's amount in cents: round-half-up(`quantity_hundredths ×
/// unit_price_cents / 100`). E.g. 1.5 × 3.33 = 4.995 → 5.00.
pub fn line_amount_cents(quantity_hundredths: i64, unit_price_cents: i64) -> i64 {
    let product = i128::from(quantity_hundredths) * i128::from(unit_price_cents);
    clamp_i64(div_round_half_up(product, 100))
}

/// GST at 10% of a (GST-exclusive) subtotal, rounded half-up to the cent.
pub fn gst_cents(subtotal_cents: i64) -> i64 {
    clamp_i64(div_round_half_up(i128::from(subtotal_cents), 10))
}

fn clamp_i64(v: i128) -> i64 {
    i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX })
}

/// Format cents with thousands separators and exactly 2 dp, no currency
/// symbol: `123456` → `"1,234.56"`, `-5` → `"-0.05"`. The currency code is
/// printed separately (in a column header), per the invoice layout.
pub fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    let whole = (abs / 100).to_string();
    let frac = abs % 100;
    let mut grouped = String::with_capacity(whole.len() + whole.len() / 3);
    for (i, ch) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("{sign}{grouped}.{frac:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_quantity_accepts_integers_and_up_to_two_decimals() {
        assert_eq!(parse_quantity("2"), Ok(200));
        assert_eq!(parse_quantity("1.5"), Ok(150));
        assert_eq!(parse_quantity("0.25"), Ok(25));
        assert_eq!(parse_quantity("0.01"), Ok(1));
        assert_eq!(parse_quantity(".5"), Ok(50));
        assert_eq!(parse_quantity("10.50"), Ok(1050));
        assert_eq!(parse_quantity("007"), Ok(700));
        assert_eq!(parse_quantity("  3  "), Ok(300));
    }

    #[test]
    fn parse_quantity_accepts_the_maximum() {
        assert_eq!(parse_quantity("1000000"), Ok(MAX_QUANTITY_HUNDREDTHS));
        assert_eq!(parse_quantity("1000000.00"), Ok(MAX_QUANTITY_HUNDREDTHS));
    }

    #[test]
    fn parse_quantity_rejects_more_than_two_decimals() {
        assert!(parse_quantity("1.234").is_err());
        assert!(parse_quantity("0.001").is_err());
        // Even trailing zeros: the rule is about what was typed.
        assert!(parse_quantity("1.500").is_err());
    }

    #[test]
    fn parse_quantity_rejects_zero_and_negatives() {
        assert!(parse_quantity("0").is_err());
        assert!(parse_quantity("0.00").is_err());
        assert!(parse_quantity("-1").is_err());
        assert!(parse_quantity("-0.5").is_err());
    }

    #[test]
    fn parse_quantity_rejects_over_the_maximum() {
        assert!(parse_quantity("1000000.01").is_err());
        assert!(parse_quantity("1000001").is_err());
        assert!(parse_quantity("99999999999999999999999999").is_err());
    }

    #[test]
    fn parse_quantity_rejects_garbage() {
        for bad in [
            "", " ", ".", "1.", "abc", "1,5", "1.2.3", "+1", "1e3", "½", "1 000", "0x10", "--1",
        ] {
            assert!(parse_quantity(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn format_quantity_is_minimal() {
        assert_eq!(format_quantity(200), "2");
        assert_eq!(format_quantity(150), "1.5");
        assert_eq!(format_quantity(25), "0.25");
        assert_eq!(format_quantity(1), "0.01");
        assert_eq!(format_quantity(1050), "10.5");
        assert_eq!(format_quantity(MAX_QUANTITY_HUNDREDTHS), "1000000");
        assert_eq!(format_quantity(0), "0");
    }

    #[test]
    fn format_then_parse_round_trips() {
        for h in [
            1,
            5,
            10,
            25,
            99,
            100,
            101,
            150,
            12345,
            MAX_QUANTITY_HUNDREDTHS,
        ] {
            assert_eq!(parse_quantity(&format_quantity(h)), Ok(h), "{h}");
        }
    }

    #[test]
    fn line_amount_multiplies_and_rounds_half_up() {
        // 2 × 400.00 = 800.00
        assert_eq!(line_amount_cents(200, 40_000), 80_000);
        // 1.5 × 3.33 = 4.995 → 5.00 (exactly half a cent rounds up)
        assert_eq!(line_amount_cents(150, 333), 500);
        // 0.25 × 0.01 = 0.0025 → 0.00
        assert_eq!(line_amount_cents(25, 1), 0);
        // 0.5 × 0.01 = 0.005 → 0.01
        assert_eq!(line_amount_cents(50, 1), 1);
        // 0.33 × 1.00 = 0.33
        assert_eq!(line_amount_cents(33, 100), 33);
        // 1.25 × 0.99 = 1.2375 → 1.24
        assert_eq!(line_amount_cents(125, 99), 124);
        // zero price is allowed and is zero
        assert_eq!(line_amount_cents(300, 0), 0);
    }

    #[test]
    fn line_amount_does_not_overflow_at_the_maximum_bounds() {
        // 1,000,000 × 10,000,000.00 = 10,000,000,000,000.00
        assert_eq!(
            line_amount_cents(MAX_QUANTITY_HUNDREDTHS, MAX_UNIT_PRICE_CENTS),
            1_000_000_000_000_000
        );
    }

    #[test]
    fn gst_is_a_tenth_rounded_half_up() {
        assert_eq!(gst_cents(0), 0);
        assert_eq!(gst_cents(100_000), 10_000);
        assert_eq!(gst_cents(4), 0); // 0.4c → 0
        assert_eq!(gst_cents(5), 1); // 0.5c → 1
        assert_eq!(gst_cents(15), 2); // 1.5c → 2
        assert_eq!(gst_cents(12_345), 1_235); // 1,234.5c → 1,235
        assert_eq!(gst_cents(12_344), 1_234);
    }

    #[test]
    fn format_cents_groups_thousands_with_two_decimals() {
        assert_eq!(format_cents(0), "0.00");
        assert_eq!(format_cents(5), "0.05");
        assert_eq!(format_cents(100), "1.00");
        assert_eq!(format_cents(99_999), "999.99");
        assert_eq!(format_cents(123_456), "1,234.56");
        assert_eq!(format_cents(100_000_000), "1,000,000.00");
        assert_eq!(format_cents(1_000_000_000_000_000), "10,000,000,000,000.00");
        assert_eq!(format_cents(-123_456), "-1,234.56");
        assert_eq!(format_cents(-5), "-0.05");
    }

    #[test]
    fn format_cents_handles_i64_extremes_without_panicking() {
        assert!(format_cents(i64::MAX).ends_with(".07"));
        assert!(format_cents(i64::MIN).starts_with('-'));
    }
}
