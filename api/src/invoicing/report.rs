//! Reports over an invoicing instance's finalized invoices, credit notes,
//! payments and expenses — pure functions over rows the caller has already
//! read. See CLAUDE.md's "Reports" house rule.
//!
//! - [`gst_summary`]: the figures a BAS asks for (G1 total sales, 1A GST on
//!   sales, 1B GST on purchases) over a date range, on a cash or accrual
//!   basis.
//! - [`aging`]: unpaid invoices bucketed by how far past due they are.
//! - [`project_financials`]: what one project has invoiced, been paid,
//!   and cost.
//!
//! Every amount is integer cents, like the rest of invoicing.

use chrono::NaiveDate;

use crate::db;
use crate::invoicing::money;

/// Which date makes a sale count in a period.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Basis {
    /// When the money arrives: each payment's date, with its GST share
    /// apportioned from the invoice it pays.
    Cash,
    /// When the invoice (or credit note) is issued.
    Accrual,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GstSummary {
    /// G1: total sales, GST-inclusive, net of credit notes.
    pub sales_cents: i64,
    /// 1A: GST on those sales.
    pub gst_on_sales_cents: i64,
    /// Every expense in the period, GST-inclusive (vehicle trips included —
    /// they carry no GST).
    pub purchases_cents: i64,
    /// 1B: GST included in those expenses.
    pub gst_on_purchases_cents: i64,
    pub invoice_count: usize,
    pub credit_note_count: usize,
    pub payment_count: usize,
    pub expense_count: usize,
}

impl GstSummary {
    /// 1A − 1B: positive means GST to pay, negative a refund.
    pub fn net_gst_cents(&self) -> i64 {
        self.gst_on_sales_cents - self.gst_on_purchases_cents
    }
}

fn in_range(date: &str, from: &str, to: &str) -> bool {
    date >= from && date <= to
}

/// `amount × part / whole`, rounded half-up, in `i128` so nothing overflows.
/// Zero when `whole` isn't positive.
pub fn apportion(amount: i64, part: i64, whole: i64) -> i64 {
    if whole <= 0 {
        return 0;
    }
    let num = i128::from(amount) * i128::from(part);
    let den = i128::from(whole);
    let rounded = if num >= 0 {
        (num + den / 2) / den
    } else {
        -((-num + den / 2) / den)
    };
    i64::try_from(rounded).unwrap_or(0)
}

/// The GST summary for `from..=to` (`YYYY-MM-DD`). `invoices` must be
/// finalized; `expenses` are counted by their own date on either basis
/// (Toolbox records when an expense happened, not when it was paid).
///
/// - **Accrual:** each invoice issued in the period adds its total and GST;
///   each credit note issued in the period subtracts its own.
/// - **Cash:** each payment received in the period adds its amount, and the
///   invoice's GST × amount ÷ invoice total as its GST share — where the
///   invoice's total and GST are taken *net of its credit notes*, so a
///   credited invoice's payments carry proportionally less GST. Credit
///   notes have no cash effect of their own.
pub fn gst_summary(
    basis: Basis,
    from: &str,
    to: &str,
    invoices: &[db::Invoice],
    credit_notes: &[db::CreditNote],
    expenses: &[db::Expense],
) -> GstSummary {
    let mut out = GstSummary::default();
    match basis {
        Basis::Accrual => {
            for inv in invoices {
                if inv
                    .issue_date
                    .as_deref()
                    .is_some_and(|d| in_range(d, from, to))
                {
                    out.sales_cents += inv.total_cents.unwrap_or(0);
                    out.gst_on_sales_cents += inv.gst_cents.unwrap_or(0);
                    out.invoice_count += 1;
                }
            }
            for note in credit_notes {
                if in_range(&note.issue_date, from, to) {
                    out.sales_cents -= note.total_cents;
                    out.gst_on_sales_cents -= note.gst_cents;
                    out.credit_note_count += 1;
                }
            }
        }
        Basis::Cash => {
            for inv in invoices {
                let net_total = inv.total_cents.unwrap_or(0) - inv.credited_cents;
                let net_gst = inv.gst_cents.unwrap_or(0) - inv.credited_gst_cents;
                let mut counted = false;
                for p in inv.payments.iter().filter(|p| in_range(&p.date, from, to)) {
                    out.sales_cents += p.amount_cents;
                    // Never more GST than the invoice carries, even when
                    // overpaid.
                    out.gst_on_sales_cents +=
                        apportion(p.amount_cents.min(net_total.max(0)), net_gst, net_total);
                    out.payment_count += 1;
                    counted = true;
                }
                if counted {
                    out.invoice_count += 1;
                }
            }
        }
    }
    for e in expenses
        .iter()
        .filter(|e| in_range(&e.fields.date, from, to))
    {
        out.purchases_cents += e.amount_cents();
        out.gst_on_purchases_cents += e.gst_cents();
        out.expense_count += 1;
    }
    out
}

/// Whole days from `due` to `today` (both `YYYY-MM-DD`): positive when
/// `today` is past `due`. `0` when either fails to parse.
pub fn days_between(due: &str, today: &str) -> i64 {
    match (
        NaiveDate::parse_from_str(due, "%Y-%m-%d"),
        NaiveDate::parse_from_str(today, "%Y-%m-%d"),
    ) {
        (Ok(d), Ok(t)) => (t - d).num_days(),
        _ => 0,
    }
}

/// Outstanding balances bucketed by days past due.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Aging {
    /// Not yet due (or due today).
    pub current_cents: i64,
    pub days_1_to_30_cents: i64,
    pub days_31_to_60_cents: i64,
    pub days_61_to_90_cents: i64,
    pub days_over_90_cents: i64,
}

impl Aging {
    pub fn total_cents(&self) -> i64 {
        self.current_cents
            + self.days_1_to_30_cents
            + self.days_31_to_60_cents
            + self.days_61_to_90_cents
            + self.days_over_90_cents
    }
}

/// Bucket every finalized invoice with a positive balance by how many days
/// past its due date `today` is. Returns the buckets and the outstanding
/// invoices, most overdue first.
pub fn aging<'a>(invoices: &'a [db::Invoice], today: &str) -> (Aging, Vec<&'a db::Invoice>) {
    let mut out = Aging::default();
    let mut outstanding: Vec<&db::Invoice> = invoices
        .iter()
        .filter(|i| i.status == db::InvoiceStatus::Finalized && i.balance_cents() > 0)
        .collect();
    for inv in &outstanding {
        let balance = inv.balance_cents();
        let days = inv
            .due_date
            .as_deref()
            .map(|d| days_between(d, today))
            .unwrap_or(0);
        let bucket = match days {
            d if d <= 0 => &mut out.current_cents,
            1..=30 => &mut out.days_1_to_30_cents,
            31..=60 => &mut out.days_31_to_60_cents,
            61..=90 => &mut out.days_61_to_90_cents,
            _ => &mut out.days_over_90_cents,
        };
        *bucket += balance;
    }
    outstanding.sort_by(|a, b| {
        a.due_date
            .cmp(&b.due_date)
            .then_with(|| a.number.cmp(&b.number))
    });
    (out, outstanding)
}

/// One project's money, all GST-exclusive except where named.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectFinancials {
    /// Finalized invoices, net of credit notes, before GST.
    pub invoiced_cents: i64,
    /// The same, GST-inclusive — what the client was asked to pay.
    pub invoiced_incl_gst_cents: i64,
    pub paid_cents: i64,
    /// Still owed across the project's invoices (never negative).
    pub outstanding_cents: i64,
    /// Expenses on the project, before GST.
    pub expenses_cents: i64,
    /// Billable items not yet on any invoice, before GST.
    pub unbilled_cents: i64,
    /// `invoiced_cents − expenses_cents`.
    pub profit_cents: i64,
}

pub fn project_financials(
    invoices: &[db::Invoice],
    expenses: &[db::Expense],
    unbilled_items: &[db::BillableItem],
) -> ProjectFinancials {
    let mut out = ProjectFinancials::default();
    for inv in invoices
        .iter()
        .filter(|i| i.status == db::InvoiceStatus::Finalized)
    {
        let total = inv.total_cents.unwrap_or(0);
        let gst = inv.gst_cents.unwrap_or(0);
        out.invoiced_incl_gst_cents += total - inv.credited_cents;
        out.invoiced_cents += (total - gst) - (inv.credited_cents - inv.credited_gst_cents);
        out.paid_cents += inv.paid_cents();
        out.outstanding_cents += inv.balance_cents().max(0);
    }
    out.expenses_cents = expenses.iter().map(db::Expense::amount_ex_gst_cents).sum();
    out.unbilled_cents = unbilled_items
        .iter()
        .map(|i| money::line_amount_cents(i.quantity_hundredths, i.unit_price_cents))
        .sum();
    out.profit_cents = out.invoiced_cents - out.expenses_cents;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invoice(issue: &str, due: &str, total: i64, gst: i64) -> db::Invoice {
        db::Invoice {
            id: format!("inv-{issue}-{total}"),
            instance_id: "i".into(),
            project_id: "p".into(),
            status: db::InvoiceStatus::Finalized,
            version: 2,
            item_ids: vec![],
            created_by_user_id: "u".into(),
            created_at: 0,
            updated_at: 0,
            number: Some(1),
            issue_date: Some(issue.into()),
            snapshot: None,
            total_cents: Some(total),
            finalized_at: None,
            finalized_by_user_id: None,
            paid_date: None,
            pdf_s3_key: None,
            due_date: Some(due.into()),
            gst_cents: Some(gst),
            payments: vec![],
            credited_cents: 0,
            credited_gst_cents: 0,
            sent_at: None,
            sent_to: vec![],
        }
    }

    fn payment(date: &str, amount: i64) -> db::InvoicePayment {
        db::InvoicePayment {
            id: date.into(),
            date: date.into(),
            amount_cents: amount,
            note: None,
            recorded_by_user_id: "u".into(),
            recorded_at: 0,
        }
    }

    fn credit_note(issue: &str, total: i64, gst: i64) -> db::CreditNote {
        db::CreditNote {
            id: "cn".into(),
            instance_id: "i".into(),
            invoice_id: "inv".into(),
            project_id: "p".into(),
            number: 1,
            issue_date: issue.into(),
            reason: "r".into(),
            snapshot: "{}".into(),
            subtotal_cents: total - gst,
            gst_cents: gst,
            total_cents: total,
            created_by_user_id: "u".into(),
            created_at: 0,
            pdf_s3_key: None,
            sent_at: None,
            sent_to: vec![],
        }
    }

    fn purchase(date: &str, amount: i64, gst: Option<i64>) -> db::Expense {
        db::Expense {
            id: format!("e-{date}"),
            instance_id: "i".into(),
            fields: db::ExpenseFields {
                project_id: None,
                date: date.into(),
                category: db::ExpenseCategory::Materials,
                description: None,
                detail: db::ExpenseDetail::Purchase {
                    supplier: "S".into(),
                    amount_cents: amount,
                    gst_cents: gst,
                },
            },
            billable_item_id: None,
            receipt: None,
            created_by_user_id: "u".into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn accrual_counts_issue_dates_and_nets_credit_notes() {
        let invoices = [
            invoice("2026-07-05", "2026-07-19", 11_000, 1_000),
            invoice("2026-06-30", "2026-07-14", 22_000, 2_000), // previous quarter
        ];
        let notes = [credit_note("2026-08-01", 1_100, 100)];
        let expenses = [
            purchase("2026-07-10", 5_500, Some(500)),
            purchase("2026-07-11", 1_000, None),
        ];
        let s = gst_summary(
            Basis::Accrual,
            "2026-07-01",
            "2026-09-30",
            &invoices,
            &notes,
            &expenses,
        );
        assert_eq!(s.sales_cents, 9_900);
        assert_eq!(s.gst_on_sales_cents, 900);
        assert_eq!(s.purchases_cents, 6_500);
        assert_eq!(s.gst_on_purchases_cents, 500);
        assert_eq!(s.net_gst_cents(), 400);
        assert_eq!(
            (s.invoice_count, s.credit_note_count, s.expense_count),
            (1, 1, 2)
        );
    }

    #[test]
    fn cash_apportions_gst_to_each_payment() {
        let mut inv = invoice("2026-06-20", "2026-07-04", 11_000, 1_000);
        inv.payments = vec![payment("2026-06-30", 5_500), payment("2026-07-02", 5_500)];
        let s = gst_summary(Basis::Cash, "2026-07-01", "2026-09-30", &[inv], &[], &[]);
        assert_eq!(s.sales_cents, 5_500);
        assert_eq!(s.gst_on_sales_cents, 500);
        assert_eq!((s.invoice_count, s.payment_count), (1, 1));
    }

    #[test]
    fn cash_nets_credits_out_of_the_gst_share() {
        let mut inv = invoice("2026-07-01", "2026-07-15", 11_000, 1_000);
        inv.credited_cents = 5_500;
        inv.credited_gst_cents = 500;
        inv.payments = vec![payment("2026-07-10", 5_500)];
        let s = gst_summary(Basis::Cash, "2026-07-01", "2026-09-30", &[inv], &[], &[]);
        assert_eq!(s.sales_cents, 5_500);
        assert_eq!(s.gst_on_sales_cents, 500);
    }

    #[test]
    fn aging_buckets_by_days_past_due() {
        let mut paid = invoice("2026-01-01", "2026-01-15", 1_000, 0);
        paid.payments = vec![payment("2026-01-10", 1_000)];
        let invoices = [
            invoice("2026-09-01", "2026-10-20", 100, 0), // not due
            invoice("2026-09-01", "2026-10-01", 200, 0), // 9 days
            invoice("2026-07-01", "2026-08-01", 300, 0), // 70 days
            invoice("2026-01-01", "2026-02-01", 400, 0), // > 90
            paid,
        ];
        let (a, outstanding) = aging(&invoices, "2026-10-10");
        assert_eq!(a.current_cents, 100);
        assert_eq!(a.days_1_to_30_cents, 200);
        assert_eq!(a.days_31_to_60_cents, 0);
        assert_eq!(a.days_61_to_90_cents, 300);
        assert_eq!(a.days_over_90_cents, 400);
        assert_eq!(a.total_cents(), 1_000);
        assert_eq!(outstanding.len(), 4);
        assert_eq!(outstanding[0].due_date.as_deref(), Some("2026-02-01"));
    }

    #[test]
    fn project_financials_net_credits_and_gst() {
        let mut inv = invoice("2026-07-01", "2026-07-15", 11_000, 1_000);
        inv.credited_cents = 1_100;
        inv.credited_gst_cents = 100;
        inv.payments = vec![payment("2026-07-10", 4_000)];
        let expenses = [purchase("2026-07-02", 2_200, Some(200))];
        let f = project_financials(&[inv], &expenses, &[]);
        assert_eq!(f.invoiced_cents, 9_000);
        assert_eq!(f.invoiced_incl_gst_cents, 9_900);
        assert_eq!(f.paid_cents, 4_000);
        assert_eq!(f.outstanding_cents, 5_900);
        assert_eq!(f.expenses_cents, 2_000);
        assert_eq!(f.profit_cents, 7_000);
    }

    #[test]
    fn apportion_rounds_half_up_and_guards_zero() {
        assert_eq!(apportion(5_500, 1_000, 11_000), 500);
        assert_eq!(apportion(1, 1, 2), 1);
        assert_eq!(apportion(100, 10, 0), 0);
    }
}
