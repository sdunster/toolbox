# Bounce and complaint feedback for outbound mail (issue #6), plus delivery
# and open/click tracking:
#
#   SES configuration set (ses.tf) -> event destination
#     (BOUNCE, COMPLAINT, DELIVERY, DELIVERY_DELAY, REJECT, OPEN, CLICK)
#     -> this SNS topic -> inbound-mail Lambda, invoked directly
#        -> api/src/suppression.rs records the address in email_suppression
#           (bounces and complaints only)
#        -> api/src/broadcast_events.rs records the outcome, or the first
#           open/click, on the broadcast recipient its message tags name
#        -> api/src/ticket_delivery.rs does the same for ticket mail (per
#           recipient for delivery, per message for opens/clicks)
#
# Subscribing to OPEN and CLICK is what makes SES add an open-tracking pixel
# to every HTML message sent through the configuration set, and rewrite its
# links through SES's click-tracking redirect (no custom tracking domain, so
# links go via awstrack.me). Untagged mail (login codes, staff notices) gets
# both too; its events are simply not recorded.
#
# Every identity uses aws_sesv2_configuration_set.main as its default, so this
# covers every send — instance mail and system mail alike. The Lambda is the
# same function that consumes inbound mail from SQS (lambda_inbound.tf); it
# tells the two event shapes apart itself. No queue in between: the work is a
# couple of idempotent upserts, and Lambda's own asynchronous retries (twice,
# by default) cover a transient failure. The inbound_mail_errors alarm
# (monitoring.tf) already watches the function.

resource "aws_sns_topic" "mail_feedback" {
  name = "toolbox-mail-feedback"
}

data "aws_iam_policy_document" "mail_feedback_topic" {
  statement {
    sid    = "AllowSESPublish"
    effect = "Allow"

    principals {
      type        = "Service"
      identifiers = ["ses.amazonaws.com"]
    }

    actions   = ["sns:Publish"]
    resources = [aws_sns_topic.mail_feedback.arn]

    condition {
      test     = "StringEquals"
      variable = "AWS:SourceAccount"
      values   = [var.aws_account_id]
    }
  }
}

resource "aws_sns_topic_policy" "mail_feedback" {
  arn    = aws_sns_topic.mail_feedback.arn
  policy = data.aws_iam_policy_document.mail_feedback_topic.json
}

resource "aws_sesv2_configuration_set_event_destination" "feedback" {
  configuration_set_name = aws_sesv2_configuration_set.main.configuration_set_name
  event_destination_name = "bounce-complaint-feedback"

  event_destination {
    enabled              = true
    matching_event_types = ["BOUNCE", "COMPLAINT", "DELIVERY", "DELIVERY_DELAY", "REJECT", "OPEN", "CLICK"]

    sns_destination {
      topic_arn = aws_sns_topic.mail_feedback.arn
    }
  }

  depends_on = [aws_sns_topic_policy.mail_feedback]
}

resource "aws_lambda_permission" "mail_feedback_sns" {
  statement_id  = "AllowMailFeedbackSNSInvoke"
  action        = "lambda:InvokeFunction"
  function_name = aws_lambda_function.inbound_mail.function_name
  principal     = "sns.amazonaws.com"
  source_arn    = aws_sns_topic.mail_feedback.arn
}

resource "aws_sns_topic_subscription" "mail_feedback_lambda" {
  topic_arn = aws_sns_topic.mail_feedback.arn
  protocol  = "lambda"
  endpoint  = aws_lambda_function.inbound_mail.arn

  depends_on = [aws_lambda_permission.mail_feedback_sns]
}
