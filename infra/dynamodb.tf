# DynamoDB tables for Toolbox.
#
# This file is the source of truth for the schema — `api/src/bin/local-tables.rs`
# is transcribed from it by hand (DynamoDB Local has no Terraform provider) and
# must be kept in sync whenever a table or GSI is added here; `make
# local-tables-check` in CI is the tripwire that catches drift. See SCHEMA.md for
# the full attribute-by-attribute documentation.
#
# Conventions:
#   - Table name is "${var.db_prefix}_<entity>"; GSI name is "<hashkey>-index" or
#     "<hashkey>-<sortkey>-index".
#   - PAY_PER_REQUEST billing throughout — no provisioned throughput to tune.
#   - Durable tables get deletion_protection_enabled + 35-day point-in-time
#     recovery. The three ephemeral tables (login_code, ephemeral_state,
#     processed_message) get native TTL instead, and no deletion protection —
#     their rows are, by design, worthless once expired.
#   - Only key and GSI-key attributes are declared below. DynamoDB requires an
#     `attribute` block for every attribute used as a table or index key, and
#     forbids one for anything else — the remaining fields on each item are
#     schema-free and documented in SCHEMA.md instead.
#
# This project is prod-only (see CLAUDE.md / the build plan), so there is no
# parallel test-prefix table set the way seslogin has one.

# ── instance ───────────────────────────────────────────────────────────────────

resource "aws_dynamodb_table" "instance" {
  name                        = "${var.db_prefix}_instance"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }
  attribute {
    name = "slug"
    type = "S"
  }

  # Resolves a human-readable slug (from a URL: /app/:slug, /submit/:slug) to an
  # instance id. KEYS_ONLY: the caller always follows up with a GetItem for the
  # full instance record, so projecting more here would just be wasted storage.
  global_secondary_index {
    name = "slug-index"
    key_schema {
      attribute_name = "slug"
      key_type       = "HASH"
    }
    projection_type = "KEYS_ONLY"
  }
}

# ── inbound_address ──────────────────────────────────────────────────────────

resource "aws_dynamodb_table" "inbound_address" {
  name                        = "${var.db_prefix}_inbound_address"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "address"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  # Hash key is the lowercased full address ("support@example.com") or a
  # wildcard ("*@sub.example.com"), not an opaque id — inbound-mail routing
  # (api/src/inbound/routing.rs, a later step) does GetItem-by-address directly,
  # first for the exact address and then for the "*@domain" fallback.
  attribute {
    name = "address"
    type = "S"
  }
  attribute {
    name = "instance_id"
    type = "S"
  }

  # Instance settings page: list every inbound address (and its kind) owned by
  # an instance. ALL: the settings UI renders address/kind/created_at directly
  # from the list, and there are at most a handful of addresses per instance, so
  # the per-item storage cost of projecting everything is negligible next to
  # avoiding an N-way BatchGetItem.
  global_secondary_index {
    name = "instance_id-index"
    key_schema {
      attribute_name = "instance_id"
      key_type       = "HASH"
    }
    projection_type = "ALL"
  }
}

# ── user ──────────────────────────────────────────────────────────────────────

resource "aws_dynamodb_table" "user" {
  name                        = "${var.db_prefix}_user"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }
  attribute {
    name = "email"
    type = "S"
  }

  # Email-code login (requestAuthCode/verifyAuthCode): resolve email -> user id
  # without reading the full item. KEYS_ONLY for the same reason as
  # instance.slug-index — the login path only needs the id to drive the next
  # GetItem.
  global_secondary_index {
    name = "email-index"
    key_schema {
      attribute_name = "email"
      key_type       = "HASH"
    }
    projection_type = "KEYS_ONLY"
  }
}

# ── membership ────────────────────────────────────────────────────────────────

resource "aws_dynamodb_table" "membership" {
  name                        = "${var.db_prefix}_membership"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }
  attribute {
    name = "instance_id"
    type = "S"
  }
  attribute {
    name = "user_id"
    type = "S"
  }

  # Instance settings page: list every member (and their role) of an instance.
  # ALL avoids an N-way BatchGetItem to render the member list.
  global_secondary_index {
    name = "instance_id-index"
    key_schema {
      attribute_name = "instance_id"
      key_type       = "HASH"
    }
    projection_type = "ALL"
  }
  # "me" query / instance switcher: list every instance a user belongs to, with
  # their role in each. ALL for the same reason as instance_id-index above — a
  # user typically belongs to a handful of instances at most.
  global_secondary_index {
    name = "user_id-index"
    key_schema {
      attribute_name = "user_id"
      key_type       = "HASH"
    }
    projection_type = "ALL"
  }
}

# ── ticket ────────────────────────────────────────────────────────────────────
#
# instance_status, instance_visible and instance_assignee are the sparse-GSI
# trick that drives every ticket list page. They are composite marker
# attributes — not independent fields — and per the house rule (CLAUDE.md:
# "omit optional attributes, never write Null") they are written only when
# applicable and REMOVEd, never set to AttributeValue::Null, when they stop
# applying. DynamoDB only indexes items where a GSI's hash key attribute is
# present, so REMOVEing the attribute is what drops a ticket out of a list —
# writing Null would leave the item indexed with a null sort position instead.
resource "aws_dynamodb_table" "ticket" {
  name                        = "${var.db_prefix}_ticket"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }
  # "{instance_id}#open" | "{instance_id}#closed" | "{instance_id}#deleted".
  # Always present (every ticket has a status), so this GSI is not sparse in
  # the "attribute sometimes absent" sense — it is sparse in the sense that
  # each value only ever matches one status at a time.
  attribute {
    name = "instance_status"
    type = "S"
  }
  # "{instance_id}", present only when status != deleted. Sparse: sort order
  # for the All page, with deleted tickets dropped out entirely.
  attribute {
    name = "instance_visible"
    type = "S"
  }
  # "{instance_id}#{assignee_user_id}", present only when assigned and visible.
  # Sparse: the "assigned to me" filter.
  attribute {
    name = "instance_assignee"
    type = "S"
  }
  attribute {
    name = "last_activity_at"
    type = "N"
  }
  # "{instance_id}#{number}", always present — used to resolve the
  # "[#{slug}-{number}]" subject tag (after slug -> instance_id) back to a
  # ticket during inbound-mail threading.
  attribute {
    name = "instance_number"
    type = "S"
  }

  # Open/Closed list pages, split by status, newest activity first.
  global_secondary_index {
    name = "instance_status-last_activity_at-index"
    key_schema {
      attribute_name = "instance_status"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "last_activity_at"
      key_type       = "RANGE"
    }
    projection_type = "ALL"
  }
  # All list page (every non-deleted ticket for the instance).
  global_secondary_index {
    name = "instance_visible-last_activity_at-index"
    key_schema {
      attribute_name = "instance_visible"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "last_activity_at"
      key_type       = "RANGE"
    }
    projection_type = "ALL"
  }
  # "Assigned to me" filter.
  global_secondary_index {
    name = "instance_assignee-last_activity_at-index"
    key_schema {
      attribute_name = "instance_assignee"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "last_activity_at"
      key_type       = "RANGE"
    }
    projection_type = "ALL"
  }
  # ALL projection on the three listing GSIs above: every ticket list page in
  # the web UI renders subject/status/requester/assignee/last_activity_at
  # straight from the list response — projecting everything avoids a
  # per-row GetItem for what is, by construction, the common case (paging
  # through a queue).
  #
  # Subject-tag resolution during inbound-mail threading: exact lookup of a
  # ticket by its per-instance number. KEYS_ONLY — the caller does a follow-up
  # GetItem on the resolved id anyway, to get a strongly consistent read
  # before appending a message and bumping last_activity_at.
  global_secondary_index {
    name = "instance_number-index"
    key_schema {
      attribute_name = "instance_number"
      key_type       = "HASH"
    }
    projection_type = "KEYS_ONLY"
  }
}

# ── ticket_message ───────────────────────────────────────────────────────────

resource "aws_dynamodb_table" "ticket_message" {
  name                        = "${var.db_prefix}_ticket_message"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }
  attribute {
    name = "ticket_id"
    type = "S"
  }
  attribute {
    name = "created_at"
    type = "N"
  }
  attribute {
    name = "rfc_message_id"
    type = "S"
  }

  # Thread view: every message for a ticket, in order. ALL — the thread view
  # renders body_text/body_html/attachments directly from the list, and a
  # ticket's message count is small enough that projecting everything beats an
  # N-way GetItem per page render.
  global_secondary_index {
    name = "ticket_id-created_at-index"
    key_schema {
      attribute_name = "ticket_id"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "created_at"
      key_type       = "RANGE"
    }
    projection_type = "ALL"
  }
  # Inbound-mail threading: resolve an In-Reply-To/References header back to
  # the ticket_message (and thus ticket_id) it answers. KEYS_ONLY — this is an
  # id resolution step; the caller reads the resolved ticket_message (and its
  # parent ticket) with a separate strongly consistent GetItem.
  global_secondary_index {
    name = "rfc_message_id-index"
    key_schema {
      attribute_name = "rfc_message_id"
      key_type       = "HASH"
    }
    projection_type = "KEYS_ONLY"
  }
}

# ── counter ───────────────────────────────────────────────────────────────────
# Atomic ADD counter, one row per instance (hash key = instance id), backing
# per-instance sequential ticket numbers. See SCHEMA.md "Known issues and
# risks" for the read-then-write race this table's *misuse* would create —
# the number must come from an atomic UpdateItem ADD, never a read-modify-write.
# Also holds one `{instance_id}#invoice#{number}` reservation row per finalized
# invoice, created in the finalize transaction — see SCHEMA.md.

resource "aws_dynamodb_table" "counter" {
  name                        = "${var.db_prefix}_counter"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }
}

# ── login_code ────────────────────────────────────────────────────────────────

resource "aws_dynamodb_table" "login_code" {
  name         = "${var.db_prefix}_login_code"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "email"

  attribute {
    name = "email"
    type = "S"
  }

  ttl {
    attribute_name = "expires_at"
    enabled        = true
  }
}

# ── user_token ────────────────────────────────────────────────────────────────

resource "aws_dynamodb_table" "user_token" {
  name                        = "${var.db_prefix}_user_token"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }

  # No token_hash GSI: an mtu_{id}.{secret} token carries its own row id, so
  # verification is a strongly consistent GetItem on id (same shape as
  # api_token below).
}

# ── oauth_grant ───────────────────────────────────────────────────────────────
# One row per OAuth client a user has authorized (the MCP interface): the
# current access + refresh token hashes and the grant's audience. Durable table
# (deletion protection + PITR) whose rows also carry a native TTL on
# expires_at — the grant's absolute 90-day cap — so a grant that outlives it
# is reaped without a sweeper. Expiry is *also* checked in the application,
# since TTL deletion can lag by up to 48h. See SCHEMA.md.

resource "aws_dynamodb_table" "oauth_grant" {
  name                        = "${var.db_prefix}_oauth_grant"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }
  attribute {
    name = "user_id"
    type = "S"
  }

  # Backs the "connected apps" list (a user's own grants). ALL: low-cardinality
  # and every field is rendered directly from the list.
  global_secondary_index {
    name = "user_id-index"
    key_schema {
      attribute_name = "user_id"
      key_type       = "HASH"
    }
    projection_type = "ALL"
  }

  ttl {
    attribute_name = "expires_at"
    enabled        = true
  }
}

# ── api_token ─────────────────────────────────────────────────────────────────
# Instance-scoped integration credentials (mta_{id}.{secret}) authorising
# submitVerifiedTicket. No token_hash GSI, same as user_token
# above — see SCHEMA.md for why: the token carries its own row id, so
# verification is a GetItem by id, never a GSI lookup with an eventual-
# consistency window.

resource "aws_dynamodb_table" "api_token" {
  name                        = "${var.db_prefix}_api_token"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }
  attribute {
    name = "instance_id"
    type = "S"
  }

  # The token management page: list every token for an instance. ALL — same
  # reasoning as membership's instance_id-index: low-cardinality, low-traffic,
  # and every field is rendered directly from the list.
  global_secondary_index {
    name = "instance_id-index"
    key_schema {
      attribute_name = "instance_id"
      key_type       = "HASH"
    }
    projection_type = "ALL"
  }
}

# ── project ──────────────────────────────────────────────────────────────────
# A client/job an invoicing instance bills against — see CLAUDE.md's
# "Invoicing" house rule. Invoicing-only: every write path rejects a support
# instance.

resource "aws_dynamodb_table" "project" {
  name                        = "${var.db_prefix}_project"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }
  attribute {
    name = "instance_id"
    type = "S"
  }

  # The projects list page: every project for one instance. ALL — same
  # reasoning as inbound_address/api_token's instance_id-index: low
  # cardinality (an instance's clients/jobs, not a user-generated table), and
  # every field is rendered directly from the list.
  global_secondary_index {
    name = "instance_id-index"
    key_schema {
      attribute_name = "instance_id"
      key_type       = "HASH"
    }
    projection_type = "ALL"
  }
}

# ── billable_item ────────────────────────────────────────────────────────────
# One line of work recorded against a project, later collected onto an
# invoice — see CLAUDE.md's "Invoicing" house rule. `date` (YYYY-MM-DD) is
# the sort key of both listing GSIs, newest first; it is also a DynamoDB
# reserved word, so every expression that names it aliases it (`#d`).

resource "aws_dynamodb_table" "billable_item" {
  name                        = "${var.db_prefix}_billable_item"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }
  attribute {
    name = "instance_id"
    type = "S"
  }
  attribute {
    name = "project_id"
    type = "S"
  }
  attribute {
    name = "date"
    type = "S"
  }

  # The instance-wide billable items page. ALL: every field is rendered in
  # the list, and the unbilled/billed filter (`invoice_id` presence) runs as
  # a FilterExpression over the projected rows.
  global_secondary_index {
    name = "instance_id-date-index"
    key_schema {
      attribute_name = "instance_id"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "date"
      key_type       = "RANGE"
    }
    projection_type = "ALL"
  }

  # One project's items — the project page's list, and (next) the pool an
  # invoice is drawn from. ALL for the same reason.
  global_secondary_index {
    name = "project_id-date-index"
    key_schema {
      attribute_name = "project_id"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "date"
      key_type       = "RANGE"
    }
    projection_type = "ALL"
  }
}

# ── expense ──────────────────────────────────────────────────────────────────
# Money an invoicing instance spent — a purchase, or a cents-per-km vehicle
# trip — optionally against one of its projects; see CLAUDE.md's "Expenses"
# house rule. Same key shape as billable_item: `date` (YYYY-MM-DD, a reserved
# word, aliased `#d`) sorts both listing GSIs, newest first.

resource "aws_dynamodb_table" "expense" {
  name                        = "${var.db_prefix}_expense"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }
  attribute {
    name = "instance_id"
    type = "S"
  }
  attribute {
    name = "project_id"
    type = "S"
  }
  attribute {
    name = "date"
    type = "S"
  }

  # The instance-wide expenses page, and the per-person financial-year
  # vehicle km total (a `date BETWEEN` range on this index). ALL: every
  # field is rendered in the list, and the category filter runs as a
  # FilterExpression over the projected rows.
  global_secondary_index {
    name = "instance_id-date-index"
    key_schema {
      attribute_name = "instance_id"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "date"
      key_type       = "RANGE"
    }
    projection_type = "ALL"
  }

  # One project's expenses. Sparse: `project_id` is optional and omitted
  # when unset, so an expense with no project never appears here.
  global_secondary_index {
    name = "project_id-date-index"
    key_schema {
      attribute_name = "project_id"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "date"
      key_type       = "RANGE"
    }
    projection_type = "ALL"
  }
}

# ── invoice ──────────────────────────────────────────────────────────────────
# A project's billable items collected for billing — see CLAUDE.md's
# "Invoicing" house rule. `created_at` (N) is the sort key of both listing
# GSIs, newest first. `status`, `number`, and `version` are DynamoDB
# reserved words, so every expression that names one aliases it.
# Attach/detach/finalize use TransactWriteItems (Update/Put/Delete items
# only, no ConditionCheck) against this table and `billable_item` (finalize:
# `counter`) together;
# IAM authorises each item via the existing PutItem/UpdateItem/DeleteItem grants.

resource "aws_dynamodb_table" "invoice" {
  name                        = "${var.db_prefix}_invoice"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }
  attribute {
    name = "instance_id"
    type = "S"
  }
  attribute {
    name = "project_id"
    type = "S"
  }
  attribute {
    name = "created_at"
    type = "N"
  }

  # The instance-wide invoices list. ALL: every field the list needs (status,
  # number, total, project) is projected, and the DRAFT/UNPAID/PAID filter
  # runs as a FilterExpression over the projected rows.
  global_secondary_index {
    name = "instance_id-created_at-index"
    key_schema {
      attribute_name = "instance_id"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "created_at"
      key_type       = "RANGE"
    }
    projection_type = "ALL"
  }

  # One project's invoices — the project page's list. ALL for the same
  # reason.
  global_secondary_index {
    name = "project_id-created_at-index"
    key_schema {
      attribute_name = "project_id"
      key_type       = "HASH"
    }
    key_schema {
      attribute_name = "created_at"
      key_type       = "RANGE"
    }
    projection_type = "ALL"
  }
}

# ── webauthn_credential ──────────────────────────────────────────────────────

resource "aws_dynamodb_table" "webauthn_credential" {
  name                        = "${var.db_prefix}_webauthn_credential"
  billing_mode                = "PAY_PER_REQUEST"
  hash_key                    = "id"
  deletion_protection_enabled = true

  point_in_time_recovery {
    enabled                 = true
    recovery_period_in_days = 35
  }

  attribute {
    name = "id"
    type = "S"
  }
  attribute {
    name = "user_id"
    type = "S"
  }

  # Passkey login (discoverable-credential flow) and the settings page's
  # passkey list/the 10-passkeys-per-user cap. ALL: both call sites need the
  # full serialized credential, not just its id.
  global_secondary_index {
    name = "user_id-index"
    key_schema {
      attribute_name = "user_id"
      key_type       = "HASH"
    }
    projection_type = "ALL"
  }
}

# ── ephemeral_state ──────────────────────────────────────────────────────────
# Generic ephemeral key/value store with native TTL, namespaced by a `kind`
# discriminator (not a GSI key — see SCHEMA.md). Backs WebAuthn
# registration/login challenges and the requester submit-token flow.

resource "aws_dynamodb_table" "ephemeral_state" {
  name         = "${var.db_prefix}_ephemeral_state"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "id"

  attribute {
    name = "id"
    type = "S"
  }

  ttl {
    attribute_name = "expires_at"
    enabled        = true
  }
}

# ── processed_message ────────────────────────────────────────────────────────
# Inbound-mail idempotency: one row per SES message id, written with a
# conditional PutItem (attribute_not_exists(ses_message_id)) before any other
# processing happens, so a duplicate SQS delivery of the same message exits
# cleanly instead of creating a second ticket/reply. See SCHEMA.md "Known
# issues and risks" for the window this does and does not close.

resource "aws_dynamodb_table" "processed_message" {
  name         = "${var.db_prefix}_processed_message"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "ses_message_id"

  attribute {
    name = "ses_message_id"
    type = "S"
  }

  ttl {
    attribute_name = "expires_at"
    enabled        = true
  }
}
