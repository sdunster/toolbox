//! Renders a fictional sample invoice PDF for visual review — see
//! CLAUDE.md's "Invoicing" house rule ("PDF") and the design doc's "PDF (PR
//! 4)" section. Mirrors the structure of the product's original sample
//! invoice: a multi-line item with two `"* "` bullets, quantity 2 × 400.00,
//! no GST, and a bank-style payment-details block — all fictional.
//!
//! Usage: `cargo run --example render_sample_invoice -- <output.pdf> [gst|multipage|stress]`
//!
//! - no second argument: the single-page, no-GST sample.
//! - `gst`: the same invoice, but GST-registered.
//! - `multipage`: a longer, GST-registered invoice whose line table spans
//!   more than one page.
//! - `stress`: the single-page sample with every long-text field (client
//!   name, address, reference, seller email, payment details) pushed to
//!   its documented maximum — for visually confirming the wrapping/
//!   pagination fix.

use toolbox::invoicing::money;
use toolbox::invoicing::pdf::render_invoice_pdf;
use toolbox::invoicing::snapshot::{
    InvoiceSnapshot, InvoiceSnapshotBillTo, InvoiceSnapshotLine, InvoiceSnapshotSeller,
};

fn fictional_seller() -> InvoiceSnapshotSeller {
    InvoiceSnapshotSeller {
        name: Some("Fictional Trades Pty Ltd".into()),
        abn: Some("11 222 333 444".into()),
        address: Some("1 Fictional Street\nSomewhere NSW 2000".into()),
        phone: Some("0400 000 000".into()),
        email: Some("billing@fictionaltrades.example".into()),
    }
}

fn fictional_bill_to() -> InvoiceSnapshotBillTo {
    InvoiceSnapshotBillTo {
        name: "Example Client Pty Ltd".into(),
        abn: Some("55 666 777 888".into()),
        address: Some("2 Client Avenue\nElsewhere NSW 2000".into()),
    }
}

fn fictional_payment_details() -> String {
    "Please make all payments to the below account details:\n\
     Account name: Fictional Trades Pty Ltd\n\
     BSB: 000-000\n\
     Account number: 00000000\n\
     Reference: Invoice number"
        .to_string()
}

fn build_snapshot(gst_registered: bool, lines: Vec<InvoiceSnapshotLine>) -> InvoiceSnapshot {
    let subtotal_cents: i64 = lines.iter().map(|l| l.amount_cents).sum();
    let gst_cents = if gst_registered {
        money::gst_cents(subtotal_cents)
    } else {
        0
    };
    InvoiceSnapshot {
        schema_version: toolbox::invoicing::snapshot::SCHEMA_VERSION,
        title: if gst_registered {
            "Tax Invoice"
        } else {
            "Invoice"
        }
        .to_string(),
        display_number: Some("008".into()),
        issue_date: Some("2026-08-19".into()),
        seller: fictional_seller(),
        bill_to: fictional_bill_to(),
        reference: Some("42 Site Road, Elsewhere NSW 2000".into()),
        currency: "AUD".into(),
        gst_registered,
        lines,
        subtotal_cents,
        gst_cents,
        total_cents: subtotal_cents + gst_cents,
        payment_details: Some(fictional_payment_details()),
        due_date: Some("2026-09-02".into()),
        credit_note: None,
        no_gst_note: !gst_registered,
    }
}

fn simple_line(desc: &str, qty_hundredths: i64, price_cents: i64) -> InvoiceSnapshotLine {
    InvoiceSnapshotLine {
        date: "2026-08-19".into(),
        description: desc.into(),
        quantity: money::format_quantity(qty_hundredths),
        quantity_hundredths: qty_hundredths,
        unit_price_cents: price_cents,
        amount_cents: money::line_amount_cents(qty_hundredths, price_cents),
        gst_free: false,
    }
}

/// Mirrors the product's original sample: one multi-line item, two `"* "`
/// bullets, quantity 2 at $400.00.
fn single_page_sample() -> InvoiceSnapshot {
    let lines = vec![simple_line(
        "Site visit and consultation\n* Assessed existing installation\n* Prepared written report",
        200,
        40_000,
    )];
    build_snapshot(false, lines)
}

fn multipage_sample() -> InvoiceSnapshot {
    let mut lines: Vec<InvoiceSnapshotLine> = (1..=45)
        .map(|i| {
            simple_line(
                &format!(
                    "Line item {i}: general trades work\n* Materials and travel included\n- Café visit follow-up – client’s “special” request"
                ),
                150,
                12_345,
            )
        })
        .collect();
    lines.push(simple_line("Final inspection", 100, 20_000));
    build_snapshot(true, lines)
}

/// A stress test for the header-block wrapping/pagination fix (CLAUDE.md's
/// "Invoicing" house rule, "PDF (PR 4)"): every field that can carry
/// long user-entered text pushed to (or near) its documented maximum —
/// a ~200-char client name, a 60-line address, a ~2000-char payment-details
/// block, and a long unbroken token (a fake overlong email) with no spaces
/// to break on — all fictional, to visually confirm nothing overlaps,
/// crosses into the other column, or runs off the bottom of a page.
fn stress_sample() -> InvoiceSnapshot {
    let mut snap = single_page_sample();
    snap.bill_to.name =
        "Extraordinarily Long Fictional Trading Company Proprietary Limited ".repeat(4);
    snap.bill_to.name.truncate(200);
    snap.bill_to.address = Some(
        (1..=60)
            .map(|i| format!("Address line {i}, Somewhere NSW 2000"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    snap.reference = Some(
        "A very long job reference that keeps going and going, well past what a real one would ever need to say".into(),
    );
    snap.seller.email = Some(format!(
        "very-long-mailbox-name-{}@fictional-example.example",
        "x".repeat(260)
    ));
    let mut payment_details = "Please make payment within 14 days of the issue date to the account below, quoting the invoice number as the payment reference. ".repeat(20);
    payment_details.truncate(2000);
    snap.payment_details = Some(payment_details);
    snap
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let out_path = args.next().ok_or_else(|| {
        anyhow::anyhow!("usage: render_sample_invoice <output.pdf> [gst|multipage]")
    })?;
    let variant = args.next().unwrap_or_default();

    let snapshot = match variant.as_str() {
        "" => single_page_sample(),
        "gst" => {
            let mut snap = single_page_sample();
            snap.title = "Tax Invoice".into();
            snap.gst_registered = true;
            snap.no_gst_note = false;
            snap.gst_cents = money::gst_cents(snap.subtotal_cents);
            snap.total_cents = snap.subtotal_cents + snap.gst_cents;
            snap
        }
        "multipage" => multipage_sample(),
        "stress" => stress_sample(),
        other => anyhow::bail!("unknown variant {other:?} (expected gst, multipage or stress)"),
    };

    let bytes = render_invoice_pdf(&snapshot)?;
    std::fs::write(&out_path, &bytes)?;
    eprintln!("wrote {} bytes to {out_path}", bytes.len());
    Ok(())
}
