# The worker queue: everything the worker Lambda (lambda_worker.tf) does in
# the background arrives here.
#
#   Inbound mail: SES receipt rule (ses.tf) -> S3 (raw MIME, s3_mail.tf)
#     -> SNS topic (below) -> this queue -> worker Lambda (event source mapping)
#   Worker jobs (api/src/queue.rs): sent straight to this queue by the API
#     lambda (sendBroadcast) and by the worker itself (each broadcast batch
#     queues the next, with an SQS delay).
#
# The worker tells the two apart by the message body: a job is JSON with a
# `job` field, inbound mail is an SNS envelope.

resource "aws_sqs_queue" "worker_dlq" {
  name                      = "toolbox-worker-dlq"
  message_retention_seconds = 1209600 # 14 days
}

resource "aws_sqs_queue" "worker" {
  name = "toolbox-worker"

  # Must be >= the worker lambda's timeout (60s, lambda_worker.tf).
  # If it were shorter, SQS could make a message visible to a second
  # consumer while the first invocation was still mid-processing it —
  # exactly the duplicate-delivery race processed_message's conditional
  # PutItem (and a broadcast batch's per-recipient claim) exists to guard
  # against, so there's no reason to invite it when a generous visibility
  # timeout avoids it for free.
  visibility_timeout_seconds = 90

  redrive_policy = jsonencode({
    deadLetterTargetArn = aws_sqs_queue.worker_dlq.arn
    maxReceiveCount     = 3
  })
}

# Only SNS needs a queue policy: the two Lambdas send jobs under their own
# roles' sqs:SendMessage grants (iam.tf).
data "aws_iam_policy_document" "worker_queue" {
  statement {
    sid    = "AllowSNSPublish"
    effect = "Allow"

    principals {
      type        = "Service"
      identifiers = ["sns.amazonaws.com"]
    }

    actions   = ["sqs:SendMessage"]
    resources = [aws_sqs_queue.worker.arn]

    condition {
      test     = "ArnEquals"
      variable = "aws:SourceArn"
      values   = [aws_sns_topic.inbound_mail.arn]
    }
  }
}

resource "aws_sqs_queue_policy" "worker" {
  queue_url = aws_sqs_queue.worker.id
  policy    = data.aws_iam_policy_document.worker_queue.json
}

# Still named for what it carries: only inbound mail goes through SNS.
resource "aws_sns_topic" "inbound_mail" {
  name = "toolbox-inbound"
}

data "aws_iam_policy_document" "inbound_mail_topic" {
  statement {
    sid    = "AllowSESPublish"
    effect = "Allow"

    principals {
      type        = "Service"
      identifiers = ["ses.amazonaws.com"]
    }

    actions   = ["sns:Publish"]
    resources = [aws_sns_topic.inbound_mail.arn]

    condition {
      test     = "StringEquals"
      variable = "AWS:SourceAccount"
      values   = [var.aws_account_id]
    }
  }
}

resource "aws_sns_topic_policy" "inbound_mail" {
  arn    = aws_sns_topic.inbound_mail.arn
  policy = data.aws_iam_policy_document.inbound_mail_topic.json
}

resource "aws_sns_topic_subscription" "inbound_mail_sqs" {
  topic_arn = aws_sns_topic.inbound_mail.arn
  protocol  = "sqs"
  endpoint  = aws_sqs_queue.worker.arn
}
