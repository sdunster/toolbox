# The mail bucket: raw inbound MIME, extracted attachments, and pending
# (not-yet-attached) upload targets. Bucket name includes the account id
# purely for S3's global-uniqueness requirement — it identifies nothing on
# its own, unlike a domain name.
#
# Key layout (see api/src/s3storage.rs and api/src/inbound/pipeline.rs):
#   inbound/{ses_message_id}                       raw MIME, written by SES
#   attachments/{ticket_id}/{message_id}/{n}/{filename}
#   pending/{...}                                  presigned-PUT targets for
#                                                   createAttachmentUpload,
#                                                   moved into attachments/
#                                                   by replyToTicket
#   invoices/{instance_id}/{invoice_id}/Invoice-{displayNumber}.pdf
#                                                   a finalized invoice's
#                                                   rendered PDF, cached by
#                                                   downloadInvoicePdf. NOT
#                                                   covered by any lifecycle
#                                                   rule below — an invoice
#                                                   PDF is a permanent
#                                                   financial record, not
#                                                   transient mail.
#   credit-notes/{instance_id}/{credit_note_id}/Credit-Note-CN-{n}.pdf
#                                                   the same, for a credit
#                                                   note — also permanent.
#   receipts/{instance_id}/{expense_id}/{nanoid}/{filename}
#                                                   an expense's receipt,
#                                                   uploaded straight here
#                                                   by presigned PUT
#                                                   (createExpenseReceiptUpload)
#                                                   — kept, like the PDFs:
#                                                   it backs a tax claim.

resource "aws_s3_bucket" "mail" {
  bucket = "toolbox-mail-${var.aws_account_id}"
}

resource "aws_s3_bucket_public_access_block" "mail" {
  bucket = aws_s3_bucket.mail.id

  block_public_acls       = true
  ignore_public_acls      = true
  block_public_policy     = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_lifecycle_configuration" "mail" {
  bucket = aws_s3_bucket.mail.id

  # Raw inbound MIME is only ever needed once the pipeline has parsed,
  # routed, and stored a ticket_message for it — attachments extracted from
  # a message are copied out under attachments/ and are NOT covered by this
  # rule, so they outlive the raw source they came from.
  rule {
    id     = "expire-inbound-raw"
    status = "Enabled"

    filter {
      prefix = "inbound/"
    }

    expiration {
      days = var.inbound_retention_days
    }
  }

  # pending/ holds presigned-PUT upload targets for createAttachmentUpload
  # that replyToTicket hasn't yet moved into attachments/. A short expiry
  # cleans up abandoned uploads (closed tab, failed mutation, ...) without
  # needing an explicit sweep job.
  rule {
    id     = "expire-pending-uploads"
    status = "Enabled"

    filter {
      prefix = "pending/"
    }

    expiration {
      days = 1
    }
  }
}

# SES's documented pattern for its S3 receipt action: allow the SES service
# principal to write, conditioned on the receiving account so another
# account's SES setup can't be pointed at this bucket. Scoped to inbound/ —
# SES never writes anywhere else in this bucket.
data "aws_iam_policy_document" "mail_bucket" {
  statement {
    sid    = "AllowSESPuts"
    effect = "Allow"

    principals {
      type        = "Service"
      identifiers = ["ses.amazonaws.com"]
    }

    actions   = ["s3:PutObject"]
    resources = ["${aws_s3_bucket.mail.arn}/inbound/*"]

    condition {
      test     = "StringEquals"
      variable = "aws:Referer"
      values   = [var.aws_account_id]
    }
  }
}

resource "aws_s3_bucket_policy" "mail" {
  bucket = aws_s3_bucket.mail.id
  policy = data.aws_iam_policy_document.mail_bucket.json
}
