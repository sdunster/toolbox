//! Object-storage abstraction for the mail bucket: raw MIME, attachments, and
//! the presigned upload/download flow.
//!
//! Mirrors the [`crate::mail`] / [`crate::sesmail`] / [`crate::mockmail`]
//! split: this module holds the trait, [`crate::s3storage`] backs it with
//! real S3 (bucket name from the `MAIL_BUCKET` env var — never hardcoded),
//! and [`crate::mockstorage`] backs it with a local directory so the whole
//! inbound pipeline — and `createAttachmentUpload`/`replyToTicket`'s
//! attachment move — can be exercised with no AWS account (`make
//! local-mail`, `make dev-local`).
//!
//! Every key is bucket-relative. Layout (see `api/src/inbound/attachments.rs`
//! for the functions that build these):
//!
//! - `inbound/{ses_message_id}` — raw MIME, written by SES's own S3 action
//!   (the real Lambda only ever reads this key; nothing here ever writes it).
//! - `attachments/{ticket_id}/{message_id}/{n}/{filename}` — a stored
//!   attachment, whether it arrived inbound or was uploaded by an agent.
//! - `pending/{instance_id}/{nanoid}/{filename}` — an agent's upload
//!   (`createAttachmentUpload`) not yet attached to a message; moved into
//!   its final `attachments/…` key by `replyToTicket`.
//! - `invoices/{instance_id}/{invoice_id}/Invoice-{displayNumber}.pdf` — a
//!   finalized invoice's rendered PDF (`graphql::mutations::download_invoice_pdf`),
//!   written once and cached forever (`db::Invoice.pdf_s3_key`) — see
//!   CLAUDE.md's "Invoicing" house rule. Unlike everything else in this
//!   bucket, nothing expires this key: `infra/s3_mail.tf`'s lifecycle rule
//!   deliberately excludes `invoices/` because a finalized invoice's PDF is
//!   a permanent financial record, not transient mail.
//! - `credit-notes/{instance_id}/{credit_note_id}/Credit-Note-CN-{n}.pdf` —
//!   a credit note's PDF, cached the same way (`db::CreditNote.pdf_s3_key`).
//! - `receipts/{instance_id}/{expense_id}/{nanoid}/{filename}` — an
//!   expense's receipt, uploaded straight to its final key by a presigned
//!   PUT (`createExpenseReceiptUpload`) and recorded by
//!   `attachExpenseReceipt`. Kept, like the PDFs.

use anyhow::Result;
use std::future::Future;
use std::time::Duration;

/// How long a presigned URL stays valid. Generous enough for a slow upload
/// or a browser tab left open, short enough that a leaked URL is not a
/// standing liability.
pub const PRESIGN_EXPIRY: Duration = Duration::from_secs(15 * 60);

/// Keep only `[A-Za-z0-9._-]` from a caller-chosen download filename,
/// falling back to a fixed name if that strips everything — cheap insurance
/// against a filename built from untrusted-ish data (a display number,
/// technically caller-controlled once instances grow enough fields feeding
/// it) landing unescaped inside a `Content-Disposition` header, where a
/// stray quote or CRLF would be a header-injection risk.
pub fn sanitize_download_filename(filename: &str) -> String {
    let cleaned: String = filename
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    if cleaned.is_empty() {
        "download".to_string()
    } else {
        cleaned
    }
}

/// `Sync` for the same reason as [`crate::db::Handler`]/[`crate::mail::Handler`]:
/// a `&impl Handler` is held across `.await` inside the `Send` futures the
/// GraphQL/Poem stack builds.
pub trait Handler: Sync {
    /// Write bytes directly — used by the inbound pipeline to store parsed
    /// attachments, and (for the AWS-free `local-mail` path, which has no
    /// SES S3 action to have written it already) the raw MIME itself.
    fn put_bytes(
        &self,
        key: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Read bytes directly — the inbound Lambda's fetch of the raw MIME SES
    /// already wrote to `inbound/{ses_message_id}`.
    fn get_bytes(&self, key: &str) -> impl Future<Output = Result<Vec<u8>>> + Send;

    /// Move an object from one key to another (copy + delete for S3; a
    /// rename for the local mock) — how `replyToTicket` moves a
    /// `pending/…` upload into its final `attachments/…` key.
    fn move_object(&self, from_key: &str, to_key: &str) -> impl Future<Output = Result<()>> + Send;

    /// A presigned PUT URL for `createAttachmentUpload`.
    fn presign_put(
        &self,
        key: &str,
        content_type: &str,
    ) -> impl Future<Output = Result<String>> + Send;

    /// A presigned GET URL for an attachment download.
    fn presign_get(&self, key: &str) -> impl Future<Output = Result<String>> + Send;

    /// A presigned GET URL that forces a download with the given filename
    /// (`Content-Disposition: attachment; filename="…"`), rather than
    /// letting the browser render the object inline — what
    /// `downloadInvoicePdf` hands back so "Download PDF" saves
    /// `Invoice-008.pdf` instead of navigating to it. `filename` is the
    /// caller's un-sanitised choice (e.g. `Invoice-{displayNumber}.pdf`);
    /// the implementation is responsible for making it safe to place in a
    /// `Content-Disposition` header — see [`sanitize_download_filename`],
    /// which `s3storage`'s implementation calls.
    fn presign_get_download(
        &self,
        key: &str,
        filename: &str,
    ) -> impl Future<Output = Result<String>> + Send;

    /// The size, in bytes, of an already-stored object — used after
    /// `replyToTicket` moves a `pending/…` upload into place, to fill in
    /// `Attachment.size` without re-reading the whole object.
    fn object_size(&self, key: &str) -> impl Future<Output = Result<u64>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_download_filename_keeps_the_safe_case_unchanged() {
        assert_eq!(
            sanitize_download_filename("Invoice-008.pdf"),
            "Invoice-008.pdf"
        );
    }

    #[test]
    fn sanitize_download_filename_strips_everything_else() {
        assert_eq!(
            sanitize_download_filename("Invoice \"008\"\r\n.pdf"),
            "Invoice008.pdf"
        );
        assert_eq!(
            sanitize_download_filename("../../etc/passwd"),
            "....etcpasswd"
        );
    }

    #[test]
    fn sanitize_download_filename_falls_back_when_nothing_survives() {
        assert_eq!(sanitize_download_filename("\"\r\n"), "download");
        assert_eq!(sanitize_download_filename(""), "download");
    }
}
