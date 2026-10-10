# AWS Backup for the durable DynamoDB tables.
#
# The durable tables already have PITR (35-day continuous backups) and
# deletion protection (dynamodb.tf). PITR only guards against accidental
# writes/deletes within its own window; this adds independent daily
# snapshots in a separate vault, so a problem with the table itself (or a
# bug that corrupts PITR's restore path) has a second, isolated recovery
# path.

locals {
  # Durable tables only. The three TTL'd ephemeral tables (login_code,
  # ephemeral_state, processed_message) are intentionally excluded — their
  # rows are, by design, worthless once expired (login codes, WebAuthn
  # challenges, submit tokens, inbound-mail idempotency keys), so backing
  # them up would restore already-stale or already-irrelevant data at real
  # cost for no value. See dynamodb.tf's file-level comment.
  backup_tables = [
    aws_dynamodb_table.instance,
    aws_dynamodb_table.inbound_address,
    aws_dynamodb_table.user,
    aws_dynamodb_table.membership,
    aws_dynamodb_table.ticket,
    aws_dynamodb_table.ticket_message,
    aws_dynamodb_table.counter,
    aws_dynamodb_table.user_token,
    aws_dynamodb_table.api_token,
    aws_dynamodb_table.webauthn_credential,
    aws_dynamodb_table.project,
    aws_dynamodb_table.billable_item,
    aws_dynamodb_table.invoice,
    aws_dynamodb_table.expense,
    aws_dynamodb_table.credit_note,
  ]
}

data "aws_iam_policy_document" "backup_assume_role" {
  statement {
    actions = ["sts:AssumeRole"]

    principals {
      type        = "Service"
      identifiers = ["backup.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "backup" {
  name               = "toolbox-backup-role"
  assume_role_policy = data.aws_iam_policy_document.backup_assume_role.json
}

resource "aws_iam_role_policy_attachment" "backup" {
  role       = aws_iam_role.backup.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSBackupServiceRolePolicyForBackup"
}

resource "aws_iam_role_policy_attachment" "backup_restore" {
  role       = aws_iam_role.backup.name
  policy_arn = "arn:aws:iam::aws:policy/service-role/AWSBackupServiceRolePolicyForRestores"
}

resource "aws_backup_vault" "main" {
  # Deliberately NOT renamed with the rest of the toolbox rename: an AWS
  # Backup vault can't be deleted while it holds recovery points, and this
  # one already has 35 days of history. The name is pure internal plumbing —
  # nothing user-facing ever sees it — so there's no reason to force a
  # destroy/recreate (and the resulting gap in daily-snapshot coverage) just
  # for cosmetic consistency.
  name = "microticket"
}

resource "aws_backup_plan" "main" {
  name = "toolbox-daily"

  rule {
    rule_name         = "daily"
    target_vault_name = aws_backup_vault.main.name
    # 14:00 UTC ~= midnight Sydney (quiet hour).
    schedule = "cron(0 14 * * ? *)"

    lifecycle {
      delete_after = 35
    }
  }
}

resource "aws_backup_selection" "main" {
  name         = "toolbox-dynamodb"
  iam_role_arn = aws_iam_role.backup.arn
  plan_id      = aws_backup_plan.main.id
  resources    = local.backup_tables[*].arn
}
