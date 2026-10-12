//! GraphQL types for the money that moves after an invoice is issued:
//! payments, credit notes, and the reports built over them. Resolvers live
//! on `QueryRoot`/`MutationRoot` like everything else; this module just
//! keeps their types out of the already-large `query.rs`. See CLAUDE.md's
//! "Payments and credit notes" and "Reports" house rules.

use std::marker::PhantomData;

use anyhow::{Result, anyhow};
use async_graphql::dataloader::DataLoader;
use async_graphql::{Context, Enum, ID, InputObject, Object, SimpleObject};

use crate::app::{App, HasDb};
use crate::db;
use crate::db::Handler as _;
use crate::invoicing::{self, report};

use super::ProjectId;
use super::dataloader::DatabaseLoader;
use super::query::{Invoice, InvoiceBillToInfo, InvoiceLineInfo, InvoiceSellerInfo, Project};

/// One payment received against an invoice.
#[derive(SimpleObject, Clone, Debug)]
pub struct InvoicePaymentInfo {
    pub id: ID,
    /// `YYYY-MM-DD`.
    pub date: String,
    /// GST-inclusive cents.
    pub amount_cents: i64,
    pub note: Option<String>,
    /// Unix seconds.
    pub recorded_at: i64,
}

impl From<&db::InvoicePayment> for InvoicePaymentInfo {
    fn from(p: &db::InvoicePayment) -> Self {
        Self {
            id: ID(p.id.clone()),
            date: p.date.clone(),
            amount_cents: p.amount_cents,
            note: p.note.clone(),
            recorded_at: p.recorded_at as i64,
        }
    }
}

/// `recordInvoicePayment`'s argument.
#[derive(InputObject, Clone, Debug)]
pub struct RecordPaymentInput {
    /// `YYYY-MM-DD` — when the money arrived.
    pub date: String,
    /// GST-inclusive cents, > 0 and no more than the invoice's balance.
    pub amount_cents: i64,
    pub note: Option<String>,
}

/// One line of a credit note: a GST-exclusive amount being credited.
#[derive(InputObject, Clone, Debug)]
pub struct CreditNoteLineInput {
    pub description: String,
    /// GST-exclusive cents, > 0.
    pub amount_cents: i64,
    #[graphql(default)]
    pub gst_free: bool,
}

/// `issueCreditNote`'s argument. Omit `lines` to credit the whole invoice
/// (only allowed when nothing has been credited against it yet).
#[derive(InputObject, Clone, Debug)]
pub struct CreditNoteInput {
    /// `YYYY-MM-DD`; on or after the invoice's issue date.
    pub issue_date: String,
    /// Why — printed on the credit note. Required.
    pub reason: String,
    pub lines: Option<Vec<CreditNoteLineInput>>,
}

/// `sendInvoice`/`sendCreditNote`'s argument. An empty `to` means the
/// project's client email.
#[derive(InputObject, Clone, Debug, Default)]
pub struct SendDocumentInput {
    #[graphql(default)]
    pub to: Vec<String>,
    #[graphql(default)]
    pub cc: Vec<String>,
    /// An optional covering note, put above the summary in the email.
    pub message: Option<String>,
}

/// A `credit_note` row, exposed to its invoicing instance's members. Every
/// printable field reads the frozen snapshot.
#[derive(Debug, PartialEq)]
pub struct CreditNote<A: App + HasDb + Send + Sync> {
    _marker: PhantomData<A>,
    rec: db::CreditNote,
}

impl<A: App + HasDb + Send + Sync> CreditNote<A> {
    pub fn new(rec: db::CreditNote) -> Self {
        Self {
            _marker: PhantomData,
            rec,
        }
    }

    fn snapshot(&self) -> Result<invoicing::snapshot::InvoiceSnapshot> {
        serde_json::from_str(&self.rec.snapshot)
            .map_err(|e| anyhow!("Credit note {} has a corrupt snapshot: {e}", self.rec.id))
    }
}

impl<A: App + HasDb + Send + Sync> Clone for CreditNote<A> {
    fn clone(&self) -> Self {
        Self::new(self.rec.clone())
    }
}

#[Object]
impl<A: App + HasDb + Send + Sync + 'static> CreditNote<A> {
    async fn id(&self) -> ID {
        ID(self.rec.id.clone())
    }
    async fn number(&self) -> i32 {
        self.rec.number as i32
    }
    /// `CN-001`.
    async fn display_number(&self) -> String {
        self.rec.display_number()
    }
    /// `YYYY-MM-DD`.
    async fn issue_date(&self) -> &str {
        &self.rec.issue_date
    }
    async fn reason(&self) -> &str {
        &self.rec.reason
    }
    /// The invoice this credit note adjusts. A strongly consistent read,
    /// not the dataloader: `issueCreditNote` returns this field in the same
    /// response that just changed the invoice's balance.
    async fn invoice(&self, ctx: &Context<'_>) -> Result<Invoice<A>> {
        let app = ctx.data_unchecked::<std::sync::Arc<A>>();
        app.db()
            .get_invoice_consistent(&self.rec.invoice_id)
            .await?
            .map(Invoice::new)
            .ok_or_else(|| anyhow!("Invoice with ID {} missing", self.rec.invoice_id))
    }
    async fn project(&self, ctx: &Context<'_>) -> Result<Project<A>> {
        let loader = ctx.data_unchecked::<DataLoader<DatabaseLoader<A>>>();
        loader
            .load_one(ProjectId(ID(self.rec.project_id.clone())))
            .await
            .map_err(|e| anyhow!("Failed to load project via DataLoader: {}", e))?
            .map(Project::new)
            .ok_or_else(|| anyhow!("Project with ID {} missing", self.rec.project_id))
    }
    /// "Adjustment Note" or "Credit Note".
    async fn title(&self) -> Result<String> {
        Ok(self.snapshot()?.title)
    }
    async fn bill_to(&self) -> Result<InvoiceBillToInfo> {
        Ok(self.snapshot()?.bill_to.into())
    }
    async fn seller(&self) -> Result<InvoiceSellerInfo> {
        Ok(self.snapshot()?.seller.into())
    }
    async fn lines(&self) -> Result<Vec<InvoiceLineInfo>> {
        Ok(self
            .snapshot()?
            .lines
            .into_iter()
            .map(InvoiceLineInfo::from)
            .collect())
    }
    async fn subtotal_cents(&self) -> i64 {
        self.rec.subtotal_cents
    }
    async fn gst_cents(&self) -> i64 {
        self.rec.gst_cents
    }
    /// GST-inclusive — what this takes off the invoice's balance.
    async fn total_cents(&self) -> i64 {
        self.rec.total_cents
    }
    async fn currency(&self) -> Result<String> {
        Ok(self.snapshot()?.currency)
    }
    async fn gst_registered(&self) -> Result<bool> {
        Ok(self.snapshot()?.gst_registered)
    }
    async fn created_at(&self) -> i64 {
        self.rec.created_at as i64
    }
    /// When `sendCreditNote` last mailed it; `null` if never.
    async fn sent_at(&self) -> Option<i64> {
        self.rec.sent_at.map(|t| t as i64)
    }
    async fn sent_to(&self) -> &[String] {
        &self.rec.sent_to
    }
}

/// `gstReport`'s basis.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum ReportBasisType {
    /// By payment date, GST apportioned per payment.
    Cash,
    /// By issue date.
    Accrual,
}

impl From<ReportBasisType> for report::Basis {
    fn from(b: ReportBasisType) -> Self {
        match b {
            ReportBasisType::Cash => Self::Cash,
            ReportBasisType::Accrual => Self::Accrual,
        }
    }
}

/// `gstReport`: the BAS figures for a period.
#[derive(SimpleObject, Clone, Debug)]
pub struct GstReport {
    /// `YYYY-MM-DD`, inclusive.
    pub from: String,
    pub to: String,
    pub basis: ReportBasisType,
    /// Whether the instance is set as GST-registered *now* — a non-registered
    /// business has no BAS GST to report, though the sales figures still
    /// stand.
    pub gst_registered: bool,
    pub currency: String,
    /// G1: total sales, GST-inclusive, net of credit notes.
    pub sales_cents: i64,
    /// 1A: GST on sales.
    pub gst_on_sales_cents: i64,
    /// Every expense in the period, GST-inclusive.
    pub purchases_cents: i64,
    /// 1B: GST on purchases.
    pub gst_on_purchases_cents: i64,
    /// 1A − 1B: positive is GST payable, negative a refund.
    pub net_gst_cents: i64,
    pub invoice_count: i32,
    pub credit_note_count: i32,
    pub payment_count: i32,
    pub expense_count: i32,
}

/// `receivables`: what clients owe, by how overdue it is.
#[derive(Debug)]
pub struct ReceivablesReport<A: App + HasDb + Send + Sync> {
    pub as_of: String,
    pub currency: String,
    pub aging: report::Aging,
    pub invoices: Vec<db::Invoice>,
    pub _marker: PhantomData<A>,
}

#[Object]
impl<A: App + HasDb + Send + Sync + 'static> ReceivablesReport<A> {
    /// `YYYY-MM-DD` — the (UTC) date the buckets are measured from.
    async fn as_of(&self) -> &str {
        &self.as_of
    }
    async fn currency(&self) -> &str {
        &self.currency
    }
    async fn total_cents(&self) -> i64 {
        self.aging.total_cents()
    }
    /// Not yet due.
    async fn current_cents(&self) -> i64 {
        self.aging.current_cents
    }
    async fn days_1_to_30_cents(&self) -> i64 {
        self.aging.days_1_to_30_cents
    }
    async fn days_31_to_60_cents(&self) -> i64 {
        self.aging.days_31_to_60_cents
    }
    async fn days_61_to_90_cents(&self) -> i64 {
        self.aging.days_61_to_90_cents
    }
    async fn days_over_90_cents(&self) -> i64 {
        self.aging.days_over_90_cents
    }
    /// Every invoice with a balance owing, most overdue first.
    async fn invoices(&self) -> Vec<Invoice<A>> {
        self.invoices.iter().cloned().map(Invoice::new).collect()
    }
}

/// `Project.financials`. Everything GST-exclusive except where named.
#[derive(SimpleObject, Clone, Debug)]
pub struct ProjectFinancialsInfo {
    /// Finalized invoices, net of credit notes.
    pub invoiced_cents: i64,
    pub invoiced_incl_gst_cents: i64,
    pub paid_cents: i64,
    pub outstanding_cents: i64,
    pub expenses_cents: i64,
    /// Billable items not yet on any invoice.
    pub unbilled_cents: i64,
    /// Invoiced − expenses.
    pub profit_cents: i64,
}

impl From<report::ProjectFinancials> for ProjectFinancialsInfo {
    fn from(f: report::ProjectFinancials) -> Self {
        Self {
            invoiced_cents: f.invoiced_cents,
            invoiced_incl_gst_cents: f.invoiced_incl_gst_cents,
            paid_cents: f.paid_cents,
            outstanding_cents: f.outstanding_cents,
            expenses_cents: f.expenses_cents,
            unbilled_cents: f.unbilled_cents,
            profit_cents: f.profit_cents,
        }
    }
}

/// `invoicingExport`'s kind.
#[derive(Enum, Copy, Clone, Eq, PartialEq, Debug)]
pub enum CsvExportType {
    Invoices,
    CreditNotes,
    Payments,
    Expenses,
}

impl From<CsvExportType> for invoicing::csv::Export {
    fn from(k: CsvExportType) -> Self {
        match k {
            CsvExportType::Invoices => Self::Invoices,
            CsvExportType::CreditNotes => Self::CreditNotes,
            CsvExportType::Payments => Self::Payments,
            CsvExportType::Expenses => Self::Expenses,
        }
    }
}

/// An expense's uploaded receipt. Download it with
/// `downloadExpenseReceipt`.
#[derive(SimpleObject, Clone, Debug)]
pub struct ExpenseReceiptInfo {
    pub filename: String,
    pub content_type: String,
    pub size: i64,
}

impl From<&db::ExpenseReceipt> for ExpenseReceiptInfo {
    fn from(r: &db::ExpenseReceipt) -> Self {
        Self {
            filename: r.filename.clone(),
            content_type: r.content_type.clone(),
            size: r.size as i64,
        }
    }
}
