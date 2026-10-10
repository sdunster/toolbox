//! CSV exports for an accountant: invoices, credit notes, payments and
//! expenses over a date range. Pure — the caller reads the rows and the
//! projects they name. See CLAUDE.md's "Reports" house rule.
//!
//! Amounts are plain decimals (`1234.50`, no thousands separator, no
//! currency symbol) so a spreadsheet reads them as numbers. Every free-text
//! cell is quoted when it needs to be, and one that starts with `=`, `+`,
//! `-` or `@` gets a leading `'` so a spreadsheet can't run it as a formula
//! (CSV injection).

use std::collections::HashMap;

use crate::db;
use crate::invoicing::vehicle;

/// Which export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Export {
    Invoices,
    CreditNotes,
    Payments,
    Expenses,
}

/// Cents as a plain decimal: `123456` → `"1234.56"`, `-5` → `"-0.05"`.
pub fn plain_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// One free-text cell, made safe for a spreadsheet.
fn text(raw: &str) -> String {
    let defused = if raw.starts_with(['=', '+', '-', '@']) {
        format!("'{raw}")
    } else {
        raw.to_string()
    };
    quote(&defused)
}

/// Quote a cell if it contains a delimiter, quote or line break.
fn quote(cell: &str) -> String {
    if cell.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", cell.replace('"', "\"\""))
    } else {
        cell.to_string()
    }
}

fn line(cells: &[String]) -> String {
    let mut out = cells.join(",");
    out.push_str("\r\n");
    out
}

fn in_range(date: &str, from: &str, to: &str) -> bool {
    date >= from && date <= to
}

fn project_cells(projects: &HashMap<String, db::Project>, id: Option<&str>) -> (String, String) {
    match id.and_then(|id| projects.get(id)) {
        Some(p) => (text(&p.name), text(&p.client_name)),
        None => (String::new(), String::new()),
    }
}

/// Finalized invoices issued in `from..=to`, oldest first.
pub fn invoices(
    invoices: &[db::Invoice],
    projects: &HashMap<String, db::Project>,
    from: &str,
    to: &str,
) -> String {
    let mut out = line(
        &[
            "Number",
            "Issue date",
            "Due date",
            "Project",
            "Client",
            "Subtotal",
            "GST",
            "Total",
            "Credited",
            "Paid",
            "Balance",
            "Paid date",
        ]
        .map(String::from),
    );
    let mut rows: Vec<&db::Invoice> = invoices
        .iter()
        .filter(|i| {
            i.issue_date
                .as_deref()
                .is_some_and(|d| in_range(d, from, to))
        })
        .collect();
    rows.sort_by(|a, b| {
        a.issue_date
            .cmp(&b.issue_date)
            .then(a.number.cmp(&b.number))
    });
    for inv in rows {
        let total = inv.total_cents.unwrap_or(0);
        let gst = inv.gst_cents.unwrap_or(0);
        let (project, client) = project_cells(projects, Some(&inv.project_id));
        out.push_str(&line(&[
            inv.number.map(|n| format!("{n:03}")).unwrap_or_default(),
            inv.issue_date.clone().unwrap_or_default(),
            inv.due_date.clone().unwrap_or_default(),
            project,
            client,
            plain_cents(total - gst),
            plain_cents(gst),
            plain_cents(total),
            plain_cents(inv.credited_cents),
            plain_cents(inv.paid_cents()),
            plain_cents(inv.balance_cents()),
            inv.paid_date.clone().unwrap_or_default(),
        ]));
    }
    out
}

/// Credit notes issued in `from..=to`, oldest first. `invoice_numbers`
/// maps an invoice id to its display number.
pub fn credit_notes(
    notes: &[db::CreditNote],
    invoice_numbers: &HashMap<String, String>,
    projects: &HashMap<String, db::Project>,
    from: &str,
    to: &str,
) -> String {
    let mut out = line(
        &[
            "Number",
            "Issue date",
            "Invoice",
            "Project",
            "Client",
            "Reason",
            "Subtotal",
            "GST",
            "Total",
        ]
        .map(String::from),
    );
    let mut rows: Vec<&db::CreditNote> = notes
        .iter()
        .filter(|n| in_range(&n.issue_date, from, to))
        .collect();
    rows.sort_by(|a, b| {
        a.issue_date
            .cmp(&b.issue_date)
            .then(a.number.cmp(&b.number))
    });
    for note in rows {
        let (project, client) = project_cells(projects, Some(&note.project_id));
        out.push_str(&line(&[
            note.display_number(),
            note.issue_date.clone(),
            invoice_numbers
                .get(&note.invoice_id)
                .cloned()
                .unwrap_or_default(),
            project,
            client,
            text(&note.reason),
            plain_cents(note.subtotal_cents),
            plain_cents(note.gst_cents),
            plain_cents(note.total_cents),
        ]));
    }
    out
}

/// Every payment received in `from..=to`, oldest first.
pub fn payments(
    invoices: &[db::Invoice],
    projects: &HashMap<String, db::Project>,
    from: &str,
    to: &str,
) -> String {
    let mut out =
        line(&["Date", "Invoice", "Project", "Client", "Amount", "Note"].map(String::from));
    let mut rows: Vec<(&db::InvoicePayment, &db::Invoice)> = invoices
        .iter()
        .flat_map(|inv| inv.payments.iter().map(move |p| (p, inv)))
        .filter(|(p, _)| in_range(&p.date, from, to))
        .collect();
    rows.sort_by(|a, b| a.0.date.cmp(&b.0.date).then(a.1.number.cmp(&b.1.number)));
    for (p, inv) in rows {
        let (project, client) = project_cells(projects, Some(&inv.project_id));
        out.push_str(&line(&[
            p.date.clone(),
            inv.number.map(|n| format!("{n:03}")).unwrap_or_default(),
            project,
            client,
            plain_cents(p.amount_cents),
            text(p.note.as_deref().unwrap_or("")),
        ]));
    }
    out
}

/// Expenses dated `from..=to`, oldest first.
pub fn expenses(
    expenses: &[db::Expense],
    projects: &HashMap<String, db::Project>,
    from: &str,
    to: &str,
) -> String {
    let mut out = line(
        &[
            "Date",
            "Category",
            "Supplier",
            "Description",
            "Project",
            "Amount",
            "GST",
            "Amount ex GST",
            "Distance (km)",
            "Rate (c/km)",
            "Re-billed",
        ]
        .map(String::from),
    );
    let mut rows: Vec<&db::Expense> = expenses
        .iter()
        .filter(|e| in_range(&e.fields.date, from, to))
        .collect();
    rows.sort_by(|a, b| {
        a.fields
            .date
            .cmp(&b.fields.date)
            .then(a.created_at.cmp(&b.created_at))
    });
    for e in rows {
        let (supplier, distance, rate) = match &e.fields.detail {
            db::ExpenseDetail::Purchase { supplier, .. } => {
                (text(supplier), String::new(), String::new())
            }
            db::ExpenseDetail::VehicleKm {
                distance_tenths_km,
                rate_cents_per_km,
            } => (
                String::new(),
                vehicle::format_distance_km(*distance_tenths_km),
                rate_cents_per_km.to_string(),
            ),
        };
        let (project, _) = project_cells(projects, e.fields.project_id.as_deref());
        out.push_str(&line(&[
            e.fields.date.clone(),
            text(e.fields.category.label()),
            supplier,
            text(e.fields.description.as_deref().unwrap_or("")),
            project,
            plain_cents(e.amount_cents()),
            plain_cents(e.gst_cents()),
            plain_cents(e.amount_ex_gst_cents()),
            distance,
            rate,
            if e.billable_item_id.is_some() {
                "yes"
            } else {
                ""
            }
            .to_string(),
        ]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_cents_has_no_separators() {
        assert_eq!(plain_cents(123_456), "1234.56");
        assert_eq!(plain_cents(-5), "-0.05");
        assert_eq!(plain_cents(0), "0.00");
    }

    #[test]
    fn text_quotes_and_defuses_formulas() {
        assert_eq!(text("plain"), "plain");
        assert_eq!(text("a, b"), "\"a, b\"");
        assert_eq!(text("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(text("=SUM(A1)"), "'=SUM(A1)");
        assert_eq!(text("line\nbreak"), "\"line\nbreak\"");
    }

    #[test]
    fn expenses_export_filters_by_date_and_marks_rebilled() {
        let mk = |date: &str, rebilled: bool| db::Expense {
            id: date.into(),
            instance_id: "i".into(),
            fields: db::ExpenseFields {
                project_id: None,
                date: date.into(),
                category: db::ExpenseCategory::Materials,
                description: Some("Timber, treated".into()),
                detail: db::ExpenseDetail::Purchase {
                    supplier: "Hardware Co".into(),
                    amount_cents: 11_000,
                    gst_cents: Some(1_000),
                },
            },
            billable_item_id: rebilled.then(|| "item".to_string()),
            receipt: None,
            created_by_user_id: "u".into(),
            created_at: 0,
            updated_at: 0,
        };
        let csv = expenses(
            &[mk("2026-07-01", true), mk("2026-06-30", false)],
            &HashMap::new(),
            "2026-07-01",
            "2026-07-31",
        );
        let lines: Vec<&str> = csv.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[1],
            "2026-07-01,Materials & supplies,Hardware Co,\"Timber, treated\",,110.00,10.00,100.00,,,yes"
        );
    }
}
