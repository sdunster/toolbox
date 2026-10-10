//! Vehicle trips claimed by the ATO's cents-per-kilometre method — see
//! CLAUDE.md's "Expenses" house rule.
//!
//! - **Distance** is a decimal with at most 1 dp, stored as integer
//!   *tenths of a km* (`"12.5"` → `125`), `0 < d ≤ 5,000`.
//! - **Rate** comes from [`ATO_RATES`] by the trip's financial year, never
//!   from the caller, and is stored on the trip so a later table change
//!   never alters a saved claim.
//! - **Amount** = round-half-up(`distance_tenths_km × rate_cents_per_km /
//!   10`), in cents — derived on read, never stored.

use chrono::{Datelike, NaiveDate};

/// The ATO caps a cents-per-km claim at this many business km per car per
/// financial year. Toolbox shows a running total against it; it never
/// refuses a trip.
pub const ANNUAL_CAP_KM: i64 = 5_000;

/// Largest single trip, in tenths of a km: 5,000.0 km (the whole annual cap).
pub const MAX_DISTANCE_TENTHS_KM: i64 = ANNUAL_CAP_KM * 10;

/// The ATO's published cents-per-km rate, keyed by the year a financial
/// year *starts* in (`2025` = FY 2025–26, 1 July 2025 – 30 June 2026).
/// Each July, add the new year's line (and its row in the test below) once
/// the ATO publishes it; until then, a trip dated in that year is refused
/// rather than claimed at a guessed rate.
///
/// 2026–27 is 91c: a base rate of 89c plus a 2c uplift the ATO applied to
/// that year only.
pub const ATO_RATES: &[(i32, i64)] = &[
    (2020, 72),
    (2021, 72),
    (2022, 78),
    (2023, 85),
    (2024, 88),
    (2025, 88),
    (2026, 91),
];

/// The financial year (by its starting calendar year) a `YYYY-MM-DD` date
/// falls in: 1 July onward belongs to the year that starts that July.
pub fn financial_year_of(date: NaiveDate) -> i32 {
    if date.month() >= 7 {
        date.year()
    } else {
        date.year() - 1
    }
}

/// `2025` → `"2025–26"`, the form the ATO (and the web app) writes it in.
pub fn format_financial_year(start_year: i32) -> String {
    format!("{start_year}–{:02}", (start_year + 1).rem_euclid(100))
}

/// The first and last day of a financial year, as `YYYY-MM-DD` strings —
/// the inclusive `BETWEEN` bounds for a `date` sort-key query.
pub fn financial_year_bounds(start_year: i32) -> (String, String) {
    (
        format!("{start_year:04}-07-01"),
        format!("{:04}-06-30", start_year + 1),
    )
}

/// The cents-per-km rate for a financial year, or a complete sentence fit
/// to show the user when the table has no entry for it.
pub fn rate_for_financial_year(start_year: i32) -> Result<i64, String> {
    ATO_RATES
        .iter()
        .find(|(year, _)| *year == start_year)
        .map(|(_, rate)| *rate)
        .ok_or_else(|| {
            format!(
                "No ATO cents-per-km rate for FY {} is available yet",
                format_financial_year(start_year)
            )
        })
}

/// The rate for a trip on `date` (canonical `YYYY-MM-DD`).
pub fn rate_for_date(date: &str) -> Result<i64, String> {
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| format!("{date:?} is not a valid date (expected YYYY-MM-DD)"))?;
    rate_for_financial_year(financial_year_of(parsed))
}

/// Parse a user-typed distance (`"12"`, `"12.5"`, `".5"`) into tenths of a
/// km. Rejects blank input, anything but ASCII digits and at most one `.`,
/// more than 1 decimal place, zero, and anything over
/// [`MAX_DISTANCE_TENTHS_KM`].
pub fn parse_distance_km(raw: &str) -> Result<i64, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("Distance is required".to_string());
    }
    if s.starts_with('-') {
        return Err("Distance must be greater than zero".to_string());
    }
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    let well_formed = !(whole.is_empty() && frac.is_empty())
        && whole.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
        && !s.ends_with('.');
    if !well_formed {
        return Err(format!(
            "{s:?} is not a valid distance (use a number of km like 12 or 12.5)"
        ));
    }
    if frac.len() > 1 {
        return Err("Distance can have at most 1 decimal place".to_string());
    }
    let whole_trimmed = whole.trim_start_matches('0');
    if whole_trimmed.len() > 4 {
        return Err(format!(
            "A single trip cannot be more than {ANNUAL_CAP_KM} km"
        ));
    }
    let whole_value: i64 = if whole_trimmed.is_empty() {
        0
    } else {
        whole_trimmed
            .parse()
            .map_err(|_| format!("{s:?} is not a valid distance"))?
    };
    let frac_value = frac.bytes().next().map_or(0, |b| i64::from(b - b'0'));
    let tenths = whole_value * 10 + frac_value;
    if tenths <= 0 {
        return Err("Distance must be greater than zero".to_string());
    }
    if tenths > MAX_DISTANCE_TENTHS_KM {
        return Err(format!(
            "A single trip cannot be more than {ANNUAL_CAP_KM} km"
        ));
    }
    Ok(tenths)
}

/// Format tenths of a km as the shortest string that round-trips through
/// [`parse_distance_km`]: `120` → `"12"`, `125` → `"12.5"`.
pub fn format_distance_km(tenths: i64) -> String {
    match tenths % 10 {
        0 => format!("{}", tenths / 10),
        f => format!("{}.{}", tenths / 10, f.abs()),
    }
}

/// A trip's claim in cents: round-half-up(`distance_tenths_km ×
/// rate_cents_per_km / 10`). E.g. 12.5 km × 88c = 1,100c; 0.5 km × 91c =
/// 45.5c → 46c.
pub fn trip_amount_cents(distance_tenths_km: i64, rate_cents_per_km: i64) -> i64 {
    let product = i128::from(distance_tenths_km) * i128::from(rate_cents_per_km);
    i64::try_from((product + 5) / 10).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn financial_year_turns_over_on_1_july() {
        assert_eq!(financial_year_of(d("2026-06-30")), 2025);
        assert_eq!(financial_year_of(d("2026-07-01")), 2026);
        assert_eq!(financial_year_of(d("2026-01-01")), 2025);
        assert_eq!(financial_year_of(d("2025-12-31")), 2025);
    }

    #[test]
    fn financial_year_formatting_and_bounds() {
        assert_eq!(format_financial_year(2025), "2025–26");
        assert_eq!(format_financial_year(2099), "2099–00");
        assert_eq!(
            financial_year_bounds(2025),
            ("2025-07-01".to_string(), "2026-06-30".to_string())
        );
    }

    #[test]
    fn every_published_rate() {
        for (year, rate) in [
            (2020, 72),
            (2021, 72),
            (2022, 78),
            (2023, 85),
            (2024, 88),
            (2025, 88),
            (2026, 91),
        ] {
            assert_eq!(rate_for_financial_year(year), Ok(rate), "FY {year}");
        }
    }

    #[test]
    fn rate_for_date_uses_the_trips_financial_year() {
        assert_eq!(rate_for_date("2026-06-30"), Ok(88));
        assert_eq!(rate_for_date("2026-07-01"), Ok(91));
    }

    #[test]
    fn a_year_outside_the_table_is_an_error_not_a_guess() {
        assert_eq!(
            rate_for_financial_year(2027),
            Err("No ATO cents-per-km rate for FY 2027–28 is available yet".to_string())
        );
        assert!(rate_for_date("2020-06-30").is_err());
    }

    #[test]
    fn parse_distance_accepts_up_to_one_decimal_place() {
        assert_eq!(parse_distance_km("12"), Ok(120));
        assert_eq!(parse_distance_km(" 12.5 "), Ok(125));
        assert_eq!(parse_distance_km(".5"), Ok(5));
        assert_eq!(parse_distance_km("0005"), Ok(50));
        assert_eq!(parse_distance_km("5000"), Ok(MAX_DISTANCE_TENTHS_KM));
    }

    #[test]
    fn parse_distance_rejects_bad_input() {
        for bad in [
            "", "0", "0.0", "-1", "1.25", "1.", "abc", "1e3", "5000.1", "99999", "1,000",
        ] {
            assert!(parse_distance_km(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn format_distance_round_trips() {
        for tenths in [1, 5, 10, 125, 120, MAX_DISTANCE_TENTHS_KM] {
            assert_eq!(parse_distance_km(&format_distance_km(tenths)), Ok(tenths));
        }
        assert_eq!(format_distance_km(125), "12.5");
        assert_eq!(format_distance_km(120), "12");
    }

    #[test]
    fn trip_amount_rounds_half_up() {
        assert_eq!(trip_amount_cents(125, 88), 1_100);
        assert_eq!(trip_amount_cents(5, 91), 46); // 45.5c → 46c
        assert_eq!(trip_amount_cents(1, 84), 8); // 8.4c → 8c
        assert_eq!(trip_amount_cents(MAX_DISTANCE_TENTHS_KM, 91), 455_000);
    }
}
