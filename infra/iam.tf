# Per-Lambda execution roles. Each gets only the inline policies its own
# function needs (named by capability, not by resource), rather than one
# shared role — the API lambda can only send to the worker queue, never
# consume it, the worker lambda never needs a broader DynamoDB grant than the
# API lambda does, etc.

data "aws_iam_policy_document" "lambda_assume_role" {
  statement {
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["lambda.amazonaws.com"]
    }
  }
}

locals {
  # DynamoDB access is granted by table-name prefix, defined once here,
  # rather than listing every table's ARN — a new table added to
  # dynamodb.tf never needs a matching IAM change. Covers both the tables
  # themselves and their GSIs.
  dynamodb_table_arns = [
    "arn:aws:dynamodb:${var.aws_region}:${var.aws_account_id}:table/${var.db_prefix}*",
    "arn:aws:dynamodb:${var.aws_region}:${var.aws_account_id}:table/${var.db_prefix}*/index/*",
  ]
}

# ── API lambda role ─────────────────────────────────────────────────────────

resource "aws_iam_role" "api_lambda" {
  name               = "toolbox-api-lambda-role"
  assume_role_policy = data.aws_iam_policy_document.lambda_assume_role.json
}

resource "aws_iam_role_policy_attachment" "api_lambda_logs" {
  role       = aws_iam_role.api_lambda.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy" "api_lambda_dynamodb" {
  name = "dynamodb-access"
  role = aws_iam_role.api_lambda.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = [
        "dynamodb:GetItem",
        "dynamodb:PutItem",
        "dynamodb:UpdateItem",
        "dynamodb:DeleteItem",
        "dynamodb:Query",
        "dynamodb:Scan",
        "dynamodb:BatchGetItem",
        "dynamodb:BatchWriteItem",
      ]
      Resource = local.dynamodb_table_arns
    }]
  })
}

# Outbound replies (replyToTicket, addInternalNote never sends) and the
# email-code / submit-code flows.
resource "aws_iam_role_policy" "api_lambda_ses" {
  name = "ses-send"
  role = aws_iam_role.api_lambda.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["ses:SendEmail", "ses:SendRawEmail"]
      Resource = "*"
    }]
  })
}

# Presigned GET for attachment downloads, presigned PUT into pending/ for
# createAttachmentUpload, and the pending/ -> attachments/ copy replyToTicket
# performs server-side. See api/src/s3storage.rs.
resource "aws_iam_role_policy" "api_lambda_s3_mail" {
  name = "s3-mail"
  role = aws_iam_role.api_lambda.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = [
        "s3:GetObject",
        "s3:PutObject",
        "s3:DeleteObject",
      ]
      Resource = "${aws_s3_bucket.mail.arn}/*"
    }]
  })
}

# createBroadcastAssetUpload presigns PUTs into the public email-assets
# bucket (s3_email_assets.tf). Write only: reads go through CloudFront.
resource "aws_iam_role_policy" "api_lambda_s3_email_assets" {
  name = "s3-email-assets"
  role = aws_iam_role.api_lambda.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["s3:PutObject"]
      Resource = "${aws_s3_bucket.email_assets.arn}/email-assets/*"
    }]
  })
}

# sendBroadcast queues a broadcast's first batch for the worker
# (api/src/broadcast_send.rs). Send only — the worker consumes.
resource "aws_iam_role_policy" "api_lambda_sqs_send" {
  name = "sqs-send"
  role = aws_iam_role.api_lambda.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["sqs:SendMessage"]
      Resource = aws_sqs_queue.worker.arn
    }]
  })
}

# ── Worker lambda role ──────────────────────────────────────────────────────

resource "aws_iam_role" "worker_lambda" {
  name               = "toolbox-worker-lambda-role"
  assume_role_policy = data.aws_iam_policy_document.lambda_assume_role.json
}

resource "aws_iam_role_policy_attachment" "worker_lambda_logs" {
  role       = aws_iam_role.worker_lambda.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole"
}

resource "aws_iam_role_policy" "worker_lambda_dynamodb" {
  name = "dynamodb-access"
  role = aws_iam_role.worker_lambda.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = [
        "dynamodb:GetItem",
        "dynamodb:PutItem",
        "dynamodb:UpdateItem",
        "dynamodb:DeleteItem",
        "dynamodb:Query",
        "dynamodb:Scan",
        "dynamodb:BatchGetItem",
        "dynamodb:BatchWriteItem",
      ]
      Resource = local.dynamodb_table_arns
    }]
  })
}

# Reads the raw MIME the SES receipt rule wrote (inbound/) and writes
# extracted attachments (attachments/). See api/src/s3storage.rs and
# api/src/inbound/pipeline.rs.
resource "aws_iam_role_policy" "worker_lambda_s3" {
  name = "s3-mail"
  role = aws_iam_role.worker_lambda.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = [
        "s3:GetObject",
        "s3:PutObject",
      ]
      Resource = "${aws_s3_bucket.mail.arn}/*"
    }]
  })
}

# Step 7's "notify requesters and CCs" — the worker lambda sends outbound
# mail directly (and broadcast batches), the same as the API lambda's replyToTicket path.
resource "aws_iam_role_policy" "worker_lambda_ses" {
  name = "ses-send"
  role = aws_iam_role.worker_lambda.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["ses:SendEmail", "ses:SendRawEmail"]
      Resource = "*"
    }]
  })
}

resource "aws_iam_role_policy" "worker_lambda_sqs_consume" {
  name = "sqs-consume"
  role = aws_iam_role.worker_lambda.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = [
        "sqs:ReceiveMessage",
        "sqs:DeleteMessage",
        "sqs:GetQueueAttributes",
      ]
      Resource = aws_sqs_queue.worker.arn
    }]
  })
}

# Each broadcast batch queues the next (api/src/broadcast_send.rs).
resource "aws_iam_role_policy" "worker_lambda_sqs_send" {
  name = "sqs-send"
  role = aws_iam_role.worker_lambda.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["sqs:SendMessage"]
      Resource = aws_sqs_queue.worker.arn
    }]
  })
}
