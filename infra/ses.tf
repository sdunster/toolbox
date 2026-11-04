# SES: the domain identity (with easy DKIM), a custom MAIL FROM subdomain, a
# configuration set for reputation tracking, and inbound receiving.
#
# Uses the SESv2 identity/configuration-set resources (aws_sesv2_*) but the
# classic v1 receipt-rule resources for inbound — the Terraform AWS provider
# has no SESv2 equivalent for receipt rules, and both APIs operate on the
# same underlying account identity, so mixing them is safe.

resource "aws_sesv2_configuration_set" "main" {
  configuration_set_name = "toolbox"

  reputation_options {
    reputation_metrics_enabled = true
  }

  sending_options {
    sending_enabled = true
  }
}

resource "aws_sesv2_email_identity" "main" {
  email_identity = var.support_domain

  # Every send from this identity — including the raw sends api/src/sesmail.rs
  # makes via aws-sdk-sesv2, which never sets a per-message configuration set
  # — uses this one by default. That's what makes the bounce/complaint
  # reputation metrics in monitoring.tf actually see traffic.
  configuration_set_name = aws_sesv2_configuration_set.main.configuration_set_name
}

resource "aws_sesv2_email_identity_mail_from_attributes" "main" {
  email_identity   = aws_sesv2_email_identity.main.email_identity
  mail_from_domain = "mail.${var.support_domain}"

  # If the custom MAIL FROM domain's MX/SPF records ever go missing (DNS
  # propagation lag, a misconfigured zone), fall back to sending from
  # amazonses.com rather than hard-failing every outbound send.
  behavior_on_mx_failure = "USE_DEFAULT_VALUE"
}

# The web app's own domain, as a SENDING-ONLY identity for system mail (login
# codes — see api/src/mail.rs's MAIL_FROM_VAR) — never added to the inbound
# receipt rule below, so it never opens tickets. Skipped entirely when
# web_domain and support_domain are the same value, since that would collide
# with aws_sesv2_email_identity.main above.
resource "aws_sesv2_email_identity" "system" {
  count          = var.web_domain == var.support_domain ? 0 : 1
  email_identity = var.web_domain

  configuration_set_name = aws_sesv2_configuration_set.main.configuration_set_name
}

resource "aws_sesv2_email_identity_mail_from_attributes" "system" {
  count            = var.web_domain == var.support_domain ? 0 : 1
  email_identity   = aws_sesv2_email_identity.system[0].email_identity
  mail_from_domain = "mail.${var.web_domain}"

  behavior_on_mx_failure = "USE_DEFAULT_VALUE"
}

# Additional mail domains: same shape as the primary identity above, one per
# entry in var.additional_mail_domains.
resource "aws_sesv2_email_identity" "additional" {
  for_each       = toset(var.additional_mail_domains)
  email_identity = each.value

  configuration_set_name = aws_sesv2_configuration_set.main.configuration_set_name
}

resource "aws_sesv2_email_identity_mail_from_attributes" "additional" {
  for_each         = aws_sesv2_email_identity.additional
  email_identity   = each.value.email_identity
  mail_from_domain = "mail.${each.value.email_identity}"

  behavior_on_mx_failure = "USE_DEFAULT_VALUE"
}

# ── Inbound receiving ─────────────────────────────────────────────────────────
#
# An AWS account has exactly ONE active receipt rule set per region. If
# anything else in this account ever receives mail, aws_ses_active_receipt_rule_set
# below is the resource that conflicts — check `aws ses
# describe-active-receipt-rule-set` before applying against an account that
# might already have one.

resource "aws_ses_receipt_rule_set" "main" {
  rule_set_name = "toolbox"
}

resource "aws_ses_active_receipt_rule_set" "main" {
  rule_set_name = aws_ses_receipt_rule_set.main.rule_set_name
}

# recipients = the mail domains (not a specific address) catches every
# local part at that domain — this is what makes both plain addresses and
# the "*@domain" wildcard inbound_address kind work; routing.rs resolves the
# actual address -> instance mapping downstream, this rule just accepts
# everything and hands it to S3 + SNS.
#
# The SNS topic is the S3 action's own notification (`topic_arn`), not a
# separate `sns_action`: an SNS action embeds the whole message in the
# notification, so SES bounces anything over 150 KB with "552 5.3.4 Message
# length exceeds limit set by recipient" — which a reply quoting an HTML
# thread or carrying a screenshot easily is. The worker only reads
# `mail.messageId` from the notification and fetches the raw MIME from S3, so
# it never needed the content; and the S3 action publishes only after the
# object is written, so the worker can't race it.
resource "aws_ses_receipt_rule" "inbound" {
  name          = "inbound"
  rule_set_name = aws_ses_receipt_rule_set.main.rule_set_name
  recipients    = concat([var.support_domain], var.additional_mail_domains)
  enabled       = true
  scan_enabled  = true

  s3_action {
    position          = 1
    bucket_name       = aws_s3_bucket.mail.id
    object_key_prefix = "inbound/"
    topic_arn         = aws_sns_topic.inbound_mail.arn
  }

  # SES must be able to write to the bucket before this rule can reference
  # it — without this, a from-scratch apply can create the rule before the
  # bucket policy exists.
  depends_on = [aws_s3_bucket_policy.mail]
}
