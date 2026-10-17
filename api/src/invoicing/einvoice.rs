//! Australian e-invoicing: a finalized invoice or credit note as a Peppol
//! PINT A-NZ e-invoice — UBL 2.1 XML, the format the ATO's Peppol e-invoicing
//! framework exchanges between businesses. See CLAUDE.md's "E-invoicing"
//! house rule.
//!
//! [`render_xml`] is pure, like `invoicing::pdf::render_invoice_pdf`: the
//! frozen [`InvoiceSnapshot`] is its only input, so the e-invoice can never
//! disagree with the PDF about what was invoiced. It is deterministic down to
//! the byte (there is no timestamp or random id in it).
//!
//! What Toolbox's data can and can't say, and how each gap is filled:
//!
//! - **Both parties are Australian** (country `AU`), identified by ABN under
//!   ICD `0151` — as their Peppol endpoint and as their legal registration
//!   (A-NZ rules `aligned-ibr-001-aunz`/`-004-aunz` require both ABNs). A
//!   missing or invalid ABN on either side is a [`problems`] entry, and
//!   rendering refuses.
//! - **Addresses are free text.** The first lines become the street lines; a
//!   last line shaped `Suburb NSW 2000` is split into city, state and
//!   postcode ([`split_address`]).
//! - **GST** maps to the A-NZ tax categories under tax scheme `GST`: a
//!   GST-registered seller's lines are `S` (10%) or, when GST-free, `Z`
//!   (0%); an unregistered seller's are all `O` ("not subject to tax", no
//!   rate), which A-NZ allows only on its own.
//! - **A buyer reference is mandatory** (`aligned-ibrp-003`): the project's
//!   reference when it has one, else the document's own number.
//! - **Payment details are free text**, so they travel as the payment terms
//!   note rather than as structured bank details.
//! - **A credit note** is a UBL `CreditNote` (type 381) carrying the reason
//!   as its note and the adjusted invoice as its billing reference; its
//!   amounts are positive, as UBL requires.
//!
//! Every rule above is checked against OpenPEPPOL's published PINT A-NZ
//! Schematron in development — see `tests::*` for the shapes it was
//! validated on.

use crate::invoicing::csv::plain_cents;
use crate::invoicing::money;
use crate::invoicing::snapshot::InvoiceSnapshot;

/// PINT A-NZ billing's specification identifier (ibt-024).
pub const CUSTOMIZATION_ID: &str = "urn:peppol:pint:billing-1@aunz-1";
/// Peppol's billing business process (ibt-023).
pub const PROFILE_ID: &str = "urn:peppol:bis:billing";
/// ISO 6523 ICD for an Australian Business Number.
pub const ABN_SCHEME: &str = "0151";

const STATES: [&str; 8] = ["NSW", "VIC", "QLD", "SA", "WA", "TAS", "NT", "ACT"];

/// An ABN as 11 bare digits, if `raw` is a valid one: spaces are ignored,
/// and the ATO's weighted checksum must hold (subtract 1 from the first
/// digit; Σ digit × weight must be divisible by 89).
pub fn normalize_abn(raw: &str) -> Option<String> {
    let digits: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if digits.len() != 11 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    const WEIGHTS: [u32; 11] = [10, 1, 3, 5, 7, 9, 11, 13, 15, 17, 19];
    let sum: u32 = digits
        .bytes()
        .enumerate()
        .map(|(i, b)| {
            let d = u32::from(b - b'0');
            let d = if i == 0 { d.wrapping_sub(1) } else { d };
            d.wrapping_mul(WEIGHTS[i])
        })
        .fold(0u32, u32::wrapping_add);
    // A leading 0 underflows the first term; no valid ABN starts with 0.
    if digits.starts_with('0') || !sum.is_multiple_of(89) {
        return None;
    }
    Some(digits)
}

/// Why `snapshot` can't be issued as an e-invoice yet — empty when it can.
/// Works on a draft's live preview too (the missing number and issue date
/// aren't counted), so the problems can be fixed before finalizing, while
/// the project and invoicing settings still feed the invoice.
pub fn problems(snapshot: &InvoiceSnapshot) -> Vec<String> {
    let mut out = Vec::new();
    if snapshot
        .seller
        .name
        .as_deref()
        .is_none_or(|n| n.trim().is_empty())
    {
        out.push("The invoicing settings have no business name".to_string());
    }
    match snapshot.seller.abn.as_deref() {
        None => out.push("The invoicing settings have no ABN".to_string()),
        Some(abn) if normalize_abn(abn).is_none() => {
            out.push(format!("The business ABN {abn:?} isn't a valid ABN"))
        }
        Some(_) => {}
    }
    match snapshot.bill_to.abn.as_deref() {
        None => out.push("The client has no ABN (set it on the project)".to_string()),
        Some(abn) if normalize_abn(abn).is_none() => {
            out.push(format!("The client ABN {abn:?} isn't a valid ABN"))
        }
        Some(_) => {}
    }
    if snapshot.bill_to.name.trim().is_empty() {
        out.push("The client has no name".to_string());
    }
    if snapshot.lines.is_empty() {
        out.push("It has no lines".to_string());
    }
    if !is_currency_code(&snapshot.currency) {
        out.push(format!("{:?} isn't a currency code", snapshot.currency));
    }
    out
}

fn is_currency_code(s: &str) -> bool {
    s.len() == 3 && s.bytes().all(|b| b.is_ascii_uppercase())
}

/// The file name a download or attachment uses: `Invoice-008.xml`,
/// `Credit-Note-CN-001.xml` — the PDF's name with the XML extension.
pub fn filename(snapshot: &InvoiceSnapshot) -> String {
    let number = snapshot.display_number.as_deref().unwrap_or("unknown");
    match snapshot.credit_note {
        Some(_) => format!("Credit-Note-{number}.xml"),
        None => format!("Invoice-{number}.xml"),
    }
}

/// Render a finalized invoice's or credit note's snapshot as a PINT A-NZ
/// UBL document. `Err` carries every [`problems`] entry (joined), or says
/// the snapshot isn't finalized.
pub fn render_xml(snapshot: &InvoiceSnapshot) -> Result<String, String> {
    let problems = problems(snapshot);
    if !problems.is_empty() {
        return Err(format!(
            "This can't be issued as an e-invoice: {}.",
            problems.join("; ")
        ));
    }
    let (Some(number), Some(issue_date)) = (
        snapshot.display_number.as_deref(),
        snapshot.issue_date.as_deref(),
    ) else {
        return Err("Only a finalized invoice has an e-invoice".to_string());
    };
    // `problems` has vouched for all three.
    let seller_name = snapshot.seller.name.as_deref().unwrap_or_default();
    let seller_abn = snapshot
        .seller
        .abn
        .as_deref()
        .and_then(normalize_abn)
        .unwrap_or_default();
    let buyer_abn = snapshot
        .bill_to
        .abn
        .as_deref()
        .and_then(normalize_abn)
        .unwrap_or_default();

    let is_credit = snapshot.credit_note.is_some();
    let root = if is_credit { "CreditNote" } else { "Invoice" };
    let currency = snapshot.currency.as_str();
    let amount = |cents: i64| format!(r#"currencyID="{currency}">{}"#, plain_cents(cents));

    let mut x = Xml::default();
    x.raw(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    x.raw(&format!(
        r#"<{root} xmlns="urn:oasis:names:specification:ubl:schema:xsd:{root}-2" xmlns:cac="urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2" xmlns:cbc="urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2">"#
    ));
    x.depth += 1;
    x.leaf("cbc:CustomizationID", CUSTOMIZATION_ID);
    x.leaf("cbc:ProfileID", PROFILE_ID);
    x.leaf("cbc:ID", number);
    x.leaf("cbc:IssueDate", issue_date);
    if is_credit {
        x.leaf("cbc:CreditNoteTypeCode", "381");
    } else {
        if let Some(due) = snapshot.due_date.as_deref() {
            x.leaf("cbc:DueDate", due);
        }
        x.leaf("cbc:InvoiceTypeCode", "380");
    }
    if let Some(info) = &snapshot.credit_note {
        x.leaf("cbc:Note", &info.reason);
    }
    x.leaf("cbc:DocumentCurrencyCode", currency);
    x.leaf(
        "cbc:BuyerReference",
        snapshot
            .reference
            .as_deref()
            .filter(|r| !r.trim().is_empty())
            .unwrap_or(number),
    );
    if let Some(info) = &snapshot.credit_note {
        x.open("cac:BillingReference");
        x.open("cac:InvoiceDocumentReference");
        x.leaf("cbc:ID", &info.invoice_display_number);
        if !info.invoice_issue_date.is_empty() {
            x.leaf("cbc:IssueDate", &info.invoice_issue_date);
        }
        x.close("cac:InvoiceDocumentReference");
        x.close("cac:BillingReference");
    }

    x.open("cac:AccountingSupplierParty");
    party(
        &mut x,
        &PartyFields {
            name: seller_name,
            abn: &seller_abn,
            address: snapshot.seller.address.as_deref(),
            gst_registered: snapshot.gst_registered,
            phone: snapshot.seller.phone.as_deref(),
            email: snapshot.seller.email.as_deref(),
        },
    );
    x.close("cac:AccountingSupplierParty");

    x.open("cac:AccountingCustomerParty");
    party(
        &mut x,
        &PartyFields {
            name: &snapshot.bill_to.name,
            abn: &buyer_abn,
            address: snapshot.bill_to.address.as_deref(),
            gst_registered: false,
            phone: None,
            email: None,
        },
    );
    x.close("cac:AccountingCustomerParty");

    if let Some(details) = snapshot
        .payment_details
        .as_deref()
        .filter(|d| !d.trim().is_empty())
    {
        x.open("cac:PaymentTerms");
        x.leaf("cbc:Note", details.trim());
        x.close("cac:PaymentTerms");
    }

    // One breakdown per tax category, in a fixed order.
    let categories: Vec<Category> = [Category::Standard, Category::GstFree, Category::NoGst]
        .into_iter()
        .filter(|c| {
            snapshot
                .lines
                .iter()
                .any(|l| Category::of(snapshot.gst_registered, l.gst_free) == *c)
        })
        .collect();
    x.open("cac:TaxTotal");
    x.raw_leaf("cbc:TaxAmount", &amount(snapshot.gst_cents));
    for category in &categories {
        let taxable: i64 = snapshot
            .lines
            .iter()
            .filter(|l| Category::of(snapshot.gst_registered, l.gst_free) == *category)
            .map(|l| l.amount_cents)
            .sum();
        let tax = match category {
            // `snapshot::totals` charges GST on exactly these lines.
            Category::Standard => snapshot.gst_cents,
            _ => 0,
        };
        x.open("cac:TaxSubtotal");
        x.raw_leaf("cbc:TaxableAmount", &amount(taxable));
        x.raw_leaf("cbc:TaxAmount", &amount(tax));
        category.write(&mut x, "cac:TaxCategory", true);
        x.close("cac:TaxSubtotal");
    }
    x.close("cac:TaxTotal");

    x.open("cac:LegalMonetaryTotal");
    x.raw_leaf("cbc:LineExtensionAmount", &amount(snapshot.subtotal_cents));
    x.raw_leaf("cbc:TaxExclusiveAmount", &amount(snapshot.subtotal_cents));
    x.raw_leaf("cbc:TaxInclusiveAmount", &amount(snapshot.total_cents));
    x.raw_leaf("cbc:PayableAmount", &amount(snapshot.total_cents));
    x.close("cac:LegalMonetaryTotal");

    let (line_tag, quantity_tag) = if is_credit {
        ("cac:CreditNoteLine", "cbc:CreditedQuantity")
    } else {
        ("cac:InvoiceLine", "cbc:InvoicedQuantity")
    };
    for (i, line) in snapshot.lines.iter().enumerate() {
        x.open(line_tag);
        x.leaf("cbc:ID", &(i + 1).to_string());
        // UN/ECE Rec 20 "C62": a unit count ("one").
        x.raw_leaf(
            quantity_tag,
            &format!(
                r#"unitCode="C62">{}"#,
                money::format_quantity(line.quantity_hundredths)
            ),
        );
        x.raw_leaf("cbc:LineExtensionAmount", &amount(line.amount_cents));
        // A credit note's line dates are just its issue date; an invoice's
        // are when the work was done.
        if !is_credit {
            x.open("cac:InvoicePeriod");
            x.leaf("cbc:StartDate", &line.date);
            x.leaf("cbc:EndDate", &line.date);
            x.close("cac:InvoicePeriod");
        }
        let (name, description) = item_name(&line.description);
        x.open("cac:Item");
        if let Some(description) = description {
            x.leaf("cbc:Description", description);
        }
        x.leaf("cbc:Name", &name);
        Category::of(snapshot.gst_registered, line.gst_free).write(
            &mut x,
            "cac:ClassifiedTaxCategory",
            false,
        );
        x.close("cac:Item");
        x.open("cac:Price");
        x.raw_leaf("cbc:PriceAmount", &amount(line.unit_price_cents));
        x.close("cac:Price");
        x.close(line_tag);
    }

    x.depth -= 1;
    x.raw(&format!("</{root}>"));
    Ok(x.out)
}

/// The A-NZ tax category of a line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Category {
    /// `S`: GST at 10%.
    Standard,
    /// `Z`: a GST-free supply by a registered seller.
    GstFree,
    /// `O`: the seller isn't registered for GST.
    NoGst,
}

impl Category {
    fn of(gst_registered: bool, gst_free: bool) -> Self {
        match (gst_registered, gst_free) {
            (false, _) => Self::NoGst,
            (true, true) => Self::GstFree,
            (true, false) => Self::Standard,
        }
    }

    /// `<tag>` with this category's code, rate (none for `O`, per
    /// `aligned-ibrp-o-05-aunz`) and, in the document's breakdown
    /// (`breakdown`), the reason an `O` category carries no GST.
    fn write(self, x: &mut Xml, tag: &str, breakdown: bool) {
        x.open(tag);
        match self {
            Self::Standard => {
                x.leaf("cbc:ID", "S");
                x.leaf("cbc:Percent", "10");
            }
            Self::GstFree => {
                x.leaf("cbc:ID", "Z");
                x.leaf("cbc:Percent", "0");
            }
            Self::NoGst => {
                x.leaf("cbc:ID", "O");
                if breakdown {
                    x.leaf("cbc:TaxExemptionReason", "Not registered for GST");
                }
            }
        }
        x.open("cac:TaxScheme");
        x.leaf("cbc:ID", "GST");
        x.close("cac:TaxScheme");
        x.close(tag);
    }
}

struct PartyFields<'a> {
    name: &'a str,
    /// 11 bare digits, already validated.
    abn: &'a str,
    address: Option<&'a str>,
    /// Write the GST registration (ibt-031) — the seller's, when registered.
    gst_registered: bool,
    phone: Option<&'a str>,
    email: Option<&'a str>,
}

/// `cac:Party`, in UBL's element order: endpoint, postal address, GST
/// registration, legal entity, contact.
fn party(x: &mut Xml, p: &PartyFields<'_>) {
    x.open("cac:Party");
    x.raw_leaf(
        "cbc:EndpointID",
        &format!(r#"schemeID="{ABN_SCHEME}">{}"#, p.abn),
    );
    x.open("cac:PostalAddress");
    let address = split_address(p.address.unwrap_or_default());
    if let Some(street) = address.street.first() {
        x.leaf("cbc:StreetName", street);
    }
    if let Some(street) = address.street.get(1) {
        x.leaf("cbc:AdditionalStreetName", street);
    }
    if let Some(city) = address.city {
        x.leaf("cbc:CityName", city);
    }
    if let Some(postcode) = address.postcode {
        x.leaf("cbc:PostalZone", postcode);
    }
    if let Some(state) = address.state {
        x.leaf("cbc:CountrySubentity", state);
    }
    if address.street.len() > 2 {
        x.open("cac:AddressLine");
        x.leaf("cbc:Line", &address.street[2..].join(", "));
        x.close("cac:AddressLine");
    }
    x.open("cac:Country");
    x.leaf("cbc:IdentificationCode", "AU");
    x.close("cac:Country");
    x.close("cac:PostalAddress");
    if p.gst_registered {
        x.open("cac:PartyTaxScheme");
        x.leaf("cbc:CompanyID", p.abn);
        x.open("cac:TaxScheme");
        x.leaf("cbc:ID", "GST");
        x.close("cac:TaxScheme");
        x.close("cac:PartyTaxScheme");
    }
    x.open("cac:PartyLegalEntity");
    x.leaf("cbc:RegistrationName", p.name.trim());
    x.raw_leaf(
        "cbc:CompanyID",
        &format!(r#"schemeID="{ABN_SCHEME}">{}"#, p.abn),
    );
    x.close("cac:PartyLegalEntity");
    let phone = p.phone.map(str::trim).filter(|s| !s.is_empty());
    let email = p.email.map(str::trim).filter(|s| !s.is_empty());
    if phone.is_some() || email.is_some() {
        x.open("cac:Contact");
        if let Some(phone) = phone {
            x.leaf("cbc:Telephone", phone);
        }
        if let Some(email) = email {
            x.leaf("cbc:ElectronicMail", email);
        }
        x.close("cac:Contact");
    }
    x.close("cac:Party");
}

/// A free-text address, split for UBL.
#[derive(Debug, Default, PartialEq)]
struct Address<'a> {
    street: Vec<&'a str>,
    city: Option<&'a str>,
    state: Option<&'a str>,
    postcode: Option<&'a str>,
}

/// Non-blank lines become street lines, except a last line shaped like an
/// Australian locality — `Suburb STATE 2000` — which becomes the city,
/// state and postcode. Anything else stays as written.
fn split_address(raw: &str) -> Address<'_> {
    let mut street: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut address = Address::default();
    if let Some(last) = street.last() {
        let tokens: Vec<&str> = last.split_whitespace().collect();
        if let [.., state, postcode] = tokens.as_slice()
            && tokens.len() >= 3
            && STATES.contains(&state.to_ascii_uppercase().as_str())
            && postcode.len() == 4
            && postcode.bytes().all(|b| b.is_ascii_digit())
        {
            let state_at = last.rfind(state).unwrap_or(0);
            let city = last[..state_at].trim().trim_end_matches(',').trim();
            // An empty element is invalid (ibr-079).
            address.city = (!city.is_empty()).then_some(city);
            address.state = Some(state);
            address.postcode = Some(postcode);
            street.pop();
        }
    }
    address.street = street;
    address
}

/// A line's item name (ibt-153) and, when there's more to it, its full
/// description (ibt-154): the name is the first line of the description,
/// without a leading bullet.
fn item_name(description: &str) -> (String, Option<&str>) {
    let first = description.lines().next().unwrap_or_default().trim();
    let first = first
        .strip_prefix("* ")
        .or_else(|| first.strip_prefix("- "))
        .unwrap_or(first)
        .trim();
    let name = if first.is_empty() { "Item" } else { first };
    let rest = (description.trim() != name).then_some(description.trim());
    (name.to_string(), rest)
}

/// A tiny pretty-printing XML writer — enough for UBL, with every text
/// value escaped.
#[derive(Default)]
struct Xml {
    out: String,
    depth: usize,
}

impl Xml {
    fn raw(&mut self, line: &str) {
        for _ in 0..self.depth {
            self.out.push_str("  ");
        }
        self.out.push_str(line);
        self.out.push('\n');
    }

    fn open(&mut self, tag: &str) {
        self.raw(&format!("<{tag}>"));
        self.depth += 1;
    }

    fn close(&mut self, tag: &str) {
        self.depth -= 1;
        self.raw(&format!("</{tag}>"));
    }

    fn leaf(&mut self, tag: &str, text: &str) {
        self.raw(&format!("<{tag}>{}</{tag}>", escape(text)));
    }

    /// `<tag attrs>value</tag>`, where `attrs_and_value` is already escaped
    /// (`currencyID="AUD">12.34`).
    fn raw_leaf(&mut self, tag: &str, attrs_and_value: &str) {
        self.raw(&format!("<{tag} {attrs_and_value}</{tag}>"));
    }
}

/// Escape text for an XML element, dropping the control characters XML 1.0
/// can't carry at all.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            '\n' | '\t' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invoicing::snapshot::{
        CreditLine, InvoiceSnapshotBillTo, InvoiceSnapshotLine, InvoiceSnapshotSeller,
        build_credit_note_snapshot, totals,
    };

    // Valid ABNs (the checksum holds); not real businesses' details.
    const SELLER_ABN: &str = "51 824 753 556";
    const BUYER_ABN: &str = "53 004 085 616";

    fn line(
        description: &str,
        qty_hundredths: i64,
        price: i64,
        gst_free: bool,
    ) -> InvoiceSnapshotLine {
        InvoiceSnapshotLine {
            date: "2026-08-19".into(),
            description: description.into(),
            quantity: money::format_quantity(qty_hundredths),
            quantity_hundredths: qty_hundredths,
            unit_price_cents: price,
            amount_cents: money::line_amount_cents(qty_hundredths, price),
            gst_free,
        }
    }

    pub(super) fn invoice(gst_registered: bool) -> InvoiceSnapshot {
        let lines = vec![
            line("Labour\n* site visit\n* report", 150, 333, false),
            line("Permit fee", 100, 5_000, true),
        ];
        let (subtotal_cents, gst_cents, total_cents) = totals(&lines, gst_registered);
        InvoiceSnapshot {
            schema_version: 2,
            title: if gst_registered {
                "Tax Invoice"
            } else {
                "Invoice"
            }
            .into(),
            display_number: Some("008".into()),
            issue_date: Some("2026-08-19".into()),
            seller: InvoiceSnapshotSeller {
                name: Some("Fictional Trades & Sons Pty Ltd".into()),
                abn: Some(SELLER_ABN.into()),
                address: Some("Unit 2\n1 Fictional St\nSomewhere NSW 2000".into()),
                phone: Some("0400 000 000".into()),
                email: Some("billing@fictional.example".into()),
            },
            bill_to: InvoiceSnapshotBillTo {
                name: "Fictional Client Pty Ltd".into(),
                abn: Some(BUYER_ABN.into()),
                address: Some("2 Client Ave\nElsewhere, VIC 3000".into()),
            },
            reference: Some("PO <42>".into()),
            currency: "AUD".into(),
            gst_registered,
            lines,
            subtotal_cents,
            gst_cents,
            total_cents,
            payment_details: Some("BSB 000-000 Acc 00000000".into()),
            due_date: Some("2026-09-02".into()),
            credit_note: None,
            no_gst_note: !gst_registered,
        }
    }

    pub(super) fn credit_note() -> InvoiceSnapshot {
        build_credit_note_snapshot(
            &invoice(true),
            3,
            "2026-08-25",
            "Overcharged",
            &[CreditLine {
                description: "Labour adjustment".into(),
                amount_cents: 1_000,
                gst_free: false,
            }],
        )
    }

    #[test]
    fn abns_are_checksummed() {
        assert_eq!(normalize_abn(SELLER_ABN).as_deref(), Some("51824753556"));
        assert_eq!(normalize_abn("53004085616").as_deref(), Some("53004085616"));
        assert_eq!(normalize_abn("51 824 753 557"), None);
        assert_eq!(normalize_abn("11 222 333 444"), None);
        assert_eq!(normalize_abn("5182475355"), None);
        assert_eq!(normalize_abn("5182475355a"), None);
        assert_eq!(normalize_abn("01824753556"), None);
    }

    #[test]
    fn a_complete_invoice_has_no_problems() {
        assert!(problems(&invoice(true)).is_empty());
        assert!(problems(&credit_note()).is_empty());
    }

    #[test]
    fn missing_or_bad_abns_are_problems_and_refuse_rendering() {
        let mut snap = invoice(true);
        snap.seller.abn = Some("11 222 333 444".into());
        snap.bill_to.abn = None;
        let found = problems(&snap);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].contains("business ABN"));
        assert!(found[1].contains("client has no ABN"));
        let err = render_xml(&snap).unwrap_err();
        assert!(err.contains("business ABN") && err.contains("client has no ABN"));
    }

    #[test]
    fn a_draft_preview_is_checked_but_not_rendered() {
        let mut snap = invoice(true);
        snap.display_number = None;
        snap.issue_date = None;
        assert!(problems(&snap).is_empty());
        assert!(render_xml(&snap).is_err());
    }

    #[test]
    fn a_tax_invoice_carries_the_pint_anz_header_and_both_abns() {
        let xml = render_xml(&invoice(true)).unwrap();
        assert!(xml.starts_with(r#"<?xml version="1.0" encoding="UTF-8"?>"#));
        assert!(xml.contains(
            "<cbc:CustomizationID>urn:peppol:pint:billing-1@aunz-1</cbc:CustomizationID>"
        ));
        assert!(xml.contains("<cbc:ProfileID>urn:peppol:bis:billing</cbc:ProfileID>"));
        assert!(xml.contains("<cbc:InvoiceTypeCode>380</cbc:InvoiceTypeCode>"));
        assert!(xml.contains("<cbc:DueDate>2026-09-02</cbc:DueDate>"));
        assert!(xml.contains(r#"<cbc:EndpointID schemeID="0151">51824753556</cbc:EndpointID>"#));
        assert!(xml.contains(r#"<cbc:EndpointID schemeID="0151">53004085616</cbc:EndpointID>"#));
        // Escaped text.
        assert!(xml.contains("Fictional Trades &amp; Sons Pty Ltd"));
        assert!(xml.contains("<cbc:BuyerReference>PO &lt;42&gt;</cbc:BuyerReference>"));
    }

    #[test]
    fn gst_categories_and_totals_add_up() {
        let snap = invoice(true);
        let xml = render_xml(&snap).unwrap();
        // 1.5 × 3.33 = 4.995 → 5.00 at 10% (0.50); 50.00 GST-free.
        assert_eq!(snap.gst_cents, 50);
        assert!(xml.contains(r#"<cbc:TaxableAmount currencyID="AUD">5.00</cbc:TaxableAmount>"#));
        assert!(xml.contains(r#"<cbc:TaxableAmount currencyID="AUD">50.00</cbc:TaxableAmount>"#));
        assert!(xml.contains(r#"<cbc:TaxAmount currencyID="AUD">0.50</cbc:TaxAmount>"#));
        assert!(xml.contains("<cbc:ID>S</cbc:ID>") && xml.contains("<cbc:ID>Z</cbc:ID>"));
        assert!(xml.contains(r#"<cbc:PayableAmount currencyID="AUD">55.50</cbc:PayableAmount>"#));
        assert!(xml.contains(r#"<cbc:InvoicedQuantity unitCode="C62">1.5</cbc:InvoicedQuantity>"#));
        // The seller's GST registration.
        assert!(xml.contains("<cac:PartyTaxScheme>"));
    }

    #[test]
    fn an_unregistered_seller_is_not_subject_to_gst() {
        let xml = render_xml(&invoice(false)).unwrap();
        assert!(xml.contains("<cbc:ID>O</cbc:ID>"));
        assert!(!xml.contains("<cbc:ID>S</cbc:ID>") && !xml.contains("<cbc:ID>Z</cbc:ID>"));
        assert!(!xml.contains("<cbc:Percent>"));
        assert!(!xml.contains("<cac:PartyTaxScheme>"));
        assert_eq!(xml.matches("<cac:TaxSubtotal>").count(), 1);
    }

    #[test]
    fn a_credit_note_references_its_invoice() {
        let xml = render_xml(&credit_note()).unwrap();
        assert!(xml.contains(
            "<CreditNote xmlns=\"urn:oasis:names:specification:ubl:schema:xsd:CreditNote-2\""
        ));
        assert!(xml.contains("<cbc:ID>CN-003</cbc:ID>"));
        assert!(xml.contains("<cbc:CreditNoteTypeCode>381</cbc:CreditNoteTypeCode>"));
        assert!(xml.contains("<cbc:Note>Overcharged</cbc:Note>"));
        let reference = xml.split("<cac:InvoiceDocumentReference>").nth(1).unwrap();
        assert!(reference.trim_start().starts_with("<cbc:ID>008</cbc:ID>"));
        assert!(xml.contains("<cac:CreditNoteLine>"));
        assert!(!xml.contains("<cbc:DueDate>"));
        assert_eq!(filename(&credit_note()), "Credit-Note-CN-003.xml");
        assert_eq!(filename(&invoice(true)), "Invoice-008.xml");
    }

    #[test]
    fn rendering_is_deterministic() {
        assert_eq!(render_xml(&invoice(true)), render_xml(&invoice(true)));
    }

    #[test]
    fn addresses_split_off_an_australian_locality() {
        let a = split_address("Unit 2\n1 Fictional St\nSurry Hills NSW 2010");
        assert_eq!(a.street, vec!["Unit 2", "1 Fictional St"]);
        assert_eq!(a.city, Some("Surry Hills"));
        assert_eq!(a.state, Some("NSW"));
        assert_eq!(a.postcode, Some("2010"));
        let b = split_address("PO Box 1\nWellington 6011\nNew Zealand");
        assert_eq!(b.street.len(), 3);
        assert_eq!(b.city, None);
        assert_eq!(split_address(""), Address::default());
        let c = split_address("1 Fictional St\n, NSW 2000");
        assert_eq!((c.city, c.state), (None, Some("NSW")));
    }

    #[test]
    fn item_names_are_the_first_line() {
        assert_eq!(item_name("Permit fee"), ("Permit fee".to_string(), None));
        assert_eq!(
            item_name("Labour\n* site visit"),
            ("Labour".to_string(), Some("Labour\n* site visit"))
        );
        assert_eq!(item_name("* bullet").0, "bullet");
    }

    #[test]
    fn control_characters_are_dropped() {
        assert_eq!(escape("a\u{1}b\r\n<"), "ab&#13;\n&lt;");
    }
}
