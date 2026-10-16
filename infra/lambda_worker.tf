# The worker: inbound mail and background jobs from the worker queue
# (sqs.tf), and SES events from the feedback topic (ses_feedback.tf). See
# api/src/bin/worker/main.rs.
resource "aws_lambda_function" "worker" {
  function_name = "toolbox-worker"
  role          = aws_iam_role.worker_lambda.arn
  runtime       = "provided.al2023"
  handler       = "bootstrap"
  timeout       = 60
  memory_size   = 512
  filename      = "${path.module}/placeholder.zip"

  environment {
    variables = {
      DB_PREFIX = var.db_prefix
      # api/src/s3storage.rs: same bucket the API lambda reads/writes — raw
      # MIME lands under inbound/, extracted attachments under attachments/.
      MAIL_BUCKET = aws_s3_bucket.mail.id
      # api/src/mail.rs: this function now sends mail of its own —
      # api/src/staff_notify.rs's best-effort staff notifications, sent from
      # the system sender, not an instance's inbound address. Without this,
      # a staff notification sent from this Lambda would fall back to
      # mail::FROM_FALLBACK's reserved `.test` domain and be refused by SES.
      MAIL_FROM = "no-reply@${var.web_domain}"
      # api/src/staff_notify.rs: web app origin for the "View ticket"/
      # "Change your notification settings" links a staff notification
      # email carries. See lambda_api.tf's identical variable.
      APP_BASE_URL = "https://${var.web_domain}"
      # api/src/sqsqueue.rs: each broadcast batch queues the next one here.
      WORKER_QUEUE_URL = aws_sqs_queue.worker.url
    }
  }

  logging_config {
    log_format = "JSON"
  }

  # See lambda_api.tf's identical block — Terraform owns configuration, CI
  # owns code.
  lifecycle {
    ignore_changes = [filename, source_code_hash]
  }
}

# A second trigger, not configured here: ses_feedback.tf's SNS topic invokes
# this function directly with bounce/complaint events (aws_lambda_permission
# + an SNS subscription). The binary tells the two event shapes apart itself.
resource "aws_lambda_event_source_mapping" "worker" {
  event_source_arn = aws_sqs_queue.worker.arn
  function_name    = aws_lambda_function.worker.arn
  batch_size       = 1
}

# A broadcast batch queues its own successor (api/src/broadcast_send.rs), so
# every batch is this function invoking itself through the worker queue.
# Lambda's recursive loop detection counts those hops via the X-Ray trace
# header and, by default, refuses the invocation after ~16 — the job then
# exhausts its receives without ever running and lands in the DLQ, stalling
# any broadcast past ~80 recipients. The loop is deliberate and bounded
# (MAX_BROADCAST_RECIPIENTS / BATCH_SIZE hops, BATCH_INTERVAL_SECS apart).
resource "aws_lambda_function_recursion_config" "worker" {
  function_name  = aws_lambda_function.worker.function_name
  recursive_loop = "Allow"
}
