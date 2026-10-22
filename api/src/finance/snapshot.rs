//! The frozen JSON an invoice prints — see CLAUDE.md's "Finance" house
//! rule ("Snapshot on finalize"). [`InvoiceSnapshot`] is what
//! `finalize_invoice` serializes onto the `invoice` row's `snapshot`
//! attribute, and what a draft invoice's GraphQL fields build live, every
//! time, from the client/project/instance/items as they currently stand —
//! the same pure function, [`build_snapshot`], produces both, so the draft
//! preview and the frozen finalized version can never diverge in shape.
//!
//! `schema_version` exists so a future change to this shape has somewhere
//! to branch on old rows without a migration. A credit note is the same
//! printable shape with its `credit_note` block set (see
//! [`build_credit_note_snapshot`]). Version 4 is the first stored shape:
//! every finance row from before it was deleted when it shipped. Add a
//! field as `#[serde(default)]` so the rows written since keep parsing.

use serde::{Deserialize, Serialize};

use crate::db;
use crate::finance::money;

/// The current snapshot shape.
pub const SCHEMA_VERSION: u32 = 4;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvoiceSnapshotSeller {
    pub name: Option<String>,
    /// The seller's legal name, when the invoice went out under a brand
    /// with a different name — printed as "{legal_name} trading as {name}".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legal_name: Option<String>,
    pub abn: Option<String>,
    pub address: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvoiceSnapshotBillTo {
    pub name: String,
    pub abn: Option<String>,
    pub address: Option<String>,
}

/// One printed line. `quantity` is the same shortest-decimal-string form
/// `BillableItem.quantity`/`BillableItemInput.quantity` use
/// (`finance::money::format_quantity`); `quantity_hundredths` rides along
/// too so a future PDF/report never needs to re-parse the display string.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvoiceSnapshotLine {
    pub date: String,
    pub description: String,
    pub quantity: String,
    pub quantity_hundredths: i64,
    /// Negative on a discount line, like `amount_cents`.
    pub unit_price_cents: i64,
    pub amount_cents: i64,
    /// What the quantity counts (`hrs`), printed after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// A discount line: printed with its (negative) amount only, no
    /// quantity or unit price.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub discount: bool,
    /// No GST on this line, even on a GST-registered invoice.
    #[serde(default)]
    pub gst_free: bool,
}

/// What a credit note prints about the invoice it adjusts. Present only on
/// a credit note's snapshot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CreditNoteSnapshotInfo {
    /// The adjusted invoice's display number (`"008"`).
    pub invoice_display_number: String,
    /// `YYYY-MM-DD`.
    pub invoice_issue_date: String,
    pub reason: String,
}

/// One line of a credit note, as `issueCreditNote` takes it: a GST-exclusive
/// amount being credited. Negative only when a full credit carries an
/// invoice's discount line across.
#[derive(Clone, Debug, PartialEq)]
pub struct CreditLine {
    pub description: String,
    pub amount_cents: i64,
    pub gst_free: bool,
}

/// Everything an invoice prints, frozen at finalization (or built live, for
/// a draft's preview) by [`build_snapshot`]. See that function's doc
/// comment for how each field is derived.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvoiceSnapshot {
    pub schema_version: u32,
    /// "Tax Invoice" when the instance is GST-registered, else "Invoice".
    pub title: String,
    /// The number as printed, in the instance's `db::NumberFormat`
    /// (`"INV-0042"`, `"CN-003"`). `None` for a draft preview — a draft has
    /// no number yet.
    pub display_number: Option<String>,
    /// `YYYY-MM-DD`. `None` for a draft preview.
    pub issue_date: Option<String>,
    pub seller: InvoiceSnapshotSeller,
    pub bill_to: InvoiceSnapshotBillTo,
    pub reference: Option<String>,
    /// ISO 4217-shaped code (`db::Instance::currency_or_default`).
    pub currency: String,
    /// When `false`, "No GST has been charged." is printed.
    pub gst_registered: bool,
    /// Sorted by `date`, then `created_at` — see [`build_snapshot`]'s doc
    /// comment.
    pub lines: Vec<InvoiceSnapshotLine>,
    pub subtotal_cents: i64,
    /// `0` when `gst_registered` is `false`.
    pub gst_cents: i64,
    pub total_cents: i64,
    /// Free text printed above the payment details.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub payment_details: Option<String>,
    /// `YYYY-MM-DD`. `None` for a draft preview and for a credit note.
    #[serde(default)]
    pub due_date: Option<String>,
    /// Set only on a credit note's snapshot.
    #[serde(default)]
    pub credit_note: Option<CreditNoteSnapshotInfo>,
}

/// What an invoice is built from: everything [`build_snapshot`] reads.
#[derive(Clone, Copy, Debug)]
pub struct SnapshotSource<'a> {
    pub instance: &'a db::Instance,
    pub client: &'a db::Client,
    pub project: &'a db::Project,
    /// The invoice's own reference (PO number), which wins over the
    /// project's.
    pub reference: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub items: &'a [db::BillableItem],
}

/// Build an invoice's printable content from its client/project/instance/
/// items, with no I/O and no side effects — the one function
/// `finalize_invoice` calls to produce the frozen snapshot, and a draft
/// invoice's GraphQL resolvers call (with `number: None, issue_date: None`)
/// to produce the live preview. Because both paths run the exact same code,
/// a draft's on-screen preview and a finalized invoice's PDF can never
/// disagree about how a value is computed — only about *which* rows were
/// read.
///
/// - `title`: "Tax Invoice" iff `instance.gst_registered`, else "Invoice".
/// - `display_number`: `number` in the instance's `NumberFormat`, or `None`.
/// - `seller`/`payment_details`: the instance's business settings, with the
///   project's brand's overrides applied (`finance::brand::resolve`).
/// - `bill_to`: the client's name/ABN/address, verbatim.
/// - `reference`: the invoice's own, else the project's.
/// - `lines`: `items` sorted by `date`, then `created_at` (the tie-break
///   that makes same-day lines print in the order they were recorded), each
///   amount `BillableItem::amount_cents` (negative for a discount).
/// - `subtotal_cents`/`gst_cents`/`total_cents`: [`totals`].
/// - `currency`: the instance's (`currency_or_default`) — never per brand.
pub fn build_snapshot(
    source: &SnapshotSource<'_>,
    number: Option<u32>,
    issue_date: Option<&str>,
    due_date: Option<&str>,
) -> InvoiceSnapshot {
    let SnapshotSource {
        instance,
        client,
        project,
        reference,
        notes,
        items,
    } = *source;
    let gst_registered = instance.gst_registered;
    let title = if gst_registered {
        "Tax Invoice"
    } else {
        "Invoice"
    }
    .to_string();

    let mut sorted_items: Vec<&db::BillableItem> = items.iter().collect();
    sorted_items.sort_by(|a, b| a.date.cmp(&b.date).then(a.created_at.cmp(&b.created_at)));

    let lines: Vec<InvoiceSnapshotLine> = sorted_items
        .into_iter()
        .map(|item| InvoiceSnapshotLine {
            date: item.date.clone(),
            description: item.description.clone(),
            quantity: money::format_quantity(item.quantity_hundredths),
            quantity_hundredths: item.quantity_hundredths,
            unit_price_cents: if item.discount {
                -item.unit_price_cents
            } else {
                item.unit_price_cents
            },
            amount_cents: item.amount_cents(),
            unit: item.unit.clone(),
            discount: item.discount,
            gst_free: item.gst_free,
        })
        .collect();

    let (subtotal_cents, gst_cents, total_cents) = totals(&lines, gst_registered);
    let seller = crate::finance::brand::resolve(instance, project.brand_id.as_deref());

    InvoiceSnapshot {
        schema_version: SCHEMA_VERSION,
        title,
        display_number: number.map(|n| instance.number_format.invoice(n)),
        issue_date: issue_date.map(str::to_string),
        seller: InvoiceSnapshotSeller {
            name: seller.name,
            legal_name: seller.legal_name,
            abn: instance.business_abn.clone(),
            address: seller.address,
            phone: seller.phone,
            email: seller.email,
        },
        bill_to: InvoiceSnapshotBillTo {
            name: client.fields.name.clone(),
            abn: client.fields.abn.clone(),
            address: client.fields.address.clone(),
        },
        reference: reference
            .map(str::to_string)
            .or_else(|| project.reference.clone()),
        currency: instance.currency_or_default().to_string(),
        gst_registered,
        lines,
        subtotal_cents,
        gst_cents,
        total_cents,
        notes: notes.map(str::to_string),
        payment_details: seller.payment_details,
        due_date: due_date.map(str::to_string),
        credit_note: None,
    }
}

/// The subtotal of the lines GST applies to — every line not `gst_free`.
fn taxable_cents(lines: &[InvoiceSnapshotLine]) -> i64 {
    lines
        .iter()
        .filter(|l| !l.gst_free)
        .map(|l| l.amount_cents)
        .sum()
}

/// Subtotal, GST and total for a set of lines: GST is 10% (round-half-up)
/// of the subtotal of the lines that *aren't* GST-free, and only when the
/// seller is GST-registered. A discount line's negative amount reduces
/// whichever of the two it belongs to.
pub fn totals(lines: &[InvoiceSnapshotLine], gst_registered: bool) -> (i64, i64, i64) {
    let subtotal_cents: i64 = lines.iter().map(|l| l.amount_cents).sum();
    let gst_cents = if gst_registered {
        money::gst_cents(taxable_cents(lines))
    } else {
        0
    };
    (subtotal_cents, gst_cents, subtotal_cents + gst_cents)
}

/// Refuse an invoice whose discounts outweigh what it charges: neither the
/// subtotal nor the GST-bearing part of it may go below zero. A draft's
/// preview may show such totals; finalizing one is refused.
pub fn check_discounts(snapshot: &InvoiceSnapshot) -> Result<(), String> {
    if snapshot.subtotal_cents < 0 {
        return Err("The discounts come to more than the charges".to_string());
    }
    if taxable_cents(&snapshot.lines) < 0 {
        return Err(
            "The discounts on GST lines come to more than those lines charge — \
             mark a discount GST-free if it applies to GST-free lines"
                .to_string(),
        );
    }
    Ok(())
}

/// The lines a full credit of `invoice` would carry: one per invoice line,
/// same description, amount and GST treatment (a discount line carries
/// across as a negative credit line).
pub fn full_credit_lines(invoice: &InvoiceSnapshot) -> Vec<CreditLine> {
    invoice
        .lines
        .iter()
        .map(|l| CreditLine {
            description: l.description.clone(),
            amount_cents: l.amount_cents,
            gst_free: l.gst_free,
        })
        .collect()
}

/// Build a credit note's frozen content from the snapshot of the invoice it
/// adjusts — the seller, bill-to, reference, currency and GST registration
/// all come from that invoice as it was finalized, never from today's
/// settings, so the two documents always agree. Titled "Adjustment Note"
/// when the invoice was a tax invoice (the ATO's name for it), else "Credit
/// Note"; numbered `display_number` (the instance's credit-note format); no
/// due date, notes or payment details.
pub fn build_credit_note_snapshot(
    invoice: &InvoiceSnapshot,
    display_number: &str,
    issue_date: &str,
    reason: &str,
    credit_lines: &[CreditLine],
) -> InvoiceSnapshot {
    let gst_registered = invoice.gst_registered;
    let lines: Vec<InvoiceSnapshotLine> = credit_lines
        .iter()
        .map(|l| InvoiceSnapshotLine {
            date: issue_date.to_string(),
            description: l.description.clone(),
            quantity: money::format_quantity(100),
            quantity_hundredths: 100,
            unit_price_cents: l.amount_cents,
            amount_cents: l.amount_cents,
            unit: None,
            discount: l.amount_cents < 0,
            gst_free: l.gst_free,
        })
        .collect();
    let (subtotal_cents, gst_cents, total_cents) = totals(&lines, gst_registered);
    InvoiceSnapshot {
        schema_version: SCHEMA_VERSION,
        title: if gst_registered {
            "Adjustment Note"
        } else {
            "Credit Note"
        }
        .to_string(),
        display_number: Some(display_number.to_string()),
        issue_date: Some(issue_date.to_string()),
        seller: invoice.seller.clone(),
        bill_to: invoice.bill_to.clone(),
        reference: invoice.reference.clone(),
        currency: invoice.currency.clone(),
        gst_registered,
        lines,
        subtotal_cents,
        gst_cents,
        total_cents,
        notes: None,
        payment_details: None,
        due_date: None,
        credit_note: Some(CreditNoteSnapshotInfo {
            invoice_display_number: invoice.display_number.clone().unwrap_or_default(),
            invoice_issue_date: invoice.issue_date.clone().unwrap_or_default(),
            reason: reason.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fictional_instance(gst_registered: bool) -> db::Instance {
        db::Instance {
            id: "inst1".into(),
            name: "Fictional Trades Pty Ltd".into(),
            slug: "fictional-trades".into(),
            kind: db::InstanceKind::Finance,
            public_submission_enabled: false,
            from_name: String::new(),
            signature: String::new(),
            created_at: 0,
            deleted: false,
            business_name: Some("Fictional Trades Pty Ltd".into()),
            business_abn: Some("11 222 333 444".into()),
            business_address: Some("1 Fictional St\nSomewhere NSW 2000".into()),
            business_phone: Some("0400 000 000".into()),
            business_email: Some("billing@fictional.example".into()),
            payment_details: Some("BSB 000-000 Acc 00000000".into()),
            gst_registered,
            currency: None,
            payment_terms_days: None,
            broadcast_senders: Vec::new(),
            brands: Vec::new(),
            number_format: db::NumberFormat::default(),
        }
    }

    fn fictional_client() -> db::Client {
        db::Client {
            id: "client1".into(),
            instance_id: "inst1".into(),
            fields: db::ClientFields {
                name: "Fictional Client Pty Ltd".into(),
                abn: Some("55 666 777 888".into()),
                address: Some("2 Client Ave\nElsewhere NSW 2000".into()),
                email: None,
                payment_terms_days: None,
            },
            archived: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn fictional_project() -> db::Project {
        db::Project {
            id: "proj1".into(),
            instance_id: "inst1".into(),
            client_id: "client1".into(),
            name: "Fictional Job".into(),
            reference: Some("42 Site Road".into()),
            default_unit_price_cents: None,
            brand_id: None,
            archived: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn item(
        id: &str,
        date: &str,
        created_at: u64,
        qty_hundredths: i64,
        price_cents: i64,
    ) -> db::BillableItem {
        db::BillableItem {
            id: id.into(),
            instance_id: "inst1".into(),
            project_id: "proj1".into(),
            date: date.into(),
            description: format!("Line {id}"),
            quantity_hundredths: qty_hundredths,
            unit_price_cents: price_cents,
            unit: None,
            discount: false,
            invoice_id: None,
            gst_free: false,
            source_expense_id: None,
            created_by_user_id: "user1".into(),
            created_at,
            updated_at: created_at,
        }
    }

    fn discount(id: &str, date: &str, created_at: u64, amount_cents: i64) -> db::BillableItem {
        db::BillableItem {
            discount: true,
            ..item(id, date, created_at, 100, amount_cents)
        }
    }

    /// [`build_snapshot`] for the fictional client, with no invoice-level
    /// reference or notes.
    fn build(
        instance: &db::Instance,
        project: &db::Project,
        items: &[db::BillableItem],
        number: Option<u32>,
        issue_date: Option<&str>,
        due_date: Option<&str>,
    ) -> InvoiceSnapshot {
        let client = fictional_client();
        build_snapshot(
            &SnapshotSource {
                instance,
                client: &client,
                project,
                reference: None,
                notes: None,
                items,
            },
            number,
            issue_date,
            due_date,
        )
    }

    #[test]
    fn title_reflects_gst_registration() {
        let instance = fictional_instance(false);
        let snap = build(&instance, &fictional_project(), &[], None, None, None);
        assert_eq!(snap.title, "Invoice");
        assert!(!snap.gst_registered);
        assert_eq!(snap.gst_cents, 0);

        let instance = fictional_instance(true);
        let snap = build(&instance, &fictional_project(), &[], None, None, None);
        assert_eq!(snap.title, "Tax Invoice");
        assert!(snap.gst_registered);
    }

    #[test]
    fn display_number_is_zero_padded_and_grows_past_999() {
        let instance = fictional_instance(false);
        let project = fictional_project();
        assert_eq!(
            build(&instance, &project, &[], Some(8), None, None).display_number,
            Some("008".to_string())
        );
        assert_eq!(
            build(&instance, &project, &[], Some(42), None, None).display_number,
            Some("042".to_string())
        );
        assert_eq!(
            build(&instance, &project, &[], Some(1234), None, None).display_number,
            Some("1234".to_string())
        );
        assert_eq!(
            build(&instance, &project, &[], None, None, None).display_number,
            None
        );
    }

    #[test]
    fn display_number_follows_the_instance_number_format() {
        let mut instance = fictional_instance(false);
        instance.number_format = db::NumberFormat {
            invoice_prefix: Some("INV-".into()),
            credit_note_prefix: None,
            padding: Some(5),
        };
        assert_eq!(
            build(&instance, &fictional_project(), &[], Some(42), None, None).display_number,
            Some("INV-00042".to_string())
        );
    }

    #[test]
    fn lines_are_sorted_by_date_then_created_at() {
        let instance = fictional_instance(false);
        let project = fictional_project();
        let items = vec![
            item("c", "2026-08-20", 300, 100, 1000),
            item("a", "2026-08-19", 100, 100, 1000),
            item("b", "2026-08-19", 200, 100, 1000),
        ];
        let snap = build(
            &instance,
            &project,
            &items,
            Some(1),
            Some("2026-08-20"),
            None,
        );
        let descriptions: Vec<&str> = snap.lines.iter().map(|l| l.description.as_str()).collect();
        assert_eq!(descriptions, vec!["Line a", "Line b", "Line c"]);
    }

    #[test]
    fn totals_without_gst() {
        let instance = fictional_instance(false);
        let project = fictional_project();
        let items = vec![
            item("a", "2026-08-19", 1, 200, 40_000), // 2 x 400.00 = 800.00
            item("b", "2026-08-19", 2, 150, 333),    // 1.5 x 3.33 = 4.995 -> 5.00
        ];
        let snap = build(
            &instance,
            &project,
            &items,
            Some(1),
            Some("2026-08-19"),
            Some("2026-09-02"),
        );
        assert_eq!(snap.subtotal_cents, 80_500);
        assert_eq!(snap.gst_cents, 0);
        assert_eq!(snap.total_cents, 80_500);
    }

    #[test]
    fn totals_with_gst_rounds_half_up() {
        let instance = fictional_instance(true);
        let project = fictional_project();
        let items = vec![item("a", "2026-08-19", 1, 100, 12_345)]; // subtotal 123.45
        let snap = build(
            &instance,
            &project,
            &items,
            Some(1),
            Some("2026-08-19"),
            Some("2026-09-02"),
        );
        assert_eq!(snap.subtotal_cents, 12_345);
        assert_eq!(snap.gst_cents, 1_235); // 12.345 -> 1,234.5c -> 1,235c half-up
        assert_eq!(snap.total_cents, 13_580);
    }

    #[test]
    fn bill_to_comes_from_the_client_and_seller_from_the_instance() {
        let instance = fictional_instance(true);
        let project = fictional_project();
        let snap = build(
            &instance,
            &project,
            &[],
            Some(1),
            Some("2026-08-19"),
            Some("2026-09-02"),
        );
        assert_eq!(snap.bill_to.name, "Fictional Client Pty Ltd");
        assert_eq!(snap.bill_to.abn.as_deref(), Some("55 666 777 888"));
        assert_eq!(snap.reference.as_deref(), Some("42 Site Road"));
        assert_eq!(
            snap.seller.name.as_deref(),
            Some("Fictional Trades Pty Ltd")
        );
        assert_eq!(snap.seller.phone.as_deref(), Some("0400 000 000"));
        assert_eq!(snap.currency, "AUD");
    }

    #[test]
    fn a_brand_changes_the_seller_but_not_the_abn() {
        let mut instance = fictional_instance(true);
        instance.brands = vec![db::Brand {
            id: "b1".into(),
            name: "Positive Rate".into(),
            email: Some("hello@positiverate.example".into()),
            phone: None,
            address: None,
            payment_details: Some("Pay Positive Rate".into()),
            archived: false,
        }];
        let mut project = fictional_project();
        project.brand_id = Some("b1".into());
        let snap = build(&instance, &project, &[], None, None, None);
        assert_eq!(snap.seller.name.as_deref(), Some("Positive Rate"));
        assert_eq!(
            snap.seller.legal_name.as_deref(),
            Some("Fictional Trades Pty Ltd")
        );
        assert_eq!(snap.seller.abn.as_deref(), Some("11 222 333 444"));
        assert_eq!(
            snap.seller.email.as_deref(),
            Some("hello@positiverate.example")
        );
        assert_eq!(snap.seller.phone.as_deref(), Some("0400 000 000"));
        assert_eq!(snap.payment_details.as_deref(), Some("Pay Positive Rate"));

        // The default brand prints exactly what it always did.
        let snap = build(&instance, &fictional_project(), &[], None, None, None);
        assert_eq!(
            snap.seller.name.as_deref(),
            Some("Fictional Trades Pty Ltd")
        );
        assert_eq!(snap.seller.legal_name, None);
        assert!(!serde_json::to_string(&snap).unwrap().contains("legal_name"));
    }

    #[test]
    fn optional_fields_are_left_out_of_the_json_and_default_back() {
        let snap = build(
            &fictional_instance(true),
            &fictional_project(),
            &[item("a", "2026-08-19", 1, 100, 1_000)],
            Some(1),
            Some("2026-08-19"),
            Some("2026-09-02"),
        );
        let json = serde_json::to_string(&snap).unwrap();
        for absent in ["\"unit\"", "\"discount\"", "\"notes\"", "legal_name"] {
            assert!(!json.contains(absent), "{absent} in {json}");
        }
        let parsed: InvoiceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, snap);
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let instance = fictional_instance(true);
        let project = fictional_project();
        let items = vec![item("a", "2026-08-19", 1, 100, 12_345)];
        let snap = build(
            &instance,
            &project,
            &items,
            Some(8),
            Some("2026-08-19"),
            Some("2026-09-02"),
        );
        let json = serde_json::to_string(&snap).expect("serialize");
        let round_tripped: InvoiceSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(snap, round_tripped);
        assert_eq!(round_tripped.schema_version, SCHEMA_VERSION);
        assert_eq!(round_tripped.due_date.as_deref(), Some("2026-09-02"));
    }

    #[test]
    fn gst_free_lines_are_left_out_of_the_gst() {
        let instance = fictional_instance(true);
        let project = fictional_project();
        let mut free = item("b", "2026-08-19", 2, 100, 5_000);
        free.gst_free = true;
        let items = vec![item("a", "2026-08-19", 1, 100, 10_000), free];
        let snap = build(
            &instance,
            &project,
            &items,
            Some(1),
            Some("2026-08-19"),
            None,
        );
        assert_eq!(snap.subtotal_cents, 15_000);
        assert_eq!(snap.gst_cents, 1_000);
        assert_eq!(snap.total_cents, 16_000);
        assert!(snap.lines[1].gst_free);
    }

    #[test]
    fn a_full_credit_mirrors_the_invoice() {
        let instance = fictional_instance(true);
        let project = fictional_project();
        let mut free = item("b", "2026-08-19", 2, 100, 5_000);
        free.gst_free = true;
        let items = vec![item("a", "2026-08-19", 1, 100, 10_000), free];
        let invoice = build(
            &instance,
            &project,
            &items,
            Some(7),
            Some("2026-08-19"),
            Some("2026-09-02"),
        );
        let note = build_credit_note_snapshot(
            &invoice,
            "CN-003",
            "2026-08-25",
            "Duplicate billing",
            &full_credit_lines(&invoice),
        );
        assert_eq!(note.title, "Adjustment Note");
        assert_eq!(note.display_number.as_deref(), Some("CN-003"));
        assert_eq!(note.total_cents, invoice.total_cents);
        assert_eq!(note.gst_cents, invoice.gst_cents);
        assert_eq!(note.bill_to, invoice.bill_to);
        assert_eq!(note.payment_details, None);
        assert_eq!(note.due_date, None);
        let info = note.credit_note.expect("credit note block");
        assert_eq!(info.invoice_display_number, "007");
        assert_eq!(info.invoice_issue_date, "2026-08-19");
    }

    #[test]
    fn a_credit_note_against_a_non_gst_invoice_is_a_plain_credit_note() {
        let invoice = build(
            &fictional_instance(false),
            &fictional_project(),
            &[item("a", "2026-08-19", 1, 100, 10_000)],
            Some(1),
            Some("2026-08-19"),
            Some("2026-09-02"),
        );
        let note = build_credit_note_snapshot(
            &invoice,
            "CN-001",
            "2026-08-20",
            "Discount",
            &[CreditLine {
                description: "Discount".into(),
                amount_cents: 2_500,
                gst_free: false,
            }],
        );
        assert_eq!(note.title, "Credit Note");
        assert_eq!(note.gst_cents, 0);
        assert_eq!(note.total_cents, 2_500);
    }

    #[test]
    fn a_discount_line_reduces_the_subtotal_and_the_gst() {
        let instance = fictional_instance(true);
        let items = vec![
            item("a", "2026-08-19", 1, 200, 10_000), // 2 x 100.00
            discount("b", "2026-08-19", 2, 2_500),   // -25.00
        ];
        let snap = build(
            &instance,
            &fictional_project(),
            &items,
            Some(1),
            Some("2026-08-19"),
            None,
        );
        assert_eq!(snap.subtotal_cents, 17_500);
        assert_eq!(snap.gst_cents, 1_750);
        assert_eq!(snap.total_cents, 19_250);
        let line = &snap.lines[1];
        assert!(line.discount);
        assert_eq!(line.amount_cents, -2_500);
        assert_eq!(line.unit_price_cents, -2_500);
        assert_eq!(check_discounts(&snap), Ok(()));
    }

    #[test]
    fn a_gst_free_discount_reduces_only_the_gst_free_part() {
        let instance = fictional_instance(true);
        let mut free = item("b", "2026-08-19", 2, 100, 5_000);
        free.gst_free = true;
        let mut free_discount = discount("c", "2026-08-19", 3, 1_000);
        free_discount.gst_free = true;
        let items = vec![item("a", "2026-08-19", 1, 100, 10_000), free, free_discount];
        let snap = build(&instance, &fictional_project(), &items, None, None, None);
        assert_eq!(snap.subtotal_cents, 14_000);
        assert_eq!(snap.gst_cents, 1_000);
    }

    #[test]
    fn discounts_can_not_outweigh_the_charges() {
        let instance = fictional_instance(true);
        let project = fictional_project();
        let snap = build(
            &instance,
            &project,
            &[
                item("a", "2026-08-19", 1, 100, 1_000),
                discount("b", "2026-08-19", 2, 1_500),
            ],
            None,
            None,
            None,
        );
        assert!(check_discounts(&snap).is_err());

        // A GST-bearing discount can't eat into the GST-free lines either.
        let mut free = item("a", "2026-08-19", 1, 100, 10_000);
        free.gst_free = true;
        let snap = build(
            &instance,
            &project,
            &[free, discount("b", "2026-08-19", 2, 1_000)],
            None,
            None,
            None,
        );
        assert_eq!(snap.subtotal_cents, 9_000);
        assert!(check_discounts(&snap).is_err());
    }

    #[test]
    fn a_full_credit_carries_a_discount_across() {
        let instance = fictional_instance(true);
        let invoice = build(
            &instance,
            &fictional_project(),
            &[
                item("a", "2026-08-19", 1, 100, 10_000),
                discount("b", "2026-08-19", 2, 2_000),
            ],
            Some(4),
            Some("2026-08-19"),
            Some("2026-09-02"),
        );
        let note = build_credit_note_snapshot(
            &invoice,
            "CN-001",
            "2026-08-20",
            "Cancelled",
            &full_credit_lines(&invoice),
        );
        assert_eq!(note.total_cents, invoice.total_cents);
        assert_eq!(note.gst_cents, invoice.gst_cents);
        assert!(note.lines[1].discount);
    }

    #[test]
    fn units_notes_and_an_invoice_reference_are_printed() {
        let instance = fictional_instance(false);
        let client = fictional_client();
        let project = fictional_project();
        let mut hours = item("a", "2026-08-19", 1, 250, 9_000);
        hours.unit = Some("hrs".into());
        let items = [hours];
        let snap = build_snapshot(
            &SnapshotSource {
                instance: &instance,
                client: &client,
                project: &project,
                reference: Some("PO 1234"),
                notes: Some("Thanks for your business"),
                items: &items,
            },
            None,
            None,
            None,
        );
        assert_eq!(snap.lines[0].unit.as_deref(), Some("hrs"));
        assert_eq!(snap.reference.as_deref(), Some("PO 1234"));
        assert_eq!(snap.notes.as_deref(), Some("Thanks for your business"));

        // Without its own, the invoice prints the project's reference.
        let snap = build(&instance, &project, &[], None, None, None);
        assert_eq!(snap.reference.as_deref(), Some("42 Site Road"));
        assert_eq!(snap.notes, None);
    }
}
