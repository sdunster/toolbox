//! Server-side PDF rendering for a finalized invoice — see CLAUDE.md's
//! "Invoicing" house rule ("PDF"). [`render_invoice_pdf`] is pure (no I/O,
//! no network): it takes an [`InvoiceSnapshot`] — the *only* input, per that
//! house rule — and returns the PDF's bytes.
//!
//! **Content is deterministic; the file's bytes are not, quite.** Calling
//! it twice with the same snapshot draws exactly the same title, header
//! text, table rows, totals and payment details, at exactly the same
//! positions, on exactly the same number of pages — and this module fixes
//! the PDF's `CreationDate`/`ModDate` to the invoice's issue date (midnight
//! UTC) rather than leaving them at printpdf's default of "now", which
//! would otherwise make every render differ. But printpdf 0.7 has no public
//! way to make the file's trailer `/ID` deterministic: `PdfDocument::new`
//! picks a random 32-character `document_id`, and `save`/`save_to_bytes`
//! generates a second random 32-character `instance_id`, unconditionally,
//! every time — see `pdf_document.rs` in the printpdf source; there is no
//! setter for either. `with_document_id` exists but sets the *XMP*
//! metadata's document id, not the trailer's, and this document's
//! conformance profile (the crate default) doesn't even emit an XMP stream,
//! so that call would change nothing. So two renders of the same snapshot
//! are byte-identical everywhere *except* those two trailer strings — this
//! is exercised by
//! `tests::rendering_the_same_snapshot_twice_is_content_identical`, which
//! asserts the two outputs are the same length and differ only in a small
//! window at the end of the file.
//!
//! That residual nondeterminism is harmless for what this module is used
//! for (`graphql::mutations::download_invoice_pdf`'s first-render-wins
//! cache): two callers racing the first render each produce a complete,
//! correct PDF of the same invoice: whichever one's `put_bytes` lands last
//! simply overwrites the other with an equally valid rendering — last
//! writer wins, not a race that corrupts anything.
//!
//! Layout mirrors the product's original sample invoice (see the design
//! doc's "PDF (PR 4)" section for the exact numbers): a title, a two-column
//! header (invoice details/bill-to on the left, seller details on the
//! right), a line-item table with a grey header row and shaded body rows,
//! totals, and a payment-details footer. Every field that carries
//! user-entered text (bill-to name/ABN/address, reference, seller
//! name/address/phone/email/ABN, item descriptions, payment details) is
//! word-wrapped to its column's available width — breaking a single word
//! longer than that width by characters rather than overflowing — and
//! every line anywhere in the document (header columns, table rows with
//! the header row repeated on continuation pages, totals, the GST note,
//! payment details) is drawn through a bottom-margin-aware cursor that
//! starts a fresh page rather than ever drawing below [`BOTTOM_MARGIN`] or
//! dropping a line.
//!
//! Fonts are Liberation Sans Regular/Bold (SIL OFL 1.1 — see
//! `api/assets/fonts/OFL.txt`), embedded via `include_bytes!` so the
//! renderer has no filesystem dependency and builds standalone for
//! `cargo lambda`. Glyph widths for word-wrapping and right-alignment are
//! measured with `owned_ttf_parser` — the same crate `printpdf` itself uses
//! internally to embed the font, reused here rather than duplicated with a
//! second TTF-parsing crate.

use std::io::{BufWriter, Cursor};

use anyhow::{Result, anyhow};
use owned_ttf_parser::{AsFaceRef as _, Face, OwnedFace};
use printpdf::path::PaintMode;
use printpdf::{
    Color, IndirectFontRef, Mm, OffsetDateTime, PdfDocument, PdfDocumentReference,
    PdfLayerReference, Pt, Rect, Rgb,
};

use crate::invoicing::money;
use crate::invoicing::snapshot::InvoiceSnapshot;

const REGULAR_TTF: &[u8] = include_bytes!("../../assets/fonts/LiberationSans-Regular.ttf");
const BOLD_TTF: &[u8] = include_bytes!("../../assets/fonts/LiberationSans-Bold.ttf");

// ── Page geometry (pt; A4 portrait) ─────────────────────────────────────────

const PAGE_W_PT: f32 = 595.28;
const PAGE_H_PT: f32 = 841.89;
/// Left content edge, and the left edge of the line table's Description
/// column.
const MARGIN_LEFT: f32 = 57.0;
/// Right content edge, and the right edge of the line table's last column.
const MARGIN_RIGHT: f32 = 538.0;
/// Nothing is drawn below this — a row/block that would cross it starts a
/// new page instead.
const BOTTOM_MARGIN: f32 = 50.0;
/// Where the table header row (and, for a draft-preview-shaped continuation,
/// any content) resumes at the top of a second-or-later page.
const CONTINUATION_TOP: f32 = 780.0;

const TITLE_Y: f32 = 755.0;
const TITLE_SIZE: f32 = 30.0;
/// Where the two-column header block (invoice date/number/bill-to on the
/// left, seller details on the right) starts, below the title.
const COLUMNS_TOP: f32 = 705.0;
const COLUMN_FONT_SIZE: f32 = 14.0;
const COLUMN_LINE_HEIGHT: f32 = 16.0;
/// Gap between the header block and the line table.
const TABLE_TOP_GAP: f32 = 28.0;

/// Horizontal midpoint of the page, and the gap either side of it that
/// separates the header block's two columns — a wrapped left-column line
/// must not cross into the right column's territory, and vice versa.
const PAGE_MID: f32 = PAGE_W_PT / 2.0;
const COLUMN_GUTTER: f32 = 12.0;
/// Available width for a wrapped left-column line: [`MARGIN_LEFT`] to
/// `PAGE_MID - COLUMN_GUTTER`.
const LEFT_COLUMN_MAX_WIDTH: f32 = PAGE_MID - COLUMN_GUTTER - MARGIN_LEFT;
/// Available width for a wrapped right-column line: `PAGE_MID +
/// COLUMN_GUTTER` to [`MARGIN_RIGHT`].
const RIGHT_COLUMN_MAX_WIDTH: f32 = MARGIN_RIGHT - (PAGE_MID + COLUMN_GUTTER);
/// Available width for a wrapped payment-details line: the full content
/// width, [`MARGIN_LEFT`] to [`MARGIN_RIGHT`].
const PAYMENT_CONTENT_WIDTH: f32 = MARGIN_RIGHT - MARGIN_LEFT;

// Table column boundaries (absolute x, pt).
const COL_DESC_X0: f32 = MARGIN_LEFT;
const COL_DESC_X1: f32 = 342.0;
const COL_QTY_X1: f32 = 408.0;
const COL_PRICE_X1: f32 = 473.0;
const COL_TOTAL_X1: f32 = MARGIN_RIGHT;
/// Inset from a column's edges for its text.
const CELL_PAD: f32 = 6.0;

const HEADER_ROW_HEIGHT: f32 = 22.0;
const HEADER_FONT_SIZE: f32 = 12.0;
const BODY_FONT_SIZE: f32 = 10.0;
const BODY_LINE_HEIGHT: f32 = 13.0;
/// Vertical padding above/below a body row's wrapped description lines.
const ROW_V_PAD: f32 = 8.0;
/// Hanging indent for a bulleted description line (and its wrapped
/// continuations), and the gap after the bullet glyph.
const BULLET_INDENT: f32 = 12.0;

const TOTALS_FONT_SIZE: f32 = 12.0;
const TOTALS_LINE_HEIGHT: f32 = 16.0;
const NOTE_FONT_SIZE: f32 = 14.0;
const PAYMENT_FONT_SIZE: f32 = 12.0;
const PAYMENT_LINE_HEIGHT: f32 = 14.0;

const HEADER_FILL: (f32, f32, f32) = (0.74, 0.75, 0.75);
const BODY_FILL: (f32, f32, f32) = (0.86, 0.86, 0.86);

fn mm(pt: f32) -> Mm {
    Mm::from(Pt(pt))
}

fn grey(c: (f32, f32, f32)) -> Color {
    Color::Rgb(Rgb::new(c.0, c.1, c.2, None))
}

fn black() -> Color {
    Color::Rgb(Rgb::new(0.0, 0.0, 0.0, None))
}

/// Parsed copies of the embedded Regular/Bold faces, used only to measure
/// glyph advances (word-wrap, right-alignment) — never to embed, which
/// `printpdf::PdfDocumentReference::add_external_font` does separately from
/// its own copy of the raw bytes.
struct Metrics {
    regular: OwnedFace,
    bold: OwnedFace,
}

impl Metrics {
    fn new() -> Result<Self> {
        Ok(Self {
            regular: OwnedFace::from_vec(REGULAR_TTF.to_vec(), 0)
                .map_err(|e| anyhow!("parsing embedded Liberation Sans Regular: {e:?}"))?,
            bold: OwnedFace::from_vec(BOLD_TTF.to_vec(), 0)
                .map_err(|e| anyhow!("parsing embedded Liberation Sans Bold: {e:?}"))?,
        })
    }

    fn face(&self, bold: bool) -> &Face<'_> {
        if bold {
            self.bold.as_face_ref()
        } else {
            self.regular.as_face_ref()
        }
    }

    /// The width, in pt, `text` would occupy at `size`pt. A character
    /// missing from the font's cmap (shouldn't happen for anything
    /// Liberation Sans covers) falls back to half the font size rather than
    /// zero, so a run of such characters doesn't collapse to nothing and
    /// wrap/right-align math stays sane.
    fn width(&self, text: &str, size: f32, bold: bool) -> f32 {
        let face = self.face(bold);
        let units_per_em = f32::from(face.units_per_em());
        text.chars()
            .map(|c| {
                face.glyph_index(c)
                    .and_then(|gid| face.glyph_hor_advance(gid))
                    .map(|advance| f32::from(advance) / units_per_em * size)
                    .unwrap_or(size * 0.5)
            })
            .sum()
    }
}

/// Greedy word-wrap of one already-bullet-stripped line into as many lines
/// as needed to fit `max_width`pt at `size`. A single word wider than
/// `max_width` — a long email address, an unbroken token — is itself
/// broken into as many character-chunks as it takes to fit, via
/// [`break_word`], rather than overflowing the column or looping forever.
/// An empty `text` yields exactly one empty line, so blank description
/// lines are preserved rather than swallowed.
fn wrap_line(metrics: &Metrics, text: &str, max_width: f32, size: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if metrics.width(word, size, false) > max_width {
            // The word alone doesn't fit even on an empty line: flush
            // whatever's pending, then spread the word itself across as
            // many character-chunks as it takes.
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            lines.extend(break_word(metrics, word, max_width, size));
            continue;
        }
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if current.is_empty() || metrics.width(&candidate, size, false) <= max_width {
            current = candidate;
        } else {
            lines.push(std::mem::replace(&mut current, word.to_string()));
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Break a single word (already known too wide for `max_width` on its own)
/// into as many character-chunks as it takes for each chunk to fit. Mirrors
/// [`wrap_line`]'s own greedy accumulation, just per-character instead of
/// per-word — a chunk of exactly one character is still emitted even if
/// that character alone is wider than `max_width`, so this never loops
/// forever or drops input.
fn break_word(metrics: &Metrics, word: &str, max_width: f32, size: f32) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in word.chars() {
        let mut candidate = current.clone();
        candidate.push(ch);
        if current.is_empty() || metrics.width(&candidate, size, false) <= max_width {
            current = candidate;
        } else {
            chunks.push(std::mem::replace(&mut current, ch.to_string()));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Word-wrap a (possibly multi-line) block of plain text into the physical
/// lines that will be drawn: each `\n`-separated line is independently
/// word-wrapped to `max_width` via [`wrap_line`]. Unlike
/// [`layout_description`], there is no bullet handling — this is for
/// single-column text (header fields, payment details), not the
/// Description column.
fn wrap_text_lines(metrics: &Metrics, text: &str, max_width: f32, size: f32) -> Vec<String> {
    text.lines()
        .flat_map(|line| wrap_line(metrics, line, max_width, size))
        .collect()
}

/// One physical line to draw in the Description column.
struct DescLine {
    text: String,
    /// Horizontal offset, in pt, from the column's own left padding —
    /// nonzero for a bulleted line and its wrapped continuations (a hanging
    /// indent), zero otherwise.
    indent: f32,
    /// Whether this is the first physical line of a bulleted logical line —
    /// only it gets the "•" glyph.
    bullet_start: bool,
}

/// Split a (possibly multi-line) description into the physical lines that
/// will be drawn: each `\n`-separated line is checked for a `"* "`/`"- "`
/// bullet prefix (stripped and replaced with "•" at render time) and then
/// word-wrapped to `max_width`.
fn layout_description(metrics: &Metrics, description: &str, max_width: f32) -> Vec<DescLine> {
    let mut out = Vec::new();
    for raw_line in description.split('\n') {
        let (bullet, content, indent) = match raw_line
            .strip_prefix("* ")
            .or_else(|| raw_line.strip_prefix("- "))
        {
            Some(rest) => (true, rest, BULLET_INDENT),
            None => (false, raw_line, 0.0),
        };
        let available = (max_width - indent).max(10.0);
        let wrapped = wrap_line(metrics, content, available, BODY_FONT_SIZE);
        for (i, text) in wrapped.into_iter().enumerate() {
            out.push(DescLine {
                text,
                indent,
                bullet_start: bullet && i == 0,
            });
        }
    }
    out
}

fn row_height(desc_lines: usize) -> f32 {
    (desc_lines.max(1) as f32) * BODY_LINE_HEIGHT + ROW_V_PAD
}

/// `"YYYY-MM-DD"` → `"DD/MM/YYYY"`. Falls back to the input unchanged if it
/// isn't that shape — every caller here is a value already validated by
/// `invoicing::validate_item_date`, so this is a display transform, not a
/// second validation.
fn format_ddmmyyyy(iso: &str) -> String {
    match iso.split('-').collect::<Vec<_>>().as_slice() {
        [y, m, d] => format!("{d}/{m}/{y}"),
        _ => iso.to_string(),
    }
}

/// Draw `text` left-aligned at `(x, y)`.
fn draw_left(
    layer: &PdfLayerReference,
    font: &IndirectFontRef,
    text: &str,
    size: f32,
    x: f32,
    y: f32,
) {
    if text.is_empty() {
        return;
    }
    layer.use_text(text, size, mm(x), mm(y), font);
}

/// Draw `text` right-aligned so its right edge lands at `right_x`.
#[allow(clippy::too_many_arguments)]
fn draw_right(
    layer: &PdfLayerReference,
    metrics: &Metrics,
    font: &IndirectFontRef,
    bold: bool,
    text: &str,
    size: f32,
    right_x: f32,
    y: f32,
) {
    if text.is_empty() {
        return;
    }
    let width = metrics.width(text, size, bold);
    layer.use_text(text, size, mm(right_x - width), mm(y), font);
}

fn fill_rect(layer: &PdfLayerReference, x0: f32, y0: f32, x1: f32, y1: f32, color: Color) {
    layer.set_fill_color(color);
    let rect = Rect::new(mm(x0), mm(y0), mm(x1), mm(y1)).with_mode(PaintMode::Fill);
    layer.add_rect(rect);
}

struct Fonts {
    regular: IndirectFontRef,
    bold: IndirectFontRef,
}

/// Everything needed to draw on the current page/layer, threaded through the
/// drawing helpers below instead of recreated per call.
struct Ctx<'a> {
    metrics: &'a Metrics,
    fonts: &'a Fonts,
}

/// Start a new page, returning its layer and the y (pt) to begin drawing at.
fn add_page(doc: &PdfDocumentReference) -> (PdfLayerReference, f32) {
    let (page, layer) = doc.add_page(mm(PAGE_W_PT), mm(PAGE_H_PT), "Layer 1");
    (doc.get_page(page).get_layer(layer), CONTINUATION_TOP)
}

/// Draw the table's header row (column labels on a grey background) and
/// return the y to draw the first body row at.
fn draw_table_header(layer: &PdfLayerReference, ctx: &Ctx<'_>, currency: &str, y: f32) -> f32 {
    fill_rect(
        layer,
        MARGIN_LEFT,
        y - HEADER_ROW_HEIGHT,
        MARGIN_RIGHT,
        y,
        grey(HEADER_FILL),
    );
    let text_y = y - HEADER_ROW_HEIGHT / 2.0 - HEADER_FONT_SIZE * 0.36;
    layer.set_fill_color(black());
    draw_left(
        layer,
        &ctx.fonts.bold,
        "Description",
        HEADER_FONT_SIZE,
        COL_DESC_X0 + CELL_PAD,
        text_y,
    );
    for (label, right_x) in [
        ("Quantity", COL_QTY_X1),
        ("Unit Price", COL_PRICE_X1),
        (currency, COL_TOTAL_X1),
    ] {
        draw_right(
            layer,
            ctx.metrics,
            &ctx.fonts.bold,
            true,
            label,
            HEADER_FONT_SIZE,
            right_x - CELL_PAD,
            text_y,
        );
    }
    y - HEADER_ROW_HEIGHT
}

/// Draw one body row (its shaded background, description lines, and the
/// three right-aligned numeric cells) at `y` (top of the row) and return the
/// y for the next row.
#[allow(clippy::too_many_arguments)]
fn draw_table_row(
    layer: &PdfLayerReference,
    ctx: &Ctx<'_>,
    line: &crate::invoicing::snapshot::InvoiceSnapshotLine,
    desc_lines: &[DescLine],
    y: f32,
) -> f32 {
    let height = row_height(desc_lines.len());
    let row_bottom = y - height;
    fill_rect(
        layer,
        MARGIN_LEFT,
        row_bottom,
        MARGIN_RIGHT,
        y,
        grey(BODY_FILL),
    );
    layer.set_fill_color(black());

    let mut line_y = y - ROW_V_PAD / 2.0 - BODY_FONT_SIZE * 0.36;
    for desc_line in desc_lines {
        let base_x = COL_DESC_X0 + CELL_PAD + desc_line.indent;
        if desc_line.bullet_start {
            draw_left(
                layer,
                &ctx.fonts.regular,
                "•",
                BODY_FONT_SIZE,
                base_x - BULLET_INDENT * 0.6,
                line_y,
            );
        }
        draw_left(
            layer,
            &ctx.fonts.regular,
            &desc_line.text,
            BODY_FONT_SIZE,
            base_x,
            line_y,
        );
        line_y -= BODY_LINE_HEIGHT;
    }

    let first_line_y = y - ROW_V_PAD / 2.0 - BODY_FONT_SIZE * 0.36;
    draw_right(
        layer,
        ctx.metrics,
        &ctx.fonts.regular,
        false,
        &line.quantity,
        BODY_FONT_SIZE,
        COL_QTY_X1 - CELL_PAD,
        first_line_y,
    );
    draw_right(
        layer,
        ctx.metrics,
        &ctx.fonts.regular,
        false,
        &money::format_cents(line.unit_price_cents),
        BODY_FONT_SIZE,
        COL_PRICE_X1 - CELL_PAD,
        first_line_y,
    );
    draw_right(
        layer,
        ctx.metrics,
        &ctx.fonts.regular,
        false,
        &money::format_cents(line.amount_cents),
        BODY_FONT_SIZE,
        COL_TOTAL_X1 - CELL_PAD,
        first_line_y,
    );
    row_bottom
}

/// One row of the header block's row-grid: `None` is a blank spacer row (a
/// gap between fields), `Some((text, bold))` is a line to draw in that
/// column at this row.
type HeaderRow = Option<(String, bool)>;

/// Build the left column's rows top-to-bottom: invoice date, invoice
/// number, a blank spacer, "Invoice to" + the wrapped bill-to name/ABN/
/// address, then (if present) a blank spacer, "Reference", and the wrapped
/// reference text. Every user-supplied field is wrapped to `max_width` via
/// [`wrap_text_lines`] — for a field short enough to need only one line
/// (the common case) this produces exactly the same row sequence, at the
/// same 16pt-per-row cadence, as the original fixed-`y` version, so a
/// normal invoice's layout is unchanged.
fn build_left_column_rows(
    metrics: &Metrics,
    snap: &InvoiceSnapshot,
    max_width: f32,
) -> Vec<HeaderRow> {
    let mut rows = Vec::new();
    let issue_date = snap.issue_date.as_deref().unwrap_or("");
    let number = snap.display_number.as_deref().unwrap_or("Draft");
    match &snap.credit_note {
        Some(info) => {
            rows.push(Some((
                format!("Date: {}", format_ddmmyyyy(issue_date)),
                false,
            )));
            rows.push(Some((format!("Credit note number: {number}"), false)));
            rows.push(Some((
                format!(
                    "Adjusts invoice {} of {}",
                    info.invoice_display_number,
                    format_ddmmyyyy(&info.invoice_issue_date)
                ),
                false,
            )));
            rows.push(None);
            rows.push(Some(("Reason".to_string(), true)));
            for line in wrap_text_lines(metrics, &info.reason, max_width, COLUMN_FONT_SIZE) {
                rows.push(Some((line, false)));
            }
        }
        None => {
            rows.push(Some((
                format!("Invoice date: {}", format_ddmmyyyy(issue_date)),
                false,
            )));
            if let Some(due) = snap.due_date.as_deref() {
                rows.push(Some((format!("Due date: {}", format_ddmmyyyy(due)), true)));
            }
            rows.push(Some((format!("Invoice number: {number}"), false)));
        }
    }
    rows.push(None);
    rows.push(Some(("Invoice to".to_string(), true)));
    for line in wrap_text_lines(metrics, &snap.bill_to.name, max_width, COLUMN_FONT_SIZE) {
        rows.push(Some((line, false)));
    }
    if let Some(abn) = snap.bill_to.abn.as_deref().filter(|s| !s.is_empty()) {
        for line in wrap_text_lines(metrics, &format!("ABN: {abn}"), max_width, COLUMN_FONT_SIZE) {
            rows.push(Some((line, false)));
        }
    }
    if let Some(address) = snap.bill_to.address.as_deref().filter(|s| !s.is_empty()) {
        for line in wrap_text_lines(metrics, address, max_width, COLUMN_FONT_SIZE) {
            rows.push(Some((line, false)));
        }
    }
    if let Some(reference) = snap.reference.as_deref().filter(|s| !s.is_empty()) {
        rows.push(None);
        rows.push(Some(("Reference".to_string(), true)));
        for line in wrap_text_lines(metrics, reference, max_width, COLUMN_FONT_SIZE) {
            rows.push(Some((line, false)));
        }
    }
    rows
}

/// Build the right column's rows top-to-bottom: the wrapped seller name/
/// address, then (if present) a blank spacer + "Contact" + wrapped phone/
/// email, then (if present) a blank spacer + "ABN" + wrapped ABN. Same
/// wrapping/row-cadence reasoning as [`build_left_column_rows`].
fn build_right_column_rows(
    metrics: &Metrics,
    snap: &InvoiceSnapshot,
    max_width: f32,
) -> Vec<HeaderRow> {
    let mut rows = Vec::new();
    if let Some(name) = snap.seller.name.as_deref().filter(|s| !s.is_empty()) {
        for line in wrap_text_lines(metrics, name, max_width, COLUMN_FONT_SIZE) {
            rows.push(Some((line, false)));
        }
    }
    if let Some(address) = snap.seller.address.as_deref().filter(|s| !s.is_empty()) {
        for line in wrap_text_lines(metrics, address, max_width, COLUMN_FONT_SIZE) {
            rows.push(Some((line, false)));
        }
    }
    let phone = snap.seller.phone.as_deref().filter(|s| !s.is_empty());
    let email = snap.seller.email.as_deref().filter(|s| !s.is_empty());
    if phone.is_some() || email.is_some() {
        rows.push(None);
        rows.push(Some(("Contact".to_string(), true)));
        if let Some(phone) = phone {
            for line in wrap_text_lines(metrics, phone, max_width, COLUMN_FONT_SIZE) {
                rows.push(Some((line, false)));
            }
        }
        if let Some(email) = email {
            for line in wrap_text_lines(metrics, email, max_width, COLUMN_FONT_SIZE) {
                rows.push(Some((line, false)));
            }
        }
    }
    if let Some(abn) = snap.seller.abn.as_deref().filter(|s| !s.is_empty()) {
        rows.push(None);
        rows.push(Some(("ABN".to_string(), true)));
        for line in wrap_text_lines(metrics, abn, max_width, COLUMN_FONT_SIZE) {
            rows.push(Some((line, false)));
        }
    }
    rows
}

/// Draw the title + two-column header block (invoice date/number/bill-to on
/// the left, seller details on the right), row by row in lockstep so a page
/// break (when the next row would cross [`BOTTOM_MARGIN`]) always starts a
/// fresh page for *both* columns together — never mid-column, never
/// dropping a line. `layer`/`doc` are threaded through (and `*layer`
/// reassigned) exactly like the table-row loop in [`render_invoice_pdf`].
/// Returns the y to start the line table at, plus [`TABLE_TOP_GAP`].
fn draw_header_block(
    doc: &PdfDocumentReference,
    layer: &mut PdfLayerReference,
    ctx: &Ctx<'_>,
    snap: &InvoiceSnapshot,
) -> f32 {
    layer.set_fill_color(black());
    draw_left(
        layer,
        &ctx.fonts.bold,
        &snap.title,
        TITLE_SIZE,
        MARGIN_LEFT,
        TITLE_Y,
    );

    let left_rows = build_left_column_rows(ctx.metrics, snap, LEFT_COLUMN_MAX_WIDTH);
    let right_rows = build_right_column_rows(ctx.metrics, snap, RIGHT_COLUMN_MAX_WIDTH);
    let row_count = left_rows.len().max(right_rows.len());

    let mut y = COLUMNS_TOP;
    for i in 0..row_count {
        if y - COLUMN_LINE_HEIGHT < BOTTOM_MARGIN {
            let (new_layer, new_y) = add_page(doc);
            *layer = new_layer;
            y = new_y;
        }
        layer.set_fill_color(black());
        if let Some(Some((text, bold))) = left_rows.get(i) {
            draw_left(
                layer,
                if *bold {
                    &ctx.fonts.bold
                } else {
                    &ctx.fonts.regular
                },
                text,
                COLUMN_FONT_SIZE,
                MARGIN_LEFT,
                y,
            );
        }
        if let Some(Some((text, bold))) = right_rows.get(i) {
            draw_right(
                layer,
                ctx.metrics,
                if *bold {
                    &ctx.fonts.bold
                } else {
                    &ctx.fonts.regular
                },
                *bold,
                text,
                COLUMN_FONT_SIZE,
                MARGIN_RIGHT,
                y,
            );
        }
        y -= COLUMN_LINE_HEIGHT;
    }

    y - TABLE_TOP_GAP
}

/// Draw the totals block (right-aligned) below the table, returning the y
/// for whatever comes next. Each line goes through the same
/// bottom-margin-aware new-page check as everything else — in practice this
/// tiny, fixed-size block (at most 3 lines) never needs it, but there's no
/// reason for it to be the one block in the document that could still lose
/// a line off the bottom of the page.
fn draw_totals(
    doc: &PdfDocumentReference,
    layer: &mut PdfLayerReference,
    ctx: &Ctx<'_>,
    snap: &InvoiceSnapshot,
    y: f32,
) -> f32 {
    let mut y = y;
    let draw_line = |layer: &mut PdfLayerReference, y: &mut f32, bold: bool, text: &str| {
        if *y - TOTALS_LINE_HEIGHT < BOTTOM_MARGIN {
            let (new_layer, new_y) = add_page(doc);
            *layer = new_layer;
            *y = new_y;
        }
        layer.set_fill_color(black());
        draw_right(
            layer,
            ctx.metrics,
            if bold {
                &ctx.fonts.bold
            } else {
                &ctx.fonts.regular
            },
            bold,
            text,
            TOTALS_FONT_SIZE,
            MARGIN_RIGHT,
            *y,
        );
        *y -= TOTALS_LINE_HEIGHT;
    };
    if snap.gst_registered {
        draw_line(
            layer,
            &mut y,
            false,
            &format!(
                "Subtotal {}   {}",
                snap.currency,
                money::format_cents(snap.subtotal_cents)
            ),
        );
        draw_line(
            layer,
            &mut y,
            false,
            &format!("GST (10%)   {}", money::format_cents(snap.gst_cents)),
        );
    }
    let total_label = if snap.credit_note.is_some() {
        "Total credit"
    } else {
        "Total"
    };
    draw_line(
        layer,
        &mut y,
        true,
        &format!(
            "{total_label} {}   {}",
            snap.currency,
            money::format_cents(snap.total_cents)
        ),
    );
    y
}

/// Draw the "No GST has been charged." note (when applicable) and the
/// payment-details block, returning the final y. `payment_details` is
/// word-wrapped to the full content width ([`PAYMENT_CONTENT_WIDTH`]) and
/// then drawn line by line through the same bottom-margin-aware cursor as
/// everything else, so a multi-thousand-character payment block spans as
/// many extra pages as it needs rather than running off the bottom of one.
fn draw_notes_and_payment(
    doc: &PdfDocumentReference,
    layer: &mut PdfLayerReference,
    ctx: &Ctx<'_>,
    snap: &InvoiceSnapshot,
    y: f32,
) -> f32 {
    let mut y = y;
    layer.set_fill_color(black());
    if snap.no_gst_note {
        y -= 8.0;
        if y - COLUMN_LINE_HEIGHT < BOTTOM_MARGIN {
            let (new_layer, new_y) = add_page(doc);
            *layer = new_layer;
            y = new_y;
        }
        layer.set_fill_color(black());
        draw_left(
            layer,
            &ctx.fonts.regular,
            "No GST has been charged.",
            NOTE_FONT_SIZE,
            MARGIN_LEFT,
            y,
        );
        y -= COLUMN_LINE_HEIGHT;
    }
    if let Some(details) = snap.payment_details.as_deref().filter(|s| !s.is_empty()) {
        y -= 12.0;
        for line in wrap_text_lines(
            ctx.metrics,
            details,
            PAYMENT_CONTENT_WIDTH,
            PAYMENT_FONT_SIZE,
        ) {
            if y - PAYMENT_LINE_HEIGHT < BOTTOM_MARGIN {
                let (new_layer, new_y) = add_page(doc);
                *layer = new_layer;
                y = new_y;
            }
            layer.set_fill_color(black());
            draw_left(
                layer,
                &ctx.fonts.regular,
                &line,
                PAYMENT_FONT_SIZE,
                MARGIN_LEFT,
                y,
            );
            y -= PAYMENT_LINE_HEIGHT;
        }
    }
    y
}

/// Render an invoice's PDF from its frozen (or draft-preview) snapshot. No
/// I/O: everything needed to draw the document is already in `snapshot`.
pub fn render_invoice_pdf(snapshot: &InvoiceSnapshot) -> Result<Vec<u8>> {
    let metrics = Metrics::new()?;
    let (doc, page1, layer1) =
        PdfDocument::new(&snapshot.title, mm(PAGE_W_PT), mm(PAGE_H_PT), "Layer 1");
    // Fix the timestamps printpdf would otherwise default to `now()` (the
    // one part of this module's "current time" nondeterminism it *can*
    // eliminate — see this module's doc comment for what it can't) to the
    // invoice's own issue date at midnight UTC, so two renders of the same
    // snapshot never differ just because they happened to run a second
    // apart. A draft preview (no `issue_date`) falls back to the Unix
    // epoch rather than `now()`, for the same reason: this function's
    // output is a pure function of its input, including for that input.
    let doc = {
        let creation = document_datetime(snapshot);
        doc.with_creation_date(creation).with_mod_date(creation)
    };
    let fonts = Fonts {
        regular: doc
            .add_external_font(Cursor::new(REGULAR_TTF))
            .map_err(|e| anyhow!("embedding Liberation Sans Regular: {e}"))?,
        bold: doc
            .add_external_font(Cursor::new(BOLD_TTF))
            .map_err(|e| anyhow!("embedding Liberation Sans Bold: {e}"))?,
    };
    let ctx = Ctx {
        metrics: &metrics,
        fonts: &fonts,
    };

    let mut layer = doc.get_page(page1).get_layer(layer1);
    let mut y = draw_header_block(&doc, &mut layer, &ctx, snapshot);
    if y - HEADER_ROW_HEIGHT < BOTTOM_MARGIN {
        let (new_layer, new_y) = add_page(&doc);
        layer = new_layer;
        y = new_y;
    }
    y = draw_table_header(&layer, &ctx, &snapshot.currency, y);

    for line in &snapshot.lines {
        // A GST-free line on a tax invoice says so under its description.
        let description = if snapshot.gst_registered && line.gst_free {
            format!("{}\n(GST-free)", line.description)
        } else {
            line.description.clone()
        };
        let desc_lines = layout_description(
            &metrics,
            &description,
            COL_DESC_X1 - COL_DESC_X0 - 2.0 * CELL_PAD,
        );
        let needed = row_height(desc_lines.len());
        if y - needed < BOTTOM_MARGIN {
            let (new_layer, new_y) = add_page(&doc);
            layer = new_layer;
            y = draw_table_header(&layer, &ctx, &snapshot.currency, new_y);
        }
        y = draw_table_row(&layer, &ctx, line, &desc_lines, y);
    }

    y -= 10.0;
    y = draw_totals(&doc, &mut layer, &ctx, snapshot, y);
    draw_notes_and_payment(&doc, &mut layer, &ctx, snapshot, y);

    let mut bytes = Vec::new();
    {
        let mut writer = BufWriter::new(&mut bytes);
        doc.save(&mut writer)
            .map_err(|e| anyhow!("saving rendered invoice PDF: {e}"))?;
    }
    Ok(bytes)
}

/// The `OffsetDateTime` (midnight UTC) to stamp the PDF's `CreationDate`/
/// `ModDate` with: the snapshot's `issue_date` (`YYYY-MM-DD`) if present,
/// else the Unix epoch — never `OffsetDateTime::now_utc()`, which would
/// reintroduce the nondeterminism this function exists to remove. Falls
/// back to the epoch for a date string that fails to parse too; every
/// caller's `issue_date` is already validated elsewhere
/// (`invoicing::validate_item_date`), so this is a display-safe default,
/// not a second validation.
fn document_datetime(snap: &InvoiceSnapshot) -> OffsetDateTime {
    let timestamp = snap
        .issue_date
        .as_deref()
        .and_then(midnight_utc_unix_timestamp)
        .unwrap_or(0);
    OffsetDateTime::from_unix_timestamp(timestamp).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// `"YYYY-MM-DD"` → seconds since the Unix epoch for midnight UTC on that
/// date, or `None` if it isn't that shape. Implemented with a plain
/// calendar calculation (Howard Hinnant's `days_from_civil`) rather than
/// pulling in the `time` crate's own date-construction API, which isn't a
/// direct dependency of this crate (only a transitive one, via printpdf).
fn midnight_utc_unix_timestamp(iso: &str) -> Option<i64> {
    let [y, m, d] = iso.split('-').collect::<Vec<_>>()[..] else {
        return None;
    };
    let year: i64 = y.parse().ok()?;
    let month: u32 = m.parse().ok()?;
    let day: u32 = d.parse().ok()?;
    Some(days_from_civil(year, month, day) * 86_400)
}

/// Days since the Unix epoch (1970-01-01) for a given proleptic-Gregorian
/// civil date. Howard Hinnant's well-known constant-time algorithm
/// (<https://howardhinnant.github.io/date_algorithms.html#days_from_civil>).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (i64::from(m) + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invoicing::snapshot::{
        InvoiceSnapshotBillTo, InvoiceSnapshotLine, InvoiceSnapshotSeller,
    };

    fn seller() -> InvoiceSnapshotSeller {
        InvoiceSnapshotSeller {
            name: Some("Fictional Trades Pty Ltd".into()),
            abn: Some("11 222 333 444".into()),
            address: Some("1 Fictional St\nSomewhere NSW 2000".into()),
            phone: Some("0400 000 000".into()),
            email: Some("billing@fictional.example".into()),
        }
    }

    fn bill_to() -> InvoiceSnapshotBillTo {
        InvoiceSnapshotBillTo {
            name: "Fictional Client Pty Ltd".into(),
            abn: Some("55 666 777 888".into()),
            address: Some("2 Client Ave\nElsewhere NSW 2000".into()),
        }
    }

    fn line(desc: &str, qty_hundredths: i64, price_cents: i64) -> InvoiceSnapshotLine {
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

    fn base_snapshot(gst_registered: bool, lines: Vec<InvoiceSnapshotLine>) -> InvoiceSnapshot {
        let subtotal_cents: i64 = lines.iter().map(|l| l.amount_cents).sum();
        let gst_cents = if gst_registered {
            money::gst_cents(subtotal_cents)
        } else {
            0
        };
        InvoiceSnapshot {
            schema_version: crate::invoicing::snapshot::SCHEMA_VERSION,
            title: if gst_registered {
                "Tax Invoice"
            } else {
                "Invoice"
            }
            .into(),
            display_number: Some("008".into()),
            issue_date: Some("2026-08-19".into()),
            seller: seller(),
            bill_to: bill_to(),
            reference: Some("42 Site Road".into()),
            currency: "AUD".into(),
            gst_registered,
            lines,
            subtotal_cents,
            gst_cents,
            total_cents: subtotal_cents + gst_cents,
            payment_details: Some(
                "Please make all payments to the below account details:\nBSB 000-000\nAcc 00000000"
                    .into(),
            ),
            due_date: Some("2026-09-02".into()),
            credit_note: None,
            no_gst_note: !gst_registered,
        }
    }

    #[test]
    fn renders_a_valid_pdf_of_non_trivial_size() {
        let snap = base_snapshot(
            false,
            vec![line("* Site visit\n* Written report", 200, 40_000)],
        );
        let bytes = render_invoice_pdf(&snap).expect("render");
        assert!(bytes.starts_with(b"%PDF"));
        assert!(
            bytes.len() > 1000,
            "expected a non-trivial PDF, got {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn renders_a_gst_registered_invoice() {
        let snap = base_snapshot(true, vec![line("Consulting", 100, 12_345)]);
        let bytes = render_invoice_pdf(&snap).expect("render");
        assert!(bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn a_long_invoice_spans_multiple_pages() {
        let lines: Vec<InvoiceSnapshotLine> = (0..60)
            .map(|i| {
                line(
                    &format!("Line item number {i} with a reasonably long description"),
                    100,
                    5_000,
                )
            })
            .collect();
        let snap = base_snapshot(false, lines);
        let bytes = render_invoice_pdf(&snap).expect("render");
        assert!(bytes.starts_with(b"%PDF"));
        // lopdf serializes without a space between adjacent name tokens
        // (`/Type/Page`, not `/Type /Page`); exclude `/Type/Pages` (the
        // page-tree root, always exactly one) with a trailing-slash check.
        let page_count = count_occurrences(&bytes, b"/Type/Page/");
        assert!(page_count >= 2, "expected >=2 pages, counted {page_count}");
    }

    #[test]
    fn non_ascii_description_renders_without_error() {
        let snap = base_snapshot(
            false,
            vec![line(
                "Café visit – client’s “special” request: touché",
                100,
                10_000,
            )],
        );
        let bytes = render_invoice_pdf(&snap).expect("render");
        assert!(bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn draft_preview_with_no_number_or_issue_date_still_renders() {
        let mut snap = base_snapshot(false, vec![line("Draft line", 100, 1_000)]);
        snap.display_number = None;
        snap.issue_date = None;
        let bytes = render_invoice_pdf(&snap).expect("render");
        assert!(bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn wrap_line_preserves_a_blank_line() {
        let metrics = Metrics::new().unwrap();
        assert_eq!(wrap_line(&metrics, "", 200.0, 10.0), vec!["".to_string()]);
    }

    #[test]
    fn wrap_line_breaks_long_text_into_multiple_lines() {
        let metrics = Metrics::new().unwrap();
        let text = "one two three four five six seven eight nine ten";
        let lines = wrap_line(&metrics, text, 60.0, 10.0);
        assert!(lines.len() > 1);
        for l in &lines {
            assert!(
                metrics.width(l, 10.0, false) <= 60.0 + 1.0,
                "{l:?} too wide"
            );
        }
    }

    #[test]
    fn layout_description_marks_bullet_lines() {
        let metrics = Metrics::new().unwrap();
        let lines = layout_description(&metrics, "Intro\n* First bullet\n- Second bullet", 250.0);
        assert!(!lines[0].bullet_start);
        assert!(
            lines
                .iter()
                .any(|l| l.bullet_start && l.text == "First bullet")
        );
        assert!(
            lines
                .iter()
                .any(|l| l.bullet_start && l.text == "Second bullet")
        );
    }

    #[test]
    fn wrap_line_breaks_a_long_unbroken_word_by_characters() {
        let metrics = Metrics::new().unwrap();
        let word = "x".repeat(120);
        let lines = wrap_line(&metrics, &word, 60.0, 10.0);
        assert!(lines.len() > 1, "expected the word to be split into chunks");
        for l in &lines {
            assert!(
                metrics.width(l, 10.0, false) <= 60.0 + 1.0,
                "{l:?} too wide"
            );
        }
        // No characters lost or reordered by the split.
        assert_eq!(lines.concat(), word);
    }

    #[test]
    fn wrap_line_breaks_a_long_unbroken_word_mixed_with_short_ones() {
        let metrics = Metrics::new().unwrap();
        let token = "a".repeat(100);
        let text = format!("see {token} for details");
        let lines = wrap_line(&metrics, &text, 60.0, 10.0);
        assert!(lines.len() > 2);
        for l in &lines {
            assert!(
                metrics.width(l, 10.0, false) <= 60.0 + 1.0,
                "{l:?} too wide"
            );
        }
        // Every original word still appears somewhere in the wrapped output.
        let joined = lines.join(" ");
        assert!(joined.contains("see"));
        assert!(joined.contains("for"));
        assert!(joined.contains("details"));
    }

    /// A ~200-char client/business name built from real words (so it wraps
    /// by word, not by character) — bill-to's `name` is documented up to
    /// 200 chars.
    fn very_long_name() -> String {
        let mut name =
            "Extraordinarily Long Fictional Trading Company Proprietary Limited ".repeat(4);
        name.truncate(200);
        name
    }

    /// A 60-line address — well beyond anything a real address needs, but
    /// within what the field allows.
    fn sixty_line_address() -> String {
        (1..=60)
            .map(|i| format!("Address line {i}, Somewhere NSW 2000"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A ~2000-char payment-details block, well beyond a normal bank-detail
    /// blurb but within what the field allows.
    fn very_long_payment_details() -> String {
        let mut details = "Please make payment within 14 days of the issue date to the account below, quoting the invoice number as the payment reference. ".repeat(20);
        details.truncate(2000);
        details
    }

    /// A single ~300-character token with no whitespace at all (a long
    /// email address is the realistic case; this is longer still), to
    /// exercise character-level breaking rather than word-wrapping.
    fn very_long_unbroken_token() -> String {
        format!(
            "very-long-mailbox-name-{}@fictional-example.example",
            "x".repeat(260)
        )
    }

    #[test]
    fn renders_with_a_very_long_client_name() {
        let mut snap = base_snapshot(false, vec![line("Consulting", 100, 10_000)]);
        snap.bill_to.name = very_long_name();
        let bytes = render_invoice_pdf(&snap).expect("render");
        assert!(bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn renders_with_a_60_line_address_and_spans_multiple_pages() {
        let mut snap = base_snapshot(false, vec![line("Consulting", 100, 10_000)]);
        snap.bill_to.address = Some(sixty_line_address());
        let bytes = render_invoice_pdf(&snap).expect("render");
        assert!(bytes.starts_with(b"%PDF"));
        let page_count = count_occurrences(&bytes, b"/Type/Page/");
        assert!(
            page_count >= 2,
            "expected a 60-line address to overflow onto a second page, counted {page_count}"
        );
    }

    #[test]
    fn renders_with_a_2000_char_payment_block() {
        // A 2000-char payment block wraps to enough lines to matter, but
        // (at this field's max length, alone on an otherwise-small
        // invoice) still fits below the table on one page — the point of
        // this test is that it renders without losing any lines, which
        // `payment_details_wraps_to_the_full_wrapped_line_count` below
        // checks directly rather than by inferring it from a page count.
        let mut snap = base_snapshot(false, vec![line("Consulting", 100, 10_000)]);
        snap.payment_details = Some(very_long_payment_details());
        let bytes = render_invoice_pdf(&snap).expect("render");
        assert!(bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn payment_details_wraps_to_the_full_wrapped_line_count() {
        // A direct check that wrapping a long payment-details block doesn't
        // silently drop any of it: every wrapped physical line fits the
        // content width, and rejoining them (word-wrap only ever inserts
        // breaks at whitespace here, since none of these words are long
        // enough to need character-breaking) reconstructs the original
        // whitespace-normalised text.
        let metrics = Metrics::new().unwrap();
        let details = very_long_payment_details();
        let lines = wrap_text_lines(&metrics, &details, PAYMENT_CONTENT_WIDTH, PAYMENT_FONT_SIZE);
        assert!(
            lines.len() > 10,
            "expected many wrapped lines, got {}",
            lines.len()
        );
        for l in &lines {
            assert!(
                metrics.width(l, PAYMENT_FONT_SIZE, false) <= PAYMENT_CONTENT_WIDTH + 1.0,
                "{l:?} too wide"
            );
        }
        assert_eq!(
            lines.join(" "),
            details.split_whitespace().collect::<Vec<_>>().join(" ")
        );
    }

    #[test]
    fn renders_with_a_very_long_unbroken_token_in_a_header_field() {
        let mut snap = base_snapshot(false, vec![line("Consulting", 100, 10_000)]);
        snap.seller.email = Some(very_long_unbroken_token());
        let bytes = render_invoice_pdf(&snap).expect("render");
        assert!(bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn renders_with_every_long_field_at_once_without_error() {
        let mut snap = base_snapshot(true, vec![line("Consulting", 100, 10_000)]);
        snap.bill_to.name = very_long_name();
        snap.bill_to.address = Some(sixty_line_address());
        snap.seller.email = Some(very_long_unbroken_token());
        snap.payment_details = Some(very_long_payment_details());
        snap.reference = Some(very_long_name());
        let bytes = render_invoice_pdf(&snap).expect("render");
        assert!(bytes.starts_with(b"%PDF"));
        let page_count = count_occurrences(&bytes, b"/Type/Page/");
        assert!(
            page_count >= 2,
            "expected the combined stress fixture to span multiple pages, counted {page_count}"
        );
    }

    /// The core determinism claim (see this module's doc comment): two
    /// renders of the same snapshot are byte-identical everywhere except
    /// the small trailer window holding printpdf's two always-random
    /// `/ID` strings — never a difference anywhere in the actual content.
    #[test]
    fn rendering_the_same_snapshot_twice_is_content_identical() {
        let snap = base_snapshot(
            true,
            vec![line("* Site visit\n* Written report", 200, 40_000)],
        );
        let a = render_invoice_pdf(&snap).expect("render 1");
        let b = render_invoice_pdf(&snap).expect("render 2");
        assert_eq!(
            a.len(),
            b.len(),
            "two renders of an identical snapshot must be the same length"
        );
        let prefix_len = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
        let suffix_len = a
            .iter()
            .rev()
            .zip(b.iter().rev())
            .take_while(|(x, y)| x == y)
            .count();
        let differing = a
            .len()
            .saturating_sub(prefix_len)
            .saturating_sub(suffix_len);
        assert!(
            differing > 0,
            "expected printpdf's random trailer /ID strings to differ between renders"
        );
        assert!(
            differing <= 100,
            "expected only the trailer's two random 32-character /ID strings to differ \
             (<=100 bytes), but {differing} bytes differ between two renders of the same \
             snapshot — some content became nondeterministic"
        );
    }

    #[test]
    fn document_datetime_is_deterministic_and_ignores_the_clock() {
        let mut snap = base_snapshot(false, vec![line("Consulting", 100, 1_000)]);
        snap.issue_date = Some("2026-08-19".into());
        let a = document_datetime(&snap);
        let b = document_datetime(&snap);
        assert_eq!(a, b);
        assert_eq!(a.year(), 2026);
        assert_eq!(u8::from(a.month()), 8);
        assert_eq!(a.day(), 19);
        assert_eq!((a.hour(), a.minute(), a.second()), (0, 0, 0));
    }

    #[test]
    fn document_datetime_falls_back_to_the_epoch_for_a_draft_preview() {
        let mut snap = base_snapshot(false, vec![line("Consulting", 100, 1_000)]);
        snap.issue_date = None;
        assert_eq!(document_datetime(&snap), OffsetDateTime::UNIX_EPOCH);
    }

    fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    }

    #[test]
    fn renders_a_credit_note_with_a_gst_free_line() {
        let mut free = line("Permit fee", 100, 5_000);
        free.gst_free = true;
        let mut snap = base_snapshot(true, vec![line("Labour", 100, 10_000), free]);
        snap.title = "Adjustment Note".into();
        snap.display_number = Some("CN-001".into());
        snap.due_date = None;
        snap.payment_details = None;
        snap.credit_note = Some(crate::invoicing::snapshot::CreditNoteSnapshotInfo {
            invoice_display_number: "008".into(),
            invoice_issue_date: "2026-08-19".into(),
            reason: "Work not completed".into(),
        });
        let bytes = render_invoice_pdf(&snap).expect("renders");
        assert!(bytes.starts_with(b"%PDF"));
    }
}
