resource "aws_lambda_function" "api" {
  function_name = "toolbox-api"
  role          = aws_iam_role.api_lambda.arn
  runtime       = "provided.al2023"
  handler       = "bootstrap"
  timeout       = 30
  memory_size   = 256
  filename      = "${path.module}/placeholder.zip"

  environment {
    variables = merge(
      {
        DB_PREFIX = var.db_prefix
        # api/src/s3storage.rs: bucket for presigned attachment upload/download.
        # System mail (login codes) sends from the web app's own domain, not a
        # tenant's mail domain — instance-scoped mail (ticket replies and
        # notifications) sends from the instance's own inbound address instead,
        # so replies thread back to the right tenant.
        MAIL_FROM   = "no-reply@${var.web_domain}"
        MAIL_BUCKET = aws_s3_bucket.mail.id
        # api/src/s3storage.rs: the public bucket createBroadcastAssetUpload
        # presigns image uploads into, served at APP_BASE_URL/email-assets/*.
        EMAIL_ASSETS_BUCKET = aws_s3_bucket.email_assets.id
        # api/src/app.rs: WebAuthn relying-party id/origin. Both default to
        # localhost values meant only for `make dev`, so both must be set
        # explicitly here.
        WEBAUTHN_RP_ID     = var.web_domain
        WEBAUTHN_RP_ORIGIN = "https://${var.web_domain}"
        # api/src/staff_notify.rs: web app origin for the "View ticket"/
        # "Change your notification settings" links a staff notification
        # email carries. Defaults to localhost, meant only for `make dev`,
        # so this must be set explicitly here too.
        APP_BASE_URL = "https://${var.web_domain}"
        # api/src/base_url.rs: the API's public origin, used as the OAuth issuer
        # and to build the token/registration endpoint URLs. Required behind
        # CloudFront, which doesn't forward Host: without it the fallback would
        # name the Function URL's host, and OAuth clients would reject the mismatch.
        API_BASE_URL = "https://${var.web_domain}"
        # api/src/sqsqueue.rs: sendBroadcast queues a broadcast's first batch
        # for the worker here.
        WORKER_QUEUE_URL = aws_sqs_queue.worker.url
      },
      # api/src/oauth.rs: secret the stateless OAuth client ids are signed with.
      # Unset (empty), dynamic client registration answers 503 and no client can
      # complete the flow — same optional-key shape as Turnstile below. Omit the
      # key entirely rather than setting it empty.
      var.oauth_client_id_secret == "" ? {} : { OAUTH_CLIENT_ID_SECRET = var.oauth_client_id_secret },
      # api/src/turnstile.rs: verification is skipped whenever this is
      # unset, so omit the key entirely rather than setting it to an empty
      # string — an empty env var and a missing one are not the same thing
      # to std::env::var.
      var.turnstile_secret_key == "" ? {} : { TURNSTILE_SECRET_KEY = var.turnstile_secret_key }
    )
  }

  logging_config {
    log_format = "JSON"
  }

  # Terraform owns this function's configuration (role, env vars, memory);
  # CI owns its code (deploy-prod.yml runs `cargo lambda deploy` on every
  # push to prod, which updates code+hash directly, bypassing Terraform).
  # Without ignore_changes, `terraform apply` would overwrite whatever CI
  # last shipped with this placeholder.
  lifecycle {
    ignore_changes = [filename, source_code_hash]
  }
}

resource "aws_lambda_function_url" "api" {
  function_name      = aws_lambda_function.api.function_name
  authorization_type = "NONE"

  cors {
    allow_credentials = true
    allow_headers     = ["authorization", "content-type", "x-client-version"]
    allow_methods     = ["GET", "POST"]
    allow_origins     = var.allowed_origins
    max_age           = 600
  }
}
