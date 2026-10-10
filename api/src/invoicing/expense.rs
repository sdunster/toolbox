//! Expense input validation — see CLAUDE.md's "Expenses" house rule. The
//! one place a [`db::ExpenseFields`] is built from caller input, shared by
//! `createExpense`/`updateExpense`, so the category ↔ shape rule (a
//! [`db::ExpenseCategory::VehicleKm`] expense is always a trip, every other
//! category always a purchase) can't drift between them.

use crate::db;
use crate::invoicing::{self, vehicle};

/// Longest supplier name, in characters.
pub const MAX_SUPPLIER_LEN: usize = 200;

/// Largest single purchase, in cents: 10,000,000.00 (GST-inclusive) — the
/// same ceiling as a billable item's unit price.
pub const MAX_AMOUNT_CENTS: i64 = invoicing::money::MAX_UNIT_PRICE_CENTS;

/// An expense as the caller sent it, before validation. Purchase-only and
/// trip-only fields are all optional here; [`validate_expense_input`]
/// decides which the category requires and which it refuses.
#[derive(Clone, Debug, Default)]
pub struct ExpenseInput<'a> {
    pub project_id: Option<String>,
    pub date: &'a str,
    pub category: Option<db::ExpenseCategory>,
    pub description: Option<&'a str>,
    pub supplier: Option<&'a str>,
    pub amount_cents: Option<i64>,
    pub gst_cents: Option<i64>,
    pub distance_km: Option<&'a str>,
}

fn non_blank(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|s| !s.is_empty())
}

/// Validate and normalise an expense. Errors are complete sentences fit to
/// show the user.
///
/// - Every expense: `date` is a canonical `YYYY-MM-DD`; `description` is
///   trimmed, blank means none, ≤ 2000 chars.
/// - A purchase: `supplier` (≤ 200 chars) and `amountCents` (GST-inclusive,
///   `0 < a ≤ 1,000,000,000`) are required; `gstCents` is optional,
///   `0 ≤ g ≤ amountCents`; `distanceKm` is refused.
/// - A vehicle trip: `distanceKm` and `description` (its business purpose)
///   are required; `supplier`/`amountCents`/`gstCents` are refused — the
///   amount comes from distance × the ATO rate for the date's financial
///   year, which is looked up here and never accepted from the caller.
pub fn validate_expense_input(input: ExpenseInput<'_>) -> Result<db::ExpenseFields, String> {
    let category = input
        .category
        .ok_or_else(|| "Category is required".to_string())?;
    let date = invoicing::validate_item_date(input.date)?;
    let description = match non_blank(input.description) {
        Some(d) => Some(invoicing::validate_description(d)?),
        None => None,
    };
    let supplier = non_blank(input.supplier);
    let distance = non_blank(input.distance_km);

    let detail = if category == db::ExpenseCategory::VehicleKm {
        if supplier.is_some() || input.amount_cents.is_some() || input.gst_cents.is_some() {
            return Err(
                "A vehicle trip takes a distance, not a supplier, amount or GST".to_string(),
            );
        }
        let distance = distance.ok_or_else(|| "Distance is required".to_string())?;
        if description.is_none() {
            return Err("Describe the trip's business purpose".to_string());
        }
        db::ExpenseDetail::VehicleKm {
            distance_tenths_km: vehicle::parse_distance_km(distance)?,
            rate_cents_per_km: vehicle::rate_for_date(&date)?,
        }
    } else {
        if distance.is_some() {
            return Err("Only a vehicle trip (cents per km) takes a distance".to_string());
        }
        let supplier = supplier.ok_or_else(|| "Supplier is required".to_string())?;
        if supplier.chars().count() > MAX_SUPPLIER_LEN {
            return Err(format!(
                "Supplier cannot be longer than {MAX_SUPPLIER_LEN} characters"
            ));
        }
        let amount_cents = input
            .amount_cents
            .ok_or_else(|| "Amount is required".to_string())?;
        if amount_cents <= 0 {
            return Err("Amount must be greater than zero".to_string());
        }
        if amount_cents > MAX_AMOUNT_CENTS {
            return Err("Amount cannot be more than 10,000,000.00".to_string());
        }
        if let Some(gst) = input.gst_cents {
            if gst < 0 {
                return Err("GST cannot be negative".to_string());
            }
            if gst > amount_cents {
                return Err("GST cannot be more than the amount".to_string());
            }
        }
        db::ExpenseDetail::Purchase {
            supplier: supplier.to_string(),
            amount_cents,
            gst_cents: input.gst_cents,
        }
    };

    Ok(db::ExpenseFields {
        project_id: input.project_id,
        date,
        category,
        description,
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn purchase() -> ExpenseInput<'static> {
        ExpenseInput {
            date: "2026-08-03",
            category: Some(db::ExpenseCategory::Materials),
            supplier: Some("  Bunnings "),
            amount_cents: Some(11_000),
            gst_cents: Some(1_000),
            ..Default::default()
        }
    }

    fn trip() -> ExpenseInput<'static> {
        ExpenseInput {
            date: "2026-07-01",
            category: Some(db::ExpenseCategory::VehicleKm),
            description: Some("Site visit"),
            distance_km: Some("12.5"),
            ..Default::default()
        }
    }

    #[test]
    fn a_purchase_is_trimmed_and_kept() {
        let f = validate_expense_input(purchase()).unwrap();
        assert_eq!(f.description, None);
        assert_eq!(
            f.detail,
            db::ExpenseDetail::Purchase {
                supplier: "Bunnings".to_string(),
                amount_cents: 11_000,
                gst_cents: Some(1_000),
            }
        );
    }

    #[test]
    fn a_purchase_needs_supplier_and_a_positive_amount() {
        for bad in [
            ExpenseInput {
                supplier: Some("  "),
                ..purchase()
            },
            ExpenseInput {
                amount_cents: None,
                ..purchase()
            },
            ExpenseInput {
                amount_cents: Some(0),
                ..purchase()
            },
            ExpenseInput {
                amount_cents: Some(MAX_AMOUNT_CENTS + 1),
                ..purchase()
            },
            ExpenseInput {
                gst_cents: Some(-1),
                ..purchase()
            },
            ExpenseInput {
                gst_cents: Some(11_001),
                ..purchase()
            },
            ExpenseInput {
                distance_km: Some("3"),
                ..purchase()
            },
            ExpenseInput {
                category: None,
                ..purchase()
            },
            ExpenseInput {
                date: "2026-8-3",
                ..purchase()
            },
        ] {
            assert!(validate_expense_input(bad.clone()).is_err(), "{bad:?}");
        }
        let gst_free = ExpenseInput {
            gst_cents: None,
            ..purchase()
        };
        assert!(validate_expense_input(gst_free).is_ok());
    }

    #[test]
    fn a_trip_takes_its_rate_from_the_date() {
        let f = validate_expense_input(trip()).unwrap();
        assert_eq!(
            f.detail,
            db::ExpenseDetail::VehicleKm {
                distance_tenths_km: 125,
                rate_cents_per_km: 91
            }
        );
        let earlier = ExpenseInput {
            date: "2026-06-30",
            ..trip()
        };
        assert_eq!(
            validate_expense_input(earlier).unwrap().detail,
            db::ExpenseDetail::VehicleKm {
                distance_tenths_km: 125,
                rate_cents_per_km: 88
            }
        );
    }

    #[test]
    fn a_trip_refuses_purchase_fields_and_needs_a_purpose() {
        for bad in [
            ExpenseInput {
                supplier: Some("Shell"),
                ..trip()
            },
            ExpenseInput {
                amount_cents: Some(100),
                ..trip()
            },
            ExpenseInput {
                gst_cents: Some(0),
                ..trip()
            },
            ExpenseInput {
                description: Some("  "),
                ..trip()
            },
            ExpenseInput {
                distance_km: None,
                ..trip()
            },
            ExpenseInput {
                date: "2030-01-01",
                ..trip()
            },
        ] {
            assert!(validate_expense_input(bad.clone()).is_err(), "{bad:?}");
        }
    }
}
