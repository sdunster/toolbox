resource "aws_sns_topic" "alerts" {
  name = "toolbox-alerts"
}

# Optional, so the stack can be stood up before anyone has decided where alerts
# should land. An email subscription is outward-facing — AWS mails the address a
# confirmation the moment it is created — so pointing it at a placeholder is
# worse than not creating it. Set alert_email later and re-apply; the topic and
# every alarm already exist, so nothing else changes.
resource "aws_sns_topic_subscription" "alerts_email" {
  count     = var.alert_email == "" ? 0 : 1
  topic_arn = aws_sns_topic.alerts.arn
  protocol  = "email"
  endpoint  = var.alert_email
}

# ── Worker queue ─────────────────────────────────────────────────────────────

resource "aws_cloudwatch_metric_alarm" "worker_dlq_not_empty" {
  alarm_name          = "toolbox-worker-dlq-not-empty"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "ApproximateNumberOfMessagesVisible"
  namespace           = "AWS/SQS"
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  alarm_description   = "A message landed in the worker DLQ (inbound mail or a worker job) — it failed processing 3 times (maxReceiveCount) and needs manual attention. See sqs.tf."
  treat_missing_data  = "notBreaching"

  dimensions = {
    QueueName = aws_sqs_queue.worker_dlq.name
  }

  alarm_actions = [aws_sns_topic.alerts.arn]
  ok_actions    = [aws_sns_topic.alerts.arn]
}

# ── Lambda errors ────────────────────────────────────────────────────────────

resource "aws_cloudwatch_metric_alarm" "api_errors" {
  alarm_name          = "toolbox-api-errors"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "Errors"
  namespace           = "AWS/Lambda"
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  alarm_description   = "toolbox-api raised an unhandled error."
  treat_missing_data  = "notBreaching"

  dimensions = {
    FunctionName = aws_lambda_function.api.function_name
  }

  alarm_actions = [aws_sns_topic.alerts.arn]
  ok_actions    = [aws_sns_topic.alerts.arn]
}

resource "aws_cloudwatch_metric_alarm" "worker_errors" {
  alarm_name          = "toolbox-worker-errors"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "Errors"
  namespace           = "AWS/Lambda"
  period              = 300
  statistic           = "Sum"
  threshold           = 0
  alarm_description   = "toolbox-worker raised an unhandled error (distinct from a DLQ landing — this fires on the first failed attempt, the DLQ alarm fires after all 3 retries are exhausted)."
  treat_missing_data  = "notBreaching"

  dimensions = {
    FunctionName = aws_lambda_function.worker.function_name
  }

  alarm_actions = [aws_sns_topic.alerts.arn]
  ok_actions    = [aws_sns_topic.alerts.arn]
}

# ── SES reputation ───────────────────────────────────────────────────────────
# ses.tf's configuration set has reputation_metrics_enabled = true and is the
# default configuration set for the domain identity, so every send publishes
# into these two account-standard metrics
# (https://docs.aws.amazon.com/ses/latest/dg/monitor-sending-activity-using-cloudwatch.html),
# dimensioned by the configuration set name. Thresholds are AWS's own
# published "at risk" reputation guidance, not something Toolbox-specific.

resource "aws_cloudwatch_metric_alarm" "ses_bounce_rate" {
  alarm_name          = "toolbox-ses-bounce-rate"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "Reputation.BounceRate"
  namespace           = "AWS/SES"
  period              = 3600
  statistic           = "Average"
  threshold           = 0.05 # AWS's own sandbox/reputation-risk threshold is 5%
  alarm_description   = "Outbound bounce rate is above 5% — SES may pause sending if this continues."
  treat_missing_data  = "notBreaching"

  dimensions = {
    "ses:configuration-set" = aws_sesv2_configuration_set.main.configuration_set_name
  }

  alarm_actions = [aws_sns_topic.alerts.arn]
  ok_actions    = [aws_sns_topic.alerts.arn]
}

resource "aws_cloudwatch_metric_alarm" "ses_complaint_rate" {
  alarm_name          = "toolbox-ses-complaint-rate"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 1
  metric_name         = "Reputation.ComplaintRate"
  namespace           = "AWS/SES"
  period              = 3600
  statistic           = "Average"
  threshold           = 0.001 # AWS's own reputation-risk threshold is 0.1%
  alarm_description   = "Outbound complaint rate is above 0.1% — SES may pause sending if this continues."
  treat_missing_data  = "notBreaching"

  dimensions = {
    "ses:configuration-set" = aws_sesv2_configuration_set.main.configuration_set_name
  }

  alarm_actions = [aws_sns_topic.alerts.arn]
  ok_actions    = [aws_sns_topic.alerts.arn]
}
